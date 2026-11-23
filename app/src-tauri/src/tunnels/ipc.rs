use std::{collections::HashMap, net::TcpStream, num::NonZeroU64, path::Path, sync::{Arc, Mutex, Weak}, thread, time::Duration};

use serde::{Deserialize, Serialize};
use tauri::{Manager, Webview, ipc::{Invoke, InvokeBody}};
use tokio_util::sync::CancellationToken;

use crate::{
    connections::{SavedRoute, SavedSsm, profile::{Ssh, SshAuth, SshTransport}, with_saved_route},
    ssh::registration::confirm_tunnel,
    workspace_window::EXPIRED,
};

use super::{Destination, Observation, Owner, Route, State, Transport, TunnelId, Tunnels, View,
    ssh::{SshChild, require_supported_listener},
    ssm::{ForwardPlan, Prepared, SsmChild},
    transport::{Listener, SshPlan, SshServerPolicy}};

const OPEN: &str = "tunnel_tab_open";
const START: &str = "tunnel_start";
const STATUS: &str = "tunnel_status";
const SHARE: &str = "tunnel_share";
const CLOSE: &str = "tunnel_tab_close";
const STALE: &str = "터널 탭이 닫혔습니다";
const CLOSING: &str = "창을 닫는 중입니다";
const APPROVAL_PENDING: &str = "열려 있는 SSH 터널 승인 창을 먼저 닫으세요";
const LIMIT: usize = 32;

#[derive(Default)]
pub(crate) struct TunnelTabs(Arc<Mutex<Document>>);

#[derive(Default)]
struct Document {
    epoch: u64,
    destroyed: bool,
    approving: bool,
    close_request: Option<String>,
    tabs: HashMap<String, Tab>,
    entries: HashMap<TunnelId, Entry>,
    tunnels: Tunnels<Forward>,
}

impl Document {
    fn ensure_not_closing(&self) -> Result<(), &'static str> {
        if self.close_request.is_some() { return Err(CLOSING); }
        Ok(())
    }

    // SSM 준비 작업이 끝났을 때 같은 문서의 같은 시작 기록만 핸들을 받는다.
    fn starting(&self, epoch: u64, id: TunnelId) -> bool {
        !self.destroyed && self.epoch == epoch && self.tunnels.entries.get(&id)
            .is_some_and(|tunnel| matches!(tunnel.state, State::Starting) && tunnel.transport.is_none())
    }
}

/// 터널 등록부가 소유하는 전달 자식이다.
pub(crate) enum Forward {
    Ssh(SshChild),
    Ssm(SsmChild),
}

impl Forward {
    fn open_stream(&mut self, address: std::net::SocketAddr) -> Result<TcpStream, String> {
        match self {
            Self::Ssh(child) => child.open_stream(address),
            Self::Ssm(child) => child.open_stream(address),
        }
    }
}

impl Transport for Forward {
    fn observe(&mut self) -> Observation {
        match self {
            Self::Ssh(child) => child.observe(),
            Self::Ssm(child) => child.observe(),
        }
    }

    fn close(&mut self) -> std::io::Result<()> {
        match self {
            Self::Ssh(child) => child.close(),
            Self::Ssm(child) => child.close(),
        }
    }
}

struct Tab {
    owner: Owner,
    target: String,
    saved: Arc<SavedRoute>,
    label: String,
    ids: Vec<TunnelId>,
}

/// RDP·VNC 작업이 화면 승인 전에 읽은 저장 경로의 신원이다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Confirmed<'a> {
    Ssh(NonZeroU64, &'a Ssh),
    Ssm(&'a SavedSsm),
}

impl<'a> From<(NonZeroU64, &'a Ssh)> for Confirmed<'a> {
    fn from((revision, ssh): (NonZeroU64, &'a Ssh)) -> Self { Self::Ssh(revision, ssh) }
}

impl<'a> From<&'a SavedRoute> for Confirmed<'a> {
    fn from(saved: &'a SavedRoute) -> Self {
        match saved {
            SavedRoute::Ssh((revision, ssh)) => Self::Ssh(*revision, ssh),
            SavedRoute::Ssm(ssm) => Self::Ssm(ssm),
        }
    }
}

/// 원격 화면 경로로 쓸 수 있는 저장 대상(직접 SSH 또는 SSM)을 읽는다.
pub(crate) fn load_route(directory: &Path, target: &str) -> Result<SavedRoute, String> {
    with_saved_route(directory, target, |saved| saved)?
        .filter(|saved| !matches!(saved, SavedRoute::Ssh((_, ssh)) if !matches!(ssh.transport, SshTransport::Direct {})))
        .ok_or_else(|| "저장된 직접 SSH 또는 SSM 경로가 필요합니다".into())
}

