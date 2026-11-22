use crate::connections::probe::Runs;
use crate::ssm::CancellationToken;
use portable_pty::{CommandBuilder, PtySize};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Condvar, Mutex, Weak, atomic::{AtomicU64, Ordering}},
    time::{Duration, Instant},
};

pub(crate) mod ipc;
pub(crate) mod process;
pub(crate) mod probe;
pub(crate) mod remote;
mod saved;
#[cfg(all(test, any(windows, unix)))]
mod pty_flow_tests;
#[cfg(test)]
pub(crate) mod tests;

use process::{Launch, Session};
#[cfg(test)]
pub(crate) use process::LiveSsmFixture;
use remote::RemoteClose;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum LocalClose {
    #[default]
    NotStarted,
    CliReaped,
    Unconfirmed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct CloseResult {
    local: LocalClose,
    remote: RemoteClose,
    #[serde(skip_serializing)]
    local_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
}

impl CloseResult {
    pub(crate) fn warning(&self) -> Option<String> {
        if self.remote == RemoteClose::Unconfirmed {
            return self.target.as_ref().map(|target| format!("종료 확인 불가: {target}"));
        }
        self.local_error.clone().or_else(|| (self.local == LocalClose::Unconfirmed).then(|| "로컬 자식 종료 확인 불가".into()))
    }
}

enum Retired {
    Completed { owner: String, result: CloseResult },
    Unconfirmed(Arc<Session>),
}

impl Retired {
    fn owner(&self) -> &str {
        match self {
            Self::Completed { owner, .. } => owner,
            Self::Unconfirmed(session) => &session.owner,
        }
    }
}

#[derive(Default)]
pub(crate) struct Sessions {
    next: AtomicU64,
    registry: Arc<Mutex<Registry>>,
    controls_changed: Arc<Condvar>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LiveSsmIdentity {
    pub(crate) id: String,
    pub(crate) document: u64,
    pub(crate) account_id: String,
    pub(crate) role: Option<Arc<crate::ssm::AssumedRole>>,
    pub(crate) profile: String,
    pub(crate) region: String,
    pub(crate) instance_id: String,
}

pub(crate) const CONTROL_CANCELLED: &str = "제어 세션이 취소되었습니다";
pub(crate) const CENTRAL_UNAVAILABLE: &str = "선택한 중앙 SSM 세션을 확인할 수 없습니다";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ControlExitReport {
    pub(crate) pending: usize,
    pub(crate) local_unconfirmed: usize,
    pub(crate) remote_unconfirmed: usize,
}

impl ControlExitReport {
    pub(crate) fn warning(self) -> Option<String> {
        (self.pending != 0 || self.local_unconfirmed != 0 || self.remote_unconfirmed != 0).then(|| {
            format!("전용 제어 작업 종료 확인: 대기 {}건, 로컬 미확인 {}건, SSM 미확인 {}건", self.pending, self.local_unconfirmed, self.remote_unconfirmed)
        })
    }
}

struct ControlCleanup {
    local_unconfirmed: bool,
    remote_unconfirmed: bool,
}

impl ControlCleanup {
    fn count(&self, report: &mut ControlExitReport) {
        report.local_unconfirmed += usize::from(self.local_unconfirmed);
        report.remote_unconfirmed += usize::from(self.remote_unconfirmed);
    }
}

struct TrackedControl {
    session: Arc<Session>,
    retain_cleanup: bool,
}

struct ControlExit {
    owner: String,
    document: u64,
    request_id: String,
    phase: ControlExitPhase,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ControlExitPhase {
    Draining,
    CheckingSsh,
    Ready,
    DestroyQueued(Option<i32>),
    Destroyed(Option<i32>),
    Consumed,
}

impl ControlExit {
    fn matches(&self, owner: &str, document: u64, request_id: &str) -> bool {
        self.owner == owner && self.document == document && self.request_id == request_id
    }

    fn sealed(&self) -> bool {
        matches!(self.phase, ControlExitPhase::DestroyQueued(_) | ControlExitPhase::Destroyed(_) | ControlExitPhase::Consumed)
    }
}

pub(crate) struct ControlBinding {
    registry: Arc<Mutex<Registry>>,
    central: Weak<Session>,
    owner: String,
    identity: LiveSsmIdentity,
    request: CancellationToken,
    cancel: CancellationToken,
}

impl ControlBinding {
    pub(crate) fn identity(&self) -> &LiveSsmIdentity {
        &self.identity
    }

    pub(crate) fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    #[cfg(test)]
    pub(crate) fn registry_unlocked_fixture(&self) -> bool {
        self.registry.try_lock().is_ok()
    }

    pub(crate) fn forward_request_cancel(&self) {
        let request = self.request.clone();
        let cancel = self.cancel.clone();
        tauri::async_runtime::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {},
                _ = request.cancelled() => cancel.cancel(),
            };
        });
    }

    fn check_request(&self) -> Result<(), String> {
        if self.request.is_cancelled() { Err(CONTROL_CANCELLED.into()) } else { Ok(()) }
    }

    pub(crate) fn consume_control_close(&self, id: u64) {
        let mut registry = self.registry.lock().unwrap();
        if !registry.shutdown {
            registry.control_cleanup.remove(&id);
        }
    }

    // 중앙 세션 확인과 입력 예약만 처리한다. AWS 조회와 PTY 입출력은 잠금 밖에서 처리한다.
    pub(crate) fn admit<T>(&self, action: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        let registry = self.registry.lock().unwrap();
        self.check_request()?;
        if registry.control_exit.is_some() { return Err(CENTRAL_UNAVAILABLE.into()); }
        registry.with_live_ssm_session(&self.owner, &self.identity, |_, _| {
            self.check_request()?;
            action()
        }).ok_or(CENTRAL_UNAVAILABLE)?
    }

    pub(crate) fn submit_start<T>(&self, spawn: impl FnOnce() -> T) -> Result<T, String> {
        let central = self.central.upgrade().ok_or(CENTRAL_UNAVAILABLE)?;
        let _submit = central.control_submit.lock().map_err(|_| CENTRAL_UNAVAILABLE)?;
        self.admit(|| {
            if self.cancel.is_cancelled() { return Err(CONTROL_CANCELLED.into()); }
            Ok(())
        })?;
        // 제출 잠금만 유지한다. 종료는 새 제출을 닫은 뒤 레지스트리 밖에서 이 호출을 기다린다.
        Ok(spawn())
    }
}

