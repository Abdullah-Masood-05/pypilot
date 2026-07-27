//! F4, the live import guardian.
//!
//! Given the text of a Python buffer, work out which imports will fail and why.
//! This runs on a debounce while the user types, so the ordering of the filters
//! matters: stdlib and local modules are rejected on string comparisons alone,
//! installed packages on a directory listing, and only what survives all three
//! reaches PyPI.
//!
//! [`classify`] holds the decision table and takes no I/O, so every branch is
//! testable without a network or a filesystem.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use futures::future::join_all;

use crate::core::imports::{self, ImportRef};
use crate::core::installed::Installed;
use crate::core::modules;
use crate::core::platform::Platform;
use crate::core::stdlib;
use crate::matrix;
use crate::pypi::metadata::PackageAnalysis;
use crate::pypi::pyversion::{PyVersion, PyVersionSet};
use crate::pypi::MetadataSource;

/// Why an import will not work right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The interpreter in use can run it; it just isn't installed.
    NotInstalled,
    /// A venv exists, but its Python has no wheel for this package.
    IncompatibleInterpreter {
        current: PyVersion,
        supported: PyVersionSet,
        target: Option<PyVersion>,
        /// Packages already in the venv, so the fix can be honest about scope.
        reinstall_count: usize,
    },
    /// No virtualenv yet. The target comes from every import in the file, so the
    /// first environment is built on a version that suits all of them.
    NoEnvironment { target: Option<PyVersion> },
    /// PyPI has no such project. Usually a typo, sometimes a private index.
    NotOnPyPi,
    /// The package is installed, but the attribute this file uses is not in it.
    ///
    /// This is the case metadata cannot see. mediapipe 0.10.35 installs on every
    /// CPython 3.x and then raises on `mp.solutions`, because that release
    /// dropped the legacy API. Reading the package on disk catches it at edit
    /// time rather than at run time.
    AttributeMissing {
        /// The attribute the buffer reaches for, e.g. `solutions`.
        attribute: String,
        /// Version of the package that is actually installed.
        installed_version: Option<String>,
        /// What the package does expose, to point at the replacement.
        available: Vec<String>,
    },
}

/// One actionable import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub import: ImportRef,
    /// The PyPI distribution that provides this import.
    pub package: String,
    /// True when the name came from the bundled table rather than the import
    /// itself. The UI says so, because a rename the user did not write is
    /// exactly the thing they should get to see before installing.
    pub renamed: bool,
    pub problem: Problem,
}

/// Everything [`classify`] needs to know about the environment.
#[derive(Debug, Clone, Default)]
pub struct BufferContext {
    /// Python version of the project venv, or `None` when there is no venv.
    pub venv_python: Option<PyVersion>,
    /// How many distributions are installed in that venv.
    pub installed_count: usize,
    /// Newest interpreter every unsatisfied import in this file agrees on.
    pub file_target: Option<PyVersion>,
}

/// The decision table. Pure, so each branch is directly testable.
pub fn classify(
    import: ImportRef,
    package: String,
    renamed: bool,
    analysis: Option<&PackageAnalysis>,
    ctx: &BufferContext,
) -> Finding {
    let problem = match (analysis, ctx.venv_python) {
        // Nothing on PyPI under that name. Never offer to install it.
        (None, _) => Problem::NotOnPyPi,

        // No venv yet: the fix builds one, sized for the whole file.
        (Some(_), None) => Problem::NoEnvironment {
            target: ctx.file_target,
        },

        (Some(a), Some(current)) => {
            if a.supported.contains(current) {
                Problem::NotInstalled
            } else {
                Problem::IncompatibleInterpreter {
                    current,
                    supported: a.supported.clone(),
                    target: a.supported.max(),
                    reinstall_count: ctx.installed_count,
                }
            }
        }
    };

    Finding {
        import,
        package,
        renamed,
        problem,
    }
}

/// Analyze a buffer and return everything worth a diagnostic.
///
/// `workspace` is used to recognize the project's own modules, which must never
/// be reported as missing packages. `platform` decides which wheels count, and
/// is a parameter rather than a lookup so the answer is reproducible off the
/// machine that produced it.
pub async fn analyze_buffer<S: MetadataSource>(
    source: &str,
    workspace: &Path,
    platform: Platform,
    venv_python: Option<PyVersion>,
    installed: &Installed,
    metadata: &S,
) -> Vec<Finding> {
    let local = local_modules(workspace);

    // Keep every occurrence for the squiggles, but resolve each package once.
    let candidates: Vec<(ImportRef, String, bool)> = imports::extract(source)
        .into_iter()
        .filter(|i| !stdlib::is_stdlib(&i.module) && !local.contains(&i.module))
        .map(|i| {
            let package = matrix::resolve_package(&i.module);
            let renamed = matrix::is_mapped(&i.module);
            (i, package, renamed)
        })
        .filter(|(_, package, _)| !installed.contains(package))
        .collect();

    if candidates.is_empty() {
        return Vec::new();
    }

    let unique: BTreeSet<String> = candidates.iter().map(|(_, p, _)| p.clone()).collect();

    let fetched = join_all(unique.into_iter().map(|name| async move {
        let analysis = metadata
            .fetch(&name)
            .await
            .ok()
            .map(|meta| meta.analyze(&platform));
        (name, analysis)
    }))
    .await;

    let analyses: HashMap<String, Option<PackageAnalysis>> = fetched.into_iter().collect();

    // With no venv, the first environment should suit every import in the file,
    // so intersect them all rather than sizing for whichever one is on screen.
    let file_target = analyses
        .values()
        .flatten()
        .map(|a| a.supported.clone())
        .reduce(|acc, s| acc.intersect(&s))
        .and_then(|set| set.max());

    let ctx = BufferContext {
        venv_python,
        installed_count: installed.count(),
        file_target,
    };

    candidates
        .into_iter()
        .map(|(import, package, renamed)| {
            let analysis = analyses.get(&package).and_then(|a| a.as_ref());
            classify(import, package, renamed, analysis, &ctx)
        })
        .collect()
}

