use std::{path::PathBuf, time::{Duration, SystemTime}};

use aws_credential_types::{Credentials, provider::{ProvideCredentials, error::CredentialsError, future::ProvideCredentials as ProvideFuture}};
use aws_sdk_sso::{Client, config::Region};
use aws_smithy_async::time::SharedTimeSource;
use aws_smithy_runtime::expiring_cache::ExpiringCache;
use aws_smithy_runtime_api::client::http::SharedHttpClient;
use aws_smithy_types::{DateTime, retry::RetryConfig};
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::ssm::{DirectSsoConfig, sso_cache_entry};

#[derive(Debug)]
pub(super) struct LoginRequired;

impl std::fmt::Display for LoginRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SSO login required")
    }
}
impl std::error::Error for LoginRequired {}

#[derive(Debug)]
pub(super) struct Provider {
    client: Client,
    account: String,
    role: String,
    token_path: PathBuf,
    clock: SharedTimeSource,
    cache: ExpiringCache<Credentials, CredentialsError>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedToken {
    access_token: String,
    expires_at: String,
}

impl Provider {
    pub(super) fn new(config: DirectSsoConfig, home: PathBuf, transport: SharedHttpClient, clock: SharedTimeSource) -> Self {
        let token_path = home.join(sso_cache_entry(config.cache_key()));
        let region = config.region;
        let endpoint = format!("https://portal.sso.{region}.{}", super::dns_suffix(&region));
        let client = Client::from_conf(aws_sdk_sso::Config::builder()
            .behavior_version_latest()
            .region(Region::new(region))
            .endpoint_url(endpoint)
            .http_client(transport)
            .retry_config(RetryConfig::disabled())
            .build());
        Self { client, account: config.account, role: config.role, token_path, clock,
            cache: ExpiringCache::new(Duration::from_secs(15)) }
    }

    async fn fetch(&self) -> Result<(Credentials, SystemTime), CredentialsError> {
        use std::io::{ErrorKind, Read};
        let path = self.token_path.clone();
        let token = tokio::task::spawn_blocking(move || {
            let io_error = |error: std::io::Error| if error.kind() == ErrorKind::NotFound {
                CredentialsError::provider_error(LoginRequired)
            } else {
                CredentialsError::provider_error(error)
            };
            let metadata = path.symlink_metadata().map_err(io_error)?;
            if !metadata.is_file() || metadata.len() > 1024 * 1024 {
                return Err(CredentialsError::unhandled("invalid SSO cache file"));
            }
            let mut bytes = Zeroizing::new(Vec::new());
            std::fs::File::open(path).and_then(|file| file.take(1024 * 1024 + 1).read_to_end(&mut bytes)).map_err(io_error)?;
            if bytes.len() > 1024 * 1024 {
                return Err(CredentialsError::unhandled("invalid SSO cache size"));
            }
            let token: CachedToken = serde_json::from_slice(&bytes).map_err(CredentialsError::unhandled)?;
            Ok::<_, CredentialsError>((Zeroizing::new(token.access_token), token.expires_at))
        }).await.map_err(CredentialsError::unhandled)??;
        let (token, expires_at) = token;
        let expiration: SystemTime = DateTime::from_str(&expires_at, aws_smithy_types::date_time::Format::DateTime)
            .map_err(CredentialsError::unhandled)?.try_into().map_err(CredentialsError::unhandled)?;
        if token.is_empty() || expiration <= self.clock.now() {
            return Err(CredentialsError::provider_error(LoginRequired));
        }
        let output = self.client.get_role_credentials().account_id(&self.account).role_name(&self.role)
            .access_token(&*token).send().await.map_err(CredentialsError::provider_error)?;
        let role = output.role_credentials.ok_or_else(|| CredentialsError::unhandled("SSO role credentials missing"))?;
        let expiry: SystemTime = DateTime::from_millis(role.expiration).try_into().map_err(CredentialsError::unhandled)?;
        let required = || CredentialsError::unhandled("incomplete SSO role credentials");
        let access = role.access_key_id.filter(|value| !value.is_empty()).ok_or_else(required)?;
        let secret = role.secret_access_key.filter(|value| !value.is_empty()).ok_or_else(required)?;
        let token = role.session_token.filter(|value| !value.is_empty()).ok_or_else(required)?;
        if expiry <= self.clock.now() { return Err(required()); }
        Ok((Credentials::builder().access_key_id(access).secret_access_key(secret).session_token(token)
            .expiry(expiry).account_id(&self.account).provider_name("approved-sso").build(), expiry))
    }
}

impl ProvideCredentials for Provider {
    fn provide_credentials<'a>(&'a self) -> ProvideFuture<'a> where Self: 'a {
        ProvideFuture::new(async move {
            if let Some(credentials) = self.cache.yield_or_clear_if_expired(self.clock.now()).await {
                return Ok(credentials);
            }
            self.cache.get_or_load(|| self.fetch()).await
        })
    }
}
