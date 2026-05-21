//! Cross-check the SSA-consuming LLVM backend against the
//! tree-based path for a curated set of programs in the
//! scalar / control-flow subset. For each source, both
//! backends generate LLVM IR; we run the SSA-IR through
//! `lli` and assert the exit code matches the expected
//! value. Mirrors the tree-based LLVM run-test infra so the
//! SSA migration doesn't silently diverge.

use std::process::Command;

use vani::compile;
use vani::ssa::lower_program;
use vani::ssa_backend_llvm;

const PROGRAMS: &[(&str, &str, i32)] = &[
    (
        "literal_return",
        "fn main() -> i64 { return 42; }",
        42,
    ),
    (
        "let_threads",
        "fn main() -> i64 { let x: i64 = 41; return x + 1; }",
        42,
    ),
    (
        "if_else_picks_branch",
        "fn main() -> i64 { if 1 < 2 { return 7; } else { return 9; } }",
        7,
    ),
    (
        "while_loop_counts_to_five",
        "fn main() -> i64 { let n: i64 = 0; while n < 5 { n = n + 1; } return n; }",
        5,
    ),
    (
        "user_fn_call_returns_inc",
        "fn inc(x: i64) -> i64 { return x + 1; } fn main() -> i64 { return inc(41); }",
        42,
    ),
    (
        "for_loop_sums_range",
        // 0+1+2+3+4 = 10
        "fn main() -> i64 { let s: i64 = 0; for i from 0 to 5 { s = s + i; } return s; }",
        10,
    ),
    (
        "array_literal_index",
        // xs = [10, 20, 30]; return xs[1] → 20
        "fn main() -> i64 { let xs: [i64; 3] = [10, 20, 30]; return xs[1]; }",
        20,
    ),
    (
        "array_index_assign",
        // xs = [1, 2, 3]; xs[2] = 99; return xs[2] → 99
        "fn main() -> i64 { let xs: [i64; 3] = [1, 2, 3]; xs[2] = 99; return xs[2]; }",
        99,
    ),
    (
        "array_loop_sum",
        // 1+2+3+4 = 10
        "fn main() -> i64 { let xs: [i64; 4] = [1, 2, 3, 4]; let s: i64 = 0; for i from 0 to 4 { s = s + xs[i]; } return s; }",
        10,
    ),
    (
        "fn_ptr_indirect_call",
        // pass `double` as an argument, call through the
        // fn-ptr; double(21) = 42
        "pure fn double(x: i64) -> i64 { return x + x; } fn apply(f: fn(i64) -> i64, x: i64) -> i64 { return f(x); } fn main() -> i64 { return apply(double, 21); }",
        42,
    ),
    (
        "vec_creates_and_indexes",
        // vec(10, 20, 30)[1] → 20. Exercises the shared
        // `intent_vec_i64__from` runtime helper, the Vec
        // struct typedef, `.data` extract, GEP+load, and
        // the `intent_vec_i64__free` drop.
        "fn main() -> i64 { let xs: Vec<i64> = vec(10, 20, 30); return xs[1]; }",
        20,
    ),
];

fn lli_available() -> bool {
    Command::new("lli")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_ll(ll_source: &str, tag: &str) -> i32 {
    use std::io::Write;
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ll_path = dir.join(format!("intent-ssa-llvm-{}-{}-{}.ll", tag, pid, nanos));
    {
        let mut f = std::fs::File::create(&ll_path).expect("create .ll");
        f.write_all(ll_source.as_bytes()).expect("write .ll");
    }
    let run = Command::new("lli")
        .arg(&ll_path)
        .status()
        .unwrap_or_else(|e| {
            panic!("lli failed to start on `{}`: {}", tag, e);
        });
    let _ = std::fs::remove_file(&ll_path);
    run.code().unwrap_or(-1)
}

#[test]
fn ssa_llvm_backend_matches_expected_exit_codes() {
    if !lli_available() {
        eprintln!("warning: lli not on PATH; skipping SSA-LLVM cross-check");
        return;
    }
    for (name, src, expected) in PROGRAMS {
        let checked = compile(src).unwrap_or_else(|errs| {
            panic!("test program `{}` did not type-check: {:?}", name, errs)
        });
        let (module, errs) = lower_program(&checked.ir);
        assert!(
            errs.is_empty(),
            "SSA lower errors for `{}`: {:?}",
            name,
            errs
        );
        let ll = ssa_backend_llvm::emit(&module).unwrap_or_else(|e| {
            panic!("SSA-LLVM emit failed for `{}`: {}", name, e)
        });
        let code = run_ll(&ll, name);
        assert_eq!(
            code, *expected,
            "exit code mismatch on `{}`: got {}, expected {}.\nGenerated LLVM IR:\n{}",
            name, code, expected, ll
        );
    }
}
