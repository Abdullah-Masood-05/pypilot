//! Constraint solve: GPU + driver + framework release -> a concrete answer.
//!
//! One function backs two callers. [`crate::matrix::check`] uses the finding
//! for the read-only report and the F5 toast. [`crate::core::install`] uses
//! the index URL to make an actual `torch` install pull the wheel built for
//! the driver that is really in the machine, instead of whatever the default
//! index happens to hand back.

use crate::core::gpu::{self, Accelerator};
use crate::core::platform::Platform;
use crate::core::project::Requirement;
use crate::core::{Finding, FixKind, Severity};
use crate::matrix::frameworks::{CudaBuild, Framework, FrameworkTable, ReleaseInfo};
use crate::matrix::nvidia::{CudaVersion, DriverSupport, DriverTable};
use crate::matrix::refresh;
use crate::pypi::version::VersionSpec;
use crate::settings::Settings;

/// What solving one framework dependency produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FrameworkSolve {
    /// The release this was solved against: the pin's target, or the newest
    /// curated release when unpinned. `None` only when the package name is
    /// not a framework this module handles.
    pub resolved_version: Option<String>,
    /// Read-only guidance, `None` when there is nothing worth reporting.
    pub finding: Option<Finding>,
    /// `Some(url)` when the install should be pointed at a specific index —
    /// a CUDA build matched to the driver, or the smaller CPU-only index.
    /// `None` means: not a framework this module handles, tensorflow (which
    /// has no alternate index), or a version outside the bundled snapshot.
    pub index_url: Option<String>,
}

/// Solve for `package_name`, given its declared requirement (`None` when
/// unpinned). Probes the GPU itself, so this is the one entry point both
/// [`crate::matrix::check`] and the install path call. Reads the framework and
/// driver tables through [`refresh::read`], so a background-refreshed copy is
/// used automatically once one exists, per F7's `data_refresh_days`.
pub async fn solve_framework(
    package_name: &str,
    requirement: Option<&Requirement>,
    settings: &Settings,
) -> Option<FrameworkSolve> {
    let framework = Framework::from_package_name(package_name)?;
    let frameworks_json = refresh::read(
        "frameworks.json",
        crate::matrix::FRAMEWORKS_JSON,
        settings.data_refresh_days,
    );
    let table = FrameworkTable::load(&frameworks_json)?;
    let accelerator = gpu::detect().await;

    let version = resolve_version(&table, framework, requirement)?;
    let Some(release) = table.release(framework, &version) else {
        return Some(unknown_release(framework, &version));
    };

    let mut solved = match accelerator {
        Accelerator::AppleSilicon => apple_silicon(framework, &version),
        Accelerator::None => no_gpu(release),
        Accelerator::Nvidia {
            name,
            driver_version,
        } => {
            let nvidia_json = refresh::read(
                "nvidia.json",
                crate::matrix::NVIDIA_JSON,
                settings.data_refresh_days,
            );
            let driver_table = DriverTable::load(&nvidia_json);
            let os = Platform::current().os;
            let support = driver_table.and_then(|t| t.support_for(&driver_version, os));
            with_nvidia(release, &name, &driver_version, support)
        }
    };
    solved.resolved_version = Some(version);
    Some(solved)
}

/// The version to evaluate: the newest curated release matching a pin, or the
/// newest curated release at all when unpinned. Only versions the bundled
/// table has build data for are candidates, since that data is the entire
/// point of solving.
fn resolve_version(
    table: &FrameworkTable,
    framework: Framework,
    requirement: Option<&Requirement>,
) -> Option<String> {
    if let Some(req) = requirement.filter(|r| r.is_pinned()) {
        let spec = VersionSpec::parse(&req.spec);
        let versions = table.all_versions(framework);
        if let Some(picked) = spec.select_newest(versions.iter().map(|s| s.as_str())) {
            return Some(picked.raw);
        }
        // Pinned to something outside the bundled snapshot: fall through with
        // the spec text so the caller can still name what was asked for,
        // rather than going silent about a version it could not check.
        return Some(
            req.spec
                .trim_start_matches(['=', '~', '>', '<', '!', ' '])
                .to_string(),
        );
    }
    table.latest(framework).map(|s| s.to_string())
}

