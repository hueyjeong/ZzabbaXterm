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
use crate::terminal::ControlBinding;

#[derive(Clone, Copy)]
pub(crate) enum SessionDocument<'a> {
    DefaultShell,
    AiSharedControl { role: &'a AssumedRole, target: &'a str },
    /// R01이 승인 범위를 대조해 만든 공식 포트 전달 문서와 매개변수다.
    PortForward { document: &'static str, parameters: &'a str },
}

// 각 요청을 앱 세션에 보관하고 start-session은 요청당 한 번만 호출한다.
pub struct StartRequest {
    scope: Scope,
    target: String,
    document_name: Option<&'static str>,
    parameters: Option<String>,
    reason: String,
    owner: Arc<str>,
    role: Option<Arc<AssumedRole>>,
    credentials: Arc<Credentials>,
    program: Arc<Path>,
    command_issued: bool,
    termination_id: Option<String>,
    #[cfg(test)]
    fixture: bool,
}

// CommandBuilder를 노출하면 복제해 같은 인가를 여러 번 실행할 수 있다.
/// PTY를 시작할 때 소비하는 인가다.
///
/// ```compile_fail
/// use zzabbaxterm_lib::ssm::StartPermit;
/// fn duplicate(permit: StartPermit) { let _ = permit.clone(); }
/// ```
pub struct StartPermit(CommandBuilder, Arc<Credentials>, plugin::CheckedPlugin);

type StartedPty = (PtyPair, Box<dyn Child + Send + Sync>);

impl StartPermit {
    pub fn spawn(self, size: PtySize) -> io::Result<(PtyPair, Box<dyn Child + Send + Sync>)> {
        let pair = self.prepare_pty(size)?;
        self.spawn_in(pair, None, None, || {})
    }

    pub(crate) fn spawn_cancellable(self, size: PtySize, cancel: &CancellationToken, control: Option<&ControlBinding>) -> io::Result<StartedPty> {
        let pair = self.prepare_pty(size)?;
        self.spawn_in(pair, Some(cancel), control, || {})
    }

    fn prepare_pty(&self, size: PtySize) -> io::Result<PtyPair> {
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
        Ok(pair)
    }

    fn spawn_in(self, pair: PtyPair, cancel: Option<&CancellationToken>, control: Option<&ControlBinding>, before_spawn: impl FnOnce()) -> io::Result<StartedPty> {
        let Self(command, credentials, plugin) = self;
        let spawn = || {
            before_spawn();
            pair.slave.spawn_command(command).map_err(io::Error::other)
        };
        let child = if let Some(binding) = control {
            binding.submit_start(spawn).map_err(|error| io::Error::new(io::ErrorKind::Interrupted, error))??
        } else {
            if cancel.is_some_and(CancellationToken::is_cancelled) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "SSM: 시작 취소"));
            }
            spawn()?
        };
        Ok((
            pair,
            Box::new(SessionChild { child, _plugin: plugin, _credentials: credentials }),
        ))
    }
}

const FORWARD_DOCUMENTS: [&str; 2] = ["AWS-StartPortForwardingSession", "AWS-StartPortForwardingSessionToRemoteHost"];

/// 포트 전달 자식을 시작할 때 소비하는 인가다. 자식의 stdout·stderr는 호출자가 판독한다.
pub(crate) struct ForwardPermit {
    command: std::process::Command,
    program: Arc<Path>,
    cwd: std::path::PathBuf,
    credentials: Arc<Credentials>,
    plugin: plugin::CheckedPlugin,
}

/// 자식이 끝날 때까지 CLI 홈과 검증한 플러그인 파일을 보존한다.
pub(crate) struct ForwardGuard {
    _credentials: Arc<Credentials>,
    _plugin: plugin::CheckedPlugin,
}

