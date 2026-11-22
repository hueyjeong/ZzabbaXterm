use portable_pty::{Child, ExitStatus};
use std::{io, thread, time::{Duration, Instant}};

const POLL: Duration = Duration::from_millis(10);
const REAP_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(unix)]
pub(super) fn poll(child: &mut dyn Child) -> io::Result<Option<ExitStatus>> {
    child.try_wait()
}

#[cfg(windows)]
pub(super) fn ready(child: &dyn Child) -> io::Result<bool> {
    use windows_sys::Win32::{Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT}, System::Threading::WaitForSingleObject};
    let handle = child.as_raw_handle().ok_or(io::ErrorKind::InvalidInput)?;
    // 종료 코드 259도 유효하므로, 먼저 같은 프로세스 핸들의 신호를 확인한다.
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(windows)]
pub(super) fn poll(child: &mut dyn Child) -> io::Result<Option<ExitStatus>> {
    use windows_sys::Win32::System::Threading::GetExitCodeProcess;
    if !ready(child)? { return Ok(None); }
    let handle = child.as_raw_handle().ok_or(io::ErrorKind::InvalidInput)?;
    let mut code = 0;
    if unsafe { GetExitCodeProcess(handle, &mut code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(ExitStatus::with_exit_code(code)))
}

#[cfg(unix)]
fn kill(child: &dyn Child) -> io::Result<()> {
    let pid = child.process_id().and_then(|pid| libc::pid_t::try_from(pid).ok())
        .filter(|&pid| pid > 0).ok_or(io::ErrorKind::InvalidInput)?;
    // 정상 대기가 미종료를 반환한 자식은 감독자만 회수하므로 PID가 재사용될 수 없다.
    if unsafe { libc::kill(pid, libc::SIGKILL) } == -1 { return Err(io::Error::last_os_error()); }
    Ok(())
}

#[cfg(windows)]
fn kill(child: &dyn Child) -> io::Result<()> {
    use windows_sys::Win32::System::Threading::TerminateProcess;
    let handle = child.as_raw_handle().ok_or(io::ErrorKind::InvalidInput)?;
    // portable-pty 0.9의 WinChild::kill은 TerminateProcess 오류를 버린다.
    if unsafe { TerminateProcess(handle, 1) } == 0 { return Err(io::Error::last_os_error()); }
    Ok(())
}

pub(super) fn terminate(child: &mut dyn Child) -> Result<ExitStatus, String> {
    if let Some(status) = poll(child).map_err(|error| format!("wait: {error}"))? { return Ok(status); }
    if let Err(kill) = kill(child) {
        return match poll(child) {
            Ok(Some(status)) => Ok(status),
            Ok(None) => Err(format!("kill: {kill}")),
            Err(wait) => Err(format!("kill: {kill}; wait: {wait}")),
        };
    }
    wait_until(child, Instant::now() + REAP_TIMEOUT)
}

fn wait_until(child: &mut dyn Child, deadline: Instant) -> Result<ExitStatus, String> {
    loop {
        if let Some(status) = poll(child).map_err(|error| format!("wait: {error}"))? { return Ok(status); }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() { return Err("wait: 시간이 초과되었습니다".into()); }
        thread::sleep(POLL.min(remaining));
    }
}

#[cfg(test)]
pub(super) mod tests;
