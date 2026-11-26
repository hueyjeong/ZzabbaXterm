use std::{
    ffi::{CStr, CString, OsStr},
    fs::{File, Metadata},
    io::{self, BufRead, BufReader, Read, Seek, Write},
    mem::MaybeUninit,
    os::{fd::AsRawFd, unix::{ffi::OsStrExt, fs::{MetadataExt, OpenOptionsExt}}},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use super::{
    nano_wire::{MAX_CONTENT, Observation, Snapshot},
    os::{self, Access, Directory},
    publication::{PublicationError, verify_bytes},
};

const MAX_PATH: usize = 4096;
const DIRECTORY_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
// 조상 경로는 검색 권한만 요구한다(예: 0711 홈 상위). 읽기 열기는 최종 부모에만 한다.
const ANCESTOR_FLAGS: libc::c_int = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW;

#[derive(Debug)]
pub(super) enum Error {
    Conflict,
    Cancelled,
    Io(io::Error),
    Unknown(Option<PathBuf>),
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            // 이름이 이미 생긴 경우만 충돌이다. 권한·미지원·사라진 경로는
            // 다시 불러오기로 풀리지 않으므로 충돌 대화상자로 반복시키지 않는다.
            io::ErrorKind::AlreadyExists => Self::Conflict,
            _ => Self::Io(error),
        }
    }
}

pub(super) struct GuardedFile {
    parent: Directory,
    name: CString,
    uid: u32,
    mount: u64,
    original: File,
    access: Access,
    snapshot: Snapshot,
    unknown: Option<Option<PathBuf>>,
}

impl GuardedFile {
    pub(super) fn prepare(path: &Path) -> io::Result<(Self, Snapshot)> {
        let (parent_path, name) = split_path(path)?;
        // SAFETY: geteuid has no arguments and the helper runs as the login account.
        let uid = unsafe { libc::geteuid() };
        let parent = Directory::from_file(parent_path.into(), open_parent(parent_path, uid)?)?;
        let mount = ext4_mount(parent.handle())?;
        check_parent(&parent, uid, mount)?;
        let original = os::open_regular_at(parent.handle(), &name, libc::O_RDONLY)?;
        check_original(&original, uid, mount)?;
        let access = Access::read_editor(&original)?;
        check_write(&parent, &name, &original)?;
        parent.stage_probe(&access, |file| check_inode(file, mount, false))?;
        os::set_read_lease(&original)?;
        let stamp = file_stamp(&original.metadata()?);
        let observation = observe(&original)?;
        check_parent(&parent, uid, mount)?;
        require_named(&parent, &name, &original)?;
        check_original(&original, uid, mount)?;
        if !access.matches_editor(&original)? || !os::read_lease_held(&original)
            || file_stamp(&original.metadata()?) != stamp
        {
            return Err(changed());
        }
        let snapshot = new_snapshot(observation)?;
        // The held inode prevents ID reuse, but a UI session must not keep a write-blocking lease.
        os::clear_read_lease(&original)?;
        Ok((Self { parent, name, uid, mount, original, access, snapshot, unknown: None }, snapshot))
    }

    pub(super) fn compare(&self) -> Result<Observation, Error> {
        self.current().map(|(_, observation)| observation)
    }

    pub(super) fn reload(&mut self, expected: Observation) -> Result<Snapshot, Error> {
        let (file, observation) = self.current()?;
        if observation != expected {
            return Err(Error::Conflict);
        }
        let snapshot = new_snapshot(observation)?;
        self.original = file;
        self.snapshot = snapshot;
        Ok(snapshot)
    }

