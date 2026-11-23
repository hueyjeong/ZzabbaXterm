use std::{fs, path::Path};

use aws_sdk_s3::primitives::SdkBody;

use super::{super::*, *};
use crate::editor::storage_files::{LocalKind, local_names};
use crate::storage::tests::{client, object_response, replay, response};

fn approve(state: &StorageIpc, document: &Document, session_id: &str, root: &Path) -> String {
    let closed = state.begin_local_root(document, session_id).unwrap();
    assert!(state.begin_local_root(document, session_id).is_none(), "대화상자는 탭마다 하나만 연다");
    match state.finish_local_root(document, session_id, &closed, Some(LocalRoot::open(root))) {
        RootApproval::Approved { root_id, .. } => root_id,
        other => panic!("루트 승인 실패: {other:?}"),
    }
}

fn list_request(session_id: &str, root_id: &str, path: &str) -> ListRequest {
    ListRequest { session_id: session_id.into(), root_id: root_id.into(), path: path.into() }
}

/// 디렉터리를 가리키는 링크를 만든다. Windows는 권한 없이 만들 수 있는 junction을 쓴다.
fn link_directory(target: &Path, link: &Path) {
    #[cfg(windows)]
    {
        let status = std::process::Command::new("cmd").arg("/C").arg("mklink").arg("/J").arg(link).arg(target)
            .stdout(std::process::Stdio::null()).status().unwrap();
        assert!(status.success(), "junction 생성 실패");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[test]
fn storage_local_path_rejects_parent_absolute_drive_unc_nul_and_streams() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("inner")).unwrap();
    fs::write(dir.path().join("inner").join("a.txt"), b"a").unwrap();
    let root = LocalRoot::open(dir.path()).unwrap();
    for path in [
        "..", "../x", "inner/..", "inner/../inner", ".", "./inner", "/", "/etc", "/inner", "inner/", "inner//a.txt",
        "C:", "C:/Windows", "C:\\Windows", "\\\\server\\share", "//server/share", "inner\\a.txt",
        "inner/a.txt:stream", "a.txt::$DATA", "inner\0", "inner/a.txt\0",
    ] {
        assert!(local_names(path).is_err(), "{path:?}");
        assert_eq!(LocalFailure::from(root.list(path, LIST_LIMIT).unwrap_err()), LocalFailure::InvalidPath, "{path:?}");
        assert!(root.open_upload(path).is_err(), "{path:?}");
        assert!(root.download_target(path, "x.txt").is_err(), "{path:?}");
    }
    for name in ["..", "a/b", "a\\b", "C:", "x:stream", "", "\0"] {
        assert!(root.download_target("inner", name).is_err(), "{name:?}");
    }
    assert_eq!(local_names("").unwrap(), Vec::<&str>::new());
    assert_eq!(local_names("inner/a.txt").unwrap(), ["inner", "a.txt"]);
    let (entries, truncated) = root.list("inner", LIST_LIMIT).unwrap();
    assert!(!truncated);
    assert_eq!(entries.len(), 1);
    assert_eq!((entries[0].name.as_str(), entries[0].kind, entries[0].bytes), ("a.txt", LocalKind::File, Some(1)));
    assert!(root.open_upload("inner/a.txt").is_ok());
    assert!(root.open_upload("inner").is_err());
    assert!(root.list("inner/a.txt", LIST_LIMIT).is_err());
    if cfg!(windows) {
        // 대소문자·짧은 이름 별칭은 정규화 결과가 달라 거부한다.
        assert_eq!(LocalFailure::from(root.list("INNER", LIST_LIMIT).unwrap_err()), LocalFailure::Link);
    }
}

#[test]
fn storage_local_list_marks_truncation_at_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    for index in 0..5 { fs::write(dir.path().join(format!("{index}.txt")), b"").unwrap(); }
    let root = LocalRoot::open(dir.path()).unwrap();
    let (entries, truncated) = root.list("", 3).unwrap();
    assert_eq!((entries.len(), truncated), (3, true));
    let (entries, truncated) = root.list("", 5).unwrap();
    assert_eq!((entries.len(), truncated), (5, false));
}

