use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Barrier, atomic::{AtomicU64, Ordering}},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

use super::{profile::Draft, store::{SaveStatus, Store, StoreError}};

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "zzabbaxterm-c01-{}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn file(&self) -> PathBuf {
        self.0.join("connections.json")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn draft(dir: &Path, group: &str, target: &str) -> Value {
    json!({
        "version": 1,
        "groupRevision": null,
        "targetRevision": null,
        "group": {"id": group, "displayName": "개발"},
        "target": {
            "id": target,
            "groupId": group,
            "displayName": "작업 셸",
            "defaultMethod": "local",
            "local": {
                "shell": if cfg!(windows) { "powerShell" } else { "bash" },
                "startDirectory": dir,
            },
        },
    })
}

fn save(store: &Store, input: Value) -> Result<SaveStatus, StoreError> {
    let draft: Draft = serde_json::from_value(input).unwrap();
    store.save(draft.validate().unwrap())
}

#[cfg(any(windows, unix))]
pub(crate) fn with_saved_pty_targets(run: impl FnOnce(&Path, &Path)) {
    let root = TestDir::new();
    let store = Store::new(&root.0);
    let dirs = ["first", "second"].map(|id| {
        let directory = root.0.join(format!("{id} directory"));
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("cwd-marker"), id).unwrap();
        assert_eq!(save(&store, draft(&directory, id, id)).unwrap(), SaveStatus::Saved);
        directory
    });
    run(&root.0, &dirs[1]);
}

mod local_probe {
    use super::*;
    use crate::terminal::Sessions;
    use crate::connections::probe::{RunKey, SaveRequest, start, wait, cancel, save_confirmed};

    fn start_at(sessions: &Sessions, dir: &Path, version: u64) -> RunKey {
        start(sessions, "main", sessions.document("main"), "draft".into(), version,
            serde_json::from_value(draft(dir, "work", "first")).unwrap()).unwrap()
    }

    fn save_at(sessions: &Sessions, owner: &str, run: &RunKey, input: Value, store: &Store) -> Result<(), String> {
        let request: SaveRequest = serde_json::from_value(json!({"run": run, "draft": input})).unwrap();
        save_confirmed(sessions, owner, sessions.document(owner), request, store)
    }

    #[test]
    fn c01_probe_normal_exit_authorizes_only_the_tested_draft() {
        let dir = TestDir::new();
        let store = Store::new(&dir.0);
        let sessions = Sessions::default();
        let input = draft(&dir.0, "work", "first");
        let run = start_at(&sessions, &dir.0, 1);
        assert!(save_at(&sessions, "main", &run, input.clone(), &store).is_err());
        let result = wait(&sessions, "main", 0, &run).unwrap();
        assert!(!result.session_id.is_empty());
        assert!(save_at(&sessions, "other", &run, input.clone(), &store).is_err());
        let forged = serde_json::from_value(json!({"draftId": "draft", "draftVersion": 1, "runId": "invented"})).unwrap();
        assert!(save_at(&sessions, "main", &forged, input.clone(), &store).is_err());
        let mut forged = serde_json::to_value(&run).unwrap();
        forged["draftVersion"] = json!(2);
        assert!(save_at(&sessions, "main", &serde_json::from_value(forged).unwrap(), input.clone(), &store).is_err());
        let mut modified = input.clone();
        modified["target"]["displayName"] = json!("시험 후 수정");
        assert!(save_at(&sessions, "main", &run, modified, &store).is_err());
        assert!(!dir.file().exists());
        save_at(&sessions, "main", &run, input.clone(), &store).unwrap();
        assert_eq!(store.load().unwrap().targets[0].profile.local.start_directory, dir.0);
        assert!(save_at(&sessions, "main", &run, input, &store).is_err());
    }

