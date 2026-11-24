use super::*;

fn fixture_sso() -> DirectSsoConfig {
    DirectSsoConfig {
        account: "123456789012".into(), role: "Fixture".into(), region: "us-east-1".into(),
        start_url: "https://example.invalid/start".into(), session: None,
    }
}

pub(super) fn fixture_cli() -> AwsCli {
    AwsCli(Arc::from(std::env::current_exe().unwrap()))
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
