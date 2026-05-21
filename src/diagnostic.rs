use crate::span::Span;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub span: Span,
    pub message: String,
    /// Optional secondary spans with short notes that point to related
    /// source locations (e.g., the original move site, the prior binding,
    /// the ensures clause violated by a return). These are rendered after
    /// the primary diagnostic with the same source-line + underline format.
    pub related: Vec<(Span, String)>,
}

impl Diagnostic {
    pub fn new(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
            related: Vec::new(),
        }
    }

    pub fn with_related(mut self, span: Span, note: impl Into<String>) -> Self {
        self.related.push((span, note.into()));
        self
    }
}

pub fn format_diagnostics(path: &str, source: &str, diagnostics: &[Diagnostic]) -> String {
    let mut output = String::new();

    for diagnostic in diagnostics {
        render_one(&mut output, path, source, diagnostic.span, "error", &diagnostic.message);
        for (span, note) in &diagnostic.related {
            render_one(&mut output, path, source, *span, "note", note);
        }
    }

    output
}

fn render_one(output: &mut String, path: &str, source: &str, span: Span, level: &str, message: &str) {
    let (line_number, column_number, line_start, line_end) = line_info(source, span.start);
    let line = &source[line_start..line_end];
    let span_start_byte = span.start.min(line_end).max(line_start);
    let span_end_byte = span.end.min(line_end).max(span_start_byte);
    let underline_start = char_count(&source[line_start..span_start_byte]);
    let underline_width = char_count(&source[span_start_byte..span_end_byte]).max(1);

    output.push_str(&format!(
        "{}:{}:{}: {}: {}\n",
        path, line_number, column_number, level, message
    ));
    output.push_str(line);
    output.push('\n');
    output.push_str(&" ".repeat(underline_start));
    output.push_str(&"^".repeat(underline_width));
    output.push('\n');
}

fn line_info(source: &str, offset: usize) -> (usize, usize, usize, usize) {
    let clamped = offset.min(source.len());
    let mut line_number = 1;
    let mut line_start = 0;

    for (index, byte) in source.bytes().enumerate() {
        if index >= clamped {
            break;
        }
        if byte == b'\n' {
            line_number += 1;
            line_start = index + 1;
        }
    }

    let line_end = source[line_start..]
        .find('\n')
        .map(|relative| line_start + relative)
        .unwrap_or(source.len());
    let column_number = char_count(&source[line_start..clamped]) + 1;

    (line_number, column_number, line_start, line_end)
}

fn char_count(text: &str) -> usize {
    text.chars().count()
}

/// Tracks where each source file's contents live in a concatenated multi-file
/// build buffer, so a global span offset can be mapped back to the original
/// file + local offset for accurate diagnostics.
#[derive(Clone, Debug, Default)]
pub struct FileMap {
    entries: Vec<FileEntry>,
}

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: String,
    pub source: String,
    /// Byte offset in the concatenated buffer where this file's content starts.
    pub start: usize,
}

impl FileMap {
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    pub fn push(&mut self, path: String, source: String, start: usize) {
        self.entries.push(FileEntry { path, source, start });
    }

    /// Find the file entry containing the given global offset, plus the
    /// local offset within that file's source.
    pub fn lookup(&self, global_offset: usize) -> Option<(&FileEntry, usize)> {
        // Scan in reverse so later (deeper-pushed) files win on tie at
        // file-boundary offsets; in practice ranges don't overlap.
        for entry in self.entries.iter().rev() {
            if entry.start <= global_offset
                && global_offset <= entry.start + entry.source.len()
            {
                return Some((entry, global_offset - entry.start));
            }
        }
        None
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// One past the last byte covered by any entry. New entries
    /// pushed after this can use this as their `start` so spans
    /// stay unambiguous.
    pub fn end_offset(&self) -> usize {
        self.entries
            .iter()
            .map(|e| e.start + e.source.len())
            .max()
            .unwrap_or(0)
    }

    /// Append every entry from `other` to `self`, shifting each
    /// entry's `start` so it sits past `self`'s current end (plus
    /// a one-byte gap so no boundary lookup wins twice). Returns
    /// the shift amount the caller should add to any diagnostic
    /// span produced against `other` so they remain valid in the
    /// merged map.
    ///
    /// Used by `intentc check --json` across multiple files: each
    /// `compile_path` call returns its own FileMap (starting at 0)
    /// and a list of diagnostics whose spans are relative to that
    /// map; merging shifts both into a single global frame so the
    /// JSON formatter can emit a single `{"diagnostics": [...]}`
    /// object covering the whole run.
    pub fn extend_with(&mut self, other: &FileMap) -> usize {
        let shift = self.end_offset() + if self.entries.is_empty() { 0 } else { 1 };
        for entry in &other.entries {
            self.entries.push(FileEntry {
                path: entry.path.clone(),
                source: entry.source.clone(),
                start: entry.start + shift,
            });
        }
        shift
    }
}

pub fn format_diagnostics_with_files(map: &FileMap, diagnostics: &[Diagnostic]) -> String {
    let mut output = String::new();
    for d in diagnostics {
        render_with_filemap(&mut output, map, d.span, "error", &d.message);
        for (span, note) in &d.related {
            render_with_filemap(&mut output, map, *span, "note", note);
        }
    }
    output
}

fn render_with_filemap(
    output: &mut String,
    map: &FileMap,
    span: Span,
    level: &str,
    message: &str,
) {
    let Some((entry, local_start)) = map.lookup(span.start) else {
        // Fallback: print without file context if mapping fails.
        output.push_str(&format!("?:?:?: {}: {}\n", level, message));
        return;
    };
    let source = &entry.source;
    let local_end = span
        .end
        .saturating_sub(entry.start)
        .min(source.len());

    let (line_number, column_number, line_start, line_end) = line_info(source, local_start);
    let line = &source[line_start..line_end];
    let span_start_byte = local_start.min(line_end).max(line_start);
    let span_end_byte = local_end.min(line_end).max(span_start_byte);
    let underline_start = char_count(&source[line_start..span_start_byte]);
    let underline_width = char_count(&source[span_start_byte..span_end_byte]).max(1);

    output.push_str(&format!(
        "{}:{}:{}: {}: {}\n",
        entry.path, line_number, column_number, level, message
    ));
    output.push_str(line);
    output.push('\n');
    output.push_str(&" ".repeat(underline_start));
    output.push_str(&"^".repeat(underline_width));
    output.push('\n');
}

pub fn format_diagnostics_json_with_files(map: &FileMap, diagnostics: &[Diagnostic]) -> String {
    let mut out = String::from("{\"diagnostics\":[");
    for (i, d) in diagnostics.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        emit_diagnostic_json_with_map(&mut out, map, d);
    }
    out.push_str("]}\n");
    out
}