    #[test]
    fn c01_probe_missing_directory_invalidates_prior_success() {
        let dir = TestDir::new();
        let store = Store::new(&dir.0);
        let sessions = Sessions::default();
        let valid = draft(&dir.0, "work", "first");
        assert_eq!(save(&store, valid.clone()).unwrap(), SaveStatus::Saved);
        let original = fs::read(dir.file()).unwrap();
        let run = start_at(&sessions, &dir.0, 1);
        wait(&sessions, "main", 0, &run).unwrap();
        let missing = dir.0.join("missing");
        let second = start_at(&sessions, &missing, 2);
        let error = wait(&sessions, "main", 0, &second).err().unwrap();
        assert!(error.contains("작업 디렉터리"), "{error}");
        cancel(&sessions, "main", 0, &second).unwrap();
        cancel(&sessions, "main", 0, &run).unwrap();
        assert!(save_at(&sessions, "main", &run, valid, &store).is_err());
        assert_eq!(fs::read(dir.file()).unwrap(), original);
    }

    #[test]
    fn c01_probe_cancel_and_navigation_revoke_only_owned_runs() {
        let dir = TestDir::new();
        let store = Store::new(&dir.0);
        let sessions = Sessions::default();
        let input = draft(&dir.0, "work", "first");
        let run = start_at(&sessions, &dir.0, 1);
        let other = start(&sessions, "other", 0, "draft".into(), 1,
            serde_json::from_value(input.clone()).unwrap()).unwrap();
        assert!(cancel(&sessions, "other", 0, &run).is_err());
        cancel(&sessions, "main", 0, &run).unwrap();
        assert!(wait(&sessions, "main", 0, &run).is_err());
        let run = start_at(&sessions, &dir.0, 2);
        wait(&sessions, "main", 0, &run).unwrap();
        sessions.navigate("main");
        assert!(wait(&sessions, "main", 0, &run).is_err());
        assert!(start(&sessions, "main", 0, "draft".into(), 3,
            serde_json::from_value(input.clone()).unwrap()).is_err());
        assert!(save_at(&sessions, "main", &run, input.clone(), &store).is_err());
        sessions.destroy("main");
        assert!(start(&sessions, "main", sessions.document("main"), "draft".into(), 3,
            serde_json::from_value(input.clone()).unwrap()).is_err());
        wait(&sessions, "other", 0, &other).unwrap();
        save_at(&sessions, "other", &other, input, &store).unwrap();
    }
}

#[test]
fn c01_forbidden_fields_cannot_change_an_existing_file() {
    let dir = TestDir::new();
    let store = Store::new(&dir.0);
    let input = draft(&dir.0, "work", "first");
    assert_eq!(save(&store, input.clone()).unwrap(), SaveStatus::Saved);
    let original = fs::read(dir.file()).unwrap();

    for (pointer, field) in [
        ("", "accessToken"),
        ("/group", "password"),
        ("/target", "transport"),
        ("/target/local", "authentication"),
        ("/target/local", "password"),
        ("/target/local", "command"),
        ("/target/local", "args"),
    ] {
        let mut forbidden = input.clone();
        forbidden.pointer_mut(pointer).unwrap().as_object_mut().unwrap()
            .insert(field.into(), json!(""));
        let error = serde_json::from_value::<Draft>(forbidden).unwrap_err();
        assert!(error.to_string().contains(&format!("unknown field `{field}`")));
    }
    assert_eq!(fs::read(dir.file()).unwrap(), original);
}

#[test]
fn c01_simultaneous_saves_preserve_two_same_named_targets() {
    let dir = TestDir::new();
    let barrier = Barrier::new(2);
    thread::scope(|scope| {
        let writers = [("work", "first"), ("personal", "second")].map(|(group, target)| {
            let dir = &dir.0;
            let barrier = &barrier;
            scope.spawn(move || {
                let store = Store::new(dir);
                barrier.wait();
                save(&store, draft(dir, group, target)).unwrap()
            })
        });
        for writer in writers {
            assert_eq!(writer.join().unwrap(), SaveStatus::Saved);
        }
    });

    let file = Store::new(&dir.0).load().unwrap();
    assert_eq!(file.groups.len(), 2);
    assert_eq!(file.targets.len(), 2);
    for (id, group) in [("first", "work"), ("second", "personal")] {
        let target = file.targets.iter().find(|entry| entry.profile.id.as_str() == id).unwrap();
        assert_eq!(target.profile.group_id.as_str(), group);
        assert_eq!(target.profile.display_name, "작업 셸");
        assert_eq!(target.profile.local.start_directory, dir.0);
    }
}

