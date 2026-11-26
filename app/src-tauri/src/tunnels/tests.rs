use super::*;
use std::{
    cell::Cell,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    rc::Rc,
    time::Duration,
};

struct Probe {
    signal: Rc<Cell<Observation>>,
    closes: Rc<Cell<usize>>,
    close_failures: usize,
}

impl Probe {
    fn new() -> Self {
        Self {
            signal: Rc::new(Cell::new(Observation::Pending)),
            closes: Rc::default(),
            close_failures: 0,
        }
    }
}

impl Transport for Probe {
    fn observe(&mut self) -> Observation {
        self.signal.replace(Observation::Pending)
    }

    fn close(&mut self) -> io::Result<()> {
        self.closes.set(self.closes.get() + 1);
        if self.close_failures > 0 {
            self.close_failures -= 1;
            return Err(io::Error::other("cleanup failed"));
        }
        Ok(())
    }
}

impl Transport for Option<TcpListener> {
    fn observe(&mut self) -> Observation {
        assert!(self.is_some());
        Observation::Listening
    }

    fn close(&mut self) -> io::Result<()> {
        drop(self.take());
        Ok(())
    }
}

fn route(listen: SocketAddr) -> Route {
    Route::SshDynamic { server: "test".into(), listen }
}

fn local() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

#[test]
fn tunnels_observe_only_the_owned_transport_and_keep_terminal_state() {
    let mut tunnels = Tunnels::default();
    let a = tunnels.open_tab("one", "a").unwrap();
    let b = tunnels.open_tab("two", "b").unwrap();
    let outsider = tunnels.open_tab("two", "a").unwrap();
    let destination = Destination { host: "test".into(), port: 123 };
    let id = tunnels.begin(a.clone(), Route::SshLocal {
        server: "test".into(), listen: local(), destination,
    }).unwrap();
    let other = tunnels.begin(outsider.clone(), Route::Ssm {
        listen: local(), node: "test".into(), port: 456, host: None,
    }).unwrap();
    let mut probe = Probe::new();
    probe.close_failures = 1;
    let (signal, closes) = (probe.signal.clone(), probe.closes.clone());
    let independent = Probe::new();
    independent.signal.set(Observation::Listening);
    let independent_closes = independent.closes.clone();
    assert!(tunnels.finish_start(other, Ok(independent)).unwrap());
    assert!(tunnels.finish_start(id, Ok(probe)).unwrap());
    assert!(!tunnels.share(id, &outsider, b.clone()));
    assert!(tunnels.share(id, &a, b.clone()));
    assert!(tunnels.share(id, &a, b.clone()));
    let view = tunnels.observe(id, &a).unwrap();
    assert_eq!(view.id, id);
    assert!(matches!(view.route, Route::SshLocal { destination, .. } if destination.port == 123));
    assert_eq!(*view.owners, HashSet::from([a.clone(), b.clone()]));
    assert!(matches!(view.state, State::Starting));

    signal.set(Observation::Listening);
    assert!(tunnels.observe(id, &outsider).is_none());
    assert!(!tunnels.release(id, &outsider).unwrap());
    assert!(matches!(tunnels.observe(id, &b).unwrap().state, State::Ready));
    assert!(matches!(tunnels.observe(id, &a).unwrap().state, State::Ready));
    signal.set(Observation::Ended("exited".into()));
    for owner in [&a, &b] {
        assert!(matches!(tunnels.observe(id, owner).unwrap().state,
            State::Disconnected(End { reason, cleanup_error: Some(error) })
            if reason == "exited" && error.to_string() == "cleanup failed"));
    }
    assert_eq!(closes.get(), 1);
    assert!(matches!(tunnels.observe(other, &outsider).unwrap().state, State::Ready));
    assert_eq!(independent_closes.get(), 0);
    assert!(!tunnels.share(id, &a, outsider.clone()));
    assert!(tunnels.retry_cleanup(id).unwrap());
    assert!(matches!(tunnels.observe(id, &b).unwrap().state,
        State::Disconnected(End { cleanup_error: None, .. })));
    assert!(tunnels.release(id, &a).unwrap());
    assert!(tunnels.observe(id, &a).is_none());
    assert!(tunnels.release(id, &b).unwrap());
    assert_eq!(closes.get(), 2);
    drop(tunnels);
    assert_eq!(independent_closes.get(), 1);
}

