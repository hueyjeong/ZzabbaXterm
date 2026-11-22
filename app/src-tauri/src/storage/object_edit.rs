use std::{
    collections::HashMap,
    error::Error as StdError,
    sync::Arc,
    time::Duration,
};

use aws_sdk_s3::{
    Client,
    error::{ProvideErrorMetadata, SdkError},
    operation::put_object::PutObjectError,
    primitives::ByteStream,
    types::{BucketVersioningStatus, ChecksumAlgorithm, ChecksumMode, ChecksumType, ObjectOwnership, ServerSideEncryption, StorageClass},
};
use aws_smithy_runtime::client::http::body::content_length_enforcement::ContentLengthError;
use aws_smithy_runtime_api::{
    box_error::BoxError,
    client::{
        interceptors::{Intercept, context::AfterDeserializationInterceptorContextRef},
        orchestrator::HttpResponse,
        runtime_components::RuntimeComponents,
    },
};
use aws_smithy_types::{DateTime, config_bag::ConfigBag, date_time::Format};

use super::{AwsS3, Error, Failure, Stage, check_bucket, sdk_error, single_attempt};

const MAX_EDIT_BYTES: usize = 1024 * 1024;
const BODY_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(super) enum EditError {
    Storage { error: Error },
    Unsupported { reason: EditRestriction },
    Conflict { status: u16 },
    // 저장 전 관측이 다른 버전이나 속성을 가리킨다.
    SnapshotChanged,
    // PUT이 먼저 반영됐을 수 있다. 같은 리비전으로 재시도하기 전에 다시 읽어야 한다.
    Unconfirmed,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum EditRestriction {
    Key,
    TooLarge,
    Text,
    ContentEncoding,
    Versioning,
    Ownership,
    ObjectLock,
    ObjectProperties,
    Tags,
    Annotations,
}

impl EditRestriction {
    pub(super) fn message(&self) -> &'static str {
        match self {
            Self::Key => "S3 객체 키 길이가 1~1,024바이트 범위를 벗어났습니다",
            Self::TooLarge => "편집 용량 한도는 1 MiB입니다",
            Self::Text => "UTF-8 평문만 편집할 수 있습니다",
            Self::ContentEncoding => "압축되거나 인코딩된 객체는 편집할 수 없습니다",
            Self::Versioning => "현재 객체 버전 또는 버킷의 버전 관리 활성 상태를 확인할 수 없습니다",
            Self::Ownership => "버킷 소유자 강제 적용이 필요합니다",
            Self::ObjectLock => "Object Lock이 적용된 버킷은 편집할 수 없습니다",
            Self::ObjectProperties => "이 객체의 저장 속성을 보존할 수 없습니다",
            Self::Tags => "태그가 있는 객체는 편집할 수 없습니다",
            Self::Annotations => "주석이 없는 객체만 편집할 수 있습니다",
        }
    }
}

use EditRestriction as Restriction;

#[derive(Debug)]
pub(super) struct EditableObject {
    pub(super) text: String,
    pub(super) revision: EditRevision,
}

#[derive(Debug)]
pub(super) struct EditRevision {
    target: Arc<EditTarget>,
    e_tag: String,
    version_id: String,
    bytes: usize,
}

#[derive(Debug)]
struct EditTarget {
    client: Client,
    bucket: String,
    key: String,
    content_type: Option<String>,
    content_encoding: Option<String>,
    content_language: Option<String>,
    content_disposition: Option<String>,
    cache_control: Option<String>,
    expires: Option<DateTime>,
    website_redirect_location: Option<String>,
    metadata: Option<HashMap<String, String>>,
    checksum: Option<ChecksumAlgorithm>,
}

