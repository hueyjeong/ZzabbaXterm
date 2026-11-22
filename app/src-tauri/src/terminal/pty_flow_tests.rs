use super::{CLOSED, Event, Sessions, shell, size};
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

#[cfg(windows)]
const PROMPT: &[u8] = b"function prompt { 'L03-' + 'READY> ' }\r";
#[cfg(unix)]
const PROMPT: &[u8] = b"PS1='L03-''READY> '\r";
const READY: &[u8] = b"L03-READY>";

#[cfg(windows)]
const FLOOD: &[u8] = b"while ($true) { [Console]::WriteLine('L03-FLOOD:0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ;') }\r";
#[cfg(unix)]
const FLOOD: &[u8] = b"yes 'L03-FLOOD:0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ;'\r";

#[cfg(windows)]
const AFTER_INTERRUPT: &[u8] = b"Write-Output ('L03-' + 'AFTER:' + (9 * 9) + ';')\r";
#[cfg(unix)]
const AFTER_INTERRUPT: &[u8] = b"printf 'L03-AFTER:%d;\\n' \"$((9 * 9))\"\r";

struct Pty<'a> {
    sessions: &'a Sessions,
    id: String,
    events: Receiver<Event>,
    output: Vec<u8>,
}

impl<'a> Pty<'a> {
    fn open(sessions: &'a Sessions, script: Option<&str>) -> Self {
        let mut command = shell();
        if let Some(script) = script {
            #[cfg(windows)]
            command.args(["-Command", script]);
            #[cfg(unix)]
            command.args(["-c", script]);
        }
        Self::open_with(sessions, command)
    }

    fn open_with(sessions: &'a Sessions, command: portable_pty::CommandBuilder) -> Self {
        Self::open_launch(sessions, super::Launch::Local(command))
    }

    fn open_launch(sessions: &'a Sessions, launch: super::Launch) -> Self {
        let document = sessions.document("main");
        let (tx, events) = mpsc::channel();
        let id = sessions
            .open_document("main", document, size(24, 120), launch, move |event| {
                tx.send(event).map_err(|error| error.to_string())
            })
            .expect("네이티브 PTY 열기");
        sessions.start("main", &id).unwrap();
        let pty = Self { sessions, id, events, output: Vec::new() };
        #[cfg(windows)]
        {
            let mut pty = pty;
            // ConPTY의 커서 위치 질의에 xterm과 같은 응답을 보낸다.
            pty.until_bytes(b"\x1b[6n", 0, false);
            pty.input(b"\x1b[1;1R");
            pty
        }
        #[cfg(unix)]
        pty
    }

    fn input(&self, bytes: &[u8]) {
        self.sessions.input("main", &self.id, bytes).unwrap();
    }

    fn ack(&self, position: usize) {
        self.sessions.ack("main", &self.id, position as u64).unwrap();
    }

    fn next(&mut self, deadline: Instant) -> Event {
        let now = Instant::now();
        assert!(now < deadline, "세션 {}: PTY 출력 {}바이트 뒤 이벤트 대기 제한 시간 초과", self.id, self.output.len());
        let event = self.events.recv_timeout(deadline - now)
            .unwrap_or_else(|error| panic!("세션 {}: PTY 출력 {}바이트 뒤 이벤트 대기: {error}; 끝부분 {:?}", self.id, self.output.len(), String::from_utf8_lossy(&self.output[self.output.len().saturating_sub(256)..])));
        if let Event::Bytes(bytes) = &event {
            assert!((1..=16 * 1024).contains(&bytes.len()), "출력 조각이 16 KiB 범위를 벗어남: {}", bytes.len());
            self.output.extend_from_slice(bytes);
        }
        event
    }

