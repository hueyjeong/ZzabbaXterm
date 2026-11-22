use super::*;
use std::{future::Future, pin::Pin, sync::atomic::AtomicUsize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use zeroize::Zeroizing;

const VERSION: &[u8] = b"RFB 003.008\n";
const OLD: Size = Size {
    width: 2,
    height: 1,
};
const NEW: Size = Size {
    width: 3,
    height: 1,
};

struct Fixture {
    target: Target,
    listener: TcpListener,
    lookups: AtomicUsize,
}

impl Fixture {
    async fn new() -> Self {
        Self {
            target: Target {
                profile_id: "profile".into(),
                host: "localhost".into(),
                port: 5900,
            },
            listener: TcpListener::bind("127.0.0.1:0").await.unwrap(),
            lookups: AtomicUsize::new(0),
        }
    }

    fn route(&self, owner: &'static str) -> Route<'_> {
        Route::Fixture {
            registry: self,
            owner,
            tunnel_id: "vnc-tunnel",
        }
    }
}

impl TunnelRegistry for Fixture {
    fn open_verified<'a>(
        &'a self,
        owner: &'a str,
        id: &'a str,
        _: &'a Target,
    ) -> Pin<Box<dyn Future<Output = Result<VerifiedTunnel, Error>> + Send + 'a>> {
        Box::pin(async move {
            if owner != "viewer" || id != "vnc-tunnel" {
                return Err(Error::UnverifiedRoute);
            }
            Ok(VerifiedTunnel {
                target: self.target.clone(),
                stream: TcpStream::connect(self.listener.local_addr()?).await?,
            })
        })
    }
}

impl Secrets for Fixture {
    async fn vnc_password(&self, target: &Target) -> Option<Zeroizing<Vec<u8>>> {
        assert_eq!(target, &self.target);
        self.lookups.fetch_add(1, Ordering::Relaxed);
        Some(Zeroizing::new(b"password".to_vec()))
    }

    fn while_live(&self, write: &mut dyn FnMut()) -> bool { write(); true }
}

async fn expect<const N: usize>(stream: &mut (impl AsyncRead + Unpin), bytes: [u8; N]) {
    let mut actual = [0; N];
    stream.read_exact(&mut actual).await.unwrap();
    assert_eq!(actual, bytes);
}

async fn offer(stream: &mut (impl AsyncRead + AsyncWrite + Unpin), types: &[u8]) {
    stream.write_all(VERSION).await.unwrap();
    expect(stream, *b"RFB 003.008\n").await;
    stream.write_u8(types.len() as u8).await.unwrap();
    stream.write_all(types).await.unwrap();
}

async fn init(stream: &mut (impl AsyncRead + AsyncWrite + Unpin), size: Size) {
    stream.write_u32(0).await.unwrap();
    expect(stream, [1]).await;
    stream.write_u16(size.width).await.unwrap();
    stream.write_u16(size.height).await.unwrap();
    stream.write_all(&[0; 20]).await.unwrap();
    expect(
        stream,
        [
            0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0,
        ],
    )
    .await;
    expect(
        stream,
        [
            2, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, 0, 255, 255, 255, 33, 255, 255, 254, 204,
        ],
    )
    .await;
    request(stream, size, false).await;
}

async fn request(stream: &mut (impl AsyncRead + Unpin), size: Size, incremental: bool) {
    let [a, b] = size.width.to_be_bytes();
    let [c, d] = size.height.to_be_bytes();
    expect(stream, [3, u8::from(incremental), 0, 0, 0, 0, a, b, c, d]).await;
}

async fn header(stream: &mut (impl AsyncWrite + Unpin), count: u16) {
    stream.write_all(&[0, 0]).await.unwrap();
    stream.write_u16(count).await.unwrap();
}

async fn rect(stream: &mut (impl AsyncWrite + Unpin), x: u16, y: u16, size: Size, encoding: i32) {
    for field in [x, y, size.width, size.height] {
        stream.write_u16(field).await.unwrap();
    }
    stream.write_i32(encoding).await.unwrap();
}

async fn layout(stream: &mut (impl AsyncWrite + Unpin), reason: u16, status: u16, size: Size) {
    header(stream, 1).await;
    layout_rect(stream, reason, status, size).await;
}

