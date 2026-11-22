use std::{
    cell::Cell,
    sync::atomic::{AtomicU64, Ordering},
};

use tokio::io::{AsyncWriteExt, duplex};

use super::*;

#[derive(Default)]
struct State {
    drafts: Cell<usize>,
    reviews: Cell<usize>,
}

impl Requests for State {
    fn list(&self) -> Result<Vec<Alias>, Rejected> {
        Ok(vec![Alias {
            id: "dev-a".into(),
            display_name: "개발 노드".into(),
            method: "local".into(),
            generation: "4".into(),
        }])
    }

    fn open_draft(&self, draft: ValidatedDraft) -> Result<String, Rejected> {
        assert!(matches!(draft.method(), crate::connections::profile::MethodRef::Local { start_directory, .. } if start_directory.is_absolute()));
        self.drafts.set(self.drafts.get() + 1);
        Ok("draft-1".into())
    }

    fn request_review(&self, proposal: Proposal) -> Result<String, Rejected> {
        assert_eq!(proposal.operation, Query::Uptime);
        if proposal.target_id != "dev-a" {
            return Err(Rejected);
        }
        self.reviews.set(self.reviews.get() + 1);
        Ok("review-1".into())
    }
}

#[tokio::test]
async fn restricted_methods_and_exact_fields() {
    let requests = State::default();
    for bytes in [
        br#"{"method":"approve","proposalId":"review-1"}"#.as_slice(),
        br#"{"method":"save"}"#,
        br#"{"method":"session"}"#,
        br#"{"method":"execute"}"#,
        br#"{"method":"shell"}"#,
        br#"{"method":"server"}"#,
        br#"{"method":"vault"}"#,
        br#"{"method":"cloud"}"#,
        br#"{"method":"list","token":"forged"}"#,
        br#"{"method":"propose","approved":true,"proposal":{"targetId":"dev-a","operation":"uptime"}}"#,
        br#"{"method":"propose","proposal":{"targetId":"dev-a","operation":"uptime","arguments":[]}}"#,
        br#"{"method":"propose","proposal":{"targetId":"dev-a","operation":"uptime; id"}}"#,
        br#"{"method":"propose","proposal":{"targetId":"dev a","operation":"uptime"}}"#,
        br#"{"method":"list"}{"method":"list"}"#,
        b"{\"method\":\"\xff\"}",
    ] {
        let (mut client, mut server) = duplex(MAX_FRAME);
        write_frame(&mut client, bytes, MAX_FRAME).await.unwrap();
        serve_connection(&mut server, &requests).await.unwrap();
        let reply = read_frame(&mut client, MAX_FRAME).await.unwrap();
        assert_eq!(decode::<Receipt>(&reply).unwrap(), Receipt::Failed { error: Error::InvalidRequest }, "{bytes:?}");
    }
    assert_eq!(requests.drafts.get(), 0);
    assert_eq!(requests.reviews.get(), 0);
}

#[tokio::test]
async fn invalid_frame_length_and_stalled_client() {
    let requests = State::default();
    for size in [0, (MAX_FRAME + 1) as u32] {
        let (mut client, mut server) = duplex(128);
        client.write_u32(size).await.unwrap();
        serve_connection(&mut server, &requests).await.unwrap();
        assert_eq!(
            decode::<Receipt>(&read_frame(&mut client, MAX_FRAME).await.unwrap()).unwrap(),
            Receipt::Failed {
                error: Error::InvalidRequest
            }
        );
    }
    let (mut client, mut server) = duplex(128);
    client.write_u32(32).await.unwrap();
    client.write_u8(b'{').await.unwrap();
    assert_eq!(
        serve_connection(&mut server, &requests)
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut
    );
}

#[tokio::test]
async fn verified_local_transport_dispatches_only_requests() {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let service = format!(
        "ai-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let endpoint = Endpoint::for_service(&service).unwrap();
    let mut listener = Listener::bind(&endpoint).unwrap();
    assert!(Listener::bind(&endpoint).is_err());
    let request = |target| Outbound::Propose {
        proposal: Proposal {
            target_id: target,
            generation: "4".into(),
            operation: Query::Uptime,
        },
    };
    let draft = serde_json::json!({
        "version": 1,
        "group": {"id": "dev", "displayName": "개발"},
        "target": {"id": "dev-a", "groupId": "dev", "displayName": "노드", "defaultMethod": "local",
            "local": {"shell": if cfg!(windows) { "powerShell" } else { "bash" },
                      "startDirectory": std::env::temp_dir()}}
    });
    let draft = parse("draft", serde_json::to_vec(&draft).unwrap().as_slice())
        .ok()
        .unwrap();
    let requests = State::default();
    for (call, expected) in [
        (
            Outbound::List,
            Ok(Receipt::Listed {
                connections: requests.list().unwrap(),
            }),
        ),
        (
            draft,
            Ok(Receipt::Drafted {
                draft_id: "draft-1".into(),
            }),
        ),
        (request("missing".into()), Err(Error::Rejected)),
        (
            request("dev-a".into()),
            Ok(Receipt::Proposed {
                proposal_id: "review-1".into(),
            }),
        ),
    ] {
        let (result, server) = tokio::join!(exchange(&endpoint, &call), async {
            let mut connection = listener.accept().await.unwrap();
            serve_connection(&mut connection, &requests).await
        });
        server.unwrap();
        assert_eq!(result, expected);
    }
    assert_eq!(requests.drafts.get(), 1);
    assert_eq!(requests.reviews.get(), 1);
}
