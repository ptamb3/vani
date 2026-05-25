# vāṇī (वाणी) — Project Status

> The project was renamed from `future_compiler` to **VANI** (वाणी, Sanskrit for "speech")
> on 2026-05-21. "VANI" expands to *Verbose Alternative Natural Interface* — the
> design goal is code that reads like speech, not punctuation.

> Single-page snapshot of what the compiler does today, what's queued
> next, and known issues. Update this file whenever a feature lands,
> a TODO is added/closed, or an issue is resolved/discovered.
> Cross-reference [README.md](README.md) for the language tour and
> [TODO.md](TODO.md) for the canonical work list.

**Last updated:** 2026-05-24
**Test totals:** 904 lib + 47 end-to-end tests passing; the cross-backend parity runner covers all 57 examples under `examples/`. (Win32 LLVM dispatch adds 4 host-gated tests that fire on Windows hosts only — futex/WaitOnAddress, CreateThread for tasks, plus the new CreateThread fan-out parallel-for tests in tree-LLVM and SSA-LLVM.)

---

## Current feature set

### Language surface
- **Scalar types:** `i8/16/32/64`, `u8/16/32/64`, `f32/64`, `bool` (all `Copy`).
- **Aggregates:** `[T; N]` (stack, affine), `Vec<T>` (heap, affine, monomorphized struct) with `vec`/`push`/`set`/`clone`/`len`/index/`clone_at`. `Vec<T>` accepts non-`Copy` elements (`Vec<Vec<i64>>`, `Vec<[i64; N]>`); reading inner non-Copy slots into a binding goes through `clone_at(&xs, i)` (returns an owned deep-clone) since bare indexing would alias.
- **Strings:** `Str` (borrowed C-string, `Copy`, `==`/`!=`/`<`/`<=`/`>`/`>=` via strcmp, `len` via strlen), `OwnedStr` (heap, affine, produced by `+` concat).
- **References:** second-class `&T` and `&mut T` (params only, no returns, no let-bindings, no nested refs). Call-site aliasing rejected. Auto-deref for indexing.
- **First-class fn-pointers** `fn(T1, ...) -> R` (FnRef + indirect call). Pure / parallel-for / lock-graph passes reject indirect calls conservatively.
- **Control flow:** `if/else`, `else if`, `while`, `for i in lo..hi`, `for x in &xs`, `for x in xs` (consuming), `break`, `continue`. Lexical scoping (nested scopes, shadowing).
- **Constructs:** `let`, `let _ =`, plain `name = expr;` reassignment, `assert cond[, "msg"]`, `prove`, `print` (multi-item), discarded `call();` / `receiver.method();` as a statement (sugar for `let _ = …`), block expressions `let r = { let a = …; let b = …; a + b };` (Let stmts then tail expr).
- `intent "…";` module header; multi-file with `use "path.intent";` (transitive resolve, cycle detection).

### Verification & contracts
- `requires` / `ensures` — call-site discharge, callee-side check. `ensures _return[k] == V` array facts propagate.
- `invariant` on `while`/`for` (entry + preservation with last-reassignment substitution + post-loop facts).
- Contradictory-requires detection.
- SMT (z3) with BitVec overflow semantics, FloatingPoint theory (NaN/±inf modeled), signed/unsigned compare split, casts via sign/zero-extend.
- Symbolic SMT arrays per Vec/Array binding with versioned store axioms.
- SMT-driven runtime-guard elision: bounds, divisor, shift checks dropped when discharged.
- Three-layer `prove`: constant fold → structural tautology → SMT.
- Dev opt-out `INTENTC_NO_VERIFY=1`.
- Affine-ownership with move-state reconciliation at if/while merges and break/continue jump points; per-scope auto-drops.

### Concurrency
- `parallel for` with reduction clauses (`+`, `*`, `&&`, `||`, `min`, `max`, bitwise `&`/`|`/`^`). Verifier proves race-freedom.
- `task <name> { … } / join <name>;` — affine handles, Copy-only captures, real-thread.
- `Atomic<T>` for i8..i64, u8..u64, bool — `atomic_new`/`load`/`store`/`fetch_add`/`compare_exchange`, seq_cst.
- `Channel<T, N>` — Vyukov MPSC ring buffer (per-slot seq counter, power-of-2 N, integer + bool elements).
- `Mutex<T>` + RAII `Guard<T>` — Drepper three-state futex on Linux, WaitOnAddress on Windows, sched_yield/SwitchToThread fallback elsewhere. Cross-function double-acquire detection via transitive `locks_params` propagation.

### Compilation pipeline
- Lexer → Parser (error recovery) → Type checker → SMT verifier → Typed IR → SSA IR → backend.
- Two backends:
  - **LLVM** (default for `emit` / `run` / `build`, AOT via `llc + cc`).
  - **C** (`--backend=c`, legacy; still authoritative for several patterns).
- `intentc` routes through the SSA pipeline first with graceful fallback to the tree backends when the SSA-path gate rejects the program (parallel-for, Tasks, `Channel`/`Atomic`/`Mutex`/`Guard` params).
- **SSA-C covers:** scalars, control flow, arrays, Vec, StrLit, RefOf, fn-pointers, multi-item print, strcmp/strlen, OwnedStr concat, assert-with-message, OwnedStr Drop.
- **SSA-LLVM covers:** same set + phi nodes for block params, vec_struct typedefs, `intent_str_concat` reused from tree-LLVM. `Hint(ParallelForBegin)` regions get **real `@GOMP_parallel` outlining** including:
  - **Captures** — body-block free variables get marshalled by-value through the ctx struct under the shared `%cap_<i>_p` / `%cap_<i>` naming convention; the outlined fn loads each capture and aliases it to the SSA `%v_<id>` so body instructions resolve without operand rewriting.
  - **All reduction ops** — `+` (`atomicrmw add`), `&`/`|`/`^` bitwise (`atomicrmw and/or/xor`), `min`/`max` (`atomicrmw min`/`max` for signed, `umin`/`umax` for unsigned), `*` via `cmpxchg` retry loop, `&&`/`||` on bool via an i8 shadow (parent allocates i8, zext-stores init; outlined fn `atomicrmw and/or` on i8 with `zext i1→i8` of the increment; final-load uses `icmp ne i8 _, 0`). Parent-side `alloca` accumulator carries the user's init value; the pointer flows through the ctx struct; after `@GOMP_parallel` the parent loads the final value and binds to the exit-block param.
  - **`Hint(TaskBegin)` tasks** — single-block task bodies get lifted into `@intent_task_<N>` outlined fns; the spawn site emits `@pthread_create` (POSIX) or `@CreateThread` (Win32) against the outlined fn with a heap-allocated ctx holding captures by-value; the matching `Hint(TaskJoin)` emits `@pthread_join` / `@WaitForSingleObject` + `@CloseHandle` plus `@free` of the ctx. Multi-block task bodies (with control flow inside the `task { … }`) surface `EmitError` → tree-LLVM fallback.
- **SSA-C handles `parallel for`** via the
  `emit_parallel_for_region` machinery for single-block
  bodies with the canonical {counter, …reductions} carry
  shape. Emits structured `for (v_<counter> = start; v_<counter> < end; v_<counter>++) { … }` + `_Pragma("omp parallel for [reduction(op: v_<carry>)…]")`.
  `min` / `max` are recognized as intrinsics and lower to
  inline ternaries (matches tree-C). Multi-block bodies and
  non-canonical carry shapes surface `EmitError` → tree-C
  fallback. `parallel.intent` runs end-to-end through SSA-C
  with the right outputs (100, 240000, 1, 10, 40, 0, 62, 40
  for the eight reductions).
- **SSA-C handles `task`/`join`** via outlined `static void*
  intent_task_<N>(void*)` functions emitted to a module-
  scope `TASK_OUTLINES` buffer (spliced between user-fn
  prototypes and bodies so task outlines can call into
  user-defined helpers). Single-block task bodies only —
  multi-block bodies surface `EmitError` → tree-C fallback.
  Spawn site uses `intent_thread_create` (POSIX
  `pthread_create` / Win32 `CreateThread` via the cross-
  platform wrapper); join uses `intent_thread_join` +
  `free` of the heap ctx. Cross-platform threading
  wrappers + the `intent_task_handle` typedef are now
  shared with tree-C via
  `backend_c::emit_intent_thread_wrappers_c`.
- **C runtime is Windows-portable:** `intent_thread_t` wrappers (`CreateThread` / `pthread_create`), mutex `WaitOnAddress` / futex arms; `intentc build` gates `-pthread` vs `-lsynchronization`.
- **LLVM runtime is Windows-portable** for both tree-LLVM and SSA-LLVM: `cfg!(target_os)`-gated threading declares + spawn-site (`@CreateThread` returning HANDLE, `ptrtoint`'d into the i64 handle slot) + join-site (`@WaitForSingleObject` + `@CloseHandle`) + mutex park (`@WaitOnAddress`) + Guard drop wake (`@WakeByAddressSingle`). `parallel for` open-codes a hardcoded-N=4 `@CreateThread` fan-out on Windows in lieu of libgomp; the outlined fn returns `i8*` (CreateThread's start-routine ABI) and reads `tid`/`nt` out of a per-thread `WinParArg { i8* ctx, i64 tid, i64 nt }` struct instead of calling `omp_get_*`. Helper `host_uses_win32_threading()` drives all the dispatch.

### Tooling
- `intentc check / emit / emit-c / run / build / test` subcommands. `--json` diagnostics for editors/CI.
- `intent-lsp` binary: didOpen/didChange/didClose, publishDiagnostics, hover, definition, references, rename, completion, code actions (single-char insert quickfix), semantic tokens (7-type legend, declaration/readonly modifiers).

---

## TODOs

The list below replaces the now-closed backend / verifier / Vec-of-non-Copy
queue. Canonical priority order — work top-down. Items 1–9 are explicit
language-surface gaps; items 10–11 are deferred multi-week build-outs.
Closed-out history lives in TODO.md.

### Design philosophy (read before reordering)

The compiler's reach should be **20 % of Rust/C++'s feature set covering
80 % of the programs an average programmer writes**, optimized for two
things above all else:

1. **Memory safety enforced at compile time.** No NULL dereferences, no
   use-after-free, no double-free, no data races, no resource leaks, no
   uninitialized reads. Errors that crash or corrupt at runtime in C++
   surface as type errors before the program ever runs.
2. **The programmer writes ownership once; the compiler schedules
   cleanup.** Affine ownership + automatic drop chains at scope exit
   mean the user declares WHAT owns a resource — the compiler decides
   WHEN to release it. No `free`, no `close`, no `delete`, no `try /
   finally` boilerplate in user code.

Concrete consequences that shape every choice below:

- **RAII is the only resource-management mechanism.** Affine types are
  auto-dropped at scope exit; struct fields drop in reverse-declaration
  order; user types opt into custom cleanup by implementing the `Drop`
  interface. The compiler already does this implicitly today for
  `Vec<T>`, `Mutex<T>`/`Guard<T>`, `OwnedStr`, and `Task` handles —
  Tier 1 promotes it from a built-in privilege to a user-extensible
  one without changing the underlying machinery.
- **No exceptions.** Hidden control flow breaks both the SMT verifier
  and the "memorize the language" goal. Failure is modelled with
  `Result<T, E>` + the `?` operator — every fallible call's failure
  edge is visible in the source.
- **Composition over inheritance.** Structs can hold other structs and
  implement multiple interfaces. There is no inheritance chain, no
  method override, no diamond resolution.
- **Generics WITHOUT trait bounds in v1.** Type parameters are opaque
  inside the function — only "move / pass / return" work generically.
  Anything that needs structural behaviour (compare, hash, equality,
  drop) goes through an **interface bound** (`<T: Cmp>`). This keeps
  the language tiny without losing collections.
- **Interfaces, not full traits.** No interface inheritance, no default
  methods, no associated types in v1. One layer of abstraction. `Drop`
  is the one interface the compiler treats specially (auto-invoked at
  scope exit).
- **Affine ownership is the only safety mechanism.** No GC, no
  reference-counting, no first-class lifetimes. Borrow-checking
  remains scoped to function parameters (`ref T` / `mut ref T`).

### Syntax conventions (keep keywords first; symbols only when math-universal)

Intent prefers spelled-out keywords over operator soup. The table
below is the canonical replacement list — every TODO item in the
tiers below uses it. The rule: a keyword wins unless the operator
form is also valid math notation (`<`, `==`, `&&`, `<T>` for type
parameters, `{ }` for blocks, etc.).

| Concept | Rust/C++ shape | Intent shape | Why |
| --- | --- | --- | --- |
| Borrow | `&x` | `ref x` | reads as "reference to" |
| Mutable borrow | `&mut x` | `mut ref x` | composes `mut` + `ref` |
| Path resolution | `Point::dist`, `Color::Red` | `Point.dist`, `Color.Red` | one operator for fields + namespace |
| Error propagation | `parse(s)?` | `try parse(s)` | explicit "try this" |
| Range (loop) | `for i in 0..10` | `for i from 0 to 10` | BASIC-style English |
| Range (slice) | `xs[lo..hi]` | `xs[lo to hi]` | same `to` |
| Return type | `fn foo() -> i64` | `fn foo() returns i64` | the arrow is jargon |
| Match arm | `Red => "red"` | `Red then "red"` | reads as "if Red then …" |
| Closure | `\|x\| x + 1` | `with x do x + 1` | no pipe-symbol |
| Method/field receiver | `&self`, `&mut self` | `self`, `mut self` | `self` is always borrowed; `mut` flips it |
| Interface decl | `trait Cmp` | `interface Cmp` | universally familiar |
| Interface impl | `impl Drop for File` | `implement Drop for File` | reads as a sentence |
| Methods block | `impl Point { … }` | `methods on Point { … }` | reads as a sentence |
| Generic bound | `fn min<T: Cmp>` | `fn min<T> where T is Cmp` | `where … is …` reads as English |

Things explicitly NOT replaced (math-universal symbols carry their
weight):

- `<T>` type parameters — replacing with `of T` makes `Map<K, V>`
  ambiguous.
- `:` type annotations (`let x: i64`) — Python / TypeScript shape,
  universally known.
- `==` `!=` `<` `<=` `>` `>=` `&&` `||` `!` — math/boolean basics.
- `{ }` blocks, `[ ]` indexing, `( )` grouping/call.

#### Retroactive updates to existing syntax

Today's source uses a few shapes that pre-date the keyword-first
convention. These get a single sweep when Tier 1 starts (or sooner
if surface is small enough to fold in). Tracked here so the rewrite
isn't accidentally piecemeal:

| Today's shape | New shape | Where it shows up |
| --- | --- | --- |
| `for i in 0..n` | `for i from 0 to n` | every `for`-counter loop |
| `for x in &xs` | `for x in ref xs` | borrowing iteration |
| `for x in &mut xs` | `for x in mut ref xs` | mutable iteration (when added) |
| `parallel for i in 0..n` | `parallel for i from 0 to n` | parallel loops |
| Param type `&T` / `&mut T` | `ref T` / `mut ref T` | fn signatures + ref expressions |
| Call site `foo(&x)` / `foo(&mut x)` | `foo(ref x)` / `foo(mut ref x)` | every borrow at a call |
| Range slice `xs[lo..hi]` | `xs[lo to hi]` | future slice ops (T3.10) |

Concurrency keywords that DON'T need rewriting (already keyword-shaped
and clear): `task`, `join`, `parallel`, `reduce <var> with <op>`,
`pure fn`, `requires`, `ensures`, `invariant`, `intent`, `use`, `as`,
`assert`, `prove`, `print`.

`with` is reused in two contexts: `reduce <var> with <op>` (operator
follows the keyword) and `with x do <body>` (identifier + `do`
follows). The parser disambiguates on the token immediately after
`with` — no genuine ambiguity, and keeping two separate keywords
would lose the reading "perform this reduction *with* the +
operator" / "evaluate this expression *with* x bound to …".

