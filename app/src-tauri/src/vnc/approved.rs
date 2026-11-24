use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::future::pending;
use serde::Deserialize;
use tauri::{Manager, WebviewWindow, ipc::Channel};
use tokio::sync::oneshot;
use zeroize::Zeroizing;
use tokio_util::sync::CancellationToken;

use crate::{connections::SavedRoute, desktop_dialog::{Waiting, raise}, tunnels::{Destination, ipc::{Confirmed, DesktopLease, Protocol, TunnelTabs, describe_route, load_route, route_scope, route_title}}};

use super::{Auth, Error, Route, Secrets, Tab, Target, SessionId, lifetime::Lifetime, transport};
#[cfg(target_os = "linux")]
use super::vault::Frozen;

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub(super) enum Draft {
    Direct { host: String, port: u16, password: bool },
    Routed { source: Source, password: bool },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Source {
    tab_handle: String,
    tunnel_id: String,
    target_id: String,
    destination: Remote,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Remote { host: String, port: u16 }

pub(super) struct Approved {
    target: Target,
    auth: Auth,
    life: Arc<Lifetime>,
    lease: Option<DesktopLease>,
    password: Option<Password>,
    used: AtomicBool,
}

enum Password {
    #[cfg(target_os = "linux")]
    Linux(Arc<Frozen>),
    #[cfg(windows)]
    Windows(WebviewWindow),
    #[cfg(all(test, windows))]
    Fixture(Box<dyn Fn() -> Option<Zeroizing<Vec<u8>>> + Send + Sync>),
}

impl Draft {
    pub(super) async fn approve(self, window: &WebviewWindow, life: Arc<Lifetime>, epoch: u64,
                                held: &mut Option<DesktopLease>, notify: &Channel<String>) -> Result<Approved, String> {
        let (target, auth, password) = match self {
            Self::Direct { host, port, password } => {
                let auth = auth_for(password);
                let host = host.parse::<std::net::IpAddr>().map_or_else(|_| host.to_ascii_lowercase(), |ip| ip.to_string());
                let target = Target { profile_id: format!("[{host}]:{port}"), host, port };
                transport::validate(&target).map_err(|_| "VNC 호스트 또는 포트가 잘못되었습니다")?;
                let password = if auth == Auth::VncPassword { Some(Password::prepare(window, &target, &life, None).await?) } else { None };
                let service = password.as_ref().map_or_else(|| "없음".into(), |source| source.label(&target));
                let description = format!("대상: [{}]:{}\n경로: 직접 TLS (서버 인증서 이름 확인)\n서비스 인증: {service}", target.host, target.port);
                if !confirm(window, description, &life, None, notify).await { return Err(CANCELLED.into()); }
                (target, auth, password)
            }
            Self::Routed { source, password } => {
                let auth = auth_for(password);
                #[cfg(not(any(windows, target_os = "linux")))]
                if password { return Err("이 운영체제는 VNC 비밀번호 연결을 지원하지 않습니다".into()); }
                let lease = source.lease(window, &life, epoch, auth, held, notify).await?;
                let destination = lease.destination();
                let target = Target { profile_id: routed_scope(lease.route(), destination), host: destination.host.clone(), port: destination.port };
                let password = if auth == Auth::VncPassword {
                    lease.while_live(|| ()).ok_or(ROUTE_LOST)?;
                    let loss = lease.loss();
                    let password = tokio::select! {
                        biased;
                        _ = loss.cancelled() => return Err(ROUTE_LOST.into()),
                        _ = life.cancelled() => return Err(CANCELLED.into()),
                        result = Password::prepare(window, &target, &life, Some(loss.clone())) => result?,
                    };
                    #[cfg(target_os = "linux")]
                    {
                        let Password::Linux(item) = &password;
                        let description = format!("서비스 인증: VNC 비밀번호\n항목 ID: {}", item.id());
                        if !confirm(window, description, &life, Some(loss.clone()), notify).await {
                            return Err(if loss.is_cancelled() { ROUTE_LOST } else { CANCELLED }.into());
                        }
                    }
                    Some(password)
                } else { None };
                lease.while_live(|| ()).ok_or(ROUTE_LOST)?;
                (target, auth, password)
            }
        };
        Ok(Approved { target, auth, life, lease: held.take(), password, used: AtomicBool::new(false) })
    }
}

const CANCELLED: &str = "VNC 연결 승인이 취소되었습니다";
const ROUTE_LOST: &str = "VNC SSH 터널 연결이 끊겼습니다";

fn auth_for(password: bool) -> Auth { if password { Auth::VncPassword } else { Auth::ExplicitNone } }

fn routed_scope(route: &SavedRoute, destination: &Destination) -> String {
    format!("{}|vnc://[{}]:{}", route_scope(route), destination.host, destination.port)
}

impl Source {
    async fn lease<'a>(self, window: &WebviewWindow, life: &Arc<Lifetime>, epoch: u64, auth: Auth,
                       held: &'a mut Option<DesktopLease>, notify: &Channel<String>) -> Result<&'a DesktopLease, String> {
        let Self { tab_handle, tunnel_id, target_id, destination: Remote { host, port } } = self;
        if tab_handle.len() != 32 || !tab_handle.bytes().all(|b| b.is_ascii_hexdigit()) ||
            !tunnel_id.parse::<u64>().is_ok_and(|id| id > 0 && id.to_string() == tunnel_id)
        { return Err("VNC 터널 선택자가 올바르지 않습니다".into()); }
        if port == 0 || !matches!(host.as_str(), "127.0.0.1" | "::1") {
            return Err("VNC 터널은 터널 끝 호스트의 루프백 서비스만 지원합니다".into());
        }
        let app = window.app_handle().clone();
        if life.revoked() || app.state::<TunnelTabs>().epoch() != epoch { return Err(CANCELLED.into()); }
        let destination = Destination { host, port };
        let directory = window.path().app_config_dir().map_err(|_| "터널 대상 설정을 읽을 수 없습니다")?;
        let snapshot = {
            let directory = directory.clone();
            let target_id = target_id.clone();
            tauri::async_runtime::spawn_blocking(move || load_route(&directory, &target_id)).await
                .map_err(|_| "터널 대상 설정을 읽을 수 없습니다")??
        };
        let service = match auth { Auth::VncPassword => "VNC 비밀번호", Auth::ExplicitNone => "없음" };
        let description = format!("경로: 터널 {tunnel_id}\n{}\n원격 VNC 목적지: [{}]:{}\n서비스 인증: {service}",
            describe_route(&snapshot), destination.host, destination.port);
        app.state::<TunnelTabs>().preflight_desktop(epoch, (&tab_handle, &tunnel_id),
            &target_id, &destination, Protocol::Vnc)?;
        if !confirm(window, description, life, None, notify).await {
            return Err(CANCELLED.into());
        }
        let grant = life.clone();
        let (lease, verified) = tauri::async_runtime::spawn_blocking(move || {
            if grant.revoked() { return Err(CANCELLED); }
            let lease = app.state::<TunnelTabs>().open_desktop(epoch, &directory, (&tab_handle, &tunnel_id),
                &target_id, &destination, Protocol::Vnc, &snapshot)
                .map_err(|_| "승인된 VNC 터널을 열 수 없습니다")?;
            let verified = lease.approval() == Confirmed::from(&snapshot) && lease.target() == target_id && lease.destination() == &destination;
            Ok((lease, verified))
        }).await.map_err(|_| "VNC SSH 터널을 열 수 없습니다")??;
        let lease = held.insert(lease);
        if !verified { return Err("승인된 VNC 터널 신원이 변경되었습니다".into()); }
        if life.revoked() { return Err(CANCELLED.into()); }
        Ok(lease)
    }
}

pub(super) async fn route_lost(loss: Option<&CancellationToken>) {
    match loss { Some(loss) => loss.cancelled().await, None => pending().await }
}

async fn confirm(window: &WebviewWindow, description: String, life: &Arc<Lifetime>, loss: Option<CancellationToken>,
                 notify: &Channel<String>) -> bool {
    let (answer, result) = oneshot::channel();
    let parent = window.clone();
    let grant = life.clone();
    let dialog_loss = loss.clone();
    // 창을 띄우지 못하고 빠져나가도 가드가 버려지며 대기 끝 신호가 간다.
    let _waiting = Waiting::new(notify.clone());
    if window.run_on_main_thread(move || {
        let live = move || active(&grant, dialog_loss.as_ref());
        if !answer.is_closed() && live() { raise(&parent); }
        let accepted = !answer.is_closed() && live() && confirm_native(&parent, &description, live);
        let _ = answer.send(accepted);
    }).is_err() {
        return false;
    }
    result.await == Ok(true) && active(life, loss.as_ref())
}

#[cfg(target_os = "linux")]
fn confirm_native(window: &WebviewWindow, description: &str, live: impl Fn() -> bool + 'static) -> bool {
    use gtk::prelude::*;

    let Ok(parent) = window.gtk_window() else { return false; };
    let dialog = gtk::MessageDialog::new(Some(&parent), gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        gtk::MessageType::Warning, gtk::ButtonsType::OkCancel, description);
    dialog.set_title("VNC 연결 승인");
    dialog.set_default_response(gtk::ResponseType::Cancel);
    let timer = gtk::glib::timeout_add_local(std::time::Duration::from_millis(100), {
        let dialog = dialog.clone();
        move || {
            if !live() { dialog.response(gtk::ResponseType::Cancel); }
            gtk::glib::ControlFlow::Continue
        }
    });
    let accepted = dialog.run() == gtk::ResponseType::Ok;
    timer.remove();
    dialog.close();
    accepted
}

/// 취소가 기본 버튼이라 Enter로 승인되지 않는다.
#[cfg(windows)]
fn confirm_native(window: &WebviewWindow, description: &str, _: impl Fn() -> bool) -> bool {
    crate::desktop_dialog::confirm_box(window, "VNC 연결 승인", description, true)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn confirm_native(window: &WebviewWindow, description: &str, _: impl Fn() -> bool) -> bool {
    rfd::MessageDialog::new().set_parent(window).set_title("VNC 연결 승인").set_description(description)
        .set_level(rfd::MessageLevel::Warning).set_buttons(rfd::MessageButtons::OkCancel).show() == rfd::MessageDialogResult::Ok
}

impl Approved {
    pub(super) async fn open(&mut self, tab: &mut Tab) -> Result<SessionId, Error> {
        let stream = self.lease.as_mut().map(DesktopLease::take_stream).transpose().map_err(|_| Error::Disconnected)?;
        let route = match (self.lease.as_ref(), stream) {
            (Some(lease), Some(stream)) => Route::R01 { lease, stream },
            (None, None) => Route::DirectTls { trust_anchor_pem: None },
            _ => return Err(Error::UnverifiedRoute),
        };
        tab.open(&self.target, route, self.auth, self).await
    }

    pub(super) fn loss(&self) -> Option<CancellationToken> { self.lease.as_ref().map(DesktopLease::loss) }

    pub(super) fn into_lease(self) -> Option<DesktopLease> { self.lease }

    #[cfg(all(test, windows))]
    pub(super) fn fixture(target: Target, life: Arc<Lifetime>, input: impl Fn() -> Option<Zeroizing<Vec<u8>>> + Send + Sync + 'static) -> Self {
        Self { target, auth: Auth::VncPassword, life, lease: None, password: Some(Password::Fixture(Box::new(input))), used: AtomicBool::new(false) }
    }
}

impl Secrets for Approved {
    async fn vnc_password(&self, verified_target: &Target) -> Option<Zeroizing<Vec<u8>>> {
        if self.auth != Auth::VncPassword || verified_target != &self.target ||
            !self.while_live(&mut || ()) || self.used.swap(true, Ordering::AcqRel)
        { return None; }
        let secret = match self.password.as_ref()? {
            #[cfg(target_os = "linux")]
            Password::Linux(item) => {
                let item = item.clone();
                let life = self.life.clone();
                let loss = self.loss();
                tauri::async_runtime::spawn_blocking(move || item.read(|| active(&life, loss.as_ref())))
                    .await.ok().flatten()
            }
            #[cfg(windows)]
            Password::Windows(window) => {
                let display = if let Some(lease) = &self.lease {
                    format!("{}\n원격 VNC: [{}]:{}", route_title(lease.route()), verified_target.host, verified_target.port)
                } else { verified_target.profile_id.clone() };
                super::password_dialog::read(window, display, &self.life, self.loss()).await
            },
            #[cfg(all(test, windows))]
            Password::Fixture(input) => input(),
            #[cfg(not(any(windows, target_os = "linux")))]
            _ => None,
        };
        if !self.while_live(&mut || ()) { return None; }
        secret
    }

    fn while_live(&self, write: &mut dyn FnMut()) -> bool {
        self.life.live(|| match &self.lease {
            Some(lease) => lease.while_live(write).is_some(),
            None => { write(); true }
        }).unwrap_or(false)
    }
}

fn active(life: &Lifetime, loss: Option<&CancellationToken>) -> bool {
    !life.revoked() && !loss.is_some_and(CancellationToken::is_cancelled)
}

impl Password {
    #[cfg(target_os = "linux")]
    async fn prepare(_: &WebviewWindow, target: &Target, life: &Arc<Lifetime>, loss: Option<CancellationToken>) -> Result<Self, &'static str> {
        let scope = target.profile_id.clone();
        let grant = life.clone();
        tauri::async_runtime::spawn_blocking(move || {
            Frozen::freeze(&scope, || active(&grant, loss.as_ref()))
        }).await.ok().flatten().map(|item| Self::Linux(Arc::new(item)))
            .ok_or("범위와 항목 ID가 일치하는 잠금 해제된 VNC 자격 증명이 필요합니다")
    }

    #[cfg(windows)]
    async fn prepare(window: &WebviewWindow, _: &Target, _: &Arc<Lifetime>, _: Option<CancellationToken>) -> Result<Self, &'static str> {
        Ok(Self::Windows(window.clone()))
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    async fn prepare(_: &WebviewWindow, _: &Target, _: &Arc<Lifetime>, _: Option<CancellationToken>) -> Result<Self, &'static str> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn vnc_routed_draft_rejects_mixed_routes_and_unknown_fields() {
        let direct = json!({ "host": "localhost", "port": 5900, "password": false });
        let routed = json!({ "password": true, "source": { "tabHandle": "owner", "tunnelId": "1", "targetId": "target",
            "destination": { "host": "127.0.0.1", "port": 5900 } } });
        assert!(matches!(serde_json::from_value(direct.clone()), Ok(Draft::Direct { .. })));
        assert!(matches!(serde_json::from_value(routed.clone()), Ok(Draft::Routed { .. })));
        let mut mixed = direct;
        mixed["source"] = routed["source"].clone();
        assert!(serde_json::from_value::<Draft>(mixed.clone()).is_err());
        mixed["source"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<Draft>(mixed).is_err());
        let mut extra = routed;
        extra["source"]["destination"]["localPort"] = json!(5901);
        assert!(serde_json::from_value::<Draft>(extra).is_err());
    }

    #[test]
    fn vnc_routed_scope_binds_ssh_identity_and_exact_service() {
        use crate::connections::{SavedSsm, profile::{Ssh, SshAuth, SshTransport, Ssm}};
        let routed_scope = |ssh: Ssh, destination: &Destination| routed_scope(&SavedRoute::Ssh((std::num::NonZeroU64::MIN, ssh)), destination);

        let ssh = Ssh { host: "server.example".into(), port: 22, account: "operator".into(),
            host_key_fingerprint: "SHA256:pin".into(), transport: SshTransport::Direct {}, auth: SshAuth::ManualPassword {} };
        let destination = Destination { host: "127.0.0.1".into(), port: 5900 };
        let scope = routed_scope(ssh, &destination);
        assert_eq!(scope, "ssh://operator@[server.example]:22#SHA256:pin|vnc://[127.0.0.1]:5900");
        for change in 0..6 {
            let mut ssh = Ssh { host: "server.example".into(), port: 22, account: "operator".into(),
                host_key_fingerprint: "SHA256:pin".into(), transport: SshTransport::Direct {}, auth: SshAuth::ManualPassword {} };
            let mut destination = Destination { host: "127.0.0.1".into(), port: 5900 };
            match change {
                0 => ssh.host = "other.example".into(),
                1 => ssh.port = 2222,
                2 => ssh.account = "other".into(),
                3 => ssh.host_key_fingerprint = "SHA256:other".into(),
                4 => destination.host = "::1".into(),
                _ => destination.port = 5901,
            }
            assert_ne!(routed_scope(ssh, &destination), scope);
        }
        let ssm = SavedRoute::Ssm(SavedSsm { revision: std::num::NonZeroU64::MIN, desktop: None,
            target: Ssm { profile: "zzabba-demo".into(), region: "ap-northeast-1".into(), instance_id: "i-07c8e1d461943e5f3".into() } });
        assert_eq!(super::routed_scope(&ssm, &destination),
            "ssm://zzabba-demo@ap-northeast-1/i-07c8e1d461943e5f3|vnc://[127.0.0.1]:5900");
    }
}
