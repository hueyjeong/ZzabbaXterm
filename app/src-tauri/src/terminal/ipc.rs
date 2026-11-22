use super::{Sessions, shell, size, stage};
use tauri::{Manager, WebviewWindow, ipc::{Channel, InvokeResponseBody}};

async fn blocking<T: Send + 'static>(job: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(job).await.map_err(|error| stage("worker", error))?
}

#[tauri::command]
pub(crate) async fn terminal_open(webview: WebviewWindow, rows: u16, cols: u16, output: Channel<InvokeResponseBody>) -> Result<String, String> {
    let document = webview.state::<Sessions>().document(webview.label());
    blocking(move || {
        webview.state::<Sessions>().open_document(webview.label(), document, size(rows, cols), shell(), move |event| {
            event.send(&output).map_err(|error| error.to_string())
        })
    }).await
}

#[tauri::command]
pub(crate) fn terminal_start(webview: WebviewWindow, id: String) -> Result<(), String> {
    webview.state::<Sessions>().start(webview.label(), &id)
}

#[tauri::command]
pub(crate) async fn terminal_input(webview: WebviewWindow, id: String, bytes: Vec<u8>) -> Result<(), String> {
    blocking(move || webview.state::<Sessions>().input(webview.label(), &id, &bytes)).await
}

#[tauri::command]
pub(crate) async fn terminal_resize(webview: WebviewWindow, id: String, rows: u16, cols: u16) -> Result<(), String> {
    blocking(move || webview.state::<Sessions>().resize(webview.label(), &id, size(rows, cols))).await
}

#[tauri::command]
pub(crate) fn terminal_ack(webview: WebviewWindow, id: String, position: u64) -> Result<(), String> {
    webview.state::<Sessions>().ack(webview.label(), &id, position)
}

#[tauri::command]
pub(crate) async fn terminal_close(webview: WebviewWindow, id: String) -> Result<(), String> {
    blocking(move || webview.state::<Sessions>().close(webview.label(), &id)).await
}
