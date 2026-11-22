use super::*;
use serde_json::{Value, json};
use std::{collections::VecDeque, ffi::OsStr};

const OWNER: &str = "arn:aws:sts::123456789012:assumed-role/Sample/Sample";
const TARGET: &str = "i-01234567";
const REASON: &str = "zzabbaxterm-0123456789abcdef0123456789abcdef";
const SESSION_ID: &str = "Sample-0123456789abcdef0";

fn request() -> StartRequest {
    StartRequest {
        scope: Scope::new("team".into(), "us-west-2".into()).unwrap(),
        target: TARGET.into(),
        reason: REASON.into(),
        owner: OWNER.into(),
        command_issued: true,
        termination_id: None,
    }
}

fn session() -> Value {
    json!({"SessionId": SESSION_ID, "Reason": REASON, "Owner": OWNER,
           "Target": TARGET, "Status": "Terminated"})
}

fn capture(stdout: Vec<u8>) -> super::super::Captured {
    super::super::Captured {
        exit_code: Some(0),
        stdout,
        stderr: Vec::new(),
    }
}

#[test]
fn start_identity_and_lookup_commands_preserve_selected_scope() {
    let request = request();
    let mut request = StartRequest {
        command_issued: false,
        ..request
    };
    let cwd = std::path::Path::new("fixture-cwd");
    let start = request.issue_start_command(cwd).unwrap();
    assert_eq!(start.get_cwd().unwrap(), cwd.as_os_str());
    assert_eq!(
        start.get_argv(),
        &[
            if cfg!(windows) { "aws.exe" } else { "aws" },
            "ssm",
            "start-session",
            "--profile",
            "team",
            "--region",
            "us-west-2",
            "--output",
            "json",
            "--target",
            TARGET,
            "--reason",
            REASON,
            "--no-cli-pager",
            "--no-cli-auto-prompt",
        ]
    );
    let shared = [
        "--profile",
        "team",
        "--region",
        "us-west-2",
        "--output",
        "json",
    ];
    let flags = ["--no-cli-pager", "--no-cli-auto-prompt"];
    for (spec, command, args) in [
        (
            identity_command(&request.scope),
            ["sts", "get-caller-identity"],
            vec![],
        ),
        (
            request.lookup_command(SessionLookup::Active, None),
            ["ssm", "describe-sessions"],
            vec![
                "--state",
                "Active",
                "--filters",
                "key=Target,value=i-01234567",
            ],
        ),
        (
            request.lookup_command(SessionLookup::History, None),
            ["ssm", "describe-sessions"],
            vec![
                "--state",
                "History",
                "--filters",
                "key=Target,value=i-01234567",
            ],
        ),
    ] {
        assert_eq!(spec.program(), "aws");
        assert_eq!(
            spec.args(),
            command
                .into_iter()
                .chain(shared)
                .chain(args)
                .chain(flags)
                .collect::<Vec<_>>()
        );
    }
    assert!(request.take_start_command().unwrap().is_none());
}