impl AwsS3 {
    // 호출자는 승인된 지역 클라이언트와 목록의 원본 키를 전달한다.
    pub(super) async fn read_object_for_edit(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<EditableObject, EditError> {
        check_bucket(bucket, Stage::GetObject).map_err(EditError::from)?;
        if !(1..=1024).contains(&key.len()) {
            return Err(unsupported(Restriction::Key));
        }
        self.check_edit_bucket(bucket).await?;
        let mut output = self.client.get_object()
            .bucket(bucket)
            .key(key)
            .checksum_mode(ChecksumMode::Enabled)
            .customize()
            .interceptor(CompleteResponse(None))
            .send()
            .await
            .map_err(|error| edit_sdk_error(Stage::GetObject, error))?;
        let e_tag = output.e_tag.take().filter(|tag| valid_etag(tag)).ok_or_else(invalid_read)?;
        let version_id = output.version_id.take().filter(|id| valid_version(id))
            .ok_or_else(|| unsupported(Restriction::Versioning))?;
        let bytes = edit_length(output.content_length())?;
        if !output.content_type().is_none_or(is_utf8_content_type) {
            return Err(unsupported(Restriction::Text));
        }
        if !is_identity(output.content_encoding()) {
            return Err(unsupported(Restriction::ContentEncoding));
        }
        if output.delete_marker() == Some(true)
            || output.content_range().is_some()
            || output.missing_meta().is_some_and(|value| value != 0)
            || output.storage_class().is_some_and(|class| class != &StorageClass::Standard)
            || output.server_side_encryption() != Some(&ServerSideEncryption::Aes256)
            || output.ssekms_key_id().is_some()
            || output.sse_customer_algorithm().is_some()
            || output.sse_customer_key_md5().is_some()
            || output.bucket_key_enabled() == Some(true)
            || output.object_lock_mode().is_some()
            || output.object_lock_retain_until_date().is_some()
            || output.object_lock_legal_hold_status().is_some()
            || output.object_lock_event_hold().is_some()
            || output.object_lock_event_hold_duration_days().is_some()
            || output.object_lock_event_hold_duration_years().is_some()
            || output.replication_status().is_some()
            || output.restore().is_some()
        {
            return Err(unsupported(Restriction::ObjectProperties));
        }
        // 명시 태그 개수가 있어도 태그 부재는 별도 버전 조회로 확인한다.
        if output.tag_count().is_some_and(|count| count != 0) {
            return Err(unsupported(Restriction::Tags));
        }
        let mut checksum = None;
        if output.checksum_type().is_some_and(|kind| kind != &ChecksumType::FullObject)
            || output.checksum_md5.is_some()
            || output.checksum_sha512.is_some()
            || output.checksum_xxhash3.is_some()
            || output.checksum_xxhash64.is_some()
            || output.checksum_xxhash128.is_some()
        {
            return Err(unsupported(Restriction::ObjectProperties));
        }
        for (value, algorithm) in [
            (&output.checksum_crc32, ChecksumAlgorithm::Crc32),
            (&output.checksum_crc32_c, ChecksumAlgorithm::Crc32C),
            (&output.checksum_crc64_nvme, ChecksumAlgorithm::Crc64Nvme),
            (&output.checksum_sha1, ChecksumAlgorithm::Sha1),
            (&output.checksum_sha256, ChecksumAlgorithm::Sha256),
        ] {
            if let Some(value) = value
                && (value.is_empty() || value.contains('-') || checksum.replace(algorithm).is_some())
            {
                return Err(unsupported(Restriction::ObjectProperties));
            }
        }
        // SDK는 실제 Expires 헤더를 이전 이름의 필드에 역직렬화한다.
        #[allow(deprecated)]
        let expires = match (output.expires.take(), output.expires_string()) {
            (Some(date), _) => Some(date),
            (None, Some(value)) => Some(DateTime::from_str(value, Format::HttpDate)
                .map_err(|_| unsupported(Restriction::ObjectProperties))?),
            (None, None) => None,
        };
        let text = tokio::time::timeout(BODY_TIMEOUT, read_text(output.body, bytes))
            .await
            .map_err(|_| Error::at(Stage::DownloadBody, Failure::TimedOut))??;
        self.check_edit_properties(bucket, key, &version_id).await?;
        Ok(EditableObject {
            text,
            revision: EditRevision {
                target: Arc::new(EditTarget {
                    client: self.client.clone(),
                    bucket: bucket.into(),
                    key: key.into(),
                    content_type: output.content_type,
                    content_encoding: output.content_encoding,
                    content_language: output.content_language,
                    content_disposition: output.content_disposition,
                    cache_control: output.cache_control,
                    expires,
                    website_redirect_location: output.website_redirect_location,
                    metadata: output.metadata,
                    checksum,
                }),
                e_tag,
                version_id,
                bytes,
            },
        })
    }

    pub(super) async fn save_edited_object(
        &self,
        revision: &EditRevision,
        text: String,
    ) -> Result<EditRevision, EditError> {
        let target = &revision.target;
        if !std::ptr::eq(self.client.config(), target.client.config()) {
            return Err(Error::at(Stage::PutObject, Failure::Request).into());
        }
        validate_text(&text)?;
        self.check_edit_bucket(&target.bucket).await?;
        self.check_edit_properties(&target.bucket, &target.key, &revision.version_id).await?;
        let current = self.client.head_object()
            .bucket(&target.bucket)
            .key(&target.key)
            .if_match(&revision.e_tag)
            .customize()
            .interceptor(CompleteResponse(None))
            .send()
            .await
            .map_err(|error| edit_sdk_error(Stage::GetObject, error))?;
        if current.version_id() != Some(revision.version_id.as_str())
            || current.e_tag() != Some(revision.e_tag.as_str())
            || current.content_length() != Some(revision.bytes as i64)
            || current.delete_marker() == Some(true)
            || current.storage_class().is_some_and(|class| class != &StorageClass::Standard)
            || current.replication_status().is_some()
            || current.restore().is_some()
            || current.server_side_encryption() != Some(&ServerSideEncryption::Aes256)
        {
            return Err(EditError::SnapshotChanged);
        }
        // 버전과 태그·주석은 저장 전 관측이다. S3 PUT의 원자 조건은 현재 ETag뿐이다.
        let bytes = text.len();
        let has_content_type = target.content_type.is_some();
        let result = self.client.put_object()
            .bucket(&target.bucket)
            .key(&target.key)
            .if_match(&revision.e_tag)
            .storage_class(StorageClass::Standard)
            .server_side_encryption(ServerSideEncryption::Aes256)
            .set_content_type(target.content_type.clone())
            .set_content_encoding(target.content_encoding.clone())
            .set_content_language(target.content_language.clone())
            .set_content_disposition(target.content_disposition.clone())
            .set_cache_control(target.cache_control.clone())
            .set_expires(target.expires)
            .set_website_redirect_location(target.website_redirect_location.clone())
            .set_metadata(target.metadata.clone())
            .set_checksum_algorithm(target.checksum.clone())
            .body(ByteStream::from(text.into_bytes()))
            .customize()
            .config_override(single_attempt())
            .mutate_request(move |request| {
                if !has_content_type {
                    request.headers_mut().remove("content-type");
                }
            })
            .interceptor(CompleteResponse(None))
            .send()
            .await
            .map_err(put_error)?;
        let e_tag = result.e_tag.filter(|tag| valid_etag(tag)).ok_or(EditError::Unconfirmed)?;
        let version_id = result.version_id.filter(|id| valid_version(id) && id != &revision.version_id)
            .ok_or(EditError::Unconfirmed)?;
        Ok(EditRevision { target: Arc::clone(target), e_tag, version_id, bytes })
    }

    async fn check_edit_bucket(&self, bucket: &str) -> Result<(), EditError> {
        let versioning = self.client.get_bucket_versioning().bucket(bucket).customize()
            .interceptor(CompleteResponse(Some(b"</VersioningConfiguration>")))
            .send().await.map_err(|error| edit_sdk_error(Stage::GetObject, error))?;
        if versioning.status() != Some(&BucketVersioningStatus::Enabled) {
            return Err(unsupported(Restriction::Versioning));
        }
        let ownership = self.client.get_bucket_ownership_controls().bucket(bucket).customize()
            .interceptor(CompleteResponse(Some(b"</OwnershipControls>")))
            .send().await.map_err(|error| edit_sdk_error(Stage::GetObject, error))?;
        if !matches!(ownership.ownership_controls().map(|controls| controls.rules()),
            Some([rule]) if rule.object_ownership() == &ObjectOwnership::BucketOwnerEnforced)
        {
            return Err(unsupported(Restriction::Ownership));
        }
        match self.client.get_object_lock_configuration().bucket(bucket).send().await {
            Err(error) if error.raw_response().is_some_and(|response| response.status().as_u16() == 404)
                && error.as_service_error().and_then(ProvideErrorMetadata::code) == Some("ObjectLockConfigurationNotFoundError") => Ok(()),
            Ok(_) => Err(unsupported(Restriction::ObjectLock)),
            Err(error) => Err(edit_sdk_error(Stage::GetObject, error)),
        }
    }

    async fn check_edit_properties(&self, bucket: &str, key: &str, version_id: &str) -> Result<(), EditError> {
        let tags = self.client.get_object_tagging().bucket(bucket).key(key).version_id(version_id).customize()
            .interceptor(CompleteResponse(Some(b"</Tagging>")))
            .send().await.map_err(|error| edit_sdk_error(Stage::GetObject, error))?;
        if tags.version_id() != Some(version_id) {
            return Err(EditError::SnapshotChanged);
        }
        if !tags.tag_set().is_empty() {
            return Err(unsupported(Restriction::Tags));
        }
        let annotations = self.client.list_object_annotations().bucket(bucket).key(key).version_id(version_id)
            .max_annotation_results(1).customize()
            .interceptor(CompleteResponse(Some(b"</ListObjectAnnotationsOutput>")))
            .send().await.map_err(|error| edit_sdk_error(Stage::GetObject, error))?;
        if annotations.object_version_id() != Some(version_id)
            || annotations.bucket().is_some_and(|name| name != bucket)
            || annotations.key().is_some_and(|name| name != key)
        {
            return Err(EditError::SnapshotChanged);
        }
        if annotations.annotation_count() != Some(0)
            || !annotations.annotations().is_empty()
            || annotations.next_continuation_token().is_some()
        {
            return Err(unsupported(Restriction::Annotations));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct CompleteResponse(Option<&'static [u8]>);

impl Intercept for CompleteResponse {
    fn name(&self) -> &'static str { "CompleteObjectEditResponse" }

    fn read_after_deserialization(
        &self,
        context: &AfterDeserializationInterceptorContextRef<'_>,
        _: &RuntimeComponents,
        _: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let response = context.response();
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Ok(());
        }
        // 잘린 XML이 부분적으로 파싱돼도 속성 부재의 근거로 삼을 수 없다.
        if status != 200 || self.0.is_some_and(|end| !response.body().bytes()
            .is_some_and(|body| body.trim_ascii_end().ends_with(end)))
        {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "불완전한 S3 편집 응답").into());
        }
        for (name, value) in response.headers().iter_bytes() {
            let preserved = name.starts_with("x-amz-meta-") || matches!(name,
                "cache-control" | "content-disposition" | "content-encoding" | "content-language" |
                "content-type" | "expires" | "x-amz-website-redirect-location"
            );
            if preserved && std::str::from_utf8(value).is_err() {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "보존할 수 없는 S3 응답 헤더").into());
            }
        }
        Ok(())
    }
}

