use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io,
    mem::{offset_of, size_of},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr::{null, null_mut},
    slice,
};

use windows_sys::{
    Wdk::{Foundation::OBJECT_ATTRIBUTES, Storage::FileSystem::*},
    Win32::{
        Foundation::{
            GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree, MAX_PATH,
            OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, RtlNtStatusToDosError, STATUS_FILE_IS_A_DIRECTORY,
            STATUS_NAME_TOO_LONG, UNICODE_STRING,
        },
        Security::{
            ACE_HEADER,
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
            *,
        },
        Storage::FileSystem::*,
        System::{
            IO::IO_STATUS_BLOCK,
            WindowsProgramming::{DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOVABLE},
        },
    },
};

use super::publication::{PublicationError, temporary_name};

const SHARE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

pub(super) type FileId = (u64, [u8; 16]);

pub(super) fn file_id(file: &File) -> io::Result<FileId> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: the file handle and correctly sized output live through the call.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

pub(super) fn valid_path(path: &Path) -> bool {
    matches!(path.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
}

pub(super) fn valid_name(name: &OsStr) -> bool {
    // The handle-relative API takes a single component, never a path or NTFS stream.
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .encode_wide()
            .all(|ch| ch != 0 && ch != b':' as u16 && ch != b'/' as u16 && ch != b'\\' as u16)
}

/// 클라우드 동기화 자리 표시자(OneDrive Files On-Demand 등)의 재분석 태그다. 이름 대리(링크)가 아니다.
pub(super) fn is_cloud_tag(tag: u32) -> bool {
    const IO_REPARSE_TAG_CLOUD: u32 = 0x9000_001A;
    const IO_REPARSE_TAG_CLOUD_MASK: u32 = 0x0000_F000;
    tag & !IO_REPARSE_TAG_CLOUD_MASK == IO_REPARSE_TAG_CLOUD
}

fn reparse_tag(file: &File) -> io::Result<u32> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: the file handle and correctly sized output live through the call.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 { 0 } else { info.ReparseTag })
}

