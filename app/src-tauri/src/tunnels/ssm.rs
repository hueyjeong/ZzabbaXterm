use std::{
    io::{self, BufRead, BufReader, Read},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    process::{Child, Command},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use tokio_util::sync::CancellationToken;

use crate::{
    connections::profile::Ssm,
    ssm::{AwsCli, CliError, ForwardGuard, ForwardPermit, Scope, SessionDocument, SessionLookup, StartRequest, valid_session_id},
    terminal::remote::start_error,
};

use super::{Observation, Route, Transport, transport::{Event, Failure, Listener, SsmPlan, SsmScope, observe_ssm}};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(unix)]
use unix::{Tree, configure};
#[cfg(windows)]
use windows::{Tree, configure};
#[cfg(test)]
pub(super) mod tests;

// 공식 CLI의 StartSession 호출, 플러그인 웹소켓 연결, DescribeSessions 확인까지의 한도다.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(20);
const VERIFY_INTERVAL: Duration = Duration::from_millis(500);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(1);
// 플러그인은 bind 뒤에 준비 문장을 출력하므로 소켓 표 반영 지연만 기다린다.
const LISTEN_TIMEOUT: Duration = Duration::from_secs(3);
// 트리를 정리한 뒤 출력 읽기 작업이 끝나기를 기다리는 한도다. 터널 문서 잠금 안에서 호출된다.
const READER_TIMEOUT: Duration = Duration::from_secs(1);
const LINE_LIMIT: usize = 512;
// 빈 포트를 고른 뒤 플러그인이 bind하기 전에 다른 프로그램이 가져간 경우의 재시도 한도다.
const BIND_ATTEMPTS: usize = 3;
const STDERR_LIMIT: usize = 8 * 1024;

const FORMAT: &str = "SSM 플러그인 출력 형식을 확인할 수 없습니다";
const SESSION_MISMATCH: &str = "SSM 플러그인 세션 ID가 일치하지 않습니다";
const PLUGIN_FAILED: &str = "SSM 플러그인 시작 실패";
const OWNERSHIP: &str = "SSM 세션 소유권 확인 불가";
const LISTENER_OWNERSHIP: &str = "SSM 수신 소유권 불일치";

/// 저장 SSM 대상으로 전달 경로를 검증하고 공식 문서 인자를 고정한다. 사용자는 이 계획의 문서·매개변수를 보고 승인한다.
pub(crate) struct ForwardPlan {
    listen: SocketAddr,
    node: String,
    scope: Scope,
    document: &'static str,
    parameters: String,
}

impl ForwardPlan {
    /// 수신 주소·노드 형식·원격 호스트를 검사하고, 경로의 노드가 저장 대상의 노드와 같을 때만 계획을 만든다.
    /// 프로필·리전은 저장 대상에서만 가져오므로 따로 대조할 승인 값이 없다.
    pub(crate) fn new(route: &Route, target: &Ssm) -> Result<Self, String> {
        let scope = SsmScope { profile: &target.profile, region: &target.region };
        let plan = SsmPlan::new(route, scope, route, scope).map_err(|error| format!("SSM 전달 승인: {error:?}"))?;
        let Route::Ssm { node, .. } = route else { unreachable!("SSM 전달 계획 필요") };
        let [_, _, _, parameters] = plan.document_options(scope, &target.instance_id)
            .map_err(|error| format!("SSM 전달 승인: {error:?}"))?;
        let parameters = parameters.to_owned();
        let scope = Scope::new(target.profile.clone(), target.region.clone()).map_err(start_error)?;
        Ok(Self { listen: plan.listen(), node: node.clone(), scope, document: plan.document(), parameters })
    }

    pub(crate) fn document(&self) -> &'static str { self.document }
    pub(crate) fn parameters(&self) -> &str { &self.parameters }
}

/// C02와 같은 자격 경계로 시작 직전까지 준비한 실행 인가다.
pub(crate) struct Prepared {
    listen: SocketAddr,
    remote: Arc<RemoteSession>,
    permit: ForwardPermit,
}