/// 승인 창에 표시할 경로 신원이다.
pub(crate) fn describe_route(saved: &SavedRoute) -> String {
    match saved {
        SavedRoute::Ssh((_, ssh)) => format!("SSH 서버: {}:{}\nSSH 계정: {}\nSSH 호스트 키: {}",
            ssh.host, ssh.port, ssh.account, ssh.host_key_fingerprint),
        SavedRoute::Ssm(ssm) => format!("SSM 프로필: {}\nSSM 리전: {}\nSSM 관리 노드: {}",
            ssm.target.profile, ssm.target.region, ssm.target.instance_id),
    }
}

/// 같은 경로 신원에만 묶는 짧은 범위 이름이다.
pub(crate) fn route_scope(saved: &SavedRoute) -> String {
    match saved {
        SavedRoute::Ssh((_, ssh)) => format!("ssh://{}@[{}]:{}#{}", ssh.account, ssh.host, ssh.port, ssh.host_key_fingerprint),
        SavedRoute::Ssm(ssm) => format!("ssm://{}@{}/{}", ssm.target.profile, ssm.target.region, ssm.target.instance_id),
    }
}

/// 한 줄 경로 표시다.
pub(crate) fn route_title(saved: &SavedRoute) -> String {
    match saved {
        SavedRoute::Ssh((_, ssh)) => format!("SSH 터널: {}@[{}]:{}", ssh.account, ssh.host, ssh.port),
        SavedRoute::Ssm(ssm) => format!("SSM 터널: {} · {} · {}", ssm.target.profile, ssm.target.region, ssm.target.instance_id),
    }
}

struct Entry {
    endpoint: String,
    protocol: Option<Protocol>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Protocol { Rdp, Vnc }

#[derive(Deserialize)]
#[serde(tag = "direction", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
enum PortRequest {
    Local { listen: u16, host: String, port: u16, protocol: Option<Protocol> },
    Remote { listen: u16, host: String, port: u16 },
    Dynamic { listen: u16 },
    /// `host`가 없으면 노드 자신의 포트, 있으면 노드에서 접속하는 원격 호스트다.
    Ssm { listen: u16, port: u16, host: Option<String>, protocol: Option<Protocol> },
}

impl PortRequest {
    fn ssh(self, server: String) -> Result<(Route, SshServerPolicy, Option<Protocol>), String> {
        let address = |port| std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let result = match self {
            Self::Local { listen, host, port, protocol } => {
                if protocol == Some(Protocol::Vnc) && !matches!(host.as_str(), "127.0.0.1" | "::1") {
                    return Err("VNC 터널은 SSH 서버의 루프백 서비스만 지원합니다".into());
                }
                (Route::SshLocal { server, listen: address(listen), destination: Destination { host, port } },
                    SshServerPolicy::FixedLocal, protocol)
            }
            Self::Remote { listen, host, port } => {
                require_supported_listener(Listener::Remote(address(listen)))?;
                (Route::SshRemote { server, listen: address(listen), destination: Destination { host, port } },
                    SshServerPolicy::ExactRemoteLoopback, None)
            }
            Self::Dynamic { listen } => (Route::SshDynamic { server, listen: address(listen) },
                SshServerPolicy::DestinationRestrictedSocks, None),
            Self::Ssm { .. } => return Err("SSM 전달은 저장된 SSM 경로에서 시작합니다".into()),
        };
        Ok(result)
    }
}

#[derive(Serialize)]
struct Status {
    id: String,
    endpoint: String,
    route: String,
    owners: Vec<String>,
    state: &'static str,
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    desktop: Option<DesktopStatus>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DesktopStatus {
    protocol: Protocol,
    target_id: String,
    destination: DesktopDestination,
}

#[derive(Serialize)]
struct DesktopDestination {
    host: String,
    port: u16,
}

fn route_label(route: &Route) -> String {
    let forward = |listen: &std::net::SocketAddr, destination: &Destination| {
        let Destination { host, port } = destination;
        if host.contains(':') { format!("{listen} → [{host}]:{port}") }
        else { format!("{listen} → {host}:{port}") }
    };
    match route {
        Route::SshLocal { listen, destination, .. } => format!("-L {}", forward(listen, destination)),
        Route::SshRemote { listen, destination, .. } => format!("-R {}", forward(listen, destination)),
        Route::SshDynamic { listen, .. } => format!("-D {listen} → SOCKS"),
        Route::Ssm { listen, node, port, host: None } => format!("SSM {listen} → {node}:{port}"),
        Route::Ssm { listen, node, port, host: Some(host) } if host.contains(':') => format!("SSM {listen} → {node} → [{host}]:{port}"),
        Route::Ssm { listen, node, port, host: Some(host) } => format!("SSM {listen} → {node} → {host}:{port}"),
    }
}

// 노드 자신의 서비스는 문서가 노드의 localhost로 접속하므로 루프백 목적지로 표시한다.
fn ssm_destination(host: &Option<String>, port: u16) -> Destination {
    Destination { host: host.clone().unwrap_or_else(|| "127.0.0.1".into()), port }
}

fn status(view: View<'_>, owners: Vec<String>, entry: &Entry, target: &str) -> Status {
    let (state, end) = match view.state {
        State::Starting => ("starting", None),
        State::Ready => ("ready", None),
        State::Failed(end) => ("failed", Some(end)),
        State::Disconnected(end) => ("disconnected", Some(end)),
    };
    let destination = match view.route {
        Route::SshLocal { destination, .. } => Some(destination.clone()),
        Route::Ssm { host, port, .. } => Some(ssm_destination(host, *port)),
        _ => None,
    };
    let desktop = match (view.state, destination, entry.protocol) {
        (State::Ready, Some(destination), Some(protocol))
            if desktop_listener(view.route, target, &destination).is_ok() => Some(DesktopStatus {
                protocol, target_id: target.into(), destination: DesktopDestination { host: destination.host, port: destination.port },
            }),
        _ => None,
    };
    Status { id: view.id.0.to_string(), endpoint: entry.endpoint.clone(), route: route_label(view.route), owners, state, desktop,
        error: end.map(|end| match &end.cleanup_error {
            Some(error) => format!("{} · 종료 확인 불가: {error}", end.reason),
            None => end.reason.clone(),
        }) }
}

/// 소켓을 넘겨받은 작업도 종료할 때까지 이 임대를 보관한다.
#[allow(dead_code, reason = "R02·R03의 네이티브 세션에서 사용")]
pub(crate) struct DesktopLease {
    document: Weak<Mutex<Document>>,
    epoch: u64,
    id: TunnelId,
    owner: Option<Owner>,
    target: String,
    route: Arc<SavedRoute>,
    destination: Destination,
    protocol: Protocol,
    stream: Option<TcpStream>,
    loss: CancellationToken,
}

#[allow(dead_code, reason = "R02·R03의 네이티브 세션에서 사용")]
impl DesktopLease {
    pub(crate) fn target(&self) -> &str { &self.target }
    pub(crate) fn route(&self) -> &SavedRoute { &self.route }
    pub(crate) fn approval(&self) -> Confirmed<'_> { self.route.as_ref().into() }
    pub(crate) fn destination(&self) -> &Destination { &self.destination }
    pub(crate) fn protocol(&self) -> Protocol { self.protocol }
    pub(crate) fn loss(&self) -> CancellationToken { self.loss.child_token() }

