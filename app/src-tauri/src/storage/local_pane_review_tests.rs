use std::{fs, path::{Path, PathBuf}};

use aws_sdk_s3::primitives::SdkBody;

use super::{super::{super::*, *}, approve, list_request};
use crate::storage::tests::{client, replay, response};

fn select_request(session: &Session, root_id: &str, key: &str, path: &str, direction: Direction) -> SelectRequest {
    SelectRequest {
        target: Target { session_id: session.session_id.clone(), bucket: "selected-bucket".into(), region: session.region.clone(), key: key.into() },
        root_id: root_id.into(),
        path: path.into(),
        direction,
    }
}

type Prompts = std::sync::Arc<std::sync::Mutex<Vec<(&'static str, String)>>>;

/// 네이티브 확인 대신 제목·내용을 기록하고 정해 둔 답을 돌려준다.
fn answer(prompts: &Prompts, accept: bool) -> impl FnOnce(&'static str, String, watch::Receiver<bool>) -> std::future::Ready<bool> {
    let prompts = prompts.clone();
    move |dialog, prompt, _| {
        prompts.lock().unwrap().push((dialog, prompt));
        std::future::ready(accept)
    }
}

fn no_prompt(_: &'static str, prompt: String, _: watch::Receiver<bool>) -> std::future::Ready<bool> {
    panic!("예상하지 않은 확인: {prompt}")
}

fn release(state: &StorageIpc, document: &Document, session: &Session, result: SelectResult) {
    let SelectResult::Approved { approval_id } = result else { panic!("승인 아님: {result:?}") };
    assert!(state.cancel_approval(document, &ApprovalId { session_id: session.session_id.clone(), approval_id }));
}

#[test]
fn storage_local_names_reject_windows_devices_and_reserved_characters() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("inner")).unwrap();
    let root = LocalRoot::open(dir.path()).unwrap();
    for name in [
        "CON", "con", "Con.txt", "NUL.tar.gz", "aux ", "PRN", "COM1", "com9.log", "LPT0", "lpt3.txt", "CONIN$",
        "a<b", "a>b", "a\"b", "a|b", "a?b", "a*b", "a\u{1}b",
    ] {
        assert_eq!(valid_local_name(name), !cfg!(windows), "{name:?}");
        if cfg!(windows) {
            let listed = root.list(&format!("inner/{name}"), LIST_LIMIT).unwrap_err();
            assert_eq!(LocalFailure::from(listed), LocalFailure::InvalidPath, "{name:?}");
            let download = open_local(&root, "inner", &format!("원격/{name}"), Direction::Download);
            assert_eq!(LocalFailure::from(download.err().unwrap()), LocalFailure::InvalidPath, "{name:?}");
        }
    }
    for name in ["COM10", "CONSOLE", "console.txt", "LPTX", "nul1", "x.con", "한글 이름.txt"] {
        assert!(valid_local_name(name), "{name:?}");
    }
}

