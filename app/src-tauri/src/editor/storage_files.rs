use super::{COMMIT, OpenError, Version, check_parent, os, publication::PublicationError};
use sha2::{Digest, Sha256};
use std::{ffi::OsString, fs::{self, File}, io::{self, Read}, path::{Path, PathBuf}};

struct Location {
    selected_parent: PathBuf,
    parent: os::Directory,
    name: OsString,
}

impl Location {
    fn open(path: &Path) -> io::Result<Self> {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(io::ErrorKind::InvalidInput.into());
        };
        if !path.is_absolute() || !os::valid_path(path) || !os::valid_name(name) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let canonical = fs::canonicalize(parent)?;
        if !os::valid_path(&canonical) { return Err(io::ErrorKind::InvalidInput.into()); }
        let result = Self { selected_parent: parent.into(), parent: os::Directory::open_storage(canonical)?, name: name.into() };
        result.check_parent()?;
        Ok(result)
    }

    fn check_parent(&self) -> io::Result<()> {
        check_parent(&self.selected_parent, &self.parent).map_err(|error| match error {
            OpenError::Io(source) => source,
            _ => changed(),
        })
    }

    #[cfg(unix)]
    fn open_file(&self) -> io::Result<Option<File>> {
        self.parent.open_storage_file(&self.name)
    }

    #[cfg(windows)]
    fn open_file(&self) -> io::Result<Option<File>> {
        match self.parent.open_file(&self.name) {
            Ok(Some(file)) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Ok(None) => Err(io::ErrorKind::InvalidInput.into()),
            Err(error) => Err(error),
        }
    }

    fn check_file(&self, id: &os::FileId) -> io::Result<()> {
        self.check_parent()?;
        if self.parent.entry_id(&self.name)?.as_ref() != Some(id) { return Err(changed()); }
        Ok(())
    }
}

pub(crate) fn open_upload(path: &Path) -> io::Result<File> {
    upload_at(&Location::open(path)?)
}

fn upload_at(location: &Location) -> io::Result<File> {
    let file = location.open_file()?.ok_or(io::ErrorKind::NotFound)?;
    check_local_data(&file)?;
    location.check_file(&os::file_id(&file)?)?;
    Ok(file)
}

pub(crate) struct DownloadTarget {
    location: Location,
    original: Option<(File, Version)>,
}

impl DownloadTarget {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        Self::at(Location::open(path)?)
    }

    fn at(location: Location) -> io::Result<Self> {
        let original = if let Some(mut file) = location.open_file()? {
            check_local_data(&file)?;
            let version = version(&mut file)?;
            location.check_file(&version.id)?;
            Some((file, version))
        } else { None };
        Ok(Self { location, original })
    }

    pub(crate) fn replaces_existing(&self) -> bool { self.original.is_some() }

    pub(crate) fn stage(&self) -> io::Result<DownloadStage<'_>> {
        self.location.check_parent()?;
        let access = self.original.as_ref().map(|(file, _)| os::Access::read(file)).transpose()?;
        Ok(DownloadStage { target: self, temp: self.location.parent.stage(access.as_ref())? })
    }
}

pub(crate) struct DownloadStage<'a> {
    target: &'a DownloadTarget,
    temp: os::Temporary<'a>,
}

