use super::{CancellationToken, CliError, CommandSpec, Tool};
#[cfg(windows)]
use std::path::PathBuf;
use std::{ffi::OsString, path::Path};

const FILE: &str = if cfg!(windows) {
    "session-manager-plugin.exe"
} else {
    "session-manager-plugin"
};

pub(super) struct CheckedPlugin {
    file: std::sync::Arc<Path>,
    path: OsString,
}

impl CheckedPlugin {
    pub(super) fn path(&self) -> &std::ffi::OsStr {
        &self.path
    }

    pub(super) fn recheck(&self, aws: &Path, cwd: &Path) -> Result<(), CliError> {
        if self
            .file
            .parent()
            .and_then(|dir| dir.join(FILE).canonicalize().ok())
            .as_deref()
            != Some(self.file.as_ref())
        {
            return Err(CliError::PluginMissing);
        }
        check_precedence(aws, cwd, &self.file)
    }
}

pub(super) async fn checked_path(
    aws: &Path,
    cwd: &Path,
    cancel: &CancellationToken,
) -> Result<CheckedPlugin, CliError> {
    let plugin = Tool::Plugin.resolve()?;
    // PATH의 항목을 재구성하지 말고 검증한 파일의 디렉터리만 넘긴다.
    let dir = plugin.parent().ok_or(CliError::PluginMissing)?;
    if dir.join(FILE).canonicalize().ok().as_deref() != Some(plugin.as_ref()) {
        return Err(CliError::PluginMissing);
    }
    check_precedence(aws, cwd, &plugin)?;
    let output = CommandSpec::version(Tool::Plugin, plugin.clone())
        .capture(cancel)
        .await?
        .success()?;
    if !supports_environment_response(&output) {
        return Err(CliError::PluginMissing);
    }
    let path = std::env::join_paths([dir]).map_err(|_| CliError::PluginMissing)?;
    Ok(CheckedPlugin { file: plugin, path })
}

fn supports_environment_response(output: &[u8]) -> bool {
    let Ok(version) = std::str::from_utf8(output) else {
        return false;
    };
    let mut parts = [0u32; 4];
    let mut count = 0;
    for (i, part) in version.trim().split('.').enumerate() {
        if i == parts.len() || part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        let Ok(value) = part.parse() else {
            return false;
        };
        parts[i] = value;
        count += 1;
    }
    count > 0 && parts > [1, 2, 497, 0]
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
        assert!(supports_environment_response(b"1.2.497.1\r\n"));
        for invalid in [
            b"1.2.497.0".as_slice(),
            b"1.2. 900",
            b"1.2.764.0.1",
            b"\xff",
        ] {
            assert!(!supports_environment_response(invalid));
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
        }
    }
}
