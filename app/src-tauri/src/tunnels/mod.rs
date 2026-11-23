use std::{
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    sync::Mutex,
};

use tokio_util::sync::CancellationToken;

pub(crate) mod ipc;
pub(crate) mod ssh;
pub(crate) mod ssm;
pub(crate) mod transport;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct TunnelId(u64);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Owner {
    // 통합 계층이 확인한 창 인스턴스와 탭 인스턴스의 식별자다.
    pub(crate) window: String,
    pub(crate) tab: String,
    generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Destination {
    pub(crate) host: String,
    pub(crate) port: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Route {
    SshLocal { server: String, listen: SocketAddr, destination: Destination },
    SshRemote { server: String, listen: SocketAddr, destination: Destination },
    SshDynamic { server: String, listen: SocketAddr },
    // host가 없으면 노드 자신의 포트, 있으면 노드에서 접속하는 원격 호스트다.
    Ssm { listen: SocketAddr, node: String, port: u16, host: Option<String> },
}

pub(crate) enum Observation {
    Pending,
    Listening,
    Ended(String),
}

pub(crate) trait Transport {
    /// `Listening`은 이 핸들의 실제 전달 확인이며 대상 서비스의 응답과는 별개다.
    fn observe(&mut self) -> Observation;

    /// 이 전달이 생성한 자식만 정리하고 종료를 확인한다.
    /// 오류가 나면 미확인 자원을 다시 정리할 수 있도록 같은 핸들을 유지한다.
    fn close(&mut self) -> io::Result<()>;
}

#[derive(Debug)]
pub(crate) struct End {
    pub(crate) reason: String,
    pub(crate) cleanup_error: Option<io::Error>,
}

#[derive(Debug)]
pub(crate) enum State {
    Starting,
    Ready,
    Failed(End),
    Disconnected(End),
}

pub(crate) struct View<'a> {
    pub(crate) id: TunnelId,
    pub(crate) route: &'a Route,
    pub(crate) owners: &'a HashSet<Owner>,
    pub(crate) state: &'a State,
}

struct Tunnel<T> {
    route: Route,
    owners: HashSet<Owner>,
    state: State,
    transport: Option<T>,
    desktop_loss: Option<CancellationToken>,
}

impl<T> Tunnel<T> {
    fn revoke_desktops(&self) {
        if let Some(loss) = &self.desktop_loss {
            loss.cancel();
        }
    }
}

fn next_id() -> u64 {
    static NEXT: Mutex<u64> = Mutex::new(1);
    let mut next = NEXT.lock().unwrap();
    let id = *next;
    *next = id.checked_add(1).expect("터널 수명 ID를 모두 사용했습니다");
    id
}

struct Cleanup<T> {
    transports: Vec<T>,
    observed_windows: HashSet<String>,
    desktop: Option<DesktopCleanup>,
}

struct DesktopCleanup {
    window: String,
    error: String,
}

fn close_owned<T: Transport>(
    cleanup: &mut HashMap<TunnelId, Cleanup<T>>,
    id: TunnelId,
    mut transport: T,
) -> io::Result<()> {
    match transport.close() {
        Ok(()) => Ok(()),
        Err(error) => {
            cleanup.entry(id).or_insert_with(|| Cleanup {
                transports: Vec::new(),
                observed_windows: HashSet::new(),
                desktop: None,
            }).transports.push(transport);
            Err(error)
        }
    }
}

fn close_released<T: Transport>(
    cleanup: &mut HashMap<TunnelId, Cleanup<T>>,
    id: TunnelId,
    tunnel: Tunnel<T>,
    window: &str,
    desktop: bool,
) -> io::Result<()> {
    tunnel.revoke_desktops();
    let result = match tunnel.transport {
        Some(transport) => close_owned(cleanup, id, transport),
        None => match tunnel.state {
            State::Failed(end) | State::Disconnected(end) if desktop => end.cleanup_error.map_or(Ok(()), Err),
            _ => Ok(()),
        },
    };
    if let Err(error) = &result {
        let pending = cleanup.get_mut(&id).unwrap();
        if desktop {
            pending.desktop = Some(DesktopCleanup { window: window.into(), error: error.to_string() });
        } else {
            pending.observed_windows.insert(window.into());
        }
    }
    result
}

pub(crate) struct Tunnels<T: Transport> {
    entries: HashMap<TunnelId, Tunnel<T>>,
    open_tabs: HashMap<String, HashMap<String, u64>>,
    cleanup: HashMap<TunnelId, Cleanup<T>>,
    closed_windows: HashSet<String>,
}

impl<T: Transport> Default for Tunnels<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            open_tabs: HashMap::new(),
            cleanup: HashMap::new(),
            closed_windows: HashSet::new(),
        }
    }
}

