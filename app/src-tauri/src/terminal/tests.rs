use super::*;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const SIZE: PtySize = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
#[cfg(windows)]
const COMPUTE: &[u8] = b"Write-Output (7+5)\r";
#[cfg(unix)]
const COMPUTE: &[u8] = b"printf '%s\\n' \"$((7+5))\"\r";
#[cfg(windows)]
const PID: &[u8] = b"Write-Output $PID\r";
#[cfg(unix)]
const PID: &[u8] = b"echo $$\r";
#[cfg(windows)]
const INTERRUPTIBLE: &[u8] = b"Write-Output (10+3); Start-Sleep 60\r";
#[cfg(unix)]
const INTERRUPTIBLE: &[u8] = b"printf '%s\\n' \"$((10+3))\"; sleep 60\r";

struct Peer<'a> {
    sessions: &'a Sessions,
    id: String,
    rx: Receiver<Event>,
    bytes: Vec<u8>,
}

impl<'a> Peer<'a> {
    fn open(sessions: &'a Sessions) -> Self {
        let (tx, rx) = mpsc::channel();
        let id = sessions.open("main", SIZE, shell(), move |event| tx.send(event).map_err(|e| e.to_string())).expect("L01: 네이티브 셸 열기");
        sessions.start("main", &id).unwrap();
        let peer = Self { sessions, id, rx, bytes: Vec::new() };
        #[cfg(windows)]
        let peer = {
            let mut peer = peer;
            let deadline = Instant::now() + Duration::from_secs(15);
            while !peer.bytes.windows(4).any(|bytes| bytes == b"\x1b[6n") {
                assert!(matches!(peer.next(deadline), Event::Bytes(_)));
            }
            // xterm answers ConPTY's initial cursor-position query through onData.
            peer.send(b"\x1b[1;1R");
            peer
        };
        peer
    }

    fn send(&self, command: &[u8]) {
        self.sessions.input("main", &self.id, command).unwrap();
    }

    fn next(&mut self, deadline: Instant) -> Event {
        let event = self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|e| panic!("L01: PTY 출력/종료 대기: {e}; {:?}", String::from_utf8_lossy(&self.bytes)));
        if let Event::Bytes(bytes) = &event {
            self.bytes.extend_from_slice(bytes);
            let _ = self.sessions.ack("main", &self.id, self.bytes.len() as u64);
        }
        event
    }

    fn number(&mut self, from: usize) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(matches!(self.next(deadline), Event::Bytes(_)));
            let text = String::from_utf8_lossy(&self.bytes[from..]);
            for line in text.split_inclusive('\n').filter(|line| line.ends_with('\n')) {
                // ConPTY may wrap rendered lines in VT color/cursor sequences.
                let mut visible = String::new();
                let mut chars = line.chars();
                while let Some(ch) = chars.next() {
                    if ch == '\x1b' && chars.next() == Some('[') {
                        for code in chars.by_ref() {
                            if ('@'..='~').contains(&code) { break; }
                        }
                    } else {
                        visible.push(ch);
                    }
                }
                if let Ok(value) = visible.trim().parse() { return value; }
            }
        }
    }
}

pub(crate) fn assert_live_shell_progress(sessions: &Sessions) {
    let mut peer = Peer::open(sessions);
    peer.send(COMPUTE);
    assert_eq!(peer.number(0), 12);
    sessions.ack("main", &peer.id, peer.bytes.len() as u64).unwrap();
}

impl Drop for Peer<'_> {
    fn drop(&mut self) { let _ = self.sessions.close("main", &self.id); }
}

#[test]
fn l01_terminal_calculates_12_and_reports_exit_7_after_output() {
    let sessions = Sessions::default();
    let mut peer = Peer::open(&sessions);
    peer.send(COMPUTE);
    assert_eq!(peer.number(0), 12);
    let before_interrupt = peer.bytes.len();
    peer.send(INTERRUPTIBLE);
    assert_eq!(peer.number(before_interrupt), 13);
    let after_interrupt = peer.bytes.len();
    peer.send(b"\x03");
    #[cfg(windows)]
    {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !peer.bytes[after_interrupt..].windows(3).any(|bytes| bytes == b"PS ") {
            assert!(matches!(peer.next(deadline), Event::Bytes(_)));
        }
    }
    peer.send(COMPUTE);
    assert_eq!(peer.number(after_interrupt), 12);
    peer.send(b"exit 7\r");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match peer.next(deadline) {
            Event::Bytes(_) => {},
            Event::Exited(code) => { assert_eq!(code, 7); break; }
            Event::Disconnected(reason) => panic!("L01: 자연 종료 대신 단절: {reason}"),
            other => panic!("예상하지 못한 로컬 이벤트: {other:?}"),
        }
    }
    assert!(sessions.input("main", &peer.id, b"x").is_err());
}

