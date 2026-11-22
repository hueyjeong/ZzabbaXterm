use super::{Document, StorageIpc};
use crate::storage::{AwsS3, Failure, ObjectRow, Page, Stage, object_edit::{EditError, EditRevision}};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::watch;
use uuid::Uuid;

#[derive(Default)]
pub(super) struct Edits {
    rows: HashMap<String, Arc<ListedObject>>,
    editors: HashMap<String, Editor>,
}

struct ListedObject {
    scope: Arc<ListingScope>,
    key: String,
}

struct ListingScope {
    bucket: String,
    region: String,
}

struct Editor {
    target: Arc<ListedObject>,
    regional: Arc<AwsS3>,
    revision: Arc<EditRevision>,
    busy: bool,
    unknown: bool,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub(super) enum ListedRow {
    Prefix { prefix: String },
    Object { key: String, bytes: Option<u64>, row_id: String },
}

impl Edits {
    pub(super) fn clear_rows(&mut self) {
        self.rows.clear();
    }

    pub(super) fn listed(&mut self, page: Page<ObjectRow>, bucket: &str, region: &str) -> Page<ListedRow> {
        let scope = Arc::new(ListingScope { bucket: bucket.into(), region: region.into() });
        Page {
            next_token: page.next_token,
            rows: page.rows.into_iter().map(|row| match row {
                ObjectRow::Prefix { prefix } => ListedRow::Prefix { prefix },
                ObjectRow::Object { key, bytes } => {
                    let row_id = Uuid::new_v4().to_string();
                    self.rows.insert(row_id.clone(), Arc::new(ListedObject { scope: scope.clone(), key: key.clone() }));
                    ListedRow::Object { key, bytes, row_id }
                }
            }).collect(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct EditOpenRequest {
    session_id: String,
    row_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct EditRequest {
    session_id: String,
    handle: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct EditSaveRequest {
    session_id: String,
    handle: String,
    text: String,
}

#[derive(Serialize)]
pub(super) struct Opened {
    handle: String,
    label: String,
    text: String,
}

#[derive(Serialize)]
#[serde(untagged)]
pub(super) enum OpenResult {
    Opened(Opened),
    Error(EditResult),
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(super) enum EditResult {
    Read { text: String },
    Saved,
    Conflict { message: String },
    Unknown { message: &'static str },
    Denied { message: &'static str },
    Disconnected { message: &'static str },
    Failed { message: String },
}

impl EditResult {
    fn stale() -> Self {
        Self::Disconnected { message: "스토리지 세션 또는 편집기가 닫혔습니다" }
    }

    fn denied() -> Self {
        Self::Denied { message: "편집할 객체 또는 핸들을 확인할 수 없습니다" }
    }

    fn busy() -> Self {
        Self::Denied { message: "이 객체의 편집 작업이 진행 중입니다" }
    }

    fn unknown() -> Self {
        Self::Unknown { message: "저장 결과를 확인할 수 없습니다. 다시 읽기가 필요합니다" }
    }

    fn from_error(error: EditError, saving: bool) -> Self {
        match error {
            EditError::Storage { error } => match error.failure {
                Failure::SsoLoginRequired => expired(),
                Failure::Service { status: 401 | 403, .. } => Self::Denied { message: "S3 객체에 접근할 권한이 없습니다" },
                failure => Self::Failed { message: format!("S3 객체 편집 실패: {:?} · {failure:?}", error.stage) },
            },
            EditError::Unsupported { reason } => Self::Failed { message: reason.message().into() },
            EditError::Conflict { status } if saving => Self::Conflict { message: format!("S3 조건부 저장 충돌 (HTTP {status})") },
            EditError::SnapshotChanged if saving => Self::Conflict { message: "원격 객체 버전이 바뀌었습니다".into() },
            EditError::Conflict { .. } | EditError::SnapshotChanged => Self::Failed { message: "객체를 읽는 동안 원격 버전이 바뀌었습니다".into() },
            EditError::Unconfirmed if saving => Self::unknown(),
            EditError::Unconfirmed => Self::Failed { message: "읽기 결과를 확인할 수 없습니다".into() },
        }
    }
}

fn record_expiration(tab: &mut super::Tab, error: &EditError) {
    if let EditError::Storage { error } = error
        && error.failure == Failure::SsoLoginRequired
    {
        tab.expired = true;
        tab.selection = None;
        tab.edits.clear_rows();
    }
}

fn expired() -> EditResult {
    EditResult::Disconnected { message: "AWS SSO 재로그인이 필요합니다" }
}

// 탭을 닫거나 문서를 탐색하면 탭에 속한 편집 핸들도 폐기된다.
impl StorageIpc {
    pub(super) async fn edit_open(&self, document: &Document, request: EditOpenRequest) -> OpenResult {
        match self.open_listed_object(document, request).await {
            Ok(opened) => OpenResult::Opened(opened),
            Err(error) => OpenResult::Error(error),
        }
    }

    async fn open_listed_object(&self, document: &Document, request: EditOpenRequest) -> Result<Opened, EditResult> {
        let EditOpenRequest { session_id, row_id } = request;
        let (client, target, mut closed) = {
            let mut registry = self.0.lock().unwrap();
            let tab = registry.tab(document, &session_id).ok_or_else(EditResult::stale)?;
            let client = tab.client.aws().ok_or_else(EditResult::denied)?;
            if tab.expired { return Err(expired()); }
            let target = tab.edits.rows.get(&row_id).ok_or_else(EditResult::denied)?;
            (client.clone(), target.clone(), tab.closed.subscribe())
        };
        let regional = if target.scope.region == client.region {
            client.clone()
        } else {
            Arc::new(client.in_region(&target.scope.region, Stage::GetObject)
                .map_err(|error| EditResult::from_error(EditError::Storage { error }, false))?.into_owned())
        };
        let result = tokio::select! {
            biased;
            _ = closed.wait_for(|closed| *closed) => return Err(EditResult::stale()),
            result = regional.read_object_for_edit(&target.scope.bucket, &target.key) => result,
        };
        let mut registry = self.0.lock().unwrap();
        let tab = registry.tab(document, &session_id).ok_or_else(EditResult::stale)?;
        if !tab.edits.rows.contains_key(&row_id) || !tab.client.aws().is_some_and(|current| Arc::ptr_eq(current, &client)) {
            return Err(EditResult::denied());
        }
        let object = match result {
            Ok(object) => object,
            Err(error) => {
                record_expiration(tab, &error);
                return Err(EditResult::from_error(error, false));
            }
        };
        if tab.expired { return Err(expired()); }
        let handle = Uuid::new_v4().to_string();
        let label = target.key.clone();
        tab.edits.editors.insert(handle.clone(), Editor { target, regional, revision: Arc::new(object.revision), busy: false, unknown: false });
        Ok(Opened { handle, label, text: object.text })
    }

    pub(super) async fn edit_compare(&self, document: &Document, request: EditRequest) -> EditResult {
        self.edit_read(document, request, false).await
    }

    pub(super) async fn edit_reload(&self, document: &Document, request: EditRequest) -> EditResult {
        self.edit_read(document, request, true).await
    }

    async fn edit_read(&self, document: &Document, request: EditRequest, reload: bool) -> EditResult {
        let mut operation = match self.begin_edit(document, &request, if reload { Action::Reload } else { Action::Compare }) {
            Ok(operation) => operation,
            Err(error) => return error,
        };
        let result = tokio::select! {
            biased;
            _ = operation.closed.wait_for(|closed| *closed) => return EditResult::stale(),
            result = operation.regional.read_object_for_edit(&operation.target.scope.bucket, &operation.target.key) => result,
        };
        operation.finish(result, |editor, object| {
            if reload {
                editor.revision = Arc::new(object.revision);
                editor.unknown = false;
            }
            EditResult::Read { text: object.text }
        })
    }

    pub(super) async fn edit_save(&self, document: &Document, request: EditSaveRequest) -> EditResult {
        let EditSaveRequest { session_id, handle, text } = request;
        let request = EditRequest { session_id, handle };
        let mut operation = match self.begin_edit(document, &request, Action::Save) {
            Ok(operation) => operation,
            Err(error) => return error,
        };
        let result = tokio::select! {
            biased;
            _ = operation.closed.wait_for(|closed| *closed) => return EditResult::unknown(),
            result = operation.regional.save_edited_object(&operation.revision, text) => result,
        };
        operation.finish(result, |editor, revision| {
            editor.revision = Arc::new(revision);
            EditResult::Saved
        })
    }

    pub(super) fn edit_close(&self, document: &Document, request: EditRequest) -> bool {
        let mut registry = self.0.lock().unwrap();
        registry.tab(document, &request.session_id)
            .filter(|tab| tab.client.aws().is_some())
            .is_some_and(|tab| tab.edits.editors.remove(&request.handle).is_some())
    }

    fn begin_edit<'a>(&'a self, document: &'a Document, request: &'a EditRequest, action: Action) -> Result<Pending<'a>, EditResult> {
        let mut registry = self.0.lock().unwrap();
        let tab = registry.tab(document, &request.session_id).ok_or_else(EditResult::stale)?;
        let client = tab.client.aws().ok_or_else(EditResult::denied)?;
        if tab.expired { return Err(expired()); }
        let editor = tab.edits.editors.get_mut(&request.handle).ok_or_else(EditResult::denied)?;
        if editor.busy { return Err(EditResult::busy()); }
        if action == Action::Save && editor.unknown { return Err(EditResult::unknown()); }
        editor.busy = action != Action::Compare;
        Ok(Pending {
            storage: self, document, request, action,
            client: client.clone(), closed: tab.closed.subscribe(),
            regional: editor.regional.clone(),
            target: editor.target.clone(), revision: editor.revision.clone(), finished: false,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action { Compare, Reload, Save }

struct Pending<'a> {
    storage: &'a StorageIpc,
    document: &'a Document,
    request: &'a EditRequest,
    action: Action,
    client: Arc<AwsS3>,
    regional: Arc<AwsS3>,
    closed: watch::Receiver<bool>,
    target: Arc<ListedObject>,
    revision: Arc<EditRevision>,
    finished: bool,
}

impl Pending<'_> {
    fn finish<T>(mut self, result: Result<T, EditError>, apply: impl FnOnce(&mut Editor, T) -> EditResult) -> EditResult {
        let mut registry = self.storage.0.lock().unwrap();
        let saving = self.action == Action::Save;
        let Some(tab) = registry.tab(self.document, &self.request.session_id)
            .filter(|tab| tab.client.aws().is_some_and(|current| Arc::ptr_eq(current, &self.client))) else {
            return if saving { EditResult::unknown() } else { EditResult::stale() };
        };
        let Some(editor) = tab.edits.editors.get_mut(&self.request.handle) else {
            return if saving { EditResult::unknown() } else { EditResult::stale() };
        };
        if self.action == Action::Compare && (editor.busy || !Arc::ptr_eq(&editor.revision, &self.revision)) {
            return EditResult::busy();
        }
        self.finished = true;
        if self.action != Action::Compare { editor.busy = false; }
        if saving && matches!(&result, Err(EditError::Unconfirmed)) { editor.unknown = true; }
        if tab.expired && !saving && result.is_ok() {
            return expired();
        }
        match result {
            Ok(value) => apply(editor, value),
            Err(error) => {
                record_expiration(tab, &error);
                EditResult::from_error(error, saving)
            }
        }
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if self.finished || self.action == Action::Compare { return; }
        // 요청 취소만으로는 이미 전송된 PUT의 적용 여부를 알 수 없다.
        let mut registry = self.storage.0.lock().unwrap();
        if let Some(editor) = registry.tab(self.document, &self.request.session_id)
            .filter(|tab| tab.client.aws().is_some_and(|current| Arc::ptr_eq(current, &self.client)))
            .and_then(|tab| tab.edits.editors.get_mut(&self.request.handle)) {
            editor.busy = false;
            editor.unknown |= self.action == Action::Save;
        }
    }
}

#[cfg(test)]
#[path = "edit_ipc_tests.rs"]
mod tests;
