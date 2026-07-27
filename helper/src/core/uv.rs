//! uv driver — detection, managed download, and the install operations.
//!
//! uv is never installed system-wide. If it isn't already on `$PATH`, PyPilot
//! fetches the correct standalone archive for the OS/arch from astral-sh/uv's
//! GitHub releases into a managed data dir, extracts it, and verifies it with
//! `uv --version` before ever using it.
//!
//! In `package_manager = "pip"` mode this module is inert: [`detect`] returns
//! `None` and [`ensure`] refuses — nothing here downloads or invokes uv.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::Deserialize;

use crate::core::command::{self, Output};
use crate::core::platform::{self, Platform};
use crate::pypi::pyversion::PyVersion;
use crate::settings::{PackageManager, Settings};

/// A usable uv binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UvInfo {
    pub path: PathBuf,
    pub version: String,
    /// True when this is the PyPilot-managed copy (vs. one found on `$PATH`).
    pub managed: bool,
}

/// Where the managed uv binary lives.
fn managed_uv_path() -> PathBuf {
    let name = if cfg!(windows) { "uv.exe" } else { "uv" };
    platform::data_dir().join("uv").join(name)
}

/// Detect an available uv, without downloading. Returns `None` in pip mode.
pub async fn detect(settings: &Settings) -> Option<UvInfo> {
    if settings.package_manager == PackageManager::Pip {
        return None;
    }

    // Prefer the managed copy (known-good, verified on install).
    let managed = managed_uv_path();
    if managed.is_file() {
        if let Some(version) = command::probe_version(&managed, &["--version"]).await {
            return Some(UvInfo {
                path: managed,
                version: clean_version(&version),
                managed: true,
            });
        }
    }

    // Otherwise a uv on PATH.
    if let Some(version) = command::probe_version("uv", &["--version"]).await {
        return Some(UvInfo {
            path: PathBuf::from("uv"),
            version: clean_version(&version),
            managed: false,
        });
    }

    None
}

/// Ensure a usable uv exists, downloading the managed copy if necessary.
/// Errors in pip mode — callers must branch on `package_manager` first.
pub async fn ensure(settings: &Settings) -> crate::Result<UvInfo> {
    if settings.package_manager == PackageManager::Pip {
        bail!("uv is disabled in pip mode (package_manager = \"pip\")");
    }
    if let Some(info) = detect(settings).await {
        return Ok(info);
    }
    download_managed(Platform::current()).await
}

/// Download, extract, and verify the standalone uv for `platform`.
async fn download_managed(platform: Platform) -> crate::Result<UvInfo> {
    let asset_name = uv_asset_name(platform);
    let url = github_asset_url("astral-sh/uv", &asset_name)
        .await
        .with_context(|| format!("locating uv release asset `{asset_name}`"))?;

    let bytes = http_get_bytes(&url)
        .await
        .with_context(|| format!("downloading uv from {url}"))?;

    let dest = platform::data_dir().join("uv");
    tokio::fs::create_dir_all(&dest).await.ok();

    if asset_name.ends_with(".zip") {
        extract_zip(&bytes, &dest)?;
    } else {
        extract_tar_gz(&bytes, &dest)?;
    }

    let binary = locate_binary(&dest).context("uv binary not found in extracted archive")?;
    #[cfg(unix)]
    make_executable(&binary)?;

    // Verify before trusting it.
    let version = command::probe_version(&binary, &["--version"])
        .await
        .context("downloaded uv failed `uv --version` verification")?;

    // Normalize into the canonical managed path.
    let canonical = managed_uv_path();
    if binary != canonical {
        if let Some(parent) = canonical.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        tokio::fs::copy(&binary, &canonical).await.ok();
        #[cfg(unix)]
        make_executable(&canonical)?;
    }

    Ok(UvInfo {
        path: canonical,
        version: clean_version(&version),
        managed: true,
    })
}

/// astral-sh/uv standalone asset name for a platform.
fn uv_asset_name(platform: Platform) -> String {
    let triple = platform.uv_triple();
    if platform.os == platform::Os::Windows {
        format!("uv-{triple}.zip")
    } else {
        format!("uv-{triple}.tar.gz")
    }
}