fn apple_silicon(framework: Framework, version: &str) -> FrameworkSolve {
    let name = framework_display_name(framework);
    FrameworkSolve {
        finding: Some(Finding {
            severity: Severity::Info,
            title: format!("{name} on Apple Silicon uses MPS"),
            detail: format!(
                "Apple Silicon detected. {name} {version} accelerates through Metal (MPS), not CUDA. The default wheel is correct; no special index is needed."
            ),
            fix: FixKind::Manual,
        }),
        index_url: None,
        ..Default::default()
    }
}

fn no_gpu(release: ReleaseInfo) -> FrameworkSolve {
    match release {
        ReleaseInfo::Torch { cpu_index_url, .. } => FrameworkSolve {
            // Silent: this is the "nothing to fix" case. The index is still
            // set, because the default index otherwise risks pulling a much
            // larger CUDA-bundled wheel on a machine that cannot use it.
            finding: None,
            index_url: Some(cpu_index_url),
            ..Default::default()
        },
        ReleaseInfo::TensorFlow { .. } => FrameworkSolve::default(),
    }
}

fn with_nvidia(
    release: ReleaseInfo,
    gpu_name: &str,
    driver_version: &str,
    support: Option<DriverSupport>,
) -> FrameworkSolve {
    match release {
        ReleaseInfo::Torch {
            version,
            cpu_index_url,
            cuda_builds,
        } => solve_torch(
            &version,
            gpu_name,
            driver_version,
            support,
            cpu_index_url,
            cuda_builds,
        ),
        ReleaseInfo::TensorFlow { version, min_cuda } => {
            solve_tensorflow(&version, gpu_name, driver_version, support, min_cuda)
        }
    }
}

fn solve_torch(
    version: &str,
    gpu_name: &str,
    driver_version: &str,
    support: Option<DriverSupport>,
    cpu_index_url: String,
    mut cuda_builds: Vec<CudaBuild>,
) -> FrameworkSolve {
    let Some(support) = support else {
        return FrameworkSolve {
            finding: Some(Finding {
                severity: Severity::Error,
                title: "NVIDIA driver too old for any known CUDA build".to_string(),
                detail: format!(
                    "{gpu_name}'s driver ({driver_version}) is older than every CUDA release PyPilot's data covers. Update the driver, or install the CPU build: pip install torch=={version} --index-url {cpu_index_url}"
                ),
                fix: FixKind::Manual,
            }),
            index_url: Some(cpu_index_url),
            ..Default::default()
        };
    };

    cuda_builds.sort_by_key(|b| b.cuda);
    let best = cuda_builds
        .iter()
        .rev()
        .find(|b| b.cuda <= support.max_known);

    let Some(best) = best else {
        // Driver supports some CUDA, just not enough for anything this torch
        // release ships. Rare (torch's floor is usually generous) but real.
        let oldest = cuda_builds.first();
        let ask = if oldest.is_some() {
            format!(
                " Upgrade the driver, or install an older torch release matching CUDA {}.",
                support.max_known
            )
        } else {
            String::new()
        };
        return FrameworkSolve {
            finding: Some(Finding {
                severity: Severity::Error,
                title: "NVIDIA driver too old for this torch release".to_string(),
                detail: format!(
                    "{gpu_name}'s driver ({driver_version}) supports CUDA up to {}. torch {version} needs at least {}.{ask}",
                    support.max_known,
                    oldest.map(|b| b.cuda.to_string()).unwrap_or_default(),
                ),
                fix: FixKind::Manual,
            }),
            index_url: Some(cpu_index_url),
            ..Default::default()
        };
    };

    let soft_note = if support.exceeds_known_table {
        " (your driver is newer than PyPilot's bundled table; this is a conservative floor, not a ceiling)"
    } else {
        ""
    };

    FrameworkSolve {
        finding: Some(Finding {
            severity: Severity::Info,
            title: format!("torch build matched to your driver: {}", best.tag),
            detail: format!(
                "{gpu_name}'s driver ({driver_version}) supports CUDA up to {}{soft_note}. Recommended: pip install torch=={version} --index-url {}",
                support.max_known, best.index_url
            ),
            fix: FixKind::Manual,
        }),
        index_url: Some(best.index_url.clone()),
        ..Default::default()
    }
}

