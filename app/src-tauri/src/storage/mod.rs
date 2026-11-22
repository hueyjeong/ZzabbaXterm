mod auth;
mod download;
pub(crate) mod ipc;
#[cfg(test)]
mod tests;

use std::{error::Error as StdError, fs::File, time::Duration};

use crate::editor::storage_files::DownloadTarget;

use aws_credential_types::provider::{SharedCredentialsProvider, error::CredentialsError};
use aws_sdk_s3::{
    Client,
    config::Region,
    error::{ProvideErrorMetadata, SdkError},
    primitives::ByteStream,
    types::EncodingType,
};
use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
use percent_encoding::percent_decode_str;
use aws_smithy_async::time::SharedTimeSource;
use aws_smithy_runtime_api::client::http::SharedHttpClient;
use aws_smithy_types::timeout::TimeoutConfig;

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct BucketRow {
    name: String,
    region: String,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Page<T> {
    pub rows: Vec<T>,
    pub next_token: Option<String>,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum ObjectRow {
    Prefix { prefix: String },
    Object { key: String, bytes: Option<u64> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Stage {
    Profile,
    Identity,
    ListBuckets,
    HeadBucket,
    ListObjects,
    UploadSource,
    PutObject,
    GetObject,
    DownloadBody,
    DownloadWrite,
    DownloadCommit,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum Failure {
    InvalidSsoProfile,
    RetestRequired,
    AccountMismatch,
    InvalidRegion,
    InvalidBucket,
    BucketRegionMismatch { region: String },
    InvalidResponse,
    Credentials,
    SsoLoginRequired,
    LoginFailed,
    Service { status: u16, code: Option<String> },
    Request,
    TimedOut,
    LocalIo { error_kind: String },
    RecoveryUnconfirmed { recovery_path: String },
    BodyRead,
    Checksum,
    Length { expected: Option<u64> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DownloadIntegrity {
    SdkChecksumVerified,
    LengthOnly,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Downloaded {
    pub bytes: u64,
    pub integrity: DownloadIntegrity,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Error {
    pub stage: Stage,
    pub failure: Failure,
    pub bytes: u64,
}

impl Error {
    fn at(stage: Stage, failure: Failure) -> Self {
        Self {
            stage,
            failure,
            bytes: 0,
        }
    }
}

// SDK에는 C02 검증을 통과한 공급자 설정만 전달한다.
#[derive(PartialEq, Eq)]
struct VerifiedSsoSelection {
    scope: crate::ssm::Scope,
    config: crate::ssm::DirectSsoConfig,
}

#[derive(Clone)]
pub(crate) struct AwsS3 {
    client: Client,
    endpoint: String,
    region: String,
}

impl AwsS3 {
    async fn for_sso_profile(region: &str, config: crate::ssm::DirectSsoConfig) -> Result<Self, Error> {
        let home = crate::ssm::user_home().map_err(|_| Error::at(Stage::Profile, Failure::InvalidSsoProfile))?;
        let transport = aws_smithy_http_client::Builder::new()
            .tls_provider(aws_smithy_http_client::tls::Provider::Rustls(
                aws_smithy_http_client::tls::rustls_provider::CryptoMode::AwsLc,
            )).build_https();
        Self::verified(region, config, home, SharedHttpClient::new(transport), SharedTimeSource::default()).await
    }

    async fn verified(region: &str, config: crate::ssm::DirectSsoConfig, home: std::path::PathBuf,
        transport: SharedHttpClient, clock: SharedTimeSource) -> Result<Self, Error> {
        let expected = config.account.clone();
        let provider = SharedCredentialsProvider::new(auth::Provider::new(config, home, transport.clone(), clock.clone()));
        let sts = aws_sdk_sts::Client::from_conf(aws_sdk_sts::Config::builder()
            .behavior_version_latest().region(Region::new(region.to_owned()))
            .endpoint_url(format!("https://sts.{region}.{}", dns_suffix(region)))
            .http_client(transport.clone()).time_source(clock.clone())
            .identity_cache(aws_sdk_sts::config::IdentityCache::no_cache())
            .timeout_config(response_timeout()).sleep_impl(aws_smithy_async::rt::sleep::TokioSleep::new())
            .credentials_provider(provider.clone()).build());
        let identity = sts.get_caller_identity().send().await.map_err(|error| sdk_error(Stage::Identity, error))?;
        let account = identity.account().ok_or_else(|| Error::at(Stage::Identity, Failure::InvalidResponse))?;
        if account != expected {
            return Err(Error::at(Stage::Identity, Failure::AccountMismatch));
        }
        if identity.arn().and_then(|arn| arn.split(':').nth(4)) != Some(account) {
            return Err(Error::at(Stage::Identity, Failure::InvalidResponse));
        }
        let client = Client::from_conf(s3_config(region).http_client(transport).time_source(clock)
            .identity_cache(aws_sdk_s3::config::IdentityCache::no_cache())
            .credentials_provider(provider).build());
        Ok(Self { client, endpoint: endpoint(region), region: region.into() })
    }

    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub(crate) async fn list_buckets(&self, token: Option<&str>) -> Result<Page<BucketRow>, Error> {
        let output = self
            .client
            .list_buckets()
            .set_continuation_token(token.map(str::to_owned))
            .max_buckets(1000)
            .send()
            .await
            .map_err(|err| sdk_error(Stage::ListBuckets, err))?;
        let rows = output
            .buckets
            .unwrap_or_default()
            .into_iter()
            .map(|bucket| {
                let invalid = || Error::at(Stage::ListBuckets, Failure::InvalidResponse);
                let name = bucket.name.ok_or_else(invalid)?;
                let region = bucket.bucket_region.filter(|region| self.accepts_region(region)).ok_or_else(invalid)?;
                Ok(BucketRow { name, region })
            })
            .collect::<Result<_, _>>()?;
        Ok(Page {
            rows,
            next_token: output.continuation_token,
        })
    }

    fn accepts_region(&self, region: &str) -> bool {
        crate::ssm::valid_region(region) && partition(&self.region) == partition(region)
    }

    fn in_region(&self, region: &str, stage: Stage) -> Result<std::borrow::Cow<'_, Self>, Error> {
        if !self.accepts_region(region) {
            return Err(Error::at(stage, Failure::InvalidRegion));
        }
        if region == self.region {
            return Ok(std::borrow::Cow::Borrowed(self));
        }
        let client = Client::from_conf(self.client.config().to_builder().region(Region::new(region.to_owned())).build());
        Ok(std::borrow::Cow::Owned(Self { client, endpoint: endpoint(region), region: region.into() }))
    }

    async fn check_bucket_region(&self, bucket: &str) -> Result<(), Error> {
        let stage = Stage::HeadBucket;
        check_bucket(bucket, stage)?;
        let result = self.client.head_bucket().bucket(bucket).send().await;
        let actual = match &result {
            Ok(output) => output.bucket_region(),
            Err(error) => error.raw_response().and_then(|response| response.headers().get("x-amz-bucket-region")),
        };
        if let Some(actual) = actual {
            if !self.accepts_region(actual) {
                return Err(Error::at(stage, Failure::InvalidResponse));
            }
            if actual != self.region {
                return Err(Error::at(stage, Failure::BucketRegionMismatch { region: actual.to_owned() }));
            }
            // HeadBucket의 리전 헤더는 목록 권한과 독립적이다.
            return Ok(());
        } else if result.is_ok() {
            return Err(Error::at(stage, Failure::InvalidResponse));
        }
        result.map(|_| ()).map_err(|error| sdk_error(stage, error))
    }

    pub(crate) async fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        token: Option<&str>,
    ) -> Result<Page<ObjectRow>, Error> {
        check_bucket(bucket, Stage::ListObjects)?;
        let output = self
            .client
            .list_objects_v2()
            .bucket(bucket)
            .prefix(prefix)
            .delimiter("/")
            .encoding_type(EncodingType::Url)
            .set_continuation_token(token.map(str::to_owned))
            .send()
            .await
            .map_err(|err| sdk_error(Stage::ListObjects, err))?;
        let invalid = || Error::at(Stage::ListObjects, Failure::InvalidResponse);
        if output.encoding_type() != Some(&EncodingType::Url)
            || output.is_truncated().unwrap_or_default() != output.next_continuation_token.is_some()
            || output
                .next_continuation_token
                .as_deref()
                .is_some_and(|next| next.is_empty() || Some(next) == token)
        {
            return Err(invalid());
        }
        // URL 인코딩은 XML에 넣을 수 없는 키도 전달한다. 토큰에는 이 변환을 적용하지 않는다.
        let decode = |value: Option<String>| {
            let value = value.ok_or_else(invalid)?;
            if value.bytes().enumerate().any(|(i, byte)| {
                byte == b'%'
                    && !value
                        .as_bytes()
                        .get(i + 1..i + 3)
                        .is_some_and(|digits| digits.iter().all(u8::is_ascii_hexdigit))
            }) {
                return Err(invalid());
            }
            Ok(
                match percent_decode_str(&value)
                    .decode_utf8()
                    .map_err(|_| invalid())?
                {
                    std::borrow::Cow::Borrowed(_) => value,
                    std::borrow::Cow::Owned(decoded) => decoded,
                },
            )
        };
        let mut rows = Vec::with_capacity(output.common_prefixes().len() + output.contents().len());
        for prefix in output.common_prefixes.unwrap_or_default() {
            rows.push(ObjectRow::Prefix {
                prefix: decode(prefix.prefix)?,
            });
        }
        for object in output.contents.unwrap_or_default() {
            let bytes = object.size().and_then(|n| u64::try_from(n).ok());
            rows.push(ObjectRow::Object {
                key: decode(object.key)?,
                bytes,
            });
        }
        Ok(Page {
            rows,
            next_token: output.next_continuation_token,
        })
    }

    pub(crate) async fn upload(
        &self,
        bucket: &str,
        key: &str,
        source: File,
    ) -> Result<u64, Error> {
        check_bucket(bucket, Stage::PutObject)?;
        let body = ByteStream::read_from().file(tokio::fs::File::from_std(source)).build()
            .await
            .map_err(|_| Error::at(Stage::UploadSource, Failure::Request))?;
        let bytes = body.size_hint().0;
        self.client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(body)
            .customize()
            .config_override(single_attempt())
            .send()
            .await
            .map_err(|err| sdk_error(Stage::PutObject, err))?;
        Ok(bytes)
    }

    pub(crate) async fn download(
        &self,
        bucket: &str,
        key: &str,
        target: &DownloadTarget,
        closed: &tokio::sync::watch::Sender<bool>,
    ) -> Result<Downloaded, Error> {
        check_bucket(bucket, Stage::GetObject)?;
        download::download(&self.client, bucket, key, target, closed).await
    }
}

fn endpoint(region: &str) -> String {
    format!("https://s3.{region}.{}", dns_suffix(region))
}

fn partition(region: &str) -> &str {
    ["us-iso-", "us-isob-", "us-isof-", "eu-isoe-", "us-gov-", "cn-", "eusc-"].into_iter()
        .find(|prefix| region.starts_with(prefix)).unwrap_or("aws")
}

fn response_timeout() -> TimeoutConfig {
    TimeoutConfig::builder().connect_timeout(Duration::from_secs(5)).operation_timeout(Duration::from_secs(20)).build()
}

fn single_attempt() -> aws_sdk_s3::config::Builder {
    aws_sdk_s3::config::Builder::new()
        .retry_config(aws_smithy_types::retry::RetryConfig::disabled())
}

fn s3_config(region: &str) -> aws_sdk_s3::config::Builder {
    // 기본 설정 체인을 거치지 않으면 환경과 프로필의 endpoint override가 들어올 수 없다.
    aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .region(Region::new(region.to_owned()))
        .force_path_style(true)
        .timeout_config(response_timeout())
        .sleep_impl(aws_smithy_async::rt::sleep::TokioSleep::new())
        .retry_config(aws_smithy_types::retry::RetryConfig::disabled())
}

fn dns_suffix(region: &str) -> &'static str {
    if region.starts_with("cn-") {
        "amazonaws.com.cn"
    } else {
        "amazonaws.com"
    }
}

fn check_bucket(bucket: &str, stage: Stage) -> Result<(), Error> {
    let label = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    if !(3..=63).contains(&bucket.len())
        || !bucket.split('.').all(|part| {
            part.bytes().next().is_some_and(label)
                && part.bytes().last().is_some_and(label)
                && part.bytes().all(|byte| label(byte) || byte == b'-')
        })
        || ["--x-s3", "--table-s3", "-s3alias", "--ol-s3", ".mrap"]
            .iter()
            .any(|suffix| bucket.ends_with(suffix))
    {
        return Err(Error::at(stage, Failure::InvalidBucket));
    }
    Ok(())
}

fn sdk_error<E: ProvideErrorMetadata + StdError + 'static>(
    stage: Stage,
    error: SdkError<E, HttpResponse>,
) -> Error {
    if matches!(error, SdkError::TimeoutError(_)) {
        return Error::at(stage, Failure::TimedOut);
    }
    let failure =
        if let Some(response) = error.raw_response().filter(|r| r.status().as_u16() >= 300) {
            let code = error
                .as_service_error()
                .and_then(ProvideErrorMetadata::code)
                .filter(|code| {
                    !code.is_empty()
                        && code.len() <= 96
                        && code
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                })
                .map(str::to_owned);
            Failure::Service {
                status: response.status().as_u16(),
                code,
            }
        } else {
            let mut failure = Failure::Request;
            let mut cause = error.source();
            while let Some(source) = cause {
                if source.is::<CredentialsError>() {
                    failure = Failure::Credentials;
                }
                if source
                    .downcast_ref::<SdkError<
                        aws_sdk_sso::operation::get_role_credentials::GetRoleCredentialsError,
                        HttpResponse,
                    >>()
                    .and_then(SdkError::as_service_error)
                    .is_some_and(|err| err.is_unauthorized_exception())
                    || source.is::<auth::LoginRequired>()
                {
                    failure = Failure::SsoLoginRequired;
                    break;
                }
                cause = source.source();
            }
            if error.raw_response().is_some() {
                Failure::InvalidResponse
            } else {
                failure
            }
        };
    Error::at(stage, failure)
}
