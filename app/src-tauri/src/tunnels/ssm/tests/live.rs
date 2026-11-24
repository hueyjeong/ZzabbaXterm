//! 실제 AWS SSM 노드 시험이다. 대상은 환경 변수로만 받는다.
//! - `ZZABBA_LIVE_PROFILE`, `ZZABBA_LIVE_REGION`, `ZZABBA_LIVE_NODE`: AWS CLI 프로필, 리전, 관리 노드 ID
//! - `ZZABBA_LIVE_KEY`: 노드 `authorized_keys`에 등록한 개인 키 경로(암호 없음). 같은 경로의 `.pub`를 틀린 호스트 키로 쓴다.
//! - `ZZABBA_LIVE_HOSTKEY`: 노드 `/etc/ssh/ssh_host_ed25519_key.pub` 사본 경로
//!
//! 시험 B는 노드 127.0.0.1:9009 에코 서버가 필요하다(`tests/ssm_forward_bytes.py node-params`).
//! 실행: `cargo test --lib tunnels::ssm::tests::live -- --ignored --nocapture --test-threads=1`
//! 각 시험은 끝에 대상 노드의 Active 세션이 없는지 확인하므로 다른 세션을 연 채 실행하지 않는다.
//! 프로필이 SSO 직접 프로필이 아니면 앱 검증이 거부하므로, 시험 안에서만 자격 내보내기를 사용자 AWS 설정으로 우회한다.

use std::{
    io::{BufRead, BufReader},
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use ssh_key::{HashAlg, PublicKey};

use super::super::{ForwardPlan, SsmChild, open_ssh_forward, prepare};
use crate::{
    connections::profile::Ssm,
    ssh::{ApprovedHost, Authentication, Client, ConnectionPlan, Endpoint, Invocation, Transport, TrustStore, tests::FakeForward},
    tunnels::{Observation, Route, Transport as _},
};
use tokio_util::sync::CancellationToken;

const ECHO_PORT: u16 = 9009;

struct Live {
    target: Ssm,
    key: PathBuf,
    host_key: String,
}

fn live() -> Live {
    let required = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("{key} 환경 변수가 필요합니다"));
    let target = Ssm {
        profile: required("ZZABBA_LIVE_PROFILE"),
        region: required("ZZABBA_LIVE_REGION"),
        instance_id: required("ZZABBA_LIVE_NODE"),
    };
    let scope = crate::ssm::Scope::new(target.profile.clone(), target.region.clone()).unwrap();
    if let Err(error) = scope.direct_sso_config() {
        println!("앱 프로필 검증 거부: {error:?} · 시험 안에서 자격 내보내기만 우회");
        let _ = crate::ssm::LIVE_EXPORT_PROFILE.set(target.profile.clone());
    }
    let host_key = std::fs::read_to_string(required("ZZABBA_LIVE_HOSTKEY")).unwrap().trim().to_owned();
    Live { target, key: required("ZZABBA_LIVE_KEY").into(), host_key }
}

fn fingerprint(key: &str) -> String {
    PublicKey::from_openssh(key).unwrap().fingerprint(HashAlg::Sha256).to_string()
}

fn wait_ready(child: &mut SsmChild) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(70);
    loop {
        match child.observe() {
            Observation::Listening => return Ok(()),
            Observation::Pending if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            ended => {
                let progress = child.progress.lock().unwrap();
                println!("준비 실패 진행: bound={:?} verified={:?} failure={:?} stderr={:?}", progress.bound,
                    progress.verified, progress.failure, String::from_utf8_lossy(&progress.stderr));
                return Err(match ended { Observation::Ended(reason) => reason, _ => "SSM 준비 대기 시간 초과".into() });
            }
        }
    }
}

fn session_id(child: &SsmChild) -> String {
    child.progress.lock().unwrap().verified.clone().expect("확인된 세션 ID")
}

