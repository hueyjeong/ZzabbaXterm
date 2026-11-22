use super::*;
use aws_sdk_s3::primitives::SdkBody;
use crate::storage::tests::{client, replay, response};

const BUCKET: &str = "selected-bucket";
const KEY: &str = "실험//../한글 +%2F.txt";
const PATH: &str = "/selected-bucket/%EC%8B%A4%ED%97%98//../%ED%95%9C%EA%B8%80%20%2B%252F.txt";
const ETAG: &str = "\"opaque-first+etag/1\"";
const NEXT_ETAG: &str = "\"opaque-next+etag/2\"";
const VERSION: &str = "v+%/1";
const NEXT_VERSION: &str = "v+%/2";

fn bucket_checks() -> [HttpResponse; 3] {
    [
        response(200, "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"),
        response(200, "<OwnershipControls><Rule><ObjectOwnership>BucketOwnerEnforced</ObjectOwnership></Rule></OwnershipControls>"),
        response(404, "<Error><Code>ObjectLockConfigurationNotFoundError</Code></Error>"),
    ]
}

fn tag_response(version: &str, xml: &str) -> HttpResponse {
    http::Response::builder().header("x-amz-version-id", version)
        .body(SdkBody::from(xml.to_owned())).unwrap().try_into().unwrap()
}

fn annotation_response(version: &str, xml: &str) -> HttpResponse {
    http::Response::builder().header("x-amz-object-version-id", version)
        .body(SdkBody::from(xml.to_owned())).unwrap().try_into().unwrap()
}

fn empty_properties(version: &str) -> [HttpResponse; 2] {
    [
        tag_response(version, "<Tagging><TagSet/></Tagging>"),
        annotation_response(version, "<ListObjectAnnotationsOutput><AnnotationCount>0</AnnotationCount></ListObjectAnnotationsOutput>"),
    ]
}

fn object(body: impl Into<SdkBody>, length: usize, tag: &str, version: &str, extra: &[(&str, &str)]) -> HttpResponse {
    let mut output = http::Response::builder()
        .header("content-type", "text/plain; charset=UTF-8")
        .header("content-language", "ko")
        .header("content-disposition", "inline; filename=example.txt")
        .header("cache-control", "private, max-age=300")
        .header("expires", "Sat, 14 Nov 2026 00:00:00 GMT")
        .header("x-amz-website-redirect-location", "/another-object")
        .header("x-amz-meta-source", "editor test")
        .header("x-amz-meta-empty", "")
        .header("x-amz-server-side-encryption", "AES256")
        .header("content-length", length)
        .header("etag", tag)
        .header("x-amz-version-id", version);
    for &(name, value) in extra {
        output = output.header(name, value);
    }
    output.body(body.into()).unwrap().try_into().unwrap()
}

fn open_responses(text: &str) -> impl Iterator<Item = HttpResponse> {
    bucket_checks().into_iter()
        .chain([object(text.to_owned(), text.len(), ETAG, VERSION, &[])])
        .chain(empty_properties(VERSION))
}

fn preflight(length: usize, tag: &str, version: &str) -> impl Iterator<Item = HttpResponse> {
    bucket_checks().into_iter().chain(empty_properties(version))
        .chain([object(SdkBody::empty(), length, tag, version, &[])])
}

fn put_response(tag: &str, version: &str) -> HttpResponse {
    http::Response::builder().header("etag", tag).header("x-amz-version-id", version)
        .body(SdkBody::empty()).unwrap().try_into().unwrap()
}

