use std::{cell::Cell, io};

use super::super::{Observation, Transport};
use super::*;
use crate::{connections::with_saved_ssh, tunnels::ssh::SavedSsh};
use serde_json::json;

struct Probe<'a> { signal: Observation, closes: &'a Cell<usize>, close_failures: usize }

impl Transport for Probe<'_> {
    fn observe(&mut self) -> Observation { std::mem::replace(&mut self.signal, Observation::Pending) }
    fn close(&mut self) -> io::Result<()> {
        let attempts = self.closes.get() + 1;
        self.closes.set(attempts);
        if attempts <= self.close_failures { return Err(io::Error::other(format!("close failed {attempts}"))); }
        Ok(())
    }
}

#[test]
fn tunnels_ipc_rejects_raw_destinations_and_unsupported_transports() {
    let forged = json!({"tab":"x", "request":{"direction":"dynamic","listen":1080,"command":"ssh"}});
    assert!(serde_json::from_value::<Start>(forged).is_err());
    let remote_vnc = PortRequest::Local { listen: 5901, host: "other.example".into(), port: 5900, protocol: Some(Protocol::Vnc) };
    assert!(remote_vnc.ssh("saved".into()).unwrap_err().contains("루프백"));
    let ssm = PortRequest::Ssm { listen: 3391, port: 3389, host: None, protocol: None };
    assert!(ssm.ssh("saved".into()).unwrap_err().contains("SSM 경로"));
    #[cfg(windows)]
    assert!(PortRequest::Remote { listen: 3391, host: "127.0.0.1".into(), port: 3389 }
        .ssh("saved".into()).unwrap_err().contains("Windows OpenSSH"));
}

