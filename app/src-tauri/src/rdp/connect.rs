use std::{future::poll_fn, pin::Pin, sync::{Arc, Mutex}, task::Poll};

use ironrdp::{
    connector::{
        self, ClientConnector, ClientConnectorState, ConnectionResult, Credentials, DesktopSize,
        Sequence, credssp::CredsspSequence, sspi::generator::GeneratorState,
    },
    core::{WriteBuf, decode},
    pdu::{
        PduHint,
        gcc::KeyboardType,
        nego::{ConnectionConfirm, SecurityProtocol},
        rdp::capability_sets::{BitmapCodecs, MajorPlatformType},
        x224::X224,
    },
};
use ironrdp_tokio::{FramedWrite as _, TokioFramed, bytes::Bytes};
use tokio::{io::{AsyncRead, AsyncReadExt as _, AsyncWrite}, net::TcpStream};
use tokio_rustls::{
    TlsConnector,
    client::TlsStream,
    rustls::{
        self,
        client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        crypto::CryptoProvider,
        pki_types::{CertificateDer, ServerName, UnixTime},
        DigitallySignedStruct, SignatureScheme,
    },
};
use x509_cert::{Certificate, der::Decode as _};

use super::{Account, Error, Target, clipboard::TextClipboard, ipc::AuthIo, route::Endpoint, trust};

pub(super) type Transport = TokioFramed<TlsStream<TcpStream>>;

fn config(size: DesktopSize, scale: u32, tls_only: bool) -> connector::Config {
    connector::Config {
        desktop_size: size,
        // 100~500%가 아니면 0으로 알려 서버 기본 배율을 쓴다.
        desktop_scale_factor: super::display::scale(scale).unwrap_or(0),
        // NLA-first requests HYBRID|HYBRID_EX only, so the server cannot downgrade to TLS.
        // TLS-only requests PROTOCOL_SSL and skips CredSSP; the caller must opt in.
        enable_tls: tls_only,
        enable_credssp: !tls_only,
        // Smart-card variant with no identity emits no pre-TLS username cookie.
        credentials: Credentials::SmartCard {
            pin: String::new(),
            config: None,
        },
        domain: None,
        // xrdp는 빌드 번호 419 이하를 크기를 바꿀 수 없는 옛 클라이언트로 보고 Deactivate All 없이 화면만 바꾼다.
        // Windows 10 22H2(19045)로 알려 Display Control 해상도 변경이 재활성화로 이어지게 한다.
        client_build: 19045,
        client_name: "ZzabbaXterm".into(),
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_functional_keys_count: 12,
        keyboard_layout: 0x0000_0409,
        ime_file_name: String::new(),
        bitmap: Some(connector::BitmapConfig {
            lossy_compression: false,
            color_depth: 32,
            codecs: BitmapCodecs(Vec::new()),
        }),
        dig_product_id: String::new(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".into(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        platform: if cfg!(windows) {
            MajorPlatformType::WINDOWS
        } else if cfg!(target_os = "macos") {
            MajorPlatformType::MACINTOSH
        } else {
            MajorPlatformType::UNIX
        },
        hardware_id: None,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        performance_flags: Default::default(),
        license_cache: None,
        timezone_info: Default::default(),
        compression_type: None,
        enable_server_pointer: false,
        pointer_software_rendering: false,
        multitransport_flags: None,
    }
}

pub(super) fn trusted_roots(
    mut approved: rustls::RootCertStore,
) -> Result<rustls::RootCertStore, Error> {
    #[cfg(windows)]
    if let Ok(store) = schannel::cert_store::CertStoreBuilder::new()
        .read_only()
        .open_current_user("ROOT")
    {
        use schannel::cert_context::ValidUses;
        for cert in store.certs() {
            let tls = match cert.valid_uses() {
                Ok(ValidUses::All) => true,
                Ok(ValidUses::Oids(oids)) => oids.iter().any(|oid| oid == "1.3.6.1.5.5.7.3.1"),
                Err(_) => false,
            };
            if tls && matches!(cert.is_time_valid(), Ok(true)) {
                let _ = approved.add(cert.to_der().to_vec().into());
            }
        }
    }
    #[cfg(target_os = "linux")]
    approved.add_parsable_certificates(
        rustls_native_certs::load_certs_from_paths(
            Some(std::path::Path::new("/etc/ssl/certs/ca-certificates.crt")),
            None,
        )
        .certs,
    );
    if approved.is_empty() {
        return Err(Error::TrustStore);
    }
    Ok(approved)
}