#[test]
fn ownership_uses_only_complete_exact_service_matches() {
    use VerificationError::Unconfirmed;
    let request = request();
    let mut cases = vec![
        ("exact", json!({"Sessions": [session()]}), Some(SESSION_ID)),
        ("empty", json!({"Sessions": []}), None),
        ("missing inventory", json!({}), None),
        (
            "truncated",
            json!({"Sessions": [session()], "NextToken": "opaque"}),
            None,
        ),
        (
            "duplicates",
            json!({"Sessions": [session(), session()]}),
            None,
        ),
    ];
    for (key, value) in [
        ("SessionId", "--debug"),
        ("SessionId", ""),
        ("Reason", "zzabbaxterm-lookalike"),
        ("Owner", "arn:aws:iam::123456789012:role/Sample"),
        ("Target", "i-11111111"),
    ] {
        let mut wrong = session();
        wrong[key] = json!(value);
        cases.push((key, json!({"Sessions": [wrong]}), None));
    }
    for key in ["SessionId", "Reason", "Owner", "Target"] {
        let mut incomplete = session();
        incomplete.as_object_mut().unwrap().remove(key);
        cases.push((key, json!({"Sessions": [incomplete, session()]}), None));
    }
    let mut ordinary = session();
    ordinary.as_object_mut().unwrap().remove("Reason");
    ordinary["SessionId"] = json!("Ordinary-0123456789abcdef0");
    cases.push((
        "ordinary CLI row",
        json!({"Sessions": [ordinary, session()]}),
        Some(SESSION_ID),
    ));
    let mut unrelated = session();
    unrelated["SessionId"] = json!("Forged-0123456789abcdef0");
    unrelated["Reason"] = json!("zzabbaxterm-0123456789abcdef0123456789abcdef-extra");
    cases.push((
        "unrelated nonce",
        json!({"Sessions": [unrelated, session()]}),
        Some(SESSION_ID),
    ));
    let mut twin = session();
    twin["SessionId"] = json!("Other-0123456789abcdef0");
    cases.push(("two ids", json!({"Sessions": [session(), twin]}), None));
    let mut duplicate_id = session();
    duplicate_id["Reason"] = json!("different reason");
    cases.push((
        "duplicate id with a different reason",
        json!({"Sessions": [session(), duplicate_id]}),
        None,
    ));
    for (label, inventory, expected) in cases {
        let result = request
            .find_session(Ok(fixture(inventory)))
            .and_then(|session| session.ok_or(Unconfirmed));
        if let Some(expected) = expected {
            let session = result.unwrap();
            assert_eq!(session.session_id(), expected, "{label}");
            assert!(session.is_terminated(), "{label}");
        } else {
            assert_eq!(result.err(), Some(Unconfirmed), "{label}");
        }
    }
    for error in [
        CliError::AccessDenied,
        CliError::Cancelled,
        CliError::TimedOut,
    ] {
        assert_eq!(request.find_session(Err(error)).err(), Some(Unconfirmed));
    }
    let mut denied = capture(serde_json::to_vec(&json!({"Sessions":[session()]})).unwrap());
    denied.exit_code = Some(254);
    denied.stderr = b"An error occurred (AccessDeniedException) when calling the DescribeSessions operation: denied".to_vec();
    assert_eq!(request.find_session(Ok(denied)).err(), Some(Unconfirmed));
    assert_eq!(
        request.find_session(Ok(capture(b"{...".to_vec()))).err(),
        Some(Unconfirmed)
    );
}

#[test]
fn owner_is_a_validated_sts_arn() {
    assert_eq!(
        parse_caller_arn(&serde_json::to_vec(&json!({"Arn": OWNER})).unwrap()),
        Ok(OWNER.into())
    );
    for identity in [b"{}".as_slice(), br#"{"Arn":""}"#, br#"{"Arn":"Sample"}"#] {
        assert_eq!(parse_caller_arn(identity), Err(CliError::InvalidResponse));
    }
}

#[test]
fn pty_filters_fixture_environment_without_mutating_the_process() {
    let spec = request().scope.command("ssm", "start-session", &[]);
    let mut command = CommandBuilder::new(spec.program());
    command.env("EXTRA_BASE_ENV", "discarded");
    let inherited = spec.env_remove().iter().map(|key| (*key, "discarded"));
    apply_start_environment(
        &mut command,
        &spec,
        inherited.chain([
            ("AWS_CONFIG_FILE", "fixture-config"),
            ("PATH", "fixture-path"),
            ("AWS_MAX_ATTEMPTS", "9"),
            ("PATHEXT", ".CMD;.EXE"),
        ]),
    );
    assert!(command.get_env("EXTRA_BASE_ENV").is_none());
    for key in spec.env_remove() {
        assert!(command.get_env(key).is_none(), "{key}");
    }
    for (key, value) in spec.env_set() {
        assert_eq!(command.get_env(key), Some(OsStr::new(value)), "{key}");
    }
    assert_eq!(
        command.get_env("AWS_CONFIG_FILE"),
        Some(OsStr::new("fixture-config"))
    );
    assert_eq!(command.get_env("PATH"), Some(OsStr::new("fixture-path")));
    assert_eq!(command.get_env("AWS_MAX_ATTEMPTS"), Some(OsStr::new("1")));
    #[cfg(windows)]
    assert_eq!(command.get_env("PATHEXT"), Some(OsStr::new(".EXE")));
}

