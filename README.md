# vāṇī (वाणी) — VANI

<p align="center">
  <img src="vani_logo.png" alt="vāṇī logo" width="240">
</p>

**Verbose Alternative Natural Interface — code like you speak.**

Pronounced **vaa-NEE** (Sanskrit *vāṇī* — long-a, retroflex-n, long-i;
stress on the second syllable). वाणी is the Sanskrit word for *speech*,
*voice*, or *language itself*.

## Philosophy

vāṇī is a small systems language **inspired by Rust and C/C++** in
its semantic model — static types, affine ownership, references with
explicit `mut` / `ref` discipline, compile-time monomorphization,
direct LLVM / C code generation, and predictable cost — but with a
surface that **reads as close to natural language as a strict compiler
will let it**.

The goal is to let users *express the same program* in whichever
spelling reads most naturally to them, without weakening the
language's correctness or performance guarantees. Three concrete
commitments make that work:

1. **Same execution model as Rust / C / C++.** The output is
   fully deterministic. The same source compiles to the same LLVM IR
   / C, with the same runtime behavior on a given target, every time.
   No interpreter, no garbage collector, no surprise allocator, no
   hidden control flow. `prove` / `ensures` / `requires` constraints
   are discharged at compile time by Z3-backed SMT; runtime cost is
   what you'd get in idiomatic Rust.
