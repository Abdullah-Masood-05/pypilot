//! F1 — environment bootstrap. The one flow behind both `pypilot setup` and the
//! LSP "Fix everything" command, so the two can never drift.
//!
//! uv mode (default): ensure uv → `uv python install X.Y` → `uv venv --python X.Y`
//! → `uv sync` (pyproject) or `uv pip install -r requirements.txt`.
//!
//! pip mode: pick an already-installed interpreter for X.Y → `python -m venv` →
//! `pip install`. If the required Python isn't installed, we stop with a clear
//! "install Python X.Y" message (pip mode can't fetch interpreters).

use std::path::{Path, PathBuf};

use crate::core::solver;
use crate::core::{pip, uv};
use crate::pypi::pyversion::PyVersion;
use crate::pypi::MetadataSource;
use crate::settings::{PackageManager, ResolvedMode, Settings};

/// One executed step, for a human-readable summary.
#[derive(Debug, Clone)]
pub struct Step {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// Result of a bootstrap run.
#[derive(Debug, Clone)]
pub struct SetupSummary {
    pub package_manager: PackageManager,
    pub resolved_mode: ResolvedMode,
    pub python: Option<PyVersion>,
    pub venv_path: PathBuf,
    pub why: String,
    pub steps: Vec<Step>,
    /// Overall success (all critical steps ok).
    pub ok: bool,
}

impl SetupSummary {
    fn failed(&self) -> bool {
        !self.ok
    }
}

/// Run the full bootstrap for `workspace`, honoring `package_manager` and resolved mode.
pub async fn run<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
) -> crate::Result<SetupSummary> {
    let mode = settings.resolve_mode().await;
    run_with_mode(workspace, settings, source, mode).await
}

/// Run bootstrap for `workspace` with an explicit resolved mode (e.g. from user prompt).
pub async fn run_with_mode<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
    mode: ResolvedMode,
) -> crate::Result<SetupSummary> {
    let assessment = solver::assess(workspace, settings, source).await;

    let venv_path = workspace.join(".venv");
    let mut summary = SetupSummary {
        package_manager: settings.package_manager,
        resolved_mode: mode,
        python: assessment.target_python,
        venv_path: venv_path.clone(),
        why: derive_why(&assessment),
        steps: Vec::new(),
        ok: true,
    };

    // Refuse when dependencies fundamentally conflict — there's no interpreter to
    // pick, so installing anything would just fail confusingly.
    if let Some(compat) = &assessment.compat {
        if compat.intersection.is_empty() && !compat.per_package.is_empty() {
            summary.ok = false;
            summary.steps.push(Step {
                name: "resolve Python version".into(),
                ok: false,
                detail: compat
                    .conflicts
                    .first()
                    .map(|c| c.explanation.clone())
                    .unwrap_or_else(|| "dependencies have no common supported Python".into()),
            });
            return Ok(summary);
        }
    }

    match mode {
        ResolvedMode::UvFull => {
            run_uv_full(workspace, settings, &assessment, &venv_path, &mut summary).await
        }
        ResolvedMode::UvPipCompat => {
            run_uv_pip_compat(workspace, settings, &assessment, &venv_path, &mut summary).await
        }
        ResolvedMode::PurePip => run_pip(workspace, &assessment, &venv_path, &mut summary).await,
    }

    Ok(summary)
}

