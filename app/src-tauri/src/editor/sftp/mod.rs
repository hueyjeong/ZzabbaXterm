use std::{fmt::Write as _, sync::Arc};

use russh_sftp::{
    client::{RawSftpSession, error::Error as ProtocolError},
    protocol::{FileAttributes, OpenFlags, SftpHandle, StatusCode},
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::ssh::ConnectionPlan;

mod pipe;
pub(crate) use pipe::Session;

const POSIX_RENAME: &str = "posix-rename@openssh.com";
const FILE_LIMIT: usize = 16 * 1024 * 1024;
const ENTRY_LIMIT: usize = 16_384;
const PATH_LIMIT: usize = 4096;
const CHUNK: usize = 16 * 1024;

type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
pub(crate) enum Error {
    Terminal(String),
    Unavailable,
    Spawn(std::io::Error),
    Prepare { cause: Box<Error>, status: std::process::ExitStatus, stderr: String },
    Reap(std::io::Error),
    Disconnected,
    TimedOut,
    Protocol(ProtocolError),
    InvalidReply,
    UnsupportedVersion(u32),
    InvalidPath,
    PathChanged,
    ReadChanged,
    NotRegular,
    InvalidUtf8,
    LimitExceeded,
    MissingPermissions,
    UnsafePermissions,
    PermissionMismatch,
    WrongConnectionPlan,
    ReopenRequired,
    AtomicReplaceUnavailable,
    Random(getrandom::Error),
}

impl From<ProtocolError> for Error {
    fn from(error: ProtocolError) -> Self {
        Self::Protocol(error)
    }
}

pub(crate) struct Entry {
    name: String,
    path: String,
    is_directory: bool,
    directory: Arc<ListedDirectory>,
}

impl Entry {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn is_directory(&self) -> bool {
        self.is_directory
    }
}

struct ListedDirectory {
    plan: Arc<ConnectionPlan>,
    access_path: String,
    canonical_path: String,
}

impl ListedDirectory {
    async fn check(&self, connection: &Connection) -> Result<()> {
        if !Arc::ptr_eq(&self.plan, &connection.plan) {
            return Err(Error::WrongConnectionPlan);
        }
        if connection.canonical(&self.access_path).await? != self.canonical_path {
            return Err(Error::PathChanged);
        }
        if self.access_path != self.canonical_path
            && connection.canonical(&self.canonical_path).await? != self.canonical_path
        {
            return Err(Error::PathChanged);
        }
        Ok(())
    }
}

pub(crate) struct File {
    directory: Arc<ListedDirectory>,
    path: String,
    baseline: Revision,
    unconfirmed: bool,
}

#[derive(Debug)]
pub(crate) enum RemoteVersion {
    Text(String),
    Missing,
    NotRegular,
    NotUtf8,
    TooLarge,
    DirectoryChanged,
    ReadChanged,
}

#[derive(Debug)]
pub(crate) enum SaveStage {
    Prepare,
    Create,
    Write,
    Permissions,
    Close,
    Recheck,
    Replace,
}

#[derive(Debug)]
pub(crate) enum Cleanup {
    Removed,
    Failed(Error),
    SkippedDirectoryChanged,
}

#[derive(Debug)]
pub(crate) enum SaveOutcome {
    Saved,
    Conflict {
        remote: RemoteVersion,
        cleanup: Cleanup,
    },
    Failed {
        stage: SaveStage,
        error: Error,
        cleanup: Option<Cleanup>,
    },
    Unconfirmed(Error),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Access {
    mode: u32,
    uid: Option<u32>,
    gid: Option<u32>,
}

impl Access {
    fn regular(attrs: &FileAttributes) -> Result<Self> {
        let mode = attrs.permissions.ok_or(Error::MissingPermissions)?;
        if mode & 0o170000 != 0o100000 {
            return Err(Error::NotRegular);
        }
        Ok(Self {
            mode: mode & 0o7777,
            uid: attrs.uid,
            gid: attrs.gid,
        })
    }
}

#[derive(PartialEq, Eq)]
struct Revision {
    hash: [u8; 32],
    access: Access,
}

impl Revision {
    fn new(bytes: &[u8], access: Access) -> Self {
        Self {
            hash: Sha256::digest(bytes).into(),
            access,
        }
    }
}

struct Connection {
    channel: RawSftpSession,
    plan: Arc<ConnectionPlan>,
    atomic_replace: bool,
}

impl Connection {
    async fn from_subsystem(
        plan: Arc<ConnectionPlan>,
        stdin: impl AsyncWrite + Unpin + Send + 'static,
        stdout: impl AsyncRead + Unpin + Send + 'static,
        protocol_timeout_secs: u64,
    ) -> Result<Self> {
        let channel = RawSftpSession::new(tokio::io::join(stdout, stdin));
        channel.set_timeout(protocol_timeout_secs);
        let hello = channel.init().await?;
        if hello.version != 3 {
            return Err(Error::UnsupportedVersion(hello.version));
        }
        let atomic_replace = hello.extensions.get(POSIX_RENAME).is_some_and(|v| v == "1");
        Ok(Self {
            channel,
            plan,
            atomic_replace,
        })
    }

    /// None은 서버의 시작 디렉터리다. 명시적 절대 경로는 Rust가 선택한다.
    pub(crate) async fn list(&mut self, directory: Option<&str>) -> Result<Vec<Entry>> {
        if let Some(dir) = directory {
            validate_absolute(dir)?;
        }
        let access_path = directory.unwrap_or(".");
        let directory = Arc::new(ListedDirectory {
            plan: Arc::clone(&self.plan),
            canonical_path: self.canonical(access_path).await?,
            access_path: access_path.to_owned(),
        });
        self.list_bound(directory).await
    }

    pub(crate) async fn list_directory(&mut self, entry: &Entry) -> Result<Vec<Entry>> {
        if !entry.is_directory {
            return Err(Error::InvalidPath);
        }
        entry.directory.check(self).await?;
        let directory = Arc::new(ListedDirectory {
            plan: Arc::clone(&entry.directory.plan),
            access_path: child_path(&entry.directory.access_path, &entry.name),
            canonical_path: entry.path.clone(),
        });
        directory.check(self).await?;
        self.list_bound(directory).await
    }

    async fn list_bound(&self, directory: Arc<ListedDirectory>) -> Result<Vec<Entry>> {
        let handle = self
            .channel
            .opendir(directory.canonical_path.as_str())
            .await?
            .handle;
        let result = self.list_entries(&directory, &handle).await;
        let closed = self.channel.close(handle).await;
        let entries = result?;
        closed?;
        directory.check(self).await?;
        Ok(entries)
    }

    async fn list_entries(
        &self,
        directory: &Arc<ListedDirectory>,
        handle: &SftpHandle,
    ) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        let mut seen = 0;
        loop {
            match self.channel.readdir(handle).await {
                Ok(reply) if !reply.files.is_empty() => {
                    seen += reply.files.len();
                    if seen > ENTRY_LIMIT {
                        return Err(Error::LimitExceeded);
                    }
                    for entry in reply.files {
                        let Ok(name) = entry.filename.into_string() else {
                            continue;
                        };
                        if !valid_component(&name) || name.starts_with(".zzabba-edit-") {
                            continue;
                        }
                        let is_directory = entry.attrs.is_dir();
                        if is_directory || entry.attrs.is_regular() {
                            let path = child_path(&directory.canonical_path, &name);
                            validate_absolute(&path)?;
                            entries.push(Entry {
                                name,
                                path,
                                is_directory,
                                directory: Arc::clone(directory),
                            });
                        }
                    }
                }
                Ok(_) => return Err(Error::InvalidReply),
                Err(error) if status(&error, StatusCode::Eof) => return Ok(entries),
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Rust에 보관한 목록 항목을 받는다. WebView 입력으로 항목을 재구성하지 않는다.
    pub(crate) async fn open(&mut self, entry: &Entry) -> Result<(File, String)> {
        if entry.is_directory {
            return Err(Error::NotRegular);
        }
        self.open_bound(&entry.directory, &entry.path).await
    }

    /// 결과 미확인이나 충돌 이후 같은 연결 계획과 경로에서 새 기준본을 읽는다.
    pub(crate) async fn reopen(&mut self, file: &File) -> Result<(File, String)> {
        self.open_bound(&file.directory, &file.path).await
    }

    async fn open_bound(
        &self,
        directory: &Arc<ListedDirectory>,
        path: &str,
    ) -> Result<(File, String)> {
        let (bytes, access) = self.read_path(directory, path).await?;
        let baseline = Revision::new(&bytes, access);
        let text = String::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)?;
        Ok((
            File {
                path: path.to_owned(),
                directory: Arc::clone(directory),
                baseline,
                unconfirmed: false,
            },
            text,
        ))
    }

    pub(crate) async fn save(&mut self, file: &mut File, edited: &str) -> SaveOutcome {
        if let Err(error) = self.check_save(file, edited) {
            return failed(SaveStage::Prepare, error, None);
        }
        let dir = file.directory.canonical_path.as_str();
        if let Err(error) = file.directory.check(self).await {
            return failed(SaveStage::Prepare, error, None);
        }
        let temp = match temporary_path(dir) {
            Ok(path) => path,
            Err(error) => return failed(SaveStage::Create, error, None),
        };
        let handle = match self
            .channel
            .open(
                temp.as_str(),
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
                FileAttributes {
                    permissions: Some(0o600),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(reply) => reply.handle,
            // 배타적 생성 성공 응답을 받기 전에는 이 이름의 소유권을 알 수 없다.
            Err(error) => return failed(SaveStage::Create, error.into(), None),
        };
        let upload = self
            .upload(&handle, edited.as_bytes(), file.baseline.access)
            .await;
        let closed = self.channel.close(handle).await;
        if let Err((stage, error)) = upload {
            return failed(stage, error, Some(self.cleanup(dir, &temp).await));
        }
        if let Err(error) = closed {
            return failed(
                SaveStage::Close,
                error.into(),
                Some(self.cleanup(dir, &temp).await),
            );
        }
        match self.recheck(file).await {
            Ok(None) => (),
            Ok(Some(remote)) => {
                return SaveOutcome::Conflict {
                    remote,
                    cleanup: self.cleanup(dir, &temp).await,
                };
            }
            Err(error) => {
                return failed(
                    SaveStage::Recheck,
                    error,
                    Some(self.cleanup(dir, &temp).await),
                );
            }
        }
        // 해시 확인과 교체는 서로 다른 요청이므로 이 사이의 외부 쓰기를 막을 수 없다.
        // 응답 대기 중 호출이 취소되어도 다시 열기 전에는 재시도하지 않는다.
        file.unconfirmed = true;
        match self
            .channel
            .posix_rename(temp.as_str(), file.path.as_str())
            .await
        {
            Ok(_) => {
                file.unconfirmed = false;
                file.baseline = Revision::new(edited.as_bytes(), file.baseline.access);
                SaveOutcome::Saved
            }
            Err(ProtocolError::Status(status))
                if matches!(
                    status.status_code,
                    StatusCode::NoSuchFile
                        | StatusCode::PermissionDenied
                        | StatusCode::Failure
                        | StatusCode::BadMessage
                        | StatusCode::OpUnsupported
                ) =>
            {
                file.unconfirmed = false;
                let error = if status.status_code == StatusCode::OpUnsupported {
                    Error::AtomicReplaceUnavailable
                } else {
                    ProtocolError::Status(status).into()
                };
                failed(
                    SaveStage::Replace,
                    error,
                    Some(self.cleanup(dir, &temp).await),
                )
            }
            Err(error) => SaveOutcome::Unconfirmed(error.into()),
        }
    }

    fn check_save(&self, file: &File, edited: &str) -> Result<()> {
        if !Arc::ptr_eq(&self.plan, &file.directory.plan) {
            return Err(Error::WrongConnectionPlan);
        }
        if file.unconfirmed {
            return Err(Error::ReopenRequired);
        }
        if !self.atomic_replace {
            return Err(Error::AtomicReplaceUnavailable);
        }
        if edited.len() > FILE_LIMIT {
            return Err(Error::LimitExceeded);
        }
        if file.baseline.access.mode & 0o7000 != 0 {
            return Err(Error::UnsafePermissions);
        }
        Ok(())
    }

    async fn read_path(
        &self,
        directory: &ListedDirectory,
        path: &str,
    ) -> Result<(Vec<u8>, Access)> {
        directory.check(self).await?;
        let before = self.channel.lstat(path).await?.attrs;
        let access = Access::regular(&before)?;
        self.check_target_path(path).await?;
        let handle = self
            .channel
            .open(path, OpenFlags::READ, FileAttributes::empty())
            .await?
            .handle;
        let result = async {
            let attrs = self.channel.fstat(&handle).await?.attrs;
            if attrs.size.is_some_and(|len| len > FILE_LIMIT as u64) {
                return Err(Error::LimitExceeded);
            }
            unchanged(&before, &attrs)?;
            let bytes = self.read_contents(&handle).await?;
            let after = self.channel.fstat(&handle).await?.attrs;
            unchanged(&attrs, &after)?;
            if after.size.is_some_and(|len| len != bytes.len() as u64) {
                return Err(Error::ReadChanged);
            }
            Ok(bytes)
        }
        .await;
        let closed = self.channel.close(handle).await;
        let bytes = result?;
        closed?;
        directory.check(self).await?;
        self.check_target_path(path).await?;
        unchanged(&before, &self.channel.lstat(path).await?.attrs)?;
        Ok((bytes, access))
    }

    async fn check_target_path(&self, path: &str) -> Result<()> {
        if self.canonical(path).await? != path {
            return Err(Error::PathChanged);
        }
        Ok(())
    }

    async fn read_contents(&self, handle: &SftpHandle) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        loop {
            match self
                .channel
                .read(handle, bytes.len() as u64, CHUNK as u32)
                .await
            {
                Ok(reply) => {
                    if reply.data.is_empty() || reply.data.len() > CHUNK {
                        return Err(Error::InvalidReply);
                    }
                    if bytes.len() + reply.data.len() > FILE_LIMIT {
                        return Err(Error::LimitExceeded);
                    }
                    if bytes.is_empty() {
                        bytes = reply.data;
                    } else {
                        bytes.extend_from_slice(&reply.data);
                    }
                }
                Err(error) if status(&error, StatusCode::Eof) => return Ok(bytes),
                Err(error) => return Err(error.into()),
            }
        }
    }

    async fn upload(
        &self,
        handle: &SftpHandle,
        bytes: &[u8],
        access: Access,
    ) -> std::result::Result<(), (SaveStage, Error)> {
        let initial = self
            .channel
            .fstat(handle)
            .await
            .map_err(Error::from)
            .and_then(|reply| Access::regular(&reply.attrs))
            .map_err(|error| (SaveStage::Permissions, error))?;
        if initial.mode & !0o600 != 0 {
            return Err((SaveStage::Permissions, Error::UnsafePermissions));
        }
        for (index, chunk) in bytes.chunks(CHUNK).enumerate() {
            self.channel
                .write_nowait(handle, (index * CHUNK) as u64, chunk)
                .map_err(|error| (SaveStage::Write, error.into()))?
                .await
                .map_err(|error| (SaveStage::Write, error.into()))?;
        }
        self.apply_permissions(handle, access)
            .await
            .map_err(|error| (SaveStage::Permissions, error))
    }

    async fn apply_permissions(&self, handle: &SftpHandle, expected: Access) -> Result<()> {
        let current = Access::regular(&self.channel.fstat(handle).await?.attrs)?;
        // 소유자나 그룹 변경은 v3에서 두 ID가 모두 확인된 경우에만 전송한다.
        let mut attrs = FileAttributes::empty();
        if (current.uid, current.gid) != (expected.uid, expected.gid) {
            if expected.uid.is_none() || expected.gid.is_none() {
                return Err(Error::MissingPermissions);
            }
            (attrs.uid, attrs.gid) = (expected.uid, expected.gid);
        }
        if current == expected {
            return Ok(());
        }
        attrs.permissions = Some(expected.mode);
        self.channel.fsetstat(handle, attrs).await?;
        if Access::regular(&self.channel.fstat(handle).await?.attrs)? != expected {
            return Err(Error::PermissionMismatch);
        }
        Ok(())
    }

    async fn recheck(&self, file: &File) -> Result<Option<RemoteVersion>> {
        match self.read_path(&file.directory, &file.path).await {
            Ok((bytes, access)) if Revision::new(&bytes, access) == file.baseline => Ok(None),
            Ok((bytes, _)) => Ok(Some(match String::from_utf8(bytes) {
                Ok(text) => RemoteVersion::Text(text),
                Err(_) => RemoteVersion::NotUtf8,
            })),
            Err(Error::Protocol(error)) if status(&error, StatusCode::NoSuchFile) => {
                Ok(Some(RemoteVersion::Missing))
            }
            Err(Error::PathChanged) => Ok(Some(RemoteVersion::DirectoryChanged)),
            Err(Error::ReadChanged) => Ok(Some(RemoteVersion::ReadChanged)),
            Err(Error::NotRegular) => Ok(Some(RemoteVersion::NotRegular)),
            Err(Error::LimitExceeded) => Ok(Some(RemoteVersion::TooLarge)),
            Err(error) => Err(error),
        }
    }

    async fn canonical(&self, path: &str) -> Result<String> {
        let mut names = self.channel.realpath(path).await?.files;
        if names.len() != 1 {
            return Err(Error::InvalidReply);
        }
        let path = names
            .pop()
            .unwrap()
            .filename
            .into_string()
            .map_err(|_| Error::InvalidPath)?;
        validate_absolute(&path)?;
        Ok(path)
    }

    async fn cleanup(&self, dir: &str, temp: &str) -> Cleanup {
        match self.canonical(dir).await {
            Ok(current) if current == dir => match self.channel.remove(temp).await {
                Ok(_) => Cleanup::Removed,
                Err(error) => Cleanup::Failed(error.into()),
            },
            Ok(_) => Cleanup::SkippedDirectoryChanged,
            Err(error) => Cleanup::Failed(error),
        }
    }
}

fn validate_absolute(path: &str) -> Result<()> {
    let Some(components) = path.strip_prefix('/') else {
        return Err(Error::InvalidPath);
    };
    if path.len() > PATH_LIMIT
        || (!components.is_empty() && !components.split('/').all(valid_component))
    {
        return Err(Error::InvalidPath);
    }
    Ok(())
}

fn valid_component(name: &str) -> bool {
    !name.is_empty() && !matches!(name, "." | "..") && !name.contains(['/', '\\', '\0'])
}

// SFTP v3의 관측값만 비교한다. 같은 속성을 가진 교체 파일을 구별하는 파일 ID는 없다.
fn unchanged(before: &FileAttributes, after: &FileAttributes) -> Result<()> {
    if Access::regular(before)? != Access::regular(after)?
        || before.size != after.size
        || before.mtime != after.mtime
    {
        return Err(Error::ReadChanged);
    }
    Ok(())
}

fn child_path(dir: &str, name: &str) -> String {
    format!("{}/{name}", dir.trim_end_matches('/'))
}

fn temporary_path(dir: &str) -> Result<String> {
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(Error::Random)?;
    let mut path = child_path(dir, ".zzabba-edit-");
    for byte in random {
        write!(path, "{byte:02x}").expect("String 쓰기");
    }
    validate_absolute(&path)?;
    Ok(path)
}

fn status(error: &ProtocolError, code: StatusCode) -> bool {
    matches!(error, ProtocolError::Status(status) if status.status_code == code)
}

fn failed(stage: SaveStage, error: Error, cleanup: Option<Cleanup>) -> SaveOutcome {
    SaveOutcome::Failed {
        stage,
        error,
        cleanup,
    }
}

#[cfg(test)]
mod tests;
