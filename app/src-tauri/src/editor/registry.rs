use std::{
    collections::HashMap,
    fs::File,
    io,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{LocalConflict, LocalFile, LocalVersion, OpenError, SaveError, SaveStage};
use super::cli::{Pending, WaitForClose};
use crate::workspace_window::{EXPIRED, MAIN, WindowKey};

pub(crate) const CLOSE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Document(u64);

#[derive(Default)]
pub(crate) struct LocalEditors(Mutex<Registry>);

#[derive(Default)]
struct Registry {
    next_document: u64,
    documents: HashMap<String, (Document, CancellationToken)>,
    files: HashMap<String, OpenFile>,
    offers: HashMap<String, Offer>,
    window_close: Option<WindowClose>,
    picker: Option<(WindowKey, std::sync::Weak<()>)>,
}

struct WindowClose {
    window: WindowKey,
    document: Option<Document>,
    url: Option<tauri::Url>,
    id: String,
    revoked: CancellationToken,
    phase: ClosePhase,
}

enum ClosePhase {
    AwaitingUi {
        deadline: Instant,
        reply: oneshot::Sender<CloseReport>,
    },
    Checking,
    DestroyQueued,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum CloseDecision {
    Ready,
    Blocked,
    Discard { files: Vec<String> },
}

pub(crate) struct CloseReport {
    pub(crate) decision: CloseDecision,
    pub(crate) completed: oneshot::Sender<Result<bool, String>>,
}

pub(crate) struct CloseAttempt {
    pub(crate) id: String,
    pub(crate) deadline: Instant,
    pub(crate) revoked: CancellationToken,
    pub(crate) reply: Option<oneshot::Receiver<CloseReport>>,
}

struct OpenFile {
    document: Document,
    file: Arc<Mutex<LocalFile>>,
    wait: Option<WaitForClose>,
}

struct Offer {
    document: Document,
    handle: String,
    file: LocalFile,
    text: Option<String>,
    pending: Pending,
    _done: oneshot::Sender<()>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub(super) enum Selected {
    Cancelled,
    Opened { handle: String, name: String, text: String },
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub(super) enum EditorError {
    DocumentClosed,
    Busy,
    InvalidHandle,
    InvalidSelection,
    InvalidUtf8,
    ReadFailed { os_code: Option<i32> },
    SaveFailed { stage: SaveStage, os_code: Option<i32> },
    PublicationUnconfirmed { recovery_path: String },
    Conflict { conflict: Conflict },
    Unavailable,
}

#[derive(Debug, Serialize)]
#[serde(tag = "reason", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub(super) enum Conflict {
    Modified { disk_text: Option<String> },
    Replaced { disk_text: Option<String> },
    Missing,
    PathChanged,
    NotRegular,
    ChangedDuringRead,
    PermissionsChanged,
}

impl From<LocalConflict> for EditorError {
    fn from(conflict: LocalConflict) -> Self {
        fn text(version: LocalVersion) -> Option<String> {
            match version {
                LocalVersion::Text(text) => Some(text),
                LocalVersion::NonUtf8(_) => None,
            }
        }

        Self::Conflict {
            conflict: match conflict {
                LocalConflict::Modified(version) => Conflict::Modified { disk_text: text(version) },
                LocalConflict::Replaced(version) => Conflict::Replaced { disk_text: text(version) },
                LocalConflict::Missing => Conflict::Missing,
                LocalConflict::PathChanged => Conflict::PathChanged,
                LocalConflict::NotRegular => Conflict::NotRegular,
                LocalConflict::ChangedDuringRead => Conflict::ChangedDuringRead,
                LocalConflict::PermissionsChanged => Conflict::PermissionsChanged,
            },
        }
    }
}

impl From<OpenError> for EditorError {
    fn from(error: OpenError) -> Self {
        match error {
            OpenError::InvalidPath => Self::InvalidSelection,
            OpenError::InvalidUtf8 => Self::InvalidUtf8,
            OpenError::Conflict(conflict) => conflict.into(),
            OpenError::Io(source) => Self::ReadFailed { os_code: source.raw_os_error() },
        }
    }
}

impl From<SaveError> for EditorError {
    fn from(error: SaveError) -> Self {
        match error {
            SaveError::Conflict(conflict) => conflict.into(),
            SaveError::Io { stage, source } => Self::SaveFailed { stage, os_code: source.raw_os_error() },
            SaveError::Revoked => Self::DocumentClosed,
            SaveError::PublicationUnconfirmed { recovery_path } => Self::PublicationUnconfirmed {
                recovery_path: recovery_path.to_str().map_or_else(|| format!("{recovery_path:?}"), str::to_owned),
            },
        }
    }
}

impl LocalEditors {
    pub(crate) fn navigate(&self, label: &str) {
        let mut registry = self.0.lock().unwrap();
        registry.revoke(label);
        registry.next_document += 1;
        let document = Document(registry.next_document);
        registry.documents.insert(label.into(), (document, CancellationToken::new()));
    }

    pub(crate) fn destroy(&self, label: &str) {
        let mut registry = self.0.lock().unwrap();
        registry.revoke(label);
        if label == MAIN && registry.window_close.as_ref().is_some_and(|request| matches!(request.phase, ClosePhase::DestroyQueued)) {
            registry.window_close = None;
        }
    }

    pub(crate) fn request_window_close(&self, window: WindowKey, url: Option<tauri::Url>, trusted: bool) -> Result<Option<CloseAttempt>, &'static str> {
        let mut registry = self.0.lock().unwrap();
        if registry.window_close.is_some() || registry.picker_active(window) { return Ok(None); }
        let id = registry.new_handle().map_err(|_| "창 닫기 요청 생성 실패")?;
        let document = registry.main_document();
        let revoked = CancellationToken::new();
        let deadline = Instant::now() + CLOSE_RESPONSE_TIMEOUT;
        let (phase, reply) = if document.is_some() && url.is_some() && trusted {
            let (send, reply) = oneshot::channel();
            (ClosePhase::AwaitingUi { deadline, reply: send }, Some(reply))
        } else {
            (ClosePhase::Checking, None)
        };
        registry.window_close = Some(WindowClose { window, document, url, id: id.clone(), revoked: revoked.clone(), phase });
        Ok(Some(CloseAttempt { id, deadline, revoked, reply }))
    }

    pub(crate) fn pending_window_close(&self, window: WindowKey, url: &tauri::Url) -> Option<String> {
        let registry = self.0.lock().unwrap();
        registry.awaiting_ui(window, url).map(|request| request.id.clone())
    }

    pub(crate) fn respond_to_window_close(&self, window: WindowKey, url: &tauri::Url, id: &str, decision: CloseDecision)
        -> Result<oneshot::Receiver<Result<bool, String>>, &'static str> {
        let mut registry = self.0.lock().unwrap();
        if registry.awaiting_ui(window, url).is_none_or(|request| request.id != id) { return Err(EXPIRED); }
        let request = registry.window_close.as_mut().unwrap();
        let ClosePhase::AwaitingUi { reply, .. } = std::mem::replace(&mut request.phase, ClosePhase::Checking) else { unreachable!() };
        let (completed, result) = oneshot::channel();
        reply.send(CloseReport { decision, completed }).map_err(|_| EXPIRED)?;
        Ok(result)
    }

    pub(crate) fn use_close_fallback(&self, window: WindowKey, id: &str) -> bool {
        let mut registry = self.0.lock().unwrap();
        let document = registry.main_document();
        let Some(request) = registry.window_close.as_mut().filter(|request| {
            request.window == window && request.document == document && request.id == id && !request.revoked.is_cancelled()
        }) else { return false; };
        if !matches!(request.phase, ClosePhase::AwaitingUi { .. }) { return false; }
        request.phase = ClosePhase::Checking;
        true
    }

    pub(crate) fn check_window_close(&self, window: WindowKey, id: &str, url: Option<&tauri::Url>) -> bool {
        self.0.lock().unwrap().checking_close(window, id, url)
    }

    pub(crate) fn queue_window_destroy(&self, window: WindowKey, id: &str, url: Option<&tauri::Url>, destroy: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        let mut registry = self.0.lock().unwrap();
        if !registry.checking_close(window, id, url) { return Err(EXPIRED.into()); }
        destroy()?;
        registry.window_close.as_mut().unwrap().phase = ClosePhase::DestroyQueued;
        Ok(())
    }

    pub(crate) fn clear_window_close(&self, id: &str) {
        let mut registry = self.0.lock().unwrap();
        if registry.window_close.as_ref().is_some_and(|request| request.id == id) { registry.window_close = None; }
    }

    pub(super) fn begin_main_picker(&self, window: WindowKey, document: Document) -> Result<Arc<()>, EditorError> {
        let mut registry = self.0.lock().unwrap();
        registry.check_document(MAIN, document)?;
        if registry.picker_active(window) || registry.window_close.is_some() { return Err(EditorError::Busy); }
        let picker = Arc::new(());
        registry.picker = Some((window, Arc::downgrade(&picker)));
        Ok(picker)
    }

    pub(super) fn document(&self, label: &str) -> Result<Document, EditorError> {
        self.0.lock().unwrap().documents.get(label).map(|(document, _)| *document).ok_or(EditorError::DocumentClosed)
    }

    pub(super) fn scope(&self, label: &str) -> Result<(Document, CancellationToken), EditorError> {
        self.0.lock().unwrap().documents.get(label).cloned().ok_or(EditorError::DocumentClosed)
    }

    pub(super) fn selected(
        &self,
        label: &str,
        document: Document,
        path: Option<&Path>,
    ) -> Result<Selected, EditorError> {
        self.0.lock().unwrap().check_document(label, document)?;
        let Some(path) = path else { return Ok(Selected::Cancelled) };
        let (file, text) = LocalFile::open(path)?;
        let name = file.display_name();
        let mut registry = self.0.lock().unwrap();
        registry.check_document(label, document)?;
        let handle = registry.new_handle()?;
        registry.files.insert(handle.clone(), OpenFile { document, file: Arc::new(Mutex::new(file)), wait: None });
        Ok(Selected::Opened { handle, name, text })
    }

    pub(super) fn offer(
        &self, label: &str, document: Document, pending: Pending, file: LocalFile, text: String,
    ) -> Result<(String, oneshot::Receiver<()>), EditorError> {
        let mut registry = self.0.lock().unwrap();
        registry.check_document(label, document)?;
        if pending.is_inactive() { return Err(EditorError::Unavailable); }
        let (id, handle) = match (registry.new_handle(), registry.new_handle()) {
            (Ok(id), Ok(handle)) if id != handle => (id, handle),
            _ => {
                pending.open_failed();
                return Err(EditorError::Unavailable);
            }
        };
        let (done, completed) = oneshot::channel();
        registry.offers.insert(id.clone(), Offer { document, handle, file, text: Some(text), pending, _done: done });
        Ok((id, completed))
    }

    pub(super) fn pending_offers(&self, label: &str, document: Document) -> Result<Vec<String>, EditorError> {
        let registry = self.0.lock().unwrap();
        registry.check_document(label, document)?;
        Ok(registry.offers.iter()
            .filter(|(_, offer)| offer.document == document && offer.text.is_some() && !offer.pending.is_inactive())
            .map(|(id, _)| id.clone())
            .collect())
    }

    pub(super) fn claim(&self, label: &str, document: Document, id: &str) -> Result<Selected, EditorError> {
        let mut registry = self.0.lock().unwrap();
        registry.check_document(label, document)?;
        let offer = registry.offers.get_mut(id).filter(|offer| offer.document == document && !offer.pending.is_inactive())
            .ok_or(EditorError::InvalidHandle)?;
        let text = offer.text.take().ok_or(EditorError::InvalidHandle)?;
        Ok(Selected::Opened { handle: offer.handle.clone(), name: offer.file.display_name(), text })
    }

    pub(super) fn respond_to_offer(
        &self, label: &str, document: Document, id: &str, handle: &str, attached: bool,
    ) -> Result<(), EditorError> {
        let mut registry = self.0.lock().unwrap();
        registry.check_document(label, document)?;
        let valid = registry.offers.get(id).is_some_and(|offer| {
            offer.document == document && offer.handle == handle && offer.text.is_none() && !offer.pending.is_inactive()
        });
        if !valid { return Err(EditorError::InvalidHandle); }
        let Offer { handle, file, pending, _done, .. } = registry.offers.remove(id).unwrap();
        if !attached {
            pending.open_failed();
            return Ok(());
        }
        // 문서 폐기와 탭 닫기는 대기 토큰이 설치된 뒤에만 이 항목을 볼 수 있다.
        let wait = pending.opened().map_err(|()| EditorError::Unavailable)?;
        registry.files.insert(handle, OpenFile { document, file: Arc::new(Mutex::new(file)), wait: Some(wait) });
        Ok(())
    }

    pub(super) fn discard_offer(&self, document: Document, id: &str, failed: bool) {
        let mut registry = self.0.lock().unwrap();
        if registry.offers.get(id).is_some_and(|offer| offer.document == document) {
            let offer = registry.offers.remove(id).unwrap();
            if failed { offer.pending.open_failed(); }
        }
    }

    pub(super) fn save(&self, label: &str, document: Document, handle: &str, text: &str) -> Result<(), EditorError> {
        self.save_with_sync(label, document, handle, text, File::sync_all)
    }

    pub(super) fn save_with_sync(
        &self, label: &str, document: Document, handle: &str, text: &str,
        sync: impl FnOnce(&File) -> io::Result<()>,
    ) -> Result<(), EditorError> {
        let file = Arc::clone(self.0.lock().unwrap().file(label, document, handle)?);
        let mut file = file.lock().map_err(|_| EditorError::Unavailable)?;
        self.0.lock().unwrap().file(label, document, handle)?;
        file.save_checked(text, sync, || {
            // 파일 게시와 기준 갱신이 끝날 때까지 문서와 탭을 유지한다.
            let registry = self.0.lock().unwrap();
            registry.file(label, document, handle).map_err(|_| SaveError::Revoked)?;
            Ok(registry)
        }).map_err(Into::into)
    }

    pub(super) fn compare(&self, label: &str, document: Document, handle: &str) -> Result<String, EditorError> {
        self.with_file(label, document, handle, |file| file.compare().map_err(Into::into))
    }

    pub(super) fn reload(&self, label: &str, document: Document, handle: &str) -> Result<String, EditorError> {
        self.with_file(label, document, handle, |file| file.reload().map_err(Into::into))
    }

    pub(super) fn close(&self, label: &str, document: Document, handle: &str) -> Result<(), EditorError> {
        let mut registry = self.0.lock().unwrap();
        registry.file(label, document, handle)?;
        if let Some(wait) = registry.files.remove(handle).and_then(|file| file.wait) {
            wait.closed();
        }
        Ok(())
    }

    fn with_file<T>(
        &self,
        label: &str,
        document: Document,
        handle: &str,
        action: impl FnOnce(&mut LocalFile) -> Result<T, EditorError>,
    ) -> Result<T, EditorError> {
        let file = Arc::clone(self.0.lock().unwrap().file(label, document, handle)?);
        let mut file = file.lock().map_err(|_| EditorError::Unavailable)?;
        // 파일 잠금을 기다리는 동안 닫히거나 문서가 바뀐 요청은 실행하지 않는다.
        self.0.lock().unwrap().file(label, document, handle)?;
        let result = action(&mut file);
        self.0.lock().unwrap().file(label, document, handle)?;
        result
    }
}

impl Registry {
    fn new_handle(&self) -> Result<String, EditorError> {
        loop {
            let mut random = [0; 16];
            getrandom::fill(&mut random).map_err(|_| EditorError::Unavailable)?;
            let handle = format!("{:032x}", u128::from_ne_bytes(random));
            if !self.files.contains_key(&handle) && !self.offers.contains_key(&handle)
                && !self.offers.values().any(|offer| offer.handle == handle)
            {
                return Ok(handle);
            }
        }
    }

    fn main_document(&self) -> Option<Document> {
        self.documents.get(MAIN).map(|(document, _)| *document)
    }

    fn awaiting_ui(&self, window: WindowKey, url: &tauri::Url) -> Option<&WindowClose> {
        let request = self.window_close.as_ref()?;
        (request.window == window && request.document.is_some() && request.document == self.main_document()
            && request.url.as_ref() == Some(url) && !request.revoked.is_cancelled()
            && matches!(request.phase, ClosePhase::AwaitingUi { deadline, .. } if Instant::now() < deadline)).then_some(request)
    }

    fn picker_active(&self, window: WindowKey) -> bool {
        self.picker.as_ref().is_some_and(|(owner, picker)| *owner == window && picker.strong_count() != 0)
    }

    fn checking_close(&self, window: WindowKey, id: &str, url: Option<&tauri::Url>) -> bool {
        self.window_close.as_ref().is_some_and(|request| request.window == window && request.document == self.main_document()
            && request.id == id && !request.revoked.is_cancelled() && matches!(request.phase, ClosePhase::Checking)
            && !self.picker_active(window) && request.url.as_ref() == url)
    }

    fn check_document(&self, label: &str, document: Document) -> Result<(), EditorError> {
        if self.documents.get(label).map(|(owner, _)| *owner) != Some(document) {
            return Err(EditorError::DocumentClosed);
        }
        Ok(())
    }

    fn file(&self, label: &str, document: Document, handle: &str) -> Result<&Arc<Mutex<LocalFile>>, EditorError> {
        self.check_document(label, document)?;
        match self.files.get(handle) {
            Some(file) if file.document == document => Ok(&file.file),
            _ => Err(EditorError::InvalidHandle),
        }
    }

    fn revoke(&mut self, label: &str) {
        if label == MAIN {
            // 이전 시도의 SSH 게이트가 해제되기 전에는 다음 시도를 시작하지 않는다.
            if let Some(request) = &self.window_close { request.revoked.cancel(); }
        }
        if let Some((document, lifetime)) = self.documents.remove(label) {
            lifetime.cancel();
            self.offers.retain(|_, offer| offer.document != document);
            self.files.retain(|_, file| file.document != document);
        }
    }
}

#[cfg(test)]
mod window_close_tests {
    use super::{CloseDecision, ClosePhase, EditorError, LocalEditors, EXPIRED, MAIN};
    use std::time::Instant;

    const WINDOW: usize = 1;

    #[test]
    fn report_requires_the_current_window_document_url_and_id_and_is_consumed_once() {
        let editors = LocalEditors::default();
        let url: tauri::Url = "tauri://localhost/".parse().unwrap();
        let other = "tauri://localhost/other".parse().unwrap();
        editors.navigate(MAIN);
        let mut attempt = editors.request_window_close(WINDOW, Some(url.clone()), true).unwrap().unwrap();
        assert_eq!(editors.pending_window_close(WINDOW, &url).as_deref(), Some(attempt.id.as_str()));
        assert!(editors.request_window_close(WINDOW, Some(url.clone()), true).unwrap().is_none());
        for (window, url, id) in [(WINDOW + 1, &url, attempt.id.as_str()), (WINDOW, &other, attempt.id.as_str()), (WINDOW, &url, "old")] {
            assert_eq!(editors.respond_to_window_close(window, url, id, CloseDecision::Ready).err(), Some(EXPIRED));
        }
        assert!(!editors.check_window_close(WINDOW, &attempt.id, Some(&url)));
        let mut result = editors.respond_to_window_close(WINDOW, &url, &attempt.id, CloseDecision::Blocked).unwrap();
        assert!(editors.pending_window_close(WINDOW, &url).is_none());
        assert!(editors.respond_to_window_close(WINDOW, &url, &attempt.id, CloseDecision::Ready).is_err());
        assert!(!editors.use_close_fallback(WINDOW, &attempt.id));
        let report = attempt.reply.as_mut().unwrap().try_recv().unwrap();
        assert!(matches!(report.decision, CloseDecision::Blocked));
        report.completed.send(Ok(false)).unwrap();
        assert_eq!(result.try_recv().unwrap(), Ok(false));
    }

    #[test]
    fn stalled_ui_expires_before_fallback_and_cannot_reenter_after_timeout() {
        let editors = LocalEditors::default();
        let url: tauri::Url = "tauri://localhost/".parse().unwrap();
        editors.navigate(MAIN);
        let attempt = editors.request_window_close(WINDOW, Some(url.clone()), true).unwrap().unwrap();
        let mut registry = editors.0.lock().unwrap();
        let ClosePhase::AwaitingUi { deadline, .. } = &mut registry.window_close.as_mut().unwrap().phase else { panic!() };
        *deadline = Instant::now();
        drop(registry);
        assert!(editors.pending_window_close(WINDOW, &url).is_none());
        assert_eq!(editors.respond_to_window_close(WINDOW, &url, &attempt.id, CloseDecision::Ready).err(), Some(EXPIRED));
        assert!(editors.use_close_fallback(WINDOW, &attempt.id));
        assert!(!editors.use_close_fallback(WINDOW, &attempt.id));
        assert!(editors.check_window_close(WINDOW, &attempt.id, Some(&url)));
        assert!(!editors.check_window_close(WINDOW + 1, &attempt.id, Some(&url)));
        assert!(editors.respond_to_window_close(WINDOW, &url, &attempt.id, CloseDecision::Ready).is_err());
        editors.clear_window_close(&attempt.id);
        assert!(editors.request_window_close(WINDOW, Some(url), true).unwrap().is_some());
    }

    #[test]
    fn navigation_revokes_an_answered_close_and_a_fallback_without_a_document() {
        let editors = LocalEditors::default();
        let url: tauri::Url = "tauri://localhost/".parse().unwrap();
        let initial = editors.request_window_close(WINDOW, None, false).unwrap().unwrap();
        assert!(initial.reply.is_none());
        assert!(editors.check_window_close(WINDOW, &initial.id, None));
        editors.navigate(MAIN);
        assert!(initial.revoked.is_cancelled());
        assert!(!editors.check_window_close(WINDOW, &initial.id, None));
        editors.clear_window_close(&initial.id);

        let mut attempt = editors.request_window_close(WINDOW, Some(url.clone()), true).unwrap().unwrap();
        let _result = editors.respond_to_window_close(WINDOW, &url, &attempt.id, CloseDecision::Ready).unwrap();
        assert!(matches!(attempt.reply.as_mut().unwrap().try_recv().unwrap().decision, CloseDecision::Ready));
        editors.navigate(MAIN);
        assert!(attempt.revoked.is_cancelled());
        assert_eq!(editors.queue_window_destroy(WINDOW, &attempt.id, Some(&url), || panic!("old document")), Err(EXPIRED.into()));
        assert!(editors.request_window_close(WINDOW, Some(url.clone()), true).unwrap().is_none());
        editors.clear_window_close(&attempt.id);
        let next = editors.request_window_close(WINDOW, Some(url.clone()), true).unwrap().unwrap();
        assert_ne!(next.id, attempt.id);
        assert!(editors.respond_to_window_close(WINDOW, &url, &attempt.id, CloseDecision::Ready).is_err());
        editors.destroy(MAIN);
        assert!(next.revoked.is_cancelled());
        assert!(editors.respond_to_window_close(WINDOW, &url, &next.id, CloseDecision::Ready).is_err());
        editors.clear_window_close(&next.id);
        assert!(editors.request_window_close(WINDOW, None, false).unwrap().is_some());
    }

    #[test]
    fn active_picker_survives_navigation_and_excludes_native_close() {
        let editors = LocalEditors::default();
        editors.navigate(MAIN);
        let picker = editors.begin_main_picker(WINDOW, editors.document(MAIN).unwrap()).unwrap();
        editors.navigate(MAIN);
        let url: tauri::Url = "https://external.invalid/".parse().unwrap();
        assert!(editors.request_window_close(WINDOW, Some(url.clone()), false).unwrap().is_none());
        drop(picker);
        let attempt = editors.request_window_close(WINDOW, Some(url.clone()), false).unwrap().unwrap();
        assert!(attempt.reply.is_none());
        assert!(editors.respond_to_window_close(WINDOW, &url, &attempt.id, CloseDecision::Ready).is_err());
        assert!(matches!(editors.begin_main_picker(WINDOW, editors.document(MAIN).unwrap()), Err(EditorError::Busy)));
        assert!(editors.check_window_close(WINDOW, &attempt.id, Some(&url)));
        assert_eq!(editors.queue_window_destroy(WINDOW, &attempt.id, Some(&url), || Err("destroy failed".into())), Err("destroy failed".into()));
        assert!(editors.check_window_close(WINDOW, &attempt.id, Some(&url)));
        assert!(editors.queue_window_destroy(WINDOW, &attempt.id, Some(&url), || Ok(())).is_ok());
        assert!(editors.request_window_close(WINDOW, Some(url), false).unwrap().is_none());
    }
}
