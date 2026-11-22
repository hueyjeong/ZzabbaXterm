use super::{Event, Sessions, shell, size};
use std::{
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

const WINDOW: usize = 256 * 1024;
const PARTIAL: usize = 17;
const RECORDS: usize = 8192;
const PAYLOAD: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ한";
const TIMEOUT: Duration = Duration::from_secs(20);
const QUIET: Duration = Duration::from_millis(250);

#[cfg(windows)]
const LOAD: &str = r#"
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
for ($i = 0; $i -lt @COUNT@; $i++) {
    [Console]::WriteLine(('L03-ROW:{0:D6}:@PAYLOAD@;' -f $i))
}
$a = [Console]::ReadLine()
$b = [Console]::ReadLine()
[Console]::WriteLine("L03-END:${a}:${b};")
exit 7
"#;
#[cfg(unix)]
const LOAD: &str = r#"
for ((i=0; i<@COUNT@; i++)); do
    printf 'L03-ROW:%06d:@PAYLOAD@;\n' "$i"
done
IFS= read -r a && IFS= read -r b || exit 1
printf 'L03-END:%s:%s;\n' "$a" "$b"
exit 7
"#;

#[cfg(windows)]
const PEER: &str = r#"$n = [int][Console]::ReadLine(); [Console]::WriteLine(('L03-PEER:{0};' -f ($n * $n)))"#;
#[cfg(unix)]
const PEER: &str = r#"IFS= read -r n && printf 'L03-PEER:%d;\n' "$((n * n))""#;

struct Pty<'a> {
    sessions: &'a Sessions,
    id: String,
    events: Receiver<Event>,
    output: Vec<u8>,
}

impl<'a> Pty<'a> {
    fn open(sessions: &'a Sessions, script: &str) -> Self {
        let mut command = shell();
        #[cfg(windows)]
        command.args(["-Command", script]);
        #[cfg(unix)]
        command.args(["-c", script]);
        let (tx, events) = mpsc::channel();
        let id = sessions
            .open("main", size(24, 120), command, move |event| {
                tx.send(event).map_err(|error| error.to_string())
            })
            .expect("네이티브 PTY 열기");
        sessions.start("main", &id).unwrap();
        let mut pty = Self { sessions, id, events, output: Vec::new() };
        #[cfg(windows)]
        {
            // ConPTY의 커서 위치 질의에 xterm과 같은 응답을 보낸다.
            pty.until_bytes(b"\x1b[6n");
            pty.input(b"\x1b[1;1R");
        }
        pty
    }

    fn input(&self, bytes: &[u8]) {
        self.sessions.input("main", &self.id, bytes).unwrap();
    }

    fn ack(&self, position: usize) {
        self.sessions.ack("main", &self.id, position as u64).unwrap();
    }

    fn next(&mut self, deadline: Instant) -> Event {
        let event = self.events.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| panic!("세션 {}: PTY 출력 {}바이트 뒤 이벤트 대기: {error}", self.id, self.output.len()));
        if let Event::Bytes(bytes) = &event {
            assert!((1..=16 * 1024).contains(&bytes.len()), "출력 조각이 16 KiB 범위를 벗어남: {}", bytes.len());
            self.output.extend_from_slice(bytes);
        }
        event
    }

    fn until_bytes(&mut self, marker: &[u8]) {
        let deadline = Instant::now() + TIMEOUT;
        let mut offset = 0;
        while !self.output[offset..].windows(marker.len()).any(|part| part == marker) {
            offset = self.output.len().saturating_sub(marker.len() - 1);
            assert!(matches!(self.next(deadline), Event::Bytes(_)), "표식 전에 PTY가 종료됨: {marker:?}");
        }
    }

    fn fill(&mut self, limit: usize) {
        let deadline = Instant::now() + TIMEOUT;
        while self.output.len() < limit {
            assert!(matches!(self.next(deadline), Event::Bytes(_)), "크레딧을 채우기 전에 PTY가 종료됨");
        }
        assert_eq!(self.output.len(), limit, "누적 미확인 출력 상한 초과");
    }

    fn assert_stalled(&self) {
        assert!(matches!(self.events.recv_timeout(QUIET), Err(RecvTimeoutError::Timeout)),
            "크레딧을 모두 사용한 뒤 ACK 없이 PTY 이벤트가 도착함");
    }
}