impl ForwardPermit {
    /// `configure`는 프로세스 트리 정리 설정(작업 개체·프로세스 그룹)만 더한다.
    pub(crate) fn spawn(self, configure: impl FnOnce(&mut std::process::Command)) -> io::Result<(std::process::Child, ForwardGuard)> {
        let expired = || io::Error::new(io::ErrorKind::PermissionDenied, "SSM credentials expired");
        self.credentials.require_current().map_err(|_| expired())?;
        self.plugin.recheck(&self.program, &self.cwd)
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "SSM plugin search changed"))?;
        self.credentials.require_current().map_err(|_| expired())?;
        self.spawn_unchecked(configure)
    }

    fn spawn_unchecked(mut self, configure: impl FnOnce(&mut std::process::Command)) -> io::Result<(std::process::Child, ForwardGuard)> {
        self.command.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        configure(&mut self.command);
        let child = self.command.spawn()?;
        Ok((child, ForwardGuard { _credentials: self.credentials, _plugin: self.plugin }))
    }
}

#[cfg(test)]
impl ForwardPermit {
    /// 가짜 플러그인 프로그램을 같은 파이프·트리 설정으로 실행한다.
    pub(crate) fn fixture(program: &Path, args: &[&str]) -> Self {
        let mut command = std::process::Command::new(program);
        command.args(args);
        Self {
            command,
            program: program.into(),
            cwd: std::env::temp_dir(),
            credentials: super::tests::fixture_credentials(),
            plugin: plugin::tests::fixture_plugin(),
        }
    }

    pub(crate) fn spawn_fixture(self, configure: impl FnOnce(&mut std::process::Command)) -> io::Result<(std::process::Child, ForwardGuard)> {
        self.spawn_unchecked(configure)
    }
}

#[cfg(test)]
pub(crate) fn fixture_start(program: &Path) -> (AwsCli, Scope, StartRequest, StartPermit) {
    let scope = Scope::new("fixture".into(), "us-west-2".into()).unwrap();
    let program: Arc<Path> = program.into();
    let credentials = super::tests::fixture_credentials();
    let request = StartRequest {
        scope: scope.clone(),
        target: "i-01234567".into(),
        document_name: None,
        parameters: None,
        reason: "zzabbaxterm-0123456789abcdef0123456789abcdef".into(),
        owner: "arn:aws:sts::123456789012:assumed-role/Sample/Sample".into(),
        role: None,
        credentials: credentials.clone(),
        program: program.clone(),
        command_issued: true,
        termination_id: None,
        fixture: true,
    };
    let plugin = plugin::tests::fixture_plugin();
    let command = request.start_command(credentials.home().path(), plugin.path());
    (AwsCli(program), scope, request, StartPermit(command, credentials, plugin))
}

#[cfg(test)]
impl StartPermit {
    pub(crate) fn spawn_fixture(self, size: PtySize, cancel: &CancellationToken, control: Option<&ControlBinding>, after_open: impl FnOnce(), before_spawn: impl FnOnce()) -> io::Result<StartedPty> {
        let pair = native_pty_system().openpty(size).map_err(io::Error::other)?;
        after_open();
        self.spawn_in(pair, Some(cancel), control, before_spawn)
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
        self.prepare_document_start(scope, node, SessionDocument::DefaultShell, cancel)
    }

