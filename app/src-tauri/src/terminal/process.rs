use super::{CHUNK, CLOSED, Event, WINDOW, stage};
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

pub(super) struct Session {
    pub(super) owner: String,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    flow: Mutex<Flow>,
    wake: Condvar,
}

impl Session {
    pub(super) fn new(owner: &str) -> Self {
        Self {
            owner: owner.into(),
            master: Mutex::new(None),
            writer: Mutex::new(None),
            flow: Mutex::new(Flow::default()),
            wake: Condvar::new(),
        }
    }

    pub(super) fn launch(
        self: &Arc<Self>,
        size: PtySize,
        command: CommandBuilder,
        send: impl Fn(Event) -> Result<(), String> + Send + 'static,
        retired: impl FnOnce() + Send + 'static,
    ) -> Result<(), String> {
        self.begin_launch(size, command, send, retired)?.recv()
            .map_err(|error| stage("open handshake", error))?
    }

    pub(super) fn begin_launch(
        self: &Arc<Self>,
        size: PtySize,
        command: CommandBuilder,
        send: impl Fn(Event) -> Result<(), String> + Send + 'static,
        retired: impl FnOnce() + Send + 'static,
    ) -> Result<mpsc::Receiver<Result<(), String>>, String> {
        let this = Arc::clone(self);
        let (ready, opened) = mpsc::sync_channel(1);
        if let Err(error) = thread::Builder::new().name("terminal-pty".into()).spawn(move || {
            let result = this.run(size, command, send, &ready);
            if let Err(error) = result {
                let _ = ready.send(Err(error));
            }
            retired();
            this.finish();
        }) {
            self.finish();
            return Err(stage("PTY worker", error));
        }
        Ok(opened)
    }

    fn run(
        &self,
        size: PtySize,
        command: CommandBuilder,
        send: impl Fn(Event) -> Result<(), String> + Send + 'static,
        ready: &mpsc::SyncSender<Result<(), String>>,
    ) -> Result<(), String> {
        let pair = native_pty_system().openpty(size).map_err(|e| stage("openpty", e))?;
        #[cfg(unix)]
        set_nonblocking(pair.master.as_ref()).map_err(|e| stage("nonblocking", e))?;
        let reader = pair.master.try_clone_reader().map_err(|e| stage("clone reader", e))?;
        let writer = pair.master.take_writer().map_err(|e| stage("take writer", e))?;
        thread::scope(|scope| {
            let (event_tx, event_rx) = mpsc::sync_channel(1);
            let drain = thread::Builder::new().name("terminal-output".into()).spawn_scoped(scope, move || {
                let end = self.drain(reader, &send);
                if let Err(error) = end {
                    self.stop(Stop::Disconnected(error));
                }
                // Only this worker sends, including the final status, so channel order is stable.
                if let Ok(event) = event_rx.recv() {
                    let event = match self.flow.lock().unwrap().stop.as_ref() {
                        Some(Stop::Closed) => None,
                        Some(Stop::Disconnected(error)) => Some(Event::Disconnected(error.clone())),
                        None => event,
                    };
                    if let Some(event) = event {
                        let _ = send(event);
                    }
                }
            }).map_err(|e| stage("reader worker", e))?;

            if self.flow.lock().unwrap().stop.is_some() {
                self.teardown(pair, writer, event_tx, drain);
                return Err(stage("open", CLOSED));
            }
            let mut child = match pair.slave.spawn_command(command) {
                Ok(child) => child,
                Err(error) => {
                    self.teardown(pair, writer, event_tx, drain);
                    return Err(stage("spawn", format_args!("{error:#}")));
                }
            };
            drop(pair.slave);
            *self.writer.lock().unwrap() = Some(writer);
            *self.master.lock().unwrap() = Some(pair.master);
            self.flow.lock().unwrap().start_deadline = Some(std::time::Instant::now() + START_TIMEOUT);
            let _ = ready.send(Ok(()));

            let event = self.reap(child.as_mut());
            let master = self.master.lock().unwrap().take();
            #[cfg(windows)]
            if let Ok(mut writer) = self.writer.try_lock() {
                writer.take();
            }
            // ConPTY closes with the drain worker still available. A stop bypasses ACK credit
            // and drains/discards until the console releases its output pipe.
            drop(master);
            self.writer.lock().unwrap().take();
            let _ = event_tx.send(event);
            let _ = drain.join();
            Ok(())
        })
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

    fn reap(&self, child: &mut dyn Child) -> Option<Event> {
        loop {
            let mut flow = self.flow.lock().unwrap();
            flow.expire_start();
            flow.expire_ack();
            if flow.stop.is_some() {
                self.wake.notify_all();
                drop(flow);
                self.terminate(child);
                return None;
            }
            if let Some(deadline) = flow.start_deadline {
                // Keep the ConPTY reader ready to drain on start or timeout.
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                drop(self.wake.wait_timeout(flow, remaining).unwrap());
                continue;
            }
            match child_ready(child) {
                Ok(true) => {
                    flow.child_exited = true;
                    self.wake.notify_all();
                    drop(flow);
                    return Some(match child.wait() {
                        Ok(status) => Event::Exited(status.exit_code()),
                        Err(error) => Event::Disconnected(stage("wait", error)),
                    });
                }
                Ok(false) => drop(self.wake.wait_timeout(flow, POLL).unwrap()),
                Err(error) => {
                    flow.child_exited = true;
                    drop(flow);
                    self.stop(Stop::Disconnected(stage("wait", error)));
                    self.terminate(child);
                    return None;
                }
            }
        }
    }

    fn terminate(&self, child: &mut dyn Child) {
        #[cfg(unix)]
        if let Some(pid) = child.process_id() {
            let master = self.master.lock().unwrap();
            if let Some(master) = master.as_deref() {
                kill_foreground(master, pid as libc::pid_t);
            }
            // Only the supervisor reaps the child, so its PID cannot yet be recycled.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
        #[cfg(windows)]
        {
            let _ = child.kill();
        }
        let _ = child.wait();
    }

    fn drain(&self, mut reader: Box<dyn Read + Send>, send: &super::SendEvent) -> Result<(), String> {
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
        let mut flow = self.flow.lock().unwrap();
        flow.expire_start();
        if flow.stop.is_some() || flow.finished || (!flow.started && flow.start_deadline.is_none()) {
            self.wake.notify_all();
            return Err(CLOSED.into());
        }
        flow.started = true;
        flow.start_deadline = None;
        self.wake.notify_all();
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
    use windows_sys::Win32::{Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT}, System::Threading::WaitForSingleObject};
    let handle = child.as_raw_handle().ok_or(io::ErrorKind::InvalidInput)?;
    // portable-pty 0.9 treats the valid exit code 259 as STILL_ACTIVE in try_wait.
    // A signaled process handle distinguishes it from a running process.
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
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
