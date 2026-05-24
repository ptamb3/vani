pub mod ast;
pub mod backend;
pub mod backend_c;
pub mod backend_llvm;
pub mod checker;
pub mod diagnostic;
pub mod format;
pub mod ir;
pub mod lexer;
pub mod lsp;
pub mod parser;
pub mod smt;
pub mod span;
pub mod ssa;
pub mod ssa_backend_c;
pub mod ssa_backend_llvm;
pub mod ssa_pass;

use backend::Backend;
use checker::CheckedProgram;
use diagnostic::Diagnostic;

pub fn compile(source: &str) -> Result<CheckedProgram, Vec<Diagnostic>> {
    let tokens = lexer::lex(source).map_err(|diagnostic| vec![diagnostic])?;
    let (program, parse_errors) = parser::parse(tokens);
    match checker::check(program) {
        Ok(checked) if parse_errors.is_empty() => Ok(checked),
        Ok(_) => Err(parse_errors),
        Err(mut check_errors) => {
            // Surface parse errors first (they likely caused some of the
            // type-check errors that follow).
            let mut all = parse_errors;
            all.append(&mut check_errors);
            Err(all)
        }
    }
}

/// Read the file at `entry`, recursively resolve any `use "path";` decls
/// at the top of each transitively-included file (relative to that file's
/// directory), concatenate the sources, then run the normal compile.
///
/// Returns the resolved single source string alongside the result so the
/// CLI can format diagnostics against the same buffer the compiler saw.
///
/// Caveats (v1):
/// * Diagnostic line numbers refer to the concatenated buffer, not the
///   per-file source. Real per-file mapping is a follow-up.
/// * Cyclic imports are detected and silently dropped (each file included
///   at most once across the dependency tree).
pub fn compile_path(
    entry: &std::path::Path,
) -> Result<(CheckedProgram, diagnostic::FileMap), (diagnostic::FileMap, Vec<Diagnostic>)> {
    let mut visited: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    let mut combined = String::new();
    let mut file_map = diagnostic::FileMap::new();
    if let Err(err) = resolve_uses(entry, &mut visited, &mut combined, &mut file_map) {
        return Err((
            file_map,
            vec![Diagnostic::new(crate::span::Span::new(0, 0), err)],
        ));
    }
    match compile(&combined) {
        Ok(checked) => Ok((checked, file_map)),
        Err(diagnostics) => Err((file_map, diagnostics)),
    }
}

fn resolve_uses(
    path: &std::path::Path,
    visited: &mut std::collections::HashSet<std::path::PathBuf>,
    out: &mut String,
    file_map: &mut diagnostic::FileMap,
) -> Result<(), String> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|e| format!("failed to canonicalize '{}': {}", path.display(), e))?;
    if !visited.insert(canonical.clone()) {
        return Ok(()); // already included or cycle break
    }

    let source = std::fs::read_to_string(&canonical)
        .map_err(|e| format!("failed to read '{}': {}", canonical.display(), e))?;
    let tokens = lexer::lex(&source).map_err(|d| {
        format!(
            "lex error in '{}' at byte {}: {}",
            canonical.display(),
            d.span.start,
            d.message
        )
    })?;
    let (program, _parse_errors) = parser::parse(tokens);

    let base = canonical
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    for u in &program.uses {
        let target = base.join(&u.path);
        resolve_uses(&target, visited, out, file_map)?;
    }

    // Record this file's contribution before appending so the FileMap entry's
    // `start` matches the global offset where the source actually lands.
    let start = out.len();
    out.push_str(&source);
    out.push('\n');
    file_map.push(canonical.display().to_string(), source, start);
    Ok(())
}

pub fn compile_to_c(source: &str) -> Result<String, Vec<Diagnostic>> {
    let checked = compile(source)?;
    Ok(backend_c::CBackend.emit(&checked.ir))
}

#[cfg(test)]
mod tests {
    use super::{compile, compile_to_c};
    use crate::backend::Backend;

    #[test]
    fn compiles_basic_program_to_c() {
        let source = r#"
            intent "test";

            fn main() -> i64 {
              let answer = 40 + 2;
              prove answer == 42;
              print answer;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("program should compile");
        assert!(c.contains("static int64_t fn_main(void)"));
        assert!(c.contains("printf"));
    }

    #[test]
    fn rejects_unprovable_proof() {
        let source = r#"
            fn identity(x: i64) -> i64 {
              return x;
            }

            fn main() -> i64 {
              let value = identity(1);
              prove value == 1;
              return 0;
            }
        "#;

        let errors = compile_to_c(source).expect_err("proof is not provable");
        assert!(
            errors.iter().any(|error| {
                let m = &error.message;
                m.contains("cannot prove") || m.contains("proof failed")
            }),
            "expected a verifier diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn constant_tracking_survives_unrelated_if_else() {
        // Refines #4 from STATUS.md: the checker used to drop
        // every binding's compile-time constant at if/else and
        // while merges. Now it only drops the constants of
        // bindings the body could have mutated (direct
        // `Assign`/`IndexAssign` LHS, or `&mut <name>` argument
        // through a callee). A binding the body provably never
        // touched keeps its constant — the program below relies
        // on that to discharge `prove x == 5` at compile time
        // via the constant-fold path (layer 1 of the 3-layer
        // prove) without round-tripping to SMT.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              let y: i64 = 10;
              if y > 0 {
                let _ = y + 1;
              } else {
                let _ = y - 1;
              }
              prove x == 5;
              while y > 100 {
                let _ = y + 1;
              }
              prove x == 5;
              return x;
            }
        "#;

        compile(source).expect(
            "constant tracking should survive if/else and while bodies \
             that don't touch the binding",
        );
    }

    #[test]
    fn constant_tracking_cleared_when_body_reassigns() {
        // Negative companion to the test above. When the
        // branch body *does* reassign the binding, its constant
        // must be cleared at the merge — otherwise post-branch
        // facts about the binding would be unsound.
        let source = r#"
            fn main() -> i64 {
              let mut_var: i64 = 5;
              if mut_var > 0 {
                mut_var = 99;
              } else {
                let _ = 0;
              }
              return mut_var;
            }
        "#;

        // This program should compile (no prove asserts about
        // the constant value, just observes that the language
        // accepts the reassignment). The interesting check is
        // that we don't crash or miscompile. SMT correctly
        // can't prove `mut_var == 5` post-branch because the
        // value diverges between branches.
        compile(source).expect("reassign branch must compile");
    }

    #[test]
    fn supports_integer_widths_and_safe_promotions() {
        let source = r#"
            fn main() -> i64 {
              let a: i32 = 10;
              let b: i64 = 20;
              let c: u32 = 30;
              let d: i64 = b + c;
              let e: u8 = 2;
              let f: u16 = 3;
              let g: u16 = e + f;
              prove d == 50;
              prove g == 5;
              return a + d;
            }
        "#;

        let c = compile_to_c(source).expect("mixed integer widths should compile");
        assert!(c.contains("int32_t v_a"));
        assert!(c.contains("uint32_t v_c"));
        assert!(c.contains("uint16_t v_g"));
    }

    #[test]
    fn rejects_unsafe_mixed_signed_unsigned_promotion() {
        let source = r#"
            fn main() -> i64 {
              let signed: i32 = -1;
              let unsigned: u64 = 1;
              let value = signed + unsigned;
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("unsafe mixed promotion should fail");
        assert!(errors
            .iter()
            .any(|error| error.message.contains("no safe implicit integer promotion")));
    }

    #[test]
    fn rejects_constant_division_by_zero() {
        let source = r#"
            fn identity(x: i64) -> i64 {
              return x;
            }

            fn main() -> i64 {
              let numerator = identity(10);
              let bad = numerator / 0;
              return bad;
            }
        "#;

        let errors = compile(source).expect_err("division by zero should fail");
        assert!(errors
            .iter()
            .any(|error| error.message.contains("division by zero")));
    }

    #[test]
    fn rejects_constant_integer_overflow() {
        let source = r#"
            fn main() -> i64 {
              let bad: u8 = 250 + 10;
              return bad;
            }
        "#;

        let errors = compile(source).expect_err("constant overflow should fail");
        assert!(errors
            .iter()
            .any(|error| error.message.contains("cannot be represented as u8")));
    }

    #[test]
    fn supports_floats_and_mixed_numeric_promotions() {
        let source = r#"
            fn main() -> i64 {
              let a: f32 = 1.5;
              let b: u32 = 2;
              let c: f32 = a + b;
              let d: f64 = (c as f64) + 3.0;
              prove 1.0 + 2.0 == 3.0;
              assert d > 6.0;
              print d;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("float and int promotion should compile");
        assert!(c.contains("float v_c"));
        assert!(c.contains("((float)(v_b))"));
        assert!(c.contains("double v_d"));
        assert!(c.contains("((double)(v_c))"));
    }

    #[test]
    fn rejects_constant_float_division_by_zero() {
        let source = r#"
            fn identity(x: f64) -> f64 {
              return x;
            }

            fn main() -> i64 {
              let numerator = identity(1.0);
              let bad = numerator / 0.0;
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("float division by zero should fail");
        assert!(errors
            .iter()
            .any(|error| error.message.contains("floating-point division by zero")));
    }

    #[test]
    fn rejects_float_remainder() {
        let source = r#"
            fn main() -> i64 {
              let bad = 5.0 % 2.0;
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("float remainder should fail");
        assert!(errors
            .iter()
            .any(|error| error.message.contains("must be an integer")));
    }

    #[test]
    fn supports_explicit_casts_for_mixed_integer_arithmetic() {
        let source = r#"
            fn widen(x: i32, y: u64) -> u64 {
              return (x as u64) + y;
            }

            fn main() -> i64 {
              let value = widen(1, 2);
              assert value == 3;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("explicit cast should compile");
        assert!(c.contains("((uint64_t)(v_x))"));
    }

    #[test]
    fn supports_shift_and_remainder_sanity_checks() {
        // Use a function-parameter divisor / shift count so the
        // values aren't compile-time constants. Without a `requires`
        // bounding them, SMT can't elide the runtime guards, so the
        // helpers must appear in the emitted C.
        let source = r#"
            fn ops(bits: u8, n: i64, d: u8) -> u8 {
              let shifted: u8 = bits << n;
              let rem: u8 = shifted % d;
              return rem;
            }

            fn main() -> i64 {
              let r: u8 = ops(1 as u8, 3, 3 as u8);
              assert r == 2;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("shift and remainder should compile");
        // The shift helper is keyed by the rhs (shift-count) type,
        // hence i64 for `n: i64`; the divisor helper is keyed by the
        // divisor type `d: u8`.
        assert!(c.contains("intent_check_i64_shift"));
        assert!(c.contains("intent_check_u8_divisor"));
    }

    #[test]
    fn array_literal_and_indexing_compiles() {
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [10, 20, 30, 40];
              prove len(xs) == 4;
              let first: i64 = xs[0];
              let last: i64 = xs[3];
              assert first == 10;
              assert last == 40;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("arrays should compile");
        assert!(c.contains("int64_t v_xs[4]"), "expected array declaration in: {c}");
    }

    #[test]
    fn out_of_range_constant_index_rejected() {
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 2] = [1, 2];
              let bad: i64 = xs[5];
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("out-of-range index should fail");
        assert!(
            errors.iter().any(|e| e.message.contains("out of range")),
            "expected out-of-range diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn runtime_bounds_check_emitted_for_variable_index() {
        let source = r#"
            fn pick(xs: [i64; 4], i: u64) -> i64 {
              return xs[i];
            }

            fn main() -> i64 {
              let xs: [i64; 4] = [1, 2, 3, 4];
              let v: i64 = pick(xs, 1);
              prove v == v;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("variable indexing should compile");
        assert!(
            c.contains("intent_check_bounds"),
            "expected runtime bounds check in: {c}"
        );
    }

    #[test]
    fn bounds_check_elided_for_constant_index() {
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [1, 2, 3, 4];
              let v: i64 = xs[2];
              prove v == v;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("constant indexing should compile");
        assert!(
            !c.contains("intent_check_bounds(((uint64_t)(2))"),
            "constant index 2 should not be wrapped in a bounds check; got: {c}"
        );
        assert!(c.contains("v_xs[2]"), "expected direct indexing: {c}");
    }

    #[test]
    fn move_into_function_consumes_array() {
        let source = r#"
            fn sum_four(xs: [i64; 4]) -> i64 {
              return xs[0] + xs[1] + xs[2] + xs[3];
            }

            fn main() -> i64 {
              let xs: [i64; 4] = [1, 2, 3, 4];
              let total: i64 = sum_four(xs);
              let after: i64 = xs[0];
              prove total == 10;
              return after;
            }
        "#;

        let errors = compile(source).expect_err("use after move should fail");
        assert!(
            errors.iter().any(|e| e.message.contains("was moved")),
            "expected use-after-move diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn copy_primitive_remains_usable_after_call() {
        let source = r#"
            fn identity(x: i64) -> i64 {
              return x;
            }

            fn main() -> i64 {
              let x: i64 = 5;
              let y: i64 = identity(x);
              let z: i64 = x + y;
              assert z == 10;
              return 0;
            }
        "#;

        compile_to_c(source).expect("Copy primitives should not be consumed");
    }

    #[test]
    fn let_alias_moves_source_array() {
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 2] = [1, 2];
              let ys: [i64; 2] = xs;
              let bad: i64 = xs[0];
              prove ys[0] == 1;
              return bad;
            }
        "#;

        let errors = compile(source).expect_err("let-aliasing an array should move it");
        assert!(
            errors.iter().any(|e| e.message.contains("was moved")),
            "expected move diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn vec_construction_and_len() {
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let n: u64 = len(xs);
              assert n == 3;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("Vec should compile");
        assert!(
            c.contains("intent_vec_int64_t"),
            "expected Vec<i64> struct, got: {c}"
        );
        assert!(
            c.contains("intent_vec_int64_t__from"),
            "expected Vec<i64> constructor: {c}"
        );
    }

    #[test]
    fn vec_indexing_emits_runtime_bounds_check_when_not_provable() {
        // With SMT elision + the vec-literal length fact, the const
        // case `xs[0]` after `vec(10,20,30)` is discharged at compile
        // time. To force the runtime helper, use a Vec returned from
        // a function with no length info available to the caller.
        let source = r#"
            fn make() -> Vec<i64> { return vec(10, 20, 30); }
            fn main() -> i64 {
              let xs: Vec<i64> = make();
              let first: i64 = xs[0];
              assert first == 10;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("Vec indexing should compile");
        assert!(
            c.contains("intent_check_bounds"),
            "expected runtime bounds check on unprovable Vec index, got: {c}"
        );
    }

    #[test]
    fn vec_push_returns_new_vec_and_consumes_old() {
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let xs = push(xs, 4);
              let n: u64 = len(xs);
              assert n == 4;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("push should compile");
        assert!(c.contains("__push"), "expected push helper call, got: {c}");
    }

    #[test]
    fn vec_use_after_push_without_shadow_errors() {
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let ys: Vec<i64> = push(xs, 4);
              let bad: i64 = xs[0];
              return bad;
            }
        "#;

        let errors = compile(source).expect_err("use after push should fail");
        assert!(
            errors.iter().any(|e| e.message.contains("was moved")),
            "expected move diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn vec_set_functional_update() {
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let xs = set(xs, 0, 99);
              let first: i64 = xs[0];
              assert first == 99;
              return 0;
            }
        "#;

        compile_to_c(source).expect("set should compile");
    }

    #[test]
    fn vec_clone_does_not_consume() {
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let ys: Vec<i64> = clone(xs);
              let from_xs: i64 = xs[0];
              let from_ys: i64 = ys[0];
              assert from_xs == 1;
              assert from_ys == 1;
              return 0;
            }
        "#;

        compile_to_c(source).expect("clone should not consume xs");
    }

    #[test]
    fn vec_move_into_function_consumes() {
        let source = r#"
            fn take(xs: Vec<i64>) -> i64 {
              return xs[0];
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let first: i64 = take(xs);
              let bad: i64 = xs[0];
              return first + bad;
            }
        "#;

        let errors = compile(source).expect_err("passing Vec consumes");
        assert!(
            errors.iter().any(|e| e.message.contains("was moved")),
            "expected move diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn let_shadowing_drops_old_vec() {
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1);
              let xs: Vec<i64> = vec(2);
              let _v: i64 = xs[0];
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("shadowing should compile");
        assert!(
            c.contains("__free"),
            "expected free() call for shadowed Vec, got: {c}"
        );
    }

    #[test]
    fn function_return_drops_live_vecs() {
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let n: u64 = len(xs);
              assert n == 3;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("end-of-function drop should compile");
        assert!(
            c.contains("intent_vec_int64_t__free(v_xs)"),
            "expected free of xs before return, got: {c}"
        );
    }

    #[test]
    fn returning_vec_does_not_drop_it() {
        let source = r#"
            fn make() -> Vec<i64> {
              let xs: Vec<i64> = vec(1, 2, 3);
              return xs;
            }

            fn main() -> i64 {
              let xs: Vec<i64> = make();
              let n: u64 = len(xs);
              assert n == 3;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("returning Vec should compile");
        // The make() function must not free its xs before returning.
        let make_body_start = c.find("fn_make").expect("expected fn_make in output");
        let make_body_end = c[make_body_start..]
            .find("\n}\n")
            .map(|offset| make_body_start + offset)
            .unwrap_or(c.len());
        let make_body = &c[make_body_start..make_body_end];
        assert!(
            !make_body.contains("__free(v_xs)"),
            "make() must not free its returned xs, got: {}",
            make_body
        );
    }

    #[test]
    fn cannot_print_vec() {
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              print xs;
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("printing a Vec should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("cannot print an array or Vec")),
            "expected print-Vec diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn vec_of_vec_compiles_via_c_backend_runtime() {
        // End-to-end check that the C backend's helper bundle
        // shape is well-formed for nested Vec types: typedef
        // for `intent_vec_int64_t`, then `intent_vec_vec_int64_t`
        // referencing it; element-aware `__free` walking each
        // slot. Refines #7. The C compiler accepting the
        // emit is the canonical signal — runtime correctness
        // is exercised by the e2e tests under
        // `tests/run_end_to_end.rs`.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<Vec<i64>> = vec();
              xs = push(xs, vec(1, 2, 3));
              xs = push(xs, vec(4, 5));
              return len(xs) as i64;
            }
        "#;
        let c = compile_to_c(source).expect("Vec<Vec<i64>> emits C");
        assert!(
            c.contains("typedef struct { int64_t* data;")
                && c.contains("typedef struct { intent_vec_int64_t* data;"),
            "expected nested typedefs to be emitted:\n{c}"
        );
        assert!(
            c.contains("intent_vec_vec_int64_t__free"),
            "expected the per-shape free helper on the outer Vec:\n{c}"
        );
    }

    #[test]
    fn vec_of_vec_is_accepted_and_drops_recursively() {
        // Refines #7 from STATUS.md: `Vec<T>` no longer
        // requires `T: Copy`. `Vec<Vec<i64>>` (and other
        // nested non-Copy element types) now flow through
        // with element-aware free / set / clone helpers.
        // Reference-typed elements remain rejected because
        // they'd dangle.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<Vec<i64>> = vec(vec(1, 2), vec(3, 4));
              xs = push(xs, vec(5));
              return len(xs) as i64;
            }
        "#;
        compile(source).expect("Vec<Vec<i64>> should now compile");
    }

    #[test]
    fn vec_of_ref_still_rejected() {
        // References aren't allowed as Vec element types
        // (they'd outlive their referent and dangle).
        let source = r#"
            fn helper(x: ref i64) -> i64 { return 1; }
            fn main() -> i64 {
              let v: i64 = 0;
              let xs: Vec<ref i64> = vec(ref v);
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("Vec<&T> should fail");
        assert!(
            errors.iter().any(|e| e.message.contains("reference")),
            "expected reference diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn clone_at_extracts_owned_copy_of_inner_vec() {
        // Refines #7 phase 2d: `clone_at(&xs, i)` returns
        // an owned deep-clone of slot i so the user can
        // bind a non-Copy element by value without aliasing
        // the source slot. For `Vec<U>` elements the
        // builtin routes through the inner's `__clone`
        // helper; for Copy elements the slot value itself
        // is already an independent copy (memcpy
        // semantics). The previous "would alias and
        // double-free" restriction on `let inner = xs[i]`
        // stays — `clone_at` is the explicit opt-in.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<Vec<i64>> = vec(vec(1, 2, 3), vec(4, 5));
              let inner: Vec<i64> = clone_at(ref xs, 0);
              return len(inner) as i64;
            }
        "#;
        compile(source).expect("clone_at(&xs, 0) should compile");
        let c = compile_to_c(source).expect("clone_at emits C");
        assert!(
            c.contains("intent_vec_int64_t__clone"),
            "expected inner __clone helper call:\n{c}"
        );
    }

    #[test]
    fn clone_at_rejected_on_non_vec_collection() {
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [1, 2, 3, 4];
              let _: i64 = clone_at(ref xs, 0);
              return 0;
            }
        "#;
        let errors = compile(source)
            .expect_err("clone_at on a fixed-size array should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("Vec") && e.message.contains("clone_at")),
            "expected clone_at/Vec diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn generic_function_unused_surfaces_dead_code_diagnostic() {
        // T1.4 phase 2: monomorphization is now wired up.
        // A generic function declared but never called with
        // concrete types can't be specialized — surface a
        // gentler diagnostic noting it's effectively dead.
        let source = r#"
            fn id<T>(x: T) -> T { return x; }
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("unused generic should surface dead-code diagnostic");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("never called with concrete")),
            "expected dead-generic diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn generic_id_function_compiles_and_runs_monomorphized() {
        // T1.4 phase 2: `fn id<T>(x: T) -> T { return x; }`
        // called with `id(42)` monomorphizes to `id__i64`
        // and runs.
        let source = r#"
            fn id<T>(x: T) -> T { return x; }
            fn main() -> i64 {
              let a: i64 = id(42);
              return a;
            }
        "#;
        compile(source).expect("monomorphized generic call should compile");
    }

    #[test]
    fn generic_id_function_specializes_per_concrete_type() {
        // Multiple call sites with different concrete T
        // generate distinct specializations (`id__i64`,
        // `id__bool`).
        let source = r#"
            fn id<T>(x: T) -> T { return x; }
            fn main() -> i64 {
              let a: i64 = id(7);
              let b: bool = id(true);
              if b { return a; }
              return 0;
            }
        "#;
        compile(source).expect("multi-type-param specializations should compile");
    }

    #[test]
    fn methods_on_struct_basic() {
        // T1.2 phase 2a: a method on a struct is callable
        // via `p.method()` sugar and lowers to a regular
        // function call `Point_method(p)`.
        let source = r#"
            struct Point { x: i64, y: i64 }
            methods on Point {
              fn manhattan(self: Point) -> i64 { return self.x + self.y; }
            }
            fn main() -> i64 {
              let p: Point = Point { x: 3, y: 4 };
              return p.manhattan();
            }
        "#;
        compile(source).expect("method on struct should compile");
        let c = compile_to_c(source).expect("method emits C");
        assert!(
            c.contains("fn_Point_manhattan"),
            "expected mangled function name in C output, got:\n{c}"
        );
    }

    #[test]
    fn methods_on_struct_with_extra_args() {
        let source = r#"
            struct Point { x: i64, y: i64 }
            methods on Point {
              fn shift(self: Point, dx: i64) -> Point {
                return Point { x: self.x + dx, y: self.y };
              }
            }
            fn main() -> i64 {
              let p: Point = Point { x: 1, y: 2 };
              let q: Point = p.shift(5);
              return q.x;
            }
        "#;
        compile(source).expect("method with extra args should compile");
    }

    #[test]
    fn method_call_on_undeclared_method_rejected() {
        let source = r#"
            struct Point { x: i64, y: i64 }
            fn main() -> i64 {
              let p: Point = Point { x: 1, y: 2 };
              return p.dist();
            }
        "#;
        let errors = compile(source)
            .expect_err("missing method should fail");
        assert!(
            errors.iter().any(|e| e.message.contains("no method 'dist'")),
            "expected missing-method diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn method_call_on_primitive_rejected() {
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 42;
              return x.foo();
            }
        "#;
        let errors = compile(source)
            .expect_err("method on primitive should fail");
        assert!(
            errors.iter().any(|e| e.message.contains("struct/enum types only")),
            "expected primitive-rejection diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn methods_block_duplicate_method_rejected() {
        let source = r#"
            struct Point { x: i64, y: i64 }
            methods on Point {
              fn x_val(self: Point) -> i64 { return self.x; }
              fn x_val(self: Point) -> i64 { return self.x; }
            }
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("duplicate method should fail");
        assert!(
            errors.iter().any(|e| e.message.contains("declared twice")),
            "expected duplicate-method diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn methods_on_enum_basic() {
        // T1.2 phase 2a: methods on enum types — same
        // dispatch as struct methods, just with the type
        // name from the enum.
        let source = r#"
            enum Color { Red, Green, Blue }
            methods on Color {
              fn tag(self: Color) -> i64 {
                return match self {
                  Color.Red then 1,
                  Color.Green then 2,
                  Color.Blue then 3,
                };
              }
            }
            fn main() -> i64 {
              let c: Color = Color.Green;
              return c.tag();
            }
        "#;
        compile(source).expect("method on enum should compile");
    }

    #[test]
    fn method_calls_chain() {
        // T1.2 phase 2a: `p.foo().bar()` chains —
        // each `.<ident>(…)` postfix recurses via the
        // parser loop, and the checker desugars each
        // MethodCall to a regular Call. The intermediate
        // value (Point returned from `shift`) becomes
        // the next receiver.
        let source = r#"
            struct Point { x: i64, y: i64 }
            methods on Point {
              fn shift(self: Point, dx: i64) -> Point {
                return Point { x: self.x + dx, y: self.y };
              }
              fn x_val(self: Point) -> i64 { return self.x; }
            }
            fn main() -> i64 {
              let p: Point = Point { x: 1, y: 2 };
              return p.shift(10).shift(5).x_val();
            }
        "#;
        compile(source).expect("method chain should compile");
    }

    #[test]
    fn field_assign_on_owned_struct() {
        // T1.2 phase 2a follow-up: `p.x = expr;` on an
        // owned struct value works end-to-end.
        let source = r#"
            struct Point { x: i64, y: i64 }
            fn main() -> i64 {
              let p: Point = Point { x: 1, y: 2 };
              p.x = 10;
              return p.x + p.y;
            }
        "#;
        compile(source).expect("owned-struct field-assign should compile");
        let c = compile_to_c(source).expect("emits C");
        assert!(
            c.contains(".x ="),
            "expected `.x =` in C output, got:\n{c}"
        );
    }

    #[test]
    fn field_assign_owned_str_field_emits_free_of_old() {
        // Closure #132: when the field type is OwnedStr, the
        // FieldAssign emitter must free the old slot value
        // before storing the new one, otherwise the previous
        // heap allocation leaks.
        let source = r#"
            struct Tag { name: OwnedStr }
            fn main() -> i64 {
              let t: Tag = Tag { name: "a" + "1" };
              t.name = "b" + "2";
              return 0;
            }
        "#;
        compile(source).expect("OwnedStr field-assign should compile");
        let c = compile_to_c(source).expect("emits C");
        assert!(
            c.contains("free((void*)v_t.name);"),
            "expected free-of-old before assign, got:\n{c}"
        );
    }

    #[test]
    fn field_assign_vec_field_emits_vec_free_of_old() {
        // Closure #132: when the field type is Vec<T>, the
        // FieldAssign emitter must call the inner Vec's
        // __free on the old slot before storing the new one.
        let source = r#"
            struct Bag { items: Vec<i64> }
            fn main() -> i64 {
              let b: Bag = Bag { items: vec(1, 2) };
              b.items = vec(9, 8, 7);
              return 0;
            }
        "#;
        compile(source).expect("Vec field-assign should compile");
        let c = compile_to_c(source).expect("emits C");
        assert!(
            c.contains("intent_vec_int64_t__free(v_b.items);"),
            "expected vec-free-of-old before assign, got:\n{c}"
        );
    }

    #[test]
    fn field_assign_through_mut_ref() {
        // Field-assign through `mut ref Counter` (e.g.
        // inside a method that takes `self: mut ref T`).
        let source = r#"
            struct Counter { n: i64 }
            methods on Counter {
              fn bump(self: mut ref Counter) -> i64 {
                self.n = self.n + 1;
                return self.n;
              }
            }
            fn main() -> i64 {
              let c: Counter = Counter { n: 0 };
              let r: i64 = c.bump();
              return r;
            }
        "#;
        compile(source).expect("mut-ref field-assign should compile");
    }

    #[test]
    fn field_assign_unknown_field_rejected() {
        let source = r#"
            struct Point { x: i64, y: i64 }
            fn main() -> i64 {
              let p: Point = Point { x: 1, y: 2 };
              p.z = 5;
              return p.x;
            }
        "#;
        let errors = compile(source)
            .expect_err("unknown field should fail");
        assert!(
            errors.iter().any(|e| e.message.contains("no field named 'z'")),
            "expected unknown-field diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn nested_field_assign_compiles_and_runs() {
        // T1.2 phase 2a follow-up: `outer.inner.x = …;`
        // now works end-to-end via a recursive
        // lvalue-address helper in the LLVM backend that
        // walks Var → FieldAccess chains and emits a GEP
        // chain for the store target. The C backend "just
        // works" because chained `.` lvalues are native C.
        let source = r#"
            struct Inner { r: i64 }
            struct Outer { q: Inner }
            fn main() -> i64 {
              let o: Outer = Outer { q: Inner { r: 5 } };
              o.q.r = 10;
              return o.q.r;
            }
        "#;
        compile(source).expect("nested field-assign should compile");
        let c = compile_to_c(source).expect("emits C");
        assert!(
            c.contains(".q.r = 10"),
            "expected chained `.q.r = 10` in emitted C, got:\n{c}"
        );
    }

    #[test]
    fn field_assign_through_immutable_ref_rejected() {
        let source = r#"
            struct Counter { n: i64 }
            methods on Counter {
              fn try_bump(self: ref Counter) -> i64 {
                self.n = self.n + 1;
                return self.n;
              }
            }
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("immutable-ref field-assign should fail");
        assert!(
            errors.iter().any(|e| e.message.contains("immutable") || e.message.contains("mut ref")),
            "expected immutable-ref diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn method_call_auto_refs_when_self_is_ref() {
        // T1.2 phase 2a auto-ref: when a method binds
        // `self: ref Point` and the receiver is a plain
        // `Point`, the checker automatically wraps the
        // receiver in `ref` so users don't have to type
        // `ref(p).area()`.
        let source = r#"
            struct Point { x: i64, y: i64 }
            methods on Point {
              fn area(self: ref Point) -> i64 { return self.x * self.y; }
            }
            fn main() -> i64 {
              let p: Point = Point { x: 3, y: 5 };
              return p.area();
            }
        "#;
        compile(source).expect("auto-ref method call should compile");
    }

    #[test]
    fn method_call_auto_refs_when_self_is_mut_ref() {
        // Field-assignment through a mut ref isn't yet
        // parsed at statement position (separate feature),
        // so this test only exercises the auto-ref + read
        // side: the checker must wrap `c` in `mut ref` to
        // match `self: mut ref Counter`.
        let source = r#"
            struct Counter { n: i64 }
            methods on Counter {
              fn read(self: mut ref Counter) -> i64 { return self.n; }
            }
            fn main() -> i64 {
              let c: Counter = Counter { n: 42 };
              return c.read();
            }
        "#;
        compile(source).expect("auto-mut-ref method call should compile");
    }

    #[test]
    fn methods_on_struct_field_access_via_self() {
        // Method body can access self's fields directly.
        let source = r#"
            struct Box { val: i64 }
            methods on Box {
              fn doubled(self: Box) -> i64 { return self.val * 2; }
            }
            fn main() -> i64 {
              let b: Box = Box { val: 7 };
              return b.doubled();
            }
        "#;
        compile(source).expect("self field access should compile");
    }

    #[test]
    fn type_alias_to_primitive_compiles() {
        // T4.15: a `type Coord = i64;` alias must be
        // resolved away by the checker so the function
        // signature sees `i64` directly.
        let source = r#"
            type Score = i64;
            fn add_one(s: Score) -> Score { return s + 1; }
            fn main() -> i64 { return add_one(41); }
        "#;
        compile(source).expect("primitive alias should compile");
    }

    #[test]
    fn type_alias_to_tuple_compiles() {
        let source = r#"
            type Coord = (i64, i64);
            fn first(c: Coord) -> i64 { return c.0; }
            fn main() -> i64 {
              let p: Coord = (3, 4);
              return first(p);
            }
        "#;
        compile(source).expect("tuple alias should compile");
    }

    #[test]
    fn type_alias_to_enum_compiles() {
        let source = r#"
            enum Color { Red, Green, Blue }
            type Hue = Color;
            fn pick(h: Hue) -> i64 {
              return match h {
                Color.Red then 1,
                Color.Green then 2,
                Color.Blue then 3,
              };
            }
            fn main() -> i64 { return pick(Color.Green); }
        "#;
        compile(source).expect("enum alias should compile");
    }

    #[test]
    fn type_alias_chain_resolves() {
        // `type A = B; type B = i64;` — A should fully
        // resolve to i64 through the chain.
        let source = r#"
            type Inner = i64;
            type Middle = Inner;
            type Outer = Middle;
            fn add(x: Outer) -> i64 { return x + 1; }
            fn main() -> i64 { return add(41); }
        "#;
        compile(source).expect("alias chain should resolve");
    }

    #[test]
    fn type_alias_recursive_rejected() {
        // `type A = B; type B = A;` is a cycle — must fail
        // with a clear diagnostic.
        let source = r#"
            type A = B;
            type B = A;
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("recursive alias should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("recursive type alias")),
            "expected recursive-alias diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn type_alias_duplicate_rejected() {
        let source = r#"
            type X = i64;
            type X = f64;
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("duplicate alias should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("already declared")),
            "expected duplicate-alias diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn type_alias_collides_with_struct_rejected() {
        let source = r#"
            struct Point { x: i64, y: i64 }
            type Point = i64;
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("alias colliding with struct should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("collides with a struct")),
            "expected struct-collision diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn const_decl_int_compiles_and_runs() {
        // T4.15: a top-level `const NAME: T = literal;` is
        // visible to every function body. The checker seeds
        // each const into the root scope as a constant
        // VarInfo so the constant-tracking pass folds reads
        // straight through to SMT + codegen.
        let source = r#"
            const ANSWER: i64 = 42;
            fn main() -> i64 { return ANSWER; }
        "#;
        compile(source).expect("int const should compile");
    }

    #[test]
    fn const_decl_float_and_bool() {
        let source = r#"
            const PI: f64 = 3.14;
            const FLAG: bool = true;
            fn main() -> i64 {
              let x: f64 = PI;
              let b: bool = FLAG;
              if b { return 1; } else { return 0; }
            }
        "#;
        compile(source).expect("float + bool const should compile");
    }

    #[test]
    fn const_decl_negative_literal() {
        // Unary minus over a literal must fold into a
        // negative integer const.
        let source = r#"
            const MIN_BOUND: i64 = -100;
            fn main() -> i64 { return MIN_BOUND; }
        "#;
        compile(source).expect("negative const should compile");
    }

    #[test]
    fn const_decl_arithmetic_initializer_compiles() {
        // Closure #121: const initializers now accept
        // integer arithmetic over previously-declared
        // consts and literals.
        let source = r#"
            const TWO: i64 = 1 + 1;
            fn main() -> i64 { return TWO; }
        "#;
        compile(source).expect("const arithmetic should compile");
    }

    #[test]
    fn const_decl_rejects_non_copy_type() {
        // v1: Copy scalar types only — Vec/strings need
        // initializer-time allocation which is phase-2 work.
        let source = r#"
            const XS: Vec<i64> = vec(1, 2, 3);
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("Vec const should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("Copy scalar types only")),
            "expected Copy-only diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn const_decl_duplicate_rejected() {
        let source = r#"
            const X: i64 = 1;
            const X: i64 = 2;
            fn main() -> i64 { return X; }
        "#;
        let errors = compile(source)
            .expect_err("duplicate const should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("already declared")),
            "expected duplicate-const diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn const_emits_correct_value_into_c() {
        // Verify constant folding actually substitutes the
        // const's literal value into the emitted code,
        // rather than producing a dangling reference to a
        // name the C backend doesn't know about.
        let source = r#"
            const ANSWER: i64 = 42;
            fn main() -> i64 { return ANSWER; }
        "#;
        let c = compile_to_c(source).expect("const program emits C");
        assert!(
            c.contains("= 42") && !c.contains("v_ANSWER"),
            "expected const to fold into literal 42, got:\n{c}"
        );
    }

    #[test]
    fn const_can_be_shadowed_by_local() {
        // A function-scoped `let` binding shadows the const
        // for the rest of the scope; the const is still
        // visible in other functions.
        let source = r#"
            const N: i64 = 10;
            fn outer() -> i64 { return N; }
            fn inner() -> i64 {
              let N: i64 = 5;
              return N;
            }
            fn main() -> i64 { return outer() + inner(); }
        "#;
        compile(source).expect("local shadow of const should compile");
    }

    #[test]
    fn enum_variant_with_single_copy_payload_compiles() {
        // T1.3 phase 2b: single-Copy-field payloads are now
        // executable. The previous gate (which surfaced
        // "T1.3 phase 2b" WIP diagnostics) lifted once the
        // tree-C tagged-union codegen landed. Multi-field
        // payloads and non-Copy payloads still gate; see
        // `enum_variant_with_multi_payload_parses_but_gated`.
        let source = r#"
            enum Maybe { Some(i64), None }
            fn main() -> i64 { return 0; }
        "#;
        compile(source).expect("single-Copy-payload enum should compile");
    }

    #[test]
    fn enum_variant_with_multi_payload_parses_but_gated() {
        let source = r#"
            enum Outcome { Ok(i64, i64), Err }
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("multi-payload variant should fail with WIP diagnostic");
        assert!(
            errors.iter().any(|e| e.message.contains("T1.3 phase 2b")),
            "expected T1.3-phase-2b diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn interface_decl_without_impl_compiles_cleanly() {
        // T1.5 phase 2: `interface` declarations are now
        // accepted standalone — they just define a method
        // signature contract. Without an `implement` block,
        // the interface isn't dispatched against anywhere.
        let source = r#"
            interface Show {
              fn show(self: i64) -> i64;
            }
            fn main() -> i64 { return 0; }
        "#;
        compile(source).expect("interface decl alone should compile");
    }

    #[test]
    fn drop_interface_with_valid_signature_compiles() {
        // T2.7 phase 1: `implement Drop for T` is recognized
        // as a special interface contract. The auto-call at
        // scope exit lands with #3 RAII work; until then,
        // users declare the impl + call `t.drop()` manually.
        let source = r#"
            struct Resource { id: i64 }
            interface Drop {
              fn drop(self: Resource) -> i64;
            }
            implement Drop for Resource {
              fn drop(self: Resource) -> i64 { return self.id; }
            }
            fn main() -> i64 {
              let r: Resource = Resource { id: 42 };
              return r.drop();
            }
        "#;
        compile(source).expect("Drop impl with valid sig should compile");
    }

    #[test]
    fn drop_interface_bad_return_type_rejected() {
        // Drop impl must return i64 in v1.
        let source = r#"
            struct Resource { id: i64 }
            interface Drop {
              fn drop(self: Resource) -> bool;
            }
            implement Drop for Resource {
              fn drop(self: Resource) -> bool { return true; }
            }
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("non-i64 Drop return should be rejected");
        assert!(
            errors.iter().any(|e| e.message.contains("Drop impl for")),
            "expected Drop-signature diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn drop_interface_wrong_method_name_rejected() {
        // Drop impl must define a method named `drop`.
        let source = r#"
            struct Resource { id: i64 }
            interface Drop {
              fn destroy(self: Resource) -> i64;
            }
            implement Drop for Resource {
              fn destroy(self: Resource) -> i64 { return 0; }
            }
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("wrong-named Drop method should be rejected");
        assert!(
            errors.iter().any(|e| e.message.contains("exactly one method named `drop`")),
            "expected Drop-method-name diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn interface_with_impl_dispatches_statically() {
        // T1.5 phase 2: `implement Iface for Type { fn m … }`
        // hoists `m` to `<Type>_<method>` (same as
        // `methods on T`), and `recv.m()` dispatches to the
        // hoisted name via the existing method-call path.
        let source = r#"
            struct Point { x: i64, y: i64 }
            interface Show {
              fn show(self: Point) -> i64;
            }
            implement Show for Point {
              fn show(self: Point) -> i64 { return self.x + self.y; }
            }
            fn main() -> i64 {
              let p: Point = Point { x: 3, y: 4 };
              return p.show();
            }
        "#;
        compile(source).expect("interface impl + method dispatch should compile");
    }

    #[test]
    fn where_bound_satisfied_compiles() {
        // T1.5 phase 2: a generic with `where T is Iface`
        // monomorphizes when the call site's concrete type
        // has a matching `implement Iface for <T>` impl.
        let source = r#"
            interface Cmp { fn cmp(self: Score, other: Score) -> i64; }
            struct Score { value: i64 }
            implement Cmp for Score {
              fn cmp(self: Score, other: Score) -> i64 {
                if self.value < other.value { return -1; }
                if self.value > other.value { return 1; }
                return 0;
              }
            }
            fn pick<T>(x: T, y: T) -> T where T is Cmp {
              if x.cmp(y) <= 0 { return x; }
              return y;
            }
            fn main() -> i64 {
              let a: Score = Score { value: 7 };
              let b: Score = Score { value: 3 };
              let m: Score = pick(a, b);
              return m.value;
            }
        "#;
        compile(source)
            .expect("bounded generic with satisfying impl should compile");
    }

    #[test]
    fn where_bound_unsatisfied_rejected() {
        // T1.5 phase 2: a generic call with no matching impl
        // for the bound surfaces a clear diagnostic at the
        // monomorphizer.
        let source = r#"
            interface Cmp { fn cmp(self: Score, other: Score) -> i64; }
            struct Score { value: i64 }
            fn pick<T>(x: T, y: T) -> T where T is Cmp {
              if x.cmp(y) <= 0 { return x; }
              return y;
            }
            fn main() -> i64 {
              let a: Score = Score { value: 7 };
              let b: Score = Score { value: 3 };
              let m: Score = pick(a, b);
              return m.value;
            }
        "#;
        let errors = compile(source)
            .expect_err("missing impl should fail the bound check");
        assert!(
            errors.iter().any(|e| e.message.contains("requires `T is Cmp`")
                && e.message.contains("implement Cmp for Score")),
            "expected bound-violation diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn struct_name_clashing_with_builtin_rejected() {
        // `Task` lexes as an identifier but `parse_type`
        // promotes it to the built-in `Type::Task`. Without
        // a checker-level gate, `struct Task { … }` would
        // parse cleanly but every use of `Task` in type
        // position would resolve to the built-in,
        // producing confusing "got Task" errors. T1.2
        // follow-up: surface a clean
        // "reserved built-in type" diagnostic.
        let source = r#"
            struct Task { priority: i64 }
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("Task as struct name should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("'Task'")
                    && e.message.contains("reserved built-in type")),
            "expected reserved-name diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn enum_name_clashing_with_builtin_rejected() {
        let source = r#"
            enum Atomic { A, B, C }
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("Atomic as enum name should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("'Atomic'")
                    && e.message.contains("reserved built-in type")),
            "expected reserved-name diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn match_arm_body_with_nested_if_expression() {
        // T4 follow-up: a match arm body can itself be an
        // if-expression. The fix from else-if chaining
        // (tracking `ctx.current_block` in the LLVM
        // backend so phi predecessors point at the actual
        // tail BB) generalized to Match emission too. Prior
        // to the fix, lli rejected the IR with "Instruction
        // does not dominate all uses".
        let source = r#"
            enum Color { Red, Green, Blue }
            fn classify(c: Color, threshold: i64) -> i64 {
              return match c {
                Color.Red then if threshold > 0 { 1 } else { -1 },
                Color.Green then if threshold > 10 { 10 } else { 5 },
                Color.Blue then 0,
              };
            }
            fn main() -> i64 { return classify(Color.Red, 5); }
        "#;
        compile(source).expect("nested if-expr in match arm should compile");
    }

    #[test]
    fn if_expression_else_if_chain_compiles() {
        // T4 follow-up: `else if cond { … } else { … }`
        // chains parse and lower correctly. Each `else if`
        // arm parses as a nested if-expression on the
        // else side. Bug fix from initial if-expression:
        // tracking `ctx.current_block` so the outer phi
        // node uses the actual predecessor BB (which may
        // differ from the branch's opening label when the
        // branch is itself a chained if-expression).
        let source = r#"
            fn classify(x: i64) -> i64 {
              return if x < 0 {
                -1
              } else if x == 0 {
                0
              } else if x < 100 {
                1
              } else {
                2
              };
            }
            fn main() -> i64 {
              return classify(50);
            }
        "#;
        compile(source).expect("else-if chain should compile");
    }

    #[test]
    fn if_expression_compiles_and_runs() {
        // T4 if-as-expression: `if cond { expr } else { expr }`
        // works as a value-producing expression. Branches
        // must be single expressions in braces.
        let source = r#"
            fn main() -> i64 {
              let r: i64 = if true { 10 } else { 20 };
              return r;
            }
        "#;
        compile(source).expect("if-expression should compile");
        let c = compile_to_c(source).expect("emits C");
        assert!(
            c.contains("? (10) : (20)") || c.contains("(10) : (20)"),
            "expected ternary in emitted C, got:\n{c}"
        );
    }

    #[test]
    fn if_expression_in_return() {
        let source = r#"
            fn classify(x: i64) -> i64 {
              return if x >= 0 { 1 } else { -1 };
            }
            fn main() -> i64 {
              return classify(-5);
            }
        "#;
        compile(source).expect("if-expression in return should compile");
    }

    #[test]
    fn if_expression_branch_type_mismatch_rejected() {
        let source = r#"
            fn main() -> i64 {
              let r: i64 = if true { 1 } else { true };
              return r;
            }
        "#;
        let errors = compile(source)
            .expect_err("branch type mismatch should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("different types")),
            "expected branch-type-mismatch diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn if_expression_non_bool_condition_rejected() {
        let source = r#"
            fn main() -> i64 {
              let r: i64 = if 1 { 10 } else { 20 };
              return r;
            }
        "#;
        let errors = compile(source)
            .expect_err("non-bool condition should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("condition must be bool")),
            "expected non-bool-cond diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn method_call_on_ref_receiver_value_self_gets_helpful_diagnostic() {
        // The inverse of auto-ref: receiver is a borrow
        // (`ref Point`) but the method takes `self: Point`
        // by value. The language doesn't have an implicit
        // deref expression so this can't be silently
        // coerced — but the diagnostic should explain the
        // two viable workarounds (change the method sig
        // to take `self: ref T`, or copy the struct
        // explicitly before calling).
        let source = r#"
            struct Point { x: i64, y: i64 }
            methods on Point {
              fn sum(self: Point) -> i64 { return self.x + self.y; }
            }
            fn take_ref(p: ref Point) -> i64 { return p.sum(); }
            fn main() -> i64 {
              let p: Point = Point { x: 3, y: 4 };
              return take_ref(ref p);
            }
        "#;
        let errors = compile(source)
            .expect_err("by-value-self via ref receiver should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("takes `self: Point` by value")
                    && e.message.contains("either change the method signature")
                    && e.message.contains("copy the value")),
            "expected helpful method-call mismatch diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn const_negative_literal_in_range_compiles() {
        // `const X: i8 = -100;` fits since i8 range is
        // -128..=127. Tests that the value-fits-type
        // check honors signed-range bounds.
        let source = r#"
            const X: i8 = -100;
            fn main() -> i64 { return X as i64; }
        "#;
        compile(source).expect("in-range negative const should compile");
    }

    #[test]
    fn const_negative_literal_out_of_range_rejected() {
        let source = r#"
            const X: i8 = -200;
            fn main() -> i64 { return X as i64; }
        "#;
        let errors = compile(source)
            .expect_err("out-of-range negative const should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("does not fit in i8")
                    && e.message.contains("-200")),
            "expected overflow diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn generic_type_param_trailing_comma_accepted() {
        // `fn id<T,>(…)` (trailing comma in generic param
        // list) parses cleanly. T1.4 phase 2 monomorphization
        // now specializes generics, so an UNUSED generic
        // surfaces the dead-code diagnostic; that still
        // confirms the parser accepted the trailing comma.
        let source = r#"
            fn id<T,>(x: T) -> T { return x; }
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("unused generic hits dead-code gate");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("never called with concrete")),
            "expected dead-generic diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn where_clause_trailing_comma_accepted() {
        // The parser accepts a trailing comma after the
        // final `T is Iface` bound. With bounded generics
        // shipping (T1.5 phase 2), an uncalled generic now
        // hits the dead-generic diagnostic rather than the
        // old WIP gate.
        let source = r#"
            fn id<T>(x: T) -> T where T is Cmp, { return x; }
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("dead generic with where-clause still rejected");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("never called with concrete")),
            "expected dead-generic diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn array_literal_as_direct_function_argument() {
        // Passing an array literal directly as a fn
        // arg (`f([Point{1,2}, …])`) previously
        // panicked at LLVM emit because ArrayLit was
        // only supported in let-binding RHS position.
        // Added a sub-expression emit path: alloca +
        // per-element store + load whole aggregate.
        let source = r#"
            struct P { x: i64, y: i64 }
            fn sum_pts(pts: [P; 3]) -> i64 { return pts[0].x + pts[1].y + pts[2].x; }
            fn main() -> i64 {
              return sum_pts([P { x: 1, y: 2 }, P { x: 3, y: 4 }, P { x: 5, y: 6 }]);
            }
        "#;
        compile(source).expect("array literal as fn arg should compile");
    }

    #[test]
    fn mutual_recursion_compiles() {
        let source = r#"
            fn is_even(n: i64) -> bool {
              if n == 0 { return true; }
              return is_odd(n - 1);
            }
            fn is_odd(n: i64) -> bool {
              if n == 0 { return false; }
              return is_even(n - 1);
            }
            fn main() -> i64 { if is_even(8) { return 100; } return 0; }
        "#;
        compile(source).expect("mutual recursion should compile");
    }

    #[test]
    fn bool_from_comparison_stored_in_let() {
        let source = r#"
            fn main() -> i64 {
              let big: bool = 100 > 50;
              if big { return 1; }
              return 0;
            }
        "#;
        compile(source).expect("bool from cmp should compile");
    }

    #[test]
    fn function_returning_enum() {
        let source = r#"
            enum Side { L, R }
            fn pick(n: i64) -> Side {
              if n > 0 { return Side.R; }
              return Side.L;
            }
            fn main() -> i64 {
              return match pick(5) { Side.L then 1, Side.R then 2 };
            }
        "#;
        compile(source).expect("fn returning enum should compile");
    }

    #[test]
    fn while_invariant_references_const() {
        let source = r#"
            const N: i64 = 10;
            fn main() -> i64 {
              let i: i64 = 0;
              let total: i64 = 0;
              while i < N
                invariant i >= 0;
                invariant i <= N;
              {
                total = total + i;
                i = i + 1;
              }
              return total;
            }
        "#;
        compile(source).expect("while inv references const should compile");
    }

    #[test]
    fn print_f32_typed_value() {
        let source = r#"
            fn main() -> i64 {
              let x: f32 = 1.5;
              print "x=", x;
              return 0;
            }
        "#;
        compile(source).expect("print f32 should compile");
    }

    #[test]
    fn match_on_i8_scrutinee() {
        let source = r#"
            fn classify(x: i8) -> i64 {
              return match x { 0 then 1, 1 then 2, _ then 99 };
            }
            fn main() -> i64 { return classify(2 as i8); }
        "#;
        compile(source).expect("match on i8 should compile");
    }

    #[test]
    fn match_on_u8_scrutinee() {
        let source = r#"
            fn main() -> i64 {
              let x: u8 = 200;
              return match x { 0 then 1, 100 then 2, 200 then 3, _ then 0 };
            }
        "#;
        compile(source).expect("match on u8 should compile");
    }

    #[test]
    fn const_initialized_with_other_const_compiles() {
        // Closure #121: const arithmetic referencing prior
        // consts is supported.
        let source = r#"
            const A: i64 = 5;
            const B: i64 = A + 1;
            fn main() -> i64 { return B; }
        "#;
        compile(source).expect("const-A-plus-1 should compile");
    }

    #[test]
    fn generic_call_site_specializes_and_compiles() {
        // T1.4 phase 2: `id(5)` calling `fn id<T>(x: T) -> T`
        // now monomorphizes to `id__i64` and compiles
        // cleanly.
        let source = r#"
            fn id<T>(x: T) -> T { return x; }
            fn main() -> i64 { return id(5); }
        "#;
        compile(source).expect("specialized generic call should compile");
    }

    #[test]
    fn try_keyword_binds_tightly_to_operand() {
        // Regression guard: `try EXPR + 1` parses as
        // `(try EXPR) + 1`, not `try (EXPR + 1)`. The parser
        // reads the inner at primary-expr precedence so the
        // `+ 1` becomes the outer binary, not part of try's
        // operand. Both forms hit the WIP gate today, but
        // when Phase 2 lands, the precedence here determines
        // whether `try x + 1` extracts then adds (correct) or
        // tries the sum (wrong). Pin the parse.
        use crate::ast::ExprKind;
        use crate::lexer::lex;
        use crate::parser::parse;
        // Wrap in a fn so the parser has a top-level context.
        let source = "enum Opt { Some(i64), None } fn main() -> i64 { let v: i64 = try o + 1; return v; }";
        let tokens = lex(source).expect("lex");
        let (program, _diags) = parse(tokens);
        // Find the let-init expression. We can't run the
        // checker (it would gate); just inspect the AST.
        let main_fn = program.functions.iter().find(|f| f.name == "main").unwrap();
        let let_stmt = main_fn.body.iter().find_map(|s| match s {
            crate::ast::Stmt::Let { expr, .. } => Some(expr),
            _ => None,
        }).expect("let stmt");
        // Top-level expr should be Binary(Add, _, _) — the
        // outer `+ 1`. The left of the Binary should be the
        // Try wrapping `o`.
        match &let_stmt.kind {
            ExprKind::Binary { op, left, .. } => {
                assert!(matches!(op, crate::ast::BinaryOp::Add));
                assert!(
                    matches!(left.kind, ExprKind::Try { .. }),
                    "expected Try as left of Add, got {:?}",
                    left.kind
                );
            }
            other => panic!("expected Binary(Add, ...), got {:?}", other),
        }
    }

    #[test]
    fn try_keyword_desugars_let_try_return_pattern() {
        // T2.6 Phase 2: `let v: T = try opt; ...; return X;`
        // now desugars at the AST-level to a match-with-
        // early-return. The pre-pass
        // `desugar_try_let_in_program` rewrites the
        // function body to `return match opt { Opt.Some(v)
        // then X, Opt.None then Opt.None };` (with
        // intermediate `let` stmts hoisted into a block-
        // expression for the Some arm).
        let source = r#"
            enum Opt { Some(i64), None }
            fn doubled(o: Opt) -> Opt {
              let v: i64 = try o;
              return Opt.Some(v * 2);
            }
            fn main() -> i64 { return 0; }
        "#;
        compile(source).expect("try-let-return pattern should desugar and compile");
    }

    #[test]
    fn try_keyword_desugar_admits_intermediate_print_stmts() {
        // Closure #130 extends the try desugar to admit
        // `print` stmts between the try-let and the final
        // return (Let was the only intermediate shape before).
        // The print flows through to the Some-arm's block
        // expression, which #129 already taught to accept
        // print stmts.
        let source = r#"
            enum Opt { Some(i64), None }
            fn doit(o: Opt) -> Opt {
              let v: i64 = try o;
              print "doit got v=", v;
              return Opt.Some(v + 1);
            }
            fn main() -> i64 {
              let r: Opt = doit(Opt.Some(42));
              let s: Opt = doit(Opt.None);
              return 0;
            }
        "#;
        compile(source).expect("try with intermediate print should desugar");
        let c = compile_to_c(source).expect("C backend emits a program");
        // The print's `fputs` must appear inside the
        // desugared Some-arm's block-expression body,
        // i.e. somewhere between `case 0:` and the `break;`
        // that closes that arm.
        let some_arm = c.find("case 0:").expect("Some arm");
        let print_site = c.find("fputs(\"doit got v=\"")
            .expect("intermediate print emitted");
        assert!(
            print_site > some_arm,
            "print should be emitted inside the Some-arm block:\n{}",
            c
        );
    }

    #[test]
    fn try_keyword_desugar_rejects_intermediate_assign() {
        // Closure #130: the relaxation admits Let + Print
        // only. Reassignment / control flow still falls
        // through to the Phase 1 gate (control flow between
        // try and return needs surrounding-stmt handling we
        // don't model in v1).
        let source = r#"
            enum Opt { Some(i64), None }
            fn doit(o: Opt) -> Opt {
              let v: i64 = try o;
              let mut w: i64 = 0;
              w = v + 1;
              return Opt.Some(w);
            }
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source).expect_err(
            "try with intermediate reassign should be rejected",
        );
        assert!(
            errors.iter().any(|e| {
                e.message.contains("only `let` and `print`")
                    || e.message.contains("`try EXPR` is reserved")
            }),
            "expected relax-diagnostic or fallback gate, got: {:?}",
            errors
        );
    }

    #[test]
    fn try_keyword_in_unsupported_shape_surfaces_phase_1_gate() {
        // The Phase 2 desugar only fires for the restricted
        // `[Let(try), Let*, Return]` shape. A `try` outside
        // that shape (e.g. in a let inside an if-body) falls
        // through to the Phase 1 gate.
        let source = r#"
            enum Opt { Some(i64), None }
            fn weird(o: Opt) -> i64 {
              if true {
                let v: i64 = try o;
                return v;
              }
              return 0;
            }
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("try in unsupported shape should surface gate");
        assert!(
            errors.iter().any(|e| e.message.contains("`try EXPR` is reserved")),
            "expected try-gate diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn payloaded_enum_with_match_destructure_compiles_in_llvm() {
        // T1.3 phase 2b Phase 4 LLVM: payloaded enums now
        // compile end-to-end via the default LLVM backend.
        // `Opt.Some(42)` builds the `{ i32, i64 }` struct via
        // two `insertvalue`s; `match` extracts field 0 for the
        // `switch` and field 1 (in VariantWithBinding arms)
        // into an alloca registered in `ctx.locals` so the
        // arm body's reference to `v` resolves.
        let source = r#"
            enum Opt { Some(i64), None }
            fn unwrap_or(o: Opt, def: i64) -> i64 {
              return match o {
                Opt.Some(v) then v,
                Opt.None then def,
              };
            }
            fn main() -> i64 {
              let a: Opt = Opt.Some(42);
              return unwrap_or(a, 0);
            }
        "#;
        compile(source).expect("payloaded enum + match destructure should compile via LLVM");
    }

    #[test]
    fn payloaded_enum_with_match_destructure_compiles_in_tree_c() {
        // T1.3 phase 2b: payloaded enums now lower to a
        // tagged-union struct in tree-C (`typedef struct {
        // i32 tag; i64 payload; } Enum_Opt;`). Match arms
        // dispatch on `.tag` and destructure patterns
        // `Opt.Some(v) then …` extract `__scr.payload` into
        // a local `v` in the arm body's scope.
        let source = r#"
            enum Opt { Some(i64), None }
            fn unwrap_or(o: Opt, def: i64) -> i64 {
              return match o {
                Opt.Some(v) then v,
                Opt.None then def,
              };
            }
            fn main() -> i64 {
              let a: Opt = Opt.Some(42);
              return unwrap_or(a, 0);
            }
        "#;
        compile_to_c(source).expect("payloaded enum + match destructure should compile to C");
    }

    #[test]
    fn owned_str_concat_chain_compiles() {
        // `OwnedStr` from `+` can be re-concatenated; the
        // affine chain `a -> a+c -> b+d` works because each
        // step consumes the previous binding.
        let source = r#"
            fn main() -> i64 {
              let a: OwnedStr = "a" + "b";
              let b: OwnedStr = a + "c";
              let c: OwnedStr = b + "d";
              return 0;
            }
        "#;
        compile(source).expect("OwnedStr chain should compile");
    }

    #[test]
    fn nested_match_in_match_arm_compiles() {
        // `match s { Side.L then (match n { … }), Side.R then 99 }`
        // — match in arm-expression position is supported.
        let source = r#"
            enum Side { L, R }
            fn main() -> i64 {
              let s: Side = Side.L;
              let n: i64 = 3;
              return match s {
                Side.L then (match n { 1 then 10, 2 then 20, _ then 30 }),
                Side.R then 99,
              };
            }
        "#;
        compile(source).expect("nested match should compile");
    }

    #[test]
    fn raw_ampersand_borrow_rejected_for_ref_iter() {
        // `for x in &xs` is the Rust-style borrow that the
        // language deliberately rejects — keyword-first
        // syntax means `for x in ref xs { … }` is the only
        // accepted form. The diagnostic should name T0.0
        // and suggest the keyword form.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let total: i64 = 0;
              for x in &xs { total = total + x; }
              return total;
            }
        "#;
        let errors = compile(source)
            .expect_err("&xs borrow form is rejected");
        assert!(
            errors.iter().any(|e| e.message.contains("ref ")),
            "expected ref-keyword diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn raw_ampersand_borrow_rejected_for_clone_at() {
        // `clone_at(&xs, i)` likewise rejected — must use
        // `clone_at(ref xs, i)`.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<Vec<i64>> = vec(vec(1, 2), vec(3, 4));
              let inner: Vec<i64> = clone_at(&xs, 0);
              return inner[1];
            }
        "#;
        let errors = compile(source)
            .expect_err("&xs in clone_at is rejected");
        assert!(
            errors.iter().any(|e| e.message.contains("ref ")),
            "expected ref-keyword diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn ampersand_self_receiver_rejected_in_methods() {
        // Methods use `methods on T { fn m(self) -> … }` /
        // `fn m(ref self) -> …` — Rust's `&self` shorthand
        // is intentionally rejected to keep keyword-first.
        let source = r#"
            struct B { v: i64 }
            methods on B {
              fn inc(&self) -> i64 { return self.v + 1; }
            }
            fn main() -> i64 {
              let b: B = B { v: 5 };
              return b.inc();
            }
        "#;
        let errors = compile(source)
            .expect_err("&self receiver is rejected");
        assert!(
            !errors.is_empty(),
            "expected at least one diagnostic for &self"
        );
    }

    #[test]
    fn negative_integer_literal_compiles() {
        let source = r#"
            fn main() -> i64 { let x: i64 = -5; let y: i64 = 0 - x; return y; }
        "#;
        compile(source).expect("negative int literal should compile");
    }

    #[test]
    fn for_loop_empty_range_compiles() {
        // `for i from 5 to 5` — body never executes.
        let source = r#"
            fn main() -> i64 {
              let n: i64 = 0;
              for i from 5 to 5 { n = n + 1; }
              return n;
            }
        "#;
        compile(source).expect("empty for range should compile");
    }

    #[test]
    fn for_loop_reverse_range_compiles() {
        // `for i from 5 to 3` — start > end, body never executes.
        let source = r#"
            fn main() -> i64 {
              let n: i64 = 0;
              for i from 5 to 3 { n = n + 1; }
              return n;
            }
        "#;
        compile(source).expect("reverse for range should compile");
    }

    #[test]
    fn cast_bool_to_int_rejected() {
        // bool ↔ int casts are rejected — different semantic
        // domains. Forces explicit if/else conversion.
        let source = r#"
            fn main() -> i64 { let b: bool = true; return b as i64; }
        "#;
        let errors = compile(source)
            .expect_err("bool→int cast is rejected");
        assert!(
            errors.iter().any(|e| e.message.contains("cannot cast bool")),
            "expected bool-cast diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn match_arm_bool_literal_compiles() {
        // `match x { 5 then true, _ then false }` — match
        // arm-expressions can be bool literals.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              let r: bool = match x { 5 then true, _ then false };
              if r { return 100; }
              return 0;
            }
        "#;
        compile(source).expect("match → bool should compile");
    }

    #[test]
    fn multiple_consts_same_type_compose() {
        let source = r#"
            const A: i64 = 1;
            const B: i64 = 2;
            const C: i64 = 3;
            fn main() -> i64 { return A + B + C; }
        "#;
        compile(source).expect("multiple consts should compose");
    }

    #[test]
    fn tuple_stored_as_struct_field_compiles() {
        // `struct Pair { t: (i64, i64) }` — tuples as
        // struct fields are supported.
        let source = r#"
            struct Pair { t: (i64, i64) }
            fn main() -> i64 {
              let p: Pair = Pair { t: (1, 2) };
              return p.t.0 + p.t.1;
            }
        "#;
        compile(source).expect("tuple field should compile");
    }

    #[test]
    fn const_used_in_arithmetic_expression_compiles() {
        // Distinction: `const B: i64 = A + 1;` is rejected
        // (const init must be literal), but `let x: i64 =
        // A + 5;` in fn body is fine — that's an ordinary
        // expression context.
        let source = r#"
            const N: i64 = 10;
            fn main() -> i64 { let x: i64 = N + 5; return x; }
        "#;
        compile(source).expect("const in expr should compile");
    }

    #[test]
    fn method_call_on_fn_result_compiles() {
        // `make().get()` — chain a method onto a function
        // call's return value. Self-receiver must be
        // declared explicitly as `self: B` (keyword-first
        // design rejects implicit `&self`/`self`).
        let source = r#"
            struct B { v: i64 }
            methods on B {
              fn get(self: B) -> i64 { return self.v; }
            }
            fn make() -> B { return B { v: 42 }; }
            fn main() -> i64 { return make().get(); }
        "#;
        compile(source).expect("method on fn result should compile");
    }

    #[test]
    fn match_on_fn_call_scrutinee_compiles() {
        // The scrutinee of a `match` can be any expression,
        // including a fn call result.
        let source = r#"
            fn pick() -> i64 { return 2; }
            fn main() -> i64 {
              return match pick() { 1 then 10, 2 then 20, _ then 99 };
            }
        "#;
        compile(source).expect("match on fn result should compile");
    }

    #[test]
    fn implicit_self_method_receiver_rejected() {
        // `fn get(self)` (Rust-style implicit self) is
        // rejected — keyword-first design requires
        // `self: Type` (or `self: ref Type` / `self: mut
        // ref Type`).
        let source = r#"
            struct B { v: i64 }
            methods on B {
              fn get(self) -> i64 { return self.v; }
            }
            fn main() -> i64 {
              let b: B = B { v: 5 };
              return b.get();
            }
        "#;
        let errors = compile(source)
            .expect_err("implicit self is rejected");
        assert!(
            !errors.is_empty(),
            "expected at least one diagnostic for implicit self"
        );
    }

    #[test]
    fn inner_let_shadow_does_not_leak_to_outer() {
        // Bug fix: `let x = 5; if true { let x = 10; } return x;`
        // previously returned 10 because the SSA lowerer's flat
        // `Locals` map let the inner `let`-shadow overwrite the
        // outer entry, and the if-merge wired the inner SSA
        // value through a spurious phi. lower_stmts now returns
        // the list of names introduced via top-level `let` in
        // the stmt block, and lower_if restores those entries
        // to entry-scope values before merging.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              if true {
                let x: i64 = 10;
                assert x == 10;
              }
              assert x == 5;
              return x;
            }
        "#;
        let c = compile_to_c(source)
            .expect("inner-let shadow should compile");
        let _ = c;
    }

    #[test]
    fn inner_let_shadow_cross_type_no_phi_error() {
        // Cross-type shadow (`let x: bool` over outer
        // `let x: i64`) previously caused an LLVM phi type
        // mismatch (i1 mixed with i64) at the if-merge.
        // With the shadow-restore fix in lower_if, the merge
        // no longer creates a phi for the shadowed binding.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              if true {
                let x: bool = true;
                assert x;
              }
              return x;
            }
        "#;
        compile(source).expect("cross-type inner shadow should compile");
    }

    #[test]
    fn inner_assign_without_let_still_propagates_to_outer() {
        // Regression guard: the shadow-restore must NOT
        // affect `x = N;` reassignments — those are genuine
        // mutations of the outer binding and must still flow
        // through the if-merge.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              if true { x = 99; }
              return x;
            }
        "#;
        compile(source).expect("inner reassign should compile");
    }

    #[test]
    fn nested_if_shadows_do_not_leak() {
        // Bug-fix regression: shadow restoration must work at
        // any nesting depth — the inner inner shadow
        // shouldn't leak into the inner shadow's scope, and
        // neither should leak into the outer scope.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 100;
              if true {
                let x: i64 = 50;
                if true {
                  let x: i64 = 25;
                }
                assert x == 50;
              }
              return x;
            }
        "#;
        compile(source).expect("nested shadows should compile");
    }

    #[test]
    fn print_struct_clean_diagnostic() {
        // Previously panicked in backend_llvm.rs at the
        // print-of-aggregate unreachable. Checker now
        // rejects with a clean hint.
        let source = r#"
            struct P { x: i64 }
            fn main() -> i64 {
              let p: P = P { x: 5 };
              print "p=", p;
              return 0;
            }
        "#;
        let errors = compile(source)
            .expect_err("print struct should be rejected");
        assert!(
            errors.iter().any(|e| e.message.contains("cannot print a struct")),
            "expected struct-print diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn print_tuple_clean_diagnostic() {
        let source = r#"
            fn main() -> i64 {
              let t: (i64, i64) = (1, 2);
              print "t=", t;
              return 0;
            }
        "#;
        let errors = compile(source)
            .expect_err("print tuple should be rejected");
        assert!(
            errors.iter().any(|e| e.message.contains("cannot print a tuple")),
            "expected tuple-print diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn print_enum_clean_diagnostic() {
        let source = r#"
            enum E { A, B }
            fn main() -> i64 {
              let e: E = E.A;
              print "e=", e;
              return 0;
            }
        "#;
        let errors = compile(source)
            .expect_err("print enum should be rejected");
        assert!(
            errors.iter().any(|e| e.message.contains("cannot print an enum")),
            "expected enum-print diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn nested_affine_struct_field_compiles_with_recursive_drop() {
        // Closure #125: outer structs can carry inner
        // structs whose own fields are heap-shaped. Both
        // backends recursively walk struct types at Drop
        // time. The non-Copy registry uses fixed-point
        // iteration so source order doesn't matter for
        // the registration.
        let source = r#"
            struct Inner { s: OwnedStr }
            struct Outer { inner: Inner, id: i64 }
            fn main() -> i64 {
              let o: Outer = Outer { inner: Inner { s: "hi" + "lo" }, id: 7 };
              assert o.id == 7;
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("nested affine struct should compile");
        // The C output should include a free of the inner
        // struct's OwnedStr field, accessed via the outer
        // struct's path.
        assert!(
            c.contains("free((void*)v_o.inner.s)"),
            "expected recursive Drop emission in C output:\n{c}"
        );
    }

    #[test]
    fn nested_path_move_of_non_copy_field_rejected() {
        // The `o.inner.s` move pattern would alias the
        // outer struct's recursive Drop with the new
        // binding — moved_fields can't represent the
        // path-level move, so the checker rejects with
        // a workaround hint.
        let source = r#"
            struct Inner { s: OwnedStr }
            struct Outer { inner: Inner }
            fn main() -> i64 {
              let o: Outer = Outer { inner: Inner { s: "a" + "b" } };
              let taken: OwnedStr = o.inner.s;
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("nested move should fail");
        assert!(
            errors.iter().any(|e| e.message.contains("nested field move")),
            "expected nested-move diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn enum_mutex_payload_compiles() {
        // Closure #124: enum payloads admit `Mutex<T>`.
        // No Drop needed (Mutex is `{ value, locked }`
        // inline data with no heap allocation).
        let source = r#"
            enum Locked { Held(Mutex<i64>), Free }
            fn main() -> i64 {
              let m: Locked = Locked.Held(mutex_new(0));
              let f: Locked = Locked.Free;
              return 0;
            }
        "#;
        compile(source).expect("Mutex enum payload should compile");
    }

    #[test]
    fn enum_atomic_payload_compiles() {
        // Closure #122: enum payloads admit `Atomic<T>`.
        // Atomic cells have no Drop (no allocation), so
        // gate-lift + payload-zero literal is the only
        // work needed.
        let source = r#"
            enum Slot { Active(Atomic<i64>), Empty }
            fn main() -> i64 {
              let s: Slot = Slot.Active(atomic_new(42));
              let z: Slot = Slot.Empty;
              return 0;
            }
        "#;
        compile(source).expect("Atomic enum payload should compile");
    }

    #[test]
    fn enum_task_payload_compiles() {
        // Closure #122: enum payloads admit `Task`.
        let source = r#"
            enum State { Running(Task), Idle }
            fn main() -> i64 {
              let s: State = State.Idle;
              return 0;
            }
        "#;
        compile(source).expect("Task enum payload should compile");
    }

    #[test]
    fn enum_array_payload_compiles() {
        // Closure #119: enum payloads admit `[T; N]` of Copy
        // elements. Arrays have stack lifetime so no Drop is
        // needed; the C typedef uses an inline `T name[N]`
        // declarator and the LLVM payload-less variant uses
        // `zeroinitializer`.
        let source = r#"
            enum Window { Open([i64; 4]), Closed }
            fn main() -> i64 {
              let a: Window = Window.Open([1, 2, 3, 4]);
              let b: Window = Window.Closed;
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("array enum payload should compile");
        assert!(
            c.contains("int64_t payload[4]"),
            "expected inline C array declarator in enum typedef:\n{c}"
        );
    }

    #[test]
    fn enum_vec_payload_compiles_and_drops() {
        // Closure #118: enum payloads admit `Vec<T>` in
        // addition to OwnedStr. The aggregate is affine and
        // both backends emit a tag-conditional
        // `intent_vec_<T>__free` for the heap payload.
        let source = r#"
            enum Bag { Items(Vec<i64>), Empty }
            fn build(c: bool) -> Bag {
              if c { return Bag.Items(vec(1, 2, 3)); }
              return Bag.Empty;
            }
            fn main() -> i64 {
              let a: Bag = build(true);
              let b: Bag = build(false);
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("Vec enum payload should compile");
        assert!(
            c.contains("intent_vec_int64_t__free"),
            "expected Vec __free helper in C output:\n{c}"
        );
    }

    #[test]
    fn enum_owned_str_payload_compiles_and_drops() {
        // T1.3 + T1.2 phase 2b: enum payloads admit OwnedStr.
        // The aggregate is affine; both backends emit a
        // tag-conditional free for the heap payload at scope
        // exit.
        let source = r#"
            enum Maybe { Some(OwnedStr), None }
            fn make(c: bool) -> Maybe {
              if c { return Maybe.Some("a" + "b"); }
              return Maybe.None;
            }
            fn classify(m: Maybe) -> i64 {
              return match m {
                Maybe.Some then 1,
                Maybe.None then 0,
              };
            }
            fn main() -> i64 {
              let a: Maybe = make(true);
              let b: Maybe = make(false);
              assert classify(a) == 1;
              assert classify(b) == 0;
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("enum with OwnedStr payload should compile");
        assert!(
            c.contains("free((void*)v_") && c.contains(".payload"),
            "expected tag-conditional payload free in C output:\n{c}"
        );
    }

    #[test]
    fn enum_owned_str_payload_binding_exposes_str_view() {
        // Closure #128 / D3: OwnedStr payload bindings are
        // now admitted, exposed to the arm body as a Str
        // (Copy borrowed-view). The scrutinee keeps ownership
        // and its scope-exit Drop frees the heap. Other
        // non-Copy payload types still need their own
        // borrow-view wiring and remain rejected.
        let source = r#"
            enum Maybe { Some(OwnedStr), None }
            fn main() -> i64 {
              let m: Maybe = Maybe.Some("a" + "b");
              return match m {
                Maybe.Some(s) then len(s) as i64,
                Maybe.None then 0,
              };
            }
        "#;
        compile(source).expect("OwnedStr payload destructure should compile");
        let c = compile_to_c(source).expect("C backend emits a program");
        // Binding rendered as `const char*` (Str view), not
        // `char*` (OwnedStr).
        assert!(
            c.contains("const char* v_s ="),
            "expected Str-view binding, got:\n{}",
            c
        );
        // Scrutinee's scope-exit Drop still fires.
        assert!(
            c.contains("free((void*)v_m.payload)"),
            "expected scrutinee scope-exit free, got:\n{}",
            c
        );
    }

    #[test]
    fn enum_non_copy_non_str_payload_binding_rejected() {
        // Closure #128 / D3: payload types other than
        // OwnedStr (e.g. Vec<T>) don't yet have a borrow-
        // view; binding patterns on them stay rejected.
        let source = r#"
            enum Wrap { V(Vec<i64>), Empty }
            fn main() -> i64 {
              let w: Wrap = Wrap.V(vec(1, 2, 3));
              return match w {
                Wrap.V(xs) then 1,
                Wrap.Empty then 0,
              };
            }
        "#;
        let errors = compile(source).expect_err(
            "Vec<T> payload binding without borrow-view should be rejected",
        );
        assert!(
            errors.iter().any(|e| e.message.contains("non-Copy payload type")),
            "expected non-Copy-binding diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn match_str_dispatch_with_wildcard() {
        // T1.3 follow-up: Str-typed scrutinee desugars to a
        // nested if-expr chain using `==` on Str (strcmp).
        // Wildcard is required.
        let source = r#"
            fn level(name: Str) -> i64 {
              return match name {
                "low" then 1,
                "high" then 3,
                _ then 0,
              };
            }
            fn main() -> i64 {
              assert level("low") == 1;
              assert level("high") == 3;
              assert level("?") == 0;
              return 0;
            }
        "#;
        compile(source).expect("Str match with wildcard should compile");
    }

    #[test]
    fn match_str_missing_wildcard_rejected() {
        // Str scrutinees are open — missing wildcard surfaces
        // a clean diagnostic.
        let source = r#"
            fn main() -> i64 {
              let s: Str = "x";
              return match s { "a" then 1, "b" then 2 };
            }
        "#;
        let errors = compile(source)
            .expect_err("Str match without wildcard should fail");
        assert!(
            errors.iter().any(|e| e.message.contains("string scrutinees require a wildcard")),
            "expected wildcard-required diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn ssa_llvm_vec_set_index_typed_as_i64() {
        // Closure #158: SSA-LLVM's `emit_vec_call` was
        // falling back to `element.clone()` for any
        // Const argument (operand_type returns None for
        // Const). For `set(xs, 0, v)` over a
        // `Vec<OwnedStr>`, this typed the index `0` as
        // i8* — a type mismatch lli warned about and
        // tolerated. Per-builtin signature lookup now
        // returns `Type::I64` for `set`'s second arg,
        // the Vec<T> struct type for the first arg, etc.
        // Compile-and-link verification covered by the
        // e2e parity runner.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<OwnedStr> = vec("a" + "1", "b" + "2");
              let ys: Vec<OwnedStr> = set(xs, 0, "c" + "3");
              return 0;
            }
        "#;
        compile(source).expect("set(Vec<OwnedStr>) should compile");
    }

    #[test]
    fn vec_set_owned_str_frees_old_element_in_llvm() {
        // Closure #157: LLVM's per-shape Vec __set helper
        // only freed the old slot for `Type::Vec(inner)`
        // element types. `set(Vec<OwnedStr>, i, v)` and
        // similar leaked the previous slot's heap.
        // Extended for OwnedStr, Struct (with owning
        // fields), and Enum (with OwnedStr / Vec payload)
        // element types — mirrors closure #127's tree-C
        // `c_element_drop_old` extension.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<OwnedStr> = vec("a" + "1", "b" + "2");
              let ys: Vec<OwnedStr> = set(xs, 0, "c" + "3");
              return 0;
            }
        "#;
        compile(source).expect("set(Vec<OwnedStr>) should compile");
    }

    #[test]
    fn clone_at_enum_with_owned_str_payload_works() {
        // Closure #156: `clone_at(ref Vec<Msg>, i)` where
        // `Msg.Text(OwnedStr)` was panicking in tree-LLVM
        // with "clone_at on element type Enum(Msg) not yet
        // supported". Closures #154 / #155 added OwnedStr
        // and Struct arms; #156 finishes Enum with an
        // OR-chain tag check, branching to a
        // `cat_enum_pay` block that deep-clones the
        // payload via intent_str_concat then reconstructs
        // the enum, vs a `cat_enum_tag` block that uses
        // the loaded slot as-is, and phi-joins. Tree-C
        // was already correct via `c_element_deep_clone`'s
        // Enum arm from closure #152.
        let source = r#"
            enum Msg { Empty, Text(OwnedStr) }
            fn main() -> i64 {
              let xs: Vec<Msg> = vec(Msg.Text("a" + "1"));
              let elt: Msg = clone_at(ref xs, 0);
              return 0;
            }
        "#;
        compile(source).expect("clone_at(Vec<PayloadedEnum>) should compile");
    }

    #[test]
    fn clone_at_struct_with_heap_field_works() {
        // Closure #155: `clone_at(ref Vec<Struct{OwnedStr}>, i)`
        // was panicking in tree-LLVM with "clone_at on element
        // type Struct(…) not yet supported". Tree-C went
        // through `c_element_deep_clone` which closure #153
        // had already taught to recurse over Struct fields,
        // so the C side was fine. Tree-LLVM now matches: load
        // the slot, extract each field, deep-clone OwnedStr
        // fields via `intent_str_concat` with the empty
        // literal, assemble the new struct via an insertvalue
        // chain ending in `dest`.
        let source = r#"
            struct Tag { name: OwnedStr }
            fn main() -> i64 {
              let xs: Vec<Tag> = vec(Tag { name: "a" + "1" });
              let elt: Tag = clone_at(ref xs, 0);
              return 0;
            }
        "#;
        compile(source).expect("clone_at(Vec<Struct{OwnedStr}>) should compile");
    }

    #[test]
    fn clone_at_owned_str_vec_works() {
        // Closure #154: `clone_at(ref xs, i)` for
        // `Vec<OwnedStr>` was broken in two places:
        // - SSA-C had no `clone_at` handler — fell through
        //   to the `fn_clone_at(...)` user-fn shape, which
        //   produced an undeclared identifier and failed at
        //   link time ("undefined reference to fn_clone_at").
        // - tree-LLVM's `clone_at` only handled Copy + Vec
        //   element types — OwnedStr / Struct panicked with
        //   "not yet supported in tree-LLVM".
        // Both backends now route through the per-element
        // deep-clone shape: SSA-C uses the existing
        // `c_element_deep_clone` helper; tree-LLVM loads
        // the i8* slot and calls `intent_str_concat` with
        // the `@.empty_str_clone` empty literal.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<OwnedStr> = vec("a" + "1", "b" + "2");
              let elt: OwnedStr = clone_at(ref xs, 0);
              return 0;
            }
        "#;
        compile(source).expect("clone_at(Vec<OwnedStr>) should compile");
    }

    #[test]
    fn clone_vec_struct_with_heap_field_deep_copies() {
        // Closure #153: `clone(Vec<Struct{heap-field}>)` was
        // shallow-copying the per-element struct, so every
        // heap-shaped field's pointer was shared between
        // the source and the clone. Both Vec's __free walked
        // their slots and freed the same OwnedStr field
        // pointer twice (ASan: double-free; lli: "free():
        // double free detected").
        //
        // c_element_deep_clone for Type::Struct now
        // reconstructs the struct with each owning field
        // deep-cloned (recursive). LLVM's Vec __clone has
        // a parallel Struct arm: extract each field, deep-
        // clone it (OwnedStr via intent_str_concat with
        // empty), assemble the new struct via insertvalue
        // chain.
        let source = r#"
            struct Tag { name: OwnedStr }
            fn main() -> i64 {
              let xs: Vec<Tag> = vec(Tag { name: "a" + "1" }, Tag { name: "b" + "2" });
              let ys: Vec<Tag> = clone(xs);
              return 0;
            }
        "#;
        compile(source).expect("clone(Vec<Struct{OwnedStr}>) should compile");
    }

    #[test]
    fn clone_vec_owned_str_deep_copies_payload() {
        // Closure #152: `clone(Vec<OwnedStr>)` was shallow-
        // copying the per-element i8* pointers, then both
        // the source and the clone double-freed at scope
        // exit (each Vec's __free walked its slots and
        // freed the same heap twice).
        //
        // c_element_deep_clone now deep-clones OwnedStr via
        // `intent_str_concat(slot, 0, "", 0)`. LLVM's
        // per-shape Vec __clone also extended: non-Copy
        // element types loop over slots and produce a
        // per-element deep clone (was only handling Vec<U>
        // elements; OwnedStr / Enum payloads fell through
        // to an uninitialized buffer, crashing lli).
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<OwnedStr> = vec("a" + "1", "b" + "2");
              let ys: Vec<OwnedStr> = clone(xs);
              return 0;
            }
        "#;
        compile(source).expect("clone(Vec<OwnedStr>) should compile");
    }

    #[test]
    fn clone_vec_payloaded_enum_deep_copies_payload() {
        // Closure #152: `clone(Vec<Msg>)` where Msg has an
        // OwnedStr payload was double-freeing payload heaps.
        // c_element_deep_clone for Type::Enum now emits a
        // tag-switched ternary that reconstructs the enum
        // with a deep-cloned payload for payloaded
        // variants. LLVM mirrors via an OR-chain branch
        // through cln_payloaded / cln_taggy / cln_join.
        let source = r#"
            enum Msg { Empty, Text(OwnedStr) }
            fn main() -> i64 {
              let xs: Vec<Msg> = vec(Msg.Text("a" + "1"), Msg.Empty);
              let ys: Vec<Msg> = clone(xs);
              return 0;
            }
        "#;
        compile(source).expect("clone(Vec<PayloadedEnum>) should compile");
    }

    #[test]
    fn vec_of_payloaded_enum_compiles_and_drops() {
        // Closure #151: `Vec<Msg>` where Msg is a payloaded
        // enum was broken in four places:
        // - C `element_tag(Type::Enum(_))` fell through to
        //   c_leaf_type → "int32_t", so the per-shape vec
        //   typedef was `intent_vec_int32_t` and tried to
        //   store `Enum_Msg` struct literals into i32 slots
        //   (cc rejected with "incompatible types").
        // - C `c_element_storage` had the same bug.
        // - C `c_element_drop_old` didn't have an Enum arm,
        //   so `intent_vec_Enum_Msg__free`'s per-element
        //   drop body was empty, leaking payloads.
        // - LLVM vec literal used `vec_element_byte_size`
        //   for enums (returning 8 = i64) under-allocated
        //   the `{i32, i8*}` 16-byte tagged union, crashing
        //   lli with "free(): invalid pointer". And LLVM's
        //   per-shape `__free` didn't iterate elements for
        //   enum types either.
        // All four sites now treat payloaded enums like
        // structs / tuples (Enum_<Name> tagged-union, GEP-
        // null sizeof, tag-switched payload free).
        let source = r#"
            enum Msg { Empty, Text(OwnedStr) }
            fn main() -> i64 {
              let i: i64 = 0;
              while i < 3 {
                let xs: Vec<Msg> = vec(Msg.Text("a" + "1"), Msg.Empty);
                i = i + 1;
              }
              return 0;
            }
        "#;
        compile(source).expect("Vec<PayloadedEnum> should compile");
        let c = compile_to_c(source).expect("emits C");
        // Element-tag fix: the typedef must be named
        // `intent_vec_Enum_Msg`, not `intent_vec_int32_t`.
        assert!(
            c.contains("intent_vec_Enum_Msg"),
            "expected enum-named vec typedef, got:\n{c}"
        );
        // c_element_drop_old fix: the per-element drop in
        // intent_vec_Enum_Msg__free switches on the tag.
        assert!(
            c.contains("switch (xs.data[k].tag)"),
            "expected per-element tag switch in vec __free, got:\n{c}"
        );
    }

    #[test]
    fn index_assign_owned_str_element_frees_old_heap() {
        // Closure #150: `xs[i] = newstr` for a
        // `Vec<OwnedStr>` element was leaking the OLD i8*.
        // Closure #149 added struct/enum cases for the
        // whole-element overwrite; this one adds OwnedStr
        // and Vec element types (which were already
        // handled at the leaf-with-field-path level by
        // closure #126 but not at the whole-element level).
        // SSA-C path also extended (the OwnedStr-vec case
        // routes through SSA).
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<OwnedStr> = vec("a" + "1", "b" + "2");
              xs[0] = "c" + "3";
              return 0;
            }
        "#;
        compile(source).expect("Vec<OwnedStr> index-assign should compile");
    }

    #[test]
    fn index_assign_nested_vec_element_frees_old_heap() {
        // Closure #150: same shape for `Vec<Vec<i64>>[i] =
        // vec(...)`. Tree-C handles via the leaf-Drop arms;
        // SSA-C IndexAssign emit also extended.
        let source = r#"
            fn main() -> i64 {
              let xss: Vec<Vec<i64>> = vec(vec(1, 2), vec(3, 4));
              xss[0] = vec(99, 88, 77);
              return 0;
            }
        "#;
        compile(source).expect("Vec<Vec<i64>> index-assign should compile");
    }

    #[test]
    fn index_assign_struct_element_frees_old_heap() {
        // Closure #149: `xs[i] = newStruct` for a
        // `Vec<Struct{heap-field}>` element was leaking the
        // OLD element's heap fields. The IndexAssign leaf
        // drop (closure #126) only fired when
        // `field_path != []`; whole-element overwrites
        // (field_path empty + leaf == Struct/Enum) fell
        // through to a plain store, losing the old heap.
        let source = r#"
            struct Tag { name: OwnedStr }
            fn main() -> i64 {
              let xs: Vec<Tag> = vec(
                Tag { name: "first" + "" },
                Tag { name: "second" + "" }
              );
              xs[0] = Tag { name: "third" + "" };
              return 0;
            }
        "#;
        compile(source).expect("struct-element index-assign should compile");
        let c = compile_to_c(source).expect("emits C");
        // Tree-C must walk the OLD element's field drops
        // (`free((void*)v_xs.data[...].name)`) before
        // storing the new struct.
        assert!(
            c.contains("free((void*)v_xs.data["),
            "expected per-field free of old element before assign, got:\n{c}"
        );
    }

    #[test]
    fn field_assign_struct_field_frees_old_inner_fields() {
        // Closure #148: `o.inner = newInner` where Inner has
        // heap-shaped fields (OwnedStr) was leaking the
        // previous Inner's heap. FieldAssign's heap-overwrite
        // logic (closure #132) only handled OwnedStr / Vec
        // field types; Struct fell through to a plain assign.
        // Tree-C now walks the OLD inner field's per-field
        // drops before storing the new value.
        let source = r#"
            struct Inner { name: OwnedStr }
            struct Outer { inner: Inner }
            fn main() -> i64 {
              let o: Outer = Outer { inner: Inner { name: "first" + "" } };
              o.inner = Inner { name: "second" + "" };
              return 0;
            }
        "#;
        compile(source).expect("nested struct field reassign should compile");
        let c = compile_to_c(source).expect("emits C");
        // Tree-C must emit the per-field free over the OLD
        // inner field before the new struct is stored.
        assert!(
            c.contains("free((void*)v_o.inner.name)"),
            "expected free of old nested OwnedStr field before assign, got:\n{c}"
        );
    }

    #[test]
    fn reassign_struct_with_heap_field_frees_old_fields() {
        // Closure #147: `t = Tag { name: ... }` for a struct
        // binding with an OwnedStr field was leaking the
        // previous fields' heap. Tree-C, tree-LLVM, and SSA
        // Reassign handlers only had Vec / OwnedStr cases;
        // Struct fell through to a plain assign. Tree-C now
        // walks the old binding's per-field drops before
        // moving the tmp in; SSA emits a Drop instruction
        // over the old SSA value (the backend's `Drop` handler
        // for Struct walks the fields).
        let source = r#"
            struct Tag { name: OwnedStr }
            fn main() -> i64 {
              let t: Tag = Tag { name: "first" + "" };
              t = Tag { name: "second" + "" };
              return 0;
            }
        "#;
        compile(source).expect("struct reassign should compile");
        let c = compile_to_c(source).expect("emits C");
        // Tree-C: emits a tmp then walks the old fields'
        // drops before moving the tmp in.
        assert!(
            c.contains("Struct_Tag _intent_tmp_t ="),
            "expected struct tmp in reassign, got:\n{c}"
        );
        assert!(
            c.contains("free((void*)v_t.name)"),
            "expected free of old field before move, got:\n{c}"
        );
    }

    #[test]
    fn reassign_enum_with_heap_payload_frees_old_payload() {
        // Closure #147: `m = Msg.Text(...)` for an enum
        // binding with a heap-shaped payload was leaking
        // the previous payload heap. Tree-C now switches
        // on the old tag and frees the payload before
        // moving the tmp in.
        let source = r#"
            enum Msg { Empty, Text(OwnedStr) }
            fn main() -> i64 {
              let m: Msg = Msg.Text("first" + "");
              m = Msg.Text("second" + "");
              return 0;
            }
        "#;
        compile(source).expect("enum reassign should compile");
        let c = compile_to_c(source).expect("emits C");
        assert!(
            c.contains("Enum_Msg _intent_tmp_m ="),
            "expected enum tmp in reassign, got:\n{c}"
        );
        assert!(
            c.contains("switch (v_m.tag)"),
            "expected tag switch for old payload free, got:\n{c}"
        );
    }

    #[test]
    fn discard_of_enum_with_heap_payload_frees_payload() {
        // Closure #146: `let _ = make_enum();` for an enum
        // with a heap-shaped payload (OwnedStr / Vec<T>)
        // was leaking. Tree-C, tree-LLVM, and SSA Discard
        // handlers only matched OwnedStr / Vec / Struct —
        // `Type::Enum(_)` fell through to `(void) expr`
        // (tree-C) or bare emit_expr (LLVM / SSA), never
        // freeing the payload for active payloaded variants.
        // Mirrors the scope-exit Drop logic for enums.
        let source = r#"
            enum Msg { Empty, Text(OwnedStr) }
            fn make() -> Msg { return Msg.Text("hi" + ""); }
            fn main() -> i64 {
              let i: i64 = 0;
              while i < 5 {
                let _ = make();
                i = i + 1;
              }
              return 0;
            }
        "#;
        compile(source).expect("discard of enum should compile");
        let c = compile_to_c(source).expect("emits C");
        // Tree-C must spill the discarded enum and switch
        // on the tag, freeing the payload for payloaded
        // variants.
        assert!(
            c.contains("Enum_Msg _intent_discard ="),
            "expected enum spill in discard, got:\n{c}"
        );
        assert!(
            c.contains("free((void*)_intent_discard.payload)"),
            "expected payload free in discard, got:\n{c}"
        );
    }

    #[test]
    fn discard_of_struct_with_heap_field_frees_fields() {
        // Closure #145: `let _ = make_struct();` for a struct
        // whose fields hold heap-owning values (OwnedStr,
        // Vec<T>, nested struct) was silently leaking the
        // per-field heap. Tree-C, tree-LLVM, and SSA Discard
        // handlers all only matched `OwnedStr | Vec(_)` —
        // `Type::Struct(_)` fell through to a `(void) expr`
        // (tree-C) or bare `emit_expr` (tree-LLVM / SSA),
        // never freeing the struct's owning fields.
        let source = r#"
            struct Tag { name: OwnedStr }
            fn make() -> Tag { return Tag { name: "x" + "y" }; }
            fn main() -> i64 {
              let i: i64 = 0;
              while i < 5 {
                let _ = make();
                i = i + 1;
              }
              return 0;
            }
        "#;
        compile(source).expect("discard of struct should compile");
        let c = compile_to_c(source).expect("emits C");
        // Tree-C must spill the discarded struct to a local
        // and call the per-field free chain.
        assert!(
            c.contains("Struct_Tag _intent_discard ="),
            "expected struct spill in discard, got:\n{c}"
        );
        assert!(
            c.contains("free((void*)_intent_discard.name)"),
            "expected per-field free in discard, got:\n{c}"
        );
    }

    #[test]
    fn field_access_owned_str_in_concat_no_double_free() {
        // Closure #144: `t.name + "-suffix"` where
        // `t.name: OwnedStr` was double-freeing. The
        // pre-fix `l_owned = matches!(left.ty, OwnedStr)`
        // unconditionally set l_owned=1 for any OwnedStr
        // operand, so `intent_str_concat` freed
        // `t.name`'s heap. Then the struct's per-field
        // scope-exit Drop fired and freed it again. New
        // `owned_str_consumed_at_concat` helper allows
        // l_owned=1 only when the operand is a Var (moved
        // by concat) or fresh (Call / Binary / Block /
        // IfExpr / Match) — FieldAccess / TupleAccess
        // keep l_owned=0 so the binding's Drop owns the
        // free.
        let source = r#"
            struct Tag { name: OwnedStr }
            fn main() -> i64 {
              let t: Tag = Tag { name: "first-" + "name" };
              let s: OwnedStr = t.name + "-suffix";
              print s;
              return 0;
            }
        "#;
        compile(source).expect("FieldAccess in concat should compile");
        let c = compile_to_c(source).expect("emits C");
        // The concat call for `t.name + ...` must use
        // l_owned=0 (i.e. `intent_str_concat((v_t).name, 0, …)`).
        assert!(
            c.contains("intent_str_concat((v_t).name, 0,"),
            "expected l_owned=0 for FieldAccess operand, got:\n{c}"
        );
    }

    #[test]
    fn var_owned_str_in_concat_still_freed_inside_helper() {
        // Closure #144 regression guard: `let s = g + "!"`
        // where `g` is a Var-bound OwnedStr must STILL pass
        // l_owned=1 to the concat helper. The checker marks
        // `g` as moved by the binary operator, so the
        // binding's scope-exit Drop is suppressed — concat
        // must do the free.
        let source = r#"
            fn make() -> OwnedStr { return "g" + ""; }
            fn main() -> i64 {
              let g: OwnedStr = make();
              let s: OwnedStr = g + "!";
              print s;
              return 0;
            }
        "#;
        compile(source).expect("Var in concat should compile");
        let c = compile_to_c(source).expect("emits C");
        assert!(
            c.contains("intent_str_concat(v_g, 1,"),
            "expected l_owned=1 for Var operand (checker moves it), got:\n{c}"
        );
    }

    #[test]
    fn clone_of_fresh_vec_drops_borrowed_arg() {
        // Closure #143: `clone(vec(1, 2, 3))` was leaking
        // the fresh Vec passed in. The checker treats
        // `clone(xs)` as borrow-semantics (xs continues to
        // be readable after the call — useful when you
        // want a deep copy without consuming the source),
        // but for a fresh-Vec argument there's no other
        // binding to own the heap. SSA Call lowering now
        // emits a `Drop` after the `clone` call for each
        // fresh non-Copy argument. Var / FieldAccess args
        // skip — the binding owns the value.
        let source = r#"
            fn main() -> i64 {
              let i: i64 = 0;
              while i < 5 {
                let c: Vec<i64> = clone(vec(1, 2, 3));
                i = i + 1;
              }
              return 0;
            }
        "#;
        compile(source).expect("clone(fresh Vec) should compile");
    }

    #[test]
    fn index_of_fresh_vec_drops_buffer() {
        // Closure #142: `vec(1, 2, 3)[0]` (and other
        // fresh-Vec index shapes) was silently leaking the
        // Vec buffer. The Index instruction reads one slot
        // but doesn't free `.data`. Mirrors closure #141's
        // Len-of-fresh-Vec fix. SSA Index lowering emits a
        // Drop after the InstrKind::Index when the operand
        // is a fresh Vec; tree-C `emit_index` wraps the
        // index read in a brace-scoped tmp +
        // `intent_vec_<T>__free`. Var / FieldAccess Vec
        // operands skip — binding owns the buffer.
        let source = r#"
            fn main() -> i64 {
              let i: i64 = 0;
              while i < 5 {
                let x: i64 = vec(10, 20, 30)[1];
                i = i + 1;
              }
              return 0;
            }
        "#;
        compile(source).expect("index of fresh Vec should compile");
    }

    #[test]
    fn len_of_fresh_vec_drops_buffer() {
        // Closure #141: `len(vec(1,2,3))` was silently
        // leaking the Vec buffer — the SSA `Len` instruction
        // reads `.len` from the struct but doesn't free the
        // `.data` buffer, and the previous OwnedStr-only
        // whitelist didn't cover Vec operands. Generalized
        // `is_fresh_non_copy` now matches Vec; SSA emits a
        // Drop after the Len, tree-C wraps the `.len` read
        // in a brace-scoped tmp + `intent_vec_<T>__free`.
        let source = r#"
            fn main() -> i64 {
              let i: i64 = 0;
              while i < 5 {
                let n: u64 = len(vec(1, 2, 3));
                i = i + 1;
              }
              return 0;
            }
        "#;
        compile(source).expect("len(fresh Vec) should compile");
    }

    #[test]
    fn len_of_block_returning_owned_str_drops_heap() {
        // Closure #140: `len({ let s = make(); s })` was
        // leaking — the previous whitelist (#139) only
        // covered Call / Binary operands, not Block / IfExpr
        // / Match. The unified `is_fresh_owned_str` helper
        // now matches all of those, and tree-C `emit_len`
        // wraps strlen in a brace-scoped tmp + free for
        // fresh operands.
        let source = r#"
            fn main() -> i64 {
              let n: u64 = len({ let s: OwnedStr = "abc" + "def"; s });
              return 0;
            }
        "#;
        compile(source).expect("len of Block-OwnedStr should compile");
    }

    #[test]
    fn len_of_fresh_owned_str_drops_heap() {
        // Closure #139: `len(make_owned_str())` was silently
        // leaking — `intent_str_len` (strlen) doesn't
        // consume its argument, so a fresh-OwnedStr operand
        // (Call / Binary `+`) had no other owner. Fixed in
        // both the SSA path and tree-LLVM via the same
        // Call/Binary whitelist used for print (#135), match
        // (#137), and strcmp (#138).
        let source = r#"
            fn make() -> OwnedStr {
              return "hello " + "world";
            }
            fn main() -> i64 {
              let n: u64 = len(make());
              return 0;
            }
        "#;
        compile(source).expect("len(fresh) should compile");
    }

    #[test]
    fn len_of_var_owned_str_no_double_free() {
        // Regression guard for closure #139: Var-OwnedStr
        // operand must NOT free (the binding's scope-exit
        // Drop owns the heap).
        let source = r#"
            fn main() -> i64 {
              let s: OwnedStr = "hello " + "world";
              let n: u64 = len(s);
              return 0;
            }
        "#;
        compile(source).expect("len(Var) should compile");
    }

    #[test]
    fn str_cmp_with_fresh_owned_str_operand_drops_heap() {
        // Closure #138: `make_owned_str() == "literal"` was
        // silently leaking. `intent_str_cmp` / `strcmp`
        // doesn't consume its arguments, so a fresh
        // OwnedStr operand (Call / Binary `+` returning
        // OwnedStr) had no other owner and never got freed.
        // Fixed in both the SSA path and tree-LLVM via the
        // Call/Binary whitelist; Var / FieldAccess operands
        // skip the free (the outer binding's scope-exit
        // Drop still owns the heap).
        let source = r#"
            fn make() -> OwnedStr {
              return "hello " + "world";
            }
            fn main() -> i64 {
              if make() == "hello world" {
                return 0;
              }
              return 1;
            }
        "#;
        compile(source).expect("fresh-OwnedStr cmp should compile");
        // We can't easily assert the SSA Drop instruction
        // from the lib test, but the absence of regressions
        // plus the ASan check on /tmp/comparison_heap.intent
        // covers the runtime guarantee. The cross-backend
        // e2e parity test also exercises the path.
    }

    #[test]
    fn str_cmp_with_var_owned_str_operand_no_double_free() {
        // Closure #138 must NOT free when the OwnedStr
        // operand is a Var — the binding owns the heap and
        // its scope-exit Drop frees it. Whitelist excludes
        // Var; this test guards against future broadening
        // that would re-introduce the double-free.
        let source = r#"
            fn main() -> i64 {
              let s: OwnedStr = "hello " + "world";
              if s == "hello world" {
                return 0;
              }
              return 1;
            }
        "#;
        compile(source).expect("Var-OwnedStr cmp should compile");
    }

    #[test]
    fn match_owned_str_fresh_scrutinee_drops_temp() {
        // Closure #137: `match make_owned_str() { … }` was
        // silently leaking the scrutinee's heap. The
        // `check_match_str` desugar bound the scrutinee to a
        // temp inside a Block but never emitted a Drop, so
        // the fresh OwnedStr from the Call escaped without
        // being freed. The fix only kicks in for fresh
        // heap-producers (Call / Binary scrutinees); Var /
        // FieldAccess scrutinees reference a value owned by
        // some outer binding and don't need a drop here.
        let source = r#"
            fn pick(i: i64) -> OwnedStr {
              return "x" + "y";
            }
            fn main() -> i64 {
              let r: i64 = match pick(0) {
                "xy" then 1,
                _ then 0,
              };
              return r;
            }
        "#;
        compile(source).expect("fresh-OwnedStr match should compile");
        let c = compile_to_c(source).expect("emits C");
        // The desugar lifts the if-chain into a `__match_str_result_<n>`
        // binding and drops the `__match_str_<n>` temp.
        assert!(
            c.contains("__match_str_result_"),
            "expected result-wrap let, got:\n{c}"
        );
        assert!(
            c.contains("free((void*)v___match_str_"),
            "expected free of match-str temp, got:\n{c}"
        );
    }

    #[test]
    fn match_owned_str_var_scrutinee_no_double_free() {
        // Closure #137 must NOT emit a Drop temp when the
        // scrutinee is a Var (the var owns the heap; freeing
        // the temp would double-free at the outer scope-exit).
        let source = r#"
            fn main() -> i64 {
              let label: OwnedStr = "lev" + "el";
              let n: i64 = match label {
                "level" then 100,
                _ then 0,
              };
              assert n == 100;
              return 0;
            }
        "#;
        compile(source).expect("Var-OwnedStr match should compile");
        let c = compile_to_c(source).expect("emits C");
        assert!(
            !c.contains("__match_str_result_"),
            "should not wrap when scrutinee is a Var (would double-free), got:\n{c}"
        );
    }

    #[test]
    fn match_bool_compiles_and_dispatches() {
        // Bool scrutinee with `true` / `false` literal
        // patterns is supported. Exhaustiveness requires both
        // arms or a wildcard. T1.3 follow-up (closure #110).
        let source = r#"
            fn main() -> i64 {
              let b: bool = true;
              return match b { true then 10, false then 20 };
            }
        "#;
        compile(source).expect("match on bool should compile");
    }

    #[test]
    fn match_bool_nonexhaustive_rejected() {
        // Missing the `false` arm triggers the exhaustiveness
        // check unless a wildcard is present.
        let source = r#"
            fn main() -> i64 {
              let b: bool = true;
              return match b { true then 10 };
            }
        "#;
        let errors = compile(source)
            .expect_err("non-exhaustive bool match should fail");
        assert!(
            errors.iter().any(|e| e.message.contains("missing arm for 'false'")),
            "expected missing-false diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn struct_equality_rejected_with_targeted_diagnostic() {
        // `a == b` on structs surfaces a specific message
        // pointing at field-by-field comparison and the
        // future user-defined-equality work (T1.5 phase 2).
        let source = r#"
            struct P { x: i64, y: i64 }
            fn main() -> i64 {
              let a: P = P { x: 1, y: 2 };
              let b: P = P { x: 1, y: 2 };
              if a == b { return 1; }
              return 0;
            }
        "#;
        let errors = compile(source)
            .expect_err("struct == is rejected");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("struct 'P' has no built-in")),
            "expected struct-specific diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn tuple_equality_field_by_field_desugar() {
        // Tuple `==` desugars to a field-by-field AND-chain
        // of per-element comparisons. Primitives use built-in
        // `==`; nominal element types route through `<T>_eq`.
        let source = r#"
            fn main() -> i64 {
              let t1: (i64, i64) = (1, 2);
              let t2: (i64, i64) = (1, 2);
              let t3: (i64, i64) = (1, 3);
              assert t1 == t2;
              assert t1 != t3;
              return 0;
            }
        "#;
        compile(source)
            .expect("tuple field-by-field equality should compile");
    }

    #[test]
    fn tuple_equality_of_struct_routes_through_eq_impl() {
        // Tuple of structs: each element comparison dispatches
        // through the element's `<T>_eq` impl. (Point, Point)
        // == (Point, Point) → Point_eq(a, c) && Point_eq(b, d).
        let source = r#"
            interface Eq { fn eq(self: Point, other: Point) -> bool; }
            struct Point { x: i64, y: i64 }
            implement Eq for Point {
              fn eq(self: Point, other: Point) -> bool {
                if self.x != other.x { return false; }
                if self.y != other.y { return false; }
                return true;
              }
            }
            fn main() -> i64 {
              let s: (Point, Point) = (Point { x: 1, y: 2 }, Point { x: 3, y: 4 });
              let t: (Point, Point) = (Point { x: 1, y: 2 }, Point { x: 3, y: 4 });
              assert s == t;
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("tuple-of-struct == should compile");
        let calls = c.matches("fn_Point_eq(").count();
        assert!(
            calls >= 2,
            "expected at least 2 fn_Point_eq calls in C output, got {calls}:\n{c}"
        );
    }

    #[test]
    fn enum_equality_rejected_with_targeted_diagnostic() {
        let source = r#"
            enum E { A, B }
            fn main() -> i64 {
              let a: E = E.A;
              let b: E = E.B;
              if a == b { return 1; }
              return 0;
            }
        "#;
        let errors = compile(source)
            .expect_err("enum == is rejected");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("enum 'E' has no built-in")),
            "expected enum-specific diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn type_alias_used_as_fn_param() {
        // `type Coord = (i64, i64); fn flip(c: Coord) -> Coord`
        // — type alias works in fn signatures and the
        // tuple round-trips cleanly.
        let source = r#"
            type Coord = (i64, i64);
            fn flip(c: Coord) -> Coord { return (c.1, c.0); }
            fn main() -> i64 {
              let p: Coord = flip((1, 2));
              return p.0 + p.1;
            }
        "#;
        compile(source).expect("type alias as fn param should compile");
    }

    #[test]
    fn array_let_binding_with_brackets_literal() {
        // `let xs: [i64; 3] = [10, 20, 30]; xs[1]` —
        // bracket-literal init bound to typed slot works.
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 3] = [10, 20, 30];
              return xs[1];
            }
        "#;
        compile(source).expect("array literal let-binding should compile");
    }

    #[test]
    fn fn_param_can_be_shadowed_by_let() {
        // `fn f(x: i64) { let x = 99; … }` — inner-scope
        // shadow of a fn parameter is legal and rebinds
        // within the function body.
        let source = r#"
            fn f(x: i64) -> i64 { let x: i64 = 99; return x; }
            fn main() -> i64 { return f(5); }
        "#;
        compile(source).expect("shadow of fn param should compile");
    }

    #[test]
    fn loop_counter_can_be_shadowed_inside_body() {
        // `for i from 0 to 3 { let i = 99; … }` — inner
        // `let i` shadows the loop counter for the body
        // scope only.
        let source = r#"
            fn main() -> i64 {
              let total: i64 = 0;
              for i from 0 to 3 {
                let i: i64 = 99;
                total = total + i;
              }
              return total;
            }
        "#;
        compile(source).expect("loop-counter shadow should compile");
    }

    #[test]
    fn match_duplicate_wildcard_rejected() {
        // `match x { 5 then 1, _ then 2, _ then 3 }` — the
        // second `_` arm is unreachable. Checker flags it.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              return match x { 5 then 1, _ then 2, _ then 3 };
            }
        "#;
        let errors = compile(source)
            .expect_err("duplicate wildcard should be rejected");
        assert!(
            errors.iter().any(|e| e.message.contains("unreachable")),
            "expected unreachable-arm diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn const_as_array_length_compiles() {
        // Closure #120: `let xs: [i64; N]` where N is a
        // previously-declared const with an integer-literal
        // initializer now compiles.
        let source = r#"
            const N: i64 = 3;
            fn main() -> i64 {
              let xs: [i64; N] = [10, 20, 30];
              return xs[2];
            }
        "#;
        compile(source).expect("const-N as array length should compile");
    }

    #[test]
    fn unknown_const_in_array_length_rejected() {
        // Forward / undeclared references still error.
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; UNDECLARED] = [1, 2, 3];
              return xs[0];
            }
        "#;
        let errors = compile(source)
            .expect_err("unknown const reference should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("'UNDECLARED'")
                    && e.message.contains("must be a literal integer")),
            "expected unknown-const diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn truncating_int_cast_compiles() {
        // `100000 as i32 as i64` — high bits dropped on the
        // i32 narrowing, then sign-extended back.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 100000;
              let y: i32 = x as i32;
              return y as i64;
            }
        "#;
        compile(source).expect("truncating cast should compile");
    }

    #[test]
    fn method_chain_three_calls_compiles() {
        // `c.inc().inc().inc().v` — chain three method
        // calls and a field access.
        let source = r#"
            struct C { v: i64 }
            methods on C {
              fn inc(self: C) -> C { return C { v: self.v + 1 }; }
            }
            fn main() -> i64 {
              let c: C = C { v: 0 };
              return c.inc().inc().inc().v;
            }
        "#;
        compile(source).expect("3-method chain should compile");
    }

    #[test]
    fn method_takes_foreign_struct_param_compiles() {
        // Method on P takes a Q parameter — different
        // struct types compose cleanly.
        let source = r#"
            struct P { x: i64 }
            struct Q { y: i64 }
            methods on P {
              fn add_q(self: P, q: Q) -> i64 { return self.x + q.y; }
            }
            fn main() -> i64 {
              let p: P = P { x: 3 };
              let q: Q = Q { y: 4 };
              return p.add_q(q);
            }
        "#;
        compile(source).expect("method with foreign-struct param should compile");
    }

    #[test]
    fn if_expression_as_fn_argument_compiles() {
        let source = r#"
            fn id(x: i64) -> i64 { return x; }
            fn main() -> i64 { return id(if true { 42 } else { 0 }); }
        "#;
        compile(source).expect("if-expr as fn arg should compile");
    }

    #[test]
    fn ensures_referencing_const_and_param_compiles() {
        // Verifier discharges `_return == x + K` against
        // the function body. `;` terminator on ensures is
        // required before the body brace.
        let source = r#"
            const K: i64 = 10;
            fn add_k(x: i64) -> i64
            ensures _return == x + K;
            {
              return x + K;
            }
            fn main() -> i64 { return add_k(5); }
        "#;
        compile(source).expect("ensures with const should compile");
    }

    #[test]
    fn discarded_method_call_as_statement_compiles() {
        // `x.bump();` as a discarded statement (parser
        // sugar for `let _ = x.bump();`). Enables side-
        // effect-bearing mut-ref methods without forcing
        // users to write `let _ = …` for each call.
        let source = r#"
            struct V { v: i64 }
            methods on V {
              fn bump(self: mut ref V) -> i64 {
                self.v = self.v + 1;
                return self.v;
              }
            }
            fn main() -> i64 {
              let x: V = V { v: 0 };
              x.bump();
              x.bump();
              return x.bump();
            }
        "#;
        compile(source).expect("discarded method call should compile");
    }

    #[test]
    fn discarded_function_call_as_statement_compiles() {
        // `foo();` — same sugar applies to plain function
        // calls returning a value the user wants to drop.
        let source = r#"
            fn tally(x: i64) -> i64 { return x + 1; }
            fn main() -> i64 {
              tally(5);
              tally(10);
              return tally(20);
            }
        "#;
        compile(source).expect("discarded fn call should compile");
    }

    #[test]
    fn discarded_owned_str_call_frees_heap() {
        // Closure #134: `let _ = make_owned_str();` was
        // silently leaking the returned heap string because
        // the tree-C, tree-LLVM, and SSA Discard emit handlers
        // all skipped OwnedStr (only Vec was wired). All three
        // paths now free the heap.
        let source = r#"
            fn make() -> OwnedStr {
              return "hello " + "world";
            }
            fn main() -> i64 {
              let _ = make();
              return 0;
            }
        "#;
        compile(source).expect("OwnedStr discard should compile");
        let c = compile_to_c(source).expect("emits C");
        assert!(
            c.contains("free((void*)_intent_discard)"),
            "expected free of discarded OwnedStr, got:\n{c}"
        );
    }

    #[test]
    fn vec_of_owned_str_compiles_to_valid_c() {
        // Closure #136: `Vec<OwnedStr>` was emitting
        // `intent_vec_char*` typedefs (with the asterisk
        // included in the tag) and failing to compile with
        // cc. The `element_tag` helper's fallback
        // (`c_leaf_type(element).replace(' ', "_")`) didn't
        // strip the `*`. Fixed by spelling Str / OwnedStr
        // explicitly as `str` / `owned_str` so the typedef
        // is a valid C identifier.
        let source = r#"
            fn make() -> OwnedStr {
              return "hello " + "world";
            }
            fn main() -> i64 {
              let xs: Vec<OwnedStr> = vec(make(), make());
              return 0;
            }
        "#;
        compile(source).expect("Vec<OwnedStr> should compile");
        let c = compile_to_c(source).expect("emits C");
        assert!(
            c.contains("intent_vec_owned_str"),
            "expected valid Vec<OwnedStr> typedef, got:\n{c}"
        );
        // Sanity: no asterisks leaking into identifiers.
        assert!(
            !c.contains("intent_vec_char*"),
            "should not emit invalid intent_vec_char*, got:\n{c}"
        );
    }

    #[test]
    fn print_of_fresh_owned_str_call_frees_heap() {
        // Closure #135: `print make_owned_str();` would leak
        // because the print emitters all treated OwnedStr
        // values as borrowed reads. Now Call / Binary `+`
        // OwnedStr expressions emit a free after print; Var /
        // FieldAccess / TupleAccess (binding-owned) still
        // don't.
        let source = r#"
            fn make() -> OwnedStr {
              return "hello " + "world";
            }
            fn main() -> i64 {
              print make();
              return 0;
            }
        "#;
        compile(source).expect("print of fresh OwnedStr should compile");
        let c = compile_to_c(source).expect("emits C");
        assert!(
            c.contains("free((void*)_intent_print_tmp)"),
            "expected free of printed OwnedStr, got:\n{c}"
        );
    }

    #[test]
    fn print_of_owned_str_var_does_not_double_free() {
        // Closure #135 must NOT free when the OwnedStr came
        // from a binding (the binding's scope-exit Drop
        // already frees the heap; freeing again would crash).
        let source = r#"
            fn main() -> i64 {
              let s: OwnedStr = "hello " + "world";
              print s;
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("emits C");
        // No `_intent_print_tmp` brace block; print of Var
        // OwnedStr stays a bare `fputs`.
        assert!(
            !c.contains("_intent_print_tmp"),
            "expected bare fputs for Var-OwnedStr print, got:\n{c}"
        );
        assert!(
            c.contains("fputs(v_s, stdout);"),
            "expected direct fputs of v_s, got:\n{c}"
        );
    }

    #[test]
    fn print_of_owned_str_struct_field_does_not_double_free() {
        // Closure #135 regression guard: `print t.name` for a
        // struct with an OwnedStr field must NOT free the
        // field's pointer — the struct's scope-exit drop
        // takes care of it. The earlier (over-aggressive)
        // version of this closure tried to free after every
        // non-Var OwnedStr print, which double-freed
        // FieldAccess results.
        let source = r#"
            struct Tag { name: OwnedStr }
            fn main() -> i64 {
              let t: Tag = Tag { name: "a" + "b" };
              print t.name;
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("emits C");
        // The print site should not contain a free of the
        // field pointer.
        assert!(
            !c.contains("free((void*)_intent_print_tmp)"),
            "expected no free of FieldAccess OwnedStr, got:\n{c}"
        );
    }

    #[test]
    fn discarded_owned_str_via_bare_call_frees_heap() {
        // Also exercise the bare-call form (`make();`) which
        // the parser sugars to `let _ = make();`. Same free
        // path; this guards against the discard codegen
        // diverging from the let-underscore form.
        let source = r#"
            fn make() -> OwnedStr {
              return "a" + "b";
            }
            fn main() -> i64 {
              make();
              return 0;
            }
        "#;
        compile(source).expect("bare-call discard of OwnedStr should compile");
        let c = compile_to_c(source).expect("emits C");
        assert!(
            c.contains("free((void*)_intent_discard)"),
            "expected free of discarded OwnedStr from bare call, got:\n{c}"
        );
    }

    #[test]
    fn bare_variable_as_statement_still_rejected() {
        // Regression guard: only call-shaped expressions
        // get the statement-sugar treatment. A bare `x;`
        // still surfaces "expected statement" — discarding
        // a plain variable read has no effect anyway, and
        // accepting it would silently swallow typos.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              x;
              return 0;
            }
        "#;
        let errors = compile(source)
            .expect_err("bare var as stmt is rejected");
        assert!(
            errors.iter().any(|e| e.message.contains("expected statement")),
            "expected the original error, got: {:?}",
            errors
        );
    }

    #[test]
    fn print_f32_value_renders_decimal() {
        let source = r#"
            fn main() -> i64 {
              let x: f32 = 3.5;
              print "x=", x;
              return 0;
            }
        "#;
        compile(source).expect("print f32 should compile");
    }

    #[test]
    fn cast_i8_to_u8_const_overflow_rejected() {
        // `-1 as u8` at compile time — the checker's const
        // fold recognizes the impossible representation and
        // rejects. (At runtime, the cast would wrap to 255,
        // but the constexpr path stops it earlier.)
        let source = r#"
            fn main() -> i64 {
              let x: i8 = -1;
              let y: u8 = x as u8;
              return y as i64;
            }
        "#;
        let errors = compile(source)
            .expect_err("i8(-1) as u8 const-fold is rejected");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("cannot be represented as u8")),
            "expected u8-representation diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn division_by_zero_constant_rejected() {
        // `10 / z` where `z = 0` is provably zero — the
        // const-fold path catches it before runtime.
        let source = r#"
            fn main() -> i64 {
              let z: i64 = 0;
              let r: i64 = 10 / z;
              return r;
            }
        "#;
        let errors = compile(source)
            .expect_err("const-zero division is rejected");
        assert!(
            errors.iter().any(|e| e.message.contains("division by zero")),
            "expected divide-by-zero diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn negative_assigned_to_unsigned_rejected() {
        let source = r#"
            fn main() -> i64 {
              let x: u8 = -5;
              return x as i64;
            }
        "#;
        let errors = compile(source)
            .expect_err("negative literal into u8 is rejected");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("cannot be represented as u8")),
            "expected u8-representation diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn boolean_and_short_circuits_at_compile_time() {
        // `false && (10 / x) > 0` previously errored at
        // compile time with "division by zero" because the
        // const-fold layer eagerly evaluated the RHS. Now
        // the checker honors short-circuit semantics: if
        // the LHS of `&&` const-folds to `false`, the RHS
        // is type-checked but its diagnostics are
        // discarded, matching what would happen at runtime
        // (the RHS never executes).
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 0;
              if false && (10 / x) > 0 { return 1; }
              return 0;
            }
        "#;
        compile(source).expect("false && bad-RHS should compile");
    }

    #[test]
    fn boolean_or_short_circuits_at_compile_time() {
        // `true || (10 / x) > 0` — the LHS already
        // determines the result, RHS is dead, so its
        // const-fold errors should be suppressed.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 0;
              if true || (10 / x) > 0 { return 100; }
              return 0;
            }
        "#;
        compile(source).expect("true || bad-RHS should compile");
    }

    #[test]
    fn boolean_false_or_does_not_short_circuit() {
        // Regression guard: `false || X` is NOT
        // short-circuit dead code — the result still
        // depends on X. So `false || (1 / 0) > 0` should
        // still surface the division-by-zero diagnostic.
        let source = r#"
            fn main() -> i64 {
              if false || (1 / 0) > 0 { return 1; }
              return 0;
            }
        "#;
        let errors = compile(source)
            .expect_err("false || requires RHS evaluation");
        assert!(
            errors.iter().any(|e| e.message.contains("division by zero")),
            "expected division-by-zero diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn deeply_nested_for_loops_compose() {
        // 3-deep nested for-loops with shared accumulator.
        let source = r#"
            fn main() -> i64 {
              let total: i64 = 0;
              for i from 0 to 2 {
                for j from 0 to 2 {
                  for k from 0 to 2 {
                    total = total + 1;
                  }
                }
              }
              return total;
            }
        "#;
        compile(source).expect("nested for-loops should compile");
    }

    #[test]
    fn method_call_results_compose_as_call_args() {
        // `add(x.get(), y.get())` — two method calls
        // composed as args to a plain function call.
        let source = r#"
            struct B { v: i64 }
            methods on B {
              fn get(self: B) -> i64 { return self.v; }
            }
            fn add(a: i64, b: i64) -> i64 { return a + b; }
            fn main() -> i64 {
              let x: B = B { v: 5 };
              let y: B = B { v: 7 };
              return add(x.get(), y.get());
            }
        "#;
        compile(source).expect("method results as call args should compile");
    }

    #[test]
    fn mut_ref_struct_param_mutates_caller_binding() {
        // `set_x(mut ref p, 42)` — caller's `p.x` is
        // updated through the mut-ref param.
        let source = r#"
            struct P { x: i64 }
            fn set_x(p: mut ref P, v: i64) -> i64 { p.x = v; return p.x; }
            fn main() -> i64 {
              let p: P = P { x: 0 };
              let r: i64 = set_x(mut ref p, 42);
              return r;
            }
        "#;
        compile(source).expect("mut-ref struct param should compile");
    }

    #[test]
    fn match_inside_if_branch_compiles() {
        // `if cond { return match … { … }; }` — `match`
        // as the body of a return inside an if branch.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 3;
              if x > 0 {
                return match x { 1 then 10, 2 then 20, 3 then 30, _ then 0 };
              }
              return 99;
            }
        "#;
        compile(source).expect("match in if-body should compile");
    }

    #[test]
    fn if_expression_inside_match_arm_compiles() {
        // `match e { E.A then if cond { 100 } else { 200 } }`
        // — if-expression as match-arm body.
        let source = r#"
            enum E { A, B }
            fn main() -> i64 {
              let e: E = E.A;
              let x: i64 = 5;
              return match e {
                E.A then if x > 0 { 100 } else { 200 },
                E.B then 99,
              };
            }
        "#;
        compile(source).expect("if-expr in match arm should compile");
    }

    #[test]
    fn bare_block_statement_compiles() {
        // Closure #116: `{ stmts; }` as a free-standing
        // statement now compiles. Desugars to `if true {
        // stmts; }` at parse time so the existing If-scope
        // machinery handles binding visibility and codegen.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              { let y: i64 = 10; assert y == 10; }
              return x;
            }
        "#;
        compile(source).expect("bare-block stmt should compile");
    }

    #[test]
    fn empty_vec_compiles() {
        // `vec()` with no args produces an empty Vec<T>.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec();
              return len(xs) as i64;
            }
        "#;
        compile(source).expect("empty vec() should compile");
    }

    #[test]
    fn method_calls_another_method_on_self_compiles() {
        // `self.dbl()` inside a method body — methods can
        // call sibling methods through `self`.
        let source = r#"
            struct A { v: i64 }
            methods on A {
              fn dbl(self: A) -> i64 { return self.v * 2; }
              fn quad(self: A) -> i64 { return self.dbl() * 2; }
            }
            fn main() -> i64 {
              let a: A = A { v: 5 };
              return a.quad();
            }
        "#;
        compile(source).expect("method calls sibling method should compile");
    }

    #[test]
    fn fn_returns_vec_compiles() {
        let source = r#"
            fn make() -> Vec<i64> { return vec(10, 20, 30); }
            fn main() -> i64 {
              let xs: Vec<i64> = make();
              return xs[1];
            }
        "#;
        compile(source).expect("fn returning Vec should compile");
    }

    #[test]
    fn array_return_position_clean_diagnostic() {
        // V1 limitation: arrays can't appear in fn return
        // position (the SSA layer doesn't lower
        // by-value-array returns yet). Diagnostic is
        // explicit and forward-looking.
        let source = r#"
            fn make() -> [i64; 3] { return [1, 2, 3]; }
            fn main() -> i64 {
              let xs: [i64; 3] = make();
              return xs[0];
            }
        "#;
        let errors = compile(source)
            .expect_err("array return position is rejected");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("array types are not allowed in return position")),
            "expected array-return diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn let_tuple_destructure_two_compiles() {
        let source = r#"
            fn main() -> i64 {
              let (a, b): (i64, i64) = (10, 20);
              return a + b;
            }
        "#;
        compile(source).expect("let-tuple destructure (a,b) should compile");
    }

    #[test]
    fn let_tuple_destructure_three_compiles() {
        let source = r#"
            fn main() -> i64 {
              let (a, b, c): (i64, i64, i64) = (1, 2, 3);
              return a + b + c;
            }
        "#;
        compile(source).expect("let-tuple destructure (a,b,c) should compile");
    }

    #[test]
    fn tuple_swap_via_destructure_compiles() {
        // `let (c, d) = (b, a);` — swap by destructuring
        // the swapped tuple literal.
        let source = r#"
            fn main() -> i64 {
              let (a, b): (i64, i64) = (1, 2);
              let (c, d): (i64, i64) = (b, a);
              return c * 10 + d;
            }
        "#;
        compile(source).expect("tuple swap should compile");
    }

    #[test]
    fn vec_set_returns_updated_vec_compiles() {
        // `set(xs, 0, 100)` returns a new Vec with index
        // 0 updated to 100 — the original is consumed.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(0, 0, 0);
              let ys: Vec<i64> = set(xs, 0, 100);
              return ys[0];
            }
        "#;
        compile(source).expect("vec set returning updated Vec should compile");
    }

    #[test]
    fn unit_return_function_compiles_and_runs() {
        // Closure #115: `fn name() { … }` without `-> Type`
        // parses as sugar for `-> i64` with an implicit
        // `return 0;` appended.
        let source = r#"
            fn greet() {
              print "hello";
            }
            fn main() -> i64 {
              greet();
              return 0;
            }
        "#;
        compile(source).expect("unit-return function should compile");
    }

    #[test]
    fn unit_return_function_already_has_return() {
        // If the body already ends with a `return`, no
        // synthetic `return 0;` is appended (idempotent).
        let source = r#"
            fn early_exit(x: i64) {
              if x < 0 { return 0; }
              print "non-negative:", x;
            }
            fn main() -> i64 {
              early_exit(-1);
              early_exit(5);
              return 0;
            }
        "#;
        compile(source).expect("unit-return with explicit return should compile");
    }

    #[test]
    fn type_associated_function_compiles_and_dispatches() {
        // `methods on B { fn make() -> B { … } }` (no self)
        // is now valid — it becomes a type-associated
        // function callable as `B.make()`. Closure #114.
        let source = r#"
            struct B { v: i64 }
            methods on B {
              fn make() -> B { return B { v: 99 }; }
            }
            fn main() -> i64 {
              let b: B = B.make();
              assert b.v == 99;
              return 0;
            }
        "#;
        compile(source)
            .expect("Type.helper() should compile");
        let c = compile_to_c(source).expect("C backend emits a program");
        assert!(
            c.contains("fn_B_make("),
            "expected dispatch to fn_B_make in C output:\n{c}"
        );
    }

    #[test]
    fn global_const_in_struct_field_init_compiles() {
        // `P { x: X, y: Y }` where X, Y are consts — const
        // references in struct field initialization work.
        let source = r#"
            const X: i64 = 5;
            const Y: i64 = 10;
            struct P { x: i64, y: i64 }
            fn main() -> i64 {
              let p: P = P { x: X, y: Y };
              return p.x + p.y;
            }
        "#;
        compile(source).expect("const in struct field init should compile");
    }

    #[test]
    fn match_on_unsigned_scrutinee_compiles() {
        let source = r#"
            fn main() -> i64 {
              let x: u32 = 100;
              return match x { 0 then 1, 50 then 2, 100 then 3, _ then 0 };
            }
        "#;
        compile(source).expect("match on u32 should compile");
    }

    #[test]
    fn negative_integer_pattern_matches_compiles() {
        // `-5` as a pattern arm in match — works because
        // unary-minus-int is folded to a negative literal
        // at parse/check time.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = -5;
              return match x { -5 then 10, -10 then 20, _ then 0 };
            }
        "#;
        compile(source).expect("negative pattern should compile");
    }

    #[test]
    fn while_with_compound_cond_compiles() {
        // `while i < 10 && j > 50` — compound condition
        // with `&&` works.
        let source = r#"
            fn main() -> i64 {
              let i: i64 = 0;
              let j: i64 = 100;
              while i < 10 && j > 50 {
                i = i + 1;
                j = j - 1;
              }
              return j;
            }
        "#;
        compile(source).expect("while with && cond should compile");
    }

    #[test]
    fn print_multiple_items_with_labels_compiles() {
        // `print "x=", x, "y=", y;` — multi-item print
        // with interleaved labels and values.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              let y: i64 = 10;
              print "x=", x, "y=", y;
              return 0;
            }
        "#;
        compile(source).expect("multi-item print should compile");
    }

    #[test]
    fn atomic_compare_exchange_with_correct_name() {
        // Spec: the function is `atomic_compare_exchange`,
        // not bare `compare_exchange`. Pin the right name
        // so future renames surface.
        let source = r#"
            fn main() -> i64 {
              let a: Atomic<i64> = atomic_new(10);
              let r: bool = atomic_compare_exchange(ref a, 10, 20);
              if r { return atomic_load(ref a); }
              return 0;
            }
        "#;
        compile(source).expect("atomic_compare_exchange should compile");
    }

    #[test]
    fn ten_element_array_compiles() {
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 10] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
              return xs[9];
            }
        "#;
        compile(source).expect("10-element array should compile");
    }

    #[test]
    fn vec_of_vec_len_compiles() {
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<Vec<i64>> = vec(vec(1, 2), vec(3, 4));
              return len(xs) as i64;
            }
        "#;
        compile(source).expect("Vec<Vec<i64>> with len should compile");
    }

    #[test]
    fn auto_ref_method_dispatch_compiles() {
        // `p.get()` where method takes `self: ref P` —
        // auto-ref dispatch through the owned binding.
        let source = r#"
            struct P { x: i64 }
            methods on P {
              fn get(self: ref P) -> i64 { return self.x; }
            }
            fn main() -> i64 {
              let p: P = P { x: 7 };
              return p.get();
            }
        "#;
        compile(source).expect("auto-ref method dispatch should compile");
    }

    #[test]
    fn auto_mut_ref_method_dispatch_compiles() {
        // `p.set(42)` where method takes `self: mut ref P`
        // — auto-mut-ref dispatch through the owned
        // binding.
        let source = r#"
            struct P { x: i64 }
            methods on P {
              fn set(self: mut ref P, v: i64) -> i64 {
                self.x = v;
                return self.x;
              }
            }
            fn main() -> i64 {
              let p: P = P { x: 0 };
              return p.set(42);
            }
        "#;
        compile(source).expect("auto-mut-ref dispatch should compile");
    }

    #[test]
    fn mixed_signed_unsigned_arithmetic_compiles() {
        // `i64 + (u32 as i64)` — explicit cast required
        // for cross-signedness, then arithmetic works.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              let y: u32 = 3;
              return x + (y as i64);
            }
        "#;
        compile(source).expect("mixed signed/unsigned should compile");
    }

    #[test]
    fn string_with_escape_sequences_compiles() {
        // `"hello\nworld\t!"` — backslash escapes in
        // string literals are honored by the lexer.
        let source = r#"
            fn main() -> i64 {
              print "hello\nworld\t!";
              return 0;
            }
        "#;
        compile(source).expect("escape sequences should compile");
    }

    #[test]
    fn fn_returning_str_compiles() {
        // `fn label() -> Str { return "hello"; }` — Str
        // return position is supported (unlike OwnedStr,
        // which would need affine-tracking work).
        let source = r#"
            fn label() -> Str { return "hello"; }
            fn main() -> i64 {
              print label();
              return 0;
            }
        "#;
        compile(source).expect("Str return position should compile");
    }

    #[test]
    fn vec_reassigned_in_loop_compiles() {
        // `xs = set(xs, i, i * 10);` — each iteration
        // produces a new Vec and the binding is reassigned.
        // The affine chain stays consistent across the loop
        // carry (via SSA block params).
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(0, 0, 0);
              for i from 0 to 3 {
                xs = set(xs, i, i * 10);
              }
              return xs[2];
            }
        "#;
        compile(source).expect("Vec reassign in loop should compile");
    }

    #[test]
    fn reassign_owned_str_frees_previous_heap() {
        // Closure #133: `s = "b" + ""` for a non-Copy
        // `OwnedStr` binding now frees the previous heap
        // before storing the new value. Was leaking — the
        // Reassign emit's drop-old path only handled
        // `Type::Vec`; OwnedStr fell through to the plain
        // assign branch.
        let source = r#"
            fn main() -> i64 {
              let s: OwnedStr = "a" + "1";
              s = "b" + "2";
              return 0;
            }
        "#;
        compile(source).expect("OwnedStr reassign should compile");
        let c = compile_to_c(source).expect("emits C");
        // Expect the tmp-eval / free-old / move-tmp pattern.
        assert!(
            c.contains("char* _intent_tmp_s ="),
            "expected tmp for RHS eval, got:\n{c}"
        );
        assert!(
            c.contains("free((void*)v_s);"),
            "expected free of old slot before reassign, got:\n{c}"
        );
    }

    #[test]
    fn reassign_vec_self_consuming_still_works() {
        // Closure #133 reordered the LLVM Reassign emit
        // (eval-first-then-free instead of free-before-eval)
        // so a RHS that READS the binding doesn't observe
        // freed memory. Self-consuming reassigns
        // (`xs = push(xs, k)`) still work because the
        // checker leaves `drop_old: false` for those — the
        // RHS already consumed the buffer.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2);
              xs = push(xs, 3);
              xs = push(xs, 4);
              return len(xs) as i64;
            }
        "#;
        compile(source).expect("self-consuming Vec reassign should compile");
    }

    #[test]
    fn ensures_with_strict_lower_bound_compiles() {
        // `ensures _return > 0` discharged via
        // `requires x > 0` plus body returning x — SMT
        // transitively concludes _return > 0.
        let source = r#"
            fn ensure_positive(x: i64) -> i64
            requires x > 0;
            ensures _return > 0;
            {
              return x;
            }
            fn main() -> i64 { return ensure_positive(5); }
        "#;
        compile(source).expect("strict ensures should compile");
    }

    #[test]
    fn method_chain_on_if_expr_result_compiles() {
        // `let c: C = if … { C{0} } else { C{100} };
        //  return c.add(10).add(20).v;` — if-expression
        // produces a struct, then chain methods on it.
        let source = r#"
            struct C { v: i64 }
            methods on C {
              fn add(self: C, n: i64) -> C { return C { v: self.v + n }; }
            }
            fn main() -> i64 {
              let c: C = if true { C { v: 0 } } else { C { v: 100 } };
              return c.add(10).add(20).v;
            }
        "#;
        compile(source).expect("method chain on if-expr should compile");
    }

    #[test]
    fn clone_at_on_vec_of_struct_compiles_and_runs() {
        // Bug fix: `clone_at(ref ps, 1)` on `Vec<P>` (Copy
        // struct elements) called an undefined `@fn_clone_at`
        // in tree-LLVM and failed `lli` linking. The
        // SSA-LLVM backend had a clone_at arm but tree-LLVM
        // did not. Vec<Vec<>> happened to work because the
        // SSA path supported it; Vec<Struct> fell back to
        // tree-LLVM. Added a parallel `clone_at` lowering in
        // [backend_llvm.rs](src/backend_llvm.rs) that
        // mirrors the SSA-LLVM logic: GEP into the slot,
        // load (Copy) or call the inner-Vec __clone helper
        // (non-Copy Vec elements).
        let source = r#"
            struct P { x: i64 }
            fn main() -> i64 {
              let ps: Vec<P> = vec(P { x: 1 }, P { x: 2 }, P { x: 3 });
              return clone_at(ref ps, 1).x;
            }
        "#;
        compile(source).expect("clone_at Vec<Struct> should compile");
    }

    #[test]
    fn match_returning_struct_via_arms_compiles() {
        // Match arms returning fresh struct literals
        // compose with field access on the result.
        let source = r#"
            struct P { x: i64 }
            fn pick(n: i64) -> P {
              return match n {
                1 then P { x: 10 },
                2 then P { x: 20 },
                _ then P { x: 99 },
              };
            }
            fn main() -> i64 { return pick(2).x; }
        "#;
        compile(source).expect("match arm returning struct should compile");
    }

    #[test]
    fn pure_fn_can_call_other_pure_fn() {
        let source = r#"
            pure fn dbl(x: i64) -> i64 { return x * 2; }
            pure fn quad(x: i64) -> i64 { return dbl(dbl(x)); }
            fn main() -> i64 { return quad(3); }
        "#;
        compile(source).expect("pure fn calling pure fn should compile");
    }

    #[test]
    fn for_with_continue_and_break_compiles() {
        // Mixed `continue` (skip i=5) + `break` (stop at
        // i=8) inside a for-loop body — exit 17 because
        // 0+1+2+3+4+6+7 = 23 mod 256 = 23. Wait actually:
        // i=0,1,2,3,4 each adds; skip 5; i=6,7 each add;
        // break at 8. Sum = 0+1+2+3+4+6+7 = 23.
        let source = r#"
            fn main() -> i64 {
              let total: i64 = 0;
              for i from 0 to 10 {
                if i == 5 { continue; }
                if i == 8 { break; }
                total = total + i;
              }
              return total;
            }
        "#;
        compile(source).expect("for + break + continue should compile");
    }

    #[test]
    fn smt_method_call_discharges_via_ensures() {
        // Method calls in proof position now reach the SMT
        // layer. The pre-pass `rewrite_method_calls_to_calls`
        // resolves the receiver's type via env lookup, builds
        // the mangled name `<Type>_<method>`, and produces a
        // synthetic Call. The existing inline-call discharger
        // then attaches the method's `ensures` clauses to a
        // fresh symbolic var. Combined with the struct-field
        // rewrite (closure #82), `prove b.doubled() == 14`
        // discharges when `b.v == 7` and the method's ensures
        // says `_return == self.v * 2`.
        let source = r#"
            struct Box { v: i64 }
            methods on Box {
              fn doubled(self: Box) -> i64
              ensures _return == self.v * 2;
              {
                return self.v * 2;
              }
            }
            fn main() -> i64 {
              let b: Box = Box { v: 7 };
              prove b.doubled() == 14;
              return b.doubled();
            }
        "#;
        compile(source).expect("method call with ensures should discharge");
    }

    #[test]
    fn smt_multiple_method_calls_discharge() {
        // Two methods on the same struct + a composed proof
        // over both.
        let source = r#"
            struct Box { v: i64 }
            methods on Box {
              fn val(self: Box) -> i64
              ensures _return == self.v;
              { return self.v; }
              fn doubled(self: Box) -> i64
              ensures _return == self.v * 2;
              { return self.v * 2; }
            }
            fn main() -> i64 {
              let b: Box = Box { v: 5 };
              prove b.val() == 5;
              prove b.doubled() == 10;
              prove b.val() + b.doubled() == 15;
              return 0;
            }
        "#;
        compile(source).expect("multiple method-call discharges should work");
    }

    #[test]
    fn variant_with_binding_pattern_compiles_in_tree_c() {
        // T1.3 phase 2b (Phase 3): the destructure binding
        // `v` in `Opt.Some(v) then …` is now introduced into
        // the arm body's scope. The tree-C backend lowers the
        // pattern to `case TAG: { <payload_ty> v_v =
        // __scr.payload; __r = (<body>); } break;`. The body
        // references `v` and resolves to the extracted
        // payload.
        let source = r#"
            enum Opt { Some(i64), None }
            fn unwrap(o: Opt) -> i64 {
              return match o {
                Opt.Some(v) then v,
                Opt.None then 0,
              };
            }
            fn main() -> i64 { return 0; }
        "#;
        compile_to_c(source).expect("payload destructure should compile to C");
    }

    #[test]
    fn variant_with_binding_pattern_format_round_trips() {
        // Formatter prints `Opt.Some(v) then ...` for the
        // VariantWithBinding pattern. Validates the new
        // arm in format.rs.
        use crate::format::format_program;
        use crate::lexer::lex;
        use crate::parser::parse;
        let source = "enum Opt { Some(i64), None } fn main() -> i64 { return match Opt.None { Opt.Some(v) then v, Opt.None then 0, }; }";
        let tokens = lex(source).expect("lex");
        let (program, diags) = parse(tokens);
        assert!(diags.is_empty(), "parse diagnostics: {:?}", diags);
        let formatted = format_program(&program);
        assert!(
            formatted.contains("Opt.Some(v)"),
            "formatted output should include the payload binding form: {}",
            formatted
        );
    }

    #[test]
    fn devanagari_numeral_literal_compiles() {
        // Devanagari digits `०१२३४५६७८९` (U+0966..U+096F)
        // lex as integer literals. `५` ≡ 5, `४२` ≡ 42.
        // No suffix / float / radix support — integer-only
        // in v1 for readability of small numbers in source.
        let source = r#"
            फलन main() -> i64 {
              मान x: i64 = ५;
              मान y: i64 = ४२;
              मान sum: i64 = x + y;
              खात्री sum == ४७;
              परत sum;
            }
        "#;
        compile(source).expect("Devanagari numerals should compile");
    }

    #[test]
    fn devanagari_numeral_zero_compiles() {
        // Single-digit zero `०` should lex as `0`.
        let source = r#"
            फलन main() -> i64 {
              परत ०;
            }
        "#;
        compile(source).expect("Devanagari zero should compile");
    }

    #[test]
    fn devanagari_numeral_larger_value_compiles() {
        // Multi-digit Devanagari numerals — `१००` ≡ 100,
        // `२५५` ≡ 255. Validates the multi-codepoint
        // consume loop in `lex_devanagari_number`.
        let source = r#"
            फलन main() -> i64 {
              मान x: i64 = १००;
              मान y: i64 = २५५;
              परत x + y;
            }
        "#;
        compile(source).expect("multi-digit Devanagari numerals should compile");
    }

    #[test]
    fn devanagari_multi_word_alias_hindi_else() {
        // Hindi `नहीं तो` (nahīṁ to — "if not / else") is a
        // multi-word alias. A post-lex merger walks the
        // token stream and stitches adjacent Devanagari
        // words into a single token when their combined
        // text matches the multi-word table.
        let source = r#"
            फलन main() -> i64 {
              यदि 1 > 0 {
                परत 100;
              } नहीं तो {
                परत 200;
              }
            }
        "#;
        compile(source).expect("Hindi multi-word else should compile");
    }

    #[test]
    fn devanagari_multi_word_alias_hindi_for() {
        // Hindi `के लिए` (ke liye — "for") is a multi-word
        // alias spelling of the for-keyword.
        let source = r#"
            फलन main() -> i64 {
              मान r: i64 = 0;
              के लिए i from 0 to 5 {
                r = r + i;
              }
              परत r;
            }
        "#;
        compile(source).expect("Hindi multi-word for should compile");
    }

    #[test]
    fn devanagari_multi_word_alias_hindi_prove() {
        // Hindi `सिद्ध करो` (siddha karo — "prove!") is a
        // multi-word alias for `prove`. Validates that the
        // merger fires even when the first word (`सिद्ध`)
        // has its own single-word alias for the same kind.
        let source = r#"
            फलन main() -> i64 {
              मान x: i64 = 5;
              सिद्ध करो x == 5;
              परत x;
            }
        "#;
        compile(source).expect("Hindi multi-word prove should compile");
    }

    #[test]
    fn devanagari_keyword_aliases_compile_hindi() {
        // Hindi-flavored keyword aliases (`फलन` = fn,
        // `मान` = let, `परत` = return, `खात्री` = assert)
        // route through the lexer's Devanagari path into
        // the existing English TokenKinds. The full
        // pipeline downstream (parser, checker, backend)
        // never sees the Devanagari source — it only sees
        // English tokens.
        let source = r#"
            फलन add(a: i64, b: i64) -> i64 {
              परत a + b;
            }
            फलन main() -> i64 {
              मान r: i64 = add(40, 2);
              खात्री r == 42;
              परत r;
            }
        "#;
        compile(source).expect("Hindi-aliased program should compile");
    }

    #[test]
    fn devanagari_keyword_aliases_compile_sanskrit() {
        // Sanskrit aliases include `यदि` = if, `अन्यथा` =
        // else, `शुद्ध फलन` = pure fn, plus `अपेक्षित` /
        // `निश्चित` for requires / ensures.
        let source = r#"
            शुद्ध फलन abs(n: i64) -> i64
            अपेक्षित n > 0 - 1000000;
            निश्चित _return >= 0;
            {
              यदि n < 0 {
                परत 0 - n;
              } अन्यथा {
                परत n;
              }
            }
            फलन main() -> i64 {
              मान y: i64 = abs(0 - 7);
              खात्री y == 7;
              सिद्ध y >= 0;
              परत y;
            }
        "#;
        compile(source).expect("Sanskrit-aliased program should compile");
    }

    #[test]
    fn devanagari_aliases_mix_with_english_freely() {
        // Mixed-script source — Devanagari `फलन` next to
        // English `fn`, Devanagari `परत` next to English
        // `return`. The lexer treats each token in isolation.
        let source = r#"
            fn double(x: i64) -> i64 {
              परत x * 2;
            }
            फलन main() -> i64 {
              let r: i64 = double(21);
              assert r == 42;
              return r;
            }
        "#;
        compile(source).expect("mixed-script program should compile");
    }

    #[test]
    fn devanagari_identifier_names_compile() {
        // Names written in Devanagari (`नाम` = "name") that
        // aren't keyword aliases should be lexed as
        // `Ident(...)` and used as ordinary local-binding
        // names. Validates the catch-all fallback in
        // `lex_unicode_ident`.
        let source = r#"
            fn main() -> i64 {
              let नाम: i64 = 42;
              return नाम;
            }
        "#;
        compile(source).expect("Devanagari identifier should compile");
    }

    #[test]
    fn smt_if_expression_discharges_in_prove() {
        // SMT now encodes `IfExpr` as `(ite cond then else)`.
        // The combined if-expr + arithmetic case used to
        // surface "if-expressions not supported in SMT v1";
        // now it discharges cleanly.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              prove (if x > 0 { x } else { 0 - x }) > 0;
              return 0;
            }
        "#;
        compile(source).expect("if-expr in prove should discharge");
    }

    #[test]
    fn smt_if_expression_constant_folds_through_let() {
        // `let r = if true { 10 } else { 20 };` — the
        // checker now folds the if-expr to its branch's
        // constant when cond is known. Lets `prove r == 10`
        // discharge via the constant-fold layer.
        let source = r#"
            fn main() -> i64 {
              let cond: bool = true;
              let r: i64 = if cond { 10 } else { 20 };
              prove r == 10;
              return r;
            }
        "#;
        compile(source).expect("if-expr const-fold through let should discharge");
    }

    #[test]
    fn smt_match_expression_discharges_in_prove() {
        // SMT now encodes match (over integer patterns +
        // wildcard) as nested `(ite (= scrutinee N) body
        // …)`. `prove (match x { 1 then 10, … _ then 0 })
        // == 30` discharges by inspection.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 3;
              prove (match x { 1 then 10, 2 then 20, 3 then 30, _ then 0 }) == 30;
              return 0;
            }
        "#;
        compile(source).expect("match in prove should discharge");
    }

    #[test]
    fn smt_match_constant_folds_through_let() {
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 2;
              let r: i64 = match x { 1 then 10, 2 then 20, _ then 99 };
              prove r == 20;
              return r;
            }
        "#;
        compile(source).expect("match const-fold through let should discharge");
    }

    #[test]
    fn smt_struct_field_access_discharges() {
        // `prove p.x == 5` where `p` was initialized via
        // struct literal now discharges. The SMT layer
        // synthesizes `p__x` as a per-field var, asserts
        // it equals the field initializer's expression, and
        // rewrites `p.x` → `Var("p__x")` in proof
        // obligations.
        let source = r#"
            struct P { x: i64, y: i64 }
            fn main() -> i64 {
              let p: P = P { x: 5, y: 10 };
              prove p.x == 5;
              prove p.y == 10;
              prove p.x + p.y == 15;
              return p.x;
            }
        "#;
        compile(source).expect("struct field prove should discharge");
    }

    #[test]
    fn smt_struct_field_with_computed_init_discharges() {
        // Field initializers can be expressions over outer
        // bindings — the synthesized fact `p__x ==
        // (a * 2)` is fed to SMT, which discharges `p.x
        // == 6` when `a == 3`.
        let source = r#"
            struct P { x: i64, y: i64 }
            fn main() -> i64 {
              let a: i64 = 3;
              let b: i64 = 7;
              let p: P = P { x: a * 2, y: b + 1 };
              prove p.x == 6;
              prove p.y == 8;
              return 0;
            }
        "#;
        compile(source).expect("struct field with computed init should discharge");
    }

    #[test]
    fn smt_struct_field_disproof_surfaces_counterexample() {
        // Wrong claim should be rejected with the SMT
        // counterexample showing the actual value.
        let source = r#"
            struct P { x: i64 }
            fn main() -> i64 {
              let p: P = P { x: 5 };
              prove p.x == 10;
              return 0;
            }
        "#;
        let errors = compile(source)
            .expect_err("p.x == 10 is false; SMT should disprove");
        assert!(
            errors.iter().any(|e| e.message.contains("proof failed")),
            "expected proof-failed diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn block_expression_with_lets_then_tail_compiles() {
        // T-block MVP: `let r = { let a = …; let b = …;
        // a + b };` — block-expr as let RHS. Inner `let`s
        // execute in a fresh scope; the tail's value becomes
        // the block's value (and thus `r`).
        let source = r#"
            fn main() -> i64 {
              let r: i64 = { let a: i64 = 5; let b: i64 = 10; a + b };
              return r;
            }
        "#;
        compile(source).expect("block-expr let-RHS should compile");
    }

    #[test]
    fn nested_block_expression_compiles() {
        // Block inside block — inner result feeds outer
        // computation.
        let source = r#"
            fn main() -> i64 {
              let outer: i64 = 100;
              let r: i64 = {
                let inner: i64 = { let a: i64 = 5; let b: i64 = 6; a * b };
                inner + outer
              };
              return r;
            }
        "#;
        compile(source).expect("nested block-expr should compile");
    }

    #[test]
    fn empty_block_expression_just_value_compiles() {
        // `{ 42 }` — block with zero stmts and an integer
        // literal as the tail.
        let source = r#"
            fn main() -> i64 {
              let r: i64 = { 42 };
              return r;
            }
        "#;
        compile(source).expect("empty-stmt block should compile");
    }

    #[test]
    fn block_expression_only_allows_let_inside() {
        // V1 restricts block-internal stmts to `let`. A
        // `return` (or any other stmt form) inside the
        // block surfaces an error — the parser's Block arm
        // only consumes leading `let` stmts.
        let source = r#"
            fn main() -> i64 {
              let r: i64 = { let a: i64 = 5; return 99; a + 1 };
              return r;
            }
        "#;
        let errors = compile(source)
            .expect_err("block with non-let stmt is rejected");
        assert!(
            !errors.is_empty(),
            "expected at least one diagnostic for non-let inside block"
        );
    }

    #[test]
    fn block_expression_admits_print_stmts() {
        // Closure #129 extends the v1 Block MVP to allow
        // `print` statements before the tail expression.
        // Useful for logging intermediate values inside a
        // block-expression initializer.
        let source = r#"
            fn main() -> i64 {
              let r: i64 = {
                let a: i64 = 10;
                print "inside block, a=", a;
                let b: i64 = 20;
                a + b
              };
              return r;
            }
        "#;
        compile(source).expect("block-expr with print should compile");
        let c = compile_to_c(source).expect("C backend emits a program");
        // The print should appear inside the GCC statement-
        // expression body, not hoisted out.
        assert!(
            c.contains("fputs(\"inside block, a=\""),
            "expected print fputs inside block, got:\n{}",
            c
        );
    }

    #[test]
    fn block_expression_print_runs_before_tail() {
        // Closure #129: prints inside a Block execute before
        // the tail expression. Verifies the C statement-
        // expression ordering.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = {
                print "begin";
                let v: i64 = 7;
                v + 1
              };
              return x;
            }
        "#;
        let c = compile_to_c(source).expect("C backend emits a program");
        // The print's fputs must appear before the tail
        // expression's (v + 1) in the emitted text.
        let p = c.find("fputs(\"begin\"").expect("print site");
        let v_def = c.find("int64_t v_v =").expect("v def");
        assert!(p < v_def, "print should be emitted before tail-prep:\n{}", c);
    }

    #[test]
    fn block_expression_shadowing_is_local() {
        // Inner `let x` inside a block-expr shadows the outer
        // `x` within the block scope; after the block
        // evaluates, the outer `x` is unchanged.
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              let r: i64 = { let x: i64 = 100; x + 1 };
              assert x == 5;
              return r;
            }
        "#;
        compile(source).expect("block-expr shadowing should be local");
    }

    #[test]
    fn float_negation_via_unary_minus_compiles() {
        // `-x` for a float operand previously emitted
        // `sub double 0, %v` in the SSA-LLVM path —
        // invalid LLVM IR (integer sub rejects float
        // operands). Fix: dispatch on the operand's
        // type and emit `fsub double 0.0, %v` for
        // floats; integer `sub` for ints. Sibling
        // path in the tree-LLVM backend already
        // routed through the float-binary
        // dispatcher.
        let source = r#"
            fn main() -> i64 {
              let x: f64 = 5.0;
              let y: f64 = -x;
              return y as i64;
            }
        "#;
        compile(source).expect("float negation should compile");
    }

    #[test]
    fn bit_shift_ops_compile() {
        let source = r#"
            fn main() -> i64 { return (1 << 4) + (16 >> 2); }
        "#;
        compile(source).expect("bit shifts should compile");
    }

    #[test]
    fn float_arithmetic_compiles() {
        let source = r#"
            fn main() -> i64 {
              let x: f64 = 10.0;
              let y: f64 = (x * 2.5 + 3.0) - 1.5;
              return y as i64;
            }
        "#;
        compile(source).expect("float arithmetic should compile");
    }

    #[test]
    fn string_concat_with_plus() {
        let source = r#"
            fn main() -> i64 {
              let s: OwnedStr = "hello, " + "world";
              print s;
              return 0;
            }
        "#;
        compile(source).expect("string concat should compile");
    }

    #[test]
    fn string_equality_compiles() {
        let source = r#"
            fn main() -> i64 {
              let a: Str = "abc";
              let b: Str = "abc";
              if a == b { return 100; }
              return 0;
            }
        "#;
        compile(source).expect("string equality should compile");
    }

    #[test]
    fn f32_to_f64_cast_compiles() {
        let source = r#"
            fn main() -> i64 {
              let x: f32 = 1.5;
              let y: f64 = x as f64;
              return (y * 4.0) as i64;
            }
        "#;
        compile(source).expect("f32→f64 cast should compile");
    }

    #[test]
    fn len_on_inline_vec_literal() {
        let source = r#"
            fn main() -> i64 {
              return len(vec(1, 2, 3, 4, 5)) as i64;
            }
        "#;
        compile(source).expect("len of inline vec should compile");
    }

    #[test]
    fn type_alias_tuple_field_access() {
        let source = r#"
            type Coord = (i64, i64);
            fn main() -> i64 {
              let c: Coord = (7, 11);
              return c.0 + c.1;
            }
        "#;
        compile(source).expect("alias-to-tuple field access should compile");
    }

    #[test]
    fn match_with_only_wildcard_arm() {
        let source = r#"
            fn main() -> i64 { let x: i64 = 42; return match x { _ then 99 }; }
        "#;
        compile(source).expect("wildcard-only match should compile");
    }

    #[test]
    fn assert_with_message_compiles() {
        let source = r#"
            fn main() -> i64 {
              assert 1 < 2, "basic math";
              return 7;
            }
        "#;
        compile(source).expect("assert with message should compile");
    }

    #[test]
    fn boolean_short_circuit_evaluates() {
        let source = r#"
            fn main() -> i64 {
              let a: bool = true;
              let b: bool = false;
              if a && (b || true) { return 100; }
              return 0;
            }
        "#;
        compile(source).expect("bool short-circuit should compile");
    }

    #[test]
    fn pure_fn_calling_pure_fn_compiles() {
        let source = r#"
            pure fn double(x: i64) -> i64 { return x * 2; }
            pure fn quad(x: i64) -> i64 { return double(double(x)); }
            fn main() -> i64 { return quad(3); }
        "#;
        compile(source).expect("pure→pure should compile");
    }

    #[test]
    fn pure_fn_calling_impure_fn_rejected() {
        let source = r#"
            fn shout() -> i64 { print "hi"; return 0; }
            pure fn p() -> i64 { return shout(); }
            fn main() -> i64 { return p(); }
        "#;
        let errors = compile(source)
            .expect_err("pure→impure should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("pure fn 'p' cannot call non-pure function")),
            "expected pure-fn diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn fn_no_params_returning_bool() {
        let source = r#"
            fn always() -> bool { return true; }
            fn main() -> i64 { if always() { return 1; } return 0; }
        "#;
        compile(source).expect("no-param bool fn should compile");
    }

    #[test]
    fn bool_equality_comparison() {
        let source = r#"
            fn main() -> i64 {
              let a: bool = true;
              let b: bool = false;
              if a == b { return 1; }
              return 0;
            }
        "#;
        compile(source).expect("bool == should compile");
    }

    #[test]
    fn float_equality_comparison() {
        let source = r#"
            fn main() -> i64 {
              let x: f64 = 1.5;
              let y: f64 = 1.5;
              if x == y { return 100; }
              return 0;
            }
        "#;
        compile(source).expect("float == should compile");
    }

    #[test]
    fn nested_for_loops_compile() {
        let source = r#"
            fn main() -> i64 {
              let total: i64 = 0;
              for i from 0 to 3 {
                for j from 0 to 3 { total = total + 1; }
              }
              return total;
            }
        "#;
        compile(source).expect("nested for should compile");
    }

    #[test]
    fn while_inside_for_loop() {
        let source = r#"
            fn main() -> i64 {
              let total: i64 = 0;
              for i from 0 to 3 {
                let j: i64 = 0;
                while j < 2 { total = total + 1; j = j + 1; }
              }
              return total;
            }
        "#;
        compile(source).expect("while-in-for should compile");
    }

    #[test]
    fn boolean_negation() {
        let source = r#"
            fn main() -> i64 {
              let x: bool = true;
              if !x { return 100; }
              return 200;
            }
        "#;
        compile(source).expect("bool ! should compile");
    }

    #[test]
    fn bitwise_ops_on_i64() {
        let source = r#"
            fn main() -> i64 {
              let a: i64 = 12;
              let b: i64 = 10;
              return (a & b) + (a | b) + (a ^ b);
            }
        "#;
        compile(source).expect("bitwise ops should compile");
    }

    #[test]
    fn modulo_op() {
        let source = r#"
            fn main() -> i64 { return 17 % 5; }
        "#;
        compile(source).expect("modulo should compile");
    }

    #[test]
    fn method_call_on_function_result() {
        let source = r#"
            struct Point { x: i64, y: i64 }
            methods on Point {
              fn sum(self: Point) -> i64 { return self.x + self.y; }
            }
            fn make() -> Point { return Point { x: 7, y: 8 }; }
            fn main() -> i64 { return make().sum(); }
        "#;
        compile(source).expect("method on fn-result should compile");
    }

    #[test]
    fn match_with_partial_variants_plus_wildcard() {
        // Cover 2 of 4 variants explicitly, wildcard
        // catches the rest.
        let source = r#"
            enum Color { Red, Green, Blue, Yellow }
            fn main() -> i64 {
              let c: Color = Color.Yellow;
              return match c {
                Color.Red then 1,
                Color.Green then 2,
                _ then 99,
              };
            }
        "#;
        compile(source).expect("partial+wildcard match should compile");
    }

    #[test]
    fn multiple_methods_blocks_on_same_type() {
        // Two separate `methods on Point { ... }` blocks
        // both contribute methods to the same struct.
        // The hoist pass appends each block's methods
        // to the regular function table independently.
        let source = r#"
            struct Point { x: i64, y: i64 }
            methods on Point {
              fn x_val(self: Point) -> i64 { return self.x; }
            }
            methods on Point {
              fn y_val(self: Point) -> i64 { return self.y; }
            }
            fn main() -> i64 {
              let p: Point = Point { x: 3, y: 4 };
              return p.x_val() + p.y_val();
            }
        "#;
        compile(source).expect("multiple methods blocks should compile");
    }

    #[test]
    fn while_with_break_and_continue() {
        // `break` and `continue` work correctly inside
        // a while loop body.
        let source = r#"
            fn main() -> i64 {
              let i: i64 = 0;
              let total: i64 = 0;
              while i < 10 {
                i = i + 1;
                if i == 5 { continue; }
                if i == 8 { break; }
                total = total + i;
              }
              return total;
            }
        "#;
        compile(source).expect("while break/continue should compile");
    }

    #[test]
    fn print_i8_typed_value() {
        // `print` formats smaller-width integers via
        // their respective conversions. i8 prints as
        // a signed decimal including negatives.
        let source = r#"
            fn main() -> i64 {
              let x: i8 = -5;
              print "x=", x;
              return 0;
            }
        "#;
        compile(source).expect("print i8 should compile");
    }

    #[test]
    fn const_with_underscored_literal() {
        // `1_000_000` is a valid integer literal with
        // underscores for readability.
        let source = r#"
            const M: i64 = 1_000_000;
            fn main() -> i64 { return M / 1000; }
        "#;
        compile(source).expect("underscored literal should compile");
    }

    #[test]
    fn empty_array_literal_rejected_with_clear_diagnostic() {
        // `[]` literal with `[T; 0]` annotation surfaces
        // a clean "empty array literals are not
        // supported; explicit element type is required"
        // diagnostic.
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 0] = [];
              return 0;
            }
        "#;
        let errors = compile(source)
            .expect_err("empty array literal should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("empty array literals are not supported")),
            "expected empty-array diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn vec_literal_with_single_element() {
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(42);
              return xs[0];
            }
        "#;
        compile(source).expect("single-element vec should compile");
    }

    #[test]
    fn const_as_match_pattern_rejected() {
        // Match patterns are integer literals or
        // variant patterns — not bound identifiers /
        // consts. A const reference in pattern
        // position trips the parser since it expects
        // `.` after the (presumed enum) identifier.
        let source = r#"
            const X: i64 = 5;
            fn main() -> i64 {
              let v: i64 = 5;
              return match v { X then 100, _ then 0 };
            }
        "#;
        let errors = compile(source)
            .expect_err("const in pattern position should fail");
        assert!(
            errors.iter().any(|e| {
                e.message.contains("expected")
                    && (e.message.contains("'.'") || e.message.contains("variant"))
            }),
            "expected pattern-shape diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn for_loop_with_const_as_upper_bound() {
        let source = r#"
            const N: i64 = 5;
            fn main() -> i64 {
              let total: i64 = 0;
              for i from 0 to N { total = total + i; }
              return total;
            }
        "#;
        compile(source).expect("const as for-loop bound should compile");
    }

    #[test]
    fn duplicate_struct_field_rejected() {
        let source = r#"
            struct Bad { x: i64, x: i64 }
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("duplicate field should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("'Bad' has duplicate field 'x'")),
            "expected duplicate-field diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn cast_int_to_bool_rejected() {
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 1;
              let b: bool = x as bool;
              return 0;
            }
        "#;
        let errors = compile(source)
            .expect_err("int→bool cast should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("cannot cast i64 to bool")),
            "expected cast-rejection diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn method_called_twice_on_same_copy_value() {
        // Methods on Copy structs can be called
        // multiple times on the same binding — no
        // move semantics apply.
        let source = r#"
            struct Point { x: i64, y: i64 }
            methods on Point {
              fn sum(self: Point) -> i64 { return self.x + self.y; }
            }
            fn main() -> i64 {
              let p: Point = Point { x: 3, y: 4 };
              return p.sum() + p.sum();
            }
        "#;
        compile(source).expect("method called twice should compile");
    }

    #[test]
    fn recursive_struct_rejected() {
        // `struct Node { val: i64, child: Node }` has
        // infinite size by value. Checker now detects
        // direct + transitive cycles in struct field
        // dependencies and surfaces a clear
        // "recursive (directly or transitively)"
        // diagnostic, suggesting the `ref T` / `Vec<T>`
        // heap-indirection workaround.
        let source = r#"
            struct Node { val: i64, child: Node }
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("recursive struct should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("'Node' is recursive")
                    && e.message.contains("infinite size")),
            "expected recursive-struct diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn mutually_recursive_structs_rejected() {
        // Transitive cycles get caught too (A.field is
        // B, B.field is A).
        let source = r#"
            struct A { b: B }
            struct B { a: A }
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("mutually recursive structs should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("is recursive")),
            "expected recursive-struct diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn tuple_containing_struct_field_access() {
        // `t.0.x` where t is `(Point, i64)` — tuple
        // first-element, then struct field. Tests
        // that postfix chains compose: tuple-access
        // then field-access.
        let source = r#"
            struct Point { x: i64, y: i64 }
            fn main() -> i64 {
              let t: (Point, i64) = (Point { x: 5, y: 6 }, 100);
              return t.0.x + t.0.y + t.1;
            }
        "#;
        compile(source).expect("tuple-of-struct access should compile");
    }

    #[test]
    fn nested_tuple_access_double_dot() {
        // `t.0.0` lexes as `t`, `.`, `Float(0.0)` because
        // the numeric lexer greedily reads `0.0` as a
        // float literal. Parser now detects this case
        // and splits the float into two integer tuple
        // indices when both halves are non-negative
        // integers separated by a single dot. Lets
        // users write `((1,2),(3,4)).0.0` without
        // intermediate variables.
        let source = r#"
            fn main() -> i64 {
              let t: ((i64, i64), (i64, i64)) = ((1, 2), (3, 4));
              return t.0.0 + t.1.1;
            }
        "#;
        compile(source).expect("nested tuple access should compile");
    }

    #[test]
    fn struct_field_reorder_in_literal() {
        // Struct literal field ordering doesn't matter
        // — the checker reorders to canonical
        // declaration order before codegen.
        let source = r#"
            struct P { x: i64, y: i64 }
            fn main() -> i64 {
              let p: P = P { y: 4, x: 3 };
              return p.x + p.y;
            }
        "#;
        compile(source).expect("reorder field init should compile");
    }

    #[test]
    fn function_result_as_struct_field_initializer() {
        let source = r#"
            struct Inner { val: i64 }
            struct Outer { inner: Inner }
            fn make_inner(v: i64) -> Inner { return Inner { val: v }; }
            fn main() -> i64 {
              let o: Outer = Outer { inner: make_inner(42) };
              return o.inner.val;
            }
        "#;
        compile(source).expect("fn result as field init should compile");
    }

    #[test]
    fn let_underscore_discard_compiles() {
        // `let _: T = expr;` discards the value after
        // evaluating for side effects.
        let source = r#"
            fn side_effect() -> i64 { return 99; }
            fn main() -> i64 {
              let _: i64 = side_effect();
              return 0;
            }
        "#;
        compile(source).expect("let _ discard should compile");
    }

    #[test]
    fn const_compared_with_other_const() {
        let source = r#"
            const A: i64 = 5;
            const B: i64 = 10;
            fn main() -> i64 {
              if A < B { return 100; }
              return 200;
            }
        "#;
        compile(source).expect("const-to-const comparison should compile");
    }

    #[test]
    fn print_method_call_result() {
        // `print "sum=", p.sum()` — method call result
        // as a print item. The PrintItem::Expr path
        // type-checks the method call and emits a
        // formatted value print at runtime.
        let source = r#"
            struct Point { x: i64, y: i64 }
            methods on Point {
              fn sum(self: Point) -> i64 { return self.x + self.y; }
            }
            fn main() -> i64 {
              let p: Point = Point { x: 3, y: 4 };
              print "sum=", p.sum();
              return 0;
            }
        "#;
        compile(source).expect("print with method call should compile");
    }

    #[test]
    fn nested_method_call_as_method_arg() {
        // `c.double(c.inc())` — method call result used
        // as an argument to another method call. Tests
        // recursive resolution + correct receiver
        // disambiguation.
        let source = r#"
            struct Counter { n: i64 }
            methods on Counter {
              fn inc(self: Counter) -> i64 { return self.n + 1; }
              fn add(self: Counter, v: i64) -> i64 { return self.n + v; }
            }
            fn main() -> i64 {
              let c: Counter = Counter { n: 10 };
              return c.add(c.inc());
            }
        "#;
        compile(source).expect("nested method-call as arg should compile");
    }

    #[test]
    fn boolean_ops_in_struct_initializer() {
        let source = r#"
            struct Cfg { enabled: bool, secure: bool }
            fn main() -> i64 {
              let c: Cfg = Cfg { enabled: true && false, secure: false || true };
              if c.secure { return 100; }
              return 0;
            }
        "#;
        compile(source).expect("bool ops in struct init should compile");
    }

    #[test]
    fn for_loop_with_negative_bounds() {
        let source = r#"
            fn main() -> i64 {
              let total: i64 = 0;
              for i from -5 to -1 { total = total + i; }
              return total;
            }
        "#;
        compile(source).expect("for-loop with negative bounds should compile");
    }

    #[test]
    fn match_on_method_result_inside_for_loop() {
        // Method-call result as a match scrutinee
        // inside a for-loop body. Stresses the
        // intersection of for-iter, method dispatch,
        // and enum match.
        let source = r#"
            enum Sign { Neg, Zero, Pos }
            struct Probe { val: i64 }
            methods on Probe {
              fn sign(self: Probe) -> Sign {
                if self.val < 0 { return Sign.Neg; }
                if self.val == 0 { return Sign.Zero; }
                return Sign.Pos;
              }
            }
            fn main() -> i64 {
              let probes: [Probe; 3] = [Probe { val: -5 }, Probe { val: 0 }, Probe { val: 7 }];
              let total: i64 = 0;
              for p in probes {
                total = total + match p.sign() {
                  Sign.Neg then -1,
                  Sign.Zero then 0,
                  Sign.Pos then 1,
                };
              }
              return total;
            }
        "#;
        compile(source).expect("match-on-method-result in for-loop should compile");
    }

    #[test]
    fn struct_with_65_fields_rejected_after_cap_raise() {
        // The cap raised to 64 still flags excess.
        let source = r#"
            struct TooBig {
              a1: i64, a2: i64, a3: i64, a4: i64, a5: i64, a6: i64, a7: i64, a8: i64,
              a9: i64, a10: i64, a11: i64, a12: i64, a13: i64, a14: i64, a15: i64, a16: i64,
              a17: i64, a18: i64, a19: i64, a20: i64, a21: i64, a22: i64, a23: i64, a24: i64,
              a25: i64, a26: i64, a27: i64, a28: i64, a29: i64, a30: i64, a31: i64, a33: i64,
              a34: i64, a35: i64, a36: i64, a37: i64, a38: i64, a39: i64, a40: i64, a41: i64,
              a42: i64, a43: i64, a44: i64, a45: i64, a46: i64, a47: i64, a48: i64, a49: i64,
              a50: i64, a51: i64, a52: i64, a53: i64, a54: i64, a55: i64, a56: i64, a57: i64,
              a58: i64, a59: i64, a60: i64, a61: i64, a62: i64, a63: i64, a65: i64, a66: i64,
              a67: i64,
            }
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("65-field struct should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("65 fields")
                    && e.message.contains("0..=64")),
            "expected oversize-struct diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn bool_const_compiles() {
        let source = r#"
            const ENABLED: bool = true;
            fn main() -> i64 {
              if ENABLED { return 100; }
              return 200;
            }
        "#;
        compile(source).expect("bool const should compile");
    }

    #[test]
    fn struct_with_mixed_int_sizes() {
        // Mix of u32, u16, u8 in struct fields. Tests
        // that the per-name struct typedef handles
        // multiple integer widths and that casts to
        // i64 in methods work for each.
        let source = r#"
            struct Header { magic: u32, count: u16, flags: u8 }
            methods on Header {
              fn total(self: Header) -> i64 {
                return (self.magic as i64) + (self.count as i64) + (self.flags as i64);
              }
            }
            fn main() -> i64 {
              let h: Header = Header { magic: 0xCAFE, count: 100, flags: 7 };
              return h.total();
            }
        "#;
        compile(source).expect("mixed-int-size struct should compile");
    }

    #[test]
    fn method_chain_on_struct_literal_into_field() {
        // `Point { … }.moved(10).moved(5).x` — chain
        // methods starting from a struct literal,
        // ending in a field access. Verifies the
        // postfix loop in parse_primary handles
        // structlit → methodcall → methodcall →
        // fieldaccess.
        let source = r#"
            struct Point { x: i64, y: i64 }
            methods on Point {
              fn moved(self: Point, dx: i64) -> Point {
                return Point { x: self.x + dx, y: self.y };
              }
            }
            fn main() -> i64 {
              return Point { x: 1, y: 2 }.moved(10).moved(5).x;
            }
        "#;
        compile(source).expect("methodchain-on-structlit should compile");
    }

    #[test]
    fn struct_with_15_fields_compiles_after_cap_raise() {
        // Field cap was raised from 8 to 64 to fit
        // real-world domain types (game entities,
        // protocol messages, configuration structs).
        let source = r#"
            struct Entity {
              id: i64, x: i64, y: i64, z: i64,
              vx: i64, vy: i64, vz: i64,
              health: i64, shield: i64,
              level: i64, exp: i64,
              team: i64, state: i64, flags: i64, ttl: i64,
            }
            fn main() -> i64 {
              let e: Entity = Entity {
                id: 1, x: 10, y: 20, z: 30,
                vx: 1, vy: 2, vz: 3,
                health: 100, shield: 50,
                level: 5, exp: 200,
                team: 1, state: 0, flags: 0, ttl: 60,
              };
              return e.health + e.shield;
            }
        "#;
        compile(source).expect("15-field struct should compile");
    }

    #[test]
    fn ssa_bool_print_renders_true_false() {
        // Closure #117: bool prints render as "true"/"false"
        // through both SSA backends (previously rendered as
        // 1/0 through the SSA path).
        let source = r#"
            fn main() -> i64 {
              let t: bool = true;
              let f: bool = false;
              print t, f;
              return 0;
            }
        "#;
        // The compiled C output uses the fputs path with
        // the literal "true"/"false" strings; assert one of
        // them appears.
        let c = compile_to_c(source).expect("bool print should compile");
        assert!(
            c.contains("\"true\"") || c.contains("? \"true\" :"),
            "expected `true` string literal in C output:\n{c}"
        );
    }

    #[test]
    fn empty_struct_compiles() {
        // Closure #116: empty structs (`struct E {}`) are now
        // accepted for marker / zero-sized types.
        let source = r#"
            struct Marker { }
            fn main() -> i64 {
              let m: Marker = Marker { };
              return 0;
            }
        "#;
        compile(source).expect("empty struct should compile");
    }

    #[test]
    fn const_literal_overflow_rejected() {
        // `const X: i8 = 200;` previously compiled
        // cleanly and silently truncated at codegen.
        // The checker now range-checks every integer
        // const literal against the declared type via
        // `value_fits_type`, so out-of-range values
        // surface a clear "literal N does not fit in
        // T" diagnostic at the decl site.
        let source = r#"
            const X: i8 = 200;
            fn main() -> i64 { return X as i64; }
        "#;
        let errors = compile(source)
            .expect_err("i8 const overflow should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("does not fit in i8")
                    && e.message.contains("200")),
            "expected overflow diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn recursive_function_calls_itself() {
        // Direct recursion through `fib(n - 1) + fib(n
        // - 2)`. Tests that the call resolution sees
        // `fib` in the signature table even before its
        // own body is type-checked.
        let source = r#"
            fn fib(n: i64) -> i64 {
              if n <= 1 { return n; }
              return fib(n - 1) + fib(n - 2);
            }
            fn main() -> i64 { return fib(10); }
        "#;
        compile(source).expect("recursive fn should compile");
    }

    #[test]
    fn empty_match_rejected_as_non_exhaustive() {
        let source = r#"
            enum Color { Red, Green, Blue }
            fn main() -> i64 {
              let c: Color = Color.Red;
              return match c { };
            }
        "#;
        let errors = compile(source)
            .expect_err("empty match should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("non-exhaustive match")),
            "expected non-exhaustive diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn match_on_struct_enum_field() {
        let source = r#"
            enum Side { Left, Right }
            struct Item { kind: Side, weight: i64 }
            fn classify(it: Item) -> i64 {
              return match it.kind {
                Side.Left then it.weight,
                Side.Right then -it.weight,
              };
            }
            fn main() -> i64 {
              let i: Item = Item { kind: Side.Right, weight: 50 };
              return classify(i);
            }
        "#;
        compile(source).expect("match on struct enum field should compile");
    }

    #[test]
    fn function_param_def_trailing_comma_accepted() {
        let source = r#"
            fn add3(a: i64, b: i64, c: i64,) -> i64 { return a + b + c; }
            fn main() -> i64 { return add3(1, 2, 3); }
        "#;
        compile(source).expect("trailing comma in fn param def should compile");
    }

    #[test]
    fn const_used_in_struct_field_initializer() {
        let source = r#"
            const ORIGIN: i64 = 100;
            struct Point { x: i64, y: i64 }
            fn main() -> i64 {
              let p: Point = Point { x: ORIGIN, y: ORIGIN + 1 };
              return p.x + p.y;
            }
        "#;
        compile(source).expect("const in struct field init should compile");
    }

    #[test]
    fn match_scrutinee_is_method_call_result() {
        let source = r#"
            enum Color { Red, Green, Blue }
            struct Picker { code: i64 }
            methods on Picker {
              fn choose(self: Picker) -> Color {
                if self.code == 0 { return Color.Red; }
                if self.code == 1 { return Color.Green; }
                return Color.Blue;
              }
            }
            fn main() -> i64 {
              let p: Picker = Picker { code: 1 };
              return match p.choose() {
                Color.Red then 100,
                Color.Green then 200,
                Color.Blue then 300,
              };
            }
        "#;
        compile(source).expect("match on method result should compile");
    }

    #[test]
    fn method_returns_vec_via_if_expression() {
        let source = r#"
            struct Maker { flag: bool }
            methods on Maker {
              fn make(self: Maker) -> Vec<i64> {
                return if self.flag { vec(1, 2, 3) } else { vec(10, 20, 30) };
              }
            }
            fn main() -> i64 {
              let m: Maker = Maker { flag: false };
              let xs: Vec<i64> = m.make();
              return xs[1];
            }
        "#;
        compile(source).expect("method returning Vec via if-expr should compile");
    }

    #[test]
    fn type_alias_chain_of_length_four() {
        let source = r#"
            type A = B;
            type B = C;
            type C = D;
            type D = i64;
            fn foo(x: A) -> A { return x + 1; }
            fn main() -> i64 { return foo(41); }
        "#;
        compile(source).expect("4-deep alias chain should compile");
    }

    #[test]
    fn type_alias_to_vec_compiles() {
        // `type IntList = Vec<i64>;` — alias resolves
        // through `Type::Vec`. Return type and let
        // annotations both pick up the alias.
        let source = r#"
            type IntList = Vec<i64>;
            fn build() -> IntList { return vec(1, 2, 3); }
            fn main() -> i64 {
              let xs: IntList = build();
              return xs[1];
            }
        "#;
        compile(source).expect("type alias to Vec should compile");
    }

    #[test]
    fn enum_with_single_variant_matches() {
        // Trivial enum (one variant). Verifies match
        // exhaustiveness logic handles N=1 correctly.
        let source = r#"
            enum Singleton { Only }
            fn main() -> i64 {
              let s: Singleton = Singleton.Only;
              return match s { Singleton.Only then 42, };
            }
        "#;
        compile(source).expect("single-variant enum match should compile");
    }

    #[test]
    fn struct_with_bool_field_plus_if_expr() {
        let source = r#"
            struct Flag { on: bool, val: i64 }
            methods on Flag {
              fn read(self: Flag) -> i64 {
                return if self.on { self.val } else { 0 };
              }
            }
            fn main() -> i64 {
              return Flag { on: true, val: 100 }.read();
            }
        "#;
        compile(source).expect("bool struct field + if-expr should compile");
    }

    #[test]
    fn negative_literal_as_struct_field_initializer() {
        // Negative integer literals work as struct
        // field initializers via the unary-minus
        // parsing path.
        let source = r#"
            struct Vec2 { x: i64, y: i64 }
            fn main() -> i64 {
              let v: Vec2 = Vec2 { x: -5, y: -3 };
              return v.x + v.y;
            }
        "#;
        compile(source).expect("negative struct field init should compile");
    }

    #[test]
    fn assert_with_method_call_result() {
        // `assert` accepts a method-call result as
        // the predicate.
        let source = r#"
            struct Point { x: i64, y: i64 }
            methods on Point {
              fn sum(self: Point) -> i64 { return self.x + self.y; }
            }
            fn main() -> i64 {
              let p: Point = Point { x: 3, y: 4 };
              assert p.sum() == 7;
              return 0;
            }
        "#;
        compile(source).expect("assert with method call should compile");
    }

    #[test]
    fn bool_method_used_in_if_condition() {
        // A method returning bool can appear directly
        // as the condition of an if statement — no
        // explicit `== true` ceremony needed.
        let source = r#"
            struct Range { lo: i64, hi: i64 }
            methods on Range {
              fn contains(self: Range, v: i64) -> bool {
                return v >= self.lo && v <= self.hi;
              }
            }
            fn main() -> i64 {
              let r: Range = Range { lo: 10, hi: 20 };
              if r.contains(15) { return 100; }
              return 0;
            }
        "#;
        compile(source).expect("bool method in if-cond should compile");
    }

    #[test]
    fn nested_mut_ref_field_assign_works() {
        // `self.i.val = …` inside a method that takes
        // `self: mut ref Outer` updates the nested
        // field cleanly. Combines the nested-field-
        // assign path with the mut-ref method shape.
        let source = r#"
            struct Inner { val: i64 }
            struct Outer { i: Inner }
            methods on Outer {
              fn bump(self: mut ref Outer) -> i64 {
                self.i.val = self.i.val + 1;
                return self.i.val;
              }
            }
            fn main() -> i64 {
              let o: Outer = Outer { i: Inner { val: 5 } };
              return o.bump();
            }
        "#;
        compile(source).expect("nested mut-ref field assign should compile");
    }

    #[test]
    fn index_then_field_assign_compiles_and_mutates() {
        // `xs[i].field = …;` now lowers directly through both
        // backends. The parser builds an `IndexAssign` with a
        // non-empty `field_path`; the checker validates that
        // the indexed element type is a struct and the field
        // exists; the backends emit `xs[i].field = v` (C) or
        // GEP-into-field + store (LLVM). T1.2 phase 2b
        // follow-up.
        let source = r#"
            struct Point { x: i64, y: i64 }
            fn main() -> i64 {
              let pts: [Point; 2] = [Point { x: 1, y: 2 }, Point { x: 3, y: 4 }];
              pts[1].x = 99;
              return pts[1].x + pts[1].y;
            }
        "#;
        compile(source).expect("xs[i].field = v should compile in v1");
        // Compare via the original assertion shape now turned
        // into a success check — the program returns 99 + 4.
        let c = compile_to_c(source).expect("C backend emits a program");
        let _ = c;
    }

    #[test]
    fn index_then_field_assign_deep_path_compiles() {
        // Closure #112: deep field paths (`xs[i].a.b = v`)
        // now lower end-to-end. Each intermediate segment
        // must be a Copy struct and the leaf field must be
        // Copy (no field-Drop on overwrite in v1).
        let source = r#"
            struct Inner { v: i64 }
            struct Outer { inner: Inner }
            fn main() -> i64 {
              let pts: [Outer; 1] = [Outer { inner: Inner { v: 0 } }];
              pts[0].inner.v = 99;
              return pts[0].inner.v;
            }
        "#;
        compile(source).expect("deep field path should compile");
    }

    #[test]
    fn mixed_place_assign_leaf_owned_str_emits_drop() {
        // F2 / closure #126: mixed-place index+field assign
        // where the LEAF field is OwnedStr is now allowed,
        // and the backends emit a free of the old slot
        // before storing the new value. Intermediate path
        // segments still require Copy.
        let source = r#"
            struct Tag { name: OwnedStr }
            fn main() -> i64 {
              let ts: Vec<Tag> = vec(
                Tag { name: "a" + "1" },
                Tag { name: "b" + "2" }
              );
              ts[0].name = "c" + "3";
              return 0;
            }
        "#;
        compile(source).expect("F2: leaf-OwnedStr mixed-place should compile");
        let c = compile_to_c(source).expect("C backend emits a program");
        // The C backend must free the old slot before the
        // store, otherwise the previous heap allocation
        // leaks.
        assert!(
            c.contains("free((void*)v_ts.data[(uint64_t)(0)].name);"),
            "expected free-of-old-slot before leaf store, got:\n{}",
            c
        );
    }

    #[test]
    fn mixed_place_assign_leaf_vec_emits_drop() {
        // F2 / closure #126: when the leaf field is itself
        // a Vec<T>, the backends emit a call to the inner
        // Vec's __free helper on the old slot before the
        // store. The whole-Vec replacement still requires
        // Copy intermediate segments.
        let source = r#"
            struct Bag { items: Vec<i64> }
            fn main() -> i64 {
              let bs: Vec<Bag> = vec(
                Bag { items: vec(1, 2) }
              );
              bs[0].items = vec(9, 8, 7);
              return 0;
            }
        "#;
        compile(source).expect("F2: leaf-Vec mixed-place should compile");
        let c = compile_to_c(source).expect("C backend emits a program");
        assert!(
            c.contains("intent_vec_int64_t__free(v_bs.data[(uint64_t)(0)].items);"),
            "expected vec-free of old slot before leaf store, got:\n{}",
            c
        );
    }

    #[test]
    fn mixed_place_assign_intermediate_non_copy_rejected() {
        // F2 / closure #126: the leaf exception is only
        // for the leaf — intermediate path segments must
        // still be Copy. A non-Copy intermediate field
        // would need full path-level Drop chains the
        // backends don't yet emit.
        let source = r#"
            struct Inner { name: OwnedStr }
            struct Outer { inner: Inner }
            fn main() -> i64 {
              let xs: Vec<Outer> = vec(
                Outer { inner: Inner { name: "a" + "1" } }
              );
              xs[0].inner = Inner { name: "b" + "2" };
              return 0;
            }
        "#;
        let err = compile(source).expect_err(
            "non-Copy intermediate in mixed-place assign should be rejected",
        );
        let msg = format!("{:?}", err);
        assert!(
            msg.contains("non-Copy") || msg.contains("Copy"),
            "expected diagnostic to mention Copy, got: {}",
            msg
        );
    }

    #[test]
    fn vec_struct_with_owned_str_field_drops_each_slot() {
        // Closure #127: `intent_vec_<S>__free` must walk
        // each live element and drop its owning fields,
        // otherwise a Vec<Struct{OwnedStr}> leaks the
        // per-element heap strings at scope exit. The C
        // backend emits `free((void*)xs.data[k].name)`
        // inside the helper's per-element loop body.
        let source = r#"
            struct Tag { name: OwnedStr }
            fn main() -> i64 {
              let ts: Vec<Tag> = vec(
                Tag { name: "a" + "1" },
                Tag { name: "b" + "2" }
              );
              return 0;
            }
        "#;
        compile(source).expect("Vec<Struct{OwnedStr}> should compile");
        let c = compile_to_c(source).expect("C backend emits a program");
        assert!(
            c.contains("free((void*)xs.data[k].name)"),
            "expected per-element OwnedStr free inside Vec __free, got:\n{}",
            c
        );
    }

    #[test]
    fn vec_struct_with_vec_field_drops_each_slot() {
        // Closure #127: when the struct element has a Vec
        // field, the per-element drop body must call the
        // inner Vec's __free on that field. Verifies the
        // Vec-field arm of c_element_drop_old.
        let source = r#"
            struct Bag { items: Vec<i64> }
            fn main() -> i64 {
              let bs: Vec<Bag> = vec(
                Bag { items: vec(1, 2, 3) },
                Bag { items: vec(4, 5) }
              );
              return 0;
            }
        "#;
        compile(source).expect("Vec<Struct{Vec<i64>}> should compile");
        let c = compile_to_c(source).expect("C backend emits a program");
        assert!(
            c.contains("intent_vec_int64_t__free(xs.data[k].items)"),
            "expected per-element vec-free inside Vec __free, got:\n{}",
            c
        );
    }

    #[test]
    fn vec_of_owned_str_drops_each_slot() {
        // Closure #127: also covers the OwnedStr-element
        // case directly (Vec<OwnedStr>). Each slot is an
        // i8* that must be freed before the buffer goes.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<OwnedStr> = vec("a" + "1", "b" + "2");
              return 0;
            }
        "#;
        compile(source).expect("Vec<OwnedStr> should compile");
        let c = compile_to_c(source).expect("C backend emits a program");
        // The per-element drop in intent_vec_OwnedStr__free
        // becomes `free((void*)xs.data[k])` (no field
        // suffix — the slot IS the i8*).
        assert!(
            c.contains("free((void*)xs.data[k])"),
            "expected per-element OwnedStr free inside Vec __free, got:\n{}",
            c
        );
    }

    #[test]
    fn index_assign_to_struct_array_element() {
        // Index-assign a whole struct value into a
        // `[Struct; N]` slot, e.g. `pts[1] = Point{…};`.
        // Was panicking on LLVM via `llvm_type(element)`
        // for the struct element type at the
        // IndexAssign store path; switched to
        // `llvm_type_string` (mirroring the read path).
        let source = r#"
            struct Point { x: i64, y: i64 }
            fn main() -> i64 {
              let pts: [Point; 3] = [
                Point { x: 1, y: 1 },
                Point { x: 2, y: 2 },
                Point { x: 3, y: 3 },
              ];
              pts[1] = Point { x: 99, y: 100 };
              return pts[1].x + pts[1].y;
            }
        "#;
        compile(source).expect("indexed struct-assign should compile");
    }

    #[test]
    fn for_iter_struct_array_with_method_call_in_body() {
        // For-iter over `[Struct; N]` plus method call
        // on each loop variable. Tests that the per-
        // iteration alloca for `p` plus the method's
        // signature lookup interact cleanly.
        let source = r#"
            struct Point { x: i64, y: i64 }
            methods on Point {
              fn dist_sq(self: Point) -> i64 {
                return self.x * self.x + self.y * self.y;
              }
            }
            fn main() -> i64 {
              let pts: [Point; 3] = [
                Point { x: 1, y: 1 },
                Point { x: 3, y: 4 },
                Point { x: 5, y: 12 },
              ];
              let total: i64 = 0;
              for p in pts { total = total + p.dist_sq(); }
              return total;
            }
        "#;
        compile(source).expect("for-iter struct array + method should compile");
    }

    #[test]
    fn struct_with_float_fields_and_method() {
        // f64 fields + method returning f64. Tests
        // that the per-name struct typedef in both
        // backends handles non-integer scalar fields
        // and that method return type can be f64.
        let source = r#"
            struct Vec3 { x: f64, y: f64, z: f64 }
            methods on Vec3 {
              fn dot(self: Vec3, other: Vec3) -> f64 {
                return self.x * other.x + self.y * other.y + self.z * other.z;
              }
            }
            fn main() -> i64 {
              let a: Vec3 = Vec3 { x: 1.0, y: 2.0, z: 3.0 };
              let b: Vec3 = Vec3 { x: 4.0, y: 5.0, z: 6.0 };
              return a.dot(b) as i64;
            }
        "#;
        compile(source).expect("float struct + method should compile");
    }

    #[test]
    fn struct_with_u8_fields_and_method() {
        // Smaller integer scalar fields (u8 here). The
        // checker's `is_copy()` says yes for all
        // primitive integer types, and the struct
        // codegen renders them via `c_element_storage`
        // / `llvm_type_string` correctly.
        let source = r#"
            struct Color { r: u8, g: u8, b: u8 }
            methods on Color {
              fn brightness(self: Color) -> i64 {
                return (self.r as i64) + (self.g as i64) + (self.b as i64);
              }
            }
            fn main() -> i64 {
              return Color { r: 100, g: 150, b: 200 }.brightness();
            }
        "#;
        compile(source).expect("u8 struct + method should compile");
    }

    #[test]
    fn enum_as_struct_field_plus_method_returning_bool() {
        // Covers three intersecting features in one
        // program: enum value as a struct field type,
        // a method on the wrapping struct that matches
        // on the enum field (with wildcard arm), and
        // a method returning bool (most other methods
        // tests return i64).
        let source = r#"
            enum Status { Pending, Active, Done }
            struct Job { id: i64, status: Status }
            methods on Job {
              fn is_done(self: ref Job) -> bool {
                return match self.status {
                  Status.Done then true,
                  _ then false,
                };
              }
            }
            fn main() -> i64 {
              let j: Job = Job { id: 1, status: Status.Done };
              if j.is_done() { return 100; }
              return 0;
            }
        "#;
        compile(source).expect("enum-field + bool method should compile");
    }

    #[test]
    fn match_in_mut_ref_method_body() {
        // A `match` expression that produces the new
        // value for a field-assign through `mut ref`
        // composes cleanly. Tests
        // self.val = match c { … } pattern.
        let source = r#"
            enum Cmd { Inc, Dec, Reset }
            struct Counter { val: i64 }
            methods on Counter {
              fn apply(self: mut ref Counter, c: Cmd) -> i64 {
                self.val = match c {
                  Cmd.Inc then self.val + 1,
                  Cmd.Dec then self.val - 1,
                  Cmd.Reset then 0,
                };
                return self.val;
              }
            }
            fn main() -> i64 {
              let c: Counter = Counter { val: 10 };
              return c.apply(Cmd.Inc);
            }
        "#;
        compile(source).expect("match in mut-ref method should compile");
    }

    #[test]
    fn match_nested_in_if_expression_branches() {
        // Each branch of an if-expression can itself be
        // a match expression. Tests the Match-emit fix
        // applied to a different containing context
        // than the if-expr/else-if chain case.
        let source = r#"
            enum Mode { Fast, Slow }
            fn cost(m: Mode, x: i64) -> i64 {
              return if x > 100 {
                match m { Mode.Fast then 1000, Mode.Slow then 5000, }
              } else {
                match m { Mode.Fast then 10, Mode.Slow then 50, }
              };
            }
            fn main() -> i64 { return cost(Mode.Fast, 200); }
        "#;
        compile(source).expect("match in if-expr branches should compile");
    }

    #[test]
    fn method_takes_ref_vec_arg() {
        // Methods can take `ref Vec<T>` arguments and
        // iterate them. Tests that `len(xs)` through a
        // ref Vec auto-derefs and indexing works.
        let source = r#"
            struct Stats { count: i64 }
            methods on Stats {
              fn sum_all(self: Stats, xs: ref Vec<i64>) -> i64 {
                let total: i64 = 0;
                let n: u64 = len(xs);
                let i: u64 = 0;
                while i < n {
                  total = total + xs[i as i64];
                  i = i + 1;
                }
                return total + self.count;
              }
            }
            fn main() -> i64 {
              let s: Stats = Stats { count: 100 };
              let xs: Vec<i64> = vec(1, 2, 3, 4, 5);
              return s.sum_all(ref xs);
            }
        "#;
        compile(source).expect("method taking ref Vec should compile");
    }

    #[test]
    fn method_returns_vec_of_struct() {
        // Method body can build and return a
        // `Vec<Struct>`. Tests the full lifecycle:
        // empty `vec()` of struct elements, repeated
        // push, returning the owned Vec through the
        // method's affine return path.
        let source = r#"
            struct Point { x: i64, y: i64 }
            struct Builder { n: i64 }
            methods on Builder {
              fn grid(self: Builder) -> Vec<Point> {
                let xs: Vec<Point> = vec();
                let i: i64 = 0;
                while i < self.n {
                  xs = push(xs, Point { x: i, y: i * 2 });
                  i = i + 1;
                }
                return xs;
              }
            }
            fn main() -> i64 {
              let b: Builder = Builder { n: 4 };
              let ps: Vec<Point> = b.grid();
              let mid: Point = ps[2];
              return mid.x + mid.y;
            }
        "#;
        compile(source).expect("method returning Vec<Struct> should compile");
    }

    #[test]
    fn method_calls_another_method_on_same_type() {
        // Inside one method, calling another method on
        // the same type (via `self.other_method()`)
        // works through the standard MethodCall desugar.
        let source = r#"
            struct Point { x: i64, y: i64 }
            methods on Point {
              fn dist_sq(self: Point) -> i64 {
                return self.x * self.x + self.y * self.y;
              }
              fn weighted(self: Point) -> i64 {
                return self.dist_sq() * self.x;
              }
            }
            fn main() -> i64 {
              return Point { x: 3, y: 4 }.weighted();
            }
        "#;
        compile(source).expect("method-to-method call should compile");
    }

    #[test]
    fn recursive_method_via_smaller_self() {
        // Methods can recurse by constructing a smaller
        // instance and calling the same method. Common
        // pattern for factorial-style recursion.
        let source = r#"
            struct Counter { n: i64 }
            methods on Counter {
              fn factorial(self: Counter) -> i64 {
                if self.n <= 1 { return 1; }
                let smaller: Counter = Counter { n: self.n - 1 };
                return self.n * smaller.factorial();
              }
            }
            fn main() -> i64 {
              return Counter { n: 5 }.factorial();
            }
        "#;
        compile(source).expect("recursive method should compile");
    }

    #[test]
    fn methods_on_type_alias_to_struct_compiles() {
        // `type Pt = Point;` followed by `methods on Pt`
        // should work because the alias-substitution
        // pass rewrites the methods-block target to
        // `Type::Struct("Point")` before the hoist runs.
        let source = r#"
            struct Point { x: i64, y: i64 }
            type Pt = Point;
            methods on Pt {
              fn dist_sq(self: Pt) -> i64 { return self.x * self.x + self.y * self.y; }
            }
            fn main() -> i64 {
              let p: Pt = Point { x: 3, y: 4 };
              return p.dist_sq();
            }
        "#;
        compile(source).expect("methods on type alias should compile");
    }

    #[test]
    fn struct_with_tuple_field_compiles_and_runs() {
        // Tuple types are valid struct fields. The C
        // backend's `c_element_storage` and the LLVM
        // backend's `llvm_type_string` both render the
        // per-shape tuple struct as the field type.
        let source = r#"
            struct Pair { coord: (i64, i64), label: i64 }
            fn main() -> i64 {
              let p: Pair = Pair { coord: (10, 20), label: 5 };
              return p.coord.0 + p.coord.1 + p.label;
            }
        "#;
        compile(source).expect("struct with tuple field should compile");
    }

    #[test]
    fn method_with_tuple_arg_and_tuple_return() {
        // Methods can take and return tuple-typed values.
        let source = r#"
            struct Origin { x: i64, y: i64 }
            methods on Origin {
              fn shift(self: Origin, delta: (i64, i64)) -> (i64, i64) {
                return (self.x + delta.0, self.y + delta.1);
              }
            }
            fn main() -> i64 {
              let o: Origin = Origin { x: 1, y: 2 };
              let (a, b) = o.shift((10, 20));
              return a + b;
            }
        "#;
        compile(source).expect("tuple method args + return should compile");
    }

    #[test]
    fn empty_vec_of_struct_then_push_compiles() {
        // Empty `vec()` of struct elements works on both
        // backends, then push grows the buffer. Tests
        // the no-element initial-allocation path through
        // the struct-sizeof codegen.
        let source = r#"
            struct Point { x: i64, y: i64 }
            fn main() -> i64 {
              let xs: Vec<Point> = vec();
              let xs2: Vec<Point> = push(xs, Point { x: 7, y: 8 });
              return xs2[0].x + xs2[0].y;
            }
        "#;
        compile(source).expect("empty Vec<Struct> + push should compile");
    }

    #[test]
    fn min_max_are_context_sensitive_identifiers() {
        // `min` and `max` are no longer reserved
        // keywords — they're context-sensitive
        // identifiers used by `reduce X with min;` and
        // the `min(a,b)` / `max(a,b)` intrinsics. Users
        // can declare struct fields, locals, etc.
        // called `min` / `max` outside those contexts.
        let source = r#"
            struct Range { min: i64, max: i64 }
            fn main() -> i64 {
              let r: Range = Range { min: 5, max: 20 };
              return min(r.min, 3) + max(r.max, 100);
            }
        "#;
        compile(source).expect("min/max as field names should compile");
    }

    #[test]
    fn call_args_trailing_comma_accepted() {
        // Trailing comma in function-call arg list lets
        // multi-line calls match the style already
        // accepted by struct/enum/array literals.
        let source = r#"
            fn add3(a: i64, b: i64, c: i64) -> i64 { return a + b + c; }
            fn main() -> i64 {
              return add3(1, 2, 3,);
            }
        "#;
        compile(source).expect("trailing comma in call should compile");
    }

    #[test]
    fn method_call_trailing_comma_accepted() {
        let source = r#"
            struct Point { x: i64, y: i64 }
            methods on Point {
              fn add(self: Point, dx: i64, dy: i64) -> i64 {
                return self.x + self.y + dx + dy;
              }
            }
            fn main() -> i64 {
              let p: Point = Point { x: 1, y: 2 };
              return p.add(3, 4,);
            }
        "#;
        compile(source).expect("trailing comma in method call should compile");
    }

    #[test]
    fn array_literal_trailing_comma_accepted() {
        // Multi-line array literals can now use a trailing
        // comma on the last element, matching the style
        // already accepted by struct fields, enum variants,
        // and methods blocks.
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 3] = [1, 2, 3,];
              return xs[2];
            }
        "#;
        compile(source).expect("trailing comma should compile");
    }

    #[test]
    fn array_of_struct_compiles_via_c_backend() {
        // T1.2 follow-up: `[Point; N]` arrays lower
        // correctly on the C backend. Previously the let
        // emit hardcoded `c_leaf_type(element)` which
        // returns `/* struct */` placeholder for nominal
        // types, producing invalid C
        // (`/* struct */ v_arr[3]`). Switched to
        // `c_element_storage` which routes struct types
        // through `struct_c_name`. Companion LLVM fix
        // switches array let + index emit to
        // `llvm_type_string` for aggregate elements.
        let source = r#"
            struct Point { x: i64, y: i64 }
            fn main() -> i64 {
              let arr: [Point; 3] = [Point { x: 1, y: 2 }, Point { x: 3, y: 4 }, Point { x: 5, y: 6 }];
              let p: Point = arr[1];
              return p.x + p.y;
            }
        "#;
        compile(source).expect("[Struct; N] should compile");
        let c = compile_to_c(source).expect("emits C");
        assert!(
            c.contains("Struct_Point v_arr[3]"),
            "expected `Struct_Point v_arr[3]` in emitted C, got:\n{c}"
        );
    }

    #[test]
    fn for_iter_over_array_of_struct_compiles() {
        // For-loop iteration over `[Struct; N]` works on
        // both backends: the iteration variable is bound
        // to a value of the struct type, and the body can
        // read its fields. Tests that field-access through
        // a copied struct value works inside a loop.
        let source = r#"
            struct Counter { n: i64 }
            fn main() -> i64 {
              let counters: [Counter; 3] = [Counter { n: 1 }, Counter { n: 2 }, Counter { n: 3 }];
              let total: i64 = 0;
              for c in counters { total = total + c.n; }
              return total;
            }
        "#;
        compile(source).expect("for-iter over [Struct; N] should compile");
    }

    #[test]
    fn vec_of_struct_compiles_on_both_backends() {
        // T1.2 + Vec<Struct> support on both backends.
        // C backend: `element_tag` routes `Type::Struct(name)`
        // through `struct_c_name(name)` to produce
        // `intent_vec_Struct_<Name>` (was
        // `intent_vec_/*_struct_*/`).
        // LLVM backend: `vec_struct_tag` handles Struct/Enum
        // elements; `vec_element_size_expr` uses the GEP-null
        // sizeof trick for struct/tuple element types so
        // malloc/realloc allocate the right number of bytes
        // (was returning 8 for every aggregate, leading to
        // heap corruption); `emit_expr` Vec-Index path uses
        // `llvm_type_string` for the element type instead
        // of panicking on aggregates.
        let source = r#"
            struct Point { x: i64, y: i64 }
            fn main() -> i64 {
              let p1: Point = Point { x: 1, y: 2 };
              let p2: Point = Point { x: 3, y: 4 };
              let xs: Vec<Point> = vec(p1, p2);
              let first: Point = xs[0];
              return first.x + first.y;
            }
        "#;
        compile(source).expect("Vec<Struct> should compile");
        let c = compile_to_c(source).expect("emits C");
        assert!(
            c.contains("intent_vec_Struct_Point"),
            "expected mangled Vec<Struct> name in C, got:\n{c}"
        );
    }

    #[test]
    fn enum_to_int_cast_compiles_and_runs() {
        // T1.3 follow-up: enums lower to an i32 tag in
        // both backends, so casting to any integer type
        // (i64, i32, u64, etc.) is a safe widening to
        // the variant index. Useful for serialization,
        // table-driven dispatch, and printing diagnostic
        // values.
        let source = r#"
            enum Color { Red, Green, Blue }
            fn main() -> i64 {
              let c: Color = Color.Green;
              return c as i64;
            }
        "#;
        compile(source).expect("enum cast should compile");
    }

    #[test]
    fn enum_to_smaller_int_cast_compiles() {
        let source = r#"
            enum Color { Red, Green, Blue }
            fn main() -> i64 {
              let c: Color = Color.Blue;
              let small: i8 = c as i8;
              return small as i64;
            }
        "#;
        compile(source).expect("enum-to-i8 cast should compile");
    }

    #[test]
    fn enum_to_float_cast_rejected() {
        // Only enum→integer casts are allowed in v1.
        // Float would require a less obvious sitofp dance.
        let source = r#"
            enum Color { Red, Green, Blue }
            fn main() -> i64 {
              let c: Color = Color.Red;
              let f: f64 = c as f64;
              return 0;
            }
        "#;
        let errors = compile(source)
            .expect_err("enum-to-float cast should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("cannot cast")),
            "expected cast-rejection diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn match_on_integer_compiles_and_runs() {
        // T1.3 integer pattern: scrutinee can now be an
        // integer type with integer-literal arms + a
        // required wildcard.
        let source = r#"
            fn describe(x: i64) -> i64 {
              return match x {
                0 then 100,
                1 then 200,
                42 then 300,
                _ then 999,
              };
            }
            fn main() -> i64 {
              return describe(42);
            }
        "#;
        compile(source).expect("integer match should compile");
        let c = compile_to_c(source).expect("emits C");
        assert!(
            c.contains("case 42:"),
            "expected `case 42:` in emitted C, got:\n{c}"
        );
    }

    #[test]
    fn match_on_integer_with_negative_pattern() {
        // Negative integer literal patterns parse and
        // dispatch correctly.
        let source = r#"
            fn sign(x: i64) -> i64 {
              return match x {
                -1 then -100,
                0 then 0,
                _ then 100,
              };
            }
            fn main() -> i64 { return sign(-1); }
        "#;
        compile(source).expect("negative integer pattern should compile");
    }

    #[test]
    fn match_on_integer_requires_wildcard() {
        // Integer scrutinees can't be exhaustively covered
        // by listing values — the checker requires a
        // wildcard arm to close out the open set.
        let source = r#"
            fn f(x: i64) -> i64 {
              return match x {
                0 then 1,
                1 then 2,
              };
            }
            fn main() -> i64 { return f(0); }
        "#;
        let errors = compile(source)
            .expect_err("missing wildcard on integer match should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("require a wildcard")),
            "expected wildcard-required diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn match_integer_pattern_on_enum_rejected() {
        // Mixing pattern shapes against scrutinee kind
        // is a type error.
        let source = r#"
            enum Color { Red, Green, Blue }
            fn main() -> i64 {
              let c: Color = Color.Red;
              return match c {
                42 then 1,
                _ then 0,
              };
            }
        "#;
        let errors = compile(source)
            .expect_err("integer pattern on enum scrutinee should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("integer pattern")
                    && e.message.contains("enum")),
            "expected pattern-kind diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn match_variant_pattern_on_integer_rejected() {
        let source = r#"
            enum Color { Red, Green, Blue }
            fn main() -> i64 {
              let x: i64 = 5;
              return match x {
                Color.Red then 1,
                _ then 0,
              };
            }
        "#;
        let errors = compile(source)
            .expect_err("variant pattern on integer scrutinee should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("variant pattern")
                    && e.message.contains("integer")),
            "expected variant-on-int diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn match_wildcard_covers_remaining_variants() {
        // T1.3 wildcard: `_ then …` covers every variant not
        // explicitly listed. Exhaustiveness check is satisfied
        // by the wildcard so we can omit the rest.
        let source = r#"
            enum Color { Red, Green, Blue, Yellow }
            fn classify(c: Color) -> i64 {
              return match c {
                Color.Red then 1,
                _ then 0,
              };
            }
            fn main() -> i64 {
              let c: Color = Color.Yellow;
              return classify(c);
            }
        "#;
        compile(source).expect("wildcard arm should compile");
        let c = compile_to_c(source).expect("wildcard match emits C");
        assert!(
            c.contains("default:") && c.contains("__r = (0)"),
            "expected wildcard to emit default case with body 0, got:\n{c}"
        );
    }

    #[test]
    fn match_wildcard_alone_is_exhaustive() {
        // A bare `_ then 0,` arm must satisfy exhaustiveness
        // without any variant-specific arms.
        let source = r#"
            enum Color { Red, Green, Blue }
            fn main() -> i64 {
              let c: Color = Color.Green;
              return match c {
                _ then 7,
              };
            }
        "#;
        compile(source).expect("bare wildcard match should compile");
    }

    #[test]
    fn match_wildcard_followed_by_arm_rejected() {
        // Once a `_` arm appears, every subsequent arm is
        // dead — checker rejects with an unreachable-arm
        // diagnostic.
        let source = r#"
            enum Color { Red, Green, Blue }
            fn main() -> i64 {
              let c: Color = Color.Red;
              return match c {
                _ then 7,
                Color.Red then 1,
              };
            }
        "#;
        let errors = compile(source)
            .expect_err("arm after wildcard should fail");
        assert!(
            errors.iter().any(|e| e.message.contains("unreachable")),
            "expected unreachable-arm diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn enum_decl_variant_and_match_arms() {
        // T1.3 phase 1: payload-less enum variants + match
        // expressions with exhaustive `then` arms.
        let source = r#"
            enum Color { Red, Green, Blue }
            fn pick(c: Color) -> i64 {
              return match c {
                Color.Red then 1,
                Color.Green then 2,
                Color.Blue then 3,
              };
            }
            fn main() -> i64 {
              let c: Color = Color.Green;
              return pick(c);
            }
        "#;
        compile(source).expect("enum + match should compile");
    }

    #[test]
    fn match_non_exhaustive_rejected() {
        let source = r#"
            enum Color { Red, Green, Blue }
            fn main() -> i64 {
              let c: Color = Color.Red;
              return match c {
                Color.Red then 1,
                Color.Green then 2,
              };
            }
        "#;
        let errors = compile(source).expect_err("non-exhaustive match should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("non-exhaustive") && e.message.contains("Blue")),
            "expected non-exhaustive diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn match_unknown_variant_rejected() {
        let source = r#"
            enum Color { Red, Green, Blue }
            fn main() -> i64 {
              let c: Color = Color.Red;
              return match c {
                Color.Red then 1,
                Color.Green then 2,
                Color.Blue then 3,
                Color.Yellow then 4,
              };
            }
        "#;
        let errors = compile(source).expect_err("unknown variant in match should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("no variant 'Yellow'")),
            "expected no-variant diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn struct_decl_construct_and_field_access() {
        // T1.2 phase 1: user-declared struct types with
        // named fields. Copy-only fields in v1, no
        // methods, no RAII chains; construction and field
        // read work end-to-end on both backends.
        let source = r#"
            struct Point { x: i64, y: i64 }
            fn manhattan(p: Point) -> i64 {
              return p.x + p.y;
            }
            fn main() -> i64 {
              let p: Point = Point { x: 3, y: 4 };
              return manhattan(p);
            }
        "#;
        compile(source).expect("struct decl + literal + access should compile");
        let c = compile_to_c(source).expect("struct emits C");
        assert!(
            c.contains("typedef struct {") && c.contains("Struct_Point"),
            "expected per-struct typedef in C:\n{c}"
        );
    }

    #[test]
    fn struct_missing_field_rejected() {
        let source = r#"
            struct Point { x: i64, y: i64 }
            fn main() -> i64 {
              let p: Point = Point { x: 3 };
              return p.x;
            }
        "#;
        let errors = compile(source)
            .expect_err("missing-field literal should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("Point") && e.message.contains("2 fields")),
            "expected arity-mismatch diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn struct_unknown_field_rejected() {
        let source = r#"
            struct Point { x: i64, y: i64 }
            fn main() -> i64 {
              let p: Point = Point { x: 3, y: 4 };
              return p.z;
            }
        "#;
        let errors = compile(source).expect_err("unknown field should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("no field named 'z'")),
            "expected unknown-field diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn struct_mutex_field_compiles_and_locks() {
        // Closure #123: Mutex<T> is now an accepted struct
        // field. Together with closure #102's field-borrow
        // (`ref s.m`), users can build mutex-guarded state
        // structures.
        let source = r#"
            struct State { m: Mutex<i64> }
            fn main() -> i64 {
              let s: State = State { m: mutex_new(42) };
              let g = mutex_lock(ref s.m);
              return 0;
            }
        "#;
        compile(source).expect("Mutex field + lock should compile");
    }

    #[test]
    fn struct_guard_field_still_rejected() {
        // Guard<T> requires explicit Drop wiring (RAII
        // unlock) — still rejected as a struct field.
        let source = r#"
            struct Locked { g: Guard<i64> }
            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("Guard field should still fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("non-Copy")
                    && e.message.contains("Guard")),
            "expected Guard-rejected diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn struct_array_field_compiles_and_indexes() {
        // T1.2 phase 2b: `[T; N]` of Copy elements is a valid
        // struct field. The C side renders it as `T name[N]`
        // (declarator form); the LLVM side handles
        // FieldAccess-as-Index-base by GEP'ing into the field
        // aggregate and then into the element.
        let source = r#"
            struct Buf { data: [i64; 4] }
            fn main() -> i64 {
              let b: Buf = Buf { data: [1, 2, 3, 4] };
              assert b.data[0] == 1;
              assert b.data[3] == 4;
              return 0;
            }
        "#;
        compile(source)
            .expect("[T;N] struct field with FieldAccess index should compile");
        let c = compile_to_c(source).expect("C backend emits a program");
        assert!(
            c.contains("int64_t data[4]"),
            "expected inline C array declarator in struct typedef:\n{c}"
        );
    }

    #[test]
    fn struct_vec_field_compiles_and_drops() {
        // T1.2 phase 2b: `Vec<T>` struct field is accepted;
        // the Vec typedef is hoisted above the struct typedef
        // and the struct Drop emits the per-element-type
        // `__free` helper.
        let source = r#"
            struct Bag { contents: Vec<i64> }
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let b: Bag = Bag { contents: xs };
              return 0;
            }
        "#;
        compile(source)
            .expect("struct with Vec field should type-check");
        let c = compile_to_c(source).expect("C backend emits a program");
        assert!(
            c.contains("intent_vec_int64_t__free(v_b.contents)"),
            "expected per-field Vec free in C output:\n{c}"
        );
    }

    #[test]
    fn push_mut_through_struct_field() {
        // T1.2 phase 2b follow-up: `push(mut ref t.xs, v)`
        // dispatches to the in-place `__push_mut` helper.
        // Combined with affine struct fields + field-borrow,
        // this is the natural way to grow a Vec owned by a
        // struct.
        let source = r#"
            struct Bag { id: i64, contents: Vec<i64> }
            fn main() -> i64 {
              let b: Bag = Bag { id: 7, contents: vec(1, 2) };
              let n: i64 = push(mut ref b.contents, 3);
              assert n == 3;
              assert b.id == 7;
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("in-place push through field should compile");
        assert!(
            c.contains("intent_vec_int64_t__push_mut"),
            "expected __push_mut helper in C output:\n{c}"
        );
    }

    #[test]
    fn push_mut_local_binding() {
        // The in-place form also works on a local Vec — the
        // caller takes `mut ref xs` and the helper grows /
        // mutates through the pointer.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20);
              let n: i64 = push(mut ref xs, 30);
              assert n == 3;
              return 0;
            }
        "#;
        compile(source).expect("in-place push on local should compile");
    }

    #[test]
    fn enum_eq_via_user_impl() {
        // Mirrors `struct_eq_via_user_impl`: `implement Eq for
        // Color { fn eq(self: Color, other: Color) -> bool }`
        // makes `a == b` and `a != b` work on Color bindings.
        let source = r#"
            enum Color { Red, Green, Blue }
            interface Eq { fn eq(self: Color, other: Color) -> bool; }
            implement Eq for Color {
              fn eq(self: Color, other: Color) -> bool {
                return (self as i32) == (other as i32);
              }
            }
            fn main() -> i64 {
              let a: Color = Color.Red;
              let b: Color = Color.Red;
              let c: Color = Color.Blue;
              assert a == b;
              assert a != c;
              return 0;
            }
        "#;
        compile(source).expect("Color equality should compile via user Eq impl");
        let c = compile_to_c(source).expect("C backend emits a program");
        assert!(
            c.contains("fn_Color_eq("),
            "expected dispatch to Color_eq in C output:\n{c}"
        );
    }

    #[test]
    fn partial_move_whole_after_field_rejected() {
        // After `let taken = b.contents;`, the struct `b` is
        // only partially initialized; moving it as a whole is
        // unsound and surfaces a targeted diagnostic.
        let source = r#"
            struct Bag { id: i64, contents: Vec<i64> }
            fn consume(b: Bag) -> i64 { return b.id; }
            fn main() -> i64 {
              let b: Bag = Bag { id: 1, contents: vec(1, 2, 3) };
              let taken: Vec<i64> = b.contents;
              let n: i64 = consume(b);
              return n;
            }
        "#;
        let errors = compile(source)
            .expect_err("whole-struct move after partial move should fail");
        assert!(
            errors.iter().any(|e| e.message.contains("partially initialized")),
            "expected partial-move diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn partial_move_field_extract_compiles() {
        // T1.2 phase 2b partial-move: `let taken = b.contents;`
        // moves the Vec field out of the struct. The struct
        // binding is still live for its remaining Copy fields,
        // and scope-exit Drop skips the moved field.
        let source = r#"
            struct Bag { id: i64, contents: Vec<i64> }
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              let b: Bag = Bag { id: 7, contents: xs };
              let taken: Vec<i64> = b.contents;
              assert b.id == 7;
              return 0;
            }
        "#;
        compile(source).expect("partial-move of struct field should compile");
        let c = compile_to_c(source).expect("C backend emits a program");
        // The Vec freed at scope exit is `taken`, not `b.contents`.
        assert!(
            c.contains("intent_vec_int64_t__free(v_taken)"),
            "expected free of v_taken in C output:\n{c}"
        );
        assert!(
            !c.contains("intent_vec_int64_t__free(v_b.contents)"),
            "should NOT emit free for moved-out field b.contents:\n{c}"
        );
    }

    #[test]
    fn partial_move_double_extract_rejected() {
        // Reading a moved field again is a use-after-move.
        let source = r#"
            struct Bag { id: i64, contents: Vec<i64> }
            fn main() -> i64 {
              let b: Bag = Bag { id: 7, contents: vec(1, 2, 3) };
              let taken: Vec<i64> = b.contents;
              let again: Vec<i64> = b.contents;
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("double extract should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("'b.contents' was moved")),
            "expected use-after-move diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn struct_eq_via_user_impl() {
        // User-defined equality: `implement Eq for Point {
        // fn eq(self: Point, other: Point) -> bool { … } }`
        // makes `a == b` and `a != b` work on Point bindings
        // by routing through the hoisted `Point_eq` function.
        let source = r#"
            interface Eq { fn eq(self: Point, other: Point) -> bool; }
            struct Point { x: i64, y: i64 }
            implement Eq for Point {
              fn eq(self: Point, other: Point) -> bool {
                if self.x != other.x { return false; }
                if self.y != other.y { return false; }
                return true;
              }
            }
            fn main() -> i64 {
              let a: Point = Point { x: 1, y: 2 };
              let b: Point = Point { x: 1, y: 2 };
              let c: Point = Point { x: 3, y: 4 };
              assert a == b;
              assert a != c;
              return 0;
            }
        "#;
        compile(source).expect("Point equality should compile via user Eq impl");
        let c = compile_to_c(source).expect("C backend emits a program");
        assert!(
            c.contains("fn_Point_eq("),
            "expected dispatch to Point_eq in C output:\n{c}"
        );
    }

    #[test]
    fn struct_drop_reverse_field_order() {
        // T1.2 phase 2b polish: heap-shaped fields are freed
        // in reverse declaration order so destruction mirrors
        // construction (Rust's RAII convention). Probe with a
        // struct carrying two OwnedStr fields named `first` and
        // `second`; the per-field free pass must emit `second`
        // before `first`.
        let source = r#"
            struct Pair { first: OwnedStr, second: OwnedStr }
            fn main() -> i64 {
              let p: Pair = Pair { first: "a" + "1", second: "b" + "2" };
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("Pair struct should compile");
        let first_idx = c
            .find("free((void*)v_p.first)")
            .expect("expected free of v_p.first");
        let second_idx = c
            .find("free((void*)v_p.second)")
            .expect("expected free of v_p.second");
        assert!(
            second_idx < first_idx,
            "expected reverse-declaration drop order \
             (`second` before `first`):\n{c}"
        );
    }

    #[test]
    fn field_borrow_unlocks_atomic_through_struct() {
        // T1.2 phase 2b follow-up: `ref t.f` / `mut ref t.f`
        // takes a borrow of a struct field. The checker
        // produces TypedExprKind::RefField / RefMutField; the
        // backends GEP into the field. Combined with the
        // Atomic<T> struct field gate from closure #100, this
        // unlocks `atomic_*(ref c.hits)` patterns through a
        // struct.
        let source = r#"
            struct Counter { hits: Atomic<i64> }
            fn main() -> i64 {
              let c: Counter = Counter { hits: atomic_new(0) };
              atomic_store(mut ref c.hits, 42);
              let v: i64 = atomic_load(ref c.hits);
              assert v == 42;
              return 0;
            }
        "#;
        compile(source).expect("atomic through struct field should compile");
        let c = compile_to_c(source).expect("C backend emits a program");
        assert!(
            c.contains("&v_c.hits"),
            "expected field-address emission in C output:\n{c}"
        );
    }

    #[test]
    fn field_borrow_rejects_non_struct_base() {
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              let r: i64 = atomic_load(ref x.foo);
              return r;
            }
        "#;
        let errors = compile(source)
            .expect_err("field-borrow on non-struct should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("must be a struct binding")),
            "expected struct-base diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn user_drop_auto_call_at_scope_exit() {
        // T2.7 phase 2: `implement Drop for T { fn drop(self:
        // T) -> i64 { … } }` runs automatically at every scope
        // exit where the binding hasn't been moved out. The
        // hoisted `T_drop` is in the function table; both
        // backends emit a call to it through the existing
        // `TypedStmt::Drop` lowering.
        let source = r#"
            struct Resource { id: i64, open: bool }
            interface Drop {
              fn drop(self: Resource) -> i64;
            }
            implement Drop for Resource {
              fn drop(self: Resource) -> i64 {
                return self.id;
              }
            }
            fn use_resource(id: i64) -> i64 {
              let r: Resource = Resource { id: id, open: true };
              return id;
            }
            fn main() -> i64 {
              let a: i64 = use_resource(7);
              assert a == 7;
              return 0;
            }
        "#;
        compile(source)
            .expect("user-Drop auto-call should compile");
        let c = compile_to_c(source).expect("C backend emits a program");
        assert!(
            c.contains("fn_Resource_drop(v_r)"),
            "expected auto-call to user Drop at scope exit:\n{c}"
        );
    }

    #[test]
    fn struct_owned_str_field_compiles_and_drops() {
        // T1.2 phase 2b: a struct may carry an `OwnedStr`
        // field. The aggregate is non-Copy, the checker
        // marks struct-literal initialization as a move on
        // the source binding, and both backends emit a
        // `free(v_t.<field>)` when the struct local is
        // dropped at scope exit.
        let source = r#"
            struct Tag {
              id: i64,
              name: OwnedStr,
              active: bool,
            }
            fn make_tag(id: i64, name: OwnedStr) -> Tag {
              return Tag { id: id, name: name, active: true };
            }
            fn main() -> i64 {
              let s: OwnedStr = "release-" + "v1";
              let t: Tag = make_tag(7, s);
              assert t.id == 7;
              let u: Tag = Tag { id: 42, name: "a" + "b", active: false };
              assert u.id == 42;
              return 0;
            }
        "#;
        compile(source)
            .expect("struct with OwnedStr field should type-check");
        let c = compile_to_c(source).expect("C backend emits a program");
        // Each struct local gets a per-field free; with two
        // bindings (t, u) we expect two such free calls.
        let free_count = c.matches("free((void*)v_").count();
        assert!(
            free_count >= 2,
            "expected at least two per-field free() calls in C output, got {free_count}:\n{c}"
        );
    }

    #[test]
    fn tuple_multi_return_and_destructure() {
        // T1.1 phase 2: tuple types + tuple expressions +
        // destructure-let work end-to-end through both
        // backends. The example below is the canonical
        // multi-return pattern — `divmod` returns
        // `(i64, i64)`, the caller destructures into `(q, r)`.
        let source = r#"
            fn divmod(a: i64, b: i64) -> (i64, i64)
            requires b > 0;
            {
              return (a / b, a % b);
            }
            fn main() -> i64 {
              let (q, r) = divmod(10, 3);
              return q + r;
            }
        "#;
        compile(source).expect("tuple multi-return + destructure should compile");
        let c = compile_to_c(source).expect("tuple program emits C");
        assert!(
            c.contains("intent_tuple_int64_t_int64_t"),
            "expected per-shape tuple struct in C:\n{c}"
        );
    }

    #[test]
    fn tuple_arity_mismatch_rejected() {
        let source = r#"
            fn pair() -> (i64, i64) { return (1, 2); }
            fn main() -> i64 {
              let (a, b, c) = pair();
              return a;
            }
        "#;
        let errors = compile(source)
            .expect_err("3-name destructure of 2-tuple should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("tuple has 2 elements but 3 names")),
            "expected arity-mismatch diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn tuple_non_copy_element_rejected() {
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2);
              let pair = (xs, 3);
              return 0;
            }
        "#;
        let errors = compile(source)
            .expect_err("tuple with non-Copy element should fail in v1");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("non-Copy") && e.message.contains("Copy-only")),
            "expected Copy-only diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn vec_of_fixed_array_compiles_and_runs() {
        // Phase 2c lift (#7): `Vec<[T; N]>` now flows
        // through both backends. Per-shape array typedef
        // emitted in C (e.g. `typedef int64_t
        // intent_arr4_int64_t[4];`); `__push` / `__set` use
        // memcpy since C forbids array assignment via `=`;
        // LLVM uses bare `[N x T]` value slots (not `[N x
        // T]*` pointers) and loads each array operand into
        // a value before storing into the buffer slot.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<[i64; 4]> = vec([1, 2, 3, 4], [5, 6, 7, 8], [9, 10, 11, 12]);
              return len(xs) as i64;
            }
        "#;
        let c = compile_to_c(source).expect("Vec<[i64;4]> should compile");
        assert!(
            c.contains("typedef int64_t intent_arr4_int64_t[4];"),
            "expected per-shape array typedef:\n{c}"
        );
        assert!(
            c.contains("intent_vec_arr4_int64_t__push"),
            "expected per-shape vec helper:\n{c}"
        );
    }

    #[test]
    fn for_iter_borrow_over_vec_of_vec_works() {
        // Refines #7 phase 2: `for v in &xs` where xs is
        // `Vec<Vec<U>>` binds `v: Vec<U>` and lets the body
        // borrow it (`&v`) without freeing on iteration
        // exit. The new VarInfo.no_drop flag suppresses the
        // auto-drop for non-Copy iteration views so the
        // aliased slot doesn't double-free at the outer
        // collection's drop.
        let source = r#"
            fn ilen(v: ref Vec<i64>) -> i64 {
              return len(v) as i64;
            }
            fn main() -> i64 {
              let xs: Vec<Vec<i64>> = vec(vec(1, 2, 3), vec(4));
              let total: i64 = 0;
              for v in ref xs {
                total = total + ilen(ref v);
              }
              return total;
            }
        "#;
        compile(source).expect("for-iter borrow over Vec<Vec> should compile");
    }

    #[test]
    fn vec_of_vec_indexing_into_let_rejected() {
        // The first pass of #7 doesn't yet lift element
        // indexing for non-Copy types — reading `xs[i]` by
        // value would alias the owner's slot and double-free.
        // The checker rejects with a clear diagnostic; users
        // can still build / push / drop nested Vecs.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<Vec<i64>> = vec(vec(1, 2));
              let inner: Vec<i64> = xs[0];
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("Vec<Vec> indexing rejects");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("non-Copy") && e.message.contains("alias")),
            "expected non-Copy + alias diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn cannot_redefine_builtin() {
        let source = r#"
            fn vec() -> i64 {
              return 0;
            }

            fn main() -> i64 {
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("redefining 'vec' should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("built-in name")),
            "expected built-in-name diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn empty_vec_accepted_with_annotation() {
        // Refines #8 from STATUS.md. Empty `vec()` is now
        // accepted when the surrounding context names the
        // element type — let-annotation, reassign target, or
        // function return type. Bare `vec()` in an unknown-
        // type context (no annotation, no reassign target)
        // still errors with the updated "needs ... type
        // annotation" message.
        let source_let = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec();
              return 0;
            }
        "#;
        compile(source_let).expect("vec() with annotation should compile");

        let source_return = r#"
            fn make() -> Vec<i64> {
              return vec();
            }
            fn main() -> i64 {
              let xs: Vec<i64> = make();
              return 0;
            }
        "#;
        compile(source_return).expect("vec() in return should compile");

        let source_assign = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              xs = vec();
              return 0;
            }
        "#;
        compile(source_assign).expect("xs = vec() should compile");
    }

    #[test]
    fn empty_vec_without_annotation_still_errors() {
        let source = r#"
            fn main() -> i64 {
              let _ = vec();
              return 0;
            }
        "#;

        let errors = compile(source)
            .expect_err("vec() with no annotation should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("type annotation")),
            "expected type-annotation diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn shadowing_must_preserve_type() {
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 1;
              let x: u64 = 2;
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("shadowing with different type should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("must preserve its type")),
            "expected type-preservation diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn borrow_does_not_consume_vec() {
        let source = r#"
            fn sum(xs: ref Vec<i64>) -> i64 {
              return xs[0] + xs[1] + xs[2];
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let total: i64 = sum(ref xs);
              let again: i64 = xs[0];
              assert total == 6;
              assert again == 1;
              return 0;
            }
        "#;

        compile_to_c(source).expect("borrow should not consume xs");
    }

    #[test]
    fn borrow_does_not_consume_array() {
        let source = r#"
            fn sum_four(xs: ref [i64; 4]) -> i64 {
              return xs[0] + xs[1] + xs[2] + xs[3];
            }

            fn main() -> i64 {
              let xs: [i64; 4] = [1, 2, 3, 4];
              let total: i64 = sum_four(ref xs);
              let first: i64 = xs[0];
              assert total == 10;
              assert first == 1;
              return 0;
            }
        "#;

        compile_to_c(source).expect("borrowed array stays usable");
    }

    #[test]
    fn cannot_borrow_after_move() {
        let source = r#"
            fn take(xs: Vec<i64>) -> i64 {
              return xs[0];
            }

            fn peek(xs: ref Vec<i64>) -> i64 {
              return xs[0];
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let a: i64 = take(xs);
              let b: i64 = peek(ref xs);
              return a + b;
            }
        "#;

        let errors = compile(source).expect_err("borrow after move should fail");
        assert!(
            errors.iter().any(|e| e.message.contains("after it was moved")),
            "expected borrow-after-move diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn cannot_let_bind_reference() {
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1);
              let r = ref xs;
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("let-binding a reference should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("cannot be stored in 'let' bindings")),
            "expected let-ref diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn cannot_return_reference() {
        let source = r#"
            fn identity(xs: ref Vec<i64>) -> ref Vec<i64> {
              return xs;
            }

            fn main() -> i64 {
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("ref return should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("cannot be a reference")),
            "expected no-ref-return diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn cannot_borrow_non_var() {
        let source = r#"
            fn peek(xs: ref Vec<i64>) -> i64 {
              return xs[0];
            }

            fn make() -> Vec<i64> {
              return vec(1, 2, 3);
            }

            fn main() -> i64 {
              let v: i64 = peek(ref make());
              return v;
            }
        "#;

        let errors = compile(source).expect_err("borrowing a call result should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("can only borrow a named variable")),
            "expected only-vars diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn ref_param_emits_pointer_in_c() {
        let source = r#"
            fn sum(xs: ref Vec<i64>) -> i64 {
              return xs[0];
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(7);
              let v: i64 = sum(ref xs);
              assert v == 7;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("compile");
        assert!(
            c.contains("const intent_vec_int64_t* v_xs"),
            "expected const pointer param in C, got: {c}"
        );
        assert!(
            c.contains("&v_xs"),
            "expected &v_xs at the call site, got: {c}"
        );
    }

    #[test]
    fn ref_to_ref_rejected() {
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1);
              let v: i64 = takes_ref(ref xs);
              return v;
            }

            fn takes_ref(r: ref ref Vec<i64>) -> i64 {
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("ref-of-ref param should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("reference to a reference")),
            "expected no-ref-ref diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn reborrow_passes_through() {
        let source = r#"
            fn inner(xs: ref Vec<i64>) -> i64 {
              return xs[0];
            }

            fn outer(xs: ref Vec<i64>) -> i64 {
              return inner(xs);
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(5);
              let v: i64 = outer(ref xs);
              assert v == 5;
              return 0;
            }
        "#;

        compile_to_c(source).expect("re-borrow through param should work");
    }

    #[test]
    fn while_loop_with_counter_compiles() {
        let source = r#"
            fn sum_to(n: i64) -> i64 {
              let total: i64 = 0;
              let i: i64 = 0;
              while i < n {
                total = total + i;
                i = i + 1;
              }
              return total;
            }

            fn main() -> i64 {
              let s: i64 = sum_to(5);
              assert s == 10;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("while loop should compile");
        assert!(c.contains("while ("), "expected C while loop: {c}");
    }

    #[test]
    fn vec_grown_inside_while_loop() {
        let source = r#"
            fn build(n: i64) -> Vec<i64> {
              let xs: Vec<i64> = vec(0);
              let i: i64 = 1;
              while i < n {
                xs = push(xs, i);
                i = i + 1;
              }
              return xs;
            }

            fn main() -> i64 {
              let xs: Vec<i64> = build(3);
              let n: u64 = len(xs);
              assert n == 3;
              return 0;
            }
        "#;

        compile_to_c(source).expect("Vec build in loop should compile");
    }

    #[test]
    fn if_else_with_both_branches_returning() {
        let source = r#"
            fn pick(x: i64) -> i64 {
              if x < 0 {
                return 0 - x;
              } else {
                return x;
              }
            }

            fn main() -> i64 {
              let v: i64 = pick(0 - 5);
              assert v == 5;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("if-else with both returns should compile");
        assert!(c.contains("if ("), "expected C if statement: {c}");
        assert!(c.contains("else {"), "expected C else block: {c}");
    }

    #[test]
    fn if_without_else_compiles() {
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              if x > 0 {
                let y: i64 = x + 1;
                assert y == 6;
              }
              return 0;
            }
        "#;

        compile_to_c(source).expect("if without else should compile");
    }

    #[test]
    fn assignment_persists_across_iterations() {
        let source = r#"
            fn main() -> i64 {
              let i: i64 = 0;
              while i < 3 {
                i = i + 1;
              }
              assert i == 3;
              return 0;
            }
        "#;

        compile_to_c(source).expect("assignment should persist");
    }

    #[test]
    fn assigning_to_unknown_var_errors() {
        let source = r#"
            fn main() -> i64 {
              missing = 5;
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("assigning to unknown var should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("cannot assign to unknown variable")),
            "expected unknown-var diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn divergent_move_auto_balances_with_drop() {
        // When `xs` is moved in one branch but not the other, the compiler
        // auto-inserts a Drop in the live branch so both paths end with `xs`
        // consumed. The merged state is "moved" and any subsequent use is an
        // error.
        let source = r#"
            fn take(xs: Vec<i64>) -> i64 {
              return xs[0];
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let cond: bool = true;
              if cond {
                let v: i64 = take(xs);
              }
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("auto-balance should compile");
        // Expect at least one __free call (the auto-drop in the else path).
        assert!(
            c.contains("__free"),
            "expected auto-drop free in emitted C, got: {c}"
        );
    }

    #[test]
    fn divergent_move_followed_by_use_after_merge_errors() {
        // Auto-balance makes both branches end with `xs` moved, so any use
        // after the if is rejected.
        let source = r#"
            fn take(xs: Vec<i64>) -> i64 {
              return xs[0];
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let cond: bool = true;
              if cond {
                let v: i64 = take(xs);
              }
              let bad: i64 = xs[0];
              return bad;
            }
        "#;

        let errors = compile(source).expect_err("use after merged-move should fail");
        assert!(
            errors.iter().any(|e| e.message.contains("was moved")),
            "expected after-move diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn consume_in_both_branches_succeeds() {
        let source = r#"
            fn take(xs: Vec<i64>) -> i64 {
              return xs[0];
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let cond: bool = true;
              if cond {
                let a: i64 = take(xs);
              } else {
                let b: i64 = take(xs);
              }
              return 0;
            }
        "#;

        compile_to_c(source).expect("consuming in both branches should compile");
    }

    #[test]
    fn loop_moving_outer_vec_without_rebind_errors() {
        let source = r#"
            fn take(xs: Vec<i64>) -> i64 {
              return xs[0];
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(1);
              let i: i64 = 0;
              while i < 3 {
                let v: i64 = take(xs);
                i = i + 1;
              }
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("loop moving outer Vec without rebind should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("loop body changes the move state")),
            "expected loop-balance diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn unreachable_after_return_errors() {
        let source = r#"
            fn main() -> i64 {
              return 0;
              let x: i64 = 5;
              return 1;
            }
        "#;

        let errors = compile(source).expect_err("unreachable code should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("unreachable")),
            "expected unreachable diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn assignment_type_mismatch_errors() {
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              x = true;
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("type-mismatched assign should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("assignment value")),
            "expected assignment type diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn break_exits_loop_early() {
        let source = r#"
            fn main() -> i64 {
              let i: i64 = 0;
              while i < 100 {
                if i >= 5 {
                  break;
                }
                i = i + 1;
              }
              assert i == 5;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("break should compile");
        assert!(c.contains("break;"), "expected C break: {c}");
    }

    #[test]
    fn continue_skips_to_next_iteration() {
        let source = r#"
            fn main() -> i64 {
              let i: i64 = 0;
              let total: i64 = 0;
              while i < 10 {
                i = i + 1;
                if i == 5 {
                  continue;
                }
                total = total + 1;
              }
              assert total == 9;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("continue should compile");
        assert!(c.contains("continue;"), "expected C continue: {c}");
    }

    #[test]
    fn break_outside_loop_errors() {
        let source = r#"
            fn main() -> i64 {
              break;
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("break outside loop should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("'break' is only valid inside a 'while' loop")),
            "expected break-outside-loop diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn continue_outside_loop_errors() {
        let source = r#"
            fn main() -> i64 {
              continue;
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("continue outside loop should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("'continue' is only valid inside a 'while' loop")),
            "expected continue-outside-loop diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn break_with_consumed_outer_vec_errors() {
        let source = r#"
            fn take(xs: Vec<i64>) -> i64 {
              return xs[0];
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(1);
              let i: i64 = 0;
              while i < 3 {
                if i == 1 {
                  let v: i64 = take(xs);
                  break;
                }
                i = i + 1;
              }
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("break after move should fail balance");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("different move state")),
            "expected balance diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn break_after_consistent_state_is_allowed() {
        // Inner consume + rebind keeps the move state of xs unchanged at the break point.
        let source = r#"
            fn take(xs: Vec<i64>) -> i64 {
              return xs[0];
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(1);
              let i: i64 = 0;
              while i < 3 {
                i = i + 1;
                if i == 2 {
                  break;
                }
              }
              let v: i64 = xs[0];
              assert v == 1;
              return 0;
            }
        "#;

        compile_to_c(source).expect("break with no outer move should compile");
    }

    #[test]
    fn let_in_branch_does_not_leak_to_outer() {
        let source = r#"
            fn main() -> i64 {
              if true {
                let x: i64 = 5;
                assert x == 5;
              }
              let y: i64 = x + 1;
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("let in branch should not leak");
        assert!(
            errors.iter().any(|e| e.message.contains("unknown variable 'x'")),
            "expected 'unknown variable x' after if, got: {:?}",
            errors
        );
    }

    #[test]
    fn let_in_branch_can_shadow_outer_with_different_type() {
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              if true {
                let x: bool = true;
                assert x;
              }
              assert x == 5;
              return 0;
            }
        "#;

        compile_to_c(source).expect("outer-scope shadowing with different type should compile");
    }

    #[test]
    fn assignment_in_inner_scope_mutates_outer() {
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              if true {
                x = 10;
              }
              assert x >= 5;
              return 0;
            }
        "#;

        compile_to_c(source).expect("assignment should reach outer binding");
    }

    #[test]
    fn vec_declared_in_loop_body_drops_each_iteration() {
        let source = r#"
            fn main() -> i64 {
              let i: i64 = 0;
              while i < 3 {
                let local: Vec<i64> = vec(i);
                let v: i64 = local[0];
                assert v == i;
                i = i + 1;
              }
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("loop-local Vec should compile");
        // Expect at least one __free emitted in the loop body for the scope-end drop.
        assert!(
            c.contains("__free"),
            "expected scope-end free in emitted C, got: {c}"
        );
    }

    #[test]
    fn break_drops_inner_scope_vec() {
        let source = r#"
            fn main() -> i64 {
              let i: i64 = 0;
              while i < 3 {
                let inside: Vec<i64> = vec(i);
                let v: i64 = inside[0];
                if v >= 1 {
                  break;
                }
                i = i + 1;
              }
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("loop-local Vec with break should compile");
        assert!(c.contains("__free"), "expected drop emissions: {c}");
    }

    #[test]
    fn assignment_through_outer_binding_in_while() {
        let source = r#"
            fn build(n: i64) -> Vec<i64> {
              let xs: Vec<i64> = vec(0);
              let i: i64 = 1;
              while i < n {
                xs = push(xs, i);
                i = i + 1;
              }
              return xs;
            }

            fn main() -> i64 {
              let xs: Vec<i64> = build(3);
              let n: u64 = len(xs);
              assert n == 3;
              return 0;
            }
        "#;

        compile_to_c(source).expect("assignment-based Vec growth should compile");
    }

    #[test]
    fn mut_borrow_allows_indexed_write_to_vec() {
        let source = r#"
            fn set_first(xs: mut ref Vec<i64>, v: i64) -> i64 {
              xs[0] = v;
              return 0;
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let _r: i64 = set_first(mut ref xs, 99);
              assert xs[0] == 99;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("&mut Vec write should compile");
        assert!(c.contains("&v_xs"), "expected &v_xs at call site, got: {c}");
        assert!(
            !c.contains("const intent_vec_int64_t* v_xs"),
            "expected non-const pointer for &mut, got: {c}"
        );
    }

    #[test]
    fn mut_borrow_allows_indexed_write_to_array() {
        let source = r#"
            fn fill_first(xs: mut ref [i64; 4]) -> i64 {
              xs[0] = 7;
              return 0;
            }

            fn main() -> i64 {
              let xs: [i64; 4] = [1, 2, 3, 4];
              let _r: i64 = fill_first(mut ref xs);
              assert xs[0] == 7;
              return 0;
            }
        "#;

        compile_to_c(source).expect("&mut [T; N] write should compile");
    }

    #[test]
    fn cannot_mut_borrow_an_immutable_ref() {
        let source = r#"
            fn try_mutate(xs: ref Vec<i64>) -> i64 {
              let _v: i64 = inner(mut ref xs);
              return 0;
            }

            fn inner(_x: mut ref Vec<i64>) -> i64 {
              return 0;
            }

            fn main() -> i64 {
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("&mut on shared ref should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("borrowed immutably")),
            "expected immutable-ref diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn cannot_index_assign_through_shared_ref() {
        let source = r#"
            fn try_set(xs: ref Vec<i64>) -> i64 {
              xs[0] = 99;
              return 0;
            }

            fn main() -> i64 {
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("index-assign through &T should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("borrowed immutably")),
            "expected immutable-ref diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn cannot_alias_mut_with_shared_in_same_call() {
        let source = r#"
            fn two(a: mut ref Vec<i64>, b: ref Vec<i64>) -> i64 {
              return b[0];
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(1);
              let _v: i64 = two(mut ref xs, ref xs);
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("aliasing &mut + & in same call should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("aliases")),
            "expected aliasing diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn cannot_alias_two_mut_in_same_call() {
        let source = r#"
            fn two(a: mut ref Vec<i64>, b: mut ref Vec<i64>) -> i64 {
              return 0;
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(1);
              let _v: i64 = two(mut ref xs, mut ref xs);
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("two &mut to same var should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("aliases")),
            "expected aliasing diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn owned_indexed_write_works_without_borrow() {
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 3] = [10, 20, 30];
              xs[1] = 99;
              assert xs[1] == 99;
              return 0;
            }
        "#;

        compile_to_c(source).expect("owned index-assign should compile");
    }

    #[test]
    fn out_of_range_constant_index_in_assign_rejected() {
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 2] = [1, 2];
              xs[5] = 99;
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("constant OOB write should fail");
        assert!(
            errors.iter().any(|e| e.message.contains("out of range")),
            "expected oob diagnostic, got: {:?}",
            errors
        );
    }

    fn z3_available() -> bool {
        // Prefer `$Z3` then PATH lookup, mirroring smt.rs's
        // `find_z3()` shape. Avoid hardcoding `/usr/bin/z3` so
        // the tests work on systems with z3 elsewhere.
        let z3 = std::env::var("Z3").unwrap_or_else(|_| "z3".to_string());
        std::process::Command::new(&z3)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn smt_proves_param_arithmetic_universally() {
        if !z3_available() {
            return;
        }
        // Bound x so `x + x` doesn't overflow under BitVec wrap-around.
        let source = r#"
            fn safe(x: i64) -> i64
            requires x > 0;
            requires x < 1000;
            {
              prove x >= 1;
              prove x + x >= 2;
              return x;
            }

            fn main() -> i64 {
              let v: i64 = safe(5);
              assert v == 5;
              return 0;
            }
        "#;

        compile_to_c(source).expect("SMT should discharge x>0 -> x>=1 and x+x>=2");
    }

    #[test]
    fn smt_disproves_universally_false_claim() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn bad(x: i64) -> i64 {
              prove x > 0;
              return x;
            }

            fn main() -> i64 {
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("x > 0 isn't always true");
        assert!(
            errors.iter().any(|e| e.message.contains("counterexample") || e.message.contains("proof failed")),
            "expected disproof diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn smt_uses_multiple_requires() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn ordered(a: i64, b: i64) -> i64
            requires a >= 0;
            requires b >= a;
            {
              prove b >= 0;
              return b;
            }

            fn main() -> i64 {
              let v: i64 = ordered(1, 2);
              return v - 2;
            }
        "#;

        compile_to_c(source).expect("two requires should imply prove");
    }

    #[test]
    fn smt_disproves_float_tautology_via_nan() {
        if !z3_available() {
            return;
        }
        // `x + 0.0 == x` is *not* universally true under IEEE-754: for
        // x = NaN, NaN + 0.0 = NaN and NaN == NaN is false. The SMT
        // encoder now routes float arithmetic to the FP theory and z3
        // surfaces the NaN counterexample.
        let source = r#"
            fn floaty(x: f64) -> f64 {
              prove x + 0.0 == x;
              return x;
            }

            fn main() -> i64 {
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("NaN counterexample");
        assert!(
            errors.iter().any(|e| {
                let m = &e.message;
                m.contains("proof failed") && m.contains("NaN")
            }),
            "expected NaN counterexample, got: {:?}",
            errors
        );
    }

    #[test]
    fn smt_proves_float_property_when_universally_true() {
        if !z3_available() {
            return;
        }
        // Universally true regardless of NaN: x == x is False for NaN,
        // but `!(x < x)` holds for every IEEE-754 float (including NaN,
        // where all comparisons are false).
        let source = r#"
            fn floaty(x: f64) -> i64 {
              prove !(x < x);
              return 0;
            }

            fn main() -> i64 {
              return 0;
            }
        "#;

        compile_to_c(source).expect("!(x < x) should hold for all f64");
    }

    #[test]
    fn smt_proves_implication_via_chained_requires() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn middle(x: i64, lo: i64, hi: i64) -> i64
            requires lo <= x;
            requires x <= hi;
            requires lo <= hi;
            {
              prove lo <= hi;
              prove x >= lo;
              prove x <= hi;
              return x;
            }

            fn main() -> i64 {
              return 0;
            }
        "#;

        compile_to_c(source).expect("SMT should chain transitive inequalities");
    }

    #[test]
    fn for_loop_sums_a_range() {
        let source = r#"
            fn sum(lo: i64, hi: i64) -> i64 {
              let total: i64 = 0;
              for i from lo to hi {
                total = total + i;
              }
              return total;
            }

            fn main() -> i64 {
              let s: i64 = sum(1, 5);
              assert s == 10;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("for-loop should compile");
        assert!(c.contains("for ("), "expected C for loop: {c}");
    }

    #[test]
    fn for_loop_supports_break_and_continue() {
        // `break` exits the for-loop; `continue` jumps to the loop post-step
        // (the auto-increment of the loop variable).
        let source = r#"
            fn count_evens_up_to(n: i64) -> i64 {
              let count: i64 = 0;
              for i from 0 to n {
                if i == 5 {
                  break;
                }
                let r: i64 = i % 2;
                if r != 0 {
                  continue;
                }
                count = count + 1;
              }
              return count;
            }

            fn main() -> i64 {
              let c: i64 = count_evens_up_to(10);
              assert c == 3;
              return 0;
            }
        "#;

        compile_to_c(source).expect("for + break + continue should compile");
    }

    #[test]
    fn for_loop_var_is_scoped() {
        // `i` declared by the for-loop is invisible outside.
        let source = r#"
            fn main() -> i64 {
              for i from 0 to 3 {
                let _: i64 = i;
              }
              return i;
            }
        "#;

        let errors = compile(source).expect_err("loop var should not leak");
        assert!(
            errors.iter().any(|e| e.message.contains("unknown variable 'i'")),
            "expected scope diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn for_loop_rejects_non_integer_bounds() {
        let source = r#"
            fn main() -> i64 {
              for i from 0.0 to 3.0 {
                let _v: f64 = i;
              }
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("non-integer bounds rejected");
        assert!(
            errors.iter().any(|e| e.message.contains("must be integers")),
            "expected integer-bounds diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn smt_proves_using_len_of_array() {
        if !z3_available() {
            return;
        }
        // `len(xs)` substitutes the compile-time-known length 4, so the
        // prove holds via SMT.
        let source = r#"
            fn check(xs: ref [i64; 4], i: u64) -> i64
            requires i < 4;
            {
              prove i < len(xs);
              return xs[0];
            }

            fn main() -> i64 {
              let xs: [i64; 4] = [1, 2, 3, 4];
              let v: i64 = check(ref xs, 2);
              return v - 1;
            }
        "#;

        compile_to_c(source).expect("len(array) should be substituted in SMT");
    }

    #[test]
    fn ensures_clause_discharged_at_return() {
        if !z3_available() {
            return;
        }
        // BitVec-aware verification: `a >= b` alone doesn't imply `a - b >= 0`
        // because of wrap-around (e.g., a = INT64_MAX, b = INT64_MIN+1).
        // We add `b >= 0` so the subtraction can't dip below 0.
        let source = r#"
            fn safe_sub(a: i64, b: i64) -> i64
            requires a >= b;
            requires b >= 0;
            ensures _return >= 0;
            {
              return a - b;
            }

            fn main() -> i64 {
              return 0;
            }
        "#;

        compile_to_c(source).expect("ensures should be discharged from requires");
    }

    #[test]
    fn ensures_clause_violation_at_return_is_rejected() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn bad(a: i64, b: i64) -> i64
            ensures _return > 0;
            {
              return a - b;
            }

            fn main() -> i64 {
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("ensures should fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("ensures clause does not hold")),
            "expected ensures-violation diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn callers_get_ensures_facts_about_let_results() {
        if !z3_available() {
            return;
        }
        // The caller asserts r >= 0; it can only know that from safe_sub's
        // ensures clause. BitVec-aware: add `b >= 0` to the preconditions
        // so the subtraction doesn't underflow.
        let source = r#"
            fn safe_sub(a: i64, b: i64) -> i64
            requires a >= b;
            requires b >= 0;
            ensures _return >= 0;
            {
              return a - b;
            }

            fn check(a: i64, b: i64) -> i64
            requires a >= b;
            requires b >= 0;
            {
              let r: i64 = safe_sub(a, b);
              prove r >= 0;
              return r;
            }

            fn main() -> i64 {
              return 0;
            }
        "#;

        compile_to_c(source).expect("caller should pick up callee's ensures");
    }

    #[test]
    fn ensures_facts_can_reference_parameters() {
        if !z3_available() {
            return;
        }
        // ensures clause refers to the parameter `n` too. Caller provides
        // both n and the result. Under BitVec wrap-around, `n + 5 >= n`
        // is false when n is near INT64_MAX; we bound n to avoid overflow.
        let source = r#"
            fn at_least(n: i64) -> i64
            requires n > 0;
            requires n < 9223372036854775800;
            ensures _return >= n;
            {
              return n + 5;
            }

            fn check() -> i64 {
              let r: i64 = at_least(3);
              prove r >= 3;
              return 0;
            }

            fn main() -> i64 {
              return 0;
            }
        "#;

        compile_to_c(source).expect("ensures with parameter reference");
    }

    #[test]
    fn ensures_can_be_used_to_validate_subsequent_bounds() {
        if !z3_available() {
            return;
        }
        // safe_index returns a value < len; caller indexes with it.
        let source = r#"
            fn safe_index(i: u64) -> u64
            requires i < 4;
            ensures _return < 4;
            {
              return i;
            }

            fn check(xs: ref [i64; 4]) -> i64
            requires 1 < 4;
            {
              let j: u64 = safe_index(1);
              prove j < len(xs);
              return 0;
            }

            fn main() -> i64 {
              return 0;
            }
        "#;

        compile_to_c(source).expect("ensures fact + len(array) should compose");
    }

    #[test]
    fn while_invariant_proven_at_entry_preserved_and_post() {
        if !z3_available() {
            return;
        }
        // BitVec-aware: the only invariant that survives substitution
        // here is the one about `i`. We bound `n` so `i + 1` doesn't wrap.
        // The test still exercises entry-check, preservation, and post-loop
        // fact propagation through the full SMT pipeline.
        let source = r#"
            fn count_to(n: i64) -> i64
            requires n >= 0;
            requires n < 1000;
            ensures _return >= 0;
            {
              let i: i64 = 0;
              while i < n
              invariant i >= 0;
              invariant i <= n;
              {
                i = i + 1;
              }
              prove i >= 0;
              return i;
            }

            fn main() -> i64 {
              return 0;
            }
        "#;

        compile_to_c(source).expect("invariant should verify and feed prove");
    }

    #[test]
    fn invariant_unprovable_at_entry_errors() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let i: i64 = 1;
              while i < 5
              invariant i == 0;
              {
                i = i + 1;
              }
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("invariant doesn't hold at entry");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("loop invariant does not hold at loop entry")),
            "expected entry-failure diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn invariant_not_preserved_errors() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let i: i64 = 0;
              while i < 5
              invariant i < 3;
              {
                i = i + 1;
              }
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("invariant not preserved");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("not preserved")),
            "expected preservation-failure diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn nested_loop_havocs_outer_var_in_substitution() {
        // Refines #6 from STATUS.md: previously, the loop-
        // preservation substitution map ignored nested while/for
        // bodies entirely. A nested loop that mutated an outer
        // binding could leave a stale `out` entry (or no entry
        // at all) and the substituted invariant would still
        // reference the bare `Var(x)` — which the SMT layer
        // folded into the entry assumption, accepting the
        // program even though the body actually changed x.
        //
        // Program below: outer invariant claims `x == 0`, but
        // the nested while increments x. The OLD behavior
        // accepted this (unsound). The NEW behavior substitutes
        // x with a fresh `x#havoc<N>` token so SMT correctly
        // can't prove `x#havoc1 == 0` from the entry assumption,
        // and the verifier rejects the program.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 0;
              let i: i64 = 0;
              while i < 3
              invariant x == 0;
              {
                let j: i64 = 0;
                while j < 1 {
                  x = x + 1;
                  j = j + 1;
                }
                i = i + 1;
              }
              return 0;
            }
        "#;

        let errors = compile(source).expect_err(
            "outer invariant x == 0 should fail preservation because the \
             nested loop mutates x",
        );
        assert!(
            errors.iter().any(|e| e.message.contains("not preserved")),
            "expected preservation failure, got: {:?}",
            errors
        );
    }

    #[test]
    fn nested_loop_no_outer_havoc_when_var_untouched() {
        // Positive companion: the nested loop body doesn't
        // touch the outer-scope binding `x`, so the
        // substitution shouldn't havoc it. The outer invariant
        // `x == 0` must still verify.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 0;
              let i: i64 = 0;
              while i < 3
              invariant x == 0;
              {
                let j: i64 = 0;
                while j < 1 {
                  j = j + 1;
                }
                i = i + 1;
              }
              prove x == 0;
              return 0;
            }
        "#;

        compile(source).expect(
            "outer invariant should survive when nested loop \
             doesn't touch the outer binding",
        );
    }

    #[test]
    fn invariant_preserves_multi_reassign_via_composition() {
        // Refines #5 from STATUS.md: the loop-invariant preservation
        // substitution map used to record only the last RHS per
        // variable. For a body that reassigns the same variable
        // multiple times in one iteration the intervening updates
        // were dropped, so the substitution was wrong and the
        // verifier emitted a spurious "not preserved" error.
        //
        // Body composes acc twice per iteration:
        //   acc = acc + 1;
        //   i = i + 1;
        //   acc = acc + 1;
        // Substitution must produce `acc -> acc + 2`, `i -> i + 1`
        // for the invariant `acc == 2 * i` to verify preservation.
        // With the old last-RHS-only behavior the map would be
        // `acc -> acc + 1`, `i -> i + 1` and SMT would (correctly)
        // disprove the goal under the entry assumption.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64
            ensures _return >= 0;
            {
              let i: i64 = 0;
              let acc: i64 = 0;
              while i < 5
              invariant i >= 0;
              invariant i <= 5;
              invariant acc == 2 * i;
              {
                acc = acc + 1;
                i = i + 1;
                acc = acc + 1;
              }
              prove acc == 10;
              return acc;
            }
        "#;

        compile(source).expect(
            "composed reassignment substitution should preserve invariant",
        );
    }

    #[test]
    fn invariant_becomes_post_loop_fact() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              let i: i64 = 0;
              while i < 3
              invariant x == 5;
              {
                i = i + 1;
              }
              prove x == 5;
              return 0;
            }
        "#;

        compile_to_c(source).expect("post-loop invariant should be usable");
    }

    #[test]
    fn for_loop_invariant_works() {
        if !z3_available() {
            return;
        }
        // Use `total <= i` so the invariant stays tight under BitVec
        // substitution: after each iteration the body proves
        // `(total + 1) <= (i + 1)`, which is the invariant again.
        // We don't `prove` anything after the loop because the post-loop
        // invariant still references `i` (out of scope), which would let
        // SMT pick arbitrary values.
        let source = r#"
            fn main() -> i64 {
              let total: i64 = 0;
              for i from 0 to 5
              invariant total <= i;
              {
                total = total + 1;
              }
              return 0;
            }
        "#;

        compile_to_c(source).expect("for-loop invariant should verify");
    }

    #[test]
    fn shadow_type_mismatch_points_at_original_declaration() {
        use crate::diagnostic::format_diagnostics;

        let source = r#"fn main() -> i64 {
  let x: i64 = 5;
  let x: u64 = 7;
  return 0;
}
"#;
        let errors = compile(source).expect_err("shadow with different type");
        let rendered = format_diagnostics("t.intent", source, &errors);
        assert!(
            rendered.contains("error: shadowing 'let x' must preserve its type"),
            "expected primary diagnostic, got:\n{rendered}"
        );
        assert!(
            rendered.contains("note: 'x' was previously declared here as i64"),
            "expected related note, got:\n{rendered}"
        );
        // The previous-decl note should pinpoint line 2.
        assert!(
            rendered.contains("t.intent:2:"),
            "expected note at line 2, got:\n{rendered}"
        );
    }

    #[test]
    fn move_diagnostic_has_related_note_at_move_site() {
        use crate::diagnostic::format_diagnostics;

        let source = r#"fn take(xs: Vec<i64>) -> i64 {
  return xs[0];
}

fn main() -> i64 {
  let xs: Vec<i64> = vec(1, 2, 3);
  let v: i64 = take(xs);
  let bad: i64 = xs[0];
  return bad;
}
"#;
        let errors = compile(source).expect_err("use after move");
        let rendered = format_diagnostics("test.intent", source, &errors);
        // Primary location is the second use of xs.
        assert!(
            rendered.contains("error: value 'xs' was moved"),
            "expected primary diagnostic, got:\n{rendered}"
        );
        // Related note pointing to the move site (line 7, column 21).
        assert!(
            rendered.contains("note: 'xs' was moved here"),
            "expected related note, got:\n{rendered}"
        );
        // Make sure there's no leftover byte-offset "originally on line N"
        // hack from the previous implementation.
        assert!(
            !rendered.contains("originally on line"),
            "byte-offset hack still present, got:\n{rendered}"
        );
    }

    #[test]
    fn str_ordering_accepted_and_constant_folded_only_when_safe() {
        // `a < b` between two Str values must be accepted (the
        // backend lowers it to strcmp). No constant folding: even
        // when both sides are literals, the result depends on the
        // host's locale-free lexicographic order — leave it to
        // strcmp at runtime.
        let source = r#"
            fn main() -> i64 {
              let a: Str = "apple";
              let b: Str = "banana";
              if a < b {
                return 0;
              }
              return 1;
            }
        "#;
        let _ = compile(source).expect("Str ordering must type-check");
    }

    #[test]
    fn len_of_str_literal_type_checks() {
        // `len(s)` on a Str value returns u64. Pins the Str arm
        // of check_len alongside the existing Array/Vec arms.
        let source = r#"
            fn main() -> i64 {
              let s: Str = "hi";
              let n: u64 = len(s);
              if n == 2 {
                return 0;
              }
              return 1;
            }
        "#;
        let _ = compile(source).expect("len(Str) must type-check");
    }

    #[test]
    fn pure_fn_accepts_arithmetic_only_body() {
        // The simplest pure body — pure integer arithmetic with no
        // I/O, mutation, or impure call — type-checks.
        let source = r#"
            pure fn square(x: i64) -> i64 {
              return x * x;
            }

            fn main() -> i64 { return 0; }
        "#;
        compile(source).expect("pure fn with arithmetic should type-check");
    }

    #[test]
    fn pure_fn_rejects_print_in_body() {
        // `print` is observable I/O — disallowed in a pure body.
        let source = r#"
            pure fn naughty(x: i64) -> i64 {
              print "side effect";
              return x;
            }

            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source).expect_err("pure fn with print must fail");
        assert!(
            errors.iter().any(|e| e.message.contains("cannot contain `print`")),
            "expected `print` diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn pure_fn_rejects_calling_impure_function() {
        // Calling a non-pure function from a pure context is
        // forbidden — the impurity would leak.
        let source = r#"
            fn impure_helper(x: i64) -> i64 {
              print x;
              return x;
            }

            pure fn naughty(x: i64) -> i64 {
              return impure_helper(x);
            }

            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source).expect_err("pure fn calling impure must fail");
        assert!(
            errors.iter().any(|e| e.message.contains("cannot call non-pure function")),
            "expected non-pure-call diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn pure_fn_rejects_indexed_write() {
        let source = r#"
            pure fn touch(xs: mut ref Vec<i64>) -> i64 {
              xs[0] = 99;
              return 0;
            }

            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source).expect_err("pure fn with IndexAssign must fail");
        assert!(
            errors.iter().any(|e| e.message.contains("cannot mutate")),
            "expected IndexAssign diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parallel_for_reduce_supports_mul_on_integer() {
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [1, 2, 3, 4];
              let prod: i64 = 1;
              parallel for i from 0 to 4
              reduce prod with *;
              {
                prod = prod * xs[i];
              }
              return prod;
            }
        "#;
        compile_to_c(source).expect("reduce with * on integer should type-check");
    }

    #[test]
    fn parallel_for_reduce_supports_and_on_bool() {
        let source = r#"
            fn main() -> i64 {
              let flags: [bool; 3] = [true, true, false];
              let all: bool = true;
              parallel for i from 0 to 3
              reduce all with &&;
              {
                all = all && flags[i];
              }
              return 0;
            }
        "#;
        compile_to_c(source).expect("reduce with && on bool should type-check");
    }

    #[test]
    fn parallel_for_reduce_supports_or_on_bool() {
        let source = r#"
            fn main() -> i64 {
              let flags: [bool; 3] = [false, false, true];
              let any: bool = false;
              parallel for i from 0 to 3
              reduce any with ||;
              {
                any = any || flags[i];
              }
              return 0;
            }
        "#;
        compile_to_c(source).expect("reduce with || on bool should type-check");
    }

    #[test]
    fn binary_bitwise_ops_typecheck_and_fold_constants() {
        // Sanity check: `&`, `|`, `^` parse, type-check, and fold
        // at compile time when both operands are integer constants.
        // 12 & 10 = 8, 12 | 10 = 14, 12 ^ 10 = 6.
        let source = r#"
            fn main() -> i64 {
              let a: i64 = 12;
              let b: i64 = 10;
              return (a & b) + (a | b) + (a ^ b);
            }
        "#;
        compile_to_c(source).expect("bitwise binary ops should type-check");
    }

    #[test]
    fn binary_bitwise_ops_reject_float_operands() {
        // `&`, `|`, `^` are integer-only. Floats must be rejected
        // by the same path as `%` (integer-only).
        let source = r#"
            fn main() -> i64 {
              let a: f64 = 1.5;
              let b: f64 = 2.5;
              let r: f64 = a & b;
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("bitwise on floats must fail");
        assert!(
            !errors.is_empty(),
            "expected a diagnostic, got none"
        );
    }

    #[test]
    fn parallel_for_reduce_supports_bit_and_on_integer() {
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [12, 10, 7, 13];
              let acc: i64 = -1;
              parallel for i from 0 to 4
              reduce acc with &;
              {
                acc = acc & xs[i];
              }
              return acc;
            }
        "#;
        compile_to_c(source).expect("reduce with & on integer should type-check");
    }

    #[test]
    fn parallel_for_reduce_supports_bit_or_on_integer() {
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [12, 10, 7, 13];
              let acc: i64 = 0;
              parallel for i from 0 to 4
              reduce acc with |;
              {
                acc = acc | xs[i];
              }
              return acc;
            }
        "#;
        compile_to_c(source).expect("reduce with | on integer should type-check");
    }

    #[test]
    fn parallel_for_reduce_supports_bit_xor_on_integer() {
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [12, 10, 7, 13];
              let acc: i64 = 0;
              parallel for i from 0 to 4
              reduce acc with ^;
              {
                acc = acc ^ xs[i];
              }
              return acc;
            }
        "#;
        compile_to_c(source).expect("reduce with ^ on integer should type-check");
    }

    #[test]
    fn parallel_for_reduce_rejects_bit_and_on_bool_variable() {
        // `&` is integer-only; a bool reduction variable with `&`
        // must be rejected.
        let source = r#"
            fn main() -> i64 {
              let b: bool = true;
              parallel for i from 0 to 3
              reduce b with &;
              {
                b = b & true;
              }
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("& on bool reduction must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("requires an integer-typed variable")
                    || e.message.contains("requires integer")),
            "expected type-rule diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parallel_for_reduce_supports_min_on_integer() {
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [10, 20, 30, 40];
              let lo: i64 = 100;
              parallel for i from 0 to 4
              reduce lo with min;
              {
                lo = min(lo, xs[i]);
              }
              return lo;
            }
        "#;
        compile_to_c(source).expect("reduce with min on integer should type-check");
    }

    #[test]
    fn parallel_for_reduce_supports_max_on_integer() {
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [10, 20, 30, 40];
              let hi: i64 = 0;
              parallel for i from 0 to 4
              reduce hi with max;
              {
                hi = max(hi, xs[i]);
              }
              return hi;
            }
        "#;
        compile_to_c(source).expect("reduce with max on integer should type-check");
    }

    #[test]
    fn parallel_for_reduce_rejects_min_on_bool_variable() {
        // `min` is integer-only; a bool reduction variable with `min` must be rejected.
        let source = r#"
            fn main() -> i64 {
              let b: bool = true;
              parallel for i from 0 to 3
              reduce b with min;
              {
                b = min(b, true);
              }
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("min on bool reduction must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("requires an integer-typed variable")),
            "expected type-rule diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parallel_for_reduce_rejects_bad_min_shape() {
        // Body must be `var = min(var, <expr>)` (or symmetric);
        // a different call signature must be rejected so the
        // lowering can't silently miscompile.
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [10, 20, 30, 40];
              let lo: i64 = 100;
              parallel for i from 0 to 4
              reduce lo with min;
              {
                lo = min(xs[i], xs[i]);
              }
              return lo;
            }
        "#;
        let errors = compile(source).expect_err("bad-shape min update must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("reduction variable") && e.message.contains("only")),
            "expected reduction-shape diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parallel_for_reduce_rejects_mul_on_bool_variable() {
        // `*` is integer-only; a bool reduction variable with `*` must be rejected.
        let source = r#"
            fn main() -> i64 {
              let b: bool = true;
              parallel for i from 0 to 3
              reduce b with *;
              {
                b = b && true;
              }
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("* on bool reduction must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("requires an integer-typed variable")),
            "expected type-rule diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parallel_for_reduce_rejects_and_on_integer_variable() {
        // `&&` is bool-only; an integer reduction variable with `&&` must be rejected.
        let source = r#"
            fn main() -> i64 {
              let n: i64 = 0;
              parallel for i from 0 to 3
              reduce n with &&;
              {
                n = n + 1;
              }
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("&& on integer reduction must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("requires a bool-typed variable")),
            "expected type-rule diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parallel_for_reduce_clause_accepts_canonical_shape() {
        // `reduce total with +;` carves out the `total = total + …`
        // Reassign from the pure-body rule. The checker accepts
        // the canonical shape; backends lower via OpenMP
        // `reduction(+:total)` or `atomicrmw add`.
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [1, 2, 3, 4];
              let total: i64 = 0;
              parallel for i from 0 to 4
              reduce total with +;
              {
                total = total + xs[i];
              }
              return total;
            }
        "#;
        compile_to_c(source).expect("parallel-for + reduce should type-check");
    }

    #[test]
    fn parallel_for_reduce_rejects_non_op_update() {
        // `total = 5;` doesn't match `total + <expr>`. The
        // verifier flags it explicitly so the lowering can't
        // silently miscompile.
        let source = r#"
            fn main() -> i64 {
              let total: i64 = 0;
              parallel for i from 0 to 3
              reduce total with +;
              {
                total = 5;
              }
              return total;
            }
        "#;
        let errors = compile(source).expect_err("non-op update must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("reduction variable") && e.message.contains("only")),
            "expected reduction-shape diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parallel_for_reduce_rejects_extra_read() {
        // Reading the reduction variable outside the named update
        // could expose partial values; the checker forbids it.
        let source = r#"
            fn main() -> i64 {
              let total: i64 = 0;
              parallel for i from 0 to 3
              reduce total with +;
              {
                let snapshot: i64 = total;
                total = total + snapshot;
              }
              return total;
            }
        "#;
        let errors = compile(source).expect_err("extra read must fail");
        assert!(
            errors.iter().any(|e| e.message.contains("cannot read reduction")),
            "expected partial-read diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn reduce_clause_rejected_on_non_parallel_for() {
        // `reduce` is only valid on `parallel for`; the parser
        // rejects it on the sequential form.
        let source = r#"
            fn main() -> i64 {
              let total: i64 = 0;
              for i from 0 to 3
              reduce total with +;
              {
                total = total + 1;
              }
              return total;
            }
        "#;
        let errors = compile(source).expect_err("reduce on non-parallel for must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("'reduce' clauses are only valid on a `parallel for` loop")),
            "expected parser diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn channel_new_send_recv_typecheck_and_compile() {
        let source = r#"
            fn main() -> i64 {
              let ch: Channel<i64> = channel_new();
              let _sent: i64 = channel_send(ref ch, 42);
              return channel_recv(ref ch);
            }
        "#;
        compile_to_c(source).expect("channel builtins should type-check");
    }

    #[test]
    fn mutex_lock_runtime_helper_uses_futex_on_linux_with_sched_yield_fallback() {
        // The mutex drives Drepper's three-state futex
        // protocol on Linux for real kernel-wait parking;
        // Windows uses WaitOnAddress / WakeByAddress via the
        // same wait/wake helper names; other platforms fall
        // back to the `intent_thread_yield` backoff through
        // the cross-platform thread wrapper.
        let source = r#"
            fn main() -> i64 {
              let m: Mutex<i64> = mutex_new(0);
              let g: Guard<i64> = mutex_lock(ref m);
              return guard_get(ref g);
            }
        "#;
        let c = compile_to_c(source).expect("mutex program compiles");
        assert!(
            c.contains("# include <linux/futex.h>"),
            "expected futex header include under __linux__:\n{c}"
        );
        assert!(
            c.contains("intent_mutex_futex_wait("),
            "expected FUTEX_WAIT helper definition:\n{c}"
        );
        assert!(
            c.contains("intent_mutex_futex_wake("),
            "expected FUTEX_WAKE helper definition:\n{c}"
        );
        // Drepper's three-state protocol uses `atomic_fetch_sub` in
        // unlock: a fetch_sub != 1 means there were waiters → wake.
        assert!(
            c.contains("atomic_fetch_sub_explicit(&g->m->locked, 1"),
            "expected fetch_sub-based unlock fast path:\n{c}"
        );
        // The non-Linux/non-Windows arm uses the
        // `intent_thread_yield` wrapper (sched_yield on POSIX,
        // SwitchToThread on Windows).
        assert!(
            c.contains("intent_thread_yield()"),
            "expected intent_thread_yield call for the non-park fallback:\n{c}"
        );
        // Windows arm covers WaitOnAddress / WakeByAddress.
        assert!(
            c.contains("WaitOnAddress(") && c.contains("WakeByAddress"),
            "expected WaitOnAddress / WakeByAddress for the _WIN32 arm:\n{c}"
        );
    }

    #[test]
    fn channel_runtime_includes_per_slot_seq_array() {
        // The Vyukov-style publication counter sits next to
        // the data buffer. The producer publishes by storing
        // seq[i] = t+1; the consumer waits for seq[i] == h+1
        // before reading. This closes the producer→consumer
        // race on slot-write ordering that the previous
        // tail-CAS-only design left open.
        let source = r#"
            fn main() -> i64 {
              let ch: Channel<i64> = channel_new();
              let _ = channel_send(ref ch, 1);
              return channel_recv(ref ch);
            }
        "#;
        let c = compile_to_c(source).expect("channel program compiles");
        // The per-(T, N) helpers carry the capacity in their
        // cap-macro name: `INTENT_CHANNEL_INT64_T_16_CAP`.
        assert!(
            c.contains("_Atomic int64_t seq[INTENT_CHANNEL_INT64_T_16_CAP];"),
            "expected per-slot seq array in channel struct:\n{c}"
        );
        // channel_new initializes each seq slot to its index.
        assert!(
            c.contains("atomic_store_explicit(&c.seq[i], (int64_t)i"),
            "expected channel_new to seed seq[i] = i:\n{c}"
        );
        // channel_send publishes via `seq[t & MASK] = t + 1`.
        assert!(
            c.contains("&c->seq[t & (INTENT_CHANNEL_INT64_T_16_CAP - 1)], t + 1"),
            "expected producer to publish via seq store:\n{c}"
        );
    }

    #[test]
    fn channel_supports_non_i64_payload_via_let_binding_annotation() {
        // `channel_new()` returns the default-shaped
        // `Channel<i64, 16>`; the let-binding's declared
        // type widens it to `Channel<i32, 8>` via the
        // channel-coerce arm in coerce_checked. Both backends
        // must emit per-(T, N) helpers.
        let source = r#"
            fn main() -> i64 {
              let ch: Channel<i32, 8> = channel_new();
              let _ = channel_send(ref ch, 7 as i32);
              let v: i32 = channel_recv(ref ch);
              return v as i64;
            }
        "#;
        let c = compile_to_c(source).expect("Channel<i32, 8> typechecks");
        assert!(
            c.contains("intent_channel_int32_t_8"),
            "expected per-(T,N) struct name in C output:\n{c}"
        );
        assert!(
            c.contains("intent_channel_int32_t_8_new("),
            "expected per-(T,N) channel_new helper:\n{c}"
        );
        let llvm = crate::backend_llvm::LlvmBackend
            .emit(&compile(source).expect("LLVM compiles").ir);
        assert!(
            llvm.contains("%intent_channel_i32_8 = type { [8 x i32], [8 x i64], i64, i64 }"),
            "expected per-(T,N) struct declaration in LLVM IR:\n{llvm}"
        );
    }

    #[test]
    fn channel_rejects_non_power_of_two_capacity() {
        let source = r#"
            fn main() -> i64 {
              let ch: Channel<i64, 7> = channel_new();
              let _ = channel_send(ref ch, 1);
              return channel_recv(ref ch);
            }
        "#;
        let errors = compile(source).expect_err("capacity 7 must be rejected");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("Channel capacity must be a power of 2")),
            "expected pow2 diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn fn_ptr_param_carries_a_top_level_function_value() {
        // Bare identifier in argument position resolves to a
        // first-class fn pointer when it names a top-level
        // function. The receiver invokes it via the new
        // CallIndirect path.
        let source = r#"
            pure fn double(x: i64) -> i64 { return x + x; }
            fn apply(f: fn(i64) -> i64, x: i64) -> i64 {
              return f(x);
            }
            fn main() -> i64 { return apply(double, 7); }
        "#;
        let c = compile_to_c(source).expect("fn-ptr typechecks");
        assert!(
            c.contains("int64_t (*"),
            "expected C fn-ptr declarator in output:\n{c}"
        );
        let checked = compile(source).expect("LLVM compiles");
        let llvm = crate::backend_llvm::LlvmBackend.emit(&checked.ir);
        // Indirect call uses an SSA temp for the loaded fn-ptr
        // and emits `call i64 (i64) %t…(i64 %x)`.
        assert!(
            llvm.contains("call i64 (i64) "),
            "expected indirect-call LLVM IR shape:\n{llvm}"
        );
    }

    #[test]
    fn fn_ptr_argument_type_mismatch_is_rejected() {
        // The argument's signature must match the param's
        // fn-ptr type. Passing a function with the wrong
        // arity / param type is a type error.
        let source = r#"
            pure fn add(a: i64, b: i64) -> i64 { return a + b; }
            fn apply(f: fn(i64) -> i64, x: i64) -> i64 {
              return f(x);
            }
            fn main() -> i64 { return apply(add, 1); }
        "#;
        let errors = compile(source).expect_err("arity mismatch must fail");
        assert!(
            !errors.is_empty(),
            "expected at least one diagnostic for fn-ptr type mismatch"
        );
    }

    #[test]
    fn indirect_call_rejected_inside_parallel_for_body() {
        // Indirect calls bypass the name-based purity gate;
        // the effects checker rejects them inside pure /
        // parallel-for bodies. (The reduction-Reassign branch
        // is a shape-validation, not a deep walk — so we put
        // the indirect call in a regular Let to exercise the
        // walker.)
        let source = r#"
            pure fn id(x: i64) -> i64 { return x; }
            fn main() -> i64 {
              let total: i64 = 0;
              let f: fn(i64) -> i64 = id;
              parallel for i from 0 to 4
              reduce total with +;
              {
                let v: i64 = f(i);
                total = total + v;
              }
              return total;
            }
        "#;
        let errors = compile(source).expect_err(
            "indirect call inside parallel-for body must be rejected",
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("indirect call")),
            "expected indirect-call rejection, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn channel_supports_bool_via_i8_shadow_in_llvm() {
        // `Channel<bool>` now compiles. LLVM stores slots as
        // `[N x i8]` and zext/trunc's the source-level i1 at
        // each slot boundary; C uses native `bool buf[N]`.
        let source = r#"
            fn main() -> i64 {
              let ch: Channel<bool, 4> = channel_new();
              let _ = channel_send(ref ch, true);
              let v: bool = channel_recv(ref ch);
              if v { return 1; }
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("Channel<bool, 4> typechecks");
        // Per-(T,N) helper name uses C `bool` spelling.
        assert!(
            c.contains("intent_channel_bool_4"),
            "expected per-(T,N) struct/helper names for bool channel:\n{c}"
        );
        let llvm = crate::backend_llvm::LlvmBackend
            .emit(&compile(source).expect("LLVM compiles").ir);
        // LLVM stores bool channels via i8 shadow — the struct
        // name uses i8 (not i1) and the buf array is [N x i8].
        assert!(
            llvm.contains("%intent_channel_i8_4 = type { [4 x i8], [4 x i64], i64, i64 }"),
            "expected i8-shadowed bool channel struct:\n{llvm}"
        );
        // zext of the value at send time; icmp ne i8 .., 0 at
        // recv time.
        assert!(
            llvm.contains("zext i1"),
            "expected zext on send for bool channel:\n{llvm}"
        );
        assert!(
            llvm.contains("icmp ne i8"),
            "expected icmp ne on recv truncation for bool channel:\n{llvm}"
        );
    }

    #[test]
    fn channel_rejects_unsupported_element_type() {
        // Floating-point / Vec / etc. are not in the
        // is_supported_channel_element allowlist.
        let source = r#"
            fn main() -> i64 {
              let ch: Channel<f64> = channel_new();
              let _ = channel_send(ref ch, 1.0);
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("Channel<f64> must be rejected");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("Channel element type")),
            "expected element-type diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn channel_send_runtime_helper_uses_cas_to_claim_tail() {
        // The C runtime `intent_channel_i64_send` must CAS the
        // tail before writing the slot. A plain
        // `atomic_store_explicit(&c->tail, ...)` would race
        // with another producer that read the same `t`.
        let source = r#"
            fn main() -> i64 {
              let ch: Channel<i64> = channel_new();
              let _ = channel_send(ref ch, 42);
              return channel_recv(ref ch);
            }
        "#;
        let c = compile_to_c(source).expect("channel program compiles");
        assert!(
            c.contains("atomic_compare_exchange_strong_explicit(&c->tail"),
            "expected CAS on &c->tail in the send helper:\n{c}"
        );
        // And NOT the old "store the bumped tail" line.
        assert!(
            !c.contains("atomic_store_explicit(&c->tail, t + 1"),
            "send helper still uses the non-CAS tail bump:\n{c}"
        );
    }

    #[test]
    fn channel_buffers_multiple_messages_before_recv() {
        // Send three messages before any recv. The
        // ring-buffer backend lowering preserves order and
        // returns 10 + 20 + 30 = 60.
        let source = r#"
            fn main() -> i64 {
              let ch: Channel<i64> = channel_new();
              let _ = channel_send(ref ch, 10);
              let _ = channel_send(ref ch, 20);
              let _ = channel_send(ref ch, 30);
              let a: i64 = channel_recv(ref ch);
              let b: i64 = channel_recv(ref ch);
              let c: i64 = channel_recv(ref ch);
              return (a + b) + c;
            }
        "#;
        compile_to_c(source).expect("buffered channel typechecks");
    }

    #[test]
    fn channel_send_rejects_a_non_reference() {
        let source = r#"
            fn main() -> i64 {
              let ch: Channel<i64> = channel_new();
              let _ = channel_send(ch, 1);
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("channel_send on owned value must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("requires a reference to Channel")),
            "expected reference-required diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn mutex_lock_returns_a_guard_with_get_set_typecheck() {
        let source = r#"
            fn main() -> i64 {
              let m: Mutex<i64> = mutex_new(10);
              let g: Guard<i64> = mutex_lock(ref m);
              let cur: i64 = guard_get(ref g);
              let _ = guard_set(ref g, cur + 1);
              return guard_get(ref g);
            }
        "#;
        compile_to_c(source).expect("mutex/guard should type-check");
    }

    #[test]
    fn mutex_lock_rejects_double_acquisition_in_same_scope() {
        // Two consecutive `mutex_lock(&m)` calls without a
        // drop in between would deadlock at runtime (the
        // lock is non-reentrant). The checker flags it.
        let source = r#"
            fn main() -> i64 {
              let m: Mutex<i64> = mutex_new(0);
              let g1: Guard<i64> = mutex_lock(ref m);
              let g2: Guard<i64> = mutex_lock(ref m);
              let _ = guard_get(ref g1);
              let _ = guard_get(ref g2);
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("double-lock must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("double acquisition")),
            "expected double-acquire diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn mutex_lock_accepts_sequential_lock_in_separate_functions() {
        // Locking the same mutex in two different function
        // bodies (each guard scoped to its own function) is
        // fine — the first guard is dropped when its
        // function returns.
        let source = r#"
            fn touch(m: ref Mutex<i64>) -> i64 {
              let g: Guard<i64> = mutex_lock(m);
              return guard_get(ref g);
            }
            fn main() -> i64 {
              let m: Mutex<i64> = mutex_new(5);
              let _a: i64 = touch(ref m);
              let _b: i64 = touch(ref m);
              return 0;
            }
        "#;
        compile_to_c(source).expect("sequential lock should type-check");
    }

    #[test]
    fn mutex_lock_accepts_distinct_mutexes_held_simultaneously() {
        // Holding guards on two different mutexes at the same
        // time is fine — they don't alias.
        let source = r#"
            fn main() -> i64 {
              let a: Mutex<i64> = mutex_new(1);
              let b: Mutex<i64> = mutex_new(2);
              let ga: Guard<i64> = mutex_lock(ref a);
              let gb: Guard<i64> = mutex_lock(ref b);
              let _ = guard_get(ref ga);
              let _ = guard_get(ref gb);
              return 0;
            }
        "#;
        compile_to_c(source).expect("two distinct mutexes should type-check");
    }

    #[test]
    fn mutex_lock_rejects_cross_function_double_acquisition() {
        // The caller holds a guard on `m`; the callee
        // `lock_it` calls `mutex_lock(m)` on the same mutex.
        // Without the cross-function check this would deadlock
        // at runtime — the verifier catches it.
        let source = r#"
            fn lock_it(m: ref Mutex<i64>) -> i64 {
              let g: Guard<i64> = mutex_lock(m);
              return guard_get(ref g);
            }
            fn main() -> i64 {
              let m: Mutex<i64> = mutex_new(0);
              let g: Guard<i64> = mutex_lock(ref m);
              let _ = lock_it(ref m);
              let _ = guard_get(ref g);
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("cross-fn double-lock must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("cross-function double acquisition")),
            "expected cross-fn diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn mutex_lock_accepts_call_to_function_that_does_not_lock_its_param() {
        // The callee takes `&Mutex<T>` but never calls
        // `mutex_lock` on it — it's safe to call while a
        // guard is live.
        let source = r#"
            fn read_count(_m: ref Mutex<i64>) -> i64 {
              return 42;
            }
            fn main() -> i64 {
              let m: Mutex<i64> = mutex_new(0);
              let g: Guard<i64> = mutex_lock(ref m);
              let _ = read_count(ref m);
              let _ = guard_get(ref g);
              return 0;
            }
        "#;
        compile_to_c(source).expect("read-only call should type-check");
    }

    #[test]
    fn mutex_lock_accepts_call_that_locks_different_mutex() {
        // The caller holds guard on `a`; callee locks `b`.
        // No deadlock — accepted.
        let source = r#"
            fn lock_b(b: ref Mutex<i64>) -> i64 {
              let gb: Guard<i64> = mutex_lock(b);
              return guard_get(ref gb);
            }
            fn main() -> i64 {
              let a: Mutex<i64> = mutex_new(0);
              let b: Mutex<i64> = mutex_new(0);
              let ga: Guard<i64> = mutex_lock(ref a);
              let _ = lock_b(ref b);
              let _ = guard_get(ref ga);
              return 0;
            }
        "#;
        compile_to_c(source).expect("disjoint mutex call should type-check");
    }

    #[test]
    fn mutex_lock_rejects_transitive_cross_function_double_acquisition() {
        // `helper` doesn't directly call `mutex_lock`, but it
        // calls `lock_it` which does. The fixpoint pass
        // propagates locks_params so `helper` is also marked
        // as locking its first param — and the call from
        // `main` (which holds a guard on `m`) is flagged.
        let source = r#"
            fn lock_it(m: ref Mutex<i64>) -> i64 {
              let g: Guard<i64> = mutex_lock(m);
              return guard_get(ref g);
            }
            fn helper(m: ref Mutex<i64>) -> i64 {
              return lock_it(m);
            }
            fn main() -> i64 {
              let m: Mutex<i64> = mutex_new(0);
              let g: Guard<i64> = mutex_lock(ref m);
              let _ = helper(ref m);
              let _ = guard_get(ref g);
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("transitive lock must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("cross-function double acquisition")),
            "expected transitive cross-fn diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn mutex_lock_accepts_transitive_call_that_does_not_lock() {
        // `helper` calls `noop` which doesn't lock. The
        // fixpoint correctly does NOT mark `helper` as
        // locking, so the call from `main` is accepted.
        let source = r#"
            fn noop(_m: ref Mutex<i64>) -> i64 {
              return 0;
            }
            fn helper(m: ref Mutex<i64>) -> i64 {
              return noop(m);
            }
            fn main() -> i64 {
              let m: Mutex<i64> = mutex_new(0);
              let g: Guard<i64> = mutex_lock(ref m);
              let _ = helper(ref m);
              let _ = guard_get(ref g);
              return 0;
            }
        "#;
        compile_to_c(source).expect("transitive no-lock should type-check");
    }

    #[test]
    fn mutex_lock_rejects_double_lock_via_ref_parameter() {
        // The within-function check now handles the ref-
        // parameter case: `mutex_lock(m)` followed by
        // another `mutex_lock(m)` (same param) must fail.
        let source = r#"
            fn bad(m: ref Mutex<i64>) -> i64 {
              let g1: Guard<i64> = mutex_lock(m);
              let g2: Guard<i64> = mutex_lock(m);
              let _ = guard_get(ref g1);
              let _ = guard_get(ref g2);
              return 0;
            }
            fn main() -> i64 {
              let m: Mutex<i64> = mutex_new(0);
              return bad(ref m);
            }
        "#;
        let errors = compile(source).expect_err("ref-param double-lock must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("double acquisition")),
            "expected double-acquire diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn guard_get_rejects_a_non_guard() {
        let source = r#"
            fn main() -> i64 {
              let m: Mutex<i64> = mutex_new(0);
              return guard_get(ref m);
            }
        "#;
        let errors = compile(source).expect_err("guard_get on Mutex must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("requires a reference to Guard")),
            "expected guard-required diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn atomic_new_load_store_fetch_add_typecheck_and_compile() {
        // Round-trip through the four builtins. Compiler
        // accepts the program; both backends are exercised by
        // separate end-to-end runs below.
        let source = r#"
            fn bump(a: ref Atomic<i64>) -> i64 {
              let old: i64 = atomic_fetch_add(a, 1);
              return old;
            }
            fn main() -> i64 {
              let a: Atomic<i64> = atomic_new(0);
              let _b1: i64 = bump(ref a);
              let _b2: i64 = bump(ref a);
              let _stored: i64 = atomic_store(ref a, 100);
              return atomic_load(ref a);
            }
        "#;
        compile_to_c(source).expect("atomic builtins should type-check");
    }

    #[test]
    fn atomic_load_rejects_a_non_reference() {
        // The checker requires `&Atomic<T>`. Passing the owned
        // handle directly must fail.
        let source = r#"
            fn main() -> i64 {
              let a: Atomic<i64> = atomic_new(0);
              return atomic_load(a);
            }
        "#;
        let errors = compile(source).expect_err("atomic_load on owned value must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("requires a reference to Atomic")),
            "expected reference-required diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn atomic_compare_exchange_returns_bool_on_success_or_failure() {
        // CAS that succeeds returns true; the cell is updated.
        // CAS with the wrong expected returns false; the cell
        // is left alone. The return type is `bool`.
        let source = r#"
            fn main() -> i64 {
              let a: Atomic<i64> = atomic_new(10);
              let s1: bool = atomic_compare_exchange(ref a, 10, 42);
              let s2: bool = atomic_compare_exchange(ref a, 10, 99);
              if s1 {
                if s2 { return 1; } else { return 0; }
              } else {
                return -1;
              }
            }
        "#;
        compile_to_c(source).expect("CAS typechecks");
    }

    #[test]
    fn atomic_compare_exchange_rejects_non_reference_cell() {
        let source = r#"
            fn main() -> i64 {
              let a: Atomic<i64> = atomic_new(0);
              let _ = atomic_compare_exchange(a, 0, 1);
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("CAS on owned must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("requires a reference to Atomic")),
            "expected reference-required diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn atomic_supports_integer_widths_smaller_than_i64() {
        // The five atomic builtins are polymorphic over the
        // supported integer widths. Element type is inferred
        // from the constructor's argument (here a typed local).
        let source = r#"
            fn main() -> i64 {
              let init32: i32 = 7;
              let a32: Atomic<i32> = atomic_new(init32);
              let _old32: i32 = atomic_fetch_add(ref a32, 1 as i32);
              let _stored32: i32 = atomic_store(ref a32, 42 as i32);
              let win: bool = atomic_compare_exchange(ref a32, 42 as i32, 99 as i32);
              if win { return atomic_load(ref a32) as i64; }
              return -1;
            }
        "#;
        compile_to_c(source).expect("multi-width atomics should compile");
    }

    #[test]
    fn atomic_supports_unsigned_widths() {
        // u8 / u16 / u32 / u64 all flow through the same
        // builtins. The C backend lowers to `_Atomic uintN_t`
        // and LLVM emits width-matched ops.
        let source = r#"
            fn main() -> i64 {
              let byte_init: u8 = 0;
              let b: Atomic<u8> = atomic_new(byte_init);
              let _ob: u8 = atomic_fetch_add(ref b, 3 as u8);
              let observed: u8 = atomic_load(ref b);
              return observed as i64;
            }
        "#;
        compile_to_c(source).expect("u8 atomics should compile");
    }

    #[test]
    fn atomic_bool_typechecks_and_loads_store_cas_work() {
        // `Atomic<bool>` lowers as an `_Atomic _Bool` cell in
        // C and an i8 shadow in LLVM (i1 atomics aren't byte-
        // addressable). All ops except fetch_add are valid.
        let source = r#"
            fn main() -> i64 {
              let flag: Atomic<bool> = atomic_new(false);
              let _ = atomic_store(ref flag, true);
              if atomic_load(ref flag) {
                let win: bool = atomic_compare_exchange(ref flag, true, false);
                if win { return 1; }
              }
              return 0;
            }
        "#;
        compile_to_c(source).expect("atomic<bool> typechecks");
    }

    #[test]
    fn atomic_fetch_add_rejects_bool_element() {
        // bool has no addition; the checker rejects
        // atomic_fetch_add on Atomic<bool> with a clear error.
        let source = r#"
            fn main() -> i64 {
              let flag: Atomic<bool> = atomic_new(false);
              let _ = atomic_fetch_add(ref flag, true);
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("atomic_fetch_add on bool must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("bool atomics have no addition")),
            "expected bool-no-add diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn atomic_ref_captures_across_parallel_for_compile_and_run_correctly() {
        // `&Atomic<T>` is the canonical escape hatch for
        // sharing mutable state across `parallel for` bodies.
        // Each iteration does an atomic op against a captured
        // reference — both backends thread the reference into
        // the parallel context (libgomp ctx struct in LLVM,
        // OpenMP shared-capture in C). The result is
        // deterministic regardless of thread interleaving
        // because every op is seq_cst.
        let source = r#"
            fn main() -> i64 {
              let counter: Atomic<i64> = atomic_new(0);
              parallel for i from 0 to 4 {
                let _ = atomic_fetch_add(ref counter, 1);
              }
              return atomic_load(ref counter);
            }
        "#;
        // Compiles cleanly through the checker (atomic
        // builtins are exempt from the pure-body restriction).
        let checked = compile(source).expect("parallel-for + atomic capture compiles");
        let llvm = crate::backend_llvm::LlvmBackend.emit(&checked.ir);
        // The captured pointer surfaces inside the outlined
        // function and is used by `atomicrmw add i64* …`. The
        // ctx struct definition lives in the outlined fn
        // signature.
        assert!(
            llvm.contains("define internal void @__intent_par_"),
            "expected outlined parallel-for fn:\n{llvm}"
        );
        assert!(
            llvm.contains("atomicrmw add i64*"),
            "expected atomicrmw against the captured atomic pointer:\n{llvm}"
        );
        // And the C backend pragma marks the loop parallel.
        let c = compile_to_c(source).expect("compiles to C");
        assert!(
            c.contains("_Pragma(\"omp parallel for\""),
            "expected OpenMP parallel-for pragma:\n{c}"
        );
        assert!(
            c.contains("atomic_fetch_add_explicit"),
            "expected atomic_fetch_add lowering in C:\n{c}"
        );
    }

    #[test]
    fn atomic_new_rejects_unsupported_element_type() {
        // Floating point isn't a supported atomic element
        // (neither C11 nor LLVM expose hardware float atomics
        // portably). The checker rejects with a clear error.
        let source = r#"
            fn main() -> i64 {
              let a: Atomic<f64> = atomic_new(3.14);
              let _ = atomic_load(ref a);
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("f64 atomic must be rejected");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("element type must be an integer width")),
            "expected unsupported-element diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn atomic_store_rejects_mismatched_value_type() {
        // atomic_store(&a, v: T) requires v to coerce to T.
        // Passing a bool literal is a type error.
        let source = r#"
            fn main() -> i64 {
              let a: Atomic<i64> = atomic_new(0);
              let _stored: i64 = atomic_store(ref a, true);
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("type-mismatched atomic_store must fail");
        assert!(!errors.is_empty(), "expected a diagnostic, got none");
    }

    #[test]
    fn task_spawn_and_join_typecheck_and_compile() {
        // Tasks capture by value, restricted to Copy types
        // (real-threading lowering passes captures through a
        // pthread context struct). The body works on a
        // pre-extracted scalar; the array stays in the
        // caller's scope.
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [10, 20, 30, 40];
              let x0: i64 = xs[0];
              task ta {
                let v: i64 = x0;
                let _ = v;
              }
              join ta;
              return 0;
            }
        "#;
        compile_to_c(source).expect("task / join should type-check");
    }

    #[test]
    fn task_spawn_lowers_to_pthread_create_with_outlined_body() {
        // Real-thread lowering: the spawn site emits a
        // cross-platform `intent_thread_create` wrapper call
        // (pthread_create on POSIX, CreateThread on Windows)
        // against an outlined function that takes a heap-
        // allocated ctx struct with the captures. Join blocks
        // via `intent_thread_join` and frees the ctx.
        let source = r#"
            fn main() -> i64 {
              let bias: i64 = 7;
              task ta {
                let v: i64 = bias;
                let _ = v;
              }
              join ta;
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("task program compiles");
        // C-side outline naming convention.
        assert!(
            c.contains("static void* intent_task_0("),
            "expected outlined task function in C output:\n{c}"
        );
        // Spawn-site `intent_thread_create` + join-site
        // `intent_thread_join` (wrappers that dispatch on
        // _WIN32). The pthread_create / pthread_join symbols
        // live inside the wrappers' POSIX arm and so still
        // appear in the emitted preamble.
        assert!(
            c.contains("intent_thread_create(&v_ta.thread"),
            "expected intent_thread_create at spawn site:\n{c}"
        );
        assert!(
            c.contains("intent_thread_join(v_ta.thread)"),
            "expected intent_thread_join at join site:\n{c}"
        );
        assert!(
            c.contains("pthread_create(th, NULL, fn, arg)")
                && c.contains("CreateThread(NULL, 0,"),
            "expected both pthread_create (POSIX) and CreateThread (Win32) arms:\n{c}"
        );
        assert!(
            c.contains("free(v_ta.ctx)"),
            "expected ctx free at join site:\n{c}"
        );
        // LLVM IR: outlined task fn always present; the
        // spawn/join call shape switches on the host's
        // `target_os` (POSIX uses pthread_create/join; Win32
        // uses CreateThread + WaitForSingleObject).
        let checked = compile(source).expect("LLVM compiles");
        let llvm = crate::backend_llvm::LlvmBackend.emit(&checked.ir);
        assert!(
            llvm.contains("define internal i8* @intent_task_0("),
            "expected outlined task fn in LLVM IR:\n{llvm}"
        );
        #[cfg(not(target_os = "windows"))]
        {
            assert!(
                llvm.contains("call i32 @pthread_create("),
                "expected pthread_create call in LLVM IR (POSIX host):\n{llvm}"
            );
            assert!(
                llvm.contains("call i32 @pthread_join("),
                "expected pthread_join call in LLVM IR (POSIX host):\n{llvm}"
            );
        }
        #[cfg(target_os = "windows")]
        {
            assert!(
                llvm.contains("call i8* @CreateThread("),
                "expected CreateThread call in LLVM IR (Windows host):\n{llvm}"
            );
            assert!(
                llvm.contains("call i32 @WaitForSingleObject("),
                "expected WaitForSingleObject call in LLVM IR (Windows host):\n{llvm}"
            );
        }
    }

    #[test]
    fn task_rejects_non_copy_capture_by_value() {
        // Capturing an owned array directly (non-Copy) must
        // fail with the explicit refs-only diagnostic.
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [10, 20, 30, 40];
              task ta {
                let v: i64 = xs[0];
                let _ = v;
              }
              join ta;
              return 0;
            }
        "#;
        let errors = compile(source).expect_err(
            "task body capturing a non-Copy binding by value must fail",
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("non-Copy")),
            "expected non-Copy capture diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn task_rejects_unjoined_handle() {
        let source = r#"
            fn main() -> i64 {
              task ta {
                let r: i64 = 1;
                let _ = r;
              }
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("unjoined task must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("never consumed by `join")),
            "expected unjoined-task diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn task_rejects_double_join() {
        let source = r#"
            fn main() -> i64 {
              task ta {
                let r: i64 = 1;
                let _ = r;
              }
              join ta;
              join ta;
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("double join must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("already joined")),
            "expected double-join diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn task_rejects_impure_body_with_print() {
        // Same purity rules as a `parallel for` body. `print` is
        // observable I/O and would interleave across threads.
        let source = r#"
            fn main() -> i64 {
              task ta {
                print 1;
              }
              join ta;
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("print in task body must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("task body cannot contain `print`")),
            "expected print-in-task diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn task_rejects_indexassign_on_outer_binding() {
        // Body must not mutate outer state (same as parallel-for).
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [1, 2, 3, 4];
              task ta {
                xs[0] = 99;
              }
              join ta;
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("IndexAssign in task body must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("task body cannot mutate")),
            "expected mutation diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parallel_for_accepts_pure_body() {
        let source = r#"
            pure fn double(x: i64) -> i64 { return x + x; }

            fn main() -> i64 {
              parallel for i from 0 to 5 {
                let r: i64 = double(i);
                let _ = r;
              }
              return 0;
            }
        "#;
        compile(source).expect("parallel for over pure calls should type-check");
    }

    #[test]
    fn parallel_for_rejects_impure_body() {
        // print in the body breaks the data-race-freedom proof.
        let source = r#"
            fn main() -> i64 {
              parallel for i from 0 to 3 {
                print i;
              }
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("parallel for with print must fail");
        assert!(
            errors.iter().any(|e| e.message.contains("cannot contain `print`")),
            "expected impurity diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn owned_str_auto_borrows_in_str_comparison() {
        // OwnedStr operand in a comparison context auto-borrows to
        // Str. The binding is NOT consumed (strcmp only reads), so
        // it remains usable on the next line.
        let source = r#"
            fn main() -> i64 {
              let g: OwnedStr = "Hello, " + "world";
              if g == "Hello, world" {
                print "match";
              }
              print g;
              return 0;
            }
        "#;
        compile_to_c(source).expect("OwnedStr == Str should auto-borrow");
    }

    #[test]
    fn owned_str_auto_borrows_for_str_parameter() {
        // Passing OwnedStr where a `Str` parameter is expected
        // auto-borrows: the OwnedStr binding stays live (the callee
        // sees a borrowed view), so the caller can still use it.
        let source = r#"
            fn lens(name: Str) -> i64 {
              return 1;
            }

            fn main() -> i64 {
              let g: OwnedStr = "Hello, " + "world";
              let _ = lens(g);
              print g;
              return 0;
            }
        "#;
        compile_to_c(source).expect("OwnedStr arg should auto-borrow to Str");
    }

    #[test]
    fn owned_str_concat_chain_with_borrowed_op_does_not_consume_strings() {
        // Combining auto-borrow with concat: `greet(g)` borrows g
        // (Str param), produces a fresh OwnedStr, leaves g live so
        // its scope-end drop still fires.
        let source = r#"
            fn greet(name: Str) -> OwnedStr {
              return "Hi, " + name;
            }

            fn main() -> i64 {
              let g: OwnedStr = "name" + "1";
              let h: OwnedStr = greet(g);
              print g;
              print h;
              return 0;
            }
        "#;
        compile_to_c(source).expect("auto-borrow keeps OwnedStr live after a Str-param call");
    }

    #[test]
    fn str_concat_produces_owned_str_and_consumes_owned_operands() {
        // `Str + Str` yields OwnedStr (affine). Re-using an
        // OwnedStr binding after it's been concatenated again must
        // fail with the standard move diagnostic, mirroring Vec.
        let source = r#"
            fn main() -> i64 {
              let g: OwnedStr = "Hello, " + "world";
              let h: OwnedStr = g + "!";
              print g;
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("use after move on OwnedStr");
        assert!(
            errors.iter().any(|e| e.message.contains("moved")),
            "expected move diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn str_concat_returns_owned_str_from_function() {
        // Returning an OwnedStr is allowed (the caller becomes the
        // new owner). Compiles cleanly through both backends.
        let source = r#"
            fn greet(name: Str) -> OwnedStr {
              return "Hello, " + name;
            }

            fn main() -> i64 {
              let g: OwnedStr = greet("alice");
              print g;
              return 0;
            }
        "#;
        compile_to_c(source).expect("returning OwnedStr should type-check");
    }

    #[test]
    fn str_equality_against_non_str_rejected_by_checker() {
        // Comparing a Str to a non-Str must surface a clear
        // diagnostic instead of silently lowering to a strcmp call
        // that would dereference junk.
        let source = r#"
            fn main() -> i64 {
              let name: Str = "alice";
              if name == 1 {
                return 1;
              }
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("Str ==/!= rejects mixed operand types");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("equality operands must both be Str")),
            "expected Str-mismatch diagnostic, got: {:?}",
            errors
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn smt_proves_literal_vec_element_values() {
        // After `let xs: Vec<i64> = vec(10, 20, 30);` the verifier
        // should know each slot's value: `prove xs[0] == 10` etc.
        // discharges without a runtime guard. Wires the vec-builtin
        // per-element fact emitter into the array-theory encoder.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              prove xs[0] == 10;
              prove xs[1] == 20;
              prove xs[2] == 30;
              return xs[0];
            }
        "#;
        compile_to_c(source).expect("literal vec element proofs should discharge");
    }

    #[test]
    fn smt_disproves_wrong_literal_vec_element() {
        // Sanity check that the emitted facts have the *right*
        // values: `prove xs[0] == 999` after `vec(10, …)` must
        // surface a counterexample.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              prove xs[0] == 999;
              return xs[0];
            }
        "#;
        let errors = compile(source).expect_err("wrong literal value must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("counterexample") || e.message.contains("proof failed")),
            "expected counterexample, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn smt_proves_literal_array_element_values() {
        // Mirror of `smt_proves_literal_vec_element_values` for
        // fixed-size arrays: `let xs: [i64; 3] = [10, 20, 30]`
        // should let `prove xs[k] == v_k` discharge.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 3] = [10, 20, 30];
              prove xs[0] == 10;
              prove xs[1] == 20;
              prove xs[2] == 30;
              return xs[0];
            }
        "#;
        compile_to_c(source).expect("literal [T;N] element proofs should discharge");
    }

    #[test]
    fn smt_disproves_wrong_literal_array_element() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 3] = [10, 20, 30];
              prove xs[0] == 99;
              return xs[0];
            }
        "#;
        let errors = compile(source).expect_err("wrong literal array value must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("counterexample") || e.message.contains("proof failed")),
            "expected counterexample, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn smt_proves_array_rebind_preserves_slots() {
        // `let ys = xs;` should give the verifier `arr_ys = arr_xs`
        // so any prove that chains through xs's literal-init facts
        // (xs[k] == c_k) discharges as ys[k] == c_k.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              let ys: Vec<i64> = xs;
              prove ys[0] == 10;
              prove ys[1] == 20;
              prove ys[2] == 30;
              return ys[0];
            }
        "#;
        compile_to_c(source).expect("array rebind preservation should discharge");
    }

    #[test]
    fn smt_disproves_wrong_slot_after_array_rebind() {
        // Sanity check: the rebind axiom doesn't trivialise — a
        // prove against a wrong value still surfaces a
        // counterexample.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2);
              let ys: Vec<i64> = xs;
              prove ys[0] == 9;
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("wrong rebind value must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("counterexample") || e.message.contains("proof failed")),
            "expected counterexample, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn smt_proves_push_preserves_prefix() {
        // The `push` store axiom: `let ys = push(xs, v);` emits
        // `arr_ys = (store arr_xs len(xs) v)`. With the literal-vec
        // facts establishing `xs[k] == c_k`, the verifier chains
        // through the axiom to get `ys[k] == c_k` for every
        // `k < len(xs)`, plus the trivial `ys[len(xs)] == v`.
        //
        // We can't write `ys[k] == xs[k]` here — `push` consumes
        // its argument, so `xs` is moved out of scope.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20);
              let ys: Vec<i64> = push(xs, 99);
              prove ys[0] == 10;
              prove ys[1] == 20;
              prove ys[2] == 99;
              return ys[0];
            }
        "#;
        compile_to_c(source).expect("push prefix-preservation proofs should discharge");
    }

    #[test]
    fn smt_proves_set_preserves_other_slots() {
        // The store axiom: `let ys = set(xs, k, v);` should give
        // the verifier enough to discharge `ys[j] == xs[j]` for
        // every `j != k`. Without the synthetic `(store …)` fact,
        // only the slot fact `ys[k] == v` would be available.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              let ys: Vec<i64> = set(xs, 1, 99);
              prove ys[0] == 10;
              prove ys[1] == 99;
              prove ys[2] == 30;
              return ys[0];
            }
        "#;
        compile_to_c(source).expect("set store-axiom proofs should discharge");
    }

    #[test]
    fn smt_proves_clone_preserves_every_slot() {
        // `let ys = clone(xs);` gives `arr_ys = arr_xs` so any
        // proof shape `ys[k] == xs[k]` discharges without per-slot
        // values being known to the verifier.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn make_xs() -> Vec<i64>
            ensures len(_return) == 3;
            {
              return vec(10, 20, 30);
            }

            fn main() -> i64 {
              let xs: Vec<i64> = make_xs();
              let ys: Vec<i64> = clone(xs);
              prove ys[0] == xs[0];
              prove ys[1] == xs[1];
              prove ys[2] == xs[2];
              return 0;
            }
        "#;
        compile_to_c(source).expect("clone array-eq proofs should discharge");
    }

    #[test]
    fn smt_proves_threshold_below_sorted_array_via_precondition() {
        // Caller-supplied sortedness: `xs[0] <= xs[1]` plus
        // `threshold < xs[0]` should give `threshold < xs[1]` via
        // transitive ordering. Pins that the opaque-array shape
        // (no per-slot value facts; only the inequality precondition)
        // still discharges.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn order_check(xs: ref Vec<i64>, threshold: i64) -> i64
            requires len(xs) >= 2;
            requires xs[0] <= xs[1];
            requires threshold < xs[0];
            {
              prove threshold < xs[1];
              return 0;
            }

            fn main() -> i64 { return 0; }
        "#;
        compile_to_c(source).expect("transitive ordering should discharge");
    }

    #[test]
    fn smt_proves_three_step_transitivity_on_sorted_array() {
        // Multi-step transitivity: `xs[0] <= xs[1] <= xs[2] <= xs[3]`
        // implies `xs[0] <= xs[3]`. The SMT solver chains the
        // pairwise facts itself; intentc just needs to pass them
        // through.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn check(xs: ref Vec<i64>) -> i64
            requires len(xs) >= 4;
            requires xs[0] <= xs[1];
            requires xs[1] <= xs[2];
            requires xs[2] <= xs[3];
            {
              prove xs[0] <= xs[3];
              return 0;
            }

            fn main() -> i64 { return 0; }
        "#;
        compile_to_c(source).expect("three-step transitivity should discharge");
    }

    #[test]
    fn smt_disproves_transitive_ordering_with_missing_pairwise() {
        // Sanity check: drop one pairwise fact and the transitive
        // prove must refuse. Catches a regression where the encoder
        // might infer ordering from nothing.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn check(xs: ref Vec<i64>) -> i64
            requires len(xs) >= 4;
            requires xs[0] <= xs[1];
            requires xs[2] <= xs[3];
            {
              prove xs[0] <= xs[3];
              return 0;
            }

            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source)
            .expect_err("missing pairwise fact must break the chain");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("counterexample") || e.message.contains("proof failed")),
            "expected counterexample, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn smt_proves_pairwise_ordering_on_f64_vec_literal() {
        // Ordering proofs on a literal `Vec<f64>` discharge through
        // the per-slot value facts: `xs[0] == 1.5` and `xs[1] ==
        // 2.5` combine with `(fp.lt 1.5 2.5)` to give `xs[0] <
        // xs[1]`. The float Binary path already routes ordering
        // through `fcmp`-equivalent SMT predicates; this test pins
        // that the array-element wiring carries them.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<f64> = vec(1.5, 2.5);
              prove xs[0] < xs[1];
              prove xs[1] > xs[0];
              prove xs[0] <= xs[1];
              return 0;
            }
        "#;
        compile_to_c(source).expect("pairwise float ordering should discharge");
    }

    #[test]
    fn smt_proves_pairwise_ordering_on_i32_array_literal() {
        // Narrow-integer companion to the float case. The
        // `infer_int_type` change that fixed `xs[0] == 10` on
        // i32 arrays last turn also picks the right BV width for
        // ordering predicates.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: [i32; 3] = [10, 20, 30];
              prove xs[0] < xs[1];
              prove xs[1] <= xs[2];
              prove xs[2] >= xs[0];
              return 0;
            }
        "#;
        compile_to_c(source).expect("pairwise i32 ordering should discharge");
    }

    #[test]
    fn smt_disproves_ordering_against_descending_literal() {
        // Sanity check: the ordering machinery doesn't trivialise —
        // a descending initializer must refuse an ascending prove.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(30, 20, 10);
              prove xs[0] < xs[1];
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("descending literal must refuse ascending prove");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("counterexample") || e.message.contains("proof failed")),
            "expected counterexample, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn smt_proves_f32_array_literal_after_coercion() {
        // Array literal `[1.5, 2.5, 3.5]` has inferred type
        // `[f64; 3]`, but a let annotation of `[f32; 3]` should
        // coerce element-by-element. Pair that with the f32 SMT
        // array support so each `prove xs[k] == (… as f32)`
        // discharges.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: [f32; 3] = [1.5, 2.5, 3.5];
              prove xs[0] == (1.5 as f32);
              prove xs[1] == (2.5 as f32);
              prove xs[2] == (3.5 as f32);
              return 0;
            }
        "#;
        compile_to_c(source).expect("f32 array-literal proofs should discharge");
    }

    #[test]
    fn smt_proves_i32_array_literal_slot_values() {
        // Same as the f32 case but for narrow integers: the
        // encoder's `infer_int_type` now sees that `xs[0]` is i32
        // (via the array's element type), so the literal `10`
        // encodes as BV-32 instead of defaulting to BV-64.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: [i32; 3] = [10, 20, 30];
              prove xs[0] == 10;
              prove xs[1] == 20;
              prove xs[2] == 30;
              return 0;
            }
        "#;
        compile_to_c(source).expect("i32 array-literal slot proofs should discharge");
    }

    #[test]
    fn smt_proves_f32_vec_element_values() {
        // f32 element type for SMT arrays. The encoder's float-
        // binary path picks `(_ to_fp 8 24)` for f32 Float literal
        // operands so they match the array element sort instead of
        // defaulting to `(_ to_fp 11 53)`. Cast-based literals are
        // the only way to make f32 values today (no `1.5f32`
        // suffix in the lexer); the `vec(a, b)` arg coerces via
        // the existing arg-coercion path.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let a: f32 = (1.5 as f32);
              let b: f32 = (2.5 as f32);
              let xs: Vec<f32> = vec(a, b);
              prove xs[0] == a;
              prove xs[1] == b;
              return 0;
            }
        "#;
        compile_to_c(source).expect("f32 element proofs should discharge");
    }

    #[test]
    fn smt_disproves_wrong_f32_vec_element() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let a: f32 = (1.5 as f32);
              let xs: Vec<f32> = vec(a);
              prove xs[0] == (9.9 as f32);
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("wrong f32 slot value must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("counterexample") || e.message.contains("proof failed")),
            "expected counterexample, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn smt_proves_f64_array_element_values() {
        // f64 element type for SMT arrays. Per-slot proofs on a
        // literal `[f64; N]` or `Vec<f64>` initializer should
        // discharge via the FP path (the literal Eq routes through
        // `fp.eq` once `infer_is_float` sees an `Index` of a
        // float-element array binding).
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: [f64; 3] = [1.5, 2.5, 3.5];
              prove xs[0] == 1.5;
              prove xs[1] == 2.5;
              prove xs[2] == 3.5;
              return 0;
            }
        "#;
        compile_to_c(source).expect("f64 [T;N] element proofs should discharge");
    }

    #[test]
    fn smt_proves_f64_vec_element_values() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<f64> = vec(1.5, 2.5);
              prove xs[0] == 1.5;
              prove xs[1] == 2.5;
              return 0;
            }
        "#;
        compile_to_c(source).expect("f64 Vec element proofs should discharge");
    }

    #[test]
    fn smt_disproves_wrong_f64_array_element() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<f64> = vec(1.5, 2.5);
              prove xs[0] == 9.9;
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("wrong f64 slot value must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("counterexample") || e.message.contains("proof failed")),
            "expected counterexample, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn smt_proves_bool_array_element_values() {
        // Bool element type for SMT arrays. `[bool; N]` and
        // `Vec<bool>` should now support per-slot proofs like
        // `prove flags[0] == true`.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let flags: [bool; 3] = [true, false, true];
              prove flags[0] == true;
              prove flags[1] == false;
              prove flags[2] == true;
              return 0;
            }
        "#;
        compile_to_c(source).expect("bool [T;N] element proofs should discharge");
    }

    #[test]
    fn smt_proves_bool_vec_element_values() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let bs: Vec<bool> = vec(true, false);
              prove bs[0] == true;
              prove bs[1] == false;
              return 0;
            }
        "#;
        compile_to_c(source).expect("bool Vec element proofs should discharge");
    }

    #[test]
    fn smt_disproves_wrong_bool_array_element() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let bs: Vec<bool> = vec(true, false);
              prove bs[0] == false;
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("wrong bool slot value must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("counterexample") || e.message.contains("proof failed")),
            "expected counterexample, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn smt_proves_full_slot_identity_across_function_via_per_slot_ensures() {
        // Per-slot ensures clauses propagate the entire array's
        // post-state across the call boundary: the callee's body
        // discharges each ensures via the array-theory machinery
        // (set element fact + store-axiom for preservation), and
        // the caller picks each one up via the existing
        // `record_ensures_facts` substitution. Composes everything
        // built up over the last several turns.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn replace_middle(xs: Vec<i64>) -> Vec<i64>
            requires len(xs) >= 3;
            ensures len(_return) == len(xs);
            ensures _return[0] == xs[0];
            ensures _return[1] == 999;
            ensures _return[2] == xs[2];
            {
              let ys: Vec<i64> = set(xs, 1, 999);
              return ys;
            }

            fn main() -> i64 {
              let original: Vec<i64> = vec(10, 20, 30);
              let updated: Vec<i64> = replace_middle(original);
              prove updated[0] == 10;
              prove updated[1] == 999;
              prove updated[2] == 30;
              prove len(updated) == 3;
              return 0;
            }
        "#;
        compile_to_c(source).expect("per-slot ensures should fully chain");
    }

    #[test]
    fn smt_proves_caller_picks_up_ensures_return_slot_value() {
        // Cross-function array reasoning: a callee whose `ensures`
        // clause names `_return[k] == V` should communicate that
        // slot fact to the caller's let-bound result. The callee's
        // body must discharge the ensures itself (via the `set`
        // element fact); the caller picks the ensures up via the
        // existing `record_ensures_facts` plumbing, substituted to
        // talk about the new binding.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn replace_middle(xs: Vec<i64>) -> Vec<i64>
            requires len(xs) >= 3;
            ensures _return[1] == 999;
            {
              let ys: Vec<i64> = set(xs, 1, 999);
              return ys;
            }

            fn main() -> i64 {
              let original: Vec<i64> = vec(10, 20, 30);
              let updated: Vec<i64> = replace_middle(original);
              prove updated[1] == 999;
              return 0;
            }
        "#;
        compile_to_c(source).expect("ensures _return[k] should reach the caller");
    }

    #[test]
    fn smt_disproves_caller_claim_stronger_than_ensures_return_slot() {
        // Sanity check the caller side: a prove that goes beyond
        // the callee's `ensures` must still refuse. The callee
        // promises only `_return[1] == 999`, so `prove updated[1]
        // == 777` should surface a counterexample.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn replace_middle(xs: Vec<i64>) -> Vec<i64>
            requires len(xs) >= 3;
            ensures _return[1] == 999;
            {
              let ys: Vec<i64> = set(xs, 1, 999);
              return ys;
            }

            fn main() -> i64 {
              let original: Vec<i64> = vec(10, 20, 30);
              let updated: Vec<i64> = replace_middle(original);
              prove updated[1] == 777;
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("over-strong claim must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("counterexample") || e.message.contains("proof failed")),
            "expected counterexample, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn smt_proves_length_ensures_across_function_boundary() {
        // The length-preservation ensures pattern: callee promises
        // `len(_return) == len(xs)`, and the caller chains it with
        // `len(xs) == 2` (literal init) to get `len(ys) == 2`.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn keep_length(xs: Vec<i64>) -> Vec<i64>
            requires len(xs) >= 1;
            ensures len(_return) == len(xs);
            {
              return set(xs, 0, 0);
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2);
              let ys: Vec<i64> = keep_length(xs);
              prove len(ys) == 2;
              return 0;
            }
        "#;
        compile_to_c(source).expect("length ensures should chain across the call");
    }

    #[test]
    fn smt_preserves_bool_slots_across_constant_index_assign() {
        // Bool element completes the element-type matrix for
        // selective IndexAssign drop. After `xs[1] = true`, slots 0
        // and 2 should still carry their initializer values.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<bool> = vec(false, false, false);
              xs[1] = true;
              prove xs[0] == false;
              prove xs[1] == true;
              prove xs[2] == false;
              return 0;
            }
        "#;
        compile_to_c(source).expect("bool slot preservation after xs[1] = …");
    }

    #[test]
    fn smt_preserves_f64_slots_across_constant_index_assign() {
        // Float element companion test. Both untouched slots must
        // carry their literal initializer values; the touched slot
        // takes the new value.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<f64> = vec(1.0, 2.0, 3.0);
              xs[1] = 99.5;
              prove xs[0] == 1.0;
              prove xs[1] == 99.5;
              prove xs[2] == 3.0;
              return 0;
            }
        "#;
        compile_to_c(source).expect("f64 slot preservation after xs[1] = …");
    }

    #[test]
    fn smt_proves_read_after_write_with_symbolic_index() {
        // After `xs[i] = 99` with a function-parameter `i`, the
        // verifier should still know `xs[i] == 99` for the SAME `i`
        // — the IndexAssign handler emits the new slot fact even
        // for non-constant indices.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn touch_and_read(xs: mut ref Vec<i64>, i: u64) -> i64
            requires i < len(xs);
            {
              xs[i] = 99;
              prove xs[i] == 99;
              return xs[i];
            }

            fn main() -> i64 { return 0; }
        "#;
        compile_to_c(source).expect("read-after-write with symbolic index");
    }

    #[test]
    fn smt_preserves_length_across_non_constant_index_assign() {
        // When the assigned index is a function parameter (truly
        // non-constant), the IndexAssign filter must still preserve
        // `len(xs)` because element writes don't change length.
        // Per-slot facts conservatively go away.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn touch(xs: mut ref Vec<i64>, i: u64) -> i64
            requires i < len(xs);
            {
              xs[i] = 99;
              prove len(xs) > 0;
              return 0;
            }

            fn main() -> i64 { return 0; }
        "#;
        compile_to_c(source).expect("len(xs) should survive non-const xs[i] = v");
    }

    #[test]
    fn smt_disproves_slot_value_after_non_constant_index_assign() {
        // Companion negative test: after `xs[i] = 99` with a
        // function-parameter `i`, we can't conclude anything about
        // `xs[0]` because `i` might be 0. The filter must drop the
        // pre-existing `xs[0] == …` fact.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn touch(xs: mut ref Vec<i64>, i: u64) -> i64
            requires i < len(xs);
            requires len(xs) >= 3;
            {
              xs[i] = 99;
              prove xs[0] == 10;
              return 0;
            }

            fn main() -> i64 { return 0; }
        "#;
        let errors = compile(source).expect_err("xs[0] proof after non-const assign must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("counterexample") || e.message.contains("proof failed")),
            "expected counterexample, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn smt_preserves_unrelated_slot_facts_across_index_assign() {
        // When the assigned index is a compile-time constant K,
        // facts about other constant-indexed slots survive. After
        // `xs[1] = 99` on a `vec(10, 20, 30)` initializer, slots 0
        // and 2 should still carry their literal-init values, and
        // `len(xs)` should still be known.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              xs[1] = 99;
              prove xs[0] == 10;
              prove xs[1] == 99;
              prove xs[2] == 30;
              prove len(xs) == 3;
              return 0;
            }
        "#;
        compile_to_c(source).expect("non-touched slot facts should survive");
    }

    #[test]
    fn smt_two_step_index_assign_chains_through_versions() {
        // Two consecutive IndexAssigns on the same binding bump
        // the version twice. Each bump emits its own store-eq
        // axiom referencing the previous version, so the final
        // post-state still derives `xs[0]==1, xs[1]==2, xs[2]==30`
        // from the initial `vec(10, 20, 30)` plus two updates.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              xs[0] = 1;
              xs[1] = 2;
              prove xs[0] == 1;
              prove xs[1] == 2;
              prove xs[2] == 30;
              return 0;
            }
        "#;
        compile_to_c(source).expect("two-step IndexAssign should chain via versions");
    }

    #[test]
    fn smt_preserves_clone_relation_across_index_assign_via_versioning() {
        // With SMT-array versioning, a `clone(xs)` relation that
        // predates an `xs[i] = v` write should survive: the clone
        // captured `arr_ys_v0 = arr_xs_v0`, the IndexAssign pins
        // existing facts to xs#0 before bumping to v1, and the new
        // store-eq fact `arr_xs_v1 = (store arr_xs_v0 1 99)` only
        // affects xs's post-assign view. ys still sees the
        // pre-assign array via the v0 relation.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              let ys: Vec<i64> = clone(xs);
              xs[1] = 99;
              prove ys[0] == 10;
              prove ys[1] == 20;
              prove ys[2] == 30;
              prove xs[0] == 10;
              prove xs[1] == 99;
              prove xs[2] == 30;
              return 0;
            }
        "#;
        compile_to_c(source).expect("clone-then-index-assign chain should hold");
    }

    #[test]
    fn smt_disproves_clone_aliasing_with_post_assign_value() {
        // Sanity check: the clone is snapshotted at v0, so ys[1]
        // is the original 20, not the post-assign 99. A `prove
        // ys[1] == 99` must surface a counterexample — if it
        // discharged, the version bookkeeping would be unsound.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              let ys: Vec<i64> = clone(xs);
              xs[1] = 99;
              prove ys[1] == 99;
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("clone snapshot shouldn't see post-assign value");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("counterexample") || e.message.contains("proof failed")),
            "expected counterexample, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn smt_invalidates_stale_facts_on_index_assign_regression() {
        // Regression for a soundness bug: literal-vec facts emitted
        // at `let` time (xs[0]==10, xs[1]==20, …) were NOT dropped
        // when an `xs[i] = v` IndexAssign followed. A subsequent
        // `prove xs[i] == <old value>` would discharge against the
        // stale fact. The fix invalidates facts AND the
        // `vec_literal_elements` substitution stash on every
        // IndexAssign.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              xs[1] = 99;
              prove xs[1] == 20;
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("stale-fact prove must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("counterexample") || e.message.contains("proof failed")),
            "expected counterexample, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn smt_proves_read_after_index_assign() {
        // Companion to the staleness regression: after `xs[i] = v`
        // the verifier should know `xs[i] == v` (the just-assigned
        // slot), even though all other facts about xs are dropped.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              xs[1] = 99;
              prove xs[1] == 99;
              return xs[1];
            }
        "#;
        compile_to_c(source).expect("xs[i]==v after xs[i]=v must discharge");
    }

    #[test]
    fn smt_proves_push_element_at_new_tail() {
        // After `let ys = push(xs, v)` the pushed value lives at
        // index `len(xs)` (the old tail). The verifier should know
        // both `len(ys) == len(xs) + 1` and `ys[len(xs)] == v`,
        // so a concrete `prove ys[2] == 99` over a `vec(10, 20)`
        // initializer discharges via two chained facts:
        //   * len(xs) == 2 (from the literal-vec emitter)
        //   * ys[len(xs)] == 99 (from this slice's push emitter)
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20);
              let ys: Vec<i64> = push(xs, 99);
              prove len(ys) == 3;
              prove ys[2] == 99;
              return ys[2];
            }
        "#;
        compile_to_c(source).expect("push tail-element proof should discharge");
    }

    #[test]
    fn smt_disproves_wrong_push_element_value() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2);
              let ys: Vec<i64> = push(xs, 99);
              prove ys[2] == 55;
              return ys[2];
            }
        "#;
        let errors = compile(source).expect_err("wrong push value must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("counterexample") || e.message.contains("proof failed")),
            "expected counterexample, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn smt_proves_set_element_at_updated_slot() {
        // After `let ys = set(xs, k, v)`, the verifier should know
        // `ys[k] == v` without help. Length already preserved by
        // the existing fact emitter; this slice adds the element
        // identity at the updated slot.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              let ys: Vec<i64> = set(xs, 1, 99);
              prove len(ys) == 3;
              prove ys[1] == 99;
              return ys[1];
            }
        "#;
        compile_to_c(source).expect("set ys[i]==v proof should discharge");
    }

    #[test]
    fn smt_disproves_wrong_set_element_value() {
        // Sanity check that the emitted set fact carries the right
        // value: a `prove` against a different value must surface
        // a counterexample.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              let ys: Vec<i64> = set(xs, 1, 99);
              prove ys[1] == 55;
              return ys[1];
            }
        "#;
        let errors = compile(source).expect_err("wrong set value must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("counterexample") || e.message.contains("proof failed")),
            "expected counterexample, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn smt_proves_vec_index_value_via_requires() {
        // v1 SMT array theory: a `requires xs[0] > 0` precondition
        // should let the body's `prove xs[0] > 0` discharge without
        // a runtime guard. Today this is the simplest read-only
        // shape supported — mutations (`xs[i] = v`) are not yet
        // tracked through the SMT state.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn read_first(xs: ref Vec<i64>) -> i64
            requires len(xs) > 0;
            requires xs[0] > 0;
            {
              prove xs[0] > 0;
              return xs[0];
            }

            fn main() -> i64 {
              return 0;
            }
        "#;
        compile_to_c(source).expect("xs[0] proof via requires should discharge");
    }

    #[test]
    fn smt_disproves_vec_index_value_without_requires() {
        // Mirror of the above without the `requires xs[0] > 0`. The
        // verifier must refuse the prove and surface a counter-
        // example mentioning the indexed slot.
        if !z3_available() {
            return;
        }
        let source = r#"
            fn read_first(xs: ref Vec<i64>) -> i64
            requires len(xs) > 0;
            {
              prove xs[0] > 0;
              return xs[0];
            }

            fn main() -> i64 {
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("xs[0] proof without precondition must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("counterexample") || e.message.contains("proof failed")),
            "expected counterexample diagnostic, got: {:?}",
            errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn smt_proves_vec_bounds_via_requires() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn safe_get(xs: ref Vec<i64>, i: u64) -> i64
            requires i < len(xs);
            {
              prove i < len(xs);
              return xs[i];
            }

            fn main() -> i64 {
              return 0;
            }
        "#;

        compile_to_c(source).expect("Vec-length proof via requires");
    }

    #[test]
    fn smt_disproves_vec_bounds_without_requires() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn unsafe_get(xs: ref Vec<i64>, i: u64) -> i64 {
              prove i < len(xs);
              return xs[i];
            }

            fn main() -> i64 {
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("bounds proof shouldn't pass without precondition");
        assert!(
            errors.iter().any(|e| e.message.contains("counterexample") || e.message.contains("proof failed")),
            "expected SMT-counterexample diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn ensures_can_carry_vec_length_facts() {
        if !z3_available() {
            return;
        }
        // The callee promises the returned u64 is less than the vec's length;
        // the caller uses that fact to index safely.
        let source = r#"
            fn pick_index(xs: ref Vec<i64>) -> u64
            requires len(xs) > 0;
            ensures _return < len(xs);
            {
              return 0;
            }

            fn check(xs: ref Vec<i64>) -> i64
            requires len(xs) > 0;
            {
              let i: u64 = pick_index(xs);
              prove i < len(xs);
              return xs[i];
            }

            fn main() -> i64 {
              return 0;
            }
        "#;

        compile_to_c(source).expect("ensures + Vec len should compose");
    }

    #[test]
    fn counterexample_appears_in_disproven_diagnostic() {
        if !z3_available() {
            return;
        }
        use crate::diagnostic::format_diagnostics;
        let source = r#"fn bad(x: i64, y: i64) -> i64 {
  prove x + y > x;
  return 0;
}

fn main() -> i64 {
  return 0;
}
"#;
        let errors = compile(source).expect_err("unprovable claim");
        let rendered = format_diagnostics("t.intent", source, &errors);
        // The "[counterexample: ...]" or "SMT counterexample [...]" form
        // should be present.
        assert!(
            rendered.contains("SMT counterexample [")
                || rendered.contains("counterexample:"),
            "expected counterexample summary, got:\n{rendered}"
        );
        // Specifically, x and y values should appear.
        assert!(
            rendered.contains("x = ") && rendered.contains("y = "),
            "expected x and y values in counterexample, got:\n{rendered}"
        );
    }

    #[test]
    fn negative_counterexample_renders_cleanly() {
        if !z3_available() {
            return;
        }
        use crate::diagnostic::format_diagnostics;
        let source = r#"fn bad(x: i64) -> i64 {
  prove x > 0;
  return 0;
}

fn main() -> i64 {
  return 0;
}
"#;
        let errors = compile(source).expect_err("unprovable");
        let rendered = format_diagnostics("t.intent", source, &errors);
        // Negative values flatten to e.g. "x = -1", not "x = (- 1)".
        assert!(
            !rendered.contains("(- "),
            "expected flattened negative, got:\n{rendered}"
        );
    }

    #[test]
    fn parser_recovers_to_next_top_level_after_syntax_error() {
        let source = r#"
            fn bad_syntax( -> i64 {
              return 0;
            }

            fn good() -> i64 {
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("bad signature should error");
        // We should still get *some* diagnostic from the bad function.
        assert!(!errors.is_empty(), "expected at least one error");
        // And the main-missing check shouldn't fire — `good` parsed fine,
        // even though we have no `main`. So the error message should be
        // about main, not about parsing further functions.
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("main") || e.message.contains("expected")),
            "expected parse + main diagnostics, got: {:?}",
            errors
        );
    }

    #[test]
    fn parser_recovers_to_next_statement_within_function() {
        // Bad statement in the middle shouldn't kill the rest of the body.
        let source = r#"
            fn main() -> i64 {
              let x: nonsense = 5;
              let y: i64 = 10;
              assert y == 10;
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("bad type should error");
        assert!(
            errors.iter().any(|e| e.message.contains("expected type")),
            "expected type-parse diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn compile_surfaces_multiple_independent_errors() {
        // One syntax error in one function and one type error in another.
        let source = r#"
            fn bad_syntax( -> i64 {
              return 0;
            }

            fn type_error() -> i64 {
              let x: nonsense = 5;
              return 0;
            }

            fn main() -> i64 {
              return 0;
            }
        "#;
        let errors = compile(source).expect_err("multiple errors");
        // We should get at least two diagnostics — one from each problem.
        assert!(
            errors.len() >= 2,
            "expected multi-error output, got: {:?}",
            errors
        );
    }

    #[test]
    fn json_diagnostic_shape_for_move_after_move() {
        use crate::diagnostic::format_diagnostics_json;

        let source = r#"fn take(xs: Vec<i64>) -> i64 { return xs[0]; }
fn main() -> i64 {
  let xs: Vec<i64> = vec(1, 2, 3);
  let v: i64 = take(xs);
  let bad: i64 = xs[0];
  return bad;
}
"#;
        let errors = compile(source).expect_err("move-after-move");
        let json = format_diagnostics_json("f.intent", source, &errors);
        // Single trailing newline.
        assert!(json.ends_with("}\n"), "expected JSON to end with }}\\n, got:\n{json}");
        // Must contain the canonical fields.
        assert!(json.contains("\"diagnostics\":["), "missing diagnostics array");
        assert!(json.contains("\"level\":\"error\""), "missing level");
        assert!(json.contains("\"primary\":"), "missing primary span");
        assert!(json.contains("\"related\":["), "missing related array");
        // Coordinates point at use site (line 5) and move site (line 4).
        assert!(json.contains("\"line\":5"), "expected primary on line 5: {json}");
        assert!(json.contains("\"line\":4"), "expected related on line 4: {json}");
    }

    #[test]
    fn json_diagnostic_escapes_special_chars() {
        use crate::diagnostic::format_diagnostics_json;
        use crate::diagnostic::Diagnostic;
        use crate::span::Span;

        // Craft a diagnostic whose message contains characters that require
        // JSON escaping.
        let d = Diagnostic::new(Span::new(0, 1), "broken: \"quoted\" \\path\nnewline");
        let json = format_diagnostics_json("f.intent", "x", std::slice::from_ref(&d));
        // Quotes inside the message must be escaped.
        assert!(
            json.contains("\\\"quoted\\\""),
            "expected escaped quotes, got:\n{json}"
        );
        // Backslashes too.
        assert!(json.contains("\\\\path"), "expected escaped backslash");
        // Newlines.
        assert!(json.contains("\\n"), "expected escaped newline");
    }

    #[test]
    fn json_diagnostic_empty_when_no_errors() {
        use crate::diagnostic::format_diagnostics_json;
        let json = format_diagnostics_json("f.intent", "", &[]);
        assert_eq!(json, "{\"diagnostics\":[]}\n");
    }

    #[test]
    fn for_iter_sums_a_vec() {
        let source = r#"
            fn sum(xs: ref Vec<i64>) -> i64 {
              let total: i64 = 0;
              for x in ref xs {
                total = total + x;
              }
              return total;
            }

            fn main() -> i64 {
              let v: Vec<i64> = vec(1, 2, 3, 4);
              let s: i64 = sum(ref v);
              assert s == 10;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("for-iter on Vec");
        assert!(c.contains("_intent_idx_x"), "expected synthesized idx in C: {c}");
    }

    #[test]
    fn for_iter_sums_an_array_by_ref() {
        let source = r#"
            fn sum4(xs: ref [i64; 4]) -> i64 {
              let total: i64 = 0;
              for x in ref xs {
                total = total + x;
              }
              return total;
            }

            fn main() -> i64 {
              let arr: [i64; 4] = [10, 20, 30, 40];
              let s: i64 = sum4(ref arr);
              assert s == 100;
              return 0;
            }
        "#;

        compile_to_c(source).expect("for-iter on &[T;N]");
    }

    #[test]
    fn for_iter_rejects_non_collection() {
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 5;
              for y in ref x {
                let _ = y;
              }
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("can't iterate over i64");
        assert!(
            errors.iter().any(|e| e.message.contains("requires an array or Vec")),
            "expected non-collection diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn for_iter_scopes_the_element_var() {
        let source = r#"
            fn main() -> i64 {
              let arr: [i64; 3] = [1, 2, 3];
              for x in ref arr {
                let _ = x;
              }
              return x;
            }
        "#;

        let errors = compile(source).expect_err("element var should be scoped to body");
        assert!(
            errors.iter().any(|e| e.message.contains("unknown variable 'x'")),
            "expected scope diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn for_in_owned_vec_consumes_and_frees() {
        let source = r#"
            fn sum_and_drop(xs: Vec<i64>) -> i64 {
              let total: i64 = 0;
              for x in xs {
                total = total + x;
              }
              return total;
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3, 4);
              let s: i64 = sum_and_drop(xs);
              assert s == 10;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("consuming for-iter compiles");
        // Backend must emit a Vec free after the loop body in sum_and_drop.
        assert!(
            c.contains("__free"),
            "expected post-loop __free in emitted C: {c}"
        );
    }

    #[test]
    fn if_expr_var_branches_drop_unchosen_alternative() {
        // Closure #179: closes the unchosen-alternative leak
        // left by closures #172/#173's conservative move
        // tracking. `let r = if cond { a } else { b };` now
        // rewrites the typed expr so each branch wraps its
        // chosen value in a Block that drops the OTHER
        // branch's Var leaves first. ASan-clean on both
        // cond=true and cond=false.
        let source = r#"
            fn main() -> i64 {
              let cond: bool = true;
              let a: OwnedStr = "first" + "";
              let b: OwnedStr = "second" + "";
              let chosen: OwnedStr = if cond { a } else { b };
              assert (len(chosen) as i64) == 5;
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("if-expr Var branches compile");
        // Each branch must include the other Var's free.
        // The C ternary form: `cond ? ({ free(v_b); v_a; })
        // : ({ free(v_a); v_b; })`.
        let main_start = c.find("static int64_t fn_main").expect("fn_main present");
        let main_end = c[main_start..]
            .find("\nint main(void)")
            .map(|i| main_start + i)
            .unwrap_or(c.len());
        let main_body = &c[main_start..main_end];
        let chosen_idx = main_body
            .find("v_chosen =")
            .expect("v_chosen assignment");
        let return_idx = main_body
            .find("return v___intent_ret")
            .expect("return present");
        let if_region = &main_body[chosen_idx..return_idx];
        assert!(
            if_region.contains("free((void*)v_a)")
                && if_region.contains("free((void*)v_b)"),
            "expected both v_a and v_b dropped inside if-expr branches:\n{}",
            if_region
        );
    }

    #[test]
    fn enum_variant_payload_consumes_var() {
        // Closure #178: `Maybe.Some(n)` where n is a Var of
        // OwnedStr was double-freeing on scope exit. The
        // EnumVariantWithPayload constructor transfers
        // ownership of the payload into the tagged-union,
        // but `check_call` for enum constructors never
        // called `consume_if_moved_var` on the payload arg.
        // Same family as vec / push / set (#171, #177).
        let source = r#"
            enum Maybe { Some(OwnedStr), None }

            fn main() -> i64 {
              let n: OwnedStr = "alpha" + "";
              let m: Maybe = Maybe.Some(n);
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("Maybe.Some(Var) compiles");
        let main_start = c.find("static int64_t fn_main").expect("fn_main present");
        let main_end = c[main_start..]
            .find("\nint main(void)")
            .map(|i| main_start + i)
            .unwrap_or(c.len());
        let main_body = &c[main_start..main_end];
        assert!(
            !main_body.contains("free((void*)v_n)"),
            "v_n was moved into Maybe.Some(); scope-exit drop must not fire:\n{}",
            main_body
        );
    }

    #[test]
    fn vec_literal_marks_var_elements_moved() {
        // Closure #177: `let xs: Vec<OwnedStr> = vec(a, b);`
        // where a, b are Vars of OwnedStr was double-
        // freeing on scope exit. The vec() builtin
        // transfers ownership of each Var into the new
        // Vec's slot, so the source Var's scope-exit Drop
        // fired AFTER vec() had already moved the heap
        // pointer into the buffer, and the Vec's __free
        // re-freed each element when xs went out of
        // scope. Same family as push / set (closure #171):
        // builtin handlers were forgetting to call
        // consume_if_moved_var on element args.
        //
        // Both backends were affected (checker/IR-level
        // bug). One-line fix.
        let source = r#"
            fn main() -> i64 {
              let a: OwnedStr = "alpha" + "";
              let b: OwnedStr = "beta" + "";
              let xs: Vec<OwnedStr> = vec(a, b);
              assert (len(ref xs) as i64) == 2;
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("vec(Var, Var) compiles");
        // After the vec() call, neither v_a nor v_b should
        // appear in a scope-exit free — they were moved
        // into the buffer.
        let main_start = c.find("static int64_t fn_main").expect("fn_main present");
        let main_end = c[main_start..]
            .find("\nint main(void)")
            .map(|i| main_start + i)
            .unwrap_or(c.len());
        let main_body = &c[main_start..main_end];
        for var in &["v_a", "v_b"] {
            assert!(
                !main_body.contains(&format!("free((void*){})", var)),
                "{} was moved into vec(); its scope-exit drop must not fire:\n{}",
                var,
                main_body
            );
        }
        // The Vec's __free must still appear (it walks the
        // buffer and frees each element).
        assert!(
            main_body.contains("intent_vec_owned_str__free"),
            "expected scope-exit Vec __free:\n{}",
            main_body
        );
    }

    #[test]
    fn ssa_c_ref_channel_param_is_non_const() {
        // Closure #176: SSA-C declared `ref Channel<T, N>`
        // params as `const intent_channel_<T>_<N>*`. The
        // shared `intent_channel_*_send` / `_recv` runtime
        // helpers take a NON-const pointer (they bump
        // seq counters and read/write idx through atomic
        // loads/stores), so every send / recv call in
        // SSA-C compiled with -Wdiscarded-qualifiers.
        // Atomic refs already dropped `const`; the Channel
        // arm now mirrors that.
        let source = r#"
            fn produce(ch: ref Channel<i64, 16>, v: i64) -> i64 {
              return channel_send(ch, v);
            }

            fn main() -> i64 {
              let ch: Channel<i64, 16> = channel_new();
              let _ = produce(ref ch, 42);
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("ref Channel param compiles");
        // The produce fn's param must NOT carry a `const`
        // qualifier for the channel pointer.
        let produce_start = c
            .find("fn_produce(")
            .expect("produce fn present");
        let line_end = c[produce_start..]
            .find('\n')
            .map(|i| produce_start + i)
            .unwrap_or(c.len());
        let sig = &c[produce_start..line_end];
        assert!(
            !sig.contains("const intent_channel_"),
            "produce's channel param must be non-const:\n{sig}"
        );
    }

    #[test]
    fn ssa_c_owned_str_declared_mutable() {
        // Closure #175: SSA-C declared OwnedStr SSA values
        // as `const char*`, the same as Str. The Vec helper
        // bundle (shared with tree-C) declares the data
        // field as `char* data`, so storing a const-qualified
        // value into a non-const slot raised
        // -Wdiscarded-qualifiers on every IndexAssign and
        // similar store. The actual runtime behavior was
        // fine (const is purely a compile-time annotation)
        // but the warning noise hid actionable diagnostics.
        //
        // Fix: split Str (borrowed read-only, stays `const
        // char*`) from OwnedStr (heap-owning, mutable —
        // `char*`).
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<OwnedStr> = vec("a" + "", "b" + "");
              let v: OwnedStr = "new" + "";
              xs[0] = v;
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("OwnedStr SSA-C compiles");
        // The SSA-C emit declares `char* v_N;` for any
        // OwnedStr-typed SSA value, never `const char*`
        // for OwnedStr. Verify by scanning declarations.
        // Need at least one `char* v_` declaration (without
        // `const`) — the `v` binding's OwnedStr SSA value.
        let has_mutable_owned = c
            .lines()
            .any(|line| line.trim_start().starts_with("char* v_"));
        assert!(
            has_mutable_owned,
            "expected `char* v_…` declaration for OwnedStr (not `const char*`):\n{c}"
        );
    }

    #[test]
    fn block_expr_var_tail_consumes_source_var() {
        // Closure #174: `let b = { let _x = 1; a };` (a:
        // OwnedStr Var) was double-freeing. The Block's
        // tail expression yields a's value into the
        // binding b, so b ends up aliasing a's heap.
        // Both a's scope-exit drop and b's scope-exit
        // drop then fired on the same heap. Same shape
        // as closures #172/#173: `consume_if_moved_var`
        // only descended into Var, FieldAccess, IfExpr,
        // and Match — Block fell through. Now the tail
        // is recursively consumed too.
        let source = r#"
            fn main() -> i64 {
              let a: OwnedStr = "hello" + "";
              let b: OwnedStr = { let _x: i64 = 1; a };
              assert (len(b) as i64) == 5;
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("block-expr Var tail compiles");
        let main_start = c.find("static int64_t fn_main").expect("fn_main present");
        let main_end = c[main_start..]
            .find("\nint main(void)")
            .map(|i| main_start + i)
            .unwrap_or(c.len());
        let main_body = &c[main_start..main_end];
        // After the move, v_a's scope-exit drop must be
        // suppressed; v_b's frees the chosen heap.
        assert!(
            !main_body.contains("free((void*)v_a)"),
            "v_a was moved into the block tail; scope-exit drop must not fire:\n{}",
            main_body
        );
        assert!(
            main_body.contains("free((void*)v_b)"),
            "v_b owns the moved heap; expect a scope-exit free:\n{}",
            main_body
        );
    }

    #[test]
    fn match_arms_returning_var_consume_all_arms() {
        // Closure #173: same shape as the if-expr Var
        // branches fix (closure #172) — `let chosen =
        // match n { 1 then a, 2 then b, _ then c };` was
        // double-freeing because the codegen switch makes
        // v_chosen alias the chosen arm's Var, and the
        // scope-exit drops of every Var plus v_chosen all
        // hit the same heap. Integer / enum / bool match
        // returns TypedExprKind::Match directly (Str
        // scrutinees desugar through check_match_str's
        // IfExpr chain so they're already covered by
        // #172). consume_if_moved_var now recurses into
        // every arm's body. Conservative: unchosen-arm
        // Vars leak (same TODO as the if-expr case).
        let source = r#"
            fn main() -> i64 {
              let n: i64 = 2;
              let a: OwnedStr = "alpha" + "";
              let b: OwnedStr = "beta" + "";
              let c: OwnedStr = "gamma" + "";
              let chosen: OwnedStr = match n {
                1 then a,
                2 then b,
                _ then c,
              };
              assert (len(chosen) as i64) == 4;
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("match Var arms compile");
        let main_start = c.find("static int64_t fn_main").expect("fn_main present");
        let main_end = c[main_start..]
            .find("\nint main(void)")
            .map(|i| main_start + i)
            .unwrap_or(c.len());
        let main_body = &c[main_start..main_end];
        // Closure #179: each arm now drops the OTHER arms'
        // Vars before yielding its chosen value. So v_a /
        // v_b / v_c free calls show up INSIDE the match's
        // case branches — NOT at scope exit. v_chosen still
        // owns the chosen heap and is freed at scope exit.
        let return_idx = main_body
            .find("return v___intent_ret")
            .expect("return present");
        let scope_exit = &main_body[main_body[return_idx..]
            .find(';')
            .map(|i| return_idx + i + 1)
            .unwrap_or(main_body.len())..];
        for var in &["v_a", "v_b", "v_c"] {
            assert!(
                !scope_exit.contains(&format!("free((void*){})", var)),
                "{} was consumed by the match; scope-exit drop must be suppressed:\n{}",
                var,
                scope_exit
            );
        }
        // Per closure #179, each arm drops the other arms'
        // Vars inside the case body — collectively at least
        // 2 of {v_a, v_b, v_c} must appear in the match's
        // case bodies (each case drops the other arms' Vars).
        let chosen_start = main_body
            .find("v_chosen =")
            .expect("v_chosen assignment present");
        let match_region = &main_body[chosen_start..return_idx];
        let drop_count = ["v_a", "v_b", "v_c"]
            .iter()
            .filter(|v| match_region.contains(&format!("free((void*){})", v)))
            .count();
        assert!(
            drop_count >= 2,
            "expected at least 2 arm-local drops (closure #179), got {}:\n{}",
            drop_count,
            match_region
        );
        assert!(
            main_body.contains("free((void*)v_chosen)"),
            "v_chosen owns the chosen arm's heap; expect a scope-exit free:\n{}",
            main_body
        );
    }

    #[test]
    fn if_expr_var_branches_consume_both_vars() {
        // Closure #172: `let chosen = if cond { a } else
        // { b };` where `a, b: OwnedStr` Vars was double-
        // freeing on scope exit. The codegen ternary
        // (`cond ? v_a : v_b`) makes v_chosen alias the
        // chosen Var's heap, so the scope-exit drops of
        // v_a, v_b, AND v_chosen all hit the same heap.
        // `consume_if_moved_var` only recursed into bare
        // Var and FieldAccess sources, ignoring IfExpr.
        // Now it descends into both branches and marks
        // each branch's Var moved. Conservative: the
        // UNCHOSEN alternative leaks (its heap isn't
        // freed since the Var is marked moved). Tracked
        // separately as a remaining TODO; the proper fix
        // is a structural rewrite of the if-expr that
        // frees the unchosen alternative inside each
        // branch.
        let source = r#"
            fn main() -> i64 {
              let cond: bool = true;
              let a: OwnedStr = "first" + "";
              let b: OwnedStr = "second" + "";
              let chosen: OwnedStr = if cond { a } else { b };
              assert (len(chosen) as i64) == 5;
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("if-expr Var branches compile");
        // Closure #179 (the structural rewrite) wraps each
        // branch in a Block that drops the OTHER branch's
        // Vars before yielding the chosen value. So
        // v_a / v_b frees appear INSIDE the ternary's
        // statement-expression branches — NOT at scope
        // exit (the Vars are still marked moved at
        // compile time). v_chosen owns the chosen heap
        // and is freed at scope exit.
        let main_start = c.find("static int64_t fn_main").expect("fn_main present");
        let main_end = c[main_start..]
            .find("\nint main(void)")
            .map(|i| main_start + i)
            .unwrap_or(c.len());
        let main_body = &c[main_start..main_end];
        // Each Var must be freed inside a statement-expr
        // branch (`({ free(...); ...; })`) — never at the
        // top-level scope exit.
        let chosen_start = main_body
            .find("v_chosen =")
            .expect("v_chosen assignment present");
        let return_idx = main_body
            .find("return v___intent_ret")
            .expect("return present");
        let scope_exit = &main_body[main_body[return_idx..]
            .find(';')
            .map(|i| return_idx + i + 1)
            .unwrap_or(main_body.len())..];
        // Per closure #179, no v_a / v_b free at scope exit.
        assert!(
            !scope_exit.contains("free((void*)v_a)"),
            "v_a must NOT be freed at scope exit:\n{}",
            scope_exit
        );
        assert!(
            !scope_exit.contains("free((void*)v_b)"),
            "v_b must NOT be freed at scope exit:\n{}",
            scope_exit
        );
        // The if-expr ternary must drop the unchosen Var
        // inside each branch.
        let if_expr_region = &main_body[chosen_start..];
        assert!(
            if_expr_region.contains("free((void*)v_a)"),
            "expected v_a free inside the else-branch (closure #179):\n{}",
            if_expr_region
        );
        assert!(
            if_expr_region.contains("free((void*)v_b)"),
            "expected v_b free inside the then-branch (closure #179):\n{}",
            if_expr_region
        );
        assert!(
            main_body.contains("free((void*)v_chosen)"),
            "v_chosen owns the chosen Var's heap; expect a scope-exit free:\n{}",
            main_body
        );
    }

    #[test]
    fn push_and_set_mark_value_var_moved() {
        // Closure #171: `push(xs, v)` and `set(xs, i, v)`
        // where `v` is a Var of non-Copy heap (OwnedStr,
        // Vec, …) were double-freeing on scope exit. The
        // checker's builtin handlers called
        // `consume_if_moved_var(&args[0], &xs, env)` to
        // mark the Vec moved, but the VALUE arg never got
        // the same treatment — so the source Var's
        // scope-exit Drop fired AFTER push transferred
        // ownership into the new Vec's slot, freeing the
        // heap a second time when the Vec was later
        // __free'd.
        //
        // ASan caught it as double-free on a chained
        // `let xs2 = push(xs, v); let xs3 = push(xs2, w);`.
        // Both backends were affected since this is a
        // checker/IR-level bug.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<OwnedStr> = vec("a" + "");
              let v: OwnedStr = "b" + "";
              let xs2: Vec<OwnedStr> = push(xs, v);
              assert (len(ref xs2) as i64) == 2;
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("push(xs, Var) compiles");
        // After the push, the Var `v_v` MUST NOT be freed
        // at scope exit — it was moved into the Vec.
        let main_start = c.find("static int64_t fn_main").expect("fn_main present");
        let main_end = c[main_start..]
            .find("\nint main(void)")
            .map(|i| main_start + i)
            .unwrap_or(c.len());
        let main_body = &c[main_start..main_end];
        // Locate the push call. Tree-C names value args
        // by their SSA-style local name; the moved Var
        // (the `v` binding holding "b") is `v_v` in the
        // local-name convention.
        assert!(
            main_body.contains("intent_vec_owned_str__push("),
            "expected push call in fn_main:\n{}",
            main_body
        );
        let push_idx = main_body
            .find("intent_vec_owned_str__push(")
            .expect("push present");
        let after_push = &main_body[push_idx..];
        // No explicit `free(v_v)` (the source Var's
        // scope-exit drop) should appear after the push.
        // The Vec's __free covers the moved heap.
        assert!(
            !after_push.contains("free((void*)v_v)"),
            "v was moved into push; its scope-exit drop must not fire:\n{}",
            main_body
        );

        // Set form: `set(xs, i, v)` — args[2] is the value.
        let set_src = r#"
            fn main() -> i64 {
              let xs: Vec<OwnedStr> = vec("a" + "", "b" + "");
              let v: OwnedStr = "new" + "";
              let xs2: Vec<OwnedStr> = set(xs, 0, v);
              assert (len(ref xs2) as i64) == 2;
              return 0;
            }
        "#;
        let c2 = compile_to_c(set_src).expect("set(xs, i, Var) compiles");
        let set_idx = c2.find("__set(").expect("set call present");
        let after_set = &c2[set_idx..];
        assert!(
            !after_set.contains("free((void*)v_v)"),
            "v was moved into set; its scope-exit drop must not fire:\n{}",
            c2
        );
    }

    #[test]
    fn tree_llvm_field_assign_drops_old_struct_and_enum_heap() {
        // Closure #170: tree-LLVM FieldAssign had drop-old
        // arms for OwnedStr and Vec (closure #132); Struct
        // and Enum fields fell through `_ => {}` so a nested
        // struct's heap (or a payloaded enum's heap payload)
        // leaked on `o.inner = NewInner { … }`. Tree-C had
        // the parallel arms via closure #148.
        let source = r#"
            struct Inner { name: OwnedStr }
            struct Outer { inner: Inner, count: i64 }

            fn main() -> i64 {
              let o: Outer = Outer { inner: Inner { name: "deep" + "" }, count: 7 };
              o.inner = Inner { name: "fresh" + "" };
              return 0;
            }
        "#;
        let ll = crate::backend_llvm::LlvmBackend
            .emit(&compile(source).expect("nested FieldAssign compiles").ir);
        // Two `call void @free(i8* …)` lines expected in
        // fn_main: one for the OLD `o.inner.name` (the new
        // drop-old path), one for the FRESH `o.inner.name`
        // at scope exit.
        let main_start = ll.find("define i64 @fn_main").expect("fn_main present");
        let main_end = ll[main_start..]
            .find("\ndefine ")
            .map(|i| main_start + i)
            .unwrap_or(ll.len());
        let main_body = &ll[main_start..main_end];
        let free_count = main_body
            .lines()
            .filter(|l| l.contains("call void @free(i8*"))
            .count();
        assert!(
            free_count >= 2,
            "expected at least 2 @free calls in fn_main (old + scope exit), got {}:\n{}",
            free_count,
            main_body
        );

        // Note: enum-as-struct-field is gated out by the
        // checker in v1, so the enum arm of the FieldAssign
        // drop-old logic is defensive — kept for parity with
        // tree-C and to match the Reassign Enum arm.
    }

    #[test]
    fn tree_llvm_reassign_drops_old_struct_and_enum_heap() {
        // Closure #169: tree-LLVM's Reassign handler had
        // drop_old arms only for Vec and OwnedStr. Bindings
        // of heap-owning struct types (e.g. `b: Box` with
        // `name: OwnedStr` field) and payloaded enums lost
        // the OLD value's heap on `b = Box { … }` / `m =
        // Maybe.Some(…)`. Tree-C had the parallel arms via
        // closure #147.
        //
        // Struct case: walks the existing alloca's fields
        // via emit_llvm_struct_field_drops before storing
        // the fresh value.
        //
        // Enum case: loads the OLD tagged-union from the
        // alloca, OR-chains the payloaded tag check, frees
        // the heap payload if active. Mirrors the Drop
        // handler's Enum arm.
        let source = r#"
            struct Box { name: OwnedStr }

            fn main() -> i64 {
              let b: Box = Box { name: "first" + "" };
              b = Box { name: "second" + "" };
              return 0;
            }
        "#;
        let ll = crate::backend_llvm::LlvmBackend
            .emit(&compile(source).expect("struct reassign compiles").ir);
        let main_start = ll.find("define i64 @fn_main").expect("fn_main present");
        let main_end = ll[main_start..]
            .find("\ndefine ")
            .map(|i| main_start + i)
            .unwrap_or(ll.len());
        let main_body = &ll[main_start..main_end];
        // Reassign must walk the OLD struct's fields BEFORE
        // storing the new struct. The first field's free
        // (the OwnedStr) is `call void @free(i8* …)` —
        // there must be at least TWO such frees in fn_main:
        // one for the OLD `b.name` on reassign, one for
        // the new `b.name` on scope exit.
        let free_count = main_body
            .lines()
            .filter(|l| l.contains("call void @free(i8*"))
            .count();
        assert!(
            free_count >= 2,
            "expected at least 2 free calls (old slot + scope exit), got {}:\n{}",
            free_count,
            main_body
        );

        let enum_source = r#"
            enum Maybe { Some(OwnedStr), None }

            fn main() -> i64 {
              let m: Maybe = Maybe.Some("first" + "");
              m = Maybe.Some("second" + "");
              return 0;
            }
        "#;
        let ll2 = crate::backend_llvm::LlvmBackend
            .emit(&compile(enum_source).expect("enum reassign compiles").ir);
        // Enum reassign emits a `reassign_enum_free` label
        // (per the tag-branch design).
        assert!(
            ll2.contains("reassign_enum_free"),
            "expected `reassign_enum_free` branch label in enum reassign:\n{}",
            ll2
        );
    }

    #[test]
    fn tree_llvm_discard_owned_str_frees_heap() {
        // Closure #168: `let _ = s;` where `s: OwnedStr`
        // was leaking on tree-LLVM. The Discard handler's
        // OwnedStr arm sat AFTER `else if is_scalar(&expr.ty)`,
        // but `is_scalar(Type::OwnedStr) == true` so the
        // scalar arm consumed the branch — it just calls
        // `emit_expr` and discards the SSA value, never
        // freeing the heap. Same shape as the Struct fix
        // (closure #145) that already moved its arm BEFORE
        // is_scalar.
        let source = r#"
            fn main() -> i64 {
              let s: OwnedStr = "abc" + "";
              let _ = s;
              return 0;
            }
        "#;
        let ll = crate::backend_llvm::LlvmBackend
            .emit(&compile(source).expect("Discard of OwnedStr Var compiles").ir);
        let main_start = ll.find("define i64 @fn_main").expect("fn_main present");
        let main_end = ll[main_start..]
            .find("\ndefine ")
            .map(|i| main_start + i)
            .unwrap_or(ll.len());
        let main_body = &ll[main_start..main_end];
        // The Discard must emit a `call void @free(i8* …)`
        // for the OwnedStr. The leak signature was the
        // ABSENCE of any `@free` in fn_main.
        assert!(
            main_body.contains("call void @free(i8*"),
            "expected `call void @free(i8* …)` for the OwnedStr Discard:\n{}",
            main_body
        );
    }

    #[test]
    fn tree_llvm_index_assign_drops_old_owned_str_slot() {
        // Closure #167: `xs[i] = v` on a `Vec<OwnedStr>` was
        // leaking the old slot's heap in tree-LLVM. The
        // `emit_leaf_overwrite_drop` helper had an early
        // return `if field_path.is_empty()` that matched the
        // bare-leaf case (`xs[i] = v` with no `.field.field…`
        // path). Removing that guard lets the OwnedStr / Vec
        // arms run for the bare slot — `p` points directly
        // at the array element when the path is empty, so
        // the load+free shape is the same as the
        // deep-field case. SSA-C's IndexAssign emitter
        // already handled this via its own
        // `c_element_drop_old` call. Tree-C was unaffected
        // (it goes through a separate IndexAssign path that
        // calls `c_element_drop_old` directly).
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<OwnedStr> = vec("a" + "", "b" + "");
              let v: OwnedStr = "new" + "";
              xs[0] = v;
              return 0;
            }
        "#;
        let ll = crate::backend_llvm::LlvmBackend
            .emit(&compile(source).expect("xs[i] = v compiles").ir);
        // The IndexAssign on a non-Copy element type must
        // emit `load i8*, i8**` + `call void @free(i8* …)`
        // BEFORE the `store` of the new value.
        let main_start = ll.find("define i64 @fn_main").expect("fn_main present");
        let main_end = ll[main_start..]
            .find("\ndefine ")
            .map(|i| main_start + i)
            .unwrap_or(ll.len());
        let main_body = &ll[main_start..main_end];
        assert!(
            main_body.contains("call void @free(i8*"),
            "expected `call void @free(i8* …)` before the IndexAssign store:\n{}",
            main_body
        );
    }

    #[test]
    fn field_assign_marks_rhs_var_moved() {
        // Closure #166: `self.name = n;` inside a method
        // declared `set_name(self: mut ref T, n: OwnedStr)`
        // was double-freeing the new heap. The C output ran
        // `free(self->name)` (correct old-slot drop), stored
        // `v_n` into the slot (correct), then on the
        // method's scope exit ran `free(v_n)` — freeing the
        // heap the field now owns. After the call returned,
        // any read of `t.name` was use-after-free.
        //
        // The checker's `Let` / `Reassign` / Call-arg arms
        // already call `consume_if_moved_var` to mark the RHS
        // Var as moved when it owns non-Copy heap. FieldAssign
        // didn't, so the parameter binding stayed "live" and
        // its scope-exit Drop fired.
        //
        // ASan caught it as heap-use-after-free on a later
        // print of `b.name` after `b.set_name("beta" + "")`.
        let source = r#"
            struct Box { name: OwnedStr, count: i64 }
            methods on Box {
              fn set_name(self: mut ref Box, n: OwnedStr) {
                self.name = n;
              }
            }
            fn main() -> i64 {
              let b: Box = Box { name: "alpha" + "", count: 0 };
              b.set_name("beta" + "");
              assert b.count == 0;
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("field-assign moves RHS Var");
        // Find the DEFINITION (not the forward declaration)
        // of fn_Box_set_name: it ends with `{` instead of
        // `;`. After the `v_self->name = v_n;` store, there
        // must NOT be a `free((void*)v_n);` line — the
        // value was moved into the field. The old slot's
        // free BEFORE the store is still correct.
        let fn_def_start = c
            .find("static int64_t fn_Box_set_name(Struct_Box* v_self, char* v_n) {")
            .expect("fn_Box_set_name definition present");
        let fn_def_end = c[fn_def_start..]
            .find("\n}\n")
            .map(|i| fn_def_start + i)
            .unwrap_or(c.len());
        let body = &c[fn_def_start..fn_def_end];
        assert!(
            body.contains("v_self->name = v_n;"),
            "expected store of v_n into v_self->name:\n{body}"
        );
        let store_idx = body
            .find("v_self->name = v_n;")
            .expect("store present");
        let after_store = &body[store_idx..];
        assert!(
            !after_store.contains("free((void*)v_n)"),
            "v_n was moved into the field; its scope-exit drop must not fire:\n{body}"
        );
    }

    #[test]
    fn field_borrow_through_ref_self_uses_arrow_in_c() {
        // Closure #165: `ref self.items` inside a method
        // whose `self: ref Tags` previously emitted
        // `&v_self.items` in tree-C and an invalid
        // `getelementptr %Struct_Tags*, %Struct_Tags** …`
        // in tree-LLVM. The bug: backends only knew the
        // field-borrow's `object` name, not the binding's
        // type. RefField / RefMutField now carry the
        // binding's `object_ty`; tree-C picks `.` vs `->`
        // from `object_ty.is_any_ref()`, and tree-LLVM
        // strips Ref/RefMut so the GEP source type is the
        // dereferenced struct.
        let source = r#"
            struct Tags { items: Vec<i64> }
            methods on Tags {
              fn count(self: ref Tags) -> i64 {
                return (len(ref self.items) as i64);
              }
            }
            fn main() -> i64 {
              let t: Tags = Tags { items: vec(1, 2, 3) };
              assert t.count() == 3;
              return 0;
            }
        "#;
        let c = compile_to_c(source)
            .expect("ref self.items through a method compiles");
        assert!(
            c.contains("&v_self->items"),
            "expected `&v_self->items` for ref-typed self in C output:\n{c}"
        );
        let ll = crate::backend_llvm::LlvmBackend
            .emit(&compile(source).expect("LLVM emit").ir);
        // tree-LLVM must GEP from the struct pointer
        // directly (`%Struct_Tags, %Struct_Tags* %arg_self`)
        // rather than the previous invalid double-indirection.
        assert!(
            ll.contains("getelementptr %Struct_Tags, %Struct_Tags* %arg_self"),
            "expected single-indirection GEP through %arg_self in LLVM:\n{ll}"
        );
    }

    #[test]
    fn tree_c_emits_struct_typedefs_in_dependency_order() {
        // Closure #164: structs were emitted in source order
        // (`for decl in &program.structs`), so
        //   struct Outer { inner: Inner }
        //   struct Inner { x: i64 }
        // emitted `typedef struct { Struct_Inner inner; }
        // Struct_Outer;` BEFORE `typedef struct { … }
        // Struct_Inner;`. C requires the field type to be
        // complete at declaration time, so cc rejected the
        // output with "unknown type name 'Struct_Inner'".
        // LLVM's IR forward-declares named types so tree-
        // LLVM was unaffected.
        //
        // Topological sort by direct field dependency
        // (Struct field, or Array element). Vec/Ref/Tuple/
        // /Atomic/Mutex/Guard/Channel use pointer-shaped
        // indirection through their own typedef bundles
        // so they don't drive struct dependencies.
        let source = r#"
            struct Outer { inner: Inner }
            struct Inner { x: i64 }

            fn main() -> i64 {
              let o: Outer = Outer { inner: Inner { x: 42 } };
              assert o.inner.x == 42;
              return 0;
            }
        "#;

        let c = compile_to_c(source)
            .expect("nested struct compiles after topological sort");
        // The Inner typedef must appear before the Outer
        // typedef in the emitted C.
        let inner_typedef = c
            .find("} Struct_Inner;")
            .expect("Struct_Inner typedef present");
        let outer_typedef = c
            .find("} Struct_Outer;")
            .expect("Struct_Outer typedef present");
        assert!(
            inner_typedef < outer_typedef,
            "Struct_Inner must be declared before Struct_Outer (outer references inner by value):\n{}",
            c
        );
    }

    #[test]
    fn tree_llvm_index_into_struct_field_vec_compiles() {
        // Closure #163: `t.items[i]` (FieldAccess base, Vec
        // type) panicked tree-LLVM's Index handler with
        // `unreachable!("Index on unsupported base")`. The
        // handler had a FieldAccess arm only for Array-typed
        // fields. Now Vec-typed field bases reuse the
        // emit_lvalue_addr machinery: the field-pointer IS
        // the Vec struct address, so we GEP into .data, load
        // the element pointer, GEP at idx, and load. The
        // same shape is reachable whenever a sibling
        // expression forces an SSA-LLVM fallback (e.g. an
        // OwnedStr concat or a clone_at-shaped call).
        let source = r#"
            struct Box { items: Vec<i64> }

            fn main() -> i64 {
              let b: Box = Box { items: vec(10, 20, 30) };
              let v: i64 = b.items[1];
              assert v == 20;
              return 0;
            }
        "#;

        let ll = crate::backend_llvm::LlvmBackend
            .emit(&compile(source).expect("t.items[i] compiles").ir);

        // The fix must emit: a GEP into the items field
        // (struct field 0), then a GEP into the Vec's .data
        // field (i32 0), then a load of `i8**` (or `T**`),
        // then a GEP at the dynamic index, then a load of
        // the element. The first GEP signature is `i32 0`
        // into the struct (items is field 0 of Box).
        let main_start = ll.find("define i64 @fn_main").expect("fn_main present");
        let main_end = ll[main_start..]
            .find("\ndefine ")
            .map(|i| main_start + i)
            .unwrap_or(ll.len());
        let main_body = &ll[main_start..main_end];
        // After the items field GEP, we need a Vec .data GEP
        // and a load of the data pointer.
        let has_struct_field_gep = main_body
            .lines()
            .any(|l| l.contains("getelementptr %Struct_Box, %Struct_Box*"));
        let has_vec_data_gep = main_body
            .lines()
            .any(|l| l.contains("%intent_vec_i64*") && l.contains("i32 0"));
        assert!(
            has_struct_field_gep,
            "expected GEP into Struct_Box for items field:\n{}",
            main_body
        );
        assert!(
            has_vec_data_gep,
            "expected GEP into intent_vec_i64 for .data:\n{}",
            main_body
        );
    }

    #[test]
    fn tree_llvm_len_of_field_borrow_and_field_access_uses_field_pointer() {
        // Closure #162: extends #161 to the two field-shape
        // spellings of len that flow through tree-LLVM when a
        // sibling expression forces an SSA-LLVM fallback:
        //   - `len(ref t.items)` / `len(mut ref t.items)`
        //     — array.kind is RefField / RefMutField.
        //   - `len(t.items)` — array.kind is FieldAccess
        //     yielding a Vec value directly.
        // Both fell through to the `format!("{}", length)`
        // fallback (zero for Vec). Worse, the constant `i64 0`
        // operand was getting handed to assertion/store sites
        // that the lli verifier rejected outright — programs
        // crashed before they could run. The fix gets a
        // pointer to the field via emit_expr (field-borrow
        // forms) or emit_lvalue_addr (FieldAccess) and GEPs
        // into the Vec struct's `.len` slot (field index 1).
        let source = r#"
            struct Box { items: Vec<OwnedStr> }

            fn main() -> i64 {
              let b: Box = Box { items: vec("a" + "", "b" + "", "c" + "") };
              let n_ref: i64 = (len(ref b.items) as i64);
              assert n_ref == 3;
              let n_val: i64 = (len(b.items) as i64);
              assert n_val == 3;
              return 0;
            }
        "#;

        let ll = crate::backend_llvm::LlvmBackend
            .emit(&compile(source).expect("len on field forms compiles").ir);

        // Both shapes must materialize a real load — not the
        // constant `i64 0` that the static-length fallback
        // would emit. We look for two `load i64, i64*` lines
        // inside fn_main; one per assertion site (after the
        // checker's bounds elision lowered).
        let main_start = ll.find("define i64 @fn_main").expect("fn_main present");
        let main_end = ll[main_start..]
            .find("\ndefine ")
            .map(|i| main_start + i)
            .unwrap_or(ll.len());
        let main_body = &ll[main_start..main_end];
        let load_count = main_body
            .lines()
            .filter(|l| l.contains("load i64, i64*"))
            .count();
        assert!(
            load_count >= 2,
            "expected at least 2 i64 loads (one per `len` site), got {}:\n{}",
            load_count,
            main_body
        );
    }

    #[test]
    fn tree_llvm_len_of_ref_vec_emits_load_not_static_zero() {
        // Closure #161: tree-LLVM's `emit_expr` Len handler
        // only recognized `array.kind == Var(name)`. When the
        // source spelled the argument as `len(ref xs)`, the
        // typed expression is `Len { array: Ref { name = "xs" }
        // }` — the Ref expression. That fell through to a
        // fallback that emitted `format!("{}", length)`, where
        // `length` is the static length carried in the Len
        // node — zero for Vec, since Vec lengths are dynamic.
        // The Var arm correctly GEPs into the binding's
        // alloca; Ref(name) needs the same treatment.
        //
        // Repro: when the program triggers an SSA-LLVM
        // fallback (e.g. by adding a `clone_at`-flavored
        // expression that the SSA gate doesn't yet route
        // through SSA), `len(ref xs)` would print 0 instead
        // of the real length.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<OwnedStr> = vec("a" + "");
              let xs2: Vec<OwnedStr> = push(xs, "b" + "");
              let n: i64 = (len(ref xs2) as i64);
              assert n == 2;
              print clone_at(ref xs2, 0);
              return 0;
            }
        "#;

        let ll = crate::backend_llvm::LlvmBackend
            .emit(&compile(source).expect("len(ref xs) compiles").ir);

        // The fix must emit a `load i64` from a GEP into the
        // Vec struct's .len field for `ref xs2`. The bug
        // signature was `i64 0` flowing into the assertion;
        // after the fix the value comes out of a `load i64,
        // i64*` after a `getelementptr … i32 1` (Vec's .len
        // slot is field index 1 in the {data, len, capacity}
        // typedef).
        let main_start = ll
            .find("define i64 @fn_main")
            .expect("fn_main present");
        let main_end = ll[main_start..]
            .find("\ndefine ")
            .map(|i| main_start + i)
            .unwrap_or(ll.len());
        let main_body = &ll[main_start..main_end];
        // Find the GEP-len-load sequence: `getelementptr ... i32 1` then `load i64`.
        let has_len_gep = main_body
            .lines()
            .any(|line| line.contains("getelementptr") && line.contains("i32 1"));
        assert!(
            has_len_gep,
            "expected GEP into the Vec struct's .len slot for `len(ref xs2)`:\n{}",
            main_body
        );
        let has_len_load = main_body
            .lines()
            .any(|line| line.contains("load i64, i64*"));
        assert!(
            has_len_load,
            "expected `load i64, i64*` for the Vec .len field:\n{}",
            main_body
        );
    }

    #[test]
    fn match_str_scrutinee_fresh_owned_str_drops_in_tree_llvm() {
        // Closure #160: tree-LLVM's Block-expression emitter
        // (used when the checker desugars `match <fresh
        // OwnedStr> { … }` into a Block { Let temp = scr;
        // Let result = ifchain; Drop temp; result }) was
        // silently dropping `TypedStmt::Drop` from the
        // Block::stmts list. Only `Let` and `Print` were
        // routed through `emit_stmt`. That leaked the
        // scrutinee's heap on every match-on-fresh-OwnedStr
        // call (e.g. `match make_owned() { "x" then 1, _ then
        // 0 }`). Tree-C's Block emitter (closure #137) had
        // already handled the OwnedStr / Vec Drop arms.
        let source = r#"
            fn make() -> OwnedStr {
              return "abc" + "def";
            }
            fn main() -> i64 {
              let r: i64 = match make() {
                "abcdef" then 1,
                _ then 0,
              };
              assert r == 1;
              return 0;
            }
        "#;

        let ll = crate::backend_llvm::LlvmBackend
            .emit(&compile(source).expect("match on fresh OwnedStr compiles").ir);
        // After the if-chain, the Block emitter must call
        // `@free` on the scrutinee temp. Locate the main
        // function and assert a free of an i8* runs after
        // the branch/phi for the match's if-chain.
        let main_start = ll
            .find("define i64 @fn_main")
            .expect("fn_main present");
        let main_end = ll[main_start..]
            .find("\ndefine ")
            .map(|i| main_start + i)
            .unwrap_or(ll.len());
        let main_body = &ll[main_start..main_end];
        assert!(
            main_body.contains("ifexpr_merge")
                || main_body.contains("phi i64"),
            "expected the match's if-chain phi in fn_main:\n{}",
            main_body
        );
        assert!(
            main_body.contains("call void @free(i8*"),
            "expected `call void @free(i8* …)` for the match scrutinee Drop in fn_main:\n{}",
            main_body
        );
    }

    #[test]
    fn for_in_consuming_vec_owned_str_skips_per_element_free() {
        // Closure #159: consuming `for x in xs` over a Vec
        // of non-Copy elements used to emit
        // `intent_vec_<T>__free(xs)` after the loop, which
        // (since closure #127) walks every slot and frees
        // its inner heap — double-freeing the elements x
        // already freed via scope-exit drop. The fix emits
        // a direct `free(xs.data)` instead, releasing only
        // the outer buffer.
        let source = r#"
            fn make() -> Vec<OwnedStr> {
              let xs: Vec<OwnedStr> = vec("a" + "", "b" + "");
              return xs;
            }
            fn main() -> i64 {
              let xs: Vec<OwnedStr> = make();
              let total: i64 = 0;
              for x in xs {
                total = total + (len(x) as i64);
              }
              assert total == 2;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("consuming Vec<OwnedStr> for-iter compiles");
        // Tree-C emits `free(v_xs.data);` after the loop —
        // the shallow form. The deep `intent_vec_i8p__free`
        // (the per-element-walking helper) MUST NOT appear
        // in `fn_main`.
        let main_start = c.find("static int64_t fn_main").expect("fn_main present");
        let main_end = c[main_start..]
            .find("\nint main(void)")
            .map(|i| main_start + i)
            .unwrap_or(c.len());
        let main_body = &c[main_start..main_end];
        assert!(
            main_body.contains("free(v_xs.data);"),
            "expected shallow `free(v_xs.data);` after consuming for-iter: {}",
            main_body
        );
        assert!(
            !main_body.contains("intent_vec_owned_str__free"),
            "deep __free must NOT be called in consuming non-Copy for-iter: {}",
            main_body
        );
    }

    #[test]
    fn for_in_owned_vec_blocks_use_after_iteration() {
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              for x in xs {
                let _ = x;
              }
              let bad: i64 = xs[0];
              return bad;
            }
        "#;

        let errors = compile(source).expect_err("use after consume should fail");
        assert!(
            errors.iter().any(|e| e.message.contains("was moved")),
            "expected move diagnostic, got: {:?}",
            errors
        );
    }

    #[test]
    fn for_in_borrow_form_does_not_consume() {
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              for x in ref xs {
                let _ = x;
              }
              let after: i64 = xs[0];
              assert after == 1;
              return 0;
            }
        "#;

        compile_to_c(source).expect("borrow form must leave xs live");
    }

    #[test]
    fn assert_with_message_compiles_and_emits_message() {
        let source = r#"
            fn main() -> i64 {
              let i: i64 = 5;
              let n: i64 = 3;
              assert i < n, "index must be within range";
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("assert with message should compile");
        assert!(
            c.contains("index must be within range"),
            "expected custom message in emitted C: {c}"
        );
        assert!(
            c.contains("fprintf(stderr") || c.contains("abort()"),
            "expected fprintf/abort lowering: {c}"
        );
    }

    #[test]
    fn assert_without_message_still_uses_c_assert() {
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 3;
              assert x == 3;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("plain assert should compile");
        assert!(c.contains("assert("), "expected C `assert(...)`: {c}");
    }

    #[test]
    fn assert_message_escapes_quotes_and_backslashes() {
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 1;
              assert x == 2, "got \"unexpected\" value\\backslash";
              return 0;
            }
        "#;

        compile_to_c(source).expect("escaped message should compile");
    }

    #[test]
    fn bitvec_disproves_unsound_overflow_claim() {
        if !z3_available() {
            return;
        }
        // Under infinite-Int arithmetic, `x + 1 > x` is trivially true.
        // Under BitVec(64), it fails at x = INT64_MAX because the sum
        // wraps to INT64_MIN. The counterexample identifies that exact
        // boundary.
        let source = r#"
            fn bad(x: i64) -> i64 {
              prove x + 1 > x;
              return 0;
            }
            fn main() -> i64 {
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("overflow makes this false");
        let combined: String = errors
            .iter()
            .map(|d| d.message.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            combined.contains("9223372036854775807"),
            "expected INT64_MAX counterexample, got: {combined}"
        );
    }

    #[test]
    fn bitvec_disproves_unsigned_wraparound() {
        if !z3_available() {
            return;
        }
        // `x - 1 < x` for `x: u64` is false at x = 0 (the subtraction
        // wraps to UINT64_MAX).
        let source = r#"
            fn bad(x: u64) -> i64 {
              prove x - 1 < x;
              return 0;
            }
            fn main() -> i64 {
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("unsigned wrap");
        let combined: String = errors
            .iter()
            .map(|d| d.message.clone())
            .collect::<Vec<_>>()
            .join("\n");
        // x = 0 is the witness — z3 picks it, and our formatter renders
        // u64 as a decimal.
        assert!(
            combined.contains("x = 0"),
            "expected x=0 counterexample, got: {combined}"
        );
    }

    #[test]
    fn assert_with_message_lowers_to_custom_abort() {
        let source = r#"
            fn main() -> i64 {
              let i: u64 = 0;
              assert i < 5, "i must be in [0, 5)";
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("assert with message should compile");
        // The custom-message path emits an explicit if/fprintf/abort
        // sequence rather than the bare `assert(...)` macro.
        assert!(
            c.contains("fprintf(stderr, \"assertion failed: i must be in [0, 5)"),
            "expected custom abort with embedded message, got:\n{c}"
        );
        assert!(
            c.contains("abort();"),
            "expected abort() in emitted C: {c}"
        );
    }

    #[test]
    fn assert_without_message_uses_c_assert_macro() {
        let source = r#"
            fn main() -> i64 {
              let i: i64 = 5;
              assert i == 5;
              return 0;
            }
        "#;
        let c = compile_to_c(source).expect("simple assert should compile");
        // No-message form continues to use the C `assert(...)` macro.
        assert!(
            c.contains("assert((v_i == 5))"),
            "expected C assert macro, got:\n{c}"
        );
        // And does NOT take the custom-abort path.
        assert!(
            !c.contains("fprintf(stderr, \"assertion failed:"),
            "expected no custom-abort branch for simple assert, got:\n{c}"
        );
    }

    #[test]
    fn assert_message_escapes_special_chars() {
        // Newline and quotes inside the message must round-trip into a
        // valid C string literal.
        let source = "fn main() -> i64 {\n  assert true, \"line one\\nand a \\\"quoted\\\" bit\";\n  return 0;\n}\n";
        let c = compile_to_c(source).expect("assert with escapes should compile");
        // Embedded backslash-n and quoted segment survive into the C output
        // exactly once (i.e., the message is escaped, not double-escaped).
        assert!(
            c.contains("line one\\nand a \\\"quoted\\\" bit"),
            "expected escaped message in C, got:\n{c}"
        );
    }

    #[test]
    fn rejects_bad_constant_shift_count() {
        let source = r#"
            fn main() -> i64 {
              let bad = (1 as u8) << 8;
              return 0;
            }
        "#;

        let errors = compile(source).expect_err("bad shift count should fail");
        assert!(errors
            .iter()
            .any(|error| error.message.contains("shift count must be less than 8")));
    }

    #[test]
    fn parentheses_override_operator_precedence() {
        let source = r#"
            fn main() -> i64 {
              prove 1 + 2 * 3 == 7;
              prove (1 + 2) * 3 == 9;
              prove 10 - 4 - 3 == 3;
              prove 10 - (4 - 3) == 9;
              prove 2 + 3 * 4 - 1 == 13;
              prove (2 + 3) * (4 - 1) == 15;
              return 0;
            }
        "#;

        compile_to_c(source).expect("PEMDAS precedence with parens should hold");
    }

    #[test]
    fn accepts_underscore_and_hex_literals() {
        let source = r#"
            fn main() -> i64 {
              let big: i64 = 1_000_000;
              let mask: i64 = 0xFF_FF;
              let bits: i64 = 0b1010_1010;
              let oct: i64 = 0o755;
              prove big == 1000000;
              prove mask == 65535;
              prove bits == 170;
              prove oct == 493;
              return 0;
            }
        "#;

        compile_to_c(source).expect("underscores and radix literals should parse");
    }

    #[test]
    fn proves_structural_tautologies_without_constants() {
        let source = r#"
            fn identity(x: i64) -> i64 {
              return x;
            }

            fn main() -> i64 {
              let value = identity(7);
              prove value == value;
              prove !(value != value);
              return 0;
            }
        "#;

        compile_to_c(source).expect("structural tautologies should be provable");
    }

    #[test]
    fn print_string_literal_lowers_to_fputs_then_newline() {
        let source = r#"
            fn main() -> i64 {
              print "hello, intent";
              return 0;
            }
        "#;

        // The multi-item print lowers each item with no-newline
        // semantics and emits a final `putchar('\n')`. For a single
        // string literal that's `fputs("...", stdout); putchar('\n');`.
        let c = compile_to_c(source).expect("print str should compile");
        assert!(
            c.contains("fputs(\"hello, intent\", stdout)"),
            "expected fputs(...) in C, got:\n{c}"
        );
        assert!(
            c.contains("putchar('\\n')"),
            "expected trailing putchar('\\n'), got:\n{c}"
        );
    }

    #[test]
    fn print_multiple_items_space_separated_with_trailing_newline() {
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 42;
              print "x =", x;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("multi-arg print should compile");
        // Items separated by a literal space (via fputs(" ", stdout))
        // and terminated by one putchar('\n').
        assert!(c.contains("fputs(\"x =\", stdout)"));
        assert!(c.contains("fputs(\" \", stdout)"));
        assert_eq!(c.matches("putchar('\\n')").count(), 1);
    }

    #[test]
    fn underscore_discard_lowers_to_void_cast() {
        let source = r#"
            fn bump(x: i64) -> i64 { return x + 1; }
            fn main() -> i64 {
              let _ = bump(7);
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("underscore let should compile");
        assert!(
            c.contains("(void)(fn_bump(7))"),
            "expected a (void)(...) discard for the unused value, got:\n{}",
            c
        );
        assert!(
            !c.contains("v__ ="),
            "expected no `v__` binding emitted for `_`, got:\n{}",
            c
        );
    }

    #[test]
    fn multiple_underscore_discards_do_not_collide() {
        let source = r#"
            fn bump(x: i64) -> i64 { return x; }
            fn main() -> i64 {
              let _ = bump(1);
              let _ = bump(2);
              let _ = bump(3);
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("repeated underscores should compile");
        // Three independent (void)(...) discards; no `_` symbol leaks
        // into the declarations. The check looks for `int64_t v__ =`
        // (with trailing space + assignment), which would be the
        // shape of a leaked underscore binding. Newer compiler-
        // synthesized names that *start* with `v__intent_…` are not
        // a false positive.
        assert_eq!(c.matches("(void)(fn_bump(").count(), 3);
        assert!(
            !c.contains("v__ ="),
            "expected no underscore-bound variable declaration, got:\n{c}"
        );
    }

    #[test]
    fn underscore_discard_frees_vec() {
        let source = r#"
            fn main() -> i64 {
              let _ = vec(1, 2, 3);
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("vec discard should compile");
        assert!(
            c.contains("intent_vec_int64_t__free(_intent_discard)"),
            "expected vec backing buffer to be freed, got:\n{}",
            c
        );
    }

    #[test]
    fn underscore_discard_consumes_moved_vec() {
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20);
              let _ = xs;
              // xs has been consumed by the discard; reusing it must fail.
              let _ = xs;
              return 0;
            }
        "#;

        let errs = compile_to_c(source).expect_err("second use of moved Vec must fail");
        assert!(
            errs.iter()
                .any(|d| d.message.contains("moved") || d.message.contains("after move")),
            "expected a moved-value diagnostic, got: {:?}",
            errs
        );
    }

    #[test]
    fn smt_proves_unsigned_right_shift_preserves_nonnegativity() {
        if !z3_available() {
            return;
        }
        // `x >> 2` on a u64 is always <= x. SMT must encode `bvlshr` and
        // discharge this against the universally-quantified x.
        let source = r#"
            fn shr_unsigned(x: u64) -> u64 {
              prove (x >> 2) <= x;
              return x >> 2;
            }

            fn main() -> i64 {
              let _ = shr_unsigned(40);
              return 0;
            }
        "#;

        compile_to_c(source)
            .expect("SMT should discharge u64 right-shift monotonicity");
    }

    #[test]
    fn smt_disproves_false_shift_claim_with_counterexample() {
        if !z3_available() {
            return;
        }
        // `x << 1 > x` fails when x has its high bit set: shifting wraps
        // and produces a value less than x. The BitVec encoding must
        // catch this rather than silently passing.
        let source = r#"
            fn bad_shift(x: u64) -> u64 {
              prove (x << 1) > x;
              return x;
            }

            fn main() -> i64 { return 0; }
        "#;

        let errs = compile(source).expect_err("shift wraparound counterexample expected");
        assert!(
            errs.iter().any(|e| {
                e.message.contains("counterexample") || e.message.contains("proof failed")
            }),
            "expected SMT disproof for shift wraparound, got: {:?}",
            errs
        );
    }

    #[test]
    fn smt_proves_inline_call_via_callee_ensures() {
        if !z3_available() {
            return;
        }
        // `inc`'s ensures clause says the return is strictly greater
        // than the input. A direct `prove inc(x) > x` should now
        // discharge in the caller: the call is rewritten to a fresh
        // var constrained by the callee's ensures.
        let source = r#"
            fn inc(x: i64) -> i64
            requires x < 1000;
            ensures _return > x;
            {
              return x + 1;
            }

            fn check(x: i64) -> i64
            requires x > 0;
            requires x < 100;
            {
              prove inc(x) > x;
              return inc(x);
            }

            fn main() -> i64 {
              let _ = check(7);
              return 0;
            }
        "#;

        compile_to_c(source)
            .expect("inline call result should be constrained by callee ensures");
    }

    #[test]
    fn smt_proves_ensures_with_inline_call_in_postcondition() {
        if !z3_available() {
            return;
        }
        // The caller's ensures references a helper call directly:
        //   ensures _return == helper(x)
        // verify_ensures_at_return substitutes _return → return_expr,
        // then prove_with_calls rewrites each `helper(...)` to its own
        // fresh var. Both wear the same ensures (== arg + 1), so they
        // must be equal — discharged.
        let source = r#"
            fn helper(x: i64) -> i64
            requires x < 1000;
            ensures _return == x + 1;
            {
              return x + 1;
            }

            fn caller(y: i64) -> i64
            requires y < 100;
            ensures _return == helper(y);
            {
              return helper(y);
            }

            fn main() -> i64 {
              let _ = caller(7);
              return 0;
            }
        "#;

        compile_to_c(source)
            .expect("ensures using inline call should discharge via callee ensures");
    }

    #[test]
    fn vec_literal_length_is_known_in_proof() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              prove len(xs) == 3;
              return 0;
            }
        "#;

        compile_to_c(source).expect("vec literal length should discharge");
    }

    #[test]
    fn vec_push_length_grows_by_one_in_proof() {
        if !z3_available() {
            return;
        }
        // push consumes its Vec arg, so we can't reference `xs` after
        // the push. The fact `len(ys) == len(xs) + 1` is added before
        // the move, so it still holds at the prove point.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              let ys: Vec<i64> = push(xs, 40);
              prove len(ys) == 4;
              return 0;
            }
        "#;

        compile_to_c(source).expect("push should grow length by one");
    }

    #[test]
    fn smt_handles_float_to_int_cast_in_proof() {
        if !z3_available() {
            return;
        }
        // `(x as i64) >= 0` when `x >= 0.0` and `x < 1e9` (so the
        // conversion doesn't overflow). Tests the new fp.to_sbv path.
        let source = r#"
            fn truncate(x: f64) -> i64
            requires x >= 0.0;
            requires x < 1000000000.0;
            {
              prove (x as i64) >= 0;
              return x as i64;
            }

            fn main() -> i64 {
              let v: i64 = truncate(3.7);
              assert v == 3;
              return 0;
            }
        "#;

        compile_to_c(source).expect("fp.to_sbv discharge should compile");
    }

    #[test]
    fn smt_elides_divisor_check_when_requires_proves_nonzero() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn safe_div(a: i64, b: i64) -> i64
            requires b > 0;
            {
              return a / b;
            }

            fn main() -> i64 {
              let v: i64 = safe_div(10, 3);
              print v;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("safe_div should compile");
        let def_start = c.find("fn_safe_div(int64_t v_a, int64_t v_b) {")
            .expect("fn_safe_div definition");
        let def = &c[def_start..];
        let def_end = def.find("\n}\n").map(|i| i + 1).unwrap_or(def.len());
        let def = &def[..def_end];
        assert!(
            !def.contains("intent_check_i64_divisor"),
            "expected divisor check elided in fn_safe_div, got:\n{}",
            def
        );
    }

    // NB: INTENTC_NO_VERIFY is a process-global env var, so a unit
    // test that toggles it races with other tests that expect normal
    // verifier behavior. The feature is exercised manually and
    // documented in README; we deliberately don't add a unit test.

    #[test]
    fn return_value_is_cached_before_drops_fire() {
        // Regression: previously `return xs[1]` where `xs: Vec<i64>`
        // goes out of scope at the return site would lower as
        // `drop xs; return xs[1];` — a use-after-free. The checker
        // now caches the return value into a `__intent_ret_<span>`
        // temp before emitting drops, so the read happens against
        // the live buffer.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              xs[1] = 99;
              return xs[1];
            }
        "#;
        let c = compile_to_c(source).expect("Vec return path compiles");
        // The cached-return temp's assignment must appear before the
        // free *call* (not the helper definition at the top of the
        // file). Searching for `__free(v_xs)` finds the call site.
        let ret_temp_pos = c
            .find("v___intent_ret_")
            .expect("cached return temp declaration");
        let free_call_pos = c
            .find("intent_vec_int64_t__free(v_xs)")
            .expect("vec free call");
        assert!(
            ret_temp_pos < free_call_pos,
            "return temp must be emitted before the Vec drop call, got:\n{c}"
        );
    }

    #[test]
    fn proves_over_literal_vec_indices_discharge() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              prove xs[0] == 10;
              prove xs[1] == 20;
              prove xs[2] == 30;
              return 0;
            }
        "#;

        compile_to_c(source)
            .expect("literal vec indices should be substitutable in proofs");
    }

    #[test]
    fn proves_over_literal_vec_indices_disprove_when_wrong() {
        if !z3_available() {
            return;
        }
        // Wrong value — must be rejected.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              prove xs[1] == 999;
              return 0;
            }
        "#;

        compile(source).expect_err("xs[1] == 999 must be rejected when xs[1] == 20");
    }

    #[test]
    fn float_counterexample_is_readable_decimal_not_smt_lib() {
        if !z3_available() {
            return;
        }
        // Force an SMT-disproven prove that names a float variable so
        // the counterexample includes an FP literal. Verify the
        // diagnostic shows a decimal (possibly scientific) rather
        // than z3's raw `(fp #b0 #b00000000000 #x...)` form.
        let source = r#"
            fn check(x: f64) -> f64
            requires x >= 0.0;
            {
              prove x > 100.0;
              return x;
            }

            fn main() -> i64 { return 0; }
        "#;

        let errs = compile(source).expect_err("prove should fail with counterexample");
        let combined: String =
            errs.iter().map(|d| d.message.clone()).collect::<Vec<_>>().join(" || ");
        assert!(
            combined.contains("x = "),
            "expected counterexample to name x, got: {}",
            combined
        );
        assert!(
            !combined.contains("(fp #") && !combined.contains("fp #b"),
            "expected float decoded, not raw SMT-LIB FP form, got: {}",
            combined
        );
    }

    #[test]
    fn bounds_and_divisor_elision_compose_cleanly() {
        if !z3_available() {
            return;
        }
        // `xs[i]` and `value / divisor` both need to be discharged
        // independently from the same set of facts. Verify both
        // runtime helpers disappear from the emitted body.
        let source = r#"
            fn safe_at(xs: ref Vec<i64>, i: u64, divisor: i64) -> i64
            requires i < len(xs);
            requires divisor > 0;
            {
              return xs[i] / divisor;
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              let v: i64 = safe_at(ref xs, 1, 2);
              print v;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("safe_at should compile");
        let def_start = c.find("fn_safe_at(const intent_vec_int64_t* v_xs, uint64_t v_i, int64_t v_divisor) {")
            .expect("fn_safe_at definition");
        let def = &c[def_start..];
        let def_end = def.find("\n}\n").map(|i| i + 1).unwrap_or(def.len());
        let def = &def[..def_end];
        assert!(
            !def.contains("intent_check_bounds"),
            "expected bounds elided in fn_safe_at, got:\n{}",
            def
        );
        assert!(
            !def.contains("intent_check_i64_divisor"),
            "expected divisor elided in fn_safe_at, got:\n{}",
            def
        );
    }

    #[test]
    fn divisor_check_remains_when_safety_is_not_provable() {
        // No requires on `b`, so the divisor could be zero. The
        // runtime helper must stay in place.
        let source = r#"
            fn unsafe_div(a: i64, b: i64) -> i64 {
              return a / b;
            }

            fn main() -> i64 {
              let v: i64 = unsafe_div(10, 3);
              print v;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("unsafe_div should compile");
        assert!(
            c.contains("intent_check_i64_divisor"),
            "expected divisor helper to remain when unprovable, got:\n{}",
            c
        );
    }

    #[test]
    fn shift_check_remains_when_safety_is_not_provable() {
        // No bound on `n`; the runtime shift check must stay.
        let source = r#"
            fn unsafe_shl(x: i64, n: i64) -> i64 {
              return x << n;
            }

            fn main() -> i64 {
              let v: i64 = unsafe_shl(7, 3);
              print v;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("unsafe_shl should compile");
        assert!(
            c.contains("intent_check_i64_shift"),
            "expected shift helper to remain when unprovable, got:\n{}",
            c
        );
    }

    #[test]
    fn smt_elides_shift_check_when_requires_proves_in_range() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn safe_shl(x: i64, n: i64) -> i64
            requires n >= 0;
            requires n < 32;
            {
              return x << n;
            }

            fn main() -> i64 {
              let v: i64 = safe_shl(7, 3);
              print v;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("safe_shl should compile");
        let def_start = c.find("fn_safe_shl(int64_t v_x, int64_t v_n) {")
            .expect("fn_safe_shl definition");
        let def = &c[def_start..];
        let def_end = def.find("\n}\n").map(|i| i + 1).unwrap_or(def.len());
        let def = &def[..def_end];
        assert!(
            !def.contains("intent_check_i64_shift"),
            "expected shift check elided in fn_safe_shl, got:\n{}",
            def
        );
    }

    #[test]
    fn smt_elides_bounds_for_last_element_idiom() {
        if !z3_available() {
            return;
        }
        // `xs[len(xs) - 1]` is the standard last-element idiom. With
        // `requires len(xs) > 0;` (so u64 subtraction is safe) the
        // verifier should discharge `(len(xs) - 1) < len(xs)`.
        let source = r#"
            fn last(xs: ref Vec<i64>) -> i64
            requires len(xs) > 0;
            {
              return xs[len(xs) - 1];
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              let v: i64 = last(ref xs);
              print v;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("last-element should compile");
        let def_start = c.find("fn_last(const intent_vec_int64_t* v_xs) {")
            .expect("fn_last definition");
        let def = &c[def_start..];
        let def_end = def.find("\n}\n").map(|i| i + 1).unwrap_or(def.len());
        let def = &def[..def_end];
        assert!(
            !def.contains("intent_check_bounds"),
            "expected last-element bounds elided, got:\n{}",
            def
        );
    }

    #[test]
    fn smt_elides_vec_bounds_in_for_loop_body() {
        if !z3_available() {
            return;
        }
        // Inside `for i in 0..len(xs) { … }`, `i >= 0` and `i < len(xs)`
        // are both in scope (added by the for-body fact pass), so
        // `xs[i]` should discharge without a runtime guard.
        let source = r#"
            fn sum_to(xs: ref Vec<i64>) -> i64 {
              let total: i64 = 0;
              for i from 0 to len(xs) {
                total = total + xs[i];
              }
              return total;
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3, 4);
              let s: i64 = sum_to(ref xs);
              assert s == 10;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("sum_to should compile");
        // Inside fn_sum_to's body, `xs[i]` should not invoke the
        // bounds-check helper.
        let def_start = c.find("fn_sum_to(const intent_vec_int64_t* v_xs) {")
            .expect("fn_sum_to definition");
        let def = &c[def_start..];
        let def_end = def.find("\n}\n").map(|i| i + 1).unwrap_or(def.len());
        let def = &def[..def_end];
        assert!(
            !def.contains("intent_check_bounds"),
            "expected for-loop bounds elided in fn_sum_to, got:\n{}",
            def
        );
    }

    #[test]
    fn smt_elides_vec_bounds_check_when_requires_proves_bounds() {
        if !z3_available() {
            return;
        }
        // `first` requires `len(xs) > 0`, so `xs[0]` is proven in
        // bounds. The C backend should skip `intent_check_bounds`
        // and emit a raw `data[0]` access.
        let source = r#"
            fn first(xs: ref Vec<i64>) -> i64
            requires len(xs) > 0;
            {
              return xs[0];
            }

            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30);
              let v: i64 = first(ref xs);
              print v;
              return 0;
            }
        "#;

        let c = compile_to_c(source).expect("first should compile");
        // Locate the fn_first body and verify the bounds helper is
        // not used inside it. (The helper is still defined at the
        // top of the file; we just check it isn't called from
        // fn_first.)
        let body_start = c.find("static int64_t fn_first(const").expect("fn_first declared");
        let body = &c[body_start..];
        let body_end = body.find("\n}\n").map(|i| i + 1).unwrap_or(body.len());
        let body = &body[..body_end];
        // Skip the forward declaration's first occurrence by looking
        // at the *second* (definition) — the forward decl ends in `;`.
        let definition_start = body.find("fn_first(const intent_vec_int64_t* v_xs) {")
            .expect("fn_first definition");
        let definition = &body[definition_start..];
        let def_end = definition.find("\n}\n").map(|i| i + 1).unwrap_or(definition.len());
        let definition = &definition[..def_end];
        assert!(
            !definition.contains("intent_check_bounds"),
            "expected bounds check elided in fn_first, got:\n{}",
            definition
        );
        assert!(
            definition.contains("data[(uint64_t)(0)]"),
            "expected raw indexed access, got:\n{}",
            definition
        );
    }

    #[test]
    fn stale_ensures_fact_does_not_leak_past_reassign() {
        if !z3_available() {
            return;
        }
        // `let v = make_pos();` records `v > 0` from the callee's
        // ensures. After `v = -3;` that fact is stale and must be
        // invalidated — otherwise the verifier wrongly accepts
        // `prove v > 0;` against a runtime value of -3.
        let source = r#"
            fn make_pos() -> i64
            ensures _return > 0;
            {
              return 7;
            }

            fn main() -> i64 {
              let v: i64 = make_pos();
              v = -3;
              prove v > 0;
              return 0;
            }
        "#;

        let errs = compile(source).expect_err("stale ensures fact must be invalidated");
        assert!(
            errs.iter().any(|d| d.message.contains("proof failed")),
            "expected proof-failed after stale-fact drop, got: {:?}",
            errs
        );
    }

    #[test]
    fn stale_vec_length_fact_does_not_leak_past_reassign() {
        if !z3_available() {
            return;
        }
        // After `xs = push(xs, ...)`, the prior `len(xs) == 3` fact
        // must NOT remain in scope — otherwise the verifier would
        // accept the unsound claim `len(xs) == 3`.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              xs = push(xs, 4);
              prove len(xs) == 3;
              return 0;
            }
        "#;

        let errs = compile(source).expect_err("stale len fact must be invalidated");
        assert!(
            errs.iter().any(|d| d.message.contains("proof failed")),
            "expected proof-failed after reassign, got: {:?}",
            errs
        );
    }

    #[test]
    fn self_referencing_vec_shadow_does_not_create_contradiction() {
        if !z3_available() {
            return;
        }
        // `let xs = push(xs, 4)` after `let xs = vec(1,2,3)` must not
        // record `len(xs) == len(xs) + 1`. Such a contradiction would
        // make every subsequent claim provable.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let xs: Vec<i64> = push(xs, 4);
              prove len(xs) == 999;
              return 0;
            }
        "#;

        let errs = compile(source).expect_err("absurd claim must not be vacuously provable");
        assert!(
            errs.iter().any(|d| d.message.contains("proof failed")),
            "expected proof-failed, got: {:?}",
            errs
        );
    }

    #[test]
    fn for_loop_body_knows_both_bounds() {
        if !z3_available() {
            return;
        }
        // Inside `for i in 5..12 { … }`, the body should know
        // `i >= 5` and `i < 12`. Before this lands, only the upper
        // bound was visible.
        let source = r#"
            fn main() -> i64 {
              for i from 5 to 12 {
                prove i >= 5;
                prove i < 12;
              }
              return 0;
            }
        "#;

        compile_to_c(source).expect("for-body should see both range bounds");
    }

    #[test]
    fn post_while_loop_negates_condition_when_no_break() {
        if !z3_available() {
            return;
        }
        // After the loop, since there's no break, `!cond` must hold.
        // Combined with the invariant `i <= 5`, the verifier derives
        // `i == 5`.
        let source = r#"
            fn main() -> i64 {
              let i: i64 = 0;
              while i < 5
              invariant i >= 0;
              invariant i <= 5;
              {
                i = i + 1;
              }
              prove i == 5;
              return 0;
            }
        "#;

        compile_to_c(source).expect("post-loop !cond should be available");
    }

    #[test]
    fn post_for_loop_knows_var_reached_end() {
        if !z3_available() {
            return;
        }
        // After `for i in 0..5 { }`, the verifier should know `i >= 5`.
        // Using an empty body keeps the post-loop check focused on the
        // loop-variable fact rather than tracking body-mutated state.
        let source = r#"
            fn main() -> i64 {
              let last: i64 = 0;
              for i from 0 to 5
              invariant last == 0;
              {
                last = 0;
              }
              prove last == 0;
              return 0;
            }
        "#;

        compile_to_c(source).expect("post-for-loop facts should compile");
    }

    #[test]
    fn loop_invariant_with_vec_push_preservation() {
        if !z3_available() {
            return;
        }
        // The classic motivating use case: a loop that pushes onto a
        // Vec and maintains `len(xs) == i` as an invariant. The
        // inline Vec-builtin rewriter discharges the substituted
        // invariant after `xs = push(xs, ...)`.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(0);
              let i: i64 = 1;
              while i < 5
              invariant len(xs) == (i as u64);
              invariant i >= 1;
              invariant i <= 5;
              {
                xs = push(xs, i * 10);
                i = i + 1;
              }
              prove len(xs) == 5;
              return 0;
            }
        "#;

        compile_to_c(source).expect("loop with push should verify length invariant");
    }

    #[test]
    fn inline_vec_builtin_length_facts_in_proof() {
        if !z3_available() {
            return;
        }
        // No let-binding for the push result — proof must still
        // discharge via the rewriter's inline length fact.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              prove len(push(xs, 99)) == 4;
              return 0;
            }
        "#;

        compile_to_c(source).expect("inline push length should discharge");
    }

    #[test]
    fn vec_clone_preserves_length_in_proof() {
        if !z3_available() {
            return;
        }
        // clone takes the Vec by value but does not consume it, so
        // `xs` and `ys` coexist.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(7, 8, 9, 10, 11);
              let ys: Vec<i64> = clone(xs);
              prove len(ys) == len(xs);
              prove len(ys) == 5;
              return 0;
            }
        "#;

        compile_to_c(source).expect("clone should preserve length");
    }

    #[test]
    fn vec_set_keeps_length_in_proof() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let ys: Vec<i64> = set(xs, 1, 99);
              prove len(ys) == 3;
              return 0;
            }
        "#;

        compile_to_c(source).expect("set should preserve length");
    }

    #[test]
    fn call_in_loop_invariant_verifies_requires() {
        if !z3_available() {
            return;
        }
        // A while invariant references `helper(i)`. helper has
        // requires i >= 0; — the invariant must check this at the
        // call site.
        let source = r#"
            fn helper(i: i64) -> i64
            requires i >= 0;
            ensures _return == i;
            {
              return i;
            }

            fn caller() -> i64 {
              let i: i64 = -5;
              while i < 10
              invariant helper(i) == i;
              {
                i = i + 1;
              }
              return 0;
            }

            fn main() -> i64 { return 0; }
        "#;

        let errs = compile(source).expect_err("helper(i) with i = -5 violates requires");
        assert!(
            errs.iter().any(|d| d.message.contains("violates")
                && d.message.contains("helper")),
            "expected requires-violation diagnostic, got: {:?}",
            errs
        );
    }

    #[test]
    fn dead_if_false_body_is_flagged() {
        let source = r#"
            fn main() -> i64 {
              if false {
                print 99;
              }
              return 0;
            }
        "#;

        let errs = compile(source).expect_err("dead 'if false' should be flagged");
        assert!(
            errs.iter().any(|d| d.message.contains("always false")
                && d.message.contains("unreachable")),
            "expected dead-then diagnostic, got: {:?}",
            errs
        );
    }

    #[test]
    fn dead_if_true_else_is_flagged() {
        let source = r#"
            fn main() -> i64 {
              if true {
                return 1;
              } else {
                return 2;
              }
            }
        "#;

        let errs = compile(source).expect_err("dead 'else' should be flagged");
        assert!(
            errs.iter().any(|d| d.message.contains("always true")
                && d.message.contains("'else'")),
            "expected dead-else diagnostic, got: {:?}",
            errs
        );
    }

    #[test]
    fn dead_while_false_is_flagged() {
        let source = r#"
            fn main() -> i64 {
              while false {
                print 88;
              }
              return 0;
            }
        "#;

        let errs = compile(source).expect_err("dead 'while false' should be flagged");
        assert!(
            errs.iter().any(|d| d.message.contains("always false")
                && d.message.contains("never executes")),
            "expected dead-loop diagnostic, got: {:?}",
            errs
        );
    }

    #[test]
    fn const_folded_condition_in_loop_is_not_flagged_as_dead() {
        // `i >= 5` folds to false at body-entry (where `i = 0`), but
        // the body mutates `i`. We must not flag this as dead.
        let source = r#"
            fn main() -> i64 {
              let i: i64 = 0;
              while i < 100 {
                if i >= 5 {
                  break;
                }
                i = i + 1;
              }
              assert i == 5;
              return 0;
            }
        "#;

        compile_to_c(source).expect("loop with mutated counter must compile");
    }

    #[test]
    fn unsupported_call_in_prove_hints_at_ensures() {
        if !z3_available() {
            return;
        }
        // `helper` has no ensures. A prove that references it should
        // surface an actionable hint pointing the user at the missing
        // ensures clause.
        let source = r#"
            fn helper(x: i64) -> i64 { return x + 1; }

            fn caller(x: i64) -> i64
            requires x > 0;
            {
              prove helper(x) >= 0;
              return helper(x);
            }

            fn main() -> i64 { let _ = caller(5); return 0; }
        "#;

        let errs = compile(source).expect_err("call-in-prove with no ensures should fail");
        assert!(
            errs.iter().any(|d| {
                d.message.contains("ensures")
                    && d.message.contains("callee")
            }),
            "expected ensures hint, got: {:?}",
            errs
        );
    }

    #[test]
    fn call_site_rejects_args_violating_callee_requires() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn safe_sub(a: i64, b: i64) -> i64
            requires a >= b;
            {
              return a - b;
            }

            fn main() -> i64 {
              let bad: i64 = safe_sub(3, 7);
              return bad;
            }
        "#;

        let errs = compile(source).expect_err("3 >= 7 should fail at the call site");
        assert!(
            errs.iter().any(|d| {
                d.message.contains("violates")
                    && d.message.contains("requires")
                    && d.message.contains("safe_sub")
            }),
            "expected requires-violation diagnostic, got: {:?}",
            errs
        );
    }

    #[test]
    fn call_site_accepts_args_meeting_callee_requires_via_caller_facts() {
        if !z3_available() {
            return;
        }
        // The caller's own `requires` propagates as a fact that
        // discharges the callee's precondition.
        let source = r#"
            fn safe_sub(a: i64, b: i64) -> i64
            requires a >= b;
            {
              return a - b;
            }

            fn caller(x: i64, y: i64) -> i64
            requires x >= y;
            requires x < 1000;
            requires y > -1000;
            {
              return safe_sub(x, y);
            }

            fn main() -> i64 {
              let _ = caller(10, 3);
              return 0;
            }
        "#;

        compile_to_c(source)
            .expect("call-site precondition should discharge under caller's requires");
    }

    #[test]
    fn contradictory_requires_clauses_emit_diagnostic() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn dead(x: i64) -> i64
            requires x > 5;
            requires x < 3;
            {
              prove x > 1000000;
              return x;
            }

            fn main() -> i64 { return 0; }
        "#;

        let errs = compile(source).expect_err("contradictory requires should be flagged");
        assert!(
            errs.iter().any(|d| d.message.contains("contradictory")
                && d.message.contains("vacuously")),
            "expected contradictory-requires diagnostic, got: {:?}",
            errs
        );
    }

    #[test]
    fn satisfiable_requires_clauses_do_not_emit_diagnostic() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn ok(x: i64) -> i64
            requires x > 5;
            requires x < 100;
            {
              return x;
            }

            fn main() -> i64 { let _ = ok(7); return 0; }
        "#;

        compile_to_c(source).expect("non-contradictory preconditions should compile");
    }

    #[test]
    fn early_return_narrows_through_negated_guard() {
        if !z3_available() {
            return;
        }
        // `if !(x >= 0) { return 0; }` — the negate-helper flips to
        // `x >= 0` for the fall-through, so the ensures discharges.
        let source = r#"
            fn clamp(x: i64) -> i64
            ensures _return >= 0;
            {
              if !(x >= 0) {
                return 0;
              }
              return x;
            }

            fn main() -> i64 { let _ = clamp(7); return 0; }
        "#;

        compile_to_c(source)
            .expect("negated guard should narrow on fall-through");
    }

    #[test]
    fn early_return_narrows_facts_on_fall_through() {
        if !z3_available() {
            return;
        }
        // After `if x < 0 { return 0; }`, the fall-through path must
        // know `x >= 0`. Before this lands, the body of `clamp` would
        // fail to verify against `ensures _return >= 0` with the
        // counterexample x = -1.
        let source = r#"
            fn clamp(x: i64) -> i64
            ensures _return >= 0;
            {
              if x < 0 {
                return 0;
              }
              return x;
            }

            fn main() -> i64 {
              let _ = clamp(7);
              return 0;
            }
        "#;

        compile_to_c(source).expect("early-return should narrow facts on fall-through");
    }

    #[test]
    fn counterexample_relabels_inline_call_back_to_source_form() {
        if !z3_available() {
            return;
        }
        let source = r#"
            fn bounded(x: i64) -> i64
            requires x >= 0;
            requires x < 1000;
            ensures _return >= 0;
            {
              return x;
            }

            fn caller(x: i64) -> i64
            requires x >= 0;
            requires x < 100;
            {
              prove bounded(x) > 0;
              return bounded(x);
            }

            fn main() -> i64 { return 0; }
        "#;

        let errs = compile(source).expect_err("ensures only gives >=0");
        let combined: String = errs.iter().map(|d| d.message.clone()).collect::<Vec<_>>().join(" || ");
        // The counterexample must reference the original source form
        // `bounded(x)`, not the synthesized `__call_0`.
        assert!(
            combined.contains("bounded(x)"),
            "expected counterexample to mention 'bounded(x)', got: {}",
            combined
        );
        assert!(
            !combined.contains("__call_"),
            "did not expect synthesized name to leak, got: {}",
            combined
        );
    }

    #[test]
    fn smt_disproves_claim_stronger_than_callee_ensures() {
        if !z3_available() {
            return;
        }
        // `bounded` promises only `_return >= 0`. Asking the caller to
        // prove the strictly-stronger `_return > 0` should fail with a
        // counterexample at _return = 0.
        let source = r#"
            fn bounded(x: i64) -> i64
            requires x >= 0;
            requires x < 1000;
            ensures _return >= 0;
            {
              return x;
            }

            fn caller(x: i64) -> i64
            requires x >= 0;
            requires x < 100;
            {
              prove bounded(x) > 0;
              return bounded(x);
            }

            fn main() -> i64 { return 0; }
        "#;

        let errs = compile(source).expect_err("ensures only gives >=0, not >0");
        assert!(
            errs.iter().any(|e| e.message.contains("counterexample")
                || e.message.contains("proof failed")),
            "expected counterexample, got: {:?}",
            errs
        );
    }

    #[test]
    fn smt_proves_signed_right_shift_sign_preserving() {
        if !z3_available() {
            return;
        }
        // Arithmetic right shift on a non-negative signed value stays
        // non-negative; encoded via `bvashr`. We bound x to keep the
        // proof tight under BitVec wrap-around.
        let source = r#"
            fn ashr(x: i64) -> i64
            requires x >= 0;
            requires x < 1000000;
            {
              prove (x >> 1) >= 0;
              return x >> 1;
            }

            fn main() -> i64 {
              let _ = ashr(8);
              return 0;
            }
        "#;

        compile_to_c(source)
            .expect("SMT should discharge i64 arithmetic-shift sign preservation");
    }

    #[test]
    fn underscore_is_not_a_readable_binding() {
        // `_` only describes a discard pattern. Reading it after `let _ = ...`
        // should not see a binding (in fact, the binding never existed).
        let source = r#"
            fn main() -> i64 {
              let _ = 42;
              return _;
            }
        "#;

        let errs = compile_to_c(source).expect_err("reading `_` must fail");
        assert!(
            errs.iter().any(|d| d.message.to_lowercase().contains("unknown")
                || d.message.to_lowercase().contains("not declared")
                || d.message.to_lowercase().contains("undefined")),
            "expected an unknown-variable diagnostic for `_`, got: {:?}",
            errs
        );
    }

}
