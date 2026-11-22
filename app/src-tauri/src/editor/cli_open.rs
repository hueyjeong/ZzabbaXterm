use tauri::{AppHandle, Emitter, EventTarget, Manager};

use super::{LocalFile, cli::Pending, registry::LocalEditors};

pub(super) const MAIN: &str = "main";

pub(crate) fn prepare_cli(app: AppHandle, pending: Pending) -> impl Future<Output = ()> + Send + 'static {
    let targeted = (|| {
        let (document, lifetime) = app.state::<LocalEditors>().scope(MAIN).ok()?;
        let webview = app.get_webview_window(MAIN)?;
        (webview.as_ref().window().label() == MAIN).then_some((document, lifetime))
    })();
    async move {
        let Some((document, lifetime)) = targeted else { return; };
        let editors = app.state::<LocalEditors>();
        let opening = pending.opening.clone();
        if lifetime.is_cancelled() || pending.is_inactive() { return; }
        if !pending.path().is_absolute() || pending.path().to_str().is_none() {
            pending.open_failed();
            return;
        }

        let path = pending.path().to_owned();
        let read_lifetime = lifetime.clone();
        let read_opening = opening.clone();
        let opened = tokio::select! {
            biased;
            _ = lifetime.cancelled() => return,
            _ = opening.cancel.cancelled() => return,
            result = tauri::async_runtime::spawn_blocking(move || {
                if read_lifetime.is_cancelled() || read_opening.is_inactive() { return None; }
                Some(LocalFile::open(&path))
            }) => result,
        };
        if lifetime.is_cancelled() || pending.is_inactive() { return; }
        let Ok(Some(Ok((file, text)))) = opened else { pending.open_failed(); return; };
        let Ok((request_id, completed)) = editors.offer(MAIN, document, pending, file, text) else { return; };

        #[derive(Clone, serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Request<'a> { request_id: &'a str }

        if app.emit_to(EventTarget::webview(MAIN), "editor-local-cli-open", Request { request_id: &request_id }).is_err() {
            editors.discard_offer(document, &request_id, true);
            return;
        }
        tokio::select! {
            biased;
            _ = lifetime.cancelled() => {},
            _ = opening.cancel.cancelled() => editors.discard_offer(document, &request_id, false),
            _ = completed => {},
        }
    }
}
