pub mod ai;
mod connections;
mod editor;
mod local_ipc;
#[allow(dead_code, reason = "SFTP는 별도 통합 경계")]
mod ssh;
pub mod ssm;
mod terminal;
mod workspace_window;

use editor::registry::LocalEditors;
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
                webview.state::<Sessions>().navigate(webview.label());
                webview.state::<LocalEditors>().navigate(webview.label());
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
            tauri::RunEvent::WindowEvent { label, event: tauri::WindowEvent::Destroyed, .. } => {
                if let Some(registration) = app.try_state::<ssh::registration::Registration>() {
                    registration.document_changed(&label);
                }
                app.state::<Sessions>().destroy(&label);
                app.state::<LocalEditors>().destroy(&label);
            }
            tauri::RunEvent::Exit => app.state::<Sessions>().shutdown(),
            _ => {}
        });
}