    pub(crate) fn prepare_document_start(
        &self,
        scope: &Scope,
        node: &OnlineNode,
        document: SessionDocument<'_>,
        cancel: &CancellationToken,
    ) -> Result<StartRequest, CliError> {
        require_node_scope(&self.0, scope, node)?;
        if cancel.is_cancelled() {
            return Err(CliError::Cancelled);
        }
        node.credentials.require_current()?;
        prepare_start(scope, node, document)
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

    pub(crate) async fn authorize_forward(
        &self,
        request: &mut StartRequest,
        scope: &Scope,
        cancel: &CancellationToken,
    ) -> Result<Option<ForwardPermit>, CliError> {
        request
            .authorize_forward_with(&self.0, scope, async |cwd| {
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
        request.wait_connected_with(cancel, async |spec, timeout| {
            #[cfg(test)]
            if request.fixture { return spec.capture_fixture(cancel, timeout).await; }
            spec.capture_with_timeout(cancel, timeout).await
        }).await
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
        #[cfg(test)]
        let fixture = request.fixture;
        request
            .close_with(&self.0, current_scope, async |spec| {
                #[cfg(test)]
                if fixture { return spec.capture_fixture(cancel, CLOSE_LOOKUP_TIMEOUT).await; }
                spec.capture_with_timeout(cancel, CLOSE_LOOKUP_TIMEOUT).await
            })
            .await
    }

    pub(crate) async fn recheck_closed(&self, request: &StartRequest, scope: &Scope) -> Result<(), VerificationError> {
        request.require_issued_scope(&self.0, scope)?;
        let cancel = CancellationToken::new();
        request.confirm_terminated(request.termination_id.as_deref(), async |spec| {
            #[cfg(test)]
            if request.fixture { return spec.capture_fixture(&cancel, CLOSE_LOOKUP_TIMEOUT).await; }
            spec.capture_with_timeout(&cancel, CLOSE_LOOKUP_TIMEOUT).await
        }).await
    }
}

impl StartRequest {
    pub(crate) fn account_id(&self) -> &str {
        caller_account_id(&self.owner)
    }

    pub(crate) fn assumed_role(&self) -> Option<&Arc<AssumedRole>> {
        self.role.as_ref()
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
        check_plugin: impl AsyncFnMut(&Path) -> Result<plugin::CheckedPlugin, CliError>,
    ) -> Result<Option<StartPermit>, CliError> {
        if self.parameters.is_some() {
            return Err(CliError::InvalidTarget);
        }
        let Some((cwd, path)) = self.authorize_checked(program, scope, check_plugin).await? else {
            return Ok(None);
        };
        let command = self.start_command(&cwd, path.path());
        Ok(Some(StartPermit(command, self.credentials.clone(), path)))
    }

    async fn authorize_forward_with(
        &mut self,
        program: &Path,
        scope: &Scope,
        check_plugin: impl AsyncFnMut(&Path) -> Result<plugin::CheckedPlugin, CliError>,
    ) -> Result<Option<ForwardPermit>, CliError> {
        if self.parameters.is_none() || self.document_name.is_none() {
            return Err(CliError::InvalidTarget);
        }
        let Some((cwd, plugin)) = self.authorize_checked(program, scope, check_plugin).await? else {
            return Ok(None);
        };
        let command = self.forward_command(&cwd, plugin.path());
        Ok(Some(ForwardPermit { command, program: self.program.clone(), cwd, credentials: self.credentials.clone(), plugin }))
    }

    async fn authorize_checked(
        &mut self,
        program: &Path,
        scope: &Scope,
        mut check_plugin: impl AsyncFnMut(&Path) -> Result<plugin::CheckedPlugin, CliError>,
    ) -> Result<Option<(std::path::PathBuf, plugin::CheckedPlugin)>, CliError> {
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
        self.command_issued = true;
        Ok(Some((cwd, path)))
    }

    fn start_spec(&self) -> CommandSpec {
        let mut args = vec!["--target", self.target.as_str(), "--reason", self.reason.as_str()];
        if let Some(document) = self.document_name {
            args.extend(["--document-name", document]);
        }
        if let Some(parameters) = &self.parameters {
            args.extend(["--parameters", parameters.as_str()]);
        }
        self.scope
            .command(&self.program, "ssm", "start-session", &args)
            .with_credentials(&self.scope, &self.credentials)
    }

    fn start_command(&self, cwd: &Path, plugin_path: &OsStr) -> CommandBuilder {
        let spec = self.start_spec();
        let mut command = CommandBuilder::new(spec.program());
        command.args(spec.args());
        command.cwd(cwd);
        apply_start_environment(&mut command, &spec, std::env::vars_os());
        // StartSession 작업 이름이 들어 있는 CLI 오류 형식으로 고정한다.
        command.env("AWS_CLI_ERROR_FORMAT", "legacy");
        command.env("PATH", plugin_path);
        command
    }

    // 포트 전달은 PTY 대신 파이프로 실행해 플러그인의 준비 출력을 그대로 읽는다.
    fn forward_command(&self, cwd: &Path, plugin_path: &OsStr) -> std::process::Command {
        let spec = self.start_spec();
        let mut command = std::process::Command::new(spec.program());
        command.args(spec.args());
        command.current_dir(cwd);
        apply_start_environment(&mut command, &spec, std::env::vars_os());
        command.env("AWS_CLI_ERROR_FORMAT", "legacy");
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

pub(crate) fn start_rejection(output: &[u8]) -> Option<CliError> {
    #[cfg(windows)]
    let output = conpty_diagnostic(output)?;
    let output = std::str::from_utf8(output).ok()?;
    let mut lines = output.lines().map(str::trim).filter(|line| {
        !line.is_empty() && line.as_bytes() != super::cli::HISTORY_WARNING
    });
    let line = lines.next()?;
    if lines.next().is_some() { return None; }
    let line = line.strip_prefix("aws: [ERROR]: ").unwrap_or(line);
    let (code, _) = line.strip_prefix("An error occurred (")?
        .split_once(") when calling the StartSession operation:")?;
    Some(match code {
        "AccessDenied" | "AccessDeniedException" | "NotAuthorized" | "UnauthorizedOperation" => CliError::AccessDenied,
        "ExpiredToken" | "ExpiredTokenException" => CliError::CredentialsExpired,
        "InvalidClientTokenId" | "UnrecognizedClientException" => CliError::CredentialsUnavailable,
        "InvalidDocument" => CliError::InvalidSessionDocument,
        "TargetNotConnected" => CliError::TargetNotConnected,
        _ => return None,
    })
}

#[cfg(windows)]
fn conpty_diagnostic(mut output: &[u8]) -> Option<&[u8]> {
    const SHUTDOWN_MODES: [&[u8]; 2] = [b"\x1b[?9001l", b"\x1b[?1004l"];
    // ConPTY 종료 모드 제어열은 같은 스트림의 진단문 앞이나 뒤에 올 수 있다.
    while output.starts_with(b"\x1b") {
        if let Some(title) = output.strip_prefix(b"\x1b]0;") {
            output = &title[title.iter().position(|&byte| byte == 7)? + 1..];
        } else if let Some(rest) = [b"\x1b[6n".as_slice(), b"\x1b[?9001h", b"\x1b[?1004h", b"\x1b[m", b"\x1b[?25h"]
            .into_iter().chain(SHUTDOWN_MODES)
            .find_map(|control| output.strip_prefix(control))
        {
            output = rest;
        } else {
            return None;
        }
    }
    while let Some(rest) = SHUTDOWN_MODES
        .into_iter().find_map(|control| output.strip_suffix(control))
    {
        output = rest;
    }
    (!output.contains(&0x1b)).then_some(output)
}

#[cfg(all(test, windows))]
#[test]
fn start_rejection_accepts_conpty_shutdown_modes_before_diagnostic() {
    let output = b"\x1b[6n\x1b[?9001h\x1b[?1004h\x1b[m\x1b]0;fixture\x07\x1b[?25h\x1b[?9001l\x1b[?1004l\
        An error occurred (TargetNotConnected) when calling the StartSession operation: offline\n";
    assert!(matches!(start_rejection(output), Some(CliError::TargetNotConnected)));
}

trait StartEnvironment {
    fn clear(&mut self);
    fn set(&mut self, key: &OsStr, value: &OsStr);
    fn remove(&mut self, key: &str);
}

impl StartEnvironment for CommandBuilder {
    fn clear(&mut self) { self.env_clear(); }
    fn set(&mut self, key: &OsStr, value: &OsStr) { self.env(key, value); }
    fn remove(&mut self, key: &str) { self.env_remove(key); }
}

impl StartEnvironment for std::process::Command {
    fn clear(&mut self) { self.env_clear(); }
    fn set(&mut self, key: &OsStr, value: &OsStr) { self.env(key, value); }
    fn remove(&mut self, key: &str) { self.env_remove(key); }
}

fn apply_start_environment(
    command: &mut impl StartEnvironment,
    spec: &CommandSpec,
    inherited: impl IntoIterator<Item = (impl AsRef<std::ffi::OsStr>, impl AsRef<std::ffi::OsStr>)>,
) {
    // portable-pty의 Windows 레지스트리 환경 대신 일반 CLI와 같은 프로세스 환경을 사용한다.
    command.clear();
    for (key, value) in inherited {
        command.set(key.as_ref(), value.as_ref());
    }
    #[cfg(windows)]
    command.set(OsStr::new("PATHEXT"), OsStr::new(".EXE"));
    for key in spec.env_remove() {
        command.remove(key);
    }
    for (key, value) in spec.env_set() {
        command.set(OsStr::new(key), OsStr::new(value));
    }
    spec.bound_environment(|key, value| {
        command.set(OsStr::new(key), value);
    });
}

fn require_node_scope(program: &Path, scope: &Scope, node: &OnlineNode) -> Result<(), CliError> {
    if &node.scope != scope || node.program.as_ref() != program {
        return Err(CliError::InvalidTarget);
    }
    Ok(())
}

fn caller_account_id(owner: &str) -> &str {
    owner.splitn(6, ':').nth(4).expect("validated STS caller ARN")
}

fn prepare_start(scope: &Scope, node: &OnlineNode, document: SessionDocument<'_>) -> Result<StartRequest, CliError> {
    let (document_name, parameters) = match document {
        SessionDocument::DefaultShell => (None, None),
        SessionDocument::AiSharedControl { role, target } => {
            if target != node.instance_id() {
                return Err(CliError::InvalidTarget);
            }
            if node.role.as_deref() != Some(role) {
                return Err(CliError::CallerChanged);
            }
            (Some("ZzabbaAiSharedControl"), None)
        }
        SessionDocument::PortForward { document, parameters } => {
            if !FORWARD_DOCUMENTS.contains(&document) || parameters.is_empty() {
                return Err(CliError::InvalidTarget);
            }
            (Some(document), Some(parameters.to_owned()))
        }
    };
    let mut nonce = [0; 16];
    getrandom::fill(&mut nonce).map_err(|_| CliError::RandomUnavailable)?;
    Ok(StartRequest {
        scope: scope.clone(),
        target: node.instance_id.clone(),
        document_name,
        parameters,
        reason: format!("zzabbaxterm-{:032x}", u128::from_be_bytes(nonce)),
        owner: node.owner.clone(),
        role: node.role.clone(),
        credentials: node.credentials.clone(),
        program: node.program.clone(),
        command_issued: false,
        termination_id: None,
        #[cfg(test)]
        fixture: false,
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

#[derive(PartialEq, Eq)]
pub(crate) struct AssumedRole {
    partition: Box<str>,
    account: Box<str>,
    id: Box<str>,
}

impl std::fmt::Debug for AssumedRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AssumedRole(..)")
    }
}

#[cfg(test)]
impl AssumedRole {
    pub(crate) fn fixture(account: &str) -> Arc<Self> {
        Arc::new(Self { partition: "aws".into(), account: account.into(), id: "AROADBQP57FF2AEXAMPLE".into() })
    }
}

pub(super) fn captured_caller(captured: Captured) -> Result<(Arc<str>, Option<Arc<AssumedRole>>), CliError> {
    parse_caller(&captured.success()?)
}

fn parse_caller(bytes: &[u8]) -> Result<(Arc<str>, Option<Arc<AssumedRole>>), CliError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Identity {
        // 일반 셸은 ARN으로 소유자를 확인하고, A03만 추가 역할 증거를 요구한다.
        arn: String,
        account: Option<serde_json::Value>,
        user_id: Option<serde_json::Value>,
    }
    let identity: Identity = decode(bytes)?;
    let arn = identity.arn;
    let mut fields = arn.splitn(6, ':');
    let [
        Some("arn"),
        Some(partition),
        Some(service @ ("iam" | "sts")),
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
    let role = if service == "sts" && identity.account.as_ref().and_then(serde_json::Value::as_str) == Some(account) {
        identity.user_id.as_ref().and_then(serde_json::Value::as_str)
            .and_then(|user_id| assumed_role(partition, account, resource, user_id))
    } else {
        None
    };
    Ok((Arc::from(arn), role))
}

fn assumed_role(partition: &str, account: &str, resource: &str, user_id: &str) -> Option<Arc<AssumedRole>> {
    if partition != "aws" && !partition.strip_prefix("aws-").is_some_and(|suffix| {
        suffix.split('-').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
    }) {
        return None;
    }
    let (name, session) = resource.strip_prefix("assumed-role/")?.split_once('/')?;
    let (id, user_session) = user_id.split_once(':')?;
    let valid_name = |name: &str, min: usize| {
        (min..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_+=,.@-".contains(&b))
    };
    let suffix = id.strip_prefix("AROA")?;
    if !valid_name(name, 1) || !valid_name(session, 2) || user_session != session
        || !(16..=128).contains(&id.len()) || !suffix.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        return None;
    }
    Some(Arc::new(AssumedRole { partition: partition.into(), account: account.into(), id: id.into() }))
}

pub(crate) fn valid_session_id(id: &str) -> bool {
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
