use std::path::PathBuf;
use tauri::{WebviewWindow, Window};

use super::Rejection;

pub(super) fn pick(window: &Window) -> Result<Option<PathBuf>, Rejection> {
    #[cfg(target_os = "linux")]
    return on_gtk(window, pick_gtk);
    #[cfg(not(target_os = "linux"))]
    Ok(rfd::FileDialog::new().set_parent(window)
        .set_title("SSH 서버 공개 키 선택").add_filter("OpenSSH 공개 키", &["pub"]).pick_file())
}

pub(super) fn confirm(window: &Window, title: &'static str, description: String) -> Result<bool, Rejection> {
    #[cfg(target_os = "linux")]
    return on_gtk(window, move |parent| confirm_gtk(parent, title, &description));
    #[cfg(not(target_os = "linux"))]
    Ok(rfd::MessageDialog::new().set_parent(window)
        .set_title(title).set_description(description)
        .set_buttons(rfd::MessageButtons::OkCancel).show() == rfd::MessageDialogResult::Ok)
}

#[cfg(target_os = "linux")]
fn on_gtk<T: Send + 'static>(window: &Window, show: impl FnOnce(&gtk::ApplicationWindow) -> T + Send + 'static) -> Result<T, Rejection> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let parent = window.clone();
    window.run_on_main_thread(move || {
        let _ = tx.send(parent.gtk_window().map(|parent| show(&parent)));
    }).map_err(|_| Rejection::StoreUnavailable)?;
    rx.recv().map_err(|_| Rejection::StoreUnavailable)?.map_err(|_| Rejection::StoreUnavailable)
}

#[cfg(target_os = "linux")]
use gtk::prelude::*;

#[cfg(target_os = "linux")]
fn key_dialog(parent: &gtk::ApplicationWindow) -> gtk::FileChooserDialog {
    let dialog = gtk::FileChooserDialog::with_buttons(
        Some("SSH 서버 공개 키 선택"), Some(parent), gtk::FileChooserAction::Open,
        &[("취소", gtk::ResponseType::Cancel), ("선택", gtk::ResponseType::Accept)],
    );
    dialog.set_modal(true);
    dialog.set_destroy_with_parent(true);
    dialog.set_local_only(true);
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("OpenSSH 공개 키"));
    filter.add_pattern("*.[pP][uU][bB]");
    dialog.add_filter(filter);
    dialog
}

#[cfg(target_os = "linux")]
fn confirmation_dialog(parent: &gtk::ApplicationWindow, title: &str, description: &str) -> gtk::MessageDialog {
    let dialog = gtk::MessageDialog::new(
        Some(parent), gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        gtk::MessageType::Question, gtk::ButtonsType::OkCancel, description,
    );
    dialog.set_title(title);
    dialog.set_default_response(gtk::ResponseType::Cancel);
    dialog
}

#[cfg(target_os = "linux")]
fn pick_gtk(parent: &gtk::ApplicationWindow) -> Option<PathBuf> {
    let dialog = key_dialog(parent);
    let path = (dialog.run() == gtk::ResponseType::Accept).then(|| dialog.filename()).flatten();
    dialog.close();
    path
}

#[cfg(target_os = "linux")]
fn confirm_gtk(parent: &gtk::ApplicationWindow, title: &str, description: &str) -> bool {
    let dialog = confirmation_dialog(parent, title, description);
    let approved = dialog.run() == gtk::ResponseType::Ok;
    dialog.close();
    approved
}

#[cfg(all(test, target_os = "linux"))]
#[test]
#[ignore = "격리 X11 디스플레이 필요"]
fn native_dialogs_are_modal_transients_of_the_same_window() {
    gtk::init().unwrap();
    let app = gtk::Application::new(None, Default::default());
    app.register(None::<&gtk::gio::Cancellable>).unwrap();
    let parent = gtk::ApplicationWindow::new(&app);
    let picker = key_dialog(&parent);
    let confirm = confirmation_dialog(&parent, super::HOST_CONFIRM_TITLE, "dev.example.invalid:2222");
    let parent: gtk::Window = parent.upcast();
    for dialog in [picker.upcast::<gtk::Window>(), confirm.upcast::<gtk::Window>()] {
        assert_eq!(dialog.transient_for().as_ref(), Some(&parent));
        assert!(dialog.is_modal());
        assert!(dialog.must_destroy_with_parent());
        dialog.close();
    }
}

/// SSH 서버 키 확인 창의 종류다.
#[derive(Clone, Copy)]
pub(super) enum Decision {
    Trust,
    Replace,
}

impl Decision {
    /// 제목, 본문 위 질문, 승인 단추, 경고 아이콘 여부다.
    fn text(self) -> (&'static str, &'static str, &'static str, bool) {
        match self {
            Self::Trust => (super::host_trust::TRUST_TITLE, "이 서버 키를 신뢰할까요?", "신뢰", false),
            Self::Replace => (super::host_trust::REPLACE_TITLE, "서버 키를 교체할까요?", "교체", true),
        }
    }
}

/// 창의 수명이다. 탭 취소 토큰이 취소되거나 5분이 지나면 창을 닫고 거절로 본다.
struct Lifetime {
    cancel: tokio_util::sync::CancellationToken,
    start: std::time::Instant,
}

impl Lifetime {
    fn ended(&self) -> bool {
        self.cancel.is_cancelled() || self.start.elapsed() >= std::time::Duration::from_secs(300)
    }
}

