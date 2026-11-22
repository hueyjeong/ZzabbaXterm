use super::*;

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let path = std::env::temp_dir().join(temporary_name().unwrap());
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn cleanup_checked(&self, entries: usize) {
        assert_eq!(fs::read_dir(&self.0).unwrap().count(), entries);
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn saves_exact_utf8_twice_without_touching_another_same_name() {
    let left = Sandbox::new();
    let right = Sandbox::new();
    let path = left.path("한글.txt");
    let other = right.path("한글.txt");
    let initial = "\u{feff}한글\r\n둘\n끝\r";
    fs::write(&path, initial).unwrap();
    fs::write(&other, "다른 파일\n").unwrap();
    let (mut file, text) = LocalFile::open(&path).unwrap();
    assert_eq!(text, initial);

    for edited in ["\u{feff}편집\r\n가\n나\r", "\u{feff}재저장\r\n"] {
        file.save(edited).unwrap();
        assert_eq!(fs::read(&path).unwrap(), edited.as_bytes());
        assert_eq!(LocalFile::open(&path).unwrap().1, edited);
    }
    assert_eq!(fs::read(&other).unwrap(), "다른 파일\n".as_bytes());
    left.cleanup_checked(1);
}

#[test]
fn detects_equal_length_equal_mtime_after_staging_and_preserves_both_versions() {
    let dir = Sandbox::new();
    let path = dir.path("same.txt");
    fs::write(&path, "원본\r\n").unwrap();
    let before = fs::metadata(&path).unwrap();
    let (mut file, mut buffer) = LocalFile::open(&path).unwrap();
    buffer.replace_range(.., "미저장\r\n");

    let result = file.save_with_sync(&buffer, |staged| {
        staged.sync_all()?;
        let mut writer = File::options().write(true).open(&path)?;
        writer.write_all("외부\r\n".as_bytes())?;
        writer.set_modified(before.modified()?)?;
        Ok(())
    });
    let after = fs::metadata(&path).unwrap();
    assert_eq!(after.len(), before.len());
    assert_eq!(after.modified().unwrap(), before.modified().unwrap());
    assert!(
        matches!(result, Err(SaveError::Conflict(LocalConflict::Modified(LocalVersion::Text(text)))) if text == "외부\r\n")
    );
    assert_eq!(fs::read(&path).unwrap(), "외부\r\n".as_bytes());
    assert_eq!(buffer, "미저장\r\n");
    assert!(matches!(
        file.save(&buffer),
        Err(SaveError::Conflict(LocalConflict::Modified(_)))
    ));
    dir.cleanup_checked(1);
}

#[test]
fn same_bytes_new_file_is_a_replacement_and_missing_file_is_not_recreated() {
    let dir = Sandbox::new();
    let path = dir.path("target.txt");
    let moved = dir.path("original.txt");
    fs::write(&path, "same\n").unwrap();
    let (mut file, _) = LocalFile::open(&path).unwrap();
    fs::rename(&path, &moved).unwrap();
    fs::write(&path, "same\n").unwrap();
    let buffer = String::from("mine\n");
    assert!(
        matches!(file.save(&buffer), Err(SaveError::Conflict(LocalConflict::Replaced(LocalVersion::Text(text)))) if text == "same\n")
    );
    assert_eq!(fs::read(&path).unwrap(), b"same\n");
    fs::remove_file(&path).unwrap();
    assert!(matches!(
        file.save(&buffer),
        Err(SaveError::Conflict(LocalConflict::Missing))
    ));
    assert!(!path.exists());
    assert_eq!(fs::read(&moved).unwrap(), b"same\n");
    assert_eq!(buffer, "mine\n");
    dir.cleanup_checked(1);
}

#[cfg(unix)]
#[test]
fn changed_parent_during_staging_is_not_written_or_cleaned_by_path() {
    let dir = Sandbox::new();
    let parent = dir.path("parent");
    let moved = dir.path("moved");
    fs::create_dir(&parent).unwrap();
    let path = parent.join("target.txt");
    fs::write(&path, "original\n").unwrap();
    let (mut file, _) = LocalFile::open(&path).unwrap();

    let result = file.save_with_sync("mine\n", |staged| {
        staged.sync_all()?;
        fs::rename(&parent, &moved)?;
        fs::create_dir(&parent)?;
        fs::write(&path, "unrelated\n")
    });
    assert!(
        matches!(result, Err(SaveError::Conflict(LocalConflict::PathChanged))),
        "{result:?}"
    );
    assert_eq!(fs::read(&path).unwrap(), b"unrelated\n");
    assert_eq!(fs::read(moved.join("target.txt")).unwrap(), b"original\n");
    assert_eq!(fs::read_dir(moved).unwrap().count(), 1);
    assert_eq!(fs::read_dir(parent).unwrap().count(), 1);
}

#[cfg(windows)]
#[test]
fn changed_parent_during_staging_is_not_written_or_cleaned_by_path() {
    use std::os::windows::fs::symlink_dir;

    let dir = Sandbox::new();
    let parent = dir.path("parent");
    let other = dir.path("other");
    let alias = dir.path("alias");
    fs::create_dir(&parent).unwrap();
    fs::create_dir(&other).unwrap();
    fs::write(parent.join("target.txt"), "original\n").unwrap();
    fs::write(other.join("target.txt"), "unrelated\n").unwrap();
    symlink_dir(&parent, &alias).unwrap();
    let (mut file, _) = LocalFile::open(&alias.join("target.txt")).unwrap();

    let result = file.save_with_sync("mine\n", |staged| {
        staged.sync_all()?;
        fs::remove_dir(&alias)?;
        symlink_dir(&other, &alias)
    });
    assert!(
        matches!(result, Err(SaveError::Conflict(LocalConflict::PathChanged))),
        "{result:?}"
    );
    assert_eq!(fs::read(other.join("target.txt")).unwrap(), b"unrelated\n");
    assert_eq!(fs::read(parent.join("target.txt")).unwrap(), b"original\n");
    assert_eq!(fs::read_dir(other).unwrap().count(), 1);
    assert_eq!(fs::read_dir(parent).unwrap().count(), 1);
}

#[test]
fn sync_error_preserves_original_and_allows_retry_with_same_buffer() {
    let dir = Sandbox::new();
    let path = dir.path("target.txt");
    let unrelated = dir.path(".zzabba-edit-unrelated.tmp");
    fs::write(&path, "old\n").unwrap();
    fs::write(&unrelated, "keep\n").unwrap();
    let (mut file, _) = LocalFile::open(&path).unwrap();
    let buffer = String::from("mine\n");
    assert!(
        matches!(file.save_with_sync(&buffer, |_| Err(io::Error::other("injected sync failure"))), Err(SaveError::Io { stage: SaveStage::SyncTemporary, source }) if source.to_string() == "injected sync failure")
    );
    assert_eq!(fs::read(&path).unwrap(), b"old\n");
    assert_eq!(fs::read(&unrelated).unwrap(), b"keep\n");
    assert_eq!(buffer, "mine\n");
    dir.cleanup_checked(2);
    file.save(&buffer).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"mine\n");
}

