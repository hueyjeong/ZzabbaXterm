use super::*;
use serde_json::{Value, json};

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
            request.issue_start_command().unwrap(),
            ["ssm", "start-session"],
            vec!["--target", TARGET, "--reason", REASON],
        ),
        (
            identity_command(&request.scope),
            ["sts", "get-caller-identity"],
            vec![],
        ),
        (
            request.lookup_command(SessionLookup::Active),
            ["ssm", "describe-sessions"],
            vec![
                "--state",
                "Active",
                "--filters",
                "key=Target,value=i-01234567",
            ],
        ),
        (
            request.lookup_command(SessionLookup::History),
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
        let result = request.verify_result(
            &request.scope,
            OWNER,
            Ok(capture(serde_json::to_vec(&inventory).unwrap())),
        );
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
        assert_eq!(
            request
                .verify_result(&request.scope, OWNER, Err(error))
                .err(),
            Some(Unconfirmed)
        );
    }
    let mut denied = capture(serde_json::to_vec(&json!({"Sessions":[session()]})).unwrap());
    denied.exit_code = Some(254);
    denied.stderr = b"An error occurred (AccessDeniedException) when calling the DescribeSessions operation: denied".to_vec();
    assert_eq!(
        request
            .verify_result(&request.scope, OWNER, Ok(denied))
            .err(),
        Some(Unconfirmed)
    );
    assert_eq!(
        request
            .verify_result(&request.scope, OWNER, Ok(capture(b"{...".to_vec())))
            .err(),
        Some(Unconfirmed)
    );
}

#[test]
fn ownership_rejects_sts_and_scope_changes_even_for_matching_inventory() {
    let request = request();
    let output = serde_json::to_vec(&json!({"Sessions": [session()]})).unwrap();
    assert_eq!(
        parse_caller_arn(&serde_json::to_vec(&json!({"Arn": OWNER})).unwrap()),
        Ok(OWNER.into())
    );
    for identity in [b"{}".as_slice(), br#"{"Arn":""}"#, br#"{"Arn":"Sample"}"#] {
        assert_eq!(parse_caller_arn(identity), Err(CliError::InvalidResponse));
    }
    for (profile, region, caller) in [
        ("other", "us-west-2", OWNER),
        ("team", "us-east-1", OWNER),
        (
            "team",
            "us-west-2",
            "arn:aws:sts::123456789012:assumed-role/Sample/Other",
        ),
    ] {
        let scope = Scope::new(profile.into(), region.into()).unwrap();
        assert_eq!(
            request
                .verify_result(&scope, caller, Ok(capture(output.clone())))
                .err(),
            Some(VerificationError::Unconfirmed)
        );
    }
}
