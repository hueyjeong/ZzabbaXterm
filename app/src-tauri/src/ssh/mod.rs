mod public_key_file;
pub(crate) mod nano;
pub(crate) mod registration;
pub(crate) mod terminal;
pub(crate) mod host_trust;
mod trust;
#[cfg(windows)]
mod trust_windows;

#[cfg(test)]
pub(crate) mod tests;

use std::{
    ffi::OsStr,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

pub(crate) use trust::{ApprovedHost, Endpoint, TrustStore};

pub(crate) const TRUST_DIRECTORY: &str = "ssh-trust";

#[derive(Debug)]
pub(crate) enum Error {
    InvalidEndpoint,
    ApprovedEndpointMismatch,
    ApprovedAccountMismatch,
    InvalidAccount,
    InvalidPath,
    TrustRootChanged,
    UnprotectedTrustAcl,
    InvalidPublicKey,
    InvalidFingerprint,
    FingerprintMismatch,
    UnregisteredHost,
    ExistingPinConflict,
    InvalidTrustRecord,
    StoredAuthenticationUnavailable,
    StoredSftpAskpassRequired,
    Io(io::Error),
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(crate) enum Transport {
    Direct,
    /// 앱이 소유한 SSM 포트 전달의 루프백 포트로 접속한다. 전달의 수명은 접속 계획에 묶인다.
    Ssm { port: u16, forward: Forward },
}

pub(crate) type Forward = Box<dyn crate::tunnels::Transport + Send>;

pub(crate) enum Authentication<'a> {
    Agent { public_key: &'a str },
    AgentFile { public_key_path: &'a Path },
    KeyFile { private_key: &'a Path },
    ManualPassword,
    StoredPassword { item: &'a str },
}

pub(crate) enum Client {
    Ssh,
    Sftp,
}

const DESTINATION: &str = "zxt-target";
const NO_ASKPASS: [(&str, Option<&str>); 2] = [
    ("SSH_ASKPASS", None),
    ("SSH_ASKPASS_REQUIRE", Some("never")),
];

/// 설정, 신뢰 스냅샷, Agent 공개 키의 수명은 이 계획에 묶인다.
pub(crate) struct ConnectionPlan {
    ssh: PathBuf,
    config: PathBuf,
    #[cfg(any(windows, target_os = "linux"))]
    stored: Option<Stored>,
    public_key: bool,
    // SFTP·편집기 자식이 같은 계획을 쓰는 동안 전달을 유지하고, 계획이 사라질 때 함께 닫는다.
    forward: Option<std::sync::Mutex<Forward>>,
    _files: SessionFiles,
    _root: trust::RootGuard,
}

#[cfg(any(windows, target_os = "linux"))]
pub(crate) struct Stored {
    pub(crate) target: String,
    pub(crate) user: String,
    pub(crate) fingerprint: String,
    pub(crate) item: String,
    // 승인 화면이 읽은 대상과 접속 시 검증하는 PIN을 계획의 수명 동안 고정한다.
    _snapshot: [fs::File; 2],
    #[cfg(windows)]
    _directory: trust::RootGuard,
}

pub(crate) struct Invocation<'a> {
    pub(crate) program: &'a Path,
    pub(crate) args: Vec<&'a OsStr>,
    pub(crate) environment: &'static [(&'static str, Option<&'static str>)],
}

impl ConnectionPlan {
    pub(crate) fn prepare(
        store: &TrustStore,
        approved: &ApprovedHost<'_>,
        account: &str,
        transport: Transport,
        auth: Authentication<'_>,
    ) -> Result<Self, Error> {
        if account.len() > 64
            || !account.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
            || !account.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            return Err(Error::InvalidAccount);
        }
        let endpoint = approved.endpoint;
        let alias = endpoint.alias();
        let (pin, host_algorithms, root) = store.pin(approved)?;
        // SSM 전달은 루프백으로 접속하되 HostKeyAlias로 원래 대상의 PIN을 그대로 검증한다.
        let (host, port, forward) = match transport {
            Transport::Direct => (endpoint.host.as_str(), endpoint.port, None),
            Transport::Ssm { port: 0, .. } => return Err(Error::InvalidEndpoint),
            Transport::Ssm { port, forward } => ("127.0.0.1", port, Some(std::sync::Mutex::new(forward))),
        };
        let stored_item = match auth {
            Authentication::StoredPassword { item } => Some(item),
            _ => None,
        };
        #[cfg(not(any(windows, target_os = "linux")))]
        if stored_item.is_some() {
            return Err(Error::StoredAuthenticationUnavailable);
        }

        let programs = openssh_directory()?;
        let ssh = programs.join(if cfg!(windows) { "ssh.exe" } else { "ssh" });
        let files = session_files(&store.root)?;
        let known_hosts = files.path().join("known_hosts");
        #[cfg(target_os = "linux")]
        let pinned = stored_item.map(|_| sealed_snapshot(pin.as_bytes())).transpose()?;
        #[cfg(target_os = "linux")]
        let known_hosts = pinned.as_ref().map(snapshot_path).unwrap_or(known_hosts);
        let known_hosts_value = config_path(&known_hosts)?;
        #[cfg(target_os = "linux")]
        if pinned.is_none() {
            new_session_file(&known_hosts)?.write_all(pin.as_bytes())?;
        }
        #[cfg(not(target_os = "linux"))]
        new_session_file(&known_hosts)?.write_all(pin.as_bytes())?;

        let agent_key;
        let auth = match auth {
            Authentication::AgentFile { public_key_path } => {
                agent_key = public_key_file::read(public_key_path)?;
                Authentication::Agent { public_key: &agent_key }
            }
            other => other,
        };
        let (method, public_key, password, agent, batch, identity) = match auth {
            Authentication::Agent { public_key } => {
                let identity = files.path().join("agent.pub");
                let key = trust::plain_public_key(public_key)?;
                let key = key.to_openssh().map_err(|_| Error::InvalidPublicKey)?;
                let mut file = new_session_file(&identity)?;
                writeln!(file, "{key}")?;
                ("publickey", "yes", "no", None, "yes", config_path(&identity)?)
            }
            Authentication::KeyFile { private_key } => {
                ("publickey", "yes", "no", Some("none"), "no", config_path(private_key)?)
            }
            Authentication::ManualPassword | Authentication::StoredPassword { .. } => {
                ("password", "no", "yes", Some("none"), "no", "none".to_owned())
            }
            Authentication::AgentFile { .. } => unreachable!("Agent 공개 키를 먼저 로드합니다"),
        };

        // OpenSSH 9.5는 CertificateFile none을 상대 파일명으로 읽는다.
        let certificate_disabled = config_path(&files.path().join("no-certificate"))?;
        let config = files.path().join("ssh_config");
        let mut contents = Vec::new();
        let mut file = if stored_item.is_none() { Some(new_session_file(&config)?) } else { None };
        let output: &mut dyn Write = match &mut file {
            Some(file) => file,
            None => &mut contents,
        };
        // Host 패턴 대신 단일 목적지를 고정해 대상 인자가 달라도 기본 설정으로 빠지지 않는다.
        writeln!(
            output,
            "HostName {host}\nPort {port}\nUser {account}\nHostKeyAlias {alias}\n\
             StrictHostKeyChecking yes\nUserKnownHostsFile {known_hosts_value}\n\
             GlobalKnownHostsFile none\nKnownHostsCommand none\nVerifyHostKeyDNS no\n\
             CheckHostIP no\nUpdateHostKeys no\n\
             HostKeyAlgorithms {host_algorithms}\nCanonicalizeHostname no\n\
             ProxyCommand none\nProxyJump none\nControlMaster no\nControlPath none\n\
             PermitLocalCommand no\nClearAllForwardings yes\nForwardAgent no\nForwardX11 no\n\
             EscapeChar none\n\
             IdentitiesOnly yes\nAddKeysToAgent no\nCertificateFile {certificate_disabled}\n\
             PubkeyAcceptedAlgorithms -*cert*\nSecurityKeyProvider internal\n\
             GSSAPIAuthentication no\nHostbasedAuthentication no\nKbdInteractiveAuthentication no\n\
             PreferredAuthentications {method}\nPubkeyAuthentication {public_key}\n\
             PasswordAuthentication {password}\nBatchMode {batch}\nIdentityFile {identity}",
        )?;
        if let Some(agent) = agent {
            writeln!(output, "IdentityAgent {agent}")?;
        }
        // SSM 세션은 입력이 없으면 유휴 제한으로 끊기므로 ssh 수준 keepalive를 보낸다.
        if forward.is_some() {
            writeln!(output, "ServerAliveInterval 60\nServerAliveCountMax 3")?;
        }
        if stored_item.is_some() {
            writeln!(output, "NumberOfPasswordPrompts 1")?;
        }
        drop(file);
        #[cfg(target_os = "linux")]
        let sealed = stored_item.map(|_| sealed_snapshot(&contents)).transpose()?;
        #[cfg(target_os = "linux")]
        let config = sealed.as_ref().map(snapshot_path).unwrap_or(config);
        #[cfg(windows)]
        if stored_item.is_some() {
            new_session_file(&config)?.write_all(&contents)?;
        }
        #[cfg(windows)]
        let (directory, snapshots) = {
            let directory = trust_windows::root(files.path())?;
            let snapshots = [trust_windows::pin(&known_hosts)?, trust_windows::pin(&config)?];
            if stored_item.is_some() {
                use std::io::Read;
                for (file, expected) in snapshots.iter().zip([pin.as_bytes(), contents.as_slice()]) {
                    let mut actual = Vec::with_capacity(expected.len() + 1);
                    file.take((expected.len() + 1) as u64).read_to_end(&mut actual)?;
                    if actual != expected {
                        return Err(Error::InvalidTrustRecord);
                    }
                }
            }
            if agent.is_none() {
                trust_windows::pin(&files.path().join("agent.pub"))?;
            }
            (directory, snapshots)
        };
        #[cfg(any(windows, target_os = "linux"))]
        let stored = stored_item.map(|item| Stored {
            target: alias,
            user: account.to_owned(),
            fingerprint: approved.fingerprint(),
            item: item.to_owned(),
            #[cfg(target_os = "linux")]
            _snapshot: [pinned.unwrap(), sealed.unwrap()],
            #[cfg(windows)]
            _snapshot: snapshots,
            #[cfg(windows)]
            _directory: directory,
        });
        Ok(Self {
            ssh, config,
            #[cfg(any(windows, target_os = "linux"))]
            stored,
            public_key: public_key == "yes",
            forward,
            _files: files, _root: root,
        })
    }

    #[cfg(any(windows, target_os = "linux"))]
    pub(crate) fn stored(&self) -> Option<&Stored> { self.stored.as_ref() }

    /// SSM 전달이 끝났으면 그 사유를 돌려준다. 직접 전송은 항상 `None`이다.
    pub(crate) fn forward_ended(&self) -> Option<String> {
        match self.forward.as_ref()?.lock().unwrap().observe() {
            crate::tunnels::Observation::Ended(reason) => Some(format!("SSM 전달 종료: {reason}")),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn config_path_for_test(&self) -> &std::path::Path {
        &self.config
    }

    /// 승인된 askpass 교환에서만 사용한다. 이 호출 자체는 askpass를 활성화하지 않는다.
    #[cfg(any(windows, target_os = "linux"))]
    pub(crate) fn stored_ssh_invocation(&self) -> Result<Invocation<'_>, Error> {
        self.stored().ok_or(Error::StoredAuthenticationUnavailable)?;
        Ok(self.client_invocation(Client::Ssh))
    }

    // 원격 nano는 배치 로그인이 가능한 Agent·키 대상에서만 연다.
    fn editor_invocation(&self, tty: bool) -> Option<Invocation<'_>> {
        if !self.public_key { return None; }
        // 저장 암호 대상은 배치 로그인이 불가능하므로 편집기 경로를 열지 않는다.
        let mut invocation = self.invocation(Client::Ssh).ok()?;
        invocation.args.splice(..0, [
            if tty { "-tt" } else { "-T" },
            "-oBatchMode=yes", "-oPreferredAuthentications=publickey",
            "-oPasswordAuthentication=no", "-oKbdInteractiveAuthentication=no",
        ].map(OsStr::new));
        Some(invocation)
    }

    /// argv는 셸 문자열로 조합하지 않고, 자식 환경에 지정한 항목을 모두 적용한다.
    pub(crate) fn invocation(&self, client: Client) -> Result<Invocation<'_>, Error> {
        #[cfg(any(windows, target_os = "linux"))]
        if self.stored().is_some() {
            return Err(match client {
                Client::Ssh => Error::StoredAuthenticationUnavailable,
                // 별도 자식을 여는 배치 SFTP에는 PTY 승인을 재사용할 수 없다.
                Client::Sftp => Error::StoredSftpAskpassRequired,
            });
        }
        Ok(self.client_invocation(client))
    }

    fn client_invocation(&self, client: Client) -> Invocation<'_> {
        let config = self.config.as_os_str();
        let tail = [OsStr::new("-F"), config, OsStr::new("--"), OsStr::new(DESTINATION)];
        let args = match client {
            Client::Ssh => tail.to_vec(),
            Client::Sftp => ["-T", "-oBatchMode=yes", "-s"].map(OsStr::new)
                .into_iter().chain(tail).chain([OsStr::new("sftp")]).collect(),
        };
        Invocation { program: &self.ssh, args, environment: &NO_ASKPASS }
    }
}

#[cfg(target_os = "linux")]
fn sealed_snapshot(contents: &[u8]) -> io::Result<fs::File> {
    use std::os::fd::FromRawFd;
    // SAFETY: 커널이 새 파일 디스크립터의 소유권을 반환한다.
    let fd = unsafe { libc::memfd_create(c"zxt-ssh-plan".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: 성공한 memfd_create가 반환한 디스크립터다.
    let mut file = unsafe { fs::File::from_raw_fd(fd) };
    // SAFETY: 열린 디스크립터와 일반 파일 권한 마스크를 전달한다.
    if unsafe { libc::fchmod(fd, 0o600) } < 0 {
        return Err(io::Error::last_os_error());
    }
    file.write_all(contents)?;
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    // SAFETY: 열린 디스크립터에 F_ADD_SEALS 비트마스크를 전달한다.
    if unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

#[cfg(target_os = "linux")]
fn snapshot_path(file: &fs::File) -> PathBuf {
    use std::os::fd::AsRawFd;
    PathBuf::from(format!("/proc/{}/fd/{}", std::process::id(), file.as_raw_fd()))
}

#[cfg(unix)]
type SessionFiles = tempfile::TempDir;

#[cfg(unix)]
fn session_files(root: &Path) -> io::Result<SessionFiles> {
    use std::os::unix::fs::PermissionsExt;
    tempfile::Builder::new().prefix("ssh plan ")
        .permissions(fs::Permissions::from_mode(0o700)).tempdir_in(root)
}

#[cfg(windows)]
struct SessionFiles(PathBuf);

#[cfg(windows)]
impl SessionFiles {
    fn path(&self) -> &Path { &self.0 }
}

#[cfg(windows)]
impl Drop for SessionFiles {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(windows)]
fn session_files(root: &Path) -> io::Result<SessionFiles> {
    let directory = tempfile::Builder::new().prefix("ssh plan ").disable_cleanup(true)
        .make_in(root, |path| {
            trust_windows::create_dir(path)?;
            Ok(SessionFiles(path.to_owned()))
        })?;
    Ok(directory.into_parts().0)
}

#[cfg(windows)]
use trust_windows::create_file as new_session_file;

#[cfg(unix)]
fn new_session_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)
}

fn config_path(path: &Path) -> Result<String, Error> {
    if !path.is_absolute() || path.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
        return Err(Error::InvalidPath);
    }
    let value = path.to_str().ok_or(Error::InvalidPath)?;
    if value.chars().any(|c| c.is_control() || "\"%$`*?[{".contains(c)) {
        return Err(Error::InvalidPath);
    }
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        if !matches!(path.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_)))
            || value[2..].contains([':', '<', '>', '|'])
            || path.components().any(|part| matches!(part, Component::Normal(name) if unsafe_windows_component(name)))
        {
            return Err(Error::InvalidPath);
        }
        Ok(format!("\"{}\"", value.replace('\\', "/")))
    }
    #[cfg(not(windows))]
    {
        if value.contains('\\') {
            return Err(Error::InvalidPath);
        }
        Ok(format!("\"{value}\""))
    }
}

#[cfg(windows)]
fn unsafe_windows_component(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else { return true };
    if name.ends_with(['.', ' ']) {
        return true;
    }
    let stem = name.split('.').next().unwrap_or(name).trim_end().to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || stem.strip_prefix("COM").or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|unit| matches!(unit, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"))
}

#[cfg(unix)]
fn openssh_directory() -> io::Result<PathBuf> {
    Ok(PathBuf::from("/usr/bin"))
}

#[cfg(windows)]
fn openssh_directory() -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut buffer = [0; 260];
    // SAFETY: 버퍼는 전달한 길이만큼 쓰기 가능하다.
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    if length >= buffer.len() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length])).join("OpenSSH"))
}
