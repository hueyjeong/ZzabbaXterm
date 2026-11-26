use std::{
    ffi::c_void,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle},
    ptr::null_mut,
};

use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions};
use windows_sys::Win32::{
    Foundation::{ERROR_PIPE_BUSY, INVALID_HANDLE_VALUE, LocalFree},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
        },
        GetTokenInformation, OWNER_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_ELEVATION, TOKEN_USER, TokenElevation, TokenUser,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_READ_DATA, FILE_WRITE_DATA, OPEN_EXISTING,
        READ_CONTROL, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, SYNCHRONIZE,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

pub struct Endpoint {
    name: String,
    sid: String,
}

impl Endpoint {
    pub fn for_service(service: &str) -> io::Result<Self> {
        let service = super::service_name(service)?;
        let sid = current_user_sid()?;
        Ok(Self {
            name: format!(r"\\.\pipe\zxt-{sid}-{service}"),
            sid,
        })
    }

    pub async fn connect(&self) -> io::Result<NamedPipeClient> {
        let name = wide(&self.name);
        let handle = loop {
            // SAFETY: The path is NUL-terminated. Only the returned handle is retained.
            let raw = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    FILE_READ_DATA | FILE_WRITE_DATA | READ_CONTROL | SYNCHRONIZE,
                    0,
                    null_mut(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_IDENTIFICATION | SECURITY_SQOS_PRESENT,
                    null_mut(),
                )
            };
            if raw != INVALID_HANDLE_VALUE {
                // SAFETY: The valid handle transfers to its sole owner.
                break unsafe { OwnedHandle::from_raw_handle(raw) };
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) {
                return Err(error);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        let mut owner = null_mut();
        let mut descriptor = null_mut();
        // SAFETY: The open pipe handle and both output locations are valid.
        let status = unsafe {
            GetSecurityInfo(
                handle.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let _descriptor = LocalAllocation(descriptor);
        if sid_string(owner)? != self.sid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "IPC server owner does not match",
            ));
        }
        // SAFETY: This is an overlapped client pipe handle, transferred to Tokio.
        unsafe { NamedPipeClient::from_raw_handle(handle.into_raw_handle()) }
    }
}

pub struct Listener {
    next: NamedPipeServer,
    name: String,
    descriptor: LocalAllocation,
}

impl Listener {
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        let sddl = wide(&format!("O:{0}D:P(A;;GA;;;{0})", endpoint.sid));
        let mut descriptor = null_mut();
        // SAFETY: The input is a NUL-terminated SDDL string, and the output is writable.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let descriptor = LocalAllocation(descriptor);
        let next = create(&endpoint.name, &descriptor, true)?;
        Ok(Self {
            next,
            name: endpoint.name.clone(),
            descriptor,
        })
    }

    pub(crate) fn current_process_is_elevated() -> io::Result<bool> {
        let token = current_process_token()?;
        let mut elevation = TOKEN_ELEVATION::default();
        let size = std::mem::size_of::<TOKEN_ELEVATION>() as u32;
        let mut written = 0;
        // SAFETY: 토큰은 열려 있고 두 출력 버퍼의 크기와 정렬은 API 요구사항을 충족한다.
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                size,
                &mut written,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if written != size {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(elevation.TokenIsElevated != 0)
    }

    pub async fn accept(&mut self) -> io::Result<NamedPipeServer> {
        self.next.connect().await?;
        let next = create(&self.name, &self.descriptor, false)?;
        Ok(std::mem::replace(&mut self.next, next))
    }
}

fn create(name: &str, descriptor: &LocalAllocation, first: bool) -> io::Result<NamedPipeServer> {
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    // SAFETY: The attributes and owned descriptor remain valid for CreateNamedPipeW.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
    }
}

struct LocalAllocation(*mut c_void);

// SAFETY: The allocation is exclusively owned, immutable after construction, and LocalFree is thread-safe.
unsafe impl Send for LocalAllocation {}
unsafe impl Sync for LocalAllocation {}

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: The Win32 functions returned this owned LocalAlloc allocation.
        unsafe { LocalFree(self.0) };
    }
}

fn current_process_token() -> io::Result<OwnedHandle> {
    let mut token = null_mut();
    // SAFETY: The process pseudo-handle is valid and the output location is writable.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken returned an owned handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(token) })
}

fn current_user_sid() -> io::Result<String> {
    let token = current_process_token()?;
    let mut size = 0;
    // SAFETY: A zero-sized query supplies only a writable output length.
    unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut size) };
    if (size as usize) < std::mem::size_of::<TOKEN_USER>() {
        return Err(io::Error::last_os_error());
    }
    let mut data = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: This aligned buffer has at least `size` writable bytes.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            data.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: The successful TokenUser query initialized an aligned TOKEN_USER referencing this buffer.
    sid_string(unsafe { (*data.as_ptr().cast::<TOKEN_USER>()).User.Sid })
}

fn sid_string(sid: *mut c_void) -> io::Result<String> {
    if sid.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IPC user SID is missing",
        ));
    }
    let mut text = null_mut();
    // SAFETY: Callers supply a SID from a live Win32 token or security descriptor.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _allocation = LocalAllocation(text.cast());
    // SAFETY: Win32 returned a NUL-terminated wide string, held by `_allocation`.
    let length = (0..)
        .find(|&index| unsafe { *text.add(index) } == 0)
        .unwrap();
    let text = unsafe { std::slice::from_raw_parts(text, length) };
    String::from_utf16(text).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_token_reports_elevation() {
        Listener::current_process_is_elevated().unwrap();
    }

    #[tokio::test]
    async fn client_rejects_unexpected_pipe_owner() {
        let mut endpoint = Endpoint::for_service(&format!("owner-test-{}", std::process::id())).unwrap();
        let _listener = Listener::bind(&endpoint).unwrap();
        endpoint.sid = "S-1-0-0".into();
        assert_eq!(endpoint.connect().await.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }
}