#[derive(Default)]
struct Registry {
    sessions: HashMap<String, Arc<Session>>,
    controls: HashMap<u64, TrackedControl>,
    control_cleanup: HashMap<u64, ControlCleanup>,
    retained_control_cleanup: ControlExitReport,
    control_exit: Option<ControlExit>,
    close_results: HashMap<String, Retired>,
    destroyed: HashSet<String>,
    documents: HashMap<String, u64>,
    probes: Runs,
    closing: HashSet<String>,
    exiting: bool,
    shutdown: bool,
}

struct ControlJob {
    id: u64,
    registry: Arc<Mutex<Registry>>,
    changed: Arc<Condvar>,
}

impl Drop for ControlJob {
    fn drop(&mut self) {
        let mut registry = self.registry.lock().unwrap();
        // 레지스트리 잠금이 종료 결과 기록과 pending 감소를 같은 관찰로 묶는다.
        let control = registry.controls.remove(&self.id).unwrap();
        let result = control.session.close_result();
        let cleanup = ControlCleanup {
            local_unconfirmed: result.local == LocalClose::Unconfirmed,
            remote_unconfirmed: result.remote == RemoteClose::Unconfirmed,
        };
        if control.retain_cleanup {
            cleanup.count(&mut registry.retained_control_cleanup);
        } else if cleanup.local_unconfirmed || cleanup.remote_unconfirmed {
            registry.control_cleanup.insert(self.id, cleanup);
        }
        control.session.finish();
        self.changed.notify_all();
    }
}

impl Registry {
    fn closing_owner(&self, owner: &str) -> bool {
        self.closing.contains(owner) || self.control_exit.as_ref().is_some_and(|exit| {
            exit.owner == owner && exit.phase != ControlExitPhase::Draining
        })
    }

