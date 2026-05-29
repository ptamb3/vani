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

**Shipped status (2026-05-29, through closure #351):**

- **Level 1 — ✅ COMPLETE.** All operations on existing
  primitives shipped through closures #293–#301, with
  heap-allocating string ops added in #348 (`str_trim`) /
  #349 (`str_replace`) / #350 (`str_split`). Hash family
  rounded out with `hash_f64` (#347) and SipHash-2-4
  (`siphash_i64` / `siphash_str`, #351).
- **Level 2 — ✅ COMPLETE.** Containers shipped through
  #302–#307, plus tombstone-aware `hashset_remove` (#342) and
  `hashmap_remove` (#343), and BTreeSet/BTreeMap range queries
  (#346).
- **Level 3 — ✅ COMPLETE** (modulo richer closure-as-value
  follow-ups). Anonymous fns (#308), eager `vec_map` / `_fold`
  / `_filter` (#309–#310), method-call sugar across containers
  (#311–#312), closures with captures (#314 + #315), fused
  single-pass combinators (#316–#317), auto-fusion (#318).
  Richer closure work (capture-by-ref, non-Copy captures,
  passing closures across function boundaries, `.collect()`,
  non-i64 element types) is queued under closure
  #354+ in TODO.md.
- **Level 4 — ✅ all 8 items shipped.** UnionFind (#325),
  BinaryHeap (#326), BloomFilter (#327), AVL Bst (#328 +
  #332), Graph + BFS/DFS/Dijkstra/A*/topo/Kruskal/Prim
  (#329 + #333–#338), Trie + delete + arena compaction + u8
  alphabet (#330 + #340 + #344 + #345), SkipList + remove +
  tail tracker (#331 + #339 + #341).

**How to apply (current state, 2026-05-29):**

- Levels 1–4 of this roadmap are done. The next focal area is
  the **post-Level-4 extensions** captured in TODO.md's
  *Granular queue (2026-05-29, after #351)* subsection:
  1. **#352 — `Hash`/`Ord` interface for user struct keys**
     (lets users put their own structs into HashSet/HashMap/
     BTreeSet/BTreeMap by implementing the trait). Routes
     through the existing interface-dispatch machinery
     (#220-#228) — *the natural next step*.
  2. #353 — Anonymous-fn shorthand `|x| x + 1`.
  3. #354+ — Richer closure-as-value support (multi-session).
  4. #355 — Trie sparse children (memory optimization).
  5. #356 — `btreeset_min/max`, `btreemap_min_key/max_key`.
  6. #357 — `vec_zip` (depends on tuple-element Vec).
- When the user says "continue" / "next" after closure #351,
  the next focal item is **#352 Hash/Ord interface** unless
  they specify otherwise.
- Deferred (intent recorded, not actively queued): non-Copy V
  AFFINE-TENSION shift; wider K/V widths; async; Kosh; SSA-LLVM
  atomicrmw multi-block; Devanagari SOV. See TODO.md
  *Deferred* subsection for the canonical list.

Cross-references: [[project-vani-affine-standing]],
[[project-vani-container-affine-contract]],
[[project-vani-async-design]],
[[project-vani-condvar-design]].
