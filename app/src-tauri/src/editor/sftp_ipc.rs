use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::{Manager, ipc::{CommandArg, CommandItem, Invoke}};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::terminal::Sessions;

use super::{
    nano_bridge::{NanoBridge, PublishError, Publisher},
    nano_wire::{MAX_CONTENT, Observation, Snapshot},
    sftp::{self, Entry, File, Session},
};

mod response;
use response::{Error, SaveResult, SaveStage};

type Result<T, E = Error> = std::result::Result<T, E>;
type Slot = Arc<AttachmentSlot>;

#[derive(Default)]
struct AttachmentSlot {
    lifetime: CancellationToken,
    owners: Mutex<Owners>,
    state: AsyncMutex<Attachment>,
}

#[derive(Default)]
struct Owners {
    panel: Option<Arc<CancellationToken>>,
    bridge: Option<CancellationToken>,
    closing: bool,
}

pub(crate) enum SharedSftpNamespace {
    AdministratorConfirmed,
}

pub(crate) struct BridgeSlot {
    label: String,
    generation: u64,
    id: String,
    slot: Slot,
    cancel: CancellationToken,
}

impl BridgeSlot {
    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }

    fn check(&self, state: &SftpIpc) -> Result<()> {
        state.current(&self.label, self.generation, &self.id, &self.slot)?;
        if self.cancel.is_cancelled() {
            return Err(Error::Disconnected);
        }
        Ok(())
    }
}

impl Drop for BridgeSlot {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[derive(Debug)]
pub(crate) struct BridgeError(Error);

impl From<Error> for BridgeError {
    fn from(error: Error) -> Self {
        Self(error)
    }
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl std::error::Error for BridgeError {}

#[derive(Default)]
enum Attachment {
    #[default]
    Opening,
    Active(Attached),
    Closing,
    CloseFailed(Error),
}

impl Attachment {
    async fn connect(&mut self, connect: impl Future<Output = Result<Session, sftp::Error>>) -> Result<&mut Attached> {
        if matches!(self, Self::Opening) {
            match connect.await {
                Ok(session) => *self = Self::Active(Attached::new(session)),
                Err(sftp::Error::Reap(_)) => {
                    *self = Self::CloseFailed(Error::RemoteFailure);
                    return Err(Error::RemoteFailure);
                }
                Err(error) => return Err(error.into()),
            }
        } else if let Self::Active(attached) = self {
            attached.refresh_transport(connect).await?;
        }
        match self {
            Self::Active(attached) => Ok(attached),
            Self::CloseFailed(error) => Err(*error),
            Self::Opening | Self::Closing => Err(Error::Disconnected),
        }
    }