#[test]
fn storage_local_links_are_listed_but_never_entered_or_transferred() {
    let dir = tempfile::tempdir().unwrap();
    let root_path = dir.path().join("root");
    let outside = dir.path().join("outside");
    fs::create_dir_all(root_path.join("inner")).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret.txt"), b"secret").unwrap();
    link_directory(&outside, &root_path.join("escape"));
    link_directory(&root_path.join("inner"), &root_path.join("alias"));
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.join("secret.txt"), root_path.join("secret-link.txt")).unwrap();

    let root = LocalRoot::open(&root_path).unwrap();
    let (entries, _) = root.list("", LIST_LIMIT).unwrap();
    let kind = |name: &str| entries.iter().find(|entry| entry.name == name).map(|entry| entry.kind);
    assert_eq!(kind("escape"), Some(LocalKind::Link));
    assert_eq!(kind("alias"), Some(LocalKind::Link));
    assert_eq!(kind("inner"), Some(LocalKind::Folder));
    for link in ["escape", "alias"] {
        assert_eq!(LocalFailure::from(root.list(link, LIST_LIMIT).unwrap_err()), LocalFailure::Link, "{link}");
        assert!(root.open_upload(&format!("{link}/secret.txt")).is_err(), "{link}");
        assert!(root.download_target(link, "new.txt").is_err(), "{link}");
    }
    assert!(root.open_upload("escape").is_err());
    assert!(root.download_target("", "escape").is_err());
    #[cfg(unix)]
    {
        assert_eq!(kind("secret-link.txt"), Some(LocalKind::Link));
        assert!(root.open_upload("secret-link.txt").is_err());
        assert!(root.download_target("", "secret-link.txt").is_err());
    }
    assert!(!outside.join("new.txt").exists());
    assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), b"secret");

    // 링크 자체를 루트로 고르면 정규화한 실제 폴더를 승인한다.
    let linked = LocalRoot::open(&root_path.join("escape")).unwrap();
    assert_eq!(linked.path(), fs::canonicalize(&outside).unwrap());
}

#[tokio::test]
async fn storage_local_approval_is_revoked_by_close_navigate_and_shutdown() {
    let reply = replay([]);
    let state = StorageIpc::default();
    let document = state.document("main");
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), b"a").unwrap();

    let session = state.insert(document.clone(), "first".into(), "eu-west-1".into(), client(&reply)).unwrap();
    let root_id = approve(&state, &document, &session.session_id, dir.path());
    let listed = state.list_local(&document, list_request(&session.session_id, &root_id, "")).await.unwrap();
    assert_eq!(listed.entries.len(), 1);
    assert!(state.list_local(&document, list_request(&session.session_id, "other-root", "")).await.is_err());
    assert!(state.list_local(&state.document("other"), list_request(&session.session_id, &root_id, "")).await.is_err());
    // 새 루트를 승인하면 이전 루트 ID는 쓸 수 없다.
    let replaced = approve(&state, &document, &session.session_id, dir.path());
    assert_eq!(state.list_local(&document, list_request(&session.session_id, &root_id, "")).await.unwrap_err(), LocalFailure::Denied);
    assert!(state.close(&document, &session.session_id));
    assert_eq!(state.list_local(&document, list_request(&session.session_id, &replaced, "")).await.unwrap_err(), LocalFailure::Denied);
    assert!(state.begin_local_root(&document, &session.session_id).is_none());

    let session = state.insert(document.clone(), "second".into(), "eu-west-1".into(), client(&reply)).unwrap();
    let root_id = approve(&state, &document, &session.session_id, dir.path());
    state.navigate("main");
    let document = state.document("main");
    assert!(state.local_root(&document, &session.session_id, &root_id).is_none());

    let session = state.insert(document.clone(), "third".into(), "eu-west-1".into(), client(&reply)).unwrap();
    let closed = state.begin_local_root(&document, &session.session_id).unwrap();
    assert!(state.close(&document, &session.session_id));
    // 대화상자가 열린 동안 탭이 닫히면 고른 폴더도 승인하지 않는다.
    assert!(matches!(state.finish_local_root(&document, &session.session_id, &closed, Some(LocalRoot::open(dir.path()))), RootApproval::Denied));

    let session = state.insert(document.clone(), "fourth".into(), "eu-west-1".into(), client(&reply)).unwrap();
    let root_id = approve(&state, &document, &session.session_id, dir.path());
    state.shutdown();
    assert!(state.local_root(&document, &session.session_id, &root_id).is_none());
}

