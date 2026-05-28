---
name: project-vani-data-structures-roadmap
description: "Data structures + algorithms roadmap (Levels 1-4, sequenced by dependency, all flagged for affine compliance)"
metadata:
  node_type: memory
  type: project
---

User asked on 2026-05-27 for a full data-structures + algorithms
roadmap under affine ownership:

> "what about all linear and non-linear data structures. I want
> all operations on data structures available in language
> including but not limited to sorting algorithms, search, etc."

> "yes update all todo, status, memory, readme files with this
> list and other updates so far in language. affine decision still
> stays. if something doesn't comply with affine flag in markdown
> files and reasoning. line these todo in sequence."

Roadmap captured in:
- `/home/ptambe/vani/TODO.md` — under *Data structures +
  algorithms roadmap (2026-05-27)* — full per-item plan + affine
  contract
- `/home/ptambe/vani/README.md` — under *Data structures +
  algorithms — affine-first roadmap* — compact table form
- `/home/ptambe/vani/STATUS.md` — under *Data structures +
  algorithms roadmap (added 2026-05-27)* — brief summary

**Why:** the user wants this on disk as the canonical roadmap so
future sessions don't re-derive it. The roadmap is the next focal
area after the closure #269–#291 burst (FFI + manifest + generics
+ try_vec + attributes + nested arrays).

**Levels:**

- **Level 1** — operations on existing primitives (✅ AFFINE):
  `Vec.sort` / `sort_by(fn)`, `reverse` / `dedup`, `find` /
  `contains` / `binary_search`, `pop` / `swap_remove` / `insert`
  / `clear`, `Array.sort` (Copy only), String ops
  (`split` / `contains` / `starts_with` / `ends_with` / `trim` /
  `replace`), `parse_int` / `parse_float`, math (`pow` / `abs` /
  `sqrt` / `sin` / `cos` / `tan` / `floor` / `ceil`, fn-form
  `min` / `max`), RNG (`seed_rng` / `rand_i64` /
  `rand_in_range`), `Hash` interface + FNV-1a / SipHash builtin.
- **Level 2** — generic containers (deps: Level 1 + generic decls
  #281):
  - HashSet<T> (✅ Copy / ⚠️ owning)
  - **HashMap<K, V>** — ⚠️ AFFINE-TENSION
    (`get -> Option<ref V>`; `insert` consumes; `remove` moves);
    see [[project-vani-container-affine-contract]].
  - BTreeSet<T> (✅ Copy / ⚠️ owning)
  - BTreeMap<K, V> — ⚠️ same contract
  - Deque<T> ring buffer over Vec (✅)
  - BinaryHeap<T> (✅)
- **Level 3** — closures + iterators (deps: Level 2 baseline):
  - Closures with captured state — ⚠️ capture-by-value moves;
    capture-by-ref produces a second-class closure
  - `.map` / `.filter` / `.fold` loop-fused, zero-alloc default
    (✅)
  - `sort_by` / `find_by` lifted to closure (✅)
- **Level 4** — advanced / arena-based (deps: Level 3):
  - BST / AVL / red-black via node arena + `i32` child indices
    (✅)
  - B-tree arena (✅)
  - Trie arena (✅)
  - Graphs as `Vec<Node>` + `Vec<Vec<u32>>` adjacency +
    algorithms (BFS / DFS / Dijkstra / A* / topo / Kruskal /
    Prim) (✅)
  - Union-Find (✅), skip list (✅), Bloom filter (✅)

**Deferred / 🛑 NON-COMPLIANT** (substitutes named):
doubly-linked list w/ raw pointers (→ Deque + index BST); Rc /
Arc (→ index graphs + Channel + Mutex); iterators yielding owned
T (→ by-ref iteration / .fold / .collect); Pin self-references (→
arena pattern); GC (→ affine + scope-Drop).

**How to apply:**

- When the user says "next" after 2026-05-27, the next focal
  area is **Level 1** of this roadmap — start with `Vec.sort` +
  `Vec.sort_by` (the most-requested algorithm; in-place
  quicksort + insertion-sort small-N cutoff). Files:
  `src/checker.rs` (builtin recognizer) + both backends + 2 lib
  tests + 1 example.
- Level 1 items can ship in any order — none have hard
  inter-dependencies within Level 1.
- Level 2 (HashMap etc.) needs the Level 1 Hash interface (item
  4j) PLUS the generic decls infrastructure shipped via #281.
- Level 3 closures is the prerequisite for the async arc
  ([[project-vani-async-design]]).

Cross-references: [[project-vani-affine-standing]],
[[project-vani-container-affine-contract]],
[[project-vani-async-design]],
[[project-vani-condvar-design]].