    fn control_exit_in(&self, owner: &str, document: u64, request_id: &str, phase: ControlExitPhase) -> bool {
        !self.shutdown && self.control_exit.as_ref().is_some_and(|exit| {
            exit.phase == phase && exit.matches(owner, document, request_id)
        })
    }

    fn retain_control_cleanup(&mut self) {
        // 요청자에게 아직 전달되지 않은 미확인 결과는 시도 수명과 분리해 보존한다.
        for (_, cleanup) in self.control_cleanup.drain() { cleanup.count(&mut self.retained_control_cleanup); }
        for control in self.controls.values_mut() { control.retain_cleanup = true; }
    }

    fn control_report(&self) -> ControlExitReport {
        let mut report = ControlExitReport { pending: self.controls.len(), ..self.retained_control_cleanup };
        for cleanup in self.control_cleanup.values() { cleanup.count(&mut report); }
        report
    }

    fn main_session(&self, owner: &str, document: u64, id: &str) -> Option<&Arc<Session>> {
        if self.shutdown || self.exiting || self.closing_owner(owner) || self.destroyed.contains(owner)
            || self.documents.get(owner).copied().unwrap_or(0) != document
        {
            return None;
        }
        let session = self.sessions.get(id)?;
        (session.owner == owner && session.document == Some(document)).then_some(session)
    }

    fn with_live_ssm_session<T>(&self, owner: &str, identity: &LiveSsmIdentity, check: impl FnOnce(&Arc<Session>, &CancellationToken) -> T) -> Option<T> {
        let session = self.main_session(owner, identity.document, &identity.id)?;
        session.with_ssm_identity(|live, cancel| {
            (live.account_id == identity.account_id && live.profile == identity.profile
                && live.region == identity.region && live.instance_id == identity.instance_id
                && live.role == identity.role).then(|| check(session, cancel))
        }).flatten()
    }
}

impl Sessions {
    #[cfg(test)]
    fn open(&self, owner: &str, size: PtySize, command: CommandBuilder, send: impl Fn(Event) -> Result<(), String> + Send + 'static) -> Result<String, String> {
        self.open_document(owner, self.document(owner), size, Launch::Local(command), send)
    }

    pub(crate) fn document(&self, owner: &str) -> u64 {
        self.registry.lock().unwrap().documents.get(owner).copied().unwrap_or(0)
    }

    pub(crate) fn live_ssm_identity(&self, owner: &str, document: u64, id: &str) -> Option<LiveSsmIdentity> {
        let registry = self.registry.lock().unwrap();
        registry.main_session(owner, document, id)?.with_ssm_identity(|identity, _| LiveSsmIdentity {
            id: id.into(),
            document,
            account_id: identity.account_id.clone(),
            role: identity.role.clone(),
            profile: identity.profile.clone(),
            region: identity.region.clone(),
            instance_id: identity.instance_id.clone(),
        })
    }

    // 콜백은 두 잠금 아래에서 실행되므로 터미널에 다시 접근하거나 대기하면 안 된다.
    pub(crate) fn with_live_ssm_session<T>(&self, owner: &str, identity: &LiveSsmIdentity, check: impl FnOnce() -> T) -> Option<T> {
        self.registry.lock().unwrap().with_live_ssm_session(owner, identity, |_, _| check())
    }

    pub(crate) fn bind_ssm_control(&self, owner: &str, identity: &LiveSsmIdentity, request: &CancellationToken) -> Result<Arc<ControlBinding>, String> {
        if request.is_cancelled() { return Err(CONTROL_CANCELLED.into()); }
        if identity.role.is_none() { return Err(CENTRAL_UNAVAILABLE.into()); }
        let registry = self.registry.lock().unwrap();
        if registry.control_exit.is_some() { return Err(CENTRAL_UNAVAILABLE.into()); }
        let (central, cancel) = registry
            .with_live_ssm_session(owner, identity, |session, cancel| (Arc::downgrade(session), cancel.child_token()))
            .ok_or(CENTRAL_UNAVAILABLE)?;
        Ok(Arc::new(ControlBinding {
            registry: Arc::clone(&self.registry), central, owner: owner.into(), identity: identity.clone(), request: request.clone(), cancel,
        }))
    }

