use super::*;

fn fixture(state: &NanoBridge) -> (NanoOffer, oneshot::Receiver<Outcome>, CancellationToken, CancellationToken) {
    let request = CancellationToken::new();
    let slot = CancellationToken::new();
    let (offer, done) = state.offer(uuid::Uuid::new_v4().simple().to_string(), 5, "12", Opened {
        handle: "exact-handle".into(),
        name: "한글.txt".into(),
        text: "내용".into(),
    }, request.clone(), slot.clone());
    (offer, done, request, slot)
}

#[test]
fn recovery_claim_and_mount_stay_in_the_exact_document() {
    let state = NanoBridge::default();
    let (offer, mut done, _, _) = fixture(&state);
    let other = fixture(&state).0;
    let id = &offer.offer_id;
    assert!(state.pending("other", 5).is_empty());
    assert!(state.pending(MAIN, 4).is_empty());
    assert!(state.pending(MAIN, 5).iter().any(|pending| &pending.offer_id == id && pending.session_id == "12"));
    assert!(state.claim("other", 5, id).is_none());
    assert!(state.claim(MAIN, 4, id).is_none());
    assert!(!state.mounted(MAIN, 5, id, "exact-handle"));
    let claim = state.claim(MAIN, 5, id).unwrap();
    assert_eq!(claim.handle, "exact-handle");
    assert_eq!(claim.name, "한글.txt");
    assert_eq!(claim.text, "내용");
    assert!(state.claim(MAIN, 5, id).is_none());
    assert_eq!(state.pending(MAIN, 5).len(), 1);
    assert!(!state.mounted(MAIN, 5, id, "wrong-handle"));
    assert!(!state.mounted(MAIN, 6, id, &claim.handle));
    assert!(state.mounted(MAIN, 5, id, &claim.handle));
    assert_eq!(state.mount_failed(MAIN, 5, id, None), MountStatus::Unavailable);
    state.interrupted(id, true);
    state.file_closed(MAIN, 5, "12", &other.offer_id, &claim.handle);
    state.file_closed(MAIN, 5, "13", id, &claim.handle);
    state.file_closed(MAIN, 4, "12", id, &claim.handle);
    state.file_closed(MAIN, 5, "12", id, "wrong-handle");
    assert!(matches!(done.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
    state.file_closed(MAIN, 5, "12", id, &claim.handle);
    assert_eq!(done.try_recv().unwrap(), Outcome::Closed);
}

#[test]
fn cancel_and_timeout_only_allow_fallback_before_claim() {
    let state = NanoBridge::default();
    let (offer, mut done, request, _) = fixture(&state);
    request.cancel();
    assert!(state.claim(MAIN, 5, &offer.offer_id).is_none());
    state.interrupted(&offer.offer_id, false);
    assert_eq!(done.try_recv().unwrap(), Outcome::Unattached);

    let (offer, mut done, _, _) = fixture(&state);
    state.interrupted(&offer.offer_id, true);
    assert_eq!(done.try_recv().unwrap(), Outcome::Unattached);

    let (offer, mut done, _, _) = fixture(&state);
    state.claim(MAIN, 5, &offer.offer_id).unwrap();
    state.interrupted(&offer.offer_id, true);
    assert_eq!(done.try_recv().unwrap(), Outcome::Failed);
    assert!(!state.mounted(MAIN, 5, &offer.offer_id, "exact-handle"));
}

#[test]
fn failed_claim_delivery_and_disconnected_mount_fail_the_remote_wait() {
    let state = NanoBridge::default();
    let (offer, mut done, _, _) = fixture(&state);
    state.claim(MAIN, 5, &offer.offer_id).unwrap();
    assert_eq!(state.mount_failed(MAIN, 4, &offer.offer_id, None), MountStatus::Unavailable);
    assert!(matches!(done.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
    assert_eq!(state.mount_failed(MAIN, 5, &offer.offer_id, None), MountStatus::Unavailable);
    assert_eq!(done.try_recv().unwrap(), Outcome::Failed);
    assert_eq!(state.mount_failed(MAIN, 5, &offer.offer_id, None), MountStatus::Unavailable);

    let (offer, mut done, _, slot) = fixture(&state);
    state.claim(MAIN, 5, &offer.offer_id).unwrap();
    assert!(state.mounted(MAIN, 5, &offer.offer_id, "exact-handle"));
    slot.cancel();
    assert!(!state.is_mounted(MAIN, 5, "12", &offer.offer_id, "exact-handle"));
    state.file_closed(MAIN, 5, "12", &offer.offer_id, "exact-handle");
    assert_eq!(done.try_recv().unwrap(), Outcome::Failed);
}

#[test]
fn recovery_retains_a_live_exact_mount_or_revokes_the_exact_claim() {
    let state = NanoBridge::default();
    let (offer, mut done, _, _) = fixture(&state);
    let id = &offer.offer_id;
    let claim = state.claim(MAIN, 5, id).unwrap();
    let handle = claim.handle.as_str();
    assert_eq!(state.mount_failed(MAIN, 5, id, Some("wrong-handle")), MountStatus::Unavailable);
    assert!(state.mounted(MAIN, 5, id, handle));
    assert_eq!(state.mount_failed("other", 5, id, Some(handle)), MountStatus::Unavailable);
    assert_eq!(state.mount_failed(MAIN, 4, id, Some(handle)), MountStatus::Unavailable);
    assert_eq!(state.mount_failed(MAIN, 5, id, Some(handle)), MountStatus::Mounted);
    assert!(state.is_mounted(MAIN, 5, "12", id, handle));
    assert!(!state.is_mounted(MAIN, 5, "12", id, "wrong-handle"));
    assert_eq!(state.mount_failed(MAIN, 5, id, Some("wrong-handle")), MountStatus::Unavailable);
    assert!(matches!(done.try_recv(), Err(oneshot::error::TryRecvError::Empty)));

    let (offer, mut done, _, _) = fixture(&state);
    let revoked_id = &offer.offer_id;
    state.claim(MAIN, 5, revoked_id).unwrap();
    assert_eq!(state.mount_failed(MAIN, 5, revoked_id, Some(handle)), MountStatus::Unavailable);
    assert_eq!(done.try_recv().unwrap(), Outcome::Failed);
    assert!(!state.mounted(MAIN, 5, revoked_id, handle));
    assert!(state.is_mounted(MAIN, 5, "12", id, handle));
}

#[test]
fn cancelling_a_pending_offer_preserves_fallback_and_cancelled_mounts_are_unavailable() {
    let state = NanoBridge::default();
    let (offer, mut done, _, _) = fixture(&state);
    assert_eq!(state.mount_failed(MAIN, 5, &offer.offer_id, None), MountStatus::Unavailable);
    assert_eq!(done.try_recv().unwrap(), Outcome::Unattached);
    assert!(state.claim(MAIN, 5, &offer.offer_id).is_none());

    let (offer, mut done, cancel, _) = fixture(&state);
    let claim = state.claim(MAIN, 5, &offer.offer_id).unwrap();
    assert!(state.mounted(MAIN, 5, &offer.offer_id, &claim.handle));
    cancel.cancel();
    assert_eq!(state.mount_failed(MAIN, 5, &offer.offer_id, Some(&claim.handle)), MountStatus::Unavailable);
    assert_eq!(done.try_recv().unwrap(), Outcome::Failed);
}

#[test]
fn mount_recovery_result_is_a_lowercase_wire_string() {
    let wire = |status| serde_json::to_value(status).unwrap();
    assert_eq!(wire(MountStatus::Mounted), serde_json::json!("mounted"));
    assert_eq!(wire(MountStatus::Unavailable), serde_json::json!("unavailable"));
}

#[tokio::test]
async fn only_matching_cancel_interrupts_open_without_revoking_the_slot() {
    let cancel = CancellationToken::new();
    let request = cancel.child_token();
    let slot = CancellationToken::new();
    let (send, mut receive) = mpsc::channel(2);
    send.try_send(Request::Cancel { id: 10 }).unwrap();
    {
        let opened = async {
            request.cancelled().await;
            Outcome::Unattached
        };
        let pending = drive_open(11, opened, &request, &cancel, &slot, &mut receive);
        tokio::pin!(pending);
        tokio::select! {
            biased;
            outcome = &mut pending => panic!("unexpected response: {outcome:?}"),
            _ = std::future::ready(()) => {},
        }
        assert!(!request.is_cancelled());
        send.try_send(Request::Cancel { id: 11 }).unwrap();
        assert_eq!(pending.await, Some(Outcome::Unattached));
    }
    assert!(!slot.is_cancelled());
}

#[tokio::test]
async fn channel_disconnect_retires_the_slot_and_drains_open() {
    let cancel = CancellationToken::new();
    let request = cancel.child_token();
    let slot = CancellationToken::new();
    let (send, mut receive) = mpsc::channel(1);
    drop(send);
    let mut drained = false;
    let opened = async {
        slot.cancelled().await;
        drained = true;
        Outcome::Failed
    };
    assert_eq!(drive_open(1, opened, &request, &cancel, &slot, &mut receive).await, None);
    assert!(request.is_cancelled());
    assert!(drained);
}