/// 사용자 AWS CLI로 세션 상태를 조회한다. 없으면 `None`이다.
fn session_status(target: &Ssm, id: &str, state: &str) -> Option<String> {
    let output = Command::new("aws").args([
        "ssm", "describe-sessions", "--profile", &target.profile, "--region", &target.region, "--state", state,
        "--filters", &format!("key=SessionId,value={id}"), "--query", "Sessions[0].Status", "--output", "text",
    ]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let status = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (status != "None" && !status.is_empty()).then_some(status)
}

/// 앱의 종료 작업자가 terminate-session을 마칠 때까지 History의 최종 상태를 기다린다.
fn wait_terminated(target: &Ssm, id: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let active = session_status(target, id, "Active");
        let history = session_status(target, id, "History");
        if active.is_none() && history.as_deref() == Some("Terminated") {
            return "Terminated".into();
        }
        if Instant::now() >= deadline {
            return format!("Active={active:?} History={history:?}");
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// 준비에 실패한 시도의 세션까지 앱 종료 작업자가 닫았는지 대상의 Active 목록으로 확인한다.
fn assert_no_active(target: &Ssm) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let output = Command::new("aws").args([
            "ssm", "describe-sessions", "--profile", &target.profile, "--region", &target.region, "--state", "Active",
            "--filters", &format!("key=Target,value={}", target.instance_id), "--query", "Sessions[].SessionId", "--output", "text",
        ]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let active = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if active.is_empty() {
            println!("남은 Active 세션 없음");
            return;
        }
        assert!(Instant::now() < deadline, "남은 Active 세션: {active}");
        std::thread::sleep(Duration::from_secs(3));
    }
}

// 이 PC에서 플러그인이 `Starting session` 뒤 준비 출력 없이 멈추는 경우가 있어(공식 CLI 단독 실행에서도 재현) 준비 실패만 다시 시도한다.
const ATTEMPTS: usize = 5;

fn command(invocation: Invocation<'_>, front: &[&str], remote: &str) -> Command {
    let mut command = Command::new(invocation.program);
    command.args(front).args(invocation.args).arg(remote);
    for &(name, value) in invocation.environment {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
    command.env_remove("BW_SESSION").stdin(Stdio::null());
    command
}

const BATCH: [&str; 2] = ["-T", "-oBatchMode=yes"];

#[test]
#[ignore = "실제 AWS SSM 노드 필요"]
fn live_ssh_over_ssm_end_to_end() {
    let live = live();
    let root = tempfile::tempdir().unwrap();
    let endpoint = Endpoint::new(&live.target.instance_id, 22).unwrap();
    let store = TrustStore::create(&root.path().join("ssh-trust")).unwrap();
    let approved = ApprovedHost::confirm(&endpoint, &endpoint, &fingerprint(&live.host_key)).unwrap();
    store.register(&approved, &live.host_key).unwrap();

    let cancel = CancellationToken::new();
    let (port, forward) = (1..=ATTEMPTS).find_map(|attempt| open_ssh_forward(&live.target, 22, &cancel)
        .inspect_err(|error| println!("A 전달 시도 {attempt} 실패: {error}")).ok()).expect("SSH 전달 준비");
    let id = session_id(&forward);
    println!("A 세션: {id} · 로컬 포트 {port}");
    let plan = Arc::new(ConnectionPlan::prepare(&store, &approved, "ubuntu",
        Transport::Ssm { port, forward: Box::new(forward) }, Authentication::KeyFile { private_key: &live.key }).unwrap());

    let output = command(plan.invocation(Client::Ssh).unwrap(), &BATCH, "echo zzabba-ok; uname -m").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("A ssh 종료 {:?} · 출력 {:?} · 오류 {:?}", output.status.code(), stdout, String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success());
    assert_eq!(stdout.lines().collect::<Vec<_>>(), ["zzabba-ok", "aarch64"]);

    // ssh 세션을 열어 둔 채 같은 계획으로 SFTP를 붙인다. 두 번째 TCP 연결이 같은 전달을 지난다.
    let mut held = command(plan.invocation(Client::Ssh).unwrap(), &BATCH, "echo zzabba-hold; sleep 20")
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut line = String::new();
    BufReader::new(held.stdout.take().unwrap()).read_line(&mut line).unwrap();
    assert_eq!(line.trim_end(), "zzabba-hold");
    let names = tauri::async_runtime::block_on(async {
        let tracker = tokio_util::task::TaskTracker::new();
        let mut session = crate::editor::sftp::Session::connect(plan.clone(), CancellationToken::new(), tracker.token())
            .await.unwrap();
        let entries = session.list(None).await.unwrap();
        session.close().await.unwrap();
        entries.iter().map(|entry| entry.name().to_owned()).collect::<Vec<_>>()
    });
    println!("A SFTP 홈 목록: {names:?}");
    assert!(names.iter().any(|name| name == ".ssh"));
    assert!(held.try_wait().unwrap().is_none(), "SFTP 중 ssh 세션 유지");
    held.kill().unwrap();
    held.wait().unwrap();
    assert_eq!(plan.forward_ended(), None);

    // 다른 키를 PIN으로 등록하면 같은 전달을 지나도 ssh가 호스트 키를 거부한다.
    let other = std::fs::read_to_string(live.key.with_extension("pub")).unwrap();
    let wrong_root = tempfile::tempdir().unwrap();
    let wrong_store = TrustStore::create(&wrong_root.path().join("ssh-trust")).unwrap();
    let wrong = ApprovedHost::confirm(&endpoint, &endpoint, &fingerprint(other.trim())).unwrap();
    wrong_store.register(&wrong, other.trim()).unwrap();
    let wrong_plan = ConnectionPlan::prepare(&wrong_store, &wrong, "ubuntu", FakeForward::transport(port).0,
        Authentication::KeyFile { private_key: &live.key }).unwrap();
    let rejected = command(wrong_plan.invocation(Client::Ssh).unwrap(), &BATCH, "echo zzabba-ok").output().unwrap();
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    println!("A 틀린 호스트 키: 종료 {:?} · {}", rejected.status.code(), stderr.trim());
    assert_eq!(rejected.status.code(), Some(255));
    assert!(!String::from_utf8_lossy(&rejected.stdout).contains("zzabba-ok"));
    assert!(stderr.contains("Host key verification failed"), "{stderr}");
    drop(wrong_plan);

    drop(plan);
    let status = wait_terminated(&live.target, &id);
    println!("A 세션 {id} 최종 상태: {status}");
    assert_eq!(status, "Terminated");
    assert_no_active(&live.target);
}

#[test]
#[ignore = "실제 AWS SSM 노드 필요"]
fn live_port_forward_preserves_bytes() {
    let live = live();
    let mut last = None;
    // 준비 실패와 이 시험 PC의 일시적인 Winsock 루프백 실패만 새 포트로 다시 시도한다.
    for attempt in 1..=ATTEMPTS {
        let listen = SocketAddr::from((Ipv4Addr::LOCALHOST, super::free_port().port()));
        let route = Route::Ssm { listen, node: live.target.instance_id.clone(), port: ECHO_PORT, host: None };
        let plan = ForwardPlan::new(&route, &live.target).unwrap();
        println!("B 문서: {} · 매개변수: {}", plan.document(), plan.parameters());
        let mut child = tauri::async_runtime::block_on(prepare(plan, &CancellationToken::new())).unwrap().spawn().unwrap();
        if let Err(error) = wait_ready(&mut child) {
            println!("B 시도 {attempt} 준비 실패: {error}");
            drop(child);
            last = Some(error);
            continue;
        }
        let id = session_id(&child);
        let check = Command::new("python")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ssm_forward_bytes.py"))
            .args(["check", "--port", &listen.port().to_string()]).output().unwrap();
        println!("B 시도 {attempt} · 세션 {id} · 종료 {:?}\n{}{}", check.status.code(),
            String::from_utf8_lossy(&check.stdout), String::from_utf8_lossy(&check.stderr));
        child.close().unwrap();
        drop(child);
        let status = wait_terminated(&live.target, &id);
        println!("B 세션 {id} 최종 상태: {status}");
        assert_eq!(status, "Terminated");
        super::assert_closed(listen);
        match check.status.code() {
            Some(0) => return assert_no_active(&live.target),
            Some(2) => last = Some(format!("연결 실패: {}", check.status)),
            _ => panic!("바이트 불일치: {:?}", check.status),
        }
    }
    panic!("시도 반복 실패: {last:?}");
}
