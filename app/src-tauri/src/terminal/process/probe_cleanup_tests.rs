use super::{ssh_child::tests::{Isolated, Restricted}, *};
use crate::{ssh::{ApprovedHost, Authentication, Endpoint, Transport, TrustStore}, terminal::probe::Probe};

#[test]
fn f01_probe_retry_reaps_only_the_original_child_and_releases_its_plan() {
    let dir = tempfile::tempdir().unwrap();
    let store = TrustStore::create(&dir.path().join("trust")).unwrap();
    let endpoint = Endpoint::new("127.0.0.1", 22).unwrap();
    let approved = ApprovedHost::confirm(&endpoint, &endpoint, "SHA256:UCUiLr7Pjs9wFFJMDByLgc3NrtdU344OgUM45wZPcIQ").unwrap();
    store.register(&approved, "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti").unwrap();
    let plan = ConnectionPlan::prepare(&store, &approved, "fixture", Transport::Direct, Authentication::ManualPassword).unwrap();
    let config = std::path::PathBuf::from(plan.invocation(crate::ssh::Client::Ssh).unwrap().args[1]);
    let child = Isolated::waiting();
    let mut peer = Isolated::waiting();
    let mut child = Restricted {
        #[cfg(windows)]
        handle: ssh_child::tests::restricted_handle(&child.0, 0x00101000), // SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION
        child,
    };
    let probe = Probe::with_timeout("main", Duration::from_secs(8), false, true);
    let session = probe.session();
    session.close_result.lock().unwrap().local = LocalClose::Unconfirmed;
    let failure = terminate_ssh(&mut child).unwrap_err();
    session.set_ssh_error(failure);
    let stdin = child.child.0.stdin.take().unwrap();
    *session.ssh_pending.lock().unwrap() = Some(OwnedSsh { child: Box::new(child), _plan: Arc::new(plan) });
    probe.finish(Err("취소".into()));

    assert!(probe.cancel_result(false).is_err());
    assert!(!probe.confirm_ssh_close());
    assert!(config.exists());
    drop(stdin);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if probe.cancel_result(true).is_ok() { break; }
        assert!(std::time::Instant::now() < deadline, "소유 자식 종료 확인 제한 시간 초과");
        thread::sleep(POLL);
    }
    assert!(probe.confirm_ssh_close());
    assert!(!config.exists());
    assert!(peer.0.try_wait().unwrap().is_none());
}