/// 프로필의 SSO 자격을 내보내고 노드의 온라인 상태와 플러그인을 확인한 뒤 실행 인가를 발급한다.
/// 실행 인자에는 `--profile`을 넣지 않는다. 내보낸 자격과 격리한 CLI 홈이 범위를 고정한다.
pub(crate) async fn prepare(plan: ForwardPlan, cancel: &CancellationToken) -> Result<Prepared, String> {
    let ForwardPlan { listen, node, scope, document, parameters } = plan;
    let cli = AwsCli::probe(cancel).await.map_err(start_error)?;
    let online = cli.online_nodes(&scope, cancel).await.map_err(start_error)?
        .into_iter().find(|online| online.instance_id() == node)
        .ok_or_else(|| start_error(CliError::TargetNotConnected))?;
    cli.check_plugin(cancel).await.map_err(start_error)?;
    let mut request = cli.prepare_document_start(&scope, &online,
        SessionDocument::PortForward { document, parameters: &parameters }, cancel).map_err(start_error)?;
    let permit = cli.authorize_forward(&mut request, &scope, cancel).await.map_err(start_error)?
        .ok_or_else(|| start_error(CliError::Cancelled))?;
    Ok(Prepared { listen, remote: Arc::new(RemoteSession { cli, scope, request: Mutex::new(request) }), permit })
}

impl Prepared {
    /// 호출자는 생성 결과를 같은 잠금에서 등록한다.
    pub(crate) fn spawn(self) -> Result<SsmChild, String> {
        let Self { listen, remote, permit } = self;
        let verifier = remote.clone();
        SsmChild::start(
            listen,
            |configure| permit.spawn(configure).map(|(child, guard)| (child, Some(guard))),
            Box::new(move |expected: &str, cancel: &CancellationToken| verifier.verify(expected, cancel)),
            Some(Box::new(move || remote.terminate())),
        )
    }
}

struct RemoteSession {
    cli: AwsCli,
    scope: Scope,
    request: Mutex<StartRequest>,
}

impl RemoteSession {
    // 요청별 Reason·Owner·Target이 일치하는 단일 세션의 ID만 소유권 근거로 받는다.
    fn verify(&self, expected: &str, cancel: &CancellationToken) -> Result<String, String> {
        let deadline = Instant::now() + VERIFY_TIMEOUT;
        while Instant::now() < deadline && !cancel.is_cancelled() {
            let request = self.request.lock().unwrap();
            let found = tauri::async_runtime::block_on(
                self.cli.verify_session(&request, &self.scope, SessionLookup::Active, cancel));
            drop(request);
            match found {
                Ok(session) if session.session_id() != expected => return Err(OWNERSHIP.into()),
                Ok(session) if session.is_connected() => return Ok(session.session_id().into()),
                Ok(session) if session.is_terminated() => return Err("SSM 세션이 종료되었습니다".into()),
                _ => thread::sleep(VERIFY_INTERVAL),
            }
        }
        Err("SSM 세션 연결 확인 불가".into())
    }

    // 로컬 자식을 정리한 뒤 같은 요청의 세션만 terminate-session과 History 조회로 닫는다.
    fn terminate(&self) {
        let cancel = CancellationToken::new();
        let mut request = self.request.lock().unwrap();
        let closed = tauri::async_runtime::block_on(self.cli.close_session(&mut request, &self.scope, &cancel));
        if closed.is_err() {
            eprintln!("SSM 포트 전달 세션 종료 확인 불가: {}", request.target());
        }
    }
}

/// SSH-over-SSM의 노드 SSH 포트 전달을 열고 수신 소유권까지 확인한다. 루프백 빈 포트를 쓰고 bind 충돌이면 새 포트로 다시 시도한다.
pub(crate) fn open_ssh_forward(target: &Ssm, port: u16, cancel: &CancellationToken) -> Result<(u16, SsmChild), String> {
    let pick = || std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).and_then(|listener| listener.local_addr());
    ready_on_free_port(cancel, pick, |listen| {
        let route = Route::Ssm { listen, node: target.instance_id.clone(), port, host: None };
        let plan = ForwardPlan::new(&route, target)?;
        tauri::async_runtime::block_on(prepare(plan, cancel))?.spawn()
    })
}

