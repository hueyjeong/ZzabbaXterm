mod input;

use std::{collections::HashMap, io, sync::{Arc, Mutex}, task::Poll, time::Duration};

use ironrdp::connector::DesktopSize;
use serde::{Deserialize, Serialize};
use tauri::{Manager, UriSchemeContext, UriSchemeResponder, Webview, WebviewWindow,
    http::{Request, Response}, ipc::{Channel, CommandArg, CommandItem, Invoke, InvokeError}};
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{connections::{SavedRoute, profile::valid_id}, tunnels::{Destination, ipc::{DesktopCleanupError, DesktopLease, Protocol, TunnelTabs, describe_route, load_route}}};
use super::{Error, Event, Session, native, route};
use input::{Command, Controls};

const COMMANDS: [&str; 6] = ["rdp_prepare", "rdp_connect", "rdp_input", "rdp_input_batch", "rdp_ack", "rdp_close"];
const CLOSED: &str = "RDP 세션이 닫혔습니다";
const MAX_SAFE: u64 = (1 << 53) - 1;
const CHUNK: usize = 256 * 1024;
const MAX_JSON: usize = 6 * super::clipboard::MAX_TEXT + 4096;
const MAX_BATCH: usize = 64;
/// 묶음 처리 중 이 개수마다 controls 잠금을 놓아 화면 이벤트 처리가 오래 기다리지 않게 한다.
const LOCK_SPAN: usize = 16;

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
    /// 탭 화면 영역의 장치 픽셀 크기와 배율(%)이다. 없으면 1024×768로 시작한다.
    #[serde(default)]
    size: Option<DisplaySize>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DisplaySize { width: u16, height: u16, scale: u32 }

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
    /// 첫 연결 크기도 해상도 변경과 같은 범위를 쓴다. 서버 최대 면적은 연결 뒤에야 알 수 있다.
    fn desktop(&self) -> Result<(DesktopSize, u32), Error> {
        let Some(DisplaySize { width, height, scale }) = self.size else { return Ok((DesktopSize { width: 1024, height: 768 }, 0)) };
        super::display::check(width, height, u64::MAX)?;
        Ok((DesktopSize { width, height }, scale))
    }

    #[cfg(test)]
    fn approval_text(&self, dns: &str) -> Result<String, Error> {
        self.route_approval_text(dns, false)
    }

    /// SSH 경로는 서버 루프백만, SSM 경로는 노드 자신(루프백) 또는 TLS 인증 이름(DNS 이름 또는 IP 주소)의 원격 호스트만 받는다.
    fn route_approval_text(&self, dns: &str, ssm: bool) -> Result<String, Error> {
        if route::server_name(dns).is_none() || self.port == 0 { return Err(Error::InvalidServerName); }
        let login = if self.tls_only { "NLA, 미지원 시 TLS 전용(표준 RDP 보안 없음)" } else { "NLA" };
        let Some(source) = &self.source else {
            return Ok(format!("서버: {}\n경로: 직접\n로그인: {login}", route::host_port(dns, self.port)));
        };
        let ForwardDestination { host, port } = &source.destination;
        let loopback = matches!(host.as_str(), "127.0.0.1" | "::1");
        if !(loopback || (ssm && host.eq_ignore_ascii_case(dns))) || *port != self.port || !valid_id(&source.target_id) {
            return Err(Error::InvalidForwardDestination);
        }
        let destination = route::host_port(host, *port);
        let (via, target) = if ssm { ("SSM", "SSM 경로 대상 ID") } else { ("SSH", "SSH 대상 ID") };
        Ok(format!("경로: R01 {via} 전달\n{target}: {}\n포워드 목적지: {destination}\nTLS 인증 이름: {dns}\n로그인: {login}", source.target_id))
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
                match input_batch(&slot, vec![command]).await.map_err(|error| InvokeError::from(Failure::from(error)))?.pop() {
                    Some(Reply::Failed(error)) => Err(InvokeError::from(Failure::from(error))),
                    Some(reply) => Ok(reply),
                    None => Err(InvokeError::from(CLOSED)),
                }
            });
        }
        "rdp_input_batch" => {
            let commands = read!(Vec<Command>, "inputs");
            drop(state);
            invoke.resolver.respond_async(async move { input_batch(&slot, commands).await.map_err(|error| InvokeError::from(Failure::from(error))) });
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

async fn run(window: &WebviewWindow, slot: &Slot, notify: &Channel<String>, draft: Draft, tunnel_epoch: u64, approval: Arc<()>, lease: &mut Option<DesktopLease>) -> Result<(), Failure> {
    let life = &slot.life;
    let prompt = life.child_token();
    let _prompt_lifetime = prompt.clone().drop_guard();
    let dns = draft.host.to_ascii_lowercase();
    let saved = match &draft.source {
        Some(source) => {
            let directory = window.path().app_config_dir().map_err(|error| error.to_string())?;
            let target_id = source.target_id.clone();
            Some(tauri::async_runtime::spawn_blocking(move || load_route(&directory, &target_id))
                .await.map_err(|_| message(Error::Transport))??)
        }
        None => None,
    };
    let description = draft.route_approval_text(&dns, matches!(saved, Some(SavedRoute::Ssm(_)))).map_err(message)?;
    let trust_path = window.path().app_config_dir().map_err(|error| error.to_string())?.join("rdp-trust.json");
    let (size, scale) = draft.desktop().map_err(message)?;
    let (target, loss, target_name, endpoint, pin_route) = if let (Some(source), Some(saved)) = (draft.source, saved) {
        let directory = window.path().app_config_dir().map_err(|error| error.to_string())?;
        let destination = Destination { host: source.destination.host, port: source.destination.port };
        window.state::<TunnelTabs>().preflight_desktop(tunnel_epoch, (&source.tab_handle, &source.tunnel_id),
            &source.target_id, &destination, Protocol::Rdp)?;
        let description = format!("{description}\n{}", describe_route(&saved, &directory));
        native::confirm(window.clone(), description, prompt.clone(), approval.clone(), notify.clone()).await.map_err(message)?;
        let target_name = format!("{dns} (TLS 인증 이름)");
        // 전달 경로의 endpoint는 루프백이라 경로마다 같을 수 있다. 인증서 고정은 경로 대상 ID로 구분한다.
        let pin_route = Some(source.target_id.clone());
        let caller = window.clone();
        let live = prompt.clone();
        // 발급 중 취소되어도 작업을 기다려 인계 소켓과 임대를 이 경로에서 정리한다.
        let (issued, approved_destination) = tauri::async_runtime::spawn_blocking(move || {
            if live.is_cancelled() { return Err(CLOSED.into()); }
            let lease = caller.state::<TunnelTabs>().open_desktop(tunnel_epoch, &directory,
                (&source.tab_handle, &source.tunnel_id), &source.target_id, &destination, Protocol::Rdp, &saved)?;
            Ok::<_, String>((lease, destination))
        }).await.map_err(|_| message(Error::Transport))??;
        let lease = lease.insert(issued);
        if life.is_cancelled() { return Err(CLOSED.into()); }
        let loss = lease.loss();
        let target = route::forwarded(dns.clone(), &approved_destination, size, scale, lease, draft.tls_only).map_err(message)?;
        let endpoint = route::host_port(&approved_destination.host, approved_destination.port);
        (target, Some(loss), target_name, endpoint, pin_route)
    } else {
        native::confirm(window.clone(), description, prompt.clone(), approval.clone(), notify.clone()).await.map_err(message)?;
        let target_name = route::host_port(&dns, draft.port);
        let endpoint = target_name.clone();
        (route::direct(dns.clone(), draft.port, size, scale, draft.tls_only), None, target_name, endpoint, None)
    };
    let stopped = async {
        tokio::select! { biased;
            () = life.cancelled() => Error::Closed,
            () = async { match loss { Some(loss) => loss.cancelled().await, None => std::future::pending().await } } => Error::Transport,
        }
    };
    tokio::pin!(stopped);
    let config = window.path().app_config_dir().map_err(|error| error.to_string())?;
    let accounts = (config.join("rdp-accounts.json"), dns.clone(), endpoint.clone(), pin_route.clone());
    let remembered = super::accounts::load(&accounts.0, &accounts.1, &accounts.2, accounts.3.as_deref());
    let entered = std::sync::Mutex::new(None);
    let trust = super::trust::Trust {
        path: trust_path,
        dns: dns.clone(),
        endpoint,
        route: pin_route,
        approve: {
            let window = window.clone();
            let live = prompt.clone();
            let approval = approval.clone();
            let notify = notify.clone();
            move |request| {
                let window = window.clone();
                let live = live.clone();
                let approval = approval.clone();
                let notify = notify.clone();
                async move { native::trust(window, request, live, approval, notify).await }
            }
        },
    };
    let mut session = tokio::select! { biased;
        error = &mut stopped => {
            prompt.cancel();
            return Err(error.into());
        },
        session = Session::open(target, tokio_rustls::rustls::RootCertStore::empty(), trust,
            || async {
                let account = native::account(window.clone(), target_name, remembered, prompt.clone(), approval.clone(), notify.clone()).await?;
                *entered.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(account.login());
                Ok(account)
            }, AuthIo::new(slot, lease.as_ref())) => session.map_err(message)?,
    };
    // 세션이 열린 뒤에만 계정 이름을 기억한다. 저장 실패는 연결을 막지 않는다.
    if let Some(login) = entered.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner) {
        let _ = super::accounts::save(&accounts.0, &accounts.1, &accounts.2, accounts.3.as_deref(), &login);
    }
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
        session.finish().await.map_err(Failure::from)
    } else {
        session.close().await;
        result.map_err(Failure::from)
    }
}

