use super::*;
use std::{cell::Cell, fs};
use super::super::public_key_file::MAX_KEY_BYTES;

const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti";
const SHA: &str = "SHA256:UCUiLr7Pjs9wFFJMDByLgc3NrtdU344OgUM45wZPcIQ";
const OTHER_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIC43GzF6fNOSizxOL5WTkKKSfu0OGcQymwLyXHRmq8Pm";
const OTHER_SHA: &str = "SHA256:k/KBYp42xBgn72aCjefPr1hQdnwjGXOW2zC2gDA9ehw";
const OWNER: &str = "main";

fn setup() -> (tempfile::TempDir, Registration) {
    let root = tempfile::tempdir().unwrap();
    let store = TrustStore::create(&root.path().join("pins")).unwrap();
    (root, Registration::new(store))
}

fn endpoint() -> Endpoint {
    Endpoint::new("dev.example.invalid", 2222).unwrap()
}

fn target(id: &str) -> Option<Endpoint> {
    (id == "dev").then(endpoint)
}

fn request() -> Request {
    Request {
        target_id: "dev".into(), host: "DEV.example.invalid".into(), port: 2222,
        expected_fingerprint: Some(SHA.into()),
    }
}

fn verify(
    state: &Registration,
    request: Request,
    resolve: impl Fn(&str) -> Option<Endpoint>,
    pick: impl FnOnce() -> Option<PathBuf>,
    confirm: impl FnOnce(&Endpoint, &Fingerprint) -> bool,
) -> Result<Prepared, Rejection> {
    let ticket = state.begin()?;
    let pending = Pending { state, ticket: &ticket };
    pending.verify(request, resolve, || Ok(pick()), |endpoint, sha| Ok(confirm(endpoint, sha)))
}

fn assert_no_pin(state: &Registration) {
    let endpoint = endpoint();
    let approved = ApprovedHost::confirm(&endpoint, &endpoint, SHA).unwrap();
    assert!(matches!(state.store.pin(&approved), Err(Error::UnregisteredHost)));
}

#[test]
fn checks_current_target_and_fingerprint_before_opening_the_picker() {
    let (_root, state) = setup();
    for (request, error) in [
        (Request { target_id: "unknown".into(), ..request() }, Rejection::InvalidTarget),
        (Request { host: "other.example.invalid".into(), ..request() }, Rejection::EndpointMismatch),
        (Request { port: 22, ..request() }, Rejection::EndpointMismatch),
        (Request { port: 0, ..request() }, Rejection::InvalidEndpoint),
        (Request { host: "host\nInjectedOption".into(), ..request() }, Rejection::InvalidEndpoint),
        (Request { expected_fingerprint: Some("SHA256:invalid".into()), ..request() }, Rejection::InvalidFingerprint),
    ] {
        assert_eq!(verify(&state, request, target, || panic!("picker"), |_, _| panic!("confirm")).err(), Some(error));
    }
    assert_no_pin(&state);
}

#[test]
fn rejects_non_public_input_and_fingerprint_mismatch_before_confirmation() {
    let (root, state) = setup();
    for (filename, data, error) in [
        ("host.txt", KEY.as_bytes().to_vec(), Rejection::InvalidPublicKey),
        ("secret.pub", b"-----BEGIN OPENSSH PRIVATE KEY-----\nnot a real secret\n-----END OPENSSH PRIVATE KEY-----".to_vec(), Rejection::InvalidPublicKey),
        ("binary.pub", vec![0xff], Rejection::InvalidPublicKey),
        ("oversized.pub", format!("{KEY} {}", "x".repeat(MAX_KEY_BYTES)).into_bytes(), Rejection::InvalidPublicKey),
        ("multiple.pub", format!("{KEY}\n{KEY}").into_bytes(), Rejection::InvalidPublicKey),
        ("wrong.pub", OTHER_KEY.as_bytes().to_vec(), Rejection::FingerprintMismatch),
    ] {
        let path = root.path().join(filename);
        fs::write(&path, data).unwrap();
        assert_eq!(verify(&state, request(), target, || Some(path), |_, _| panic!("confirm")).err(), Some(error));
    }
    let directory = root.path().join("directory.pub");
    fs::create_dir(&directory).unwrap();
    assert_eq!(read_public_key(&directory), Err(Rejection::InvalidPublicKey));
    assert_no_pin(&state);
}

