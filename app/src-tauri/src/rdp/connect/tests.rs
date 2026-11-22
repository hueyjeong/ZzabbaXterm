use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use ironrdp::{
    acceptor::{Acceptor, BeginResult, accept_begin, accept_credssp, accept_finalize},
    core::{decode, encode_vec},
    pdu::{
        geometry::InclusiveRectangle,
        input::fast_path::{FastPathInput, FastPathInputEvent, KeyboardFlags},
        nego::{ConnectionConfirm, ConnectionRequest, ResponseFlags},
        x224::X224,
    },
};
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair, date_time_ymd};
use tokio::{io::AsyncWriteExt as _, net::TcpListener, time::timeout};
use tokio_rustls::{TlsAcceptor, rustls::pki_types::PrivatePkcs8KeyDer};

use super::*;
use crate::rdp::ipc::Slot;

#[test]
fn credssp_does_not_resume_write_or_flush_after_revocation() {
    use std::{cell::Cell, io::{self, Read as _, Write as _}, net, task::{Context, Waker}};

    struct Writer<'a> {
        socket: net::TcpStream,
        blocked: &'a Cell<bool>,
        limit: usize,
        pending: Vec<u8>,
        writes: usize,
        flushes: usize,
    }

    impl AsyncWrite for Writer<'_> {
        fn poll_write(mut self: Pin<&mut Self>, _: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
            self.writes += 1;
            let count = if self.blocked.get() { self.limit.min(bytes.len()) } else { bytes.len() };
            if count == 0 { return Poll::Pending; }
            self.limit = self.limit.saturating_sub(count);
            if count == bytes.len() && self.blocked.get() {
                self.pending.extend_from_slice(bytes);
            } else {
                self.socket.write_all(&bytes[..count])?;
            }
            Poll::Ready(Ok(count))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            this.flushes += 1;
            if this.blocked.get() { return Poll::Pending; }
            Poll::Ready(this.socket.write_all(&this.pending))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            panic!("철회된 TLS 송신에서 shutdown 호출")
        }
    }

    for (routed, close_slot) in [(true, false), (true, true), (false, true)] {
        for limit in [0, 1, 3] {
            let (tabs, lease) = crate::tunnels::ipc::rdp_test_lease();
            let slot = Slot::new(1);
            let auth = AuthIo::new(&slot, routed.then_some(&lease));
            let listener = net::TcpListener::bind((net::Ipv4Addr::LOCALHOST, 0)).unwrap();
            let socket = net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            socket.set_nonblocking(true).unwrap();
            let (mut peer, _) = listener.accept().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            let blocked = Cell::new(true);
            let mut writer = Writer { socket, blocked: &blocked, limit, pending: Vec::new(), writes: 0, flushes: 0 };
            let mut cx = Context::from_waker(Waker::noop());
            {
                let mut send = std::pin::pin!(write_credssp(&mut writer, b"abc", &auth));
                assert!(send.as_mut().poll(&mut cx).is_pending());
                let expected = if close_slot {
                    slot.close();
                    assert_eq!(lease.while_live(|| ()), Some(()));
                    Error::Closed
                } else {
                    tabs.navigate(false);
                    Error::Transport
                };
                blocked.set(false);
                assert_eq!(send.as_mut().poll(&mut cx), Poll::Ready(Err(expected)));
            }
            assert_eq!(writer.writes, if limit == 1 { 2 } else { 1 });
            assert_eq!(writer.flushes, usize::from(limit == 3));
            drop(writer);
            let mut received = Vec::new();
            peer.read_to_end(&mut received).unwrap();
            let expected: &[u8] = if limit == 1 { b"a" } else { b"" };
            assert_eq!(received.as_slice(), expected);
        }
    }
}

const REMOTE: &str = "rdp.example.test";

fn root() -> CertifiedIssuer<'static, KeyPair> {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap()
}

fn listener() -> TcpListener {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    TcpListener::from_std(listener).unwrap()
}

async fn fixture_target(listener: &TcpListener, size: DesktopSize) -> Target {
    let stream = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
    Target { endpoint: Endpoint::Forwarded(stream), server_name: REMOTE.into(), size }
}