#[derive(Debug)]
struct Capture {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
    seen: Arc<Mutex<Option<CertificateDer<'static>>>>,
}

impl ServerCertVerifier for Capture {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.seen.lock().unwrap() = Some(CertificateDer::from(end_entity.as_ref().to_vec()));
        self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

#[derive(Debug)]
struct Pinned {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
    fingerprint: String,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if trust::fingerprint(end_entity.as_ref()) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// 체인 검증은 rustls에 맡기고, 실패하면 제시된 인증서를 승인·고정에 쓴다.
#[derive(Clone)]
pub(super) struct Tls {
    roots: Arc<rustls::RootCertStore>,
    provider: Arc<CryptoProvider>,
    config: Arc<rustls::ClientConfig>,
    seen: Arc<Mutex<Option<CertificateDer<'static>>>>,
}

impl Tls {
    fn verifier(&self) -> Arc<rustls::client::WebPkiServerVerifier> {
        rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::clone(&self.roots), Arc::clone(&self.provider))
            .build()
            .expect("a non-empty root store builds a verifier")
    }

    fn build(&self, verifier: Arc<dyn ServerCertVerifier>) -> Arc<rustls::ClientConfig> {
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::clone(&self.provider))
            .with_safe_default_protocol_versions()
            .expect("ring supports the default TLS versions")
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        config.resumption = rustls::client::Resumption::disabled();
        Arc::new(config)
    }

    fn connector(&self) -> TlsConnector {
        TlsConnector::from(Arc::clone(&self.config))
    }

    fn pinned(&self, fingerprint: &str) -> TlsConnector {
        TlsConnector::from(self.build(Arc::new(Pinned {
            inner: self.verifier(),
            fingerprint: fingerprint.into(),
        })))
    }

    fn seen(&self) -> Option<CertificateDer<'static>> {
        self.seen.lock().unwrap().take()
    }
}

pub(super) fn tls_config(roots: rustls::RootCertStore) -> Tls {
    let roots = Arc::new(roots);
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let seen = Arc::new(Mutex::new(None));
    let verifier = Arc::new(Capture {
        inner: rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::clone(&roots), Arc::clone(&provider))
            .build()
            .expect("a non-empty root store builds a verifier"),
        seen: Arc::clone(&seen),
    });
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.resumption = rustls::client::Resumption::disabled();
    Tls { roots, provider, config: Arc::new(config), seen }
}

struct Verified {
    transport: Transport,
    server_name: String,
    public_key: Vec<u8>,
    connector: ClientConnector,
    selected: SecurityProtocol,
}

#[cfg(windows)]
mod windows_policy;