#[cfg(unix)]
#[test]
fn rejects_symlinks_and_fifos_without_reading_them() {
    use std::{ffi::CString, os::unix::{ffi::OsStrExt, fs::symlink}};
    let (root, _) = setup();
    let public = root.path().join("host.pub");
    fs::write(&public, KEY).unwrap();
    let link = root.path().join("link.pub");
    symlink(public, &link).unwrap();
    assert_eq!(read_public_key(&link), Err(Rejection::InvalidPublicKey));
    let fifo = root.path().join("fifo.pub");
    let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: path는 임시 디렉터리 아래에 있으며 NUL로 끝난다.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    assert_eq!(read_public_key(&fifo), Err(Rejection::InvalidPublicKey));
}

#[test]
fn cancellation_navigation_and_changed_selection_cannot_commit_a_pin() {
    let (root, state) = setup();
    let path = root.path().join("host.pub");
    fs::write(&path, KEY).unwrap();
    let pick = || Some(path.clone());
    assert_eq!(verify(&state, request(), target, || None, |_, _| panic!("confirm")).err(), Some(Rejection::Cancelled));
    assert_eq!(verify(&state, request(), target, pick, |_, _| false).err(), Some(Rejection::Cancelled));
    assert_eq!(verify(&state, request(), target, || {
        state.document_changed(OWNER);
        assert_eq!(state.begin(), Err(Rejection::Busy));
        pick()
    }, |_, _| panic!("confirm")).err(), Some(Rejection::Cancelled));
    assert_eq!(verify(&state, request(), target, pick, |_, _| {
        state.document_changed(OWNER);
        true
    }).err(), Some(Rejection::Cancelled));
    let changed = Cell::new(false);
    assert_eq!(verify(&state, request(), |id| {
        if changed.get() { Endpoint::new("dev.example.invalid", 22).ok() } else { target(id) }
    }, pick, |_, _| {
        changed.set(true);
        true
    }).err(), Some(Rejection::EndpointMismatch));
    assert!(state.pending.lock().unwrap().is_none());
    assert_no_pin(&state);
}

#[test]
fn commits_exactly_the_confirmed_key_without_automatic_replacement() {
    let (root, state) = setup();
    let path = root.path().join("host.pub");
    fs::write(&path, format!("{KEY} administrator\r\n")).unwrap();
    let prepared = verify(&state, request(), target, || Some(path.clone()), |actual, fingerprint| {
        assert_eq!(actual, &endpoint());
        assert_eq!(fingerprint.to_string(), SHA);
        state.document_changed("another-webview");
        // 확인 뒤 파일을 다시 열면 다른 키가 읽힌다.
        fs::write(&path, OTHER_KEY).unwrap();
        true
    }).unwrap();
    assert_no_pin(&state);
    assert_eq!(prepared.commit(&state.store), Ok(()));
    let replacement = Request { expected_fingerprint: Some(OTHER_SHA.into()), ..request() };
    let replacement = verify(&state, replacement, target, || Some(path), |_, _| true).unwrap();
    assert_eq!(replacement.commit(&state.store), Err(Rejection::ExistingPinConflict));
    let endpoint = endpoint();
    let approved = ApprovedHost::confirm(&endpoint, &endpoint, SHA).unwrap();
    assert_eq!(state.store.pin(&approved).unwrap_or_else(|error| panic!("{error:?}")).0, format!("[dev.example.invalid]:2222 {KEY}\n"));
}

#[test]
fn saved_registration_rejects_a_profile_revision_changed_during_confirmation() {
    use crate::connections::{self, profile::Draft, tests::ssh_draft};
    use serde_json::json;

    let (root, state) = setup();
    let config = root.path().join("config");
    let mut input = ssh_draft(&config);
    input["target"]["ssh"]["host"] = json!("dev.example.invalid");
    input["target"]["ssh"]["hostKeyFingerprint"] = json!(SHA);
    let draft = |input| serde_json::from_value::<Draft>(input).unwrap().validate().unwrap();
    assert_eq!(connections::register(&config, draft(input.clone())).unwrap(), ("ssh-server".into(), "1".into()));
    let path = root.path().join("host.pub");
    fs::write(&path, KEY).unwrap();
    let sessions = Sessions::default();
    assert_eq!(state.register_saved(&config, "SSH-server".into(), &sessions, 0,
        || panic!("picker"), |_, _| panic!("confirm")), Err(Rejection::InvalidTarget));
    input["groupRevision"] = json!(1);
    input["targetRevision"] = json!(1);
    input["target"]["displayName"] = json!("새 이름");
    assert_eq!(state.register_saved(&config, "ssh-server".into(), &sessions, 0,
        || Ok(Some(path.clone())), |endpoint, fingerprint| {
            assert_eq!(endpoint.port, 2222);
            assert_eq!(fingerprint.to_string(), SHA);
            connections::save_test_profile(&config, draft(input));
            Ok(true)
        }), Err(Rejection::EndpointMismatch));
    assert_no_pin(&state);
    assert_eq!(state.register_saved(&config, "ssh-server".into(), &sessions, 0,
        || Ok(Some(path.clone())), |_, _| {
            sessions.navigate(OWNER);
            Ok(true)
        }), Err(Rejection::StaleDocument));
    assert_no_pin(&state);
    let profile = config.join("connections.json");
    let before = fs::read(&profile).unwrap();
    state.register_saved(&config, "ssh-server".into(), &sessions, sessions.document(OWNER),
        || Ok(Some(path.clone())), |_, _| Ok(true)).unwrap();
    assert_eq!(fs::read(profile).unwrap(), before);
    let endpoint = endpoint();
    let approved = ApprovedHost::confirm(&endpoint, &endpoint, SHA).unwrap();
    assert_eq!(state.store.pin(&approved).unwrap().0, format!("[dev.example.invalid]:2222 {KEY}\n"));
}

