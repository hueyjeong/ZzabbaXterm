use super::CliError;
use sha1::{Digest, Sha1};
use std::{
    ffi::OsStr,
    fs,
    io::{ErrorKind, Read, Write},
    path::Path,
};
use tempfile::TempDir;
use zeroize::Zeroizing;

pub(super) struct CliHome(TempDir);

impl CliHome {
    pub(super) fn new() -> Result<Self, CliError> {
        let mut builder = tempfile::Builder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        builder
            .prefix("zzabba-aws-")
            .tempdir()
            .map(Self)
            .map_err(|_| CliError::ProcessIo)
    }

    pub(super) fn path(&self) -> &Path {
        self.0.path()
    }

    pub(super) fn import_sso_token(&self, source_home: &Path, key: &str) -> Result<(), CliError> {
        // Botocore는 sso-session 이름(레거시는 시작 URL)의 SHA-1을 파일 이름으로 쓴다.
        let name = format!("{:x}.json", Sha1::digest(key.as_bytes()));
        let relative = Path::new(".aws/sso/cache").join(name);
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
    }
}

pub(super) fn user_home() -> Result<std::path::PathBuf, CliError> {
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
}
