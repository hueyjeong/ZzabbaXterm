use std::{collections::HashMap, sync::{Arc, Mutex}};

use serde::{Deserialize, Serialize};
use tauri::{Manager, Webview, WebviewWindow, ipc::{Channel, CommandArg, CommandItem, Invoke, InvokeError}};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::tunnels::ipc::{DesktopCleanupError, DesktopLease, TunnelTabs};

use super::{Change, Error, Event, Input, ResizeResult, Size, Tab, approved::{Draft, route_lost}, lifetime::Lifetime, output::{Mailbox, Output}, presentation::{Image, Surface}};

pub(crate) const COMMANDS: [&str; 5] = ["vnc_create", "vnc_connect", "vnc_input", "vnc_ack", "vnc_close"];
const CLOSED: &str = "VNC 세션이 닫혔습니다";
const CLEANUP_UNCONFIRMED: &str = "종료 확인 불가";
const MAX_SAFE: u64 = (1 << 53) - 1;

#[derive(Default)]
pub(crate) struct Sessions(Mutex<Registry>);

#[derive(Default)]
struct Registry {
    document: u64,
    document_proof: Option<(String, String, u64)>,
    closed: bool,
    tabs: HashMap<String, Slot>,
    approving: Option<Arc<()>>,
}

#[derive(Default)]
struct Slot {
    close_gate: Arc<AsyncMutex<()>>,
    finished: Option<CancellationToken>,
    generation: u64,
    current: Option<Running>,
    closing_owner: bool,
    cleanup: Option<Cleanup>,
}

enum Cleanup {
    Ticket(DesktopCleanupError),
    WorkerFailed,
}

struct Running {
    life: Arc<Lifetime>,
    approval: Arc<()>,
    input: mpsc::Sender<Command>,
    mailbox: Arc<Mailbox>,
}

impl Drop for Running {
    fn drop(&mut self) { self.life.revoke(); self.mailbox.clear(); }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Owner { tab: String, document: u64 }

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Identity { session_id: String, generation: u64 }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Command {
    session_id: String,
    generation: u64,
    epoch: u64,
    sequence: u64,
    action: Action,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum Action {
    Key { keysym: u32, down: bool },
    Pointer { x: u16, y: u16, buttons: u8 },
    Release,
    CutText { text: String },
    Resize { width: u16, height: u16 },
}

impl Action {
    fn input(self) -> Result<Input, Error> {
        Ok(match self {
            Self::Key { keysym, down } if keysym > 0 && keysym <= 0x0110_ffff => Input::Key { keysym, down },
            Self::Pointer { x, y, buttons } if buttons & !7 == 0 => Input::Pointer { x, y, buttons },
            Self::Release => Input::Release,
            Self::CutText { text } if text.len() <= super::wire::MAX_TEXT * 2 => Input::CutText(text),
            Self::Resize { width, height } => Input::Resize(Size { width, height }),
            _ => return Err(Error::LengthLimit),
        })
    }
}

impl Registry {
    fn document_for(&self, proof: &str) -> Option<(u64, u64)> {
        let (expected, _, tunnel_epoch) = self.document_proof.as_ref()?;
        (!self.closed && expected == proof).then_some((self.document, *tunnel_epoch))
    }

    fn slot(&mut self, owner: &Owner, document: u64) -> Result<&mut Slot, &'static str> {
        if self.closed || document != self.document || document != owner.document {
            return Err(CLOSED);
        }
        self.tabs.get_mut(&owner.tab).ok_or(CLOSED)
    }
}

impl Sessions {
    pub(crate) fn retire(&self, closed: bool) {
        let mut state = self.0.lock().unwrap();
        state.closed |= closed;
        state.document = state.document.saturating_add(1);
        state.document_proof = None;
        state.tabs.clear();
    }

    pub(super) fn packet<T>(&self, proof: &str, origin: &str, ticket: &str, deliver: impl FnOnce(bool, Vec<u8>) -> T) -> Option<T> {
        let state = self.0.lock().unwrap();
        if state.document_for(proof).is_none() || state.document_proof.as_ref()?.1 != origin { return None; }
        let (raw, bytes) = state.tabs.values().filter_map(|slot| slot.current.as_ref())
            .find_map(|active| active.life.live(|| active.mailbox.take(ticket)).flatten())?;
        // 큰 본문을 응답으로 넘기는 동안 다른 탭의 명령이 전역 잠금을 기다리지 않게 먼저 놓는다.
        drop(state);
        Some(deliver(raw, bytes))
    }
}

pub(super) fn opaque_id() -> Result<String, &'static str> {
    let mut id = [0_u8; 16];
    getrandom::fill(&mut id).map_err(|_| "VNC 탭 ID를 만들 수 없습니다")?;
    Ok(format!("{:032x}", u128::from_ne_bytes(id)))
}