    pub(super) fn save(
        &mut self,
        token: [u8; 16],
        bytes: &[u8],
        cancelled: impl FnOnce() -> bool,
    ) -> Result<Snapshot, Error> {
        self.confirmed()?;
        if token != self.snapshot.token {
            return Err(Error::Conflict);
        }
        if bytes.len() > MAX_CONTENT {
            return Err(io::Error::from(io::ErrorKind::FileTooLarge).into());
        }
        std::str::from_utf8(bytes).map_err(invalid_utf8)?;
        let (original, observation) = self.current()?;
        if observation != self.snapshot.observation {
            return Err(Error::Conflict);
        }
        let name = OsStr::from_bytes(self.name.to_bytes());
        check_write(&self.parent, &self.name, &original)?;
        let snapshot = new_snapshot(Observation { len: bytes.len() as u64, digest: Sha256::digest(bytes).into() })?;
        let mut temp = self.parent.stage(Some(&self.access))?;
        let staged = temp.checked_file()?;
        check_original(staged, self.uid, self.mount)?;
        if staged.metadata()?.mode() & 0o7777 != 0o600 {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied).into());
        }
        staged.write_all(bytes)?;
        self.access.finish_staging(staged)?;
        staged.sync_all()?;
        let result = temp.publish_editor_guarded(name, &original, |original, staged| {
            if self.inspect(original)? != observation {
                return Err(changed());
            }
            check_original(staged, self.uid, self.mount)?;
            if !self.access.matches_editor(staged)? {
                return Err(changed());
            }
            verify_bytes(staged, bytes)?;
            check_parent(&self.parent, self.uid, self.mount)
        }, |file| {
            check_inode(file, self.mount, false)?;
            check_parent(&self.parent, self.uid, self.mount)
        }, || !cancelled());
        match result {
            Ok(published) => {
                self.original = published;
                self.snapshot = snapshot;
                Ok(snapshot)
            }
            Err(PublicationError::Io(error)) if error.kind() == io::ErrorKind::Interrupted => Err(Error::Cancelled),
            Err(PublicationError::Io(error)) => Err(error.into()),
            Err(PublicationError::Unconfirmed(path)) => {
                let recovery = self.check_recovery(&path).is_some().then_some(path);
                self.unknown = Some(recovery.clone());
                Err(Error::Unknown(recovery))
            }
        }
    }

    pub(super) fn recovery_name(&self, path: &Path) -> Option<String> {
        Some(format!("{}/original", self.check_recovery(path)?))
    }

    // 결과를 확정하지 못한 파일은 비교·재로드가 성립하지 않는다.
    pub(super) fn unresolved(&self) -> bool {
        self.unknown.is_some()
    }

    fn check_recovery<'a>(&self, path: &'a Path) -> Option<&'a str> {
        let dir = path.parent()?;
        let name = dir.file_name()?.to_str()?;
        let suffix = name.strip_prefix(".zxt-edit-")?.strip_suffix(".tmp")?;
        if path.file_name()? != "original" || dir.parent()? != self.parent.path.as_path()
            || suffix.len() != 32 || !suffix.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        // Only disclose a path still naming the selected original under the approved parent.
        let parent = open_parent(&self.parent.path, self.uid).ok()?;
        if os::file_id(&parent).ok()? != os::file_id(self.parent.handle()).ok()? || mount_id(&parent).ok()? != self.mount {
            return None;
        }
        let staging = os::open_at(&parent, &CString::new(name).ok()?, DIRECTORY_FLAGS).ok()?;
        let info = staging.metadata().ok()?;
        if info.uid() != self.uid || info.mode() & 0o777 != 0o700 || mount_id(&staging).ok()? != self.mount
            || os::regular_id(&staging, c"original").ok()? != Some(os::file_id(&self.original).ok()?)
        {
            return None;
        }
        Some(name)
    }

    fn confirmed(&self) -> Result<(), Error> {
        match &self.unknown {
            Some(recovery) => Err(Error::Unknown(recovery.clone())),
            None => Ok(()),
        }
    }

    fn inspect(&self, file: &File) -> io::Result<Observation> {
        require_named(&self.parent, &self.name, file)?;
        if os::file_id(file)? != os::file_id(&self.original)? {
            return Err(changed());
        }
        check_original(file, self.uid, self.mount)?;
        if !self.access.matches_editor(file)? {
            return Err(changed());
        }
        observe(file)
    }

    fn current(&self) -> Result<(File, Observation), Error> {
        self.confirmed()?;
        check_parent(&self.parent, self.uid, self.mount)?;
        let file = os::open_regular_at(self.parent.handle(), &self.name, libc::O_RDONLY)?;
        os::set_read_lease(&file)?;
        let stamp = file_stamp(&file.metadata()?);
        let observation = self.inspect(&file)?;
        require_named(&self.parent, &self.name, &file)?;
        check_parent(&self.parent, self.uid, self.mount)?;
        if !os::read_lease_held(&file) || file_stamp(&file.metadata()?) != stamp {
            return Err(Error::Conflict);
        }
        os::clear_read_lease(&file)?;
        Ok((file, observation))
    }
}

fn changed() -> io::Error { io::ErrorKind::AlreadyExists.into() }

fn unsupported() -> io::Error { io::ErrorKind::Unsupported.into() }

