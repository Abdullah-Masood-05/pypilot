//! Disk cache for PyPI metadata.
//!
//! Two tiers, matching PyPI's immutability guarantees:
//!   * **Per-release** (`release/<name>-<version>.json`) — the file list for a
//!     specific released version never changes, so this is cached forever.
//!   * **Latest pointer** (`latest/<name>.json`) — "what is the newest version"
//!     changes over time, so this carries a fetch timestamp and a 24h TTL.
//!
//! Lives under the platform cache dir (see [`crate::core::platform::cache_dir`]).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::core::platform;
use crate::pypi::metadata::PackageMetadata;

/// 24 hours, in seconds — TTL for "latest version" lookups.
const LATEST_TTL_SECS: u64 = 24 * 60 * 60;

#[derive(Debug, Serialize, Deserialize)]
struct CachedLatest {
    fetched_at_unix: u64,
    metadata: PackageMetadata,
}

/// File-backed cache. Construct with [`Cache::open`]; callers that want to bypass
/// disk (tests) can use [`Cache::disabled`].
#[derive(Clone)]
pub struct Cache {
    root: Option<PathBuf>,
}

impl Cache {
    /// Open the on-disk cache under the platform cache dir.
    pub fn open() -> Cache {
        Cache {
            root: Some(platform::cache_dir().join("pypi")),
        }
    }

    /// A no-op cache (never reads or writes).
    pub fn disabled() -> Cache {
        Cache { root: None }
    }

    fn latest_path(&self, name: &str) -> Option<PathBuf> {
        self.root
            .as_ref()
            .map(|r| r.join("latest").join(format!("{}.json", normalize(name))))
    }

    fn release_path(&self, name: &str, version: &str) -> Option<PathBuf> {
        self.root.as_ref().map(|r| {
            r.join("release")
                .join(format!("{}-{}.json", normalize(name), version))
        })
    }

    /// Return cached latest metadata if present and within the 24h TTL.
    pub fn get_latest(&self, name: &str) -> Option<PackageMetadata> {
        let path = self.latest_path(name)?;
        let text = std::fs::read_to_string(&path).ok()?;
        let cached: CachedLatest = serde_json::from_str(&text).ok()?;
        if now_unix().saturating_sub(cached.fetched_at_unix) <= LATEST_TTL_SECS {
            Some(cached.metadata)
        } else {
            None
        }
    }

    /// Return immutable per-release metadata if we've ever fetched this version.
    pub fn get_release(&self, name: &str, version: &str) -> Option<PackageMetadata> {
        let path = self.release_path(name, version)?;
        let text = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Persist freshly-fetched metadata into both tiers.
    pub fn put(&self, name: &str, metadata: &PackageMetadata) {
        // Immutable per-release entry (cache forever).
        if let Some(path) = self.release_path(name, &metadata.info.version) {
            write_json(
                &path, metadata,
                /* overwrite = */ false, // immutable: don't rewrite if it exists
            );
        }
        // Latest pointer with timestamp (24h TTL).
        if let Some(path) = self.latest_path(name) {
            let cached = CachedLatest {
                fetched_at_unix: now_unix(),
                metadata: metadata.clone(),
            };
            write_json(&path, &cached, /* overwrite = */ true);
        }
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T, overwrite: bool) {
    if !overwrite && path.exists() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(value) {
        let _ = std::fs::write(path, text);
    }
}

/// Normalize a package name for use in a filename (PEP 503-ish).
fn normalize(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
