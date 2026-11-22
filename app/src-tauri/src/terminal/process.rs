use super::remote;
use crate::ssm::CancellationToken;
use crate::ssh::{ConnectionPlan, terminal::DirectLaunch};
use super::{CHUNK, CLOSED, CloseResult, ControlBinding, Event, LocalClose, WINDOW, stage};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    io::{self, Read, Write},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
use tokio_util::task::{TaskTracker, task_tracker::TaskTrackerToken};

const POLL: Duration = Duration::from_millis(10);
const START_TIMEOUT: Duration = Duration::from_secs(5);
const ACK_TIMEOUT: Duration = Duration::from_secs(15);
const ACK_STALLED: &str = "ack: 출력 처리 확인 시간이 초과되었습니다";
const WRITE_TIMED_OUT: &str = "제어 세션 쓰기 시간이 초과되었습니다";

pub(crate) enum Stop {
    Closed,
    Disconnected(String),
}

#[derive(Default)]
struct Flow {
    connected: bool,
    started: bool,
    attached: bool,
    attach_error: Option<String>,
    start_deadline: Option<std::time::Instant>,
    child_exited: bool,
    finished: bool,
    stop: Option<Stop>,
    ssm_identity: Option<remote::SsmIdentity>,
    sftp: Option<SftpOwner>,
    sent: u64,
    acked: u64,
    ack_deadline: Option<std::time::Instant>,
    write_deadline: Option<Instant>,
}

struct SftpOwner {
    plan: Arc<ConnectionPlan>,
    cancel: CancellationToken,
    workers: Option<TaskTracker>,
}

impl Flow {
    fn expire_start(&mut self) {
        if self.stop.is_none() && self.start_deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            self.stop = Some(Stop::Disconnected("start: 시작 요청 시간이 초과되었습니다".into()));
        }
    }

    fn expire_ack(&mut self) {
        if self.stop.is_none() && self.ack_deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            self.stop = Some(Stop::Disconnected(ACK_STALLED.into()));
        }
    }

    fn expire_write(&mut self) {
        if self.stop.is_none() && self.write_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            self.stop = Some(Stop::Disconnected(WRITE_TIMED_OUT.into()));
        }
    }

    fn live(&self) -> Result<(), String> {
        if !self.started || self.child_exited || self.stop.is_some() {
            return Err(CLOSED.into());
        }
        Ok(())
    }
}

pub(crate) enum Launch {
    Local(CommandBuilder),
    Ssm(remote::Target),
    AiControl(remote::Target, Arc<ControlBinding>),
    Ssh(DirectLaunch),
}

impl Launch {
    pub(super) fn is_ssm(&self) -> bool {
        matches!(self, Self::Ssm(_) | Self::AiControl(..))
    }
}

pub(crate) struct Session {
    pub(super) owner: String,
    pub(super) document: Option<u64>,
    control: bool,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    cancel: Option<CancellationToken>,
    close_result: Mutex<CloseResult>,
    unconfirmed: Mutex<Option<remote::Remote>>,
    ssh_pending: Mutex<Option<OwnedSsh>>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    flow: Mutex<Flow>,
    wake: Condvar,
}

struct OwnedSsh {
    child: Box<dyn Child + Send + Sync>,
    _plan: Arc<ConnectionPlan>,
}

impl Session {
    pub(super) fn new(owner: &str, ssm: bool) -> Self {
        Self::with_cancel(owner, ssm.then(CancellationToken::new), false)
    }

    pub(crate) fn dedicated(binding: &ControlBinding) -> Self {
        Self::with_cancel("", Some(binding.cancel_token().clone()), true)
    }

    fn with_cancel(owner: &str, cancel: Option<CancellationToken>, control: bool) -> Self {
        Self {
            owner: owner.into(),
            document: None,
            control,
            cancel,
            close_result: Mutex::new(CloseResult::default()),
            unconfirmed: Mutex::new(None),
            ssh_pending: Mutex::new(None),
            master: Mutex::new(None),
            writer: Mutex::new(None),
            flow: Mutex::new(Flow::default()),
            wake: Condvar::new(),
        }
    }

    pub(super) fn is_ssm(&self) -> bool {
        self.cancel.is_some()
    }

    pub(super) fn with_ssm_identity<T>(&self, check: impl FnOnce(&remote::SsmIdentity, &CancellationToken) -> T) -> Option<T> {
        let flow = self.flow.lock().unwrap();
        flow.live().ok()?;
        let cancel = self.cancel.as_ref()?;
        if flow.finished || cancel.is_cancelled() { return None; }
        Some(check(flow.ssm_identity.as_ref()?, cancel))
    }

