use super::{CancellationToken, CliError, CommandSpec, Tool};
use std::{
    ffi::OsString,
    fs::File,
    path::{Path, PathBuf},
};

const FILE: &str = if cfg!(windows) {
    "session-manager-plugin.exe"
} else {
    "session-manager-plugin"
};

pub(super) struct CheckedPlugin {
    file: std::sync::Arc<Path>,
    path: OsString,
    _file_guard: Option<File>,
}

impl CheckedPlugin {
    pub(super) fn path(&self) -> &std::ffi::OsStr {
        &self.path
    }

    pub(super) fn recheck(&self, aws: &Path, cwd: &Path) -> Result<(), CliError> {
        validate_aws(aws)?;
        #[cfg(windows)]
        super::windows_acl::trusted_chain(&self.file, true).map_err(|_| CliError::PluginMissing)?;
        if self
            .file
            .parent()
            .and_then(|dir| dir.join(FILE).canonicalize().ok())
            .as_deref()
            != Some(self.file.as_ref())
        {
            return Err(CliError::PluginMissing);
        }
        #[cfg(target_os = "linux")]
        if let Some(guard) = &self._file_guard {
            use std::os::unix::fs::MetadataExt;
            let current = self.file.metadata().map_err(|_| CliError::PluginMissing)?;
            let opened = guard.metadata().map_err(|_| CliError::PluginMissing)?;
            if (current.dev(), current.ino()) != (opened.dev(), opened.ino()) {
                return Err(CliError::PluginMissing);
            }
        }
        check_precedence(aws, cwd, &self.file)
    }
}

pub(super) async fn checked_path(
    aws: &Path,
    cwd: &Path,
    cancel: &CancellationToken,
) -> Result<CheckedPlugin, CliError> {
    validate_aws(aws)?;
    let (plugin, guard) = official_file()?;
    let dir = plugin.parent().ok_or(CliError::PluginMissing)?;
    check_precedence(aws, cwd, &plugin)?;
    let output = CommandSpec::version(Tool::Plugin, plugin.clone().into())
        .capture_in(cwd, cancel)
        .await?
        .success()?;
    require_supported_version(&output, cfg!(windows))?;
    let path = std::env::join_paths([dir]).map_err(|_| CliError::PluginMissing)?;
    Ok(CheckedPlugin {
        file: plugin.into(),
        path,
        _file_guard: Some(guard),
    })
}

pub(super) fn validate_aws(path: &Path) -> Result<(), CliError> {
    super::aws_install::validate(path)
}

#[cfg(target_os = "linux")]
fn official_file() -> Result<(PathBuf, File), CliError> {
    let path = PathBuf::from("/usr/local/sessionmanagerplugin/bin").join(FILE);
    verify_linux_install(&path)?;
    let file = File::open(&path).map_err(|_| CliError::PluginMissing)?;
    Ok((path, file))
}

#[cfg(target_os = "linux")]
fn verify_linux_install(path: &Path) -> Result<(), CliError> {
    use std::os::unix::fs::MetadataExt;
    // 공식 패키지 경로의 모든 조상과 실행 파일은 root만 수정할 수 있어야 한다.
    for entry in path.ancestors() {
        let meta = entry
            .symlink_metadata()
            .map_err(|_| CliError::PluginMissing)?;
        if meta.uid() != 0
            || meta.mode() & 0o022 != 0
            || meta.file_type().is_symlink()
            || (entry == path && (!meta.is_file() || meta.mode() & 0o111 == 0))
            || (entry != path && !meta.is_dir())
        {
            return Err(CliError::PluginMissing);
        }
    }
    Ok(())
}

#[cfg(windows)]
fn official_file() -> Result<(PathBuf, File), CliError> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::UI::Shell::FOLDERID_ProgramFiles;
    let root = super::windows_acl::known_folder(&FOLDERID_ProgramFiles)
        .map_err(|_| CliError::PluginMissing)?;
    let file = root.join("Amazon/SessionManagerPlugin/bin").join(FILE);
    super::windows_acl::trusted_chain(&file, true).map_err(|_| CliError::PluginMissing)?;
    let file = file.canonicalize().map_err(|_| CliError::PluginMissing)?;
    // Windows allows the same executable to be opened for reading, but not written or deleted.
    let guard = OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&file)
        .map_err(|_| CliError::PluginMissing)?;
    Ok((file, guard))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn official_file() -> Result<(PathBuf, File), CliError> {
    Err(CliError::PluginMissing)
}