#[tokio::test]
async fn storage_local_download_creates_new_file_and_keeps_existing_when_replacement_declined() {
    let reply = replay([
        object_response(6, None, SdkBody::from("object")),
        response(200, SdkBody::empty()),
    ]);
    let state = StorageIpc::default();
    let document = state.document("main");
    let session = state.insert(document.clone(), "fixture".into(), "eu-west-1".into(), client(&reply)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("받는 폴더")).unwrap();
    let root_id = approve(&state, &document, &session.session_id, dir.path());
    let target = || Target {
        session_id: session.session_id.clone(),
        bucket: "selected-bucket".into(),
        region: session.region.clone(),
        key: "원격//폴더/받을 파일 +%.txt".into(),
    };
    let approve_transfer = |file: Option<std::io::Result<ApprovedFile>>| {
        let id = state.begin_selection(&document, target(), Direction::Download).unwrap();
        state.finish_selection(&document, &session.session_id, id, file)
    };
    let root = state.local_root(&document, &session.session_id, &root_id).unwrap();
    let destination = dir.path().join("받는 폴더").join("받을 파일 +%.txt");

    let ApprovedFile::Download(new_file) = open_local(&root, "받는 폴더", &target().key, Direction::Download).unwrap() else { panic!() };
    assert!(!new_file.replaces_existing());
    let LocalFileApproval::Approved { approval_id } = approve_transfer(Some(Ok(ApprovedFile::Download(new_file)))) else { panic!() };
    let downloaded = state.download(&document, ApprovedTransfer { target: target(), approval_id }).await.unwrap();
    assert_eq!(downloaded.bytes, 6);
    assert_eq!(fs::read(&destination).unwrap(), b"object");

    // 같은 이름이 있으면 교체 확인이 필요하고, 거절하면 승인 없이 기존 파일을 유지한다.
    let ApprovedFile::Download(existing) = open_local(&root, "받는 폴더", &target().key, Direction::Download).unwrap() else { panic!() };
    assert!(existing.replaces_existing());
    drop(existing);
    assert!(matches!(approve_transfer(None), LocalFileApproval::Cancelled));
    assert!(state.download(&document, ApprovedTransfer { target: target(), approval_id: "declined".into() }).await.is_err());
    assert_eq!(fs::read(&destination).unwrap(), b"object");
    assert_eq!(fs::read_dir(dir.path().join("받는 폴더")).unwrap().count(), 1);

    // 폴더로 끝나는 키는 다운로드 대상이 아니다.
    assert!(open_local(&root, "받는 폴더", "원격/", Direction::Download).is_err());

    // 업로드는 루트 안의 파일을 열어 기존 업로드 경로로 보낸다.
    let upload = || Target { key: "원격/올림.txt".into(), ..target() };
    let file = open_local(&root, "받는 폴더/받을 파일 +%.txt", &upload().key, Direction::Upload);
    let id = state.begin_selection(&document, upload(), Direction::Upload).unwrap();
    let LocalFileApproval::Approved { approval_id } = state.finish_selection(&document, &session.session_id, id, Some(file)) else { panic!() };
    assert_eq!(state.upload(&document, ApprovedTransfer { target: upload(), approval_id }).await, Ok(6));
    assert_eq!(reply.actual_requests().count(), 2);
}

#[test]
fn storage_local_list_ipc_returns_root_relative_entries() {
    use serde_json::json;
    use tauri::{ipc::{CallbackFn, InvokeBody}, test, utils::acl::ExecutionContext, webview::InvokeRequest};

    let mut context = test::mock_context(test::noop_assets());
    context.runtime_authority_mut().__allow_command("storage_local_list".into(), ExecutionContext::Local);
    let app = test::mock_builder().manage(StorageIpc::default()).invoke_handler(dispatch).build(context).unwrap();
    let window = tauri::WebviewWindowBuilder::new(&app, "main", Default::default()).build().unwrap();
    let state = app.state::<StorageIpc>();
    let document = state.document("main");
    let reply = replay([]);
    let session = state.insert(document.clone(), "fixture".into(), "eu-west-1".into(), client(&reply)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("하위 폴더")).unwrap();
    fs::write(dir.path().join("하위 폴더").join("한글.txt"), b"abc").unwrap();
    let root_id = approve(&state, &document, &session.session_id, dir.path());
    let list = |body| test::get_ipc_response(&window, InvokeRequest {
        cmd: "storage_local_list".into(), callback: CallbackFn(0), error: CallbackFn(1),
        url: if cfg!(windows) { "http://tauri.localhost" } else { "tauri://localhost" }.parse().unwrap(),
        body: InvokeBody::Json(json!({ "request": body })), headers: Default::default(),
        invoke_key: test::INVOKE_KEY.into(),
    });
    let value = list(json!({ "sessionId": session.session_id, "rootId": root_id, "path": "하위 폴더" }))
        .unwrap().deserialize::<serde_json::Value>().unwrap();
    assert_eq!(value["ok"], json!(true));
    assert_eq!(value["value"]["truncated"], json!(false));
    let entry = &value["value"]["entries"][0];
    assert_eq!((&entry["name"], &entry["kind"], &entry["bytes"]), (&json!("한글.txt"), &json!("file"), &json!(3)));
    assert!(entry["modified"].is_u64());
    let denied = list(json!({ "sessionId": session.session_id, "rootId": "other", "path": "" }))
        .unwrap().deserialize::<serde_json::Value>().unwrap();
    assert_eq!(denied, json!({ "ok": false, "error": { "kind": "denied" } }));
    let invalid = list(json!({ "sessionId": session.session_id, "rootId": root_id, "path": "../x" }))
        .unwrap().deserialize::<serde_json::Value>().unwrap();
    assert_eq!(invalid, json!({ "ok": false, "error": { "kind": "invalidPath" } }));
    // 알 수 없는 필드(예: 절대 경로)는 요청 단계에서 거부한다.
    assert!(list(json!({ "sessionId": session.session_id, "rootId": root_id, "path": "", "absolute": "C:\\" })).is_err());
}
