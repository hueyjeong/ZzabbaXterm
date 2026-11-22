use portable_pty::CommandBuilder;
use std::path::Path;
use std::sync::Arc;
use crate::connections::profile::{Ssh, SshAuth, SshTransport};

use super::{ApprovedHost, Authentication, Client, ConnectionPlan, Error, Invocation, Transport, TrustStore};

pub(crate) struct DirectLaunch {
    command: CommandBuilder,
    plan: Arc<ConnectionPlan>,
    editor: Option<(super::nano::Binding, tokio::sync::oneshot::Sender<()>)>,
}

impl DirectLaunch {
    pub(crate) fn prepare(
        store: &TrustStore,
        approved_host: &ApprovedHost<'_>,
        account: &str,
        approved_account: &str,
        auth: Authentication<'_>,
    ) -> Result<Self, Error> {
        if account != approved_account {
            return Err(Error::ApprovedAccountMismatch);
        }
        let plan = Arc::new(ConnectionPlan::prepare(store, approved_host, account, Transport::Direct, auth)?);
        let command = command(plan.invocation(Client::Ssh));
        Ok(Self { command, plan, editor: None })
    }

    pub(crate) fn enable_editor(&mut self, app: tauri::AppHandle, owner: String, document: u64) -> tokio::sync::oneshot::Sender<String> {
        let (id, parent) = tokio::sync::oneshot::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        self.editor = Some((super::nano::Binding { app, owner, document, parent, started: ready }, started));
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
    pub(crate) fn fixture(&mut self, executable: &Path) -> std::path::PathBuf {
        self.command.get_argv_mut()[0] = executable.as_os_str().to_owned();
        self.command.env("ZZABBA_F01_FIXTURE_WAIT", "1");
        self.plan.config.clone()
    }
}

#[cfg(all(test, windows))]
pub(crate) fn restrict_fixture_key(path: &Path) {
    super::trust_windows::create_file(path).unwrap();
}


pub(crate) fn saved(config: &Path, ssh: &Ssh) -> Result<DirectLaunch, String> {
    if !matches!(ssh.transport, SshTransport::Direct {}) {
        return Err("SSH-over-SSM을 사용할 수 없습니다".into());
    }
    let auth = match &ssh.auth {
        SshAuth::Agent { public_key_path } => Authentication::AgentFile { public_key_path },
        SshAuth::KeyFile { private_key_path } => Authentication::KeyFile { private_key: private_key_path },
        SshAuth::ManualPassword {} => Authentication::ManualPassword,
        SshAuth::StoredPassword { .. } => return Err("저장된 SSH 암호를 사용할 수 없습니다".into()),
    };
    // 저장본의 실제 대상과 사용자 확인값을 사용하며, 매번 PIN을 다시 검사한다.
    let endpoint = super::Endpoint::new(&ssh.host, ssh.port).map_err(pin_error)?;
    let approved = ApprovedHost::confirm(&endpoint, &endpoint, &ssh.host_key_fingerprint).map_err(pin_error)?;
    DirectLaunch::prepare(&trust_store(config)?, &approved, &ssh.account, &ssh.account, auth).map_err(pin_error)
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

fn command(invocation: Invocation<'_>) -> CommandBuilder {
    let mut command = CommandBuilder::new(invocation.program);
    command.args(invocation.args);
    for &(name, value) in invocation.environment {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        }
    }
    command.env_remove("BW_SESSION");
    // ConPTY의 실행기 검색이 ssh.exe 부재 시 PATHEXT의 다른 확장자를 시도한다.
    #[cfg(windows)]
    command.env_remove("PATHEXT");
    command.env("TERM", "xterm-256color");
    command
}

#[cfg(test)]
mod tests;
