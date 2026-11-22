use std::time::Duration;

const OBSERVE_INTERVAL: Duration = Duration::from_millis(250);
const CLOSE_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);
const CLOSE_ATTEMPTS: usize = 4;

use super::{
    AwsCli, CancellationToken, Captured, CliError, CommandSpec, Credentials, OnlineNode, Scope,
    decode, plugin,
};
use portable_pty::{
    Child, ChildKiller, CommandBuilder, ExitStatus, PtyPair, PtySize, native_pty_system,
};
use serde::Deserialize;
use std::{ffi::OsStr, io, path::Path, sync::Arc};

// 각 요청을 앱 세션에 보관하고 start-session은 요청당 한 번만 호출한다.
pub struct StartRequest {
    scope: Scope,
    target: String,
    reason: String,
    owner: Arc<str>,
    credentials: Arc<Credentials>,
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
pub struct StartPermit(CommandBuilder, Arc<Credentials>, plugin::CheckedPlugin);

impl StartPermit {
    pub fn spawn(self, size: PtySize) -> io::Result<(PtyPair, Box<dyn Child + Send + Sync>)> {
        self.1.require_current().map_err(|_| {
            io::Error::new(io::ErrorKind::PermissionDenied, "SSM credentials expired")
        })?;
        let pair = native_pty_system()
            .openpty(size)
            .map_err(io::Error::other)?;
        self.1.require_current().map_err(|_| {
            io::Error::new(io::ErrorKind::PermissionDenied, "SSM credentials expired")
        })?;
        let cwd = self
            .0
            .get_cwd()
            .ok_or_else(|| io::Error::other("SSM working directory missing"))?;
        self.2
            .recheck(
                self.0.get_argv()[0].as_os_str().as_ref(),
                cwd.as_os_str().as_ref(),
            )
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "SSM plugin search changed"))?;
        let child = pair.slave.spawn_command(self.0).map_err(io::Error::other)?;
        Ok((
            pair,
            Box::new(SessionChild {
                child,
                _plugin: self.2,
                _credentials: self.1,
            }),
        ))
    }
}

struct SessionChild {
    child: Box<dyn Child + Send + Sync>,
    _plugin: plugin::CheckedPlugin,
    _credentials: Arc<Credentials>,
}

impl std::fmt::Debug for SessionChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionChild").finish_non_exhaustive()
    }
}

impl ChildKiller for SessionChild {
    fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }
    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        self.child.clone_killer()
    }
}

