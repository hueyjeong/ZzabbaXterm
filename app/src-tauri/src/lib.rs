pub mod ai;
mod navigation;
mod connections;
mod desktop_dialog;
mod editor;
#[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
mod elevation;
mod local_ipc;
mod rdp;
mod ssh;
pub mod ssm;
mod storage;
mod terminal;
mod tunnels;
mod vnc;
mod workspace_window;

pub mod editor_cli {
    pub use crate::editor::cli::{CliError, run};
}

#[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
#[path = "auth"]
pub mod local_auth {
    #[path = "mod.rs"]
    mod implementation;
    pub use self::implementation::run_askpass;
    pub(crate) use self::implementation::{
        Approval, Bitwarden, Document, Error, Exchange, Pending, TrustedFile, installed_helper,
    };
    pub(crate) use self::implementation::sudo::{
        failure as sudo_failure, program as sudo_program, spawn as spawn_sudo,
    };
    pub(crate) use self::implementation::vault::valid_item;
}

use editor::registry::LocalEditors;
use editor::sftp_ipc::SftpIpc;
use storage::ipc::{self as storage_ipc, StorageIpc};
use tauri::Manager;
use terminal::{Sessions, ipc::*};
use workspace_window::workspace_window;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let other_commands: fn(tauri::ipc::Invoke) -> bool = tauri::generate_handler![
        terminal_start, terminal_input, terminal_resize, terminal_ack, terminal_close,
        workspace_window,
        ssm::target_login::ssm_target_relogin, ssm::target_login::ssm_target_relogin_cancel,
    ];
    tauri::Builder::default()
        .manage(Sessions::default())
        .manage(LocalEditors::default())
        .manage(SftpIpc::default())
        .manage(editor::nano_bridge::NanoBridge::default())
        .manage(StorageIpc::default())
        .manage(ai::ipc::State::default())
        .manage(ai::execution::State::default())
        .manage(ai::session_selection::State::default())
        .manage(tunnels::ipc::TunnelTabs::default())
        .manage(rdp::ipc::Sessions::default())
        .manage(vnc::ipc::Sessions::default())
        .manage(ssm::target_login::TargetLogins::default())
        .register_asynchronous_uri_scheme_protocol("rdp-packet", rdp::ipc::protocol)
        .register_asynchronous_uri_scheme_protocol("vnc-packet", vnc::output::protocol)
        .plugin(ssm::ipc::lifecycle())
        .plugin(navigation::guard())
        .plugin(navigation::clipboard_read())
        .setup(|app| {
            if let Ok(registration) = connections::ipc::initialize_ssh_registration(app) {
                app.manage(registration);
            }
            ai::cli::start(app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == workspace_window::MAIN
                && let tauri::WindowEvent::CloseRequested { api, .. } = event
            {
                api.prevent_close();
                workspace_window::request_close(window, None);
            }
        })
        .on_page_load(|webview, payload| {
            if payload.event() == tauri::webview::PageLoadEvent::Started {
                webview.state::<LocalEditors>().navigate(webview.label());
                let main = webview.label() == "main" && webview.window_ref().label() == "main";
                if main {
                    webview.state::<rdp::ipc::Sessions>().retire();
                    webview.state::<vnc::ipc::Sessions>().retire(false);
                    webview.state::<ai::execution::State>().cancel_all();
                    webview.state::<ssm::target_login::TargetLogins>().cancel_all();
                }
                if let Some(registration) = webview.try_state::<ssh::registration::Registration>() {
                    registration.document_changed(webview.label());
                }
                let sessions = webview.state::<Sessions>();
                sessions.navigate(webview.label());
                webview.state::<ai::session_selection::State>()
                    .document_changed(webview.label(), sessions.document(webview.label()));
                webview.state::<SftpIpc>().navigate(webview.label(), sessions.document(webview.label()));
                webview.state::<StorageIpc>().navigate(webview.label());
                if main {
                    webview.state::<tunnels::ipc::TunnelTabs>().navigate(false);
                }
            } else if payload.event() == tauri::webview::PageLoadEvent::Finished {
                rdp::ipc::issue_document(&webview, payload.url());
                vnc::ipc::issue_document(&webview, payload.url());
            }
        })
        .invoke_handler(move |invoke| {
            if invoke.message.command() == "terminal_open" {
                terminal_open(invoke);
                true
            } else if invoke.message.command() == "elevation_open" {
                #[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
                {
                    elevation_open(invoke);
                    true
                }
                #[cfg(not(all(feature = "stored-auth", any(windows, target_os = "linux"))))]
                {
                    invoke.resolver.reject("이 빌드에서는 저장 인증 기반 sudo 승격을 사용할 수 없습니다");
                    true
                }
            } else if invoke.message.command() == "elevation_open_remote" {
                #[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
                {
                    elevation_open_remote(invoke);
                    true
                }
                #[cfg(not(all(feature = "stored-auth", any(windows, target_os = "linux"))))]
                {
                    invoke.resolver.reject("이 빌드에서는 저장 인증 기반 sudo 승격을 사용할 수 없습니다");
                    true
                }
            } else if ai::session_selection::handles(invoke.message.command()) {
                ai::session_selection::dispatch(invoke);
                true
            } else if ai::ipc::handles(invoke.message.command()) {
                ai::ipc::dispatch(invoke);
                true
            } else if invoke.message.command() == ssm::ipc::COMMAND {
                ssm::ipc::dispatch(invoke);
                true
            } else if tunnels::ipc::handles(invoke.message.command()) {
                tunnels::ipc::dispatch(invoke);
                true
            } else if rdp::ipc::handles(invoke.message.command()) {
                rdp::ipc::dispatch(invoke);
                true
            } else if vnc::ipc::COMMANDS.contains(&invoke.message.command()) {
                vnc::ipc::dispatch(invoke);
                true
            } else if invoke.message.command().starts_with("editor_sftp_") {
                editor::sftp_ipc::dispatch(invoke)
            } else if invoke.message.command().starts_with("editor_remote_nano_") {
                editor::nano_ipc::dispatch(invoke)
            } else if invoke.message.command().starts_with("storage_") {
                storage_ipc::dispatch(invoke)
            } else {
                match editor::ipc::dispatch(invoke) {
                    Ok(()) => true,
                    Err(invoke) => connections::ipc::handle(invoke, other_commands),
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| match event {
            tauri::RunEvent::WindowEvent { label, event: tauri::WindowEvent::CloseRequested { api, .. }, .. } => {
                if label != workspace_window::MAIN && !app.state::<Sessions>().close_ssh_probes(Some(&label)) {
                    api.prevent_close();
                }
            }
            tauri::RunEvent::ExitRequested { api, code, .. } => {
                let sessions = app.state::<Sessions>();
                if let Some(main) = app.get_webview_window(workspace_window::MAIN) {
                    api.prevent_exit();
                    workspace_window::request_close(&main.as_ref().window(), code);
                } else if !sessions.consume_control_exit_approval(code) && !sessions.close_ssh_probes(None) {
                    api.prevent_exit();
                }
            }
            tauri::RunEvent::WindowEvent { label, event: tauri::WindowEvent::Destroyed, .. } => {
                if label == "main" {
                    app.state::<rdp::ipc::Sessions>().retire();
                    app.state::<vnc::ipc::Sessions>().retire(true);
                    app.state::<ssm::target_login::TargetLogins>().cancel_all();
                }
                if let Some(registration) = app.try_state::<ssh::registration::Registration>() {
                    registration.document_changed(&label);
                }
                let sessions = app.state::<Sessions>();
                let exit_code = sessions.destroy(&label);
                app.state::<ai::session_selection::State>()
                    .document_changed(&label, sessions.document(&label));
                app.state::<SftpIpc>().destroy(&label);
                app.state::<LocalEditors>().destroy(&label);
                app.state::<StorageIpc>().navigate(&label);
                if label == "main" {
                    app.state::<tunnels::ipc::TunnelTabs>().navigate(true);
                }
                // Wry는 이 콜백 뒤 마지막 창의 자동 종료 이벤트를 보낸다.
                // 명시적 종료만 재예약하고, 자동 이벤트에는 그 승인 코드를 소비시키지 않는다.
                if let Some(code) = exit_code { app.exit(code); }
            }
            tauri::RunEvent::Exit => {
                app.state::<ssm::target_login::TargetLogins>().cancel_all();
                app.state::<rdp::ipc::Sessions>().retire();
                app.state::<vnc::ipc::Sessions>().retire(true);
                app.state::<tunnels::ipc::TunnelTabs>().navigate(true);
                app.state::<LocalEditors>().destroy("main");
                app.state::<SftpIpc>().shutdown();
                app.state::<StorageIpc>().shutdown();
                app.state::<Sessions>().shutdown();
            }
            _ => {}
        });
}
