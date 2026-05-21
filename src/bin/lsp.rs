//! `intent-lsp` binary: a thin shim that hands stdio to the LSP
//! server defined in [`vani::lsp`].

use std::process::ExitCode;

fn main() -> ExitCode {
    match vani::lsp::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // LSP servers are expected to log to stderr — the editor
            // captures it for the user.
            eprintln!("intent-lsp: {}", error);
            ExitCode::FAILURE
        }
    }
}
