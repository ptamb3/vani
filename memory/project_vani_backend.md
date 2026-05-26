---
name: project-vani-backend
description: "vāṇī compiler state — pipeline, backends, verifier; refreshed 2026-05-25 to reflect closures #1-#220 landed"
metadata:
  node_type: memory
  type: project
---

`~/vani/` (renamed from `~/future-compiler/` on 2026-05-21) is
a Rust-based verified-affine compiler. **VANI** = *Verbose
Alternative Natural Interface*; *वाणी* = Sanskrit for "speech".

## Pipeline

Lexer → Parser → Type-checker / SMT verifier (z3) → Typed IR
(tree-shaped, [`src/ir.rs`](src/ir.rs)) → SSA IR
([`src/ssa.rs`](src/ssa.rs), [`src/ssa_pass.rs`](src/ssa_pass.rs))
→ one of four backends:

| Backend | Module | Role |
|---|---|---|
| `LlvmBackend` | [`src/backend_llvm.rs`](src/backend_llvm.rs) | Tree-LLVM — fallback for shapes SSA-LLVM doesn't cover |
| `CBackend` | [`src/backend_c.rs`](src/backend_c.rs) | Tree-C — same role for C output |
| `ssa_backend_llvm` | [`src/ssa_backend_llvm.rs`](src/ssa_backend_llvm.rs) | Default LLVM path |
| `ssa_backend_c` | [`src/ssa_backend_c.rs`](src/ssa_backend_c.rs) | Default C path |

The `emit_c_via_ssa` / `emit_llvm_via_ssa` helpers in
[`src/main.rs`](src/main.rs) run a per-program
`ssa_path_supports` gate and fall through to the tree backend
when SSA can't represent a shape (Vec<Atomic|Channel>,
parallel-for-with-payloaded-enums, etc.).

## Language surface (as of #220)

- **Scalars:** i8/i16/i32/i64, u8/u16/u32/u64, f32/f64, bool
- **Aggregates:** `Vec<T>` (heap, affine, monomorphized), `[T;N]` (stack), `Tuple` (Copy-only in v1)
- **Strings:** `Str` (borrowed `const char*`), `OwnedStr` (heap, affine)
- **User types:** `struct {...}`, `enum {Variant, Variant(payload)}`
- **References:** second-class `ref T` / `mut ref T` (params only)
- **Function-pointers:** `fn(T) -> R` — first-class, Copy
- **`dyn IfaceName`:** Phase 1 parsed (#220); coercion + dispatch pending Phase 2-3
- **Concurrency:** `parallel for` with reductions, `task` + `join`, `Atomic<T>`, `Channel<T,N>`, `Mutex<T>` + `Guard<T>` (i64 only)
- **Control flow:** if/else, while, for-range, for-iter, break, continue, match (int/bool/enum/Str patterns), `try` keyword (Rust `?` sugar for early-return on None/Err)
- **Block expressions:** `let r = { stmts; tail };` — Let + Print stmts before tail (closures #129, #194-#201 hardened the scope drops)
- **Builtins:** `vec(...)`, `push`, `push_mut`, `pop` (#219), `set`, `clone`, `clone_at`, `len`, `atomic_*`, `channel_send/recv`, `mutex_lock`, `guard_get/set`

## Verifier (z3)

`requires` / `ensures` / `invariant` / `prove` clauses discharge
via z3. BitVec overflow-aware arithmetic, IEEE-754 floats with
casts, shifts, inline-call discharge of callee ensures, loop
invariants with post-loop narrowing, contradictory-requires
detection, Vec-builtin length facts, SMT array theory for
indexing, bounds elision, effects/ownership analysis
(`pure fn`, affine `Vec<T>` / `OwnedStr`), race-freedom proofs
for `parallel for`.

## Affine ownership

Auto-Drop at scope exit for every non-Copy non-moved binding.
Partial-move tracking is one-level deep (`let xs = t.x` works;
nested `let y = t.x.inner` rejected). Multi-field drop order
is declaration-order today (Rust convention = reverse-decl;
deferred). `Drop for T` is suppressed when T has heap fields
(per-field free runs instead).

## Big items closed this session (#193-#220)

- Block-expression family: scope-exit drops, sibling-let drops,
  shadow-name handling, `let _` discard, RHS move tracking,
  walker recursion for enum/alias/tuple resolution, user-Drop
  for Copy structs (closures #194-#201, #207).
- Parametric type spelling (Atomic / Channel / FnPtr) across
  every type-spelling helper: `c_element_storage`,
  `element_tag`, `vec_struct_tag`, `format_declarator`. Eight
  separate bugs fixed (closures #208-#215).
- `try` keyword polish: nested blocks, multiple trys in one
  body (#217, #218).
- `pop(mut ref xs) -> T` builtin — completes the Vec-as-stack
  story (#219).
- Vtables Phase 1 — `Type::Object` + `dyn Iface` parsing
  (#220). Phase 2 (coercion + method dispatch) and Phase 3
  (codegen) queued.
- Tree-C / SSA-C codegen quality: `-Wstrict-prototypes`,
  `-Wmissing-braces`, `-Wunused-label`, `-Wswitch-bool`,
  `-Wuninitialized`, `-Wdiscarded-qualifiers` clean across all
  58 examples.

## Default-behavior reminders

- **LLVM is the default backend.** `intentc emit` /
  `intentc run` / `intentc build` route through
  `emit_llvm_via_ssa`. C backend stays available via
  `--backend=c`.
- **Use `llvm_type_string` for aggregates and references** —
  `llvm_type` intentionally panics on those so callers don't
  silently pick i64 as a fallback.
- **OMP_NUM_THREADS=1 under lli** — MCJIT isn't thread-safe
  for concurrent function resolution.
- **libgomp `-load=` probing** lives in
  `add_libgomp_load_flags` (in main.rs) and a test-local
  mirror in backend_llvm.rs. Honor `INTENT_LIBGOMP`.

## Test totals (2026-05-25 post-#228)

**920 lib + 47 e2e + 11 vtables-phase3 tests passing.**
Cross-backend parity runner covers all 58 examples. ASan
/ UBSan clean across all examples. LLVM `opt -verify` /
`opt -O3` clean. Vtables epic A is end-to-end on both
backends through all 5 phases: dispatch + codegen + struct
fields + Vec<dyn> + ref dyn + auto-borrow `==`.

## Pending epics

| # | Item | Effort | Status |
|---|---|---|---|
| A | Vtables — Phase 2 (coercion + method dispatch) | ~6-8h | Phase 2a (#221) + Phase 2b (#222) done |
| A | Vtables — Phase 3 (codegen) | ~12-16h | Phase 3a tree-C (#223) + Phase 3b tree-LLVM (#224) done |
| A | Vtables — Phase 4 (collections + borrows) | ~6h | 4a (#225) + 4b (#226) + 4c (#227) done |
| A | Vtables — Phase 5 (auto-`==` polish) | ~3h | Done (#228) |
| B | Partial-move expansion (per-field `moved`, multi-field reverse-decl drop, Mutex<non-i64>) | medium per item | Queued |
| C | `Drop for T` with heap fields (`fn drop(mut self: T)` signature design) | medium | Needs design |
| #5 last | Non-let stmts between `try` and `return` | low | Needs Block-expr stmt vocabulary extension |
| Devanagari | Script-aware diagnostics + grammar review | medium | Parked per user request |

## Related memory

- [[project-vani-status-file]] — STATUS.md update protocol
- [[feedback-vani-file-access]] — standing approval for /tmp + ~/vani access
