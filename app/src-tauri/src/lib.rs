mod connections;
pub mod ssm;
mod terminal;
mod workspace_window;

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
        .plugin(ssm::ipc::lifecycle())
        .on_page_load(|webview, payload| {
            if payload.event() == tauri::webview::PageLoadEvent::Started {
                webview.state::<Sessions>().navigate(webview.label());
            }
        })
        .invoke_handler(move |invoke| {
            if invoke.message.command() == "terminal_open" {
                terminal_open(invoke);
                true
            } else if invoke.message.command() == ssm::ipc::COMMAND {
                ssm::ipc::dispatch(invoke);
                true
            } else {
                connections::ipc::handle(invoke, other_commands)
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| match event {
            tauri::RunEvent::WindowEvent { label, event: tauri::WindowEvent::Destroyed, .. } => {
                app.state::<Sessions>().destroy(&label);
            }
            tauri::RunEvent::Exit => app.state::<Sessions>().shutdown(),
            _ => {}
        });
}
