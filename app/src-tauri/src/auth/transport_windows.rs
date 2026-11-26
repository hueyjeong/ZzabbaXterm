use std::{
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    io,
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    ptr::null_mut,
};

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    },
    Storage::FileSystem::{
        FILE_READ_DATA, FILE_WRITE_DATA, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    },
    System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId},
};

use super::{address_id, peer};

pub(in crate::local_auth) struct Listener {
    inner: Option<NamedPipeServer>,
    address: OsString,
}

impl Listener {
    pub(in crate::local_auth) fn bind() -> io::Result<Self> {
        let address = OsString::from(format!(r"\\.\pipe\zxt-askpass-{}", address_id()?));
        let sid = crate::ssm::windows_acl::user_sid_text()?;
        let sddl = format!("O:{sid}D:P(A;;FA;;;{sid})");
        let wide: Vec<_> = OsStr::new(&sddl).encode_wide().chain([0]).collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: wide is NUL-terminated and descriptor is writable output storage.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        // SAFETY: the attributes and descriptor stay valid until creation returns.
        let server = unsafe {
            ServerOptions::new()
                .first_pipe_instance(true)
                .max_instances(1)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    &address,
                    (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
                )
        };
        // SAFETY: the conversion routine allocated this descriptor with LocalAlloc.
        unsafe { LocalFree(descriptor) };
        Ok(Self {
            inner: Some(server?),
            address,
        })
    }

    pub(in crate::local_auth) fn address(&self) -> &OsStr {
        &self.address
    }

    pub(in crate::local_auth) async fn accept(&mut self) -> io::Result<Stream> {
        let server = self.inner.take().ok_or(io::ErrorKind::BrokenPipe)?;
        server.connect().await?;
        Ok(Stream(server))
    }

    pub(in crate::local_auth) fn reject(&mut self, stream: Stream) -> io::Result<()> {
        stream.0.disconnect()?;
        self.inner = Some(stream.0);
        Ok(())
    }
}

pub(in crate::local_auth) struct Stream(NamedPipeServer);

impl Stream {
    pub(in crate::local_auth) fn peer(&self) -> io::Result<peer::Process> {
        let mut pid = 0;
        // SAFETY: the server end stays open and pid points to writable storage.
        if unsafe { GetNamedPipeClientProcessId(self.0.as_raw_handle(), &mut pid) } == 0 || pid == 0
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        peer::Process::open(pid)
    }
}

impl std::ops::Deref for Stream {
    type Target = NamedPipeServer;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Stream {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

pub(super) fn connect(endpoint: &OsStr, broker: u32) -> io::Result<File> {
    let endpoint = endpoint.to_str().ok_or(io::ErrorKind::InvalidInput)?;
    let suffix = endpoint
        .strip_prefix(r"\\.\pipe\zxt-askpass-")
        .ok_or(io::ErrorKind::InvalidInput)?;
    if suffix.len() != 32 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    // FILE_APPEND_DATA와 같은 비트인 FILE_CREATE_PIPE_INSTANCE는 클라이언트 권한에서 제외한다.
    let pipe = OpenOptions::new()
        .access_mode(FILE_READ_DATA | FILE_WRITE_DATA)
        .custom_flags(SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION)
        .open(endpoint)?;
    let mut pid = 0;
    // SAFETY: pipe is live and pid points to writable storage.
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 || pid != broker
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(pipe)
}
