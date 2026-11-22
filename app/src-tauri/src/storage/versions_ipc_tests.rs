use super::*;
use crate::storage::tests::{client, replay, response};

#[tokio::test]
async fn version_count_requires_the_verified_bucket_region_and_document() {
    let state = StorageIpc::default();
    let document = state.document("main");
    let other = state.document("other");
    let fixture = replay([response(200,
        "<ListVersionsResult><EncodingType>url</EncodingType><IsTruncated>false</IsTruncated></ListVersionsResult>",
    )]);
    let session = state.insert(document.clone(), "profile".into(), "eu-west-1".into(), client(&fixture)).unwrap();
    let request = |bucket: &str, region: &str| PreviousFilesRequest {
        session_id: session.session_id.clone(),
        bucket: bucket.into(),
        region: region.into(),
        prefix: String::new(),
    };
    let valid = || request("selected-bucket", "eu-west-1");
    let invalid = || Err(unavailable(Stage::ListObjects));
    assert_eq!(state.count_previous_files(&other, valid()).await, invalid());
    assert_eq!(state.count_previous_files(&document, request("other-bucket", "eu-west-1")).await, invalid());
    assert_eq!(state.count_previous_files(&document, request("selected-bucket", "eu-west-2")).await, invalid());
    assert_eq!(fixture.actual_requests().count(), 0);
    assert_eq!(state.count_previous_files(&document, valid()).await, Ok(0));
    assert!(state.close(&document, &session.session_id));
    assert_eq!(state.count_previous_files(&document, valid()).await, invalid());
    assert_eq!(fixture.actual_requests().count(), 1);
}

#[tokio::test]
async fn closing_the_session_cancels_the_running_version_request() {
    use aws_sdk_s3::{Client, config::Credentials};
    use aws_smithy_http_client::test_util::NeverClient;

    let transport = NeverClient::new();
    let client = AwsS3 {
        client: Client::from_conf(super::super::s3_config("eu-west-1")
            .credentials_provider(Credentials::new("fixture", "fixture", None, None, "fixture"))
            .http_client(transport.clone()).build()),
        endpoint: String::new(),
        region: "eu-west-1".into(),
    };
    let state = StorageIpc::default();
    let document = state.document("main");
    let session = state.insert(document.clone(), "profile".into(), "eu-west-1".into(), client).unwrap();
    let request = PreviousFilesRequest {
        session_id: session.session_id.clone(),
        bucket: "selected-bucket".into(),
        region: "eu-west-1".into(),
        prefix: String::new(),
    };
    let scan = state.count_previous_files(&document, request);
    let close = async {
        while transport.num_calls() == 0 { tokio::task::yield_now().await; }
        assert!(state.close(&document, &session.session_id));
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(scan, close) })
        .await.unwrap();
    assert_eq!(result, Err(unavailable(Stage::ListObjects)));
    assert_eq!(transport.num_calls(), 1);
}