/// 연결·입력 실패를 탭에 알리는 값이다. `kind: "transport"`는 전송 연결이 끊긴 경우로, 터널 경로는 다시 열어야 한다.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Failure { kind: Option<&'static str>, message: String }

impl Serialize for Failure {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        if let Some(kind) = self.kind { map.serialize_entry("kind", kind)?; }
        map.serialize_entry("message", &self.message)?;
        map.end()
    }
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self { kind: (error == Error::Transport).then_some("transport"), message: message(error).into() }
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self { Self { kind: None, message } }
}

impl From<&'static str> for Failure {
    fn from(message: &'static str) -> Self { message.to_owned().into() }
}

/// 입력 한 건의 결과다. `busy`는 순번을 쓰지 않았으므로 같은 순번으로 다시 보낸다.
#[derive(Debug, PartialEq, Eq)]
enum Reply { Ok, Stale, Busy, Failed(Error) }

impl Serialize for Reply {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Self::Ok => serializer.serialize_str("ok"),
            Self::Stale => serializer.serialize_str("stale"),
            Self::Busy => serializer.serialize_str("busy"),
            Self::Failed(error) => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("error", message(*error))?;
                if *error == Error::Transport { map.serialize_entry("kind", "transport")?; }
                map.end()
            }
        }
    }
}

