use crate::connections::probe::Runs;
use portable_pty::{CommandBuilder, PtySize};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, atomic::{AtomicU64, Ordering}},
};

pub(crate) mod ipc;
mod process;
pub(crate) mod probe;
mod saved;
#[cfg(all(test, any(windows, unix)))]
mod pty_flow_tests;
#[cfg(test)]
mod tests;

use process::Session;

#[derive(Default)]
pub(crate) struct Sessions {
    next: AtomicU64,
    registry: Arc<Mutex<Registry>>,
}

#[derive(Default)]
struct Registry {
    sessions: HashMap<String, Arc<Session>>,
    destroyed: HashSet<String>,
    documents: HashMap<String, u64>,
    probes: Runs,
    shutdown: bool,
}

impl Sessions {
    #[cfg(test)]
    fn open(&self, owner: &str, size: PtySize, command: CommandBuilder, send: impl Fn(Event) -> Result<(), String> + Send + 'static) -> Result<String, String> {
        self.open_document(owner, self.document(owner), size, command, send)
    }

    pub(crate) fn document(&self, owner: &str) -> u64 {
        self.registry.lock().unwrap().documents.get(owner).copied().unwrap_or(0)
    }

    pub(crate) fn with_probes<T>(&self, owner: &str, document: u64, use_probes: impl FnOnce(&mut Runs) -> Result<T, String>) -> Result<T, String> {
        let mut registry = self.registry.lock().unwrap();
        if registry.shutdown || registry.destroyed.contains(owner) {
            return Err("창이 닫혔습니다".into());
        }
        if registry.documents.get(owner).copied().unwrap_or(0) != document {
            return Err("문서가 바뀌었습니다".into());
        }
        use_probes(&mut registry.probes)
    }

    pub(crate) fn next_id(&self) -> String {
        self.next.fetch_add(1, Ordering::Relaxed).to_string()
    }

    fn open_document(&self, owner: &str, document: u64, size: PtySize, command: CommandBuilder, send: impl Fn(Event) -> Result<(), String> + Send + 'static) -> Result<String, String> {
        validate_size(size)?;
        let id = self.next_id();
        let session = Arc::new(Session::new(owner));
        {
            let mut registry = self.registry.lock().unwrap();
            if registry.shutdown || registry.destroyed.contains(owner) {
                return Err("open: 창이 닫혔습니다".into());
            }
            if registry.documents.get(owner).copied().unwrap_or(0) != document {
                return Err("open: 문서가 바뀌었습니다".into());
            }
            registry.sessions.insert(id.clone(), session.clone());
        }
        let registry = Arc::clone(&self.registry);
        let worker_id = id.clone();
        let result = session.launch(size, command, send, move || {
            registry.lock().unwrap().sessions.remove(&worker_id);
        });
        if let Err(error) = result {
            session.wait_finished();
            self.registry.lock().unwrap().sessions.remove(&id);
            return Err(error);
        }
        Ok(id)
    }

    fn get(&self, owner: &str, id: &str) -> Result<Arc<Session>, String> {
        let session = self.registry.lock().unwrap().sessions.get(id).cloned()
            .ok_or(CLOSED)?;
        if session.owner != owner {
            return Err("다른 창의 세션입니다".into());
        }
        Ok(session)
    }

    fn start(&self, owner: &str, id: &str) -> Result<(), String> {
        self.get(owner, id)?.start()
    }

    fn input(&self, owner: &str, id: &str, bytes: &[u8]) -> Result<(), String> {
        self.get(owner, id)?.input(bytes)
    }

    fn resize(&self, owner: &str, id: &str, size: PtySize) -> Result<(), String> {
        validate_size(size)?;
        self.get(owner, id)?.resize(size)
    }

    fn ack(&self, owner: &str, id: &str, position: u64) -> Result<(), String> {
        self.get(owner, id)?.ack(position)
    }

    fn close(&self, owner: &str, id: &str) -> Result<(), String> {
        let session = self.get(owner, id)?;
        session.stop(process::Stop::Closed);
        session.wait_finished();
        Ok(())
    }

    pub(crate) fn navigate(&self, owner: &str) {
        self.stop_owner(owner, false);
    }

    pub(crate) fn destroy(&self, owner: &str) {
        self.stop_owner(owner, true);
    }

    fn stop_owner(&self, owner: &str, destroyed: bool) {
        let mut registry = self.registry.lock().unwrap();
        *registry.documents.entry(owner.into()).or_default() += 1;
        if destroyed {
            registry.destroyed.insert(owner.into());
        }
        registry.probes.stop_owner(owner);
        for session in registry.sessions.values().filter(|session| session.owner == owner) {
            session.stop(process::Stop::Closed);
        }
    }

    pub(crate) fn shutdown(&self) {
        let (sessions, probes) = {
            let mut registry = self.registry.lock().unwrap();
            registry.shutdown = true;
            (registry.sessions.values().cloned().collect::<Vec<_>>(), registry.probes.stop_all())
        };
        for session in &sessions {
            session.stop(process::Stop::Closed);
        }
        for session in sessions {
            session.wait_finished();
        }
        probes.iter().for_each(|probe| probe.wait_finished());
    }
}

fn validate_size(size: PtySize) -> Result<(), String> {
    if !(1..=i16::MAX as u16).contains(&size.rows) || !(1..=i16::MAX as u16).contains(&size.cols) {
        return Err("size: 행과 열은 각각 1~32767이어야 합니다".into());
    }
    Ok(())
}

fn stage(name: &str, error: impl std::fmt::Display) -> String {
    format!("{name}: {error}")
}

fn size(rows: u16, cols: u16) -> PtySize {
    PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
}

type SendEvent = dyn Fn(Event) -> Result<(), String> + Send;

const CHUNK: usize = 16 * 1024;
const WINDOW: usize = 256 * 1024;

const CLOSED: &str = "세션이 닫혔습니다";

#[cfg(test)]
impl Drop for Sessions {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum End {
    Exited(u32),
    Disconnected(String),
}

impl Event {
    fn send(self, channel: &tauri::ipc::Channel<tauri::ipc::InvokeResponseBody>) -> tauri::Result<()> {
        use tauri::ipc::{InvokeResponseBody, IpcResponse};
        channel.send(match self {
            Self::Bytes(bytes) => InvokeResponseBody::Raw(bytes),
            Self::Exited(code) => End::Exited(code).body()?,
            Self::Disconnected(reason) => End::Disconnected(reason).body()?,
        })
    }
}

#[derive(Debug)]
enum Event {
    Bytes(Vec<u8>),
    Exited(u32),
    Disconnected(String),
}

fn shell() -> CommandBuilder {
    #[cfg(windows)]
    let mut cmd = CommandBuilder::new("pwsh.exe");
    #[cfg(unix)]
    let mut cmd = CommandBuilder::new("/bin/bash");
    #[cfg(windows)]
    cmd.args(["-NoLogo", "-NoProfile"]);
    #[cfg(unix)]
    cmd.args(["--noprofile", "--norc", "-i"]);
    cmd.env("TERM", "xterm-256color");
    cmd
}
