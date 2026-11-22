use serde::Deserialize;
use tauri::{AppHandle, Emitter, EventTarget, Manager, Url, Webview, Window};

use crate::{editor::registry::LocalEditors, terminal::Sessions};

pub(crate) const MAIN: &str = "main";

const EXPIRED: &str = "창 닫기 요청이 만료되었습니다";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum WindowAction {
    Drag,
    ToggleMaximize,
    Minimize,
    Close,
    RespondToClose,
}

pub(crate) fn issue_close_document(webview: &Webview, url: &Url) {
    if webview.label() != MAIN || webview.window_ref().label() != MAIN { return; }
    let bundled = if cfg!(any(windows, target_os = "android")) {
        url.host_str() == Some("tauri.localhost") && matches!(url.scheme(), "http" | "https") && url.port().is_none()
    } else {
        url.host_str() == Some("localhost") && url.scheme() == "tauri" && url.port().is_none()
    };
    let development = tauri::is_dev() && webview.config().build.dev_url.as_ref()
        .is_some_and(|app| app.origin() == url.origin());
    if (bundled || development) && webview.url().as_ref().ok() == Some(url) {
        webview.state::<LocalEditors>().ready_for_window_close(url);
    }
}

pub(crate) fn request_close(app: &AppHandle) {
    let Some(webview) = app.get_webview_window(MAIN) else { return; };
    if webview.as_ref().window_ref().label() != MAIN { return; }
    let Ok(url) = webview.url() else { return; };
    let editors = app.state::<LocalEditors>();
    let Ok(Some(id)) = editors.request_window_close(&url) else { return; };

    #[derive(Clone, serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Request<'a> { request_id: &'a str }

    if app.emit_to(EventTarget::webview(MAIN), "workspace-close-requested", Request { request_id: &id }).is_err() {
        editors.clear_window_close(&id, false);
    }
}

async fn close_approved(webview: Webview, window: Window, id: String) -> Result<(), String> {
    let editors = webview.state::<LocalEditors>();
    let sessions = webview.state::<Sessions>();
    let app = webview.app_handle().clone();
    let result: Result<(), String> = async {
        let confirmed = tauri::async_runtime::spawn_blocking(move || {
            app.state::<Sessions>().close_ssh_probes(Some(MAIN))
        }).await.map_err(|error| error.to_string())?;
        if !confirmed { return Err("SSH 시험 자식 종료 확인 불가".into()); }

        let (answer, completed) = tokio::sync::oneshot::channel();
        let caller = webview.clone();
        let request_id = id.clone();
        // 문서 전환과 최종 승인 확인을 같은 이벤트 루프에서 처리한다.
        webview.run_on_main_thread(move || {
            let result = (|| {
                let url = caller.url().map_err(|_| EXPIRED)?;
                caller.state::<LocalEditors>().with_approved_window_close(&url, &request_id, || {
                    // 그래픽 정리는 이 승인과 SSH 종료 확인 뒤, 창을 폐기하기 직전에 수행한다.
                    window.destroy().map_err(|error| error.to_string())
                }).ok_or(EXPIRED)?
            })();
            let _ = answer.send(result);
        }).map_err(|error| error.to_string())?;
        completed.await.map_err(|error| error.to_string())?
    }.await;
    if result.is_err() {
        sessions.release_close_gate(Some(MAIN));
        editors.clear_window_close(&id, true);
    }
    result
}

#[tauri::command]
pub(crate) async fn workspace_window(
    webview: Webview,
    action: WindowAction,
    request_id: Option<String>,
    approved: Option<bool>,
) -> Result<(), String> {
    let window = webview.window();
    if webview.label() != MAIN || window.label() != MAIN { return Err(EXPIRED.into()); }
    match action {
        WindowAction::Drag => window.start_dragging(),
        WindowAction::ToggleMaximize => (|| {
            if window.is_resizable()? {
                if window.is_maximized()? {
                    window.unmaximize()?;
                } else if window.is_maximizable()? {
                    window.maximize()?;
                }
            }
            Ok(())
        })(),
        WindowAction::Minimize => window.minimize(),
        WindowAction::Close => window.close(),
        WindowAction::RespondToClose => {
            let (Some(id), Some(approved)) = (request_id, approved) else { return Err(EXPIRED.into()); };
            let url = webview.url().map_err(|_| EXPIRED)?;
            webview.state::<LocalEditors>().respond_to_window_close(&url, &id, approved)?;
            return if approved { close_approved(webview, window, id).await } else { Ok(()) };
        }
    }
    .map_err(|error| error.to_string())
}
