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
    let registry = state.0.lock().unwrap();
    assert!(!registry.compatible_is_pending(&document, &id, &first));
    assert!(!registry.compatible_is_pending(&other, &id, &second));
    assert!(registry.compatible_is_pending(&document, &id, &second));
    drop(registry);
    state.navigate("main");
    assert!(*second.borrow());
    assert!(state.reserve_compatible(&document, &id).is_err());
}

#[test]
fn only_the_current_reservation_publishes_settings_and_tab() {
    let state = StorageIpc::default();
    let document = state.document("main");
    let id = Uuid::new_v4().to_string();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("service.json");
    let prepare = |alias: &str| {
        const ENDPOINT: &str = "https://service.example.test";
        const REGION: &str = "local-test-1";
        let settings = Settings::select(SettingsInput {
            service_id: "service".into(), alias: alias.into(), endpoint: ENDPOINT.into(),
            region: REGION.into(), addressing: AddressStyle::Path,
        }, "item".into()).unwrap();
        let service = CompatibleService::new("service".into(), ENDPOINT, REGION.into(), AddressStyle::Path).unwrap();
        let key = ServiceKeyRef::new(&service, "item".into()).unwrap();
        let client = service.connect_with(&key, ServiceKeyAlias::new(alias.into()).unwrap(), |_, _| {
            ServiceKey::new("fixture-access".into(), "fixture-secret".into())
        }, || aws_smithy_runtime_api::client::http::SharedHttpClient::new(crate::storage::tests::replay([]))).unwrap();
        PreparedOpen {
            client, target: settings.view(), settings: Some(settings.stage_save(path.clone()).unwrap()),
            initial: InitialPage::Buckets { page: Page { rows: Vec::new(), next_token: None } },
            approved_saved: ApprovedSaved { path: path.clone(), expected: None },
        }
    };

    let first = state.reserve_compatible(&document, &id).unwrap();
    let cancelled = prepare("cancelled");
    assert!(state.close(&document, &id));
    assert!(matches!(state.finish_compatible(document.clone(), id.clone(), &first, Ok(cancelled)), Err(OpenError::Settings(SettingsError::Cancelled))));
    assert_eq!(Settings::load(&path).err(), Some(SettingsError::NotFound));
    assert!(!state.close(&document, &id));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

    let stale = state.reserve_compatible(&document, &id).unwrap();
    let before_replacement = prepare("stale-before");
    let after_replacement = prepare("stale-after");
    assert!(state.close(&document, &id));
    let current_doc = document.clone();
    let current = state.reserve_compatible(&current_doc, &id).unwrap();
    assert!(matches!(state.finish_compatible(document.clone(), id.clone(), &stale, Ok(before_replacement)), Err(OpenError::Settings(SettingsError::Cancelled))));
    assert_eq!(Settings::load(&path).err(), Some(SettingsError::NotFound));
    let session = state.finish_compatible(current_doc.clone(), id.clone(), &current, Ok(prepare("current"))).unwrap();
    assert_eq!(session.target.alias, "current");
    assert!(matches!(state.finish_compatible(document, id.clone(), &stale, Ok(after_replacement)), Err(OpenError::Settings(SettingsError::Cancelled))));
    assert_eq!(Settings::load(&path).unwrap().view().alias, "current");
    assert!(state.close(&current_doc, &id));

    let before_navigation = state.reserve_compatible(&current_doc, &id).unwrap();
    let navigating = prepare("navigating");
    state.navigate("main");
    assert!(matches!(state.finish_compatible(current_doc, id.clone(), &before_navigation, Ok(navigating)), Err(OpenError::Settings(SettingsError::Cancelled))));
    assert_eq!(Settings::load(&path).unwrap().view().alias, "current");
    assert!(!state.close(&state.document("main"), &id));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);

    for existing in [false, true] {
        if !existing {
            std::fs::remove_file(&path).unwrap();
        }
        let race = |alias| {
            let state = StorageIpc::default();
            let document = state.document("main");
            let id = Uuid::new_v4().to_string();
            let closed = state.reserve_compatible(&document, &id).unwrap();
            let mut prepared = prepare(alias);
            if existing {
                let saved = Settings::load(&path).unwrap();
                prepared.approved_saved.expected = Some((saved.saved_view().unwrap(), saved.item_id().into()));
            }
            (state, document, id, closed, prepared)
        };
        let first = race("first");
        let second = race("second");
        let start = std::sync::Barrier::new(2);
        let finish = |(state, document, id, closed, prepared): (StorageIpc, _, _, _, _)| {
            start.wait();
            state.finish_compatible(document, id, &closed, Ok(prepared))
        };
        let (winner, loser) = std::thread::scope(|scope| {
            let first = scope.spawn(|| finish(first));
            let second = finish(second);
            match (first.join().unwrap(), second) {
                (Ok(winner), Err(loser)) | (Err(loser), Ok(winner)) => (winner, loser),
                _ => panic!("expected one publish"),
            }
        });
        assert!(matches!(loser, OpenError::Settings(SettingsError::WrongService)));
        assert_eq!(Settings::load(&path).unwrap().view(), winner.target);
    }
}

