use super::*;
use serde_json::json;

fn document(key: &str) -> Document {
    Document {
        key: key.into(),
        requests: HashMap::new(),
    }
}

fn nodes() -> Operation {
    Operation::ListOnlineNodes {
        profile: "test-sso".into(),
        region: "eu-west-1".into(),
    }
}

#[test]
fn wire_contract_accepts_only_discovery_and_masks_failures() {
    let request = json!({"document":"generation", "request":{
        "action":"run", "requestId":"17", "operation":{
            "kind":"listOnlineNodes", "profile":"test-sso", "region":"eu-west-1"
        }
    }});
    let parsed = Request::deserialize(&request).unwrap();
    assert_eq!(parsed.document, "generation");
    assert!(
        matches!(parsed.request, Action::Run { request_id: "17", operation:
        Operation::ListOnlineNodes { profile, region } } if profile == "test-sso" && region == "eu-west-1")
    );
    for request in [
        json!({"action":"relogin", "requestId":"17", "cause":"SsoLoginRequired"}),
        json!({"action":"relogin", "requestId":"17", "profile":"different"}),
        json!({"action":"run", "requestId":"17", "operation":{"kind":"startSession"}}),
        json!({"action":"cancel", "requestId":17}),
    ] {
        assert!(
            Request::deserialize(&json!({"document":"generation", "request":request})).is_err()
        );
    }
    for (error, code) in [
        (
            CliError::CommandFailed {
                exit_code: Some(91),
            },
            "Unknown",
        ),
        (CliError::AccessDenied, "AccessDenied"),
        (CliError::CredentialsExpired, "CredentialsExpired"),
        (CliError::SsoLoginRequired, "SsoLoginRequired"),
    ] {
        assert_eq!(
            serde_json::to_value(Failure { code: error.into() }).unwrap(),
            json!({"code":code})
        );
    }
    assert_eq!(
        serde_json::to_value(Value::Region(None)).unwrap(),
        json!(null)
    );
    assert_eq!(
        serde_json::to_value(Value::Nodes(vec![])).unwrap(),
        json!([])
    );
    assert_eq!(
        serde_json::to_value(Value::Nodes(vec![Node {
            instance_id: "i-0123456789abcdef0".into()
        }]))
        .unwrap(),
        json!([{"instanceId":"i-0123456789abcdef0"}])
    );
}

#[test]
fn cancellation_completion_navigation_and_close_are_owner_scoped() {
    let mut state = Requests::default();
    state.documents.insert("a".into(), document("old"));
    state.documents.insert("b".into(), document("other"));
    let a = state.reserve("a", "old").unwrap();
    let b = state.reserve("b", "other").unwrap();
    let sibling = state.reserve("a", "old").unwrap();
    let (_, a_cancel) = state.start("a", "old", a, Some(nodes())).unwrap();
    let (_, b_cancel) = state.start("b", "other", b, Some(nodes())).unwrap();
    let (_, sibling_cancel) = state.start("a", "old", sibling, Some(nodes())).unwrap();

    state.cancel("b", "other", a).unwrap();
    assert!(!a_cancel.is_cancelled());
    assert_eq!(state.finish("b", "other", a, None), Err(Code::Cancelled));
    state.finish("a", "old", sibling, None).unwrap();
    assert!(!a_cancel.is_cancelled() && !b_cancel.is_cancelled() && !sibling_cancel.is_cancelled());
    assert!(matches!(
        state.start("a", "old", sibling, Some(nodes())),
        Err(Code::Cancelled)
    ));

    let reserved = state.reserve("a", "old").unwrap();
    state.cancel("a", "old", reserved).unwrap();
    assert!(matches!(
        state.start("a", "old", reserved, Some(nodes())),
        Err(Code::Cancelled)
    ));
    state.documents.insert("a".into(), document("new"));
    assert!(a_cancel.is_cancelled());
    assert_eq!(state.finish("a", "old", a, None), Err(Code::Cancelled));
    let new_id = state.reserve("a", "new").unwrap();
    let (_, new_cancel) = state.start("a", "new", new_id, Some(nodes())).unwrap();
    assert_eq!(state.cancel("a", "old", new_id), Err(Code::Cancelled));
    assert!(!new_cancel.is_cancelled());
    state.documents.remove("a");
    assert!(new_cancel.is_cancelled() && !b_cancel.is_cancelled());
    assert_eq!(state.finish("a", "new", new_id, None), Err(Code::Cancelled));
    state.finish("b", "other", b, None).unwrap();
}

#[test]
fn relogin_requires_and_consumes_the_exact_expired_request() {
    let mut state = Requests::default();
    state.documents.insert("a".into(), document("current"));
    let expired = state.reserve("a", "current").unwrap();
    let denied = state.reserve("a", "current").unwrap();
    assert!(matches!(
        state.start("a", "current", expired, None),
        Err(Code::SsoLoginNotApplicable)
    ));
    state.start("a", "current", expired, Some(nodes())).unwrap();
    state.start("a", "current", denied, Some(nodes())).unwrap();
    assert!(matches!(
        state.start("a", "current", expired, None),
        Err(Code::SsoLoginNotApplicable)
    ));
    state.finish("a", "current", denied, None).unwrap();
    assert!(matches!(
        state.start("a", "current", denied, None),
        Err(Code::Cancelled)
    ));

    let scope = Scope::new("test-sso".into(), "eu-west-1".into()).unwrap();
    state
        .finish("a", "current", expired, Some(scope.clone()))
        .unwrap();
    let (work, login_cancel) = state.start("a", "current", expired, None).unwrap();
    assert!(matches!(work, Work::Relogin(actual) if actual == scope));
    assert!(matches!(
        state.start("a", "current", expired, None),
        Err(Code::SsoLoginNotApplicable)
    ));
    state.cancel("a", "current", expired).unwrap();
    assert!(login_cancel.is_cancelled());
    assert_eq!(
        state.finish("a", "current", expired, None),
        Err(Code::Cancelled)
    );
}
