use super::super::tests::{EXPORT, fixture_cli, fixture_credentials};
use super::*;
use serde_json::{Value, json};
use std::{collections::VecDeque, ffi::OsStr};

const OWNER: &str = "arn:aws:sts::123456789012:assumed-role/Sample/Sample";
const ROLE_ID: &str = "AROADBQP57FF2AEXAMPLE";
const TARGET: &str = "i-01234567";
const REASON: &str = "zzabbaxterm-0123456789abcdef0123456789abcdef";
const SESSION_ID: &str = "Sample-0123456789abcdef0";

fn request() -> StartRequest {
    StartRequest {
        scope: Scope::new("team".into(), "us-west-2".into()).unwrap(),
        target: TARGET.into(),
        document_name: None,
        parameters: None,
        reason: REASON.into(),
        owner: OWNER.into(),
        role: None,
        credentials: fixture_credentials(),
        program: fixture_cli().0,
        command_issued: true,
        termination_id: None,
        fixture: false,
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
    let cwd = std::path::Path::new("fixture-cwd");
    let start = request.start_command(cwd, OsStr::new("fixture-plugin-path"));
    assert_eq!(start.get_cwd().unwrap(), cwd.as_os_str());
    assert_eq!(start.get_argv()[0], request.program.as_os_str());
    assert_eq!(
        &start.get_argv()[1..],
        &[
            "ssm",
            "start-session",
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
    let shared = ["--region", "us-west-2", "--output", "json"];
    let flags = ["--no-cli-pager", "--no-cli-auto-prompt"];
    for (spec, command, args) in [
        (
            identity_command(&request.program, &request.scope, &request.credentials),
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
        assert_eq!(spec.program(), request.program.as_ref());
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
fn owner_is_validated_and_role_requires_consistent_assumed_role_evidence() {
    let parse = |value| parse_caller(&serde_json::to_vec(&value).unwrap()).unwrap();
    let (owner, missing_role) = parse(json!({"Arn": OWNER}));
    assert_eq!(owner.as_ref(), OWNER);
    assert!(missing_role.is_none());
    let (_, role) = parse(json!({"Arn": OWNER, "Account": "123456789012", "UserId": format!("{ROLE_ID}:Sample")}));
    assert_eq!(role, Some(AssumedRole::fixture("123456789012")));
    for (arn, account, user_id) in [
        (OWNER, "999999999999", format!("{ROLE_ID}:Sample")),
        (OWNER, "123456789012", format!("{ROLE_ID}:Another")),
        (OWNER, "123456789012", "AROA:Sample".into()),
        (OWNER, "123456789012", "AIDADBQP57FF2AEXAMPLE:Sample".into()),
        ("arn:aws:sts::123456789012:assumed-role//Sample", "123456789012", format!("{ROLE_ID}:Sample")),
        ("arn:aws:sts::123456789012:assumed-role/Sample/Extra/Sample", "123456789012", format!("{ROLE_ID}:Sample")),
        ("arn:awsfake:sts::123456789012:assumed-role/Sample/Sample", "123456789012", format!("{ROLE_ID}:Sample")),
        ("arn:aws:iam::123456789012:root", "123456789012", "123456789012".into()),
        ("arn:aws:iam::123456789012:user/Sample", "123456789012", "AIDADBQP57FF2AEXAMPLE".into()),
        ("arn:aws:sts::123456789012:federated-user/Sample", "123456789012", "123456789012:Sample".into()),
    ] {
        let (owner, role) = parse(json!({"Arn": arn, "Account": account, "UserId": user_id}));
        assert_eq!(owner.as_ref(), arn);
        assert!(role.is_none(), "{arn}");
    }
    let (_, malformed) = parse(json!({"Arn": OWNER, "Account": 123456789012u64, "UserId": []}));
    assert!(malformed.is_none());
    for identity in [b"{}".as_slice(), br#"{"Arn":""}"#, br#"{"Arn":"Sample"}"#] {
        assert_eq!(parse_caller(identity).err(), Some(CliError::InvalidResponse));
    }
}

#[test]
fn pty_filters_fixture_environment_without_mutating_the_process() {
    let request = request();
    let spec = request
        .scope
        .command(&request.program, "ssm", "start-session", &[]);
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
    let cli = fixture_cli();
    let scope = request().scope;
    let mut captured = Vec::new();
    let nodes = cli
        .online_nodes_with(
            &scope,
            "[default]\n",
            Arc::new(super::super::home::CliHome::new().unwrap()),
            |spec| {
                assert_eq!(spec.program(), cli.0.as_ref());
                let action = spec.args()[1].clone();
                let mut environment = std::collections::HashMap::new();
                spec.bound_environment(|key, value| {
                    environment.insert(key.to_owned(), value.to_os_string());
                });
                let reply = match action.as_str() {
                    "export-credentials" => {
                        assert_eq!(
                            spec.args()[2..6],
                            ["--profile", "default", "--format", "process"]
                        );
                        assert_eq!(
                            std::fs::read_to_string(&environment["AWS_CONFIG_FILE"]).unwrap(),
                            "[default]\n"
                        );
                        assert!(!environment.contains_key("AWS_SESSION_TOKEN"));
                        assert_eq!(environment["HOME"], environment["USERPROFILE"]);
                        assert!(
                            !Path::new(&environment["HOME"])
                                .join(".aws/cli/alias")
                                .exists()
                        );
                        capture(EXPORT.as_bytes().to_vec())
                    }
                    action => {
                        assert!(
                            !spec
                                .args()
                                .iter()
                                .any(|arg| arg == "--profile" || arg.contains("fixture-token"))
                        );
                        assert_eq!(environment["AWS_SESSION_TOKEN"], "fixture-token");
                        assert_eq!(environment["HOME"], environment["USERPROFILE"]);
                        assert!(
                            !Path::new(&environment["HOME"])
                                .join(".aws/sso/cache")
                                .exists()
                        );
                        assert_eq!(environment["AWS_DEFAULT_REGION"], "us-west-2");
                        match action {
                            "describe-instance-information" => {
                                fixture(json!({"InstanceInformationList": [
                                    {"InstanceId": TARGET, "PingStatus": "Online"},
                                    {"InstanceId": "i-11111111", "PingStatus": "ConnectionLost"}
                                ]}))
                            }
                            "get-caller-identity" => identity(),
                            _ => panic!("unexpected command"),
                        }
                    }
                };
                captured.push(action);
                std::future::ready(Ok(reply))
            },
        )
        .await
        .unwrap();
    assert_eq!(
        captured,
        [
            "export-credentials",
            "get-caller-identity",
            "describe-instance-information"
        ]
    );
    assert_eq!(nodes.len(), 1);
    let node = &nodes[0];
    let other = Scope::new("other".into(), "us-west-2".into()).unwrap();
    let other_program = cli.0.with_file_name("other-aws.exe");
    assert_eq!(
        require_node_scope(&cli.0, &other, node),
        Err(CliError::InvalidTarget)
    );
    assert_eq!(
        require_node_scope(&other_program, &scope, node),
        Err(CliError::InvalidTarget)
    );
    let cancel = CancellationToken::new();
    let (_, central_role) = parse_caller(&fixture(json!({
        "Arn": "arn:aws:sts::123456789012:assumed-role/Sample/OldSession",
        "Account": "123456789012", "UserId": format!("{ROLE_ID}:OldSession"),
    })).stdout).unwrap();
    let central_role = central_role.unwrap();
    let wrong_account = AssumedRole::fixture("999999999999");
    let wrong_id = AssumedRole { id: "AROAZZZZ57FF2AEXAMPLE".into(), account: "123456789012".into(), partition: "aws".into() };
    let wrong_partition = AssumedRole { id: ROLE_ID.into(), account: "123456789012".into(), partition: "aws-cn".into() };
    let shared = SessionDocument::AiSharedControl { role: &central_role, target: TARGET };
    for (scope, document, error) in [
        (&other, shared, CliError::InvalidTarget),
        (&scope, SessionDocument::AiSharedControl { role: &wrong_account, target: TARGET }, CliError::CallerChanged),
        (&scope, SessionDocument::AiSharedControl { role: &wrong_id, target: TARGET }, CliError::CallerChanged),
        (&scope, SessionDocument::AiSharedControl { role: &wrong_partition, target: TARGET }, CliError::CallerChanged),
        (&scope, SessionDocument::AiSharedControl { role: &central_role, target: "i-11111111" }, CliError::InvalidTarget),
    ] {
        assert_eq!(cli.prepare_document_start(scope, node, document, &cancel).err(), Some(error));
    }
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(cli.prepare_document_start(&scope, node, shared, &cancelled).err(), Some(CliError::Cancelled));

    let mut reasons = Vec::new();
    for (uses_shared_document, mut start) in [
        (false, cli.prepare_start(&scope, node, &cancel).await.unwrap()),
        (true, cli.prepare_document_start(&scope, node, shared, &cancel).unwrap()),
    ] {
        for (program, scope) in [(other_program.as_path(), &scope), (cli.0.as_ref(), &other)] {
            let invalid = start
                .authorize_with(program, scope, async |_| panic!("unexpected command"))
                .await;
            assert_eq!(invalid.err(), Some(CliError::InvalidTarget));
        }
        let missing = start
            .authorize_with(&cli.0, &scope, async |_| Err(CliError::PluginMissing))
            .await;
        assert_eq!(missing.err(), Some(CliError::PluginMissing));
        let permit = start
            .authorize_with(
                &cli.0,
                &scope,
                async |_| Ok(plugin::tests::fixture_plugin()),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(permit.0.get_argv()[0], cli.0.as_os_str());
        let mut expected = vec!["ssm", "start-session", "--region", "us-west-2", "--output", "json",
            "--target", TARGET, "--reason", start.reason.as_str()];
        if uses_shared_document {
            expected.extend(["--document-name", "ZzabbaAiSharedControl"]);
        }
        expected.extend(["--no-cli-pager", "--no-cli-auto-prompt"]);
        assert_eq!(&permit.0.get_argv()[1..], expected.as_slice());
        assert_eq!(
            permit.0.get_env("PATH"),
            Some(OsStr::new("fixture-plugin-path"))
        );
        assert_eq!(
            permit.0.get_env("AWS_SESSION_TOKEN"),
            Some(OsStr::new("fixture-token"))
        );
        assert!(Arc::ptr_eq(&permit.1, &node.credentials));
        assert_eq!(start.target(), TARGET);
        let nonce = start.reason.strip_prefix("zzabbaxterm-").unwrap();
        assert_eq!(nonce.len(), 32);
        assert!(nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(
            start
                .authorize_with(&cli.0, &scope, async |_| panic!("unexpected command"))
                .await
                .unwrap()
                .is_none()
        );
        reasons.push(start.reason);
    }
    let mut node = nodes.into_iter().next().unwrap();
    for arn in [OWNER, "arn:aws:iam::123456789012:root", "arn:aws:iam::123456789012:user/Sample"] {
        (node.owner, node.role) = parse_caller(&fixture(json!({"Arn": arn})).stdout).unwrap();
        let regular = cli.prepare_start(&scope, &node, &cancel).await.unwrap();
        assert_eq!(regular.account_id(), "123456789012");
        assert!(regular.assumed_role().is_none());
        assert_eq!(cli.prepare_document_start(&scope, &node, shared, &cancel).err(), Some(CliError::CallerChanged));
    }
    assert_ne!(reasons[0], reasons[1]);
}

#[tokio::test]
async fn expired_export_stops_before_sts_or_discovery() {
    let expired = EXPORT.replace("2099-01-01", "2000-01-01");
    let result = fixture_cli()
        .online_nodes_with(
            &request().scope,
            "[default]\n",
            Arc::new(super::super::home::CliHome::new().unwrap()),
            |spec| {
                assert_eq!(spec.args()[1], "export-credentials");
                std::future::ready(Ok(capture(expired.as_bytes().to_vec())))
            },
        )
        .await;
    assert_eq!(result.err(), Some(CliError::CredentialsExpired));
}

fn fixture(value: Value) -> Captured {
    capture(serde_json::to_vec(&value).unwrap())
}

fn identity() -> Captured {
    fixture(json!({"Arn": OWNER, "Account": "123456789012", "UserId": format!("{ROLE_ID}:Sample")}))
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
    let program = request.program.clone();
    let result = request
        .close_with(&program, scope, async |spec| {
            let (args, reply) = replies.pop_front().expect("unexpected command");
            assert_eq!(spec.program(), program.as_ref());
            let mut token_matches = false;
            spec.bound_environment(|key, value| {
                if key == "AWS_SESSION_TOKEN" {
                    token_matches = value == "fixture-token";
                }
            });
            assert!(token_matches);
            assert_eq!(
                spec.args(),
                args[..2]
                    .iter()
                    .copied()
                    .chain(["--region", "us-west-2", "--output", "json"])
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

#[tokio::test]
async fn expired_session_reports_unconfirmed_without_a_cli_call() {
    let mut request = request();
    let scope = request.scope.clone();
    Arc::get_mut(&mut request.credentials).unwrap().expire_for_test();

    assert_eq!(check_close(&mut request, &scope, []).await, Err(VerificationError::Unconfirmed));
    assert!(request.termination_id.is_none());
}

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
            [(HISTORY, Ok(inventory(vec![session()]))),]
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

#[tokio::test]
async fn termination_history_can_lag_without_a_second_write() {
    let mut request = request();
    let scope = request.scope.clone();
    let mut terminating = session();
    terminating["Status"] = json!("Terminating");
    assert_eq!(check_close(&mut request, &scope, [
        (HISTORY, Ok(inventory(vec![]))),
        (ACTIVE, Ok(inventory(vec![active_session()]))),
        (TERMINATE, Ok(fixture(json!({"SessionId": SESSION_ID})))),
        (CONFIRM, Ok(inventory(vec![]))),
        (CONFIRM, Ok(inventory(vec![terminating.clone()]))),
        (CONFIRM, Ok(inventory(vec![session()]))),
    ]).await, Ok(()));
    assert_eq!(check_close(&mut request, &scope, [
        (CONFIRM, Ok(inventory(vec![terminating]))),
        (CONFIRM, Ok(inventory(vec![session()]))),
    ]).await, Ok(()));
}

#[tokio::test]
async fn already_terminating_session_uses_only_reads() {
    let mut request = request();
    let scope = request.scope.clone();
    let mut terminating = session();
    terminating["Status"] = json!("Terminating");
    assert_eq!(check_close(&mut request, &scope, [
        (HISTORY, Ok(inventory(vec![]))),
        (ACTIVE, Ok(inventory(vec![terminating]))),
        (CONFIRM, Ok(inventory(vec![]))),
        (CONFIRM, Ok(inventory(vec![session()]))),
    ]).await, Ok(()));
}

#[tokio::test]
async fn readiness_retries_only_reads_after_timeout() {
    let request = request();
    let cancel = CancellationToken::new();
    let mut connected = session();
    connected["Status"] = json!("Connected");
    let mut results = [Err(CliError::TimedOut), Ok(inventory(vec![connected]))].into_iter();
    assert_eq!(request.wait_connected_with(&cancel, async |command, timeout| {
        assert_eq!(&command.args()[..2], &ACTIVE[..2]);
        assert!(command.args().windows(2).any(|args| args == ["--state", "Active"]));
        assert!(timeout <= CLOSE_LOOKUP_TIMEOUT);
        results.next().expect("unexpected read")
    }).await, Ok(()));
    assert!(results.next().is_none());
}

#[tokio::test]
async fn shared_control_rejects_unowned_session_records() {
    let mut request = request();
    request.document_name = Some("ZzabbaAiSharedControl");
    let scope = request.scope.clone();
    for (field, value) in [("Target", "i-11111111"), ("Owner", "another-owner")] {
        let mut wrong = active_session();
        wrong[field] = json!(value);
        let mut reply = Some(inventory(vec![wrong.clone()]));
        assert_eq!(request.wait_connected_with(&CancellationToken::new(), async |_, _| {
            Ok(reply.take().expect("unexpected lookup"))
        }).await, Err(VerificationError::Unconfirmed));
        assert_eq!(check_close(&mut request, &scope, [
            (HISTORY, Ok(inventory(vec![]))),
            (ACTIVE, Ok(inventory(vec![wrong]))),
        ]).await, Err(VerificationError::Unconfirmed));
    }
}

#[tokio::test]
async fn cleanup_retries_each_timed_out_read_without_repeating_the_write() {
    let mut request = request();
    let scope = request.scope.clone();
    assert_eq!(check_close(&mut request, &scope, [
        (HISTORY, Err(CliError::TimedOut)),
        (HISTORY, Ok(inventory(vec![]))),
        (ACTIVE, Err(CliError::TimedOut)),
        (HISTORY, Ok(inventory(vec![]))),
        (ACTIVE, Ok(inventory(vec![active_session()]))),
        (TERMINATE, Err(CliError::TimedOut)),
        (CONFIRM, Err(CliError::TimedOut)),
        (CONFIRM, Ok(inventory(vec![session()]))),
    ]).await, Ok(()));
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
    for history in [denied(), inventory(vec![session(), session()])] {
        assert_eq!(
            check_close(&mut request, &scope, [(HISTORY, Ok(history))]).await,
            Err(Unconfirmed)
        );
    }
    let mut twin = active_session();
    twin["SessionId"] = json!("twin-0123456789abcdef0");
    for active in [
        denied(),
        inventory(vec![active_session(), twin]),
        inventory(vec![session()]),
    ] {
        assert_eq!(
            check_close(
                &mut request,
                &scope,
                [(HISTORY, Ok(inventory(vec![]))), (ACTIVE, Ok(active)),]
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
                [(CONFIRM, Ok(inventory(vec![different_id.clone()]))),]
            )
            .await,
            Err(Unconfirmed)
        );
    }
}

#[test]
fn port_forward_start_uses_fixed_document_and_exported_credentials_without_profile() {
    let mut request = request();
    request.document_name = Some("AWS-StartPortForwardingSessionToRemoteHost");
    request.parameters = Some("host=rdp.example.test,portNumber=3389,localPortNumber=50000".into());
    let cwd = std::path::Path::new("fixture-cwd");
    let command = request.forward_command(cwd, OsStr::new("fixture-plugin-path"));
    assert_eq!(command.get_program(), request.program.as_os_str());
    assert_eq!(command.get_current_dir(), Some(cwd));
    let args: Vec<_> = command.get_args().collect();
    assert_eq!(args, [
        "ssm", "start-session", "--region", "us-west-2", "--output", "json",
        "--target", TARGET, "--reason", REASON,
        "--document-name", "AWS-StartPortForwardingSessionToRemoteHost",
        "--parameters", "host=rdp.example.test,portNumber=3389,localPortNumber=50000",
        "--no-cli-pager", "--no-cli-auto-prompt",
    ]);
    assert!(!args.iter().any(|arg| *arg == "--profile"));
    let env: std::collections::HashMap<_, _> = command.get_envs().collect();
    let value = |key: &str| env.get(OsStr::new(key)).copied().flatten();
    assert_eq!(value("AWS_CLI_ERROR_FORMAT"), Some(OsStr::new("legacy")));
    assert_eq!(value("PATH"), Some(OsStr::new("fixture-plugin-path")));
    assert_eq!(value("AWS_REGION"), Some(OsStr::new("us-west-2")));
    assert!(value("AWS_ACCESS_KEY_ID").is_some_and(|key| !key.is_empty()));
    for removed in ["AWS_PROFILE", "AWS_DEFAULT_PROFILE", "AWS_ENDPOINT_URL_SSM"] {
        assert_eq!(value(removed), None, "{removed}");
    }
}

#[tokio::test]
async fn port_forward_and_terminal_permits_do_not_cross() {
    let cli = fixture_cli();
    let mut forward = request();
    forward.command_issued = false;
    forward.document_name = Some("AWS-StartPortForwardingSession");
    forward.parameters = Some("portNumber=22,localPortNumber=50022".into());
    let scope = forward.scope.clone();
    assert_eq!(forward.authorize_with(&cli.0, &scope, async |_| panic!("unexpected command")).await.err(),
        Some(CliError::InvalidTarget));
    let permit = forward.authorize_forward_with(&cli.0, &scope, async |_| Ok(plugin::tests::fixture_plugin()))
        .await.unwrap().unwrap();
    assert!(permit.command.get_args().any(|arg| arg == "AWS-StartPortForwardingSession"));
    assert!(forward.authorize_forward_with(&cli.0, &scope, async |_| panic!("unexpected command")).await.unwrap().is_none());
    let mut shell = request();
    shell.command_issued = false;
    assert_eq!(shell.authorize_forward_with(&cli.0, &scope, async |_| panic!("unexpected command")).await.err(),
        Some(CliError::InvalidTarget));
}
