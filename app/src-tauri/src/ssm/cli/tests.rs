use super::*;

#[test]
fn exit_and_service_code_rows_are_distinct() {
    use CliError::*;
    let rows: &[(i32, &str, CliError)] = &[
        (
            254,
            "An error occurred (AccessDeniedException) when calling the DescribeInstanceInformation operation: denied",
            AccessDenied,
        ),
        (
            254,
            "aws: [ERROR]: An error occurred (TargetNotConnected) when calling the StartSession operation: disconnected",
            TargetNotConnected,
        ),
        (
            254,
            r#"{"Code":"ExpiredTokenException","Message":"redacted"}"#,
            CredentialsExpired,
        ),
        (
            254,
            "An error occurred (ExpiredToken) when calling the DescribeInstanceInformation operation: expired",
            CredentialsExpired,
        ),
        (
            255,
            "aws: [ERROR]: Error when retrieving token from sso: Token has expired and refresh failed",
            SsoLoginRequired,
        ),
        (
            255,
            "Error loading SSO Token: Token for example does not exist",
            SsoLoginRequired,
        ),
        (
            255,
            "The SSO session associated with this profile has expired or is otherwise invalid. To refresh this SSO session run aws sso login with the corresponding profile.",
            SsoLoginRequired,
        ),
        (
            255,
            "Error when retrieving token from sso: unrelated I/O error",
            CommandFailed {
                exit_code: Some(255),
            },
        ),
        (
            255,
            "The config profile (missing) could not be found",
            ProfileNotFound,
        ),
        (
            253,
            "aws: [ERROR]: An error occurred (NoRegion): You must specify a region.",
            InvalidRegion,
        ),
        (
            255,
            "Provided region_name 'bad' doesn't match a supported format.",
            InvalidRegion,
        ),
        (
            255,
            "Could not connect to the endpoint URL: redacted",
            EndpointUnavailable,
        ),
        (
            255,
            "Unable to parse config file: redacted",
            InvalidConfiguration,
        ),
        (
            253,
            "aws: [ERROR]: An error occurred (NoCredentials): Unable to locate credentials",
            CredentialsUnavailable,
        ),
        (
            254,
            r#"{"Code":"Other","Message":"ExpiredTokenException AccessDeniedException"}"#,
            CommandFailed {
                exit_code: Some(254),
            },
        ),
        (1, "", CommandFailed { exit_code: Some(1) }),
    ];
    for (exit, stderr, expected) in rows {
        let capture = Captured {
            exit_code: Some(*exit),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        };
        assert_eq!(capture.success(), Err(*expected));
    }
    assert_eq!(classify(None, b""), CommandFailed { exit_code: None });
    assert_eq!(
        classify(Some(255), b"\xff"),
        CommandFailed {
            exit_code: Some(255)
        }
    );
}

#[tokio::test]
async fn missing_tool_is_attributed_and_pre_cancel_wins() {
    let cancel = CancellationToken::new();
    for (tool, expected) in [
        (Tool::Aws, CliError::AwsCliMissing),
        (Tool::Plugin, CliError::PluginMissing),
    ] {
        let cmd = Command::new(".zzabbaxterm-no-such-test-executable/nonexistent");
        assert_eq!(
            capture(cmd, tool, LOOKUP_TIMEOUT, &cancel).await.err(),
            Some(expected)
        );
    }
    cancel.cancel();
    let cmd = Command::new(".zzabbaxterm-no-such-test-executable/nonexistent");
    assert_eq!(
        capture(cmd, Tool::Aws, LOOKUP_TIMEOUT, &cancel).await.err(),
        Some(CliError::Cancelled)
    );
}

