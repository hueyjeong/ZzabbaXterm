use std::{
    sync::atomic::{AtomicU64, Ordering},
};

use tokio::io::{AsyncWriteExt, duplex};

use super::*;

#[derive(Default)]
struct State {
    reviews: AtomicU64,
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

    fn request_review(&self, proposal: Proposal, _: Instant) -> Result<String, Rejected> {
        assert_eq!(proposal.operation, Query::Uptime);
        if proposal.target_id != "dev-a" {
            return Err(Rejected);
        }
        self.reviews.fetch_add(1, Ordering::Relaxed);
        Ok("review-1".into())
    }
}

#[tokio::test]
async fn restricted_methods_and_exact_fields() {
    let requests = Arc::new(State::default());
    for bytes in [
        br#"{"method":"approve","proposalId":"review-1"}"#.as_slice(),
        br#"{"method":"save"}"#,
        br#"{"method":"register"}"#,
        br#"{"method":"draft"}"#,
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
        serve_connection(&mut server, requests.clone(), Arc::new(Semaphore::new(1))).await.unwrap();
        let reply = read_frame(&mut client, MAX_FRAME).await.unwrap();
        assert_eq!(decode::<Receipt>(&reply).unwrap(), Receipt::Failed { error: Error::InvalidRequest }, "{bytes:?}");
    }
    assert_eq!(requests.reviews.load(Ordering::Relaxed), 0);
}

#[test]
fn register_adds_to_the_app_store_without_overwriting_existing_profiles() {
    let root = connections::tests::TestDir::new();
    let directory = root.path().join("config");
    let mut input = connections::tests::draft(&root.path().join("missing"), "work", "dev-a");
    let parse_registration = |value: &serde_json::Value| {
        let Command::Register(draft) = parse("register", value.to_string().as_bytes()).unwrap() else {
            panic!("등록 명령이 필요합니다");
        };
        draft
    };
    let output = register(&directory, parse_registration(&input)).unwrap();
    assert_eq!(serde_json::to_value(output).unwrap(), serde_json::json!({"id": "dev-a", "generation": "1"}));
    input["targetRevision"] = serde_json::json!(1);
    input["target"]["displayName"] = serde_json::json!("바뀐 이름");
    assert!(matches!(register(&directory, parse_registration(&input)), Err(Error::TargetConflict)));
    let saved = connections::ai_aliases(&directory).unwrap();
    assert_eq!(saved, [Alias {
        id: "dev-a".into(), display_name: "작업 셸".into(), method: "local".into(), generation: "1".into(),
    }]);
}

#[test]
fn register_saves_supported_remote_settings_without_keys_or_host_trust() {
    use serde_json::json;

    let root = connections::tests::TestDir::new();
    let missing_key = root.path().join("missing-key");
    let ssh = connections::tests::ssh_draft(root.path());
    let mut ssm = ssh.clone();
    ssm["target"] = json!({
        "id": "ssm-node", "groupId": "work", "displayName": "개발 노드", "defaultMethod": "ssm",
        "ssm": {"profile": "missing-profile", "region": "ap-northeast-2", "instanceId": "i-0123456789abcdef0"}
    });
    for (index, input) in [
        json!({"kind": "manualPassword"}),
        json!({"kind": "keyFile", "privateKeyPath": missing_key}),
        json!({"kind": "agent", "publicKeyPath": missing_key}),
    ].into_iter().map(|auth| {
        let mut input = ssh.clone();
        input["target"]["ssh"]["auth"] = auth;
        input
    }).chain([ssm]).enumerate() {
        let Command::Register(draft) = parse("register", input.to_string().as_bytes()).unwrap() else {
            panic!("등록 명령이 필요합니다");
        };
        let output = register(&root.path().join(index.to_string()), draft).unwrap();
        assert_eq!(serde_json::to_value(output).unwrap(), json!({"id": input["target"]["id"], "generation": "1"}));
    }
}