#[test]
fn c01_stale_edit_of_the_same_target_returns_a_conflict() {
    let dir = TestDir::new();
    let store = Store::new(&dir.0);
    let mut editor = draft(&dir.0, "work", "first");
    assert_eq!(save(&store, editor.clone()).unwrap(), SaveStatus::Saved);
    editor["groupRevision"] = json!(1);
    editor["targetRevision"] = json!(1);
    editor["target"]["displayName"] = json!("먼저 저장한 이름");
    assert_eq!(save(&Store::new(&dir.0), editor.clone()).unwrap(), SaveStatus::Saved);
    let original = fs::read(dir.file()).unwrap();

    editor["target"]["displayName"] = json!("나중에 보낸 이름");
    assert_eq!(save(&store, editor).unwrap(), SaveStatus::TargetConflict);
    assert_eq!(fs::read(dir.file()).unwrap(), original);
    let file = store.load().unwrap();
    assert_eq!(file.targets[0].revision.get(), 2);
    assert_eq!(file.targets[0].profile.display_name, "먼저 저장한 이름");
}

#[cfg(windows)]
#[test]
fn c01_failed_windows_replacement_preserves_previous_bytes() {
    let dir = TestDir::new();
    let store = Store::new(&dir.0);
    assert_eq!(save(&store, draft(&dir.0, "work", "first")).unwrap(), SaveStatus::Saved);
    let path = dir.file();
    let original = fs::read(&path).unwrap();
    let permissions = fs::metadata(&path).unwrap().permissions();
    let mut read_only = permissions.clone();
    read_only.set_readonly(true);
    fs::set_permissions(&path, read_only).unwrap();

    let result = save(&store, draft(&dir.0, "work", "second"));
    fs::set_permissions(&path, permissions).unwrap();
    assert!(matches!(result, Err(StoreError::Replace(_))), "{result:?}");
    assert_eq!(fs::read(&path).unwrap(), original);
    let file = Store::new(&dir.0).load().unwrap();
    assert_eq!(file.targets.len(), 1);
    assert_eq!(file.targets[0].profile.id.as_str(), "first");
    assert!(!dir.0.join("connections.json.tmp").exists());
}

#[cfg(windows)]
#[test]
fn c01_windows_replacement_preserves_custom_dacl() {
    use std::process::Command;

    let dir = TestDir::new();
    let store = Store::new(&dir.0);
    assert_eq!(save(&store, draft(&dir.0, "work", "first")).unwrap(), SaveStatus::Saved);
    let path = dir.file();
    let acl = |extra: &[&str]| {
        let output = Command::new("icacls.exe").arg(&path).args(extra).output().unwrap();
        assert!(output.status.success(), "{:?}", output);
        output.stdout
    };
    let inherited = acl(&[]);
    acl(&["/inheritance:d"]);
    let original = acl(&[]);
    assert_ne!(original, inherited);

    assert_eq!(save(&store, draft(&dir.0, "work", "second")).unwrap(), SaveStatus::Saved);
    assert_eq!(acl(&[]), original);
    assert_eq!(store.load().unwrap().targets.len(), 2);
}

#[test]
fn c01_unknown_and_corrupt_files_refuse_a_save() {
    let dir = TestDir::new();
    let store = Store::new(&dir.0);
    for bytes in [
        br#"{"futureField":{"new":true},"version":2}"#.as_slice(),
        br#"{"version":1,"futureField":[],"groups":[],"targets":[]}"#,
        b"{",
    ] {
        fs::write(dir.file(), bytes).unwrap();
        let result = save(&store, draft(&dir.0, "work", "first"));
        if bytes.starts_with(br#"{"futureField"#) {
            assert!(matches!(result, Err(StoreError::Version(2))), "{result:?}");
        } else {
            assert!(matches!(result, Err(StoreError::Format(_))), "{result:?}");
        }
        assert_eq!(fs::read(dir.file()).unwrap(), bytes);
    }
}

