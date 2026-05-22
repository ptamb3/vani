# vāṇī (वाणी) — Project Status

> The project was renamed from `future_compiler` to **VANI** (वाणी, Sanskrit for "speech")
> on 2026-05-21. "VANI" expands to *Verbose Alternative Natural Interface* — the
> design goal is code that reads like speech, not punctuation.

> Single-page snapshot of what the compiler does today, what's queued
> next, and known issues. Update this file whenever a feature lands,
> a TODO is added/closed, or an issue is resolved/discovered.
> Cross-reference [README.md](README.md) for the language tour and
> [TODO.md](TODO.md) for the canonical work list.

**Last updated:** 2026-05-22
**Test totals:** 779 lib + 47 end-to-end tests passing. (Win32 LLVM dispatch adds 4 host-gated tests that fire on Windows hosts only — futex/WaitOnAddress, CreateThread for tasks, plus the new CreateThread fan-out parallel-for tests in tree-LLVM and SSA-LLVM.)

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
