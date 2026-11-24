use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Barrier, atomic::{AtomicU64, Ordering}},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

use super::{profile::Draft, store::{SaveStatus, Store, StoreError}};

pub(crate) struct TestDir(PathBuf);

impl TestDir {
    pub(crate) fn new() -> Self {
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

    pub(crate) fn path(&self) -> &Path {
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

pub(crate) fn draft(dir: &Path, group: &str, target: &str) -> Value {
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

pub(crate) fn ssm_draft(dir: &Path) -> Value {
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
        "version": 2,
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

pub(crate) fn ssh_draft(dir: &Path) -> Value {
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
    file["version"] = json!(2);
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

pub(crate) fn desktop_draft(dir: &Path, method: &str, settings: Value) -> Value {
    let mut value = draft(dir, "work", &format!("{method}-desktop"));
    value["target"] = json!({
        "id": format!("{method}-desktop"), "groupId": "work", "displayName": "원격 화면", "defaultMethod": method,
    });
    value["target"][method] = settings;
    value
}

#[test]
fn desktop_profiles_extend_existing_v1_files_without_migration() {
    let dir = TestDir::new();
    let local = draft(dir.path(), "work", "local-shell");
    let ssh = ssh_draft(dir.path());
    let mut file = json!({
        "version": 1,
        "groups": [{"revision": 3, "profile": local["group"]}],
        "targets": [{"revision": 4, "profile": local["target"]}, {"revision": 5, "profile": ssh["target"]}],
    });
    fs::write(dir.file(), serde_json::to_vec(&file).unwrap()).unwrap();
    let store = Store::new(dir.path());
    let ssm = json!({"kind": "ssm", "profile": "company-sso", "region": "ap-northeast-2", "instanceId": "i-0123456789abcdef0"});
    for input in [
        desktop_draft(dir.path(), "rdp", json!({"host": "win.example.test", "port": 3389, "tlsOnly": false, "route": {"kind": "direct"}})),
        desktop_draft(dir.path(), "vnc", json!({"host": "", "port": 5900, "route": ssm})),
    ] {
        let mut input = input;
        input["groupRevision"] = json!(3);
        assert_eq!(save(&store, input.clone()).unwrap(), SaveStatus::Saved);
        file["targets"].as_array_mut().unwrap().push(json!({"revision": 1, "profile": input["target"]}));
    }
    file["version"] = json!(2);
    assert_eq!(serde_json::to_value(store.load().unwrap()).unwrap(), file);
    assert!(matches!(store.load().unwrap().targets[3].profile.method(),
        super::profile::MethodRef::Desktop { route: super::profile::DesktopRoute::Ssm(_) }));
}

#[test]
fn desktop_profiles_reject_secrets_unsafe_hosts_and_plaintext_routes() {
    let dir = TestDir::new();
    let checked = |value: Value| serde_json::from_value::<Draft>(value).unwrap().validate().is_ok();
    let rdp = |host: &str, route: Value| desktop_draft(dir.path(), "rdp", json!({"host": host, "port": 3389, "tlsOnly": true, "route": route}));
    let vnc = |host: &str, route: Value| desktop_draft(dir.path(), "vnc", json!({"host": host, "port": 5900, "route": route}));
    let ssm = json!({"kind": "ssm", "profile": "company-sso", "region": "ap-northeast-2", "instanceId": "i-0123456789abcdef0"});
    let ssh = json!({"kind": "ssh", "targetId": "ssh-server"});
    let direct = json!({"kind": "direct"});
    for valid in [
        rdp("win.example.test", direct.clone()), rdp("xrdp.example.test", ssh.clone()), rdp("rdp.internal.example", ssm.clone()),
        vnc("vnc.example.test", direct.clone()), vnc("192.0.2.10", direct.clone()), vnc("", ssh.clone()), vnc("", ssm.clone()),
    ] {
        assert!(checked(valid.clone()), "{valid}");
    }
    for (pointer, key) in [("/target", "ssh"), ("/target/rdp", "password"), ("/target/rdp/route", "profile"), ("/target/rdp", "account")] {
        let mut input = rdp("win.example.test", direct.clone());
        input.pointer_mut(pointer).unwrap()[key] = json!("forbidden");
        assert!(serde_json::from_value::<Draft>(input).is_err(), "{pointer}/{key}");
    }
    let mut tls = vnc("vnc.example.test", direct.clone());
    tls["target"]["vnc"]["tlsOnly"] = json!(true);
    assert!(serde_json::from_value::<Draft>(tls).is_err());
    let mut bad_ssm = ssm.clone();
    bad_ssm["profile"] = json!("--profile");
    let mut zero = rdp("win.example.test", direct.clone());
    zero["target"]["rdp"]["port"] = json!(0);
    for invalid in [
        zero, rdp("", direct.clone()), rdp("10.0.1.25", direct.clone()), rdp("win.example.test
other", direct.clone()),
        rdp("127.0.0.1", ssh.clone()), rdp("a,b", ssm.clone()), rdp("host=other", ssm.clone()), rdp("host name", ssm.clone()),
        rdp("win.example.test", bad_ssm), rdp("win.example.test", json!({"kind": "ssh", "targetId": "--other"})),
        vnc("", direct.clone()), vnc("127.0.0.1", ssh.clone()), vnc("vnc.internal.example", ssm.clone()),
        // RDP는 SSM 경로에서도 인증서 이름이 필요하다. 숫자 최종 라벨은 리졸버가 IP로 해석한다.
        rdp("", ssm.clone()), rdp("10.0.1.25", ssm.clone()), rdp("2852039166", ssm.clone()), rdp("127.1", direct.clone()),
        rdp("0x7f000001", direct.clone()), vnc("999.1.1.1", direct.clone()),
    ] {
        assert!(!checked(invalid.clone()), "{invalid}");
    }
}

pub(crate) fn s3_draft(dir: &Path) -> Value {
    let mut value = draft(dir, "work", "s3-bucket");
    value["target"] = json!({
        "id": "s3-bucket", "groupId": "work", "displayName": "개발 버킷", "defaultMethod": "s3",
        "s3": {"profile": "company-sso", "region": "ap-northeast-2", "bucket": "dev.bucket", "prefix": " 폴더//한글/ "},
    });
    value
}

#[test]
fn s3_v1_round_trip_preserves_references_and_verified_account() {
    let dir = TestDir::new();
    let store = Store::new(dir.path());
    let local = draft(dir.path(), "work", "local-shell");
    assert_eq!(save(&store, local.clone()).unwrap(), SaveStatus::Saved);
    let ssm = ssm_draft(dir.path());
    assert_eq!(save(&store, ssm.clone()).unwrap(), SaveStatus::Saved);

    let s3 = s3_draft(dir.path());
    let validate = |value: Value| serde_json::from_value::<Draft>(value).unwrap().validate().unwrap();
    for account in [None, Some("12345678901x".into())] {
        assert!(matches!(store.save_authorized(validate(s3.clone()), account, || Ok(()), || {}), Err(StoreError::InvalidTestedAccount)));
    }
    assert_eq!(store.save_authorized(validate(s3.clone()), Some("012345678901".into()), || Ok(()), || {}).unwrap(), SaveStatus::Saved);
    super::with_saved_method(dir.path(), "s3-bucket", |method, account| {
        let super::profile::MethodRef::S3 { s3 } = method else { panic!("S3 프로필이 필요합니다"); };
        assert_eq!(s3.profile, "company-sso");
        assert_eq!(s3.region, "ap-northeast-2");
        assert_eq!(s3.bucket, "dev.bucket");
        assert_eq!(s3.prefix, " 폴더//한글/ ");
        assert_eq!(account, Some("012345678901"));
        Ok(())
    }).unwrap();
    super::with_ai_targets(dir.path(), |targets| {
        assert_eq!(targets.iter().map(|target| target.profile_id.as_str()).collect::<Vec<_>>(), ["local-shell", "ssm-node"]);
    }).unwrap();

    let mut file = serde_json::to_value(store.load().unwrap()).unwrap();
    assert_eq!(file, json!({
        "version": 2,
        "groups": [{"revision": 1, "profile": local["group"]}],
        "targets": [
            {"revision": 1, "profile": local["target"]},
            {"revision": 1, "profile": ssm["target"]},
            {"revision": 1, "profile": s3["target"], "tested_account_id": "012345678901"},
        ],
    }));
    file["targets"][0]["tested_account_id"] = json!("012345678901");
    fs::write(dir.file(), serde_json::to_vec(&file).unwrap()).unwrap();
    assert!(matches!(store.load(), Err(StoreError::InvalidTestedAccount)));
}

#[test]
fn s3_accepts_literal_utf8_prefix_and_rejects_invalid_or_non_reference_fields() {
    let dir = TestDir::new();
    let valid = s3_draft(dir.path());
    let checked = |value| serde_json::from_value::<Draft>(value).unwrap().validate().is_ok();
    for prefix in [String::new(), format!("{}x", "한".repeat(341))] {
        let mut input = valid.clone();
        input["target"]["s3"]["prefix"] = json!(prefix);
        assert!(checked(input));
    }
    for (key, value) in [
        ("profile", json!("--profile")), ("region", json!("-region")),
        ("bucket", json!("https://b.example")), ("bucket", json!("a..b")),
        ("bucket", json!("dev.-blocked")), ("bucket", json!("127.0.0.1")),
        ("bucket", json!("access-s3alias")), ("bucket", json!("xn--reserved")),
        ("prefix", json!("한".repeat(342))),
    ] {
        let mut input = valid.clone();
        input["target"]["s3"][key] = value;
        assert!(!checked(input), "{key}");
    }
    for (pointer, key) in [
        ("/target", "ssm"), ("/target", "testedAccountId"), ("/target/s3", "testedAccountId"),
        ("/target/s3", "accessKeyId"), ("/target/s3", "secretAccessKey"),
        ("/target/s3", "sessionToken"), ("/target/s3", "endpoint"),
    ] {
        let mut input = valid.clone();
        input.pointer_mut(pointer).unwrap()[key] = json!("forbidden");
        assert!(serde_json::from_value::<Draft>(input).is_err(), "{pointer}/{key}");
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
        assert!(result.session_id.as_deref().is_some_and(|id| !id.is_empty()));
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
        assert_eq!(target.profile.group_id.as_ref().unwrap().as_str(), group);
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
        br#"{"futureField":{"new":true},"version":3}"#.as_slice(),
        br#"{"version":1,"futureField":[],"groups":[],"targets":[]}"#,
        b"{",
    ] {
        fs::write(dir.file(), bytes).unwrap();
        let result = save(&store, draft(&dir.0, "work", "first"));
        if bytes.starts_with(br#"{"futureField"#) {
            assert!(matches!(result, Err(StoreError::Version(3))), "{result:?}");
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

mod schema_v2 {
    use super::*;
    use crate::connections::{profile::{GroupDraft, Revision, TargetDraft, ValidatedGroup, ValidatedTarget, ValidationError}, store::Expected};

    fn group(id: &str, parent: Option<&str>, revision: Option<u64>) -> ValidatedGroup {
        serde_json::from_value::<GroupDraft>(json!({
            "version": 2, "revision": revision, "group": {"id": id, "displayName": id, "parentId": parent},
        })).unwrap().validate().unwrap()
    }

    fn target(value: &Value, revision: Option<u64>) -> ValidatedTarget {
        serde_json::from_value::<TargetDraft>(json!({"version": 2, "targetRevision": revision, "target": value}))
            .unwrap().validate().unwrap()
    }

    fn local(dir: &Path, group: Option<&str>, id: &str) -> Value {
        let mut value = draft(dir, group.unwrap_or("unused"), id)["target"].clone();
        if group.is_none() {
            value.as_object_mut().unwrap().remove("groupId");
        }
        value
    }

    fn revision(value: u64) -> Revision {
        Revision::new(value).unwrap()
    }

    fn ok() -> Result<(), String> {
        Ok(())
    }

    fn save_target(store: &Store, value: &Value, revision: Option<u64>) -> Result<SaveStatus, StoreError> {
        store.save_validated(target(value, revision), |_, _| None, ok)
    }

    fn expected(entries: &[(&str, &str, u64)]) -> Vec<Expected> {
        serde_json::from_value(Value::Array(entries.iter().map(|(kind, id, revision)| {
            json!({"kind": kind, "id": id, "revision": revision})
        }).collect())).unwrap()
    }

    #[test]
    fn v1_file_loads_as_top_level_groups_and_first_write_keeps_a_v1_backup() {
        let dir = TestDir::new();
        let store = Store::new(dir.path());
        let backup = dir.path().join("connections.v1.json");
        let v1 = draft(dir.path(), "work", "local-shell");
        let original = serde_json::to_vec(&json!({
            "version": 1,
            "groups": [{"revision": 3, "profile": v1["group"]}],
            "targets": [{"revision": 4, "profile": v1["target"]}],
        })).unwrap();
        fs::write(dir.file(), &original).unwrap();

        let loaded = serde_json::to_value(store.load().unwrap()).unwrap();
        assert_eq!(loaded["version"], json!(2));
        assert_eq!(loaded["groups"][0]["profile"], json!({"id": "work", "displayName": "개발"}));
        assert!(!backup.exists());
        assert_eq!(fs::read(dir.file()).unwrap(), original);

        assert_eq!(store.save_group(group("child", Some("work"), None), ok).unwrap(), SaveStatus::Saved);
        assert_eq!(fs::read(&backup).unwrap(), original);
        let saved: Value = serde_json::from_slice(&fs::read(dir.file()).unwrap()).unwrap();
        assert_eq!(saved["version"], json!(2));
        assert_eq!(saved["groups"][1]["profile"], json!({"id": "child", "displayName": "child", "parentId": "work"}));

        // 이후 쓰기와 다시 나타난 v1 파일 모두 처음 백업을 덮어쓰지 않는다.
        assert_eq!(save_target(&store, &local(dir.path(), None, "top"), None).unwrap(), SaveStatus::Saved);
        let mut other = serde_json::from_slice::<Value>(&original).unwrap();
        other["groups"][0]["revision"] = json!(9);
        fs::write(dir.file(), serde_json::to_vec(&other).unwrap()).unwrap();
        assert_eq!(store.save_group(group("next", None, None), ok).unwrap(), SaveStatus::Saved);
        assert_eq!(fs::read(&backup).unwrap(), original);
        assert_eq!(store.load().unwrap().groups[0].revision.get(), 9);
    }

    #[test]
    fn group_parent_must_exist_and_cycles_and_depth_are_rejected() {
        let dir = TestDir::new();
        let store = Store::new(dir.path());
        let self_parent = json!({"version": 2, "revision": null, "group": {"id": "a", "displayName": "a", "parentId": "a"}});
        assert_eq!(serde_json::from_value::<GroupDraft>(self_parent).unwrap().validate().err(), Some(ValidationError::GroupParent));
        assert!(matches!(store.save_group(group("a", Some("missing"), None), ok), Err(StoreError::MissingGroup)));

        for depth in 1..=8 {
            let parent = (depth > 1).then(|| format!("g{}", depth - 1));
            assert_eq!(store.save_group(group(&format!("g{depth}"), parent.as_deref(), None), ok).unwrap(), SaveStatus::Saved);
        }
        assert!(matches!(store.save_group(group("g9", Some("g8"), None), ok), Err(StoreError::GroupDepth)));
        // 최상위 그룹을 다른 그룹 아래로 옮겨 9단계가 되는 이동도 거부한다.
        assert_eq!(store.save_group(group("top", None, None), ok).unwrap(), SaveStatus::Saved);
        let original = fs::read(dir.file()).unwrap();
        assert!(matches!(store.save_group(group("g1", Some("top"), Some(1)), ok), Err(StoreError::GroupDepth)));
        // 2단 순환: g1 → g2 → g1
        assert!(matches!(store.save_group(group("g1", Some("g2"), Some(1)), ok), Err(StoreError::GroupCycle)));
        assert_eq!(fs::read(dir.file()).unwrap(), original);

        // 순환이 있는 파일은 읽지 않는다.
        let mut file: Value = serde_json::from_slice(&original).unwrap();
        file["groups"][0]["profile"]["parentId"] = json!("g2");
        fs::write(dir.file(), serde_json::to_vec(&file).unwrap()).unwrap();
        assert!(matches!(store.load(), Err(StoreError::GroupCycle)));
    }

    #[test]
    fn group_save_detects_revision_conflict_renames_and_moves_with_parent_id() {
        let dir = TestDir::new();
        let store = Store::new(dir.path());
        let named = |name: &str, parent: Option<&str>, revision: Option<u64>| {
            serde_json::from_value::<GroupDraft>(json!({
                "version": 2, "revision": revision, "group": {"id": "a", "displayName": name, "parentId": parent},
            })).unwrap().validate().unwrap()
        };
        let created = named("처음", None, None);
        assert_eq!(created.next_revision(), Some(revision(1)));
        assert_eq!(store.save_group(created, ok).unwrap(), SaveStatus::Saved);
        assert_eq!(store.save_group(named("중복", None, None), ok).unwrap(), SaveStatus::GroupConflict);
        let renamed = named("바뀐 이름", None, Some(1));
        assert_eq!(renamed.next_revision(), Some(revision(2)));
        assert_eq!(store.save_group(renamed, ok).unwrap(), SaveStatus::Saved);
        assert_eq!(store.save_group(named("늦은 이름", None, Some(1)), ok).unwrap(), SaveStatus::GroupConflict);
        assert_eq!(store.save_group(group("b", None, None), ok).unwrap(), SaveStatus::Saved);
        assert_eq!(store.save_group(named("바뀐 이름", Some("b"), Some(2)), ok).unwrap(), SaveStatus::Saved);
        let file = store.load().unwrap();
        let moved = &file.groups[0];
        assert_eq!((moved.revision.get(), moved.profile.display_name.as_str()), (3, "바뀐 이름"));
        assert_eq!(moved.profile.parent_id.as_ref().map(|parent| parent.as_str()), Some("b"));
        assert!(matches!(store.save_group(group("b", Some("a"), Some(1)), ok), Err(StoreError::GroupCycle)));

        // 그룹을 함께 보내는 기존 초안은 상위 그룹을 바꾸지 않는다.
        let mut combined = draft(dir.path(), "a", "shell");
        combined["group"]["displayName"] = json!("바뀐 이름");
        combined["groupRevision"] = json!(3);
        assert_eq!(save(&store, combined).unwrap(), SaveStatus::Saved);
        let file = store.load().unwrap();
        assert_eq!(file.groups[0].revision.get(), 3);
        assert_eq!(file.groups[0].profile.parent_id.as_ref().map(|parent| parent.as_str()), Some("b"));
    }

    #[test]
    fn group_delete_cascades_only_with_the_exact_descendant_snapshot() {
        let dir = TestDir::new();
        let store = Store::new(dir.path());
        for (id, parent) in [("a", None), ("b", Some("a")), ("c", None)] {
            assert_eq!(store.save_group(group(id, parent, None), ok).unwrap(), SaveStatus::Saved);
        }
        for (group, id) in [(Some("a"), "t1"), (Some("b"), "t2"), (Some("c"), "t3"), (None, "t4")] {
            assert_eq!(save_target(&store, &local(dir.path(), group, id), None).unwrap(), SaveStatus::Saved);
        }
        let snapshot = [("group", "b", 1), ("target", "t1", 1), ("target", "t2", 1)];
        let original = fs::read(dir.file()).unwrap();
        for (root, seen) in [
            (1, &snapshot[..2]),
            (1, &[snapshot[0], snapshot[1], snapshot[2], ("target", "t3", 1)][..]),
            (1, &[snapshot[0], snapshot[1], ("target", "t2", 2)][..]),
            (1, &[snapshot[0], snapshot[1], snapshot[2], snapshot[2]][..]),
            (2, &snapshot[..]),
        ] {
            assert_eq!(store.delete_group("a", revision(root), expected(seen), ok).unwrap(), SaveStatus::GroupConflict, "{seen:?}");
        }
        assert_eq!(store.delete_group("missing", revision(1), Vec::new(), ok).unwrap(), SaveStatus::GroupConflict);
        assert_eq!(fs::read(dir.file()).unwrap(), original);

        // 클라이언트가 본 뒤에 하위 세션이 추가되면 지우지 않는다.
        assert_eq!(save_target(&store, &local(dir.path(), Some("b"), "t5"), None).unwrap(), SaveStatus::Saved);
        assert_eq!(store.delete_group("a", revision(1), expected(&snapshot), ok).unwrap(), SaveStatus::GroupConflict);
        let mut seen = snapshot.to_vec();
        seen.push(("target", "t5", 1));
        assert_eq!(store.delete_group("a", revision(1), expected(&seen), ok).unwrap(), SaveStatus::Saved);
        let file = store.load().unwrap();
        assert_eq!(file.groups.iter().map(|entry| entry.profile.id.as_str()).collect::<Vec<_>>(), ["c"]);
        assert_eq!(file.targets.iter().map(|entry| entry.profile.id.as_str()).collect::<Vec<_>>(), ["t3", "t4"]);
    }

    #[test]
    fn target_delete_checks_revision_and_rejects_a_desktop_ssh_route_reference() {
        let dir = TestDir::new();
        let store = Store::new(dir.path());
        assert_eq!(store.save_group(group("work", None, None), ok).unwrap(), SaveStatus::Saved);
        let mut ssh = ssh_draft(dir.path())["target"].clone();
        ssh["ssh"]["auth"] = json!({"kind": "keyFile", "privateKeyPath": dir.path().join("private-key")});
        assert_eq!(save_target(&store, &ssh, None).unwrap(), SaveStatus::Saved);
        let rdp = desktop_draft(dir.path(), "rdp", json!({
            "host": "xrdp.example.test", "port": 3389, "tlsOnly": false, "route": {"kind": "ssh", "targetId": "ssh-server"},
        }))["target"].clone();
        assert_eq!(save_target(&store, &rdp, None).unwrap(), SaveStatus::Saved);
        let original = fs::read(dir.file()).unwrap();

        assert_eq!(store.delete_target("ssh-server", revision(2), ok).unwrap(), SaveStatus::TargetConflict);
        assert_eq!(store.delete_target("missing", revision(1), ok).unwrap(), SaveStatus::TargetConflict);
        let referenced = SaveStatus::Referenced(vec!["rdp-desktop".into()]);
        assert_eq!(store.delete_target("ssh-server", revision(1), ok).unwrap(), referenced);
        // SSH 세션을 수동 암호 인증으로 바꾸는 저장도 경로를 끊으므로 거부한다.
        let mut manual = ssh.clone();
        manual["ssh"]["auth"] = json!({"kind": "manualPassword"});
        assert_eq!(save_target(&store, &manual, Some(1)).unwrap(), referenced);
        let mut broken = rdp.clone();
        broken["rdp"]["route"]["targetId"] = json!("missing");
        assert!(matches!(save_target(&store, &broken, Some(1)), Err(StoreError::DesktopRoute)));
        assert!(matches!(store.delete_target("rdp-desktop", revision(1), || Err("문서가 바뀌었습니다".into())),
            Err(StoreError::Authorization(_))));
        assert_eq!(fs::read(dir.file()).unwrap(), original);

        assert_eq!(store.delete_target("rdp-desktop", revision(1), ok).unwrap(), SaveStatus::Saved);
        assert_eq!(store.delete_target("ssh-server", revision(1), ok).unwrap(), SaveStatus::Saved);
        assert!(store.load().unwrap().targets.is_empty());
    }

    #[test]
    fn validated_save_records_configured_sso_account_and_keeps_it_when_scope_is_unchanged() {
        let dir = TestDir::new();
        let store = Store::new(dir.path());
        assert_eq!(store.save_group(group("work", None, None), ok).unwrap(), SaveStatus::Saved);
        let account = || store.load().unwrap().targets[0].tested_account_id.clone();
        let calls = std::cell::RefCell::new(Vec::new());
        let configured = |value: &'static str| {
            let calls = &calls;
            move |profile: &str, region: &str| {
                calls.borrow_mut().push(format!("{profile}/{region}"));
                Some(value.to_owned())
            }
        };
        let mut s3 = s3_draft(dir.path())["target"].clone();
        assert_eq!(store.save_validated(target(&s3, None), configured("012345678901"), ok).unwrap(), SaveStatus::Saved);
        assert_eq!(account().as_deref(), Some("012345678901"));
        s3["s3"]["bucket"] = json!("other.bucket");
        assert_eq!(store.save_validated(target(&s3, Some(1)), configured("999999999999"), ok).unwrap(), SaveStatus::Saved);
        assert_eq!(account().as_deref(), Some("012345678901"));
        s3["s3"]["region"] = json!("us-east-1");
        assert_eq!(store.save_validated(target(&s3, Some(2)), configured("999999999999"), ok).unwrap(), SaveStatus::Saved);
        assert_eq!(account().as_deref(), Some("999999999999"));
        assert_eq!(*calls.borrow(), ["company-sso/ap-northeast-2", "company-sso/us-east-1"]);
        assert_eq!(store.save_validated(target(&s3, Some(1)), configured("012345678901"), ok).unwrap(), SaveStatus::TargetConflict);

        // 프로필과 리전이 같아도 방식이 바뀌면 계정을 다시 정한다.
        let mut ssm = ssm_draft(dir.path())["target"].clone();
        ssm["id"] = json!("s3-bucket");
        ssm["ssm"]["region"] = json!("us-east-1");
        assert_eq!(store.save_validated(target(&ssm, Some(3)), configured("111111111111"), ok).unwrap(), SaveStatus::Saved);
        assert_eq!(account().as_deref(), Some("111111111111"));
    }

    #[test]
    fn validated_save_without_sso_config_leaves_account_empty() {
        let dir = TestDir::new();
        let store = Store::new(dir.path());
        let mut ssm = ssm_draft(dir.path())["target"].clone();
        ssm.as_object_mut().unwrap().remove("groupId");
        assert_eq!(store.save_validated(target(&ssm, None), |_, _| None, ok).unwrap(), SaveStatus::Saved);
        let mut s3 = s3_draft(dir.path())["target"].clone();
        s3.as_object_mut().unwrap().remove("groupId");
        assert_eq!(store.save_validated(target(&s3, None), |_, _| Some("not-an-account".into()), ok).unwrap(), SaveStatus::Saved);
        let file = store.load().unwrap();
        assert!(file.targets.iter().all(|entry| entry.tested_account_id.is_none() && entry.profile.group_id.is_none()));
        crate::connections::with_saved_method(dir.path(), "s3-bucket", |_, account| {
            assert_eq!(account, None);
            Ok(())
        }).unwrap();

        // 없는 그룹을 가리키거나 v1 초안이면 저장하지 않는다.
        let original = fs::read(dir.file()).unwrap();
        assert!(matches!(save_target(&store, &local(dir.path(), Some("missing"), "lost"), None), Err(StoreError::MissingGroup)));
        let v1 = json!({"version": 1, "targetRevision": null, "target": local(dir.path(), None, "old")});
        assert_eq!(serde_json::from_value::<TargetDraft>(v1).unwrap().validate().err(), Some(ValidationError::Version(1)));
        let mut port = ssh_draft(dir.path())["target"].clone();
        port["ssh"]["port"] = json!(0);
        let port = json!({"version": 2, "targetRevision": null, "target": port});
        assert_eq!(serde_json::from_value::<TargetDraft>(port).unwrap().validate().err().map(|error| error.field()), Some("ssh.port"));
        assert_eq!(fs::read(dir.file()).unwrap(), original);
    }

    #[test]
    fn combined_drafts_accept_v1_and_v2_and_require_the_target_in_the_group() {
        let dir = TestDir::new();
        let mut input = draft(dir.path(), "work", "shell");
        assert!(serde_json::from_value::<Draft>(input.clone()).unwrap().validate().is_ok());
        input["version"] = json!(2);
        input["group"]["parentId"] = json!("parent");
        assert!(serde_json::from_value::<Draft>(input.clone()).unwrap().validate().is_ok());
        input["version"] = json!(3);
        assert_eq!(serde_json::from_value::<Draft>(input.clone()).unwrap().validate().err(), Some(ValidationError::Version(3)));
        input["version"] = json!(2);
        input["target"].as_object_mut().unwrap().remove("groupId");
        assert_eq!(serde_json::from_value::<Draft>(input).unwrap().validate().err(), Some(ValidationError::GroupMismatch));
    }
}
