use portable_pty::{Child, CommandBuilder, SlavePty};
use std::path::Path;
use std::sync::Arc;
#[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
use std::time::{Duration, Instant};
use crate::connections::profile::{Ssh, SshAuth, SshTransport};
use tokio_util::sync::CancellationToken;

use super::{ApprovedHost, Authentication, Client, ConnectionPlan, Error, Invocation, Transport, TrustStore};

// 승인 창을 읽는 시간은 교환 예산과 분리한다. 실제 askpass 교환 기한은 spawn에서 다시 잰다.
#[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
pub(crate) const APPROVAL_TTL: Duration = Duration::from_secs(120);

pub(crate) struct DirectLaunch {
    command: CommandBuilder,
    plan: Arc<ConnectionPlan>,
    editor: Option<(super::nano::Binding, tokio::sync::oneshot::Sender<()>)>,
    notice: Option<String>,
    #[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
    stored: Option<StoredLaunch>,
}

/// 승인 한 건과 그 Exchange는 실행 자식과 탭 수명에 함께 묶인다.
#[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
struct StoredLaunch {
    document: crate::local_auth::Document,
    approval: crate::local_auth::Approval,
    vault: crate::local_auth::Bitwarden,
}

/// PTY 감독자가 넘겨받는 실행 자식과 수명 동안 보관할 교환이다.
pub(crate) struct SshSpawn {
    pub(crate) child: Box<dyn Child + Send + Sync>,
    pub(crate) plan: Arc<ConnectionPlan>,
    #[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
    pub(crate) stored: Option<crate::local_auth::Exchange>,
}

impl DirectLaunch {
    pub(crate) fn prepare(
        store: &TrustStore,
        approved_host: &ApprovedHost<'_>,
        account: &str,
        approved_account: &str,
        auth: Authentication<'_>,
        transport: Transport,
    ) -> Result<Self, Error> {
        if account != approved_account {
            return Err(Error::ApprovedAccountMismatch);
        }
        if matches!(auth, Authentication::StoredPassword { .. }) {
            return Err(Error::StoredAuthenticationUnavailable);
        }
        let plan = Arc::new(ConnectionPlan::prepare(store, approved_host, account, transport, auth)?);
        let command = command(plan.invocation(Client::Ssh)?);
        Ok(Self {
            command,
            plan,
            editor: None,
            notice: None,
            #[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
            stored: None,
        })
    }

    /// 저장 암호 대상은 승인된 Exchange를 자식과 함께 넘긴다. 그 밖에는 평소 명령을 실행한다.
    #[allow(unused_mut, reason = "stored-auth 빌드에서만 승인을 꺼낸다")]
    pub(crate) fn spawn(mut self, slave: &dyn SlavePty) -> Result<SshSpawn, String> {
        #[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
        if let Some(stored) = self.stored.take() {
            let (child, exchange) = tauri::async_runtime::block_on(async {
                stored
                    .approval
                    .spawn_ssh(&stored.document, &self.plan, slave, stored.vault)
            })
            .map_err(|error| stored_error(&error))?;
            return Ok(SshSpawn { child, plan: self.plan, stored: Some(exchange) });
        }
        let child = slave
            .spawn_command(self.command)
            .map_err(|error| format!("spawn: {error:#}"))?;
        Ok(SshSpawn {
            child,
            plan: self.plan,
            #[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
            stored: None,
        })
    }

    pub(crate) fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub(crate) fn enable_editor(&mut self, app: tauri::AppHandle, owner: String, document: u64) -> tokio::sync::oneshot::Sender<String> {
        let (editor, id) = editor_binding(app, owner, document);
        self.editor = Some(editor);
        id
    }

    pub(crate) fn editor_requested(&self) -> bool { self.editor.is_some() }

    pub(crate) fn prepare_editor(&mut self, cancel: tokio_util::sync::CancellationToken) -> Option<(super::nano::Running, tokio::sync::oneshot::Sender<()>)> {
        let (binding, started) = self.editor.take()?;
        let invocation = self.plan.editor_invocation(true)?;
        let (ready, running) = super::nano::Running::prepare(Arc::clone(&self.plan), binding, cancel)?;
        let mut shell = command(invocation);
        shell.args([super::nano::HELPER, "shell", &ready.session_id]);
        self.command = shell;
        Some((running, started))
    }