fn load(sessions: &Sessions) -> Pty<'_> {
    Pty::open(sessions, &LOAD.replace("@COUNT@", &RECORDS.to_string()).replace("@PAYLOAD@", PAYLOAD))
}

// 네이티브 open/input/close가 반환하지 않는 회귀도 시험 전체를 멈추지 않게 한다.
fn bounded(run: impl FnOnce() + Send + 'static) {
    let (done, complete) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
        let _ = done.send(result);
    });
    let result = complete.recv_timeout(TIMEOUT * 3).expect("실제 PTY 시나리오 또는 회수 제한 시간 초과");
    worker.join().unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn l03_pty_credit_resumes_in_order_without_blocking_input_or_peer() {
    bounded(|| {
        let sessions = Sessions::default();
        let mut a = load(&sessions);
        a.fill(WINDOW);
        a.assert_stalled();

        a.input(b"11");
        a.input(b"1\r");
        a.input(b"222\r");
        let mut b = Pty::open(&sessions, PEER);
        b.input(b"9\r");
        b.until_bytes(b"L03-PEER:81;");
        b.ack(b.output.len());
        assert!(matches!(a.events.try_recv(), Err(mpsc::TryRecvError::Empty)), "B가 응답하는 동안 A의 출력 상한이 풀림");

        a.ack(PARTIAL);
        a.fill(WINDOW + PARTIAL);
        a.ack(PARTIAL);
        a.ack(PARTIAL - 1);
        a.assert_stalled();

        a.ack(a.output.len());
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match a.next(deadline) {
                Event::Bytes(_) => a.ack(a.output.len()),
                Event::Exited(code) => {
                    assert_eq!(code, 7);
                    break;
                }
                Event::Disconnected(reason) => panic!("자연 종료 대신 PTY 단절: {reason}"),
            }
        }

        // ConPTY가 줄 사이에 생성하는 VT 제어는 허용하되 레코드 안의 원본 바이트를 비교한다.
        let prefix = b"L03-ROW:";
        let mut rows = a.output.windows(prefix.len()).enumerate()
            .filter_map(|(offset, bytes)| (bytes == prefix).then_some(offset));
        let mut last_end = 0;
        for i in 0..RECORDS {
            let offset = rows.next().unwrap_or_else(|| panic!("레코드 {i} 누락"));
            let row = format!("L03-ROW:{i:06}:{PAYLOAD};");
            last_end = offset + row.len();
            assert_eq!(a.output.get(offset..last_end), Some(row.as_bytes()), "레코드 {i}의 순서 또는 바이트 불일치");
        }
        assert!(rows.next().is_none(), "추가 레코드 수신");
        let end = b"L03-END:111:222;";
        assert_eq!(a.output.windows(b"L03-END:".len()).filter(|bytes| *bytes == b"L03-END:").count(), 1,
            "종료 표식 누락 또는 중복");
        assert!(a.output[last_end..].windows(end.len()).any(|bytes| bytes == end),
            "입력 순서 또는 마지막 출력과 종료 순서가 다름");
    });
}

#[test]
fn l03_pty_close_reaps_credit_blocked_session_without_ack() {
    bounded(|| {
        let sessions = Sessions::default();
        let mut a = load(&sessions);
        a.fill(WINDOW);
        a.assert_stalled();

        sessions.close("main", &a.id).expect("크레딧 대기 중 세션 닫기");
        assert!(matches!(a.events.try_recv(), Err(mpsc::TryRecvError::Disconnected)),
            "닫기 완료 뒤 출력 송신자가 남았거나 이벤트가 추가됨");
        assert!(sessions.get("main", &a.id).is_err(), "닫기 완료 뒤 세션이 등록부에 남음");
    });
}
