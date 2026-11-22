use serde::Deserialize;
use tauri::Window;
use crate::terminal::Sessions;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum WindowAction {
    Drag,
    ToggleMaximize,
    Minimize,
    Close,
}

#[tauri::command]
pub(crate) async fn workspace_window(window: Window, sessions: tauri::State<'_, Sessions>, action: WindowAction) -> Result<(), String> {
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
        WindowAction::Close => {
            if !sessions.close_ssh_probes(Some(window.label())) {
                return Err("SSH 시험 자식 종료 확인 불가".into());
            }
            let result = window.close();
            if result.is_err() { sessions.release_close_gate(Some(window.label())); }
            result
        }
    }
    .map_err(|error| error.to_string())
}
