fn main() {
    let app = tauri_build::AppManifest::new().commands(&[
        "terminal_open", "terminal_start", "terminal_input", "terminal_resize", "terminal_ack",
        "terminal_close", "workspace_window",
        "connection_test_start", "connection_test_wait", "connection_test_cancel", "connection_save",
        "connections_load",
        "ssm_discovery",
        "storage_open", "storage_list_buckets", "storage_list_objects", "storage_select_local_file",
        "storage_upload", "storage_download", "storage_cancel_approval", "storage_close",
    ]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(app))
        .expect("failed to build app permissions");
}
