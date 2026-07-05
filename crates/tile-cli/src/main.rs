//! The `tile` binary entrypoint.

use std::process::ExitCode;

fn main() -> ExitCode {
    match tile_cli::app::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("tile: {err}");
            ExitCode::FAILURE
        }
    }
}
