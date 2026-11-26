//! 승인된 고정 작업 하나의 수명. 실행 승인은 한 번만 소비하고 전송 결과는 증거로만 판정한다.

use std::{
    fmt::Write as _,
    num::NonZeroU64,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::{ssm::CancellationToken, terminal::{LiveSsmIdentity, Sessions}};

use super::{
    SelectedTarget,
    approval::Policy,
    control::{ControlTransport, Output},
    control_wire::{APPROVAL_WINDOW, WireOperation},
    shared::{KeyEvidence, KeyTracker, MainSession, ReservedKey},
    shared_protocol::{Event as ProtocolEvent, SharedProtocol},
    shared_wire::Ticket,
};

const OWNER: &str = "main";
const APPROVAL_TTL: Duration = Duration::from_secs(120);
const CLAIM_WAIT: Duration = APPROVAL_WINDOW;
const START_WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(200);
const OUTPUT_LIMIT: usize = 8 * 1024;

/// 실행기가 받아들일 수 있는 유일한 작업 표다. 셸 문자열과 임의 경로는 여기에 들어오지 않는다.
#[derive(Debug)]
pub(crate) struct Task {
    pub(crate) id: &'static str,
    pub(crate) account: &'static str,
    pub(crate) program: &'static str,
    pub(crate) args: &'static [&'static str],
    pub(crate) operation: WireOperation,
}

pub(crate) const UPTIME: Task = Task {
    id: "uptime",
    account: "zxt-ai-control",
    program: "/usr/bin/uptime",
    args: &[],
    operation: WireOperation::Uptime,
};

pub(crate) fn task(id: &str) -> Option<&'static Task> {
    (id == UPTIME.id).then_some(&UPTIME)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LiveSession {
    pub(crate) id: String,
    pub(crate) selection_id: NonZeroU64,
    pub(crate) identity: LiveSsmIdentity,
}

#[derive(Clone)]
pub(crate) struct NewApproval {
    pub(crate) proposal_id: u32,
    pub(crate) target: SelectedTarget,
    pub(crate) policy: Policy,
    pub(crate) session: LiveSession,
    pub(crate) document: u64,
    pub(crate) task: &'static Task,
}

#[derive(Clone)]
pub(crate) struct Approval {
    approval_id: NonZeroU64,
    request: NewApproval,
    expires_at: Instant,
}

