//! `vani.toml` manifest parser. Closure #280 — foundational
//! for multi-file projects + the future Kosh package manager.
//!
//! V1 format (minimal viable):
//!
//! ```toml
//! [package]
//! name = "my_project"
//! entry = "src/main.vani"
//! ```
//!
//! `name` identifies the project (no current usage beyond
//! display; reserved for Kosh registry coordinates).
//! `entry` is the relative path from the manifest dir to the
//! `.vani` source containing `fn main`. The driver passes this
//! to the existing single-file pipeline (which already does
//! `use "path"` resolution for multi-file via #235).
//!
//! Future v2 additions (queued, not in this closure):
//! `[deps]` table for Kosh-registry packages, optional
//! `[build]` knobs for backend default / opt level /
//! `--link-with` defaults.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Parsed `vani.toml` manifest. The driver reads this when
/// `intentc build` / `run` / `check` is invoked from a
/// directory containing a `vani.toml` (or its parent walk).
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub package_name: String,
    /// Absolute path to the entry `.vani` file. Resolved
    /// from the manifest's `entry` key relative to the
    /// manifest's containing directory.
    pub entry_path: PathBuf,
    /// Directory containing the manifest (everything else is
    /// resolved relative to this).
    pub root_dir: PathBuf,
    /// Closure #287: dependency table. Each entry names a
    /// dependency and its resolved local-path entry source.
    /// V1 supports `[deps] name = { path = "../lib-dir" }`
    /// only; registry deps (Kosh) ship in a future closure.
    pub deps: Vec<Dependency>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Dependency {
    /// Name the dep is referred to by in `[deps]`.
    pub name: String,
    /// Absolute path to the dep's entry .vani source.
    /// Resolved as `<dep_root>/<dep_manifest.entry>`.
    pub entry_path: PathBuf,
}

#[derive(Debug)]
pub enum ManifestError {
    Io(String),
    Parse { line: usize, message: String },
    MissingField { section: String, key: String },
    UnknownSection(String),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestError::Io(m) => write!(f, "vani.toml: {}", m),
            ManifestError::Parse { line, message } => {
                write!(f, "vani.toml:{}: {}", line, message)
            }
            ManifestError::MissingField { section, key } => write!(
                f,
                "vani.toml: missing required `{}` in [{}] section",
                key, section
            ),
            ManifestError::UnknownSection(name) => {
                write!(f, "vani.toml: unknown section [{}]", name)
            }
        }
    }
}

