# vāṇī (वाणी) — VANI

**Verbose Alternative Natural Interface — code like you speak.**

Pronounced **vaa-NEE** (Sanskrit *vāṇī* — long-a, retroflex-n, long-i;
stress on the second syllable). वाणी is the Sanskrit word for *speech*,
*voice*, or *language itself*. The name captures the design goal: a
programming language whose source reads like prose, not like a thicket
of punctuation. Where most languages reach
for `&`, `&mut`, `::`, `??`, `&*`, or `<>` to denote behavior, VANI uses
the keywords you would already say out loud — `ref`, `mut ref`, `then`,
`ensures`, `requires`, `parallel for`, `methods on`. The compiler does the
heavy lifting; the source stays human-shaped.

This is a small Rust-based compiler prototype that demonstrates the
approach end-to-end:

- intent metadata
- typed functions, structs, enums, methods, tuples, type aliases, consts
- typed IR lowered to SSA form
- verifiable `requires`, `ensures`, `assert`, `prove`, and `invariant`
  constraints, discharged by a Z3-backed SMT layer
- compile-time bounds / divisor / shift / overflow checking
- affine ownership with automatic destructors (Vec, OwnedStr, Atomic,
  Mutex, Guard, Channel, Task)
- LLVM IR as the default backend (with AOT via `llc + cc`), C as the
  legacy / portability backend, both reached through a shared SSA pipeline
- `parallel for` with verified race-freedom + reductions, and
  `task` / `join` with real-thread spawning on Linux and Windows

The production compiler core is in Rust. Python is excellent for
experiments, testing, and AI orchestration, but Rust is the better default
for a compiler that must be fast, memory-safe, deterministic, and close to
ABI / native code generation.

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
return a + b."* — no `::`, no `<>`, no `&`. Every operator is a word that
makes sense when spoken.

### Why keyword-first

Most language friction comes from punctuation: `&mut self`, `Option<Box<T>>`,
`if let Some(x) = …`, `xs.iter().filter(…).map(…)`. VANI deliberately
trades brevity for readability:

| Other languages | VANI |
|---|---|
| `&xs` (borrow) | `ref xs` |
| `&mut xs` (mut borrow) | `mut ref xs` |
| `fn(&self)` (method) | `fn name(self: ref Type)` |
| `Vec::with_capacity(n)` (path call) | (free function — no `::`) |
| `impl Drop for T` (interface impl, planned) | `methods on T` |
| `match Some(x) => …` (planned) | `match Some(x) then …` |
| `xs?` (try operator, planned) | `try foo(…)` (keyword form, planned) |
| `loop { … }` | `while true { … }` |
| `for x in &xs` | `for x in ref xs` |

The compiler never silently changes the meaning of source. Aliasing,
ownership transfer, and pure-vs-effectful boundaries are all visible in
the words on screen.

### वाणी (*vāṇī*) — Devanagari notation (planned)

Devanagari notation lets the source read in the writer's mother tongue.
The first three languages are **Sanskrit** (*saṁskṛta* — the canonical
Devanagari language and grammar root), **Hindi** (*hindī*), and **Marathi**
(*marāṭhī*). They share the script but use slightly different verbs for
the common keywords. The idea is **alias-based**: every English keyword
gets one or more Devanagari aliases, and the lexer accepts whichever form
the source file uses. A single program may mix forms freely; the compiler
treats them as the same token. This is gated behind a future closure —
see the Roadmap below.

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

Supported today (786 lib + 47 e2e tests passing):

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
  [examples/push_mut.intent](examples/push_mut.intent).
- Tuples `(T1, T2, ...)` (n in 2..=4) with `.0` / `.1` access; destructure
  `let (a, b) = expr;`.
- Structs `struct Point { x: i64, y: i64 }` with up to 64 fields; field access
  `p.x` and field assign `p.x = v;`.
- Enums: `enum Color { Red, Green, Blue }`. Payloaded variants `enum Opt
  { Some(i64), None }` work in both backends — tagged-union codegen lays
  them out as `{ i32 tag, T payload }`. Match destructure
  `Opt.Some(v) then …` binds the payload into the arm scope. V1 limits
  payloads to single Copy fields per variant + uniform payload type
  across variants. See [examples/option_types.intent](examples/option_types.intent).
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
  intermediate lets, return) — see [examples/try_keyword.intent](examples/try_keyword.intent).
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
  [examples/generic_functions.intent](examples/generic_functions.intent).
  V1 limits: single type parameter, body must be type-correct without
  knowing T (pass-through patterns).
- **Interfaces** `interface Show { fn show(self: T) -> R; }` + `implement
  Show for Point { fn show(self: Point) -> R { … } }` — static dispatch
  via `recv.show()`. The impl hoists to `T_show`; the existing method-
  dispatch path resolves the call at compile-time based on the receiver's
  type. V1 limits: static dispatch only (no vtables); each impl must cover
  every interface method; signatures must match exactly. See
  [examples/interfaces.intent](examples/interfaces.intent).
