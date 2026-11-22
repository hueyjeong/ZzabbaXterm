use super::remote;
use crate::ssm::CancellationToken;
use crate::ssh::{ConnectionPlan, terminal::DirectLaunch};
use super::{CHUNK, CLOSED, CloseResult, Event, LocalClose, WINDOW, stage};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    io::{self, Read, Write},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::Duration,
};

const POLL: Duration = Duration::from_millis(10);
const START_TIMEOUT: Duration = Duration::from_secs(5);
const ACK_TIMEOUT: Duration = Duration::from_secs(15);
const ACK_STALLED: &str = "ack: 출력 처리 확인 시간이 초과되었습니다";

pub(super) enum Stop {
    Closed,
    Disconnected(String),
}

#[derive(Default)]
struct Flow {
    started: bool,
    attached: bool,
    attach_error: Option<String>,
    start_deadline: Option<std::time::Instant>,
    child_exited: bool,
    finished: bool,
    stop: Option<Stop>,
    sent: u64,
    acked: u64,
    ack_deadline: Option<std::time::Instant>,
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

    fn live(&self) -> Result<(), String> {
        if !self.started || self.child_exited || self.stop.is_some() {
            return Err(CLOSED.into());
        }
        Ok(())
    }
}

pub(super) enum Launch {
    Local(CommandBuilder),
    Ssm(remote::Target),
    Ssh(DirectLaunch),
}

impl Launch {
    pub(super) fn is_ssm(&self) -> bool {
        matches!(self, Self::Ssm(_))
    }
}

pub(super) struct Session {
    pub(super) owner: String,
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
    _plan: ConnectionPlan,
}

impl Session {
    pub(super) fn new(owner: &str, ssm: bool) -> Self {
        Self {
            owner: owner.into(),
            cancel: ssm.then(CancellationToken::new),
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

    pub(super) fn launch(
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
        let mut remote = match &command {
            Launch::Ssm(target) => Some(tauri::async_runtime::block_on(target.prepare(self.cancel.as_ref().unwrap()))?),
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
            let drain = thread::Builder::new().name("terminal-output".into()).spawn_scoped(scope, move || {
                if ssm {
                    drop(self.wake.wait_while(self.flow.lock().unwrap(), |flow| {
                        !flow.attached && flow.stop.is_none()
                    }).unwrap());
                }
                let end = self.drain(reader, &send_event);
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
                Launch::Ssm(_) => None,
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
                if !ssm { flow.start_deadline = Some(std::time::Instant::now() + START_TIMEOUT); }
                self.wake.notify_all();
            }
            if !ssm { let _ = ready.send(Ok(())); }

            let event = thread::scope(|scope| {
                if let Some(remote) = remote.as_ref() {
                    scope.spawn(move || {
                        let cancel = self.cancel.as_ref().unwrap();
                        let result = remote.wait_connected(cancel).and_then(|()| {
                            if !cancel.is_cancelled() { send_event(Event::Ready)?; }
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

    pub(super) fn close_result(&self) -> CloseResult {
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

    fn drain(&self, mut reader: Box<dyn Read + Send>, send: &impl Fn(Event) -> Result<(), String>) -> Result<(), String> {
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

    pub(super) fn start_until(&self, deadline: Option<std::time::Instant>) -> Result<(), String> {
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

    pub(super) fn ack(&self, position: u64) -> Result<(), String> {
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

    pub(super) fn input(&self, mut bytes: &[u8]) -> Result<(), String> {
        self.flow.lock().unwrap().live()?;
        let mut writer = self.writer.lock().unwrap();
        let writer = writer.as_mut().ok_or(CLOSED)?;
        while !bytes.is_empty() {
            self.flow.lock().unwrap().live()?;
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

    pub(super) fn stop(&self, reason: Stop) {
        if let Some(cancel) = &self.cancel { cancel.cancel(); }
        let mut flow = self.flow.lock().unwrap();
        if flow.stop.is_none() || matches!(reason, Stop::Closed) {
            flow.stop = Some(reason);
        }
        self.wake.notify_all();
    }

    fn finish(&self) {
        self.flow.lock().unwrap().finished = true;
        self.wake.notify_all();
    }

    pub(super) fn wait_finished(&self) {
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
use ssh_child::{poll as poll_ssh, terminate as terminate_ssh};