/// Locate the nearest `vani.toml` by walking up from `start`
/// until either found or the filesystem root is reached.
/// Returns `None` if no manifest is found.
pub fn find_manifest(start: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = if start.is_file() {
        start.parent()
    } else {
        Some(start)
    };
    while let Some(dir) = cur {
        let candidate = dir.join("vani.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        cur = dir.parent();
    }
    None
}

pub fn load_manifest(manifest_path: &Path) -> Result<Manifest, ManifestError> {
    let source = std::fs::read_to_string(manifest_path).map_err(|e| {
        ManifestError::Io(format!("read '{}': {}", manifest_path.display(), e))
    })?;
    let (sections, dep_entries) = parse_toml_minimal(&source)?;
    let pkg = sections.get("package").ok_or(ManifestError::MissingField {
        section: "package".into(),
        key: "package".into(),
    })?;
    let name = pkg
        .get("name")
        .ok_or_else(|| ManifestError::MissingField {
            section: "package".into(),
            key: "name".into(),
        })?
        .clone();
    let entry_rel = pkg
        .get("entry")
        .ok_or_else(|| ManifestError::MissingField {
            section: "package".into(),
            key: "entry".into(),
        })?
        .clone();
    let root_dir = manifest_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let entry_path = root_dir.join(entry_rel);
    // Closure #287: resolve each [deps] entry's local path
    // by recursively loading the dep's manifest and reading
    // its entry-source path.
    let mut deps: Vec<Dependency> = Vec::new();
    for (dep_name, dep_path_rel) in &dep_entries {
        let dep_dir = root_dir.join(dep_path_rel);
        let dep_manifest = dep_dir.join("vani.toml");
        let dep_loaded = load_manifest(&dep_manifest).map_err(|e| {
            ManifestError::Io(format!(
                "loading dep '{}' from '{}': {}",
                dep_name, dep_manifest.display(), e
            ))
        })?;
        deps.push(Dependency {
            name: dep_name.clone(),
            entry_path: dep_loaded.entry_path,
        });
    }
    Ok(Manifest {
        package_name: name,
        entry_path,
        root_dir,
        deps,
    })
}

// Minimal TOML subset:
// - `[section]` headers
// - `key = "value"` (string values only in v1)
// - `# comments` to end of line
// - empty / whitespace lines tolerated
// - top-level key/value rejected (sections required)
//
// Anything more complex (arrays, nested tables, dotted keys,
// non-string values) surfaces a clear parse error pointing
// at the line.
// Closure #287: v2 extends the parser to handle `[deps]`
// section with inline-table values:
//   [deps]
//   math = { path = "../math-lib" }
// Returns the regular section map alongside a Vec of
// (dep_name, dep_path_rel) entries.
fn parse_toml_minimal(
    source: &str,
) -> Result<(HashMap<String, HashMap<String, String>>, Vec<(String, String)>), ManifestError> {
    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut deps: Vec<(String, String)> = Vec::new();
    let mut current_section: Option<String> = None;
    for (lineno_zero, raw) in source.lines().enumerate() {
        let line_no = lineno_zero + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') {
            let end = trimmed.find(']').ok_or(ManifestError::Parse {
                line: line_no,
                message: "section header missing closing `]`".into(),
            })?;
            let name = trimmed[1..end].trim();
            // Known sections: `package`, `deps`.
            match name {
                "package" | "deps" => {}
                other => return Err(ManifestError::UnknownSection(other.into())),
            }
            current_section = Some(name.to_string());
            sections.entry(name.to_string()).or_default();
            continue;
        }
        let eq = trimmed.find('=').ok_or(ManifestError::Parse {
            line: line_no,
            message: "expected `key = …`".into(),
        })?;
        let key = trimmed[..eq].trim().to_string();
        let value_raw = trimmed[eq + 1..].trim();
        let section = current_section.as_deref().ok_or(ManifestError::Parse {
            line: line_no,
            message: "key/value outside any [section]".into(),
        })?;
        // Closure #287: `[deps]` accepts an inline-table
        // form `name = { path = "..." }`. v1 only `path` key.
        if section == "deps" && value_raw.starts_with('{') {
            let close = value_raw.rfind('}').ok_or(ManifestError::Parse {
                line: line_no,
                message: "inline table missing closing `}`".into(),
            })?;
            let inner = &value_raw[1..close];
            // Parse `path = "..."` from inner.
            let inner_eq = inner.find('=').ok_or(ManifestError::Parse {
                line: line_no,
                message: "inline table needs `path = \"...\"`".into(),
            })?;
            let inner_key = inner[..inner_eq].trim();
            if inner_key != "path" {
                return Err(ManifestError::Parse {
                    line: line_no,
                    message: format!(
                        "deps `{}`: only the `path` key is recognized in v1; got `{}`",
                        key, inner_key
                    ),
                });
            }
            let inner_val = inner[inner_eq + 1..].trim();
            if !inner_val.starts_with('"') {
                return Err(ManifestError::Parse {
                    line: line_no,
                    message: "path value must be a quoted string".into(),
                });
            }
            let after_open = &inner_val[1..];
            let qclose = after_open.find('"').ok_or(ManifestError::Parse {
                line: line_no,
                message: "path value missing closing `\"`".into(),
            })?;
            let path_val = after_open[..qclose].to_string();
            deps.push((key, path_val));
            continue;
        }
        if !value_raw.starts_with('"') {
            return Err(ManifestError::Parse {
                line: line_no,
                message: format!(
                    "value of `{}` must be a quoted string (or an inline \
                     table `{{ path = \"...\" }}` in [deps])",
                    key
                ),
            });
        }
        // Strip trailing comment after the closing quote.
        let after_open = &value_raw[1..];
        let close = after_open.find('"').ok_or(ManifestError::Parse {
            line: line_no,
            message: "string value missing closing `\"`".into(),
        })?;
        let value = after_open[..close].to_string();
        sections
            .entry(section.to_string())
            .or_default()
            .insert(key, value);
    }
    Ok((sections, deps))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_manifest() {
        let s = r#"
            [package]
            name = "my_project"
            entry = "src/main.vani"
        "#;
        let (sections, _deps) = parse_toml_minimal(s).expect("parses");
        assert_eq!(sections["package"]["name"], "my_project");
        assert_eq!(sections["package"]["entry"], "src/main.vani");
    }

    #[test]
    fn tolerates_comments_and_blank_lines() {
        let s = "\
            # leading comment\n\
            \n\
            [package]\n\
            # mid-section\n\
            name = \"x\"\n\
            entry = \"e.vani\"  # trailing comment\n\
        ";
        let (sections, _deps) = parse_toml_minimal(s).expect("parses");
        assert_eq!(sections["package"]["entry"], "e.vani");
    }

    #[test]
    fn rejects_unknown_section() {
        let s = "[unknown]\nfoo = \"bar\"\n";
        let err = parse_toml_minimal(s).expect_err("rejects");
        assert!(matches!(err, ManifestError::UnknownSection(ref n) if n == "unknown"));
    }

    #[test]
    fn rejects_non_string_value() {
        let s = "[package]\nname = 42\n";
        let err = parse_toml_minimal(s).expect_err("rejects");
        match err {
            ManifestError::Parse { message, .. } => {
                assert!(message.contains("quoted string"));
            }
            _ => panic!("expected Parse error, got {:?}", err),
        }
    }

    #[test]
    fn rejects_kv_outside_section() {
        let s = "name = \"x\"\n";
        let err = parse_toml_minimal(s).expect_err("rejects");
        match err {
            ManifestError::Parse { message, .. } => {
                assert!(message.contains("outside any"));
            }
            _ => panic!("expected Parse error, got {:?}", err),
        }
    }

    #[test]
    fn load_manifest_surfaces_missing_entry() {
        let dir = std::env::temp_dir().join(format!(
            "vani-manifest-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mf = dir.join("vani.toml");
        std::fs::write(&mf, "[package]\nname = \"x\"\n").unwrap();
        let err = load_manifest(&mf).expect_err("missing entry");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(
            err,
            ManifestError::MissingField { ref key, .. } if key == "entry"
        ));
    }

    #[test]
    fn parses_deps_inline_table() {
        let s = r#"
            [package]
            name = "x"
            entry = "e.vani"

            [deps]
            mathlib = { path = "../math" }
            other = { path = "../other-lib" }
        "#;
        let (_sections, deps) = parse_toml_minimal(s).expect("parses");
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].0, "mathlib");
        assert_eq!(deps[0].1, "../math");
        assert_eq!(deps[1].0, "other");
        assert_eq!(deps[1].1, "../other-lib");
    }

    #[test]
    fn rejects_unknown_key_in_inline_table() {
        let s = r#"
            [package]
            name = "x"
            entry = "e.vani"

            [deps]
            mathlib = { version = "1.0" }
        "#;
        let err = parse_toml_minimal(s).expect_err("rejects non-path key");
        match err {
            ManifestError::Parse { message, .. } => {
                assert!(message.contains("only the `path` key is recognized"));
            }
            _ => panic!("expected Parse error, got {:?}", err),
        }
    }

    #[test]
    fn find_manifest_walks_up() {
        let dir = std::env::temp_dir().join(format!(
            "vani-find-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sub = dir.join("nested/deep");
        std::fs::create_dir_all(&sub).unwrap();
        let mf = dir.join("vani.toml");
        std::fs::write(&mf, "[package]\nname = \"x\"\nentry = \"e.vani\"\n").unwrap();
        let found = find_manifest(&sub).expect("finds via parent walk");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found, mf);
    }
}