Concrete example combining the above — a struct with custom RAII,
a method block, a fallible constructor with `?`-propagation, an
interface implementation, and a generic function with a bound:

```intent
struct FileHandle { fd: i32, path: OwnedStr }

methods on FileHandle {
  fn open(path: Str) returns Result<FileHandle, Str> {
    let fd: i32 = try sys_open(path);
    return Ok(FileHandle { fd: fd, path: path });
  }

  fn read(self) returns Result<OwnedStr, Str> {
    return sys_read(self.fd);
  }
}

implement Drop for FileHandle {
  fn drop(mut self) {
    sys_close(self.fd);
  }
}

interface Cmp { fn cmp(self, other: ref Self) returns i64; }

fn min<T>(a: T, b: T) returns T where T is Cmp {
  if a.cmp(ref b) <= 0 { return a; } else { return b; }
}

enum Color { Red, Green, Blue }

fn describe(c: Color) returns Str {
  match c {
    Color.Red   then "warm",
    Color.Green then "cool",
    Color.Blue  then "cool",
  }
}

fn sum_each(xs: ref Vec<i64>) returns i64 {
  let total: i64 = 0;
  for x in ref xs { total = total + x; }
  return total;
}

fn main() returns i64 {
  let h: FileHandle = try FileHandle.open("/tmp/x.txt");
  let body: OwnedStr = try h.read();
  for i from 0 to 5 {
    print body;
  }
  return 0;
}
```

### Tier 0 — Syntax sweep (no new features; aligns existing source to the convention)

0. **Apply the keyword-first sweep** — *done 2026-05-20*. Lexer
   gained `Ref` / `From` / `To` keyword tokens. Parser rewired:
   type position accepts `ref T` / `mut ref T` (old `&T` / `&mut T`
   surfaces a guidance error pointing at the new keyword), unary
   borrow accepts `ref x` / `mut ref x` (likewise), for-loop range
   shape is now `for VAR from LO to HI` (old `for VAR in LO..HI`
   removed), for-iter borrow is `for VAR in ref XS`. Formatter
   updated to emit the new shapes for `Type::Ref` / `Type::RefMut`,
   `ExprKind::Ref` / `ExprKind::RefMut`, and both for-loop variants.
   `Type::Display` matches. All 27 example files swept, every test
   that pinned source text (4 lib tests + 2 SSA crosscheck files)
   migrated. Bitwise `&` / `|` / `^` operators remain available in
   binary position (reductions, expressions); `&` as a prefix borrow
   is rejected with a friendly hint. 456 lib + 47 e2e + 3
   integration tests green on the new syntax.

### Tier 1 — Composite types (foundation; rest of the list waits on this)

