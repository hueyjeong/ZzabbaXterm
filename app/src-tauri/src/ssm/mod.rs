mod aws_install;
mod cli;
mod credentials;
mod home;
pub mod ipc;
mod plugin;
mod profile;
mod session;
pub(crate) mod target_login;
#[cfg(windows)]
pub(crate) mod windows_acl;

pub use cli::{CliError, CommandSpec};
pub(crate) use home::{sso_cache_entry, user_home};
pub(crate) use profile::{DirectSsoConfig, ProfileAuth};
pub use session::{SessionLookup, StartPermit, StartRequest, VerificationError, VerifiedSession};
pub(crate) use session::{AssumedRole, ForwardGuard, ForwardPermit, SessionDocument, start_rejection, valid_session_id};
#[cfg(test)]
pub(crate) use session::fixture_start;
pub use tokio_util::sync::CancellationToken;

use cli::{Captured, Tool};
use credentials::{Credentials, export_env};
use profile::Profiles;
use serde::{Deserialize, de::DeserializeOwned};
use std::{path::Path, sync::Arc};

/// 실제 노드 시험 전용이다. 이 프로필은 앱이 지원하는 프로필이 아니어도 사용자 AWS 설정으로 자격만 내보내고,
/// 신원 확인·노드 조회·세션 시작과 종료는 앱 경로를 그대로 쓴다.
#[cfg(test)]
pub(crate) static LIVE_EXPORT_PROFILE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// 두 번째 값은 `aws login` 프로필 지원 여부(AWS CLI 2.32.0 이상)다.
pub struct AwsCli(Arc<Path>, bool);

const LOGIN_CLI_MINIMUM: &str = "2.32.0";

#[derive(Clone, Eq, PartialEq)]
pub struct Scope {
    profile: Arc<str>,
    region: Arc<str>,
}

impl Scope {
    pub fn new(profile: String, region: String) -> Result<Self, CliError> {
        validate_profile(&profile)?;
        if !valid_region(&region) {
            return Err(CliError::InvalidRegion);
        }
        Ok(Self {
            profile: profile.into(),
            region: region.into(),
        })
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    pub(crate) fn direct_sso_config(&self) -> Result<DirectSsoConfig, CliError> {
        Profiles::load()?.take_sso_config(self.profile())
    }

    pub(crate) fn auth(&self) -> Result<ProfileAuth, CliError> {
        Profiles::load()?.auth(self.profile())
    }

    fn command(
        &self,
        program: &Arc<Path>,
        service: &str,
        action: &str,
        extras: &[&str],
    ) -> CommandSpec {
        let mut args = vec![
            service,
            action,
            "--region",
            &self.region,
            "--output",
            "json",
        ];
        args.extend_from_slice(extras);
        CommandSpec::aws(program, &args)
    }
}

impl AwsCli {
    pub async fn probe(cancel: &CancellationToken) -> Result<Self, CliError> {
        Profiles::load()?;
        let program = Tool::Aws.resolve()?;
        let result = CommandSpec::version(Tool::Aws, program.clone())
            .capture(cancel)
            .await?;
        let version = [&result.stdout, &result.stderr].into_iter().find_map(|output| {
            std::str::from_utf8(output)
                .ok()
                .and_then(|line| line.trim().strip_prefix("aws-cli/"))
                .and_then(|line| line.split_ascii_whitespace().next())
                .and_then(plugin::numeric_version)
                .filter(|version| version[0] == 2 && *version >= [2, 31, 0, 0])
        });
        result.success()?;
        let Some(version) = version else {
            return Err(CliError::AwsCliNotV2);
        };
        Ok(Self(program, version >= [2, 32, 0, 0]))
    }

    /// `aws login` 프로필은 `login_session`을 아는 CLI에서만 내보내거나 로그인할 수 있다.
    fn require_auth_support(&self, auth: &ProfileAuth) -> Result<(), CliError> {
        match auth {
            ProfileAuth::Login(_) if !self.1 => Err(CliError::AwsCliUpgradeRequired { minimum: LOGIN_CLI_MINIMUM }),
            _ => Ok(()),
        }
    }

    pub async fn check_plugin(&self, cancel: &CancellationToken) -> Result<(), CliError> {
        let home = home::CliHome::new()?;
        let cwd = home.path();
        plugin::checked_path(&self.0, cwd, cancel).await?;
        Ok(())
    }