async fn run_uv_full(
    workspace: &Path,
    settings: &Settings,
    assessment: &crate::core::Assessment,
    venv_path: &Path,
    summary: &mut SetupSummary,
) {
    let uv_info = match uv::ensure(settings).await {
        Ok(info) => {
            summary.steps.push(Step {
                name: "ensure uv".into(),
                ok: true,
                detail: format!(
                    "uv {} ({})",
                    info.version,
                    if info.managed { "managed" } else { "on PATH" }
                ),
            });
            info
        }
        Err(e) => {
            fail(summary, "ensure uv", &e.to_string());
            return;
        }
    };

    // Scaffold pyproject.toml when the project has none yet so that subsequent
    // `uv add` calls have a manifest to write into. We do this before creating
    // the venv because `uv init` itself emits a sensible initial venv via the
    // pyproject it creates. Existing pyproject.toml files are never touched.
    let has_pyproject = workspace.join("pyproject.toml").is_file();
    if !has_pyproject {
        match uv::init(&uv_info, workspace).await {
            Ok(out) if out.success() => step_ok(summary, "uv init", "pyproject.toml created"),
            Ok(out) => {
                // Non-zero but not fatal: the directory may already have a
                // hello.py that uv init refuses to overwrite. Proceed and let
                // the downstream sync/add surface a clearer error if needed.
                step_ok(
                    summary,
                    "uv init",
                    &format!("(warning) {}", out.stderr.trim()),
                );
            }
            Err(e) => fail(summary, "uv init", &e.to_string()),
        }
        if summary.failed() {
            return;
        }
    }

    // Install the target Python if one was determined.
    if let Some(py) = summary.python {
        match uv::python_install(&uv_info, py, workspace).await {
            Ok(out) if out.success() => {
                step_ok(summary, &format!("uv python install {py}"), "ready")
            }
            Ok(out) => fail(
                summary,
                &format!("uv python install {py}"),
                out.stderr.trim(),
            ),
            Err(e) => fail(summary, &format!("uv python install {py}"), &e.to_string()),
        }
        if summary.failed() {
            return;
        }

        match uv::create_venv(&uv_info, py, venv_path, workspace).await {
            Ok(out) if out.success() => {
                step_ok(summary, "uv venv", &venv_path.display().to_string())
            }
            Ok(out) => fail(summary, "uv venv", out.stderr.trim()),
            Err(e) => fail(summary, "uv venv", &e.to_string()),
        }
        if summary.failed() {
            return;
        }
    }

    install_deps_uv(workspace, &uv_info, assessment, summary).await;
}

async fn install_deps_uv(
    workspace: &Path,
    uv_info: &uv::UvInfo,
    assessment: &crate::core::Assessment,
    summary: &mut SetupSummary,
) {
    // After run_uv may have called `uv init`, re-check for pyproject.toml.
    let has_pyproject = workspace.join("pyproject.toml").is_file();
    let requirements = workspace.join("requirements.txt");

    if has_pyproject && requirements.is_file() {
        // If requirements.txt exists and pyproject.toml exists (whether newly created by
        // `uv init` or pre-existing), add and resolve dependencies using `uv add -r requirements.txt`.
        match uv::add_requirements(uv_info, &requirements, None, workspace).await {
            Ok(out) if out.success() => step_ok(
                summary,
                "uv add -r requirements.txt",
                "dependencies added to pyproject.toml and installed",
            ),
            Ok(out) => fail(summary, "uv add -r requirements.txt", out.stderr.trim()),
            Err(e) => fail(summary, "uv add -r requirements.txt", &e.to_string()),
        }
    } else if has_pyproject {
        // Use `uv sync` which reads pyproject.toml / uv.lock and installs the
        // declared dependencies.
        match uv::sync(uv_info, workspace).await {
            Ok(out) if out.success() => step_ok(summary, "uv sync", "dependencies installed"),
            Ok(out) => fail(summary, "uv sync", out.stderr.trim()),
            Err(e) => fail(summary, "uv sync", &e.to_string()),
        }
    } else if requirements.is_file() {
        // No pyproject.toml (init must have been skipped or failed): fall back
        // to the plain requirements install.
        match uv::pip_install_requirements(uv_info, &requirements, workspace).await {
            Ok(out) if out.success() => {
                step_ok(summary, "uv pip install -r requirements.txt", "installed")
            }
            Ok(out) => fail(
                summary,
                "uv pip install -r requirements.txt",
                out.stderr.trim(),
            ),
            Err(e) => fail(
                summary,
                "uv pip install -r requirements.txt",
                &e.to_string(),
            ),
        }
    } else {
        step_ok(
            summary,
            "install dependencies",
            "no manifest to install from",
        );
    }
    let _ = assessment;
}