#[test]
fn unsupported_ssm_stops_before_approval_or_process_creation() {
    let tabs = TunnelTabs::default();
    let dir = tempfile::tempdir().unwrap();
    let request = || PortRequest::Ssm { listen: 3391, port: 3389, host: None, protocol: None };
    let result = tabs.start(0, dir.path(), "unused", request(), |_| panic!("SSM 승인 창을 열었습니다"));
    assert_eq!(result.unwrap_err(), STALE);
    // SSH 대상 탭에서는 SSM 전달을 승인하거나 실행하지 않는다.
    let draft = serde_json::from_value::<crate::connections::profile::Draft>(crate::connections::tests::ssh_draft(dir.path()))
        .unwrap().validate().unwrap();
    crate::connections::save_test_profile(dir.path(), draft);
    let handle = tabs.open(0, dir.path(), "ssh-server".into(), "SSH 탭".into()).unwrap();
    let result = tabs.start(0, dir.path(), &handle, request(), |_| panic!("SSM 승인 창을 열었습니다"));
    assert_eq!(result.unwrap_err(), "저장된 SSM 대상이 필요합니다");
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
        assert_eq!(tabs.confirm_desktop_cleanup(0, "close", &CancellationToken::new()).unwrap_err(), APPROVAL_PENDING);
        let doc = tabs.document(0).unwrap();
        assert!(doc.approving);
        assert!(doc.close_request.is_none());
        Ok(false)
    });
    assert_eq!(cancelled.unwrap(), None);
    assert!(tabs.snapshot(0, &handle).unwrap().is_empty());
    assert!(!tabs.document(0).unwrap().approving);

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
    let original = with_saved_ssh(dir.path(), target, |saved| saved.unwrap()).unwrap();
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
    assert_eq!(tabs.open_desktop(epoch, dir.path(), (&outsider, &id), target, &destination, Protocol::Rdp, (original.0, &original.1))
        .err().unwrap(), "이 탭이 소유한 터널이 아닙니다");
    let issue = |handle: &str, protocol, approved: &SavedSsh| tabs.open_desktop(
        epoch, dir.path(), (handle, &id), target, &destination, protocol, (approved.0, &approved.1),
    );
    assert_eq!(issue(&handle, Protocol::Vnc, &original).err().unwrap(), "승인된 터널 서비스가 일치하지 않습니다");
    assert_eq!(issue(&handle, Protocol::Rdp, &original).err().unwrap(), "터널 수신이 준비되지 않았습니다");

    profile["targetRevision"] = json!(1);
    profile["target"]["ssh"]["auth"] = key("replacement");
    save(profile.clone());
    let replacement = with_saved_ssh(dir.path(), target, |saved| saved.unwrap()).unwrap();
    assert_eq!(issue(&handle, Protocol::Rdp, &original).err().unwrap(), "승인된 SSH 접속 설정이 변경되었습니다");
    profile["targetRevision"] = json!(2);
    profile["target"]["ssh"]["auth"] = key("original");
    save(profile);
    assert_eq!(issue(&handle, Protocol::Rdp, &original).err().unwrap(), "승인된 SSH 접속 설정이 변경되었습니다");

    let restored = with_saved_ssh(dir.path(), target, |saved| saved.unwrap()).unwrap();
    assert_eq!(restored.1, original.1);
    assert_ne!(restored.0, original.0);
    let reopened = tabs.open(epoch, dir.path(), target.into(), "원복 후 터널".into()).unwrap();
    let reopened_id = {
        let mut doc = tabs.document(epoch).unwrap();
        let owner = doc.tabs[&reopened].owner.clone();
        let route = doc.tunnels.entries[&TunnelId(id.parse().unwrap())].route.clone();
        let id = doc.tunnels.begin(owner, route).unwrap();
        doc.tabs.get_mut(&reopened).unwrap().ids.push(id);
        doc.entries.insert(id, Entry { endpoint: String::new(), protocol: Some(Protocol::Rdp) });
        id.0.to_string()
    };
    let issue_reopened = |approved: &SavedSsh| tabs.open_desktop(epoch, dir.path(), (&reopened, &reopened_id),
        target, &destination, Protocol::Rdp, (approved.0, &approved.1));
    let different_ssh = (restored.0, replacement.1);
    assert_eq!(issue_reopened(&original).err().unwrap(), "승인된 SSH 접속 설정이 변경되었습니다");
    assert_eq!(issue_reopened(&different_ssh).err().unwrap(), "승인된 SSH 접속 설정이 변경되었습니다");
    assert_eq!(issue_reopened(&restored).err().unwrap(), "터널 수신이 준비되지 않았습니다");

    tabs.navigate(false);
    assert_eq!(issue(&handle, Protocol::Rdp, &original).err().unwrap(), STALE);
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
    for end in ["release", "child exit", "navigation"] {
        let closes = Cell::new(0);
        let mut tunnels = Tunnels::default();
        let source = tunnels.open_tab("0", "source").unwrap();
        let id = tunnels.begin(source.clone(), Route::SshLocal {
            server: "ssh-server".into(), listen: ([127, 0, 0, 1], 3390).into(),
            destination: Destination { host: "127.0.0.1".into(), port: 3389 },
        }).unwrap();
        tunnels.finish_start(id, Ok(Probe { signal: Observation::Listening, closes: &closes, close_failures: 0 })).unwrap();
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

#[test]
fn last_desktop_release_reports_active_or_observed_child_failure_and_retries_only_that_child() {
    for observed in [false, true] {
        let closes = Cell::new(0);
        let unrelated_closes = Cell::new(0);
        let mut tunnels = Tunnels::default();
        let source = tunnels.open_tab("0", "source").unwrap();
        let route = Route::SshLocal { server: "saved".into(), listen: ([127, 0, 0, 1], 3390).into(),
            destination: Destination { host: "127.0.0.1".into(), port: 3389 } };
        let id = tunnels.begin(source.clone(), route.clone()).unwrap();
        tunnels.finish_start(id, Ok(Probe { signal: Observation::Listening, closes: &closes, close_failures: 2 })).unwrap();
        assert!(matches!(tunnels.observe(id, &source).unwrap().state, State::Ready));
        let owner = tunnels.retain_desktop(id, &source).unwrap();
        let other_desktop = tunnels.retain_desktop(id, &source).unwrap();
        let loss = tunnels.entries.get_mut(&id).unwrap().desktop_loss.get_or_insert_with(CancellationToken::new).child_token();

        let unrelated = tunnels.open_tab("0", "unrelated").unwrap();
        let other_id = tunnels.begin(unrelated, route).unwrap();
        tunnels.finish_start(other_id, Ok(Probe { signal: Observation::Listening, closes: &unrelated_closes, close_failures: 0 })).unwrap();
        assert!(tunnels.close_tab(&source).is_empty());
        if observed {
            tunnels.entries.get_mut(&id).unwrap().transport.as_mut().unwrap().signal = Observation::Ended("exit unconfirmed".into());
            assert!(matches!(tunnels.observe_route(id).unwrap().state, State::Disconnected(_)));
        }
        assert!(tunnels.release(id, &other_desktop).unwrap());
        assert_eq!(closes.get(), usize::from(observed));
        assert_eq!(tunnels.release(id, &owner).unwrap_err().to_string(), "close failed 1");
        assert!(loss.is_cancelled());
        assert!(!tunnels.entries.contains_key(&id));
        assert_eq!(tunnels.desktop_cleanup_error(id, "0"), Some("close failed 1"));
        assert!(tunnels.confirm_desktop_cleanup("1", |_| panic!("다른 문서 전달 철회")).is_ok());
        tunnels.retry_desktop_cleanup(id, "1").unwrap();
        tunnels.retry_desktop_cleanup(other_id, "0").unwrap();
        assert_eq!(closes.get(), 1);
        assert_eq!(tunnels.retry_desktop_cleanup(id, "0").unwrap_err().to_string(), "close failed 2");
        assert_eq!(tunnels.desktop_cleanup_error(id, "0"), Some("close failed 2"));
        tunnels.retry_desktop_cleanup(id, "0").unwrap();
        tunnels.retry_desktop_cleanup(id, "0").unwrap();
        assert_eq!(tunnels.desktop_cleanup_error(id, "0"), None);
        assert_eq!(closes.get(), 3);
        assert_eq!(unrelated_closes.get(), 0);
    }
}

#[test]
fn same_document_hard_close_reports_new_failure_and_retries_only_desktop_owned_children() {
    let closes = Cell::new(0);
    let shared_closes = Cell::new(0);
    let other_document_closes = Cell::new(0);
    let detached_closes = Cell::new(0);
    let mut tunnels = Tunnels::default();
    fn issue<'a>(tunnels: &mut Tunnels<Probe<'a>>, window: &str, name: &str, closes: &'a Cell<usize>,
                 close_failures: usize, source_open: bool) -> (TunnelId, Owner, CancellationToken) {
        let source = tunnels.open_tab(window, name).unwrap();
        let id = tunnels.begin(source.clone(), Route::SshLocal {
            server: "saved".into(), listen: ([127, 0, 0, 1], 3390).into(),
            destination: Destination { host: "127.0.0.1".into(), port: 3389 },
        }).unwrap();
        tunnels.finish_start(id, Ok(Probe { signal: Observation::Listening, closes, close_failures })).unwrap();
        assert!(matches!(tunnels.observe(id, &source).unwrap().state, State::Ready));
        let owner = tunnels.retain_desktop(id, &source).unwrap();
        let loss = tunnels.entries.get_mut(&id).unwrap().desktop_loss.get_or_insert_with(CancellationToken::new).child_token();
        if !source_open { assert!(tunnels.close_tab(&source).is_empty()); }
        (id, owner, loss)
    }
    let (id, owner, loss) = issue(&mut tunnels, "0", "source", &closes, 1, false);
    let (_, _, shared_loss) = issue(&mut tunnels, "0", "shared", &shared_closes, 0, true);
    let (_, _, other_loss) = issue(&mut tunnels, "1", "other", &other_document_closes, 0, false);

    let detached_source = tunnels.open_tab("0", "detached").unwrap();
    let detached_id = tunnels.begin(detached_source.clone(), Route::SshLocal {
        server: "saved".into(), listen: ([127, 0, 0, 1], 3391).into(),
        destination: Destination { host: "127.0.0.1".into(), port: 3389 },
    }).unwrap();
    tunnels.finish_start(detached_id, Ok(Probe { signal: Observation::Listening, closes: &detached_closes, close_failures: 1 })).unwrap();
    assert_eq!(tunnels.close_tab(&detached_source).len(), 1);

    assert_eq!(tunnels.confirm_desktop_cleanup("0", |removed| assert_eq!(removed, id)).unwrap_err().to_string(), "close failed 1");
    assert_eq!(closes.get(), 1);
    assert!(loss.is_cancelled());
    assert!(!tunnels.release(id, &owner).unwrap());
    assert_eq!(tunnels.desktop_cleanup_error(id, "0"), Some("close failed 1"));
    assert!(tunnels.confirm_desktop_cleanup("0", |_| panic!("이미 철회한 전달")).is_ok());
    assert_eq!(closes.get(), 2);
    assert_eq!(tunnels.desktop_cleanup_error(id, "0"), None);
    assert_eq!(shared_closes.get(), 0);
    assert_eq!(other_document_closes.get(), 0);
    assert!(!shared_loss.is_cancelled());
    assert!(!other_loss.is_cancelled());
    assert_eq!(detached_closes.get(), 1);
}

