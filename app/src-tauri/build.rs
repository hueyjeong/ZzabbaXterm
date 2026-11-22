fn main() {
    let app = tauri_build::AppManifest::new().commands(&[
        "terminal_open", "terminal_start", "terminal_input", "terminal_resize", "terminal_ack",
        "terminal_close", "workspace_window",
        "connection_test_start", "connection_test_wait", "connection_test_cancel", "connection_save",
        "connections_load",
        "verify_ssh_host",
        "ssm_discovery",
    ]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(app))
        .expect("failed to build app permissions");
}
