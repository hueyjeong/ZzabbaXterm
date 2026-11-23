use std::{
    io, mem::size_of, net::SocketAddr,
    os::windows::{io::{AsRawHandle, FromRawHandle, OwnedHandle}, process::CommandExt},
    process::{Child, Command},
    ptr,
    time::{Duration, Instant},
};

use windows_sys::Win32::System::{
    JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    },
    Threading::CREATE_NO_WINDOW,
};

const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// aws.exe가 띄우는 session-manager-plugin.exe까지 같은 작업 개체에 묶는다.
/// 작업 개체 핸들을 닫아도 남은 프로세스는 `KILL_ON_JOB_CLOSE`로 종료된다.
pub(super) struct Tree(OwnedHandle);

pub(super) fn configure(command: &mut Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

#[repr(C)]
struct ProcessIds {
    _assigned: u32,
    listed: u32,
    ids: [usize; 64],
}

impl Tree {
    pub(super) fn new() -> io::Result<Self> {
        // SAFETY: 이름 없는 작업 개체를 만들고 성공한 핸들만 소유한다.
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: 위에서 받은 유효한 핸들을 한 번만 소유로 옮긴다.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: 구조체 포인터와 크기가 정보 클래스와 일치한다.
        if unsafe {
            SetInformationJobObject(job.0.as_raw_handle(), JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(), size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32)
        } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// 생성 직후 호출한다. aws.exe는 Python 초기화와 StartSession 응답 뒤에야 플러그인을 띄운다.
    pub(super) fn adopt(&mut self, child: &Child) -> io::Result<()> {
        // SAFETY: 두 핸들 모두 이 호출 동안 유효하다.
        if unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), child.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn exited(&self, child: &mut Child) -> io::Result<Option<String>> {
        Ok(child.try_wait()?.map(|status| status.to_string()))
    }

    /// 작업 개체 안의 프로세스만 종료하고 CLI 자식의 회수와 작업 개체가 빈 것을 확인한다.
    pub(super) fn stop(&mut self, child: &mut Child) -> io::Result<()> {
        // SAFETY: 소유한 작업 개체 핸들이다. 빈 작업 개체에도 성공한다.
        if unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let deadline = Instant::now() + STOP_TIMEOUT;
        while child.try_wait()?.is_none() {
            if Instant::now() >= deadline { return Err(io::ErrorKind::TimedOut.into()); }
            std::thread::sleep(Duration::from_millis(10));
        }
        while self.active()? != 0 {
            if Instant::now() >= deadline { return Err(io::ErrorKind::TimedOut.into()); }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    fn active(&self) -> io::Result<u32> {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: 출력 버퍼는 정보 클래스의 구조체와 크기가 같다.
        if unsafe {
            QueryInformationJobObject(self.0.as_raw_handle(), JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32, ptr::null_mut())
        } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.ActiveProcesses)
    }

    fn pids(&self) -> io::Result<Vec<u32>> {
        let mut list = ProcessIds { _assigned: 0, listed: 0, ids: [0; 64] };
        // SAFETY: 헤더 뒤에 64개 ID 칸을 둔 버퍼 크기를 그대로 넘긴다.
        let ok = unsafe {
            QueryInformationJobObject(self.0.as_raw_handle(), JobObjectBasicProcessIdList,
                (&mut list as *mut ProcessIds).cast(), size_of::<ProcessIds>() as u32, ptr::null_mut())
        };
        if ok == 0 && list.listed == 0 {
            return Err(io::Error::last_os_error());
        }
        let listed = (list.listed as usize).min(list.ids.len());
        Ok(list.ids[..listed].iter().filter_map(|id| u32::try_from(*id).ok()).collect())
    }

    pub(super) fn owns_stream(&self, _: &Child, listen: SocketAddr, peer: SocketAddr) -> io::Result<bool> {
        crate::tunnels::ssh::windows::owns_stream(&self.pids()?, listen, peer)
    }
}
