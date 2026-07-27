//! The actions behind F4's code actions.
//!
//! Two operations: add one package to the current environment, and rebuild the
//! environment on a different Python before adding it. Both record the package
//! in the project manifest, because an install that vanishes on the next clone
//! has not really solved the user's problem.
//!
//! Installing `torch` or `tensorflow` here also consults F2's solver (see
//! [`crate::matrix::solve`]) for an index URL matched to the machine's GPU
//! driver, so the quick fix that follows a bare `import torch` pulls the CUDA
//! build the driver can actually run rather than whatever the default index
//! happens to hand back.

use std::path::Path;

use anyhow::bail;

use crate::core::setup::{SetupSummary, Step};
use crate::core::{pip, uv};
use crate::matrix;
use crate::pypi::pyversion::PyVersion;
use crate::settings::{PackageManager, Settings};

/// Install `package` into the project's existing environment and record it.
pub async fn install_package(
    workspace: &Path,
    settings: &Settings,
    package: &str,
) -> crate::Result<SetupSummary> {
    let mut summary = SetupSummary {
        package_manager: settings.package_manager,
        python: None,
        venv_path: workspace.join(".venv"),
        why: format!("Installing {package}."),
        steps: Vec::new(),
        ok: true,
    };

    let pkgs = vec![package.to_string()];
    let has_pyproject = workspace.join("pyproject.toml").is_file();

    // Unpinned here — this path installs whatever "the package" resolves to
    // right now, so F2 solves against the newest curated release.
    let hardware = matrix::solve::solve_framework(package, None, settings).await;
    if let Some(solved) = &hardware {
        if let Some(finding) = &solved.finding {
            push(&mut summary, "GPU/CUDA check", true, &finding.detail);
        }
    }
    let index_url = hardware.as_ref().and_then(|h| h.index_url.as_deref());

    match settings.package_manager {
        PackageManager::Uv => {
            let uv_info = match uv::ensure(settings).await {
                Ok(info) => info,
                Err(e) => {
                    push(&mut summary, "ensure uv", false, &e.to_string());
                    return Ok(summary);
                }
            };

            // `uv add` writes pyproject itself; otherwise install then record.
            if has_pyproject {
                match uv::add(&uv_info, &pkgs, index_url, workspace).await {
                    Ok(out) if out.success() => {
                        push(
                            &mut summary,
                            &format!("uv add {package}"),
                            true,
                            "installed and added to pyproject.toml",
                        );
                    }
                    Ok(out) => push(
                        &mut summary,
                        &format!("uv add {package}"),
                        false,
                        out.stderr.trim(),
                    ),
                    Err(e) => push(
                        &mut summary,
                        &format!("uv add {package}"),
                        false,
                        &e.to_string(),
                    ),
                }
            } else {
                match uv::pip_install(&uv_info, &pkgs, index_url, workspace).await {
                    Ok(out) if out.success() => {
                        push(
                            &mut summary,
                            &format!("uv pip install {package}"),
                            true,
                            "installed",
                        );
                        record_in_requirements(workspace, package, &mut summary);
                    }
                    Ok(out) => push(
                        &mut summary,
                        &format!("uv pip install {package}"),
                        false,
                        out.stderr.trim(),
                    ),
                    Err(e) => push(
                        &mut summary,
                        &format!("uv pip install {package}"),
                        false,
                        &e.to_string(),
                    ),
                }
            }
        }
        PackageManager::Pip => {
            let venv = workspace.join(".venv");
            if !venv.is_dir() {
                push(
                    &mut summary,
                    "locate venv",
                    false,
                    "no .venv in this project; run setup first",
                );
                return Ok(summary);
            }
            match pip::install_packages(&venv, &pkgs, index_url, workspace).await {
                Ok(out) if out.success() => {
                    push(
                        &mut summary,
                        &format!("pip install {package}"),
                        true,
                        "installed",
                    );
                    record_in_requirements(workspace, package, &mut summary);
                }
                Ok(out) => push(
                    &mut summary,
                    &format!("pip install {package}"),
                    false,
                    out.stderr.trim(),
                ),
                Err(e) => push(
                    &mut summary,
                    &format!("pip install {package}"),
                    false,
                    &e.to_string(),
                ),
            }
        }
    }

    Ok(summary)
}