pub(crate) fn issue_document(webview: &Webview, url: &tauri::Url) {
    let bundled = if cfg!(windows) {
        url.host_str() == Some("tauri.localhost") && matches!(url.scheme(), "http" | "https") && url.port().is_none()
    } else {
        url.host_str() == Some("localhost") && url.scheme() == "tauri" && url.port().is_none()
    };
    let development = tauri::is_dev() && webview.config().build.dev_url.as_ref()
        .is_some_and(|app| app.origin() == url.origin());
    if !(bundled || development) || webview.url().as_ref().ok() != Some(url) { return; }
    let sessions = webview.state::<Sessions>();
    let mut state = sessions.0.lock().unwrap();
    if state.closed || state.document_proof.is_some() { return; }
    let tunnel_epoch = webview.state::<TunnelTabs>().epoch();
    let Ok(proof) = opaque_id() else { return; };
    let generation = state.document;
    let origin = if bundled && !cfg!(windows) { "tauri://localhost".to_owned() } else { url.origin().ascii_serialization() };
    let url = serde_json::to_string(url.as_str()).unwrap();
    let script = format!("if (window === top && location.href === {url} && (window.__ZZABBA_VNC_DOCUMENT__?.generation ?? -1) < {generation}) {{ window.__ZZABBA_VNC_DOCUMENT__ = Object.freeze({{ generation: {generation}, proof: '{proof}' }}); }}");
    if webview.eval(script).is_ok() { state.document_proof = Some((proof, origin, tunnel_epoch)); }
}

pub(crate) fn dispatch(invoke: Invoke) {
    let caller = invoke.message.webview_ref();
    if caller.label() != "main" || caller.window_ref().label() != "main" || invoke.acl.is_none() {
        invoke.resolver.reject(CLOSED);
        return;
    }
    let Some(command) = COMMANDS.into_iter().find(|name| *name == invoke.message.command()) else {
        invoke.resolver.reject(CLOSED);
        return;
    };
    let sessions = caller.state::<Sessions>();
    let (document, tunnel_epoch) = {
        let state = sessions.0.lock().unwrap();
        let proof = invoke.message.headers().get("x-zzabba-vnc-document").and_then(|header| header.to_str().ok());
        let Some(epochs) = proof.and_then(|proof| state.document_for(proof)) else {
            invoke.resolver.reject(CLOSED);
            return;
        };
        epochs
    };
    let arg = |key| CommandItem { name: command, key, plugin: None, message: &invoke.message, acl: &invoke.acl };
    macro_rules! read {
        ($ty:ty, $key:literal) => {
            match <$ty>::from_command(arg($key)) {
                Ok(value) => value,
                Err(error) => { invoke.resolver.invoke_error(error); return; }
            }
        };
    }
    let window = read!(WebviewWindow, "webview");
    if command == "vnc_create" {
        let result = (|| {
            let sessions = window.state::<Sessions>();
            let mut state = sessions.0.lock().unwrap();
            if state.closed || state.document != document || document > MAX_SAFE || state.tabs.len() >= 8 {
                return Err("VNC 탭을 더 열 수 없습니다");
            }
            let owner = Owner { tab: opaque_id()?, document };
            state.tabs.insert(owner.tab.clone(), Slot::default());
            Ok(owner)
        })();
        match result { Ok(owner) => invoke.resolver.resolve(owner), Err(e) => invoke.resolver.reject(e) }
        return;
    }
    let owner = read!(Owner, "owner");
    if command == "vnc_connect" {
        let draft = read!(Draft, "draft");
        let output = read!(Channel<String>, "output");
        let sessions = window.state::<Sessions>();
        let result = prepare(&sessions, &owner, document);
        match result {
            Ok(work) => invoke.resolver.respond_async(async move {
                connect(&window, owner, draft, output, work, tunnel_epoch).await.map_err(InvokeError::from)
            }),
            Err(error) => invoke.resolver.reject(error),
        }
        return;
    }
    if command == "vnc_close" {
        let generation = read!(Option<u64>, "generation");
        let retry = read!(Option<bool>, "retry").unwrap_or(false);
        let gate = match begin_close(&sessions, &owner, document, generation) {
            Ok(gate) => gate,
            Err(error) => { invoke.resolver.reject(error); return; }
        };
        invoke.resolver.respond_async(async move {
            close(&window.state::<Sessions>(), &owner, document, generation, retry, gate).await.map_err(InvokeError::from)
        });
        return;
    }
    let sessions = window.state::<Sessions>();
    let mut state = sessions.0.lock().unwrap();
    let result = state.slot(&owner, document).and_then(|slot| match command {
        "vnc_ack" => {
            let generation = u64::from_command(arg("generation")).map_err(|_| CLOSED)?;
            let sequence = u64::from_command(arg("sequence")).map_err(|_| CLOSED)?;
            let offset = u32::from_command(arg("offset")).map_err(|_| CLOSED)?;
            slot.current.as_ref().filter(|_| slot.generation == generation).ok_or(CLOSED)?
                .mailbox.ack(sequence, offset).ok_or(CLOSED)?;
            Ok(())
        }
        "vnc_input" => {
            let input = Command::from_command(arg("input")).map_err(|_| CLOSED)?;
            let running = slot.current.as_ref().filter(|_| slot.generation == input.generation).ok_or(CLOSED)?;
            running.input.try_send(input).map_err(|_| "VNC 입력 큐가 가득 찼거나 닫혔습니다")
        }
        _ => Err(CLOSED),
    });
    match result {
        Ok(()) => invoke.resolver.resolve(()),
        Err(error) => invoke.resolver.reject(error),
    }
}

