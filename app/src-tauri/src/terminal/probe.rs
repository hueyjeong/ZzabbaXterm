use crate::connections::profile::{MethodRef, Shell, ValidatedDraft};
use portable_pty::CommandBuilder;
use std::{
    sync::{Arc, Condvar, Mutex, mpsc},
    time::{Duration, Instant},
};

use super::{Event, process::{Launch, Session, Stop}, remote, size};

const MARKER: &[u8] = b"ZZABBA-C01:314159";
const SSM_MARKER: &[u8] = b"ZZABBA-C02:314159";
// 표식의 완성 문자열은 PTY가 되비추는 입력에 없다.
const SSM_CHECK: &[u8] = b"[ \"$(uname -s)\" = Linux ] || exit 19; printf 'ZZABBA-C02:%d\\n' \"$((314000+159))\" && exit 0; exit 19\r";
const TIMEOUT: Duration = Duration::from_secs(8);
const SSM_TIMEOUT: Duration = Duration::from_secs(60);
pub(super) const TIMED_OUT: &str = "연결 시험 시간이 초과되었습니다";

fn scan(needle: &[u8], matched: &mut usize, bytes: &[u8]) -> bool {
    for &byte in bytes {
        if *matched == needle.len() { break; }
        if byte == needle[*matched] {
            *matched += 1;
        } else {
            *matched = usize::from(byte == needle[0]);
        }
    }
    *matched == needle.len()
}

pub(crate) struct Probe {
    session: Arc<Session>,
    deadline: Instant,
    state: Mutex<State>,
    ready: Condvar,
}

#[derive(Default)]
struct State {
    result: Option<Result<(), String>>,
    stopped: bool,
}

impl Probe {
    pub(crate) fn for_draft(owner: &str, draft: &ValidatedDraft) -> Self {
        let ssm = matches!(draft.method(), MethodRef::Ssm { .. });
        Self::with_timeout(owner, if ssm { SSM_TIMEOUT } else { TIMEOUT }, ssm)
    }

    #[cfg(test)]
    pub(crate) fn new(owner: &str) -> Self {
        Self::with_timeout(owner, TIMEOUT, false)
    }

    fn with_timeout(owner: &str, timeout: Duration, ssm: bool) -> Self {
        Self { session: Arc::new(Session::new(owner, ssm)), deadline: Instant::now() + timeout, state: Mutex::default(), ready: Condvar::new() }
    }

    pub(crate) fn run(&self, draft: &ValidatedDraft) {
        let launch = match draft.method() {
            MethodRef::Local { shell, start_directory } => command(shell, start_directory).map(Launch::Local),
            MethodRef::Ssm { profile, region, instance_id } => remote::Target::new(profile, region, instance_id).map(Launch::Ssm),
        };
        self.finish(launch.and_then(|launch| self.measure(launch)));
    }

    fn finish(&self, result: Result<(), String>) {
        self.state.lock().unwrap().result = Some(if result.is_ok() && Instant::now() >= self.deadline { Err(TIMED_OUT.into()) } else { result });
        self.ready.notify_all();
    }

    pub(crate) fn finished(&self) -> bool {
        self.state.lock().unwrap().result.is_some()
    }

    pub(crate) fn wait(&self) -> Result<(), String> {
        let (state, _) = self.ready.wait_timeout_while(self.state.lock().unwrap(), self.remaining(), |state| {
            state.result.is_none() && !state.stopped
        }).unwrap();
        if self.session.is_ssm() && (state.stopped || state.result.is_none()) {
            let reason = if state.stopped { "연결 시험이 취소되었습니다" } else { TIMED_OUT };
            drop(state);
            self.session.stop(Stop::Closed);
            return self.after_cleanup(Err(reason.into()));
        }
        if state.stopped {
            return Err("연결 시험이 취소되었습니다".into());
        }
        state.result.clone().unwrap_or_else(|| Err(TIMED_OUT.into()))
    }

    pub(crate) fn wait_finished(&self) {
        drop(self.ready.wait_while(self.state.lock().unwrap(), |state| state.result.is_none()).unwrap());
    }

    pub(crate) fn cancel_result(&self) -> Result<(), String> {
        if self.session.is_ssm() {
            self.wait_finished();
            self.session.cleanup_result()?;
        }
        Ok(())
    }

    fn after_cleanup(&self, result: Result<(), String>) -> Result<(), String> {
        self.wait_finished();
        combine_cleanup(result, self.session.cleanup_result())
    }