fn ready_on_free_port(
    cancel: &CancellationToken,
    mut pick: impl FnMut() -> io::Result<SocketAddr>,
    mut start: impl FnMut(SocketAddr) -> Result<SsmChild, String>,
) -> Result<(u16, SsmChild), String> {
    let mut conflict = None;
    for _ in 0..BIND_ATTEMPTS {
        let listen = pick().map_err(|error| format!("SSM 수신 포트 선택: {error}"))?;
        let ready = start(listen).and_then(|mut child| loop {
            if cancel.is_cancelled() { return Err("SSM 포트 전달이 취소되었습니다".into()); }
            match child.observe() {
                Observation::Listening => return Ok(child),
                Observation::Pending => thread::sleep(Duration::from_millis(50)),
                Observation::Ended(reason) => return Err(reason),
            }
        });
        match ready {
            Ok(child) => return Ok((listen.port(), child)),
            Err(error) if error == OutputFault::BindConflict.message(listen) => conflict = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(conflict.unwrap_or_default())
}

type Verify = Box<dyn FnOnce(&str, &CancellationToken) -> Result<String, String> + Send>;
type Terminate = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Progress {
    bound: Option<(u16, String)>,
    verified: Option<String>,
    failure: Option<String>,
    stdout_closed: bool,
    stderr_closed: bool,
    stderr: Vec<u8>,
}

/// aws CLI 자식과 그 자식인 session-manager-plugin을 한 트리로 소유한다.
pub(crate) struct SsmChild {
    child: Child,
    tree: Tree,
    listen: SocketAddr,
    progress: Arc<Mutex<Progress>>,
    cancel: CancellationToken,
    deadline: Instant,
    listen_deadline: Option<Instant>,
    ready: bool,
    terminate: Option<Terminate>,
    readers: Vec<JoinHandle<()>>,
    _guard: Option<ForwardGuard>,
}

impl SsmChild {
    fn start(
        listen: SocketAddr,
        spawn: impl FnOnce(fn(&mut Command)) -> io::Result<(Child, Option<ForwardGuard>)>,
        verify: Verify,
        terminate: Option<Terminate>,
    ) -> Result<Self, String> {
        if super::ssh::local_conflict(Listener::Local(listen)) {
            return Err(ended(observe_ssm(listen, Event::Ended(Failure::BindConflict), None)));
        }
        let mut tree = Tree::new().map_err(|error| format!("SSM 프로세스 트리: {error}"))?;
        let (mut child, guard) = spawn(configure).map_err(|error| format!("SSM 포트 전달 시작: {error}"))?;
        if let Err(error) = tree.adopt(&child) {
            // 플러그인을 띄우기 전인 CLI 자식만 있으므로 그 핸들만 정리한다.
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("SSM 프로세스 트리: {error}"));
        }
        let progress = Arc::new(Mutex::new(Progress::default()));
        let cancel = CancellationToken::new();
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let mut readers = Vec::new();
        let spawned = (|| {
            readers.push(spawn_named("ssm-forward-stdout", {
                let progress = progress.clone();
                let cancel = cancel.clone();
                move || read_stdout(stdout, listen, &progress, verify, &cancel)
            })?);
            readers.push(spawn_named("ssm-forward-stderr", {
                let progress = progress.clone();
                move || read_stderr(stderr, &progress)
            })?);
            io::Result::Ok(())
        })();
        let mut forward = Self {
            child, tree, listen, progress, cancel, deadline: Instant::now() + READY_TIMEOUT, listen_deadline: None, ready: false,
            terminate, readers, _guard: guard,
        };
        if let Err(error) = spawned {
            let cleanup = forward.close().err().map(|error| format!(" · 종료 확인 불가: {error}")).unwrap_or_default();
            return Err(format!("SSM 출력 감시: {error}{cleanup}"));
        }
        Ok(forward)
    }

    fn poll(&mut self) -> Result<bool, String> {
        if let Some(status) = self.tree.exited(&mut self.child).map_err(|error| format!("SSM 자식 상태: {error}"))? {
            if self.ready {
                return Err(ended(observe_ssm(self.listen, Event::Ended(Failure::OwnedProcessExited), None)));
            }
            self.settle();
            return Err(self.start_failure(&status));
        }
        let progress = self.progress.lock().unwrap();
        if let Some(failure) = &progress.failure {
            return Err(failure.clone());
        }
        if self.ready {
            return Ok(true);
        }
        if let (Some((port, parsed)), Some(verified)) = (&progress.bound, &progress.verified) {
            let address = SocketAddr::from((Ipv4Addr::LOCALHOST, *port));
            let bound = Event::SsmPluginBound { address, session_id: parsed };
            if !matches!(observe_ssm(self.listen, bound, Some(verified)), Observation::Listening) {
                return Err(if address == self.listen { OWNERSHIP.into() } else { format!("SSM 플러그인 수신 주소 불일치: {address}") });
            }
            drop(progress);
            // 출력만으로는 다른 프로그램이 연 같은 포트와 구분할 수 없다. 이 트리가 승인 주소의 LISTEN 소켓을 소유할 때만 준비다.
            let owned = self.tree.listeners(&self.child, self.listen.port())
                .map_err(|error| format!("{LISTENER_OWNERSHIP}: {error}"))?;
            if let Some(owned) = owned.filter(|owned| !owned.contains(&self.listen)) {
                if let Some(other) = owned.first() {
                    return Err(format!("{LISTENER_OWNERSHIP}: {other}"));
                }
                let deadline = *self.listen_deadline.get_or_insert_with(|| (Instant::now() + LISTEN_TIMEOUT).min(self.deadline));
                if Instant::now() >= deadline {
                    return Err(format!("{LISTENER_OWNERSHIP}: {}", self.listen));
                }
                return Ok(false);
            }
            self.ready = true;
            return Ok(true);
        }
        if Instant::now() >= self.deadline {
            return Err("SSM 포트 전달 확인 시간 초과".into());
        }
        Ok(false)
    }

    // 종료 직후 남은 출력을 읽을 시간을 준다. 플러그인이 파이프를 쥐고 남아 있으면 한도에서 멈춘다.
    fn settle(&self) {
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        while Instant::now() < deadline {
            let progress = self.progress.lock().unwrap();
            if progress.stdout_closed && progress.stderr_closed { return; }
            drop(progress);
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn start_failure(&self, status: &str) -> String {
        let progress = self.progress.lock().unwrap();
        if let Some(failure) = &progress.failure {
            return failure.clone();
        }
        if let Some(reason) = start_rejection(&progress.stderr) {
            return reason;
        }
        if bind_conflict(&String::from_utf8_lossy(&progress.stderr)) || super::ssh::local_conflict(Listener::Local(self.listen)) {
            return ended(observe_ssm(self.listen, Event::Ended(Failure::BindConflict), None));
        }
        format!("SSM 포트 전달 시작 실패: {status}")
    }

    /// 승인된 로컬 수신 주소로 연결하고, 받아들인 쪽 소켓이 이 트리의 프로세스 것인지 확인한다.
    pub(super) fn open_stream(&mut self, address: SocketAddr) -> Result<TcpStream, String> {
        if !self.ready || address != self.listen {
            return Err("승인된 SSM 수신 주소가 일치하지 않습니다".into());
        }
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(1))
            .map_err(|error| format!("SSM 터널 스트림 연결: {error}"))?;
        let client = stream.local_addr().map_err(|error| error.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let alive = |forward: &mut Self| forward.tree.exited(&mut forward.child).map(|status| status.is_none());
            if !alive(self).map_err(|error| error.to_string())? { break; }
            if self.tree.owns_stream(&self.child, address, client).map_err(|error| format!("SSM 스트림 소유권: {error}"))?
                && alive(self).map_err(|error| error.to_string())?
            {
                return Ok(stream);
            }
            if Instant::now() >= deadline { break; }
            thread::sleep(Duration::from_millis(5));
        }
        Err("SSM 플러그인이 승인된 터널 스트림을 소유하지 않습니다".into())
    }
}

impl Transport for SsmChild {
    fn observe(&mut self) -> Observation {
        match self.poll() {
            Ok(true) => Observation::Listening,
            Ok(false) => Observation::Pending,
            Err(error) => Observation::Ended(error),
        }
    }

    fn close(&mut self) -> io::Result<()> {
        self.cancel.cancel();
        self.tree.stop(&mut self.child)?;
        // 트리를 정리했으므로 파이프 끝이 닫혀 읽기 작업이 끝난다. 트리 밖으로 넘어간 파이프가 있으면 한도 뒤 분리한다.
        let deadline = Instant::now() + READER_TIMEOUT;
        while self.readers.iter().any(|reader| !reader.is_finished()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        for reader in self.readers.drain(..) {
            if reader.is_finished() {
                let _ = reader.join();
            } else {
                eprintln!("SSM 포트 전달 출력 읽기 종료 확인 불가");
            }
        }
        // StartSession 거부로 끝난 경우에만 원격 세션이 없다. 준비 출력 전에 닫혀도 같은 요청의 세션을 찾아 닫는다.
        let rejected = start_rejection(&self.progress.lock().unwrap().stderr).is_some();
        if let Some(terminate) = self.terminate.take().filter(|_| !rejected) {
            // 원격 종료 조회는 수 초가 걸리므로 등록부 잠금 밖에서 같은 요청의 세션만 닫는다.
            if let Err(error) = thread::Builder::new().name("ssm-forward-close".into()).spawn(terminate) {
                eprintln!("SSM 포트 전달 세션 종료 작업 시작 불가: {error}");
            }
        }
        Ok(())
    }
}

impl Drop for SsmChild {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn spawn_named(name: &str, work: impl FnOnce() + Send + 'static) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name(name.into()).spawn(work)
}

fn read_stdout(stdout: impl Read, listen: SocketAddr, progress: &Arc<Mutex<Progress>>, verify: Verify, cancel: &CancellationToken) {
    let mut reader = BufReader::new(stdout);
    let mut output = PluginOutput::new();
    let mut verify = Some(verify);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = match (&mut reader).take(LINE_LIMIT as u64 + 1).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        if output.is_ready() {
            continue;
        }
        let parsed = if read > LINE_LIMIT && !line.ends_with(b"\n") {
            Err(OutputFault::Format)
        } else {
            std::str::from_utf8(&line).map_err(|_| OutputFault::Format).and_then(|text| output.line(text))
        };
        let mut state = progress.lock().unwrap();
        match parsed {
            Ok(None) => {}
            Ok(Some((port, session))) => {
                state.bound = Some((port, session.clone()));
                drop(state);
                let shared = progress.clone();
                let cancel = cancel.clone();
                let verify = verify.take().expect("한 번만 준비된다");
                let checked = spawn_named("ssm-forward-verify", move || {
                    let result = verify(&session, &cancel);
                    let mut state = shared.lock().unwrap();
                    match result {
                        Ok(verified) => state.verified = Some(verified),
                        Err(error) => { state.failure.get_or_insert(error); }
                    }
                });
                if let Err(error) = checked {
                    progress.lock().unwrap().failure.get_or_insert(format!("SSM 세션 확인 작업: {error}"));
                }
            }
            Err(fault) => {
                state.failure.get_or_insert(fault.message(listen));
                // 실패를 기록한 뒤에도 자식이 쓰기에서 막히지 않도록 끝까지 비운다.
                drop(state);
                let _ = io::copy(&mut reader, &mut io::sink());
                break;
            }
        }
    }
    progress.lock().unwrap().stdout_closed = true;
}

fn read_stderr(stderr: impl Read, progress: &Arc<Mutex<Progress>>) {
    let mut kept = Vec::new();
    let mut stderr = stderr;
    let _ = (&mut stderr).take(STDERR_LIMIT as u64).read_to_end(&mut kept);
    progress.lock().unwrap().stderr = kept;
    let _ = io::copy(&mut stderr, &mut io::sink());
    progress.lock().unwrap().stderr_closed = true;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OutputFault {
    Format,
    BindConflict,
    SessionMismatch,
    PluginFailed,
}

impl OutputFault {
    fn message(self, listen: SocketAddr) -> String {
        match self {
            Self::Format => FORMAT.into(),
            Self::BindConflict => ended(observe_ssm(listen, Event::Ended(Failure::BindConflict), None)),
            Self::SessionMismatch => SESSION_MISMATCH.into(),
            Self::PluginFailed => PLUGIN_FAILED.into(),
        }
    }
}

/// session-manager-plugin 포트 세션의 준비 출력 판독기다.
/// `Starting session with SessionId: <id>` → `Port <n> opened for sessionId <id>.` → `Waiting for connections...`
/// 순서만 준비로 인정하고, 준비 전의 다른 문장은 실패로 처리한다. 준비 뒤의 연결별 출력은 읽고 버린다.
#[derive(Debug, Default)]
pub(super) struct PluginOutput {
    session: Option<String>,
    port: Option<u16>,
    ready: bool,
}

impl PluginOutput {
    pub(super) fn new() -> Self { Self::default() }

    pub(super) fn is_ready(&self) -> bool { self.ready }

    /// 준비가 확인된 줄에서 `(수신 포트, 세션 ID)`를 돌려준다.
    pub(super) fn line(&mut self, raw: &str) -> Result<Option<(u16, String)>, OutputFault> {
        let line = raw.trim_matches(|c: char| c == '\r' || c == '\n' || c == ' ');
        if self.ready || line.is_empty() {
            return Ok(None);
        }
        if bind_conflict(line) {
            return Err(OutputFault::BindConflict);
        }
        if let Some(id) = line.strip_prefix("Starting session with SessionId: ") {
            if self.session.is_some() || !valid_session_id(id) {
                return Err(OutputFault::Format);
            }
            self.session = Some(id.into());
            return Ok(None);
        }
        if let Some(rest) = line.strip_prefix("Port ") {
            let (port, id) = rest.split_once(" opened for sessionId ").ok_or(OutputFault::Format)?;
            let id = id.strip_suffix('.').ok_or(OutputFault::Format)?;
            let port = (!port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit()))
                .then(|| port.parse::<u16>().ok()).flatten().filter(|port| *port != 0)
                .ok_or(OutputFault::Format)?;
            let Some(session) = self.session.as_deref() else { return Err(OutputFault::Format) };
            if session != id {
                return Err(OutputFault::SessionMismatch);
            }
            if self.port.replace(port).is_some() {
                return Err(OutputFault::Format);
            }
            return Ok(None);
        }
        if line == "Waiting for connections..." {
            let (Some(session), Some(port)) = (&self.session, self.port) else { return Err(OutputFault::Format) };
            self.ready = true;
            return Ok(Some((port, session.clone())));
        }
        if line.starts_with("Cannot perform start session") {
            return Err(OutputFault::PluginFailed);
        }
        Err(OutputFault::Format)
    }
}

fn bind_conflict(text: &str) -> bool {
    ["address already in use", "Only one usage of each socket address", "bind: An attempt was made to access a socket"]
        .iter().any(|needle| text.contains(needle))
}

/// `AWS_CLI_ERROR_FORMAT=legacy`로 고정한 StartSession 거부를 문서 실행 거부·권한·자격·노드 상태로 나눈다.
pub(super) fn start_rejection(stderr: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(stderr).ok()?;
    if text.lines().any(|line| {
        line.contains("Token has expired and refresh failed")
            || line.contains("The SSO session associated with this profile has expired")
            || line.starts_with("Error loading SSO Token")
    }) {
        return Some(start_error(CliError::SsoLoginRequired));
    }
    let line = text.lines().map(str::trim).find_map(|line| {
        line.strip_prefix("aws: [ERROR]: ").unwrap_or(line).strip_prefix("An error occurred (")
    })?;
    let (code, message) = line.split_once(") when calling the StartSession operation:")?;
    if code.is_empty() || !code.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let document = message.contains(":document/AWS-StartPortForwardingSession");
    Some(match code {
        "AccessDenied" | "AccessDeniedException" | "NotAuthorized" | "UnauthorizedOperation" if document =>
            "SSM: 포트 전달 문서 실행 권한 거부".into(),
        "AccessDenied" | "AccessDeniedException" | "NotAuthorized" | "UnauthorizedOperation" => start_error(CliError::AccessDenied),
        "InvalidDocument" | "InvalidDocumentOperation" | "InvalidParameters" | "InvalidDocumentVersion" =>
            "SSM: 포트 전달 문서 실행 거부".into(),
        "ExpiredToken" | "ExpiredTokenException" => start_error(CliError::CredentialsExpired),
        "InvalidClientTokenId" | "UnrecognizedClientException" => "SSM: 자격 증명 사용 불가".into(),
        "TargetNotConnected" => start_error(CliError::TargetNotConnected),
        _ => format!("SSM: 시작 거부 ({code})"),
    })
}

/// 가짜 CLI·플러그인 트리로 실제 판독·정리 경로를 실행한다. 세션 확인은 `verified`를 돌려준다.
#[cfg(test)]
pub(super) fn fixture_child(mode: &str, listen: SocketAddr, printed: Option<u16>, verified: Result<&str, &str>,
                            terminated: Option<Arc<std::sync::atomic::AtomicBool>>) -> Result<SsmChild, String> {
    let program = tests::fixture_program();
    let (port, printed) = (listen.port().to_string(), printed.unwrap_or(listen.port()).to_string());
    let permit = ForwardPermit::fixture(program, &[mode, &port, tests::SESSION, &printed]);
    let verified = verified.map(str::to_owned).map_err(str::to_owned);
    SsmChild::start(
        listen,
        |configure| permit.spawn_fixture(configure).map(|(child, guard)| (child, Some(guard))),
        Box::new(move |expected: &str, _: &CancellationToken| {
            assert_eq!(expected, tests::SESSION);
            verified
        }),
        terminated.map(|flag| Box::new(move || flag.store(true, std::sync::atomic::Ordering::SeqCst)) as Terminate),
    )
}

fn ended(observation: Observation) -> String {
    let Observation::Ended(reason) = observation else { unreachable!("종료 사유 필요") };
    reason
}
