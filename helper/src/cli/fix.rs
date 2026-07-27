//! `pypilot fix python`: recompute the target interpreter and rebuild the venv.
//! `pypilot fix cuda`: re-pin torch/tensorflow to the build matching the driver.

use std::path::Path;

use crate::core::{install, project, solver, uv};
use crate::matrix::frameworks::Framework;
use crate::matrix::solve::solve_framework;
use crate::pypi::MetadataSource;
use crate::settings::{PackageManager, Settings};

pub async fn run<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    what: &str,
) -> crate::Result<()> {
    match what {
        "python" => fix_python(workspace, settings, source).await,
        "cuda" => fix_cuda(workspace, settings).await,
        other => anyhow::bail!(
            "unknown target `{other}`. Try `pypilot fix python` or `pypilot fix cuda`."
        ),
    }
}

async fn fix_cuda(workspace: &Path, settings: &Settings) -> crate::Result<()> {
    let deps = project::scan(workspace);
    let frameworks: Vec<_> = deps
        .packages
        .iter()
        .filter(|r| Framework::from_package_name(&r.name).is_some())
        .collect();

    if frameworks.is_empty() {
        anyhow::bail!(
            "no torch or tensorflow dependency declared here, so there is nothing to re-pin"
        );
    }

    let mut any_failed = false;
    for req in frameworks {
        let Some(solved) = solve_framework(&req.name, Some(req), settings).await else {
            continue;
        };
        if let Some(finding) = &solved.finding {
            println!("{}\n  {}\n", finding.title, finding.detail);
        }

        let Some(version) = &solved.resolved_version else {
            continue;
        };
        let spec = format!("{}=={version}", req.name);

        match settings.package_manager {
            PackageManager::Uv => {
                let uv_info = uv::ensure(settings).await?;
                let out = uv::pip_install(
                    &uv_info,
                    std::slice::from_ref(&spec),
                    solved.index_url.as_deref(),
                    workspace,
                )
                .await?;
                if out.success() {
                    println!("  ok installed {spec}");
                } else {
                    any_failed = true;
                    println!("  failed {}", out.stderr.trim());
                }
            }
            PackageManager::Pip => {
                let venv = workspace.join(".venv");
                if !venv.is_dir() {
                    any_failed = true;
                    println!("  failed no .venv in this project; run setup first");
                    continue;
                }
                let out = crate::core::pip::install_packages(
                    &venv,
                    std::slice::from_ref(&spec),
                    solved.index_url.as_deref(),
                    workspace,
                )
                .await?;
                if out.success() {
                    println!("  ok installed {spec}");
                } else {
                    any_failed = true;
                    println!("  failed {}", out.stderr.trim());
                }
            }
        }
    }

    if any_failed {
        anyhow::bail!("one or more installs did not finish, see above");
    }
    Ok(())
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
