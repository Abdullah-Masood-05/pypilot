//! `pypilot check <pkg>`: can this package run on the current environment?
//!
//! Read-only, and useful before adding a dependency rather than after the
//! install fails.

use std::path::Path;

use colored::Colorize;

use crate::core::platform::Platform;
use crate::core::{installed, probe};
use crate::pypi::MetadataSource;
use crate::settings::Settings;

pub async fn run<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    package: &str,
) -> crate::Result<()> {
    let meta = source
        .fetch(package)
        .await
        .map_err(|e| anyhow::anyhow!("PyPI has no usable metadata for `{package}`: {e}"))?;

    let analysis = meta.analyze(&Platform::current());
    let probes = probe::run(workspace, settings).await;

    println!(
        "{} {} ({})",
        "PyPilot check —".bold().cyan(),
        analysis.name.bold(),
        format!("v{}", analysis.version).cyan()
    );
    println!(
        "{}",
        "── Package Details ──────────────────────────────────────"
            .bold()
            .cyan()
    );
    println!(
        "  {:<18}: {}",
        "supported Python",
        analysis.supported.to_range_string().green().bold()
    );
    if let Some(rp) = &analysis.requires_python {
        println!("  {:<18}: {}", "requires-python", rp.to_string().green());
    }
    if analysis.sdist_only {
        println!(
            "  {:<18}: {}",
            "wheels",
            "none for this platform, will compile from source".yellow()
        );
    }

    let venv = workspace.join(".venv");
    let present = installed::scan(&venv).contains(&analysis.name);
    println!(
        "  {:<18}: {}",
        "installed",
        if present {
            "yes".green().bold().to_string()
        } else {
            "no".yellow().to_string()
        }
    );

    println!(
        "{}",
        "── Verdict ──────────────────────────────────────────────"
            .bold()
            .cyan()
    );
    match probes.venv.as_ref().and_then(|v| v.python) {
        Some(current) if analysis.supported.contains(current) => {
            println!(
                "  {} This project's Python {} can run {}.",
                "✓".green().bold(),
                current.to_string().cyan().bold(),
                analysis.name.bold()
            );
        }
        Some(current) => {
            let suggestion = analysis
                .supported
                .max()
                .map(|t| format!(" Use Python {t} instead."))
                .unwrap_or_default();
            println!(
                "  {} {} does not support this project's Python {}.{}",
                "[ERROR]".red().bold(),
                analysis.name.bold(),
                current.to_string().yellow().bold(),
                suggestion.cyan()
            );
        }
        None => {
            let suggestion = analysis
                .supported
                .max()
                .map(|t| format!(" Python {t} would suit it."))
                .unwrap_or_default();
            println!(
                "  {} This project has no virtual environment yet.{}",
                "[INFO ]".cyan().bold(),
                suggestion.cyan()
            );
        }
    }

    Ok(())
}
