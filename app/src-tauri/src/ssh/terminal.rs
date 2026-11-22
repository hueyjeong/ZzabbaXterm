use portable_pty::CommandBuilder;
use serde::Deserialize;
use std::path::Path;
use crate::connections::profile::{Ssh, SshAuth, SshTransport};

use super::{ApprovedHost, Authentication, Client, ConnectionPlan, Error, Invocation, Transport, TrustStore};

pub(crate) struct DirectLaunch {
    command: CommandBuilder,
    plan: ConnectionPlan,
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
        let plan = ConnectionPlan::prepare(store, approved_host, account, Transport::Direct, auth)?;
        let command = command(plan.invocation(Client::Ssh));
        Ok(Self { command, plan })
    }

    /// PTY 감독자는 자식 회수 뒤 세션을 제거하기 전에 계획을 해제한다.
    pub(crate) fn into_parts(self) -> (CommandBuilder, ConnectionPlan) {
        (self.command, self.plan)
    }

    pub(crate) fn for_probe(mut self) -> Self {
        self.command.arg("printf 'ZZABBA-F01:%d\\n' \"$((314000+159))\"");
        self
    }

    #[cfg(test)]
    pub(crate) fn fixture(&mut self, executable: &Path) -> std::path::PathBuf {
        self.command.get_argv_mut()[0] = executable.as_os_str().to_owned();
        self.command.env("ZZABBA_F01_FIXTURE_WAIT", "1");
        self.plan.config.clone()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PinReference {
    host: String,
    port: u16,
    host_key_fingerprint: String,
}

pub(crate) fn verify_pin(config: &Path, pin: &PinReference) -> Result<(), String> {
    let endpoint = super::Endpoint::new(&pin.host, pin.port).map_err(pin_error)?;
    let approved = ApprovedHost::confirm(&endpoint, &endpoint, &pin.host_key_fingerprint).map_err(pin_error)?;
    trust_store(config)?.pin(&approved).map(|_| ()).map_err(pin_error)
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
    match TrustStore::open(&config.join("ssh-trust")) {
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
