use std::{
    fs, io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::fs::MetadataExt,
    },
    path::Path,
};

use super::TrustedFile;

pub(super) fn check_install(path: &Path) -> io::Result<()> {
    if path.canonicalize()? != path {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    for entry in path.ancestors() {
        let meta = entry.symlink_metadata()?;
        if meta.uid() != 0
            || meta.mode() & 0o022 != 0
            || (if entry == path {
                !meta.is_file() || meta.mode() & 0o111 == 0
            } else {
                !meta.is_dir()
            })
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
    }
    Ok(())
}

pub(in crate::local_auth) struct Handle(OwnedFd);

impl Handle {
    pub(super) fn open(pid: u32) -> io::Result<Self> {
        // SAFETY: pidfd_open takes a numeric PID and flags and returns an owned fd.
        Self::from_fd(unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as RawFd)
    }

    pub(in crate::local_auth) fn from_peer(socket: RawFd) -> io::Result<Self> {
        let mut fd: RawFd = -1;
        let mut len = size_of::<RawFd>() as libc::socklen_t;
        // Ubuntu 24.04의 Linux 6.8은 피어 PID 재사용 없이 소켓에서 직접 pidfd를 얻는다.
        const SO_PEERPIDFD: libc::c_int = 77;
        // SAFETY: the kernel writes one fd and its length into initialized storage.
        if unsafe {
            libc::getsockopt(
                socket,
                libc::SOL_SOCKET,
                SO_PEERPIDFD,
                (&mut fd as *mut RawFd).cast(),
                &mut len,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Self::from_fd(fd)
    }

    fn from_fd(fd: RawFd) -> io::Result<Self> {
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both kernel APIs return a new, exclusively owned fd on success.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub(super) fn alive(&self) -> bool {
        let mut fd = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: fd points to a single initialized pollfd for this call.
        unsafe { libc::poll(&mut fd, 1, 0) == 0 }
    }

    pub(super) fn matches(&self, pid: u32, expected: &TrustedFile) -> io::Result<bool> {
        let link = format!("/proc/{pid}/exe");
        if fs::read_link(&link)? != expected.path {
            return Ok(false);
        }
        let (actual, trusted) = (fs::metadata(link)?, expected.guard.metadata()?);
        Ok((actual.dev(), actual.ino()) == (trusted.dev(), trusted.ino()))
    }

    pub(super) fn parent_is(&self, pid: u32, root: u32, _: &Self) -> io::Result<bool> {
        let stat = fs::read(format!("/proc/{pid}/stat"))?;
        let end = stat
            .iter()
            .rposition(|&b| b == b')')
            .ok_or(io::ErrorKind::InvalidData)?;
        let parent = stat[end + 1..]
            .split(|b| b.is_ascii_whitespace())
            .filter(|s| !s.is_empty())
            .nth(1)
            .and_then(|s| std::str::from_utf8(s).ok())
            .and_then(|s| s.parse::<u32>().ok());
        Ok(parent == Some(root))
    }

    pub(super) fn verified_output(
        &self,
        pid: u32,
        root: u32,
        parent: &Self,
        proof: u64,
    ) -> io::Result<Output> {
        let fd = i32::try_from(proof)
            .ok()
            .filter(|fd| *fd > 2)
            .ok_or(io::ErrorKind::InvalidInput)?;
        let output = Output(open_descriptor(pid, fd)?);
        if !self.alive() || !output.matches(root, parent)? {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(output)
    }
}

pub(super) struct Output(fs::File);

impl Output {
    pub(super) fn matches(&self, root: u32, parent: &Handle) -> io::Result<bool> {
        if !parent.alive() {
            return Ok(false);
        }
        let meta = self.0.metadata()?;
        let identity = (meta.dev(), meta.ino());
        let process = format!("/proc/{root}");
        for entry in fs::read_dir(format!("{process}/fd"))? {
            let entry = entry?;
            let Some(fd) = entry
                .file_name()
                .to_str()
                .and_then(|fd| fd.parse::<i32>().ok())
            else {
                continue;
            };
            if fd <= 2 {
                continue;
            }
            let Ok(reader) = open_descriptor(root, fd) else {
                continue;
            };
            let meta = reader.metadata()?;
            if (meta.dev(), meta.ino()) != identity {
                continue;
            }
            let Ok(info) = fs::read_to_string(format!("{process}/fdinfo/{fd}")) else {
                continue;
            };
            let read_only = info
                .lines()
                .find_map(|line| {
                    line.strip_prefix("flags:")
                        .and_then(|v| i32::from_str_radix(v.trim(), 8).ok())
                })
                .is_some_and(|flags| flags & (libc::O_ACCMODE | libc::O_PATH) == libc::O_RDONLY);
            if read_only
                && open_descriptor(root, fd)
                    .and_then(|f| f.metadata())
                    .is_ok_and(|m| (m.dev(), m.ino()) == identity)
                && parent.alive()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn open_descriptor(pid: u32, fd: i32) -> io::Result<fs::File> {
    let path = format!("/proc/{pid}/fd/{fd}");
    if !fs::read_link(&path)?
        .as_os_str()
        .as_encoded_bytes()
        .starts_with(b"pipe:[")
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let name = std::ffi::CString::new(path).expect("numeric descriptor path");
    // O_PATH로는 읽기 끝을 추가하거나 파이프의 EOF를 지연하지 않는다.
    let fd = unsafe { libc::open(name.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { fs::File::from_raw_fd(fd) };
    let meta = file.metadata()?;
    if meta.mode() & libc::S_IFMT != libc::S_IFIFO {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(file)
}

pub(in crate::local_auth) fn stdout_proof(output: &fs::File) -> io::Result<u64> {
    let meta = output.metadata()?;
    let fd = output.as_raw_fd();
    // SAFETY: the descriptor is owned, and F_GETFL needs no third argument.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0
        || flags & libc::O_ACCMODE != libc::O_WRONLY
        || meta.mode() & libc::S_IFMT != libc::S_IFIFO
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(fd as u64)
}
