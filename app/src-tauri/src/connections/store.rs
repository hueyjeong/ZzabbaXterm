use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, Metadata, OpenOptions},
    io::{self, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::{fs::DirBuilder, os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt}};
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT};

use serde::{Deserialize, Serialize};

use super::profile::{
    DesktopRoute, Group, MAX_GROUP_DEPTH, Method, MethodRef, Revision, SshAuth, SshTransport, Target, TargetId, VERSION,
    ValidatedDraft, ValidatedGroup, ValidatedTarget, ValidationError,
};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SaveStatus {
    Saved,
    GroupConflict,
    TargetConflict,
    /// 이 변경으로 SSH 경로가 끊기는 원격 화면 세션 ID다.
    Referenced(Vec<String>),
}

/// 하위 포함 그룹 삭제에서 클라이언트가 본 하위 항목과 개정이다.
#[derive(Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Expected {
    kind: EntryKind,
    id: String,
    revision: Revision,
}

impl Expected {
    /// 그룹과 함께 지워지는 연결 대상의 ID다.
    pub(super) fn target_id(&self) -> Option<&str> {
        matches!(self.kind, EntryKind::Target).then_some(self.id.as_str())
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum EntryKind {
    Group,
    Target,
}

#[derive(Debug)]
pub(super) enum StoreError {
    Io(io::Error),
    Format(serde_json::Error),
    Version(u32),
    InvalidProfile(ValidationError),
    DuplicateId,
    MissingGroup,
    GroupCycle,
    GroupDepth,
    /// 원격 화면의 SSH 경로가 Agent·키 파일 인증의 저장된 직접 SSH 대상을 가리키지 않는다.
    DesktopRoute,
    RevisionExhausted,
    Replace(io::Error),
    Authorization(String),
    InvalidTestedAccount,
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Deserialize)]
struct FileVersion {
    version: u32,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProfileFile {
    version: u32,
    pub(super) groups: Vec<Entry<Group>>,
    pub(super) targets: Vec<Entry<Target>>,
    /// v1 파일에서 읽어 메모리에서 v2로 올렸다. 처음 v2로 쓰기 직전에 원본을 백업한다.
    #[serde(skip)]
    from_v1: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Entry<T> {
    pub(super) revision: Revision,
    pub(super) profile: T,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) tested_account_id: Option<String>,
}

pub(super) struct Store {
    directory: PathBuf,
    #[cfg(test)]
    on_lock_wait: Option<Box<dyn Fn() + Send + Sync>>,
}

impl Store {
    // 절대 경로의 앱 전용 하위 디렉터리를 받는다. 상위 경로는 호출자가 신뢰할 수 있는
    // 사용자 설정 경로로 준비한다. Windows에서는 app_config_dir() 아래를 사용하며 ACL은 상속한다.
    pub(super) fn new(directory: &Path) -> Self {
        // 끝의 구분자나 `.`가 링크 검사에 영향을 주지 않도록 최종 이름을 다시 붙인다.
        let directory = match (directory.parent(), directory.file_name()) {
            (Some(parent), Some(name)) => parent.join(name),
            _ => directory.into(),
        };
        Self { directory, #[cfg(test)] on_lock_wait: None }
    }

    #[cfg(test)]
    pub(super) fn observe_lock_wait(mut self, on_wait: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_lock_wait = Some(Box::new(on_wait));
        self
    }

    pub(super) fn load(&self) -> Result<ProfileFile, StoreError> {
        self.with_loaded(|file| file)
    }

    pub(super) fn with_loaded<T>(&self, read: impl FnOnce(ProfileFile) -> T) -> Result<T, StoreError> {
        let lock = self.open_lock()?;
        lock.lock_shared()?;
        self.read_file().map(read)
    }

    fn read_file(&self) -> Result<ProfileFile, StoreError> {
        let path = self.directory.join("connections.json");
        let mut input = match open_regular(&path, &mut OpenOptions::new().read(true)) {
            Ok(input) => input,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ProfileFile { version: VERSION, groups: Vec::new(), targets: Vec::new(), from_v1: false });
            }
            Err(error) => return Err(error.into()),
        };
        let mut contents = Vec::new();
        input.read_to_end(&mut contents)?;
        let header: FileVersion = serde_json::from_slice(&contents).map_err(StoreError::Format)?;
        // v1 그룹에는 상위 그룹이 없으므로 그대로 최상위 그룹이 된다.
        let from_v1 = match header.version {
            1 => true,
            VERSION => false,
            other => return Err(StoreError::Version(other)),
        };
        let mut file: ProfileFile = serde_json::from_slice(&contents).map_err(StoreError::Format)?;
        if from_v1 && (file.groups.iter().any(|entry| entry.profile.parent_id.is_some())
            || file.targets.iter().any(|entry| entry.profile.group_id.is_none()))
        {
            use serde::de::Error;
            return Err(StoreError::Format(serde_json::Error::custom("v1 파일에는 parentId가 없고 모든 대상에 groupId가 있습니다")));
        }
        file.version = VERSION;
        file.from_v1 = from_v1;
        file.validate()?;
        Ok(file)
    }

