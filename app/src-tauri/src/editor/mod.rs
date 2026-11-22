//! 네이티브 UI가 승인한 절대 경로를 여는 내부 파일 경계.

#[cfg_attr(unix, path = "unix.rs")]
#[cfg_attr(windows, path = "windows.rs")]
mod os;
pub(crate) mod cli;
mod cli_open;
pub(crate) use cli_open::prepare_cli;
pub(crate) mod ipc;
pub(crate) mod registry;
pub(crate) mod sftp;
pub(crate) mod sftp_ipc;
pub(crate) mod storage_files;
#[cfg(test)]
mod tests;

use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use sha2::{Digest, Sha256};

static COMMIT: Mutex<()> = Mutex::new(());
const CLI_READ_LIMIT: usize = 8 * 1024 * 1024;

pub(crate) struct LocalFile {
    selected_parent: PathBuf,
    cli_path: bool,
    parent: os::Directory,
    name: OsString,
    version: Version,
    // 열린 파일을 붙들어 파일 ID가 탭의 수명 중 재사용되지 않게 한다.
    original: File,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LocalVersion {
    Text(String),
    NonUtf8(Vec<u8>),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LocalConflict {
    Modified(LocalVersion),
    Replaced(LocalVersion),
    Missing,
    PathChanged,
    NotRegular,
    ChangedDuringRead,
    PermissionsChanged,
}

#[derive(Debug)]
pub(crate) enum OpenError {
    InvalidPath,
    InvalidUtf8,
    Conflict(LocalConflict),
    Io(io::Error),
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum SaveStage {
    InspectOriginal,
    CreateTemporary,
    WriteTemporary,
    VerifyTemporary,
    PreservePermissions,
    SyncTemporary,
    Replace,
}

#[derive(Debug)]
pub(crate) enum SaveError {
    Conflict(LocalConflict),
    Io { stage: SaveStage, source: io::Error },
}

#[derive(PartialEq, Eq)]
struct Version {
    id: os::FileId,
    digest: [u8; 32],
}

impl LocalFile {
    /// `path`는 파일 선택기가 지정한다. 최종 심볼릭 링크는 거부한다.
    pub(crate) fn open(path: &Path) -> Result<(Self, String), OpenError> {
        Self::open_with(path, None)
    }

    fn open_cli(path: &Path, cancelled: &dyn Fn() -> bool) -> Result<(Self, String), OpenError> {
        if !path.to_str().is_some_and(|text| !text.contains('\0')) {
            return Err(OpenError::InvalidPath);
        }
        Self::open_with(path, Some(cancelled))
    }

    fn open_with(path: &Path, cancelled: Option<&dyn Fn() -> bool>) -> Result<(Self, String), OpenError> {
        let (Some(selected_parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(OpenError::InvalidPath);
        };
        if !path.is_absolute() || !os::valid_path(path) || !os::valid_name(name) {
            return Err(OpenError::InvalidPath);
        }
        let parent = if let Some(cancelled) = cancelled {
            check_cancelled(cancelled).map_err(OpenError::Io)?;
            os::Directory::open_cli(selected_parent).map_err(OpenError::Io)?
        } else {
            let canonical_parent = fs::canonicalize(selected_parent).map_err(OpenError::Io)?;
            if !os::valid_path(&canonical_parent) {
                return Err(OpenError::InvalidPath);
            }
            os::Directory::open(canonical_parent).map_err(OpenError::Io)?
        };
        let cli_path = cancelled.is_some();
        let (original, version, bytes) = snapshot(selected_parent, &parent, name, cli_path, cancelled)?;
        let text = String::from_utf8(bytes).map_err(|_| OpenError::InvalidUtf8)?;
        Ok((
            Self {
                selected_parent: selected_parent.into(),
                cli_path,
                parent,
                name: name.into(),
                version,
                original,
            },
            text,
        ))
    }

    fn display_name(&self) -> String {
        self.name.to_string_lossy().into_owned()
    }

    fn snapshot(&self) -> Result<(File, Version, Vec<u8>), OpenError> {
        snapshot(&self.selected_parent, &self.parent, &self.name, self.cli_path, None)
    }

    fn compare(&self) -> Result<String, OpenError> {
        let (_, _, bytes) = self.snapshot()?;
        String::from_utf8(bytes).map_err(|_| OpenError::InvalidUtf8)
    }

    fn reload(&mut self) -> Result<String, OpenError> {
        let (original, version, bytes) = self.snapshot()?;
        let text = String::from_utf8(bytes).map_err(|_| OpenError::InvalidUtf8)?;
        self.original = original;
        self.version = version;
        Ok(text)
    }

    /// 호출자가 버퍼를 소유한다. 충돌과 오류에서 버퍼와 저장 기준은 그대로 남는다.
    /// 해시 검사와 OS 교체는 별도 작업이다. 마지막 검사 직후의 외부 쓰기는 경합할 수 있다.
    pub(crate) fn save(&mut self, buffer: &str) -> Result<(), SaveError> {
        self.save_with_sync(buffer, File::sync_all)
    }

    fn save_with_sync(
        &mut self,
        buffer: &str,
        sync: impl FnOnce(&File) -> io::Result<()>,
    ) -> Result<(), SaveError> {
        use SaveStage::*;

        check_open_parent(&self.selected_parent, &self.parent, self.cli_path).map_err(save_read_error)?;
        let access = os::Access::read(&self.original).map_err(at(PreservePermissions))?;
        let mut temp = self.parent.stage(Some(&access)).map_err(at(CreateTemporary))?;
        let bytes = buffer.as_bytes();
        temp.file().write_all(bytes).map_err(at(WriteTemporary))?;
        access
            .finish_staging(temp.file())
            .map_err(at(PreservePermissions))?;
        sync(temp.file()).map_err(at(SyncTemporary))?;
        verify_bytes(temp.file(), bytes).map_err(at(VerifyTemporary))?;
        let id = os::file_id(temp.file()).map_err(at(VerifyTemporary))?;
        let digest = Sha256::digest(bytes).into();

        // 앱의 두 탭은 검사와 교체를 같은 순서로 실행한다.
        let _commit = COMMIT.lock().map_err(|_| {
            at(InspectOriginal)(io::Error::other("local editor save lock poisoned"))
        })?;
        let (current, version, disk_bytes) = self.snapshot().map_err(save_read_error)?;
        if self.version != version {
            let disk = match String::from_utf8(disk_bytes) {
                Ok(text) => LocalVersion::Text(text),
                Err(error) => LocalVersion::NonUtf8(error.into_bytes()),
            };
            let reason = if self.version.id != version.id {
                LocalConflict::Replaced(disk)
            } else {
                LocalConflict::Modified(disk)
            };
            return Err(SaveError::Conflict(reason));
        }
        if !access.matches(&current).map_err(at(PreservePermissions))? {
            return Err(SaveError::Conflict(LocalConflict::PermissionsChanged));
        }
        self.parent
            .check_replace_access(&self.name, &current)
            .map_err(at(Replace))?;
        if !access
            .matches(temp.file())
            .map_err(at(PreservePermissions))?
        {
            return Err(at(PreservePermissions)(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "temporary file access differs before replacement",
            )));
        }
        let file = temp.replace(&self.name).map_err(at(Replace))?;
        self.original = file;
        self.version = Version { id, digest };
        Ok(())
    }
}

fn at(stage: SaveStage) -> impl FnOnce(io::Error) -> SaveError {
    move |source| SaveError::Io { stage, source }
}

fn save_read_error(error: OpenError) -> SaveError {
    match error {
        OpenError::Conflict(reason) => SaveError::Conflict(reason),
        OpenError::Io(source) => at(SaveStage::InspectOriginal)(source),
        OpenError::InvalidPath | OpenError::InvalidUtf8 => unreachable!("snapshot reads raw bytes"),
    }
}

fn lookup_error(error: io::Error) -> OpenError {
    match error.kind() {
        io::ErrorKind::NotFound => OpenError::Conflict(LocalConflict::Missing),
        io::ErrorKind::NotADirectory => OpenError::Conflict(LocalConflict::PathChanged),
        _ => OpenError::Io(error),
    }
}

fn check_parent(selected: &Path, opened: &os::Directory) -> Result<(), OpenError> {
    let path = fs::canonicalize(selected).map_err(lookup_error)?;
    if path != opened.path || !opened.still_at_path().map_err(lookup_error)? {
        return Err(OpenError::Conflict(LocalConflict::PathChanged));
    }
    Ok(())
}

fn check_open_parent(selected: &Path, opened: &os::Directory, cli_path: bool) -> Result<(), OpenError> {
    if !cli_path {
        return check_parent(selected, opened);
    }
    if !opened.still_at_cli_path(selected).map_err(lookup_error)? {
        return Err(OpenError::Conflict(LocalConflict::PathChanged));
    }
    Ok(())
}

fn snapshot(
    selected_parent: &Path,
    parent: &os::Directory,
    name: &OsStr,
    cli_path: bool,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<(File, Version, Vec<u8>), OpenError> {
    let check_parent = || {
        if let Some(cancelled) = cancelled {
            check_cancelled(cancelled).map_err(OpenError::Io)?;
        }
        check_open_parent(selected_parent, parent, cli_path)
    };
    check_parent()?;
    let mut file = parent
        .open_file(name)
        .map_err(lookup_error)?
        .ok_or(OpenError::Conflict(LocalConflict::NotRegular))?;
    let id = os::file_id(&file).map_err(OpenError::Io)?;
    let before = file.metadata().map_err(OpenError::Io)?;
    let bytes = if let Some(cancelled) = cancelled {
        let bytes = read_cli_bytes(&mut file, before.len(), cancelled).map_err(OpenError::Io)?;
        check_cancelled(cancelled).map_err(OpenError::Io)?;
        bytes
    } else {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(OpenError::Io)?;
        bytes
    };
    let after = file.metadata().map_err(OpenError::Io)?;
    if before.len() != after.len()
        || after.len() != bytes.len() as u64
        || before.modified().map_err(OpenError::Io)? != after.modified().map_err(OpenError::Io)?
    {
        return Err(OpenError::Conflict(LocalConflict::ChangedDuringRead));
    }
    let digest = Sha256::digest(&bytes).into();
    let named_id = parent.entry_id(name).map_err(lookup_error)?;
    check_parent()?;
    match named_id {
        Some(named_id) if named_id == id => Ok((file, Version { id, digest }, bytes)),
        Some(_) => Err(OpenError::Conflict(LocalConflict::ChangedDuringRead)),
        None => Err(OpenError::Conflict(LocalConflict::NotRegular)),
    }
}

fn check_cancelled(cancelled: &dyn Fn() -> bool) -> io::Result<()> {
    if cancelled() { Err(io::ErrorKind::Interrupted.into()) } else { Ok(()) }
}

fn read_cli_bytes(
    reader: &mut impl Read,
    len: u64,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<Vec<u8>> {
    if len > CLI_READ_LIMIT as u64 {
        return Err(io::ErrorKind::FileTooLarge.into());
    }
    let mut bytes = Vec::with_capacity(len as usize);
    let mut chunk = [0; 32 * 1024];
    loop {
        check_cancelled(cancelled)?;
        let remaining = CLI_READ_LIMIT - bytes.len();
        // 한 바이트로 상한에 도달한 파일의 EOF와 읽는 중 증가를 구별한다.
        let chunk_len = remaining.max(1).min(chunk.len());
        let count = match reader.read(&mut chunk[..chunk_len]) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if count == 0 {
            return Ok(bytes);
        }
        if count > remaining {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        if bytes.len() + count > bytes.capacity() {
            // 파일이 늘어도 버퍼 용량 증가를 CLI 상한 내로 제한한다.
            let capacity = (bytes.capacity() * 2).max(bytes.len() + count).min(CLI_READ_LIMIT);
            bytes.reserve_exact(capacity - bytes.len());
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
}

fn verify_bytes(file: &mut File, expected: &[u8]) -> io::Result<()> {
    file.rewind()?;
    let mut actual = [0; 8192];
    for chunk in expected.chunks(actual.len()) {
        let read = &mut actual[..chunk.len()];
        file.read_exact(read)?;
        if read != chunk {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "temporary file mismatch",
            ));
        }
    }
    if file.read(&mut actual[..1])? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "temporary file has trailing bytes",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod cli_read_tests {
    use super::*;
    use std::{cell::Cell, io::repeat};

    #[test]
    fn cli_reads_at_most_the_limit_and_rejects_larger_metadata_before_reading() {
        let limit = CLI_READ_LIMIT as u64;
        let mut oversized = repeat(b'x').take(limit + 2);
        assert_eq!(
            read_cli_bytes(&mut oversized, limit + 1, &|| false).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge,
        );
        assert_eq!(oversized.limit(), limit + 2);

        assert_eq!(read_cli_bytes(&mut repeat(b'x').take(limit), limit, &|| false).unwrap().len(), CLI_READ_LIMIT);
        assert_eq!(
            read_cli_bytes(&mut oversized, 1, &|| false).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge,
        );
        assert_eq!(oversized.limit(), 1);
    }

    #[test]
    fn cli_stops_before_the_next_chunk_when_cancelled() {
        let calls = Cell::new(0);
        let mut input = repeat(b'x').take(CLI_READ_LIMIT as u64);
        let error = read_cli_bytes(&mut input, 0, &|| calls.replace(calls.get() + 1) != 0)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(input.limit(), (CLI_READ_LIMIT - 32 * 1024) as u64);
        assert_eq!(calls.get(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn cli_keeps_its_parent_policy_after_open_without_keeping_the_size_limit() {
        use std::os::windows::fs::symlink_dir;

        let sandbox = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(sandbox.path()).unwrap();
        let parent = root.join("parent");
        fs::create_dir(&parent).unwrap();
        let path = parent.join("edit.txt");
        fs::write(&path, "before").unwrap();
        let (mut file, _) = LocalFile::open_cli(&path, &|| false).unwrap();

        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(CLI_READ_LIMIT as u64 + 1)
            .unwrap();
        assert_eq!(file.compare().unwrap().len(), CLI_READ_LIMIT + 1);
        assert_eq!(file.reload().unwrap().len(), CLI_READ_LIMIT + 1);

        // 같은 파일을 가리키는 링크로 경로 재검증을 실행한다.
        let alias = root.join("alias");
        symlink_dir(&parent, &alias).unwrap();
        let (gui_file, _) = LocalFile::open(&alias.join("edit.txt")).unwrap();
        assert_eq!(gui_file.compare().unwrap().len(), CLI_READ_LIMIT + 1);
        file.selected_parent = alias;
        assert!(file.compare().is_err());
        assert!(file.reload().is_err());
        assert!(file.save("after").is_err());
        assert_eq!(fs::metadata(&path).unwrap().len(), CLI_READ_LIMIT as u64 + 1);
    }
}

fn temporary_name() -> io::Result<OsString> {
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(format!(".zzabba-edit-{:032x}.tmp", u128::from_ne_bytes(random)).into())
}
