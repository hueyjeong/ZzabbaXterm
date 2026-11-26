//! 원격 sudo 승격의 확인·run·askpass 채널이다.
//!
//! 서버의 고정 helper를 같은 서버·계정의 별도 SSH 채널에서 실행한다. 승인 카드 뒤에만
//! askpass 채널이 암호를 1회 전달하며, run 채널(메인 PTY)에는 암호를 쓰지 않는다.

use std::{
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use portable_pty::CommandBuilder;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command as TokioCommand,
};
use tokio_util::sync::CancellationToken;

use crate::local_auth::{Bitwarden, Error};
use crate::ssh::ConnectionPlan;

use super::{Document, Request};

/// 관리자가 설치한 고정 경로다. 앱은 이 파일을 설치하지 않는다.
pub(crate) const HELPER: &str = "/usr/local/libexec/zxt-elevate";
const VERSION: &str = "zxt-elevate 1";
const VERIFY_TTL: Duration = Duration::from_secs(10);
const BRIDGE_TTL: Duration = Duration::from_secs(120);
const MAX_LINE: usize = 4096;

/// helper의 설치·신원을 별도 SSH 채널에서 확인한다. 실패하면 승인 창을 열지 않는다.
pub(crate) fn verify(plan: &Arc<ConnectionPlan>, account: &str) -> Result<(), String> {
    let mut command = crate::ssh::terminal::remote_channel(
        plan,
        &[HELPER.to_owned(), "--verify".to_owned()],
    )?
    .piped;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|_| unavailable("실행 파일을 시작할 수 없습니다"))?;
    let started = Instant::now();
    while child.try_wait().map_err(|_| unavailable("상태를 읽을 수 없습니다"))?.is_none() {
        if started.elapsed() >= VERIFY_TTL {
            let _ = child.kill();
            let _ = child.wait();
            return Err(unavailable("확인 응답 시간이 초과되었습니다"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().map_err(|_| unavailable("응답을 읽을 수 없습니다"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    parse_verify(&text, account)
        .ok_or_else(|| unavailable("설치·소유자·계정 조건이 맞지 않습니다"))?;
    Ok(())
}

fn unavailable(reason: &str) -> String {
    format!("원격 sudo 승격 helper를 확인할 수 없습니다({reason}). 터미널에서 직접 입력하세요")
}

/// `--verify` 응답을 검증해 표시 대상을 돌려준다.
pub(crate) fn parse_verify(text: &str, account: &str) -> Option<()> {
    let mut lines = text.lines();
    if lines.next()? != VERSION {
        return None;
    }
    let mut user = None;
    let mut owner = None;
    let mut mode = None;
    let mut path = None;
    for line in lines {
        let (key, value) = line.split_once('=')?;
        match key {
            "user" => user = Some(value),
            "owner" => owner = Some(value),
            "mode" => mode = Some(value),
            "path" => path = Some(value),
            _ => (),
        }
    }
    if user != Some(account)
        || owner != Some("0")
        || path != Some(HELPER)
        || mode
            .and_then(|mode| u32::from_str_radix(mode, 8).ok())
            .is_none_or(|mode| mode & 0o022 != 0)
    {
        return None;
    }
    Some(())
}

/// 승인 뒤 실행할 run·bridge 채널과 상태다.
pub(crate) struct Launch {
    pub(crate) command: CommandBuilder,
    pub(crate) frame: Vec<u8>,
    pub(crate) bridge: Bridge,
    pub(crate) state: Arc<State>,
}

pub(crate) struct Bridge {
    command: Command,
    id: String,
    nonce: String,
    item: String,
    vault: Bitwarden,
}

pub(crate) struct State {
    pub(crate) run_done: CancellationToken,
    pub(crate) cancel: CancellationToken,
    pub(crate) unconfirmed: AtomicBool,
    /// bridge가 응답을 받기 전에 끊겼을 때 run 자식을 끝낸다.
    pub(crate) abort: AtomicBool,
    /// 시험과 관찰을 위해 남긴다. 암호 내용은 담지 않는다.
    pub(crate) sent: AtomicBool,
    /// run/bridge의 `-F` 설정을 세션 수명 동안 유지한다.
    _plan: Option<Arc<ConnectionPlan>>,
}

impl State {
    pub(crate) fn unconfirmed(&self) -> bool {
        self.unconfirmed.load(Ordering::Relaxed)
    }

    pub(crate) fn abort(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn fixture(unconfirmed: bool, sent: bool) -> Self {
        Self {
            run_done: CancellationToken::new(),
            cancel: CancellationToken::new(),
            unconfirmed: AtomicBool::new(unconfirmed),
            abort: AtomicBool::new(false),
            sent: AtomicBool::new(sent),
            _plan: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn plan_config(&self) -> Option<std::path::PathBuf> {
        self._plan.as_ref().map(|plan| plan.config_path_for_test().to_owned())
    }
}

impl Launch {
    pub(crate) fn build(
        plan: Arc<ConnectionPlan>,
        document: &Document,
        request: &Request,
        vault: Bitwarden,
    ) -> Result<Self, String> {
        let nonce = super::request_id().ok_or("원격 승격 난수를 만들 수 없습니다")?;
        let id = document.request.clone();
        let program = request
            .program
            .to_str()
            .ok_or("실행 파일 경로가 UTF-8이 아닙니다")?;
        let frame = run_frame(&id, &nonce, program, &request.args)?;
        let run = crate::ssh::terminal::remote_channel(
            &plan,
            &[HELPER.to_owned(), "serve".into(), id.clone(), nonce.clone()],
        )?;
        let bridge = crate::ssh::terminal::remote_channel(
            &plan,
            &[HELPER.to_owned(), "bridge".into(), id.clone(), nonce.clone()],
        )?;
        Ok(Self {
            command: run.terminal,
            frame: frame.into_bytes(),
            bridge: Bridge {
                command: bridge.piped,
                id,
                nonce,
                item: request.item.clone(),
                vault,
            },
            state: Arc::new(State {
                run_done: CancellationToken::new(),
                cancel: CancellationToken::new(),
                unconfirmed: AtomicBool::new(false),
                abort: AtomicBool::new(false),
                sent: AtomicBool::new(false),
                _plan: Some(plan),
            }),
        })
    }
}

/// run 채널 프레임은 승인 ID·난수·절대 실행 파일·인자 배열만 담는다.
pub(crate) fn run_frame(id: &str, nonce: &str, program: &str, args: &[String]) -> Result<String, String> {
    if !token(id) || !token(nonce) || !program.starts_with('/') || args.len() > 64 {
        return Err("원격 승격 요청이 올바르지 않습니다".into());
    }
    let mut frame = format!(
        "ZXT-ELEVATE 1 {id} {nonce} {} {}",
        STANDARD.encode(program.as_bytes()),
        args.len()
    );
    for argument in args {
        frame.push(' ');
        frame.push_str(&STANDARD.encode(argument.as_bytes()));
    }
    frame.push('\n');
    Ok(frame)
}

fn token(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl Bridge {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn nonce(&self) -> &str {
        &self.nonce
    }

    /// bridge 채널은 askpass 요청을 받은 뒤에만 금고를 열고 암호를 1회 쓴다.
    pub(crate) async fn run(self, state: Arc<State>) -> Result<(), Error> {
        let mut command = self.command;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = TokioCommand::from(command)
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| Error::ChannelUnavailable)?;
        let mut stdin = child.stdin.take().ok_or(Error::ChannelUnavailable)?;
        let stdout = child.stdout.take().ok_or(Error::ChannelUnavailable)?;
        let stderr = child.stderr.take().ok_or(Error::ChannelUnavailable)?;
        let diagnostics = tokio::spawn(async move {
            let mut buffer = Vec::new();
            let _ = stderr.take(8192).read_to_end(&mut buffer).await;
            buffer
        });
        let mut reader = BufReader::new(stdout);
        let line = tokio::select! {
            biased;
            _ = state.cancel.cancelled() => {
                let _ = child.kill().await;
                return Err(Error::Cancelled);
            }
            _ = state.run_done.cancelled() => {
                let _ = child.kill().await;
                return Err(Error::Cancelled);
            }
            result = tokio::time::timeout(BRIDGE_TTL, read_line(&mut reader)) => {
                result.map_err(|_| Error::ChannelUnavailable)??
            }
        };
        parse_request(&line, &self.id, &self.nonce)?;
        let password = tokio::select! {
            biased;
            _ = state.cancel.cancelled() => {
                let _ = child.kill().await;
                return Err(Error::Cancelled);
            }
            _ = state.run_done.cancelled() => {
                let _ = child.kill().await;
                return Err(Error::Cancelled);
            }
            result = self.vault.password(&self.item) => result?,
        };
        let frame = zeroize::Zeroizing::new(format!(
            "PASSWORD 2 {} {} {}\n",
            self.id,
            self.nonce,
            STANDARD.encode(password.as_slice())
        ));
        state.sent.store(true, Ordering::Relaxed);
        if stdin.write_all(frame.as_bytes()).await.is_err()
            || stdin.shutdown().await.is_err()
        {
            let _ = child.kill().await;
            let _ = diagnostics.await;
            return Err(Error::ResponseUnknown);
        }
        drop(stdin);
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .map_err(|_| Error::ResponseUnknown)?
            .map_err(|_| Error::ResponseUnknown)?;
        let _ = diagnostics.await;
        if !status.success() {
            return Err(Error::ResponseUnknown);
        }
        Ok(())
    }
}

/// bridge의 요청 프레임은 정확히 `REQUEST 2 <승인 ID> <난수>`여야 한다.
/// 탭 수명 동안 bridge를 감시한다. run이 끝나기 전 채널이 끊기면 결과 미확인이다.
pub(crate) fn watch(bridge: Bridge, state: Arc<State>, notify: impl FnOnce(&'static str)) {
    let outcome = tauri::async_runtime::block_on(async {
        tokio::select! {
            biased;
            _ = state.cancel.cancelled() => None,
            _ = state.run_done.cancelled() => None,
            result = bridge.run(state.clone()) => Some(result),
        }
    });
    if let Some(Err(error)) = outcome
        && !state.cancel.is_cancelled()
        && !state.run_done.is_cancelled()
    {
        state.unconfirmed.store(true, Ordering::Relaxed);
        // 암호를 아직 건네지 않았으면 askpass가 기다리므로 run을 끝낸다.
        if !state.sent.load(Ordering::Relaxed) {
            state.abort.store(true, Ordering::Relaxed);
        }
        if let Some(message) = super::failure(error) {
            notify(message);
        }
    }
}

pub(crate) fn parse_request(line: &str, id: &str, nonce: &str) -> Result<(), Error> {
    let fields: Vec<&str> = line.trim_end_matches(['\r', '\n']).split(' ').collect();
    if fields.len() != 4
        || fields[0] != "REQUEST"
        || fields[1] != "2"
        || fields[2] != id
        || fields[3] != nonce
    {
        return Err(Error::ChannelUnavailable);
    }
    Ok(())
}

async fn read_line(reader: &mut BufReader<impl tokio::io::AsyncRead + Unpin>) -> Result<String, Error> {
    let mut line = Vec::new();
    while line.len() <= MAX_LINE {
        let byte = reader.read_u8().await.map_err(|_| Error::ChannelUnavailable)?;
        line.push(byte);
        if byte == b'\n' {
            return String::from_utf8(line).map_err(|_| Error::ChannelUnavailable);
        }
    }
    Err(Error::ChannelUnavailable)
}

#[cfg(test)]
mod tests;
