use super::*;
use crate::storage::tests::{client, replay, response};
use aws_sdk_s3::primitives::SdkBody;
use aws_smithy_runtime_api::client::orchestrator::HttpResponse;

const KEY: &str = "실험//../한글 +%2F.txt";
const BUCKET: &str = "selected-bucket";
const REGION: &str = "eu-west-1";

fn listed_row(state: &StorageIpc, document: &Document, session_id: &str) -> String {
    let mut registry = state.0.lock().unwrap();
    let tab = registry.tab(document, session_id).unwrap();
    let page = tab.edits.listed(Page {
        rows: vec![ObjectRow::Prefix { prefix: "실험/".into() }, ObjectRow::Object { key: KEY.into(), bytes: Some(4) }],
        next_token: None,
    }, BUCKET, OBJECT_REGION);
    assert_eq!(page.rows[0], ListedRow::Prefix { prefix: "실험/".into() });
    let ListedRow::Object { key, row_id, .. } = &page.rows[1] else { panic!("객체 행 필요") };
    assert_eq!(key, KEY);
    assert_eq!(tab.edits.rows.get(row_id).unwrap().key, KEY);
    row_id.clone()
}

#[test]
fn 편집_요청은_클라이언트가_제공한_대상_키를_받지_않는다() {
    assert!(serde_json::from_value::<EditOpenRequest>(serde_json::json!({
        "sessionId": "session", "rowId": "row", "key": "other.txt"
    })).is_err());
    assert!(serde_json::from_value::<EditSaveRequest>(serde_json::json!({
        "sessionId": "session", "handle": "editor", "text": "changed", "region": "eu-west-3"
    })).is_err());
}

const OBJECT_REGION: &str = "eu-west-2";

fn snapshot(version: &str, text: &'static str) -> [HttpResponse; 6] {
    let headers = |name: &str, body: &'static str| {
        http::Response::builder().status(200).header(name, version)
            .body(SdkBody::from(body)).unwrap().try_into().unwrap()
    };
    [
        response(200, "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"),
        response(200, "<OwnershipControls><Rule><ObjectOwnership>BucketOwnerEnforced</ObjectOwnership></Rule></OwnershipControls>"),
        response(404, "<Error><Code>ObjectLockConfigurationNotFoundError</Code></Error>"),
        http::Response::builder().status(200)
            .header("etag", format!("\"{version}\""))
            .header("content-type", "text/plain")
            .header("content-length", text.len().to_string())
            .header("x-amz-version-id", version)
            .header("x-amz-server-side-encryption", "AES256")
            .body(SdkBody::from(text)).unwrap().try_into().unwrap(),
        headers("x-amz-version-id", "<Tagging><TagSet/></Tagging>"),
        headers("x-amz-object-version-id", "<ListObjectAnnotationsOutput><AnnotationCount>0</AnnotationCount></ListObjectAnnotationsOutput>"),
    ]
}

#[tokio::test]
async fn 객체_행은_문서와_세션에_묶이고_다음_목록에서_폐기된다() {
    let fixture = replay([]);
    let state = StorageIpc::default();
    let document = state.document("main");
    let other = state.document("other");
    let session = state.insert(document.clone(), "profile".into(), REGION.into(), client(&fixture)).unwrap();
    let other_session = state.insert(document.clone(), "profile".into(), REGION.into(), client(&fixture)).unwrap();
    let row_id = listed_row(&state, &document, &session.session_id);
    let open = |id: &str| EditOpenRequest { session_id: id.into(), row_id: row_id.clone() };
    assert!(matches!(state.edit_open(&other, open(&session.session_id)).await, OpenResult::Error(EditResult::Disconnected { .. })));
    assert!(matches!(state.edit_open(&document, open(&other_session.session_id)).await, OpenResult::Error(EditResult::Denied { .. })));
    state.0.lock().unwrap().tab(&document, &session.session_id).unwrap().edits.clear_rows();
    assert!(matches!(state.edit_open(&document, open(&session.session_id)).await, OpenResult::Error(EditResult::Denied { .. })));
    assert_eq!(fixture.actual_requests().count(), 0);
}