#[tokio::test]
async fn validation_reuses_first_page_and_rejects_failed_requests_and_stale_key_revision() {
    use aws_smithy_runtime_api::client::http::SharedHttpClient;
    let state = StorageIpc::default();
    let document = state.document("main");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("service.json");
    let settings = |item: &str| Settings::select(SettingsInput {
        service_id: "fixture".into(), alias: "fixture".into(), endpoint: "https://fixture.example.test".into(),
        region: "local-test-1".into(), addressing: AddressStyle::Path,
    }, item.into()).unwrap();
    for (status, xml, expected_failure) in [
        (403, "<Error><Code>AccessDenied</Code></Error>", Some(Failure::Service { status: 403, code: Some("AccessDenied".into()) })),
        (400, "<Error><Code>AuthorizationHeaderMalformed</Code></Error>", Some(Failure::SigningRegion { status: 400, code: "AuthorizationHeaderMalformed".into() })),
        (200, "<ListAllMyBucketsResult><Buckets><Bucket><Name>fixture-bucket</Name></Bucket></Buckets><ContinuationToken>next</ContinuationToken></ListAllMyBucketsResult>", None),
    ] {
        let id = Uuid::new_v4().to_string();
        let closed = state.reserve_compatible(&document, &id).unwrap();
        let transport = crate::storage::tests::replay([response(status, xml)]);
        let service = CompatibleService::new("fixture".into(), "https://fixture.example.test", "local-test-1".into(), AddressStyle::Path).unwrap();
        let reference = ServiceKeyRef::new(&service, "original".into()).unwrap();
        let client = service.connect_with(&reference, ServiceKeyAlias::new("fixture".into()).unwrap(), |_, _| {
            ServiceKey::new("fixture-access".into(), "fixture-secret".into())
        }, || SharedHttpClient::new(transport.clone())).unwrap();
        let verified = verify_open(&client, OpenScope::ServiceBrowse, &closed).await;
        assert_eq!(transport.actual_requests().count(), 1);
        if let Some(failure) = expected_failure {
            let result = state.finish_compatible(document.clone(), id.clone(), &closed, verified.map(|_| unreachable!()));
            assert!(matches!(result, Err(OpenError::Connection { connection }) if connection.stage == Stage::ListBuckets && connection.failure == failure));
            assert_eq!(Settings::load(&path).err(), Some(SettingsError::NotFound));
            assert!(!state.close(&document, &id));
        } else {
            let initial = verified.unwrap();
            let InitialPage::Buckets { page } = &initial else { panic!("expected buckets") };
            assert_eq!(page.rows.len(), 1);
            assert_eq!(page.next_token.as_deref(), Some("next"));
            let original = settings("original");
            original.stage_save(path.clone()).unwrap().publish().unwrap();
            let expected = Settings::load(&path).unwrap().saved_view().unwrap();
            assert_eq!(Settings::check_saved(&path, &expected, "unapproved"), Err(SettingsError::WrongService));
            let prepared = PreparedOpen {
                client, target: original.view(), settings: Some(original.stage_save(path.clone()).unwrap()), initial,
                approved_saved: ApprovedSaved { path: path.clone(), expected: Some((expected, "original".into())) },
            };
            settings("replacement").stage_save(path.clone()).unwrap().publish().unwrap();
            assert!(matches!(state.finish_compatible(document.clone(), id.clone(), &closed, Ok(prepared)), Err(OpenError::Settings(SettingsError::WrongService))));
            assert_eq!(Settings::load(&path).unwrap().item_id(), "replacement");
            assert!(!state.close(&document, &id));
            let id = Uuid::new_v4().to_string();
            let closed = state.reserve_compatible(&document, &id).unwrap();
            let client = service.connect_with(&reference, ServiceKeyAlias::new("fixture".into()).unwrap(), |_, _| {
                ServiceKey::new("fixture-access".into(), "fixture-secret".into())
            }, || SharedHttpClient::new(crate::storage::tests::replay([response(status, xml)]))).unwrap();
            let initial = verify_open(&client, OpenScope::ServiceBrowse, &closed).await.unwrap();
            original.stage_save(path.clone()).unwrap().publish().unwrap();
            let saved = Settings::load(&path).unwrap();
            let ready = PreparedOpen {
                client, target: saved.view(), settings: None, initial,
                approved_saved: ApprovedSaved { path: path.clone(), expected: Some((saved.saved_view().unwrap(), saved.item_id().into())) },
            };
            let session = state.finish_compatible(document.clone(), id.clone(), &closed, Ok(ready)).unwrap();
            let InitialPage::Buckets { page } = session.initial else { panic!("expected buckets") };
            assert_eq!(page.rows[0].name, "fixture-bucket");
            assert_eq!(page.next_token.as_deref(), Some("next"));
            assert!(state.close(&document, &id));
        }
    }
}