impl DownloadStage<'_> {
    pub(crate) fn writer(&mut self) -> io::Result<File> { self.temp.file().try_clone() }

    pub(crate) fn commit(self, closed: &tokio::sync::watch::Sender<bool>, digest: [u8; 32]) -> Result<(), PublicationError> {
        self.commit_inner(closed, digest, || {})
    }

    fn commit_inner(mut self, closed: &tokio::sync::watch::Sender<bool>, digest: [u8; 32], before_publish: impl FnOnce()) -> Result<(), PublicationError> {
        let target = &self.target.location;
        let _commit = COMMIT.lock().map_err(|_| io::Error::other("local file commit lock poisoned"))?;
        if let Some((original, _)) = &self.target.original {
            os::Access::read(original)?.finish_staging(self.temp.file())?;
        }
        self.temp.file().sync_all()?;
        if version(self.temp.file())?.digest != digest { return Err(changed().into()); }
        // 내려받은 파일은 인터넷 영역으로 표시한다. 표시 실패는 다운로드 실패로 만들지 않지만,
        // 교체할 원본에 표시가 있었으면(또는 확인할 수 없으면) 표시 없는 파일로 바꾸지 않는다.
        #[cfg(windows)]
        if let Err(error) = self.temp.mark_internet() {
            if self.target.original.as_ref().is_some_and(|(original, _)| !matches!(os::read_zone(original), Ok(None))) {
                return Err(error.into());
            }
            eprintln!("S3 다운로드 영역 표시 실패: {error}");
        }
        target.check_parent()?;
        let replacement = if let Some((original, selected)) = &self.target.original {
            let mut current = target.open_file()?.ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            check_local_data(&current)?;
            let access = os::Access::read(original)?;
            if !access.matches(self.temp.file())? { return Err(changed().into()); }
            let verify = move |file: &mut File| {
                if version(file)? != *selected || !access.matches(file)? { return Err(changed()); }
                target.check_parent()
            };
            verify(&mut current)?;
            target.parent.check_replace_access(&target.name, &current)?;
            target.check_file(&selected.id)?;
            Some((current, verify))
        } else {
            None
        };
        let live = closed.borrow();
        if *live { return Err(io::Error::from(io::ErrorKind::Interrupted).into()); }
        before_publish();
        if let Some((current, verify)) = replacement {
            self.temp.publish_download(&target.name, &current, verify)
        } else {
            Ok(self.temp.install(&target.name, false)?)
        }
    }
}

fn version(file: &mut File) -> io::Result<Version> {
    use std::io::Seek;
    file.rewind()?;
    let id = os::file_id(file)?;
    let before = file.metadata()?;
    let mut hash = Sha256::new();
    let read = io::copy(file, &mut hash)?;
    let after = file.metadata()?;
    if before.len() != read || after.len() != read || before.modified()? != after.modified()? {
        return Err(changed());
    }
    Ok(Version { id, digest: hash.finalize().into() })
}

/// 네이티브 폴더 대화상자로 승인한 S3 로컬 창의 루트다. 경로는 정규화한 절대 경로다.
pub(crate) struct LocalRoot {
    directory: os::Directory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum LocalKind {
    Folder,
    File,
    Link,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalEntry {
    pub(crate) name: String,
    pub(crate) kind: LocalKind,
    pub(crate) bytes: Option<u64>,
    pub(crate) modified: Option<u64>,
    /// 클라우드 전용 파일(데이터 회수 필요)이다. 업로드할 수 없다.
    pub(crate) offline: bool,
}

/// 루트 밖을 가리키는 링크·재분석 지점 또는 경로 별칭을 거부할 때의 오류 원인이다.
#[derive(Debug)]
pub(crate) struct OutsideRoot;

impl std::fmt::Display for OutsideRoot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("link or path outside the approved root")
    }
}

impl std::error::Error for OutsideRoot {}

pub(crate) fn outside_root() -> io::Error { io::Error::new(io::ErrorKind::PermissionDenied, OutsideRoot) }

pub(crate) fn is_outside_root(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|source| source.is::<OutsideRoot>())
}

/// 승인한 루트가 옮겨지거나 이름이 바뀌어 같은 경로에 없을 때의 오류 원인이다.
#[derive(Debug)]
pub(crate) struct RootChanged;

impl std::fmt::Display for RootChanged {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("approved root moved or renamed")
    }
}

impl std::error::Error for RootChanged {}

pub(crate) fn is_root_changed(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|source| source.is::<RootChanged>())
}