    pub(crate) fn stop(&self) {
        self.session.stop(Stop::Closed);
        self.state.lock().unwrap().stopped = true;
        self.ready.notify_all();
    }

    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    fn measure(&self, command: Launch) -> Result<(), String> {
        if self.remaining().is_zero() {
            return Err(TIMED_OUT.into());
        }
        let ssm = command.is_ssm();
        let marker = if ssm { SSM_MARKER } else { MARKER };
        let (sender, receiver) = mpsc::sync_channel(1);
        let session = Arc::downgrade(&self.session);
        let output = std::cell::Cell::new((0u64, 0usize));
        #[cfg(windows)]
        let cursor_query = std::cell::Cell::new(0usize);
        let launched = self.session.begin_launch(size(24, 80), command, move |event| {
            let (position, mut matched) = output.get();
            match event {
                Event::Bytes(bytes) => {
                    scan(marker, &mut matched, &bytes);
                    let position = position + bytes.len() as u64;
                    output.set((position, matched));
                    let session = session.upgrade().ok_or(super::CLOSED)?;
                    session.ack(position)?;
                    #[cfg(windows)]
                    {
                        // L01에서는 xterm이 이 요청에 응답하지만 시험에는 화면이 없다.
                        const QUERY: &[u8] = b"\x1b[6n";
                        let mut matched = cursor_query.get();
                        if matched < QUERY.len() && scan(QUERY, &mut matched, &bytes) {
                            session.input(b"\x1b[1;1R")?;
                        }
                        cursor_query.set(matched);
                    }
                    Ok(())
                }
                Event::Exited(0) if matched == marker.len() => sender.send(Ok(())).map_err(|err| err.to_string()),
                Event::Exited(23) if !ssm => sender.send(Err("셸이 요청한 작업 디렉터리에서 시작하지 못했습니다".into()))
                    .map_err(|err| err.to_string()),
                Event::Exited(code) => sender.send(Err(format!("셸 시험 출력 또는 종료 상태가 올바르지 않습니다 (종료 코드 {code})")))
                    .map_err(|err| err.to_string()),
                Event::Disconnected(error) => sender.send(Err(error)).map_err(|err| err.to_string()),
            }
        }, || {});
        let result = launched.and_then(|opened| {
            self.receive(&opened)??;
            self.session.start_until(Some(self.deadline))?;
            if ssm { self.session.input(SSM_CHECK)?; }
            self.receive(&receiver)?
        });
        if result.is_err() {
            self.session.stop(Stop::Closed);
        }
        self.session.wait_finished();
        combine_cleanup(result, self.session.cleanup_result())
    }

    fn receive<T>(&self, receiver: &mpsc::Receiver<T>) -> Result<T, String> {
        let remaining = self.remaining();
        if remaining.is_zero() {
            return Err(TIMED_OUT.into());
        }
        receiver.recv_timeout(remaining).map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => TIMED_OUT.into(),
            mpsc::RecvTimeoutError::Disconnected => "연결 시험이 취소되었습니다".into(),
        })
    }
}

fn combine_cleanup(result: Result<(), String>, cleanup: Result<(), String>) -> Result<(), String> {
    match (result, cleanup) {
        (Err(error), Err(cleanup)) if !error.contains(&cleanup) => Err(format!("{error}; {cleanup}")),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (result, _) => result,
    }
}

fn command(shell: Shell, directory: &std::path::Path) -> Result<CommandBuilder, String> {
    super::validate_working_directory(directory)?;
    let mut command = match shell {
        #[cfg(windows)]
        Shell::PowerShell => {
            let path = std::env::var_os("PATH").ok_or("PowerShell: PATH가 없습니다")?;
            let executable = std::env::split_paths(&path).filter(|path| path.is_absolute())
                .map(|path| path.join("pwsh.exe")).find(|path| path.is_file())
                .ok_or("PowerShell: pwsh.exe를 찾을 수 없습니다")?;
            let mut command = CommandBuilder::new(executable);
            command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"]);
            command.arg(format!(
                "$directory = $env:ZZABBA_C01_CWD; if (-not ({})) {{ exit 23 }}; [Console]::WriteLine(('ZZABBA-C01:{{0}}' -f (314000+159))); exit 0",
                super::POWERSHELL_CWD_MATCH,
            ));
            command
        }
        #[cfg(unix)]
        Shell::Bash => {
            let mut command = CommandBuilder::new("/bin/bash");
            command.env_remove("BASH_ENV");
            command.args(["--noprofile", "--norc", "-c",
                "[[ . -ef $ZZABBA_C01_CWD ]] || exit 23; builtin printf 'ZZABBA-C01:%d\\n' \"$((314000+159))\""]);
            command
        }
        #[allow(unreachable_patterns)]
        _ => return Err("지원하지 않는 셸입니다".into()),
    };
    command.cwd(directory);
    command.env("ZZABBA_C01_CWD", directory);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c01_probe_times_out_and_reaps_a_shell_that_does_not_exit() {
        let mut command = super::super::shell();
        #[cfg(windows)]
        command.args(["-NonInteractive", "-Command", "[Console]::WriteLine('ZZABBA-C01:314159'); Start-Sleep 60"]);
        #[cfg(unix)]
        command.args(["-c", "builtin printf 'ZZABBA-C01:314159\\n'; read -rt 60"]);
        let probe = Probe::with_timeout("timeout", Duration::from_secs(1), false);
        assert_eq!(probe.measure(Launch::Local(command)), Err(TIMED_OUT.into()));
    }

    #[test]
    fn c01_probe_counts_time_before_the_open_handshake() {
        let probe = Probe::with_timeout("startup", Duration::from_millis(50), false);
        let (ready, opened) = mpsc::sync_channel(1);
        assert_eq!(probe.receive(&opened), Err(TIMED_OUT.into()));
        ready.send(()).unwrap();
        assert_eq!(probe.receive(&opened), Err(TIMED_OUT.into()));
    }
}
