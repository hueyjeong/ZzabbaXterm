use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use portable_pty::PtySize;

use crate::{
    ssm::CancellationToken,
    terminal::{
        CENTRAL_UNAVAILABLE as UNAVAILABLE, CONTROL_CANCELLED as CANCELLED, CloseResult,
        ControlBinding, Event, LiveSsmIdentity, Sessions,
        process::{Session, Stop},
        remote::Target,
    },
};

const OPEN_TIMEOUT: Duration = Duration::from_secs(60);
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(10);
const MAX_INPUT: usize = 4096;
const TIMED_OUT: &str = "제어 세션 대기 시간이 초과되었습니다";

#[derive(Debug)]
pub(crate) enum Output {
    Bytes(Vec<u8>),
    // 공식 SSM 연결만 확인한 상태다. 노드 협상과 작업 실행은 별도다.
    SsmConnected,
    CliExited(u32),
}

/// 독립 PTY는 중앙 세션에 결부하며 앱 종료 시 전용 작업으로만 추적한다.
#[must_use]
pub(crate) struct ControlTransport {
    binding: Arc<ControlBinding>,
    session: Arc<Session>,
    output: mpsc::Receiver<Event>,
    open_deadline: Instant,
    connected: bool,
    received: u64,
    #[cfg(windows)]
    cursor_query: usize,
}

impl ControlTransport {
    /// 승인된 중앙 세션과 프로필로 블로킹 작업자에서 독립 운반을 연다.
    pub(crate) fn open(
        sessions: &Sessions,
        owner: &str,
        identity: &LiveSsmIdentity,
        cancel: &CancellationToken,
    ) -> Result<Self, String> {
        let binding = sessions.bind_ssm_control(owner, identity, cancel)?;
        let target = Target::new(&identity.profile, &identity.region, &identity.instance_id)?;
        // 미확인 PTY 출력 크레딧이 바이트 큐를 제한한다. 블로킹 전송은 종료를 막을 수 있다.
        let (tx, output) = mpsc::channel();
        let open_deadline = Instant::now() + OPEN_TIMEOUT;
        let (session, opened) = sessions.launch_control(&binding, PtySize::default(), target,
            move |event| tx.send(event).map_err(|_| CANCELLED.into()),
        )?;
        let transport = Self {
            binding, session, output,
            open_deadline, connected: false, received: 0,
            #[cfg(windows)]
            cursor_query: 0,
        };
        let result = opened.and_then(|()| transport.session.start_until(Some(open_deadline)));
        let result = result.and_then(|()| transport.require_active());
        if let Err(error) = result {
            return Err(match transport.close().warning() {
                Some(warning) if !error.contains(&warning) => format!("{error}; {warning}"),
                _ => error,
            });
        }
        Ok(transport)
    }

    /// 시작 안내와 노드 프레임의 구분은 프로토콜 소유자가 맡는다.
    pub(crate) fn read(&mut self) -> Result<Output, String> {
        let result = self.read_output();
        if let Err(error) = &result {
            self.session.stop(Stop::Disconnected(error.clone()));
        }
        result
    }

    fn read_output(&mut self) -> Result<Output, String> {
        let deadline = if self.connected { Instant::now() + READ_TIMEOUT } else { self.open_deadline };
        loop {
            match self.next_event(deadline)? {
                Event::Connecting => continue,
                Event::Ready { account_id } => {
                    self.require_active()?;
                    if account_id != self.binding.identity().account_id || !self.session.connected() {
                        return Err(UNAVAILABLE.into());
                    }
                    self.connected = true;
                    return Ok(Output::SsmConnected);
                }
                Event::Bytes(bytes) => {
                    #[cfg(windows)]
                    self.answer_console_cursor(&bytes)?;
                    self.received += bytes.len() as u64;
                    // 종료와 ACK가 경합해도 이미 받은 마지막 바이트는 전달한다.
                    let _ = self.session.ack(self.received);
                    return Ok(Output::Bytes(bytes));
                }
                Event::Exited(code) => {
                    self.connected = false;
                    return Ok(Output::CliExited(code));
                }
                Event::Disconnected(error) => return Err(error),
            }
        }
    }

    #[cfg(windows)]
    fn answer_console_cursor(&mut self, bytes: &[u8]) -> Result<(), String> {
        const QUERY: &[u8] = b"\x1b[6n";
        if self.connected || self.cursor_query == QUERY.len() {
            return Ok(());
        }
        for &byte in bytes {
            if byte == QUERY[self.cursor_query] {
                self.cursor_query += 1;
                if self.cursor_query == QUERY.len() {
                    // ConPTY의 초기 커서 상속 질의에 한 번만 응답한다.
                    return self.session.control_input(&self.binding, b"\x1b[1;1R", Instant::now() + WRITE_TIMEOUT, false);
                }
            } else {
                self.cursor_query = usize::from(byte == QUERY[0]);
            }
        }
        Ok(())
    }

    fn next_event(&self, deadline: Instant) -> Result<Event, String> {
        loop {
            self.require_active()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(TIMED_OUT.into());
            }
            match self.output.recv_timeout(remaining.min(POLL)) {
                Ok(event) => {
                    self.require_active()?;
                    if Instant::now() >= deadline {
                        return Err(TIMED_OUT.into());
                    }
                    return Ok(event);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err("제어 세션 출력이 닫혔습니다".into()),
            }
        }
    }

    fn require_active(&self) -> Result<(), String> {
        self.binding.admit(|| Ok(()))
    }

    /// 공식 연결 확인 후에만 프레임을 넣는다. 부분 쓰기 오류는 세션을 닫고 반환한다.
    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.is_empty() || bytes.len() > MAX_INPUT {
            return Err("제어 세션 입력 크기가 범위를 벗어났습니다".into());
        }
        let result = if self.connected {
            self.session.control_input(&self.binding, bytes, Instant::now() + WRITE_TIMEOUT, true)
        } else {
            self.require_active().and(Err("공식 SSM 연결을 확인할 수 없습니다".into()))
        };
        if let Err(error) = &result {
            self.session.stop(Stop::Disconnected(error.clone()));
        }
        result
    }

    /// 로컬 CLI 회수와 공식 SSM 종료 확인을 반환한다.
    pub(crate) fn close(self) -> CloseResult {
        self.session.stop(Stop::Closed);
        self.session.wait_finished();
        self.session.close_result()
    }
}

impl Drop for ControlTransport {
    fn drop(&mut self) {
        self.session.stop(Stop::Closed);
    }
}

#[cfg(test)]
mod tests;