fn solve_tensorflow(
    version: &str,
    gpu_name: &str,
    driver_version: &str,
    support: Option<DriverSupport>,
    min_cuda: CudaVersion,
) -> FrameworkSolve {
    let Some(support) = support else {
        return FrameworkSolve {
            finding: Some(Finding {
                severity: Severity::Error,
                title: "NVIDIA driver too old for any known CUDA build".to_string(),
                detail: format!(
                    "{gpu_name}'s driver ({driver_version}) is older than every CUDA release PyPilot's data covers, and tensorflow {version} needs CUDA {min_cuda}. Update the driver."
                ),
                fix: FixKind::Manual,
            }),
            ..Default::default()
        };
    };

    if support.max_known >= min_cuda {
        // Fine — silent, nothing to fix.
        return FrameworkSolve::default();
    }

    let soft_note = if support.exceeds_known_table {
        " (your driver is newer than PyPilot's bundled table; this is a conservative floor)"
    } else {
        ""
    };

    FrameworkSolve {
        finding: Some(Finding {
            severity: Severity::Error,
            title: "NVIDIA driver too old for this tensorflow release".to_string(),
            detail: format!(
                "{gpu_name}'s driver ({driver_version}) supports CUDA up to {}{soft_note}. tensorflow {version} needs CUDA {min_cuda} or newer (minimum driver {}). Update the driver, or pin an older tensorflow release.",
                support.max_known, support.min_driver_for_max
            ),
            fix: FixKind::Manual,
        }),
        ..Default::default()
    }
}

fn unknown_release(framework: Framework, version: &str) -> FrameworkSolve {
    let name = framework_display_name(framework);
    FrameworkSolve {
        resolved_version: Some(version.to_string()),
        finding: Some(Finding {
            severity: Severity::Info,
            title: format!("{name} {version} is not in PyPilot's bundled CUDA data"),
            detail: format!(
                "{name} {version} is newer than PyPilot's bundled matrix, so its driver compatibility could not be checked here. See pytorch.org/get-started/locally or tensorflow.org/install for the current index URL."
            ),
            fix: FixKind::Manual,
        }),
        index_url: None,
    }
}

