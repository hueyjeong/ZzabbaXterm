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
/// 백그라운드면 포커스를 가져가지 않고 포커스를 받을 때까지 작업 표시줄을 깜빡인다.
pub(crate) fn raise(window: &WebviewWindow) {
    let _ = window.unminimize();
    if foreground(window) {
        focus(window);
    } else {
        let _ = window.request_user_attention(Some(UserAttentionType::Critical));
    }
}

/// tao의 set_focus는 SetForegroundWindow가 실패하면 Alt 입력을 우회해 넣는다. Windows에서는 직접 부르고 실패는 무시한다.
#[cfg(windows)]
fn focus(window: &WebviewWindow) {
    use windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
    if let Ok(hwnd) = window.hwnd() {
        unsafe { SetForegroundWindow(hwnd.0 as _); }
    }
}

#[cfg(not(windows))]
fn focus(window: &WebviewWindow) {
    let _ = window.set_focus();
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