#[test]
fn storage_local_root_restriction_covers_system_credentials_home_and_drive_roots() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    let home = base.join("home");
    let system = base.join("system");
    for path in [home.join(".ssh"), home.join("문서"), system.join("sub"), base.join("other")] {
        fs::create_dir_all(path).unwrap();
    }
    let protected = vec![home.join(".ssh"), system.clone()];
    let check = |path: &Path| restricted(path, Some(&home), &protected);
    for path in [home.clone(), base.to_owned(), home.join(".ssh"), system.clone(), system.join("sub"), home.join(".ssh").join("없는 하위")] {
        assert!(check(&path), "{path:?}");
    }
    for path in [home.join("문서"), base.join("other")] {
        assert!(!check(&path), "{path:?}");
    }
    if cfg!(windows) {
        assert!(check(Path::new(&home.to_string_lossy().to_uppercase())));
    }
    // 비어 있거나 상대인 보호 경로는 모든 경로를 막지 않는다.
    assert!(!restricted(&base.join("other"), None, &[PathBuf::new(), "relative".into()]));
    let drive = base.ancestors().last().unwrap();
    assert!(restricted(drive, None, &[]));

    // 실제 환경 변수 기준이다.
    assert!(restricted_root(drive));
    let home = PathBuf::from(std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap());
    for path in [home.clone(), home.join(".ssh"), home.join(".config").join("app"), home.join(".local")] {
        assert!(restricted_root(&path), "{path:?}");
    }
    assert!(!restricted_root(&home.join("zzabba 작업")));
    #[cfg(windows)]
    {
        for name in ["windir", "ProgramFiles", "APPDATA", "LOCALAPPDATA", "PROGRAMDATA"] {
            let path = PathBuf::from(std::env::var_os(name).unwrap());
            assert!(restricted_root(&path), "{name}");
            assert!(restricted_root(&path.join("하위")), "{name}");
        }
        assert!(restricted_root(base), "임시 폴더는 LOCALAPPDATA 아래다");
    }
    #[cfg(unix)]
    for path in ["/", "/etc", "/etc/ssh", "/usr", "/usr/bin"] {
        assert!(restricted_root(Path::new(path)), "{path}");
    }
}

#[test]
fn storage_local_executable_names_are_case_insensitive() {
    for name in ["setup.exe", "RUN.BAT", "a.b.Ps1", "x.desktop", "install.sh", "바로 가기.lnk", "page.URL", "x.msi", "s.vbs", "s.JSE", "r.reg", "c.cmd"] {
        assert!(executable_name(name), "{name}");
    }
    for name in ["readme.txt", "exe", "archive.tar.gz", "photo.jpeg", "noext", "exe.txt"] {
        assert!(!executable_name(name), "{name}");
    }
}

#[tokio::test]
async fn storage_local_select_confirms_existing_object_executable_and_replacement() {
    let reply = replay([
        response(200, SdkBody::empty()),
        response(200, SdkBody::empty()),
        response(404, SdkBody::empty()),
        response(403, SdkBody::empty()),
    ]);
    let state = StorageIpc::default();
    let document = state.document("main");
    let session = state.insert(document.clone(), "fixture".into(), "eu-west-1".into(), client(&reply)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("올릴 파일.txt"), b"upload").unwrap();
    fs::write(dir.path().join("있는 파일.txt"), b"existing").unwrap();
    let root_id = approve(&state, &document, &session.session_id, dir.path());
    let prompts = Prompts::default();
    let upload = || select_request(&session, &root_id, "원격/올릴 파일.txt", "올릴 파일.txt", Direction::Upload);

    // 같은 키가 있으면 확인하고, 거절하면 취소한다.
    assert_eq!(state.select_local(&document, upload(), answer(&prompts, false)).await, SelectResult::Cancelled);
    let result = state.select_local(&document, upload(), answer(&prompts, true)).await;
    release(&state, &document, &session, result);
    assert_eq!(prompts.lock().unwrap().iter().map(|(dialog, _)| *dialog).collect::<Vec<_>>(), ["S3 객체 교체", "S3 객체 교체"]);
    // 없으면 확인 없이 승인한다.
    let result = state.select_local(&document, upload(), no_prompt).await;
    release(&state, &document, &session, result);
    // 존재 확인이 실패하면 업로드하지 않고 실제 오류를 돌려준다.
    let SelectResult::Failed { error } = state.select_local(&document, upload(), no_prompt).await else { panic!("오류 아님") };
    assert_eq!(error.stage, Stage::HeadObject);
    assert!(matches!(error.failure, crate::storage::Failure::Service { status: 403, .. }), "{error:?}");
    let requests: Vec<_> = reply.actual_requests().collect();
    assert_eq!(requests.len(), 4);
    assert!(requests.iter().all(|request| request.method() == "HEAD"));

    // 실행 파일 형식의 새 파일과 같은 이름의 기존 파일은 확인한다.
    prompts.lock().unwrap().clear();
    let download = |key: &str| select_request(&session, &root_id, key, "", Direction::Download);
    assert_eq!(state.select_local(&document, download("원격/setup.EXE"), answer(&prompts, false)).await, SelectResult::Cancelled);
    assert!(!dir.path().join("setup.EXE").exists());
    let result = state.select_local(&document, download("원격/install.sh"), answer(&prompts, true)).await;
    release(&state, &document, &session, result);
    assert_eq!(state.select_local(&document, download("원격/있는 파일.txt"), answer(&prompts, false)).await, SelectResult::Cancelled);
    let result = state.select_local(&document, download("원격/새 파일.txt"), no_prompt).await;
    release(&state, &document, &session, result);
    {
        let prompts = prompts.lock().unwrap();
        assert_eq!(prompts.iter().map(|(dialog, _)| *dialog).collect::<Vec<_>>(), ["다운로드 실행 파일", "다운로드 실행 파일", "다운로드 파일 교체"]);
        assert!(prompts[2].1.contains("있는 파일.txt"));
    }
    assert_eq!(fs::read(dir.path().join("있는 파일.txt")).unwrap(), b"existing");

    // 잘못된 이름과 경로는 원인을 돌려준다.
    let rejected = || SelectResult::Rejected { reason: LocalFailure::InvalidPath };
    assert_eq!(state.select_local(&document, download("원격/"), no_prompt).await, rejected());
    if cfg!(windows) {
        assert_eq!(state.select_local(&document, download("원격/CON.txt"), no_prompt).await, rejected());
    }
    let invalid = select_request(&session, &root_id, "원격/x", "../x", Direction::Upload);
    assert_eq!(state.select_local(&document, invalid, no_prompt).await, rejected());
    assert_eq!(serde_json::to_value(rejected()).unwrap(), serde_json::json!({ "kind": "rejected", "reason": { "kind": "invalidPath" } }));
    assert_eq!(reply.actual_requests().count(), 4);
}

