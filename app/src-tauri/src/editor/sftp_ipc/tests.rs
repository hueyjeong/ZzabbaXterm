use serde_json::json;

use super::*;

fn snapshot(token: u8, text: &str) -> Snapshot {
    Snapshot {
        token: [token; 16],
        observation: Observation { len: text.len() as u64, digest: Sha256::digest(text.as_bytes()).into() },
    }
}

fn publisher() -> Publisher {
    Publisher::channel(1, CancellationToken::new()).0
}

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
    use crate::editor::nano_bridge::Outcome;
    use sftp::tests::in_memory_session;

    const PATH: &str = "/workspace/한글.txt";
    const OFFER: &str = "first-offer";
    let first_root = tempfile::tempdir().unwrap();
    let second_root = tempfile::tempdir().unwrap();
    let ipc = SftpIpc::default();
    let nano = NanoBridge::default();
    ipc.navigate("main", 1);
    ipc.navigate("other", 1);
    assert!(matches!(ipc.bind_bridge("other", 1, "first", SharedSftpNamespace::AdministratorConfirmed),
        Err(BridgeError(Error::Unavailable))));
    let bridge = ipc.bind_bridge("main", 1, "first", SharedSftpNamespace::AdministratorConfirmed).unwrap();
    let remote = ipc.open_bridge_with(&bridge, PATH, OFFER, snapshot(1, "첫 파일"), publisher(),
        async { Ok(in_memory_session(first_root.path(), "첫 파일").await) }).await.unwrap();

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
    let same_name = ipc.open_bridge_with(&other, PATH, "second-offer", snapshot(1, "둘째 파일"), publisher(),
        async { Ok(in_memory_session(second_root.path(), "둘째 파일").await) }).await.unwrap();
    assert_eq!(same_name.name, remote.name);
    assert_ne!(same_name.text, remote.text);
    let handle = remote.handle.clone();
    let request = CancellationToken::new();
    let (_, mut observed) = nano.offer(OFFER.into(), 1, "first", remote, request, bridge.cancellation());
    nano.claim("main", 1, OFFER).unwrap();
    assert!(nano.mounted("main", 1, OFFER, &handle));
    ipc.discard_bridge_file(&other, OFFER, &handle).await;
    ipc.discard_bridge_file(&bridge, OFFER, &listed.handle).await;
    ipc.discard_bridge_file(&bridge, "wrong-offer", &handle).await;
    panel.cancel();
    {
        let mut guard = slot.state.lock().await;
        ipc.release_panel("main", 1, "first", &slot, &panel, &mut guard).await.unwrap();
        let Attachment::Active(attached) = &mut *guard else { panic!("패널 분리로 브리지 전송이 종료됨") };
        assert!(matches!(attached.read(&listed.handle, false, |_| false).await, Err(Error::InvalidHandle)));
        assert_eq!(attached.check_file(&slot, &handle, &nano, "main", 1, "first"), Ok(()));
        assert!(matches!(attached.read(&handle, true, |_| false).await, Err(Error::InvalidHandle)));
        assert!(matches!(attached.save(&handle, "새 내용", |_| false).await, Err(Error::InvalidHandle)));
        assert!(matches!(attached.open(&listing.entries[0].entry_id).await, Err(Error::InvalidEntry)));
    }
    ipc.close_file(&nano, "main", 1, "second", &other.slot, &handle).await;
    assert!(matches!(observed.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
    ipc.close_file(&nano, "main", 1, "first", &slot, &handle).await;
    assert_eq!(observed.try_recv().unwrap(), Outcome::Closed);

    let (reattached, new_panel) = ipc.reserve("main", 1, "first").unwrap();
    assert!(Arc::ptr_eq(&reattached, &slot));
    {
        let mut guard = slot.state.lock().await;
        ipc.release_panel("main", 1, "first", &slot, &panel, &mut guard).await.unwrap();
        let owners = slot.owners.lock().unwrap();
        assert!(Arc::ptr_eq(owners.panel.as_ref().unwrap(), &new_panel));
    }
    let discarded = ipc.open_bridge_with(&bridge, PATH, "discarded", snapshot(1, "첫 파일"), publisher(),
        async { panic!("다시 열기도 같은 전송을 사용해야 함") }).await.unwrap();
    let discarded_handle = discarded.handle.clone();
    let (_, mut unnotified) = nano.offer("discarded".into(), 1, "first", discarded,
        CancellationToken::new(), bridge.cancellation());
    nano.claim("main", 1, "discarded").unwrap();
    ipc.discard_bridge_file(&bridge, "discarded", &discarded_handle).await;
    ipc.close_file(&nano, "main", 1, "first", &slot, &discarded_handle).await;
    assert!(matches!(unnotified.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));

    let cancelled = bridge.cancellation();
    ipc.navigate("main", 2);
    assert!(cancelled.is_cancelled() && other.cancellation().is_cancelled());
    assert!(matches!(ipc.open_bridge_with(&bridge, PATH, OFFER, snapshot(1, "첫 파일"), publisher(),
        async { panic!("이전 세대의 요청") }).await,
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
    assert!(matches!(ipc.open_bridge_with(&bridge, "/workspace/한글.txt", "offer", snapshot(1, "내용"), publisher(), start).await,
        Err(BridgeError(Error::Disconnected))));

    let (slot, panel) = ipc.reserve("main", 1, "ssh").unwrap();
    let listing = ipc.attach_panel("main", 1, "ssh", &slot, &panel, async { panic!("취소는 공유 전송을 닫지 않음") }).await.unwrap();
    let mut guard = slot.state.lock().await;
    let Attachment::Active(attached) = &mut *guard else { panic!("SFTP 전송 없음") };
    assert!(attached.files.is_empty());
    assert_eq!(attached.open(&listing.entries[0].entry_id).await.unwrap().text, "내용");
}

#[tokio::test]
async fn bridge_file_authority_requires_the_exact_live_mount() {
    use crate::editor::nano_bridge::{MountStatus, Outcome};

    const PATH: &str = "/workspace/한글.txt";
    const OFFER: &str = "offer";
    let root = tempfile::tempdir().unwrap();
    let ipc = SftpIpc::default();
    let nano = NanoBridge::default();
    ipc.navigate("main", 1);
    let bridge = ipc.bind_bridge("main", 1, "ssh", SharedSftpNamespace::AdministratorConfirmed).unwrap();
    let opened = ipc.open_bridge_with(&bridge, PATH, OFFER, snapshot(1, "내용"), publisher(), async {
        Ok(sftp::tests::in_memory_session(root.path(), "내용").await)
    }).await.unwrap();
    let handle = opened.handle.clone();
    let request = CancellationToken::new();
    let (_, mut done) = nano.offer(OFFER.into(), 1, "ssh", opened, request.clone(), bridge.cancellation());
    let (slot, panel) = ipc.reserve("main", 1, "ssh").unwrap();
    let listing = ipc.attach_panel("main", 1, "ssh", &slot, &panel, async { panic!("공유 전송 재사용") }).await.unwrap();
    {
        let mut guard = slot.state.lock().await;
        let Attachment::Active(attached) = &mut *guard else { panic!("SFTP 전송 없음") };
        let listed = attached.open(&listing.entries[0].entry_id).await.unwrap();
        assert_eq!(attached.check_file(&slot, &listed.handle, &nano, "main", 1, "ssh"), Ok(()));
        let check = |label: &str, generation, parent: &str, handle: &str| {
            attached.check_file(&slot, handle, &nano, label, generation, parent)
        };
        assert_eq!(check("main", 1, "ssh", &handle), Err(Error::InvalidHandle));
        nano.claim("main", 1, OFFER).unwrap();
        assert_eq!(check("main", 1, "ssh", &handle), Err(Error::InvalidHandle));
        assert!(nano.mounted("main", 1, OFFER, &handle));
        assert_eq!(check("main", 1, "ssh", &handle), Ok(()));
        assert_eq!(check("other", 1, "ssh", &handle), Err(Error::InvalidHandle));
        assert_eq!(check("main", 2, "ssh", &handle), Err(Error::InvalidHandle));
        assert_eq!(check("main", 1, "other", &handle), Err(Error::InvalidHandle));
        assert_eq!(check("main", 1, "ssh", "other-handle"), Err(Error::InvalidHandle));
        assert!(!nano.is_mounted("main", 1, "ssh", "wrong-offer", &handle));
        request.cancel();
        assert_eq!(check("main", 1, "ssh", &handle), Err(Error::InvalidHandle));
        assert_eq!(check("main", 1, "ssh", &listed.handle), Ok(()));
    }
    assert_eq!(nano.mount_failed("main", 1, OFFER, Some(&handle)), MountStatus::Unavailable);
    ipc.discard_bridge_file(&bridge, OFFER, &handle).await;
    {
        let guard = slot.state.lock().await;
        let Attachment::Active(attached) = &*guard else { panic!("실패 폐기는 공유 전송을 유지해야 함") };
        assert!(!attached.files.contains_key(&handle));
    }
    ipc.close_file(&nano, "main", 1, "ssh", &slot, &handle).await;
    assert_eq!(done.try_recv().unwrap(), Outcome::Failed);
}

#[tokio::test]
async fn snapshot_mismatch_does_not_create_a_bridge_handle() {
    let root = tempfile::tempdir().unwrap();
    let ipc = SftpIpc::default();
    ipc.navigate("main", 1);
    let bridge = ipc.bind_bridge("main", 1, "ssh", SharedSftpNamespace::AdministratorConfirmed).unwrap();
    let opened = ipc.open_bridge_with(&bridge, "/workspace/한글.txt", "offer", snapshot(1, "다른"), publisher(), async {
        Ok(sftp::tests::in_memory_session(root.path(), "내용").await)
    }).await;
    assert!(matches!(opened, Err(BridgeError(Error::ReadChanged))));
    let guard = bridge.slot.state.lock().await;
    let Attachment::Active(attached) = &*guard else { panic!("SFTP 전송 없음") };
    assert!(attached.files.is_empty());
}

#[tokio::test]
async fn nano_save_requires_exact_submitted_bytes_and_reload_clears_unknown() {
    use crate::editor::nano_bridge::{PublicationOperation, PublicationReply};

    let root = tempfile::tempdir().unwrap();
    let ipc = SftpIpc::default();
    let nano = NanoBridge::default();
    ipc.navigate("main", 1);
    let bridge = ipc.bind_bridge("main", 1, "ssh", SharedSftpNamespace::AdministratorConfirmed).unwrap();
    let request = CancellationToken::new();
    let (publisher, mut work) = Publisher::channel(1, request.clone());
    let opened = ipc.open_bridge_with(&bridge, "/workspace/한글.txt", "offer", snapshot(1, "원본"), publisher, async {
        Ok(sftp::tests::in_memory_session(root.path(), "원본").await)
    }).await.unwrap();
    let handle = opened.handle.clone();
    let _offer = nano.offer("offer".into(), 1, "ssh", opened, request, bridge.cancellation());
    let mounted = |offer: &str| nano.is_mounted("main", 1, "ssh", offer, &handle);
    let mut guard = bridge.slot.state.lock().await;
    let Attachment::Active(attached) = &mut *guard else { panic!("SFTP 전송 없음") };

    for claim in [false, true] {
        if claim { nano.claim("main", 1, "offer").unwrap(); }
        assert!(matches!(attached.save(&handle, "제출", mounted).await, Err(Error::InvalidHandle)));
        assert_eq!(attached.read(&handle, true, mounted).await, Err(Error::InvalidHandle));
        assert!(matches!(work.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)));
    }
    assert!(nano.mounted("main", 1, "offer", &handle));

    let (result, ()) = tokio::join!(attached.save(&handle, "제출", mounted), async {
        let job = work.recv().await.unwrap();
        assert!(matches!(&job.operation, PublicationOperation::Save { token, text } if *token == [1; 16] && text == "제출"));
        // SFTP와 반환 관찰값이 일치해도 제출본과 다르면 미확인이다.
        job.respond(Ok(PublicationReply::Saved(snapshot(2, "원본"))));
    });
    assert!(matches!(result.unwrap(), SaveResult::Unknown { error: Error::ReadChanged, recovery_path: None }));
    assert!(matches!(attached.save(&handle, "제출", mounted).await.unwrap(),
        SaveResult::Failed { error: Error::ReopenRequired, .. }));
    assert!(matches!(work.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)));

    let baseline = snapshot(3, "원본");
    let (read, ()) = tokio::join!(attached.read(&handle, true, mounted), async {
        let job = work.recv().await.unwrap();
        assert!(matches!(&job.operation, PublicationOperation::Compare));
        job.respond(Ok(PublicationReply::Observed(baseline.observation)));
        let job = work.recv().await.unwrap();
        assert!(matches!(&job.operation, PublicationOperation::Reload(observation) if *observation == baseline.observation));
        job.respond(Ok(PublicationReply::Reloaded(baseline)));
    });
    assert_eq!(read.unwrap(), "원본");

    for (token, text) in [(3, "제출"), (4, "둘째")] {
        let (result, ()) = tokio::join!(attached.save(&handle, text, mounted), async {
            let job = work.recv().await.unwrap();
            assert!(matches!(&job.operation, PublicationOperation::Save { token: sent, text: body } if *sent == [token; 16] && body == text));
            std::fs::write(root.path().join("original"), text).unwrap();
            job.respond(Ok(PublicationReply::Saved(snapshot(token + 1, text))));
        });
        assert!(matches!(result.unwrap(), SaveResult::Saved));
    }

    let (result, ()) = tokio::join!(attached.save(&handle, "셋째", mounted), async {
        let job = work.recv().await.unwrap();
        job.respond(Err(PublishError::Unknown(Some(".zxt-edit-stage/original".into()))));
    });
    assert_eq!(serde_json::to_value(result.unwrap()).unwrap(), json!({
        "status": "unknown", "error": { "kind": "remoteFailure" },
        "recoveryPath": "/workspace/.zxt-edit-stage/original"
    }));
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
