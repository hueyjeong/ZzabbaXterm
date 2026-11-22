use super::{
    AwsCli, CancellationToken, Captured, CliError, CommandSpec, OnlineNode, Scope, decode,
};
use portable_pty::CommandBuilder;
use serde::Deserialize;

// 각 요청을 앱 세션에 보관하고 start-session은 요청당 한 번만 호출한다.
pub struct StartRequest {
    scope: Scope,
    target: String,
    reason: String,
    owner: String,
    command_issued: bool,
    termination_id: Option<String>,
}

#[derive(Clone, Copy)]
pub enum SessionLookup {
    Active,
    History,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationError {
    Unconfirmed,
}

pub struct VerifiedSession {
    session_id: String,
    status: SessionStatus,
}

impl VerifiedSession {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn is_terminated(&self) -> bool {
        self.status == SessionStatus::Terminated
    }
}

impl AwsCli {
    pub async fn prepare_start(
        &self,
        scope: &Scope,
        node: &OnlineNode,
        cancel: &CancellationToken,
    ) -> Result<StartRequest, CliError> {
        prepare_start_with(scope, node, async |spec| {
            super::require_direct_profile(scope.profile())?;
            spec.capture(cancel).await
        })
        .await
    }

    pub async fn verify_session(
        &self,
        request: &StartRequest,
        current_scope: &Scope,
        lookup: SessionLookup,
        cancel: &CancellationToken,
    ) -> Result<VerifiedSession, VerificationError> {
        use VerificationError::Unconfirmed;
        request.require_issued_scope(current_scope)?;
        super::require_direct_profile(current_scope.profile()).map_err(|_| Unconfirmed)?;
        let caller = caller_arn(current_scope, async |spec| spec.capture(cancel).await)
            .await
            .map_err(|_| Unconfirmed)?;
        if caller != request.owner {
            return Err(Unconfirmed);
        }
        let result = request.lookup_command(lookup, None).capture(cancel).await;
        request.find_session(result)?.ok_or(Unconfirmed)
    }

    pub async fn close_session(
        &self,
        request: &mut StartRequest,
        current_scope: &Scope,
        cancel: &CancellationToken,
    ) -> Result<(), VerificationError> {
        request.require_issued_scope(current_scope)?;
        super::require_direct_profile(current_scope.profile())
            .map_err(|_| VerificationError::Unconfirmed)?;
        request
            .close_with(current_scope, async |spec| spec.capture(cancel).await)
            .await
    }
}

impl StartRequest {
    // 반환된 명령은 앱 세션을 소유하는 PTY에 한 번만 넘긴다.
    pub fn take_start_command(&mut self) -> Result<Option<CommandBuilder>, CliError> {
        if self.command_issued {
            return Ok(None);
        }
        super::require_direct_profile(self.scope.profile())?;
        let cwd = std::env::current_dir().map_err(|_| CliError::ProcessIo)?;
        Ok(self.issue_start_command(&cwd))
    }