fn require_supported_version(output: &[u8], windows: bool) -> Result<(), CliError> {
    // AWS 지원 하한과 Windows 국제 키보드 수정: plugin-version-history.html
    let (minimum, label) = if windows {
        ([1, 2, 792, 0], "1.2.792.0")
    } else {
        ([1, 2, 764, 0], "1.2.764.0")
    };
    let version = std::str::from_utf8(output)
        .ok()
        .and_then(|version| numeric_version(version.trim()))
        .ok_or(CliError::InvalidResponse)?;
    if version < minimum {
        return Err(CliError::PluginUpgradeRequired { minimum: label });
    }
    Ok(())
}

pub(super) fn numeric_version(version: &str) -> Option<[u32; 4]> {
    let mut parts = [0u32; 4];
    let mut count = 0;
    for (i, part) in version.split('.').enumerate() {
        if i == parts.len() || part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let value = part.parse().ok()?;
        parts[i] = value;
        count += 1;
    }
    (count >= 3).then_some(parts)
}

#[cfg(not(windows))]
fn check_precedence(_: &Path, _: &Path, _: &Path) -> Result<(), CliError> {
    // POSIX execvp는 슬래시가 없는 이름을 전달받으면 설정한 PATH만 검색한다.
    Ok(())
}

#[cfg(windows)]
fn check_precedence(aws: &Path, cwd: &Path, plugin: &Path) -> Result<(), CliError> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::{
        GetSystemDirectoryW, GetWindowsDirectoryW,
    };

    fn directory(
        get: unsafe extern "system" fn(*mut u16, u32) -> u32,
    ) -> Result<PathBuf, CliError> {
        let mut buf = [0u16; 260];
        // 두 API는 길이를 UTF-16 코드 단위로 반환한다. 버퍼를 넘는 경로는 거부한다.
        let len = unsafe { get(buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if len == 0 || len >= buf.len() {
            return Err(CliError::PluginMissing);
        }
        Ok(OsString::from_wide(&buf[..len]).into())
    }

    let windows = directory(GetWindowsDirectoryW)?;
    let system = directory(GetSystemDirectoryW)?;
    // Python subprocess는 Windows에서 CreateProcessW에 비정규 이름을 넘긴다.
    // PATH보다 앞서는 위치에 다른 파일이 있거나 읽을 수 없으면 시작을 거부한다.
    check_candidates(
        [
            aws.parent().ok_or(CliError::PluginMissing)?,
            cwd,
            &system,
            &windows.join("System"),
            &windows,
        ],
        plugin,
    )
}

#[cfg(any(windows, test))]
fn check_candidates<'a>(
    dirs: impl IntoIterator<Item = &'a Path>,
    plugin: &Path,
) -> Result<(), CliError> {
    for dir in dirs {
        let candidate = dir.join(FILE);
        match candidate.symlink_metadata() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Ok(_) if candidate.canonicalize().ok().as_deref() == Some(plugin) => (),
            _ => return Err(CliError::PluginMissing),
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn version_and_shadow_candidates_fail_closed() {
        for (windows, old, minimum) in [
            (false, "1.2.763.99", "1.2.764.0"),
            (true, "1.2.791.99", "1.2.792.0"),
        ] {
            assert_eq!(require_supported_version(old.as_bytes(), windows), Err(CliError::PluginUpgradeRequired { minimum }));
            assert_eq!(require_supported_version(format!("{minimum}\r\n").as_bytes(), windows), Ok(()));
            assert_eq!(require_supported_version(b"1.2.896.0", windows), Ok(()));
        }
        for invalid in [
            b"".as_slice(),
            b"1.2. 900",
            b"1.2.764.0.1",
            b"\xff",
        ] {
            assert_eq!(require_supported_version(invalid, false), Err(CliError::InvalidResponse));
        }
        let chosen = tempfile::tempdir().unwrap();
        let earlier = tempfile::tempdir().unwrap();
        let file = chosen.path().join(FILE);
        std::fs::write(&file, b"fixture").unwrap();
        let file = file.canonicalize().unwrap();
        assert!(check_candidates([earlier.path(), chosen.path()], &file).is_ok());
        std::fs::write(earlier.path().join(FILE), b"shadow").unwrap();
        assert_eq!(
            check_candidates([earlier.path(), chosen.path()], &file),
            Err(CliError::PluginMissing)
        );
    }

    pub(in crate::ssm) fn fixture_plugin() -> CheckedPlugin {
        CheckedPlugin {
            file: std::env::current_exe().unwrap().into(),
            path: "fixture-plugin-path".into(),
            _file_guard: None,
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_held_file_denies_write_and_rename() {
        use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt};
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE);
        std::fs::write(&file, b"example").unwrap();
        let guard = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&file)
            .unwrap();
        assert!(std::fs::File::open(&file).is_ok());
        assert!(OpenOptions::new().write(true).open(&file).is_err());
        let renamed = dir.path().join("renamed.exe");
        assert!(std::fs::rename(&file, &renamed).is_err());
        drop(guard);
        std::fs::rename(&file, renamed).unwrap();
    }
}
