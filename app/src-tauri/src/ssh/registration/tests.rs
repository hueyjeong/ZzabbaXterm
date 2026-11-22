use super::*;
use std::{cell::Cell, fs};

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
        expected_fingerprint: SHA.into(),
    }
}

fn pending(state: &Registration) -> Pending<'_> {
    state.begin(OWNER).unwrap();
    Pending { state, owner: OWNER }
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
        (Request { expected_fingerprint: "SHA256:invalid".into(), ..request() }, Rejection::InvalidFingerprint),
    ] {
        assert_eq!(pending(&state).verify(request, target, || panic!("picker"), |_, _| panic!("confirm")), Err(error));
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
        assert_eq!(pending(&state).verify(request(), target, || Some(path), |_, _| panic!("confirm")), Err(error));
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
    assert_eq!(pending(&state).verify(request(), target, || None, |_, _| panic!("confirm")), Err(Rejection::Cancelled));
    assert_eq!(pending(&state).verify(request(), target, pick, |_, _| false), Err(Rejection::Cancelled));
    assert_eq!(pending(&state).verify(request(), target, || {
        state.document_changed(OWNER);
        assert_eq!(state.begin(OWNER), Err(Rejection::Busy));
        pick()
    }, |_, _| panic!("confirm")), Err(Rejection::StaleDocument));
    assert_eq!(pending(&state).verify(request(), target, pick, |_, _| {
        state.document_changed(OWNER);
        true
    }), Err(Rejection::StaleDocument));
    let changed = Cell::new(false);
    assert_eq!(pending(&state).verify(request(), |id| {
        if changed.get() { Endpoint::new("dev.example.invalid", 22).ok() } else { target(id) }
    }, pick, |_, _| {
        changed.set(true);
        true
    }), Err(Rejection::EndpointMismatch));
    assert_no_pin(&state);
}

#[test]
fn commits_exactly_the_confirmed_key_without_automatic_replacement() {
    let (root, state) = setup();
    let path = root.path().join("host.pub");
    fs::write(&path, format!("{KEY} administrator\r\n")).unwrap();
    let expected = VerifiedHost { host: request().host, port: 2222, fingerprint: SHA.into(), target_id: "dev".into() };
    let result = pending(&state).verify(request(), target, || Some(path.clone()), |actual, fingerprint| {
        assert_eq!(actual, &endpoint());
        assert_eq!(fingerprint.to_string(), SHA);
        state.document_changed("another-webview");
        // 확인 뒤 파일을 다시 열면 다른 키가 읽힌다.
        fs::write(&path, OTHER_KEY).unwrap();
        true
    });
    assert_eq!(result, Ok(expected));
    let replacement = Request { expected_fingerprint: OTHER_SHA.into(), ..request() };
    assert_eq!(pending(&state).verify(replacement, target, || Some(path), |_, _| true), Err(Rejection::ExistingPinConflict));
    let endpoint = endpoint();
    let approved = ApprovedHost::confirm(&endpoint, &endpoint, SHA).unwrap();
    assert_eq!(state.store.pin(&approved).unwrap_or_else(|error| panic!("{error:?}")).0, format!("[dev.example.invalid]:2222 {KEY}\n"));
}
