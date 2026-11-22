use super::*;

#[cfg(windows)]
#[test]
fn windows_listener_observation_requires_exact_address_and_owner() {
    for bind in ["127.0.0.1:0", "[::1]:0"] {
        let listener = TcpListener::bind(bind).unwrap();
        let address = listener.local_addr().unwrap();
        let pid = std::process::id();
        assert!(windows::owns_listener(address, pid).unwrap());
        assert!(!windows::owns_listener(address, 0).unwrap());
        let wildcard = if address.is_ipv4() { "0.0.0.0" } else { "::" };
        let wrong = (wildcard.parse::<std::net::IpAddr>().unwrap(), address.port()).into();
        assert!(!windows::owns_listener(wrong, pid).unwrap());
    }
}

#[cfg(target_os = "linux")]
mod loopback {
    use super::*;
    use crate::{ssh::{ApprovedHost, Endpoint, TrustStore, TRUST_DIRECTORY}, tunnels::{Destination, State}};
    use serde_json::json;
    use std::{fs, io::{Read, Write}, net::{SocketAddr, TcpStream}, thread};

    const SERVER: &str = "test-server";
    const TIMEOUT: Duration = Duration::from_secs(4);

    fn free() -> SocketAddr {
        TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
    }

    struct Fixture {
        directory: tempfile::TempDir,
        ssh: SavedSsh,
    }

    impl Fixture {
        fn new() -> Self {
            // 격리 서버의 일회 키와 루프백 리슨 포트만 받는다.
            let root = std::env::var_os("ZZABBA_R01_LOOPBACK_FIXTURE").expect("격리 SSH 시험 디렉터리 필요");
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

        fn start(&self, tunnels: &mut Tunnels<SshChild>, owner: &Owner, route: Route) -> Start {
            let policy = match route {
                Route::SshLocal { .. } => SshServerPolicy::FixedLocal,
                Route::SshRemote { .. } => SshServerPolicy::ExactRemoteLoopback,
                Route::SshDynamic { .. } => SshServerPolicy::DestinationRestrictedSocks,
                _ => unreachable!(),
            };
            tunnels.begin_ssh(owner.clone(), self.directory.path(), route.clone(), &route, &self.ssh, policy).unwrap()
        }
    }

    fn attach(tunnels: &mut Tunnels<SshChild>, start: Start) -> TunnelId {
        let (id, result) = start.spawn();
        assert!(tunnels.finish_start(id, result).unwrap());
        id
    }

    fn wait(tunnels: &mut Tunnels<SshChild>, id: TunnelId, owner: &Owner) -> StateKind {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match tunnels.observe(id, owner).unwrap().state {
                State::Starting if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                State::Starting => panic!("터널 상태 확인 시간 초과"),
                State::Ready => return StateKind::Ready,
                State::Failed(end) => return StateKind::Failed(end.reason.clone()),
                State::Disconnected(_) => return StateKind::Disconnected,
            }
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum StateKind { Ready, Failed(String), Disconnected }

    fn tcp(address: SocketAddr) -> TcpStream {
        let stream = TcpStream::connect_timeout(&address, TIMEOUT).unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(TIMEOUT)).unwrap();
        stream
    }

