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

use super::publication::{PublicationError, temporary_name};

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
    pub(super) fn open_storage(path: PathBuf) -> io::Result<Self> {
        Self::open(path)
    }

    pub(super) fn open(path: PathBuf) -> io::Result<Self> {
        let file = File::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&path)?;
        Self::from_file(path, file)
    }

    pub(super) fn from_file(path: PathBuf, file: File) -> io::Result<Self> {
        let id = file_id(&file)?;
        Ok(Self { path, file, id })
    }

    #[cfg(target_os = "linux")]
    pub(super) fn handle(&self) -> &File {
        &self.file
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
        let created = directory_id(&self.file, &name)?.ok_or(io::ErrorKind::AlreadyExists)?;
        let directory = match self.open_at(&name, STAGING_DIRECTORY_FLAGS).and_then(|opened| {
            if file_id(&opened)? != created {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            Ok(opened)
        }) {
            Ok(directory) => directory,
            Err(error) => {
                remove_staging(&self.file, &name, created);
                return Err(error);
            }
        };
        let mut temp = Temporary {
            parent: self,
            directory,
            id: created,
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

    #[cfg(target_os = "linux")]
    pub(super) fn stage_probe(&self, access: &Access, verify: impl Fn(&File) -> io::Result<()>) -> io::Result<()> {
        let mut scope = self.stage(Some(access))?;
        let directory = Self::from_file(
            self.path.join(OsStr::from_bytes(scope.name.to_bytes())),
            open_at(&scope.directory, c".", libc::O_RDONLY | libc::O_DIRECTORY)?,
        )?;
        let source = OsStr::new("body");
        verify(scope.checked_file()?)?;
        access.finish_staging(scope.file())?;
        let original = open_regular_at(&directory.file, c"body", libc::O_RDONLY)?;
        if file_id(&original)? != file_id(scope.checked_file()?)? {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        let mut next = directory.stage(Some(access))?;
        verify(next.checked_file()?)?;
        access.finish_staging(next.file())?;
        scope.file = Some(original.try_clone()?);
        let result = next.publish_editor_guarded(source, &original, |original, staged| {
            verify(original)?;
            verify(staged)
        }, &verify, || true);
        match result {
            Ok(published) => {
                scope.file = Some(published);
                Ok(())
            }
            Err(PublicationError::Io(error)) => Err(error),
            Err(PublicationError::Unconfirmed(_)) => Err(io::Error::other("editor publication probe did not complete")),
        }
    }

    fn open_at(&self, name: &CStr, flags: libc::c_int) -> io::Result<File> {
        open_at(&self.file, name, flags)
    }

    fn stat_id(&self, name: &CStr) -> io::Result<Option<FileId>> {
        regular_id(&self.file, name)
    }
}

fn directory_id(directory: &File, name: &CStr) -> io::Result<Option<FileId>> {
    entry_id(directory, name, libc::S_IFDIR)
}

pub(super) fn regular_id(directory: &File, name: &CStr) -> io::Result<Option<FileId>> {
    entry_id(directory, name, libc::S_IFREG)
}

fn entry_id(directory: &File, name: &CStr, kind: libc::mode_t) -> io::Result<Option<FileId>> {
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
    Ok((stat.st_mode & libc::S_IFMT == kind)
        .then_some((stat.st_dev as u64, stat.st_ino as u64)))
}

fn remove_staging(parent: &File, name: &CStr, expected: FileId) {
    if directory_id(parent, name).ok().flatten() == Some(expected) {
        // SAFETY: the held parent and validated name identify the empty staging directory.
        unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
    }
}

// O_PATH permits cleanup even when an inherited default ACL removed owner access.
#[cfg(target_os = "linux")]
const STAGING_DIRECTORY_FLAGS: libc::c_int = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW;
#[cfg(not(target_os = "linux"))]
const STAGING_DIRECTORY_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;

pub(super) fn open_at(directory: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
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
pub(super) fn open_regular_at(directory: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    let node = open_at(directory, name, libc::O_PATH | libc::O_NOFOLLOW)?;
    if !node.metadata()?.is_file() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    let proc = proc_file(&node);
    let file = open_at(directory, &proc, flags | libc::O_NONBLOCK)?;
    let id = file_id(&file)?;
    if id != file_id(&node)? || regular_id(directory, name)? != Some(id) {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    Ok(file)
}

#[cfg(target_os = "linux")]
fn proc_file(file: &File) -> CString {
    CString::new(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap()
}

#[cfg(target_os = "linux")]
pub(super) fn clear_read_lease(file: &File) -> io::Result<()> {
    // SAFETY: F_SETLEASE takes an integer argument on the held descriptor.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_UNLCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn set_read_lease(file: &File) -> io::Result<()> {
    // Linux UAPI F_SETSIG. Use a nonfatal notification instead of SIGIO.
    const F_SETSIG: libc::c_int = 10;
    let fd = file.as_raw_fd();
    // SAFETY: both commands take an integer argument on this owned descriptor.
    if unsafe { libc::fcntl(fd, F_SETSIG, libc::SIGURG) } == -1
        || unsafe { libc::fcntl(fd, libc::F_SETLEASE, libc::F_RDLCK) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn read_lease_held(file: &File) -> bool {
    // SAFETY: F_GETLEASE takes no third argument and the descriptor is live.
    (unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLEASE) }) == libc::F_RDLCK
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
        Self::read_inner(file, false)
    }

    pub(super) fn read_editor(file: &File) -> io::Result<Self> {
        Self::read_inner(file, true)
    }

    fn read_inner(file: &File, editor: bool) -> io::Result<Self> {
        check_acl(file, false)?;
        let metadata = file.metadata()?;
        if editor {
            if metadata.nlink() != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "hard links cannot be preserved",
                ));
            }
            #[cfg(target_os = "linux")]
            // SAFETY: a null buffer with size zero queries the length of visible attribute names.
            match unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0) } {
                -1 => return Err(io::Error::last_os_error()),
                0 => (),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "extended attributes cannot be preserved",
                    ));
                }
            }
        }
        Ok(Self {
            uid: metadata.uid(),
            gid: metadata.gid(),
            mode: metadata.mode() & 0o7777,
        })
    }

    pub(super) fn matches(&self, file: &File) -> io::Result<bool> {
        Ok(*self == Self::read(file)?)
    }

    pub(super) fn matches_editor(&self, file: &File) -> io::Result<bool> {
        Ok(*self == Self::read_editor(file)?)
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
    id: FileId,
    name: CString,
    file: Option<File>,
}