#[test]
fn invalid_utf8_is_rejected_at_open_and_returned_losslessly_on_conflict() {
    let dir = Sandbox::new();
    let path = dir.path("target.txt");
    fs::write(&path, [0xff, 0x80]).unwrap();
    assert!(matches!(
        LocalFile::open(&path),
        Err(OpenError::InvalidUtf8)
    ));
    fs::write(&path, "ok").unwrap();
    let (mut file, _) = LocalFile::open(&path).unwrap();
    fs::write(&path, [0xff, 0x80]).unwrap();
    assert!(
        matches!(file.save("mine"), Err(SaveError::Conflict(LocalConflict::Modified(LocalVersion::NonUtf8(bytes)))) if bytes == [0xff, 0x80])
    );
    assert_eq!(fs::read(&path).unwrap(), [0xff, 0x80]);
    dir.cleanup_checked(1);
}

#[cfg(unix)]
use std::os::unix::fs::symlink;
#[cfg(windows)]
use std::os::windows::fs::symlink_file as symlink;

#[test]
fn does_not_open_or_follow_a_final_symlink() {
    let dir = Sandbox::new();
    let path = dir.path("target.txt");
    let other = dir.path("other.txt");
    let link = dir.path("link.txt");
    fs::write(&path, "old").unwrap();
    fs::write(&other, "keep").unwrap();
    let (mut file, _) = LocalFile::open(&path).unwrap();
    symlink(&other, &link).unwrap();
    assert!(matches!(
        LocalFile::open(&link),
        Err(OpenError::Conflict(LocalConflict::NotRegular))
    ));
    fs::remove_file(&path).unwrap();
    fs::rename(&link, &path).unwrap();
    assert!(matches!(
        file.save("mine"),
        Err(SaveError::Conflict(LocalConflict::NotRegular))
    ));
    assert!(
        fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&other).unwrap(), b"keep");
    dir.cleanup_checked(2);
}

