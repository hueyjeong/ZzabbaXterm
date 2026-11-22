use std::time::Duration;

use tokio::{sync::{mpsc, oneshot}, time::timeout};
use tokio_util::sync::CancellationToken;

pub(crate) use super::super::nano_wire::publication::Operation as PublicationOperation;
use super::super::nano_wire::publication::{MAX_CONTENT, Observation, Snapshot};

// SFTP 파일 I/O와 같은 시간 한도 안에서 슬롯을 반환한다.
pub(crate) const PUBLICATION_IO_LIMIT: Duration = Duration::from_secs(10);

pub(crate) struct Publisher {
    open_id: u64,
    request: CancellationToken,
    work: mpsc::Sender<PublicationWork>,
}

// 워커는 프레임 전송 직전에 요청 취소와 응답 수신자 종료를 확인한다.
pub(crate) struct PublicationWork {
    pub(crate) open_id: u64,
    pub(crate) request: CancellationToken,
    pub(crate) operation: PublicationOperation,
    pub(crate) reply: oneshot::Sender<Result<PublicationReply, PublishError>>,
}

impl PublicationWork {
    pub(crate) fn respond(self, result: Result<PublicationReply, PublishError>) {
        let _ = self.reply.send(result);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PublicationReply {
    Observed(Observation),
    Reloaded(Snapshot),
    Saved(Snapshot),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PublishError {
    Conflict,
    Unavailable,
    Unknown(Option<String>),
}

impl Publisher {
    #[cfg(test)]
    pub(crate) fn channel(open_id: u64, request: CancellationToken) -> (Self, mpsc::Receiver<PublicationWork>) {
        let (work, receiver) = mpsc::channel(1);
        (Self::new(open_id, request, work), receiver)
    }

    pub(crate) fn new(open_id: u64, request: CancellationToken, work: mpsc::Sender<PublicationWork>) -> Self {
        Self { open_id, request, work }
    }

    pub(crate) async fn compare(&self) -> Result<Observation, PublishError> {
        match self.submit(|| PublicationOperation::Compare, PublishError::Unavailable).await? {
            PublicationReply::Observed(observation) => Ok(observation),
            _ => Err(PublishError::Unavailable),
        }
    }

    pub(crate) async fn reload(&self, observation: Observation) -> Result<Snapshot, PublishError> {
        match self.submit(|| PublicationOperation::Reload(observation), PublishError::Unavailable).await? {
            PublicationReply::Reloaded(snapshot) => Ok(snapshot),
            _ => Err(PublishError::Unavailable),
        }
    }

    pub(crate) async fn save(&self, token: [u8; 16], text: &str) -> Result<Snapshot, PublishError> {
        if text.len() > MAX_CONTENT {
            return Err(PublishError::Unavailable);
        }
        match self.submit(|| PublicationOperation::Save { token, text: text.to_owned() }, PublishError::Unknown(None)).await? {
            PublicationReply::Saved(snapshot) => Ok(snapshot),
            _ => Err(PublishError::Unknown(None)),
        }
    }

    async fn submit(
        &self, operation: impl FnOnce() -> PublicationOperation, lost: PublishError,
    ) -> Result<PublicationReply, PublishError> {
        let permit = self.work.try_reserve().map_err(|_| PublishError::Unavailable)?;
        if self.request.is_cancelled() {
            return Err(PublishError::Unavailable);
        }
        let operation = operation();
        if self.request.is_cancelled() || self.work.is_closed() {
            return Err(PublishError::Unavailable);
        }
        let (reply, mut received) = oneshot::channel();
        permit.send(PublicationWork { open_id: self.open_id, request: self.request.clone(), operation, reply });

        // 큐에 넣은 뒤에는 활성 요청이 취소되어도 워커의 확정 응답을 기다린다.
        match timeout(PUBLICATION_IO_LIMIT, &mut received).await {
            Ok(result) => result.unwrap_or(Err(lost)),
            Err(_) => {
                received.close();
                received.try_recv().unwrap_or(Err(lost))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OBSERVATION: Observation = Observation { len: 0, digest: [0; 32] };
    const SNAPSHOT: Snapshot = Snapshot { token: [7; 16], observation: OBSERVATION };

    #[tokio::test]
    async fn bounded_queue_rejects_work_before_dispatch() {
        let request = CancellationToken::new();
        let (publisher, mut queue) = Publisher::channel(23, request.clone());
        let oversized = "한".repeat(MAX_CONTENT / "한".len() + 1);
        assert_eq!(publisher.save(SNAPSHOT.token, &oversized).await, Err(PublishError::Unavailable));
        assert!(matches!(queue.try_recv(), Err(mpsc::error::TryRecvError::Empty)));

        let (observed, ()) = tokio::join!(biased; publisher.compare(), async {
            assert_eq!(publisher.save(SNAPSHOT.token, "한글").await, Err(PublishError::Unavailable));
            let work = queue.try_recv().unwrap();
            assert_eq!(work.open_id, 23);
            assert_eq!(work.operation, PublicationOperation::Compare);
            work.respond(Ok(PublicationReply::Observed(OBSERVATION)));
        });
        assert_eq!(observed, Ok(OBSERVATION));

        request.cancel();
        assert_eq!(publisher.reload(OBSERVATION).await, Err(PublishError::Unavailable));
        assert!(matches!(queue.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
        let (publisher, queue) = Publisher::channel(24, CancellationToken::new());
        drop(queue);
        assert_eq!(publisher.save(SNAPSHOT.token, "한글").await, Err(PublishError::Unavailable));
    }

    #[tokio::test]
    async fn cancellation_preserves_qualified_save_outcomes() {
        for (reply, expected) in [
            (Ok(PublicationReply::Saved(SNAPSHOT)), Ok(SNAPSHOT)),
            (Err(PublishError::Conflict), Err(PublishError::Conflict)),
            (Err(PublishError::Unavailable), Err(PublishError::Unavailable)),
            (Err(PublishError::Unknown(Some("recovery".into()))), Err(PublishError::Unknown(Some("recovery".into())))),
        ] {
            let request = CancellationToken::new();
            let (publisher, mut queue) = Publisher::channel(23, request.clone());
            let (saved, ()) = tokio::join!(biased; publisher.save(SNAPSHOT.token, "한글"), async {
                let work = queue.try_recv().unwrap();
                assert!(matches!(&work.operation, PublicationOperation::Save { token, text }
                    if *token == SNAPSHOT.token && text == "한글"));
                assert!(!work.request.is_cancelled());
                request.cancel();
                assert!(work.request.is_cancelled());
                tokio::task::yield_now().await;
                work.respond(reply);
            });
            assert_eq!(saved, expected);
        }
    }

    #[tokio::test]
    async fn lost_save_reply_after_dequeue_is_unknown() {
        let request = CancellationToken::new();
        let (publisher, mut queue) = Publisher::channel(23, request.clone());
        let (saved, ()) = tokio::join!(biased; publisher.save(SNAPSHOT.token, "한글"), async {
            let work = queue.try_recv().unwrap();
            work.request.cancel();
            assert!(request.is_cancelled());
            drop(work);
        });
        assert_eq!(saved, Err(PublishError::Unknown(None)));
    }

    #[tokio::test]
    async fn abandoned_save_closes_reply_before_dispatch() {
        let request = CancellationToken::new();
        let (publisher, mut queue) = Publisher::channel(23, request.clone());
        let work = tokio::select! {
            biased;
            result = publisher.save(SNAPSHOT.token, "한글") => panic!("unexpected save result: {result:?}"),
            work = queue.recv() => work.unwrap(),
        };
        assert!(work.reply.is_closed());
        assert!(!request.is_cancelled());
    }
}
