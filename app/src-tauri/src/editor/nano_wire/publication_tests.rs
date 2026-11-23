use super::*;

const VALUE: Observation = Observation { len: 3, digest: [0xab; 32] };
const SNAPSHOT: Snapshot = Snapshot { token: [0xcd; TOKEN_LEN], observation: VALUE };

#[test]
fn active_open_round_trips_at_path_limit_and_stays_off_local_lane() {
    let mut path = vec![b'a'; MAX_PATH];
    path[0] = b'/';
    let open = ActiveOpen { id: 9, path, snapshot: SNAPSHOT };
    let bytes = open.encode().unwrap();
    assert_eq!(decode_open_len(bytes[..4].try_into().unwrap()).unwrap(), bytes.len() - 4);
    assert_eq!(ActiveOpen::decode(&bytes[4..]).unwrap(), open);
    assert!(super::super::Request::decode(&bytes[4..]).is_err());
    let mut invalid_open = bytes.clone();
    invalid_open[4 + PREFIX + SNAP_LEN] = b'a';
    assert!(ActiveOpen::decode(&invalid_open[4..]).is_err());
    assert!(decode_open_len(u32::MAX.to_be_bytes()).is_err());
    for len in 4..4 + PREFIX + SNAP_LEN + 1 {
        assert!(ActiveOpen::decode(&bytes[4..len]).is_err());
    }
}

#[test]
fn peer_frame_lengths_are_checked_before_the_body() {
    let open = ActiveOpen { id: 9, path: b"/test".to_vec(), snapshot: SNAPSHOT }.encode().unwrap();
    let header = open[..4].try_into().unwrap();
    let prefix = open[4..4 + PREFIX].try_into().unwrap();
    assert!(matches!(decode_peer_prefix(header, prefix), Ok((len, PeerFrame::Open(9))) if len + 4 == open.len()));
    assert!(decode_peer_prefix((MAX_OPEN_PAYLOAD as u32 + 1).to_be_bytes(), prefix).is_err());
    let stock = super::super::Request::Open { id: 9, path: b"/test".to_vec() }.encode().unwrap();
    assert!(decode_peer_prefix(stock[..4].try_into().unwrap(), stock[4..4 + PREFIX].try_into().unwrap()).is_err());
    let outcome = Outcome::Saved(SNAPSHOT).encode(9, 1).unwrap();
    let prefix = outcome[4..4 + PREFIX].try_into().unwrap();
    assert!(matches!(decode_peer_prefix(outcome[..4].try_into().unwrap(), prefix), Ok((_, PeerFrame::Outcome(9)))));
    assert!(decode_peer_prefix((outcome.len() as u32 - 5).to_be_bytes(), prefix).is_err());
}