1. **Tuples** — *done 2026-05-20*. `(T1, T2, T3)` multi-return,
   light-weight grouping. v1 ships 2..=4 element tuples, Copy-only
   element types, destructure-only access. **AST:** new
   `Type::Tuple(Vec<Type>)`, `ExprKind::Tuple(Vec<Expr>)`,
   `ExprKind::TupleAccess { tuple, index }`, and a parse-only
   `Stmt::LetTuple { names, annotation, expr }` that the checker
   desugars. **IR:** `TypedExprKind::Tuple` + `TypedExprKind::TupleAccess`
   (no new `TypedStmt` variant — the checker lowers `LetTuple` to
   a sequence of `TypedStmt::Let`s reading from a synthetic temp).
   **Parser:** tuple type `(T1, …)` accepted in every type position;
   tuple expression `(e1, …)` disambiguated from grouped expression
   by the top-level comma; destructure-let
   `let (a, b) = expr;` produces `Stmt::LetTuple`. **Checker:**
   `check_expr` handles both new ExprKinds (enforces 2..=4 elements,
   Copy-only, in-bounds index); `check_one_stmt`'s LetTuple arm
   verifies arity matches names count, rejects duplicate names,
   and emits the desugared sequence. **Tree-C:** new
   `tuple_c_struct` / `emit_tuple_bundle` / `collect_tuple_shapes`
   pipeline emits per-shape `typedef struct { T1 _0; T2 _1; …; }
   intent_tuple_<tags>;` in the preamble before vec / array
   typedefs; tuple construction emits a designated-initializer
   compound literal; `.<index>` access emits `._<index>` field
   reads. **Tree-LLVM:** `llvm_type_string` returns the
   anonymous-struct literal `{ T1, T2, … }`; tuple construction
   emits an `insertvalue` chain; `TupleAccess` emits `extractvalue`;
   `is_scalar` includes Tuple so the existing scalar Let path's
   `alloca + store` shapes work uniformly. **SSA backends:** tuple
   lowering surfaces `LowerError` for now, routing programs
   through the tree-fallback. **Tests:** three new lib tests
   (`tuple_multi_return_and_destructure`,
   `tuple_arity_mismatch_rejected`,
   `tuple_non_copy_element_rejected`). 456 → 459 lib tests; 47
   e2e + 3 integration unchanged. **Follow-ups (later phases):**
   non-Copy elements (lifts the Copy gate; needs RAII drop chains
   like Vec phase 1), `.0` / `.1` direct field-access syntax in
   source (today's destructure-let desugars to that internally),
   nested tuples / tuples >4 elements, SSA backend support.
2. **Structs (records)** — *phase 1 done 2026-05-20*. Top-level
   `struct Point { x: i64, y: i64 }` decls, field-init literals
   `Point { x: 3, y: 4 }`, and field-access `p.x`. v1 caps at 1..=8
   fields, **Copy-only** fields, no `methods on` blocks yet, no
   RAII chains. Both backends emit a per-name struct typedef
   (C: `typedef struct { … } Struct_<Name>;`; LLVM: `%Struct_<Name>
   = type { … }`). New `TokenKind::Dot` enables the postfix
   `.<ident>` / `.<int>` access syntax (re-used for `t.0` tuple
   access from T1.1). Four new lib tests pin: working struct
   decl + literal + access, missing-field rejection, unknown-
   field rejection, non-Copy-field rejection. SSA backends fall
   back to tree via `LowerError` (parallel to tuples).
   **Phase 2a done 2026-05-20**: `methods on Point { fn
   dist(self: Point) -> i64 { … } }` block syntax +
   `p.dist()` method-call sugar work end-to-end. New
   `methods` lexer keyword; AST `MethodsBlock { for_type,
   methods }` on `Program`; new
   `ExprKind::MethodCall { receiver, method, args }` for
   the postfix `.<ident>(args)` shape (existing
   `.<ident>` stays as FieldAccess when no `(` follows so
   `p.x` still field-accesses). Parser disambiguates via
   lookahead on `(`. The checker hoists each method into
   the regular function table with mangled name
   `<TypeName>_<methodName>` (after enum + alias
   resolution so the type name is accurate), validates the
   methods-block target is a struct/enum, catches
   duplicate methods, and catches mangled-name collisions
   with existing functions. MethodCall expressions desugar
   at check time: the receiver's type yields the
   type-name, and the call becomes a regular
   `Call { name: "<T>_<method>", args: [receiver, …] }`
   consumed by the existing call-checking pipeline.
   **Auto-ref**: when the method's first param is
   `ref T` / `mut ref T` and the receiver is a plain
   value of `T`, the desugar wraps the receiver in
   `ExprKind::Ref` / `ExprKind::RefMut` so users can
   write `p.method()` whether the method binds `self`
   by value or by borrow — no manual `ref(p).method()`
   ceremony. **Field assignment** (`p.x = expr;` and
   `self.field = expr;` through a `mut ref T` receiver)
   now works end-to-end. New `Stmt::FieldAssign` +
   `TypedStmt::FieldAssign` carry the (object, field,
   value, through_mut_ref) shape; parser uses a
   `looks_like_field_assign` lookahead that walks
   `<ident>(.<ident>)+ =` and rejects `.<ident>(`
   patterns (which are method calls). The checker
   validates the place is an owned struct or a
   `mut ref` to one, requires the field name to exist,
   coerces the value type, and forbids field-assign
   through an immutable `ref`. Tree-C emits
   `obj.field = value;` / `obj->field = value;`;
   tree-LLVM emits the appropriate `getelementptr` +
   `store` (or `load` + `insertvalue` + `store` for
   owned structs). SSA path gates with a clear
   "field assignment is not yet supported"
   `LowerError` so it falls back to the tree backend.
   The effects-checker treats FieldAssign as a side
   effect so `parallel for` / `task` reject it
   correctly. Twelve new lib tests pin: basic method,
   method with extra args, missing-method rejection,
   primitive-receiver rejection, duplicate-method
   rejection, self-field access, auto-ref to `ref T`
   self, auto-ref to `mut ref T` self, owned-struct
   field-assign with `.x =` in emitted C, mut-ref
   field-assign via a counter-bump method, unknown-field
   rejection, immutable-ref field-assign rejection. Two new
   format round-trip tests pin the methods-block +
   method-call surface shapes. A new
   `examples/methods.intent` exercises four shapes
   (value-self, ref-self, ref-self reading consts,
   value-self returning a new instance) end-to-end
   and is included in the `intentc test` pass.
   **Nested affine struct fields + recursive Drop done
   2026-05-22**: `struct Outer { inner: Inner, … }` where
   Inner has heap fields now compiles. Both backends emit
   recursive Drop walks through nested struct types. The
   non-Copy registry uses fixed-point iteration so source
   order doesn't matter. Nested-path moves like
   `let v = o.inner.s;` are gated with a workaround hint
   (move the inner struct out first) — full path-level
   move tracking is deferred. See
   [examples/nested_struct_drop.intent](examples/nested_struct_drop.intent).

   **Mutex / Channel enum payloads done 2026-05-22**:
   `enum Locked { Held(Mutex<i64>), Free }` and analog
   for Channel now compile. Symmetric to closure #123's
   struct-field work — Mutex/Channel are inline layouts
   with no Drop concern; gate-lift + LLVM
   `zeroinitializer` extension cover everything.

   **Mutex / Channel struct fields done 2026-05-22**:
   `struct State { m: Mutex<i64> }` and the analog for
   `Channel<T, N>` now compile. Combined with the
   field-borrow work from closure #102,
   `mutex_lock(ref s.m)` flows cleanly. Both are inline
   struct layouts; per-field Drop is a no-op. Only
   `Guard<T>` remains rejected as a struct field — its
   RAII unlock is bespoke and needs more wiring.

   **Task + Atomic enum payloads done 2026-05-22**: closes
   the originally-listed affine enum payload types. Only
   Mutex / Guard / Channel payloads remain rejected. Both
   Task and Atomic have no Drop story (Task drops via
   join; Atomic is a primitive cell). LLVM
   `llvm_type_string` gained a Task arm
   (`%intent_task_handle`); the payload-less `zeroinitializer`
   list extended to include Task.

   **Const initializer arithmetic done 2026-05-22**:
   `const B: i64 = A + 1;` (and `*`, `-`, `/`, `%`) folds
   over previously-declared integer consts. Checker
   `literal_const_value` and parser `expr_as_int_literal`
   both walk Var / Binary nodes recursively with checked
   arithmetic. The resolved value flows into the `[T; N]`
   array-length resolver from closure #120 too.

   **`const N` as array length done 2026-05-22**: users
   can now declare `const SIZE: i64 = 8;` and reference
   SIZE in array types (`[i64; SIZE]`) across let
   annotations, fn params, struct fields, and array
   literals. The parser stashes integer-literal const
   values during `parse_const_decl` and resolves them in
   the array-length slot at parse time. Forward references
   and non-literal const initializers still error cleanly.

   **`[T; N]` enum payload done 2026-05-22**: arrays of
   Copy elements are now valid as enum payloads. No Drop
   needed (stack lifetime). C-side uses an inline `T name[N]`
   declarator (via `format_declarator`) and a bare-brace
   `{e1, e2, …}` initializer for the variant constructor.
   LLVM `zeroinitializer` extended to cover Array payloads
   for payload-less variants. See
   [examples/enum_arr_payload.intent](examples/enum_arr_payload.intent).

   **Vec<T> enum payload done 2026-05-22**: extends #113's
   OwnedStr work — enum variants can now hold `Vec<T>`
   payloads. Both backends emit a tag-conditional
   `intent_vec_<T>__free` at scope exit. C ordering pre-
   pass walks enum payload types alongside struct fields
   so the Vec typedef lands before the enum typedef. LLVM
   uses `zeroinitializer` (C uses `{0}`) for payload-less
   variants of aggregate-payloaded enums. See
   [examples/enum_vec_payload.intent](examples/enum_vec_payload.intent).

   **SSA bool-print parity done 2026-05-22**: bool prints
   through both SSA backends now render as "true"/"false"
   instead of "1"/"0". SSA-C uses `fputs(v ? "true" :
   "false", stdout)`; SSA-LLVM uses `select i1` over two
   private string globals + `printf("%s", …)`.

   **Empty struct + bare-block scope-stmt done 2026-05-22**:
   `struct E {}` is now accepted for marker/zero-sized
   types; struct-lit lookahead handles `Type {}`. Bare
   `{ stmts; }` as a free-standing statement desugars to
   `if true { stmts; }` at parse time, providing explicit
   scoping for nested bindings.

   **Unit-return functions done 2026-05-22**: `fn f() { … }`
   without `-> Type` is parser sugar for `-> i64` with an
   implicit `return 0;` appended. Callers invoke as bare
   statement or via `let _ = f();`. Idempotent synthesis —
   if the user already ends with `return`, no extra is added.
   See [examples/unit_return.intent](examples/unit_return.intent).

   **Type-associated functions done 2026-05-22**:
   `methods on T { fn helper(args) -> R { … } }` (without
   `self`) declares a type-associated function, callable
   as `T.helper(args)`. The checker hoists it to the same
   `<T>_<method>` mangled name; the MethodCall handler
   recognizes a Var receiver naming a struct/enum and
   dispatches directly to that mangled function (no
   self-receiver prefix). Co-exists with regular
   `recv.method()` dispatch in the same block. See
   [examples/type_associated_fn.intent](examples/type_associated_fn.intent).

   **Enum payloads admit OwnedStr done 2026-05-22**: enums
   like `enum Maybe { Some(OwnedStr), None }` are now valid
   in v1. The aggregate is affine; both backends emit a
   tag-conditional `free` at scope exit, only for variants
   that carry a heap payload. New `ENUM_NON_COPY_REGISTRY`
   in [src/ast.rs](src/ast.rs) parallels the struct one;
   `LLVM_/(C)_ENUM_PAYLOAD_TAGS_REGISTRY` thread-locals
   drive the per-variant dispatch. v1 limitation: matching
   without a binding only — destructure-binding patterns
   for non-Copy payloads are rejected (alias-vs-Drop
   tracking is deferred). See
   [examples/enum_owned_payload.intent](examples/enum_owned_payload.intent).

   **Deep field paths for `xs[i].a.b = v` done 2026-05-22**:
   the depth gate in the checker was lifted; the existing
   loop already validates each segment with per-step type
   descent and a Copy check on intermediates and the leaf.
   Backends already iterated over segments (closure #109).
   See updated
   [examples/mixed_place_assign.intent](examples/mixed_place_assign.intent).

   **Mixed-place leaf-OwnedStr / leaf-Vec done 2026-05-22**:
   the Copy gate on the leaf segment of `xs[i].a.b = v` was
   relaxed for heap-shaped types (`OwnedStr` and `Vec<T>`).
   Intermediate segments still require Copy. Both backends
   emit a free of the old slot before storing the new value:
   the C backend writes `free((void*)<lv>)` (OwnedStr) or
   `intent_vec_<T>__free(<lv>)` (Vec); LLVM loads the old
   pointer and calls `@free` or `@intent_vec_<tag>__free`.
   Closure #126 / F2. See updated
   [examples/mixed_place_assign.intent](examples/mixed_place_assign.intent).

   **LLVM Vec `__set` frees old element done 2026-05-23**:
   The per-shape Vec `__set` helper in LLVM only freed
   the old slot for `Type::Vec(inner)` element types —
   `set(Vec<OwnedStr>, …)`, `set(Vec<Struct{heap}>, …)`
   and `set(Vec<Enum{OwnedStr}>, …)` leaked the previous
   slot's heap. Closure #127 had extended the analogous
   tree-C `c_element_drop_old`; this closes the LLVM
   parallel by adding `Type::OwnedStr` (direct `@free`),
   `Type::Struct` (per-field `emit_vec_element_struct_drop`)
   and `Type::Enum` (extract tag/payload, OR-chain over
   payloaded tags, branch to free vs done block) arms.
   Closure #157.

   **SSA-LLVM gates out Vec<Atomic/Channel> → tree-LLVM done 2026-05-25**:
   SSA-LLVM represents `Atomic<T>` as the alloca *pointer*
   (so subsequent `&counter` references reuse the same
   address), and Channel similarly indirects through the
   struct. Storing a pointer-shaped SSA value into an
   `i32` Vec slot emitted `store i32 %ptr, …` which
   failed the LLVM IR verifier with a type mismatch (the
   element width is the underlying scalar `i32`, not
   `i32*`). Tree-LLVM doesn't have this issue — it goes
   through a different vec emit path. Closure #212 adds
   `stmt_uses_vec_of_atomic_or_channel` to
   `ssa_llvm_extra_reject` so any program containing
   `Vec<Atomic<T>>` / `Vec<Channel<T,N>>` (at any nesting
   depth) routes through tree-LLVM. SSA-C unaffected;
   its emit already handles these correctly. Test
   totals: 904 lib + 47 e2e passing. Closure #212.

   **tree-C Vec<Atomic/Channel> typedef collision done 2026-05-25**:
   `Vec<Atomic<T>>` element-tag fell through to
   `c_leaf_type(Atomic).replace(' ', '_')` which returned
   the hardcoded `_Atomic int64_t` regardless of T →
   typedef `intent_vec__Atomic_int64_t` for ANY
   Vec<Atomic<T>>. Two distinct `Vec<Atomic<T>>` in the
   same program collapsed to one typedef whose `data`
   field had the FIRST T's width → ASan stack-buffer-
   overflow on memcpy when widths differed (e.g. u32 vs
   u8). Same shape for `Vec<Channel<T, N>>` (different
   (T, N) would collide). Fix: add
   `Type::Atomic(element)` and `Type::Channel(element,
   capacity)` arms to `element_tag` so distinct shapes
   get distinct typedef names like
   `intent_vec_atomic_uint32_t` and
   `intent_vec_channel_int64_t_4`. Test totals: 904 lib
   + 47 e2e passing. Closure #211.

   **tree-C RefField const-strip for Mutex/Atomic/Channel done 2026-05-25**:
   When borrowing a struct via `ref T` and then field-
   borrowing a Mutex/Atomic/Channel field (`ref c.lock`),
   the C lowering took the address through a `const T*`
   pointer → `const Mutex*` operand. The runtime helper
   `intent_mutex_i64_lock` (and Atomic/Channel ops) takes
   a non-const pointer — atomic-style ops are inherently
   mutating even via a read-only borrow. gcc warned
   `-Wdiscarded-qualifiers`. Closure #176 already handled
   direct `ref Mutex/Channel/Atomic` params; #210 covers
   field-borrow through a `ref Struct`. Tree-C now emits
   `(intent_mutex_i64*)&v_c->lock` with an explicit const-
   strip cast. -Wdiscarded-qualifiers sweep over all 58
   examples clean. Test totals: 903 lib + 47 e2e passing.
   Closure #210.

   **tree-C Atomic<T> struct field element width fix done 2026-05-25**:
   Parallel to #208 for `Atomic<T>` as a struct field.
   The c_leaf_type fallback returned `_Atomic int64_t`
   for any Atomic; an `Atomic<u32>` field declared at
   i64 width would have wrong memory size / alignment /
   lock-free properties vs the declared type
   (functionally tolerated at runtime via implicit
   conversion, but type-incorrect). Fix: add an
   `Atomic(element)` arm to `c_element_storage` that
   calls `c_atomic_storage(element)` → `_Atomic
   <c_leaf_type(element)>`. Test totals: 902 lib + 47
   e2e passing. Closure #209.

   **tree-C Channel<T,N> struct field capacity fix done 2026-05-25**:
   `Channel<T, N>` as a struct field emitted with the
   hardcoded fallback type `intent_channel_int64_t_16`
   because `c_element_storage` fell through to
   `c_leaf_type` for Channel, and `c_leaf_type(Channel)`
   returns the 16-capacity fallback (the comment there
   explicitly notes callers must special-case Channel).
   A field of `Channel<i64, 4>` therefore didn't match
   the constructor's `intent_channel_int64_t_4_new()`
   return type and cc rejected with "incompatible types".
   Same shape for non-i64 Channel element types. Fix:
   add a `Channel(elt, cap)` arm to `c_element_storage`
   that calls `c_channel_storage(elt, cap)`. Test
   totals: 901 lib + 47 e2e passing. Closure #208.

   **tree-C Block-expr user-Drop for Copy structs done 2026-05-25**:
   tree-C's Block-expr Drop emit (inline arm for non-
   stmt-level Drops) had a Struct branch that walked
   per-field free chains but never checked
   USER_DROP_REGISTRY. For a Copy-but-user-Drop struct
   (e.g. `Resource { id: i64 }` plus `implement Drop`),
   the per-field walk emitted nothing and the user's
   drop method was silently skipped at Block-expr scope
   exit. The regular stmt-level Struct Drop handler at
   backend_c.rs:1965-1987 already had the user-Drop
   check; the Block-expr inline arm needed the same.
   Tree-LLVM unaffected (its Block-expr emit forwards
   Drop to emit_stmt which already handles user-Drop).
   Test totals: 900 lib + 47 e2e passing. Closure #207.

   **SSA-C parallel-for post-loop counter uses end-bound done 2026-05-25**:
   Per OpenMP, the iteration variable inside `omp parallel
   for` is implicitly private — reading its value AFTER
   the loop is undefined. SSA-C's
   `emit_parallel_for_region` propagated header→exit
   block-args literally, so a Phi capturing the post-loop
   counter value rendered as `v_3 = v_2` where v_2 is the
   (now-undefined) counter. gcc warned `v_2 is used
   uninitialized`. Fix: substitute the counter operand
   with the loop's `end` operand when emitting exit-arg
   assignments — the well-defined post-loop value is
   exactly the loop bound (parallel-for forbids `break`
   per closure #190). Test totals: 899 lib + 47 e2e
   passing. Closure #206.

   **tree-C match-on-bool cast for switch done 2026-05-25**:
   gcc warns `switch condition has boolean value`
   (-Wswitch-bool) when the dispatch expression is
   bool-typed. Tree-C's Match emit passed the bool
   scrutinee directly to `switch(…)` with `case 0` /
   `case 1` arms. Fix: cast bool scrutinees to int
   (`switch ((int)v_b)`) so the canonical 0/1 dispatch
   is unambiguous. Test totals: 898 lib + 47 e2e
   passing. Closure #205.

   **SSA-C omits unused block labels done 2026-05-25**:
   SSA-C emitted a `bbN:` label for EVERY block,
   including the entry block of a straight-line fn that
   no `goto` ever targets. gcc warned "label 'bb0'
   defined but not used" (-Wunused-label) and the noise
   hid real diagnostics. Fix: pre-scan all terminators
   (Jump, Branch) plus special-region targets (parallel-
   for exit, multi-block task end) to build a
   `referenced_blocks` set, then emit a label only for
   blocks in that set. -Wunused-label sweep over all 58
   examples now clean. Test totals: 897 lib + 47 e2e
   passing. Closure #204.

   **tree-C array-payload no-variant brace-init done 2026-05-25**:
   `.payload = 0` for an enum whose payload is an array
   type (e.g. `Window.Closed` when `Window` carries an
   `[i64; 4]` payload) was tripping `-Wmissing-braces`
   and is technically ill-formed C — an array can't be
   initialized from a bare integer (gcc accepts via the
   zero-fill extension; stricter compilers reject).
   Tree-C's payload-less variant emit had brace-init for
   Vec/Tuple/Struct but not Array. Added Array to the
   brace-init list — emits `.payload = {0}`. Test
   totals: 896 lib + 47 e2e passing. Closure #203.

   **SSA-C empty-param prototype `(void)` done 2026-05-25**:
   SSA-C emitted `fn_main()` with empty parens for no-
   arg functions. In C, empty parens mean "unspecified
   prototype" (K&R style), not "no args" — tripping
   `-Wstrict-prototypes` and breaking -Werror builds.
   Both `emit_function_prototype` and `emit_function`
   now write `(void)` when `f.params` is empty,
   mirroring what tree-C's `emit_params` already did.
   `-Wstrict-prototypes` sweep over all 58 examples now
   clean. Test totals: 895 lib + 47 e2e passing.
   Closure #202.

   **Block-expr Let RHS move tracking done 2026-05-25**:
   The Block-expr `Stmt::Let` arm (closure #129 MVP)
   never called `consume_if_moved_var(rhs, …)`, so
   `let n = b.name` (partial-move) or `let n = outer_var`
   (Var move) inside a Block-expr didn't propagate the
   move into the env. The struct's per-field free / outer
   Var's scope-exit free then double-freed the heap that
   the moved-out binding's drop ALSO freed — ASan ABORT.
   Fix: mirror the regular fn-body Let arm — call
   `consume_if_moved_var(rhs, &rhs_checked, env)` then
   `inject_branch_drops(&mut rhs_checked.expr)`. Test
   totals: 894 lib + 47 e2e passing. Closure #201.

   **Block-expr `let _ = …` Discard handling done 2026-05-25**:
   The Block-expr `check_expr` arm always called
   `env.insert_current(name)` and emitted
   `TypedStmt::Let`. For `name == "_"`, two consecutive
   discards collided on the synthetic name (`v__`
   redefined) and the fresh OwnedStr/Vec result leaked
   because Discard wasn't on the Block emit's accepted
   arm list. Fix: detect `name == "_"` in the Block-expr
   Let arm and emit `TypedStmt::Discard { expr }` —
   mirrors the regular fn-body Let path (closure #134).
   Tree-C Block emit grew a Discard arm covering
   OwnedStr/Vec/Struct/Enum with brace-scoped tmps;
   tree-LLVM Block emit now forwards Discard to
   `emit_stmt` alongside Print/Drop. Test totals: 893
   lib + 47 e2e passing. Closure #200.

   **Block-expr shadow-name false-move done 2026-05-25**:
   When the OUTER `consume_if_moved_var` walked into a
   Block-expr's tail (closure #174), the inner scope had
   already been popped. `env.lookup_mut(name)` then walked
   past the gone inner shadow and marked an outer-scope
   binding of the same name as moved — surfacing a
   spurious "value 'a' was moved" diagnostic on
   subsequent uses of the outer `a`. Closure #194's
   inner `consume_if_moved_var` already marked the inner
   binding before pop_scope; closure #199 plugs the
   outer recursion: skip the recursion when the Block's
   tail is a `Var(name)` and the same Block declares a
   `Let` with that name. Test totals: 892 lib + 47 e2e
   passing. Closure #199.

   **tree-C tuple-shape collection in control flow done 2026-05-25**:
   `collect_tuple_shapes_in_expr` handled Tuple/
   TupleAccess/Unary/Binary/Call/ArrayLit/Cast/Index/
   Len/CallIndirect but fell through `_ => {}` for
   Block/IfExpr/Match. A tuple type that only appeared
   inside a Block-expr inner Let (`let p: (i64, i64) =
   (1, 2)`) never had its `intent_tuple_<…>` typedef
   emitted and cc rejected with `unknown type name
   intent_tuple_<…>`. The Vec walker already handled
   Block/IfExpr/Match arms (closure #129); the tuple
   walker was the outlier. Mirrored the same three
   arms. Test totals: 891 lib + 47 e2e passing.
   Closure #198.

   **Block-expr inner type-alias resolution done 2026-05-25**:
   Parallel to closure #196 for the type-alias
   substitution pass. `sub_aliases_in_stmt` had the same
   pre-existing limitation — it never descended into a
   Stmt's `expr` field, so any `let p: AliasName = …;`
   inside a Block-expr kept the unsubstituted alias and
   the checker rejected with the unresolved-name
   diagnostic. New `sub_aliases_in_expr` walks every
   expression shape and recurses through nested Lets,
   mirroring the #196 enum walker. Test totals: 890 lib
   + 47 e2e passing. Closure #197.

   **Block-expr inner enum-let annotation resolution done 2026-05-24**:
   `resolve_enum_types_in_stmt` walked top-level fn
   bodies and the bodies of `if`/`while`/`for`/`for-iter`
   /task — but never descended into a Stmt's `expr`
   field, so any Let inside a Block-expr (e.g. `let r =
   { let a: Maybe = …; … }`) kept its annotation as
   `Type::Struct("Maybe")` instead of being resolved to
   `Type::Enum("Maybe")`. `coerce_checked` then got
   actual=Type::Enum, target=Type::Struct, both rendered
   as "Maybe", and rejected with "let initializer must
   be assignable to Maybe, got Maybe" — a confusing
   identical-text diagnostic. Fix: extend
   `resolve_enum_types_in_stmt` to call a new
   `resolve_enum_types_in_expr` for every expression
   field, and have the expr walker descend into Block,
   IfExpr, Match, Cast, Binary, Call, Tuple, StructLit,
   FieldAccess, Try, etc. Test totals: 889 lib + 47
   e2e passing. Closure #196.

   **inject_branch_drops skips inner Block decls done 2026-05-24**:
   With closure #194's tail-spill landing `let
   __block_tail_<span> = …` inside each Block-expr,
   the `if-expr cond { Block1 } else { Block2 }` shape
   broke at codegen because
   `collect_branch_var_leaves` was treating the inner
   spill Var as a "leaf" of the branch and
   inject_branch_drops then emitted `Drop
   __block_tail_<span>` in the OTHER branch — where
   that name isn't declared. cc rejected with
   `undeclared identifier v___block_tail_<n>`. Fix in
   `collect_branch_var_leaves`: when descending into
   `Block { stmts, tail }`, filter out any Var name
   that a `Let` inside the same Block introduces. The
   filter is symmetric for spill Vars (synthetic) and
   user-declared inner Vars. Test totals: 888 lib +
   47 e2e passing. Closure #195.

   **Block-expr sibling-let scope-exit drops done 2026-05-24**:
   `let r = { let a = …; let b = …; a };` was leaking
   b's heap. The Block-expr type-checker pushed and
   popped a scope but never called
   `emit_current_scope_drops`, so sibling lets that the
   tail neither consumed nor moved were never freed.
   Fix in `check_expr` for `ExprKind::Block`: after the
   tail is checked, call `consume_if_moved_var(tail,
   …)` to propagate tail-Var moves into the inner
   scope, then synthesize Drop stmts for the remaining
   non-moved non-Copy bindings. When drops exist, spill
   the tail into a synthetic `__block_tail_<span>` Let
   so the Drops fire AFTER the tail evaluates (avoids
   UAF for tails that borrow a sibling, e.g.
   `{ let a = …; len(a) }`). When the tail already
   consumes every sibling (binary concat, fn args), the
   drops list is empty and no spill is emitted. Both
   tree-C and tree-LLVM benefit since the Block emit
   was already wired for Drop stmts (closures #160,
   #192, #193). Test totals: 887 lib + 47 e2e passing.
   Closure #194.

   **tree-C Block Drop Enum: tag switch + payload free done 2026-05-24**:
   Parallel to closure #192's Struct arm. Block-expr
   Drop for a payloaded enum needed to switch on the
   active tag and free the heap payload (OwnedStr /
   Vec). Inject_branch_drops's branch-wrap left enum-
   typed Vars in the unchosen branch with their
   payload heap leaked. Added the Enum arm: emits a
   `switch (v_name.tag) { case T1: free_call; break;
   default: break; }` form (same shape as the Reassign
   Enum drop in closure #147). Test totals: 885 lib +
   47 e2e passing. Closure #193.

   **tree-C Block Drop Struct emits field chain done 2026-05-24**:
   tree-C's Block-expression emit (used by
   inject_branch_drops's branch-wrap from closures #179
   / #180) handled `Drop OwnedStr` and `Drop Vec` arms
   but fell through `_ => {}` for `Drop Struct` — leaking
   the unchosen branch's heap on if-expr / match Var
   branches with struct types. Added the Struct arm:
   walks the STRUCT_FIELDS_REGISTRY and emits the per-
   field free chain (mirrors `emit_struct_field_drops`
   used by Stmt::Drop). Test totals: 884 lib + 47 e2e
   passing. Closure #192.

   **Task body_blocks via CFG reachability done 2026-05-24**:
   Task body containing a for-loop with `continue` was
   failing SSA-C and SSA-LLVM emit. The task region
   collection used `(begin_id..=end_id)` for body_blocks
   — a contiguous range. Closures #185 / #187 step
   blocks plus if-then/else/merge blocks created later
   in the same body got BlockIds beyond end_block, so
   they fell outside the range. Parent (fn_main) emitted
   them with goto-targets pointing at skipped step
   blocks → undefined-label errors. Fix: walk the CFG
   from begin_block, collecting all reachable blocks
   without following end_block's successors. Mirrored
   in both SSA-C and SSA-LLVM. Test totals: 883 lib +
   47 e2e passing. Closure #191.

   **Parallel-for body rejects break done 2026-05-24**:
   `break` inside a `parallel for` body must be rejected
   — OpenMP's `parallel for` pragma forbids early exit
   from worker iterations. The C backend forwarded
   `break;` directly into the `_Pragma("omp parallel
   for")` loop, and gcc/clang rejected with "break
   statement used with OpenMP for loop". Tree-LLVM
   accepted it with ambiguous cross-thread semantics.
   Checker now diagnoses break inside a parallel-for
   body with a clear message pointing at the
   Mutex<bool>-guarded flag workaround. `continue` is
   still allowed (OpenMP accepts it; the #185-#189 fixes
   ensure correct increment). Test totals: 882 lib + 47
   e2e passing. Closure #190.

   **tree-LLVM parallel-for outlined fn continue done 2026-05-24**:
   tree-LLVM's outlined parallel-for (`@__intent_par_<N>`
   invoked via @GOMP_parallel / CreateThread) didn't push
   a LoopFrame onto its FnCtx — `continue` inside the
   body fell through to the "outside a loop" no-op
   branch, then continued past the if-merge into the rest
   of the loop body. `total = total + 1` ran on every
   iteration regardless of the continue, breaking
   reduction totals. Pre-existing bug; SSA-LLVM falls
   back to tree-LLVM for multi-block parallel-for bodies
   so the LLVM run hit this path. Fix mirrors closures
   #185–#188: push a LoopFrame with header=step, emit a
   step block that loads-bumps-stores i_addr then jumps
   to hdr. Both natural body-end and `continue` jump to
   step. Test totals: 881 lib + 47 e2e passing. Closure
   #189.

   **tree-LLVM for-range continue emits step block done 2026-05-24**:
   tree-LLVM's `TypedStmt::For` (range form) had the same
   continue-infinite-loop bug as the for-iter form
   (#186) and SSA paths (#185, #187). `continue` jumped
   straight to for_header with i_addr unchanged →
   infinite loop. Pre-existing bug since tree-LLVM range-
   for shipped. Now uses a `for_step` block between body-
   end and header for the increment; both continue and
   natural fallthrough jump to step. Test totals: 880
   lib + 47 e2e passing. Closure #188.

   **SSA for-range continue + parallel-for shape done 2026-05-24**:
   `for i from start to end` (range form, lowered via
   `lower_integer_for`) had the same continue-infinite-
   loop bug as the for-iter form fixed in #185.
   Restructured with the same `step` block shape: step
   bumps the counter, jumps to header. Body's natural
   end and `continue` both jump to step. LoopFrame's
   header is step (the continue target).
   `ParallelForShape` grew a `step_block` field so the
   SSA-C / SSA-LLVM parallel-for recognizers can absorb
   step into the OpenMP / outlined-fn region — they now
   skip step alongside header / body. Without this
   update, the C / LLVM emit referenced step as a
   free-standing basic block with `goto bb_step;` that
   no other block defined. Test totals: 879 lib + 47
   e2e passing. Closure #187.

   **tree-LLVM for-iter continue emits step block done 2026-05-24**:
   tree-LLVM had the same continue-infinite-loop bug as
   SSA (closed in #185). `continue` jumped straight to
   iter_header with i_addr unchanged → infinite loop.
   Pre-existing bug since tree-LLVM for-iter shipped.
   Fix mirrors #185: introduce `iter_step` block that
   bumps i_addr then jumps to header. LoopFrame's header
   points to step (continue target). Body's natural end
   also jumps to step. Tree-C is unaffected (uses C's
   native `for (i = 0; i < len; i++)` form). Test totals:
   878 lib + 47 e2e passing. Closure #186.

   **SSA for-iter continue increments counter done 2026-05-24**:
   `continue` inside an SSA for-iter was jumping straight
   to the header block with the OLD i_header value — the
   increment only happened on the natural body-
   fallthrough path. Every `continue` re-entered the
   same iteration → infinite loop (hang at runtime).
   Pre-existing bug from when SSA for-iter was added.
   Fix: introduce a `step` block between body-end and
   header. Step takes the carry params, increments idx,
   then jumps to header. Both the natural fallthrough
   and `continue` jump to step (with the OLD i_header)
   so the increment fires uniformly. LoopFrame.header
   now points to step (the continue target), with the
   step's carry params replacing the header's. Test
   totals: 877 lib + 47 e2e passing. Closure #185.

   **SSA consuming for-iter emits buffer Drop on normal exit done 2026-05-24**:
   `for x in xs` (consuming form, Vec of Copy elements)
   flowing through SSA wasn't emitting any Drop for the
   consumed buffer — the checker marks the source as
   moved and SSA's lower_for_iter ignored the consumes
   flag. On normal loop completion the outer buffer
   leaked. SSA's gate already routes Vec<non-Copy> to
   tree backends (closure #159), so SSA only sees
   Vec<Copy>; `intent_vec_<T>__free` IS the shallow free
   for Copy elements. Emit an InstrKind::Drop at the
   loop's exit block. Test totals: 876 lib + 47 e2e
   passing. Closure #184. Known remaining: early
   `return` from inside the body still skips this Drop
   (tracked in STATUS.md known-issues).

   **`is_fresh_owned_str` refined for if-expr Var branches done 2026-05-24**:
   `print if cond { a } else { b };` (a, b: OwnedStr
   Vars) was double-freeing. `is_fresh_owned_str` /
   `is_fresh_non_copy` used a kind-only whitelist that
   returned true for any IfExpr / Match / Block,
   regardless of contents. Print's "free fresh result
   after use" path then freed the Var's heap; scope-exit
   freed it again. Refined to recurse into branches: an
   if-expr / match / block is fresh only when EVERY
   leaf is a fresh non-Copy producer (Call or Binary).
   Var leaves disqualify. Test totals: 875 lib + 47
   e2e passing. Closure #183.

   **inject_branch_drops at push/set xs arg done 2026-05-24**:
   `push(if cond { xs1 } else { xs2 }, v)` and `set(if
   cond { xs1 } else { xs2 }, i, v)` were leaking the
   unchosen Vec. The builtin handlers had wired
   inject_branch_drops into the value arg (closure #180)
   but not the Vec arg. Symmetric fix. Test totals: 874
   lib + 47 e2e passing. Closure #182.

   **inject_branch_drops at Return stmt done 2026-05-24**:
   `return if cond { a } else { b };` (a, b non-Copy
   Vars) was still leaking. inject_branch_drops was
   wired into Let / Reassign / Index / Field / Call /
   Method / vec / push / set / enum payload (#179, #180)
   but the Return-stmt arm was missed. One-line addition
   right before `try_elide_bounds_in_typed_expr` on the
   return expression. Test totals: 873 lib + 47 e2e
   passing. Closure #181.

   **inject_branch_drops at remaining consume sites done 2026-05-24**:
   Extends closure #179's structural-rewrite to the rest
   of the consume_if_moved_var sites: named-function
   Call args, MethodCall args (via Type-associated and
   `obj.method()` paths), StructLit field values,
   EnumVariantWithPayload constructor arg, `vec(…)`
   element args, `push()` value, and `set()` value.
   Same wrap-each-branch-in-Block-with-Drops pattern as
   #179. Each site now adds the inject after consume,
   so `f(if cond { a } else { b })` and similar shapes
   no longer leak the unchosen alternative. Test
   totals: 872 lib + 47 e2e passing. Closure #180.

   **If-expr / match Var-branch unchosen leak fixed done 2026-05-24**:
   Closes the unchosen-alternative leak left behind by
   the conservative move-tracking from closures
   #172/#173. The checker now rewrites if-expr / match /
   block-tail typed expressions so each branch wraps its
   chosen value in a Block that drops the OTHER
   branches' Var leaves before yielding. C ternary form:
   `cond ? ({ free(v_b); v_a; }) : ({ free(v_a); v_b; })`.
   LLVM emits the equivalent through the Block emitter
   (closure #160 already wired Block Drop forwarding).
   The rewrite is wired into Let, Reassign, IndexAssign,
   and FieldAssign — the most common move contexts.
   inject_branch_drops walks IfExpr, Match, and Block
   recursively so nested patterns work too. Test totals:
   871 lib + 47 e2e passing. Closure #179.

   **`Enum.Some(v)` consumes Var payload done 2026-05-24**:
   `Maybe.Some(n)` where n is a Var of OwnedStr was
   double-freeing on scope exit. The
   EnumVariantWithPayload constructor transfers
   ownership of the payload into the tagged-union, but
   `check_call`'s enum-constructor branch never called
   `consume_if_moved_var` on the payload arg. Source
   Var's scope-exit Drop fired AFTER the constructor
   stored the payload pointer, and the enum's drop
   re-freed the same heap. Same family as vec / push /
   set (#171, #177). One-line addition. Both backends
   were affected (checker/IR-level bug). Test totals:
   870 lib + 47 e2e passing. Closure #178.

   **`vec(a, b, …)` consumes Var element args done 2026-05-24**:
   `let xs: Vec<OwnedStr> = vec(a, b);` (a, b: Var
   OwnedStr) was double-freeing on scope exit. The
   vec() builtin transfers each Var's heap into the
   new Vec's slot, but `check_vec_builtin` never called
   `consume_if_moved_var` on its element args — the
   source Var's scope-exit Drop fired AFTER vec()
   already stored the pointer, and the Vec's __free
   re-freed each slot. Same family as push / set
   (closure #171); one-line addition in the
   element-coerce loop. Both backends were affected
   (checker/IR-level bug). Test totals: 869 lib + 47
   e2e passing. Closure #177.

   **SSA-C `ref Channel<T,N>` param drops `const` done 2026-05-24**:
   `fn produce(ch: ref Channel<i64, 16>, v: i64)` was
   declared as `const intent_channel_int64_t_16*`. The
   shared `intent_channel_*_send` / `_recv` helpers take
   a NON-const pointer (they bump seq counters and idx
   atomically through the cell pointer). Every send /
   recv site raised -Wdiscarded-qualifiers. Atomic refs
   already dropped `const` (the closest analogue);
   Channel now mirrors that. Caught via `cc -Wall -Wextra
   -c` on the concurrency example. Test totals: 868 lib
   + 47 e2e passing. Closure #176.

   **SSA-C `OwnedStr` declared `char*` not `const char*` done 2026-05-24**:
   SSA-C lumped `Str` and `OwnedStr` together as `const
   char*`. The shared Vec helper bundle declares the
   data field as `char* data`, so storing an OwnedStr
   value into a slot raised -Wdiscarded-qualifiers on
   every IndexAssign / Reassign / push / set. Runtime
   was fine (const is purely a compile-time tag) but
   the noise hid real warnings. Split: `Str` keeps
   `const char*` (borrowed read-only), `OwnedStr`
   becomes `char*` (heap-owning, mutable — matches
   tree-C). Test totals: 867 lib + 47 e2e passing.
   Closure #175.

   **Block-expr Var tail consumes source Var done 2026-05-24**:
   `let b = { let _x = 1; a };` (a: OwnedStr Var) was
   double-freeing on scope exit. The Block's tail
   expression yields a's value into b, so b aliases a's
   heap; both Vars' scope-exit drops then fire. Same
   shape as closures #172/#173 — `consume_if_moved_var`
   covered Var, FieldAccess, IfExpr, and Match but
   Block fell through. Now the tail is recursively
   consumed. Closure #174. Test totals: 866 lib + 47
   e2e passing.

   **Match arms returning Var consume all arms done 2026-05-24**:
   Same shape as closure #172 but for match scrutinees
   that stay as `TypedExprKind::Match` (integer / enum /
   bool). `let chosen = match n { 1 then a, 2 then b, _
   then c };` was double-freeing because the codegen
   switch makes v_chosen alias one of the Vars and the
   scope-exit drops of every Var plus v_chosen all hit
   the same heap. `consume_if_moved_var` now recurses
   into every arm's body the same way it recurses into
   if-expr branches. Str scrutinees were already
   covered through check_match_str's IfExpr-chain
   desugar. Conservative: unchosen-arm Vars leak (same
   TODO as the if-expr case). Test totals: 865 lib +
   47 e2e passing. Closure #173.

   **If-expr Var branches consume both Vars done 2026-05-24**:
   `let chosen = if cond { a } else { b };` (a, b: Vars
   of OwnedStr) was double-freeing on scope exit. The
   codegen ternary `cond ? v_a : v_b` makes v_chosen
   alias the chosen Var's heap, so the scope-exit drops
   of v_a, v_b, AND v_chosen all hit the same heap.
   `consume_if_moved_var` only descended into bare Var
   and FieldAccess sources — IfExpr fell through `_ =>
   {}`. Now it recurses into both branches and marks
   each branch's Var moved. Conservative: the UNCHOSEN
   alternative leaks (its heap isn't freed since the Var
   is marked moved). Both backends were affected
   (checker/IR-level bug). Closure #172. Known
   remaining: unchosen-alternative leak (tracked in
   TODO.md). Test totals: 864 lib + 47 e2e passing.

   **`push(xs, v)` / `set(xs, i, v)` consume value Var done 2026-05-24**:
   `push(xs, v)` and `set(xs, i, v)` where `v` is a Var
   of OwnedStr (or any non-Copy heap-owner) were
   double-freeing on scope exit. The checker's builtin
   handlers consumed `args[0]` (the Vec) via
   `consume_if_moved_var` but never the value arg —
   so the source Var stayed "live" and its scope-exit
   drop fired AFTER push transferred ownership into the
   new Vec's slot, freeing the heap a second time when
   the Vec was later __free'd. ASan caught it on a
   chained `let xs2 = push(xs, v); let xs3 =
   push(xs2, w);`. Both backends were affected
   (checker/IR-level bug). Two-line fix: also call
   `consume_if_moved_var` on the value arg in push() and
   set(). Test totals: 863 lib + 47 e2e passing.
   Closure #171.

   **tree-LLVM nested FieldAssign drops old heap done 2026-05-24**:
   `o.inner = NewInner { name: "fresh" + "" };` (struct-
   typed field on a struct that already owns heap) was
   leaking the OLD nested struct's heap-owning fields.
   Tree-LLVM's FieldAssign had drop-old arms for OwnedStr
   and Vec (closure #132); Struct and Enum fell through
   `_ => {}`. Tree-C had the parallel arms via closure
   #148. Added Struct arm (walks the OLD nested struct's
   heap-owning fields via emit_llvm_struct_field_drops
   before overwriting) and a defensive Enum arm (mirrors
   the Reassign Enum drop in closure #169; the checker
   currently gates enum-as-struct-field but kept for
   parity). Test totals: 862 lib + 47 e2e passing.
   Closure #170.

   **tree-LLVM Reassign of struct/enum drops old heap done 2026-05-24**:
   `b = Box { name: "second" + "" };` (where `b: Box` has
   `name: OwnedStr`) and `m = Maybe.Some("second" + "");`
   (payloaded enum) were leaking the OLD value's heap.
   Tree-LLVM's Reassign drop_old match only had arms for
   Vec and OwnedStr; Struct / Enum fell through `_ => {}`.
   Tree-C had the parallel arms via closure #147. Added
   the Struct arm (walk OLD alloca's heap-owning fields
   via emit_llvm_struct_field_drops) and the Enum arm
   (load OLD tagged-union, branch on tag, free heap
   payload if active — mirrors the Drop handler's Enum
   arm). Test totals: 861 lib + 47 e2e passing. Closure
   #169.

   **tree-LLVM Discard of OwnedStr frees heap done 2026-05-24**:
   `let _ = s;` (s: OwnedStr) was leaking. The Discard
   handler's OwnedStr arm sat AFTER the `is_scalar(&expr.ty)`
   arm, but `is_scalar(Type::OwnedStr)` returns true, so the
   scalar arm consumed the branch — it just calls
   `emit_expr` and discards the SSA value, never freeing
   the heap. Same shape as the Struct fix (closure #145)
   that already moved its arm BEFORE `is_scalar`. Now
   OwnedStr is checked first too. Test totals: 860 lib +
   47 e2e passing. Closure #168.

   **tree-LLVM `xs[i] = v` drops old slot done 2026-05-24**:
   `emit_leaf_overwrite_drop` had an early-return on
   `field_path.is_empty()` so the bare-leaf IndexAssign
   on a `Vec<OwnedStr>` / `Vec<Vec<T>>` skipped freeing
   the old slot entirely — the previous element's heap
   leaked. The early-return was originally meant to gate
   the deep mixed-place path; turns out the OwnedStr /
   Vec arms work for both shapes since `p` is the slot
   pointer in either case. Removing the guard fixes the
   leak; Copy element types stay no-ops via the wildcard
   match arm. SSA-C handled this through its own
   `c_element_drop_old` call, tree-C through a separate
   IndexAssign path. Verified ASan-clean. Test totals:
   859 lib + 47 e2e passing. Closure #167.

   **FieldAssign marks RHS Var moved done 2026-05-24**:
   `self.name = n;` inside `fn set_name(self: mut ref T, n:
   OwnedStr)` was double-freeing the new heap. The C
   output ran `free(self->name)` (correct old-slot drop),
   stored `v_n` into the slot (correct), then on the
   method's scope exit ran `free(v_n)` — freeing the heap
   the field now owns. ASan caught it as heap-use-after-
   free on the next read of `b.name`. The checker's Let /
   Reassign / Call-arg arms already call
   `consume_if_moved_var` to mark the RHS Var moved when
   it owns non-Copy heap. FieldAssign was missing that
   call. One-line addition. Verified ASan-clean on both
   backends. Test totals: 858 lib + 47 e2e passing.
   Closure #166.

   **Field-borrow through ref-typed self done 2026-05-23**:
   `ref self.items` inside a method declared `self: ref T`
   was broken on both backends. Tree-C emitted
   `&v_self.items` — gcc rejected it with "request for
   member 'items' in something not a structure" since
   v_self is `Struct_T*`. Tree-LLVM emitted
   `getelementptr %Struct_T*, %Struct_T** %arg_self, …`
   which lli rejected because %arg_self IS %Struct_T*,
   not a pointer-to-pointer. Both bugs stemmed from
   RefField/RefMutField carrying only the binding's
   name — not its type — so backends couldn't tell
   whether the binding was owned (use `.`) or borrowed
   (use `->`). Fix: add `object_ty: Type` to RefField /
   RefMutField in the IR; tree-C picks `.` vs `->` from
   `object_ty.is_any_ref()`; tree-LLVM derefs object_ty
   before spelling the GEP source type so the
   indirection level matches. Test totals: 857 lib +
   47 e2e passing. Closure #165.

   **tree-C struct typedefs topologically sorted done 2026-05-23**:
   Source order was emitting `typedef struct {
   Struct_Inner inner; } Struct_Outer;` BEFORE
   `Struct_Inner` was declared. cc rejected the output
   with "unknown type name 'Struct_Inner'". The emit
   loop now does a DFS over the struct dependency graph
   (direct `Struct(S)` field or `[S; N]` field), so
   dependencies come first. Vec / Ref / Atomic / Mutex /
   Guard / Channel / Tuple all introduce pointer-shaped
   indirection through their own typedef bundles, so
   they don't drive struct dependencies. LLVM's IR
   forward-declares named types so tree-LLVM was
   unaffected. Test totals: 856 lib + 47 e2e passing.
   Closure #164.

   **tree-LLVM `t.items[i]` for Vec field done 2026-05-23**:
   `b.items[1]` (FieldAccess base, Vec element type) was
   panicking the tree-LLVM Index handler with
   `unreachable!("Index on unsupported base")`. The
   handler already had a FieldAccess arm for Array-typed
   fields; the parallel Vec arm now reuses
   emit_lvalue_addr to get the field-pointer (which is
   itself the Vec struct address), GEPs into .data, loads
   the element pointer, GEPs at the dynamic idx, and
   loads. Same shape is reachable whenever an OwnedStr
   concat or a clone_at sibling forces an SSA fallback.
   Test totals: 855 lib + 47 e2e passing. Closure #163.

   **tree-LLVM `len` on field forms done 2026-05-23**:
   Closure #161 fixed `len(ref xs)` (Ref/RefMut wrapping
   Var). This closes the other two shapes that also fell
   through to the static-length fallback in tree-LLVM:
   `len(ref t.items)` / `len(mut ref t.items)` (RefField /
   RefMutField) and `len(t.items)` (FieldAccess yielding a
   Vec). Both shapes hit lli's verifier with the `i64 0`
   operand the fallback emitted, crashing the program
   before it could run. Field-borrow forms reuse the
   field-pointer that `emit_expr` already materializes;
   bare field-access calls `emit_lvalue_addr` to get a
   pointer to the field. Both then GEP into the Vec's
   `.len` slot (field index 1) and load. Test totals:
   854 lib + 47 e2e passing. Closure #162.

   **tree-LLVM `len(ref Vec)` GEP+load fix done 2026-05-23**:
   `emit_expr` Len handler only matched `array.kind ==
   Var(name)`. When the source spelled the argument as
   `len(ref xs)`, the typed expression is `Len { array:
   Ref { name = "xs" } }`, so the Var arm was skipped and
   the handler fell through to a `format!("{}", length)`
   fallback. `length` carries the static array length —
   for Vec it's always 0, so any `len(ref xs)` on a Vec
   that landed on tree-LLVM (e.g. when a sibling
   expression forced an SSA fallback) returned 0 instead
   of the real length. Now the Ref/RefMut(name) case
   resolves to the same alloca address as Var(name) and
   takes the GEP-into-.len + load path. Test totals: 853
   lib + 47 e2e passing. Closure #161.

   **tree-LLVM Block-expr emits Drop stmts done 2026-05-23**:
   `match <fresh OwnedStr> { … }` was leaking the
   scrutinee's heap on tree-LLVM. The checker's
   `check_match_str` desugar wraps the if-chain in a
   Block { Let temp = scr; Let result = ifchain; Drop
   temp; result } (closure #137), so the temp gets
   released after the if-chain evaluates. Tree-C's
   Block emitter handled the Drop arm; tree-LLVM's
   emitter only routed `Let` and `Print` through
   `emit_stmt` — the Drop was silently discarded.
   Fix: extend the Block emitter to also forward
   `TypedStmt::Drop` to `emit_stmt`, which already
   knows how to free OwnedStr / Vec / Struct / Enum
   bindings registered in `ctx.locals` (each Let stmt
   above the Drop puts the binding's alloca address
   there). Verified clean under
   `-fsanitize=address,leak` for `match make_owned()
   { "abcdef" then 1, _ then 0 }`. Closure #160.

   **Consuming `for x in xs` on `Vec<non-Copy>` shallow-frees done 2026-05-23**:
   `for x in xs` (consuming form) over `Vec<OwnedStr>` /
   `Vec<Vec<T>>` / `Vec<Struct{heap}>` /
   `Vec<Enum{heap-payload}>` was double-freeing the
   per-element heap. Each iteration loaded the slot into
   `x` and freed it via `x`'s scope-exit drop; then the
   post-loop code called `intent_vec_<T>__free(xs)` which,
   after closure #127, walks every slot and frees its
   inner heap → second free of the same heap (ASan
   double-free). Fix: tree-C and tree-LLVM `emit_for_iter`
   now emit a shallow `free(xs.data)` (only the outer
   buffer) when the element type is non-Copy and the
   collection is owned. Copy-element collections still
   route through `intent_vec_<T>__free` (which is just
   `free(xs.data)` for Copy elements anyway). The SSA path
   never emitted a Drop for the consumed collection at
   all — silently leaking the outer buffer (no IR shape
   for "free outer buffer only"). Gated out via
   `stmt_ssa_supported`'s `ForIter` arm: programs with
   consuming for-iter over non-Copy Vec elements now fall
   back to tree-LLVM / tree-C. Verified clean under
   `-fsanitize=address,leak` for `Vec<OwnedStr>`,
   `Vec<Vec<i64>>`, and `Vec<Struct{OwnedStr,i64}>`.
   Closure #159.

   **SSA-LLVM vec set/push/clone arg type fix done 2026-05-23**:
   `emit_vec_call` in
   [src/ssa_backend_llvm.rs](src/ssa_backend_llvm.rs) was
   falling back to `element.clone()` whenever
   `operand_type(...)` returned `None` (which happens for
   every `Operand::Const`). That typed `set`'s i64 index
   slot as the element type — e.g. `set(Vec<OwnedStr>,
   0, v)` emitted `i8* 0` for the literal index, which
   the lli verifier warned about and the call site
   tolerated by accident. Fix: a per-builtin signature
   table (`sig_at(pos)`) returns the correct expected
   type per position: `push(Vec<T>, T)`, `set(Vec<T>,
   i64, T)`, `clone(Vec<T>)`. Const operands now type
   correctly. Closure #158.

   **`clone_at` Enum element done 2026-05-23**:
   tree-LLVM's `clone_at` panicked for Enum element
   types. Closures #154 / #155 added OwnedStr / Struct
   arms; #156 finishes Enum with an OR-chain over the
   payloaded tags, branching to a deep-clone block
   (`intent_str_concat` of the OwnedStr payload, then
   insertvalue into a new enum struct) vs a tag-only
   block (use the loaded slot as-is), phi-joined into
   `dest`. Tree-C was already correct via
   `c_element_deep_clone`'s Enum arm from #152.

   **`clone_at` Struct element done 2026-05-23**:
   tree-LLVM's `clone_at` panicked with "Struct(…) not
   yet supported" when the Vec element was a struct with
   heap fields. Closure #154 only added the OwnedStr arm;
   #155 finishes Struct: load the slot, extract each
   field, deep-clone OwnedStr fields via
   `intent_str_concat` with the empty literal, assemble
   via an insertvalue chain. Tree-C was already correct
   (closure #153 made `c_element_deep_clone` recurse
   through Struct fields). Closure #155.

   **`clone_at(ref xs, i)` for OwnedStr / Struct slots done 2026-05-23**:
   `clone_at(ref Vec<OwnedStr>, i)` was broken in two
   places: (1) SSA-C had no `clone_at` handler — fell
   through to the `fn_clone_at(...)` user-fn shape and
   failed at link time with an undefined-reference; (2)
   tree-LLVM's `clone_at` only handled Copy and `Vec<U>`
   element types — OwnedStr / Struct panicked with "not
   yet supported in tree-LLVM". Both backends now produce
   per-element deep clones: SSA-C routes through the
   existing `c_element_deep_clone` helper; tree-LLVM
   loads the i8* slot and calls `intent_str_concat` with
   the `@.empty_str_clone` empty literal. Closure #154.

   **`clone(Vec<Struct{heap-field}>)` deep-copies done 2026-05-23**:
   `clone(Vec<Tag>)` where `Tag` carries an OwnedStr field
   was shallow-copying the struct, sharing the field's
   heap pointer between source and clone — both __free
   sites then freed the same heap (ASan double-free; lli
   abort). C `c_element_deep_clone` adds a `Type::Struct`
   arm that reconstructs the struct with each owning
   field deep-cloned (recursive call) and Copy fields
   copied as-is. LLVM's per-shape Vec __clone gets a
   parallel Struct arm: extract each field, deep-clone
   (OwnedStr via the `@.empty_str_clone`-fed
   `intent_str_concat`), assemble the new struct via an
   insertvalue chain. Closure #153.

   **`clone(Vec<OwnedStr>)` / `clone(Vec<Enum>)` deep-copies done 2026-05-23**:
   the per-shape Vec `__clone` helper was shallow-copying
   the per-element heap pointers — `clone(Vec<OwnedStr>)`
   produced a new Vec whose i8* slots aliased the source's,
   so the source's free + the clone's free double-freed the
   shared heap. C `c_element_deep_clone` now deep-clones
   `OwnedStr` via `intent_str_concat(slot, 0, "", 0)` (round-
   trip through the concat helper with an empty literal,
   gives a strdup-like copy) and `Enum` via a tag-switched
   ternary that reconstructs the enum with a deep-cloned
   OwnedStr payload for payloaded variants. LLVM's per-
   shape `__clone` helper extended to loop over slots for
   ANY non-Copy element type (was only handling `Vec<U>`
   elements; `OwnedStr` / `Enum` payloads fell through to
   an uninitialized buffer, crashing lli with "free():
   invalid pointer"). LLVM emit also adds an
   `@.empty_str_clone` private constant. Closure #152.

   **`Vec<PayloadedEnum>` compiles + drops correctly done 2026-05-23**:
   `Vec<Msg>` where `Msg` is a payloaded enum was broken
   in four places: (1) C `element_tag` and (2)
   `c_element_storage` fell through to `c_leaf_type` →
   "int32_t" for enums, so the per-shape typedef tried to
   store `Enum_Msg` struct literals into i32 slots (cc
   rejected); (3) `c_element_drop_old` lacked an Enum arm,
   so the per-element drop body was empty (payloads leaked
   at `intent_vec_Enum_Msg__free` time); (4) LLVM vec
   literal used `vec_element_byte_size` for enums
   (returning 8 = i64), under-allocating the 16-byte
   tagged union and crashing lli with "free(): invalid
   pointer". All four sites now treat payloaded enums
   like structs/tuples: `Enum_<Name>` tagged-union
   typedef, GEP-null sizeof, tag-switched per-element
   payload free. Closure #151.

   **IndexAssign whole-element for OwnedStr/Vec elements done 2026-05-23**:
   `Vec<OwnedStr>[i] = "x" + ""` and `Vec<Vec<i64>>[i] =
   vec(…)` were leaking the OLD element's heap. Closure
   #149's IndexAssign whole-element extension only added
   the Struct/Enum element-type arms; OwnedStr and Vec
   element types fell through to a plain store. Tree-C
   `emit_index_assign` now also frees the OLD slot for
   `Type::OwnedStr` (`free((void*)<lv>)`) and `Type::Vec`
   (`intent_vec_<T>__free(<lv>)`) leaf cases. SSA-C's
   `InstrKind::IndexAssign` emitter extended in parallel
   (the `Vec<OwnedStr>` case routes through SSA, not
   tree-C). Closure #150.

   **IndexAssign of Struct/Enum element frees old heap done 2026-05-23**:
   `xs[i] = newStruct` for a `Vec<Struct{heap-field}>`
   element was leaking the OLD element's heap fields.
   The IndexAssign leaf-drop logic (closure #126) only
   fired when `field_path != []` (i.e. `xs[i].field =
   …`); whole-element overwrites (field_path empty +
   leaf == Struct/Enum) fell through to a plain store,
   losing the old heap. Tree-C's `emit_index_assign`
   now also handles the `field_path == []` case for
   leaf-Struct (walk per-field drops over the OLD
   element) and leaf-Enum (switch on the OLD tag to
   free the payload). Closure #149.

   **FieldAssign of Struct-typed field frees old heap done 2026-05-23**:
   `o.inner = newInner` where Inner has heap-shaped fields
   (OwnedStr / Vec) was leaking the previous Inner's
   heap. FieldAssign's heap-overwrite logic (from closure
   #132) only handled OwnedStr / Vec field types; Struct
   fell through to a plain assign. Tree-C now also walks
   the OLD struct field's per-field drops via
   `emit_struct_field_drops` before storing the new
   value. Enum-typed struct fields are still gated by
   the checker (not yet supported), so no fix needed
   there. Closure #148. Verified leak-free under
   `-fsanitize=address,leak`.

   **Reassign of Struct / Enum with heap fields done 2026-05-23**:
   `t = Tag { name: …}` and `m = Msg.Text(…)` for bindings
   with heap-shaped fields / payloads were leaking the
   previous heap. Tree-C, tree-LLVM, and SSA Reassign
   handlers only had Vec / OwnedStr drop-old cases — Struct
   / Enum fell through to plain assign. Tree-C now eval-
   into-tmp, walk the old binding's per-field drops (Struct)
   or switch-on-tag payload free (Enum), then move the tmp
   in. SSA's drop_old whitelist extended to admit non-Copy
   Struct / Enum; backends' `Drop` emit handlers already
   knew how to walk those. Closure #147. Verified leak-free
   under `-fsanitize=address,leak`.

   **`let _ = make_enum()` frees heap payload done 2026-05-23**:
   `let _ = make_enum();` for an enum with a heap-shaped
   payload (OwnedStr / Vec<T>) was leaking. Same shape
   as closure #145's struct discard fix — Tree-C, tree-
   LLVM, and SSA Discard handlers only matched
   OwnedStr / Vec / Struct; `Type::Enum(_)` fell
   through. Tree-C spills to `Enum_<Name> _intent_discard`,
   switches on the tag, and frees the payload for the
   payloaded variants. Tree-LLVM mirrors the scope-exit
   Drop logic for enums (extractvalue tag / extractvalue
   payload, OR-chain of `icmp eq` tags, conditional
   branch to the free block, `@free` or
   `@intent_vec_<tag>__free`). SSA Discard emits
   `InstrKind::Drop` for non-Copy enums. Closure #146.

   **`let _ = make_struct()` frees heap fields done 2026-05-23**:
   `let _ = make_struct();` for a struct with heap-shaped
   fields (OwnedStr, Vec<T>, nested struct) was silently
   leaking the per-field heap. Tree-C, tree-LLVM, and SSA
   Discard handlers all only matched `OwnedStr | Vec(_)`
   — `Type::Struct(_)` fell through to a `(void) expr`
   (tree-C) or bare `emit_expr` (LLVM / SSA), never
   freeing the struct's owning fields. Tree-C now spills
   to a brace-scoped `_intent_discard` local and walks
   the fields via `emit_struct_field_drops`; tree-LLVM
   spills to an alloca and walks via
   `emit_llvm_struct_field_drops` (the existing per-field
   helper used by scope-exit Drop). The tree-LLVM arm
   also had to be moved BEFORE the `is_scalar` check —
   `is_scalar(Type::Struct(_))` returns true since the
   alloca path treats structs like scalars; without the
   reorder the discard would skip the Struct arm. SSA
   Discard emits an `InstrKind::Drop` for non-Copy
   structs. Closure #145. Verified leak-free under
   `-fsanitize=address,leak` against a 100-iter loop
   (was leaking ~300 bytes pre-fix).

   **`intent_str_concat` l_owned flag fix done 2026-05-23**:
   `t.name + "-suffix"` where `t.name: OwnedStr` was
   DOUBLE-FREEING. The concat helper's `l_owned`/`r_owned`
   flag was set unconditionally for any OwnedStr-typed
   operand, so concat freed the struct field's heap. Then
   the struct's per-field scope-exit Drop also freed it
   (the partial-move tracking only kicks in for whole-
   binding `Stmt::Assign` moves, not for FieldAccess in
   binary-op operands). New
   `crate::ir::owned_str_consumed_at_concat(expr)` helper
   uses a refined rule: `l_owned=1` only when the operand
   is a Var (checker marks Var as moved by the binary op,
   so the binding's Drop is suppressed and concat MUST
   free) OR fresh (Call / Binary / Block / IfExpr /
   Match — no other owner). FieldAccess / TupleAccess /
   Ref keep `l_owned=0` so the binding's Drop owns the
   free. Closure #144. Verified leak-free + double-free-
   free under `-fsanitize=address,leak` against both the
   Var-Var concat (`g + "!"`) and the FieldAccess-Str
   concat (`t.name + "-suffix"`) shapes. See updated
   [examples/struct_owned_field.intent](examples/struct_owned_field.intent).

   **`clone(fresh_vec)` drops borrowed arg done 2026-05-22**:
   `clone(vec(1, 2, 3))` was silently leaking the fresh
   Vec passed in. The checker treats `clone(xs)` as
   borrow-semantics — xs continues to be readable after
   the call (useful for "deep copy without consuming the
   source") — but for a fresh-Vec argument there's no
   other binding to own the heap. SSA Call lowering now
   emits a `Drop` after the `clone` call for each fresh
   non-Copy argument. Var / FieldAccess args skip
   (binding owns). Other built-ins (`push`, `set`,
   `vec()`) either consume their args at the checker
   level or are construction-only. Closure #143.

   **`Index` of fresh Vec drops buffer done 2026-05-22**:
   `vec(1, 2, 3)[0]` (and other fresh-Vec index shapes)
   was silently leaking the Vec buffer — the `Index`
   instruction reads one element but doesn't free
   `.data`. Mirrors closure #141 for `Len`. SSA Index
   lowering emits a `Drop` after the `InstrKind::Index`
   when the operand is a fresh Vec. Tree-C `emit_index`
   for Vec wraps the index read in a brace-scoped tmp +
   `intent_vec_<T>__free` for fresh operands. Var /
   FieldAccess Vec operands keep the simple form.
   Closure #142.

   **`len` of fresh Vec drops buffer done 2026-05-22**:
   `len(vec(1, 2, 3))` was leaking the Vec buffer — the
   `Len` instruction reads `.len` from the struct but
   doesn't touch `.data`. Generalized
   `is_fresh_owned_str` to `is_fresh_non_copy` (matches
   both OwnedStr AND `Vec<T>` over the same
   Call/Binary/Block/IfExpr/Match kind whitelist). SSA
   `Len` lowering for non-Str arrays now emits a `Drop`
   after the Len instruction when the operand is fresh.
   Tree-C `emit_len` for Vec wraps the `.len` read in a
   brace-scoped tmp + `intent_vec_<T>__free` for fresh
   operands. Verified leak-free under
   `-fsanitize=address,leak` against a 1000-iteration
   loop (1000 × 5-element vecs previously left ~40KB
   of leaked buffers). Closure #141. See updated
   [examples/vectors.intent](examples/vectors.intent).

   **Unified fresh-OwnedStr helper + tree-C strcmp/strlen fixes done 2026-05-22**:
   The per-site `matches!(e.ty, OwnedStr) && matches!(e.kind,
   Call | Binary)` whitelist used by closures #135 (print),
   #137 (match scrutinee), #138 (strcmp), and #139 (strlen)
   is now a single `crate::ir::is_fresh_owned_str(expr)`
   helper. The unified helper also broadens the set to
   include `Block` / `IfExpr` / `Match` expressions
   returning OwnedStr — these escape an inner heap to the
   outer context (the inner Let bindings are never Drop'd
   by the Block emitters in v1, so the value's only owner
   is the outer use site). The leak surfaced as
   `len({ let s = make(); s })` slipping past closure
   #139's narrower whitelist. The tree-C emit_len and
   emit_binary-strcmp paths (previously untouched by #138
   / #139) now also free fresh operands via GCC statement-
   expression-wrapped temps. Closure #140. Verified
   leak-free under `-fsanitize=address,leak`.

   **`len` of fresh OwnedStr drops heap done 2026-05-22**:
   `len(make_owned_str())` was silently leaking —
   `intent_str_len` (strlen) doesn't consume its argument,
   so a fresh-OwnedStr operand (Call / Binary `+`) had no
   other binding to own the heap after the `len` call.
   Fixed in both the SSA lowering of `TypedExprKind::Len`
   for `Str/OwnedStr` (emits a `Drop` instruction after
   the `intent_str_len` call) and the tree-LLVM
   `TypedExprKind::Len` arm (emits `call void @free(i8* %v)`
   after the strlen). Var / FieldAccess operands skip
   the drop — same conservative whitelist as closures
   #135 / #137 / #138. Closure #139. Verified leak-free
   under `-fsanitize=address,leak`. See updated
   [examples/strings_concat.intent](examples/strings_concat.intent).

   **`strcmp` of fresh OwnedStr drops heap done 2026-05-22**:
   `make_owned_str() == "literal"` (and `!=`, `<`, `<=`,
   `>`, `>=`) was silently leaking — `intent_str_cmp` /
   `strcmp` doesn't consume its arguments, so a fresh
   OwnedStr operand (Call / Binary `+`) had no other owner
   after the comparison. Fixed in both the SSA lowering of
   string comparison (emits a `Drop` instruction after the
   strcmp call for each fresh operand) and the tree-LLVM
   Binary-strcmp branch (emits `call void @free(i8* %v)`
   after the compare). Var / FieldAccess operands skip
   the drop — same whitelist as closure #135's print
   handling and closure #137's match scrutinee, so the
   outer binding's scope-exit Drop owns the heap and
   nothing double-frees. Closure #138. Verified leak-free
   under `-fsanitize=address,leak`. See updated
   [examples/strings_concat.intent](examples/strings_concat.intent).

   **`match make_owned_str() { … }` drops temp scrutinee done 2026-05-22**:
   `match` on a fresh OwnedStr scrutinee (Call or `+`
   concat producing a heap string with no other owner)
   was silently leaking. `check_match_str` bound the
   scrutinee to a temp inside a synthetic Block but never
   emitted a Drop for the temp, so the heap escaped at
   Block exit. Restructured the synthetic Block to wrap
   the if-chain through a `__match_str_result_<n>` let,
   emit a `TypedStmt::Drop` for the temp after the
   if-chain runs, then yield the result var as the
   block's tail. Tree-C / tree-LLVM Block codegen also
   extended to emit Drop stmts inside the GCC
   statement-expression body. The fix uses the same
   conservative whitelist as closure #135 (Call / Binary
   only) so Var / FieldAccess scrutinees — which alias an
   outer-binding's heap — don't get spuriously dropped
   (would double-free at the outer scope's existing
   Drop). Closure #137. Verified leak-free under
   `-fsanitize=address,leak`. See updated
   [examples/match_str.intent](examples/match_str.intent).

   **`Vec<OwnedStr>` compiles to valid C done 2026-05-22**:
   the C backend's `element_tag` helper was leaking the
   `*` from `c_leaf_type(OwnedStr) = "char*"` into the
   per-shape Vec typedef name — `Vec<OwnedStr>` emitted
   `typedef … intent_vec_char*;` and the cc step failed
   with "expected ';'…before '*'". Added explicit arms
   for Type::Str (`str`) and Type::OwnedStr (`owned_str`)
   so the typedef becomes `intent_vec_owned_str`. LLVM
   was already sanitizing `*`→`p` via its own
   `vec_struct_tag`. No example exercised `Vec<OwnedStr>`
   before — `examples/strings_concat.intent` now does.
   Closure #136.

   **`print` of fresh OwnedStr expression frees heap done 2026-05-22**:
   `print make_owned_str();` was silently leaking the
   returned heap string. All three print emit paths —
   SSA (which routes both SSA-C and SSA-LLVM via
   `intent_print_item`), tree-C `emit_print_expr_no_newline`,
   and tree-LLVM `emit_print_expr_no_newline` — handled
   OwnedStr as a borrowed read (the right thing for
   `print s;` where `s: OwnedStr`) but never freed the
   heap when the printed value came from a fresh
   expression with no other owner. The fix uses a
   conservative whitelist: free after print only when the
   item's TypedExprKind is `Call { … }` or `Binary { … }`
   (the v1 OwnedStr heap-producers). Var / FieldAccess /
   TupleAccess and any other variant skip the free so the
   binding's scope-exit Drop still has the only handle —
   freeing eagerly there would double-free (e.g. the
   `print t.name` pattern in
   `examples/struct_owned_field.intent`). Closure #135.
   Verified leak-free under `-fsanitize=address,leak` on
   both the fresh-call and binding-owned shapes. See
   updated [examples/strings_concat.intent](examples/strings_concat.intent).

   **`let _ = …` discard of OwnedStr frees heap done 2026-05-22**:
   `let _ = make_owned_str();` (and the bare-call form
   `make();` that the parser sugars to it) was silently
   leaking the returned heap string. All three Discard
   emit paths — tree-C, tree-LLVM, and the SSA lowerer
   that fed both SSA-C and SSA-LLVM — handled `Vec<T>` but
   skipped `OwnedStr`, falling through to a `(void)`-style
   no-op. SSA Reassign lowering was also extended to lower
   `drop_old` reassigns for OwnedStr / Vec (was a hard
   reject — fell back to tree backends) so closure #133's
   `current = "step-" + ""` shape lowers through SSA too.
   Closure #134. Verified leak-free under
   `-fsanitize=address,leak`. See updated
   [examples/strings_concat.intent](examples/strings_concat.intent).

   **Reassign of OwnedStr frees previous heap done 2026-05-22**:
   `s = "b" + ""` for a non-Copy `OwnedStr` binding now
   frees the previous heap string before storing the new
   value. Was a real leak: the Reassign emit's drop-old
   path only handled `Type::Vec`; OwnedStr fell through to
   the plain-assign branch and the previous allocation was
   lost. C emits the same tmp-eval / free-old / move-tmp
   shape Vec uses; LLVM emits eval-first then free-old then
   store (the previous order — free-before-eval — was also
   incorrect for any non-consuming RHS that READS the
   binding, e.g. `s = s + ""`-ish patterns; the LLVM Vec
   path had the same latent issue and is also fixed here).
   Closure #133. Verified leak-free under
   `-fsanitize=address,leak`. See updated
   [examples/strings_concat.intent](examples/strings_concat.intent).

   **FieldAssign with heap-shaped field frees old slot done 2026-05-22**:
   `t.name = newstr` for an OwnedStr field (and
   `b.items = newvec` for a Vec field) now frees the
   previous slot's heap before storing the new value, both
   for plain owned-struct assigns and through-`mut ref`
   borrows. Mirrors the leaf-Drop logic from closure #126
   for mixed-place index-assigns (`xs[i].field = …`). Was
   a real leak: a struct with an OwnedStr field that gets
   overwritten leaked the old string until scope exit, and
   the scope-exit drop only freed the latest pointer. The
   bug was masked because no example exercised the
   field-overwrite pattern. Closure #132. C backend now
   emits `free((void*)<lvalue>)` or
   `intent_vec_<T>__free(<lvalue>)` before the assign;
   LLVM loads the old pointer/struct via the same GEP and
   calls `@free` or the matching vec `__free`. Verified
   leak-free under `-fsanitize=address,leak`. See updated
   [examples/struct_owned_field.intent](examples/struct_owned_field.intent).

   **Cross-backend parity runner covers all examples done 2026-05-22**:
   the `llvm_backend_run_produces_same_output_as_c` runner
   in [tests/run_end_to_end.rs](tests/run_end_to_end.rs)
   was missing 14 of the 57 examples — including
   `try_keyword.intent`, `block_expressions.intent`,
   `option_types.intent`, `option_error_propagation.intent`,
   `interfaces.intent`, `generic_functions.intent`,
   `composite_types.intent`, `fn_pointers.intent`,
   `methods.intent`, `assert_messages.intent`,
   `tracker.intent`, and the three Devanagari keyword
   examples (`hindi_keywords.intent`,
   `marathi_keywords.intent`, `sanskrit_keywords.intent`).
   Closure #130 surfaced this gap by hitting a pre-existing
   C codegen bug that the parity runner would have caught
   at landing time had `try_keyword.intent` been listed.
   All 57 examples now run identically on both backends.
   Closure #131.

   **`try` desugar admits intermediate `print` done 2026-05-22**:
   the `let v = try opt; …; return X;` desugar's
   intermediate-stmt check was relaxed from Let-only to
   Let + Print, riding on closure #129's extension of
   block expressions. Useful for tracing the happy path
   while the desugar still short-circuits the None case.
   This closure also fixed a pre-existing C-backend codegen
   bug surfaced by trying to emit the OFFICIAL example
   end-to-end: match expressions with a payloaded-enum
   result type were using `c_element_storage` (returns the
   bare `int32_t` tag for any enum) instead of `c_type_name`
   (returns `Enum_<Name>` for payloaded enums); the bug
   never showed up in unit tests because they stopped at
   `compile()` and didn't `emit`+`cc`. Closure #130. See
   updated [examples/try_keyword.intent](examples/try_keyword.intent).

   **Block expressions admit print stmts done 2026-05-22**:
   `let r = { let a = …; print "log", a; tail }` now
   compiles. The v1 Block MVP was Let-only; the relaxation
   keeps the same shape (Let prefix + tail expression) but
   also lets `print` stmts interleave for logging
   intermediate values. Control flow, reassignment, and
   other shapes still surface the existing diagnostic.
   Parser accepts a Let/Print prefix; checker pushes
   `TypedStmt::Print` into the block's stmts; tree-C and
   tree-LLVM Block emitters emit print stmts inline (via
   `emit_print_items` / the standard stmt emitter). SSA
   Block routing is unchanged (still falls back to tree
   backends). Closure #129. See updated
   [examples/block_expressions.intent](examples/block_expressions.intent).

   **OwnedStr enum payload destructure done 2026-05-22**:
   `match m { Msg.Text(s) then … }` now compiles when the
   variant payload is `OwnedStr`. The binding `s` is
   exposed to the arm body as a `Str` (Copy borrowed view),
   so the scrutinee retains ownership and its existing
   scope-exit Drop frees the heap exactly once. Other
   non-Copy payload types (`Vec<T>`, structs with owning
   fields, …) still need their own borrow-view wiring and
   stay rejected. Closure #128 / D3. Verified leak-free
   under `-fsanitize=address,leak`. See updated
   [examples/enum_owned_payload.intent](examples/enum_owned_payload.intent).

   **Vec element drops walk owning fields done 2026-05-22**:
   `intent_vec_<S>__free` now iterates every live element
   and drops its owning resources before freeing the buffer,
   for `S = OwnedStr`, `S = Struct{…}` with owning fields,
   and `S = Vec<U>` (was already handled). Closes a
   pre-existing leak where `Vec<Struct{OwnedStr…}>` and
   `Vec<OwnedStr>` left their element heaps unfreed at
   scope exit. C: `c_element_drop_old` extended with
   `OwnedStr` and `Struct` arms (the latter via
   `emit_struct_field_drops`); LLVM: per-element loop body
   emits per-field GEP + load + `@free` /
   `@intent_vec_<tag>__free` driven by a slim local
   counter. Closure #127. Verified leak-free under
   `-fsanitize=address,leak`. See updated
   [examples/struct_owned_field.intent](examples/struct_owned_field.intent).

   **Match on `Str` / `OwnedStr` scrutinee done 2026-05-22**:
   string-literal patterns desugar at the checker level to a
   nested if-expression chain over `==` on Str (strcmp-based).
   The scrutinee binds to a temp once so any side-effecting
   expression evaluates exactly once. Wildcard required.
   No backend changes — uses existing `==`/IfExpr/Block
   primitives. See
   [examples/match_str.intent](examples/match_str.intent).

   **Match on `bool` scrutinee done 2026-05-22**:
   `match b { true then …, false then …, _ then … }` works.
   Exhaustiveness requires both arms OR a wildcard. Bool
   patterns lower as int_value=0/1 so the existing
   integer-switch shape handles dispatch uniformly across
   both backends. New `Pattern::Bool` + `Pattern::Str`
   variants on the AST; Str still surfaces a "not yet
   supported" diagnostic (strcmp-dispatch is the natural
   follow-up). See
   [examples/match_bool.intent](examples/match_bool.intent).

   **`xs[i].field = v` mixed-place assign done 2026-05-22**:
   single-level field path on the indexed element. Parser
   builds an `IndexAssign` with a non-empty `field_path`;
   the checker validates the path against the struct decl;
   both backends GEP/access into the slot and store at the
   field offset. Copy-leaf only in v1 (avoids field-Drop on
   overwrite). See
   [examples/mixed_place_assign.intent](examples/mixed_place_assign.intent).

   **In-place `push(mut ref xs, v)` done 2026-05-22**: a
   second form of `push` that operates through a Vec
   pointer instead of consuming + returning the Vec.
   Useful for growing a Vec owned by a struct field
   without partial-move + write-back. Same realloc
   logic as the consuming form; returns `i64` (new len).
   See [examples/push_mut.intent](examples/push_mut.intent).

   **Tuple auto-equality done 2026-05-22**: tuples are
   anonymous so they can't have a user `Eq` impl, but the
   checker synthesizes an AND-chain of per-element
   comparisons: `(a, b) == (c, d)` → `a == c && b == d`.
   Primitive elements use built-in `==`; nominal element
   types (struct/enum) dispatch through the element's
   `<T>_eq` impl. See
   [examples/tuple_eq.intent](examples/tuple_eq.intent).

   **Enum `==` desugar + partial-then-whole-move done
   2026-05-22**: `check_equality` matches `(Enum, Enum)` of
   the same nominal type in addition to struct-struct, so
   `implement Eq for Color { fn eq(self: Color, other: Color)
   -> bool }` makes `a == b` work on Color bindings. The
   enum-type resolver (`resolve_enum_types_in_program`) now
   walks `program.impls` so the impl body's `self: Color`
   resolves to `Type::Enum` (was `Type::Struct`, blocking
   `self as i32`). Separately, moving a struct as a whole
   after a partial-field-move now emits a clean diagnostic
   ("cannot move 'b' — its field 'f' was previously moved
   out"). See
   [examples/enum_eq.intent](examples/enum_eq.intent).

   **Partial-move tracking done 2026-05-21**: `let taken =
   bag.contents;` moves a single field out of a struct
   without invalidating the rest of the struct. A new
   `VarInfo.moved_fields` map tracks which fields have been
   moved out; `TypedStmt::Drop` gained a `moved_fields:
   Vec<String>` list that both backends consult to skip the
   per-field free for moved-out fields. Reading a moved
   field again surfaces a use-after-move diagnostic. See
   [examples/partial_move.intent](examples/partial_move.intent).

   **User-Eq desugar for struct `==` done 2026-05-21**:
   `a == b` and `a != b` on two bindings of the same struct
   type desugar to `<T>_eq(a, b)` / `!<T>_eq(a, b)` whenever
   an `implement Eq for T { fn eq(self: T, other: T) -> bool }`
   is in scope. The hoisted method is the same statically-
   dispatched path used by the `recv.eq(...)` MethodCall
   form. Tuple / enum auto-equality can use the same recipe
   when needed. See
   [examples/struct_eq.intent](examples/struct_eq.intent).

   **Reverse-declaration field drop order done 2026-05-21**:
   struct Drop walks fields in reverse declaration order so
   destruction mirrors construction (Rust's RAII convention).
   Pure code-shape change in both backends' Drop emit.

   **Field-borrow expressions done 2026-05-21**: `ref t.f`
   and `mut ref t.f` now work, single-level. Two new
   `TypedExprKind` variants (`RefField` / `RefMutField`)
   carry the (object, field, field_index) triple; tree-C
   emits `&v_t.f`; tree-LLVM GEPs into the struct. The
   immediate unlock is atomic operations through a struct
   field (`atomic_store(mut ref c.hits, 42)`). Vec push
   through field still needs partial-move tracking. See
   [examples/struct_atomic_field.intent](examples/struct_atomic_field.intent).

   **T2.7 phase 2 — user-Drop auto-call at scope exit done 2026-05-21**:
   `implement Drop for T { fn drop(self: T) -> i64 }` now runs
   automatically at every scope exit where a non-moved binding
   of T goes out of scope. The auto-call is suppressed when
   T also has heap-shaped fields (OwnedStr / Vec) — those
   route through the per-field free pass; users invoke
   `t.drop()` explicitly for richer behavior. Two key wiring
   pieces: (1) the affine-aggregate registry now picks up
   any struct with a hoisted `<T>_drop` function (so the
   scope-exit pass doesn't short-circuit on `is_copy()=true`
   for Copy-only structs), and (2) `self` inside `<T>_drop`
   bodies gets `VarInfo.no_drop = true` to break the
   otherwise-infinite recursion (both the scope-exit pass and
   the Return-path cleanup consult this flag). See
   [examples/drop_interface.intent](examples/drop_interface.intent).

   **T1.2 phase 2b — affine struct fields expanded 2026-05-21**:
   Structs now accept `OwnedStr`, `Vec<T>`, `[T;N]` of Copy
   elements, `Task`, and `Atomic<T>` as fields. Both backends
   emit per-field `free` (heap fields) or no-op (stack-shaped
   fields) on struct Drop. Tree-LLVM gained a FieldAccess-as-
   Index-base arm so `t.data[i]` works. Tree-C reorders to
   emit Vec typedefs before struct typedefs for `struct {
   xs: Vec<T> }` to resolve at its declaration. Struct field
   `[T;N]` uses an inline `T name[N]` declarator and a bare-
   brace `{…}` initializer for the StructLit field path
   (C forbids compound-literal-array assignment into struct
   members). See
   [examples/struct_mixed_fields.intent](examples/struct_mixed_fields.intent).

   **T1.5 phase 2 — bounded generics done 2026-05-21**:
   `fn min<T>(a: T, b: T) -> T where T is Cmp` now monomorphizes
   when the call-site concrete type has a matching
   `implement Cmp for <T>` decl. The previous WIP gate in
   `monomorphize_generics_in_program` has been replaced by an
   impl-existence check that walks `program.impls` for each
   (template, concrete) pair and surfaces a clean diagnostic
   if no satisfying impl is in scope. Scope-aware first-arg
   inference (annotated `let` + fn params) means calls like
   `let m: Score = min(a, b);` resolve correctly. Vtables /
   dynamic dispatch still pending. See
   [examples/bounded_generics.intent](examples/bounded_generics.intent).

   **Phase 2b (OwnedStr fields) done 2026-05-21**: structs
   may now carry an `OwnedStr` field. The aggregate is
   automatically affine; both backends emit a `free` of the
   field at scope exit, and the checker treats struct-literal
   initialization from a `Var` as a move on the source
   binding so a heap string can flow `caller → struct field →
   drop` without a double-free. Implementation: new
   thread-local `STRUCT_NON_COPY_REGISTRY` in
   [src/ast.rs](src/ast.rs) consulted by `Type::is_copy()`;
   per-backend `STRUCT_FIELDS_REGISTRY` / `LLVM_STRUCT_FIELDS_REGISTRY`
   populated at emit start so the `TypedStmt::Drop` handler
   can free each owning field by name (C: `free((void*)v_t.<field>)`)
   or index (LLVM: GEP + load i8* + `@free`). The LLVM
   string-interning pre-pass (`collect_strings_in_expr` in
   [src/backend_llvm.rs](src/backend_llvm.rs)) was the
   blocker — it didn't recurse into `StructLit` / `Tuple` /
   `Match` / `IfExpr` / `Block` / etc., so string literals
   nested inside struct-field initializers fell back to
   `i8* null` and segfaulted at `strlen`. Now recurses into
   every sub-expression form. A new lib test
   `struct_owned_str_field_compiles_and_drops` and example
   `examples/struct_owned_field.intent` exercise the path
   end-to-end. **Phase 2b still pending**: other affine
   field types (`Vec<T>`, `[T;N]`, `Task`, `Atomic<T>`),
   auto-drop chains in reverse-declaration order across
   multiple owning fields, `methods on Type<T>` generic
   methods (depends on T1.4 phase 2), user-defined `Drop`
   auto-call at scope exit (T2.7 phase 2, depends on this
   work).
3. **Enums + `match`** — *phase 1 done 2026-05-20*. Payload-less
   `enum Color { Red, Green, Blue }` declarations, variant references
   `Color.Red`, and `match scrutinee { Variant then expr, … }`
   expressions with **exhaustiveness checking** (every variant must
   have an arm; missing variants are a compile-time error). New
   `enum`, `match`, `then` lexer keywords. The parser produces
   `Type::Struct(name)` for any uppercase-identifier nominal type;
   a new `resolve_enum_types_in_program` pass rewrites
   `Type::Struct(name)` → `Type::Enum(name)` for declared enums
   before signature collection so all downstream analysis sees the
   right Type variant. Enums lower to a 32-bit tag in both backends
   (no per-name typedef needed — they fit in `i32`/`int32_t`).
   Tree-C `match` uses a GCC statement-expression with `switch`;
   tree-LLVM uses an LLVM `switch` with per-arm basic blocks
   merged via `phi`. Three new lib tests pin: working enum + match
   round-trip, non-exhaustive rejection, unknown-variant rejection.
   **Phase 2a done 2026-05-20**: parser accepts payload
   syntax — `enum Maybe { Some(T), None }`, `enum Outcome
   { Ok(i64, i64), Err }`. `EnumVariant.payload: Vec<Type>`
   carries the declared payload types; empty `Vec` means
   payload-less (back-compat with phase 1). The checker has a
   phase-2b gate: any program that declares a payloaded
   variant surfaces a clear "T1.3 phase 2b: tagged-union
   codegen + pattern binding are still in progress" diagnostic
   so users learn the syntax parses but isn't yet executable.
   **Phase 2b/3 done 2026-05-21 (tree-C)**: single-Copy-payload
   enums now compile end-to-end via `--backend=c`. The
   compiler lays them out as `typedef struct { int32_t tag;
   T payload; } Enum_<Name>;` where T is the shared payload
   type. Constructors `Opt.Some(42)` build `(Enum_Opt){.tag
   = 0, .payload = 42}`; match dispatches on `__scr.tag` and
   destructure arms `Opt.Some(v) then …` extract `__scr
   .payload` into a local `v` in the arm body's scope. The
   `TypedEnumDecl` carries `payload_types: Vec<Option<Type>>`
   and `TypedMatchArm` carries `binding: Option<(String,
   Type)>`. LLVM driver currently rejects payloaded enums
   with a "use --backend=c" diagnostic; tree-LLVM
   tagged-union codegen is queued as a follow-up.
   **Still pending**: multi-field payloads (`Pair(i64,
   i64)`), non-Copy payloads (Vec/OwnedStr), mixed payload
   types across variants (would need a union representation
   in the C struct rather than a single field), nested
   destructure (`Outcome.Ok((a, b))`), guards (`Color.Red
   if cond then …`), and LLVM tagged-union codegen.
   **Wildcard `_` pattern done 2026-05-20**: `_ then …`
   arms are accepted by the parser (lexed as the
   identifier `_`), satisfy exhaustiveness without
   listing every variant, and lower cleanly. Tree-C emits
   a `default: __r = (body); break;` case inside the GCC
   stmt-expr switch; tree-LLVM uses the wildcard's basic
   block as the switch's default label so no
   `unreachable + abort` block is emitted. An arm after
   the wildcard is dead and surfaces an "unreachable arm"
   diagnostic. AST refactored:
   `MatchArm.enum_name + variant` fields replaced by
   `MatchArm.pattern: Pattern { Variant { enum_name,
   variant }, Wildcard }`; `TypedMatchArm` gained
   `is_wildcard: bool`. Three new lib tests pin the
   surface (`match_wildcard_covers_remaining_variants`,
   `match_wildcard_alone_is_exhaustive`,
   `match_wildcard_followed_by_arm_rejected`).
   **Integer-literal patterns done 2026-05-20**: match
   scrutinees can now be any integer type (i8/i16/…/u64)
   in addition to enums; arms accept literal-integer
   patterns (positive or negative) plus a required
   wildcard. `Pattern::Int(i128)` joins `Variant` and
   `Wildcard` in the Pattern enum; `TypedMatchArm` gained
   `int_value: Option<i128>` so backends emit
   `case <int_value>:` instead of the variant tag. The
   checker rejects duplicate integer values, out-of-type
   overflow, missing-wildcard non-exhaustiveness, and
   cross-kind patterns (variant-on-int + int-on-enum).
   Five new lib tests pin the surface.
4. **Simple generics** — *phase 1 started 2026-05-20.* **Done so
   far:** `Type::Param(String)` in AST; `Function.type_params` carries
   declared type parameters; parser accepts `fn name<T1, T2>(…)`
   syntax and resolves identifier-typed positions against the
   in-scope type-param set so a bare `T` parses as `Type::Param(T)`
   rather than `Type::Struct(T)`; `Type::Display` prints param names
   verbatim. The checker surfaces a clear "T1.4 phase 2:
   monomorphization is still in progress, specialize manually for
   now" diagnostic on any generic function declaration. One new lib
   test (`generic_function_syntax_parses_but_gated`) pins the gate
   shape. **Phase 2 pending**: call-site type-arg inference, body
   type-check with substituted T, monomorphization (one specialized
   `TypedFunction` per (fn × type-args) seen at call sites),
   name-mangling so backends emit distinct symbols per
   specialization, recursion-safe specialization queue.
5. **Interfaces + bounded generics** — *phase 1 done 2026-05-20.*
   `interface Cmp { fn cmp(self, other: ref Self) returns i64; }`
   plus `implement Cmp for Point { … }` and the bound form
   `fn min<T>(a: T, b: T) returns T where T is Cmp`. No interface
   inheritance, no default methods, no associated types in v1.
   **Done so far:** `InterfaceDecl`, `InterfaceMethod`, `ImplDecl`,
   `WhereClause` in AST; `Program.interfaces`, `Program.impls`,
   `Function.where_clauses` carry the surface declarations; lexer
   recognizes `interface`, `implement`, `where`, `is`; parser
   accepts top-level `interface Name { fn m(…) -> T; … }`,
   `implement Iface for Type { fn m(…) -> T { … } … }`, and
   `where T is C, U is D` clauses after the return type. The
   checker surfaces clear "T1.5 phase 2: dispatch / bounded-generic
   checking is still in progress, specialize manually" diagnostics on
   any interface decl, impl block, or `where` clause. Three new lib
   tests (`interface_decl_parses_but_gated`,
   `implement_for_parses_but_gated`, `where_bound_parses_but_gated`)
   pin the gate shape. **Phase 2 pending**: interface-method
   signature verification against impl methods, vtable layout +
   dispatch (static-monomorphized first, dynamic later if needed),
   `where T is C` constraint propagation through the
   monomorphization queue (depends on T1.4 phase 2), `Self` type
   inside interfaces, conflict detection on overlapping impls.

### Tier 2 — Error handling + safe absence + custom RAII (built on Tier 1)

6. **`Option<T>` + `Result<T, E>` + `try` keyword** —
   `enum Option<T> { Some(T), None }`,
   `enum Result<T, E> { Ok(T), Err(E) }` shipped as built-in enums.
   `try expr` unwraps `Ok` / `Some` or short-circuits the current
   function with `Err` / `None`. Every fallible call's failure edge
   stays explicit in the source — no exceptions, no `?` operator,
   no hidden control flow. Implementation depends on #3.
7. **User-defined `Drop` interface** —
   `interface Drop { fn drop(mut self); }` plus
   `implement Drop for FileHandle { fn drop(mut self) { close(self.fd); } }`.
   `Drop` is the single compiler-recognized "magic" interface: when
   a type implements it, the auto-drop pass at scope exit invokes
   the user's `drop` method (in addition to recursively dropping
   affine fields). Compile-time guarantees stay: each value's `drop`
   runs exactly once, and the value is unusable after the call (the
   existing affine-move bookkeeping carries through). Lets user types
   wrap file descriptors / sockets / raw FFI pointers with the same
   leak-free behavior `Vec<T>` / `Mutex<T>` already get. Depends on
   #2 + #5.

### Tier 3 — Collections (depend on generics + interfaces)

8. **`Map<K, V>`** — hashed key-value store. Requires `K: Hash + Eq`
   (two built-in interfaces shipped alongside). Keys are values (no
   reference keys).
9. **`Set<T>`** — sibling to `Map`; thin wrapper requiring `T: Hash + Eq`.
10. **Slice type `&[T]`** — first-class read-only view over
    `Vec<T>` / `[T; N]`. Unblocks generic helpers like
    `fn sum(xs: &[i64]) -> i64` and range subslicing (`&xs[lo..hi]`).

### Tier 4 — Ergonomics & expressiveness

11. **`format!("x = {}, y = {}", x, y)`** — compiler-recognized
    intrinsic (NOT a user-defined macro system). Lowers to a chain of
    string-concat builtins at parse time.
12. **Method-call syntax `xs.len()`, `p.dist()`** — parser sugar for
    `len(xs)` / `Point::dist(p)`. No new lookup machinery; resolves to
    the existing free-function table during type-check.
13. **Closures (Copy-capture only)** —
    `let inc = with x: i64 do x + 1;`. Captures by value only and
    only Copy types — same rule the existing `task` body uses.
    Required for collection methods (`xs.map(with x do x * 2)`).
14. **Block expressions** — `let r = { let t = compute(); t + 1 };`.
    Already half-supported by the parser; finish + lower properly so
    `match` arms / closures have a natural body shape.
15. **Type aliases + `const`** — `type Coord = (i64, i64);`,
    `const PI: f64 = 3.14159;`. Cheap aliasing for readability; v1
    rejects recursive aliases and non-Copy `const`s.
    *Type aliases done 2026-05-20.* `type Name = Target;`
    top-level declarations parse via new `type` keyword;
    AST `TypeAlias { name, target }` lives on
    `Program.type_aliases`. The checker resolves each
    alias's target (recursively unfolding alias chains
    `Outer → Middle → Inner → i64`) into a fully concrete
    type. A DFS-based cycle detector rejects recursive
    aliases (`type A = B; type B = A;`) with a clear
    "recursive type alias" diagnostic. After resolution,
    a substitution pass walks every Type position in the
    program (function signatures, struct fields, const
    types, let/return/etc. in bodies) and replaces
    `Type::Struct(alias_name)` with the alias's resolved
    target. This means backends never see the alias name
    — they get a clean concrete type tree. Aliases that
    point at enums resolve correctly because the alias
    pass runs *after* `resolve_enum_types_in_program`
    has rewritten `Struct(Color) → Enum(Color)` inside
    alias targets too. Seven new lib tests pin: primitive
    alias, tuple alias, enum alias, alias chain,
    recursive alias rejection, duplicate alias rejection,
    struct-collision rejection. One new format
    round-trip test. *`const` done 2026-05-20.* `const NAME: T = literal;`
    top-level declarations work end-to-end: lexer keyword
    `const`, AST `ConstDecl` on `Program.consts`, parser
    `parse_const_decl`, formatter emission via
    `format_const_decl`. The checker validates Copy-only
    scalar types (i64/i32/.../f64/bool), rejects non-literal
    initializers (arithmetic + calls land in a later phase),
    catches duplicate names + collisions with structs/enums/
    functions, and folds unary-minus over a literal so
    `const MIN: i64 = -100;` works. Const bindings get
    seeded into the function env's root scope with
    `is_const: true` and `constant: Some(TypedConst::…)`;
    Var-resolution substitutes the literal in-place so the
    C/LLVM backends never see an unbound `v_NAME` symbol.
    Function-scoped `let NAME` cleanly shadows the const
    (the local lives in a deeper scope; `is_const: false`).
    Eight new lib tests + one new format round-trip pin the
    feature. **Pending**: type aliases (`type Coord = (i64,
    i64);`), const initializers with simple arithmetic
    (`const TAU: f64 = 2.0 * 3.14;`), const string literals,
    const struct/tuple values.

### Tier 5 — Verifier precision (nice-to-have, not blocking)

16. **`forall` quantifiers in invariants** —
    `invariant forall i: 0 <= i < len(xs), xs[i] >= 0;`. Encodes
    array-wide properties without manual unrolling. SMT support is
    already in z3; the surface needs parser + checker plumbing.
17. **Opaque-call return refinement** — drop the "`prove foo(args) > 0`
    requires `ensures` on `foo`" caveat by tracking a richer return-
    fact set. Lower priority than user-visible language surface.

### Tier 6 — Concurrency widening (mostly parameterization, cheap after #4)

18. **Parametric `Mutex<T>` / `Guard<T>`** — drop the i64-payload
    restriction once generics land.
19. **Parametric `Channel<T>`** — currently integer + bool only.
20. **`RwLock<T>` / `Barrier` / `CondVar`** — broader sync primitives.

### Tier 7 — Backend / runtime polish

21. **Windows `OMP_NUM_THREADS` lookup** — current Win32 parallel-for
    hardcodes N=4 workers; plumb a runtime query through the existing
    `WinParArg` (the outlined fn already reads `nt` from it).
22. **`break value` / labeled `continue`** — loop-as-expression form.
    Memorable, no new types, finite parser/checker work.

### Tier 8 — Deferred (revisit after the language feature set is complete)

23. **`async` / `await`** — adds a second control-flow story on top of
    `task`/`join`. Multi-week; skip until v1 surface stabilizes.
24. **Cranelift backend** — fast JIT path independent of LLVM.
25. **Direct-asm targets (x86_64-linux first)** — teaching path +
    tiny-target option.

### Explicitly out of scope (do not propose without consensus)

- **Inheritance, method overriding, virtual dispatch.** Use composition
  + interfaces instead.
- **Exceptions, `try`/`catch`, panicking propagation.** Use `Result` + `?`.
- **C++-style templates / specialization.** Generics are
  parameter-only.
- **First-class lifetimes / borrow checker beyond function params.**
  Affine ownership is the safety mechanism.
- **Macro system.** `format!` is a single compiler intrinsic, not a
  user-extensible macro.
- **Operator overloading on user types** in v1 (might lift later).

## Caveats / out-of-scope (intentionally not on the TODO list)

These are design decisions or working-as-intended trade-offs — not
gaps to close. They appear in Known Issues with the "*Working as
intended*" tag where applicable.

- **Cross-compilation.** `intentc` bakes the host's `target_os` /
  `target_arch` into the emitted artifact (e.g., `SYS_futex`
  number, threading dispatch, `-lsynchronization` link flag). A
  `--target=` flag is out of scope for v1 — single-host-target is
  the operating assumption.
- **`!cond` post-loop fact dropped when body can `break`.** Adding
  it would be unsound under a break; the verifier conservatively
  omits it. Working as intended.
- **References are second-class.** `&T` / `&mut T` only as function
  parameter types; not as returns, let-bindings, or aggregate
  elements. Rust-style first-class references are explicitly out
  of scope for v1.
- **`prove foo(args) > 0;` requires `ensures` on `foo`.** Calls to
  functions without ensures fall back to "unsupported" since the
  solver has no fact about the return value. Working as intended —
  declare `ensures` to enable inline-call proofs.
- **`INTENTC_NO_VERIFY=1` bypass.** Skips every SMT round-trip.
  Useful for fast dev iteration; do not set in CI (a violated
  `ensures` won't surface). Runtime safety guards stay in place.

---

## Known issues

These are caveats present in the current implementation. Each links to
the TODO that would resolve it (or notes that the trade-off is
intentional). Resolved entries are deleted, not struck through —
TODO.md keeps the history.

### Backend / codegen
- **Full concurrency surface now flows through SSA on both backends.** `Atomic`, `Mutex`/`Guard`, `Channel`, `parallel for`, and `task`/`join` all use the SSA path; only multi-block task bodies and other shape-recognizer mismatches fall back via `EmitError`. (No active TODO in this row — kept here to document the milestone.)
- **No cross-compilation.** intentc bakes the host's `target_os` / `target_arch` into the emitted artifact (e.g., `SYS_futex` number, threading dispatch, link flags). A `--target=` flag is out of scope for v1; both C and LLVM backends emit code that only links on the same OS intentc was built for. *Trade-off, not a bug — flag separately if needed.*
- **Parallel-for thread count is hardcoded N=4 on Windows.** The Win32 fan-out doesn't read `OMP_NUM_THREADS` (or query `GetSystemInfo`) — it always spawns 3 worker threads plus the calling thread. *Trade-off: keeps the LLVM IR portable across LLVM versions without depending on getenv linkage. A future revision can plug in a runtime lookup helper; the per-thread `WinParArg` struct already carries `nt` so the outlined fn would not change.*

### Verifier / SMT
- **Natural-exit `!cond` post-loop fact omitted when the body can `break`.** Would be unsound; the verifier conservatively drops the fact. *Working as intended.*
- **`prove foo(args) > 0;` only works if `foo` has `ensures`.** Calls to functions without ensures fall back to "unsupported" since the solver has no fact about the return value. *Working as intended — declare ensures.*
- **Bare `let inner = xs[i]` is rejected for `Vec<non-Copy>`.** Direct indexing would alias the owner's slot and double-free; the checker emits a clear hint pointing users at the new `clone_at(&xs, i)` builtin that returns an owned deep-clone of the slot. *Working as intended — clone_at is the explicit opt-in for non-Copy slot reads.*

### Language surface gaps
- **No mutable references to atomics-as-payloads.** Workaround: pre-extract scalars before spawning a task. *Tracked indirectly by future affine-rules work.*
- **References are second-class.** `&T` / `&mut T` only as function parameter types; not as returns, let-bindings, or aggregate elements. *Working as intended for v1 — Rust-style first-class references are explicitly out of scope.*
- **Early `return` from inside a consuming `for x in xs` body leaks the outer Vec buffer.** The post-loop shallow-free is emitted inline by the for-iter backend code and runs only on natural completion / break. `return` skips it. Workaround: use `break` to exit the loop, then return after. *Tracked as a structural-rewrite TODO: introduce a `TypedStmt::ForIterShallowFree` variant and emit it at every Return inside the consuming loop body.*

### Tooling
- **`INTENTC_NO_VERIFY=1` skips every SMT round-trip.** Useful for fast iteration; do not set in CI — a violated `ensures` won't surface. Runtime safety guards stay in place. *Working as intended.*

---

## Update protocol for this file

When you finish a unit of work, update STATUS.md in the same commit:

- **Feature added** → add a bullet to the matching subsection. Keep the wording terse; the README has the long form.
- **TODO closed** → delete it from the TODO list above; if it had a Known Issues entry, delete or rewrite that entry too.
- **TODO added** → insert at the priority position; cross-reference any related Known Issues entry.
- **Issue discovered** → add to Known Issues; if a fix is planned, also add a TODO and link them.
- **Issue resolved** → delete the entry; do not strike through (`~~`). TODO.md preserves the history if you need it.
- **Test totals shifted** → update the header line.
- **Date roll** → update `Last updated:` to today.
