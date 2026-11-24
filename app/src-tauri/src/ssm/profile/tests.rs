use super::*;
use crate::ssm::{CancellationToken, OnlineNode, Scope, filter_profiles, tests::fixture_cli};

fn sso(profiles: &Profiles, name: &str) -> DirectSsoConfig {
    match profiles.auth(name).unwrap() {
        ProfileAuth::Sso(config) => config,
        ProfileAuth::Login(_) => panic!("SSO 프로필이어야 한다"),
    }
}

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
    assert_eq!(
        sso(&profiles, "direct team").cli_ini(),
        "[default]\nsso_account_id = 123456789012\nsso_role_name = Fixture\nsso_session = corp\n[sso-session \"corp\"]\nsso_region = us-east-1\nsso_start_url = https://example.invalid/start\n"
    );
    assert_eq!(
        sso(&profiles, "direct team").cache_key(),
        "corp"
    );
    assert_eq!(
        sso(&profiles, "default").cache_key(),
        "https://example.invalid/start"
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
    let cli = fixture_cli();
    let cancel = CancellationToken::new();
    let node = OnlineNode {
        scope: scope.clone(),
        instance_id: "i-01234567".into(),
        owner: "fixture-owner".into(),
        role: None,
        credentials: crate::ssm::tests::fixture_credentials(),
        program: cli.0.clone(),
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
            assert_eq!(cli.prepare_start(&scope, &node, &cancel).await.err(), None);
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

#[test]
fn aws_login_profiles_export_only_their_login_session() {
    let root = "arn:aws:iam::123456789012:root";
    let config = [
        format!("[default]\nlogin_session = {root}\nregion = ap-northeast-1\n"),
        "[profile no-region]\nlogin_session = arn:aws:sts::123456789012:assumed-role/Admin/dev@example.com\n".into(),
        format!("[profile with-keys]\nlogin_session = {root}\n"),
        format!("[profile with-sso]\nlogin_session = {root}\nsso_session = corp\n"),
        format!("[profile with-role]\nlogin_session = {root}\nrole_arn = arn:aws:iam::123456789012:role/x\n"),
        "[profile not-arn]\nlogin_session = root\n".into(),
        "[profile spaced]\nlogin_session = arn:aws:iam::1 :root\n".into(),
        format!("[profile bad-region]\nlogin_session = {root}\nregion = not a region\n"),
    ]
    .concat();
    let mut profiles = Profiles::parse(&config, "[with-keys]\naws_access_key_id = synthetic\n").unwrap();
    let ProfileAuth::Login(default) = profiles.auth("default").unwrap() else { panic!("aws login 프로필") };
    assert_eq!((default.session.as_str(), default.region.as_deref()), (root, Some("ap-northeast-1")));
    assert_eq!(
        ProfileAuth::Login(default.clone()).cli_ini_for("default", "us-west-2"),
        format!("[default]\nlogin_session = {root}\nregion = ap-northeast-1\n")
    );
    let ProfileAuth::Login(no_region) = profiles.auth("no-region").unwrap() else { panic!("aws login 프로필") };
    // 갱신에 필요한 리전은 대상 리전으로 채운다.
    assert_eq!(
        ProfileAuth::Login(no_region).cli_ini_for("no-region", "us-west-2"),
        "[profile \"no-region\"]\nlogin_session = arn:aws:sts::123456789012:assumed-role/Admin/dev@example.com\nregion = us-west-2\n"
    );
    assert_eq!(ProfileAuth::Login(default).lock_key(), root);
    for name in ["with-keys", "with-sso", "with-role", "not-arn", "spaced", "bad-region"] {
        assert_eq!(profiles.require(name), Err(CliError::UnsupportedProfile), "{name}");
    }
    // S3처럼 SSO 토큰을 직접 쓰는 기능은 aws login 프로필을 받지 않는다.
    assert_eq!(profiles.take_sso_config("default").err(), Some(CliError::UnsupportedProfile));
}