pub(crate) fn rdp_test_lease() -> (TunnelTabs, DesktopLease) {
    test_lease(Protocol::Rdp)
}

fn test_lease(protocol: Protocol) -> (TunnelTabs, DesktopLease) {
    let dir = tempfile::tempdir().unwrap();
    test_lease_at(dir.path(), protocol)
}

fn test_lease_at(directory: &Path, protocol: Protocol) -> (TunnelTabs, DesktopLease) {
    let tabs = TunnelTabs::default();
    let target = "ssh-server";
    let profile = crate::connections::tests::ssh_draft(directory);
    let draft = serde_json::from_value::<crate::connections::profile::Draft>(profile).unwrap().validate().unwrap();
    crate::connections::save_test_profile(directory, draft);
    let handle = tabs.open(0, directory, target.into(), "시험 터널".into()).unwrap();
    let destination = Destination { host: "127.0.0.1".into(), port: 5900 };
    let lease = {
        let mut doc = tabs.document(0).unwrap();
        let source = doc.tabs[&handle].owner.clone();
        let route = doc.tabs[&handle].saved.clone();
        let id = doc.tunnels.begin(source.clone(), Route::SshLocal {
            server: target.into(), listen: ([127, 0, 0, 1], 5901).into(), destination: destination.clone(),
        }).unwrap();
        let tunnel = doc.tunnels.entries.get_mut(&id).unwrap();
        tunnel.state = State::Ready;
        let loss = tunnel.desktop_loss.get_or_insert_with(CancellationToken::new).child_token();
        let owner = doc.tunnels.retain_desktop(id, &source).unwrap();
        DesktopLease { document: Arc::downgrade(&tabs.0), epoch: 0, id, owner: Some(owner), target: target.into(), route,
            destination, protocol, stream: None, loss }
    };
    (tabs, lease)
}

