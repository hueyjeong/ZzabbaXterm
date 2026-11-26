use super::*;
use crate::ai::control_wire::{ClientFrame, NodeFrame, VERSION, WireStream};
use crate::ai::shared::ReservedKey;
use crate::ai::shared_wire::{Client, Node, Ticket};
use crate::terminal::LiveSsmIdentity;
use std::sync::{Arc, mpsc};
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

fn live_with(id: &str, selection: u64) -> LiveSession {
    LiveSession {
        id: id.into(),
        selection_id: generation(selection),
        identity: LiveSsmIdentity { id: id.into(), ..identity() },
    }
}

fn live() -> LiveSession {
    live_with("7", 2)
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

fn start_for(id: NonZeroU64, session: &str, selection: u64) -> Start {
    Start { approval_id: id, task_id: "uptime".into(), session_id: session.into(), generation: generation(selection) }
}

fn start(id: NonZeroU64) -> Start {
    start_for(id, "7", 2)
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
    assert_eq!(task("uptime").unwrap().account, "zxt-ai-control");
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
    let first = issued(&state, policy, now);
    let (_, exited_run, _) = state.start(&start(first), &live(), policy, 3, now).unwrap();
    exited_run.set_phase(Phase::Running);
    exited_run.progress.lock().unwrap().job_id = Some(first);
    exited_run.finish(Phase::Exited, Some(0), None);
    let exited = state.status(3).unwrap();
    let second = issued(&state, policy, now);
    let (_, unconfirmed_run, _) = state.start(&start(second), &live(), policy, 3, now).unwrap();
    unconfirmed_run.set_phase(Phase::Running);
    unconfirmed_run.progress.lock().unwrap().job_id = Some(second);
    unconfirmed_run.finish(Phase::Unconfirmed, None, Some("제어 채널이 끊겼습니다".into()));
    let unconfirmed = state.status(3).unwrap();
    assert_ne!(exited.approval_id, unconfirmed.approval_id);
    assert_eq!(exited.code, Some(0));
    assert_eq!(unconfirmed.error.as_deref(), Some("제어 채널이 끊겼습니다"));
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
fn session_ids_allow_zero_but_reject_leading_zeros_and_text() {
    assert_eq!(parse_id("0"), Some(0));
    assert_eq!(parse_id("7"), Some(7));
    for invalid in ["", "00", "07", "-1", "x", "1.0"] {
        assert!(parse_id(invalid).is_none(), "{invalid}");
    }
}

#[test]
fn first_session_id_zero_can_be_approved_and_started() {
    let now = Instant::now();
    let state = State::default();
    let policy = policy(1);
    let session = live_with("0", 2);
    let id = state.issue(NewApproval { session: session.clone(), ..approval(policy) }, now).unwrap().approval_id.parse().unwrap();
    let (_, run, _) = state.start(&start_for(id, "0", 2), &session, policy, 3, now).unwrap();
    assert_eq!(run.main.id, 0);
    assert_eq!(run.main.generation, generation(2));
}

#[test]
fn stop_after_confirmed_exit_keeps_the_exit_state() {
    let now = Instant::now();
    let state = State::default();
    let policy = policy(1);
    let id = issued(&state, policy, now);
    let (_, run, _) = state.start(&start(id), &live(), policy, 3, now).unwrap();
    run.set_phase(Phase::Running);
    run.finish(Phase::Exited, Some(0), None);
    assert_eq!(state.stop(id, 3).error(), ExecError::NotRunning);
    assert_eq!(state.status(3).unwrap().phase, Phase::Exited);
}

#[test]
fn confirmed_exit_after_stop_request_wins() {
    let now = Instant::now();
    let state = State::default();
    let policy = policy(1);
    let id = issued(&state, policy, now);
    let (_, run, commands) = state.start(&start(id), &live(), policy, 3, now).unwrap();
    run.set_phase(Phase::Running);
    assert_eq!(state.stop(id, 3).unwrap().phase, Phase::StopRequested);
    assert!(matches!(commands.try_recv(), Ok(Command::Stop)));
    run.finish(Phase::Exited, Some(0), None);
    assert_eq!(state.status(3).unwrap().phase, Phase::Exited);
    assert!(!run.active());
}

#[test]
fn terminal_state_is_not_overwritten_by_a_late_finish() {
    let now = Instant::now();
    let state = State::default();
    let policy = policy(1);
    let id = issued(&state, policy, now);
    let (_, run, _) = state.start(&start(id), &live(), policy, 3, now).unwrap();
    run.finish(Phase::Unconfirmed, None, Some("단절".into()));
    run.finish(Phase::Exited, Some(0), None);
    let view = state.status(3).unwrap();
    assert_eq!(view.phase, Phase::Unconfirmed);
    assert!(view.error.is_some());
}

#[test]
fn reserved_key_is_bound_to_the_selected_session_and_one_run() {
    let now = Instant::now();
    let state = State::default();
    let policy = policy(1);
    let session = live_with("0", 2);
    let id = state.issue(NewApproval { session: session.clone(), ..approval(policy) }, now).unwrap().approval_id.parse().unwrap();
    let (_, run, _) = state.start(&start_for(id, "0", 2), &session, policy, 3, now).unwrap();
    assert_eq!(state.observe_prefix("0", &[0x18], now), None);
    assert_eq!(state.observe_prefix("0", &[b'u'], now), Some(ReservedKey::Claim));
    let other = LiveSsmIdentity { id: "5".into(), ..identity() };
    let selected = LiveSsmIdentity { id: "0".into(), ..identity() };
    state.record_key(3, "0", ReservedKey::Claim, Some((generation(2), &selected)), now);
    state.record_key(3, "0", ReservedKey::Claim, Some((generation(2), &other)), now);
    assert!(!run.take_key(ReservedKey::Claim, now));
    state.record_key(3, "0", ReservedKey::Claim, Some((generation(2), &selected)), now);
    assert!(run.take_key(ReservedKey::Claim, now));
    assert!(!run.take_key(ReservedKey::Claim, now));
}

#[test]
fn selection_change_or_old_approval_cannot_reuse_key_evidence() {
    let now = Instant::now();
    let state = State::default();
    let policy = policy(1);
    let session = live_with("0", 2);
    let first = state.issue(NewApproval { session: session.clone(), ..approval(policy) }, now).unwrap().approval_id.parse().unwrap();
    let (_, run, _) = state.start(&start_for(first, "0", 2), &session, policy, 3, now).unwrap();
    let selected = LiveSsmIdentity { id: "0".into(), ..identity() };
    assert_eq!(state.observe_prefix("0", &[0x18], now), None);
    assert_eq!(state.observe_prefix("0", &[b'u'], now), Some(ReservedKey::Claim));
    state.record_key(3, "0", ReservedKey::Claim, Some((generation(3), &selected)), now);
    assert!(!run.take_key(ReservedKey::Claim, now));
    state.record_key(3, "0", ReservedKey::Claim, Some((generation(2), &selected)), now);
    run.finish(Phase::Exited, Some(0), None);
    let second = state.issue(NewApproval { session: session.clone(), ..approval(policy) }, now).unwrap().approval_id.parse().unwrap();
    let (_, next, _) = state.start(&start_for(second, "0", 2), &session, policy, 3, now).unwrap();
    assert!(!next.take_key(ReservedKey::Claim, now));
}

#[test]
fn reserved_prefix_requires_ctrl_x_then_a_single_letter() {
    let state = State::default();
    let now = Instant::now();
    assert_eq!(state.observe_prefix("0", &[b'u'], now), None);
    assert_eq!(state.observe_prefix("0", &[0x18], now), None);
    assert_eq!(state.observe_prefix("0", &[b'p'], now), Some(ReservedKey::Pair));
    assert_eq!(state.observe_prefix("0", &[0x18], now), None);
    assert_eq!(state.observe_prefix("0", &[b'u'], now + Duration::from_secs(3)), None);
    assert_eq!(state.observe_prefix("0", b"u", now), None);
}

const NONCE: &str = "0123456789abcdef0123456789abcdef";
const PAIR_CODE: &str = "0123456789abcdef0123456789abcdefabcdef0123456789abcdef0123456789";

fn frame(value: impl serde::Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    bytes
}

#[derive(Default)]
struct FakeChannel {
    step: usize,
    channel: Option<String>,
    ticket: Option<Ticket>,
    writes: Vec<Vec<u8>>,
}

impl Channel for FakeChannel {
    fn poll(&mut self, _: Duration) -> Result<Option<Output>, String> {
        self.step += 1;
        let bytes = match self.step {
            1 => return Ok(Some(Output::SsmConnected)),
            2 => frame(NodeFrame::Ready { version: VERSION, nonce: NONCE }),
            3 => {
                let Some(channel) = self.channel.clone() else { return Ok(None) };
                let mut bytes = frame(NodeFrame::HelloAck { version: VERSION, channel: &channel });
                bytes.extend(frame(Node::Pairing {
                    channel: &channel, seq: generation(1), code: PAIR_CODE, expires_in_ms: 60_000,
                }));
                bytes.extend(frame(Node::Paired {
                    channel: &channel, seq: generation(2), uid: 1000, shell_generation: generation(5),
                }));
                bytes
            }
            4 => {
                let (Some(channel), Some(ticket)) = (self.channel.clone(), self.ticket) else { return Ok(None) };
                frame(Node::Claim { channel: &channel, seq: generation(3), ticket })
            }
            5 => {
                let (Some(channel), Some(ticket)) = (self.channel.clone(), self.ticket) else { return Ok(None) };
                frame(NodeFrame::Started { channel: &channel, job_id: ticket.approval_id, seq: generation(4) })
            }
            6 => {
                let (Some(channel), Some(ticket)) = (self.channel.clone(), self.ticket) else { return Ok(None) };
                frame(NodeFrame::Output {
                    channel: &channel, job_id: ticket.approval_id, seq: generation(5), stream: WireStream::Stdout, data: "aGk=",
                })
            }
            7 => {
                let (Some(channel), Some(ticket)) = (self.channel.clone(), self.ticket) else { return Ok(None) };
                frame(NodeFrame::Exited {
                    channel: &channel, job_id: ticket.approval_id, seq: generation(6), code: Some(0), signal: None,
                })
            }
            _ => return Ok(None),
        };
        Ok(Some(Output::Bytes(bytes)))
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        if let Ok(ClientFrame::Hello { channel, .. }) = serde_json::from_slice::<ClientFrame<'_>>(bytes) {
            self.channel = Some(channel.to_owned());
        }
        if let Ok(Client::Reserve { ticket, .. }) = serde_json::from_slice::<Client<'_>>(bytes) {
            self.ticket = Some(ticket);
        }
        self.writes.push(bytes.to_vec());
        Ok(())
    }
}

