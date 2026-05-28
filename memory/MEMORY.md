- [vāṇī backend state](project_vani_backend.md) — pipeline, backends, verifier, language surface, current closures landed (refreshed #1-#291)
- [vāṇī STATUS.md update protocol](project_vani_status_file.md) — single-page feature set + TODOs + known issues file; update on every commit that changes any of those three
- [vāṇī design philosophy](feedback_vani_design_philosophy.md) — composition > inheritance, vtables = original intent only, build data structures from Vec, keep language minimal
- [vāṇī file access standing approval](feedback_vani_file_access.md) — read/write under /tmp and ~/vani without re-asking, plus cargo / intentc / git commit autonomy
- [vāṇī language design directions](feedback_vani_language_design.md) — file extension `.vani`, per-file language purity, within-language aliases expected, Cranelift/x86_64-asm deprioritized
- [vāṇī affine ownership — standing v1 decision](project_vani_affine_standing.md) — every container / algorithm / API must carry ✅ AFFINE / ⚠️ AFFINE-TENSION / 🛑 NON-COMPLIANT flag with reasoning
- [vāṇī data structures + algorithms roadmap](project_vani_data_structures_roadmap.md) — Levels 1-4 sequenced (sort / find / HashMap / BTree / Deque / BinaryHeap / closures / iterators / arena-based trees + graphs); all flagged
- [vāṇī container API affine contract](project_vani_container_affine_contract.md) — get / insert / remove / iter shapes for Map / Set / Deque / Heap under single-owner
- [vāṇī condition variables (Condvar) design](project_vani_condvar_design.md) — pairs with Mutex<T> + Guard<T>; futex / WaitOnAddress / pthread-cond codegen; ✅ AFFINE; single-session M effort
- [vāṇī async / asyncio design](project_vani_async_design.md) — compiler-lowered state machines on arena; explicitly NOT Pin / self-references; depends on Level 3 closures

<!--
Consolidation note (2026-05-25):
- vāṇī work formerly ran from ~/shortcut-mcp-server cwd; canonical home is now ~/vani/.
- Memory moved here from ~/.claude/projects/-home-ptambe-shortcut-mcp-server/memory/.
- ~/shortcut-mcp-server/future-compiler/ empty stub removed.
- ~/shortcut-mcp-server/.claude/settings.json stripped of 61 vāṇī-related Bash allowlist entries and the /home/ptambe/future-compiler additionalDirectories entry.
- Historical session JSONL logs under ~/.claude/projects/-home-ptambe-shortcut-mcp-server/ deleted (active session log retained while runtime writes to it).
- Future vāṇī sessions should launch with cwd = ~/vani/.
-->