    /// 철회와 직렬화할 비차단 I/O 한 번만 실행한다. 콜백은 임대나 터널 등록부에 재진입할 수 없다.
    pub(crate) fn while_live<T>(&self, f: impl FnOnce() -> T) -> Option<T> {
        let document = self.document.upgrade()?;
        let doc = document.lock().ok()?;
        if doc.destroyed || doc.epoch != self.epoch || self.loss.is_cancelled() {
            return None;
        }
        let route = doc.tunnels.entries.get(&self.id)?;
        if !matches!(route.state, State::Ready) || !route.owners.contains(self.owner.as_ref()?) {
            return None;
        }
        let result = f();
        drop(doc);
        Some(result)
    }

    pub(crate) fn take_stream(&mut self) -> Result<TcpStream, String> {
        if self.loss.is_cancelled() { return Err(STALE.into()); }
        self.stream.take().ok_or_else(|| "터널 소켓을 이미 인계했습니다".into())
    }

    /// 그래픽 세션이 인계받은 소켓을 닫은 뒤 차단 작업에서 호출한다.
    pub(crate) fn release(mut self) -> Result<(), DesktopCleanupError> {
        self.release_inner().map_err(|(owner, reason)| DesktopCleanupError {
            document: self.document.clone(), epoch: self.epoch, id: self.id, window: owner.window, reason,
        })
    }

    fn release_inner(&mut self) -> Result<(), (Owner, String)> {
        let Some(owner) = self.owner.take() else { return Ok(()) };
        let document = self.document.upgrade();
        let doc = document.as_ref().map(|document| document.lock().unwrap());
        self.loss.cancel();
        drop(self.stream.take());
        let Some(mut doc) = doc.filter(|doc| !doc.destroyed && doc.epoch == self.epoch) else { return Ok(()) };
        let result = doc.tunnels.release(self.id, &owner);
        if !doc.tunnels.entries.contains_key(&self.id) {
            doc.entries.remove(&self.id);
        }
        let reason = match result {
            Err(error) => error.to_string(),
            // 창 닫기 확인이 먼저 임대를 철회했더라도 같은 자식의 종료 오류를 돌려준다.
            Ok(false) => match doc.tunnels.desktop_cleanup_error(self.id, &owner.window) {
                Some(error) => error.into(),
                None => return Ok(()),
            },
            Ok(true) => return Ok(()),
        };
        Err((owner, reason))
    }
}

/// 같은 그래픽 탭에만 보관하는 종료 재확인 권한. 전송 경로나 프로세스를 새로 찾지 않는다.
#[derive(Debug)]
pub(crate) struct DesktopCleanupError {
    document: Weak<Mutex<Document>>,
    epoch: u64,
    id: TunnelId,
    window: String,
    reason: String,
}

impl std::fmt::Display for DesktopCleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SSH 터널 종료 확인 불가: {}", self.reason)
    }
}

