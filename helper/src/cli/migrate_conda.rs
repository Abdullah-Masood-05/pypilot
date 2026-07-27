//! `pypilot migrate-conda`: F5's optional conda migration action.
//!
//! Writes a `pyproject.toml` from an `environment.yml`, then gets out of the
//! way: it never deletes the conda file and never touches an existing
//! `pyproject.toml`, since either would destroy something the user might
//! still need.

use std::path::Path;

use crate::core::project;

pub async fn run(workspace: &Path) -> crate::Result<()> {
    let deps = project::scan(workspace);
    if !deps.has_conda_env {
        anyhow::bail!("no environment.yml found in {}", workspace.display());
    }

    let target = workspace.join("pyproject.toml");
    if target.is_file() {
        anyhow::bail!(
            "{} already exists; migrate-conda never overwrites an existing pyproject.toml",
            target.display()
        );
    }

    let project_name = workspace
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());

    let toml = project::conda_migration_pyproject(&deps, &project_name);
    std::fs::write(&target, &toml)?;

    println!("Wrote {}", target.display());
    println!(
        "\n{} dependencies carried over as-is from environment.yml.",
        deps.packages.len()
    );
    println!(
        "Conda package names do not always match their PyPI name, and some have no PyPI \
         equivalent at all, so review the dependencies list before running `pypilot setup`."
    );
    println!("\nThe environment.yml was left in place; delete it once you're satisfied.");

    // Let a running LSP pick up the new manifest immediately.
    crate::core::rescan::notify(workspace, crate::core::rescan::Kind::Setup);
    Ok(())
}