fn begin_close(sessions: &Sessions, owner: &Owner, document: u64, generation: Option<u64>) -> Result<Arc<AsyncMutex<()>>, &'static str> {
    let mut state = sessions.0.lock().unwrap();
    let slot = state.slot(owner, document)?;
    if generation.is_some_and(|generation| slot.generation != generation) { return Err(CLOSED); }
    if generation.is_none() { slot.closing_owner = true; }
    slot.current = None;
    Ok(slot.close_gate.clone())
}

async fn close(sessions: &Sessions, owner: &Owner, document: u64, generation: Option<u64>, retry: bool,
               gate: Arc<AsyncMutex<()>>) -> Result<bool, &'static str> {
    let _guard = gate.lock_owned().await;
    let (finished, retry) = {
        let mut state = sessions.0.lock().unwrap();
        let slot = state.slot(owner, document)?;
        if generation.is_some_and(|generation| slot.generation != generation) { return Err(CLOSED); }
        let retry = if retry && generation.is_none() && matches!(slot.cleanup, Some(Cleanup::Ticket(_))) { slot.cleanup.take() } else { None };
        (slot.finished.clone(), retry)
    };
    if let Some(finished) = finished { finished.cancelled().await; }
    let retry = if let Some(Cleanup::Ticket(mut ticket)) = retry {
        Some(tauri::async_runtime::spawn_blocking(move || {
            ticket.retry().err().map(|_| Cleanup::Ticket(ticket))
        }).await.unwrap_or(Some(Cleanup::WorkerFailed)))
    } else { None };
    let mut state = sessions.0.lock().unwrap();
    let slot = state.slot(owner, document)?;
    if let Some(retry) = retry { slot.cleanup = retry; }
    if slot.cleanup.is_some() { return Ok(false); }
    if generation.is_none() { state.tabs.remove(&owner.tab); }
    Ok(true)
}

async fn release(lease: Option<DesktopLease>) -> Option<Cleanup> {
    let lease = lease?;
    tauri::async_runtime::spawn_blocking(move || lease.release().err().map(Cleanup::Ticket))
        .await.unwrap_or(Some(Cleanup::WorkerFailed))
}

struct Work {
    previous: Option<CancellationToken>,
    finished: DropGuard,
    generation: u64,
    life: Arc<Lifetime>,
    approval: Arc<()>,
    inputs: mpsc::Receiver<Command>,
    mailbox: Arc<Mailbox>,
}

fn prepare(sessions: &Sessions, owner: &Owner, document: u64) -> Result<Work, &'static str> {
    let mut state = sessions.0.lock().unwrap();
    if state.approving.is_some() { return Err("VNC 연결 승인 대기 중입니다"); }
    let slot = state.slot(owner, document)?;
    if slot.closing_owner || slot.cleanup.is_some() { return Err(CLEANUP_UNCONFIRMED); }
    slot.current = None;
    slot.generation = slot.generation.checked_add(1).filter(|n| *n <= MAX_SAFE).ok_or(CLOSED)?;
    let (input, inputs) = mpsc::channel(8);
    let life = Arc::new(Lifetime::default());
    let approval = Arc::new(());
    let mailbox = Arc::new(Mailbox::default());
    slot.current = Some(Running { life: life.clone(), approval: approval.clone(), input, mailbox: mailbox.clone() });
    let finished = CancellationToken::new();
    // 교체 작업도 앞 세대의 소켓 종료와 임대 정리가 끝난 뒤에만 열린다.
    let previous = slot.finished.replace(finished.clone());
    let work = Work { previous, finished: finished.drop_guard(), generation: slot.generation, life, approval: approval.clone(), inputs, mailbox };
    state.approving = Some(approval);
    Ok(work)
}