    fn until_bytes(&mut self, marker: &[u8], mut offset: usize, ack: bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !self.output[offset..].windows(marker.len()).any(|part| part == marker) {
            offset = offset.max(self.output.len().saturating_sub(marker.len() - 1));
            if ack {
                self.ack(self.output.len());
            }
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
        let event = self.events.recv_timeout(QUIET);
        assert!(matches!(event, Err(RecvTimeoutError::Timeout)), "ACK를 보류했지만 PTY 이벤트가 도착함: {event:?}");
    }
}

#[test]
fn f01_two_saved_ssh_launches_reap_only_the_owned_plan_and_child() {
    use crate::ssh::{ApprovedHost, Endpoint, TrustStore, TRUST_DIRECTORY};
    use serde_json::json;

    bounded(|| {
        let config = tempfile::tempdir().unwrap();
        let config = config.path();
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti";
        let sha = "SHA256:UCUiLr7Pjs9wFFJMDByLgc3NrtdU344OgUM45wZPcIQ";
        let pin = json!({"host":"dev.example.invalid", "port":2222, "hostKeyFingerprint":sha});
        let key_path = config.join("fixture.pub");
        std::fs::write(&key_path, key).unwrap();
        let mut ssh = pin;
        ssh["account"] = json!("approved-user");
        ssh["transport"] = json!({"kind":"direct"});
        ssh["auth"] = json!({"kind":"agent", "publicKeyPath": key_path});
        let mut file = json!({"version":1, "groups":[{"revision":1,"profile":{"id":"work","displayName":"작업"}}],
            "targets":[{"revision":1,"profile":{"id":"ssh","groupId":"work","displayName":"개발 서버","defaultMethod":"ssh","ssh":ssh}}]});
        let profile = config.join("connections.json");
        let save = |file: &serde_json::Value| std::fs::write(&profile, serde_json::to_vec(file).unwrap()).unwrap();
        save(&file);
        assert!(matches!(super::saved::launch(config, "ssh"), Err(error) if error == "등록된 SSH 호스트 키가 없습니다"));
        let store = TrustStore::create(&config.join(TRUST_DIRECTORY)).unwrap();
        let endpoint = Endpoint::new("dev.example.invalid", 2222).unwrap();
        store.register(&ApprovedHost::confirm(&endpoint, &endpoint, sha).unwrap(), key).unwrap();
        let fingerprint = file.pointer_mut("/targets/0/profile/ssh/hostKeyFingerprint").unwrap();
        let correct = std::mem::replace(fingerprint, json!(format!("SHA256:{}", "A".repeat(43))));
        save(&file);
        assert!(matches!(super::saved::launch(config, "ssh"), Err(error) if error.contains("지문이 다릅니다")));
        file["targets"][0]["profile"]["ssh"]["hostKeyFingerprint"] = correct;
        save(&file);

        let fixture = config.join(format!("ssh fixture{}", std::env::consts::EXE_SUFFIX));
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ssh/terminal/fixture.rs");
        let compiled = std::process::Command::new(option_env!("RUSTC").unwrap_or("rustc"))
            .arg(source).arg("-o").arg(&fixture).output().unwrap();
        assert!(compiled.status.success(), "{}", String::from_utf8_lossy(&compiled.stderr));
        let sessions = Sessions::default();
        let open = || {
            let super::Launch::Ssh(mut launch) = super::saved::launch(config, "ssh").unwrap() else { panic!("SSH 계획 필요") };
            let path = launch.fixture(&fixture);
            let mut pty = Pty::open_launch(&sessions, super::Launch::Ssh(launch));
            pty.until_bytes(b"F01-READY", 0, true);
            (pty, path)
        };
        let (first, first_plan) = open();
        let (mut second, second_plan) = open();
        assert_ne!(first.id, second.id);
        assert_ne!(first_plan, second_plan);
        assert!(sessions.live_ssm_identity("main", sessions.document("main"), &first.id).is_none());
        assert!(matches!(tauri::async_runtime::block_on(sessions.connect_sftp("other", &first.id)),
            Err(crate::editor::sftp::Error::Terminal(error)) if error == "다른 창의 세션입니다"));
        let (first_context, first_cancel, first_worker) = sessions.get("main", &first.id).unwrap().sftp_context().unwrap();
        let (same_context, _, same_worker) = sessions.get("main", &first.id).unwrap().sftp_context().unwrap();
        let (second_context, second_cancel, second_worker) = sessions.get("main", &second.id).unwrap().sftp_context().unwrap();
        assert!(std::sync::Arc::ptr_eq(&first_context, &same_context));
        assert!(!std::sync::Arc::ptr_eq(&first_context, &second_context));
        assert_eq!(first_context.invocation(crate::ssh::Client::Sftp).args[4], first_plan.as_os_str());
        assert_eq!(second_context.invocation(crate::ssh::Client::Sftp).args[4], second_plan.as_os_str());
        drop((first_context, same_context, second_context, same_worker, second_worker));
        let cancelled = first_cancel.clone();
        let worker = thread::spawn(move || {
            tauri::async_runtime::block_on(cancelled.cancelled());
            drop(first_worker);
        });
        assert!(sessions.close("other", &first.id).is_err());
        assert!(sessions.close("main", &first.id).unwrap().warning().is_none());
        worker.join().unwrap();
        assert!(first_cancel.is_cancelled());
        assert!(!second_cancel.is_cancelled());
        assert!(!first_plan.exists());
        assert!(second_plan.exists());
        second.input(b"fixture-input\r");
        second.until_bytes(b"F01-INPUT-OK", 0, true);
        assert!(sessions.close("main", &second.id).unwrap().warning().is_none());
        assert!(second_cancel.is_cancelled());
        assert!(!second_plan.exists());
        assert_eq!(std::fs::read_dir(config.join("ssh-trust")).unwrap().count(), 1);
    });
}

fn load(sessions: &Sessions) -> Pty<'_> {
    Pty::open(sessions, Some(&LOAD.replace("@COUNT@", &RECORDS.to_string()).replace("@PAYLOAD@", PAYLOAD)))
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
        let mut b = Pty::open(&sessions, Some(PEER));
        b.input(b"9\r");
        b.until_bytes(b"L03-PEER:81;", 0, false);
        b.ack(b.output.len());
        assert!(matches!(a.events.try_recv(), Err(mpsc::TryRecvError::Empty)), "B가 응답하는 동안 A의 출력 상한이 풀림");