fn emit_diagnostic_json_with_map(out: &mut String, map: &FileMap, d: &Diagnostic) {
    out.push_str("{\"level\":\"error\",\"message\":");
    push_json_string(out, &d.message);
    out.push_str(",\"primary\":");
    emit_span_json_with_map(out, map, d.span);
    if !d.related.is_empty() {
        out.push_str(",\"related\":[");
        for (i, (span, note)) in d.related.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"message\":");
            push_json_string(out, note);
            out.push_str(",\"span\":");
            emit_span_json_with_map(out, map, *span);
            out.push('}');
        }
        out.push(']');
    }
    out.push('}');
}

fn emit_span_json_with_map(out: &mut String, map: &FileMap, span: Span) {
    if let Some((entry, local_start)) = map.lookup(span.start) {
        let (line_start, col_start, _, _) = line_info(&entry.source, local_start);
        let local_end = span.end.saturating_sub(entry.start).min(entry.source.len());
        let (line_end, col_end, _, _) = line_info(&entry.source, local_end);
        out.push('{');
        out.push_str("\"file\":");
        push_json_string(out, &entry.path);
        out.push_str(&format!(
            ",\"line\":{},\"col\":{},\"end_line\":{},\"end_col\":{}",
            line_start, col_start, line_end, col_end
        ));
        out.push('}');
    } else {
        out.push_str("{\"file\":null}");
    }
}

/// JSON serialization of a diagnostic list. Hand-rolled to keep zero
/// dependencies. Shape:
///
/// ```json
/// {
///   "diagnostics": [
///     {
///       "level": "error",
///       "message": "value 'xs' was moved; cannot use after move",
///       "primary": { "file": "f.intent", "line": 8, "col": 18,
///                    "end_line": 8, "end_col": 20 },
///       "related": [
///         { "message": "'xs' was moved here",
///           "span": { "file": "f.intent", "line": 7, "col": 21, ... } }
///       ]
///     }
///   ]
/// }
/// ```
///
/// Always ends with a single newline so consumers can read it line by line.
pub fn format_diagnostics_json(path: &str, source: &str, diagnostics: &[Diagnostic]) -> String {
    let mut out = String::from("{\"diagnostics\":[");
    for (i, d) in diagnostics.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        emit_diagnostic_json(&mut out, path, source, d);
    }
    out.push_str("]}\n");
    out
}

fn emit_diagnostic_json(out: &mut String, path: &str, source: &str, d: &Diagnostic) {
    out.push_str("{\"level\":\"error\",\"message\":");
    push_json_string(out, &d.message);
    out.push_str(",\"primary\":");
    emit_span_json(out, path, source, d.span);
    if !d.related.is_empty() {
        out.push_str(",\"related\":[");
        for (i, (span, note)) in d.related.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"message\":");
            push_json_string(out, note);
            out.push_str(",\"span\":");
            emit_span_json(out, path, source, *span);
            out.push('}');
        }
        out.push(']');
    }
    out.push('}');
}

fn emit_span_json(out: &mut String, path: &str, source: &str, span: Span) {
    let (line_start, col_start, _, _) = line_info(source, span.start);
    let (line_end, col_end, _, _) = line_info(source, span.end);
    out.push('{');
    out.push_str("\"file\":");
    push_json_string(out, path);
    out.push_str(&format!(
        ",\"line\":{},\"col\":{},\"end_line\":{},\"end_col\":{}",
        line_start, col_start, line_end, col_end
    ));
    out.push('}');
}

fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}