impl DesktopCleanupError {
    /// 현재 문서의 같은 핸들만 재확인한다. 실패하면 같은 오류 값을 보관한다.
    pub(crate) fn retry(&mut self) -> Result<(), String> {
        let document = self.document.upgrade().ok_or(STALE)?;
        let mut doc = document.lock().unwrap();
        if doc.destroyed || doc.epoch != self.epoch { return Err(STALE.into()); }
        if let Err(error) = doc.tunnels.retry_desktop_cleanup(self.id, &self.window) {
            self.reason = error.to_string();
            return Err(self.to_string());
        }
        Ok(())
    }
}

impl Drop for DesktopLease {
    fn drop(&mut self) {
        if let Err((_, reason)) = self.release_inner() {
            // 작업이 사라져도 실패한 자식은 문서의 종료 확인 대상에 남는다.
            eprintln!("SSH 터널 종료 확인 불가: {reason}");
        }
    }
}

// 터널 탭을 닫아도 SSH 자식 종료를 확인한다. 작업은 활성 전달당 하나다.
fn watch_desktop_route(document: Weak<Mutex<Document>>, epoch: u64, id: TunnelId, loss: CancellationToken) -> std::io::Result<()> {
    thread::Builder::new().name("desktop-tunnel".into()).spawn(move || loop {
        thread::sleep(Duration::from_millis(100));
        let Some(document) = document.upgrade() else { break; };
        let mut doc = document.lock().unwrap();
        if loss.is_cancelled() || doc.epoch != epoch
            || !doc.tunnels.observe_route(id).is_some_and(|route| matches!(route.state, State::Ready))
        {
            break;
        }
    })?;
    Ok(())
}

fn desktop_listener(route: &Route, target: &str, expected: &Destination) -> Result<std::net::SocketAddr, String> {
    if let Route::Ssm { listen, port, host, .. } = route {
        // 대상 일치는 호출자가 탭의 저장 대상으로 확인한다.
        if listen.ip() != std::net::Ipv4Addr::LOCALHOST || listen.port() == 0 {
            return Err("승인된 로컬 SSM 수신 주소가 일치하지 않습니다".into());
        }
        if *port == 0 || ssm_destination(host, *port) != *expected {
            return Err("승인된 터널 목적지가 일치하지 않습니다".into());
        }
        return Ok(*listen);
    }
    let Route::SshLocal { server, listen, destination } = route else { return Err("로컬 SSH 전달이 필요합니다".into()); };
    if listen.ip() != std::net::Ipv4Addr::LOCALHOST || listen.port() == 0 {
        return Err("승인된 로컬 SSH 수신 주소가 일치하지 않습니다".into());
    }
    if server != target || destination != expected || destination.port == 0
        || !matches!(destination.host.as_str(), "127.0.0.1" | "::1")
    {
        return Err("승인된 터널 목적지가 일치하지 않습니다".into());
    }
    Ok(*listen)
}

impl TunnelTabs {
    pub(crate) fn epoch(&self) -> u64 { self.0.lock().unwrap().epoch }

    /// 작업자에서만 실행한다. 성공한 게이트는 창이 파괴될 때까지 유지한다.
    /// 실패한 닫기 요청은 입력을 복원하기 전에 같은 ID로 게이트를 해제한다.
    pub(crate) fn confirm_desktop_cleanup(&self, epoch: u64, request_id: &str, revoked: &CancellationToken) -> Result<(), String> {
        let mut doc = self.document(epoch).map_err(|_| EXPIRED)?;
        if revoked.is_cancelled() { return Err(EXPIRED.into()); }
        doc.ensure_not_closing()?;
        if doc.approving { return Err(APPROVAL_PENDING.into()); }
        doc.close_request = Some(request_id.into());
        let Document { entries, tunnels, .. } = &mut *doc;
        tunnels.confirm_desktop_cleanup(&epoch.to_string(), |id| { entries.remove(&id); })
            .map_err(|error| format!("SSH 터널 종료 확인 불가: {error}"))
    }

    pub(crate) fn release_desktop_close_gate(&self, epoch: u64, request_id: &str) {
        if let Ok(mut doc) = self.document(epoch)
            && doc.close_request.as_deref() == Some(request_id)
        {
            doc.close_request = None;
        }
    }

    pub(crate) fn navigate(&self, destroyed: bool) {
        let mut doc = self.0.lock().unwrap();
        if doc.destroyed { return; }
        let window = doc.epoch.to_string();
        doc.epoch += 1;
        doc.destroyed = destroyed;
        doc.close_request = None;
        doc.tabs.clear();
        doc.entries.clear();
        doc.tunnels.revoke_desktops();
        let mut old = std::mem::take(&mut doc.tunnels);
        drop(doc);
        for (id, error) in old.close_window(&window) {
            if let Err(retry) = old.retry_cleanup(id) {
                eprintln!("SSH 터널 종료 확인 불가: {error}; {retry}");
            }
        }
    }

