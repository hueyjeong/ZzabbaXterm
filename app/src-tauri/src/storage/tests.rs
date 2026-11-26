use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use aws_runtime::env_config::file::{EnvConfigFileKind, EnvConfigFiles};
use aws_sdk_s3::{config::Credentials, primitives::SdkBody};
use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
use http_body::{Body, Frame};

use super::*;

pub(super) fn replay(responses: impl IntoIterator<Item = HttpResponse>) -> StaticReplayClient {
    StaticReplayClient::new(
        responses
            .into_iter()
            .map(|response| ReplayEvent::new(http::Request::new(SdkBody::empty()), response))
            .collect(),
    )
}

pub(super) fn response(status: u16, body: impl Into<SdkBody>) -> HttpResponse {
    http::Response::builder()
        .status(status)
        .body(body.into())
        .unwrap()
        .try_into()
        .unwrap()
}

pub(super) fn client(replay: &StaticReplayClient) -> AwsS3 {
    let config = s3_config("eu-west-1")
        .credentials_provider(Credentials::new(
            "test-key",
            "test-secret",
            None,
            None,
            "test",
        ))
        .http_client(replay.clone())
        .build();
    AwsS3 {
        client: Client::from_conf(config),
        endpoint: "https://s3.eu-west-1.amazonaws.com".into(),
        region: "eu-west-1".into(),
    }
}

#[tokio::test]
async fn s01_s3_무응답을_시간_초과로_끝내고_재시도하지_않는다() {
    let transport = aws_smithy_http_client::test_util::NeverClient::new();
    let mut s3 = client(&replay([]));
    s3.client = Client::from_conf(s3.client.config().to_builder()
        .http_client(transport.clone())
        .timeout_config(TimeoutConfig::builder().operation_timeout(Duration::from_millis(100)).build())
        .build());
    let result = tokio::time::timeout(Duration::from_secs(3), s3.list_buckets(None)).await.unwrap();
    assert_eq!(result, Err(Error::at(Stage::ListBuckets, Failure::TimedOut)));
    assert_eq!(transport.num_calls(), 1);
}

#[tokio::test]
async fn storage_upload_stream_uses_the_held_file() {
    use aws_smithy_http_client::test_util::infallible_client_fn;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chosen.bin");
    std::fs::write(&path, b"original").unwrap();
    let file = crate::editor::storage_files::open_upload(&path).unwrap();
    std::fs::rename(&path, dir.path().join("previous.bin")).unwrap();
    std::fs::write(&path, b"replacement").unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let client = AwsS3 {
        client: Client::from_conf(s3_config("eu-west-1")
            .credentials_provider(Credentials::new("fixture", "fixture", None, None, "fixture"))
            .request_checksum_calculation(aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired)
            .http_client(infallible_client_fn(move |request| {
                send.send(request.into_body()).unwrap();
                http::Response::new(SdkBody::empty())
            })).build()),
        endpoint: String::new(),
        region: "eu-west-1".into(),
    };
    assert_eq!(client.upload("selected-bucket", "chosen.bin", file).await.unwrap(), 8);
    let body = ByteStream::new(receive.recv().unwrap()).collect().await.unwrap().into_bytes();
    assert_eq!(body.as_ref(), b"original");
    assert_eq!(std::fs::read(path).unwrap(), b"replacement");
}

