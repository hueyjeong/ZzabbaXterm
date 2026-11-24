//! 첫 연결의 SSH 호스트 키 확인(TOFU)이다.
//! 핀이 없을 때만 서버 키를 받아 OS 확인 창으로 묻고, 사용자가 신뢰하면 신뢰 저장소에 고정한다.
//! 고정한 뒤에는 `StrictHostKeyChecking yes`가 접속마다 같은 키인지 확인하고, 키가 바뀌면 인증 전에 끊는다.

use std::{collections::HashMap, path::Path, sync::{Arc, Mutex, OnceLock}};

use ssh_key::{Algorithm, EcdsaCurve, HashAlg, PublicKey};

use crate::connections::profile::{Ssh, SshTransport};
use super::{ApprovedHost, Endpoint, Error, TRUST_DIRECTORY, TrustStore, terminal::{pin_error, trust_store}, trust::plain_public_key};

/// OS 확인 창을 띄우고 사용자의 선택을 돌려준다. WebView 밖의 네이티브 창만 넣는다.
pub(crate) type Confirm = Arc<dyn Fn(String) -> Result<bool, String> + Send + Sync>;

/// 서버 키 목록(`ssh-keyscan` 출력)을 받는다. 시험은 가짜 출력을 넣는다.
pub(crate) type Scan<'a> = &'a dyn Fn(&str, u16) -> Result<String, String>;

/// 키를 받을 주소다. SSM 경유면 앱이 연 전달의 루프백 포트다.
#[derive(Clone, Copy)]
pub(crate) enum Via {
    Direct,
    Forward(u16),
}

pub(crate) const TRUST_TITLE: &str = "SSH 서버 키 확인";
pub(crate) const REPLACE_TITLE: &str = "SSH 서버 키 변경";
pub(crate) const INPUT_MISMATCH: &str = "서버 호스트 키가 입력한 지문과 다릅니다";
pub(crate) const DECLINED: &str = "SSH 서버 키 신뢰가 취소되었습니다";
const NO_KEY: &str = "SSH 서버 호스트 키를 받을 수 없습니다";
const CONFLICT: &str = "다른 SSH 호스트 키가 이미 등록되어 있습니다";

/// 등록된 핀의 지문이다. 지문을 입력했는데 핀과 다르면 중단한다.
pub(crate) fn pinned(config: &Path, ssh: &Ssh) -> Result<Option<String>, String> {
    let endpoint = Endpoint::new(&ssh.host, ssh.port).map_err(pin_error)?;
    let store = match TrustStore::open(&config.join(TRUST_DIRECTORY)) {
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        result => result.map_err(pin_error)?,
    };
    pinned_in(&store, &endpoint, ssh)
}

/// 확인 창을 띄울 수 없는 경로는 핀이 있어야 한다. SSM 경유 저장 암호 경로가 쓴다.
#[cfg_attr(not(all(feature = "stored-auth", any(windows, target_os = "linux"))), allow(dead_code))]
pub(crate) fn required(config: &Path, ssh: &Ssh) -> Result<String, String> {
    pinned(config, ssh)?.ok_or_else(|| pin_error(Error::UnregisteredHost))
}

/// 승인 문구와 승인 범위에 넣는 지문이다. 핀이 있으면 핀 지문, 없으면 입력한 지문이다.
pub(crate) fn shown(config: &Path, ssh: &Ssh) -> String {
    pinned(config, ssh).ok().flatten()
        .or_else(|| ssh.host_key_fingerprint.clone())
        .unwrap_or_else(|| "미등록".into())
}

fn pinned_in(store: &TrustStore, endpoint: &Endpoint, ssh: &Ssh) -> Result<Option<String>, String> {
    let Some(key) = store.pinned(endpoint).map_err(pin_error)? else { return Ok(None) };
    let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
    if ssh.host_key_fingerprint.as_ref().is_some_and(|expected| *expected != fingerprint) {
        return Err(pin_error(Error::FingerprintMismatch));
    }
    Ok(Some(fingerprint))
}