    #[cfg(test)]
    pub(super) fn save(&self, draft: ValidatedDraft) -> Result<SaveStatus, StoreError> {
        self.save_with_rename(draft, false, |from, to| fs::rename(from, to))
    }

    /// 연결 시험을 통과한 대상만 저장한다. `authorize`는 잠금 뒤와 교체 직전에 시험 실행이 유효한지 다시 확인하고,
    /// `committed`는 교체 뒤 그 실행을 소진한다. SSM과 S3는 시험에서 확인한 계정을 기록한다.
    pub(super) fn save_authorized(
        &self,
        draft: ValidatedTarget,
        tested_account_id: Option<String>,
        authorize: impl FnMut() -> Result<(), String>,
        committed: impl FnOnce(),
    ) -> Result<SaveStatus, StoreError> {
        if draft.method().uses_tested_account() != tested_account_id.is_some() {
            return Err(StoreError::InvalidTestedAccount);
        }
        self.save_target(draft, authorize, committed, |_, target| {
            validate_tested_account(tested_account_id.as_deref(), target.method().uses_tested_account())?;
            Ok(tested_account_id)
        })
    }

    pub(super) fn register(&self, draft: ValidatedDraft) -> Result<SaveStatus, StoreError> {
        self.save_with_rename(draft, true, |from, to| fs::rename(from, to))
    }

    /// 그룹을 추가하거나 이름을 바꾸거나 상위 그룹을 바꾼다.
    pub(super) fn save_group(
        &self,
        draft: ValidatedGroup,
        authorize: impl FnMut() -> Result<(), String>,
    ) -> Result<SaveStatus, StoreError> {
        let (group, revision) = draft.into_parts();
        self.modify(authorize, || {}, |from, to| fs::rename(from, to), |file| {
            match file.groups.iter_mut().find(|entry| entry.profile.id == group.id) {
                entry if entry.as_ref().map(|entry| entry.revision) != revision => return Ok(SaveStatus::GroupConflict),
                Some(entry) => entry.update(group)?,
                None => file.groups.push(Entry::new(group)),
            }
            Ok(SaveStatus::Saved)
        })
    }

    /// 그룹과 하위 그룹, 그 안의 세션을 지운다. 클라이언트가 본 하위 항목과 개정이 저장본과 정확히 같아야 한다.
    pub(super) fn delete_group(
        &self,
        id: &str,
        revision: Revision,
        expected: Vec<Expected>,
        authorize: impl FnMut() -> Result<(), String>,
    ) -> Result<SaveStatus, StoreError> {
        self.modify(authorize, || {}, |from, to| fs::rename(from, to), |file| {
            if !file.groups.iter().any(|entry| entry.profile.id.as_str() == id && entry.revision == revision) {
                return Ok(SaveStatus::GroupConflict);
            }
            let mut removed = HashSet::from([id.to_owned()]);
            loop {
                let before = removed.len();
                for entry in &file.groups {
                    if entry.profile.parent_id.as_ref().is_some_and(|parent| removed.contains(parent.as_str())) {
                        removed.insert(entry.profile.id.as_str().to_owned());
                    }
                }
                if removed.len() == before { break; }
            }
            let in_removed = |group: Option<&super::profile::GroupId>| group.is_some_and(|group| removed.contains(group.as_str()));
            let actual: HashSet<Expected> = file.groups.iter().filter(|entry| entry.profile.id.as_str() != id && removed.contains(entry.profile.id.as_str()))
                .map(|entry| Expected { kind: EntryKind::Group, id: entry.profile.id.as_str().into(), revision: entry.revision })
                .chain(file.targets.iter().filter(|entry| in_removed(entry.profile.group_id.as_ref()))
                    .map(|entry| Expected { kind: EntryKind::Target, id: entry.profile.id.as_str().into(), revision: entry.revision }))
                .collect();
            let count = expected.len();
            let expected: HashSet<Expected> = expected.into_iter().collect();
            if expected.len() != count || expected != actual {
                return Ok(SaveStatus::GroupConflict);
            }
            file.targets.retain(|entry| !in_removed(entry.profile.group_id.as_ref()));
            file.groups.retain(|entry| !removed.contains(entry.profile.id.as_str()));
            Ok(SaveStatus::Saved)
        })
    }

