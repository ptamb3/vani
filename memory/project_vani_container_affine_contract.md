---
name: project-vani-container-affine-contract
description: "Affine-compatible container API contract — get / insert / remove / iter shapes for Map / Set / Deque / Heap under single-owner semantics"
metadata:
  node_type: memory
  type: project
---

Canonical container API shapes under vāṇī's affine model. Any new
container proposed for Level 2 of the data-structures roadmap MUST
follow these contracts.

**Why:** affine semantics make the "obvious" `get -> Option<V>`
shape unsound — calling `get` would move V out of the map and
leave the map with a hole that scope-exit Drop would still
double-free. The user wants these contracts written down so future
sessions don't re-derive them.

## Lookup (does NOT consume)

- `get(m, ref k) -> Option<ref V>` — borrowed view; map keeps the
  entry.
- `get_mut(m, ref k) -> Option<mut ref V>` — mutable borrow.
- `contains_key(m, ref k) -> bool`.
- `peek(ref h) -> Option<ref T>` — for BinaryHeap.

## Move-out (the explicit "I want ownership" path)

- `remove(m, ref k) -> Option<V>` — moves V out; caller must
  consume.
- `pop(h) -> Option<T>` — Deque / BinaryHeap.
- `swap_remove(xs, i) -> T` — Vec, O(1) swap with last element.

## Insert / mutate (consumes)

- `insert(m, k, v) -> Option<V>` — consumes both `k` and `v`;
  returns the **previous** value at that key (if any) which the
  caller must consume.
- `push_back(d, v)` / `push_front(d, v)` — Deque, consumes v.
- `push(h, v)` — BinaryHeap, consumes v.

## Iteration (does NOT consume by default)

- `for_each(m, fn(ref K, ref V))` — visit each entry by-ref.
- `for_each_mut(m, fn(ref K, mut ref V))` — mutable visit.
- `drain(m, fn(K, V))` — consumes the map; visitor gets owned K
  and V. Only API that moves entries out wholesale.

## Storage layout (affine drop discipline)

- Single backing `Vec<Entry<K, V>>` for HashMap / HashSet
  (open-addressing Robin Hood) so one drop walk frees everything.
- Single arena `Vec<Node>` + `i32` child indices for BTreeMap /
  BTreeSet / BST / B-tree / Trie. Indices, not pointers.
- Graphs: `Vec<Node>` for vertex data + `Vec<Vec<u32>>` adjacency
  list. Cycles in indices are fine — ownership graph stays
  acyclic.

## How to apply

- When designing a container, pick the API shape from this file
  before writing any code.
- If a proposed API would force a move-out from a read-only op,
  reshape to return `Option<ref V>` instead.
- If the container needs to hold non-Copy K or V, the contract
  becomes ⚠️ AFFINE-TENSION (document in TODO.md alongside the
  item with the flag).

Cross-references: [[project-vani-affine-standing]],
[[project-vani-data-structures-roadmap]].