#[cfg(unix)]
#[test]
fn c01_unix_private_creation_preserves_existing_config_until_saved() {
    use std::os::unix::fs::PermissionsExt;

    let root = TestDir::new();
    let directory = root.0.join("private");
    let store = Store::new(&directory);
    assert_eq!(save(&store, draft(&root.0, "work", "first")).unwrap(), SaveStatus::Saved);
    let file = directory.join("connections.json");
    let lock = directory.join("connections.lock");
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&directory), 0o700);
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(&lock), 0o600);

    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    let original = fs::read(&file).unwrap();
    assert_eq!(store.load().unwrap().targets[0].profile.id.as_str(), "first");
    assert_eq!(fs::read(&file).unwrap(), original);
    assert_eq!(mode(&directory), 0o755);
    assert_eq!(mode(&file), 0o644);
    assert_eq!(save(&store, draft(&root.0, "work", "second")).unwrap(), SaveStatus::Saved);
    assert_eq!(store.load().unwrap().targets.len(), 2);
    assert_eq!(mode(&file), 0o600);
}

#[cfg(unix)]
#[test]
fn c01_unix_rejects_existing_and_dangling_links_without_touching_targets() {
    use std::{io::ErrorKind, os::unix::fs::symlink};

    let root = TestDir::new();
    let real = root.0.join("real");
    fs::create_dir(&real).unwrap();
    let real_file = real.join("connections.json");
    fs::write(&real_file, b"unrelated data").unwrap();
    let missing = root.0.join("missing");
    let config = root.0.join("config");
    let check = |directory: &Path| {
        let result = save(&Store::new(directory), draft(&root.0, "work", "first"));
        assert!(matches!(&result, Err(StoreError::Io(error)) if error.kind() == ErrorKind::PermissionDenied), "{result:?}");
        assert_eq!(fs::read(&real_file).unwrap(), b"unrelated data");
        assert!(!missing.exists());
    };

    for target in [&real, &missing] {
        symlink(target, &config).unwrap();
        check(&config.join("."));
        assert_eq!(fs::read_link(&config).unwrap(), *target);
        assert!(!real.join("connections.lock").exists());
        fs::remove_file(&config).unwrap();
    }
    fs::create_dir(&config).unwrap();
    for name in ["connections.json", "connections.lock", "connections.json.tmp"] {
        let path = config.join(name);
        if name == "connections.lock" {
            fs::remove_file(&path).unwrap();
        }
        for target in [&real_file, &missing] {
            symlink(target, &path).unwrap();
            check(&config);
            assert_eq!(fs::read_link(&path).unwrap(), *target);
            fs::remove_file(&path).unwrap();
        }
        assert!(!config.join("connections.json").exists());
    }
}

#[cfg(unix)]
#[test]
fn c01_failed_unix_replacement_preserves_previous_bytes() {
    let dir = TestDir::new();
    let store = Store::new(&dir.0);
    assert_eq!(save(&store, draft(&dir.0, "work", "first")).unwrap(), SaveStatus::Saved);
    let original = fs::read(dir.file()).unwrap();
    let edit: Draft = serde_json::from_value(draft(&dir.0, "work", "second")).unwrap();
    let result = store.save_with_failing_rename(edit.validate().unwrap());
    assert!(matches!(&result, Err(StoreError::Replace(error)) if error.raw_os_error() == Some(libc::ENOTDIR)), "{result:?}");
    assert_eq!(fs::read(dir.file()).unwrap(), original);
    let file = Store::new(&dir.0).load().unwrap();
    assert_eq!(file.targets.len(), 1);
    assert_eq!(file.targets[0].profile.id.as_str(), "first");
    assert!(!dir.0.join("connections.json.tmp").exists());
}
