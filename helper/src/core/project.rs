//! Project dependency parsing.
//!
//! Detects and parses the Python signal files in a workspace and normalizes their
//! dependencies to bare PyPI package names (versions/markers/extras stripped),
//! which is all the F3 engine needs. Conda `environment.yml` is detected and
//! flagged (per plan we don't fight conda in v1, only offer migration later).

use std::path::{Path, PathBuf};

/// A dependency as the project declares it: a name plus whatever version
/// constraint came with it.
///
/// The constraint matters. `mediapipe` resolves to the newest release, while
/// `mediapipe==0.10.14` resolves to one that supports a narrower set of Python
/// versions, so dropping the specifier produces a confidently wrong answer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Requirement {
    /// Normalized per PEP 503.
    pub name: String,
    /// The specifier as written, e.g. `==0.10.14`. Empty when unconstrained.
    pub spec: String,
}

impl Requirement {
    /// A dependency with no version constraint.
    pub fn any(name: impl Into<String>) -> Requirement {
        Requirement {
            name: name.into(),
            spec: String::new(),
        }
    }

    pub fn is_pinned(&self) -> bool {
        !self.spec.is_empty()
    }
}

impl std::fmt::Display for Requirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.name, self.spec)
    }
}

/// What we learned from a workspace's project files.
#[derive(Debug, Clone, Default)]
pub struct ProjectDeps {
    /// Signal files that were found and parsed.
    pub sources: Vec<PathBuf>,
    /// Declared dependencies, deduped and normalized.
    pub packages: Vec<Requirement>,
    /// `requires-python` from pyproject, if declared.
    pub declared_requires_python: Option<String>,
    /// True when an `environment.yml` is present.
    pub has_conda_env: bool,
    /// Python pin parsed from a conda `environment.yml`, if any.
    pub conda_python: Option<String>,
}

impl ProjectDeps {
    pub fn is_python_project(&self) -> bool {
        !self.sources.is_empty()
    }

    /// Just the names, for display and for callers that don't care about pins.
    pub fn names(&self) -> Vec<String> {
        self.packages.iter().map(|r| r.name.clone()).collect()
    }
}

/// Scan a workspace directory (non-recursively at the root) for project files.
pub fn scan(workspace: &Path) -> ProjectDeps {
    let mut deps = ProjectDeps::default();
    let mut names = Vec::new();

    let pyproject = workspace.join("pyproject.toml");
    if pyproject.is_file() {
        if let Ok(text) = std::fs::read_to_string(&pyproject) {
            let (pkgs, rp) = parse_pyproject(&text);
            names.extend(pkgs);
            deps.declared_requires_python = rp;
            deps.sources.push(pyproject);
        }
    }

    let requirements = workspace.join("requirements.txt");
    if requirements.is_file() {
        if let Ok(text) = std::fs::read_to_string(&requirements) {
            names.extend(parse_requirements(&text));
            deps.sources.push(requirements);
        }
    }

    let env_yml = workspace.join("environment.yml");
    if env_yml.is_file() {
        if let Ok(text) = std::fs::read_to_string(&env_yml) {
            let (pkgs, py) = parse_environment_yml(&text);
            names.extend(pkgs);
            deps.conda_python = py;
        }
        deps.has_conda_env = true;
        deps.sources.push(env_yml);
    }

    // A bare `.py` file also makes this a Python project even with no manifest.
    if deps.sources.is_empty() && has_python_file(workspace) {
        deps.sources.push(workspace.join("*.py"));
    }

    deps.packages = dedupe(names);
    deps
}

