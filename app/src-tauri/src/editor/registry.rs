use std::{
    collections::HashMap,
    fs::File,
    io,
    path::Path,
    sync::{Arc, Mutex},
};

use serde::Serialize;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{LocalConflict, LocalFile, LocalVersion, OpenError, SaveError, SaveStage};
use super::cli::{Pending, WaitForClose};

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
    close_document: Option<(Document, tauri::Url)>,
    window_close: Option<WindowClose>,
}

struct WindowClose {
    document: Document,
    id: String,
    approved: bool,
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
        if label == "main" { registry.window_close = None; }
    }

    pub(crate) fn ready_for_window_close(&self, url: &tauri::Url) {
        let mut registry = self.0.lock().unwrap();
        if let Some((document, _)) = registry.documents.get("main") {
            registry.close_document = Some((*document, url.clone()));
        }
    }

    pub(crate) fn request_window_close(&self, url: &tauri::Url) -> Result<Option<String>, &'static str> {
        let mut registry = self.0.lock().unwrap();
        let Some(document) = registry.window_close_document(url) else { return Ok(None) };
        if let Some(request) = &registry.window_close {
            return Ok((!request.approved && request.document == document).then(|| request.id.clone()));
        }
        let id = registry.new_handle().map_err(|_| "창 닫기 요청 생성 실패")?;
        registry.window_close = Some(WindowClose { document, id: id.clone(), approved: false });
        Ok(Some(id))
    }

    pub(crate) fn respond_to_window_close(&self, url: &tauri::Url, id: &str, approved: bool) -> Result<(), &'static str> {
        let mut registry = self.0.lock().unwrap();
        let document = registry.window_close_document(url);
        let request = registry.window_close.as_mut().filter(|request| {
            !request.approved && request.id == id && Some(request.document) == document
        }).ok_or("창 닫기 요청이 만료되었습니다")?;
        if approved {
            request.approved = true;
        } else {
            registry.window_close = None;
        }
        Ok(())
    }

    pub(crate) fn with_approved_window_close<T>(&self, url: &tauri::Url, id: &str, close: impl FnOnce() -> T) -> Option<T> {
        let registry = self.0.lock().unwrap();
        let request = registry.window_close.as_ref()?;
        (request.approved && request.id == id && Some(request.document) == registry.window_close_document(url)).then(close)
    }

    pub(crate) fn clear_window_close(&self, id: &str, approved: bool) {
        let mut registry = self.0.lock().unwrap();
        if registry.window_close.as_ref().is_some_and(|request| request.id == id && request.approved == approved) {
            registry.window_close = None;
        }
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

    fn window_close_document(&self, url: &tauri::Url) -> Option<Document> {
        self.close_document.as_ref().filter(|(_, ready_url)| ready_url == url).map(|(document, _)| *document)
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
        if label == "main" {
            self.close_document = None;
            // 진행 중인 SSH 정리가 닫기 게이트를 풀 때까지 다음 요청을 기다린다.
            if self.window_close.as_ref().is_some_and(|request| !request.approved) {
                self.window_close = None;
            }
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
    use super::LocalEditors;

    #[test]
    fn close_requires_the_current_document_and_consumes_each_response() {
        let editors = LocalEditors::default();
        let url = "tauri://localhost/".parse().unwrap();
        let other = "tauri://localhost/other".parse().unwrap();
        editors.navigate("main");
        assert!(editors.request_window_close(&url).unwrap().is_none());
        editors.ready_for_window_close(&url);
        let id = editors.request_window_close(&url).unwrap().unwrap();
        assert_eq!(editors.request_window_close(&url).unwrap().as_deref(), Some(id.as_str()));
        assert!(editors.respond_to_window_close(&other, &id, true).is_err());
        editors.respond_to_window_close(&url, &id, false).unwrap();
        assert!(editors.respond_to_window_close(&url, &id, true).is_err());

        let approved = editors.request_window_close(&url).unwrap().unwrap();
        editors.respond_to_window_close(&url, &approved, true).unwrap();
        assert!(editors.respond_to_window_close(&url, &approved, true).is_err());
        assert_eq!(editors.with_approved_window_close(&url, &approved, || ()), Some(()));
        editors.navigate("main");
        editors.ready_for_window_close(&url);
        assert!(editors.with_approved_window_close(&url, &approved, || ()).is_none());
        assert!(editors.request_window_close(&url).unwrap().is_none());
        editors.clear_window_close(&approved, true);
        let pending = editors.request_window_close(&url).unwrap().unwrap();
        editors.navigate("main");
        editors.ready_for_window_close(&url);
        assert!(editors.respond_to_window_close(&url, &pending, true).is_err());
        let next = editors.request_window_close(&url).unwrap().unwrap();
        assert_ne!(next, pending);
        editors.destroy("main");
        assert!(editors.respond_to_window_close(&url, &next, true).is_err());
    }
}