    fn document(&self, epoch: u64) -> Result<std::sync::MutexGuard<'_, Document>, &'static str> {
        let doc = self.0.lock().unwrap();
        if doc.epoch != epoch || doc.destroyed { return Err(STALE); }
        Ok(doc)
    }

    fn writable_document(&self, epoch: u64) -> Result<std::sync::MutexGuard<'_, Document>, &'static str> {
        let doc = self.document(epoch)?;
        doc.ensure_not_closing()?;
        Ok(doc)
    }

    fn open(&self, epoch: u64, directory: &Path, target: String, label: String) -> Result<String, String> {
        if label.is_empty() || label.len() > 256 || label.chars().any(char::is_control) { return Err("터널 탭 이름이 올바르지 않습니다".into()); }
        let saved = Arc::new(with_saved_route(directory, &target, |saved| saved)?
            .ok_or("저장된 SSH 또는 SSM 대상이 필요합니다")?);
        let mut doc = self.writable_document(epoch)?;
        if doc.tabs.len() >= LIMIT { return Err("열 수 있는 터널 탭 수를 초과했습니다".into()); }
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).map_err(|_| "터널 탭 ID를 발급할 수 없습니다")?;
        let handle = format!("{:032x}", u128::from_ne_bytes(nonce));
        let owner = doc.tunnels.open_tab(&epoch.to_string(), &handle).ok_or(STALE)?;
        doc.tabs.insert(handle.clone(), Tab { owner, target, saved, label, ids: Vec::new() });
        Ok(handle)
    }

    fn start(&self, epoch: u64, directory: &Path, handle: &str, request: PortRequest,
             confirm: impl FnOnce(String) -> Result<bool, String>) -> Result<Option<String>, String> {
        let (target, owner, saved) = {
            let doc = self.writable_document(epoch)?;
            let tab = doc.tabs.get(handle).filter(|tab| doc.tunnels.is_open(&tab.owner)).ok_or(STALE)?;
            (tab.target.clone(), tab.owner.clone(), tab.saved.clone())
        };
        if matches!(request, PortRequest::Ssm { .. }) {
            return self.start_ssm(epoch, directory, handle, (target, owner, saved), request, confirm);
        }
        let SavedRoute::Ssh(approved) = saved.as_ref() else {
            return Err("저장된 직접 SSH 대상이 필요합니다".into());
        };
        let (_, ssh) = approved;
        if !matches!(ssh.transport, SshTransport::Direct {}) { return Err("SSH-over-SSM 전달을 사용할 수 없습니다".into()); }
        let (route, policy, protocol) = request.ssh(target)?;
        if !matches!(ssh.auth, SshAuth::Agent { .. } | SshAuth::KeyFile { .. }) {
            return Err("SSH 전달에는 Agent 또는 비대화형 키 인증이 필요합니다".into());
        }
        SshPlan::new(&route, &route, policy).map_err(|error| format!("SSH 전달 설정: {error:?}"))?;
        let host = &ssh.host;
        let host_port = if host.contains(':') { format!("[{host}]:{}", ssh.port) } else { format!("{host}:{}", ssh.port) };
        let endpoint = format!("{}@{host_port}", ssh.account);
        let policy_label = match policy {
            SshServerPolicy::FixedLocal => "",
            SshServerPolicy::ExactRemoteLoopback => "\n승인 전 서버 설정 확인: GatewayPorts no 또는 clientspecified, PermitListen에 표시된 루프백 수신 허용",
            SshServerPolicy::DestinationRestrictedSocks => "\n승인 전 서버 설정 확인: PermitOpen으로 SOCKS 목적지 제한",
        };
        let service = match protocol { Some(Protocol::Rdp) => "\n서비스: RDP", Some(Protocol::Vnc) => "\n서비스: VNC", None => "" };
        {
            let mut doc = self.writable_document(epoch)?;
            if !doc.tunnels.is_open(&owner) { return Err(STALE.into()); }
            if doc.approving { return Err(APPROVAL_PENDING.into()); }
            if doc.entries.len() >= LIMIT { return Err("열 수 있는 터널 수를 초과했습니다".into()); }
            doc.approving = true;
        }
        let accepted = confirm(format!("{endpoint}\n{}\n{}{policy_label}{service}", ssh.host_key_fingerprint, route_label(&route)));
        let mut doc = self.0.lock().unwrap();
        doc.approving = false;
        if doc.destroyed || doc.epoch != epoch || !doc.tunnels.is_open(&owner) { return Err(STALE.into()); }
        doc.ensure_not_closing()?;
        if !accepted? { return Ok(None); }
        let start = doc.tunnels.begin_ssh(owner, directory, route.clone(), &route, approved, policy)?;
        // 생성과 핸들 등록을 같은 잠금에서 완료해 문서 종료 뒤 살아남는 자식을 막는다.
        let (id, result) = start.spawn();
        doc.tunnels.finish_start(id, result.map(Forward::Ssh)).map_err(|error| error.to_string())?;
        doc.entries.insert(id, Entry { endpoint, protocol });
        doc.tabs.get_mut(handle).unwrap().ids.push(id);
        Ok(Some(id.0.to_string()))
    }

    /// 저장된 SSM 대상 또는 SSM 경로를 쓰는 원격 화면 프로필에서만 공식 포트 전달을 시작한다.
    /// 승인 후에는 시작 기록만 등록하고 응답한다. 자격 확인·노드 조회·실행은 작업자에서 이어진다.
    fn start_ssm(&self, epoch: u64, directory: &Path, handle: &str, (target, owner, saved): (String, Owner, Arc<SavedRoute>),
                 request: PortRequest, confirm: impl FnOnce(String) -> Result<bool, String>) -> Result<Option<String>, String> {
        let PortRequest::Ssm { listen, port, host, protocol } = request else { unreachable!("SSM 요청 필요") };
        let SavedRoute::Ssm(ssm) = saved.as_ref() else {
            return Err("저장된 SSM 대상이 필요합니다".into());
        };
        match &ssm.desktop {
            Some(desktop) if protocol != Some(desktop.protocol) || port != desktop.port || host != desktop.host => {
                return Err("저장된 원격 화면 경로와 전달 요청이 일치하지 않습니다".into());
            }
            None if protocol == Some(Protocol::Vnc) && host.is_some() => {
                return Err("VNC SSM 터널은 노드 자신의 서비스만 지원합니다".into());
            }
            _ => {}
        }
        let route = Route::Ssm { listen: std::net::SocketAddr::from(([127, 0, 0, 1], listen)), node: ssm.target.instance_id.clone(), port, host };
        let plan = ForwardPlan::new(&route, &route, ssm)?;
        let endpoint = format!("{} · {} · {}", ssm.target.profile, ssm.target.region, ssm.target.instance_id);
        let service = match protocol { Some(Protocol::Rdp) => "\n서비스: RDP", Some(Protocol::Vnc) => "\n서비스: VNC", None => "" };
        {
            let mut doc = self.writable_document(epoch)?;
            if !doc.tunnels.is_open(&owner) { return Err(STALE.into()); }
            if doc.approving { return Err(APPROVAL_PENDING.into()); }
            if doc.entries.len() >= LIMIT { return Err("열 수 있는 터널 수를 초과했습니다".into()); }
            doc.approving = true;
        }
        let accepted = confirm(format!("{}\n문서: {}\n매개변수: {}\n{}{service}",
            describe_route(&saved), plan.document(), plan.parameters(), route_label(&route)));
        let mut doc = self.0.lock().unwrap();
        doc.approving = false;
        if doc.destroyed || doc.epoch != epoch || !doc.tunnels.is_open(&owner) { return Err(STALE.into()); }
        doc.ensure_not_closing()?;
        if !accepted? { return Ok(None); }
        // 승인한 기록이 저장 파일의 현재 값과 같을 때만 시작한다.
        if with_saved_route(directory, &target, |current| current.as_ref() == Some(saved.as_ref()))? != true {
            return Err("승인된 SSM 대상 설정이 변경되었습니다".into());
        }
        let id = doc.tunnels.begin(owner, route).ok_or(STALE)?;
        doc.entries.insert(id, Entry { endpoint, protocol });
        doc.tabs.get_mut(handle).unwrap().ids.push(id);
        if let Err(error) = start_ssm_worker(Arc::downgrade(&self.0), epoch, id, plan) {
            doc.tunnels.finish_start(id, Err(format!("SSM 시작 작업: {error}"))).map_err(|error| error.to_string())?;
        }
        Ok(Some(id.0.to_string()))
    }

    // 승인 전에는 기록만 확인하고, 전송 상태 갱신과 소켓 발급은 승인 후에 한다.
    pub(crate) fn preflight_desktop(&self, epoch: u64, source: (&str, &str), target: &str,
                                    destination: &Destination, protocol: Protocol) -> Result<(), String> {
        let (handle, id) = source;
        let id = TunnelId(id.parse().map_err(|_| "터널 ID가 올바르지 않습니다")?);
        let doc = self.writable_document(epoch)?;
        let tab = doc.tabs.get(handle).filter(|tab| doc.tunnels.is_open(&tab.owner)).ok_or(STALE)?;
        if tab.target != target { return Err("승인된 터널 대상이 일치하지 않습니다".into()); }
        if !tab.ids.contains(&id) { return Err("이 탭이 소유한 터널이 아닙니다".into()); }
        if doc.entries.get(&id).ok_or(STALE)?.protocol != Some(protocol) {
            return Err("승인된 터널 서비스가 일치하지 않습니다".into());
        }
        let route = doc.tunnels.entries.get(&id).filter(|route| route.owners.contains(&tab.owner))
            .ok_or("이 탭이 소유한 터널이 아닙니다")?;
        if !matches!(route.state, State::Ready) { return Err("터널 수신이 준비되지 않았습니다".into()); }
        desktop_listener(&route.route, target, destination)?;
        route.transport.as_ref().ok_or(STALE)?;
        Ok(())
    }

    /// 네이티브 승인과 문서 검증을 마친 그래픽 작업에만 연결 소켓을 인계한다.
    #[allow(dead_code, reason = "R02·R03의 네이티브 세션에서 사용")]
    pub(crate) fn open_desktop<'c>(&self, epoch: u64, directory: &Path, source: (&str, &str),
                               target: &str, destination: &Destination, protocol: Protocol,
                               confirmed: impl Into<Confirmed<'c>>) -> Result<DesktopLease, String> {
        let confirmed = confirmed.into();
        let (handle, id) = source;
        let id = TunnelId(id.parse().map_err(|_| "터널 ID가 올바르지 않습니다")?);
        let mut doc = self.writable_document(epoch)?;
        let Document { tabs, entries, tunnels, .. } = &mut *doc;
        let tab = tabs.get(handle).filter(|tab| tunnels.is_open(&tab.owner)).ok_or(STALE)?;
        let approved = Some(&tab.saved).filter(|_| tab.target == target).ok_or("승인된 터널 대상이 일치하지 않습니다")?;
        if !tab.ids.contains(&id) { return Err("이 탭이 소유한 터널이 아닙니다".into()); }
        let service = entries.get(&id).ok_or(STALE)?.protocol;
        let document = Arc::downgrade(&self.0);
        // 읽기 잠금을 소켓 검증과 등록까지 유지해 저장 설정 변경과 발급을 직렬화한다.
        with_saved_route(directory, target, |current| {
            let changed = confirmed != Confirmed::from(approved.as_ref()) || current.as_ref() != Some(approved.as_ref());
            match approved.as_ref() {
                SavedRoute::Ssh((_, ssh)) if changed || !matches!(ssh.transport, SshTransport::Direct {}) =>
                    return Err("승인된 SSH 접속 설정이 변경되었습니다".into()),
                SavedRoute::Ssm(_) if changed => return Err("승인된 SSM 대상 설정이 변경되었습니다".into()),
                _ => {}
            }
            if service != Some(protocol) { return Err("승인된 터널 서비스가 일치하지 않습니다".into()); }
            let view = tunnels.observe(id, &tab.owner).ok_or("이 탭이 소유한 터널이 아닙니다")?;
            if !matches!(view.state, State::Ready) { return Err("터널 수신이 준비되지 않았습니다".into()); }
            let address = desktop_listener(view.route, target, destination)?;
            let route = tunnels.entries.get_mut(&id).ok_or(STALE)?;
            let stream = route.transport.as_mut().ok_or(STALE)?.open_stream(address)?;
            if route.desktop_loss.is_none() {
                let loss = CancellationToken::new();
                watch_desktop_route(document.clone(), epoch, id, loss.clone()).map_err(|error| format!("SSH 터널 상태 감시: {error}"))?;
                route.desktop_loss = Some(loss);
            }
            let loss = route.desktop_loss.as_ref().unwrap().child_token();
            let owner = tunnels.retain_desktop(id, &tab.owner).ok_or(STALE)?;
            Ok(DesktopLease { document, epoch, id, owner: Some(owner), target: target.into(), route: approved.clone(),
                destination: destination.clone(), protocol, stream: Some(stream), loss })
        })?
    }

    fn snapshot(&self, epoch: u64, handle: &str) -> Result<Vec<Status>, String> {
        let mut doc = self.document(epoch)?;
        let Document { tabs, tunnels, entries, .. } = &mut *doc;
        let tab = tabs.get(handle).ok_or(STALE)?;
        Ok(tab.ids.iter().filter_map(|id| {
            let view = tunnels.observe(*id, &tab.owner)?;
            let mut owners: Vec<_> = view.owners.iter().filter_map(|owner| tabs.get(&owner.tab))
                .map(|tab| tab.label.clone()).collect();
            owners.sort_unstable();
            Some(status(view, owners, entries.get(id)?, &tab.target))
        }).collect())
    }

    fn share(&self, epoch: u64, handle: &str, to: &str, id: TunnelId) -> Result<(), String> {
        let mut doc = self.writable_document(epoch)?;
        let from = doc.tabs.get(handle).ok_or(STALE)?;
        let recipient = doc.tabs.get(to).ok_or(STALE)?;
        if from.target != recipient.target || from.saved != recipient.saved { return Err("같은 저장 대상의 탭에만 터널을 공유할 수 있습니다".into()); }
        let (owner, recipient) = (from.owner.clone(), recipient.owner.clone());
        if !doc.tunnels.share(id, &owner, recipient) { return Err("공유할 수 있는 터널이 없습니다".into()); }
        let ids = &mut doc.tabs.get_mut(to).unwrap().ids;
        if !ids.contains(&id) { ids.push(id); }
        Ok(())
    }

    fn close(&self, epoch: u64, handle: &str) -> Result<(), String> {
        let mut doc = self.writable_document(epoch)?;
        let Some(tab) = doc.tabs.get(handle) else { return Ok(()) };
        let owner = tab.owner.clone();
        let ids = tab.ids.clone();
        let failures = doc.tunnels.close_tab(&owner);
        for (id, _) in failures {
            doc.tunnels.retry_cleanup(id).map_err(|error| format!("터널 종료 확인 불가: {error}"))?;
        }
        for id in ids {
            if !doc.tunnels.entries.contains_key(&id) {
                if doc.tunnels.desktop_cleanup_error(id, &owner.window).is_none() {
                    doc.tunnels.retry_cleanup(id).map_err(|error| format!("터널 종료 확인 불가: {error}"))?;
                }
                doc.entries.remove(&id);
            }
        }
        doc.tabs.remove(handle);
        Ok(())
    }
}