#[test]
fn tunnels_close_during_start_and_ignore_late_results_after_reopen() {
    for by_window in [false, true] {
        let mut tunnels = Tunnels::default();
        let a = tunnels.open_tab("one", "a").unwrap();
        let id = tunnels.begin(a.clone(), route(local())).unwrap();
        if by_window {
            assert!(tunnels.close_window(&a.window).is_empty());
            assert!(tunnels.begin(a.clone(), route(local())).is_none());
        } else {
            assert!(tunnels.close_tab(&a).is_empty());
        }
        let b = tunnels.open_tab(if by_window { "two" } else { "one" }, "a").unwrap();
        let replacement = tunnels.begin(b.clone(), route(local())).unwrap();
        assert_ne!(id, replacement);
        let current = Probe::new();
        current.signal.set(Observation::Listening);
        let current_closes = current.closes.clone();
        assert!(tunnels.finish_start(replacement, Ok(current)).unwrap());
        let late = Probe::new();
        late.signal.set(Observation::Listening);
        let late_closes = late.closes.clone();
        assert!(!tunnels.finish_start(id, Ok(late)).unwrap());
        assert_eq!(late_closes.get(), 1);
        assert!(!tunnels.release(id, &a).unwrap());
        assert!(tunnels.observe(id, &a).is_none());
        if by_window {
            assert!(!tunnels.share(replacement, &b, a));
        }
        assert!(matches!(tunnels.observe(replacement, &b).unwrap().state, State::Ready));
        assert_eq!(current_closes.get(), 0);
        assert!(tunnels.release(replacement, &b).unwrap());
        assert_eq!(current_closes.get(), 1);
    }
}

#[test]
fn tunnels_report_exit_before_ready_as_start_failure() {
    let mut tunnels = Tunnels::default();
    let a = tunnels.open_tab("one", "a").unwrap();
    let id = tunnels.begin(a.clone(), route(local())).unwrap();
    let probe = Probe::new();
    probe.signal.set(Observation::Ended("early exit".into()));
    let closes = probe.closes.clone();
    assert!(tunnels.finish_start(id, Ok(probe)).unwrap());
    assert!(matches!(tunnels.observe(id, &a).unwrap().state,
        State::Failed(End { reason, cleanup_error: None }) if reason == "early exit"));
    assert_eq!(closes.get(), 1);
}

fn expect_response(addr: SocketAddr, listener: &TcpListener, response: &[u8; 8]) {
    let timeout = Duration::from_secs(2);
    let mut client = TcpStream::connect_timeout(&addr, timeout).unwrap();
    client.set_read_timeout(Some(timeout)).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_write_timeout(Some(timeout)).unwrap();
    peer.write_all(response).unwrap();
    let mut received = [0; 8];
    client.read_exact(&mut received).unwrap();
    assert_eq!(&received, response);
}

#[test]
fn tunnels_keep_shared_listener_until_last_owner_and_close_only_it() {
    let listener = TcpListener::bind(local()).unwrap();
    let addr = listener.local_addr().unwrap();
    let other_listener = TcpListener::bind(local()).unwrap();
    let other_addr = other_listener.local_addr().unwrap();
    let mut tunnels = Tunnels::default();
    let a = tunnels.open_tab("one", "a").unwrap();
    let b = tunnels.open_tab("two", "b").unwrap();
    let c = tunnels.open_tab("two", "c").unwrap();
    let id = tunnels.begin(a.clone(), route(addr)).unwrap();
    let other = tunnels.begin(c.clone(), route(other_addr)).unwrap();
    assert!(tunnels.finish_start(id, Ok(Some(listener))).unwrap());
    assert!(tunnels.finish_start(other, Ok(Some(other_listener))).unwrap());
    assert!(tunnels.share(id, &a, b.clone()));
    assert!(tunnels.close_window(&a.window).is_empty());
    assert!(!tunnels.release(id, &a).unwrap());
    assert!(matches!(tunnels.observe(id, &b).unwrap().state, State::Ready));
    expect_response(addr, tunnels.entries[&id].transport.as_ref().unwrap().as_ref().unwrap(), b"shared!!");
    let timeout = Duration::from_secs(2);
    assert!(tunnels.release(id, &b).unwrap());
    assert!(tunnels.observe(id, &b).is_none());
    assert!(matches!(TcpStream::connect_timeout(&addr, timeout).unwrap_err().kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut));
    expect_response(other_addr, tunnels.entries[&other].transport.as_ref().unwrap().as_ref().unwrap(), b"other!!!");
    assert!(tunnels.close_window(&c.window).is_empty());
    assert!(matches!(TcpStream::connect_timeout(&other_addr, timeout).unwrap_err().kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut));
}