        a.ack(PARTIAL);
        a.fill(WINDOW + PARTIAL);
        a.ack(PARTIAL);
        a.ack(PARTIAL - 1);
        a.assert_stalled();

        let from = a.output.len();
        let end = b"L03-END:111:222;";
        a.until_bytes(end, from, true);
        // 빈 입력은 PTY에 쓰지 않고 자식 종료 상태만 확인한다.
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match sessions.input("main", &a.id, b"") {
                Err(error) => { assert_eq!(error, CLOSED); break; }
                Ok(()) => {
                    assert!(Instant::now() < deadline, "마지막 출력 뒤 자식이 종료되지 않음");
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
        // ConPTY가 마지막 표식 뒤에 보낼 수 있는 VT 바이트까지 받되 ACK는 보류한다.
        loop {
            assert!(Instant::now() < deadline, "자식 종료 후 PTY 출력이 끝나지 않음");
            match a.events.recv_timeout(QUIET) {
                Ok(Event::Bytes(bytes)) => a.output.extend_from_slice(&bytes),
                Err(RecvTimeoutError::Timeout) => break,
                other => panic!("마지막 ACK 전 종료 이벤트 또는 출력 채널 닫힘: {other:?}"),
            }
        }
        a.ack(a.output.len());
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match a.next(deadline) {
                Event::Bytes(_) => a.ack(a.output.len()),
                Event::Exited(7) => break,
                event => panic!("마지막 ACK 뒤 종료 이벤트: {event:?}"),
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

#[test]
fn l03_pty_ctrl_c_during_credit_stall_keeps_shell_reusable() {
    bounded(|| {
        let sessions = Sessions::default();
        let mut pty = Pty::open(&sessions, None);
        pty.input(PROMPT);
        pty.until_bytes(READY, 0, true);
        let start = pty.output.len();
        pty.ack(start);

        pty.input(FLOOD);
        pty.fill(start + WINDOW);
        assert!(pty.output[start..].windows(b"L03-FLOOD:".len()).any(|bytes| bytes == b"L03-FLOOD:"));
        pty.assert_stalled();

        pty.input(b"\x03");
        let interrupted = pty.output.len();
        pty.ack(interrupted);
        pty.until_bytes(READY, interrupted, true);
        let resumed = pty.output.len();
        pty.input(AFTER_INTERRUPT);
        pty.until_bytes(b"L03-AFTER:81;", resumed, true);
        sessions.close("main", &pty.id).unwrap();
    });
}

#[test]
fn c01_saved_pty_selects_exact_id_and_keeps_sessions_after_open_errors() {
    bounded(|| crate::connections::tests::with_saved_pty_targets(|config, removed| {
        #[cfg(windows)]
        const READ_CWD: &[u8] = b"[Console]::WriteLine(('C01-' + 'CWD:' + [IO.File]::ReadAllText('cwd-marker') + ';'))\r";
        #[cfg(unix)]
        const READ_CWD: &[u8] = b"IFS= read -r marker < cwd-marker; printf 'C01-%s:%s;\\n' CWD \"$marker\"\r";

        let sessions = Sessions::default();
        let open = |id| Pty::open_with(&sessions, super::saved::command(config, id).unwrap());
        let mut first = open("first");
        let mut second = open("second");
        let read = |pty: &mut Pty<'_>, expected: &[u8]| {
            let from = pty.output.len();
            pty.input(READ_CWD);
            pty.until_bytes(expected, from, true);
            pty.ack(pty.output.len());
        };
        read(&mut first, b"C01-CWD:first;");
        read(&mut second, b"C01-CWD:second;");
        #[cfg(windows)]
        assert_eq!(super::saved::command(config, "verbatim").err().as_deref(),
            Some("작업 디렉터리: Windows 드라이브의 일반 절대 경로만 지원합니다"));
        sessions.close("main", &second.id).unwrap();

        let delayed = super::saved::command(config, "second").unwrap();
        std::fs::remove_dir_all(removed).unwrap();
        for (id, expected) in [("FIRST", "저장된 연결 대상을 찾을 수 없습니다"), ("second", "작업 디렉터리:")] {
            let result = super::saved::launch(config, id).and_then(|command| {
                sessions.open_document("main", sessions.document("main"), size(24, 120), command, |_| Ok(()))
            });
            let error = result.unwrap_err();
            assert!(error.starts_with(expected), "{id}: {error}");
        }
        // 경로 검사와 PTY 실행 사이에 경로가 사라져도 홈 셸이 열린 채 남지 않아야 한다.
        let mut late = Pty::open_with(&sessions, delayed);
        late.until_bytes(b"Could not start in the saved working directory.", 0, true);
        late.ack(late.output.len());
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match late.next(deadline) {
                Event::Bytes(_) => late.ack(late.output.len()),
                Event::Exited(23) => break,
                event => panic!("작업 디렉터리가 사라진 셸의 종료 상태: {event:?}"),
            }
        }

        read(&mut first, b"C01-CWD:first;");
        sessions.close("main", &first.id).unwrap();
    }));
}

#[test]
fn c02_close_before_start_retires_only_the_unstarted_pty() {
    bounded(|| {
        let sessions = Sessions::default();
        let document = sessions.document("main");
        let open = || {
            let target = super::remote::Target::new("fixture", "us-east-1", "i-0123456789abcdef0").unwrap();
            let (send, events) = mpsc::channel();
            let id = sessions.open_document("main", document, size(24, 80), super::Launch::Ssm(target), move |event| {
                send.send(event).map_err(|error| error.to_string())
            }).unwrap();
            (id, events)
        };
        let (first, first_events) = open();
        let (peer, peer_events) = open();
        assert!(sessions.live_ssm_identity("main", document, &first).is_none());
        // 마법사나 탭이 닫기를 누르기 전에 네이티브 작업자가 먼저 끝날 수 있다.
        let retired = sessions.get("main", &first).unwrap();
        retired.stop(super::process::Stop::Closed);
        retired.wait_finished();
        assert!(sessions.close("other", &first).is_err());
        assert_eq!(sessions.close("main", &first).unwrap(), super::CloseResult::default());
        assert!(matches!(first_events.try_recv(), Err(mpsc::TryRecvError::Disconnected)));
        assert_eq!(sessions.start("main", &first), Err(CLOSED.into()));
        assert!(matches!(peer_events.try_recv(), Err(mpsc::TryRecvError::Empty)));
        sessions.close("main", &peer).unwrap();
        assert!(matches!(peer_events.try_recv(), Err(mpsc::TryRecvError::Disconnected)));
    });
}

#[test]
fn c02_native_start_outcomes() {
    bounded(|| {
        let root = tempfile::tempdir().unwrap();
        let exe = root.path().join(format!("ssm fixture{}", std::env::consts::EXE_SUFFIX));
        let compiled = std::process::Command::new(option_env!("RUSTC").unwrap_or("rustc"))
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/src/terminal/ssm_fixture.rs"))
            .arg("-o").arg(&exe).output().unwrap();
        assert!(compiled.status.success(), "{}", String::from_utf8_lossy(&compiled.stderr));

        for mode in ["not-spawned", "reject", "unknown", "connected"] {
            let dir = root.path().join(mode);
            std::fs::create_dir(&dir).unwrap();
            let program = dir.join(exe.file_name().unwrap());
            if mode != "not-spawned" { std::fs::copy(&exe, &program).unwrap(); }
            std::fs::write(dir.join("mode"), mode).unwrap();
            let sessions = Sessions::default();
            let mut target = super::remote::Target::new("fixture", "us-west-2", "i-01234567").unwrap();
            target.fixture = Some(program);
            let (send, events) = mpsc::channel();
            let id = sessions.open_document("main", sessions.document("main"), size(24, 120), super::Launch::Ssm(target), move |event| {
                send.send(event).map_err(|error| error.to_string())
            }).unwrap();
            let start = sessions.start("main", &id);
            if mode == "not-spawned" {
                assert_eq!(start, Err("SSM PTY: 시작 실패".into()));
            } else {
                start.unwrap();
                let mut pty = Pty { sessions: &sessions, id: id.clone(), events, output: Vec::new() };
                let deadline = Instant::now() + TIMEOUT;
                let mut ready = false;
                #[cfg(windows)]
                let mut cursor = false;
                loop {
                    match pty.next(deadline) {
                        Event::Bytes(_) => {
                            pty.ack(pty.output.len());
                            #[cfg(windows)]
                            if !cursor && pty.output.windows(4).any(|bytes| bytes == b"\x1b[6n") {
                                pty.input(b"\x1b[1;1R");
                                cursor = true;
                            }
                        }
                        Event::Connecting => {}
                        Event::Ready { .. } if mode == "connected" => {
                            assert!(sessions.live_ssm_identity("main", sessions.document("main"), &id).is_none());
                            ready = true;
                            pty.input(b"fixture-input\r");
                        }
                        Event::Exited(0) if mode == "connected" => {
                            assert!(ready);
                            assert!(pty.output.windows(22).any(|bytes| bytes == b"fixture-input-received"));
                            break;
                        }
                        Event::Disconnected(error) => {
                            assert!(!ready);
                            match mode {
                                "reject" => assert_eq!(error, "SSM: 노드 연결 불가", "{:?}", String::from_utf8_lossy(&pty.output)),
                                "unknown" => assert!(error.contains("종료 확인 불가: i-01234567"), "{error}"),
                                _ => panic!("{mode}: {error}"),
                            }
                            break;
                        }
                        event => panic!("{mode}: {event:?}"),
                    }
                }
            }
            let closed = sessions.close("main", &id).unwrap();
            assert!(sessions.live_ssm_identity("main", sessions.document("main"), &id).is_none());
            use super::remote::RemoteClose;
            assert_eq!(closed.remote, match mode {
                "unknown" => RemoteClose::Unconfirmed,
                "connected" => RemoteClose::Confirmed,
                _ => RemoteClose::NotStarted,
            });
            assert_eq!(dir.join("start").is_file(), mode != "not-spawned");
            assert_eq!(dir.join("History").is_file(), matches!(mode, "unknown" | "connected"));
        }
    });
}

#[test]
fn c02_native_pty_creation_failure_stays_local() {
    #[cfg(unix)]
    {
        const CHILD: &str = "ZZABBA_C02_PTY_LIMIT_TEST";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "terminal::pty_flow_tests::c02_native_pty_creation_failure_stays_local", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            assert!(result.status.success(), "{}\n{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
            return;
        }
    }

    let program = std::env::current_exe().unwrap();
    let mut target = super::remote::Target::new("fixture", "us-west-2", "i-01234567").unwrap();
    target.fixture = Some(program);
    let cancel = crate::ssm::CancellationToken::new();
    let mut remote = tauri::async_runtime::block_on(target.prepare(None, &cancel)).unwrap();
    #[cfg(windows)]
    let size = size(0, 0);
    #[cfg(unix)]
    let size = {
        // 리소스 한도는 이 전용 시험 프로세스에만 적용한다.
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) }, 0);
        limit.rlim_cur = 3;
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        size(24, 80)
    };
    let Err(error) = portable_pty::native_pty_system().openpty(size) else { panic!("네이티브 PTY 생성 실패가 필요합니다") };
    println!("PTY 생성 실패: {error}");
    assert_eq!(remote.spawn(size, &cancel).err().as_deref(), Some("SSM PTY: 시작 실패"));
    assert_eq!(remote.close(), (super::remote::RemoteClose::NotStarted, None));
}
