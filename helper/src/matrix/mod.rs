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

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use serde::Deserialize;

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
}
