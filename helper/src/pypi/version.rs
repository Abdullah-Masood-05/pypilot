//! Package versions and the specifiers that select them.
//!
//! A dependency line pins a *release*, and the release decides which Python
//! versions are usable. `mediapipe==0.10.14` ships wheels for CPython 3.9 to
//! 3.12; `mediapipe` unpinned resolves to 0.10.35, whose wheels are tagged
//! `py3` and install anywhere. Reading only the newest release therefore gives
//! the wrong answer for any project that pins or caps a dependency.
//!
//! Comparison here is on the release segments only. Epochs, post-releases and
//! local versions do not change which release an installer picks in the cases
//! that matter to us, and pre-releases are excluded from selection because
//! installers skip them unless asked.

use std::cmp::Ordering;

use crate::pypi::pyversion::Clause;

/// A PyPI release version, comparable by release segments.
#[derive(Debug, Clone)]
pub struct PackageVersion {
    /// The version exactly as PyPI spells it, for display and for URLs.
    pub raw: String,
    release: Vec<u64>,
    prerelease: bool,
}

impl PackageVersion {
    /// Parse a version string. Returns `None` when there are no numeric
    /// segments to compare, which rules out anything we could order.
    pub fn parse(raw: &str) -> Option<PackageVersion> {
        let text = raw.trim();
        if text.is_empty() {
            return None;
        }

        // Drop an epoch prefix ("1!2.0") and any local segment ("1.0+cpu").
        let without_epoch = text.split_once('!').map(|(_, r)| r).unwrap_or(text);
        let core = without_epoch.split('+').next().unwrap_or(without_epoch);

        // Release segments run until the first character that is not a digit or
        // a dot. Everything after that is a pre/post/dev suffix.
        let split_at = core
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(core.len());
        let (numeric, suffix) = core.split_at(split_at);

        let release: Vec<u64> = numeric
            .split('.')
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        if release.is_empty() {
            return None;
        }

        let tail = suffix.trim_start_matches('.').to_ascii_lowercase();
        let prerelease = ["a", "b", "c", "rc", "alpha", "beta", "pre", "dev"]
            .iter()
            .any(|marker| tail.starts_with(marker));

        Some(PackageVersion {
            raw: text.to_string(),
            release,
            prerelease,
        })
    }

    /// Alphas, betas, release candidates and dev builds. Installers ignore these
    /// unless a specifier names one, so selection does too.
    pub fn is_prerelease(&self) -> bool {
        self.prerelease
    }
}

impl Ord for PackageVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let n = self.release.len().max(other.release.len());
        for i in 0..n {
            let a = self.release.get(i).copied().unwrap_or(0);
            let b = other.release.get(i).copied().unwrap_or(0);
            match a.cmp(&b) {
                Ordering::Equal => continue,
                other => return other,
            }
        }
        // A release outranks its own pre-releases: 1.0 is newer than 1.0rc1.
        other.prerelease.cmp(&self.prerelease)
    }
}

impl PartialOrd for PackageVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// Equality has to agree with the ordering, or `Ord`'s contract is broken.
// Deriving it would compare the raw strings and call 1.0 different from 1.0.0,
// while `cmp` zero-pads and calls them equal.
impl PartialEq for PackageVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for PackageVersion {}

/// The version constraint from a dependency line, such as `>=1.2,<2`.
#[derive(Default)]
pub struct VersionSpec {
    clauses: Vec<Clause>,
    /// True when the specifier names a pre-release, which allows selecting one.
    allows_prerelease: bool,
}

impl VersionSpec {
    /// Parse the specifier portion of a requirement. An empty string means the
    /// dependency is unconstrained.
    pub fn parse(spec: &str) -> VersionSpec {
        let spec = spec.trim();
        if spec.is_empty() {
            return VersionSpec::default();
        }

        let allows_prerelease = spec
            .split(',')
            .filter_map(|c| PackageVersion::parse(strip_operator(c.trim())))
            .any(|v| v.is_prerelease());

        VersionSpec {
            clauses: spec
                .split(',')
                .filter_map(|c| Clause::parse(c.trim()))
                .collect(),
            allows_prerelease,
        }
    }

    /// No constraint at all, so the newest release is the right one.
    pub fn is_unconstrained(&self) -> bool {
        self.clauses.is_empty()
    }

    pub fn matches(&self, version: &PackageVersion) -> bool {
        if version.is_prerelease() && !self.allows_prerelease {
            return false;
        }
        self.clauses.iter().all(|c| c.matches(&version.release))
    }

