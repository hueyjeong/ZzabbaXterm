use serde_json::json;

use super::*;

#[test]
fn only_the_same_document_and_parent_can_revoke_an_attachment() {
    let ipc = SftpIpc::default();
    ipc.navigate("main", 1);
    ipc.navigate("other", 1);
    let (first, _) = ipc.reserve("main", 1, "ssh-1").unwrap();
    let (second, _) = ipc.reserve("main", 1, "ssh-2").unwrap();
    let (other, _) = ipc.reserve("other", 1, "ssh-1").unwrap();

    ipc.close_parent("main", 0, "ssh-1");
    assert!(ipc.current("main", 1, "ssh-1", &first).is_ok());
    ipc.close_parent("main", 1, "ssh-1");
    assert_eq!(ipc.current("main", 1, "ssh-1", &first), Err(Error::Disconnected));
    assert!(ipc.current("main", 1, "ssh-2", &second).is_ok());
    assert!(ipc.current("other", 1, "ssh-1", &other).is_ok());

    ipc.navigate("main", 2);
    let (current, _) = ipc.reserve("main", 2, "ssh-2").unwrap();
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
    let (closing, _) = ipc.reserve("main", 1, "ssh").unwrap();
    closing.owners.lock().unwrap().closing = true;
    assert_eq!(ipc.current("main", 1, "ssh", &closing), Err(Error::Disconnected));
    assert!(ipc.reserve("main", 1, "ssh").is_err());
    assert!(ipc.remove("main", 1, "ssh", Some(&closing)).unwrap().is_some());
    let (replacement, _) = ipc.reserve("main", 1, "ssh").unwrap();
    assert!(ipc.remove("main", 1, "ssh", Some(&closing)).unwrap().is_none());
    assert!(ipc.current("main", 1, "ssh", &replacement).is_ok());
}