fn valid_etag(value: &str) -> bool {
    value.strip_prefix('"').and_then(|tag| tag.strip_suffix('"')).is_some_and(|tag| {
        !tag.is_empty() && tag.bytes().all(|byte| byte == b'!' || (b'#'..=b'~').contains(&byte))
    })
}

fn valid_version(value: &str) -> bool {
    (1..=1024).contains(&value.len()) && value != "null" && !value.chars().any(char::is_control)
}

fn is_identity(value: Option<&str>) -> bool {
    value.is_none_or(|encoding| encoding.eq_ignore_ascii_case("identity"))
}

fn edit_length(value: Option<i64>) -> Result<usize, EditError> {
    let value = value.filter(|n| *n >= 0).ok_or_else(invalid_read)?;
    if value > MAX_EDIT_BYTES as i64 {
        return Err(unsupported(Restriction::TooLarge));
    }
    Ok(value as usize)
}

async fn read_text(mut body: ByteStream, expected: usize) -> Result<String, EditError> {
    let length = |bytes| Error { stage: Stage::DownloadBody, failure: Failure::Length { expected: Some(expected as u64) }, bytes };
    let mut data = Vec::with_capacity(expected);
    loop {
        let chunk = body.try_next().await.map_err(|error| {
            let mut source = error.source();
            while let Some(cause) = source {
                if cause.is::<ContentLengthError>() {
                    return length(data.len() as u64);
                }
                if cause.is::<aws_smithy_checksums::body::validate::Error>() {
                    return Error::at(Stage::DownloadBody, Failure::Checksum);
                }
                source = cause.source();
            }
            Error::at(Stage::DownloadBody, Failure::BodyRead)
        })?;
        let Some(chunk) = chunk else { break };
        if chunk.len() > expected - data.len() {
            return Err(length(data.len() as u64).into());
        }
        data.extend_from_slice(&chunk);
    }
    if data.len() != expected {
        return Err(length(data.len() as u64).into());
    }
    let text = String::from_utf8(data).map_err(|_| unsupported(Restriction::Text))?;
    validate_text(&text)?;
    Ok(text)
}