#[tokio::test]
async fn validation_reports_tls_failure_without_a_retry() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use aws_smithy_runtime_api::client::{
        http::{HttpConnector, HttpConnectorFuture, SharedHttpConnector, http_client_fn},
        orchestrator::HttpRequest,
        result::ConnectorError,
    };

    #[derive(Clone, Debug, Default)]
    struct RejectTls(Arc<AtomicUsize>);
    impl HttpConnector for RejectTls {
        fn call(&self, _: HttpRequest) -> HttpConnectorFuture {
            self.0.fetch_add(1, Ordering::Relaxed);
            HttpConnectorFuture::new(async {
                let certificate = rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer);
                let error = std::io::Error::new(std::io::ErrorKind::InvalidData, certificate);
                Err(ConnectorError::io(Box::new(error)))
            })
        }
    }

    let transport = RejectTls::default();
    let connector = transport.clone();
    let service = CompatibleService::new("fixture".into(), "https://fixture.example.test", "local-test-1".into(), AddressStyle::Path).unwrap();
    let reference = ServiceKeyRef::new(&service, "item".into()).unwrap();
    let client = service.connect_with(&reference, ServiceKeyAlias::new("fixture".into()).unwrap(), |_, _| {
        ServiceKey::new("fixture-access".into(), "fixture-secret".into())
    }, || http_client_fn(move |_, _| SharedHttpConnector::new(connector.clone()))).unwrap();
    let cancelled = verify_open(&client, OpenScope::ServiceBrowse, &watch::Sender::new(true)).await.unwrap_err();
    assert!(matches!(cancelled, OpenError::Settings(SettingsError::Cancelled)));
    assert_eq!(transport.0.load(Ordering::Relaxed), 0);
    let error = verify_open(&client, OpenScope::ServiceBrowse, &watch::Sender::new(false)).await.unwrap_err();
    assert!(matches!(error, OpenError::Connection { connection } if connection.stage == Stage::ListBuckets && connection.failure == Failure::Tls));
    assert_eq!(transport.0.load(Ordering::Relaxed), 1);
}

#[test]
fn open_scope_is_required_and_shows_the_approved_address() {
    let parse = |scope| {
        let mut command = serde_json::json!({
            "sessionId": "fixture",
            "target": {"kind": "select", "itemId": "item", "settings": {
                "serviceId": "fixture", "alias": "fixture", "endpoint": "https://store.example.test:9443",
                "region": "local-test-1", "addressing": "path"
            }}
        });
        if let Some(scope) = scope { command["scope"] = scope; }
        serde_json::from_value::<OpenCommand>(command)
    };
    assert!(parse(None).is_err());
    assert!(parse(Some(serde_json::json!({"kind": "service_browse", "bucket": "sample"}))).is_err());
    assert!(parse(Some(serde_json::json!({"kind": "bucket", "bucket": "sample"}))).is_err());
    let request = parse(Some(serde_json::json!({"kind": "bucket", "bucket": "sample", "prefix": "한\n\0/"}))).unwrap();
    let OpenRequest::Select { settings, item_id } = request.target else { panic!("expected select") };
    let mut view = Settings::select(settings, item_id).unwrap().view();
    assert_eq!(request.scope.approval_target(&view).unwrap(), "버킷 주소: https://store.example.test:9443/sample/\n접두어: \"한\\n\\0/\"");
    view.addressing = AddressStyle::VirtualHosted;
    assert_eq!(request.scope.approval_target(&view).unwrap(), "버킷 주소: https://sample.store.example.test:9443/\n접두어: \"한\\n\\0/\"");
    assert_eq!(OpenScope::ServiceBrowse.approval_target(&view).unwrap(), "서비스 전체 버킷 목록: https://store.example.test:9443/");
    for (bucket, prefix, failure) in [
        ("", String::new(), Failure::InvalidBucket),
        ("with.dot", String::new(), Failure::Destination),
        ("sample", "한".repeat(342), Failure::Request),
    ] {
        let error = OpenScope::Bucket { bucket: bucket.into(), prefix }.approval_target(&view).unwrap_err();
        assert_eq!(error, Error::at(Stage::ListObjects, failure));
    }
}