/// 경로의 항목 자체(재분석 지점을 따라가지 않음)가 클라우드 자리 표시자인지 확인한다.
pub(super) fn cloud_placeholder(path: &Path) -> io::Result<bool> {
    let file = File::options()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(SHARE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    Ok(is_cloud_tag(reparse_tag(&file)?))
}

pub(super) struct Directory {
    pub(super) path: PathBuf,
    file: File,
    id: FileId,
    /// 스토리지 로컬 창은 클라우드 자리 표시자 폴더와 데이터가 로컬에 있는 자리 표시자 파일을 허용한다.
    cloud: bool,
}

impl Directory {
    pub(super) fn open(path: PathBuf) -> io::Result<Self> {
        Self::open_with(path, false)
    }

    pub(super) fn open_storage(path: PathBuf) -> io::Result<Self> {
        Self::open_with(path, true)
    }

    fn open_with(path: PathBuf, cloud: bool) -> io::Result<Self> {
        let file = Self::open_path(&path, cloud)?;
        let id = file_id(&file)?;
        Ok(Self { path, file, id, cloud })
    }

    pub(super) fn open_cli(path: &Path) -> io::Result<Self> {
        let disk = match path.components().next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::Disk(disk) | Prefix::VerbatimDisk(disk) => disk,
                _ => return Err(io::ErrorKind::InvalidInput.into()),
            },
            _ => return Err(io::ErrorKind::InvalidInput.into()),
        };
        let drive = [u16::from(disk), b':' as u16, b'\\' as u16, 0];
        // SAFETY: 드라이브 루트를 가리키는 UTF-16 문자열은 NUL로 끝난다.
        if !matches!(
            unsafe { GetDriveTypeW(drive.as_ptr()) },
            DRIVE_FIXED | DRIVE_REMOVABLE | DRIVE_CDROM | DRIVE_RAMDISK
        ) {
            return Err(io::ErrorKind::InvalidInput.into());
        }

        let path = std::path::absolute(path)?;
        let mut components = path.components();
        components.next(); // 위에서 검사한 드라이브 접두어.
        if components.next() != Some(Component::RootDir)
            || !components.clone().all(|part| matches!(part, Component::Normal(name) if valid_name(name)))
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let root = Self::open_path(Path::new(&OsString::from_wide(&drive[..3])), false)?;
        let relative = components.as_path();
        // 전체 하위 경로를 한 번에 열어 검사와 열기 사이의 경로 바꿔치기를 피한다.
        let file = if relative.as_os_str().is_empty() {
            root
        } else {
            let file = Self::open_at(
                &root,
                relative.as_os_str(),
                FILE_OPEN,
                FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
                null(),
                true,
            )
            .map_err(nt_error)?;
            Self::check_directory(&file, false)?;
            file
        };
        let path = final_path(&file)
            .filter(|path| valid_path(path))
            .ok_or(io::ErrorKind::InvalidInput)?;
        let id = file_id(&file)?;
        Ok(Self { path, file, id, cloud: false })
    }

    pub(super) fn still_at_cli_path(&self, path: &Path) -> io::Result<bool> {
        let current = Self::open_cli(path)?;
        Ok(current.id == self.id && current.path == self.path)
    }

    fn open_path(path: &Path, cloud: bool) -> io::Result<File> {
        let file = File::options()
            .access_mode(FILE_READ_ATTRIBUTES | FILE_TRAVERSE)
            .share_mode(SHARE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        Self::check_directory(&file, cloud)?;
        Ok(file)
    }

    fn check_directory(file: &File, cloud: bool) -> io::Result<()> {
        let metadata = file.metadata()?;
        let reparse = metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        if !metadata.is_dir() || (reparse && !(cloud && is_cloud_tag(reparse_tag(file)?))) {
            return Err(io::ErrorKind::NotADirectory.into());
        }
        Ok(())
    }

    pub(super) fn still_at_path(&self) -> io::Result<bool> {
        Ok(file_id(&Self::open_path(&self.path, self.cloud)?)? == self.id)
    }

    pub(super) fn open_file(&self, name: &OsStr) -> io::Result<Option<File>> {
        self.open_regular(name, GENERIC_READ)
    }

    pub(super) fn check_replace_access(&self, _: &OsStr, file: &File) -> io::Result<()> {
        check_writable_attributes(file.metadata()?.file_attributes())?;
        drop(reopen(file, FILE_WRITE_DATA, SHARE)?);
        Ok(())
    }
    pub(super) fn entry_id(&self, name: &OsStr) -> io::Result<Option<FileId>> {
        self.open_regular(name, FILE_READ_ATTRIBUTES)?
            .as_ref()
            .map(file_id)
            .transpose()
    }

    fn open_regular(&self, name: &OsStr, access: u32) -> io::Result<Option<File>> {
        let file = match Self::open_at(&self.file, name, FILE_OPEN, access, null(), false) {
            Err(STATUS_FILE_IS_A_DIRECTORY) => return Ok(None),
            result => result.map_err(nt_error)?,
        };
        let metadata = file.metadata()?;
        let attributes = metadata.file_attributes();
        // 클라우드 자리 표시자는 데이터가 로컬에 있을 때만(회수 표시 없음) 일반 파일로 쓴다.
        let local_cloud = self.cloud
            && attributes & (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_RECALL_ON_OPEN) == 0
            && is_cloud_tag(reparse_tag(&file)?);
        Ok(
            (metadata.is_file() && (attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 || local_cloud))
                .then_some(file),
        )
    }

    pub(super) fn stage(&self, access: Option<&Access>) -> io::Result<Temporary<'_>> {
        let name = temporary_name()?;
        let file = Self::open_at(
            &self.file,
            &name,
            FILE_CREATE,
            GENERIC_READ | GENERIC_WRITE | DELETE,
            access.map_or(null(), |access| access.descriptor.cast()),
            false,
        )
        .map_err(nt_error)?;
        let temp = Temporary {
            directory: self,
            file: Some(file),
        };
        // Verify the inherited descriptor before exposing any buffer bytes in the directory.
        if let Some(access) = access && !access.matches(temp.file.as_ref().expect("temporary file is open"))? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "temporary file access differs",
            ));
        }
        Ok(temp)
    }

    fn open_at(
        directory: &File,
        name: &OsStr,
        disposition: u32,
        access: u32,
        descriptor: *const SECURITY_DESCRIPTOR,
        cli_directory: bool,
    ) -> Result<File, i32> {
        let mut wide: Vec<_> = name.encode_wide().collect();
        let bytes = u16::try_from(wide.len() * 2).map_err(|_| STATUS_NAME_TOO_LONG)?;
        let name = UNICODE_STRING {
            Length: bytes,
            MaximumLength: bytes,
            Buffer: wide.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: directory.as_raw_handle(),
            ObjectName: &name,
            Attributes: OBJ_CASE_INSENSITIVE | if cli_directory { OBJ_DONT_REPARSE } else { 0 },
            SecurityDescriptor: descriptor,
            SecurityQualityOfService: null(),
        };
        let mut handle = null_mut();
        // SAFETY: all arguments live through this synchronous open; the name is length-delimited.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                access | SYNCHRONIZE,
                &attributes,
                &mut IO_STATUS_BLOCK::default(),
                null(),
                FILE_ATTRIBUTE_NORMAL,
                // Deny data opens until the staged handle is closed after publication.
                if disposition == FILE_CREATE { 0 } else { SHARE },
                disposition,
                FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT
                    | if cli_directory { FILE_DIRECTORY_FILE } else { FILE_NON_DIRECTORY_FILE },
                null(),
                0,
            )
        };
        if status < 0 {
            return Err(status);
        }
        // SAFETY: NtCreateFile returned a new, owned handle.
        Ok(unsafe { File::from_raw_handle(handle) })
    }
}

