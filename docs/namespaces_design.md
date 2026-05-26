# Namespaces / modules — design doc

**Status:** shipped — closures #242–#258 (see STATUS.md / TODO.md).
**Authored:** 2026-05-26; revised 2026-05-26 to mark the implemented surface.

## What shipped (closures #242–#258)

The original proposal below was implemented in full, plus several
follow-ups that were initially queued. Snapshot of the surface as
of closure #258:

| Form | Closures | Notes |
|------|----------|-------|
| `module foo { … }` | #242 | inline blocks; private-by-default |
| `pub fn` / `pub struct` / etc. | #243 | visibility per item via differentiated `__priv__` mangling |
| `pub struct/enum/const/type` | #244 | full visibility coverage across all item kinds |
| `use foo::bar;` | #245 | single-item import |
| `implement Iface for T` orphan rules | #246 | strict — must live in iface OR for-type's module |
| `use foo::{a, b};` | #247 | multi-item brace list |
| Nested `module a { module b { … } }` + deep paths `a::b::c::Item` | #248 | worklist-based flatten; arbitrary nesting |
| Implicit sibling-module references (`inner::f` from inside `outer`) | #249 | qualify checks nested-module-name set |
| Formatter support + `examples/modules.vani` | #250 | round-trips |
| Glob `use foo::*;` | #253 | non-transitive (direct children only) |
| `use foo::bar as baz;` rename + collision diagnostic | #254 | both single-item and per-entry in brace lists |
| `use` inside `module { }` bodies | #256 | scoped to module body, no leak |
| Re-exports `pub use foo::bar;` | #257 | transitively resolved via fixed-point |
| `pub(kosh)` visibility tier | #258 | preparatory; behaves as `pub` today, enforces at the future kosh boundary |

## Goal

Partition vāṇī's currently-flat global name set so large programs (and
a future standard library) can manage their own surface without name
collisions or accidental exposure.

## Design principles (user-stated)

1. **Easy for newcomers.** Surface should be discoverable from
   first principles — no surprise lookup rules.
2. **Compile-time catching.** Visibility violations + missing-name
   errors should fire at type-check time, not surface as cryptic
   runtime bugs.
3. **Natural flow with the language.** vāṇī already reads like
   Rust (interface/implement/struct, affine ownership, `mut ref`).

## Recommendation: Rust-style modules

Reasoning:
- Rust modules use **explicit paths** (`foo::bar`). C++ namespaces use
  ADL (argument-dependent lookup) which is a frequent source of
  surprise for new users.
- Rust's `pub` / private-by-default is **enforced at compile time**.
  C++ namespaces have no visibility concept — `private:` works only
  inside classes.
- Rust's `use foo::bar;` is per-item explicit. C++'s `using namespace
  foo;` pollutes the global namespace.
- vāṇī's existing keywords (`interface`, `implement`, `methods on T`)
  already feel Rust-like; modules slot in naturally.

## Syntax (proposed v1)

### Inline module blocks

```vani
module math {
  pub fn square(x: i64) -> i64 {
    return x * x;
  }

  pub struct Point {
    x: i64,
    y: i64,
  }

  // Private — only callable inside math.
  fn double(n: i64) -> i64 { return n * 2; }
}

fn main() -> i64 {
  let p: math::Point = math::Point { x: 3, y: 4 };
  return math::square(p.x);  // = 9
}
```

### `use` to bring items into scope

```vani
module math {
  pub fn square(x: i64) -> i64 { return x * x; }
}

use math::square;

fn main() -> i64 {
  return square(5);  // = 25, no `math::` prefix needed
}
```

### Visibility

- Default: items are **private to the enclosing module**.
- `pub` exports an item to be reachable from outside the module.
- Top-level items (not inside any `module`) stay globally visible
  — preserves back-compat for every existing example.

### Path separator: `::` (not `.`)

vāṇī's `.` is already overloaded:
- `obj.field` — struct field access
- `obj.method()` — method call
- `EnumName.Variant` — enum constructor
- `Type.helper()` — type-associated function

Adding `module.fn()` would create context-sensitive ambiguity.
Using `::` (Rust convention; familiar to C++ users too) avoids
this. The `::` token is new to the lexer but doesn't collide
with any existing operator.