#[tokio::test]
async fn direct_bucket_publishes_the_first_page_and_allows_only_that_bucket() {
    use super::super::BucketPageRequest;
    use aws_smithy_runtime_api::client::http::SharedHttpClient;
    use crate::storage::tests::replay;
    const ENDPOINT: &str = "https://store.example.test:9443";
    const REGION: &str = "local-test-1";
    const EMPTY: &str = "<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>false</IsTruncated></ListBucketResult>";
    const PAGE: &str = "<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>true</IsTruncated><NextContinuationToken>next</NextContinuationToken><Contents><Key>dir%2Fobject</Key><Size>3</Size></Contents></ListBucketResult>";
    let service = CompatibleService::new("fixture".into(), ENDPOINT, REGION.into(), AddressStyle::Path).unwrap();
    let reference = ServiceKeyRef::new(&service, "item".into()).unwrap();
    let settings = Settings::select(SettingsInput {
        service_id: "fixture".into(), alias: "fixture".into(), endpoint: ENDPOINT.into(),
        region: REGION.into(), addressing: AddressStyle::Path,
    }, "item".into()).unwrap();
    let state = StorageIpc::default();
    let document = state.document("main");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("service.json");
    let scope = || OpenScope::Bucket { bucket: "sample".into(), prefix: "dir/".into() };
    for (status, body, failure) in [
        (403, "<Error><Code>AccessDenied</Code></Error>", Some(Failure::Service { status: 403, code: Some("AccessDenied".into()) })),
        (200, "<ListBucketResult/>", Some(Failure::InvalidResponse)),
        (200, PAGE, None),
    ] {
        let id = Uuid::new_v4().to_string();
        let closed = state.reserve_compatible(&document, &id).unwrap();
        let transport = replay([response(status, body), response(200, EMPTY)]);
        let client = service.connect_with(&reference, ServiceKeyAlias::new("fixture".into()).unwrap(), |_, _| {
            ServiceKey::new("fixture-access".into(), "fixture-secret".into())
        }, || SharedHttpClient::new(transport.clone())).unwrap();
        let verified = verify_open(&client, scope(), &closed).await;
        assert_eq!(transport.actual_requests().count(), 1);
        let request = transport.actual_requests().next().unwrap();
        let uri: http::Uri = request.uri().parse().unwrap();
        assert_eq!(uri.authority().unwrap().as_str(), "store.example.test:9443");
        assert_eq!(uri.path(), "/sample/");
        let query = uri.query().unwrap();
        for field in ["list-type=2", "prefix=dir%2F", "delimiter=%2F", "encoding-type=url"] {
            assert!(query.split('&').any(|pair| pair == field), "missing {field} in {query}");
        }
        assert!(request.headers().get("authorization").unwrap().contains("fixture-access/"));
        assert!(!path.exists());
        let result = state.finish_compatible(document.clone(), id.clone(), &closed, verified.map(|initial| PreparedOpen {
            client, target: settings.view(), settings: Some(settings.stage_save(path.clone()).unwrap()), initial,
            approved_saved: ApprovedSaved { path: path.clone(), expected: None },
        }));
        if let Some(failure) = failure {
            assert!(matches!(result, Err(OpenError::Connection { connection }) if connection == Error::at(Stage::ListObjects, failure)));
            assert!(!path.exists());
            assert!(!state.close(&document, &id));
            continue;
        }
        let session = result.unwrap();
        assert!(path.exists());
        assert_eq!(serde_json::to_value(session).unwrap()["initial"], serde_json::json!({
            "kind": "objects", "location": {"bucket": "sample", "prefix": "dir/", "region": REGION},
            "page": {"rows": [{"kind": "object", "key": "dir/object", "bytes": 3}], "nextToken": "next"}
        }));
        let bucket_page = state.list_buckets(&document, BucketPageRequest { session_id: id.clone(), token: None }).await;
        assert_eq!(bucket_page.unwrap_err(), Error::at(Stage::ListBuckets, Failure::Destination));
        let page = |bucket: &str, token: Option<&str>| ObjectPageRequest {
            session_id: id.clone(), bucket: bucket.into(), prefix: "dir/".into(), region: REGION.into(), token: token.map(str::to_owned),
        };
        assert_eq!(state.list_objects(&document, page("other", None)).await.unwrap_err(), Error::at(Stage::ListObjects, Failure::Destination));
        let target = |bucket: &str| Target { session_id: id.clone(), bucket: bucket.into(), key: "dir/object".into(), region: REGION.into() };
        assert!(state.begin_selection(&document, target("other"), Direction::Download).is_none());
        let approval_id = state.begin_selection(&document, target("sample"), Direction::Download).unwrap();
        let local = dir.path().join("download.txt");
        assert!(matches!(state.finish_selection(&document, &id, approval_id.clone(), Some(ApprovedFile::open(&local, Direction::Download))),
            LocalFileApproval::Approved { .. }));
        assert!(state.download(&document, ApprovedTransfer { target: target("other"), approval_id }).await.is_err());
        assert_eq!(transport.actual_requests().count(), 1);
        state.list_objects(&document, page("sample", Some("next"))).await.unwrap();
        assert_eq!(transport.actual_requests().count(), 2);
        assert!(state.close(&document, &id));
    }
}

