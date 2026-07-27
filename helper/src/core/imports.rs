//! Import extraction from Python source.
//!
//! Deliberately a line scanner rather than a parser. F4 only needs the top-level
//! module of each import plus its position in the buffer, and a scanner stays
//! fast enough to run on every keystroke debounce without a syntax tree.
//!
//! It does track string state, because an `import` inside a docstring is not an
//! import and squiggling it would be wrong. Comments are stripped the same way.

/// One imported module, with the buffer range covering the module path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRef {
    /// Top-level module: `cv2` from `import cv2.aruco`, `os` from `from os.path import join`.
    pub module: String,
    /// The local name bound to the package *root*, when there is one.
    ///
    /// `import mediapipe` binds `mediapipe`, `import mediapipe as mp` binds
    /// `mp`, and `import a.b` binds `a`. It is `None` for `from x import y` and
    /// for `import a.b as c`, where the name refers to something below the
    /// root; attribute checks against the root would be wrong for those.
    pub binding: Option<String>,
    /// Zero-based line number.
    pub line: u32,
    /// Zero-based character offsets covering the module path as written.
    pub start: u32,
    pub end: u32,
}

/// A `name.attribute` access found in the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrUse {
    /// The local name on the left, e.g. `mp`.
    pub base: String,
    /// The attribute on the right, e.g. `solutions`.
    pub attr: String,
    pub line: u32,
    /// Range covering `base.attr`.
    pub start: u32,
    pub end: u32,
}

/// Extract every absolute import in `source`.
///
/// Relative imports (`from . import x`) are skipped: they name modules inside
/// the project, never a PyPI distribution.
pub fn extract(source: &str) -> Vec<ImportRef> {
    let mut out = Vec::new();
    let mut open_delim: Option<&'static str> = None;

    for (lineno, raw) in source.lines().enumerate() {
        let (code, next_delim) = strip_strings_and_comments(raw, open_delim);
        open_delim = next_delim;

        let trimmed = code.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        // Column offset introduced by leading indentation.
        let indent = code.len() - trimmed.len();

        if let Some(rest) = trimmed.strip_prefix("import ") {
            let base = indent + "import ".len();
            collect_import_list(rest, lineno as u32, base, &mut out);
        } else if let Some(rest) = trimmed.strip_prefix("from ") {
            let base = indent + "from ".len();
            collect_from(rest, lineno as u32, base, &mut out);
        }
    }
    out
}

/// Find `base.attr` accesses for the given local names.
///
/// Only the first attribute after the name is taken: `mp.solutions.hands`
/// yields `solutions`, which is the one that has to exist on the package for
/// the rest of the chain to be reachable. Import statements are skipped, since
/// `import a.b` is not an attribute access.
pub fn extract_attr_uses(source: &str, bases: &[String]) -> Vec<AttrUse> {
    if bases.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut open_delim: Option<&'static str> = None;

    for (lineno, raw) in source.lines().enumerate() {
        let (code, next_delim) = strip_strings_and_comments(raw, open_delim);
        open_delim = next_delim;

        let trimmed = code.trim_start();
        if trimmed.is_empty() || trimmed.starts_with("import ") || trimmed.starts_with("from ") {
            continue;
        }

        let chars: Vec<char> = code.chars().collect();
        for base in bases {
            let base_chars: Vec<char> = base.chars().collect();
            let mut i = 0;
            while i + base_chars.len() < chars.len() {
                if chars[i..i + base_chars.len()] != base_chars[..] {
                    i += 1;
                    continue;
                }
                // Must be a standalone name followed by a dot. A preceding dot
                // means it is somebody else's attribute (`self.mp.foo`), not the
                // module bound by the import.
                let before_ok = i == 0 || (!is_ident_char(chars[i - 1]) && chars[i - 1] != '.');
                let dot_at = i + base_chars.len();
                if !before_ok || chars[dot_at] != '.' {
                    i += 1;
                    continue;
                }

                let attr: String = chars[dot_at + 1..]
                    .iter()
                    .take_while(|c| is_ident_char(**c))
                    .collect();
                if attr.is_empty() || attr.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                    i = dot_at + 1;
                    continue;
                }

                let end = dot_at + 1 + attr.chars().count();
                out.push(AttrUse {
                    base: base.clone(),
                    attr,
                    line: lineno as u32,
                    start: i as u32,
                    end: end as u32,
                });
                i = end;
            }
        }
    }
    out
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `import a.b as x, c` -> refs for `a` and `c`.
fn collect_import_list(rest: &str, line: u32, base: usize, out: &mut Vec<ImportRef>) {
    let mut offset = base;
    for part in rest.split(',') {
        let leading = part.len() - part.trim_start().len();
        let token = part.trim();
        // Drop an `as alias` suffix; the module path is the first word.
        let mut words = token.split_whitespace();
        let path = words.next().unwrap_or("");
        let alias = match (words.next(), words.next()) {
            (Some("as"), Some(name)) => Some(name),
            _ => None,
        };

        if is_module_path(path) {
            // `import a.b` binds `a`; `import a.b as c` binds `c`, which points
            // below the root, so the root has no local name here.
            let dotted = path.contains('.');
            let binding = match (alias, dotted) {
                (Some(name), false) => Some(name.to_string()),
                (Some(_), true) => None,
                (None, _) => path.split('.').next().map(|s| s.to_string()),
            };
            let start = offset + leading;
            push_ref(path, binding, line, start, out);
        }
        // +1 for the comma we split on.
        offset += part.len() + 1;
    }
}