/// 데이터가 로컬에 없는 클라우드 전용 파일을 열지 않을 때의 오류 원인이다.
#[derive(Debug)]
pub(crate) struct CloudOnly;

impl std::fmt::Display for CloudOnly {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("cloud-only file")
    }
}

impl std::error::Error for CloudOnly {}

pub(crate) fn is_cloud_only(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|source| source.is::<CloudOnly>())
}

/// 파일 데이터를 읽으려면 클라우드에서 회수해야 하는 속성이다.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn needs_recall(attributes: u32) -> bool {
    const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
    attributes & (FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) != 0
}

fn offline(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        needs_recall(metadata.file_attributes())
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

fn cloud_only() -> io::Error { io::Error::new(io::ErrorKind::Unsupported, CloudOnly) }

/// 연 핸들의 속성(GetFileInformationByHandle)으로 데이터가 로컬에 있는지 다시 확인한다. 데이터를 읽기 전에 부른다.
pub(crate) fn check_local_data(file: &File) -> io::Result<()> {
    if offline(&file.metadata()?) { return Err(cloud_only()); }
    Ok(())
}

/// 이름이 클라우드 전용 파일이면 거부한다. 없거나 읽을 수 없으면 이후 열기 검사에 맡긴다.
fn reject_offline(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && offline(&metadata) => Err(cloud_only()),
        _ => Ok(()),
    }
}

/// Windows 장치 이름이다. 확장자 앞부분을 대소문자 구분 없이 비교한다.
fn reserved_device(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ').to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit())
}

/// 루트 기준 상대 경로의 한 구간이다. 구분자·드라이브·대체 데이터 스트림·NUL·`.`·`..`를 거부한다.
pub(crate) fn valid_local_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".."
        && !name.contains(['/', '\\', ':', '\0'])
        && !(cfg!(windows) && (name.ends_with('.') || name.ends_with(' ') || reserved_device(name)
            || name.contains(['<', '>', '"', '|', '?', '*']) || name.chars().any(char::is_control)))
        && os::valid_name(name.as_ref())
}

/// `a/b/c` 형식의 상대 경로를 구간으로 나눈다. 빈 문자열은 루트다.
pub(crate) fn local_names(relative: &str) -> io::Result<Vec<&str>> {
    if relative.is_empty() { return Ok(Vec::new()); }
    if relative.len() > 4096 { return Err(io::ErrorKind::InvalidInput.into()); }
    let names: Vec<_> = relative.split('/').collect();
    if !names.iter().all(|name| valid_local_name(name)) { return Err(io::ErrorKind::InvalidInput.into()); }
    Ok(names)
}

/// 링크·재분석 지점이면 참이다. Windows의 클라우드 자리 표시자는 링크가 아니다.
fn is_link(path: &Path, metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return !os::cloud_placeholder(path).unwrap_or(false);
        }
    }
    #[cfg(not(windows))]
    let _ = path;
    metadata.file_type().is_symlink()
}