/// 핀이 있으면 그 지문을 쓴다. 없으면 서버 키를 받아 확인 창을 거친 뒤 고정한다.
/// `confirm`이 없으면 등록된 핀만 받는다.
pub(crate) fn trusted(config: &Path, ssh: &Ssh, via: Via, scan: Scan<'_>, confirm: Option<&Confirm>) -> Result<String, String> {
    if let Some(fingerprint) = pinned(config, ssh)? {
        return Ok(fingerprint);
    }
    let Some(confirm) = confirm else { return Err(pin_error(Error::UnregisteredHost)) };
    let endpoint = Endpoint::new(&ssh.host, ssh.port).map_err(pin_error)?;
    let store = trust_store(config)?;
    // 같은 서버의 탭 두 개가 동시에 열려도 확인 창은 하나만 뜬다.
    let lock = alias_lock(&endpoint.alias());
    let _guard = lock.lock().map_err(|_| pin_error(Error::TrustRootChanged))?;
    if let Some(fingerprint) = pinned_in(&store, &endpoint, ssh)? {
        return Ok(fingerprint);
    }
    let (key, fingerprint) = scanned(&endpoint, ssh, via, scan)?;
    if !confirm(prompt(&endpoint, ssh, &key, &fingerprint))? {
        return Err(DECLINED.into());
    }
    let approved = ApprovedHost::confirm(&endpoint, &endpoint, &fingerprint).map_err(pin_error)?;
    store.register(&approved, &openssh(&key)?).map_err(trust_error)?;
    Ok(fingerprint)
}

/// 메뉴 "호스트 키 확인…"의 결과다.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Checked {
    Registered,
    Unchanged,
    Replaced,
}

/// 서버 키를 다시 받아 핀과 비교한다. 핀이 없으면 등록 확인 창을, 다르면 이전·새 지문을 보여 주는 교체 확인 창을 띄운다.
pub(crate) fn check(config: &Path, ssh: &Ssh, via: Via, scan: Scan<'_>, confirm: &Confirm, replace: &Confirm)
    -> Result<(String, Checked), String>
{
    let endpoint = Endpoint::new(&ssh.host, ssh.port).map_err(pin_error)?;
    let store = trust_store(config)?;
    let lock = alias_lock(&endpoint.alias());
    let _guard = lock.lock().map_err(|_| pin_error(Error::TrustRootChanged))?;
    let (key, fingerprint) = scanned(&endpoint, ssh, via, scan)?;
    let approved = ApprovedHost::confirm(&endpoint, &endpoint, &fingerprint).map_err(pin_error)?;
    let Some(previous) = store.pinned(&endpoint).map_err(pin_error)? else {
        if !confirm(prompt(&endpoint, ssh, &key, &fingerprint))? {
            return Err(DECLINED.into());
        }
        store.register(&approved, &openssh(&key)?).map_err(trust_error)?;
        return Ok((fingerprint, Checked::Registered));
    };
    let old = previous.fingerprint(HashAlg::Sha256).to_string();
    if old == fingerprint {
        return Ok((fingerprint, Checked::Unchanged));
    }
    let message = format!(
        "서버: {}\n포트: {}\n{}키 종류: {}\n이전 SHA256 지문: {old}\n새 SHA256 지문: {fingerprint}",
        endpoint.host, endpoint.port, route_line(ssh), key.algorithm().as_str(),
    );
    if !replace(message)? {
        return Err("SSH 서버 키 교체가 취소되었습니다".into());
    }
    store.replace(&approved, &previous, &openssh(&key)?).map_err(trust_error)?;
    Ok((fingerprint, Checked::Replaced))
}

fn scanned(endpoint: &Endpoint, ssh: &Ssh, via: Via, scan: Scan<'_>) -> Result<(PublicKey, String), String> {
    let (host, port) = match via {
        Via::Direct => (endpoint.host.as_str(), endpoint.port),
        Via::Forward(port) => ("127.0.0.1", port),
    };
    let key = choose(&scan(host, port)?).ok_or(NO_KEY)?;
    let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
    // 입력한 지문이 있으면 확인 창 없이 먼저 대조한다.
    if ssh.host_key_fingerprint.as_ref().is_some_and(|expected| *expected != fingerprint) {
        return Err(INPUT_MISMATCH.into());
    }
    Ok((key, fingerprint))
}

fn prompt(endpoint: &Endpoint, ssh: &Ssh, key: &PublicKey, fingerprint: &str) -> String {
    let matched = if ssh.host_key_fingerprint.is_some() { "\n입력한 지문과 일치" } else { "" };
    format!(
        "서버: {}\n포트: {}\n{}키 종류: {}\nSHA256 지문: {fingerprint}{matched}",
        endpoint.host, endpoint.port, route_line(ssh), key.algorithm().as_str(),
    )
}

fn route_line(ssh: &Ssh) -> String {
    match &ssh.transport {
        SshTransport::Ssm(target) => format!("연결 경로: AWS SSM 경유 · {} · {} · {}\n", target.profile, target.region, target.instance_id),
        SshTransport::Direct {} => String::new(),
    }
}

fn openssh(key: &PublicKey) -> Result<String, String> {
    key.to_openssh().map_err(|_| pin_error(Error::InvalidPublicKey))
}

fn trust_error(error: Error) -> String {
    match error {
        Error::ExistingPinConflict => CONFLICT.into(),
        other => pin_error(other),
    }
}

fn alias_lock(alias: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS.get_or_init(Mutex::default).lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    Arc::clone(locks.entry(alias.to_owned()).or_default())
}

/// `ssh-keyscan` 출력에서 ed25519 → ECDSA(P-256, P-384, P-521) → RSA 순서로 키 하나를 고른다.
pub(super) fn choose(output: &str) -> Option<PublicKey> {
    let rank = |key: &PublicKey| match key.algorithm() {
        Algorithm::Ed25519 => Some(0),
        Algorithm::Ecdsa { curve: EcdsaCurve::NistP256 } => Some(1),
        Algorithm::Ecdsa { curve: EcdsaCurve::NistP384 } => Some(2),
        Algorithm::Ecdsa { curve: EcdsaCurve::NistP521 } => Some(3),
        Algorithm::Rsa { .. } => Some(4),
        _ => None,
    };
    output.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once(' '))
        .filter_map(|(_, key)| plain_public_key(key).ok())
        .filter_map(|key| rank(&key).map(|rank| (rank, key)))
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, key)| key)
}