/// `from a.b import c` -> ref for `a`. Relative imports are skipped.
fn collect_from(rest: &str, line: u32, base: usize, out: &mut Vec<ImportRef>) {
    let leading = rest.len() - rest.trim_start().len();
    let path = rest.split_whitespace().next().unwrap_or("");
    // `.mod`, `..pkg` and bare `.` are all project-relative.
    if path.starts_with('.') || !is_module_path(path) {
        return;
    }
    // `from x import y` binds y, which is not the package root.
    push_ref(path, None, line, base + leading, out);
}

fn push_ref(
    path: &str,
    binding: Option<String>,
    line: u32,
    start: usize,
    out: &mut Vec<ImportRef>,
) {
    let Some(top) = path.split('.').next() else {
        return;
    };
    if top.is_empty() {
        return;
    }
    out.push(ImportRef {
        module: top.to_string(),
        binding,
        line,
        start: start as u32,
        end: (start + path.chars().count()) as u32,
    });
}

/// A dotted identifier path, and nothing else.
fn is_module_path(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        && s.chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
}

/// Blank out string literals and comments so their contents are never scanned.
///
/// Returns the masked line plus the triple-quote delimiter still open at the end
/// of it, if any. Masking preserves length so column positions stay correct.
fn strip_strings_and_comments(
    line: &str,
    mut open_delim: Option<&'static str>,
) -> (String, Option<&'static str>) {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;

    while i < chars.len() {
        // Inside a triple-quoted string: consume until the closing delimiter.
        if let Some(delim) = open_delim {
            if starts_with_at(&chars, i, delim) {
                open_delim = None;
                out.push_str("   ");
                i += 3;
            } else {
                out.push(' ');
                i += 1;
            }
            continue;
        }

        // Opening triple quote.
        if starts_with_at(&chars, i, "\"\"\"") || starts_with_at(&chars, i, "'''") {
            let delim = if chars[i] == '"' { "\"\"\"" } else { "'''" };
            open_delim = Some(delim);
            out.push_str("   ");
            i += 3;
            continue;
        }

        // Single-quoted string: skip to its close on this line.
        if chars[i] == '"' || chars[i] == '\'' {
            let quote = chars[i];
            out.push(' ');
            i += 1;
            while i < chars.len() {
                let escaped = chars[i] == '\\';
                out.push(' ');
                i += 1;
                if escaped {
                    if i < chars.len() {
                        out.push(' ');
                        i += 1;
                    }
                    continue;
                }
                if chars[i - 1] == quote {
                    break;
                }
            }
            continue;
        }

        // Comment: the rest of the line is dead.
        if chars[i] == '#' {
            break;
        }

        out.push(chars[i]);
        i += 1;
    }

    (out, open_delim)
}