pub(crate) struct Snapshot {
    pub(crate) proposal_id: u32,
    pub(crate) target: SelectedTarget,
    pub(crate) policy: Policy,
    pub(crate) session: LiveSession,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Phase {
    Starting,
    Pairing,
    WaitingClaim,
    AwaitingStart,
    Running,
    StopRequested,
    Exited,
    Unconfirmed,
    Failed,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ExecError {
    #[error("실행 승인을 다시 받아야 합니다")]
    Approval,
    #[error("실행 승인이 만료되었습니다")]
    Expired,
    #[error("다른 세션에는 이 실행 승인을 사용할 수 없습니다")]
    Session,
    #[error("정책이 바뀌어 실행 승인을 사용할 수 없습니다")]
    Policy,
    #[error("이미 실행 중인 작업이 있습니다")]
    Busy,
    #[error("중지할 실행 작업이 없습니다")]
    NotRunning,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Card {
    approval_id: String,
    proposal_id: String,
    task_id: &'static str,
    session_id: String,
    selection_id: String,
    node: String,
    account: &'static str,
    program: &'static str,
    args: &'static [&'static str],
    policy: &'static str,
    expires_in_ms: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct View {
    approval_id: String,
    proposal_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_id: Option<String>,
    task_id: &'static str,
    session_id: String,
    selection_id: String,
    node: String,
    account: &'static str,
    program: &'static str,
    args: &'static [&'static str],
    policy: &'static str,
    phase: Phase,
    output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pairing_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub(crate) struct Start {
    pub(crate) approval_id: NonZeroU64,
    pub(crate) task_id: String,
    pub(crate) session_id: String,
    pub(crate) generation: NonZeroU64,
}

#[derive(Debug)]
pub(crate) enum Command {
    Stop,
}

struct Progress {
    phase: Phase,
    active: bool,
    job_id: Option<NonZeroU64>,
    output: String,
    pairing_code: Option<String>,
    code: Option<i32>,
    error: Option<String>,
}

impl Default for Progress {
    fn default() -> Self {
        Self { phase: Phase::Starting, active: true, job_id: None, output: String::new(),
            pairing_code: None, code: None, error: None }
    }
}

pub(crate) struct Run {
    approval_id: NonZeroU64,
    proposal_id: u32,
    document: u64,
    task: &'static Task,
    main: MainSession,
    session_id: String,
    selection_id: NonZeroU64,
    node: String,
    progress: Mutex<Progress>,
    commands: Mutex<mpsc::Sender<Command>>,
    keys: Arc<Mutex<KeyTracker>>,
    cancel: CancellationToken,
}

impl Run {
    fn phase(&self) -> Phase {
        self.progress.lock().unwrap().phase
    }

    fn active(&self) -> bool {
        self.progress.lock().unwrap().active
    }

    fn set_phase(&self, phase: Phase) {
        let mut progress = self.progress.lock().unwrap();
        if !terminal(progress.phase) {
            progress.phase = phase;
        }
    }

    fn pairing(&self, code: String) {
        let mut progress = self.progress.lock().unwrap();
        if terminal(progress.phase) { return; }
        progress.phase = Phase::Pairing;
        progress.pairing_code = Some(code);
    }

    fn paired(&self) {
        let mut progress = self.progress.lock().unwrap();
        if terminal(progress.phase) { return; }
        progress.pairing_code = None;
        progress.phase = Phase::WaitingClaim;
    }

    fn output(&self, bytes: &[u8]) {
        let mut progress = self.progress.lock().unwrap();
        if terminal(progress.phase) { return; }
        if progress.output.len() < OUTPUT_LIMIT {
            if progress.output.len() + bytes.len() <= OUTPUT_LIMIT {
                progress.output.push_str(&String::from_utf8_lossy(bytes));
            } else {
                progress.output.push('…');
            }
        }
    }

    fn finish(&self, phase: Phase, code: Option<i32>, error: Option<String>) {
        let mut progress = self.progress.lock().unwrap();
        if terminal(progress.phase) { return; }
        progress.phase = phase;
        progress.active = false;
        progress.pairing_code = None;
        progress.code = code;
        progress.error = error;
    }

    /// 중지 판정과 상태 전이를 같은 잠금에서 처리한다. 확정 종료가 먼저면 요청을 거부한다.
    fn request_stop(&self) -> Result<(), ExecError> {
        let mut progress = self.progress.lock().unwrap();
        if !progress.active || progress.phase != Phase::Running {
            return Err(ExecError::NotRunning);
        }
        progress.phase = Phase::StopRequested;
        if self.commands.lock().unwrap().send(Command::Stop).is_err() {
            progress.phase = Phase::Unconfirmed;
            progress.active = false;
            progress.error = Some("중지 요청을 전달하지 못했습니다".into());
        }
        Ok(())
    }

    fn take_key(&self, key: ReservedKey, now: Instant) -> bool {
        self.keys.lock().unwrap().take(self.approval_id.get(), self.main, key, now)
    }

    fn view(&self) -> View {
        let progress = self.progress.lock().unwrap();
        View {
            approval_id: self.approval_id.to_string(),
            proposal_id: self.proposal_id.to_string(),
            job_id: progress.job_id.map(|id| id.to_string()),
            task_id: self.task.id,
            session_id: self.session_id.clone(),
            selection_id: self.selection_id.to_string(),
            node: self.node.clone(),
            account: self.task.account,
            program: self.task.program,
            args: self.task.args,
            policy: policy_label(),
            phase: progress.phase,
            output: progress.output.clone(),
            pairing_code: progress.pairing_code.clone(),
            code: progress.code,
            error: progress.error.clone(),
        }
    }
}

fn policy_label() -> &'static str {
    "allowed"
}

#[derive(Default)]
struct Inner {
    next: u64,
    approval: Option<Approval>,
    run: Option<Arc<Run>>,
}

#[derive(Clone, Default)]
pub(crate) struct State {
    inner: Arc<Mutex<Inner>>,
    keys: Arc<Mutex<KeyTracker>>,
}

impl State {
    /// 같은 작업의 새 승인은 이전 대기 승인을 폐기한다. 실행 중이면 승인을 발급하지 않는다.
    pub(crate) fn issue(&self, request: NewApproval, now: Instant) -> Result<Card, ExecError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.run.as_ref().is_some_and(|run| run.active()) {
            return Err(ExecError::Busy);
        }
        let approval_id = inner.next.checked_add(1).and_then(NonZeroU64::new).ok_or(ExecError::Approval)?;
        inner.next = approval_id.get();
        let approval = Approval { approval_id, request, expires_at: now + APPROVAL_TTL };
        let card = Card {
            approval_id: approval.approval_id.to_string(),
            proposal_id: approval.request.proposal_id.to_string(),
            task_id: approval.request.task.id,
            session_id: approval.request.session.id.clone(),
            selection_id: approval.request.session.selection_id.to_string(),
            node: approval.request.session.identity.instance_id.clone(),
            account: approval.request.task.account,
            program: approval.request.task.program,
            args: approval.request.task.args,
            policy: policy_label(),
            expires_in_ms: APPROVAL_TTL.as_millis() as u64,
        };
        inner.approval = Some(approval);
        Ok(card)
    }

    pub(crate) fn snapshot(&self, approval_id: NonZeroU64) -> Option<Snapshot> {
        let inner = self.inner.lock().unwrap();
        let approval = inner.approval.as_ref().filter(|approval| approval.approval_id == approval_id)?;
        Some(Snapshot {
            proposal_id: approval.request.proposal_id,
            target: approval.request.target.clone(),
            policy: approval.request.policy,
            session: approval.request.session.clone(),
        })
    }

    /// 봉인된 승인을 한 번만 소비한다. 거부 사유에는 전송이 없다.
    pub(crate) fn start(
        &self,
        request: &Start,
        live: &LiveSession,
        policy: Policy,
        document: u64,
        now: Instant,
    ) -> Result<(Approval, Arc<Run>, mpsc::Receiver<Command>), ExecError> {
        let mut inner = self.inner.lock().unwrap();
        let Some(approval) = inner.approval.take() else { return Err(ExecError::Approval) };
        let refuse = |inner: &mut Inner, approval: Approval, error: ExecError| {
            inner.approval = Some(approval);
            error
        };
        if now >= approval.expires_at {
            return Err(ExecError::Expired);
        }
        if approval.approval_id != request.approval_id
            || approval.request.task.id != request.task_id
            || approval.request.session.id != request.session_id
            || approval.request.session.selection_id != request.generation
        {
            return Err(refuse(&mut inner, approval, ExecError::Approval));
        }
        if approval.request.document != document || approval.request.session != *live {
            return Err(refuse(&mut inner, approval, ExecError::Session));
        }
        if approval.request.policy != policy {
            return Err(refuse(&mut inner, approval, ExecError::Policy));
        }
        if inner.run.as_ref().is_some_and(|run| run.active()) {
            return Err(refuse(&mut inner, approval, ExecError::Busy));
        }
        let main_id = parse_id(&approval.request.session.id).ok_or(ExecError::Session)?;
        let (sender, receiver) = mpsc::channel();
        let run = Arc::new(Run {
            approval_id: approval.approval_id,
            proposal_id: approval.request.proposal_id,
            document,
            task: approval.request.task,
            main: MainSession {
                id: main_id,
                generation: approval.request.session.selection_id,
                document,
            },
            session_id: approval.request.session.id.clone(),
            selection_id: approval.request.session.selection_id,
            node: approval.request.session.identity.instance_id.clone(),
            progress: Mutex::new(Progress::default()),
            commands: Mutex::new(sender),
            keys: Arc::clone(&self.keys),
            cancel: CancellationToken::new(),
        });
        inner.run = Some(Arc::clone(&run));
        Ok((approval, run, receiver))
    }

    pub(crate) fn stop(&self, approval_id: NonZeroU64, document: u64) -> Result<View, ExecError> {
        let inner = self.inner.lock().unwrap();
        let run = inner.run.as_ref()
            .filter(|run| run.approval_id == approval_id && run.document == document)
            .ok_or(ExecError::NotRunning)?;
        run.request_stop()?;
        Ok(run.view())
    }

    pub(crate) fn status(&self, document: u64) -> Option<View> {
        let inner = self.inner.lock().unwrap();
        inner.run.as_ref().filter(|run| run.document == document).map(|run| run.view())
    }

    /// 문서가 바뀌면 대기 승인과 실행 중 채널을 정리한다. 노드는 채널 종료로 소유 자식을 회수한다.
    pub(crate) fn cancel_all(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.approval = None;
        if let Some(run) = &inner.run {
            run.cancel.cancel();
        }
        self.keys.lock().unwrap().clear_all();
    }

    /// Ctrl-X 접두 뒤의 p/u만 예약 키로 인정한다. 일반 입력은 접두만 지운다.
    pub(crate) fn observe_prefix(&self, session_id: &str, bytes: &[u8], now: Instant) -> Option<ReservedKey> {
        let id = parse_id(session_id)?;
        let mut keys = self.keys.lock().unwrap();
        if bytes == [0x18] {
            keys.prefix(id, now);
            return None;
        }
        let key = match bytes {
            [b'p'] => ReservedKey::Pair,
            [b'u'] => ReservedKey::Claim,
            _ => {
                keys.clear(id);
                return None;
            }
        };
        keys.complete(id, now).then_some(key)
    }

    /// 입력이 들어온 탭과 그때 확인된 선택 세대가 실행 중인 작업과 같을 때만 증거를 남긴다.
    pub(crate) fn record_key(
        &self,
        document: u64,
        session_id: &str,
        key: ReservedKey,
        selection: Option<(NonZeroU64, &LiveSsmIdentity)>,
        now: Instant,
    ) {
        let evidence = selection
            .filter(|(_, identity)| identity.id == session_id)
            .and_then(|(generation, _)| parse_id(session_id).map(|id| MainSession { id, generation, document }))
            .and_then(|main| {
                let inner = self.inner.lock().unwrap();
                let run = inner.run.as_ref().filter(|run| run.active() && run.main == main)?;
                Some(KeyEvidence { approval_id: run.approval_id.get(), main, key })
            });
        let mut keys = self.keys.lock().unwrap();
        match evidence {
            Some(evidence) => keys.record(evidence, now),
            // 다른 탭·바뀐 선택의 예약 키는 앞선 증거까지 폐기한다.
            None => keys.invalidate(key),
        }
    }
}

pub(crate) fn matches_session(target: &SelectedTarget, identity: &LiveSsmIdentity) -> bool {
    target.ssm_profile.as_deref() == Some(identity.profile.as_str())
        && target.region == identity.region
        && target.node_id == identity.instance_id
        && identity.role.is_some()
}

pub(crate) fn launch(app: AppHandle, run: Arc<Run>, approval: Approval, commands: mpsc::Receiver<Command>) {
    let failed = Arc::clone(&run);
    let spawned = std::thread::Builder::new()
        .name("ai-execution".into())
        .spawn(move || worker(app, run, approval, commands));
    if spawned.is_err() {
        failed.finish(Phase::Failed, None, Some("실행 작업을 시작하지 못했습니다".into()));
    }
}

fn worker(app: AppHandle, run: Arc<Run>, approval: Approval, commands: mpsc::Receiver<Command>) {
    let sessions = app.state::<Sessions>();
    let mut transport = match ControlTransport::open(&sessions, OWNER, &approval.request.session.identity, &run.cancel) {
        Ok(transport) => transport,
        Err(error) => {
            run.finish(failure(Phase::Starting), None, Some(error));
            return;
        }
    };
    let outcome = {
        let mut driver = Driver::new(&mut transport, &run, &approval, &commands);
        driver.drive()
    };
    let _ = transport.close();
    if let Err((phase, error)) = outcome {
        run.finish(phase, None, Some(error));
    }
}

trait Channel {
    fn poll(&mut self, timeout: Duration) -> Result<Option<Output>, String>;
    fn write(&mut self, bytes: &[u8]) -> Result<(), String>;
}

impl Channel for ControlTransport {
    fn poll(&mut self, timeout: Duration) -> Result<Option<Output>, String> {
        ControlTransport::poll(self, timeout)
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        ControlTransport::write(self, bytes)
    }
}

struct Driver<'a> {
    transport: &'a mut dyn Channel,
    run: &'a Arc<Run>,
    approval: &'a Approval,
    commands: &'a mpsc::Receiver<Command>,
    protocol: SharedProtocol,
    ticket: Option<Ticket>,
    job: Option<NonZeroU64>,
    pair_deadline: Option<Instant>,
    claim_deadline: Option<Instant>,
    start_deadline: Option<Instant>,
    stop_sent: bool,
    exit: Option<Option<i32>>,
}