    /// 터미널 감독자와 SFTP 작업자가 같은 계획을 공유한다.
    pub(crate) fn into_parts(self) -> (CommandBuilder, Arc<ConnectionPlan>) {
        (self.command, self.plan)
    }

    pub(crate) fn for_probe(mut self, nonce: u128) -> Self {
        self.command.get_argv_mut().insert(1, "-T".into());
        // 원격 명령의 출력에만 완성된 표식이 나타난다.
        self.command.arg(format!("printf 'ZZABBA-F01:%s%s\\n' '{:016x}' '{:016x}'", (nonce >> 64) as u64, nonce as u64));
        self
    }

    #[cfg(test)]
    pub(crate) fn replace_command(&mut self, command: CommandBuilder) {
        self.command = command;
    }

    #[cfg(test)]
    pub(crate) fn fixture(&mut self, executable: &Path) -> std::path::PathBuf {
        self.command.get_argv_mut()[0] = executable.as_os_str().to_owned();
        self.command.env("ZZABBA_F01_FIXTURE_WAIT", "1");
        self.plan.config.clone()
    }
}

type Editor = (super::nano::Binding, tokio::sync::oneshot::Sender<()>);

fn editor_binding(app: tauri::AppHandle, owner: String, document: u64) -> (Editor, tokio::sync::oneshot::Sender<String>) {
    let (id, parent) = tokio::sync::oneshot::channel();
    let (started, ready) = tokio::sync::oneshot::channel();
    ((super::nano::Binding { app, owner, document, parent, started: ready }, started), id)
}

type PrepareForward = Box<dyn FnOnce(&CancellationToken) -> Result<DirectLaunch, String> + Send>;

/// SSH-over-SSM 탭은 먼저 열고, PTY 작업자가 탭의 취소 토큰으로 전달과 접속 계획을 준비한다.
pub(crate) struct ForwardLaunch {
    prepare: PrepareForward,
    editor: Option<Editor>,
}

impl ForwardLaunch {
    /// 저장 암호가 아닌 SSH-over-SSM 대상은 PIN만 먼저 확인하고 전달 준비를 미룬다.
    fn saved(config: &Path, ssh: &Ssh) -> Result<Option<Self>, String> {
        if !matches!(ssh.transport, SshTransport::Ssm(_)) || matches!(ssh.auth, SshAuth::StoredPassword { .. }) {
            return Ok(None);
        }
        let endpoint = super::Endpoint::new(&ssh.host, ssh.port).map_err(pin_error)?;
        let approved = ApprovedHost::confirm(&endpoint, &endpoint, &ssh.host_key_fingerprint).map_err(pin_error)?;
        trust_store(config)?.pin(&approved).map_err(pin_error)?;
        let (config, ssh) = (config.to_owned(), ssh.clone());
        Ok(Some(Self { prepare: Box::new(move |cancel| saved(&config, &ssh, cancel)), editor: None }))
    }

    #[cfg(test)]
    pub(crate) fn fixture(prepare: impl FnOnce(&CancellationToken) -> Result<DirectLaunch, String> + Send + 'static) -> Self {
        Self { prepare: Box::new(prepare), editor: None }
    }

    /// 취소되면 이미 시작한 SSM 자식과 원격 세션은 전달을 버리는 경로로 정리된다.
    pub(crate) fn prepare(self, cancel: &CancellationToken) -> Result<DirectLaunch, String> {
        let mut launch = (self.prepare)(cancel)?;
        launch.editor = self.editor;
        Ok(launch)
    }
}

/// 터미널 탭이 여는 저장 SSH 실행이다.
pub(crate) enum SavedLaunch {
    Ready(DirectLaunch),
    Forward(ForwardLaunch),
}

