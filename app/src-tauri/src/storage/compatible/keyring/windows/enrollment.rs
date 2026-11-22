use std::{ptr::null_mut, time::{Duration, Instant}};

use tokio::sync::{oneshot, watch};
use windows_sys::Win32::{
    Foundation::HWND,
    UI::{Controls::{EM_EMPTYUNDOBUFFER, EM_SETLIMITTEXT}, WindowsAndMessaging::*},
};
use zeroize::Zeroizing;

use super::{KeyAccessError, MAX_FIELD_UNITS, valid_payload};

const DESCRIPTION: i32 = 100;
const KEY_ID: i32 = 101;
const SECRET: i32 = 102;
const TIMER: usize = 1;
const TIMEOUT: Duration = Duration::from_secs(300);
const DIALOG_USER: i32 = (2 * size_of::<isize>()) as i32;
const MAX_DESCRIPTION_UNITS: usize = 2048;
type Payload = Zeroizing<Vec<u8>>;

pub(crate) async fn prompt<R: tauri::Runtime>(
    window: tauri::Window<R>, description: String, closed: watch::Receiver<bool>,
) -> Result<Payload, KeyAccessError> {
    if description.is_empty() || description.contains('\0')
        || description.encode_utf16().count() > MAX_DESCRIPTION_UNITS
    {
        return Err(KeyAccessError::Invalid);
    }
    let (send, reply) = oneshot::channel();
    let owner = window.clone();
    let expires = Instant::now() + TIMEOUT;
    window.run_on_main_thread(move || {
        let mut state = Dialog { send, closed, expires, result: Err(KeyAccessError::Cancelled) };
        if state.revoked() { return; }
        let result = match owner.hwnd() {
            Ok(parent) => {
                let template = template(&description.replace('\n', "\r\n"));
                // 동기 모달이 반환할 때까지 템플릿과 콜백 상태를 유지한다.
                let code = unsafe {
                    DialogBoxIndirectParamW(null_mut(), template.0.as_ptr().cast(), parent.0.cast(),
                        Some(dialog_proc), &raw mut state as isize)
                };
                if state.revoked() { Err(KeyAccessError::Cancelled) }
                else if code == -1 { Err(KeyAccessError::Unavailable) }
                else { state.result }
            }
            Err(_) => Err(KeyAccessError::Unavailable),
        };
        let _ = state.send.send(result);
    }).map_err(|_| KeyAccessError::Unavailable)?;
    tokio::time::timeout(TIMEOUT, reply).await
        .map_err(|_| KeyAccessError::Cancelled)?
        .map_err(|_| KeyAccessError::Cancelled)?
}

struct Dialog {
    send: oneshot::Sender<Result<Payload, KeyAccessError>>,
    closed: watch::Receiver<bool>,
    expires: Instant,
    result: Result<Payload, KeyAccessError>,
}

impl Dialog {
    fn revoked(&self) -> bool {
        Instant::now() >= self.expires || self.send.is_closed()
            || *self.closed.borrow() || self.closed.has_changed().is_err()
    }
}

