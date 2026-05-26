- [vāṇī backend state](project_vani_backend.md) — pipeline, backends, verifier, language surface, current closures landed
- [vāṇī STATUS.md update protocol](project_vani_status_file.md) — single-page feature set + TODOs + known issues file; update on every commit that changes any of those three
- [vāṇī design philosophy](feedback_vani_design_philosophy.md) — composition > inheritance, vtables = original intent only, build data structures from Vec, keep language minimal
- [vāṇī file access standing approval](feedback_vani_file_access.md) — read/write under /tmp and ~/vani without re-asking, plus cargo / intentc / git commit autonomy
- [vāṇī language design directions](feedback_vani_language_design.md) — file extension `.vani`, per-file language purity, within-language aliases expected, Cranelift/x86_64-asm deprioritized

<!--
Consolidation note (2026-05-25):
- vāṇī work formerly ran from ~/shortcut-mcp-server cwd; canonical home is now ~/vani/.
- Memory moved here from ~/.claude/projects/-home-ptambe-shortcut-mcp-server/memory/.
- ~/shortcut-mcp-server/future-compiler/ empty stub removed.
- ~/shortcut-mcp-server/.claude/settings.json stripped of 61 vāṇī-related Bash allowlist entries and the /home/ptambe/future-compiler additionalDirectories entry.
- Historical session JSONL logs under ~/.claude/projects/-home-ptambe-shortcut-mcp-server/ deleted (active session log retained while runtime writes to it).
- Future vāṇī sessions should launch with cwd = ~/vani/.
-->