#[test]
fn saved_registration_acl_allows_only_the_local_main_webview() {
    use tauri::ipc::Origin;

    let mut context: tauri::Context<tauri::Wry> = tauri::generate_context!();
    let acl = context.runtime_authority_mut();
    let command = "register_saved_ssh_host";
    assert!(acl.resolve_access(command, "main", "main", &Origin::Local).is_some());
    assert!(acl.resolve_access(command, "main", "secondary", &Origin::Local).is_none());
    let remote = Origin::Remote { url: "https://remote.example.invalid".parse().unwrap() };
    assert!(acl.resolve_access(command, "main", "main", &remote).is_none());
}

#[test]
fn trust_saved_rejects_busy_unknown_targets_and_a_document_changed_during_confirmation() {
    use crate::{connections::{self, profile::Draft, tests::ssh_draft}, ssh::{TRUST_DIRECTORY, host_trust::Confirm}};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    let (root, state) = setup();
    let config = root.path().join("config");
    let mut input = ssh_draft(&config);
    input["target"]["ssh"]["host"] = json!("dev.example.invalid");
    input["target"]["ssh"].as_object_mut().unwrap().remove("hostKeyFingerprint");
    connections::register(&config, serde_json::from_value::<Draft>(input).unwrap().validate().unwrap()).unwrap();
    let pins = TrustStore::create(&config.join(TRUST_DIRECTORY)).unwrap();
    let sessions = Arc::new(Sessions::default());
    let scan = |_: &Path, host: &str, port: u16, _: &CancellationToken| Ok(format!("[{host}]:{port} {KEY}"));
    let refuse: Confirm = Arc::new(|_, _| panic!("확인 창을 열었습니다"));
    let trust_saved = |id: &str, document: u64, trust: &Confirm| {
        state.trust_saved(&config, id, &sessions, document, (trust, &refuse), &scan, crate::tunnels::ssm::open_ssh_forward)
    };

    assert_eq!(trust_saved("other", 0, &refuse), Err(Rejection::InvalidTarget.saved_message().into()));
    // 열려 있는 확인 창이 있으면 새 창을 띄우지 않는다.
    state.begin().unwrap();
    assert_eq!(trust_saved("ssh-server", 0, &refuse), Err(Rejection::Busy.saved_message().into()));
    state.pending.lock().unwrap().take();
    assert_eq!(trust_saved("ssh-server", 1, &refuse), Err(Rejection::StaleDocument.saved_message().into()));

    // 확인 창을 띄운 사이 페이지가 바뀌면 승인해도 핀을 쓰지 않는다.
    let navigated = Arc::clone(&sessions);
    let navigate: Confirm = Arc::new(move |_, _| { navigated.navigate(OWNER); Ok(true) });
    assert_eq!(trust_saved("ssh-server", 0, &navigate), Err(Rejection::StaleDocument.saved_message().into()));
    assert!(pins.pinned(&endpoint()).unwrap().is_none());
    assert!(state.pending.lock().unwrap().is_none());

    let document = sessions.document(OWNER);
    let accept: Confirm = Arc::new(|message, _| {
        assert!(message.contains("SHA256 지문: SHA256:UCUiLr7Pjs9wFFJMDByLgc3NrtdU344OgUM45wZPcIQ"), "{message}");
        Ok(true)
    });
    assert_eq!(trust_saved("ssh-server", document, &accept), Ok(Trusted { fingerprint: SHA.into(), status: "registered" }));
    assert_eq!(trust_saved("ssh-server", document, &refuse), Ok(Trusted { fingerprint: SHA.into(), status: "unchanged" }));
    assert!(pins.pinned(&endpoint()).unwrap().is_some());
}
