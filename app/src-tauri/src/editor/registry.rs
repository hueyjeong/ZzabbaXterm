use std::{
    collections::{HashMap, hash_map::Entry},
    path::Path,
    sync::{Arc, Mutex},
};

use serde::Serialize;

use super::{LocalConflict, LocalFile, LocalVersion, OpenError, SaveError, SaveStage};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Document(u64);

#[derive(Default)]
pub(crate) struct LocalEditors(Mutex<Registry>);

#[derive(Default)]
struct Registry {
    next_document: u64,
    documents: HashMap<String, Document>,
    files: HashMap<String, (Document, Arc<Mutex<LocalFile>>)>,
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
        }
    }
}

impl LocalEditors {
    pub(crate) fn navigate(&self, label: &str) {
        let mut registry = self.0.lock().unwrap();
        registry.revoke(label);
        registry.next_document += 1;
        let document = Document(registry.next_document);
        registry.documents.insert(label.into(), document);
    }

    pub(crate) fn destroy(&self, label: &str) {
        self.0.lock().unwrap().revoke(label);
    }

    pub(super) fn document(&self, label: &str) -> Result<Document, EditorError> {
        self.0.lock().unwrap().documents.get(label).copied().ok_or(EditorError::DocumentClosed)
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
        let handle = loop {
            let mut random = [0; 16];
            getrandom::fill(&mut random).map_err(|_| EditorError::Unavailable)?;
            let handle = format!("{:032x}", u128::from_ne_bytes(random));
            if let Entry::Vacant(entry) = registry.files.entry(handle.clone()) {
                entry.insert((document, Arc::new(Mutex::new(file))));
                break handle;
            }
        };
        Ok(Selected::Opened { handle, name, text })
    }

    pub(super) fn save(&self, label: &str, document: Document, handle: &str, text: &str) -> Result<(), EditorError> {
        self.with_file(label, document, handle, |file| file.save(text).map_err(Into::into))
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
        registry.files.remove(handle);
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
    fn check_document(&self, label: &str, document: Document) -> Result<(), EditorError> {
        if self.documents.get(label) != Some(&document) {
            return Err(EditorError::DocumentClosed);
        }
        Ok(())
    }

    fn file(&self, label: &str, document: Document, handle: &str) -> Result<&Arc<Mutex<LocalFile>>, EditorError> {
        self.check_document(label, document)?;
        match self.files.get(handle) {
            Some((owner, file)) if *owner == document => Ok(file),
            _ => Err(EditorError::InvalidHandle),
        }
    }

    fn revoke(&mut self, label: &str) {
        if let Some(document) = self.documents.remove(label) {
            self.files.retain(|_, (owner, _)| *owner != document);
        }
    }
}
