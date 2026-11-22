mod connections;
mod editor;
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
        .on_page_load(|webview, payload| {
            if payload.event() == tauri::webview::PageLoadEvent::Started {
                webview.state::<Sessions>().navigate(webview.label());
                webview.state::<LocalEditors>().navigate(webview.label());
            }
        })
        .invoke_handler(move |invoke| {
            if invoke.message.command() == "terminal_open" {
                terminal_open(invoke);
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
                app.state::<Sessions>().destroy(&label);
                app.state::<LocalEditors>().destroy(&label);
            }
            tauri::RunEvent::Exit => app.state::<Sessions>().shutdown(),
            _ => {}
        });
}