impl<T: Transport> Tunnels<T> {
    // 통합 계층이 탭을 실제로 열 때 등록하고 이벤트에는 발급된 Owner를 사용한다.
    pub(crate) fn open_tab(&mut self, window: &str, tab: &str) -> Option<Owner> {
        if self.closed_windows.contains(window) {
            return None;
        }
        let tabs = self.open_tabs.entry(window.into()).or_default();
        if tabs.contains_key(tab) {
            return None;
        }
        let generation = next_id();
        tabs.insert(tab.into(), generation);
        Some(Owner { window: window.into(), tab: tab.into(), generation })
    }

    fn is_open(&self, owner: &Owner) -> bool {
        self.open_tabs.get(&owner.window).and_then(|tabs| tabs.get(&owner.tab))
            == Some(&owner.generation)
    }

    pub(crate) fn begin(&mut self, owner: Owner, route: Route) -> Option<TunnelId> {
        if !self.is_open(&owner) {
            return None;
        }
        let id = TunnelId(next_id());
        self.entries.insert(id, Tunnel {
            route,
            owners: HashSet::from([owner]),
            state: State::Starting,
            transport: None,
            desktop_loss: None,
        });
        Some(id)
    }

    // 시작 작업은 레지스트리 밖에서 실행하고 발급받은 ID로만 결과를 돌려준다.
    // 핸들을 얻은 뒤 시작이 실패하면 Ok로 전달하고 observe에서 Ended를 돌려준다.
    pub(crate) fn finish_start(&mut self, id: TunnelId, result: Result<T, String>) -> io::Result<bool> {
        let Some(tunnel) = self.entries.get_mut(&id).filter(|tunnel| {
            matches!(tunnel.state, State::Starting) && tunnel.transport.is_none()
        }) else {
            if let Ok(transport) = result {
                close_owned(&mut self.cleanup, id, transport)?;
            }
            return Ok(false);
        };
        match result {
            Ok(transport) => tunnel.transport = Some(transport),
            Err(reason) => tunnel.state = State::Failed(End { reason, cleanup_error: None }),
        }
        Ok(true)
    }

    pub(crate) fn share(&mut self, id: TunnelId, owner: &Owner, recipient: Owner) -> bool {
        if !self.is_open(owner) || !self.is_open(&recipient) {
            return false;
        }
        let Some(tunnel) = self.entries.get_mut(&id) else { return false };
        if !tunnel.owners.contains(owner) || !matches!(tunnel.state, State::Starting | State::Ready) {
            return false;
        }
        tunnel.owners.insert(recipient);
        true
    }

    fn retain_desktop(&mut self, id: TunnelId, source: &Owner) -> Option<Owner> {
        if !self.is_open(source) { return None; }
        let tunnel = self.entries.get_mut(&id)?;
        if !tunnel.owners.contains(source) || !matches!(tunnel.state, State::Ready) { return None; }
        // 그래픽 소유자는 원본 탭과 수명이 다르며 터널 IPC 탭으로 등록하지 않는다.
        let owner = Owner { window: source.window.clone(), tab: String::new(), generation: next_id() };
        tunnel.owners.insert(owner.clone());
        Some(owner)
    }

    fn revoke_desktops(&self) {
        for tunnel in self.entries.values() {
            tunnel.revoke_desktops();
        }
    }