    fn answer(listener: &TcpListener, mut client: TcpStream, tag: &[u8; 4]) {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + TIMEOUT;
        let mut service = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("전달 서비스 연결: {error}"),
            }
        };
        service.write_all(tag).unwrap();
        let mut received = [0; 4];
        client.read_exact(&mut received).unwrap();
        assert_eq!(&received, tag);
    }

    fn local(listen: SocketAddr, port: u16) -> Route {
        Route::SshLocal { server: SERVER.into(), listen, destination: Destination { host: "127.0.0.1".into(), port } }
    }

    fn remote(listen: SocketAddr, port: u16) -> Route {
        Route::SshRemote { server: SERVER.into(), listen, destination: Destination { host: "127.0.0.1".into(), port } }
    }

    #[test]
    #[ignore = "네트워크 없는 Ubuntu 컨테이너의 일회 키 SSH 서버 필요"]
    fn isolated_openssh_forwarding_and_tab_lifetime() {
        let fixture = Fixture::new();
        let mut tunnels = Tunnels::default();
        let a = tunnels.open_tab("window", "a").unwrap();
        let b = tunnels.open_tab("shared-window", "b").unwrap();
        let c = tunnels.open_tab("other-window", "c").unwrap();
        let d = tunnels.open_tab("other-window", "d").unwrap();
        let service = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = service.local_addr().unwrap().port();

        let listen_local = free();
        let start = fixture.start(&mut tunnels, &a, local(listen_local, port));
        let id = attach(&mut tunnels, start);
        assert_eq!(wait(&mut tunnels, id, &a), StateKind::Ready);
        let local_child = tunnels.entries[&id].transport.as_ref().unwrap().child.id();
        assert!(tunnels.share(id, &a, b.clone()));

        let listen_remote = free();
        let start = fixture.start(&mut tunnels, &c, remote(listen_remote, port));
        let other = attach(&mut tunnels, start);
        assert_eq!(wait(&mut tunnels, other, &c), StateKind::Ready);
        answer(&service, tcp(listen_remote), b"R01R");

        let listen_dynamic = free();
        let start = fixture.start(&mut tunnels, &d, Route::SshDynamic { server: SERVER.into(), listen: listen_dynamic });
        let socks = attach(&mut tunnels, start);
        assert_eq!(wait(&mut tunnels, socks, &d), StateKind::Ready);
        let mut stream = tcp(listen_dynamic);
        stream.write_all(&[5, 1, 0]).unwrap();
        let mut method = [0; 2];
        stream.read_exact(&mut method).unwrap();
        assert_eq!(method, [5, 0]);
        let [hi, lo] = port.to_be_bytes();
        stream.write_all(&[5, 1, 0, 1, 127, 0, 0, 1, hi, lo]).unwrap();
        let mut reply = [0; 10];
        stream.read_exact(&mut reply).unwrap();
        assert_eq!(&reply[..4], &[5, 0, 0, 1]);
        answer(&service, stream, b"R01D");

        let external = TcpListener::bind("127.0.0.1:0").unwrap();
        let external_address = external.local_addr().unwrap();
        let child = tunnels.entries.get_mut(&id).unwrap().transport.as_mut().unwrap();
        assert!(child.open_stream(external_address).is_err());
        // 소유권 검사용 소켓은 보류 큐에서 꺼내고 새 연결로 외부 서비스 응답을 확인한다.
        drop(external.accept().unwrap());
        assert!(tunnels.close_window("window").is_empty());
        let stream = tunnels.entries.get_mut(&id).unwrap().transport.as_mut().unwrap().open_stream(listen_local).unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        answer(&service, stream, b"R01L");
        assert!(tunnels.close_window("shared-window").is_empty());
        assert!(!Path::new(&format!("/proc/{local_child}")).exists(), "SSH 자식이 회수되지 않았습니다");
        assert!(TcpStream::connect_timeout(&listen_local, TIMEOUT).is_err());
        answer(&service, tcp(listen_remote), b"R01C");
        answer(&external, tcp(external_address), b"LIVE");

        tunnels.entries.get_mut(&socks).unwrap().transport.as_mut().unwrap().child.kill().unwrap();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if wait(&mut tunnels, socks, &d) == StateKind::Disconnected { break; }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(tunnels.observe(socks, &c).is_none());
        answer(&service, tcp(listen_remote), b"R01X");
        assert!(tunnels.close_tab(&c).is_empty());
        let deadline = Instant::now() + TIMEOUT;
        while TcpStream::connect_timeout(&listen_remote, TIMEOUT).is_ok() {
            assert!(Instant::now() < deadline, "원격 수신이 닫히지 않았습니다");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "네트워크 없는 Ubuntu 컨테이너의 일회 키 SSH 서버 필요"]
    fn isolated_openssh_bind_races_and_late_close() {
        let fixture = Fixture::new();
        let mut tunnels = Tunnels::default();
        let owner = tunnels.open_tab("window", "tab").unwrap();
        for remote_side in [false, true] {
            let listener = free();
            let policy = SshServerPolicy::FixedLocal;
            let proposed = local(listener, 9);
            let different = local(listener, 10);
            assert!(tunnels.begin_ssh(owner.clone(), fixture.directory.path(), proposed.clone(), &different, &fixture.ssh, policy).is_err());
            let mut changed = serde_json::to_value(&fixture.ssh.1).unwrap();
            changed["port"] = json!(1);
            let changed = (fixture.ssh.0, serde_json::from_value(changed).unwrap());
            assert!(tunnels.begin_ssh(owner.clone(), fixture.directory.path(), proposed.clone(), &proposed, &changed, policy).is_err());
            let stale = (std::num::NonZeroU64::new(2).unwrap(), serde_json::from_value(serde_json::to_value(&fixture.ssh.1).unwrap()).unwrap());
            assert!(tunnels.begin_ssh(owner.clone(), fixture.directory.path(), proposed.clone(), &proposed, &stale, policy).is_err());
            let route = if remote_side { remote(listener, 9) } else { local(listener, 9) };
            let start = fixture.start(&mut tunnels, &owner, route);
            let (id, result) = start.spawn();
            // 새 자식은 제어 요청 전에는 포트를 열지 않는다.
            let existing = TcpListener::bind(listener).unwrap();
            assert!(tunnels.finish_start(id, result).unwrap());
            match wait(&mut tunnels, id, &owner) {
                StateKind::Failed(message) => assert!(message.contains(&listener.to_string()), "{message}"),
                state => panic!("충돌 결과: {state:?}"),
            }
            answer(&existing, tcp(listener), b"LIVE");
        }
        let listen = free();
        let old = fixture.start(&mut tunnels, &owner, local(listen, 9));
        assert!(tunnels.close_tab(&owner).is_empty());
        let reopened = tunnels.open_tab("window", "tab").unwrap();
        let start = fixture.start(&mut tunnels, &reopened, local(listen, 9));
        let id = attach(&mut tunnels, start);
        assert_eq!(wait(&mut tunnels, id, &reopened), StateKind::Ready);
        let (old_id, result) = old.spawn();
        assert!(!tunnels.finish_start(old_id, result).unwrap());
        assert!(tunnels.close_tab(&owner).is_empty());
        assert_eq!(wait(&mut tunnels, id, &reopened), StateKind::Ready);
    }
}