fn nt_error(status: i32) -> io::Error {
    // SAFETY: RtlNtStatusToDosError translates a status value without accessing a pointer.
    io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
}

fn check_writable_attributes(attributes: u32) -> io::Result<()> {
    if attributes & FILE_ATTRIBUTE_READONLY != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "original file is read-only",
        ));
    }
    if attributes & FILE_ATTRIBUTE_ENCRYPTED != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "encrypted file replacement",
        ));
    }
    Ok(())
}

const ACCESS_INFO: u32 = OWNER_SECURITY_INFORMATION
    | GROUP_SECURITY_INFORMATION
    | DACL_SECURITY_INFORMATION
    | LABEL_SECURITY_INFORMATION
    | SCOPE_SECURITY_INFORMATION;

pub(super) struct Access {
    descriptor: PSECURITY_DESCRIPTOR,
    owner: PSID,
    group: PSID,
    dacl: *mut ACL,
    label: *mut ACL,
    protected: bool,
}

impl Access {
    pub(super) fn read(file: &File) -> io::Result<Self> {
        let mut access = Self {
            descriptor: null_mut(),
            owner: null_mut(),
            group: null_mut(),
            dacl: null_mut(),
            label: null_mut(),
            protected: false,
        };
        // SAFETY: the handle is valid; the output pointers remain owned by the returned descriptor.
        let error = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                ACCESS_INFO,
                &mut access.owner,
                &mut access.group,
                &mut access.dacl,
                &mut access.label,
                &mut access.descriptor,
            )
        };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        reject_central_access_policy(access.label)?;
        let mut control = 0;
        // SAFETY: the descriptor and output storage are valid.
        if unsafe { GetSecurityDescriptorControl(access.descriptor, &mut control, &mut 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        access.protected = control & SE_DACL_PROTECTED != 0;
        Ok(access)
    }

    pub(super) fn read_editor(file: &File) -> io::Result<Self> {
        Self::read_preserved(file, false)
    }

    pub(super) fn matches_editor(&self, file: &File) -> io::Result<bool> {
        self.matches_preserved(file, false)
    }

    /// zone이면 다운로드 교체에서 원본의 `Zone.Identifier` 스트림 하나는 버려도 된다.
    fn read_preserved(file: &File, zone: bool) -> io::Result<Self> {
        check_auxiliary_data(file, zone)?;
        Self::read(file)
    }

    fn matches_preserved(&self, file: &File, zone: bool) -> io::Result<bool> {
        check_auxiliary_data(file, zone)?;
        self.matches(file)
    }

    pub(super) fn matches(&self, file: &File) -> io::Result<bool> {
        let other = Self::read(file)?;
        // SAFETY: owner and group SIDs are held by the two live descriptors.
        let same_sids = unsafe {
            EqualSid(self.owner, other.owner) != 0 && EqualSid(self.group, other.group) != 0
        };
        Ok(same_sids
            && self.protected == other.protected
            && self.acl_bytes(self.dacl)? == other.acl_bytes(other.dacl)?
            && self.acl_bytes(self.label)? == other.acl_bytes(other.label)?)
    }

    fn acl_bytes(&self, acl: *const ACL) -> io::Result<Option<&[u8]>> {
        if acl.is_null() {
            return Ok(None);
        }
        let mut info = ACL_SIZE_INFORMATION::default();
        // SAFETY: the ACL belongs to this live descriptor and info is the correct output type.
        unsafe {
            if GetAclInformation(
                acl,
                (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            // Skip the header: AclSize includes unused storage and can differ for identical ACEs.
            Ok(Some(slice::from_raw_parts(
                acl.cast::<u8>().add(size_of::<ACL>()),
                info.AclBytesInUse as usize - size_of::<ACL>(),
            )))
        }
    }

    pub(super) fn finish_staging(&self, file: &File) -> io::Result<()> {
        if !self.matches(file)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "temporary file access changed",
            ));
        }
        Ok(())
    }
}

impl Drop for Access {
    fn drop(&mut self) {
        if !self.descriptor.is_null() {
            // SAFETY: GetSecurityInfo allocated this descriptor for LocalFree.
            unsafe { LocalFree(self.descriptor) };
        }
    }
}

