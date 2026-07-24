//! The constraint solver — turns probes + project + PyPI metadata into findings.
//!
//! [`assess`] is the single read-only entry point used by both `doctor` and F5.
//! The decision logic lives in the pure [`synthesize`] function so it can be unit
//! tested without spawning any processes. Every finding states the *why* — which
//! package constrained the Python choice.

use std::path::Path;

use crate::core::platform::Platform;
use crate::core::probe::Probes;
use crate::core::project::ProjectDeps;
use crate::core::{Assessment, Finding, FixKind, Severity};
use crate::matrix;
use crate::pypi::pyversion::PyVersion;
use crate::pypi::{self, CompatReport, MetadataSource};
use crate::settings::Settings;

/// Flattened, process-free view of the environment for [`synthesize`].
#[derive(Debug, Clone, Default)]
pub struct EnvSummary {
    pub has_venv: bool,
    pub venv_python: Option<PyVersion>,
    pub uv_present: bool,
    pub interpreters: Vec<PyVersion>,
}

impl EnvSummary {
    fn from_probes(p: &Probes) -> EnvSummary {
        EnvSummary {
            has_venv: p.venv.is_some(),
            venv_python: p.venv.as_ref().and_then(|v| v.python),
            uv_present: p.uv.is_some(),
            interpreters: p.interpreters.iter().map(|i| i.version).collect(),
        }
    }
}

/// Full read-only assessment of a workspace (F5 / doctor).
pub async fn assess<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
) -> Assessment {
    let probes = crate::core::probe::run(workspace, settings).await;
    let project = crate::core::project::scan(workspace);

    let compat = if project.packages.is_empty() {
        None
    } else {
        Some(pypi::analyze(source, Platform::current(), &project.packages).await)
    };

    // F2 seam (empty in Phase 1).
    let hardware = matrix::check(workspace, settings, &project.packages).await;

    let env = EnvSummary::from_probes(&probes);
    let (target_python, mut findings) = synthesize(&env, &project, compat.as_ref());
    findings.extend(hardware.findings);

    Assessment {
        probes,
        project,
        compat,
        target_python,
        findings,
    }
}

/// Pure decision logic: given a flattened environment, the project, and the
/// compatibility report, produce the target interpreter and the findings.
pub fn synthesize(
    env: &EnvSummary,
    project: &ProjectDeps,
    compat: Option<&CompatReport>,
) -> (Option<PyVersion>, Vec<Finding>) {
    let mut findings = Vec::new();

    // Not a Python project at all → stay entirely silent.
    if !project.is_python_project() {
        return (None, findings);
    }

    // Conda projects: detect + offer migration, don't fight conda in v1.
    if project.has_conda_env {
        findings.push(Finding {
            severity: Severity::Info,
            title: "Conda environment detected".to_string(),
            detail: "environment.yml found. PyPilot can translate it to a uv/pyproject setup, but won't manage conda directly in this version.".to_string(),
            fix: FixKind::Manual,
        });
    }

    let Some(compat) = compat else {
        // Python files but no declared dependencies to check.
        if !env.has_venv {
            findings.push(Finding {
                severity: Severity::Warning,
                title: "No virtual environment".to_string(),
                detail: "This project has Python files but no .venv. Create one to isolate its dependencies.".to_string(),
                fix: FixKind::SetupEnvironment,
            });
        }
        return (None, findings);
    };

    // Surface unresolved dependency names (typos, private packages).
    for (name, err) in &compat.unresolved {
        findings.push(Finding {
            severity: Severity::Warning,
            title: format!("Could not resolve `{name}`"),
            detail: format!(
                "PyPI lookup failed for `{name}` ({err}). It may be a typo or a private package."
            ),
            fix: FixKind::Manual,
        });
    }

    // sdist-only compile warnings.
    for w in &compat.sdist_warnings {
        findings.push(Finding {
            severity: Severity::Warning,
            title: "Will compile from source".to_string(),
            detail: w.clone(),
            fix: FixKind::Manual,
        });
    }

    // Empty intersection → hard conflict; can't be auto-fixed by choosing a version.
    if compat.intersection.is_empty() {
        let detail = compat
            .conflicts
            .first()
            .map(|c| c.explanation.clone())
            .unwrap_or_else(|| {
                "The project's dependencies have no common supported Python version.".to_string()
            });
        findings.push(Finding {
            severity: Severity::Error,
            title: "Dependencies conflict on Python version".to_string(),
            detail,
            fix: FixKind::Manual,
        });
        return (None, findings);
    }

    let target = compat.suggested_python();

    // Determine the interpreter currently in effect: the venv's, else nothing.
    match env.venv_python {
        // Venv exists and is compatible → all good (stay silent).
        Some(current) if compat.is_compatible(current) => {}

        // Venv exists but its Python is unsupported → recreate on the right version.
        Some(current) => {
            let why = incompat_reason(compat, current, target);
            findings.push(Finding {
                severity: Severity::Error,
                title: format!("Python {current} is not supported by this project"),
                detail: why,
                fix: target
                    .map(FixKind::RecreateWithPython)
                    .unwrap_or(FixKind::Manual),
            });
        }

        // No venv (or unknown version) → set one up on the correct interpreter.
        None => {
            let range = compat.intersection.to_range_string();
            let why = match target {
                Some(t) => format!(
                    "Dependencies support Python {range}. Recommended: {t}{}.",
                    capping_clause(compat)
                ),
                None => format!("Dependencies support Python {range}."),
            };
            findings.push(Finding {
                severity: Severity::Warning,
                title: "No environment set up yet".to_string(),
                detail: why,
                fix: FixKind::SetupEnvironment,
            });
        }
    }

    (target, findings)
}

