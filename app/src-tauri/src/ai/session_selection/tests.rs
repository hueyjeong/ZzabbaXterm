use super::*;
use serde_json::{Value, json};

#[test]
fn requests_require_exact_json_objects_and_string_ids() {
    let read = InvokeBody::Json(json!({}));
    assert_eq!(parse(READ, &read), Ok(Request::Read));
    let select = InvokeBody::Json(json!({"id": "0"}));
    assert_eq!(parse(SELECT, &select), Ok(Request::Select { id: "0" }));
    let large_id = "9007199254740993";
    let confirm = InvokeBody::Json(json!({"id": "0", "selectionId": large_id}));
    let selection_id = NonZeroU64::new(9_007_199_254_740_993).unwrap();
    assert_eq!(parse(CONFIRM, &confirm), Ok(Request::Confirm { id: "0", selection_id }));
    let clear = InvokeBody::Json(json!({"selectionId": large_id}));
    assert_eq!(parse(CLEAR, &clear), Ok(Request::Clear { selection_id }));
    for (command, body) in [
        (READ, json!(null)),
        (READ, json!({"document": 0})),
        (SELECT, json!(["0"])),
        (SELECT, json!({"id": 0})),
        (SELECT, json!({"id": "0", "accountId": "000000000000"})),
        (CONFIRM, json!({"id": "0", "selectionId": "1", "approvalChannel": "fake"})),
        (CONFIRM, json!({"id": "0", "selectionId": 1})),
        (CLEAR, json!({"selectionId": "1", "owner": "main"})),
        (CLEAR, json!({"selectionId": "0"})),
        (CLEAR, json!({"selectionId": "+1"})),
        (CLEAR, json!({"selectionId": "01"})),
    ] {
        assert_eq!(parse(command, &InvokeBody::Json(body)), Err(INVALID));
    }
    assert_eq!(parse(READ, &InvokeBody::Raw(vec![])), Err(INVALID));
}

#[test]
fn selection_confirms_one_live_session_and_rejects_old_replies() {
    let state = State::default();
    let sessions = Sessions::default();
    let document = sessions.document(OWNER);
    let a = sessions.insert_live_ssm_fixture(OWNER, document, "123456789012", "us-west-2", "i-01234567");
    let b = sessions.insert_live_ssm_fixture(OWNER, document, "123456789012", "us-west-2", "i-01234567");
    let foreign = sessions.insert_live_ssm_fixture("other", document, "123456789012", "us-west-2", "i-01234567");
    let a_id = a.identity.id.clone();
    let b_id = &b.identity.id;
    let request = |command, body| {
        let body = InvokeBody::Json(body);
        state.handle(&sessions, document, parse(command, &body).unwrap())
            .map(|response| response.deserialize::<Value>().unwrap())
    };
    let view = |id: &str, selection_id: &str, confirmed| {
        json!({"id": id, "region": "us-west-2", "node": "i-01234567", "selectionId": selection_id, "confirmed": confirmed})
    };

    assert_eq!(request(READ, json!({})), Ok(Value::Null));
    for id in ["saved-profile", foreign.identity.id.as_str()] {
        assert_eq!(request(SELECT, json!({"id": id})), Err(UNAVAILABLE.into()));
    }
    assert_eq!(request(SELECT, json!({"id": a_id})), Ok(view(&a_id, "1", false)));
    assert_eq!(request(CONFIRM, json!({"id": b_id, "selectionId": "1"})), Err(STALE.into()));
    let confirm_a = || request(CONFIRM, json!({"id": a_id, "selectionId": "1"}));
    assert_eq!(confirm_a(), Ok(view(&a_id, "1", true)));
    assert_eq!(confirm_a(), Ok(view(&a_id, "1", true)));

    sessions.navigate("other");
    state.document_changed("other", sessions.document("other"));
    assert_eq!(request(READ, json!({})), Ok(view(&a_id, "1", true)));
    assert_eq!(request(SELECT, json!({"id": b_id})), Ok(view(b_id, "2", false)));
    assert_eq!(request(SELECT, json!({"id": a_id})), Ok(view(&a_id, "3", false)));
    assert_eq!(confirm_a(), Err(STALE.into()));
    assert_eq!(request(CLEAR, json!({"selectionId": "1"})), Err(STALE.into()));
    assert_eq!(request(READ, json!({})), Ok(view(&a_id, "3", false)));

    let confirm_current = || request(CONFIRM, json!({"id": a_id, "selectionId": "3"}));
    assert_eq!(confirm_current(), Ok(view(&a_id, "3", true)));
    drop(a);
    assert_eq!(request(READ, json!({})), Err(UNAVAILABLE.into()));
    assert_eq!(confirm_current(), Err(UNAVAILABLE.into()));
    assert_eq!(request(CLEAR, json!({"selectionId": "3"})), Ok(Value::Null));
    assert_eq!(request(READ, json!({})), Ok(Value::Null));
    assert_eq!(confirm_current(), Err(STALE.into()));
}