impl SavedLaunch {
    pub(crate) fn enable_editor(&mut self, app: tauri::AppHandle, owner: String, document: u64) -> tokio::sync::oneshot::Sender<String> {
        match self {
            Self::Ready(launch) => launch.enable_editor(app, owner, document),
            Self::Forward(launch) => {
                let (editor, id) = editor_binding(app, owner, document);
                launch.editor = Some(editor);
                id
            }
        }
    }
}

#[cfg(all(test, windows))]
pub(crate) fn restrict_fixture_key(path: &Path) {
    super::trust_windows::create_file(path).unwrap();
}


/// 원격 sudo 승격은 배치 로그인이 가능한 직접 SSH(Agent·키 파일)에서만 연다.
pub(crate) fn remote_plan(config: &Path, ssh: &Ssh) -> Result<Arc<ConnectionPlan>, String> {
    if !matches!(ssh.transport, SshTransport::Direct {}) {
        return Err("원격 sudo 승격은 직접 SSH 연결에서만 사용할 수 있습니다. 터미널에서 직접 입력하세요".into());
    }
    if !matches!(ssh.auth, SshAuth::Agent { .. } | SshAuth::KeyFile { .. }) {
        return Err("원격 sudo 승격은 SSH Agent 또는 키 파일 연결에서만 사용할 수 있습니다. 터미널에서 직접 입력하세요".into());
    }
    let launch = saved(config, ssh, &CancellationToken::new())?;
    let (_, plan) = launch.into_parts();
    Ok(plan)
}

/// 같은 계획에서 승격 run·bridge·확인 채널을 만든다. 원격 인자는 고정 ASCII 토큰만 받는다.
pub(crate) struct RemoteChannel {
    pub(crate) terminal: CommandBuilder,
    pub(crate) piped: std::process::Command,
}

pub(crate) fn remote_channel(plan: &Arc<ConnectionPlan>, remote: &[String]) -> Result<RemoteChannel, String> {
    if remote.is_empty()
        || !remote.iter().all(|token| {
            !token.is_empty()
                && token.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"/._:-".contains(&byte))
        })
    {
        return Err("원격 승격 인자가 올바르지 않습니다".into());
    }
    let invocation = plan.invocation(Client::Ssh).map_err(pin_error)?;
    let mut terminal = CommandBuilder::new(invocation.program);
    terminal.get_argv_mut().insert(1, "-T".into());
    terminal.args(invocation.args.iter().cloned());
    terminal.args(remote.iter().cloned());
    for &(name, value) in invocation.environment {
        match value {
            Some(value) => terminal.env(name, value),
            None => terminal.env_remove(name),
        };
    }
    let mut piped = std::process::Command::new(invocation.program);
    piped.arg("-T");
    piped.args(invocation.args.iter().cloned());
    piped.args(remote.iter());
    for &(name, value) in invocation.environment {
        match value {
            Some(value) => piped.env(name, value),
            None => piped.env_remove(name),
        };
    }
    terminal.env_remove("BW_SESSION");
    piped.env_remove("BW_SESSION");
    for name in ["ZZABBA_ASKPASS_ENDPOINT", "ZZABBA_ASKPASS_BROKER_PID", "ZZABBA_ASKPASS_MODE"] {
        terminal.env_remove(name);
        piped.env_remove(name);
    }
    terminal.env("TERM", "xterm-256color");
    piped.env("TERM", "xterm-256color");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        piped.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        terminal.env_remove("PATHEXT");
    }
    Ok(RemoteChannel { terminal, piped })
}

/// SSH-over-SSM 대상은 SSM 포트 전달이 수신을 확인할 때까지 기다린다. `cancel`은 그 대기를 끊는다.
pub(crate) fn saved(config: &Path, ssh: &Ssh, cancel: &CancellationToken) -> Result<DirectLaunch, String> {
    let endpoint = super::Endpoint::new(&ssh.host, ssh.port).map_err(pin_error)?;
    prepare_saved(config, ssh, &endpoint, cancel)
}