async fn layout_rect(stream: &mut (impl AsyncWrite + Unpin), reason: u16, status: u16, size: Size) {
    rect(stream, reason, status, size, -308).await;
    stream.write_all(&[1, 0, 0, 0]).await.unwrap();
    stream.write_u32(7).await.unwrap();
    for field in [0, 0, size.width, size.height] {
        stream.write_u16(field).await.unwrap();
    }
    stream.write_u32(0x80).await.unwrap();
}

async fn frame(stream: &mut (impl AsyncWrite + Unpin), size: Size) {
    header(stream, 1).await;
    rect(stream, 0, 0, size, 0).await;
    for _ in 0..size.width {
        stream.write_all(&[11, 22, 33, 0]).await.unwrap();
    }
}

async fn next(tab: &mut Tab, id: SessionId) -> Event {
    timeout(Duration::from_secs(2), tab.next(id))
        .await
        .unwrap()
        .unwrap()
}

fn cut(text: &str) -> Input {
    Input::CutText(text.into())
}
fn dimensions(size: Size) -> Event {
    Event::Frame(vec![Change::Size(size)])
}

#[tokio::test]
async fn vnc_rejects_unverified_tunnel_wrong_target_and_plain_direct_before_secret() {
    let fixture = Fixture::new().await;
    let mut tab = Tab::default();
    let auth = Auth::VncPassword;
    assert_eq!(
        tab.open(&fixture.target, fixture.route("other"), auth, &fixture)
            .await,
        Err(Error::UnverifiedRoute)
    );
    let mut wrong = fixture.target.clone();
    for change in 0..3 {
        match change {
            0 => wrong.profile_id = "other".into(),
            1 => wrong.host = "other.test".into(),
            _ => wrong.port = 5901,
        }
        assert_eq!(
            tab.open(&wrong, fixture.route("viewer"), auth, &fixture)
                .await,
            Err(Error::UnverifiedRoute)
        );
        wrong = fixture.target.clone();
    }
    let direct = Target {
        host: "127.0.0.1".into(),
        port: fixture.listener.local_addr().unwrap().port(),
        ..fixture.target.clone()
    };
    for _ in 0..3 {
        drop(fixture.listener.accept().await.unwrap());
    }
    let server = async {
        let (mut stream, _) = fixture.listener.accept().await.unwrap();
        stream.write_all(VERSION).await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), 22); // TLS handshake, not RFB
        stream.shutdown().await.unwrap();
    };
    let ((), result) = tokio::join!(
        server,
        tab.open(
            &direct,
            Route::DirectTls {
                trust_anchor_pem: None
            },
            auth,
            &fixture
        )
    );
    assert_eq!(result, Err(Error::TlsIdentity));
    assert_eq!(fixture.lookups.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn vnc_direct_tls_requires_matching_certificate_name_before_rfb() {
    use tokio_native_tls::{TlsAcceptor, native_tls};

    let fixture = Fixture::new().await;
    let identity = native_tls::Identity::from_pkcs8(
        include_bytes!("fixture-localhost.pem"),
        include_bytes!("fixture-localhost-key.pem"),
    )
    .unwrap();
    let acceptor = TlsAcceptor::from(native_tls::TlsAcceptor::new(identity).unwrap());
    let route = || Route::DirectTls {
        trust_anchor_pem: Some(include_bytes!("fixture-ca.pem")),
    };
    let mut tab = Tab::default();
    for (host, valid) in [("127.0.0.1", false), ("localhost", true)] {
        let target = Target {
            host: host.into(),
            port: fixture.listener.local_addr().unwrap().port(),
            ..fixture.target.clone()
        };
        let server = async {
            let (tcp, _) = fixture.listener.accept().await.unwrap();
            let tls = acceptor.accept(tcp).await;
            if valid {
                let mut stream = tls.unwrap();
                offer(&mut stream, &[1]).await;
                expect(&mut stream, [1]).await;
                init(&mut stream, OLD).await;
            } else if let Ok(mut stream) = tls {
                assert_eq!(stream.read(&mut [0]).await.unwrap_or(0), 0);
            }
        };
        let auth = if valid {
            Auth::ExplicitNone
        } else {
            Auth::VncPassword
        };
        let ((), result) = tokio::join!(server, tab.open(&target, route(), auth, &fixture));
        if valid {
            assert_eq!(next(&mut tab, result.unwrap()).await, dimensions(OLD));
        } else {
            assert_eq!(result, Err(Error::TlsIdentity));
        }
        assert_eq!(fixture.lookups.load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn vnc_none_offer_cannot_downgrade_password_authentication() {
    let fixture = Fixture::new().await;
    let mut tab = Tab::default();
    let server = async {
        let (mut stream, _) = fixture.listener.accept().await.unwrap();
        offer(&mut stream, &[1]).await;
        assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
    };
    let ((), result) = tokio::join!(
        server,
        tab.open(
            &fixture.target,
            fixture.route("viewer"),
            Auth::VncPassword,
            &fixture
        )
    );
    assert_eq!(result, Err(Error::SecurityType));
    assert_eq!(fixture.lookups.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn vnc_server_changes_and_resize_ack_are_ordered_and_session_ids_expire() {
    let mut fixture = Fixture::new().await;
    fixture.target.port = fixture.listener.local_addr().unwrap().port();
    let identity = tokio_native_tls::native_tls::Identity::from_pkcs8(
        include_bytes!("fixture-localhost.pem"), include_bytes!("fixture-localhost-key.pem"),
    ).unwrap();
    let tls = tokio_native_tls::TlsAcceptor::from(tokio_native_tls::native_tls::TlsAcceptor::new(identity).unwrap());
    let route = || Route::DirectTls { trust_anchor_pem: Some(include_bytes!("fixture-ca.pem")) };
    let mut tab = Tab::default();
    let (initial_checked, initial_check) = tokio::sync::oneshot::channel();
    let (resize_checked, resize_check) = tokio::sync::oneshot::channel();
    let server = async {
        let (stream, _) = fixture.listener.accept().await.unwrap();
        let mut stream = tls.accept(stream).await.unwrap();
        offer(&mut stream, &[1, 2]).await;
        expect(&mut stream, [2]).await;
        stream.write_all(&[0; 16]).await.unwrap();
        expect(
            &mut stream,
            [
                0xff, 0x97, 0x50, 0x2e, 0x94, 0x22, 0xf0, 0x89, 0xff, 0x97, 0x50, 0x2e, 0x94, 0x22,
                0xf0, 0x89,
            ],
        )
        .await;
        init(&mut stream, OLD).await;
        layout(&mut stream, 0, 0, OLD).await;
        request(&mut stream, OLD, true).await;
        initial_check.await.unwrap();
        frame(&mut stream, OLD).await;
        request(&mut stream, OLD, true).await;
        expect(
            &mut stream,
            [4, 1, 0, 0, 0, 0, 0, 97, 4, 0, 0, 0, 0, 0, 0, 97],
        )
        .await;
        expect(&mut stream, [5, 1, 0, 1, 0, 0]).await;
        expect(&mut stream, [6, 0, 0, 0, 0, 0, 0, 1, 0xe9]).await;
        stream
            .write_all(&[3, 0, 0, 0, 0, 0, 0, 2, b'Z', 0xf1])
            .await
            .unwrap();
        header(&mut stream, 1).await;
        rect(
            &mut stream,
            1,
            0,
            Size {
                width: 1,
                height: 1,
            },
            1,
        )
        .await;
        stream.write_u32(0).await.unwrap();
        request(&mut stream, OLD, true).await;
        let resize = [
            251, 0, 0, 3, 0, 1, 1, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 3, 0, 1, 0, 0, 0, 0x80,
        ];
        expect(&mut stream, resize).await;
        layout(&mut stream, 2, 0, NEW).await;
        request(&mut stream, OLD, true).await;
        layout(
            &mut stream,
            1,
            4,
            Size {
                width: 0,
                height: 0,
            },
        )
        .await;
        request(&mut stream, OLD, true).await;
        expect(&mut stream, resize).await;
        layout(&mut stream, 1, 0, NEW).await;
        expect(&mut stream, [5, 0, 0, 1, 0, 0]).await;
        request(&mut stream, NEW, false).await;
        resize_check.await.unwrap();
        frame(&mut stream, NEW).await;
        request(&mut stream, NEW, true).await;
        expect(&mut stream, [4, 1, 0, 0, 0, 0, 0, 98]).await;
        layout(&mut stream, 0, 0, OLD).await;
        expect(&mut stream, [4, 0, 0, 0, 0, 0, 0, 98]).await;
        request(&mut stream, OLD, false).await;
        frame(&mut stream, OLD).await;
        request(&mut stream, OLD, true).await;
    };
    let client = async {
        let id = tab
            .open(
                &fixture.target,
                route(),
                Auth::VncPassword,
                &fixture,
            )
            .await
            .unwrap();
        assert_eq!(next(&mut tab, id).await, dimensions(OLD));
        assert_input_waits_for_frame(&tab, id);
        initial_checked.send(()).unwrap();
        assert_eq!(
            next(&mut tab, id).await,
            Event::Frame(vec![
                Change::Pixels {
                    rect: Rect {
                        x: 0,
                        y: 0,
                        size: OLD
                    },
                    rgba: vec![11, 22, 33, 255, 11, 22, 33, 255]
                },
                Change::Ready,
            ])
        );
        for down in [true, false] {
            tab.input(id, Input::Key { keysym: 97, down }).unwrap();
        }
        tab.input(
            id,
            Input::Pointer {
                x: 1,
                y: 0,
                buttons: 1,
            },
        )
        .unwrap();
        tab.input(id, cut("é")).unwrap();
        let first = next(&mut tab, id).await;
        let second = next(&mut tab, id).await;
        let (clipboard, copy) = if matches!(first, Event::CutText(_)) { (first, second) } else { (second, first) };
        assert_eq!(clipboard, Event::CutText("Zñ".into()));
        let cell = Size {
            width: 1,
            height: 1,
        };
        assert_eq!(
            copy,
            Event::Frame(vec![Change::Copy {
                source: Rect {
                    x: 0,
                    y: 0,
                    size: cell
                },
                destination: Rect {
                    x: 1,
                    y: 0,
                    size: cell
                },
            }])
        );
        tab.input(id, Input::Resize(NEW)).unwrap();
        assert_eq!(
            next(&mut tab, id).await,
            Event::Resize(ResizeResult::Rejected(4))
        );
        tab.input(id, Input::Resize(NEW)).unwrap();
        assert_eq!(next(&mut tab, id).await, dimensions(NEW));
        assert_eq!(
            next(&mut tab, id).await,
            Event::Resize(ResizeResult::Applied(NEW))
        );
        assert_input_waits_for_frame(&tab, id);
        resize_checked.send(()).unwrap();
        assert!(
            matches!(next(&mut tab, id).await, Event::Frame(changes) if changes.last() == Some(&Change::Ready))
        );
        tab.input(id, Input::Key { keysym: 98, down: true }).unwrap();
        assert_eq!(next(&mut tab, id).await, dimensions(OLD));
        assert!(matches!(next(&mut tab, id).await, Event::Frame(changes) if changes.last() == Some(&Change::Ready)));
        assert!(matches!(next(&mut tab, id).await, Event::Closed(_)));
        assert_eq!(tab.input(id, cut("old")), Err(Error::Disconnected));
        id
    };
    let ((), old) = timeout(Duration::from_secs(5), async {
        tokio::join!(server, client)
    })
    .await
    .unwrap();
    let server = async {
        let (mut stream, _) = fixture.listener.accept().await.unwrap();
        offer(&mut stream, &[1]).await;
        expect(&mut stream, [1]).await;
        init(&mut stream, OLD).await;
    };
    let ((), reopened) = tokio::join!(
        server,
        tab.open(
            &fixture.target,
            fixture.route("viewer"),
            Auth::ExplicitNone,
            &fixture
        )
    );
    let id = reopened.unwrap();
    assert_ne!(old, id);
    assert_eq!(tab.next(old).await, Err(Error::Disconnected));
    assert_eq!(tab.input(old, cut("old")), Err(Error::Disconnected));
    assert_eq!(next(&mut tab, id).await, dimensions(OLD));
    assert_eq!(fixture.lookups.load(Ordering::Relaxed), 1);
}

fn assert_input_waits_for_frame(tab: &Tab, id: SessionId) {
    for input in [
        Input::Key { keysym: 120, down: true },
        Input::Pointer {
            x: 0,
            y: 0,
            buttons: 1,
        },
        cut("discard"),
        Input::Resize(NEW),
    ] {
        assert_eq!(tab.input(id, input), Err(Error::ScreenNotReady));
    }
}

#[tokio::test]
async fn vnc_extended_layout_rejects_pixels_before_or_after_every_reason() {
    let pixel = Size {
        width: 1,
        height: 1,
    };
    for reason in [0, 1, 2] {
        for layout_first in [false, true] {
            for encoding in [0, 1] {
                let (mut sender, mut receiver) = tokio::io::duplex(64);
                header(&mut sender, 2).await;
                if layout_first {
                    layout_rect(&mut sender, reason, 0, OLD).await;
                }
                rect(&mut sender, 0, 0, pixel, encoding).await;
                sender.write_all(&[0; 4]).await.unwrap();
                if !layout_first {
                    layout_rect(&mut sender, reason, 0, OLD).await;
                }
                drop(sender);
                assert_eq!(
                    wire::read_message(&mut receiver, OLD).await.err(),
                    Some(Error::InvalidFrame)
                );
            }
        }
    }
}

#[tokio::test]
async fn vnc_malformed_size_copy_and_payload_fail_closed() {
    assert_eq!(
        (Size {
            width: 8193,
            height: 1
        })
        .pixels(),
        Err(Error::InvalidFrame)
    );
    for (encoding, dst, payload, error) in [
        (0, 2, &[][..], Error::InvalidFrame),
        (1, 0, &[0, 2, 0, 0][..], Error::InvalidFrame),
        (
            0,
            0,
            &[1, 2, 3][..],
            Error::Io(io::ErrorKind::UnexpectedEof),
        ),
    ] {
        let (mut sender, mut receiver) = tokio::io::duplex(64);
        header(&mut sender, 1).await;
        rect(
            &mut sender,
            dst,
            0,
            Size {
                width: 1,
                height: 1,
            },
            encoding,
        )
        .await;
        sender.write_all(payload).await.unwrap();
        drop(sender);
        assert_eq!(
            wire::read_message(&mut receiver, OLD).await.err(),
            Some(error)
        );
    }
    let mut frame = Frame::new(OLD).unwrap();
    let mut pixel = Rect {
        x: 0,
        y: 0,
        size: Size {
            width: 1,
            height: 1,
        },
    };
    assert!(!frame.cover(pixel));
    assert!(!frame.cover(pixel));
    pixel.x = 1;
    assert!(frame.cover(pixel));
}

#[tokio::test]
async fn vnc_revoked_after_lookup_writes_no_challenge_response() {
    struct Revoking { life: super::lifetime::Lifetime, reads: AtomicUsize }
    impl Secrets for Revoking {
        async fn vnc_password(&self, _: &Target) -> Option<Zeroizing<Vec<u8>>> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.life.revoke();
            Some(Zeroizing::new(b"isolated".to_vec()))
        }
        fn while_live(&self, write: &mut dyn FnMut()) -> bool { self.life.live(write).is_some() }
    }
    let grant = Revoking { life: Default::default(), reads: AtomicUsize::new(0) };
    let target = Target { profile_id: "fixture".into(), host: "localhost".into(), port: 5900 };
    let (mut client, mut server) = tokio::io::duplex(64);
    let handshake = async {
        let result = wire::handshake(&mut client, &target, Auth::VncPassword, &grant).await;
        drop(client);
        result
    };
    let peer = async {
        offer(&mut server, &[2]).await;
        expect(&mut server, [2]).await;
        server.write_all(&[0; 16]).await.unwrap();
        assert_eq!(server.read(&mut [0; 16]).await.unwrap(), 0);
    };
    let (result, ()) = tokio::join!(handshake, peer);
    assert_eq!(result, Err(Error::Disconnected));
    assert_eq!(grant.reads.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn vnc_revocation_between_response_writes_discards_remainder() {
    struct MidWrite(AtomicUsize);
    impl Secrets for MidWrite {
        async fn vnc_password(&self, _: &Target) -> Option<Zeroizing<Vec<u8>>> {
            Some(Zeroizing::new(b"isolated".to_vec()))
        }
        fn while_live(&self, write: &mut dyn FnMut()) -> bool {
            if self.0.fetch_add(1, Ordering::Relaxed) != 0 { return false; }
            write();
            true
        }
    }
    let grant = MidWrite(AtomicUsize::new(0));
    let target = Target { profile_id: "fixture".into(), host: "127.0.0.1".into(), port: 5900 };
    let (mut client, mut server) = tokio::io::duplex(12);
    let handshake = async {
        let result = wire::handshake(&mut client, &target, Auth::VncPassword, &grant).await;
        drop(client);
        result
    };
    let peer = async {
        offer(&mut server, &[2]).await;
        expect(&mut server, [2]).await;
        server.write_all(&[0; 16]).await.unwrap();
        let mut response = Vec::new();
        server.read_to_end(&mut response).await.unwrap();
        assert_eq!(response.len(), 12);
    };
    let (result, ()) = tokio::join!(handshake, peer);
    assert_eq!(result, Err(Error::Disconnected));
    assert_eq!(grant.0.load(Ordering::Relaxed), 2);
}

#[cfg(windows)]
#[tokio::test]
async fn vnc_windows_password_is_requested_once_after_verified_tls_and_complete_challenge() {
    use tokio_native_tls::{TlsAcceptor, native_tls};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = Target { profile_id: "fixture".into(), host: "localhost".into(), port: listener.local_addr().unwrap().port() };
    let acceptor = TlsAcceptor::from(native_tls::TlsAcceptor::new(native_tls::Identity::from_pkcs8(
        include_bytes!("fixture-localhost.pem"), include_bytes!("fixture-localhost-key.pem"),
    ).unwrap()).unwrap());
    for (offered, len, cancel) in [([1, 2], 16, false), ([1, 2], 16, true), ([1, 1], 0, false), ([1, 2], 15, false)] {
        let prompts = Arc::new(AtomicUsize::new(0));
        let grant = approved::Approved::fixture(target.clone(), Arc::default(), {
            let prompts = prompts.clone();
            move || {
                prompts.fetch_add(1, Ordering::Relaxed);
                (!cancel).then(|| Zeroizing::new(b"password".to_vec()))
            }
        });
        let mut tab = Tab::default();
        let route = || Route::DirectTls { trust_anchor_pem: Some(include_bytes!("fixture-ca.pem")) };
        let peer = async {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(tcp).await.unwrap();
            offer(&mut stream, &offered).await;
            if len != 0 {
                expect(&mut stream, [2]).await;
                assert_eq!(prompts.load(Ordering::Relaxed), 0);
                stream.write_all(&[0; 16][..len]).await.unwrap();
                if len < 16 { stream.shutdown().await.unwrap(); }
                if len == 16 && !cancel {
                    let mut response = [0; 16];
                    stream.read_exact(&mut response).await.unwrap();
                    assert_ne!(response, [0; 16]);
                    assert_ne!(&response[..8], b"password");
                    init(&mut stream, OLD).await;
                    return;
                }
            }
            assert_eq!(stream.read(&mut [0; 16]).await.unwrap_or(0), 0);
        };
        let mut wrong = target.clone();
        wrong.port ^= 1;
        assert!(grant.vnc_password(&wrong).await.is_none());
        let ((), opened) = tokio::join!(peer, tab.open(&target, route(), Auth::VncPassword, &grant));
        match (len, cancel) {
            (16, false) => assert!(opened.is_ok()),
            (16, true) => assert_eq!(opened, Err(Error::MissingSecret)),
            (0, _) => assert_eq!(opened, Err(Error::SecurityType)),
            _ => assert!(matches!(opened, Err(Error::Io(_)))),
        }
        assert_eq!(prompts.load(Ordering::Relaxed), usize::from(len == 16));
        if len == 16 { assert!(grant.vnc_password(&target).await.is_none()); }
    }
}
