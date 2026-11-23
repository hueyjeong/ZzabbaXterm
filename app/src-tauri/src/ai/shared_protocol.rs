//! 독립 SSM 제어 채널의 프레이밍과 상태. 승인과 전송은 호출자가 관리한다.

use std::{num::NonZeroU64, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use zeroize::{Zeroize, Zeroizing};

use super::{
    control_wire::{ClientFrame, MAX_FRAME_BYTES, NodeFrame, VERSION, WireOperation, WireStream},
    shared_wire::{Client, Node, Ticket},
};

const START_BANNER: &[u8] = b"Starting session with SessionId: ";
const HISTORY_WARNING: &[u8] = b"Warning: Unable to record CLI history. Check file permissions for ./.";
// SSM 에이전트는 셸 프로필을 `sh`의 PTY에 입력하므로 배너 뒤에 프로필 명령이 한 번 에코된다.
const PROFILE_COMMAND: &[u8] = b"exec /usr/local/libexec/zzabba-ai-control --shared-control";
const PROFILE_PROMPTS: [&[u8]; 2] = [b"$ ", b"# "];
const MAX_OUTPUT_BYTES: usize = 1024;
const MAX_PAIR_MS: u64 = 60_000;

pub(crate) enum Event {
    Ready,
    HelloAck,
    Pairing { code: Zeroizing<String>, expires_in: Duration },
    Paired { uid: u32, shell_generation: NonZeroU64 },
    Denied,
    Claim { ticket: Ticket },
    Started { job_id: NonZeroU64 },
    Output { job_id: NonZeroU64, stream: WireStream, bytes: Vec<u8> },
    Exited { job_id: NonZeroU64, code: Option<i32>, signal: Option<i32> },
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ProtocolError {
    #[error("제어 채널 프레임이 크기 제한을 넘었습니다")]
    FrameTooLarge,
    #[error("제어 채널의 시작 출력이 올바르지 않습니다")]
    StartupOutput,
    #[error("제어 채널 프레임이 올바르지 않습니다")]
    InvalidFrame,
    #[error("제어 채널 상태 또는 프레임 순서가 일치하지 않습니다")]
    UnexpectedFrame,
    #[error("제어 채널 SSM CLI가 종료되었습니다 (코드 {0})")]
    CliExited(u32),
}

#[derive(Default)]
enum Phase {
    #[default]
    AwaitReady,
    Ready(String),
    AwaitHelloAck,
    AwaitPairing,
    AwaitPaired,
    Paired(NonZeroU64),
    Failed,
}

// 셸 프로필 에코는 배너 뒤 한 번이다. 프롬프트는 에코 앞이나 바로 다음 줄 앞에 한 번 온다.
#[derive(Default, Clone, Copy)]
enum ProfileEcho {
    #[default]
    None,
    AwaitingPrompt,
    Done,
}

#[derive(Default, Clone, Copy)]
enum Work {
    #[default]
    Idle,
    Reserved(Ticket, WireOperation),
    Claimed(Ticket, WireOperation),
    Granted(NonZeroU64),
    Running { id: NonZeroU64, stop_sent: bool },
}

#[derive(Default)]
pub(crate) struct SharedProtocol {
    phase: Phase,
    connected: bool,
    startup_bytes: usize,
    saw_blank: bool,
    saw_history_warning: bool,
    saw_banner: bool,
    profile_echo: ProfileEcho,
    partial: Zeroizing<Vec<u8>>,
    channel: String,
    incoming: u64,
    outgoing: u64,
    last_approval: u64,
    work: Work,
}

impl SharedProtocol {
    // PTY 바이트와 공식 연결 이벤트는 별도 스레드에서 도착할 수 있다.
    pub(crate) fn ssm_connected(&mut self) -> Result<Option<Event>, ProtocolError> {
        if self.connected || matches!(self.phase, Phase::Failed) {
            return Err(ProtocolError::UnexpectedFrame);
        }
        self.connected = true;
        Ok(matches!(self.phase, Phase::Ready(_)).then_some(Event::Ready))
    }

    // 한 번에 이벤트 하나만 넘기고, 처리하지 않은 입력은 호출자에게 남긴다.
    pub(crate) fn feed(&mut self, input: &mut &[u8]) -> Result<Option<Event>, ProtocolError> {
        let result = self.read(input);
        if result.is_err() {
            self.phase = Phase::Failed;
            self.partial.zeroize();
        }
        result
    }

    fn read(&mut self, input: &mut &[u8]) -> Result<Option<Event>, ProtocolError> {
        if matches!(self.phase, Phase::Failed) {
            return Err(ProtocolError::UnexpectedFrame);
        }
        while !input.is_empty() {
            let remaining = if matches!(self.phase, Phase::AwaitReady) {
                MAX_FRAME_BYTES - self.startup_bytes
            } else {
                MAX_FRAME_BYTES
            } - self.partial.len();
            let window = &input[..input.len().min(remaining)];
            let Some(end) = window.iter().position(|&byte| byte == b'\n') else {
                if input.len() >= remaining {
                    return Err(ProtocolError::FrameTooLarge);
                }
                if self.partial.capacity() == 0 {
                    self.partial.reserve_exact(MAX_FRAME_BYTES - 1);
                }
                self.partial.extend_from_slice(input);
                *input = &[];
                return Ok(None);
            };
            let (line, rest) = (*input).split_at(end + 1);
            *input = rest;
            let event = if self.partial.is_empty() {
                self.line(&line[..end])?
            } else {
                let mut assembled = std::mem::take(&mut self.partial);
                assembled.extend_from_slice(&line[..end]);
                let result = self.line(&assembled);
                assembled.as_mut_slice().zeroize();
                assembled.clear();
                self.partial = assembled;
                result?
            };
            if event.is_some() {
                return Ok(event);
            }
        }
        Ok(None)
    }

    fn line(&mut self, line: &[u8]) -> Result<Option<Event>, ProtocolError> {
        let awaiting_ready = matches!(self.phase, Phase::AwaitReady);
        if awaiting_ready {
            self.startup_bytes += line.len() + 1;
        }
        // Windows 로컬 콘솔은 LF를 CRLF로 출력할 수 있다. Linux 노드 프레임은 LF다.
        let line = if awaiting_ready || cfg!(windows) {
            line.strip_suffix(b"\r").unwrap_or(line)
        } else {
            line
        };
        #[cfg(windows)]
        let line = if awaiting_ready {
            strip_conpty_start(line).ok_or(ProtocolError::StartupOutput)?
        } else {
            line
        };
        // 프롬프트가 에코보다 늦으면 `exec` 직후의 Ready 프레임 앞에 붙는다.
        let line = match self.profile_echo {
            ProfileEcho::AwaitingPrompt if awaiting_ready => {
                self.profile_echo = ProfileEcho::Done;
                PROFILE_PROMPTS.iter().find_map(|prompt| line.strip_prefix(*prompt)).unwrap_or(line)
            }
            _ => line,
        };
        if awaiting_ready && !line.starts_with(b"{") {
            return self.preamble(line);
        }
        if line.contains(&b'\r') || !line.starts_with(b"{") {
            return Err(ProtocolError::InvalidFrame);
        }
        if let Ok(frame) = serde_json::from_slice::<NodeFrame<'_>>(line) {
            return self.control_frame(frame);
        }
        let frame = serde_json::from_slice::<Node<'_>>(line).map_err(|_| ProtocolError::InvalidFrame)?;
        self.shared_frame(frame).map(Some)
    }

    fn preamble(&mut self, line: &[u8]) -> Result<Option<Event>, ProtocolError> {
        if self.saw_banner && matches!(self.profile_echo, ProfileEcho::None) {
            if line == PROFILE_COMMAND {
                self.profile_echo = ProfileEcho::AwaitingPrompt;
                return Ok(None);
            }
            if PROFILE_PROMPTS.iter().any(|prompt| line.strip_prefix(*prompt) == Some(PROFILE_COMMAND)) {
                self.profile_echo = ProfileEcho::Done;
                return Ok(None);
            }
        }
        if !self.saw_banner {
            if line == HISTORY_WARNING && !self.saw_history_warning {
                self.saw_history_warning = true;
                return Ok(None);
            }
            if line.is_empty() && !self.saw_blank {
                self.saw_blank = true;
                return Ok(None);
            }
            if let Some(id) = line.strip_prefix(START_BANNER)
                && !id.is_empty()
                && id.len() <= 96
                && !id.starts_with(b"-")
                && id.iter().all(|byte| byte.is_ascii_alphanumeric() || b"_+=,.@-".contains(byte))
            {
                // 이 문구는 PTY 프리앰블일 뿐, 공식 연결 이벤트를 대체하지 않는다.
                self.saw_banner = true;
                return Ok(None);
            }
        }
        Err(ProtocolError::StartupOutput)
    }

    fn control_frame(&mut self, frame: NodeFrame<'_>) -> Result<Option<Event>, ProtocolError> {
        let event = match frame {
            NodeFrame::Ready { version: VERSION, nonce }
                if matches!(self.phase, Phase::AwaitReady) && lower_hex(nonce, 32) =>
            {
                self.phase = Phase::Ready(nonce.to_owned());
                return Ok(self.connected.then_some(Event::Ready));
            }
            NodeFrame::HelloAck { version: VERSION, channel }
                if matches!(self.phase, Phase::AwaitHelloAck) && channel == self.channel.as_str() =>
            {
                self.phase = Phase::AwaitPairing;
                Event::HelloAck
            }
            NodeFrame::Denied { channel, seq }
                if matches!(self.work, Work::Reserved(..) | Work::Claimed(..)) =>
            {
                self.accept(channel, seq)?;
                self.work = Work::Idle;
                Event::Denied
            }
            NodeFrame::Started { channel, job_id, seq }
                if matches!(self.work, Work::Granted(id) if id == job_id) =>
            {
                self.accept(channel, seq)?;
                self.work = Work::Running { id: job_id, stop_sent: false };
                Event::Started { job_id }
            }
            NodeFrame::Output { channel, job_id, seq, stream, data } if self.running(job_id) => {
                self.accept(channel, seq)?;
                if data.is_empty() || data.len() > MAX_OUTPUT_BYTES.div_ceil(3) * 4 {
                    return Err(ProtocolError::InvalidFrame);
                }
                let bytes = STANDARD.decode(data).map_err(|_| ProtocolError::InvalidFrame)?;
                if bytes.len() > MAX_OUTPUT_BYTES {
                    return Err(ProtocolError::InvalidFrame);
                }
                Event::Output { job_id, stream, bytes }
            }
            NodeFrame::Exited { channel, job_id, seq, code, signal } if self.running(job_id) => {
                if !matches!((code, signal), (Some(0..=255), None) | (None, Some(1..=64))) {
                    return Err(ProtocolError::InvalidFrame);
                }
                self.accept(channel, seq)?;
                self.work = Work::Idle;
                Event::Exited { job_id, code, signal }
            }
            _ => return Err(ProtocolError::UnexpectedFrame),
        };
        Ok(Some(event))
    }

    fn shared_frame(&mut self, frame: Node<'_>) -> Result<Event, ProtocolError> {
        match frame {
            Node::Pairing { channel, seq, code, expires_in_ms }
                if matches!(self.phase, Phase::AwaitPairing)
                    && lower_hex(code, 64) && (1..=MAX_PAIR_MS).contains(&expires_in_ms) =>
            {
                self.accept(channel, seq)?;
                self.phase = Phase::AwaitPaired;
                Ok(Event::Pairing {
                    code: Zeroizing::new(code.to_owned()),
                    expires_in: Duration::from_millis(expires_in_ms),
                })
            }
            Node::Paired { channel, seq, uid, shell_generation }
                if matches!(self.phase, Phase::AwaitPaired) =>
            {
                self.accept(channel, seq)?;
                self.phase = Phase::Paired(shell_generation);
                Ok(Event::Paired { uid, shell_generation })
            }
            Node::Claim { channel, seq, ticket } => {
                let Work::Reserved(expected, operation) = self.work else {
                    return Err(ProtocolError::UnexpectedFrame);
                };
                if expected != ticket {
                    return Err(ProtocolError::UnexpectedFrame);
                }
                self.accept(channel, seq)?;
                self.work = Work::Claimed(ticket, operation);
                Ok(Event::Claim { ticket })
            }
            _ => Err(ProtocolError::UnexpectedFrame),
        }
    }

    fn accept(&mut self, channel: &str, seq: NonZeroU64) -> Result<(), ProtocolError> {
        if channel != self.channel.as_str() || self.incoming.checked_add(1) != Some(seq.get()) {
            return Err(ProtocolError::UnexpectedFrame);
        }
        self.incoming = seq.get();
        Ok(())
    }

    fn running(&self, job_id: NonZeroU64) -> bool {
        matches!(self.work, Work::Running { id, .. } if id == job_id)
    }

    // 전송이 실패하면 호출자는 이 채널을 폐기한다. 프레임은 재전송하지 않는다.
    pub(crate) fn hello(&mut self, channel: &str) -> Result<Vec<u8>, ProtocolError> {
        let Phase::Ready(nonce) = &self.phase else {
            return Err(ProtocolError::UnexpectedFrame);
        };
        if !self.connected || !lower_hex(channel, 32) || channel == nonce.as_str() {
            return Err(ProtocolError::UnexpectedFrame);
        }
        let bytes = encode(ClientFrame::Hello { version: VERSION, nonce, channel })?;
        self.channel.push_str(channel);
        self.phase = Phase::AwaitHelloAck;
        Ok(bytes)
    }

    pub(crate) fn reserve(&mut self, ticket: Ticket, operation: WireOperation) -> Result<Vec<u8>, ProtocolError> {
        if !matches!(self.work, Work::Idle) || ticket.approval_id.get() <= self.last_approval
            || !matches!(self.phase, Phase::Paired(generation) if generation == ticket.shell_generation)
        {
            return Err(ProtocolError::UnexpectedFrame);
        }
        let bytes = self.send(|channel, seq| Client::Reserve { channel, seq, ticket, operation })?;
        self.last_approval = ticket.approval_id.get();
        self.work = Work::Reserved(ticket, operation);
        Ok(bytes)
    }

    // 제공자가 기존 앱 상태의 정확한 승인을 한 번 소비한 뒤에만 호출한다.
    pub(crate) fn grant(&mut self, ticket: Ticket, operation: WireOperation) -> Result<Vec<u8>, ProtocolError> {
        if !matches!(self.work, Work::Claimed(expected, op) if expected == ticket && op == operation) {
            return Err(ProtocolError::UnexpectedFrame);
        }
        let bytes = self.send(|channel, seq| Client::Grant { channel, seq, ticket, operation })?;
        self.work = Work::Granted(ticket.approval_id);
        Ok(bytes)
    }

    pub(crate) fn deny(&mut self, ticket: Ticket) -> Result<Vec<u8>, ProtocolError> {
        if !matches!(self.work, Work::Reserved(expected, _) | Work::Claimed(expected, _) if expected == ticket) {
            return Err(ProtocolError::UnexpectedFrame);
        }
        let bytes = self.send(|channel, seq| Client::Deny { channel, seq, ticket })?;
        self.work = Work::Idle;
        Ok(bytes)
    }

    pub(crate) fn stop(&mut self, job_id: NonZeroU64) -> Result<Vec<u8>, ProtocolError> {
        if !matches!(self.work, Work::Running { id, stop_sent: false } if id == job_id) {
            return Err(ProtocolError::UnexpectedFrame);
        }
        let bytes = self.send(|channel, seq| Client::Stop { channel, seq, job_id })?;
        self.work = Work::Running { id: job_id, stop_sent: true };
        Ok(bytes)
    }

    fn send(&mut self, frame: impl for<'a> FnOnce(&'a str, NonZeroU64) -> Client<'a>) -> Result<Vec<u8>, ProtocolError> {
        if !matches!(self.phase, Phase::Paired(_)) {
            return Err(ProtocolError::UnexpectedFrame);
        }
        let seq = self.outgoing.checked_add(1).and_then(NonZeroU64::new)
            .ok_or(ProtocolError::UnexpectedFrame)?;
        let bytes = encode(frame(&self.channel, seq))?;
        self.outgoing = seq.get();
        Ok(bytes)
    }

    pub(crate) fn cli_exited(&mut self, code: u32) -> ProtocolError {
        self.phase = Phase::Failed;
        self.partial.zeroize();
        ProtocolError::CliExited(code)
    }
}

fn encode(frame: impl serde::Serialize) -> Result<Vec<u8>, ProtocolError> {
    let mut bytes = serde_json::to_vec(&frame).map_err(|_| ProtocolError::InvalidFrame)?;
    if bytes.len() >= MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge);
    }
    bytes.push(b'\n');
    Ok(bytes)
}

fn lower_hex(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(windows)]
fn strip_conpty_start(mut line: &[u8]) -> Option<&[u8]> {
    const SEQUENCES: &[&[u8]] = &[b"\x1b[6n", b"\x1b[?9001h", b"\x1b[?1004h", b"\x1b[m", b"\x1b[?25h"];
    while line.starts_with(b"\x1b") {
        if let Some(title) = line.strip_prefix(b"\x1b]0;") {
            let end = title.iter().position(|&byte| byte == 7)?;
            if std::str::from_utf8(&title[..end]).ok()?.chars().any(char::is_control) {
                return None;
            }
            line = &title[end + 1..];
        } else {
            line = SEQUENCES.iter().find_map(|prefix| line.strip_prefix(*prefix))?;
        }
    }
    Some(line)
}

#[cfg(test)]
mod tests;