    pub(crate) fn observe(&mut self, id: TunnelId, owner: &Owner) -> Option<View<'_>> {
        if !self.entries.get(&id)?.owners.contains(owner) { return None; }
        let tunnel = self.observe_route(id)?;
        Some(View { id, route: &tunnel.route, owners: &tunnel.owners, state: &tunnel.state })
    }

    fn observe_route(&mut self, id: TunnelId) -> Option<&mut Tunnel<T>> {
        let tunnel = self.entries.get_mut(&id)?;
        if let Some(transport) = &mut tunnel.transport {
            match transport.observe() {
                Observation::Pending => {}
                Observation::Listening => tunnel.state = State::Ready,
                Observation::Ended(reason) => {
                    tunnel.revoke_desktops();
                    let transport = tunnel.transport.take().unwrap();
                    let cleanup_error = close_owned(&mut self.cleanup, id, transport).err();
                    if cleanup_error.is_some() {
                        self.cleanup.get_mut(&id).unwrap().observed_windows
                            .extend(tunnel.owners.iter().map(|owner| owner.window.clone()));
                    }
                    let end = End { reason, cleanup_error };
                    tunnel.state = if matches!(tunnel.state, State::Ready) {
                        State::Disconnected(end)
                    } else {
                        State::Failed(end)
                    };
                }
            }
        }
        Some(tunnel)
    }

    // 개별 연결만 해제한다. 터널 IPC 탭 자체의 종료에는 close_tab을 사용한다.
    pub(crate) fn release(&mut self, id: TunnelId, owner: &Owner) -> io::Result<bool> {
        let Some(tunnel) = self.entries.get_mut(&id) else { return Ok(false) };
        if !tunnel.owners.remove(owner) {
            return Ok(false);
        }
        if tunnel.owners.is_empty() {
            let tunnel = self.entries.remove(&id).unwrap();
            close_released(&mut self.cleanup, id, tunnel, &owner.window, owner.tab.is_empty())?;
        } else if !tunnel.owners.iter().any(|owner| owner.tab.is_empty())
            && let Some(loss) = tunnel.desktop_loss.take()
        {
            loss.cancel();
        }
        Ok(true)
    }

    fn desktop_cleanup_error(&self, id: TunnelId, window: &str) -> Option<&str> {
        let pending = self.cleanup.get(&id)?.desktop.as_ref()?;
        (pending.window == window).then_some(pending.error.as_str())
    }

    fn retry_desktop_cleanup(&mut self, id: TunnelId, window: &str) -> io::Result<()> {
        if self.desktop_cleanup_error(id, window).is_none() { return Ok(()); }
        self.retry_cleanup(id).map(|_| ())
    }

    fn confirm_desktop_cleanup(&mut self, window: &str, mut forget: impl FnMut(TunnelId)) -> io::Result<()> {
        let mut pending: Vec<_> = self.cleanup.iter().filter_map(|(id, pending)| {
            pending.desktop.as_ref().filter(|desktop| desktop.window == window).map(|_| *id)
        }).collect();
        let mut failure = None;
        // 터널 탭이 없는 전달만 이 문서의 그래픽 소유자와 함께 철회한다.
        for (id, tunnel) in self.entries.extract_if(|_, tunnel| {
            tunnel.owners.iter().all(|owner| owner.window == window && owner.tab.is_empty())
        }) {
            forget(id);
            let already_pending = self.cleanup.contains_key(&id);
            if let Err(error) = close_released(&mut self.cleanup, id, tunnel, window, true) {
                if already_pending { pending.push(id); }
                else { failure = Some(error); }
            }
        }
        for id in pending {
            if let Err(error) = self.retry_desktop_cleanup(id, window) { failure = Some(error); }
        }
        failure.map_or(Ok(()), Err)
    }

    pub(crate) fn close_tab(&mut self, owner: &Owner) -> Vec<(TunnelId, io::Error)> {
        if !self.is_open(owner) {
            return Vec::new();
        }
        self.open_tabs.get_mut(&owner.window).unwrap().remove(&owner.tab);
        self.detach(|candidate| candidate == owner)
    }

    pub(crate) fn close_window(&mut self, window: &str) -> Vec<(TunnelId, io::Error)> {
        self.closed_windows.insert(window.into());
        self.open_tabs.remove(window);
        let observed: Vec<_> = self.cleanup.iter()
            .filter(|(_, pending)| pending.observed_windows.contains(window)
                || pending.desktop.as_ref().is_some_and(|desktop| desktop.window == window))
            .map(|(id, _)| *id)
            .collect();
        let mut errors = self.detach(|owner| owner.window == window);
        for id in observed {
            if let Err(error) = self.retry_cleanup(id) {
                errors.push((id, error));
            }
        }
        errors
    }

    fn detach(&mut self, matches: impl Fn(&Owner) -> bool) -> Vec<(TunnelId, io::Error)> {
        let mut errors = Vec::new();
        self.entries.retain(|id, tunnel| {
            tunnel.owners.retain(|owner| !matches(owner));
            if !tunnel.owners.is_empty() {
                return true;
            }
            tunnel.revoke_desktops();
            if let Some(transport) = tunnel.transport.take()
                && let Err(error) = close_owned(&mut self.cleanup, *id, transport)
            {
                errors.push((*id, error));
            }
            false
        });
        errors
    }

    // Rust 호출자가 실패를 돌려받은 ID로만 미확인 자원의 종료를 다시 시도한다.
    pub(crate) fn retry_cleanup(&mut self, id: TunnelId) -> io::Result<bool> {
        let Some(pending) = self.cleanup.get_mut(&id) else { return Ok(false) };
        let mut failure = None;
        pending.transports.retain_mut(|transport| match transport.close() {
            Ok(()) => false,
            Err(error) => {
                failure = Some(error);
                true
            }
        });
        if let Some(error) = failure {
            if let Some(desktop) = &mut pending.desktop { desktop.error = error.to_string(); }
            return Err(error);
        }
        self.cleanup.remove(&id);
        if let Some(tunnel) = self.entries.get_mut(&id)
            && let State::Failed(end) | State::Disconnected(end) = &mut tunnel.state
        {
            end.cleanup_error = None;
        }
        Ok(true)
    }
}

impl<T: Transport> Drop for Tunnels<T> {
    fn drop(&mut self) {
        self.revoke_desktops();
        for tunnel in self.entries.values_mut() {
            if let Some(transport) = &mut tunnel.transport {
                let _ = transport.close();
            }
        }
        for pending in self.cleanup.values_mut() {
            for transport in &mut pending.transports {
                let _ = transport.close();
            }
        }
    }
}
