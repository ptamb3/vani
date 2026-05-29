# Onboarding

This is a Rust-implemented compiler for an experimental, SMT-verified
systems language. The README has the language reference; this file
covers how to *work on the compiler itself*.

## Setting up

Rust toolchain (1.75+), `z3`, `lli`, `llc`, `opt`, and `cc`:

```bash
sudo apt install build-essential z3 llvm clang   # Debian/Ubuntu
brew install z3 llvm                              # macOS
```

Verify:

```bash
cargo --version
z3 --version
lli --version
llc --version
opt --version
cc --version
```

The compiler **runs** without the LLVM tools — `intentc check` only
needs Rust and z3. `intentc run --backend=llvm` needs `lli`; `intentc
build` needs `llc` + `cc` (+ optional `opt`). The C backend (`run
--backend=c`) needs `cc`.

`INTENTC_NO_VERIFY=1` skips SMT entirely for fast iteration on
non-proof code changes. Don't set it in CI.

## Build, test, run

```bash
cargo build                         # Quick build
cargo test                          # Full suite (1255 lib + 54 e2e + 14 other, ~90s)
cargo test smt_                     # Subset matching a prefix
cargo test --release                # Faster compile, same coverage
cargo clippy                        # Lints
cargo run -- run examples/basics.vani   # Try a sample
```

If a test needs `lli` / `llc` / `z3` and the tool isn't installed,
the test gracefully *skips* rather than fails. Look for tests gated
by `lli_available()` / `z3_available()`.

## Project layout

```
src/
  ast.rs              Source-level AST (Stmt, Expr, Type, BinaryOp)
  lexer.rs            Tokenizer
  parser.rs           Hand-written recursive-descent parser
  checker.rs          Type checker + verifier driver. Largest file by far.
  ir.rs               Typed IR (TypedExpr, TypedStmt). Backend input.
  smt.rs              SMT-LIB encoder + z3 subprocess driver
  diagnostic.rs       Error/note formatting (human + JSON)
  span.rs             Source spans
  backend.rs          `Backend` trait — single emit() surface
  backend_c.rs        C backend (legacy; see deprecation note in file)
  backend_llvm.rs     LLVM IR backend (default; primary target)
  main.rs             CLI: check / emit / emit-c / run / build
  lib.rs              Public API + most unit tests
  ssa.rs              SSA-form IR + lowering pass (the closure-#241/#251
                      parallel-for region recognition lives here)
  ssa_backend_c.rs    SSA-C backend (used for parallel-for + tasks paths)
  ssa_backend_llvm.rs SSA-LLVM backend (used when the SSA path supports
                      the program shape; falls back to tree-LLVM otherwise)
  format.rs           Formatter (`intentc fmt`); preserves comments + blank
                      lines, round-trips to the same AST.
tests/
  run_end_to_end.rs   Integration: invoke the binary on real .vani files
examples/             User-facing .vani sample programs
README.md             Language reference (user-facing)
docs/namespaces_design.md  Design rationale for the namespaces feature
                            (modules, use, kosh).
```

## Pipeline overview

```
.vani  →  Lexer  →  Parser  →  Checker (+ SMT)  →  TypedIR  ─┬→  Tree Backend  →  output
                                                              └→  SSA Lowerer  →  SSA Backend  →  output
```

* The **checker** does type checking *and* verification. It calls
  z3 via `src/smt.rs` for `prove`/`ensures`/loop invariants, plus
  the SMT-discharged elision pass that flips `checked: bool` on
  `Index`/`Binary` nodes when proven safe.
* The **IR** (`TypedIR`) is tree-shaped. The `checked: bool` field
  on Index/Binary is the main backend-affecting verifier output.
