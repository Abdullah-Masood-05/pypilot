//! Turning guardian findings into editor diagnostics and quick fixes.
//!
//! Each diagnostic carries its own fix in the LSP `data` field, so when the
//! editor asks for code actions we rebuild them from the diagnostic under the
//! cursor instead of re-analyzing the buffer.
//!
//! Every install action names the distribution it will install. When an import
//! resolves to a different name (`cv2` to `opencv-python`) the title says both,
//! because the user is approving a name they never typed.

use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::*;

use crate::core::guardian::{Finding, Problem};

/// Command IDs. The install commands take arguments; the F5 ones do not.
pub const CMD_INSTALL: &str = "pypilot.installPackage";
pub const CMD_RECREATE: &str = "pypilot.recreateWithPython";

/// The fix attached to a diagnostic, round-tripped through `Diagnostic.data`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixData {
    pub package: String,
    pub action: FixAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FixAction {
    /// Install into the environment that already exists.
    Install,
    /// Build the project's first environment on `python`, then install.
    Create { python: String },
    /// Replace the existing environment with one on `python`, then install.
    Recreate {
        python: String,
        reinstall_count: usize,
    },
    /// Nothing safe to automate.
    None,
}

/// Build the diagnostic for one finding.
pub fn to_diagnostic(finding: &Finding) -> Diagnostic {
    let provides = if finding.renamed {
        format!(" (provides `{}`)", finding.import.module)
    } else {
        String::new()
    };

    let (severity, message, action) = match &finding.problem {
        Problem::NotInstalled => (
            DiagnosticSeverity::WARNING,
            format!("`{}`{provides} is not installed.", finding.package),
            FixAction::Install,
        ),

        Problem::IncompatibleInterpreter {
            current,
            supported,
            target,
            reinstall_count,
        } => {
            let message = format!(
                "`{}`{provides} has no wheels for Python {current}. It supports {}.",
                finding.package,
                supported.to_range_string()
            );
            let action = match target {
                Some(t) => FixAction::Recreate {
                    python: t.to_string(),
                    reinstall_count: *reinstall_count,
                },
                None => FixAction::None,
            };
            (DiagnosticSeverity::ERROR, message, action)
        }

        Problem::NoEnvironment { target } => {
            let message = format!(
                "`{}`{provides} is not installed, and this project has no virtual environment yet.",
                finding.package
            );
            let action = match target {
                Some(t) => FixAction::Create {
                    python: t.to_string(),
                },
                None => FixAction::None,
            };
            (DiagnosticSeverity::WARNING, message, action)
        }

        // Deliberately actionless. Offering to install a name PyPI does not know
        // is how a typo becomes a supply chain incident.
        Problem::NotOnPyPi => (
            DiagnosticSeverity::WARNING,
            format!(
                "PyPI has no project named `{}`. Check the spelling, or it may come from a private index.",
                finding.package
            ),
            FixAction::None,
        ),
    };

    let data = serde_json::to_value(FixData {
        package: finding.package.clone(),
        action,
    })
    .ok();

    Diagnostic {
        range: Range {
            start: Position {
                line: finding.import.line,
                character: finding.import.start,
            },
            end: Position {
                line: finding.import.line,
                character: finding.import.end,
            },
        },
        severity: Some(severity),
        source: Some("pypilot".to_string()),
        message,
        data,
        ..Default::default()
    }
}

