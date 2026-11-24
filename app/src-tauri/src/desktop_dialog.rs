//! RDP·VNC 네이티브 승인 창이 함께 쓰는 전경 처리와 승인 대기 신호다.

use tauri::{UserAttentionType, WebviewWindow, ipc::Channel};

/// 승인·계정·인증서 창을 띄우고 닫을 때 화면 탭의 출력 채널로 보내는 신호다.
pub(crate) const APPROVAL_WAITING: &str = "approval";
pub(crate) const APPROVAL_DONE: &str = "approval-done";

/// 만들 때 승인 대기를, 버릴 때 대기 끝을 탭에 알린다. 창을 띄우지 못하고 빠져나가도 끝 신호가 간다.
pub(crate) struct Waiting(Channel<String>);

impl Waiting {
    pub(crate) fn new(notify: Channel<String>) -> Self {
        let _ = notify.send(APPROVAL_WAITING.into());
        Self(notify)
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        let _ = self.0.send(APPROVAL_DONE.into());
    }
}

/// 메인 스레드에서 창을 띄우기 직전에 부른다. 최소화를 풀고, 앱이 이미 전경일 때만 포커스를 옮긴다.
/// 백그라운드에서 set_focus를 부르면 tao가 SetForegroundWindow 실패 뒤 다른 앱에 Alt 입력을 넣으므로 작업 표시줄 알림만 쓴다.
pub(crate) fn raise(window: &WebviewWindow) {
    let _ = window.unminimize();
    if foreground(window) {
        let _ = window.set_focus();
    } else {
        let _ = window.request_user_attention(Some(UserAttentionType::Informational));
    }
}

#[cfg(windows)]
fn foreground(_: &WebviewWindow) -> bool {
    use windows_sys::Win32::{System::Threading::GetCurrentProcessId,
        UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId}};
    let mut process = 0;
    unsafe {
        let hwnd = GetForegroundWindow();
        !hwnd.is_null() && GetWindowThreadProcessId(hwnd, &mut process) != 0 && process == GetCurrentProcessId()
    }
}

#[cfg(not(windows))]
fn foreground(window: &WebviewWindow) -> bool {
    window.is_focused().unwrap_or(false)
}

/// Windows 확인 창이다. 취소가 기본 버튼이라 Enter로 승인되지 않고, 주 창 뒤에 숨지 않게 전경으로 띄운다.
#[cfg(windows)]
pub(crate) fn confirm_box(window: &WebviewWindow, title: &str, description: &str, warning: bool) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IDOK, MB_DEFBUTTON2, MB_ICONQUESTION, MB_ICONWARNING, MB_OKCANCEL, MB_SETFOREGROUND, MessageBoxW,
    };
    let wide = |text: &str| text.encode_utf16().chain([0]).collect::<Vec<_>>();
    let text = wide(description);
    let title = wide(title);
    let Ok(hwnd) = window.hwnd() else { return false; };
    let icon = if warning { MB_ICONWARNING } else { MB_ICONQUESTION };
    unsafe {
        MessageBoxW(hwnd.0 as _, text.as_ptr(), title.as_ptr(), MB_OKCANCEL | icon | MB_DEFBUTTON2 | MB_SETFOREGROUND) == IDOK
    }
}