#[test]
fn desktop_preflight_rejects_closed_source_and_failed_cleanup_records() {
    let (tabs, lease) = test_lease(Protocol::Rdp);
    let id = lease.id;
    let source = tabs.document(0).unwrap().tabs.keys().next().unwrap().clone();
    let selector = id.0.to_string();
    let preflight = || tabs.preflight_desktop(0, (&source, &selector), lease.target(), lease.destination(), Protocol::Rdp);
    let owner = {
        let mut doc = tabs.document(0).unwrap();
        doc.entries.insert(id, Entry { endpoint: String::new(), protocol: Some(Protocol::Rdp) });
        let tab = doc.tabs.get_mut(&source).unwrap();
        tab.ids.push(id);
        tab.owner.clone()
    };
    assert_eq!(lease.while_live(|| 7), Some(7));
    {
        let mut doc = tabs.document(0).unwrap();
        assert!(doc.tunnels.entries.get_mut(&id).unwrap().owners.remove(&owner));
    }
    assert_eq!(preflight().unwrap_err(), "이 탭이 소유한 터널이 아닙니다");
    {
        let mut doc = tabs.document(0).unwrap();
        doc.tunnels.entries.get_mut(&id).unwrap().owners.insert(owner.clone());
        inject_desktop_cleanup_failure(&mut doc, id);
    }
    assert_eq!(preflight().unwrap_err(), "터널 수신이 준비되지 않았습니다");
    {
        let mut doc = tabs.document(0).unwrap();
        assert!(doc.tunnels.cleanup.contains_key(&id));
        assert!(doc.tunnels.close_tab(&owner).is_empty());
        assert!(doc.tabs.contains_key(&source));
        assert!(doc.entries.contains_key(&id));
    }
    assert_eq!(preflight().unwrap_err(), STALE);
    tabs.close(0, &source).unwrap();
    assert_eq!(preflight().unwrap_err(), STALE);
}

#[test]
fn desktop_live_gate_rejects_removed_owner_and_retired_document() {
    let (tabs, lease) = test_lease(Protocol::Vnc);
    assert_eq!(lease.while_live(|| 7), Some(7));
    {
        let mut doc = tabs.document(0).unwrap();
        let tunnel = doc.tunnels.entries.get_mut(&lease.id).unwrap();
        assert!(tunnel.owners.remove(lease.owner.as_ref().unwrap()));
    }
    assert!(!lease.loss.is_cancelled());
    assert_eq!(lease.while_live(|| panic!("소유권 해제 후 쓰기")), None::<()>);
    tabs.navigate(false);
    assert_eq!(lease.while_live(|| panic!("문서 이동 후 쓰기")), None::<()>);
}

#[test]
fn cleanup_ticket_and_hard_close_reject_retired_document() {
    let (tabs, lease) = test_lease(Protocol::Rdp);
    let source = tabs.document(0).unwrap().tabs.keys().next().unwrap().clone();
    tabs.close(0, &source).unwrap();
    inject_desktop_cleanup_failure(&mut tabs.document(0).unwrap(), lease.id);
    let mut error = lease.release().unwrap_err();
    let revoked = CancellationToken::new();
    tabs.confirm_desktop_cleanup(0, "old", &revoked).unwrap();
    revoked.cancel();
    assert_eq!(tabs.close(0, &source).unwrap_err(), CLOSING);
    tabs.navigate(false);
    assert_eq!(error.retry().unwrap_err(), STALE);
    let current = CancellationToken::new();
    assert_eq!(tabs.confirm_desktop_cleanup(0, "old", &current).unwrap_err(), EXPIRED);
    tabs.confirm_desktop_cleanup(1, "new", &current).unwrap();
    tabs.release_desktop_close_gate(0, "new");
    tabs.release_desktop_close_gate(1, "old");
    assert_eq!(tabs.close(1, &source).unwrap_err(), CLOSING);
    tabs.release_desktop_close_gate(1, "new");
    tabs.close(1, &source).unwrap();
}