fn split_path(path: &Path) -> io::Result<(&Path, CString)> {
    let bytes = path.as_os_str().as_bytes();
    let invalid = || io::Error::from(io::ErrorKind::InvalidInput);
    if bytes.len() > MAX_PATH || bytes.first() != Some(&b'/') || bytes.contains(&0)
        || bytes[1..].split(|byte| *byte == b'/').any(|part| matches!(part, b"" | b"." | b".."))
    {
        return Err(invalid());
    }
    let parent = path.parent().ok_or_else(invalid)?;
    let name = CString::new(path.file_name().ok_or_else(invalid)?.as_bytes()).map_err(|_| invalid())?;
    Ok((parent, name))
}

fn trusted_dir(file: &File, uid: u32) -> io::Result<()> {
    let info = file.metadata()?;
    if !info.is_dir() || (info.uid() != 0 && info.uid() != uid) || info.mode() & 0o022 != 0 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}

fn open_parent(path: &Path, uid: u32) -> io::Result<File> {
    let flags = |last: bool| if last { DIRECTORY_FLAGS } else { ANCESTOR_FLAGS };
    let mut components = path.iter().skip(1).peekable();
    let mut directory = File::options().read(true).custom_flags(flags(components.peek().is_none())).open("/")?;
    trusted_dir(&directory, uid)?;
    while let Some(component) = components.next() {
        let name = CString::new(component.as_bytes()).map_err(|_| changed())?;
        directory = os::open_at(&directory, &name, flags(components.peek().is_none()))?;
        trusted_dir(&directory, uid)?;
    }
    Ok(directory)
}

fn require_named(parent: &Directory, name: &CStr, file: &File) -> io::Result<()> {
    let name = OsStr::from_bytes(name.to_bytes());
    if parent.entry_id(name)? != Some(os::file_id(file)?) {
        return Err(changed());
    }
    Ok(())
}

fn check_write(parent: &Directory, name: &CStr, original: &File) -> io::Result<()> {
    let writable = os::open_regular_at(parent.handle(), name, libc::O_WRONLY)?;
    if os::file_id(&writable)? != os::file_id(original)? {
        return Err(changed());
    }
    Ok(())
}

fn check_parent(parent: &Directory, uid: u32, mount: u64) -> io::Result<()> {
    let named = open_parent(&parent.path, uid)?;
    let held = parent.handle();
    if os::file_id(&named)? != os::file_id(held)? || mount_id(&named)? != mount {
        return Err(changed());
    }
    trusted_dir(held, uid)?;
    // SAFETY: a null buffer queries only the attribute length on the held parent.
    if unsafe { libc::fgetxattr(held.as_raw_fd(), c"system.posix_acl_default".as_ptr(), std::ptr::null_mut(), 0) } >= 0 {
        return Err(unsupported());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ENODATA) {
        return Err(error);
    }
    check_inode(held, mount, true)
}

fn check_original(file: &File, uid: u32, mount: u64) -> io::Result<()> {
    let info = file.metadata()?;
    if !info.is_file() || info.uid() != uid || info.nlink() != 1
        || info.mode() & 0o7000 != 0 || info.mode() & 0o600 != 0o600
    {
        return Err(unsupported());
    }
    check_inode(file, mount, false)
}

fn mount_id(file: &File) -> io::Result<u64> {
    let mut info = MaybeUninit::<libc::statx>::zeroed();
    // SAFETY: statx writes to a valid struct and AT_EMPTY_PATH selects the held descriptor.
    let info = unsafe {
        if libc::statx(file.as_raw_fd(), c"".as_ptr(), libc::AT_EMPTY_PATH, libc::STATX_MNT_ID, info.as_mut_ptr()) == -1 {
            return Err(io::Error::last_os_error());
        }
        info.assume_init()
    };
    if info.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(unsupported());
    }
    Ok(info.stx_mnt_id)
}

fn ext4_mount(file: &File) -> io::Result<u64> {
    let mut info = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: fstatfs initializes the supplied result on success.
    let info = unsafe {
        if libc::fstatfs(file.as_raw_fd(), info.as_mut_ptr()) == -1 {
            return Err(io::Error::last_os_error());
        }
        info.assume_init()
    };
    if info.f_type != libc::EXT4_SUPER_MAGIC {
        return Err(unsupported());
    }
    // ext2/3 share the same magic. The held mount's type selects the ext4 FIEMAP contract.
    let id = mount_id(file)?;
    let id_text = id.to_string();
    for line in BufReader::new(File::open("/proc/self/mountinfo")?).split(b'\n') {
        let line = line?;
        let mut fields = line.split(|byte| *byte == b' ');
        if fields.next() == Some(id_text.as_bytes()) {
            if fields.find(|field| *field == b"-").is_some() && fields.next() == Some(b"ext4") {
                return Ok(id);
            }
            break;
        }
    }
    Err(unsupported())
}

