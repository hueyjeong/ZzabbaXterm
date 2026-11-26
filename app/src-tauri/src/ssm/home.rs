use super::CliError;
use sha1::{Digest, Sha1};
use std::{
    ffi::OsStr,
    fs,
    io::{ErrorKind, Read, Write},
    path::Path,
};
#[cfg(not(windows))]
use tempfile::TempDir;
use zeroize::Zeroizing;

pub(super) struct CliHome {
    #[cfg(not(windows))]
    dir: TempDir,
    #[cfg(windows)]
    dir: std::path::PathBuf,
    // `aws login` 갱신은 새 갱신 토큰을 캐시에 다시 쓴다. 사본이 아니라 사용자 캐시를 직접 가리킨다.
    login_cache: Option<std::path::PathBuf>,
    // 같은 login_session의 갱신 잠금. 내보내기가 끝나 홈을 버리면 풀린다.
    _refresh: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl CliHome {
    #[cfg(not(windows))]
    pub(super) fn new() -> Result<Self, CliError> {
        let mut builder = tempfile::Builder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        builder
            .prefix("zxt-aws-")
            .tempdir()
            .map(|dir| Self { dir, login_cache: None, _refresh: None })
            .map_err(|_| CliError::ProcessIo)
    }

    #[cfg(windows)]
    pub(super) fn new() -> Result<Self, CliError> {
        use windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData;
        let parent = super::windows_acl::known_folder(&FOLDERID_LocalAppData)
            .map_err(|_| CliError::ProcessIo)?;
        super::windows_acl::trusted_chain(&parent, false)
            .map_err(|_| CliError::InvalidConfiguration)?;
        Self::create_in(&parent)
    }

    #[cfg(windows)]
    fn create_in(parent: &Path) -> Result<Self, CliError> {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).map_err(|_| CliError::RandomUnavailable)?;
        let path = parent.join(format!("zxt-aws-{:032x}", u128::from_ne_bytes(nonce)));
        super::windows_acl::private_dir(&path).map_err(|_| CliError::ProcessIo)?;
        Ok(Self { dir: path, login_cache: None, _refresh: None })
    }

    pub(super) fn path(&self) -> &Path {
        #[cfg(windows)]
        return &self.dir;
        #[cfg(not(windows))]
        self.dir.path()
    }

    pub(super) fn use_login_cache(&mut self, cache: std::path::PathBuf, refresh: tokio::sync::OwnedMutexGuard<()>) {
        self.login_cache = Some(cache);
        self._refresh = Some(refresh);
    }

    pub(super) fn import_sso_token(&self, source_home: &Path, key: &str) -> Result<(), CliError> {
        // Botocore는 sso-session 이름(레거시는 시작 URL)의 SHA-1을 파일 이름으로 쓴다.
        let relative = sso_cache_entry(key);
        let source = source_home.join(&relative);
        match fs::symlink_metadata(&source) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Ok(metadata) if metadata.file_type().is_file() && metadata.len() <= 1024 * 1024 => (),
            _ => return Err(CliError::InvalidConfiguration),
        }
        let destination = self.path().join(&relative);
        fs::create_dir_all(destination.parent().ok_or(CliError::ProcessIo)?)
            .map_err(|_| CliError::ProcessIo)?;
        let mut bytes = Zeroizing::new(Vec::new());
        fs::File::open(source)
            .and_then(|file| file.take(1024 * 1024 + 1).read_to_end(&mut bytes))
            .map_err(|_| CliError::ProcessIo)?;
        if bytes.len() > 1024 * 1024 {
            return Err(CliError::InvalidConfiguration);
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(destination)
            .and_then(|mut file| file.write_all(&bytes))
            .map_err(|_| CliError::ProcessIo)
    }

    pub(super) fn env(&self, mut set: impl FnMut(&str, &OsStr)) {
        // AWS CLI alias와 Botocore 캐시는 모두 Python의 expanduser 경로를 사용한다.
        for name in [
            "HOME",
            "USERPROFILE",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
        ] {
            set(name, self.path().as_os_str());
        }
        if let Some(cache) = &self.login_cache {
            set("AWS_LOGIN_CACHE_DIRECTORY", cache.as_os_str());
        }
    }
}

