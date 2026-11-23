mod input;

use std::{collections::HashMap, io, sync::{Arc, Mutex}, task::Poll, time::Duration};

use ironrdp::connector::DesktopSize;
use serde::{Deserialize, Serialize};
use tauri::{Manager, UriSchemeContext, UriSchemeResponder, Webview, WebviewWindow,
    http::{Request, Response}, ipc::{Channel, CommandArg, CommandItem, Invoke, InvokeError}};
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{connections::{profile::{SshTransport, valid_id}, with_saved_ssh}, tunnels::{Destination, ipc::{DesktopCleanupError, DesktopLease, Protocol, TunnelTabs}}};
use super::{Error, Event, Session, native, route};
use input::{Command, Controls};

const COMMANDS: [&str; 5] = ["rdp_prepare", "rdp_connect", "rdp_input", "rdp_ack", "rdp_close"];
const CLOSED: &str = "RDP 세션이 닫혔습니다";
const MAX_SAFE: u64 = (1 << 53) - 1;
const CHUNK: usize = 256 * 1024;
const MAX_JSON: usize = 6 * super::clipboard::MAX_TEXT + 4096;

pub(crate) fn handles(command: &str) -> bool { COMMANDS.contains(&command) }

#[derive(Default)]
pub(crate) struct Sessions(Mutex<Registry>);

#[derive(Default)]
struct Registry {
    document: u64,
    proof: Option<DocumentProof>,
    generation: u64,
    tabs: HashMap<String, Arc<Slot>>,
    approving: Option<Arc<()>>,
}

struct DocumentProof {
    key: String,
    origin: String,
    tunnel_epoch: u64,
}

pub(super) struct Slot {
    generation: u64,
    life: CancellationToken,
    mailbox: Mutex<Option<Packet>>,
    controls: AsyncMutex<Controls>,
    cleanup: AsyncMutex<Cleanup>,
}

pub(super) struct AuthIo<'a> {
    slot: &'a Slot,
    lease: Option<&'a DesktopLease>,
}

impl<'a> AuthIo<'a> {
    pub(super) fn new(slot: &'a Slot, lease: Option<&'a DesktopLease>) -> Self {
        Self { slot, lease }
    }

    pub(super) fn poll<T>(&self, io: impl FnOnce() -> Poll<io::Result<T>>) -> Poll<Result<T, Error>> {
        let poll = || {
            let _mailbox = self.slot.mailbox.lock().unwrap();
            if self.slot.life.is_cancelled() {
                return Poll::Ready(Err(Error::Closed));
            }
            io().map_err(|_| Error::Authentication)
        };
        // R01 → mailbox → 비차단 TLS poll. Pending에서도 잠금은 즉시 풀린다.
        match self.lease {
            Some(lease) => lease.while_live(poll).unwrap_or(Poll::Ready(Err(Error::Transport))),
            None => poll(),
        }
    }
}

#[derive(Default)]
enum Cleanup {
    #[default]
    Prepared,
    Running(oneshot::Receiver<Result<(), CleanupFailure>>),
    Failed(DesktopCleanupError),
    Unconfirmed,
    Done,
}

enum CleanupFailure {
    Tunnel(DesktopCleanupError, Option<String>),
    Unconfirmed,
}

