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
        let directory = tempfile::Builder::new().prefix("zxt-ssh-")
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
        owns_stream(child.id(), listen, peer)
    }

    pub(super) fn close(&mut self) -> io::Result<()> {
        self.request.as_mut().map_or(Ok(()), stop)
    }
}

/// `pid`의 열린 파일 중 승인 수신 주소에서 받아들인 `peer` 연결 소켓이 있는지 /proc으로 확인한다.
pub(in crate::tunnels) fn owns_stream(pid: u32, listen: SocketAddr, peer: SocketAddr) -> io::Result<bool> {
    let (SocketAddr::V4(listen), SocketAddr::V4(peer)) = (listen, peer) else { return Ok(false); };
    let address = |addr: std::net::SocketAddrV4| format!("{:08X}:{:04X}", u32::from_ne_bytes(addr.ip().octets()), addr.port());
    let (local, remote) = (address(listen), address(peer));
    let root = format!("/proc/{pid}");
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

/// `pid`의 열린 파일 중 `port`의 LISTEN 소켓(IPv4·IPv6)이 있으면 그 수신 주소를 /proc으로 모은다.
pub(in crate::tunnels) fn listeners(pid: u32, port: u16) -> io::Result<Vec<SocketAddr>> {
    let root = format!("/proc/{pid}");
    let mut rows = Vec::new();
    for (table, v6) in [("tcp", false), ("tcp6", true)] {
        let Ok(file) = fs::File::open(format!("{root}/net/{table}")) else { continue };
        for row in BufReader::new(file).lines().skip(1) {
            let row = row?;
            let mut fields = row.split_ascii_whitespace().skip(1);
            let (Some(local), Some(_), Some("0A")) = (fields.next(), fields.next(), fields.next()) else { continue };
            let Some(inode) = fields.nth(5).filter(|inode| *inode != "0") else { continue };
            let Some((address, hex_port)) = local.split_once(':') else { continue };
            if u16::from_str_radix(hex_port, 16).ok() != Some(port) { continue; }
            let Some(ip) = proc_address(address, v6) else { continue };
            rows.push((format!("socket:[{inode}]"), SocketAddr::new(ip, port)));
        }
    }
    let mut found = Vec::new();
    if rows.is_empty() { return Ok(found); }
    for fd in fs::read_dir(format!("{root}/fd"))? {
        let Ok(link) = fs::read_link(fd?.path()) else { continue };
        if let Some((_, address)) = rows.iter().find(|(socket, _)| link.as_os_str() == std::ffi::OsStr::new(socket)) {
            found.push(*address);
        }
    }
    Ok(found)
}

// /proc/net/tcp*는 네트워크 순서 32비트 단어를 호스트 순서 16진수로 적는다.
fn proc_address(hex: &str, v6: bool) -> Option<std::net::IpAddr> {
    let words = if v6 { 4 } else { 1 };
    if hex.len() != words * 8 { return None; }
    let mut octets = Vec::with_capacity(words * 4);
    for index in 0..words {
        octets.extend(u32::from_str_radix(&hex[index * 8..index * 8 + 8], 16).ok()?.to_ne_bytes());
    }
    Some(if v6 { <[u8; 16]>::try_from(octets).ok()?.into() } else { <[u8; 4]>::try_from(octets).ok()?.into() })
}
