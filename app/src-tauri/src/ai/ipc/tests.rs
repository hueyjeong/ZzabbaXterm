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