fn fixture_policy(_: &str, _: &[rustls::pki_types::CertificateDer<'_>]) -> Result<(), Error> {
    Ok(())
}

async fn exercise(
    listener: &TcpListener,
    protocol: SecurityProtocol,
    forwarded: bool,
    dns: &str,
    expired: bool,
    known: bool,
    policy: TestPeerPolicy,
) -> (Error, usize, bool) {
    let ca = root();
    let unknown = root();
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec![dns.to_owned()]).unwrap();
    if expired {
        params.not_before = date_time_ymd(2000, 1, 1);
        params.not_after = date_time_ymd(2001, 1, 1);
    }
    let cert = params
        .signed_by(&key, if known { &ca } else { &unknown })
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.der().clone()).unwrap();
    let tls_server = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.der().clone(), if known { ca.der() } else { unknown.der() }.clone()],
        PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
    )
    .unwrap();
    let server = async {
        let (socket, _) = listener.accept().await.unwrap();
        let mut request = TokioFramed::new(socket);
        let (_, bytes) = request.read_pdu().await.unwrap();
        let request_pdu = decode::<X224<ConnectionRequest>>(&bytes).unwrap().0;
        let offered = SecurityProtocol::HYBRID | SecurityProtocol::HYBRID_EX;
        assert_eq!(request_pdu.protocol, offered);
        assert!(request_pdu.nego_data.is_none());
        let mut socket = request.into_inner();
        assert!(socket.1.is_empty());
        let pdu = X224(ConnectionConfirm::Response {
            flags: ResponseFlags::empty(),
            protocol,
        });
        socket
            .0
            .write_all(&encode_vec(&pdu).unwrap())
            .await
            .unwrap();
        if protocol.is_empty() || !offered.contains(protocol) || protocol.bits().count_ones() != 1 {
            assert_eq!(socket.0.read(&mut [0u8; 1]).await.unwrap(), 0);
            return false;
        }
        let Ok(mut tls) = TlsAcceptor::from(Arc::new(tls_server))
            .accept(socket.0)
            .await
        else {
            return false;
        };
        let mut first = [0u8; 1];
        // Synthetic credentials can emit their first CredSSP TSRequest only on the accepted TLS connection.
        tls.read_exact(&mut first).await.is_ok() && first[0] == 0x30
    };
    let calls = AtomicUsize::new(0);
    let size = DesktopSize { width: 800, height: 600 };
    let target = if forwarded {
        fixture_target(listener, size).await
    } else {
        Target {
            endpoint: Endpoint::Direct(listener.local_addr().unwrap().port()),
            server_name: "localhost".into(),
            size,
        }
    };
    let name = ServerName::try_from(target.server_name.clone()).unwrap();
    let slot = Slot::new(1);
    let client = connect(
        target,
        name,
        tls_config(roots),
        || async {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(Account {
                username: "fixture".into(),
                domain: None,
                password: zeroize::Zeroizing::new("fixture-only".into()),
            })
        },
        ironrdp::displaycontrol::client::DisplayControlClient::new(|_| Ok(Vec::new())),
        AuthIo::new(&slot, None),
    );
    let client = TEST_PEER_POLICY.scope(policy, client);
    let (result, saw_nla) = timeout(Duration::from_secs(5), async {
        tokio::join!(client, server)
    })
    .await
    .unwrap_or_else(|_| panic!("fixture did not finish for {protocol:?} {dns}"));
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("fixture stops before session activation"),
    };
    (error, calls.load(Ordering::Relaxed), saw_nla)
}

#[tokio::test]
async fn verifies_remote_name_chain_and_expiry_before_credentials() {
    let listener = listener();
    for (name, expired, known) in [
        ("wrong.example.test", false, true),
        (REMOTE, true, true),
        (REMOTE, false, false),
    ] {
        assert_eq!(
            exercise(
                &listener,
                SecurityProtocol::HYBRID,
                true,
                name,
                expired,
                known,
                fixture_policy,
            )
            .await,
            (Error::TlsIdentity, 0, false)
        );
    }
    for protocol in [SecurityProtocol::HYBRID, SecurityProtocol::HYBRID_EX] {
        assert_eq!(
            exercise(&listener, protocol, true, REMOTE, false, true, fixture_policy).await,
            (Error::Authentication, 1, true)
        );
    }
}

#[tokio::test]
async fn direct_connection_verifies_the_dns_name_before_credentials() {
    let listener = listener();
    assert_eq!(
        exercise(&listener, SecurityProtocol::HYBRID, false, REMOTE, false, true, fixture_policy).await,
        (Error::TlsIdentity, 0, false)
    );
    assert_eq!(
        exercise(&listener, SecurityProtocol::HYBRID, false, "localhost", false, true, fixture_policy).await,
        (Error::Authentication, 1, true)
    );
}

