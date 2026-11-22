use serde::{Deserialize, Serialize};
use tauri::{Emitter, EventTarget, Manager, Url, Webview, Window};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::{
    editor::registry::{CloseAttempt, CloseDecision, CloseReport, LocalEditors},
    terminal::Sessions,
};

pub(crate) const MAIN: &str = "main";

pub(crate) const EXPIRED: &str = "창 닫기 요청이 만료되었습니다";

pub(crate) type WindowKey = usize;

pub(crate) fn window_key(window: &Window) -> Option<WindowKey> {
    #[cfg(windows)]
    return window.hwnd().ok().map(|handle| handle.0 as usize);
    #[cfg(target_os = "linux")]
    return window.gtk_window().ok().map(|handle| {
        use gtk::prelude::ObjectType;
        handle.as_ptr() as usize
    });
    #[cfg(target_os = "macos")]
    return window.ns_window().ok().map(|handle| handle as usize);
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    return None;
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum WindowAction {
    Drag,
    ToggleMaximize,
    Minimize,
    Close,
    PendingClose,
    RespondToClose,
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum WindowReply {
    Pending(Option<String>),
    Responded(bool),
}

fn trusted_url(webview: &Webview, url: &Url) -> bool {
    let bundled = if cfg!(any(windows, target_os = "android")) {
        url.host_str() == Some("tauri.localhost") && matches!(url.scheme(), "http" | "https") && url.port().is_none()
    } else {
        url.host_str() == Some("localhost") && url.scheme() == "tauri" && url.port().is_none()
    };
    let development = tauri::is_dev() && webview.config().build.dev_url.as_ref()
        .is_some_and(|app| app.origin() == url.origin());
    bundled || development
}

pub(crate) fn request_close(window: &Window) {
    let Some(key) = window_key(window) else { return; };
    let webview = close_webview(window, key);
    let url = webview.as_ref().and_then(|webview| webview.url().ok());
    let trusted = webview.as_ref().zip(url.as_ref()).is_some_and(|(webview, url)| trusted_url(webview, url));
    let attempt = match window.state::<LocalEditors>().request_window_close(key, url, trusted) {
        Ok(Some(attempt)) => attempt,
        Ok(None) => return,
        Err(_) => {
            eprintln!("창 닫기 요청 생성 실패");
            return;
        }
    };

    #[derive(Clone, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Request<'a> { request_id: &'a str }

    let emitted = attempt.reply.is_some() && window.emit_to(
        EventTarget::webview(MAIN), "workspace-close-requested", Request { request_id: &attempt.id },
    ).is_ok();
    let window = window.clone();
    tauri::async_runtime::spawn(async move {
        run_close(window, key, attempt, emitted).await;
    });
}

async fn run_close(window: Window, key: WindowKey, mut attempt: CloseAttempt, emitted: bool) {
    let editors = window.state::<LocalEditors>();
    let report = if let Some(mut reply) = attempt.reply.take() {
        let received = if emitted {
            tokio::select! {
                biased;
                _ = attempt.revoked.cancelled() => Some(None),
                result = &mut reply => Some(result.ok()),
                _ = tokio::time::sleep_until(attempt.deadline.into()) => None,
            }
        } else { None };
        match received {
            Some(report) => report,
            None if editors.use_close_fallback(key, &attempt.id) => None,
            None => {
                // 마감 직전에 레지스트리가 받은 응답은 타이머가 먼저 깨어나도 유효하다.
                tokio::select! {
                    biased;
                    _ = attempt.revoked.cancelled() => None,
                    result = reply => result.ok(),
                }
            }
        }
    } else { None };

    let (decision, completed) = match report {
        Some(CloseReport { decision, completed }) => (Some(decision), Some(completed)),
        None => (None, None),
    };
    let result = if attempt.revoked.is_cancelled() {
        Err(EXPIRED.into())
    } else {
        complete_close(&window, key, &attempt, decision).await
    };
    if !matches!(result, Ok(true)) { editors.clear_window_close(&attempt.id); }
    if let Some(completed) = completed {
        let _ = completed.send(result);
    } else if result.is_err_and(|error| error != EXPIRED) {
        eprintln!("창 닫기 보류: 네이티브 닫기 또는 SSH 종료 확인 실패");
    }
}

async fn complete_close(window: &Window, key: WindowKey, attempt: &CloseAttempt, decision: Option<CloseDecision>) -> Result<bool, String> {
    let require_trusted_url = decision.is_some();
    let prompt = match decision {
        Some(CloseDecision::Ready) => None,
        Some(CloseDecision::Discard { files }) if !files.is_empty() && files.iter().all(|file| !file.contains('\0')) => Some(format!(
            "저장하지 않은 편집을 버리고 창을 닫으시겠습니까?\n\n{}", files.join("\n"),
        )),
        Some(CloseDecision::Discard { .. }) => return Err("창 닫기 확인 목록이 올바르지 않습니다".into()),
        Some(CloseDecision::Blocked) => return Ok(false),
        None => Some("편집 상태를 확인할 수 없습니다. 저장하지 않은 편집을 버리고 창을 닫으시겠습니까?".into()),
    };

    let (answer, accepted) = oneshot::channel();
    let parent = window.clone();
    let id = attempt.id.clone();
    let revoked = attempt.revoked.clone();
    window.run_on_main_thread(move || {
        let result: Result<Option<bool>, String> = (|| {
            valid_close(&parent, key, &id, require_trusted_url)?;
            let enabled = parent.is_enabled().map_err(|error| error.to_string())?;
            if !enabled || modal_open(&parent) { return Ok(None); }
            if let Some(prompt) = prompt {
                if !confirm_native(&parent, &prompt, &revoked) { return Ok(None); }
                valid_close(&parent, key, &id, require_trusted_url)?;
                if modal_open(&parent) { return Ok(None); }
            }
            let was_enabled = parent.is_enabled().map_err(|error| error.to_string())?;
            if was_enabled {
                parent.set_enabled(false).map_err(|error| error.to_string())?;
                if !matches!(parent.is_enabled(), Ok(false)) {
                    parent.set_enabled(true).map_err(|error| error.to_string())?;
                    return Err("창 입력 차단 실패".into());
                }
            }
            Ok(Some(was_enabled))
        })();
        let _ = answer.send(result);
    }).map_err(|error| error.to_string())?;
    let Some(was_enabled) = accepted.await.map_err(|error| error.to_string())?? else { return Ok(false); };

    let app = window.app_handle().clone();
    let result: Result<(), String> = async {
        let revoked = attempt.revoked.clone();
        let confirmed = tauri::async_runtime::spawn_blocking(move || {
            if revoked.is_cancelled() { return Err(EXPIRED); }
            Ok(app.state::<Sessions>().close_ssh_probes(Some(MAIN)))
        }).await.map_err(|error| error.to_string())??;
        if !confirmed { return Err("SSH 시험 자식 종료 확인 불가".into()); }

        let (answer, completed) = oneshot::channel();
        let parent = window.clone();
        let id = attempt.id.clone();
        // 실제 창과 현재 문서를 확인한 같은 이벤트 루프에서 폐기를 예약한다.
        window.run_on_main_thread(move || {
            let result = (|| {
                let url = valid_close(&parent, key, &id, require_trusted_url)?;
                parent.state::<LocalEditors>().queue_window_destroy(key, &id, url.as_ref(), || {
                    parent.destroy().map_err(|error| error.to_string())
                })
            })();
            let _ = answer.send(result);
        }).map_err(|error| error.to_string())?;
        completed.await.map_err(|error| error.to_string())?
    }.await;
    if let Err(error) = result {
        window.state::<Sessions>().release_close_gate(Some(MAIN));
        if was_enabled {
            let (answer, restored) = oneshot::channel();
            let parent = window.clone();
            window.run_on_main_thread(move || {
                let result = if window_key(&parent) == Some(key) { parent.set_enabled(true) } else { Ok(()) };
                let _ = answer.send(result);
            }).map_err(|restore| format!("{error} · {restore}"))?;
            restored.await.map_err(|restore| format!("{error} · {restore}"))?
                .map_err(|restore| format!("{error} · {restore}"))?;
        }
        return Err(error);
    }
    Ok(true)
}

#[cfg(target_os = "linux")]
fn modal_open(window: &Window) -> bool {
    use gtk::prelude::*;
    let Ok(parent) = window.gtk_window() else { return true; };
    let parent = parent.upcast::<gtk::Window>();
    gtk::Window::list_toplevels().into_iter().any(|widget| {
        widget.downcast::<gtk::Window>().is_ok_and(|dialog| {
            dialog.is_visible() && dialog.is_modal() && dialog.transient_for().as_ref() == Some(&parent)
        })
    })
}

#[cfg(not(target_os = "linux"))]
fn modal_open(_: &Window) -> bool { false }

fn close_webview(window: &Window, key: WindowKey) -> Option<Webview> {
    window.app_handle().get_webview(MAIN).filter(|webview| window_key(&webview.window()) == Some(key))
}

fn valid_close(window: &Window, key: WindowKey, id: &str, require_trusted_url: bool) -> Result<Option<Url>, &'static str> {
    if window_key(window) != Some(key) { return Err(EXPIRED); }
    let webview = close_webview(window, key);
    let url = webview.as_ref().and_then(|webview| webview.url().ok());
    if require_trusted_url && !webview.as_ref().zip(url.as_ref()).is_some_and(|(webview, url)| trusted_url(webview, url)) {
        return Err(EXPIRED);
    }
    window.state::<LocalEditors>().check_window_close(key, id, url.as_ref()).then_some(url).ok_or(EXPIRED)
}

#[cfg(target_os = "linux")]
fn confirm_native(window: &Window, prompt: &str, revoked: &CancellationToken) -> bool {
    use gtk::prelude::*;
    let Ok(parent) = window.gtk_window() else { return false; };
    let dialog = gtk::MessageDialog::new(Some(&parent), gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        gtk::MessageType::Warning, gtk::ButtonsType::OkCancel, prompt);
    dialog.set_title("창 닫기");
    dialog.set_default_response(gtk::ResponseType::Cancel);
    let timer = gtk::glib::timeout_add_local(std::time::Duration::from_millis(100), {
        let dialog = dialog.clone();
        let revoked = revoked.clone();
        move || {
            if revoked.is_cancelled() { dialog.response(gtk::ResponseType::Cancel); }
            gtk::glib::ControlFlow::Continue
        }
    });
    let accepted = dialog.run() == gtk::ResponseType::Ok;
    timer.remove();
    dialog.close();
    accepted
}

#[cfg(windows)]
fn confirm_native(window: &Window, prompt: &str, revoked: &CancellationToken) -> bool {
    use windows_sys::Win32::{Foundation::HWND, UI::{Controls::*, WindowsAndMessaging::*}};

    unsafe extern "system" fn event(hwnd: HWND, message: u32, _: usize, _: isize, context: isize) -> i32 {
        if message == TDN_TIMER as u32 && unsafe { &*(context as *const CancellationToken) }.is_cancelled() {
            unsafe { SendMessageW(hwnd, TDM_CLICK_BUTTON as u32, IDCANCEL as usize, 0); }
        }
        0
    }

    let Ok(parent) = window.hwnd() else { return false; };
    let title: Vec<_> = "창 닫기\0".encode_utf16().collect();
    let prompt: Vec<_> = prompt.encode_utf16().chain([0]).collect();
    let config = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: parent.0,
        dwFlags: TDF_CALLBACK_TIMER | TDF_ALLOW_DIALOG_CANCELLATION | TDF_SIZE_TO_CONTENT,
        dwCommonButtons: TDCBF_OK_BUTTON | TDCBF_CANCEL_BUTTON,
        nDefaultButton: IDCANCEL,
        pszWindowTitle: title.as_ptr(),
        pszContent: prompt.as_ptr(),
        pfCallback: Some(event),
        lpCallbackData: revoked as *const CancellationToken as isize,
        ..Default::default()
    };
    let mut button = 0;
    // TaskDialogIndirect가 반환할 때까지 문자열과 철회 토큰이 살아 있다.
    let status = unsafe { TaskDialogIndirect(&config, &mut button, std::ptr::null_mut(), std::ptr::null_mut()) };
    status == 0 && button == IDOK
}

