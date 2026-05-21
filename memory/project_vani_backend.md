---
name: project-vani-backend
description: "vani (formerly future-compiler) backend status — LLVM is default, parallelism + reductions shipped"
metadata: 
  node_type: memory
  type: project
  originSessionId: 656e7218-5702-41e7-a9dd-1764e5b8ee2c
---

> **Project renamed 2026-05-21**: `~/future-compiler/` → `~/vani/`,
> crate `future_compiler` → `vani`. VANI = *Verbose Alternative
> Natural Interface*; वाणी = Sanskrit for "speech". Same compiler
> pipeline, same code, new identity. Refer to the new path going
> forward.

The `~/vani/` project's compiler pipeline is now Lexer
→ Parser → Type checker / SMT verifier → Typed IR (tree-shaped)
→ backend, with two backend implementations: `LlvmBackend`
([src/backend_llvm.rs](src/backend_llvm.rs)) and `CBackend`
([src/backend_c.rs](src/backend_c.rs)). Both implement the
`Backend` trait in [src/backend.rs](src/backend.rs).

**Why this memory exists:** the project went through a mid-2026
re-platforming from a C-only backend to LLVM-first. Some older
notes still anchor on the C backend as primary; that's stale.

**Status (2026-05-19):**

- **SSA pipeline is now first-class.** `intentc emit/run/build`
  (both `--backend=c` and `--backend=llvm`) routes through
  `emit_c_via_ssa` / `emit_llvm_via_ssa` helpers in
  [src/main.rs](src/main.rs). Each helper runs a module-wide
  `ssa_path_supports(&TypedProgram)` gate: programs that use
  Vec/Array/Channel/Atomic/Mutex/Guard/OwnedStr (in params or
  returns), parallel-for, Tasks, multi-item Print, Str-literal
  Print items, Assert with a custom message, or
  OwnedStr-producing exprs (Str+Str concat) bypass SSA and
  emit via the tree backends. The rest of the suite flows
  through `lower_program` → `ssa_backend_c` /
  `ssa_backend_llvm`. SSA-LLVM's `intent_print` emits per-type
  format-string globals + `@printf`; `Terminator::Return`
  now uses the function's declared return type; `Const::Float`
  uses `fadd <T> 0.0, c` so float literals stay typed.
- **LLVM is the default** for `intentc emit`, `intentc run`, and
  `intentc build` (the last AOT-compiles to a native binary via
  `llc -filetype=obj` → `cc` linker). C backend is preserved for
  back-compat (`intentc emit-c`, `intentc emit --backend=c`,
  `intentc run --backend=c`) and is on the deprecation path.
- **Verifier surface** that already landed: BitVec overflow-aware
  arithmetic, IEEE-754 floats with casts, shifts, contracts
  (requires/ensures with inline-call discharge), loop invariants
  with post-loop narrowing, contradictory-requires detection,
  Vec-builtin length facts (vec/push/set/clone), SMT array
  theory for `xs[i]` reasoning, bounds elision, an
  effects/ownership verifier (`pure fn`, affine `Vec<T>` /
  `OwnedStr`), and **race-freedom proofs for `parallel for`**.
- **Parallelism** is built end-to-end. C lowers `parallel for`
  to `_Pragma("omp parallel for")` (the `run` driver auto-adds
  `-fopenmp` when toolchain probe succeeds). LLVM lifts each
  body into `@__intent_par_<N>` outlined fns that call
  `@GOMP_parallel` from libgomp; captures pass by pointer
  through an inline ctx struct.
- **Reduction ops** supported on `parallel for`: `+`, `*`, `&&`,
  `||`, `min`, `max`. LLVM lowering: `atomicrmw add` for `+`,
  `cmpxchg`-retry for `*` (atomicrmw doesn't expose mul),
  `atomicrmw min`/`max` (signed) or `umin`/`umax` (unsigned)
  for `min`/`max`, and `atomicrmw and`/`or i8*` against a
  parent-allocated i8 shadow for `&&`/`||` (atomicrmw rejects
  i1; the shadow is zext-initialized, atomically updated by
  the outlined fn, and on exit the parent does `icmp ne i8 …,
  0` and stores back into the original i1 alloca).
- **LLVM backend TODO paper-cuts** got a sweep on 2026-05-18:
  14 defensive sites converted to `unreachable!` with clear
  panic messages; array-let-from-var copies via whole-aggregate
  load/store (LLVM optimizes to memcpy, matching the C
  backend's `memcpy(v_ys, v_xs, sizeof(v_ys))`); Discard of a
  Vec extracts the data pointer and calls `@free`.

**How to apply:**

- **Default to LLVM** for new backend work. Touch the C backend
  only for parity bug fixes or explicit `--backend=c` paths.
- **Use `llvm_type_string` for aggregates and references**
  (`[N x T]`, `%intent_vec_<elt>`, `<inner>*`); `llvm_type`
  intentionally panics on those types so callers don't silently
  pick `i64` as a fallback.
- **OMP_NUM_THREADS=1 under lli.** MCJIT isn't thread-safe for
  concurrent function resolution; `intentc run` and the
  `run_lli` test helper both cap to one thread. AOT builds
  (`intentc build`) leave the env alone for real parallelism.
- **libgomp `-load=` probing** lives in two places:
  `add_libgomp_load_flags` in [src/main.rs](src/main.rs) and a
  test-local mirror `add_libgomp_load_flags_for_tests` in
  [src/backend_llvm.rs](src/backend_llvm.rs) (the tests can't
  reach `main.rs`). Honor `INTENT_LIBGOMP` as the env override.

**Pending roadmap items** (snapshot — see
[TODO.md](TODO.md) in the repo for the live list):
bitwise `&`/`|`/`^` reductions (need new `BinaryOp` variants
first), `task` keyword, CFG/SSA IR refactor, LSP, Cranelift
backend.

**Test totals (2026-05-19 post-SSA-flip):** 439 lib +
47 e2e tests passing. Bool-reduction lli tests are gated on
`lli_available()`; the helper also -loads libgomp.
