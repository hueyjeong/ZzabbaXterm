use super::{COMMIT, OpenError, Version, check_parent, os, publication::PublicationError};
use sha2::{Digest, Sha256};
use std::{ffi::OsString, fs::{self, File}, io::{self, Read}, path::{Path, PathBuf}};

struct Location {
    selected_parent: PathBuf,
    parent: os::Directory,
    name: OsString,
}

impl Location {
    fn open(path: &Path) -> io::Result<Self> {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(io::ErrorKind::InvalidInput.into());
        };
        if !path.is_absolute() || !os::valid_path(path) || !os::valid_name(name) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let canonical = fs::canonicalize(parent)?;
        if !os::valid_path(&canonical) { return Err(io::ErrorKind::InvalidInput.into()); }
        let result = Self { selected_parent: parent.into(), parent: os::Directory::open(canonical)?, name: name.into() };
        result.check_parent()?;
        Ok(result)
    }

    fn check_parent(&self) -> io::Result<()> {
        check_parent(&self.selected_parent, &self.parent).map_err(|error| match error {
            OpenError::Io(source) => source,
            _ => changed(),
        })
    }

    #[cfg(unix)]
    fn open_file(&self) -> io::Result<Option<File>> {
        self.parent.open_storage_file(&self.name)
    }

    #[cfg(windows)]
    fn open_file(&self) -> io::Result<Option<File>> {
        match self.parent.open_file(&self.name) {
            Ok(Some(file)) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Ok(None) => Err(io::ErrorKind::InvalidInput.into()),
            Err(error) => Err(error),
        }
    }

    fn check_file(&self, id: &os::FileId) -> io::Result<()> {
        self.check_parent()?;
        if self.parent.entry_id(&self.name)?.as_ref() != Some(id) { return Err(changed()); }
        Ok(())
    }
}

pub(crate) fn open_upload(path: &Path) -> io::Result<File> {
    let location = Location::open(path)?;
    let file = location.open_file()?.ok_or(io::ErrorKind::NotFound)?;
    location.check_file(&os::file_id(&file)?)?;
    Ok(file)
}

pub(crate) struct DownloadTarget {
    location: Location,
    original: Option<(File, Version)>,
}

impl DownloadTarget {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let location = Location::open(path)?;
        let original = if let Some(mut file) = location.open_file()? {
            let version = version(&mut file)?;
            location.check_file(&version.id)?;
            Some((file, version))
        } else { None };
        Ok(Self { location, original })
    }

    pub(crate) fn replaces_existing(&self) -> bool { self.original.is_some() }

    pub(crate) fn stage(&self) -> io::Result<DownloadStage<'_>> {
        self.location.check_parent()?;
        let access = self.original.as_ref().map(|(file, _)| os::Access::read(file)).transpose()?;
        Ok(DownloadStage { target: self, temp: self.location.parent.stage(access.as_ref())? })
    }
}

pub(crate) struct DownloadStage<'a> {
    target: &'a DownloadTarget,
    temp: os::Temporary<'a>,
}