async fn run_uv_pip_compat(
    workspace: &Path,
    settings: &Settings,
    assessment: &crate::core::Assessment,
    venv_path: &Path,
    summary: &mut SetupSummary,
) {
    let uv_info = match uv::ensure(settings).await {
        Ok(info) => {
            summary.steps.push(Step {
                name: "ensure uv".into(),
                ok: true,
                detail: format!(
                    "uv {} ({})",
                    info.version,
                    if info.managed { "managed" } else { "on PATH" }
                ),
            });
            info
        }
        Err(e) => {
            fail(summary, "ensure uv", &e.to_string());
            return;
        }
    };

    // In pip-compatible mode: NEVER call uv init, NEVER create pyproject.toml / uv.lock.
    // Use uv venv + uv pip install + uv pip freeze into requirements.txt.
    let target_py = summary
        .python
        .or_else(|| assessment.probes.interpreters.first().map(|i| i.version));

    if let Some(py) = target_py {
        match uv::python_install(&uv_info, py, workspace).await {
            Ok(out) if out.success() => {
                step_ok(summary, &format!("uv python install {py}"), "ready")
            }
            Ok(out) => fail(
                summary,
                &format!("uv python install {py}"),
                out.stderr.trim(),
            ),
            Err(e) => fail(summary, &format!("uv python install {py}"), &e.to_string()),
        }
        if summary.failed() {
            return;
        }

        match uv::create_venv(&uv_info, py, venv_path, workspace).await {
            Ok(out) if out.success() => {
                step_ok(summary, "uv venv", &venv_path.display().to_string())
            }
            Ok(out) => fail(summary, "uv venv", out.stderr.trim()),
            Err(e) => fail(summary, "uv venv", &e.to_string()),
        }
        if summary.failed() {
            return;
        }
    }

    let requirements = workspace.join("requirements.txt");
    if requirements.is_file() {
        match uv::pip_install_requirements(&uv_info, &requirements, workspace).await {
            Ok(out) if out.success() => {
                step_ok(summary, "uv pip install -r requirements.txt", "installed");
                freeze_into_requirements_uv(&uv_info, workspace, summary).await;
            }
            Ok(out) => fail(
                summary,
                "uv pip install -r requirements.txt",
                out.stderr.trim(),
            ),
            Err(e) => fail(
                summary,
                "uv pip install -r requirements.txt",
                &e.to_string(),
            ),
        }
    } else if !assessment.project.packages.is_empty() {
        match uv::pip_install(&uv_info, &assessment.project.names(), None, workspace).await {
            Ok(out) if out.success() => {
                step_ok(summary, "uv pip install", "installed");
                freeze_into_requirements_uv(&uv_info, workspace, summary).await;
            }
            Ok(out) => fail(summary, "uv pip install", out.stderr.trim()),
            Err(e) => fail(summary, "uv pip install", &e.to_string()),
        }
    } else {
        step_ok(
            summary,
            "install dependencies",
            "no manifest to install from",
        );
    }
}

/// Run `uv pip freeze > requirements.txt` and record the step.
pub(crate) async fn freeze_into_requirements_uv(
    uv: &uv::UvInfo,
    workspace: &Path,
    summary: &mut SetupSummary,
) {
    match uv::pip_freeze(uv, workspace).await {
        Ok(out) if out.success() => {
            let path = workspace.join("requirements.txt");
            if let Err(e) = std::fs::write(&path, &out.stdout) {
                fail(summary, "uv pip freeze > requirements.txt", &e.to_string());
            } else {
                step_ok(
                    summary,
                    "uv pip freeze > requirements.txt",
                    "requirements.txt updated with exact pins",
                );
            }
        }
        Ok(out) => fail(
            summary,
            "uv pip freeze > requirements.txt",
            out.stderr.trim(),
        ),
        Err(e) => fail(summary, "uv pip freeze > requirements.txt", &e.to_string()),
    }
}