async fn connect(window: &WebviewWindow, owner: Owner, draft: Draft, notify: Channel<String>, work: Work,
                 tunnel_epoch: u64) -> Result<(), String> {
    let Work { previous, finished: _finished, generation, life, approval, inputs, mailbox } = work;
    let output = Output { life: life.clone(), mailbox, notify };
    let sessions = window.state::<Sessions>();
    if let Some(previous) = previous { previous.cancelled().await; }
    let mut tab = Tab::default();
    let mut lease = None;
    let ready = {
        let mut state = sessions.0.lock().unwrap();
        state.slot(&owner, owner.document).map(|slot| slot.cleanup.is_none())
    };
    let opened = match ready {
        Err(error) => Err(error.into()),
        Ok(false) => Err(CLEANUP_UNCONFIRMED.into()),
        Ok(true) if life.revoked() => Err(CLOSED.into()),
        Ok(true) => match draft.approve(window, life.clone(), tunnel_epoch, &mut lease, &output.notify).await {
            Err(error) => Err(error),
            Ok(mut approved) => {
                let loss = approved.loss();
                let result = tokio::select! {
                    biased;
                    _ = life.cancelled() => Err(CLOSED.into()),
                    _ = route_lost(loss.as_ref()) => Err(message(&Error::Disconnected)),
                    result = approved.open(&mut tab) => result.map_err(|error| message(&error)),
                };
                lease = approved.into_lease();
                result
            }
        },
    };
    {
        let mut state = sessions.0.lock().unwrap();
        if state.approving.as_ref().is_some_and(|current| Arc::ptr_eq(current, &approval)) { state.approving = None; }
    }
    let mut disconnected = None;
    let result = match opened {
        Err(error) => Err(error),
        Ok(id) => {
            let loss = lease.as_ref().map(DesktopLease::loss);
            let identity = Identity { session_id: id.get().to_string(), generation };
            let started = control(&output, &identity, "started", serde_json::Value::Null, 0);
            let result = tokio::select! {
                biased;
                _ = life.cancelled() => Ok(()),
                _ = route_lost(loss.as_ref()) => Err(Error::Disconnected),
                result = async { started.await?; drive(&mut tab, id, &identity, &output, inputs).await } => result,
            };
            if let Err(error) = result { disconnected = Some((identity, error)); }
            Ok(())
        }
    };
    tab.stop().await;
    let cleanup = release(lease).await;
    let unconfirmed = cleanup.is_some();
    if unconfirmed {
        let mut state = sessions.0.lock().unwrap();
        if let Ok(slot) = state.slot(&owner, owner.document) { slot.cleanup = cleanup; }
    }
    if let Some((identity, error)) = disconnected {
        if !life.revoked() {
            output.mailbox.clear();
            tokio::select! {
                biased;
                _ = life.cancelled() => {},
                _ = control(&output, &identity, "closed", message(&error).into(), u32::MAX) => {},
            }
        }
    }
    let mut state = sessions.0.lock().unwrap();
    if let Ok(slot) = state.slot(&owner, owner.document) {
        if slot.current.as_ref().is_some_and(|current| Arc::ptr_eq(&current.approval, &approval)) { slot.current = None; }
    }
    if unconfirmed { Err(CLEANUP_UNCONFIRMED.into()) } else { result }
}

