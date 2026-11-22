use super::*;
use tokio::io::{DuplexStream, duplex};

fn request(wait: bool) -> Outbound {
    let Outbound::Edit { path, .. } = parse([OsString::from("한글.txt")]).unwrap();
    Outbound::Edit { path, wait }
}

fn body(wait: bool) -> Vec<u8> {
    encode_frame(&request(wait), MAX_FRAME).unwrap()
}

async fn receive(stream: &mut DuplexStream) -> Request {
    #[derive(Deserialize)]
    #[serde(tag = "method", rename_all = "camelCase")]
    enum Inbound {
        Edit(Request),
    }
    let bytes = read_frame(stream, MAX_FRAME).await.unwrap();
    let Inbound::Edit(request) = serde_json::from_slice(&bytes).unwrap();
    assert!(request.is_valid());
    send_edit(stream, Response::Accepted, Instant::now() + DEADLINE)
        .await
        .unwrap();
    request
}

#[test]
fn accepts_one_path_and_resolves_it_without_opening_it() {
    let Outbound::Edit { path, wait } = parse(["--wait", "--", "-한글.txt"].map(OsString::from)).unwrap();
    assert!(path.is_absolute());
    assert_eq!(path.file_name().unwrap(), "-한글.txt");
    assert!(wait);
    assert!(matches!(parse([OsString::from("--wait")]), Err(CliError::Usage)));
    assert!(matches!(parse([OsString::from("a".repeat(MAX_FRAME + 1))]), Err(CliError::RequestTooLarge)));
    assert!(!valid_path(Path::new("relative.txt")));
}

#[cfg(windows)]
#[test]
fn rejects_network_device_and_stream_paths() {
    for path in [r"\\server\share\file.txt", r"\\.\COM1", r"C:\file.txt:stream"] {
        assert!(!valid_path(Path::new(path)), "{path}");
    }
}

async fn open_waiting_tab() -> (
    WaitForClose,
    tokio::task::JoinHandle<Result<(), CliError>>,
    tokio::task::JoinHandle<()>,
) {
    let (mut client, mut server) = duplex(MAX_FRAME);
    let (tab, mounted) = oneshot::channel();
    let server = tokio::spawn(async move {
        let request = receive(&mut server).await;
        serve_edit(&mut server, request, |pending| async move {
            assert!(tab.send(pending.opened().unwrap()).is_ok());
        }).await.unwrap();
    });
    let client = tokio::spawn(async move { exchange(&mut client, &body(true), true).await });
    let exact = timeout(DEADLINE, mounted).await.unwrap().unwrap();
    (exact, client, server)
}

#[tokio::test]
async fn only_the_exact_tab_close_releases_wait() {
    let (exact, mut client, server) = open_waiting_tab().await;
    let (other, other_client, other_server) = open_waiting_tab().await;
    other.closed();
    timeout(DEADLINE, other_client).await.unwrap().unwrap().unwrap();
    timeout(DEADLINE, other_server).await.unwrap().unwrap();
    assert!(timeout(std::time::Duration::ZERO, &mut client).await.is_err());
    exact.closed();
    timeout(DEADLINE, client).await.unwrap().unwrap().unwrap();
    timeout(DEADLINE, server).await.unwrap().unwrap();
}

#[tokio::test]
async fn closing_the_transport_or_dropping_the_tab_is_an_error() {
    for disconnect in [false, true] {
        let (mut client, mut server) = duplex(MAX_FRAME);
        let server = async move {
            let request = receive(&mut server).await;
            if disconnect {
                send_edit(&mut server, Response::Opened, Instant::now() + DEADLINE).await.unwrap();
            } else {
                serve_edit(&mut server, request, |pending| async move {
                    drop(pending.opened().unwrap());
                }).await.unwrap();
            }
        };
        let bytes = body(true);
        let (result, ()) = timeout(DEADLINE, async { tokio::join!(exchange(&mut client, &bytes, true), server) }).await.unwrap();
        if disconnect {
            assert!(matches!(result, Err(CliError::Disconnected)));
        } else {
            assert!(matches!(result, Err(CliError::AppExited)));
        }
    }
}

#[tokio::test]
async fn rejects_close_before_open() {
    let (mut client, mut server) = duplex(MAX_FRAME);
    let server = async move {
        receive(&mut server).await;
        send_edit(&mut server, Response::Closed, Instant::now() + DEADLINE).await.unwrap();
    };
    let bytes = body(true);
    let (result, ()) = timeout(DEADLINE, async { tokio::join!(exchange(&mut client, &bytes, true), server) }).await.unwrap();
    assert!(matches!(result, Err(CliError::InvalidResponse)));
}