fn inject_desktop_cleanup_failure(doc: &mut Document, id: TunnelId) {
    assert!(doc.tunnels.entries[&id].transport.is_none());
    let window = doc.tunnels.entries[&id].owners.iter().next().unwrap().window.clone();
    doc.tunnels.cleanup.insert(id, super::super::Cleanup {
        transports: Vec::new(),
        observed_windows: [window].into(),
        desktop: None,
    });
    doc.tunnels.entries.get_mut(&id).unwrap().state = State::Disconnected(super::super::End {
        reason: "시험 전달 종료".into(), cleanup_error: Some(io::Error::other("close failed")),
    });
}

#[test]
fn explicit_release_returns_a_ticket_and_drop_keeps_pending_cleanup_visible() {
    for release in [true, false] {
        let (tabs, lease) = test_lease(Protocol::Rdp);
        let id = lease.id;
        let loss = lease.loss();
        let source = tabs.document(0).unwrap().tabs.keys().next().unwrap().clone();
        tabs.close(0, &source).unwrap();
        inject_desktop_cleanup_failure(&mut tabs.document(0).unwrap(), id);

        if release {
            let mut error = lease.release().unwrap_err();
            assert_eq!(error.to_string(), "SSH 터널 종료 확인 불가: close failed");
            assert_eq!(tabs.document(0).unwrap().tunnels.desktop_cleanup_error(id, "0"), Some("close failed"));
            error.retry().unwrap();
            error.retry().unwrap();
        } else {
            drop(lease);
            assert_eq!(tabs.document(0).unwrap().tunnels.desktop_cleanup_error(id, "0"), Some("close failed"));
            let revoked = CancellationToken::new();
            tabs.confirm_desktop_cleanup(0, "close", &revoked).unwrap();
        }
        assert!(loss.is_cancelled());
        assert_eq!(tabs.document(0).unwrap().tunnels.desktop_cleanup_error(id, "0"), None);
    }
}

#[test]
fn cancelled_or_pending_close_preserves_the_desktop_only_lease() {
    let (tabs, lease) = test_lease(Protocol::Rdp);
    let loss = lease.loss();
    let revoked = CancellationToken::new();
    let source = tabs.document(0).unwrap().tabs.keys().next().unwrap().clone();
    tabs.close(0, &source).unwrap();
    revoked.cancel();
    assert_eq!(tabs.confirm_desktop_cleanup(0, "expired", &revoked).unwrap_err(), EXPIRED);
    assert_eq!(lease.while_live(|| 7), Some(7));
    assert!(tabs.document(0).unwrap().close_request.is_none());
    tabs.document(0).unwrap().approving = true;
    assert_eq!(tabs.confirm_desktop_cleanup(0, "waiting", &CancellationToken::new()).unwrap_err(), APPROVAL_PENDING);
    assert_eq!(lease.while_live(|| 7), Some(7));
    assert!({
        let doc = tabs.document(0).unwrap();
        doc.approving && doc.close_request.is_none()
    });
    tabs.document(0).unwrap().approving = false;
    tabs.confirm_desktop_cleanup(0, "current", &CancellationToken::new()).unwrap();
    assert!(loss.is_cancelled());
    assert_eq!(lease.while_live(|| 7), None);
    assert!(lease.release().is_ok());
}

#[test]
fn confirmed_close_blocks_mutations_but_not_status_or_shared_desktop_release() {
    let dir = tempfile::tempdir().unwrap();
    let (tabs, lease) = test_lease_at(dir.path(), Protocol::Vnc);
    let id = lease.id;
    let selector = id.0.to_string();
    let source = {
        let mut doc = tabs.document(0).unwrap();
        let source = doc.tabs.keys().next().unwrap().clone();
        doc.tabs.get_mut(&source).unwrap().ids.push(id);
        doc.entries.insert(id, Entry { endpoint: String::new(), protocol: Some(Protocol::Vnc) });
        source
    };
    let target = lease.target();
    let recipient = tabs.open(0, dir.path(), target.into(), "공유받는 탭".into()).unwrap();
    let revoked = CancellationToken::new();
    tabs.confirm_desktop_cleanup(0, "close", &revoked).unwrap();
    assert_eq!(tabs.confirm_desktop_cleanup(0, "competing", &revoked).unwrap_err(), CLOSING);
    assert_eq!(tabs.open(0, dir.path(), target.into(), "새 터널".into()).unwrap_err(), CLOSING);
    assert_eq!(tabs.share(0, &source, &recipient, id).unwrap_err(), CLOSING);
    assert_eq!(tabs.close(0, &source).unwrap_err(), CLOSING);
    assert_eq!(tabs.start(0, dir.path(), &source, PortRequest::Dynamic { listen: 1080 },
        |_| panic!("창을 닫는 중에 SSH 승인을 요청했습니다")).unwrap_err(), CLOSING);
    assert_eq!(tabs.preflight_desktop(0, (&source, &selector), target, lease.destination(), Protocol::Vnc).unwrap_err(), CLOSING);
    assert_eq!(tabs.open_desktop(0, dir.path(), (&source, &selector), target, lease.destination(), Protocol::Vnc,
        lease.approval()).err().unwrap(), CLOSING);

    assert_eq!(tabs.snapshot(0, &source).unwrap().len(), 1);
    assert_eq!(lease.while_live(|| 7), Some(7));
    lease.release().unwrap();
    assert_eq!(tabs.snapshot(0, &source).unwrap().len(), 1);
    tabs.release_desktop_close_gate(0, "competing");
    assert_eq!(tabs.close(0, &source).unwrap_err(), CLOSING);
    tabs.release_desktop_close_gate(0, "close");
    tabs.share(0, &source, &recipient, id).unwrap();
    tabs.close(0, &source).unwrap();
    assert_eq!(tabs.snapshot(0, &recipient).unwrap().len(), 1);

    tabs.confirm_desktop_cleanup(0, "destroy", &revoked).unwrap();
    assert_eq!(tabs.close(0, &recipient).unwrap_err(), CLOSING);
    tabs.navigate(true);
    assert_eq!(tabs.close(0, &recipient).unwrap_err(), STALE);
}