// --- install operations -----------------------------------------------------

/// `uv python install <X.Y>` — fetch a CPython if the machine lacks it.
pub async fn python_install(uv: &UvInfo, version: PyVersion, cwd: &Path) -> crate::Result<Output> {
    command::run(
        &uv.path,
        &["python", "install", &version.to_string()],
        Some(cwd),
    )
    .await
}

/// `uv venv --clear --python <X.Y> <path>`.
///
/// `--clear` is not optional for us. Since uv 0.9 the command refuses to touch a
/// directory that already holds an environment, which would make both the
/// bootstrap and the rebuild action fail on every project that has been set up
/// once already. Replacing is what the caller means in both cases.
pub async fn create_venv(
    uv: &UvInfo,
    version: PyVersion,
    venv_path: &Path,
    cwd: &Path,
) -> crate::Result<Output> {
    command::run(&uv.path, &venv_args(version, venv_path), Some(cwd)).await
}

/// The argument vector for [`create_venv`], split out so a test can assert on it
/// without needing a uv binary present.
fn venv_args(version: PyVersion, venv_path: &Path) -> Vec<String> {
    vec![
        "venv".to_string(),
        "--clear".to_string(),
        "--python".to_string(),
        version.to_string(),
        venv_path.to_string_lossy().into_owned(),
    ]
}

/// `uv sync` (pyproject/lockfile projects).
pub async fn sync(uv: &UvInfo, cwd: &Path) -> crate::Result<Output> {
    command::run(&uv.path, &["sync"], Some(cwd)).await
}

/// `uv add <pkgs...>` — records the dependency in pyproject.toml and installs it.
/// Preferred over `uv pip install` for pyproject projects because uv edits the
/// manifest itself, preserving formatting and comments.
pub async fn add(
    uv: &UvInfo,
    packages: &[String],
    index_url: Option<&str>,
    cwd: &Path,
) -> crate::Result<Output> {
    let mut args: Vec<String> = vec!["add".into()];
    if let Some(url) = index_url {
        args.push("--index".into());
        args.push(url.to_string());
    }
    args.extend(packages.iter().cloned());
    command::run(&uv.path, &args, Some(cwd)).await
}

/// `uv pip install <pkgs...>` into the project venv.
///
/// `index_url` overrides the default index for this call only — used to point
/// a torch/tensorflow install at the CUDA build matched to the machine's GPU
/// driver (see [`crate::matrix::solve`]), without affecting any other package.
pub async fn pip_install(
    uv: &UvInfo,
    packages: &[String],
    index_url: Option<&str>,
    cwd: &Path,
) -> crate::Result<Output> {
    let mut args: Vec<String> = vec!["pip".into(), "install".into()];
    if let Some(url) = index_url {
        args.push("--index-url".into());
        args.push(url.to_string());
    }
    args.extend(packages.iter().cloned());
    command::run(&uv.path, &args, Some(cwd)).await
}

/// `uv pip install -r requirements.txt`.
pub async fn pip_install_requirements(
    uv: &UvInfo,
    requirements: &Path,
    cwd: &Path,
) -> crate::Result<Output> {
    let req = requirements.to_string_lossy().into_owned();
    command::run(&uv.path, &["pip", "install", "-r", &req], Some(cwd)).await
}

/// `uv pip install --dry-run --system --python <X.Y> <pkgs...>` — fast
/// resolvability check against a real interpreter, without needing a venv.
///
/// `--system` matters: without it uv refuses with "No virtual environment
/// found" when no `.venv` exists yet, which is exactly the situation this
/// confirmation is most useful in (before committing to a full setup). It
/// still requires `version` to be an interpreter uv can actually find on the
/// machine — callers should only call this once the target is known to exist,
/// e.g. from [`crate::core::probe::Probes::interpreters`].
pub async fn dry_run_resolve(
    uv: &UvInfo,
    version: PyVersion,
    packages: &[String],
    cwd: &Path,
) -> crate::Result<Output> {
    let mut args: Vec<String> = vec![
        "pip".into(),
        "install".into(),
        "--dry-run".into(),
        "--system".into(),
        "--python".into(),
        version.to_string(),
    ];
    args.extend(packages.iter().cloned());
    command::run(&uv.path, &args, Some(cwd)).await
}