#[tokio::test]
async fn storage_local_root_change_revokes_approval_and_blocks_dialog_during_selection() {
    let reply = replay([]);
    let state = StorageIpc::default();
    let document = state.document("main");
    let session = state.insert(document.clone(), "fixture".into(), "eu-west-1".into(), client(&reply)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root_path = dir.path().join("루트");
    let moved = dir.path().join("옮긴 루트");
    fs::create_dir(&root_path).unwrap();
    fs::write(root_path.join("a.txt"), b"a").unwrap();

    // 전송 승인 중에는 폴더 대화상자를 열지 않는다.
    let target = Target { session_id: session.session_id.clone(), bucket: "selected-bucket".into(), region: session.region.clone(), key: "k".into() };
    let pending = state.begin_selection(&document, target, Direction::Upload).unwrap();
    assert!(state.begin_local_root(&document, &session.session_id).is_none());
    assert!(matches!(state.finish_selection(&document, &session.session_id, pending, None), LocalFileApproval::Cancelled));

    let root_id = approve(&state, &document, &session.session_id, &root_path);
    fs::rename(&root_path, &moved).unwrap();
    assert_eq!(state.list_local(&document, list_request(&session.session_id, &root_id, "")).await.unwrap_err(), LocalFailure::RootChanged);
    assert!(state.local_root(&document, &session.session_id, &root_id).is_none());

    let root_id = approve(&state, &document, &session.session_id, &moved);
    fs::rename(&moved, &root_path).unwrap();
    let request = select_request(&session, &root_id, "원격/a.txt", "a.txt", Direction::Upload);
    assert_eq!(state.select_local(&document, request, no_prompt).await, SelectResult::Rejected { reason: LocalFailure::RootChanged });
    assert!(state.local_root(&document, &session.session_id, &root_id).is_none());
}
