use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Barrier, atomic::{AtomicU64, Ordering}},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

use super::{profile::Draft, store::{SaveStatus, Store, StoreError}};

pub(super) struct TestDir(PathBuf);

impl TestDir {
    pub(super) fn new() -> Self {
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

    pub(super) fn path(&self) -> &Path {
        &self.0
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

pub(super) fn draft(dir: &Path, group: &str, target: &str) -> Value {
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

fn ssm_draft(dir: &Path) -> Value {
    let mut value = draft(dir, "work", "ssm-node");
    let target = value["target"].as_object_mut().unwrap();
    target.remove("local");
    target.insert("defaultMethod".into(), json!("ssm"));
    target.insert("ssm".into(), json!({
        "profile": "company-sso", "region": "ap-northeast-2", "instanceId": "i-0123456789abcdef0"
    }));
    value
}

#[test]
fn c02_local_v1_and_ssm_references_round_trip_in_one_file() {
    let dir = TestDir::new();
    let local = draft(dir.path(), "work", "local-shell");
    fs::write(dir.file(), serde_json::to_vec(&json!({
        "version": 1,
        "groups": [{"revision": 7, "profile": local["group"]}],
        "targets": [{"revision": 9, "profile": local["target"]}],
    })).unwrap()).unwrap();
    let mut ssm = ssm_draft(dir.path());
    ssm["groupRevision"] = json!(7);
    let store = Store::new(dir.path());
    assert_eq!(save(&store, ssm.clone()).unwrap(), SaveStatus::Saved);
    let stored = serde_json::to_value(store.load().unwrap()).unwrap();
    assert_eq!(stored, json!({
        "version": 1,
        "groups": [{"revision": 7, "profile": local["group"]}],
        "targets": [{"revision": 9, "profile": local["target"]}, {"revision": 1, "profile": ssm["target"]}],
    }));
    assert!(matches!(store.load().unwrap().targets[1].profile.method(),
        super::profile::MethodRef::Ssm { profile: "company-sso", region: "ap-northeast-2", instance_id: "i-0123456789abcdef0" }));
}

#[test]
fn c02_rejects_mixed_methods_forbidden_fields_and_invalid_references() {
    let dir = TestDir::new();
    let valid = ssm_draft(dir.path());
    for (path, key) in [("target", "local"), ("target", "password"), ("ssm", "accessToken"), ("ssm", "executable"), ("ssm", "args")] {
        let mut invalid = valid.clone();
        let object = if path == "target" { &mut invalid["target"] } else { &mut invalid["target"]["ssm"] };
        object[key] = json!("forbidden");
        assert!(serde_json::from_value::<Draft>(invalid).is_err(), "{path}.{key}");
    }
    let mut local = draft(dir.path(), "work", "local-shell");
    local["target"]["ssm"] = valid["target"]["ssm"].clone();
    assert!(serde_json::from_value::<Draft>(local).is_err());
    for (key, value) in [("profile", "--profile"), ("region", "-bad"), ("instanceId", "display-name")] {
        let mut invalid = valid.clone();
        invalid["target"]["ssm"][key] = json!(value);
        assert!(serde_json::from_value::<Draft>(invalid).unwrap().validate().is_err(), "{key}");
    }
}

pub(super) fn ssh_draft(dir: &Path) -> Value {
    let mut value = draft(dir, "work", "ssh-server");
    value["target"] = json!({
        "id": "ssh-server", "groupId": "work", "displayName": "개발 서버", "defaultMethod": "ssh",
        "ssh": {
            "host": "dev.example.test", "port": 2222, "account": "deploy",
            "hostKeyFingerprint": "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "transport": {"kind": "direct"}, "auth": {"kind": "manualPassword"}
        }
    });
    value
}

#[test]
fn f01_pin_registration_resolves_only_the_validated_draft_target() {
    let dir = TestDir::new();
    let input = ssh_draft(dir.path());
    let fingerprint = input["target"]["ssh"]["hostKeyFingerprint"].as_str().unwrap();
    let draft = serde_json::from_value::<Draft>(input.clone()).unwrap().validate().unwrap();
    assert_eq!(draft.ssh_pin_endpoint("ssh-server", fingerprint), Some(crate::ssh::Endpoint::new("dev.example.test", 2222).unwrap()));
    assert_eq!(draft.ssh_pin_endpoint("other-server", fingerprint), None);
    assert_eq!(draft.ssh_pin_endpoint("ssh-server", "SHA256:other"), None);
}

#[test]
fn f01_loads_existing_local_and_ssm_v1_with_ssh_reference() {
    let dir = TestDir::new();
    let local = draft(dir.path(), "work", "local-shell");
    let ssm = ssm_draft(dir.path());
    let mut file = json!({
        "version": 1,
        "groups": [{"revision": 7, "profile": local["group"]}],
        "targets": [{"revision": 8, "profile": local["target"]}, {"revision": 9, "profile": ssm["target"]}],
    });
    fs::write(dir.file(), serde_json::to_vec(&file).unwrap()).unwrap();
    let store = Store::new(dir.path());
    let mut ssh = ssh_draft(dir.path());
    ssh["groupRevision"] = json!(7);
    ssh["target"]["ssh"]["transport"] = ssm["target"]["ssm"].clone();
    ssh["target"]["ssh"]["transport"]["kind"] = json!("ssm");
    ssh["target"]["ssh"]["auth"] = json!({"kind": "agent", "publicKeyPath": dir.path().join("test.pub")});
    assert_eq!(save(&store, ssh.clone()).unwrap(), SaveStatus::Saved);
    file["targets"].as_array_mut().unwrap().push(json!({"revision": 1, "profile": ssh["target"]}));
    assert_eq!(serde_json::to_value(store.load().unwrap()).unwrap(), file);
}

#[test]
fn f01_rejects_ambiguous_ssh_methods_and_non_reference_values() {
    let dir = TestDir::new();
    let valid = ssh_draft(dir.path());
    let checked = |value| serde_json::from_value::<Draft>(value).unwrap().validate().is_ok();
    assert!(checked(valid.clone()));
    for (pointer, key) in [
        ("/target", "local"), ("/target", "ssm"), ("/target/ssh", "command"), ("/target/ssh", "args"),
        ("/target/ssh/transport", "profile"), ("/target/ssh/auth", "password"), ("/target/ssh/auth", "itemId"),
    ] {
        let mut input = valid.clone();
        input.pointer_mut(pointer).unwrap()[key] = json!("forbidden");
        assert!(serde_json::from_value::<Draft>(input).is_err(), "{pointer}/{key}");
    }
    for (key, value) in [
        ("host", json!("-oProxyCommand")), ("host", json!("dev.example.test\nProxyCommand bad")),
        ("account", json!("user@other")), ("port", json!(0)), ("hostKeyFingerprint", json!("SHA256:not-a-pin")),
        ("transport", json!({"kind": "ssm", "profile": "--profile", "region": "ap-northeast-2", "instanceId": "i-0123456789abcdef0"})),
        ("auth", json!({"kind": "storedPassword", "itemId": "--raw"})),
        ("auth", json!({"kind": "keyFile", "privateKeyPath": "relative"})),
    ] {
        let mut input = valid.clone();
        input["target"]["ssh"][key] = value;
        assert!(!checked(input), "{key}");
    }
    for auth in [json!({"kind": "storedPassword", "itemId": "vault-item"}),
        json!({"kind": "keyFile", "privateKeyPath": dir.path().join("private-key")})] {
        let mut input = valid.clone();
        input["target"]["ssh"]["auth"] = auth;
        assert!(checked(input));
    }
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
    #[cfg(windows)]
    {
        let mut verbatim = std::ffi::OsString::from(r"\\?\");
        verbatim.push(dirs[0].as_os_str());
        verbatim.push(r"\.");
        assert_eq!(save(&store, draft(Path::new(&verbatim), "first", "verbatim")).unwrap(), SaveStatus::Saved);
    }
    run(&root.0, &dirs[1]);
}

mod local_probe {
    use super::*;
    use crate::terminal::Sessions;
    use crate::connections::probe::{RunKey, SaveRequest, start, wait, cancel, save_confirmed};

    fn start_at(sessions: &Sessions, dir: &Path, version: u64) -> RunKey {
        start(sessions, "main", sessions.document("main"), "draft".into(), version,
            serde_json::from_value(draft(dir, "work", "first")).unwrap(), dir.to_owned()).unwrap()
    }

    fn save_at(sessions: &Sessions, owner: &str, run: &RunKey, input: Value, store: &Store) -> Result<(), String> {
        let request: SaveRequest = serde_json::from_value(json!({"run": run, "draft": input})).unwrap();
        save_confirmed(sessions, owner, sessions.document(owner), request, store)
    }

    #[test]
    fn c02_local_trial_cannot_authorize_an_ssm_profile() {
        let dir = TestDir::new();
        let store = Store::new(dir.path());
        let sessions = Sessions::default();
        let local_run = start_at(&sessions, dir.path(), 1);
        assert!(wait(&sessions, "main", 0, &local_run).is_ok());
        let mut input = ssm_draft(dir.path());
        input["target"]["id"] = json!("first");
        assert!(save_at(&sessions, "main", &local_run, input, &store).is_err());
        assert!(!dir.file().exists());
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
        assert_eq!(serde_json::to_value(&store.load().unwrap().targets[0].profile).unwrap(), input["target"]);
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
            serde_json::from_value(input.clone()).unwrap(), dir.0.clone()).unwrap();
        assert!(cancel(&sessions, "other", 0, &run).is_err());
        cancel(&sessions, "main", 0, &run).unwrap();
        assert!(wait(&sessions, "main", 0, &run).is_err());
        let run = start_at(&sessions, &dir.0, 2);
        wait(&sessions, "main", 0, &run).unwrap();
        sessions.navigate("main");
        assert!(wait(&sessions, "main", 0, &run).is_err());
        assert!(start(&sessions, "main", 0, "draft".into(), 3,
            serde_json::from_value(input.clone()).unwrap(), dir.0.clone()).is_err());
        assert!(save_at(&sessions, "main", &run, input.clone(), &store).is_err());
        sessions.destroy("main");
        assert!(start(&sessions, "main", sessions.document("main"), "draft".into(), 3,
            serde_json::from_value(input.clone()).unwrap(), dir.0.clone()).is_err());
        wait(&sessions, "other", 0, &other).unwrap();
        save_at(&sessions, "other", &other, input, &store).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn c01_probe_accepts_a_directory_ending_in_dot() {
        let dir = TestDir::new();
        let mut path = dir.0.as_os_str().to_owned();
        path.push(r"\.");
        let sessions = Sessions::default();
        let run = start_at(&sessions, Path::new(&path), 1);
        wait(&sessions, "main", 0, &run).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn c01_probe_rejects_nonstandard_windows_paths() {
        let dir = TestDir::new();
        let sessions = Sessions::default();
        let mut verbatim = std::ffi::OsString::from(r"\\?\");
        verbatim.push(dir.0.as_os_str());
        verbatim.push(r"\.");
        let expected = "작업 디렉터리: Windows 드라이브의 일반 절대 경로만 지원합니다";
        for path in [Path::new(&verbatim), Path::new(r"\\server\share\dir"), Path::new(r"\\.\C:\dir")] {
            let run = start_at(&sessions, path, 1);
            assert_eq!(wait(&sessions, "main", 0, &run).err().as_deref(), Some(expected));
        }
    }

    #[test]
    fn c01_probe_save_wait_keeps_pty_live_and_rejects_a_retired_document() {
        use std::{sync::mpsc, time::Duration};

        let dir = TestDir::new();
        let sessions = Sessions::default();
        let settings = dir.0.join("settings");
        let seed = Store::new(&settings);
        seed.load().unwrap();
        let lock = fs::File::options().read(true).write(true).open(settings.join("connections.lock")).unwrap();
        lock.lock().unwrap();
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let store = Store::new(&settings).observe_lock_wait(move || blocked_tx.send(()).unwrap());
        let input = draft(&dir.0, "work", "first");
        let run = start(&sessions, "profile", 0, "draft".into(), 1,
            serde_json::from_value(input.clone()).unwrap(), dir.0.clone()).unwrap();
        wait(&sessions, "profile", 0, &run).unwrap();
        let unrelated = start_at(&sessions, &dir.0, 1);
        thread::scope(|scope| {
            let saving = scope.spawn(|| save_at(&sessions, "profile", &run, input, &store));
            let blocked = blocked_rx.recv_timeout(Duration::from_secs(5));
            let (progress_tx, progress_rx) = mpsc::channel();
            if blocked.is_ok() {
                let sessions = &sessions;
                scope.spawn(move || {
                    crate::terminal::tests::assert_live_shell_progress(sessions);
                    cancel(sessions, "main", 0, &unrelated).unwrap();
                    sessions.navigate("profile");
                    let _ = progress_tx.send(());
                });
            }
            let progress = progress_rx.recv_timeout(Duration::from_secs(10));
            drop(lock);
            blocked.expect("저장 작업이 실제 설정 파일 잠금을 기다려야 합니다");
            progress.expect("설정 파일이 잠겨 있어도 PTY 입출력·취소·문서 이동이 끝나야 합니다");
            assert_eq!(saving.join().unwrap(), Err("문서가 바뀌었습니다".into()));
        });
        assert!(!settings.join("connections.json").exists());
        assert!(!settings.join("connections.json.tmp").exists());
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
        assert_eq!(serde_json::to_value(&target.profile).unwrap(), draft(&dir.0, group, id)["target"]);
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