impl Temporary<'_> {
    pub(super) fn file(&mut self) -> &mut File {
        self.file.as_mut().expect("temporary file is open")
    }

    fn check_named(&self) -> io::Result<()> {
        let held = self.file.as_ref().expect("temporary file is open");
        if directory_id(&self.parent.file, &self.name)? != Some(self.id)
            || regular_id(&self.directory, c"body")? != Some(file_id(held)?)
        {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "temporary file was replaced"));
        }
        self.check_private()
    }

    #[cfg(target_os = "linux")]
    pub(super) fn checked_file(&mut self) -> io::Result<&mut File> {
        self.check_named()?;
        Ok(self.file())
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

    pub(super) fn install(self, name: &OsStr, replace: bool) -> io::Result<()> {
        if replace {
            return Err(io::ErrorKind::Unsupported.into());
        }
        self.install_inner(&c_name(name))
    }

    #[cfg(target_os = "linux")]
    pub(super) fn publish_editor(
        self,
        name: &OsStr,
        expected: &File,
        verify: impl FnOnce(&File) -> io::Result<()>,
    ) -> Result<File, PublicationError> {
        self.publish_editor_inner(name, expected, verify, None::<BeforeMove>, |_| Ok(()), || true, |_| Ok(()))
    }

    #[cfg(target_os = "linux")]
    pub(super) fn publish_editor_guarded(
        self,
        name: &OsStr,
        expected: &File,
        before_move: impl FnOnce(&File, &File) -> io::Result<()>,
        preserve: impl Fn(&File) -> io::Result<()>,
        authorize_move: impl FnOnce() -> bool,
    ) -> Result<File, PublicationError> {
        self.publish_editor_inner(name, expected, |_| Ok(()), Some(before_move), preserve, authorize_move, |_| Ok(()))
    }

    #[cfg(target_os = "linux")]
    fn publish_editor_inner(
        mut self,
        name: &OsStr,
        expected: &File,
        verify: impl FnOnce(&File) -> io::Result<()>,
        before_move: Option<impl FnOnce(&File, &File) -> io::Result<()>>,
        preserve: impl Fn(&File) -> io::Result<()>,
        authorize_move: impl FnOnce() -> bool,
        after_move: impl FnOnce(&File) -> io::Result<()>,
    ) -> Result<File, PublicationError> {
        let expected = file_id(expected)?;
        let original = if before_move.is_some() {
            open_regular_at(&self.parent.file, &c_name(name), libc::O_RDONLY)?
        } else {
            self.parent.open_file(name)?.ok_or_else(|| io::Error::from(io::ErrorKind::AlreadyExists))?
        };
        if file_id(&original)? != expected {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists).into());
        }
        // Closing this independent read descriptor releases the lease on every exit.
        set_read_lease(&original)?;
        let access = Access::read_editor(&original)?;
        let staged = self.file.as_ref().expect("temporary file is open");
        if !access.matches_editor(staged)? {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists).into());
        }

        if before_move.is_some() {
            // Close the staged writer before taking a lease on its verified read descriptor.
            self.check_named()?;
            let named = open_regular_at(&self.directory, c"body", libc::O_RDONLY)?;
            if file_id(&named)? != file_id(self.file())? {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists).into());
            }
            self.file = Some(named);
            set_read_lease(self.file())?;
        }
        verify(&original)?;

        let parent = self.parent;
        let before_move_enabled = before_move.is_some();
        self.publish_existing(
            name,
            expected,
            Some(original),
            before_move,
            authorize_move,
            |original| {
                after_move(original)?;
                if !access.matches_editor(original)? {
                    return Err(io::ErrorKind::AlreadyExists.into());
                }
                preserve(original)
            },
            |original, staged| {
                let published = if before_move_enabled {
                    open_regular_at(&parent.file, &c_name(name), libc::O_RDONLY)?
                } else {
                    parent.open_file(name)?.ok_or(io::ErrorKind::NotFound)?
                };
                if file_id(&published)? != staged || !access.matches_editor(original)? {
                    return Err(io::ErrorKind::AlreadyExists.into());
                }
                if before_move_enabled && !access.matches(&published)? {
                    return Err(io::ErrorKind::AlreadyExists.into());
                }
                preserve(original)?;
                preserve(&published)?;
                Ok(published)
            },
        )
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn publish_editor(
        self,
        _: &OsStr,
        _: &File,
        _: impl FnOnce(&File) -> io::Result<()>,
    ) -> Result<File, PublicationError> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "safe editor replacement is unavailable on this platform",
        )
        .into())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn publish_download(
        self,
        name: &OsStr,
        current: &File,
        verify: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> Result<(), PublicationError> {
        self.publish_existing(name, file_id(current)?, None, None::<BeforeMove>, || true, verify, |_, _| Ok(()))
    }

    #[cfg(target_os = "linux")]
    fn publish_existing<T>(
        self,
        name: &OsStr,
        expected: FileId,
        leased: Option<File>,
        before_move: Option<impl FnOnce(&File, &File) -> io::Result<()>>,
        authorize_move: impl FnOnce() -> bool,
        verify: impl FnOnce(&mut File) -> io::Result<()>,
        retain: impl FnOnce(&File, FileId) -> io::Result<T>,
    ) -> Result<T, PublicationError> {
        let staged = self.file.as_ref().expect("temporary file is open");
        self.check_private()?;
        let guarded = before_move.is_some();
        let target = c_name(name);
        let recovery = c"original";
        let staging_fd = self.directory.as_raw_fd();
        if let Some(original) = leased.as_ref() {
            if self.parent.stat_id(&target)? != Some(expected)
                || !read_lease_held(original)
                || !self.parent.still_at_path()?
            {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists).into());
            }
            if let Some(before_move) = before_move {
                let stamp = metadata_stamp(original)?;
                self.check_named()?;
                let staged_stamp = metadata_stamp(staged)?;
                before_move(original, staged)?;
                self.check_named()?;
                if !read_lease_held(original) || !read_lease_held(staged)
                    || self.parent.stat_id(&target)? != Some(expected)
                    || metadata_stamp(original)? != stamp || metadata_stamp(staged)? != staged_stamp
                {
                    return Err(io::Error::from(io::ErrorKind::AlreadyExists).into());
                }
            }
        }
        if !authorize_move() {
            return Err(io::Error::from(io::ErrorKind::Interrupted).into());
        }
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

        let result = (|| -> io::Result<T> {
            let changed = || io::Error::from(io::ErrorKind::AlreadyExists);
            if regular_id(&self.directory, recovery)? != Some(expected) {
                return Err(changed());
            }
            let mut moved = if let Some(original) = leased {
                original
            } else {
                let moved = open_at(
                    &self.directory,
                    recovery,
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                )?;
                if file_id(&moved)? != expected {
                    return Err(changed());
                }
                set_read_lease(&moved)?;
                moved
            };
            let before = moved.metadata()?;
            verify(&mut moved)?;
            let check_original = |directory: &File| -> io::Result<()> {
                let after = moved.metadata()?;
                if (after.ctime(), after.ctime_nsec()) != (before.ctime(), before.ctime_nsec())
                    || (after.uid(), after.gid(), after.mode())
                        != (before.uid(), before.gid(), before.mode())
                    || regular_id(directory, recovery)? != Some(expected)
                    || !read_lease_held(&moved)
                {
                    return Err(changed());
                }
                Ok(())
            };
            check_original(&self.directory)?;
            if guarded {
                if !read_lease_held(staged) {
                    return Err(changed());
                }
                self.install_held(&target)?;
            } else {
                self.install_inner(&target)?;
            }
            let staged_id = file_id(staged)?;
            let staged_stamp = guarded.then(|| metadata_stamp(staged)).transpose()?;
            let published = retain(&moved, staged_id)?;
            if guarded && staged.metadata()?.nlink() != 2 {
                return Err(changed());
            }
            if self.parent.stat_id(&target)? != Some(staged_id) || !self.parent.still_at_path()? {
                return Err(changed());
            }
            check_original(&self.directory)?;
            if let Some(stamp) = staged_stamp {
                if !read_lease_held(staged) || metadata_stamp(staged)? != stamp {
                    return Err(changed());
                }
                self.check_named()?;
            }
            // SAFETY: the held staging directory contains the verified recovery entry.
            if unsafe { libc::unlinkat(staging_fd, recovery.as_ptr(), 0) } == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(published)
        })();
        result.map_err(|_| {
            // The parent may have moved since selection; prefer the held directory's current name.
            let directory = std::fs::read_link(format!("/proc/self/fd/{staging_fd}"))
                .unwrap_or_else(|_| self.parent.path.join(OsStr::from_bytes(self.name.to_bytes())));
            PublicationError::Unconfirmed(
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
    ) -> Result<(), PublicationError> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "safe download replacement is unavailable on this platform",
        )
        .into())
    }

    #[cfg(target_os = "linux")]
    fn install_held(&self, target: &CStr) -> io::Result<()> {
        self.check_named()?;
        let held = self.file.as_ref().expect("temporary file is open");
        let source = proc_file(held);
        // SAFETY: procfs resolves the live descriptor, not the staging name.
        if unsafe { libc::linkat(libc::AT_FDCWD, source.as_ptr(), self.parent.file.as_raw_fd(), target.as_ptr(), libc::AT_SYMLINK_FOLLOW) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn install_inner(&self, target: &CStr) -> io::Result<()> {
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
        // SAFETY: both names are single components relative to the held directories.
        if unsafe {
            libc::linkat(
                self.directory.as_raw_fd(),
                c"body".as_ptr(),
                self.parent.file.as_raw_fd(),
                target.as_ptr(),
                0,
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
        if let Some(expected) = self.file.as_ref().and_then(|file| file_id(file).ok()) {
            if regular_id(&self.directory, c"body").ok().flatten() == Some(expected) {
                // SAFETY: the private held directory contains the verified body entry.
                unsafe { libc::unlinkat(self.directory.as_raw_fd(), c"body".as_ptr(), 0) };
            }
        }
        remove_staging(&self.parent.file, &self.name, self.id);
    }
}

#[cfg(target_os = "linux")]
type BeforeMove = fn(&File, &File) -> io::Result<()>;

#[cfg(target_os = "linux")]
fn metadata_stamp(file: &File) -> io::Result<(i64, i64)> {
    let info = file.metadata()?;
    Ok((info.ctime(), info.ctime_nsec()))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{fs, io::Write};

    #[test]
    fn guarded_probe_leaves_no_artifact_and_uses_the_shared_publisher() {
        let root = tempfile::tempdir().unwrap();
        let parent = Directory::open(root.path().to_owned()).unwrap();
        let path = root.path().join("chosen");
        fs::write(&path, b"original").unwrap();
        let file = File::open(&path).unwrap();
        let id = file_id(&file).unwrap();
        let access = Access::read_editor(&file).unwrap();
        parent.stage_probe(&access, |_| Ok(())).unwrap();
        assert_eq!(file_id(&File::open(&path).unwrap()).unwrap(), id);
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn guarded_cancellation_leaves_the_original_and_cleans_the_stage() {
        let root = tempfile::tempdir().unwrap();
        let parent = Directory::open(root.path().to_owned()).unwrap();
        let name = OsStr::new("chosen");
        let path = root.path().join(name);
        fs::write(&path, b"original").unwrap();
        let original = File::open(&path).unwrap();
        let access = Access::read_editor(&original).unwrap();
        let mut temp = parent.stage(None).unwrap();
        temp.checked_file().unwrap().write_all(b"edited").unwrap();
        access.finish_staging(temp.file()).unwrap();

        let result = temp.publish_editor_guarded(name, &original, |current, staged| {
            assert_eq!(file_id(current).unwrap(), file_id(&original).unwrap());
            assert!(read_lease_held(current) && read_lease_held(staged));
            Ok(())
        }, |_| panic!("cancelled publication crossed the move boundary"), || false);
        assert!(matches!(result, Err(PublicationError::Io(error)) if error.kind() == io::ErrorKind::Interrupted));
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn guarded_publication_keeps_recovery_when_the_target_is_claimed() {
        let root = tempfile::tempdir().unwrap();
        let parent = Directory::open(root.path().to_owned()).unwrap();
        let name = OsStr::new("chosen");
        let path = root.path().join(name);
        fs::write(&path, b"original").unwrap();
        let original = File::open(&path).unwrap();
        let access = Access::read_editor(&original).unwrap();
        let mut temp = parent.stage(None).unwrap();
        temp.checked_file().unwrap().write_all(b"edited").unwrap();
        access.finish_staging(temp.file()).unwrap();

        let result = temp.publish_editor_inner(name, &original, |_| Ok(()), Some((|_, _| Ok(())) as BeforeMove), |_| Ok(()), || true, |_| {
            File::create_new(&path)?.write_all(b"external")
        });
        let Err(PublicationError::Unconfirmed(recovery)) = result else {
            panic!("expected recovery, got {result:?}");
        };
        assert_eq!(fs::read(&path).unwrap(), b"external");
        assert_eq!(fs::read(&recovery).unwrap(), b"original");
        assert_eq!(file_id(&File::open(&recovery).unwrap()).unwrap(), file_id(&original).unwrap());
        assert_eq!(fs::read_dir(recovery.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn guarded_publication_does_not_delete_a_different_inode_moved_after_authorization() {
        let root = tempfile::tempdir().unwrap();
        let parent = Directory::open(root.path().to_owned()).unwrap();
        let name = OsStr::new("chosen");
        let path = root.path().join(name);
        let held = root.path().join("held");
        fs::write(&path, b"original").unwrap();
        let original = File::open(&path).unwrap();
        let access = Access::read_editor(&original).unwrap();
        let mut temp = parent.stage(None).unwrap();
        temp.checked_file().unwrap().write_all(b"edited").unwrap();
        access.finish_staging(temp.file()).unwrap();

        let result = temp.publish_editor_guarded(name, &original, |_, _| Ok(()), |_| Ok(()), || {
            fs::rename(&path, &held).unwrap();
            fs::write(&path, b"external").unwrap();
            true
        });
        let Err(PublicationError::Unconfirmed(recovery)) = result else {
            panic!("expected recovery, got {result:?}");
        };
        assert!(!path.exists());
        assert_eq!(fs::read(&held).unwrap(), b"original");
        assert_eq!(fs::read(&recovery).unwrap(), b"external");
        assert_ne!(file_id(&File::open(recovery).unwrap()).unwrap(), file_id(&original).unwrap());
    }

    #[test]
    fn staging_drop_does_not_remove_a_replaced_body_or_directory() {
        let root = tempfile::tempdir().unwrap();
        let parent = Directory::open(root.path().to_owned()).unwrap();
        let mut temp = parent.stage(None).unwrap();
        let stage = root.path().join(OsStr::from_bytes(temp.name.to_bytes()));
        let moved = root.path().join("moved");
        fs::rename(&stage, &moved).unwrap();
        fs::create_dir(&stage).unwrap();
        fs::rename(moved.join("body"), moved.join("old-body")).unwrap();
        fs::write(moved.join("body"), b"external").unwrap();
        assert_eq!(temp.checked_file().unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        drop(temp);
        assert!(stage.is_dir());
        assert_eq!(fs::read(moved.join("body")).unwrap(), b"external");
    }

    #[test]
    fn editor_returns_a_read_only_handle_to_the_new_named_file() {
        let root = tempfile::tempdir().unwrap();
        let parent = Directory::open(root.path().to_owned()).unwrap();
        let name = OsStr::new("chosen");
        let path = root.path().join(name);
        fs::write(&path, b"original").unwrap();
        let original = File::open(&path).unwrap();
        let access = Access::read_editor(&original).unwrap();
        let mut temp = parent.stage(None).unwrap();
        temp.file().write_all(b"edited").unwrap();
        access.finish_staging(temp.file()).unwrap();

        let mut published = temp.publish_editor(name, &original, |_| Ok(())).unwrap();
        assert_eq!(
            file_id(&published).unwrap(),
            file_id(&File::open(&path).unwrap()).unwrap()
        );
        assert_eq!(
            published.write_all(b"outside").unwrap_err().raw_os_error(),
            Some(libc::EBADF)
        );
        assert_eq!(fs::read(&path).unwrap(), b"edited");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn editor_leaves_original_named_when_a_writer_is_open() {
        let root = tempfile::tempdir().unwrap();
        let parent = Directory::open(root.path().to_owned()).unwrap();
        let name = OsStr::new("chosen");
        let path = root.path().join(name);
        fs::write(&path, b"original").unwrap();
        let original = File::open(&path).unwrap();
        let mut writer = File::options().write(true).open(&path).unwrap();
        let mut temp = parent.stage(None).unwrap();
        temp.file().write_all(b"edited").unwrap();

        let result = temp.publish_editor(name, &original, |_| panic!("lease admitted a writer"));
        assert!(matches!(result, Err(PublicationError::Io(error))
            if error.kind() == io::ErrorKind::WouldBlock));
        writer.write_all(b"external").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"external");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn editor_rejects_a_hard_link_without_changing_download_access() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("chosen");
        let alias = root.path().join("alias");
        fs::write(&path, b"original").unwrap();
        let original = File::open(&path).unwrap();
        let access = Access::read_editor(&original).unwrap();
        fs::hard_link(&path, &alias).unwrap();

        assert!(matches!(access.matches_editor(&original), Err(error)
            if error.kind() == io::ErrorKind::Unsupported));
        assert!(access.matches(&original).unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(fs::read(&alias).unwrap(), b"original");
    }

    #[test]
    fn editor_preserves_a_name_claim_and_reports_recovery_in_the_moved_parent() {
        let root = tempfile::tempdir().unwrap();
        let selected = root.path().join("selected");
        let moved = root.path().join("moved");
        fs::create_dir(&selected).unwrap();
        let parent = Directory::open(selected.clone()).unwrap();
        let name = OsStr::new("chosen");
        let path = selected.join(name);
        fs::write(&path, b"original").unwrap();
        let original = File::open(&path).unwrap();
        let access = Access::read_editor(&original).unwrap();
        let mut temp = parent.stage(None).unwrap();
        temp.file().write_all(b"edited").unwrap();
        access.finish_staging(temp.file()).unwrap();

        let result = temp.publish_editor_inner(name, &original, |_| Ok(()), None::<BeforeMove>, |_| Ok(()), || true, |_| {
            fs::rename(&selected, &moved)?;
            File::options()
                .write(true)
                .create_new(true)
                .open(moved.join(name))?
                .write_all(b"external")
        });
        let Err(PublicationError::Unconfirmed(recovery)) = result else {
            panic!("editor publication did not return a recovery path: {result:?}");
        };
        assert!(recovery.starts_with(&moved), "{recovery:?}");
        assert_eq!(fs::read(&recovery).unwrap(), b"original");
        // A contender can reopen the recovered original once publication returns.
        File::options()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&recovery)
            .unwrap();
        assert_eq!(fs::read(moved.join(name)).unwrap(), b"external");
        assert_eq!(fs::read_dir(recovery.parent().unwrap()).unwrap().count(), 1);
    }

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
        let Err(PublicationError::Unconfirmed(recovery)) = result else {
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
            let Err(PublicationError::Unconfirmed(recovery)) = result else {
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
