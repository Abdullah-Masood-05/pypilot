//! F4 end to end: buffer text in, editor diagnostics out, using recorded PyPI
//! fixtures so nothing here touches the network.
//!
//! The platform is pinned to Linux x86-64 throughout. Wheel availability is
//! per platform, and the mediapipe fixture ships a narrower set of Windows
//! wheels than Linux ones, so an unpinned test would assert different ranges
//! depending on which machine ran it.

use std::path::PathBuf;

use pypilot::core::guardian::{analyze_buffer, Problem};
use pypilot::core::installed::Installed;
use pypilot::core::platform::{Arch, Os, Platform};
use pypilot::pypi::pyversion::PyVersion;
use pypilot::pypi::FixtureSource;

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

fn source() -> FixtureSource {
    let mut src = FixtureSource::new();
    src.load_json("mediapipe", fixture_path("mediapipe.json"))
        .unwrap();
    src.load_json("numpy", fixture_path("numpy.json")).unwrap();
    src
}

/// A directory with no Python files, so nothing is mistaken for a local module.
fn empty_workspace() -> PathBuf {
    let dir = std::env::temp_dir().join("pypilot-guardian-empty");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The headline case. mediapipe ships no cp313 wheel, so importing it on a 3.13
/// venv must be reported against the interpreter, not as a missing package.
#[tokio::test]
async fn incompatible_import_names_the_version_and_the_fix() {
    let buffer = "import os\nimport mediapipe\n";
    let installed = Installed::default();

    let findings = analyze_buffer(
        buffer,
        &empty_workspace(),
        linux(),
        Some(PyVersion::py3(13)),
        &installed,
        &source(),
    )
    .await;

    // `os` is stdlib and must never reach PyPI or the editor.
    assert_eq!(findings.len(), 1, "only mediapipe should be reported");
    let f = &findings[0];
    assert_eq!(f.package, "mediapipe");
    assert_eq!(f.import.line, 1);

    match &f.problem {
        Problem::IncompatibleInterpreter {
            current,
            target,
            supported,
            ..
        } => {
            assert_eq!(*current, PyVersion::py3(13));
            assert_eq!(*target, Some(PyVersion::py3(12)));
            assert_eq!(supported.to_range_string(), "3.9–3.12");
        }
        other => panic!("expected an interpreter conflict, got {other:?}"),
    }
}

/// On a supported interpreter the same import is just missing, and the fix is a
/// plain install rather than a rebuild.
#[tokio::test]
async fn compatible_interpreter_gives_a_plain_install() {
    let findings = analyze_buffer(
        "import mediapipe\n",
        &empty_workspace(),
        linux(),
        Some(PyVersion::py3(12)),
        &Installed::default(),
        &source(),
    )
    .await;

    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].problem, Problem::NotInstalled);
}

/// Installed packages produce no diagnostics at all.
#[tokio::test]
async fn installed_packages_are_silent() {
    let installed = Installed {
        packages: ["mediapipe".to_string()].into_iter().collect(),
    };
    let findings = analyze_buffer(
        "import mediapipe\n",
        &empty_workspace(),
        linux(),
        Some(PyVersion::py3(12)),
        &installed,
        &source(),
    )
    .await;
    assert!(findings.is_empty(), "an installed package needs no fix");
}

/// With no venv, the suggested interpreter comes from every import in the file,
/// so the first environment already suits all of them. numpy alone would allow
/// 3.14; mediapipe pulls the answer down to 3.12.
#[tokio::test]
async fn fresh_project_sizes_the_venv_for_every_import() {
    let buffer = "import numpy\nimport mediapipe\n";
    let findings = analyze_buffer(
        buffer,
        &empty_workspace(),
        linux(),
        None,
        &Installed::default(),
        &source(),
    )
    .await;

    assert_eq!(findings.len(), 2);
    for f in &findings {
        assert_eq!(
            f.problem,
            Problem::NoEnvironment {
                target: Some(PyVersion::py3(12))
            },
            "{} should target the version the whole file agrees on",
            f.package
        );
    }
}

/// A module that lives next to the file is the project's own, not a package.
#[tokio::test]
async fn local_modules_are_never_reported() {
    let dir = std::env::temp_dir().join("pypilot-guardian-local");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("preprocess.py"), "").unwrap();

    let findings = analyze_buffer(
        "import preprocess\nimport mediapipe\n",
        &dir,
        linux(),
        Some(PyVersion::py3(12)),
        &Installed::default(),
        &source(),
    )
    .await;

    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].package, "mediapipe");
}

/// An unknown name gets flagged but never gets an install offer, which is the
/// typosquatting guard.
#[tokio::test]
async fn unknown_package_is_flagged_without_an_install() {
    let findings = analyze_buffer(
        "import mediapip\n",
        &empty_workspace(),
        linux(),
        Some(PyVersion::py3(12)),
        &Installed::default(),
        &source(),
    )
    .await;

    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].problem, Problem::NotOnPyPi);
}