fn fixture() -> (State, NonZeroU64, Approval, Arc<Run>, mpsc::Receiver<Command>) {
    let now = Instant::now();
    let state = State::default();
    let policy = policy(1);
    let session = live_with("0", 2);
    let id = state.issue(NewApproval { session: session.clone(), ..approval(policy) }, now).unwrap().approval_id.parse().unwrap();
    let (approval, run, commands) = state.start(&start_for(id, "0", 2), &session, policy, 3, now).unwrap();
    (state, id, approval, run, commands)
}

#[test]
fn driver_frames_one_approved_uptime_and_keeps_the_channel_sequence() {
    let (state, id, approval, run, commands) = fixture();
    let now = Instant::now();
    let selected = LiveSsmIdentity { id: "0".into(), ..identity() };
    for (key, letter) in [(ReservedKey::Pair, b'p'), (ReservedKey::Claim, b'u')] {
        assert_eq!(state.observe_prefix("0", &[0x18], now), None);
        assert_eq!(state.observe_prefix("0", &[letter], now), Some(key));
        state.record_key(3, "0", key, Some((generation(2), &selected)), now);
    }
    let mut fake = FakeChannel::default();
    let outcome = Driver::new(&mut fake, &run, &approval, &commands).drive();
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_eq!(state.status(3).unwrap().phase, Phase::Exited);
    assert_eq!(state.status(3).unwrap().output, "hi");
    assert_eq!(fake.writes.len(), 3);
    assert!(matches!(serde_json::from_slice::<ClientFrame<'_>>(&fake.writes[0]), Ok(ClientFrame::Hello { .. })));
    assert!(matches!(serde_json::from_slice::<Client<'_>>(&fake.writes[1]), Ok(Client::Reserve { ticket, .. }) if ticket.main_id == 0));
    assert!(matches!(serde_json::from_slice::<Client<'_>>(&fake.writes[2]), Ok(Client::Grant { .. })));
    assert_eq!(id.get(), 1);
}

#[test]
fn driver_refuses_a_pairing_without_the_selected_session_key_and_writes_no_reserve() {
    let (state, _id, approval, run, commands) = fixture();
    let now = Instant::now();
    let other = LiveSsmIdentity { id: "5".into(), ..identity() };
    assert_eq!(state.observe_prefix("0", &[0x18], now), None);
    assert_eq!(state.observe_prefix("0", &[b'p'], now), Some(ReservedKey::Pair));
    state.record_key(3, "0", ReservedKey::Pair, Some((generation(2), &other)), now);
    let mut fake = FakeChannel::default();
    let error = Driver::new(&mut fake, &run, &approval, &commands).drive().unwrap_err();
    assert_eq!(error.0, Phase::Failed);
    assert!(fake.writes.iter().any(|bytes| serde_json::from_slice::<ClientFrame<'_>>(bytes).is_ok()));
    assert!(!fake.writes.iter().any(|bytes| serde_json::from_slice::<Client<'_>>(bytes).is_ok()));
}

// 실기기 시험은 승인된 비운영 노드와 배포된 ZxtAiSharedControl 문서에서만 확인한다.
// `ZXT_AI_LIVE_PROFILE`·`ZXT_AI_LIVE_REGION`·`ZXT_AI_LIVE_NODE`를 채우고
// `cargo test -- --ignored live_control_document`로 실행한다.
#[test]
#[ignore = "실제 AWS SSM 노드·플러그인·배포된 제어 문서가 필요합니다"]
fn live_control_document_reaches_the_node_executor() {
    use std::io::Read;
    use std::sync::mpsc;

    let (Ok(profile), Ok(region), Ok(node)) = (
        std::env::var("ZXT_AI_LIVE_PROFILE"),
        std::env::var("ZXT_AI_LIVE_REGION"),
        std::env::var("ZXT_AI_LIVE_NODE"),
    ) else {
        return;
    };
    assert!(node.starts_with("i-"), "노드 ID가 필요합니다");
    let mut command = portable_pty::CommandBuilder::new(
        std::env::var("ZXT_AI_LIVE_AWS").unwrap_or_else(|_| "aws".into()),
    );
    command.args([
        "ssm", "start-session", "--profile", &profile, "--region", &region,
        "--target", &node, "--document-name", "ZxtAiSharedControl",
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