#[tokio::test(flavor = "current_thread")]
async fn start_requires_an_online_node_from_the_same_scope() {
    let scope = request().scope;
    let node = super::super::parse_online_nodes(
        &scope,
        fixture(json!({"InstanceInformationList": [
            {"InstanceId": TARGET, "PingStatus": "Online"},
            {"InstanceId": "i-11111111", "PingStatus": "ConnectionLost"}
        ]})),
    )
    .unwrap()
    .pop()
    .unwrap();
    let other = Scope::new("other".into(), "us-west-2".into()).unwrap();
    let bad = prepare_start_with(&other, &node, async |_| panic!("unexpected command")).await;
    assert_eq!(bad.err(), Some(CliError::InvalidTarget));
    let mut reasons = Vec::new();
    for _ in 0..2 {
        let mut start = prepare_start_with(&scope, &node, async |spec| {
            assert_eq!(&spec.args()[..2], ["sts", "get-caller-identity"]);
            Ok(identity())
        })
        .await
        .unwrap();
        assert_eq!(start.target(), TARGET);
        let nonce = start.reason.strip_prefix("zzabbaxterm-").unwrap();
        assert_eq!(nonce.len(), 32);
        assert!(nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(
            start
                .issue_start_command(std::path::Path::new("fixture-cwd"))
                .is_some()
        );
        assert!(start.take_start_command().unwrap().is_none());
        reasons.push(start.reason);
    }
    assert_ne!(reasons[0], reasons[1]);
}

fn fixture(value: Value) -> Captured {
    capture(serde_json::to_vec(&value).unwrap())
}

fn identity() -> Captured {
    fixture(json!({"Arn": OWNER}))
}

fn inventory(sessions: Vec<Value>) -> Captured {
    fixture(json!({"Sessions": sessions}))
}

fn active_session() -> Value {
    let mut row = session();
    row["Status"] = json!("Disconnected");
    row
}

fn denied() -> Captured {
    Captured {
        exit_code: Some(254),
        stdout: Vec::new(),
        stderr: br#"{"Code":"AccessDeniedException"}"#.to_vec(),
    }
}

type Exchange = (&'static [&'static str], Result<Captured, CliError>);

async fn check_close(
    request: &mut StartRequest,
    scope: &Scope,
    replies: impl IntoIterator<Item = Exchange>,
) -> Result<(), VerificationError> {
    let mut replies: VecDeque<_> = replies.into_iter().collect();
    let result = request
        .close_with(scope, async |spec| {
            let (args, reply) = replies.pop_front().expect("unexpected command");
            assert_eq!(spec.program(), "aws");
            assert_eq!(
                spec.args(),
                args[..2]
                    .iter()
                    .copied()
                    .chain([
                        "--profile",
                        "team",
                        "--region",
                        "us-west-2",
                        "--output",
                        "json"
                    ])
                    .chain(args[2..].iter().copied())
                    .chain(["--no-cli-pager", "--no-cli-auto-prompt"])
                    .collect::<Vec<_>>()
            );
            reply
        })
        .await;
    assert!(replies.is_empty(), "missing commands");
    result
}

const STS: &[&str] = &["sts", "get-caller-identity"];
const HISTORY: &[&str] = &[
    "ssm",
    "describe-sessions",
    "--state",
    "History",
    "--filters",
    "key=Target,value=i-01234567",
];
const ACTIVE: &[&str] = &[
    "ssm",
    "describe-sessions",
    "--state",
    "Active",
    "--filters",
    "key=Target,value=i-01234567",
];
const TERMINATE: &[&str] = &["ssm", "terminate-session", "--session-id", SESSION_ID];
const CONFIRM: &[&str] = &[
    "ssm",
    "describe-sessions",
    "--state",
    "History",
    "--filters",
    r#"[{"key":"SessionId","value":"Sample-0123456789abcdef0"}]"#,
];