#[test]
fn desktop_status_serializes_only_ready_approved_loopback() {
    let owners = Default::default();
    let listen = ([127, 0, 0, 1], 3390).into();
    let route = |host: &str| Route::SshLocal {
        server: "ssh-server".into(), listen, destination: Destination { host: host.into(), port: 3389 },
    };
    let encode = |route: &Route, state: &State, protocol| serde_json::to_value(status(
        View { id: TunnelId(1), route, owners: &owners, state }, Vec::new(),
        &Entry { endpoint: String::new(), protocol }, "ssh-server",
    )).unwrap();
    let local = route("::1");
    let other = route("localhost");
    assert_eq!(encode(&local, &State::Ready, Some(Protocol::Rdp))["desktop"], json!({
        "protocol": "rdp", "targetId": "ssh-server", "destination": {"host": "::1", "port": 3389}
    }));
    assert!(encode(&local, &State::Starting, Some(Protocol::Rdp)).get("desktop").is_none());
    assert!(encode(&local, &State::Ready, None).get("desktop").is_none());
    assert!(encode(&other, &State::Ready, Some(Protocol::Rdp)).get("desktop").is_none());
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "격리 Ubuntu OpenSSH 서버와 ZZABBA_R01_LOOPBACK_FIXTURE 필요"]
fn isolated_openssh_desktop_lease_outlives_source_tab() {
    use crate::tunnels::tests::loopback::{Fixture, SERVER};
    use std::{io::{ErrorKind, Read, Write}, net::TcpListener, time::Instant};

    let fixture = Fixture::new();
    let directory = fixture.directory.path();
    let timeout = Duration::from_secs(4);
    let service = TcpListener::bind("127.0.0.1:0").unwrap();
    service.set_nonblocking(true).unwrap();
    let destination = Destination { host: "127.0.0.1".into(), port: service.local_addr().unwrap().port() };
    let listen = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let tabs = TunnelTabs::default();
    let epoch = tabs.epoch();
    let source = tabs.open(epoch, directory, SERVER.into(), "시험 터널".into()).unwrap();
    let id = tabs.start(epoch, directory, &source, PortRequest::Local {
        listen: listen.port(), host: destination.host.clone(), port: destination.port, protocol: Some(Protocol::Rdp),
    }, |description| {
        let ssh = &fixture.ssh.1;
        assert_eq!(description, format!("{}@{}:{}\n{}\n-L {listen} → {}:{}\n서비스: RDP",
            ssh.account, ssh.host, ssh.port, ssh.host_key_fingerprint, destination.host, destination.port));
        Ok(true)
    }).unwrap().unwrap();
    let deadline = Instant::now() + timeout;
    loop {
        let snapshot = tabs.snapshot(epoch, &source).unwrap();
        let status = &snapshot[0];
        if status.state == "ready" { break; }
        assert!(status.state == "starting" && Instant::now() < deadline, "SSH 수신 준비 실패: {:?}", status.error);
        thread::sleep(Duration::from_millis(5));
    }

    let (revision, ssh) = &fixture.ssh;
    let mut lease = tabs.open_desktop(epoch, directory, (&source, &id), SERVER, &destination,
        Protocol::Rdp, (*revision, ssh)).unwrap();
    let loss = lease.loss();
    let mut client = lease.take_stream().unwrap();
    client.set_read_timeout(Some(timeout)).unwrap();
    let deadline = Instant::now() + timeout;
    let mut peer = loop {
        match service.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("SSH 전달 서비스 연결: {error}"),
        }
    };
    peer.set_write_timeout(Some(timeout)).unwrap();
    let mut marker = [0; 4];
    peer.write_all(b"RDP1").unwrap();
    client.read_exact(&mut marker).unwrap();
    assert_eq!(&marker, b"RDP1");

    tabs.close(epoch, &source).unwrap();
    assert!(!loss.is_cancelled());
    peer.write_all(b"RDP2").unwrap();
    client.read_exact(&mut marker).unwrap();
    assert_eq!(&marker, b"RDP2");
    assert!(!loss.is_cancelled());

    drop(client);
    lease.release().unwrap();
    assert!(loss.is_cancelled());
    let error = TcpStream::connect_timeout(&listen, timeout).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ConnectionRefused, "마지막 소유자가 닫힌 뒤 SSH 수신 포트");
}