#[tokio::test]
async fn s01_지정_버킷의_다음_페이지와_선택한_키를_그대로_요청한다() {
    let replay = replay([
        response(403, "<Error><Code>AccessDenied</Code></Error>"),
        response(
            200,
            concat!(
                "<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>true</IsTruncated>",
                "<CommonPrefixes><Prefix>%EC%8B%A4%ED%97%98%2F%2F</Prefix></CommonPrefixes>",
                "<NextContinuationToken>next+%2F=&amp;</NextContinuationToken></ListBucketResult>"
            ),
        ),
        response(
            200,
            concat!(
                "<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>false</IsTruncated>",
                "<Contents><Key>%EC%8B%A4%ED%97%98%2F%2F..%2F%ED%95%9C%EA%B8%80%20+%252F.txt</Key>",
                "<LastModified>2026-03-04T05:06:07.000Z</LastModified>",
                "<Size>3</Size><StorageClass>GLACIER_IR</StorageClass></Contents></ListBucketResult>"
            ),
        ),
        object_response(3, None, SdkBody::from("new")),
        response(
            200,
            "<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>false</IsTruncated></ListBucketResult>",
        ),
    ]);
    let s3 = client(&replay);
    let err = s3.list_buckets(None).await.unwrap_err();
    assert_eq!(err.stage, Stage::ListBuckets);
    assert_eq!(
        err.failure,
        Failure::Service {
            status: 403,
            code: Some("AccessDenied".into())
        }
    );

    let first = s3
        .list_objects("selected-bucket", "실험/", None)
        .await
        .unwrap();
    assert_eq!(first.rows, [ObjectRow::Prefix { prefix: "실험//".into() }]);
    assert_eq!(first.next_token.as_deref(), Some("next+%2F=&"));
    let second = s3
        .list_objects("selected-bucket", "실험/", first.next_token.as_deref())
        .await
        .unwrap();
    assert_eq!(second.next_token, None);
    let [ObjectRow::Object { key, bytes, last_modified, storage_class }] = second.rows.as_slice() else {
        panic!("객체 행 한 개 필요")
    };
    assert_eq!(key.as_bytes(), "실험//../한글 +%2F.txt".as_bytes());
    assert_eq!(*bytes, Some(3));
    assert_eq!(last_modified.as_deref(), Some("2026-03-04T05:06:07Z"));
    assert_eq!(storage_class.as_deref(), Some("GLACIER_IR"));
    assert_eq!(
        serde_json::to_value(&second.rows[0]).unwrap(),
        serde_json::json!({
            "kind": "object", "key": "실험//../한글 +%2F.txt", "bytes": 3,
            "lastModified": "2026-03-04T05:06:07Z", "storageClass": "GLACIER_IR",
        })
    );
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("chosen.txt");
    assert_eq!(
        s3.download("selected-bucket", key, &DownloadTarget::open(&target).unwrap(), &tokio::sync::watch::Sender::new(false)).await,
        Ok(Downloaded {
            bytes: 3,
            integrity: DownloadIntegrity::LengthOnly
        })
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"new");
    assert!(
        s3.list_objects("selected-bucket", "empty/", None)
            .await
            .unwrap()
            .rows
            .is_empty()
    );

    let requests: Vec<_> = replay
        .actual_requests()
        .map(|request| request.uri().parse::<http::Uri>().unwrap())
        .collect();
    assert_eq!(requests.len(), 5);
    assert!(
        requests
            .iter()
            .all(|uri| uri.host() == Some("s3.eu-west-1.amazonaws.com"))
    );
    assert_eq!(requests[1].path(), "/selected-bucket/");
    let query = requests[2].query().unwrap();
    assert!(
        query
            .split('&')
            .any(|part| part == "continuation-token=next%2B%252F%3D%26")
    );
    assert!(query.split('&').any(|part| part == "delimiter=%2F"));
    assert!(
        query
            .split('&')
            .any(|part| part == "prefix=%EC%8B%A4%ED%97%98%2F")
    );
    assert_eq!(
        requests[3].path(),
        "/selected-bucket/%EC%8B%A4%ED%97%98//../%ED%95%9C%EA%B8%80%20%2B%252F.txt"
    );
}

#[tokio::test]
async fn aws_버킷_목록은_누락되거나_다른_파티션의_리전을_거부한다() {
    let replay = replay([
        response(200, "<ListAllMyBucketsResult><Buckets><Bucket><Name>selected-bucket</Name></Bucket></Buckets></ListAllMyBucketsResult>"),
        response(200, "<ListAllMyBucketsResult><Buckets><Bucket><Name>selected-bucket</Name><BucketRegion>cn-north-1</BucketRegion></Bucket></Buckets></ListAllMyBucketsResult>"),
    ]);
    let s3 = client(&replay);
    for _ in 0..2 {
        assert_eq!(s3.list_buckets(None).await, Err(Error::at(Stage::ListBuckets, Failure::InvalidResponse)));
    }
}