#[test]
fn read_only_file_is_not_replaced() {
    let dir = Sandbox::new();
    let path = dir.path("target.txt");
    fs::write(&path, "original").unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    let writable = permissions.clone();
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions).unwrap();
    let (mut file, _) = LocalFile::open(&path).unwrap();
    let result = file.save("mine");
    assert!(
        matches!(result, Err(SaveError::Io { stage: SaveStage::Replace, ref source }) if source.kind() == io::ErrorKind::PermissionDenied),
        "{result:?}"
    );
    assert_eq!(fs::read(&path).unwrap(), b"original");
    assert!(fs::metadata(&path).unwrap().permissions().readonly());
    fs::set_permissions(&path, writable).unwrap();
    dir.cleanup_checked(1);
}

#[test]
fn staged_bytes_changed_at_sync_are_rejected() {
    let dir = Sandbox::new();
    let path = dir.path("target.txt");
    fs::write(&path, "original").unwrap();
    let (mut file, _) = LocalFile::open(&path).unwrap();
    let result = file.save_with_sync("mine", |mut staged| {
        staged.rewind()?;
        staged.write_all(b"torn")?;
        staged.sync_all()
    });
    assert!(
        matches!(
            result,
            Err(SaveError::Io {
                stage: SaveStage::VerifyTemporary,
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(fs::read(&path).unwrap(), b"original");
    dir.cleanup_checked(1);
}

#[cfg(windows)]
#[test]
fn staged_windows_body_does_not_admit_another_data_handle() {
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

    let dir = Sandbox::new();
    let path = dir.path("target.txt");
    fs::write(&path, "original").unwrap();
    let (mut file, _) = LocalFile::open(&path).unwrap();
    file.save_with_sync("mine", |staged| {
        let stage = fs::read_dir(&dir.0)?
            .map(|entry| entry.unwrap().path())
            .find(|entry| entry != &path)
            .unwrap();
        let error = File::options()
            .read(true)
            .write(true)
            .open(stage)
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));
        staged.sync_all()
    })
    .unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"mine");
    dir.cleanup_checked(1);
}

#[cfg(windows)]
#[test]
fn windows_failed_replace_preserves_original_and_cleans_only_the_staging_file() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    let dir = Sandbox::new();
    let path = dir.path("target.txt");
    fs::write(&path, "old\r\n").unwrap();
    let (mut file, _) = LocalFile::open(&path).unwrap();
    let blocker = File::options()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&path)
        .unwrap();
    let buffer = String::from("mine\r\n");
    let result = file.save(&buffer);
    assert!(
        matches!(
            result,
            Err(SaveError::Io {
                stage: SaveStage::Replace,
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(fs::read(&path).unwrap(), b"old\r\n");
    assert_eq!(buffer, "mine\r\n");
    dir.cleanup_checked(1);
    drop(blocker);
    file.save(&buffer).unwrap();
    assert_eq!(fs::read(&path).unwrap(), buffer.as_bytes());
}

#[cfg(windows)]
#[test]
fn windows_preserves_explicit_protected_dacl() {
    let dir = Sandbox::new();
    let path = dir.path("target.txt");
    fs::write(&path, "old").unwrap();
    let output = std::process::Command::new("icacls")
        .arg(&path)
        .args(["/inheritance:d", "/deny", "*S-1-5-32-546:(W)"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let old = super::os::Access::read(&File::open(&path).unwrap()).unwrap();
    let (mut file, _) = LocalFile::open(&path).unwrap();
    file.save("mine").unwrap();
    assert!(old.matches(&File::open(&path).unwrap()).unwrap());
    assert_eq!(fs::read(&path).unwrap(), b"mine");
    dir.cleanup_checked(1);
}

#[cfg(unix)]
#[test]
fn unix_preserves_mode_across_atomic_replacement() {
    use std::os::unix::fs::PermissionsExt;

    let dir = Sandbox::new();
    let path = dir.path("target.txt");
    fs::write(&path, "old").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let (mut file, _) = LocalFile::open(&path).unwrap();
    file.save("mine").unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o640
    );
    assert_eq!(fs::read(&path).unwrap(), b"mine");
}

#[cfg(target_os = "linux")]
fn install_linux_acl(path: &Path, name: &std::ffi::CStr) -> Vec<u8> {
    use std::os::fd::AsRawFd;

    let file = File::open(path).unwrap();
    let mut acl = 2u32.to_le_bytes().to_vec();
    let owner: u16 = if name == c"system.posix_acl_default" {
        7
    } else {
        6
    };
    for (tag, perm, id) in [
        (1u16, owner, u32::MAX),
        (2, 6, 12345),
        (4, 0, u32::MAX),
        (16, 6, u32::MAX),
        (32, 0, u32::MAX),
    ] {
        acl.extend(tag.to_le_bytes());
        acl.extend(perm.to_le_bytes());
        acl.extend(id.to_le_bytes());
    }
    // SAFETY: file and the Linux ACL xattr buffer are live for this call.
    assert_eq!(
        unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                acl.as_ptr().cast(),
                acl.len(),
                0,
            )
        },
        0,
        "{}",
        io::Error::last_os_error()
    );
    acl
}

#[cfg(target_os = "linux")]
#[test]
fn unix_rejects_extended_acl_and_keeps_original_acl_and_bytes() {
    use std::os::fd::AsRawFd;

    let dir = Sandbox::new();
    let path = dir.path("target.txt");
    fs::write(&path, "original").unwrap();
    let name = c"system.posix_acl_access";
    let expected = install_linux_acl(&path, name);
    let (mut file, _) = LocalFile::open(&path).unwrap();
    let result = file.save("mine");
    assert!(
        matches!(result, Err(SaveError::Io { stage: SaveStage::PreservePermissions, ref source }) if source.kind() == io::ErrorKind::Unsupported),
        "{result:?}"
    );
    let disk = File::open(&path).unwrap();
    let mut actual = vec![0; expected.len()];
    // SAFETY: actual is writable for the supplied byte length.
    let size = unsafe {
        libc::fgetxattr(
            disk.as_raw_fd(),
            name.as_ptr(),
            actual.as_mut_ptr().cast(),
            actual.len(),
        )
    };
    assert_eq!(size, expected.len() as isize);
    assert_eq!(actual, expected);
    assert_eq!(fs::read(&path).unwrap(), b"original");
    dir.cleanup_checked(1);
}

#[cfg(target_os = "linux")]
#[test]
fn unix_prepared_group_writable_body_stays_behind_a_private_directory() {
    use std::os::unix::fs::PermissionsExt;

    let dir = Sandbox::new();
    let path = dir.path("target.txt");
    fs::write(&path, "original").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o660)).unwrap();
    install_linux_acl(&dir.0, c"system.posix_acl_default");
    let (mut file, _) = LocalFile::open(&path).unwrap();
    file.save_with_sync("mine", |staged| {
        let private = fs::read_dir(&dir.0)?
            .map(|entry| entry.unwrap().path())
            .find(|entry| entry != &path)
            .unwrap();
        assert_eq!(fs::metadata(private)?.permissions().mode() & 0o777, 0o700);
        assert_eq!(staged.metadata()?.permissions().mode() & 0o777, 0o660);
        assert_eq!(fs::read(&path)?, b"original");
        staged.sync_all()
    })
    .unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o660
    );
    assert!(os::Access::read(&File::open(&path).unwrap()).is_ok());
    assert_eq!(fs::read(&path).unwrap(), b"mine");
    dir.cleanup_checked(1);
}

