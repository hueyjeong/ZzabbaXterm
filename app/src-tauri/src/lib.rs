mod terminal;

use tauri::Manager;
use terminal::{Sessions, ipc::*};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(Sessions::default())
        .on_page_load(|webview, payload| {
            if payload.event() == tauri::webview::PageLoadEvent::Started {
                webview.state::<Sessions>().navigate(webview.label());
            }
        })
        .invoke_handler(tauri::generate_handler![
            terminal_open, terminal_start, terminal_input, terminal_resize, terminal_ack,
            terminal_close,
        ])
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