#[cfg(windows)]
impl Drop for CliHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

pub(crate) fn sso_cache_entry(key: &str) -> std::path::PathBuf {
    let name = format!("{:x}.json", Sha1::digest(key.as_bytes()));
    Path::new(".aws/sso/cache").join(name)
}

pub(crate) fn user_home() -> Result<std::path::PathBuf, CliError> {
    // Python은 POSIX에서 HOME, Windows에서 USERPROFILE을 우선한다.
    let value = if cfg!(windows) {
        std::env::var_os("USERPROFILE").or_else(|| {
            let mut drive = std::env::var_os("HOMEDRIVE").unwrap_or_default();
            drive.push(std::env::var_os("HOMEPATH")?);
            Some(drive)
        })
    } else {
        std::env::var_os("HOME").or_else(|| std::env::home_dir().map(Into::into))
    };
    value
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or(CliError::InvalidConfiguration)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_login_homes_point_at_the_user_login_cache() {
        let names = |home: &CliHome| {
            let mut found = Vec::new();
            home.env(|name, value| found.push((name.to_owned(), value.to_owned())));
            found
        };
        let sso = CliHome::new().unwrap();
        assert!(!names(&sso).iter().any(|(name, _)| name == "AWS_LOGIN_CACHE_DIRECTORY"));
        let lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        let mut login = CliHome::new().unwrap();
        let cache = std::path::PathBuf::from("fixture-login-cache");
        login.use_login_cache(cache.clone(), lock.clone().lock_owned().await);
        let env = names(&login);
        assert!(env.contains(&("AWS_LOGIN_CACHE_DIRECTORY".to_owned(), cache.into_os_string())));
        // 격리 홈은 그대로 임시 디렉터리를 가리킨다.
        assert!(env.contains(&("HOME".to_owned(), login.path().as_os_str().to_owned())));
        // 갱신 잠금은 홈을 버릴 때 풀린다.
        assert!(lock.try_lock().is_err());
        drop(login);
        assert!(lock.try_lock().is_ok());
    }

    #[test]
    fn only_selected_cache_entry_crosses_the_home_boundary() {
        let parent = CliHome::new().unwrap();
        let cache = parent.path().join(".aws/sso/cache");
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(parent.path().join(".aws/cli")).unwrap();
        fs::write(parent.path().join(".aws/cli/alias"), b"synthetic alias").unwrap();
        fs::write(cache.join("other.json"), b"other").unwrap();
        let chosen = "a9993e364706816aba3e25717850c26c9cd0d89d.json";
        fs::write(cache.join(chosen), b"fixture-only-token").unwrap();
        let isolated = CliHome::new().unwrap();
        isolated.import_sso_token(parent.path(), "abc").unwrap();
        assert_eq!(
            fs::read(isolated.path().join(".aws/sso/cache").join(chosen)).unwrap(),
            b"fixture-only-token"
        );
        assert!(!isolated.path().join(".aws/sso/cache/other.json").exists());
        assert!(!isolated.path().join(".aws/cli/alias").exists());
    }

    #[cfg(windows)]
    #[test]
    fn copied_cache_cannot_inherit_broad_parent_access() {
        use super::super::windows_acl::tests::{assert_user_only, fixture_dir};
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join("shared");
        fixture_dir(&shared, "FA");
        let source = CliHome::create_in(root.path()).unwrap();
        let relative = Path::new(".aws/sso/cache/a9993e364706816aba3e25717850c26c9cd0d89d.json");
        let cache = source.path().join(relative);
        fs::create_dir_all(cache.parent().unwrap()).unwrap();
        fs::write(cache, b"fixture-only-token").unwrap();
        let home = CliHome::create_in(&shared).unwrap();
        home.import_sso_token(source.path(), "abc").unwrap();
        assert_user_only(home.path(), true, true);
        assert_user_only(&home.path().join(relative), false, false);
    }
}
