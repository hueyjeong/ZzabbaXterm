use super::*;
use serde_json::json;

#[test]
fn tunnels_ipc_rejects_raw_destinations_and_unsupported_transports() {
    let forged = json!({"tab":"x", "request":{"direction":"dynamic","listen":1080,"command":"ssh"}});
    assert!(serde_json::from_value::<Start>(forged).is_err());
    let remote_vnc = PortRequest::Local { listen: 5901, host: "other.example".into(), port: 5900, protocol: Some(Protocol::Vnc) };
    assert!(remote_vnc.ssh("saved".into()).unwrap_err().contains("루프백"));
    let ssm = PortRequest::Ssm { listen: 3391, port: 3389 };
    assert!(ssm.ssh("saved".into()).unwrap_err().contains("바이너리 보존"));
    #[cfg(windows)]
    assert!(PortRequest::Remote { listen: 3391, host: "127.0.0.1".into(), port: 3389 }
        .ssh("saved".into()).unwrap_err().contains("Windows OpenSSH"));
}

#[test]
fn unsupported_ssm_stops_before_approval_or_process_creation() {
    let tabs = TunnelTabs::default();
    let dir = tempfile::tempdir().unwrap();
    let result = tabs.start(0, dir.path(), "unused", PortRequest::Ssm { listen: 3391, port: 3389 },
        |_| panic!("SSM 승인 창을 열었습니다"));
    assert!(result.unwrap_err().contains("바이너리 보존"));
    assert!(tabs.document(0).unwrap().entries.is_empty());
}