#[test]
fn tunnels_bind_conflict_leaves_the_external_listener_responding() {
    let external = TcpListener::bind(local()).unwrap();
    let addr = external.local_addr().unwrap();
    let mut tunnels = Tunnels::<Option<TcpListener>>::default();
    let a = tunnels.open_tab("one", "a").unwrap();
    let id = tunnels.begin(a.clone(), Route::SshRemote {
        server: "test".into(), listen: addr, destination: Destination { host: "test".into(), port: 123 },
    }).unwrap();
    assert!(matches!(tunnels.observe(id, &a).unwrap().state, State::Starting));
    let error = TcpListener::bind(addr).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    assert!(tunnels.finish_start(id, Err(error.to_string())).unwrap());
    let view = tunnels.observe(id, &a).unwrap();
    assert!(matches!(view.route, Route::SshRemote { listen, .. } if *listen == addr));
    assert!(matches!(view.state, State::Failed(End { reason, .. }) if *reason == error.to_string()));
    assert!(tunnels.release(id, &a).unwrap());
    expect_response(addr, &external, b"external");
}

#[test]
fn tunnels_reject_late_share_to_closed_tab_even_after_same_label_reopens() {
    let mut tunnels = Tunnels::default();
    let a = tunnels.open_tab("one", "a").unwrap();
    let b = tunnels.open_tab("one", "b").unwrap();
    let id = tunnels.begin(a.clone(), route(local())).unwrap();
    let probe = Probe::new();
    let closes = probe.closes.clone();
    assert!(tunnels.finish_start(id, Ok(probe)).unwrap());
    assert!(tunnels.share(id, &a, b.clone()));
    assert!(tunnels.close_tab(&a).is_empty());
    assert!(!tunnels.share(id, &b, a.clone()));
    assert_eq!(closes.get(), 0);

    let reopened = tunnels.open_tab("one", "a").unwrap();
    assert!(!tunnels.share(id, &b, a.clone()));
    assert!(tunnels.begin(a.clone(), route(local())).is_none());
    assert!(tunnels.close_tab(&a).is_empty());
    assert!(tunnels.share(id, &b, reopened.clone()));
    assert!(tunnels.close_tab(&reopened).is_empty());
    assert!(tunnels.close_tab(&b).is_empty());
    assert_eq!(closes.get(), 1);
}

#[test]
fn tunnels_retry_only_failed_owned_cleanup_until_confirmed() {
    for dismissal in ["release", "tab", "window", "late"] {
        let mut tunnels = Tunnels::default();
        let a = tunnels.open_tab("one", "a").unwrap();
        let b = tunnels.open_tab("two", "b").unwrap();
        let id = tunnels.begin(a.clone(), route(local())).unwrap();
        let other = tunnels.begin(b.clone(), route(local())).unwrap();
        let independent = Probe::new();
        independent.signal.set(Observation::Listening);
        let independent_closes = independent.closes.clone();
        assert!(tunnels.finish_start(other, Ok(independent)).unwrap());

        let mut probe = Probe::new();
        probe.close_failures = 2;
        let closes = probe.closes.clone();
        let error = if dismissal == "late" {
            assert!(tunnels.close_tab(&a).is_empty());
            tunnels.finish_start(id, Ok(probe)).unwrap_err()
        } else {
            assert!(tunnels.finish_start(id, Ok(probe)).unwrap());
            if dismissal == "release" {
                tunnels.release(id, &a).unwrap_err()
            } else {
                let errors = if dismissal == "tab" {
                    tunnels.close_tab(&a)
                } else {
                    tunnels.close_window(&a.window)
                };
                let [(failed, error)]: [_; 1] = errors.try_into().unwrap();
                assert_eq!(failed, id);
                error
            }
        };
        assert_eq!(error.to_string(), "cleanup failed");
        assert_eq!(closes.get(), 1);
        assert!(tunnels.observe(id, &a).is_none());
        assert!(!tunnels.share(id, &a, b.clone()));
        assert!(!tunnels.retry_cleanup(other).unwrap());
        assert_eq!(tunnels.retry_cleanup(id).unwrap_err().to_string(), "cleanup failed");
        assert_eq!(closes.get(), 2);
        assert!(tunnels.retry_cleanup(id).unwrap());
        assert!(!tunnels.retry_cleanup(id).unwrap());
        assert_eq!(closes.get(), 3);
        assert!(matches!(tunnels.observe(other, &b).unwrap().state, State::Ready));
        assert_eq!(independent_closes.get(), 0);
    }
}