impl<'a> Driver<'a> {
    fn new(
        transport: &'a mut dyn Channel,
        run: &'a Arc<Run>,
        approval: &'a Approval,
        commands: &'a mpsc::Receiver<Command>,
    ) -> Self {
        Self {
            transport, run, approval, commands,
            protocol: SharedProtocol::default(),
            ticket: None, job: None,
            pair_deadline: None, claim_deadline: None, start_deadline: None,
            stop_sent: false, exit: None,
        }
    }

    fn drive(&mut self) -> Result<(), (Phase, String)> {
        loop {
            if !self.stop_sent && matches!(self.commands.try_recv(), Ok(Command::Stop)) {
                if let Some(job_id) = self.job {
                    if let Ok(bytes) = self.protocol.stop(job_id) {
                        self.transport.write(&bytes).map_err(|error| self.failure(error))?;
                        self.run.set_phase(Phase::StopRequested);
                    }
                }
                self.stop_sent = true;
            }
            match self.transport.poll(POLL) {
                Ok(Some(Output::Bytes(bytes))) => self.consume(&bytes)?,
                Ok(Some(Output::SsmConnected)) => {
                    if let Some(event) = self.protocol.ssm_connected().map_err(|error| self.failure(error.to_string()))? {
                        self.event(event)?;
                    }
                }
                Ok(Some(Output::CliExited(code))) => {
                    let error = self.protocol.cli_exited(code);
                    return Err(self.failure(error.to_string()));
                }
                Ok(None) => {}
                Err(error) => return Err(self.failure(error)),
            }
            if self.exit.is_some() {
                return Ok(());
            }
            let now = Instant::now();
            if self.ticket.is_none() && self.pair_deadline.is_some_and(|deadline| now >= deadline) {
                return Err(self.failure("중앙 셸에서 연결 코드를 확인하지 못했습니다".into()));
            }
            if self.claim_deadline.is_some_and(|deadline| now >= deadline) {
                return Err(self.failure("중앙 셸에서 예약 키 입력을 확인하지 못했습니다".into()));
            }
            if self.start_deadline.is_some_and(|deadline| now >= deadline) {
                return Err(self.failure("작업 시작을 확인하지 못했습니다".into()));
            }
        }
    }