// --- helpers ----------------------------------------------------------------

fn clean_version(raw: &str) -> String {
    // "uv 0.4.20 (abc 2024-01-01)" -> "0.4.20"
    raw.trim()
        .strip_prefix("uv")
        .unwrap_or(raw)
        .split_whitespace()
        .next()
        .unwrap_or(raw)
        .to_string()
}

#[derive(Deserialize)]
struct GhRelease {
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// Resolve a release asset's download URL via the GitHub API (latest release).
async fn github_asset_url(repo: &str, asset_name: &str) -> crate::Result<String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let client = http_client()?;
    let release: GhRelease = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    release
        .assets
        .into_iter()
        .find(|a| a.name == asset_name)
        .map(|a| a.browser_download_url)
        .ok_or_else(|| anyhow::anyhow!("no asset `{asset_name}` in {repo} latest release"))
}

async fn http_get_bytes(url: &str) -> crate::Result<Vec<u8>> {
    let client = http_client()?;
    let resp = client.get(url).send().await?.error_for_status()?;
    Ok(resp.bytes().await?.to_vec())
}

fn http_client() -> crate::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("pypilot/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(Into::into)
}

fn extract_tar_gz(bytes: &[u8], dest: &Path) -> crate::Result<()> {
    let decoder = flate2::read::GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(dest).context("unpacking uv tar.gz")?;
    Ok(())
}

fn extract_zip(bytes: &[u8], dest: &Path) -> crate::Result<()> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("opening uv zip")?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let Some(rel) = file.enclosed_name() else {
            continue;
        };
        let out = dest.join(rel);
        if file.is_dir() {
            std::fs::create_dir_all(&out)?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut writer = std::fs::File::create(&out)?;
            std::io::copy(&mut file, &mut writer)?;
        }
    }
    Ok(())
}

/// Find the `uv`/`uv.exe` binary anywhere under `dir` (archives may nest it).
fn locate_binary(dir: &Path) -> Option<PathBuf> {
    let target = if cfg!(windows) { "uv.exe" } else { "uv" };
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|n| n == target) {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(unix)]
fn make_executable(path: &Path) -> crate::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::platform::{Arch, Os};

    #[test]
    fn asset_names_per_platform() {
        assert_eq!(
            uv_asset_name(Platform {
                os: Os::Linux,
                arch: Arch::X86_64
            }),
            "uv-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            uv_asset_name(Platform {
                os: Os::Macos,
                arch: Arch::Aarch64
            }),
            "uv-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            uv_asset_name(Platform {
                os: Os::Windows,
                arch: Arch::X86_64
            }),
            "uv-x86_64-pc-windows-msvc.zip"
        );
    }

    #[tokio::test]
    async fn create_venv_passes_clear() {
        // Regression guard. Without --clear, `uv venv` errors out on any project
        // that already has an environment, which breaks both `setup` on a second
        // run and every rebuild action.
        let uv = UvInfo {
            path: PathBuf::from("uv-does-not-exist-here"),
            version: "0.0.0".into(),
            managed: false,
        };
        // The spawn fails (no such binary), but the arg vector is what matters,
        // so assert on the command we would have run.
        let args = venv_args(PyVersion::py3(12), Path::new(".venv"));
        assert!(
            args.iter().any(|a| a == "--clear"),
            "uv venv must replace an existing environment, got {args:?}"
        );
        assert!(
            create_venv(&uv, PyVersion::py3(12), Path::new(".venv"), Path::new("."))
                .await
                .is_err()
        );
    }

    #[test]
    fn version_is_cleaned() {
        assert_eq!(clean_version("uv 0.4.20 (abcd 2024-09-01)"), "0.4.20");
        assert_eq!(clean_version("0.5.0"), "0.5.0");
    }

    #[tokio::test]
    async fn pip_mode_disables_uv() {
        let s = Settings {
            package_manager: PackageManager::Pip,
            ..Settings::default()
        };
        assert!(detect(&s).await.is_none());
        assert!(ensure(&s).await.is_err());
    }
}
