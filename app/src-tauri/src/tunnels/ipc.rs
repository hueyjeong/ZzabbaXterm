use std::{collections::HashMap, net::TcpStream, path::Path, sync::{Arc, Mutex}};

use serde::{Deserialize, Serialize};
use tauri::{Manager, Webview, ipc::{Invoke, InvokeBody}};

use crate::{
    connections::{profile::{MethodRef, Ssh, SshAuth, SshTransport}, with_saved_method, with_saved_ssh},
    ssh::registration::confirm_tunnel,
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
const LIMIT: usize = 32;

#[derive(Default)]
pub(crate) struct TunnelTabs(Mutex<Document>);

#[derive(Default)]
struct Document {
    epoch: u64,
    destroyed: bool,
    approving: bool,
    tabs: HashMap<String, Tab>,
    entries: HashMap<TunnelId, Entry>,
    tunnels: Tunnels<SshChild>,
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

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
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
    }
}

fn status(view: View<'_>, owners: Vec<String>, entry: &Entry) -> Status {
    let (state, end) = match view.state {
        State::Starting => ("starting", None),
        State::Ready => ("ready", None),
        State::Failed(end) => ("failed", Some(end)),
        State::Disconnected(end) => ("disconnected", Some(end)),
    };
    Status { id: view.id.0.to_string(), endpoint: entry.endpoint.clone(), route: route_label(view.route), owners, state,
        error: end.map(|end| match &end.cleanup_error {
            Some(error) => format!("{} · 종료 확인 불가: {error}", end.reason),
            None => end.reason.clone(),
        }) }
}

#[allow(dead_code, reason = "R02·R03의 네이티브 세션에서 사용")]
pub(crate) struct DesktopRoute<'a> {
    pub(crate) target: &'a str,
    pub(crate) ssh: &'a Ssh,
    pub(crate) destination: &'a Destination,
    pub(crate) protocol: Protocol,
}

impl TunnelTabs {
    pub(crate) fn navigate(&self, destroyed: bool) {
        let mut doc = self.0.lock().unwrap();
        if doc.destroyed { return; }
        let window = doc.epoch.to_string();
        doc.epoch += 1;
        doc.destroyed = destroyed;
        doc.tabs.clear();
        doc.entries.clear();
        let mut old = std::mem::take(&mut doc.tunnels);
        drop(doc);
        for (id, error) in old.close_window(&window) {
            if let Err(retry) = old.retry_cleanup(id) {
                eprintln!("SSH 터널 종료 확인 불가: {error}; {retry}");
            }
        }
    }

    fn document(&self, epoch: u64) -> Result<std::sync::MutexGuard<'_, Document>, String> {
        let doc = self.0.lock().unwrap();
        if doc.epoch != epoch || doc.destroyed { return Err(STALE.into()); }
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
        let mut doc = self.document(epoch)?;
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
            let doc = self.document(epoch)?;
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
            let mut doc = self.document(epoch)?;
            if !doc.tunnels.is_open(&owner) { return Err(STALE.into()); }
            if doc.approving { return Err("열려 있는 SSH 터널 승인 창을 먼저 닫으세요".into()); }
            if doc.entries.len() >= LIMIT { return Err("열 수 있는 터널 수를 초과했습니다".into()); }
            doc.approving = true;
        }
        let accepted = confirm(format!("{endpoint}\n{}\n{}{policy_label}{service}", ssh.host_key_fingerprint, route_label(&route)));
        let mut doc = self.0.lock().unwrap();
        doc.approving = false;
        if doc.destroyed || doc.epoch != epoch || !doc.tunnels.is_open(&owner) { return Err(STALE.into()); }
        if !accepted? { return Ok(None); }
        let start = doc.tunnels.begin_ssh(owner, directory, route.clone(), &route, &approved, policy)?;
        // 생성과 핸들 등록을 같은 잠금에서 완료해 문서 종료 뒤 살아남는 자식을 막는다.
        let (id, result) = start.spawn();
        doc.tunnels.finish_start(id, result).map_err(|error| error.to_string())?;
        doc.entries.insert(id, Entry { endpoint, protocol });
        doc.tabs.get_mut(handle).unwrap().ids.push(id);
        Ok(Some(id.0.to_string()))
    }

    /// 로컬 리슨 주소가 아니라 승인한 실제 서비스와 프로토콜로 요청한다.
    #[allow(dead_code, reason = "R02·R03의 네이티브 세션에서 사용")]
    pub(crate) fn open_stream(&self, handle: &str, id: &str, expected: DesktopRoute<'_>) -> Result<TcpStream, String> {
        let mut doc = self.0.lock().unwrap();
        let id = TunnelId(id.parse().map_err(|_| "터널 ID가 올바르지 않습니다")?);
        let Document { tabs, entries, tunnels, .. } = &mut *doc;
        let tab = tabs.get(handle).ok_or(STALE)?;
        if tab.target != expected.target || tab.ssh.as_deref().map(|(_, ssh)| ssh) != Some(expected.ssh)
            || entries.get(&id).and_then(|entry| entry.protocol) != Some(expected.protocol)
        {
            return Err("승인된 터널 서비스가 일치하지 않습니다".into());
        }
        let view = tunnels.observe(id, &tab.owner).ok_or("이 탭이 소유한 터널이 아닙니다")?;
        if !matches!(view.state, State::Ready) {
            return Err("터널 수신이 준비되지 않았습니다".into());
        }
        let Route::SshLocal { listen, destination, .. } = view.route else { return Err("로컬 SSH 전달이 필요합니다".into()); };
        if destination != expected.destination { return Err("승인된 터널 목적지가 일치하지 않습니다".into()); }
        let address = *listen;
        tunnels.entries.get_mut(&id).and_then(|entry| entry.transport.as_mut()).ok_or(STALE)?.open_stream(address)
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
            Some(status(view, owners, entries.get(id)?))
        }).collect())
    }

    fn share(&self, epoch: u64, handle: &str, to: &str, id: TunnelId) -> Result<(), String> {
        let mut doc = self.document(epoch)?;
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
        let mut doc = self.document(epoch)?;
        let Some(tab) = doc.tabs.get(handle) else { return Ok(()) };
        let owner = tab.owner.clone();
        let ids = tab.ids.clone();
        let failures = doc.tunnels.close_tab(&owner);
        for (id, _) in failures {
            doc.tunnels.retry_cleanup(id).map_err(|error| format!("터널 종료 확인 불가: {error}"))?;
        }
        for id in ids {
            if !doc.tunnels.entries.contains_key(&id) {
                doc.tunnels.retry_cleanup(id).map_err(|error| format!("터널 종료 확인 불가: {error}"))?;
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
    let epoch = caller.state::<TunnelTabs>().0.lock().unwrap().epoch;
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
