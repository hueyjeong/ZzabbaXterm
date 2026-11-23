use std::{future::Future, io};
#[cfg(test)]
use std::pin::Pin;

use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_native_tls::{TlsConnector, native_tls};
use zeroize::Zeroizing;

use crate::tunnels::ipc::{DesktopLease, Protocol};
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

#[cfg(test)]
pub(crate) trait TunnelRegistry: Sync {
    fn open_verified<'a>(
        &'a self,
        owner: &'a str,
        tunnel_id: &'a str,
        target: &'a Target,
    ) -> Pin<Box<dyn Future<Output = Result<VerifiedTunnel, Error>> + Send + 'a>>;
}

#[cfg(test)]
pub(crate) struct VerifiedTunnel {
    pub(crate) target: Target,
    pub(crate) stream: TcpStream,
}

pub(crate) enum Route<'a> {
    DirectTls {
        trust_anchor_pem: Option<&'a [u8]>,
    },
    R01 { lease: &'a DesktopLease, stream: std::net::TcpStream },
    #[cfg(test)]
    Fixture {
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
        Route::R01 { lease, stream } => {
            let destination = lease.destination();
            if lease.protocol() != Protocol::Vnc || target.host != destination.host || target.port != destination.port ||
                !stream.peer_addr()?.ip().is_loopback()
            {
                return Err(Error::UnverifiedRoute);
            }
            lease.while_live(|| ()).ok_or(Error::Disconnected)?;
            stream.set_nonblocking(true)?;
            Ok(Box::new(TcpStream::from_std(stream)?))
        }
        #[cfg(test)]
        Route::Fixture {
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
