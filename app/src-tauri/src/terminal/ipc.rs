use super::{Sessions, shell, size, stage};
use tauri::{Manager, WebviewWindow, ipc::{Channel, CommandArg, CommandItem, Invoke, InvokeResponseBody}};

async fn blocking<T: Send + 'static>(job: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(job).await.map_err(|error| stage("worker", error))?
}

pub(crate) fn terminal_open(invoke: Invoke) {
    const COMMAND: &str = "terminal_open";
    let Invoke { message, resolver, acl } = invoke;
    // The generated async wrapper calls its command only after spawning the future.
    // Capture the generation in the synchronous, ACL-checked invoke handler instead.
    let caller = message.webview_ref();
    let document = caller.state::<Sessions>().document(caller.label());
    let argument = |key| CommandItem { name: COMMAND, key, plugin: None, message: &message, acl: &acl };
    let parse = || {
        Ok((
            WebviewWindow::from_command(argument("webview"))?,
            u16::from_command(argument("rows"))?,
            u16::from_command(argument("cols"))?,
            Channel::<InvokeResponseBody>::from_command(argument("output"))?,
        ))
    };
    let (webview, rows, cols, output) = match parse() {
        Ok(args) => args,
        Err(error) => { resolver.invoke_error(error); return; }
    };
    resolver.respond_async(async move {
        blocking(move || {
            webview.state::<Sessions>().open_document(webview.label(), document, size(rows, cols), shell(), move |event| {
                event.send(&output).map_err(|error| error.to_string())
            })
        }).await.map_err(Into::into)
    });
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
