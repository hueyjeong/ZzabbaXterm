use std::{error::Error as StdError, fmt, fs::File, io, net::IpAddr, sync::Arc};

use aws_sdk_s3::{
    Client, Config,
    config::{Credentials, Region, endpoint::Params},
    error::{ProvideErrorMetadata, SdkError},
};
use aws_smithy_http_client::{Builder, tls};
use aws_smithy_runtime_api::{
    box_error::BoxError,
    client::{
        endpoint::EndpointResolverParams,
        http::SharedHttpClient,
        interceptors::{Intercept, context::BeforeTransmitInterceptorContextRef},
        orchestrator::HttpResponse,
        runtime_components::RuntimeComponents,
    },
};
use aws_smithy_types::{config_bag::ConfigBag, retry::RetryConfig};
use http::Uri;
use tokio::sync::watch;

use crate::editor::storage_files::DownloadTarget;

use super::{BucketRow, Downloaded, Error, Failure, ObjectRow, Page, S3Operations};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AddressStyle {
    Path,
    VirtualHosted,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ConfigError {
    Endpoint,
    Region,
    ServiceId,
    KeyId,
    KeyAlias,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyAccessError {
    WrongService,
    Cancelled,
    Missing,
    Locked,
    Unavailable,
    Invalid,
    AlreadyExists,
    OutcomeUnknown,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ServiceKeyAlias(String);

impl ServiceKeyAlias {
    pub(crate) fn new(alias: String) -> Result<Self, ConfigError> {
        if alias.trim().is_empty() || alias.len() > 128 || alias.chars().any(char::is_control) {
            return Err(ConfigError::KeyAlias);
        }
        Ok(Self(alias))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// 금고 조회는 항목 ID뿐 아니라 이 서비스 ID와 정규화된 HTTPS 출처로 범위를 제한한다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServiceKeyScope {
    service_id: String,
    origin: String,
}

impl ServiceKeyScope {
    pub(crate) fn service_id(&self) -> &str {
        &self.service_id
    }

    pub(crate) fn origin(&self) -> &str {
        &self.origin
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ServiceKeyRef {
    scope: ServiceKeyScope,
    item_id: String,
}

impl ServiceKeyRef {
    /// 새 항목을 선택하거나 등록할 때 바인딩한다. 기존 참조는 원래 선택한 서비스를 보존한다.
    pub(crate) fn new(service: &CompatibleService, item_id: String) -> Result<Self, ConfigError> {
        if !opaque_id(&item_id) {
            return Err(ConfigError::KeyId);
        }
        Ok(Self {
            scope: service.scope.clone(),
            item_id,
        })
    }

    pub(crate) fn scope(&self) -> &ServiceKeyScope {
        &self.scope
    }

    pub(crate) fn item_id(&self) -> &str {
        &self.item_id
    }
}

pub(crate) struct ServiceKey(Credentials);

impl ServiceKey {
    pub(crate) fn new(
        access_key_id: String,
        secret_access_key: String,
    ) -> Result<Self, KeyAccessError> {
        if access_key_id.is_empty() || secret_access_key.is_empty() {
            return Err(KeyAccessError::Invalid);
        }
        Ok(Self(Credentials::new(
            access_key_id,
            secret_access_key,
            None,
            None,
            "S3 compatible service",
        )))
    }
}

#[derive(Debug)]
pub(crate) struct CompatibleService {
    scope: ServiceKeyScope,
    region: String,
    destination: Destination,
}

pub(crate) struct CompatibleS3 {
    operations: S3Operations,
    target: Arc<ServiceTarget>,
}

impl fmt::Debug for CompatibleS3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompatibleS3")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ServiceTarget {
    pub endpoint: String,
    pub service_id: String,
    pub region: String,
    pub key_alias: ServiceKeyAlias,
    pub addressing: AddressStyle,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CompatibleError {
    pub target: Arc<ServiceTarget>,
    pub operation: Error,
}

impl CompatibleS3 {
    pub(crate) fn target(&self) -> &ServiceTarget {
        &self.target
    }

    pub(crate) async fn list_buckets(
        &self,
        token: Option<&str>,
    ) -> Result<Page<BucketRow>, CompatibleError> {
        self.operations
            .list_buckets(token, &self.target.region)
            .await
            .map_err(|err| self.with_target(err))
    }

    pub(crate) async fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        token: Option<&str>,
    ) -> Result<Page<ObjectRow>, CompatibleError> {
        self.operations
            .list_objects(bucket, prefix, token)
            .await
            .map_err(|err| self.with_target(err))
    }

    pub(crate) async fn upload(
        &self,
        bucket: &str,
        key: &str,
        source: File,
    ) -> Result<u64, CompatibleError> {
        self.upload_if(bucket, key, source, &super::UploadCondition::None).await
    }

    pub(crate) async fn upload_if(
        &self,
        bucket: &str,
        key: &str,
        source: File,
        condition: &super::UploadCondition,
    ) -> Result<u64, CompatibleError> {
        self.operations
            .upload(bucket, key, source, condition)
            .await
            .map_err(|err| self.with_target(err))
    }

    pub(crate) async fn download(
        &self,
        bucket: &str,
        key: &str,
        target: &DownloadTarget,
        closed: &watch::Sender<bool>,
    ) -> Result<Downloaded, CompatibleError> {
        self.operations
            .download(bucket, key, target, closed)
            .await
            .map_err(|err| self.with_target(err))
    }

    pub(crate) async fn object_head(&self, bucket: &str, key: &str) -> Result<super::ObjectHead, CompatibleError> {
        self.operations
            .object_head(bucket, key)
            .await
            .map_err(|err| self.with_target(err))
    }

    fn with_target(&self, operation: Error) -> CompatibleError {
        CompatibleError {
            target: Arc::clone(&self.target),
            operation,
        }
    }
}

impl CompatibleService {
    pub(crate) fn new(
        service_id: String,
        endpoint: &str,
        region: String,
        style: AddressStyle,
    ) -> Result<Self, ConfigError> {
        if !opaque_id(&service_id) {
            return Err(ConfigError::ServiceId);
        }
        if !(1..=63).contains(&region.len())
            || !region
                .split('-')
                .all(|part| !part.is_empty() && part.bytes().all(lower_alphanumeric))
        {
            return Err(ConfigError::Region);
        }

        let authority = endpoint
            .strip_prefix("https://")
            .map(|rest| rest.strip_suffix('/').unwrap_or(rest))
            .filter(|rest| !rest.contains(['/', '@', '?', '#', '\\', '%']))
            .and_then(|rest| rest.parse::<http::uri::Authority>().ok())
            .ok_or(ConfigError::Endpoint)?;
        let host = authority.host();
        let ip = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host)
            .parse::<IpAddr>();
        match ip {
            Ok(_) if style == AddressStyle::Path => (),
            Err(_) if dns_name(host, true) => (),
            _ => return Err(ConfigError::Endpoint),
        }
        let port = https_port(&authority).ok_or(ConfigError::Endpoint)?;
        let host = host.to_ascii_lowercase();
        let origin = if port == 443 {
            format!("https://{host}")
        } else {
            format!("https://{host}:{port}")
        };
        Ok(Self {
            scope: ServiceKeyScope { service_id, origin },
            region,
            destination: Destination { host, port, style },
        })
    }

    pub(crate) fn scope(&self) -> &ServiceKeyScope {
        &self.scope
    }

    pub(crate) fn region(&self) -> &str {
        &self.region
    }

    pub(crate) fn connect(
        &self,
        reference: &ServiceKeyRef,
        alias: ServiceKeyAlias,
        read_key: impl FnOnce(&ServiceKeyScope, &str) -> Result<ServiceKey, KeyAccessError>,
    ) -> Result<CompatibleS3, KeyAccessError> {
        self.connect_with(reference, alias, read_key, || {
            https(tls::TlsContext::default())
        })
    }

    pub(super) fn connect_with(
        &self,
        reference: &ServiceKeyRef,
        alias: ServiceKeyAlias,
        read_key: impl FnOnce(&ServiceKeyScope, &str) -> Result<ServiceKey, KeyAccessError>,
        transport: impl FnOnce() -> SharedHttpClient,
    ) -> Result<CompatibleS3, KeyAccessError> {
        if self.scope != reference.scope {
            return Err(KeyAccessError::WrongService);
        }
        let credentials = read_key(reference.scope(), reference.item_id())?;
        let config = Config::builder()
            .behavior_version_latest()
            .endpoint_url(self.scope.origin())
            .region(Region::new(self.region.clone()))
            .force_path_style(self.destination.style == AddressStyle::Path)
            .disable_s3_express_session_auth(true)
            .retry_config(RetryConfig::disabled())
            .credentials_provider(credentials.0)
            .http_client(transport())
            .interceptor(self.destination.clone())
            .build();
        Ok(CompatibleS3 {
            operations: S3Operations {
                client: Client::from_conf(config),
                compatible: true,
            },
            target: Arc::new(ServiceTarget {
                endpoint: self.scope.origin.clone(),
                service_id: self.scope.service_id.clone(),
                region: self.region.clone(),
                key_alias: alias,
                addressing: self.destination.style,
            }),
        })
    }
}

fn https(context: tls::TlsContext) -> SharedHttpClient {
    Builder::new()
        .tls_provider(tls::Provider::Rustls(
            tls::rustls_provider::CryptoMode::AwsLc,
        ))
        .tls_context(context)
        .build_https()
}

fn https_port(authority: &http::uri::Authority) -> Option<u16> {
    match authority.as_str().strip_prefix(authority.host())? {
        "" => Some(443),
        suffix if suffix.starts_with(':') => authority.port_u16().filter(|port| *port != 0),
        _ => None,
    }
}

pub(super) fn bucket_list_endpoint(origin: &str, style: AddressStyle, bucket: &str) -> Result<String, Error> {
    use super::Stage::ListObjects;

    super::check_bucket(bucket, ListObjects)?;
    match style {
        AddressStyle::Path => Ok(format!("{origin}/{bucket}/")),
        AddressStyle::VirtualHosted => {
            let authority = origin.strip_prefix("https://").ok_or_else(|| Error::at(ListObjects, Failure::Destination))?;
            let host = authority.split(':').next().unwrap_or_default();
            // SDK는 점이 있는 버킷을 HTTPS 가상 호스트로 사용하지 않는다.
            if bucket.contains('.') || bucket.len() + 1 + host.len() > 253 {
                return Err(Error::at(ListObjects, Failure::Destination));
            }
            Ok(format!("https://{bucket}.{authority}/"))
        }
    }
}

pub(super) fn opaque_id(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn lower_alphanumeric(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

fn dns_name(name: &str, uppercase: bool) -> bool {
    let alphanumeric =
        |byte: u8| lower_alphanumeric(byte) || uppercase && byte.is_ascii_uppercase();
    name.len() <= 253
        && name.split('.').all(|label| {
            label.len() <= 63
                && label.bytes().next().is_some_and(alphanumeric)
                && label.bytes().last().is_some_and(alphanumeric)
                && label.bytes().all(|byte| alphanumeric(byte) || byte == b'-')
        })
}

#[derive(Clone, Debug)]
struct Destination {
    host: String,
    port: u16,
    style: AddressStyle,
}

#[derive(Debug)]
struct DestinationMismatch;

impl fmt::Display for DestinationMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("S3 compatible destination mismatch")
    }
}

impl StdError for DestinationMismatch {}

impl Destination {
    fn check(
        &self,
        context: &BeforeTransmitInterceptorContextRef<'_>,
        cfg: &ConfigBag,
    ) -> Result<(), DestinationMismatch> {
        let params = cfg
            .load::<EndpointResolverParams>()
            .and_then(EndpointResolverParams::get::<Params>)
            .ok_or(DestinationMismatch)?;
        let request = context.request();
        let uri: Uri = request.uri().parse().map_err(|_| DestinationMismatch)?;
        let authority = uri.authority().ok_or(DestinationMismatch)?;
        if uri.scheme_str() != Some("https")
            || authority.as_str().contains('@')
            || https_port(authority) != Some(self.port)
        {
            return Err(DestinationMismatch);
        }
        let mut hosts = request.headers().get_all_bytes("host");
        if hosts
            .next()
            .is_some_and(|host| !host.eq_ignore_ascii_case(authority.as_str().as_bytes()))
            || hosts.next().is_some()
        {
            return Err(DestinationMismatch);
        }
        let host = authority.host();
        let allowed = match params.bucket() {
            None => host == self.host && uri.path() == "/",
            Some(bucket) if (3..=63).contains(&bucket.len()) && dns_name(bucket, false) => {
                match self.style {
                    AddressStyle::Path => {
                        host == self.host && uri.path().split('/').nth(1) == Some(bucket)
                    }
                    AddressStyle::VirtualHosted => {
                        dns_name(host, false)
                            && host
                                .strip_suffix(&self.host)
                                .and_then(|prefix| prefix.strip_suffix('.'))
                                == Some(bucket)
                    }
                }
            }
            Some(_) => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(DestinationMismatch)
        }
    }
}

impl Intercept for Destination {
    fn name(&self) -> &'static str {
        "CompatibleDestination"
    }

    fn read_before_signing(
        &self,
        context: &BeforeTransmitInterceptorContextRef<'_>,
        _: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        Ok(self.check(context, cfg)?)
    }

    fn read_before_transmit(
        &self,
        context: &BeforeTransmitInterceptorContextRef<'_>,
        _: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        Ok(self.check(context, cfg)?)
    }
}

pub(super) fn request_failure<E: ProvideErrorMetadata + StdError + 'static>(
    error: &SdkError<E, HttpResponse>,
) -> Failure {
    let mut cause = error.source();
    while let Some(source) = cause {
        if source.is::<DestinationMismatch>() {
            return Failure::Destination;
        }
        if source.is::<rustls::Error>() {
            return Failure::Tls;
        }
        // Hyper가 TLS 오류를 여러 io::Error 페이로드에 중첩한다.
        cause = match source.downcast_ref::<io::Error>() {
            Some(error) => error
                .get_ref()
                .map(|inner| inner as &(dyn StdError + 'static)),
            None => source.source(),
        };
    }
    if let Some(response) = error.raw_response() {
        let status = response.status().as_u16();
        if status < 300 {
            return Failure::InvalidResponse;
        }
        let mut header_codes = response.headers().get_all_bytes("x-amz-error-code");
        let header_code = header_codes
            .next()
            .and_then(|code| std::str::from_utf8(code).ok());
        let code = error
            .as_service_error()
            .and_then(ProvideErrorMetadata::code)
            .or_else(|| {
                if header_codes.next().is_none() {
                    header_code
                } else {
                    None
                }
            })
            .filter(|code| {
                (1..=96).contains(&code.len())
                    && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            })
            .map(str::to_owned);
        if let Some(code) = code.as_ref().filter(|code| {
            matches!(
                code.as_str(),
                "AuthorizationHeaderMalformed"
                    | "IncorrectEndpoint"
                    | "InvalidRegion"
                    | "IllegalLocationConstraintException"
            )
        }) {
            return Failure::SigningRegion {
                status,
                code: code.clone(),
            };
        }
        if (300..400).contains(&status) {
            return Failure::Redirect { status, code };
        }
        return Failure::Service { status, code };
    }
    Failure::Request
}

#[path = "compatible/settings.rs"]
pub(super) mod settings;

#[path = "compatible/keyring/mod.rs"]
pub(super) mod keyring;

#[cfg(test)]
#[path = "compatible/tests.rs"]
pub(super) mod tests;
