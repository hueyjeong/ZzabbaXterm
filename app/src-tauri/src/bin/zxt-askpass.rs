#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    #[cfg(any(windows, target_os = "linux"))]
    if zxt_lib::local_auth::run_askpass() {
        return std::process::ExitCode::SUCCESS;
    }
    std::process::ExitCode::FAILURE
}
