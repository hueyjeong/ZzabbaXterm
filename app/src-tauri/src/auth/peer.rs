use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
};

use super::Error;

#[cfg(target_os = "linux")]
#[path = "peer_linux.rs"]
mod os;
#[cfg(windows)]
#[path = "peer_windows.rs"]
mod os;

#[cfg(target_os = "linux")]
pub(in crate::local_auth) use os::Handle as PeerHandle;
pub(super) use os::stdout_proof;
pub(super) struct Output(os::Output);

pub(super) fn installed_helper() -> Result<TrustedFile, Error> {
    let app = std::env::current_exe().map_err(|_| Error::UntrustedExecutable)?;
    let _app = TrustedFile::check(
        &app,
        if cfg!(windows) {
            "zzabbaxterm.exe"
        } else {
            "zzabbaxterm"
        },
    )?;
    TrustedFile::check(&app.with_file_name(super::HELPER), super::HELPER)
}
pub(super) const SSH: &str = if cfg!(windows) { "ssh.exe" } else { "ssh" };

pub(super) struct TrustedFile {
    path: PathBuf,
    guard: File,
}

impl TrustedFile {
    pub(super) fn check(path: &Path, name: &str) -> Result<Self, Error> {
        if !path.is_absolute()
            || path
                .file_name()
                .is_none_or(|value| !value.eq_ignore_ascii_case(name))
        {
            return Err(Error::UntrustedExecutable);
        }
        os::check_install(path).map_err(|_| Error::UntrustedExecutable)?;
        Self::open(path).map_err(|_| Error::UntrustedExecutable)
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    fn open(path: &Path) -> io::Result<Self> {
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // 프로세스가 실행되는 동안 파일 쓰기와 교체를 막는다.
            options.share_mode(1);
        }
        Ok(Self {
            path: path.to_owned(),
            guard: options.open(path)?,
        })
    }

    #[cfg(test)]
    pub(super) fn fixture(path: &Path) -> Self {
        Self::open(path).unwrap()
    }
}

pub(super) struct Process {
    pid: u32,
    handle: os::Handle,
}

impl Process {
    #[cfg(windows)]
    pub(super) fn open(pid: u32) -> io::Result<Self> {
        Ok(Self {
            pid,
            handle: os::Handle::open(pid)?,
        })
    }

    pub(super) fn from_child(child: &dyn portable_pty::Child) -> io::Result<Self> {
        let pid = child.process_id().ok_or(io::ErrorKind::NotFound)?;
        #[cfg(windows)]
        let handle = os::Handle::from_child(child, pid)?;
        #[cfg(target_os = "linux")]
        let handle = os::Handle::open(pid)?;
        Ok(Self { pid, handle })
    }

    pub(super) async fn exited(&self) {
        while self.handle.alive() {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn from_peer(pid: u32, handle: os::Handle) -> Self {
        Self { pid, handle }
    }

    pub(super) fn verified_output(&self, root: &Self, proof: u64) -> io::Result<Output> {
        self.handle
            .verified_output(self.pid, root.pid, &root.handle, proof)
            .map(Output)
    }

    pub(super) fn matches(&self, program: &TrustedFile) -> bool {
        self.handle.alive()
            && self.handle.matches(self.pid, program).unwrap_or(false)
            && self.handle.alive()
    }

    pub(super) fn is_child_of(
        &self,
        root: &Self,
        program: &TrustedFile,
        helper: &TrustedFile,
    ) -> bool {
        self.pid != root.pid
            && self.matches(helper)
            && self
                .handle
                .parent_is(self.pid, root.pid, &root.handle)
                .unwrap_or(false)
            && root.matches(program)
            && self.handle.alive()
    }
}

impl Output {
    pub(super) fn matches(&self, root: &Process) -> bool {
        self.0.matches(root.pid, &root.handle).unwrap_or(false)
    }
}
