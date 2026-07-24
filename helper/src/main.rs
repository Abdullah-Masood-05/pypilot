//! PyPilot helper entrypoint — mode dispatch.
//!
//! `pypilot <mode>`:
//!   * `doctor` — read-only probe report (nothing is executed/installed).
//!   * `setup`  — full F1 bootstrap, honoring the `package_manager` setting.
//!   * `lsp`    — stdio LSP server (the mode Zed launches).
//!   * `check` / `fix` — reserved for later phases; print a friendly stub.
//!
//! Arg parsing is hand-rolled (no shell involvement anywhere) so every downstream
//! spawned command uses arg vectors, never a shell string.

use std::process::ExitCode;

use pypilot::{cli, lsp};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "doctor".to_string());
    let rest: Vec<String> = args.collect();

    match mode.as_str() {
        "lsp" => {
            // The LSP server owns its own multi-threaded runtime.
            lsp::run_stdio();
            ExitCode::SUCCESS
        }
        "doctor" | "setup" | "check" | "fix" => {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("pypilot: failed to start async runtime: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let code = rt.block_on(cli::dispatch(&mode, &rest));
            match code {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("pypilot {mode}: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
        "--version" | "-V" | "version" => {
            println!("pypilot {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "--help" | "-h" | "help" => {
            print_help();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("pypilot: unknown mode `{other}`\n");
            print_help();
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    println!(
        "pypilot {ver} — deterministic Python environment doctor\n\n\
         USAGE:\n    pypilot <MODE> [--path <dir>]\n\n\
         MODES:\n\
         \x20   doctor    Read-only environment + compatibility report\n\
         \x20   setup     Bootstrap the environment (uv by default, pip if configured)\n\
         \x20   lsp       Run as an LSP server over stdio (used by the Zed extension)\n\
         \x20   check     [phase 2] Compatibility report for one package\n\
         \x20   fix       [phase 2] Recompute + recreate the environment\n\n\
         OPTIONS:\n\
         \x20   --path <dir>   Workspace directory to analyze (default: current dir)\n",
        ver = env!("CARGO_PKG_VERSION")
    );
}