/// Check attribute accesses against what the installed packages actually expose.
///
/// Runs only over imports that are installed, since an uninstalled package has
/// nothing on disk to read. Purely local: a directory listing and one file read
/// per package, no network.
pub fn check_attributes(source: &str, venv: &Path, installed: &Installed) -> Vec<Finding> {
    let mut out = Vec::new();

    for import in imports::extract(source) {
        if stdlib::is_stdlib(&import.module) {
            continue;
        }
        // Only `import x` / `import x as y` bind the package root.
        let Some(binding) = import.binding.clone() else {
            continue;
        };
        let package = matrix::resolve_package(&import.module);
        if !installed.contains(&package) {
            continue;
        }
        let Some(package_dir) = modules::find_package(venv, &import.module) else {
            continue;
        };

        let uses = imports::extract_attr_uses(source, &[binding]);
        if uses.is_empty() {
            continue;
        }

        let installed_version = installed.version_of(&package);
        let mut reported: HashSet<String> = HashSet::new();

        for used in uses {
            if !reported.insert(used.attr.clone()) {
                continue; // one diagnostic per attribute, not per occurrence
            }
            let modules::AttrStatus::Missing { available } =
                modules::attribute_status(&package_dir, &used.attr)
            else {
                continue;
            };

            out.push(Finding {
                import: ImportRef {
                    module: import.module.clone(),
                    binding: import.binding.clone(),
                    line: used.line,
                    start: used.start,
                    end: used.end,
                },
                package: package.clone(),
                renamed: false,
                problem: Problem::AttributeMissing {
                    attribute: used.attr,
                    installed_version: installed_version.clone(),
                    available,
                },
            });
        }
    }

    out
}

/// Module names that belong to this project, so `import utils` next to
/// `utils.py` never gets reported as a missing PyPI package.
fn local_modules(workspace: &Path) -> HashSet<String> {
    let mut names = HashSet::new();
    for root in [workspace.to_path_buf(), workspace.join("src")] {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.join("__init__.py").is_file() {
                    if let Some(n) = path.file_name() {
                        names.insert(n.to_string_lossy().into_owned());
                    }
                }
            } else if path.extension().is_some_and(|e| e == "py") {
                if let Some(stem) = path.file_stem() {
                    names.insert(stem.to_string_lossy().into_owned());
                }
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn import_of(module: &str) -> ImportRef {
        ImportRef {
            module: module.into(),
            binding: Some(module.into()),
            line: 0,
            start: 7,
            end: 7 + module.len() as u32,
        }
    }

    #[test]
    fn compatible_but_missing_offers_a_plain_install() {
        let ctx = BufferContext {
            venv_python: Some(PyVersion::py3(12)),
            installed_count: 3,
            file_target: Some(PyVersion::py3(12)),
        };
        let f = classify(
            import_of("mediapipe"),
            "mediapipe".into(),
            false,
            Some(&analysis("mediapipe", &[9, 10, 11, 12])),
            &ctx,
        );
        assert_eq!(f.problem, Problem::NotInstalled);
    }

    #[test]
    fn incompatible_interpreter_reports_target_and_reinstall_scope() {
        // The week-two case: 3.13 venv with packages in it, new import caps at 3.12.
        let ctx = BufferContext {
            venv_python: Some(PyVersion::py3(13)),
            installed_count: 3,
            file_target: Some(PyVersion::py3(12)),
        };
        let f = classify(
            import_of("mediapipe"),
            "mediapipe".into(),
            false,
            Some(&analysis("mediapipe", &[9, 10, 11, 12])),
            &ctx,
        );
        match f.problem {
            Problem::IncompatibleInterpreter {
                current,
                target,
                reinstall_count,
                ref supported,
            } => {
                assert_eq!(current, PyVersion::py3(13));
                assert_eq!(target, Some(PyVersion::py3(12)));
                assert_eq!(reinstall_count, 3);
                assert_eq!(supported.to_range_string(), "3.9–3.12");
            }
            other => panic!("expected an interpreter conflict, got {other:?}"),
        }
    }

    #[test]
    fn no_venv_sizes_the_environment_for_the_whole_file() {
        let ctx = BufferContext {
            venv_python: None,
            installed_count: 0,
            file_target: Some(PyVersion::py3(12)),
        };
        let f = classify(
            import_of("mediapipe"),
            "mediapipe".into(),
            false,
            Some(&analysis("mediapipe", &[9, 10, 11, 12])),
            &ctx,
        );
        assert_eq!(
            f.problem,
            Problem::NoEnvironment {
                target: Some(PyVersion::py3(12))
            }
        );
    }

    #[test]
    fn unknown_project_never_becomes_an_install_offer() {
        let ctx = BufferContext {
            venv_python: Some(PyVersion::py3(12)),
            ..Default::default()
        };
        let f = classify(import_of("mediapip"), "mediapip".into(), false, None, &ctx);
        assert_eq!(f.problem, Problem::NotOnPyPi);
    }
}
