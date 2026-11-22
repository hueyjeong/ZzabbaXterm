use super::*;

const OWNER: &str = "main";
const ACCOUNT: &str = "123456789012";
const REGION: &str = "us-west-2";
const NODE: &str = "i-01234567";

fn transport<'a>(
    sessions: &'a Sessions,
    identity: &'a LiveSsmIdentity,
) -> (ControlTransport<'a>, mpsc::Sender<Event>) {
    let cancel = CancellationToken::new();
    let (tx, output) = mpsc::channel();
    (ControlTransport {
        sessions,
        owner: OWNER,
        identity,
        session: Arc::new(Session::dedicated(cancel.child_token())),
        output,
        cancel,
        open_deadline: Instant::now() + OPEN_TIMEOUT,
        connected: false,
        received: 0,
        #[cfg(windows)]
        cursor_query: 0,
    }, tx)
}

#[test]
fn revalidates_the_exact_central_session_before_accepting_buffered_output() {
    let sessions = Sessions::default();
    let document = sessions.document(OWNER);
    let selected = sessions.insert_live_ssm_fixture(OWNER, document, ACCOUNT, REGION, NODE);
    let other = sessions.insert_live_ssm_fixture(OWNER, document, ACCOUNT, REGION, NODE);
    let identity = selected.identity.clone();
    let (mut transport, tx) = transport(&sessions, &identity);
    tx.send(Event::Connecting).ok().unwrap();
    tx.send(Event::Bytes(b"frame".to_vec())).ok().unwrap();
    assert!(matches!(transport.read(), Ok(Output::Bytes(bytes)) if bytes == b"frame"));
    tx.send(Event::Ready { account_id: ACCOUNT.into() }).ok().unwrap();
    drop(selected);
    assert!(require_live(&sessions, OWNER, &other.identity).is_ok());
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
    assert!(require_live(&sessions, OWNER, &selected.identity).is_ok());
    let cancel = CancellationToken::new();
    assert!(matches!(ControlTransport::open(&sessions, OWNER, &selected.identity, &cancel), Err(error) if error == UNAVAILABLE));
}

#[test]
fn enforces_frame_size_and_requires_an_observed_official_connection() {
    let sessions = Sessions::default();
    let selected = sessions.insert_live_ssm_fixture(OWNER, sessions.document(OWNER), ACCOUNT, REGION, NODE);
    let (mut transport, _tx) = transport(&sessions, &selected.identity);
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
    let (mut transport, tx) = transport(&sessions, &selected.identity);
    tx.send(Event::Bytes(vec![1])).ok().unwrap();
    transport.open_deadline = Instant::now();
    assert_eq!(transport.read().unwrap_err(), TIMED_OUT);
    transport.cancel.cancel();
    assert_eq!(transport.read().unwrap_err(), CANCELLED);
}

#[test]
fn cli_exit_is_distinct_from_ssm_connection_and_frame_output() {
    let sessions = Sessions::default();
    let selected = sessions.insert_live_ssm_fixture(OWNER, sessions.document(OWNER), ACCOUNT, REGION, NODE);
    let (mut transport, tx) = transport(&sessions, &selected.identity);
    tx.send(Event::Exited(23)).ok().unwrap();
    assert!(matches!(transport.read(), Ok(Output::CliExited(23))));
    assert!(!transport.connected);
}
