---
name: feedback-vani-language-design
description: "vāṇī language-surface preferences — file extension, language purity, alias style, deferred backends"
metadata: 
  node_type: memory
  type: feedback
  originSessionId: 656e7218-5702-41e7-a9dd-1764e5b8ee2c
---

User-stated design directions for the vāṇī language surface
(captured 2026-05-26). Use these when proposing changes to
the lexer / parser / type system / file layout.

## File extension is `.vani`

Currently `.intent` (legacy from project rename). Future
work should rename to `.vani`. Open: whether headers /
partial files get a separate suffix (`.vani.h`?) or whether
content distinguishes them.

**Why:** matches the project's current name; the legacy
`.intent` reflects the older project name.

**How to apply:** when touching example files or compiler
file-handling code, prefer / use `.vani`. Don't bulk-rename
existing files mid-session without user confirmation —
that's a 58-file rename plus test expectations.

## Language purity is per-file, not per-program

The lexer accepts keyword aliases from English / Sanskrit
/ Hindi / Marathi today, mixed in the same source file.
User wants a per-file restriction: once a file commits to
a language (e.g. by its first keyword, or an explicit
`lang english;` header), the rest of the file is bound to
that language's keyword set. Mixed-language errors should
be clear ("file declared as Hindi uses English keyword
'return'").

**Why:** language purity is a readability and consistency
property. Mixing distracts; consistency reads as written
prose.

**How to apply:** cross-file mixing stays allowed (one
file in English, another in Sanskrit, same program is
fine). Per-file purity needs lexer / parser cooperation
to track the current file's language and reject foreign
keywords.

## Within-language aliases are expected, not flagged

English and other languages should each carry multiple
spellings for the same concept (`struct` / `record`,
`return` / `give`, `->` / `returns` / `yields`, etc.).
The formatter picks a per-file canonical spelling based
on what the user chose first; the AST sees one token
kind regardless of which alias was typed.

**Why:** different mental models use different words for
the same concept; the language shouldn't force a single
spelling when multiple natural-language equivalents
exist.

**How to apply:** when extending the keyword table, group
aliases together. Don't introduce new keywords that
collide with planned aliases. Sanskrit / Hindi / Marathi
tables grow proportionally to English's alias set.

## Cranelift + x86_64-asm backends are far-future

Cranelift JIT and direct x86_64 assembly are tracked in
TODO but the user said "very last, not near future, may
not happen". Don't propose work on them unless the user
asks specifically. Spend session time on language-surface
and pipeline polish instead.

Related memory: [[project-vani-backend]] describes the
current pipeline (lexer → parser → checker → SSA → tree-C
+ tree-LLVM).
