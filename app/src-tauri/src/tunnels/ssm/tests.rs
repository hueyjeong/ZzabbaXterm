use std::{
    io::{ErrorKind, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, atomic::{AtomicBool, Ordering}},
    time::{Duration, Instant},
};

use super::*;
use crate::connections::{SavedSsm, profile::Ssm};

pub(super) const SESSION: &str = "fixture-0123456789abcdef0";
const NODE: &str = "i-07c8e1d461943e5f3";

pub(super) fn fixture_program() -> &'static Path {
    static PROGRAM: OnceLock<PathBuf> = OnceLock::new();
    PROGRAM.get_or_init(|| {
        let root = tempfile::tempdir().unwrap().keep();
        let program = root.join(format!("ssm forward fixture{}", std::env::consts::EXE_SUFFIX));
        let compiled = std::process::Command::new(option_env!("RUSTC").unwrap_or("rustc"))
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tunnels/ssm/fixture.rs"))
            .arg("-o").arg(&program).output().unwrap();
        assert!(compiled.status.success(), "{}", String::from_utf8_lossy(&compiled.stderr));
        program
    })
}

// 이 시험 PC의 Winsock은 bind·connect에서 일시적인 WSAEFAULT를 돌려줄 때가 있어 시험 보조만 다시 시도한다.
pub(in crate::tunnels) fn bind_loopback() -> TcpListener {
    let mut last = None;
    for _ in 0..200 {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
            Ok(listener) => return listener,
            Err(error) => last = Some(error),
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("루프백 bind 실패: {last:?}");
}

pub(in crate::tunnels) fn connect(address: SocketAddr) -> TcpStream {
    let mut last = None;
    for _ in 0..200 {
        match TcpStream::connect_timeout(&address, Duration::from_secs(2)) {
            Ok(stream) => return stream,
            Err(error) => last = Some(error),
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("루프백 connect 실패: {last:?}");
}

/// 연결이 한 번도 받아들여지지 않고 거부 응답이 관찰되면 수신이 닫힌 것으로 본다.
/// Windows 루프백은 거부 응답까지 SYN 재전송으로 약 2초가 걸린다.
pub(in crate::tunnels) fn assert_closed(address: SocketAddr) {
    for _ in 0..5 {
        match TcpStream::connect_timeout(&address, Duration::from_secs(6)) {
            Ok(_) => panic!("닫은 뒤에도 {address}에서 연결을 받습니다"),
            Err(error) if error.kind() == ErrorKind::ConnectionRefused => return,
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    panic!("{address}의 연결 거부를 관찰하지 못했습니다");
}

fn free_port() -> SocketAddr {
    bind_loopback().local_addr().unwrap()
}

fn wait(child: &mut SsmChild) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match child.observe() {
            Observation::Listening => return Ok(()),
            Observation::Ended(reason) => return Err(reason),
            Observation::Pending if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Observation::Pending => panic!("SSM 시험 준비 대기 시간 초과"),
        }
    }
}

fn parse(lines: &[&str]) -> Result<Option<(u16, String)>, OutputFault> {
    let mut output = PluginOutput::new();
    let mut ready = None;
    for line in lines {
        if let Some(found) = output.line(line)? { ready = Some(found); }
    }
    Ok(ready)
}

#[test]
fn plugin_output_requires_session_port_and_waiting_in_order() {
    let session = "zzabba-0a1b2c3d4e5f67890";
    let ready = parse(&["\r\n", &format!("Starting session with SessionId: {session}\r\n"),
        &format!("Port 50000 opened for sessionId {session}.\r\n"), "Waiting for connections...\r\n", "\n"]);
    assert_eq!(ready, Ok(Some((50000, session.into()))));
    // 준비 뒤의 연결별 문장은 판정에 쓰지 않는다.
    let mut output = PluginOutput::new();
    for line in [format!("Starting session with SessionId: {session}"), format!("Port 50000 opened for sessionId {session}."), "Waiting for connections...".into()] {
        output.line(&line).unwrap();
    }
    assert!(output.is_ready());
    for after in ["Connection accepted for session [x]", "Waiting for connections...", "anything else"] {
        assert_eq!(output.line(after), Ok(None));
    }
}

#[test]
fn plugin_output_variants_fail_closed() {
    let start = "Starting session with SessionId: fixture-1";
    for (lines, fault) in [
        (vec!["Port 50000 opened for sessionId fixture-1.", "Waiting for connections..."], OutputFault::Format),
        (vec![start, "Waiting for connections..."], OutputFault::Format),
        (vec![start, "Port 50000 opened for sessionId fixture-2.", "Waiting for connections..."], OutputFault::SessionMismatch),
        (vec![start, "Port 50000 opened for sessionId fixture-1"], OutputFault::Format),
        (vec![start, "Port 0 opened for sessionId fixture-1."], OutputFault::Format),
        (vec![start, "Port 70000 opened for sessionId fixture-1."], OutputFault::Format),
        (vec![start, "Port +5000 opened for sessionId fixture-1."], OutputFault::Format),
        (vec![start, "Port 50000 opened for sessionId fixture-1.", "Port 50001 opened for sessionId fixture-1."], OutputFault::Format),
        (vec![start, start], OutputFault::Format),
        (vec!["Starting session with SessionId: bad id"], OutputFault::Format),
        (vec!["Starting session with SessionId: -debug"], OutputFault::Format),
        (vec!["SessionId: fixture-1"], OutputFault::Format),
        (vec![start, "Unexpected banner"], OutputFault::Format),
        (vec![start, "Cannot perform start session: listen tcp 127.0.0.1:50000: bind: address already in use"], OutputFault::BindConflict),
        (vec![start, "Cannot perform start session: listen tcp 127.0.0.1:50000: bind: Only one usage of each socket address (protocol/network address/port) is normally permitted."], OutputFault::BindConflict),
        (vec![start, "Cannot perform start session: EOF"], OutputFault::PluginFailed),
    ] {
        assert_eq!(parse(&lines), Err(fault), "{lines:?}");
    }
    assert_eq!(parse(&[start, "Port 50000 opened for sessionId fixture-1."]), Ok(None));
}

#[test]
fn start_session_rejections_are_distinguished() {
    let rejection = |code: &str, message: &str| format!(
        "\nAn error occurred ({code}) when calling the StartSession operation: {message}\n");
    let document = "User: arn:aws:sts::123456789012:assumed-role/Sample/Sample is not authorized to perform: ssm:StartSession on resource: arn:aws:ssm:ap-northeast-1::document/AWS-StartPortForwardingSessionToRemoteHost";
    for (stderr, expected) in [
        (rejection("AccessDeniedException", document), Some("SSM: 포트 전달 문서 실행 권한 거부")),
        (rejection("AccessDeniedException", "not authorized on instance"), Some("SSM: 권한 거부")),
        (format!("aws: [ERROR]: {}", rejection("InvalidDocument", "document missing").trim()), Some("SSM: 포트 전달 문서 실행 거부")),
        (rejection("ExpiredTokenException", "expired"), Some("SSM: 자격 증명 만료")),
        (rejection("UnrecognizedClientException", "invalid token"), Some("SSM: 자격 증명 사용 불가")),
        (rejection("TargetNotConnected", "offline"), Some("SSM: 노드 연결 불가")),
        (rejection("ThrottlingException", "slow down"), Some("SSM: 시작 거부 (ThrottlingException)")),
        ("Error when retrieving token from sso: Token has expired and refresh failed\n".into(), Some("SSM: SSO 재로그인 필요")),
        (rejection("Bad Code", "x"), None),
        ("plugin crashed\n".into(), None),
    ] {
        assert_eq!(start_rejection(stderr.as_bytes()).as_deref(), expected, "{stderr}");
    }
    assert_eq!(start_rejection(b"\xff"), None);
}

fn saved(desktop: bool) -> SavedSsm {
    SavedSsm {
        revision: std::num::NonZeroU64::MIN,
        target: Ssm { profile: "zzabba-demo".into(), region: "ap-northeast-1".into(), instance_id: NODE.into() },
        desktop: desktop.then(|| crate::connections::SavedDesktop {
            protocol: crate::tunnels::ipc::Protocol::Rdp, host: Some("rdp.example.test".into()), port: 3389,
        }),
    }
}

#[test]
fn forward_plan_binds_saved_scope_and_fixed_document_arguments() {
    let listen = SocketAddr::from((Ipv4Addr::LOCALHOST, 50022));
    let node_route = Route::Ssm { listen, node: NODE.into(), port: 22, host: None };
    let plan = ForwardPlan::new(&node_route, &node_route, &saved(false)).unwrap();
    assert_eq!((plan.document(), plan.parameters()), ("AWS-StartPortForwardingSession", "portNumber=22,localPortNumber=50022"));
    assert_eq!((plan.scope.profile(), plan.scope.region(), plan.node.as_str()), ("zzabba-demo", "ap-northeast-1", NODE));
    let remote = Route::Ssm { listen, node: NODE.into(), port: 3389, host: Some("rdp.example.test".into()) };
    let plan = ForwardPlan::new(&remote, &remote, &saved(true)).unwrap();
    assert_eq!((plan.document(), plan.parameters()),
        ("AWS-StartPortForwardingSessionToRemoteHost", "host=rdp.example.test,portNumber=3389,localPortNumber=50022"));
    for (route, approved) in [
        (Route::Ssm { listen, node: "i-00000000000000000".into(), port: 22, host: None }, None),
        (node_route.clone(), Some(Route::Ssm { listen, node: NODE.into(), port: 23, host: None })),
        (Route::Ssm { listen: SocketAddr::from(([0, 0, 0, 0], 50022)), node: NODE.into(), port: 22, host: None }, None),
        (Route::Ssm { listen, node: NODE.into(), port: 22, host: Some("a,b".into()) }, None),
    ] {
        let approved = approved.unwrap_or_else(|| route.clone());
        assert!(ForwardPlan::new(&route, &approved, &saved(false)).err().is_some_and(|error| error.starts_with("SSM 전달 승인")));
    }
}

#[test]
fn fake_cli_tree_becomes_ready_only_after_verified_session_and_preserves_bytes() {
    let listen = free_port();
    let terminated = Arc::new(AtomicBool::new(false));
    let mut child = fixture_child("cli", listen, None, Ok(SESSION), Some(terminated.clone())).unwrap();
    wait(&mut child).unwrap();
    let mut stream = child.open_stream(listen).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let payload: Vec<u8> = (0..256 * 1024).map(|index| (index % 256) as u8).chain(*b"\r\n\x1b\x00").collect();
    let writer = { let mut stream = stream.try_clone().unwrap(); let payload = payload.clone();
        std::thread::spawn(move || stream.write_all(&payload).unwrap()) };
    let mut echoed = vec![0; payload.len()];
    stream.read_exact(&mut echoed).unwrap();
    writer.join().unwrap();
    assert!(echoed == payload, "에코 바이트 불일치");
    assert!(child.open_stream(free_port()).is_err());
    drop(stream);
    child.close().unwrap();
    assert_closed(listen);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !terminated.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "원격 세션 종료 작업이 실행되지 않았습니다");
        std::thread::sleep(Duration::from_millis(10));
    }
    child.close().unwrap();
}

#[test]
fn fake_cli_failures_are_reported_by_stage_and_cleaned() {
    let occupied = bind_loopback();
    let address = occupied.local_addr().unwrap();
    let error = fixture_child("cli", address, None, Ok(SESSION), None).err().unwrap();
    assert_eq!(error, format!("로컬 수신 주소 사용 중: {address}"));
    let mut client = connect(address);
    let (mut peer, _) = occupied.accept().unwrap();
    peer.write_all(b"kept").unwrap();
    let mut answer = [0; 4];
    client.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"kept");

    for (mode, printed, verified, expected) in [
        ("reject", None, Ok(SESSION), "SSM: 포트 전달 문서 실행 권한 거부".to_owned()),
        ("garbage", None, Ok(SESSION), FORMAT.to_owned()),
        ("cli", None, Ok("another-session"), OWNERSHIP.to_owned()),
        ("cli", None, Err("SSM 세션 연결 확인 불가"), "SSM 세션 연결 확인 불가".to_owned()),
    ] {
        let listen = free_port();
        let terminated = Arc::new(AtomicBool::new(false));
        let mut child = fixture_child(mode, listen, printed, verified, Some(terminated.clone())).unwrap();
        assert_eq!(wait(&mut child).err(), Some(expected), "{mode}");
        child.close().unwrap();
        assert_closed(listen);
        // StartSession 응답 전 거부에는 원격 세션이 없으므로 종료 조회를 하지 않는다.
        if mode == "reject" {
            std::thread::sleep(Duration::from_millis(100));
            assert!(!terminated.load(Ordering::SeqCst));
        }
    }

    let listen = free_port();
    let printed = free_port().port();
    let mut child = fixture_child("cli", listen, Some(printed), Ok(SESSION), None).unwrap();
    assert_eq!(wait(&mut child).err(), Some(format!("SSM 플러그인 수신 주소 불일치: 127.0.0.1:{printed}")));
    child.close().unwrap();

    let listen = free_port();
    let mut child = fixture_child("bind", listen, None, Ok(SESSION), None).unwrap();
    assert_eq!(wait(&mut child).err(), Some(format!("로컬 수신 주소 사용 중: {listen}")));
    child.close().unwrap();
}