#[tokio::test]
async fn saves_exact_key_and_preserved_headers_then_uses_the_new_revision() {
    let first = "\u{feff}한글\r\n";
    let second = "수정한 한글\n\t";
    let transport = replay(open_responses(first)
        .chain(preflight(first.len(), ETAG, VERSION))
        .chain([put_response(NEXT_ETAG, NEXT_VERSION)])
        .chain(preflight(second.len(), NEXT_ETAG, NEXT_VERSION))
        .chain([put_response(NEXT_ETAG, "v3")]));
    let s3 = client(&transport);
    let regional = s3.in_region("eu-central-1", Stage::GetObject).unwrap().into_owned();
    let opened = regional.read_object_for_edit(BUCKET, KEY).await.unwrap();
    assert_eq!(opened.text, first);
    let saved = regional.clone().save_edited_object(&opened.revision, second.into()).await.unwrap();
    assert_eq!(saved.e_tag, NEXT_ETAG);
    assert_eq!(saved.version_id, NEXT_VERSION);
    let saved_again = regional.save_edited_object(&saved, second.into()).await.unwrap();
    assert_eq!(saved_again.e_tag, NEXT_ETAG);
    assert_eq!(saved_again.version_id, "v3");

    let requests: Vec<_> = transport.actual_requests().collect();
    assert_eq!(requests.len(), 20);
    assert!(requests.iter().all(|request| request.uri().starts_with("https://s3.eu-central-1.amazonaws.com/")));
    assert_eq!(requests[3].uri().parse::<http::Uri>().unwrap().path(), PATH);
    for (tag_request, annotation_request, version) in [(4, 5, VERSION), (9, 10, VERSION), (16, 17, NEXT_VERSION)] {
        for (index, subresource) in [(tag_request, "tagging"), (annotation_request, "annotation")] {
            let uri: http::Uri = requests[index].uri().parse().unwrap();
            assert_eq!(uri.path(), PATH);
            let query = uri.query().unwrap();
            assert!(query.split('&').any(|parameter| parameter.trim_end_matches('=') == subresource));
            let version = version.strip_prefix("v+%/").unwrap();
            assert!(query.split('&').any(|parameter| parameter == format!("versionId=v%2B%25%2F{version}")));
        }
    }
    for (head, put, tag) in [(11, 12, ETAG), (18, 19, NEXT_ETAG)] {
        assert_eq!(requests[head].method(), "HEAD");
        assert_eq!(requests[head].headers().get("if-match"), Some(tag));
        assert_eq!(requests[put].method(), "PUT");
        let uri: http::Uri = requests[put].uri().parse().unwrap();
        assert_eq!(uri.path(), PATH);
        assert_eq!(uri.query(), Some("x-id=PutObject"));
        let headers = requests[put].headers();
        for (name, value) in [
            ("if-match", tag),
            ("content-type", "text/plain; charset=UTF-8"),
            ("content-language", "ko"),
            ("content-disposition", "inline; filename=example.txt"),
            ("cache-control", "private, max-age=300"),
            ("expires", "Sat, 14 Nov 2026 00:00:00 GMT"),
            ("x-amz-website-redirect-location", "/another-object"),
            ("x-amz-meta-source", "editor test"),
            ("x-amz-meta-empty", ""),
            ("x-amz-storage-class", "STANDARD"),
            ("x-amz-server-side-encryption", "AES256"),
        ] {
            assert_eq!(headers.get(name), Some(value), "{name}");
        }
        assert_eq!(requests[put].body().bytes(), Some(second.as_bytes()));
    }
}

#[tokio::test]
async fn write_rejections_and_unconfirmed_responses_are_distinct_and_not_retried() {
    for (reply, expected) in [
        (response(409, "<Error><Code>ConditionalRequestConflict</Code></Error>"), EditError::Conflict { status: 409 }),
        (response(412, "<Error><Code>PreconditionFailed</Code></Error>"), EditError::Conflict { status: 412 }),
        (response(404, "<Error><Code>NoSuchKey</Code></Error>"), EditError::Conflict { status: 404 }),
        (response(403, "<Error><Code>AccessDenied</Code></Error>"), EditError::Storage { error: Error::at(Stage::PutObject, Failure::Service { status: 403, code: Some("AccessDenied".into()) }) }),
        (response(503, "<Error><Code>SlowDown</Code></Error>"), EditError::Unconfirmed),
        (put_response("*", NEXT_VERSION), EditError::Unconfirmed),
        (put_response(NEXT_ETAG, VERSION), EditError::Unconfirmed),
        (response(200, ""), EditError::Unconfirmed),
    ] {
        let transport = replay(open_responses("text").chain(preflight(4, ETAG, VERSION)).chain([reply]));
        let s3 = client(&transport);
        let opened = s3.read_object_for_edit(BUCKET, KEY).await.unwrap();
        let error = s3.save_edited_object(&opened.revision, "after".into()).await.unwrap_err();
        assert_eq!(error, expected);
        assert_eq!(transport.actual_requests().filter(|request| request.method() == "PUT").count(), 1);
    }
    let timeout = SdkError::timeout_error(std::io::Error::new(std::io::ErrorKind::TimedOut, "fixture"));
    assert_eq!(put_error(timeout), EditError::Unconfirmed);
}

#[tokio::test]
async fn changed_version_and_other_client_never_put() {
    let transport = replay(open_responses("text")
        .chain(bucket_checks())
        .chain(empty_properties(VERSION))
        .chain([object(SdkBody::empty(), 4, ETAG, NEXT_VERSION, &[])]));
    let s3 = client(&transport);
    let opened = s3.read_object_for_edit(BUCKET, KEY).await.unwrap();
    let other = client(&transport);
    assert!(matches!(other.save_edited_object(&opened.revision, "after".into()).await,
        Err(EditError::Storage { error: Error { stage: Stage::PutObject, failure: Failure::Request, .. } })));
    assert_eq!(transport.actual_requests().count(), 6);
    assert_eq!(s3.save_edited_object(&opened.revision, "after".into()).await.unwrap_err(), EditError::SnapshotChanged);
    assert_eq!(transport.actual_requests().count(), 12);
    assert!(!transport.actual_requests().any(|request| request.method() == "PUT"));
}