fn reject_central_access_policy(sacl: *const ACL) -> io::Result<()> {
    // SYSTEM_SCOPED_POLICY_ID_ACE_TYPE, MS-DTYP §2.4.4.1.
    const SCOPED_POLICY_ID: u8 = 0x13;
    if sacl.is_null() {
        return Ok(());
    }
    // SAFETY: callers pass a live SACL returned by Windows. GetAce resolves each entry.
    unsafe {
        for index in 0..u32::from((*sacl).AceCount) {
            let mut ace = null_mut();
            if GetAce(sacl, index, &mut ace) == 0 {
                return Err(io::Error::last_os_error());
            }
            if (*ace.cast::<ACE_HEADER>()).AceType == SCOPED_POLICY_ID {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "central access policy cannot be preserved",
                ));
            }
        }
    }
    Ok(())
}

fn check_auxiliary_data(file: &File, zone: bool) -> io::Result<()> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the file handle and correctly sized output are valid.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    check_writable_attributes(info.dwFileAttributes)?;
    if info.dwFileAttributes & !(FILE_ATTRIBUTE_NORMAL | FILE_ATTRIBUTE_ARCHIVE) != 0
        || info.nNumberOfLinks != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file attributes or hard links cannot be preserved",
        ));
    }

    const UNNAMED: [u16; 7] = [58, 58, 36, 68, 65, 84, 65]; // ::$DATA
    // The aligned buffer has space for the unnamed stream and, for downloads, a zone stream.
    let mut buffer = [0u64; 32];
    // SAFETY: the output buffer is writable and its byte size is supplied.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStreamInfo,
            buffer.as_mut_ptr().cast(),
            size_of_val(&buffer) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut offset = 0;
    let mut extra = false;
    loop {
        let header = offset + offset_of!(FILE_STREAM_INFO, StreamName);
        if header > size_of_val(&buffer) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        // SAFETY: the entry header lies inside the aligned, initialized buffer.
        let entry = unsafe { buffer.as_ptr().cast::<u8>().add(offset).cast::<FILE_STREAM_INFO>().read_unaligned() };
        let length = entry.StreamNameLength as usize;
        if header + length > size_of_val(&buffer) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        // SAFETY: the name bytes were bounds-checked against the buffer above.
        let name: Vec<u16> = unsafe {
            slice::from_raw_parts(buffer.as_ptr().cast::<u8>().add(header), length)
                .chunks_exact(2)
                .map(|unit| u16::from_ne_bytes([unit[0], unit[1]]))
                .collect()
        };
        let allowed = name == UNNAMED
            || (zone && String::from_utf16_lossy(&name).eq_ignore_ascii_case(":Zone.Identifier:$DATA"));
        extra |= !allowed;
        if entry.NextEntryOffset == 0 {
            break;
        }
        offset += entry.NextEntryOffset as usize;
    }
    let mut ea = FILE_EA_INFORMATION { EaSize: 0 };
    // SAFETY: the synchronous file handle and fixed-size NT query output are live.
    let status = unsafe {
        NtQueryInformationFile(
            file.as_raw_handle(),
            &mut IO_STATUS_BLOCK::default(),
            (&mut ea as *mut FILE_EA_INFORMATION).cast(),
            size_of_val(&ea) as u32,
            FileEaInformation,
        )
    };
    if status < 0 {
        return Err(nt_error(status));
    }
    if extra || ea.EaSize != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file streams or extended attributes cannot be preserved",
        ));
    }
    Ok(())
}

