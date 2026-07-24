//! End-to-end tests over the F3 engine and the decision logic, driven entirely
//! by recorded PyPI fixtures — no live network, ever.
//!
//! Platform is pinned to Linux/x86_64 in these tests so wheel-tag filtering is
//! deterministic regardless of the CI runner's OS.

use std::path::PathBuf;

use pypilot::core::interpreter::Interpreter;
use pypilot::core::pip;
use pypilot::core::platform::{Arch, Os, Platform};
use pypilot::core::project::ProjectDeps;
use pypilot::core::solver::{synthesize, EnvSummary};
use pypilot::core::{FixKind, Severity};
use pypilot::pypi::pyversion::PyVersion;
use pypilot::pypi::{analyze, FixtureSource};

fn linux() -> Platform {
    Platform {
        os: Os::Linux,
        arch: Arch::X86_64,
    }
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn mediapipe_source() -> FixtureSource {
    let mut src = FixtureSource::new();
    src.load_json("mediapipe", fixture_path("mediapipe.json"))
        .unwrap();
    src.load_json("numpy", fixture_path("numpy.json")).unwrap();
    src
}

fn python_project(pkgs: &[&str]) -> ProjectDeps {
    ProjectDeps {
        sources: vec!["requirements.txt".into()],
        packages: pkgs.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

/// The headline case: mediapipe ships cp39–cp312 wheels only. On Python 3.13 the
/// engine must diagnose the incompatibility and suggest 3.12.
#[tokio::test]
async fn mediapipe_on_313_is_diagnosed_and_312_suggested() {
    let src = mediapipe_source();
    let report = analyze(&src, linux(), &["mediapipe".to_string()]).await;

    // Supported set is exactly 3.9–3.12.
    assert_eq!(report.intersection.to_range_string(), "3.9–3.12");
    assert_eq!(report.suggested_python(), Some(PyVersion::py3(12)));

    // 3.13 is not compatible, and mediapipe is named as the blocker.
    assert!(!report.is_compatible(PyVersion::py3(13)));
    let blockers = report.blockers_for(PyVersion::py3(13));
    assert!(blockers.iter().any(|p| p.name == "mediapipe"));

    // Now feed it to the decision logic with a 3.13 venv → recreate on 3.12.
    let env = EnvSummary {
        has_venv: true,
        venv_python: Some(PyVersion::py3(13)),
        uv_present: true,
        interpreters: vec![PyVersion::py3(13)],
    };
    let (target, findings) = synthesize(&env, &python_project(&["mediapipe"]), Some(&report));
    assert_eq!(target, Some(PyVersion::py3(12)));
    let err = findings
        .iter()
        .find(|f| f.severity == Severity::Error)
        .unwrap();
    assert_eq!(err.fix, FixKind::RecreateWithPython(PyVersion::py3(12)));
    assert!(err.detail.contains("mediapipe"));
    assert!(err.detail.contains("3.13"));
}

/// A broadly-compatible dep (numpy, cp39–cp313) does not lift mediapipe's cap:
/// the intersection is still 3.9–3.12.
#[tokio::test]
async fn intersection_respects_the_tightest_dependency() {
    let src = mediapipe_source();
    let report = analyze(
        &src,
        linux(),
        &["mediapipe".to_string(), "numpy".to_string()],
    )
    .await;
    assert_eq!(report.suggested_python(), Some(PyVersion::py3(12)));
    assert!(report.is_compatible(PyVersion::py3(12)));
    assert!(!report.is_compatible(PyVersion::py3(13)));
}

/// pip-mode fallback: compatibility analysis is identical (it never touches the
/// package manager), and pip mode can only proceed when the required interpreter
/// already exists on the machine.
#[tokio::test]
async fn pip_mode_requires_preinstalled_interpreter() {
    // Same analysis as uv mode — the engine is package-manager-agnostic.
    let src = mediapipe_source();
    let report = analyze(&src, linux(), &["mediapipe".to_string()]).await;
    let target = report.suggested_python().unwrap();
    assert_eq!(target, PyVersion::py3(12));

    // pip mode: with 3.12 present it can proceed…
    let installed = vec![
        Interpreter {
            command: "python3.12".into(),
            path: "/usr/bin/python3.12".into(),
            version: PyVersion::py3(12),
        },
        Interpreter {
            command: "python3.13".into(),
            path: "/usr/bin/python3.13".into(),
            version: PyVersion::py3(13),
        },
    ];
    assert!(pip::select_interpreter(&installed, target).is_some());

    // …but with only 3.13 present, pip mode cannot fetch 3.12 → no interpreter.
    let only_313 = vec![Interpreter {
        command: "python3.13".into(),
        path: "/usr/bin/python3.13".into(),
        version: PyVersion::py3(13),
    }];
    assert!(pip::select_interpreter(&only_313, target).is_none());
}