    pub(super) fn delete_target(
        &self,
        id: &str,
        revision: Revision,
        authorize: impl FnMut() -> Result<(), String>,
    ) -> Result<SaveStatus, StoreError> {
        self.modify(authorize, || {}, |from, to| fs::rename(from, to), |file| {
            match file.targets.iter().position(|entry| entry.profile.id.as_str() == id) {
                Some(index) if file.targets[index].revision == revision => {
                    file.targets.remove(index);
                    Ok(SaveStatus::Saved)
                }
                _ => Ok(SaveStatus::TargetConflict),
            }
        })
    }

    /// 연결 시험 없이 입력 검증만 거쳐 대상을 저장한다. SSM과 S3의 계정은 방식, 프로필, 리전이 같으면 저장본을
    /// 유지하고, 아니면 `configured_account`(로컬 SSO 설정의 계정)를 기록한다. 없으면 비워 둔다.
    pub(super) fn save_validated(
        &self,
        draft: ValidatedTarget,
        configured_account: impl FnOnce(&str, &str) -> Option<String>,
        authorize: impl FnMut() -> Result<(), String>,
    ) -> Result<SaveStatus, StoreError> {
        self.save_target(draft, authorize, || {}, |old, target| Ok(match (old, account_scope(target)) {
            (_, None) => None,
            // 저장된 계정이 없으면 같은 범위라도 로컬 SSO 설정을 다시 읽는다.
            (Some(entry), scope) if account_scope(&entry.profile) == scope && entry.tested_account_id.is_some() => {
                entry.tested_account_id.clone()
            }
            (_, Some((_, profile, region))) => configured_account(profile, region)
                .filter(|account| validate_tested_account(Some(account), true).is_ok()),
        }))
    }

    /// 대상의 개정이 저장본과 같을 때만 대상을 추가하거나 바꾼다. `account`는 기존 항목을 보고 기록할 계정을 정한다.
    fn save_target(
        &self,
        draft: ValidatedTarget,
        authorize: impl FnMut() -> Result<(), String>,
        committed: impl FnOnce(),
        account: impl FnOnce(Option<&Entry<Target>>, &Target) -> Result<Option<String>, StoreError>,
    ) -> Result<SaveStatus, StoreError> {
        let (target, revision) = draft.into_parts();
        let id = target.id.clone();
        self.modify(authorize, committed, |from, to| fs::rename(from, to), |file| {
            let old = file.targets.iter_mut().find(|entry| entry.profile.id == target.id);
            if old.as_ref().map(|entry| entry.revision) != revision {
                return Ok(SaveStatus::TargetConflict);
            }
            let account = account(old.as_deref(), &target)?;
            let entry = match old {
                Some(entry) => { entry.update(target)?; entry }
                None => {
                    file.targets.push(Entry::new(target));
                    file.targets.last_mut().unwrap()
                }
            };
            entry.tested_account_id = account;
            file.require_route(&id)?;
            Ok(SaveStatus::Saved)
        })
    }

    fn save_with_rename(
        &self,
        draft: ValidatedDraft,
        insert_only: bool,
        rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
    ) -> Result<SaveStatus, StoreError> {
        let (mut group, group_revision, target, target_revision) = draft.into_parts();
        let id = target.id.clone();
        self.modify(|| Ok(()), || {}, rename, |file| {
            let old_group = file.groups.iter_mut().find(|entry| entry.profile.id == group.id);
            let old_target = file.targets.iter_mut().find(|entry| entry.profile.id == target.id);

            if insert_only && old_target.is_some() {
                return Ok(SaveStatus::TargetConflict);
            }
            if old_target.as_ref().map(|entry| entry.revision) != target_revision {
                return Ok(SaveStatus::TargetConflict);
            }
            // 그룹을 함께 보내는 초안은 상위 그룹을 바꾸지 않는다. 이동은 그룹 저장에서만 한다.
            if let Some(entry) = &old_group {
                group.parent_id = entry.profile.parent_id.clone();
            }
            match old_group {
                // 같은 그룹을 동시에 추가한 두 초안도 이름이 같으면 각 대상을 보존한다.
                Some(entry) if entry.profile == group && group_revision.is_none() => {}
                entry if entry.as_ref().map(|entry| entry.revision) != group_revision => {
                    return Ok(SaveStatus::GroupConflict);
                }
                Some(entry) if entry.profile != group => {
                    if insert_only { return Ok(SaveStatus::GroupConflict); }
                    entry.update(group)?;
                }
                Some(_) => {}
                None => file.groups.push(Entry::new(group)),
            }
            let entry = match old_target {
                Some(entry) => { entry.update(target)?; entry }
                None => {
                    file.targets.push(Entry::new(target));
                    file.targets.last_mut().unwrap()
                }
            };
            entry.tested_account_id = None;
            file.require_route(&id)?;
            Ok(SaveStatus::Saved)
        })
    }

