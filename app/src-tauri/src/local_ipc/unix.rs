use std::{
    env, fs, io,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

use tokio::net::{UnixListener, UnixStream};

pub struct Endpoint {
    socket: PathBuf,
    uid: u32,
}

impl Endpoint {
    pub fn for_service(service: &str) -> io::Result<Self> {
        let uid = effective_uid();
        let directory = match env::var_os("XDG_RUNTIME_DIR") {
            Some(path) => {
                let path = PathBuf::from(path);
                if !path.is_absolute() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "IPC runtime path must be absolute",
                    ));
                }
                check_private_directory(&path, uid)?;
                path.join("zzabbaxterm")
            }
            None => Path::new("/tmp").join(format!("zzabbaxterm-{uid}")),
        };
        Self::in_directory(&directory, service)
    }

    fn in_directory(directory: &Path, service: &str) -> io::Result<Self> {
        let service = super::service_name(service)?;
        if !directory.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "IPC runtime path must be absolute",
            ));
        }
        let uid = effective_uid();
        Ok(Self {
            socket: directory.join(format!("{service}.sock")),
            uid,
        })
    }

    pub async fn connect(&self) -> io::Result<UnixStream> {
        self.check_directory()?;
        self.check_socket()?;
        let stream = UnixStream::connect(&self.socket).await?;
        self.check_peer(&stream)?;
        Ok(stream)
    }

    fn directory(&self) -> &Path {
        self.socket.parent().expect("IPC socket has a parent")
    }

    fn check_directory(&self) -> io::Result<()> {
        check_private_directory(self.directory(), self.uid)
    }

    fn check_socket(&self) -> io::Result<fs::Metadata> {
        let info = fs::symlink_metadata(&self.socket)?;
        if !info.file_type().is_socket() || info.uid() != self.uid || info.mode() & 0o7777 != 0o600
        {
            return Err(permission_denied());
        }
        Ok(info)
    }

    fn check_peer(&self, stream: &UnixStream) -> io::Result<()> {
        if stream.peer_cred()?.uid() != self.uid {
            return Err(permission_denied());
        }
        Ok(())
    }
}

pub struct Listener {
    listener: UnixListener,
    endpoint: Endpoint,
    identity: (u64, u64),
    _lock: fs::File,
}

fn check_private_directory(path: &Path, uid: u32) -> io::Result<()> {
    let info = fs::symlink_metadata(path)?;
    if !info.is_dir() || info.uid() != uid || info.mode() & 0o7777 != 0o700 {
        return Err(permission_denied());
    }
    Ok(())
}

impl Listener {
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        match fs::DirBuilder::new()
            .mode(0o700)
            .create(endpoint.directory())
        {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        endpoint.check_directory()?;
        let lock = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(endpoint.socket.with_extension("lock"))?;
        let info = lock.metadata()?;
        if !info.is_file() || info.uid() != endpoint.uid || info.mode() & 0o7777 != 0o600 {
            return Err(permission_denied());
        }
        // SAFETY: The file lives at least as long as the lock and its descriptor is valid.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        match endpoint.check_socket() {
            Ok(_) => fs::remove_file(&endpoint.socket)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&endpoint.socket)?;
        let secure = fs::set_permissions(&endpoint.socket, fs::Permissions::from_mode(0o600))
            .and_then(|()| endpoint.check_socket());
        let info = secure.inspect_err(|_| {
            let _ = fs::remove_file(&endpoint.socket);
        })?;
        Ok(Self {
            listener,
            endpoint: Endpoint {
                socket: endpoint.socket.clone(),
                uid: endpoint.uid,
            },
            identity: (info.dev(), info.ino()),
            _lock: lock,
        })
    }

    pub async fn accept(&mut self) -> io::Result<UnixStream> {
        let (stream, _) = self.listener.accept().await?;
        self.endpoint.check_peer(&stream)?;
        Ok(stream)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if self
            .endpoint
            .check_socket()
            .is_ok_and(|info| (info.dev(), info.ino()) == self.identity)
        {
            let _ = fs::remove_file(&self.endpoint.socket);
        }
    }
}

fn permission_denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "IPC endpoint user or permissions do not match",
    )
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no arguments or preconditions.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_requires_private_directory_and_socket() {
        let directory = env::temp_dir().join(format!("zzabba-unix-test-{}", std::process::id()));
        let endpoint = Endpoint::in_directory(&directory, "ai").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut listener = Listener::bind(&endpoint).unwrap();
            assert_eq!(endpoint.check_socket().unwrap().mode() & 0o7777, 0o600);
            assert!(Listener::bind(&endpoint).is_err());
            let _client = endpoint.connect().await.unwrap();
            assert!(listener.accept().await.is_ok());
            fs::set_permissions(&endpoint.socket, fs::Permissions::from_mode(0o660)).unwrap();
            assert_eq!(
                endpoint.connect().await.unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
            fs::set_permissions(&endpoint.socket, fs::Permissions::from_mode(0o600)).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o750)).unwrap();
            assert_eq!(
                endpoint.connect().await.unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
        });
        fs::remove_dir_all(directory).unwrap();
    }
}