#[test]
fn cancelled_or_stale_native_approval_does_not_start_a_tunnel() {
    let tabs = TunnelTabs::default();
    let dir = tempfile::tempdir().unwrap();
    let target = "ssh-server";
    let mut profile = crate::connections::tests::ssh_draft(dir.path());
    profile["target"]["ssh"]["auth"] = json!({"kind":"keyFile", "privateKeyPath": dir.path().join("client")});
    let draft = serde_json::from_value::<crate::connections::profile::Draft>(profile).unwrap().validate().unwrap();
    crate::connections::save_test_profile(dir.path(), draft);
    let handle = tabs.open(0, dir.path(), target.into(), "시험 터널".into()).unwrap();
    #[cfg(windows)]
    {
        let remote = tabs.start(0, dir.path(), &handle, PortRequest::Remote {
            listen: 3391,
            host: "127.0.0.1".into(),
            port: 3389,
        }, |_| panic!("Windows 원격 전달 승인 창을 열었습니다"));
        assert!(remote.unwrap_err().contains("Windows OpenSSH"));
        assert!(tabs.snapshot(0, &handle).unwrap().is_empty());
    }

    let request = || PortRequest::Dynamic { listen: 1080 };

    let cancelled = tabs.start(0, dir.path(), &handle, request(), |description| {
        assert!(description.contains("deploy@dev.example.test:2222"));
        assert!(description.contains("SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
        assert!(description.contains("-D 127.0.0.1:1080 → SOCKS"));
        assert!(description.contains("PermitOpen"));
        Ok(false)
    });
    assert_eq!(cancelled.unwrap(), None);
    assert!(tabs.snapshot(0, &handle).unwrap().is_empty());

    let stale = tabs.start(0, dir.path(), &handle, request(), |_| {
        tabs.navigate(false);
        Ok(true)
    });
    assert_eq!(stale.unwrap_err(), STALE);
    assert!(tabs.document(1).unwrap().entries.is_empty());
}

#[test]
fn desktop_rejects_protocol_mismatch_and_changed_saved_ssh_before_connecting() {
    let tabs = TunnelTabs::default();
    let dir = tempfile::tempdir().unwrap();
    let target = "ssh-server";
    let mut profile = crate::connections::tests::ssh_draft(dir.path());
    let key = |name| json!({"kind":"keyFile", "privateKeyPath": dir.path().join(name)});
    let save = |profile| {
        let draft = serde_json::from_value::<crate::connections::profile::Draft>(profile).unwrap().validate().unwrap();
        crate::connections::save_test_profile(dir.path(), draft);
    };
    profile["target"]["ssh"]["auth"] = key("original");
    save(profile.clone());
    let epoch = tabs.epoch();
    let handle = tabs.open(epoch, dir.path(), target.into(), "시험 터널".into()).unwrap();
    let destination = Destination { host: "127.0.0.1".into(), port: 3389 };
    let route = Route::SshLocal { server: target.into(), listen: ([127, 0, 0, 1], 3390).into(), destination: destination.clone() };
    let id = {
        let mut doc = tabs.document(epoch).unwrap();
        let owner = doc.tabs[&handle].owner.clone();
        let id = doc.tunnels.begin(owner, route).unwrap();
        doc.tabs.get_mut(&handle).unwrap().ids.push(id);
        doc.entries.insert(id, Entry { endpoint: String::new(), protocol: Some(Protocol::Rdp) });
        id.0.to_string()
    };
    let outsider = tabs.open(epoch, dir.path(), target.into(), "별도 탭".into()).unwrap();
    assert_eq!(tabs.open_desktop(epoch, dir.path(), (&outsider, &id), target, &destination, Protocol::Rdp)
        .err().unwrap(), "이 탭이 소유한 터널이 아닙니다");
    let issue = |protocol| tabs.open_desktop(epoch, dir.path(), (&handle, &id), target, &destination, protocol);
    assert_eq!(issue(Protocol::Vnc).err().unwrap(), "승인된 터널 서비스가 일치하지 않습니다");
    assert_eq!(issue(Protocol::Rdp).err().unwrap(), "터널 수신이 준비되지 않았습니다");

    profile["targetRevision"] = json!(1);
    profile["target"]["ssh"]["auth"] = key("replacement");
    save(profile.clone());
    assert_eq!(issue(Protocol::Rdp).err().unwrap(), "승인된 SSH 접속 설정이 변경되었습니다");
    profile["targetRevision"] = json!(2);
    profile["target"]["ssh"]["auth"] = key("original");
    save(profile);
    assert_eq!(issue(Protocol::Rdp).err().unwrap(), "승인된 SSH 접속 설정이 변경되었습니다");

    tabs.navigate(false);
    assert_eq!(issue(Protocol::Rdp).err().unwrap(), STALE);
}

#[test]
fn desktop_requires_exact_same_host_loopback_destination() {
    let listen = ([127, 0, 0, 1], 3390).into();
    for (host, allowed) in [("127.0.0.1", true), ("::1", true), ("127.0.0.2", false), ("localhost", false)] {
        let destination = Destination { host: host.into(), port: 3389 };
        let route = Route::SshLocal { server: "ssh-server".into(), listen, destination: destination.clone() };
        assert_eq!(desktop_listener(&route, "ssh-server", &destination).is_ok(), allowed);
        assert!(desktop_listener(&route, "other-server", &destination).is_err());
        assert!(desktop_listener(&route, "ssh-server", &Destination { port: 5900, ..destination }).is_err());
    }
}

#[test]
fn desktop_route_survives_source_close_and_cleans_up_on_last_owner() {
    use super::super::{Observation, Transport};
    use std::cell::Cell;

    struct Probe<'a> { signal: Observation, closes: &'a Cell<usize> }
    impl Transport for Probe<'_> {
        fn observe(&mut self) -> Observation { std::mem::replace(&mut self.signal, Observation::Pending) }
        fn close(&mut self) -> std::io::Result<()> {
            self.closes.set(self.closes.get() + 1);
            Ok(())
        }
    }

    for end in ["release", "child exit", "navigation"] {
        let closes = Cell::new(0);
        let mut tunnels = Tunnels::default();
        let source = tunnels.open_tab("0", "source").unwrap();
        let id = tunnels.begin(source.clone(), Route::SshLocal {
            server: "ssh-server".into(), listen: ([127, 0, 0, 1], 3390).into(),
            destination: Destination { host: "127.0.0.1".into(), port: 3389 },
        }).unwrap();
        tunnels.finish_start(id, Ok(Probe { signal: Observation::Listening, closes: &closes })).unwrap();
        assert!(matches!(tunnels.observe(id, &source).unwrap().state, State::Ready));
        let owner = tunnels.retain_desktop(id, &source).unwrap();
        let other_desktop = tunnels.retain_desktop(id, &source).unwrap();
        let loss = tunnels.entries.get_mut(&id).unwrap().desktop_loss.get_or_insert_with(CancellationToken::new).child_token();
        assert!(tunnels.close_tab(&source).is_empty());
        assert_eq!(closes.get(), 0);
        assert!(tunnels.entries[&id].transport.is_some());
        assert!(!loss.is_cancelled());
        assert!(tunnels.release(id, &other_desktop).unwrap());
        assert_eq!(closes.get(), 0);
        assert!(!loss.is_cancelled());
        match end {
            "navigation" => {
                tunnels.revoke_desktops();
                assert!(loss.is_cancelled());
                assert!(tunnels.close_window("0").is_empty());
            }
            "child exit" => {
                tunnels.entries.get_mut(&id).unwrap().transport.as_mut().unwrap().signal = Observation::Ended("exited".into());
                assert!(matches!(tunnels.observe_route(id).unwrap().state, State::Disconnected(_)));
                assert!(loss.is_cancelled());
                assert!(tunnels.release(id, &owner).unwrap());
            }
            _ => assert!(tunnels.release(id, &owner).unwrap()),
        }
        assert!(loss.is_cancelled());
        assert!(!tunnels.entries.contains_key(&id));
        assert_eq!(closes.get(), 1);
    }
}