pub(super) fn object_response(length: u64, checksum: Option<&str>, body: SdkBody) -> HttpResponse {
    let mut raw = http::Response::builder()
        .status(200)
        .header("content-length", length);
    if let Some(checksum) = checksum {
        raw = raw.header("x-amz-checksum-crc32", checksum);
    }
    raw.body(body).unwrap().try_into().unwrap()
}

#[derive(Default)]
struct BrokenStream(bool);

impl Body for BrokenStream {
    type Data = <SdkBody as Body>::Data;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Poll::Ready(Some(if std::mem::replace(&mut self.0, true) {
            Err(io::ErrorKind::ConnectionReset.into())
        } else {
            Ok(Frame::data(Self::Data::from_static(b"n")))
        }))
    }
}

pub(super) struct PausedStream {
    started: Option<tokio::sync::oneshot::Sender<()>>,
    sent: bool,
}

impl PausedStream {
    pub(super) fn new(started: tokio::sync::oneshot::Sender<()>) -> Self {
        Self { started: Some(started), sent: false }
    }
}

impl Body for PausedStream {
    type Data = <SdkBody as Body>::Data;
    type Error = io::Error;

    fn poll_frame(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if std::mem::replace(&mut self.sent, true) {
            if let Some(started) = self.started.take() { let _ = started.send(()); }
            Poll::Pending
        } else {
            Poll::Ready(Some(Ok(Frame::data(Self::Data::from_static(b"n")))))
        }
    }
}

