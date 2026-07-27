//! F2 — hardware / ML compatibility.
//!
//! For pip-installed frameworks the NVIDIA *driver* is what matters, not the
//! system CUDA toolkit: wheels bundle their own CUDA runtime, and drivers are
//! backwards compatible. The broken case is a driver too old for the wheel's
//! runtime; a system CUDA "ahead" of the framework is not a problem.
//!
//! [`nvidia`] holds the driver -> max-CUDA lookup, [`frameworks`] holds which
//! CUDA build variants a torch/tensorflow release ships, and [`solve`] ties
//! them to the machine's actual GPU (via [`crate::core::gpu`]) into a finding
//! plus, for torch, the index URL an install should use. [`check`] is the
//! read-only entry point the solver already calls into every [`crate::core::Assessment`];
//! [`solve::solve_framework`] is also called directly by the install path so
//! a real `torch` install pulls the build matched to the driver.
//!
//! The bundled datasets (`data/nvidia.json`, `data/frameworks.json`,
//! `data/import_map.json`) are embedded at compile time so an offline machine
//! always has a correct snapshot; the runtime refresh (7-day TTL per F7) will
//! overlay newer copies fetched from the project repo.

pub mod frameworks;
pub mod nvidia;
pub mod solve;

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::core::gpu::{self, Accelerator};
use crate::core::project::Requirement;
use crate::core::Finding;
use crate::settings::Settings;
use frameworks::Framework;

/// Bundled snapshots, embedded so they ship inside the binary.
pub const NVIDIA_JSON: &str = include_str!("../../data/nvidia.json");
pub const FRAMEWORKS_JSON: &str = include_str!("../../data/frameworks.json");
pub const IMPORT_MAP_JSON: &str = include_str!("../../data/import_map.json");

/// GPU / driver facts, for the report.
#[derive(Debug, Clone, Default)]
pub struct GpuInfo {
    pub name: Option<String>,
    pub driver_version: Option<String>,
    pub apple_silicon: bool,
}

/// The hardware side of an assessment.
#[derive(Debug, Clone, Default)]
pub struct HardwareReport {
    pub gpu: Option<GpuInfo>,
    pub findings: Vec<Finding>,
}

/// Run the hardware/ML compatibility check over a project's declared
/// dependencies. Read-only: probes the GPU and reads bundled data, touches
/// nothing on disk besides the GPU probe's own cache.
///
/// Silent by construction for everything except torch/tensorflow: packages
/// this module does not recognize simply produce no findings, matching the
/// "stay silent" contract rather than emitting a Info-per-package no-op.
pub async fn check(
    _workspace: &Path,
    _settings: &Settings,
    packages: &[Requirement],
) -> HardwareReport {
    let relevant: Vec<&Requirement> = packages
        .iter()
        .filter(|r| Framework::from_package_name(&r.name).is_some())
        .collect();

    if relevant.is_empty() {
        return HardwareReport::default();
    }

    let accelerator = gpu::detect().await;
    let gpu_info = Some(match &accelerator {
        Accelerator::Nvidia {
            name,
            driver_version,
        } => GpuInfo {
            name: Some(name.clone()),
            driver_version: Some(driver_version.clone()),
            apple_silicon: false,
        },
        Accelerator::AppleSilicon => GpuInfo {
            apple_silicon: true,
            ..Default::default()
        },
        Accelerator::None => GpuInfo::default(),
    });

    let mut findings = Vec::new();
    for req in relevant {
        if let Some(solved) = solve::solve_framework(&req.name, Some(req)).await {
            findings.extend(solved.finding);
        }
    }

    HardwareReport {
        gpu: gpu_info,
        findings,
    }
}

/// Loader for the bundled datasets, with room for the F7 refresh overlay.
///
/// Today it exposes the embedded snapshots verbatim; the refresh path is stubbed
/// to keep the surface stable for F2/F3-data work.
pub struct DataStore {
    pub nvidia: &'static str,
    pub frameworks: &'static str,
    pub import_map: &'static str,
}

impl DataStore {
    pub fn bundled() -> DataStore {
        DataStore {
            nvidia: NVIDIA_JSON,
            frameworks: FRAMEWORKS_JSON,
            import_map: IMPORT_MAP_JSON,
        }
    }

    /// Placeholder for the 7-day-TTL refresh from the project repo (F7).
    /// Returns the bundled snapshot until implemented.
    pub async fn load_or_refresh(_settings: &Settings) -> DataStore {
        DataStore::bundled()
    }
}

#[derive(Deserialize)]
struct ImportMapFile {
    map: HashMap<String, String>,
}

/// Import name to PyPI package name, parsed once on first use.
fn import_map() -> &'static HashMap<String, String> {
    static MAP: OnceLock<HashMap<String, String>> = OnceLock::new();
    MAP.get_or_init(|| {
        serde_json::from_str::<ImportMapFile>(IMPORT_MAP_JSON)
            .map(|f| f.map)
            .unwrap_or_default()
    })
}

/// Resolve an import name to the PyPI package that provides it.
///
/// Unlisted names resolve to themselves, which is usually right and occasionally
/// a typo. Callers must show the resolved name in any install prompt and never
/// act on it silently: that display step is the whole typosquatting defence.
pub fn resolve_package(import_name: &str) -> String {
    import_map()
        .get(import_name)
        .cloned()
        .unwrap_or_else(|| import_name.to_string())
}

/// True when the name came from the bundled table rather than falling through.
pub fn is_mapped(import_name: &str) -> bool {
    import_map().contains_key(import_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_datasets_are_valid_json() {
        // A malformed bundled dataset would be a build-time footgun for F2.
        serde_json::from_str::<serde_json::Value>(NVIDIA_JSON).unwrap();
        serde_json::from_str::<serde_json::Value>(FRAMEWORKS_JSON).unwrap();
        serde_json::from_str::<serde_json::Value>(IMPORT_MAP_JSON).unwrap();
    }

    #[test]
    fn maps_the_classic_mismatches() {
        assert_eq!(resolve_package("cv2"), "opencv-python");
        assert_eq!(resolve_package("PIL"), "pillow");
        assert_eq!(resolve_package("sklearn"), "scikit-learn");
        assert_eq!(resolve_package("yaml"), "PyYAML");
    }

    #[test]
    fn unmapped_names_resolve_to_themselves() {
        assert_eq!(resolve_package("mediapipe"), "mediapipe");
        assert_eq!(resolve_package("requests"), "requests");
        assert!(!is_mapped("mediapipe"));
        assert!(is_mapped("cv2"));
    }

    #[test]
    fn map_is_not_empty() {
        // A parse failure would silently degrade every lookup to identity.
        assert!(import_map().len() > 20);
    }

    #[tokio::test]
    async fn irrelevant_dependencies_produce_no_findings() {
        let packages = vec![Requirement::any("numpy"), Requirement::any("requests")];
        let report = check(Path::new("."), &Settings::default(), &packages).await;
        assert!(report.findings.is_empty());
        assert!(
            report.gpu.is_none(),
            "GPU is not even probed when nothing needs it"
        );
    }
}
