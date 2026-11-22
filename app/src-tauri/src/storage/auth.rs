use aws_config::{provider_config::ProviderConfig, sso::SsoCredentialsProvider};
use aws_sdk_s3::config::Region;
use aws_smithy_http_client::{Builder, tls};
use aws_smithy_runtime_api::client::{
    http::{HttpClient, HttpConnector, HttpConnectorFuture, SharedHttpConnector, http_client_fn},
    orchestrator::HttpRequest,
    result::ConnectorError,
};

use crate::ssm::DirectSsoConfig;

pub(super) fn sso_cache_needs_login(error: &(dyn std::error::Error + 'static)) -> bool {
    // aws-config 1.13.3의 캐시 오류는 비공개이며 세션 래퍼가 cause만 구현한다.
    #[allow(deprecated)]
    let cache_error = if error.to_string() == "failed to load the cached SSO token" {
        let Some(cause) = error.cause() else {
            return false;
        };
        cause
    } else {
        error
    };
    let message = cache_error.to_string();
    message == "the SSO token has expired and cannot be refreshed"
        || (message.starts_with("failed to read `")
            && message.ends_with(".json`")
            && (message.contains(".aws/sso/cache") || message.contains(".aws\\sso\\cache"))
            && cache_error
                .source()
                .and_then(|cause| cause.downcast_ref::<std::io::Error>())
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound))
}

pub(super) fn provider(config: DirectSsoConfig) -> SsoCredentialsProvider {
    let https = Builder::new()
        .tls_provider(tls::Provider::Rustls(
            tls::rustls_provider::CryptoMode::AwsLc,
        ))
        .build_https();
    provider_with(config, https)
}

pub(super) fn provider_with(config: DirectSsoConfig, https: impl HttpClient + 'static) -> SsoCredentialsProvider {
    let mut builder = SsoCredentialsProvider::builder()
        .configure(&restricted_config(&config.region, https))
        .region(Region::new(config.region))
        .account_id(config.account)
        .role_name(config.role)
        .start_url(config.start_url);
    builder.set_session_name(config.session);
    builder.build()
}

fn restricted_config(region: &str, https: impl HttpClient + 'static) -> ProviderConfig {
    let suffix = super::dns_suffix(region);
    let portal = format!("portal.sso.{region}.{suffix}");
    let oidc = format!("oidc.{region}.{suffix}");
    // SDK의 내부 SSO 클라이언트가 서비스별 endpoint 설정을 다시 읽는다.
    let https = http_client_fn(move |settings, components| {
        SharedHttpConnector::new(SsoConnector {
            inner: https.http_connector(settings, components),
            portal: portal.clone(),
            oidc: oidc.clone(),
        })
    });
    ProviderConfig::without_region()
        .with_region(Some(Region::new(region.to_owned())))
        .with_use_fips(Some(false))
        .with_use_dual_stack(Some(false))
        .with_http_client(https)
}

#[derive(Debug)]
struct SsoConnector {
    inner: SharedHttpConnector,
    portal: String,
    oidc: String,
}

impl HttpConnector for SsoConnector {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        if request.uri().parse::<http::Uri>().is_ok_and(|uri| {
            uri.scheme_str() == Some("https")
                && uri.authority().is_some_and(|host| {
                    (host.as_str() == self.portal && uri.path() == "/federation/credentials")
                        || (host.as_str() == self.oidc && uri.path() == "/token")
                })
        }) {
            return self.inner.call(request);
        }
        HttpConnectorFuture::ready(Err(ConnectorError::user("SSO endpoint rejected".into())))
    }
}

#[cfg(test)]
pub(super) async fn assert_sso_endpoint_boundary() {
    use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
    use aws_smithy_types::body::SdkBody;

    let replay = StaticReplayClient::new(vec![ReplayEvent::new(
        http::Request::new(SdkBody::empty()),
        http::Response::new(SdkBody::empty()),
    )]);
    let connector = SsoConnector {
        inner: SharedHttpConnector::new(replay.clone()),
        portal: "portal.sso.eu-west-1.amazonaws.com".into(),
        oidc: "oidc.eu-west-1.amazonaws.com".into(),
    };
    for uri in [
        "https://external.example/federation/credentials",
        "https://portal.sso.eu-west-1.amazonaws.com.external.example/federation/credentials",
        "https://portal.sso.eu-west-1.amazonaws.com/not-sso",
    ] {
        let request: HttpRequest = http::Request::builder()
            .uri(uri)
            .body(SdkBody::empty())
            .unwrap()
            .try_into()
            .unwrap();
        assert!(connector.call(request).await.is_err());
    }
    assert_eq!(replay.actual_requests().count(), 0);
    let request = http::Request::builder()
        .uri("https://portal.sso.eu-west-1.amazonaws.com/federation/credentials")
        .body(SdkBody::empty())
        .unwrap()
        .try_into()
        .unwrap();
    assert!(connector.call(request).await.is_ok());
    assert_eq!(replay.actual_requests().count(), 1);
}
