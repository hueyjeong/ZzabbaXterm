use serde::Deserialize;
use std::{
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const STDOUT_LIMIT: usize = 4 * 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;
const EMPTY_BOTO_CONFIG: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };
// AWS CLI에는 기록을 끄는 환경 변수가 없다. 디렉터리는 SQLite 기록 파일로 열 수 없다.
const HISTORY_PATH: &str = "./.";
pub(super) const HISTORY_WARNING: &[u8] =
    b"Warning: Unable to record CLI history. Check file permissions for ./.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliError {
    AwsCliMissing,
    AwsCliNotV2,
    PluginMissing,
    PluginUpgradeRequired { minimum: &'static str },
    InvalidProfile,
    ProfileNotFound,
    UnsupportedProfile,
    InvalidRegion,
    InvalidTarget,
    CallerChanged,
    InvalidConfiguration,
    CredentialsUnavailable,
    CredentialsExpired,
    SsoLoginRequired,
    SsoLoginNotApplicable,
    AccessDenied,
    TargetNotConnected,
    InvalidSessionDocument,
    EndpointUnavailable,
    InvalidResponse,
    OutputLimit,
    Cancelled,
    TimedOut,
    ProcessIo,
    CommandFailed { exit_code: Option<i32> },
    RandomUnavailable,
}

#[derive(Clone, Copy)]
pub(super) enum Tool {
    Aws,
    Plugin,
}

impl Tool {
    pub(super) fn resolve(self) -> Result<Arc<Path>, CliError> {
        let (name, missing) = match self {
            Self::Aws => ("aws", CliError::AwsCliMissing),
            Self::Plugin => ("session-manager-plugin", CliError::PluginMissing),
        };
        let file = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_owned()
        };
        let path = std::env::var_os("PATH").ok_or(missing)?;
        resolve_on_path(&file, &path).map(Arc::from).ok_or(missing)
    }
}

fn resolve_on_path(file: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .find_map(|dir| {
            let candidate = dir.join(file);
            let metadata = candidate.metadata().ok()?;
            if !metadata.is_file() {
                return None;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 == 0 {
                    return None;
                }
            }
            candidate.canonicalize().ok()
        })
}

pub struct CommandSpec {
    tool: Tool,
    program: Arc<Path>,
    config: Option<(PathBuf, Arc<super::home::CliHome>)>,
    bound: Option<(super::Scope, Arc<super::Credentials>)>,
    args: Vec<String>,
}

