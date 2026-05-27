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
    let sections = parse_toml_minimal(&source)?;
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
    Ok(Manifest {
        package_name: name,
        entry_path,
        root_dir,
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
fn parse_toml_minimal(
    source: &str,
) -> Result<HashMap<String, HashMap<String, String>>, ManifestError> {
    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
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
            // Reject unknown sections to surface typos early.
            // Known sections: `package` (v1). Future: `deps`,
            // `build`.
            match name {
                "package" => {}
                other => return Err(ManifestError::UnknownSection(other.into())),
            }
            current_section = Some(name.to_string());
            sections.entry(name.to_string()).or_default();
            continue;
        }
        let eq = trimmed.find('=').ok_or(ManifestError::Parse {
            line: line_no,
            message: "expected `key = \"value\"`".into(),
        })?;
        let key = trimmed[..eq].trim().to_string();
        let value_raw = trimmed[eq + 1..].trim();
        if !value_raw.starts_with('"') {
            return Err(ManifestError::Parse {
                line: line_no,
                message: format!(
                    "value of `{}` must be a quoted string (v1 only supports \
                     string values)",
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
        let section = current_section.as_deref().ok_or(ManifestError::Parse {
            line: line_no,
            message: "key/value outside any [section]".into(),
        })?;
        sections
            .entry(section.to_string())
            .or_default()
            .insert(key, value);
    }
    Ok(sections)
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
        let sections = parse_toml_minimal(s).expect("parses");
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
        let sections = parse_toml_minimal(s).expect("parses");
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
