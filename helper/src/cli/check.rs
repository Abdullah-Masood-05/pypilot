//! `pypilot check <pkg>`: can this package run on the current environment?
//!
//! Read-only, and useful before adding a dependency rather than after the
//! install fails.

use std::path::Path;

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

    println!("{} {}", analysis.name, analysis.version);
    println!(
        "  supported Python : {}",
        analysis.supported.to_range_string()
    );
    if let Some(rp) = &analysis.requires_python {
        println!("  requires_python  : {rp}");
    }
    if analysis.sdist_only {
        println!("  wheels           : none for this platform, so it compiles from source");
    }

    let venv = workspace.join(".venv");
    let present = installed::scan(&venv).contains(&analysis.name);
    println!(
        "  installed        : {}",
        if present { "yes" } else { "no" }
    );

    match probes.venv.as_ref().and_then(|v| v.python) {
        Some(current) if analysis.supported.contains(current) => {
            println!(
                "\nThis project's Python {current} can run {}.",
                analysis.name
            );
        }
        Some(current) => {
            let suggestion = analysis
                .supported
                .max()
                .map(|t| format!(" Use Python {t} instead."))
                .unwrap_or_default();
            println!(
                "\n{} does not support this project's Python {current}.{suggestion}",
                analysis.name
            );
        }
        None => {
            let suggestion = analysis
                .supported
                .max()
                .map(|t| format!(" Python {t} would suit it."))
                .unwrap_or_default();
            println!("\nThis project has no virtual environment yet.{suggestion}");
        }
    }

    Ok(())
}
