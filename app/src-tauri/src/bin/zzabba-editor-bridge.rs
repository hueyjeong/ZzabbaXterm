#[cfg(target_os = "linux")]
#[path = "../editor/nano_wire.rs"]
mod nano_wire;
#[cfg(target_os = "linux")]
#[path = "../editor/nano_node.rs"]
mod nano_node;
#[cfg(target_os = "linux")]
#[path = "../editor/publication.rs"]
mod publication;
#[cfg(target_os = "linux")]
#[path = "../editor/unix.rs"]
mod os;
#[cfg(target_os = "linux")]
#[path = "../editor/nano_publish.rs"]
mod nano_publish;

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
