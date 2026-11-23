use std::{
    ffi::OsString,
    io,
    os::windows::{
        ffi::OsStringExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
};

use windows_sys::Win32::{
    Foundation::{
        DUPLICATE_SAME_ACCESS, DuplicateHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE,
        WAIT_TIMEOUT,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_TYPE_PIPE, GetFileInformationByHandle, GetFileType,
    },
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        Threading::{
            GetCurrentProcess, GetProcessId, GetProcessTimes, OpenProcess, PROCESS_DUP_HANDLE,
            PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW, WaitForSingleObject,
        },
    },
};

use super::TrustedFile;

pub(super) fn check_install(path: &Path) -> io::Result<()> {
    crate::ssm::windows_acl::protected_executable(path)
}

pub(super) struct Handle(OwnedHandle);

impl Handle {
    pub(super) fn open(pid: u32) -> io::Result<Self> {
        use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
        // SAFETY: OpenProcess returns a new handle or NULL.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_DUP_HANDLE | SYNCHRONIZE,
                0,
                pid,
            )
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle) }))
    }

    pub(super) fn from_child(child: &dyn portable_pty::Child, pid: u32) -> io::Result<Self> {
        let borrowed = child.as_raw_handle().ok_or(io::ErrorKind::NotFound)?;
        let current = unsafe { GetCurrentProcess() };
        let owned = duplicate(current, borrowed)?;
        if unsafe { GetProcessId(owned.as_raw_handle()) } != pid {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(Self(owned))
    }

    pub(super) fn alive(&self) -> bool {
        // SAFETY: the handle is owned and has SYNCHRONIZE access.
        unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) == WAIT_TIMEOUT }
    }

    pub(super) fn matches(&self, _: u32, expected: &TrustedFile) -> io::Result<bool> {
        let mut path = vec![0; 32768];
        let mut len = path.len() as u32;
        // SAFETY: path is writable for len UTF-16 code units and the process handle stays open.
        if unsafe {
            QueryFullProcessImageNameW(self.0.as_raw_handle(), 0, path.as_mut_ptr(), &mut len)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let image = PathBuf::from(OsString::from_wide(&path[..len as usize])).canonicalize()?;
        if image != expected.path.canonicalize()? {
            return Ok(false);
        }
        let loaded_path = std::fs::File::open(image)?;
        Ok(file_id(&loaded_path)? == file_id(&expected.guard)?)
    }

    pub(super) fn parent_is(&self, pid: u32, root: u32, expected: &Self) -> io::Result<bool> {
        if self.created()? < expected.created()? {
            return Ok(false);
        }
        // SAFETY: a successful process snapshot is a new owned handle.
        let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        // SAFETY: the initialized entry advertises its valid size and is writable.
        let mut present = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) };
        while present != 0 {
            if entry.th32ProcessID == pid {
                return Ok(entry.th32ParentProcessID == root);
            }
            present = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) };
        }
        Ok(false)
    }

    pub(super) fn verified_output(
        &self,
        _: u32,
        root: u32,
        parent: &Self,
        proof: u64,
    ) -> io::Result<Output> {
        let raw = usize::try_from(proof).map_err(|_| io::ErrorKind::InvalidInput)? as HANDLE;
        let output = Output(duplicate(self.0.as_raw_handle(), raw)?);
        if !self.alive() || !output.matches(root, parent)? {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(output)
    }

    fn created(&self) -> io::Result<u64> {
        let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
        // SAFETY: all four arguments point to distinct writable FILETIME values.
        if unsafe {
            GetProcessTimes(
                self.0.as_raw_handle(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime))
    }
}

fn file_id(file: &std::fs::File) -> io::Result<(u32, u32, u32)> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the file is open and the output storage is sized for the API.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

pub(super) struct Output(OwnedHandle);

impl Output {
    pub(super) fn matches(&self, root: u32, parent: &Handle) -> io::Result<bool> {
        use windows_sys::Win32::System::Pipes::{
            GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
        };
        let pipe = self.0.as_raw_handle();
        let (mut server, mut client) = (0, 0);
        // SAFETY: the broker owns the duplicated pipe throughout these calls.
        Ok(parent.alive()
            && unsafe {
                GetFileType(pipe) == FILE_TYPE_PIPE
                    && GetNamedPipeServerProcessId(pipe, &mut server) != 0
                    && GetNamedPipeClientProcessId(pipe, &mut client) != 0
                    && server == root
                    && client == root
            }
            && parent.alive())
    }
}

fn duplicate(process: HANDLE, source: HANDLE) -> io::Result<OwnedHandle> {
    let mut handle = std::ptr::null_mut();
    // SAFETY: the source process is held and DuplicateHandle initializes a new owned handle.
    if unsafe {
        DuplicateHandle(
            process,
            source,
            GetCurrentProcess(),
            &mut handle,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

pub(in crate::local_auth) fn stdout_proof(file: &std::fs::File) -> io::Result<u64> {
    let handle = file.as_raw_handle();
    // SAFETY: file owns the handle through the request and write.
    if unsafe { GetFileType(handle) } != FILE_TYPE_PIPE {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(handle as usize as u64)
}
