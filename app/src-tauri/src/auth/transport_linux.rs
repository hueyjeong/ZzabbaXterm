use std::{
    ffi::{OsStr, OsString},
    io,
    os::{
        fd::AsRawFd,
        unix::{fs::PermissionsExt, net::UnixStream as BlockingStream},
    },
    path::Path,
};

use tokio::net::{UnixListener, UnixStream};

use super::{HELPER_TIMEOUT, address_id, peer};

pub(in crate::local_auth) struct Listener {
    inner: UnixListener,
    address: OsString,
    _directory: tempfile::TempDir,
}

impl Listener {
    pub(in crate::local_auth) fn bind() -> io::Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix(&format!("zzabba-{}-", address_id()?))
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in("/tmp")?;
        let path = directory.path().join("askpass");
        Ok(Self {
            inner: UnixListener::bind(&path)?,
            address: path.into_os_string(),
            _directory: directory,
        })
    }

    pub(in crate::local_auth) fn address(&self) -> &OsStr {
        &self.address
    }

    pub(in crate::local_auth) async fn accept(&mut self) -> io::Result<Stream> {
        Ok(Stream(self.inner.accept().await?.0))
    }

    pub(in crate::local_auth) fn reject(&mut self, _: Stream) -> io::Result<()> {
        Ok(())
    }
}

pub(in crate::local_auth) struct Stream(UnixStream);

impl Stream {
    pub(in crate::local_auth) fn peer(&self) -> io::Result<peer::Process> {
        let credentials = self.0.peer_cred()?;
        // SAFETY: geteuid has no arguments and no mutable state.
        if credentials.uid() != unsafe { libc::geteuid() } {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let pid = credentials
            .pid()
            .and_then(|pid| u32::try_from(pid).ok())
            .ok_or(io::ErrorKind::PermissionDenied)?;
        let handle = peer::PeerHandle::from_peer(self.0.as_raw_fd())?;
        Ok(peer::Process::from_peer(pid, handle))
    }
}

impl std::ops::Deref for Stream {
    type Target = UnixStream;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Stream {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

pub(super) fn connect(endpoint: &OsStr, broker: u32) -> io::Result<BlockingStream> {
    let socket = BlockingStream::connect(Path::new(endpoint))?;
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the length matches the writable ucred storage.
    let status = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if status != 0 || len as usize != size_of::<libc::ucred>() {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    // SAFETY: SO_PEERCRED filled the whole structure on success.
    let credentials = unsafe { credentials.assume_init() };
    if u32::try_from(credentials.pid).ok() != Some(broker)
        || credentials.uid != unsafe { libc::geteuid() }
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    socket.set_read_timeout(Some(HELPER_TIMEOUT))?;
    socket.set_write_timeout(Some(HELPER_TIMEOUT))?;
    Ok(socket)
}