fn reopen(file: &File, access: u32, share: u32) -> io::Result<File> {
    // SAFETY: ReOpenFile borrows a live handle and checks the requested access.
    let handle = unsafe { ReOpenFile(file.as_raw_handle(), access, share, 0) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ReOpenFile returned a new, owned handle.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// 인터넷 영역(ZoneId=3)이다. HostUrl은 S3 주소를 남기지 않도록 `about:internet`으로 둔다.
pub(super) const ZONE_IDENTIFIER: &[u8] = b"[ZoneTransfer]\r\nZoneId=3\r\nHostUrl=about:internet\r\n";

pub(super) struct Temporary<'a> {
    directory: &'a Directory,
    file: Option<File>,
}

// The documented variable-length rename request, with space for a Windows file name.
#[repr(C)]
struct Rename {
    flags: u32,
    root: std::os::windows::io::RawHandle,
    length: u32,
    name: [u16; 255],
}

impl Temporary<'_> {
    /// 게시 전 임시 파일에 인터넷 영역 표시(Mark of the Web)를 기록한다. 이름을 바꿔도 스트림은 파일과 함께 간다.
    pub(super) fn mark_internet(&mut self) -> io::Result<()> {
        use std::io::Write;
        let stream = Directory::open_at(
            self.file(), OsStr::new(":Zone.Identifier"), FILE_OVERWRITE_IF, GENERIC_WRITE, null(), false,
        ).map_err(nt_error)?;
        let mut stream = stream;
        stream.write_all(ZONE_IDENTIFIER)?;
        stream.flush()
    }

    pub(super) fn file(&mut self) -> &mut File {
        self.file.as_mut().expect("temporary file is open")
    }

    pub(super) fn install(self, name: &OsStr, replace: bool) -> io::Result<()> {
        self.rename(name, replace).map(drop)
    }

    pub(super) fn publish_editor(
        self,
        name: &OsStr,
        expected: &File,
        verify: impl FnOnce(&File) -> io::Result<()>,
    ) -> Result<File, PublicationError> {
        self.publish_editor_inner(name, expected, verify, || {})
    }

    fn publish_editor_inner(
        self,
        name: &OsStr,
        expected: &File,
        verify: impl FnOnce(&File) -> io::Result<()>,
        after_backup: impl FnOnce(),
    ) -> Result<File, PublicationError> {
        let directory = self.directory;
        self.publish_existing(
            name,
            expected,
            false,
            |file| verify(file),
            after_backup,
            |published| {
                // 공유를 막는 임시 핸들을 닫아야 원래 이름으로 읽기 핸들을 열 수 있다.
                drop(published);
                directory.open_file(name)?.ok_or_else(|| io::ErrorKind::NotFound.into())
            },
        )
    }

    pub(super) fn publish_download(
        self,
        name: &OsStr,
        current: &File,
        verify: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> Result<(), PublicationError> {
        self.publish_download_inner(name, current, verify, || {})
    }

    fn publish_download_inner(
        self,
        name: &OsStr,
        current: &File,
        verify: impl FnOnce(&mut File) -> io::Result<()>,
        after_backup: impl FnOnce(),
    ) -> Result<(), PublicationError> {
        self.publish_existing(name, current, true, verify, after_backup, Ok).map(drop)
    }

    fn publish_existing(
        mut self,
        name: &OsStr,
        current: &File,
        zone: bool,
        verify: impl FnOnce(&mut File) -> io::Result<()>,
        after_backup: impl FnOnce(),
        retain: impl FnOnce(File) -> io::Result<File>,
    ) -> Result<File, PublicationError> {
        let directory = self.directory;
        let backup = temporary_name()?;
        let original_id = file_id(current)?;
        // Deny other data writers and deleters while checking and moving the exact object.
        let mut original = reopen(current, GENERIC_READ | FILE_WRITE_DATA | DELETE, FILE_SHARE_READ)?;
        if file_id(&original)? != original_id {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists).into());
        }
        let access = Access::read_preserved(&original, zone)?;
        let staged_id = file_id(self.file())?;
        verify(&mut original)?;
        if directory.entry_id(name)? != Some(original_id) {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists).into());
        }
        access.finish_staging(self.file())?;
        if !directory.still_at_path()? || !access.matches_preserved(&original, zone)? {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists).into());
        }

        rename_at(directory, &original, &backup, 0)?;
        let unconfirmed = || {
            let path = final_path(&original)
                .filter(|path| path.file_name() == Some(backup.as_os_str()))
                .unwrap_or_else(|| {
                    final_path(&directory.file)
                        .as_deref()
                        .unwrap_or(&directory.path)
                        .join(&backup)
                });
            PublicationError::Unconfirmed(path)
        };
        after_backup();
        if directory.entry_id(&backup).ok() != Some(Some(original_id))
            || !matches!(access.matches_preserved(&original, zone), Ok(true))
            || !matches!(directory.still_at_path(), Ok(true))
        {
            return Err(unconfirmed());
        }
        rename_at(directory, self.file(), name, 0).map_err(|_| unconfirmed())?;
        // 게시가 끝나면 임시 파일 정리를 해제한다.
        let published = self.file.take().expect("temporary file is open");
        let published = retain(published).map_err(|_| unconfirmed())?;
        if access.finish_staging(&published).is_err()
            || file_id(&published).ok() != Some(staged_id)
            || !matches!(access.matches_preserved(&original, zone), Ok(true))
            || directory.entry_id(name).ok() != Some(Some(staged_id))
            || directory.entry_id(&backup).ok() != Some(Some(original_id))
            || !matches!(directory.still_at_path(), Ok(true))
        {
            return Err(unconfirmed());
        }

        let disposition = FILE_DISPOSITION_INFO_EX {
            Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        };
        // SAFETY: the verified backup handle still denies other data writers and deleters.
        if unsafe {
            SetFileInformationByHandle(
                original.as_raw_handle(),
                FileDispositionInfoEx,
                (&disposition as *const FILE_DISPOSITION_INFO_EX).cast(),
                size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
            )
        } == 0
        {
            return Err(unconfirmed());
        }
        drop(original);
        Ok(published)
    }

    fn rename(mut self, name: &OsStr, replace: bool) -> io::Result<File> {
        // Keep only metadata access after save so the tab does not retain write/delete access.
        let retained = reopen(self.file(), READ_CONTROL | FILE_READ_ATTRIBUTES, SHARE)?;
        let directory = self.directory;
        let flags = if replace {
            FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS
        } else {
            0
        };
        rename_at(directory, self.file(), name, flags)?;
        self.file.take();
        Ok(retained)
    }
}

