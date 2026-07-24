//! Parallel environment probes.
//!
//! The whole workspace scan (excluding network) targets <200ms, so every probe
//! runs concurrently: uv presence, venv presence + its interpreter, and the set
//! of interpreters on the machine. Hardware detection (F2) will add one more
//! concurrent probe here without disturbing the shape.

use std::path::{Path, PathBuf};

use crate::core::interpreter::{self, Interpreter};
use crate::core::uv::{self, UvInfo};
use crate::pypi::pyversion::PyVersion;
use crate::settings::Settings;

/// A discovered virtual environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VenvInfo {
    pub path: PathBuf,
    /// Interpreter version recorded in `pyvenv.cfg`, if parseable.
    pub python: Option<PyVersion>,
}

/// Everything the fast probe phase learned about the environment.
#[derive(Debug, Clone, Default)]
pub struct Probes {
    pub uv: Option<UvInfo>,
    pub venv: Option<VenvInfo>,
    pub interpreters: Vec<Interpreter>,
}

/// Run all probes concurrently for `workspace`.
pub async fn run(workspace: &Path, settings: &Settings) -> Probes {
    let uv_fut = uv::detect(settings);
    let venv_fut = detect_venv(workspace);
    let interp_fut = interpreter::discover();

    let (uv, venv, interpreters) = tokio::join!(uv_fut, venv_fut, interp_fut);

    Probes {
        uv,
        venv,
        interpreters,
    }
}

/// Look for a `.venv` (or `venv`) directory with a `pyvenv.cfg`.
async fn detect_venv(workspace: &Path) -> Option<VenvInfo> {
    for name in [".venv", "venv", "env"] {
        let dir = workspace.join(name);
        let cfg = dir.join("pyvenv.cfg");
        if let Ok(text) = tokio::fs::read_to_string(&cfg).await {
            return Some(VenvInfo {
                path: dir,
                python: parse_pyvenv_version(&text),
            });
        }
    }
    None
}

/// Parse `version = 3.12.1` (or `version_info`) out of a `pyvenv.cfg`.
fn parse_pyvenv_version(text: &str) -> Option<PyVersion> {
    for line in text.lines() {
        let line = line.trim();
        let key = line.split('=').next().map(str::trim).unwrap_or("");
        if key == "version" || key == "version_info" {
            if let Some(val) = line.split('=').nth(1) {
                if let Some(v) = PyVersion::parse(val.trim()) {
                    return Some(v);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pyvenv_cfg_version() {
        let cfg = "home = /usr/bin\nversion = 3.12.1\ninclude-system-site-packages = false\n";
        assert_eq!(parse_pyvenv_version(cfg), Some(PyVersion::py3(12)));
    }
}