    pub(crate) fn launch_control(
        &self,
        binding: &Arc<ControlBinding>,
        size: PtySize,
        target: remote::Target,
        send: impl Fn(Event) -> Result<(), String> + Send + 'static,
    ) -> Result<(u64, Arc<Session>, Result<(), String>), String> {
        let session = Arc::new(Session::dedicated(binding));
        let command = Launch::AiControl(target, Arc::clone(binding));
        // 추적은 작업자가 원격 종료 확인을 마치거나 작업자 생성에 실패할 때 끝난다.
        let job = self.track_control(binding, &session)?;
        let id = job.id;
        let opened = session.begin_launch(size, command, send, move |_| drop(job))?;
        let result = opened.recv().map_err(|error| stage("open handshake", error)).and_then(|result| result);
        Ok((id, session, result))
    }

    fn track_control(&self, binding: &ControlBinding, session: &Arc<Session>) -> Result<ControlJob, String> {
        if !Arc::ptr_eq(&self.registry, &binding.registry) { return Err(CENTRAL_UNAVAILABLE.into()); }
        let mut registry = self.registry.lock().unwrap();
        binding.check_request()?;
        if registry.control_exit.is_some() { return Err(CENTRAL_UNAVAILABLE.into()); }
        registry.with_live_ssm_session(&binding.owner, &binding.identity, |_, _| {
            binding.check_request()?;
            if binding.cancel.is_cancelled() { return Err(CONTROL_CANCELLED.into()); }
            Ok::<_, String>(())
        }).ok_or(CENTRAL_UNAVAILABLE)??;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        registry.controls.insert(id, TrackedControl { session: Arc::clone(session), retain_cleanup: false });
        Ok(ControlJob { id, registry: Arc::clone(&self.registry), changed: Arc::clone(&self.controls_changed) })
    }

    pub(crate) fn with_probes<T>(&self, owner: &str, document: u64, use_probes: impl FnOnce(&mut Runs) -> Result<T, String>) -> Result<T, String> {
        self.with_probe_access(owner, document, false, use_probes)
    }

    pub(crate) fn with_probe_access<T>(&self, owner: &str, document: u64, starting_ssh: bool,
        use_probes: impl FnOnce(&mut Runs) -> Result<T, String>) -> Result<T, String> {
        let mut registry = self.registry.lock().unwrap();
        if registry.shutdown || registry.destroyed.contains(owner)
            || starting_ssh && (registry.exiting || registry.closing_owner(owner))
        {
            return Err("창이 닫혔습니다".into());
        }
        if registry.documents.get(owner).copied().unwrap_or(0) != document {
            return Err("문서가 바뀌었습니다".into());
        }
        use_probes(&mut registry.probes)
    }

    pub(crate) fn release_close_gate(&self, owner: Option<&str>) {
        let mut registry = self.registry.lock().unwrap();
        match owner {
            Some(owner) => { registry.closing.remove(owner); }
            None => registry.exiting = false,
        }
    }

    pub(crate) fn probe_completion(&self, owner: String, document: u64, run_id: String) -> impl FnOnce() + Send + use<> {
        let registry = Arc::downgrade(&self.registry);
        move || {
            if let Some(registry) = registry.upgrade() {
                let mut registry = registry.lock().unwrap();
                let current = registry.documents.get(&owner).copied().unwrap_or(0) == document && !registry.shutdown;
                registry.probes.reap(&run_id, current);
            }
        }
    }

    pub(crate) fn next_id(&self) -> String {
        self.next.fetch_add(1, Ordering::Relaxed).to_string()
    }