#[test]
fn tunnels_close_window_retries_only_its_observed_cleanup() {
    let mut tunnels = Tunnels::default();
    let a = tunnels.open_tab("one", "a").unwrap();
    let b = tunnels.open_tab("two", "b").unwrap();
    let mut ended = |owner: &Owner, failures| {
        let id = tunnels.begin(owner.clone(), route(local())).unwrap();
        let mut probe = Probe::new();
        probe.signal.set(Observation::Ended("exited".into()));
        probe.close_failures = failures;
        let closes = probe.closes.clone();
        assert!(tunnels.finish_start(id, Ok(probe)).unwrap());
        assert!(matches!(tunnels.observe(id, owner).unwrap().state,
            State::Failed(End { cleanup_error: Some(_), .. })));
        assert_eq!(closes.get(), 1);
        (id, closes)
    };
    let (id, closes) = ended(&a, 2);
    let (other, other_closes) = ended(&b, 1);

    let [(failed, error)]: [_; 1] = tunnels.close_window(&a.window).try_into().unwrap();
    assert_eq!(failed, id);
    assert_eq!(error.to_string(), "cleanup failed");
    assert_eq!(closes.get(), 2);
    assert!(tunnels.observe(id, &a).is_none());
    assert!(tunnels.close_window(&a.window).is_empty());
    assert_eq!(closes.get(), 3);
    assert!(!tunnels.retry_cleanup(id).unwrap());
    assert_eq!(other_closes.get(), 1);
    assert!(matches!(tunnels.observe(other, &b).unwrap().state,
        State::Failed(End { cleanup_error: Some(_), .. })));
    assert!(tunnels.close_window(&b.window).is_empty());
    assert_eq!(other_closes.get(), 2);
}

#[cfg(target_os = "linux")]
pub(super) mod loopback {
    use crate::ssh::{ApprovedHost, Endpoint, TrustStore, TRUST_DIRECTORY};
    use serde_json::json;
    use std::{fs, path::Path};

    pub(in crate::tunnels) const SERVER: &str = "test-server";

    pub(in crate::tunnels) struct Fixture {
        pub(in crate::tunnels) directory: tempfile::TempDir,
        pub(in crate::tunnels) ssh: super::super::ssh::SavedSsh,
    }

    impl Fixture {
        pub(in crate::tunnels) fn new() -> Self {
            // 격리 서버의 일회 키와 루프백 리슨 포트만 받는다.
            let root = std::env::var_os("ZXT_R01_LOOPBACK_FIXTURE").expect("격리 SSH 시험 디렉터리 필요");
            let root = Path::new(&root);
            let port = fs::read_to_string(root.join("port")).unwrap().trim().parse::<u16>().unwrap();
            let key = fs::read_to_string(root.join("host.pub")).unwrap();
            let fingerprint = ssh_key::PublicKey::from_openssh(&key).unwrap().fingerprint(ssh_key::HashAlg::Sha256).to_string();
            let ssh = json!({"host":"127.0.0.1", "port":port, "hostKeyFingerprint":fingerprint,
                "account":"r01-fixture", "transport":{"kind":"direct"},
                "auth":{"kind":"keyFile", "privateKeyPath":root.join("client")}});
            let directory = tempfile::tempdir().unwrap();
            let store = TrustStore::create(&directory.path().join(TRUST_DIRECTORY)).unwrap();
            let endpoint = Endpoint::new("127.0.0.1", port).unwrap();
            store.register(&ApprovedHost::confirm(&endpoint, &endpoint, &fingerprint).unwrap(), &key).unwrap();
            fs::write(directory.path().join("connections.json"), json!({"version":1,
                "groups":[{"revision":1,"profile":{"id":"work","displayName":"작업"}}],
                "targets":[{"revision":1,"profile":{"id":SERVER,"groupId":"work","displayName":"시험 서버",
                    "defaultMethod":"ssh","ssh":ssh}}]}).to_string()).unwrap();
            Self { directory, ssh: (std::num::NonZeroU64::MIN, serde_json::from_value(ssh).unwrap()) }
        }
    }
}
