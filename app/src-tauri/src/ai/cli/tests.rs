use std::{
    sync::atomic::{AtomicU64, Ordering},
};

use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

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
        br#"{"method":"edit","path":"relative"}"#,
        br#"{"method":"edit","path":"relative","wait":true,"approved":true}"#,
        br#"{"method":"edit","path":"relative","path":"other","wait":true}"#,
        br#"{"method":"list"}{"method":"list"}"#,
        b"{\"method\":\"\xff\"}",
    ] {
        let (mut client, mut server) = duplex(MAX_FRAME);
        write_frame(&mut client, bytes, MAX_FRAME).await.unwrap();
        assert!(serve_connection(&mut server, requests.clone(), Arc::new(Semaphore::new(1)), &Arc::new(Semaphore::new(MAX_EDITS))).await.unwrap().is_none());
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
fn register_s3_keeps_the_profile_untested_and_outside_ai_targets() {
    let root = connections::tests::TestDir::new();
    let directory = root.path().join("config");
    let mut input = connections::tests::s3_draft(root.path());
    input["target"]["s3"]["profile"] = serde_json::json!("missing-profile");
    let Command::Register(draft) = parse("register", input.to_string().as_bytes()).unwrap() else {
        panic!("등록 명령이 필요합니다");
    };
    let output = register(&directory, draft).unwrap();
    assert_eq!(serde_json::to_value(output).unwrap(), serde_json::json!({"id": "s3-bucket", "generation": "1"}));
    connections::with_saved_method(&directory, "s3-bucket", |method, account| {
        assert!(matches!(method, MethodRef::S3 { s3 } if s3.profile == "missing-profile"));
        assert_eq!(account, None);
        Ok(())
    }).unwrap();
    assert!(connections::ai_aliases(&directory).unwrap().is_empty());

    input["target"]["s3"]["testedAccountId"] = serde_json::json!("012345678901");
    assert_eq!(parse("register", input.to_string().as_bytes()).err(), Some(Error::InvalidRequest));
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
        serve_connection(&mut server, requests.clone(), Arc::new(Semaphore::new(1)), &Arc::new(Semaphore::new(MAX_EDITS))).await.unwrap();
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
        serve_connection(&mut server, requests, Arc::new(Semaphore::new(1)), &Arc::new(Semaphore::new(MAX_EDITS)))
            .await
            .err().unwrap()
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
            serve_connection(&mut connection, requests.clone(), Arc::new(Semaphore::new(1)), &Arc::new(Semaphore::new(MAX_EDITS))).await
        });
        assert!(server.unwrap().is_none());
        assert_eq!(result, expected);
    }
    assert_eq!(requests.reviews.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn a_partial_client_does_not_block_other_clients_and_busy_workers_reject() {
    let endpoint = Endpoint::for_service(&format!("parallel-test-{}", std::process::id())).unwrap();
    let listener = Listener::bind(&endpoint).unwrap();
    let service = tokio::spawn(serve(listener, State::default(), deny_edit));
    let mut stalled = endpoint.connect().await.unwrap();
    stalled.write_u32(12).await.unwrap();
    stalled.write_u8(b'{').await.unwrap();
    let reply = timeout(std::time::Duration::from_secs(2), exchange(&endpoint, &Outbound::List)).await.unwrap();
    assert!(matches!(reply, Ok(Receipt::Listed { .. })));
    service.abort();

    let (mut client, mut server) = duplex(128);
    write_frame(&mut client, br#"{"method":"list"}"#, MAX_FRAME).await.unwrap();
    serve_connection(&mut server, Arc::new(State::default()), Arc::new(Semaphore::new(0)), &Arc::new(Semaphore::new(MAX_EDITS))).await.unwrap();
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
    let service = tokio::spawn(serve(listener, Oversized, deny_edit));
    assert_eq!(exchange(&endpoint, &Outbound::List).await, Err(Error::Rejected));
    service.abort();
}

async fn deny_edit(pending: edit::Pending) {
    pending.open_failed();
}

async fn edit_response(stream: &mut (impl AsyncRead + Unpin)) -> edit::Response {
    let bytes = timeout(DEADLINE, read_frame(stream, MAX_FRAME)).await.unwrap().unwrap();
    decode::<edit::Receipt>(&bytes).unwrap().edit
}

#[tokio::test]
async fn edit_waits_use_separate_slots_until_document_close() {
    let endpoint = Endpoint::for_service(&format!("edits-test-{}", std::process::id())).unwrap();
    let listener = Listener::bind(&endpoint).unwrap();
    let (opened, mut documents) = tokio::sync::mpsc::unbounded_channel();
    let service = tokio::spawn(serve(listener, State::default(), move |pending| {
        let opened = opened.clone();
        async move { opened.send(pending.opened().ok().unwrap()).ok().unwrap(); }
    }));
    let mut invalid = timeout(DEADLINE, endpoint.connect()).await.unwrap().unwrap();
    write_frame(&mut invalid, br#"{"method":"edit","path":"relative","wait":true}"#, MAX_FRAME).await.unwrap();
    assert_eq!(edit_response(&mut invalid).await, edit::Response::InvalidRequest);
    assert!(documents.try_recv().is_err());

    let request = serde_json::to_vec(&serde_json::json!({
        "method": "edit", "path": std::env::temp_dir().join("한글.txt"), "wait": true,
    })).unwrap();
    let mut clients = Vec::new();
    for _ in 0..32 {
        let mut client = timeout(DEADLINE, endpoint.connect()).await.unwrap().unwrap();
        write_frame(&mut client, &request, MAX_FRAME).await.unwrap();
        assert_eq!(edit_response(&mut client).await, edit::Response::Accepted);
        assert_eq!(edit_response(&mut client).await, edit::Response::Opened);
        clients.push((client, documents.try_recv().unwrap()));
    }
    assert!(matches!(exchange(&endpoint, &Outbound::List).await, Ok(Receipt::Listed { .. })));

    let mut overflow = timeout(DEADLINE, endpoint.connect()).await.unwrap().unwrap();
    write_frame(&mut overflow, &request, MAX_FRAME).await.unwrap();
    assert_eq!(edit_response(&mut overflow).await, edit::Response::OpenFailed);

    let (mut client, document) = clients.pop().unwrap();
    document.closed();
    assert_eq!(edit_response(&mut client).await, edit::Response::Closed);
    // 연결이 닫히기 전에 전용 슬롯이 반환된다.
    assert!(timeout(DEADLINE, client.read_u8()).await.unwrap().is_err());

    let mut next = timeout(DEADLINE, endpoint.connect()).await.unwrap().unwrap();
    write_frame(&mut next, &request, MAX_FRAME).await.unwrap();
    assert_eq!(edit_response(&mut next).await, edit::Response::Accepted);
    assert_eq!(edit_response(&mut next).await, edit::Response::Opened);
    service.abort();
}