impl CommandSpec {
    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn env_remove(&self) -> &'static [&'static str] {
        &[
            "AWS_PROFILE",
            "AWS_DEFAULT_PROFILE",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_SECURITY_TOKEN",
            "AWS_CREDENTIAL_EXPIRATION",
            "AWS_ACCOUNT_ID",
            "AWS_BEARER_TOKEN_SSO",
            "AWS_ROLE_ARN",
            "AWS_ROLE_SESSION_NAME",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
            "AWS_CONTAINER_AUTHORIZATION_TOKEN",
            "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
            "AWS_CREDENTIAL_FILE",
            "AWS_DATA_PATH",
            "AWS_ENDPOINT_URL",
            "AWS_ENDPOINT_URL_SSM",
            "AWS_ENDPOINT_URL_STS",
            "AWS_ENDPOINT_URL_SSO",
            "AWS_ENDPOINT_URL_SSO_OIDC",
            "AWS_ENDPOINT_URL_SIGNIN",
            "BROWSER",
            "AWS_SSM_START_SESSION_RESPONSE",
            "AWS_SSM_PLUGIN_ADDITIONAL_STREAM_HOSTS",
            "AWS_SDK_LOAD_CONFIG",
            "_AWS_CLI_PROFILE_CHAIN",
            "AWS_CLI_FILE_ENCODING",
            "AWS_USE_FIPS_ENDPOINT",
            "AWS_USE_DUALSTACK_ENDPOINT",
            "GODEBUG",
            "SSLKEYLOGFILE",
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "DYLD_INSERT_LIBRARIES",
            "DYLD_LIBRARY_PATH",
            "PYTHONPATH",
            "PYTHONHOME",
        ]
    }

    pub fn env_set(&self) -> &'static [(&'static str, &'static str)] {
        if matches!(self.tool, Tool::Plugin) {
            return &[];
        }
        // URL 재정의를 끄되 프로필 설정 파일과 SSO 캐시는 공식 CLI에 맡긴다.
        &[
            ("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "true"),
            ("AWS_EC2_METADATA_DISABLED", "true"),
            ("BOTO_CONFIG", EMPTY_BOTO_CONFIG),
            ("AWS_CLI_HISTORY_FILE", HISTORY_PATH),
            ("AWS_CLI_ERROR_FORMAT", "enhanced"),
            ("PYTHONIOENCODING", "utf-8"),
            // 한 번의 CLI 실행에서 같은 AWS 요청을 다시 전송할 수 없도록 한다.
            ("AWS_MAX_ATTEMPTS", "1"),
        ]
    }

    pub(super) fn aws(program: &Arc<Path>, args: &[&str]) -> Self {
        let mut args: Vec<_> = args.iter().map(|arg| (*arg).to_owned()).collect();
        args.extend(["--no-cli-pager".into(), "--no-cli-auto-prompt".into()]);
        Self {
            tool: Tool::Aws,
            program: program.clone(),
            config: None,
            bound: None,
            args,
        }
    }

    pub(super) fn version(tool: Tool, program: Arc<Path>) -> Self {
        Self {
            tool,
            program,
            config: None,
            bound: None,
            args: vec!["--version".into()],
        }
    }

    pub(super) fn with_config(mut self, path: &Path, home: Arc<super::home::CliHome>) -> Self {
        self.config = Some((path.into(), home));
        self
    }

    pub(super) fn with_credentials(
        mut self,
        scope: &super::Scope,
        credentials: &Arc<super::Credentials>,
    ) -> Self {
        self.bound = Some((scope.clone(), credentials.clone()));
        self
    }

    pub(super) fn bound_environment(&self, mut set: impl FnMut(&str, &OsStr)) {
        if let Some((path, home)) = &self.config {
            home.env(&mut set);
            super::export_env(path, &mut set);
        }
        if let Some((scope, credentials)) = &self.bound {
            credentials.env(scope.region(), set);
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(self.program());
        cmd.args(self.args());
        for key in self.env_remove() {
            cmd.env_remove(key);
        }
        for (key, value) in self.env_set() {
            cmd.env(key, value);
        }
        self.bound_environment(|key, value| {
            cmd.env(key, value);
        });
        cmd
    }

    pub(super) async fn capture(&self, cancel: &CancellationToken) -> Result<Captured, CliError> {
        self.capture_with_timeout(cancel, LOOKUP_TIMEOUT).await
    }

    pub(super) async fn capture_with_timeout(&self, cancel: &CancellationToken, timeout: Duration) -> Result<Captured, CliError> {
        if matches!(self.tool, Tool::Aws) {
            super::plugin::validate_aws(self.program())?;
        }
        if let Some((_, credentials)) = &self.bound {
            credentials.require_current()?;
        }
        capture(self.command(), self.tool, timeout, cancel).await
    }

    #[cfg(test)]
    pub(super) async fn capture_fixture(&self, cancel: &CancellationToken, timeout: Duration) -> Result<Captured, CliError> {
        capture(self.command(), self.tool, timeout, cancel).await
    }

    pub(super) async fn capture_in(
        &self,
        cwd: &Path,
        cancel: &CancellationToken,
    ) -> Result<Captured, CliError> {
        let mut cmd = self.command();
        cmd.current_dir(cwd);
        capture(cmd, self.tool, LOOKUP_TIMEOUT, cancel).await
    }

    pub(super) async fn login(&self, cancel: &CancellationToken) -> Result<(), CliError> {
        super::plugin::validate_aws(self.program())?;
        capture(self.command(), self.tool, LOGIN_TIMEOUT, cancel)
            .await?
            .success()?;
        Ok(())
    }
}

// CLI 본문과 원본 오류는 모듈 밖의 진단이나 Debug 출력으로 전달하지 않는다.
pub(super) struct Captured {
    pub(super) exit_code: Option<i32>,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

impl Captured {
    pub(super) fn success(self) -> Result<Vec<u8>, CliError> {
        match self.exit_code {
            Some(0) => Ok(self.stdout),
            code => Err(classify(code, &self.stderr)),
        }
    }
}

async fn capture(
    mut cmd: Command,
    tool: Tool,
    time_limit: Duration,
    cancel: &CancellationToken,
) -> Result<Captured, CliError> {
    if cancel.is_cancelled() {
        return Err(CliError::Cancelled);
    }

    let mut child = cmd
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| match (err.kind(), tool) {
            (io::ErrorKind::NotFound, Tool::Aws) => CliError::AwsCliMissing,
            (io::ErrorKind::NotFound, Tool::Plugin) => CliError::PluginMissing,
            _ => CliError::ProcessIo,
        })?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let read_result = async {
        let (status, stdout, stderr) = tokio::try_join!(
            async { child.wait().await.map_err(|_| CliError::ProcessIo) },
            read_limited(stdout, STDOUT_LIMIT),
            read_limited(stderr, STDERR_LIMIT),
        )?;
        Ok(Captured {
            exit_code: status.code(),
            stdout,
            stderr,
        })
    };

    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(CliError::Cancelled),
        result = tokio::time::timeout(time_limit, read_result) => {
            result.unwrap_or(Err(CliError::TimedOut))
        }
    };
    if result.is_err() {
        reap(&mut child).await?;
    }
    result
}

