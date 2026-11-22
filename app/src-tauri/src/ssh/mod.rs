pub(crate) mod terminal;
mod trust;
#[cfg(windows)]
mod trust_windows;

#[cfg(test)]
mod tests;

use std::{
    ffi::OsStr,
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
};

pub(crate) use trust::{ApprovedHost, Endpoint, TrustStore};

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
    SsmProxyUnavailable,
    StoredAuthenticationUnavailable,
    Io(io::Error),
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(crate) enum Transport {
    Direct,
    Ssm,
}

pub(crate) enum Authentication<'a> {
    Agent { public_key: &'a str },
    AgentFile { public_key_path: &'a Path },
    KeyFile { private_key: &'a Path },
    ManualPassword,
    StoredPassword,
}

pub(crate) enum Client {
    Ssh,
    Sftp,
}

const DESTINATION: &str = "zzabba-target";
const NO_ASKPASS: [(&str, Option<&str>); 2] = [
    ("SSH_ASKPASS", None),
    ("SSH_ASKPASS_REQUIRE", Some("never")),
];

/// 설정, 신뢰 스냅샷, Agent 공개 키의 수명은 이 계획에 묶인다.
pub(crate) struct ConnectionPlan {
    ssh: PathBuf,
    sftp: PathBuf,
    config: PathBuf,
    _files: SessionFiles,
    _root: trust::RootGuard,
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
        if matches!(transport, Transport::Ssm) {
            return Err(Error::SsmProxyUnavailable);
        }
        if matches!(auth, Authentication::StoredPassword) {
            return Err(Error::StoredAuthenticationUnavailable);
        }

        let programs = openssh_directory()?;
        let ssh = programs.join(if cfg!(windows) { "ssh.exe" } else { "ssh" });
        let sftp = programs.join(if cfg!(windows) { "sftp.exe" } else { "sftp" });
        let files = session_files(&store.root)?;
        let known_hosts = files.path().join("known_hosts");
        let known_hosts_value = config_path(&known_hosts)?;
        new_session_file(&known_hosts)?.write_all(pin.as_bytes())?;

        let agent_key;
        let auth = match auth {
            Authentication::AgentFile { public_key_path } => {
                use std::io::Read as _;
                config_path(public_key_path)?;
                if !fs::metadata(public_key_path)?.is_file() {
                    return Err(Error::InvalidPath);
                }
                let mut input = String::new();
                fs::File::open(public_key_path)?.take(16385).read_to_string(&mut input)?;
                if input.len() > 16384 { return Err(Error::InvalidPublicKey); }
                agent_key = input;
                Authentication::Agent { public_key: agent_key.trim() }
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
            Authentication::ManualPassword => {
                ("password", "no", "yes", Some("none"), "no", "none".to_owned())
            }
            Authentication::StoredPassword => return Err(Error::StoredAuthenticationUnavailable),
            Authentication::AgentFile { .. } => unreachable!("Agent 공개 키를 먼저 로드합니다"),
        };

        // OpenSSH 9.5는 CertificateFile none을 상대 파일명으로 읽는다.
        let certificate_disabled = config_path(&files.path().join("no-certificate"))?;
        let config = files.path().join("ssh_config");
        let mut file = new_session_file(&config)?;
        // Host 패턴 대신 단일 목적지를 고정해 대상 인자가 달라도 기본 설정으로 빠지지 않는다.
        writeln!(
            file,
            "HostName {}\nPort {}\nUser {account}\nHostKeyAlias {alias}\n\
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
            endpoint.host, endpoint.port,
        )?;
        if let Some(agent) = agent {
            writeln!(file, "IdentityAgent {agent}")?;
        }
        drop(file);
        #[cfg(windows)]
        {
            trust_windows::root(files.path())?;
            trust_windows::pin(&known_hosts)?;
            trust_windows::pin(&config)?;
            if agent.is_none() {
                trust_windows::pin(&files.path().join("agent.pub"))?;
            }
        }
        Ok(Self { ssh, sftp, config, _files: files, _root: root })
    }

    /// argv는 셸 문자열로 조합하지 않고, 자식 환경에 지정한 항목을 모두 적용한다.
    pub(crate) fn invocation(&self, client: Client) -> Invocation<'_> {
        let config = self.config.as_os_str();
        let tail = [OsStr::new("-F"), config, OsStr::new("--"), OsStr::new(DESTINATION)];
        let (program, args) = match client {
            Client::Ssh => (&self.ssh, tail.to_vec()),
            Client::Sftp => (
                &self.sftp,
                [OsStr::new("-S"), self.ssh.as_os_str()].into_iter().chain(tail).collect(),
            ),
        };
        Invocation { program, args, environment: &NO_ASKPASS }
    }
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