#[tokio::test]
async fn editable_object_class_requires_positive_metadata_evidence() {
    let mut denied_checks = bucket_checks().into_iter();
    let transport = replay(denied_checks.by_ref().take(2).chain([response(403, "<Error><Code>AccessDenied</Code></Error>")]));
    let error = client(&transport).read_object_for_edit(BUCKET, KEY).await.unwrap_err();
    assert!(matches!(error, EditError::Storage { error: Error { failure: Failure::Service { status: 403, .. }, .. } }));
    assert_eq!(transport.actual_requests().count(), 3);

    let invalid_responses = [
        (object("x", 1, ETAG, VERSION, &[("x-amz-missing-meta", "1")]), Restriction::ObjectProperties),
        (object("x", 1, ETAG, VERSION, &[("x-amz-checksum-type", "COMPOSITE")]), Restriction::ObjectProperties),
        (object("x", 1, ETAG, VERSION, &[("x-amz-storage-class", "STANDARD_IA")]), Restriction::ObjectProperties),
        (object("x", 1, ETAG, VERSION, &[("content-encoding", "gzip")]), Restriction::ContentEncoding),
        (object("x", 1, ETAG, "null", &[]), Restriction::Versioning),
        (object("", MAX_EDIT_BYTES + 1, ETAG, VERSION, &[]), Restriction::TooLarge),
    ];
    for (reply, reason) in invalid_responses {
        let transport = replay(bucket_checks().into_iter().chain([reply]));
        assert_eq!(client(&transport).read_object_for_edit(BUCKET, KEY).await.unwrap_err(), unsupported(reason));
        assert_eq!(transport.actual_requests().count(), 4);
    }
    let transport = replay(bucket_checks().into_iter().chain([
        object("", 0, ETAG, VERSION, &[]),
        tag_response(VERSION, "<Tagging><TagSet><Tag><Key>retention</Key><Value>yes</Value></Tag></TagSet></Tagging>"),
    ]));
    assert_eq!(client(&transport).read_object_for_edit(BUCKET, KEY).await.unwrap_err(), unsupported(Restriction::Tags));

    let transport = replay(bucket_checks().into_iter().chain([
        object("", 0, ETAG, VERSION, &[]),
        tag_response(VERSION, "<Tagging><TagSet/></Tagging>"),
        annotation_response(VERSION, "<ListObjectAnnotationsOutput><AnnotationCount>0</AnnotationCount><NextContinuationToken>more</NextContinuationToken></ListObjectAnnotationsOutput>"),
    ]));
    assert_eq!(client(&transport).read_object_for_edit(BUCKET, KEY).await.unwrap_err(), unsupported(Restriction::Annotations));
}

#[tokio::test]
async fn body_is_exact_bounded_utf8_text() {
    assert_eq!(read_text(ByteStream::from_static(b""), 0).await.unwrap(), "");
    assert_eq!(read_text(ByteStream::from_static(b"\xEF\xBB\xBF\r\n\t"), 6).await.unwrap(), "\u{feff}\r\n\t");
    for (body, expected) in [(b"ab".as_slice(), 1), (b"a".as_slice(), 2)] {
        assert!(matches!(read_text(ByteStream::from_static(body), expected).await,
            Err(EditError::Storage { error: Error { failure: Failure::Length { .. }, .. } })));
    }
    for body in [b"\xFF".as_slice(), b"\0", b"a\x1B"] {
        assert_eq!(read_text(ByteStream::from_static(body), body.len()).await, Err(unsupported(Restriction::Text)));
    }
    assert_eq!(validate_text(&"한".repeat(MAX_EDIT_BYTES / 3 + 1)), Err(unsupported(Restriction::TooLarge)));
    assert!(edit_length(Some(MAX_EDIT_BYTES as i64)).is_ok());
    assert_eq!(edit_length(None), Err(invalid_read()));
}

#[test]
fn accepts_opaque_etags_but_not_header_conditions() {
    for tag in [ETAG, "\"opaque\"", "\"*\"", "\"t,ag\""] {
        assert!(valid_etag(tag), "{tag:?}");
    }
    for tag in ["", "*", "opaque", "W/\"tag\"", "\"\"", "\"one\", \"two\"", " \"tag\"", "\"tag\"\r\n\"other\""] {
        assert!(!valid_etag(tag), "{tag:?}");
    }
}