#[repr(C)]
struct Fsxattr {
    flags: u32,
    extent_size: u32,
    nextents: u32,
    project: u32,
    cow_extent_size: u32,
    reserved: [u8; 8],
}

#[repr(C)]
struct FiemapCount {
    start: u64,
    length: u64,
    flags: u32,
    mapped: u32,
    capacity: u32,
    reserved: u32,
}

fn check_inode(file: &File, mount: u64, directory: bool) -> io::Result<()> {
    if mount_id(file)? != mount {
        return Err(changed());
    }
    const EXTENT: u32 = 0x0008_0000;
    const INDEX: u32 = 0x0000_1000;
    const INLINE_DATA: u32 = 0x1000_0000;
    let mut flags = 0u32;
    let mut extra = MaybeUninit::<Fsxattr>::zeroed();
    // SAFETY: the Linux GETFLAGS ioctl writes an int, and FSGETXATTR writes a struct fsxattr.
    let extra = unsafe {
        if libc::ioctl(file.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) == -1
            || libc::ioctl(file.as_raw_fd(), libc::_IOR::<Fsxattr>(b'X' as u32, 31), extra.as_mut_ptr()) == -1
        {
            return Err(io::Error::last_os_error());
        }
        extra.assume_init()
    };
    let supported = EXTENT | INLINE_DATA | if directory { INDEX } else { 0 };
    const HASATTR: u32 = 0x8000_0000;
    if flags & !supported != 0 || extra.project != 0 || extra.extent_size != 0
        || extra.cow_extent_size != 0 || extra.flags & !HASATTR != 0
    {
        return Err(unsupported());
    }
    if !directory {
        // ext4 reports inline or external xattr storage (including EA inode references).
        // Unlike listxattr, the extent count is independent of namespace visibility.
        const FIEMAP_FLAG_XATTR: u32 = 2;
        let mut map = FiemapCount { start: 0, length: u64::MAX, flags: FIEMAP_FLAG_XATTR, mapped: 0, capacity: 0, reserved: 0 };
        // SAFETY: zero capacity requests the count only; the kernel writes just this header.
        if unsafe { libc::ioctl(file.as_raw_fd(), libc::_IOWR::<FiemapCount>(b'f' as u32, 11), &mut map) } == -1 {
            return Err(io::Error::last_os_error());
        }
        if map.flags != 0 || map.mapped != 0 || extra.flags & HASATTR != 0 {
            return Err(unsupported());
        }
    }
    Ok(())
}

fn new_snapshot(observation: Observation) -> io::Result<Snapshot> {
    let mut token = [0; 16];
    getrandom::fill(&mut token).map_err(io::Error::other)?;
    Ok(Snapshot { token, observation })
}

fn file_stamp(info: &Metadata) -> (u64, i64, i64, i64, i64) {
    (info.len(), info.mtime(), info.mtime_nsec(), info.ctime(), info.ctime_nsec())
}