    fn open_document(&self, owner: &str, document: u64, size: PtySize, command: Launch, send: impl Fn(Event) -> Result<(), String> + Send + 'static) -> Result<String, String> {
        validate_size(size)?;
        let id = self.next_id();
        let mut session = Session::new(owner, command.is_ssm());
        session.document = Some(document);
        let session = Arc::new(session);
        let ssh = matches!(command, Launch::Ssh(_));
        {
            let mut registry = self.registry.lock().unwrap();
            if registry.shutdown || registry.destroyed.contains(owner) {
                return Err("open: 창이 닫혔습니다".into());
            }
            if registry.documents.get(owner).copied().unwrap_or(0) != document {
                return Err("open: 문서가 바뀌었습니다".into());
            }
            registry.sessions.insert(id.clone(), session.clone());
        }
        let registry = Arc::clone(&self.registry);
        let worker_id = id.clone();
        let result = session.launch(size, command, send, move |session| {
            let mut registry = registry.lock().unwrap();
            let result = session.close_result();
            if (session.is_ssm() || ssh && result.local != LocalClose::NotStarted) && !registry.shutdown
                && registry.documents.get(&session.owner).copied().unwrap_or(0) == document
            {
                let retired = if result.warning().is_some() {
                    Retired::Unconfirmed(Arc::clone(session))
                } else {
                    Retired::Completed { owner: session.owner.clone(), result }
                };
                registry.close_results.insert(worker_id.clone(), retired);
            }
            registry.sessions.remove(&worker_id);
        });
        if let Err(error) = result {
            session.wait_finished();
            self.registry.lock().unwrap().sessions.remove(&id);
            return Err(error);
        }
        Ok(id)
    }
    #[cfg(test)]
    pub(crate) fn reopen_ssh_fixture(&self, config: &std::path::Path, id: &str, output: tauri::ipc::Channel<tauri::ipc::InvokeResponseBody>) -> Result<String, String> {
        self.open_document("main", self.document("main"), size(24, 100), saved::launch(config, id)?,
            move |event| event.send(&output).map_err(|error| error.to_string()))
    }

    #[cfg(test)]
    pub(crate) fn close_ssh_fixture(&self, id: &str) {
        assert!(self.close("other", id).is_err());
        let result = self.close("main", id).unwrap();
        assert_eq!(result.local, LocalClose::CliReaped);
        assert_eq!(result.warning(), None);
        assert!(self.input("main", id, b"closed").is_err());
    }


    fn get(&self, owner: &str, id: &str) -> Result<Arc<Session>, String> {
        let registry = self.registry.lock().unwrap();
        let session = registry.sessions.get(id).cloned().or_else(|| {
            let probe = registry.probes.attached_probe(owner, id)?;
            probe.attached().then(|| probe.session())
        }).ok_or(CLOSED)?;
        if session.owner != owner {
            return Err("다른 창의 세션입니다".into());
        }
        Ok(session)
    }

    #[allow(dead_code, reason = "SFTP IPC 연결 전 내부 경계")]
    pub(crate) async fn connect_sftp(&self, owner: &str, id: &str) -> Result<crate::editor::sftp::Session, crate::editor::sftp::Error> {
        use crate::editor::sftp::{Error, Session};
        let (plan, cancel, worker) = {
            let registry = self.registry.lock().unwrap();
            // 연결 시험 세션은 등록된 작업 탭이 아니다.
            let session = registry.sessions.get(id).ok_or_else(|| Error::Terminal(CLOSED.into()))?;
            if session.owner != owner {
                return Err(Error::Terminal("다른 창의 세션입니다".into()));
            }
            session.sftp_context().ok_or(Error::Unavailable)?
        };
        Session::connect(plan, cancel, worker).await
    }

    pub(crate) fn start(&self, owner: &str, id: &str) -> Result<(), String> {
        self.get(owner, id)?.start()
    }

    pub(crate) fn input(&self, owner: &str, id: &str, bytes: &[u8]) -> Result<(), String> {
        self.get(owner, id)?.input(bytes)
    }

    fn resize(&self, owner: &str, id: &str, size: PtySize) -> Result<(), String> {
        validate_size(size)?;
        self.get(owner, id)?.resize(size)
    }

    pub(crate) fn ack(&self, owner: &str, id: &str, position: u64) -> Result<(), String> {
        self.get(owner, id)?.ack(position)
    }

