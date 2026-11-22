use std::{
    fs::{File, OpenOptions},
    io,
    mem::{MaybeUninit, size_of},
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::{AsRawHandle, FromRawHandle, OwnedHandle}},
    path::Path,
    ptr::null_mut,
};

use windows_sys::Win32::{
    Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree},
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetSecurityDescriptorControl, GetTokenInformation, IsWellKnownSid,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
        SECURITY_MAX_SID_SIZE, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER, TokenUser,
        WinBuiltinAdministratorsSid, WinLocalSystemSid,
        Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SE_FILE_OBJECT},
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateDirectoryW, CreateFileW,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW, GetFileInformationByHandle, READ_CONTROL,
    },
    System::{
        SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE},
        Threading::{GetCurrentProcess, OpenProcessToken},
        WindowsProgramming::DRIVE_FIXED,
    },
};

use super::Error;

pub(super) type FileId = (u32, u32, u32);

pub(super) struct RootGuard {
    pub(super) id: FileId,
    _chain: Vec<File>,
}

pub(super) fn root(path: &Path) -> Result<RootGuard, Error> {
    let (chain, id) = directory_chain(path, true)?;
    Ok(RootGuard { id, _chain: chain })
}

pub(super) fn creation_parent(path: &Path) -> Result<Vec<File>, Error> {
    let parent = path.parent().ok_or(Error::InvalidPath)?;
    Ok(directory_chain(parent, false)?.0)
}

fn directory_chain(path: &Path, check_acl: bool) -> Result<(Vec<File>, FileId), Error> {
    let ancestors: Vec<_> = path.ancestors().collect();
    let drive: Vec<_> = ancestors.last().ok_or(Error::InvalidPath)?
        .as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: 드라이브 경로는 NUL로 끝난다.
    if unsafe { GetDriveTypeW(drive.as_ptr()) } != DRIVE_FIXED {
        return Err(Error::InvalidPath);
    }
    let mut chain = Vec::with_capacity(ancestors.len());
    let mut id = None;
    for component in ancestors.into_iter().rev() {
        let file = open(component, true, check_acl && component == path)?;
        let info = info(&file)?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
            || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(Error::InvalidPath);
        }
        if component == path {
            if check_acl {
                private_acl(&file, Error::InvalidPath)?;
            }
            id = Some((info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow));
        }
        chain.push(file);
    }
    Ok((chain, id.ok_or(Error::InvalidPath)?))
}

pub(super) fn pin(path: &Path) -> Result<File, Error> {
    let file = open(path, false, true)?;
    let info = info(&file)?;
    if info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
        || info.nNumberOfLinks != 1
    {
        return Err(Error::InvalidTrustRecord);
    }
    private_acl(&file, Error::InvalidTrustRecord)?;
    Ok(file)
}

pub(super) fn check_public_key_file(file: &File) -> Result<(), Error> {
    let info = info(file)?;
    if info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
        || info.nNumberOfLinks != 1
    {
        return Err(Error::InvalidPublicKey);
    }
    Ok(())
}

fn open(path: &Path, directory: bool, acl: bool) -> io::Result<File> {
    let flags = FILE_FLAG_OPEN_REPARSE_POINT | if directory { FILE_FLAG_BACKUP_SEMANTICS } else { 0 };
    let access = (if directory { FILE_READ_ATTRIBUTES } else { GENERIC_READ }) | if acl { READ_CONTROL } else { 0 };
    let share = FILE_SHARE_READ | if directory { FILE_SHARE_WRITE } else { 0 };
    OpenOptions::new().access_mode(access).share_mode(share).custom_flags(flags).open(path)
}

fn info(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = MaybeUninit::uninit();
    // SAFETY: 열린 파일의 핸들과 충분한 출력 버퍼를 전달한다.
    unsafe {
        if GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.assume_init())
    }
}

#[repr(C)]
struct TokenUserBuffer {
    user: TOKEN_USER,
    sid: [u8; SECURITY_MAX_SID_SIZE as usize],
}

fn token_user(buffer: &mut MaybeUninit<TokenUserBuffer>) -> io::Result<PSID> {
    let mut raw = null_mut();
    let mut len = 0;
    // SAFETY: 출력 핸들을 소유권으로 넘기고, SID는 호출자가 제공한 정렬된 버퍼에 남는다.
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle::from_raw_handle(raw);
        if GetTokenInformation(token.as_raw_handle(), TokenUser, buffer.as_mut_ptr().cast(),
            size_of::<TokenUserBuffer>() as u32, &mut len) == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok((*buffer.as_ptr()).user.User.Sid)
    }
}

