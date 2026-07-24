//! F7 — user settings.
//!
//! A global config file (in the platform config dir) provides defaults; a
//! per-project file overrides individual keys. `package_manager` is respected
//! everywhere in the codebase — the uv driver refuses to run in pip mode, and the
//! setup flow branches on it.
//!
//! Per-project files are looked up, in order:
//!   1. `<workspace>/.zed/pypilot.toml`
//!   2. `<workspace>/pypilot.toml`
//!
//! The same per-project file also stores F5 dismissal state (`[state] never = true`)
//! so "Never for this project" is remembered.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Which package manager drives installs. `pip` mode never downloads or invokes uv.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum PackageManager {
    #[default]
    Uv,
    Pip,
}

/// F5 toast verbosity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum Notifications {
    All,
    #[default]
    ProblemsOnly,
    Off,
}

/// Raw, all-optional shape as parsed from a TOML file. Merging is "last write
/// wins per present key", which is why every field is an `Option`.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
struct RawSettings {
    package_manager: Option<PackageManager>,
    notifications: Option<Notifications>,
    auto_check_on_open: Option<bool>,
    data_refresh_days: Option<u32>,
    #[serde(default)]
    state: RawState,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
struct RawState {
    /// "Never for this project" — F5 dismissal.
    never: Option<bool>,
}

/// Fully-resolved settings after merging global defaults with a project override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub package_manager: PackageManager,
    pub notifications: Notifications,
    pub auto_check_on_open: bool,
    /// TTL in days for bundled matrix/mapping refresh; `0` = fully offline.
    pub data_refresh_days: u32,
    /// True when the user chose "Never for this project".
    pub dismissed: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            package_manager: PackageManager::default(),
            notifications: Notifications::default(),
            auto_check_on_open: true,
            data_refresh_days: 7,
            dismissed: false,
        }
    }
}

impl Settings {
    /// Load global settings, then apply a per-project override if present.
    pub fn load(workspace: &Path) -> Settings {
        let mut merged = RawSettings::default();

        if let Some(global) = global_config_path() {
            merge(&mut merged, read_raw(&global));
        }
        if let Some(project) = project_config_path(workspace) {
            merge(&mut merged, read_raw(&project));
        }

        let d = Settings::default();
        Settings {
            package_manager: merged.package_manager.unwrap_or(d.package_manager),
            notifications: merged.notifications.unwrap_or(d.notifications),
            auto_check_on_open: merged.auto_check_on_open.unwrap_or(d.auto_check_on_open),
            data_refresh_days: merged.data_refresh_days.unwrap_or(d.data_refresh_days),
            dismissed: merged.state.never.unwrap_or(false),
        }
    }

    /// Persist "Never for this project" into `<workspace>/.zed/pypilot.toml`,
    /// preserving any settings already there.
    pub fn mark_dismissed(workspace: &Path) -> crate::Result<()> {
        let dir = workspace.join(".zed");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("pypilot.toml");

        let mut raw = read_raw(&path).unwrap_or_default();
        raw.state.never = Some(true);
        let text = toml::to_string_pretty(&raw)?;
        std::fs::write(&path, text)?;
        Ok(())
    }
}

/// Path to the global config file, e.g. `~/.config/pypilot/config.toml`.
pub fn global_config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "PyPilot", "pypilot")
        .map(|d| d.config_dir().join("config.toml"))
}

/// Per-project config path, preferring `.zed/pypilot.toml`.
pub fn project_config_path(workspace: &Path) -> Option<PathBuf> {
    let zed = workspace.join(".zed").join("pypilot.toml");
    if zed.exists() {
        return Some(zed);
    }
    let root = workspace.join("pypilot.toml");
    if root.exists() {
        return Some(root);
    }
    // Return the preferred location even if absent, so callers can describe where
    // config *would* live; `read_raw` tolerates a missing file.
    Some(zed)
}

fn read_raw(path: &Path) -> Option<RawSettings> {
    let text = std::fs::read_to_string(path).ok()?;
    match toml::from_str::<RawSettings>(&text) {
        Ok(raw) => Some(raw),
        Err(e) => {
            eprintln!("pypilot: ignoring malformed config {}: {e}", path.display());
            None
        }
    }
}

/// Overlay `overlay` onto `base`, key by key (present keys win).
fn merge(base: &mut RawSettings, overlay: Option<RawSettings>) {
    let Some(o) = overlay else { return };
    if o.package_manager.is_some() {
        base.package_manager = o.package_manager;
    }
    if o.notifications.is_some() {
        base.notifications = o.notifications;
    }
    if o.auto_check_on_open.is_some() {
        base.auto_check_on_open = o.auto_check_on_open;
    }
    if o.data_refresh_days.is_some() {
        base.data_refresh_days = o.data_refresh_days;
    }
    if o.state.never.is_some() {
        base.state.never = o.state.never;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_uv_first() {
        let s = Settings::default();
        assert_eq!(s.package_manager, PackageManager::Uv);
        assert_eq!(s.notifications, Notifications::ProblemsOnly);
        assert!(!s.dismissed);
    }

    #[test]
    fn project_override_wins_over_global() {
        let mut base = RawSettings {
            package_manager: Some(PackageManager::Uv),
            ..Default::default()
        };
        merge(
            &mut base,
            Some(RawSettings {
                package_manager: Some(PackageManager::Pip),
                ..Default::default()
            }),
        );
        assert_eq!(base.package_manager, Some(PackageManager::Pip));
    }

    #[test]
    fn absent_overlay_keys_do_not_clobber() {
        let mut base = RawSettings {
            package_manager: Some(PackageManager::Pip),
            notifications: Some(Notifications::All),
            ..Default::default()
        };
        merge(&mut base, Some(RawSettings::default()));
        assert_eq!(base.package_manager, Some(PackageManager::Pip));
        assert_eq!(base.notifications, Some(Notifications::All));
    }
}