#[cfg(not(any(windows, target_os = "linux")))]
fn confirm_native(window: &Window, prompt: &str, _: &CancellationToken) -> bool {
    // macOS의 NSAlert는 첫 번째 버튼이 기본 동작이다.
    rfd::MessageDialog::new().set_parent(window).set_title("창 닫기").set_description(prompt)
        .set_level(rfd::MessageLevel::Warning)
        .set_buttons(rfd::MessageButtons::OkCancelCustom("취소".into(), "닫기".into())).show()
        == rfd::MessageDialogResult::Custom("닫기".into())
}

#[tauri::command]
pub(crate) async fn workspace_window(
    webview: Webview,
    action: WindowAction,
    request_id: Option<String>,
    decision: Option<CloseDecision>,
) -> Result<WindowReply, String> {
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
        WindowAction::PendingClose => {
            let pending = window_key(&window).zip(webview.url().ok()).and_then(|(key, url)| {
                if !trusted_url(&webview, &url) { return None; }
                webview.state::<LocalEditors>().pending_window_close(key, &url)
            });
            return Ok(WindowReply::Pending(pending));
        }
        WindowAction::RespondToClose => {
            let (Some(id), Some(decision)) = (request_id, decision) else { return Err(EXPIRED.into()); };
            let key = window_key(&window).ok_or(EXPIRED)?;
            let url = webview.url().map_err(|_| EXPIRED)?;
            if !trusted_url(&webview, &url) { return Err(EXPIRED.into()); }
            let result = webview.state::<LocalEditors>().respond_to_window_close(key, &url, &id, decision)?;
            return result.await.map_err(|_| EXPIRED)?.map(WindowReply::Responded);
        }
    }
    .map(|()| WindowReply::Pending(None))
    .map_err(|error| error.to_string())
}
