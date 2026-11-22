fn main() {
    let terminal = tauri_build::AppManifest::new().commands(&[
        "terminal_open", "terminal_start", "terminal_input", "terminal_resize", "terminal_ack",
        "terminal_close",
    ]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(terminal))
        .expect("failed to build app permissions");
}
