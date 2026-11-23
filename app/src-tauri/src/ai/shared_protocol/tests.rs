use super::*;

const NONCE: &str = "0123456789abcdef0123456789abcdef";
const CHANNEL: &str = "abcdef0123456789abcdef0123456789";
const PAIR_CODE: &str = "0123456789abcdef0123456789abcdefabcdef0123456789abcdef0123456789";
const OP: WireOperation = WireOperation::Uptime;

fn nz(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

fn ticket(id: u64) -> Ticket {
    Ticket {
        main_id: 1, main_generation: nz(2), document: 3, control_id: nz(4),
        control_generation: nz(5), shell_generation: nz(6), approval_id: nz(id),
        input_generation: 7, policy_revision: nz(8),
    }
}

fn receive(protocol: &mut SharedProtocol, frame: impl serde::Serialize) -> Result<Option<Event>, ProtocolError> {
    let bytes = encode(frame).unwrap();
    let mut input = bytes.as_slice();
    let event = protocol.feed(&mut input)?;
    assert!(input.is_empty());
    Ok(event)
}

fn initializing() -> SharedProtocol {
    let mut protocol = SharedProtocol::default();
    assert!(protocol.ssm_connected().unwrap().is_none());
    assert!(matches!(receive(&mut protocol, NodeFrame::Ready { version: VERSION, nonce: NONCE }), Ok(Some(Event::Ready))));
    assert!(protocol.hello(NONCE).is_err());
    assert!(protocol.reserve(ticket(9), OP).is_err());
    let hello = protocol.hello(CHANNEL).unwrap();
    assert_eq!(hello.last(), Some(&b'\n'));
    assert!(matches!(serde_json::from_slice(&hello).unwrap(), ClientFrame::Hello {
        version: VERSION, nonce: NONCE, channel: CHANNEL,
    }));
    assert!(protocol.hello(CHANNEL).is_err());
    assert!(matches!(receive(&mut protocol, NodeFrame::HelloAck {
        version: VERSION, channel: CHANNEL,
    }), Ok(Some(Event::HelloAck))));
    protocol
}

fn paired() -> SharedProtocol {
    let mut protocol = initializing();
    let pairing = Node::Pairing { channel: CHANNEL, seq: nz(1), code: PAIR_CODE, expires_in_ms: MAX_PAIR_MS };
    let bytes = encode(pairing).unwrap();
    let next = encode(Node::Paired { channel: CHANNEL, seq: nz(2), uid: 1000, shell_generation: nz(6) }).unwrap();
    let mut joined = bytes[10..].to_vec();
    joined.extend_from_slice(&next);
    assert!(protocol.feed(&mut &bytes[..10]).unwrap().is_none());
    let mut input = joined.as_slice();
    assert!(matches!(protocol.feed(&mut input), Ok(Some(Event::Pairing { code, expires_in }))
        if code.as_str() == PAIR_CODE && expires_in == Duration::from_secs(60)));
    assert_eq!(input, next.as_slice());
    assert!(matches!(protocol.feed(&mut input), Ok(Some(Event::Paired { uid: 1000, shell_generation }))
        if shell_generation == nz(6)));
    assert!(input.is_empty());
    protocol
}

fn claim(protocol: &mut SharedProtocol, ticket: Ticket, seq: u64) -> Result<Option<Event>, ProtocolError> {
    receive(protocol, Node::Claim { channel: CHANNEL, seq: nz(seq), ticket })
}

fn granted() -> SharedProtocol {
    let mut protocol = paired();
    let ticket = ticket(9);
    let reserve = protocol.reserve(ticket, OP).unwrap();
    assert!(matches!(serde_json::from_slice(&reserve).unwrap(), Client::Reserve {
        channel: CHANNEL, seq, ticket: sent, operation: OP,
    } if seq == nz(1) && sent == ticket));
    assert!(protocol.reserve(ticket, OP).is_err());
    assert!(protocol.grant(ticket, OP).is_err());
    assert!(matches!(claim(&mut protocol, ticket, 3), Ok(Some(Event::Claim { ticket: got })) if got == ticket));
    assert!(protocol.stop(ticket.approval_id).is_err());
    assert!(protocol.grant(Ticket { input_generation: 8, ..ticket }, OP).is_err());
    let grant = protocol.grant(ticket, OP).unwrap();
    assert!(matches!(serde_json::from_slice(&grant).unwrap(), Client::Grant {
        channel: CHANNEL, seq, ticket: sent, operation: OP,
    } if seq == nz(2) && sent == ticket));
    assert!(protocol.grant(ticket, OP).is_err());
    assert!(protocol.deny(ticket).is_err());
    protocol
}

fn started(protocol: &mut SharedProtocol, job_id: u64) -> Result<Option<Event>, ProtocolError> {
    receive(protocol, NodeFrame::Started { channel: CHANNEL, job_id: nz(job_id), seq: nz(4) })
}

#[test]
fn official_connection_and_fragmented_startup_are_both_required() {
    let mut protocol = SharedProtocol::default();
    let mut raw = HISTORY_WARNING.to_vec();
    raw.extend_from_slice(b"\r\n\r\nStarting session with SessionId: role+name-abc123\r\n");
    raw.extend_from_slice(&encode(NodeFrame::Ready { version: VERSION, nonce: NONCE }).unwrap());
    for byte in &raw {
        assert!(protocol.feed(&mut std::slice::from_ref(byte)).unwrap().is_none());
    }
    assert!(protocol.hello(CHANNEL).is_err());
    assert!(matches!(protocol.ssm_connected(), Ok(Some(Event::Ready))));
    assert!(protocol.ssm_connected().is_err());
    assert!(protocol.hello(CHANNEL).is_ok());
}

#[cfg(windows)]
#[test]
fn only_known_conpty_start_sequences_are_skipped() {
    let mut protocol = SharedProtocol::default();
    assert!(protocol.feed(&mut &b"\x1b[6n\x1b[?9001h\x1b[?1004h\x1b[m\x1b]0;fixture\x07\x1b[?25h\r\n"[..]).unwrap().is_none());
    let mut raw = b"\x1b[?25h".to_vec();
    raw.extend_from_slice(&encode(NodeFrame::Ready { version: VERSION, nonce: NONCE }).unwrap());
    assert!(protocol.feed(&mut raw.as_slice()).unwrap().is_none());
    assert!(matches!(protocol.ssm_connected(), Ok(Some(Event::Ready))));
    assert!(matches!(SharedProtocol::default().feed(&mut &b"\x1b[2J\n"[..]), Err(ProtocolError::StartupOutput)));
}

#[test]
fn startup_diagnostics_and_cli_exit_are_never_job_exit_evidence() {
    for output in [
        b"An error occurred (TargetNotConnected) when calling the StartSession operation: offline\n".as_slice(),
        b"Cannot perform start session: plugin failed\n",
        b"zzabba-ai-control: node setup failed\n",
        b"Starting session with SessionId: missing characters\n",
        b"Starting session with SessionId: abc\nStarting session with SessionId: abc\n",
    ] {
        let mut protocol = SharedProtocol::default();
        protocol.ssm_connected().unwrap();
        let mut input = output;
        assert!(matches!(protocol.feed(&mut input), Err(ProtocolError::StartupOutput)));
        assert!(matches!(receive(&mut protocol, NodeFrame::Ready { version: VERSION, nonce: NONCE }), Err(ProtocolError::UnexpectedFrame)));
    }
    let mut protocol = granted();
    assert!(matches!(started(&mut protocol, 9), Ok(Some(Event::Started { .. }))));
    assert_eq!(protocol.cli_exited(0), ProtocolError::CliExited(0));
    assert!(matches!(receive(&mut protocol, NodeFrame::Exited {
        channel: CHANNEL, seq: nz(5), job_id: nz(9), code: Some(0), signal: None,
    }), Err(ProtocolError::UnexpectedFrame)));
    assert!(protocol.stop(nz(9)).is_err());
}

#[test]
fn fragmented_frame_buffer_includes_the_newline_in_its_limit() {
    let mut protocol = SharedProtocol::default();
    let raw = [b'x'; MAX_FRAME_BYTES];
    assert!(protocol.feed(&mut &raw[..MAX_FRAME_BYTES - 1]).unwrap().is_none());
    assert!(matches!(protocol.feed(&mut &raw[MAX_FRAME_BYTES - 1..]), Err(ProtocolError::FrameTooLarge)));
    let mut protocol = SharedProtocol::default();
    let mut raw = encode(NodeFrame::Ready { version: VERSION, nonce: NONCE }).unwrap();
    raw.pop();
    raw.resize(MAX_FRAME_BYTES - 1, b' ');
    assert!(protocol.feed(&mut raw.as_slice()).unwrap().is_none());
    assert!(protocol.feed(&mut &b"\n"[..]).unwrap().is_none());
    assert!(matches!(protocol.ssm_connected(), Ok(Some(Event::Ready))));
}

#[test]
fn preamble_and_partial_ready_share_the_startup_budget() {
    let mut protocol = SharedProtocol::default();
    let preamble = b"\nStarting session with SessionId: abc\n";
    assert!(protocol.feed(&mut preamble.as_slice()).unwrap().is_none());
    let remainder = vec![b'x'; MAX_FRAME_BYTES - preamble.len()];
    assert!(matches!(protocol.feed(&mut remainder.as_slice()), Err(ProtocolError::FrameTooLarge)));
}

#[test]
fn wrong_handshake_generation_and_unknown_fields_fail_closed() {
    let mut protocol = initializing();
    assert!(matches!(receive(&mut protocol, Node::Paired {
        channel: CHANNEL, seq: nz(1), uid: 1000, shell_generation: nz(6),
    }), Err(ProtocolError::UnexpectedFrame)));
    let mut protocol = initializing();
    let mut raw = encode(Node::Pairing { channel: CHANNEL, seq: nz(1), code: PAIR_CODE, expires_in_ms: MAX_PAIR_MS }).unwrap();
    raw.truncate(raw.len() - 2);
    raw.extend_from_slice(b",\"extra\":0}\n");
    assert!(matches!(protocol.feed(&mut raw.as_slice()), Err(ProtocolError::InvalidFrame)));
    let mut protocol = paired();
    assert!(protocol.reserve(Ticket { shell_generation: nz(7), ..ticket(9) }, OP).is_err());
}

#[test]
fn deny_consumes_the_ticket_and_a_stale_claim_cannot_reactivate_it() {
    for claimed in [false, true] {
        let mut protocol = paired();
        let old = ticket(9);
        protocol.reserve(old, OP).unwrap();
        if claimed {
            assert!(claim(&mut protocol, old, 3).unwrap().is_some());
        }
        let deny = protocol.deny(old).unwrap();
        assert!(matches!(serde_json::from_slice(&deny).unwrap(), Client::Deny {
            channel: CHANNEL, seq, ticket: sent,
        } if seq == nz(2) && sent == old));
        assert!(protocol.deny(old).is_err());
        assert!(protocol.grant(old, OP).is_err());
        assert!(protocol.reserve(old, OP).is_err());
        protocol.reserve(ticket(10), OP).unwrap();
        assert!(matches!(claim(&mut protocol, old, if claimed { 4 } else { 3 }), Err(ProtocolError::UnexpectedFrame)));
    }
}

#[test]
fn node_reservation_expiry_is_a_distinct_denial() {
    let mut protocol = paired();
    assert!(matches!(receive(&mut protocol, NodeFrame::Denied { channel: CHANNEL, seq: nz(3) }), Err(ProtocolError::UnexpectedFrame)));
    let mut protocol = paired();
    let ticket = ticket(9);
    protocol.reserve(ticket, OP).unwrap();
    assert!(matches!(receive(&mut protocol, NodeFrame::Denied { channel: CHANNEL, seq: nz(3) }), Ok(Some(Event::Denied))));
    assert!(protocol.grant(ticket, OP).is_err());
    assert!(protocol.stop(nz(9)).is_err());
}

#[test]
fn duplicate_claim_closes_the_channel_before_grant() {
    let mut protocol = paired();
    let ticket = ticket(9);
    protocol.reserve(ticket, OP).unwrap();
    assert!(claim(&mut protocol, ticket, 3).unwrap().is_some());
    assert!(matches!(claim(&mut protocol, ticket, 4), Err(ProtocolError::UnexpectedFrame)));
    assert!(protocol.grant(ticket, OP).is_err());
}

#[test]
fn grant_start_output_stop_and_exit_preserve_job_and_independent_sequences() {
    let mut protocol = granted();
    assert!(matches!(started(&mut protocol, 9), Ok(Some(Event::Started { job_id })) if job_id == nz(9)));
    assert!(protocol.stop(nz(10)).is_err());
    let stop = protocol.stop(nz(9)).unwrap();
    assert!(matches!(serde_json::from_slice(&stop).unwrap(), Client::Stop {
        channel: CHANNEL, seq, job_id,
    } if seq == nz(3) && job_id == nz(9)));
    assert!(protocol.stop(nz(9)).is_err());
    assert!(matches!(receive(&mut protocol, NodeFrame::Output {
        channel: CHANNEL, seq: nz(5), job_id: nz(9), stream: WireStream::Stderr, data: "AP8K",
    }), Ok(Some(Event::Output { job_id, stream: WireStream::Stderr, bytes }))
        if job_id == nz(9) && bytes == [0, 255, b'\n']));
    assert!(matches!(receive(&mut protocol, NodeFrame::Exited {
        channel: CHANNEL, seq: nz(6), job_id: nz(9), code: None, signal: Some(15),
    }), Ok(Some(Event::Exited { job_id, code: None, signal: Some(15) })) if job_id == nz(9)));
    assert!(protocol.stop(nz(9)).is_err());
    assert!(protocol.reserve(ticket(9), OP).is_err());
    assert!(protocol.reserve(ticket(10), OP).is_ok());
}

#[test]
fn invalid_job_events_are_rejected_without_recovering_the_channel() {
    let oversized = STANDARD.encode([0; MAX_OUTPUT_BYTES + 1]);
    for (channel, id, seq, data, expected) in [
        (NONCE, 9, 5, "YQ==", ProtocolError::UnexpectedFrame),
        (CHANNEL, 8, 5, "YQ==", ProtocolError::UnexpectedFrame),
        (CHANNEL, 9, 4, "YQ==", ProtocolError::UnexpectedFrame),
        (CHANNEL, 9, 6, "YQ==", ProtocolError::UnexpectedFrame),
        (CHANNEL, 9, 5, "YQ", ProtocolError::InvalidFrame),
        (CHANNEL, 9, 5, "", ProtocolError::InvalidFrame),
        (CHANNEL, 9, 5, oversized.as_str(), ProtocolError::InvalidFrame),
    ] {
        let mut protocol = granted();
        started(&mut protocol, 9).unwrap();
        assert!(matches!(receive(&mut protocol, NodeFrame::Output {
            channel, seq: nz(seq), job_id: nz(id), stream: WireStream::Stdout, data,
        }), Err(error) if error == expected));
        assert!(protocol.stop(nz(9)).is_err());
    }
    let mut protocol = granted();
    assert!(matches!(receive(&mut protocol, NodeFrame::Exited {
        channel: CHANNEL, seq: nz(4), job_id: nz(9), code: Some(0), signal: None,
    }), Err(ProtocolError::UnexpectedFrame)));
    let mut protocol = granted();
    started(&mut protocol, 9).unwrap();
    assert!(matches!(receive(&mut protocol, NodeFrame::Exited {
        channel: CHANNEL, seq: nz(5), job_id: nz(9), code: Some(0), signal: Some(15),
    }), Err(ProtocolError::InvalidFrame)));
}
