use std::{
    ffi::OsStr,
    fs::File,
    io,
    mem::{offset_of, size_of},
    os::windows::{
        ffi::OsStrExt,
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
            GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree, OBJ_CASE_INSENSITIVE,
            RtlNtStatusToDosError, STATUS_FILE_IS_A_DIRECTORY, STATUS_NAME_TOO_LONG,
            UNICODE_STRING,
        },
        Security::{
            ACE_HEADER,
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
            *,
        },
        Storage::FileSystem::*,
        System::IO::IO_STATUS_BLOCK,
    },
};

use super::temporary_name;

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

pub(super) struct Directory {
    pub(super) path: PathBuf,
    file: File,
    id: FileId,
}

impl Directory {
    pub(super) fn open(path: PathBuf) -> io::Result<Self> {
        let file = Self::open_path(&path)?;
        let id = file_id(&file)?;
        Ok(Self { path, file, id })
    }

    fn open_path(path: &Path) -> io::Result<File> {
        let file = File::options()
            .access_mode(FILE_READ_ATTRIBUTES | FILE_TRAVERSE)
            .share_mode(SHARE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::ErrorKind::NotADirectory.into());
        }
        Ok(file)
    }

    pub(super) fn still_at_path(&self) -> io::Result<bool> {
        Ok(file_id(&Self::open_path(&self.path)?)? == self.id)
    }

    pub(super) fn open_file(&self, name: &OsStr) -> io::Result<Option<File>> {
        self.open_regular(name, GENERIC_READ)
    }

    pub(super) fn check_replace_access(&self, _: &OsStr, file: &File) -> io::Result<()> {
        let metadata = file.metadata()?;
        if metadata.permissions().readonly() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "original file is read-only",
            ));
        }
        if metadata.file_attributes() & FILE_ATTRIBUTE_ENCRYPTED != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "encrypted file replacement",
            ));
        }
        drop(reopen(file, FILE_WRITE_DATA)?);
        Ok(())
    }
    pub(super) fn entry_id(&self, name: &OsStr) -> io::Result<Option<FileId>> {
        self.open_regular(name, FILE_READ_ATTRIBUTES)?
            .as_ref()
            .map(file_id)
            .transpose()
    }

    fn open_regular(&self, name: &OsStr, access: u32) -> io::Result<Option<File>> {
        let file = match self.open_at(name, FILE_OPEN, access, null()) {
            Err(STATUS_FILE_IS_A_DIRECTORY) => return Ok(None),
            result => result.map_err(nt_error)?,
        };
        let metadata = file.metadata()?;
        Ok(
            (metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0)
                .then_some(file),
        )
    }

    pub(super) fn stage(&self, access: &Access) -> io::Result<Temporary<'_>> {
        let name = temporary_name()?;
        let file = self
            .open_at(
                &name,
                FILE_CREATE,
                GENERIC_READ | GENERIC_WRITE | DELETE,
                access.descriptor.cast(),
            )
            .map_err(nt_error)?;
        let temp = Temporary {
            directory: self,
            file: Some(file),
        };
        // Verify the inherited descriptor before exposing any buffer bytes in the directory.
        if !access.matches(temp.file.as_ref().expect("temporary file is open"))? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "temporary file access differs",
            ));
        }
        Ok(temp)
    }

    fn open_at(
        &self,
        name: &OsStr,
        disposition: u32,
        access: u32,
        descriptor: *const SECURITY_DESCRIPTOR,
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
            RootDirectory: self.file.as_raw_handle(),
            ObjectName: &name,
            Attributes: OBJ_CASE_INSENSITIVE,
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
                FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
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

fn reopen(file: &File, access: u32) -> io::Result<File> {
    // SAFETY: ReOpenFile borrows a live handle and checks the requested access.
    let handle = unsafe { ReOpenFile(file.as_raw_handle(), access, SHARE, 0) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ReOpenFile returned a new, owned handle.
    Ok(unsafe { File::from_raw_handle(handle) })
}

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
    pub(super) fn file(&mut self) -> &mut File {
        self.file.as_mut().expect("temporary file is open")
    }

    pub(super) fn replace(mut self, name: &OsStr) -> io::Result<File> {
        let mut request = Rename {
            flags: FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS,
            root: self.directory.file.as_raw_handle(),
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
        // Keep only metadata access after save so the tab does not retain write/delete access.
        let retained = reopen(self.file(), READ_CONTROL | FILE_READ_ATTRIBUTES)?;
        let size = (offset_of!(Rename, name) as u32 + request.length)
            .max(size_of::<FILE_RENAME_INFORMATION>() as u32);
        // SAFETY: the synchronous handle and request are live; the destination is one component
        // relative to the held directory. No copy/delete fallback is used.
        let status = unsafe {
            NtSetInformationFile(
                self.file().as_raw_handle(),
                &mut IO_STATUS_BLOCK::default(),
                (&request as *const Rename).cast(),
                size,
                FileRenameInformationEx,
            )
        };
        if status < 0 {
            return Err(nt_error(status));
        }
        self.file.take();
        Ok(retained)
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
}