/// Normalize a PyPI project name (PEP 503): lowercase, runs of `-_.` collapse to `-`.
pub fn normalize_name(name: &str) -> String {
    let lowered = name.trim().to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut prev_sep = false;
    for c in lowered.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !prev_sep && !out.is_empty() {
                out.push('-');
            }
            prev_sep = true;
        } else {
            out.push(c);
            prev_sep = false;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Parse one dependency line into a name and its version specifier.
///
/// Environment markers and extras are dropped: neither changes which release an
/// installer picks for the current machine. The version specifier is kept,
/// because it does.
pub fn parse_requirement(line: &str) -> Option<Requirement> {
    let name = requirement_name(line)?;

    let line = line.trim();
    // Cut the marker first so `; python_version<'3.10'` is not read as a bound.
    let without_marker = line.split(';').next().unwrap_or(line);
    // Then drop extras, whose brackets can contain commas.
    let without_extras = match (without_marker.find('['), without_marker.find(']')) {
        (Some(open), Some(close)) if close > open => {
            format!(
                "{}{}",
                &without_marker[..open],
                &without_marker[close + 1..]
            )
        }
        _ => without_marker.to_string(),
    };

    // The specifier begins at the first comparison character.
    let spec = match without_extras.find(['=', '<', '>', '!', '~']) {
        Some(i) => without_extras[i..]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect(),
        None => String::new(),
    };

    Some(Requirement { name, spec })
}

/// Extract the bare package name from a single requirement specifier line.
/// Returns `None` for comments, options, includes, blank lines, and URLs.
pub fn requirement_name(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
        return None;
    }
    // Strip an inline comment (only when preceded by whitespace, per pip rules).
    let line = match line.find(" #") {
        Some(i) => &line[..i],
        None => line,
    };
    // Direct URL / VCS references: "name @ url" keeps the name; bare URLs are skipped.
    if let Some(idx) = line.find('@') {
        let name = line[..idx].trim();
        if !name.is_empty()
            && name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
        {
            return Some(normalize_name(strip_extras(name)));
        }
        return None;
    }
    if line.contains("://") {
        return None;
    }
    // Cut at the first version/marker/extras delimiter.
    let end = line
        .find(['=', '<', '>', '!', '~', ';', '[', ' ', '('])
        .unwrap_or(line.len());
    let name = line[..end].trim();
    if name.is_empty() {
        return None;
    }
    Some(normalize_name(name))
}

fn strip_extras(name: &str) -> &str {
    match name.find('[') {
        Some(i) => &name[..i],
        None => name,
    }
}

fn parse_requirements(text: &str) -> Vec<Requirement> {
    text.lines().filter_map(parse_requirement).collect()
}

/// Parse dependencies + requires-python from pyproject.toml (PEP 621 and Poetry).
fn parse_pyproject(text: &str) -> (Vec<Requirement>, Option<String>) {
    let mut names = Vec::new();
    let mut requires_python = None;

    let Ok(doc) = toml::from_str::<toml::Value>(text) else {
        return (names, requires_python);
    };

    // PEP 621: [project]
    if let Some(project) = doc.get("project").and_then(|v| v.as_table()) {
        if let Some(rp) = project.get("requires-python").and_then(|v| v.as_str()) {
            requires_python = Some(rp.to_string());
        }
        if let Some(deps) = project.get("dependencies").and_then(|v| v.as_array()) {
            for d in deps.iter().filter_map(|v| v.as_str()) {
                if let Some(r) = parse_requirement(d) {
                    names.push(r);
                }
            }
        }
        // Optional-dependencies groups.
        if let Some(opt) = project
            .get("optional-dependencies")
            .and_then(|v| v.as_table())
        {
            for group in opt.values().filter_map(|v| v.as_array()) {
                for d in group.iter().filter_map(|v| v.as_str()) {
                    if let Some(r) = parse_requirement(d) {
                        names.push(r);
                    }
                }
            }
        }
    }

    // Poetry: [tool.poetry.dependencies] — keys are names; a "python" key is the
    // interpreter constraint, not a package.
    if let Some(poetry) = doc
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_table())
    {
        for key in poetry.keys() {
            if key.eq_ignore_ascii_case("python") {
                if requires_python.is_none() {
                    if let Some(rp) = poetry.get(key).and_then(|v| v.as_str()) {
                        requires_python = Some(caret_to_pep440(rp));
                    }
                }
                continue;
            }
            let spec = poetry
                .get(key)
                .and_then(|v| v.as_str())
                .map(caret_to_pep440)
                .filter(|s| !s.is_empty() && s != "*")
                .unwrap_or_default();
            names.push(Requirement {
                name: normalize_name(key),
                spec,
            });
        }
    }

    (names, requires_python)
}