/// 터미널 탭에서만 쓴다. 저장 암호 대상은 로컬 확인 창을 거친 한 번의 승인으로 실행한다.
/// SSH-over-SSM 대상은 탭을 먼저 열 수 있도록 전달 준비를 미룬다.
pub(crate) fn saved_interactive(
    config: &Path,
    ssh: &Ssh,
    owner: &str,
    document: u64,
    confirm: impl FnOnce(String) -> Result<bool, String>,
) -> Result<SavedLaunch, String> {
    if let Some(forward) = ForwardLaunch::saved(config, ssh)? {
        return Ok(SavedLaunch::Forward(forward));
    }
    #[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
    if let SshAuth::StoredPassword { item_id } = &ssh.auth {
        let endpoint = super::Endpoint::new(&ssh.host, ssh.port).map_err(pin_error)?;
        return prepare_stored(config, ssh, &endpoint, item_id, owner, document, confirm).map(SavedLaunch::Ready);
    }
    let _ = (owner, document, confirm);
    saved(config, ssh, &CancellationToken::new()).map(SavedLaunch::Ready)
}

#[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
fn prepare_stored(
    config: &Path,
    ssh: &Ssh,
    endpoint: &super::Endpoint,
    item: &str,
    owner: &str,
    document: u64,
    confirm: impl FnOnce(String) -> Result<bool, String>,
) -> Result<DirectLaunch, String> {
    let vault = crate::local_auth::Bitwarden::discover();
    prepare_stored_with(config, ssh, endpoint, item, owner, document, vault, confirm)
}

#[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
fn prepare_stored_with(
    config: &Path,
    ssh: &Ssh,
    endpoint: &super::Endpoint,
    item: &str,
    owner: &str,
    document: u64,
    vault: Result<crate::local_auth::Bitwarden, crate::local_auth::Error>,
    confirm: impl FnOnce(String) -> Result<bool, String>,
) -> Result<DirectLaunch, String> {
    prepare_stored_bounded(config, ssh, endpoint, item, owner, document, APPROVAL_TTL, vault, confirm)
}

#[allow(clippy::too_many_arguments)]
#[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
fn prepare_stored_bounded(
    config: &Path,
    ssh: &Ssh,
    endpoint: &super::Endpoint,
    item: &str,
    owner: &str,
    document: u64,
    budget: Duration,
    vault: Result<crate::local_auth::Bitwarden, crate::local_auth::Error>,
    confirm: impl FnOnce(String) -> Result<bool, String>,
) -> Result<DirectLaunch, String> {
    let vault = match vault {
        Ok(vault) => vault,
        Err(error) => return manual(config, ssh, endpoint, fallback_reason(&error)),
    };
    if !vault.has_session() {
        return manual(config, ssh, endpoint, "Bitwarden CLI 세션 키가 없습니다");
    }
    if let Err(error) = tauri::async_runtime::block_on(vault.ready()) {
        return manual(config, ssh, endpoint, fallback_reason(&error));
    }
    let approved = ApprovedHost::confirm(endpoint, endpoint, &ssh.host_key_fingerprint).map_err(pin_error)?;
    let store = trust_store(config)?;
    let transport = transport(&store, &approved, ssh, &CancellationToken::new())?;
    let plan = Arc::new(
        ConnectionPlan::prepare(
            &store,
            &approved,
            &ssh.account,
            transport,
            Authentication::StoredPassword { item },
        )
        .map_err(pin_error)?,
    );
    let details = plan.stored().ok_or("저장된 SSH 암호 계획이 없습니다")?;
    let message = format!(
        "서버: {}\n계정: {}\n호스트 키 SHA256 지문: {}\n금고 항목 ID: {}\n\n저장된 SSH 암호로 로그인하시겠습니까?",
        details.target, details.user, details.fingerprint, details.item,
    );
    let shown = Instant::now();
    if !confirm(message)? {
        return Err("저장된 SSH 암호 승인이 취소되었습니다".into());
    }
    if shown.elapsed() > budget {
        return Err("저장된 SSH 암호 승인이 만료되었습니다".into());
    }
    // 승인 기산점은 창을 닫은 뒤다. 승인 소비 규칙과 askpass 교환 기한은 그대로 둔다.
    let current = crate::local_auth::Document {
        owner: owner.into(),
        document,
        tab: uuid::Uuid::new_v4().to_string(),
    };
    let pending = crate::local_auth::Pending::request(current.clone(), Arc::clone(&plan))
        .map_err(|error| stored_error(&error))?;
    let approval = pending.approve(&current, &plan).map_err(|error| stored_error(&error))?;
    let command = command(plan.stored_ssh_invocation().map_err(pin_error)?);
    Ok(DirectLaunch {
        command,
        plan,
        editor: None,
        notice: None,
        stored: Some(StoredLaunch { document: current, approval, vault }),
    })
}

