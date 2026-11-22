use std::net::{IpAddr, TcpStream as StdTcpStream};

use ironrdp::connector::DesktopSize;
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_util::sync::CancellationToken;

use super::{Error, Target};

pub(super) fn dns_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 253 && name.parse::<IpAddr>().is_err()
        && matches!(ServerName::try_from(name), Ok(ServerName::DnsName(_)))
        && name.split('.').all(|label| !label.is_empty() && label.len() <= 63
            && !label.starts_with('-') && !label.ends_with('-')
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

pub(super) enum Endpoint {
    Direct(u16),
    Forwarded(TcpStream),
}

#[allow(dead_code, reason = "R01 어댑터 계약")]
pub(crate) enum Route {
    Direct,
    Forwarded(Forward),
}

/// R01 hands off a connected socket whose peer ownership it has already checked.
/// The adapter cancels `lost` when forwarding ends; dropping the lease releases that owner.
#[allow(dead_code, reason = "R01 어댑터 계약")]
pub(crate) struct Forward {
    stream: Option<StdTcpStream>,
    dns: String,
    port: u16,
    lost: CancellationToken,
    release: Option<Box<dyn FnOnce() + Send + Sync>>,
}

#[allow(dead_code, reason = "R01 어댑터 계약")]
impl Forward {
    pub(crate) fn approved(stream: StdTcpStream, dns: String, port: u16, lost: CancellationToken,
        release: Box<dyn FnOnce() + Send + Sync>) -> Result<Self, Error> {
        let loopback = stream.peer_addr().is_ok_and(|address| address.ip().is_loopback());
        let forward = Self { stream: Some(stream), dns: dns.to_ascii_lowercase(), port, lost, release: Some(release) };
        if !loopback || port == 0 || !dns_name(&forward.dns) {
            return Err(Error::Tcp);
        }
        Ok(forward)
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        drop(self.stream.take());
        if let Some(release) = self.release.take() { release(); }
    }
}

impl Route {
    pub(super) fn target(&mut self, dns: String, port: u16, size: DesktopSize) -> Result<Target, Error> {
        if !dns_name(&dns) || port == 0 { return Err(Error::InvalidServerName); }
        let endpoint = match self {
            Self::Direct => Endpoint::Direct(port),
            Self::Forwarded(lease) => {
                if lease.lost.is_cancelled() || lease.port != port || lease.dns != dns { return Err(Error::Tcp); }
                let stream = lease.stream.take().ok_or(Error::Closed)?;
                stream.set_nonblocking(true).map_err(|_| Error::Tcp)?;
                Endpoint::Forwarded(TcpStream::from_std(stream).map_err(|_| Error::Tcp)?)
            }
        };
        Ok(Target { endpoint, server_name: dns, size })
    }

    pub(super) fn loss(&self) -> Option<CancellationToken> {
        match self {
            Self::Direct => None,
            Self::Forwarded(lease) => Some(lease.lost.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn forwarded_lease_preserves_remote_dns_and_never_falls_back() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let socket = StdTcpStream::connect(addr).unwrap();
        let lost = CancellationToken::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut route = Route::Forwarded(Forward::approved(socket, "RDP.example.invalid".into(), 3389,
            lost.clone(), Box::new(move || tx.send(()).unwrap())).unwrap());
        let size = DesktopSize { width: 640, height: 480 };
        assert!(route.target("other.example.invalid".into(), 3389, size).is_err());
        assert!(route.target("rdp.example.invalid".into(), 3390, size).is_err());
        let verified = route.target("rdp.example.invalid".into(), 3389, size).unwrap();
        let Endpoint::Forwarded(stream) = verified.endpoint else { panic!("direct fallback"); };
        assert_eq!(stream.peer_addr().unwrap(), addr);
        assert_eq!(verified.server_name, "rdp.example.invalid");
        assert!(route.target("rdp.example.invalid".into(), 3389, size).is_err());
        lost.cancel();
        assert!(route.loss().unwrap().is_cancelled());
        drop(stream);
        drop(route);
        rx.recv().unwrap();
    }
}
