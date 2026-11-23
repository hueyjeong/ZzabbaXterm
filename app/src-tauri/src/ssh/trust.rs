use std::{
    io::{self, Read, Write},
    net::IpAddr,
    path::{Path, PathBuf},
};
#[cfg(unix)]
use std::fs;

use sha2::{Digest, Sha256};
use ssh_key::{Algorithm, Fingerprint, HashAlg, PublicKey};

use super::{Error, config_path};

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub(super) host: String,
    pub(super) port: u16,
}

impl Endpoint {
    pub(crate) fn new(host: &str, port: u16) -> Result<Self, Error> {
        if port == 0 {
            return Err(Error::InvalidEndpoint);
        }
        let host = if let Ok(ip) = host.parse::<IpAddr>() {
            ip.to_string()
        } else {
            let dns = host.strip_suffix('.').unwrap_or(host);
            if dns.len() > 253 || !dns.split('.').all(valid_label) {
                return Err(Error::InvalidEndpoint);
            }
            host.to_ascii_lowercase()
        };
        Ok(Self { host, port })
    }

    pub(super) fn alias(&self) -> String {
        // HostKeyAlias는 OpenSSH에서 포트를 덧붙이지 않고 그대로 조회한다.
        format!("[{}]:{}", self.host, self.port)
    }
}

/// 승인 화면의 서버·포트·지문과 Rust가 해석한 실제 접속 대상을 묶는다.
pub(crate) struct ApprovedHost<'a> {
    pub(super) endpoint: &'a Endpoint,
    fingerprint: Fingerprint,
}

impl<'a> ApprovedHost<'a> {
    pub(crate) fn confirm(
        actual: &'a Endpoint,
        approved: &Endpoint,
        approved_sha256: &str,
    ) -> Result<Self, Error> {
        if actual != approved {
            return Err(Error::ApprovedEndpointMismatch);
        }
        let fingerprint: Fingerprint = approved_sha256.parse().map_err(|_| Error::InvalidFingerprint)?;
        if fingerprint.algorithm() != HashAlg::Sha256 {
            return Err(Error::InvalidFingerprint);
        }
        Ok(Self { endpoint: actual, fingerprint })
    }

    #[cfg(any(windows, target_os = "linux"))]
    pub(super) fn fingerprint(&self) -> String { self.fingerprint.to_string() }
}

fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

pub(crate) struct TrustStore {
    pub(super) root: PathBuf,
    id: RootId,
}

impl TrustStore {
    /// Rust 앱이 마련한 전용 디렉터리의 소유자와 접근 권한을 확인한다.
    pub(crate) fn open(private_app_directory: &Path) -> Result<Self, Error> {
        config_path(private_app_directory)?;
        let id = check_root(private_app_directory)?.id;
        Ok(Self { root: private_app_directory.to_owned(), id })
    }

    /// 지정된 새 디렉터리만 생성하며 기존 폴더의 접근 권한은 그대로 둔다.
    pub(crate) fn create(private_app_directory: &Path) -> Result<Self, Error> {
        config_path(private_app_directory)?;
        #[cfg(windows)]
        let _parents = super::trust_windows::creation_parent(private_app_directory)?;
        #[cfg(windows)]
        super::trust_windows::create_dir(private_app_directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(private_app_directory)?;
        }
        Self::open(private_app_directory)
    }

    fn checked_root(&self) -> Result<RootGuard, Error> {
        let guard = check_root(&self.root)?;
        if guard.id != self.id {
            return Err(Error::TrustRootChanged);
        }
        Ok(guard)
    }

    pub(crate) fn register(
        &self,
        approved: &ApprovedHost<'_>,
        administrator_key: &str,
    ) -> Result<(), Error> {
        let key = plain_public_key(administrator_key)?;
        if key.fingerprint(HashAlg::Sha256) != approved.fingerprint {
            return Err(Error::FingerprintMismatch);
        }
        host_algorithms(&key)?;
        let alias = approved.endpoint.alias();
        let path = self.pin_path(&alias);
        let record = pin_record(&alias, &key)?;
        let _root = self.checked_root()?;
        #[cfg(windows)]
        let mut staged = tempfile::Builder::new().make_in(&self.root, super::trust_windows::create_file)?;
        #[cfg(unix)]
        let mut staged = tempfile::NamedTempFile::new_in(&self.root)?;
        staged.write_all(record.as_bytes())?;
        staged.as_file().sync_all()?;
        match staged.persist_noclobber(&path) {
            Ok(file) => {
                #[cfg(windows)]
                {
                    drop(file);
                    super::trust_windows::pin(&path)?;
                }
                #[cfg(unix)]
                drop(file);
                Ok(())
            }
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                if read_pin(&path)? == record {
                    Ok(())
                } else {
                    Err(Error::ExistingPinConflict)
                }
            }
            Err(error) => Err(error.error.into()),
        }
    }

    pub(super) fn pin(&self, approved: &ApprovedHost<'_>) -> Result<(String, &'static str, RootGuard), Error> {
        let root = self.checked_root()?;
        let alias = approved.endpoint.alias();
        let record = read_pin(&self.pin_path(&alias))?;
        let key_text = record
            .strip_prefix(&alias)
            .and_then(|value| value.strip_prefix(' '))
            .ok_or(Error::InvalidTrustRecord)?;
        let key = plain_public_key(key_text).map_err(|_| Error::InvalidTrustRecord)?;
        if pin_record(&alias, &key)? != record {
            return Err(Error::InvalidTrustRecord);
        }
        if key.fingerprint(HashAlg::Sha256) != approved.fingerprint {
            return Err(Error::FingerprintMismatch);
        }
        Ok((record, host_algorithms(&key)?, root))
    }

    fn pin_path(&self, alias: &str) -> PathBuf {
        self.root.join(format!("pin-{:x}.known_hosts", Sha256::digest(alias)))
    }
}