impl LocalRoot {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() { return Err(io::ErrorKind::InvalidInput.into()); }
        let canonical = fs::canonicalize(path)?;
        if !os::valid_path(&canonical) { return Err(io::ErrorKind::InvalidInput.into()); }
        Ok(Self { directory: os::Directory::open_storage(canonical)? })
    }

    pub(crate) fn path(&self) -> &Path { &self.directory.path }

    fn check(&self) -> io::Result<()> {
        match self.directory.still_at_path() {
            Ok(true) => Ok(()),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, RootChanged)),
        }
    }

    /// 모든 구간이 링크가 아닌 실제 디렉터리이고 정규화 결과가 같은 경로일 때만 반환한다.
    fn directory(&self, names: &[&str]) -> io::Result<PathBuf> {
        self.check()?;
        let mut path = self.directory.path.clone();
        for name in names {
            path.push(name);
            let metadata = fs::symlink_metadata(&path)?;
            if is_link(&path, &metadata) { return Err(outside_root()); }
            if !metadata.is_dir() { return Err(io::ErrorKind::NotADirectory.into()); }
        }
        // 짧은 이름·대소문자 별칭과 검사 뒤 바뀐 상위 구간도 정규화 비교로 거부한다.
        if fs::canonicalize(&path)? != path || !path.starts_with(&self.directory.path) { return Err(outside_root()); }
        Ok(path)
    }

    fn locate(&self, directory: &Path, name: &str) -> io::Result<Location> {
        if !valid_local_name(name) { return Err(io::ErrorKind::InvalidInput.into()); }
        let location = Location::open(&directory.join(name))?;
        if location.parent.path != directory { return Err(outside_root()); }
        self.check()?;
        Ok(location)
    }

    pub(crate) fn list(&self, relative: &str, limit: usize) -> io::Result<(Vec<LocalEntry>, bool)> {
        let directory = self.directory(&local_names(relative)?)?;
        let mut entries = Vec::new();
        let mut truncated = false;
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str().filter(|name| valid_local_name(name)) else { continue };
            // 항목 자체의 정보만 읽고 링크는 따라가지 않는다.
            let Ok(metadata) = entry.metadata() else { continue };
            let kind = if is_link(&entry.path(), &metadata) {
                LocalKind::Link
            } else if metadata.is_dir() {
                LocalKind::Folder
            } else if metadata.is_file() {
                LocalKind::File
            } else {
                continue;
            };
            if entries.len() == limit {
                truncated = true;
                break;
            }
            let modified = metadata.modified().ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|time| u64::try_from(time.as_millis()).ok());
            let file = kind == LocalKind::File;
            entries.push(LocalEntry {
                name: name.into(), kind, bytes: file.then(|| metadata.len()), modified, offline: file && offline(&metadata),
            });
        }
        if fs::canonicalize(&directory)? != directory { return Err(outside_root()); }
        self.check()?;
        Ok((entries, truncated))
    }

    /// 루트 안의 일반 파일을 링크를 따라가지 않고 연다.
    pub(crate) fn open_upload(&self, relative: &str) -> io::Result<File> {
        let names = local_names(relative)?;
        let (name, parent) = names.split_last().ok_or(io::ErrorKind::InvalidInput)?;
        let directory = self.directory(parent)?;
        reject_offline(&directory.join(name))?;
        let location = self.locate(&directory, name)?;
        upload_at(&location)
    }

    /// 루트 안의 폴더에 같은 게시 규칙으로 내려받을 대상을 연다.
    pub(crate) fn download_target(&self, folder: &str, name: &str) -> io::Result<DownloadTarget> {
        let directory = self.directory(&local_names(folder)?)?;
        if valid_local_name(name) { reject_offline(&directory.join(name))?; }
        DownloadTarget::at(self.locate(&directory, name)?)
    }
}

