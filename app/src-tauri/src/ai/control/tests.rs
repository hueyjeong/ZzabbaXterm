use super::*;

const OWNER: &str = "main";
const ACCOUNT: &str = "123456789012";
const REGION: &str = "us-west-2";
const NODE: &str = "i-01234567";

fn transport(sessions: &Sessions, identity: &LiveSsmIdentity) -> (ControlTransport, mpsc::Sender<Event>, CancellationToken) {
    let request = CancellationToken::new();
    let binding = sessions.bind_ssm_control(OWNER, identity, &request).unwrap();
    let (tx, output) = mpsc::channel();
    (ControlTransport {
        job_id: None,
        session: Arc::new(Session::dedicated(&binding)),
        binding,
        output,
        open_deadline: Instant::now() + OPEN_TIMEOUT,
        connected: false,
        received: 0,
        #[cfg(windows)]
        cursor_query: 0,
    }, tx, request)
}

#[test]
fn revalidates_the_exact_central_session_before_accepting_buffered_output() {
    let sessions = Sessions::default();
    let document = sessions.document(OWNER);
    let selected = sessions.insert_live_ssm_fixture(OWNER, document, ACCOUNT, REGION, NODE);
    let other = sessions.insert_live_ssm_fixture(OWNER, document, ACCOUNT, REGION, NODE);
    let identity = selected.identity.clone();
    let (mut transport, tx, _) = transport(&sessions, &identity);
    tx.send(Event::Connecting).ok().unwrap();
    tx.send(Event::Bytes(b"frame".to_vec())).ok().unwrap();
    assert!(matches!(transport.read(), Ok(Output::Bytes(bytes)) if bytes == b"frame"));
    tx.send(Event::Ready { account_id: ACCOUNT.into() }).ok().unwrap();
    drop(selected);
    assert!(sessions.with_live_ssm_session(OWNER, &other.identity, || ()).is_some());
    assert_eq!(transport.read().unwrap_err(), UNAVAILABLE);
}

#[test]
fn rejects_stale_or_foreign_central_identity_without_starting_cli() {
    let sessions = Sessions::default();
    let document = sessions.document(OWNER);
    let selected = sessions.insert_live_ssm_fixture(OWNER, document, ACCOUNT, REGION, NODE);
    let cancel = CancellationToken::new();
    let mut identity = selected.identity.clone();
    for (owner, account, region, node) in [
        ("other", ACCOUNT, REGION, NODE),
        (OWNER, "999999999999", REGION, NODE),
        (OWNER, ACCOUNT, "us-east-1", NODE),
        (OWNER, ACCOUNT, REGION, "i-11111111"),
    ] {
        identity.account_id = account.into();
        identity.region = region.into();
        identity.instance_id = node.into();
        assert!(matches!(ControlTransport::open(&sessions, owner, &identity, &cancel), Err(error) if error == UNAVAILABLE));
    }
    let mut identity = selected.identity.clone();
    identity.profile.push_str("-other");
    assert!(matches!(ControlTransport::open(&sessions, OWNER, &identity, &cancel), Err(error) if error == UNAVAILABLE));
}

#[test]
fn rejects_live_central_session_without_verified_role_before_starting_cli() {
    let sessions = Sessions::default();
    let document = sessions.document(OWNER);
    let selected = sessions.insert_live_ssm_fixture(OWNER, document, ACCOUNT, REGION, NODE).without_role();
    assert!(sessions.with_live_ssm_session(OWNER, &selected.identity, || ()).is_some());
    let cancel = CancellationToken::new();
    assert!(matches!(ControlTransport::open(&sessions, OWNER, &selected.identity, &cancel), Err(error) if error == UNAVAILABLE));
}

