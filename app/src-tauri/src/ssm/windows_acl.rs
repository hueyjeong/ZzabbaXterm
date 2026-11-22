use std::{
    fs::{File, OpenOptions},
    io,
    mem::{MaybeUninit, size_of},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr::null_mut,
};
use windows_sys::{
    Win32::{
        Foundation::{GENERIC_ALL, GENERIC_WRITE, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER,
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                ConvertStringSidToSidW, GetSecurityInfo, SE_FILE_OBJECT,
            },
            DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetTokenInformation, INHERIT_ONLY_ACE,
            IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
            SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE, TOKEN_QUERY, TOKEN_USER, TokenUser,
            WinBuiltinAdministratorsSid, WinLocalSystemSid,
        },
        Storage::FileSystem::{
            CreateDirectoryW, DELETE, FILE_APPEND_DATA, FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA,
            FILE_WRITE_EA, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
        },
        System::{
            Com::CoTaskMemFree,
            SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE},
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
        UI::Shell::SHGetKnownFolderPath,
    },
    core::GUID,
};

struct Local(PSECURITY_DESCRIPTOR);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

#[repr(C)]
struct UserBuffer {
    user: TOKEN_USER,
    tail: [u8; SECURITY_MAX_SID_SIZE as usize],
}

fn user(buffer: &mut MaybeUninit<UserBuffer>) -> io::Result<PSID> {
    let mut raw = null_mut();
    let mut len = 0;
    // 토큰 핸들을 소유하고 SID를 호출자의 정렬된 버퍼에 둔다.
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle::from_raw_handle(raw);
        if GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size_of::<UserBuffer>() as u32,
            &mut len,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok((*buffer.as_ptr()).user.User.Sid)
    }
}

fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
    text.encode_wide().chain([0]).collect()
}
fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "SSM directory ACL is unsafe",
    )
}

pub(super) fn known_folder(id: &GUID) -> io::Result<PathBuf> {
    let mut ptr = null_mut();
    // 성공 시 반환된 UTF-16 경로는 CoTaskMemFree로 해제한다.
    unsafe {
        let result = SHGetKnownFolderPath(id, 0, null_mut(), &mut ptr);
        if result < 0 || ptr.is_null() {
            return Err(io::Error::from_raw_os_error(result));
        }
        let len = (0..).take_while(|&n| *ptr.add(n) != 0).count();
        let path = PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(
            ptr, len,
        )));
        CoTaskMemFree(ptr.cast());
        Ok(path)
    }
}

fn descriptor(sddl: &str) -> io::Result<Local> {
    let mut raw = null_mut();
    // SDDL과 결과 포인터는 호출 동안 유효하고 결과는 Local이 해제한다.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide(sddl.as_ref()).as_ptr(),
            1,
            &mut raw,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Local(raw))
}

fn create_dir(path: &Path, descriptor: &Local) -> io::Result<()> {
    let attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    // 새 디렉터리와 보호 DACL을 한 호출로 만든다.
    if unsafe { CreateDirectoryW(wide(path.as_os_str()).as_ptr(), &attrs) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn user_sid_text() -> io::Result<String> {
    let mut buffer = MaybeUninit::uninit();
    let mut ptr = null_mut();
    // SID 문자열은 LocalFree 전까지 유효하다.
    unsafe {
        if ConvertSidToStringSidW(user(&mut buffer)?, &mut ptr) == 0 {
            return Err(io::Error::last_os_error());
        }
        let _sid = Local(ptr.cast());
        let len = (0..).take_while(|&n| *ptr.add(n) != 0).count();
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
            ptr, len,
        )))
    }
}

pub(super) fn private_dir(path: &Path) -> io::Result<()> {
    let sid = user_sid_text()?;
    create_dir(
        path,
        &descriptor(&format!("O:{sid}D:P(A;OICI;FA;;;{sid})"))?,
    )
}

pub(super) fn trusted_chain(path: &Path, file: bool) -> io::Result<()> {
    with_install_validator(|check| {
        let defining_dir = if file {
            path.parent().ok_or_else(denied)?
        } else {
            path
        };
        for entry in path.ancestors() {
            check(
                entry,
                entry != path || !file,
                entry == path || entry == defining_dir,
            )?;
        }
        Ok(())
    })
}

pub(super) fn with_install_validator<T>(
    validate: impl FnOnce(&dyn Fn(&Path, bool, bool) -> io::Result<()>) -> io::Result<T>,
) -> io::Result<T> {
    let mut buffer = MaybeUninit::uninit();
    let current = user(&mut buffer)?;
    // Windows Resource Protection의 TrustedInstaller 서비스 SID다.
    let mut installer = null_mut();
    let sid = wide("S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464".as_ref());
    if unsafe { ConvertStringSidToSidW(sid.as_ptr(), &mut installer) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _installer = Local(installer);
    validate(&|entry, directory, check_add| {
        let flags = FILE_FLAG_OPEN_REPARSE_POINT
            | if directory {
                FILE_FLAG_BACKUP_SEMANTICS
            } else {
                0
            };
        let object = OpenOptions::new()
            .access_mode(READ_CONTROL)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(flags)
            .open(entry)?;
        let meta = object.metadata()?;
        if meta.is_dir() != directory || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(denied());
        }
        let mut mask = DELETE
            | WRITE_DAC
            | WRITE_OWNER
            | FILE_DELETE_CHILD
            | FILE_WRITE_ATTRIBUTES
            | FILE_WRITE_EA
            | GENERIC_ALL
            | GENERIC_WRITE;
        // 기존 조상에 새 형제를 만들 수 있어도 보호된 기존 경로를 바꿀 권한과는 다르다.
        if check_add {
            mask |= FILE_WRITE_DATA | FILE_APPEND_DATA;
        }
        check_writers(&object, current, installer, mask)
    })
}