/// SSH 서버 키 신뢰·교체 창이다. RDP·VNC 승인 창처럼 메인 스레드에서 주 창을 앞으로 올린 뒤 띄우고, 기본 단추는 취소다.
/// 작업 스레드에서만 부른다.
pub(super) fn decide(window: &WebviewWindow, decision: Decision, description: String, cancel: &tokio_util::sync::CancellationToken)
    -> Result<bool, Rejection>
{
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let parent = window.clone();
    let lifetime = Lifetime { cancel: cancel.clone(), start: std::time::Instant::now() };
    window.run_on_main_thread(move || {
        if lifetime.ended() {
            let _ = tx.send(false);
            return;
        }
        crate::desktop_dialog::raise(&parent);
        let _ = tx.send(decide_native(&parent, decision, &description, &lifetime));
    }).map_err(|_| Rejection::StoreUnavailable)?;
    rx.recv().map_err(|_| Rejection::StoreUnavailable)
}

/// 사용자 단추(ID 100)와 취소 단추를 둔 작업 대화상자다. 결과가 100일 때만 승인이다.
#[cfg(windows)]
fn decide_native(window: &WebviewWindow, decision: Decision, description: &str, lifetime: &Lifetime) -> bool {
    use std::ptr::null_mut;
    use windows_sys::Win32::UI::{Controls::*, WindowsAndMessaging::IDCANCEL};
    const ACCEPT: i32 = 100;
    let Ok(parent) = window.hwnd() else { return false; };
    let (title, instruction, accept, warning) = decision.text();
    let wide = |text: &str| text.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (title, instruction, description, accept) = (wide(title), wide(instruction), wide(description), wide(accept));
    let buttons = [TASKDIALOG_BUTTON { nButtonID: ACCEPT, pszButtonText: accept.as_ptr() }];
    let mut config = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: parent.0 as _,
        dwFlags: TDF_CALLBACK_TIMER | TDF_ALLOW_DIALOG_CANCELLATION | TDF_SIZE_TO_CONTENT,
        dwCommonButtons: TDCBF_CANCEL_BUTTON,
        pszWindowTitle: title.as_ptr(),
        pszMainInstruction: instruction.as_ptr(),
        pszContent: description.as_ptr(),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: IDCANCEL,
        pfCallback: Some(task_event),
        lpCallbackData: lifetime as *const Lifetime as isize,
        ..Default::default()
    };
    if warning {
        config.Anonymous1.pszMainIcon = TD_WARNING_ICON;
    }
    let mut button = 0;
    // 대화상자와 콜백은 같은 스레드에서 실행되며 인자로 넘긴 값은 호출이 끝날 때까지 살아 있다.
    (unsafe { TaskDialogIndirect(&config, &mut button, null_mut(), null_mut()) } == 0) && !lifetime.ended() && button == ACCEPT
}

#[cfg(windows)]
unsafe extern "system" fn task_event(hwnd: windows_sys::Win32::Foundation::HWND, message: u32, _: usize, _: isize, context: isize) -> i32 {
    use windows_sys::Win32::UI::{Controls::{TDM_CLICK_BUTTON, TDN_TIMER}, WindowsAndMessaging::{IDCANCEL, SendMessageW}};
    if message == TDN_TIMER as u32 && unsafe { &*(context as *const Lifetime) }.ended() {
        unsafe { SendMessageW(hwnd, TDM_CLICK_BUTTON as u32, IDCANCEL as usize, 0); }
    }
    0
}

#[cfg(target_os = "linux")]
fn decide_native(window: &WebviewWindow, decision: Decision, description: &str, lifetime: &Lifetime) -> bool {
    use std::{cell::Cell, rc::Rc};
    let Ok(parent) = window.gtk_window() else { return false; };
    let (title, instruction, accept, warning) = decision.text();
    let kind = if warning { gtk::MessageType::Warning } else { gtk::MessageType::Question };
    let dialog = gtk::MessageDialog::new(Some(&parent), gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        kind, gtk::ButtonsType::None, instruction);
    dialog.set_secondary_text(Some(description));
    dialog.add_button("취소", gtk::ResponseType::Cancel);
    dialog.add_button(accept, gtk::ResponseType::Accept);
    dialog.set_title(title);
    dialog.set_default_response(gtk::ResponseType::Cancel);
    // 수명이 끝나면 취소로 닫는다. 창이 닫힌 뒤에는 타이머가 스스로 멈춘다.
    let done = Rc::new(Cell::new(false));
    let (watched, finished) = (dialog.clone(), Rc::clone(&done));
    let lifetime = Lifetime { cancel: lifetime.cancel.clone(), start: lifetime.start };
    gtk::glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
        if finished.get() {
            return gtk::glib::ControlFlow::Break;
        }
        if lifetime.ended() {
            watched.response(gtk::ResponseType::Cancel);
            return gtk::glib::ControlFlow::Break;
        }
        gtk::glib::ControlFlow::Continue
    });
    let accepted = dialog.run() == gtk::ResponseType::Accept;
    done.set(true);
    dialog.close();
    accepted
}

#[cfg(not(any(windows, target_os = "linux")))]
fn decide_native(window: &WebviewWindow, decision: Decision, description: &str, _lifetime: &Lifetime) -> bool {
    let (title, instruction, accept, _) = decision.text();
    rfd::MessageDialog::new().set_parent(window).set_title(title).set_description(format!("{instruction}\n\n{description}"))
        .set_buttons(rfd::MessageButtons::OkCancelCustom(accept.into(), "취소".into()))
        .show() == rfd::MessageDialogResult::Custom(accept.into())
}
