use std::process::ExitCode;

use zzabbaxterm_lib::editor_cli::{CliError, run};

fn main() -> ExitCode {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build();
    let result = match runtime {
        Ok(runtime) => runtime.block_on(run(std::env::args_os().skip(1))),
        Err(error) => Err(CliError::Transport(error)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(if matches!(error, CliError::Usage) {
                2
            } else {
                1
            })
        }
    }
}