    pub(super) fn sftp_context(&self) -> Option<(Arc<ConnectionPlan>, CancellationToken, TaskTrackerToken)> {
        let mut flow = self.flow.lock().unwrap();
        flow.live().ok()?;
        let sftp = flow.sftp.as_mut()?;
        let worker = sftp.workers.get_or_insert_with(TaskTracker::new).token();
        Some((Arc::clone(&sftp.plan), sftp.cancel.child_token(), worker))
    }

    pub(crate) fn launch(
        self: &Arc<Self>,
        size: PtySize,
        command: Launch,
        send: impl Fn(Event) -> Result<(), String> + Send + 'static,
        retired: impl FnOnce(&Arc<Session>) + Send + 'static,
    ) -> Result<(), String> {
        self.begin_launch(size, command, send, retired)?.recv()
            .map_err(|error| stage("open handshake", error))?
    }

    pub(super) fn begin_launch(
        self: &Arc<Self>,
        size: PtySize,
        command: Launch,
        send: impl Fn(Event) -> Result<(), String> + Send + 'static,
        retired: impl FnOnce(&Arc<Session>) + Send + 'static,
    ) -> Result<mpsc::Receiver<Result<(), String>>, String> {
        if self.control != matches!(command, Launch::AiControl(..)) || self.control && self.document.is_some() {
            self.finish();
            return Err(stage("open", "제어 세션에는 전용 PTY가 필요합니다"));
        }
        let this = Arc::clone(self);
        let (ready, opened) = mpsc::sync_channel(1);
        let ssm = command.is_ssm();
        if let Err(error) = thread::Builder::new().name("terminal-pty".into()).spawn(move || {
            let mut retired = Some(retired);
            let mut complete = || {
                if let Some(retired) = retired.take() {
                    retired(&this);
                    this.finish();
                }
            };
            let result = this.run(size, command, send, &ready, &mut complete);
            if let Err(error) = result {
                if ssm {
                    this.flow.lock().unwrap().attach_error = Some(error);
                } else {
                    let _ = ready.send(Err(error));
                }
            }
            complete();
            // 문서나 탭이 사라져도 자식과 계획은 실제 종료 확인까지 감독자가 소유한다.
            while this.observe_ssh() {
                thread::sleep(Duration::from_millis(250));
            }
        }) {
            self.finish();
            return Err(stage("PTY worker", error));
        }
        Ok(opened)
    }

