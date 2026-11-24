use tauri::{WebviewWindow, ipc::Channel};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::desktop_dialog::{Waiting, raise};
use super::{Account, Error};

/// 네이티브 창이 열려 있는 동안 탭에 승인 대기를 표시할 출력 채널이다.
pub(super) type Notify = Channel<String>;

pub(super) async fn confirm(window: WebviewWindow, description: String, live: CancellationToken, approval: Arc<()>, notify: Notify) -> Result<(), Error> {
    let approved = dialog(window, live, approval, notify, move |window| confirm_native(&window, &description)).await?;
    if approved { Ok(()) } else { Err(Error::Closed) }
}

pub(super) async fn trust(window: WebviewWindow, request: super::trust::Request, live: CancellationToken, approval: Arc<()>, notify: Notify) -> Result<bool, Error> {
    let changed = request.changed;
    let text = request.text;
    dialog(window, live, approval, notify, move |window| {
        if changed { confirm_change_native(&window, &text) } else { confirm_native(&window, &text) }
    }).await
}

/// 취소를 기본 버튼으로 두어 Enter가 지문 교체를 승인하지 않게 한다.
#[cfg(windows)]
fn confirm_change_native(window: &WebviewWindow, description: &str) -> bool {
    crate::desktop_dialog::confirm_box(window, "RDP 서버 인증서 변경", description, true)
}

#[cfg(not(windows))]
fn confirm_change_native(window: &WebviewWindow, description: &str) -> bool {
    confirm_native(window, description)
}

/// `remembered`는 이 대상에서 마지막으로 로그인한 계정 이름이다. 계정 칸에만 미리 채운다.
pub(super) async fn account(window: WebviewWindow, target: String, remembered: Option<String>, live: CancellationToken, approval: Arc<()>, notify: Notify) -> Result<Account, Error> {
    dialog(window, live, approval, notify, move |window| account_native(&window, &target, remembered.as_deref())).await?.ok_or(Error::Credentials)
}

async fn dialog<T: Send + 'static>(window: WebviewWindow, live: CancellationToken, approval: Arc<()>, notify: Notify,
    show: impl FnOnce(WebviewWindow) -> T + Send + 'static) -> Result<T, Error> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let parent = window.clone();
    let check = live.clone();
    // 메인 스레드 예약에 실패해도 가드가 버려지며 대기 끝 신호가 간다.
    let _waiting = Waiting::new(notify);
    window.run_on_main_thread(move || {
        let _approval = approval;
        if !check.is_cancelled() {
            // 최소화되었거나 다른 창 뒤에 있으면 모달 창이 주 창 뒤에 숨는다.
            raise(&parent);
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

/// 계정 칸 값이 `계정` 또는 `도메인\계정` 형식인지 본다.
#[cfg(any(windows, target_os = "linux"))]
fn valid_login(login: &str) -> bool {
    login.trim() == login && !login.is_empty() && match login.split_once('\\') {
        Some((domain, user)) => !domain.is_empty() && !user.is_empty() && !user.contains('\\'),
        None => true,
    }
}

#[cfg(any(windows, target_os = "linux"))]
fn account_from_login(mut login: String, password: zeroize::Zeroizing<String>) -> Option<Account> {
    if !valid_login(&login) || password.is_empty() { return None; }
    let domain = login.split_once('\\').map(|(domain, _)| domain.to_owned());
    if let Some(domain) = &domain { login.drain(..=domain.len()); }
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
fn account_native(window: &WebviewWindow, target: &str, remembered: Option<&str>) -> Option<Account> {
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
    // 긴 대상 이름은 끝을 줄임표로 표시한다.
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let error = gtk::Label::new(None);
    let area = dialog.content_area();
    area.add(&label);
    area.add(&gtk::Label::new(Some("계정 (또는 도메인\\계정)")));
    area.add(&login);
    area.add(&gtk::Label::new(Some("비밀번호")));
    area.add(&password);
    area.add(&error);
    // 두 칸이 모두 차야 로그인 단추를 켠다.
    let update = {
        let (dialog, login, password, error) = (dialog.clone(), login.clone(), password.clone(), error.clone());
        move || {
            dialog.set_response_sensitive(gtk::ResponseType::Ok, !login.text().is_empty() && !password.text().is_empty());
            error.set_text("");
        }
    };
    login.connect_changed({ let update = update.clone(); move |_| update() });
    password.connect_changed({ let update = update.clone(); move |_| update() });
    dialog.show_all();
    if let Some(remembered) = remembered {
        login.set_text(remembered);
        password.grab_focus();
    }
    update();
    // 계정 형식이 틀리면 창을 닫지 않고 오류를 표시한다.
    let account = loop {
        if dialog.run() != gtk::ResponseType::Ok { break None; }
        if valid_login(login.text().as_str()) {
            break account_from_login(login.text().to_string(), zeroize::Zeroizing::new(password.text().to_string()));
        }
        error.set_text("계정 형식이 올바르지 않습니다");
        login.grab_focus();
    };
    password.set_text("");
    dialog.close();
    account
}

#[cfg(windows)]
fn confirm_native(window: &WebviewWindow, description: &str) -> bool {
    crate::desktop_dialog::confirm_box(window, "RDP 연결 승인", description, false)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn confirm_native(window: &WebviewWindow, description: &str) -> bool {
    rfd::MessageDialog::new().set_parent(window).set_title("RDP 연결 승인").set_description(description)
        .set_buttons(rfd::MessageButtons::OkCancel).show() == rfd::MessageDialogResult::Ok
}

/// 앱이 직접 만든 계정 창이다. 자격 증명 관리자의 다른 항목을 목록으로 보이지 않고 비밀번호를 저장하지 않는다.
#[cfg(windows)]
fn account_native(window: &WebviewWindow, target: &str, remembered: Option<&str>) -> Option<Account> {
    let (login, password) = super::account_dialog::prompt(window.hwnd().ok()?.0 as _, target, remembered, valid_login)?;
    account_from_login(login, password)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn account_native(_: &WebviewWindow, _: &str, _: Option<&str>) -> Option<Account> { None }