fn validate_text(text: &str) -> Result<(), EditError> {
    if text.len() > MAX_EDIT_BYTES {
        return Err(unsupported(Restriction::TooLarge));
    }
    if text.chars().any(|c| c.is_control() && !matches!(c, '\t' | '\r' | '\n')) {
        return Err(unsupported(Restriction::Text));
    }
    Ok(())
}

fn is_utf8_content_type(value: &str) -> bool {
    let mut parts = value.split(';');
    let media = parts.next().unwrap_or_default().trim();
    let text = media.get(..5).is_some_and(|prefix| prefix.eq_ignore_ascii_case("text/"));
    if !text && !["application/octet-stream", "application/json", "application/xml", "application/yaml",
        "application/toml", "application/javascript"].iter().any(|kind| media.eq_ignore_ascii_case(kind))
    {
        return false;
    }
    parts.all(|parameter| {
        let Some((name, encoding)) = parameter.split_once('=') else { return false; };
        !name.trim().eq_ignore_ascii_case("charset") || encoding.trim().trim_matches('"').eq_ignore_ascii_case("UTF-8")
    })
}

fn unsupported(reason: EditRestriction) -> EditError {
    EditError::Unsupported { reason }
}

fn invalid_read() -> EditError {
    Error::at(Stage::GetObject, Failure::InvalidResponse).into()
}

