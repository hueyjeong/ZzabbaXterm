use tauri::{AppHandle, Emitter, EventTarget, Manager, Window};
use tokio_util::sync::CancellationToken;

use super::{LocalFile, cli::Pending, registry::LocalEditors};

pub(super) const MAIN: &str = "main";

pub(crate) fn prepare_cli(app: AppHandle, pending: Pending) -> impl Future<Output = ()> + Send + 'static {
    let targeted = (|| {
        let (document, lifetime) = app.state::<LocalEditors>().scope(MAIN).ok()?;
        let webview = app.get_webview(MAIN)?;
        let window = webview.window();
        (window.label() == MAIN).then_some((window, document, lifetime))
    })();
    async move {
        let Some((window, document, lifetime)) = targeted else { return; };
        let editors = app.state::<LocalEditors>();
        let opening = pending.opening.for_dialog();
        if lifetime.is_cancelled() || pending.is_inactive() { return; }
        let Some(path) = pending.path().to_str().filter(|_| pending.path().is_absolute()) else {
            pending.open_failed();
            return;
        };
        let description = format!("다음 로컬 파일을 열까요?\n\n{path:?}");
        let approved = tokio::select! {
            biased;
            _ = lifetime.cancelled() => return,
            _ = opening.cancel.cancelled() => return,
            result = confirm(window, description, &opening.cancel) => result,
        };
        if lifetime.is_cancelled() || pending.is_inactive() { return; }
        match approved {
            Some(true) => {}
            Some(false) => { pending.decline(); return; }
            None => { pending.open_failed(); return; }
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

#[cfg(not(target_os = "linux"))]
async fn confirm(window: Window, description: String, _: &CancellationToken) -> Option<bool> {
    #[cfg(windows)]
    window.hwnd().ok()?;
    #[cfg(target_os = "macos")]
    window.ns_window().ok()?;
    let result = rfd::AsyncMessageDialog::new().set_parent(&window)
        .set_title("로컬 파일 열기").set_description(description)
        .set_buttons(rfd::MessageButtons::OkCancel).show().await;
    Some(result == rfd::MessageDialogResult::Ok)
}

#[cfg(target_os = "linux")]
async fn confirm(window: Window, description: String, cancel: &CancellationToken) -> Option<bool> {
    use gtk::{ButtonsType, DialogFlags, MessageDialog, MessageType, ResponseType, prelude::*};

    let cancel = cancel.clone();
    let (send, receive) = tokio::sync::oneshot::channel();
    let owner = window.clone();
    window.run_on_main_thread(move || {
        let Ok(parent) = owner.gtk_window() else { return; };
        gtk::glib::MainContext::default().spawn_local(async move {
            let mut send = send;
            if send.is_closed() || cancel.is_cancelled() { return; }
            let dialog = MessageDialog::new(Some(&parent), DialogFlags::MODAL | DialogFlags::DESTROY_WITH_PARENT,
                MessageType::Question, ButtonsType::None, &description);
            dialog.set_title("로컬 파일 열기");
            dialog.add_button("취소", ResponseType::Cancel);
            dialog.add_button("열기", ResponseType::Accept);
            let answer = tokio::select! {
                biased;
                _ = send.closed() => None,
                _ = cancel.cancelled() => None,
                answer = dialog.run_future() => Some(answer == ResponseType::Accept),
            };
            dialog.close();
            let _ = send.send(answer);
        });
    }).ok()?;
    receive.await.ok().flatten()
}
