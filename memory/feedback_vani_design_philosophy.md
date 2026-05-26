---
name: feedback-vani-design-philosophy
description: User's standing design decisions for vāṇī — composition over inheritance, vtables with original intent only, build data structures from Vec, keep language minimal
metadata:
  node_type: memory
  type: feedback
---

User pinned several design decisions in the 2026-05-25 session
(see README's "Design Philosophy & Limitations" section,
STATUS.md "Design Q&A", and TODO.md "Design rationale" for
the long-form versions).

**1. Composition over inheritance.**

> "vtables - use with original intent. hopefully language can
> live without inheritance."

vāṇī is a composition-first language. Interfaces dispatch
**statically** by default (monomorphized via bounded generics).
Vtables (epic A) ARE on the queue — but only for their
**original intent**: dynamic dispatch on heterogeneous
collections (`Vec<dyn Drawable>`). Layout is per-interface,
frozen, fat pointer `{ &vtable, &data }`. **No inheritance
hierarchy. No parent-class walks. No virtual destructors. No
abstract base classes.** Composition stays the canonical
pattern; `dyn Iface` is the escape hatch for open-set
collections only.

**2. `try` is the Rust `?` operator — NOT C++ try/catch.**

No exceptions. No unwinding. No catch keyword. Errors are
**values** in payloaded enums; `match` is how you "handle"
them. Every control-flow path is statically visible.
`assert` calls `abort()` (no recovery).

**3. Data structures: build from existing primitives, don't
proliferate built-ins.**

> "ok you can add new datastructure based on your judegement -
> implement from scratch or reuse vector. make best judgement."

The principle: **add a new built-in only when no composition
of existing primitives gets within an order of magnitude of
optimal**.

| Structure | How it's built |
|---|---|
| Stack | `Vec<T>` + `push` / `pop` (the `pop` builtin landed in #219) |
| Queue (concurrent) | `Channel<T, N>` |
| Queue (single-thread FIFO) | `Vec<T>` front-shift, or `Channel<T, N>` even single-threaded. `VecDeque` only if benchmarks justify it. |
| Set / Map small | `Vec<(K, V)>` linear scan |
| Set / Map large | Reserved for a future stdlib module (needs hash/cmp interface first) |
| Linked list / tree | Index-based via `Vec<T>` with `parent: i64` / `next: i64`. NOT pointer-based — fights affine ownership. |

**4. Keep language simple but still fix limitations.**

> "We need to keep language simple but still fix these
> limitations."

Active limitations:
- No dyn dispatch (epic A in progress — Phase 1 landed)
- Partial-move tracking one level deep (epic B)
- `Drop` suppressed for heap-field structs (epic C)
- Tuples Copy-only, `Mutex<i64>` only
- Generics: one type parameter, literal first-arg inference
- No closures (only `fn` pointers)
- No `bool ↔ int` cast (deliberate)
- Block-expr stmts limited to Let + Print

**How to apply:**

- When facing a "should we add a new built-in / type system
  feature" decision, default to **compose from existing
  primitives**. Only add a new built-in when composition is
  ≥10× worse than a direct implementation.
- For vtable design: per-Iface fat pointer, no hierarchy. If
  the design has any parent-class lookup, walk to super, or
  virtual-destructor walk, it's wrong — push back and
  re-design.
- For error handling questions: never invoke "exceptions" /
  "try/catch" / "unwinding" — refer to enum + `match` + `try`
  sugar.
- For "limitation X is annoying, let's fix" requests: see if
  it can be unblocked by completing one of the existing epics
  (A/B/C) rather than introducing parallel mechanisms.