    fn run(
        &self,
        size: PtySize,
        command: Launch,
        send: impl Fn(Event) -> Result<(), String> + Send + 'static,
        ready: &mpsc::SyncSender<Result<(), String>>,
        complete: &mut impl FnMut(),
    ) -> Result<(), String> {
        let ssm = command.is_ssm();
        if ssm {
            self.flow.lock().unwrap().start_deadline = Some(std::time::Instant::now() + START_TIMEOUT);
            let _ = ready.send(Ok(()));
            let mut flow = self.flow.lock().unwrap();
            while let Some(deadline) = flow.start_deadline {
                flow.expire_start();
                if flow.stop.is_some() { return Err(CLOSED.into()); }
                flow = self.wake.wait_timeout(flow, deadline.saturating_duration_since(std::time::Instant::now())).unwrap().0;
            }
            if flow.stop.is_some() { return Err(CLOSED.into()); }
        }
        let control = match &command {
            Launch::AiControl(_, binding) => Some(binding.as_ref()),
            _ => None,
        };
        let _cancel_control = control.map(|binding| {
            let guard = binding.cancel_token().clone().drop_guard();
            binding.admit(|| Ok(()))?;
            binding.forward_request_cancel();
            Ok::<_, String>(guard)
        }).transpose()?;
        let mut remote = match &command {
            Launch::Ssm(target) => Some(tauri::async_runtime::block_on(target.prepare(None, self.cancel.as_ref().unwrap()))?),
            Launch::AiControl(target, binding) => Some(tauri::async_runtime::block_on(target.prepare_control(binding))?),
            Launch::Local(_) | Launch::Ssh(_) => None,
        };
        if self.flow.lock().unwrap().stop.is_some() {
            return Err(stage("open", CLOSED));
        }
        if ssm { send(Event::Connecting)?; }
        let mut ssh_plan = None;
        #[cfg(windows)]
        let job = remote.as_ref().map(|_| process_job::Job::new()).transpose()
            .map_err(|error| stage("SSM Job", error))?;
        let (pair, mut remote_child) = if let Some(request) = remote.as_mut() {
            match request.spawn(size, self.cancel.as_ref().unwrap()) {
                Ok((pair, child)) => (pair, Some(child)),
                Err(error) => return Err(self.after_failed_spawn(error, remote.take())),
            }
        } else {
            (native_pty_system().openpty(size).map_err(|e| stage("openpty", e))?, None)
        };
        let setup = || {
            // portable-pty 0.9는 프로세스 생성 시 Job 목록을 받지 않는다.
            #[cfg(windows)]
            if let (Some(job), Some(child)) = (&job, &remote_child) {
                job.assign(child.as_ref()).map_err(|error| stage("SSM Job", error))?;
            }
            #[cfg(unix)]
            set_nonblocking(pair.master.as_ref()).map_err(|e| stage("nonblocking", e))?;
            let reader = pair.master.try_clone_reader().map_err(|e| stage("clone reader", e))?;
            let writer = pair.master.take_writer().map_err(|e| stage("take writer", e))?;
            Ok((reader, writer))
        };
        let (reader, writer) = match setup() {
            Ok(io) => io,
            Err(error) => {
                if let Some(child) = remote_child.as_mut() { self.terminate(child.as_mut(), Some(pair.master.as_ref())); }
                #[cfg(windows)]
                drop(job);
                drop(pair);
                return Err(self.after_failed_spawn(error, remote.take()));
            }
        };
        let send = Mutex::new(send);
        let send_event = |event| (send.lock().unwrap())(event);
        thread::scope(|scope| {
            let (event_tx, event_rx) = mpsc::sync_channel(1);
            let (diagnostic_tx, diagnostic_rx) = mpsc::sync_channel(1);
            let drain = thread::Builder::new().name("terminal-output".into()).spawn_scoped(scope, move || {
                if ssm {
                    drop(self.wake.wait_while(self.flow.lock().unwrap(), |flow| {
                        !flow.attached && flow.stop.is_none()
                    }).unwrap());
                }
                let mut diagnostic = ssm.then(remote::StartDiagnostic::new);
                let end = self.drain(reader, &send_event, diagnostic.as_mut());
                let _ = diagnostic_tx.send(if end.is_ok() { diagnostic } else { None });
                if let Err(error) = end {
                    self.stop(Stop::Disconnected(error));
                }
                // Only this worker sends, including the final status, so channel order is stable.
                if let Ok(event) = event_rx.recv() {
                    let mut event = match self.flow.lock().unwrap().stop.as_ref() {
                        Some(Stop::Closed) => None,
                        Some(Stop::Disconnected(error)) => Some(Event::Disconnected(error.clone())),
                        None => event,
                    };
                    if let Some(error) = self.close_result().warning() {
                        event = Some(Event::Disconnected(match event {
                            Some(Event::Disconnected(reason)) => format!("{reason}; {error}"),
                            _ => error,
                        }));
                    }
                    if let Some(event) = event {
                        let _ = send_event(event);
                    }
                }
            });
            let drain = match drain {
                Ok(drain) => drain,
                Err(error) => {
                    if let Some(child) = remote_child.as_mut() { self.terminate(child.as_mut(), Some(pair.master.as_ref())); }
                    #[cfg(windows)]
                    drop(job);
                    drop(writer);
                    drop(pair);
                    return Err(self.after_failed_spawn(stage("reader worker", error), remote.take()));
                }
            };
            let command = match command {
                Launch::Ssm(_) | Launch::AiControl(..) => None,
                Launch::Local(command) => Some(command),
                Launch::Ssh(launch) => {
                    let (command, plan) = launch.into_parts();
                    ssh_plan = Some(plan);
                    Some(command)
                }
            };
            let mut child = match command {
                None => remote_child.take().unwrap(),
                Some(command) => {
                    if self.flow.lock().unwrap().stop.is_some() {
                        self.teardown(pair, writer, event_tx, drain);
                        return Err(stage("open", CLOSED));
                    }
                    match pair.slave.spawn_command(command) {
                        Ok(child) => child,
                        Err(error) => {
                            self.teardown(pair, writer, event_tx, drain);
                            return Err(stage("spawn", format_args!("{error:#}")));
                        }
                    }
                }
            };
            self.close_result.lock().unwrap().local = LocalClose::Unconfirmed;
            drop(pair.slave);
            *self.writer.lock().unwrap() = Some(writer);
            *self.master.lock().unwrap() = Some(pair.master);
            {
                let mut flow = self.flow.lock().unwrap();
                flow.attached = true;
                if flow.stop.is_none() {
                    flow.sftp = ssh_plan.as_ref().map(|plan| SftpOwner {
                        plan: Arc::clone(plan),
                        cancel: CancellationToken::new(),
                        workers: None,
                    });
                }
                if !ssm { flow.start_deadline = Some(std::time::Instant::now() + START_TIMEOUT); }
                self.wake.notify_all();
            }
            if !ssm { let _ = ready.send(Ok(())); }

            let mut connected = false;
            let event = thread::scope(|scope| {
                if let Some(remote) = remote.as_ref() {
                    let connected = &mut connected;
                    scope.spawn(move || {
                        let cancel = self.cancel.as_ref().unwrap();
                        let result = remote.wait_connected(cancel, self.document.is_some()).and_then(|identity| {
                            *connected = true;
                            if !cancel.is_cancelled() {
                                {
                                    let mut flow = self.flow.lock().unwrap();
                                    if flow.live().is_ok() {
                                        flow.connected = self.control;
                                        flow.ssm_identity = identity;
                                    }
                                }
                                send_event(Event::Ready { account_id: remote.account_id().into() })?;
                            }
                            Ok(())
                        });
                        if let Err(error) = result && !cancel.is_cancelled() {
                            self.stop(Stop::Disconnected(error));
                        }
                    });
                }
                let event = self.reap(child.as_mut(), ssh_plan.is_some());
                if let Some(cancel) = &self.cancel { cancel.cancel(); }
                event
            });
            let sftp = self.flow.lock().unwrap().sftp.take();
            if let Some(SftpOwner { cancel, workers, .. }) = sftp {
                cancel.cancel();
                if let Some(workers) = workers {
                    workers.close();
                    if !workers.is_empty() {
                        tauri::async_runtime::block_on(workers.wait());
                    }
                }
            }
            if let Some(plan) = ssh_plan
                && self.close_result().local == LocalClose::Unconfirmed
            {
                *self.ssh_pending.lock().unwrap() = Some(OwnedSsh { child, _plan: plan });
                // ConPTY 해제와 출력 스레드 대기는 회수 오류 응답을 지연시키지 않는다.
                complete();
            }
            #[cfg(windows)]
            drop(job);
            let master = self.master.lock().unwrap().take();
            #[cfg(windows)]
            if let Ok(mut writer) = self.writer.try_lock() {
                writer.take();
            }
            // ConPTY closes with the drain worker still available. A stop bypasses ACK credit
            // and drains/discards until the console releases its output pipe.
            drop(master);
            self.writer.lock().unwrap().take();
            let event = match event {
                Some(Event::Exited(254)) if ssm && !connected => {
                    if let Some(error) = diagnostic_rx.recv().ok().flatten().and_then(|output| output.rejection(254)) {
                        drop(remote.take());
                        Some(Event::Disconnected(error))
                    } else {
                        Some(Event::Exited(254))
                    }
                }
                event => event,
            };
            self.cleanup(remote.take());
            let _ = event_tx.send(event);
            let _ = drain.join();
            Ok(())
        })
    }

