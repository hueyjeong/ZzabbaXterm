use std::{
    collections::HashMap,
    sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}},
};

use serde::Serialize;
use tauri::{Manager, ipc::{CommandArg, CommandItem, Invoke}};
use tokio::sync::Mutex as AsyncMutex;

use crate::terminal::Sessions;

use super::sftp::{self, Entry, File, Session};

mod response;
use response::{Error, SaveResult, SaveStage};

type Result<T, E = Error> = std::result::Result<T, E>;
type Slot = Arc<AttachmentSlot>;

#[derive(Default)]
struct AttachmentSlot {
    closing: AtomicBool,
    state: AsyncMutex<Attachment>,
}

#[derive(Default)]
enum Attachment {
    #[default]
    Opening,
    Active(Attached),
    Closing,
    CloseFailed(Error),
}

impl Attachment {
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
struct Opened {
    handle: String,
    name: String,
    text: String,
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

    fn reserve(&self, label: &str, generation: u64, id: &str) -> Result<Slot> {
        let mut documents = self.0.lock().unwrap();
        let document = documents.get_mut(label).filter(|doc| doc.generation == generation).ok_or(Error::DocumentClosed)?;
        let std::collections::hash_map::Entry::Vacant(entry) = document.parents.entry(id.into()) else {
            return Err(Error::Unavailable);
        };
        Ok(entry.insert(Arc::default()).clone())
    }

    fn slot(&self, label: &str, generation: u64, id: &str) -> Result<Option<Slot>> {
        let documents = self.0.lock().unwrap();
        let document = documents.get(label).filter(|doc| doc.generation == generation).ok_or(Error::DocumentClosed)?;
        Ok(document.parents.get(id).cloned())
    }

    fn current(&self, label: &str, generation: u64, id: &str, slot: &Slot) -> Result<()> {
        if self.slot(label, generation, id)?.is_some_and(|active| Arc::ptr_eq(&active, slot))
            && !slot.closing.load(Ordering::Acquire)
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
        Ok(document.parents.remove(id))
    }
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

    async fn refresh_transport(&mut self, sessions: &Sessions, owner: &str, id: &str) -> Result<()> {
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
        let result = sessions.connect_sftp(owner, id).await;
        let session = self.transport.finish(result)?;
        self.transport = Transport::Connected(session);
        Ok(())
    }

    async fn open(&mut self, entry_id: &str) -> Result<Opened> {
        let entry = self.browser.entries.get(entry_id).filter(|entry| !entry.is_directory()).ok_or(Error::InvalidEntry)?;
        let result = self.transport.connected()?.open(entry).await;
        let (file, text) = self.transport.finish(result)?;
        let handle = loop {
            let handle = opaque_id()?;
            if let std::collections::hash_map::Entry::Vacant(entry) = self.files.entry(handle.clone()) {
                entry.insert(OpenFile { file, unknown: false });
                break handle;
            }
        };
        Ok(Opened { handle, name: entry.name().into(), text })
    }

    async fn read(&mut self, handle: &str, reload: bool) -> Result<String> {
        let opened = self.files.get_mut(handle).ok_or(Error::InvalidHandle)?;
        let result = self.transport.connected()?.reopen(&opened.file).await;
        let (file, text) = self.transport.finish(result)?;
        if reload {
            *opened = OpenFile { file, unknown: false };
        }
        Ok(text)
    }

    async fn save(&mut self, handle: &str, text: &str) -> Result<SaveResult> {
        let opened = self.files.get_mut(handle).ok_or(Error::InvalidHandle)?;
        if opened.unknown {
            return Ok(SaveResult::failed(SaveStage::Prepare, Error::ReopenRequired));
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
                Ok(SaveResult::Unknown { error: error.into() })
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
        let slot = get!(state.reserve(label, generation, &id), reject);
        resolver.respond_async(async move {
            let state = webview.state::<SftpIpc>();
            let label = webview.label();
            let mut guard = slot.state.lock().await;
            state.current(label, generation, &id, &slot)?;
            let sessions = webview.state::<Sessions>();
            let result = match sessions.connect_sftp(label, &id).await {
                Ok(session) => {
                    let mut attached = Attached::new(session);
                    let result = if slot.closing.load(Ordering::Acquire) {
                        Err(Error::Disconnected)
                    } else {
                        attached.list(None).await
                    };
                    *guard = Attachment::Active(attached);
                    result
                }
                Err(sftp::Error::Reap(_)) => {
                    *guard = Attachment::CloseFailed(Error::RemoteFailure);
                    Err(Error::RemoteFailure)
                }
                Err(error) => Err(error.into()),
            };
            state.current(label, generation, &id, &slot)?;
            match result {
                Ok(listing) => Ok(listing),
                Err(error) => {
                    if let Err(cleanup) = guard.close().await {
                        return Err(cleanup.into());
                    }
                    let _ = state.remove(label, generation, &id, Some(&slot));
                    Err(error.into())
                }
            }
        });
        return true;
    }
    if command == "editor_sftp_detach" {
        let slot = get!(state.slot(label, generation, &id), reject);
        if let Some(slot) = &slot {
            slot.closing.store(true, Ordering::Release);
        }
        resolver.respond_async(async move {
            let Some(slot) = slot else { return Ok(()) };
            let state = webview.state::<SftpIpc>();
            let mut guard = slot.state.lock().await;
            guard.close().await?;
            let _ = state.remove(webview.label(), generation, &id, Some(&slot));
            Ok(())
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
        ($attached:ident, $body:expr) => {
            resolver.respond_async(async move {
                let state = webview.state::<SftpIpc>();
                let sessions = webview.state::<Sessions>();
                let label = webview.label();
                let mut guard = slot.state.lock().await;
                state.current(label, generation, &id, &slot)?;
                let Attachment::Active($attached) = &mut *guard else {
                    return Err(Error::Unavailable.into());
                };
                $attached.refresh_transport(&sessions, label, &id).await?;
                state.current(label, generation, &id, &slot)?;
                let result = $body;
                state.current(label, generation, &id, &slot)?;
                result.map_err(Into::into)
            })
        };
    }
    match command {
        "editor_sftp_list" => {
            let entry = arg!(Option<String>, "entryId");
            run!(attached, attached.list(entry.as_deref()).await);
        }
        "editor_sftp_open" => {
            let entry = arg!(String, "entryId");
            run!(attached, attached.open(&entry).await);
        }
        "editor_sftp_compare" | "editor_sftp_reload" => {
            let handle = arg!(String, "handle");
            let reload = command == "editor_sftp_reload";
            run!(attached, attached.read(&handle, reload).await);
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
                // 교체를 시도한 뒤에는 세션이 닫혀도 코어가 확인한 저장 결과를 돌려준다.
                attached.save(&handle, &text).await.map_err(Into::into)
            });
        }
        "editor_sftp_close" => {
            let handle = arg!(String, "handle");
            resolver.respond_async(async move {
                let state = webview.state::<SftpIpc>();
                let mut guard = slot.state.lock().await;
                if state.current(webview.label(), generation, &id, &slot).is_ok() {
                    if let Attachment::Active(attached) = &mut *guard {
                        attached.files.remove(&handle);
                    }
                }
                Ok(())
            });
        }
        _ => unreachable!(),
    }
    true
}

#[cfg(test)]
mod tests;