#[test]
fn child_environment_leaves_config_and_cache_to_cli() {
    let cli = super::super::tests::fixture_cli();
    let spec = CommandSpec::aws(
        &cli.0,
        &["ssm", "terminate-session", "--session-id", "fixture"],
    );
    let cmd = spec.command();
    assert_eq!(cmd.as_std().get_program(), cli.0.as_os_str());
    let env: std::collections::BTreeMap<_, _> = cmd.as_std().get_envs().collect();
    for key in [
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_BEARER_TOKEN_SSO",
        "AWS_PROFILE",
        "AWS_DEFAULT_REGION",
        "AWS_ENDPOINT_URL",
        "AWS_ENDPOINT_URL_SSM",
        "AWS_ENDPOINT_URL_STS",
        "AWS_ENDPOINT_URL_SSO_OIDC",
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    ] {
        assert_eq!(env.get(std::ffi::OsStr::new(key)), Some(&None));
    }
    for (key, value) in [
        ("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "true"),
        ("AWS_EC2_METADATA_DISABLED", "true"),
        ("AWS_CLI_HISTORY_FILE", "./."),
        ("AWS_MAX_ATTEMPTS", "1"),
    ] {
        assert_eq!(
            env.get(std::ffi::OsStr::new(key)),
            Some(&Some(std::ffi::OsStr::new(value)))
        );
    }
    for key in [
        "AWS_CONFIG_FILE",
        "AWS_SHARED_CREDENTIALS_FILE",
        "HOME",
        "USERPROFILE",
        "PATH",
    ] {
        assert!(!env.contains_key(std::ffi::OsStr::new(key)));
    }
}

#[test]
fn executable_resolution_skips_relative_path_and_pins_the_chosen_file() {
    let exe = std::env::current_exe().unwrap();
    let name = exe.file_name().unwrap().to_str().unwrap();
    let path = std::env::join_paths([Path::new("."), exe.parent().unwrap()]).unwrap();
    assert_eq!(
        resolve_on_path(name, &path),
        Some(exe.canonicalize().unwrap())
    );
    assert_eq!(resolve_on_path(name, OsStr::new(".")), None);
}

// 테스트 바이너리의 자식은 로컬 파이프에만 쓴다.
#[test]
fn process_fixture() {
    use std::io::Write;
    match std::env::var("ZZABBA_SSM_TEST_FIXTURE").as_deref() {
        Ok("both") => {
            let stdout =
                std::thread::spawn(|| io::stdout().write_all(&[b'a'; 256 * 1024]).unwrap());
            io::stderr().write_all(&[b'b'; 48 * 1024]).unwrap();
            stdout.join().unwrap();
        }
        Ok("excess") => {
            let _ = io::stderr().write_all(&[b'b'; STDERR_LIMIT + 1]);
        }
        Ok("wait") => std::thread::sleep(Duration::from_secs(15)),
        Ok("bound-credentials") => {
            assert_eq!(
                std::env::var("AWS_ACCESS_KEY_ID").as_deref(),
                Ok("fixture-key")
            );
            assert_eq!(
                std::env::var("AWS_SECRET_ACCESS_KEY").as_deref(),
                Ok("fixture-secret")
            );
            assert_eq!(
                std::env::var("AWS_SESSION_TOKEN").as_deref(),
                Ok("fixture-token")
            );
            assert_eq!(
                std::env::var("AWS_DEFAULT_REGION").as_deref(),
                Ok("us-west-2")
            );
            assert!(std::env::var_os("AWS_PROFILE").is_none());
            assert!(std::env::var_os("AWS_WEB_IDENTITY_TOKEN_FILE").is_none());
            let config = std::env::var("AWS_CONFIG_FILE").unwrap();
            assert_eq!(config, if cfg!(windows) { "NUL" } else { "/dev/null" });
            let home = std::env::var_os("HOME").unwrap();
            assert_eq!(std::env::var_os("USERPROFILE").as_ref(), Some(&home));
            let home = Path::new(&home);
            assert!(home.is_dir());
            assert!(!home.join(".aws/cli/alias").exists());
            assert!(!home.join(".aws/sso/cache").exists());
        }
        _ => {}
    }
}

fn fixture(mode: &str) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "ssm::cli::tests::process_fixture", "--nocapture"])
        .env("ZZABBA_SSM_TEST_FIXTURE", mode);
    cmd
}

#[tokio::test]
async fn isolated_cli_child_receives_only_bound_credentials() {
    use super::super::{Scope, tests::fixture_credentials};
    let scope = Scope::new("team".into(), "us-west-2".into()).unwrap();
    let mut spec = CommandSpec::aws(&std::env::current_exe().unwrap().into(), &[])
        .with_credentials(&scope, &fixture_credentials());
    spec.args = vec!["--exact".into(), "ssm::cli::tests::process_fixture".into()];
    let mut cmd = spec.command();
    cmd.env("ZZABBA_SSM_TEST_FIXTURE", "bound-credentials");
    let result = capture(cmd, Tool::Aws, LOOKUP_TIMEOUT, &CancellationToken::new()).await;
    assert_eq!(result.unwrap().exit_code, Some(0));
}

#[tokio::test]
async fn reap_waits_for_the_owned_process_to_exit() {
    let mut child = fixture("wait")
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    reap(&mut child).await.unwrap();
    assert_eq!(child.id(), None);
    assert!(child.try_wait().unwrap().is_some());
    reap(&mut child).await.unwrap();
}

#[tokio::test]
async fn child_pipes_limits_deadline_and_in_flight_cancellation() {
    let cancel = CancellationToken::new();
    let result = capture(fixture("both"), Tool::Aws, LOOKUP_TIMEOUT, &cancel)
        .await
        .ok()
        .unwrap();
    assert_eq!(result.exit_code, Some(0));
    assert!(
        result
            .stdout
            .windows(256 * 1024)
            .any(|part| part.iter().all(|b| *b == b'a'))
    );
    assert_eq!(result.stderr, [b'b'; 48 * 1024]);
    assert_eq!(
        capture(fixture("excess"), Tool::Aws, LOOKUP_TIMEOUT, &cancel)
            .await
            .err(),
        Some(CliError::OutputLimit)
    );
    assert_eq!(
        capture(
            fixture("wait"),
            Tool::Aws,
            Duration::from_millis(150),
            &cancel
        )
        .await
        .err(),
        Some(CliError::TimedOut)
    );
    let (result, ()) = tokio::join!(
        capture(fixture("wait"), Tool::Aws, LOOKUP_TIMEOUT, &cancel),
        async {
            tokio::time::sleep(Duration::from_millis(150)).await;
            cancel.cancel()
        },
    );
    assert_eq!(result.err(), Some(CliError::Cancelled));
}