// SSM 준비 작업은 등록부 잠금 밖에서 실행한다. 탭이 닫히면 준비를 취소하고 자식을 만들지 않는다.
fn start_ssm_worker(document: Weak<Mutex<Document>>, epoch: u64, id: TunnelId, plan: ForwardPlan) -> std::io::Result<()> {
    thread::Builder::new().name("ssm-forward-start".into()).spawn(move || {
        let live = || document.upgrade().is_some_and(|document| document.lock().unwrap().starting(epoch, id));
        let cancel = CancellationToken::new();
        let prepared = tauri::async_runtime::block_on(async {
            let prepare = super::ssm::prepare(plan, &cancel);
            tokio::pin!(prepare);
            loop {
                tokio::select! {
                    result = &mut prepare => break result,
                    () = tokio::time::sleep(Duration::from_millis(200)) => if !live() { cancel.cancel(); },
                }
            }
        });
        let Some(document) = document.upgrade() else { return };
        let mut doc = document.lock().unwrap();
        if !doc.starting(epoch, id) { return; }
        // 생성과 핸들 등록을 같은 잠금에서 완료해 문서 종료 뒤 살아남는 자식을 막는다.
        let result = prepared.and_then(Prepared::spawn).map(Forward::Ssm);
        if let Err(error) = doc.tunnels.finish_start(id, result) {
            eprintln!("SSM 터널 종료 확인 불가: {error}");
        }
    })?;
    Ok(())
}

