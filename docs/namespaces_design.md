# Namespaces / modules — design doc

**Status:** proposal, awaiting user review before implementation
**Authored:** 2026-05-26

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

### What v1 leaves out (queued for follow-ups)

- **Nested modules** (`module a { module b { ... } }`).
- **Glob imports** (`use foo::*;`) — surprise factor; might never
  ship.
- **Multi-item imports** (`use foo::{bar, baz};`) — sugar over
  one-line-each `use`.
- **`pub(kosh)` / `pub(super)`** visibility tiers — `kosh`
  (कोश, "treasure/repository") is vāṇī's name for what Rust
  calls a crate (renamed 2026-05-26). One kosh = one
  compilation unit / one package; the future package
  registry is Vāṇī-Kosh. Rust uses `pub(crate)` for
  fine-grained control; v1 vāṇī keeps it binary
  (`pub` / private). The tier work is queued for after
  the kosh concept itself ships.
- **Re-exports** (`pub use foo::bar;`).
- **Module-level `const`** is fine but follows the same
  visibility rules as fns.
- **`implement Iface for T` orphan rules** — Rust requires impls to
  live in the module of either the trait or the type. v1 enforces
  the same. (The IFACE_IMPL_REGISTRY already keys by
  `(iface_name, type_name)` so adding a module-of qualifier is a
  small extension.)

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

## Open questions for user

1. **Keyword name: `module` or `mod`?** Rust uses `mod`. vāṇī tends
   toward more readable spellings (`interface` over `iface`,
   `implement` over `impl`). Recommend **`module`** (and reuse the
   existing `impl` alias by adding `mod` if desired later).

2. **`pub` keyword: `pub` or `public`?** Per the within-language
   alias convention (#234), both would work. Recommend `pub` as the
   canonical spelling (shorter, matches Rust) and `public` as
   alias.

3. **File-as-module (Rust default) — v2?** Today `use "path.vani"`
   imports a whole file at the top level. Adding file-as-module
   would let `use "math.vani"` automatically create a `math` module
   wrapping the file's contents. Useful for multi-file projects
   but couples namespaces with multi-file work. Recommend v1 ships
   without it; v2 adds the convention.

4. **Orphan-rule strictness?** Rust requires `implement Iface for
   T` to live in the module of either Iface or T. v1 in vāṇī
   could either enforce that or allow free-standing impls. The
   strict rule prevents incoherent global impls but is sometimes
   surprising for newcomers. Recommend **enforce strict** — it's
   the compile-time-catching choice; users surface a clear error
   if they try to define an out-of-place impl.

Once these are confirmed, the implementation can proceed in
4-5 focused sub-commits (lexer → AST → parser → checker → tests).
