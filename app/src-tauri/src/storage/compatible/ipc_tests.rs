use super::*;
use super::super::{ApprovedFile, ApprovedTransfer, Direction, LocalFileApproval, ObjectPageRequest, Stage, Target};
use crate::storage::{Failure, compatible::{CompatibleService, ServiceKey, ServiceKeyAlias, ServiceKeyRef}, tests::{PausedStream, object_response, response}};
use aws_sdk_s3::primitives::SdkBody;

#[test]
fn cancelled_open_cannot_use_a_new_reservation_or_another_document() {
    let state = StorageIpc::default();
    let document = state.document("main");
    let other = state.document("other");
    let aws_id = Uuid::new_v4();
    state.reserve_open(&document, aws_id).unwrap();
    assert!(state.reserve_compatible(&other, &aws_id.to_string()).is_err());
    assert!(state.reserve_compatible(&other, &Uuid::new_v4().simple().to_string()).is_err());
    let id = Uuid::new_v4().to_string();
    let first = state.reserve_compatible(&document, &id).unwrap();
    assert!(!state.close(&other, &id));
    assert!(state.reserve_compatible(&other, &id).is_err());
    assert!(state.close(&document, &id));
    assert!(*first.borrow());
    let second = state.reserve_compatible(&document, &id).unwrap();
    assert!(!state.compatible_is_pending(&id, &first));
    assert!(state.compatible_is_pending(&id, &second));
    state.navigate("main");
    assert!(*second.borrow());
    assert!(state.reserve_compatible(&document, &id).is_err());
}

#[tokio::test]
async fn compatible_approval_stays_in_its_tab_and_close_recovers_staging() {
    use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
    let state = StorageIpc::default();
    let doc = state.document("main");
    const REGION: &str = "local-test-1";
    let (started, paused) = tokio::sync::oneshot::channel();
    let request = || http::Request::new(SdkBody::empty());
    let list = || response(200, "<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>false</IsTruncated></ListBucketResult>");
    let transport = StaticReplayClient::new(vec![
        ReplayEvent::new(request(), list()),
        ReplayEvent::new(request(), list()),
        ReplayEvent::new(request(), object_response(3, None, SdkBody::from_body_1_x(PausedStream::new(started)))),
    ]);
    let tab = |id: &str, endpoint: &str, item: &str| {
        let service = CompatibleService::new(id.into(), endpoint, REGION.into(), AddressStyle::Path).unwrap();
        let reference = ServiceKeyRef::new(&service, item.into()).unwrap();
        let alias = ServiceKeyAlias::new(id.into()).unwrap();
        let client = service.connect_with(&reference, alias, |scope, selected| {
            assert_eq!((scope.service_id(), scope.origin(), selected), (id, endpoint, item));
            ServiceKey::new("fixture-access".into(), "fixture-secret".into())
        }, || aws_smithy_runtime_api::client::http::SharedHttpClient::new(transport.clone())).unwrap();
        let session_id = Uuid::new_v4().to_string();
        StorageIpc::insert_compatible_tab(&mut state.0.lock().unwrap(), session_id.clone(), doc.clone(), client);
        session_id
    };
    let first = tab("one", "https://one.example.test", "one-item");
    let second = tab("two", "https://two.example.test", "two-item");
    assert!(state.list_objects(&doc, ObjectPageRequest {
        session_id: first.clone(), token: None, bucket: "sample".into(), region: "us-east-1".into(), prefix: String::new(),
    }).await.is_err());
    assert_eq!(transport.actual_requests().count(), 0);
    for session_id in [&first, &second] {
        state.list_objects(&doc, ObjectPageRequest {
            session_id: session_id.clone(), token: None, bucket: "sample".into(), region: REGION.into(), prefix: String::new(),
        }).await.unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("target.txt");
    std::fs::write(&path, b"original").unwrap();
    let target = |session_id: &str| Target { session_id: session_id.into(), bucket: "sample".into(), region: REGION.into(), key: "object".into() };
    let approval_id = state.begin_selection(&doc, target(&first), Direction::Download).unwrap();
    assert!(matches!(state.finish_selection(&doc, &first, approval_id.clone(), Some(ApprovedFile::open(&path, Direction::Download))),
        LocalFileApproval::Approved { .. }));
    assert!(state.download(&doc, ApprovedTransfer { target: target(&second), approval_id: approval_id.clone() }).await.is_err());
    let mut wrong_region = target(&first);
    wrong_region.region = "us-east-1".into();
    assert!(state.download(&doc, ApprovedTransfer { target: wrong_region, approval_id: approval_id.clone() }).await.is_err());
    assert_eq!(transport.actual_requests().count(), 2);
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(state.download(&doc, ApprovedTransfer { target: target(&first), approval_id }), async {
            paused.await.unwrap();
            assert!(state.close(&doc, &first));
        })
    }).await.unwrap();
    let error = result.unwrap_err();
    assert_eq!((error.stage, error.failure, error.bytes), (Stage::DownloadBody, Failure::Request, 1));
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    assert_eq!(transport.actual_requests().count(), 3);
    let request = transport.actual_requests().last().unwrap();
    let uri: http::Uri = request.uri().parse().unwrap();
    assert_eq!(uri.scheme_str(), Some("https"));
    let authority = uri.authority().unwrap();
    assert_eq!(authority.as_str(), "one.example.test");
    assert_eq!(authority.port_u16().unwrap_or(443), 443);
    assert_eq!(uri.path(), "/sample/object");
    assert_eq!(uri.query(), Some("x-id=GetObject"));
}