fn rename_at(directory: &Directory, file: &File, name: &OsStr, flags: u32) -> io::Result<()> {
    let mut request = Rename {
        flags,
        root: directory.file.as_raw_handle(),
        length: 0,
        name: [0; 255],
    };
    for (index, ch) in name.encode_wide().enumerate() {
        *request
            .name
            .get_mut(index)
            .ok_or(io::ErrorKind::InvalidFilename)? = ch;
        request.length += 2;
    }
    let size = (offset_of!(Rename, name) as u32 + request.length)
        .max(size_of::<FILE_RENAME_INFORMATION>() as u32);
    // SAFETY: the synchronous handle and request are live; the destination is one component
    // relative to the held directory. No copy/delete fallback is used.
    let status = unsafe {
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut IO_STATUS_BLOCK::default(),
            (&request as *const Rename).cast(),
            size,
            FileRenameInformationEx,
        )
    };
    if status < 0 { Err(nt_error(status)) } else { Ok(()) }
}

fn final_path(file: &File) -> Option<PathBuf> {
    // An ancestor may have moved after the directory was opened.
    let mut path = vec![0; MAX_PATH as usize];
    loop {
        // SAFETY: the file handle and UTF-16 output buffer are live.
        let len = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                path.as_mut_ptr(),
                path.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        } as usize;
        if len == 0 {
            return None;
        }
        if len < path.len() {
            return Some(OsString::from_wide(&path[..len]).into());
        }
        path.resize(len + 1, 0);
    }
}