/// Explain why the current interpreter is unsupported, naming the blockers.
fn incompat_reason(compat: &CompatReport, current: PyVersion, target: Option<PyVersion>) -> String {
    let blockers = compat.blockers_for(current);
    let names: Vec<String> = blockers
        .iter()
        .map(|p| format!("{} (supports {})", p.name, p.supported.to_range_string()))
        .collect();
    let who = if names.is_empty() {
        "some dependencies".to_string()
    } else {
        names.join(", ")
    };
    match target {
        Some(t) => format!(
            "{who} do not support Python {current}. Recreate the environment with Python {t}."
        ),
        None => format!("{who} do not support Python {current}."),
    }
}

/// "(constrained by X, which tops out at 3.Y)" for the recommendation message.
fn capping_clause(compat: &CompatReport) -> String {
    let Some(cap) = compat.intersection.max() else {
        return String::new();
    };
    // The package whose upper bound equals the intersection's cap is the limiter.
    let limiter = compat
        .per_package
        .iter()
        .filter(|p| p.supported.max() == Some(cap))
        .min_by_key(|p| p.supported.len());
    match limiter {
        Some(p) => format!(
            " (upper bound set by {}, which supports {})",
            p.name,
            p.supported.to_range_string()
        ),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pypi::metadata::PackageAnalysis;
    use crate::pypi::pyversion::PyVersionSet;

    fn analysis(name: &str, minors: &[u8]) -> PackageAnalysis {
        PackageAnalysis {
            name: name.into(),
            version: "1.0".into(),
            supported: PyVersionSet::from_versions(minors.iter().map(|&m| PyVersion::py3(m))),
            has_platform_wheels: true,
            sdist_only: false,
            requires_python: None,
        }
    }

    fn report(pkgs: Vec<PackageAnalysis>) -> CompatReport {
        let intersection = pkgs
            .iter()
            .map(|p| p.supported.clone())
            .reduce(|a, b| a.intersect(&b))
            .unwrap_or_else(PyVersionSet::universe);
        CompatReport {
            platform: Platform::current(),
            per_package: pkgs,
            unresolved: vec![],
            intersection,
            conflicts: vec![],
            sdist_warnings: vec![],
        }
    }

    fn python_project() -> ProjectDeps {
        ProjectDeps {
            sources: vec!["requirements.txt".into()],
            packages: vec!["mediapipe".into()],
            ..Default::default()
        }
    }

    #[test]
    fn mediapipe_on_313_recommends_312() {
        // mediapipe supports 3.9–3.12; venv is 3.13.
        let compat = report(vec![analysis("mediapipe", &[9, 10, 11, 12])]);
        let env = EnvSummary {
            has_venv: true,
            venv_python: Some(PyVersion::py3(13)),
            uv_present: true,
            interpreters: vec![PyVersion::py3(13)],
        };
        let (target, findings) = synthesize(&env, &python_project(), Some(&compat));
        assert_eq!(target, Some(PyVersion::py3(12)));
        let f = findings
            .iter()
            .find(|f| f.severity == Severity::Error)
            .unwrap();
        assert_eq!(f.fix, FixKind::RecreateWithPython(PyVersion::py3(12)));
        assert!(f.detail.contains("mediapipe"));
        assert!(f.detail.contains("3.13"));
    }

    #[test]
    fn compatible_venv_stays_silent() {
        let compat = report(vec![analysis("numpy", &[9, 10, 11, 12, 13])]);
        let env = EnvSummary {
            has_venv: true,
            venv_python: Some(PyVersion::py3(12)),
            uv_present: true,
            interpreters: vec![PyVersion::py3(12)],
        };
        let (_t, findings) = synthesize(&env, &python_project(), Some(&compat));
        assert!(findings.iter().all(|f| f.severity == Severity::Info));
    }

    #[test]
    fn no_venv_suggests_setup_with_reason() {
        let compat = report(vec![analysis("mediapipe", &[9, 10, 11, 12])]);
        let env = EnvSummary::default();
        let (target, findings) = synthesize(&env, &python_project(), Some(&compat));
        assert_eq!(target, Some(PyVersion::py3(12)));
        let f = &findings[0];
        assert_eq!(f.fix, FixKind::SetupEnvironment);
        assert!(f.detail.contains("mediapipe"));
    }

    #[test]
    fn not_a_python_project_is_silent() {
        let (target, findings) = synthesize(&EnvSummary::default(), &ProjectDeps::default(), None);
        assert!(target.is_none());
        assert!(findings.is_empty());
    }
}
