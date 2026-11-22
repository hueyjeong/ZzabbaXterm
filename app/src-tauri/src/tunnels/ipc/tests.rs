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
