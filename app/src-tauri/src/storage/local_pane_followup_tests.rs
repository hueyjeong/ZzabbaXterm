use std::fs;

use aws_sdk_s3::primitives::SdkBody;
use aws_smithy_runtime_api::client::orchestrator::HttpResponse;

use super::{super::{super::*, *}, approve, review::{Prompts, answer, no_prompt, select_request}};
use crate::editor::storage_files::{CloudOnly, LocalKind, needs_recall};
use crate::storage::tests::{client, replay, response};

/// ETag가 있는 HeadObject 응답이다.
pub(super) fn head(etag: &str) -> HttpResponse {
    http::Response::builder().status(200).header("etag", etag).body(SdkBody::empty()).unwrap().try_into().unwrap()
}

fn target(session: &Session, key: &str) -> Target {
    Target { session_id: session.session_id.clone(), bucket: "selected-bucket".into(), region: session.region.clone(), key: key.into() }
}

#[tokio::test]
async fn storage_local_upload_puts_with_conditions_from_head_object() {
    const KEY: &str = "원격/올릴 파일.txt";
    let precondition = || response(412, "<Error><Code>PreconditionFailed</Code></Error>");
    let reply = replay([
        response(404, SdkBody::empty()), response(200, SdkBody::empty()),
        head("\"etag-1\""), response(200, SdkBody::empty()),
        head("\"etag-2\""), precondition(),
        response(403, SdkBody::empty()), precondition(),
        response(403, SdkBody::empty()),
        response(404, SdkBody::empty()), response(501, "<Error><Code>NotImplemented</Code></Error>"),
    ]);
    let state = StorageIpc::default();
    let document = state.document("main");
    let session = state.insert(document.clone(), "fixture".into(), "eu-west-1".into(), client(&reply)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("올릴 파일.txt"), b"upload").unwrap();
    let root_id = approve(&state, &document, &session.session_id, dir.path());
    let prompts = Prompts::default();
    let select = || select_request(&session, &root_id, KEY, "올릴 파일.txt", Direction::Upload);
    let upload = |result: SelectResult| {
        let SelectResult::Approved { approval_id } = result else { panic!("승인 아님: {result:?}") };
        state.upload(&document, ApprovedTransfer { target: target(&session, KEY), approval_id })
    };

    // 404: If-None-Match로 새 객체만 만든다.
    assert_eq!(upload(state.select_local(&document, select(), no_prompt).await).await, Ok(6));
    // 200: 교체를 승인하면 확인한 ETag로 If-Match를 붙인다.
    assert_eq!(upload(state.select_local(&document, select(), answer(&prompts, true)).await).await, Ok(6));
    // 승인 뒤 다른 곳에서 바뀌면 412를 덮어쓰지 않은 실제 오류로 돌려준다.
    let changed = || Error::at(Stage::PutObject, Failure::PreconditionFailed);
    assert_eq!(upload(state.select_local(&document, select(), answer(&prompts, true)).await).await, Err(changed()));
    // 403: 확인할 수 없음을 묻고, 승인하면 If-None-Match로 보낸다. 412는 이미 있다는 뜻이다.
    assert_eq!(upload(state.select_local(&document, select(), answer(&prompts, true)).await).await, Err(changed()));
    assert_eq!(state.select_local(&document, select(), answer(&prompts, false)).await, SelectResult::Cancelled);
    // 조건을 지원하지 않는 응답은 조건 없이 다시 보내지 않는다.
    let unsupported = upload(state.select_local(&document, select(), no_prompt).await).await.unwrap_err();
    assert_eq!((unsupported.stage, unsupported.failure), (Stage::PutObject, Failure::Service { status: 501, code: Some("NotImplemented".into()) }));

    let prompts = prompts.lock().unwrap();
    assert_eq!(prompts.iter().map(|(dialog, _)| *dialog).collect::<Vec<_>>(),
        ["S3 객체 교체", "S3 객체 교체", "S3 객체 확인 불가", "S3 객체 확인 불가"]);
    assert!(prompts[2].1.contains("selected-bucket") && prompts[2].1.contains(KEY), "{}", prompts[2].1);
    let requests: Vec<_> = reply.actual_requests().collect();
    let conditions: Vec<_> = requests.iter().map(|request| (
        request.method().to_owned(),
        request.headers().get("if-none-match").map(str::to_owned),
        request.headers().get("if-match").map(str::to_owned),
    )).collect();
    let put = |none: Option<&str>, matching: Option<&str>| ("PUT".to_owned(), none.map(str::to_owned), matching.map(str::to_owned));
    let head = || ("HEAD".to_owned(), None, None);
    assert_eq!(conditions, [
        head(), put(Some("*"), None),
        head(), put(None, Some("\"etag-1\"")),
        head(), put(None, Some("\"etag-2\"")),
        head(), put(Some("*"), None),
        head(),
        head(), put(Some("*"), None),
    ]);
}

