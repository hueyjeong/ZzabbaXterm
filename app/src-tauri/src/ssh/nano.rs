use std::{io, process::Stdio, sync::Arc, time::Duration};

use tauri::AppHandle;
use tokio::{io::{AsyncRead, AsyncReadExt, AsyncWriteExt}, process::Command, sync::{mpsc, oneshot}, time::timeout};
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::editor::{nano_bridge, nano_wire::{MAX_PAYLOAD, Ready, Request, decode_len}};

use super::ConnectionPlan;

pub(super) const HELPER: &str = "/usr/local/libexec/zzabba-editor-bridge";
const PREPARE_LIMIT: Duration = Duration::from_secs(3);
const QUEUE: usize = 8;

pub(crate) struct Binding {
    pub(super) app: AppHandle,
    pub(super) owner: String,
    pub(super) document: u64,
    pub(super) started: oneshot::Receiver<()>,
    pub(super) parent: oneshot::Receiver<String>,
}

pub(crate) struct Running {
    stop: Option<DropGuard>,
    worker: Option<tauri::async_runtime::JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(worker) = self.worker.take() {
            let _ = tauri::async_runtime::block_on(worker);
        }
    }
}

impl Running {
    pub(super) fn prepare(plan: Arc<ConnectionPlan>, binding: Binding, cancel: CancellationToken) -> Option<(Ready, Self)> {
        let invocation = plan.editor_invocation(false)?;
        if cancel.is_cancelled() { return None; }
        let mut command = Command::new(invocation.program);
        command.args(invocation.args).args([HELPER, "editor-bridge"]);
        for &(name, value) in invocation.environment {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
        for name in ["BW_SESSION", "ZZABBA_ASKPASS_ENDPOINT", "ZZABBA_ASKPASS_BROKER_PID"] {
            command.env_remove(name);
        }
        #[cfg(windows)]
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        let (ready_tx, ready_rx) = oneshot::channel();
        let worker_cancel = cancel.clone();
        let worker = tauri::async_runtime::spawn(async move {
            // 자식을 회수할 때까지 같은 설정과 PIN 스냅샷을 보유한다.
            let _plan = plan;
            if worker_cancel.is_cancelled() { return; }
            let Ok(mut child) = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
                .kill_on_drop(true).spawn() else { return; };
            let stdin = child.stdin.take().expect("piped SSH stdin");
            let stdout = child.stdout.take().expect("piped SSH stdout");
            let session_cancel = worker_cancel.child_token();
            let exchange = exchange(stdin, stdout, binding, ready_tx, session_cancel.clone());
            tokio::pin!(exchange);
            let (reaped, exchange_done) = tokio::select! {
                status = child.wait() => (status.is_ok(), false),
                _ = worker_cancel.cancelled() => (false, false),
                _ = &mut exchange => (false, true),
            };
            session_cancel.cancel();
            if !reaped {
                let _ = child.start_kill();
                while child.wait().await.is_err() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let _ = child.start_kill();
                }
            }
            if !exchange_done { let _ = exchange.await; }
        });
        let running = Self { stop: Some(cancel.clone().drop_guard()), worker: Some(worker) };
        let ready = tauri::async_runtime::block_on(async {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                result = timeout(PREPARE_LIMIT, ready_rx) => result.ok()?.ok(),
            }
        })?;
        Some((ready, running))
    }
}

async fn frame<'a>(input: &mut (impl AsyncRead + Unpin), buffer: &'a mut [u8; MAX_PAYLOAD]) -> io::Result<&'a [u8]> {
    let mut header = [0; 4];
    input.read_exact(&mut header).await?;
    let payload = &mut buffer[..decode_len(header)?];
    input.read_exact(payload).await?;
    Ok(payload)
}

async fn exchange(
    mut stdin: tokio::process::ChildStdin,
    mut stdout: tokio::process::ChildStdout,
    binding: Binding,
    ready: oneshot::Sender<Ready>,
    cancel: CancellationToken,
) -> io::Result<()> {
    let mut buffer = [0; MAX_PAYLOAD];
    let ready_frame = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        result = frame(&mut stdout, &mut buffer) => Ready::decode(result?)?,
    };
    if ready.send(ready_frame).is_err() { return Ok(()); }
    let parent = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        result = binding.parent => match result {
            Ok(parent) => parent,
            Err(_) => return Ok(()),
        },
    };
    // PTY 시작 전에는 같은 부모의 SFTP 컨텍스트가 요청을 받을 수 없다.
    tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        result = binding.started => if result.is_err() { return Ok(()); },
    }
    let (requests, incoming) = mpsc::channel(QUEUE);
    let (responses, mut outgoing) = mpsc::channel(QUEUE);
    let bridge = nano_bridge::run(binding.app, binding.owner, binding.document, parent, cancel.clone(), incoming, responses);
    tokio::pin!(bridge);
    let result = tokio::select! {
        _ = &mut bridge => return Ok(()),
        _ = cancel.cancelled() => Ok(()),
        result = async {
            loop {
                let message = Request::decode(frame(&mut stdout, &mut buffer).await?)?;
                if requests.try_send(message).is_err() { return Err(io::ErrorKind::BrokenPipe.into()); }
            }
        } => result,
        result = async {
            while let Some(response) = outgoing.recv().await {
                stdin.write_all(&response.encode()?).await?;
            }
            Ok(())
        } => result,
    };
    cancel.cancel();
    drop(outgoing);
    bridge.await;
    result
}

#[cfg(test)]
#[tokio::test]
async fn frame_rejects_oversize_and_short_payloads() {
    let mut buffer = [0; MAX_PAYLOAD];
    let oversize = (MAX_PAYLOAD as u32 + 1).to_be_bytes();
    assert_eq!(frame(&mut &oversize[..], &mut buffer).await.unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert_eq!(frame(&mut &[0, 0, 0, 2, 1][..], &mut buffer).await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
}