/// Best-effort conda environment.yml: dependency names + the `python=` pin.
fn parse_environment_yml(text: &str) -> (Vec<Requirement>, Option<String>) {
    let mut names = Vec::new();
    let mut python = None;
    let mut in_deps = false;

    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("dependencies:") {
            in_deps = true;
            continue;
        }
        if in_deps {
            // A new top-level key ends the deps block.
            if !raw.starts_with(' ') && !line.starts_with('-') && line.ends_with(':') {
                in_deps = false;
                continue;
            }
            if let Some(item) = line.strip_prefix('-') {
                let item = item.trim();
                if item.ends_with(':') {
                    // e.g. "pip:" sub-block; keep scanning its children as deps.
                    continue;
                }
                // "python=3.10" or "numpy>=1.20".
                let name_part = item
                    .split(['=', '<', '>', '!', '~', ' '])
                    .next()
                    .unwrap_or("")
                    .trim();
                if name_part.eq_ignore_ascii_case("python") {
                    python = item
                        .split('=')
                        .nth(1)
                        .map(|v| v.trim().trim_matches('=').to_string())
                        .filter(|s| !s.is_empty());
                } else if !name_part.is_empty() {
                    names.push(Requirement {
                        name: normalize_name(name_part),
                        spec: conda_spec_to_pep440(&item[name_part.len()..]),
                    });
                }
            }
        }
    }
    (names, python)
}

/// Convert the version part of a conda dependency line into PEP 440.
///
/// conda's grammar overlaps with PEP 440 but is not identical:
///   * a single `=` is a *prefix* match (`numpy=1.24` allows 1.24.1), which
///     PEP 440 spells `==1.24.*`;
///   * a third field is a build string (`numpy=1.24=py311h5a2b...`), which has
///     no PyPI equivalent and is dropped;
///   * `>=`, `<=`, `!=`, `<`, `>` and `==` already mean the same thing.
///
/// Anything unrecognized yields an empty spec rather than a guess, since a
/// wrong constraint is worse than an absent one.
fn conda_spec_to_pep440(rest: &str) -> String {
    let rest = rest.trim();
    if rest.is_empty() {
        return String::new();
    }

    // Two-character operators first, so `>=` is not read as a bare `>`.
    for op in ["==", ">=", "<=", "!=", "~="] {
        if let Some(v) = rest.strip_prefix(op) {
            let v = version_field(v);
            return if v.is_empty() {
                String::new()
            } else {
                format!("{op}{v}")
            };
        }
    }
    for op in ['>', '<'] {
        if let Some(v) = rest.strip_prefix(op) {
            let v = version_field(v);
            return if v.is_empty() {
                String::new()
            } else {
                format!("{op}{v}")
            };
        }
    }

    // A lone `=` is conda's prefix match.
    if let Some(v) = rest.strip_prefix('=') {
        let v = version_field(v);
        if v.is_empty() {
            return String::new();
        }
        // An already-exact version stays exact; otherwise widen to the prefix
        // form so `numpy=1.24` keeps matching 1.24.x as conda intends.
        return if v.ends_with('*') {
            format!("=={v}")
        } else {
            format!("=={v}.*")
        };
    }

    String::new()
}

/// Take the version out of a conda spec, discarding any build-string field.
fn version_field(s: &str) -> &str {
    s.split('=').next().unwrap_or("").trim()
}

/// Convert a Poetry caret/tilde constraint to an approximate PEP 440 form.
fn caret_to_pep440(spec: &str) -> String {
    let s = spec.trim();
    if let Some(v) = s.strip_prefix('^') {
        // ^3.9 → >=3.9,<4 (major-locked); good enough for interpreter reasoning.
        let major = v.split('.').next().unwrap_or("3");
        format!(
            ">={v},<{}",
            major.parse::<u32>().map(|m| m + 1).unwrap_or(4)
        )
    } else if let Some(v) = s.strip_prefix('~') {
        format!(">={v}")
    } else {
        s.to_string()
    }
}

