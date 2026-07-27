//! What an installed package actually provides.
//!
//! Packaging metadata answers "will this install". It says nothing about
//! whether the code you wrote will run against what got installed. mediapipe
//! 0.10.35 installs cleanly on any CPython 3.x and then fails at runtime with
//! `module 'mediapipe' has no attribute 'solutions'`, because that release
//! dropped the legacy API in favour of `tasks`.
//!
//! That is decidable without a curated table: look at the package on disk. A
//! top-level attribute is either a submodule sitting next to `__init__.py`, or
//! a name that `__init__.py` binds. If it is neither, the attribute access will
//! raise.
//!
//! The one thing that defeats this is PEP 562 lazy loading, where a module
//! defines `__getattr__` and materializes attributes on demand. Packages doing
//! that report [`AttrStatus::Unknown`] and are left alone, because guessing
//! there would produce false errors on correct code.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::core::installed;

/// Whether a package exposes a given top-level attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrStatus {
    /// Found as a submodule or as a name bound in `__init__.py`.
    Present,
    /// Neither, so accessing it raises. Carries what the package does provide,
    /// which is usually enough to point at the replacement API.
    Missing { available: Vec<String> },
    /// The package resolves attributes dynamically, so nothing can be inferred.
    Unknown,
}

/// Locate an installed package's directory inside a venv.
///
/// Only regular packages are handled: a directory with an `__init__.py`. Single
/// module distributions and namespace packages return `None`, which callers
/// treat as "cannot inspect".
pub fn find_package(venv: &Path, import_name: &str) -> Option<PathBuf> {
    for site in installed::site_packages_dirs(venv) {
        let candidate = site.join(import_name);
        if candidate.join("__init__.py").is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Does `package_dir` expose `attr` as a top-level attribute?
pub fn attribute_status(package_dir: &Path, attr: &str) -> AttrStatus {
    if is_submodule(package_dir, attr) {
        return AttrStatus::Present;
    }

    let init = package_dir.join("__init__.py");
    let Ok(source) = std::fs::read_to_string(&init) else {
        return AttrStatus::Unknown;
    };

    // PEP 562: the package can conjure attributes on access.
    if defines_dynamic_getattr(&source) {
        return AttrStatus::Unknown;
    }

    if bound_names(&source).contains(attr) {
        return AttrStatus::Present;
    }

    AttrStatus::Missing {
        available: available_attributes(package_dir, &source),
    }
}

/// A sibling of `__init__.py` that Python would import as a submodule.
fn is_submodule(package_dir: &Path, name: &str) -> bool {
    if package_dir.join(name).join("__init__.py").is_file() {
        return true;
    }
    // Source modules and compiled extensions alike.
    ["py", "pyi", "pyd", "so"]
        .iter()
        .any(|ext| package_dir.join(format!("{name}.{ext}")).is_file())
}

/// Does the module define `__getattr__`, making attributes unknowable statically?
fn defines_dynamic_getattr(source: &str) -> bool {
    source.lines().any(|line| {
        let l = line.trim_start();
        l.starts_with("def __getattr__") || l.starts_with("__getattr__ =")
    })
}

/// Top-level names `__init__.py` binds.
///
/// A line scanner, matching the rest of the crate: imports, assignments,
/// definitions, and `__all__` entries. Only column-zero statements count, so a
/// name bound inside a function body is not mistaken for a module attribute.
fn bound_names(source: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut in_all = false;

    for raw in source.lines() {
        // `__all__` often spans several lines; keep collecting until the bracket
        // closes.
        if in_all {
            collect_quoted(raw, &mut names);
            if raw.contains(']') {
                in_all = false;
            }
            continue;
        }

        let line = match raw.find('#') {
            Some(i) => &raw[..i],
            None => raw,
        };
        // Indented code binds locals, not module attributes.
        if line.starts_with([' ', '\t']) || line.trim().is_empty() {
            continue;
        }
        let line = line.trim_end();

        if line.starts_with("__all__") {
            collect_quoted(line, &mut names);
            if !line.contains(']') {
                in_all = true;
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("from ") {
            // `from x import a, b as c` binds a and c.
            if let Some((_, imported)) = rest.split_once(" import ") {
                for part in imported.trim_start_matches('(').split(',') {
                    if let Some(name) = binding_of(part) {
                        names.insert(name);
                    }
                }
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("import ") {
            // `import a.b as c` binds c; plain `import a.b` binds a.
            for part in rest.split(',') {
                let Some(name) = binding_of(part) else {
                    continue;
                };
                names.insert(name.split('.').next().unwrap_or(name.as_str()).to_string());
            }
            continue;
        }

        for keyword in ["def ", "class ", "async def "] {
            if let Some(rest) = line.strip_prefix(keyword) {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    names.insert(name);
                }
            }
        }

        // Module-level assignment, excluding comparisons and augmented forms.
        if let Some((lhs, _)) = line.split_once('=') {
            if !lhs.ends_with(['=', '!', '<', '>', '+', '-', '*', '/']) {
                for target in lhs.split(',') {
                    let name = target.split(':').next().unwrap_or(target).trim();
                    if is_identifier(name) {
                        names.insert(name.to_string());
                    }
                }
            }
        }
    }

    names
}

/// `a as b` binds b; a bare `a` binds a.
fn binding_of(fragment: &str) -> Option<String> {
    let cleaned = fragment.trim().trim_matches(|c| c == '(' || c == ')');
    let token = match cleaned.split_once(" as ") {
        Some((_, alias)) => alias.trim(),
        None => cleaned,
    };
    let token = token.trim();
    if token.is_empty() || token == "*" {
        return None;
    }
    Some(token.to_string())
}

/// Everything the package exposes, for the "did you mean" half of the message.
fn available_attributes(package_dir: &Path, init_source: &str) -> Vec<String> {
    let mut names = bound_names(init_source);

    if let Ok(entries) = std::fs::read_dir(package_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let raw = entry.file_name();
            let raw = raw.to_string_lossy();
            if raw.starts_with('_') {
                continue;
            }
            if path.is_dir() {
                if path.join("__init__.py").is_file() {
                    names.insert(raw.into_owned());
                }
            } else if path.extension().is_some_and(|e| e == "py") {
                if let Some(stem) = path.file_stem() {
                    names.insert(stem.to_string_lossy().into_owned());
                }
            }
        }
    }

    names.retain(|n| !n.starts_with('_'));
    names.into_iter().collect()
}

fn collect_quoted(line: &str, out: &mut BTreeSet<String>) {
    let mut rest = line;
    while let Some(open) = rest.find(['"', '\'']) {
        let quote = rest.as_bytes()[open] as char;
        let after = &rest[open + 1..];
        let Some(close) = after.find(quote) else {
            break;
        };
        let name = &after[..close];
        if is_identifier(name) {
            out.insert(name.to_string());
        }
        rest = &after[close + 1..];
    }
}

fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !s.chars().next().is_some_and(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake installed package to inspect.
    fn package(tag: &str, init: &str, submodules: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("pypilot-modules-{tag}"))
            .join("pkg");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("__init__.py"), init).unwrap();
        for m in submodules {
            let sub = dir.join(m);
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join("__init__.py"), "").unwrap();
        }
        dir
    }

    /// The real shape of mediapipe 0.10.35: tasks and modules on disk, a few
    /// names re-exported, and no `solutions` anywhere.
    fn mediapipe_0_10_35(tag: &str) -> PathBuf {
        package(
            tag,
            "import mediapipe.tasks.python as tasks\n\
             from mediapipe.tasks.python.vision.core.image import Image\n\
             from mediapipe.tasks.python.vision.core.image import ImageFormat\n",
            &["tasks", "modules"],
        )
    }

    #[test]
    fn removed_api_is_reported_missing() {
        // Each test gets its own directory: they run concurrently and the
        // helper wipes the tree before writing it.
        let dir = mediapipe_0_10_35("removed");
        match attribute_status(&dir, "solutions") {
            AttrStatus::Missing { available } => {
                assert!(available.contains(&"tasks".to_string()));
                assert!(
                    available.contains(&"Image".to_string()),
                    "re-exported names count as available: {available:?}"
                );
            }
            other => panic!("expected solutions to be missing, got {other:?}"),
        }
    }

    #[test]
    fn submodules_and_reexports_are_present() {
        let dir = mediapipe_0_10_35("present");
        // On disk as a directory.
        assert_eq!(attribute_status(&dir, "tasks"), AttrStatus::Present);
        // Only bound in __init__.py, never a submodule.
        assert_eq!(attribute_status(&dir, "Image"), AttrStatus::Present);
    }

    #[test]
    fn lazy_packages_are_left_alone() {
        // PEP 562 means the attribute may exist despite nothing on disk.
        let dir = package("lazy", "def __getattr__(name):\n    return 1\n", &[]);
        assert_eq!(attribute_status(&dir, "anything"), AttrStatus::Unknown);
    }

    #[test]
    fn recognizes_the_ways_a_name_gets_bound() {
        let names = bound_names(
            "import os\n\
             import a.b as alias\n\
             from x import y, z as w\n\
             CONST = 1\n\
             typed: int = 2\n\
             def fn():\n\
             \x20   local = 3\n\
             class Klass:\n\
             \x20   pass\n",
        );
        for expected in ["os", "alias", "y", "w", "CONST", "typed", "fn", "Klass"] {
            assert!(names.contains(expected), "missing {expected}: {names:?}");
        }
        assert!(
            !names.contains("local"),
            "names bound inside a function are not module attributes"
        );
    }

    #[test]
    fn dunder_all_entries_count() {
        let names = bound_names("__all__ = [\n    \"alpha\",\n    \"beta\",\n]\n");
        assert!(names.contains("alpha"));
        assert!(names.contains("beta"));
    }

    #[test]
    fn star_import_binds_nothing_specific() {
        // Cannot know what came in, so nothing is claimed.
        let names = bound_names("from x import *\n");
        assert!(names.is_empty(), "got {names:?}");
    }
}
