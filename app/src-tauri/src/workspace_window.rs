use serde::Deserialize;
use tauri::Window;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum WindowAction {
    Drag,
    ToggleMaximize,
    Minimize,
    Close,
}

#[tauri::command]
pub(crate) async fn workspace_window(window: Window, action: WindowAction) -> Result<(), String> {
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
    }
    .map_err(|error| error.to_string())
}