#[tokio::test]
async fn 비교는_기준을_유지하고_결과_미확인_후에는_다시_읽기만_저장을_해제한다() {
    let fixture = replay(snapshot("v1", "base").into_iter()
        .chain(snapshot("v2", "peer")).chain(snapshot("v2", "peer")).chain(snapshot("v2", "peer")));
    let state = StorageIpc::default();
    let document = state.document("main");
    let session = state.insert(document.clone(), "profile".into(), REGION.into(), client(&fixture)).unwrap();
    let row_id = listed_row(&state, &document, &session.session_id);
    let OpenResult::Opened(opened) = state.edit_open(&document, EditOpenRequest { session_id: session.session_id.clone(), row_id: row_id.clone() }).await else {
        panic!("편집 핸들 필요")
    };
    assert_eq!(opened.label, KEY);
    assert_eq!(opened.text, "base");
    assert_ne!(opened.handle, row_id);
    let request = || EditRequest { session_id: session.session_id.clone(), handle: opened.handle.clone() };
    let save = request();
    let pending = state.begin_edit(&document, &save, Action::Save).ok().unwrap();
    let initial_revision = pending.revision.clone();
    assert!(matches!(state.begin_edit(&document, &save, Action::Save), Err(EditResult::Denied { .. })));
    let conflict = pending.finish(Err::<(), _>(EditError::Conflict { status: 412 }), |_, ()| unreachable!());
    assert_eq!(serde_json::to_value(conflict).unwrap(), serde_json::json!({
        "kind": "conflict", "message": "S3 조건부 저장 충돌 (HTTP 412)"
    }));
    let pending = state.begin_edit(&document, &save, Action::Save).ok().unwrap();
    assert!(Arc::ptr_eq(&initial_revision, &pending.revision));
    assert_eq!(pending.finish(Err::<(), _>(EditError::Unconfirmed), |_, ()| unreachable!()), EditResult::unknown());
    assert!(matches!(state.begin_edit(&document, &save, Action::Save), Err(EditResult::Unknown { .. })));
    assert_eq!(state.edit_compare(&document, request()).await, EditResult::Read { text: "peer".into() });
    let compare = state.begin_edit(&document, &save, Action::Compare).ok().unwrap();
    assert!(Arc::ptr_eq(&compare.revision, &initial_revision));
    assert!(matches!(state.begin_edit(&document, &save, Action::Save), Err(EditResult::Unknown { .. })));
    assert_eq!(state.edit_reload(&document, request()).await, EditResult::Read { text: "peer".into() });
    assert!(matches!(compare.finish(Ok(()), |_, ()| unreachable!()), EditResult::Denied { .. }));
    let pending = state.begin_edit(&document, &save, Action::Save).ok().unwrap();
    assert!(!Arc::ptr_eq(&initial_revision, &pending.revision));
    let OpenResult::Opened(other) = state.edit_open(&document, EditOpenRequest { session_id: session.session_id.clone(), row_id }).await else {
        panic!("두 번째 편집 핸들 필요")
    };
    let other_request = EditRequest { session_id: session.session_id.clone(), handle: other.handle };
    assert_ne!(request().handle, other_request.handle);
    assert!(state.edit_close(&document, request()));
    assert_eq!(pending.finish(Ok(()), |_, ()| unreachable!()), EditResult::unknown());
    assert!(!state.edit_close(&document, request()));
    state.0.lock().unwrap().tab(&document, &session.session_id).unwrap().edits.clear_rows();
    let late = state.begin_edit(&document, &other_request, Action::Save).ok().unwrap();
    state.navigate("main");
    assert_eq!(late.finish(Ok(()), |_, ()| unreachable!()), EditResult::unknown());
    assert_eq!(state.edit_compare(&document, other_request).await, EditResult::stale());
    let new_document = state.document("main");
    assert_eq!(state.edit_compare(&new_document, request()).await, EditResult::stale());
    assert_eq!(state.edit_compare(&document, request()).await, EditResult::stale());
    let requests: Vec<_> = fixture.actual_requests().map(|request| request.uri().parse::<http::Uri>().unwrap()).collect();
    assert_eq!(requests.len(), 24);
    let endpoint = format!("s3.{OBJECT_REGION}.amazonaws.com");
    assert!(requests.iter().all(|uri| uri.host() == Some(endpoint.as_str())));
    for get in [3, 9, 15, 21] {
        assert_eq!(requests[get].path(), "/selected-bucket/%EC%8B%A4%ED%97%98//../%ED%95%9C%EA%B8%80%20%2B%252F.txt");
    }
}

#[tokio::test]
async fn 저장은_목록_리전과_키를_유지하고_확정된_응답에서만_기준을_갱신한다() {
    let mut preflight = snapshot("v1", "base");
    preflight[3] = http::Response::builder().status(200)
        .header("etag", "\"v1\"").header("content-length", "4")
        .header("x-amz-version-id", "v1").header("x-amz-server-side-encryption", "AES256")
        .body(SdkBody::empty()).unwrap().try_into().unwrap();
    preflight[3..].rotate_left(1);
    let confirmed = http::Response::builder().status(200)
        .header("etag", "\"v2\"").header("x-amz-version-id", "v2")
        .body(SdkBody::empty()).unwrap().try_into().unwrap();
    let fixture = replay(snapshot("v1", "base").into_iter().chain(preflight).chain([confirmed]));
    let state = StorageIpc::default();
    let document = state.document("main");
    let session = state.insert(document.clone(), "profile".into(), REGION.into(), client(&fixture)).unwrap();
    let row_id = listed_row(&state, &document, &session.session_id);
    let OpenResult::Opened(opened) = state.edit_open(&document, EditOpenRequest { session_id: session.session_id.clone(), row_id }).await else {
        panic!("편집 핸들 필요")
    };
    let request = EditRequest { session_id: session.session_id.clone(), handle: opened.handle.clone() };
    let before = state.begin_edit(&document, &request, Action::Compare).ok().unwrap().revision.clone();
    assert_eq!(state.edit_save(&document, EditSaveRequest {
        session_id: session.session_id, handle: opened.handle, text: "updated".into(),
    }).await, EditResult::Saved);
    let after = state.begin_edit(&document, &request, Action::Compare).ok().unwrap();
    assert!(!Arc::ptr_eq(&before, &after.revision));
    let requests: Vec<_> = fixture.actual_requests().collect();
    assert_eq!(requests.len(), 13);
    let get = requests[3].uri().parse::<http::Uri>().unwrap();
    let put = requests[12].uri().parse::<http::Uri>().unwrap();
    assert_eq!(get.host(), Some("s3.eu-west-2.amazonaws.com"));
    assert_eq!(put.host(), get.host());
    assert_eq!(put.path(), get.path());
    assert_eq!(get.query(), Some("x-id=GetObject"));
    assert_eq!(put.query(), Some("x-id=PutObject"));
    assert_eq!(requests[3].method(), "GET");
    assert_eq!(requests[12].method(), "PUT");
    assert_eq!(requests[12].headers().get("if-match"), Some("\"v1\""));
    assert_eq!(requests[12].headers().get("x-amz-server-side-encryption"), Some("AES256"));
}