fn check_writers(file: &File, current: PSID, installer: PSID, mask: u32) -> io::Result<()> {
    let (mut owner, mut dacl, mut raw) = (null_mut(), null_mut(), null_mut());
    // SID와 ACE는 GetSecurityInfo가 반환한 설명자의 수명 안에서만 읽는다.
    unsafe {
        let status = GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut raw,
        );
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let _descriptor = Local(raw);
        let trusted = |sid| {
            EqualSid(sid, current) != 0
                || EqualSid(sid, installer) != 0
                || IsWellKnownSid(sid, WinLocalSystemSid) != 0
                || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
        };
        if owner.is_null() || dacl.is_null() || !trusted(owner) {
            return Err(denied());
        }
        for index in 0..u32::from((*dacl).AceCount) {
            let mut ace = null_mut();
            if GetAce(dacl, index, &mut ace) == 0 {
                return Err(io::Error::last_os_error());
            }
            let header = &*ace.cast::<ACE_HEADER>();
            if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 {
                continue;
            }
            match u32::from(header.AceType) {
                ACCESS_DENIED_ACE_TYPE => (),
                ACCESS_ALLOWED_ACE_TYPE => {
                    let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
                    let sid = std::ptr::addr_of!(ace.SidStart).cast_mut().cast();
                    if ace.Mask & mask != 0 && !trusted(sid) {
                        return Err(denied());
                    }
                }
                _ => return Err(denied()),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use windows_sys::Win32::Security::{GetSecurityDescriptorControl, SE_DACL_PROTECTED};

    pub(in crate::ssm) fn fixture_dir(path: &Path, rights: &str) {
        let sid = user_sid_text().unwrap();
        let rules = format!("O:{sid}D:P(A;OICI;FA;;;{sid})(A;OICI;{rights};;;WD)");
        create_dir(path, &descriptor(&rules).unwrap()).unwrap();
    }

    pub(in crate::ssm) fn grant_everyone_write(path: &Path) {
        use windows_sys::Win32::Security::{
            Authorization::SetNamedSecurityInfoW, GetSecurityDescriptorDacl,
            PROTECTED_DACL_SECURITY_INFORMATION,
        };
        let sid = user_sid_text().unwrap();
        let rules = descriptor(&format!("D:P(A;;FA;;;{sid})(A;;GW;;;WD)")).unwrap();
        let (mut present, mut defaulted, mut dacl) = (0, 0, null_mut());
        // 시험이 만든 파일과 보안 설명자에만 적용한다.
        unsafe {
            assert_ne!(
                GetSecurityDescriptorDacl(rules.0, &mut present, &mut dacl, &mut defaulted),
                0
            );
            assert_eq!(
                SetNamedSecurityInfoW(
                    wide(path.as_os_str()).as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    dacl,
                    null_mut()
                ),
                0
            );
        }
    }

    pub(in crate::ssm) fn assert_user_only(path: &Path, directory: bool, protected: bool) {
        let file = OpenOptions::new()
            .access_mode(READ_CONTROL)
            .custom_flags(if directory {
                FILE_FLAG_BACKUP_SEMANTICS
            } else {
                0
            })
            .open(path)
            .unwrap();
        let mut buffer = MaybeUninit::uninit();
        let current = user(&mut buffer).unwrap();
        let (mut owner, mut dacl, mut raw) = (null_mut(), null_mut(), null_mut());
        // 테스트가 만든 객체의 설명자와 유효 기간 안의 ACE만 검사한다.
        unsafe {
            assert_eq!(
                GetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    &mut owner,
                    null_mut(),
                    &mut dacl,
                    null_mut(),
                    &mut raw
                ),
                0
            );
            let _descriptor = Local(raw);
            assert_ne!(EqualSid(owner, current), 0);
            let mut control = 0;
            let mut revision = 0;
            assert_ne!(
                GetSecurityDescriptorControl(raw, &mut control, &mut revision),
                0
            );
            assert_eq!(control & SE_DACL_PROTECTED != 0, protected);
            assert_eq!((*dacl).AceCount, 1);
            let mut ace = null_mut();
            assert_ne!(GetAce(dacl, 0, &mut ace), 0);
            let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            assert_eq!(u32::from(ace.Header.AceType), ACCESS_ALLOWED_ACE_TYPE);
            assert_ne!(
                EqualSid(std::ptr::addr_of!(ace.SidStart).cast_mut().cast(), current),
                0
            );
        }
    }

    #[test]
    fn installation_read_is_allowed_but_other_account_write_is_rejected() {
        let parent = tempfile::tempdir().unwrap();
        let read = parent.path().join("read");
        let write = parent.path().join("write");
        fixture_dir(&read, "GRGX");
        fixture_dir(&write, "GRGWGX");
        let mut buffer = MaybeUninit::uninit();
        let current = user(&mut buffer).unwrap();
        for (path, expected) in [(read, true), (write, false)] {
            let file = OpenOptions::new()
                .access_mode(READ_CONTROL)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(path)
                .unwrap();
            assert_eq!(
                check_writers(&file, current, current, FILE_WRITE_DATA | GENERIC_WRITE).is_ok(),
                expected
            );
        }
    }
}
