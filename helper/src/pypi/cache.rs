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

/// Bumped whenever the shape of a cached document changes.
///
/// The version is part of the path, so entries written by an older build are
/// simply never read rather than being deserialized into a struct that has
/// since grown a field. Missing the bump on the release that added the version
/// list made every cached package look like it had no releases at all, which
/// silently broke pinned dependencies.
const SCHEMA: &str = "v2";

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
            root: Some(platform::cache_dir().join("pypi").join(SCHEMA)),
        }
    }

    /// A no-op cache (never reads or writes).
    pub fn disabled() -> Cache {
        Cache { root: None }
    }

    /// A cache rooted at an explicit directory, for tests.
    pub fn at(root: impl Into<PathBuf>) -> Cache {
        Cache {
            root: Some(root.into()),
        }
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
    pub fn put_latest(&self, name: &str, metadata: &PackageMetadata) {
        self.put_release(name, metadata);

        // Latest pointer with timestamp (24h TTL).
        if let Some(path) = self.latest_path(name) {
            let cached = CachedLatest {
                fetched_at_unix: now_unix(),
                metadata: metadata.clone(),
            };
            write_json(&path, &cached, /* overwrite = */ true);
        }
    }

    /// Store one specific release, and only that.
    ///
    /// Deliberately leaves the latest pointer alone. Resolving a pin fetches an
    /// older release, and writing that document into the latest slot would make
    /// every later lookup for the package answer with the older release.
    pub fn put_release(&self, name: &str, metadata: &PackageMetadata) {
        if let Some(path) = self.release_path(name, &metadata.info.version) {
            write_json(
                &path, metadata,
                /* overwrite = */ false, // immutable: don't rewrite if it exists
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pypi::metadata::Info;

    fn meta(version: &str) -> PackageMetadata {
        PackageMetadata {
            info: Info {
                name: "mediapipe".into(),
                version: version.into(),
                requires_python: None,
            },
            urls: vec![],
            versions: vec!["0.10.14".into(), "0.10.35".into()],
        }
    }

    fn scratch(tag: &str) -> Cache {
        let dir = std::env::temp_dir().join(format!("pypilot-cache-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        Cache::at(dir)
    }

    #[test]
    fn storing_an_older_release_does_not_move_the_latest_pointer() {
        // The regression: resolving a pin fetches an older release, and writing
        // that into the latest slot made every later lookup answer with it.
        let cache = scratch("tiers");
        cache.put_latest("mediapipe", &meta("0.10.35"));
        cache.put_release("mediapipe", &meta("0.10.14"));

        let latest = cache.get_latest("mediapipe").expect("latest still cached");
        assert_eq!(
            latest.info.version, "0.10.35",
            "the newest release must stay the latest answer"
        );

        let pinned = cache
            .get_release("mediapipe", "0.10.14")
            .expect("the older release is retrievable on its own");
        assert_eq!(pinned.info.version, "0.10.14");
    }

    #[test]
    fn version_list_survives_a_cache_round_trip() {
        // Serialized as an array, read back through the same deserializer that
        // handles PyPI's releases object. Losing this silently breaks pins.
        let cache = scratch("roundtrip");
        cache.put_latest("mediapipe", &meta("0.10.35"));

        let back = cache.get_latest("mediapipe").unwrap();
        assert_eq!(back.versions, vec!["0.10.14", "0.10.35"]);
    }
}