fn has_python_file(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    rd.flatten()
        .any(|e| e.path().extension().is_some_and(|x| x == "py"))
}

fn dedupe(mut reqs: Vec<Requirement>) -> Vec<Requirement> {
    reqs.retain(|r| !r.name.is_empty());
    reqs.sort();
    // Same name declared twice: keep the constrained one, since an installer
    // has to satisfy every mention.
    reqs.dedup_by(|a, b| {
        if a.name != b.name {
            return false;
        }
        if b.spec.is_empty() && !a.spec.is_empty() {
            b.spec = a.spec.clone();
        }
        true
    });
    reqs
}

/// Translate a conda `environment.yml` into a minimal PEP 621 `pyproject.toml`,
/// per F5's "offer translation to pyproject/uv" migration action.
///
/// `deps` must have `has_conda_env` set. `project_name` is typically the
/// workspace directory name. This is deliberately conservative: conda package
/// names do not always match their PyPI equivalent (and some, like compiled
/// system libraries, have no PyPI equivalent at all), and guessing a mapping
/// per name would be exactly the kind of per-library hardcoding F3 refuses to
/// do elsewhere. Every parsed dependency is carried over as-is, and the caller
/// is told plainly to review it — an honest starting point, not a silent one.
pub fn conda_migration_pyproject(deps: &ProjectDeps, project_name: &str) -> String {
    // normalize_name doesn't touch whitespace (it never appears in a parsed
    // requirement name), but a directory name like "My Demo Project" needs it
    // collapsed too to satisfy PEP 621's `[A-Za-z0-9._-]` project name rule.
    let spaced = project_name.replace(char::is_whitespace, "-");
    let name = normalize_name(&spaced);
    let name = if name.is_empty() {
        "project".to_string()
    } else {
        name
    };

    let requires_python = deps.conda_python.as_deref().map(|v| {
        let v = v.trim();
        if v.starts_with(['>', '<', '=', '~', '!']) {
            v.to_string()
        } else {
            format!(">={v}")
        }
    });

    let mut out = String::new();
    out.push_str("[project]\n");
    out.push_str(&format!("name = \"{name}\"\n"));
    out.push_str("version = \"0.1.0\"\n");
    if let Some(rp) = &requires_python {
        out.push_str(&format!("requires-python = \"{rp}\"\n"));
    }

    if deps.packages.is_empty() {
        out.push_str("dependencies = []\n");
    } else {
        out.push_str("dependencies = [\n");
        for req in &deps.packages {
            out.push_str(&format!("    \"{req}\",\n"));
        }
        out.push_str("]\n");
    }

    // No [build-system] section: this describes an application (a venv plus
    // dependencies) for uv to manage, not a redistributable package. Adding a
    // build backend nobody asked for would be an assumption beyond the ask.
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_examples() {
        assert_eq!(normalize_name("Flask"), "flask");
        assert_eq!(normalize_name("opencv_python"), "opencv-python");
        assert_eq!(normalize_name("ruamel.yaml"), "ruamel-yaml");
    }

    #[test]
    fn requirement_name_strips_specifiers() {
        assert_eq!(
            requirement_name("mediapipe==0.10.9").as_deref(),
            Some("mediapipe")
        );
        assert_eq!(requirement_name("torch>=2.0,<3").as_deref(), Some("torch"));
        assert_eq!(
            requirement_name("uvicorn[standard]").as_deref(),
            Some("uvicorn")
        );
        assert_eq!(
            requirement_name("numpy ; python_version<'3.10'").as_deref(),
            Some("numpy")
        );
        assert_eq!(requirement_name("# a comment"), None);
        assert_eq!(requirement_name("-r base.txt"), None);
        assert_eq!(requirement_name(""), None);
    }

    #[test]
    fn parse_pep621_pyproject() {
        let text = r#"
[project]
name = "demo"
requires-python = ">=3.9,<3.13"
dependencies = ["mediapipe==0.10.9", "numpy>=1.24"]
"#;
        let (names, rp) = parse_pyproject(text);
        assert_eq!(rp.as_deref(), Some(">=3.9,<3.13"));
        assert!(names.iter().any(|r| r.name == "mediapipe"));
        assert!(names.iter().any(|r| r.name == "numpy"));
    }

    #[test]
    fn parse_poetry_pyproject() {
        let text = r#"
[tool.poetry.dependencies]
python = "^3.10"
requests = "^2.31"
"#;
        let (names, rp) = parse_pyproject(text);
        assert!(names.iter().any(|r| r.name == "requests"));
        assert!(!names.iter().any(|r| r.name == "python"));
        assert!(rp.unwrap().starts_with(">=3.10"));
    }

    #[test]
    fn parse_conda_env() {
        let text = "name: demo\ndependencies:\n  - python=3.10\n  - numpy>=1.20\n";
        let (names, py) = parse_environment_yml(text);
        assert_eq!(py.as_deref(), Some("3.10"));
        assert!(names.iter().any(|r| r.name == "numpy"));
        assert!(!names.iter().any(|r| r.name == "python"));
    }

    #[test]
    fn conda_dependency_pins_are_kept() {
        // Dropping these silently produced a pyproject.toml with no version
        // constraints at all, which is a different project from the one the
        // environment.yml described.
        let text = "dependencies:\n  \
             - numpy>=1.24\n  \
             - scipy=1.11\n  \
             - pandas==2.0.3\n  \
             - requests\n  \
             - pip:\n    \
             - mediapipe==0.10.14\n";
        let (names, _) = parse_environment_yml(text);

        let spec_of = |n: &str| {
            names
                .iter()
                .find(|r| r.name == n)
                .map(|r| r.spec.clone())
                .unwrap_or_else(|| panic!("{n} missing from {names:?}"))
        };
        assert_eq!(spec_of("numpy"), ">=1.24");
        // conda's single `=` is a prefix match, not an exact pin.
        assert_eq!(spec_of("scipy"), "==1.11.*");
        assert_eq!(spec_of("pandas"), "==2.0.3");
        assert_eq!(spec_of("requests"), "");
        assert_eq!(spec_of("mediapipe"), "==0.10.14");
    }

    #[test]
    fn conda_build_strings_are_discarded() {
        // `name=version=build` has no PyPI equivalent for the build field.
        let text = "dependencies:\n  - numpy=1.24=py311h5a2b\n";
        let (names, _) = parse_environment_yml(text);
        assert_eq!(names[0].spec, "==1.24.*");
    }

    #[test]
    fn conda_migration_produces_a_usable_pyproject() {
        let deps = ProjectDeps {
            sources: vec!["environment.yml".into()],
            packages: vec![Requirement::any("numpy"), req("pillow", "==10.0.0")],
            declared_requires_python: None,
            has_conda_env: true,
            conda_python: Some("3.10".to_string()),
        };
        let toml = conda_migration_pyproject(&deps, "My Demo Project");

        assert!(toml.contains("name = \"my-demo-project\""));
        assert!(
            toml.contains("requires-python = \">=3.10\""),
            "a bare conda python pin must gain a >= operator: {toml}"
        );
        assert!(toml.contains("\"numpy\""));
        assert!(toml.contains("\"pillow==10.0.0\""));
        assert!(
            !toml.contains("build-system"),
            "an application migration should not assume a build backend"
        );
    }

    #[test]
    fn conda_migration_handles_no_python_pin_and_empty_name() {
        let deps = ProjectDeps {
            has_conda_env: true,
            ..Default::default()
        };
        let toml = conda_migration_pyproject(&deps, "");
        assert!(!toml.contains("requires-python"));
        assert!(toml.contains("name = \"project\""));
        assert!(toml.contains("dependencies = []"));
    }

    fn req(name: &str, spec: &str) -> Requirement {
        Requirement {
            name: name.to_string(),
            spec: spec.to_string(),
        }
    }
}
