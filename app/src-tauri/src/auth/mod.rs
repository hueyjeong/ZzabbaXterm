pub(crate) mod peer;
pub(crate) mod sudo;
mod transport;
pub(crate) mod vault;

#[cfg(test)]
mod tests;

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use portable_pty::{Child, CommandBuilder, SlavePty};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

pub use vault::Bitwarden;
pub(crate) use peer::{TrustedFile, installed_helper};

use crate::ssh::ConnectionPlan;

const TTL: Duration = Duration::from_secs(30);
// 승인 뒤 실제 askpass 교환은 금고 조회·OpenSSH 시작을 포함하므로 승인 창과 분리해 잰다.
const EXCHANGE_TTL: Duration = Duration::from_secs(60);
const ACK: u8 = 1;
const MAX_PASSWORD: usize = 1022; // OpenSSH의 1024바이트 askpass 버퍼에서 개행과 NUL을 제외한다.
const HELPER: &str = if cfg!(windows) {
    "zxt-askpass.exe"
} else {
    "zxt-askpass"
};
const ENDPOINT_ENV: &str = "ZXT_ASKPASS_ENDPOINT";
const BROKER_ENV: &str = "ZXT_ASKPASS_BROKER_PID";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Document {
    pub(crate) owner: String,
    pub(crate) document: u64,
    pub(crate) tab: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidRequest,
    InvalidApproval,
    Cancelled,
    Expired,
    UntrustedExecutable,
    ChildUnavailable,
    ChannelUnavailable,
    ResponseUnknown,
    VaultUnavailable,
    VaultLocked,
    VaultUnauthenticated,
    ItemMissing,
    VaultFailed,
    InvalidPassword,
}

/// 로컬 확인 창에는 이 값만 표시한다. 엔드포인트와 금고 세션은 백엔드에 남는다.
#[must_use]
pub(crate) struct Pending {
    document: Document,
    plan: Arc<ConnectionPlan>,
    expires: Instant,
}

/// 승인에서 실행까지 복제하거나 다시 결합할 수 없는 소유권이다.
#[must_use]
pub(crate) struct Approval(Pending);

impl Pending {
    pub(crate) fn request(document: Document, plan: Arc<ConnectionPlan>) -> Result<Self, Error> {
        let stored = plan.stored().ok_or(Error::InvalidApproval)?;
        if [&document.owner, &document.tab]
            .into_iter()
            .any(|s| s.trim().is_empty() || s.chars().any(char::is_control))
            || !vault::valid_item(&stored.item)
        {
            return Err(Error::InvalidApproval);
        }
        Ok(Self {
            document,
            plan,
            expires: Instant::now() + TTL,
        })
    }

    /// 로컬 확인 창은 별도 입력값 대신 이 계획의 직접 SSH 대상, 계정, 지문, 금고 항목을 표시한다.
    #[cfg(test)]
    pub(crate) fn details(&self) -> &crate::ssh::Stored {
        self.plan.stored().expect("stored SSH plan required")
    }

    pub(crate) fn approve(
        self,
        current: &Document,
        plan: &Arc<ConnectionPlan>,
    ) -> Result<Approval, Error> {
        self.check(current, plan)?;
        Ok(Approval(self))
    }

    fn check(&self, current: &Document, plan: &Arc<ConnectionPlan>) -> Result<(), Error> {
        if &self.document != current || !Arc::ptr_eq(&self.plan, plan) {
            return Err(Error::InvalidApproval);
        }
        if Instant::now() >= self.expires {
            return Err(Error::Expired);
        }
        Ok(())
    }
}

impl Approval {
    /// 기존 PTY 수명 관리자가 자식을 회수하며 Exchange는 승인 계획을 함께 보관한다.
    pub(crate) fn spawn_ssh(
        self,
        current: &Document,
        plan: &Arc<ConnectionPlan>,
        slave: &dyn SlavePty,
        vault: Bitwarden,
    ) -> Result<(Box<dyn Child + Send + Sync>, Exchange), Error> {
        self.0.check(current, plan)?;
        let helper = peer::installed_helper()?;
        self.spawn(
            current,
            plan,
            |command| {
                slave
                    .spawn_command(command)
                    .map_err(|_| Error::ChildUnavailable)
            },
            vault,
            helper,
        )
    }

