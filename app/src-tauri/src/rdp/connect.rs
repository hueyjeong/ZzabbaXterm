use std::sync::Arc;

use ironrdp::{
    connector::{
        self, ClientConnector, ClientConnectorState, ConnectionResult, Credentials, DesktopSize,
        Sequence, credssp::CredsspSequence, sspi::generator::GeneratorState,
    },
    core::WriteBuf,
    pdu::{
        PduHint,
        gcc::KeyboardType,
        nego::SecurityProtocol,
        rdp::capability_sets::{BitmapCodecs, MajorPlatformType},
    },
};
use ironrdp_tokio::{FramedWrite as _, TokioFramed, bytes::Bytes};
use tokio::{io::{AsyncRead, AsyncReadExt as _, AsyncWrite}, net::TcpStream};
use tokio_rustls::{
    TlsConnector,
    client::TlsStream,
    rustls::{self, pki_types::ServerName},
};
use x509_cert::{Certificate, der::Decode as _};

use super::{Account, Error, Target, clipboard::TextClipboard, route::Endpoint};

pub(super) type Transport = TokioFramed<TlsStream<TcpStream>>;

fn config(size: DesktopSize) -> connector::Config {
    connector::Config {
        desktop_size: size,
        desktop_scale_factor: 0,
        // PROTOCOL_SSL is TLS-only; NLA uses TLS under HYBRID or HYBRID_EX.
        enable_tls: false,
        enable_credssp: true,
        // Smart-card variant with no identity emits no pre-TLS username cookie.
        credentials: Credentials::SmartCard {
            pin: String::new(),
            config: None,
        },
        domain: None,
        client_build: 0,
        client_name: "ZzabbaXterm".into(),
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_functional_keys_count: 12,
        keyboard_layout: 0,
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
    if let Ok(store) = schannel::cert_store::CertStore::open_current_user("ROOT") {
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

pub(super) fn tls_config(roots: rustls::RootCertStore) -> Arc<rustls::ClientConfig> {
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring supports the default TLS versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.resumption = rustls::client::Resumption::disabled();
    Arc::new(config)
}

struct Verified {
    transport: Transport,
    server_name: String,
    public_key: Vec<u8>,
    connector: ClientConnector,
    selected: SecurityProtocol,
}

async fn verify(
    target: Target,
    server_name: ServerName<'static>,
    tls: Arc<rustls::ClientConfig>,
) -> Result<Verified, Error> {
    let stream = match target.endpoint {
        Endpoint::Direct(port) => TcpStream::connect((target.server_name.as_str(), port))
            .await
            .map_err(|_| Error::Tcp)?,
        Endpoint::Forwarded(stream) => stream,
    };
    let local = stream.local_addr().map_err(|_| Error::Tcp)?;
    stream.set_nodelay(true).map_err(|_| Error::Tcp)?;
    let mut framed = TokioFramed::new(stream);
    let mut connector = ClientConnector::new(config(target.size), local);
    while !connector.should_perform_security_upgrade() {
        step(&mut framed, &mut connector, Error::Negotiation).await?;
    }
    let ClientConnectorState::EnhancedSecurityUpgrade {
        selected_protocol: selected,
    } = connector.state
    else {
        return Err(Error::Negotiation);
    };
    if !matches!(selected, SecurityProtocol::HYBRID | SecurityProtocol::HYBRID_EX) {
        return Err(Error::Negotiation);
    }
    let (stream, leftover) = framed.into_inner();
    if !leftover.is_empty() {
        return Err(Error::Negotiation);
    }
    let name = server_name.to_str().into_owned();
    let stream = TlsConnector::from(tls)
        .connect(server_name, stream)
        .await
        .map_err(|_| Error::TlsIdentity)?;
    let cert = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|chain| chain.first())
        .ok_or(Error::TlsIdentity)?;
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

pub(super) async fn connect<F, Fut>(
    target: Target,
    name: ServerName<'static>,
    tls: Arc<rustls::ClientConfig>,
    resolve: F,
    display: ironrdp::displaycontrol::client::DisplayControlClient,
) -> Result<(ConnectionResult, Transport), Error>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Account, Error>>,
{
    let mut verified = tokio::time::timeout(super::CONNECT_TIMEOUT, verify(target, name, tls))
        .await.map_err(|_| Error::Timeout)??;
    let account = resolve().await?;
    tokio::time::timeout(super::CONNECT_TIMEOUT, verified.authenticate(account))
        .await.map_err(|_| Error::Timeout)??;
    verified
        .connector
        .attach_static_channel(ironrdp::cliprdr::CliprdrClient::new(
            Box::<TextClipboard>::default(),
        ));
    verified
        .connector
        .attach_static_channel(ironrdp::dvc::DrdynvcClient::new().with_dynamic_channel(display));
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
    async fn authenticate(&mut self, mut account: Account) -> Result<(), Error> {
        if account.username.is_empty()
            || account.password.is_empty()
            || [account.username.as_str(), account.password.as_str()]
                .into_iter()
                .chain(account.domain.as_deref())
                .any(|field| field.len() > 1024 || field.contains('\0'))
        {
            return Err(Error::Credentials);
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
                self.transport
                    .write_all(output.filled())
                    .await
                    .map_err(|_| Error::Authentication)?;
            }
            let Some(hint) = sequence.next_pdu_hint() else {
                break;
            };
            let pdu = read_bounded(&mut self.transport, hint, Error::Authentication).await?;
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

pub(super) async fn step<S: AsyncRead + AsyncWrite + Send + Sync + Unpin>(
    transport: &mut TokioFramed<S>,
    sequence: &mut dyn Sequence,
    failure: Error,
) -> Result<(), Error> {
    let mut output = WriteBuf::new();
    let written = if let Some(hint) = sequence.next_pdu_hint() {
        let pdu = read_bounded(transport, hint, failure).await?;
        sequence.step(&pdu, &mut output)
    } else {
        sequence.step_no_input(&mut output)
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
        if (&mut *stream)
            .take(remaining as u64)
            .read_buf(buffer)
            .await
            .map_err(|_| failure)?
            == 0
        {
            return Err(failure);
        }
    }
}

#[cfg(test)]
mod tests;