- **Drop interface** `implement Drop for T { fn drop(self: T) -> i64 { … } }`
  — auto-called at every scope exit where a non-moved binding of T goes
  out of scope. Users can also call `t.drop()` manually; affine tracking
  marks the binding as moved so the auto-call won't double-fire. When T
  has heap-shaped fields (OwnedStr / Vec), the per-field free pass runs
  instead (the user's drop is then invoked explicitly when richer
  behavior is needed). See
  [examples/drop_interface.intent](examples/drop_interface.intent).
- **Mixed-place assignment** — `xs[i].field = v;` writes through an index
  plus a single-level struct field in one statement. Works on owned
  `Vec<T>` and `[T; N]`. See
  [examples/mixed_place_assign.intent](examples/mixed_place_assign.intent).
- **Partial-move tracking** — `let taken = bag.contents;` moves a single
  field out of a struct. The aggregate is still readable for its other
  fields; scope-exit Drop skips the moved field (no double-free); a
  second read of the moved field surfaces a use-after-move diagnostic.
  See [examples/partial_move.intent](examples/partial_move.intent).
- **User-defined `==` via `implement Eq for T`** — `a == b` and `a != b`
  on struct or enum bindings desugar to the hoisted `<T>_eq(a, b)` /
  `!<T>_eq(a, b)` whenever both sides are the same nominal type.
  Convention is `fn eq(self: T, other: T) -> bool`. See
  [examples/struct_eq.intent](examples/struct_eq.intent) and
  [examples/enum_eq.intent](examples/enum_eq.intent).
- **Tuple auto-equality** — tuples are anonymous, so `==` is
  compiler-derived: `(a, b) == (c, d)` rewrites to `a == c && b == d`.
  Each per-element comparison uses the element type's `==` rule
  (built-in for primitives, `<T>_eq` for nominal element types). See
  [examples/tuple_eq.intent](examples/tuple_eq.intent).
- **Field-borrow expressions** — `ref t.f` and `mut ref t.f` take a borrow
  of a struct field. The result type is `&<field_ty>` / `&mut <field_ty>`;
  backends GEP into the struct's storage. Unlocks atomic operations
  through a struct that owns the cell (`atomic_*(ref c.hits)` /
  `atomic_*(mut ref c.hits)`). Single-level only in v1
  (no `ref t.a.b`). See
  [examples/struct_atomic_field.intent](examples/struct_atomic_field.intent).
- **Structs with affine fields** — `OwnedStr`, `Vec<T>`, `[T; N]` of Copy
  elements, `Task`, and `Atomic<T>` are valid struct field types in v1.
  Heap-shaped fields (OwnedStr, Vec) are freed at scope exit; stack-shaped
  fields (arrays, Task, Atomic) need no runtime drop. Struct-literal init
  from a `Var` moves the source binding so a heap value flows `caller →
  struct field → drop` without a double-free. Field-path indexing
  (`t.data[i]`) works through both backends. Mutex / Guard / Channel still
  need explicit wiring. See
  [examples/struct_owned_field.intent](examples/struct_owned_field.intent),
  [examples/struct_mixed_fields.intent](examples/struct_mixed_fields.intent).

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
- Multi-file projects via `use "path.intent";` (transitive, cycle-detected).

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
- `Vec<T>` accepts non-`Copy` elements: `Vec<Vec<T>>`, `Vec<[T; N]>`, and
  `Vec<Struct>` all work. Reading a non-Copy slot into a binding requires
  `clone_at(ref xs, i)` — bare `let inner = xs[i]` would alias the owner's
  slot and double-free, so the checker rejects it with a hint pointing at
  `clone_at`.

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
`examples/array_proofs.intent` for the end-to-end pattern.

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

A file can pull in others with `use "path.intent";`:

```intent
// math.intent
fn double(x: i64) -> i64 { return x * 2; }
```

```intent
// main.intent
use "math.intent";

fn main() -> i64 {
  let v: i64 = double(21);
  assert v == 42;
  return 0;
}
```

`intentc check`/`emit-c`/`run` accept the entry file and recursively resolve
`use` declarations relative to each file's directory. Names from imported
files share a flat namespace — there are no module-qualified call syntaxes
yet.

Cycles are detected by canonicalized path: each file is included at most
once across the dependency tree, so `a.intent` `use`-ing `b.intent` and
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

### JSON diagnostics

`intentc check file.intent --json` produces a JSON object on stdout
suitable for editor integrations and CI:

