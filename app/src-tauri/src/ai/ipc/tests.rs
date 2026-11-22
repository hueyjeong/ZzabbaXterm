use std::fs;

use super::*;
use crate::{ai::review_proposal, connections::tests::{TestDir, draft}};

#[test]
fn ipc_rejects_caller_owned_authority_and_non_query_operations() {
    for (command, body) in [
        ("ai_targets", json!({"path":"somewhere"})),
        ("ai_select", json!({"target":{"targetId":"a","generation":"4","nodeId":"forged"}})),
        ("ai_chat", json!({"model":"fixture","messages":[{"role":"system","content":"x"}]})),
        ("ai_chat", json!({"model":"fixture","messages":[],"terminal":"secret"})),
        ("ai_propose", json!({"requestId":1,"model":"fixture","target":{"targetId":"a","generation":"4"},"operation":"anything"})),
        ("ai_cancel_proposal", json!({"requestId":1,"proposalId":"4"})),
        ("ai_decide", json!({"proposalId":"1","decision":"approve","document":4})),
        ("ai_decide", json!({"proposalId":"1","decision":"approve","target":{"targetId":"a","generation":"4"}})),
    ] {
        assert!(parse(command, &InvokeBody::Json(body)).is_err(), "{command}");
    }
    assert!(parse("ai_decide", &InvokeBody::Raw(vec![])).is_err());
    assert!(LocalReview::check("main", "main", false, 4).is_err());
    assert!(LocalReview::check("other", "main", true, 4).is_err());
}

