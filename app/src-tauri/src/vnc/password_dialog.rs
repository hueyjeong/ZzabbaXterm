use std::{ptr, sync::Arc, time::Duration};

use tauri::WebviewWindow;
use tokio::sync::oneshot;
use windows_sys::Win32::{Foundation::HWND, UI::{Controls::EM_SETLIMITTEXT, WindowsAndMessaging::*}};
use zeroize::Zeroizing;

use tokio_util::sync::CancellationToken;
use super::lifetime::Lifetime;

const EDIT: i32 = 100;
const MAX_PASSWORD: usize = 8;
const TIMER: usize = 1;
const DIALOG_USER: i32 = (2 * size_of::<isize>()) as i32;
type Secret = Zeroizing<Vec<u8>>;

pub(super) async fn read(window: &WebviewWindow, target: String, life: &Arc<Lifetime>, loss: Option<CancellationToken>) -> Option<Secret> {
    let (send, reply) = oneshot::channel();
    let owner = window.clone();
    let life = life.clone();
    window.run_on_main_thread(move || {
        let mut state = Dialog { send, life, loss, password: None };
        if state.revoked() { return; }
        let Ok(hwnd) = owner.hwnd() else { return; };
        let template = template(&target);
        // 템플릿과 콜백 상태는 동기 모달 호출의 반환까지 살아 있다.
        let accepted = unsafe {
            DialogBoxIndirectParamW(ptr::null_mut(), template.0.as_ptr().cast(), hwnd.0.cast(),
                Some(dialog_proc), &mut state as *mut Dialog as isize)
        } == IDOK as isize;
        if accepted && !state.revoked() { let _ = state.send.send(state.password.take()); }
    }).ok()?;
    tokio::time::timeout(Duration::from_secs(60), reply).await.ok()?.ok()?
}

struct Dialog {
    send: oneshot::Sender<Option<Secret>>,
    life: Arc<Lifetime>,
    loss: Option<CancellationToken>,
    password: Option<Secret>,
}

impl Dialog {
    fn revoked(&self) -> bool {
        self.send.is_closed() || self.life.revoked() || self.loss.as_ref().is_some_and(CancellationToken::is_cancelled)
    }
}

unsafe extern "system" fn dialog_proc(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize {
    // Win32는 WM_INITDIALOG에만 스택 상태 포인터를 전달하고 그 뒤에는 창에 저장한 값을 사용한다.
    unsafe {
        if msg == WM_INITDIALOG {
            SetWindowLongPtrW(hwnd, DIALOG_USER, lparam);
            let state = &*(lparam as *const Dialog);
            if state.revoked() || SetTimer(hwnd, TIMER, 100, None) == 0 {
                EndDialog(hwnd, IDCANCEL as isize);
            }
            // 아홉 번째 문자는 과다 입력을 감지하기 위해 남겨 둔다.
            SendDlgItemMessageW(hwnd, EDIT, EM_SETLIMITTEXT, MAX_PASSWORD + 1, 0);
            return 1;
        }
        let data = GetWindowLongPtrW(hwnd, DIALOG_USER) as *mut Dialog;
        if data.is_null() { return 0; }
        let command = if msg == WM_COMMAND { (wparam & 0xffff) as i32 } else { 0 };
        let revoked = (*data).revoked();
        if msg == WM_CLOSE || command == IDCANCEL || (msg == WM_TIMER && wparam == TIMER && revoked) {
            SetDlgItemTextW(hwnd, EDIT, [0_u16].as_ptr());
            EndDialog(hwnd, IDCANCEL as isize);
            return 1;
        }
        if command == IDOK {
            if !revoked {
                let mut wide = Zeroizing::new([0_u16; MAX_PASSWORD + 2]);
                let len = GetDlgItemTextW(hwnd, EDIT, wide.as_mut_ptr(), wide.len() as i32) as usize;
                (*data).password = password_bytes(&wide[..len]);
            }
            SetDlgItemTextW(hwnd, EDIT, [0_u16].as_ptr());
            EndDialog(hwnd, if (*data).password.is_some() { IDOK } else { IDCANCEL } as isize);
            return 1;
        }
    }
    0
}

fn password_bytes(wide: &[u16]) -> Option<Secret> {
    if !(1..=MAX_PASSWORD).contains(&wide.len()) || wide.iter().any(|&c| !(1..=255).contains(&c)) { return None; }
    let mut bytes = Zeroizing::new(Vec::with_capacity(wide.len()));
    bytes.extend(wide.iter().map(|&c| c as u8));
    Some(bytes)
}

// 표준 DLGTEMPLATE 헤더와 각 DLGITEMTEMPLATE는 DWORD 경계에 둔다.
#[repr(align(4))]
struct Template([u16; 512]);

fn template(target: &str) -> Template {
    let mut out = Template([0; 512]);
    let mut pos = 0;
    let mut words = |values: &[u16]| {
        out.0[pos..pos + values.len()].copy_from_slice(values);
        pos += values.len();
    };
    let style = WS_POPUP | WS_CAPTION | WS_SYSMENU | (DS_MODALFRAME | DS_CENTER | DS_SETFONT) as u32;
    words(&[style as u16, (style >> 16) as u16, 0, 0, 5, 0, 0, 270, 106, 0, 0]);
    let mut text = Vec::with_capacity(320);
    text.extend("VNC 비밀번호".encode_utf16().chain([0, 9]));
    text.extend("Segoe UI".encode_utf16().chain([0]));
    words(&text);
    let mut item = |style: u32, [x, y, w, h]: [u16; 4], id: i32, class: u16, label: &str| {
        pos += pos % 2;
        let style = style | WS_CHILD | WS_VISIBLE;
        let fixed = [style as u16, (style >> 16) as u16, 0, 0, x, y, w, h, id as u16, 0xffff, class];
        for word in fixed.into_iter().chain(label.encode_utf16()).chain([0, 0]) {
            out.0[pos] = word;
            pos += 1;
        }
    };
    item(0, [8, 8, 254, 32], -1, 0x82, target);
    item(0, [8, 43, 254, 10], -1, 0x82, "비밀번호 (Latin-1, 1–8자)");
    item(WS_TABSTOP | WS_BORDER | (ES_PASSWORD | ES_AUTOHSCROLL) as u32, [8, 55, 254, 14], EDIT, 0x81, "");
    item(WS_TABSTOP | BS_DEFPUSHBUTTON as u32, [151, 83, 54, 15], IDOK, 0x80, "인증");
    item(WS_TABSTOP, [208, 83, 54, 15], IDCANCEL, 0x80, "취소");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vnc_windows_password_bytes_reject_invalid_input_without_truncation() {
        assert_eq!(password_bytes(&[0x61, 0xe9]).unwrap().as_slice(), &[0x61, 0xe9]);
        for invalid in [&[][..], &[0], &[0x100], &[0x61; 9]] {
            assert!(password_bytes(invalid).is_none());
        }
    }
}