fn private_acl(file: &File, rejected: Error) -> Result<(), Error> {
    let mut token = MaybeUninit::uninit();
    let user = token_user(&mut token)?;
    let (mut owner, mut dacl, mut descriptor) = (null_mut(), null_mut(), null_mut());
    // SAFETY: SID/ACL 포인터는 성공 시 반환된 보안 설명자 안에 있고, 설명자는 순회 후 해제한다.
    unsafe {
        let status = GetSecurityInfo(file.as_raw_handle(), SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION, &mut owner, null_mut(),
            &mut dacl, null_mut(), &mut descriptor);
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32).into());
        }
        let _descriptor = LocalAllocation(descriptor);
        let mut control = 0;
        let mut revision = 0;
        if GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) == 0 {
            return Err(io::Error::last_os_error().into());
        }
        if control & SE_DACL_PROTECTED == 0 {
            return Err(Error::UnprotectedTrustAcl);
        }
        if owner.is_null() || dacl.is_null() || EqualSid(owner, user) == 0 {
            return Err(rejected);
        }
        for index in 0..u32::from((*dacl).AceCount) {
            let mut raw = null_mut();
            if GetAce(dacl, index, &mut raw) == 0 {
                return Err(io::Error::last_os_error().into());
            }
            match u32::from((*raw.cast::<ACE_HEADER>()).AceType) {
                ACCESS_DENIED_ACE_TYPE => continue,
                ACCESS_ALLOWED_ACE_TYPE => {
                    let sid = (&raw.cast::<ACCESS_ALLOWED_ACE>().as_ref().unwrap().SidStart as *const u32).cast_mut().cast();
                    if EqualSid(sid, user) == 0
                        && IsWellKnownSid(sid, WinLocalSystemSid) == 0
                        && IsWellKnownSid(sid, WinBuiltinAdministratorsSid) == 0
                    {
                        return Err(rejected);
                    }
                }
                _ => return Err(rejected),
            }
        }
    }
    Ok(())
}

struct LocalAllocation(PSECURITY_DESCRIPTOR);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: 포인터는 Win32가 LocalFree로 해제하도록 반환한 할당이다.
        unsafe { LocalFree(self.0) };
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

fn descriptor(protected: bool, shared: bool) -> io::Result<LocalAllocation> {
    let mut token = MaybeUninit::uninit();
    let mut sid_text = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: 변환 결과를 사용하는 동안 토큰 버퍼와 Win32 할당을 유지한다.
    unsafe {
        if ConvertSidToStringSidW(token_user(&mut token)?, &mut sid_text) == 0 {
            return Err(io::Error::last_os_error());
        }
        let _sid = LocalAllocation(sid_text.cast());
        let len = (0..).find(|&index| *sid_text.add(index) == 0).unwrap();
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, len));
        let flags = if protected { "P" } else { "" };
        let everyone = if shared { "(A;OICI;FA;;;WD)" } else { "" };
        let sddl: Vec<_> = format!("O:{sid}D:{flags}(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA){everyone}")
            .encode_utf16().chain([0]).collect();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1, &mut descriptor, null_mut()) == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(LocalAllocation(descriptor))
    }
}

fn security_attributes(descriptor: &LocalAllocation) -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    }
}

pub(super) fn create_dir(path: &Path) -> io::Result<()> {
    let descriptor = descriptor(true, false)?;
    // SAFETY: 경로와 보안 설명자는 호출이 끝날 때까지 유효하다.
    if unsafe { CreateDirectoryW(wide(path).as_ptr(), &security_attributes(&descriptor)) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn create_file(path: &Path) -> io::Result<File> {
    let descriptor = descriptor(true, false)?;
    // SAFETY: CREATE_NEW는 새 객체만 만들며 성공한 핸들은 File이 소유한다.
    unsafe {
        let handle = CreateFileW(wide(path).as_ptr(), GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE,
            &security_attributes(&descriptor), CREATE_NEW, FILE_ATTRIBUTE_NORMAL, null_mut());
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(File::from_raw_handle(handle))
    }
}

#[cfg(test)]
pub(super) fn set_test_acl(path: &Path, protected: bool, shared: bool) {
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION, UNPROTECTED_DACL_SECURITY_INFORMATION,
        Authorization::SetNamedSecurityInfoW,
    };
    let descriptor = descriptor(protected, shared).unwrap();
    let flags = if protected { PROTECTED_DACL_SECURITY_INFORMATION } else { UNPROTECTED_DACL_SECURITY_INFORMATION };
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl = null_mut();
    // SAFETY: 테스트가 소유한 임시 경로와 설명자만 사용한다.
    unsafe {
        assert_ne!(GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted), 0);
        assert_ne!(present, 0);
        assert!(!dacl.is_null());
        assert_eq!(SetNamedSecurityInfoW(wide(path).as_ptr(), SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | flags, null_mut(), null_mut(), dacl, null_mut()), 0);
    }
}