impl From<Error> for EditError {
    fn from(error: Error) -> Self {
        Self::Storage { error }
    }
}

fn edit_sdk_error<E: ProvideErrorMetadata + StdError + 'static>(stage: Stage, error: SdkError<E, HttpResponse>) -> EditError {
    match error.raw_response().map(|response| response.status().as_u16()) {
        Some(status @ (404 | 409 | 412)) => EditError::Conflict { status },
        _ => sdk_error(stage, error).into(),
    }
}

fn put_error(error: SdkError<PutObjectError, HttpResponse>) -> EditError {
    match error.raw_response().map(|response| response.status().as_u16()) {
        Some(200..=299 | 408 | 500..=599) => EditError::Unconfirmed,
        None if matches!(error, SdkError::TimeoutError(_) | SdkError::ResponseError(_)) => EditError::Unconfirmed,
        None if matches!(error, SdkError::DispatchFailure(_)) => {
            let error = sdk_error(Stage::PutObject, error);
            match error.failure {
                Failure::Credentials | Failure::SsoLoginRequired => error.into(),
                _ => EditError::Unconfirmed,
            }
        }
        _ => edit_sdk_error(Stage::PutObject, error),
    }
}

#[cfg(test)]
#[path = "object_edit_tests.rs"]
mod tests;
