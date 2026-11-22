fn main() {
    let app = tauri_build::AppManifest::new().commands(&[
        "terminal_open", "terminal_start", "terminal_input", "terminal_resize", "terminal_ack",
        "terminal_close", "workspace_window",
        "connection_test_start", "connection_test_wait", "connection_test_cancel", "connection_save",
        "connection_ssh_test_attach",
        "connections_load",
        "verify_ssh_host", "commit_ssh_host", "cancel_ssh_host",
        "editor_local_open", "editor_local_save", "editor_local_compare", "editor_local_reload",
        "editor_local_close",
        "ai_targets", "ai_models", "ai_select", "ai_chat", "ai_propose", "ai_decide",
        "ai_cancel_proposal",
        "ai_decisions",
        "ssm_discovery",
        "ai_cli_review", "ai_cli_draft",
        "ai_policy", "ai_set_policy",
    ]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(app))
        .expect("failed to build app permissions");
}