#[test]
fn register_rejects_unsupported_ssh_settings_before_save() {
    use serde_json::json;

    let root = connections::tests::TestDir::new();
    for (field, value, error) in [
        ("transport", json!({"kind": "ssm", "profile": "dev", "region": "ap-northeast-2", "instanceId": "i-0123456789abcdef0"}), Error::SshOverSsmUnavailable),
        ("auth", json!({"kind": "storedPassword", "itemId": "vault-item"}), Error::StoredSshPasswordUnavailable),
    ] {
        let mut input = connections::tests::ssh_draft(root.path());
        input["target"]["ssh"][field] = value;
        assert_eq!(parse("register", input.to_string().as_bytes()).err(), Some(error));
    }
}

#[cfg(windows)]
#[test]
fn register_rejects_nonstandard_windows_start_directories() {
    for path in [r"\\server\share\folder", r"\\?\C:\folder", r"\\.\C:\folder"] {
        let input = connections::tests::draft(Path::new(path), "work", "dev-a");
        assert_eq!(parse("register", input.to_string().as_bytes()).err(), Some(Error::UnsupportedWindowsStartDirectory), "{path}");
    }
}

#[tokio::test]
async fn invalid_frame_length_and_stalled_client() {
    let requests = Arc::new(State::default());
    for size in [0, (MAX_FRAME + 1) as u32] {
        let (mut client, mut server) = duplex(128);
        client.write_u32(size).await.unwrap();
        serve_connection(&mut server, requests.clone(), Arc::new(Semaphore::new(1))).await.unwrap();
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
        serve_connection(&mut server, requests, Arc::new(Semaphore::new(1)))
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
    let requests = Arc::new(State::default());
    for (call, expected) in [
        (
            Outbound::List,
            Ok(Receipt::Listed {
                connections: requests.list().unwrap(),
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
            serve_connection(&mut connection, requests.clone(), Arc::new(Semaphore::new(1))).await
        });
        server.unwrap();
        assert_eq!(result, expected);
    }
    assert_eq!(requests.reviews.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn a_partial_client_does_not_block_other_clients_and_busy_workers_reject() {
    let endpoint = Endpoint::for_service(&format!("parallel-test-{}", std::process::id())).unwrap();
    let listener = Listener::bind(&endpoint).unwrap();
    let service = tokio::spawn(serve(listener, State::default()));
    let mut stalled = endpoint.connect().await.unwrap();
    stalled.write_u32(12).await.unwrap();
    stalled.write_u8(b'{').await.unwrap();
    let reply = timeout(std::time::Duration::from_secs(2), exchange(&endpoint, &Outbound::List)).await.unwrap();
    assert!(matches!(reply, Ok(Receipt::Listed { .. })));
    service.abort();

    let (mut client, mut server) = duplex(128);
    write_frame(&mut client, br#"{"method":"list"}"#, MAX_FRAME).await.unwrap();
    serve_connection(&mut server, Arc::new(State::default()), Arc::new(Semaphore::new(0))).await.unwrap();
    assert_eq!(read_frame(&mut client, MAX_FRAME).await.unwrap(), br#"{"error":"rejected"}"#);
}

#[tokio::test]
async fn actual_json_overflow_rejects_before_connect_and_returns_a_wire_error() {
    let endpoint = Endpoint::for_service(&format!("overflow-test-{}", std::process::id())).unwrap();
    let request = Outbound::Propose { proposal: Proposal {
        target_id: "a".repeat(MAX_FRAME), generation: "1".into(), operation: Query::Uptime,
    }};
    assert_eq!(exchange(&endpoint, &request).await, Err(Error::InvalidRequest));

    struct Oversized;
    impl Requests for Oversized {
        fn list(&self) -> Result<Vec<Alias>, Rejected> {
            Ok(vec![Alias { id: "a".into(), display_name: "한".repeat(MAX_FRAME / 2), method: "local".into(), generation: "1".into() }])
        }
        fn request_review(&self, _: Proposal, _: Instant) -> Result<String, Rejected> { Err(Rejected) }
    }
    let listener = Listener::bind(&endpoint).unwrap();
    let service = tokio::spawn(serve(listener, Oversized));
    assert_eq!(exchange(&endpoint, &Outbound::List).await, Err(Error::Rejected));
    service.abort();
}