impl Child for SessionChild {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
    fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }
    fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }
    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        self.child.as_raw_handle()
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

    pub fn is_connected(&self) -> bool {
        self.status == SessionStatus::Connected
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
        node.credentials.require_current()?;
        prepare_start(scope, node)
    }

    pub async fn authorize_start(
        &self,
        request: &mut StartRequest,
        scope: &Scope,
        cancel: &CancellationToken,
    ) -> Result<Option<StartPermit>, CliError> {
        request
            .authorize_with(&self.0, scope, async |cwd| {
                plugin::checked_path(&self.0, cwd, cancel).await
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
        request
            .credentials
            .require_current()
            .map_err(|_| Unconfirmed)?;
        let result = request.lookup_command(lookup, None).capture(cancel).await;
        request.find_session(result)?.ok_or(Unconfirmed)
    }

    pub(crate) async fn wait_connected(
        &self,
        request: &StartRequest,
        scope: &Scope,
        cancel: &CancellationToken,
    ) -> Result<(), VerificationError> {
        request.require_issued_scope(&self.0, scope)?;
        request.wait_connected_with(cancel, async |spec, timeout| spec.capture_with_timeout(cancel, timeout).await).await
    }

    pub async fn close_session(
        &self,
        request: &mut StartRequest,
        current_scope: &Scope,
        cancel: &CancellationToken,
    ) -> Result<(), VerificationError> {
        request.require_issued_scope(&self.0, current_scope)?;
        request
            .credentials
            .require_current()
            .map_err(|_| VerificationError::Unconfirmed)?;
        request
            .close_with(&self.0, current_scope, async |spec| {
                spec.capture_with_timeout(cancel, CLOSE_LOOKUP_TIMEOUT).await
            })
            .await
    }

    pub(crate) async fn recheck_closed(&self, request: &StartRequest, scope: &Scope) -> Result<(), VerificationError> {
        request.require_issued_scope(&self.0, scope)?;
        let cancel = CancellationToken::new();
        request.confirm_terminated(request.termination_id.as_deref(), async |spec| {
            spec.capture_with_timeout(&cancel, CLOSE_LOOKUP_TIMEOUT).await
        }).await
    }
}

impl StartRequest {
    pub(crate) fn account_id(&self) -> &str {
        self.owner.splitn(6, ':').nth(4).expect("validated STS caller ARN")
    }

    async fn wait_connected_with(
        &self,
        cancel: &CancellationToken,
        mut capture: impl AsyncFnMut(CommandSpec, Duration) -> Result<Captured, CliError>,
    ) -> Result<(), VerificationError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            if cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
                return Err(VerificationError::Unconfirmed);
            }
            let timeout = deadline.saturating_duration_since(tokio::time::Instant::now()).min(CLOSE_LOOKUP_TIMEOUT);
            let result = capture(self.lookup_command(SessionLookup::Active, None), timeout).await;
            if !matches!(result, Err(CliError::TimedOut)) {
                match self.find_session(result)? {
                    Some(session) if session.is_connected() => return Ok(()),
                    Some(session) if session.status != SessionStatus::Connecting => return Err(VerificationError::Unconfirmed),
                    _ => {}
                }
            }
            if tokio::time::Instant::now() + OBSERVE_INTERVAL >= deadline {
                return Err(VerificationError::Unconfirmed);
            }
            tokio::select! {
                _ = cancel.cancelled() => return Err(VerificationError::Unconfirmed),
                _ = tokio::time::sleep(OBSERVE_INTERVAL) => {}
            }
        }
    }

    async fn authorize_with(
        &mut self,
        program: &Path,
        scope: &Scope,
        mut check_plugin: impl AsyncFnMut(&Path) -> Result<plugin::CheckedPlugin, CliError>,
    ) -> Result<Option<StartPermit>, CliError> {
        if !self.matches(program, scope) {
            return Err(CliError::InvalidTarget);
        }
        if self.command_issued {
            return Ok(None);
        }
        self.credentials.require_current()?;
        let cwd = self.credentials.home().path().to_path_buf();
        let path = check_plugin(&cwd).await?;
        self.credentials.require_current()?;
        let command = self.start_command(&cwd, path.path());
        self.command_issued = true;
        Ok(Some(StartPermit(command, self.credentials.clone(), path)))
    }

    fn start_command(&self, cwd: &Path, plugin_path: &OsStr) -> CommandBuilder {
        let spec = self
            .scope
            .command(
                &self.program,
                "ssm",
                "start-session",
                &["--target", &self.target, "--reason", &self.reason],
            )
            .with_credentials(&self.scope, &self.credentials);
        let mut command = CommandBuilder::new(spec.program());
        command.args(spec.args());
        command.cwd(cwd);
        apply_start_environment(&mut command, &spec, std::env::vars_os());
        command.env("PATH", plugin_path);
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
        self.scope
            .command(
                &self.program,
                "ssm",
                "describe-sessions",
                &["--state", state, "--filters", &filter],
            )
            .with_credentials(&self.scope, &self.credentials)
    }

    async fn close_with(
        &mut self,
        program: &Path,
        current_scope: &Scope,
        mut capture: impl AsyncFnMut(CommandSpec) -> Result<Captured, CliError>,
    ) -> Result<(), VerificationError> {
        use VerificationError::Unconfirmed;
        self.require_issued_scope(program, current_scope)?;
        self.credentials
            .require_current()
            .map_err(|_| Unconfirmed)?;
        if let Some(id) = self.termination_id.as_deref() {
            return self.confirm_terminated(Some(id), &mut capture).await;
        }
        let mut active = None;
        for attempt in 0..CLOSE_ATTEMPTS {
            if attempt != 0 { tokio::time::sleep(OBSERVE_INTERVAL).await; }
            let history = capture(self.lookup_command(SessionLookup::History, None)).await;
            if matches!(history, Err(CliError::TimedOut)) { continue; }
            if let Some(history) = self.find_session(history)? {
                return match history.status {
                    SessionStatus::Terminated => Ok(()),
                    SessionStatus::Terminating => {
                        self.termination_id = Some(history.session_id);
                        self.confirm_terminated(self.termination_id.as_deref(), &mut capture).await
                    }
                    _ => Err(Unconfirmed),
                };
            }
            let result = capture(self.lookup_command(SessionLookup::Active, None)).await;
            if matches!(result, Err(CliError::TimedOut)) { continue; }
            active = self.find_session(result)?;
            if active.is_some() { break; }
        }
        let active = active.ok_or(Unconfirmed)?;
        if active.status == SessionStatus::Terminating {
            self.termination_id = Some(active.session_id);
            return self.confirm_terminated(self.termination_id.as_deref(), &mut capture).await;
        }
        if !active.status.can_terminate() { return Err(Unconfirmed); }

        let id = self.termination_id.insert(active.session_id).as_str();
        let result = capture(
            self.scope
                .command(
                    &self.program,
                    "ssm",
                    "terminate-session",
                    &["--session-id", id],
                )
                .with_credentials(&self.scope, &self.credentials),
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
            self.termination_id.as_deref(),
            &mut capture,
        )
        .await
    }

    async fn confirm_terminated(
        &self,
        id: Option<&str>,
        mut capture: impl AsyncFnMut(CommandSpec) -> Result<Captured, CliError>,
    ) -> Result<(), VerificationError> {
        use VerificationError::Unconfirmed;
        for attempt in 0..CLOSE_ATTEMPTS {
            if attempt != 0 { tokio::time::sleep(OBSERVE_INTERVAL).await; }
            let result = capture(self.lookup_command(SessionLookup::History, id)).await;
            if matches!(result, Err(CliError::TimedOut)) { continue; }
            match self.find_session(result)? {
                Some(session) if id.is_some_and(|id| session.session_id() != id) => return Err(Unconfirmed),
                Some(session) if session.is_terminated() => return Ok(()),
                Some(session) if session.status != SessionStatus::Terminating => return Err(Unconfirmed),
                _ => {}
            }
        }
        Err(Unconfirmed)
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
    spec.bound_environment(|key, value| {
        command.env(key, value);
    });
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
        credentials: node.credentials.clone(),
        program: node.program.clone(),
        command_issued: false,
        termination_id: None,
    })
}

pub(super) fn identity_command(
    program: &Arc<Path>,
    scope: &Scope,
    credentials: &Arc<Credentials>,
) -> CommandSpec {
    scope
        .command(program, "sts", "get-caller-identity", &[])
        .with_credentials(scope, credentials)
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
