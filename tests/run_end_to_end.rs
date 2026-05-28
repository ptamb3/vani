use std::process::Command;

#[test]
fn run_basics_example_succeeds_and_prints_42() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/basics.vani", manifest_dir);

    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run should execute");

    assert!(
        output.status.success(),
        "intentc run failed with status {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("42"),
        "expected bounded_score(20) = 42 in stdout, got: {stdout}"
    );
}

#[test]
fn check_examples_all_succeed() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");

    for example in [
        "basics.vani",
        "integers.vani",
        "floats_and_shifts.vani",
        "arrays.vani",
        "array_return.vani",
        "vectors.vani",
        "borrows.vani",
        "control_flow.vani",
        "drop_interface.vani",
        "memory_safety.vani",
        "dyn_dispatch.vani",
        "early_exit.vani",
        "scopes.vani",
        "modules.vani",
        "mut_refs.vani",
        "verified.vani",
        "for_loops.vani",
        "contracts.vani",
        "invariants.vani",
        "iterate.vani",
        "assert_messages.vani",
        "inline_call_proofs.vani",
        "vec_invariants.vani",
        "bounds_elision.vani",
    ] {
        let path = format!("{}/examples/{}", manifest_dir, example);
        let output = Command::new(binary)
            .args(["check", &path])
            .output()
            .expect("intentc check should execute");
        assert!(
            output.status.success(),
            "check failed for {}: {}",
            example,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn multi_file_diagnostic_points_to_imported_file() {
    use std::fs;
    use std::path::PathBuf;

    let binary = env!("CARGO_BIN_EXE_intentc");
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "intentc-filemap-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("mkdir");

    fs::write(
        dir.join("lib.vani"),
        "fn broken(x: nonsense) -> i64 { return 0; }\n",
    )
    .expect("write lib");
    fs::write(
        dir.join("main.vani"),
        "use \"lib.vani\";\n\nfn main() -> i64 {\n  return 0;\n}\n",
    )
    .expect("write main");

    let output = Command::new(binary)
        .args(["check", dir.join("main.vani").to_str().unwrap()])
        .output()
        .expect("check");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let lib_path = dir
        .join("lib.vani")
        .canonicalize()
        .expect("canonicalize lib")
        .display()
        .to_string();

    // Clean up before asserting so the dir isn't left around on failure.
    let _ = fs::remove_dir_all(&dir);

    assert!(
        !output.status.success(),
        "expected check to fail; stderr was: {stderr}"
    );
    // The diagnostic must be attributed to the imported file's actual path,
    // pinpointing line 1 inside that file.
    assert!(
        stderr.contains(&format!("{}:1:", lib_path)),
        "expected diagnostic at {}:1:..., got:\n{}",
        lib_path,
        stderr
    );
}

#[test]
fn multi_file_compile_resolves_use() {
    use std::fs;
    use std::path::PathBuf;

    let binary = env!("CARGO_BIN_EXE_intentc");
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "intentc-multifile-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("mkdir");

    fs::write(
        dir.join("lib.vani"),
        "fn double(x: i64) -> i64 { return x * 2; }\n",
    )
    .expect("write lib");
    fs::write(
        dir.join("main.vani"),
        r#"use "lib.vani";

fn main() -> i64 {
  let x: i64 = double(21);
  assert x == 42;
  print x;
  return 0;
}
"#,
    )
    .expect("write main");

    let output = Command::new(binary)
        .args(["run", dir.join("main.vani").to_str().unwrap()])
        .output()
        .expect("run multi-file");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let status = output.status;

    // Clean up before asserting so we don't leave the dir on failure.
    let _ = fs::remove_dir_all(&dir);

    assert!(
        status.success(),
        "multi-file run failed: {} (stderr: {})",
        status,
        stderr
    );
    assert!(
        stdout.contains("42"),
        "expected double(21)==42 in stdout, got: {stdout}"
    );
}

// Closure #280: vani.toml manifest auto-discovery. When
// `intentc build|run|check` is invoked without a positional
// source file, the driver walks up from cwd to find a
// `vani.toml`, parses `[package].entry`, and uses that as
// the entry point. Tests the parent-walk + flag-interleaving
// behavior end-to-end.
// Closure #289: `#[bounded(N)]` on the SSA-LLVM path
// (default for `intentc run`). The fn under the bound runs
// normally when depth ≤ N; aborts (SIGABRT, exit 134) when
// depth exceeds N. Verifies the depth-counter
// instrumentation lands correctly in LLVM IR.
#[test]
fn bounded_attribute_aborts_when_depth_exceeded_on_llvm() {
    use std::fs;
    use std::path::PathBuf;

    let binary = env!("CARGO_BIN_EXE_intentc");
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "intentc-bounded-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("mkdir");
    let src = dir.join("bounded.vani");
    fs::write(
        &src,
        "#[bounded(3)]\n\
         fn deep(n: i64) -> i64 {\n  \
           if n <= 0 { return 0; }\n  \
           return deep(n - 1) + 1;\n\
         }\n\
         fn main() -> i64 { return deep(10); }\n",
    )
    .expect("write src");

    let bin_path = dir.join("bounded.bin");
    let build = std::process::Command::new(binary)
        .args([
            "build",
            src.to_str().unwrap(),
            "-o",
            bin_path.to_str().unwrap(),
        ])
        .output()
        .expect("intentc build executes");
    if !build.status.success() {
        let _ = fs::remove_dir_all(&dir);
        panic!(
            "intentc build (bounded LLVM) failed:\nstderr: {}",
            String::from_utf8_lossy(&build.stderr)
        );
    }
    let run = std::process::Command::new(&bin_path)
        .output()
        .expect("binary runs");
    let _ = fs::remove_dir_all(&dir);
    // Aborted process: code() returns None on Unix; check
    // via `signal()` (SIGABRT == 6). On platforms where
    // `code()` returns 134 (shell-style), also accept that.
    let code = run.status.code();
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        run.status.signal()
    };
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    assert!(
        code == Some(134) || signal == Some(6),
        "expected SIGABRT from #[bounded(3)] deep(10), got code={:?} signal={:?}",
        code,
        signal
    );
}

// Closure #287: vani.toml v2 `[deps]` with local-path
// entries pulls the dep's entry source into the main
// program's build. Validates the local-path resolution end-
// to-end: a `mathlib` package with a `triple` fn is
// declared as a dep of `main_app`, which calls `triple(7)`.
#[test]
fn manifest_deps_local_path_brings_lib_into_scope() {
    use std::fs;
    use std::path::PathBuf;

    let binary = env!("CARGO_BIN_EXE_intentc");
    let workspace: PathBuf = std::env::temp_dir().join(format!(
        "intentc-deps-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let lib_dir = workspace.join("lib");
    let main_dir = workspace.join("main");
    fs::create_dir_all(lib_dir.join("src")).expect("mkdir lib/src");
    fs::create_dir_all(main_dir.join("src")).expect("mkdir main/src");

    fs::write(
        lib_dir.join("vani.toml"),
        "[package]\nname = \"mathlib\"\nentry = \"src/mathlib.vani\"\n",
    )
    .expect("write lib manifest");
    fs::write(
        lib_dir.join("src/mathlib.vani"),
        "fn triple(x: i64) -> i64 { return x * 3; }\n",
    )
    .expect("write lib source");
    fs::write(
        main_dir.join("vani.toml"),
        "[package]\nname = \"main_app\"\nentry = \"src/main.vani\"\n\n\
         [deps]\nmathlib = { path = \"../lib\" }\n",
    )
    .expect("write main manifest");
    fs::write(
        main_dir.join("src/main.vani"),
        "fn main() -> i64 { return triple(7); }\n",
    )
    .expect("write main source");

    let output = std::process::Command::new(binary)
        .args(["run"])
        .current_dir(&main_dir)
        .output()
        .expect("intentc run executes");

    let status = output.status;
    let _ = fs::remove_dir_all(&workspace);

    assert_eq!(
        status.code(),
        Some(21),
        "expected triple(7)=21, got status {} (stderr: {})",
        status,
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn manifest_discovery_resolves_entry_from_subdir() {
    use std::fs;
    use std::path::PathBuf;

    let binary = env!("CARGO_BIN_EXE_intentc");
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "intentc-manifest-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let src_dir = dir.join("src");
    let sub_dir = dir.join("nested/deep");
    fs::create_dir_all(&src_dir).expect("mkdir src");
    fs::create_dir_all(&sub_dir).expect("mkdir nested/deep");

    fs::write(
        dir.join("vani.toml"),
        "[package]\nname = \"manifest_test\"\nentry = \"src/main.vani\"\n",
    )
    .expect("write manifest");
    fs::write(
        src_dir.join("main.vani"),
        "fn main() -> i64 { write \"from manifest\"; return 42; }\n",
    )
    .expect("write entry");

    // Invoke `intentc run` from the deep subdir with no
    // positional arg. The driver must walk up to find the
    // manifest and use its entry.
    let output = std::process::Command::new(binary)
        .args(["run"])
        .current_dir(&sub_dir)
        .output()
        .expect("intentc run executes");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let status = output.status;

    let _ = fs::remove_dir_all(&dir);

    assert!(
        status.success() || status.code() == Some(42),
        "intentc run via manifest failed: {} (stdout: {}, stderr: {})",
        status,
        stdout,
        stderr,
    );
    assert!(
        stdout.contains("from manifest"),
        "expected `from manifest` in stdout, got: {stdout}"
    );
}

#[test]
fn manifest_build_with_o_flag_finds_entry() {
    use std::fs;
    use std::path::PathBuf;

    let binary = env!("CARGO_BIN_EXE_intentc");
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "intentc-manifest-build-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let src_dir = dir.join("src");
    fs::create_dir_all(&src_dir).expect("mkdir src");
    fs::write(
        dir.join("vani.toml"),
        "[package]\nname = \"build_test\"\nentry = \"src/main.vani\"\n",
    )
    .expect("write manifest");
    fs::write(
        src_dir.join("main.vani"),
        "fn main() -> i64 { return 17; }\n",
    )
    .expect("write entry");

    let bin_path = dir.join("out_binary");
    let build = std::process::Command::new(binary)
        .args(["build", "-o", bin_path.to_str().unwrap()])
        .current_dir(&dir)
        .output()
        .expect("intentc build executes");

    if !build.status.success() {
        let _ = fs::remove_dir_all(&dir);
        panic!(
            "intentc build via manifest + -o failed:\nstderr: {}",
            String::from_utf8_lossy(&build.stderr)
        );
    }

    let run = std::process::Command::new(&bin_path)
        .output()
        .expect("binary runs");
    let exit = run.status.code().unwrap_or(-1);

    let _ = fs::remove_dir_all(&dir);

    assert_eq!(exit, 17, "expected exit 17 from manifest-built binary");
}

// FFI v4 follow-up: `intentc run --backend=c --link-with foo.c`
// threads the same linker flags as `build` so rapid iteration
// can call user-provided extern bodies without a separate
// build step. LLVM-JIT remains host-symbol-only because lli
// can't link static translation units.
#[test]
fn run_link_with_resolves_extern_c_symbol_in_run_mode() {
    use std::fs;
    use std::path::PathBuf;

    let binary = env!("CARGO_BIN_EXE_intentc");
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "intentc-runlinkwith-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("mkdir");

    let helper_c = dir.join("helper.c");
    fs::write(
        &helper_c,
        "#include <stdint.h>\nint32_t triple(int32_t x) { return x * 3; }\n",
    )
    .expect("write helper.c");

    let vani_src = dir.join("prog.vani");
    fs::write(
        &vani_src,
        "extern \"C\" fn triple(x: i32) -> i32;\n\
         \n\
         fn main() -> i64 {\n  \
           let r: i32 = triple(7 as i32);\n  \
           write \"triple(7) =\", r;\n  \
           return 0;\n}\n",
    )
    .expect("write prog.vani");

    let run = Command::new(binary)
        .args([
            "run",
            vani_src.to_str().unwrap(),
            "--backend=c",
            "--link-with",
            helper_c.to_str().unwrap(),
        ])
        .output()
        .expect("intentc run --link-with runs");

    let stdout = String::from_utf8_lossy(&run.stdout).to_string();
    let stderr = String::from_utf8_lossy(&run.stderr).to_string();
    let status = run.status;

    let _ = fs::remove_dir_all(&dir);

    assert!(
        status.success(),
        "intentc run --backend=c --link-with failed: {} (stdout: {}, stderr: {})",
        status,
        stdout,
        stderr,
    );
    assert!(
        stdout.contains("triple(7) = 21"),
        "expected `triple(7) = 21` in stdout, got: {stdout}"
    );
}

#[test]
fn run_link_with_requires_backend_c() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/basics.vani", manifest_dir);

    // Default backend is LLVM; --link-with should be rejected.
    let out = Command::new(binary)
        .args(["run", &example, "--link-with", "/tmp/whatever.c"])
        .output()
        .expect("intentc run executes");

    assert!(
        !out.status.success(),
        "expected failure when --link-with is paired with LLVM-JIT"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("require --backend=c"),
        "expected backend=c hint in stderr, got: {stderr}"
    );
}

// FFI v2: `intentc build --link-with foo.c` threads an extra
// translation unit into the link line so an `extern "C" fn`
// declaration in vāṇī source resolves at link time. End-to-end
// shape: a tiny C helper `triple(x: i32) -> i32`, a vāṇी source
// that declares + calls it, build with --link-with, run, expect
// `triple(7) = 21` on stdout.
#[test]
fn build_link_with_resolves_extern_c_symbol() {
    use std::fs;
    use std::path::PathBuf;

    let binary = env!("CARGO_BIN_EXE_intentc");
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "intentc-linkwith-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("mkdir");

    let helper_c = dir.join("helper.c");
    fs::write(
        &helper_c,
        "#include <stdint.h>\nint32_t triple(int32_t x) { return x * 3; }\n",
    )
    .expect("write helper.c");

    let vani_src = dir.join("prog.vani");
    fs::write(
        &vani_src,
        "extern \"C\" fn triple(x: i32) -> i32;\n\
         \n\
         fn main() -> i64 {\n  \
           let r: i32 = triple(7 as i32);\n  \
           write \"triple(7) =\", r;\n  \
           return 0;\n}\n",
    )
    .expect("write prog.vani");

    let bin_path = dir.join("prog");
    let build = Command::new(binary)
        .args([
            "build",
            vani_src.to_str().unwrap(),
            "--link-with",
            helper_c.to_str().unwrap(),
            "-o",
            bin_path.to_str().unwrap(),
        ])
        .output()
        .expect("intentc build runs");

    if !build.status.success() {
        let _ = fs::remove_dir_all(&dir);
        panic!(
            "intentc build --link-with failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr),
        );
    }

    let run = Command::new(&bin_path).output().expect("binary runs");
    let stdout = String::from_utf8_lossy(&run.stdout).to_string();
    let status = run.status;

    let _ = fs::remove_dir_all(&dir);

    assert!(
        status.success(),
        "linked binary exited non-zero: {} (stdout: {})",
        status,
        stdout
    );
    assert!(
        stdout.contains("triple(7) = 21"),
        "expected `triple(7) = 21` in stdout, got: {stdout}"
    );
}

#[test]
fn run_assert_messages_example() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/assert_messages.vani", manifest_dir);

    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run should execute");

    assert!(
        output.status.success(),
        "intentc run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("30"), "expected lookup(&xs, 2)==30, got: {stdout}");
}

#[test]
fn intentc_ir_dumps_typed_program() {
    use std::fs;
    let binary = env!("CARGO_BIN_EXE_intentc");
    let dir = std::env::temp_dir().join(format!(
        "intentc-ir-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("mkdir");
    let src = dir.join("i.vani");
    fs::write(&src, "fn main() -> i64 { return 7; }\n").expect("write");

    let output = Command::new(binary)
        .args(["ir", src.to_str().unwrap()])
        .output()
        .expect("intentc ir");
    let _ = fs::remove_dir_all(&dir);

    assert!(output.status.success(), "ir exited non-zero");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The typed-IR dump must show the cached-return temp + the
    // literal value. Confirms the checker's Drop-before-Return
    // soundness fix is in the IR the backends see.
    assert!(stdout.contains("TypedProgram {"));
    assert!(stdout.contains("__intent_ret_"));
    // `{:#?}` splits enum payloads across lines, so the literal
    // appears as `Int(\n  7,\n)`. Use a regex-free shape check.
    assert!(stdout.contains("Int("));
    assert!(stdout.contains("7,"));
    assert!(stdout.contains("Return {"));
}

#[test]
fn intentc_ast_dumps_parsed_program() {
    use std::fs;
    let binary = env!("CARGO_BIN_EXE_intentc");
    let dir = std::env::temp_dir().join(format!(
        "intentc-ast-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("mkdir");
    let src = dir.join("a.vani");
    fs::write(&src, "fn add(a: i64, b: i64) -> i64 { return a + b; }\n").expect("write");

    let output = Command::new(binary)
        .args(["ast", src.to_str().unwrap()])
        .output()
        .expect("intentc ast");
    let _ = fs::remove_dir_all(&dir);

    assert!(output.status.success(), "ast exited non-zero");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Spot-check a few expected substrings of the debug-format AST.
    assert!(stdout.contains("Program {"));
    assert!(stdout.contains("name: \"add\""));
    assert!(stdout.contains("return_type: I64"));
    assert!(stdout.contains("Return {"));
}

#[test]
fn intentc_tokens_dumps_token_stream() {
    use std::fs;
    let binary = env!("CARGO_BIN_EXE_intentc");
    let dir = std::env::temp_dir().join(format!(
        "intentc-tokens-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("mkdir");
    let src = dir.join("t.vani");
    fs::write(&src, "fn main() -> i64 { return 42; }\n").expect("write");

    let output = Command::new(binary)
        .args(["tokens", src.to_str().unwrap()])
        .output()
        .expect("intentc tokens");
    let _ = fs::remove_dir_all(&dir);

    assert!(output.status.success(), "tokens exited non-zero");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Spot-check a few expected tokens for a tiny program.
    assert!(stdout.contains("Fn"));
    assert!(stdout.contains("Ident(\"main\")"));
    assert!(stdout.contains("Int(42)"));
    assert!(stdout.contains("Return"));
}

#[test]
fn intentc_build_produces_runnable_native_binary() {
    // Gated on `llc` + `cc` being present. `cc` is on every dev box
    // we'd care about; `llc` ships with LLVM's `lli`.
    let llc_ok = std::process::Command::new("llc")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !llc_ok {
        return;
    }

    use std::fs;
    let binary = env!("CARGO_BIN_EXE_intentc");
    let dir = std::env::temp_dir().join(format!(
        "intentc-build-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("mkdir");
    let src = dir.join("prog.vani");
    fs::write(
        &src,
        "fn main() -> i64 {\n  let x: i64 = 7;\n  let y: i64 = 6;\n  print x * y;\n  return 0;\n}\n",
    )
    .expect("write src");
    let out_bin = dir.join("prog");

    let build_out = Command::new(binary)
        .args([
            "build",
            src.to_str().unwrap(),
            "-o",
            out_bin.to_str().unwrap(),
        ])
        .output()
        .expect("intentc build");
    assert!(
        build_out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build_out.stderr)
    );
    assert!(out_bin.exists(), "build did not produce a binary");

    // Run the binary.
    let run_out = Command::new(&out_bin).output().expect("run binary");
    let _ = fs::remove_dir_all(&dir);
    assert!(run_out.status.success(), "binary exited non-zero");
    let stdout = String::from_utf8_lossy(&run_out.stdout);
    assert!(stdout.contains("42"), "expected 42, got: {stdout}");
}

#[test]
fn llvm_backend_run_produces_same_output_as_c() {
    // Gated on `lli` being installed; mirrors the per-backend test
    // pattern in src/backend_llvm.rs.
    // Look up `lli` via $LLI / PATH rather than hardcoding /usr/bin
    // so the test works on systems with lli elsewhere (homebrew,
    // /usr/local, etc.).
    let lli = std::env::var("LLI").unwrap_or_else(|_| "lli".to_string());
    let lli_ok = std::process::Command::new(&lli)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !lli_ok {
        return;
    }

    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");

    // Every example under examples/ — the LLVM and C backends must
    // produce identical stdout AND identical exit codes for each.
    // Catches semantic divergence between the two anywhere in the
    // matrix of feature interactions. Update this list when a new
    // example file lands.
    for name in &[
        "array_proofs.vani",
        "array_return.vani",
        "arrays.vani",
        "assert_messages.vani",
        "atomics.vani",
        "basics.vani",
        "block_expressions.vani",
        "borrows.vani",
        "bounded_generics.vani",
        "bounds_elision.vani",
        "composite_types.vani",
        "concurrency.vani",
        "condvar.vani",
        "contracts.vani",
        "control_flow.vani",
        "drop_interface.vani",
        "memory_safety.vani",
        "dyn_dispatch.vani",
        "early_exit.vani",
        "enum_arr_payload.vani",
        "enum_eq.vani",
        "enum_owned_payload.vani",
        "enum_vec_payload.vani",
        "floats_and_shifts.vani",
        "fn_pointers.vani",
        "for_loops.vani",
        "generic_functions.vani",
        "hindi_keywords.vani",
        "inline_call_proofs.vani",
        "integers.vani",
        "interfaces.vani",
        "invariants.vani",
        "iterate.vani",
        "marathi_keywords.vani",
        "match_bool.vani",
        "match_str.vani",
        "methods.vani",
        "mixed_place_assign.vani",
        "modules.vani",
        "mut_refs.vani",
        "nested_struct_drop.vani",
        "option_error_propagation.vani",
        "option_types.vani",
        "parallel.vani",
        "partial_move.vani",
        "push_mut.vani",
        "sanskrit_keywords.vani",
        "scopes.vani",
        "sort.vani",
        "string_ops.vani",
        "strings.vani",
        "strings_concat.vani",
        "struct_atomic_field.vani",
        "struct_eq.vani",
        "struct_mixed_fields.vani",
        "struct_owned_field.vani",
        "tasks.vani",
        "tracker.vani",
        "try_keyword.vani",
        "tuple_eq.vani",
        "type_associated_fn.vani",
        "unit_return.vani",
        "vec_invariants.vani",
        "vectors.vani",
        "verified.vani",
    ] {
        let example = format!("{}/examples/{}", manifest_dir, name);

        let c_out = Command::new(binary)
            .args(["run", &example, "--backend=c"])
            .output()
            .expect("c run");
        let llvm_out = Command::new(binary)
            .args(["run", &example, "--backend=llvm"])
            .output()
            .expect("llvm run");

        assert!(
            c_out.status.success(),
            "C backend failed for {name}: {}",
            String::from_utf8_lossy(&c_out.stderr)
        );
        assert!(
            llvm_out.status.success(),
            "LLVM backend failed for {name}: {}",
            String::from_utf8_lossy(&llvm_out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&c_out.stdout),
            String::from_utf8_lossy(&llvm_out.stdout),
            "stdout diverges between C and LLVM for {name}"
        );
        assert_eq!(
            c_out.status.code(),
            llvm_out.status.code(),
            "exit codes diverge between C and LLVM for {name}"
        );
    }
}

#[test]
fn run_inline_call_proofs_example() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/inline_call_proofs.vani", manifest_dir);

    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run should execute");

    assert!(
        output.status.success(),
        "intentc run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("6"), "expected caller(5) = inc(5) = 6, got: {stdout}");
}

#[test]
fn run_bounds_elision_example_and_verify_no_runtime_guard() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/bounds_elision.vani", manifest_dir);

    // First, prove the program runs and prints the expected outputs.
    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run should execute");
    assert!(
        output.status.success(),
        "intentc run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for v in ["10", "30", "50", "150"] {
        assert!(stdout.contains(v), "expected {v} in stdout, got: {stdout}");
    }

    // Second, inspect the emitted C and confirm each bounds-elidable
    // function body has no intent_check_bounds call.
    let emit = Command::new(binary)
        .args(["emit-c", &example])
        .output()
        .expect("intentc emit-c should execute");
    assert!(emit.status.success(), "emit-c failed");
    let c = String::from_utf8_lossy(&emit.stdout);
    for fname in ["fn_first", "fn_at", "fn_last", "fn_sum"] {
        // Find the function *definition* (skipping the forward decl
        // by matching the open-brace tail).
        let pat = format!("{}(", fname);
        let mut search = c.as_ref();
        let mut found_def = false;
        while let Some(idx) = search.find(&pat) {
            let after = &search[idx..];
            // Definition has `{` on the same line as the closing paren;
            // the forward decl has `;`.
            let line_end = after.find('\n').unwrap_or(after.len());
            let line = &after[..line_end];
            if line.contains(") {") {
                let body_end = after.find("\n}\n").map(|i| i + 1).unwrap_or(after.len());
                let body = &after[..body_end];
                assert!(
                    !body.contains("intent_check_bounds"),
                    "expected no bounds-check call in {}: {}",
                    fname,
                    body
                );
                found_def = true;
                break;
            }
            search = &after[1..];
        }
        assert!(found_def, "could not find definition of {fname}");
    }

    // Third, do the same shape check on the LLVM backend. The
    // marker for an elided bounds check in LLVM is the absence of
    // an inline `call void @abort()` in the function body (apart
    // from the one each requires clause emits). `fn_sum` has no
    // requires, so its body must contain *zero* `@abort` calls.
    let llvm_emit = Command::new(binary)
        .args(["emit", &example])
        .output()
        .expect("intentc emit (llvm) should execute");
    assert!(llvm_emit.status.success(), "emit --backend=llvm failed");
    let ll = String::from_utf8_lossy(&llvm_emit.stdout);
    let sum_start = ll
        .find("define i64 @fn_sum(")
        .expect("expected fn_sum in LLVM IR");
    let sum_body = &ll[sum_start..];
    let sum_end = sum_body.find("\n}\n").map(|i| i + 1).unwrap_or(sum_body.len());
    let sum_body = &sum_body[..sum_end];
    assert!(
        !sum_body.contains("call void @abort()"),
        "expected no abort/guard in fn_sum LLVM body, got:\n{sum_body}"
    );
}

#[test]
fn run_vec_invariants_example() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/vec_invariants.vani", manifest_dir);

    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run should execute");

    assert!(
        output.status.success(),
        "intentc run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The loop pushes 0, 10, 20, 30, 40; verify all five appear on stdout.
    let stdout = String::from_utf8_lossy(&output.stdout);
    for v in ["0", "10", "20", "30", "40"] {
        assert!(stdout.contains(v), "expected {v} in stdout, got: {stdout}");
    }
}

#[test]
fn json_check_outputs_empty_diagnostics_on_success() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/basics.vani", manifest_dir);

    let output = Command::new(binary)
        .args(["check", &example, "--json"])
        .output()
        .expect("intentc check --json should execute");

    assert!(output.status.success(), "expected exit 0 on success");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim_end();
    assert_eq!(
        trimmed, "{\"diagnostics\":[]}",
        "expected canonical empty-success JSON, got: {stdout}"
    );
}

#[test]
fn json_check_outputs_structured_diagnostics_on_failure() {
    use std::fs;

    let binary = env!("CARGO_BIN_EXE_intentc");
    let dir = std::env::temp_dir().join(format!(
        "intentc-json-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("mkdir");
    let src = dir.join("bad.vani");
    fs::write(
        &src,
        "fn main() -> i64 {\n  return undefined_name;\n}\n",
    )
    .expect("write src");

    let output = Command::new(binary)
        .args(["check", src.to_str().unwrap(), "--json"])
        .output()
        .expect("intentc check --json");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let status = output.status;
    let _ = fs::remove_dir_all(&dir);

    assert!(!status.success(), "expected non-zero exit on failure");
    assert!(
        stdout.contains("\"diagnostics\":[")
            && stdout.contains("\"level\":\"error\"")
            && stdout.contains("undefined_name"),
        "expected structured JSON with the undefined-name error, got: {stdout}"
    );
}

#[test]
fn assert_with_message_emits_custom_runtime_diagnostic() {
    use std::fs;

    let binary = env!("CARGO_BIN_EXE_intentc");
    let dir = std::env::temp_dir().join(format!(
        "intentc-assert-msg-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("mkdir");
    let src = dir.join("bad.vani");
    fs::write(
        &src,
        "fn main() -> i64 {\n  let x: i64 = 0;\n  assert x == 1, \"x should be exactly one\";\n  return 0;\n}\n",
    )
    .expect("write src");

    let output = Command::new(binary)
        .args(["run", src.to_str().unwrap()])
        .output()
        .expect("intentc run");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let status = output.status;

    let _ = fs::remove_dir_all(&dir);

    assert!(
        !status.success(),
        "expected failure exit; stderr was: {stderr}"
    );
    assert!(
        stderr.contains("assertion failed: x should be exactly one"),
        "expected custom message on stderr, got: {stderr}"
    );
}

#[test]
fn run_iterate_example() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/iterate.vani", manifest_dir);

    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run should execute");

    assert!(
        output.status.success(),
        "intentc run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("15"), "expected total==15: {stdout}");
    assert!(stdout.contains("9"), "expected max==9: {stdout}");
    assert!(stdout.contains("3"), "expected positives==3: {stdout}");
}

#[test]
fn run_invariants_example() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/invariants.vani", manifest_dir);

    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run should execute");

    assert!(
        output.status.success(),
        "intentc run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("5"), "expected count_to(5)==5, got: {stdout}");
    assert!(stdout.contains("1"), "expected min==1, got: {stdout}");
}

#[test]
fn run_contracts_example() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/contracts.vani", manifest_dir);

    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run should execute");

    assert!(
        output.status.success(),
        "intentc run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("13"), "expected diff==13, got: {stdout}");
    assert!(stdout.contains("10"), "expected bigger==10, got: {stdout}");
}

#[test]
fn run_for_loops_example() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/for_loops.vani", manifest_dir);

    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run should execute");

    assert!(
        output.status.success(),
        "intentc run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("30"), "expected sum_squares==30, got: {stdout}");
    assert!(stdout.contains("2"), "expected first-zero==2, got: {stdout}");
}

#[test]
fn run_verified_example() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/verified.vani", manifest_dir);

    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run should execute");

    assert!(
        output.status.success(),
        "intentc run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("7"), "expected safe_subtract == 7, got: {stdout}");
}

#[test]
fn run_mut_refs_example() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/mut_refs.vani", manifest_dir);

    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run should execute");

    assert!(
        output.status.success(),
        "intentc run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("8"), "expected doubled-last 8, got: {stdout}");
    assert!(stdout.contains("9"), "expected fill value 9, got: {stdout}");
}

#[test]
fn run_scopes_example() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/scopes.vani", manifest_dir);

    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run should execute");

    assert!(
        output.status.success(),
        "intentc run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("3"), "expected counter == 3, got: {stdout}");
}

#[test]
fn run_early_exit_example() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/early_exit.vani", manifest_dir);

    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run should execute");

    assert!(
        output.status.success(),
        "intentc run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("-3"), "expected -3 in stdout, got: {stdout}");
    assert!(stdout.contains("3"), "expected positives count 3, got: {stdout}");
}

#[test]
fn run_control_flow_example() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/control_flow.vani", manifest_dir);

    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run should execute");

    assert!(
        output.status.success(),
        "intentc run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("10"), "expected total == 10, got: {stdout}");
}

#[test]
fn run_borrows_example_prints_sum() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/borrows.vani", manifest_dir);

    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run should execute");

    assert!(
        output.status.success(),
        "intentc run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("10"), "expected sum == 10, got: {stdout}");
}

#[test]
fn run_vectors_example_prints_first_element() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/vectors.vani", manifest_dir);

    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run should execute");

    assert!(
        output.status.success(),
        "intentc run failed with status {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("99"),
        "expected first == 99 in stdout, got: {stdout}"
    );
}

#[test]
fn run_arrays_example_prints_sum() {
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/arrays.vani", manifest_dir);

    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run should execute");

    assert!(
        output.status.success(),
        "intentc run failed with status {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("10"),
        "expected sum_four([1,2,3,4]) = 10 in stdout, got: {stdout}"
    );
}

#[test]
fn intentc_test_expands_directory_arg_to_intent_files() {
    // `intentc test examples/` should walk the directory and run
    // every `*.vani` inside. Same result as listing them out
    // explicitly, but the dir form is the user-friendly path.
    let lli = std::env::var("LLI").unwrap_or_else(|_| "lli".to_string());
    let lli_ok = Command::new(&lli)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !lli_ok {
        return;
    }

    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let examples_dir = format!("{}/examples", manifest_dir);

    let n_examples = std::fs::read_dir(&examples_dir)
        .expect("examples dir readable")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("vani"))
        .count();

    let run = Command::new(binary)
        .args(["test", &examples_dir])
        .output()
        .expect("intentc test <dir>");
    assert!(
        run.status.success(),
        "intentc test <dir> should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    let expected = format!("{} passed; 0 failed", n_examples);
    assert!(
        stdout.contains(&expected),
        "expected `{expected}` in summary, got:\n{stdout}"
    );
}

#[test]
fn intentc_test_trims_lli_backtrace_from_failed_stderr() {
    // When a test program aborts (failed assert), lli prints a long
    // signal-handler backtrace that's not useful to Intent users.
    // Confirm the captured stderr was truncated to the meaningful
    // line ("assertion failed: ...") and the lli boilerplate is gone.
    let lli = std::env::var("LLI").unwrap_or_else(|_| "lli".to_string());
    let lli_ok = Command::new(&lli)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !lli_ok {
        return;
    }

    let binary = env!("CARGO_BIN_EXE_intentc");
    let tmp_fail = std::env::temp_dir().join(format!(
        "intentc_trim_lli_{}.vani",
        std::process::id()
    ));
    std::fs::write(
        &tmp_fail,
        b"fn main() -> i64 {\n  assert 1 == 2, \"deliberate failure\";\n  return 0;\n}\n",
    )
    .expect("write tmp");

    let run = Command::new(binary)
        .args(["test", tmp_fail.to_str().unwrap()])
        .output()
        .expect("intentc test");
    let _ = std::fs::remove_file(&tmp_fail);
    assert_eq!(run.status.code(), Some(1));

    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("assertion failed: deliberate failure"),
        "expected the meaningful failure line, got:\n{stderr}"
    );
    assert!(
        !stderr.contains("PLEASE submit a bug report") && !stderr.contains("Stack dump:"),
        "lli backtrace should have been trimmed, got:\n{stderr}"
    );
}

#[test]
fn intentc_test_passes_for_all_examples_and_fails_on_violated_assertion() {
    // Two-part check:
    //  (a) `intentc test` over every example produces all-passes and
    //      exit 0 — same coverage as the existing per-example tests
    //      but driving the new subcommand end-to-end.
    //  (b) Adding one program that fails an assertion flips the
    //      summary to `1 failed` and the exit code to 1.

    let lli = std::env::var("LLI").unwrap_or_else(|_| "lli".to_string());
    let lli_ok = Command::new(&lli)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !lli_ok {
        return;
    }

    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let examples_dir = format!("{}/examples", manifest_dir);

    let mut paths: Vec<String> = std::fs::read_dir(&examples_dir)
        .expect("examples dir readable")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("vani"))
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no examples discovered");

    let mut args = vec!["test".to_string()];
    args.extend(paths.iter().cloned());

    let ok_run = Command::new(binary)
        .args(&args)
        .output()
        .expect("intentc test");
    assert!(
        ok_run.status.success(),
        "intentc test should pass for all examples\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&ok_run.stdout),
        String::from_utf8_lossy(&ok_run.stderr),
    );
    let ok_stdout = String::from_utf8_lossy(&ok_run.stdout);
    assert!(
        ok_stdout.contains("0 failed"),
        "expected `0 failed` in summary, got:\n{ok_stdout}"
    );

    let tmp_fail = std::env::temp_dir().join(format!(
        "intentc_test_fail_{}.vani",
        std::process::id()
    ));
    std::fs::write(
        &tmp_fail,
        b"fn main() -> i64 {\n  let x: i64 = 0;\n  assert x == 1, \"x should be one\";\n  return 0;\n}\n",
    )
    .expect("write tmp fail");

    let fail_run = Command::new(binary)
        .args(["test", tmp_fail.to_str().unwrap()])
        .output()
        .expect("intentc test fail");
    let _ = std::fs::remove_file(&tmp_fail);
    assert_eq!(
        fail_run.status.code(),
        Some(1),
        "intentc test should exit 1 on assertion failure"
    );
    let fail_stdout = String::from_utf8_lossy(&fail_run.stdout);
    assert!(
        fail_stdout.contains("FAILED") && fail_stdout.contains("1 failed"),
        "expected FAILED + `1 failed` in summary, got:\n{fail_stdout}"
    );
}

#[test]
fn expand_dir_walks_recursively_and_skips_dot_dirs() {
    // Confirms the shared dir-expansion helper used by both
    // `intentc test` and `intentc fmt`:
    //  - descends into subdirectories;
    //  - skips dot-prefixed directories (`.git`, `.cargo`, etc.).
    // Tests the behavior via `intentc test`, which exercises the
    // helper end-to-end and reports the file list in its summary.
    use std::fs;

    let lli = std::env::var("LLI").unwrap_or_else(|_| "lli".to_string());
    let lli_ok = Command::new(&lli)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !lli_ok {
        return;
    }

    let binary = env!("CARGO_BIN_EXE_intentc");
    let root = std::env::temp_dir().join(format!(
        "intentc_nested_walk_{}",
        std::process::id()
    ));
    fs::create_dir_all(root.join("sub/deep")).expect("mkdir sub/deep");
    fs::create_dir_all(root.join(".hidden")).expect("mkdir hidden");

    let trivial = "fn main() -> i64 { return 0; }\n";
    fs::write(root.join("a.vani"), trivial).expect("write a");
    fs::write(root.join("sub/b.vani"), trivial).expect("write b");
    fs::write(root.join("sub/deep/c.vani"), trivial).expect("write c");
    fs::write(root.join(".hidden/skipme.vani"), trivial).expect("write skip");

    let run = Command::new(binary)
        .args(["test", root.to_str().unwrap()])
        .output()
        .expect("intentc test <dir>");
    assert!(
        run.status.success(),
        "intentc test failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains("3 passed; 0 failed"),
        "expected 3 files passed (a, b, c), got:\n{stdout}"
    );
    assert!(
        !stdout.contains("skipme.vani"),
        "files under .hidden/ should be skipped, got:\n{stdout}"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn intentc_test_json_emits_machine_readable_results() {
    // `intentc test --json a.vani b.vani` should print one
    // object on stdout: `{"results":[…],"summary":{…}}`. Each
    // result has `path`, `ok`, `ms` and (for failures) `exit` +
    // `reason`. Pin the basic shape; substring checks suffice.
    let lli = std::env::var("LLI").unwrap_or_else(|_| "lli".to_string());
    let lli_ok = Command::new(&lli)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !lli_ok {
        return;
    }

    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let a = format!("{}/examples/basics.vani", manifest_dir);

    // Make a small failing fixture so the JSON shows a runtime
    // failure too.
    let fail_path = std::env::temp_dir().join(format!(
        "intentc_test_json_fail_{}.vani",
        std::process::id()
    ));
    std::fs::write(
        &fail_path,
        b"fn main() -> i64 {\n  assert 1 == 2;\n  return 0;\n}\n",
    )
    .expect("write fail fixture");

    let run = Command::new(binary)
        .args(["test", "--json", &a, fail_path.to_str().unwrap()])
        .output()
        .expect("intentc test --json");
    let _ = std::fs::remove_file(&fail_path);

    assert_eq!(run.status.code(), Some(1), "should exit 1 on any failure");
    let stdout = String::from_utf8_lossy(&run.stdout);

    // Single-line JSON object. Each path appears once.
    assert!(
        stdout.contains("\"results\":[") && stdout.contains("\"summary\":{"),
        "missing top-level keys, got:\n{stdout}"
    );
    assert!(
        stdout.contains("\"path\":\"") && stdout.contains("basics.vani"),
        "missing path entry, got:\n{stdout}"
    );
    assert!(
        stdout.contains("\"ok\":true") && stdout.contains("\"ok\":false"),
        "expected both ok=true and ok=false, got:\n{stdout}"
    );
    assert!(
        stdout.contains("\"reason\":\"runtime\""),
        "failing fixture should be tagged runtime, got:\n{stdout}"
    );
    assert!(
        stdout.contains("\"passed\":1") && stdout.contains("\"failed\":1"),
        "summary counts off, got:\n{stdout}"
    );
}

#[test]
fn intentc_check_smt_debug_flag_dumps_smt_query() {
    // `--smt-debug` should surface the same query/response stream
    // as `INTENTC_SMT_DEBUG=1`: each SMT round-trip emits a
    // `--- SMT query ---` block to stderr. Use a small file with
    // a `prove` so we know there's at least one query.
    let binary = env!("CARGO_BIN_EXE_intentc");
    let tmp = std::env::temp_dir().join(format!(
        "intentc_smt_debug_{}.vani",
        std::process::id()
    ));
    // The prove must NOT constant-fold — otherwise the verifier
    // short-circuits and never makes an SMT call. A parameter-
    // dependent inequality ensures z3 is consulted.
    std::fs::write(
        &tmp,
        b"fn f(a: i64) -> i64\nrequires a >= 0;\nrequires a < 1000;\n{\n  prove a + 1 > 0;\n  return a + 1;\n}\nfn main() -> i64 { return 0; }\n",
    )
    .expect("write tmp");

    // Without the flag: stderr should not include the SMT query header.
    let plain = Command::new(binary)
        .args(["check", tmp.to_str().unwrap()])
        .env_remove("INTENTC_SMT_DEBUG")
        .output()
        .expect("intentc check");
    assert!(plain.status.success());
    let plain_stderr = String::from_utf8_lossy(&plain.stderr);
    assert!(
        !plain_stderr.contains("--- SMT query ---"),
        "default run shouldn't dump SMT queries, stderr was:\n{plain_stderr}"
    );

    // With the flag: stderr should include at least one query block.
    let debug = Command::new(binary)
        .args(["check", "--smt-debug", tmp.to_str().unwrap()])
        .env_remove("INTENTC_SMT_DEBUG")
        .output()
        .expect("intentc check --smt-debug");
    let _ = std::fs::remove_file(&tmp);

    assert!(debug.status.success());
    let debug_stderr = String::from_utf8_lossy(&debug.stderr);
    assert!(
        debug_stderr.contains("--- SMT query ---"),
        "--smt-debug should dump at least one query block, stderr was:\n{debug_stderr}"
    );
}

#[test]
fn intentc_check_accepts_directory_and_summarizes() {
    // `intentc check examples/` should walk the directory and
    // type-check every `*.vani` inside, printing per-file `ok`
    // lines plus a summary, and exit 0.
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let examples_dir = format!("{}/examples", manifest_dir);

    let n_examples = std::fs::read_dir(&examples_dir)
        .expect("examples dir readable")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("vani"))
        .count();

    let run = Command::new(binary)
        .args(["check", &examples_dir])
        .output()
        .expect("intentc check <dir>");
    assert!(
        run.status.success(),
        "intentc check <dir> should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    let expected = format!("ok: {} file(s)", n_examples);
    assert!(
        stdout.contains(&expected),
        "expected `{expected}` summary, got:\n{stdout}"
    );
}

#[test]
fn intentc_check_json_combines_diagnostics_across_files() {
    // `intentc check --json a.vani b.vani` now emits a single
    // `{"diagnostics":[...]}` object covering both files. The
    // `FileMap::extend_with` helper shifts each file's span frame
    // into a global one so each diagnostic still resolves to its
    // own source path/line.
    let binary = env!("CARGO_BIN_EXE_intentc");
    let tmp_a = std::env::temp_dir().join(format!("check_json_a_{}.vani", std::process::id()));
    let tmp_b = std::env::temp_dir().join(format!("check_json_b_{}.vani", std::process::id()));
    std::fs::write(&tmp_a, b"fn main() -> i64 {\n  let x: i64 = nope;\n  return 0;\n}\n").unwrap();
    std::fs::write(&tmp_b, b"fn f() -> i64 {\n  return undefined;\n}\n").unwrap();

    let run = Command::new(binary)
        .args([
            "check",
            "--json",
            tmp_a.to_str().unwrap(),
            tmp_b.to_str().unwrap(),
        ])
        .output()
        .expect("intentc check --json multi-file");
    let _ = std::fs::remove_file(&tmp_a);
    let _ = std::fs::remove_file(&tmp_b);

    assert_eq!(run.status.code(), Some(1), "should exit 1 on errors");
    let stdout = String::from_utf8_lossy(&run.stdout);
    // The combined JSON contains both files' diagnostics, each
    // tagged with its own `file` field.
    assert!(
        stdout.contains("unknown variable 'nope'"),
        "expected first file's diagnostic, got:\n{stdout}"
    );
    assert!(
        stdout.contains("unknown variable 'undefined'"),
        "expected second file's diagnostic, got:\n{stdout}"
    );
    assert!(
        stdout.contains("check_json_a_") && stdout.contains("check_json_b_"),
        "each diagnostic should reference its own path, got:\n{stdout}"
    );
}

#[test]
fn intentc_check_json_empty_for_clean_run_across_files() {
    // Companion to the above: a clean run across multiple files
    // emits `{"diagnostics":[]}` once, not per-file.
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let a = format!("{}/examples/basics.vani", manifest_dir);
    let b = format!("{}/examples/contracts.vani", manifest_dir);
    let run = Command::new(binary)
        .args(["check", "--json", &a, &b])
        .output()
        .expect("intentc check --json clean run");
    assert!(run.status.success());
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert_eq!(
        stdout.trim(),
        "{\"diagnostics\":[]}",
        "expected single empty diagnostics object, got: {stdout}"
    );
}

#[test]
fn fmt_accepts_directory_with_check_and_in_place() {
    // `intentc fmt` should expand a directory arg the same way
    // `intentc test` does — non-recursive, alphabetized — and
    // apply --check or --in-place to each `*.vani` child. The
    // stdout mode is rejected for multi-file input (would dump
    // many files concatenated).
    use std::fs;
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");

    let tmp_dir = std::env::temp_dir().join(format!(
        "intentc_fmt_dir_{}",
        std::process::id()
    ));
    fs::create_dir_all(&tmp_dir).expect("mkdir tmp");
    // Seed two files: one canonical (just produced by fmt) and one
    // intentionally non-canonical (extra spaces inside braces).
    fs::write(
        tmp_dir.join("a.vani"),
        "fn main() -> i64 {\n  return 0;\n}\n",
    )
    .expect("write a");
    fs::write(
        tmp_dir.join("b.vani"),
        "fn main()   -> i64{\n    return 1;\n}\n",
    )
    .expect("write b");
    // Ensure the canonical seed actually matches our formatter.
    fs::copy(
        format!("{}/examples/basics.vani", manifest_dir),
        tmp_dir.join("c.vani"),
    )
    .expect("copy c");

    // (1) Default stdout mode on a directory → error.
    let run = Command::new(binary)
        .args(["fmt", tmp_dir.to_str().unwrap()])
        .output()
        .expect("intentc fmt <dir>");
    assert_eq!(
        run.status.code(),
        Some(1),
        "stdout mode on dir should be rejected"
    );
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("multiple files require --check or --in-place"),
        "expected diagnostic, got:\n{stderr}"
    );

    // (2) --check should exit 1 because the dir has non-canonical
    // files. Each non-canonical file should be reported on stderr.
    let run = Command::new(binary)
        .args(["fmt", "--check", tmp_dir.to_str().unwrap()])
        .output()
        .expect("intentc fmt --check <dir>");
    assert_eq!(run.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("b.vani: not canonically formatted"),
        "expected b.vani listed, got:\n{stderr}"
    );

    // (3) --in-place rewrites, then --check passes silently.
    let run = Command::new(binary)
        .args(["fmt", "--in-place", tmp_dir.to_str().unwrap()])
        .output()
        .expect("intentc fmt --in-place <dir>");
    assert!(run.status.success(), "in-place failed: {}", String::from_utf8_lossy(&run.stderr));
    let run = Command::new(binary)
        .args(["fmt", "--check", tmp_dir.to_str().unwrap()])
        .output()
        .expect("intentc fmt --check <dir> after");
    assert!(
        run.status.success(),
        "check after in-place should pass; stderr:\n{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(run.stdout.is_empty() && run.stderr.is_empty());

    let _ = fs::remove_dir_all(&tmp_dir);
}

#[test]
fn fmt_check_and_in_place_modes_match_canonical_form() {
    // Full life cycle of the new flags on a real example:
    //  1. --check on the unformatted source should exit 1 with a
    //     "not canonically formatted" notice on stderr.
    //  2. --in-place should rewrite to the canonical form, no
    //     change to mtime if the file is already canonical.
    //  3. --check on the canonical source should exit 0 silently.
    //  4. --check + --in-place together should be rejected.
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let src = format!("{}/examples/basics.vani", manifest_dir);
    let tmp = std::env::temp_dir().join(format!(
        "intentc_fmt_check_{}.vani",
        std::process::id()
    ));
    std::fs::copy(&src, &tmp).expect("copy fixture");

    // (1) Unformatted: --check should exit 1.
    let out = Command::new(binary)
        .args(["fmt", "--check", tmp.to_str().unwrap()])
        .output()
        .expect("intentc fmt --check");
    assert_eq!(out.status.code(), Some(1), "expected exit 1 for non-canonical");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not canonically formatted"),
        "expected `not canonically formatted` in stderr, got:\n{stderr}"
    );

    // (2) --in-place rewrites successfully.
    let out = Command::new(binary)
        .args(["fmt", "--in-place", tmp.to_str().unwrap()])
        .output()
        .expect("intentc fmt --in-place");
    assert!(
        out.status.success(),
        "fmt --in-place failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // (3) After --in-place, --check passes silently.
    let out = Command::new(binary)
        .args(["fmt", "--check", tmp.to_str().unwrap()])
        .output()
        .expect("intentc fmt --check (canonical)");
    assert!(
        out.status.success(),
        "fmt --check should pass on canonical file: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty() && out.stderr.is_empty(),
        "check on canonical should be silent: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // (4) --check and --in-place together is rejected.
    let out = Command::new(binary)
        .args(["fmt", "--check", "--in-place", tmp.to_str().unwrap()])
        .output()
        .expect("intentc fmt --check --in-place");
    assert!(
        !out.status.success(),
        "expected non-zero exit for mutually exclusive flags"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("mutually exclusive"),
        "expected mutual-exclusion diagnostic, got:\n{stderr}"
    );

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn fmt_preserves_comments_from_example_with_leading_block() {
    // `examples/vec_invariants.vani` opens with a 10-line `//`
    // block documenting the loop invariant. Earlier versions of fmt
    // would silently strip it. Now run fmt and assert each of those
    // lines reappears in the output.
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/vec_invariants.vani", manifest_dir);
    let source = std::fs::read_to_string(&example).expect("read example");

    let out = Command::new(binary)
        .args(["fmt", &example])
        .output()
        .expect("intentc fmt");
    assert!(out.status.success(), "fmt failed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Every `// …` line from the source must appear somewhere in
    // the formatted output.
    let mut comment_lines = 0;
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            comment_lines += 1;
            assert!(
                stdout.contains(trimmed),
                "missing comment line: `{trimmed}`\nformatted output:\n{stdout}"
            );
        }
    }
    assert!(comment_lines > 0, "test example should have comments");
}

#[test]
fn fmt_roundtrips_every_example() {
    // `intentc fmt` should produce source that re-parses to the
    // same AST. Whitespace and comments may differ; structural
    // shape must not. Runs `intentc fmt` on every example file and
    // pipes the output back through `intentc ast` (to canonicalize
    // the AST dump) for comparison.
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let examples_dir = format!("{}/examples", manifest_dir);

    let mut entries: Vec<_> = std::fs::read_dir(&examples_dir)
        .expect("examples dir readable")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("vani"))
        .collect();
    entries.sort();
    assert!(!entries.is_empty(), "no examples discovered");

    for path in entries {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");

        let fmt_out = Command::new(binary)
            .args(["fmt", path.to_str().unwrap()])
            .output()
            .expect("intentc fmt");
        assert!(
            fmt_out.status.success(),
            "fmt failed for {name}: {}",
            String::from_utf8_lossy(&fmt_out.stderr)
        );

        // Write the formatted source to a temp file so we can run
        // `intentc ast` on it without piping (the CLI takes a path).
        let tmp = std::env::temp_dir().join(format!("fmt_roundtrip_{}", name));
        std::fs::write(&tmp, &fmt_out.stdout).expect("write tmp");

        let ast_a = Command::new(binary)
            .args(["ast", path.to_str().unwrap()])
            .output()
            .expect("intentc ast original");
        let ast_b = Command::new(binary)
            .args(["ast", tmp.to_str().unwrap()])
            .output()
            .expect("intentc ast formatted");
        let _ = std::fs::remove_file(&tmp);

        assert!(ast_a.status.success(), "ast(orig) failed for {name}");
        assert!(ast_b.status.success(), "ast(fmt) failed for {name}");

        // Spans differ (byte offsets shift after formatting), so
        // strip every `span: Span { ... }` substring before
        // comparing. The block always renders on one line in
        // `{:#?}`-style debug output.
        let strip = |s: &str| -> String {
            let mut out = String::with_capacity(s.len());
            let mut chars = s.chars().peekable();
            while let Some(c) = chars.next() {
                if c == 's' {
                    let mut peek = String::new();
                    let mut snapshot = chars.clone();
                    for _ in 0..3 {
                        if let Some(&p) = snapshot.peek() {
                            peek.push(p);
                            snapshot.next();
                        }
                    }
                    if peek == "pan" {
                        for _ in 0..3 { chars.next(); }
                        // skip to closing `}`
                        let mut depth: i32 = 0;
                        for d in chars.by_ref() {
                            if d == '{' { depth += 1; }
                            else if d == '}' {
                                depth -= 1;
                                if depth == 0 { break; }
                            }
                        }
                        continue;
                    }
                }
                out.push(c);
            }
            out
        };

        let a = strip(&String::from_utf8_lossy(&ast_a.stdout));
        let b = strip(&String::from_utf8_lossy(&ast_b.stdout));
        assert_eq!(
            a, b,
            "AST changed across format round-trip for {name}"
        );
    }
}

#[test]
fn emit_llvm_parallel_for_lowers_to_gomp_call() {
    // The LLVM backend lifts each `parallel for` body into an
    // `@__intent_par_<N>` function and calls `@GOMP_parallel`
    // from the parent. Confirm the emitted IR has the expected
    // shape: a declaration of GOMP_parallel, an internal outlined
    // function per parallel-for, and a call site for each.
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/parallel.vani", manifest_dir);

    let out = Command::new(binary)
        .args(["emit", &example, "--backend=llvm"])
        .output()
        .expect("intentc emit --backend=llvm");
    assert!(out.status.success(), "emit failed");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("declare void @GOMP_parallel(void (i8*)*, i8*, i32, i32)"),
        "missing GOMP_parallel declaration:\n{stdout}"
    );
    let outlined = stdout
        .matches("define internal void @__intent_par_")
        .count();
    // All 11 parallel-fors get outlined: three basic, plus eight
    // reductions (`+`, `*`, `||`, `min`, `max`, bitwise `&`,
    // bitwise `|`, bitwise `^`). The `||` case used to fall back
    // to sequential because atomicrmw rejects i1, but the
    // backend now allocates an i8 shadow per bool reduction and
    // runs `atomicrmw or` against it.
    assert_eq!(
        outlined, 11,
        "expected 11 outlined functions, got {outlined}"
    );
    let call_sites = stdout.matches("call void @GOMP_parallel(").count();
    assert_eq!(
        call_sites, 11,
        "expected 11 GOMP_parallel call sites, got {call_sites}"
    );
    // The `+` reduction lowers to `atomicrmw add`; the `*`
    // reduction lowers to a `cmpxchg` retry loop (atomicrmw
    // doesn't expose `mul`). For signed integers, `min`/`max`
    // lower to the dedicated `atomicrmw min`/`atomicrmw max`
    // instructions (the unsigned variants are `umin`/`umax`).
    // Bool `||` lowers to `atomicrmw or i8*` via the shadow.
    // Bitwise `&` / `|` / `^` lower to native-width
    // `atomicrmw and` / `or` / `xor` (no shadow needed because
    // the integer width is already byte-aligned).
    assert!(
        stdout.contains("atomicrmw add"),
        "expected atomicrmw add lowering:\n{stdout}"
    );
    assert!(
        stdout.contains("cmpxchg"),
        "expected cmpxchg lowering for `*` reduction:\n{stdout}"
    );
    assert!(
        stdout.contains("atomicrmw min"),
        "expected atomicrmw min lowering for min reduction:\n{stdout}"
    );
    assert!(
        stdout.contains("atomicrmw max"),
        "expected atomicrmw max lowering for max reduction:\n{stdout}"
    );
    assert!(
        stdout.contains("atomicrmw or i8*"),
        "expected atomicrmw or on i8 shadow for `||` reduction:\n{stdout}"
    );
    assert!(
        stdout.contains("atomicrmw and i64*"),
        "expected native-width atomicrmw and for bitwise `&` reduction:\n{stdout}"
    );
    assert!(
        stdout.contains("atomicrmw or i64*"),
        "expected native-width atomicrmw or for bitwise `|` reduction:\n{stdout}"
    );
    assert!(
        stdout.contains("atomicrmw xor i64*"),
        "expected native-width atomicrmw xor for bitwise `^` reduction:\n{stdout}"
    );
}

#[test]
fn emit_c_parallel_for_pragma_appears_in_output() {
    // The C backend lowers `parallel for` to a regular for loop
    // preceded by `_Pragma("omp parallel for")`. Compilers with
    // -fopenmp parallelize; compilers without it warn-and-run
    // sequentially. The Run path auto-adds -fopenmp when the
    // probe succeeds, so the user pays nothing for unsupported
    // toolchains.
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/parallel.vani", manifest_dir);

    let out = Command::new(binary)
        .args(["emit", &example, "--backend=c"])
        .output()
        .expect("intentc emit --backend=c");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let pragma_count = stdout.matches("_Pragma(\"omp parallel for").count();
    assert_eq!(
        pragma_count, 11,
        "expected 11 omp pragmas (one per parallel for in the example), got {pragma_count}:\n{stdout}"
    );
    // Each reduction op contributes its `reduction(op: var)`
    // clause to the corresponding pragma. Tree-C names the
    // reduction var after the source binding (e.g. `v_total`);
    // SSA-C names it after the SSA carry value-id (e.g.
    // `v_37`). Functionally equivalent — accept either.
    fn has_reduction(stdout: &str, op: &str) -> bool {
        // Match `reduction(<op>:<anything>)` (tree-C) and
        // `reduction(<op>: <anything>)` (SSA-C).
        stdout.contains(&format!("reduction({}:", op))
            || stdout.contains(&format!("reduction({}: ", op))
    }
    assert!(has_reduction(&stdout, "+"), "expected `+` reduction clause:\n{stdout}");
    assert!(has_reduction(&stdout, "*"), "expected `*` reduction clause:\n{stdout}");
    assert!(has_reduction(&stdout, "||"), "expected `||` reduction clause:\n{stdout}");
    assert!(has_reduction(&stdout, "min"), "expected `min` reduction clause:\n{stdout}");
    assert!(has_reduction(&stdout, "max"), "expected `max` reduction clause:\n{stdout}");
    assert!(has_reduction(&stdout, "&"), "expected `&` reduction clause:\n{stdout}");
    assert!(has_reduction(&stdout, "|"), "expected `|` reduction clause:\n{stdout}");
    assert!(has_reduction(&stdout, "^"), "expected `^` reduction clause:\n{stdout}");
}

#[test]
fn run_parallel_example_proves_race_free_and_runs() {
    // End-to-end: the effects verifier accepts every `pure fn`
    // and `parallel for` in the example, then the backend lowers
    // the loops sequentially (semantics-preserving). Output is
    // just `0` — the example doesn't print loop values, only the
    // sentinel.
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/parallel.vani", manifest_dir);
    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run");
    assert!(
        output.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Example output: bias+xs[0] = 110, sum = 100, product =
    // 240000, OR over flags = 1, min of xs = 10, max of xs = 40,
    // bit-AND of xs = 0, bit-OR of xs = 62, bit-XOR of xs = 40.
    // (xs = [10, 20, 30, 40]; 10&20&30&40 = 0; 10|20|30|40 = 62;
    // 10^20^30^40 = 40, which collides with the max-output, so
    // the unique signal for the XOR pragma is `62` for OR.)
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in ["110", "100", "240000", "1", "10", "40", "0", "62"] {
        assert!(
            stdout.contains(line),
            "expected line `{line}` in output, got:\n{stdout}"
        );
    }
}

#[test]
fn emit_llvm_parallel_for_with_captures_extends_ctx_struct() {
    // When the parallel-for body reads outer bindings, the LLVM
    // backend extends the inline ctx struct with one pointer
    // field per capture, stores the parent allocas into those
    // fields at the call site, and emits matching loads in the
    // outlined function. Pin the resulting IR shape so a future
    // refactor can't silently drop captures.
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/parallel.vani", manifest_dir);

    let out = Command::new(binary)
        .args(["emit", &example, "--backend=llvm"])
        .output()
        .expect("intentc emit --backend=llvm");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);

    // At least one outlined function unpacks captures via
    // `%cap_<i> = load …, … * %cap_<i>_p` lines.
    assert!(
        stdout.contains("%cap_0_p = getelementptr"),
        "expected capture-field getelementptr in outlined fn:\n{stdout}"
    );
    assert!(
        stdout.contains("%cap_0 = load"),
        "expected capture-field load in outlined fn:\n{stdout}"
    );
}

#[test]
fn run_strings_concat_example_prints_joined_owned_strings() {
    // OwnedStr surface end-to-end: `Str + Str` allocates and
    // returns an OwnedStr; chaining a second concat consumes the
    // first OwnedStr and frees its buffer inside the helper.
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/strings_concat.vani", manifest_dir);
    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run");
    assert!(
        output.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Hello, alice!"),
        "expected joined alice greeting, got:\n{stdout}"
    );
    assert!(
        stdout.contains("Hello, bob."),
        "expected joined bob greeting, got:\n{stdout}"
    );
}

#[test]
fn run_strings_example_prints_each_greeting() {
    // Pins the Str feature surface: Str param, Str return, let-bound
    // Str, and ==/!= via strcmp. Also a smoke test for the LLVM
    // `Discard` path — `let _ = greet("alice")` must execute even
    // though the i64 result is dropped.
    let binary = env!("CARGO_BIN_EXE_intentc");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let example = format!("{}/examples/strings.vani", manifest_dir);

    let output = Command::new(binary)
        .args(["run", &example])
        .output()
        .expect("intentc run should execute");

    assert!(
        output.status.success(),
        "intentc run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello, alice"),
        "expected greeting in stdout, got: {stdout}"
    );
    assert!(
        stdout.contains("role: member"),
        "expected `role: member` (alice via != \"guest\" branch), got: {stdout}"
    );
    assert!(
        stdout.contains("role: visitor"),
        "expected `role: visitor` (guest falls through), got: {stdout}"
    );
    assert!(
        stdout.contains("len: 5"),
        "expected `len: 5` from len(\"hello\"), got: {stdout}"
    );
}
