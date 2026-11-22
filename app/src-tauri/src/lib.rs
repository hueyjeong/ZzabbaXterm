pub mod ai;
mod connections;
mod editor;
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
    ];
    tauri::Builder::default()
        .manage(Sessions::default())
        .manage(LocalEditors::default())
        .manage(SftpIpc::default())
        .manage(StorageIpc::default())
        .manage(ai::ipc::State::default())
        .manage(ai::session_selection::State::default())
        .manage(tunnels::ipc::TunnelTabs::default())
        .manage(rdp::ipc::Sessions::default())
        .manage(vnc::ipc::Sessions::default())
        .register_asynchronous_uri_scheme_protocol("rdp-packet", rdp::ipc::protocol)
        .register_asynchronous_uri_scheme_protocol("vnc-packet", vnc::output::protocol)
        .plugin(ssm::ipc::lifecycle())
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
                workspace_window::request_close(window);
            }
        })
        .on_page_load(|webview, payload| {
            if payload.event() == tauri::webview::PageLoadEvent::Started {
                webview.state::<LocalEditors>().navigate(webview.label());
                let main = webview.label() == "main" && webview.window_ref().label() == "main";
                if main {
                    webview.state::<rdp::ipc::Sessions>().retire();
                    webview.state::<vnc::ipc::Sessions>().retire(false);
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
            tauri::RunEvent::ExitRequested { api, .. } => {
                if !app.state::<Sessions>().close_ssh_probes(None) {
                    api.prevent_exit();
                }
            }
            tauri::RunEvent::WindowEvent { label, event: tauri::WindowEvent::Destroyed, .. } => {
                if label == "main" {
                    app.state::<rdp::ipc::Sessions>().retire();
                    app.state::<vnc::ipc::Sessions>().retire(true);
                }
                if let Some(registration) = app.try_state::<ssh::registration::Registration>() {
                    registration.document_changed(&label);
                }
                let sessions = app.state::<Sessions>();
                sessions.destroy(&label);
                app.state::<ai::session_selection::State>()
                    .document_changed(&label, sessions.document(&label));
                app.state::<SftpIpc>().destroy(&label);
                app.state::<LocalEditors>().destroy(&label);
                app.state::<StorageIpc>().navigate(&label);
                if label == "main" {
                    app.state::<tunnels::ipc::TunnelTabs>().navigate(true);
                }
            }
            tauri::RunEvent::Exit => {
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
