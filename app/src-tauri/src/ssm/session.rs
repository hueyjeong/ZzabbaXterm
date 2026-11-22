use super::{
    AwsCli, CancellationToken, Captured, CliError, CommandSpec, OnlineNode, Scope, decode,
};
use portable_pty::{Child, CommandBuilder, PtyPair, PtySize, native_pty_system};
use serde::Deserialize;
use std::{io, path::Path, sync::Arc};

// 각 요청을 앱 세션에 보관하고 start-session은 요청당 한 번만 호출한다.
pub struct StartRequest {
    scope: Scope,
    target: String,
    reason: String,
    owner: Arc<str>,
    program: Arc<Path>,
    command_issued: bool,
    termination_id: Option<String>,
}

// CommandBuilder를 노출하면 복제해 같은 인가를 여러 번 실행할 수 있다.
/// PTY를 시작할 때 소비하는 인가다.
///
/// ```compile_fail
/// use zzabbaxterm_lib::ssm::StartPermit;
/// fn duplicate(permit: StartPermit) { let _ = permit.clone(); }
/// ```
pub struct StartPermit(CommandBuilder);

impl StartPermit {
    pub fn spawn(self, size: PtySize) -> io::Result<(PtyPair, Box<dyn Child + Send + Sync>)> {
        let pair = native_pty_system()
            .openpty(size)
            .map_err(io::Error::other)?;
        let child = pair.slave.spawn_command(self.0).map_err(io::Error::other)?;
        Ok((pair, child))
    }
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
        require_node_scope(&self.0, scope, node)?;
        if cancel.is_cancelled() {
            return Err(CliError::Cancelled);
        }
        super::require_direct_profile(scope.profile())?;
        prepare_start(scope, node)
    }

    pub async fn authorize_start(
        &self,
        request: &mut StartRequest,
        scope: &Scope,
        cancel: &CancellationToken,
    ) -> Result<Option<StartPermit>, CliError> {
        request
            .authorize_with(&self.0, scope, async |spec| {
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
        request.require_issued_scope(&self.0, current_scope)?;
        super::require_direct_profile(current_scope.profile()).map_err(|_| Unconfirmed)?;
        let caller = caller_arn(&self.0, current_scope, async |spec| {
            spec.capture(cancel).await
        })
        .await
        .map_err(|_| Unconfirmed)?;
        if caller != request.owner.as_ref() {
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
        request.require_issued_scope(&self.0, current_scope)?;
        super::require_direct_profile(current_scope.profile())
            .map_err(|_| VerificationError::Unconfirmed)?;
        request
            .close_with(&self.0, current_scope, async |spec| {
                spec.capture(cancel).await
            })
            .await
    }
}

impl StartRequest {
    async fn authorize_with(
        &mut self,
        program: &Path,
        scope: &Scope,
        capture: impl AsyncFnMut(CommandSpec) -> Result<Captured, CliError>,
    ) -> Result<Option<StartPermit>, CliError> {
        if !self.matches(program, scope) {
            return Err(CliError::InvalidTarget);
        }
        if self.command_issued {
            return Ok(None);
        }
        let cwd = std::env::current_dir().map_err(|_| CliError::ProcessIo)?;
        let command = self.start_command(&cwd);
        // 인가 직전 신원을 확인한다. 외부의 실행 파일·프로필·캐시 변경은 이 경계 밖이다.
        if caller_arn(&self.program, scope, capture).await? != self.owner.as_ref() {
            return Err(CliError::CallerChanged);
        }
        self.command_issued = true;
        Ok(Some(StartPermit(command)))
    }

    fn start_command(&self, cwd: &Path) -> CommandBuilder {
        let spec = self.scope.command(
            &self.program,
            "ssm",
            "start-session",
            &["--target", &self.target, "--reason", &self.reason],
        );
        let mut command = CommandBuilder::new(spec.program());
        command.args(spec.args());
        command.cwd(cwd);
        apply_start_environment(&mut command, &spec, std::env::vars_os());
        command
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    fn matches(&self, program: &Path, scope: &Scope) -> bool {
        *scope == self.scope && program == self.program.as_ref()
    }

    fn require_issued_scope(&self, program: &Path, scope: &Scope) -> Result<(), VerificationError> {
        if self.command_issued && self.matches(program, scope) {
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
            &self.program,
            "ssm",
            "describe-sessions",
            &["--state", state, "--filters", &filter],
        )
    }

    async fn close_with(
        &mut self,
        program: &Path,
        current_scope: &Scope,
        mut capture: impl AsyncFnMut(CommandSpec) -> Result<Captured, CliError>,
    ) -> Result<(), VerificationError> {
        use VerificationError::Unconfirmed;
        self.require_issued_scope(program, current_scope)?;
        if caller_arn(&self.program, current_scope, &mut capture)
            .await
            .map_err(|_| Unconfirmed)?
            != self.owner.as_ref()
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
        let result = capture(self.scope.command(
            &self.program,
            "ssm",
            "terminate-session",
            &["--session-id", id],
        ))
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
            if record.owner.as_deref() != Some(self.owner.as_ref())
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
    for key in spec.env_remove() {
        command.env_remove(key);
    }
    for (key, value) in spec.env_set() {
        command.env(key, value);
    }
}

fn require_node_scope(program: &Path, scope: &Scope, node: &OnlineNode) -> Result<(), CliError> {
    if &node.scope != scope || node.program.as_ref() != program {
        return Err(CliError::InvalidTarget);
    }
    Ok(())
}

fn prepare_start(scope: &Scope, node: &OnlineNode) -> Result<StartRequest, CliError> {
    let mut nonce = [0; 16];
    getrandom::fill(&mut nonce).map_err(|_| CliError::RandomUnavailable)?;
    Ok(StartRequest {
        scope: scope.clone(),
        target: node.instance_id.clone(),
        reason: format!("zzabbaxterm-{:032x}", u128::from_be_bytes(nonce)),
        owner: node.owner.clone(),
        program: node.program.clone(),
        command_issued: false,
        termination_id: None,
    })
}

pub(super) fn identity_command(program: &Arc<Path>, scope: &Scope) -> CommandSpec {
    scope.command(program, "sts", "get-caller-identity", &[])
}

pub(super) async fn caller_arn(
    program: &Arc<Path>,
    scope: &Scope,
    mut capture: impl AsyncFnMut(CommandSpec) -> Result<Captured, CliError>,
) -> Result<String, CliError> {
    captured_caller_arn(capture(identity_command(program, scope)).await?)
}

pub(super) fn captured_caller_arn(captured: Captured) -> Result<String, CliError> {
    parse_caller_arn(&captured.success()?)
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