async fn run_pip(
    workspace: &Path,
    assessment: &crate::core::Assessment,
    venv_path: &Path,
    summary: &mut SetupSummary,
) {
    // pip mode requires the target Python to already exist on the machine.
    let target = summary.python;
    let interp = match target {
        Some(py) => match pip::select_interpreter(&assessment.probes.interpreters, py) {
            Some(i) => i.clone(),
            None => {
                fail(
                    summary,
                    "select interpreter",
                    &format!(
                        "Python {py} is required but not installed. Install it (e.g. from python.org or your package manager), then run setup again. pip mode cannot fetch interpreters."
                    ),
                );
                return;
            }
        },
        None => {
            // No dep-driven target: use the newest interpreter available.
            match assessment.probes.interpreters.first() {
                Some(i) => i.clone(),
                None => {
                    fail(
                        summary,
                        "select interpreter",
                        "no Python interpreter found on this machine",
                    );
                    return;
                }
            }
        }
    };
    step_ok(
        summary,
        "select interpreter",
        &format!("{} ({})", interp.command, interp.version),
    );

    match pip::create_venv(&interp, venv_path, workspace).await {
        Ok(out) if out.success() => {
            step_ok(summary, "python -m venv", &venv_path.display().to_string())
        }
        Ok(out) => fail(summary, "python -m venv", out.stderr.trim()),
        Err(e) => fail(summary, "python -m venv", &e.to_string()),
    }
    if summary.failed() {
        return;
    }

    let requirements = workspace.join("requirements.txt");
    if requirements.is_file() {
        match pip::install_requirements(venv_path, &requirements, workspace).await {
            Ok(out) if out.success() => {
                step_ok(summary, "pip install -r requirements.txt", "installed");
                // Freeze immediately: requirements.txt now reflects exact pins.
                freeze_into_requirements(venv_path, workspace, summary).await;
            }
            Ok(out) => fail(
                summary,
                "pip install -r requirements.txt",
                out.stderr.trim(),
            ),
            Err(e) => fail(summary, "pip install -r requirements.txt", &e.to_string()),
        }
    } else if !assessment.project.packages.is_empty() {
        match pip::install_packages(venv_path, &assessment.project.names(), None, workspace).await {
            Ok(out) if out.success() => {
                step_ok(summary, "pip install", "installed");
                // Freeze immediately: requirements.txt now reflects exact pins.
                freeze_into_requirements(venv_path, workspace, summary).await;
            }
            Ok(out) => fail(summary, "pip install", out.stderr.trim()),
            Err(e) => fail(summary, "pip install", &e.to_string()),
        }
    } else {
        step_ok(
            summary,
            "install dependencies",
            "no manifest to install from",
        );
    }
}

/// Run `pip freeze > requirements.txt` inside `venv` and record the step.
async fn freeze_into_requirements(venv: &Path, workspace: &Path, summary: &mut SetupSummary) {
    match pip::freeze_requirements(venv, workspace).await {
        Ok(out) if out.success() => step_ok(
            summary,
            "pip freeze > requirements.txt",
            "requirements.txt updated with exact pins",
        ),
        Ok(out) => fail(summary, "pip freeze > requirements.txt", out.stderr.trim()),
        Err(e) => fail(summary, "pip freeze > requirements.txt", &e.to_string()),
    }
}

fn derive_why(assessment: &crate::core::Assessment) -> String {
    match (&assessment.compat, assessment.target_python) {
        (Some(compat), Some(py)) => {
            let range = compat.intersection.to_range_string();
            let limiter = compat
                .per_package
                .iter()
                .filter(|p| p.supported.max() == Some(py))
                .min_by_key(|p| p.supported.len());
            match limiter {
                Some(p) => format!(
                    "Python {py} chosen: dependencies support {range}; {} caps the upper bound (supports {}).",
                    p.name,
                    p.supported.to_range_string()
                ),
                None => format!("Python {py} chosen: dependencies support {range}."),
            }
        }
        _ => "No dependency constraints; using an available interpreter.".to_string(),
    }
}

fn step_ok(summary: &mut SetupSummary, name: &str, detail: &str) {
    summary.steps.push(Step {
        name: name.to_string(),
        ok: true,
        detail: detail.to_string(),
    });
}

fn fail(summary: &mut SetupSummary, name: &str, detail: &str) {
    summary.ok = false;
    summary.steps.push(Step {
        name: name.to_string(),
        ok: false,
        detail: detail.to_string(),
    });
}