/// 금고를 쓸 수 없으면 같은 대상·계정·지문의 직접 입력으로 돌린다. 다른 자격 증명으로 재시도하지 않는다.
#[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
fn manual(config: &Path, ssh: &Ssh, endpoint: &super::Endpoint, reason: &str) -> Result<DirectLaunch, String> {
    let approved = ApprovedHost::confirm(endpoint, endpoint, &ssh.host_key_fingerprint).map_err(pin_error)?;
    let store = trust_store(config)?;
    let transport = transport(&store, &approved, ssh, &CancellationToken::new())?;
    let mut launch = DirectLaunch::prepare(
        &store,
        &approved,
        &ssh.account,
        &ssh.account,
        Authentication::ManualPassword,
        transport,
    )
    .map_err(pin_error)?;
    launch.notice = Some(format!("저장된 인증을 사용할 수 없어 직접 입력으로 전환했습니다: {reason}"));
    Ok(launch)
}

#[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
fn fallback_reason(error: &crate::local_auth::Error) -> &'static str {
    use crate::local_auth::Error as E;
    match error {
        E::VaultUnavailable => "Bitwarden CLI를 찾을 수 없습니다",
        E::VaultLocked => "Bitwarden 금고가 잠겨 있습니다",
        E::VaultUnauthenticated => "Bitwarden에 로그인되어 있지 않습니다",
        E::UntrustedExecutable => "신뢰할 수 있는 위치의 Bitwarden CLI가 아닙니다",
        _ => "Bitwarden 상태를 확인할 수 없습니다",
    }
}

#[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
fn stored_error(error: &crate::local_auth::Error) -> String {
    use crate::local_auth::Error as E;
    match error {
        E::Expired => "저장된 SSH 암호 승인이 만료되었습니다".into(),
        E::Cancelled => "저장된 SSH 암호 승인이 취소되었습니다".into(),
        E::InvalidApproval => "저장된 SSH 암호 승인 대상이 바뀌었습니다".into(),
        E::UntrustedExecutable => "저장된 인증 실행기 또는 Bitwarden 경로를 신뢰할 수 없습니다".into(),
        E::ChildUnavailable => "저장된 인증용 SSH 자식을 시작할 수 없습니다".into(),
        E::ChannelUnavailable => "저장된 인증 채널을 열 수 없습니다".into(),
        E::VaultUnavailable => "Bitwarden CLI를 찾을 수 없습니다".into(),
        E::VaultLocked => "Bitwarden 금고가 잠겨 있습니다".into(),
        E::VaultUnauthenticated => "Bitwarden에 로그인되어 있지 않습니다".into(),
        E::ItemMissing => "Bitwarden 항목을 찾을 수 없습니다".into(),
        other => format!("저장된 인증: {other:?}"),
    }
}

/// 탭이 열린 뒤 교환이 실패하면 사용자에게 금고 오류를 상태로 알린다.
#[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
pub(crate) fn stored_failure(error: &crate::local_auth::Error) -> Option<String> {
    match error {
        crate::local_auth::Error::Cancelled => None,
        other => Some(stored_error(other)),
    }
}