    fn cleanup(&self, remote: Option<remote::Remote>) {
        if let Some(mut remote) = remote {
            let (status, target) = remote.close();
            let mut result = self.close_result.lock().unwrap();
            result.remote = status;
            result.target = target;
            if status == remote::RemoteClose::Unconfirmed {
                *self.unconfirmed.lock().unwrap() = Some(remote);
            }
        }
    }

    fn after_failed_spawn(&self, error: String, remote: Option<remote::Remote>) -> String {
        self.cleanup(remote);
        match self.close_result().warning() {
            None => error,
            Some(cleanup) => format!("{error}; {cleanup}"),
        }
    }

    pub(crate) fn close_result(&self) -> CloseResult {
        self.close_result.lock().unwrap().clone()
    }

    pub(super) fn cleanup_result(&self) -> Result<(), String> {
        self.close_result().warning().map_or(Ok(()), Err)
    }

    pub(super) fn recheck_cleanup(&self) -> CloseResult {
        {
            let mut pending = self.ssh_pending.lock().unwrap();
            if let Some(ssh) = pending.as_mut() {
                match terminate_ssh(ssh.child.as_mut()) {
                    Ok(_) => self.confirm_ssh(&mut pending),
                    Err(error) => self.set_ssh_error(error),
                }
            }
        }
        let mut request = self.unconfirmed.lock().unwrap();
        if request.as_ref().is_some_and(remote::Remote::recheck_closed) {
            request.take();
            self.close_result.lock().unwrap().remote = remote::RemoteClose::Confirmed;
        }
        self.close_result()
    }

    fn confirm_ssh(&self, pending: &mut Option<OwnedSsh>) {
        pending.take();
        let mut result = self.close_result.lock().unwrap();
        result.local = LocalClose::CliReaped;
        result.local_error = None;
    }

    fn observe_ssh(&self) -> bool {
        let mut pending = self.ssh_pending.lock().unwrap();
        if let Some(ssh) = pending.as_mut()
            && matches!(poll_ssh(ssh.child.as_mut()), Ok(Some(_)))
        {
            self.confirm_ssh(&mut pending);
        }
        pending.is_some()
    }

    fn set_ssh_error(&self, error: String) {
        self.close_result.lock().unwrap().local_error = Some(stage("SSH 자식 종료 확인 불가", error));
    }

    fn teardown(
        &self,
        pair: portable_pty::PtyPair,
        writer: Box<dyn Write + Send>,
        event: mpsc::SyncSender<Option<Event>>,
        drain: thread::ScopedJoinHandle<'_, ()>,
    ) {
        self.stop(Stop::Closed);
        drop(writer);
        drop(pair);
        drop(event);
        let _ = drain.join();
    }

