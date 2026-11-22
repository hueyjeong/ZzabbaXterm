use std::{collections::HashMap, net::TcpStream, num::NonZeroU64, path::Path, sync::{Arc, Mutex, Weak}, thread, time::Duration};

use serde::{Deserialize, Serialize};
use tauri::{Manager, Webview, ipc::{Invoke, InvokeBody}};
use tokio_util::sync::CancellationToken;

use crate::{
    connections::{profile::{MethodRef, Ssh, SshAuth, SshTransport}, with_saved_method, with_saved_ssh},
    ssh::registration::confirm_tunnel,
    workspace_window::EXPIRED,
};

use super::{Destination, Owner, Route, State, TunnelId, Tunnels, View,
    ssh::{SavedSsh, SshChild, require_supported_listener},
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
    tunnels: Tunnels<SshChild>,
}

impl Document {
    fn ensure_not_closing(&self) -> Result<(), &'static str> {
        if self.close_request.is_some() { return Err(CLOSING); }
        Ok(())
    }
}

struct Tab {
    owner: Owner,
    target: String,
    ssh: Option<Arc<SavedSsh>>,
    label: String,
    ids: Vec<TunnelId>,
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
    Ssm { listen: u16, port: u16 },
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
            Self::Ssm { .. } => return Err(self.ssm_error()),
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
        #[cfg(test)]
        Route::Ssm { listen, node, port } => format!("SSM {listen} → {node}:{port}"),
    }
}