/// 실기 확인용. 기본값은 데모 노드다. `ZZABBA_SSM_FORWARD_HOLD_SECS`를 주면 실제 전달을 열어 그 시간 동안 유지한다.
/// cargo test --lib tunnels::ssm::tests::real_cli_forward -- --ignored --nocapture
#[test]
#[ignore = "로컬 AWS CLI·Session Manager 플러그인과 SSO 로그인 필요"]
fn real_cli_forward() {
    let env = |key: &str, default: &str| std::env::var(key).unwrap_or_else(|_| default.into());
    let listen = SocketAddr::from((Ipv4Addr::LOCALHOST, env("ZZABBA_SSM_FORWARD_LISTEN", "50091").parse().unwrap()));
    let port = env("ZZABBA_SSM_FORWARD_PORT", "9009").parse().unwrap();
    let host = std::env::var("ZZABBA_SSM_FORWARD_HOST").ok().filter(|host| !host.is_empty());
    let saved = SavedSsm {
        revision: std::num::NonZeroU64::MIN,
        target: Ssm {
            profile: env("ZZABBA_SSM_FORWARD_PROFILE", "zzabba-demo"),
            region: env("ZZABBA_SSM_FORWARD_REGION", "ap-northeast-1"),
            instance_id: env("ZZABBA_SSM_FORWARD_NODE", NODE),
        },
        desktop: None,
    };
    let route = Route::Ssm { listen, node: saved.target.instance_id.clone(), port, host };
    let plan = ForwardPlan::new(&route, &route, &saved).unwrap();
    println!("문서: {} · 매개변수: {}", plan.document(), plan.parameters());
    let prepared = match tauri::async_runtime::block_on(prepare(plan, &CancellationToken::new())) {
        Ok(prepared) => prepared,
        Err(error) => { println!("준비 실패: {error}"); return; }
    };
    let Some(hold) = std::env::var("ZZABBA_SSM_FORWARD_HOLD_SECS").ok().and_then(|secs| secs.parse().ok()) else {
        println!("준비 완료: 실행 인가 발급(세션 미시작)");
        return;
    };
    let mut child = prepared.spawn().unwrap();
    match wait(&mut child) {
        Ok(()) => println!("수신 준비: {listen} · {hold}초 유지"),
        Err(error) => { println!("시작 실패: {error}"); child.close().unwrap(); return; }
    }
    let deadline = Instant::now() + Duration::from_secs(hold);
    while Instant::now() < deadline {
        if let Observation::Ended(reason) = child.observe() { println!("끊김: {reason}"); break; }
        std::thread::sleep(Duration::from_millis(250));
    }
    child.close().unwrap();
    // 원격 terminate-session 확인은 별도 작업자에서 이어진다.
    std::thread::sleep(Duration::from_secs(8));
    println!("로컬 정리 완료: {listen}");
}