    fn issue_start_command(&mut self, cwd: &std::path::Path) -> Option<CommandBuilder> {
        if std::mem::replace(&mut self.command_issued, true) {
            return None;
        }
        let spec = self.scope.command(
            "ssm",
            "start-session",
            &["--target", &self.target, "--reason", &self.reason],
        );
        let mut command = CommandBuilder::new(if cfg!(windows) {
            "aws.exe"
        } else {
            spec.program()
        });
        command.args(spec.args());
        command.cwd(cwd);
        apply_start_environment(&mut command, &spec, std::env::vars_os());
        Some(command)
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    fn require_issued_scope(&self, current_scope: &Scope) -> Result<(), VerificationError> {
        if self.command_issued && *current_scope == self.scope {
            Ok(())
        } else {
            Err(VerificationError::Unconfirmed)
        }
    }

    fn lookup_command(&self, lookup: SessionLookup, session_id: Option<&str>) -> CommandSpec {
        let state = match lookup {
            SessionLookup::Active => "Active",
            SessionLookup::History => "History",
        };
        let filter = if let Some(id) = session_id {
            serde_json::json!([{"key": "SessionId", "value": id}]).to_string()
        } else {
            format!("key=Target,value={}", self.target)
        };
        self.scope.command(
            "ssm",
            "describe-sessions",
            &["--state", state, "--filters", &filter],
        )
    }

    async fn close_with(
        &mut self,
        current_scope: &Scope,
        mut capture: impl AsyncFnMut(CommandSpec) -> Result<Captured, CliError>,
    ) -> Result<(), VerificationError> {
        use VerificationError::Unconfirmed;
        self.require_issued_scope(current_scope)?;
        if caller_arn(current_scope, &mut capture)
            .await
            .map_err(|_| Unconfirmed)?
            != self.owner
        {
            return Err(Unconfirmed);
        }
        if let Some(id) = self.termination_id.as_deref() {
            return self.confirm_terminated(id, &mut capture).await;
        }
        let history =
            self.find_session(capture(self.lookup_command(SessionLookup::History, None)).await)?;
        if let Some(history) = history {
            return history.is_terminated().then_some(()).ok_or(Unconfirmed);
        }
        let active = self
            .find_session(capture(self.lookup_command(SessionLookup::Active, None)).await)?
            .filter(|session| session.status.can_terminate())
            .ok_or(Unconfirmed)?;

        let id = self.termination_id.insert(active.session_id).as_str();
        let result = capture(
            self.scope
                .command("ssm", "terminate-session", &["--session-id", id]),
        )
        .await
        .and_then(Captured::success);
        match result {
            Ok(output) => {
                if decode::<TerminationResponse>(&output)
                    .map_err(|_| Unconfirmed)?
                    .session_id
                    != id
                {
                    return Err(Unconfirmed);
                }
            }
            // 응답 유실은 같은 세션의 History만 다시 읽어 판정한다.
            Err(CliError::TimedOut | CliError::ProcessIo | CliError::OutputLimit) => {}
            Err(_) => return Err(Unconfirmed),
        }
        self.confirm_terminated(
            self.termination_id.as_deref().ok_or(Unconfirmed)?,
            &mut capture,
        )
        .await
    }

    async fn confirm_terminated(
        &self,
        id: &str,
        mut capture: impl AsyncFnMut(CommandSpec) -> Result<Captured, CliError>,
    ) -> Result<(), VerificationError> {
        self.find_session(capture(self.lookup_command(SessionLookup::History, Some(id))).await)?
            .filter(|session| session.session_id() == id && session.is_terminated())
            .map(|_| ())
            .ok_or(VerificationError::Unconfirmed)
    }

    fn find_session(
        &self,
        result: Result<Captured, CliError>,
    ) -> Result<Option<VerifiedSession>, VerificationError> {
        use VerificationError::Unconfirmed;
        let output = result
            .and_then(Captured::success)
            .map_err(|_| Unconfirmed)?;
        let inventory: SessionInventory = decode(&output).map_err(|_| Unconfirmed)?;
        if inventory.next_token.is_some() {
            return Err(Unconfirmed);
        }
        let mut verified = None;
        for record in &inventory.sessions {
            if record.reason.as_deref() != Some(self.reason.as_str()) {
                continue;
            }
            if record.owner.as_deref() != Some(self.owner.as_str())
                || record.target.as_deref() != Some(self.target.as_str())
                || verified.is_some()
            {
                return Err(Unconfirmed);
            }
            let id = record
                .session_id
                .as_deref()
                .filter(|id| valid_session_id(id))
                .ok_or(Unconfirmed)?;
            verified = Some((id, record.status.ok_or(Unconfirmed)?));
        }
        let Some((id, status)) = verified else {
            return Ok(None);
        };
        if inventory
            .sessions
            .iter()
            .filter(|record| record.session_id.as_deref() == Some(id))
            .count()
            != 1
        {
            return Err(Unconfirmed);
        }
        Ok(Some(VerifiedSession {
            session_id: id.to_owned(),
            status,
        }))
    }
}

fn apply_start_environment(
    command: &mut CommandBuilder,
    spec: &CommandSpec,
    inherited: impl IntoIterator<Item = (impl AsRef<std::ffi::OsStr>, impl AsRef<std::ffi::OsStr>)>,
) {
    // portable-pty의 Windows 레지스트리 환경 대신 일반 CLI와 같은 프로세스 환경을 사용한다.
    command.env_clear();
    for (key, value) in inherited {
        command.env(key, value);
    }
    #[cfg(windows)]
    command.env("PATHEXT", ".EXE");
    // StartSession은 멱등성 토큰을 지원하지 않는다.
    command.env("AWS_MAX_ATTEMPTS", "1");
    for key in spec.env_remove() {
        command.env_remove(key);
    }
    for (key, value) in spec.env_set() {
        command.env(key, value);
    }
}

async fn prepare_start_with(
    scope: &Scope,
    node: &OnlineNode,
    capture: impl AsyncFnMut(CommandSpec) -> Result<Captured, CliError>,
) -> Result<StartRequest, CliError> {
    if &node.scope != scope {
        return Err(CliError::InvalidTarget);
    }
    let owner = caller_arn(scope, capture).await?;
    let mut nonce = [0; 16];
    getrandom::fill(&mut nonce).map_err(|_| CliError::RandomUnavailable)?;
    Ok(StartRequest {
        scope: scope.clone(),
        target: node.instance_id.clone(),
        reason: format!("zzabbaxterm-{:032x}", u128::from_be_bytes(nonce)),
        owner,
        command_issued: false,
        termination_id: None,
    })
}

fn identity_command(scope: &Scope) -> CommandSpec {
    scope.command("sts", "get-caller-identity", &[])
}

async fn caller_arn(
    scope: &Scope,
    mut capture: impl AsyncFnMut(CommandSpec) -> Result<Captured, CliError>,
) -> Result<String, CliError> {
    parse_caller_arn(&capture(identity_command(scope)).await?.success()?)
}

fn parse_caller_arn(bytes: &[u8]) -> Result<String, CliError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Identity {
        arn: String,
    }
    let Identity { arn } = decode(bytes)?;
    let mut fields = arn.splitn(6, ':');
    let [
        Some("arn"),
        Some(partition),
        Some("iam" | "sts"),
        Some(""),
        Some(account),
        Some(resource),
    ] = std::array::from_fn(|_| fields.next())
    else {
        return Err(CliError::InvalidResponse);
    };
    if !partition.starts_with("aws")
        || account.len() != 12
        || !account.bytes().all(|b| b.is_ascii_digit())
        || resource.is_empty()
        || arn.len() > 2048
        || arn.chars().any(char::is_control)
    {
        return Err(CliError::InvalidResponse);
    }
    Ok(arn)
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_+=,.@-".contains(&b))
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SessionInventory {
    sessions: Vec<SessionRecord>,
    next_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SessionRecord {
    session_id: Option<String>,
    target: Option<String>,
    owner: Option<String>,
    reason: Option<String>,
    status: Option<SessionStatus>,
}

#[derive(Clone, Copy, Deserialize, PartialEq)]
enum SessionStatus {
    Connected,
    Connecting,
    Disconnected,
    Terminated,
    Terminating,
    Failed,
}

impl SessionStatus {
    fn can_terminate(self) -> bool {
        matches!(
            self,
            Self::Connected | Self::Connecting | Self::Disconnected
        )
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TerminationResponse {
    session_id: String,
}

#[cfg(test)]
mod tests;
