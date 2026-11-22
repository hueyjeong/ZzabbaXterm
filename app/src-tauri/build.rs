fn main() {
    let app = tauri_build::AppManifest::new().commands(&[
        "terminal_open", "terminal_start", "terminal_input", "terminal_resize", "terminal_ack",
        "terminal_close", "workspace_window",
        "connection_test_start", "connection_test_wait", "connection_test_cancel", "connection_save",
        "connections_load",
        "editor_local_open", "editor_local_save", "editor_local_compare", "editor_local_reload",
        "editor_local_close",
        "ai_targets", "ai_models", "ai_select", "ai_chat", "ai_propose", "ai_decide",
        "ssm_discovery",
        "storage_open", "storage_list_buckets", "storage_list_objects", "storage_select_local_file",
        "storage_upload", "storage_download", "storage_cancel_approval", "storage_close",
        "ai_cli_review", "ai_cli_draft",
        "ai_policy", "ai_set_policy",
    ]);
    let mut attributes = tauri_build::Attributes::new().app_manifest(app);
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        attributes = attributes.windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest());
        // Common Controls v6는 Tauri 실행 파일과 라이브러리 테스트에 필요하다.
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'");
    }
    tauri_build::try_build(attributes).expect("failed to build app permissions");
}
