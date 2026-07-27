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
//! always has a correct snapshot; [`refresh`] overlays newer copies fetched
//! from the project repo, TTL-gated by F7's `data_refresh_days`, and falls
//! back to the bundled snapshot silently on any failure.

pub mod frameworks;
pub mod nvidia;
pub mod refresh;
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

/// F7's documented default TTL, used where a caller has no `Settings` in
/// scope to read the user's configured value from (see [`import_map`]).
const DEFAULT_TTL_DAYS: u32 = 7;

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
/// dependencies. Read-only from the caller's point of view: the GPU probe has
/// its own disk cache, and any data refresh happens in the background without
/// this call waiting on it.
///
/// Silent by construction for everything except torch/tensorflow: packages
/// this module does not recognize simply produce no findings, matching the
/// "stay silent" contract rather than emitting a Info-per-package no-op.
pub async fn check(
    _workspace: &Path,
    settings: &Settings,
    packages: &[Requirement],
) -> HardwareReport {
    let relevant: Vec<&Requirement> = packages
        .iter()
        .filter(|r| Framework::from_package_name(&r.name).is_some())
        .collect();

    if relevant.is_empty() {
        return HardwareReport::default();
    }

    // Best-effort, non-blocking: refreshes the on-disk cache for the *next*
    // scan without making this one wait on a network round trip. A stale
    // cache always falls back to the bundled snapshot, never to an error.
    tokio::spawn(refresh::refresh_if_stale(settings.clone()));

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
        if let Some(solved) = solve::solve_framework(&req.name, Some(req), settings).await {
            findings.extend(solved.finding);
        }
    }

    HardwareReport {
        gpu: gpu_info,
        findings,
    }
}

#[derive(Deserialize)]
struct ImportMapFile {
    map: HashMap<String, String>,
}

/// Import name to PyPI package name, parsed once on first use.
///
/// Cached for the process lifetime, which is fine for a short-lived CLI
/// invocation or a single LSP session: the disk-cache check inside
/// [`refresh::read`] already picks up whatever the last background refresh
/// wrote, so a freshly started process always sees the newest cached data.
fn import_map() -> &'static HashMap<String, String> {
    static MAP: OnceLock<HashMap<String, String>> = OnceLock::new();
    MAP.get_or_init(|| {
        let json = refresh::read("import_map.json", IMPORT_MAP_JSON, DEFAULT_TTL_DAYS);
        serde_json::from_str::<ImportMapFile>(&json)
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
