use super::*;

fn fixture_sso() -> DirectSsoConfig {
    DirectSsoConfig {
        account: "123456789012".into(), role: "Fixture".into(), region: "us-east-1".into(),
        start_url: "https://example.invalid/start".into(), session: None, scopes: None,
    }
}

pub(super) fn fixture_cli() -> AwsCli {
    AwsCli(Arc::from(std::env::current_exe().unwrap()), true)
}

pub(super) const EXPORT: &str = r#"{"Version":1,"AccessKeyId":"fixture-key","SecretAccessKey":"fixture-secret","SessionToken":"fixture-token","Expiration":"2099-01-01T00:00:00Z"}"#;

pub(super) fn fixture_credentials() -> Arc<Credentials> {
    Arc::new(Credentials::parse(response(0, EXPORT, "")).unwrap())
}

fn response(exit_code: i32, stdout: &str, stderr: &str) -> Captured {
    Captured {
        exit_code: Some(exit_code),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

#[test]
fn configured_region_distinguishes_unset_profile_and_failure() {
    let rows = [
        (
            0,
            "ap-northeast-2\r\n",
            "",
            Ok(Some("ap-northeast-2".to_owned())),
        ),
        (1, "", "", Ok(None)),
        (
            1,
            "",
            "Warning: Unable to record CLI history. Check file permissions for ./.\r\n",
            Ok(None),
        ),
        (
            1,
            "",
            "unrelated failure",
            Err(CliError::CommandFailed { exit_code: Some(1) }),
        ),
        (
            255,
            "",
            "The config profile (missing) could not be found",
            Err(CliError::ProfileNotFound),
        ),
        (0, "https://invalid", "", Err(CliError::InvalidRegion)),
        (0, "", "", Err(CliError::InvalidRegion)),
        (
            255,
            "",
            "",
            Err(CliError::CommandFailed {
                exit_code: Some(255),
            }),
        ),
    ];
    for (exit, stdout, stderr, expected) in rows {
        assert_eq!(parse_region(response(exit, stdout, stderr)), expected);
    }
}

#[test]
fn online_list_only_accepts_complete_inventory() {
    use CliError::*;
    let scope = Scope::new("team".into(), "us-west-2".into()).unwrap();
    let rows: &[(i32, &str, &str, Result<&[&str], CliError>)] = &[
        (0, r#"{"InstanceInformationList":[]}"#, "", Ok(&[])),
        (
            0,
            r#"{"InstanceInformationList":[{"InstanceId":"mi-0123456789abcdef0","PingStatus":"Online","Name":"display name"},{"InstanceId":"i-34567890","PingStatus":"ConnectionLost"}]}"#,
            "",
            Ok(&["mi-0123456789abcdef0"]),
        ),
        (
            0,
            r#"{"InstanceInformationList":[{"InstanceId":"i-34567890","PingStatus":"Inactive"}]}"#,
            "",
            Ok(&[]),
        ),
        (0, "", "", Err(InvalidResponse)),
        (0, "{}", "", Err(InvalidResponse)),
        (
            0,
            r#"{"InstanceInformationList":[{"InstanceId":"i-34567890"}]}"#,
            "",
            Err(InvalidResponse),
        ),
        (
            0,
            r#"{"InstanceInformationList":[{"Name":"i-34567890","PingStatus":"Online"}]}"#,
            "",
            Err(InvalidResponse),
        ),
        (
            0,
            r#"{"InstanceInformationList":[{"InstanceId":"not-an-id","PingStatus":"Online"}]}"#,
            "",
            Err(InvalidResponse),
        ),
        (
            0,
            r#"{"InstanceInformationList":[],"NextToken":"opaque"}"#,
            "",
            Err(InvalidResponse),
        ),
        (
            254,
            r#"{"InstanceInformationList":[]}"#,
            "An error occurred (AccessDeniedException) when calling DescribeInstanceInformation operation: denied",
            Err(AccessDenied),
        ),
        (
            255,
            "",
            "Error when retrieving token from sso: Token has expired and refresh failed",
            Err(SsoLoginRequired),
        ),
        (
            255,
            "",
            "unexpected",
            Err(CommandFailed {
                exit_code: Some(255),
            }),
        ),
    ];
    for (exit, stdout, stderr, expected) in rows {
        let result = fixture_cli().parse_online_nodes(
            &scope,
            Arc::from("fixture-owner"),
            None,
            fixture_credentials(),
            response(*exit, stdout, stderr),
        );
        match expected {
            Ok(ids) => {
                let nodes = result.unwrap();
                assert_eq!(
                    nodes
                        .iter()
                        .map(OnlineNode::instance_id)
                        .collect::<Vec<_>>(),
                    *ids
                );
                assert!(nodes.iter().all(|node| node.scope == scope));
            }
            Err(error) => assert_eq!(result.err(), Some(*error)),
        }
    }
}

#[test]
fn discovery_commands_use_literal_arguments_and_never_fallback() {
    let profile = "team & admin";
    let scope = Scope::new(profile.into(), "us-west-2".into()).unwrap();
    let cli = fixture_cli();
    for (spec, expected) in [
        (cli.profiles_command(), vec!["configure", "list-profiles"]),
        (
            cli.region_command(profile),
            vec!["configure", "get", "region", "--profile", profile],
        ),
        (
            cli.nodes_command(&scope),
            vec![
                "ssm",
                "describe-instance-information",
                "--region",
                "us-west-2",
                "--output",
                "json",
            ],
        ),
        (
            cli.login_command(&scope, &ProfileAuth::Sso(fixture_sso())),
            vec!["sso", "login", "--profile", profile],
        ),
        (
            cli.login_command(&scope, &ProfileAuth::Login(profile::LoginConfig {
                session: "arn:aws:iam::123456789012:root".into(), region: None,
            })),
            vec!["login", "--profile", profile, "--region", "us-west-2"],
        ),
        (
            cli.login_command(&scope, &ProfileAuth::Login(profile::LoginConfig {
                session: "arn:aws:iam::123456789012:root".into(), region: Some("ap-northeast-1".into()),
            })),
            vec!["login", "--profile", profile, "--region", "ap-northeast-1"],
        ),
    ] {
        assert_eq!(spec.program(), cli.0.as_ref());
        assert_eq!(
            spec.args(),
            [expected, vec!["--no-cli-pager", "--no-cli-auto-prompt"]].concat()
        );
    }
    for error in [
        CliError::AccessDenied,
        CliError::CredentialsExpired,
        CliError::CommandFailed {
            exit_code: Some(255),
        },
    ] {
        assert_eq!(
            cli.relogin_command(&scope, error).err(),
            Some(CliError::SsoLoginNotApplicable)
        );
    }
    let plugin = CommandSpec::version(Tool::Plugin, cli.0.clone());
    assert_eq!(plugin.program(), cli.0.as_ref());
    assert_eq!(plugin.args(), ["--version"]);
    assert_eq!(
        Scope::new("--debug".into(), "us-west-2".into()).err(),
        Some(CliError::InvalidProfile)
    );
    assert_eq!(
        Scope::new(profile.into(), "other.example".into()).err(),
        Some(CliError::InvalidRegion)
    );
}

#[tokio::test]
async fn aws_login_refresh_lock_is_per_session_and_cancellable() {
    let auth = |arn: &str| ProfileAuth::Login(profile::LoginConfig { session: arn.into(), region: None });
    let first = auth("arn:aws:iam::111111111111:user/lock-fixture");
    let cancel = CancellationToken::new();
    let held = export_home(&first, &cancel).await.unwrap();
    // 다른 login_session은 기다리지 않는다.
    let other = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        export_home(&auth("arn:aws:iam::222222222222:user/lock-fixture"), &cancel),
    )
    .await
    .expect("다른 세션은 막히지 않는다");
    assert!(other.is_ok());
    // 같은 세션은 기다리고, 기다리는 중의 취소는 바로 반영된다.
    let waiting = CancellationToken::new();
    let pending = tokio::spawn({
        let first = first.clone();
        let waiting = waiting.clone();
        async move { export_home(&first, &waiting).await.map(drop) }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!pending.is_finished());
    waiting.cancel();
    assert_eq!(pending.await.unwrap(), Err(CliError::Cancelled));
    // 홈을 버리면 같은 세션의 다음 내보내기가 진행된다.
    drop(held);
    assert!(tokio::time::timeout(std::time::Duration::from_secs(5), export_home(&first, &cancel)).await.unwrap().is_ok());
}
