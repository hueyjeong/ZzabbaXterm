use std::{ffi::OsString, os::windows::ffi::OsStringExt, path::PathBuf, ptr::null_mut};

use tokio::sync::watch;
use windows_sys::Win32::{
    Foundation::HWND,
    UI::{Controls::{*, Dialogs::*}, WindowsAndMessaging::*},
};

use super::Direction;

pub(super) async fn pick<R: tauri::Runtime>(
    window: tauri::Window<R>, description: String, direction: Direction, closed: watch::Receiver<bool>,
) -> Option<PathBuf> {
    let (answer, selected) = tokio::sync::oneshot::channel();
    let owner = window.clone();
    window.run_on_main_thread(move || {
        let path = owner.hwnd().ok().and_then(|parent| show(parent.0, description, direction, closed));
        let _ = answer.send(path);
    }).ok()?;
    selected.await.ok().flatten()
}

struct Lifetime {
    closed: watch::Receiver<bool>,
    start: std::time::Instant,
}

impl Lifetime {
    fn ended(&self) -> bool {
        *self.closed.borrow() || self.start.elapsed() >= std::time::Duration::from_secs(300)
    }
}

fn show(parent: HWND, description: String, direction: Direction, closed: watch::Receiver<bool>) -> Option<PathBuf> {
    let lifetime = Lifetime { closed, start: std::time::Instant::now() };
    if lifetime.ended() { return None; }
    let context = &raw const lifetime as isize;
    let title: Vec<_> = "S3 파일 선택\0".encode_utf16().collect();
    let description: Vec<_> = description.encode_utf16().chain([0]).collect();
    let confirm = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: parent,
        dwFlags: TDF_CALLBACK_TIMER | TDF_ALLOW_DIALOG_CANCELLATION | TDF_SIZE_TO_CONTENT,
        dwCommonButtons: TDCBF_OK_BUTTON | TDCBF_CANCEL_BUTTON,
        nDefaultButton: IDCANCEL,
        pszWindowTitle: title.as_ptr(),
        pszContent: description.as_ptr(),
        pfCallback: Some(confirm_event),
        lpCallbackData: context,
        ..Default::default()
    };
    let mut button = 0;
    // 대화상자와 콜백은 같은 스레드에서 실행되며 인자로 넘긴 값은 호출이 끝날 때까지 살아 있다.
    if unsafe { TaskDialogIndirect(&confirm, &mut button, null_mut(), null_mut()) } != 0
        || button != IDOK || lifetime.ended() { return None; }

    let mut path = vec![0u16; 32_768];
    let mut request = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: parent,
        lpstrTitle: title.as_ptr(),
        lpstrFile: path.as_mut_ptr(),
        nMaxFile: path.len() as u32,
        Flags: OFN_EXPLORER | OFN_ENABLEHOOK | OFN_ENABLESIZING | OFN_NOCHANGEDIR | OFN_DONTADDTORECENT | OFN_PATHMUSTEXIST,
        lpfnHook: Some(picker_event),
        lCustData: context,
        ..Default::default()
    };
    let picked = unsafe { match direction {
        Direction::Upload => { request.Flags |= OFN_FILEMUSTEXIST; GetOpenFileNameW(&mut request) }
        Direction::Download => { request.Flags |= OFN_OVERWRITEPROMPT | OFN_NOTESTFILECREATE; GetSaveFileNameW(&mut request) }
    }};
    if picked == 0 || lifetime.ended() { return None; }
    let len = path.iter().position(|unit| *unit == 0)?;
    Some(OsString::from_wide(&path[..len]).into())
}

unsafe extern "system" fn confirm_event(hwnd: HWND, message: u32, _: usize, _: isize, context: isize) -> i32 {
    if message == TDN_TIMER as u32 && unsafe { &*(context as *const Lifetime) }.ended() {
        unsafe { SendMessageW(hwnd, TDM_CLICK_BUTTON as u32, IDCANCEL as usize, 0); }
    }
    0
}

unsafe extern "system" fn picker_event(hwnd: HWND, message: u32, wparam: usize, lparam: isize) -> usize {
    match message {
        WM_INITDIALOG => unsafe {
            let request = lparam as *const OPENFILENAMEW;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*request).lCustData);
            if SetTimer(hwnd, 1, 100, None) == 0 { PostMessageW(GetParent(hwnd), WM_COMMAND, IDCANCEL as usize, 0); }
        },
        WM_TIMER if wparam == 1 => unsafe {
            let context = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Lifetime;
            if let Some(lifetime) = context.as_ref() && lifetime.ended() {
                PostMessageW(GetParent(hwnd), WM_COMMAND, IDCANCEL as usize, 0);
            }
        },
        WM_DESTROY => unsafe { KillTimer(hwnd, 1); },
        _ => {}
    }
    0
}
