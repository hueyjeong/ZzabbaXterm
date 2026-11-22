use serde::{Deserialize, Serialize};

use super::{Document, Reply, SettingsError, SettingsInput};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::storage) struct EnrollRequest {
    pub(in crate::storage) session_id: String,
    settings: SettingsInput,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::storage) struct Enrolled {
    item_id: String,
}

#[derive(Serialize)]
#[serde(untagged)]
pub(in crate::storage) enum EnrollError {
    Settings(SettingsError),
    OutcomeUnknown {
        #[serde(rename = "outcomeUnknown")]
        outcome_unknown: Enrolled,
    },
}

impl From<SettingsError> for EnrollError {
    fn from(error: SettingsError) -> Self {
        Self::Settings(error)
    }
}

pub(in crate::storage) async fn enroll<R: tauri::Runtime>(
    webview: tauri::Webview<R>,
    document: Document,
    request: EnrollRequest,
    reserved: Result<std::sync::Arc<tokio::sync::watch::Sender<bool>>, SettingsError>,
) -> Result<Reply<Enrolled, EnrollError>, tauri::ipc::InvokeError> {
    use super::StorageIpc;
    use tauri::Manager;

    let closed = match reserved {
        Ok(closed) => closed,
        Err(error) => return Ok(Err(error.into()).into()),
    };
    let state = webview.state::<StorageIpc>();
    let result = enroll_reserved(&webview, &document, &request.session_id, request.settings, &closed).await;
    let mut registry = state.0.lock().unwrap();
    if registry.compatible_is_pending(&document, &request.session_id, &closed) {
        registry.pending_compatible.remove(&request.session_id);
    }
    Ok(result.into())
}

#[cfg(any(windows, target_os = "linux"))]
async fn enroll_reserved<R: tauri::Runtime>(
    webview: &tauri::Webview<R>,
    document: &Document,
    session_id: &str,
    input: SettingsInput,
    closed: &std::sync::Arc<tokio::sync::watch::Sender<bool>>,
) -> Result<Enrolled, EnrollError> {
    use super::{Settings, StorageIpc};
    use crate::storage::compatible::{KeyAccessError, keyring};
    use tauri::Manager;

    let item_id = uuid::Uuid::new_v4().simple().to_string();
    let settings = Settings::select(input, item_id.clone())?;
    let target = keyring::target_name(settings.scope(), &item_id).map_err(SettingsError::from)?;
    let view = settings.view();
    let description = format!("서비스: {} ({})\nHTTPS 출처: {}", view.alias, view.service_id, view.origin);
    let mut payload = keyring::prompt(webview.window(), description, closed.subscribe()).await.map_err(SettingsError::from)?;
    let app = webview.app_handle().clone();
    let document = document.clone();
    let session_id = session_id.to_owned();
    let closed = closed.clone();
    let label = format!("ZzabbaXterm S3 · {item_id}");
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<StorageIpc>();
        if !state.0.lock().unwrap().compatible_is_pending(&document, &session_id, &closed) {
            return Err(KeyAccessError::Cancelled);
        }
        // 금고에 넘긴 작업은 닫기와 겹쳐도 취소로 바꾸거나 재전송하지 않는다.
        keyring::write_new(&target, &label, &mut payload)
    }).await.unwrap_or(Err(KeyAccessError::OutcomeUnknown));
    let enrolled = Enrolled { item_id };
    match outcome {
        Ok(()) => Ok(enrolled),
        Err(KeyAccessError::OutcomeUnknown) => Err(EnrollError::OutcomeUnknown { outcome_unknown: enrolled }),
        Err(error) => Err(SettingsError::from(error).into()),
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
async fn enroll_reserved<R: tauri::Runtime>(
    _: &tauri::Webview<R>,
    _: &Document,
    _: &str,
    _: SettingsInput,
    _: &std::sync::Arc<tokio::sync::watch::Sender<bool>>,
) -> Result<Enrolled, EnrollError> {
    Err(SettingsError::KeyUnavailable.into())
}