    /// 잠금 아래에서 파일을 읽고 바꾼 뒤 검증해 원자적으로 교체한다. `change`가 `Saved`가 아니면 쓰지 않는다.
    fn modify(
        &self,
        mut authorize: impl FnMut() -> Result<(), String>,
        committed: impl FnOnce(),
        rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
        change: impl FnOnce(&mut ProfileFile) -> Result<SaveStatus, StoreError>,
    ) -> Result<SaveStatus, StoreError> {
        let lock = self.open_lock()?;
        #[cfg(test)]
        if let Some(on_wait) = &self.on_lock_wait {
            match lock.try_lock() {
                Err(fs::TryLockError::WouldBlock) => on_wait(),
                other => panic!("외부 설정 잠금이 필요합니다: {other:?}"),
            }
        }
        lock.lock()?;
        authorize().map_err(StoreError::Authorization)?;
        let mut file = self.read_file()?;
        let broken = file.broken_routes();
        let status = change(&mut file)?;
        if status != SaveStatus::Saved {
            return Ok(status);
        }
        file.validate()?;
        // 이번 변경으로 새로 끊기는 원격 화면 SSH 경로가 있으면 쓰지 않는다.
        let referenced: Vec<String> = file.broken_routes().into_iter().filter(|id| !broken.contains(id)).collect();
        if !referenced.is_empty() {
            return Ok(SaveStatus::Referenced(referenced));
        }

        self.replace(&file, |from, to| {
            // 이 승인 뒤의 원자 교체는 창을 닫더라도 이미 승인한 작업으로 끝낸다.
            authorize().map_err(StoreError::Authorization)?;
            rename(from, to).map_err(StoreError::Replace)
        })?;
        committed();
        Ok(SaveStatus::Saved)
    }

    #[cfg(all(test, unix))]
    pub(super) fn save_with_failing_rename(
        &self,
        draft: ValidatedDraft,
    ) -> Result<SaveStatus, StoreError> {
        self.save_with_rename(draft, false, |from, to| {
            // 일반 파일 아래의 경로에서는 권한과 관계없이 실제 rename이 ENOTDIR로 실패한다.
            fs::rename(from.join("not-a-directory"), to)
        })
    }

