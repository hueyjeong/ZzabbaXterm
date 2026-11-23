//! S3 탭의 로컬 창. 네이티브 폴더 대화상자로 승인한 루트 안에서만 목록과 전송 대상을 연다.
use std::{path::{Path, PathBuf}, sync::Arc};

use serde::{Deserialize, Serialize};
use tauri::Manager;
use tokio::sync::watch;
use uuid::Uuid;

use crate::editor::storage_files::{LocalEntry, LocalRoot, is_outside_root, is_root_changed, valid_local_name};
use crate::storage::{Error, Stage};

use super::{
    ApprovedFile, Direction, Document, LocalFileApproval, StorageIpc, TabClient, Target, bucket_verified, unavailable, until_closed,
};

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
    /// 시스템·자격 증명 폴더나 그 상위·하위, 드라이브 루트, 사용자 홈 자체다.
    Restricted,
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
    RootChanged,
    LocalIo { error_kind: String },
}

impl From<std::io::Error> for LocalFailure {
    fn from(error: std::io::Error) -> Self {
        use std::io::ErrorKind;
        if is_root_changed(&error) { return Self::RootChanged; }
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

/// 로컬 창 전송 승인 결과다. 거부 원인과 S3 확인 오류를 함께 돌려준다.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub(super) enum SelectResult {
    Approved { approval_id: String },
    Cancelled,
    Denied,
    Rejected { reason: LocalFailure },
    Failed { error: Error },
}

impl From<LocalFileApproval> for SelectResult {
    fn from(approval: LocalFileApproval) -> Self {
        match approval {
            LocalFileApproval::Approved { approval_id } => Self::Approved { approval_id },
            LocalFileApproval::Cancelled => Self::Cancelled,
            LocalFileApproval::Denied => Self::Denied,
        }
    }
}

/// Windows의 `\\?\` 접두어를 뺀 표시용 경로다.
fn label(root: &LocalRoot) -> String {
    let path = root.path().to_string_lossy();
    path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned()
}

fn normalized(path: &Path) -> PathBuf {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let text = path.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    PathBuf::from(if cfg!(windows) { text.to_lowercase() } else { text.to_owned() })
}

/// 드라이브 루트, 홈 자체와 그 상위, 홈 바로 아래 숨김 폴더와 그 하위, 보호 폴더와 그 상위·하위면 참이다.
/// 홈의 다른 하위 폴더는 허용한다.
pub(super) fn restricted(root: &Path, home: Option<&Path>, protected: &[PathBuf]) -> bool {
    let root = normalized(root);
    if root.parent().is_none() { return true; }
    if let Some(home) = home.filter(|home| home.is_absolute()).map(normalized) {
        if home.starts_with(&root) { return true; }
        // 홈 바로 아래 숨김 폴더(`.ssh`, `.kube`, `.docker` 등)와 그 하위는 자격 증명·설정 위치라 막는다.
        if root.strip_prefix(&home).ok().and_then(|rest| rest.components().next())
            .is_some_and(|first| first.as_os_str().to_string_lossy().starts_with('.')) { return true; }
    }
    protected.iter().filter(|path| path.is_absolute()).map(|path| normalized(path))
        .any(|path| path.starts_with(&root) || root.starts_with(&path))
}

pub(super) fn restricted_root(root: &Path) -> bool {
    let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
    #[cfg(windows)]
    let (home, mut protected) = (var("USERPROFILE"), [
        "APPDATA", "LOCALAPPDATA", "PROGRAMDATA", "windir", "SystemRoot", "ProgramFiles", "ProgramFiles(x86)", "ProgramW6432",
        "CommonProgramFiles", "CommonProgramFiles(x86)", "CommonProgramW6432",
    ].into_iter().filter_map(var).collect::<Vec<_>>());
    #[cfg(not(windows))]
    let (home, mut protected) = (var("HOME"), [
        "/etc", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/boot", "/dev", "/proc", "/sys", "/System", "/Library",
    ].map(PathBuf::from).to_vec());
    if let Some(home) = &home {
        protected.extend([".ssh", ".config", ".local", ".aws", ".gnupg"].map(|name| home.join(name)));
        #[cfg(target_os = "macos")]
        protected.push(home.join("Library"));
    }
    restricted(root, home.as_deref(), &protected)
}

/// 실행되거나 실행을 일으키는 확장자다. 새 파일로 내려받기 전에 확인한다.
pub(super) fn executable_name(name: &str) -> bool {
    const EXTENSIONS: &[&str] = &[
        "exe", "com", "cmd", "bat", "lnk", "pif", "scr", "cpl", "msc", "msi", "msp", "msix", "appx", "appinstaller", "application",
        "ps1", "psm1", "psd1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "ws", "hta", "reg", "url", "scf", "inf", "jar", "dll",
        "sh", "bash", "zsh", "csh", "command", "desktop", "appimage", "run", "py", "pyw", "chm", "xll", "docm", "xlsm", "pptm",
        "wsc", "sct", "settingcontent-ms", "library-ms", "iso", "img", "vhd", "vhdx", "ps1xml",
    ];
    name.rsplit_once('.').is_some_and(|(_, extension)| EXTENSIONS.iter().any(|known| known.eq_ignore_ascii_case(extension.trim_end())))
}

impl StorageIpc {
    /// 폴더 대화상자를 띄울 수 있으면 탭 닫힘 신호를 돌려준다. 같은 탭에서 대화상자는 하나만 열고 전송 승인 중에는 열지 않는다.
    pub(super) fn begin_local_root(&self, document: &Document, session_id: &str) -> Option<watch::Receiver<bool>> {
        let mut registry = self.0.lock().unwrap();
        let tab = registry.tab(document, session_id)?;
        if tab.local_picking || tab.selection.is_some() { return None; }
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

    /// 루트가 옮겨지거나 이름이 바뀐 승인을 폐기한다.
    fn forget_local_root(&self, document: &Document, session_id: &str, root: &Arc<LocalRoot>) {
        let mut registry = self.0.lock().unwrap();
        if let Some(tab) = registry.tab(document, session_id)
            && tab.local.as_ref().is_some_and(|local| Arc::ptr_eq(&local.root, root))
        {
            tab.local = None;
        }
    }

    pub(super) async fn list_local(&self, document: &Document, request: ListRequest) -> Result<Listing, LocalFailure> {
        let ListRequest { session_id, root_id, path } = request;
        let root = self.local_root(document, &session_id, &root_id).ok_or(LocalFailure::Denied)?;
        let listed = root.clone();
        let result = tokio::task::spawn_blocking(move || listed.list(&path, LIST_LIMIT))
            .await.map_err(|_| LocalFailure::Denied)?;
        // 목록을 읽는 동안 승인이 폐기되거나 바뀌었으면 결과를 버린다.
        if !self.local_root(document, &session_id, &root_id).is_some_and(|current| Arc::ptr_eq(&current, &root)) {
            return Err(LocalFailure::Denied);
        }
        let (entries, truncated) = result.map_err(|error| {
            let failure = LocalFailure::from(error);
            if failure == LocalFailure::RootChanged { self.forget_local_root(document, &session_id, &root); }
            failure
        })?;
        Ok(Listing { entries, truncated })
    }

    /// 업로드 전에 같은 키의 객체가 있는지 확인한다. 확인할 수 없으면 오류를 그대로 돌려준다.
    pub(super) async fn object_exists(&self, document: &Document, target: &Target) -> Result<bool, Error> {
        let stage = Stage::HeadObject;
        let (client, closed) = {
            let mut registry = self.0.lock().unwrap();
            let tab = registry.tab(document, &target.session_id).ok_or_else(|| unavailable(stage))?;
            if !bucket_verified(tab, target) { return Err(unavailable(stage)); }
            (tab.client.clone(), tab.closed.subscribe())
        };
        let result = until_closed(&closed, stage, async {
            match &client {
                TabClient::AwsSso(client) => client.in_region(&target.region, stage)?.object_exists(&target.bucket, &target.key).await,
                TabClient::Compatible(client) => client.object_exists(&target.bucket, &target.key).await.map_err(|error| error.operation),
            }
        }).await;
        self.record_expiration(document, &target.session_id, &closed, &result);
        result
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
        Some(path) => Some(tokio::task::spawn_blocking(move || {
            LocalRoot::open(&path).map(|root| (restricted_root(root.path()), root))
        }).await.unwrap_or_else(|error| Err(std::io::Error::other(error)))),
        None => None,
    };
    match selected {
        Some(Ok((true, _))) => match state.finish_local_root(document, &request.session_id, &closed, None) {
            RootApproval::Cancelled => RootApproval::Restricted,
            other => other,
        },
        selected => state.finish_local_root(document, &request.session_id, &closed, selected.map(|result| result.map(|(_, root)| root))),
    }
}

pub(super) async fn select<R: tauri::Runtime>(webview: tauri::Webview<R>, document: &Document, request: SelectRequest) -> SelectResult {
    let window = webview.window();
    webview.state::<StorageIpc>().select_local(document, request, |dialog, prompt, closed| super::confirm_native(window, dialog, prompt, closed)).await
}

impl StorageIpc {
    /// 로컬 창 전송을 승인한다. 확인이 필요하면 confirm(제목, 내용, 탭 닫힘 신호)이 참일 때만 승인한다.
    pub(super) async fn select_local<F: Future<Output = bool>>(
        &self, document: &Document, request: SelectRequest, confirm: impl FnOnce(&'static str, String, watch::Receiver<bool>) -> F,
    ) -> SelectResult {
        let SelectRequest { target, root_id, path, direction } = request;
        let session_id = target.session_id.clone();
        let checked = target.clone();
        let Some(root) = self.local_root(document, &session_id, &root_id) else { return SelectResult::Denied };
        let Some(id) = self.begin_selection(document, target, direction) else { return SelectResult::Denied };
        let Some((title, closed)) = self.selection_dialog(document, &session_id, &id) else { return SelectResult::Denied };
        let (opened_root, opened_path, key) = (root.clone(), path.clone(), checked.key.clone());
        let opened = tokio::task::spawn_blocking(move || open_local(&opened_root, &opened_path, &key, direction))
            .await.unwrap_or_else(|error| Err(std::io::Error::other(error)));
        let file = match opened {
            Ok(file) => file,
            Err(error) => {
                let reason = LocalFailure::from(error);
                if reason == LocalFailure::RootChanged { self.forget_local_root(document, &session_id, &root); }
                self.finish_selection(document, &session_id, id, None);
                return SelectResult::Rejected { reason };
            }
        };
        let confirm_with = match &file {
            ApprovedFile::Upload(_) => match self.object_exists(document, &checked).await {
                Ok(false) => None,
                Ok(true) => Some(("S3 객체 교체", format!("{title}\n\n같은 키의 객체를 교체하시겠습니까?"))),
                Err(error) => {
                    self.finish_selection(document, &session_id, id, None);
                    return SelectResult::Failed { error };
                }
            },
            ApprovedFile::Download(target) => {
                let name = checked.key.rsplit('/').next().unwrap_or_default();
                let display: PathBuf = std::iter::once(label(&root))
                    .chain(path.split('/').filter(|name| !name.is_empty()).map(str::to_owned))
                    .chain([name.to_owned()])
                    .collect();
                if target.replaces_existing() {
                    Some(("다운로드 파일 교체", format!("{title}\n\n기존 파일을 교체하시겠습니까?\n{display:?}")))
                } else if executable_name(name) {
                    Some(("다운로드 실행 파일", format!("{title}\n\n실행 파일 형식으로 저장하시겠습니까?\n{display:?}")))
                } else {
                    None
                }
            }
        };
        if let Some((dialog, prompt)) = confirm_with && !confirm(dialog, prompt, closed).await {
            return self.finish_selection(document, &session_id, id, None).into();
        }
        self.finish_selection(document, &session_id, id, Some(Ok(file))).into()
    }
}

#[cfg(test)]
#[path = "local_pane_tests.rs"]
mod tests;
