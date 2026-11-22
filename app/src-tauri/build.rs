fn main() {
    let app = tauri_build::AppManifest::new().commands(&[
        "terminal_open", "terminal_start", "terminal_input", "terminal_resize", "terminal_ack",
        "terminal_close", "workspace_window",
    ]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(app))
        .expect("failed to build app permissions");
}