#[tokio::test]
async fn valid_service_tls_does_not_publish_a_bucket_with_a_mismatched_tls_name() {
    use crate::storage::compatible::tests::loopback_list_client;
    let style = AddressStyle::VirtualHosted;
    let (browse, server) = loopback_list_client("localhost", "localhost", "", style, true, true, true);
    assert!(matches!(verify_open(&browse, OpenScope::ServiceBrowse, &watch::Sender::new(false)).await,
        Ok(InitialPage::Buckets { page }) if page.rows.is_empty()));
    assert!(server.join().unwrap());

    let (bucket, server) = loopback_list_client("localhost", "sample.localhost", "", style, true, true, false);
    let scope = OpenScope::Bucket { bucket: "sample".into(), prefix: String::new() };
    assert!(matches!(verify_open(&bucket, scope, &watch::Sender::new(false)).await,
        Ok(InitialPage::Objects { location, page }) if location.bucket == "sample" && page.rows.is_empty()));
    assert!(server.join().unwrap());

    let state = StorageIpc::default();
    let document = state.document("main");
    let id = Uuid::new_v4().to_string();
    let closed = state.reserve_compatible(&document, &id).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("service.json");
    let (client, server) = loopback_list_client("localhost", "other.localhost", "", style, true, false, false);
    let scope = OpenScope::Bucket { bucket: "other".into(), prefix: String::new() };
    let endpoint = &client.target().endpoint;
    let settings = Settings::select(SettingsInput {
        service_id: client.target().service_id.clone(), alias: "fixture".into(), endpoint: endpoint.clone(),
        region: client.target().region.clone(), addressing: style,
    }, "item".into()).unwrap();
    assert!(scope.approval_target(&settings.view()).unwrap().contains("https://other.localhost:"));
    let verified = verify_open(&client, scope, &closed).await;
    let result = state.finish_compatible(document.clone(), id.clone(), &closed, verified.map(|initial| PreparedOpen {
        client, target: settings.view(), settings: Some(settings.stage_save(path.clone()).unwrap()), initial,
        approved_saved: ApprovedSaved { path: path.clone(), expected: None },
    }));
    assert!(matches!(result, Err(OpenError::Connection { connection }) if connection == Error::at(Stage::ListObjects, Failure::Tls)));
    assert!(!server.join().unwrap());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    assert!(!state.close(&document, &id));
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
        StorageIpc::insert_compatible_tab(&mut state.0.lock().unwrap(), session_id.clone(), doc.clone(), client, None);
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
