---
name: project-vani-status-file
description: vani (formerly future-compiler) keeps a single-page STATUS.md with feature set + TODOs + known issues — update it whenever any of those three change
metadata: 
  node_type: memory
  type: project
  originSessionId: 656e7218-5702-41e7-a9dd-1764e5b8ee2c
---

> **Project renamed 2026-05-21**: `~/future-compiler/` → `~/vani/`.
> Same repo, same conventions.

The `~/vani` repo has a `STATUS.md` at the project root that
serves as the canonical single-page snapshot of (a) implemented
features, (b) pending TODOs in priority order, and (c) known issues.

**Why:** the user asked for one file that captures all three so they
don't have to cross-read the README, TODO.md, and conversation
history. Known Issues entries cross-reference TODO items where a fix
is queued.

**How to apply:** when you complete a unit of work in this repo,
update `STATUS.md` in the same commit:

- Feature added → add a bullet to the matching subsection (terse — the
  long form lives in [[project_vani_backend]]'s README reference).
- TODO closed → delete from the TODO list; if it had a Known Issues
  entry, delete or rewrite that too.
- TODO added → insert at its priority position; cross-reference any
  related Known Issues entry.
- Issue discovered → add to Known Issues; if a fix is planned, also
  add a TODO and link them inline.
- Issue resolved → delete the entry (not strike-through); TODO.md
  preserves the history.
- Test totals shifted → update the header line.
- Date roll → update `Last updated:` to today.

The file's own "Update protocol" section restates these rules so future
collaborators following it without this memory also keep it current.
