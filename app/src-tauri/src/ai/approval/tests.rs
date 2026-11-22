use super::*;
use crate::ai::review_proposal;

fn chat(target: &SelectedTarget) -> Candidate<'static> {
    Candidate::Chat(
        review_proposal(r#"{"operation":"uptime","arguments":{}}"#, target.clone()).unwrap(),
    )
}

fn target() -> SelectedTarget {
    SelectedTarget {
        profile_id: "dev-a".into(),
        region: "ap-northeast-2".into(),
        node_id: "node-a".into(),
        display_name: "개발 노드".into(),
        generation: NonZeroU64::MIN,
    }
}

fn policy() -> Policy {
    Policy {
        revision: NonZeroU64::MIN,
        blocked: false,
    }
}

fn local() -> LocalReview {
    LocalReview::check("main", "main", true, 1).unwrap()
}

fn propose_and_open(reviews: &mut Reviews, target: &SelectedTarget, policy: Policy) -> u32 {
    let id = reviews.propose(chat(target), Some(target), policy).unwrap();
    reviews.open(&local(), id, Some(target), policy).unwrap();
    id
}

fn request(proposal_id: u32, choice: Choice) -> DecisionRequest {
    DecisionRequest {
        proposal_id,
        choice,
    }
}

#[test]
fn chat_and_cli_show_the_same_registered_query_on_the_ui_selection() {
    let mut reviews = Reviews::default();
    let target = target();
    let cli = Candidate::Cli {
        target_id: "dev-a",
        operation: "uptime",
    };
    assert_eq!(
        reviews.propose(chat(&target), None, policy()),
        Err(ReviewError::TargetUnavailable)
    );
    assert_eq!(
        reviews.propose(cli, None, policy()),
        Err(ReviewError::TargetUnavailable)
    );
    assert_eq!(
        reviews.propose(
            Candidate::Cli {
                target_id: "missing-node",
                operation: "uptime"
            },
            Some(&target),
            policy()
        ),
        Err(ReviewError::TargetMismatch)
    );

    let chat = propose_and_open(&mut reviews, &target, policy());
    let cli = reviews
        .propose(
            Candidate::Cli {
                target_id: "dev-a",
                operation: "uptime",
            },
            Some(&target),
            policy(),
        )
        .unwrap();
    for (id, source) in [(chat, Source::Chat), (cli, Source::Cli)] {
        let card = reviews.open(&local(), id, Some(&target), policy()).unwrap();
        assert_eq!(card.source, source);
        assert_eq!(card.target, target);
        assert_eq!(card.command, Some("uptime"));
        assert_eq!(
            card.explanation,
            "선택한 노드의 가동 시간과 부하를 조회합니다"
        );
        let record = reviews
            .decide(
                &local(),
                request(id, Choice::Approve),
                Some(&target),
                policy(),
            )
            .unwrap();
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["decision"], "approved");
        assert_eq!(json["card"]["target"]["nodeId"], "node-a");
        assert!(json["card"]["target"].get("generation").is_none());
        assert_eq!(
            reviews.decide(
                &local(),
                request(id, Choice::Approve),
                Some(&target),
                policy()
            ),
            Err(ReviewError::Missing)
        );
    }
}

#[test]
fn blocked_policy_precedes_the_registered_query_and_other_input_is_display_only() {
    let mut reviews = Reviews::default();
    let target = target();
    for (text, policy, expected) in [
        (
            "uptime",
            Policy {
                blocked: true,
                ..policy()
            },
            Operation::Blocked {
                text: "uptime".into(),
            },
        ),
        ("who", policy(), Operation::RawReview { text: "who".into() }),
        (
            "uptime ",
            policy(),
            Operation::RawReview {
                text: "uptime ".into(),
            },
        ),
        (
            "uptime -p",
            policy(),
            Operation::RawReview {
                text: "uptime -p".into(),
            },
        ),
        (
            "uptime;who\n\u{202e}",
            policy(),
            Operation::RawReview {
                text: r"uptime;who\n\u{202e}".into(),
            },
        ),
    ] {
        let id = reviews
            .propose(
                Candidate::Cli {
                    target_id: &target.profile_id,
                    operation: text,
                },
                Some(&target),
                policy,
            )
            .unwrap();
        let card = reviews.open(&local(), id, Some(&target), policy).unwrap();
        assert_eq!(card.operation, expected);
        assert_eq!(card.command, None);
        assert_eq!(
            reviews.decide(
                &local(),
                request(id, Choice::Approve),
                Some(&target),
                policy
            ),
            Err(ReviewError::NotApprovable)
        );
    }
}

