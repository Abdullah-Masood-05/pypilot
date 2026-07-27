//! Framework release -> CUDA build variants.
//!
//! Python-version support is deliberately **not** duplicated here: torch and
//! tensorflow are ordinary PyPI packages, so F3's existing wheel-tag analysis
//! already answers "does this release support this Python" the same way it
//! does for every other dependency. This table only carries what F3 cannot
//! derive — which CUDA runtime each release's wheels target, since that is
//! encoded in a download-server URL, not in packaging metadata.

use serde::Deserialize;

use crate::matrix::nvidia::CudaVersion;

#[derive(Debug, Deserialize)]
struct FrameworksFile {
    torch: TorchFile,
    tensorflow: TensorFlowFile,
}

#[derive(Debug, Deserialize)]
struct TorchFile {
    cpu_index_url: String,
    cuda_index_url_pattern: String,
    releases: Vec<TorchReleaseRow>,
}

#[derive(Debug, Deserialize)]
struct TorchReleaseRow {
    version: String,
    cuda_builds: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TensorFlowFile {
    releases: Vec<TensorFlowReleaseRow>,
}

#[derive(Debug, Deserialize)]
struct TensorFlowReleaseRow {
    version: String,
    min_cuda: String,
}

/// One CUDA-specific torch wheel variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CudaBuild {
    /// The tag as PyTorch names it, e.g. "cu121".
    pub tag: String,
    pub cuda: CudaVersion,
    pub index_url: String,
}

/// What one release of a framework offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseInfo {
    Torch {
        version: String,
        cpu_index_url: String,
        cuda_builds: Vec<CudaBuild>,
    },
    TensorFlow {
        version: String,
        /// TensorFlow does not offer separate build variants; the single
        /// published wheel needs a driver new enough for this CUDA version.
        min_cuda: CudaVersion,
    },
}

impl ReleaseInfo {
    pub fn version(&self) -> &str {
        match self {
            ReleaseInfo::Torch { version, .. } => version,
            ReleaseInfo::TensorFlow { version, .. } => version,
        }
    }
}

/// Which framework a dependency name refers to. Matching is on the PyPI
/// distribution name, case-insensitively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framework {
    Torch,
    TensorFlow,
}

impl Framework {
    pub fn from_package_name(name: &str) -> Option<Framework> {
        match name.to_ascii_lowercase().as_str() {
            "torch" => Some(Framework::Torch),
            "tensorflow" => Some(Framework::TensorFlow),
            _ => None,
        }
    }
}

pub struct FrameworkTable {
    torch: TorchFile,
    tensorflow: TensorFlowFile,
}

impl FrameworkTable {
    pub fn load(json: &str) -> Option<FrameworkTable> {
        let file: FrameworksFile = serde_json::from_str(json).ok()?;
        Some(FrameworkTable {
            torch: file.torch,
            tensorflow: file.tensorflow,
        })
    }

    /// The newest release PyPilot has curated CUDA-build data for. Used when a
    /// dependency is unpinned, per F2's "or latest if unpinned" rule.
    pub fn latest(&self, framework: Framework) -> Option<&str> {
        match framework {
            Framework::Torch => self.torch.releases.last().map(|r| r.version.as_str()),
            Framework::TensorFlow => self.tensorflow.releases.last().map(|r| r.version.as_str()),
        }
    }

    /// Every version the bundled snapshot has data for. A pinned dependency's
    /// specifier is evaluated against exactly this list, since a version with
    /// no entry here has no CUDA-build data to solve with regardless.
    pub fn all_versions(&self, framework: Framework) -> Vec<String> {
        match framework {
            Framework::Torch => self
                .torch
                .releases
                .iter()
                .map(|r| r.version.clone())
                .collect(),
            Framework::TensorFlow => self
                .tensorflow
                .releases
                .iter()
                .map(|r| r.version.clone())
                .collect(),
        }
    }