    fn reap(&self, child: &mut dyn Child, ssh: bool) -> Option<Event> {
        loop {
            let mut flow = self.flow.lock().unwrap();
            flow.expire_start();
            flow.expire_ack();
            if self.control {
                flow.expire_write();
                if flow.stop.is_none() && self.cancel.as_ref().unwrap().is_cancelled() {
                    flow.stop = Some(Stop::Closed);
                }
            }
            if flow.stop.is_some() {
                self.wake.notify_all();
                drop(flow);
                if ssh {
                    return self.ssh_exit(terminate_ssh(child).map(|_| None));
                }
                self.terminate(child, self.master.lock().unwrap().as_deref().map(|master| master as &dyn MasterPty));
                return None;
            }
            if let Some(deadline) = flow.start_deadline {
                // Keep the ConPTY reader ready to drain on start or timeout.
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                drop(self.wake.wait_timeout(flow, remaining).unwrap());
                continue;
            }
            if ssh {
                match poll_ssh(child) {
                    Ok(None) => drop(self.wake.wait_timeout(flow, POLL).unwrap()),
                    result => {
                        flow.child_exited = true;
                        self.wake.notify_all();
                        drop(flow);
                        return self.ssh_exit(result.map_err(|error| stage("wait", error)));
                    }
                }
                continue;
            }
            match child_ready(child) {
                Ok(true) => {
                    flow.child_exited = true;
                    self.wake.notify_all();
                    drop(flow);
                    return Some(match child.wait() {
                        Ok(status) => {
                            self.close_result.lock().unwrap().local = LocalClose::CliReaped;
                            Event::Exited(status.exit_code())
                        }
                        Err(error) => Event::Disconnected(stage("wait", error)),
                    });
                }
                Ok(false) => drop(self.wake.wait_timeout(flow, POLL).unwrap()),
                Err(error) => {
                    flow.child_exited = true;
                    drop(flow);
                    self.stop(Stop::Disconnected(stage("wait", error)));
                    self.terminate(child, self.master.lock().unwrap().as_deref().map(|master| master as &dyn MasterPty));
                    return None;
                }
            }
        }
    }

    fn ssh_exit(&self, result: Result<Option<portable_pty::ExitStatus>, String>) -> Option<Event> {
        match result {
            Ok(status) => {
                self.close_result.lock().unwrap().local = LocalClose::CliReaped;
                status.map(|status| Event::Exited(status.exit_code()))
            }
            Err(error) => {
                self.set_ssh_error(error);
                // 종료 상태 조회 실패 후에도 입출력을 깨우고 출력 크레딧 대기를 푼다.
                let mut flow = self.flow.lock().unwrap();
                if flow.stop.is_none() {
                    flow.stop = Some(Stop::Closed);
                }
                self.wake.notify_all();
                None
            }
        }
    }