    fn consume(&mut self, bytes: &[u8]) -> Result<(), (Phase, String)> {
        let mut input = bytes;
        loop {
            match self.protocol.feed(&mut input) {
                Ok(Some(event)) => {
                    self.event(event)?;
                    if self.exit.is_some() {
                        return Ok(());
                    }
                }
                Ok(None) => return Ok(()),
                Err(error) => return Err(self.failure(error.to_string())),
            }
        }
    }

    fn event(&mut self, event: ProtocolEvent) -> Result<(), (Phase, String)> {
        match event {
            ProtocolEvent::Ready => {
                let channel = channel_nonce().map_err(|error| self.failure(error))?;
                let bytes = self.protocol.hello(&channel).map_err(|error| self.failure(error.to_string()))?;
                self.transport.write(&bytes).map_err(|error| self.failure(error))?;
                self.pair_deadline = Some(Instant::now() + Duration::from_secs(10));
            }
            ProtocolEvent::HelloAck => {
                self.run.set_phase(Phase::Pairing);
                self.pair_deadline = Some(Instant::now() + Duration::from_secs(60));
            }
            ProtocolEvent::Pairing { code, expires_in } => {
                self.run.pairing(code.as_str().to_owned());
                self.pair_deadline = Some(Instant::now() + expires_in);
            }
            ProtocolEvent::Paired { shell_generation, .. } => {
                self.run.paired();
                self.pair_deadline = None;
                if !self.run.take_key(ReservedKey::Pair, Instant::now()) {
                    return Err(self.failure("선택한 세션에서 페어링 키 입력을 확인하지 못했습니다".into()));
                }
                let ticket = Ticket {
                    main_id: parse_id(&self.approval.request.session.id).ok_or_else(|| self.failure("세션 ID가 올바르지 않습니다".into()))?,
                    main_generation: self.approval.request.session.selection_id,
                    document: self.approval.request.document,
                    control_id: NonZeroU64::MIN,
                    control_generation: NonZeroU64::MIN,
                    shell_generation,
                    approval_id: self.approval.approval_id,
                    input_generation: 0,
                    policy_revision: self.approval.request.policy.revision,
                };
                let bytes = self.protocol.reserve(ticket, self.approval.request.task.operation)
                    .map_err(|error| self.failure(error.to_string()))?;
                self.transport.write(&bytes).map_err(|error| self.failure(error))?;
                self.ticket = Some(ticket);
                // 노드 예약과 같은 기한에 한 번의 폴링 여유만 두어 명시적 거부 프레임을 읽는다.
                self.claim_deadline = Some(Instant::now() + CLAIM_WAIT + POLL);
            }
            ProtocolEvent::Claim { ticket } => {
                if self.ticket != Some(ticket) {
                    return Err(self.failure("인증 채널의 작업 ID가 승인과 다릅니다".into()));
                }
                if !self.run.take_key(ReservedKey::Claim, Instant::now()) {
                    return Err(self.failure("선택한 세션에서 예약 키 입력을 확인하지 못했습니다".into()));
                }
                let bytes = self.protocol.grant(ticket, self.approval.request.task.operation)
                    .map_err(|error| self.failure(error.to_string()))?;
                // grant 쓰기가 실패해도 노드가 먼저 받았을 수 있으므로 인계 이후로 기록한다.
                self.claim_deadline = None;
                self.start_deadline = Some(Instant::now() + START_WAIT);
                self.run.set_phase(Phase::AwaitingStart);
                self.transport.write(&bytes).map_err(|error| self.failure(error))?;
            }
            ProtocolEvent::Denied => {
                return Err(self.failure("노드가 예약을 거부했습니다".into()));
            }
            ProtocolEvent::Started { job_id } => {
                self.job = Some(job_id);
                self.start_deadline = None;
                self.run.progress.lock().unwrap().job_id = Some(job_id);
                self.run.set_phase(Phase::Running);
            }
            ProtocolEvent::Output { bytes, .. } => self.run.output(&bytes),
            ProtocolEvent::Exited { code, .. } => {
                self.exit = Some(code);
                self.run.finish(Phase::Exited, code, None);
            }
        }
        Ok(())
    }

    fn failure(&self, error: String) -> (Phase, String) {
        (failure(self.run.phase()), error)
    }
}

fn terminal(phase: Phase) -> bool {
    matches!(phase, Phase::Exited | Phase::Unconfirmed | Phase::Failed)
}

/// 시작을 확인하기 전 단절은 실패이고, 인계 이후 단절은 종료 확인 불가다.
fn failure(phase: Phase) -> Phase {
    if matches!(phase, Phase::AwaitingStart | Phase::Running | Phase::StopRequested) {
        Phase::Unconfirmed
    } else {
        Phase::Failed
    }
}

fn parse_id(value: &str) -> Option<u64> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    value.parse().ok()
}

fn channel_nonce() -> Result<String, String> {
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|_| "제어 채널 난수를 만들지 못했습니다".to_string())?;
    let mut value = String::with_capacity(32);
    for byte in random {
        write!(value, "{byte:02x}").map_err(|_| "제어 채널 난수를 만들지 못했습니다".to_string())?;
    }
    Ok(value)
}

#[cfg(test)]
mod tests;