    pub async fn profiles(&self, cancel: &CancellationToken) -> Result<Vec<String>, CliError> {
        let profiles = Profiles::load()?;
        let output = self.profiles_command().capture(cancel).await?.success()?;
        // `aws login`을 모르는 CLI에서는 login 프로필을 고를 수 없게 목록에서 뺀다.
        filter_profiles(&profiles, &output, self.1)
    }

    pub async fn configured_region(
        &self,
        profile: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, CliError> {
        validate_profile(profile)?;
        require_supported_profile(profile)?;
        parse_region(self.region_command(profile).capture(cancel).await?)
    }

    pub async fn online_nodes(
        &self,
        scope: &Scope,
        cancel: &CancellationToken,
    ) -> Result<Vec<OnlineNode>, CliError> {
        #[cfg(test)]
        if LIVE_EXPORT_PROFILE.get().is_some_and(|live| live == scope.profile()) {
            return self.online_nodes_with(scope, "", Arc::new(home::CliHome::new()?), |spec| async move {
                if spec.args().get(1).is_some_and(|arg| arg == "export-credentials") {
                    let args = ["configure", "export-credentials", "--profile", scope.profile(), "--format", "process"];
                    return CommandSpec::aws(&self.0, &args).capture(cancel).await;
                }
                spec.capture(cancel).await
            }).await;
        }
        let auth = scope.auth()?;
        self.require_auth_support(&auth)?;
        let home = export_home(&auth, cancel).await?;
        self.online_nodes_with(scope, &auth.cli_ini_for("default", scope.region()), Arc::new(home), |spec| async move {
            spec.capture(cancel).await
        })
        .await
    }

    /// 프로필 자격을 내보내고 STS 신원 확인만 한다. 노드 조회는 하지 않는다.
    pub(crate) async fn check_identity(
        &self,
        scope: &Scope,
        auth: &ProfileAuth,
        cancel: &CancellationToken,
    ) -> Result<(), CliError> {
        self.require_auth_support(auth)?;
        let home = export_home(auth, cancel).await?;
        let mut capture = |spec: CommandSpec| async move { spec.capture(cancel).await };
        self.caller_with(scope, &auth.cli_ini_for("default", scope.region()), Arc::new(home), &mut capture).await.map(drop)
    }

    // Tauri 작업은 캡처 클로저를 빌리지 않는 Send 퓨처를 사용한다.
    async fn online_nodes_with<F>(
        &self,
        scope: &Scope,
        config: &str,
        home: Arc<home::CliHome>,
        mut capture: impl FnMut(CommandSpec) -> F + Send,
    ) -> Result<Vec<OnlineNode>, CliError>
    where
        F: Future<Output = Result<Captured, CliError>> + Send,
    {
        let (credentials, owner, role) = self.caller_with(scope, config, home, &mut capture).await?;
        let result = capture(
            self.nodes_command(scope)
                .with_credentials(scope, &credentials),
        )
        .await?;
        self.parse_online_nodes(scope, owner, role, credentials, result)
    }

    async fn caller_with<F>(
        &self,
        scope: &Scope,
        config: &str,
        home: Arc<home::CliHome>,
        capture: &mut (impl FnMut(CommandSpec) -> F + Send),
    ) -> Result<(Arc<Credentials>, Arc<str>, Option<Arc<AssumedRole>>), CliError>
    where
        F: Future<Output = Result<Captured, CliError>> + Send,
    {
        use std::io::Write;

        let mut snapshot =
            tempfile::NamedTempFile::new_in(home.path()).map_err(|_| CliError::ProcessIo)?;
        snapshot
            .write_all(config.as_bytes())
            .map_err(|_| CliError::ProcessIo)?;
        let export = CommandSpec::aws(
            &self.0,
            &[
                "configure",
                "export-credentials",
                "--profile",
                "default",
                "--format",
                "process",
            ],
        )
        .with_config(snapshot.path(), home.clone());
        let credentials = Arc::new(Credentials::parse(capture(export).await?)?);
        drop(snapshot);
        // 마지막 참조이므로 `aws login` 갱신 잠금도 여기서 풀린다. 신원 조회는 잠금 밖에서 한다.
        drop(home);
        let identity = session::identity_command(&self.0, scope, &credentials);
        let (owner, role) = session::captured_caller(capture(identity).await?)?;
        Ok((credentials, owner, role))
    }

    #[cfg(test)]
    pub fn relogin_command(&self, scope: &Scope, cause: CliError) -> Result<CommandSpec, CliError> {
        if cause != CliError::SsoLoginRequired {
            return Err(CliError::SsoLoginNotApplicable);
        }
        Ok(self.login_command(scope, &scope.auth()?))
    }

    pub async fn relogin(
        &self,
        scope: &Scope,
        cause: CliError,
        cancel: &CancellationToken,
    ) -> Result<(), CliError> {
        if cause != CliError::SsoLoginRequired {
            return Err(CliError::SsoLoginNotApplicable);
        }
        self.relogin_saved(scope, &scope.auth()?, cancel).await
    }

    pub(crate) async fn relogin_saved(
        &self, scope: &Scope, approved: &ProfileAuth, cancel: &CancellationToken,
    ) -> Result<(), CliError> {
        use std::io::Write;

        if scope.auth()? != *approved { return Err(CliError::CallerChanged); }
        self.require_auth_support(approved)?;
        let mut snapshot = tempfile::NamedTempFile::new().map_err(|_| CliError::ProcessIo)?;
        snapshot.write_all(approved.cli_ini_for(scope.profile(), scope.region()).as_bytes()).map_err(|_| CliError::ProcessIo)?;
        let cache = match approved {
            ProfileAuth::Sso(_) => None,
            ProfileAuth::Login(_) => Some(profile::login_cache_dir()?),
        };
        self.login_command(scope, approved).login_with_config(snapshot.path(), cache.as_deref(), cancel).await
    }

    fn login_command(&self, scope: &Scope, auth: &ProfileAuth) -> CommandSpec {
        let profile = scope.profile();
        match auth {
            // 로그인 리전은 관리 노드 리전이 아니라 프로필의 IAM Identity Center 설정을 따른다.
            ProfileAuth::Sso(_) => CommandSpec::aws(&self.0, &["sso", "login", "--profile", profile]),
            // 리전을 넘겨 CLI의 대화형 리전 질문을 막는다. 다른 계정으로 로그인하면 CLI가 덮어쓰기를 묻고,
            // 입력이 닫혀 있으므로 실패한다.
            ProfileAuth::Login(config) => CommandSpec::aws(
                &self.0, &["login", "--profile", profile, "--region", config.login_region(scope.region())],
            ),
        }
    }

    fn profiles_command(&self) -> CommandSpec {
        CommandSpec::aws(&self.0, &["configure", "list-profiles"])
    }

    fn region_command(&self, profile: &str) -> CommandSpec {
        CommandSpec::aws(
            &self.0,
            &["configure", "get", "region", "--profile", profile],
        )
    }

    fn nodes_command(&self, scope: &Scope) -> CommandSpec {
        scope.command(&self.0, "ssm", "describe-instance-information", &[])
    }
}

fn filter_profiles(profiles: &Profiles, output: &[u8], login: bool) -> Result<Vec<String>, CliError> {
    Ok(text(output)?
        .lines()
        .filter(|line| validate_profile(line).is_ok() && profiles.require(line).is_ok())
        .filter(|line| login || !profiles.is_login(line))
        .map(str::to_owned)
        .collect())
}
fn require_supported_profile(profile: &str) -> Result<(), CliError> {
    Profiles::load()?.require(profile)
}

/// 내보내기용 격리 홈. SSO는 선택한 토큰만 복사하고, `aws login`은 사용자 로그인 캐시를 가리킨다.
///
/// `aws login` 갱신은 갱신 토큰을 바꿔 캐시에 다시 쓴다. 앱 안의 내보내기끼리 같은 토큰으로 동시에
/// 갱신하지 않도록 login_session별 잠금을 홈에 담는다. 잠금은 내보내기가 끝나 홈을 버릴 때 풀린다.
async fn export_home(auth: &ProfileAuth, cancel: &CancellationToken) -> Result<home::CliHome, CliError> {
    let mut home = home::CliHome::new()?;
    match auth {
        ProfileAuth::Sso(config) => home.import_sso_token(&home::user_home()?, config.cache_key())?,
        ProfileAuth::Login(config) => {
            let lock = login_refresh_lock(&config.session);
            let guard = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(CliError::Cancelled),
                guard = lock.lock_owned() => guard,
            };
            home.use_login_cache(profile::login_cache_dir()?, guard);
        }
    }
    Ok(home)
}

fn login_refresh_lock(session: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        std::sync::LazyLock::new(Default::default);
    LOCKS.lock().unwrap().entry(session.to_owned()).or_default().clone()
}

pub(crate) fn validate_profile(profile: &str) -> Result<(), CliError> {
    if profile.is_empty()
        || profile.trim() != profile
        || profile.starts_with('-')
        || profile.chars().any(char::is_control)
    {
        return Err(CliError::InvalidProfile);
    }
    Ok(())
}

pub(crate) fn valid_region(region: &str) -> bool {
    let (Some(first), Some(last)) = (region.as_bytes().first(), region.as_bytes().last()) else {
        return false;
    };
    let alphanumeric = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    region.len() <= 63
        && alphanumeric(*first)
        && alphanumeric(*last)
        && region.bytes().all(|b| alphanumeric(b) || b == b'-')
}

fn parse_region(captured: Captured) -> Result<Option<String>, CliError> {
    // `configure get`은 키가 없을 때 본문과 오류 없이 1을 반환한다.
    if captured.exit_code == Some(1)
        && captured.stdout.is_empty()
        && (captured.stderr.is_empty()
            || captured
                .stderr
                .strip_suffix(b"\r\n")
                .or_else(|| captured.stderr.strip_suffix(b"\n"))
                == Some(cli::HISTORY_WARNING))
    {
        return Ok(None);
    }
    let stdout = captured.success()?;
    let region = text(&stdout)?.trim_end_matches(['\r', '\n']);
    if !valid_region(region) {
        return Err(CliError::InvalidRegion);
    }
    Ok(Some(region.to_owned()))
}

pub struct OnlineNode {
    scope: Scope,
    instance_id: String,
    owner: Arc<str>,
    role: Option<Arc<AssumedRole>>,
    credentials: Arc<Credentials>,
    program: Arc<Path>,
}

impl OnlineNode {
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InstanceInformation {
    instance_information_list: Vec<InstanceRecord>,
    next_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InstanceRecord {
    instance_id: String,
    ping_status: PingStatus,
}

#[derive(Deserialize)]
enum PingStatus {
    Online,
    ConnectionLost,
    Inactive,
}

impl AwsCli {
    fn parse_online_nodes(
        &self,
        scope: &Scope,
        owner: Arc<str>,
        role: Option<Arc<AssumedRole>>,
        credentials: Arc<Credentials>,
        captured: Captured,
    ) -> Result<Vec<OnlineNode>, CliError> {
        let inventory: InstanceInformation = decode(&captured.success()?)?;
        if inventory.next_token.is_some() {
            return Err(CliError::InvalidResponse);
        }
        let mut nodes = Vec::new();
        for node in inventory.instance_information_list {
            if !valid_instance_id(&node.instance_id) {
                return Err(CliError::InvalidResponse);
            }
            if matches!(node.ping_status, PingStatus::Online) {
                nodes.push(OnlineNode {
                    scope: scope.clone(),
                    instance_id: node.instance_id,
                    owner: owner.clone(),
                    role: role.clone(),
                    credentials: credentials.clone(),
                    program: self.0.clone(),
                });
            }
        }
        Ok(nodes)
    }
}

pub(crate) fn valid_instance_id(id: &str) -> bool {
    let suffix = if let Some(suffix) = id.strip_prefix("mi-") {
        (suffix.len() == 17).then_some(suffix)
    } else {
        id.strip_prefix("i-")
            .filter(|suffix| matches!(suffix.len(), 8 | 17))
    };
    suffix.is_some_and(|suffix| {
        suffix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    })
}

fn text(bytes: &[u8]) -> Result<&str, CliError> {
    std::str::from_utf8(bytes).map_err(|_| CliError::InvalidResponse)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CliError> {
    serde_json::from_slice(bytes).map_err(|_| CliError::InvalidResponse)
}

#[cfg(test)]
mod tests;