    fn close(&self, owner: &str, id: &str) -> Result<CloseResult, String> {
        let (session, retired) = {
            let mut registry = self.registry.lock().unwrap();
            match registry.close_results.get(id) {
                Some(entry) if entry.owner() != owner => return Err("다른 창의 세션입니다".into()),
                Some(Retired::Completed { .. }) => {
                    let Some(Retired::Completed { result, .. }) = registry.close_results.remove(id) else { unreachable!() };
                    return Ok(result);
                }
                Some(Retired::Unconfirmed(session)) => (Arc::clone(session), true),
                None => (registry.sessions.get(id).cloned().ok_or(CLOSED)?, false),
            }
        };
        if session.owner != owner { return Err("다른 창의 세션입니다".into()); }
        if retired {
            let result = session.recheck_cleanup();
            if result.warning().is_none() { self.registry.lock().unwrap().close_results.remove(id); }
            return Ok(result);
        }
        session.stop(process::Stop::Closed);
        session.wait_finished();
        let result = session.close_result();
        if result.warning().is_none() { self.registry.lock().unwrap().close_results.remove(id); }
        Ok(result)
    }

    pub(crate) fn navigate(&self, owner: &str) {
        self.stop_owner(owner, false);
    }

    /// 폐기 사건을 처음 받은 창의 명시적 종료 코드만 재요청한다.
    pub(crate) fn destroy(&self, owner: &str) -> Option<i32> {
        self.stop_owner(owner, true)
    }

    pub(crate) fn close_ssh_probes(&self, owner: Option<&str>) -> bool {
        let probes = {
            let mut registry = self.registry.lock().unwrap();
            match owner {
                Some(owner) => { registry.closing.insert(owner.into()); }
                None => {
                    if !registry.controls.is_empty() || registry.control_exit.is_some() { return false; }
                    registry.exiting = true;
                }
            }
            registry.probes.closing_ssh(owner)
        };
        let confirmed = confirm_ssh_probes(probes);
        if !confirmed { self.release_close_gate(owner); }
        confirmed
    }

    fn stop_owner(&self, owner: &str, destroyed: bool) -> Option<i32> {
        let mut registry = self.registry.lock().unwrap();
        let document = registry.documents.entry(owner.into()).or_default();
        let previous = *document;
        *document += 1;
        registry.close_results.retain(|_, entry| entry.owner() != owner);
        if destroyed {
            registry.closing.remove(owner);
            registry.destroyed.insert(owner.into());
        }
        let mut replay = None;
        if let Some(exit) = registry.control_exit.as_mut().filter(|exit| exit.owner == owner) {
            if !exit.sealed() {
                registry.control_exit = None;
                self.controls_changed.notify_all();
            } else if destroyed && let ControlExitPhase::DestroyQueued(code) = exit.phase {
                exit.phase = ControlExitPhase::Destroyed(code);
                replay = code;
            }
        }
        registry.probes.stop_owner(owner, previous);
        let mut ssm = Vec::new();
        for session in registry.sessions.values().filter(|session| session.owner == owner) {
            session.stop(process::Stop::Closed);
            if session.is_ssm() { ssm.push(Arc::clone(session)); }
        }
        // 제출은 control_submit → 레지스트리 순서다. 먼저 세션을 중지하고 레지스트리 밖에서 생성 호출을 회수한다.
        drop(registry);
        for session in ssm { session.wait_control_submit(); }
        replay
    }

    /// 사용자 승인 뒤에만 진입을 막고, 현재 작업의 미확인 증거는 시도가 끝나도 보존한다.
    pub(crate) fn begin_control_exit(&self, owner: &str, document: u64, request_id: &str) -> bool {
        let mut registry = self.registry.lock().unwrap();
        if registry.control_exit.is_some() || registry.shutdown || registry.exiting || registry.destroyed.contains(owner)
            || registry.documents.get(owner).copied().unwrap_or(0) != document
        {
            return false;
        }
        registry.retain_control_cleanup();
        registry.control_exit = Some(ControlExit {
            owner: owner.into(), document, request_id: request_id.into(), phase: ControlExitPhase::Draining,
        });
        true
    }

    pub(crate) fn release_control_exit(&self, owner: &str, document: u64, request_id: &str) {
        let mut registry = self.registry.lock().unwrap();
        if registry.control_exit.as_ref().is_some_and(|exit| !exit.sealed() && exit.matches(owner, document, request_id)) {
            registry.control_exit = None;
            self.controls_changed.notify_all();
        }
    }

