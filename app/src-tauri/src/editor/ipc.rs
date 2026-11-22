use tauri::{
    Manager,
    ipc::{CommandArg, CommandItem, Invoke, InvokeBody, InvokeError, InvokeResolver},
};

use super::registry::{EditorError, LocalEditors};

fn blocking<T: serde::Serialize + Send + 'static>(
    resolver: InvokeResolver<tauri::Wry>,
    job: impl FnOnce() -> Result<T, EditorError> + Send + 'static,
) {
    resolver.respond_async(async move {
        tauri::async_runtime::spawn_blocking(job)
            .await
            .map_err(|_| EditorError::Unavailable)?
            .map_err(Into::into)
    });
}

pub(crate) fn dispatch(invoke: Invoke) -> Result<(), Invoke> {
    const OPEN: &str = "editor_local_open";
    const SAVE: &str = "editor_local_save";
    const COMPARE: &str = "editor_local_compare";
    const RELOAD: &str = "editor_local_reload";
    const CLOSE: &str = "editor_local_close";
    const PENDING: &str = "editor_local_cli_pending";
    const CLAIM: &str = "editor_local_cli_claim";
    const RESPOND: &str = "editor_local_cli_respond";

    let command = match invoke.message.command() {
        OPEN => OPEN,
        SAVE => SAVE,
        COMPARE => COMPARE,
        RELOAD => RELOAD,
        CLOSE => CLOSE,
        PENDING => PENDING,
        CLAIM => CLAIM,
        RESPOND => RESPOND,
        _ => return Err(invoke),
    };
    let Invoke {
        message,
        resolver,
        acl,
    } = invoke;
    let webview = message.webview_ref();
    if matches!(command, PENDING | CLAIM | RESPOND)
        && (webview.label() != super::cli_open::MAIN || webview.window().label() != super::cli_open::MAIN)
    {
        resolver.reject(EditorError::DocumentClosed);
        return Ok(());
    }
    let editors = webview.state::<LocalEditors>();
    // 비동기 작업을 시작하기 전에 Tauri가 전달한 호출 문서를 묶는다.
    let document = match editors.document(webview.label()) {
        Ok(document) => document,
        Err(error) => {
            resolver.reject(error);
            return Ok(());
        }
    };

    if command == PENDING {
        resolver.respond(editors.pending_offers(webview.label(), document).map_err(InvokeError::from));
        return Ok(());
    }

    if command == OPEN {
        let InvokeBody::Json(body) = message.payload() else {
            resolver.reject(EditorError::InvalidSelection);
            return Ok(());
        };
        if !body.is_null() && !body.as_object().is_some_and(|fields| fields.is_empty()) {
            resolver.reject(EditorError::InvalidSelection);
            return Ok(());
        }
        let picker = rfd::AsyncFileDialog::new()
            .set_parent(&webview.window())
            .set_title("파일 열기")
            .pick_file();
        let webview = webview.clone();
        resolver.respond_async(async move {
            let selected = picker.await;
            tauri::async_runtime::spawn_blocking(move || {
                webview.state::<LocalEditors>().selected(
                    webview.label(),
                    document,
                    selected.as_ref().map(rfd::FileHandle::path),
                )
            })
            .await
            .map_err(|_| EditorError::Unavailable)?
            .map_err(Into::into)
        });
        return Ok(());
    }

    let argument = |key| CommandItem {
        name: command,
        key,
        plugin: None,
        message: &message,
        acl: &acl,
    };
    if matches!(command, CLAIM | RESPOND) {
        let request_id = match String::from_command(argument("requestId")) {
            Ok(id) => id,
            Err(error) => {
                resolver.invoke_error(error);
                return Ok(());
            }
        };
        if command == CLAIM {
            resolver.respond(editors.claim(webview.label(), document, &request_id).map_err(InvokeError::from));
        } else {
            let result = (|| {
                let handle = String::from_command(argument("handle"))?;
                let attached = bool::from_command(argument("attached"))?;
                editors.respond_to_offer(webview.label(), document, &request_id, &handle, attached).map_err(InvokeError::from)
            })();
            resolver.respond(result);
        }
        return Ok(());
    }
    let handle = match String::from_command(argument("handle")) {
        Ok(handle) => handle,
        Err(error) => {
            resolver.invoke_error(error);
            return Ok(());
        }
    };
    match command {
        SAVE => {
            let text = match String::from_command(argument("text")) {
                Ok(text) => text,
                Err(error) => {
                    resolver.invoke_error(error);
                    return Ok(());
                }
            };
            let webview = webview.clone();
            blocking(resolver, move || {
                webview
                    .state::<LocalEditors>()
                    .save(webview.label(), document, &handle, &text)
            });
        }
        COMPARE | RELOAD => {
            let webview = webview.clone();
            blocking(resolver, move || {
                let editors = webview.state::<LocalEditors>();
                if command == COMPARE {
                    editors.compare(webview.label(), document, &handle)
                } else {
                    editors.reload(webview.label(), document, &handle)
                }
            });
        }
        CLOSE => resolver.respond(
            editors
                .close(webview.label(), document, &handle)
                .map_err(InvokeError::from),
        ),
        _ => unreachable!(),
    }
    Ok(())
}
