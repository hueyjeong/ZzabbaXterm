pub mod ai;
mod connections;
mod editor;
mod local_ipc;
mod ssh;
pub mod ssm;
mod storage;
mod terminal;
mod workspace_window;

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
        .plugin(ssm::ipc::lifecycle())
        .setup(|app| {
            if let Ok(registration) = connections::ipc::initialize_ssh_registration(app) {
                app.manage(registration);
            }
            ai::cli::start(app.handle().clone());
            Ok(())
        })
        .on_page_load(|webview, payload| {
            if payload.event() == tauri::webview::PageLoadEvent::Started {
                if let Some(registration) = webview.try_state::<ssh::registration::Registration>() {
                    registration.document_changed(webview.label());
                }
                let sessions = webview.state::<Sessions>();
                sessions.navigate(webview.label());
                webview.state::<SftpIpc>().navigate(webview.label(), sessions.document(webview.label()));
                webview.state::<LocalEditors>().navigate(webview.label());
                webview.state::<StorageIpc>().navigate(webview.label());
            }
        })
        .invoke_handler(move |invoke| {
            if invoke.message.command() == "terminal_open" {
                terminal_open(invoke);
                true
            } else if ai::ipc::handles(invoke.message.command()) {
                ai::ipc::dispatch(invoke);
                true
            } else if invoke.message.command() == ssm::ipc::COMMAND {
                ssm::ipc::dispatch(invoke);
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
                if !app.state::<Sessions>().close_ssh_probes(Some(&label)) {
                    api.prevent_close();
                }
            }
            tauri::RunEvent::ExitRequested { api, .. } => {
                if !app.state::<Sessions>().close_ssh_probes(None) {
                    api.prevent_exit();
                }
            }
            tauri::RunEvent::WindowEvent { label, event: tauri::WindowEvent::Destroyed, .. } => {
                if let Some(registration) = app.try_state::<ssh::registration::Registration>() {
                    registration.document_changed(&label);
                }
                app.state::<Sessions>().destroy(&label);
                app.state::<SftpIpc>().destroy(&label);
                app.state::<LocalEditors>().destroy(&label);
                app.state::<StorageIpc>().navigate(&label);
            }
            tauri::RunEvent::Exit => {
                app.state::<SftpIpc>().shutdown();
                app.state::<StorageIpc>().shutdown();
                app.state::<Sessions>().shutdown();
            }
            _ => {}
        });
}
