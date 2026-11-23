use std::net::IpAddr;

use ironrdp::connector::DesktopSize;
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;

use crate::tunnels::{Destination, ipc::{DesktopLease, Protocol}};
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

pub(super) fn direct(dns: String, port: u16, size: DesktopSize) -> Target {
    Target { endpoint: Endpoint::Direct(port), server_name: dns, size }
}

pub(super) fn forwarded(dns: String, destination: &Destination, size: DesktopSize, lease: &mut DesktopLease) -> Result<Target, Error> {
    if lease.protocol() != Protocol::Rdp || lease.destination() != destination { return Err(Error::Tcp); }
    let stream = lease.take_stream().map_err(|_| Error::Transport)?;
    stream.set_nonblocking(true).map_err(|_| Error::Tcp)?;
    let stream = TcpStream::from_std(stream).map_err(|_| Error::Tcp)?;
    Ok(Target { endpoint: Endpoint::Forwarded(stream), server_name: dns, size })
}