    async fn close(&mut self) -> Result<()> {
        let session = match std::mem::replace(self, Self::Closing) {
            Self::Active(attached) => attached.transport.into_session(),
            Self::Opening | Self::Closing => Ok(None),
            Self::CloseFailed(error) => Err(error),
        };
        let session = match session {
            Ok(session) => session,
            Err(error) => {
                *self = Self::CloseFailed(error);
                return Err(error);
            }
        };
        if let Some(session) = session {
            if let Err(error) = session.close().await {
                let error = Error::from(error);
                *self = Self::CloseFailed(error);
                return Err(error);
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct SftpIpc(Mutex<HashMap<String, Document>>);

struct Document {
    generation: u64,
    parents: HashMap<String, Slot>,
}

impl Drop for Document {
    fn drop(&mut self) {
        for slot in self.parents.values() {
            slot.lifetime.cancel();
        }
    }
}

#[derive(Default)]
struct Browser {
    entries: HashMap<String, Entry>,
    route: Vec<(String, Entry)>,
}

struct Attached {
    transport: Transport,
    browser: Browser,
    files: HashMap<String, OpenFile>,
}

enum Transport {
    Connected(Session),
    Reconnect(Option<Session>),
    CloseFailed(Error),
}

impl Transport {
    fn into_session(self) -> Result<Option<Session>> {
        match self {
            Self::Connected(session) => Ok(Some(session)),
            Self::Reconnect(session) => Ok(session),
            Self::CloseFailed(error) => Err(error),
        }
    }

    fn connected(&mut self) -> Result<&mut Session> {
        match self {
            Self::Connected(session) => Ok(session),
            Self::Reconnect(_) => Err(Error::Disconnected),
            Self::CloseFailed(error) => Err(*error),
        }
    }

    fn require_reconnect(&mut self) {
        // 새 채널도 같은 SSH 부모의 연결 계획을 사용하므로 기존 Rust 핸들을 유지한다.
        if matches!(self, Self::Connected(_)) {
            let previous = std::mem::replace(self, Self::Reconnect(None));
            *self = Self::Reconnect(previous.into_session().expect("connected transport"));
        }
    }

    fn observe(&mut self, error: &sftp::Error) {
        if matches!(error, sftp::Error::Reap(_)) {
            *self = Self::CloseFailed(Error::RemoteFailure);
        } else if matches!(Error::from(error), Error::Disconnected | Error::TimedOut) {
            self.require_reconnect();
        }
    }

    fn finish<T>(&mut self, result: Result<T, sftp::Error>) -> Result<T> {
        result.map_err(|error| {
            self.observe(&error);
            error.into()
        })
    }
}

struct OpenFile {
    file: File,
    unknown: bool,
    bridge: Option<BridgeFile>,
}

struct BridgeFile {
    offer_id: String,
    snapshot: Snapshot,
    publisher: Publisher,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Listed {
    entry_id: String,
    name: String,
    is_directory: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Listing {
    entries: Vec<Listed>,
    parent_entry_id: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct Opened {
    pub(crate) handle: String,
    pub(crate) name: String,
    pub(crate) text: String,
}

impl SftpIpc {
    pub(crate) fn navigate(&self, label: &str, generation: u64) {
        self.0.lock().unwrap().insert(label.into(), Document { generation, parents: HashMap::new() });
    }

    pub(crate) fn destroy(&self, label: &str) {
        self.0.lock().unwrap().remove(label);
    }

    pub(crate) fn shutdown(&self) {
        self.0.lock().unwrap().clear();
    }

    pub(crate) fn close_parent(&self, label: &str, generation: u64, id: &str) {
        let _ = self.remove(label, generation, id, None);
    }

    fn admit<T>(
        &self, label: &str, generation: u64, id: &str,
        claim: impl FnOnce(&mut Owners, &CancellationToken) -> Result<T>,
    ) -> Result<(Slot, T)> {
        let mut documents = self.0.lock().unwrap();
        let document = documents.get_mut(label).filter(|doc| doc.generation == generation).ok_or(Error::DocumentClosed)?;
        let slot = document.parents.entry(id.into()).or_default();
        let mut owners = slot.owners.lock().unwrap();
        if owners.closing {
            return Err(Error::Unavailable);
        }
        Ok((slot.clone(), claim(&mut owners, &slot.lifetime)?))
    }

    fn reserve(&self, label: &str, generation: u64, id: &str) -> Result<(Slot, Arc<CancellationToken>)> {
        self.admit(label, generation, id, |owners, lifetime| {
            if owners.panel.is_some() {
                return Err(Error::Unavailable);
            }
            let cancel = Arc::new(lifetime.child_token());
            owners.panel = Some(cancel.clone());
            Ok(cancel)
        })
    }

    pub(crate) fn bind_bridge(
        &self, label: &str, generation: u64, id: &str, _: SharedSftpNamespace,
    ) -> Result<BridgeSlot, BridgeError> {
        if label != "main" {
            return Err(Error::Unavailable.into());
        }
        let (slot, cancel) = self.admit(label, generation, id, |owners, lifetime| {
            if owners.bridge.is_some() {
                return Err(Error::Unavailable);
            }
            let cancel = lifetime.child_token();
            owners.bridge = Some(cancel.clone());
            Ok(cancel)
        })?;
        Ok(BridgeSlot { label: label.into(), generation, id: id.into(), slot, cancel })
    }

    pub(super) async fn open_verified_bridge(
        &self, bridge: &BridgeSlot, sessions: &Sessions, path: &str, offer_id: &str,
        snapshot: Snapshot, publisher: Publisher,
    ) -> Result<Opened, BridgeError> {
        self.open_bridge_with(bridge, path, offer_id, snapshot, publisher,
            sessions.connect_sftp(&bridge.label, &bridge.id)).await
    }

    async fn open_bridge_with(
        &self, bridge: &BridgeSlot, path: &str, offer_id: &str, snapshot: Snapshot, publisher: Publisher,
        connect: impl Future<Output = Result<Session, sftp::Error>>,
    ) -> Result<Opened, BridgeError> {
        bridge.check(self)?;
        let mut guard = bridge.slot.state.lock().await;
        bridge.check(self)?;
        let attached = guard.connect(connect).await?;
        bridge.check(self)?;
        // 패널과 채널을 공유하므로 시작한 SFTP 읽기는 기존 제한 시간 안에 회수한다.
        let result = attached.transport.connected()?.open_nano(path).await;
        let (file, text) = attached.transport.finish(result)?;
        bridge.check(self)?;
        check_observation(&text, snapshot.observation)?;
        let name = path.rsplit('/').next().expect("absolute file path").into();
        attached.insert_file(file, name, text, Some(BridgeFile { offer_id: offer_id.into(), snapshot, publisher })).map_err(Into::into)
    }

    pub(super) async fn discard_bridge_file(&self, bridge: &BridgeSlot, offer_id: &str, handle: &str) {
        let mut guard = bridge.slot.state.lock().await;
        if self.current(&bridge.label, bridge.generation, &bridge.id, &bridge.slot).is_ok() {
            if let Attachment::Active(attached) = &mut *guard {
                if attached.files.get(handle).and_then(|file| file.bridge.as_ref()).is_some_and(|file| file.offer_id == offer_id) {
                    attached.files.remove(handle);
                }
            }
        }
    }

    async fn attach_panel(
        &self, label: &str, generation: u64, id: &str, slot: &Slot, panel: &Arc<CancellationToken>,
        connect: impl Future<Output = Result<Session, sftp::Error>>,
    ) -> Result<Listing> {
        let mut guard = slot.state.lock().await;
        self.current(label, generation, id, slot)?;
        check_panel(panel)?;
        let result = match guard.connect(connect).await {
            Ok(attached) if !panel.is_cancelled() => attached.list(None).await,
            Ok(_) => Err(Error::Disconnected),
            Err(error) => Err(error),
        };
        self.current(label, generation, id, slot)?;
        check_panel(panel)?;
        if result.is_err() {
            panel.cancel();
            self.release_panel(label, generation, id, slot, panel, &mut guard).await?;
        }
        result
    }

    async fn release_panel(
        &self, label: &str, generation: u64, id: &str, slot: &Slot, panel: &Arc<CancellationToken>,
        guard: &mut Attachment,
    ) -> Result<()> {
        let preserve_bridge = {
            let mut owners = slot.owners.lock().unwrap();
            if !owners.panel.as_ref().is_some_and(|active| Arc::ptr_eq(active, panel)) {
                return Ok(());
            }
            let preserve = owners.bridge.as_ref().is_some_and(|token| !token.is_cancelled())
                || matches!(guard, Attachment::Active(attached) if attached.files.values().any(|file| file.bridge.is_some()));
            if preserve {
                owners.panel = None;
            } else {
                owners.closing = true;
            }
            preserve
        };
        if preserve_bridge {
            if let Attachment::Active(attached) = guard {
                attached.browser = Browser::default();
                attached.files.retain(|_, file| file.bridge.is_some());
            }
        } else {
            guard.close().await?;
            let _ = self.remove(label, generation, id, Some(slot));
        }
        Ok(())
    }

    async fn close_file(
        &self, nano: &NanoBridge, label: &str, generation: u64, id: &str, slot: &Slot, handle: &str,
    ) {
        let mut guard = slot.state.lock().await;
        if self.current(label, generation, id, slot).is_ok() {
            if let Attachment::Active(attached) = &mut *guard {
                if let Some(bridge) = attached.files.get(handle).and_then(|file| file.bridge.as_ref()) {
                    // 슬롯 잠금 안에서 마운트 상태를 먼저 닫아 복구 응답과 파일 제거가 엇갈리지 않게 한다.
                    nano.file_closed(label, generation, id, &bridge.offer_id, handle);
                }
                attached.files.remove(handle);
            }
        }
    }

    fn slot(&self, label: &str, generation: u64, id: &str) -> Result<Option<Slot>> {
        let documents = self.0.lock().unwrap();
        let document = documents.get(label).filter(|doc| doc.generation == generation).ok_or(Error::DocumentClosed)?;
        Ok(document.parents.get(id).cloned())
    }

    fn current(&self, label: &str, generation: u64, id: &str, slot: &Slot) -> Result<()> {
        if self.slot(label, generation, id)?.is_some_and(|active| Arc::ptr_eq(&active, slot))
            && !slot.lifetime.is_cancelled() && !slot.owners.lock().unwrap().closing
        {
            Ok(())
        } else {
            Err(Error::Disconnected)
        }
    }

    fn remove(&self, label: &str, generation: u64, id: &str, expected: Option<&Slot>) -> Result<Option<Slot>> {
        let mut documents = self.0.lock().unwrap();
        let document = documents.get_mut(label).filter(|doc| doc.generation == generation).ok_or(Error::DocumentClosed)?;
        if let Some(expected) = expected {
            if !document.parents.get(id).is_some_and(|active| Arc::ptr_eq(active, expected)) {
                return Ok(None);
            }
        }
        let removed = document.parents.remove(id);
        if let Some(slot) = &removed {
            slot.lifetime.cancel();
        }
        Ok(removed)
    }
}

fn check_panel(panel: &CancellationToken) -> Result<()> {
    if panel.is_cancelled() { Err(Error::Disconnected) } else { Ok(()) }
}

fn opaque_id() -> Result<String> {
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|_| Error::Unavailable)?;
    Ok(format!("{:032x}", u128::from_ne_bytes(random)))
}

impl Browser {
    async fn list(&mut self, transport: &mut Transport, entry_id: Option<&str>) -> Result<Listing> {
        let ancestor = entry_id.and_then(|id| self.route.iter().position(|(key, _)| key == id));
        let session = transport.connected()?;
        let result = match (entry_id, ancestor) {
            (None, _) => session.list(None).await,
            (Some(_), Some(index)) => session.list_directory(&self.route[index].1).await,
            (Some(id), None) => {
                let entry = self.entries.get(id).filter(|entry| entry.is_directory()).ok_or(Error::InvalidEntry)?;
                session.list_directory(entry).await
            }
        };
        let entries = transport.finish(result)?;
        let mut new_entries = HashMap::with_capacity(entries.len());
        let mut listed = Vec::with_capacity(entries.len());
        for entry in entries {
            let id = loop {
                let id = opaque_id()?;
                if !new_entries.contains_key(&id) && !self.entries.contains_key(&id)
                    && !self.route.iter().any(|(existing, _)| existing == &id)
                {
                    break id;
                }
            };
            listed.push(Listed { entry_id: id.clone(), name: entry.name().into(), is_directory: entry.is_directory() });
            new_entries.insert(id, entry);
        }
        match (entry_id, ancestor) {
            (None, _) => self.route.clear(),
            (Some(_), Some(index)) => self.route.truncate(index + 1),
            (Some(id), None) => self.route.push(self.entries.remove_entry(id).expect("listed directory")),
        }
        self.entries = new_entries;
        let parent_entry_id = self.route.iter().rev().nth(1).map(|(id, _)| id.clone());
        Ok(Listing { entries: listed, parent_entry_id })
    }
}

impl Attached {
    fn new(session: Session) -> Self {
        Self { transport: Transport::Connected(session), browser: Browser::default(), files: HashMap::new() }
    }

    async fn list(&mut self, entry: Option<&str>) -> Result<Listing> {
        self.browser.list(&mut self.transport, entry).await
    }

    async fn refresh_transport(&mut self, connect: impl Future<Output = Result<Session, sftp::Error>>) -> Result<()> {
        match &mut self.transport {
            Transport::Connected(_) => return Ok(()),
            Transport::CloseFailed(error) => return Err(*error),
            Transport::Reconnect(previous) => {
                if let Some(previous) = previous.take() {
                    if let Err(error) = previous.close().await {
                        let error = Error::from(error);
                        self.transport = Transport::CloseFailed(error);
                        return Err(error);
                    }
                }
            }
        }
        let session = self.transport.finish(connect.await)?;
        self.transport = Transport::Connected(session);
        Ok(())
    }

    async fn open(&mut self, entry_id: &str) -> Result<Opened> {
        let entry = self.browser.entries.get(entry_id).filter(|entry| !entry.is_directory()).ok_or(Error::InvalidEntry)?;
        let result = self.transport.connected()?.open(entry).await;
        let (file, text) = self.transport.finish(result)?;
        let name = entry.name().into();
        self.insert_file(file, name, text, None)
    }

    fn insert_file(&mut self, file: File, name: String, text: String, bridge: Option<BridgeFile>) -> Result<Opened> {
        let handle = loop {
            let handle = opaque_id()?;
            if let std::collections::hash_map::Entry::Vacant(entry) = self.files.entry(handle.clone()) {
                entry.insert(OpenFile { file, unknown: false, bridge });
                break handle;
            }
        };
        Ok(Opened { handle, name, text })
    }

    fn check_file(
        &self, slot: &Slot, handle: &str, nano: &NanoBridge, label: &str, generation: u64, id: &str,
    ) -> Result<()> {
        let file = self.files.get(handle).ok_or(Error::InvalidHandle)?;
        if let Some(bridge) = &file.bridge {
            if nano.is_mounted(label, generation, id, &bridge.offer_id, handle) {
                return Ok(());
            }
            return Err(Error::InvalidHandle);
        }
        if slot.owners.lock().unwrap().panel.as_ref().is_some_and(|panel| !panel.is_cancelled()) {
            Ok(())
        } else {
            Err(Error::Disconnected)
        }
    }

    async fn read(&mut self, handle: &str, reload: bool, mounted: impl Fn(&str) -> bool) -> Result<String> {
        let opened = self.files.get_mut(handle).ok_or(Error::InvalidHandle)?;
        if let Some(bridge) = &mut opened.bridge {
            if !mounted(&bridge.offer_id) {
                return Err(Error::InvalidHandle);
            }
            let observation = bridge.publisher.compare().await.map_err(publish_error)?;
            let text = read_verified(&mut self.transport, &opened.file, observation, None).await?;
            if !mounted(&bridge.offer_id) {
                return Err(Error::InvalidHandle);
            }
            if reload {
                let snapshot = bridge.publisher.reload(observation).await.map_err(publish_error)?;
                if snapshot.observation != observation {
                    return Err(Error::ReadChanged);
                }
                if !mounted(&bridge.offer_id) {
                    return Err(Error::InvalidHandle);
                }
                bridge.snapshot = snapshot;
                opened.unknown = false;
            }
            return Ok(text);
        }
        let result = self.transport.connected()?.reopen(&opened.file).await;
        let (file, text) = self.transport.finish(result)?;
        if reload {
            opened.file = file;
            opened.unknown = false;
        }
        Ok(text)
    }

    async fn save(&mut self, handle: &str, text: &str, mounted: impl Fn(&str) -> bool) -> Result<SaveResult> {
        let opened = self.files.get_mut(handle).ok_or(Error::InvalidHandle)?;
        if opened.unknown {
            return Ok(SaveResult::failed(SaveStage::Prepare, Error::ReopenRequired));
        }
        if let Some(bridge) = &mut opened.bridge {
            if !mounted(&bridge.offer_id) {
                return Err(Error::InvalidHandle);
            }
            if text.len() > MAX_CONTENT {
                return Ok(SaveResult::failed(SaveStage::Prepare, Error::LimitExceeded));
            }
            if let Err(error) = self.transport.connected() {
                return Ok(SaveResult::failed(SaveStage::Prepare, error));
            }
            let (error, recovery_path) = match bridge.publisher.save(bridge.snapshot.token, text).await {
                Ok(snapshot) => match read_verified(&mut self.transport, &opened.file, snapshot.observation, Some(text.as_bytes())).await {
                    Ok(_) if mounted(&bridge.offer_id) => {
                        bridge.snapshot = snapshot;
                        return Ok(SaveResult::Saved);
                    }
                    Ok(_) => (Error::Disconnected, None),
                    Err(error) => (error, None),
                },
                Err(PublishError::Conflict) => return Ok(SaveResult::Conflict {
                    remote: response::RemoteVersion::ReadChanged,
                    cleanup: None,
                }),
                Err(PublishError::Unavailable) => return Ok(SaveResult::failed(SaveStage::Prepare, Error::Unavailable)),
                Err(PublishError::Unknown(path)) => (Error::RemoteFailure, path.and_then(|name| opened.file.recovery_path(&name))),
            };
            opened.unknown = true;
            return Ok(SaveResult::Unknown { error, recovery_path });
        }
        let session = match self.transport.connected() {
            Ok(session) => session,
            Err(error) => return Ok(SaveResult::failed(SaveStage::Prepare, error)),
        };
        let result = session.save(&mut opened.file, text).await;
        match result {
            Ok(sftp::SaveOutcome::Unconfirmed(error)) => {
                self.transport.observe(&error);
                opened.unknown = true;
                self.transport.require_reconnect();
                Ok(SaveResult::Unknown { error: error.into(), recovery_path: None })
            }
            Ok(outcome) => {
                if let sftp::SaveOutcome::Failed { error, .. } = &outcome {
                    self.transport.observe(error);
                }
                Ok(outcome.into())
            }
            Err(error) => {
                self.transport.observe(&error);
                Ok(SaveResult::failed(SaveStage::Prepare, error.into()))
            }
        }
    }
}

fn publish_error(error: PublishError) -> Error {
    match error {
        PublishError::Conflict => Error::ReadChanged,
        PublishError::Unavailable => Error::Unavailable,
        PublishError::Unknown(_) => Error::RemoteFailure,
    }
}

fn check_observation(text: &str, observation: Observation) -> Result<()> {
    if text.len() as u64 != observation.len || <[u8; 32]>::from(Sha256::digest(text.as_bytes())) != observation.digest {
        return Err(Error::ReadChanged);
    }
    Ok(())
}

async fn read_verified(transport: &mut Transport, file: &File, observation: Observation, submitted: Option<&[u8]>) -> Result<String> {
    let result = transport.connected()?.read_nano(file).await;
    let text = transport.finish(result)?;
    check_observation(&text, observation)?;
    if submitted.is_some_and(|bytes| bytes != text.as_bytes()) {
        return Err(Error::ReadChanged);
    }
    Ok(text)
}

pub(crate) fn dispatch(invoke: Invoke) -> bool {
    let Invoke { message, resolver, acl } = invoke;
    let webview = message.webview_ref().clone();
    if acl.is_none() || webview.label() != "main" || webview.window_ref().label() != "main" {
        return false;
    }
    let command = match message.command() {
        "editor_sftp_attach" => "editor_sftp_attach",
        "editor_sftp_list" => "editor_sftp_list",
        "editor_sftp_open" => "editor_sftp_open",
        "editor_sftp_compare" => "editor_sftp_compare",
        "editor_sftp_reload" => "editor_sftp_reload",
        "editor_sftp_save" => "editor_sftp_save",
        "editor_sftp_close" => "editor_sftp_close",
        "editor_sftp_detach" => "editor_sftp_detach",
        _ => return false,
    };
    let label = webview.label();
    let generation = webview.state::<Sessions>().document(label);
    let state = webview.state::<SftpIpc>();
    macro_rules! get {
        ($call:expr, $reject:ident) => {
            match $call {
                Ok(value) => value,
                Err(error) => { resolver.$reject(error); return true; }
            }
        };
    }
    macro_rules! arg {
        ($type:ty, $key:literal) => {
            get!(<$type>::from_command(CommandItem { name: command, key: $key, plugin: None, message: &message, acl: &acl }), invoke_error)
        };
    }
    let id = arg!(String, "sessionId");
    if command == "editor_sftp_attach" {
        let (slot, panel) = get!(state.reserve(label, generation, &id), reject);
        resolver.respond_async(async move {
            let state = webview.state::<SftpIpc>();
            let label = webview.label();
            let sessions = webview.state::<Sessions>();
            state.attach_panel(label, generation, &id, &slot, &panel, sessions.connect_sftp(label, &id))
                .await.map_err(Into::into)
        });
        return true;
    }
    if command == "editor_sftp_detach" {
        let panel = get!(state.slot(label, generation, &id), reject).and_then(|slot| {
            let panel = slot.owners.lock().unwrap().panel.clone()?;
            panel.cancel();
            Some((slot, panel))
        });
        resolver.respond_async(async move {
            let Some((slot, panel)) = panel else { return Ok(()) };
            let state = webview.state::<SftpIpc>();
            let mut guard = slot.state.lock().await;
            state.release_panel(webview.label(), generation, &id, &slot, &panel, &mut guard).await.map_err(Into::into)
        });
        return true;
    }
    let slot = get!(state.slot(label, generation, &id), reject);
    if command == "editor_sftp_close" && slot.is_none() {
        let _ = arg!(String, "handle");
        resolver.resolve(());
        return true;
    }
    let slot = get!(slot.ok_or(Error::Unavailable), reject);
    macro_rules! run {
        ($attached:ident, $slot:ident, $access:expr, $body:expr) => {
            resolver.respond_async(async move {
                let state = webview.state::<SftpIpc>();
                let sessions = webview.state::<Sessions>();
                let label = webview.label();
                let mut guard = slot.state.lock().await;
                let $slot = &slot;
                state.current(label, generation, &id, &slot)?;
                let Attachment::Active($attached) = &mut *guard else {
                    return Err(Error::Unavailable.into());
                };
                $access?;
                $attached.refresh_transport(sessions.connect_sftp(label, &id)).await?;
                state.current(label, generation, &id, &slot)?;
                $access?;
                let result = $body;
                state.current(label, generation, &id, &slot)?;
                $access?;
                result.map_err(Into::into)
            })
        };
    }
    match command {
        "editor_sftp_list" => {
            let entry = arg!(Option<String>, "entryId");
            let panel = get!(slot.owners.lock().unwrap().panel.clone().ok_or(Error::Unavailable), reject);
            run!(attached, _slot, check_panel(&panel), attached.list(entry.as_deref()).await);
        }
        "editor_sftp_open" => {
            let entry = arg!(String, "entryId");
            let panel = get!(slot.owners.lock().unwrap().panel.clone().ok_or(Error::Unavailable), reject);
            run!(attached, _slot, check_panel(&panel), attached.open(&entry).await);
        }
        "editor_sftp_compare" | "editor_sftp_reload" => {
            let handle = arg!(String, "handle");
            let reload = command == "editor_sftp_reload";
            run!(attached, file_slot,
                attached.check_file(file_slot, &handle, &webview.state::<NanoBridge>(), webview.label(), generation, &id),
                attached.read(&handle, reload, |offer_id| webview.state::<NanoBridge>().is_mounted(webview.label(), generation, &id, offer_id, &handle)).await);
        }
        "editor_sftp_save" => {
            let handle = arg!(String, "handle");
            let text = arg!(String, "text");
            resolver.respond_async(async move {
                let mut guard = slot.state.lock().await;
                webview.state::<SftpIpc>().current(webview.label(), generation, &id, &slot)?;
                let Attachment::Active(attached) = &mut *guard else {
                    return Err(Error::Unavailable.into());
                };
                attached.check_file(&slot, &handle, &webview.state::<NanoBridge>(), webview.label(), generation, &id)?;
                // 교체를 시도한 뒤에는 세션이 닫혀도 코어가 확인한 저장 결과를 돌려준다.
                attached.save(&handle, &text, |offer_id| webview.state::<NanoBridge>().is_mounted(webview.label(), generation, &id, offer_id, &handle)).await.map_err(Into::into)
            });
        }
        "editor_sftp_close" => {
            let handle = arg!(String, "handle");
            resolver.respond_async(async move {
                webview.state::<SftpIpc>().close_file(&webview.state::<NanoBridge>(), webview.label(), generation, &id, &slot, &handle).await;
                Ok(())
            });
        }
        _ => unreachable!(),
    }
    true
}

#[cfg(test)]
mod tests;
