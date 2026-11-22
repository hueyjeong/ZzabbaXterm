use std::io;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

const VERSION: u8 = 2;
const MAX_PATH: usize = 4096;
pub(crate) const MAX_PAYLOAD: usize = 2 + 8 + MAX_PATH;
pub(crate) const STARTUP_LIMIT: std::time::Duration = std::time::Duration::from_secs(12);
pub(crate) const REGISTER: [u8; 2] = [VERSION, 8];
pub(crate) const ADMITTED: [u8; 2] = [VERSION, 9];
pub(crate) const ADMIT_NANO: [u8; 2] = [VERSION, 12];
pub(crate) const PROOF_LEN: usize = 16;
pub(crate) const CHALLENGE_LEN: usize = 2 + 4 + 4 + PROOF_LEN;
pub(crate) const CONFIRM_LEN: usize = 2 + PROOF_LEN;
pub(crate) const PTY_PREFIX: &[u8] = b"\x1b]7777;zzabba-nano-2;";
pub(crate) const PTY_FRAME_LEN: usize = PTY_PREFIX.len() + PROOF_LEN * 2 + 1;
pub(crate) const NO_PROOF: [u8; PROOF_LEN] = [0; PROOF_LEN];

pub(crate) struct Lease(Zeroizing<[u8; PROOF_LEN]>);

impl Lease {
    pub(crate) fn issue() -> io::Result<Self> {
        let mut secret = Zeroizing::new([0u8; PROOF_LEN]);
        getrandom::fill(&mut secret[..]).map_err(io::Error::other)?;
        Ok(Self(secret))
    }

    pub(crate) fn proof(&self, pid: i32, sid: i32) -> [u8; PROOF_LEN] {
        let mut digest = Sha256::new();
        digest.update(b"zzabba-nano-2/startup");
        digest.update(&self.0[..]);
        digest.update(pid.to_be_bytes());
        digest.update(sid.to_be_bytes());
        digest.finalize()[..PROOF_LEN].try_into().unwrap()
    }

    pub(crate) fn challenge(&self, pid: i32, sid: i32) -> Zeroizing<[u8; 4 + CHALLENGE_LEN]> {
        let mut bytes = Zeroizing::new([0; 4 + CHALLENGE_LEN]);
        bytes[..4].copy_from_slice(&(CHALLENGE_LEN as u32).to_be_bytes());
        bytes[4..6].copy_from_slice(&[VERSION, 11]);
        bytes[6..10].copy_from_slice(&pid.to_be_bytes());
        bytes[10..14].copy_from_slice(&sid.to_be_bytes());
        bytes[14..].copy_from_slice(&self.0[..]);
        bytes
    }

    pub(crate) fn decode_challenge(payload: &[u8]) -> io::Result<(Self, i32, i32)> {
        let (11, data) = envelope(payload)? else { return Err(invalid()); };
        let (pid, rest) = data.split_at_checked(4).ok_or_else(invalid)?;
        let (sid, secret) = rest.split_at_checked(4).ok_or_else(invalid)?;
        let pid = i32::from_be_bytes(pid.try_into().unwrap());
        let sid = i32::from_be_bytes(sid.try_into().unwrap());
        if pid <= 0 || sid <= 0 { return Err(invalid()); }
        Ok((Self(Zeroizing::new(secret.try_into().map_err(|_| invalid())?)), pid, sid))
    }

    pub(crate) fn confirm(&self) -> Zeroizing<[u8; 4 + CONFIRM_LEN]> {
        let mut bytes = Zeroizing::new([0; 4 + CONFIRM_LEN]);
        bytes[..4].copy_from_slice(&(CONFIRM_LEN as u32).to_be_bytes());
        bytes[4..6].copy_from_slice(&[VERSION, 10]);
        bytes[6..].copy_from_slice(&self.0[..]);
        bytes
    }

    pub(crate) fn confirmed(&self, payload: &[u8]) -> bool {
        matches!(envelope(payload), Ok((10, token)) if token == &self.0[..])
    }
}