#[tokio::test]
async fn bridge_and_panel_share_the_parent_transport_but_not_handles_across_parents() {
    use sftp::tests::in_memory_session;

    const PATH: &str = "/workspace/한글.txt";
    let first_root = tempfile::tempdir().unwrap();
    let second_root = tempfile::tempdir().unwrap();
    let ipc = SftpIpc::default();
    ipc.navigate("main", 1);
    ipc.navigate("other", 1);
    assert!(matches!(ipc.bind_bridge("other", 1, "first", SharedSftpNamespace::AdministratorConfirmed),
        Err(BridgeError(Error::Unavailable))));
    let bridge = ipc.bind_bridge("main", 1, "first", SharedSftpNamespace::AdministratorConfirmed).unwrap();
    let remote = ipc.open_bridge_with(&bridge, PATH, async { Ok(in_memory_session(first_root.path(), "첫 파일").await) }).await.unwrap();

    let (slot, panel) = ipc.reserve("main", 1, "first").unwrap();
    assert!(Arc::ptr_eq(&slot, &bridge.slot));
    let listing = ipc.attach_panel("main", 1, "first", &slot, &panel, async { panic!("같은 부모의 전송을 재사용해야 함") }).await.unwrap();
    let listed = {
        let mut guard = slot.state.lock().await;
        let Attachment::Active(attached) = &mut *guard else { panic!("SFTP 전송 없음") };
        assert!(matches!(attached.open(PATH).await, Err(Error::InvalidEntry)));
        attached.open(&listing.entries[0].entry_id).await.unwrap()
    };
    assert_eq!((&remote.name, &remote.text), (&listed.name, &listed.text));
    assert_ne!(remote.handle, listed.handle);

    let other = ipc.bind_bridge("main", 1, "second", SharedSftpNamespace::AdministratorConfirmed).unwrap();
    let same_name = ipc.open_bridge_with(&other, PATH, async { Ok(in_memory_session(second_root.path(), "둘째 파일").await) }).await.unwrap();
    assert_eq!(same_name.name, remote.name);
    assert_ne!(same_name.text, remote.text);
    assert!(matches!(ipc.watch_bridge_close(&other, &remote.handle, || panic!()).await, Err(BridgeError(Error::InvalidHandle))));
    assert!(matches!(ipc.watch_bridge_close(&bridge, &listed.handle, || panic!()).await, Err(BridgeError(Error::InvalidHandle))));

    let (closed, mut observed) = tokio::sync::oneshot::channel();
    ipc.watch_bridge_close(&bridge, &remote.handle, || { let _ = closed.send(()); }).await.unwrap();
    panel.cancel();
    {
        let mut guard = slot.state.lock().await;
        ipc.release_panel("main", 1, "first", &slot, &panel, &mut guard).await.unwrap();
        let Attachment::Active(attached) = &mut *guard else { panic!("패널 분리로 브리지 전송이 종료됨") };
        assert!(matches!(attached.read(&listed.handle, false).await, Err(Error::InvalidHandle)));
        assert_eq!(attached.read(&remote.handle, true).await.unwrap(), "첫 파일");
        assert!(matches!(attached.save(&remote.handle, "새 내용").await.unwrap(), SaveResult::Saved));
        assert!(matches!(attached.open(&listing.entries[0].entry_id).await, Err(Error::InvalidEntry)));
    }
    ipc.close_file("main", 1, "second", &other.slot, &remote.handle).await;
    assert!(matches!(observed.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
    ipc.close_file("main", 1, "first", &slot, &remote.handle).await;
    assert_eq!(observed.try_recv(), Ok(()));

    let (reattached, new_panel) = ipc.reserve("main", 1, "first").unwrap();
    assert!(Arc::ptr_eq(&reattached, &slot));
    {
        let mut guard = slot.state.lock().await;
        ipc.release_panel("main", 1, "first", &slot, &panel, &mut guard).await.unwrap();
        let owners = slot.owners.lock().unwrap();
        assert!(Arc::ptr_eq(owners.panel.as_ref().unwrap(), &new_panel));
    }
    let discarded = ipc.open_bridge_with(&bridge, PATH, async { panic!("다시 열기도 같은 전송을 사용해야 함") }).await.unwrap();
    ipc.watch_bridge_close(&bridge, &discarded.handle, || panic!("오퍼 폐기는 탭 닫기가 아님")).await.unwrap();
    ipc.discard_bridge_file(&bridge, &discarded.handle).await;
    ipc.close_file("main", 1, "first", &slot, &discarded.handle).await;

    let cancelled = bridge.cancellation();
    ipc.navigate("main", 2);
    assert!(cancelled.is_cancelled() && other.cancellation().is_cancelled());
    assert!(matches!(ipc.open_bridge_with(&bridge, PATH, async { panic!("이전 세대의 요청") }).await,
        Err(BridgeError(Error::DocumentClosed))));
}

#[tokio::test]
async fn bridge_cancellation_during_attach_rejects_the_open_without_revoking_the_panel() {
    let root = tempfile::tempdir().unwrap();
    let ipc = SftpIpc::default();
    ipc.navigate("main", 1);
    let bridge = ipc.bind_bridge("main", 1, "ssh", SharedSftpNamespace::AdministratorConfirmed).unwrap();
    let cancel = bridge.cancellation();
    let start = async {
        cancel.cancel();
        Ok(sftp::tests::in_memory_session(root.path(), "내용").await)
    };
    assert!(matches!(ipc.open_bridge_with(&bridge, "/workspace/한글.txt", start).await,
        Err(BridgeError(Error::Disconnected))));

    let (slot, panel) = ipc.reserve("main", 1, "ssh").unwrap();
    let listing = ipc.attach_panel("main", 1, "ssh", &slot, &panel, async { panic!("취소는 공유 전송을 닫지 않음") }).await.unwrap();
    let mut guard = slot.state.lock().await;
    let Attachment::Active(attached) = &mut *guard else { panic!("SFTP 전송 없음") };
    assert!(attached.files.is_empty());
    assert_eq!(attached.open(&listing.entries[0].entry_id).await.unwrap().text, "내용");
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
