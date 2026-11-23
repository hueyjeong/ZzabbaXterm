use std::{
    collections::VecDeque,
    future::poll_fn,
    io::{Read, Write},
    net::TcpListener,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use aws_smithy_runtime_api::client::{
    dns::{DnsFuture, ResolveDns},
    http::{HttpConnector, HttpConnectorFuture, SharedHttpConnector, http_client_fn},
    interceptors::context::BeforeTransmitInterceptorContextMut,
    orchestrator::HttpRequest,
    result::ConnectorError,
};
use http_body::Body;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

use crate::editor::storage_files::open_upload;
use super::*;

const REGION: &str = "test-region-1";
const SERVICE: &str = "store-test";
const ITEM: &str = "vault-item-01";
const KEY: &str = "S02_TEST_ONLY_ACCESS_KEY";
const SECRET: &str = "S02_TEST_ONLY_SECRET";

fn alias() -> ServiceKeyAlias {
    ServiceKeyAlias::new("시험 서비스 키".into()).unwrap()
}

fn service(id: &str, endpoint: &str, style: AddressStyle) -> CompatibleService {
    CompatibleService::new(id.into(), endpoint, REGION.into(), style).unwrap()
}

fn fake_key(scope: &ServiceKeyScope, item: &str) -> Result<ServiceKey, KeyAccessError> {
    assert_eq!(scope.service_id(), SERVICE);
    assert_eq!(item, ITEM);
    ServiceKey::new(KEY.into(), SECRET.into())
}

#[derive(Debug, PartialEq, Eq)]
struct Seen {
    uri: String,
    method: String,
    expected_key_and_region: bool,
    body: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
struct Recorder(Arc<Mutex<(Vec<Seen>, VecDeque<HttpResponse>)>>);

impl Recorder {
    fn with(responses: impl IntoIterator<Item = HttpResponse>) -> Self {
        Self(Arc::new(Mutex::new((
            Vec::new(),
            responses.into_iter().collect(),
        ))))
    }

    fn transport(&self) -> SharedHttpClient {
        let recorder = self.clone();
        http_client_fn(move |_, _| SharedHttpConnector::new(recorder.clone()))
    }

    fn seen(&self) -> Vec<Seen> {
        std::mem::take(&mut self.0.lock().unwrap().0)
    }
}

impl HttpConnector for Recorder {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let state = Arc::clone(&self.0);
        let expected_key_and_region = request.headers().get("authorization").is_some_and(|value| {
            value
                .strip_prefix("AWS4-HMAC-SHA256 Credential=")
                .and_then(|value| value.split_once(','))
                .is_some_and(|(scope, _)| {
                    let mut fields = scope.split('/');
                    fields.next() == Some(KEY)
                        && fields.next().is_some()
                        && fields.next() == Some(REGION)
                        && fields.next() == Some("s3")
                        && fields.next() == Some("aws4_request")
                        && fields.next().is_none()
                })
                && !value.contains(SECRET)
                && request.headers().get("x-amz-security-token").is_none()
        });
        let uri = request.uri().to_owned();
        let method = request.method().to_owned();
        let mut stream = request.into_body();
        HttpConnectorFuture::new(async move {
            let mut body = Vec::new();
            while let Some(frame) = poll_fn(|cx| Pin::new(&mut stream).poll_frame(cx)).await {
                if let Ok(bytes) = frame.map_err(ConnectorError::user)?.into_data() {
                    body.extend(bytes);
                }
            }
            let mut state = state.lock().unwrap();
            state.0.push(Seen {
                uri,
                method,
                expected_key_and_region,
                body,
            });
            state
                .1
                .pop_front()
                .ok_or_else(|| ConnectorError::user("unexpected test request".into()))
        })
    }
}

fn response(status: u16, code: &'static str, redirect: bool) -> HttpResponse {
    let mut response = http::Response::builder()
        .status(status)
        .header("content-type", "application/xml");
    if redirect {
        response = response
            .header("location", "https://unapproved.invalid/sample")
            .header("x-amz-bucket-region", "elsewhere");
    }
    response
        .body(aws_smithy_types::body::SdkBody::from(code))
        .unwrap()
        .try_into()
        .unwrap()
}

#[test]
fn rejects_ambiguous_https_destinations_and_invalid_fields() {
    for endpoint in [
        "http://store.example.test",
        "https:///store.example.test",
        "https://",
        "https://store.example.test:0",
        "https://store.example.test:65536",
        "https://store.example.test:",
        "https://store.example.test:abc",
        "https://user:password@store.example.test",
        "https://@store.example.test",
        "https://store.example.test?",
        "https://store.example.test/#",
        "https://store.example.test/s3",
        "https://store.example.test//",
        "https://store.example.test\\s3",
        "https://store%2eexample.test",
        "https://store..example.test",
        "https://-store.example.test",
        "https://store.example.test.",
        "https://store_example.test",
        "https://store.example.test ",
        "https://127.0.0.1",
        "https://[::1]",
    ] {
        assert_eq!(
            CompatibleService::new(
                SERVICE.into(),
                endpoint,
                REGION.into(),
                AddressStyle::VirtualHosted
            )
            .unwrap_err(),
            ConfigError::Endpoint,
            "{endpoint}"
        );
    }
    for region in ["", "-test", "test--1", "Test-1", "test/1", "us-east-1 "] {
        assert_eq!(
            CompatibleService::new(
                SERVICE.into(),
                "https://store.example.test",
                region.into(),
                AddressStyle::Path
            )
            .unwrap_err(),
            ConfigError::Region
        );
    }
    let selected = service(
        SERVICE,
        "https://STORE.example.test:443/",
        AddressStyle::Path,
    );
    assert_eq!(selected.scope().origin(), "https://store.example.test");
    assert_eq!(selected.region(), REGION);
    assert_eq!(
        ServiceKeyRef::new(&selected, "../aws".into()).unwrap_err(),
        ConfigError::KeyId
    );
    for name in ["", " ", "line\nbreak", "\x1b[31m", "\u{0085}"] {
        assert_eq!(
            ServiceKeyAlias::new(name.into()).unwrap_err(),
            ConfigError::KeyAlias
        );
    }
    assert!(matches!(
        ServiceKey::new(KEY.into(), String::new()),
        Err(KeyAccessError::Invalid)
    ));
    assert_eq!(
        CompatibleService::new(
            "".into(),
            selected.scope().origin(),
            REGION.into(),
            AddressStyle::Path
        )
        .unwrap_err(),
        ConfigError::ServiceId
    );
    let ipv6 = service(SERVICE, "https://[::1]:9443", AddressStyle::Path);
    assert_eq!(ipv6.scope().origin(), "https://[::1]:9443");
}

#[test]
fn wrong_scope_and_vault_failure_never_create_transport() {
    let selected = service(SERVICE, "https://store.example.test", AddressStyle::Path);
    let reference = ServiceKeyRef::new(&selected, ITEM.into()).unwrap();
    for wrong in [
        service(
            "other-service",
            selected.scope().origin(),
            AddressStyle::Path,
        ),
        service(SERVICE, "https://elsewhere.invalid", AddressStyle::Path),
        service(
            SERVICE,
            "https://store.example.test:9443",
            AddressStyle::Path,
        ),
    ] {
        assert_eq!(
            wrong
                .connect(&reference, alias(), |_, _| panic!("wrong-service lookup"))
                .unwrap_err(),
            KeyAccessError::WrongService
        );
    }
    for failure in [
        KeyAccessError::Cancelled,
        KeyAccessError::Missing,
        KeyAccessError::Locked,
        KeyAccessError::Unavailable,
    ] {
        let count = std::cell::Cell::new(0);
        let error = selected
            .connect_with(
                &reference,
                alias(),
                |scope, item| {
                    assert_eq!(scope, selected.scope());
                    assert_eq!(item, ITEM);
                    count.set(count.get() + 1);
                    Err(failure)
                },
                || panic!("transport without selected credentials"),
            )
            .unwrap_err();
        assert_eq!(error, failure);
        assert_eq!(count.get(), 1);
    }
}

#[tokio::test]
async fn both_styles_sign_and_transfer_only_to_the_selected_bucket() {
    const BODY: &[u8] = b"isolated s02 object";
    for (style, expected_host, target_prefix) in [
        (AddressStyle::Path, "store.example.test:9443", "/sample"),
        (
            AddressStyle::VirtualHosted,
            "sample.store.example.test:9443",
            "",
        ),
    ] {
        let selected = service(SERVICE, "https://store.example.test:9443", style);
        let reference = ServiceKeyRef::new(&selected, ITEM.into()).unwrap();
        let recorder = Recorder::with([
            response(
                200,
                "<ListAllMyBucketsResult><Buckets><Bucket><Name>sample</Name></Bucket></Buckets></ListAllMyBucketsResult>",
                false,
            ),
            response(200, "", false),
            response(
                200,
                "<ListBucketResult><EncodingType>url</EncodingType><Contents><Key>prefix/object</Key><Size>19</Size></Contents></ListBucketResult>",
                false,
            ),
            http::Response::builder()
                .header("content-length", BODY.len())
                .body(aws_smithy_types::body::SdkBody::from(BODY))
                .unwrap()
                .try_into()
                .unwrap(),
        ]);
        let lookups = std::cell::Cell::new(0);
        let client = selected
            .connect_with(
                &reference,
                alias(),
                |scope, item| {
                    lookups.set(lookups.get() + 1);
                    assert_eq!(scope.origin(), "https://store.example.test:9443");
                    fake_key(scope, item)
                },
                || recorder.transport(),
            )
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.txt");
        let target = dir.path().join("target.txt");
        std::fs::write(&source, BODY).unwrap();
        assert_eq!(
            client.list_buckets(None).await.unwrap().rows,
            [BucketRow { name: "sample".into(), region: REGION.into() }]
        );
        assert_eq!(
            client
                .upload("sample", "prefix/object", open_upload(&source).unwrap())
                .await
                .unwrap(),
            BODY.len() as u64
        );
        let list = client
            .list_objects("sample", "prefix/", None)
            .await
            .unwrap();
        assert_eq!(
            list.rows,
            [ObjectRow::Object {
                key: "prefix/object".into(),
                bytes: Some(19),
                last_modified: None,
                storage_class: None,
            }]
        );
        assert_eq!(
            client
                .download("sample", "prefix/object", &DownloadTarget::open(&target).unwrap(), &watch::Sender::new(false))
                .await
                .unwrap()
                .bytes,
            BODY.len() as u64
        );
        assert_eq!(std::fs::read(&target).unwrap(), BODY);

        let records = recorder.seen();
        assert_eq!(lookups.get(), 1);
        assert_eq!(records.len(), 4);
        assert!(records.iter().all(|record| record.expected_key_and_region));
        for (record, (method, host, path)) in records.iter().zip([
            ("GET", "store.example.test:9443", "/".to_owned()),
            (
                "PUT",
                expected_host,
                format!("{target_prefix}/prefix/object"),
            ),
            ("GET", expected_host, format!("{target_prefix}/")),
            (
                "GET",
                expected_host,
                format!("{target_prefix}/prefix/object"),
            ),
        ]) {
            let uri: Uri = record.uri.parse().unwrap();
            assert_eq!(record.method, method);
            assert_eq!(uri.scheme_str(), Some("https"));
            assert_eq!(uri.authority().unwrap().as_str(), host);
            assert_eq!(uri.path().trim_end_matches('/'), path.trim_end_matches('/'));
        }
        // 파일 스트림은 SDK가 체크섬 트레일러를 붙인 aws-chunked 본문으로 보낸다.
        let size = format!("{:x}\r\n", BODY.len());
        let payload = records[1].body.strip_prefix(size.as_bytes()).unwrap();
        let trailer = payload.strip_prefix(BODY).unwrap();
        assert!(trailer.starts_with(b"\r\n0\r\nx-amz-checksum-"));
    }
}

#[derive(Debug)]
struct ChangeBeforeSigning(&'static str);

impl Intercept for ChangeBeforeSigning {
    fn name(&self) -> &'static str {
        "ChangeBeforeSigning"
    }

    fn modify_before_signing(
        &self,
        context: &mut BeforeTransmitInterceptorContextMut<'_>,
        _: &RuntimeComponents,
        _: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        context.request_mut().set_uri(self.0)?;
        Ok(())
    }

    fn read_after_signing(
        &self,
        _: &BeforeTransmitInterceptorContextRef<'_>,
        _: &RuntimeComponents,
        _: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        panic!("destination rejection must happen before signing")
    }
}

#[tokio::test]
async fn blocks_sdk_path_fallback_and_destination_mutations_before_signing() {
    let selected = service(
        SERVICE,
        "https://store.example.test",
        AddressStyle::VirtualHosted,
    );
    let reference = ServiceKeyRef::new(&selected, ITEM.into()).unwrap();
    let recorder = Recorder::default();
    let client = selected
        .connect_with(&reference, alias(), fake_key, || recorder.transport())
        .unwrap();
    let error = client
        .list_objects("dotted.sample", "", None)
        .await
        .unwrap_err();
    assert_eq!(error.operation.failure, Failure::Destination);

    for uri in [
        "https://sample.unapproved.invalid/",
        "https://user@sample.store.example.test/",
        "https://sample.store.example.test:8443/",
        "https://sample.store.example.test:65536/",
        "http://sample.store.example.test/",
        "https://store.example.test/sample/",
        "https://other.store.example.test/",
    ] {
        let error = client
            .operations
            .client
            .list_objects_v2()
            .bucket("sample")
            .customize()
            .interceptor(ChangeBeforeSigning(uri))
            .send()
            .await
            .unwrap_err();
        assert_eq!(request_failure(&error), Failure::Destination);
    }
    assert!(recorder.seen().is_empty());
}

#[tokio::test]
async fn stops_at_redirect_denial_and_signing_region_error() {
    const CASES: &[(u16, &str, bool)] = &[
        (307, "TemporaryRedirect", true),
        (403, "AccessDenied", false),
        (400, "AuthorizationHeaderMalformed", true),
    ];
    let selected = service(SERVICE, "https://store.example.test", AddressStyle::Path);
    let reference = ServiceKeyRef::new(&selected, ITEM.into()).unwrap();
    for (status, code, redirect) in CASES.iter().copied() {
        let xml = match code {
            "TemporaryRedirect" => {
                "<Error><Code>TemporaryRedirect</Code><Endpoint>unapproved.invalid</Endpoint></Error>"
            }
            "AccessDenied" => "<Error><Code>AccessDenied</Code></Error>",
            _ => {
                "<Error><Code>AuthorizationHeaderMalformed</Code><Region>elsewhere</Region></Error>"
            }
        };
        let recorder = Recorder::with([response(status, xml, redirect)]);
        let lookups = std::cell::Cell::new(0);
        let client = selected
            .connect_with(
                &reference,
                alias(),
                |scope, item| {
                    lookups.set(lookups.get() + 1);
                    fake_key(scope, item)
                },
                || recorder.transport(),
            )
            .unwrap();
        let error = client.list_objects("sample", "", None).await.unwrap_err();
        let expected = match status {
            307 => Failure::Redirect {
                status,
                code: Some(code.into()),
            },
            403 => Failure::Service {
                status,
                code: Some(code.into()),
            },
            _ => Failure::SigningRegion {
                status,
                code: code.into(),
            },
        };
        assert_eq!(error.operation.failure, expected);
        assert_eq!(error.target.endpoint, "https://store.example.test");
        assert_eq!(error.target.region, REGION);
        assert_eq!(error.target.key_alias.as_str(), "시험 서비스 키");
        let records = recorder.seen();
        assert_eq!(lookups.get(), 1);
        assert_eq!(records.len(), 1);
        assert!(records[0].expected_key_and_region);
        assert_eq!(
            records[0].uri.parse::<Uri>().unwrap().host(),
            Some("store.example.test")
        );
    }
}

#[tokio::test]
async fn denial_preserves_safe_code_and_target_across_operations() {
    use super::super::Stage;

    let selected = service(
        SERVICE,
        "https://STORE.example.test:443/",
        AddressStyle::Path,
    );
    let reference = ServiceKeyRef::new(&selected, ITEM.into()).unwrap();
    let header_only = http::Response::builder()
        .status(403)
        .header("x-amz-error-code", "InvalidAccessKeyId")
        .body(aws_smithy_types::body::SdkBody::empty())
        .unwrap()
        .try_into()
        .unwrap();
    let denied = || {
        response(
            403,
            "<Error><Code>AccessDenied</Code><Message>private-response-text</Message></Error>",
            false,
        )
    };
    let recorder = Recorder::with([
        header_only,
        denied(),
        denied(),
        denied(),
        response(
            403,
            "<Error><Code>Bad&#10;Code</Code><Message>private-response-text</Message></Error>",
            false,
        ),
    ]);
    let client = selected
        .connect_with(&reference, alias(), fake_key, || recorder.transport())
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("existing.txt");
    std::fs::write(&local, b"keep").unwrap();

    for (error, stage, code) in [
        (
            client.list_buckets(None).await.unwrap_err(),
            Stage::ListBuckets,
            Some("InvalidAccessKeyId"),
        ),
        (
            client.list_objects("sample", "", None).await.unwrap_err(),
            Stage::ListObjects,
            Some("AccessDenied"),
        ),
        (
            client.upload("sample", "object", open_upload(&local).unwrap()).await.unwrap_err(),
            Stage::PutObject,
            Some("AccessDenied"),
        ),
        (
            client
                .download("sample", "object", &DownloadTarget::open(&local).unwrap(), &watch::Sender::new(false))
                .await
                .unwrap_err(),
            Stage::GetObject,
            Some("AccessDenied"),
        ),
        (
            client.list_buckets(None).await.unwrap_err(),
            Stage::ListBuckets,
            None,
        ),
    ] {
        assert_eq!(error.target.as_ref(), client.target());
        assert_eq!(error.target.endpoint, "https://store.example.test");
        assert_eq!(error.target.region, REGION);
        assert_eq!(error.target.key_alias.as_str(), "시험 서비스 키");
        assert_eq!(error.operation.stage, stage);
        assert_eq!(
            error.operation.failure,
            Failure::Service {
                status: 403,
                code: code.map(str::to_owned)
            }
        );
        let displayed = format!("{error:?}");
        for value in [KEY, SECRET, ITEM, "private-response-text", "Bad"] {
            assert!(!displayed.contains(value));
        }
    }
    assert_eq!(std::fs::read(&local).unwrap(), b"keep");
    let seen = recorder.seen();
    assert_eq!(seen.len(), 5);
    assert!(seen.iter().all(|record| record.expected_key_and_region));
}

// 루프백 전용 공개 시험 인증서와 폐기 가능한 시험용 개인 키다.
const CERT: &[u8] = b"-----BEGIN CERTIFICATE-----
MIIBZjCCAQygAwIBAgIBKjAKBggqhkjOPQQDAjAUMRIwEAYDVQQDDAlsb2NhbGhv
c3QwIBcNMjAwMTAxMDAwMDAwWhgPMjEyMDAxMDEwMDAwMDBaMBQxEjAQBgNVBAMM
CWxvY2FsaG9zdDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABELtKG0g/xQ/tZ2h
X7dXisuByBadSmIG03wam1v8JjX2ny9OxtLAaRBpdyjdbDvXAzsOjo9KpRn+lnTd
l9iev5qjTTBLMCYGA1UdEQQfMB2CCWxvY2FsaG9zdIIQc2FtcGxlLmxvY2FsaG9z
dDAMBgNVHRMBAf8EAjAAMBMGA1UdJQQMMAoGCCsGAQUFBwMBMAoGCCqGSM49BAMC
A0gAMEUCIQD1Dr0CqlPgpamge6dNWkAs8l2FLJ01JtWEeZAJclGz5AIge2I1/Kd9
Rnjb/6ElaPXeW4ND1aFH95+ylR2h7QqUQe8=
-----END CERTIFICATE-----";
const TEST_PRIVATE_KEY: &[u8] = b"-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgyL2HQ042X/LYpeYY
ohThM3gMqdb4+KK3bjbK3suPv+mhRANCAARC7ShtIP8UP7WdoV+3V4rLgcgWnUpi
BtN8Gptb/CY19p8vTsbSwGkQaXco3Ww71wM7Do6PSqUZ/pZ03ZfYnr+a
-----END PRIVATE KEY-----";

#[derive(Clone, Debug)]
struct LoopbackDns(&'static str);

impl ResolveDns for LoopbackDns {
    fn resolve_dns<'a>(&'a self, name: &'a str) -> DnsFuture<'a> {
        assert_eq!(name, self.0);
        DnsFuture::ready(Ok(vec![std::net::Ipv4Addr::LOCALHOST.into()]))
    }
}

pub(in crate::storage) fn loopback_list_client(
    origin: &str, host: &'static str, path: &'static str, style: AddressStyle, trusted: bool, allowed: bool,
    service_browse: bool,
) -> (CompatibleS3, std::thread::JoinHandle<bool>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from_pem_slice(CERT).unwrap()],
            PrivateKeyDer::from_pem_slice(TEST_PRIVATE_KEY).unwrap(),
        )
        .unwrap();
        let mut tls = rustls::StreamOwned::new(
            rustls::ServerConnection::new(Arc::new(config)).unwrap(),
            stream,
        );
        let mut byte = [0];
        if tls.read(&mut byte).ok() != Some(1) {
            return false;
        }
        assert!(allowed, "HTTP reached a TLS identity that should have failed");
        let mut line = vec![byte[0]];
        while !line.ends_with(b"\r\n") {
            assert!(line.len() < 1024);
            tls.read_exact(&mut byte).unwrap();
            line.push(byte[0]);
        }
        let line = std::str::from_utf8(&line).unwrap();
        let uri: Uri = line.strip_prefix("GET ").unwrap().split_once(' ').unwrap().0.parse().unwrap();
        assert_eq!(uri.path().trim_end_matches('/'), path);
        assert_eq!(uri.query().is_some_and(|query| query.contains("list-type=2")), !service_browse);
        assert_eq!(tls.conn.server_name(), Some(host));
        // 헤더 원문을 기록하지 않고 HTTP 헤더의 끝까지만 읽는다.
        let mut tail = [0, 0, b'\r', b'\n'];
        while tail != *b"\r\n\r\n" {
            tls.read_exact(&mut byte).unwrap();
            tail.rotate_left(1);
            tail[3] = byte[0];
        }
        let body = if service_browse {
            "<ListAllMyBucketsResult><Buckets/></ListAllMyBucketsResult>"
        } else {
            "<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>false</IsTruncated></ListBucketResult>"
        };
        write!(
            tls,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        true
    });
    let selected = service(
        SERVICE,
        &format!("https://{origin}:{port}"),
        style,
    );
    let reference = ServiceKeyRef::new(&selected, ITEM.into()).unwrap();
    let mut trust = tls::TrustStore::empty();
    if trusted {
        trust = trust.with_pem_certificate(CERT);
    }
    let context = tls::TlsContext::builder()
        .with_trust_store(trust)
        .build()
        .unwrap();
    let client = selected
        .connect_with(&reference, alias(), fake_key, || {
            Builder::new().tls_provider(tls::Provider::Rustls(tls::rustls_provider::CryptoMode::AwsLc))
                .tls_context(context).build_with_resolver(LoopbackDns(host))
        })
        .unwrap();
    (client, server)
}

#[tokio::test]
async fn real_https_keeps_chain_and_name_verification_on_loopback() {
    use AddressStyle::{Path, VirtualHosted};
    for (origin, bucket, host, path, style, trusted, allowed) in [
        ("localhost", "sample", "localhost", "/sample", Path, true, true),
        ("localhost", "sample", "sample.localhost", "", VirtualHosted, true, true),
        ("localhost", "other", "other.localhost", "", VirtualHosted, true, false),
        ("127.0.0.1", "sample", "127.0.0.1", "", Path, true, false),
        ("localhost", "sample", "localhost", "", Path, false, false),
    ] {
        let (client, server) = loopback_list_client(origin, host, path, style, trusted, allowed, false);
        let result = tokio::time::timeout(Duration::from_secs(5), client.list_objects(bucket, "", None))
            .await
            .unwrap();
        if allowed {
            result.unwrap();
        } else {
            assert_eq!(result.unwrap_err().operation.failure, Failure::Tls);
        }
        assert_eq!(server.join().unwrap(), allowed);
    }
}
