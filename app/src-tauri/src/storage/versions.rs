use std::{borrow::Cow, collections::{HashMap, HashSet}, time::Duration};

use aws_sdk_s3::types::EncodingType;
use aws_smithy_runtime_api::{
    box_error::BoxError,
    client::{
        interceptors::{Intercept, context::AfterDeserializationInterceptorContextRef},
        runtime_components::RuntimeComponents,
    },
};
use aws_smithy_types::config_bag::ConfigBag;
use percent_encoding::percent_decode_str;

use super::{AwsS3, Error, Failure, Stage, check_bucket, sdk_error};

const STAGE: Stage = Stage::ListObjects;
const MAX_PAGES: usize = 100;
const SCAN_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Default)]
struct History {
    current: bool,
    previous: bool,
    deleted: bool,
}

impl AwsS3 {
    pub(super) async fn count_previous_files(&self, bucket: &str, prefix: &str) -> Result<usize, Error> {
        check_bucket(bucket, STAGE)?;
        tokio::time::timeout(SCAN_TIMEOUT, self.scan_versions(bucket, prefix))
            .await
            .map_err(|_| Error::at(STAGE, Failure::TimedOut))?
    }

    async fn scan_versions(&self, bucket: &str, prefix: &str) -> Result<usize, Error> {
        let mut files = HashMap::<String, History>::new();
        let mut seen = HashSet::new();
        let mut marker: Option<(String, Option<String>)> = None;
        for _ in 0..MAX_PAGES {
            let request = self.client.list_object_versions()
                .bucket(bucket)
                .prefix(prefix)
                .delimiter("/")
                .encoding_type(EncodingType::Url);
            let request = match marker.take() {
                Some((key, version)) => request.key_marker(key).set_version_id_marker(version),
                None => request,
            };
            let page = request.customize().interceptor(CompleteXml).send().await
                .map_err(|error| sdk_error(STAGE, error))?;
            if page.encoding_type() != Some(&EncodingType::Url) {
                return Err(invalid());
            }
            let truncated = page.is_truncated().ok_or_else(invalid)?;
            let key_marker = page.next_key_marker().filter(|key| !key.is_empty());
            let version_marker = page.next_version_id_marker().filter(|version| !version.is_empty());
            if truncated {
                let key = decode_key(key_marker.ok_or_else(invalid)?)?.into_owned();
                // 하위 접두어에는 버전 ID가 없다.
                if version_marker.is_none() && !key.ends_with('/') {
                    return Err(invalid());
                }
                let next = (key, version_marker.map(str::to_owned));
                if !seen.insert(next.clone()) {
                    return Err(invalid());
                }
                marker = Some(next);
            } else if key_marker.is_some() || version_marker.is_some() {
                return Err(invalid());
            }

            for version in page.versions() {
                let key = decode_key(version.key().ok_or_else(invalid)?)?;
                if !is_file(prefix, &key) { continue; }
                let latest = version.is_latest().ok_or_else(invalid)?;
                if version.version_id().is_none_or(str::is_empty) {
                    return Err(invalid());
                }
                let history = files.entry(key.into_owned()).or_default();
                if latest { history.current = true; } else { history.previous = true; }
            }
            for deleted in page.delete_markers() {
                let key = decode_key(deleted.key().ok_or_else(invalid)?)?;
                if is_file(prefix, &key) && deleted.is_latest().ok_or_else(invalid)? {
                    files.entry(key.into_owned()).or_default().deleted = true;
                }
            }
            if !truncated {
                return Ok(files.values().filter(|file| file.current && file.previous && !file.deleted).count());
            }
        }
        Err(invalid())
    }
}

#[derive(Debug)]
struct CompleteXml;

impl Intercept for CompleteXml {
    fn name(&self) -> &'static str { "CompleteVersionListingXml" }

    fn read_after_deserialization(
        &self,
        context: &AfterDeserializationInterceptorContextRef<'_>,
        _: &RuntimeComponents,
        _: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let response = context.response();
        // SDK 파서는 루트 닫는 태그 없이 EOF가 와도 앞부분을 반환할 수 있다.
        if response.status().as_u16() == 200
            && !response.body().bytes().is_some_and(|body| {
                body.trim_ascii_end().ends_with(b"</ListVersionsResult>")
            })
        {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "잘린 S3 버전 목록 응답").into());
        }
        Ok(())
    }
}

fn is_file(prefix: &str, key: &str) -> bool {
    key.strip_prefix(prefix).is_some_and(|name| !name.is_empty() && !name.contains('/'))
}

fn decode_key(encoded: &str) -> Result<Cow<'_, str>, Error> {
    if encoded.bytes().enumerate().any(|(i, byte)| {
        byte == b'%' && !encoded.as_bytes().get(i + 1..i + 3)
            .is_some_and(|digits| digits.iter().all(u8::is_ascii_hexdigit))
    }) {
        return Err(invalid());
    }
    percent_decode_str(encoded).decode_utf8().map_err(|_| invalid())
}

fn invalid() -> Error {
    Error::at(STAGE, Failure::InvalidResponse)
}

#[cfg(test)]
#[path = "versions_tests.rs"]
mod tests;
