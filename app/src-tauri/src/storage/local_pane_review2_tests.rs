use std::fs;

use aws_sdk_s3::primitives::SdkBody;

use super::{super::{super::*, *}, approve, followup::head, review::{answer, no_prompt, select_request, Prompts}};
use crate::editor::storage_files::{check_local_data, open_upload};
use crate::storage::{AwsS3, UploadCondition};
use crate::storage::tests::{client, replay, response};

fn put_error(status: u16, code: &str) -> aws_smithy_runtime_api::client::orchestrator::HttpResponse {
    response(status, format!("<Error><Code>{code}</Code></Error>"))
}

#[tokio::test]
async fn storage_upload_condition_failures_are_classified_by_condition() {
    let reply = replay([
        put_error(404, "NoSuchKey"),
        put_error(409, "ConditionalRequestConflict"),
        put_error(409, "OperationAborted"),
        put_error(409, "ConditionalRequestConflict"),
        put_error(412, "PreconditionFailed"),
        put_error(412, "PreconditionFailed"),
        put_error(412, "PreconditionFailed"),
        put_error(404, "NoSuchBucket"),
    ]);
    let s3: AwsS3 = client(&reply);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    fs::write(&path, b"data").unwrap();
    let matching = UploadCondition::Matches("\"etag\"".into());
    let put = |condition: UploadCondition| {
        let s3 = &s3;
        let file = open_upload(&path).unwrap();
        async move { s3.upload_if("selected-bucket", "k", file, &condition).await.unwrap_err().failure }
    };
    let changed = Failure::PreconditionFailed;
    let matching_bucket_gone = || UploadCondition::Matches("\"etag\"".into());
    // If-Match: 지워진 객체(404 NoSuchKey)와 동시 조건부 쓰기(409)는 바뀐 객체로 본다. 다른 409는 일반 오류다.
    assert_eq!(put(matching.clone()).await, changed);
    assert_eq!(put(matching.clone()).await, changed);
    assert_eq!(put(matching.clone()).await, Failure::Service { status: 409, code: Some("OperationAborted".into()) });
    assert_eq!(put(UploadCondition::Absent).await, changed);
    // 412: If-Match는 바뀐 객체, If-None-Match는 이미 있는 객체, 조건 없는 PUT은 일반 오류다.
    assert_eq!(put(matching).await, changed);
    assert_eq!(put(UploadCondition::Absent).await, Failure::AlreadyExists);
    assert_eq!(put(UploadCondition::None).await, Failure::Service { status: 412, code: Some("PreconditionFailed".into()) });
    // If-Match라도 NoSuchKey가 아닌 404(버킷 삭제 등)는 일반 오류다.
    assert_eq!(put(matching_bucket_gone()).await, Failure::Service { status: 404, code: Some("NoSuchBucket".into()) });
    let conditions: Vec<_> = reply.actual_requests().map(|request| (
        request.headers().get("if-none-match").map(str::to_owned),
        request.headers().get("if-match").map(str::to_owned),
    )).collect();
    let etag = || Some("\"etag\"".to_owned());
    let star = || Some("*".to_owned());
    assert_eq!(conditions, [
        (None, etag()), (None, etag()), (None, etag()), (star(), None), (None, etag()), (star(), None), (None, None), (None, etag()),
    ]);
}

#[tokio::test]
async fn storage_local_compatible_upload_refuses_forbidden_head_and_sends_condition_headers() {
    use crate::storage::compatible::{AddressStyle, CompatibleService, ServiceKey, ServiceKeyAlias, ServiceKeyRef};

    const REGION: &str = "local-test-1";
    let reply = replay([
        response(403, SdkBody::empty()),
        response(404, SdkBody::empty()),
        response(200, SdkBody::empty()),
        head("W/\"weak\""),
    ]);
    let service = CompatibleService::new("fixture".into(), "https://storage.example.test", REGION.into(), AddressStyle::Path).unwrap();
    let reference = ServiceKeyRef::new(&service, "fixture-item".into()).unwrap();
    let alias = ServiceKeyAlias::new("fixture-alias".into()).unwrap();
    let compatible = service.connect_with(&reference, alias, |_, _| {
        ServiceKey::new("fixture-access".into(), "fixture-secret".into())
    }, || crate::storage::SharedHttpClient::new(reply.clone())).unwrap();
    let state = StorageIpc::default();
    let document = state.document("main");
    let session_id = Uuid::new_v4().to_string();
    let location = SavedLocation { bucket: "selected-bucket".into(), prefix: String::new(), region: REGION.into() };
    StorageIpc::insert_compatible_tab(&mut state.0.lock().unwrap(), session_id.clone(), document.clone(), compatible, Some(&location));
    let session = Session { session_id: session_id.clone(), profile: "fixture-alias".into(), region: REGION.into(), endpoint: String::new(), location: None };
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("올릴.txt"), b"upload").unwrap();
    let root_id = approve(&state, &document, &session_id, dir.path());
    let select = || select_request(&session, &root_id, "원격/올릴.txt", "올릴.txt", Direction::Upload);

    // 호환 서비스의 403은 확인 대화상자 없이 업로드하지 않는다.
    let SelectResult::Failed { error } = state.select_local(&document, select(), no_prompt).await else { panic!("실패 아님") };
    assert_eq!((error.stage, error.failure), (Stage::HeadObject, Failure::Service { status: 403, code: None }));
    // 404면 If-None-Match를 실제로 보낸다.
    let SelectResult::Approved { approval_id } = state.select_local(&document, select(), no_prompt).await else { panic!("승인 아님") };
    let request = ApprovedTransfer {
        target: Target { session_id: session_id.clone(), bucket: "selected-bucket".into(), region: REGION.into(), key: "원격/올릴.txt".into() },
        approval_id,
    };
    assert_eq!(state.upload(&document, request).await, Ok(6));
    // 약한 ETag는 If-Match에 쓸 수 없어 응답 오류로 업로드하지 않는다.
    let SelectResult::Failed { error } = state.select_local(&document, select(), answer(&Prompts::default(), true)).await else { panic!("실패 아님") };
    assert_eq!((error.stage, error.failure), (Stage::HeadObject, Failure::InvalidResponse));
    let requests: Vec<_> = reply.actual_requests().map(|request| (
        request.method().to_owned(), request.headers().get("if-none-match").map(str::to_owned),
    )).collect();
    assert_eq!(requests, [
        ("HEAD".to_owned(), None), ("HEAD".to_owned(), None), ("PUT".to_owned(), Some("*".to_owned())), ("HEAD".to_owned(), None),
    ]);
}

#[test]
fn storage_local_data_check_reads_handle_attributes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    fs::write(&path, b"a").unwrap();
    // 일반 파일 핸들은 통과하고, 같은 판정(needs_recall)이 회수 속성을 거부한다.
    check_local_data(&fs::File::open(&path).unwrap()).unwrap();
    assert!(crate::editor::storage_files::needs_recall(0x0040_0000));
    assert!(open_upload(&path).is_ok());
}