#[cfg(target_os = "linux")]
#[test]
fn unix_owner_without_write_permission_cannot_replace_through_writable_parent() {
    use std::os::unix::fs::PermissionsExt;

    // SAFETY: geteuid takes no arguments. Root bypasses Unix discretionary write checks.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = Sandbox::new();
    let path = dir.path("target.txt");
    fs::write(&path, "original").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o460)).unwrap();
    let (mut file, _) = LocalFile::open(&path).unwrap();
    let result = file.save("mine");
    assert!(
        matches!(result, Err(SaveError::Io { stage: SaveStage::Replace, ref source }) if source.kind() == io::ErrorKind::PermissionDenied),
        "{result:?}"
    );
    assert_eq!(fs::read(&path).unwrap(), b"original");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o460
    );
    dir.cleanup_checked(1);
}

#[test]
fn ipc_handles_belong_to_one_selection_and_document() {
    use registry::{EditorError, LocalEditors, Selected};

    let left = Sandbox::new();
    let right = Sandbox::new();
    let a = left.path("한글.txt");
    let b = right.path("한글.txt");
    fs::write(&a, "왼쪽\r\n").unwrap();
    fs::write(&b, "오른쪽\r\n").unwrap();

    let editors = LocalEditors::default();
    editors.navigate("left");
    editors.navigate("right");
    let first = editors.document("left").unwrap();
    let second = editors.document("right").unwrap();
    assert!(matches!(editors.selected("left", first, None), Ok(Selected::Cancelled)));
    let Selected::Opened { handle: a_handle, name, text } = editors.selected("left", first, Some(&a)).unwrap() else {
        panic!("the selected file was not opened");
    };
    assert_eq!((name.as_str(), text.as_str()), ("한글.txt", "왼쪽\r\n"));
    let Selected::Opened { handle: b_handle, .. } = editors.selected("left", first, Some(&b)).unwrap() else {
        panic!("the second selected file was not opened");
    };
    assert_ne!(a_handle, b_handle);
    assert!(matches!(editors.save("right", second, &a_handle, "wrong"), Err(EditorError::InvalidHandle)));
    assert!(matches!(editors.reload("right", second, &a_handle), Err(EditorError::InvalidHandle)));
    assert!(matches!(editors.close("right", second, &a_handle), Err(EditorError::InvalidHandle)));
    assert!(matches!(editors.save("left", first, a.to_str().unwrap(), "wrong"), Err(EditorError::InvalidHandle)));
    editors.close("left", first, &b_handle).unwrap();
    assert!(matches!(editors.save("left", first, &b_handle, "wrong"), Err(EditorError::InvalidHandle)));
    editors.save("left", first, &a_handle, "내 문서\r\n").unwrap();
    assert_eq!(fs::read_to_string(&a).unwrap(), "내 문서\r\n");
    assert_eq!(fs::read_to_string(&b).unwrap(), "오른쪽\r\n");

    editors.navigate("left");
    let next = editors.document("left").unwrap();
    assert!(matches!(editors.selected("left", first, Some(&a)), Err(EditorError::DocumentClosed)));
    assert!(matches!(editors.reload("left", first, &a_handle), Err(EditorError::DocumentClosed)));
    assert!(matches!(editors.close("left", next, &a_handle), Err(EditorError::InvalidHandle)));
    editors.destroy("left");
    editors.navigate("left");
    assert!(matches!(editors.selected("left", next, Some(&a)), Err(EditorError::DocumentClosed)));
}