async fn drive(tab: &mut Tab, id: super::SessionId, identity: &Identity, output: &Output, mut inputs: mpsc::Receiver<Command>) -> Result<(), Error> {
    let session = tab.current.as_mut().filter(|session| session.id == id).ok_or(Error::Disconnected)?;
    let frames = pump_frames(&mut session.events, identity, output);
    let controls = async {
        let mut serial = 0u32;
        while let Some(event) = session.controls.recv().await {
            let (kind, value) = match event {
                Event::CutText(text) => ("clipboard", text),
                Event::Resize(result) => ("notice", match result {
                    ResizeResult::Applied(size) => format!("서버 해상도: {} × {}", size.width, size.height),
                    ResizeResult::Rejected(code) => format!("서버 해상도 변경 거부 ({code})"),
                    ResizeResult::Busy => "서버 해상도 변경 대기 중".into(),
                    ResizeResult::NotNegotiated => "서버 해상도 요청을 협상하지 못했습니다".into(),
                    ResizeResult::Unchanged => "서버 해상도가 이미 같습니다".into(),
                }),
                Event::InputRejected(error) => ("notice", message(&error)),
                _ => unreachable!(),
            };
            serial = serial.checked_add(2).ok_or(Error::IdExhausted)?;
            control(output, identity, kind, value.into(), serial).await?;
        }
        std::future::pending::<Result<(), Error>>().await
    };
    let input = async {
        let mut last_input = 0;
        while let Some(input) = inputs.recv().await {
            if input.generation != identity.generation || input.session_id != identity.session_id || input.sequence <= last_input || input.sequence > MAX_SAFE { continue; }
            last_input = input.sequence;
            let epoch = output.mailbox.painted.load(std::sync::atomic::Ordering::Relaxed);
            if epoch == 0 || input.epoch != epoch { continue; }
            match input.action.input().and_then(|action| super::enqueue(&session.input, &session.screen_epoch, epoch, action)) {
                Ok(()) | Err(Error::ScreenNotReady) => {},
                Err(error) => return Err(error),
            }
        }
        Err(Error::Disconnected)
    };
    tokio::select! {
        biased;
        error = &mut session.closed => Err(error.unwrap_or(Error::Disconnected)),
        result = frames => result,
        result = controls => result,
        result = input => result,
    }
}

async fn pump_frames(events: &mut mpsc::Receiver<Event>, identity: &Identity, output: &Output) -> Result<(), Error> {
    let mut surface = Surface::default();
    let mut sequence = 0;
    let mut epoch = 0;
    while let Some(Event::Frame(changes)) = events.recv().await {
        for change in changes {
            if let Change::Size(_) = change {
                epoch = if epoch == 0 { 1 } else { epoch + 2 };
                output.mailbox.painted.store(0, std::sync::atomic::Ordering::Relaxed);
                let serial = u32::try_from(epoch).ok().filter(|serial| *serial < u32::MAX).ok_or(Error::IdExhausted)?;
                control(output, identity, "waiting", epoch.into(), serial).await?;
            }
            if let Some(image) = surface.apply(&change)? {
                sequence += 1;
                if sequence > MAX_SAFE { return Err(Error::IdExhausted); }
                send_image(output, &image, sequence, epoch).await?;
            }
        }
    }
    std::future::pending().await
}

async fn control(output: &Output, identity: &Identity, kind: &str, value: serde_json::Value, serial: u32) -> Result<(), Error> {
    let body = serde_json::to_vec(&serde_json::json!({ "identity": identity, "kind": kind, "value": value, "serial": serial }))
        .map_err(|_| Error::Disconnected)?;
    output.send(false, body, 0, serial, None).await
}

async fn send_image(output: &Output, image: &Image<'_>, sequence: u64, epoch: u64) -> Result<(), Error> {
    output.send(true, image.packet(sequence, epoch), sequence, 0, Some(epoch)).await
}