```json
{
  "diagnostics": [
    {
      "level": "error",
      "message": "value 'xs' was moved; cannot use after move",
      "primary": { "file": "f.intent", "line": 5, "col": 18, "end_line": 5, "end_col": 20 },
      "related": [
        { "message": "'xs' was moved here",
          "span": { "file": "f.intent", "line": 4, "col": 21, "end_line": 4, "end_col": 23 } }
      ]
    }
  ]
}
```

The output ends with a single newline. On success, the body is
`{"diagnostics":[]}`. Without `--json`, the human-readable form goes to
stderr as before.

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

See `examples/parallel.intent` for a runnable end-to-end
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
them before the spawn site. See `examples/tasks.intent` for
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

See `examples/atomics.intent` for a runnable demonstration.

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

See `examples/concurrency.intent` for a runnable demonstration.

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

See `examples/fn_pointers.intent` for a runnable demonstration.

## Commands

The compiler has two backends: **LLVM IR (default)** and C (legacy,
on the deprecation path). `--backend=c` opts back into the C output
for `emit` / `run`; the `emit-c` subcommand is a stable alias for
C-only emission. `run` invokes `lli` for LLVM IR and `cc` for C
output. `build` produces a native binary via `llc` + `cc` (linker
only — no C source is compiled).

### Build & run pipeline

```bash
cargo run -- check examples/basics.intent                 # Type-check + verify
cargo run -- check examples/basics.intent --json          # JSON diagnostics
cargo run -- check examples/basics.intent --no-verify     # Skip SMT (dev opt-out)

cargo run -- emit examples/basics.intent                  # LLVM IR (default)
cargo run -- emit examples/basics.intent --backend=c      # C output
cargo run -- emit examples/basics.intent -o /tmp/basics.ll
cargo run -- emit-c examples/basics.intent                # Legacy alias for --backend=c

cargo run -- run examples/basics.intent                   # LLVM via lli (default)
cargo run -- run examples/basics.intent --backend=c       # C via cc

cargo run -- build examples/basics.intent -o /tmp/basics  # AOT native binary
                                                          # (LLVM → llc → cc linker)
```

### Debug subcommands

Useful for hacking on the lexer / parser / checker. Each runs the
pipeline up to a stage and dumps a debug-format representation.

```bash
cargo run -- tokens examples/basics.intent   # Token stream from the lexer
cargo run -- ast    examples/basics.intent   # Parsed AST (skips type checker)
cargo run -- ir     examples/basics.intent   # Typed IR (what the backends see)
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

Point your editor at `intent-lsp` for `*.intent` files. For
Neovim with `nvim-lspconfig`:

```lua
local lspconfig = require('lspconfig')
local configs = require('lspconfig.configs')
if not configs.intent then
  configs.intent = {
    default_config = {
      cmd = { 'intent-lsp' },
      filetypes = { 'intent' },
      root_dir = lspconfig.util.find_git_ancestor,
      settings = {},
    },
  }