pub(crate) fn prepare_remote_editor(config: &Path, id: &str, revision: std::num::NonZeroU64, ssh: &Ssh)
    -> Result<(SavedLaunch, String), String>
{
    if !matches!(ssh.auth, SshAuth::Agent { .. } | SshAuth::KeyFile { .. }) {
        return Err("nano 연동에는 SSH Agent 또는 키 파일이 필요합니다".into());
    }
    let endpoint = super::Endpoint::new(&ssh.host, ssh.port).map_err(pin_error)?;
    let launch = match ForwardLaunch::saved(config, ssh)? {
        Some(forward) => SavedLaunch::Forward(forward),
        None => SavedLaunch::Ready(prepare_saved(config, ssh, &endpoint, &CancellationToken::new())?),
    };
    let message = format!(
        "프로필: {id}\n리비전: {revision}\n서버: {}\n포트: {}\n계정: {}\n호스트 키 지문: {}\n\n관리자가 SSH 셸과 SFTP에서 같은 경로가 같은 파일을 가리킨다고 확인했습니까?",
        endpoint.host, endpoint.port, ssh.account, ssh.host_key_fingerprint,
    );
    Ok((launch, message))
}

fn prepare_saved(config: &Path, ssh: &Ssh, endpoint: &super::Endpoint, cancel: &CancellationToken) -> Result<DirectLaunch, String> {
    let auth = match &ssh.auth {
        SshAuth::Agent { public_key_path } => Authentication::AgentFile { public_key_path },
        SshAuth::KeyFile { private_key_path } => Authentication::KeyFile { private_key: private_key_path },
        SshAuth::ManualPassword {} => Authentication::ManualPassword,
        SshAuth::StoredPassword { .. } => return Err("저장된 SSH 암호를 사용할 수 없습니다".into()),
    };
    let approved = ApprovedHost::confirm(endpoint, endpoint, &ssh.host_key_fingerprint).map_err(pin_error)?;
    let store = trust_store(config)?;
    let transport = transport(&store, &approved, ssh, cancel)?;
    DirectLaunch::prepare(&store, &approved, &ssh.account, &ssh.account, auth, transport).map_err(pin_error)
}

/// SSH-over-SSM은 등록된 PIN을 먼저 확인한 뒤 앱이 소유한 SSM 포트 전달을 열고 그 루프백 포트로 접속한다.
fn transport(store: &TrustStore, approved: &ApprovedHost<'_>, ssh: &Ssh, cancel: &CancellationToken) -> Result<Transport, String> {
    let SshTransport::Ssm(target) = &ssh.transport else { return Ok(Transport::Direct) };
    store.pin(approved).map_err(pin_error)?;
    let (port, forward) = crate::tunnels::ssm::open_ssh_forward(target, ssh.port, cancel)?;
    Ok(Transport::Ssm { port, forward: Box::new(forward) })
}

fn trust_store(config: &Path) -> Result<TrustStore, String> {
    match TrustStore::open(&config.join(super::TRUST_DIRECTORY)) {
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Err(pin_error(Error::UnregisteredHost)),
        result => result.map_err(pin_error),
    }
}

fn pin_error(error: Error) -> String {
    match error {
        Error::UnregisteredHost => "등록된 SSH 호스트 키가 없습니다".into(),
        Error::FingerprintMismatch => "등록된 SSH 호스트 키의 지문이 다릅니다".into(),
        Error::Io(error) => format!("SSH 준비: {error}"),
        error => format!("SSH 준비: {error:?}"),
    }
}

pub(crate) fn command(invocation: Invocation<'_>) -> CommandBuilder {
    let mut command = CommandBuilder::new(invocation.program);
    command.args(invocation.args);
    for &(name, value) in invocation.environment {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        }
    }
    command.env_remove("BW_SESSION");
    for name in ["ZZABBA_ASKPASS_ENDPOINT", "ZZABBA_ASKPASS_BROKER_PID"] {
        command.env_remove(name);
    }
    // ConPTY의 실행기 검색이 ssh.exe 부재 시 PATHEXT의 다른 확장자를 시도한다.
    #[cfg(windows)]
    command.env_remove("PATHEXT");
    command.env("TERM", "xterm-256color");
    command
}

#[cfg(test)]
mod tests;