### What v1 leaves out (now mostly shipped — see top of doc)

The original "queued for follow-ups" list is preserved here as
historical context; each item links to the closure that shipped it.

- **Nested modules** (`module a { module b { ... } }`). ✅ #248.
- **Glob imports** (`use foo::*;`). ✅ #253 — non-transitive only.
- **Multi-item imports** (`use foo::{bar, baz};`). ✅ #247.
- **`pub(kosh)` / `pub(super)`** visibility tiers — `kosh`
  (कोश, "treasure/repository") is vāṇī's name for what Rust
  calls a crate. One kosh = one compilation unit / one package;
  the future package registry is Vāṇī-Kosh. ✅ #258 ships
  `pub(kosh)` as preparatory syntax (today behaves identically to
  `pub`; enforcement activates when the kosh boundary ships).
  `pub(super)` remains unsupported and surfaces a clear
  diagnostic.
- **Re-exports** (`pub use foo::bar;`). ✅ #257 — transitively
  resolved via fixed-point so chained re-exports collapse.
- **Module-level `const`** is fine but follows the same
  visibility rules as fns. ✅ #244.
- **`implement Iface for T` orphan rules** — Rust requires impls to
  live in the module of either the trait or the type. ✅ #246
  enforces this strictly. The IFACE_IMPL_REGISTRY already keyed
  by `(iface_name, type_name)`; the module-of qualifier sits
  alongside.

### Still queued (post-#258)

- **Kosh package manager** (`kosh.toml`, resolver, lockfile,
  registry CLI, stdlib-as-kosh). See TODO.md item #10 for the
  full arc. The smallest beachhead — `pub(kosh)` syntax #258 —
  has shipped.
- **Devanagari surface for the namespace keywords** (`module` /
  `pub` / `use`). Blocked on grammar review for the per-language
  3-way alias parity.

## Items allowed inside a module

Same set as top level today: `fn`, `struct`, `enum`, `interface`,
`implement`, `methods on T`, `const`, `type` aliases. The body's
type-check rules are unchanged — modules are purely about name
scoping.

## What this does NOT change

- Affine ownership + Drop semantics — unchanged.
- Verification / SMT discharge — unchanged. The verifier sees the
  same typed IR (with mangled names).
- Backend codegen — modules collapse to mangled names at the
  TypedProgram level. The backends never see the `module` keyword.

## Implementation outline (multi-session)

1. **Lexer.** Add `module` keyword (English) + `pub` keyword (English)
   + `::` operator token. Devanagari aliases for `module` / `pub`
   need grammar review; defer until per-language purity has the
   3-way distinction.
2. **AST.** New `Module` decl wrapping a list of items. Items
   carry a `visibility: bool` (private/pub).
3. **Parser.** Top-level `module IDENT { items... }`. `pub` modifier
   prefix on items. `use IDENT::IDENT::...::IDENT;` declarations.
   Path expressions `IDENT::IDENT(...)` in expression position.
4. **Checker.** Flatten modules into the existing flat name space
   by mangling: `module foo { fn bar() }` becomes the internal name
   `foo::bar`. References resolve via the mangled name. Visibility
   checked at the resolution site: accessing `foo::bar` from
   outside `foo` is allowed iff `bar` is `pub`. `use foo::bar;`
   creates a local alias.
5. **Tests.** Compile a tiny multi-module program; verify
   visibility violations surface as clear diagnostics.

## Open questions — resolved

The four design questions raised before implementation, with the
answer that shipped:

1. **Keyword name: `module` or `mod`?** → **Both.** Canonical
   form is `module`; `mod` is accepted as an alias in the lexer
   (closure #242).

2. **`pub` keyword: `pub` or `public`?** → **Both.** Canonical
   form is `pub`; `public` is accepted as an alias (closure #242).

3. **File-as-module — v2?** → **Deferred.** Current
   `use "path.vani";` continues to do file-level concatenation;
   file-as-module is a future enhancement once the kosh
   package-manager arc has shape.

4. **Orphan-rule strictness?** → **Strict** (closure #246).
   `implement Iface for T` must live in the module of either Iface
   or T — or be top-level. Out-of-place impls surface a clear
   diagnostic at the impl site.