end
lspconfig.intent.setup({})
```

The cross-backend parity test runs every file under `examples/`
through both `--backend=c` and `--backend=llvm` and diffs stdout
+ exit codes. New examples are picked up automatically when wired
into `check_examples_all_succeed` and a `run_<example>_example`
test (see `tests/run_end_to_end.rs`).

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

- ⏳ `const N` as array length `[T; N]` — parser requires integer literal
  (lands cheaply with block expressions if we want to allow const-eval).
- ⏳ Const initializer with arithmetic (`const B: i64 = A + 1;`) — literal-only
  in v1; needs a const-eval pass (would land with block expressions).
- ⏳ Array types in fn return position — SSA layer lacks by-value-array return;
  clean diagnostic in place.
- ⏳ Nested arrays `[[T; N]; M]` and `[Vec<T>; N]` — SSA path doesn't lower
  by-value element loads of these shapes yet.
- ⏳ Empty struct `struct E {}` — parser requires ≥1 field.
- ⏳ Unit-return functions (`fn f() { … }` without `-> Type`) — every fn
  needs a return type in v1.
- ⏳ Type-associated functions `Type.helper()` — currently no syntax;
  `methods on T { fn no_self() … }` is rejected. Free functions are the
  recommended workaround.
- ⏳ `bool ↔ int` cast — different semantic domains, forces explicit
  `if cond { 1 } else { 0 }` and vice versa. Trade-off, may stay deferred.
- ⏳ SSA bool-print gap — bool literals via SSA path render as `1`/`0`
  instead of `true`/`false`. Tree-LLVM path correct.
- ⏳ Bare `{ … }` as scope-stmt — currently rejected with workaround
  diagnostic; lands cheaply once block expressions exist.
- ✅ `xs[i].field = v` mixed-place assign — single-level paths land
  end-to-end; deeper paths (`xs[i].a.b = v`) still need a workaround.
- ⏳ Generic function call sites — parses, gated diagnostic, lands with T1.4.
- ⏳ Enum payload variants — parses, gated diagnostic, lands with T1.3 phase 2b.
- ⏳ Match on float scrutinee — `bool` and `Str` ship today (see
  [examples/match_bool.intent](examples/match_bool.intent) and
  [examples/match_str.intent](examples/match_str.intent)); float
  comparison is the usual gnarly case (NaN, epsilon thresholds).
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
| 1 | ✅ **Block expressions** `let r = { stmts; tail-expr };` | — | low/medium | done 2026-05-21; see [examples/block_expressions.intent](examples/block_expressions.intent) |
| 2 | ✅ **SMT modeling — if-expr, match, struct field access, method calls** | — | medium | done 2026-05-21 (#82 + #84 — full coverage) |
| 3 | ✅ **T1.2 phase 2b: affine struct fields** | — | medium/high | done 2026-05-21 — `struct { … }` admits `OwnedStr`, `Vec<T>`, `[T;N]` of Copy elements, `Task`, `Atomic<T>` as fields; both backends free heap fields (OwnedStr, Vec) at scope exit; struct-literal init moves the source binding; `t.data[i]` indexing works. See [examples/struct_owned_field.intent](examples/struct_owned_field.intent), [examples/struct_mixed_fields.intent](examples/struct_mixed_fields.intent). Mutex/Guard/Channel still need explicit wiring. |
| 4 | ✅ **T1.3 phase 2b: tagged-union codegen + pattern bindings** | — | high | done 2026-05-21 — see [examples/option_types.intent](examples/option_types.intent); both backends |
| 5 | ✅ **T2.6: `try` keyword sugar for Option-like enums** | T1.3 phase 2b | low/medium | done 2026-05-21 — see [examples/try_keyword.intent](examples/try_keyword.intent). Generic Option<T> / Result<T, E> wait on #6 monomorphization. |
| 6 | ✅ **T1.4 phase 2: generic call-site monomorphization** | — | high | done 2026-05-21 — pass-through generics specialize per call-site literal type; see [examples/generic_functions.intent](examples/generic_functions.intent). Var-arg inference + interface bounds pending. |
| 7 | ✅ **T1.5 phase 2: interface dispatch (static) + bounded generics** | T1.4 phase 2 | medium/high | done 2026-05-21 — `interface` + `implement` + `recv.method()` dispatch; `fn min<T>(...) where T is Cmp` monomorphizes with bound-existence check; see [examples/interfaces.intent](examples/interfaces.intent), [examples/bounded_generics.intent](examples/bounded_generics.intent). Dynamic dispatch (vtables) still pending. |
| 8 | ✅ **T2.7: user-defined Drop interface (auto-call at scope exit)** | T1.5 phase 2, #3 | low/medium | done 2026-05-21 — `implement Drop for T` runs automatically at scope exit; `t.drop()` still works manually; auto-call suppressed for T with heap fields (those route through per-field free). See [examples/drop_interface.intent](examples/drop_interface.intent). |
| 9 | ✅ **Devanagari keyword aliases — Sanskrit / Hindi / Marathi (MVP)** | — | medium | done 2026-05-21; see [examples/hindi_keywords.intent](examples/hindi_keywords.intent), [examples/sanskrit_keywords.intent](examples/sanskrit_keywords.intent), [examples/marathi_keywords.intent](examples/marathi_keywords.intent). Multi-word aliases + script-aware diagnostics deferred. |

**Devanagari aliases (#9) — granular sketch:**

The work is mostly in the lexer (extend the keyword table) and the
diagnostic layer (so errors surface the closest matching alias when the
user mistypes a keyword in any language). No IR / backend change is
needed — the AST already speaks in English-keyword tags, and the parser
emits the same AST whichever surface alias was used. Stages:

- **9a.** Finalize the keyword alias tables for संस्कृत / हिन्दी / मराठी
  with grammar consultants. The README sketch above is the starting
  point — every English keyword needs at least one alias per language,
  picked to be idiomatic and unambiguous.
- **9b.** Extend the lexer's keyword recognition to read UTF-8 source
  with combining marks and to recognize multi-codepoint keyword tokens
  (Devanagari letters take several bytes each; the lexer's current
  single-byte loop needs to switch to a grapheme-cluster walk).
- **9c.** Add an alias table keyed by `(script_family, keyword)` and
  route every alias to the existing English-keyword `TokenKind`. Mixed-
  script files just route each token independently.
- **9d.** Update diagnostics so the error message surfaces in the script
  the source file uses most heavily (auto-detected from the keyword
  histogram), with the English alias quoted alongside for cross-reference.
- **9e.** Add per-language `examples/` files showing a working program
  written entirely in each script.
- **9f.** Document the alias tables in the README in the order they're
  finalized; deprecate the conceptual sketch above and replace it with
  the canonical list.

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