    fn terminate(&self, child: &mut dyn Child, _master: Option<&dyn MasterPty>) {
        self.close_result.lock().unwrap().local = LocalClose::Unconfirmed;
        #[cfg(unix)]
        if let Some(pid) = child.process_id() {
            if let Some(master) = _master {
                kill_foreground(master, pid as libc::pid_t);
            }
            // Only the supervisor reaps the child, so its PID cannot yet be recycled.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
        #[cfg(windows)]
        {
            let _ = child.kill();
        }
        if child.wait().is_ok() {
            self.close_result.lock().unwrap().local = LocalClose::CliReaped;
        }
    }

    fn drain(&self, mut reader: Box<dyn Read + Send>, send: &impl Fn(Event) -> Result<(), String>, mut diagnostic: Option<&mut remote::StartDiagnostic>) -> Result<(), String> {
        let mut buffer = [0; CHUNK];
        loop {
            let mut flow = self.flow.lock().unwrap();
            while flow.stop.is_none() && (!flow.started || flow.sent - flow.acked == WINDOW as u64) {
                flow = self.wait_for_ack(flow);
            }
            let limit = if flow.stop.is_some() {
                #[cfg(unix)]
                return Ok(());
                #[cfg(windows)]
                CHUNK
            } else {
                CHUNK.min(WINDOW - (flow.sent - flow.acked) as usize)
            };
            drop(flow);

            let count = match reader.read(&mut buffer[..limit]) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                #[cfg(unix)]
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let flow = self.flow.lock().unwrap();
                    if flow.stop.is_some() { break; }
                    if flow.child_exited {
                        drop(flow);
                        // The child could have written after the first EAGAIN but before reaping.
                        match reader.read(&mut buffer[..limit]) {
                            Ok(0) => break,
                            Ok(count) => count,
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                            Err(error) => return Err(stage("read", error)),
                        }
                    } else {
                        drop(self.wake.wait_timeout(flow, POLL).unwrap());
                        continue;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break,
                Err(error) => {
                    // The reader must keep draining on ConPTY shutdown after an IPC error;
                    // a failed native read itself has already released the pending read.
                    return Err(stage("read", error));
                }
            };
            if let Some(output) = &mut diagnostic { output.record(&buffer[..count]); }
            let mut flow = self.flow.lock().unwrap();
            if flow.stop.is_some() { continue; }
            if flow.sent == flow.acked {
                flow.ack_deadline = Some(std::time::Instant::now() + ACK_TIMEOUT);
            }
            flow.sent += count as u64;
            drop(flow);
            if let Err(error) = send(Event::Bytes(buffer[..count].to_vec())) {
                self.stop(Stop::Disconnected(stage("channel", error)));
            }
        }
        drop(reader);
        let mut flow = self.flow.lock().unwrap();
        while flow.stop.is_none() && flow.sent != flow.acked {
            flow = self.wait_for_ack(flow);
        }
        Ok(())
    }

    fn wait_for_ack<'a>(&'a self, flow: std::sync::MutexGuard<'a, Flow>) -> std::sync::MutexGuard<'a, Flow> {
        let Some(deadline) = flow.ack_deadline else { return self.wake.wait(flow).unwrap(); };
        let (mut flow, _) = self.wake.wait_timeout(flow, deadline.saturating_duration_since(std::time::Instant::now())).unwrap();
        flow.expire_ack();
        if flow.stop.is_some() { self.wake.notify_all(); }
        flow
    }

    pub(super) fn start(&self) -> Result<(), String> {
        self.start_until(None)
    }

    pub(crate) fn start_until(&self, deadline: Option<Instant>) -> Result<(), String> {
        let mut flow = self.flow.lock().unwrap();
        flow.expire_start();
        if flow.stop.is_some() || flow.finished || (!flow.started && flow.start_deadline.is_none()) {
            self.wake.notify_all();
            return Err(CLOSED.into());
        }
        flow.started = true;
        flow.start_deadline = None;
        self.wake.notify_all();
        if self.cancel.is_some() {
            let pending = |flow: &mut Flow| !flow.attached && !flow.finished && flow.stop.is_none();
            flow = match deadline {
                Some(deadline) => {
                    let (state, timeout) = self.wake.wait_timeout_while(flow, deadline.saturating_duration_since(std::time::Instant::now()), pending).unwrap();
                    if timeout.timed_out() { return Err(super::probe::TIMED_OUT.into()); }
                    state
                }
                None => self.wake.wait_while(flow, pending).unwrap(),
            };
            if let Some(error) = flow.attach_error.as_ref() { return Err(error.clone()); }
            if !flow.attached || flow.stop.is_some() { return Err(CLOSED.into()); }
        }
        Ok(())
    }

    pub(crate) fn ack(&self, position: u64) -> Result<(), String> {
        let mut flow = self.flow.lock().unwrap();
        if flow.stop.is_some() { return Err(CLOSED.into()); }
        if position > flow.sent { return Err("ack: 전송 범위를 넘었습니다".into()); }
        if position > flow.acked {
            flow.acked = position;
            flow.ack_deadline = (flow.acked < flow.sent).then(|| std::time::Instant::now() + ACK_TIMEOUT);
            self.wake.notify_all();
        }
        Ok(())
    }

    pub(super) fn resize(&self, size: PtySize) -> Result<(), String> {
        self.flow.lock().unwrap().live()?;
        self.master.lock().unwrap().as_ref().ok_or(CLOSED)?.resize(size)
            .map_err(|error| stage("resize", error))
    }

    pub(crate) fn connected(&self) -> bool {
        let flow = self.flow.lock().unwrap();
        flow.connected && !flow.finished && flow.live().is_ok()
            && self.cancel.as_ref().is_some_and(|cancel| !cancel.is_cancelled())
    }

    pub(crate) fn control_input(&self, binding: &ControlBinding, bytes: &[u8], deadline: Instant, frame: bool) -> Result<(), String> {
        if !self.control { return Err(CLOSED.into()); }
        let cancel = self.cancel.as_ref().unwrap();
        binding.admit(|| {
            let mut flow = self.flow.lock().unwrap();
            if frame && !flow.connected { return Err("공식 SSM 연결을 확인할 수 없습니다".into()); }
            flow.live()?;
            if cancel.is_cancelled() || flow.write_deadline.is_some() { return Err(CLOSED.into()); }
            flow.write_deadline = Some(deadline);
            self.wake.notify_all();
            Ok(())
        })?;
        let result = self.write_input(bytes, Some(binding));
        let mut flow = self.flow.lock().unwrap();
        flow.expire_write();
        flow.write_deadline = None;
        self.wake.notify_all();
        match &flow.stop {
            Some(Stop::Disconnected(error)) => Err(error.clone()),
            _ if cancel.is_cancelled() => Err(CLOSED.into()),
            _ => result.and_then(|()| flow.live()),
        }
    }

    pub(super) fn input(&self, bytes: &[u8]) -> Result<(), String> {
        if self.control { return Err(CLOSED.into()); }
        self.write_input(bytes, None)
    }

    fn write_input(&self, mut bytes: &[u8], control: Option<&ControlBinding>) -> Result<(), String> {
        self.flow.lock().unwrap().live()?;
        let mut writer = self.writer.lock().unwrap();
        let writer = writer.as_mut().ok_or(CLOSED)?;
        while !bytes.is_empty() {
            {
                let mut flow = self.flow.lock().unwrap();
                if let Some(binding) = control {
                    binding.check_request()?;
                    flow.expire_write();
                    if self.cancel.as_ref().unwrap().is_cancelled() { return Err(CLOSED.into()); }
                }
                flow.live()?;
            }
            let error: io::Error = match writer.write(bytes) {
                Ok(0) => io::ErrorKind::WriteZero.into(),
                Ok(count) => { bytes = &bytes[count..]; continue; }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                #[cfg(unix)]
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let flow = self.flow.lock().unwrap();
                    flow.live()?;
                    drop(self.wake.wait_timeout(flow, POLL).unwrap());
                    continue;
                }
                Err(error) => error,
            };
            let error = stage("write", error);
            self.stop(Stop::Disconnected(error.clone()));
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn stop(&self, reason: Stop) {
        // 중앙 세션 확인과 수동 종료는 같은 흐름 잠금에서 순서를 정한다.
        let mut flow = self.flow.lock().unwrap();
        if let Some(cancel) = &self.cancel { cancel.cancel(); }
        if let Some(sftp) = &flow.sftp { sftp.cancel.cancel(); }
        if flow.stop.is_none() || matches!(reason, Stop::Closed) {
            flow.stop = Some(reason);
        }
        self.wake.notify_all();
    }

    fn finish(&self) {
        self.flow.lock().unwrap().finished = true;
        self.wake.notify_all();
    }

    pub(crate) fn wait_finished(&self) {
        drop(self.wake.wait_while(self.flow.lock().unwrap(), |flow| !flow.finished).unwrap());
    }
}

#[cfg(unix)]
fn child_ready(child: &mut dyn Child) -> io::Result<bool> {
    child.try_wait().map(|status| status.is_some())
}

#[cfg(windows)]
fn child_ready(child: &mut dyn Child) -> io::Result<bool> {
    ssh_child::ready(child)
}

#[cfg(unix)]
fn set_nonblocking(master: &dyn MasterPty) -> io::Result<()> {
    let fd = master.as_raw_fd().ok_or(io::ErrorKind::Unsupported)?;
    // dup shares these status flags, so both reader and writer can notice a stop.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn kill_foreground(master: &dyn MasterPty, pid: libc::pid_t) {
    if let (Some(fd), Some(group)) = (master.as_raw_fd(), master.process_group_leader()) {
        // The tty's session owns its foreground group, including a group whose leader exited.
        if group > 1 && unsafe { libc::tcgetsid(fd) } == pid && unsafe { libc::tcgetpgrp(fd) } == group {
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
    }
}

#[cfg(windows)]
mod process_job;

mod ssh_child;
#[cfg(test)]
mod probe_cleanup_tests;
use ssh_child::{poll as poll_ssh, terminate as terminate_ssh};

#[cfg(test)]
pub(crate) struct LiveSsmFixture<'a> {
    sessions: &'a super::Sessions,
    id: String,
    pub(crate) identity: super::LiveSsmIdentity,
}

#[cfg(test)]
impl LiveSsmFixture<'_> {
    pub(crate) fn without_role(mut self) -> Self {
        self.identity.role = None;
        let registry = self.sessions.registry.lock().unwrap();
        registry.sessions[&self.id].flow.lock().unwrap().ssm_identity.as_mut().unwrap().role = None;
        drop(registry);
        self
    }

    pub(crate) fn stop(&self) {
        let session = self.sessions.registry.lock().unwrap().sessions[&self.id].clone();
        session.stop(Stop::Closed);
    }
}

#[cfg(test)]
impl Drop for LiveSsmFixture<'_> {
    fn drop(&mut self) {
        if let Some(session) = self.sessions.registry.lock().unwrap().sessions.remove(&self.id) {
            session.stop(Stop::Closed);
        }
    }
}