/// Rebuild the environment on `version`, then install `package` into it.
///
/// The caller has already told the user how many packages this reinstalls, so
/// this does the work without asking again.
pub async fn recreate_with_python(
    workspace: &Path,
    settings: &Settings,
    version: PyVersion,
    package: &str,
) -> crate::Result<SetupSummary> {
    let venv_path = workspace.join(".venv");
    let mut summary = SetupSummary {
        package_manager: settings.package_manager,
        python: Some(version),
        venv_path: venv_path.clone(),
        why: format!("Rebuilding the environment on Python {version} so {package} can install."),
        steps: Vec::new(),
        ok: true,
    };

    match settings.package_manager {
        PackageManager::Uv => {
            let uv_info = match uv::ensure(settings).await {
                Ok(info) => info,
                Err(e) => {
                    push(&mut summary, "ensure uv", false, &e.to_string());
                    return Ok(summary);
                }
            };

            match uv::python_install(&uv_info, version, workspace).await {
                Ok(out) if out.success() => push(
                    &mut summary,
                    &format!("uv python install {version}"),
                    true,
                    "ready",
                ),
                Ok(out) => push(
                    &mut summary,
                    &format!("uv python install {version}"),
                    false,
                    out.stderr.trim(),
                ),
                Err(e) => push(
                    &mut summary,
                    &format!("uv python install {version}"),
                    false,
                    &e.to_string(),
                ),
            }
            if !summary.ok {
                return Ok(summary);
            }

            // create_venv passes --clear, so an existing environment is replaced.
            match uv::create_venv(&uv_info, version, &venv_path, workspace).await {
                Ok(out) if out.success() => push(
                    &mut summary,
                    "uv venv",
                    true,
                    &venv_path.display().to_string(),
                ),
                Ok(out) => push(&mut summary, "uv venv", false, out.stderr.trim()),
                Err(e) => push(&mut summary, "uv venv", false, &e.to_string()),
            }
            if !summary.ok {
                return Ok(summary);
            }

            // Restore the declared dependencies, then add the new one.
            let requirements = workspace.join("requirements.txt");
            if workspace.join("pyproject.toml").is_file() {
                match uv::sync(&uv_info, workspace).await {
                    Ok(out) if out.success() => push(
                        &mut summary,
                        "uv sync",
                        true,
                        "existing dependencies restored",
                    ),
                    Ok(out) => push(&mut summary, "uv sync", false, out.stderr.trim()),
                    Err(e) => push(&mut summary, "uv sync", false, &e.to_string()),
                }
            } else if requirements.is_file() {
                match uv::pip_install_requirements(&uv_info, &requirements, workspace).await {
                    Ok(out) if out.success() => push(
                        &mut summary,
                        "reinstall requirements.txt",
                        true,
                        "existing dependencies restored",
                    ),
                    Ok(out) => push(
                        &mut summary,
                        "reinstall requirements.txt",
                        false,
                        out.stderr.trim(),
                    ),
                    Err(e) => push(
                        &mut summary,
                        "reinstall requirements.txt",
                        false,
                        &e.to_string(),
                    ),
                }
            }
            if !summary.ok {
                return Ok(summary);
            }
        }
        PackageManager::Pip => {
            // pip mode cannot fetch interpreters, so the version must be present.
            let interpreters = crate::core::interpreter::discover().await;
            let Some(interp) = pip::select_interpreter(&interpreters, version) else {
                push(
                    &mut summary,
                    "select interpreter",
                    false,
                    &format!("Python {version} is not installed. Install it, then run this again. pip mode cannot fetch interpreters."),
                );
                return Ok(summary);
            };

            if venv_path.is_dir() {
                if let Err(e) = std::fs::remove_dir_all(&venv_path) {
                    push(&mut summary, "remove old venv", false, &e.to_string());
                    return Ok(summary);
                }
            }
            match pip::create_venv(interp, &venv_path, workspace).await {
                Ok(out) if out.success() => push(
                    &mut summary,
                    "python -m venv",
                    true,
                    &venv_path.display().to_string(),
                ),
                Ok(out) => push(&mut summary, "python -m venv", false, out.stderr.trim()),
                Err(e) => push(&mut summary, "python -m venv", false, &e.to_string()),
            }
            if !summary.ok {
                return Ok(summary);
            }

            let requirements = workspace.join("requirements.txt");
            if requirements.is_file() {
                match pip::install_requirements(&venv_path, &requirements, workspace).await {
                    Ok(out) if out.success() => push(
                        &mut summary,
                        "reinstall requirements.txt",
                        true,
                        "existing dependencies restored",
                    ),
                    Ok(out) => push(
                        &mut summary,
                        "reinstall requirements.txt",
                        false,
                        out.stderr.trim(),
                    ),
                    Err(e) => push(
                        &mut summary,
                        "reinstall requirements.txt",
                        false,
                        &e.to_string(),
                    ),
                }
            }
            if !summary.ok {
                return Ok(summary);
            }
        }
    }

    // Now the package that started all this.
    let add = install_package(workspace, settings, package).await?;
    summary.steps.extend(add.steps);
    summary.ok = summary.ok && add.ok;
    Ok(summary)
}

