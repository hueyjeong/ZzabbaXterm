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

#[test]
fn cancellation_before_submission_skips_authorized_start_and_frame_writer() {
    use std::{cell::Cell, io::{self, Write}, path::Path, sync::Mutex};
    use crate::{ssm::fixture_start, terminal::remote::start_authorized};

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
        let (_, _, _, permit) = fixture_start(Path::new("fixture-aws"));
        let _replacement = match end {
            End::Live => None,
            End::Close => { selected.stop(); None }
            End::Navigate => {
                sessions.navigate(OWNER);
                Some(sessions.insert_live_ssm_fixture(OWNER, sessions.document(OWNER), ACCOUNT, REGION, NODE))
            }
            End::CancelRequest => { request.cancel(); None }
        };
        let cancel = transport.binding.cancel_token();
        assert_eq!(cancel.is_cancelled(), matches!(end, End::Close | End::Navigate));

        let start = start_authorized(permit, cancel, Some(&transport.binding), |_| {
            assert!(transport.binding.registry_unlocked_fixture());
            starts.set(starts.get() + 1);
        });
        let expected = error.map_or(Ok(()), |error| Err(error.to_owned()));
        assert_eq!(start, expected);
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