#[test]
fn operations_round_trip_and_each_truncation_is_rejected() {
    let mut outgoing = OpIds::default();
    let mut incoming = OpIds::default();
    for request in [Operation::Compare, Operation::Reload(VALUE), Operation::Save {
        token: SNAPSHOT.token,
        text: "한글\n".to_owned(),
    }] {
        let op = outgoing.next_id().unwrap();
        let encoded = request.encode(9, op).unwrap();
        let bytes = [encoded.header.as_slice(), encoded.fragment].concat();
        assert!(super::super::Request::decode(&bytes[4..]).is_err());
        for len in 0..bytes.len() {
            assert!(read_operation(&mut &bytes[..len], 9, &mut incoming).is_err());
        }
        assert_eq!(read_operation(&mut bytes.as_slice(), 9, &mut incoming).unwrap(), (op, request));
        assert_eq!(read_operation(&mut bytes.as_slice(), 9, &mut incoming).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
    assert!(OpIds(u64::MAX).next_id().is_err());
    assert!(OpIds(u64::MAX).accept(1).is_err());
}

#[test]
fn invalid_fixed_headers_fail_before_body_reads() {
    let save = Operation::Save { token: SNAPSHOT.token, text: "x" };
    let frame = save.encode(9, 1).unwrap();
    for (index, byte) in [(4, VERSION.wrapping_add(1)), (5, 2), (5, COMPARE), (13, 0), (13, 8), (21, 0), (21, 2)] {
        let mut header = frame.header.clone();
        header[index] = byte;
        let mut reader = header.as_slice();
        assert_eq!(read_operation(&mut reader, 9, &mut OpIds::default()).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(reader, &header[4 + OP_PREFIX..]);
    }
    let oversize = (OP_PREFIX + TOKEN_LEN + MAX_CONTENT + 1) as u32;
    let mut header = frame.header.clone();
    header[..4].copy_from_slice(&oversize.to_be_bytes());
    let mut reader = header.as_slice();
    assert!(read_operation(&mut reader, 9, &mut OpIds::default()).is_err());
    assert_eq!(reader, &header[4..]);
    let mut reader = frame.header.as_slice().chain(&[0xff][..]);
    assert_eq!(read_operation(&mut reader, 9, &mut OpIds::default()).unwrap_err().kind(), io::ErrorKind::InvalidData);
}

#[test]
fn save_borrows_the_fragment_at_the_exact_size_limit() {
    let mut text = "é".repeat(MAX_CONTENT / 2);
    let save = Operation::Save { token: SNAPSHOT.token, text: text.as_str() };
    let encoded = save.encode(9, 1).unwrap();
    assert_eq!(encoded.header.len(), 4 + OP_PREFIX + TOKEN_LEN);
    assert_eq!(encoded.fragment.as_ptr(), text.as_ptr());
    let mut reader = encoded.header.as_slice().chain(encoded.fragment);
    assert_eq!(read_operation(&mut reader, 9, &mut OpIds::default()).unwrap(),
        (1, Operation::Save { token: SNAPSHOT.token, text: text.clone() }));
    text.push('x');
    assert!(Operation::Save { token: SNAPSHOT.token, text }.encode(9, 1).is_err());
}

#[test]
fn responses_require_the_exact_request_operation_and_kind() {
    use OperationKind::{Compare, Reload, Save};
    for (kind, outcome) in [
        (Compare, Outcome::Observation(VALUE)),
        (Reload, Outcome::Reloaded(SNAPSHOT)),
        (Save, Outcome::Saved(SNAPSHOT)),
        (Compare, Outcome::Conflict),
        (Reload, Outcome::Unavailable),
        (Save, Outcome::Unknown(None)),
        (Save, Outcome::Unknown(Some(format!("{RECOVERY_PREFIX}{:032x}{RECOVERY_SUFFIX}", 123)))),
    ] {
        let bytes = outcome.encode(9, 1).unwrap();
        let payload = &bytes[4..];
        assert_eq!(Outcome::decode(payload, 9, 1, kind).unwrap(), outcome);
        assert!(Outcome::decode(payload, 8, 1, kind).is_err());
        assert!(Outcome::decode(payload, 9, 2, kind).is_err());
        if !matches!(outcome, Outcome::Conflict | Outcome::Unavailable) {
            for other in [Compare, Reload, Save].into_iter().filter(|other| *other != kind) {
                assert!(Outcome::decode(payload, 9, 1, other).is_err());
            }
        }
    }
    for name in ["../original", "/original", ".zzabba-edit-0000000000000000000000000000000A.tmp/original"] {
        assert!(Outcome::Unknown(Some(name.to_owned())).encode(9, 1).is_err());
        let bytes = frame(UNKNOWN, &[&9u64.to_be_bytes(), &1u64.to_be_bytes(), name.as_bytes()]);
        assert!(Outcome::decode(&bytes[4..], 9, 1, Save).is_err());
    }
    let invalid_utf8 = [0xff; RECOVERY_LEN];
    let bytes = frame(UNKNOWN, &[&9u64.to_be_bytes(), &1u64.to_be_bytes(), &invalid_utf8]);
    assert!(Outcome::decode(&bytes[4..], 9, 1, Save).is_err());
    assert!(Outcome::Observation(Observation { len: MAX_CONTENT as u64 + 1, ..VALUE }).encode(9, 1).is_err());
    assert!(Outcome::Saved(SNAPSHOT).encode(0, 1).is_err());
    assert!(Outcome::Saved(SNAPSHOT).encode(9, 0).is_err());
}
