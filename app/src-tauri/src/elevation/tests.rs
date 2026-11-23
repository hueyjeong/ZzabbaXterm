use std::path::PathBuf;

use super::*;

const ITEM: &str = "21eb36c3-234d-439d-9e1f-1d3cc5ba8125";

fn document() -> Document {
    Document {
        owner: "main".into(),
        document: 3,
        request: "a".repeat(32),
    }
}

fn program() -> PathBuf {
    std::env::current_exe().unwrap()
}

fn request() -> Request {
    Request::new(
        "demo".into(),
        program(),
        vec!["-u".into()],
        ITEM.into(),
    )
}

#[test]
fn elevation_approval_is_bound_to_session_account_program_and_arguments() {
    let original = document();
    let fixed = request();
    let pending = || Pending::request(original.clone(), fixed.clone()).unwrap();
    assert_eq!(pending().details().0, &original);
    assert_eq!(pending().details().1, &fixed);

    for changed in [
        Document { owner: "other".into(), ..original.clone() },
        Document { document: 4, ..original.clone() },
        Document { request: "b".repeat(32), ..original.clone() },
    ] {
        assert!(matches!(pending().approve(&changed, &fixed), Err(Error::InvalidApproval)));
    }
    for changed in [
        Request { target: "다른 대상".into(), ..fixed.clone() },
        Request { caller: "other".into(), ..fixed.clone() },
        Request { run_as: "user".into(), ..fixed.clone() },
        Request { program: program().with_extension("other"), ..fixed.clone() },
        Request { args: vec!["-u".into(), "extra".into()], ..fixed.clone() },
        Request { args: vec![], ..fixed.clone() },
        Request { item: "21eb36c3-234d-439d-9e1f-1d3cc5ba8126".into(), ..fixed.clone() },
    ] {
        assert!(matches!(pending().approve(&original, &changed), Err(Error::InvalidApproval)));
    }

    let mut expired = pending();
    expired.expires = Instant::now();
    assert!(matches!(expired.approve(&original, &fixed), Err(Error::Expired)));
}

#[test]
fn elevation_request_rejects_relative_or_ambiguous_execution() {
    for invalid in [
        Request { program: PathBuf::from("id"), ..request() },
        Request { target: String::new(), ..request() },
        Request { caller: "demo\nroot".into(), ..request() },
        Request { run_as: "root\nuser".into(), ..request() },
        Request { args: vec!["-u\0".into()], ..request() },
        Request { args: vec!["-u\n".into()], ..request() },
        Request { item: "search text".into(), ..request() },
    ] {
        assert!(matches!(Pending::request(document(), invalid), Err(Error::InvalidRequest)));
    }
    let invalid_document = Document { request: "short".into(), ..document() };
    assert!(matches!(Pending::request(invalid_document, request()), Err(Error::InvalidApproval)));
    assert!(validate_request(&request()).is_ok());
}

#[test]
fn elevation_message_keeps_argument_boundaries() {
    let request = Request::new(
        "demo".into(),
        PathBuf::from("/usr/bin/printf"),
        vec!["a b".into(), "c".into()],
        ITEM.into(),
    );
    let message = message(&document(), &request);
    for line in [
        "대상: 내 PC",
        "호출 계정: demo",
        "실행 계정: root",
        "실행 파일: /usr/bin/printf",
        "  1. a b",
        "  2. c",
        format!("금고 항목: {ITEM}").as_str(),
        &format!("요청 ID: {}", document().request),
    ] {
        assert!(message.lines().any(|value| value == line), "{line:?}\n{message}");
    }
    assert!(!message.lines().any(|line| line.contains("a b c")), "{message}");
}

#[test]
fn elevation_request_id_and_program_are_normalized() {
    let id = request_id().unwrap();
    assert_eq!(id.len(), 32);
    assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(canonical_program("relative/id").is_none());
    let program = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    assert_eq!(canonical_program(program.to_str().unwrap()).unwrap(), program);
}