fn starts_with_at(chars: &[char], i: usize, pat: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    i + p.len() <= chars.len() && chars[i..i + p.len()] == p[..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modules(src: &str) -> Vec<String> {
        extract(src).into_iter().map(|i| i.module).collect()
    }

    #[test]
    fn plain_import() {
        assert_eq!(modules("import os\n"), vec!["os"]);
    }

    #[test]
    fn dotted_import_keeps_top_level() {
        assert_eq!(modules("import cv2.aruco\n"), vec!["cv2"]);
        assert_eq!(modules("from os.path import join\n"), vec!["os"]);
    }

    #[test]
    fn aliased_and_multiple() {
        assert_eq!(modules("import numpy as np\n"), vec!["numpy"]);
        assert_eq!(modules("import os, sys\n"), vec!["os", "sys"]);
    }

    #[test]
    fn relative_imports_are_skipped() {
        assert!(modules("from . import helpers\n").is_empty());
        assert!(modules("from .models import User\n").is_empty());
        assert!(modules("from ..pkg import thing\n").is_empty());
    }

    #[test]
    fn indented_imports_are_found() {
        assert_eq!(modules("try:\n    import ujson\n"), vec!["ujson"]);
    }

    #[test]
    fn imports_in_comments_and_strings_are_ignored() {
        assert!(modules("# import requests\n").is_empty());
        assert!(modules("x = \"import requests\"\n").is_empty());
    }

    #[test]
    fn imports_inside_docstrings_are_ignored() {
        let src = "\"\"\"\nExample:\n    import requests\n\"\"\"\nimport os\n";
        assert_eq!(modules(src), vec!["os"]);
    }

    #[test]
    fn position_covers_the_module_path() {
        let refs = extract("import cv2.aruco\n");
        assert_eq!(refs[0].line, 0);
        assert_eq!(refs[0].start, 7);
        assert_eq!(refs[0].end, 16);
    }

    #[test]
    fn second_module_in_a_list_is_positioned() {
        let refs = extract("import os, sys\n");
        assert_eq!(refs[1].module, "sys");
        assert_eq!(refs[1].start, 11);
        assert_eq!(refs[1].end, 14);
    }

    #[test]
    fn not_an_import_statement() {
        assert!(modules("important = 1\n").is_empty());
        assert!(modules("fromage = 2\n").is_empty());
    }
}

#[cfg(test)]
mod attr_tests {
    use super::*;

    fn bases() -> Vec<String> {
        vec!["mp".to_string()]
    }

    fn found(src: &str) -> Vec<String> {
        extract_attr_uses(src, &bases())
            .into_iter()
            .map(|u| u.attr)
            .collect()
    }

    #[test]
    fn takes_the_first_attribute_in_a_chain() {
        assert_eq!(found("mp_hands = mp.solutions.hands\n"), vec!["solutions"]);
    }

    #[test]
    fn requires_a_standalone_name() {
        // Substring and attribute-of-something-else must not match.
        assert!(found("mpx.solutions\n").is_empty());
        assert!(found("self.mp.solutions\n").is_empty());
    }

    #[test]
    fn ignores_strings_and_comments() {
        assert!(found("# mp.solutions\n").is_empty());
        assert!(found("s = \"mp.solutions\"\n").is_empty());
    }

    #[test]
    fn ignores_import_statements() {
        assert!(found("import mp.solutions\n").is_empty());
    }

    #[test]
    fn records_position_over_base_and_attribute() {
        let uses = extract_attr_uses("x = mp.solutions\n", &bases());
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].line, 0);
        assert_eq!(uses[0].start, 4);
        assert_eq!(uses[0].end, 16);
    }

    #[test]
    fn binding_follows_the_alias() {
        let refs = extract("import mediapipe as mp\n");
        assert_eq!(refs[0].binding.as_deref(), Some("mp"));

        let plain = extract("import mediapipe\n");
        assert_eq!(plain[0].binding.as_deref(), Some("mediapipe"));

        // Points below the root, so no root binding.
        let dotted = extract("import a.b as c\n");
        assert_eq!(dotted[0].binding, None);
        let from = extract("from a import b\n");
        assert_eq!(from[0].binding, None);
    }
}
