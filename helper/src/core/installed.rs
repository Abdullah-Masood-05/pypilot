//! What is already installed in a project's virtualenv.
//!
//! Read straight off disk from the `.dist-info` directories rather than by
//! running `pip list`. F4 consults this on every debounce, and a subprocess per
//! keystroke pause would blow the latency budget. It also works when the venv
//! has no pip in it, which is the normal case for environments uv creates.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::core::project::normalize_name;

/// Packages present in a virtualenv, normalized per PEP 503.
#[derive(Debug, Clone, Default)]
pub struct Installed {
    pub packages: BTreeSet<String>,
    /// Installed version per package, when the metadata directory records one.
    pub versions: BTreeMap<String, String>,
}

impl Installed {
    /// Is this distribution installed? `name` is normalized before comparison.
    pub fn contains(&self, name: &str) -> bool {
        self.packages.contains(&normalize_name(name))
    }

    pub fn count(&self) -> usize {
        self.packages.len()
    }

    /// The installed version of a distribution, if known.
    pub fn version_of(&self, name: &str) -> Option<String> {
        self.versions.get(&normalize_name(name)).cloned()
    }
}

/// Read the installed distributions from a venv directory.
///
/// Returns an empty set if the venv is missing or unreadable, which callers
/// treat the same as "nothing installed".
pub fn scan(venv: &Path) -> Installed {
    let mut packages = BTreeSet::new();
    let mut versions = BTreeMap::new();
    for dir in site_packages_dirs(venv) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(dist) = distribution_name(&name) {
                if let Some(v) = distribution_version(&name) {
                    versions.insert(dist.clone(), v);
                }
                packages.insert(dist);
            }
        }
    }
    Installed { packages, versions }
}

/// The version out of a `.dist-info` directory name, if it has one.
fn distribution_version(entry: &str) -> Option<String> {
    let stem = entry
        .strip_suffix(".dist-info")
        .or_else(|| entry.strip_suffix(".egg-info"))?;
    let (_, version) = stem.split_once('-')?;
    if version.is_empty() {
        return None;
    }
    Some(version.to_string())
}

/// Candidate `site-packages` locations for a venv, across platforms.
///
/// Windows uses `Lib/site-packages`; POSIX nests under a version directory that
/// we discover rather than guess, since the venv's Python version is exactly
/// what F4 may be about to change.
pub fn site_packages_dirs(venv: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    let windows = venv.join("Lib").join("site-packages");
    if windows.is_dir() {
        dirs.push(windows);
    }

    let lib = venv.join("lib");
    if let Ok(entries) = std::fs::read_dir(&lib) {
        for entry in entries.flatten() {
            let candidate = entry.path().join("site-packages");
            if candidate.is_dir() {
                dirs.push(candidate);
            }
        }
    }
    dirs
}

/// Pull the distribution name out of a `.dist-info` or `.egg-info` directory.
///
/// `charset_normalizer-3.3.2.dist-info` -> `charset-normalizer`.
fn distribution_name(entry: &str) -> Option<String> {
    let stem = entry
        .strip_suffix(".dist-info")
        .or_else(|| entry.strip_suffix(".egg-info"))?;
    // Everything before the first `-` is the name; the rest is the version.
    let name = stem.split('-').next()?;
    if name.is_empty() {
        return None;
    }
    Some(normalize_name(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dist_info_names() {
        assert_eq!(
            distribution_name("charset_normalizer-3.3.2.dist-info").as_deref(),
            Some("charset-normalizer")
        );
        assert_eq!(
            distribution_name("opencv_python-5.0.0.93.dist-info").as_deref(),
            Some("opencv-python")
        );
        assert_eq!(
            distribution_name("numpy-2.0.1.egg-info").as_deref(),
            Some("numpy")
        );
    }

    #[test]
    fn ignores_non_metadata_entries() {
        assert!(distribution_name("numpy").is_none());
        assert!(distribution_name("__pycache__").is_none());
        assert!(distribution_name("numpy.libs").is_none());
    }

    #[test]
    fn lookup_normalizes_the_query() {
        let installed = Installed {
            packages: ["opencv-python".to_string()].into_iter().collect(),
            ..Default::default()
        };
        assert!(installed.contains("opencv_python"));
        assert!(installed.contains("OpenCV-Python"));
        assert!(!installed.contains("opencv"));
    }

    #[test]
    fn missing_venv_scans_empty() {
        let empty = scan(Path::new("definitely/not/a/real/venv"));
        assert_eq!(empty.count(), 0);
    }
}