struct Packet {
    ticket: String,
    sequence: u64,
    offset: usize,
    body: Option<(&'static str, Vec<u8>)>,
    done: oneshot::Sender<()>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Identity { session_id: String, generation: u64 }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Draft {
    host: String,
    port: u16,
    #[serde(default)]
    tls_only: bool,
    #[serde(default, deserialize_with = "read_source")]
    source: Option<Source>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Source { tab_handle: String, tunnel_id: String, target_id: String, destination: ForwardDestination }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForwardDestination { host: String, port: u16 }

fn read_source<'de, D: serde::Deserializer<'de>>(value: D) -> Result<Option<Source>, D::Error> {
    Source::deserialize(value).map(Some)
}

impl Draft {
    fn approval_text(&self, dns: &str) -> Result<String, Error> {
        if !route::dns_name(dns) || self.port == 0 { return Err(Error::InvalidServerName); }
        let login = if self.tls_only { "NLA, 미지원 시 TLS 전용(표준 RDP 보안 없음)" } else { "NLA" };
        let Some(source) = &self.source else {
            return Ok(format!("서버: {dns}:{}\n경로: 직접\n로그인: {login}", self.port));
        };
        let ForwardDestination { host, port } = &source.destination;
        if !matches!(host.as_str(), "127.0.0.1" | "::1") || *port != self.port || !valid_id(&source.target_id) {
            return Err(Error::InvalidForwardDestination);
        }
        let destination = std::net::SocketAddr::new(host.parse().map_err(|_| Error::InvalidForwardDestination)?, *port);
        Ok(format!("경로: R01 SSH 전달\nSSH 대상 ID: {}\n포워드 목적지: {destination}\nTLS 인증 DNS: {dns}\n로그인: {login}", source.target_id))
    }
}

impl Registry {
    fn get(&self, id: &Identity, document: u64) -> Option<&Arc<Slot>> {
        if document != self.document { return None; }
        self.tabs.get(&id.session_id).filter(|slot| slot.generation == id.generation)
    }
}

impl Slot {
    pub(super) fn new(generation: u64) -> Self {
        Self { generation, life: CancellationToken::new(), mailbox: Mutex::default(), controls: AsyncMutex::default(), cleanup: AsyncMutex::default() }
    }

    pub(super) fn close(&self) {
        let mut mailbox = self.mailbox.lock().unwrap();
        self.life.cancel();
        mailbox.take();
    }

    async fn finish_close(&self) -> Result<(), String> {
        let mut cleanup = self.cleanup.lock().await;
        let result = match &mut *cleanup {
            Cleanup::Prepared | Cleanup::Done => Ok(()),
            Cleanup::Running(done) => done.await.unwrap_or(Err(CleanupFailure::Unconfirmed)),
            Cleanup::Failed(_) => {
                let Cleanup::Failed(mut ticket) = std::mem::replace(&mut *cleanup, Cleanup::Unconfirmed) else { unreachable!() };
                tauri::async_runtime::spawn_blocking(move || {
                    match ticket.retry() {
                        Ok(()) => Ok(()),
                        Err(error) => Err(CleanupFailure::Tunnel(ticket, Some(error))),
                    }
                }).await.unwrap_or(Err(CleanupFailure::Unconfirmed))
            }
            Cleanup::Unconfirmed => Err(CleanupFailure::Unconfirmed),
        };
        let (state, result) = match result {
            Ok(()) => (Cleanup::Done, Ok(())),
            Err(CleanupFailure::Tunnel(ticket, retry_error)) => {
                let message = match retry_error {
                    Some(error) => format!("RDP 종료 확인 불가: {error}"),
                    None => format!("RDP 종료 확인 불가: {ticket}"),
                };
                (Cleanup::Failed(ticket), Err(message))
            }
            Err(CleanupFailure::Unconfirmed) => (Cleanup::Unconfirmed, Err("RDP 종료 확인 불가".into())),
        };
        *cleanup = state;
        result
    }

    fn ack(&self, sequence: u64, offset: usize) -> Option<()> {
        let mut mailbox = self.mailbox.lock().unwrap();
        if !mailbox.as_ref().is_some_and(|p| p.sequence == sequence && p.offset == offset && p.body.is_none()) { return None; }
        mailbox.take()?.done.send(()).ok()
    }

    async fn emit(&self, notify: &Channel<String>, sequence: u64, offset: usize, content_type: &'static str, bytes: Vec<u8>) -> Result<(), Error> {
        if bytes.len() > if content_type == "application/json" { MAX_JSON } else { CHUNK + 12 } { return Err(Error::Bounds); }
        let ticket = opaque().ok_or(Error::Closed)?;
        let (done, reply) = oneshot::channel();
        {
            let mut packet = self.mailbox.lock().unwrap();
            if self.life.is_cancelled() || packet.is_some() { return Err(Error::Closed); }
            *packet = Some(Packet { ticket: ticket.clone(), sequence, offset, body: Some((content_type, bytes)), done });
        }
        notify.send(ticket).map_err(|_| Error::Closed)?;
        tokio::time::timeout(Duration::from_secs(10), reply).await.map_err(|_| Error::Timeout)?.map_err(|_| Error::Closed)
    }

    async fn json(&self, notify: &Channel<String>, sequence: u64, body: serde_json::Value) -> Result<(), Error> {
        self.emit(notify, sequence, 0, "application/json", serde_json::to_vec(&body).map_err(|_| Error::Protocol)?).await
    }
}

impl Sessions {
    pub(crate) fn retire(&self) {
        let mut state = self.0.lock().unwrap();
        state.document = state.document.saturating_add(1);
        state.proof = None;
        for (_, slot) in state.tabs.drain() { slot.close(); }
    }
}

pub(super) fn opaque() -> Option<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).ok()?;
    Some(format!("{:032x}", u128::from_le_bytes(bytes)))
}

pub(crate) fn issue_document(webview: &Webview, url: &tauri::Url) {
    if webview.label() != "main" || webview.window_ref().label() != "main" { return; }
    let bundled = if cfg!(windows) {
        url.host_str() == Some("tauri.localhost") && matches!(url.scheme(), "http" | "https") && url.port().is_none()
    } else { url.host_str() == Some("localhost") && url.scheme() == "tauri" && url.port().is_none() };
    let dev = tauri::is_dev() && webview.config().build.dev_url.as_ref().is_some_and(|expected| expected.origin() == url.origin());
    if !(bundled || dev) || webview.url().as_ref().ok() != Some(url) { return; }
    let sessions = webview.state::<Sessions>();
    let mut state = sessions.0.lock().unwrap();
    if state.document > MAX_SAFE || state.proof.is_some() { return; }
    let Some(proof) = opaque() else { return; };
    let epoch = state.document;
    // 연결 요청 시점의 새 R01 문서가 아니라 증표를 발급한 문서에 연결을 묶는다.
    let tunnel_epoch = webview.state::<TunnelTabs>().epoch();
    let origin = if bundled && !cfg!(windows) { "tauri://localhost".into() } else { url.origin().ascii_serialization() };
    let href = serde_json::to_string(url.as_str()).unwrap();
    let script = format!("if (window === top && location.href === {href} && (window.__ZZABBA_RDP_DOCUMENT__?.epoch ?? -1) < {epoch}) {{ window.__ZZABBA_RDP_DOCUMENT__ = Object.freeze({{epoch: {epoch}, proof: '{proof}'}}); }}");
    if webview.eval(script).is_ok() { state.proof = Some(DocumentProof { key: proof, origin, tunnel_epoch }); }
}

pub(crate) fn protocol(ctx: UriSchemeContext<'_, tauri::Wry>, request: Request<Vec<u8>>, responder: UriSchemeResponder) {
    let mut responder = Some(responder);
    (|| {
        if ctx.webview_label() != "main" || request.method() != "GET" { return None; }
        let (proof, ticket) = request.uri().path().strip_prefix('/')?.split_once('-')?;
        if proof.len() != 32 || ticket.len() != 32 { return None; }
        let origin = request.headers().get("origin")?.to_str().ok()?;
        let sessions = ctx.app_handle().state::<Sessions>();
        let state = sessions.0.lock().unwrap();
        if !state.proof.as_ref().is_some_and(|expected| expected.key == proof && expected.origin == origin) { return None; }
        let (kind, body) = state.tabs.values().find_map(|slot| {
            let mut pending = slot.mailbox.lock().unwrap();
            pending.as_mut().filter(|p| p.ticket == ticket)?.body.take()
        })?;
        responder.take()?.respond(Response::builder().header("Cache-Control", "no-store")
            .header("Access-Control-Allow-Origin", origin).header("Content-Type", kind).body(body).unwrap());
        Some(())
    })();
    if let Some(responder) = responder {
        responder.respond(Response::builder().status(404).header("Cache-Control", "no-store").body(Vec::<u8>::new()).unwrap());
    }
}

pub(crate) fn dispatch(invoke: Invoke) {
    let caller = invoke.message.webview_ref();
    if caller.label() != "main" || caller.window_ref().label() != "main" || invoke.acl.is_none() {
        invoke.resolver.reject(CLOSED); return;
    }
    let Some(name) = COMMANDS.into_iter().find(|name| *name == invoke.message.command()) else { invoke.resolver.reject(CLOSED); return; };
    let sessions = caller.state::<Sessions>();
    let mut state = sessions.0.lock().unwrap();
    let proof = invoke.message.headers().get("x-zzabba-rdp-document").and_then(|v| v.to_str().ok());
    let Some(proof) = state.proof.as_ref().filter(|expected| Some(expected.key.as_str()) == proof) else {
        invoke.resolver.reject(CLOSED); return;
    };
    let tunnel_epoch = proof.tunnel_epoch;
    let document = state.document;
    let arg = |key| CommandItem { name, key, plugin: None, message: &invoke.message, acl: &invoke.acl };
    macro_rules! read {
        ($ty:ty, $key:literal) => { match <$ty>::from_command(arg($key)) {
            Ok(value) => value, Err(error) => { invoke.resolver.invoke_error(error); return; }
        }};
    }
    if name == "rdp_prepare" {
        if state.tabs.len() >= 8 || state.generation >= MAX_SAFE { invoke.resolver.reject("RDP 탭을 더 열 수 없습니다"); return; }
        let Some(session_id) = opaque() else { invoke.resolver.reject(CLOSED); return; };
        state.generation += 1;
        let generation = state.generation;
        state.tabs.insert(session_id.clone(), Arc::new(Slot::new(generation)));
        invoke.resolver.resolve(Identity { session_id, generation });
        return;
    }
    let id = read!(Identity, "id");
    let Some(slot) = state.get(&id, document).cloned() else { invoke.resolver.reject(CLOSED); return; };
    match name {
        "rdp_close" => {
            slot.close();
            let window = caller.clone();
            drop(state);
            invoke.resolver.respond_async(async move {
                slot.finish_close().await.map_err(InvokeError::from)?;
                let sessions = window.state::<Sessions>();
                let mut state = sessions.0.lock().unwrap();
                if state.get(&id, document).is_some() { state.tabs.remove(&id.session_id); }
                Ok(())
            });
        }
        "rdp_ack" => {
            let sequence = read!(u64, "sequence");
            let offset = read!(usize, "offset");
            if slot.ack(sequence, offset).is_some() { invoke.resolver.resolve(()); } else { invoke.resolver.reject(CLOSED); }
        }
        "rdp_input" => {
            let command = read!(Command, "input");
            drop(state);
            invoke.resolver.respond_async(async move {
                match slot.controls.try_lock().map_err(|_| InvokeError::from(message(Error::Busy)))?
                    .send(&slot.life, command).await {
                    Ok(()) => Ok(true),
                    Err(Error::StaleDisplay) => Ok(false),
                    Err(error) => Err(InvokeError::from(message(error))),
                }
            });
        }
        "rdp_connect" => {
            if !cfg!(any(windows, target_os = "linux")) {
                invoke.resolver.reject("이 운영체제에서는 RDP에 연결할 수 없습니다");
                return;
            }
            let window = read!(WebviewWindow, "webview");
            let draft = read!(Draft, "draft");
            let channel = read!(Channel<String>, "output");
            if state.approving.as_ref().is_some_and(|lease| Arc::strong_count(lease) > 1) {
                invoke.resolver.reject("다른 RDP 연결을 승인하는 중입니다"); return;
            }
            if slot.life.is_cancelled() { invoke.resolver.reject(CLOSED); return; }
            let Ok(mut cleanup) = slot.cleanup.try_lock() else { invoke.resolver.reject(CLOSED); return; };
            let Ok(mut controls) = slot.controls.try_lock() else { invoke.resolver.reject(CLOSED); return; };
            if controls.started { invoke.resolver.reject(CLOSED); return; }
            controls.started = true;
            drop(controls);
            let (closed, done) = oneshot::channel();
            *cleanup = Cleanup::Running(done);
            drop(cleanup);
            let approval = Arc::new(());
            state.approving = Some(approval.clone());
            drop(state);
            invoke.resolver.respond_async(async move {
                let mut lease = None;
                let result = run(&window, &slot, &channel, draft, tunnel_epoch, approval, &mut lease).await;
                slot.close();
                let cleanup = match lease {
                    Some(lease) => tauri::async_runtime::spawn_blocking(move || {
                        lease.release().map_err(|ticket| CleanupFailure::Tunnel(ticket, None))
                    })
                        .await.unwrap_or(Err(CleanupFailure::Unconfirmed)),
                    None => Ok(()),
                };
                let _ = closed.send(cleanup);
                result.map_err(InvokeError::from)
            });
        }
        _ => invoke.resolver.reject(CLOSED),
    }
}

async fn run(window: &WebviewWindow, slot: &Slot, notify: &Channel<String>, draft: Draft, tunnel_epoch: u64, approval: Arc<()>, lease: &mut Option<DesktopLease>) -> Result<(), String> {
    let life = &slot.life;
    let prompt = life.child_token();
    let _prompt_lifetime = prompt.clone().drop_guard();
    let dns = draft.host.to_ascii_lowercase();
    let description = draft.approval_text(&dns).map_err(message)?;
    let trust_path = window.path().app_config_dir().map_err(|error| error.to_string())?.join("rdp-trust.json");
    let size = DesktopSize { width: 1024, height: 768 };
    let (target, loss, target_name, endpoint) = if let Some(source) = draft.source {
        let directory = window.path().app_config_dir().map_err(|error| error.to_string())?;
        let load_directory = directory.clone();
        let target_id = source.target_id.clone();
        let (revision, ssh) = tauri::async_runtime::spawn_blocking(move || with_saved_ssh(&load_directory, &target_id, |saved| saved))
            .await.map_err(|_| message(Error::Transport))??
            .filter(|(_, ssh)| matches!(ssh.transport, SshTransport::Direct {}))
            .ok_or("저장된 직접 SSH 접속 설정이 없습니다")?;
        let destination = Destination { host: source.destination.host, port: source.destination.port };
        window.state::<TunnelTabs>().preflight_desktop(tunnel_epoch, (&source.tab_handle, &source.tunnel_id),
            &source.target_id, &destination, Protocol::Rdp)?;
        let description = format!("{description}\nSSH 서버: {}:{}\nSSH 계정: {}\nSSH 호스트 키: {}",
            ssh.host, ssh.port, ssh.account, ssh.host_key_fingerprint);
        native::confirm(window.clone(), description, prompt.clone(), approval.clone()).await.map_err(message)?;
        let target_name = format!("{dns} (TLS DNS)");
        let caller = window.clone();
        let live = prompt.clone();
        // 발급 중 취소되어도 작업을 기다려 인계 소켓과 임대를 이 경로에서 정리한다.
        let (issued, approved_destination) = tauri::async_runtime::spawn_blocking(move || {
            if live.is_cancelled() { return Err(CLOSED.into()); }
            let lease = caller.state::<TunnelTabs>().open_desktop(tunnel_epoch, &directory,
                (&source.tab_handle, &source.tunnel_id), &source.target_id, &destination, Protocol::Rdp, (revision, &ssh))?;
            Ok::<_, String>((lease, destination))
        }).await.map_err(|_| message(Error::Transport))??;
        let lease = lease.insert(issued);
        if life.is_cancelled() { return Err(CLOSED.into()); }
        let loss = lease.loss();
        let target = route::forwarded(dns.clone(), &approved_destination, size, lease, draft.tls_only).map_err(message)?;
        let endpoint = format!("{}:{}", approved_destination.host, approved_destination.port);
        (target, Some(loss), target_name, endpoint)
    } else {
        native::confirm(window.clone(), description, prompt.clone(), approval.clone()).await.map_err(message)?;
        let target_name = format!("{dns}:{}", draft.port);
        let endpoint = target_name.clone();
        (route::direct(dns.clone(), draft.port, size, draft.tls_only), None, target_name, endpoint)
    };
    let stopped = async {
        tokio::select! { biased;
            () = life.cancelled() => Error::Closed,
            () = async { match loss { Some(loss) => loss.cancelled().await, None => std::future::pending().await } } => Error::Transport,
        }
    };
    tokio::pin!(stopped);
    let trust = super::trust::Trust {
        path: trust_path,
        dns: dns.clone(),
        endpoint,
        approve: {
            let window = window.clone();
            let live = prompt.clone();
            let approval = approval.clone();
            move |request| {
                let window = window.clone();
                let live = live.clone();
                let approval = approval.clone();
                async move { native::trust(window, request, live, approval).await }
            }
        },
    };
    let mut session = tokio::select! { biased;
        error = &mut stopped => {
            prompt.cancel();
            return Err(message(error).into());
        },
        session = Session::open(target, tokio_rustls::rustls::RootCertStore::empty(), trust,
            || native::account(window.clone(), target_name, prompt.clone(), approval.clone()), AuthIo::new(slot, lease.as_ref())) => session.map_err(message)?,
    };
    drop(approval);
    let operation = async {
        let mut sequence = 0_u64;
        let mut frame_sequence = 0_u64;
        let mut size = (0, 0);
        let mut display_generation = 0;
        let first = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let event = tokio::select! {
                next = session.next() => next,
                () = tokio::time::sleep_until(first), if size == (0, 0) => return Err(Error::Timeout),
            };
            let Some(event) = event else { return Ok(()); };
            sequence = sequence.checked_add(1).filter(|n| *n <= MAX_SAFE).ok_or(Error::Bounds)?;
            match event {
                Event::Frame(frame) => {
                    let generation = session.display_generation;
                    let new_display = generation != display_generation;
                    frame_sequence += 1;
                    let rect = frame.rect;
                    let length = super::frame::frame_len(frame.width, frame.height)?;
                    let next_size = (frame.width, frame.height);
                    let full = rect.x == 0 && rect.y == 0 && rect.width == frame.width && rect.height == frame.height;
                    if usize::from(rect.x) + usize::from(rect.width) > usize::from(frame.width) ||
                        usize::from(rect.y) + usize::from(rect.height) > usize::from(frame.height) ||
                        frame.rgba.is_empty() || frame.rgba.len() != usize::from(rect.width) * usize::from(rect.height) * 4 ||
                        frame.rgba.len() > length || ((new_display || size != next_size) && !full) ||
                        (!new_display && size != next_size) || frame.session != session.id() { return Err(Error::Bounds); }
                    slot.json(notify, sequence, serde_json::json!({"sequence": sequence, "frameSequence": frame_sequence, "kind": "frame", "full": full,
                        "displayGeneration": generation, "width": frame.width, "height": frame.height,
                        "x": rect.x, "y": rect.y, "w": rect.width, "h": rect.height})).await?;
                    for (i, chunk) in frame.rgba.chunks(CHUNK).enumerate() {
                        let offset = i * CHUNK;
                        let mut packet = Vec::with_capacity(chunk.len() + 12);
                        packet.extend_from_slice(&sequence.to_le_bytes());
                        packet.extend_from_slice(&(offset as u32).to_le_bytes());
                        packet.extend_from_slice(chunk);
                        slot.emit(notify, sequence, offset + 1, "application/octet-stream", packet).await?;
                    }
                    if new_display {
                        display_generation = generation;
                        size = next_size;
                        slot.controls.lock().await.display(session.id(), session.input_handle(), size, generation);
                        sequence = sequence.checked_add(1).filter(|n| *n <= MAX_SAFE).ok_or(Error::Bounds)?;
                        slot.json(notify, sequence, serde_json::json!({"sequence": sequence, "kind": "display", "displayGeneration": generation})).await?;
                    }
                }
                Event::Channels(next) => {
                    let channels = next;
                    slot.controls.lock().await.channels(next);
                    slot.json(notify, sequence, serde_json::json!({"sequence": sequence, "kind": "channels",
                        "paste": channels.cliprdr_ready, "copy": channels.cliprdr_ready && channels.remote_clipboard_text,
                        "resize": channels.display_max_area.is_some()})).await?;
                }
                Event::ClipboardText(result) => {
                    let copy_epoch = slot.controls.lock().await.take_copy();
                    if let Some(epoch) = copy_epoch {
                        let body = match result {
                            Ok(text) => serde_json::json!({"sequence": sequence, "kind": "clipboard", "epoch": epoch, "text": text}),
                            Err(error) => serde_json::json!({"sequence": sequence, "kind": "clipboard", "epoch": epoch, "error": message(error)}),
                        };
                        slot.json(notify, sequence, body).await?;
                    }
                }
            }
        }
    };
    let result = tokio::select! { biased;
        error = &mut stopped => Err(error),
        outcome = operation => outcome,
    };
    prompt.cancel();
    if result.is_ok() {
        session.finish().await.map_err(|error| message(error).into())
    } else {
        session.close().await;
        result.map_err(|error| message(error).into())
    }
}