#[tokio::test]
async fn rejects_peer_policy_before_credentials_on_both_routes() {
    static POLICY_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn deny_remote(name: &str, peer: &[rustls::pki_types::CertificateDer<'_>]) -> Result<(), Error> {
        assert_eq!(name, REMOTE);
        assert_eq!(peer.len(), 2);
        POLICY_CALLS.fetch_add(1, Ordering::Relaxed);
        Err(Error::TlsIdentity)
    }
    fn deny_localhost(name: &str, peer: &[rustls::pki_types::CertificateDer<'_>]) -> Result<(), Error> {
        assert_eq!(name, "localhost");
        assert_eq!(peer.len(), 2);
        POLICY_CALLS.fetch_add(1, Ordering::Relaxed);
        Err(Error::TlsIdentity)
    }
    let listener = listener();
    for (forwarded, name, policy) in [
        (true, REMOTE, deny_remote as TestPeerPolicy),
        (false, "localhost", deny_localhost),
    ] {
        assert_eq!(
            exercise(&listener, SecurityProtocol::HYBRID, forwarded, name, false, true, policy).await,
            (Error::TlsIdentity, 0, false)
        );
    }
    assert_eq!(POLICY_CALLS.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn rejects_tls_only_and_legacy_modes_without_resolving_credentials() {
    let listener = listener();
    for protocol in [
        SecurityProtocol::SSL,
        SecurityProtocol::empty(),
        SecurityProtocol::SSL | SecurityProtocol::HYBRID,
    ] {
        assert_eq!(
            exercise(&listener, protocol, true, REMOTE, false, true, fixture_policy).await,
            (Error::Negotiation, 0, false)
        );
        assert!(
            timeout(Duration::from_millis(25), listener.accept())
                .await
                .is_err()
        );
    }
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn os_roots_ignore_ssl_cert_environment() {
    const CHILD: &str = "ZZABBA_RDP_ROOT_TEST_CHILD";
    if let Some(path) = std::env::var_os(CHILD) {
        let der = rustls::pki_types::CertificateDer::from(std::fs::read(path).unwrap());
        let mut approved = rustls::RootCertStore::empty();
        approved.add(der).unwrap();
        let injected = &approved.roots[0];
        let native = trusted_roots(rustls::RootCertStore::empty()).unwrap();
        assert!(!native.roots.iter().any(|root| root == injected));
        assert!(
            trusted_roots(approved.clone())
                .unwrap()
                .roots
                .iter()
                .any(|root| root == injected)
        );
        return;
    }
    let fixture = root();
    let dir = std::env::temp_dir().join(format!("zzabba-rdp-roots-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let der = dir.join("fixture.der");
    let pem = dir.join("fixture.pem");
    std::fs::write(&der, fixture.der()).unwrap();
    std::fs::write(&pem, fixture.pem()).unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "rdp::connect::tests::os_roots_ignore_ssl_cert_environment",
        ])
        .env(CHILD, &der)
        .env("SSL_CERT_FILE", &pem)
        .env("SSL_CERT_DIR", &dir)
        .output()
        .unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

struct NoNetwork;

impl ironrdp_tokio::NetworkClient for NoNetwork {
    async fn send(
        &mut self,
        _: &connector::sspi::generator::NetworkRequest,
    ) -> connector::ConnectorResult<Vec<u8>> {
        panic!("fixture NTLM must use only loopback")
    }
}

#[tokio::test]
async fn activates_and_delivers_pixels_and_key_input_only_for_the_authenticated_session() {
    use crate::rdp::{Error, Event, Input, SLOTS, Session, SessionId, frame::tests::bitmap_frame};
    use ironrdp::pdu::rdp::{
        capability_sets::{Bitmap, BitmapDrawingFlags, CapabilitySet},
        client_info::Credentials,
    };

    let listener = listener();
    let tcp_addr = listener.local_addr().unwrap();
    let ca = root();
    let key = KeyPair::generate().unwrap();
    let cert = CertificateParams::new(vec![REMOTE.into()])
        .unwrap()
        .signed_by(&key, &ca)
        .unwrap();
    let parsed = Certificate::from_der(cert.der().as_ref()).unwrap();
    let public_key = parsed
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .unwrap();
    let tls_server = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        )
        .unwrap(),
    );
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.der().clone()).unwrap();
    let tls = tls_config(roots);
    let size = DesktopSize {
        width: 800,
        height: 600,
    };
    let invalid_sent = tokio::sync::Notify::new();
    let first_checked = tokio::sync::Notify::new();
    let frames_sent = tokio::sync::Notify::new();
    for password in ["fixture-only", "fixture-wrong"] {
        let server = async {
            let (socket, _) = listener.accept().await.unwrap();
            let mut acceptor = Acceptor::new(
                SecurityProtocol::HYBRID_EX,
                size,
                vec![CapabilitySet::Bitmap(Bitmap {
                    pref_bits_per_pix: 32,
                    desktop_width: size.width,
                    desktop_height: size.height,
                    desktop_resize_flag: true,
                    drawing_flags: BitmapDrawingFlags::empty(),
                })],
                Some(Credentials {
                    username: "fixture".into(),
                    password: "fixture-only".into(),
                    domain: None,
                }),
            );
            let BeginResult::ShouldUpgrade(socket) =
                accept_begin(TokioFramed::new(socket), &mut acceptor)
                    .await
                    .unwrap()
            else {
                panic!("NLA requires TLS")
            };
            let socket = TlsAcceptor::from(Arc::clone(&tls_server))
                .accept(socket)
                .await
                .unwrap();
            acceptor.mark_security_upgrade_as_done();
            let mut framed = TokioFramed::new(socket);
            if accept_credssp(
                &mut framed,
                &mut acceptor,
                &mut NoNetwork,
                REMOTE.into(),
                public_key.to_vec(),
                None,
            )
            .await
            .is_err()
            {
                return false;
            }
            let (mut framed, result) = accept_finalize(framed, &mut acceptor).await.unwrap();
            assert!(result.credentials.is_none());
            framed
                .write_all(&crate::rdp::frame::tests::marker_frame(false))
                .await
                .unwrap();
            let dirty = InclusiveRectangle {
                left: 1,
                top: 0,
                right: 1,
                bottom: 1,
            };
            let invalid = crate::rdp::frame::tests::corrupt_bitmap_frame(dirty.clone());
            framed.write_all(&invalid).await.unwrap();
            invalid_sent.notify_one();
            first_checked.notified().await;
            let frame = bitmap_frame(dirty);
            framed.write_all(&frame).await.unwrap();
            framed.write_all(&frame).await.unwrap();
            let marked_frame = crate::rdp::frame::tests::marked_bitmap_frame();
            framed.write_all(&marked_frame).await.unwrap();
            let (action, _) = framed.read_pdu().await.unwrap();
            assert_eq!(action, ironrdp::pdu::Action::X224);
            frames_sent.notify_one();
            let (_, pdu) = framed.read_pdu().await.unwrap();
            assert_eq!(
                decode::<FastPathInput>(&pdu).unwrap().input_events(),
                &[FastPathInputEvent::KeyboardEvent(
                    KeyboardFlags::RELEASE,
                    0x1e
                )]
            );
            let mut stream = framed.into_inner().0;
            assert_eq!(
                stream.read(&mut [0u8; 1]).await.unwrap_err().kind(),
                std::io::ErrorKind::UnexpectedEof
            );
            true
        };
        let client = async {
            let slot = Slot::new(1);
            let mut session = TEST_PEER_POLICY.scope(fixture_policy, Session::open_with_tls(
                fixture_target(&listener, size).await,
                REMOTE.try_into().unwrap(),
                Arc::clone(&tls),
                || async {
                    Ok(Account {
                        username: "fixture".into(),
                        domain: None,
                        password: zeroize::Zeroizing::new(password.into()),
                    })
                },
                SLOTS.try_acquire().unwrap(),
                AuthIo::new(&slot, None),
            ))
            .await?;
            let id = session.id();
            let input = session.input_handle();
            invalid_sent.notified().await;
            assert!(
                timeout(Duration::from_millis(50), async {
                    loop {
                        match session.next().await {
                            Some(Event::Channels(_)) => {}
                            Some(_) => panic!("non-pixel update published a frame"),
                            None => panic!("session stopped before first pixels"),
                        }
                    }
                })
                .await
                .is_err()
            );
            let key = || Input::ScanCode {
                code: 0x1e,
                extended: false,
                released: true,
            };
            assert_eq!(input.input(id, key()).await, Err(Error::Busy));
            first_checked.notify_one();
            let Event::Frame(frame) = session.next().await.ok_or(Error::Closed)? else {
                panic!("first display event must come from server pixels")
            };
            assert_eq!((frame.session, frame.width, frame.height), (id, 800, 600));
            assert_eq!(&frame.rgba[4..8], &[255, 0, 0, 255]);
            // The marked third frame is processed with the second still in the event queue.
            frames_sent.notified().await;
            assert_eq!(input.input(SessionId(0), key()).await, Err(Error::Closed));
            assert_eq!(
                session
                    .input(
                        id,
                        Input::Pointer {
                            x: 800,
                            y: 0,
                            action: crate::rdp::Pointer::Move
                        }
                    )
                    .await,
                Err(Error::Bounds)
            );
            input.input(id, key()).await?;
            session.close().await;
            Ok(())
        };
        let (result, server_activated) = timeout(Duration::from_secs(5), async {
            tokio::join!(client, server)
        })
        .await
        .unwrap();
        if password != "fixture-wrong" {
            result.unwrap();
            assert!(server_activated);
        } else {
            assert!(matches!(result, Err(Error::Authentication)));
            assert!(!server_activated);
        }
    }
}