#[cfg(test)]
impl super::Sessions {
    pub(crate) fn insert_live_ssm_fixture(&self, owner: &str, document: u64, account_id: &str, region: &str, instance_id: &str) -> super::LiveSsmFixture<'_> {
        let mut session = Session::new(owner, true);
        session.document = Some(document);
        let role = Some(crate::ssm::AssumedRole::fixture(account_id));
        *session.flow.get_mut().unwrap() = Flow {
            started: true,
            ssm_identity: Some(remote::SsmIdentity {
                account_id: account_id.into(), profile: "fixture".into(), region: region.into(), instance_id: instance_id.into(),
                role: role.clone(),
            }),
            ..Flow::default()
        };
        let id = self.next_id();
        self.registry.lock().unwrap().sessions.insert(id.clone(), Arc::new(session));
        let identity = super::LiveSsmIdentity {
            id: id.clone(), document, account_id: account_id.into(), profile: "fixture".into(), region: region.into(), instance_id: instance_id.into(),
            role,
        };
        LiveSsmFixture { sessions: self, id, identity }
    }
}

#[cfg(test)]
impl Session {
    pub(crate) fn attach_control_writer_fixture(&self, writer: impl Write + Send + 'static) {
        assert!(self.control);
        *self.writer.lock().unwrap() = Some(Box::new(writer));
        let mut flow = self.flow.lock().unwrap();
        flow.started = true;
        flow.connected = true;
    }
}

