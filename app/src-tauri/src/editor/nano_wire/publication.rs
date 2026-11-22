#[cfg(test)]
use super::VERSION;
use super::{MAX_PATH, frame, invalid, valid_path, with_id};
use std::io::{self, Read};

pub(crate) const MAX_CONTENT: usize = 8 * 1024 * 1024;
pub(crate) const PUBLICATION_IO_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

// Payload prefix: version, kind, request ID; operations add an operation ID (all BE).
const PREFIX: usize = 2 + 8;
const OP_PREFIX: usize = PREFIX + 8;
const OBS_LEN: usize = 8 + 32;
const TOKEN_LEN: usize = 16;
const SNAP_LEN: usize = TOKEN_LEN + OBS_LEN;
pub(crate) const MAX_OPEN_PAYLOAD: usize = PREFIX + SNAP_LEN + MAX_PATH;
const OPEN: u8 = 12;
const COMPARE: u8 = 13;
const RELOAD: u8 = 14;
const SAVE: u8 = 15;
const OBSERVATION: u8 = 16;
const RELOADED: u8 = 17;
const SAVED: u8 = 18;
const CONFLICT: u8 = 19;
const UNAVAILABLE: u8 = 20;
const UNKNOWN: u8 = 21;
const RECOVERY_PREFIX: &str = ".zzabba-edit-";
const RECOVERY_SUFFIX: &str = ".tmp/original";
const RECOVERY_LEN: usize = RECOVERY_PREFIX.len() + 32 + RECOVERY_SUFFIX.len();

pub(crate) enum PeerFrame {
    Open(u64),
    Cancel(u64),
    Outcome(u64),
}

pub(crate) fn decode_peer_prefix(header: [u8; 4], prefix: &[u8; PREFIX]) -> io::Result<(usize, PeerFrame)> {
    let len = u32::from_be_bytes(header) as usize;
    let (kind, id, _) = with_id(prefix)?;
    let frame_kind = match kind {
        OPEN => { decode_open_len(header)?; PeerFrame::Open(id) }
        super::CANCEL if len == PREFIX => PeerFrame::Cancel(id),
        OBSERVATION if len == OP_PREFIX + OBS_LEN => PeerFrame::Outcome(id),
        RELOADED | SAVED if len == OP_PREFIX + SNAP_LEN => PeerFrame::Outcome(id),
        CONFLICT | UNAVAILABLE if len == OP_PREFIX => PeerFrame::Outcome(id),
        UNKNOWN if len == OP_PREFIX || len == OP_PREFIX + RECOVERY_LEN => PeerFrame::Outcome(id),
        _ => return Err(invalid()),
    };
    Ok((len, frame_kind))
}