#[test]
fn main_local_review_and_strict_payload_are_required() {
    assert!(matches!(
        LocalReview::check("main", "main", false, 1),
        Err(ReviewError::NotLocalMain)
    ));
    assert!(matches!(
        LocalReview::check("other", "main", true, 1),
        Err(ReviewError::NotLocalMain)
    ));
    assert!(matches!(
        LocalReview::check("main", "other", true, 1),
        Err(ReviewError::NotLocalMain)
    ));
    for field in [
        "targetId",
        "nodeId",
        "generation",
        "operation",
        "policy",
        "mode",
        "approved",
        "args",
        "source",
    ] {
        let mut value = serde_json::json!({ "proposalId": 1, "choice": "approve" });
        value[field] = serde_json::json!("forged");
        assert!(
            serde_json::from_value::<DecisionRequest>(value).is_err(),
            "{field}"
        );
    }
    let mut reviews = Reviews::default();
    let target = target();
    let id = reviews
        .propose(chat(&target), Some(&target), policy())
        .unwrap();
    let decision =
        serde_json::from_value(serde_json::json!({ "proposalId": id, "choice": "approve" }))
            .unwrap();
    assert_eq!(
        reviews.decide(&local(), decision, Some(&target), policy()),
        Err(ReviewError::NotReviewed)
    );
    assert!(matches!(
        reviews.open(&local(), id, Some(&target), policy()),
        Err(ReviewError::Missing)
    ));
}

#[test]
fn changed_target_generation_policy_or_document_discards_the_review() {
    let mut reviews = Reviews::default();
    let target = target();
    let replacement = SelectedTarget {
        node_id: "node-b".into(),
        ..target.clone()
    };
    let reconnect = SelectedTarget {
        generation: NonZeroU64::new(2).unwrap(),
        ..target.clone()
    };
    let revised = Policy {
        revision: NonZeroU64::new(2).unwrap(),
        ..policy()
    };
    let reloaded = LocalReview::check("main", "main", true, 2).unwrap();
    for (selected, current_policy, local) in [
        (None, policy(), &local()),
        (Some(&replacement), policy(), &local()),
        (Some(&reconnect), policy(), &local()),
        (Some(&target), revised, &local()),
        (
            Some(&target),
            Policy {
                blocked: true,
                ..policy()
            },
            &local(),
        ),
        (Some(&target), policy(), &reloaded),
    ] {
        let id = propose_and_open(&mut reviews, &target, policy());
        assert_eq!(
            reviews.decide(
                local,
                request(id, Choice::Approve),
                selected,
                current_policy
            ),
            Err(ReviewError::Stale)
        );
        assert!(matches!(
            reviews.open(local, id, Some(&target), policy()),
            Err(ReviewError::Missing)
        ));
    }
}

#[test]
fn cancellation_rejection_and_expiry_are_final() {
    let mut reviews = Reviews::default();
    let target = target();
    for (choice, decision) in [
        (Choice::Cancel, Decision::Cancelled),
        (Choice::Reject, Decision::Rejected),
    ] {
        let id = propose_and_open(&mut reviews, &target, policy());
        let record = reviews
            .decide(&local(), request(id, choice), Some(&target), policy())
            .unwrap();
        assert_eq!(record.decision, decision);
        assert_eq!(
            reviews.decide(
                &local(),
                request(id, Choice::Approve),
                Some(&target),
                policy()
            ),
            Err(ReviewError::Missing)
        );
    }
    let expired = propose_and_open(&mut reviews, &target, policy());
    reviews.pending.get_mut(&expired).unwrap().expires_at = Instant::now();
    assert_eq!(
        reviews.decide(
            &local(),
            request(expired, Choice::Approve),
            Some(&target),
            policy()
        ),
        Err(ReviewError::Expired)
    );
    assert!(matches!(
        reviews.open(&local(), expired, Some(&target), policy()),
        Err(ReviewError::Missing)
    ));
}