#[test]
fn document_change_rejects_old_calls_and_releases_old_selection_only() {
    let state = State::default();
    let sessions = Sessions::default();
    let document = sessions.document(OWNER);
    let a = sessions.insert_live_ssm_fixture(OWNER, document, "123456789012", "us-west-2", "i-01234567");
    let a_id = a.identity.id.as_str();
    let select = |doc, id| state.handle(&sessions, doc, Request::Select { id }).unwrap();
    let first = select(document, a_id).deserialize::<Value>().unwrap();
    let selection_id = parse_selection_id(first["selectionId"].as_str().unwrap()).unwrap();

    sessions.navigate(OWNER);
    let current = sessions.document(OWNER);
    let confirm = || Request::Confirm { id: a_id, selection_id };
    for doc in [document, current] {
        assert!(state.handle(&sessions, doc, Request::Read).is_err());
        assert!(state.handle(&sessions, doc, confirm()).is_err());
        assert!(state.handle(&sessions, doc, Request::Clear { selection_id }).is_err());
    }
    state.document_changed(OWNER, current);
    assert!(state.handle(&sessions, document, Request::Read).is_err());
    assert_eq!(state.handle(&sessions, current, Request::Read).unwrap().deserialize::<Value>().unwrap(), Value::Null);
    assert!(state.handle(&sessions, current, confirm()).is_err());

    let b = sessions.insert_live_ssm_fixture(OWNER, current, "123456789012", "us-west-2", "i-01234567");
    let second = select(current, &b.identity.id).deserialize::<Value>().unwrap();
    assert_eq!(second["selectionId"], "2");
    state.document_changed(OWNER, current);
    assert_eq!(state.handle(&sessions, current, Request::Read).unwrap().deserialize::<Value>().unwrap(), second);
}

#[test]
fn dispatcher_requires_local_acl_and_main_window_and_webview() {
    use tauri::{WebviewWindowBuilder, ipc::CallbackFn, test, webview::InvokeRequest};

    let mut context = test::mock_context(test::noop_assets());
    context.runtime_authority_mut().__allow_command(READ.into(), ExecutionContext::Local);
    let remote = "https://fixture.invalid";
    context.runtime_authority_mut().__allow_command(SELECT.into(), ExecutionContext::Remote {
        url: remote.parse().unwrap(),
    });
    let app = test::mock_builder().manage(Sessions::default()).manage(State::default())
        .invoke_handler(|invoke| {
            assert!(handles(invoke.message.command()));
            dispatch(invoke);
            true
        })
        .build(context).unwrap();
    let main = WebviewWindowBuilder::new(&app, OWNER, Default::default()).build().unwrap();
    let other = WebviewWindowBuilder::new(&app, "other", Default::default()).build().unwrap();
    let local = if cfg!(windows) { "http://tauri.localhost" } else { "tauri://localhost" };
    let request = |cmd: &str, url: &str| InvokeRequest {
        cmd: cmd.into(), callback: CallbackFn(0), error: CallbackFn(1),
        url: url.parse().unwrap(),
        body: InvokeBody::Json(json!({})), headers: Default::default(), invoke_key: test::INVOKE_KEY.into(),
    };
    assert_eq!(test::get_ipc_response(&main, request(READ, local)).unwrap().deserialize::<Value>().unwrap(), Value::Null);
    assert_eq!(test::get_ipc_response(&other, request(READ, local)).unwrap_err(), LOCAL_ONLY);
    assert_eq!(test::get_ipc_response(&main, request(SELECT, remote)).unwrap_err(), LOCAL_ONLY);
    assert!(test::get_ipc_response(&main, request(CLEAR, local)).is_err());
    assert!(LocalReview::check(OWNER, "other", true, 0).is_err());
    assert!(LocalReview::check("other", OWNER, true, 0).is_err());
    assert!(LocalReview::check(OWNER, OWNER, false, 0).is_err());
}
