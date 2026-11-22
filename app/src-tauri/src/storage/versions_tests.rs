use super::*;
use crate::storage::tests::{client, replay, response};

fn page(truncated: bool, entries: &str) -> String {
    format!("<ListVersionsResult><EncodingType>url</EncodingType><IsTruncated>{truncated}</IsTruncated>{entries}</ListVersionsResult>")
}

fn version(key: &str, id: &str, latest: bool) -> String {
    format!("<Version><Key>{key}</Key><VersionId>{id}</VersionId><IsLatest>{latest}</IsLatest></Version>")
}

#[tokio::test]
async fn counts_current_files_across_pages_in_the_verified_region() {
    let key = "%ED%95%9C%2Fa%2B%252F";
    let first = format!(
        "<NextKeyMarker>{key}</NextKeyMarker><NextVersionIdMarker>v+%2F</NextVersionIdMarker>{}",
        version(key, "v+%2F", true),
    );
    let second = [
        version(key, "old", false),
        version(key, "null", false),
        version("%ED%95%9C%2Fb", "now", true),
        "<DeleteMarker><Key>%ED%95%9C%2Fb</Key><VersionId>old-delete</VersionId><IsLatest>false</IsLatest></DeleteMarker>".into(),
        version("%ED%95%9C%2Fc", "old", false),
        "<DeleteMarker><Key>%ED%95%9C%2Fc</Key><VersionId>gone</VersionId><IsLatest>true</IsLatest></DeleteMarker>".into(),
        version("%ED%95%9C%2F", "now", true),
        version("%ED%95%9C%2F", "old", false),
        version("%ED%95%9C%2Fsub%2Fd", "now", true),
        version("%ED%95%9C%2Fsub%2Fd", "old", false),
        "<CommonPrefixes><Prefix>%ED%95%9C%2Fsub%2F</Prefix></CommonPrefixes>".into(),
    ].concat();
    let replay = replay([response(200, page(true, &first)), response(200, page(false, &second))]);
    let bound = client(&replay);
    let regional = bound.in_region("eu-central-1", STAGE).unwrap();
    assert_eq!(regional.count_previous_files("selected-bucket", "한/").await, Ok(1));
    let requests: Vec<_> = replay.actual_requests().collect();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let uri: http::Uri = request.uri().parse().unwrap();
        assert_eq!(uri.host(), Some("s3.eu-central-1.amazonaws.com"));
        assert_eq!(uri.path(), "/selected-bucket/");
        for part in ["versions", "delimiter=%2F", "prefix=%ED%95%9C%2F", "encoding-type=url"] {
            assert!(uri.query().unwrap().split('&').any(|actual| actual == part || actual.strip_suffix('=') == Some(part)));
        }
        let authorization = request.headers().get("authorization").unwrap();
        assert!(authorization.contains("Credential=test-key/"));
        assert!(authorization.contains("/eu-central-1/s3/aws4_request"));
    }
    let uri: http::Uri = requests[1].uri().parse().unwrap();
    let query = uri.query().unwrap();
    assert!(query.split('&').any(|part| part == "key-marker=%ED%95%9C%2Fa%2B%252F"));
    assert!(query.split('&').any(|part| part == "version-id-marker=v%2B%252F"));
}

#[tokio::test]
async fn incomplete_or_denied_scans_return_errors() {
    let marker = "<NextKeyMarker>a</NextKeyMarker><NextVersionIdMarker>first</NextVersionIdMarker>";
    let looping = replay([response(200, page(true, marker)), response(200, page(true, marker))]);
    assert_eq!(client(&looping).count_previous_files("selected-bucket", "").await, Err(invalid()));
    for malformed in [
        page(true, ""),
        page(true, "<NextKeyMarker>a</NextKeyMarker>"),
        page(false, &version("invalid%2x", "id", true)),
        "<ListVersionsResult><EncodingType>url</EncodingType></ListVersionsResult>".into(),
    ] {
        let fixture = replay([response(200, malformed)]);
        assert_eq!(client(&fixture).count_previous_files("selected-bucket", "").await, Err(invalid()));
    }
    let denied = replay([response(403, "<Error><Code>AccessDenied</Code></Error>")]);
    assert_eq!(client(&denied).count_previous_files("selected-bucket", "").await.unwrap_err().failure,
        Failure::Service { status: 403, code: Some("AccessDenied".into()) });
}

#[tokio::test]
async fn page_budget_never_returns_a_partial_count() {
    let fixture = replay((0..MAX_PAGES).map(|i| response(200, page(true, &format!(
        "<NextKeyMarker>a</NextKeyMarker><NextVersionIdMarker>{i}</NextVersionIdMarker>",
    )))));
    assert_eq!(client(&fixture).count_previous_files("selected-bucket", "").await, Err(invalid()));
    assert_eq!(fixture.actual_requests().count(), MAX_PAGES);
}

#[tokio::test]
async fn common_prefix_page_can_advance_without_a_version_marker() {
    let fixture = replay([
        response(200, page(true, "<NextKeyMarker>a%2F</NextKeyMarker><CommonPrefixes><Prefix>a%2F</Prefix></CommonPrefixes>")),
        response(200, page(false, "")),
    ]);
    assert_eq!(client(&fixture).count_previous_files("selected-bucket", "").await, Ok(0));
    let uri: http::Uri = fixture.actual_requests().nth(1).unwrap().uri().parse().unwrap();
    let query = uri.query().unwrap();
    assert!(query.split('&').any(|part| part == "key-marker=a%2F"));
    assert!(!query.split('&').any(|part| part.starts_with("version-id-marker=")));
}

#[tokio::test]
async fn a_final_page_missing_its_closing_tag_is_an_error() {
    let complete = page(false, &format!("{}{}", version("a", "new", true), version("a", "old", false)));
    let truncated = complete.strip_suffix("</ListVersionsResult>").unwrap();
    let fixture = replay([response(200, truncated.to_owned())]);
    assert!(client(&fixture).count_previous_files("selected-bucket", "").await.is_err());
}
