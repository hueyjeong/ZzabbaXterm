use std::{io, process::Stdio, sync::Arc, time::Duration};

use tauri::AppHandle;
use tokio::{io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt}, process::Command, sync::{mpsc, oneshot}, time::timeout};
use tokio_util::sync::{CancellationToken, DropGuard};
use zeroize::{Zeroize, Zeroizing};

use crate::editor::{nano_bridge, nano_wire::{CHALLENGE_LEN, Lease, MAX_PAYLOAD, NO_PROOF, PROOF_LEN, PTY_FRAME_LEN, PTY_PREFIX, Ready, Request, STARTUP_LIMIT, decode_len}};

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
    startup: Option<Startup>,
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
    pub(crate) fn take_startup(&mut self) -> Option<Startup> { self.startup.take() }

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
        let (proof_tx, proof_rx) = oneshot::channel();
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
            let exchange = exchange(stdin, stdout, binding, ready_tx, proof_rx, session_cancel.clone());
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
        let running = Self {
            stop: Some(cancel.clone().drop_guard()), worker: Some(worker), startup: Some(Startup::new(proof_tx)),
        };
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

const PTY_STARTUP_BYTES: usize = 64 * 1024;

pub(crate) struct Startup {
    pending: Zeroizing<[u8; PTY_FRAME_LEN]>,
    len: usize,
    proof: Option<oneshot::Sender<Zeroizing<[u8; PROOF_LEN]>>>,
    remaining: usize,
    active: bool,
}

impl Startup {
    fn new(proof: oneshot::Sender<Zeroizing<[u8; PROOF_LEN]>>) -> Self {
        Self { pending: Zeroizing::new([0; PTY_FRAME_LEN]), len: 0, proof: Some(proof), remaining: PTY_STARTUP_BYTES, active: true }
    }

    pub(crate) fn pending(&self) -> bool { self.active }

    // 예약된 시작 프레임 한 건만 소비하고 나머지 바이트는 그대로 전달한다.
    pub(crate) fn filter(&mut self, input: &[u8], mut emit: impl FnMut(&[u8])) {
        let mut run = 0;
        let mut offset = 0;
        while offset < input.len() && self.pending() {
            // 64 KiB 뒤에는 권한만 철회한다. 지연된 시작 응답은 처음 한 건까지 숨긴다.
            if self.remaining == 0 && self.len < PTY_PREFIX.len() { self.proof.take(); }
            self.remaining = self.remaining.saturating_sub(1);
            let byte = input[offset];
            if self.len < PTY_PREFIX.len() {
                if byte == PTY_PREFIX[self.len] {
                    if run != offset { emit(&input[run..offset]); }
                    self.push(byte);
                    offset += 1;
                    run = offset;
                } else if self.len == 0 {
                    offset += 1;
                } else {
                    emit(&self.pending[..self.len]);
                    self.len = 0;
                    self.remaining += 1;
                }
            } else {
                let end = self.len == PTY_FRAME_LEN - 1;
                let valid = if end { byte == b'\x07' } else { matches!(byte, b'0'..=b'9' | b'a'..=b'f') };
                if !valid {
                    self.finish(&mut emit);
                    break;
                }
                self.push(byte);
                offset += 1;
                run = offset;
                if end {
                    if let Some(tx) = self.proof.take() {
                        let _ = tx.send(self.decode());
                    }
                    self.active = false;
                    self.len = 0;
                    self.pending.zeroize();
                }
            }
        }
        if run != input.len() { emit(&input[run..]); }
    }

    fn push(&mut self, byte: u8) {
        self.pending[self.len] = byte;
        self.len += 1;
    }

    fn decode(&self) -> Zeroizing<[u8; PROOF_LEN]> {
        let hex = |byte| if byte <= b'9' { byte - b'0' } else { byte - b'a' + 10 };
        let mut proof = Zeroizing::new([0; PROOF_LEN]);
        for (out, pair) in proof.iter_mut().zip(self.pending[PTY_PREFIX.len()..PTY_FRAME_LEN - 1].chunks_exact(2)) {
            *out = hex(pair[0]) << 4 | hex(pair[1]);
        }
        proof
    }

