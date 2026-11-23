use super::*;
use crate::editor::nano_bridge::PublicationOperation;
use crate::editor::nano_wire::{Request, publication::{Observation, OpIds, Snapshot, read_operation}};
use tokio::io::{DuplexStream, ReadHalf, WriteHalf, duplex, split};

type Reply = oneshot::Receiver<Result<PublicationReply, PublishError>>;
const ID: u64 = 9;
const LIMIT: Duration = Duration::from_secs(2);
const OBSERVATION: Observation = Observation { len: 3, digest: [7; 32] };
const SNAPSHOT: Snapshot = Snapshot { token: [5; 16], observation: OBSERVATION };

struct Harness {
    output: ReadHalf<DuplexStream>,
    input: WriteHalf<DuplexStream>,
    requests: mpsc::Receiver<BridgeRequest>,
    responses: mpsc::Sender<Response>,
    work: mpsc::Sender<PublicationWork>,
}

fn setup(capacity: usize, limit: Duration, cancel: &CancellationToken) -> (Harness, impl Future<Output = io::Result<()>> + '_) {
    let (remote, local) = duplex(capacity);
    let (output, input) = split(remote);
    let (local_read, local_write) = split(local);
    let (requests, request_rx) = mpsc::channel(QUEUE);
    let (responses, response_rx) = mpsc::channel(QUEUE);
    let (work, work_rx) = mpsc::channel(WORK_QUEUE);
    (
        Harness { output, input, requests: request_rx, responses, work },
        transport(local_write, local_read, cancel, requests, response_rx, work_rx, limit),
    )
}

impl Harness {
    async fn open(&mut self, id: u64) {
        let open = ActiveOpen { id, snapshot: SNAPSHOT, path: b"/a".to_vec() };
        self.input.write_all(&open.encode().unwrap()).await.unwrap();
        assert!(matches!(self.requests.recv().await, Some(BridgeRequest::Open(actual)) if actual == open));
    }

    async fn submit(&self, open_id: u64, operation: PublicationOperation, request: CancellationToken) -> Reply {
        let (reply, receive) = oneshot::channel();
        assert!(self.work.send(PublicationWork { open_id, request, operation, reply }).await.is_ok());
        receive
    }

    async fn read_small(&mut self) -> Vec<u8> {
        let mut header = [0; 4];
        self.output.read_exact(&mut header).await.unwrap();
        let length = u32::from_be_bytes(header) as usize;
        assert!(length <= 128);
        let mut frame = vec![0; length + 4];
        frame[..4].copy_from_slice(&header);
        self.output.read_exact(&mut frame[4..]).await.unwrap();
        frame
    }
}

fn save() -> PublicationOperation {
    PublicationOperation::Save { token: SNAPSHOT.token, text: "한".to_owned() }
}

