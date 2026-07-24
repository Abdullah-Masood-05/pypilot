//! Windows-safe command runner.
//!
//! Every spawned process uses an explicit argument vector — never a shell string
//! — so quoting/interpolation differences across platforms simply don't exist.
//! There is no `sh -c` / `cmd /c` anywhere in PyPilot.

use std::ffi::OsStr;
use std::path::Path;

use tokio::process::Command;

/// Captured result of running a process.
#[derive(Debug, Clone)]
pub struct Output {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    /// First non-empty line of stdout, trimmed (handy for `--version` probes).
    pub fn first_stdout_line(&self) -> Option<&str> {
        self.stdout.lines().map(str::trim).find(|l| !l.is_empty())
    }
}

/// Run `program args...` in `cwd`, capturing stdout/stderr. Returns `Err` only
/// when the process can't be spawned at all (missing binary, permissions); a
/// non-zero exit is a successful *run* with `status != Some(0)`.
pub async fn run<S, A>(program: S, args: &[A], cwd: Option<&Path>) -> crate::Result<Output>
where
    S: AsRef<OsStr>,
    A: AsRef<OsStr>,
{
    let mut cmd = Command::new(program.as_ref());
    for a in args {
        cmd.arg(a.as_ref());
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    // Keep child stdio off our own; capture instead.
    cmd.stdin(std::process::Stdio::null());

    let out = cmd.output().await.map_err(|e| {
        anyhow::anyhow!(
            "failed to spawn `{}`: {e}",
            program.as_ref().to_string_lossy()
        )
    })?;

    Ok(Output {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Convenience: run a probe and return the trimmed first stdout line on success.
pub async fn probe_version<S, A>(program: S, args: &[A]) -> Option<String>
where
    S: AsRef<OsStr>,
    A: AsRef<OsStr>,
{
    let out = run(program, args, None).await.ok()?;
    if out.success() {
        out.first_stdout_line().map(|s| s.to_string())
    } else {
        None
    }
}
