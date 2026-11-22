use std::{path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};
use tauri::Manager;
use tokio::sync::watch;
use uuid::Uuid;

use super::{Document, Registry, StorageIpc, confirm_native};
use crate::storage::{BucketRow, Error, Page};
use crate::storage::compatible::{
    AddressStyle, CompatibleS3, opaque_id,
    settings::{ConnectIntent, SavedSettingsView, Settings, SettingsError, SettingsInput, SettingsView, StagedSettings},
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct LoadRequest {
    service_id: String,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase", deny_unknown_fields)]
pub(super) enum OpenRequest {
    Saved { expected: SavedSettingsView },
    Revise { expected: SavedSettingsView, settings: SettingsInput },
    Select { settings: SettingsInput, item_id: String },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct OpenCommand {
    pub(super) session_id: String,
    pub(super) target: OpenRequest,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CompatibleSession {
    session_id: String,
    target: SettingsView,
    initial_buckets: Page<BucketRow>,
}

#[derive(Serialize)]
#[serde(untagged)]
pub(super) enum Reply<T, E = SettingsError> {
    Value { ok: bool, value: T },
    Error { ok: bool, error: E },
}

impl<T, E> From<Result<T, E>> for Reply<T, E> {
    fn from(result: Result<T, E>) -> Self {
        match result {
            Ok(value) => Self::Value { ok: true, value },
            Err(error) => Self::Error { ok: false, error },
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(super) enum OpenError {
    Settings(SettingsError),
    Connection { connection: Error },
}

impl From<SettingsError> for OpenError {
    fn from(error: SettingsError) -> Self {
        Self::Settings(error)
    }
}

fn settings_path(webview: &tauri::Webview<impl tauri::Runtime>, id: &str) -> Result<PathBuf, SettingsError> {
    if !opaque_id(id) {
        return Err(SettingsError::InvalidServiceId);
    }
    // ID를 경로 구성 요소로 쓰지 않으며 대소문자를 구분하지 않는 파일 시스템에서도 분리한다.
    use sha2::{Digest, Sha256};
    let filename = format!("{:x}.json", Sha256::digest(id.as_bytes()));
    Ok(webview.path().app_config_dir().map_err(|_| SettingsError::Io)?.join("s3-compatible").join(filename))
}

pub(super) async fn load<R: tauri::Runtime>(webview: tauri::Webview<R>, document: Document, request: LoadRequest) -> Result<Reply<SavedSettingsView>, tauri::ipc::InvokeError> {
    let result = async {
        let path = settings_path(&webview, &request.service_id)?;
        let view = tauri::async_runtime::spawn_blocking(move || {
            let view = Settings::load(&path)?.saved_view()?;
            if request.service_id != view.service_id { return Err(SettingsError::WrongService); }
            Ok(view)
        }).await.map_err(|_| SettingsError::Io)??;
        if !webview.state::<StorageIpc>().0.lock().unwrap().active(&document) {
            return Err(SettingsError::Cancelled);
        }
        Ok(view)
    }.await;
    Ok(result.into())
}

pub(super) struct Pending {
    pub(super) document: Document,
    pub(super) closed: Arc<watch::Sender<bool>>,
}

struct PreparedOpen {
    client: CompatibleS3,
    target: SettingsView,
    settings: Option<StagedSettings>,
    initial_buckets: Page<BucketRow>,
    approved_saved: ApprovedSaved,
}

struct ApprovedSaved {
    path: PathBuf,
    expected: Option<(SavedSettingsView, String)>,
}

impl ApprovedSaved {
    fn check(&self) -> Result<(), SettingsError> {
        match &self.expected {
            Some((expected, item)) => Settings::check_saved(&self.path, expected, item),
            None => match Settings::load(&self.path) {
                Err(SettingsError::NotFound) => Ok(()),
                Err(error) => Err(error),
                Ok(_) => Err(SettingsError::WrongService),
            },
        }
    }
}

async fn verify_open(client: &CompatibleS3, closed: &watch::Sender<bool>) -> Result<Page<BucketRow>, OpenError> {
    let mut cancelled = closed.subscribe();
    tokio::select! {
        biased;
        _ = cancelled.wait_for(|closed| *closed) => Err(SettingsError::Cancelled.into()),
        page = client.verify_connection() => page.map_err(|connection| OpenError::Connection { connection }),
    }
}

impl Registry {
    fn compatible_is_pending(&self, document: &Document, id: &str, closed: &Arc<watch::Sender<bool>>) -> bool {
        self.active(document) && self.pending_compatible.get(id).is_some_and(|pending| {
            pending.document == *document && Arc::ptr_eq(&pending.closed, closed)
        })
    }
}

impl StorageIpc {
    pub(super) fn reserve_compatible(&self, document: &Document, id: &str) -> Result<Arc<watch::Sender<bool>>, SettingsError> {
        let uuid = Uuid::parse_str(id).map_err(|_| SettingsError::Cancelled)?;
        if uuid.hyphenated().encode_lower(&mut Uuid::encode_buffer()).as_bytes() != id.as_bytes() {
            return Err(SettingsError::Cancelled);
        }
        let mut registry = self.0.lock().unwrap();
        if !registry.active(document) || registry.tabs.contains_key(id)
            || registry.openings.contains_key(&uuid)
            || registry.pending_compatible.contains_key(id)
            || registry.pending_compatible.values().any(|pending| pending.document == *document) {
            return Err(SettingsError::Cancelled);
        }
        let closed = Arc::new(watch::Sender::new(false));
        registry.pending_compatible.insert(id.into(), Pending { document: document.clone(), closed: closed.clone() });
        Ok(closed)
    }

    fn finish_compatible(
        &self, document: Document, id: String, closed: &Arc<watch::Sender<bool>>,
        prepared: Result<PreparedOpen, OpenError>,
    ) -> Result<CompatibleSession, OpenError> {
        // 파일 잠금 대기와 디스크 확인 중에도 탭 종료와 탐색을 처리할 수 있다.
        let file_lock = prepared.as_ref().ok().map(|open| {
            let lock = Settings::lock_file(&open.approved_saved.path)?;
            open.approved_saved.check()?;
            Ok::<_, SettingsError>(lock)
        }).transpose();
        let mut registry = self.0.lock().unwrap();
        if !registry.compatible_is_pending(&document, &id, closed) {
            return Err(SettingsError::Cancelled.into());
        }
        registry.pending_compatible.remove(&id);
        let PreparedOpen { client, target, settings, initial_buckets, .. } = prepared?;
        let _file_lock = match file_lock {
            Ok(lock) => lock,
            Err(error) => {
                drop(registry);
                return Err(error.into());
            }
        };

        // 최종 파일 교체와 탭 게시는 같은 예약에 속한다. 임시 파일 쓰기와 동기화는 락 밖에서 끝낸다.
        let published = match settings.map(StagedSettings::publish).transpose() {
            Ok(file) => file,
            Err(error) => {
                drop(registry);
                drop(error);
                return Err(SettingsError::Io.into());
            }
        };
        Self::insert_compatible_tab(&mut registry, id.clone(), document, client);
        drop(registry);
        drop(published);
        Ok(CompatibleSession { session_id: id, target, initial_buckets })
    }
}

pub(super) async fn open<R: tauri::Runtime>(webview: tauri::Webview<R>, document: Document, request: OpenRequest, session_id: String,
    reserved: Result<Arc<watch::Sender<bool>>, SettingsError>,
) -> Result<Reply<CompatibleSession, OpenError>, tauri::ipc::InvokeError> {
    let closed = match reserved { Ok(closed) => closed, Err(error) => return Ok(Err(error.into()).into()) };
    let app = webview.app_handle().clone();
    let approved = async {
        let id = match &request {
            OpenRequest::Saved { expected } | OpenRequest::Revise { expected, .. } => &expected.service_id,
            OpenRequest::Select { settings, .. } => &settings.service_id,
        };
        let path = settings_path(&webview, id)?;
        let (settings, save, approved_saved) = tauri::async_runtime::spawn_blocking({
            let path = path.clone();
            move || {
                let (expected, revision) = match request {
                    OpenRequest::Saved { expected } => (expected, None),
                    OpenRequest::Revise { expected, settings } => (expected, Some(settings)),
                    OpenRequest::Select { settings, item_id } => {
                        let settings = Settings::select(settings, item_id)?;
                        let expected = match Settings::load(&path) {
                            Ok(saved) => Some((saved.saved_view()?, saved.item_id().into())),
                            Err(SettingsError::NotFound) => None,
                            Err(error) => return Err(error),
                        };
                        return Ok((settings, true, ApprovedSaved { path, expected }));
                    }
                };
                let saved = Settings::load(&path)?;
                if saved.saved_view()? != expected { return Err(SettingsError::WrongService); }
                let approved = ApprovedSaved { path, expected: Some((expected, saved.item_id().into())) };
                let save = revision.is_some();
                let settings = match revision {
                    Some(input) => saved.revise(input)?,
                    None => saved,
                };
                Ok((settings, save, approved))
            }
        }).await.map_err(|_| SettingsError::Io)??;
        let view = settings.view();
        let style = match view.addressing {
            AddressStyle::Path => "경로",
            AddressStyle::VirtualHosted => "가상 호스트",
        };
        let prompt = format!("서비스: {} ({})\nHTTPS 출처: {}\n서명 리전: {}\n주소 방식: {}\n키링 항목: {}",
            view.alias, view.service_id, view.origin, view.region, style, settings.item_id());
        if !confirm_native(webview.window(), "S3 호환 키링 항목 사용", prompt, closed.subscribe()).await {
            return Err(SettingsError::Cancelled);
        }
        Ok((settings, save.then_some(path), view, approved_saved))
    }.await;
    let prepared = async {
        let (settings, path, target, approved_saved) = approved?;
        let (client, target, settings, path, approved_saved) = tauri::async_runtime::spawn_blocking({
            let document = document.clone();
            let id = session_id.clone();
            let closed = closed.clone();
            move || {
                let state = app.state::<StorageIpc>();
                let intent = ConnectIntent::Connect { service_id: target.service_id.clone(), endpoint: target.origin.clone() };
                let mut approval_error = None;
                let client = settings.connect(intent, |_, item| {
                    let registry = state.0.lock().unwrap();
                    if !registry.compatible_is_pending(&document, &id, &closed) {
                        return false;
                    }
                    approval_error = if item == settings.item_id() {
                        approved_saved.check().err()
                    } else {
                        Some(SettingsError::WrongService)
                    };
                    approval_error.is_none()
                });
                let client = match approval_error {
                    Some(error) => return Err(error),
                    None => client?,
                };
                {
                    let registry = state.0.lock().unwrap();
                    if !registry.compatible_is_pending(&document, &id, &closed) {
                        return Err(SettingsError::Cancelled);
                    }
                    approved_saved.check()?;
                }
                Ok((client, target, settings, path, approved_saved))
            }
        }).await.map_err(|_| SettingsError::Io)??;
        if !webview.state::<StorageIpc>().0.lock().unwrap().compatible_is_pending(&document, &session_id, &closed) {
            return Err(SettingsError::Cancelled.into());
        }
        let initial_buckets = verify_open(&client, &closed).await?;
        Ok((client, target, settings, path, initial_buckets, approved_saved))
    }.await;
    let result = tauri::async_runtime::spawn_blocking(move || {
        let prepared = prepared.and_then(|(client, target, settings, path, initial_buckets, approved_saved)| {
            let settings = path.map(|path| {
                std::fs::create_dir_all(path.parent().unwrap()).map_err(|_| SettingsError::Io)?;
                settings.stage_save(path)
            }).transpose()?;
            Ok(PreparedOpen { client, target, settings, initial_buckets, approved_saved })
        });
        webview.state::<StorageIpc>().finish_compatible(document, session_id, &closed, prepared)
    }).await.unwrap_or_else(|_| Err(SettingsError::Io.into()));
    Ok(result.into())
}

#[cfg(test)]
#[path = "ipc_tests.rs"]
mod tests;

#[path = "enroll.rs"]
pub(super) mod enroll;