#[test]
fn l01_terminal_closing_one_pid_leaves_the_other_shell_alive() {
    let sessions = Sessions::default();
    let mut a = Peer::open(&sessions);
    let mut b = Peer::open(&sessions);
    a.send(PID);
    b.send(PID);
    let (pid_a, pid_b) = (a.number(0), b.number(0));
    eprintln!("L01: 독립 셸 PID {pid_a}, {pid_b}");
    assert_ne!(pid_a, pid_b);
    assert!(sessions.input("other", &a.id, b"exit\r").is_err());
    assert!(sessions.close("other", &a.id).is_err());
    sessions.close("main", &a.id).unwrap();
    assert!(sessions.input("main", &a.id, b"x").is_err());
    let from = b.bytes.len();
    assert!(sessions.resize("main", &b.id, PtySize { rows: 0, ..SIZE }).is_err());
    assert!(sessions.resize("main", &b.id, PtySize { cols: 0, ..SIZE }).is_err());
    assert!(sessions.ack("main", &b.id, u64::MAX).is_err());
    sessions.ack("main", &b.id, 0).unwrap();
    b.send(COMPUTE);
    assert_eq!(b.number(from), 12);
}

#[test]
fn l01_terminal_failed_opens_report_the_stage() {
    let sessions = Sessions::default();
    let missing = CommandBuilder::new("zzabbaxterm-l01-does-not-exist-2d3987.exe");
    let err = sessions.open("main", SIZE, missing, |_| Ok(())).unwrap_err();
    assert!(err.starts_with("spawn:"), "{err}");
    for size in [PtySize { rows: 0, ..SIZE }, PtySize { cols: 0, ..SIZE }] {
        let err = sessions.open("main", size, shell(), |_| Ok(())).unwrap_err();
        assert!(err.starts_with("size:"), "{err}");
    }
}

#[test]
fn l01_terminal_navigation_retires_old_document_and_accepts_new_document() {
    let sessions = Sessions::default();
    let old = Peer::open(&sessions);
    let document = sessions.document("main");
    let delayed_open = || sessions.open_document("main", document, SIZE, super::Launch::Local(shell()), |_| Ok(()));
    sessions.navigate("main");
    assert!(sessions.input("main", &old.id, b"x").is_err());
    assert_eq!(delayed_open().unwrap_err(), "open: 문서가 바뀌었습니다");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match old.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::Bytes(_)) => {},
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            other => panic!("이전 문서 채널 회수: {other:?}"),
        }
    }
    let mut new = Peer::open(&sessions);
    new.send(COMPUTE);
    assert_eq!(new.number(0), 12);
}

#[test]
fn l01_terminal_missing_start_reaps_only_the_unstarted_session() {
    let sessions = Sessions::default();
    let mut active = Peer::open(&sessions);
    active.send(COMPUTE);
    assert_eq!(active.number(0), 12);

    let (tx, rx) = mpsc::channel();
    let orphan = sessions.open("main", SIZE, shell(), move |event| tx.send(event).map_err(|e| e.to_string())).unwrap();
    match rx.recv_timeout(Duration::from_secs(12)) {
        Ok(Event::Disconnected(reason)) => assert!(reason.starts_with("start:"), "{reason}"),
        other => panic!("미시작 세션 회수: {other:?}"),
    }
    assert!(matches!(rx.recv_timeout(Duration::from_secs(2)), Err(mpsc::RecvTimeoutError::Disconnected)));
    assert!(sessions.start("main", &orphan).is_err());

    let from = active.bytes.len();
    active.send(COMPUTE);
    assert_eq!(active.number(from), 12);
}

#[test]
fn l01_terminal_stalled_output_ack_disconnects_even_below_the_window() {
    let sessions = Sessions::default();
    let (tx, rx) = mpsc::channel();
    let id = sessions.open("main", SIZE, shell(), move |event| tx.send(event).map_err(|e| e.to_string())).unwrap();
    sessions.start("main", &id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(22);
    let mut received = 0;
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap() {
            Event::Bytes(bytes) => received += bytes.len(),
            Event::Disconnected(reason) => {
                assert!(reason.starts_with("ack:"), "{reason}");
                break;
            }
            Event::Exited(code) => panic!("ACK 단절 대신 종료: {code}"),
            other => panic!("예상하지 못한 로컬 이벤트: {other:?}"),
        }
    }
    assert!((1..WINDOW).contains(&received));
    assert!(sessions.input("main", &id, b"x").is_err());
}

#[cfg(windows)]
#[test]
fn l01_terminal_exit_259_is_not_a_running_process() {
    let sessions = Sessions::default();
    let mut peer = Peer::open(&sessions);
    peer.send(b"exit 259\r");
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match peer.next(deadline) {
            Event::Bytes(_) => {},
            Event::Exited(code) => { assert_eq!(code, 259); break; }
            Event::Disconnected(reason) => panic!("종료 259 대신 단절: {reason}"),
            other => panic!("예상하지 못한 로컬 이벤트: {other:?}"),
        }
    }
}
