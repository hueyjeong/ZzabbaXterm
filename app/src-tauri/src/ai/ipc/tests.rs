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
        ("ai_propose", json!({"model":"fixture","target":{"targetId":"a","generation":"4"},"operation":"anything"})),
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
    assert!(state.begin(5, &targets, target.into()).is_err());
    let (epoch, selected) = state.begin(4, &targets, target.into()).unwrap();
    let candidate = || review_proposal(r#"{"operation":"uptime","arguments":{}}"#, selected.clone()).unwrap();
    let card = state.finish(4, &local, &targets, epoch, candidate()).unwrap();
    let wire = serde_json::to_value(&card).unwrap();
    assert_eq!(wire["target"], json!({"targetId":"work-a", "generation":"9007199254740993"}));
    assert_eq!(wire["node"], "내 PC");
    assert_eq!(wire["displayCommand"], "uptime");
    assert!(!wire.to_string().contains("startDirectory"));
    let decision = |choice| Decision { proposal_id: card.id.clone(), decision: choice };
    state.decide(4, &local, &targets, decision(PanelChoice::Cancel)).unwrap();
    assert!(state.decide(4, &local, &targets, decision(PanelChoice::Approve)).is_err());
    let card = state.finish(4, &local, &targets, epoch, candidate()).unwrap();
    profile["target"]["displayName"] = json!("같은 ID의 새 대상");
    write_store(&profile, REVISION + 1);
    let updated = read();
    assert!(state.decide(4, &local, &updated, Decision { proposal_id: card.id, decision: PanelChoice::Approve }).is_err());
    assert!(state.finish(4, &local, &updated, epoch, candidate()).is_err());
    state.select(4, None).unwrap();
    state.select(4, Some(target.clone())).unwrap();
    assert!(state.finish(4, &local, &targets, epoch, candidate()).is_err());
}

#[test]
fn cli_receipt_requires_saved_revision_and_a_one_shot_local_review() {
    let root = TestDir::new();
    let draft = draft(root.path(), "work", "local-a");
    fs::write(root.path().join("connections.json"), json!({
        "version": 1,
        "groups": [{"revision": 1,"profile": draft["group"]}],
        "targets": [
            {"revision": 1,"profile": draft["target"]},
            {"revision": 4,"profile": {"id":"ssm-a", "groupId":"work", "displayName":"원격 시험",
                "defaultMethod":"ssm", "ssm":{"profile":"fixture-secret", "region":"ap-northeast-2", "instanceId":"i-0123456789abcdef0"}}}
        ]
    }).to_string()).unwrap();
    let aliases = serde_json::to_value(connections::ai_aliases(root.path()).unwrap()).unwrap();
    assert_eq!(aliases[1], json!({"id":"ssm-a", "method":"ssm", "displayName":"원격 시험", "generation":"4"}));
    assert!(!aliases.to_string().contains("fixture-secret"));
    assert!(!aliases.to_string().contains("i-0123456789abcdef0"));
    let targets = connections::with_ai_targets(root.path(), |targets| targets).unwrap();
    let state = State::default();
    let request = |generation: &str| CliProposal { target_id: "ssm-a".into(), generation: generation.into(), operation: Query::Uptime };
    let deadline = Instant::now() + Duration::from_secs(3);
    assert!(state.receive_proposal(&targets, request("4"), Instant::now()).is_err());
    assert!(state.receive_proposal(&targets, request("3"), deadline).is_err());
    let id = state.receive_proposal(&targets, request("4"), deadline).unwrap();
    assert_eq!(state.receive_proposal(&targets, request("4"), deadline).unwrap(), id);
    let local = LocalReview::check("main", "main", true, 7).unwrap();
    let mut state = state.0.lock().unwrap();
    state.select(7, Some(targets[0].clone())).unwrap();
    assert!(state.open_cli(7, &local, &targets).unwrap().is_none());
    state.select(7, Some(targets[1].clone())).unwrap();
    let decision = || Decision { proposal_id: id.clone(), decision: PanelChoice::Approve };
    assert!(state.set_policy(false).is_ok());
    let card = state.open_cli(7, &local, &targets).unwrap().unwrap();
    assert_eq!(card.id, id);
    assert_eq!(serde_json::to_value(card).unwrap()["node"], "i-0123456789abcdef0");
    state.decide(7, &local, &targets, decision()).unwrap();
    assert!(state.decide(7, &local, &targets, decision()).is_err());
    assert!(state.open_cli(7, &local, &targets).unwrap().is_none());
}

#[test]
fn policy_changes_invalidate_open_cards_and_replay_uses_a_new_decision() {
    let host = State::default();
    let target = SelectedTarget { profile_id: "dev".into(), region: "ap-northeast-2".into(),
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
    state.decide(2, &local, &targets, reject()).unwrap();
    assert!(state.decide(2, &local, &targets, reject()).is_err());
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
