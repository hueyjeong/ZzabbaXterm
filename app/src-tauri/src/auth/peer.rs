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

pub(crate) fn installed_helper() -> Result<TrustedFile, Error> {
    let app = std::env::current_exe().map_err(|_| Error::UntrustedExecutable)?;
    let _app = TrustedFile::check(
        &app,
        if cfg!(windows) {
            "zxt-app.exe"
        } else {
            "zxt-app"
        },
    )?;
    TrustedFile::check(&app.with_file_name(super::HELPER), super::HELPER)
}
pub(super) const SSH: &str = if cfg!(windows) { "ssh.exe" } else { "ssh" };

pub(crate) struct TrustedFile {
    path: PathBuf,
    guard: File,
}

impl TrustedFile {
    pub(crate) fn check(path: &Path, name: &str) -> Result<Self, Error> {
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

    pub(crate) fn path(&self) -> &Path {
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
    pub(crate) fn fixture(path: &Path) -> Self {
        Self::open(path).unwrap()
    }
}

pub(crate) struct Process {
    pid: u32,
    handle: os::Handle,
}

impl Process {
    #[cfg(windows)]
    pub(crate) fn open(pid: u32) -> io::Result<Self> {
        Ok(Self {
            pid,
            handle: os::Handle::open(pid)?,
        })
    }

    pub(crate) fn from_child(child: &dyn portable_pty::Child) -> io::Result<Self> {
        let pid = child.process_id().ok_or(io::ErrorKind::NotFound)?;
        #[cfg(windows)]
        let handle = os::Handle::from_child(child, pid)?;
        #[cfg(target_os = "linux")]
        let handle = os::Handle::open(pid)?;
        Ok(Self { pid, handle })
    }

    pub(crate) async fn exited(&self) {
        while self.handle.alive() {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    pub(crate) fn alive(&self) -> bool {
        self.handle.alive()
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

    /// `sudo -A`의 askpass는 setuid `sudo`의 자식이지만 실행 신원은 호출 사용자다.
    /// 루트의 실행 파일·출력 파이프는 setuid 프로세스라 읽을 수 없으므로 자식 계보와 helper 신원만 확인한다.
    pub(crate) fn is_askpass_of(&self, root: &Self, helper: &TrustedFile) -> bool {
        self.pid != root.pid
            && self.matches(helper)
            && self
                .handle
                .parent_is(self.pid, root.pid, &root.handle)
                .unwrap_or(false)
            && root.handle.alive()
            && self.handle.alive()
    }
}

impl Output {
    pub(super) fn matches(&self, root: &Process) -> bool {
        self.0.matches(root.pid, &root.handle).unwrap_or(false)
    }
}