#[test]
fn ipc_conflict_preserves_disk_and_buffer_until_explicit_reload() {
    use registry::{LocalEditors, Selected};

    let dir = Sandbox::new();
    let path = dir.path("selected.txt");
    fs::write(&path, "base\n").unwrap();
    let editors = LocalEditors::default();
    editors.navigate("main");
    let document = editors.document("main").unwrap();
    let Selected::Opened { handle, .. } = editors.selected("main", document, Some(&path)).unwrap() else {
        panic!("the selected file was not opened");
    };
    let before = fs::metadata(&path).unwrap();
    fs::write(&path, "disk\n").unwrap();
    File::options().write(true).open(&path).unwrap().set_modified(before.modified().unwrap()).unwrap();
    let buffer = String::from("mine\n");
    assert_eq!(editors.compare("main", document, &handle).unwrap(), "disk\n");
    for _ in 0..2 {
        let conflict = editors.save("main", document, &handle, &buffer).unwrap_err();
        let wire = tauri::ipc::InvokeError::from(conflict).0;
        assert_eq!(wire["kind"], "conflict");
        assert_eq!(wire["conflict"]["reason"], "modified");
        assert_eq!(wire["conflict"]["diskText"], "disk\n");
        assert_eq!(fs::read_to_string(&path).unwrap(), "disk\n");
        assert_eq!(buffer, "mine\n");
    }
    fs::write(&path, [0xff]).unwrap();
    assert!(matches!(
        editors.reload("main", document, &handle),
        Err(registry::EditorError::InvalidUtf8)
    ));
    let conflict = editors.save("main", document, &handle, &buffer).unwrap_err();
    let wire = tauri::ipc::InvokeError::from(conflict).0;
    assert_eq!(wire["conflict"]["reason"], "modified");
    assert!(wire["conflict"]["diskText"].is_null());
    assert_eq!(fs::read(&path).unwrap(), [0xff]);
    fs::write(&path, "disk\n").unwrap();
    assert_eq!(editors.reload("main", document, &handle).unwrap(), "disk\n");
    editors.save("main", document, &handle, &buffer).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), buffer);
    dir.cleanup_checked(1);
}