fn message(error: &Error) -> String {
    match error {
        Error::TlsIdentity => "VNC 서버의 TLS 인증서를 확인할 수 없습니다".into(),
        Error::MissingSecret if cfg!(windows) => "VNC 비밀번호 입력이 완료되지 않았습니다".into(),
        Error::MissingSecret => "VNC 시스템 자격 증명이 없거나 잠겼습니다".into(),
        Error::SecurityType => "서버가 선택한 VNC 인증 방식을 제공하지 않습니다".into(),
        Error::Authentication | Error::InvalidSecret => "VNC 서비스 인증 실패".into(),
        Error::UnsupportedVersion | Error::UnsupportedEncoding | Error::UnsupportedMessage => "이 VNC 서버의 프로토콜을 지원하지 않습니다".into(),
        Error::InvalidCutText => "VNC 클립보드는 1 MiB 이하의 Latin-1 텍스트만 지원합니다".into(),
        Error::Io(_) | Error::Disconnected => "VNC 연결이 끊겼습니다".into(),
        error => format!("VNC: {error:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn vnc_proof_binds_the_issued_tunnel_epoch() {
        let sessions = Sessions::default();
        sessions.0.lock().unwrap().document_proof = Some(("old".into(), "tauri://localhost".into(), 41));
        assert_eq!(sessions.0.lock().unwrap().document_for("old"), Some((0, 41)));
        sessions.retire(false);
        sessions.0.lock().unwrap().document_proof = Some(("new".into(), "tauri://localhost".into(), 42));
        let state = sessions.0.lock().unwrap();
        assert_eq!(state.document_for("old"), None);
        assert_eq!(state.document_for("new"), Some((1, 42)));
    }

    #[test]
    fn vnc_replacement_discards_old_document_and_running_input() {
        let sessions = Sessions::default();
        let owner = Owner { tab: "viewer".into(), document: 0 };
        sessions.0.lock().unwrap().tabs.insert(owner.tab.clone(), Slot::default());
        let mut old = prepare(&sessions, &owner, 0).unwrap();
        {
            let mut state = sessions.0.lock().unwrap();
            state.approving = None;
            state.tabs[&owner.tab].current.as_ref().unwrap().input.try_send(Command {
                session_id: "old".into(), generation: old.generation, epoch: 1, sequence: 1, action: Action::Release,
            }).unwrap_or_else(|_| panic!("입력 큐"));
        }
        let new = prepare(&sessions, &owner, 0).unwrap();
        assert!(old.life.revoked());
        // 취소된 워커가 소유했던 큐는 여기서 폐기되며 새 워커로 이동하지 않는다.
        assert_eq!(old.inputs.try_recv().unwrap().session_id, "old");
        assert_eq!(new.generation, old.generation + 1);
        assert!(new.inputs.is_empty());
        sessions.retire(false);
        assert!(new.life.revoked());
        assert!(sessions.0.lock().unwrap().slot(&owner, 1).is_err());
    }

    #[tokio::test]
    async fn vnc_owner_close_waits_for_worker_and_retains_unconfirmed_slot() {
        let sessions = Sessions::default();
        let owner = Owner { tab: "viewer".into(), document: 0 };
        let other = Owner { tab: "other".into(), document: 0 };
        {
            let mut state = sessions.0.lock().unwrap();
            state.tabs.insert(owner.tab.clone(), Slot::default());
            state.tabs.insert(other.tab.clone(), Slot::default());
        }
        let work = prepare(&sessions, &owner, 0).unwrap();
        assert!(begin_close(&sessions, &owner, 0, Some(work.generation + 1)).is_err());
        assert!(!work.life.revoked());
        let gate = begin_close(&sessions, &owner, 0, None).unwrap();
        assert!(work.life.revoked());
        let close_pending = close(&sessions, &owner, 0, None, false, gate);
        tokio::pin!(close_pending);
        tokio::select! {
            biased;
            _ = &mut close_pending => panic!("작업 종료 확인 전에 닫힘"),
            () = std::future::ready(()) => {},
        }
        {
            let mut state = sessions.0.lock().unwrap();
            state.approving = None;
            state.slot(&owner, 0).unwrap().cleanup = Some(Cleanup::WorkerFailed);
        }
        drop(work);
        assert_eq!(close_pending.await, Ok(false));
        assert_eq!(prepare(&sessions, &owner, 0).err(), Some(CLEANUP_UNCONFIRMED));
        let other_gate = begin_close(&sessions, &other, 0, None).unwrap();
        assert_eq!(close(&sessions, &other, 0, None, true, other_gate).await, Ok(true));
        assert!(sessions.0.lock().unwrap().slot(&owner, 0).unwrap().cleanup.is_some());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn vnc_windows_password_is_discarded_if_tab_or_document_changes_at_entry() {
        use super::super::{Auth, Target, approved::Approved, wire};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use zeroize::Zeroizing;

        for retire_document in [false, true] {
            let sessions = Arc::new(Sessions::default());
            let owner = Owner { tab: "viewer".into(), document: 0 };
            sessions.0.lock().unwrap().tabs.insert(owner.tab.clone(), Slot::default());
            let work = prepare(&sessions, &owner, 0).unwrap();
            let target = Target { profile_id: "fixture".into(), host: "localhost".into(), port: 5900 };
            let grant = Approved::fixture(target.clone(), work.life.clone(), move || {
                if retire_document { sessions.retire(false); }
                else { sessions.0.lock().unwrap().tabs.remove(&owner.tab); }
                Some(Zeroizing::new(b"isolated".to_vec()))
            });
            let (mut client, mut server) = tokio::io::duplex(64);
            let peer = async {
                server.write_all(b"RFB 003.008\n").await.unwrap();
                let mut version = [0; 12];
                server.read_exact(&mut version).await.unwrap();
                server.write_all(&[1, 2]).await.unwrap();
                assert_eq!(server.read_u8().await.unwrap(), 2);
                server.write_all(&[0; 16]).await.unwrap();
                assert_eq!(server.read(&mut [0; 16]).await.unwrap(), 0);
            };
            let client = async {
                let result = wire::handshake(&mut client, &target, Auth::VncPassword, &grant).await;
                drop(client);
                result
            };
            let (result, ()) = tokio::join!(client, peer);
            assert!(work.life.revoked());
            assert_eq!(result, Err(Error::MissingSecret));
        }
    }

    #[tokio::test]
    async fn vnc_document_replacement_revokes_started_fetch_and_future_output() {
        let sessions = Sessions::default();
        let owner = Owner { tab: "viewer".into(), document: 0 };
        {
            let mut state = sessions.0.lock().unwrap();
            state.document_proof = Some(("old".into(), "tauri://localhost".into(), 0));
            state.tabs.insert(owner.tab.clone(), Slot::default());
        }
        let work = prepare(&sessions, &owner, 0).unwrap();
        let (sent, mut tickets) = mpsc::unbounded_channel();
        let output = Output { life: work.life, mailbox: work.mailbox, notify: Channel::new(move |packet| {
            let tauri::ipc::InvokeResponseBody::Json(json) = packet else { panic!("알림은 핸들만 전달") };
            sent.send(serde_json::from_str::<String>(&json).unwrap()).unwrap();
            Ok(())
        }) };
        let id = Identity { session_id: "1".into(), generation: 1 };
        let pending = control(&output, &id, "started", serde_json::Value::Null, 0);
        let replace = async {
            let ticket = tickets.recv().await.unwrap();
            assert!(sessions.packet("other", "tauri://localhost", &ticket, |_, _| ()).is_none());
            assert!(sessions.packet("old", "http://other", &ticket, |_, _| ()).is_none());
            assert!(output.mailbox.ack(0, 0).is_none());
            sessions.retire(false);
            assert!(sessions.packet("old", "tauri://localhost", &ticket, |_, _| panic!("폐기된 문서에 응답")).is_none());
            assert!(output.mailbox.take(&ticket).is_none());
            assert_eq!(output.send(true, vec![1], 1, 0, None).await, Err(Error::Disconnected));
            assert!(tickets.try_recv().is_err());
        };
        let (result, ()) = tokio::join!(pending, replace);
        assert_eq!(result, Err(Error::Disconnected));
    }

    fn test_output() -> (Output, mpsc::UnboundedReceiver<String>) {
        let (sent, packets) = mpsc::unbounded_channel();
        let notify = Channel::new(move |packet| {
            let tauri::ipc::InvokeResponseBody::Json(ticket) = packet else { panic!("출력 티켓") };
            sent.send(serde_json::from_str(&ticket).unwrap()).unwrap();
            Ok(())
        });
        (Output { life: Arc::default(), mailbox: Arc::default(), notify }, packets)
    }

    #[tokio::test]
    async fn vnc_binary_frame_is_one_packet_retrieved_once_then_acked() {
        let (output, mut packets) = test_output();
        let size = Size { width: 3840, height: 2160 };
        let pixels: Vec<u8> = (0..usize::from(size.width) * usize::from(size.height) * 4).map(|n| n as u8).collect();
        let image = Image { kind: 1, size, rect: super::super::Rect { x: 0, y: 0, size }, rgba: &pixels };
        let receive = async {
            let ticket = packets.recv().await.unwrap();
            assert!(output.mailbox.ack(7, 0).is_none());
            let (raw, packet) = output.mailbox.take(&ticket).unwrap();
            assert!(raw);
            assert_eq!(packet.len(), super::super::presentation::HEADER + pixels.len());
            assert_eq!(&packet[..17], &[[1].as_slice(), &7u64.to_le_bytes(), &1u64.to_le_bytes()].concat());
            assert_eq!(&packet[super::super::presentation::HEADER..], &pixels);
            assert!(output.mailbox.take(&ticket).is_none());
            assert!(packets.try_recv().is_err());
            assert_eq!(output.mailbox.painted.load(std::sync::atomic::Ordering::Relaxed), 0);
            assert_eq!(output.mailbox.ack(7, 0), Some(()));
            assert_eq!(output.mailbox.painted.load(std::sync::atomic::Ordering::Relaxed), 1);
            assert!(packets.try_recv().is_err());
        };
        let (result, ()) = tokio::join!(send_image(&output, &image, 7, 1), receive);
        assert_eq!(result, Ok(()));
        let oversized = vec![0; super::super::presentation::HEADER + super::super::wire::MAX_FRAME_BYTES + 1];
        assert_eq!(output.send(true, oversized, 8, 0, None).await, Err(Error::LengthLimit));
        assert!(packets.try_recv().is_err());
        use super::super::output::ack_deadline;
        assert_eq!(ack_deadline(4096), Duration::from_secs(10));
        assert_eq!(ack_deadline(super::super::presentation::HEADER + pixels.len()), Duration::from_millis(10_000 + 31 * 250));
        assert_eq!(ack_deadline(usize::MAX), Duration::from_secs(30));
    }

    #[tokio::test]
    async fn vnc_pending_frame_ack_preserves_release_control_and_resize_timeout() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (stream, mut server) = tokio::io::duplex(1024);
        let size = Size { width: 2, height: 1 };
        let mut tab = Tab::default();
        let id = tab.start(Box::new(stream), size).unwrap();
        let identity = Identity { session_id: id.get().to_string(), generation: 1 };
        let (output, mut packets) = test_output();
        let (input, inputs) = mpsc::channel(8);
        let drive = drive(&mut tab, id, &identity, &output, inputs);
        let released = std::cell::Cell::new(false);
        let peer = async {
            let confirm = |sequence, offset| {
                assert_eq!(output.mailbox.ack(sequence, offset), Some(()));
            };
            let send = |sequence, action| input.try_send(Command {
                session_id: identity.session_id.clone(), generation: 1, epoch: 1, sequence, action,
            }).unwrap_or_else(|_| panic!("입력 큐"));
            assert!(!output.mailbox.take(&packets.recv().await.unwrap()).unwrap().0);
            confirm(0, 1);
            server.write_all(&[
                0, 0, 0, 1, 0, 0, 0, 0, 0, 2, 0, 1, 255, 255, 254, 204,
                1, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 2, 0, 1, 0, 0, 0, 0,
            ]).await.unwrap();
            let mut request = [0; 10];
            server.read_exact(&mut request).await.unwrap();
            assert_eq!(request, [3, 1, 0, 0, 0, 0, 0, 2, 0, 1]);
            let frame = [0, 0, 0, 1, 0, 0, 0, 0, 0, 2, 0, 1, 0, 0, 0, 0, 1, 2, 3, 0, 4, 5, 6, 0];
            server.write_all(&frame).await.unwrap();
            server.read_exact(&mut request).await.unwrap();
            assert!(output.mailbox.take(&packets.recv().await.unwrap()).unwrap().0);
            confirm(1, 0);
            send(1, Action::Key { keysym: 97, down: true });
            send(2, Action::Pointer { x: 1, y: 0, buttons: 1 });
            send(3, Action::Resize { width: 3, height: 1 });
            let mut pressed = [0; 14];
            server.read_exact(&mut pressed).await.unwrap();
            assert_eq!(pressed, [4, 1, 0, 0, 0, 0, 0, 97, 5, 1, 0, 1, 0, 0]);
            let mut resize = [0; 24];
            server.read_exact(&mut resize).await.unwrap();
            assert_eq!(resize[0], 251);
            // 같은 타이머 틱에 만료되지 않게 ACK 대기를 늦춰 시작한다.
            tokio::time::sleep(Duration::from_millis(100)).await;
            server.write_all(&frame).await.unwrap();
            let pending_frame = packets.recv().await.unwrap();
            send(4, Action::Resize { width: 4, height: 1 });
            let (raw, notice) = output.mailbox.take(&packets.recv().await.unwrap()).unwrap();
            assert!(!raw);
            let notice = String::from_utf8(notice).unwrap();
            assert!(notice.contains("서버 해상도 변경 대기 중"));
            confirm(0, 2);
            assert!(output.mailbox.take(&pending_frame).unwrap().0);
            for _ in 0..3 {
                server.write_all(&frame).await.unwrap();
                server.read_exact(&mut request).await.unwrap();
            }
            send(5, Action::Release);
            server.read_exact(&mut pressed).await.unwrap();
            assert_eq!(pressed, [4, 0, 0, 0, 0, 0, 0, 97, 5, 0, 0, 1, 0, 0]);
            assert!(packets.try_recv().is_err());
            // 타이머가 끝날 때까지 피어와 미확인 프레임을 유지한다.
            released.set(true);
            std::future::pending::<()>().await;
        };
        let result = tokio::time::timeout(Duration::from_secs(12), async {
            tokio::select! {
                biased;
                result = drive => result,
                () = peer => panic!("펌프 종료 전 피어가 종료됨"),
            }
        }).await.unwrap();
        assert_eq!(result, Err(Error::ResizeTimeout));
        assert!(released.get());
    }
}
