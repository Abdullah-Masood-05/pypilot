//! `pypilot fix python`: recompute the target interpreter and rebuild the venv.
//!
//! `pypilot fix cuda` belongs to the hardware matrix, which is not built yet, so
//! it says so rather than pretending.

use std::path::Path;

use crate::core::{install, solver};
use crate::pypi::MetadataSource;
use crate::settings::Settings;

pub async fn run<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    what: &str,
) -> crate::Result<()> {
    match what {
        "python" => fix_python(workspace, settings, source).await,
        "cuda" => {
            anyhow::bail!(
                "`fix cuda` needs the hardware matrix, which is not implemented yet. \
                 `pypilot doctor` reports everything that currently works."
            )
        }
        other => anyhow::bail!("unknown target `{other}`. Try `pypilot fix python`."),
    }
}

async fn fix_python<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
) -> crate::Result<()> {
    let assessment = solver::assess(workspace, settings, source).await;

    let Some(compat) = &assessment.compat else {
        anyhow::bail!("no declared dependencies here, so there is no version to solve for");
    };
    if compat.intersection.is_empty() {
        let why = compat
            .conflicts
            .first()
            .map(|c| c.explanation.clone())
            .unwrap_or_else(|| "the dependencies share no supported Python version".to_string());
        anyhow::bail!("{why}");
    }
    let Some(target) = assessment.target_python else {
        anyhow::bail!("could not determine a target Python version");
    };

    let current = assessment.probes.venv.as_ref().and_then(|v| v.python);
    if current == Some(target) {
        println!("Already on Python {target}. Nothing to do.");
        return Ok(());
    }

    let installed_count = assessment
        .probes
        .venv
        .as_ref()
        .map(|v| crate::core::installed::scan(&v.path).count())
        .unwrap_or(0);

    println!(
        "Rebuilding on Python {target}. Dependencies support {}.",
        compat.intersection.to_range_string()
    );
    if installed_count > 0 {
        println!("{installed_count} installed packages will be reinstalled.");
    }

    // Rebuild and restore the declared dependencies. There is no single new
    // package here, so reuse the bootstrap rather than the add path.
    let summary = crate::core::setup::run(workspace, settings, source).await?;
    for step in &summary.steps {
        println!(
            "  {} {} {}",
            if step.ok { "ok" } else { "failed" },
            step.name,
            step.detail
        );
    }

    if summary.ok {
        println!("\nEnvironment now runs Python {target}.");
        Ok(())
    } else {
        anyhow::bail!("the rebuild did not finish, see the steps above")
    }
}

/// Install one package from the terminal, sharing the code action's path.
pub async fn install_one(
    workspace: &Path,
    settings: &Settings,
    package: &str,
) -> crate::Result<()> {
    install::validate_package_name(package)?;
    let summary = install::install_package(workspace, settings, package).await?;
    for step in &summary.steps {
        println!(
            "  {} {} {}",
            if step.ok { "ok" } else { "failed" },
            step.name,
            step.detail
        );
    }
    if summary.ok {
        Ok(())
    } else {
        anyhow::bail!("install did not finish")
    }
}
