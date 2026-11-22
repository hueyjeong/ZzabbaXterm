use portable_pty::Child;
use std::{io, mem::size_of, os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle}, ptr};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};

pub(super) struct Job(OwnedHandle);

impl Job {
    pub(super) fn new() -> io::Result<Self> {
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(job.0.as_raw_handle(), JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(), size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32)
        } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    pub(super) fn assign(&self, child: &dyn Child) -> io::Result<()> {
        let process = child.as_raw_handle().ok_or(io::ErrorKind::InvalidInput)?;
        if unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), process) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
