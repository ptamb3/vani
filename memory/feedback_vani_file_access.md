---
name: feedback-vani-file-access
description: Standing approval to read/write under /tmp and ~/vani, run cargo / intentc, and git commit in ~/vani without re-asking
metadata:
  node_type: memory
  type: feedback
---

Free to operate autonomously in the vāṇī workflow without asking on each step:
- Read / write any file under `/tmp` and `~/vani/` (and its subtree).
- Run any `cargo ...` command (build / test / run / clippy / etc.) in `~/vani/`.
- Run the `intentc` driver (and historical `intentllvm` alias) on probe files under `/tmp` or `~/vani/examples/`.
- Stage and `git commit` changes to `~/vani/` when the closure is complete (use the standard HEREDOC commit message format and skip user files like `vani_logo.png`).

**Why:** User has stated this multiple times (most recently 2026-05-24), gets annoyed by repeated permission prompts that interrupt the probe → fix → test → commit loop. The whole flow lives inside `~/vani/` with throwaway probe files in `/tmp`.

**How to apply:** When the work involves the [[project-vani-backend]] repo or temporary probe files for ASan/leak hunting, just create / edit / read / build / run / commit without seeking confirmation. Continue to confirm before:
- `git push` (visible to others).
- Destructive ops outside `~/vani/` or `/tmp`.
- Sharing diffs externally.
- Any `cargo` / `git` command in a different repo.
