//! GPU detection, the input side of F2.
//!
//! `nvidia-smi` is the only local, offline signal for "what driver is
//! installed" — there is no packaging metadata for it. The probe result is
//! cached to disk with a 24h TTL, the same convention the PyPI "latest
//! version" cache uses, since a driver upgrade is rare enough that re-running
//! `nvidia-smi` on every buffer edit would be wasted work but a permanent
//! cache could go stale after a real upgrade.
//!
//! Apple Silicon is detected from the platform, not probed, since there is no
//! `nvidia-smi` equivalent to ask and none is needed: MPS is unconditional on
//! that hardware.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::core::command;
use crate::core::platform::{self, Arch, Os, Platform};

const CACHE_TTL_SECS: u64 = 24 * 60 * 60;

/// What the machine can run ML frameworks on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accelerator {
    /// An NVIDIA GPU with a readable driver version, e.g. "535.104.05".
    Nvidia {
        name: String,
        driver_version: String,
    },
    /// Apple Silicon: MPS is available unconditionally.
    AppleSilicon,
    /// No accelerator this crate knows how to target. CPU wheels are correct.
    None,
}

/// Detect the accelerator, using the disk cache when it is fresh.
pub async fn detect() -> Accelerator {
    let platform = Platform::current();
    if platform.os == Os::Macos && platform.arch == Arch::Aarch64 {
        return Accelerator::AppleSilicon;
    }

    if let Some(cached) = read_cache() {
        return cached;
    }

    let probed = probe_nvidia_smi().await;
    write_cache(&probed);
    probed
}

async fn probe_nvidia_smi() -> Accelerator {
    let args = ["--query-gpu=driver_version,name", "--format=csv,noheader"];
    let Ok(out) = command::run("nvidia-smi", &args, None).await else {
        return Accelerator::None;
    };
    if !out.success() {
        return Accelerator::None;
    }

    // "535.104.05, NVIDIA GeForce RTX 3080" — take the first reporting GPU.
    let Some(line) = out.first_stdout_line() else {
        return Accelerator::None;
    };
    let Some((driver, name)) = line.split_once(',') else {
        return Accelerator::None;
    };
    let driver_version = driver.trim().to_string();
    let name = name.trim().to_string();
    if driver_version.is_empty() {
        return Accelerator::None;
    }

    Accelerator::Nvidia {
        name,
        driver_version,
    }
}

#[derive(Serialize, Deserialize)]
struct CachedAccelerator {
    fetched_at_unix: u64,
    #[serde(flatten)]
    kind: CacheKind,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CacheKind {
    Nvidia {
        name: String,
        driver_version: String,
    },
    None,
}

fn cache_path() -> PathBuf {
    platform::cache_dir().join("gpu.json")
}

fn read_cache() -> Option<Accelerator> {
    let text = std::fs::read_to_string(cache_path()).ok()?;
    let cached: CachedAccelerator = serde_json::from_str(&text).ok()?;
    let age = now_unix().saturating_sub(cached.fetched_at_unix);
    if age > CACHE_TTL_SECS {
        return None;
    }
    Some(match cached.kind {
        CacheKind::Nvidia {
            name,
            driver_version,
        } => Accelerator::Nvidia {
            name,
            driver_version,
        },
        CacheKind::None => Accelerator::None,
    })
}

fn write_cache(accel: &Accelerator) {
    // Apple Silicon is never cached: it is derived from the platform, not
    // probed, so caching it would just be an extra file for no benefit.
    let kind = match accel {
        Accelerator::Nvidia {
            name,
            driver_version,
        } => CacheKind::Nvidia {
            name: name.clone(),
            driver_version: driver_version.clone(),
        },
        Accelerator::None => CacheKind::None,
        Accelerator::AppleSilicon => return,
    };
    let cached = CachedAccelerator {
        fetched_at_unix: now_unix(),
        kind,
    };
    if let Some(parent) = cache_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(&cached) {
        let _ = std::fs::write(cache_path(), text);
    }
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

    #[test]
    fn ttl_is_24_hours() {
        assert_eq!(CACHE_TTL_SECS, 24 * 60 * 60);
    }
}
