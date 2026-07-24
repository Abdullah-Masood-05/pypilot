//! CLI frontends — thin wrappers over the engine.
//!
//! `doctor` is read-only (assess + print); `setup` mutates (bootstrap + print).
//! `check`/`fix` are reserved for later phases and print a friendly stub so the
//! command surface (F6) is already discoverable.

mod doctor;
mod setup;

use std::path::PathBuf;

use crate::pypi::PyPiClient;
use crate::settings::Settings;

/// Dispatch a CLI mode. `mode` is one of doctor|setup|check|fix.
pub async fn dispatch(mode: &str, args: &[String]) -> crate::Result<()> {
    let workspace = workspace_from_args(args);
    let settings = Settings::load(&workspace);

    match mode {
        "doctor" => {
            let source = PyPiClient::new();
            doctor::run(&workspace, &settings, &source).await
        }
        "setup" => {
            let source = PyPiClient::new();
            setup::run(&workspace, &settings, &source).await
        }
        "check" => {
            println!("pypilot check <pkg> — coming in Phase 2 (F4 live guardian).");
            Ok(())
        }
        "fix" => {
            println!("pypilot fix <python|cuda> — coming in Phase 2/3.");
            Ok(())
        }
        other => anyhow::bail!("unknown CLI mode `{other}`"),
    }
}

/// Resolve the workspace directory from `--path <dir>` or default to CWD.
fn workspace_from_args(args: &[String]) -> PathBuf {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--path" || a == "-p" {
            if let Some(dir) = iter.next() {
                return PathBuf::from(dir);
            }
        } else if let Some(dir) = a.strip_prefix("--path=") {
            return PathBuf::from(dir);
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}
