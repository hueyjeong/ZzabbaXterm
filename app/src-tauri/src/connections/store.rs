use std::{
    collections::HashSet,
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

use super::profile::{Group, Revision, Target, VERSION, ValidatedDraft, ValidationError};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SaveStatus {
    Saved,
    GroupConflict,
    TargetConflict,
}

#[derive(Debug)]
pub(super) enum StoreError {
    Io(io::Error),
    Format(serde_json::Error),
    Version(u32),
    InvalidProfile(ValidationError),
    DuplicateId,
    MissingGroup,
    RevisionExhausted,
    Replace(io::Error),
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
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Entry<T> {
    pub(super) revision: Revision,
    pub(super) profile: T,
}

pub(super) struct Store {
    directory: PathBuf,
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
        Self { directory }
    }

    pub(super) fn load(&self) -> Result<ProfileFile, StoreError> {
        let lock = self.open_lock()?;
        lock.lock_shared()?;
        self.read_file()
    }

    fn read_file(&self) -> Result<ProfileFile, StoreError> {
        let path = self.directory.join("connections.json");
        let mut input = match open_regular(&path, &mut OpenOptions::new().read(true)) {
            Ok(input) => input,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ProfileFile { version: VERSION, groups: Vec::new(), targets: Vec::new() });
            }
            Err(error) => return Err(error.into()),
        };
        let mut contents = Vec::new();
        input.read_to_end(&mut contents)?;
        let header: FileVersion = serde_json::from_slice(&contents).map_err(StoreError::Format)?;
        if header.version != VERSION {
            return Err(StoreError::Version(header.version));
        }
        let file: ProfileFile = serde_json::from_slice(&contents).map_err(StoreError::Format)?;
        file.validate()?;
        Ok(file)
    }

    pub(super) fn save(&self, draft: ValidatedDraft) -> Result<SaveStatus, StoreError> {
        self.save_with_rename(draft, |from, to| fs::rename(from, to))
    }

    fn save_with_rename(
        &self,
        draft: ValidatedDraft,
        rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
    ) -> Result<SaveStatus, StoreError> {
        let lock = self.open_lock()?;
        lock.lock()?;
        let mut file = self.read_file()?;
        let (group, group_revision, target, target_revision) = draft.into_parts();
        let old_group = file.groups.iter_mut().find(|entry| entry.profile.id == group.id);
        let old_target = file.targets.iter_mut().find(|entry| entry.profile.id == target.id);

        if old_target.as_ref().map(|entry| entry.revision) != target_revision {
            return Ok(SaveStatus::TargetConflict);
        }
        match old_group {
            // 같은 그룹을 동시에 추가한 두 초안도 이름이 같으면 각 대상을 보존한다.
            Some(entry) if entry.profile == group && group_revision.is_none() => {}
            entry if entry.as_ref().map(|entry| entry.revision) != group_revision => {
                return Ok(SaveStatus::GroupConflict);
            }
            Some(entry) if entry.profile != group => entry.update(group)?,
            Some(_) => {}
            None => file.groups.push(Entry::new(group)),
        }
        match old_target {
            Some(entry) => entry.update(target)?,
            None => file.targets.push(Entry::new(target)),
        }

        self.replace(&file, rename)?;
        Ok(SaveStatus::Saved)
    }

    #[cfg(all(test, unix))]
    pub(super) fn save_with_failing_rename(
        &self,
        draft: ValidatedDraft,
    ) -> Result<SaveStatus, StoreError> {
        self.save_with_rename(draft, |from, to| {
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
        rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
    ) -> Result<(), StoreError> {
        let destination = self.directory.join("connections.json");
        let temporary = self.directory.join("connections.json.tmp");
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
            rename(&temporary, &destination).map_err(StoreError::Replace)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
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
        let mut groups = HashSet::with_capacity(self.groups.len());
        for entry in &self.groups {
            entry.profile.validate().map_err(StoreError::InvalidProfile)?;
            if !groups.insert(&entry.profile.id) {
                return Err(StoreError::DuplicateId);
            }
        }
        let mut targets = HashSet::with_capacity(self.targets.len());
        for entry in &self.targets {
            entry.profile.validate().map_err(StoreError::InvalidProfile)?;
            if !targets.insert(&entry.profile.id) {
                return Err(StoreError::DuplicateId);
            }
            if !groups.contains(&entry.profile.group_id) {
                return Err(StoreError::MissingGroup);
            }
        }
        Ok(())
    }
}

impl<T> Entry<T> {
    fn new(profile: T) -> Self {
        Self { revision: Revision::MIN, profile }
    }

    fn update(&mut self, profile: T) -> Result<(), StoreError> {
        self.revision = self.revision.checked_add(1).ok_or(StoreError::RevisionExhausted)?;
        self.profile = profile;
        Ok(())
    }
}