    /// 취소를 전달하고 기한까지 기다린다. 대기 중 해제되거나 바뀐 시도의 결과는 반환 대상이 아니다.
    pub(crate) fn drain_controls_for_close(&self, owner: &str, document: u64, request_id: &str, timeout: Duration) -> Option<ControlExitReport> {
        let started = Instant::now();
        let controls = {
            let registry = self.registry.lock().unwrap();
            if !registry.control_exit_in(owner, document, request_id, ControlExitPhase::Draining) { return None; }
            registry.controls.values().map(|control| Arc::clone(&control.session)).collect::<Vec<_>>()
        };
        for session in controls {
            session.stop(process::Stop::Closed);
        }
        let registry = self.registry.lock().unwrap();
        let current = |registry: &Registry| registry.control_exit_in(owner, document, request_id, ControlExitPhase::Draining);
        let (registry, _) = self.controls_changed.wait_timeout_while(registry, timeout.saturating_sub(started.elapsed()), |registry| {
            current(registry) && !registry.controls.is_empty()
        }).unwrap();
        current(&registry).then(|| registry.control_report())
    }

    pub(crate) fn close_ssh_probes_for_exit(&self, owner: &str, document: u64, request_id: &str) -> Option<bool> {
        self.check_control_exit_ssh(owner, document, request_id, confirm_ssh_probes)
    }

    fn check_control_exit_ssh(&self, owner: &str, document: u64, request_id: &str, confirm: impl FnOnce(Vec<Arc<probe::Probe>>) -> bool) -> Option<bool> {
        let probes = {
            let mut registry = self.registry.lock().unwrap();
            if !registry.control_exit_in(owner, document, request_id, ControlExitPhase::Draining) { return None; }
            if !registry.controls.is_empty() { return Some(false); }
            registry.control_exit.as_mut().unwrap().phase = ControlExitPhase::CheckingSsh;
            self.controls_changed.notify_all();
            registry.probes.closing_ssh(Some(owner))
        };
        let confirmed = confirm(probes);
        let mut registry = self.registry.lock().unwrap();
        if !registry.control_exit_in(owner, document, request_id, ControlExitPhase::CheckingSsh) { return None; }
        if confirmed {
            registry.control_exit.as_mut().unwrap().phase = ControlExitPhase::Ready;
        } else {
            registry.control_exit = None;
        }
        Some(confirmed)
    }

    /// 두 레지스트리를 잠근 상태에서는 이벤트를 큐에 넣는 호출만 실행한다.
    pub(crate) fn queue_control_exit(&self, owner: &str, document: u64, request_id: &str, code: Option<i32>, destroy: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        let mut registry = self.registry.lock().unwrap();
        if !registry.controls.is_empty() { return Err("전용 제어 작업 종료 대기 중".into()); }
        if !registry.control_exit_in(owner, document, request_id, ControlExitPhase::Ready) {
            return Err("창 닫기 요청이 만료되었습니다".into());
        }
        registry.control_exit.as_mut().unwrap().phase = ControlExitPhase::DestroyQueued(code);
        let result = destroy();
        if result.is_err() { registry.control_exit = None; }
        result
    }

    /// 마지막 창의 자동 종료는 `None`, 명시적 재요청은 예약한 코드와 일치해야 한다.
    pub(crate) fn consume_control_exit_approval(&self, code: Option<i32>) -> bool {
        let mut registry = self.registry.lock().unwrap();
        let Some(exit) = registry.control_exit.as_mut().filter(|exit| exit.phase == ControlExitPhase::Destroyed(code)) else { return false; };
        exit.phase = ControlExitPhase::Consumed;
        true
    }

