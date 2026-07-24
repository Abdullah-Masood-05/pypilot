//! F2 seam — hardware / ML compatibility.
//!
//! Phase 1 ships this as an intentional stub: [`check`] returns an empty
//! [`HardwareReport`], and the solver already merges its findings into the
//! consolidated list. When F2 lands, the CUDA/driver/framework logic fills in
//! here and in [`DataStore`] — no caller changes required.
//!
//! The bundled datasets (`data/nvidia.json`, `data/frameworks.json`,
//! `data/import_map.json`) are embedded at compile time so an offline machine
//! always has a correct snapshot; the runtime refresh (7-day TTL per F7) will
//! overlay newer copies fetched from the project repo.

use std::path::Path;

use crate::core::Finding;
use crate::settings::Settings;

/// Bundled snapshots, embedded so they ship inside the binary.
pub const NVIDIA_JSON: &str = include_str!("../../data/nvidia.json");
pub const FRAMEWORKS_JSON: &str = include_str!("../../data/frameworks.json");
pub const IMPORT_MAP_JSON: &str = include_str!("../../data/import_map.json");

/// GPU / driver facts, populated by F2.
#[derive(Debug, Clone, Default)]
pub struct GpuInfo {
    pub name: Option<String>,
    pub driver_version: Option<String>,
}

/// The hardware side of an assessment. Empty in Phase 1.
#[derive(Debug, Clone, Default)]
pub struct HardwareReport {
    pub gpu: Option<GpuInfo>,
    pub findings: Vec<Finding>,
}

/// Run the hardware/ML compatibility check.
///
/// Phase 1: no-op. The signature already takes everything F2 needs (the workspace
/// for reading pinned framework versions, settings for offline mode) so the fill-in
/// is additive.
pub async fn check(
    _workspace: &Path,
    _settings: &Settings,
    _packages: &[String],
) -> HardwareReport {
    HardwareReport::default()
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
}