fn observe(mut file: &File) -> io::Result<Observation> {
    let before = file.metadata()?;
    if before.len() > MAX_CONTENT as u64 {
        return Err(io::ErrorKind::FileTooLarge.into());
    }
    file.rewind()?;
    let mut hash = Sha256::new();
    let mut len = 0;
    let mut pending = 0;
    let mut chunk = [0; 8192];
    loop {
        let remaining = (MAX_CONTENT as u64 + 1 - len) as usize;
        let capacity = remaining.min(chunk.len() - pending);
        let count = match file.read(&mut chunk[pending..pending + capacity]) {
            Ok(0) if pending == 0 => break,
            Ok(0) => return Err(io::ErrorKind::InvalidData.into()),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        len += count as u64;
        if len > MAX_CONTENT as u64 {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        let end = pending + count;
        match std::str::from_utf8(&chunk[..end]) {
            Ok(_) => {
                hash.update(&chunk[..end]);
                pending = 0;
            }
            Err(error) if error.error_len().is_none() => {
                let valid = error.valid_up_to();
                hash.update(&chunk[..valid]);
                chunk.copy_within(valid..end, 0);
                pending = end - valid;
            }
            Err(error) => return Err(invalid_utf8(error)),
        }
    }
    if len != before.len() || file_stamp(&before) != file_stamp(&file.metadata()?) {
        return Err(changed());
    }
    Ok(Observation { len, digest: hash.finalize().into() })
}

fn invalid_utf8(error: std::str::Utf8Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, fs, os::unix::fs::{PermissionsExt, symlink}};

    #[test]
    fn rejects_non_normal_paths_and_preserves_raw_operand_bytes() {
        for value in ["target", "/", "//target", "/parent/../target", "/parent/./target", "/parent/target/", "/parent/\0"] {
            assert_eq!(split_path(Path::new(value)).unwrap_err().kind(), io::ErrorKind::InvalidInput, "{value:?}");
        }
        let bytes = b"/home/\xff.txt";
        let (parent, name) = split_path(Path::new(OsStr::from_bytes(bytes))).unwrap();
        assert_eq!(parent, Path::new("/home"));
        assert_eq!(name.to_bytes(), b"\xff.txt");
        let too_long = format!("/{}", "x".repeat(MAX_PATH));
        assert!(split_path(Path::new(&too_long)).is_err());
    }

    #[test]
    fn observes_exactly_eight_mib_and_rejects_more() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(MAX_CONTENT as u64).unwrap();
        assert_eq!(observe(&file).unwrap().len, MAX_CONTENT as u64);
        file.set_len(MAX_CONTENT as u64 + 1).unwrap();
        assert_eq!(observe(&file).unwrap_err().kind(), io::ErrorKind::FileTooLarge);
    }

    #[test]
    fn utf8_observation_checks_sequences_split_across_reads() {
        let mut file = tempfile::tempfile().unwrap();
        let mut content = vec![b' '; 8191];
        content.extend_from_slice("한".as_bytes());
        file.write_all(&content).unwrap();
        assert_eq!(observe(&file).unwrap().digest, <[u8; 32]>::from(Sha256::digest(&content)));
        file.set_len(8192).unwrap();
        assert_eq!(observe(&file).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn refuses_procfs() {
        let proc = File::open("/proc").unwrap();
        assert_eq!(ext4_mount(&proc).unwrap_err().kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    #[ignore = "requires an ext4 parent owned by the test account; set ZXT_NANO_EXT4_FIXTURE"]
    fn guarded_ext4_round_trip() {
        let parent = PathBuf::from(env::var_os("ZXT_NANO_EXT4_FIXTURE").expect("ext4 fixture path"));
        let root = tempfile::tempdir_in(parent).unwrap();
        let path = root.path().join("한글.txt");
        fs::write(&path, "처음").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let initial = fs::metadata(&path).unwrap();
        let (mut guarded, first) = GuardedFile::prepare(&path).unwrap();
        assert_eq!(guarded.compare().unwrap(), first.observation);
        let writer = File::options().write(true).open(&path).unwrap();
        assert_eq!(GuardedFile::prepare(&path).err().unwrap().kind(), io::ErrorKind::WouldBlock);
        drop(writer);
        assert!(matches!(guarded.save(first.token, b"cancelled", || true), Err(Error::Cancelled)));
        assert_eq!(guarded.compare().unwrap(), first.observation);

        let second = guarded.save(first.token, "중간".as_bytes(), || false).unwrap();
        assert_ne!(second.token, first.token);
        let updated = fs::metadata(&path).unwrap();
        assert_eq!((updated.uid(), updated.gid(), updated.mode()), (initial.uid(), initial.gid(), initial.mode()));
        assert_eq!(fs::read(&path).unwrap(), "중간".as_bytes());
        assert!(matches!(guarded.save(first.token, b"stale", || false), Err(Error::Conflict)));
        let third = guarded.save(second.token, "마지막".as_bytes(), || false).unwrap();
        assert_eq!(guarded.compare().unwrap(), third.observation);
        assert_eq!(fs::read(&path).unwrap(), "마지막".as_bytes());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);

        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let mut external = File::options().write(true).open(&path).unwrap();
        external.write_all("다른글".as_bytes()).unwrap();
        external.set_modified(modified).unwrap();
        drop(external);
        let observed = guarded.compare().unwrap();
        assert_eq!(observed.len, third.observation.len);
        assert_ne!(observed.digest, third.observation.digest);
        assert!(matches!(guarded.save(third.token, b"stale", || false), Err(Error::Conflict)));
        assert!(matches!(guarded.reload(third.observation), Err(Error::Conflict)));
        let reloaded = guarded.reload(observed).unwrap();
        assert_eq!(reloaded.observation, observed);

        let alias = root.path().join("alias");
        fs::hard_link(&path, &alias).unwrap();
        assert_eq!(GuardedFile::prepare(&path).err().unwrap().kind(), io::ErrorKind::Unsupported);
        // 링크 수 변화는 충돌이 아니라 지원 밖 상태다. 앱은 다시 불러오기를 권하지 않는다.
        assert!(matches!(guarded.compare(), Err(Error::Io(error)) if error.kind() == io::ErrorKind::Unsupported));
        fs::remove_file(&alias).unwrap();
        symlink(&path, &alias).unwrap();
        assert!(GuardedFile::prepare(&alias).is_err());
        fs::remove_file(&alias).unwrap();
        let invalid = root.path().join("invalid.txt");
        fs::write(&invalid, b"\xff").unwrap();
        assert_eq!(GuardedFile::prepare(&invalid).err().unwrap().kind(), io::ErrorKind::InvalidData);
        fs::remove_file(invalid).unwrap();

        let file = File::open(&path).unwrap();
        for value in [b"attribute".as_slice(), &[b'x'; 768]] {
            set_xattr(&file, c"user.zxt_test", value);
            assert_eq!(GuardedFile::prepare(&path).err().unwrap().kind(), io::ErrorKind::Unsupported);
            // SAFETY: the test owns the file and the test-only attribute.
            assert_eq!(unsafe { libc::fremovexattr(file.as_raw_fd(), c"user.zxt_test".as_ptr()) }, 0);
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o440)).unwrap();
        assert_eq!(GuardedFile::prepare(&path).err().unwrap().kind(), io::ErrorKind::Unsupported);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let parent = File::open(root.path()).unwrap();
        let mut acl = 2u32.to_le_bytes().to_vec();
        for (tag, permission) in [(1u16, 7u16), (4, 0), (0x20, 0)] {
            acl.extend_from_slice(&tag.to_le_bytes());
            acl.extend_from_slice(&permission.to_le_bytes());
            acl.extend_from_slice(&u32::MAX.to_le_bytes());
        }
        set_xattr(&parent, c"system.posix_acl_default", &acl);
        assert_eq!(GuardedFile::prepare(&path).err().unwrap().kind(), io::ErrorKind::Unsupported);
        // SAFETY: the test owns the fixture directory and its test-only default ACL.
        assert_eq!(unsafe { libc::fremovexattr(parent.as_raw_fd(), c"system.posix_acl_default".as_ptr()) }, 0);

        fs::rename(&path, &alias).unwrap();
        fs::copy(&alias, &path).unwrap();
        assert!(matches!(guarded.reload(observed), Err(Error::Conflict)));
        let (guarded, _) = GuardedFile::prepare(&path).unwrap();
        let moved = root.path().with_extension("moved");
        fs::rename(root.path(), &moved).unwrap();
        fs::create_dir(root.path()).unwrap();
        let result = guarded.compare();
        fs::remove_dir(root.path()).unwrap();
        fs::rename(moved, root.path()).unwrap();
        assert!(matches!(result, Err(Error::Conflict)));

        let (mut guarded, snapshot) = GuardedFile::prepare(&path).unwrap();
        let held = root.path().join("held");
        let result = guarded.save(snapshot.token, b"editor", || {
            fs::rename(&path, &held).unwrap();
            fs::write(&path, b"external").unwrap();
            false
        });
        assert!(matches!(result, Err(Error::Unknown(None))));
        assert_eq!(fs::read(&held).unwrap(), "다른글".as_bytes());
        assert!(!path.exists());
        let moved_external = fs::read_dir(root.path()).unwrap().map(Result::unwrap)
            .find(|entry| entry.file_name().to_string_lossy().starts_with(".zxt-edit-")).unwrap();
        assert_eq!(fs::read(moved_external.path().join("original")).unwrap(), b"external");
        assert!(matches!(guarded.compare(), Err(Error::Unknown(None))));
        assert!(matches!(guarded.reload(snapshot.observation), Err(Error::Unknown(None))));
        assert!(matches!(guarded.save(snapshot.token, b"retry", || panic!("unsafe retry")), Err(Error::Unknown(None))));
    }

    fn set_xattr(file: &File, name: &CStr, data: &[u8]) {
        // SAFETY: the test owns the file and supplies valid pointers for the attribute.
        let status = unsafe { libc::fsetxattr(file.as_raw_fd(), name.as_ptr(), data.as_ptr().cast(), data.len(), 0) };
        assert_eq!(status, 0, "{}: {}", name.to_string_lossy(), io::Error::last_os_error());
    }
}
