use std::{fs::{self, Permissions}, io::{self, BufRead, BufReader}, net::SocketAddr, os::unix::fs::{FileTypeExt, PermissionsExt}, path::{Path, PathBuf}, process::{Child, Command}};

use super::{Event, Failure, Listener, SshPlan, end, local_conflict, silent, stop};

const CONTROL: &str = "control";

pub(super) struct Readiness {
    directory: tempfile::TempDir,
    program: PathBuf,
    request: Option<Child>,
}

impl Readiness {
    pub(super) fn configure(command: &mut Command, program: &Path, plan: &SshPlan) -> Result<Self, String> {
        let directory = tempfile::Builder::new().prefix("zzabba-ssh-")
            .permissions(Permissions::from_mode(0o700)).tempdir_in("/tmp")
            .map_err(|error| format!("SSH 전달 제어 경로: {error}"))?;
        let options = plan.approved_options();
        command.current_dir(directory.path()).args(&options[..10])
            .args(["-o", "ControlPersist=no", "-M", "-S", CONTROL]);
        Ok(Self { directory, program: program.into(), request: None })
    }

    pub(super) fn poll(&mut self, _: &Child, plan: &SshPlan) -> Result<bool, String> {
        if let Some(request) = &mut self.request {
            let status = request.try_wait().map_err(|error| format!("SSH 전달 요청 상태: {error}"))?;
            return match status {
                None => Ok(false),
                Some(status) if status.success() => Ok(true),
                Some(_) if local_conflict(plan.listener()) => Err(end(plan.observe(Event::Ended(Failure::BindConflict)))),
                Some(_) => Err(match plan.listener() {
                    Listener::Local(address) => format!("로컬 수신 설정 실패: {address}"),
                    Listener::Remote(address) => format!("원격 수신 설정 실패: {address} (포트 충돌 또는 서버 거부)"),
                }),
            };
        }
        let socket = self.directory.path().join(CONTROL);
        if !socket.symlink_metadata().is_ok_and(|metadata| metadata.file_type().is_socket()) {
            return Ok(false);
        }
        let options = plan.approved_options();
        let mut command = Command::new(&self.program);
        silent(&mut command);
        command.current_dir(self.directory.path()).args([
            "-F", "none", "-S", CONTROL, "-O", "forward", options[10], options[11],
            "-o", "GatewayPorts=no", "-o", "ExitOnForwardFailure=yes",
            // mux 협상 실패 시에도 다른 SSH 연결을 시작할 수 없는 주소 패밀리 조합이다.
            "-6", "--", "127.0.0.1",
        ]);
        self.request = Some(command.spawn().map_err(|error| format!("SSH 전달 요청: {error}"))?);
        Ok(false)
    }

    pub(super) fn owns_stream(&self, child: &Child, listen: SocketAddr, peer: SocketAddr) -> io::Result<bool> {
        let (SocketAddr::V4(listen), SocketAddr::V4(peer)) = (listen, peer) else { return Ok(false); };
        let address = |addr: std::net::SocketAddrV4| format!("{:08X}:{:04X}", u32::from_ne_bytes(addr.ip().octets()), addr.port());
        let (local, remote) = (address(listen), address(peer));
        let root = format!("/proc/{}", child.id());
        for row in BufReader::new(fs::File::open(format!("{root}/net/tcp"))?).lines().skip(1) {
            let row = row?;
            let mut fields = row.split_ascii_whitespace().skip(1);
            if fields.next() != Some(local.as_str()) || fields.next() != Some(remote.as_str()) || fields.next() != Some("01") { continue; }
            let Some(inode) = fields.nth(5).filter(|inode| *inode != "0") else { continue; };
            let socket = format!("socket:[{inode}]");
            for fd in fs::read_dir(format!("{root}/fd"))? {
                if fs::read_link(fd?.path()).is_ok_and(|path| path.as_os_str() == std::ffi::OsStr::new(&socket)) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub(super) fn close(&mut self) -> io::Result<()> {
        self.request.as_mut().map_or(Ok(()), stop)
    }
}
