use super::*;
use portable_pty::ChildKiller;
use std::process::{Child as Process, Command, Stdio};

#[derive(Debug)]
pub(crate) struct Isolated(pub(crate) Process);

impl Isolated {
    pub(crate) fn waiting() -> Self {
        #[cfg(unix)]
        let mut command = Command::new("/bin/cat");
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("cmd.exe");
            command.args(["/D", "/Q", "/C", "set /p line="]);
            command
        };
        Self(command.stdin(Stdio::piped()).stdout(Stdio::null()).spawn().unwrap())
    }
}

impl Drop for Isolated {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn ssh_child_deadline_leaves_owned_child_running() {
    let mut child = Isolated::waiting();
    let deadline = Instant::now() + Duration::from_millis(40);
    assert_eq!(wait_until(&mut child.0, deadline).unwrap_err(), "wait: 시간이 초과되었습니다");
    assert!(Instant::now() >= deadline);
    assert!(child.0.try_wait().unwrap().is_none());
}

#[test]
fn ssh_child_termination_reaps_only_the_owned_child() {
    let mut child = Isolated::waiting();
    let mut peer = Isolated::waiting();
    assert!(!terminate(&mut child.0).unwrap().success());
    assert!(child.0.try_wait().unwrap().is_some());
    assert!(peer.0.try_wait().unwrap().is_none());
}

#[test]
fn ssh_child_natural_exit_preserves_status() {
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 7"]);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("cmd.exe");
        command.args(["/D", "/C", "exit 259"]);
        command
    };
    let mut child = Isolated(command.spawn().unwrap());
    assert_eq!(wait_until(&mut child.0, Instant::now() + REAP_TIMEOUT).unwrap().exit_code(), if cfg!(windows) { 259 } else { 7 });
}

#[derive(Debug)]
pub(crate) struct Restricted {
    pub(crate) child: Isolated,
    #[cfg(windows)]
    pub(crate) handle: std::os::windows::io::OwnedHandle,
}

impl ChildKiller for Restricted {
    fn kill(&mut self) -> io::Result<()> { panic!("라이브러리의 kill을 호출했습니다") }
    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> { panic!("자식 소유권을 복제했습니다") }
}

impl Child for Restricted {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> { Child::try_wait(&mut self.child.0) }
    fn wait(&mut self) -> io::Result<ExitStatus> { panic!("무기한 대기를 호출했습니다") }
    fn process_id(&self) -> Option<u32> { None }
    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        use std::os::windows::io::AsRawHandle;
        Some(self.handle.as_raw_handle())
    }
}

#[test]
fn ssh_child_failed_kill_is_explicit_and_keeps_child_running() {
    let child = Isolated::waiting();
    let mut restricted = Restricted {
        #[cfg(windows)]
        handle: restricted_handle(&child.0, 0x00100000), // SYNCHRONIZE만 허용한다.
        child,
    };
    let error = terminate(&mut restricted).unwrap_err();
    assert!(error.starts_with("kill: "), "{error}");
    #[cfg(windows)]
    assert!(error.contains("(os error 5)"), "{error}");
    assert!(restricted.child.0.try_wait().unwrap().is_none());
    assert!(terminate(&mut restricted.child.0).is_ok());
}

#[cfg(windows)]
pub(crate) fn restricted_handle(child: &Process, access: u32) -> std::os::windows::io::OwnedHandle {
    use std::{os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle}, ptr};
    use windows_sys::Win32::{Foundation::DuplicateHandle, System::Threading::GetCurrentProcess};
    let mut handle = ptr::null_mut();
    let process = unsafe { GetCurrentProcess() };
    assert_ne!(unsafe { DuplicateHandle(process, AsRawHandle::as_raw_handle(child), process, &mut handle, access, 0, 0) }, 0);
    unsafe { OwnedHandle::from_raw_handle(handle) }
}

#[test]
fn ssh_child_failed_wait_is_explicit() {
    #[cfg(unix)]
    {
        let mut child = std::mem::ManuallyDrop::new(Isolated::waiting());
        // OS에서 먼저 회수해 표준 라이브러리의 캐시 없이 ECHILD를 관찰한다.
        let pid = child.0.id() as libc::pid_t;
        child.0.kill().unwrap();
        assert_eq!(unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) }, pid);
        let error = terminate(&mut child.0).unwrap_err();
        assert!(error.starts_with("wait: "), "{error}");
        assert!(error.contains(&io::Error::from_raw_os_error(libc::ECHILD).to_string()), "{error}");
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::PROCESS_TERMINATE;
        let child = Isolated::waiting();
        let mut restricted = Restricted { handle: restricted_handle(&child.0, PROCESS_TERMINATE), child };
        let error = terminate(&mut restricted).unwrap_err();
        assert!(error.starts_with("wait: "), "{error}");
        assert!(error.contains("(os error 5)"), "{error}");
        assert!(restricted.child.0.try_wait().unwrap().is_none());
    }
}