#[test]
fn cli_wait_belongs_to_the_claimed_tab_in_the_approving_document() {
    use cli::{Opening, Pending, Request, Response};
    use registry::{EditorError, LocalEditors, Selected};
    use tokio::{sync::oneshot, time::Instant};

    let dir = Sandbox::new();
    let path = dir.path("한글.txt");
    fs::write(&path, "원본").unwrap();
    let editors = LocalEditors::default();
    editors.navigate("main");
    editors.navigate("other");
    let document = editors.document("main").unwrap();
    let other = editors.document("other").unwrap();

    let offer = || {
        let request: Request = serde_json::from_value(serde_json::json!({ "path": &path, "wait": true })).unwrap();
        let (reply, response) = oneshot::channel();
        let (opened, mounted) = oneshot::channel();
        let opening = Opening::new(Instant::now() + cli::OPEN_TIMEOUT);
        let pending = Pending::new(request, reply, opened, opening.clone());
        let (file, text) = LocalFile::open(&path).unwrap();
        let (id, _) = editors.offer("main", document, pending, file, text).unwrap();
        (id, response, mounted, opening)
    };
    let (id, mut response, mut mounted, _) = offer();
    let (other_id, mut other_response, mut other_mounted, _) = offer();
    assert!(matches!(editors.claim("other", other, &id), Err(EditorError::InvalidHandle)));
    let Selected::Opened { handle, text, .. } = editors.claim("main", document, &id).unwrap() else { panic!("CLI 탭 누락"); };
    let Selected::Opened { handle: other_handle, .. } = editors.claim("main", document, &other_id).unwrap() else { panic!("CLI 탭 누락"); };
    assert_eq!(text, "원본");
    assert_ne!(handle, other_handle);
    assert!(matches!(editors.claim("main", document, &id), Err(EditorError::InvalidHandle)));
    assert!(editors.close("main", document, &handle).is_err());
    assert!(editors.respond_to_offer("main", document, &id, &other_handle, true).is_err());
    assert_eq!(mounted.try_recv(), Err(oneshot::error::TryRecvError::Empty));

    editors.respond_to_offer("main", document, &id, &handle, true).unwrap();
    editors.respond_to_offer("main", document, &other_id, &other_handle, true).unwrap();
    assert_eq!(mounted.try_recv(), Ok(()));
    assert_eq!(other_mounted.try_recv(), Ok(()));
    editors.close("main", document, &handle).unwrap();
    assert_eq!(response.try_recv(), Ok(Response::Closed));
    assert_eq!(other_response.try_recv(), Err(oneshot::error::TryRecvError::Empty));

    let (cancelled_id, mut cancelled_response, mut cancelled_mounted, opening) = offer();
    let Selected::Opened { handle: cancelled_handle, .. } = editors.claim("main", document, &cancelled_id).unwrap()
        else { panic!("CLI 탭 누락"); };
    assert!(opening.cancel_pending());
    assert!(editors.respond_to_offer("main", document, &cancelled_id, &cancelled_handle, true).is_err());
    editors.discard_offer(document, &cancelled_id, false);
    assert_eq!(cancelled_response.try_recv(), Err(oneshot::error::TryRecvError::Closed));
    assert_eq!(cancelled_mounted.try_recv(), Err(oneshot::error::TryRecvError::Closed));

    let (pending_id, mut pending_response, mut pending_mounted, _) = offer();
    editors.navigate("main");
    assert_eq!(other_response.try_recv(), Err(oneshot::error::TryRecvError::Closed));
    assert_eq!(pending_response.try_recv(), Err(oneshot::error::TryRecvError::Closed));
    assert_eq!(pending_mounted.try_recv(), Err(oneshot::error::TryRecvError::Closed));
    let next = editors.document("main").unwrap();
    assert!(matches!(editors.claim("main", next, &pending_id), Err(EditorError::InvalidHandle)));
    assert!(matches!(editors.close("main", next, &other_handle), Err(EditorError::InvalidHandle)));
}
