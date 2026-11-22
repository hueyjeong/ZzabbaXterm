use std::{
    ffi::{CStr, CString, OsStr},
    fs::{File, Permissions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Path, PathBuf},
};

use super::temporary_name;

pub(super) type FileId = (u64, u64);

pub(super) fn file_id(file: &File) -> io::Result<FileId> {
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

pub(super) fn valid_path(_: &Path) -> bool {
    true
}

pub(super) fn valid_name(name: &OsStr) -> bool {
    !name.as_bytes().contains(&0)
}

fn c_name(name: &OsStr) -> CString {
    CString::new(name.as_bytes()).expect("validated file name")
}

pub(super) struct Directory {
    pub(super) path: PathBuf,
    file: File,
    id: FileId,
}

impl Directory {
    pub(super) fn open(path: PathBuf) -> io::Result<Self> {
        let file = File::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&path)?;
        let id = file_id(&file)?;
        Ok(Self { path, file, id })
    }

    pub(super) fn still_at_path(&self) -> io::Result<bool> {
        Ok(Self::open(self.path.clone())?.id == self.id)
    }

    pub(super) fn open_file(&self, name: &OsStr) -> io::Result<Option<File>> {
        let name = c_name(name);
        if self.stat_id(&name)?.is_none() {
            return Ok(None);
        }
        let file = match self.open_at(&name, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK) {
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return Ok(None),
            other => other?,
        };
        Ok(file.metadata()?.is_file().then_some(file))
    }

    pub(super) fn check_replace_access(&self, name: &OsStr, file: &File) -> io::Result<()> {
        if file.metadata()?.permissions().readonly() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "original file is read-only",
            ));
        }
        let writable = self.open_at(
            &c_name(name),
            libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )?;
        if file_id(file)? != file_id(&writable)? {
            return Err(io::Error::other("original file changed before replacement"));
        }
        Ok(())
    }

    pub(super) fn entry_id(&self, name: &OsStr) -> io::Result<Option<FileId>> {
        self.stat_id(&c_name(name))
    }

    pub(super) fn stage(&self, _: &Access) -> io::Result<Temporary<'_>> {
        let name = c_name(&temporary_name()?);
        // SAFETY: the held parent and single component remain live through mkdirat.
        if unsafe { libc::mkdirat(self.file.as_raw_fd(), name.as_ptr(), 0o700) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let directory = self.open_at(&name, STAGING_DIRECTORY_FLAGS)?;
        let mut temp = Temporary {
            parent: self,
            directory,
            name,
            file: None,
        };
        temp.check_private()?;
        let acl_handle = open_at(&temp.directory, c".", libc::O_RDONLY | libc::O_DIRECTORY)?;
        check_acl(&acl_handle, true)?;
        temp.file = Some(open_at(
            &temp.directory,
            c"body",
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
        )?);
        Ok(temp)
    }

    fn open_at(&self, name: &CStr, flags: libc::c_int) -> io::Result<File> {
        open_at(&self.file, name, flags)
    }

    fn stat_id(&self, name: &CStr) -> io::Result<Option<FileId>> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: a successful fstatat initializes stat; the descriptor and name are valid.
        let stat = unsafe {
            if libc::fstatat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            ) == -1
            {
                return Err(io::Error::last_os_error());
            }
            stat.assume_init()
        };
        Ok((stat.st_mode & libc::S_IFMT == libc::S_IFREG)
            .then_some((stat.st_dev as u64, stat.st_ino as u64)))
    }
}

// O_PATH permits cleanup even when an inherited default ACL removed owner access.
#[cfg(target_os = "linux")]
const STAGING_DIRECTORY_FLAGS: libc::c_int = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW;
#[cfg(not(target_os = "linux"))]
const STAGING_DIRECTORY_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;