#[test]
fn enforces_frame_size_and_requires_an_observed_official_connection() {
    let sessions = Sessions::default();
    let selected = sessions.insert_live_ssm_fixture(OWNER, sessions.document(OWNER), ACCOUNT, REGION, NODE);
    let (mut transport, _tx, _) = transport(&sessions, &selected.identity);
    let oversized = [0; MAX_INPUT + 1];
    for invalid in [&[][..], &oversized[..]] {
        assert_eq!(transport.write(invalid).unwrap_err(), "제어 세션 입력 크기가 범위를 벗어났습니다");
    }
    assert_eq!(transport.write(b"frame").unwrap_err(), "공식 SSM 연결을 확인할 수 없습니다");
}

#[test]
fn cancellation_and_start_deadline_win_over_buffered_frames() {
    let sessions = Sessions::default();
    let selected = sessions.insert_live_ssm_fixture(OWNER, sessions.document(OWNER), ACCOUNT, REGION, NODE);
    let (mut transport, tx, request) = transport(&sessions, &selected.identity);
    tx.send(Event::Bytes(vec![1])).ok().unwrap();
    transport.open_deadline = Instant::now();
    assert_eq!(transport.read().unwrap_err(), TIMED_OUT);
    request.cancel();
    assert_eq!(transport.read().unwrap_err(), CANCELLED);
}

#[test]
fn cli_exit_is_distinct_from_ssm_connection_and_frame_output() {
    let sessions = Sessions::default();
    let selected = sessions.insert_live_ssm_fixture(OWNER, sessions.document(OWNER), ACCOUNT, REGION, NODE);
    let (mut transport, tx, _) = transport(&sessions, &selected.identity);
    tx.send(Event::Exited(23)).ok().unwrap();
    assert!(matches!(transport.read(), Ok(Output::CliExited(23))));
    assert!(!transport.connected);
}

#[cfg(any(windows, unix))]
#[test]
fn cancellation_after_start_preparation_skips_spawn_and_frame_writer() {
    use std::{cell::Cell, io::{self, Write}, sync::Mutex};
    use crate::ssm::fixture_start;

    struct Writer {
        binding: Arc<ControlBinding>,
        writes: Arc<Mutex<usize>>,
    }
    impl Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            assert!(self.binding.registry_unlocked_fixture());
            *self.writes.lock().unwrap() += 1;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("fixture-aws");

    enum End { Live, Close, Navigate, CancelRequest }
    for (end, error) in [
        (End::Live, None),
        (End::Close, Some(UNAVAILABLE)),
        (End::Navigate, Some(UNAVAILABLE)),
        (End::CancelRequest, Some(CANCELLED)),
    ] {
        let sessions = Sessions::default();
        let document = sessions.document(OWNER);
        let selected = sessions.insert_live_ssm_fixture(OWNER, document, ACCOUNT, REGION, NODE);
        let (mut transport, _tx, request) = transport(&sessions, &selected.identity);
        let writes = Arc::new(Mutex::new(0));
        transport.session.attach_control_writer_fixture(Writer {
            binding: Arc::clone(&transport.binding), writes: Arc::clone(&writes),
        });
        transport.connected = true;
        let starts = Cell::new(0);
        assert!(transport.binding.admit(|| Ok(())).is_ok());
        let (_, _, _, permit) = fixture_start(&missing);
        let mut _replacement = None;
        let cancel = transport.binding.cancel_token();
        let start = permit.spawn_fixture(PtySize::default(), cancel, Some(&transport.binding), || {
            assert!(transport.binding.registry_unlocked_fixture());
            match end {
                End::Live => {},
                End::Close => selected.stop(),
                End::Navigate => {
                    sessions.navigate(OWNER);
                    _replacement = Some(sessions.insert_live_ssm_fixture(OWNER, sessions.document(OWNER), ACCOUNT, REGION, NODE));
                }
                End::CancelRequest => request.cancel(),
            }
            assert_eq!(cancel.is_cancelled(), matches!(end, End::Close | End::Navigate));
        }, || {
            assert!(transport.binding.registry_unlocked_fixture());
            starts.set(starts.get() + 1);
        });
        let expected = error.map_or(Ok(()), |error| Err(error.to_owned()));
        if error.is_none() {
            // 생성 접점은 열리고, 존재하지 않는 시험 실행 파일은 로컬 생성 오류를 돌려준다.
            assert_ne!(start.err().unwrap().kind(), io::ErrorKind::Interrupted);
        } else {
            assert_eq!(start.map(|_| ()).map_err(|error| error.to_string()), expected);
        }
        assert_eq!(transport.write(b"frame"), expected);
        assert_eq!(starts.get(), usize::from(error.is_none()));
        assert_eq!(*writes.lock().unwrap(), usize::from(error.is_none()));
    }
}

