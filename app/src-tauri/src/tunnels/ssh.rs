use std::{
    io,
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
    sync::Arc,
};

use crate::{
    connections::{profile::{Ssh, SshAuth, SshTransport}, with_saved_ssh},
    ssh::{Client, ConnectionPlan, terminal},
};

use super::{Observation, Owner, Route, Transport, TunnelId, Tunnels, transport::{Event, Failure, Listener, SshPlan, SshServerPolicy}};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(unix)]
use unix::Readiness;
#[cfg(windows)]
use windows::Readiness;
#[cfg(test)]
mod tests;

pub(super) type SavedSsh = (std::num::NonZeroU64, Ssh);

pub(crate) struct Start {
    id: TunnelId,
    connection: Arc<ConnectionPlan>,
    plan: SshPlan,
}

impl Tunnels<SshChild> {
    /// 호출자가 같은 창·탭의 승인 레코드와 서버 정책을 확정한 후 전달한다.
    /// `approved.server`는 저장 대상 ID이고 `approved_ssh`는 승인 당시의 리비전과 SSH 설정이다.
    pub(crate) fn begin_ssh(
        &mut self,
        owner: Owner,
        directory: &Path,
        route: Route,
        approved: &Route,
        approved_ssh: &SavedSsh,
        policy: SshServerPolicy,
    ) -> Result<Start, String> {
        if !self.is_open(&owner) {
            return Err("터널 탭이 닫혔습니다".into());
        }
        let plan = SshPlan::new(&route, approved, policy).map_err(|error| format!("SSH 전달 승인: {error:?}"))?;
        require_supported_listener(plan.listener())?;
        let server = match approved {
            Route::SshLocal { server, .. } | Route::SshRemote { server, .. } | Route::SshDynamic { server, .. } => server,
            #[cfg(test)]
            Route::Ssm { .. } => unreachable!("SSH 전달 계획 필요"),
        };
        let connection = with_saved_ssh(directory, server, |current| -> Result<_, String> {
            let (revision, ssh) = current.ok_or("저장된 SSH 대상이 필요합니다")?;
            if revision != approved_ssh.0 || ssh != approved_ssh.1 {
                return Err("승인된 SSH 접속 설정이 변경되었습니다".into());
            }
            if !matches!(ssh.transport, SshTransport::Direct {}) {
                return Err("SSH-over-SSM 전달을 사용할 수 없습니다".into());
            }
            if !matches!(ssh.auth, SshAuth::Agent { .. } | SshAuth::KeyFile { .. }) {
                return Err("SSH 전달에는 Agent 또는 비대화형 키 인증이 필요합니다".into());
            }
            let (_, connection) = terminal::saved(directory, &ssh)?.into_parts();
            Ok(connection)
        })??;
        let id = self.begin(owner, route).ok_or("터널 탭이 닫혔습니다")?;
        Ok(Start { id, connection, plan })
    }
}

pub(super) fn require_supported_listener(listener: Listener) -> Result<(), String> {
    if !cfg!(target_os = "linux") && let Listener::Remote(address) = listener {
        let platform = if cfg!(windows) { "Windows" } else { std::env::consts::OS };
        return Err(format!("{platform} OpenSSH에서 원격 수신 확인을 사용할 수 없습니다: {address}"));
    }
    Ok(())
}

impl Start {
    /// 호출자는 상태 등록 또는 늦은 결과의 회수까지 책임진다.
    pub(crate) fn spawn(self) -> (TunnelId, Result<SshChild, String>) {
        (self.id, SshChild::spawn(self.connection, self.plan))
    }
}

pub(crate) struct SshChild {
    child: Child,
    readiness: Readiness,
    plan: SshPlan,
    deadline: Instant,
    ready: bool,
    _connection: Arc<ConnectionPlan>,
}

impl SshChild {
    fn spawn(connection: Arc<ConnectionPlan>, plan: SshPlan) -> Result<Self, String> {
        let call = connection.invocation(Client::Ssh).map_err(|error| format!("SSH 전달 준비: {error:?}"))?;
        let mut command = Command::new(call.program);
        command.args(&call.args[..2]);
        command.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
        for &(key, value) in call.environment {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        command.env_remove("BW_SESSION");
        silent(&mut command);
        let readiness = Readiness::configure(&mut command, call.program, &plan)?;
        if local_conflict(plan.listener()) {
            return Err(end(plan.observe(Event::Ended(Failure::BindConflict))));
        }
        let child = command.args(&call.args[2..]).spawn().map_err(|error| format!("SSH 전달 시작: {error}"))?;
        Ok(Self { child, readiness, plan, deadline: Instant::now() + Duration::from_secs(15), ready: false, _connection: connection })
    }

    fn poll(&mut self) -> Result<bool, String> {
        if let Some(status) = self.child.try_wait().map_err(|error| format!("SSH 자식 상태: {error}"))? {
            if self.ready {
                return Err(end(self.plan.observe(Event::Ended(Failure::OwnedProcessExited))));
            }
            return Err(if local_conflict(self.plan.listener()) {
                end(self.plan.observe(Event::Ended(Failure::BindConflict)))
            } else {
                format!("SSH 전달 시작 실패: {status}")
            });
        }
        if self.ready {
            return Ok(true);
        }
        if Instant::now() >= self.deadline {
            return Err("SSH 전달 확인 시간 초과".into());
        }
        if !self.readiness.poll(&self.child, &self.plan)? {
            return Ok(false);
        }
        self.ready = self.child.try_wait().map_err(|error| format!("SSH 자식 상태: {error}"))?.is_none();
        Ok(self.ready)
    }

    pub(super) fn open_stream(&mut self, address: SocketAddr) -> Result<TcpStream, String> {
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(1))
            .map_err(|error| format!("SSH 터널 스트림 연결: {error}"))?;
        let client = stream.local_addr().map_err(|error| error.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if self.child.try_wait().map_err(|error| error.to_string())?.is_some() { break; }
            if self.readiness.owns_stream(&self.child, address, client).map_err(|error| format!("SSH 스트림 소유권: {error}"))?
                && self.child.try_wait().map_err(|error| error.to_string())?.is_none()
            {
                return Ok(stream);
            }
            if Instant::now() >= deadline { break; }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err("SSH 자식이 승인된 터널 스트림을 소유하지 않습니다".into())
    }
}

impl Transport for SshChild {
    fn observe(&mut self) -> Observation {
        match self.poll() {
            Ok(true) => self.plan.observe(match self.plan.listener() {
                Listener::Local(address) => Event::SshLocalBound(address),
                Listener::Remote(address) => Event::SshRemoteAccepted(address),
            }),
            Ok(false) => Observation::Pending,
            Err(error) => Observation::Ended(error),
        }
    }

    fn close(&mut self) -> io::Result<()> {
        let child = stop(&mut self.child);
        child.and(self.readiness.close())
    }
}

impl Drop for SshChild {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn silent(command: &mut Command) {
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
}

fn stop(child: &mut Child) -> io::Result<()> {
    if child.try_wait()?.is_some() { return Ok(()); }
    if let Err(error) = child.kill() {
        if child.try_wait()?.is_some() { return Ok(()); }
        return Err(error);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline { return Err(io::ErrorKind::TimedOut.into()); }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn local_conflict(listener: Listener) -> bool {
    let Listener::Local(address) = listener else { return false };
    TcpListener::bind(address).is_err_and(|error| error.kind() == io::ErrorKind::AddrInUse)
}

fn end(observation: Observation) -> String {
    let Observation::Ended(reason) = observation else { unreachable!("종료 사유 필요") };
    reason
}
