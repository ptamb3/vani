//! SMT-LIB encoder + z3 subprocess driver for the `prove` verifier.
//!
//! Scope:
//!  - Integer types (i8..u64) — modeled as fixed-width `(_ BitVec N)` with
//!    overflow-aware semantics (`bvadd`/`bvmul`/etc. wrap on overflow,
//!    `bvslt`/`bvult` split signed vs unsigned comparisons). Soundness:
//!    proofs that previously held under infinite-Int math but fail in
//!    real machine semantics (e.g. `x + 1 > x` for `x: i64`, which
//!    breaks at `INT64_MAX`) are now correctly rejected with a
//!    counterexample.
//!  - Float types `f32`/`f64` — modeled with `(_ FloatingPoint 8 24)` and
//!    `(_ FloatingPoint 11 53)`; arithmetic via `fp.add`/`fp.sub`/...
//!    with `RNE` rounding; equality is `fp.eq` (NaN-aware).
//!  - `bool`, in-scope variables, the function's `requires`/`ensures`
//!    clauses, branch conditions, and ensures-derived facts about
//!    let-bound call results.
//!  - Casts: integer widening (`sign_extend` / `zero_extend`), narrowing
//!    (`extract`), and int→float / float→float (`to_fp`).
//!  - `len(xs)` resolves to a fixed BV literal for arrays and to an
//!    opaque per-binding `<name>_len: (_ BitVec 64)` for Vec.
//!
//!  - Shifts (`<<`, `>>`) on integers — `bvshl` / `bvlshr` / `bvashr` with
//!    automatic width-matching of the shift count (sign- or zero-extend
//!    when narrower than the lhs, truncate when wider). Note that
//!    SMT-LIB semantics for shifts with counts `>=` the operand width
//!    yield zero, which matches the language's runtime guard.
//!
//! Out of scope (returns `SkippedUnsupported`): array/Vec/reference
//! operations. Float→int and float→float casts now encode via
//! `(_ fp.to_sbv N)` / `(_ fp.to_ubv N)` and `(_ to_fp …)`
//! respectively, with `RNE` rounding. Inline function-call results in
//! proofs are supported when the callee has `ensures` clauses — the
//! checker rewrites them to fresh symbolic variables constrained by
//! the substituted ensures before passing the query here. Calls with
//! no signature still surface as Unsupported.
//!
//! In addition to the `try_prove` validity check, this module exposes
//! `try_satisfiable` (used by the checker to flag contradictory
//! `requires` clauses) — same encoding pipeline, no negated goal.

use crate::ast::{BinaryOp, Expr, ExprKind, Type, UnaryOp};
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Debug)]
pub enum Verdict {
    /// Z3 returned `unsat` for the negation — the prove holds.
    Proven,
    /// Z3 returned `sat` — a counterexample exists. `counterexample` is a
    /// human-readable summary of the assignment that falsifies the claim
    /// (e.g. "x = -1, y = 0"), or `None` if the model couldn't be parsed.
    Disproven { counterexample: Option<String> },
    /// Z3 returned `unknown`, or our encoder timed out / errored.
    Unknown,
    /// The expression uses features we do not yet encode (floats, indexing,
    /// calls, etc.). Caller may fall back to other checks.
    SkippedUnsupported(String),
    /// No z3 binary found in $PATH, $Z3 env var, or a few common locations.
    Unavailable,
}

/// Outcome of a satisfiability check on a list of facts. Used to detect
/// contradictory `requires` clauses (which make every proof in the body
/// vacuously true).
#[derive(Debug)]
pub enum SatVerdict {
    /// z3 returned `sat` — there exists at least one model.
    Satisfiable,
    /// z3 returned `unsat` — the facts contradict each other.
    Unsatisfiable,
    /// z3 returned `unknown`, the encoder skipped some fact, or no z3
    /// was available. Caller should treat this as "not contradictory".
    Unknown,
}

/// Check whether `facts` are jointly satisfiable for some assignment of
/// `vars`. Skipped/unsupported facts are dropped (same policy as
/// `try_prove`), so the result is conservative — we never report
/// `Unsatisfiable` unless z3 actually proved the encoded subset
/// contradictory.
pub fn try_satisfiable(
    facts: &[Expr],
    vars: &[(String, Type)],
    versions: &HashMap<String, u32>,
) -> SatVerdict {
    let Some(z3) = find_z3() else {
        return SatVerdict::Unknown;
    };
    let mut out = String::new();
    out.push_str("(set-logic ALL)\n");
    let mut declared: HashMap<String, Type> = HashMap::new();
    for (name, ty) in vars {
        declared.insert(name.clone(), ty.clone());
        if matches!(ty.deref(), Type::Vec(_)) {
            let len_var = format!("{}_len", sanitize(name));
            out.push_str(&format!("(declare-const {} (_ BitVec 64))\n", len_var));
        }
        emit_versioned_array_declarations(name, ty, versions, &mut out);
        if !supported_sort(ty) {
            continue;
        }
        let sanitized = sanitize(name);
        if ty.is_integer() {
            let sort = int_bv_sort(ty).expect("integer types have BV sort");
            out.push_str(&format!("(declare-const {} {})\n", sanitized, sort));
        } else if ty.is_float() {
            let sort = fp_sort(ty).expect("float types have FP sort");
            out.push_str(&format!("(declare-const {} {})\n", sanitized, sort));
        } else if matches!(ty, Type::Bool) {
            out.push_str(&format!("(declare-const {} Bool)\n", sanitized));
        }
    }
    for f in facts {
        if let Ok(encoded) = encode_expr(f, &declared, None, versions) {
            out.push_str(&format!("(assert {})\n", encoded));
        }
    }
    out.push_str("(check-sat)\n");

    if std::env::var("INTENTC_SMT_DEBUG").is_ok() {
        eprintln!("--- SMT sat-query ---\n{}---", out);
    }
    let output = match run_z3(&out, &z3) {
        Ok(o) => o,
        Err(_) => return SatVerdict::Unknown,
    };
    let first = output.lines().next().unwrap_or("").trim();
    match first {
        "sat" => SatVerdict::Satisfiable,
        "unsat" => SatVerdict::Unsatisfiable,
        _ => SatVerdict::Unknown,
    }
}

/// Returns `true` when the user has opted out of compile-time
/// verification by setting `INTENTC_NO_VERIFY=1`. Verifier entry
/// points consult this to skip the SMT round-trip and accept the
/// claim. Runtime safety guards (bounds, divisor, shift,
/// `requires`/`assert` lowering) are unaffected — the program still
/// runs safely, but proof-only bugs won't surface at compile time.
/// Do not set this in CI.
pub fn verifier_disabled() -> bool {
    std::env::var("INTENTC_NO_VERIFY").is_ok()
}

/// Process-wide cache: SMT query string → raw z3 stdout.
///
/// Many verifier passes call `try_prove` with structurally similar
/// goals (bounds elision, requires verification, ensures checks),
/// and the same query body can be hit at multiple program points.
/// Caching the raw z3 output skips the fork+exec+z3-init overhead
/// on hits while letting each caller re-parse with its own `vars`
/// list (so counterexample names render against the right scope).
///
/// `INTENTC_SMT_NO_CACHE=1` disables the cache for debugging /
/// reproducing nondeterminism reports.
static SMT_CACHE: std::sync::Mutex<Option<std::collections::HashMap<String, String>>> =
    std::sync::Mutex::new(None);