#[cfg(test)]
#[test]
fn live_ssm_identity_is_checked_under_registry_and_flow_locks() {
    use super::{LiveSsmIdentity, Sessions};

    let sessions = Sessions::default();
    let document = sessions.document("main");
    let fixture = sessions.insert_live_ssm_fixture("main", document, "123456789012", "us-west-2", "i-01234567");
    let expected = fixture.identity.clone();
    let id = &expected.id;
    let session = sessions.get("main", id).unwrap();
    let lookup = |document| sessions.live_ssm_identity("main", document, id);
    let identity = session.flow.lock().unwrap().ssm_identity.take();
    let before_ready = lookup(document);
    session.flow.lock().unwrap().ssm_identity = identity;
    let live = lookup(document);
    let other_owner = sessions.live_ssm_identity("other", document, id);
    let other_document = sessions.live_ssm_identity("main", document + 1, id);
    let check = || sessions.with_live_ssm_session("main", &expected, || 7);
    let locked = sessions.with_live_ssm_session("main", &expected, || {
        (sessions.registry.try_lock().is_err(), session.flow.try_lock().is_err())
    });
    let wrong_node = sessions.with_live_ssm_session("main", &LiveSsmIdentity {
        instance_id: "i-76543210".into(), ..expected.clone()
    }, || 7);
    let wrong_profile = sessions.with_live_ssm_session("main", &LiveSsmIdentity {
        profile: "another-profile".into(), ..expected.clone()
    }, || 7);
    let wrong_role = sessions.with_live_ssm_session("main", &LiveSsmIdentity {
        role: None, ..expected.clone()
    }, || 7);
    session.flow.lock().unwrap().child_exited = true;
    let exited = check();
    session.flow.lock().unwrap().child_exited = false;
    sessions.registry.lock().unwrap().documents.insert("main".into(), document + 1);
    let old_document = lookup(document);
    let current_document = lookup(document + 1);
    sessions.registry.lock().unwrap().documents.remove("main");
    session.cancel.as_ref().unwrap().cancel();
    let cancelled = check();
    session.stop(Stop::Closed);
    let stopped = check();
    drop(fixture);
    let removed = lookup(document);

    assert_eq!(before_ready, None);
    assert_eq!(live, Some(expected));
    assert_eq!(other_owner, None);
    assert_eq!(other_document, None);
    assert_eq!(locked, Some((true, true)));
    assert_eq!(wrong_node, None);
    assert_eq!(wrong_profile, None);
    assert_eq!(wrong_role, None);
    assert_eq!(exited, None);
    assert_eq!(cancelled, None);
    assert_eq!(stopped, None);
    assert_eq!(old_document, None);
    assert_eq!(current_document, None);
    assert_eq!(removed, None);
}

#[cfg(test)]
#[test]
fn control_input_is_dedicated_and_checks_deadline_before_writing() {
    let sessions = super::Sessions::default();
    let fixture = sessions.insert_live_ssm_fixture("main", 0, "123456789012", "us-west-2", "i-01234567");
    let binding = sessions.bind_ssm_control("main", &fixture.identity, &CancellationToken::new()).unwrap();
    let central = sessions.get("main", &fixture.identity.id).unwrap();
    assert_eq!(central.control_input(&binding, b"frame", Instant::now(), true), Err(CLOSED.into()));

    let control = Session::dedicated(&binding);
    assert_eq!(control.document, None);
    assert!(!control.connected());
    control.flow.lock().unwrap().started = true;
    control.flow.lock().unwrap().connected = true;
    assert!(control.connected());
    assert_eq!(control.control_input(&binding, b"frame", Instant::now(), true), Err(WRITE_TIMED_OUT.into()));
    assert!(!control.connected());
}