pub(crate) fn handles(command: &str) -> bool {
    matches!(command, OPEN | START | STATUS | SHARE | CLOSE)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Open { target: String, label: String }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start { tab: String, request: PortRequest }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TabArg { tab: String }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Share { tab: String, to: String, id: String }

fn job<T: Serialize + Send + 'static>(invoke: Invoke, work: impl FnOnce(Webview, u64) -> Result<T, String> + Send + 'static) {
    let caller = invoke.message.webview_ref().clone();
    let epoch = caller.state::<TunnelTabs>().epoch();
    invoke.resolver.respond_async(async move {
        tauri::async_runtime::spawn_blocking(move || work(caller, epoch)).await
            .map_err(|_| tauri::ipc::InvokeError::from("터널 작업을 완료할 수 없습니다"))?
            .map_err(tauri::ipc::InvokeError::from)
    });
}

pub(crate) fn dispatch(invoke: Invoke) {
    let caller = invoke.message.webview_ref();
    if invoke.acl.is_none() || caller.label() != "main" || caller.window_ref().label() != "main" {
        invoke.resolver.reject("앱의 로컬 기본 창에서만 터널을 사용할 수 있습니다");
        return;
    }
    let InvokeBody::Json(body) = invoke.message.payload() else {
        invoke.resolver.reject("터널 요청 형식이 올바르지 않습니다");
        return;
    };
    macro_rules! parse {
        ($ty:ty) => { match <$ty>::deserialize(body) {
            Ok(value) => value,
            Err(_) => { invoke.resolver.reject("터널 요청 형식이 올바르지 않습니다"); return; }
        } };
    }
    match invoke.message.command() {
        OPEN => {
            let Open { target, label } = parse!(Open);
            job(invoke, move |caller, epoch| {
                let directory = caller.path().app_config_dir().map_err(|error| error.to_string())?;
                caller.state::<TunnelTabs>().open(epoch, &directory, target, label)
            });
        }
        START => {
            let Start { tab, request } = parse!(Start);
            job(invoke, move |caller, epoch| {
                let directory = caller.path().app_config_dir().map_err(|error| error.to_string())?;
                caller.state::<TunnelTabs>().start(epoch, &directory, &tab, request, |description| confirm_tunnel(&caller.window(), description))
            });
        }
        STATUS => {
            let TabArg { tab } = parse!(TabArg);
            job(invoke, move |caller, epoch| caller.state::<TunnelTabs>().snapshot(epoch, &tab));
        }
        SHARE => {
            let Share { tab, to, id } = parse!(Share);
            job(invoke, move |caller, epoch| caller.state::<TunnelTabs>().share(epoch, &tab, &to,
                TunnelId(id.parse().map_err(|_| "터널 ID가 올바르지 않습니다")?)));
        }
        CLOSE => {
            let TabArg { tab } = parse!(TabArg);
            job(invoke, move |caller, epoch| caller.state::<TunnelTabs>().close(epoch, &tab));
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use tests::rdp_test_lease;