2. **Multiple keywords + aliases let the writer choose tone.** Most
   constructs accept more than one spelling. `let` and `assign` both
   declare a binding. `return`, `give`, `give_back`, and the two-word
   `give back` are all the same. `pub` and `public` are interchangeable.
   `module` accepts `mod`. **Devanagari surface (Phase 1)** further
   aliases the same tokens to Sanskrit (`कार्य`, `पुनरागम`, `माना`, …),
   Hindi (`फ़ंक्शन`, `लौटाओ`, …), and Marathi (`परत`, …) so a program
   can read like Indo-Aryan prose without the lexer caring which
   form was used. Per-file language purity (closure #237) lets a
   project opt into a single language and have the checker reject
   out-of-language identifiers.
3. **Keywords replace punctuation where it matters most.** Where Rust
   reaches for `&`, `&mut`, `::`, `?`, `<T>`, `'a`, vāṇī uses the words
   you'd say out loud — `ref`, `mut ref`, `module foo::bar`, `try`,
   `<T>` (kept for generics), and `where T is Trait`. The result reads
   left-to-right at speaking pace without losing the strictness of the
   Rust semantic model.

The compiler core is in Rust. Python is fine for experiments, AI
orchestration, and testing, but Rust is the better default for a
compiler that must be fast, memory-safe, deterministic, and close to
ABI / native code generation.

## Feature set (closures #1–#305)

vāṇī today is a working systems language with the following shipped
features. Surface that **reads natural-language** sits on top of a
semantic model **borrowed from Rust** and a code-generator that
**emits LLVM IR or C** with no runtime layer in between.

**Type system + memory:**
- Scalars (`i8`–`i64`, `u8`–`u64`, `f32`/`f64`, `bool`); fixed-size
  arrays `[T; N]`; heap `Vec<T>`; tuples (2–4 elements); structs (up
  to 64 fields); enums (with payloaded variants); type aliases;
  `const` bindings with literal initializers.
- `Str` (borrowed string) and `OwnedStr` (heap, affine, produced by
  `+` concat).
- **Affine ownership.** `Vec`, `OwnedStr`, `Atomic`, `Mutex`, `Guard`,
  `Channel`, `Task` are all single-owner; the checker tracks moves +
  partial moves and the backends emit deterministic destructors at
  scope exit. User-defined `Drop` interface lets a struct hook into
  the scope-exit flow.
- **References** are second-class keyword-first: `ref T` / `mut ref T`
  in parameter position, `ref x` / `mut ref x` at call sites, with
  aliasing rejected at compile time.

**Generics + dispatch:**
- **Monomorphized generics** (`fn id<T>(x: T) -> T`) — specialized per
  call-site concrete type.
- **Interfaces** (`interface Show { … }` + `implement Show for T { … }`)
  with **static dispatch** by default and **dynamic dispatch via
  `dyn Iface`** (16-byte fat pointer) for heterogeneous collections.
  Bounded generics: `fn min<T>(a: T, b: T) -> T where T is Cmp`.

**Control flow + verification:**
- `if`/`else`/`else if`, `while`, `for i from lo to hi`, `for x in xs`,
  `break` / `continue`, `match` with payloaded-variant destructure,
  `try` keyword for early-return on Option/Result-like enums.
- **SMT-discharged** `requires` / `ensures` / `assert` / `prove` /
  `invariant` via Z3. Bounds / divisor / shift / overflow checks
  elided when proven safe.

**Parallelism + concurrency:**
- `parallel for` with verified race-freedom + reductions
  (`reduce x with +`, `*`, `&&`, `||`, `&` / `|` / `^`, `min`, `max`).
- `task <name> { … }` / `join <name>;` with real pthread (Linux)
  and CreateThread (Windows) backing. `Atomic<T>` for shared
  counters, `Mutex<T>` + `Guard<T>` for critical sections,
  `Channel<T, N>` for queues.
- `Condvar` — `condvar_new` / `condvar_wait(ref cv, mut ref g) /
  condvar_wait_timeout / notify_one / notify_all`. Pairs with
  `Mutex` + `Guard` for "wait until predicate" patterns. ✅
  AFFINE (closure #292). Tree-C + SSA-C use shared runtime
  helpers (futex/WaitOnAddress/spin-yield); tree-LLVM uses
  inline IR; SSA-LLVM falls back to tree-LLVM. See
  [examples/condvar.vani](examples/condvar.vani).

**Namespaces + modules:**
- `module foo { … }` (inline + nested + deep `a::b::c::Item` paths).
- Per-item `pub` and `pub(kosh)` visibility.
- `use foo::bar [as baz];`, `use foo::{a, b};`, `use foo::*;` (direct
  children only), `pub use foo::bar;` re-exports (transitively
  resolved), module-local `use` inside `module { }` bodies, orphan
  rules on `implement Iface for T`, collision diagnostics with
  precise `use … as …;` hints.

**Backends + tooling:**
- LLVM IR (default) — tree path + SSA path with automatic fallback.
- C — same dual-path arrangement.
- `intentc check` (typecheck + SMT), `emit` (lowered source), `run`
  (compile + execute), `build` (AOT to native binary), `fmt`
  (formatter with round-trip + comment preservation), `ast` (AST
  dump), LSP integration.
- Cross-backend parity test pins identical stdout + exit code on
  every example under both backends.

**Multi-file projects:**
- `use "path.vani";` for file-level inclusion; cycle detection;
  diagnostics resolve to the original file:line via a `FileMap`.

**Since closure #258 (2026-05-26 → 2026-05-27):**
- **FFI v1–v8** — `extern "C" fn` declarations (#269), `--link-with`
  linker flag (#270), extern call-site checker (#271), extern codegen
  with mangled symbols (#272), struct-by-value rejection with `ref T`
  hint (#273), linker-discovery polish (#274), FFI callbacks via
  `Type::FnPtr` (#279), System V x86-64 small-struct return lowering
  for FFI (#288). Net: `qsort`-style callbacks and libc string /
  math interop work end-to-end without a runtime shim.
- **vani.toml manifest** — hand-rolled minimal-TOML parser
  (`src/manifest.rs`), `find_manifest` parent-walk, `[package].entry`
  auto-discovery in `intentc build|run|check` (#280); v2 added
  `[deps]` inline-table for multi-file dependency wiring (#287).
- **Generic struct + enum declarations** — `enum Result<T, E> { Ok(T),
  Err(E) }` / `struct Pair<A, B> { … }` (#281). `Type::Apply { name,
  args }` for parse-time generic instantiations; mangled names like
  `Result__Vec_I64___AllocError` flow through the monomorphizer.
- **Mixed-payload enums** — variants with different payload types
  share one enum on both backends (#283). C uses a tagged union
  (`union { Type0 v_Ok; Type1 v_Err; }`); LLVM uses `[N x i8]` byte
  buffer + per-variant bitcast.
- **`try_vec(n) -> Result<Vec<i64>, AllocError>`** — fallible
  allocation builtin emitting malloc + null-check + Result
  construction (#284). Programs handle OOM gracefully.
- **Attribute syntax + `#[bounded(N)]`** — first attribute in the
  language (#286). New `#` token + parser. Tree-LLVM uses
  thread-local globals + per-Return decrement (#289); SSA-LLVM
  mirrors the pattern (#290). C emits a thread-local counter with
  GCC `__attribute__((cleanup))` for the decrement.
- **Nested arrays `[[T; N]; M]` / `[Vec<T>; N]`** — array-element
  Copy restriction lifted, `clone_at(ref arr, i)` extended to arrays,
  per-slot per-field drops including struct fields (#291 Phases 1–4).
  Tree-LLVM `len` of a Vec rvalue (e.g. `len(clone_at(ref xs, i))`)
  now spills to alloca, GEPs `.len`, loads.
- **Prelude injection** — `Option<T>`, `Result<T, E>`, `AllocError`
  injected at AST level (NOT source-prepend, which would shift
  diagnostic line numbers) so user programs use them without `use`
  (#282).
- **Match on `f64` / `f32` scrutinees** — `Pattern::Float(f64)` AST
  variant + `check_match_float` desugar to nested IfExpr; clear
  diagnostics for missing wildcard, duplicate literals, NaN-in-pattern
  (#278).
- **Other closures #275–#277** — parallel-for purity hole closed in
  reduction RHS; DynCoerce non-Var hoist via synthetic Block expr;
  `let _ = make()` discard of fresh struct value frees heap fields.

See [STATUS.md](STATUS.md) for the closure-by-closure history and
[TODO.md](TODO.md) for what's queued. The full Roadmap (small +
multi-session items) is in the README's *Roadmap* section below.

## Memory safety & concurrency model

vāṇī treats **memory and concurrency bugs as compile-time errors**.
The runtime is meant to be boring: no garbage collector, no event
loop, no allocator-dependent fault injection, no reference counting,
no surprise rescheduling. Everything that would be a "this might
crash at 3 AM in production" bug in a less strict language fails the
type checker on the developer's laptop.

### What's caught at compile time

| Bug class | Caught at compile time | How |
|---|---|---|
| **Heap leak** ("forgot to free") | ✅ | Affine ownership. Every heap-owning binding (`Vec`, `OwnedStr`, `Atomic`, `Mutex`, `Guard`, `Channel`, `Task`, struct with heap fields) has exactly one owner. The codegen emits `free` / per-field drop at scope exit deterministically. There is no `forget()` equivalent. |
| **Double-free** | ✅ | Move tracking. After `let y = x;` (where `x: Vec<i64>`), `x` becomes unreadable; the compiler emits a "value 'x' was moved; cannot use after move" diagnostic at the next reference. Drop fires exactly once on the new owner. User-defined `Drop` is also single-fire. |
| **Use-after-free** | ✅ | Same affine machinery + second-class references. A `ref T` / `mut ref T` parameter can only be a parameter — it can't be stored in a struct, returned from a function, or held across `await` (there is no `await`). The borrow can never outlive the owner. |
| **Dangling reference** | ✅ | References are second-class. Functions cannot return references; struct fields cannot be references; references cannot be stored in arrays / `Vec`. The compiler rejects any attempt syntactically. |
| **Aliasing mutable + immutable** | ✅ | A `mut ref T` borrow rejects every subsequent shared `ref T` on the same value (and vice-versa) for the scope of the borrow. Diagnostic: "value 'v' is borrowed mutably; cannot also share-borrow". |
| **Data race in `parallel for`** | ✅ | The effects checker walks the loop body and rejects observable side effects: `print`, calls to impure functions, non-Copy moves into the body, indexed writes on captured arrays / `Vec`s. **As of closure #259**, captured Copy-typed mutations are also caught — `total = total + i;` on a binding declared OUTSIDE the body errors with "mutates captured variable 'total' without declaring it as a reduction" and points the user at `reduce` or `Atomic<T>`. Body-local lets remain free to mutate (per-iteration, not shared). Atomic / Mutex captures must be via `ref`. |
| **Data race in `task`** | ✅ | `task` captures are Copy-only by default — affine handles (Vec, Atomic, Mutex, Guard, Channel) can't ride into the thread by value. Shared state goes through `Atomic<T>` (lock-free, seq-cst) or `Mutex<T>` + `Guard<T>` (RAII unlock at scope exit). |
| **Unjoined task** ("thread leak") | ✅ | `Task` is affine. The compiler tracks each handle and requires a matching `join name;` before the handle's scope ends, even on early-return paths. Double-`join` is also rejected. |
| **Forgotten mutex release** | ✅ | `Guard<T>` is affine — taking the lock returns a `Guard` that **must** drop, and Drop emits the unlock. The borrowed inner `T` lives only as long as the Guard; the compiler rejects keeping the inner reference after the Guard drops. |
| **Integer overflow / underflow** | ✅ (where SMT proves) | The bounds-elision pass keeps `if (UB-check)` guards by default and elides only when Z3 proves the operation is in-range. `INTENTC_NO_VERIFY=1` keeps the guards in place. |
| **Array / Vec out-of-bounds** | ✅ (where SMT proves) | Same elision pass on `Index` / `IndexAssign`. Guards stay in place when SMT can't discharge the obligation. |
| **Divide / shift / mod by zero** | ✅ (where SMT proves) | Same. |
| **`assert` / `prove` / `requires` / `ensures` / `invariant`** | ✅ | Discharged by Z3 at check time. `prove` is the strict form (must hold); `ensures` is verified at every return path; `invariant` at loop entry, body, and exit. |

### What runs (without you reaching for `unsafe`)

vāṇī has **no `unsafe` block** at all. Every operation in source is
type-checked + affine-tracked. The compiler doesn't trade safety for
ergonomics anywhere — including for raw pointer arithmetic, mmap,
syscalls, or FFI. Those are out of scope for v1.

### vāṇī vs Rust — ownership at a glance

vāṇī uses **the same move-by-default model as Rust**. The goal is
that users live with `move` semantics by construction; explicit
`clone()` only happens when the user types it (and the compiler
makes it clear when that's needed). There is **no implicit clone
anywhere** in the language.

| Property | Rust | vāṇī |
|---|---|---|
| Primitive scalars (`i64`, `bool`, …) | Copy | Copy |
| Borrowed string view (`&str` / `Str`) | Copy (pointer) | Copy (pointer) |
| References (`&T` / `&mut T` vs `ref T` / `mut ref T`) | Copy | Copy (second-class, param-only) |
| Heap string (`String` / `OwnedStr`) | Move (affine) | Move (affine) |
| Heap vector (`Vec<T>` / `Vec<T>`) | Move (affine) | Move (affine) |
| Fixed array (`[T; N]`) | Copy if `T: Copy`, else Move | Affine (Move) always — explicit |
| Struct (every field Copy) | Copy if `#[derive(Copy)]` | Copy automatically (no derive needed) |
| Struct (any affine field) | Move | Move |
| Enum (every payload Copy) | Copy if derived | Copy automatically |
| Enum (any affine payload) | Move | Move |
| `Atomic<T>` / `Mutex<T>` / `Channel<T, N>` | Affine via lifetime / `Arc` | Affine, single-owner — no `Arc` equivalent |
| Thread handle (`JoinHandle` / `Task`) | Affine; must `join` or detach | Affine; **must `join`** (no detach in v1) |
| Implicit clone anywhere? | Never | Never |
| Explicit `.clone()` cost | Visible at the call site | Visible at the call site |

Two practical takeaways:

- **Reach for `ref` first, `clone()` last.** If a function only needs
  to *read* a `Vec<T>`, declare the parameter as `xs: ref Vec<T>` and
  call it as `f(ref xs)`. The borrow is cheap (pointer-sized) and the
  caller keeps ownership — no copy, no clone, no diagnostic. If the
  callee needs to mutate, use `mut ref Vec<T>` + `f(mut ref xs)`. The
  same convention works through struct fields with `ref t.field`.
- **Auto-borrow does the obvious thing.** Comparing two `OwnedStr` /
  `Vec<T>` operands via `==` or feeding an `OwnedStr` to a function
  that wants `Str` auto-borrows the operand — the binding stays
  usable on the next line. No silent clone.

For deep-copying a single `Vec<T>` slot whose element type is
non-Copy (e.g. `Vec<OwnedStr>`), use the explicit builtin
`clone_at(ref xs, i)`. There is no implicit pathway.

If you write code that *requires* a clone to compile — say,
two threads both need their own copy of a `Vec` — the diagnostic
will point at the consume site with the binding's earlier move
location and the suggestion to either restructure or call
`clone()` explicitly. The compiler never picks the clone for you.

### Smart-pointer primitives — Rust / C++ comparison

Rust ships `Box<T>`, `Rc<T>` / `Arc<T>`, `RefCell<T>`, and `Weak<T>` as
distinct types for different memory-management patterns. C++ has
`unique_ptr`, `shared_ptr`, and `weak_ptr` for the same patterns.
**vāṇी ships none of these.** Each of the use cases is either covered
by an existing primitive or **structurally avoided by the type system**:

| Rust / C++ tool | What it solves | vāṇी's approach |
|---|---|---|
| `Box<T>` / `unique_ptr<T>` | Single-owner heap allocation | `Vec<T>` and `OwnedStr` already heap-allocate and own. There is no free-form `Box<T>` for arbitrary T — recursive data structures use index-based references into a `Vec` (see *Basic data structures* below). |
| `Rc<T>` / `Arc<T>` / `shared_ptr<T>` | Reference-counted shared ownership | **Not available by design.** Shared ownership is unrepresentable in the type system. Producer / consumer parallelism uses `Channel<T, N>`. Shared mutable state across threads uses `Atomic<T>` references or `Mutex<T>` + `Guard<T>` — borrowed (not cloned) into each thread. |
| `RefCell<T>` (interior mutability) | Mutate through a shared reference at runtime | **Not available by design.** vāṇी has no runtime borrow-checker — every aliasing rule fires at compile time. The need is mitigated by `mut ref T` parameters + mixed-place assignment (`xs[i].field = v;` writes through an index into a struct field in one statement). |
| `Weak<T>` (cycle breaker) | Non-owning back-reference to break `Rc`/`Arc` cycles | **Unnecessary.** No `Rc`/`Arc` means no cycles can form. Single-owner affine types + second-class references produce a strict ownership tree — there is literally no way to construct a cyclic ownership graph in the type system. |

### What about cyclic data structures?

Graph-like data (parent ↔ child, observer pattern, doubly-linked list)
typically needs cycles in languages that have shared ownership. In
vāṇी the idiom is **indices into a `Vec`**:

```vani
struct Node {
  value: i64,
  parent: i64,    // index into nodes[]; -1 for root
  children: Vec<i64>,  // indices into nodes[]
}

fn add_child(nodes: mut ref Vec<Node>, parent_idx: i64, value: i64) -> i64 {
  let new_idx: i64 = len(nodes) as i64;
  let _ = push(mut ref nodes, Node {
    value: value,
    parent: parent_idx,
    children: vec(),
  });
  // Update parent's children list — borrow + mixed-place assign.
  // (Sketch — actual API needs a helper since you can't take
  // two mut borrows of the same Vec simultaneously.)
  return new_idx;
}
```

This trades the "ergonomic graph node" for:

- **No cycles by construction** — a Node holds indices, not pointers; nothing the verifier needs to prove about lifetimes.
- **Cache-friendliness** — all Nodes live in one contiguous Vec.
- **Cheap clone / serialize** — Vec<Node> is a flat buffer with no internal heap pointers (when fields are Copy).
- **Compile-time bounds checks** — the SMT layer can prove `idx < len(nodes)` for many patterns and elide the runtime guard.

The trade-off is **less ergonomic for tree-traversal-heavy code** —
parent pointer chases become index lookups. For graph algorithms
that fit naturally on a Vec (BFS, DFS, dependency graphs, ECS-style
arrangements) the index pattern is often *more* idiomatic than the
`Rc<RefCell<Node>>` shape Rust would use.

`dyn Iface` (closure #220–#228) covers the "heterogeneous collection
without enumerating variants" use case that often pushes Rust users
toward `Box<dyn Trait>`. vāṇी's `Vec<dyn Iface>` is a vector of
fat pointers (16 bytes each: vtable + data pointer); no `Box` needed.

### What's NOT in the language (deliberate)

- **No garbage collector.** Affine ownership + deterministic Drop
  cover what GC would cover, without the unpredictable pause.
- **`async` / `await` / event loop — queued.** Compiler-lowered
  state machines on an arena are the canonical path (NOT Rust's
  `Pin` / self-referential approach, which stays non-compliant
  under affine; see [TODO.md](TODO.md) *Async / asyncio*). Today's
  concurrency in vāṇī is
  threads (`task` + `join`) plus shared-state primitives (`Atomic`,
  `Mutex`, `Channel`). The user gets thread-safe code by construction
  — the checker rejects the source patterns that would race —
  without the function-coloring tax of `async`. Async I/O can be
  added later if a clear win shows up; for now, blocking I/O through
  a thread pool is the model.
- **No reference counting** (no `Rc` / `Arc` equivalent). Single-owner
  affine ownership means cycles can't form; there's nothing for an
  Rc to count.
- **No `unsafe` escape hatch.** Every operation goes through the
  checked surface.
- **No exceptions / no stack unwinding.** Errors are values via
  payloaded enums (`Option`-like / `Result`-like) and propagated with
  `try`. `assert` triggers a deterministic `abort()`.

### Examples — what the compiler rejects

These programs all **fail to compile**. The diagnostic text below
each is what the user actually sees today (test-pinned in
`src/lib.rs`).

#### Heap leak — impossible by construction

```vani
fn main() -> i64 {
  let v: Vec<i64> = vec(1, 2, 3);
  return 0;
  // v's heap buffer freed automatically at scope exit.
  // No `forget(v)` exists. No way to leak it.
}
```

#### Double-free — rejected via move tracking

```vani
fn main() -> i64 {
  let v: Vec<i64> = vec(1, 2, 3);
  let w: Vec<i64> = v;   // move: w now owns the buffer
  let z: Vec<i64> = v;   // ERROR: value 'v' was moved
  return 0;
}
```

#### Use-after-free — same machinery

```vani
fn consume(xs: Vec<i64>) -> u64 { return len(xs); }

fn main() -> i64 {
  let v: Vec<i64> = vec(1, 2, 3);
  let n: u64 = consume(v);   // move into consume()
  return v[0];               // ERROR: value 'v' was moved
}
```

#### Aliasing — mutable + shared borrow rejected

```vani
fn read(xs: ref Vec<i64>) -> i64 { return xs[0]; }
fn write(xs: mut ref Vec<i64>) -> i64 { xs[0] = 99; return 0; }

fn main() -> i64 {
  let v: Vec<i64> = vec(1, 2, 3);
  let _ = write(mut ref v);
  let _ = read(ref v);   // (OK — sequenced, not aliased)
  // The compiler rejects holding both borrows simultaneously.
  return 0;
}
```

#### Unjoined task — thread leak rejected

```vani
fn main() -> i64 {
  task worker {
    let _ = 42;
  }
  return 0;
  // ERROR: task handle 'worker' was never consumed by `join`
}
```

#### Forgotten mutex unlock — impossible by construction

```vani
fn main() -> i64 {
  let m: Mutex<i64> = mutex_new(0);
  {
    let g: Guard<i64> = mutex_lock(ref m);
    // ... critical section ...
  }   // Guard 'g' drops here; mutex unlocked automatically.
  return 0;
}
```

#### Impure operations inside `parallel for` — rejected

```vani
fn main() -> i64 {
  parallel for i from 0 to 3 {
    print i;   // ERROR: 'parallel for' body cannot contain `print`
               //        (observable I/O is a side effect)
  }
  return 0;
}
```

The same diagnostic fires for calls to impure functions, non-Copy
moves into the body, and indexed writes on captured arrays / `Vec`s.

#### Implicit reduction race — rejected (closure #259)

```vani
fn main() -> i64 {
  let total: i64 = 0;
  parallel for i from 0 to 100 {
    total = total + i;
    // ERROR: 'parallel for' body mutates captured variable 'total'
    //        without declaring it as a reduction; this races at
    //        runtime. Add `reduce total with <op>;` before the body,
    //        or use `Atomic<T>` for a concurrent counter.
  }
  return total;
}
```

The fix is to declare the reduction explicitly:

```vani
fn main() -> i64 {
  let total: i64 = 0;
  parallel for i from 0 to 100
  reduce total with +;
  {
    total = total + i;   // OK: declared reduction, lowered to OpenMP
                         //     `reduction(+: total)` on the C
                         //     backend and atomicrmw on LLVM.
  }
  return total;
}
```

Body-local mutations are still per-iteration and free:

```vani
fn main() -> i64 {
  parallel for i from 0 to 5 {
    let tmp: i64 = i;
    let next: i64 = tmp + 1;   // per-iteration, body-local — fine.
    let _ = next;
  }
  return 0;
}
```

See [examples/memory_safety.vani](examples/memory_safety.vani) for
the seven canonical patterns exercised end-to-end (affine Vec
ownership, explicit clone, push/pop stack, OwnedStr drop, user
Drop, parallel-for reduction, task + join), and
`src/lib.rs::tests` for the negative-test coverage (search for
`expect_err`, `use_after_move`, `double_free`, `unjoined_task`, etc).

### Known gaps (will become checks later)

These are real-world concerns that are **not yet caught at compile
time** — listed honestly so users can plan around them:

- **Recursion stack overflow.** vāṇī doesn't bound recursion depth.
  Deep call chains can blow the OS stack at runtime. Future work
  could add a recursion-depth analysis or a `#[bounded(N)]`
  annotation.
- **Mutex deadlock.** Lock-acquisition-order analysis is not yet
  implemented. Two threads taking the same two mutexes in opposite
  order can deadlock at runtime. Future work: a deadlock-free lock
  ordering pass (Rust doesn't catch this either).
- **Allocator failure (OOM).** vāṇī uses the standard allocator
  (`malloc` / LLVM's allocator); on OOM the program aborts. No
  fallible-allocation API yet.
- **Channel deadlock.** Bounded MPSC channels can wedge if every
  sender + every receiver is blocked. Today this manifests as a
  runtime hang rather than a static error.
- **Integer division by run-time-zero divisor.** When SMT can't
  prove the divisor non-zero, the elision pass leaves the runtime
  guard in (abort on zero). Compile time catches the *provable*
  cases; runtime catches the rest. Same for shift amount validity.

The first two items are the most interesting research directions
for the next year. The rest are likely runtime-aborts-with-clean-
diagnostic forever (which is the same boat as Rust).

## Language Snapshot

```intent
intent "Compute a value with checked constraints";

fn add(a: i64, b: i64) -> i64 {
  return a + b;
}

fn main() -> i64 {
  let answer = add(40, 2);
  prove 2 + 2 == 4;
  assert answer >= 0;
  print answer;
  return 0;
}
```

Read it aloud: *"function add takes a and b of type int-64, returns int-64;
return a + b."* The source reads left-to-right at speaking pace.

### Translation from Rust / C++ punctuation

vāṇī keeps the **semantic model** of Rust / C++ (static types, affine
ownership, monomorphized generics, references with explicit `mut`)
but replaces the punctuation soup with keywords. Most of the column
on the left will compile on the right with identical generated code:

| Rust / C++ | vāṇī | Notes |
|---|---|---|
| `&xs` (shared borrow) | `ref xs` | second-class, param-only |
| `&mut xs` (mut borrow) | `mut ref xs` | same semantics |
| `fn(&self)` | `fn name(self: ref Type)` | receiver is explicit |
| `Vec::with_capacity(n)` | `vec_with_capacity(n)` | free function — no path |
| `impl Drop for T` | `implement Drop for T` | auto-called at scope exit |
| `match Some(x) => …` | `match Opt.Some(x) then …` | `then` instead of `=>` |
| `xs?` (try operator) | `try expr` (keyword form) | Option / Result early-return |
| `loop { … }` | `while true { … }` | one looping construct |
| `for x in &xs` | `for x in ref xs` | borrow at the loop header |
| `mod foo { … }` | `module foo { … }` | `mod` accepted as alias |
| `pub(crate) fn …` | `pub(kosh) fn …` | कोश = "treasure / repository" |
| `pub use foo::bar;` | `pub use foo::bar;` | re-exports through current module |
| `use foo::*;` (glob) | `use foo::*;` | direct children only, non-transitive |
| `let x = …` | `let x = …` *or* `assign x = …` | aliases pick tone |
| `return x` | `return x` / `give x` / `give_back x` / `give back x` | all canonical |

The compiler never silently changes the meaning of source. Aliasing,
ownership transfer, and pure-vs-effectful boundaries are all visible
in the words on screen — surface aliases never relax a check.

### Deterministic output, multiple ways to spell it

Every alias resolves to the same `TokenKind` at the lexer boundary,
so the AST is identical regardless of which spelling the user picked.
The checker, SMT layer, SSA pass, and backends all see the same IR.
Two source files that differ only in alias choice produce
**byte-identical LLVM IR / C** (after `intentc fmt` re-emits to a
canonical form). The same program in English vs Hindi vs Sanskrit
runs the same instructions on the same target.

### वाणी (*vāṇī*) — Devanagari notation (Phase 2 shipped)

Devanagari notation lets the source read in the writer's mother tongue.
The first three languages are **Sanskrit** (*saṁskṛta* — the canonical
Devanagari language and grammar root), **Hindi** (*hindī*), and **Marathi**
(*marāṭhī*). They share the script but use slightly different verbs for
the common keywords. The idea is **alias-based**: every English keyword
gets one or more Devanagari aliases, and the lexer accepts whichever form
the source file uses. A single program may mix forms freely; the compiler
treats them as the same token.

**Phase 1** (closures #235–#237) shipped single-word Devanagari
aliases for the core control / declaration keywords plus multi-word
phrases like `नहीं तो` (else), `के लिए` (for), `सिद्ध करो` (prove) —
fused by a post-lex merger. Per-file language purity lets users opt
into a single language (Hindi-only, Sanskrit-only, Marathi-only,
English-only) via a file header.

**Phase 2** (closures #265–#267) closes the two biggest ergonomic
gaps:

1. **SOV word order.** Indo-Aryan grammar is verb-final
   (postpositions follow the noun). The parser now accepts the
   natural shape `i के लिए 0 से 5 तक { … }` (range for) and
   `X पुनरागम;` / `"x =", x लिखो;` / `cond सुनिश्चित;` /
   `expr प्रमाण;` (return / print / assert / prove with the
   verb at the end). The English keyword-first order still works
   — the SOV detector only fires when the leading token isn't
   a verb-keyword.
2. **3-way alias parity.** Sanskrit / Hindi / Marathi now each
   have a viable form for every previously English-only
   keyword (else `वरना`, mut `परिवर्तनीय`, continue `अग्रे`,
   pub `सार्वजनिक`, module `खण्ड` / `मॉड्यूल`, use `उपयोग`,
   as `यथा`, where `यत्र` / `जहाँ` / `जिथे`, is `अस्ति` /
   `है` / `आहे`, plus interface / implement / methods / try /
   task / join / parallel single-word). Sanskrit-root words
   that work as tatsama (loanwords) in Hindi + Marathi are
   documented as shared across the three.

A pure-Hindi or pure-Sanskrit or pure-Marathi program now reads
top-to-bottom in natural grammar with no English fall-back.

**Still queued**: grammar-consultant refinement pass — the
Phase-2 picks are best-effort and welcome dialect-specific
revision. See TODO.md for the closure-by-closure log.

Romanizations follow **IAST** (International Alphabet of Sanskrit
Transliteration) for Sanskrit and a Hunterian-style transliteration for
Hindi / Marathi where IAST conventions diverge from spoken pronunciation
(e.g. word-final `अ` is dropped in Hindi/Marathi but retained in
Sanskrit). Where a vowel has both forms, the spoken form is shown.

Conceptual sketch of what the same program might look like in each:

```intent
// English
fn add(a: i64, b: i64) -> i64 { return a + b; }

// संस्कृत (saṁskṛta — Sanskrit): verbs from classical Sanskrit grammar
कार्य add(a: i64, b: i64) -> i64 { पुनरागम a + b; }
// kārya add(a: i64, b: i64) -> i64 { punarāgama a + b; }

// हिन्दी (hindī — Hindi): common spoken Hindi verbs
फलन add(a: i64, b: i64) -> i64 { लौटाओ a + b; }
// phalan add(a: i64, b: i64) -> i64 { lauṭāo a + b; }

// मराठी (marāṭhī — Marathi): Marathi verbs
कार्य add(a: i64, b: i64) -> i64 { परत a + b; }
// kārya add(a: i64, b: i64) -> i64 { parat a + b; }
```

The alias table below gives each keyword in script + romanization. Read
the romanization aloud — that's the pronunciation contract for the
language.

| English | संस्कृत (Sanskrit) | हिन्दी (Hindi) | मराठी (Marathi) |
|---|---|---|---|
| `fn` | `कार्य` *kārya* | `फलन` *phalan* | `कार्य` *kārya* |
| `let` | `माना` *mānā* | `मानो` *māno* | `मान` *māna* |
| `return` | `पुनरागम` *punarāgama* | `लौटाओ` *lauṭāo* | `परत` *parat* |
| `if` | `यदि` *yadi* | `अगर` *agar* | `जर` *jar* |
| `else` | `अन्यथा` *anyathā* | `नहीं तो` *nahīṁ to* | `नाहीतर` *nāhītar* |
| `while` | `यावत्` *yāvat* | `जबतक` *jab tak* | `जोपर्यंत` *jopa­ryanta* |
| `for` | `प्रति` *prati* | `के लिए` *ke liye* | `साठी` *sāṭhī* |
| `then` | `तदा` *tadā* | `तो` *to* | `तर` *tar* |
| `ref` / `mut ref` | `दृष्ट्या` *dṛṣṭyā* / `लिखित दृष्ट्या` *likhita dṛṣṭyā* | `देखो` *dekho* / `बदलो` *badlo* | `पहा` *pahā* / `बदला` *badlā* |
| `match` | `मेल` *mela* | `मिलान` *milān* | `जुळवा` *juḷvā* |
| `assert` | `सिद्धम्` *siddham* | `सुनिश्चित` *sunishchit* | `खात्री` *khātrī* |
| `prove` | `प्रमाणयति` *pramāṇayati* | `सिद्ध करो` *siddha karo* | `सिद्ध करा` *siddha karā* |
| `requires` | `अपेक्षते` *apekṣate* | `चाहिए` *cāhiye* | `पाहिजे` *pāhije* |
| `ensures` | `सुनिश्चयति` *sunishchayati* | `निश्चित` *nishchit* | `निश्चित` *nishchit* |
| `parallel for` | `समान्तर प्रति` *samāntara prati* | `समानांतर` *samānāntar* | `समांतर` *samāntar* |
| `task` / `join` | `कार्य` *kārya* / `संयुज्` *saṁyuj* | `काम` *kām* / `जोड़ो` *joṛo* | `काम` *kām* / `जुळवा` *juḷvā* |

Pronunciation guide for the diacritics used in the romanizations:

| Mark | Roman | Sound | Example |
|---|---|---|---|
| ā | long-a | as in *father* | *kārya* = "kaar-yuh" |
| ī | long-i | as in *machine* | *vāṇī* = "vaa-nee" |
| ū | long-u | as in *rule* | *mūla* = "moo-luh" |
| ṛ | retroflex r | rolled tongue tip | *kṛṣṇa* = "krish-nuh" |
| ṇ | retroflex n | tongue against palate | *vāṇī* = "vaa-NEE" |
| ṭ / ḍ | retroflex t / d | tongue curled back | *paṭha* = "pa-tha" |
| ś / ṣ | sh-sounds | as in *shoe* / *bush* | *kṛṣṇa* = "krish-nuh" |
| ñ | palatal n | as in *canyon* (ny) | *jña* = "gya" |
| ṁ / ṃ | anusvāra | nasalizes preceding vowel | *saṁskṛta* = "sun-skrit" |
| ḥ | visarga | soft h-release | *namaḥ* = "nam-ah" |

A short worked example: the project name **वाणी** romanizes to **vāṇī**,
read as "vaa-NEE" — long-a, retroflex-n, long-i. The acronym **VANI**
keeps the same three syllables but drops the diacritics for ASCII use.

The actual keyword mapping will be finalized with grammar consultants for
each language so the verbs feel idiomatic and unambiguous in context.
Mixing scripts in the same file is supported by design — a student can
write the keywords in Devanagari and the identifiers in English, or vice
versa.

Supported today (800 lib + 47 e2e tests passing):

### Types
- Scalars: `i8`/`i16`/`i32`/`i64`, `u8`/`u16`/`u32`/`u64`, `f32`/`f64`, `bool`
  (all `Copy`).
- Strings: `Str` (borrowed C-string, `Copy`, `==`/`!=`/`<`/`<=`/`>`/`>=` via
  strcmp), `OwnedStr` (heap, affine, produced by `+` concat).
- Fixed-size stack arrays `[T; N]` (affine) with `xs[i]` and `len(xs)`.
- Heap-allocated `Vec<T>` (affine) with `vec(...)`, `push` / `set` / `clone`,
  `len`, indexing, `clone_at(ref xs, i)` for non-Copy slot reads. Empty
  `vec()` is supported. `Vec<Vec<T>>` and `Vec<Struct>` work. `push` has
  two forms: `push(xs: Vec<T>, v) -> Vec<T>` (consuming) and
  `push(xs: mut ref Vec<T>, v) -> i64` (in-place, returns the new length —
  useful through a struct field). See
  [examples/push_mut.vani](examples/push_mut.vani).
- Tuples `(T1, T2, ...)` (n in 2..=4) with `.0` / `.1` access; destructure
  `let (a, b) = expr;`.
- Structs `struct Point { x: i64, y: i64 }` with up to 64 fields; field access
  `p.x` and field assign `p.x = v;`.
- Enums: `enum Color { Red, Green, Blue }`. Payloaded variants `enum Opt
  { Some(i64), None }` work in both backends — tagged-union codegen lays
  them out as `{ i32 tag, T payload }`. Match destructure
  `Opt.Some(v) then …` binds the payload into the arm scope. V1 limits
  payloads to single Copy fields per variant + uniform payload type
  across variants. See [examples/option_types.vani](examples/option_types.vani).
- Type aliases: `type Coord = (i64, i64);`, `type X = i64;`.
- Constants: `const ANSWER: i64 = 42;` — literal initializers only in v1.

### References (second-class, keyword-first)
- `ref T` (shared) and `mut ref T` (mutable) — parameter types only;
  borrow at call sites with `ref xs` / `mut ref xs`. No reference returns,
  let-bindings, or aggregate elements. Aliasing rejected.
- Indexed write `xs[i] = v;` works on owned `[T;N]` / `Vec<T>` and through
  `mut ref` parameters.
- Auto-deref for indexing and method dispatch.

### Functions, methods, and dispatch
- Functions `fn add(a: i64, b: i64) -> i64 { … }`; pure-fn marker
  `pure fn …` for SMT-callable helpers.
- `methods on T { fn m(self: T) -> R { … } }` blocks. Receivers must be
  `self: T` / `self: ref T` / `self: mut ref T` (keyword-first; `&self`
  rejected). Method dispatch via `recv.method(args)` with auto-ref.
- First-class fn-pointers `fn(T1, ...) -> R` with `FnRef` + indirect call.
- Discarded call statements: `x.bump();` / `foo();` are sugar for
  `let _ = …;` (must be a `Call`/`MethodCall`).

### Control flow + expressions
- `if`/`else`/`else if` chains as statements OR single-expression form
  `if cond { e1 } else { e2 }` (both branches must unify).
- `while cond invariant inv1; invariant inv2; { … }`.
- `for i from lo to hi invariant inv; { … }`, `for x in ref xs { … }`,
  `for x in xs { … }` (consuming).
- `break;` / `continue;`, `assert cond[, "msg"]`, `prove`, `print` (multi-item).
- `match scrutinee { Color.Red then expr, … }` — exhaustive over enum
  variants; integer-literal patterns, `_` wildcard, and **payloaded variant
  destructure** `Opt.Some(v) then …` all supported. Bool / Str / float
  scrutinee patterns are gated.
- **Block expressions** `let r = { let a = …; let b = …; a + b };` — Let
  stmts followed by a tail expression. Inner shadows don't leak.
- **`try EXPR`** — Option/Result-like error-propagation sugar. In a
  function whose return type is a payloaded enum, `let v: T = try opt;`
  extracts the payload or short-circuits the function with the
  payload-less variant. Restricted shape in v1 (let-try as first stmt,
  intermediate lets, return) — see [examples/try_keyword.vani](examples/try_keyword.vani).
- Short-circuit `&&` and `||` honor compile-time const folding —
  `false && (provably-bad)` and `true || (provably-bad)` compile cleanly.
- Lexical scoping: inner `let x` shadowing of an outer same-name binding
  is contained to the inner scope (cross-type shadow allowed).

### Generics & interfaces
- **Generic functions** `fn id<T>(x: T) -> T { return x; }` —
  monomorphized at compile time. The pre-pass walks call sites, infers
  T from the first literal argument (v1 restriction), and generates a
  specialized copy per concrete type (`id__i64`, `id__bool`, …). The
  original generic template is dropped before codegen sees it. See
  [examples/generic_functions.vani](examples/generic_functions.vani).
  V1 limits: single type parameter, body must be type-correct without
  knowing T (pass-through patterns).
- **Interfaces** `interface Show { fn show(self: T) -> R; }` + `implement
  Show for Point { fn show(self: Point) -> R { … } }` — static dispatch
  via `recv.show()`. The impl hoists to `T_show`; the existing method-
  dispatch path resolves the call at compile-time based on the receiver's
  type. V1 limits: static dispatch only (no vtables); each impl must cover
  every interface method; signatures must match exactly. See
  [examples/interfaces.vani](examples/interfaces.vani).
- **Drop interface** `implement Drop for T { fn drop(self: T) -> i64 { … } }`
  — auto-called at every scope exit where a non-moved binding of T goes
  out of scope. Users can also call `t.drop()` manually; affine tracking
  marks the binding as moved so the auto-call won't double-fire. When T
  has heap-shaped fields (OwnedStr / Vec), the per-field free pass runs
  instead (the user's drop is then invoked explicitly when richer
  behavior is needed). See
  [examples/drop_interface.vani](examples/drop_interface.vani).
- **Mixed-place assignment** — `xs[i].field = v;` and the deeper
  `xs[i].a.b = v;` write through an index plus a struct field path in
  one statement. Works on owned `Vec<T>` and `[T; N]`. Intermediate
  segments must be Copy structs. The leaf field may be Copy OR a
  heap-shaped type (`OwnedStr` / `Vec<T>`) — when the leaf is heap-
  shaped, both backends free the previous slot value before storing
  the new one, so the old allocation does not leak. See
  [examples/mixed_place_assign.vani](examples/mixed_place_assign.vani).
- **Partial-move tracking** — `let taken = bag.contents;` moves a single
  field out of a struct. The aggregate is still readable for its other
  fields; scope-exit Drop skips the moved field (no double-free); a
  second read of the moved field surfaces a use-after-move diagnostic.
  See [examples/partial_move.vani](examples/partial_move.vani).
- **User-defined `==` via `implement Eq for T`** — `a == b` and `a != b`
  on struct or enum bindings desugar to the hoisted `<T>_eq(a, b)` /
  `!<T>_eq(a, b)` whenever both sides are the same nominal type.
  Convention is `fn eq(self: T, other: T) -> bool`. See
  [examples/struct_eq.vani](examples/struct_eq.vani) and
  [examples/enum_eq.vani](examples/enum_eq.vani).
- **Tuple auto-equality** — tuples are anonymous, so `==` is
  compiler-derived: `(a, b) == (c, d)` rewrites to `a == c && b == d`.
  Each per-element comparison uses the element type's `==` rule
  (built-in for primitives, `<T>_eq` for nominal element types). See
  [examples/tuple_eq.vani](examples/tuple_eq.vani).
- **Field-borrow expressions** — `ref t.f` and `mut ref t.f` take a borrow
  of a struct field. The result type is `&<field_ty>` / `&mut <field_ty>`;
  backends GEP into the struct's storage. Unlocks atomic operations
  through a struct that owns the cell (`atomic_*(ref c.hits)` /
  `atomic_*(mut ref c.hits)`). Single-level only in v1
  (no `ref t.a.b`). See
  [examples/struct_atomic_field.vani](examples/struct_atomic_field.vani).
- **Enums with affine payloads** — Copy types, `OwnedStr`, `Vec<T>`,
  `[T; N]` of Copy elements, `Task`, `Atomic<T>`, `Mutex<T>`, and
  `Channel<T, N>` are all valid as enum payload types in v1; only
  `Guard<T>` still needs codegen work. Heap payloads (OwnedStr, Vec) get a tag-conditional
  free at scope exit; stack-shaped payloads (array, Task, Atomic) need
  no Drop. v1 restriction: destructure-binding patterns (`Some(s)`)
  require Copy payloads. See
  [examples/enum_owned_payload.vani](examples/enum_owned_payload.vani),
  [examples/enum_vec_payload.vani](examples/enum_vec_payload.vani),
  [examples/enum_arr_payload.vani](examples/enum_arr_payload.vani).
- **Structs with affine fields** — `OwnedStr`, `Vec<T>`, `[T; N]` of Copy
  elements, `Task`, `Atomic<T>`, `Mutex<T>`, `Channel<T, N>`, and **nested
  affine structs** are valid struct field types in v1. Both backends
  recursively walk struct types at scope-exit Drop time so a `struct
  Outer { inner: Inner, id: i64 }` where `Inner` has `OwnedStr` /
  `Vec<T>` fields gets full RAII chains. Only `Guard<T>` is still
  rejected. See
  [examples/nested_struct_drop.vani](examples/nested_struct_drop.vani).
  Heap-shaped fields (OwnedStr, Vec) are freed at scope exit; stack-shaped
  fields (arrays, Task, Atomic) need no runtime drop. Struct-literal init
  from a `Var` moves the source binding so a heap value flows `caller →
  struct field → drop` without a double-free. Field-path indexing
  (`t.data[i]`) works through both backends. Mutex / Guard / Channel still
  need explicit wiring. See
  [examples/struct_owned_field.vani](examples/struct_owned_field.vani),
  [examples/struct_mixed_fields.vani](examples/struct_mixed_fields.vani).

### Verification & contracts
- `requires` / `ensures` clauses (terminated with `;`, before the body).
  `_return` references the return value; inline calls discharged via callee
  `ensures`.
- Loop invariants with substitution-based preservation and post-loop facts.
- Three-layer `prove`: constant fold → structural tautology → SMT (Z3).
- BitVec overflow-aware integer arithmetic; IEEE-754 floats (NaN/±inf
  modeled); signed/unsigned compare split; cast-via-extend.
- Symbolic SMT arrays per Vec/array binding with versioned store axioms.
- SMT-driven runtime-guard elision (bounds, divisor, shift checks).
- Compile-time const overflow and divide-by-zero detection.
- `INTENTC_NO_VERIFY=1` opt-out for fast dev iteration.

### Affine ownership
- Arrays, `Vec`, `OwnedStr`, `Task`, `Atomic`, `Mutex`, `Guard`, `Channel`
  are affine — moved on use, dropped at end of scope.
- Use-after-move is a compile error with related-span notes pointing at the
  prior move site.
- `let` shadowing drops or consumes the previous binding.
- `_` discard binding (`let _ = expr;`) covers drop for Copy results and
  triggers the affine drop chain for owned ones.

### Concurrency
- `parallel for` with reductions (`+`, `*`, `&&`, `||`, `&`, `|`, `^`,
  `min`, `max`). Verifier proves race-freedom; backends emit real threads
  (libgomp on Linux, CreateThread on Windows).
- `task <name> { … } / join <name>;` — affine handles, Copy-only captures,
  real pthread / CreateThread spawn.
- `Atomic<T>` (i8..i64, u8..u64, bool) — `atomic_new`/`atomic_load`/
  `atomic_store`/`atomic_fetch_add`/`atomic_compare_exchange`.
- `Channel<T, N>` — Vyukov MPSC ring buffer (power-of-2 N).
- `Mutex<T>` + RAII `Guard<T>` — Drepper futex (Linux), WaitOnAddress
  (Windows), sched_yield/SwitchToThread fallback.

### Tooling
- `intentc check / emit / emit-c / run / build / test` with `--json`
  machine-readable diagnostics.
- `intent-lsp` binary with hover, definition, references, rename,
  completion, code actions, semantic tokens (7 token types, 2 modifiers).
- Parser error recovery — multiple errors per compile, not just the first.
- Diagnostics with related-span notes.
- Multi-file projects via `use "path.vani";` (transitive, cycle-detected).

### Backends
- **LLVM** is the default for `emit`/`run`/`build` (AOT via `llc + cc`).
- **C** (`--backend=c`, legacy/deprecation path).
- Both have tree-shaped and SSA pipelines; `intentc` tries SSA first and
  falls back to tree backends on `EmitError`.

## Integer Rules

Arithmetic operators `+`, `-`, `*`, `/`, and `%` work on integer operands. The
compiler chooses a common result type before checking the expression:

- `i32 + i64` becomes `i64`
- `u32 + u64` becomes `u64`
- `i64 + u32` becomes `i64`, because `i64` can represent every `u32` value
- `i32 + u64` is rejected for now, because neither side can safely represent
  all values from the other side

This is intentionally more conservative than C. A verification-oriented
language should not silently convert `-1` into a huge unsigned value.

Integer constants are flexible until they are assigned or combined with a typed
operand, so these are valid:

```intent
let tiny: u8 = 42;
let wider: i64 = tiny + 1000;
```

But these are rejected at compile time:

```intent
let bad_div = 10 / 0;
let too_large: u8 = 250 + 10;
```

`%` is integer-only. A zero divisor is rejected at compile time when known, and
the C backend emits a runtime assertion around non-constant divisors.

## Float Rules

`f32` is single precision and `f64` is double precision. Float arithmetic works
with signed and unsigned integers:

- `f32 + u32` becomes `f32`
- `f64 + i64` becomes `f64`
- `f32 + f64` becomes `f64`
- a flexible literal such as `3.0` can adapt to a surrounding `f32`

Float constants must stay finite. The compiler rejects constant division by
zero and constant results that become `NaN` or infinity in the target type.
Non-constant float divisors are protected by emitted runtime assertions.

## Casts

Use `as` for explicit numeric casts:

```intent
let wide: u64 = (count as u64) + total;
let precise: f64 = (single as f64) + 2.25;
```

Implicit casts are inserted only when the checker considers them safe for this
prototype. Explicit casts are represented in the typed IR and emitted as C casts,
so generated code makes conversions visible instead of relying on C defaults.

## Shift and bitwise rules

`<<` and `>>` work on integers. The left operand determines the result type:

```intent
let bits: u8 = 1 as u8;
let shifted: u8 = bits << 3;
```

The shift count must be non-negative and smaller than the bit width of the left
operand. Known-bad counts such as `(1 as u8) << 8` are compile-time errors, and
the C backend emits runtime assertions for non-constant counts. `>>` is
arithmetic for signed integers and logical for unsigned integers.

Bitwise `&`, `|`, and `^` are integer-only (floats and bools are rejected;
bools have their own logical `&&` and `||`). Precedence follows Rust:
shifts bind tighter than `&`, which binds tighter than `^`, which binds
tighter than `|`, which sits above comparisons. `a == b | c` therefore
parses as `a == (b | c)`. The unary prefix `&` (taking a reference) is
disambiguated by position: only the infix context picks up the new
bitwise binding.

Runtime overflow checks and non-constant proof obligations belong in the next
verification pass. Today, constant mistakes are prevented by the compiler,
risky runtime divisors/counts are asserted in generated C, and richer safety can
be expressed with `requires`, `assert`, and later SMT proofs.

`requires` clauses are currently lowered to runtime `assert` calls in the
emitted C; they will become verification obligations once the SMT pipeline
lands.

`prove` is discharged in three layers, tried in order:

1. **Constant folding** — compile-time-known boolean true.
2. **Structural tautologies** — `x == x`, `!(x != x)`, `x <= x`, etc.
3. **SMT verifier** — encodes the claim plus all in-scope `requires` clauses
   as an SMT-LIB query and asks an external solver (z3) whether the negation
   is unsatisfiable. **Integer types are encoded as fixed-width
   `(_ BitVec N)`**, so overflow is faithfully modeled — `prove x + 1 > x;`
   for `x: i64` is correctly rejected with the counterexample
   `x = 9223372036854775807` (INT64_MAX, where the sum wraps). Comparisons
   pick the signed (`bvslt`/`bvsge`) or unsigned (`bvult`/`bvuge`) form
   from each variable's type. Floats use `(_ FloatingPoint 8 24)` /
   `(_ FloatingPoint 11 53)` with `fp.add`/`fp.lt`/`fp.eq` and `RNE`
   rounding. Integer casts use `sign_extend`/`zero_extend`/`extract`;
   int→float and float→float use `to_fp`. Shifts, array/Vec/reference
   operations and function-call results fall outside the v1 encoder and
   produce a "skipped" diagnostic.

For step 3 to work, install z3 and ensure it's on `$PATH` (or point `$Z3`
at the binary). Without z3, the verifier falls back to layers 1–2 and
reports "no SMT solver available" when those don't suffice.

When z3 returns `sat`, the diagnostic includes a **counterexample**
extracted from z3's model — e.g.
`proof failed: SMT counterexample [x = 0, y = 0]` for `prove x + y > x`.
The model parser handles z3's typical output forms (negative integers via
`(- N)` flatten to `-N`); Vec-length witnesses appear as `len(xs) = …`.

## Numeric Literals

Integer literals may use `_` as a digit separator, and the prefixes `0x`/`0X`,
`0b`/`0B`, and `0o`/`0O` for hex, binary, and octal. Examples:

```intent
let big: i64 = 1_000_000;
let mask: u16 = 0xFF_FF;
let bits: u8  = 0b1010_1010;
```

## Arrays and Ownership

Fixed-size arrays live on the stack and carry their length in the type:

```intent
let xs: [i64; 4] = [10, 20, 30, 40];
let n: u64       = len(xs);   // n == 4
let first: i64   = xs[0];
```

Arrays are **affine** — they are owned by a single binding at a time. Passing
an array to a function or assigning it to another `let` moves it; the source is
unusable after. Numeric primitives stay `Copy` and behave as before:

```intent
fn sum_four(xs: [i64; 4]) -> i64 {
  return xs[0] + xs[1] + xs[2] + xs[3];
}

fn main() -> i64 {
  let xs: [i64; 4] = [1, 2, 3, 4];
  let total = sum_four(xs);    // xs is moved here
  // let bad = xs[0];           // error: 'xs' was moved on the line above
  print total;
  return 0;
}
```

Array element types accept Copy primitives, structs, and tuples. Nested
arrays (`[[i64; 4]; 3]`) and `[Vec<_>; N]` are still gated — the SSA layer's
by-value-element-load path doesn't handle them yet. Array return types are
also rejected (clean diagnostic).

Bounds checks at `xs[i]` are runtime by default. When the index is a
compile-time integer constant in range, the check is elided and the C backend
emits a direct index. Out-of-range constant indices are compile errors.

## Vectors

`Vec<T>` is a heap-allocated, dynamically-sized owned collection. Like arrays,
it is **affine** (moved on use, dropped at end of scope). Element types must be
`Copy`. The four built-in operations are:

```intent
let xs: Vec<i64> = vec(10, 20, 30);
let xs           = push(xs, 40);     // consumes old xs, returns new Vec
let xs           = set(xs, 0, 99);   // functional update; returns new Vec
let ys           = clone(xs);        // independent copy; xs stays usable
let n: u64       = len(xs);          // runtime length
let first        = xs[0];            // always runtime bounds-checked
```

Notes:

- `push` and `set` consume their first argument; `clone` deliberately does not.
- `let` shadowing is the natural way to express functional update — the new
  binding must have the same type as the old.
- Buffers are freed automatically: when a `Vec` binding is shadowed without
  being consumed, or when it falls out of scope at function return without
  being returned.
- Returning a `Vec` from a function transfers ownership to the caller; no
  destructor runs at the callee site.
- The built-in names `vec`, `push`, `set`, and `clone` cannot be redefined as
  user functions.
- `vec()` with zero arguments is supported (empty Vec).
- `Vec<T>` accepts non-`Copy` elements: `Vec<Vec<T>>`, `Vec<[T; N]>`,
  `Vec<OwnedStr>`, and `Vec<Struct>` all work. Reading a non-Copy slot into a
  binding requires `clone_at(ref xs, i)` — bare `let inner = xs[i]` would alias
  the owner's slot and double-free, so the checker rejects it with a hint
  pointing at `clone_at`. At scope exit the Vec's `__free` helper walks every
  live element and drops its owning resources before releasing the buffer, so
  `Vec<OwnedStr>` and `Vec<Struct{…OwnedStr / Vec…}>` don't leak their
  per-element heaps.

Under the hood, the backend monomorphizes one C struct + helper bundle per
distinct element type used:

```c
typedef struct { int64_t* data; uint64_t len; uint64_t capacity; } intent_vec_int64_t;
static intent_vec_int64_t intent_vec_int64_t__push(intent_vec_int64_t xs, int64_t v);
static void intent_vec_int64_t__free(intent_vec_int64_t xs);
// ... etc
```

In-place reuse for `push`/`set` falls out for free: affine ownership
guarantees that `xs` is unique at the call site, so the helpers can mutate the
underlying buffer (and `realloc` it) without violating any aliasing
invariants.

## Strings

Two distinct types share the language's string surface:

- **`Str`** — borrowed, `Copy`, NUL-terminated. Models a pointer to
  either a static string literal or someone else's buffer. Supports
  `==`/`<`/etc. (via `strcmp`), `len(s)` (via `strlen`), passing to
  parameters, comparisons, etc. Always safe to re-use.
- **`OwnedStr`** — heap-allocated, NUL-terminated, **affine**.
  Produced by the `+` concat operator. The compiler tracks
  ownership through moves and inserts a runtime `free` at the end
  of every scope where an `OwnedStr` binding is still live, or
  whenever the value is moved into another concat / a return /
  another scope.

```intent
fn greet(name: Str) -> OwnedStr {
  return "Hello, " + name;   // fresh heap buffer
}

fn main() -> i64 {
  let g: OwnedStr = greet("alice");
  let banged: OwnedStr = g + "!";   // consumes `g`; `g` is now moved
  print banged;                     // freed at end of scope
  return 0;
}
```

The runtime helper `intent_str_concat(l, l_owned, r, r_owned)`
mallocs `strlen(l) + strlen(r) + 1` bytes, memcpys both operands,
NUL-terminates, and frees whichever operand had `*_owned == 1`
before returning the joined buffer. Mixing `Str` and `OwnedStr`
operands in either position works — the `_owned` flag is `0` for
`Str` (borrowed) and `1` for `OwnedStr`.

`len(s)` works for both types and dispatches to `strlen`. The
ordering / equality comparison operators (`==`, `!=`, `<`, `<=`,
`>`, `>=`) accept any combination of `Str` and `OwnedStr` operands
— the `OwnedStr` side is auto-borrowed (the comparison only reads,
so the binding stays live for its scope-end drop). Function
arguments do the same: passing an `OwnedStr` where a `Str`
parameter is expected works and leaves the caller's binding
untouched.

## References

When a function only needs to *read* a `Vec` or array, take a shared reference
instead of consuming the value:

```intent
fn sum(xs: ref Vec<i64>) -> i64 {
  return xs[0] + xs[1] + xs[2];
}

fn main() -> i64 {
  let xs: Vec<i64> = vec(1, 2, 3);
  let total: i64 = sum(ref xs);    // borrow; xs is not consumed
  let first: i64 = xs[0];          // still usable
  return 0;
}
```

Mutable references (`mut ref T`) allow in-place updates through the borrow:

```intent
fn bump(p: mut ref Point) -> i64 {
  p.x = p.x + 1;
  return p.x;
}

fn main() -> i64 {
  let p: Point = Point { x: 0, y: 0 };
  return bump(mut ref p);
}
```

References are **second-class** by design — keyword-first syntax (no `&`,
no `&mut`):

- Type spelling: `ref T` (shared), `mut ref T` (mutable). Rust-style
  `&T` / `&mut T` is rejected.
- Borrow expression: `ref x` / `mut ref x` at call sites. The inner
  expression must be a variable; function-call results and temporaries can't
  be borrowed.
- Allowed *only* as function parameter types (and method `self:` receivers).
  Forbidden as return types, `let` annotations, aggregate elements, and
  nested inside another reference.
- Auto-deref inside the callee: `xs[i]`, `len(xs)`, `p.field`,
  `recv.method()` all work without explicit dereferencing.
- Re-borrow is transparent — passing a `ref T` parameter directly to
  another function expecting `ref T` works.
- Aliasing rejected at call sites: a call cannot pass `mut ref x` alongside
  any other reference to `x`, and cannot pass a moved `x` alongside any
  borrow of `x`.

C lowering: `ref Vec<T>` becomes `const intent_vec_T*`; `mut ref Vec<T>`
becomes `intent_vec_T*`; `ref [T; N]` and `ref i64` become `const T*`.
Auto-deref expands to `(*xs).field` on the Vec case; array-by-pointer uses C
array decay so `xs[i]` continues to work syntactically.

## Control Flow

`if` / `else` / `while` are statements, and a plain `name = expr;` reassigns
an existing binding without redeclaring it:

```intent
fn sum(xs: &Vec<i64>) -> i64 {
  let total: i64 = 0;
  let i: u64 = 0;
  let n: u64 = len(xs);
  while i < n {
    total = total + xs[i];
    i = i + 1;
  }
  return total;
}

fn abs(x: i64) -> i64 {
  if x < 0 {
    return 0 - x;
  } else {
    return x;
  }
}
```

Rules:

- The condition of `if` and `while` must be `bool`.
- Branches share the parent's scope (no nested lexical scope yet). Bindings
  *declared* inside a branch persist after; for affine types, they must be
  consumed or visible in the post-merge state.
- Affine **move-state must reconcile at merges.** If `xs: Vec<T>` is moved in
  one branch of an `if` but not the other, the checker errors and asks you to
  consume or rebind it in both branches.
- For `while`, the body must leave every outer affine binding in the same
  move-state it started in. The natural pattern is to consume-then-rebind:
  `let xs = push(xs, i);` consumes the old `xs` and immediately reassigns it,
  so the body is balanced.
- `return` inside a branch terminates that path; an `if`/`if-else` where every
  path returns is itself terminating, and counts toward the function's
  "must return" obligation.
- Code after a guaranteed `return` (or after an `if-else` where both branches
  return) is rejected as unreachable.
- `name = expr;` requires `name` to be an existing binding; the RHS is coerced
  to its declared type. For affine bindings the old buffer is freed before
  the new value is installed (just like `let`-shadowing).

### Loop control: `break` / `continue`

```intent
fn find_first_negative(xs: &Vec<i64>) -> i64 {
  let i: u64 = 0;
  let result: i64 = 0 - 1;
  while i < len(xs) {
    if xs[i] < 0 {
      result = xs[i];
      break;
    }
    i = i + 1;
  }
  return result;
}
```

- `break;` exits the innermost `while`. `continue;` jumps to the next
  iteration. Both are rejected outside a loop.
- The move-state-balance rule extends to jump points: at any `break`,
  `continue`, or natural fall-through, every outer non-`Copy` binding must be
  in the same move state it had at loop start. So if you `take(xs)` inside
  the body, you must `let xs = …;` (or `xs = …;`) before any reachable jump
  out of the loop.
- After an `if`/`while`, the checker conservatively clears compile-time
  constant tracking for all bindings in scope. This avoids unsound `prove`
  discharge when branches mutate values; it's slightly over-conservative
  (constants that survived unchanged are also cleared), and is a known
  follow-up.

### Lexical scoping

Every `if`/`else`/`while` body opens a new scope:

```intent
fn main() -> i64 {
  let counter: i64 = 0;
  let i: i64 = 0;
  while i < 4 {
    let local: Vec<i64> = vec(i, i + 1, i + 2);   // declared in loop body scope
    if local[0] >= 1 {
      counter = counter + 1;                       // mutates outer counter
    }
    i = i + 1;
  }
  // `local` is not visible here; its buffer was freed each iteration.
  assert counter == 3;
  return 0;
}
```

Rules:

- `let x = …` inside an inner scope introduces a **new** binding for the
  duration of that scope. If the outer scope already has a binding called `x`,
  the inner one shadows it (possibly with a different type) and the outer
  binding is restored when the inner scope ends.
- To **mutate** an outer binding from inside an inner scope, use plain
  assignment `x = …;`. Plain assignment finds `x` via lookup that walks the
  scope stack and updates the binding wherever it lives.
- Bindings declared inside `if`/`while` bodies are dropped automatically at
  the end of their scope. For `Vec<T>` (heap-owned), this emits an
  `intent_vec_T__free` call before the C `}` closes.
- `break` and `continue` insert drop calls for every non-`Copy` live binding
  in scopes opened inside the loop body, in deepest-first order, before the
  C `break;`/`continue;`.

If you used to write `let xs = push(xs, i);` inside a loop body to mutate an
outer `xs`, you must now write `xs = push(xs, i);` — the `let` form
introduces a new inner `xs` that goes away at iteration end, which is almost
never what you wanted.

## Mutable references and indexed writes

When a function needs to *modify* a `Vec` or array element in place, take a
mutable reference and use indexed assignment:

```intent
fn double_each(xs: &mut Vec<i64>) -> u64 {
  let i: u64 = 0;
  while i < len(xs) {
    xs[i] = xs[i] * 2;
    i = i + 1;
  }
  return len(xs);
}

fn fill(xs: &mut [i64; 4], v: i64) -> i64 {
  let i: u64 = 0;
  while i < 4 {
    xs[i] = v;
    i = i + 1;
  }
  return v;
}

fn main() -> i64 {
  let xs: Vec<i64> = vec(1, 2, 3, 4);
  let n: u64 = double_each(&mut xs);
  assert n == 4;
  assert xs[3] == 8;

  let ys: [i64; 4] = [0, 0, 0, 0];
  let _ = fill(&mut ys, 9);
  assert ys[0] == 9;
  return 0;
}
```

Rules:

- `&mut T` is a parameter-only type (same second-class constraint as `&T`).
  No `&mut` returns, no `&mut` let-bindings.
- `&mut x` borrows `x` mutably for the duration of the call. The source must
  be a variable — and **not** itself a shared `&T` (you cannot upgrade an
  immutable borrow to a mutable one). Owned bindings and `&mut T` parameters
  are mutably-borrowable.
- `xs[i] = v;` writes through the subscript. Allowed when `xs` is owned
  (`[T;N]` or `Vec<T>`) or when `xs` is `&mut [T;N]` / `&mut Vec<T>`.
  Writing through `&T` is rejected.
- Bounds are checked at runtime, with the same compile-time elision for
  constant-in-range indices on owned arrays. Constant out-of-range writes
  are compile errors.
- **Aliasing rule (call-site):** within a single function call, the argument
  list cannot pass `&mut x` together with any other reference to `x`, and
  cannot pass a moved `x` together with any borrow of `x`. Multiple `&x`
  borrows of the same variable in one call are fine. Detection is purely
  syntactic at call sites (sound for second-class refs, since they can't
  escape the call).

C lowering: `&mut Vec<T>` becomes `intent_vec_T*` (no `const`); `&mut [T;N]`
becomes `T*`. The reading auto-deref for `xs[i]` / `len(xs)` works through
`&mut` exactly as it does through `&`.

### `for` loops over integer ranges

```intent
fn sum_squares(n: i64) -> i64 {
  let total: i64 = 0;
  for i in 1..n {
    total = total + i * i;
  }
  return total;
}
```

- Syntax: `for var in start..end { body }`. Both bounds must be integers;
  flexible-literal bounds adapt to the typed bound (`for i in 0..len(xs)`
  with `len(xs): u64` types `i` as `u64`).
- The loop variable is scoped to the body. Each iteration runs with the
  current value; the post-step increments by 1 before the next condition
  check, so `continue` correctly advances the counter (unlike a hand-rolled
  `while`).
- Move-balance rules and scope rules for nested let/break/continue work
  identically to `while`.

### Iterating arrays and Vecs

Use `for x in &xs { body }` to walk an array or Vec without consuming it:

```intent
fn sum(xs: &Vec<i64>) -> i64 {
  let total: i64 = 0;
  for x in &xs {
    total = total + x;
  }
  return total;
}

fn max5(xs: &[i64; 5]) -> i64 {
  let best: i64 = xs[0];
  for x in &xs {
    if x > best {
      best = x;
    }
  }
  return best;
}
```

Rules:

- The source `xs` after `&` must be a variable bound to an owned `[T; N]` /
  `Vec<T>` or a borrow `&[T; N]` / `&mut [T; N]` / `&Vec<T>` / `&mut Vec<T>`.
- Element type `T` must be `Copy` (current Vec/array constraint).
- The element variable `x` is bound only inside the loop body; the
  collection is borrowed for the loop and is not moved.
- `break` and `continue` work as in any other loop.
- Lowering: each iteration C-binds `x = xs[i]` (arrays) or `x = (*xs).data[i]`
  (Vec / &Vec) for a synthesized index variable.

**Consuming form**: `for x in xs { body }` (no `&`) moves `xs` into the
loop. For `Vec<T>` the backend frees the buffer immediately after the
loop body, so it's the natural pattern for "process every element then
discard the collection". The source must be an owned `Vec<T>` or `[T; N]`
binding — consuming a `&T` or `&mut T` parameter is rejected (use the
borrow form). After the loop, `xs` is moved; any subsequent use is a
compile error with a related note pointing at the `for` line.

## SMT verification

`prove` will reach the SMT layer when constant folding and structural
recognition both fail. Example:

```intent
fn safe_subtract(a: i64, b: i64) -> i64
requires a >= b;
{
  prove a - b >= 0;
  return a - b;
}
```

The checker encodes the function's `requires` plus the negation of the prove
expression and asks z3. If z3 returns `unsat`, the proof holds; `sat` means
z3 produced a counterexample and the prove is rejected; `unknown` or "skipped"
(unsupported features) produces a diagnostic suggesting how to simplify the
claim.

**Call sites verify callee `requires`.** When a function with `requires`
clauses is called, the checker substitutes the argument expressions for
the parameter names in each precondition and asks z3 whether the
substituted preconditions hold under the *caller's* current facts. A
counterexample produces a diagnostic such as

```
argument to 'safe_sub' violates its 'requires' clause
  [counterexample: a = 3, b = 7]
note: callee precondition
requires a >= b;
```

before the runtime check would ever fire. Preconditions outside the SMT
v1 fragment fall back silently — the runtime `assert` still guards the
call. Calls inside any statement-level expression (`let`, `=`, `return`,
`assert`, `prove`, `print`, `if`/`while` conditions) are covered.

**Contradictory `requires` are flagged.** Before checking a function's body,
the verifier asks z3 whether the requires clauses are jointly satisfiable.
If they are not, every `prove` in the body would be vacuously true and the
function is unreachable at runtime — both bad signals. A diagnostic such as

```
function 'dead' has contradictory 'requires' clauses; every proof in
its body is vacuously true and the function is unreachable
```

surfaces. Encodings that exceed the SMT v1 fragment fall back to "not
contradictory" (conservative), so the check never produces a false alarm.

Integer semantics in the SMT model are infinite-precision (SMT-LIB `Int`)
plus a range constraint per variable's type. This is sound when arithmetic
stays within the declared range — the same condition the C backend's runtime
already requires for correct execution. Wrap-around / overflow modeling is a
follow-up.

**`len(xs)` over fixed-size arrays** is substituted with the compile-time
length during SMT encoding — even when `xs` was passed by `&` or `&mut`
reference. So `requires i < len(xs)` is dischargeable for `xs: &[T; N]`
arguments.

**`len(xs)` over `Vec<T>`** is encoded as a per-binding opaque SMT integer
`<name>_len` with `>= 0`. The length is treated as an unknown but consistent
value across a single proof — so `requires i < len(xs); prove i < len(xs);`
works (both sides reference the same SMT variable), as does propagating
`ensures _return < len(xs);` from a callee that promises a safe index.

**Vec-builtin length facts.** `let r = <builtin>;` automatically records
the resulting length so subsequent proofs see the relationship between
old and new bindings:

| Builtin                | Recorded fact                  |
|------------------------|--------------------------------|
| `vec(a, b, c)`         | `len(r) == 3`                  |
| `push(xs, v)`          | `len(r) == len(xs) + 1`        |
| `set(xs, i, v)`        | `len(r) == len(xs)`            |
| `clone(xs)`            | `len(r) == len(xs)`            |

So `prove len(push(xs, v)) == len(xs) + 1` discharges (when phrased as
`let ys = push(xs, v); prove len(ys) == len(xs) + 1;` — push consumes
its argument, so the relationship must be captured before the move).
The inline form `prove len(push(xs, v)) == len(xs) + 1` also works:
the verifier rewrites the call to a fresh symbolic Vec constrained by
the same length relationship.

**Stale facts are invalidated on reassignment.** Recording length facts
about a binding raises a question: what happens when that binding is
later reassigned? The verifier drops every fact mentioning the name
(both builtin length facts and ensures-derived facts from `let r =
foo();`) at any same-scope `let` shadow or any `name = expr;`
assignment — *outside a loop body*. Inside a loop body the drop is
suppressed so the substitution-based preservation check at body-end
still sees the entry invariants; preservation then re-establishes the
invariant for the new value via the last-reassignment rewrite.

One incompleteness gained for soundness: `let xs = push(xs, v);`
(same-name shadow with a self-referencing call) records no new fact,
since the natural relationship `len(xs) == len(xs) + 1` would be a
contradiction. Rename to `let ys = push(xs, v);` to recover the
length relationship in proofs.

**Array element reasoning.** Beyond length, the verifier models each
Vec/Array binding with an integer, bool, or float element type as a
symbolic SMT array `arr_<name>: (Array (BV64) Element)`, and reads
encode as `(select arr_<name> idx)`. This lets `prove xs[k] == V`
discharge in several composable shapes:

| Construct                | Fact emitted                                      |
|--------------------------|---------------------------------------------------|
| `let xs = vec(a, b, c)`  | `xs[0] == a`, `xs[1] == b`, `xs[2] == c`          |
| `let xs: [T;N] = [..]`   | per-slot `xs[k] == elements[k]`                   |
| `let ys = set(xs, k, v)` | `ys[k] == v` plus `arr_ys = (store arr_xs k v)`   |
| `let ys = push(xs, v)`   | `ys[len(xs)] == v` plus `arr_ys = (store arr_xs len(xs) v)` |
| `let ys = clone(xs)`     | `arr_ys = arr_xs` (every slot preserved)          |
| `let ys = xs;` (rebind)  | `arr_ys = arr_xs`                                 |
| `xs[k] = v;` (const k)   | bumps xs's SMT-array version; emits `arr_xs_v{N+1} = (store arr_xs_vN k v)`; existing facts get pinned to xs#N so they continue to describe the pre-assign state, while bare `xs` references resolve to the new version |
| `xs[i] = v;` (symbolic i)| same versioning path; the SMT solver can derive `xs[j] == old_value_j` for `j != i` through the store axiom even when `i` is opaque |

The store-axiom facts let the SMT solver derive `ys[j] == xs[j]` for
slots the call didn't touch, and the `Index` encoder is element-
type-aware (BV widths, `Bool`, `(_ FloatingPoint 11 53)` for f64,
`(_ FloatingPoint 8 24)` for f32 with operand-precision threading).

**SMT-array versioning.** Each `xs[i] = v` IndexAssign bumps a
per-binding version counter (tracked in the checker's `VarInfo`)
and emits a synthetic `arr_xs_v{N+1} = (store arr_xs_vN i v)`
axiom. Existing facts are pinned to `xs#N` before the bump so they
continue describing the pre-assign array, while subsequent
references to bare `xs` resolve to the new version at SMT query
time. Cross-binding relations like `arr_ys = arr_xs` (from a
`clone`) survive an IndexAssign on `xs`: `arr_ys` stays equal to
the old `arr_xs_vN`, the store axiom links `arr_xs_vN` to
`arr_xs_v{N+1}`, and the solver can reason about both old and new
states together.

`ensures _return[k] == V` is a first-class shape and propagates to
callers: the existing `record_ensures_facts` substitution rewrites
`_return` to the let-bound result and emits the slot fact. Multiple
per-slot ensures compose into full post-call array identity. See
`examples/array_proofs.vani` for the end-to-end pattern.

**Dev opt-out: `INTENTC_NO_VERIFY=1`.** Setting this env var skips
every SMT round-trip — `prove`, `ensures`, `invariant`, contradictory-
`requires`, call-site `requires`, and bounds-elision all silently
return without contacting z3. Useful for fast iteration when you're
focused on a non-proof code change. Runtime safety guards
(`intent_check_bounds`, divisor, shift, `assert` lowering of
`requires`) are kept in place — the program still runs safely. Do
not set this in CI; verifier-only bugs (a wrong invariant, a
violated ensures) won't surface at compile time.

**SMT-discharged runtime-guard elision.** When the verifier can prove
that an `Index`, `Div`/`Rem` divisor, or `Shl`/`Shr` count is safe
from the in-scope facts, the C backend skips the matching runtime
helper (`intent_check_bounds`, `intent_check_<ty>_divisor`,
`intent_check_<ty>_shift`). Example: in

```intent
fn first(xs: &Vec<i64>) -> i64
requires len(xs) > 0;
{
  return xs[0];
}
```

the `requires len(xs) > 0` is the only fact needed to discharge
`0 < len(xs)`, so the emitted C is the raw `(*xs).data[0]` — no
runtime comparison at the access site. The same elision applies to
`xs[i]` reads inside `for i in 0..len(xs) { … }` and any other
context where the index's bounds are derivable from preconditions
and loop facts. Elision fails closed: when the SMT layer can't
discharge (Unknown / unsupported / no z3), the runtime check stays.

**`ensures` clauses** become contracts. They are verified at every `return`
site (the SMT layer substitutes `_return` with the actual return expression
and checks that requires + branch conditions imply the ensures), and at call
sites they become facts the caller can rely on:

```intent
fn safe_sub(a: i64, b: i64) -> i64
requires a >= b;
ensures _return >= 0;
{
  return a - b;
}

fn caller(a: i64, b: i64) -> i64
requires a >= b;
{
  let r: i64 = safe_sub(a, b);
  prove r >= 0;   // discharged from safe_sub's ensures
  return r;
}
```

When a `let r = foo(args);` appears in a function whose callee has ensures,
the checker substitutes parameter names with the argument expressions and
`_return` with `r`, then appends those facts to the per-scope fact list.
Subsequent `prove` queries in the same scope see them.

Inline calls in proofs work too: `prove foo(args) > 0;` is rewritten so
that the call becomes a fresh symbolic variable, the callee's `ensures`
clauses are substituted onto that variable (and the supplied args), and
the SMT solver discharges the query against those facts. Calls to
functions without `ensures` still surface as unsupported, since there is
nothing for the solver to assume about their return value.

```intent
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
  prove inc(x) > x;  // discharged via inc's ensures, no let-binding needed
  return inc(x);
}
```

Branch conditions are also added to the fact list inside `if`/`else` bodies
(so `if x > 0 { prove x >= 1; }` is dischargeable). Branch-acquired facts
revert at the merge point — with one exception: when exactly one branch
terminates (return/break/continue), execution past the merge must have
taken the *other* branch, so the verifier keeps its guard as a fact.
This makes the early-return idiom

```intent
fn clamp(x: i64) -> i64
ensures _return >= 0;
{
  if x < 0 {
    return 0;
  }
  return x;     // `x >= 0` is in scope on this line.
}
```

verify without an explicit `else`.

The same narrowing applies after a natural loop exit. After
`while cond { … }` (with no `break` in the body), the post-loop facts
include `!cond` plus the invariants — so

```intent
let i: i64 = 0;
while i < 5
invariant i >= 0;
invariant i <= 5;
{
  i = i + 1;
}
prove i == 5;            // discharged: invariants + !cond ⇒ i == 5
```

is provable. The for-loop variant adds `i >= end` rather than `!cond`.
If the body can `break`, both checks are dropped (the loop may exit
with the condition still true).

### Loop invariants

```intent
fn sum_to(n: i64) -> i64
requires n >= 0;
ensures _return >= 0;
{
  let total: i64 = 0;
  let i: i64 = 0;
  while i < n
  invariant i >= 0;
  invariant total >= 0;
  {
    total = total + i;
    i = i + 1;
  }
  prove total >= 0;   // discharged from the invariant
  return total;
}
```

What the verifier does at each `while`/`for` loop with `invariant`s:

1. **Entry**: each invariant must be provable from the current SMT facts
   (function `requires`, branch conditions, prior ensures, and let-known
   constants).
2. **Body visibility**: inside the loop body, both the invariants and the
   loop condition are added as SMT facts so the body's own proves can use
   them. (And for `for i in start..end`, the bound `i < end` is also a body
   fact.)
3. **Preservation** (at body fall-through): each invariant is re-verified
   with a **last-reassignment substitution** applied — if the body
   contains `i = i + 1`, the invariant is checked as if `i` were `i + 1`
   for the purpose of the goal. For-loop bodies also implicitly substitute
   `i` with `i + 1` for the auto-increment. This catches buggy invariants
   like `invariant i < 3;` over `i = i + 1;` while admitting the typical
   linear-counter pattern.
4. **Post-loop**: invariants become SMT facts after the loop, available to
   subsequent `prove`s and to discharge the function's `ensures` clause.

Limitations (honest v1 caveats):

- The substitution captures the *last* reassignment per variable in the
  body — multiple distinct reassignments per iteration aren't tracked
  symbolically. Use a single update per variable per iteration for sound
  preservation checks.
- Reassignments inside nested `if`/`else` branches are merged via the
  union of last-reassigns; reassignments inside nested `while`/`for`
  loops are not propagated outward.
- The natural-exit `!cond` post-loop fact is not added (it would be unsound
  in the presence of `break`).

**Float reasoning** uses SMT-LIB's `FloatingPoint` theory, so IEEE-754
edge cases surface as counterexamples. For example, `prove x + 0.0 == x;`
on `x: f64` is *not* universally true — z3 reports `x = NaN`, since
`NaN + 0.0 = NaN` and `NaN == NaN` is false. Conversely, `prove !(x < x);`
discharges (all FP comparisons with NaN return false). Counterexamples
involving NaN, ±infinity, and signed zeros are rendered as `NaN`,
`+inf`/`-inf`, `0.0`/`-0.0` instead of their raw SMT-LIB s-expressions.

### Overflow-aware integer reasoning

Integer arithmetic is encoded as fixed-width `BitVec`, not infinite-precision
`Int`. This means:

- Wrap-around is faithfully modeled. `x + 1 > x` is **not** universally
  true for `x: i64` — z3 returns the counterexample at `INT64_MAX`. To
  prove arithmetic properties about `+`/`-`/`*`, add a `requires` clause
  bounding the inputs away from overflow (e.g., `requires a >= b;
  requires b >= 0;` for `prove a - b >= 0`).
- Counterexamples render as readable decimals — `x = 9223372036854775807`,
  `y = 0`, `len(xs) = 18446744073709551615` — by parsing z3's hex output
  (`#xffffffffffffffff`) against each variable's type and applying
  signed/unsigned interpretation.
- Comparisons split signed (`bvslt`/`bvsge`/...) vs unsigned
  (`bvult`/`bvuge`/...) based on the operand types.
- Integer casts use `sign_extend` (signed widening), `zero_extend`
  (unsigned widening), and `extract` (narrowing).
- Shifts (`<<`, `>>`) encode to `bvshl` / `bvlshr` / `bvashr`. Signed
  right-shifts use the arithmetic form so the sign bit is replicated.
  The shift count is automatically padded or truncated to match the
  left operand's width, so `x: u64 >> n: u32` proves cleanly.

Still planned: full SSA encoding for stronger preservation reasoning under
multi-reassignment loop bodies.

### Assert messages

`assert cond;` lowers to the C standard `assert(...)` macro. For more
informative runtime failures, pass an optional string after a comma:

```intent
fn lookup(xs: &Vec<i64>, i: u64) -> i64
requires i < len(xs);
{
  assert i < len(xs), "lookup: index out of range";
  return xs[i];
}
```

The custom-message form lowers to an `if (!cond) { fprintf(stderr, ...);
abort(); }` sequence so the printed message reaches stderr before the
process exits. Backslash, quote, newline, and other control characters in
the message are escaped into a valid C string literal.

### Discard pattern: `let _ = ...`

`_` is a write-only discard binding. It evaluates its right-hand side
for side effects (and to consume any affine values it captures) but
never introduces a name you can read back. Repeated discards in the
same scope do not collide because nothing is inserted into the
environment.

```intent
fn pure(x: i64) -> i64 { return x + 1; }

fn main() -> i64 {
  let _ = pure(7);              // Copy result → `(void)(fn_pure(7));`
  let _ = pure(8);              // Independent discard, no name clash.

  let owned: Vec<i64> = vec(1, 2, 3);
  let _ = owned;                // Consumes `owned` and frees its buffer.
  // `owned` is no longer usable here — the checker will reject it.

  return 0;
}
```

Lowering follows the value's category:

- **Copy** types (integers, floats, bool, refs) → `(void)(<expr>);`.
- **`Vec<T>`** → brace-scoped temporary plus a `..._free(...)` call so
  the heap buffer is released exactly once.
- **`[T; N]`** → brace-scoped temporary; the array drops on scope
  exit. The `(void)_intent_discard;` keeps the compiler quiet.

Reference values are rejected outright (`references cannot appear in a
'let _' discard`) because they would dangle the moment the discard
ends.

### Multi-file projects

A file can pull in others with `use "path.vani";`:

```intent
// math.vani
fn double(x: i64) -> i64 { return x * 2; }
```

```intent
// main.vani
use "math.vani";

fn main() -> i64 {
  let v: i64 = double(21);
  assert v == 42;
  return 0;
}
```

`intentc check`/`emit-c`/`run` accept the entry file and recursively resolve
`use` declarations relative to each file's directory. By default,
names from imported files share a flat namespace — but you can carve
out scoped sub-namespaces with **inline `module` blocks** at any level
(see the *Modules and namespaces* section below).

Cycles are detected by canonicalized path: each file is included at most
once across the dependency tree, so `a.vani` `use`-ing `b.vani` and
vice versa works fine.

Diagnostics in multi-file builds now point at the **original** file and
line, not the position in the concatenated buffer. A `FileMap`
(`diagnostic::FileMap`) tracks where each file's content lives in the
combined source, and `format_diagnostics_with_files` /
`format_diagnostics_json_with_files` resolve span offsets back to the
real `path:line:col` for each diagnostic — primary span and every
related note.

Caveats (v1):
- Name collisions across files surface as the normal "function 'X' is
  already defined" diagnostic.

### How linking works (build pipeline)

`intentc build file.vani -o out` lowers the entire program through
the LLVM pipeline:

```
file.vani  →  intentc check       (typecheck + SMT)
            →  emit LLVM IR (.ll)  (SSA path or tree fallback)
            →  opt -O2 (optional)
            →  llc -filetype=obj   (-O2, PIC)  →  .o
            →  cc -o out           (links libc, -pthread)
```

There is **no separate compile-then-link step** today — the whole
program goes through one driver invocation. Multi-file inputs are
**concatenated at the source level** through `use "path.vani";`
before the LLVM backend ever sees them, so all functions land in
one `.o` and `cc` produces the final binary in a single link.

#### Generating `.o` files for external linking

Two ways to produce an object file you can hand to another linker
(GCC / Clang / Rust's linker driver):

```bash
# Step 1: emit LLVM IR
intentc emit my_lib.vani --backend=llvm -o my_lib.ll
# Step 2: assemble to .o
llc -filetype=obj -relocation-model=pic -O=2 my_lib.ll -o my_lib.o
# Step 3: link with anything else
cc -o app my_lib.o c_main.c                    # link with C
clang++ -o app my_lib.o cpp_main.cpp           # link with C++
rustc cargo_main.rs --extern my_lib=my_lib.o  # link with Rust
```

Function symbols in the produced `.o` are named `fn_<vani_name>`
(e.g. `fn add` in vāṇी lowers to `fn_add` in the object). Their
ABI matches the C ABI for the target platform (System V on Linux /
macOS, MSVC on Windows). Declare them on the C / C++ side as:

```c
extern int64_t fn_add(int64_t a, int64_t b);
```

And on the Rust side as:

```rust
extern "C" {
    fn fn_add(a: i64, b: i64) -> i64;
}
```

#### Calling INTO vāṇी from external code

Works today via the `.o` route above. The vāṇी function's signature
must use Copy / pointer-compatible types (scalars, `ref T` borrows,
`Str` borrowed pointer). Affine handles (`Vec<T>`, `OwnedStr`,
`Atomic`, `Mutex`, `Guard`, `Channel`, `Task`) at the ABI boundary
need conversion — currently no FFI helper exists, so the
recommended pattern is to expose scalar / pointer entry points and
let vāṇी own the allocations internally.

#### Calling FROM vāṇी into external code

vāṇी declares foreign functions with the `extern "C" fn` form:

```vani
extern "C" fn abs(x: i32) -> i32;
extern "C" fn sqrt(x: f64) -> f64;
extern "C" fn triple(x: i32) -> i32;   // from your own helper.c

fn main() -> i64 {
  let a: i32 = abs(-7 as i32);    // libc — links by default
  let r: f64 = sqrt(81.0 as f64); // libm — needs -lm
  let t: i32 = triple(7 as i32);  // your code — needs --link-with
  write "abs(-7) =", a;
  write "sqrt(81) =", r;
  write "triple(7) =", t;
  return 0;
}
```

The body is empty; the linker provides the symbol. Codegen emits a
prototype against the bare C-ABI name (LLVM `declare`, C `extern`),
not a `fn_<vani_name>` definition.

`intentc build` accepts two flag groups for the link step:

```bash
intentc build prog.vani --link-with helper.c -o prog   # your .c / .o
intentc build prog.vani -lm -o prog                    # system library
intentc build prog.vani --link-with helper.o -lcurl -o prog   # both
```

`--link-with PATH` (repeatable) hands an extra object or source file
to `cc`. `-l<name>` (repeatable) forwards a library-link flag
verbatim. Both flag groups appear after the vāṇี object so symbol
resolution follows usual link order.

**Effects**: extern fns are conservatively treated as impure. The
SMT engine can't reason across the FFI boundary, so any
`prove`/`assume` involving an extern call must rest on caller-side
invariants. `pure fn` bodies reject impure extern calls.

For foreign functions that are genuinely pure (`abs`, `sqrt`, the
trig functions, `strlen`, etc.), mark the declaration `pure
extern "C" fn name(...) -> R;` to opt into purity. The caller is
asserting the symbol has no side effects, no shared state, and
deterministic output — vāṇी can't verify across the FFI boundary,
so misuse falls back to runtime behavior.

```vani
pure extern "C" fn sqrt(x: f64) -> f64;   // libm — known pure
extern "C" fn rand() -> i32;              // impure — no annotation
```

**ABI scope (v1)**: scalars (`i8..i64`, `u8..u64`, `f32/f64`,
`bool`), `Str` (NUL-terminated `i8*`), and any reference
(`ref T` / `mut ref T`) — pointers cross the FFI boundary
cleanly. The checker rejects unsupported shapes at the extern
declaration site with a `ref T` migration hint:

```vani
// rejected — silent ABI corruption (packed-register passing
// in System V x86-64 wouldn't match vāṇī's emit)
extern "C" fn point_sum(p: Point) -> i32;

// accepted — pass by reference instead
extern "C" fn point_sum(p: ref Point) -> i32;
```

Owned heap handles (`Vec<T>`, `OwnedStr`) are rejected
unconditionally: their drop semantics don't survive crossing the
foreign-code boundary. Exclusive handles (`Atomic<T>`, `Mutex<T>`,
`Channel<T, N>`, `Task`, `Guard<T>`) likewise. Pass scalars / `Str`
/ `ref T` instead and let vāṇी own the allocations.

Still queued: correct ABI lowering for small aggregates by value
(packed-register passing), varargs, function-pointer callbacks,
and packed/repr(C) layout attributes.

See `examples/ffi.vani` for the canonical demo.

### JSON diagnostics

`intentc check file.vani --json` produces a JSON object on stdout
suitable for editor integrations and CI:

```json
{
  "diagnostics": [
    {
      "level": "error",
      "message": "value 'xs' was moved; cannot use after move",
      "primary": { "file": "f.vani", "line": 5, "col": 18, "end_line": 5, "end_col": 20 },
      "related": [
        { "message": "'xs' was moved here",
          "span": { "file": "f.vani", "line": 4, "col": 21, "end_line": 4, "end_col": 23 } }
      ]
    }
  ]
}
```

The output ends with a single newline. On success, the body is
`{"diagnostics":[]}`. Without `--json`, the human-readable form goes to
stderr as before.

## Modules and namespaces

vāṇī has Rust-style inline modules with explicit paths (`::`),
compile-time visibility checks, and a `use`-declaration form for
local aliases. Everything happens at parse/check time — the
backends never see the `module` keyword. Detailed design
rationale lives in [`docs/namespaces_design.md`](docs/namespaces_design.md).

```vani
module geo {
  pub struct Point { x: i64, y: i64 }

  // Private — accessible only inside `geo`.
  fn shift(p: Point, dx: i64) -> Point {
    return Point { x: p.x + dx, y: p.y };
  }

  pub fn origin() -> Point { return Point { x: 0, y: 0 }; }
  pub fn step_right(p: Point) -> Point { return shift(p, 1); }

  // Nested modules work — bare `Point` inside `bounds` would
  // need a path (`geo::Point`) or its own `use`.
  module bounds {
    pub fn area(p: geo::Point) -> i64 { return p.x * p.y; }
  }
}

// Bring items into scope. Five forms:
use geo::Point;                          // single-item
use geo::{origin, step_right};           // multi-item brace list
use geo::*;                              // glob (direct children only)
use geo::bounds::{area as bounds_area};  // per-entry `as` rename
// use geo::*;  // would collide with the lines above — caught at compile time

fn main() -> i64 {
  let p: Point = origin();
  let r: Point = step_right(p);
  let z: i64 = bounds_area(r);
  write "step_right + bounds_area =", z;
  return 0;
}
```

### Key rules

- **Private by default.** Items inside a `module` body need `pub`
  to be reachable from outside the module.
- **`pub(kosh)`** is a finer-grained tier — exported within the
  current kosh but not through the (future) kosh boundary. Today
  it behaves identically to `pub`; the bit is preserved so
  enforcement activates once kosh boundaries ship.
- **Module-local `use`** inside `module body { … }` is scoped to
  that body. It does not leak outside or into nested submodules.
- **`pub use foo::bar;`** inside a module body re-exports the item
  under the current module's namespace (`facade::bar`).
  Re-exports are resolved transitively, so chained `pub use`
  collapses to a single hop.
- **Orphan rule.** `implement Iface for T` must live in the module
  of either `Iface` or `T`, or at the top level. Out-of-place
  impls surface a precise error.
- **Collision diagnostics.** Two `use` paths that bring the same
  local name into scope produce a precise error with a
  `use … as …;` hint. Same goes for the brace-list form.

### What's "kosh"?

**Kosh** (कोश, "treasure / repository") is vāṇī's word for what
Rust calls a *crate* — one compilation unit shipping a public
API surface. The future package registry is **Vāṇī-Kosh**. The
syntax `pub(kosh) fn …` records the intent that an item is
internal to the kosh; today vāṇī compiles a single kosh at a
time so the bit is preparatory. The full package-manager arc
(manifest → resolver → registry CLI → stdlib-as-kosh) is on the
roadmap.

## Effects, ownership, and parallelism

The language has a `pure fn` modifier and a `parallel for` loop
construct. Both are verified by a single **effects checker** that
walks the typed IR and rejects observable side effects:

  - `print` (observable I/O).
  - `assert ..., "msg"` (a runtime abort with a user-facing message).
  - `xs[i] = v` (IndexAssign — mutates a mutable buffer).
  - Reassignment over a non-`Copy` value (`Vec<T>` / `OwnedStr` drop).
  - Consuming a Vec via `for x in xs` (move-and-drop).
  - Calling a non-`pure` function. Vec mutators (`vec`, `push`,
    `set`, `clone`) and `+` on strings (heap allocation) are also
    rejected — they're observable through the allocator.

A `pure fn` body must satisfy every rule above. A `parallel for`
body is held to exactly the same rules — that's how the verifier
proves each iteration is independent and therefore data-race-free:

```intent
pure fn square(x: i64) -> i64 {
  return x * x;
}

fn main() -> i64 {
  parallel for i in 0..5 {
    let r: i64 = square(i);
    let _ = r;
  }
  return 0;
}
```

**OpenMP parallelism — both backends.**

*C backend.* Each `parallel for` is emitted as a regular C `for`
loop preceded by `_Pragma("omp parallel for")`. The
`run --backend=c` path probes the C compiler for `-fopenmp` and
adds the flag when supported; with it, iterations run on a thread
pool sized by `OMP_NUM_THREADS` (default = CPU count). Compilers
without OpenMP issue an "unknown pragma" warning and fall back to
sequential — also correct, because the verifier already proved
iteration-independent semantics.

*LLVM backend.* Each `parallel for` is lifted into an internal
`@__intent_par_<N>(i8* data)` function. The parent calls
`@GOMP_parallel(body_fn, ctx, 0, 0)` with `ctx = { i64 start,
i64 end, <capture_ptrs>... }`. The capture-pointer suffix carries
one pointer field per outer binding the body reads — the
verifier already proved every such reference is read-only, so
concurrent reads through the same pointer are race-free.

At the call site the parent stores `start`, `end`, and each
capture's parent address into the ctx struct, then bitcasts to
`i8*` and calls `@GOMP_parallel`. Inside the outlined function
each thread unpacks the captures via `getelementptr` + `load`,
registers them in its own local map, then computes its iteration
slice via `omp_get_thread_num()` / `omp_get_num_threads()` and
runs the body for that slice. Non-ref captures (scalars, arrays,
`Vec<T>`) pass the alloca pointer; ref captures (`&T`, `&mut T`)
pass the ref value itself (already a pointer). The body's
existing emit code handles either form transparently through the
normal `Var` lookup.

The `run --backend=llvm` path probes the well-known
`libgomp.so.1` location and adds `-load=<path>` to lli; the
`build` path passes `-fopenmp` to the linker so the emitted
binary is fully parallel.

**Windows hosts.** libgomp isn't available on native Windows
toolchains. When `intentc` is built on Windows the LLVM backend
omits the `@GOMP_parallel` / `omp_get_*` declarations and the
call site open-codes a hardcoded N=4 `@CreateThread` fan-out
instead: tid 0 runs synchronously on the calling thread; tids
1..3 are spawned via `@CreateThread(null, 0, fn, &warg, 0,
null)`, joined with `@WaitForSingleObject(h, -1)`, and released
with `@CloseHandle(h)`. The outlined function's signature
switches to `i8* @__intent_par_<N>(i8* %_arg)` to match the
CreateThread start-routine ABI, and reads its `tid`/`nt` from a
per-thread `WinParArg { i8* ctx, i64 tid, i64 nt }` struct
(filled at the call site) instead of calling
`omp_get_thread_num` / `omp_get_num_threads`. The captured ctx
shape is the same as on POSIX. Thread count is fixed at 4 in
v1; a future revision can plumb a runtime lookup through the
existing WinParArg without changing the outlined-fn shape.

**Note on `lli` + threading.** lli's MCJIT isn't safe for
concurrent function resolution. `intentc run --backend=llvm` sets
`OMP_NUM_THREADS=1` (unless the user overrides) so JIT'd parallel-
for runs sequentially. AOT-built binaries (`intentc build`) get
real parallelism with `OMP_NUM_THREADS` defaulting to the CPU
count.

**Reduction patterns.** A `parallel for` may carry one or more
`reduce <var> with <op>;` clauses. Supported ops:

| Op   | Variable type | C lowering              | LLVM lowering                  |
|------|---------------|-------------------------|--------------------------------|
| `+`  | integer       | `reduction(+:var)`      | `atomicrmw add`                |
| `*`  | integer       | `reduction(*:var)`      | `cmpxchg`-retry loop (mul)     |
| `&&` | bool          | `reduction(&&:var)`     | `atomicrmw and i8*` against an i8 shadow allocated in the parent (LLVM rejects atomicrmw on `i1`) |
| `\|\|` | bool        | `reduction(\|\|:var)`   | `atomicrmw or i8*` against an i8 shadow (same reason) |
| `&`  | integer       | `reduction(&:var)`      | native-width `atomicrmw and` |
| `\|` | integer       | `reduction(\|:var)`     | native-width `atomicrmw or` |
| `^`  | integer       | `reduction(^:var)`      | native-width `atomicrmw xor` |
| `min` | integer      | `reduction(min:var)`    | `atomicrmw min` (signed) / `umin` (unsigned) |
| `max` | integer      | `reduction(max:var)`    | `atomicrmw max` (signed) / `umax` (unsigned) |

For `+`, `*`, `&&`, and `||` the checker requires the body to
update `<var>` only as `<var> <op> <expr>` (or `<expr> <op>
<var>`). `min` and `max` are built-in pure intrinsics, so the
body must instead read `<var> = min(<var>, <expr>)` (or
`min(<expr>, <var>)`); same for `max`. In every case the checker
also forbids reads of `<var>` anywhere else in the body —
partial-value visibility would leak otherwise.

The bool-reduction shadow works as follows: at the parallel-for
entry the parent zext-stores the current bool value into a
freshly-allocated `i8` cell, captures the shadow's address into
the outlined fn's ctx struct, and the outlined fn runs
`atomicrmw and/or i8*` against it. On return the parent reads
the shadow, computes `icmp ne i8 …, 0`, and stores the i1 back
into the original alloca.

```intent
let total: i64 = 0;
parallel for i in 0..len(xs)
reduce total with +;
{
  total = total + xs[i];
}
print total;  // sum of xs[0..len(xs)]
```

See `examples/parallel.vani` for a runnable end-to-end
demonstration on both backends.

**Task handles.** `task <name> { … }` declares an affine
`Task` handle and a side-effect-free body. The same purity
rules as a `parallel for` body apply (no `print`, no
`IndexAssign` on captured bindings, no impure calls), and each
handle must be consumed by exactly one `join <name>;` in the
same block — a forgotten join or a double join is a checker
error.

```intent
fn main() -> i64 {
  let xs: [i64; 4] = [2, 3, 4, 5];
  task ta {
    let a: i64 = xs[0] * xs[0];
    let _ = a;
  }
  task tb {
    let b: i64 = xs[3] * xs[3];
    let _ = b;
  }
  join ta;
  join tb;
  return 0;
}
```

Both backends now lower `task` to a real pthread spawn: the
body is outlined into a per-spawn function that receives a
heap-allocated ctx struct holding the captures, the spawn
site calls `pthread_create`, and `join` calls
`pthread_join` and frees the ctx. Captures are restricted
to Copy types — affine handles (Vec/Atomic/Mutex/Guard/
Channel/arrays/OwnedStr) can't ride the ctx by value, so
the supported pattern is to pre-extract scalar values from
them before the spawn site. See `examples/tasks.vani` for
the canonical shape.

**Atomic cells.** The affine model rejects shared mutable
state by default — that's why `parallel for` bodies can't
`IndexAssign` on captured arrays, and why two tasks can't
both own the same `Vec<T>`. For the patterns the affine model
can't express (counters, lock-free queues, lazy caches),
`Atomic<T>` is the opt-in escape hatch. T ranges over the
integer widths `i8`..`i64`, `u8`..`u64`, and `bool`; the five
sequentially-consistent builtins below dispatch on element
width and emit width-appropriate atomic ops on both backends.
`Atomic<bool>` uses an i8 shadow in LLVM (zext/trunc at every
operand boundary because `i1` atomics aren't byte-addressable);
`atomic_fetch_add` is rejected on bool by the checker.

| Builtin                                        | Returns |
|------------------------------------------------|---------|
| `atomic_new(initial: T) -> Atomic<T>`          | affine handle (owned) |
| `atomic_load(a: &Atomic<T>) -> T`              | current value |
| `atomic_store(a: &Atomic<T>, v: T) -> T`       | the stored value (echo) |
| `atomic_fetch_add(a: &Atomic<T>, v: T) -> T`   | the OLD value (pre-add) |
| `atomic_compare_exchange(a: &Atomic<T>, expected: T, new: T) -> bool` | true on success (cell was `expected`, now `new`); false on failure |

All five are unconditionally safe across threads — there's
no need to wrap them in `Mutex` or `Arc`. The C backend lowers
storage as `_Atomic <T>` and uses the C11 `<stdatomic.h>` ops
(`atomic_load_explicit`, `atomic_store_explicit`,
`atomic_fetch_add_explicit`, `atomic_compare_exchange_strong_explicit`,
all with `memory_order_seq_cst`); the LLVM backend emits
width-matched `load atomic iN … seq_cst, align M`, the
matching `store atomic`, `atomicrmw add iN* …`, and
`cmpxchg iN* …` (`atomic_storage_llvm` + `atomic_align` map
each supported element to its IR type and natural alignment).
The handle itself is affine: `Atomic<T>` is not Copy, so each
cell has a unique identity that two threads can share only
via references.

```intent
fn main() -> i64 {
  let counter: Atomic<i64> = atomic_new(0);
  let _o1: i64 = atomic_fetch_add(&counter, 5);
  let _o2: i64 = atomic_fetch_add(&counter, 7);
  return atomic_load(&counter);  // 12
}
```

See `examples/atomics.vani` for a runnable demonstration.

**Channels.** `Channel<T>` is an affine handle to a 16-slot
bounded ring buffer with monotonic `head` / `tail` atomic
counters. `channel_send` blocks (spin) when the buffer is
full; `channel_recv` blocks when it's empty. The buffer
preserves FIFO order — send-send-send-recv-recv-recv returns
the values in the original order. Suitable for hand-off
pipelines where one side produces a small batch before
another consumes. `Channel<T>` defaults to capacity 16; `Channel<T, N>` lets
the user pick the ring size (any power of two ≥ 1). T
ranges over the integer widths `i8`..`i64` / `u8`..`u64`
plus `bool` (the LLVM backend stores bool slots as `[N x
i8]` and zext/trunc's the source-level i1 at each slot
boundary; C uses native `bool buf[N]`). Both backends
generate one per-`(T, N)` struct + runtime helpers, so a
program using `Channel<i64, 16>` and `Channel<i32, 8>`
emits both bundles side by side. The ring uses Vyukov-style
per-slot sequence numbers (`seq[i & (N-1)]`): a producer
enters round `t` only when `seq[t & MASK] == t`, then
publishes via `store atomic seq = t+1`; the consumer waits
for `seq == h + 1` before reading and releases the slot via
`store atomic seq = h + CAP`. This makes the channel MPSC-
safe — producers don't collide on slot claim and consumers
never see unpublished data. (Real-thread parallelism still
waits on the task lowering — see TODO #5.)

| Builtin                                          | Returns |
|--------------------------------------------------|---------|
| `channel_new() -> Channel<T>`                    | affine handle (owned) |
| `channel_send(ch: &Channel<T>, v: T) -> T`       | the sent value (echo) |
| `channel_recv(ch: &Channel<T>) -> T`             | the received value |

```intent
fn main() -> i64 {
  let ch: Channel<i64> = channel_new();
  let _ = channel_send(&ch, 42);
  return channel_recv(&ch);  // 42
}
```

**Mutexes with RAII guards.** `Mutex<T>` is an affine handle to
a value protected by Drepper's three-state futex lock on
Linux. Fast path: a single seq_cst compare-exchange from
unlocked (state=0) to locked-no-waiters (state=1). Under
contention the waiter atomically marks state=2 (waiters
present) and parks in `syscall(SYS_futex, FUTEX_WAIT_PRIVATE)`;
the unlocker `atomic_fetch_sub`s the state and on the
waiters-present path calls `FUTEX_WAKE_PRIVATE` to release one
parked thread. Non-Linux builds fall back to a portable
`sched_yield()` backoff. `mutex_lock(&m)` returns an affine
`Guard<T>` whose scope-exit drop releases the lock — the
RAII pattern. Multiple operations on the value can run under
the same lock acquisition (unlike `Atomic<T>`, where each
call is a single atomic op).

| Builtin                                            | Returns |
|----------------------------------------------------|---------|
| `mutex_new(initial: T) -> Mutex<T>`                | affine mutex (owned) |
| `mutex_lock(m: &Mutex<T>) -> Guard<T>`             | affine guard (owned) |
| `guard_get(g: &Guard<T>) -> T`                     | the protected value |
| `guard_set(g: &Guard<T>, v: T) -> T`               | the stored value (echo) |

```intent
fn double_in_place(m: &Mutex<i64>) -> i64 {
  let g: Guard<i64> = mutex_lock(m);
  let cur: i64 = guard_get(&g);
  let next: i64 = cur + cur;
  let _ = guard_set(&g, next);
  return next;
  // `g` drops here — backend emits the unlock atomic store.
}
```

The C backend declares static-inline runtime helpers for both
(`<stdatomic.h>` ops with `seq_cst` ordering); the LLVM backend
emits inline atomic ops + `cmpxchg`-retry spin loops. Both v1
lowerings are sequential — there's no real threading yet — but
the runtime atomicity is correct so a future threading backend
inherits race-freedom for free.

The checker statically rejects **double acquisition** of the
same mutex while a guard is still alive. The lock is
non-reentrant, so the deadlock that would otherwise occur at
runtime turns into a compile-time error:

```intent
let m: Mutex<i64> = mutex_new(0);
let g1: Guard<i64> = mutex_lock(&m);
let g2: Guard<i64> = mutex_lock(&m);  // error: mutex 'm' is already locked
```

Sequential locks (where the first guard drops before the second
lock) and simultaneous locks on different mutexes are both
accepted. The check is syntactic — it fires when the
`mutex_lock` argument is a direct `&Var` reference or a
bare reference-typed binding; indirect arguments
(`mutex_lock(get_mutex_ref())`) skip the check rather than
overreport.

The same check extends **across function boundaries**. Each
function's signature carries a per-parameter flag for "this
parameter gets locked somewhere in my body". At a call site,
if the caller holds a live guard on a mutex AND the callee is
known to lock the corresponding parameter, the call would
deadlock on entry — flagged at compile time:

```intent
fn lock_it(m: &Mutex<i64>) -> i64 {
  let g: Guard<i64> = mutex_lock(m);
  return guard_get(&g);
}
fn main() -> i64 {
  let m: Mutex<i64> = mutex_new(0);
  let g: Guard<i64> = mutex_lock(&m);
  let _ = lock_it(&m);   // error: cross-function double acquisition
  return 0;
}
```

The cross-function analysis is **transitive**: a
fixpoint pass over the call graph propagates `locks_params`
across calls. So if `helper(m)` returns `lock_it(m)` and
`lock_it` locks its parameter, then `helper` also locks its
parameter, and the call site `helper(&m)` is flagged when
the caller holds a guard on `m`. Calls are inspected by name
in v1 — a function-pointer-style indirect dispatch would
require dataflow on the SSA layer.

See `examples/concurrency.vani` for a runnable demonstration.

**Function pointers.** `fn(T1, T2, ...) -> R` is a first-class
type. A top-level function name in expression position yields
its function-pointer value, so functions can be passed as
arguments or stored in let bindings of fn-ptr type. Calls
through a fn-ptr binding lower to native function-pointer
invocation (C function pointer / LLVM
`call <ret> (<params>) %ptr(args)`).

```intent
pure fn double(x: i64) -> i64 { return x + x; }
fn apply(f: fn(i64) -> i64, x: i64) -> i64 { return f(x); }
fn main() -> i64 { return apply(double, 7); }   // 14
```

Indirect calls bypass the name-based purity / lock-graph
passes by construction (no signature to consult). The
checker accordingly rejects `CallIndirect` inside
`parallel for` bodies, task bodies, and `pure fn` contexts;
the cross-function deadlock detector reports nothing about
indirect callees rather than making false claims. The SSA
pipeline does not yet lower fn-ptr shapes — the tree-based
backends handle them directly.

See `examples/fn_pointers.vani` for a runnable demonstration.

## Commands

The compiler has two backends: **LLVM IR (default)** and C (legacy,
on the deprecation path). `--backend=c` opts back into the C output
for `emit` / `run`; the `emit-c` subcommand is a stable alias for
C-only emission. `run` invokes `lli` for LLVM IR and `cc` for C
output. `build` produces a native binary via `llc` + `cc` (linker
only — no C source is compiled).

### Build & run pipeline

```bash
cargo run -- check examples/basics.vani                 # Type-check + verify
cargo run -- check examples/basics.vani --json          # JSON diagnostics
cargo run -- check examples/basics.vani --no-verify     # Skip SMT (dev opt-out)

cargo run -- emit examples/basics.vani                  # LLVM IR (default)
cargo run -- emit examples/basics.vani --backend=c      # C output
cargo run -- emit examples/basics.vani -o /tmp/basics.ll
cargo run -- emit-c examples/basics.vani                # Legacy alias for --backend=c

cargo run -- run examples/basics.vani                   # LLVM via lli (default)
cargo run -- run examples/basics.vani --backend=c       # C via cc

cargo run -- build examples/basics.vani -o /tmp/basics  # AOT native binary
                                                          # (LLVM → llc → cc linker)
```

### Debug subcommands

Useful for hacking on the lexer / parser / checker. Each runs the
pipeline up to a stage and dumps a debug-format representation.

```bash
cargo run -- tokens examples/basics.vani   # Token stream from the lexer
cargo run -- ast    examples/basics.vani   # Parsed AST (skips type checker)
cargo run -- ir     examples/basics.vani   # Typed IR (what the backends see)
```

### Running every example

```bash
cargo test                                                # Full suite + examples
cargo test llvm_backend_run_produces_same_output_as_c     # Cross-backend parity
```

### Editor integration via LSP

A minimal Language Server ships as the `intent-lsp` binary:

```bash
cargo build --bin intent-lsp
./target/debug/intent-lsp        # speaks LSP over stdio
```

Capabilities today:

- `textDocument/publishDiagnostics` — lexer / parser / checker
  errors pushed on every `didOpen` and `didChange`, with byte
  spans mapped to LSP line/character ranges.
- `textDocument/hover` — returns the inferred type of the
  smallest typed expression covering the cursor (e.g. hovering
  on `42` reports `: i64`, on `xs[i] + bias` reports the
  promoted integer type). Returns nothing while the document
  doesn't parse / check; reach a green state to see hover.
- `textDocument/definition` — goto-definition. Click on a
  binding reference (a `Var`, `&Var`, or `&mut Var`); the
  server returns a `Location` pointing at the binding's
  declaration site. Synthetic checker-inserted names (return
  temps, iteration counters) are filtered so navigation only
  lands on user-written declarations. `TypedStmt::Let`
  doesn't yet carry a dedicated span, so the declaration
  range is the let's RHS expression span — close enough for
  editors to land in the right neighborhood.
- `textDocument/references` — find all references.
  Resolves the binding at the cursor (each `Var` / `Ref` /
  `RefMut` carries its declaration site as
  `TypedExpr::binding_decl_span`) and collects every other
  occurrence with the matching `decl_span` — so two
  same-name bindings in different scopes stay separate.
  Honors the client's `includeDeclaration` flag.
- `textDocument/rename` — rename a binding everywhere it
  appears. Reuses references to collect occurrences,
  prepends the declaration site, and returns a
  `WorkspaceEdit` whose `TextEdit`s replace each span with
  the new name. Validates the new name syntactically (must
  match `[A-Za-z_][A-Za-z0-9_]*`) and rejects collisions
  with reserved keywords; the editor surfaces these as
  user-visible errors before applying. Scope-aware via the
  same `binding_decl_span` resolution.
- `textDocument/completion` — invocation-triggered
  completion (Ctrl+Space; no automatic trigger characters
  yet). Returns: language keywords + type names + the fixed
  builtin function set (always; works even when the doc
  doesn't compile, so mid-typed edits still get useful
  suggestions); plus every top-level function name and the
  in-scope bindings of the function the cursor is inside —
  found by checking each `TypedFunction`'s `span` against
  the cursor's byte offset. Parameters of *sibling*
  functions are no longer leaked into the completion list.
  Bindings declared after the cursor in the same scope are
  also excluded.
- `textDocument/codeAction` — quick fixes triggered by
  diagnostics in the request context. v1 recognizes one
  pattern: a parser diagnostic whose message says
  `expected '<TOK>'` for a single-character token produces
  an "Insert `<TOK>`" quick fix that patches the source at
  the diagnostic's end. The action is marked
  `is_preferred: true` so editors configured to auto-apply
  the preferred quick fix on save will close the trivial
  cases (missing `;`, `)`, `}`, …) without user
  intervention. Adding more patterns is straightforward —
  the fix-classifier is one helper per pattern.
- `textDocument/semanticTokens/full` — lex-driven semantic
  highlighting with IR-driven refinement. Re-lexes the
  source and assigns each token a type from the legend
  (`variable`, `function`, `parameter`, `type`, `keyword`,
  `number`, `string`). Type primitives (`i64`, `Vec`, etc.)
  and known type-position identifiers (`Atomic`, `Channel`,
  `Mutex`, `Guard`, `Task`, `Str`/`OwnedStr`) become `type`;
  `min`/`max` become `function`; literals get
  `number`/`string`; identifier-shaped tokens default to
  `variable`. When the document compiles, the typed IR is
  walked to override token types at known callee and
  parameter-declaration spans (using the `name_span` fields
  on `TypedExprKind::Call` and `TypedParam`): a `Call`
  callee is upgraded to `function` and a parameter
  declaration to `parameter`. Two semantic-token modifiers
  ship as well: `declaration` (set on parameter declaration
  sites) and `readonly` (set on parameter declarations and on
  every `Var` read whose `binding_decl_span` resolves to a
  parameter — parameters can't be reassigned without
  shadowing). Returns the empty token list on lex errors so
  the editor's UI stays responsive during mid-edit states.

Point your editor at `intent-lsp` for `*.vani` files. For
Neovim with `nvim-lspconfig`:

```lua
local lspconfig = require('lspconfig')
local configs = require('lspconfig.configs')
if not configs.vani then
  configs.vani = {
    default_config = {
      cmd = { 'intent-lsp' },
      filetypes = { 'vani' },
      root_dir = lspconfig.util.find_git_ancestor,
      settings = {},
    },
  }
end
lspconfig.vani.setup({})
```

The cross-backend parity test runs every file under `examples/`
through both `--backend=c` and `--backend=llvm` and diffs stdout
+ exit codes. New examples are picked up automatically when wired
into `check_examples_all_succeed` and a `run_<example>_example`
test (see `tests/run_end_to_end.rs`).

## Design Philosophy & Limitations

vāṇī aims for a **small, fully-verifiable surface** — the core
primitives compose into richer patterns rather than the language
growing a new built-in for every shape. Three design decisions
that come up frequently:

### Composition over inheritance — `dyn Iface` is the escape hatch

Interfaces dispatch **statically** by default: `fn min<T>(a: T, b: T)
-> T where T is Cmp` monomorphizes at each call site to the concrete
T's `cmp` impl. Composition (struct fields + interface bounds) covers
most patterns; static dispatch is faster and lets the verifier see
through every call site.

For **heterogeneous collections** (the workflow where static dispatch
falls short), use `dyn Iface` — a fat pointer carrying `{ &vtable,
&data }` (16 bytes) that holds any T implementing the interface:

```intent
struct Circle { r: i64 }
struct Square { side: i64 }

interface Drawable {
  fn area(self: Circle) -> i64;
}

implement Drawable for Circle { fn area(self: Circle) -> i64 { … } }
implement Drawable for Square { fn area(self: Square) -> i64 { … } }

fn total_area(shapes: Vec<dyn Drawable>) -> i64 {
  let total: i64 = 0;
  for s in shapes { total = total + s.area(); }
  return total;
}
```

`dyn` works at let bindings, fn params (owned + `ref dyn` borrow),
struct fields, and `Vec<dyn Iface>`. No inheritance, no abstract
base classes — just a per-interface vtable with one fn-ptr per
method in declaration order. See
[examples/dyn_dispatch.vani](examples/dyn_dispatch.vani) for the
end-to-end shape. Tagged enums (`enum Shape { Circle(...), … }`) are
still a fine alternative when the variant set is closed and known
at the call site.

### `try` is a value-flow shortcut, not an exception system

vāṇī has no exceptions, no `catch`, no stack unwinding. Errors are
**values** carried in payloaded enums (Option-like, Result-like).
The `try` keyword is the Rust `?` operator — sugar for early-return
on the None / Err arm:

```intent
fn run(opt: Opt) -> Opt {
  let v: i64 = try opt;          // None? return Opt.None now.
  return Opt.Some(v + 1);        // happy path.
}
```

To "catch" a possible-None value, use `match`:

```intent
match maybe_value {
  Opt.Some(v) then use(v),       // happy path
  Opt.None then handle_missing(), // "catch" the None
}
```

Every control-flow path is statically visible — no hidden unwind
from any call site. `assert` triggers `abort()`; there's no
mechanism to recover from a failed assertion.

### Data structures + algorithms — affine-first roadmap

vāṇी keeps **affine ownership** as the standing language decision.
Every container, algorithm, or API on the roadmap is flagged for
compliance — items that fight single-owner semantics are explicitly
marked with the affine-friendly substitute named in the same row.

**Flag legend.** ✅ AFFINE — single-owner holds end-to-end.
⚠️ AFFINE-TENSION — compiles, but the API needs a careful contract
(e.g. `get` returns `Option<ref V>` not `V`; `remove() -> Option<V>`
is the move-out path; `insert(k, v)` consumes both).
🛑 NON-COMPLIANT — cannot ship as designed; substitute named.

**Shipped today.** Backbone primitives that already cover ~70% of
real use cases:

| Structure | Shipped form | Flag |
|-----------|--------------|------|
| Stack | `Vec<T>` + `push(mut ref xs, v)` + `pop(mut ref xs)` (closure #219) | ✅ AFFINE |
| Sort (in-place) | `sort(mut ref xs)` / `sort_by(mut ref xs, cmp: fn(i64, i64) -> i64)` on `Vec<i64>` (closure #293) | ✅ AFFINE |
| Reverse / dedup | `reverse(mut ref xs)` (any Copy element) / `dedup(mut ref xs)` (Vec<i64>) (closure #294) | ✅ AFFINE |
| Search | `find(ref xs, needle) -> Option<i64>` / `contains(ref xs, needle) -> bool` / `binary_search(ref xs, needle) -> Option<i64>` on `Vec<i64>` (closure #295) | ✅ AFFINE |
| Mutators | `swap_remove(mut ref xs, i) -> T` / `insert(mut ref xs, i, v) -> i64` / `clear(mut ref xs) -> i64` (any non-array element) (closure #296) | ✅ AFFINE |
| Array ops | `sort` / `sort_by` / `reverse` / `find` / `contains` / `binary_search` extended to `[i64; N]` (closure #297) | ✅ AFFINE |
| String ops | `str_contains` / `str_starts_with` / `str_ends_with` -> bool, `parse_int` / `parse_float` -> `Option<i64>` / `Option<f64>` (closure #298) | ✅ AFFINE |
| Math | `pow` / `sqrt` / `sin` / `cos` / `tan` / `floor` / `ceil` (f64 -> f64), `abs` overloaded (i64 -> i64, f64 -> f64) (closure #299) | ✅ AFFINE |
| RNG | `seed_rng(u64)` / `rand_i64()` / `rand_in_range(lo, hi)` — thread-local xorshift64 (closure #300) | ✅ AFFINE |
| Hash | `hash_i64(i64)` / `hash_str(Str)` / `hash_combine(u64, u64)` -> `u64` — FNV-1a (closure #301) | ✅ AFFINE |
| BinaryHeap | `heap_push` / `heap_pop` / `heap_peek` / `heapify` on `Vec<i64>` — min-heap (closure #302) | ✅ AFFINE |
| Deque | `Deque<i64>` ring buffer w/ 8 builtins (new / push_back / push_front / pop_back / pop_front / peek_back / peek_front / len) (closure #303) | ✅ AFFINE |
| HashSet | `HashSet<i64>` open-addressing hash set w/ 4 builtins (new / insert / contains / len) (closure #304) | ✅ AFFINE |
| HashMap | `HashMap<i64, i64>` open-addressing key/value map w/ 5 builtins (new / insert / get / contains_key / len) (closure #305) | ✅ AFFINE under v1 Copy-V; ⚠️ AFFINE-TENSION when V goes non-Copy |
| BTreeSet | `BTreeSet<i64>` ordered set on sorted-Vec backing w/ 5 builtins (new / insert / contains / remove / len) (closure #306) | ✅ AFFINE |
| BTreeMap | `BTreeMap<i64, i64>` ordered key/value map on parallel-sorted-Vec backing w/ 6 builtins (new / insert / get / contains_key / remove / len) (closure #307) | ✅ AFFINE under v1 Copy-V; ⚠️ AFFINE-TENSION when V goes non-Copy |
| UnionFind | Disjoint-set with path-compressed find + union-by-rank; parallel `parent`/`rank` i64 arrays; 5 builtins + method sugar (closure #325, **first Level 4 arena container**) | ✅ AFFINE |
| BinaryHeap (dedicated) | `BinaryHeap<T>` first-class affine handle (i64 v1) backed by `i64*` + `len` + `cap`; min-heap; 5 builtins (`new` / `push` / `pop` / `peek` / `len`) + method sugar; `pop`/`peek` return `Option<i64>` (closure #326, **Level 4 #2**) | ✅ AFFINE |
| BloomFilter | `BloomFilter` probabilistic membership tester; bit array + `num_bits` + `num_hashes` + `insert_count`; double-hashing on FNV-1a; 5 builtins (`new` / `insert` / `contains` / `len` / `count`) + method sugar; false positives possible, false negatives impossible; v1 keys are i64 (closure #327, **Level 4 #6**) | ✅ AFFINE |
| Bst | `Bst<T>` binary search tree on a node arena; parallel `keys` (i64) + `left` / `right` (i32 child indices) arrays + root index; 7 builtins (`new` / `insert` / `contains` / `remove` / `len` / `min` / `max`) + method sugar; `min` / `max` return `Option<i64>`; v1 unbalanced — AVL / RB rotations queued (closure #328, **Level 4 #3**) | ✅ AFFINE |
| Graph | `Graph` weighted directed graph; per-edge parallel `edge_src` / `edge_dst` (i32) + `edge_weight` (i64) arrays; 7 builtins (`new` / `add_edge` / `num_nodes` / `num_edges` / `bfs_reach` / `dfs_reach` / `dijkstra`) + method sugar; `dijkstra` returns `Option<i64>` (O(V²) — no BinaryHeap dependency); v1 i64 weights, i32 nodes (closure #329, **Level 4 #5**) | ✅ AFFINE |
| Trie | `Trie` prefix tree on a node arena; flat 26 × num_nodes i32 `children` + per-node `is_end` byte; 6 builtins (`new` / `insert` / `contains` / `starts_with` / `len` / `node_count`) + method sugar; v1 a-z alphabet — operations short-circuit to false on any non-a-z input character (closure #330, **Level 4 #4**) | ✅ AFFINE |
| Anon fn | `fn(p: T) -> R { body }` in value position; lambda-lifted to `__anon_fn_<N>` (closure #308). v1: no captured environment | ✅ AFFINE |
| Closures w/ captures | `let f = fn(x) -> R { ...captured_n... }; f(...)` — capture-by-value of Copy outer bindings; callable in same fn only (closure #314). Closures may be declared at top level or inside `if`/`while`/`for` bodies (closure #315) | ✅ AFFINE under v1 Copy contract |
| Iter combinators | Eager `vec_map(ref xs, f) -> Vec<i64>` + `vec_filter(ref xs, p) -> Vec<i64>` + `vec_fold(ref xs, init, g) -> i64` on Vec<i64> (closures #309 + #310); slicing `vec_take(ref xs, n)` / `vec_drop(ref xs, n)` (closure #313); fused single-pass family `vec_map_fold` / `vec_filter_fold` / `vec_map_filter` / `vec_map_filter_fold` (closures #316 + #317). Pair with anon fns or top-level fn-refs | ✅ AFFINE |
| Method-call sugar | `xs.map(f)` / `xs.filter(p)` / `xs.fold(init, g)` / `xs.sort_by(cmp)` / `xs.sort()` on `Vec<T>` receivers desugar to the builtins (closure #311); `m.get(k)` / `m.insert(k, v)` / `s.contains(v)` / `d.push_back(v)` / `.len()` etc. on HashMap / HashSet / BTreeMap / BTreeSet / Deque (closure #312); `xs.take(n)` / `xs.drop(n)` / uniform `xs.len()` on Vec (closure #313); Vec mutators + search (`xs.push(v)` / `xs.pop()` / `xs.reverse()` / `xs.dedup()` / `xs.swap_remove(i)` / `xs.insert(i, v)` / `xs.clear()` / `xs.find(v)` / `xs.contains(v)` / `xs.binary_search(v)`) (closure #320); `[T; N]` Array sugar (`arr.sort()` / `arr.sort_by(cmp)` / `arr.reverse()` / `arr.find(v)` / `arr.contains(v)` / `arr.binary_search(v)`) (closure #321) | ✅ AFFINE |
| Queue (concurrent) | `Channel<T, N>` MPSC ring buffer w/ futex blocking | ✅ AFFINE |
| Wait / signal | `Condvar` w/ `wait` / `wait_timeout` / `notify_one` / `notify_all` (closure #292) | ✅ AFFINE |
| Array (fixed) | `[T; N]` w/ nested-array support (closure #291) | ✅ AFFINE |
| Heap-vec | `Vec<T>` incl. `Vec<Vec<T>>`, `Vec<Struct{OwnedStr…}>` | ✅ AFFINE |
| Owned string | `OwnedStr` from `"a" + "b"`; `Str` for borrowed | ✅ AFFINE |
| Result / Option | Prelude-injected generic enums (#282 + #281) | ✅ AFFINE |
| Shared atomic | `Atomic<T>` for shared counters | ✅ AFFINE |
| Shared mutable | `Mutex<T>` + `Guard<T>` | ✅ AFFINE |
| Fallible alloc | `try_vec(n) -> Result<Vec<i64>, AllocError>` (#284) | ✅ AFFINE |

**Sequenced queue.** Full per-item detail (with implementation
plan and affine contract) lives in [TODO.md](TODO.md) under the
*Data structures + algorithms roadmap* section.

| Level | Items | Affine flag |
|-------|-------|-------------|
| **1 — Operations on existing primitives** ✅ **COMPLETE** | `Vec.sort` / `sort_by(fn)` (#293) · `Vec.reverse` / `Vec.dedup` (#294) · `Vec.find` / `contains` / `binary_search` (#295) · `Vec.swap_remove` / `insert` / `clear` (#296) · Array ops on `[i64; N]` (#297) · `str_contains` / `str_starts_with` / `str_ends_with` / `parse_int` / `parse_float` (#298) · Math: `pow` / `sqrt` / `sin` / `cos` / `tan` / `floor` / `ceil` + overloaded `abs` (#299) · RNG: `seed_rng` / `rand_i64` / `rand_in_range` (#300) · Hash: `hash_i64` / `hash_str` / `hash_combine` (FNV-1a) (#301) | ✅ AFFINE |
| **2 — Generic containers** (deps: Level 1, generic decls #281) ✅ **COMPLETE** | ✅ BinaryHeap-on-Vec (#302) · ✅ `Deque<i64>` (#303) · ✅ `HashSet<i64>` (#304) · ✅ `HashMap<i64, i64>` (#305, AFFINE under Copy-V; AFFINE-TENSION queued for non-Copy V) · ✅ `BTreeSet<i64>` (#306, sorted-Vec backing) · ✅ `BTreeMap<i64, i64>` (#307, parallel sorted-Vec backing). Dedicated `BinaryHeap<T>` wrapper landed at Level 4 (#326); node-arena B-tree variants → Level 4 | ✅ / ⚠️ AFFINE-TENSION |
| **3 — Closures + iterators** | ✅ Anonymous fn expressions w/o captures (#308) · ✅ Eager `vec_map` / `vec_fold` / `vec_filter` on Vec<i64> via fn-ptr args (#309 + #310) · ✅ Method-call sugar across Vec + affine containers (#311 + #312) · ✅ `vec_take` / `vec_drop` + uniform `xs.len()` (#313) · ✅ Closures w/ captured state (#314 + nested scopes #315) · ✅ Fused single-pass combinators `vec_map_fold` / `vec_filter_fold` / `vec_map_filter` / `vec_map_filter_fold` (#316 + #317) · ✅ Auto-fusion of `vec_map + vec_fold` chains (#318). ⏳ Auto-fusion of more chain shapes; non-Copy captures; capture-by-ref; passing closures as fn-ptr args; `.collect()` / `vec_zip` | ✅ / ⚠️ AFFINE-TENSION |
| **4 — Advanced / domain-specific** | BST / AVL / red-black via node arena + `i32` child indices (✅), B-tree arena (✅), Trie arena (✅), graphs as `Vec<Node>` + `Vec<Vec<u32>>` adjacency (✅), graph algorithms BFS / DFS / Dijkstra / A* / topo / Kruskal / Prim (✅), Union-Find (✅), skip list (✅), Bloom filter (✅) | ✅ AFFINE |

**Deferred / non-compliant** (flagged with reasoning + substitute):

| Item | Why non-compliant | Substitute |
|------|-------------------|------------|
| 🛑 Doubly-linked list w/ raw `prev` / `next` pointers | Two pointers into one node violate single-owner | Index-based Deque (Level 2 #15); index-based BST (Level 4 #20) |
| 🛑 Rc / Arc reference-counted shared ownership | Cycles defeat cycle-free Drop; deliberate v1 trade-off | Index-based graphs (Level 4 #23) for shared refs; `Channel<T, N>` for cross-task ownership; `Mutex<T>` for shared mutable |
| 🛑 Iterators yielding owned `T` | Would move every element out; tail Drop then double-frees | `for x in xs` already iterates by Copy-value (Copy T) or by-ref (non-Copy T); combinator chain (Level 3 #18) is by-ref or consume-whole-Vec via `.fold` / `.collect` |
| 🛑 Self-referential structs (Pin / pinning) | Affine moves invalidate self-pointers | Index-based arena pattern (Level 4 #20–#23) |
| 🛑 Garbage collector (any flavor) | Duplicates affine's deterministic Drop; defeats no-runtime promise | Affine + scope-exit Drop already covers it |

**The principle remains: add a new built-in only when no composition
of existing primitives gets within an order of magnitude of optimal.**
The roadmap above is what to ship — and *how to ship it under affine*
— not a wishlist of every container ever designed.

### Current limitations

The honest list, grouped by which work item closes them:

**Type system**

- Tuples are Copy-only — no `OwnedStr` / `Vec<T>` in a tuple element.
- Generic monomorphization supports first-arg literal inference and
  one type parameter per fn (`<T>`, not `<T, U>`).
- No closures — only top-level `fn` pointers via the `fn` keyword.
- No `bool ↔ int` cast (deliberate — forces explicit branching).
- `Mutex<T>` restricted to `Mutex<i64>` (other widths waiting on a
  parametric runtime helper).
- Type aliases can't be recursive.

**Affine ownership**

- Partial-move tracking is one level deep — `let xs = t.x` works,
  but `let y = t.x.inner` (nested field move) is rejected (epic B).
- `Drop for T` accepts both `fn drop(self: T)` (by-value — only
  valid when T has no heap-owning fields; consumes self) and
  `fn drop(self: mut ref T)` (runs first, then the auto-per-field
  free runs — works for any T including heap-owning fields).

**`try` desugar**

- `while` loops between `try` and `return` aren't supported —
  while doesn't have a single tail-expression for the Some-arm
  to absorb. `if cond { return X; }` guards work via the
  AST-level guard-if rewriter (closure #232).

**Block expressions**

- `let r = { stmts; tail-expr };` admits `let`, `print`, and
  assignment statements. No inner control flow (`if`/`while`/`for`).
  Hoist control flow outside the block.

**Memory & runtime model**

- No GC, no Rc / Arc — affine + scope-exit Drop only.
- Async / await / coroutines: **queued** (see *Async / asyncio*
  in [TODO.md](TODO.md)). The canonical path is compiler-lowered
  state machines on an arena — explicitly NOT Rust-style `Pin<&mut
  Self>` self-references (those stay 🛑 NON-COMPLIANT under
  affine). Today's concurrency is real threads (`task` + `join`)
  plus shared-state via `Atomic` / `Mutex` / `Channel`.
- No exceptions (covered above).

**Tooling**

- Devanagari script support parked at user request — keyword
  aliases land, multi-word aliases + script-aware diagnostics
  deferred.

What *does* work well (so the limitation list reads in context):
all 58 examples are leak-clean under `gcc -fsanitize=address,leak`,
UBSan-clean, LLVM `opt -verify` clean, cross-backend stdout-parity
tested, and SMT-verified (z3 discharge of `requires` / `ensures` /
`prove` / `invariant`). The `for` body of a `parallel for` is
proved race-free before lowering to OpenMP.

## Why Rust

Rust fits the compiler core because it gives:

- fast lexing, parsing, type checking, and lowering
- strong ownership and enum modeling for AST/IR invariants
- deterministic builds and single-binary distribution
- good FFI and ABI integration
- easy migration to Cranelift, LLVM, or direct assembly backends
- safe concurrency for future parallel compilation and optimization passes

Python still belongs in the system as:

- a research harness
- benchmark runner
- AI planning/orchestration layer
- fuzzing and corpus tooling
- notebook-style design exploration

## Roadmap

The work splits into two queues: **small items** (each independently
landable, < 1 session) and **multi-session items** (each touches
checker + IR + multiple backends + tests, ordered by dependency then
effort). See [STATUS.md](STATUS.md) for the live "Known Issues" list and
[TODO.md](TODO.md) for the full closure history.

### Small items

These are contained surface gaps and diagnostic polishes. Most of the
"todo" side will land naturally when the corresponding multi-session item
lands.

**Done (most recent first):**

- ✅ Devanagari Sanskrit / Hindi / Marathi 3-way alias parity —
  `वरना` (else, Hindi), `परिवर्तनीय` (mut), `अग्रे` (continue,
  Sanskrit), `सार्वजनिक` (pub), `खण्ड` (module), `उपयोग` (use),
  `यथा` (as), `यत्र`/`जहाँ`/`जिथे` (where), `अस्ति`/`है`/`आहे`
  (is), `संकेत` (interface), `कार्यान्वित` (implement), `विधि`
  (methods), `प्रयास` (try), `नियोग` (task), `संयोजन` (join),
  `समानांतर` (parallel single-word) — closure #267
- ✅ Devanagari SOV verb-at-end statements — `X पुनरागम;` /
  `… लिखो;` / `cond सुनिश्चित;` / `expr प्रमाण;` — closure #266
- ✅ Devanagari SOV word order for range `for` — `i के लिए 0 से
  5 तक { … }` — closure #265
- ✅ SSA-LLVM multi-block parallel-for atomicrmw emit via
  Phi-traceback (closure #264) — multi-block bodies no longer
  fall back to tree-LLVM
- ✅ Codegen fixes: SSA-LLVM identity-cast `bitcast` for pointer
  types (#263); `len(ref OwnedStr)` 4-layer dereference fix (#262)
- ✅ `examples/memory_safety.vani` — 7 canonical safety patterns
  end-to-end (#261)
- ✅ Move-rejection diagnostic carries a type-aware fix hint —
  `ref x` for borrowing, `clone(x)` for deep copy, exclusive
  handles say "cannot be cloned" (#260)
- ✅ Parallel-for implicit-reduction race check — captured Copy
  mutation without `reduce` clause errors at compile time (#259)
- ✅ Namespaces / modules — `module foo { … }`, `pub` / `pub(kosh)`,
  `use foo::bar [as baz];` / `use foo::*;` / `use foo::{a, b};`,
  module-local `use`, `pub use` re-exports, nested modules with deep
  paths, orphan rules, collision diagnostics — closures #242–#258
- ✅ "Kosh" (कोश) adopted as vāṇī's word for the future crate concept;
  `pub(kosh)` accepted as preparatory syntax
- ✅ Keyword aliases: `assign` (let), `give` / `give_back` /
  `give back` (return)
- ✅ SSA Step 3b — multi-block parallel-for body in SSA-C (closure #251)
- ✅ Array types in fn return position (#239)
- ✅ Formatter support for module blocks + `use_paths` round-trip (#250)
- ✅ `clone_at` on `Vec<Struct>` tree-LLVM lowering
- ✅ Methods without `self` rejected with clean diagnostic
- ✅ Bare-block `{ … }` as statement — helpful diagnostic with workaround
- ✅ Compile-time short-circuit `&&` / `||` honors dead-code RHS
- ✅ Discarded `call();` / `receiver.method();` as a statement
- ✅ Sharper diagnostics for struct / tuple / enum `==` / `!=`
- ✅ `print` of struct / tuple / enum → targeted diagnostics (was: backend panic)
- ✅ Inner-`let` shadow leak in SSA `lower_if` fixed
- ✅ `ArrayLit` as direct fn argument (was: backend panic)
- ✅ Float negation in SSA-LLVM (was: invalid `sub double` IR)
- ✅ Empty `vec()` supported
- ✅ Vec-of-Vec / Vec-of-Struct end-to-end via `clone_at(ref xs, i)`
- ✅ `methods on T { fn m(self: T) … }` with field assignment + auto-ref
- ✅ Match: wildcards + integer + bool + string patterns + enum-to-int cast
- ✅ `if`-as-expression + `else if` chaining + Match phi fix
- ✅ Format polish: trailing commas everywhere, struct/methods round-trips
- ✅ Const decls + type aliases + const overflow check
- ✅ Discarded call-stmt sugar — `let _ = …` desugared at parse
- ✅ Composition coverage — 80+ probe + regression tests across the
  struct / enum / Vec / method / if-expr / match / const / type-alias /
  affine-shadow surfaces

**Todo (small):**

These either land naturally with a queued multi-session item, or are
deliberately deferred as v1 trade-offs.

- ✅ `const N` as array length `[T; N]` — works for previously-declared
  consts with an integer-literal initializer.
- ✅ Const initializer arithmetic — `const B: i64 = A + 1;` (and `* / - %`)
  folds at parse time across previously-declared integer consts.
- ✅ Array types in fn return position — `fn make() -> [i64; 3]`
  compiles + runs on both backends. Tree-C wraps via a per-shape
  struct (`intent_arr_ret_N_T`); tree-LLVM returns `[N x T]` by
  value natively. SSA-LLVM falls back to tree-LLVM for the
  stack-aliasing case. See [examples/array_return.vani](examples/array_return.vani).
- ✅ Nested arrays `[[T; N]; M]` and `[Vec<T>; N]` — closure #291
  Phases 1–4 (2026-05-27). Array-element-must-be-Copy restriction
  lifted; `clone_at(ref arr, i)` extended to arrays; per-slot
  per-field drops including struct-slot field walks; tree-LLVM
  `len` of a Vec rvalue (`len(clone_at(ref xs, i))`) spills to
  alloca, GEPs `.len`, loads.
- ✅ Empty struct `struct E {}` — useful for marker / zero-sized types.
- ✅ Unit-return functions — `fn f() { … }` without `-> Type` is sugar
  for `-> i64` with an implicit `return 0;` appended. Callers invoke as
  a bare statement (`f();`) or via `let _ = f();`. See
  [examples/unit_return.vani](examples/unit_return.vani).
- ✅ Type-associated functions `Type.helper(args)` — declare with
  `methods on T { fn helper(args) -> R { … } }` (no `self`); call as
  `T.helper(args)`. Constructors and other type-namespaced helpers.
  See [examples/type_associated_fn.vani](examples/type_associated_fn.vani).
- ⏳ `bool ↔ int` cast — different semantic domains, forces explicit
  `if cond { 1 } else { 0 }` and vice versa. Trade-off, may stay deferred.
- ✅ SSA bool-print parity — bool prints render as `true`/`false`
  through both SSA backends (closure #117 fixed the `1`/`0` gap).
- ✅ Bare `{ … }` as scope-stmt — provides an explicit nested scope
  for binding visibility. Desugars to `if true { … }` at parse time.
- ✅ `xs[i].field = v` mixed-place assign — including deep paths
  (`xs[i].a.b = v`); each intermediate segment must be a Copy struct
  and the leaf field must be Copy.
- ⏳ Generic function call sites — parses, gated diagnostic, lands with T1.4.
- ⏳ Enum payload variants — parses, gated diagnostic, lands with T1.3 phase 2b.
- ✅ Match on float scrutinee — closure #278 (2026-05-27).
  `Pattern::Float(f64)` AST variant + `check_match_float` desugars
  to a nested IfExpr chain; diagnostics for missing wildcard,
  duplicate literals, NaN-in-pattern, wrong scrutinee type.
(Tuple / struct / enum `==` all ship today — see the
"Generics & interfaces" section above.)

**Trade-offs (working as intended, not on the queue):**

No cross-compilation; Windows parallel-for thread count hardcoded N=4;
references second-class (param-only); natural-exit `!cond` post-loop fact
dropped when body can `break`; `prove foo(args)` requires `foo` to have
`ensures`; `INTENTC_NO_VERIFY=1` skips SMT (dev opt-out, never in CI).

### Multi-session items

Ordered by **dependency first, then effort** (lowest effort wins among
items with the same dependency level). Each fully closes a queued
roadmap surface and unblocks the items below it.

| # | Item | Depends on | Est. effort | Unlocks |
|---|---|---|---|---|
| 1 | ✅ **Block expressions** `let r = { stmts; tail-expr };` | — | low/medium | done 2026-05-21; see [examples/block_expressions.vani](examples/block_expressions.vani) |
| 2 | ✅ **SMT modeling — if-expr, match, struct field access, method calls** | — | medium | done 2026-05-21 (#82 + #84 — full coverage) |
| 3 | ✅ **T1.2 phase 2b: affine struct fields** | — | medium/high | done 2026-05-21 — `struct { … }` admits `OwnedStr`, `Vec<T>`, `[T;N]` of Copy elements, `Task`, `Atomic<T>` as fields; both backends free heap fields (OwnedStr, Vec) at scope exit; struct-literal init moves the source binding; `t.data[i]` indexing works. See [examples/struct_owned_field.vani](examples/struct_owned_field.vani), [examples/struct_mixed_fields.vani](examples/struct_mixed_fields.vani). Mutex/Guard/Channel still need explicit wiring. |
| 4 | ✅ **T1.3 phase 2b: tagged-union codegen + pattern bindings** | — | high | done 2026-05-21 — see [examples/option_types.vani](examples/option_types.vani); both backends |
| 5 | ✅ **T2.6: `try` keyword sugar for Option-like enums** | T1.3 phase 2b | low/medium | done 2026-05-21 — see [examples/try_keyword.vani](examples/try_keyword.vani). Generic Option<T> / Result<T, E> wait on #6 monomorphization. |
| 6 | ✅ **T1.4 phase 2: generic call-site monomorphization** | — | high | done 2026-05-21 — pass-through generics specialize per call-site literal type; see [examples/generic_functions.vani](examples/generic_functions.vani). Var-arg inference + interface bounds pending. |
| 7 | ✅ **T1.5 phase 2 + 3: interface dispatch (static + dynamic) + bounded generics** | T1.4 phase 2 | medium/high | done 2026-05-25 — static `recv.method()` dispatch + bounded generics done 2026-05-21; `dyn Iface` fat-pointer dispatch (owned, `ref dyn`, `Vec<dyn>`, struct fields of dyn) shipped via closures #220-#228, see [examples/dyn_dispatch.vani](examples/dyn_dispatch.vani). |
| 8 | ✅ **T2.7: user-defined Drop interface (auto-call at scope exit)** | T1.5 phase 2, #3 | low/medium | done 2026-05-25 — `implement Drop for T` runs automatically at scope exit. Two signatures supported: `fn drop(self: T)` (by-value, consumes self — only valid when T has no heap-owning fields) and `fn drop(self: mut ref T)` (runs first then per-field free — works for any T including OwnedStr / Vec / nested-struct fields, closure #229). See [examples/drop_interface.vani](examples/drop_interface.vani). |
| 9 | ✅ **Devanagari keyword aliases — Sanskrit / Hindi / Marathi (Phase 1 + 2)** | — | medium/high | Phase 1 done 2026-05-21 (single-word aliases + multi-word fusion `नहीं तो` / `के लिए` / `सिद्ध करो`). Phase 2 done 2026-05-26/27 (closures #265–#267): SOV word order for range `for` (`i के लिए 0 से 5 तक { … }`) and verb-at-end statements (`X पुनरागम;` / `… लिखो;` / `cond सुनिश्चित;` / `expr प्रमाण;`), plus 3-way alias parity for the previously English-only keywords. Grammar-consultant refinement pass still welcome. See [examples/hindi_keywords.vani](examples/hindi_keywords.vani), [examples/sanskrit_keywords.vani](examples/sanskrit_keywords.vani), [examples/marathi_keywords.vani](examples/marathi_keywords.vani). |
| 10 | ✅ **Namespaces — modules, visibility, use, kosh** | — | high | done 2026-05-26 across closures #242–#258. `module foo { … }` blocks (inline + nested + deep `a::b::c::Item` paths), per-item `pub` / `pub(kosh)` visibility, `use foo::bar [as baz];` / `use foo::{a, b};` / `use foo::*;` import forms (top-level AND inside module bodies), `pub use foo::bar;` re-exports (transitively resolved), orphan rules for `implement Iface for T`, collision diagnostics, formatter round-trip. See [examples/modules.vani](examples/modules.vani) and the *Modules and namespaces* section above. The full kosh package-manager arc (manifest, resolver, registry, stdlib-as-kosh) is still on the deferred queue — see [TODO.md](TODO.md) item #10. |
| 11 | ✅ **SSA-LLVM multi-block parallel-for body — atomicrmw emit** | #10 (SSA Step 3b recognizer) | medium/high | done 2026-05-26 (closure #264). The recognizer (#241) accepts multi-block bodies; SSA-C emit landed (#251); SSA-LLVM Phi-traceback now locates the actual reduction-update across conditional branches and replaces it with atomicrmw at its production site. Multi-block bodies (e.g. `parallel for { if cond { acc = acc + i; } }`) no longer fall back to tree-LLVM — they lower directly to atomicrmw in the outlined fn. |
| 12 | ✅ **FFI v1–v8 (`extern "C" fn` end-to-end)** | — | high | done 2026-05-27 across closures #269–#274, #279, #285, #288. `extern "C" fn` declarations, `--link-with PATH` / `-l<name>` flags, extern call-site checker, mangled-symbol codegen, struct-by-value rejection with `ref T` hint, callbacks via `Type::FnPtr`, System V x86-64 small-struct return lowering. Net: `qsort`-style callbacks and libc string / math interop work end-to-end without a runtime shim. |
| 13 | ✅ **vani.toml manifest (v1 + v2 [deps])** | — | medium | done 2026-05-27 (#280 + #287). Hand-rolled minimal-TOML parser, `find_manifest` parent-walk, `[package].entry` auto-discovery, `[deps]` inline-table for multi-file dependency wiring. |
| 14 | ✅ **Generic struct + enum declarations** | #6 | high | done 2026-05-27 (#281 + #282). `Type::Apply { name, args }` for parse-time generic instantiations; mangled names like `Result__Vec_I64___AllocError`; `Option<T>` / `Result<T, E>` / `AllocError` injected at AST level as prelude. |
| 15 | ✅ **Mixed-payload enums + `try_vec(n) -> Result<Vec<i64>, AllocError>`** | #14 | medium/high | done 2026-05-27 (#283 + #284). C uses tagged union `union { Type0 v_Ok; Type1 v_Err; }`; LLVM uses `[N x i8]` byte buffer + per-variant bitcast. `try_vec` builtin emits malloc + null-check + Result construction. |
| 16 | ✅ **Attribute syntax + `#[bounded(N)]`** | — | medium | done 2026-05-27 (#286, #289, #290). First attribute in the language. New `#` token + parser; tree-LLVM uses thread-local globals + per-Return decrement; SSA-LLVM mirrors the pattern; C emits a thread-local counter with GCC `__attribute__((cleanup))` for the decrement. |
| 17 | ✅ **Nested arrays `[[T; N]; M]` / `[Vec<T>; N]`** | — | medium | done 2026-05-27 (#291 Phases 1–4). Array-element Copy restriction lifted; `clone_at(ref arr, i)` extended to arrays; per-slot per-field drops including struct-slot field walks; tree-LLVM `len` of a Vec rvalue spills to alloca, GEPs `.len`, loads. |
| 18 | ⏳ **Data structures + algorithms roadmap (Levels 1–4)** | #14 (for Level 2+) | high (multi-session) | Levels 1–4 sequenced under affine ownership. Level 1: `sort` / `sort_by` / `find` / `binary_search` / `pop` / RNG / Hash interface. Level 2: `HashSet` / `HashMap` (⚠️ AFFINE-TENSION — `get -> Option<ref V>`) / `BTreeSet` / `BTreeMap` / `Deque` / `BinaryHeap`. Level 3: closures + iterator combinators. Level 4: arena-based BST / B-tree / Trie / graphs + algorithms. Full per-item plan in [TODO.md](TODO.md). |
| 19 | ✅ **Condition variables (`Condvar`)** | — | medium (single session) | done 2026-05-28 (closure #292). ✅ AFFINE — new builtin type, stack-by-value. 5 builtins (`condvar_new / wait(ref cv, mut ref g: Guard<i64>) / wait_timeout / notify_one / notify_all`). Tree-C + SSA-C: shared runtime helpers (futex/WaitOnAddress/spin-yield). Tree-LLVM: inline IR per call site (`%intent_condvar = type { i32 }`, atomicrmw + syscall/WakeByAddress). SSA-LLVM: falls back to tree-LLVM. 5 lib tests + `examples/condvar.vani` cross-backend parity. Pending follow-ups: cross-task wait/notify (needs task-capture rule expansion), direct SSA-LLVM support, wider Mutex widths. |
| 20 | ⏳ **Async / asyncio** (⚠️ AFFINE-TENSION via compiler-lowered state machines; 🛑 NOT Pin / self-references) | Level 3 closures (#18) | high (multi-session) | Each `async fn` lowers to an enum-of-frames in `Vec<StateMachine>`. Single-threaded event-loop driver `intent_async_run`; non-blocking I/O (file / socket / timer) under epoll / kqueue / IOCP; `Channel<T, N>` is the cooperative coordination primitive; `Future<T>` = generic enum w/ `Ready(T)` / `Pending` (uses #281 + #283). NOT shipping: Rust-style `Pin<&mut Self>`, panic-based cancellation, stackful coroutines, async inside `parallel for`. Full design in [TODO.md](TODO.md) under *Async / asyncio*. |
| 21 | ⏳ **Kosh package manager + Vāṇī-Kosh registry** | #10, #13 | high (multi-session) | `kosh.toml` manifest, resolver + lockfile, `pub(kosh)` enforcement at the boundary, registry CLI (`intentc kosh add`, `kosh publish`), stdlib-as-kosh. Item #10 in [TODO.md](TODO.md). |

**Devanagari aliases (#9) — current state + remaining work:**

**Phase 1** (closures #235–#237; 2026-05-21). The lexer recognizes
single-word Devanagari aliases (Sanskrit / Hindi / Marathi) for
`fn` / `let` / `return` / `if` / `else` / `while` / `for` / `prove`
and friends, plus multi-word phrases via a post-lex merger
(`नहीं तो` → else, `के लिए` → for, `सिद्ध करो` → prove). Per-language
**purity v1** lets users opt a file into a single language (Hindi /
Sanskrit / Marathi / English) via a header marker; the checker then
rejects out-of-language identifiers.

**Phase 2** (closures #265–#267; 2026-05-26/27) — closes the two
biggest ergonomic gaps:
- **SOV word-order parsing** (#265 + #266). Range `for` now
  accepts the natural Indo-Aryan shape `i के लिए 0 से 5 तक { … }`
  (variable + `के लिए`; operands + `से` / `तक` postpositions),
  and the four verb-like statements accept the verb-at-end form
  (`X पुनरागम;` = return; `… लिखो;` = print; `cond सुनिश्चित;`
  = assert; `expr प्रमाण;` = prove). The detector keys off
  Ident-followed-by-verb or scan-to-`;`-ending-in-verb so the
  English keyword-first forms still parse.
- **3-way alias parity** (#267). Every previously English-only
  keyword now has a Sanskrit / Hindi / Marathi alias —
  `वरना` (else, Hindi), `परिवर्तनीय` (mut, Sanskrit/Hindi),
  `अग्रे` (continue, Sanskrit), `सार्वजनिक` (pub, all three),
  `खण्ड` (module, all three), `उपयोग` (use, all three),
  `यथा` (as, all three), `यत्र`/`जहाँ`/`जिथे` (where, per
  language), `अस्ति`/`है`/`आहे` (is, per language), plus
  interface / implement / methods / try / task / join /
  parallel single-word. Sanskrit-root tatsama forms (e.g.
  `संरचना` = struct) are documented as shared rather than
  duplicated. A pure-Hindi or pure-Sanskrit or pure-Marathi
  program now reads top-to-bottom with no English fall-back.

**Still queued:**
- **Grammar-consultant refinement.** Phase-2 verb picks are
  best-effort; idiomatic dialect-specific revision is welcome.
- **Script-aware diagnostics (9d).** Errors today emit in
  English; a per-source-script diagnostic mode is queued.

**Long-term beyond v1**

- Cranelift backend (fast native JIT, no LLVM dependency).
- Direct-asm targets (x86_64-linux first, then small-targets).
- Work-stealing scheduler for `task` fan-out.
- SIMD-targeted lowering.
- GPU / accelerator backends.
- Richer aliasing rules — region / lifetime inference beyond the
  second-class `ref` / `mut ref` discipline.
- AI collaboration: keep human-readable source as the authority, let AI
  produce candidate algorithms, constraints, proofs, tests, and
  target-specific optimizations. The compiler verifies the candidates
  before accepting them.

## Contributing

VANI is an open-source research compiler. Patches, bug reports, and
example programs are all welcome.

- [CONTRIBUTING.md](CONTRIBUTING.md) — pre-PR checklist, code
  conventions, commit-message style, and how to file issues.
- [ONBOARDING.md](ONBOARDING.md) — toolchain prerequisites, project
  layout, and an end-to-end "add a feature" walkthrough.
- [STATUS.md](STATUS.md) — single-page snapshot of the current feature
  set, the priority-ordered TODO queue, and known issues.

## License

Released under the [MIT License](LICENSE). VANI / वाणी is a free
non-commercial project; common phrases and the project name carry no
registered trademark — see *Trademark* below.

### Trademark

The project name **VANI** (वाणी, *vāṇī*) and the tagline *"code like you
speak"* are unregistered common-law marks of The VANI Authors. You may
use them to refer to the project ("compatible with VANI", "implementation
of VANI") and in good-faith forks. Please don't use them in a way that
implies endorsement by the project, or as your own product brand. If in
doubt, ask in an issue.
