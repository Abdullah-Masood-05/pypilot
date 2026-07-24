//! Project dependency parsing.
//!
//! Detects and parses the Python signal files in a workspace and normalizes their
//! dependencies to bare PyPI package names (versions/markers/extras stripped),
//! which is all the F3 engine needs. Conda `environment.yml` is detected and
//! flagged (per plan we don't fight conda in v1, only offer migration later).

use std::path::{Path, PathBuf};

/// What we learned from a workspace's project files.
#[derive(Debug, Clone, Default)]
pub struct ProjectDeps {
    /// Signal files that were found and parsed.
    pub sources: Vec<PathBuf>,
    /// Normalized dependency names (deduped, lowercased).
    pub packages: Vec<String>,
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

fn parse_requirements(text: &str) -> Vec<String> {
    text.lines().filter_map(requirement_name).collect()
}

/// Parse dependencies + requires-python from pyproject.toml (PEP 621 and Poetry).
fn parse_pyproject(text: &str) -> (Vec<String>, Option<String>) {
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
                if let Some(n) = requirement_name(d) {
                    names.push(n);
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
                    if let Some(n) = requirement_name(d) {
                        names.push(n);
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
            names.push(normalize_name(key));
        }
    }

    (names, requires_python)
}

/// Best-effort conda environment.yml: dependency names + the `python=` pin.
fn parse_environment_yml(text: &str) -> (Vec<String>, Option<String>) {
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
                    names.push(normalize_name(name_part));
                }
            }
        }
    }
    (names, python)
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

fn dedupe(mut names: Vec<String>) -> Vec<String> {
    names.retain(|n| !n.is_empty());
    names.sort();
    names.dedup();
    names
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
        assert!(names.contains(&"mediapipe".to_string()));
        assert!(names.contains(&"numpy".to_string()));
    }

    #[test]
    fn parse_poetry_pyproject() {
        let text = r#"
[tool.poetry.dependencies]
python = "^3.10"
requests = "^2.31"
"#;
        let (names, rp) = parse_pyproject(text);
        assert!(names.contains(&"requests".to_string()));
        assert!(!names.contains(&"python".to_string()));
        assert!(rp.unwrap().starts_with(">=3.10"));
    }

    #[test]
    fn parse_conda_env() {
        let text = "name: demo\ndependencies:\n  - python=3.10\n  - numpy>=1.20\n";
        let (names, py) = parse_environment_yml(text);
        assert_eq!(py.as_deref(), Some("3.10"));
        assert!(names.contains(&"numpy".to_string()));
        assert!(!names.contains(&"python".to_string()));
    }
}