#[cfg(test)]
type TestPeerPolicy = fn(&str, &[rustls::pki_types::CertificateDer<'_>]) -> Result<(), Error>;

#[cfg(test)]
tokio::task_local! {
    static TEST_PEER_POLICY: TestPeerPolicy;
}

fn verify_peer_policy(
    name: &str,
    peer: &[rustls::pki_types::CertificateDer<'_>],
) -> Result<(), Error> {
    #[cfg(test)]
    if let Ok(result) = TEST_PEER_POLICY.try_with(|policy| policy(name, peer)) {
        return result;
    }
    #[cfg(windows)]
    return windows_policy::verify(name, peer);
    #[cfg(not(windows))]
    {
        let _ = (name, peer);
        Ok(())
    }
}

enum Negotiated {
    Upgraded(TokioFramed<TcpStream>, ClientConnector, SecurityProtocol),
    NlaUnavailable,
}

async fn open_stream(endpoint: &mut Endpoint, host: &str) -> Result<TcpStream, Error> {
    match endpoint {
        Endpoint::Direct(port) => TcpStream::connect((host, *port)).await.map_err(|_| Error::Tcp),
        Endpoint::Forwarded { stream, listen } => match stream.take() {
            Some(stream) => Ok(stream),
            // 같은 승인된 로컬 전달에 다시 연결해 TLS 전용 협상을 반복한다.
            None => TcpStream::connect(*listen).await.map_err(|_| Error::Tcp),
        },
    }
}

async fn negotiate(target: &mut Target, credssp: bool) -> Result<Negotiated, Error> {
    let stream = open_stream(&mut target.endpoint, &target.server_name).await?;
    let local = stream.local_addr().map_err(|_| Error::Tcp)?;
    stream.set_nodelay(true).map_err(|_| Error::Tcp)?;
    let mut framed = TokioFramed::new(stream);
    let mut connector = ClientConnector::new(config(target.size, target.scale, !credssp), local);
    // 연결 요청만 보내고, 선택된 프로토콜을 직접 읽어 TLS 전용 선택을 구분한다.
    step(&mut framed, &mut connector, Error::Negotiation).await?;
    let hint = connector.next_pdu_hint().ok_or(Error::Negotiation)?;
    let pdu = read_bounded(&mut framed, hint, Error::Negotiation, None).await?;
    let selected = match decode::<X224<ConnectionConfirm>>(&pdu).map_err(|_| Error::Negotiation)?.0 {
        ConnectionConfirm::Response { protocol, .. } => protocol,
        ConnectionConfirm::Failure { .. } => return Err(Error::Negotiation),
    };
    if credssp {
        // xrdp는 HYBRID를 모르면 SSL이 아니라 RDP(표준 보안)를 고른다. NLA가 아닌 선택은
        // 모두 NLA 미지원으로 보고, opt-in이면 SSL 요청으로 한 번만 다시 협상한다.
        if !matches!(selected, SecurityProtocol::HYBRID | SecurityProtocol::HYBRID_EX) {
            return Ok(Negotiated::NlaUnavailable);
        }
    } else if selected != SecurityProtocol::SSL {
        return Err(Error::Negotiation);
    }
    let mut output = WriteBuf::new();
    connector.step(&pdu, &mut output).map_err(|_| Error::Negotiation)?;
    if !output.filled().is_empty() {
        framed.write_all(output.filled()).await.map_err(|_| Error::Negotiation)?;
    }
    Ok(Negotiated::Upgraded(framed, connector, selected))
}

fn failure_reason(error: &std::io::Error) -> &'static str {
    use rustls::CertificateError;
    let Some(rustls::Error::InvalidCertificate(certificate)) = error.get_ref().and_then(|error| error.downcast_ref::<rustls::Error>()) else {
        return "체인 검증 실패";
    };
    match certificate {
        CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => "이름 불일치",
        CertificateError::Expired | CertificateError::ExpiredContext { .. } => "만료",
        CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => "유효 기간 전",
        CertificateError::UnknownIssuer => "알 수 없는 CA",
        _ => "체인 검증 실패",
    }
}

fn describe(dns: &str, endpoint: &str, cert: &CertificateDer<'_>, fingerprint: &str, previous: Option<&str>, reason: &str) -> trust::Request {
    use x509_cert::ext::pkix::{SubjectAltName, name::GeneralName};
    let subject = Certificate::from_der(cert.as_ref())
        .map(|cert| format!("{}", cert.tbs_certificate.subject))
        .unwrap_or_else(|_| "알 수 없음".into());
    let san = Certificate::from_der(cert.as_ref())
        .ok()
        .and_then(|cert| cert.tbs_certificate.get::<SubjectAltName>().ok().flatten())
        .map(|(_, san)| {
            san.0.iter().filter_map(|name| match name {
                GeneralName::DnsName(name) => Some(name.to_string()),
                // IP 주소로 연결하는 서버는 IP SAN으로 확인하므로 함께 보여 준다.
                GeneralName::IpAddress(ip) => match *ip.as_bytes() {
                    [a, b, c, d] => Some(std::net::Ipv4Addr::new(a, b, c, d).to_string()),
                    _ => <[u8; 16]>::try_from(ip.as_bytes()).ok().map(|v6| std::net::Ipv6Addr::from(v6).to_string()),
                },
                _ => None,
            }).collect::<Vec<_>>().join(", ")
        })
        .filter(|san| !san.is_empty())
        .unwrap_or_else(|| "없음".into());
    let text = if let Some(previous) = previous {
        format!(
            "RDP 서버 인증서가 바뀌었습니다\n\n원격 이름: {dns}\n대상: {endpoint}\n주체: {subject}\nSAN: {san}\n사유: {reason}\n이전 SHA-256: {previous}\n새 SHA-256: {fingerprint}\n\n새 지문으로 교체하고 연결합니까?"
        )
    } else {
        format!(
            "RDP 서버 인증서를 신뢰합니까?\n\n원격 이름: {dns}\n대상: {endpoint}\n주체: {subject}\nSAN: {san}\n사유: {reason}\nSHA-256: {fingerprint}\n\n이 지문을 저장하고 연결합니다."
        )
    };
    trust::Request { text, changed: previous.is_some() }
}

async fn negotiate_rdp(target: &mut Target) -> Result<(TokioFramed<TcpStream>, ClientConnector, SecurityProtocol), Error> {
    match negotiate(target, true).await? {
        Negotiated::Upgraded(framed, connector, selected) => Ok((framed, connector, selected)),
        Negotiated::NlaUnavailable => {
            if !target.tls_only {
                return Err(Error::TlsOnlyRequired);
            }
            match negotiate(target, false).await? {
                Negotiated::Upgraded(framed, connector, selected) => Ok((framed, connector, selected)),
                Negotiated::NlaUnavailable => Err(Error::Negotiation),
            }
        }
    }
}

async fn verify<A, AFut>(
    mut target: Target,
    server_name: ServerName<'static>,
    tls: Tls,
    trust: trust::Trust<A>,
) -> Result<Verified, Error>
where
    A: FnOnce(trust::Request) -> AFut,
    AFut: Future<Output = Result<bool, Error>>,
{
    let name = server_name.to_str().into_owned();
    let (framed, mut connector, mut selected) = tokio::time::timeout(super::CONNECT_TIMEOUT, negotiate_rdp(&mut target))
        .await.map_err(|_| Error::Timeout)??;
    let (stream, leftover) = framed.into_inner();
    if !leftover.is_empty() {
        return Err(Error::Negotiation);
    }
    let stream = match tokio::time::timeout(super::CONNECT_TIMEOUT, tls.connector().connect(server_name.clone(), stream))
        .await.map_err(|_| Error::Timeout)? {
        Ok(stream) => {
            let peer = stream.get_ref().1.peer_certificates().ok_or(Error::TlsIdentity)?;
            verify_peer_policy(&name, peer)?;
            stream
        }
        Err(error) => {
            let cert = tls.seen().ok_or(Error::TlsIdentity)?;
            let fingerprint = trust::fingerprint(cert.as_ref());
            let stored = trust::load(&trust.path, &trust.dns, &trust.endpoint, trust.route.as_deref());
            let known = stored.as_deref() == Some(fingerprint.as_str());
            if !known {
                let request = describe(&trust.dns, &trust.endpoint, &cert, &fingerprint, stored.as_deref(), failure_reason(&error));
                if !(trust.approve)(request).await? {
                    return Err(Error::TlsIdentity);
                }
                trust::save(&trust.path, &trust.dns, &trust.endpoint, trust.route.as_deref(), &fingerprint)?;
            }
            // 같은 대상에 다시 협상한 뒤 저장된 지문으로만 TLS를 완료한다.
            let (framed, next, next_selected) = tokio::time::timeout(super::CONNECT_TIMEOUT, negotiate_rdp(&mut target))
                .await.map_err(|_| Error::Timeout)??;
            connector = next;
            selected = next_selected;
            let (stream, leftover) = framed.into_inner();
            if !leftover.is_empty() {
                return Err(Error::Negotiation);
            }
            let stream = tokio::time::timeout(super::CONNECT_TIMEOUT, tls.pinned(&fingerprint).connect(server_name, stream))
                .await.map_err(|_| Error::Timeout)?.map_err(|_| Error::TlsIdentity)?;
            let peer = stream.get_ref().1.peer_certificates().ok_or(Error::TlsIdentity)?;
            #[cfg(windows)]
            windows_policy::verify_disallowed(peer)?;
            stream
        }
    };
    let peer = stream.get_ref().1.peer_certificates().ok_or(Error::TlsIdentity)?;
    let cert = peer.first().ok_or(Error::TlsIdentity)?;
    let cert = Certificate::from_der(cert.as_ref()).map_err(|_| Error::TlsIdentity)?;
    let public_key = cert.tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .ok_or(Error::TlsIdentity)?
        .to_vec();
    connector.mark_security_upgrade_as_done();
    Ok(Verified {
        transport: TokioFramed::new(stream),
        server_name: name,
        public_key,
        connector,
        selected,
    })
}

pub(super) async fn connect<F, Fut, A, AFut>(
    target: Target,
    name: ServerName<'static>,
    tls: Tls,
    trust: trust::Trust<A>,
    resolve: F,
    auth: AuthIo<'_>,
) -> Result<(ConnectionResult, Transport), Error>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Account, Error>>,
    A: FnOnce(trust::Request) -> AFut,
    AFut: Future<Output = Result<bool, Error>>,
{
    // 인증서 승인은 사용자 입력을 기다리므로 연결 타임아웃에 넣지 않는다.
    let mut verified = verify(target, name, tls, trust).await?;
    let account = resolve().await?;
    tokio::time::timeout(super::CONNECT_TIMEOUT, verified.authenticate(account, &auth))
        .await.map_err(|_| Error::Timeout)??;
    verified
        .connector
        .attach_static_channel(ironrdp::cliprdr::CliprdrClient::new(
            Box::<TextClipboard>::default(),
        ));
    verified
        .connector
        .attach_static_channel(ironrdp::dvc::DrdynvcClient::new().with_dynamic_channel(super::display::DisplayControl::default()));
    let result = tokio::time::timeout(super::CONNECT_TIMEOUT, async {
        loop {
            step(
                &mut verified.transport,
                &mut verified.connector,
                Error::Activation,
            )
            .await?;
            if matches!(verified.connector.state, ClientConnectorState::ConnectTimeAutoDetection { .. }) {
                verified.connector.config.credentials = Credentials::UsernamePassword {
                    username: String::new(),
                    password: String::new(),
                };
                verified.connector.config.domain = None;
            }
            if let ClientConnectorState::Connected { result } = verified.connector.state {
                return Ok::<_, Error>(result);
            }
        }
    }).await.map_err(|_| Error::Timeout)??;
    Ok((result, verified.transport))
}

impl Verified {
    async fn authenticate(&mut self, mut account: Account, auth: &AuthIo<'_>) -> Result<(), Error> {
        if account.username.is_empty()
            || account.password.is_empty()
            || [account.username.as_str(), account.password.as_str()]
                .into_iter()
                .chain(account.domain.as_deref())
                .any(|field| field.len() > 1024 || field.contains('\0'))
        {
            return Err(Error::Credentials);
        }
        if !matches!(self.selected, SecurityProtocol::HYBRID | SecurityProtocol::HYBRID_EX) {
            // TLS 전용에는 CredSSP가 없으므로 계정은 검증된 TLS 위의 Client Info로 전달된다.
            // 자동 로그온을 켜야 xrdp가 sesman 로그인 창 대신 세션을 시작한다.
            self.connector.config.autologon = true;
            self.connector.config.credentials = Credentials::UsernamePassword {
                username: account.username,
                password: std::mem::take(&mut *account.password),
            };
            self.connector.config.domain = account.domain;
            return Ok(());
        }
        let (mut sequence, mut request) = CredsspSequence::init(
            Credentials::UsernamePassword {
                username: account.username.clone(),
                password: std::mem::take(&mut *account.password),
            },
            account.domain.as_deref(),
            self.selected,
            self.server_name.clone().into(),
            std::mem::take(&mut self.public_key),
            None,
        )
        .map_err(|_| Error::Authentication)?;
        let mut output = WriteBuf::new();
        loop {
            let state = match sequence.process_ts_request(request).start() {
                GeneratorState::Completed(result) => result.map_err(|_| Error::Authentication)?,
                // NTLM needs no network callback. Keep transport scoped to the user's target.
                GeneratorState::Suspended(_) => return Err(Error::Authentication),
            };
            output.clear();
            if sequence
                .handle_process_result(state, &mut output)
                .map_err(|_| Error::Authentication)?
                .size()
                .is_some()
            {
                write_credssp(self.transport.get_inner_mut().0, output.filled(), auth).await?;
            }
            let Some(hint) = sequence.next_pdu_hint() else {
                break;
            };
            let pdu = read_bounded(&mut self.transport, hint, Error::Authentication, Some(auth)).await?;
            let Some(next) = sequence
                .decode_server_message(&pdu)
                .map_err(|_| Error::Authentication)?
            else {
                break;
            };
            request = next;
        }
        self.connector.mark_credssp_as_done();
        // NLA already authenticated; subsequent client info has no password (nor does reactivation retain one).
        self.connector.config.credentials = Credentials::UsernamePassword {
            username: account.username,
            password: String::new(),
        };
        self.connector.config.domain = account.domain;
        Ok(())
    }
}

async fn write_credssp<S: AsyncWrite + Unpin>(
    stream: &mut S,
    mut bytes: &[u8],
    auth: &AuthIo<'_>,
) -> Result<(), Error> {
    // TLS가 보관한 바이트는 flush에서도 전송되므로 각 poll을 철회와 직렬화한다.
    while !bytes.is_empty() {
        let count = poll_fn(|cx| {
            auth.poll(|| Pin::new(&mut *stream).poll_write(cx, bytes))
        }).await?;
        if count == 0 {
            return Err(Error::Authentication);
        }
        bytes = &bytes[count..];
    }
    poll_fn(|cx| {
        auth.poll(|| Pin::new(&mut *stream).poll_flush(cx))
    }).await
}

pub(super) async fn step<S: AsyncRead + AsyncWrite + Send + Sync + Unpin>(
    transport: &mut TokioFramed<S>,
    sequence: &mut dyn Sequence,
    failure: Error,
) -> Result<(), Error> {
    let pdu = match sequence.next_pdu_hint() {
        Some(hint) => Some(read_bounded(transport, hint, failure, None).await?),
        None => None,
    };
    apply(transport, sequence, pdu.as_deref(), failure).await
}

/// 재활성화 순서의 한 단계다. xrdp는 Deactivate All 뒤에도 화면 갱신(fast-path와 slow-path Update·Pointer)을 보내므로
/// 활성화 PDU만 순서에 넘기고 화면 갱신은 버린다. 재활성화가 끝나면 서버가 새 크기의 전체 화면을 다시 보낸다.
pub(super) async fn reactivation_step<S: AsyncRead + AsyncWrite + Send + Sync + Unpin>(
    transport: &mut TokioFramed<S>,
    sequence: &mut dyn Sequence,
) -> Result<(), Error> {
    if sequence.next_pdu_hint().is_none() {
        return apply(transport, sequence, None, Error::Activation).await;
    }
    loop {
        let pdu = read_bounded(transport, &ironrdp::pdu::RDP_HINT, Error::Activation, None).await?;
        if !graphics_update(&pdu) {
            return apply(transport, sequence, Some(&pdu), Error::Activation).await;
        }
    }
}

fn graphics_update(pdu: &[u8]) -> bool {
    use ironrdp::pdu::{Action, mcs::decode_send_data_indication, rdp::headers::{IoChannelPdu, ShareDataPdu, decode_io_channel}};
    match ironrdp::pdu::find_size(pdu) {
        Ok(Some(info)) if info.action == Action::FastPath => true,
        Ok(Some(_)) => decode_send_data_indication(pdu)
            .ok()
            .and_then(|data| decode_io_channel(data).ok())
            .is_some_and(|io| matches!(io, IoChannelPdu::Data(ctx) if matches!(ctx.pdu, ShareDataPdu::Update(_) | ShareDataPdu::Pointer(_)))),
        _ => false,
    }
}

async fn apply<S: AsyncRead + AsyncWrite + Send + Sync + Unpin>(
    transport: &mut TokioFramed<S>,
    sequence: &mut dyn Sequence,
    pdu: Option<&[u8]>,
    failure: Error,
) -> Result<(), Error> {
    let mut output = WriteBuf::new();
    let written = match pdu {
        Some(pdu) => sequence.step(pdu, &mut output),
        None => sequence.step_no_input(&mut output),
    }
    .map_err(|_| failure)?;
    if written.size().is_some() {
        transport
            .write_all(output.filled())
            .await
            .map_err(|_| failure)?;
    }
    Ok(())
}

pub(super) async fn read_bounded<S: AsyncRead + Unpin>(
    transport: &mut TokioFramed<S>,
    hint: &dyn PduHint,
    failure: Error,
    auth: Option<&AuthIo<'_>>,
) -> Result<Bytes, Error> {
    const LIMIT: usize = 65_535;
    let (stream, buffer) = transport.get_inner_mut();
    loop {
        if let Some((matched, size)) = hint.find_size(buffer).map_err(|_| failure)? {
            if !matched || size == 0 || size > LIMIT {
                return Err(failure);
            }
            if buffer.len() >= size {
                return Ok(buffer.split_to(size).freeze());
            }
        }
        let remaining = LIMIT - buffer.len();
        if remaining == 0 {
            return Err(failure);
        }
        buffer.reserve(remaining.min(4096));
        let mut stream = (&mut *stream).take(remaining as u64);
        let read = stream.read_buf(buffer);
        let count = if let Some(auth) = auth {
            // TLS 읽기 오류도 소켓에 alert를 쓸 수 있다.
            let mut read = std::pin::pin!(read);
            poll_fn(|cx| auth.poll(|| read.as_mut().poll(cx))).await?
        } else {
            read.await.map_err(|_| failure)?
        };
        if count == 0 {
            return Err(failure);
        }
    }
}

#[cfg(test)]
mod tests;