#[tokio::test]
async fn s01_다운로드에_실패하면_기존_파일을_보존하고_완료하면_교체한다() {
    let replay = replay([
        object_response(3, None, SdkBody::from_body_1_x(BrokenStream::default())),
        object_response(4, None, SdkBody::from("new")),
        object_response(3, Some("AAAAAA=="), SdkBody::from("new")),
        object_response(3, Some("!"), SdkBody::from("new")),
        response(403, "<Error><Code>AccessDenied</Code></Error>"),
        object_response(3, Some("a+NERQ=="), SdkBody::from("new")),
    ]);
    let s3 = client(&replay);
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("선택.txt");
    std::fs::write(&target, b"old").unwrap();
    for (stage, failure, bytes) in [
        (Stage::DownloadBody, Failure::BodyRead, 1),
        (
            Stage::DownloadBody,
            Failure::Length { expected: Some(4) },
            3,
        ),
        (Stage::DownloadBody, Failure::Checksum, 3),
        (Stage::DownloadBody, Failure::Checksum, 0),
        (
            Stage::GetObject,
            Failure::Service {
                status: 403,
                code: Some("AccessDenied".into()),
            },
            0,
        ),
    ] {
        let error = s3
            .download("selected-bucket", "%2F/../other.txt", &DownloadTarget::open(&target).unwrap(), &tokio::sync::watch::Sender::new(false))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            Error {
                stage,
                failure,
                bytes
            }
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    assert_eq!(
        s3.download("selected-bucket", "%2F/../other.txt", &DownloadTarget::open(&target).unwrap(), &tokio::sync::watch::Sender::new(false))
            .await,
        Ok(Downloaded {
            bytes: 3,
            integrity: DownloadIntegrity::SdkChecksumVerified
        })
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"new");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn s01_설정된_외부_호스트를_차단한다() {
    const PROFILE: &str = "[default]\naws_access_key_id = test-default\naws_secret_access_key = test-default\n\
        [profile chosen]\nsso_start_url = https://example.awsapps.com/start\nsso_region = eu-west-1\n\
        sso_account_id = 012345678901\nsso_role_name = readOnly\nservices = custom\nendpoint_url = https://external.example\n\
        [services custom]\nsso =\n  endpoint_url = https://external.example\ns3 =\n  endpoint_url = https://external.example\n";
    let env = aws_types::os_shim_internal::Env::from_slice(&[
        ("AWS_ENDPOINT_URL", "https://external.example"),
        ("AWS_ENDPOINT_URL_S3", "https://external.example"),
    ]);
    let configured = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .env(env)
        .profile_name("chosen")
        .profile_files(
            EnvConfigFiles::builder()
                .with_contents(EnvConfigFileKind::Config, PROFILE)
                .build(),
        )
        .region(Region::new("eu-west-1"))
        .no_credentials()
        .load()
        .await;
    let overridden = replay([response(
        200,
        "<ListAllMyBucketsResult><Buckets/></ListAllMyBucketsResult>",
    )]);
    let inherited = aws_sdk_s3::config::Builder::from(&configured)
        .credentials_provider(Credentials::new(
            "test-key",
            "test-secret",
            None,
            None,
            "test",
        ))
        .http_client(overridden.clone())
        .build();
    Client::from_conf(inherited)
        .list_buckets()
        .send()
        .await
        .unwrap();
    let request = overridden
        .actual_requests()
        .next()
        .unwrap()
        .uri()
        .parse::<http::Uri>()
        .unwrap();
    assert_eq!(request.host(), Some("external.example"));
    let replay = replay([response(
        200,
        "<ListAllMyBucketsResult><Buckets/></ListAllMyBucketsResult>",
    )]);
    assert!(
        client(&replay)
            .list_buckets(None)
            .await
            .unwrap()
            .rows
            .is_empty()
    );
    assert_eq!(
        replay
            .actual_requests()
            .next()
            .unwrap()
            .uri()
            .parse::<http::Uri>()
            .unwrap()
            .host(),
        Some("s3.eu-west-1.amazonaws.com")
    );
}

#[tokio::test]
async fn s01_get_object_권한만으로_받고_미검증_체크섬을_구분한다() {
    for (kind, header, sum, integrity) in [
        (
            Some("COMPOSITE"),
            "x-amz-checksum-crc32",
            "6Rk6yw==-2",
            DownloadIntegrity::LengthOnly,
        ),
        (
            Some("COMPOSITE"),
            "x-amz-checksum-crc32",
            "6Rk6yw==",
            DownloadIntegrity::LengthOnly,
        ),
        (
            Some("COMPOSITE"),
            "x-amz-checksum-crc32",
            "NhCmhg==",
            DownloadIntegrity::LengthOnly,
        ),
        (
            None,
            "x-amz-checksum-crc32",
            "6Rk6yw==-2",
            DownloadIntegrity::LengthOnly,
        ),
        (
            Some("FULL_OBJECT"),
            "x-amz-checksum-crc32",
            "NhCmhg==",
            DownloadIntegrity::SdkChecksumVerified,
        ),
        (
            Some("FULL_OBJECT"),
            "x-amz-checksum-md5",
            "XUFAKrxLKna5cZ2REBfFkg==",
            DownloadIntegrity::LengthOnly,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("selected.txt");
        std::fs::write(&target, b"old").unwrap();
        let mut output = http::Response::builder()
            .status(200)
            .header("content-length", 5)
            .header(header, sum);
        if let Some(kind) = kind {
            output = output.header("x-amz-checksum-type", kind);
        }
        let replay = replay([
            output
                .body(SdkBody::from("hello"))
                .unwrap()
                .try_into()
                .unwrap(),
            response(403, "<Error><Code>AccessDenied</Code></Error>"),
        ]);
        assert_eq!(
            client(&replay)
                .download("selected-bucket", "object", &DownloadTarget::open(&target).unwrap(), &tokio::sync::watch::Sender::new(false))
                .await,
            Ok(Downloaded {
                bytes: 5,
                integrity
            })
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"hello");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        let mut requests = replay.actual_requests();
        let request = requests.next().unwrap();
        assert_eq!(
            request.uri(),
            "https://s3.eu-west-1.amazonaws.com/selected-bucket/object?x-id=GetObject"
        );
        assert_eq!(
            request.headers().get("x-amz-checksum-mode"),
            Some("ENABLED")
        );
        assert!(requests.next().is_none());
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct Clock(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl Clock {
    pub(super) fn advance(&self, seconds: u64) {
        self.0.fetch_add(seconds, std::sync::atomic::Ordering::Relaxed);
    }
}

impl aws_smithy_async::time::TimeSource for Clock {
    fn now(&self) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(
            1_800_000_000 + self.0.load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

pub(super) fn role_credentials(key: &str, clock: &Clock, lifetime_secs: u128) -> HttpResponse {
    use aws_smithy_async::time::TimeSource;
    let expiration = clock.now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() + lifetime_secs * 1000;
    response(200, format!(r#"{{"roleCredentials":{{"accessKeyId":"{key}","secretAccessKey":"synthetic","sessionToken":"temporary","expiration":{expiration}}}}}"#))
}

pub(super) fn caller_identity(account: &str) -> HttpResponse {
    response(200, format!("<GetCallerIdentityResponse><GetCallerIdentityResult><Account>{account}</Account><Arn>arn:aws:sts::{account}:assumed-role/ReadOnly/fixture</Arn><UserId>fixture</UserId></GetCallerIdentityResult></GetCallerIdentityResponse>"))
}

#[tokio::test]
async fn s01_캐시_부재와_만료만_같은_프로필의_재로그인으로_분류한다() {
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join(crate::ssm::sso_cache_entry("s01-test-session"));
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let transport = replay([]);
    for (contents, failure) in [
        (None, Failure::SsoLoginRequired),
        (Some(r#"{"accessToken":"test-only","expiresAt":"2000-01-01T00:00:00Z"}"#), Failure::SsoLoginRequired),
        (Some("not-json"), Failure::Credentials),
    ] {
        if let Some(contents) = contents { std::fs::write(&file, contents).unwrap(); }
        let config = crate::ssm::DirectSsoConfig {
            account: "012345678901".into(), role: "ReadOnly".into(), region: "eu-west-1".into(),
            start_url: "https://s01.invalid/start".into(), session: Some("s01-test-session".into()), scopes: None,
        };
        let result = AwsS3::verified("eu-west-1", config, home.path().to_owned(),
            SharedHttpClient::new(transport.clone()), SharedTimeSource::default()).await;
        assert_eq!(result.err().unwrap().failure, failure);
    }
    assert_eq!(transport.actual_requests().count(), 0);
}

// 승인된 비운영 계정에서 S3 객체 편집 경로를 직접 확인하는 수동 하네스다.
// ZXT_LIVE_PROFILE_ACCOUNT, ZXT_LIVE_PROFILE_ROLE, ZXT_LIVE_PROFILE_START_URL,
// ZXT_LIVE_BUCKET, ZXT_LIVE_KEY가 모두 있을 때만 실행한다.
#[tokio::test]
#[ignore = "실제 AWS SSO 프로필과 시험 버킷이 필요하다"]
async fn live_s3_object_edit_round_trip() {
    let Ok(account) = std::env::var("ZXT_LIVE_PROFILE_ACCOUNT") else {
        eprintln!("live S3 편집: 환경 변수가 없어 실행하지 않습니다");
        return;
    };
    let role = std::env::var("ZXT_LIVE_PROFILE_ROLE").unwrap();
    let start_url = std::env::var("ZXT_LIVE_PROFILE_START_URL").unwrap();
    let bucket = std::env::var("ZXT_LIVE_BUCKET").unwrap();
    let key = std::env::var("ZXT_LIVE_KEY").unwrap();
    let config = crate::ssm::DirectSsoConfig {
        account,
        role,
        region: "ap-northeast-1".into(),
        start_url,
        session: std::env::var("ZXT_LIVE_PROFILE_SESSION").ok(),
        scopes: None,
    };
    let client = AwsS3::for_sso_profile("ap-northeast-1", config).await.expect("SSO 프로필");
    let opened = client.read_object_for_edit(&bucket, &key).await.expect("GetObject 검증");
    assert!(opened.text.contains("S3"));
    let edited = format!("{}라이브 저장 확인\n", opened.text);
    let saved = client.save_edited_object(&opened.revision, edited.clone()).await;
    assert!(saved.is_ok(), "If-Match 저장 실패: {:?}", saved.err());
    let again = client.read_object_for_edit(&bucket, &key).await.expect("저장 결과 읽기");
    assert_eq!(again.text, edited);
    eprintln!("live S3 편집 통과: {}바이트", edited.len());
}
