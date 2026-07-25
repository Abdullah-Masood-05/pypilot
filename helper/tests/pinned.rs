//! A pinned dependency must be judged on the release that will actually be
//! installed, not on whatever is newest.
//!
//! mediapipe is the case that exposed this. Unpinned it resolves to 0.10.35,
//! whose wheels are tagged `py3` and install on any CPython 3.x. Pinned to
//! 0.10.14 it resolves to a release whose wheels stop at cp312. Reading the
//! newest release for a pinned dependency therefore recommends an interpreter
//! the pin cannot install on, which is the exact failure this tool exists to
//! prevent.

use std::path::PathBuf;

use pypilot::core::platform::{Arch, Os, Platform};
use pypilot::core::project::Requirement;
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

/// The newest release under the plain name, plus the older one addressable by
/// exact version, mirroring how the real client resolves a pin.
fn source() -> FixtureSource {
    let mut src = FixtureSource::new();
    src.load_json("mediapipe", fixture_path("mediapipe-0.10.35.json"))
        .unwrap();
    src.load_json("mediapipe==0.10.14", fixture_path("mediapipe-0.10.14.json"))
        .unwrap();
    src.load_json("numpy", fixture_path("numpy.json")).unwrap();
    src
}

fn req(name: &str, spec: &str) -> Requirement {
    Requirement {
        name: name.to_string(),
        spec: spec.to_string(),
    }
}

/// Unpinned: the py3-tagged 0.10.35 really does install anywhere.
#[tokio::test]
async fn unpinned_reports_the_newest_release() {
    let report = analyze(&source(), linux(), &[Requirement::any("mediapipe")]).await;

    assert_eq!(report.per_package[0].version, "0.10.35");
    assert!(
        report.is_compatible(PyVersion::py3(14)),
        "a py3-none wheel installs on any CPython 3.x"
    );
}

/// Pinned: the answer must come from 0.10.14's wheels, which stop at 3.12.
#[tokio::test]
async fn exact_pin_is_judged_on_the_pinned_release() {
    let report = analyze(&source(), linux(), &[req("mediapipe", "==0.10.14")]).await;

    assert_eq!(
        report.per_package[0].version, "0.10.14",
        "the pinned release is what gets analyzed"
    );
    assert_eq!(report.intersection.to_range_string(), "3.9–3.12");
    assert_eq!(report.suggested_python(), Some(PyVersion::py3(12)));
    assert!(
        !report.is_compatible(PyVersion::py3(14)),
        "0.10.14 has no wheel for 3.14, so 3.14 must not be recommended"
    );
}

/// An upper bound behaves like a pin: the newest release below the cap wins.
#[tokio::test]
async fn upper_bound_resolves_below_the_cap() {
    let report = analyze(&source(), linux(), &[req("mediapipe", "<0.10.20")]).await;

    assert_eq!(report.per_package[0].version, "0.10.14");
    assert_eq!(report.suggested_python(), Some(PyVersion::py3(12)));
}

/// A lower bound leaves the newest release as the answer, so no extra lookup is
/// needed and the result matches the unpinned case.
#[tokio::test]
async fn lower_bound_keeps_the_newest_release() {
    let report = analyze(&source(), linux(), &[req("mediapipe", ">=0.10")]).await;

    assert_eq!(report.per_package[0].version, "0.10.35");
    assert!(report.is_compatible(PyVersion::py3(14)));
}

/// The pin propagates into the project-wide intersection, dragging the
/// recommendation down even though the other dependency allows 3.14.
#[tokio::test]
async fn a_pin_constrains_the_whole_project() {
    let report = analyze(
        &source(),
        linux(),
        &[req("mediapipe", "==0.10.14"), Requirement::any("numpy")],
    )
    .await;

    assert_eq!(report.suggested_python(), Some(PyVersion::py3(12)));
    let blockers = report.blockers_for(PyVersion::py3(14));
    assert!(
        blockers.iter().any(|p| p.name == "mediapipe"),
        "mediapipe must be named as the package that caps the version"
    );
}

/// A constraint nothing satisfies is reported rather than silently ignored.
#[tokio::test]
async fn impossible_pin_is_reported() {
    let report = analyze(&source(), linux(), &[req("mediapipe", "==99.0.0")]).await;

    assert!(report.per_package.is_empty());
    assert_eq!(report.unresolved.len(), 1);
    assert!(
        report.unresolved[0].1.contains("no release"),
        "got: {}",
        report.unresolved[0].1
    );
}

/// Pre-releases are not selected unless the specifier asks for one.
#[tokio::test]
async fn prerelease_is_not_picked_by_a_plain_lower_bound() {
    // 0.11.0rc1 exists in the fixture's release list but must not be chosen.
    let report = analyze(&source(), linux(), &[req("mediapipe", ">=0.10.14")]).await;
    assert_eq!(report.per_package[0].version, "0.10.35");
}