#[tokio::test(flavor = "current_thread")]
async fn existing_history_ends_cleanup_without_a_write() {
    let mut request = request();
    let scope = request.scope.clone();
    assert_eq!(
        check_close(
            &mut request,
            &scope,
            [
                (STS, Ok(identity())),
                (HISTORY, Ok(inventory(vec![session()]))),
            ]
        )
        .await,
        Ok(())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn active_session_is_terminated_once_and_confirmed_by_history() {
    let mut request = request();
    let scope = request.scope.clone();
    let mut neighbor = active_session();
    neighbor["SessionId"] = json!("other-0123456789abcdef0");
    neighbor["Reason"] = json!("another-tab");
    assert_eq!(
        check_close(
            &mut request,
            &scope,
            [
                (STS, Ok(identity())),
                (HISTORY, Ok(inventory(vec![]))),
                (ACTIVE, Ok(inventory(vec![neighbor, active_session()]))),
                (TERMINATE, Ok(fixture(json!({"SessionId": SESSION_ID})))),
                (CONFIRM, Ok(inventory(vec![session()]))),
            ]
        )
        .await,
        Ok(())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn denied_ambiguous_or_unowned_sessions_never_authorize_a_write() {
    use VerificationError::Unconfirmed;
    let mut request = request();
    let scope = request.scope.clone();
    for other in [
        Scope::new("other".into(), "us-west-2".into()).unwrap(),
        Scope::new("team".into(), "us-east-1".into()).unwrap(),
    ] {
        assert_eq!(
            check_close(&mut request, &other, []).await,
            Err(Unconfirmed)
        );
    }
    request.command_issued = false;
    assert_eq!(
        check_close(&mut request, &scope, []).await,
        Err(Unconfirmed)
    );
    request.command_issued = true;
    assert_eq!(
        check_close(
            &mut request,
            &scope,
            [(
                STS,
                Ok(fixture(
                    json!({"Arn": "arn:aws:sts::123456789012:assumed-role/Sample/Other"})
                ))
            ),]
        )
        .await,
        Err(Unconfirmed)
    );
    for history in [denied(), inventory(vec![session(), session()])] {
        assert_eq!(
            check_close(
                &mut request,
                &scope,
                [(STS, Ok(identity())), (HISTORY, Ok(history)),]
            )
            .await,
            Err(Unconfirmed)
        );
    }
    let mut twin = active_session();
    twin["SessionId"] = json!("twin-0123456789abcdef0");
    for active in [
        denied(),
        inventory(vec![]),
        inventory(vec![active_session(), twin]),
        inventory(vec![session()]),
    ] {
        assert_eq!(
            check_close(
                &mut request,
                &scope,
                [
                    (STS, Ok(identity())),
                    (HISTORY, Ok(inventory(vec![]))),
                    (ACTIVE, Ok(active)),
                ]
            )
            .await,
            Err(Unconfirmed)
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_denied_and_mismatched_write_responses_cannot_trigger_another_write() {
    use VerificationError::Unconfirmed;
    let mut different_id = session();
    different_id["SessionId"] = json!("other-0123456789abcdef0");
    for (write, history, expected) in [
        (
            Err(CliError::TimedOut),
            Some(inventory(vec![different_id.clone()])),
            Err(Unconfirmed),
        ),
        (
            Err(CliError::TimedOut),
            Some(inventory(vec![session()])),
            Ok(()),
        ),
        (
            Ok(fixture(json!({"SessionId": SESSION_ID}))),
            Some(inventory(vec![active_session()])),
            Err(Unconfirmed),
        ),
        (Ok(denied()), None, Err(Unconfirmed)),
        (
            Ok(fixture(json!({"SessionId": "other-0123456789abcdef0"}))),
            None,
            Err(Unconfirmed),
        ),
    ] {
        let mut request = request();
        let scope = request.scope.clone();
        let mut replies = vec![
            (STS, Ok(identity())),
            (HISTORY, Ok(inventory(vec![]))),
            (ACTIVE, Ok(inventory(vec![active_session()]))),
            (TERMINATE, write),
        ];
        if let Some(history) = history {
            replies.push((CONFIRM, Ok(history)));
        }
        assert_eq!(check_close(&mut request, &scope, replies).await, expected);
        assert_eq!(
            check_close(
                &mut request,
                &scope,
                [
                    (STS, Ok(identity())),
                    (CONFIRM, Ok(inventory(vec![different_id.clone()]))),
                ]
            )
            .await,
            Err(Unconfirmed)
        );
    }
}