#[cfg(windows)]
pub(super) use super::trust_windows::RootGuard;
#[cfg(windows)]
use super::trust_windows::{FileId as RootId, root as check_root};

#[cfg(unix)]
type RootId = (u64, u64);

#[cfg(unix)]
pub(super) struct RootGuard {
    id: RootId,
}

#[cfg(unix)]
fn check_root(path: &Path) -> Result<RootGuard, Error> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || !user_only(&meta) {
        return Err(Error::InvalidPath);
    }
    // /tmp 같은 sticky 디렉터리는 소유자가 다른 자식 이름의 교체를 막는다.
    for ancestor in path.ancestors().skip(1) {
        let meta = fs::symlink_metadata(ancestor)?;
        // SAFETY: geteuid는 호출 프로세스의 유효 UID를 반환한다.
        let uid = unsafe { libc::geteuid() };
        if !meta.is_dir() || (meta.uid() != 0 && meta.uid() != uid)
            || (meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0)
        {
            return Err(Error::InvalidPath);
        }
    }
    Ok(RootGuard { id: (meta.dev(), meta.ino()) })
}

#[cfg(unix)]
fn user_only(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid는 호출 프로세스의 유효 UID를 반환한다.
    let uid = unsafe { libc::geteuid() };
    meta.uid() == uid && meta.mode() & 0o077 == 0
}

fn read_pin(path: &Path) -> Result<String, Error> {
    #[cfg(windows)]
    let file = super::trust_windows::pin(path);
    #[cfg(unix)]
    let file = unix_pin(path);

    let mut record = String::new();
    file.and_then(|file| {
        // 고정 호스트 레코드는 16 KiB 이내다.
        const LIMIT: usize = 16 * 1024;
        file.take((LIMIT + 1) as u64).read_to_string(&mut record)?;
        if record.len() > LIMIT {
            return Err(Error::InvalidTrustRecord);
        }
        Ok(record)
    }).map_err(|error| match error {
        Error::Io(error) if error.kind() == io::ErrorKind::NotFound => Error::UnregisteredHost,
        Error::Io(error) if error.kind() == io::ErrorKind::InvalidData => Error::InvalidTrustRecord,
        other => other,
    })
}

#[cfg(unix)]
fn unix_pin(path: &Path) -> Result<fs::File, Error> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path).map_err(|error| match error.raw_os_error() {
            Some(libc::ELOOP) => Error::InvalidTrustRecord,
            _ => error.into(),
        })?;
    let meta = file.metadata()?;
    if !meta.is_file() || !user_only(&meta) || meta.nlink() != 1 {
        return Err(Error::InvalidTrustRecord);
    }
    Ok(file)
}

pub(super) fn plain_public_key(text: &str) -> Result<PublicKey, Error> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    if text.contains(['\r', '\n', '\0']) {
        return Err(Error::InvalidPublicKey);
    }
    let mut key = PublicKey::from_openssh(text).map_err(|_| Error::InvalidPublicKey)?;
    key.set_comment("");
    Ok(key)
}

fn pin_record(alias: &str, key: &PublicKey) -> Result<String, Error> {
    let key = key.to_openssh().map_err(|_| Error::InvalidPublicKey)?;
    Ok(format!("{alias} {key}\n"))
}

fn host_algorithms(key: &PublicKey) -> Result<&'static str, Error> {
    use ssh_key::EcdsaCurve::{NistP256, NistP384, NistP521};
    match key.algorithm() {
        Algorithm::Ed25519 => Ok("ssh-ed25519"),
        Algorithm::Rsa { .. } => Ok("rsa-sha2-512,rsa-sha2-256"),
        Algorithm::Ecdsa { curve: NistP256 } => Ok("ecdsa-sha2-nistp256"),
        Algorithm::Ecdsa { curve: NistP384 } => Ok("ecdsa-sha2-nistp384"),
        Algorithm::Ecdsa { curve: NistP521 } => Ok("ecdsa-sha2-nistp521"),
        _ => Err(Error::InvalidPublicKey),
    }
}
