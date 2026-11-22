mod cli;
pub mod ipc;
mod profile;
mod session;

pub use cli::{CliError, CommandSpec};
pub use session::{SessionLookup, StartRequest, VerificationError, VerifiedSession};
pub use tokio_util::sync::CancellationToken;

use cli::{Captured, Tool};
use profile::Profiles;
use serde::{Deserialize, de::DeserializeOwned};
use std::sync::Arc;

pub struct AwsCli(());

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

    fn command(&self, service: &str, action: &str, extras: &[&str]) -> CommandSpec {
        let mut args = vec![
            service,
            action,
            "--profile",
            &self.profile,
            "--region",
            &self.region,
            "--output",
            "json",
        ];
        args.extend_from_slice(extras);
        CommandSpec::aws(&args)
    }
}

impl AwsCli {
    pub async fn probe(cancel: &CancellationToken) -> Result<Self, CliError> {
        Profiles::load()?;
        let result = CommandSpec::version(Tool::Aws).capture(cancel).await?;
        if !result.stdout.starts_with(b"aws-cli/2.") && !result.stderr.starts_with(b"aws-cli/2.") {
            result.success()?;
            return Err(CliError::AwsCliNotV2);
        }
        result.success()?;
        Ok(Self(()))
    }

    pub async fn check_plugin(&self, cancel: &CancellationToken) -> Result<(), CliError> {
        CommandSpec::version(Tool::Plugin)
            .capture(cancel)
            .await?
            .success()?;
        Ok(())
    }

    pub async fn profiles(&self, cancel: &CancellationToken) -> Result<Vec<String>, CliError> {
        let profiles = Profiles::load()?;
        let output = profiles_command().capture(cancel).await?.success()?;
        filter_profiles(&profiles, &output)
    }

    pub async fn configured_region(
        &self,
        profile: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, CliError> {
        validate_profile(profile)?;
        require_direct_profile(profile)?;
        parse_region(region_command(profile).capture(cancel).await?)
    }

    pub async fn online_nodes(
        &self,
        scope: &Scope,
        cancel: &CancellationToken,
    ) -> Result<Vec<OnlineNode>, CliError> {
        require_direct_profile(scope.profile())?;
        parse_online_nodes(scope, nodes_command(scope).capture(cancel).await?)
    }

    pub fn relogin_command(&self, scope: &Scope, cause: CliError) -> Result<CommandSpec, CliError> {
        if cause != CliError::SsoLoginRequired {
            return Err(CliError::SsoLoginNotApplicable);
        }
        require_direct_profile(scope.profile())?;
        Ok(login_command(scope.profile()))
    }

    pub async fn relogin(
        &self,
        scope: &Scope,
        cause: CliError,
        cancel: &CancellationToken,
    ) -> Result<(), CliError> {
        self.relogin_command(scope, cause)?.login(cancel).await
    }
}

fn filter_profiles(profiles: &Profiles, output: &[u8]) -> Result<Vec<String>, CliError> {
    Ok(text(output)?
        .lines()
        .filter(|line| validate_profile(line).is_ok() && profiles.require(line).is_ok())
        .map(str::to_owned)
        .collect())
}
fn require_direct_profile(profile: &str) -> Result<(), CliError> {
    Profiles::load()?.require(profile)
}

fn login_command(profile: &str) -> CommandSpec {
    // 로그인 리전은 관리 노드 리전이 아니라 프로필의 IAM Identity Center 설정을 따른다.
    CommandSpec::aws(&["sso", "login", "--profile", profile])
}

fn profiles_command() -> CommandSpec {
    CommandSpec::aws(&["configure", "list-profiles"])
}

fn region_command(profile: &str) -> CommandSpec {
    CommandSpec::aws(&["configure", "get", "region", "--profile", profile])
}

fn nodes_command(scope: &Scope) -> CommandSpec {
    scope.command("ssm", "describe-instance-information", &[])
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

fn parse_online_nodes(scope: &Scope, captured: Captured) -> Result<Vec<OnlineNode>, CliError> {
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
            });
        }
    }
    Ok(nodes)
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