* The **SSA lowerer** (`src/ssa.rs`) optionally lowers the tree IR
  into a CFG-of-basic-blocks form. The driver wrappers
  `emit_c_via_ssa` / `emit_llvm_via_ssa` in `main.rs` try the SSA
  path first; if the SSA backend returns `EmitError` (program shape
  the SSA path doesn't handle yet) they transparently fall back to
  the tree backend. Both paths produce semantically identical output;
  the cross-backend parity test pins this.
* **Backends** consume either the tree `TypedProgram` (tree-C /
  tree-LLVM) or the SSA `Module` (ssa-C / ssa-LLVM) and emit text
  (C source / LLVM IR). The `Backend` trait in `src/backend.rs` is
  the tree-path emit surface.

## Where to look for common changes

| You want to… | Edit |
|---|---|
| Add a new keyword / syntax | `lexer.rs` + `parser.rs` + `ast.rs` |
| Add a type-system rule | `checker.rs` |
| Extend SMT encoding | `smt.rs::encode_expr` |
| Add a new SMT-discharged proof | `checker.rs::try_smt_prove` / `try_elide_*` |
| Change C output | `backend_c.rs` |
| Change LLVM output (tree path) | `backend_llvm.rs` |
| Change LLVM output (SSA path) | `ssa_backend_llvm.rs` |
| Change C output (SSA path) | `ssa_backend_c.rs` |
| Lower a new construct into SSA | `ssa.rs::lower_*` |
| Recognize a region (parallel-for / task) | `ssa_backend_c.rs::recognize_*` (shared with SSA-LLVM) |
| Add a CLI subcommand or flag | `main.rs` (HELP + `parse_emit_args`) |
| Add an example program | `examples/<name>.vani` + wire into both `check_examples_all_succeed` and a `run_*` test in `tests/run_end_to_end.rs` |

## Conventions

* **LLVM is the default backend.** New work targets LLVM first; the
  C backend (`backend_c.rs`) carries a deprecation note. See its
  module-top comment for the retirement plan.
* **Cross-backend parity** is enforced by
  `tests/run_end_to_end.rs::llvm_backend_run_produces_same_output_as_c`,
  which diffs both backends' stdout + exit code across every
  example. Add new examples there.
* **Verifier failure modes are conservative.** When the SMT layer
  can't discharge a goal (Unknown/SkippedUnsupported/Unavailable),
  the elision pass leaves runtime guards in place. Sound-then-
  precise is the rule: never silently drop a guard.
* **No new `.md` files** unless the user asks. Both this file and
  the README were explicitly requested.
* **Tests live next to the feature they test** — unit tests in
  `lib.rs`'s `tests` module; integration tests in
  `tests/run_end_to_end.rs`. Pre-existing examples have their own
  `run_<example>_example` test for documentation by example.

## Useful env vars

| Var | Effect |
|---|---|
| `INTENTC_NO_VERIFY=1` | Skip all SMT round-trips (dev opt-out) |
| `INTENTC_SMT_DEBUG=1` | Dump every SMT query + z3 output to stderr |
| `INTENTC_SMT_NO_CACHE=1` | Disable the in-process SMT result cache |
| `Z3=path`, `LLI=path`, `LLC=path`, `OPT=path`, `CC=path` | Override tool lookups |

## Adding a feature, end-to-end

1. **Sketch in `examples/`** as a `.vani` program that exercises
   the new shape. If it can't typecheck yet, that's expected; come
   back to it in step 4.
2. **Wire through the pipeline:** lexer → parser → AST → IR
   → checker → both backends. Each step has tests next to the
   relevant file.
3. **Add per-backend tests** in `lib.rs` (for static shape) and
   `tests/run_end_to_end.rs` (for end-to-end behavior). Pin
   counterexamples / abort exits to specific values.
4. **Wire the example** into `check_examples_all_succeed` and add
   a `run_<example>_example` test. The cross-backend parity test
   will pick it up automatically.
5. **Run `cargo test` + `cargo clippy`** before committing.

## What's still ahead

See README's *Roadmap* section for the longer arc and
[TODO.md](TODO.md) for the canonical queue. As of 2026-05-29 the
heavy lifts that **have** landed (some originally listed here
as ahead) include:

- CFG/SSA IR refactor.
- Parallelism (`parallel for` / reductions / atomics / channels
  / mutexes / tasks / `Condvar`).
- Ownership-in-verifier (affine types, Drop, vec/owned-str heap
  tracking).
- Payloaded enums + match (incl. mixed-payload enums).
- Vtables for dynamic dispatch (`dyn Iface`).
- Namespaces (modules with visibility, `use`, re-exports,
  `pub(kosh)`).
- `Str` + `OwnedStr`.
- FFI v1–v8 (`extern "C" fn`, `--link-with`, FFI callbacks).
- **Data-structures + algorithms roadmap, Levels 1–4** (closures
  #293–#345): `Vec` sort / search / mutator builtins, math / RNG /
  hash, `BinaryHeap` / `Deque` / `HashSet` (with tombstone
  remove) / `HashMap` (with tombstone remove) / `BTreeSet` /
  `BTreeMap`, anonymous fns + closures with captures + iterator
  combinators + method-call sugar, `UnionFind` / `BloomFilter` /
  AVL `Bst` / weighted `Graph` + BFS/DFS/Dijkstra/A*/topo-sort/
  Kruskal/Prim, `Trie` with delete + arena compaction + full u8
  alphabet, `SkipList` with O(1) `max` via maintained `tail_node`.
  See [STATUS.md](STATUS.md) for the closure-by-closure history.

Currently pending:
- Closure-as-value richer support (capture-by-ref, non-Copy
  captures, `.collect()`, lazy iterators, non-i64 element types
  in combinators, tuple-element `vec_zip`).
- `BTreeSet`/`BTreeMap` range queries.
- Sparse per-node children for `Trie` (memory optimization;
  currently fixed 256-wide after closure #345).
- Hash / Ord trait for user struct keys; SipHash; `hash_f64`.
- SSA-LLVM multi-block atomicrmw emit (Phi-traceback; tree-LLVM
  is the correctness fallback today).
- The **kosh** (कोश) package-manager arc: manifest (`kosh.toml`),
  resolver + lockfile, registry + CLI, and stdlib-as-kosh.
  Currently single-kosh; `pub(kosh)` already records intent.
- Devanagari SOV word order and 3-way Sanskrit/Hindi/Marathi
  alias parity (both blocked on grammar review).
- `async` (deferred until concrete need; coroutines or
  poll-and-runtime are both possible, decision deferred).