fn open_at(directory: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    // SAFETY: the directory descriptor and NUL-terminated name live through openat.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            0o600 as libc::c_int,
        )
    };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new, owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn check_acl(file: &File, remove_inherited: bool) -> io::Result<()> {
    for name in [c"system.posix_acl_access", c"system.nfs4_acl"] {
        // SAFETY: a null buffer with size zero queries only the attribute length.
        if unsafe { libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0) } >= 0
        {
            if !remove_inherited || name != c"system.posix_acl_access" {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "extended file ACL cannot be preserved",
                ));
            }
            // SAFETY: this handle names only the newly staged file.
            if unsafe { libc::fremovexattr(file.as_raw_fd(), name.as_ptr()) } == -1 {
                return Err(io::Error::last_os_error());
            }
        } else {
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ENODATA) => (),
                Some(libc::EOPNOTSUPP) if name == c"system.nfs4_acl" => (),
                _ => return Err(error),
            }
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn check_acl(_: &File, _: bool) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "file ACL verification is unavailable",
    ))
}

#[derive(PartialEq, Eq)]
pub(super) struct Access {
    uid: u32,
    gid: u32,
    mode: u32,
}

impl Access {
    pub(super) fn read(file: &File) -> io::Result<Self> {
        check_acl(file, false)?;
        let metadata = file.metadata()?;
        Ok(Self {
            uid: metadata.uid(),
            gid: metadata.gid(),
            mode: metadata.mode() & 0o7777,
        })
    }

    pub(super) fn matches(&self, file: &File) -> io::Result<bool> {
        Ok(*self == Self::read(file)?)
    }

    pub(super) fn finish_staging(&self, file: &File) -> io::Result<()> {
        check_acl(file, true)?;
        let staged = Self::read(file)?;
        if (self.uid, self.gid) != (staged.uid, staged.gid) {
            // SAFETY: file owns a valid descriptor for this temporary file.
            if unsafe { libc::fchown(file.as_raw_fd(), self.uid, self.gid) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        file.set_permissions(Permissions::from_mode(self.mode))?;
        if !self.matches(file)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "temporary file mode or owner differs",
            ));
        }
        Ok(())
    }
}

pub(super) struct Temporary<'a> {
    parent: &'a Directory,
    directory: File,
    name: CString,
    file: Option<File>,
}

impl Temporary<'_> {
    pub(super) fn file(&mut self) -> &mut File {
        self.file.as_mut().expect("temporary file is open")
    }

    fn check_private(&self) -> io::Result<()> {
        let metadata = self.directory.metadata()?;
        // SAFETY: geteuid has no pointer arguments.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o700 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "staging directory is not private and accessible",
            ));
        }
        Ok(())
    }

    pub(super) fn replace(mut self, name: &OsStr) -> io::Result<File> {
        self.check_private()?;
        let held = self.file.as_ref().expect("temporary file is open");
        let named = open_at(
            &self.directory,
            c"body",
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )?;
        if file_id(held)? != file_id(&named)? {
            return Err(io::Error::other("temporary file was replaced"));
        }
        let target = c_name(name);
        // SAFETY: both names are single components relative to the held directories.
        if unsafe {
            libc::renameat(
                self.directory.as_raw_fd(),
                c"body".as_ptr(),
                self.parent.file.as_raw_fd(),
                target.as_ptr(),
            )
        } == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(self.file.take().expect("temporary file is open"))
    }
}

impl Drop for Temporary<'_> {
    fn drop(&mut self) {
        if self.file.is_some() {
            // SAFETY: only the saving user can access the holding directory's body entry.
            unsafe { libc::unlinkat(self.directory.as_raw_fd(), c"body".as_ptr(), 0) };
        }
        if self
            .parent
            .open_at(&self.name, STAGING_DIRECTORY_FLAGS)
            .and_then(|named| Ok(file_id(&named)? == file_id(&self.directory)?))
            .unwrap_or(false)
        {
            // SAFETY: the open parent and validated name identify the empty staging directory.
            unsafe {
                libc::unlinkat(
                    self.parent.file.as_raw_fd(),
                    self.name.as_ptr(),
                    libc::AT_REMOVEDIR,
                )
            };
        }
    }
}
