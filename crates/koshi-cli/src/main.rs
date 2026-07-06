//! The `koshi` binary entrypoint.

use std::process::ExitCode;

fn main() -> ExitCode {
    match koshi_cli::app::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("koshi: {err}");
            ExitCode::FAILURE
        }
    }
}