fn changed() -> io::Error { io::Error::new(io::ErrorKind::AlreadyExists, "selected file or directory changed") }

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[cfg(windows)]
    #[test]
    fn editor_save_keeps_zone_identifier_and_rejects_other_streams() {
        let stream = |path: &Path, name: &str| {
            let mut stream = path.as_os_str().to_owned();
            stream.push(name);
            PathBuf::from(stream)
        };
        let dir = tempfile::tempdir().unwrap();
        let zone = b"[ZoneTransfer]\r\nZoneId=3\r\nReferrerUrl=about:internet\r\n";

        // 내려받은 파일을 편집기로 저장해도 같은 영역 표시가 남는다.
        let path = dir.path().join("받은 파일.txt");
        fs::write(&path, b"old").unwrap();
        fs::write(stream(&path, ":Zone.Identifier"), zone).unwrap();
        let (mut editor, _) = super::super::LocalFile::open(&path).unwrap();
        editor.save("new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::read(stream(&path, ":Zone.Identifier")).unwrap(), zone);
        editor.save("again").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"again");
        assert_eq!(fs::read(stream(&path, ":Zone.Identifier")).unwrap(), zone);

        // 다른 스트림이 있거나 영역 표시가 상한을 넘으면 저장하지 않는다.
        for (name, extra) in [("other.txt", ":other"), ("large.txt", ":Zone.Identifier")] {
            let path = dir.path().join(name);
            fs::write(&path, b"old").unwrap();
            fs::write(stream(&path, ":Zone.Identifier"), zone).unwrap();
            let content = if extra == ":other" { b"other".to_vec() } else { vec![b'z'; 4097] };
            fs::write(stream(&path, extra), &content).unwrap();
            if let Ok((mut editor, _)) = super::super::LocalFile::open(&path) {
                assert!(editor.save("new").is_err(), "{name}");
            }
            assert_eq!(fs::read(&path).unwrap(), b"old", "{name}");
            assert_eq!(fs::read(stream(&path, extra)).unwrap(), content, "{name}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn storage_cloud_placeholder_tags_are_not_links() {
        for tag in [0x9000_001A, 0x9000_101A, 0x9000_701A, 0x9000_F01A] {
            assert!(os::is_cloud_tag(tag), "{tag:#x}");
        }
        // 심볼릭 링크, 마운트 지점(junction), 앱 실행 별칭, 이전 OneDrive 태그, 태그 없음
        for tag in [0xA000_000C, 0xA000_0003, 0x8000_001B, 0x8000_0021, 0x9000_001B, 0] {
            assert!(!os::is_cloud_tag(tag), "{tag:#x}");
        }
    }

    #[test]
    fn storage_upload_reads_original_after_same_name_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chosen.bin");
        fs::write(&path, b"original").unwrap();
        let mut upload = open_upload(&path).unwrap();
        let moved = dir.path().join("moved.bin");
        fs::rename(&path, &moved).unwrap();
        fs::write(&path, b"replacement").unwrap();
        let mut body = Vec::new();
        upload.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"original");
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
    }

    #[cfg(unix)]
    #[test]
    fn storage_download_unix_accepts_absent_only_and_never_clobbers() {
        use std::os::unix::fs::symlink;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let closed = tokio::sync::watch::Sender::new(false);
        let path = dir.path().join("new.bin");
        let approval = DownloadTarget::open(&path).unwrap();
        assert!(!approval.replaces_existing());
        let transfer = |approval: &DownloadTarget| {
            let mut stage = approval.stage().unwrap();
            stage.writer().unwrap().write_all(b"object").unwrap();
            stage.commit(&closed, Sha256::digest(b"object").into())
        };
        transfer(&approval).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"object");
        assert!(DownloadTarget::open(&path).unwrap().replaces_existing());

        let late = dir.path().join("late.bin");
        let approval = DownloadTarget::open(&late).unwrap();
        fs::write(&late, b"external").unwrap();
        assert!(matches!(transfer(&approval), Err(PublicationError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists));
        assert_eq!(fs::read(&late).unwrap(), b"external");

        let link = dir.path().join("link.bin");
        let dangling = dir.path().join("dangling.bin");
        let directory = dir.path().join("directory");
        let fifo = dir.path().join("fifo");
        symlink(&path, &link).unwrap();
        symlink(dir.path().join("absent"), &dangling).unwrap();
        fs::create_dir(&directory).unwrap();
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: the name is NUL-terminated and belongs to the isolated fixture.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        for path in [link, dangling, directory, fifo] {
            assert_eq!(DownloadTarget::open(&path).err().unwrap().kind(), io::ErrorKind::InvalidInput);
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 6);
    }

    #[test]
    fn storage_download_preserves_changes_since_selection() {
        for action in ["create", "edit", "in-place", "delete", "replace", "parent", "cancel"] {
            let dir = tempfile::tempdir().unwrap();
            let parent = dir.path().join("parent");
            fs::create_dir(&parent).unwrap();
            let path = parent.join("chosen.bin");
            if action != "create" { fs::write(&path, b"original").unwrap(); }
            let target = DownloadTarget::open(&path).unwrap();
            let mut stage = target.stage().unwrap();
            stage.writer().unwrap().write_all(b"download").unwrap();
            let closed = tokio::sync::watch::Sender::new(action == "cancel");
            let moved_parent = dir.path().join("old-parent");
            let expected = match action {
                "create" => { fs::write(&path, b"unseen").unwrap(); Some(&b"unseen"[..]) }
                "edit" => {
                    let (mut editor, _) = super::super::LocalFile::open(&path).unwrap();
                    editor.save("editor changes").unwrap();
                    Some(&b"editor changes"[..])
                }
                "delete" => { fs::remove_file(&path).unwrap(); None }
                "replace" => {
                    fs::rename(&path, parent.join("previous.bin")).unwrap();
                    fs::write(&path, b"original").unwrap();
                    Some(&b"original"[..])
                }
                "in-place" => { fs::write(&path, b"external").unwrap(); Some(&b"external"[..]) }
                "parent" => {
                    match fs::rename(&parent, &moved_parent) {
                        Ok(()) => {}
                        Err(error) if cfg!(windows) && error.kind() == io::ErrorKind::PermissionDenied => {
                            assert_eq!(fs::read(&path).unwrap(), b"original");
                            drop(stage);
                            assert_eq!(fs::read_dir(&parent).unwrap().count(), 1);
                            continue;
                        }
                        Err(error) => panic!("parent rename: {error}"),
                    }
                    fs::create_dir(&parent).unwrap();
                    fs::write(&path, b"other parent").unwrap();
                    Some(&b"other parent"[..])
                }
                _ => Some(&b"original"[..]),
            };
            assert!(stage.commit(&closed, Sha256::digest(b"download").into()).is_err(), "{action}");
            if let Some(expected) = expected {
                assert_eq!(fs::read(&path).unwrap(), expected, "{action}");
            } else { assert!(!path.exists()); }
            if action == "parent" {
                assert_eq!(fs::read(moved_parent.join("chosen.bin")).unwrap(), b"original");
                assert_eq!(fs::read_dir(&moved_parent).unwrap().count(), 1);
            }
            assert_eq!(fs::read_dir(&parent).unwrap().count(), usize::from(expected.is_some()) + usize::from(action == "replace"));
        }
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn storage_download_preserves_late_destination_changes() {
        for action in ["create", "in-place", "replace"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("chosen.bin");
            let moved = dir.path().join("original.bin");
            if action != "create" { fs::write(&path, b"original").unwrap(); }
            let target = DownloadTarget::open(&path).unwrap();
            let mut stage = target.stage().unwrap();
            stage.writer().unwrap().write_all(b"download").unwrap();
            let closed = tokio::sync::watch::Sender::new(false);
            let result = stage.commit_inner(&closed, Sha256::digest(b"download").into(), || {
                if action == "replace" { fs::rename(&path, &moved).unwrap(); }
                fs::write(&path, b"external").unwrap();
            });
            if cfg!(target_os = "linux") && action != "create" {
                let Err(PublicationError::Unconfirmed(recovery)) = result else {
                    panic!("{action}: expected a recovery path, got {result:?}");
                };
                assert!(!path.exists(), "{action}");
                assert_eq!(fs::read(&recovery).unwrap(), b"external", "{action}");
                assert_eq!(fs::read_dir(recovery.parent().unwrap()).unwrap().count(), 1);
            } else {
                let Err(PublicationError::Io(error)) = result else {
                    panic!("{action}: expected a local IO error, got {result:?}");
                };
                if action == "create" {
                    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
                }
                assert_eq!(fs::read(&path).unwrap(), b"external", "{action}");
            }
            if action == "replace" { assert_eq!(fs::read(&moved).unwrap(), b"original"); }
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1 + usize::from(action == "replace"));
        }
    }
}
