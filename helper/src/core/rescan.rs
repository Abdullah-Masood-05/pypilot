//! CLI → LSP rescan bridge.
//!
//! A task-spawned `pypilot doctor|setup` is a separate short-lived process; it
//! cannot draw editor toasts. The long-running LSP instance can. Bridge: the CLI
//! drops a per-workspace request file in the platform cache dir, and the LSP
//! polls that file's mtime (one `stat` per second — effectively free) and
//! re-runs the scan → toast when it changes.
//!
//! The file lives outside the workspace so repos are never dirtied, and it is
//! keyed by the canonicalized workspace path so multiple workspaces don't
//! collide. Both writer and watcher are the same binary, so the hash is
//! guaranteed consistent.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::core::platform;

/// What triggered the request — recorded for future use / debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Doctor,
    Setup,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Doctor => "doctor",
            Kind::Setup => "setup",
        }
    }
}

/// The request-file path for a workspace.
pub fn request_path(workspace: &Path) -> PathBuf {
    // Canonicalize so `--path .` from a task and Zed's absolute root agree.
    let canon = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let key = canon.to_string_lossy().to_lowercase();

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let digest = hasher.finish();

    platform::cache_dir()
        .join("rescan")
        .join(format!("{digest:016x}.req"))
}

/// Ask a running LSP (if any) to rescan this workspace and toast the result.
/// Best-effort: errors are swallowed — the CLI's own output already happened.
pub fn notify(workspace: &Path, kind: Kind) {
    let path = request_path(workspace);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let _ = std::fs::write(&path, format!("{} {stamp}", kind.as_str()));
}

/// Current mtime of the request file, if it exists.
pub fn request_mtime(workspace: &Path) -> Option<SystemTime> {
    std::fs::metadata(request_path(workspace))
        .and_then(|m| m.modified())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_is_deterministic_and_outside_workspace() {
        let ws = std::env::temp_dir();
        let a = request_path(&ws);
        let b = request_path(&ws);
        assert_eq!(a, b);
        assert!(!a.starts_with(&ws) || a.starts_with(platform::cache_dir()));
        assert!(a.extension().is_some_and(|e| e == "req"));
    }

    #[test]
    fn notify_writes_and_mtime_is_visible() {
        let ws = std::env::temp_dir();
        notify(&ws, Kind::Doctor);
        assert!(request_path(&ws).is_file());
        assert!(request_mtime(&ws).is_some());
    }
}
