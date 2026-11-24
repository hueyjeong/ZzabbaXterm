use std::net::{IpAddr, SocketAddr};

use ironrdp::connector::DesktopSize;
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::{DnsName, ServerName};

use crate::tunnels::{Destination, ipc::{DesktopLease, Protocol}};
use super::{Error, Target};

/// TLS 인증서를 확인할 서버 이름이다. 프로필 저장과 같은 규칙으로 DNS 이름 또는 표준 표기의 IP 주소만 받는다.
/// IP 주소는 인증서의 IP SAN으로 확인한다.
pub(super) fn server_name(name: &str) -> Option<ServerName<'static>> {
    if !crate::connections::profile::rdp_server_name(name) { return None; }
    match name.parse::<IpAddr>() {
        Ok(ip) => Some(ServerName::from(ip)),
        Err(_) => DnsName::try_from(name.to_owned()).ok().map(ServerName::DnsName),
    }
}

/// 승인 창과 신뢰 창에 보이는 `호스트:포트`다. IPv6 주소는 괄호로 감싼다.
pub(super) fn host_port(host: &str, port: u16) -> String {
    match host.parse::<IpAddr>() {
        Ok(ip) => SocketAddr::new(ip, port).to_string(),
        Err(_) => format!("{host}:{port}"),
    }
}

pub(super) enum Endpoint {
    Direct(u16),
    /// 첫 소켓을 쓰고 나서도 `listen`으로 같은 로컬 전달에 다시 연결할 수 있다.
    Forwarded { stream: Option<TcpStream>, listen: std::net::SocketAddr },
}

pub(super) fn direct(dns: String, port: u16, size: DesktopSize, tls_only: bool) -> Target {
    Target { endpoint: Endpoint::Direct(port), server_name: dns, size, tls_only }
}

pub(super) fn forwarded(dns: String, destination: &Destination, size: DesktopSize, lease: &mut DesktopLease, tls_only: bool) -> Result<Target, Error> {
    if lease.protocol() != Protocol::Rdp || lease.destination() != destination { return Err(Error::Tcp); }
    let stream = lease.take_stream().map_err(|_| Error::Transport)?;
    stream.set_nonblocking(true).map_err(|_| Error::Tcp)?;
    let stream = TcpStream::from_std(stream).map_err(|_| Error::Tcp)?;
    let listen = stream.peer_addr().map_err(|_| Error::Tcp)?;
    Ok(Target { endpoint: Endpoint::Forwarded { stream: Some(stream), listen }, server_name: dns, size, tls_only })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_server_names_are_checked_against_the_ip_san_and_numeric_tails_are_rejected() {
        assert!(matches!(server_name("10.0.1.25"), Some(ServerName::IpAddress(_))));
        assert!(matches!(server_name("2001:db8::25"), Some(ServerName::IpAddress(_))));
        assert!(matches!(server_name("rdp.example.test"), Some(ServerName::DnsName(_))));
        for name in ["", "2852039166", "127.1", "0x7f000001", "2001:0db8::25", "2001:DB8::25", "[2001:db8::25]",
            "fe80::1%eth0", "0.0.0.0", "::", "rdp..example.test"] {
            assert!(server_name(name).is_none(), "{name}");
        }
        assert_eq!(host_port("2001:db8::25", 3389), "[2001:db8::25]:3389");
        assert_eq!(host_port("rdp.example.test", 3389), "rdp.example.test:3389");
    }
}