fn status(view: View<'_>, owners: Vec<String>, entry: &Entry, target: &str) -> Status {
    let (state, end) = match view.state {
        State::Starting => ("starting", None),
        State::Ready => ("ready", None),
        State::Failed(end) => ("failed", Some(end)),
        State::Disconnected(end) => ("disconnected", Some(end)),
    };
    let desktop = match (view.state, view.route, entry.protocol) {
        (State::Ready, Route::SshLocal { server, destination, .. }, Some(protocol))
            if desktop_listener(view.route, target, destination).is_ok() => Some(DesktopStatus {
                protocol, target_id: server.clone(), destination: DesktopDestination { host: destination.host.clone(), port: destination.port },
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
    ssh: Arc<SavedSsh>,
    destination: Destination,
    protocol: Protocol,
    stream: Option<TcpStream>,
    loss: CancellationToken,
}

#[allow(dead_code, reason = "R02·R03의 네이티브 세션에서 사용")]
impl DesktopLease {
    pub(crate) fn target(&self) -> &str { &self.target }
    pub(crate) fn ssh(&self) -> (NonZeroU64, &Ssh) { (self.ssh.0, &self.ssh.1) }
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
        let ssh = match with_saved_ssh(directory, &target, |saved| saved)? {
            Some(saved) => Some(Arc::new(saved)),
            None => with_saved_method(directory, &target, |method, _| match method {
                MethodRef::Ssm { .. } => Ok(None),
                _ => Err("저장된 SSH 또는 SSM 대상이 필요합니다".into()),
            })?,
        };
        let mut doc = self.writable_document(epoch)?;
        if doc.tabs.len() >= LIMIT { return Err("열 수 있는 터널 탭 수를 초과했습니다".into()); }
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).map_err(|_| "터널 탭 ID를 발급할 수 없습니다")?;
        let handle = format!("{:032x}", u128::from_ne_bytes(nonce));
        let owner = doc.tunnels.open_tab(&epoch.to_string(), &handle).ok_or(STALE)?;
        doc.tabs.insert(handle.clone(), Tab { owner, target, ssh, label, ids: Vec::new() });
        Ok(handle)
    }

    fn start(&self, epoch: u64, directory: &Path, handle: &str, request: PortRequest,
             confirm: impl FnOnce(String) -> Result<bool, String>) -> Result<Option<String>, String> {
        if matches!(request, PortRequest::Ssm { .. }) {
            return Err(request.ssm_error());
        }
        let (target, owner, approved) = {
            let doc = self.writable_document(epoch)?;
            let tab = doc.tabs.get(handle).filter(|tab| doc.tunnels.is_open(&tab.owner)).ok_or(STALE)?;
            (tab.target.clone(), tab.owner.clone(), tab.ssh.clone())
        };
        let Some(approved) = approved else {
            return Err("저장된 직접 SSH 대상이 필요합니다".into());
        };
        let (_, ssh) = approved.as_ref();
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
        let start = doc.tunnels.begin_ssh(owner, directory, route.clone(), &route, &approved, policy)?;
        // 생성과 핸들 등록을 같은 잠금에서 완료해 문서 종료 뒤 살아남는 자식을 막는다.
        let (id, result) = start.spawn();
        doc.tunnels.finish_start(id, result).map_err(|error| error.to_string())?;
        doc.entries.insert(id, Entry { endpoint, protocol });
        doc.tabs.get_mut(handle).unwrap().ids.push(id);
        Ok(Some(id.0.to_string()))
    }

    // 승인 전에는 기록만 확인하고, 전송 상태 갱신과 소켓 발급은 승인 후에 한다.
    pub(crate) fn preflight_desktop(&self, epoch: u64, source: (&str, &str), target: &str,
                                    destination: &Destination, protocol: Protocol) -> Result<(), String> {
        let (handle, id) = source;
        let id = TunnelId(id.parse().map_err(|_| "터널 ID가 올바르지 않습니다")?);
        let doc = self.writable_document(epoch)?;
        let tab = doc.tabs.get(handle).filter(|tab| doc.tunnels.is_open(&tab.owner)).ok_or(STALE)?;
        if tab.ssh.is_none() || tab.target != target { return Err("승인된 SSH 대상이 일치하지 않습니다".into()); }
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
    pub(crate) fn open_desktop(&self, epoch: u64, directory: &Path, source: (&str, &str),
                               target: &str, destination: &Destination, protocol: Protocol,
                               confirmed_ssh: (NonZeroU64, &Ssh)) -> Result<DesktopLease, String> {
        let (handle, id) = source;
        let id = TunnelId(id.parse().map_err(|_| "터널 ID가 올바르지 않습니다")?);
        let mut doc = self.writable_document(epoch)?;
        let Document { tabs, entries, tunnels, .. } = &mut *doc;
        let tab = tabs.get(handle).filter(|tab| tunnels.is_open(&tab.owner)).ok_or(STALE)?;
        let approved = tab.ssh.as_ref().filter(|_| tab.target == target).ok_or("승인된 SSH 대상이 일치하지 않습니다")?;
        if !tab.ids.contains(&id) { return Err("이 탭이 소유한 터널이 아닙니다".into()); }
        let service = entries.get(&id).ok_or(STALE)?.protocol;
        let document = Arc::downgrade(&self.0);
        // 읽기 잠금을 소켓 검증과 등록까지 유지해 저장 설정 변경과 발급을 직렬화한다.
        with_saved_ssh(directory, target, |current| {
            if confirmed_ssh != (approved.0, &approved.1) || current.as_ref() != Some(approved.as_ref())
                || !matches!(approved.1.transport, SshTransport::Direct {})
            {
                return Err("승인된 SSH 접속 설정이 변경되었습니다".into());
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
            Ok(DesktopLease { document, epoch, id, owner: Some(owner), target: target.into(), ssh: approved.clone(),
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
        if from.target != recipient.target || from.ssh != recipient.ssh { return Err("같은 저장 대상의 탭에만 터널을 공유할 수 있습니다".into()); }
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

impl PortRequest {
    fn ssm_error(&self) -> String {
        let Self::Ssm { listen, port } = self else { unreachable!() };
        format!("공식 SSM 포트 전달의 바이너리 보존을 확인할 수 없습니다: 127.0.0.1:{listen} → {port}")
    }
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
