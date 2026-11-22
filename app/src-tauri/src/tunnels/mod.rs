use std::{
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    sync::Mutex,
};

pub(crate) mod ipc;
pub(crate) mod ssh;
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
    #[cfg(test)]
    Ssm { listen: SocketAddr, node: String, port: u16 },
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
            }).transports.push(transport);
            Err(error)
        }
    }
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

    pub(crate) fn observe(&mut self, id: TunnelId, owner: &Owner) -> Option<View<'_>> {
        let tunnel = self.entries.get_mut(&id)?;
        if !tunnel.owners.contains(owner) {
            return None;
        }
        if let Some(transport) = &mut tunnel.transport {
            match transport.observe() {
                Observation::Pending => {}
                Observation::Listening => tunnel.state = State::Ready,
                Observation::Ended(reason) => {
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
        Some(View { id, route: &tunnel.route, owners: &tunnel.owners, state: &tunnel.state })
    }

    // 열린 탭의 개별 연결만 해제한다. 탭 자체의 종료에는 close_tab을 사용한다.
    #[cfg(test)]
    pub(crate) fn release(&mut self, id: TunnelId, owner: &Owner) -> io::Result<bool> {
        let Some(tunnel) = self.entries.get_mut(&id) else { return Ok(false) };
        if !tunnel.owners.remove(owner) {
            return Ok(false);
        }
        if tunnel.owners.is_empty()
            && let Some(transport) = self.entries.remove(&id).unwrap().transport
        {
            close_owned(&mut self.cleanup, id, transport)?;
        }
        Ok(true)
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
            .filter(|(_, pending)| pending.observed_windows.contains(window))
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
