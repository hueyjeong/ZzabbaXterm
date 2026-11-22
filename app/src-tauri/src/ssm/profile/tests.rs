use super::*;
use crate::ssm::{AwsCli, CancellationToken, OnlineNode, Scope, filter_profiles};

#[test]
fn profile_fixture() {
    if env::var_os("ZZABBA_SSM_PROFILE_FIXTURE").is_none() {
        return;
    }
    let profiles = Profiles::load().unwrap();
    let candidates = b"direct team\r\ndefault\r\nstatic\r\nvia-process\r\nvia-role\r\nweb\r\nshared\r\nbroken\r\nambiguous\r\nmultiline\r\nshared-indent\r\n";
    assert_eq!(
        filter_profiles(&profiles, candidates).unwrap(),
        ["direct team", "default"]
    );
    for name in [
        "static",
        "via-process",
        "via-role",
        "web",
        "shared",
        "broken",
        "ambiguous",
        "multiline",
        "shared-indent",
    ] {
        assert_eq!(profiles.require(name), Err(CliError::UnsupportedProfile));
    }
    assert_eq!(profiles.require("missing"), Err(CliError::ProfileNotFound));
    let scope = Scope::new("via-process".into(), "us-east-1".into()).unwrap();
    let cli = AwsCli(());
    let cancel = CancellationToken::new();
    let node = OnlineNode {
        scope: scope.clone(),
        instance_id: "i-01234567".into(),
    };
    assert_eq!(
        cli.relogin_command(&scope, CliError::SsoLoginRequired)
            .err(),
        Some(CliError::UnsupportedProfile)
    );
    assert_eq!(
        cli.relogin_command(
            &Scope::new("direct team".into(), "us-east-1".into()).unwrap(),
            CliError::SsoLoginRequired
        )
        .unwrap()
        .args()[..4],
        ["sso", "login", "--profile", "direct team"]
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            assert_eq!(
                cli.configured_region(scope.profile(), &cancel).await.err(),
                Some(CliError::UnsupportedProfile)
            );
            assert_eq!(
                cli.online_nodes(&scope, &cancel).await.err(),
                Some(CliError::UnsupportedProfile)
            );
            assert_eq!(
                cli.prepare_start(&scope, &node, &cancel).await.err(),
                Some(CliError::UnsupportedProfile)
            );
        });
}

#[test]
fn selected_profile_is_validated_against_isolated_config_and_shared_credentials() {
    const SSO: &str =
        "sso_account_id = 123456789012\nsso_role_name = Fixture\nsso_session = corp\n";
    let root = env::temp_dir().join(format!("zzabba-profile-fixture-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let config = root.join("config");
    let credentials = root.join("credentials");
    std::fs::write(
        &config,
        format!(
            r#"
[profile "direct team"] # ordinary header comment
{SSO}region = us-west-2
s3 =
  addressing_style = path
[sso-session corp] ; another header comment
sso_start_url = https://example.invalid/start
sso_region = us-east-1
[default]# adjacent header comment
sso_account_id = 123456789012
sso_role_name = Fixture
sso_start_url = https://example.invalid/start
sso_region = us-east-1
[profile static]
{SSO}aws_access_key_id = synthetic
[profile via-process]
{SSO}	region = us-east-1
	s3 =
{nbsp}credential_process = never-run-fixture
[profile via-role]
{SSO}source_profile = default
role_arn = fixture
[profile web]
{SSO}web_identity_token_file = fixture
[profile shared]
{SSO}
[profile broken]
sso_session = absent
[profile ambiguous] unexpected
{SSO}
[profile multiline]
{SSO}region = us-east-1
  another-line
[profile shared-indent]
{SSO}
"#,
            nbsp = '\u{a0}',
        ),
    )
    .unwrap();
    std::fs::write(
        &credentials,
        "[shared] ; synthetic credentials\nAWS_SECRET_ACCESS_KEY = synthetic\n[shared-indent]\n\ts3 =\n\u{2003}AWS_ACCESS_KEY_ID: synthetic\n",
    )
    .unwrap();
    let status = std::process::Command::new(env::current_exe().unwrap())
        .args(["--exact", "ssm::profile::tests::profile_fixture"])
        .env("ZZABBA_SSM_PROFILE_FIXTURE", "1")
        .env("AWS_CONFIG_FILE", &config)
        .env("AWS_SHARED_CREDENTIALS_FILE", &credentials)
        .env("PATH", &root)
        .status();
    std::fs::remove_file(config).unwrap();
    std::fs::remove_file(credentials).unwrap();
    std::fs::remove_dir(root).unwrap();
    assert!(status.unwrap().success());
}

#[test]
fn inherited_ambiguous_and_plugin_configuration_fail_closed() {
    for config in [
        "[DEFAULT]\ncredential_process=never-run\n",
        "[plugins]\ncli_legacy_plugin_path=fixture\n",
        "[profile a]\na=1\nA=2\n",
    ] {
        assert_eq!(
            Profiles::parse(config, "").err(),
            Some(CliError::InvalidConfiguration)
        );
    }
}

#[test]
fn duplicate_aliases_and_ambiguous_sso_sections_reject_only_dependents() {
    let profiles = Profiles::parse(
        "[profile alias]\nsso_session=x\nsso_account_id=123456789012\nsso_role_name=Fixture\n\
         [profile 'alias']\nsso_account_id=123456789012\nsso_role_name=Fixture\n\
         sso_region=us-east-1\nsso_start_url=https://example.invalid\n\
         [profile connected]\nsso_session=x\nsso_account_id=123456789012\nsso_role_name=Fixture\n\
         [sso-session x] trailing text\nsso_region=us-east-1\nsso_start_url=https://example.invalid\n\
         [profile independent]\nsso_account_id=123456789012\nsso_role_name=Fixture\n\
         sso_region=us-east-1\nsso_start_url=https://example.invalid\n",
        "",
    ).unwrap();
    assert_eq!(profiles.require("alias"), Err(CliError::UnsupportedProfile));
    assert_eq!(
        profiles.require("connected"),
        Err(CliError::UnsupportedProfile)
    );
    assert_eq!(profiles.require("independent"), Ok(()));
}
