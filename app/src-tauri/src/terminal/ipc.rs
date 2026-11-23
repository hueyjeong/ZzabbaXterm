use std::time::Instant;

use super::{Launch, Sessions, saved, shell, size, stage, validate_size};
use crate::ai::{execution, session_selection};
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
            Option::<String>::from_command(argument("targetId"))?,
            Option::<bool>::from_command(argument("remoteEditor"))?,
        ))
    };
    let (webview, rows, cols, output, target_id, remote_editor) = match parse() {
        Ok(args) => args,
        Err(error) => { resolver.invoke_error(error); return; }
    };
    if target_id.is_some() && (caller.label() != "main" || caller.window_ref().label() != "main" || acl.is_none()) {
        resolver.reject("앱의 로컬 기본 창에서만 저장된 연결을 열 수 있습니다");
        return;
    }
    let remote_editor = remote_editor == Some(true);
    if remote_editor && target_id.is_none() {
        resolver.reject("nano 연동에는 저장된 SSH 연결이 필요합니다");
        return;
    }
    let window = target_id.is_some().then(|| caller.window());
    resolver.respond_async(async move {
        blocking(move || {
            let sessions = webview.state::<Sessions>();
            let owner = webview.label();
            let size = size(rows, cols);
            let send = move |event: super::Event| event.send(&output).map_err(|error| error.to_string());
            let Some(id) = target_id else {
                return sessions.open_document(owner, document, size, Launch::Local(shell()), send);
            };
            let directory = webview.path().app_config_dir().map_err(|error| stage("앱 설정 디렉터리", error))?;
            let Some(window) = window else {
                return sessions.open_document(owner, document, size, saved::launch(&directory, &id)?, send);
            };
            if !remote_editor {
                let launch = saved::launch_interactive(&directory, &id, owner, document, |message| {
                    crate::ssh::registration::confirm_stored_auth(&window, message)
                })?;
                return sessions.open_document(owner, document, size, launch, send);
            }
            validate_size(size)?;
            let (registration, command, parent) = saved::approve_remote_editor(&directory, &id, |message| {
                sessions.with_probe_access(owner, document, true, |_| Ok(()))?;
                crate::ssh::registration::confirm_remote_editor(&window, message)
            }, |mut launch| {
                let parent = launch.enable_editor(webview.app_handle().clone(), owner.into(), document);
                let command = Launch::Ssh(launch);
                let registration = sessions.register_document(owner, document, &command, true)?;
                Ok((registration, command, parent))
            })?;
            let id = sessions.launch_document(registration, size, command, send)?;
            let _ = parent.send(id.clone());
            Ok(id)
        }).await.map_err(Into::into)
    });
}

#[tauri::command]
pub(crate) async fn terminal_start(webview: WebviewWindow, id: String) -> Result<(), String> {
    blocking(move || webview.state::<Sessions>().start(webview.label(), &id)).await
}

#[tauri::command]
pub(crate) async fn terminal_input(webview: WebviewWindow, id: String, bytes: Vec<u8>) -> Result<(), String> {
    blocking(move || {
        let sessions = webview.state::<Sessions>();
        if webview.label() == "main" && webview.as_ref().window().label() == "main" {
            let state = webview.state::<execution::State>().inner().clone();
            let now = Instant::now();
            if let Some(key) = state.observe_prefix(&id, &bytes, now) {
                let document = sessions.document("main");
                let confirmed = webview.state::<session_selection::State>().confirmed(&sessions, document);
                let selection = confirmed.as_ref().map(|(generation, identity)| (*generation, identity));
                state.record_key(document, &id, key, selection, now);
            }
        }
        sessions.input(webview.label(), &id, &bytes)
    }).await
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
    let document = webview.state::<Sessions>().document(webview.label());
    blocking(move || {
        let result = webview.state::<Sessions>().close(webview.label(), &id)?;
        webview.state::<crate::editor::sftp_ipc::SftpIpc>().close_parent(webview.label(), document, &id);
        result.warning().map_or(Ok(()), Err)
    }).await
}
