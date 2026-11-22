#[cfg(target_os = "linux")]
#[path = "../editor/nano_wire.rs"]
mod nano_wire;
#[cfg(target_os = "linux")]
#[path = "../editor/nano_node.rs"]
mod nano_node;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "linux")]
    match nano_node::run() {
        Ok(()) => return std::process::ExitCode::SUCCESS,
        Err(error) => eprintln!("zzabba-editor-bridge: {error}"),
    }
    #[cfg(not(target_os = "linux"))]
    eprintln!("zzabba-editor-bridge requires Linux");
    std::process::ExitCode::FAILURE
}
