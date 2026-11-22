use crate::connections::probe::Runs;
use portable_pty::{CommandBuilder, PtySize};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, atomic::{AtomicU64, Ordering}},
};

pub(crate) mod ipc;
mod process;
pub(crate) mod probe;
mod remote;
mod saved;
#[cfg(all(test, any(windows, unix)))]
mod pty_flow_tests;
#[cfg(test)]
pub(crate) mod tests;

use process::{Launch, Session};
use remote::RemoteClose;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum LocalClose {
    #[default]
    NotStarted,
    CliReaped,
    Unconfirmed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct CloseResult {
    local: LocalClose,
    remote: RemoteClose,
    #[serde(skip_serializing)]
    local_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
}

impl CloseResult {
    fn warning(&self) -> Option<String> {
        if self.remote == RemoteClose::Unconfirmed {
            return self.target.as_ref().map(|target| format!("종료 확인 불가: {target}"));
        }
        self.local_error.clone().or_else(|| (self.local == LocalClose::Unconfirmed).then(|| "로컬 자식 종료 확인 불가".into()))
    }
}

enum Retired {
    Completed { owner: String, result: CloseResult },
    Unconfirmed(Arc<Session>),
}

impl Retired {
    fn owner(&self) -> &str {
        match self {
            Self::Completed { owner, .. } => owner,
            Self::Unconfirmed(session) => &session.owner,
        }
    }
}

#[derive(Default)]
pub(crate) struct Sessions {
    next: AtomicU64,
    registry: Arc<Mutex<Registry>>,
}

#[derive(Default)]
struct Registry {
    sessions: HashMap<String, Arc<Session>>,
    close_results: HashMap<String, Retired>,
    destroyed: HashSet<String>,
    documents: HashMap<String, u64>,
    probes: Runs,
    shutdown: bool,
}

impl Sessions {
    #[cfg(test)]
    fn open(&self, owner: &str, size: PtySize, command: CommandBuilder, send: impl Fn(Event) -> Result<(), String> + Send + 'static) -> Result<String, String> {
        self.open_document(owner, self.document(owner), size, Launch::Local(command), send)
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

    pub(crate) fn probe_completion(&self, owner: String, document: u64, run_id: String) -> impl FnOnce() + Send + use<> {
        let registry = Arc::downgrade(&self.registry);
        move || {
            if let Some(registry) = registry.upgrade() {
                let mut registry = registry.lock().unwrap();
                let current = registry.documents.get(&owner).copied().unwrap_or(0) == document && !registry.shutdown;
                registry.probes.reap(&run_id, current);
            }
        }
    }

    pub(crate) fn next_id(&self) -> String {
        self.next.fetch_add(1, Ordering::Relaxed).to_string()
    }

    fn open_document(&self, owner: &str, document: u64, size: PtySize, command: Launch, send: impl Fn(Event) -> Result<(), String> + Send + 'static) -> Result<String, String> {
        validate_size(size)?;
        let id = self.next_id();
        let session = Arc::new(Session::new(owner, command.is_ssm()));
        let ssh = matches!(command, Launch::Ssh(_));
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
        let result = session.launch(size, command, send, move |session| {
            let mut registry = registry.lock().unwrap();
            let result = session.close_result();
            if (session.is_ssm() || ssh && result.local != LocalClose::NotStarted) && !registry.shutdown
                && registry.documents.get(&session.owner).copied().unwrap_or(0) == document
            {
                let retired = if result.warning().is_some() {
                    Retired::Unconfirmed(Arc::clone(session))
                } else {
                    Retired::Completed { owner: session.owner.clone(), result }
                };
                registry.close_results.insert(worker_id.clone(), retired);
            }
            registry.sessions.remove(&worker_id);
        });
        if let Err(error) = result {
            session.wait_finished();
            self.registry.lock().unwrap().sessions.remove(&id);
            return Err(error);
        }
        Ok(id)
    }
    #[cfg(test)]
    pub(crate) fn reopen_ssh_fixture(&self, config: &std::path::Path, id: &str, output: tauri::ipc::Channel<tauri::ipc::InvokeResponseBody>) -> Result<String, String> {
        self.open_document("main", self.document("main"), size(24, 100), saved::launch(config, id)?,
            move |event| event.send(&output).map_err(|error| error.to_string()))
    }

    #[cfg(test)]
    pub(crate) fn close_ssh_fixture(&self, id: &str) {
        assert!(self.close("other", id).is_err());
        let result = self.close("main", id).unwrap();
        assert_eq!(result.local, LocalClose::CliReaped);
        assert_eq!(result.warning(), None);
        assert!(self.input("main", id, b"closed").is_err());
    }


    fn get(&self, owner: &str, id: &str) -> Result<Arc<Session>, String> {
        let registry = self.registry.lock().unwrap();
        let session = registry.sessions.get(id).cloned().or_else(|| {
            let probe = registry.probes.attached_probe(owner, id)?;
            probe.attached().then(|| probe.session())
        }).ok_or(CLOSED)?;
        if session.owner != owner {
            return Err("다른 창의 세션입니다".into());
        }
        Ok(session)
    }

    pub(crate) fn start(&self, owner: &str, id: &str) -> Result<(), String> {
        self.get(owner, id)?.start()
    }

    pub(crate) fn input(&self, owner: &str, id: &str, bytes: &[u8]) -> Result<(), String> {
        self.get(owner, id)?.input(bytes)
    }

    fn resize(&self, owner: &str, id: &str, size: PtySize) -> Result<(), String> {
        validate_size(size)?;
        self.get(owner, id)?.resize(size)
    }

    pub(crate) fn ack(&self, owner: &str, id: &str, position: u64) -> Result<(), String> {
        self.get(owner, id)?.ack(position)
    }

    fn close(&self, owner: &str, id: &str) -> Result<CloseResult, String> {
        let (session, retired) = {
            let mut registry = self.registry.lock().unwrap();
            match registry.close_results.get(id) {
                Some(entry) if entry.owner() != owner => return Err("다른 창의 세션입니다".into()),
                Some(Retired::Completed { .. }) => {
                    let Some(Retired::Completed { result, .. }) = registry.close_results.remove(id) else { unreachable!() };
                    return Ok(result);
                }
                Some(Retired::Unconfirmed(session)) => (Arc::clone(session), true),
                None => (registry.sessions.get(id).cloned().ok_or(CLOSED)?, false),
            }
        };
        if session.owner != owner { return Err("다른 창의 세션입니다".into()); }
        if retired {
            let result = session.recheck_cleanup();
            if result.warning().is_none() { self.registry.lock().unwrap().close_results.remove(id); }
            return Ok(result);
        }
        session.stop(process::Stop::Closed);
        session.wait_finished();
        let result = session.close_result();
        if result.warning().is_none() { self.registry.lock().unwrap().close_results.remove(id); }
        Ok(result)
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
        registry.close_results.retain(|_, entry| entry.owner() != owner);
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
            registry.close_results.clear();
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

pub(crate) fn probe_size(rows: u16, cols: u16) -> Result<PtySize, String> {
    let size = size(rows, cols);
    validate_size(size)?;
    Ok(size)
}

fn size(rows: u16, cols: u16) -> PtySize {
    PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
}

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
    Connecting,
    Ready,
}

impl Event {
    fn send(self, channel: &tauri::ipc::Channel<tauri::ipc::InvokeResponseBody>) -> tauri::Result<()> {
        use tauri::ipc::{InvokeResponseBody, IpcResponse};
        channel.send(match self {
            Self::Bytes(bytes) => InvokeResponseBody::Raw(bytes),
            Self::Exited(code) => End::Exited(code).body()?,
            Self::Disconnected(reason) => End::Disconnected(reason).body()?,
            Self::Connecting => End::Connecting.body()?,
            Self::Ready => End::Ready.body()?,
        })
    }
}

#[derive(Debug)]
enum Event {
    Bytes(Vec<u8>),
    Exited(u32),
    Disconnected(String),
    Connecting,
    Ready,
}

fn validate_working_directory(directory: &std::path::Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        // 확장·장치·UNC 경로에는 일반 Win32 경로 정규화를 적용할 수 없다.
        if !matches!(directory.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))) {
            return Err("작업 디렉터리: Windows 드라이브의 일반 절대 경로만 지원합니다".into());
        }
    }
    if !std::fs::metadata(directory).map_err(|error| stage("작업 디렉터리", error))?.is_dir() {
        return Err("작업 디렉터리: 디렉터리가 아닙니다".into());
    }
    Ok(())
}

#[cfg(windows)]
const POWERSHELL_CWD_MATCH: &str = concat!(
    "[string]::Equals(",
    "[IO.Path]::TrimEndingDirectorySeparator([IO.Path]::GetFullPath($directory)), ",
    "[IO.Path]::TrimEndingDirectorySeparator([IO.Path]::GetFullPath([Environment]::CurrentDirectory)), ",
    "[StringComparison]::Ordinal)",
);

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