    /// Look up curated data for an exact release. `None` when the version is
    /// not in the bundled snapshot — newer than PyPilot's data, or a very old
    /// release nobody curated. Callers must degrade gracefully, not error.
    pub fn release(&self, framework: Framework, version: &str) -> Option<ReleaseInfo> {
        match framework {
            Framework::Torch => {
                let row = self.torch.releases.iter().find(|r| r.version == version)?;
                let cuda_builds = row
                    .cuda_builds
                    .iter()
                    .filter_map(|tag| {
                        Some(CudaBuild {
                            tag: tag.clone(),
                            cuda: parse_cuda_tag(tag)?,
                            index_url: self.torch.cuda_index_url_pattern.replace("{tag}", tag),
                        })
                    })
                    .collect();
                Some(ReleaseInfo::Torch {
                    version: row.version.clone(),
                    cpu_index_url: self.torch.cpu_index_url.clone(),
                    cuda_builds,
                })
            }
            Framework::TensorFlow => {
                let row = self
                    .tensorflow
                    .releases
                    .iter()
                    .find(|r| r.version == version)?;
                Some(ReleaseInfo::TensorFlow {
                    version: row.version.clone(),
                    min_cuda: parse_dotted_cuda(&row.min_cuda)?,
                })
            }
        }
    }
}

/// "cu121" -> CUDA 12.1. The major is always the first two digits and the
/// minor is whatever follows; this has held for every tag PyTorch has
/// published (cu118, cu121, cu124, cu126, cu128, cu130, cu132, ...), so it is
/// computed rather than curated.
fn parse_cuda_tag(tag: &str) -> Option<CudaVersion> {
    let digits = tag.strip_prefix("cu")?;
    if digits.len() < 3 {
        return None;
    }
    let major: u32 = digits[..2].parse().ok()?;
    let minor: u32 = digits[2..].parse().ok()?;
    Some(CudaVersion { major, minor })
}

fn parse_dotted_cuda(s: &str) -> Option<CudaVersion> {
    let (major, minor) = s.split_once('.')?;
    Some(CudaVersion {
        major: major.parse().ok()?,
        minor: minor.parse().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> FrameworkTable {
        FrameworkTable::load(crate::matrix::FRAMEWORKS_JSON).expect("bundled table parses")
    }

    #[test]
    fn cu_tag_parses_single_and_double_digit_minors() {
        assert_eq!(
            parse_cuda_tag("cu118"),
            Some(CudaVersion {
                major: 11,
                minor: 8
            })
        );
        assert_eq!(
            parse_cuda_tag("cu121"),
            Some(CudaVersion {
                major: 12,
                minor: 1
            })
        );
        assert_eq!(
            parse_cuda_tag("cu130"),
            Some(CudaVersion {
                major: 13,
                minor: 0
            })
        );
    }

    #[test]
    fn framework_name_matching_is_case_insensitive() {
        assert_eq!(
            Framework::from_package_name("Torch"),
            Some(Framework::Torch)
        );
        assert_eq!(
            Framework::from_package_name("TensorFlow"),
            Some(Framework::TensorFlow)
        );
        assert_eq!(Framework::from_package_name("numpy"), None);
    }

    #[test]
    fn torch_release_lists_its_cuda_builds_with_urls() {
        let t = table();
        let info = t.release(Framework::Torch, "2.4.0").unwrap();
        let ReleaseInfo::Torch {
            cuda_builds,
            cpu_index_url,
            ..
        } = info
        else {
            panic!("expected a torch release");
        };
        assert!(cpu_index_url.ends_with("/cpu"));
        let cu121 = cuda_builds.iter().find(|b| b.tag == "cu121").unwrap();
        assert_eq!(
            cu121.cuda,
            CudaVersion {
                major: 12,
                minor: 1
            }
        );
        assert_eq!(cu121.index_url, "https://download.pytorch.org/whl/cu121");
    }

    #[test]
    fn tensorflow_release_carries_its_minimum_cuda() {
        let t = table();
        let info = t.release(Framework::TensorFlow, "2.15.0").unwrap();
        assert_eq!(
            info,
            ReleaseInfo::TensorFlow {
                version: "2.15.0".into(),
                min_cuda: CudaVersion {
                    major: 12,
                    minor: 2
                },
            }
        );
    }

    #[test]
    fn unknown_version_is_none_not_an_error() {
        let t = table();
        assert!(t.release(Framework::Torch, "0.0.1").is_none());
    }

    #[test]
    fn latest_is_the_last_curated_release() {
        let t = table();
        assert!(t.latest(Framework::Torch).is_some());
        assert!(t.latest(Framework::TensorFlow).is_some());
    }
}