    fn open_lock(&self) -> io::Result<File> {
        if !self.directory.is_absolute() || self.directory.file_name().is_none() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "앱 설정 디렉터리 경로가 올바르지 않습니다"));
        }
        #[cfg(unix)]
        let created = DirBuilder::new().mode(0o700).create(&self.directory);
        #[cfg(windows)]
        let created = fs::create_dir(&self.directory);
        match created {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let directory = fs::symlink_metadata(&self.directory)?;
        if is_link(&directory) || !directory.is_dir() {
            return Err(unsafe_path());
        }
        #[cfg(unix)]
        // SAFETY: geteuid는 인수나 사용자 메모리를 사용하지 않는다.
        if directory.uid() != unsafe { libc::geteuid() } || directory.mode() & 0o022 != 0 {
            return Err(unsafe_path());
        }
        // 교체되는 설정 파일과 별개의 파일을 잠근다.
        open_regular(
            &self.directory.join("connections.lock"),
            OpenOptions::new().read(true).write(true).create(true),
        )
    }

    fn replace(
        &self,
        file: &ProfileFile,
        rename: impl FnOnce(&Path, &Path) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let destination = self.directory.join("connections.json");
        let temporary = self.directory.join("connections.json.tmp");
        if file.from_v1 {
            self.backup_v1(&destination)?;
        }
        // 잠금 아래에서 이전 비정상 종료가 남긴 임시 파일을 회수한다.
        match fs::symlink_metadata(&temporary) {
            Ok(metadata) if is_link(&metadata) || !metadata.is_file() => return Err(unsafe_path().into()),
            Ok(_) => fs::remove_file(&temporary)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(windows)]
        options.access_mode(windows_sys::Win32::Foundation::GENERIC_WRITE | windows_sys::Win32::Storage::FileSystem::WRITE_DAC);
        let output = open_regular(&temporary, &mut options)?;
        let result = (|| {
            // 기존 파일의 권한을 적용할 때까지 임시 파일은 비워 둔다.
            #[cfg(windows)]
            preserve_dacl(&destination, &output)?;
            let mut writer = BufWriter::new(output);
            serde_json::to_writer(&mut writer, file).map_err(StoreError::Format)?;
            writer.flush()?;
            writer.get_ref().sync_all()?;
            drop(writer);
            rename(&temporary, &destination)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

impl Store {
    /// v1 원본을 `connections.v1.json`으로 한 번만 복사한다. v1으로 읽히는 백업이 이미 있으면 덮어쓰지 않는다.
    /// 임시 파일을 끝까지 쓴 뒤 덮어쓰지 않는 하드 링크로 확정하므로, 중간에 끝나도 불완전한 백업이 남지 않는다.
    fn backup_v1(&self, source: &Path) -> Result<(), StoreError> {
        let backup = self.directory.join("connections.v1.json");
        let temporary = self.directory.join("connections.v1.json.tmp");
        match fs::symlink_metadata(&backup) {
            Ok(metadata) if is_link(&metadata) || !metadata.is_file() => return Err(unsafe_path().into()),
            Ok(_) => {
                let mut contents = Vec::new();
                open_regular(&backup, OpenOptions::new().read(true))?.read_to_end(&mut contents)?;
                if serde_json::from_slice::<FileVersion>(&contents).is_ok_and(|header| header.version == 1) {
                    return Ok(());
                }
                // 비었거나 v1이 아닌 백업은 다시 만든다.
                fs::remove_file(&backup)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        match fs::symlink_metadata(&temporary) {
            Ok(metadata) if is_link(&metadata) || !metadata.is_file() => return Err(unsafe_path().into()),
            Ok(_) => fs::remove_file(&temporary)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(windows)]
        options.access_mode(windows_sys::Win32::Foundation::GENERIC_WRITE | windows_sys::Win32::Storage::FileSystem::WRITE_DAC);
        let mut output = open_regular(&temporary, &mut options)?;
        let result = (|| {
            // 원본의 권한을 적용할 때까지 임시 파일은 비워 둔다.
            #[cfg(windows)]
            preserve_dacl(source, &output)?;
            let mut contents = Vec::new();
            open_regular(source, OpenOptions::new().read(true))?.read_to_end(&mut contents)?;
            output.write_all(&contents)?;
            output.sync_all()?;
            drop(output);
            fs::hard_link(&temporary, &backup)
        })();
        let _ = fs::remove_file(&temporary);
        result.map_err(StoreError::Io)
    }
}

fn open_regular(path: &Path, options: &mut OpenOptions) -> io::Result<File> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if is_link(&metadata) || !metadata.is_file() => return Err(unsafe_path()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(windows)]
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if is_link(&metadata) || !metadata.is_file() {
        return Err(unsafe_path());
    }
    Ok(file)
}

fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return true;
    }
    metadata.file_type().is_symlink()
}

fn unsafe_path() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "안전하지 않은 앱 설정 경로입니다")
}

#[cfg(windows)]
fn preserve_dacl(destination: &Path, output: &File) -> io::Result<()> {
    use std::{os::windows::io::AsRawHandle, ptr::{null, null_mut}};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl,
            PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
            UNPROTECTED_DACL_SECURITY_INFORMATION,
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo}},
        Storage::FileSystem::READ_CONTROL,
    };

    let input = match open_regular(destination, OpenOptions::new().access_mode(READ_CONTROL)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let (mut dacl, mut descriptor) = (null_mut(), null_mut());
    // SAFETY: 핸들과 출력 포인터가 유효하며, DACL은 LocalFree 전까지 descriptor에 속한다.
    unsafe {
        let error = GetSecurityInfo(input.as_raw_handle(), SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION, null_mut(), null_mut(), &mut dacl, null_mut(), &mut descriptor);
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        let (mut control, mut revision) = (0, 0);
        let result = if GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) == 0 {
            Err(io::Error::last_os_error())
        } else {
            let inheritance = if control & SE_DACL_PROTECTED != 0 {
                PROTECTED_DACL_SECURITY_INFORMATION
            } else {
                UNPROTECTED_DACL_SECURITY_INFORMATION
            };
            let error = SetSecurityInfo(output.as_raw_handle(), SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | inheritance, null_mut(), null_mut(), dacl, null());
            if error == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(error as i32)) }
        };
        LocalFree(descriptor);
        result
    }
}

