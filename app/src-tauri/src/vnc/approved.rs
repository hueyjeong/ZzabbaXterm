use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use serde::Deserialize;
use tauri::WebviewWindow;
use tokio::sync::oneshot;
use zeroize::Zeroizing;

use super::{Auth, Error, Route, Secrets, Tab, Target, SessionId, lifetime::Lifetime, transport};
#[cfg(target_os = "linux")]
use super::vault::Frozen;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Draft {
    host: String,
    port: u16,
    password: bool,
}

pub(super) struct Approved {
    target: Target,
    auth: Auth,
    life: Arc<Lifetime>,
    password: Option<Password>,
    used: AtomicBool,
}

enum Password {
    #[cfg(target_os = "linux")]
    Linux(Frozen),
    #[cfg(windows)]
    Windows(WebviewWindow),
    #[cfg(all(test, windows))]
    Fixture(Box<dyn Fn() -> Option<Zeroizing<Vec<u8>>> + Send + Sync>),
}

impl Draft {
    pub(super) async fn approve(self, window: &WebviewWindow, life: Arc<Lifetime>) -> Result<Approved, &'static str> {
        let (target, auth) = self.validate()?;
        let password = if auth == Auth::VncPassword { Some(Password::prepare(window, &target, &life).await?) } else { None };
        if life.revoked() { return Err("VNC 연결 승인이 취소되었습니다"); }
        let service = password.as_ref().map_or_else(|| "서비스 인증 없음".to_owned(), |source| source.label(&target));
        let description = format!("대상: [{}]:{}\n경로: 직접 TLS (서버 인증서 이름 확인)\n서비스 인증: {service}", target.host, target.port);
        if !confirm(window, description, &life).await {
            return Err("VNC 연결 승인이 취소되었습니다");
        }
        Ok(Approved { target, auth, life, password, used: AtomicBool::new(false) })
    }

    fn validate(self) -> Result<(Target, Auth), &'static str> {
        let host = self.host.parse::<std::net::IpAddr>().map_or_else(
            |_| self.host.to_ascii_lowercase(), |ip| ip.to_string(),
        );
        let target = Target { profile_id: format!("[{host}]:{}", self.port), host, port: self.port };
        transport::validate(&target).map_err(|_| "VNC 호스트 또는 포트가 잘못되었습니다")?;
        Ok((target, if self.password { Auth::VncPassword } else { Auth::ExplicitNone }))
    }
}

async fn confirm(window: &WebviewWindow, description: String, life: &Arc<Lifetime>) -> bool {
    let (answer, result) = oneshot::channel();
    let parent = window.clone();
    let grant = life.clone();
    if window.run_on_main_thread(move || {
        let _ = answer.send(!grant.revoked() && confirm_native(&parent, &description));
    }).is_err() {
        return false;
    }
    result.await == Ok(true) && !life.revoked()
}

#[cfg(target_os = "linux")]
fn confirm_native(window: &WebviewWindow, description: &str) -> bool {
    use gtk::prelude::*;

    let Ok(parent) = window.gtk_window() else { return false; };
    let dialog = gtk::MessageDialog::new(Some(&parent), gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        gtk::MessageType::Warning, gtk::ButtonsType::OkCancel, description);
    dialog.set_title("VNC 연결 승인");
    dialog.set_default_response(gtk::ResponseType::Cancel);
    let accepted = dialog.run() == gtk::ResponseType::Ok;
    dialog.close();
    accepted
}

#[cfg(not(target_os = "linux"))]
fn confirm_native(window: &WebviewWindow, description: &str) -> bool {
    rfd::MessageDialog::new().set_parent(window).set_title("VNC 연결 승인").set_description(description)
        .set_level(rfd::MessageLevel::Warning).set_buttons(rfd::MessageButtons::OkCancel).show() == rfd::MessageDialogResult::Ok
}

impl Approved {
    pub(super) async fn open(self, tab: &mut Tab) -> Result<SessionId, Error> {
        tab.open(&self.target, Route::DirectTls { trust_anchor_pem: None }, self.auth, &self).await
    }

    #[cfg(all(test, windows))]
    pub(super) fn fixture(target: Target, life: Arc<Lifetime>, input: impl Fn() -> Option<Zeroizing<Vec<u8>>> + Send + Sync + 'static) -> Self {
        Self { target, auth: Auth::VncPassword, life, password: Some(Password::Fixture(Box::new(input))), used: AtomicBool::new(false) }
    }
}

impl Secrets for Approved {
    async fn vnc_password(&self, verified_target: &Target) -> Option<Zeroizing<Vec<u8>>> {
        if self.life.revoked() || self.auth != Auth::VncPassword || verified_target != &self.target || self.used.swap(true, Ordering::AcqRel) {
            return None;
        }
        let secret = match self.password.as_ref()? {
            #[cfg(target_os = "linux")]
            Password::Linux(item) => item.read(|| !self.life.revoked()),
            #[cfg(windows)]
            Password::Windows(window) => super::password_dialog::read(window, verified_target, &self.life).await,
            #[cfg(all(test, windows))]
            Password::Fixture(input) => input(),
            #[cfg(not(any(windows, target_os = "linux")))]
            _ => None,
        };
        if self.life.revoked() { return None; }
        secret
    }

    fn while_live(&self, write: &mut dyn FnMut()) -> bool { self.life.live(write).is_some() }
}

impl Password {
    #[cfg(target_os = "linux")]
    async fn prepare(_: &WebviewWindow, target: &Target, life: &Arc<Lifetime>) -> Result<Self, &'static str> {
        let scope = target.profile_id.clone();
        let grant = life.clone();
        tauri::async_runtime::spawn_blocking(move || {
            if grant.revoked() { return None; }
            Frozen::freeze(&scope)
        }).await.ok().flatten().map(Self::Linux)
            .ok_or("범위와 항목 ID가 일치하는 잠금 해제된 VNC 자격 증명이 필요합니다")
    }

    #[cfg(windows)]
    async fn prepare(window: &WebviewWindow, _: &Target, _: &Arc<Lifetime>) -> Result<Self, &'static str> {
        Ok(Self::Windows(window.clone()))
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    async fn prepare(_: &WebviewWindow, _: &Target, _: &Arc<Lifetime>) -> Result<Self, &'static str> {
        Err("이 운영체제는 VNC 비밀번호 연결을 지원하지 않습니다")
    }

    fn label(&self, _target: &Target) -> String {
        match self {
            #[cfg(target_os = "linux")]
            Self::Linux(item) => format!("VNC 비밀번호\n금고 범위: {}\n항목 ID: {}", _target.profile_id, item.id()),
            #[cfg(windows)]
            Self::Windows(_) => "VNC 비밀번호 (검증 후 일회 입력)".into(),
            #[cfg(all(test, windows))]
            Self::Fixture(_) => unreachable!(),
            #[cfg(not(any(windows, target_os = "linux")))]
            _ => unreachable!(),
        }
    }
}