impl DownloadStage<'_> {
    pub(crate) fn writer(&mut self) -> io::Result<File> { self.temp.file().try_clone() }

    pub(crate) fn commit(self, closed: &tokio::sync::watch::Sender<bool>, digest: [u8; 32]) -> Result<(), PublicationError> {
        self.commit_inner(closed, digest, || {})
    }

    fn commit_inner(mut self, closed: &tokio::sync::watch::Sender<bool>, digest: [u8; 32], before_publish: impl FnOnce()) -> Result<(), PublicationError> {
        let target = &self.target.location;
        let _commit = COMMIT.lock().map_err(|_| io::Error::other("local file commit lock poisoned"))?;
        if let Some((original, _)) = &self.target.original {
            os::Access::read(original)?.finish_staging(self.temp.file())?;
        }
        self.temp.file().sync_all()?;
        if version(self.temp.file())?.digest != digest { return Err(changed().into()); }
        target.check_parent()?;
        let replacement = if let Some((original, selected)) = &self.target.original {
            let mut current = target.open_file()?.ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            let access = os::Access::read(original)?;
            if !access.matches(self.temp.file())? { return Err(changed().into()); }
            let verify = move |file: &mut File| {
                if version(file)? != *selected || !access.matches(file)? { return Err(changed()); }
                target.check_parent()
            };
            verify(&mut current)?;
            target.parent.check_replace_access(&target.name, &current)?;
            target.check_file(&selected.id)?;
            Some((current, verify))
        } else {
            None
        };
        let live = closed.borrow();
        if *live { return Err(io::Error::from(io::ErrorKind::Interrupted).into()); }
        before_publish();
        if let Some((current, verify)) = replacement {
            self.temp.publish_download(&target.name, &current, verify)
        } else {
            Ok(self.temp.install(&target.name, false)?)
        }
    }
}

fn version(file: &mut File) -> io::Result<Version> {
    use std::io::Seek;
    file.rewind()?;
    let id = os::file_id(file)?;
    let before = file.metadata()?;
    let mut hash = Sha256::new();
    let read = io::copy(file, &mut hash)?;
    let after = file.metadata()?;
    if before.len() != read || after.len() != read || before.modified()? != after.modified()? {
        return Err(changed());
    }
    Ok(Version { id, digest: hash.finalize().into() })
}

