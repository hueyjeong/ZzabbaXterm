//! S3 탭의 로컬 창. 네이티브 폴더 대화상자로 승인한 루트 안에서만 목록과 전송 대상을 연다.
use std::{path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};
use tauri::Manager;
use tokio::sync::watch;
use uuid::Uuid;

use crate::editor::storage_files::{LocalEntry, LocalRoot, is_outside_root, valid_local_name};

use super::{ApprovedFile, Direction, Document, LocalFileApproval, StorageIpc, Target};

/// 한 번에 돌려주는 로컬 목록 항목의 상한이다.
pub(super) const LIST_LIMIT: usize = 5000;

pub(super) struct LocalApproval {
    id: String,
    root: Arc<LocalRoot>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct RootRequest {
    session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ListRequest {
    session_id: String,
    root_id: String,
    path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SelectRequest {
    #[serde(flatten)]
    target: Target,
    root_id: String,
    path: String,
    direction: Direction,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub(super) enum RootApproval {
    Approved { root_id: String, label: String },
    Cancelled,
    Denied,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Listing {
    entries: Vec<LocalEntry>,
    truncated: bool,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub(super) enum LocalFailure {
    Denied,
    InvalidPath,
    Link,
    NotFound,
    LocalIo { error_kind: String },
}

impl From<std::io::Error> for LocalFailure {
    fn from(error: std::io::Error) -> Self {
        use std::io::ErrorKind;
        if is_outside_root(&error) { return Self::Link; }
        match error.kind() {
            ErrorKind::InvalidInput => Self::InvalidPath,
            ErrorKind::NotFound => Self::NotFound,
            kind => Self::LocalIo { error_kind: format!("{kind:?}") },
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
pub(super) enum LocalResult<T> {
    Success { ok: bool, value: T },
    Failure { ok: bool, error: LocalFailure },
}

impl<T> From<Result<T, LocalFailure>> for LocalResult<T> {
    fn from(result: Result<T, LocalFailure>) -> Self {
        match result {
            Ok(value) => Self::Success { ok: true, value },
            Err(error) => Self::Failure { ok: false, error },
        }
    }
}

/// Windows의 `\\?\` 접두어를 뺀 표시용 경로다.
fn label(root: &LocalRoot) -> String {
    let path = root.path().to_string_lossy();
    path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned()
}

impl StorageIpc {
    /// 폴더 대화상자를 띄울 수 있으면 표시 제목과 탭 닫힘 신호를 돌려준다. 같은 탭에서 대화상자는 하나만 연다.
    pub(super) fn begin_local_root(&self, document: &Document, session_id: &str) -> Option<watch::Receiver<bool>> {
        let mut registry = self.0.lock().unwrap();
        let tab = registry.tab(document, session_id)?;
        if tab.local_picking { return None; }
        tab.local_picking = true;
        Some(tab.closed.subscribe())
    }

    pub(super) fn finish_local_root(
        &self, document: &Document, session_id: &str, closed: &watch::Receiver<bool>,
        selected: Option<std::io::Result<LocalRoot>>,
    ) -> RootApproval {
        let mut registry = self.0.lock().unwrap();
        let Some(tab) = registry.tab(document, session_id).filter(|tab| closed.same_channel(&tab.closed.subscribe())) else {
            return RootApproval::Denied;
        };
        tab.local_picking = false;
        match selected {
            Some(Ok(root)) => {
                let id = Uuid::new_v4().to_string();
                let label = label(&root);
                tab.local = Some(LocalApproval { id: id.clone(), root: Arc::new(root) });
                RootApproval::Approved { root_id: id, label }
            }
            Some(Err(_)) => RootApproval::Denied,
            None => RootApproval::Cancelled,
        }
    }

    pub(super) fn local_root(&self, document: &Document, session_id: &str, root_id: &str) -> Option<Arc<LocalRoot>> {
        let mut registry = self.0.lock().unwrap();
        let local = registry.tab(document, session_id)?.local.as_ref()?;
        (local.id == root_id).then(|| local.root.clone())
    }

    pub(super) async fn list_local(&self, document: &Document, request: ListRequest) -> Result<Listing, LocalFailure> {
        let ListRequest { session_id, root_id, path } = request;
        let root = self.local_root(document, &session_id, &root_id).ok_or(LocalFailure::Denied)?;
        let listed = root.clone();
        let (entries, truncated) = tokio::task::spawn_blocking(move || listed.list(&path, LIST_LIMIT))
            .await.map_err(|_| LocalFailure::Denied)??;
        // 목록을 읽는 동안 승인이 폐기되거나 바뀌었으면 결과를 버린다.
        if !self.local_root(document, &session_id, &root_id).is_some_and(|current| Arc::ptr_eq(&current, &root)) {
            return Err(LocalFailure::Denied);
        }
        Ok(Listing { entries, truncated })
    }
}

/// 업로드는 루트 기준 파일 경로를, 다운로드는 루트 기준 폴더와 객체 키의 기본 이름을 연다.
pub(super) fn open_local(root: &LocalRoot, path: &str, key: &str, direction: Direction) -> std::io::Result<ApprovedFile> {
    match direction {
        Direction::Upload => root.open_upload(path).map(ApprovedFile::Upload),
        Direction::Download => {
            let name = key.rsplit('/').next().filter(|name| valid_local_name(name))
                .ok_or(std::io::ErrorKind::InvalidInput)?;
            root.download_target(path, name).map(ApprovedFile::Download)
        }
    }
}

pub(super) async fn open_root<R: tauri::Runtime>(webview: tauri::Webview<R>, document: &Document, request: RootRequest) -> RootApproval {
    let state = webview.state::<StorageIpc>();
    let Some(closed) = state.begin_local_root(document, &request.session_id) else { return RootApproval::Denied };
    let picked = super::pick_local_folder(webview.window(), "S3 로컬 폴더 선택".into(), closed.clone()).await;
    let selected = match picked {
        Some(path) => Some(tokio::task::spawn_blocking(move || LocalRoot::open(&path))
            .await.unwrap_or_else(|error| Err(std::io::Error::other(error)))),
        None => None,
    };
    state.finish_local_root(document, &request.session_id, &closed, selected)
}

pub(super) async fn select<R: tauri::Runtime>(webview: tauri::Webview<R>, document: &Document, request: SelectRequest) -> LocalFileApproval {
    let state = webview.state::<StorageIpc>();
    let SelectRequest { target, root_id, path, direction } = request;
    let session_id = target.session_id.clone();
    let key = target.key.clone();
    let Some(root) = state.local_root(document, &session_id, &root_id) else { return LocalFileApproval::Denied };
    let Some(id) = state.begin_selection(document, target, direction) else { return LocalFileApproval::Denied };
    let Some((title, closed)) = state.selection_dialog(document, &session_id, &id) else { return LocalFileApproval::Denied };
    let display: PathBuf = std::iter::once(label(&root))
        .chain(path.split('/').filter(|name| !name.is_empty()).map(str::to_owned))
        .chain(key.rsplit('/').next().map(str::to_owned))
        .collect();
    let opened = tokio::task::spawn_blocking(move || open_local(&root, &path, &key, direction))
        .await.unwrap_or_else(|error| Err(std::io::Error::other(error)));
    let selected = match opened {
        Ok(ApprovedFile::Download(target)) if target.replaces_existing() => {
            let prompt = format!("{title}\n\n기존 파일을 교체하시겠습니까?\n{display:?}");
            super::confirm_replace(webview.window(), prompt, closed).await.then_some(Ok(ApprovedFile::Download(target)))
        }
        other => Some(other),
    };
    state.finish_selection(document, &session_id, id, selected)
}

#[cfg(test)]
#[path = "local_pane_tests.rs"]
mod tests;
