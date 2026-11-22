use std::{
    ffi::{CStr, CString, OsStr},
    fs::{self, File, Permissions},
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

use super::{storage_files::DownloadPublicationError, temporary_name};

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

    pub(super) fn open_cli(path: &Path) -> io::Result<Self> {
        Self::open(fs::canonicalize(path)?)
    }

    pub(super) fn still_at_cli_path(&self, path: &Path) -> io::Result<bool> {
        Ok(fs::canonicalize(path)? == self.path && self.still_at_path()?)
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

    pub(super) fn open_storage_file(&self, name: &OsStr) -> io::Result<Option<File>> {
        let name = c_name(name);
        // Only ENOENT from the no-follow lookup on the held parent means a new destination.
        let selected = match self.stat_id(&name) {
            Ok(Some(id)) => id,
            Ok(None) => return Err(io::ErrorKind::InvalidInput.into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let file = self.open_at(&name, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK)?;
        if !file.metadata()?.is_file() || file_id(&file)? != selected {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(Some(file))
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

    pub(super) fn stage(&self, _: Option<&Access>) -> io::Result<Temporary<'_>> {
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
        stat_id(&self.file, name)
    }
}

fn stat_id(directory: &File, name: &CStr) -> io::Result<Option<FileId>> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: a successful fstatat initializes stat; the descriptor and name are valid.
    let stat = unsafe {
        if libc::fstatat(
            directory.as_raw_fd(),
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

    pub(super) fn install(mut self, name: &OsStr, replace: bool) -> io::Result<()> {
        self.install_inner(name, replace)
    }

    pub(super) fn replace(mut self, name: &OsStr) -> io::Result<File> {
        self.install_inner(name, true)?;
        Ok(self.file.take().expect("temporary file is open"))
    }

    #[cfg(target_os = "linux")]
    pub(super) fn publish_download(
        mut self,
        name: &OsStr,
        current: &File,
        verify: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> Result<(), DownloadPublicationError> {
        self.check_private()?;
        let expected = file_id(current)?;
        let target = c_name(name);
        let recovery = c"original";
        let staging_fd = self.directory.as_raw_fd();
        // SAFETY: the held directory descriptors and single-component names are valid.
        if unsafe {
            libc::syscall(
                libc::SYS_renameat2 as libc::c_long,
                self.parent.file.as_raw_fd(),
                target.as_ptr(),
                staging_fd,
                recovery.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } == -1
        {
            let error = io::Error::last_os_error();
            return Err(match error.raw_os_error() {
                Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP) => {
                    io::Error::new(io::ErrorKind::Unsupported, error).into()
                }
                _ => error.into(),
            });
        }

        let result = (|| -> io::Result<()> {
            let changed = || io::Error::from(io::ErrorKind::AlreadyExists);
            if stat_id(&self.directory, recovery)? != Some(expected) {
                return Err(changed());
            }
            let mut moved = open_at(
                &self.directory,
                recovery,
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )?;
            if file_id(&moved)? != expected {
                return Err(changed());
            }
            let moved_fd = moved.as_raw_fd();
            // Linux UAPI F_SETSIG. Use a nonfatal notification instead of SIGIO.
            const F_SETSIG: libc::c_int = 10;
            // SAFETY: both commands take an integer argument on this owned descriptor.
            if unsafe { libc::fcntl(moved_fd, F_SETSIG, libc::SIGURG) } == -1
                || unsafe { libc::fcntl(moved_fd, libc::F_SETLEASE, libc::F_RDLCK) } == -1
            {
                return Err(io::Error::last_os_error());
            }
            let before = moved.metadata()?;
            verify(&mut moved)?;
            let check_original = |directory: &File| -> io::Result<()> {
                let after = moved.metadata()?;
                if (after.ctime(), after.ctime_nsec()) != (before.ctime(), before.ctime_nsec())
                    || (after.uid(), after.gid(), after.mode())
                        != (before.uid(), before.gid(), before.mode())
                    || stat_id(directory, recovery)? != Some(expected)
                    // SAFETY: F_GETLEASE takes no third argument; moved still holds the descriptor.
                    || unsafe { libc::fcntl(moved_fd, libc::F_GETLEASE) } != libc::F_RDLCK
                {
                    return Err(changed());
                }
                Ok(())
            };
            check_original(&self.directory)?;
            self.install_inner(name, false)?;
            let staged = self.file.as_ref().expect("temporary file is open");
            if self.parent.stat_id(&target)? != Some(file_id(staged)?)
                || !self.parent.still_at_path()?
            {
                return Err(changed());
            }
            check_original(&self.directory)?;
            // SAFETY: the held staging directory contains the verified recovery entry.
            if unsafe { libc::unlinkat(staging_fd, recovery.as_ptr(), 0) } == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })();
        result.map_err(|_| {
            // The parent may have moved since selection; prefer the held directory's current name.
            let directory = std::fs::read_link(format!("/proc/self/fd/{staging_fd}"))
                .unwrap_or_else(|_| self.parent.path.join(OsStr::from_bytes(self.name.to_bytes())));
            DownloadPublicationError::Unconfirmed(
                directory.join(OsStr::from_bytes(recovery.to_bytes())),
            )
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn publish_download(
        self,
        _: &OsStr,
        _: &File,
        _: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> Result<(), DownloadPublicationError> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "safe download replacement is unavailable on this platform",
        )
        .into())
    }

    fn install_inner(&mut self, name: &OsStr, replace: bool) -> io::Result<()> {
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
        if !replace {
            if unsafe { libc::linkat(self.directory.as_raw_fd(), c"body".as_ptr(), self.parent.file.as_raw_fd(), target.as_ptr(), 0) } == -1 {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
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
        Ok(())
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{fs, io::Write};

    #[test]
    fn download_keeps_both_files_when_the_destination_is_claimed_after_the_move() {
        let root = tempfile::tempdir().unwrap();
        let parent = Directory::open(root.path().to_owned()).unwrap();
        let name = OsStr::new("chosen");
        let path = root.path().join(name);
        fs::write(&path, b"original").unwrap();
        let current = parent.open_storage_file(name).unwrap().unwrap();
        let mut temp = parent.stage(None).unwrap();
        temp.file().write_all(b"download").unwrap();

        let result = temp.publish_download(name, &current, |_| {
            File::options()
                .write(true)
                .create_new(true)
                .open(&path)?
                .write_all(b"external")
        });
        let Err(DownloadPublicationError::Unconfirmed(recovery)) = result else {
            panic!("download publication did not return a recovery path: {result:?}");
        };
        assert_eq!(fs::read(&path).unwrap(), b"external");
        assert_eq!(fs::read(&recovery).unwrap(), b"original");
        assert_eq!(fs::read_dir(recovery.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn download_keeps_recovery_when_a_writer_is_open_or_breaks_the_lease() {
        for already_open in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let parent = Directory::open(root.path().to_owned()).unwrap();
            let name = OsStr::new("chosen");
            let path = root.path().join(name);
            let alias = root.path().join("alias");
            fs::write(&path, b"original").unwrap();
            fs::hard_link(&path, &alias).unwrap();
            let current = parent.open_storage_file(name).unwrap().unwrap();
            let mut temp = parent.stage(None).unwrap();
            temp.file().write_all(b"download").unwrap();
            let open_writer = || {
                File::options()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&alias)
            };
            let writer = already_open.then(|| open_writer().unwrap());
            let mut verified = false;
            let result = temp.publish_download(name, &current, |_| {
                verified = true;
                assert_eq!(open_writer().unwrap_err().kind(), io::ErrorKind::WouldBlock);
                Ok(())
            });
            let Err(DownloadPublicationError::Unconfirmed(recovery)) = result else {
                panic!("download publication did not return a recovery path: {result:?}");
            };
            assert_eq!(verified, !already_open);
            assert_eq!(fs::read(&recovery).unwrap(), b"original");
            assert_eq!(fs::read(&alias).unwrap(), b"original");
            assert_eq!(fs::read_dir(recovery.parent().unwrap()).unwrap().count(), 1);
            assert!(!path.exists());
            drop(writer);
        }
    }
}