pub(super) fn message(error: Error) -> &'static str {
    match error {
        Error::Bounds => "RDP 화면 또는 입력 범위가 올바르지 않습니다",
        Error::Busy => "RDP 세션이 작업 중입니다",
        Error::ChannelUnavailable => "서버가 이 RDP 채널을 열지 않았습니다",
        Error::Clipboard => "RDP 클립보드 교환 실패",
        Error::Closed => CLOSED,
        Error::Credentials => "RDP 계정 입력이 취소되었거나 올바르지 않습니다",
        Error::InvalidServerName => "RDP 서버 DNS 이름 또는 포트가 올바르지 않습니다",
        Error::InvalidForwardDestination => "RDP 터널 목적지가 올바르지 않습니다",
        Error::Tcp => "RDP TCP 연결 실패",
        Error::Negotiation => "RDP 서버와 NLA를 협상할 수 없습니다",
        Error::TlsOnlyRequired => "RDP 서버가 NLA를 지원하지 않습니다. 연결 설정에서 'NLA를 지원하지 않는 서버(TLS 전용) 허용'을 켜야 합니다",
        Error::TrustStore => "RDP 신뢰 저장소를 읽을 수 없습니다",
        Error::TlsIdentity => "RDP 서버 TLS 인증서 검증 실패",
        Error::Authentication => "RDP NLA 계정 인증 실패",
        Error::Activation => "RDP 화면 초기화 실패",
        Error::Protocol => "RDP 응답 형식 오류",
        Error::StaleDisplay => "RDP 화면 세대가 교체되었습니다",
        Error::Transport => "RDP 전송 연결이 끊겼습니다",
        Error::Timeout => "RDP 응답 시간 초과",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rdp_close_waits_for_session_shutdown_and_lease_release() {
        use std::task::{Context, Waker};

        let slot = Slot::new(1);
        let (closed, done) = oneshot::channel();
        *slot.cleanup.lock().await = Cleanup::Running(done);
        slot.close();
        assert!(slot.life.is_cancelled());
        let mut close = std::pin::pin!(slot.finish_close());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(close.as_mut().poll(&mut cx).is_pending());
        assert!(closed.send(Ok(())).is_ok());
        assert!(matches!(close.as_mut().poll(&mut cx), std::task::Poll::Ready(Ok(()))));
    }

    #[test]
    fn rdp_approval_separates_forward_destination_from_tls_dns() {
        let mut draft: Draft = serde_json::from_value(serde_json::json!({
            "host": "rdp.example.invalid", "port": 3390,
            "source": { "tabHandle": "owned-tab", "tunnelId": "7", "targetId": "ssh-server",
                "destination": { "host": "::1", "port": 3390 } }
        })).unwrap();
        assert_eq!(draft.approval_text(&draft.host).unwrap(),
            "경로: R01 SSH 전달\nSSH 대상 ID: ssh-server\n포워드 목적지: [::1]:3390\nTLS 인증 DNS: rdp.example.invalid\n로그인: NLA");
        draft.port = 3389;
        assert_eq!(draft.approval_text(&draft.host), Err(Error::InvalidForwardDestination));
        assert!(serde_json::from_value::<Draft>(serde_json::json!({
            "host": "rdp.example.invalid", "port": 3390, "source": null
        })).is_err());
        let direct: Draft = serde_json::from_value(serde_json::json!({
            "host": "rdp.example.invalid", "port": 3389
        })).unwrap();
        assert_eq!(direct.approval_text(&direct.host).unwrap(),
            "서버: rdp.example.invalid:3389\n경로: 직접\n로그인: NLA");
        let opt_in: Draft = serde_json::from_value(serde_json::json!({
            "host": "rdp.example.invalid", "port": 3389, "tlsOnly": true
        })).unwrap();
        assert_eq!(opt_in.approval_text(&opt_in.host).unwrap(),
            "서버: rdp.example.invalid:3389\n경로: 직접\n로그인: NLA, 미지원 시 TLS 전용(표준 RDP 보안 없음)");
        let forwarded_opt_in: Draft = serde_json::from_value(serde_json::json!({
            "host": "rdp.example.invalid", "port": 3390, "tlsOnly": true,
            "source": { "tabHandle": "owned-tab", "tunnelId": "7", "targetId": "ssh-server",
                "destination": { "host": "127.0.0.1", "port": 3390 } }
        })).unwrap();
        assert_eq!(forwarded_opt_in.approval_text(&forwarded_opt_in.host).unwrap(),
            "경로: R01 SSH 전달\nSSH 대상 ID: ssh-server\n포워드 목적지: 127.0.0.1:3390\nTLS 인증 DNS: rdp.example.invalid\n로그인: NLA, 미지원 시 TLS 전용(표준 RDP 보안 없음)");
    }

    #[test]
    fn rdp_document_retirement_discards_unclaimed_output_and_rejects_old_generations() {
        let sessions = Sessions::default();
        let slot = Arc::new(Slot::new(1));
        let id = Identity { session_id: "tab".into(), generation: 1 };
        let (done, mut acknowledged) = oneshot::channel();
        {
            let mut state = sessions.0.lock().unwrap();
            state.proof = Some(DocumentProof { key: "old".into(), origin: "app".into(), tunnel_epoch: 2 });
            state.tabs.insert(id.session_id.clone(), slot.clone());
            assert!(state.get(&id, 0).is_some());
            assert!(state.get(&Identity { session_id: id.session_id.clone(), generation: 2 }, 0).is_none());
        }
        *slot.mailbox.lock().unwrap() = Some(Packet { ticket: "one".into(), sequence: 2, offset: 1,
            body: Some(("application/octet-stream", vec![7])), done });
        assert!(slot.ack(2, 1).is_none());
        assert!(matches!(acknowledged.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        sessions.retire();
        assert!(matches!(acknowledged.try_recv(), Err(oneshot::error::TryRecvError::Closed)));
        let state = sessions.0.lock().unwrap();
        assert!(state.proof.is_none() && state.get(&id, 0).is_none());
        assert!(slot.life.is_cancelled());
    }
}