/// OpenSSH `ssh`로 키 교환만 해서 서버 키를 받는다. 셸 없이 인자 배열로 실행하고 호스트는 `--` 뒤에 둔다.
/// Windows OpenSSH 9.5의 `ssh-keyscan`은 sntrup761 키 교환을 처리하지 못해 최신 서버에서 키를 받지 못하므로 쓰지 않는다.
/// 빈 임시 `known_hosts`에 `accept-new`로 키 한 줄만 기록하게 하고, 모든 인증 방식을 꺼 키 교환 직후 끊는다.
pub(crate) fn keyscan(host: &str, port: u16) -> Result<String, String> {
    use std::{io::Read, process::{Command, Stdio}, time::{Duration, Instant}};
    const LIMIT: u64 = 16 * 1024;
    const DEADLINE: Duration = Duration::from_secs(20);
    let failed = |error: std::io::Error| format!("SSH 서버 키 받기: {error}");
    let directory = tempfile::tempdir().map_err(failed)?;
    let known_hosts = directory.path().join("known_hosts");
    let known_hosts_value = super::config_path(&known_hosts).map_err(pin_error)?;
    let program = super::openssh_directory().map_err(failed)?.join(if cfg!(windows) { "ssh.exe" } else { "ssh" });
    let mut command = Command::new(program);
    command.args(["-F", "none", "-T", "-p", &port.to_string(), "-l", "zzabba-host-key"])
        .args([
            "BatchMode=yes", "StrictHostKeyChecking=accept-new", &format!("UserKnownHostsFile={known_hosts_value}"),
            "GlobalKnownHostsFile=none", "HashKnownHosts=no", "CheckHostIP=no", "UpdateHostKeys=no",
            "ProxyCommand=none", "ProxyJump=none", "ConnectTimeout=10", "ControlMaster=no", "ControlPath=none",
            "PubkeyAuthentication=no", "PasswordAuthentication=no", "KbdInteractiveAuthentication=no",
            "HostbasedAuthentication=no", "IdentityAgent=none", "ForwardAgent=no", "ClearAllForwardings=yes",
            "SessionType=none", "LogLevel=ERROR",
            "HostKeyAlgorithms=ssh-ed25519,ecdsa-sha2-nistp256,ecdsa-sha2-nistp384,ecdsa-sha2-nistp521,rsa-sha2-512,rsa-sha2-256",
        ].into_iter().flat_map(|option| ["-o".to_owned(), option.to_owned()]))
        .args(["--", host])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .env_remove("SSH_AUTH_SOCK").env_remove("SSH_ASKPASS").env("SSH_ASKPASS_REQUIRE", "never");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(failed)?;
    let started = Instant::now();
    // 인증이 모두 꺼져 있어 보통 키 교환 직후 끝난다. 기한을 넘기면 끊는다.
    while child.try_wait().map_err(failed)?.is_none() {
        if started.elapsed() > DEADLINE {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut text = String::new();
    match std::fs::File::open(&known_hosts) {
        Ok(file) => { file.take(LIMIT).read_to_string(&mut text).map_err(failed)?; }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(failed(error)),
    }
    Ok(text)
}

#[cfg(test)]
mod tests;
