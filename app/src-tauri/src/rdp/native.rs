use tauri::WebviewWindow;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::{Account, Error};

pub(super) async fn confirm(window: WebviewWindow, description: String, live: CancellationToken, approval: Arc<()>) -> Result<(), Error> {
    let approved = dialog(window, live, approval, move |window| confirm_native(&window, &description)).await?;
    if approved { Ok(()) } else { Err(Error::Closed) }
}

pub(super) async fn account(window: WebviewWindow, target: String, live: CancellationToken, approval: Arc<()>) -> Result<Account, Error> {
    dialog(window, live, approval, move |window| account_native(&window, &target)).await?.ok_or(Error::Credentials)
}

async fn dialog<T: Send + 'static>(window: WebviewWindow, live: CancellationToken, approval: Arc<()>,
    show: impl FnOnce(WebviewWindow) -> T + Send + 'static) -> Result<T, Error> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let parent = window.clone();
    let check = live.clone();
    window.run_on_main_thread(move || {
        let _approval = approval;
        if !check.is_cancelled() {
            let answer = show(parent);
            if !check.is_cancelled() { let _ = tx.send(answer); }
        }
    }).map_err(|_| Error::Closed)?;
    tokio::select! {
        biased;
        () = live.cancelled() => Err(Error::Closed),
        answer = rx => answer.map_err(|_| Error::Closed),
    }
}

#[cfg(any(windows, target_os = "linux"))]
fn account_from_login(mut login: String, password: zeroize::Zeroizing<String>) -> Option<Account> {
    if login.trim() != login || login.is_empty() || password.is_empty() { return None; }
    let domain = match login.split_once('\\') {
        Some((domain, user)) if !domain.is_empty() && !user.is_empty() && !user.contains('\\') => {
            let domain = domain.to_owned();
            login.drain(..=domain.len());
            Some(domain)
        }
        Some(_) => return None,
        None => None,
    };
    Some(Account { username: login, domain, password })
}

#[cfg(target_os = "linux")]
use gtk::prelude::*;

#[cfg(target_os = "linux")]
fn confirm_native(window: &WebviewWindow, description: &str) -> bool {
    let Ok(parent) = window.gtk_window() else { return false; };
    let dialog = gtk::MessageDialog::new(Some(&parent), gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        gtk::MessageType::Question, gtk::ButtonsType::OkCancel, description);
    dialog.set_title("RDP 연결 승인");
    dialog.set_default_response(gtk::ResponseType::Cancel);
    let accepted = dialog.run() == gtk::ResponseType::Ok;
    dialog.close();
    accepted
}

#[cfg(target_os = "linux")]
fn account_native(window: &WebviewWindow, target: &str) -> Option<Account> {
    let parent = window.gtk_window().ok()?;
    let dialog = gtk::Dialog::with_buttons(Some("RDP 계정 인증"), Some(&parent),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        &[("취소", gtk::ResponseType::Cancel), ("로그인", gtk::ResponseType::Ok)]);
    dialog.set_default_response(gtk::ResponseType::Cancel);
    let login = gtk::Entry::new();
    login.set_max_length(256);
    let password = gtk::Entry::new();
    password.set_max_length(256);
    password.set_visibility(false);
    password.set_input_purpose(gtk::InputPurpose::Password);
    let label = gtk::Label::new(Some(&format!("검증된 서버: {target}")));
    let area = dialog.content_area();
    area.add(&label);
    area.add(&gtk::Label::new(Some("계정 (또는 도메인\\계정)")));
    area.add(&login);
    area.add(&gtk::Label::new(Some("비밀번호")));
    area.add(&password);
    dialog.show_all();
    let account = (dialog.run() == gtk::ResponseType::Ok)
        .then(|| account_from_login(login.text().to_string(), zeroize::Zeroizing::new(password.text().to_string()))).flatten();
    password.set_text("");
    dialog.close();
    account
}

#[cfg(not(target_os = "linux"))]
fn confirm_native(window: &WebviewWindow, description: &str) -> bool {
    rfd::MessageDialog::new().set_parent(window).set_title("RDP 연결 승인").set_description(description)
        .set_buttons(rfd::MessageButtons::OkCancel).show() == rfd::MessageDialogResult::Ok
}

#[cfg(windows)]
fn account_native(window: &WebviewWindow, target: &str) -> Option<Account> {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Security::Credentials::{CREDUI_FLAGS_ALWAYS_SHOW_UI, CREDUI_FLAGS_DO_NOT_PERSIST,
        CREDUI_FLAGS_GENERIC_CREDENTIALS, CREDUI_FLAGS_EXCLUDE_CERTIFICATES,
        CREDUI_INFOW, CredUIPromptForCredentialsW};
    use zeroize::Zeroizing;
    let wide = |text: &str| text.encode_utf16().chain([0]).collect::<Vec<_>>();
    let title = wide("RDP 계정 인증");
    let message = wide(&format!("검증된 서버: {target}\n계정 또는 도메인\\계정"));
    let target = wide(&super::ipc::opaque()?);
    let info = CREDUI_INFOW {
        cbSize: std::mem::size_of::<CREDUI_INFOW>() as u32,
        hwndParent: window.hwnd().ok()?.0 as _, pszMessageText: message.as_ptr(),
        pszCaptionText: title.as_ptr(), hbmBanner: null_mut(),
    };
    let mut login = [0_u16; 514];
    let mut secret = Zeroizing::new([0_u16; 257]);
    let mut save = 0;
    // Generic + always show + do not persist prevents reuse and storage of the entered password.
    let result = unsafe { CredUIPromptForCredentialsW(&info, target.as_ptr(), null(), 0,
        login.as_mut_ptr(), login.len() as u32, secret.as_mut_ptr(), secret.len() as u32, &mut save,
        CREDUI_FLAGS_GENERIC_CREDENTIALS | CREDUI_FLAGS_ALWAYS_SHOW_UI | CREDUI_FLAGS_DO_NOT_PERSIST
            | CREDUI_FLAGS_EXCLUDE_CERTIFICATES) };
    if result != 0 { return None; }
    let decode = |s: &[u16]| String::from_utf16(&s[..s.iter().position(|unit| *unit == 0)?]).ok();
    account_from_login(decode(&login)?, Zeroizing::new(decode(secret.as_ref())?))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn account_native(_: &WebviewWindow, _: &str) -> Option<Account> { None }
