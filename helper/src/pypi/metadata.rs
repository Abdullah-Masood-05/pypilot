//! PyPI JSON metadata model and per-package Python-support analysis.
//!
//! We deserialize only the fields we need from `https://pypi.org/pypi/<name>/json`:
//! the `info` block (version + `requires_python`) and `urls` (the distribution
//! files for that latest version). The full `releases` map is huge and we don't
//! need it, so it is skipped entirely.

use serde::{Deserialize, Serialize};

use crate::core::platform::Platform;
use crate::pypi::pyversion::{parse_requires_python, PyVersionSet};
use crate::pypi::wheel::WheelTags;

/// The subset of a PyPI package JSON document PyPilot consumes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageMetadata {
    pub info: Info,
    /// Distribution files for `info.version` (the latest release).
    #[serde(default)]
    pub urls: Vec<DistFile>,
    /// Every published version, needed to resolve a pinned or capped
    /// dependency to the release an installer would actually choose.
    ///
    /// PyPI sends this as a `releases` object whose values are per-version file
    /// lists. Those lists are large and we only want the keys, so they are
    /// discarded while parsing. Serializing writes a plain array, which is why
    /// the deserializer accepts both shapes: the array is what comes back out
    /// of our own cache.
    #[serde(
        rename = "releases",
        default,
        deserialize_with = "deserialize_version_list"
    )]
    pub versions: Vec<String>,
}

/// Accept either PyPI's `{version: [files]}` object or our cached `[version]`
/// array, keeping only the version strings.
fn deserialize_version_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct VersionList;

    impl<'de> serde::de::Visitor<'de> for VersionList {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a releases object or an array of version strings")
        }

        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: serde::de::MapAccess<'de>,
        {
            let mut out = Vec::new();
            while let Some(key) = map.next_key::<String>()? {
                // Skip the file list without allocating it.
                map.next_value::<serde::de::IgnoredAny>()?;
                out.push(key);
            }
            Ok(out)
        }

        fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
        where
            S: serde::de::SeqAccess<'de>,
        {
            let mut out = Vec::new();
            while let Some(v) = seq.next_element::<String>()? {
                out.push(v);
            }
            Ok(out)
        }
    }

    deserializer.deserialize_any(VersionList)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Info {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub requires_python: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistFile {
    pub filename: String,
    /// "bdist_wheel" | "sdist" | ...
    #[serde(default)]
    pub packagetype: String,
    /// Per-file `requires_python`, occasionally set independently of `info`.
    #[serde(default)]
    pub requires_python: Option<String>,
    #[serde(default)]
    pub yanked: bool,
}

impl DistFile {
    pub fn is_wheel(&self) -> bool {
        self.packagetype == "bdist_wheel" || self.filename.ends_with(".whl")
    }

    pub fn is_sdist(&self) -> bool {
        self.packagetype == "sdist"
            || self.filename.ends_with(".tar.gz")
            || self.filename.ends_with(".zip")
    }
}

/// Result of analyzing one package's latest release against a platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageAnalysis {
    pub name: String,
    pub version: String,
    /// Python minors this package can actually be installed on, here.
    pub supported: PyVersionSet,
    /// True if the package ships any wheel for this platform.
    pub has_platform_wheels: bool,
    /// True if only an sdist is available (would compile from source).
    pub sdist_only: bool,
    /// The `requires_python` string that was in effect (for messaging).
    pub requires_python: Option<String>,
}

impl PackageMetadata {
    /// Compute the set of Python versions this package supports on `platform`.
    ///
    /// Logic:
    /// * Start from `requires_python` (or the full universe if absent).
    /// * If the package ships platform wheels, intersect with the union of those
    ///   wheels' supported minors — this is what catches the "no cp313 wheel" case.
    /// * If there are no platform wheels but an sdist exists, keep the
    ///   `requires_python` set and flag `sdist_only` (compiles from source).
    pub fn analyze(&self, platform: &Platform) -> PackageAnalysis {
        let declared = self
            .info
            .requires_python
            .as_deref()
            .map(parse_requires_python)
            .unwrap_or_else(PyVersionSet::universe);

        let mut wheel_union = PyVersionSet::empty();
        let mut has_platform_wheels = false;
        let mut has_any_wheel = false;
        let mut has_sdist = false;

        for file in &self.urls {
            if file.yanked {
                continue;
            }
            if file.is_wheel() {
                has_any_wheel = true;
                if let Some(tags) = WheelTags::parse(&file.filename) {
                    let s = tags.supported_on(platform);
                    if !s.is_empty() {
                        has_platform_wheels = true;
                        wheel_union = wheel_union.union(&s);
                    }
                }
            } else if file.is_sdist() {
                has_sdist = true;
            }
        }

        let supported = if has_platform_wheels {
            declared.intersect(&wheel_union)
        } else {
            // No usable wheels: either sdist-only (compile) or nothing. Either way
            // the interpreter constraint is whatever requires_python says.
            declared
        };

        let sdist_only = !has_platform_wheels && has_sdist;
        // If a package has wheels but none for this platform and no sdist, that's
        // still surfaced via has_platform_wheels=false + empty wheel_union, but we
        // fall back to requires_python so we don't wrongly claim zero support.
        let _ = has_any_wheel;

        PackageAnalysis {
            name: self.info.name.clone(),
            version: self.info.version.clone(),
            supported,
            has_platform_wheels,
            sdist_only,
            requires_python: self.info.requires_python.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::platform::{Arch, Os};
    use crate::pypi::pyversion::PyVersion;

    fn linux() -> Platform {
        Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        }
    }

    fn wheel(name: &str) -> DistFile {
        DistFile {
            filename: name.to_string(),
            packagetype: "bdist_wheel".to_string(),
            requires_python: None,
            yanked: false,
        }
    }

    #[test]
    fn wheel_tags_narrow_requires_python() {
        // requires_python loose (>=3.8) but wheels only up to cp312.
        let meta = PackageMetadata {
            info: Info {
                name: "mediapipe".into(),
                version: "0.10.9".into(),
                requires_python: Some(">=3.8".into()),
            },
            urls: vec![
                wheel("mediapipe-0.10.9-cp39-cp39-manylinux_2_17_x86_64.whl"),
                wheel("mediapipe-0.10.9-cp312-cp312-manylinux_2_17_x86_64.whl"),
            ],
            versions: vec!["0.10.9".into()],
        };
        let a = meta.analyze(&linux());
        assert!(a.has_platform_wheels);
        assert!(a.supported.contains(PyVersion::py3(9)));
        assert!(a.supported.contains(PyVersion::py3(12)));
        assert!(!a.supported.contains(PyVersion::py3(13)));
    }

    #[test]
    fn sdist_only_keeps_requires_python_and_flags() {
        let meta = PackageMetadata {
            info: Info {
                name: "somelib".into(),
                version: "1.0".into(),
                requires_python: Some(">=3.9".into()),
            },
            urls: vec![DistFile {
                filename: "somelib-1.0.tar.gz".into(),
                packagetype: "sdist".into(),
                requires_python: None,
                yanked: false,
            }],
            versions: vec!["1.0".into()],
        };
        let a = meta.analyze(&linux());
        assert!(a.sdist_only);
        assert!(!a.has_platform_wheels);
        assert!(a.supported.contains(PyVersion::py3(13)));
    }
}
