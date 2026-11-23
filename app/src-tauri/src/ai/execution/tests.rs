use super::*;
use crate::terminal::LiveSsmIdentity;
use std::time::Duration;

fn generation(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

fn identity() -> LiveSsmIdentity {
    LiveSsmIdentity {
        id: "7".into(),
        document: 3,
        account_id: "123456789012".into(),
        role: Some(crate::ssm::AssumedRole::fixture("123456789012")),
        profile: "ops".into(),
        region: "ap-northeast-2".into(),
        instance_id: "i-0123456789abcdef0".into(),
    }
}

fn target() -> SelectedTarget {
    SelectedTarget {
        profile_id: "dev".into(),
        ssm_profile: Some("ops".into()),
        region: "ap-northeast-2".into(),
        node_id: "i-0123456789abcdef0".into(),
        display_name: "개발 노드".into(),
        generation: generation(1),
    }
}

fn live() -> LiveSession {
    LiveSession { id: "7".into(), selection_id: generation(2), identity: identity() }
}

fn policy(revision: u64) -> Policy {
    Policy { revision: generation(revision), blocked: false }
}

fn approval(policy: Policy) -> NewApproval {
    NewApproval {
        proposal_id: 11,
        target: target(),
        policy,
        session: live(),
        document: 3,
        task: task("uptime").unwrap(),
    }
}

fn start(id: NonZeroU64) -> Start {
    Start { approval_id: id, task_id: "uptime".into(), session_id: "7".into(), generation: generation(2) }
}

fn issued(state: &State, policy: Policy, now: Instant) -> NonZeroU64 {
    state.issue(approval(policy), now).unwrap().approval_id.parse().unwrap()
}

trait Refusal {
    fn error(self) -> ExecError;
}

impl<T> Refusal for Result<T, ExecError> {
    fn error(self) -> ExecError {
        match self {
            Ok(_) => panic!("거부되어야 합니다"),
            Err(error) => error,
        }
    }
}

#[test]
fn task_table_exposes_only_the_fixed_absolute_uptime() {
    assert_eq!(task("uptime").unwrap().program, "/usr/bin/uptime");
    assert!(task("uptime").unwrap().args.is_empty());
    assert_eq!(task("uptime").unwrap().account, "zzabba-ai-control");
    assert!(task("rm").is_none());
    assert!(task("uptime ").is_none());
}

#[test]
fn approval_is_consumed_once_and_duplicate_submission_is_refused() {
    let now = Instant::now();
    let state = State::default();
    let policy = policy(1);
    let id = issued(&state, policy, now);
    assert!(state.start(&start(id), &live(), policy, 3, now).is_ok());
    assert_eq!(state.start(&start(id), &live(), policy, 3, now).error(), ExecError::Approval);
    assert!(state.snapshot(id).is_none());
}

#[test]
fn other_session_submission_is_refused_without_consuming_the_original() {
    let now = Instant::now();
    let state = State::default();
    let policy = policy(1);
    let id = issued(&state, policy, now);
    let other = LiveSession { selection_id: generation(9), ..live() };
    assert_eq!(state.start(&start(id), &other, policy, 3, now).error(), ExecError::Session);
    assert!(state.snapshot(id).is_some());
    assert!(state.start(&start(id), &live(), policy, 3, now).is_ok());
}

#[test]
fn foreign_document_submission_is_refused() {
    let now = Instant::now();
    let state = State::default();
    let policy = policy(1);
    let id = issued(&state, policy, now);
    assert_eq!(state.start(&start(id), &live(), policy, 4, now).error(), ExecError::Session);
    assert!(state.start(&start(id), &live(), policy, 3, now).is_ok());
}

#[test]
fn expired_approval_is_refused_and_discarded() {
    let now = Instant::now();
    let state = State::default();
    let policy = policy(1);
    let id = issued(&state, policy, now);
    assert_eq!(state.start(&start(id), &live(), policy, 3, now + APPROVAL_TTL).error(), ExecError::Expired);
    assert!(state.snapshot(id).is_none());
}

#[test]
fn changed_policy_is_refused_without_consuming_the_approval() {
    let now = Instant::now();
    let state = State::default();
    let id = issued(&state, policy(1), now);
    assert_eq!(state.start(&start(id), &live(), policy(2), 3, now).error(), ExecError::Policy);
    assert!(state.snapshot(id).is_some());
    assert!(state.start(&start(id), &live(), policy(1), 3, now).is_ok());
}

#[test]
fn disconnected_or_reconnected_session_cannot_reuse_the_approval() {
    let now = Instant::now();
    let state = State::default();
    let policy = policy(1);
    let id = issued(&state, policy, now);
    state.cancel_all();
    assert!(state.snapshot(id).is_none());
    assert_eq!(state.start(&start(id), &live(), policy, 3, now).error(), ExecError::Approval);
    let next = issued(&state, policy, now);
    let reconnected = LiveSession { selection_id: generation(3), ..live() };
    assert_eq!(state.start(&start(next), &reconnected, policy, 3, now).error(), ExecError::Session);
}

#[test]
fn a_new_approval_invalidates_the_previous_one() {
    let now = Instant::now();
    let state = State::default();
    let policy = policy(1);
    let first = issued(&state, policy, now);
    let second = issued(&state, policy, now);
    assert!(first < second);
    assert_eq!(state.start(&start(first), &live(), policy, 3, now).error(), ExecError::Approval);
    assert!(state.start(&start(second), &live(), policy, 3, now).is_ok());
}

#[test]
fn stop_is_requested_only_for_the_running_job() {
    let now = Instant::now();
    let state = State::default();
    let policy = policy(1);
    let id = issued(&state, policy, now);
    let (_, run, commands) = state.start(&start(id), &live(), policy, 3, now).unwrap();
    assert_eq!(state.stop(id, 3).error(), ExecError::NotRunning);
    run.set_phase(Phase::Running);
    let view = state.stop(id, 3).unwrap();
    assert_eq!(view.phase, Phase::StopRequested);
    assert!(matches!(commands.try_recv(), Ok(Command::Stop)));
    assert_eq!(state.stop(id, 3).error(), ExecError::NotRunning);
}

#[test]
fn stopped_job_exposes_exit_and_unconfirmed_as_distinct_results() {
    let now = Instant::now();
    let state = State::default();
    let policy = policy(1);
    let id = issued(&state, policy, now);
    let (_, run, _) = state.start(&start(id), &live(), policy, 3, now).unwrap();
    run.set_phase(Phase::Running);
    run.progress.lock().unwrap().job_id = Some(id);
    run.finish(Phase::Exited, Some(0), None);
    let exited = state.status(3).unwrap();
    run.finish(Phase::Unconfirmed, None, Some("제어 채널이 끊겼습니다".into()));
    let unconfirmed = state.status(3).unwrap();
    assert_eq!(exited.approval_id, unconfirmed.approval_id);
    assert_eq!(exited.job_id, unconfirmed.job_id);
    assert_eq!(exited.phase, Phase::Exited);
    assert_eq!(unconfirmed.phase, Phase::Unconfirmed);
    assert!(state.status(4).is_none());
}

#[test]
fn pre_start_failures_are_not_exit_evidence() {
    for phase in [Phase::Starting, Phase::Pairing, Phase::WaitingClaim, Phase::Exited, Phase::Failed] {
        assert_eq!(failure(phase), Phase::Failed, "{phase:?}");
    }
    for phase in [Phase::AwaitingStart, Phase::Running, Phase::StopRequested] {
        assert_eq!(failure(phase), Phase::Unconfirmed, "{phase:?}");
    }
}

#[test]
fn session_match_requires_the_exact_ssm_scope_and_role() {
    let target = target();
    let identity = identity();
    assert!(matches_session(&target, &identity));
    assert!(!matches_session(&target, &LiveSsmIdentity { region: "us-east-1".into(), ..identity.clone() }));
    assert!(!matches_session(&target, &LiveSsmIdentity { instance_id: "i-fffffffffffffffff".into(), ..identity.clone() }));
    assert!(!matches_session(&target, &LiveSsmIdentity { role: None, ..identity.clone() }));
}

#[test]
fn only_strict_positive_ids_are_accepted() {
    assert_eq!(parse_id("7"), Some(generation(7)));
    for invalid in ["", "0", "07", "-1", "x", "1.0"] {
        assert!(parse_id(invalid).is_none(), "{invalid}");
    }
}

// 실기기 시험은 승인된 비운영 노드와 배포된 ZzabbaAiSharedControl 문서에서만 확인한다.
// `ZZABBA_AI_LIVE_PROFILE`·`ZZABBA_AI_LIVE_REGION`·`ZZABBA_AI_LIVE_NODE`를 채우고
// `cargo test -- --ignored live_control_document`로 실행한다.
#[test]
#[ignore = "실제 AWS SSM 노드·플러그인·배포된 제어 문서가 필요합니다"]
fn live_control_document_reaches_the_node_executor() {
    use std::io::Read;
    use std::sync::mpsc;

    let (Ok(profile), Ok(region), Ok(node)) = (
        std::env::var("ZZABBA_AI_LIVE_PROFILE"),
        std::env::var("ZZABBA_AI_LIVE_REGION"),
        std::env::var("ZZABBA_AI_LIVE_NODE"),
    ) else {
        return;
    };
    assert!(node.starts_with("i-"), "노드 ID가 필요합니다");
    let mut command = portable_pty::CommandBuilder::new(
        std::env::var("ZZABBA_AI_LIVE_AWS").unwrap_or_else(|_| "aws".into()),
    );
    command.args([
        "ssm", "start-session", "--profile", &profile, "--region", &region,
        "--target", &node, "--document-name", "ZzabbaAiSharedControl",
    ]);
    let pty = portable_pty::native_pty_system().openpty(portable_pty::PtySize::default()).unwrap();
    let mut child = pty.slave.spawn_command(command).unwrap();
    drop(pty.slave);
    let mut reader = pty.master.try_clone_reader().unwrap();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = [0; 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(size) => { if sender.send(buffer[..size].to_vec()).is_err() { return; } }
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = Vec::new();
    let found = loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else { break false };
        match receiver.recv_timeout(remaining.min(Duration::from_millis(250))) {
            Ok(bytes) => {
                seen.extend_from_slice(&bytes);
                if seen.windows(14).any(|window| window == b"\"type\":\"ready\"") { break true; }
                if seen.len() > 64 * 1024 { break false; }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break false,
        }
    };
    let _ = child.kill();
    let text = String::from_utf8_lossy(&seen);
    assert!(found, "제어 문서가 ready 프레임에 도달하지 않았습니다: {text}");
}