fn cache_lookup(query: &str) -> Option<String> {
    if std::env::var("INTENTC_SMT_NO_CACHE").is_ok() {
        return None;
    }
    let guard = SMT_CACHE.lock().ok()?;
    guard.as_ref()?.get(query).cloned()
}

fn cache_store(query: String, output: String) {
    if std::env::var("INTENTC_SMT_NO_CACHE").is_ok() {
        return;
    }
    let Ok(mut guard) = SMT_CACHE.lock() else {
        return;
    };
    guard
        .get_or_insert_with(std::collections::HashMap::new)
        .insert(query, output);
}

pub fn try_prove(
    prove_expr: &Expr,
    requires: &[Expr],
    vars: &[(String, Type)],
    versions: &HashMap<String, u32>,
) -> Verdict {
    let Some(z3) = find_z3() else {
        return Verdict::Unavailable;
    };
    let query = match build_query(prove_expr, requires, vars, versions) {
        Ok(q) => q,
        Err(EncodeError::Unsupported(reason)) => return Verdict::SkippedUnsupported(reason),
    };
    // Append `(get-model)` so z3 prints the assignment on `sat`. On `unsat`
    // it's silently ignored, so we never have to do a second query.
    let query = format!("{}{}", query, "(get-model)\n");
    if std::env::var("INTENTC_SMT_DEBUG").is_ok() {
        eprintln!("--- SMT query ---\n{}---", query);
    }

    if let Some(cached) = cache_lookup(&query) {
        if std::env::var("INTENTC_SMT_DEBUG").is_ok() {
            eprintln!("--- SMT output (cached) ---\n{}---", cached);
        }
        return parse_verdict(&cached, vars);
    }

    match run_z3(&query, &z3) {
        Ok(output) => {
            if std::env::var("INTENTC_SMT_DEBUG").is_ok() {
                eprintln!("--- SMT output ---\n{}---", output);
            }
            cache_store(query, output.clone());
            parse_verdict(&output, vars)
        }
        Err(_) => Verdict::Unavailable,
    }
}

fn find_z3() -> Option<String> {
    if let Ok(path) = std::env::var("Z3") {
        if std::path::Path::new(&path).exists() {
            return Some(path);
        }
    }
    for candidate in ["z3", "/usr/bin/z3", "/usr/local/bin/z3", "/opt/z3/bin/z3"] {
        if Command::new(candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return Some(candidate.to_string());
        }
    }
    None
}