async fn reap(child: &mut tokio::process::Child) -> Result<(), CliError> {
    // 공식 onedir CLI는 단일 프로세스다. 프로필 도우미는 차단하고 외부 브라우저는 그대로 둔다.
    if child.try_wait().map_err(|_| CliError::ProcessIo)?.is_none()
        && child.kill().await.is_err()
        && child.try_wait().map_err(|_| CliError::ProcessIo)?.is_none()
    {
        return Err(CliError::ProcessIo);
    }
    Ok(())
}

async fn read_limited(pipe: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>, CliError> {
    let mut bytes = Vec::new();
    pipe.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| CliError::ProcessIo)?;
    if bytes.len() > limit {
        return Err(CliError::OutputLimit);
    }
    Ok(bytes)
}

#[derive(Deserialize)]
struct ErrorCode<'a> {
    #[serde(rename = "Code", borrow)]
    code: &'a str,
}

fn classify(exit_code: Option<i32>, stderr: &[u8]) -> CliError {
    use CliError::*;
    let failed = CommandFailed { exit_code };
    let Ok(stderr) = std::str::from_utf8(stderr) else {
        return failed;
    };
    let stderr = stderr.trim();
    let message = stderr
        .lines()
        .find_map(|line| line.strip_prefix("aws: [ERROR]: "))
        .or_else(|| {
            stderr
                .lines()
                .find(|line| line.starts_with("An error occurred ("))
        })
        .or_else(|| stderr.lines().last())
        .unwrap_or_default();
    let message = message
        .strip_prefix("Unable to retrieve credentials: ")
        .unwrap_or(message);
    let code = serde_json::from_str::<ErrorCode<'_>>(stderr)
        .ok()
        .map(|error| error.code)
        .or_else(|| {
            message
                .strip_prefix("An error occurred (")?
                .split_once(')')
                .map(|(code, _)| code)
        });
    match code {
        Some("AccessDenied" | "AccessDeniedException" | "UnauthorizedOperation") => AccessDenied,
        Some("TargetNotConnected") => TargetNotConnected,
        Some("ExpiredToken" | "ExpiredTokenException") => CredentialsExpired,
        Some("NoRegion" | "InvalidRegion") => InvalidRegion,
        Some("NoCredentials" | "InvalidClientTokenId" | "UnrecognizedClientException") => {
            CredentialsUnavailable
        }
        Some("ProfileNotFound") => ProfileNotFound,
        Some("Configuration") => InvalidConfiguration,
        Some(_) => failed,
        None if message.starts_with("The config profile (")
            && message.ends_with(") could not be found") =>
        {
            ProfileNotFound
        }
        None if message.starts_with("You must specify a region.")
            || (message.starts_with("Provided region_name '")
                && message.ends_with("' doesn't match a supported format.")) =>
        {
            InvalidRegion
        }
        None if message == "Unable to locate credentials" => CredentialsUnavailable,
        None if message.starts_with("Unable to parse config file:")
            || message.starts_with("Missing the following required SSO configuration values:") =>
        {
            InvalidConfiguration
        }
        None if message.starts_with("Could not connect to the endpoint URL:") => {
            EndpointUnavailable
        }
        None if message == "no credentials found" => CredentialsUnavailable,
        None if message
            == "Error when retrieving token from sso: Token has expired and refresh failed"
            || message
                == "The SSO session associated with this profile has expired or is otherwise invalid. To refresh this SSO session run aws sso login with the corresponding profile."
            || (message.starts_with("Error loading SSO Token: Token for ")
                && (message.ends_with(" does not exist") || message.ends_with(" is invalid"))) =>
        {
            SsoLoginRequired
        }
        None => failed,
    }
}

#[cfg(test)]
mod tests;