fn save_draft(directory: &Path, profile: serde_json::Value) {
    let draft = serde_json::from_value::<crate::connections::profile::Draft>(profile).unwrap().validate().unwrap();
    crate::connections::save_test_profile(directory, draft);
}

fn ssm_desktop_draft(directory: &Path, id: &str, protocol: &str, host: &str, port: u16) -> serde_json::Value {
    let mut profile = crate::connections::tests::draft(directory, "work", id);
    let route = json!({"kind": "ssm", "profile": "company-sso", "region": "ap-northeast-2", "instanceId": "i-0123456789abcdef0"});
    let settings = if protocol == "rdp" { json!({"host": host, "port": port, "tlsOnly": false, "route": route}) }
        else { json!({"host": host, "port": port, "route": route}) };
    profile["target"] = json!({"id": id, "groupId": "work", "displayName": "원격 화면", "defaultMethod": protocol, protocol: settings});
    profile
}

#[test]
fn ssm_start_binds_saved_scope_and_fixed_desktop_route_before_approval() {
    let tabs = TunnelTabs::default();
    let dir = tempfile::tempdir().unwrap();
    save_draft(dir.path(), crate::connections::tests::ssm_draft(dir.path()));
    let node = tabs.open(0, dir.path(), "ssm-node".into(), "SSM 탭".into()).unwrap();
    let ssm = |port, host: Option<&str>, protocol| PortRequest::Ssm { listen: 50022, port, host: host.map(Into::into), protocol };
    assert_eq!(tabs.start(0, dir.path(), &node, ssm(5901, Some("vnc.example.test"), Some(Protocol::Vnc)),
        |_| panic!("승인 창을 열었습니다")).unwrap_err(), "VNC SSM 터널은 노드 자신의 서비스만 지원합니다");
    assert!(tabs.start(0, dir.path(), &node, ssm(22, Some("a,b"), None), |_| panic!("승인 창을 열었습니다"))
        .unwrap_err().starts_with("SSM 전달 승인"));
    let declined = tabs.start(0, dir.path(), &node, ssm(22, None, None), |description| {
        assert_eq!(description, "SSM 프로필: company-sso\nSSM 리전: ap-northeast-2\nSSM 관리 노드: i-0123456789abcdef0\n문서: AWS-StartPortForwardingSession\n매개변수: portNumber=22,localPortNumber=50022\nSSM 127.0.0.1:50022 → i-0123456789abcdef0:22");
        Ok(false)
    });
    assert_eq!(declined.unwrap(), None);
    assert!(tabs.document(0).unwrap().entries.is_empty());
    // SSM 대상 탭에서는 SSH 전달을 시작하지 않는다.
    assert_eq!(tabs.start(0, dir.path(), &node, PortRequest::Dynamic { listen: 1080 }, |_| panic!("승인 창을 열었습니다"))
        .unwrap_err(), "저장된 직접 SSH 대상이 필요합니다");

    save_draft(dir.path(), ssm_desktop_draft(dir.path(), "rdp-ssm", "rdp", "rdp.example.test", 3389));
    let desktop = tabs.open(0, dir.path(), "rdp-ssm".into(), "RDP 경로".into()).unwrap();
    for changed in [ssm(3390, Some("rdp.example.test"), Some(Protocol::Rdp)), ssm(3389, None, Some(Protocol::Rdp)),
        ssm(3389, Some("other.example.test"), Some(Protocol::Rdp)), ssm(3389, Some("rdp.example.test"), None)]
    {
        assert_eq!(tabs.start(0, dir.path(), &desktop, changed, |_| panic!("승인 창을 열었습니다")).unwrap_err(),
            "저장된 원격 화면 경로와 전달 요청이 일치하지 않습니다");
    }
    let declined = tabs.start(0, dir.path(), &desktop, ssm(3389, Some("rdp.example.test"), Some(Protocol::Rdp)), |description| {
        assert!(description.contains("문서: AWS-StartPortForwardingSessionToRemoteHost\n매개변수: host=rdp.example.test,portNumber=3389,localPortNumber=50022"));
        assert!(description.ends_with("SSM 127.0.0.1:50022 → i-0123456789abcdef0 → rdp.example.test:3389\n서비스: RDP"));
        Ok(false)
    });
    assert_eq!(declined.unwrap(), None);
    // 다른 저장 대상의 탭에는 공유하지 않는다.
    assert_eq!(tabs.share(0, &node, &desktop, TunnelId(1)).unwrap_err(), "같은 저장 대상의 탭에만 터널을 공유할 수 있습니다");
}

