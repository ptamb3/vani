---
name: project-vani-affine-standing
description: "Affine ownership is vāṇī's v1 standing decision — every container / algorithm / API proposed must be flagged for compliance with reasoning"
metadata:
  node_type: memory
  type: project
---

**Affine ownership is the standing language decision for vāṇī v1.**
The compiler tracks single-owner semantics across `Vec`, `OwnedStr`,
`Atomic`, `Mutex`, `Guard`, `Channel`, `Task`, user `Drop` types,
nested struct fields, partial moves, and parallel-for / task
captures. Deterministic destructors run at scope exit.

**Why:** the user re-stated this on 2026-05-27:

> "affine decision still stays. if something doesn't comply with
> affine flag in markdown files and reasoning."

He wants every proposed container, algorithm, or API in
[[project-vani-backend]] / TODO.md / STATUS.md / README.md
flagged with one of:

- ✅ **AFFINE** — single-owner holds end-to-end.
- ⚠️ **AFFINE-TENSION** — compiles, but the API needs a careful
  contract (e.g. `get -> Option<ref V>` not `V`;
  `remove() -> Option<V>` is the move-out path;
  `insert(k, v)` consumes both).
- 🛑 **NON-COMPLIANT** — cannot ship as designed; the
  affine-friendly substitute MUST be named.

**Affine-friendly substitute patterns** (use these by default):

- Linked structures → **index-based arenas** (`Vec<Node>` with
  `i32` child indices; `-1` = none).
- Shared ownership → `Channel<T, N>` (cross-task) or `Mutex<T>` /
  `Atomic<T>` (cross-thread shared state).
- Iteration → `for x in xs` (by Copy-value for Copy T; by-ref for
  non-Copy T); combinators are by-ref or consume-whole-Vec via
  `.fold` / `.collect`.
- Map / set lookup → `get(m, ref k) -> Option<ref V>` (borrowed
  view); `remove(m, ref k) -> Option<V>` is the move-out path;
  `insert(k, v)` consumes both. Full contract in
  [[project-vani-container-affine-contract]].

**Permanently deferred (with substitutes named):**

- 🛑 Rc / Arc reference-counted shared ownership → substitute:
  index graphs + `Channel` + `Mutex`.
- 🛑 Doubly-linked list with raw `prev` / `next` pointers →
  substitute: index-based Deque + index-based BST.
- 🛑 Iterators yielding owned `T` → substitute: by-ref iteration
  / `.fold` / `.collect`.
- 🛑 Self-referential structs (Pin) → substitute: arena pattern
  (also rules out Rust-style async; see
  [[project-vani-async-design]]).
- 🛑 Garbage collector (any flavor) → substitute: affine +
  scope-exit Drop.
- 🛑 Stackful coroutines / fibers → substitute:
  compiler-lowered stackless state machines (in async design).

**How to apply:**

- When proposing a new container / algorithm / concurrency
  primitive, lead with the affine flag. Examples:
  - "HashMap<K, V> — ⚠️ AFFINE-TENSION: `get -> Option<ref V>`
    keeps the entry in the map; `insert(k, v)` consumes both;
    `remove(k) -> Option<V>` is the move-out path."
  - "Doubly-linked list with raw prev/next — 🛑 NON-COMPLIANT:
    two pointers into one node violate single-owner. Substitute:
    index-based Deque + index-based BST."
- Reject any proposal that would relax affine for a single
  feature ("just one Rc, just for this case") — affine is the
  v1 design contract.
- Cross-references: [[project-vani-data-structures-roadmap]],
  [[project-vani-container-affine-contract]],
  [[project-vani-async-design]],
  [[project-vani-condvar-design]],
  [[feedback-vani-design-philosophy]].
