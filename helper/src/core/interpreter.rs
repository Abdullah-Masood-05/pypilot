//! Interpreter discovery.
//!
//! Probes the machine for CPython interpreters by trying the conventional
//! command names in parallel and asking each for its version. Results are deduped
//! by the interpreter's resolved `sys.executable` so `python` and `python3`
//! pointing at the same binary count once.

use std::collections::HashMap;

use crate::core::command;
use crate::pypi::pyversion::{PyVersion, MAX_MINOR, MIN_MINOR};

/// A Python interpreter found on the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interpreter {
    /// The command name we found it under (e.g. "python3.12").
    pub command: String,
    /// Its resolved absolute path (`sys.executable`).
    pub path: String,
    pub version: PyVersion,
}

/// One-line Python program that prints "MAJOR.MINOR\t<executable path>".
const PROBE: &str =
    "import sys;print('%d.%d\t%s'%(sys.version_info[0],sys.version_info[1],sys.executable))";

/// Discover interpreters, running every candidate probe concurrently.
pub async fn discover() -> Vec<Interpreter> {
    let mut candidates: Vec<String> = vec!["python3".to_string(), "python".to_string()];
    for minor in MIN_MINOR..=MAX_MINOR {
        candidates.push(format!("python3.{minor}"));
    }

    let probes = candidates.into_iter().map(|cmd| async move {
        let out = command::run(&cmd, &["-c", PROBE], None).await.ok()?;
        if !out.success() {
            return None;
        }
        let line = out.first_stdout_line()?;
        let (ver, path) = line.split_once('\t')?;
        let version = PyVersion::parse(ver)?;
        Some(Interpreter {
            command: cmd,
            path: path.trim().to_string(),
            version,
        })
    });

    let found = futures::future::join_all(probes).await;

    // Dedupe by resolved executable path, preferring the most specific command.
    let mut by_path: HashMap<String, Interpreter> = HashMap::new();
    for interp in found.into_iter().flatten() {
        by_path
            .entry(interp.path.clone())
            .and_modify(|existing| {
                if interp.command.len() > existing.command.len() {
                    *existing = interp.clone();
                }
            })
            .or_insert(interp);
    }

    let mut list: Vec<Interpreter> = by_path.into_values().collect();
    list.sort_by_key(|i| std::cmp::Reverse(i.version));
    list
}

/// Find an interpreter matching an exact minor version (for pip-mode venv creation).
pub fn find_version(interpreters: &[Interpreter], version: PyVersion) -> Option<&Interpreter> {
    interpreters.iter().find(|i| i.version == version)
}