#[tokio::test]
async fn exact_open_and_increasing_operation_ids_survive_fragmented_input() {
    let cancel = CancellationToken::new();
    let (mut peer, relay) = setup(128, LIMIT, &cancel);
    let drive = async {
        peer.open(ID).await;
        let stale = peer.submit(ID - 1, save(), CancellationToken::new()).await;
        assert_eq!(stale.await.unwrap(), Err(PublishError::Unavailable));

        let old_cancel = Request::Cancel { id: ID - 1 }.encode().unwrap();
        peer.input.write_all(&old_cancel[..6]).await.unwrap();
        let reply = peer.submit(ID, PublicationOperation::Compare, CancellationToken::new()).await;
        let frame = peer.read_small().await;
        let mut ids = OpIds::default();
        assert_eq!(read_operation(&mut &frame[..], ID, &mut ids).unwrap(), (1, PublicationOperation::Compare));
        peer.input.write_all(&old_cancel[6..]).await.unwrap();
        peer.input.write_all(&Outcome::Observation(OBSERVATION).encode(ID, 1).unwrap()).await.unwrap();
        assert_eq!(reply.await.unwrap(), Ok(PublicationReply::Observed(OBSERVATION)));
        assert!(matches!(peer.requests.try_recv(), Err(mpsc::error::TryRecvError::Empty)));

        let reply = peer.submit(ID, save(), CancellationToken::new()).await;
        let frame = peer.read_small().await;
        assert_eq!(read_operation(&mut &frame[..], ID, &mut ids).unwrap(), (2, save()));
        peer.input.write_all(&Outcome::Saved(SNAPSHOT).encode(ID, 2).unwrap()).await.unwrap();
        assert_eq!(reply.await.unwrap(), Ok(PublicationReply::Saved(SNAPSHOT)));
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(relay, drive);
    assert!(result.is_ok());
}

#[tokio::test]
async fn wrong_open_operation_id_or_outcome_kind_disconnects_without_replaying_save() {
    for (id, op, outcome) in [
        (ID + 1, 1, Outcome::Saved(SNAPSHOT)),
        (ID, 2, Outcome::Saved(SNAPSHOT)),
        (ID, 1, Outcome::Observation(OBSERVATION)),
    ] {
        let cancel = CancellationToken::new();
        let (mut peer, relay) = setup(128, LIMIT, &cancel);
        let drive = async {
            peer.open(ID).await;
            let reply = peer.submit(ID, save(), CancellationToken::new()).await;
            let _ = peer.read_small().await;
            peer.input.write_all(&outcome.encode(id, op).unwrap()).await.unwrap();
            assert_eq!(reply.await.unwrap(), Err(PublishError::Unknown(None)));
            assert_eq!(peer.output.read(&mut [0]).await.unwrap(), 0);
        };
        let (result, ()) = tokio::join!(relay, drive);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}

#[tokio::test]
async fn exact_cancel_keeps_started_save_ahead_of_detach_and_drops_unsent_work() {
    let cancel = CancellationToken::new();
    let (mut peer, relay) = setup(128, LIMIT, &cancel);
    let drive = async {
        peer.open(ID).await;
        let reply = peer.submit(ID, save(), CancellationToken::new()).await;
        let _ = peer.read_small().await;
        let queued = peer.submit(ID, save(), CancellationToken::new()).await;
        peer.input.write_all(&Request::Cancel { id: ID }.encode().unwrap()).await.unwrap();
        assert!(matches!(peer.requests.recv().await, Some(BridgeRequest::Cancel(ID))));
        peer.responses.send(Response::Closed { id: ID }).await.unwrap();
        assert!(timeout(Duration::from_millis(10), peer.output.read(&mut [0])).await.is_err());
        let recovery = ".zzabba-edit-00000000000000000000000000000001.tmp/original".to_owned();
        peer.input.write_all(&Outcome::Unknown(Some(recovery.clone())).encode(ID, 1).unwrap()).await.unwrap();
        assert_eq!(reply.await.unwrap(), Err(PublishError::Unknown(Some(recovery))));
        assert_eq!(queued.await.unwrap(), Err(PublishError::Unavailable));
        let response = peer.read_small().await;
        assert_eq!(Response::decode(&response[4..]).unwrap(), Response::Closed { id: ID });

        peer.open(ID + 1).await;
        let reply = peer.submit(ID + 1, PublicationOperation::Compare, CancellationToken::new()).await;
        let frame = peer.read_small().await;
        assert_eq!(read_operation(&mut &frame[..], ID + 1, &mut OpIds::default()).unwrap(), (1, PublicationOperation::Compare));
        peer.input.write_all(&Outcome::Observation(OBSERVATION).encode(ID + 1, 1).unwrap()).await.unwrap();
        assert_eq!(reply.await.unwrap(), Ok(PublicationReply::Observed(OBSERVATION)));
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(relay, drive);
    assert!(result.is_ok());
}

#[tokio::test]
async fn cancellation_and_closed_receivers_skip_unsent_work() {
    for closed in [false, true] {
        let cancel = CancellationToken::new();
        let (mut peer, relay) = setup(128, LIMIT, &cancel);
        let drive = async {
            peer.open(ID).await;
            let request = CancellationToken::new();
            if !closed { request.cancel(); }
            let reply = peer.submit(ID, save(), request).await;
            if closed {
                drop(reply);
            } else {
                assert_eq!(reply.await.unwrap(), Err(PublishError::Unavailable));
            }
            assert!(timeout(Duration::from_millis(10), peer.output.read(&mut [0])).await.is_err());
            cancel.cancel();
        };
        let ((), result) = tokio::join!(drive, relay);
        assert!(result.is_ok());
    }

    let cancel = CancellationToken::new();
    let (mut peer, relay) = setup(128, LIMIT, &cancel);
    let drive = async {
        peer.open(ID).await;
        let reply = peer.submit(ID, save(), CancellationToken::new()).await;
        let Harness { mut output, input: _input, responses, .. } = peer;
        drop(responses);
        assert_eq!(reply.await.unwrap(), Err(PublishError::Unavailable));
        assert_eq!(output.read(&mut [0]).await.unwrap(), 0);
    };
    let ((), result) = tokio::join!(drive, relay);
    assert!(result.is_ok());
}

#[tokio::test]
async fn write_timeout_response_timeout_and_eof_after_send_are_unknown() {
    for (capacity, close, kind) in [
        (16, false, io::ErrorKind::TimedOut),
        (128, false, io::ErrorKind::TimedOut),
        (128, true, io::ErrorKind::UnexpectedEof),
    ] {
        let cancel = CancellationToken::new();
        let (mut peer, relay) = setup(capacity, Duration::from_millis(50), &cancel);
        let drive = async {
            peer.open(ID).await;
            let reply = peer.submit(ID, save(), CancellationToken::new()).await;
            let already_read = if capacity > 16 { peer.read_small().await.len() } else { 0 };
            if close { peer.input.shutdown().await.unwrap(); }
            assert_eq!(reply.await.unwrap(), Err(PublishError::Unknown(None)));
            let mut remaining = Vec::new();
            peer.output.read_to_end(&mut remaining).await.unwrap();
            let frame = save();
            let frame = frame.encode(ID, 1).unwrap();
            let expected = (frame.header.len() + frame.fragment.len()).min(capacity);
            assert_eq!(remaining.len() + already_read, expected);
        };
        let (result, ()) = tokio::join!(relay, drive);
        assert_eq!(result.unwrap_err().kind(), kind);
    }
}

#[tokio::test]
async fn cancellation_reports_pending_save_and_releases_queued_reply_before_bridge_cleanup() {
    let cancel = CancellationToken::new();
    let (mut peer, relay) = setup(128, LIMIT, &cancel);
    let drive = async {
        peer.open(ID).await;
        let pending = peer.submit(ID, save(), CancellationToken::new()).await;
        let _ = peer.read_small().await;
        let queued = peer.submit(ID, save(), CancellationToken::new()).await;
        cancel.cancel();
        assert_eq!(pending.await.unwrap(), Err(PublishError::Unknown(None)));
        assert_eq!(queued.await.unwrap(), Err(PublishError::Unavailable));
        assert_eq!(peer.output.read(&mut [0]).await.unwrap(), 0);
    };
    let (result, ()) = tokio::join!(relay, drive);
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
}

#[tokio::test]
async fn peer_frame_cap_matches_the_wire_and_rejects_stock_open_at_the_prefix() {
    let mut path = vec![b'x'; 4096];
    path[0] = b'/';
    let open = ActiveOpen { id: ID, path, snapshot: SNAPSHOT };
    let encoded = open.encode().unwrap();
    assert_eq!(encoded.len(), 4 + MAX_OPEN_PAYLOAD);
    let mut reader = PeerReader { input: &encoded[..], bytes: [0; 4 + MAX_OPEN_PAYLOAD], filled: 0 };
    let (kind, payload) = reader.next().await.unwrap();
    assert!(matches!(kind, PeerFrame::Open(ID)));
    assert_eq!(ActiveOpen::decode(payload).unwrap(), open);

    let stock = Request::Open { id: ID, path: b"/stock".to_vec() }.encode().unwrap();
    let mut oversized = encoded[..4 + PEER_PREFIX].to_vec();
    oversized[..4].copy_from_slice(&(MAX_OPEN_PAYLOAD as u32 + 1).to_be_bytes());
    let mut oversized_outcome = Outcome::Saved(SNAPSHOT).encode(ID, 1).unwrap();
    oversized_outcome[..4].copy_from_slice(&(MAX_OPEN_PAYLOAD as u32).to_be_bytes());
    for bytes in [&stock[..4 + PEER_PREFIX], oversized.as_slice(), &oversized_outcome[..4 + PEER_PREFIX]] {
        let mut reader = PeerReader { input: bytes, bytes: [0; 4 + MAX_OPEN_PAYLOAD], filled: 0 };
        assert!(matches!(reader.next().await, Err(error) if error.kind() == io::ErrorKind::InvalidData));
    }
}