fn framework_display_name(f: Framework) -> &'static str {
    match f {
        Framework::Torch => "torch",
        Framework::TensorFlow => "tensorflow",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> FrameworkTable {
        FrameworkTable::load(crate::matrix::FRAMEWORKS_JSON).unwrap()
    }

    fn torch_2_4() -> (String, String, Vec<CudaBuild>) {
        let ReleaseInfo::Torch {
            version,
            cpu_index_url,
            cuda_builds,
        } = table().release(Framework::Torch, "2.4.0").unwrap()
        else {
            unreachable!()
        };
        (version, cpu_index_url, cuda_builds)
    }

    #[test]
    fn driver_supporting_cu121_but_not_cu124_gets_cu121() {
        // torch 2.4.0 offers cu118/cu121/cu124. A driver capped at CUDA 12.2
        // (real GA floor for 12.2 is 535.54.03) can't run cu124.
        let (version, cpu_index_url, cuda_builds) = torch_2_4();
        let support = DriverSupport {
            max_known: CudaVersion {
                major: 12,
                minor: 2,
            },
            min_driver_for_max: "535.54.03".into(),
            exceeds_known_table: false,
        };
        let solved = solve_torch(
            &version,
            "RTX 3080",
            "535.54.03",
            Some(support),
            cpu_index_url,
            cuda_builds,
        );
        assert_eq!(
            solved.index_url.as_deref(),
            Some("https://download.pytorch.org/whl/cu121")
        );
        assert_eq!(solved.finding.unwrap().severity, Severity::Info);
    }

    #[test]
    fn driver_supporting_the_newest_build_is_offered_it() {
        let (version, cpu_index_url, cuda_builds) = torch_2_4();
        let support = DriverSupport {
            max_known: CudaVersion {
                major: 12,
                minor: 6,
            },
            min_driver_for_max: "560.28.03".into(),
            exceeds_known_table: false,
        };
        let solved = solve_torch(
            &version,
            "RTX 4090",
            "560.28.03",
            Some(support),
            cpu_index_url,
            cuda_builds,
        );
        assert_eq!(
            solved.index_url.as_deref(),
            Some("https://download.pytorch.org/whl/cu124")
        );
    }

    #[test]
    fn no_driver_data_falls_back_to_cpu_with_an_error() {
        let (version, cpu_index_url, cuda_builds) = torch_2_4();
        let solved = solve_torch(
            &version,
            "GT 210",
            "195.36.15",
            None,
            cpu_index_url.clone(),
            cuda_builds,
        );
        assert_eq!(solved.index_url, Some(cpu_index_url));
        assert_eq!(solved.finding.unwrap().severity, Severity::Error);
    }

    #[test]
    fn tensorflow_within_driver_capability_is_silent() {
        let support = DriverSupport {
            max_known: CudaVersion {
                major: 12,
                minor: 5,
            },
            min_driver_for_max: "555.42.02".into(),
            exceeds_known_table: false,
        };
        let solved = solve_tensorflow(
            "2.16.1",
            "RTX 4090",
            "555.42.02",
            Some(support),
            CudaVersion {
                major: 12,
                minor: 3,
            },
        );
        assert!(solved.finding.is_none());
        assert!(solved.index_url.is_none());
    }

    #[test]
    fn tensorflow_needing_newer_cuda_than_the_driver_has_is_an_error() {
        let support = DriverSupport {
            max_known: CudaVersion {
                major: 11,
                minor: 8,
            },
            min_driver_for_max: "520.61.05".into(),
            exceeds_known_table: false,
        };
        let solved = solve_tensorflow(
            "2.16.1",
            "GTX 1080",
            "520.61.05",
            Some(support),
            CudaVersion {
                major: 12,
                minor: 3,
            },
        );
        let f = solved.finding.unwrap();
        assert_eq!(f.severity, Severity::Error);
        assert!(f.detail.contains("12.3"));
    }

    #[test]
    fn soft_note_appears_when_driver_exceeds_the_table() {
        let (version, cpu_index_url, cuda_builds) = torch_2_4();
        let support = DriverSupport {
            max_known: CudaVersion {
                major: 12,
                minor: 9,
            },
            min_driver_for_max: "575.51.03".into(),
            exceeds_known_table: true,
        };
        let solved = solve_torch(
            &version,
            "RTX 5090",
            "999.00",
            Some(support),
            cpu_index_url,
            cuda_builds,
        );
        assert!(solved
            .finding
            .unwrap()
            .detail
            .contains("conservative floor"));
    }

    #[tokio::test]
    async fn non_framework_package_is_not_handled() {
        assert!(solve_framework("numpy", None, &Settings::default())
            .await
            .is_none());
    }

    #[tokio::test]
    async fn resolved_version_is_set_on_the_returned_solve() {
        // Whatever the machine's actual accelerator is, an unpinned torch
        // request must resolve to the table's newest curated release.
        let solved = solve_framework("torch", None, &Settings::default())
            .await
            .unwrap();
        let table = table();
        assert_eq!(
            solved.resolved_version.as_deref(),
            table.latest(Framework::Torch)
        );
    }
}