pub(crate) fn startup_frame(proof: [u8; PROOF_LEN]) -> [u8; PTY_FRAME_LEN] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0; PTY_FRAME_LEN];
    bytes[..PTY_PREFIX.len()].copy_from_slice(PTY_PREFIX);
    for (digit, byte) in bytes[PTY_PREFIX.len()..PTY_FRAME_LEN - 1].chunks_exact_mut(2).zip(proof) {
        digit[0] = HEX[(byte >> 4) as usize];
        digit[1] = HEX[(byte & 15) as usize];
    }
    bytes[PTY_FRAME_LEN - 1] = b'\x07';
    bytes
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Ready {
    pub(crate) session_id: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Request {
    Open { id: u64, path: Vec<u8> },
    Cancel { id: u64 },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Response {
    Closed { id: u64 },
    Unattached { id: u64 },
    Failed { id: u64 },
    Busy { id: u64 },
}

pub(crate) fn valid_session_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub(crate) fn decode_len(header: [u8; 4]) -> io::Result<usize> {
    let len = u32::from_be_bytes(header) as usize;
    if (2..=MAX_PAYLOAD).contains(&len) {
        Ok(len)
    } else {
        Err(invalid())
    }
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid remote editor frame")
}

fn frame(kind: u8, fields: &[&[u8]]) -> Vec<u8> {
    let len = 2 + fields.iter().map(|field| field.len()).sum::<usize>();
    let mut bytes = Vec::with_capacity(4 + len);
    bytes.extend_from_slice(&(len as u32).to_be_bytes());
    bytes.extend_from_slice(&[VERSION, kind]);
    for field in fields {
        bytes.extend_from_slice(field);
    }
    bytes
}

fn envelope(payload: &[u8]) -> io::Result<(u8, &[u8])> {
    match payload {
        [VERSION, kind, rest @ ..] if payload.len() <= MAX_PAYLOAD => Ok((*kind, rest)),
        _ => Err(invalid()),
    }
}

fn with_id(payload: &[u8]) -> io::Result<(u8, u64, &[u8])> {
    let (kind, data) = envelope(payload)?;
    let (bytes, rest) = data.split_at_checked(8).ok_or_else(invalid)?;
    let id = u64::from_be_bytes(bytes.try_into().map_err(|_| invalid())?);
    if id == 0 {
        return Err(invalid());
    }
    Ok((kind, id, rest))
}

fn valid_path(path: &[u8]) -> bool {
    path.len() <= MAX_PATH && path.first() == Some(&b'/') && !path.contains(&0)
}

impl Ready {
    pub(crate) fn encode(&self) -> io::Result<Vec<u8>> {
        if !valid_session_id(&self.session_id) {
            return Err(invalid());
        }
        Ok(frame(1, &[self.session_id.as_bytes()]))
    }

    pub(crate) fn decode(payload: &[u8]) -> io::Result<Self> {
        let (kind, data) = envelope(payload)?;
        let id = std::str::from_utf8(data).map_err(|_| invalid())?;
        if kind != 1 || !valid_session_id(id) {
            return Err(invalid());
        }
        Ok(Self { session_id: id.to_owned() })
    }
}

impl Request {
    pub(crate) fn encode(&self) -> io::Result<Vec<u8>> {
        match self {
            Self::Open { id, path } if *id != 0 && valid_path(path) => {
                Ok(frame(2, &[&id.to_be_bytes(), path]))
            }
            Self::Cancel { id } if *id != 0 => Ok(frame(3, &[&id.to_be_bytes()])),
            _ => Err(invalid()),
        }
    }

    pub(crate) fn decode(payload: &[u8]) -> io::Result<Self> {
        match with_id(payload)? {
            (2, id, path) if valid_path(path) => Ok(Self::Open { id, path: path.to_vec() }),
            (3, id, []) => Ok(Self::Cancel { id }),
            _ => Err(invalid()),
        }
    }
}

impl Response {
    pub(crate) fn id(&self) -> u64 {
        match *self {
            Self::Closed { id } | Self::Unattached { id } | Self::Failed { id } | Self::Busy { id } => id,
        }
    }

    pub(crate) fn encode(&self) -> io::Result<Vec<u8>> {
        let id = self.id();
        if id == 0 {
            return Err(invalid());
        }
        let kind = match self {
            Self::Closed { .. } => 4,
            Self::Unattached { .. } => 5,
            Self::Failed { .. } => 6,
            Self::Busy { .. } => 7,
        };
        Ok(frame(kind, &[&id.to_be_bytes()]))
    }

    pub(crate) fn decode(payload: &[u8]) -> io::Result<Self> {
        match with_id(payload)? {
            (4, id, []) => Ok(Self::Closed { id }),
            (5, id, []) => Ok(Self::Unattached { id }),
            (6, id, []) => Ok(Self::Failed { id }),
            (7, id, []) => Ok(Self::Busy { id }),
            _ => Err(invalid()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_preserves_non_utf8_and_control_bytes() {
        let request = Request::Open { id: 1, path: b"/\xff\n".to_vec() };
        let bytes = request.encode().unwrap();
        assert_eq!(bytes, b"\0\0\0\r\x02\x02\0\0\0\0\0\0\0\x01/\xff\n");
        assert_eq!(Request::decode(&bytes[4..]).unwrap(), request);
    }

    #[test]
    fn cancel_contains_only_the_exact_request_id() {
        let request = Request::Cancel { id: 0x0102_0304_0506_0708 };
        let mut bytes = request.encode().unwrap();
        assert_eq!(bytes, b"\0\0\0\n\x02\x03\x01\x02\x03\x04\x05\x06\x07\x08");
        assert_eq!(Request::decode(&bytes[4..]).unwrap(), request);
        bytes.push(0);
        assert!(Request::decode(&bytes[4..]).is_err());
    }

    #[test]
    fn rejects_invalid_lengths_versions_ids_and_path() {
        assert!(decode_len(1u32.to_be_bytes()).is_err());
        assert!(decode_len((MAX_PAYLOAD as u32 + 1).to_be_bytes()).is_err());
        assert!(!valid_session_id("0123456789abcdef0123456789abcdeF"));
        let ready = Ready { session_id: "0123456789abcdef0123456789abcdef".into() };
        let mut bytes = ready.encode().unwrap();
        assert_eq!(Ready::decode(&bytes[4..]).unwrap(), ready);
        assert!(Ready::decode(&bytes[4..bytes.len() - 1]).is_err());
        bytes[4] = VERSION + 1;
        assert!(Ready::decode(&bytes[4..]).is_err());
        bytes[4] = 1;
        assert!(Ready::decode(&bytes[4..]).is_err());
        assert!(Response::Closed { id: 0 }.encode().is_err());
        assert!(Request::Open { id: 3, path: b"/x\0y".to_vec() }.encode().is_err());
    }

    #[test]
    fn startup_proof_is_fixed_width_and_distinct_from_the_control_secret() {
        let lease = Lease(Zeroizing::new([0xaa; PROOF_LEN]));
        let challenge = lease.challenge(271, 432);
        let (client, pid, sid) = Lease::decode_challenge(&challenge[4..]).unwrap();
        assert_eq!((pid, sid), (271, 432));
        assert!(Lease::decode_challenge(&challenge[4..challenge.len() - 1]).is_err());
        assert!(Lease::decode_challenge(&lease.challenge(0, 1)[4..]).is_err());
        assert!(lease.confirmed(&client.confirm()[4..]));
        assert_ne!(lease.proof(pid, sid), client.proof(pid + 1, sid));
        assert_ne!(lease.proof(pid, sid), client.proof(pid, sid + 1));
        let response = startup_frame(client.proof(pid, sid));
        let proof = &response[PTY_PREFIX.len()..PTY_FRAME_LEN - 1];
        assert!(response.starts_with(PTY_PREFIX));
        assert_eq!(proof.len(), 2 * PROOF_LEN);
        assert!(proof.iter().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')));
        assert_ne!(proof, &[b'a'; 2 * PROOF_LEN]);
        assert!(!Lease(Zeroizing::new([0; PROOF_LEN])).confirmed(&lease.confirm()[4..]));
    }

    #[test]
    fn responses_have_distinct_outcomes_and_preserve_id() {
        for response in [Response::Closed { id: 9 }, Response::Unattached { id: 9 },
            Response::Failed { id: 9 }, Response::Busy { id: 9 }] {
            let mut bytes = response.encode().unwrap();
            assert_eq!(Response::decode(&bytes[4..]).unwrap(), response);
            bytes.push(0);
            assert!(Response::decode(&bytes[4..]).is_err());
        }
    }
}