    /// The newest release satisfying this specifier, which is what an installer
    /// would pick.
    pub fn select_newest<'a, I>(&self, versions: I) -> Option<PackageVersion>
    where
        I: IntoIterator<Item = &'a str>,
    {
        versions
            .into_iter()
            .filter_map(PackageVersion::parse)
            .filter(|v| self.matches(v))
            .max()
    }
}

/// Remove a leading comparison operator so the version can be parsed.
fn strip_operator(clause: &str) -> &str {
    clause.trim_start_matches(['>', '<', '=', '!', '~', ' '])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> PackageVersion {
        PackageVersion::parse(s).unwrap()
    }

    #[test]
    fn orders_by_release_segments() {
        assert!(v("0.10.35") > v("0.10.14"));
        // Numeric, not lexicographic: "10" sorts above "2" here.
        assert!(v("0.10.10") > v("0.10.2"));
        assert!(v("1.0") < v("1.0.1"));
        assert!(v("2.0") > v("1.99.99"));
    }

    #[test]
    fn zero_pads_shorter_versions() {
        assert_eq!(v("1.0.0"), v("1.0"));
        assert_eq!(v("1.0.0").cmp(&v("1.0")), Ordering::Equal);
    }

    #[test]
    fn detects_prereleases() {
        assert!(v("1.0.0rc1").is_prerelease());
        assert!(v("2.0b3").is_prerelease());
        assert!(v("1.0.dev1").is_prerelease());
        assert!(!v("1.0.0").is_prerelease());
        assert!(!v("1.0.post1").is_prerelease());
    }

    #[test]
    fn releases_outrank_their_prereleases() {
        assert!(v("1.0.0") > v("1.0.0rc1"));
    }

    #[test]
    fn handles_epochs_and_local_versions() {
        assert_eq!(v("1!2.0").release, vec![2, 0]);
        assert_eq!(v("2.4.0+cpu").release, vec![2, 4, 0]);
    }

    #[test]
    fn exact_pin_selects_that_release() {
        let spec = VersionSpec::parse("==0.10.14");
        let picked = spec
            .select_newest(["0.10.9", "0.10.14", "0.10.35"])
            .unwrap();
        assert_eq!(picked.raw, "0.10.14");
    }

    #[test]
    fn upper_bound_selects_newest_below_it() {
        let spec = VersionSpec::parse(">=0.10,<0.11");
        let picked = spec
            .select_newest(["0.9.1", "0.10.9", "0.10.14", "0.11.0", "1.0.0"])
            .unwrap();
        assert_eq!(picked.raw, "0.10.14");
    }

    #[test]
    fn unconstrained_takes_the_newest() {
        let spec = VersionSpec::parse("");
        assert!(spec.is_unconstrained());
        let picked = spec.select_newest(["0.10.14", "0.10.35", "0.9.0"]).unwrap();
        assert_eq!(picked.raw, "0.10.35");
    }

    #[test]
    fn lower_bound_still_takes_the_newest() {
        let spec = VersionSpec::parse(">=0.10");
        let picked = spec.select_newest(["0.9.0", "0.10.14", "0.10.35"]).unwrap();
        assert_eq!(picked.raw, "0.10.35");
    }

    #[test]
    fn compatible_release_caps_the_minor() {
        // ~=0.10.0 means >=0.10.0,<0.11
        let spec = VersionSpec::parse("~=0.10.0");
        let picked = spec
            .select_newest(["0.10.14", "0.10.35", "0.11.0"])
            .unwrap();
        assert_eq!(picked.raw, "0.10.35");
    }

    #[test]
    fn prereleases_are_skipped_unless_requested() {
        let spec = VersionSpec::parse(">=1.0");
        let picked = spec.select_newest(["1.0.0", "1.1.0rc1"]).unwrap();
        assert_eq!(picked.raw, "1.0.0", "an rc should not win by default");

        let explicit = VersionSpec::parse("==1.1.0rc1");
        let picked = explicit.select_newest(["1.0.0", "1.1.0rc1"]).unwrap();
        assert_eq!(picked.raw, "1.1.0rc1");
    }

    #[test]
    fn impossible_constraint_selects_nothing() {
        let spec = VersionSpec::parse(">=99.0");
        assert!(spec.select_newest(["1.0.0", "2.0.0"]).is_none());
    }
}
