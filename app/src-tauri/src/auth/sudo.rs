use std::{io, path::Path, sync::atomic::AtomicBool, time::Duration};

use portable_pty::{Child, CommandBuilder};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::Instant,
};
use super::{
    ACK, BROKER_ENV, ENDPOINT_ENV, Error, Exchange, Bitwarden, peer, transport,
};

/// 관리자가 설치한 고정 경로의 sudo만 승격 실행기에 쓴다.
pub(crate) fn program() -> Result<peer::TrustedFile, Error> {
    #[cfg(target_os = "linux")]
    {
        ["/usr/bin/sudo", "/bin/sudo"]
            .into_iter()
            .map(Path::new)
            .find(|path| path.is_file())
            .ok_or(Error::UntrustedExecutable)
            .and_then(|path| peer::TrustedFile::check(path, "sudo"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(Error::UntrustedExecutable)
    }
}

const SUDO_ASKPASS_ENV: &str = "SUDO_ASKPASS";
pub(crate) const MODE_ENV: &str = "ZXT_ASKPASS_MODE";
pub(crate) const SUDO_MODE: &str = "sudo";
// 승인 뒤 금고 조회와 sudo의 askpass 시작을 모두 기다린다.
const EXCHANGE_TTL: Duration = Duration::from_secs(60);

pub(crate) fn spawn(
    program: &Path,
    args: &[String],
    helper: peer::TrustedFile,
    sudo: peer::TrustedFile,
    vault: Bitwarden,
    item: &str,
    spawn: impl FnOnce(CommandBuilder) -> Result<Box<dyn Child + Send + Sync>, Error>,
) -> Result<(Box<dyn Child + Send + Sync>, Exchange), Error> {
    tokio::runtime::Handle::try_current().map_err(|_| Error::ChannelUnavailable)?;
    let listener = transport::Listener::bind().map_err(|_| Error::ChannelUnavailable)?;
    let mut command = CommandBuilder::new(sudo.path());
    command.args(["-A", "--"]);
    command.arg(program);
    command.args(args);
    for key in [
        "BW_SESSION",
        "SSH_ASKPASS",
        "SSH_ASKPASS_REQUIRE",
        "SSH_ASKPASS_PROMPT",
        "SSH_AUTH_SOCK",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "LD_AUDIT",
        "PATHEXT",
        SUDO_ASKPASS_ENV,
        ENDPOINT_ENV,
        BROKER_ENV,
        MODE_ENV,
    ] {
        command.env_remove(key);
    }
    command.env(SUDO_ASKPASS_ENV, helper.path());
    command.env(MODE_ENV, SUDO_MODE);
    command.env(ENDPOINT_ENV, listener.address());
    command.env(BROKER_ENV, std::process::id().to_string());

    let item = item.to_owned();
    let mut child = spawn(command)?;
    let root = match peer::Process::from_child(&*child) {
        Ok(root) => root,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::ChildUnavailable);
        }
    };
    let exchange = Exchange::start_detached(Instant::now() + EXCHANGE_TTL, move |sent| async move {
        exchange(listener, root, helper, &vault, &item, &sent).await
    });
    Ok((child, exchange))
}

async fn exchange(
    mut listener: transport::Listener,
    root: peer::Process,
    helper: peer::TrustedFile,
    vault: &Bitwarden,
    item: &str,
    sent: &AtomicBool,
) -> Result<(), Error> {
    let (mut stream, candidate) = loop {
        let stream = tokio::select! {
            biased;
            // 캐시된 인증으로 sudo가 askpass를 부르지 않고 끝날 수 있다. 이때 금고 조회는 0회다.
            _ = root.exited() => return Ok(()),
            stream = listener.accept() => stream.map_err(channel)?,
        };
        if let Ok(candidate) = stream.peer() {
            if candidate.is_askpass_of(&root, &helper) {
                break (stream, candidate);
            }
        }
        listener.reject(stream).map_err(|_| Error::ChannelUnavailable)?;
    };
    let _proof = stream.read_u64().await.map_err(channel)?;
    // 이 연결 하나로만 소비한다. 조회 실패나 연결 종료에도 다시 받지 않는다.
    let password = tokio::select! {
        biased;
        _ = stream.read_u8() => return Err(Error::ChannelUnavailable),
        result = vault.password(item) => result?,
    };
    if !candidate.is_askpass_of(&root, &helper) || !root.alive() {
        return Err(Error::ChildUnavailable);
    }
    stream
        .write_u16(password.len() as u16)
        .await
        .map_err(channel)?;
    // 이 시점 뒤 연결이 닫히면 응답이 없었던 취소와 구별한다.
    sent.store(true, std::sync::atomic::Ordering::Relaxed);
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

/// 사용자에게 직접 입력을 안내할 때 쓰는 실패 문구다.
pub(crate) fn failure(error: Error) -> Option<&'static str> {
    match error {
        Error::VaultUnavailable => Some("Bitwarden CLI를 찾을 수 없습니다"),
        Error::VaultLocked => Some("Bitwarden 금고가 잠겨 있습니다"),
        Error::VaultUnauthenticated => Some("Bitwarden에 로그인되어 있지 않습니다"),
        Error::ItemMissing => Some("저장한 sudo 암호 항목을 찾을 수 없습니다"),
        Error::InvalidPassword => Some("저장한 sudo 암호가 올바르지 않습니다"),
        Error::Expired => Some("sudo 승인 시간이 만료되었습니다"),
        Error::ChannelUnavailable => Some("승격 채널이 끊겼습니다. 결과 미확인"),
        Error::ResponseUnknown => Some("승격 응답을 확인할 수 없습니다. 결과 미확인"),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