#[test]
fn request_cancel_reaches_the_control_child_without_cancelling_the_central_session() {
    let sessions = Sessions::default();
    let selected = sessions.insert_live_ssm_fixture(OWNER, sessions.document(OWNER), ACCOUNT, REGION, NODE);
    let (transport, _, request) = transport(&sessions, &selected.identity);
    transport.binding.forward_request_cancel();
    request.cancel();
    tauri::async_runtime::block_on(async {
        tokio::time::timeout(Duration::from_secs(5), transport.binding.cancel_token().cancelled()).await.unwrap();
    });
    assert!(sessions.with_live_ssm_session(OWNER, &selected.identity, || ()).is_some());
}

#[test]
fn shutdown_completes_control_cancel_before_start_and_rejects_late_launch() {
    let (done, finished) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        control_shutdown_before_start_case();
        let _ = done.send(());
    });
    match finished.recv_timeout(Duration::from_secs(10)) {
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => worker.join().unwrap(),
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("전용 제어 작업 완료 대기 시간 초과"),
    }
}

fn control_shutdown_before_start_case() {
    {
        let closing = Sessions::default();
        let peer = closing.insert_live_ssm_fixture(OWNER, closing.document(OWNER), ACCOUNT, REGION, NODE);
        let late = closing.bind_ssm_control(OWNER, &peer.identity, &CancellationToken::new()).unwrap();
        assert!(closing.begin_control_exit(OWNER, peer.identity.document, "exit"));
        assert_eq!(closing.drain_controls_for_close(OWNER, peer.identity.document, "exit", Duration::ZERO), Some(crate::terminal::ControlExitReport::default()));
        assert!(!late.cancel_token().is_cancelled());
        assert_eq!(late.admit(|| unreachable!()), Err::<(), _>(UNAVAILABLE.into()));
        assert!(matches!(closing.launch_control(&late, PtySize::default(), Target::new("fixture", REGION, NODE).unwrap(), |_| unreachable!()), Err(error) if error == UNAVAILABLE));
    }

    let sessions = Sessions::default();
    let central = sessions.insert_live_ssm_fixture(OWNER, sessions.document(OWNER), ACCOUNT, REGION, NODE);
    let document = central.identity.document;
    let request = CancellationToken::new();
    let binding = sessions.bind_ssm_control(OWNER, &central.identity, &request).unwrap();
    let (events, output) = mpsc::channel();
    let (_, control, opened) = sessions.launch_control(
        &binding,
        PtySize::default(),
        Target::new("fixture", REGION, NODE).unwrap(),
        move |event| events.send(event).map_err(|error| error.to_string()),
    ).unwrap();
    opened.unwrap();
    // 중앙 픽스처에는 완료를 통지할 작업자가 없다. 중앙 탭이 닫힌 상태를 만든다.
    drop(central);

    assert!(sessions.begin_control_exit(OWNER, document, "exit"));
    assert_eq!(sessions.drain_controls_for_close(OWNER, document, "exit", Duration::from_secs(5)), Some(crate::terminal::ControlExitReport::default()));
    assert!(matches!(output.try_recv(), Err(mpsc::TryRecvError::Disconnected)));
    assert!(binding.cancel_token().is_cancelled());
    assert!(control.start_until(None).is_err());
    assert_eq!(control.close_result(), CloseResult::default());
    let target = Target::new("fixture", REGION, NODE).unwrap();
    assert!(matches!(sessions.launch_control(&binding, PtySize::default(), target, |_| unreachable!()), Err(error) if error == UNAVAILABLE));
}
