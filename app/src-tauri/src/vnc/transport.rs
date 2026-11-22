use std::{future::Future, io, pin::Pin};

use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_native_tls::{TlsConnector, native_tls};
use zeroize::Zeroizing;

use super::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Target {
    pub(crate) profile_id: String,
    pub(crate) host: String,
    pub(crate) port: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Auth {
    VncPassword,
    ExplicitNone,
}

// 등록부가 소유 ID, 살아 있는 터널, 인증된 동일 호스트 목적지를 검사하고
// 해당 터널에 묶인 스트림을 연다. 주소만 반환하면 교체된 리스너와 경합한다.
pub(crate) trait TunnelRegistry: Sync {
    fn open_verified<'a>(
        &'a self,
        owner: &'a str,
        tunnel_id: &'a str,
        target: &'a Target,
    ) -> Pin<Box<dyn Future<Output = Result<VerifiedTunnel, Error>> + Send + 'a>>;
}

pub(crate) struct VerifiedTunnel {
    pub(crate) target: Target,
    pub(crate) stream: TcpStream,
}

pub(crate) enum Route<'a> {
    DirectTls {
        trust_anchor_pem: Option<&'a [u8]>,
    },
    #[allow(dead_code)] // 실제 등록부 어댑터가 통합되기 전에는 테스트에서만 연다.
    R01 {
        registry: &'a dyn TunnelRegistry,
        owner: &'a str,
        tunnel_id: &'a str,
    },
}

// 프로필과 실제 호스트·서비스 포트가 함께 비밀 조회 키를 이룬다.
pub(crate) trait Secrets: Sync {
    fn vnc_password(&self, verified_target: &Target) -> impl Future<Output = Option<Zeroizing<Vec<u8>>>> + Send;
    fn while_live(&self, write: &mut dyn FnMut()) -> bool;
}

pub(super) trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

pub(super) async fn connect(target: &Target, route: Route<'_>) -> Result<Box<dyn Stream>, Error> {
    validate(target)?;
    match route {
        Route::DirectTls { trust_anchor_pem } => {
            let mut builder = native_tls::TlsConnector::builder();
            builder.min_protocol_version(Some(native_tls::Protocol::Tlsv12));
            if let Some(pem) = trust_anchor_pem {
                let cert =
                    native_tls::Certificate::from_pem(pem).map_err(|_| Error::TlsIdentity)?;
                builder
                    .disable_built_in_roots(true)
                    .add_root_certificate(cert);
            }
            let connector = builder.build().map_err(|_| Error::TlsIdentity)?;
            let tcp = TcpStream::connect((target.host.as_str(), target.port)).await?;
            let tls = TlsConnector::from(connector)
                .connect(&target.host, tcp)
                .await
                .map_err(|_| Error::TlsIdentity)?;
            Ok(Box::new(tls))
        }
        Route::R01 {
            registry,
            owner,
            tunnel_id,
        } => {
            if owner.is_empty() || tunnel_id.is_empty() {
                return Err(Error::UnverifiedRoute);
            }
            let proof = registry.open_verified(owner, tunnel_id, target).await?;
            let peer = proof.stream.peer_addr()?;
            if &proof.target != target || !peer.ip().is_loopback() {
                return Err(Error::UnverifiedRoute);
            }
            Ok(Box::new(proof.stream))
        }
    }
}

pub(super) fn validate(target: &Target) -> Result<(), Error> {
    if target.profile_id.is_empty()
        || target.port == 0
        || target.host.is_empty()
        || !(target.host.parse::<std::net::IpAddr>().is_ok()
            || target.host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            }) && target.host.len() <= 253)
    {
        return Err(Error::UnverifiedRoute);
    }
    Ok(())
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}