#[test]
fn storage_local_offline_files_are_reported_and_rejected_as_cloud_only() {
    const RECALL_ON_OPEN: u32 = 0x0004_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
    const REPARSE_POINT: u32 = 0x400;
    const ARCHIVE: u32 = 0x20;
    for attributes in [RECALL_ON_OPEN, RECALL_ON_DATA_ACCESS, RECALL_ON_DATA_ACCESS | REPARSE_POINT | ARCHIVE] {
        assert!(needs_recall(attributes), "{attributes:#x}");
    }
    for attributes in [0, ARCHIVE, REPARSE_POINT | ARCHIVE] {
        assert!(!needs_recall(attributes), "{attributes:#x}");
    }
    let error = std::io::Error::new(std::io::ErrorKind::Unsupported, CloudOnly);
    assert_eq!(LocalFailure::from(error), LocalFailure::CloudOnly);
    assert_eq!(serde_json::to_value(SelectResult::Rejected { reason: LocalFailure::CloudOnly }).unwrap(),
        serde_json::json!({ "kind": "rejected", "reason": { "kind": "cloudOnly" } }));

    // 일반 파일은 오프라인이 아니다.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), b"a").unwrap();
    let (entries, _) = LocalRoot::open(dir.path()).unwrap().list("", LIST_LIMIT).unwrap();
    assert_eq!((entries[0].kind, entries[0].offline), (LocalKind::File, false));
    assert_eq!(serde_json::to_value(&entries[0]).unwrap()["offline"], serde_json::json!(false));
}

#[cfg(windows)]
#[tokio::test]
async fn storage_download_marks_internet_zone_for_new_and_replaced_files() {
    use crate::storage::tests::object_response;
    const ZONE: &[u8] = b"[ZoneTransfer]\r\nZoneId=3\r\nHostUrl=about:internet\r\n";

    let reply = replay([
        object_response(6, None, SdkBody::from("object")),
        object_response(7, None, SdkBody::from("object2")),
    ]);
    let state = StorageIpc::default();
    let document = state.document("main");
    let session = state.insert(document.clone(), "fixture".into(), "eu-west-1".into(), client(&reply)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root_id = approve(&state, &document, &session.session_id, dir.path());
    let key = "원격/받은 파일.txt";
    let path = dir.path().join("받은 파일.txt");
    let zone = || {
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        fs::read(stream)
    };

    // 로컬 창 새 파일
    let SelectResult::Approved { approval_id } = state.select_local(&document, select_request(&session, &root_id, key, "", Direction::Download), no_prompt).await
    else { panic!("승인 아님") };
    state.download(&document, ApprovedTransfer { target: target(&session, key), approval_id }).await.unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"object");
    assert_eq!(zone().unwrap(), ZONE);

    // 기존 대화상자 경로로 표시된 파일을 교체해도 표시가 남는다.
    let id = state.begin_selection(&document, target(&session, key), Direction::Download).unwrap();
    let file = ApprovedFile::open(&path, Direction::Download).unwrap();
    assert!(matches!(&file, ApprovedFile::Download(target) if target.replaces_existing()));
    let LocalFileApproval::Approved { approval_id } = state.finish_selection(&document, &session.session_id, id, Some(Ok(file))) else { panic!() };
    state.download(&document, ApprovedTransfer { target: target(&session, key), approval_id }).await.unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"object2");
    assert_eq!(zone().unwrap(), ZONE);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}