    pub(crate) fn shutdown(&self) {
        let (sessions, probes, fallback) = {
            let mut registry = self.registry.lock().unwrap();
            registry.shutdown = true;
            self.controls_changed.notify_all();
            registry.close_results.clear();
            let mut sessions = registry.sessions.values().cloned().collect::<Vec<_>>();
            let fallback = !registry.control_exit.as_ref().is_some_and(ControlExit::sealed);
            if fallback {
                sessions.extend(registry.controls.values().map(|control| Arc::clone(&control.session)));
            }
            (sessions, registry.probes.stop_all(), fallback)
        };
        for session in &sessions {
            session.stop(process::Stop::Closed);
        }
        for session in sessions {
            session.wait_finished();
        }
        for probe in probes {
            probe.wait_finished();
            if probe.is_ssh() { let _ = probe.cancel_result(true); }
        }
        if fallback {
            let report = self.registry.lock().unwrap().control_report();
            if let Some(warning) = report.warning() { eprintln!("{warning}"); }
        }
    }
}

fn confirm_ssh_probes(probes: Vec<Arc<probe::Probe>>) -> bool {
    for probe in &probes { probe.stop(); }
    probes.into_iter().all(|probe| probe.confirm_ssh_close())
}

fn validate_size(size: PtySize) -> Result<(), String> {
    if !(1..=i16::MAX as u16).contains(&size.rows) || !(1..=i16::MAX as u16).contains(&size.cols) {
        return Err("size: 행과 열은 각각 1~32767이어야 합니다".into());
    }
    Ok(())
}

fn stage(name: &str, error: impl std::fmt::Display) -> String {
    format!("{name}: {error}")
}

pub(crate) fn probe_size(rows: u16, cols: u16) -> Result<PtySize, String> {
    let size = size(rows, cols);
    validate_size(size)?;
    Ok(size)
}

fn size(rows: u16, cols: u16) -> PtySize {
    PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
}

const CHUNK: usize = 16 * 1024;
const WINDOW: usize = 256 * 1024;

const CLOSED: &str = "세션이 닫혔습니다";

#[cfg(test)]
impl Drop for Sessions {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum End {
    Exited(u32),
    Disconnected(String),
    Connecting,
    Ready,
}

impl Event {
    fn send(self, channel: &tauri::ipc::Channel<tauri::ipc::InvokeResponseBody>) -> tauri::Result<()> {
        use tauri::ipc::{InvokeResponseBody, IpcResponse};
        channel.send(match self {
            Self::Bytes(bytes) => InvokeResponseBody::Raw(bytes),
            Self::Exited(code) => End::Exited(code).body()?,
            Self::Disconnected(reason) => End::Disconnected(reason).body()?,
            Self::Connecting => End::Connecting.body()?,
            Self::Ready { .. } => End::Ready.body()?,
        })
    }
}

#[derive(Debug)]
pub(crate) enum Event {
    Bytes(Vec<u8>),
    Exited(u32),
    Disconnected(String),
    Connecting,
    Ready { account_id: String },
}

fn validate_working_directory(directory: &std::path::Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        // 확장·장치·UNC 경로에는 일반 Win32 경로 정규화를 적용할 수 없다.
        if !matches!(directory.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))) {
            return Err("작업 디렉터리: Windows 드라이브의 일반 절대 경로만 지원합니다".into());
        }
    }
    if !std::fs::metadata(directory).map_err(|error| stage("작업 디렉터리", error))?.is_dir() {
        return Err("작업 디렉터리: 디렉터리가 아닙니다".into());
    }
    Ok(())
}

#[cfg(windows)]
const POWERSHELL_CWD_MATCH: &str = concat!(
    "[string]::Equals(",
    "[IO.Path]::TrimEndingDirectorySeparator([IO.Path]::GetFullPath($directory)), ",
    "[IO.Path]::TrimEndingDirectorySeparator([IO.Path]::GetFullPath([Environment]::CurrentDirectory)), ",
    "[StringComparison]::Ordinal)",
);

fn shell() -> CommandBuilder {
    #[cfg(windows)]
    let mut cmd = CommandBuilder::new("pwsh.exe");
    #[cfg(unix)]
    let mut cmd = CommandBuilder::new("/bin/bash");
    #[cfg(windows)]
    cmd.args(["-NoLogo", "-NoProfile"]);
    #[cfg(unix)]
    cmd.args(["--noprofile", "--norc", "-i"]);
    cmd.env("TERM", "xterm-256color");
    cmd
}
