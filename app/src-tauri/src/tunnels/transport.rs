use super::{Observation, Route};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
#[cfg(test)]
use std::num::NonZeroU16;

#[cfg(test)]
pub(crate) fn local_loopback(port: NonZeroU16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port.get()))
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum PlanError {
    ApprovalMismatch,
    ServerPolicyRequired,
    #[cfg(test)]
    WrongTransport,
    InvalidListener,
    InvalidDestination,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Listener {
    Local(SocketAddr),
    Remote(SocketAddr),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SshServerPolicy {
    FixedLocal,
    ExactRemoteLoopback,
    DestinationRestrictedSocks,
}

pub(crate) struct SshPlan {
    // 승인된 접속 계획과 다른 서버에 전달 옵션을 붙이지 않도록 이 값을 대조한다.
    server: String,
    listener: Listener,
    flag: &'static str,
    forward: String,
}

impl SshPlan {
    /// `Route::server`는 F01 호스트·포트·계정과 승인된 인증 방식에 묶인 앱의 접속 식별자다.
    /// `approved`와 `policy`는 IPC 입력이 아니라 승인된 접속 레코드에서 가져온다.
    pub(crate) fn new(
        route: &Route,
        approved: &Route,
        policy: SshServerPolicy,
    ) -> Result<Self, PlanError> {
        if route != approved {
            return Err(PlanError::ApprovalMismatch);
        }
        let (server, flag, listener, destination, required_policy) = match route {
            Route::SshLocal { server, listen, destination } => (
                server, "-L", Listener::Local(*listen), Some(destination),
                SshServerPolicy::FixedLocal,
            ),
            Route::SshRemote { server, listen, destination } => (
                server, "-R", Listener::Remote(*listen), Some(destination),
                SshServerPolicy::ExactRemoteLoopback,
            ),
            Route::SshDynamic { server, listen } => (
                server, "-D", Listener::Local(*listen), None,
                SshServerPolicy::DestinationRestrictedSocks,
            ),
            #[cfg(test)]
            Route::Ssm { .. } => return Err(PlanError::WrongTransport),
        };
        if policy != required_policy {
            return Err(PlanError::ServerPolicyRequired);
        }
        let (Listener::Local(address) | Listener::Remote(address)) = listener;
        // GatewayPorts=no가 그대로 사용하는 정확한 IPv4/IPv6 루프백만 원격 수신에 허용한다.
        if address.port() == 0
            || !address.ip().is_loopback()
            || (matches!(listener, Listener::Remote(_))
                && address.ip() != Ipv4Addr::LOCALHOST
                && address.ip() != std::net::Ipv6Addr::LOCALHOST)
            || matches!(address, SocketAddr::V6(v6) if v6.scope_id() != 0 || v6.flowinfo() != 0)
        {
            return Err(PlanError::InvalidListener);
        }
        let forward = if let Some(destination) = destination {
            if destination.port == 0 {
                return Err(PlanError::InvalidDestination);
            }
            match destination.host.parse::<IpAddr>() {
                Ok(IpAddr::V6(ip)) => format!("{address}:[{ip}]:{}", destination.port),
                Ok(IpAddr::V4(ip)) => format!("{address}:{ip}:{}", destination.port),
                Err(_) if dns_name(&destination.host) => {
                    format!("{address}:{}:{}", destination.host, destination.port)
                }
                Err(_) => return Err(PlanError::InvalidDestination),
            }
        } else {
            address.to_string()
        };
        Ok(Self { server: server.clone(), listener, flag, forward })
    }

    /// 옵션은 F01이 발급한 SSH invocation의 `--` 앞에 넣고, 환경과 신뢰 파일은 그대로 유지한다.
    /// `-R`은 원격 sshd의 `GatewayPorts no` 또는 승인 인터페이스를 그대로 따르는
    /// `clientspecified`와 `PermitListen` 확인이 필요하다. `-D` 목적지는 서버의 `PermitOpen`으로 제한한다.
    pub(crate) fn options(&self, f01_server: &str) -> Result<[&str; 12], PlanError> {
        if self.server != f01_server {
            return Err(PlanError::ApprovalMismatch);
        }
        Ok([
            "-N", "-T", "-e", "none", "-o", "ExitOnForwardFailure=yes", "-o",
            "ClearAllForwardings=no", "-o", "GatewayPorts=no", self.flag, &self.forward,
        ])
    }

    pub(super) fn listener(&self) -> Listener {
        self.listener
    }

    pub(super) fn approved_options(&self) -> [&str; 12] {
        self.options(&self.server).expect("승인된 SSH 전달 계획 필요")
    }

    pub(crate) fn observe(&self, event: Event) -> Observation {
        match event {
            Event::SshLocalBound(address) if self.listener == Listener::Local(address) => {
                Observation::Listening
            }
            Event::SshRemoteAccepted(address) if self.listener == Listener::Remote(address) => {
                Observation::Listening
            }
            _ => event.pending_or_ended(self.listener),
        }
    }
}

fn dns_name(host: &str) -> bool {
    let name = host.strip_suffix('.').unwrap_or(host);
    name.len() <= 253 && name.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SsmScope<'a> {
    pub(crate) profile: &'a str,
    pub(crate) region: &'a str,
}

#[cfg(test)]
pub(crate) struct SsmPlan<'a> {
    listen: SocketAddr,
    node: String,
    scope: SsmScope<'a>,
    document: &'static str,
    parameters: String,
}

#[cfg(test)]
impl<'a> SsmPlan<'a> {
    /// 승인 범위는 C02가 조회한 노드와 프로필·리전을 모두 포함한다.
    pub(crate) fn new(
        route: &Route,
        scope: SsmScope<'a>,
        approved_route: &Route,
        approved_scope: SsmScope<'_>,
    ) -> Result<Self, PlanError> {
        if route != approved_route || scope != approved_scope {
            return Err(PlanError::ApprovalMismatch);
        }
        let Route::Ssm { listen, node, port, host } = route else {
            return Err(PlanError::WrongTransport);
        };
        // 플러그인은 주소 인자를 받지 않고 localhost에 바인드한다. 이 주소가 관찰될 때만 준비로 본다.
        if listen.ip() != Ipv4Addr::LOCALHOST || listen.port() == 0 {
            return Err(PlanError::InvalidListener);
        }
        let suffix = node.strip_prefix("mi-").filter(|s| s.len() == 17).or_else(|| {
            node.strip_prefix("i-").filter(|s| matches!(s.len(), 8 | 17))
        });
        if *port == 0 || !suffix.is_some_and(|s| {
            s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        }) {
            return Err(PlanError::InvalidDestination);
        }
        // 원격 호스트는 문서 매개변수 구분자(`,`·`=`)와 공백·제어 문자가 없는 DNS 이름 또는 IP 리터럴만 받는다.
        let (document, parameters) = match host {
            None => ("AWS-StartPortForwardingSession", format!("portNumber={port},localPortNumber={}", listen.port())),
            Some(host) if host.parse::<IpAddr>().is_ok() || dns_name(host) => (
                "AWS-StartPortForwardingSessionToRemoteHost",
                format!("host={host},portNumber={port},localPortNumber={}", listen.port()),
            ),
            Some(_) => return Err(PlanError::InvalidDestination),
        };
        Ok(Self { listen: *listen, node: node.clone(), scope, document, parameters })
    }

    /// C02의 검증된 요청과 승인 범위를 대조한 뒤 고정 문서 인자만 돌려준다.
    /// C02 소유의 `ssm start-session --region … --output json --target … --reason …`에 붙인다.
    /// 프로필은 C02가 내보낸 자격 증명의 범위다. 실행 인자로 `--profile`을 추가하면 그 자격이 우회된다.
    pub(crate) fn document_options(
        &self,
        c02_scope: SsmScope<'_>,
        c02_node: &str,
    ) -> Result<[&str; 4], PlanError> {
        if self.scope != c02_scope || self.node != c02_node {
            return Err(PlanError::ApprovalMismatch);
        }
        Ok(["--document-name", self.document, "--parameters", &self.parameters])
    }

    pub(crate) fn observe(&self, event: Event, verified_session_id: Option<&str>) -> Observation {
        match event {
            Event::SsmPluginBound { address, session_id }
                if self.listen == address && !session_id.is_empty() && verified_session_id == Some(session_id) =>
            {
                Observation::Listening
            }
            _ => event.pending_or_ended(Listener::Local(self.listen)),
        }
    }
}

/// 소유한 실행 핸들에서 읽은 이벤트다. 수신 준비와 각 사용자의 목적지 응답은 별개다.
pub(crate) enum Event {
    #[cfg(test)]
    Pending,
    // OpenSSH의 "Local forwarding listening on" 로그는 bind 전이므로 준비 근거가 아니다.
    SshLocalBound(SocketAddr),
    // 승인한 sshd의 원격 전달 성공 응답이다. 실제 인터페이스는 서버 정책에 의존한다.
    SshRemoteAccepted(SocketAddr),
    #[cfg(test)]
    SsmPluginBound { address: SocketAddr, session_id: &'static str },
    Ended(Failure),
}

pub(crate) enum Failure {
    BindConflict,
    OwnedProcessExited,
}

impl Event {
    fn pending_or_ended(self, listener: Listener) -> Observation {
        let Self::Ended(failure) = self else {
            return Observation::Pending;
        };
        let reason = match failure {
            Failure::BindConflict => {
                let (side, address) = match listener {
                    Listener::Local(address) => ("로컬", address),
                    Listener::Remote(address) => ("원격", address),
                };
                return Observation::Ended(format!("{side} 수신 주소 사용 중: {address}"));
            }
            Failure::OwnedProcessExited => "터널 프로세스가 종료되었습니다",
        };
        Observation::Ended(reason.into())
    }
}

#[cfg(test)]
#[path = "transport/tests.rs"]
mod tests;