#[test]
fn ssm_status_exposes_only_ready_desktop_destinations() {
    let owners = Default::default();
    let listen = ([127, 0, 0, 1], 50389).into();
    let route = |host: Option<&str>| Route::Ssm { listen, node: "i-0123456789abcdef0".into(), port: 3389, host: host.map(Into::into) };
    let encode = |route: &Route, state: &State, protocol| serde_json::to_value(status(
        View { id: TunnelId(1), route, owners: &owners, state }, Vec::new(),
        &Entry { endpoint: String::new(), protocol }, "rdp-ssm",
    )).unwrap();
    let remote = route(Some("rdp.example.test"));
    assert_eq!(encode(&remote, &State::Ready, Some(Protocol::Rdp))["desktop"], json!({
        "protocol": "rdp", "targetId": "rdp-ssm", "destination": {"host": "rdp.example.test", "port": 3389}
    }));
    assert_eq!(encode(&route(None), &State::Ready, Some(Protocol::Vnc))["desktop"]["destination"], json!({"host": "127.0.0.1", "port": 3389}));
    assert_eq!(encode(&remote, &State::Ready, None)["route"], "SSM 127.0.0.1:50389 → i-0123456789abcdef0 → rdp.example.test:3389");
    assert!(encode(&remote, &State::Starting, Some(Protocol::Rdp)).get("desktop").is_none());
    assert!(encode(&remote, &State::Ready, None).get("desktop").is_none());
}

#[test]
fn ssm_desktop_lease_uses_the_owned_plugin_listener_and_outlives_the_source_tab() {
    use std::io::{Read, Write};

    let tabs = TunnelTabs::default();
    let dir = tempfile::tempdir().unwrap();
    save_draft(dir.path(), ssm_desktop_draft(dir.path(), "vnc-ssm", "vnc", "", 5900));
    let saved = with_saved_route(dir.path(), "vnc-ssm", |saved| saved.unwrap()).unwrap();
    let epoch = tabs.epoch();
    let handle = tabs.open(epoch, dir.path(), "vnc-ssm".into(), "VNC 경로".into()).unwrap();
    let listen = super::super::ssm::tests::bind_loopback().local_addr().unwrap();
    let selector = {
        let mut doc = tabs.document(epoch).unwrap();
        let owner = doc.tabs[&handle].owner.clone();
        let id = doc.tunnels.begin(owner, Route::Ssm { listen, node: "i-0123456789abcdef0".into(), port: 5900, host: None }).unwrap();
        doc.tabs.get_mut(&handle).unwrap().ids.push(id);
        doc.entries.insert(id, Entry { endpoint: String::new(), protocol: Some(Protocol::Vnc) });
        let child = super::super::ssm::fixture_child("cli", listen, None, Ok("fixture-0123456789abcdef0"), None).unwrap();
        assert!(doc.tunnels.finish_start(id, Ok(Forward::Ssm(child))).unwrap());
        id.0.to_string()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let status = loop {
        let snapshot = tabs.snapshot(epoch, &handle).unwrap();
        let status = serde_json::to_value(&snapshot[0]).unwrap();
        if status["state"] == "ready" { break status; }
        assert!(status["state"] == "starting" && std::time::Instant::now() < deadline, "{status}");
        thread::sleep(Duration::from_millis(20));
    };
    let destination = Destination { host: "127.0.0.1".into(), port: 5900 };
    assert_eq!(status["desktop"], json!({"protocol": "vnc", "targetId": "vnc-ssm", "destination": {"host": "127.0.0.1", "port": 5900}}));
    tabs.preflight_desktop(epoch, (&handle, &selector), "vnc-ssm", &destination, Protocol::Vnc).unwrap();
    assert_eq!(tabs.preflight_desktop(epoch, (&handle, &selector), "vnc-ssm", &Destination { host: "::1".into(), port: 5900 }, Protocol::Vnc)
        .unwrap_err(), "승인된 터널 목적지가 일치하지 않습니다");
    let mut lease = tabs.open_desktop(epoch, dir.path(), (&handle, &selector), "vnc-ssm", &destination, Protocol::Vnc, &saved).unwrap();
    assert_eq!(lease.approval(), Confirmed::from(&saved));
    let mut stream = lease.take_stream().unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    tabs.close(epoch, &handle).unwrap();
    let sample = [0x00, b'\r', b'\n', 0x1b, 0xff];
    stream.write_all(&sample).unwrap();
    let mut echoed = [0; 5];
    stream.read_exact(&mut echoed).unwrap();
    assert_eq!(echoed, sample);
    drop(stream);
    lease.release().unwrap();
    super::super::ssm::tests::assert_closed(listen);
}
