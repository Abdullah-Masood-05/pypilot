//! pip-mode driver (F1 fallback).
//!
//! Same detection and compatibility logic as uv mode — only the install engine
//! differs. pip mode never downloads or invokes uv: it uses an interpreter that
//! already exists on the machine, `python -m venv`, and `pip install`. If the
//! required Python isn't installed, we can't fetch it (that's uv's job), so we
//! surface a clear "install Python X.Y manually" message instead.

use std::path::{Path, PathBuf};

use crate::core::command::{self, Output};
use crate::core::interpreter::{self, Interpreter};
use crate::pypi::pyversion::PyVersion;

/// The venv's interpreter path, given the venv root.
pub fn venv_python(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}

/// Create a venv at `venv_path` using an already-installed `interpreter`.
pub async fn create_venv(
    interpreter: &Interpreter,
    venv_path: &Path,
    cwd: &Path,
) -> crate::Result<Output> {
    let venv = venv_path.to_string_lossy().into_owned();
    command::run(&interpreter.command, &["-m", "venv", &venv], Some(cwd)).await
}

/// `python -m pip install -r requirements.txt` inside the venv.
pub async fn install_requirements(
    venv: &Path,
    requirements: &Path,
    cwd: &Path,
) -> crate::Result<Output> {
    let py = venv_python(venv);
    let req = requirements.to_string_lossy().into_owned();
    command::run(&py, &["-m", "pip", "install", "-r", &req], Some(cwd)).await
}

/// `python -m pip install <pkgs...>` inside the venv.
pub async fn install_packages(
    venv: &Path,
    packages: &[String],
    cwd: &Path,
) -> crate::Result<Output> {
    let py = venv_python(venv);
    let mut args: Vec<String> = vec!["-m".into(), "pip".into(), "install".into()];
    args.extend(packages.iter().cloned());
    command::run(&py, &args, Some(cwd)).await
}

/// Pick an installed interpreter for the target version, preferring an exact
/// minor match. Returns `None` when the required Python isn't on the machine —
/// the caller then tells the user to install it (pip mode can't fetch Pythons).
pub fn select_interpreter(interpreters: &[Interpreter], target: PyVersion) -> Option<&Interpreter> {
    interpreter::find_version(interpreters, target)
}