fn changed() -> io::Error { io::Error::new(io::ErrorKind::AlreadyExists, "selected file or directory changed") }

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn storage_upload_reads_original_after_same_name_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chosen.bin");
        fs::write(&path, b"original").unwrap();
        let mut upload = open_upload(&path).unwrap();
        let moved = dir.path().join("moved.bin");
        fs::rename(&path, &moved).unwrap();
        fs::write(&path, b"replacement").unwrap();
        let mut body = Vec::new();
        upload.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"original");
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
    }

    #[cfg(unix)]
    #[test]
    fn storage_download_unix_accepts_absent_only_and_never_clobbers() {
        use std::os::unix::fs::symlink;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let closed = tokio::sync::watch::Sender::new(false);
        let path = dir.path().join("new.bin");
        let approval = DownloadTarget::open(&path).unwrap();
        assert!(!approval.replaces_existing());
        let transfer = |approval: &DownloadTarget| {
            let mut stage = approval.stage().unwrap();
            stage.writer().unwrap().write_all(b"object").unwrap();
            stage.commit(&closed, Sha256::digest(b"object").into())
        };
        transfer(&approval).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"object");
        assert!(DownloadTarget::open(&path).unwrap().replaces_existing());

        let late = dir.path().join("late.bin");
        let approval = DownloadTarget::open(&late).unwrap();
        fs::write(&late, b"external").unwrap();
        assert!(matches!(transfer(&approval), Err(PublicationError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists));
        assert_eq!(fs::read(&late).unwrap(), b"external");

        let link = dir.path().join("link.bin");
        let dangling = dir.path().join("dangling.bin");
        let directory = dir.path().join("directory");
        let fifo = dir.path().join("fifo");
        symlink(&path, &link).unwrap();
        symlink(dir.path().join("absent"), &dangling).unwrap();
        fs::create_dir(&directory).unwrap();
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: the name is NUL-terminated and belongs to the isolated fixture.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        for path in [link, dangling, directory, fifo] {
            assert_eq!(DownloadTarget::open(&path).err().unwrap().kind(), io::ErrorKind::InvalidInput);
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 6);
    }

    #[test]
    fn storage_download_preserves_changes_since_selection() {
        for action in ["create", "edit", "in-place", "delete", "replace", "parent", "cancel"] {
            let dir = tempfile::tempdir().unwrap();
            let parent = dir.path().join("parent");
            fs::create_dir(&parent).unwrap();
            let path = parent.join("chosen.bin");
            if action != "create" { fs::write(&path, b"original").unwrap(); }
            let target = DownloadTarget::open(&path).unwrap();
            let mut stage = target.stage().unwrap();
            stage.writer().unwrap().write_all(b"download").unwrap();
            let closed = tokio::sync::watch::Sender::new(action == "cancel");
            let moved_parent = dir.path().join("old-parent");
            let expected = match action {
                "create" => { fs::write(&path, b"unseen").unwrap(); Some(&b"unseen"[..]) }
                "edit" => {
                    let (mut editor, _) = super::super::LocalFile::open(&path).unwrap();
                    editor.save("editor changes").unwrap();
                    Some(&b"editor changes"[..])
                }
                "delete" => { fs::remove_file(&path).unwrap(); None }
                "replace" => {
                    fs::rename(&path, parent.join("previous.bin")).unwrap();
                    fs::write(&path, b"original").unwrap();
                    Some(&b"original"[..])
                }
                "in-place" => { fs::write(&path, b"external").unwrap(); Some(&b"external"[..]) }
                "parent" => {
                    match fs::rename(&parent, &moved_parent) {
                        Ok(()) => {}
                        Err(error) if cfg!(windows) && error.kind() == io::ErrorKind::PermissionDenied => {
                            assert_eq!(fs::read(&path).unwrap(), b"original");
                            drop(stage);
                            assert_eq!(fs::read_dir(&parent).unwrap().count(), 1);
                            continue;
                        }
                        Err(error) => panic!("parent rename: {error}"),
                    }
                    fs::create_dir(&parent).unwrap();
                    fs::write(&path, b"other parent").unwrap();
                    Some(&b"other parent"[..])
                }
                _ => Some(&b"original"[..]),
            };
            assert!(stage.commit(&closed, Sha256::digest(b"download").into()).is_err(), "{action}");
            if let Some(expected) = expected {
                assert_eq!(fs::read(&path).unwrap(), expected, "{action}");
            } else { assert!(!path.exists()); }
            if action == "parent" {
                assert_eq!(fs::read(moved_parent.join("chosen.bin")).unwrap(), b"original");
                assert_eq!(fs::read_dir(&moved_parent).unwrap().count(), 1);
            }
            assert_eq!(fs::read_dir(&parent).unwrap().count(), usize::from(expected.is_some()) + usize::from(action == "replace"));
        }
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn storage_download_preserves_late_destination_changes() {
        for action in ["create", "in-place", "replace"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("chosen.bin");
            let moved = dir.path().join("original.bin");
            if action != "create" { fs::write(&path, b"original").unwrap(); }
            let target = DownloadTarget::open(&path).unwrap();
            let mut stage = target.stage().unwrap();
            stage.writer().unwrap().write_all(b"download").unwrap();
            let closed = tokio::sync::watch::Sender::new(false);
            let result = stage.commit_inner(&closed, Sha256::digest(b"download").into(), || {
                if action == "replace" { fs::rename(&path, &moved).unwrap(); }
                fs::write(&path, b"external").unwrap();
            });
            if cfg!(target_os = "linux") && action != "create" {
                let Err(PublicationError::Unconfirmed(recovery)) = result else {
                    panic!("{action}: expected a recovery path, got {result:?}");
                };
                assert!(!path.exists(), "{action}");
                assert_eq!(fs::read(&recovery).unwrap(), b"external", "{action}");
                assert_eq!(fs::read_dir(recovery.parent().unwrap()).unwrap().count(), 1);
            } else {
                let Err(PublicationError::Io(error)) = result else {
                    panic!("{action}: expected a local IO error, got {result:?}");
                };
                if action == "create" {
                    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
                }
                assert_eq!(fs::read(&path).unwrap(), b"external", "{action}");
            }
            if action == "replace" { assert_eq!(fs::read(&moved).unwrap(), b"original"); }
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1 + usize::from(action == "replace"));
        }
    }
}