impl ProfileFile {
    fn validate(&self) -> Result<(), StoreError> {
        let mut parents = HashMap::with_capacity(self.groups.len());
        for entry in &self.groups {
            entry.profile.validate().map_err(StoreError::InvalidProfile)?;
            validate_tested_account(entry.tested_account_id.as_deref(), false)?;
            if parents.insert(&entry.profile.id, entry.profile.parent_id.as_ref()).is_some() {
                return Err(StoreError::DuplicateId);
            }
        }
        // 상위 그룹을 따라 올라가며 존재, 순환, 깊이를 확인한다.
        for entry in &self.groups {
            let mut seen = HashSet::new();
            let mut current = Some(&entry.profile.id);
            while let Some(id) = current {
                if !seen.insert(id) {
                    return Err(StoreError::GroupCycle);
                }
                if seen.len() > MAX_GROUP_DEPTH {
                    return Err(StoreError::GroupDepth);
                }
                current = *parents.get(id).ok_or(StoreError::MissingGroup)?;
            }
        }
        let mut targets = HashSet::with_capacity(self.targets.len());
        for entry in &self.targets {
            entry.profile.validate().map_err(StoreError::InvalidProfile)?;
            validate_tested_account(entry.tested_account_id.as_deref(), entry.profile.method().uses_tested_account())?;
            if !targets.insert(&entry.profile.id) {
                return Err(StoreError::DuplicateId);
            }
            if entry.profile.group_id.as_ref().is_some_and(|group| !parents.contains_key(group)) {
                return Err(StoreError::MissingGroup);
            }
        }
        Ok(())
    }

    /// 원격 화면의 SSH 경로는 자신이 아닌, Agent·키 파일 인증의 저장된 직접 SSH 대상만 가리킨다.
    fn route_ready(&self, target: &Target) -> bool {
        let MethodRef::Desktop { route: DesktopRoute::Ssh { target_id } } = target.method() else { return true };
        self.targets.iter().any(|entry| entry.profile.id.as_str() == target_id && entry.profile.id != target.id
            && matches!(&entry.profile.method, Method::Ssh { ssh } if matches!(ssh.transport, SshTransport::Direct {})
                && matches!(ssh.auth, SshAuth::Agent { .. } | SshAuth::KeyFile { .. })))
    }

    /// 쓰기 전에 기존 파일에 이미 끊겨 있던 경로와 새로 끊긴 경로를 구별하는 데 쓴다.
    fn broken_routes(&self) -> Vec<String> {
        self.targets.iter().filter(|entry| !self.route_ready(&entry.profile))
            .map(|entry| entry.profile.id.as_str().to_owned()).collect()
    }

    fn require_route(&self, id: &TargetId) -> Result<(), StoreError> {
        match self.targets.iter().find(|entry| entry.profile.id == *id) {
            Some(entry) if !self.route_ready(&entry.profile) => Err(StoreError::DesktopRoute),
            _ => Ok(()),
        }
    }
}

/// SSM과 S3 대상의 계정을 정하는 방식, 프로필, 리전이다.
fn account_scope(target: &Target) -> Option<(&'static str, &str, &str)> {
    match target.method() {
        MethodRef::Ssm { profile, region, .. } => Some(("ssm", profile, region)),
        MethodRef::S3 { s3 } => Some(("s3", &s3.profile, &s3.region)),
        _ => None,
    }
}

fn validate_tested_account(account: Option<&str>, allowed: bool) -> Result<(), StoreError> {
    if account.is_some_and(|id| !allowed || id.len() != 12 || !id.bytes().all(|byte| byte.is_ascii_digit())) {
        return Err(StoreError::InvalidTestedAccount);
    }
    Ok(())
}

impl<T> Entry<T> {
    fn new(profile: T) -> Self {
        Self { revision: Revision::MIN, profile, tested_account_id: None }
    }

    fn update(&mut self, profile: T) -> Result<(), StoreError> {
        self.revision = self.revision.checked_add(1).ok_or(StoreError::RevisionExhausted)?;
        self.profile = profile;
        Ok(())
    }
}