    fn spawn(
        self,
        current: &Document,
        plan: &Arc<ConnectionPlan>,
        spawn: impl FnOnce(CommandBuilder) -> Result<Box<dyn Child + Send + Sync>, Error>,
        vault: Bitwarden,
        helper: peer::TrustedFile,
    ) -> Result<(Box<dyn Child + Send + Sync>, Exchange), Error> {
        self.0.check(current, plan)?;
        let Pending { plan, .. } = self.0;
        let invocation = plan.stored_ssh_invocation().map_err(|_| Error::InvalidApproval)?;
        let program = peer::TrustedFile::check(invocation.program, peer::SSH)?;
        let mut command = crate::ssh::terminal::command(invocation);
        tokio::runtime::Handle::try_current().map_err(|_| Error::ChannelUnavailable)?;
        let listener = transport::Listener::bind().map_err(|_| Error::ChannelUnavailable)?;

        command.get_argv_mut()[0] = program.path().as_os_str().to_owned();
        remove_ssh_inherited(&mut command);
        command.env("SSH_ASKPASS", helper.path());
        command.env("SSH_ASKPASS_REQUIRE", "force");
        command.env(ENDPOINT_ENV, listener.address());
        command.env(BROKER_ENV, std::process::id().to_string());

        let mut child = spawn(command)?;
        let root = peer::Process::from_child(&*child)
            .ok()
            .filter(|p| p.matches(&program));
        let Some(root) = root else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::ChildUnavailable);
        };
        let exchange = Exchange::start(Instant::now() + EXCHANGE_TTL, plan, move |plan, sent| async move {
            let item = &plan.stored().expect("stored SSH plan required").item;
            exchange(listener, root, program, helper, &vault, item, &sent).await
        });
        Ok((child, exchange))
    }
}

/// SSH askpass 자식은 상속 환경의 sudo 승격·금고 변수를 받지 않는다.
fn remove_ssh_inherited(command: &mut CommandBuilder) {
    for key in [
        "BW_SESSION",
        "SSH_ASKPASS_PROMPT",
        "SSH_AUTH_SOCK",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "LD_AUDIT",
        "PATHEXT",
        "SUDO_ASKPASS",
        sudo::MODE_ENV,
    ] {
        command.env_remove(key);
    }
}

/// 탭/문서 수명에 보관한다. 취소 또는 drop은 미사용 응답과 실행 중인 bw를 폐기한다.
#[must_use]
pub struct Exchange {
    cancel: CancellationToken,
    task: JoinHandle<Result<(), Error>>,
    _plan: Option<Arc<ConnectionPlan>>,
}

impl Exchange {
    fn start<F>(
        expires: Instant,
        plan: Arc<ConnectionPlan>,
        run: impl FnOnce(Arc<ConnectionPlan>, Arc<AtomicBool>) -> F,
    ) -> Self
    where
        F: Future<Output = Result<(), Error>> + Send + 'static,
    {
        let cancel = CancellationToken::new();
        let cancelled = cancel.clone();
        let sent = Arc::new(AtomicBool::new(false));
        let run = run(plan.clone(), sent.clone());
        let task = tokio::spawn(async move {
            let outcome = tokio::select! {
                biased;
                _ = cancelled.cancelled() => Err(Error::Cancelled),
                _ = tokio::time::sleep_until(expires) => Err(Error::Expired),
                result = run => return result,
            };
            if sent.load(Ordering::Relaxed) {
                Err(Error::ResponseUnknown)
            } else {
                outcome
            }
        });
        Self {
            cancel,
            task,
            _plan: Some(plan),
        }
    }