impl From<Result<(), Error>> for Reply {
    fn from(result: Result<(), Error>) -> Self {
        match result {
            Ok(()) => Self::Ok,
            Err(Error::StaleDisplay) => Self::Stale,
            Err(Error::Busy) => Self::Busy,
            Err(error) => Self::Failed(error),
        }
    }
}

/// 입력을 순서대로 처리하고 `busy`에서 멈춘다. 남은 항목은 같은 순번으로 다시 보내야 한다.
async fn input_batch(slot: &Slot, commands: Vec<Command>) -> Result<Vec<Reply>, Error> {
    if commands.is_empty() || commands.len() > MAX_BATCH { return Err(Error::Bounds); }
    let mut replies = Vec::with_capacity(commands.len());
    let mut commands = commands.into_iter().peekable();
    while commands.peek().is_some() {
        let mut controls = tokio::select! { biased;
            () = slot.life.cancelled() => return Err(Error::Closed),
            controls = slot.controls.lock() => controls,
        };
        for command in commands.by_ref().take(LOCK_SPAN) {
            let reply = Reply::from(controls.send(&slot.life, command).await);
            let busy = reply == Reply::Busy;
            replies.push(reply);
            if busy { return Ok(replies); }
        }
    }
    Ok(replies)
}

pub(super) fn message(error: Error) -> &'static str {
    match error {
        Error::Bounds | Error::OutOfImage { .. } => "RDP 화면 또는 입력 범위가 올바르지 않습니다",
        Error::Busy => "RDP 세션이 작업 중입니다",
        Error::DisplayLimit => "서버가 허용하는 최대 화면 면적을 넘습니다",
        Error::CopyPending => "RDP 복사가 진행 중입니다",
        Error::ChannelUnavailable => "서버가 이 RDP 채널을 열지 않았습니다",
        Error::Clipboard => "RDP 클립보드 교환 실패",
        Error::Closed => CLOSED,
        Error::Credentials => "RDP 계정 입력이 취소되었거나 올바르지 않습니다",
        Error::InvalidServerName => "RDP 서버 이름 또는 포트가 올바르지 않습니다",
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

    fn input(sequence: u64, action: serde_json::Value) -> Command {
        serde_json::from_value(serde_json::json!({ "sequence": sequence, "epoch": 1, "displayGeneration": 1, "action": action })).unwrap()
    }

    #[test]
    fn rdp_input_replies_map_busy_and_stale_without_failing() {
        assert_eq!(Reply::from(Err(Error::Busy)), Reply::Busy);
        assert_eq!(Reply::from(Err(Error::StaleDisplay)), Reply::Stale);
        assert_eq!(Reply::from(Err(Error::CopyPending)), Reply::Failed(Error::CopyPending));
        assert_eq!(serde_json::to_value([Reply::Ok, Reply::Stale, Reply::Busy, Reply::Failed(Error::CopyPending), Reply::Failed(Error::Transport)]).unwrap(),
            serde_json::json!(["ok", "stale", "busy", { "error": message(Error::CopyPending) },
                { "error": message(Error::Transport), "kind": "transport" }]));
        assert_eq!(serde_json::to_value(Failure::from(Error::Transport)).unwrap(),
            serde_json::json!({ "kind": "transport", "message": message(Error::Transport) }));
        assert_eq!(serde_json::to_value(Failure::from(CLOSED)).unwrap(), serde_json::json!({ "message": CLOSED }));
    }

    #[tokio::test]
    async fn rdp_input_batch_stops_at_busy_and_accepts_the_same_sequence_again() {
        use super::super::{Input, InputHandle, SessionId};

        let slot = Slot::new(1);
        let id = SessionId(1);
        let (sender, mut commands) = tokio::sync::mpsc::channel(1);
        slot.controls.lock().await.display(id, InputHandle { id, input: sender.clone() }, (640, 480), 1);
        let key = |sequence, down| input(sequence, serde_json::json!({ "kind": "key", "code": 0x1e, "extended": false, "down": down }));
        assert_eq!(input_batch(&slot, Vec::new()).await, Err(Error::Bounds));
        assert_eq!(input_batch(&slot, (1..=65).map(|n| key(n, true)).collect()).await, Err(Error::Bounds));

        // 세션 작업이 명령을 꺼내지 않아 큐가 찬 상태다. 두 번째 항목은 busy이고 세 번째는 처리하지 않는다.
        let (filler, _) = oneshot::channel();
        sender.try_send((None, Input::ReadClipboardText, filler)).unwrap();
        let replies = input_batch(&slot, vec![input(1, serde_json::json!({ "kind": "active", "active": true })), key(2, true), key(3, false)]).await;
        assert_eq!(replies, Ok(vec![Reply::Ok, Reply::Busy]));

        let written = tokio::spawn(async move {
            let mut keys = Vec::new();
            while let Some((_, input, ack)) = commands.recv().await {
                if let Input::ScanCode { code, released, .. } = input { keys.push((code, released)); }
                let _ = ack.send(Ok(()));
            }
            keys
        });
        assert_eq!(input_batch(&slot, vec![key(2, true), key(3, false)]).await, Ok(vec![Reply::Ok, Reply::Ok]));
        assert_eq!(input_batch(&slot, vec![key(3, true)]).await, Ok(vec![Reply::Failed(Error::Closed)]));
        drop(slot);
        drop(sender);
        assert_eq!(written.await.unwrap(), vec![(0x1e, false), (0x1e, true)]);
    }

    #[test]
    fn rdp_approval_separates_forward_destination_from_tls_dns() {
        let mut draft: Draft = serde_json::from_value(serde_json::json!({
            "host": "rdp.example.invalid", "port": 3390,
            "source": { "tabHandle": "owned-tab", "tunnelId": "7", "targetId": "ssh-server",
                "destination": { "host": "::1", "port": 3390 } }
        })).unwrap();
        assert_eq!(draft.approval_text(&draft.host).unwrap(),
            "경로: R01 SSH 전달\nSSH 대상 ID: ssh-server\n포워드 목적지: [::1]:3390\nTLS 인증 이름: rdp.example.invalid\n로그인: NLA");
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
            "경로: R01 SSH 전달\nSSH 대상 ID: ssh-server\n포워드 목적지: 127.0.0.1:3390\nTLS 인증 이름: rdp.example.invalid\n로그인: NLA, 미지원 시 TLS 전용(표준 RDP 보안 없음)");
    }

    #[test]
    fn rdp_connect_uses_the_tab_size_and_scale_within_display_bounds() {
        let draft = |size: serde_json::Value| -> Draft {
            serde_json::from_value(serde_json::json!({ "host": "rdp.example.invalid", "port": 3389, "size": size })).unwrap()
        };
        let default: Draft = serde_json::from_value(serde_json::json!({ "host": "rdp.example.invalid", "port": 3389 })).unwrap();
        assert_eq!(default.desktop().map(|(size, scale)| (size.width, size.height, scale)), Ok((1024, 768, 0)));
        let fitted = draft(serde_json::json!({ "width": 1474, "height": 812, "scale": 125 }));
        assert_eq!(fitted.desktop().map(|(size, scale)| (size.width, size.height, scale)), Ok((1474, 812, 125)));
        for size in [(1475, 812), (198, 812), (1474, 199), (4098, 812)] {
            let invalid = draft(serde_json::json!({ "width": size.0, "height": size.1, "scale": 100 }));
            assert_eq!(invalid.desktop().map(|_| ()), Err(Error::Bounds), "{size:?}");
        }
        assert!(serde_json::from_value::<Draft>(serde_json::json!({
            "host": "rdp.example.invalid", "port": 3389, "size": { "width": 1280, "height": 720 }
        })).is_err());
    }

    #[test]
    fn rdp_approval_accepts_ip_server_names_and_brackets_ipv6() {
        let draft = |host: &str, destination: Option<&str>| -> Draft {
            let mut value = serde_json::json!({ "host": host, "port": 3389 });
            if let Some(destination) = destination {
                value["source"] = serde_json::json!({ "tabHandle": "owned-tab", "tunnelId": "7", "targetId": "ssm-node",
                    "destination": { "host": destination, "port": 3389 } });
            }
            serde_json::from_value(value).unwrap()
        };
        let direct = draft("10.0.1.25", None);
        assert_eq!(direct.approval_text(&direct.host).unwrap(), "서버: 10.0.1.25:3389\n경로: 직접\n로그인: NLA");
        let direct = draft("2001:db8::25", None);
        assert_eq!(direct.approval_text(&direct.host).unwrap(), "서버: [2001:db8::25]:3389\n경로: 직접\n로그인: NLA");
        // SSM 원격 호스트는 TLS 인증 이름과 같은 IP 주소여야 한다.
        let ssm = draft("2001:db8::25", Some("2001:db8::25"));
        assert_eq!(ssm.route_approval_text(&ssm.host, true).unwrap(),
            "경로: R01 SSM 전달\nSSM 경로 대상 ID: ssm-node\n포워드 목적지: [2001:db8::25]:3389\nTLS 인증 이름: 2001:db8::25\n로그인: NLA");
        let other = draft("10.0.1.25", Some("10.0.1.26"));
        assert_eq!(other.route_approval_text(&other.host, true), Err(Error::InvalidForwardDestination));
        // 리졸버가 IP로 해석하는 숫자 꼬리 이름, 비표준 IPv6 표기, 지정되지 않은 주소는 받지 않는다.
        for host in ["2852039166", "127.1", "0x7f000001", "2001:0db8::25", "[2001:db8::25]", "0.0.0.0", "::"] {
            let invalid = draft(host, None);
            assert_eq!(invalid.approval_text(&invalid.host), Err(Error::InvalidServerName), "{host}");
        }
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