fn run_z3(query: &str, z3: &str) -> std::io::Result<String> {
    let mut child = Command::new(z3)
        .arg("-in")
        .arg("-T:5")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(query.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_verdict(output: &str, vars: &[(String, Type)]) -> Verdict {
    let first = output.lines().next().unwrap_or("").trim();
    match first {
        "unsat" => Verdict::Proven,
        "sat" => {
            let model = parse_model(output);
            let counterexample = format_counterexample(&model, vars);
            Verdict::Disproven { counterexample }
        }
        _ => Verdict::Unknown,
    }
}

/// Extract `(define-fun NAME () SORT VALUE)` entries from a z3 model
/// block. Z3 prints long values on their own lines, so we tokenize first
/// and walk the token stream paren-aware rather than scan line-by-line.
fn parse_model(output: &str) -> HashMap<String, String> {
    let tokens = tokenize_sexpr(output);
    let mut model = HashMap::new();
    let mut i = 0;
    while i + 1 < tokens.len() {
        if tokens[i] == "(" && tokens[i + 1] == "define-fun" {
            // Pattern: ( define-fun NAME ( ) SORT VALUE... )
            // where VALUE... is a balanced subsequence.
            let name_idx = i + 2;
            if name_idx >= tokens.len() {
                break;
            }
            let name = tokens[name_idx].clone();
            // Skip "( )"
            let mut j = name_idx + 1;
            if j >= tokens.len() || tokens[j] != "(" {
                i += 1;
                continue;
            }
            j += 1;
            // Walk until the matching ")" of the empty arg list.
            let mut depth: i32 = 1;
            while j < tokens.len() && depth > 0 {
                match tokens[j].as_str() {
                    "(" => depth += 1,
                    ")" => depth -= 1,
                    _ => {}
                }
                j += 1;
            }
            if j >= tokens.len() {
                break;
            }
            // Now at SORT. It's either a single atom (e.g. "Int", "Bool")
            // or a parenthesized form (e.g. `(_ FloatingPoint 11 53)`).
            if tokens[j] == "(" {
                let mut sd: i32 = 1;
                j += 1;
                while j < tokens.len() && sd > 0 {
                    match tokens[j].as_str() {
                        "(" => sd += 1,
                        ")" => sd -= 1,
                        _ => {}
                    }
                    j += 1;
                }
            } else {
                j += 1; // single-atom sort
            }
            if j >= tokens.len() {
                break;
            }
            // VALUE: read until the closing ) of the outer (define-fun ...).
            // We started at depth 1 (inside define-fun). Walk to depth 0.
            let value_start = j;
            let mut value_depth: i32 = 1;
            while j < tokens.len() && value_depth > 0 {
                match tokens[j].as_str() {
                    "(" => value_depth += 1,
                    ")" => value_depth -= 1,
                    _ => {}
                }
                if value_depth == 0 {
                    break;
                }
                j += 1;
            }
            let value_tokens = &tokens[value_start..j];
            let value = format_value(value_tokens);
            model.insert(name, value);
            i = j + 1;
        } else {
            i += 1;
        }
    }
    model
}

fn tokenize_sexpr(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for c in s.chars() {
        match c {
            '(' | ')' => {
                if !current.is_empty() {
                    out.push(current.clone());
                    current.clear();
                }
                out.push(c.to_string());
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    out.push(current.clone());
                    current.clear();
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn format_value(tokens: &[String]) -> String {
    // Single literal token.
    if tokens.len() == 1 {
        return tokens[0].clone();
    }
    // (- N) negative integer.
    if tokens.len() == 4
        && tokens[0] == "("
        && tokens[1] == "-"
        && tokens[3] == ")"
    {
        return format!("-{}", tokens[2]);
    }
    // Floating-point special values: `(_ NaN 11 53)`, `(_ +oo 11 53)`,
    // `(_ -oo 11 53)`, `(_ +zero 11 53)`, `(_ -zero 11 53)`.
    if tokens.len() >= 4 && tokens[0] == "(" && tokens[1] == "_" {
        let symbol = match tokens[2].as_str() {
            "NaN" => Some("NaN"),
            "+oo" => Some("+inf"),
            "-oo" => Some("-inf"),
            "+zero" => Some("0.0"),
            "-zero" => Some("-0.0"),
            _ => None,
        };
        if let Some(s) = symbol {
            return s.to_string();
        }
    }
    // Anything else: join with spaces (e.g. nested s-expr for fp values).
    tokens.join(" ")
}

fn format_counterexample(
    model: &HashMap<String, String>,
    vars: &[(String, Type)],
) -> Option<String> {
    if model.is_empty() {
        return None;
    }
    let mut entries: Vec<String> = Vec::new();
    for (name, ty) in vars {
        if !supported_sort(ty) && !matches!(ty.deref(), Type::Vec(_)) {
            continue;
        }
        let sanitized = sanitize(name);
        if let Some(value) = model.get(&sanitized) {
            entries.push(format!("{} = {}", name, render_value(value, ty)));
        }
        // Vec-length witnesses are 64-bit BVs (matching `u64`).
        let len_var = format!("{}_len", sanitized);
        if let Some(value) = model.get(&len_var) {
            entries.push(format!(
                "len({}) = {}",
                name,
                render_value(value, &Type::U64)
            ));
        }
    }
    if entries.is_empty() {
        None
    } else {
        Some(entries.join(", "))
    }
}

/// Render an SMT-model value as a human-readable string, given the original
/// language type of the variable. The model parser leaves hex BV literals
/// (`#xFF`) and `(_ bv5 8)` forms intact; here we convert them to decimal
/// with the right signedness.
fn render_value(raw: &str, ty: &Type) -> String {
    if let Some(bits) = parse_bv_bits(raw, ty) {
        return bits;
    }
    if let Some(s) = parse_fp_literal(raw, ty) {
        return s;
    }
    raw.to_string()
}

/// Decode z3's `(fp #b<sign> #b<exp> #x<mantissa>)` form into a
/// human-readable decimal. Returns `None` if the input doesn't look
/// like an FP literal — the caller falls back to the raw text.
fn parse_fp_literal(raw: &str, ty: &Type) -> Option<String> {
    if !ty.is_float() {
        return None;
    }
    // Strip "(" and trailing ")", then split into whitespace tokens.
    let trimmed = raw.trim();
    let trimmed = trimmed.strip_prefix('(')?.strip_suffix(')')?;
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    // Expected: ["fp", sign, exponent, mantissa] — there may be
    // extra whitespace handled by split_whitespace.
    if tokens.len() != 4 || tokens[0] != "fp" {
        // Special-case z3's named-FP constants (NaN, +oo, -oo, +zero, -zero).
        return match trimmed {
            t if t.contains("NaN") => Some("NaN".to_string()),
            t if t.contains("+oo") || t.contains("plus_infinity") => Some("+inf".to_string()),
            t if t.contains("-oo") || t.contains("minus_infinity") => Some("-inf".to_string()),
            t if t.contains("+zero") || t.contains("plus_zero") => Some("0.0".to_string()),
            t if t.contains("-zero") || t.contains("minus_zero") => Some("-0.0".to_string()),
            _ => None,
        };
    }
    let sign_bits = parse_bv_raw(tokens[1])?;
    let exp_bits = parse_bv_raw(tokens[2])?;
    let mant_bits = parse_bv_raw(tokens[3])?;
    match ty {
        Type::F64 => {
            // Recompose: 1 sign + 11 exp + 52 mantissa = 64 bits.
            let bits = ((sign_bits.0 & 1) << 63)
                | ((exp_bits.0 & ((1u128 << 11) - 1)) << 52)
                | (mant_bits.0 & ((1u128 << 52) - 1));
            Some(render_float(f64::from_bits(bits as u64)))
        }
        Type::F32 => {
            let bits = ((sign_bits.0 & 1) << 31)
                | ((exp_bits.0 & ((1u128 << 8) - 1)) << 23)
                | (mant_bits.0 & ((1u128 << 23) - 1));
            Some(render_float(f32::from_bits(bits as u32) as f64))
        }
        _ => None,
    }
}

/// Render a float in a readable form: special-case NaN/inf/zero,
/// use plain decimal for numbers near 1 (e.g. `3.14`), and
/// scientific notation for very large/small values where the plain
/// form would be ~hundreds of digits long.
fn render_float(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "+inf".to_string() } else { "-inf".to_string() };
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0.0".to_string() } else { "0.0".to_string() };
    }
    let abs = v.abs();
    if (1e-4..1e16).contains(&abs) {
        format!("{}", v)
    } else {
        format!("{:e}", v)
    }
}

/// Parse a `#bDIGITS` or `#xDIGITS` BV literal into (value, width-in-bits).
fn parse_bv_raw(s: &str) -> Option<(u128, u32)> {
    if let Some(rest) = s.strip_prefix("#x") {
        let value = u128::from_str_radix(rest, 16).ok()?;
        let width = (rest.len() as u32) * 4;
        Some((value, width))
    } else if let Some(rest) = s.strip_prefix("#b") {
        let value = u128::from_str_radix(rest, 2).ok()?;
        let width = rest.len() as u32;
        Some((value, width))
    } else {
        None
    }
}

fn parse_bv_bits(raw: &str, ty: &Type) -> Option<String> {
    let width = int_bits(ty)?;
    let unsigned = if let Some(hex) = raw.strip_prefix("#x") {
        u128::from_str_radix(hex, 16).ok()?
    } else if let Some(bin) = raw.strip_prefix("#b") {
        u128::from_str_radix(bin, 2).ok()?
    } else if let Some(rest) = raw.strip_prefix("(_ bv") {
        // Form: `(_ bvN W)` — extract N.
        let n_str = rest.split_whitespace().next()?;
        n_str.parse::<u128>().ok()?
    } else {
        return None;
    };
    if is_signed_int(ty) {
        // Interpret high bit as sign.
        let signed = if width < 128 && (unsigned >> (width - 1)) & 1 == 1 {
            let modulus = 1u128 << width;
            (unsigned as i128) - (modulus as i128)
        } else {
            unsigned as i128
        };
        Some(signed.to_string())
    } else {
        Some(unsigned.to_string())
    }
}

#[derive(Debug)]
enum EncodeError {
    Unsupported(String),
}

fn build_query(
    prove_expr: &Expr,
    requires: &[Expr],
    vars: &[(String, Type)],
    versions: &HashMap<String, u32>,
) -> Result<String, EncodeError> {
    let mut out = String::new();
    out.push_str("(set-logic ALL)\n");
    let mut declared: HashMap<String, Type> = HashMap::new();
    for (name, ty) in vars {
        // Record every binding's type in the map so the encoder can resolve
        // type-directed forms like `len(xs)`. Only emit SMT-LIB declarations
        // for the sorts we actually model (integers and bool).
        declared.insert(name.clone(), ty.clone());

        // For Vec<T> bindings (including by-ref forms `&Vec<T>` and
        // `&mut Vec<T>`), declare an opaque symbolic length `<name>_len` as
        // a 64-bit BitVec (matching the `u64` type of `len(xs)`). BitVec
        // values are non-negative by definition so no extra constraint is
        // needed.
        if matches!(ty.deref(), Type::Vec(_)) {
            let len_var = format!("{}_len", sanitize(name));
            out.push_str(&format!("(declare-const {} (_ BitVec 64))\n", len_var));
        }
        emit_versioned_array_declarations(name, ty, versions, &mut out);

        if !supported_sort(ty) {
            continue;
        }
        let sanitized = sanitize(name);
        if ty.is_integer() {
            let sort = int_bv_sort(ty).expect("integer types have BV sort");
            out.push_str(&format!("(declare-const {} {})\n", sanitized, sort));
            // BitVec naturally encodes the type's range — no extra range
            // constraint is needed (or possible — bvN values are mod 2^N).
        } else if ty.is_float() {
            let sort = fp_sort(ty).expect("float types have an FP sort");
            out.push_str(&format!("(declare-const {} {})\n", sanitized, sort));
            // No range / NaN / Inf constraints in v1 — the encoder accepts
            // the full IEEE-754 value space. Programs that need finiteness
            // can encode it with a `requires` clause and the structural
            // recognizer.
        } else if matches!(ty, Type::Bool) {
            out.push_str(&format!("(declare-const {} Bool)\n", sanitized));
        }
    }
    for req in requires {
        // Skip preconditions / facts we can't encode rather than failing
        // the whole query — they just don't contribute to the proof.
        if let Ok(encoded) = encode_expr(req, &declared, None, versions) {
            out.push_str(&format!("(assert {})\n", encoded));
        }
    }
    let encoded = encode_expr(prove_expr, &declared, None, versions)?;
    out.push_str(&format!("(assert (not {}))\n", encoded));
    out.push_str("(check-sat)\n");
    Ok(out)
}

fn int_bits(ty: &Type) -> Option<u32> {
    match ty {
        Type::I8 | Type::U8 => Some(8),
        Type::I16 | Type::U16 => Some(16),
        Type::I32 | Type::U32 => Some(32),
        Type::I64 | Type::U64 => Some(64),
        _ => None,
    }
}

fn int_bv_sort(ty: &Type) -> Option<String> {
    int_bits(ty).map(|w| format!("(_ BitVec {})", w))
}

fn is_signed_int(ty: &Type) -> bool {
    matches!(ty, Type::I8 | Type::I16 | Type::I32 | Type::I64)
}

/// Encode an integer literal as a BitVec of the given width.
/// Negative values become their two's-complement bit pattern.
fn encode_int_literal_bv(value: i128, width: u32) -> String {
    let mask: u128 = if width >= 128 {
        u128::MAX
    } else {
        (1u128 << width) - 1
    };
    let bits = (value as u128) & mask;
    format!("(_ bv{} {})", bits, width)
}

/// Walk the AST to find the integer type of a sub-expression, by consulting
/// `vars` for Var references and Cast targets. Returns None if no integer
/// context can be determined (e.g., the expression is only literals).
fn infer_int_type(expr: &Expr, vars: &HashMap<String, Type>) -> Option<Type> {
    match &expr.kind {
        ExprKind::Int(_) => None,
        ExprKind::Var(name) => vars
            .get(parse_versioned_name(name).0)
            .filter(|t| t.is_integer())
            .cloned(),
        ExprKind::Cast { ty, .. } if ty.is_integer() => Some(ty.clone()),
        ExprKind::Unary { expr, .. } => infer_int_type(expr, vars),
        ExprKind::Binary { left, right, .. } => infer_int_type(left, vars)
            .or_else(|| infer_int_type(right, vars)),
        ExprKind::Len { .. } => Some(Type::U64),
        // Reading from a named integer-element array returns the
        // element type, so the Binary path picks the right BitVec
        // width for a literal operand (e.g. `xs[0] == 10` over a
        // Vec<i32> needs literal `10` to encode as BV-32, not BV-64).
        // Honor `name#N` version suffix by stripping it before the
        // type lookup.
        ExprKind::Index { array, .. } => {
            if let ExprKind::Var(name) = &array.kind {
                let base = parse_versioned_name(name).0;
                if let Some(ty) = vars.get(base) {
                    if let Some(elem) = smt_array_element(ty) {
                        if elem.is_integer() {
                            return Some(elem.clone());
                        }
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn encode_int_binary(op: BinaryOp, l: &str, r: &str, signed: bool) -> Result<String, EncodeError> {
    use BinaryOp::*;
    Ok(match op {
        Add => format!("(bvadd {} {})", l, r),
        Sub => format!("(bvsub {} {})", l, r),
        Mul => format!("(bvmul {} {})", l, r),
        Div if signed => format!("(bvsdiv {} {})", l, r),
        Div => format!("(bvudiv {} {})", l, r),
        Rem if signed => format!("(bvsrem {} {})", l, r),
        Rem => format!("(bvurem {} {})", l, r),
        Lt if signed => format!("(bvslt {} {})", l, r),
        Lt => format!("(bvult {} {})", l, r),
        Le if signed => format!("(bvsle {} {})", l, r),
        Le => format!("(bvule {} {})", l, r),
        Gt if signed => format!("(bvsgt {} {})", l, r),
        Gt => format!("(bvugt {} {})", l, r),
        Ge if signed => format!("(bvsge {} {})", l, r),
        Ge => format!("(bvuge {} {})", l, r),
        Eq => format!("(= {} {})", l, r),
        Ne => format!("(distinct {} {})", l, r),
        BitAnd => format!("(bvand {} {})", l, r),
        BitOr => format!("(bvor {} {})", l, r),
        BitXor => format!("(bvxor {} {})", l, r),
        And | Or | Shl | Shr => {
            return Err(EncodeError::Unsupported(format!(
                "integer operator {:?} not supported in SMT v1 BitVec encoding",
                op
            )))
        }
    })
}

fn encode_float_binary(op: BinaryOp, l: &str, r: &str) -> Result<String, EncodeError> {
    use BinaryOp::*;
    Ok(match op {
        Add => format!("(fp.add RNE {} {})", l, r),
        Sub => format!("(fp.sub RNE {} {})", l, r),
        Mul => format!("(fp.mul RNE {} {})", l, r),
        Div => format!("(fp.div RNE {} {})", l, r),
        Lt => format!("(fp.lt {} {})", l, r),
        Le => format!("(fp.leq {} {})", l, r),
        Gt => format!("(fp.gt {} {})", l, r),
        Ge => format!("(fp.geq {} {})", l, r),
        Eq => format!("(fp.eq {} {})", l, r),
        Ne => format!("(not (fp.eq {} {}))", l, r),
        And | Or | BitAnd | BitOr | BitXor | Rem | Shl | Shr => {
            return Err(EncodeError::Unsupported(format!(
                "operator {:?} is not defined on floats",
                op
            )))
        }
    })
}

fn supported_sort(ty: &Type) -> bool {
    ty.is_integer() || ty.is_float() || matches!(ty, Type::Bool)
}

/// For a binding type that names an aggregate (Vec or Array, possibly
/// wrapped in a reference), return the leaf element type when it's
/// something we can model as an SMT array element — currently
/// integers (BitVec), bool, f32, and f64 (both FloatingPoint
/// sorts).
fn smt_array_element(ty: &Type) -> Option<&Type> {
    match ty.deref() {
        Type::Vec(elem) | Type::Array { element: elem, .. }
            if elem.is_integer() || matches!(**elem, Type::Bool) || elem.is_float() =>
        {
            Some(elem)
        }
        _ => None,
    }
}

/// Versioned-array declaration: emit
/// `(declare-const arr_<name>_v<K> (Array (_ BitVec 64) <ELEM>))`
/// for every version K from 0 up to the binding's current version
/// (inclusive). Each `xs[i] = v` IndexAssign bumps the version and
/// adds a synthetic `arr_xs_v{K+1} = (store arr_xs_vK i v)` axiom,
/// so older facts referencing earlier versions remain sound while
/// proofs about the post-assign state see the updated array.
fn emit_versioned_array_declarations(
    name: &str,
    ty: &Type,
    versions: &HashMap<String, u32>,
    out: &mut String,
) {
    let Some(elem) = smt_array_element(ty) else {
        return;
    };
    let elem_sort = if elem.is_integer() {
        let bits = int_bits(elem).expect("integer element has bits");
        format!("(_ BitVec {})", bits)
    } else if matches!(elem, Type::Bool) {
        "Bool".to_string()
    } else {
        // matches!(elem, Type::F32 | Type::F64) by virtue of smt_array_element.
        fp_sort(elem).expect("float element has FP sort").to_string()
    };
    let max_version = versions.get(name).copied().unwrap_or(0);
    for v in 0..=max_version {
        out.push_str(&format!(
            "(declare-const arr_{}_v{} (Array (_ BitVec 64) {}))\n",
            sanitize(name),
            v,
            elem_sort
        ));
    }
}

/// Split a binding-name string into `(base, Option<version>)`. The
/// checker emits explicit-version names like `"xs#3"` for synthetic
/// store-eq facts that span two SMT-array generations. Bare names
/// resolve to the current version via the `versions` map.
fn parse_versioned_name(s: &str) -> (&str, Option<u32>) {
    if let Some((base, ver)) = s.rsplit_once('#') {
        if let Ok(v) = ver.parse::<u32>() {
            return (base, Some(v));
        }
    }
    (s, None)
}

/// SMT name `arr_<sanitized_base>_v<version>` for the array
/// constant of the given binding at the given version. The version
/// comes from an explicit `name#N` suffix when present, or the
/// versions map's current entry otherwise (default 0).
fn smt_array_name_for(
    name: &str,
    versions: &HashMap<String, u32>,
) -> (String, u32) {
    let (base, explicit) = parse_versioned_name(name);
    let v = explicit.unwrap_or_else(|| versions.get(base).copied().unwrap_or(0));
    (format!("arr_{}_v{}", sanitize(base), v), v)
}

/// SMT-LIB FloatingPoint sort literal for f32 / f64.
fn fp_sort(ty: &Type) -> Option<&'static str> {
    match ty {
        Type::F32 => Some("(_ FloatingPoint 8 24)"),
        Type::F64 => Some("(_ FloatingPoint 11 53)"),
        _ => None,
    }
}

/// SMT-LIB `to_fp` constructor for f32 / f64.
fn to_fp(ty: &Type) -> Option<&'static str> {
    match ty {
        Type::F32 => Some("(_ to_fp 8 24)"),
        Type::F64 => Some("(_ to_fp 11 53)"),
        _ => None,
    }
}

fn sanitize(name: &str) -> String {
    let mut buf = String::with_capacity(name.len() + 1);
    buf.push('v');
    buf.push('_');
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            buf.push(ch);
        } else {
            buf.push('_');
        }
    }
    buf
}

/// Best-effort type inference at the AST layer so we can route binary ops
/// to integer vs floating-point SMT operators. We only need to distinguish
/// "is this expression float?" — the precise sort (f32 vs f64) doesn't
/// matter for choosing operator names.
/// Returns the *precise* FP type (f32 or f64) implied by `expr`'s
/// shape — analogous to `infer_int_type` but for floats. Used by
/// the encoder's float-binary path to pick a target sort for
/// `Float` literal operands so they match the surrounding array
/// element / cast / var type.
fn infer_float_type(expr: &Expr, vars: &HashMap<String, Type>) -> Option<Type> {
    match &expr.kind {
        ExprKind::Var(name) => vars
            .get(parse_versioned_name(name).0)
            .filter(|t| t.is_float())
            .cloned(),
        ExprKind::Cast { ty, .. } if ty.is_float() => Some(ty.clone()),
        ExprKind::Index { array, .. } => {
            if let ExprKind::Var(name) = &array.kind {
                let base = parse_versioned_name(name).0;
                if let Some(ty) = vars.get(base) {
                    if let Some(elem) = smt_array_element(ty) {
                        if elem.is_float() {
                            return Some(elem.clone());
                        }
                    }
                }
            }
            None
        }
        ExprKind::Unary { expr, .. } => infer_float_type(expr, vars),
        ExprKind::Binary { left, right, .. } => {
            infer_float_type(left, vars).or_else(|| infer_float_type(right, vars))
        }
        _ => None,
    }
}

fn infer_is_float(expr: &Expr, vars: &HashMap<String, Type>) -> bool {
    match &expr.kind {
        ExprKind::Float(_) => true,
        ExprKind::Var(name) => vars
            .get(parse_versioned_name(name).0)
            .map(|t| t.is_float())
            .unwrap_or(false),
        ExprKind::Cast { ty, .. } => ty.is_float(),
        ExprKind::Unary { expr, .. } => infer_is_float(expr, vars),
        ExprKind::Binary { left, right, .. } => {
            infer_is_float(left, vars) || infer_is_float(right, vars)
        }
        // Indexing a float-element array yields a float, so binary
        // ops like `xs[k] == 1.5` route to the FP path correctly.
        ExprKind::Index { array, .. } => {
            if let ExprKind::Var(name) = &array.kind {
                let base = parse_versioned_name(name).0;
                if let Some(ty) = vars.get(base) {
                    if let Some(elem) = smt_array_element(ty) {
                        return elem.is_float();
                    }
                }
            }
            false
        }
        _ => false,
    }
}

/// Infer the integer type of a `match` scrutinee for sizing literal
/// patterns. Looks up bare variables in `vars`; returns None if the
/// scrutinee's type can't be statically resolved here (more complex
/// expressions fall back to i64-sized literal patterns).
fn infer_scrutinee_int_type(
    scrutinee: &Expr,
    vars: &HashMap<String, Type>,
) -> Option<Type> {
    match &scrutinee.kind {
        ExprKind::Var(name) => {
            vars.get(name).cloned().filter(|t| int_bits(t).is_some())
        }
        ExprKind::Int(_) => Some(Type::I64),
        _ => None,
    }
}

fn encode_expr(
    expr: &Expr,
    vars: &HashMap<String, Type>,
    target_int: Option<&Type>,
    versions: &HashMap<String, u32>,
) -> Result<String, EncodeError> {
    match &expr.kind {
        ExprKind::Int(value) => {
            // Integer literals are encoded as BitVec of the surrounding
            // context's width. Default to i64 when no context is available.
            let ty = target_int.cloned().unwrap_or(Type::I64);
            let width = int_bits(&ty).unwrap_or(64);
            Ok(encode_int_literal_bv(*value, width))
        }
        ExprKind::Bool(value) => Ok(if *value { "true".into() } else { "false".into() }),
        ExprKind::Str(_) => Err(EncodeError::Unsupported(
            "string literal in proof not supported".into(),
        )),
        ExprKind::Float(value) => {
            // We don't know the target sort here; pick f64 by default.
            Ok(format!("((_ to_fp 11 53) RNE {})", value))
        }
        ExprKind::Var(name) => {
            // Versioned names (`xs#N`) are reserved for synthetic
            // array-relation Calls (`__smt_store_eq`, `__smt_array_eq`)
            // that handle them directly. A versioned name reaching
            // the bare-Var arm means a synthetic fact slipped past
            // its intended interceptor; reject explicitly rather
            // than silently sanitizing into a never-declared SMT
            // const.
            if name.contains('#') {
                return Err(EncodeError::Unsupported(format!(
                    "versioned name '{}' not valid in scalar position",
                    name
                )));
            }
            let Some(ty) = vars.get(name) else {
                return Err(EncodeError::Unsupported(format!(
                    "variable '{}' is not declared in the SMT context (type may be unsupported)",
                    name
                )));
            };
            if !supported_sort(ty) {
                return Err(EncodeError::Unsupported(format!(
                    "variable '{}' has unsupported type {} for SMT",
                    name, ty
                )));
            }
            Ok(sanitize(name))
        }
        ExprKind::Unary { op, expr } => {
            let inner = encode_expr(expr, vars, target_int, versions)?;
            let is_float = infer_is_float(expr, vars);
            Ok(match op {
                UnaryOp::Neg if is_float => format!("(fp.neg {})", inner),
                UnaryOp::Neg => format!("(bvneg {})", inner),
                UnaryOp::Not => format!("(not {})", inner),
            })
        }
        ExprKind::Binary { op, left, right } => {
            let is_float =
                infer_is_float(left, vars) || infer_is_float(right, vars);
            if is_float {
                // Pick a target FP type so a Float literal operand
                // takes the right `to_fp` constructor (f32 vs f64).
                // Without this, every f32 array comparison would
                // mismatch sorts: the array's element is FP-8/24
                // but the literal would default to FP-11/53.
                let target = infer_float_type(left, vars)
                    .or_else(|| infer_float_type(right, vars))
                    .unwrap_or(Type::F64);
                let l = encode_float_operand(left, vars, &target, versions)?;
                let r = encode_float_operand(right, vars, &target, versions)?;
                return encode_float_binary(*op, &l, &r);
            }

            // Integer path: figure out the operand type (controls literal
            // widths AND signed/unsigned operator selection).
            let op_ty = infer_int_type(left, vars)
                .or_else(|| infer_int_type(right, vars))
                .unwrap_or(Type::I64);

            // Shifts: BitVec `bv{shl,lshr,ashr}` require both operands to
            // share width. Encode the rhs at its natural type, then pad or
            // truncate it to match the lhs width. Choose `bvashr` for signed
            // right-shifts so the sign bit is replicated (matching C's
            // shift-of-signed semantics).
            if matches!(op, BinaryOp::Shl | BinaryOp::Shr) {
                let lhs_w = int_bits(&op_ty).unwrap_or(64);
                let rhs_ty = infer_int_type(right, vars).unwrap_or(op_ty.clone());
                let rhs_w = int_bits(&rhs_ty).unwrap_or(64);
                let l = encode_expr(left, vars, Some(&op_ty), versions)?;
                let r_encoded = encode_expr(right, vars, Some(&rhs_ty), versions)?;
                let r_matched = if rhs_w == lhs_w {
                    r_encoded
                } else if rhs_w < lhs_w {
                    let pad = lhs_w - rhs_w;
                    let extend = if is_signed_int(&rhs_ty) {
                        "sign_extend"
                    } else {
                        "zero_extend"
                    };
                    format!("((_ {} {}) {})", extend, pad, r_encoded)
                } else {
                    format!("((_ extract {} 0) {})", lhs_w - 1, r_encoded)
                };
                let shift_op = match (op, is_signed_int(&op_ty)) {
                    (BinaryOp::Shl, _) => "bvshl",
                    (BinaryOp::Shr, true) => "bvashr",
                    (BinaryOp::Shr, false) => "bvlshr",
                    _ => unreachable!(),
                };
                return Ok(format!("({} {} {})", shift_op, l, r_matched));
            }

            let l = encode_expr(left, vars, Some(&op_ty), versions)?;
            let r = encode_expr(right, vars, Some(&op_ty), versions)?;

            // Boolean connectives stay in the Bool theory.
            if matches!(op, BinaryOp::And | BinaryOp::Or) {
                let smt = if matches!(op, BinaryOp::And) { "and" } else { "or" };
                return Ok(format!("({} {} {})", smt, l, r));
            }

            encode_int_binary(*op, &l, &r, is_signed_int(&op_ty))
        }
        ExprKind::Cast { expr, ty } => {
            let source_is_float = infer_is_float(expr, vars);
            if ty.is_integer() && source_is_float {
                // Float → integer: SMT-LIB has `(_ fp.to_sbv N)` and
                // `(_ fp.to_ubv N)`. RNE rounds half-to-even; the C
                // backend uses round-toward-zero (`(int)x`), but for
                // proof purposes RNE is a sound approximation when the
                // user reasons about non-fractional values.
                let target_w = int_bits(ty).expect("integer types have a bit width");
                let conv = if is_signed_int(ty) {
                    format!("(_ fp.to_sbv {})", target_w)
                } else {
                    format!("(_ fp.to_ubv {})", target_w)
                };
                let inner = encode_expr(expr, vars, None, versions)?;
                Ok(format!("({} RNE {})", conv, inner))
            } else if ty.is_integer() {
                // Integer cast: widen with sign_extend / zero_extend, narrow
                // with extract, same-width with identity.
                let target_w = int_bits(ty).expect("integer types have a bit width");
                let source_ty = infer_int_type(expr, vars).unwrap_or(Type::I64);
                let source_w = int_bits(&source_ty).unwrap_or(64);
                let inner = encode_expr(expr, vars, Some(&source_ty), versions)?;
                if target_w == source_w {
                    Ok(inner)
                } else if target_w > source_w {
                    let extend = if is_signed_int(&source_ty) {
                        "sign_extend"
                    } else {
                        "zero_extend"
                    };
                    let pad = target_w - source_w;
                    Ok(format!("((_ {} {}) {})", extend, pad, inner))
                } else {
                    // Narrowing: take the lower `target_w` bits.
                    Ok(format!(
                        "((_ extract {} 0) {})",
                        target_w - 1,
                        inner
                    ))
                }
            } else if ty.is_float() && source_is_float {
                // Float → float (e.g. f32 → f64). `to_fp` from another
                // FP sort needs a rounding mode argument; RNE is the
                // standard choice.
                let to = to_fp(ty).expect("float types have to_fp");
                let inner = encode_expr(expr, vars, None, versions)?;
                Ok(format!("({} RNE {})", to, inner))
            } else if ty.is_float() {
                // Integer → float (existing path).
                let source_ty = infer_int_type(expr, vars);
                let inner = encode_expr(expr, vars, source_ty.as_ref(), versions)?;
                let to = to_fp(ty).expect("float types have to_fp");
                Ok(format!("({} RNE {})", to, inner))
            } else {
                Err(EncodeError::Unsupported(format!(
                    "cast to {} not supported in SMT v1",
                    ty
                )))
            }
        }
        ExprKind::Len { array } => {
            // For fixed-size arrays — including arrays passed by `&` or
            // `&mut` reference — len() is a compile-time constant.
            // For Vec<T> (also by-ref), use the per-binding symbolic
            // length variable declared in `build_query`. Length is
            // invariant across `xs[i] = v` so it doesn't need
            // versioning; strip `#N` suffix before lookup.
            if let ExprKind::Var(name) = &array.kind {
                let base = parse_versioned_name(name).0;
                if let Some(ty) = vars.get(base) {
                    let underlying = ty.deref();
                    if let Type::Array { length, .. } = underlying {
                        // `len(arr)` returns u64; emit as a 64-bit BV literal.
                        return Ok(encode_int_literal_bv(*length as i128, 64));
                    }
                    if matches!(underlying, Type::Vec(_)) {
                        return Ok(format!("{}_len", sanitize(base)));
                    }
                }
            }
            Err(EncodeError::Unsupported(
                "len() in proofs requires a named array/Vec binding".into(),
            ))
        }
        ExprKind::Call { name, args, .. } if name == "__smt_store_eq" && args.len() == 4 => {
            // Synthetic store-equality axiom emitted by the checker
            // for `let ys = set(xs, k, v)`: encodes as
            //   (= arr_ys (store arr_xs k v))
            // Read by the SMT solver as: ys is xs with slot k
            // replaced by v. Lets proofs about *other* slots
            // (`ys[j] == xs[j]` for `j != k`) discharge alongside
            // the trivial `ys[k] == v`.
            //
            // args = [Var(ys), Var(xs), index_expr, value_expr]
            let (Some(result), Some(base)) = (
                var_name(&args[0]),
                var_name(&args[1]),
            ) else {
                return Err(EncodeError::Unsupported(
                    "__smt_store_eq base/result must be Vars".into(),
                ));
            };
            // Names may carry explicit `#N` version suffixes (e.g.,
            // `"xs#2"`) emitted by the IndexAssign handler when the
            // store-eq fact crosses two versions of the same binding.
            // `smt_array_name_for` resolves both versioned and bare
            // forms to `arr_<base>_v<N>`.
            let result_base = parse_versioned_name(result).0;
            let Some(result_ty) = vars.get(result_base) else {
                return Err(EncodeError::Unsupported(format!(
                    "no SMT model for result '{}'",
                    result
                )));
            };
            let Some(elem) = smt_array_element(result_ty) else {
                return Err(EncodeError::Unsupported(format!(
                    "result '{}' has unsupported element type",
                    result
                )));
            };
            let idx_bv = encode_index_to_64(&args[2], vars, versions)?;
            let val = if elem.is_integer() {
                encode_expr(&args[3], vars, Some(elem), versions)?
            } else {
                encode_expr(&args[3], vars, None, versions)?
            };
            let (result_name, _) = smt_array_name_for(result, versions);
            let (base_name, _) = smt_array_name_for(base, versions);
            Ok(format!(
                "(= {} (store {} {} {}))",
                result_name, base_name, idx_bv, val
            ))
        }
        ExprKind::Call { name, args, .. } if name == "__smt_array_eq" && args.len() == 2 => {
            // Synthetic array-equality axiom emitted by the checker
            // for `let ys = clone(xs)`: encodes as
            //   (= arr_ys arr_xs)
            // The clone copies every slot, so the solver gets all
            // `ys[j] == xs[j]` facts for free.
            let (Some(result), Some(base)) = (
                var_name(&args[0]),
                var_name(&args[1]),
            ) else {
                return Err(EncodeError::Unsupported(
                    "__smt_array_eq result/base must be Vars".into(),
                ));
            };
            let result_base = parse_versioned_name(result).0;
            if vars.get(result_base).and_then(smt_array_element).is_none() {
                return Err(EncodeError::Unsupported(format!(
                    "result '{}' is not an SMT array binding",
                    result
                )));
            }
            let (result_name, _) = smt_array_name_for(result, versions);
            let (base_name, _) = smt_array_name_for(base, versions);
            Ok(format!("(= {} {})", result_name, base_name))
        }
        ExprKind::Call { name, .. } => Err(EncodeError::Unsupported(format!(
            "function call '{}' not supported in SMT v1",
            name
        ))),
        ExprKind::ArrayLit { .. } => Err(EncodeError::Unsupported(
            "array literals not supported in SMT v1".into(),
        )),
        ExprKind::Index { array, index } => {
            // Array indexing: `xs[i]` becomes `(select arr_xs_v<N>
            // i)` where N is the binding's current version. The
            // checker tracks per-binding versions: each `xs[i] = v`
            // IndexAssign bumps the counter and emits a synthetic
            // store-eq fact that links old and new arrays, so old
            // facts referring to earlier versions stay sound.
            // Explicit `name#N` suffix (emitted by IndexAssign) is
            // honored too — the synthetic store-eq fact carries
            // both new and old version names.
            let ExprKind::Var(name) = &array.kind else {
                return Err(EncodeError::Unsupported(
                    "SMT array indexing requires a named binding base".into(),
                ));
            };
            let base = parse_versioned_name(name).0;
            let Some(ty) = vars.get(base) else {
                return Err(EncodeError::Unsupported(format!(
                    "no SMT model for indexed binding '{}'",
                    name
                )));
            };
            if smt_array_element(ty).is_none() {
                return Err(EncodeError::Unsupported(format!(
                    "indexed binding '{}' has unsupported element type",
                    name
                )));
            }
            let idx_bv = encode_index_to_64(index, vars, versions)?;
            let (arr_name, _) = smt_array_name_for(name, versions);
            Ok(format!("(select {} {})", arr_name, idx_bv))
        }
        ExprKind::Ref { .. } | ExprKind::RefMut { .. } => Err(EncodeError::Unsupported(
            "references not supported in SMT v1".into(),
        )),
        ExprKind::Tuple(_) | ExprKind::TupleAccess { .. } => {
            Err(EncodeError::Unsupported(
                "tuples not supported in SMT v1".into(),
            ))
        }
        ExprKind::StructLit { .. } | ExprKind::FieldAccess { .. } => {
            Err(EncodeError::Unsupported(
                "structs not supported in SMT v1".into(),
            ))
        }
        ExprKind::Match { scrutinee, arms } => {
            // Encode as a chain of `(ite (= scrutinee N) body
            // …)` for integer patterns, with the trailing
            // wildcard arm (or last specific arm if no
            // wildcard) as the final else. Enum-variant
            // patterns are deferred — they'd need the
            // EnumInfo registry plumbed through to the SMT
            // layer to translate variant names to tag values.
            // For v1, an unsupported enum-variant pattern
            // bails the whole encoding.
            let scrutinee_str = encode_expr(scrutinee, vars, None, versions)?;
            let scrutinee_ty = infer_scrutinee_int_type(scrutinee, vars);
            // Build the chain from right to left: start with
            // the last arm's body, then wrap each prior arm in
            // an ite.
            if arms.is_empty() {
                return Err(EncodeError::Unsupported(
                    "match with no arms cannot be encoded".into(),
                ));
            }
            // Find the "default" tail body: a wildcard arm if
            // present, else the last arm's body (which by
            // checker rules covers the remaining variants).
            let mut acc: Option<String> = None;
            for arm in arms.iter().rev() {
                let body_str = encode_expr(&arm.body, vars, target_int, versions)?;
                match &arm.pattern {
                    crate::ast::Pattern::Wildcard => {
                        // Wildcard always evaluates to its body.
                        acc = Some(body_str);
                    }
                    crate::ast::Pattern::Int(value) => {
                        let lit_bv = match scrutinee_ty.as_ref() {
                            Some(ty) => encode_int_literal_bv(
                                *value,
                                int_bits(ty).unwrap_or(64),
                            ),
                            None => encode_int_literal_bv(*value, 64),
                        };
                        let cond = format!("(= {} {})", scrutinee_str, lit_bv);
                        acc = Some(match acc {
                            Some(prev) => format!("(ite {} {} {})", cond, body_str, prev),
                            // No fallthrough — last arm IS this
                            // integer pattern. Per checker rules
                            // the match was exhaustive only via a
                            // wildcard above; reaching this with
                            // no `acc` means a missing wildcard
                            // and a non-total match. Bail.
                            None => return Err(EncodeError::Unsupported(
                                "match without a wildcard arm cannot be encoded in SMT (would be partial)".into(),
                            )),
                        });
                    }
                    crate::ast::Pattern::Bool(_) => {
                        return Err(EncodeError::Unsupported(
                            "bool match patterns not yet supported in SMT".into(),
                        ));
                    }
                    crate::ast::Pattern::Str(_) => {
                        return Err(EncodeError::Unsupported(
                            "string match patterns not yet supported in SMT".into(),
                        ));
                    }
                    crate::ast::Pattern::Float(_) => {
                        return Err(EncodeError::Unsupported(
                            "float match patterns not yet supported in SMT".into(),
                        ));
                    }
                    crate::ast::Pattern::Variant { .. } => {
                        return Err(EncodeError::Unsupported(
                            "enum-variant match patterns not yet supported in SMT".into(),
                        ));
                    }
                    crate::ast::Pattern::VariantWithBinding { .. } => {
                        return Err(EncodeError::Unsupported(
                            "payloaded variant destructure patterns not yet supported in SMT".into(),
                        ));
                    }
                }
            }
            acc.ok_or_else(|| EncodeError::Unsupported(
                "match arm chain ended up empty".into(),
            ))
        }
        ExprKind::MethodCall { .. } => Err(EncodeError::Unsupported(
            "method calls not supported in SMT v1".into(),
        )),
        ExprKind::IfExpr { cond, then_value, else_value } => {
            // Z3 encoding: `(ite cond-bool then else)`. The
            // checker has already proven both branches unify
            // to the same type, so target_int (if given)
            // applies to both branches uniformly.
            let cond_str = encode_expr(cond, vars, None, versions)?;
            let then_str = encode_expr(then_value, vars, target_int, versions)?;
            let else_str = encode_expr(else_value, vars, target_int, versions)?;
            Ok(format!("(ite {} {} {})", cond_str, then_str, else_str))
        }
        ExprKind::Block { .. } => Err(EncodeError::Unsupported(
            "block expressions not supported in SMT v1".into(),
        )),
        ExprKind::Try { .. } => Err(EncodeError::Unsupported(
            "try expressions not supported in SMT v1".into(),
        )),
        ExprKind::AnonFn { .. } => Err(EncodeError::Unsupported(
            "anonymous fn expressions not supported in SMT v1".into(),
        )),
    }
}

/// Encode a float operand with a known target FP precision, so a
/// `Float` literal takes the right `to_fp` constructor (f32 vs
/// f64). Non-literal operands fall through to the normal encoder —
/// their sort comes from their own type (e.g., `(select arr_xs i)`
/// is FP-N for an arr_xs declared with element sort FP-N).
fn encode_float_operand(
    expr: &Expr,
    vars: &HashMap<String, Type>,
    target_fp: &Type,
    versions: &HashMap<String, u32>,
) -> Result<String, EncodeError> {
    if let ExprKind::Float(value) = &expr.kind {
        let constructor = fp_to_fp(target_fp)
            .ok_or_else(|| EncodeError::Unsupported("target_fp must be F32 or F64".into()))?;
        return Ok(format!("({} RNE {})", constructor, value));
    }
    encode_expr(expr, vars, None, versions)
}

/// SMT-LIB `to_fp` constructor for a given float type. Mirrors the
/// existing `fp_sort`/`to_fp` helpers but kept inline so the
/// float-operand encoder can pick its constructor without going
/// through `to_fp`'s `&'static str` indirection.
fn fp_to_fp(ty: &Type) -> Option<&'static str> {
    match ty {
        Type::F32 => Some("(_ to_fp 8 24)"),
        Type::F64 => Some("(_ to_fp 11 53)"),
        _ => None,
    }
}

/// If `expr` is a bare `Var(name)`, return the name; else None.
/// Used by the synthetic-Call special cases to confirm their AST
/// args are simple binding references.
fn var_name(expr: &Expr) -> Option<&str> {
    if let ExprKind::Var(name) = &expr.kind {
        Some(name.as_str())
    } else {
        None
    }
}

/// Encode an integer expression as a 64-bit BitVec, sign- or zero-
/// extending if narrower. Used by SMT array reads (`(select arr i)`)
/// and the synthetic store fact, both of which need a 64-bit index.
fn encode_index_to_64(
    index: &Expr,
    vars: &HashMap<String, Type>,
    versions: &HashMap<String, u32>,
) -> Result<String, EncodeError> {
    let raw = encode_expr(index, vars, None, versions)?;
    let idx_ty = infer_int_type(index, vars).unwrap_or(Type::U64);
    if matches!(idx_ty, Type::I64 | Type::U64) {
        return Ok(raw);
    }
    if let Some(bits) = int_bits(&idx_ty) {
        let pad = 64 - bits;
        let op = if idx_ty.is_signed_integer() {
            "sign_extend"
        } else {
            "zero_extend"
        };
        return Ok(format!("((_ {} {}) {})", op, pad, raw));
    }
    Err(EncodeError::Unsupported(
        "index expression must be an integer".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::Span;

    fn s() -> Span {
        Span::new(0, 0)
    }

    fn lit_int(v: i128) -> Expr {
        Expr {
            kind: ExprKind::Int(v),
            span: s(),
        }
    }

    fn var(name: &str) -> Expr {
        Expr {
            kind: ExprKind::Var(name.into()),
            span: s(),
        }
    }

    fn binary(op: BinaryOp, l: Expr, r: Expr) -> Expr {
        Expr {
            kind: ExprKind::Binary {
                op,
                left: Box::new(l),
                right: Box::new(r),
            },
            span: s(),
        }
    }

    #[test]
    fn proves_constant_tautology() {
        let expr = binary(BinaryOp::Eq, lit_int(2), lit_int(2));
        let v = try_prove(&expr, &[], &[], &HashMap::new());
        assert!(matches!(v, Verdict::Proven), "got {:?}", v);
    }

    #[test]
    fn proves_universally_true_inequality_over_i64() {
        // For all x : i64, x + 1 != x.
        let lhs = binary(BinaryOp::Add, var("x"), lit_int(1));
        let expr = binary(BinaryOp::Ne, lhs, var("x"));
        let vars = vec![("x".to_string(), Type::I64)];
        let v = try_prove(&expr, &[], &vars, &HashMap::new());
        assert!(matches!(v, Verdict::Proven), "got {:?}", v);
    }

    #[test]
    fn rejects_unprovable_claim() {
        // Claim "x > 0" without any requires — z3 finds counterexample x = 0.
        let expr = binary(BinaryOp::Gt, var("x"), lit_int(0));
        let vars = vec![("x".to_string(), Type::I64)];
        let v = try_prove(&expr, &[], &vars, &HashMap::new());
        assert!(matches!(v, Verdict::Disproven { .. }), "got {:?}", v);
    }

    #[test]
    fn proves_under_requires_precondition() {
        // requires x > 0; prove x >= 1.
        let req = binary(BinaryOp::Gt, var("x"), lit_int(0));
        let expr = binary(BinaryOp::Ge, var("x"), lit_int(1));
        let vars = vec![("x".to_string(), Type::I64)];
        let v = try_prove(&expr, &[req], &vars, &HashMap::new());
        assert!(matches!(v, Verdict::Proven), "got {:?}", v);
    }

    #[test]
    fn unsupported_features_return_skipped() {
        // Function calls in proof expressions aren't encoded in SMT v1.
        let expr = Expr {
            kind: ExprKind::Call {
                name: "user_fn".into(),
                name_span: crate::span::Span::default(),
                args: vec![],
            },
            span: s(),
        };
        let v = try_prove(&expr, &[], &[], &HashMap::new());
        match v {
            Verdict::SkippedUnsupported(_) => {}
            other => panic!("expected SkippedUnsupported, got {:?}", other),
        }
    }
}
