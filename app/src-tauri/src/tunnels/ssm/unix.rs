use std::{
    io, net::SocketAddr,
    os::unix::process::CommandExt,
    process::{Child, Command},
    time::{Duration, Instant},
};

const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// aws CLI를 새 프로세스 그룹의 리더로 실행해 session-manager-plugin까지 같은 그룹에 둔다.
/// 리더를 회수하기 전에만 그룹 신호를 보내 다른 프로그램이 같은 번호를 재사용하는 경우를 피한다.
pub(super) struct Tree {
    group: Option<libc::pid_t>,
    reaped: bool,
}

pub(super) fn configure(command: &mut Command) {
    command.process_group(0);
}

impl Tree {
    pub(super) fn new() -> io::Result<Self> {
        Ok(Self { group: None, reaped: false })
    }

    pub(super) fn adopt(&mut self, child: &Child) -> io::Result<()> {
        self.group = Some(libc::pid_t::try_from(child.id()).map_err(|_| io::ErrorKind::InvalidData)?);
        Ok(())
    }

    /// 리더를 회수하지 않고(`WNOWAIT`) 종료 여부만 읽는다.
    pub(super) fn exited(&self, child: &mut Child) -> io::Result<Option<String>> {
        if self.reaped {
            return Ok(Some("회수됨".into()));
        }
        // SAFETY: 0으로 초기화한 siginfo_t를 출력 버퍼로 넘기고, 반환 값을 확인한 뒤에만 읽는다.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let pid = libc::id_t::try_from(child.id()).map_err(|_| io::ErrorKind::InvalidData)?;
        if unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) } != 0 {
            return Err(io::Error::last_os_error());
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let (changed, status) = unsafe { (info.si_pid(), info.si_status()) };
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let (changed, status) = (info.si_pid, info.si_status);
        if changed == 0 {
            return Ok(None);
        }
        Ok(Some(if info.si_code == libc::CLD_EXITED { format!("종료 코드 {status}") } else { format!("신호 {status}") }))
    }

    pub(super) fn stop(&mut self, child: &mut Child) -> io::Result<()> {
        let Some(group) = self.group else { return Ok(()) };
        if !self.reaped {
            // SAFETY: 아직 회수하지 않은 리더가 그룹 번호를 붙잡고 있다.
            if unsafe { libc::killpg(group, libc::SIGKILL) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) { return Err(error); }
            }
        }
        let deadline = Instant::now() + STOP_TIMEOUT;
        while !self.reaped {
            if child.try_wait()?.is_some() {
                self.reaped = true;
                break;
            }
            if Instant::now() >= deadline { return Err(io::ErrorKind::TimedOut.into()); }
            std::thread::sleep(Duration::from_millis(10));
        }
        // 신호 0은 그룹의 존재만 확인한다. 남은 구성원이 없을 때 정리 완료로 본다.
        loop {
            // SAFETY: 신호를 보내지 않는 존재 확인이다.
            if unsafe { libc::killpg(group, 0) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) { return Ok(()); }
                return Err(error);
            }
            if Instant::now() >= deadline { return Err(io::ErrorKind::TimedOut.into()); }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn owns_stream(&self, child: &Child, listen: SocketAddr, peer: SocketAddr) -> io::Result<bool> {
        // 확인 중 끝난 자손의 /proc 읽기 실패는 소유하지 않은 것으로 본다.
        Ok(descendants(child.id())?.into_iter()
            .any(|pid| crate::tunnels::ssh::unix::owns_stream(pid, listen, peer).unwrap_or(false)))
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn owns_stream(&self, _: &Child, _: SocketAddr, _: SocketAddr) -> io::Result<bool> {
        Ok(false)
    }
}

/// `/proc/<pid>/task/*/children`으로 리더와 그 자손을 모은다. 자손 수는 CLI와 플러그인 몇 개로 제한된다.
#[cfg(target_os = "linux")]
fn descendants(root: u32) -> io::Result<Vec<u32>> {
    let mut found = vec![root];
    let mut next = 0;
    while next < found.len() && found.len() < 64 {
        let pid = found[next];
        next += 1;
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else { continue };
        for task in tasks.flatten() {
            let Ok(children) = std::fs::read_to_string(task.path().join("children")) else { continue };
            found.extend(children.split_ascii_whitespace().filter_map(|id| id.parse::<u32>().ok()));
        }
    }
    Ok(found)
}