    pub(crate) fn finish(&mut self, mut emit: impl FnMut(&[u8])) {
        if (1..PTY_PREFIX.len()).contains(&self.len) { emit(&self.pending[..self.len]); }
        self.proof.take();
        self.active = false;
        self.pending.zeroize();
        self.len = 0;
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
    proof: oneshot::Receiver<Zeroizing<[u8; PROOF_LEN]>>,
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
    tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        result = timeout(STARTUP_LIMIT, verify_startup(&mut stdin, &mut stdout, proof)) => {
            result.map_err(|_| io::ErrorKind::TimedOut)??;
        }
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

async fn verify_startup(
    stdin: &mut (impl AsyncWrite + Unpin),
    stdout: &mut (impl AsyncRead + Unpin),
    mut proof: oneshot::Receiver<Zeroizing<[u8; PROOF_LEN]>>,
) -> io::Result<()> {
    let challenge = async {
        let mut header = [0; 4];
        stdout.read_exact(&mut header).await?;
        if decode_len(header)? != CHALLENGE_LEN { return Err(io::ErrorKind::InvalidData.into()); }
        let mut bytes = Zeroizing::new([0; CHALLENGE_LEN]);
        stdout.read_exact(&mut bytes[..]).await?;
        Lease::decode_challenge(&bytes[..])
    };
    tokio::pin!(challenge);
    let (decoded, observed) = tokio::select! {
        decoded = &mut challenge => (decoded?, proof.await.map_err(|_| io::ErrorKind::InvalidData)?),
        observed = &mut proof => {
            let observed = observed.map_err(|_| io::ErrorKind::InvalidData)?;
            if *observed == NO_PROOF { return Err(io::ErrorKind::PermissionDenied.into()); }
            (challenge.await?, observed)
        }
    };
    let (lease, pid, sid) = decoded;
    if *observed == NO_PROOF || *observed != lease.proof(pid, sid) {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    stdin.write_all(&lease.confirm()[..]).await
}

#[cfg(test)]
#[tokio::test]
async fn frame_rejects_oversize_and_short_payloads() {
    let mut buffer = [0; MAX_PAYLOAD];
    let oversize = (MAX_PAYLOAD as u32 + 1).to_be_bytes();
    assert_eq!(frame(&mut &oversize[..], &mut buffer).await.unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert_eq!(frame(&mut &[0, 0, 0, 2, 1][..], &mut buffer).await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
}

#[cfg(test)]
mod startup_tests {
    use super::*;
    use crate::editor::nano_wire::startup_frame;

    #[tokio::test]
    async fn only_the_bound_pty_response_confirms_the_auxiliary_lease() {
        let lease = Lease::issue().unwrap();
        let challenge = lease.challenge(17, 23);
        for (observed, accepted) in [(lease.proof(17, 23), true), (lease.proof(18, 23), false), (NO_PROOF, false)] {
            let (tx, rx) = oneshot::channel();
            assert!(tx.send(Zeroizing::new(observed)).is_ok());
            let mut reply = Vec::new();
            let result = verify_startup(&mut reply, &mut &challenge[..], rx).await;
            if accepted {
                assert!(result.is_ok());
                assert!(lease.confirmed(&reply[4..]));
            } else {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
                assert!(reply.is_empty());
            }
        }
    }

    #[test]
    fn startup_hides_only_the_first_frame_across_every_read_split() {
        let proof = [0x5a; PROOF_LEN];
        let frame = startup_frame(proof);
        let before = "한글\x1b[6n\x1b]0;제목\x07\x1b".as_bytes();
        let after = "입력\r\n".as_bytes();
        let input = [before, frame.as_slice(), after, frame.as_slice()].concat();
        let expected = [before, after, frame.as_slice()].concat();
        for split in 0..=input.len() {
            let (tx, mut rx) = oneshot::channel();
            let mut startup = Startup::new(tx);
            let mut output = Vec::new();
            for part in [&input[..split], &input[split..]] {
                startup.filter(part, |part| output.extend_from_slice(part));
            }
            assert_eq!(output, expected);
            assert_eq!(*rx.try_recv().unwrap(), proof);
            assert!(!startup.pending());
        }
    }

    #[test]
    fn startup_preserves_partial_normal_escapes_and_bounds_search_bytes() {
        let (tx, mut rx) = oneshot::channel();
        let mut startup = Startup::new(tx);
        let mut output = Vec::new();
        startup.filter(b"left\x1b]7777;zz", |part| output.extend_from_slice(part));
        startup.finish(|part| output.extend_from_slice(part));
        assert_eq!(output, b"left\x1b]7777;zz");
        assert!(rx.try_recv().is_err());

        let (tx, mut rx) = oneshot::channel();
        let mut startup = Startup::new(tx);
        let mut input = vec![b'x'; PTY_STARTUP_BYTES];
        input.extend_from_slice(&startup_frame([7; PROOF_LEN]));
        output.clear();
        startup.filter(&input, |part| output.extend_from_slice(part));
        assert_eq!(output, &input[..PTY_STARTUP_BYTES]);
        assert!(!startup.pending());
        assert!(rx.try_recv().is_err());
    }
}