/// Append a package to requirements.txt, creating it if needed, skipping it if
/// the name is already listed.
fn record_in_requirements(workspace: &Path, package: &str, summary: &mut SetupSummary) {
    let path = workspace.join("requirements.txt");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    let already = existing
        .lines()
        .filter_map(crate::core::project::requirement_name)
        .any(|n| n == crate::core::project::normalize_name(package));
    if already {
        return;
    }

    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(package);
    text.push('\n');

    match std::fs::write(&path, text) {
        Ok(()) => push(summary, "record in requirements.txt", true, package),
        Err(e) => push(summary, "record in requirements.txt", false, &e.to_string()),
    }
}

fn push(summary: &mut SetupSummary, name: &str, ok: bool, detail: &str) {
    if !ok {
        summary.ok = false;
    }
    summary.steps.push(Step {
        name: name.to_string(),
        ok,
        detail: detail.to_string(),
    });
}

/// Guard used by the CLI surface: refuse an empty package name rather than
/// shelling out with one.
pub fn validate_package_name(name: &str) -> crate::Result<()> {
    if name.trim().is_empty() {
        bail!("no package name given");
    }
    if name.starts_with('-') {
        bail!("`{name}` looks like a flag, not a package name");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_names_that_would_become_flags() {
        assert!(validate_package_name("mediapipe").is_ok());
        assert!(validate_package_name("").is_err());
        assert!(validate_package_name("   ").is_err());
        assert!(validate_package_name("--upgrade").is_err());
    }

    #[test]
    fn requirements_recording_is_idempotent() {
        let dir = std::env::temp_dir().join("pypilot-req-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("requirements.txt"), "numpy\n").unwrap();

        let mut summary = SetupSummary {
            package_manager: PackageManager::Uv,
            python: None,
            venv_path: dir.join(".venv"),
            why: String::new(),
            steps: Vec::new(),
            ok: true,
        };

        record_in_requirements(&dir, "mediapipe", &mut summary);
        record_in_requirements(&dir, "mediapipe", &mut summary);

        let text = std::fs::read_to_string(dir.join("requirements.txt")).unwrap();
        assert_eq!(text.matches("mediapipe").count(), 1);
        assert!(text.contains("numpy"));
    }
}
