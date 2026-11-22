use serde_json::json;

use super::*;

#[test]
fn only_the_same_document_and_parent_can_revoke_an_attachment() {
    let ipc = SftpIpc::default();
    ipc.navigate("main", 1);
    ipc.navigate("other", 1);
    let first = ipc.reserve("main", 1, "ssh-1").unwrap();
    let second = ipc.reserve("main", 1, "ssh-2").unwrap();
    let other = ipc.reserve("other", 1, "ssh-1").unwrap();

    ipc.close_parent("main", 0, "ssh-1");
    assert!(ipc.current("main", 1, "ssh-1", &first).is_ok());
    ipc.close_parent("main", 1, "ssh-1");
    assert_eq!(ipc.current("main", 1, "ssh-1", &first), Err(Error::Disconnected));
    assert!(ipc.current("main", 1, "ssh-2", &second).is_ok());
    assert!(ipc.current("other", 1, "ssh-1", &other).is_ok());

    ipc.navigate("main", 2);
    let current = ipc.reserve("main", 2, "ssh-2").unwrap();
    assert_eq!(ipc.current("main", 1, "ssh-2", &second), Err(Error::DocumentClosed));
    ipc.close_parent("main", 1, "ssh-2");
    assert!(ipc.current("main", 2, "ssh-2", &current).is_ok());
    ipc.destroy("main");
    assert_eq!(ipc.current("main", 2, "ssh-2", &current), Err(Error::DocumentClosed));
    assert!(ipc.current("other", 1, "ssh-1", &other).is_ok());
}

#[test]
fn closing_a_slot_revokes_requests_and_cannot_remove_its_replacement() {
    let ipc = SftpIpc::default();
    ipc.navigate("main", 1);
    let closing = ipc.reserve("main", 1, "ssh").unwrap();
    closing.closing.store(true, Ordering::Release);
    assert_eq!(ipc.current("main", 1, "ssh", &closing), Err(Error::Disconnected));
    assert!(ipc.reserve("main", 1, "ssh").is_err());
    assert!(ipc.remove("main", 1, "ssh", Some(&closing)).unwrap().is_some());
    let replacement = ipc.reserve("main", 1, "ssh").unwrap();
    assert!(ipc.remove("main", 1, "ssh", Some(&closing)).unwrap().is_none());
    assert!(ipc.current("main", 1, "ssh", &replacement).is_ok());
}

#[tokio::test]
async fn an_unconfirmed_close_is_not_reported_as_closed_on_retry() {
    let mut attachment = Attachment::CloseFailed(Error::RemoteFailure);
    assert_eq!(attachment.close().await, Err(Error::RemoteFailure));
    assert_eq!(attachment.close().await, Err(Error::RemoteFailure));
}

#[test]
fn a_core_reap_failure_is_retained_across_transport_close() {
    let mut transport = Transport::Reconnect(None);
    transport.observe(&sftp::Error::Reap(std::io::Error::other("회수 실패")));
    transport.require_reconnect();
    assert!(matches!(transport.into_session(), Err(Error::RemoteFailure)));
}

#[test]
fn wire_save_outcomes_preserve_confirmed_state_without_remote_diagnostics() {
    let json_result = |outcome| serde_json::to_value(SaveResult::from(outcome)).unwrap();
    assert_eq!(json_result(sftp::SaveOutcome::Saved), json!({"status":"saved"}));
    assert_eq!(
        json_result(sftp::SaveOutcome::Conflict {
            remote: sftp::RemoteVersion::Text("원격 내용".into()),
            cleanup: sftp::Cleanup::Removed,
        }),
        json!({"status":"conflict","remote":{"kind":"text","text":"원격 내용"},"cleanup":{"kind":"removed"}})
    );
    assert_eq!(
        json_result(sftp::SaveOutcome::Unconfirmed(sftp::Error::Disconnected)),
        json!({"status":"unknown","error":{"kind":"disconnected"}})
    );
    assert_eq!(
        json_result(sftp::SaveOutcome::Failed {
            stage: sftp::SaveStage::Prepare,
            error: sftp::Error::Terminal("진단 원문".into()),
            cleanup: None,
        }),
        json!({"status":"failed","stage":"prepare","error":{"kind":"unavailable"}})
    );
}