    /// 연결 계획 없이 승인 한 건만 감시한다. sudo 교환은 plan 대신 탭 수명 토큰으로 취소한다.
    pub(crate) fn start_detached<F>(expires: Instant, run: impl FnOnce(Arc<AtomicBool>) -> F) -> Self
    where
        F: Future<Output = Result<(), Error>> + Send + 'static,
    {
        let cancel = CancellationToken::new();
        let cancelled = cancel.clone();
        let sent = Arc::new(AtomicBool::new(false));
        let run = run(sent.clone());
        let task = tokio::spawn(async move {
            let outcome = tokio::select! {
                biased;
                _ = cancelled.cancelled() => Err(Error::Cancelled),
                _ = tokio::time::sleep_until(expires) => Err(Error::Expired),
                result = run => return result,
            };
            if sent.load(Ordering::Relaxed) {
                Err(Error::ResponseUnknown)
            } else {
                outcome
            }
        });
        Self {
            cancel,
            task,
            _plan: None,
        }
    }

    /// 교환 결과 관찰을 시험할 때 고정 결과를 돌려준다.
    #[cfg(test)]
    pub(crate) fn fixture_result(result: Result<(), Error>) -> Self {
        let cancel = CancellationToken::new();
        let task = tauri::async_runtime::block_on(async move {
            tokio::spawn(async move { result })
        });
        Self {
            cancel,
            task,
            _plan: None,
        }
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub async fn result(&mut self) -> Result<(), Error> {
        (&mut self.task)
            .await
            .unwrap_or(Err(Error::ChannelUnavailable))
    }
}

impl Drop for Exchange {
    fn drop(&mut self) {
        self.cancel();
    }
}

async fn exchange(
    mut listener: transport::Listener,
    root: peer::Process,
    program: peer::TrustedFile,
    helper: peer::TrustedFile,
    vault: &Bitwarden,
    item: &str,
    sent: &AtomicBool,
) -> Result<(), Error> {
    let (mut stream, candidate) = loop {
        let stream = tokio::select! {
            biased;
            _ = root.exited() => return Err(Error::ChildUnavailable),
            stream = listener.accept() => stream.map_err(channel)?,
        };
        if let Ok(candidate) = stream.peer() {
            if candidate.is_child_of(&root, &program, &helper) {
                break (stream, candidate);
            }
        }
        listener
            .reject(stream)
            .map_err(|_| Error::ChannelUnavailable)?;
    };
    let proof = stream.read_u64().await.map_err(channel)?;
    let output = candidate.verified_output(&root, proof).map_err(channel)?;
    if !candidate.is_child_of(&root, &program, &helper) {
        return Err(Error::ChildUnavailable);
    }
    // 검증한 연결 하나로만 소비한다. 조회 실패나 연결 종료에도 다시 받지 않는다.
    let password = tokio::select! {
        biased;
        _ = stream.read_u8() => return Err(Error::ChannelUnavailable),
        result = vault.password(item) => result?,
    };
    if !candidate.is_child_of(&root, &program, &helper) || !output.matches(&root) {
        return Err(Error::ChildUnavailable);
    }
    stream
        .write_u16(password.len() as u16)
        .await
        .map_err(channel)?;
    // 이 시점 뒤 연결이 닫히면 응답이 없었던 취소와 구별한다.
    sent.store(true, Ordering::Relaxed);
    stream
        .write_all(&password)
        .await
        .map_err(|_| Error::ResponseUnknown)?;
    drop(password);
    match stream.read_u8().await {
        Ok(ACK) => Ok(()),
        _ => Err(Error::ResponseUnknown),
    }
}

fn channel(_: io::Error) -> Error {
    Error::ChannelUnavailable
}

/// 전용 바이너리 진입점이다. 실패 시 stdout/stderr에 내용을 쓰지 않는다.
#[doc(hidden)]
pub fn run_askpass() -> bool {
    transport::run_helper().is_ok()
}