pub(crate) fn is_operation_kind(kind: u8) -> bool {
    matches!(kind, COMPARE | RELOAD | SAVE)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Observation {
    pub(crate) len: u64,
    pub(crate) digest: [u8; 32],
}

impl Observation {
    fn encode(self) -> io::Result<[u8; OBS_LEN]> {
        if self.len > MAX_CONTENT as u64 {
            return Err(invalid());
        }
        let mut bytes = [0; OBS_LEN];
        bytes[..8].copy_from_slice(&self.len.to_be_bytes());
        bytes[8..].copy_from_slice(&self.digest);
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let bytes: &[u8; OBS_LEN] = bytes.try_into().map_err(|_| invalid())?;
        let (len, digest) = bytes.split_first_chunk::<8>().ok_or_else(invalid)?;
        let len = u64::from_be_bytes(*len);
        if len > MAX_CONTENT as u64 {
            return Err(invalid());
        }
        Ok(Self { len, digest: digest.try_into().map_err(|_| invalid())? })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) token: [u8; TOKEN_LEN],
    pub(crate) observation: Observation,
}

impl Snapshot {
    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let (token, observation) = bytes.split_first_chunk().ok_or_else(invalid)?;
        Ok(Self { token: *token, observation: Observation::decode(observation)? })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ActiveOpen {
    pub(crate) id: u64,
    pub(crate) path: Vec<u8>,
    pub(crate) snapshot: Snapshot,
}

pub(crate) fn decode_open_len(header: [u8; 4]) -> io::Result<usize> {
    let len = u32::from_be_bytes(header) as usize;
    if !(PREFIX + SNAP_LEN + 1..=MAX_OPEN_PAYLOAD).contains(&len) {
        return Err(invalid());
    }
    Ok(len)
}

impl ActiveOpen {
    pub(crate) fn encode(&self) -> io::Result<Vec<u8>> {
        if self.id == 0 || !valid_path(&self.path) {
            return Err(invalid());
        }
        Ok(frame(OPEN, &[
            &self.id.to_be_bytes(), &self.snapshot.token,
            &self.snapshot.observation.encode()?, &self.path,
        ]))
    }

    pub(crate) fn decode(payload: &[u8]) -> io::Result<Self> {
        let (prefix, data) = payload.split_at_checked(PREFIX).ok_or_else(invalid)?;
        let (kind, id, _) = with_id(prefix)?;
        let (snapshot, path) = data.split_at_checked(SNAP_LEN).ok_or_else(invalid)?;
        if kind != OPEN || !valid_path(path) {
            return Err(invalid());
        }
        Ok(Self { id, path: path.to_vec(), snapshot: Snapshot::decode(snapshot)? })
    }
}

#[derive(Default)]
pub(crate) struct OpIds(u64);

impl OpIds {
    pub(crate) fn next_id(&mut self) -> io::Result<u64> {
        self.0 = self.0.checked_add(1).ok_or_else(invalid)?;
        Ok(self.0)
    }

    pub(crate) fn accept(&mut self, op: u64) -> io::Result<()> {
        if self.0.checked_add(1) != Some(op) {
            return Err(invalid());
        }
        self.0 = op;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationKind {
    Compare,
    Reload,
    Save,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Operation<T = String> {
    Compare,
    Reload(Observation),
    Save { token: [u8; TOKEN_LEN], text: T },
}

pub(crate) struct OperationFrame<'a> {
    pub(crate) header: Vec<u8>,
    pub(crate) fragment: &'a [u8],
}

impl<T> Operation<T> {
    pub(crate) fn kind(&self) -> OperationKind {
        match self {
            Self::Compare => OperationKind::Compare,
            Self::Reload(_) => OperationKind::Reload,
            Self::Save { .. } => OperationKind::Save,
        }
    }
}

impl<T: AsRef<str>> Operation<T> {
    pub(crate) fn encode(&self, id: u64, op: u64) -> io::Result<OperationFrame<'_>> {
        if id == 0 || op == 0 {
            return Err(invalid());
        }
        let id = id.to_be_bytes();
        let op = op.to_be_bytes();
        let mut fragment = &[][..];
        let header = match self {
            Self::Compare => frame(COMPARE, &[&id, &op]),
            Self::Reload(value) => frame(RELOAD, &[&id, &op, &value.encode()?]),
            Self::Save { token, text } => {
                fragment = text.as_ref().as_bytes();
                if fragment.len() > MAX_CONTENT {
                    return Err(invalid());
                }
                let mut header = frame(SAVE, &[&id, &op, token]);
                let len = (OP_PREFIX + TOKEN_LEN + fragment.len()) as u32;
                header[..4].copy_from_slice(&len.to_be_bytes());
                header
            }
        };
        Ok(OperationFrame { header, fragment })
    }
}

pub(crate) fn read_operation(
    reader: &mut impl Read,
    id: u64,
    ids: &mut OpIds,
) -> io::Result<(u64, Operation)> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let len = u32::from_be_bytes(length) as usize;
    if !(OP_PREFIX..=OP_PREFIX + TOKEN_LEN + MAX_CONTENT).contains(&len) {
        return Err(invalid());
    }
    let mut prefix = [0; OP_PREFIX];
    reader.read_exact(&mut prefix)?;
    let (kind, op, _) = operation_fields(&prefix, id)?;
    let body_len = len - OP_PREFIX;
    let mut next = OpIds(ids.0);
    next.accept(op)?;
    let operation = match kind {
        COMPARE if body_len == 0 => Operation::Compare,
        RELOAD if body_len == OBS_LEN => {
            let mut observation = [0; OBS_LEN];
            reader.read_exact(&mut observation)?;
            Operation::Reload(Observation::decode(&observation)?)
        }
        SAVE if (TOKEN_LEN..=TOKEN_LEN + MAX_CONTENT).contains(&body_len) => {
            let mut token = [0; TOKEN_LEN];
            reader.read_exact(&mut token)?;
            let mut body = vec![0; body_len - TOKEN_LEN];
            reader.read_exact(&mut body)?;
            let text = String::from_utf8(body).map_err(|_| invalid())?;
            Operation::Save { token, text }
        }
        _ => return Err(invalid()),
    };
    *ids = next;
    Ok((op, operation))
}

fn operation_fields(payload: &[u8], id: u64) -> io::Result<(u8, u64, &[u8])> {
    let (kind, received_id, data) = with_id(payload)?;
    let (op, rest) = data.split_first_chunk().ok_or_else(invalid)?;
    let op = u64::from_be_bytes(*op);
    if received_id != id || op == 0 {
        return Err(invalid());
    }
    Ok((kind, op, rest))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Observation(Observation),
    Reloaded(Snapshot),
    Saved(Snapshot),
    Conflict,
    Unavailable,
    Unknown(Option<String>),
}

fn valid_recovery(name: &str) -> bool {
    name.len() == RECOVERY_LEN && name.strip_prefix(RECOVERY_PREFIX)
        .and_then(|name| name.strip_suffix(RECOVERY_SUFFIX))
        .is_some_and(|hex| hex.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))
}

impl Outcome {
    pub(crate) fn encode(&self, id: u64, op: u64) -> io::Result<Vec<u8>> {
        if id == 0 || op == 0 {
            return Err(invalid());
        }
        let id = id.to_be_bytes();
        let op = op.to_be_bytes();
        Ok(match self {
            Self::Observation(value) => frame(OBSERVATION, &[&id, &op, &value.encode()?]),
            Self::Reloaded(snapshot) | Self::Saved(snapshot) => {
                let kind = if matches!(self, Self::Reloaded(_)) { RELOADED } else { SAVED };
                frame(kind, &[&id, &op, &snapshot.token, &snapshot.observation.encode()?])
            }
            Self::Conflict => frame(CONFLICT, &[&id, &op]),
            Self::Unavailable => frame(UNAVAILABLE, &[&id, &op]),
            Self::Unknown(None) => frame(UNKNOWN, &[&id, &op]),
            Self::Unknown(Some(name)) if valid_recovery(name) => {
                frame(UNKNOWN, &[&id, &op, name.as_bytes()])
            }
            Self::Unknown(Some(_)) => return Err(invalid()),
        })
    }

    pub(crate) fn decode(payload: &[u8], id: u64, op: u64, for_kind: OperationKind) -> io::Result<Self> {
        let (kind, received_op, data) = operation_fields(payload, id)?;
        if received_op != op {
            return Err(invalid());
        }
        match (for_kind, kind, data) {
            (OperationKind::Compare, OBSERVATION, _) => Ok(Self::Observation(Observation::decode(data)?)),
            (OperationKind::Reload, RELOADED, _) => Ok(Self::Reloaded(Snapshot::decode(data)?)),
            (OperationKind::Save, SAVED, _) => Ok(Self::Saved(Snapshot::decode(data)?)),
            (_, CONFLICT, []) => Ok(Self::Conflict),
            (_, UNAVAILABLE, []) => Ok(Self::Unavailable),
            (OperationKind::Save, UNKNOWN, []) => Ok(Self::Unknown(None)),
            (OperationKind::Save, UNKNOWN, _) if data.len() == RECOVERY_LEN => {
                let name = std::str::from_utf8(data).map_err(|_| invalid())?;
                if !valid_recovery(name) {
                    return Err(invalid());
                }
                Ok(Self::Unknown(Some(name.to_owned())))
            }
            _ => Err(invalid()),
        }
    }
}

#[cfg(test)]
#[path = "publication_tests.rs"]
mod tests;
