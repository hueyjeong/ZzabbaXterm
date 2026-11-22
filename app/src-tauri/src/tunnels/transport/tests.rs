use super::*;
use super::super::Destination;
use std::{
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    time::Duration,
};

const SSH: &str = "approved-ssh-connection";
const NODE: &str = "i-0123456789abcdef0";
const SCOPE: SsmScope<'static> = SsmScope { profile: "Fixture SSO", region: "us-east-2" };

fn local(port: u16) -> SocketAddr {
    local_loopback(NonZeroU16::new(port).unwrap())
}

fn local_route(listen: SocketAddr, host: &str) -> Route {
    Route::SshLocal { server: SSH.into(), listen, destination: Destination { host: host.into(), port: 9443 } }
}

fn remote_route(listen: SocketAddr) -> Route {
    Route::SshRemote { server: SSH.into(), listen, destination: Destination { host: "::1".into(), port: 8080 } }
}

fn ssm_route(listen: SocketAddr, node: &str) -> Route {
    Route::Ssm { listen, node: node.into(), port: 3389 }
}

#[test]
fn fixed_ssh_modes_have_distinct_arguments_and_sides() {
    use SshServerPolicy::*;
    let on_server = "[::1]:15022".parse().unwrap();
    let on_client = local(15443);
    for (route, policy, flag, forward) in [
        (local_route(on_client, "internal.example.invalid"), FixedLocal, "-L", "127.0.0.1:15443:internal.example.invalid:9443"),
        (remote_route(on_server), ExactRemoteLoopback, "-R", "[::1]:15022:[::1]:8080"),
        (Route::SshDynamic { server: SSH.into(), listen: on_client }, DestinationRestrictedSocks, "-D", "127.0.0.1:15443"),
    ] {
        let plan = SshPlan::new(&route, &route, policy).unwrap();
        assert_eq!(plan.options(SSH).unwrap(), [
            "-N", "-T", "-e", "none", "-o", "ExitOnForwardFailure=yes", "-o",
            "ClearAllForwardings=no", "-o", "GatewayPorts=no", flag, forward,
        ]);
        assert_eq!(plan.options("different-connection"), Err(PlanError::ApprovalMismatch));
        let (accepted, wrong_side, expected_error) = if flag == "-R" {
            (Event::SshRemoteAccepted(on_server), Event::SshLocalBound(on_server), format!("원격 수신 주소 사용 중: {on_server}"))
        } else {
            (Event::SshLocalBound(on_client), Event::SshRemoteAccepted(on_client), format!("로컬 수신 주소 사용 중: {on_client}"))
        };
        assert!(matches!(plan.observe(wrong_side), Observation::Pending));
        assert!(matches!(plan.observe(accepted), Observation::Listening));
        assert!(matches!(plan.observe(Event::Ended(Failure::BindConflict)), Observation::Ended(message) if message == expected_error));
    }
}

#[test]
fn rejects_changed_endpoint_policy_and_forwarding_syntax() {
    use SshServerPolicy::*;
    let approved = remote_route(local(8080));
    assert_eq!(SshPlan::new(&remote_route(local(8081)), &approved, ExactRemoteLoopback).err(), Some(PlanError::ApprovalMismatch));
    assert_eq!(SshPlan::new(&approved, &approved, FixedLocal).err(), Some(PlanError::ServerPolicyRequired));
    let socks = Route::SshDynamic { server: SSH.into(), listen: local(1080) };
    assert_eq!(SshPlan::new(&socks, &socks, FixedLocal).err(), Some(PlanError::ServerPolicyRequired));
    for address in ["0.0.0.0:8080", "127.0.0.2:8080", "127.0.0.1:0"] {
        let route = remote_route(address.parse().unwrap());
        assert_eq!(SshPlan::new(&route, &route, ExactRemoteLoopback).err(), Some(PlanError::InvalidListener));
    }
    for host in ["*", "[::1]", "host:443", "host other", "/tmp/socket", "host\nRemoteForward"] {
        let route = local_route(local(8080), host);
        assert_eq!(SshPlan::new(&route, &route, FixedLocal).err(), Some(PlanError::InvalidDestination));
    }
}

#[test]
fn ssm_requires_c02_scope_and_uses_only_the_fixed_node_port_document() {
    let listen = local(13389);
    let route = ssm_route(listen, NODE);
    let other_profile = SsmScope { profile: "Other SSO", ..SCOPE };
    let other_region = SsmScope { region: "us-west-2", ..SCOPE };
    assert_eq!(SsmPlan::new(&route, other_profile, &route, SCOPE).err(), Some(PlanError::ApprovalMismatch));
    let plan = SsmPlan::new(&route, SCOPE, &route, SCOPE).unwrap();
    for (scope, node) in [(other_profile, NODE), (other_region, NODE), (SCOPE, "i-00000000000000000")] {
        assert_eq!(plan.document_options(scope, node), Err(PlanError::ApprovalMismatch));
    }
    assert_eq!(plan.document_options(SCOPE, NODE).unwrap(), [
        "--document-name", "AWS-StartPortForwardingSession", "--parameters", "portNumber=3389,localPortNumber=13389",
    ]);
    let injected = ssm_route(listen, "i---debug");
    assert_eq!(SsmPlan::new(&injected, SCOPE, &injected, SCOPE).err(), Some(PlanError::InvalidDestination));
    let wildcard = ssm_route("0.0.0.0:13389".parse().unwrap(), NODE);
    assert_eq!(SsmPlan::new(&wildcard, SCOPE, &wildcard, SCOPE).err(), Some(PlanError::InvalidListener));
    let event = |address, session_id| Event::SsmPluginBound { address, session_id };
    for (address, id, verified) in [
        (listen, "session-1", None),
        (listen, "session-2", Some("session-1")),
        (local(13390), "session-1", Some("session-1")),
    ] {
        assert!(matches!(plan.observe(event(address, id), verified), Observation::Pending));
    }
    assert!(matches!(plan.observe(event(listen, "session-1"), Some("session-1")), Observation::Listening));
}

#[test]
fn occupied_loopback_is_not_readiness_and_existing_listener_answers() {
    let existing = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = existing.local_addr().unwrap();
    let route = local_route(address, "internal.example.invalid");
    let plan = SshPlan::new(&route, &route, SshServerPolicy::FixedLocal).unwrap();
    let mut client = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let (mut peer, _) = existing.accept().unwrap();
    assert!(matches!(plan.observe(Event::Pending), Observation::Pending));
    // 시험에서만 같은 주소의 실제 bind 결과를 시작 이벤트로 전달한다.
    let bind_error = TcpListener::bind(address).unwrap_err();
    assert_eq!(bind_error.kind(), io::ErrorKind::AddrInUse);
    assert!(matches!(plan.observe(Event::Ended(Failure::BindConflict)), Observation::Ended(message) if message.ends_with(&address.to_string())));
    peer.write_all(b"existing").unwrap();
    let mut answer = [0; 8];
    client.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"existing");
}
