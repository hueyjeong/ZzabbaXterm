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
    }
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
                "<Size>3</Size></Contents></ListBucketResult>"
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
    let [ObjectRow::Object { key, bytes }] = second.rows.as_slice() else {
        panic!("객체 행 한 개 필요")
    };
    assert_eq!(key.as_bytes(), "실험//../한글 +%2F.txt".as_bytes());
    assert_eq!(*bytes, Some(3));
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("chosen.txt");
    assert_eq!(
        s3.download("selected-bucket", key, &target, &tokio::sync::watch::Sender::new(false)).await,
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

fn object_response(length: u64, checksum: Option<&str>, body: SdkBody) -> HttpResponse {
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

struct ClosingStream {
    closed: std::sync::Arc<tokio::sync::watch::Sender<bool>>,
    sent: bool,
}

impl Body for ClosingStream {
    type Data = <SdkBody as Body>::Data;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if std::mem::replace(&mut self.sent, true) {
            self.closed.send_replace(true);
            Poll::Pending
        } else {
            Poll::Ready(Some(Ok(Frame::data(Self::Data::from_static(b"n")))))
        }
    }
}

#[tokio::test]
async fn s01_다운로드_본문_중에_닫으면_기존_파일을_보존한다() {
    let closed = std::sync::Arc::new(tokio::sync::watch::Sender::new(false));
    let stream = ClosingStream { closed: closed.clone(), sent: false };
    let replay = replay([object_response(3, None, SdkBody::from_body_1_x(stream))]);
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("기존.txt");
    std::fs::write(&target, b"old").unwrap();
    let error = client(&replay)
        .download("selected-bucket", "object", &target, &closed)
        .await
        .unwrap_err();
    assert_eq!(error.stage, Stage::DownloadBody);
    assert_eq!(error.bytes, 1);
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    assert_eq!(replay.actual_requests().count(), 1);
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
            .download("selected-bucket", "%2F/../other.txt", &target, &tokio::sync::watch::Sender::new(false))
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
        s3.download("selected-bucket", "%2F/../other.txt", &target, &tokio::sync::watch::Sender::new(false))
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
    auth::assert_sso_endpoint_boundary().await;
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
                .download("selected-bucket", "object", &target, &tokio::sync::watch::Sender::new(false))
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

#[test]
fn s01_캐시_부재와_만료만_같은_프로필의_재로그인으로_분류한다() {
    const CHILD: &str = "ZZABBA_S01_TEST_SSO_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let home = tempfile::tempdir().unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "storage::tests::s01_캐시_부재와_만료만_같은_프로필의_재로그인으로_분류한다",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("HOME", home.path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
        return;
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let profile = |session: Option<&str>| crate::ssm::DirectSsoConfig {
            account: "012345678901".into(), role: "readOnly".into(), region: "eu-west-1".into(),
            start_url: "https://s01.invalid/start".into(), session: session.map(str::to_owned),
        };
        let cache = std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(".aws/sso/cache");
        std::fs::create_dir_all(&cache).unwrap();
        let no_requests = replay([]);
        let run = |config| async {
            let provider = auth::provider_with(config, no_requests.clone());
            let mut s3 = client(&no_requests);
            s3.client = Client::from_conf(s3_config("eu-west-1").credentials_provider(provider).http_client(no_requests.clone()).build());
            s3.list_buckets(None).await.unwrap_err().failure
        };
        let modern = Some("s01-test-session");
        assert_eq!(run(profile(None)).await, Failure::SsoLoginRequired);
        assert_eq!(run(profile(modern)).await, Failure::SsoLoginRequired);
        let token_file = cache.join("2cfabe17813c5b0a714b9e3acdd164d8aa71252c.json");
        std::fs::write(&token_file, r#"{"accessToken":"test-only","expiresAt":"2000-01-01T00:00:00Z"}"#).unwrap();
        assert_eq!(run(profile(modern)).await, Failure::SsoLoginRequired);
        std::fs::write(token_file, "not-json").unwrap();
        assert_eq!(run(profile(modern)).await, Failure::Credentials);
        assert_eq!(no_requests.actual_requests().count(), 0);
    });
}