unsafe extern "system" fn dialog_proc(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize {
    unsafe {
        if msg == WM_INITDIALOG {
            SetWindowLongPtrW(hwnd, DIALOG_USER, lparam);
            let state = &mut *(lparam as *mut Dialog);
            if state.revoked() || SetTimer(hwnd, TIMER, 100, None) == 0 {
                if !state.revoked() { state.result = Err(KeyAccessError::Unavailable); }
                EndDialog(hwnd, IDCANCEL as isize);
                return 1;
            }
            for id in [KEY_ID, SECRET] {
                // 한 글자 초과 입력을 저장하지 않고 거부할 수 있도록 감지 공간을 둔다.
                SendDlgItemMessageW(hwnd, id, EM_SETLIMITTEXT, MAX_FIELD_UNITS + 1, 0);
            }
            SendDlgItemMessageW(hwnd, DESCRIPTION, EM_SETLIMITTEXT, MAX_DESCRIPTION_UNITS * 2, 0);
            return 1;
        }
        let data = GetWindowLongPtrW(hwnd, DIALOG_USER) as *mut Dialog;
        if data.is_null() { return 0; }
        let command = if msg == WM_COMMAND { (wparam & 0xffff) as i32 } else { 0 };
        if msg == WM_DESTROY {
            KillTimer(hwnd, TIMER);
            return 0;
        }
        let revoked = (*data).revoked();
        if msg == WM_CLOSE || command == IDCANCEL || (msg == WM_TIMER && wparam == TIMER && revoked)
            || command == IDOK
        {
            if command == IDOK && !revoked {
                (*data).result = read_payload(hwnd);
            }
            for id in [KEY_ID, SECRET] {
                SetDlgItemTextW(hwnd, id, [0_u16].as_ptr());
                SendDlgItemMessageW(hwnd, id, EM_EMPTYUNDOBUFFER, 0, 0);
            }
            EndDialog(hwnd, if command == IDOK && !revoked { IDOK } else { IDCANCEL } as isize);
            return 1;
        }
    }
    0
}

fn read_payload(hwnd: HWND) -> Result<Payload, KeyAccessError> {
    let mut payload = Zeroizing::new(Vec::with_capacity(2 * 3 * MAX_FIELD_UNITS + 1));
    for id in [KEY_ID, SECRET] {
        let mut wide = Zeroizing::new([0_u16; MAX_FIELD_UNITS + 2]);
        let len = unsafe { GetDlgItemTextW(hwnd, id, wide.as_mut_ptr(), wide.len() as i32) } as usize;
        if !(1..=MAX_FIELD_UNITS).contains(&len) { return Err(KeyAccessError::Invalid); }
        if id == SECRET { payload.push(b'\n'); }
        for scalar in char::decode_utf16(wide[..len].iter().copied()) {
            let mut bytes = Zeroizing::new([0_u8; 4]);
            payload.extend_from_slice(scalar.map_err(|_| KeyAccessError::Invalid)?.encode_utf8(&mut *bytes).as_bytes());
        }
    }
    if valid_payload(&payload) { Ok(payload) } else { Err(KeyAccessError::Invalid) }
}

// DLGTEMPLATE와 각 DLGITEMTEMPLATE는 DWORD 경계에 둔다.
#[repr(align(4))]
struct Template([u16; 4608]);

fn template(description: &str) -> Template {
    let mut out = Template([0; 4608]);
    let style = WS_POPUP | WS_CAPTION | WS_SYSMENU | (DS_MODALFRAME | DS_CENTER | DS_SETFONT) as u32;
    let header = [style as u16, (style >> 16) as u16, 0, 0, 7, 0, 0, 320, 161, 0, 0];
    let mut pos = 0;
    for word in header.into_iter().chain("S3 호환 키 등록".encode_utf16()).chain([0, 9])
        .chain("Segoe UI".encode_utf16()).chain([0])
    {
        out.0[pos] = word;
        pos += 1;
    }
    let mut item = |style: u32, [x, y, w, h]: [u16; 4], id: i32, class: u16, label: &str| {
        pos += pos % 2;
        let style = style | WS_CHILD | WS_VISIBLE;
        let fixed = [style as u16, (style >> 16) as u16, 0, 0, x, y, w, h, id as u16, 0xffff, class];
        for word in fixed.into_iter().chain(label.encode_utf16()).chain([0, 0]) {
            out.0[pos] = word;
            pos += 1;
        }
    };
    item(WS_TABSTOP | BS_DEFPUSHBUTTON as u32, [257, 138, 55, 15], IDCANCEL, 0x80, "취소");
    item(WS_TABSTOP | WS_BORDER | WS_VSCROLL | (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32,
        [8, 8, 304, 55], DESCRIPTION, 0x81, description);
    item(0, [8, 69, 304, 10], -1, 0x82, "액세스 키 ID");
    item(WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32, [8, 81, 304, 14], KEY_ID, 0x81, "");
    item(0, [8, 101, 304, 10], -1, 0x82, "비밀 액세스 키");
    item(WS_TABSTOP | WS_BORDER | (ES_PASSWORD | ES_AUTOHSCROLL) as u32, [8, 113, 304, 14], SECRET, 0x81, "");
    item(WS_TABSTOP, [199, 138, 55, 15], IDOK, 0x80, "등록");
    out
}