impl Drop for Temporary<'_> {
    fn drop(&mut self) {
        if let Some(file) = &self.file {
            let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
            // SAFETY: this handle names only the staging object, even if someone renamed it.
            unsafe {
                SetFileInformationByHandle(
                    file.as_raw_handle(),
                    FileDispositionInfo,
                    (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                    size_of::<FILE_DISPOSITION_INFO>() as u32,
                )
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };

    #[test]
    fn cli_rejects_directory_reparse_points_in_the_parent_path() {
        use std::{fs, os::windows::fs::symlink_dir};

        let sandbox = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(sandbox.path()).unwrap();
        let target = base.join("target");
        let alias = base.join("alias");
        fs::create_dir_all(target.join("nested")).unwrap();
        symlink_dir(&target, &alias).unwrap();
        assert!(Directory::open_cli(&target.join("nested")).is_ok());
        assert!(Directory::open_cli(&alias).is_err());
        assert!(Directory::open_cli(&alias.join("nested")).is_err());
    }

    #[test]
    fn central_access_policy_is_rejected_even_with_a_mandatory_label() {
        for (sddl, has_policy) in [
            ("S:(ML;;NW;;;ME)", false),
            ("S:(ML;;NW;;;ME)(SP;;;;;S-1-17-1-2-3-4)", true),
        ] {
            let wide: Vec<_> = sddl.encode_utf16().chain([0]).collect();
            let mut access = Access {
                descriptor: null_mut(),
                owner: null_mut(),
                group: null_mut(),
                dacl: null_mut(),
                label: null_mut(),
                protected: false,
            };
            // SAFETY: Windows parses this in-memory fixture; no filesystem policy is set.
            unsafe {
                assert_ne!(
                    ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        wide.as_ptr(),
                        SDDL_REVISION_1,
                        &mut access.descriptor,
                        null_mut()
                    ),
                    0,
                    "{}",
                    io::Error::last_os_error()
                );
                assert_ne!(
                    GetSecurityDescriptorSacl(access.descriptor, &mut 0, &mut access.label, &mut 0),
                    0
                );
            }
            let result = reject_central_access_policy(access.label);
            if has_policy {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Unsupported);
            } else {
                result.unwrap();
            }
        }
    }

    #[test]
    fn download_publication_preserves_a_late_claim_and_cleans_up_only_on_success() {
        use std::{
            fs,
            io::{Read, Write},
        };

        for claimed in [false, true] {
            let sandbox = tempfile::tempdir().unwrap();
            let directory = Directory::open(fs::canonicalize(sandbox.path()).unwrap()).unwrap();
            let name = OsStr::new("target.txt");
            let path = directory.path.join(name);
            fs::write(&path, b"original").unwrap();
            let current = directory.open_file(name).unwrap().unwrap();
            let access = Access::read(&current).unwrap();
            let mut stage = directory.stage(Some(&access)).unwrap();
            stage.file().write_all(b"download").unwrap();
            let verify = |file: &mut File| {
                let mut actual = Vec::new();
                file.read_to_end(&mut actual)?;
                assert_eq!(actual, b"original");
                Ok(())
            };
            let result = if claimed {
                stage.publish_download_inner(name, &current, verify, || {
                    File::create_new(&path).unwrap().write_all(b"external").unwrap();
                })
            } else {
                stage.publish_download(name, &current, verify)
            };
            if claimed {
                let Err(PublicationError::Unconfirmed(recovery)) = result else {
                    panic!("{result:?}");
                };
                assert_eq!(fs::read(recovery).unwrap(), b"original");
                assert_eq!(fs::read(&path).unwrap(), b"external");
            } else {
                result.unwrap();
                assert_eq!(fs::read(&path).unwrap(), b"download");
            }
            assert_eq!(fs::read_dir(&directory.path).unwrap().count(), 1 + usize::from(claimed));
        }
    }

    #[test]
    fn editor_returns_a_reader_or_preserves_both_original_and_late_claim() {
        use std::{fs, io::{Read, Write}};

        for claimed in [false, true] {
            let sandbox = tempfile::tempdir().unwrap();
            let directory = Directory::open(fs::canonicalize(sandbox.path()).unwrap()).unwrap();
            let name = OsStr::new("target.txt");
            let path = directory.path.join(name);
            fs::write(&path, b"original").unwrap();
            let current = directory.open_file(name).unwrap().unwrap();
            let access = Access::read_editor(&current).unwrap();
            let mut stage = directory.stage(Some(&access)).unwrap();
            stage.file().write_all(b"edited").unwrap();
            let staged_id = file_id(stage.file()).unwrap();
            let result = stage.publish_editor_inner(name, &current, |mut file| {
                let mut bytes = [0; 8];
                file.read_exact(&mut bytes)?;
                assert_eq!(&bytes, b"original");
                Ok(())
            }, || {
                if claimed {
                    File::create_new(&path).unwrap().write_all(b"outside").unwrap();
                }
            });
            if claimed {
                let Err(PublicationError::Unconfirmed(recovery)) = result else {
                    panic!("{result:?}");
                };
                assert_eq!(fs::read(recovery).unwrap(), b"original");
                assert_eq!(fs::read(&path).unwrap(), b"outside");
            } else {
                let mut file = result.unwrap();
                let mut bytes = [0; 6];
                file.read_exact(&mut bytes).unwrap();
                assert_eq!(&bytes, b"edited");
                assert_eq!(file_id(&file).unwrap(), staged_id);
                assert_eq!(directory.entry_id(name).unwrap(), Some(staged_id));
            }
            assert_eq!(fs::read_dir(&directory.path).unwrap().count(), 1 + usize::from(claimed));
        }
    }

    #[test]
    fn editor_preserves_auxiliary_data_on_rejection() {
        use std::{fs, io::Write};

        for kind in ["ADS", "EA", "hard link"] {
            let sandbox = tempfile::tempdir().unwrap();
            let directory = Directory::open(fs::canonicalize(sandbox.path()).unwrap()).unwrap();
            let name = OsStr::new("target.txt");
            let path = directory.path.join(name);
            let stream = directory.path.join("target.txt:outside");
            let link = directory.path.join("outside.txt");
            fs::write(&path, b"original").unwrap();
            let current = directory.open_file(name).unwrap().unwrap();
            let access = Access::read_editor(&current).unwrap();
            let mut stage = directory.stage(Some(&access)).unwrap();
            stage.file().write_all(b"edited").unwrap();

            #[repr(align(4))]
            struct EaRecord([u8; 17]);
            // FILE_FULL_EA_INFORMATION: 다음 항목 없음, 이름 X, 값 outside.
            let ea = EaRecord(*b"\0\0\0\0\0\x01\x07\0X\0outside");
            match kind {
                "ADS" => fs::write(&stream, b"outside").unwrap(),
                "EA" => {
                    let file = reopen(&current, FILE_WRITE_EA, SHARE).unwrap();
                    // SAFETY: 동기 파일 핸들과 EA 레코드가 호출 동안 유효하다.
                    let status = unsafe {
                        NtSetEaFile(file.as_raw_handle(), &mut IO_STATUS_BLOCK::default(),
                            ea.0.as_ptr().cast(), ea.0.len() as u32)
                    };
                    assert_eq!(status, 0, "{}", nt_error(status));
                }
                _ => fs::hard_link(&path, &link).unwrap(),
            }

            assert!(access.matches_editor(&current).is_err(), "{kind}");
            assert!(matches!(stage.publish_editor(name, &current, |_| Ok(())),
                Err(PublicationError::Io(_))), "{kind}");
            assert_eq!(fs::read(&path).unwrap(), b"original");
            match kind {
                "ADS" => assert_eq!(fs::read(&stream).unwrap(), b"outside"),
                "EA" => {
                    let mut output = [0u32; 8];
                    let file = File::open(&path).unwrap();
                    // SAFETY: 동기 파일 핸들과 정렬된 쓰기 버퍼가 호출 동안 유효하다.
                    let status = unsafe {
                        NtQueryEaFile(file.as_raw_handle(), &mut IO_STATUS_BLOCK::default(),
                            output.as_mut_ptr().cast(), size_of_val(&output) as u32,
                            true, null(), 0, null(), true)
                    };
                    assert_eq!(status, 0, "{}", nt_error(status));
                    // SAFETY: 데이터 범위가 초기화된 출력 버퍼 안에 있다.
                    let bytes = unsafe { slice::from_raw_parts(output.as_ptr().cast::<u8>(), ea.0.len()) };
                    assert_eq!(bytes, ea.0);
                }
                _ => assert_eq!(fs::read(&link).unwrap(), b"original"),
            }
            assert_eq!(fs::read_dir(&directory.path).unwrap().count(), if kind == "hard link" { 2 } else { 1 });
        }
    }

    #[test]
    fn editor_preserves_changes_made_before_the_original_is_frozen() {
        use std::{fs, io::{Read, Write}};

        for replaced in [false, true] {
            let sandbox = tempfile::tempdir().unwrap();
            let directory = Directory::open(fs::canonicalize(sandbox.path()).unwrap()).unwrap();
            let name = OsStr::new("target.txt");
            let path = directory.path.join(name);
            let moved = directory.path.join("moved.txt");
            fs::write(&path, b"original").unwrap();
            let current = directory.open_file(name).unwrap().unwrap();
            let access = Access::read_editor(&current).unwrap();
            let mut stage = directory.stage(Some(&access)).unwrap();
            stage.file().write_all(b"edited").unwrap();
            if replaced {
                fs::rename(&path, &moved).unwrap();
            }
            fs::write(&path, b"external").unwrap();
            let result = stage.publish_editor(name, &current, |mut file| {
                let mut bytes = [0; 8];
                file.read_exact(&mut bytes)?;
                if &bytes != b"original" {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                Ok(())
            });
            let Err(PublicationError::Io(error)) = result else { panic!("{result:?}") };
            let expected = if replaced { io::ErrorKind::AlreadyExists } else { io::ErrorKind::InvalidData };
            assert_eq!(error.kind(), expected);
            assert_eq!(fs::read(&path).unwrap(), b"external");
            if replaced {
                assert_eq!(fs::read(&moved).unwrap(), b"original");
            }
            assert_eq!(fs::read_dir(&directory.path).unwrap().count(), 1 + usize::from(replaced));
        }
    }

    #[test]
    fn editor_preserves_an_open_writers_content_on_sharing_violation() {
        use std::{fs, io::Write};
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

        let sandbox = tempfile::tempdir().unwrap();
        let directory = Directory::open(fs::canonicalize(sandbox.path()).unwrap()).unwrap();
        let name = OsStr::new("target.txt");
        let path = directory.path.join(name);
        fs::write(&path, b"original").unwrap();
        let current = directory.open_file(name).unwrap().unwrap();
        let access = Access::read_editor(&current).unwrap();
        let mut stage = directory.stage(Some(&access)).unwrap();
        stage.file().write_all(b"edited").unwrap();
        let mut writer = File::options().append(true).share_mode(SHARE).open(&path).unwrap();
        writer.write_all(b" outside").unwrap();
        let result = stage.publish_editor(name, &current, |_| panic!("writer still holds the original"));
        let Err(PublicationError::Io(error)) = result else { panic!("{result:?}") };
        assert_eq!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));
        writer.write_all(b" writer").unwrap();
        drop(writer);
        assert_eq!(fs::read(&path).unwrap(), b"original outside writer");
        assert_eq!(fs::read_dir(&directory.path).unwrap().count(), 1);
    }
}