#[test]
fn stored_generation_and_document_gate_late_results_and_decisions() {
    let root = TestDir::new();
    let mut profile = draft(root.path(), "work", "work-a");
    let file = root.path().join("connections.json");
    let write_store = |draft: &Value, revision| {
        fs::write(&file, json!({
            "version": 1,
            "groups": [{"revision": 1,"profile": draft["group"]}],
            "targets": [{"revision": revision,"profile": draft["target"]}],
        }).to_string()).unwrap();
    };
    const REVISION: u64 = 9_007_199_254_740_993;
    write_store(&profile, REVISION);
    let read = || connections::with_ai_targets(root.path(), |targets| targets).unwrap();
    let targets = read();
    let target = &targets[0];
    let local = LocalReview::check("main", "main", true, 4).unwrap();
    let mut state = ReviewState::default();
    state.select(4, Some(target.clone())).unwrap();
    assert!(state.begin(5, &targets, 1, target.into()).is_err());
    let (epoch, selected) = state.begin(4, &targets, 1, target.into()).unwrap();
    let candidate = || review_proposal(r#"{"operation":"uptime","arguments":{}}"#, selected.clone()).unwrap();
    let card = state.finish(4, &local, &targets, 1, epoch, candidate()).unwrap();
    let wire = serde_json::to_value(&card).unwrap();
    assert_eq!(wire["target"], json!({"targetId":"work-a", "generation":"9007199254740993"}));
    assert_eq!(wire["node"], "내 PC");
    assert_eq!(wire["displayCommand"], "uptime");
    assert!(!wire.to_string().contains("startDirectory"));
    let decision = |choice| Decision { proposal_id: card.id.clone(), decision: choice };
    assert_eq!(state.decide(4, &local, &targets, decision(PanelChoice::Cancel)), Ok(RecordedDecision::Cancelled));
    assert_eq!(state.decide(4, &local, &targets, decision(PanelChoice::Approve)), Ok(RecordedDecision::Cancelled));
    state.begin(4, &targets, 2, target.into()).unwrap();
    let card = state.finish(4, &local, &targets, 2, epoch, candidate()).unwrap();
    profile["target"]["displayName"] = json!("같은 ID의 새 대상");
    write_store(&profile, REVISION + 1);
    let updated = read();
    assert!(state.decide(4, &local, &updated, Decision { proposal_id: card.id, decision: PanelChoice::Approve }).is_err());
    state.begin(4, &targets, 3, target.into()).unwrap();
    assert!(state.finish(4, &local, &updated, 3, epoch, candidate()).is_err());
    state.begin(4, &targets, 4, target.into()).unwrap();
    state.select(4, None).unwrap();
    state.select(4, Some(target.clone())).unwrap();
    assert!(state.finish(4, &local, &targets, 4, epoch, candidate()).is_err());
}

#[test]
fn cli_receipt_requires_saved_revision_and_preserves_the_actual_ssm_scope() {
    let root = TestDir::new();
    let ssh = connections::tests::ssh_draft(root.path());
    let draft = draft(root.path(), "work", "local-a");
    fs::write(root.path().join("connections.json"), json!({
        "version": 1,
        "groups": [{"revision": 1,"profile": draft["group"]}],
        "targets": [
            {"revision": 1,"profile": draft["target"]},
            {"revision": 4,"profile": {"id":"ssm-a", "groupId":"work", "displayName":"원격 시험",
                "defaultMethod":"ssm", "ssm":{"profile":"ops-a", "region":"ap-northeast-2", "instanceId":"i-0123456789abcdef0"}}},
            {"revision": 4,"profile": {"id":"ssm-b", "groupId":"work", "displayName":"원격 시험",
                "defaultMethod":"ssm", "ssm":{"profile":"ops-b", "region":"us-west-2", "instanceId":"i-0123456789abcdef0"}}},
            {"revision": 5,"profile": ssh["target"]}
        ]
    }).to_string()).unwrap();
    let aliases = serde_json::to_value(connections::ai_aliases(root.path()).unwrap()).unwrap();
    assert_eq!(aliases.as_array().unwrap().len(), 3);
    assert_eq!(aliases[1], json!({"id":"ssm-a", "method":"ssm", "displayName":"원격 시험", "generation":"4"}));
    assert!(!aliases.to_string().contains("ops-a"));
    assert!(!aliases.to_string().contains("i-0123456789abcdef0"));
    let targets = connections::with_ai_targets(root.path(), |targets| targets).unwrap();
    assert_eq!(targets.len(), 3);
    let state = State::default();
    let request = |generation: &str| CliProposal { target_id: "ssm-a".into(), generation: generation.into(), operation: Query::Uptime };
    let deadline = Instant::now() + Duration::from_secs(3);
    assert!(state.receive_proposal(&targets, request("4"), Instant::now()).is_err());
    assert!(state.receive_proposal(&targets, request("3"), deadline).is_err());
    let id = state.receive_proposal(&targets, request("4"), deadline).unwrap();
    assert_eq!(state.receive_proposal(&targets, request("4"), deadline).unwrap(), id);
    let other = CliProposal { target_id: "ssm-b".into(), ..request("4") };
    let other_id = state.receive_proposal(&targets, other, deadline).unwrap();
    let local = LocalReview::check("main", "main", true, 7).unwrap();
    let mut state = state.0.lock().unwrap();
    state.select(7, Some(targets[0].clone())).unwrap();
    assert!(state.open_cli(7, &local, &targets).unwrap().is_none());
    state.select(7, Some(targets[1].clone())).unwrap();
    let decision = || Decision { proposal_id: id.clone(), decision: PanelChoice::Approve };
    assert!(state.set_policy(false).is_ok());
    let card = state.open_cli(7, &local, &targets).unwrap().unwrap();
    assert_eq!(card.id, id);
    let card = serde_json::to_value(card).unwrap();
    assert_eq!(card["node"], "i-0123456789abcdef0");
    assert_eq!(card["ssm"], json!({"profile":"ops-a", "region":"ap-northeast-2"}));
    assert_eq!(state.decide(7, &local, &targets, decision()), Ok(RecordedDecision::Approved));
    assert_eq!(state.decide(7, &local, &targets, decision()), Ok(RecordedDecision::Approved));
    assert!(state.open_cli(7, &local, &targets).unwrap().is_none());
    state.select(7, Some(targets[2].clone())).unwrap();
    let card = serde_json::to_value(state.open_cli(7, &local, &targets).unwrap().unwrap()).unwrap();
    assert_eq!(card["id"], other_id);
    assert_eq!(card["node"], "i-0123456789abcdef0");
    assert_eq!(card["ssm"], json!({"profile":"ops-b", "region":"us-west-2"}));
}

#[test]
fn policy_changes_invalidate_open_cards_and_replay_uses_a_new_decision() {
    let host = State::default();
    let target = SelectedTarget { profile_id: "dev".into(), ssm_profile: Some("ops".into()), region: "ap-northeast-2".into(),
        node_id: "fixture".into(), display_name: "시험".into(), generation: NonZeroU64::MIN };
    let targets = [target.clone()];
    let local = LocalReview::check("main", "main", true, 2).unwrap();
    let request = || CliProposal { target_id: "dev".into(), generation: "1".into(), operation: Query::Uptime };
    let deadline = Instant::now() + Duration::from_secs(3);
    let first = host.receive_proposal(&targets, request(), deadline).unwrap();
    {
        let mut state = host.0.lock().unwrap();
        state.select(2, Some(target)).unwrap();
        assert_eq!(state.open_cli(2, &local, &targets).unwrap().unwrap().id, first);
    }
    assert_eq!(host.receive_proposal(&targets, request(), deadline).unwrap(), first);
    {
        let mut state = host.0.lock().unwrap();
        state.set_policy(true).unwrap();
        assert!(state.decide(2, &local, &targets, Decision { proposal_id: first.clone(), decision: PanelChoice::Approve }).is_err());
    }
    let next = host.receive_proposal(&targets, request(), deadline).unwrap();
    assert_ne!(first, next);
    let mut state = host.0.lock().unwrap();
    let card = state.open_cli(2, &local, &targets).unwrap().unwrap();
    assert_eq!(serde_json::to_value(card).unwrap()["kind"], "blocked");
    let reject = || Decision { proposal_id: next.clone(), decision: PanelChoice::Reject };
    assert_eq!(state.decide(2, &local, &targets, reject()), Ok(RecordedDecision::Rejected));
    assert_eq!(state.decide(2, &local, &targets, reject()), Ok(RecordedDecision::Rejected));
}

#[test]
fn cancelling_either_side_of_model_completion_keeps_shared_capacity_available() {
    let host = State::default();
    let target = SelectedTarget { profile_id: "dev".into(), ssm_profile: None, region: String::new(),
        node_id: "내 PC".into(), display_name: "시험".into(), generation: NonZeroU64::MIN };
    let targets = [target.clone()];
    let local = LocalReview::check("main", "main", true, 2).unwrap();
    let candidate = || review_proposal(r#"{"operation":"uptime","arguments":{}}"#, target.clone()).unwrap();
    {
        let mut state = host.0.lock().unwrap();
        state.select(2, Some(target.clone())).unwrap();
        state.cancel(2, &local, 1);
        assert_eq!(state.begin(2, &targets, 1, (&target).into()), Err(CANCELLED.into()));
        for request_id in 2..66 {
            let (epoch, _) = state.begin(2, &targets, request_id, (&target).into()).unwrap();
            if request_id % 2 == 0 {
                state.cancel(2, &local, request_id);
                assert_eq!(state.finish(2, &local, &targets, request_id, epoch, candidate()).err().as_deref(), Some(CANCELLED));
            } else {
                let card = state.finish(2, &local, &targets, request_id, epoch, candidate()).unwrap();
                state.cancel(2, &local, request_id);
                assert!(state.decide(2, &local, &targets, Decision { proposal_id: card.id, decision: PanelChoice::Approve }).is_err());
            }
        }
        let (epoch, _) = state.begin(2, &targets, 66, (&target).into()).unwrap();
        state.cancel(2, &local, 65);
        assert!(state.finish(2, &local, &targets, 66, epoch, candidate()).is_ok());
    }
    let request = CliProposal { target_id: "dev".into(), generation: "1".into(), operation: Query::Uptime };
    assert!(host.receive_proposal(&targets, request, Instant::now() + Duration::from_secs(3)).is_ok());
}

#[test]
fn draft_retransmissions_do_not_extend_expiry_or_open_twice() {
    let dir = TestDir::new();
    let make = || serde_json::from_value::<connections::profile::Draft>(draft(dir.path(), "work", "a")).unwrap().validate().unwrap();
    let host = State::default();
    let deadline = Instant::now() + Duration::from_secs(3);
    assert!(host.receive_draft(make(), Instant::now()).is_err());
    let id = host.receive_draft(make(), deadline).unwrap();
    let expiry = host.0.lock().unwrap().draft.as_ref().unwrap().expires_at;
    assert_eq!(host.receive_draft(make(), deadline).unwrap(), id);
    {
        let mut state = host.0.lock().unwrap();
        assert_eq!(state.draft.as_ref().unwrap().expires_at, expiry);
        assert_eq!(state.open_draft().unwrap()["draftId"], id);
        assert!(state.open_draft().unwrap().is_null());
    }
    assert_eq!(host.receive_draft(make(), deadline).unwrap(), id);
    host.0.lock().unwrap().draft.as_mut().unwrap().expires_at = Instant::now();
    assert_ne!(host.receive_draft(make(), deadline).unwrap(), id);
}