/// Rebuild quick fixes from the diagnostics the editor sent back.
pub fn code_actions(diagnostics: &[Diagnostic]) -> Vec<CodeActionOrCommand> {
    let mut actions = Vec::new();

    for diagnostic in diagnostics {
        // Only ours carry a FixData payload.
        if diagnostic.source.as_deref() != Some("pypilot") {
            continue;
        }
        let Some(fix) = diagnostic
            .data
            .clone()
            .and_then(|d| serde_json::from_value::<FixData>(d).ok())
        else {
            continue;
        };

        let (title, command) = match &fix.action {
            FixAction::Install => (
                format!("Install `{}`", fix.package),
                Command {
                    title: format!("Install {}", fix.package),
                    command: CMD_INSTALL.to_string(),
                    arguments: Some(vec![serde_json::Value::String(fix.package.clone())]),
                },
            ),

            FixAction::Create { python } => (
                format!(
                    "Create environment on Python {python} and install `{}`",
                    fix.package
                ),
                Command {
                    title: "Create environment".to_string(),
                    command: CMD_RECREATE.to_string(),
                    arguments: Some(vec![
                        serde_json::Value::String(python.clone()),
                        serde_json::Value::String(fix.package.clone()),
                    ]),
                },
            ),

            FixAction::Recreate {
                python,
                reinstall_count,
            } => {
                // Say how much gets rebuilt. "3 packages will be reinstalled" is
                // the difference between a trusted fix and a surprise.
                let scope = match reinstall_count {
                    0 => String::new(),
                    1 => " (1 existing package will be reinstalled)".to_string(),
                    n => format!(" ({n} existing packages will be reinstalled)"),
                };
                (
                    format!(
                        "Rebuild environment on Python {python} and install `{}`{scope}",
                        fix.package
                    ),
                    Command {
                        title: "Rebuild environment".to_string(),
                        command: CMD_RECREATE.to_string(),
                        arguments: Some(vec![
                            serde_json::Value::String(python.clone()),
                            serde_json::Value::String(fix.package.clone()),
                        ]),
                    },
                )
            }

            FixAction::None => continue,
        };

        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title,
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![diagnostic.clone()]),
            command: Some(command),
            is_preferred: Some(true),
            ..Default::default()
        }));
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::imports::ImportRef;
    use crate::pypi::pyversion::{PyVersion, PyVersionSet};

    fn finding(problem: Problem, package: &str, renamed: bool) -> Finding {
        Finding {
            import: ImportRef {
                module: "cv2".into(),
                line: 2,
                start: 7,
                end: 10,
            },
            package: package.into(),
            renamed,
            problem,
        }
    }

    fn supported(minors: &[u8]) -> PyVersionSet {
        PyVersionSet::from_versions(minors.iter().map(|&m| PyVersion::py3(m)))
    }

    #[test]
    fn missing_package_is_a_warning_with_an_install_fix() {
        let d = to_diagnostic(&finding(Problem::NotInstalled, "mediapipe", false));
        assert_eq!(d.severity, Some(DiagnosticSeverity::WARNING));
        assert!(d.message.contains("mediapipe"));

        let actions = code_actions(&[d]);
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn renamed_imports_show_both_names() {
        let d = to_diagnostic(&finding(Problem::NotInstalled, "opencv-python", true));
        assert!(d.message.contains("opencv-python"));
        assert!(d.message.contains("cv2"), "must reveal the resolved rename");

        let CodeActionOrCommand::CodeAction(action) = &code_actions(&[d])[0] else {
            panic!("expected a code action");
        };
        assert!(action.title.contains("opencv-python"));
    }

    #[test]
    fn interpreter_conflict_is_an_error_naming_the_range() {
        let d = to_diagnostic(&finding(
            Problem::IncompatibleInterpreter {
                current: PyVersion::py3(13),
                supported: supported(&[9, 10, 11, 12]),
                target: Some(PyVersion::py3(12)),
                reinstall_count: 3,
            },
            "mediapipe",
            false,
        ));
        assert_eq!(d.severity, Some(DiagnosticSeverity::ERROR));
        assert!(d.message.contains("3.13"));
        assert!(d.message.contains("3.9–3.12"));

        let CodeActionOrCommand::CodeAction(action) = &code_actions(&[d])[0] else {
            panic!("expected a code action");
        };
        assert!(action.title.contains("Python 3.12"));
        assert!(
            action
                .title
                .contains("3 existing packages will be reinstalled"),
            "the fix must state its blast radius, got: {}",
            action.title
        );
    }

    #[test]
    fn fresh_project_creates_rather_than_rebuilds() {
        let d = to_diagnostic(&finding(
            Problem::NoEnvironment {
                target: Some(PyVersion::py3(12)),
            },
            "mediapipe",
            false,
        ));
        let CodeActionOrCommand::CodeAction(action) = &code_actions(&[d])[0] else {
            panic!("expected a code action");
        };
        // There is nothing to rebuild or reinstall when no venv exists.
        assert!(
            action.title.starts_with("Create environment"),
            "{}",
            action.title
        );
        assert!(!action.title.contains("reinstalled"));
    }

    #[test]
    fn unknown_projects_get_no_install_action() {
        let d = to_diagnostic(&finding(Problem::NotOnPyPi, "mediapip", false));
        assert!(
            code_actions(&[d]).is_empty(),
            "never offer to install a name PyPI does not have"
        );
    }

    #[test]
    fn foreign_diagnostics_are_ignored() {
        let other = Diagnostic {
            source: Some("ruff".to_string()),
            message: "unused import".to_string(),
            ..Default::default()
        };
        assert!(code_actions(&[other]).is_empty());
    }
}
