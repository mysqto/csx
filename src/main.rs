//! Thin binary entry point that delegates into the `csx` library.

use std::process::ExitCode;

fn main() -> ExitCode {
    match csx::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("csx: {e}");
            ExitCode::FAILURE
        }
    }
}
