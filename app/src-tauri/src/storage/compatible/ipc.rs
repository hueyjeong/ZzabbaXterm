use std::{path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};
use tauri::Manager;
use tokio::sync::watch;
use uuid::Uuid;

use super::{Document, StorageIpc, confirm_native};
use crate::storage::compatible::{
    AddressStyle, opaque_id,
    settings::{ConnectIntent, Settings, SettingsError, SettingsInput, SettingsView},
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct LoadRequest {
    service_id: String,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase", deny_unknown_fields)]
pub(super) enum OpenRequest {
    Saved { expected: SettingsView },
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
}

#[derive(Serialize)]
#[serde(untagged)]
pub(super) enum Reply<T> {
    Value { ok: bool, value: T },
    Error { ok: bool, error: SettingsError },
}

impl<T> From<Result<T, SettingsError>> for Reply<T> {
    fn from(result: Result<T, SettingsError>) -> Self {
        match result {
            Ok(value) => Self::Value { ok: true, value },
            Err(error) => Self::Error { ok: false, error },
        }
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

pub(super) async fn load<R: tauri::Runtime>(webview: tauri::Webview<R>, document: Document, request: LoadRequest) -> Result<Reply<SettingsView>, tauri::ipc::InvokeError> {
    let result = async {
        let path = settings_path(&webview, &request.service_id)?;
        let view = tauri::async_runtime::spawn_blocking(move || {
            let view = Settings::load(&path)?.view();
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

    fn compatible_is_pending(&self, id: &str, closed: &Arc<watch::Sender<bool>>) -> bool {
        self.0.lock().unwrap().pending_compatible.get(id).is_some_and(|pending| Arc::ptr_eq(&pending.closed, closed))
    }
}

pub(super) async fn open<R: tauri::Runtime>(webview: tauri::Webview<R>, document: Document, request: OpenRequest, session_id: String,
    reserved: Result<Arc<watch::Sender<bool>>, SettingsError>,
) -> Result<Reply<CompatibleSession>, tauri::ipc::InvokeError> {
    let closed = match reserved { Ok(closed) => closed, Err(error) => return Ok(Err(error).into()) };
    let state = webview.state::<StorageIpc>();
    let result = async {
        let id = match &request {
            OpenRequest::Saved { expected } => &expected.service_id,
            OpenRequest::Select { settings, .. } => &settings.service_id,
        };
        let path = settings_path(&webview, id)?;
        let (settings, save) = tauri::async_runtime::spawn_blocking({
            let path = path.clone();
            move || match request {
                OpenRequest::Saved { expected } => {
                    let settings = Settings::load(&path)?;
                    if settings.view() != expected { return Err(SettingsError::WrongService); }
                    Ok((settings, false))
                }
                OpenRequest::Select { settings, item_id } => Ok((Settings::select(settings, item_id)?, true)),
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
        let app = webview.app_handle().clone();
        let approved_id = session_id.clone();
        let approved_signal = closed.clone();
        let intent = ConnectIntent::Connect { service_id: view.service_id.clone(), endpoint: view.origin.clone() };
        let client = tauri::async_runtime::spawn_blocking(move || {
            let state = app.state::<StorageIpc>();
            let client = settings.connect(intent, |_, _| state.compatible_is_pending(&approved_id, &approved_signal))?;
            if !state.compatible_is_pending(&approved_id, &approved_signal) { return Err(SettingsError::Cancelled); }
            if save {
                std::fs::create_dir_all(path.parent().unwrap()).map_err(|_| SettingsError::Io)?;
                settings.save(&path)?;
            }
            Ok(client)
        }).await.map_err(|_| SettingsError::Io)??;
        Ok((client, view))
    }.await;
    let mut registry = state.0.lock().unwrap();
    if !registry.pending_compatible.get(&session_id).is_some_and(|pending| Arc::ptr_eq(&pending.closed, &closed)) {
        return Ok(Err(SettingsError::Cancelled).into());
    }
    registry.pending_compatible.remove(&session_id);
    Ok(result.map(|(client, target)| {
        StorageIpc::insert_compatible_tab(&mut registry, session_id.clone(), document, client);
        CompatibleSession { session_id, target }
    }).into())
}

#[cfg(test)]
#[path = "ipc_tests.rs"]
mod tests;
