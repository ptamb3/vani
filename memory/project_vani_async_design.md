---
name: project-vani-async-design
description: "Async / asyncio for vāṇī — compiler-lowered state machines on an arena; explicitly NOT Rust-style Pin self-references"
metadata:
  node_type: memory
  type: project
---

User asked on 2026-05-27 to add async / asyncio to the TODO. The
canonical design lives in `/home/ptambe/vani/TODO.md` under *Async
/ asyncio — concurrency arc*. Summary here:

**Affine flag: ⚠️ AFFINE-TENSION (compiler-lowered state machines)
/ 🛑 NON-COMPLIANT (Rust-style `Pin<&mut Self>` self-references).**

**Why:** stackless coroutines need to capture locals across
`await` points. Rust uses Pin + self-referential structs — but
that's 🛑 NON-COMPLIANT per [[project-vani-affine-standing]].

## Canonical path

- Compiler lowers each `async fn` body to an enum-of-frames; each
  frame is an owned affine struct in `Vec<StateMachine>` arena.
- Frames never hold raw pointers into other frames; cross-frame
  data flows by index or by move on suspend / resume.
- Single-threaded event-loop driver: `intent_async_run(task)`
  polls the root state machine until completion.
- `Future<T>` generic enum with `Ready(T)` / `Pending` variants
  (uses #281 generic decls + #283 mixed-payload lift; lives in
  prelude alongside Option / Result).
- `await` is statement-or-expression sugar that the checker
  rewrites at the state-machine boundary.
- Non-blocking I/O primitives (file / socket / timer) lower to
  epoll / kqueue / IOCP under the hood; user sees `async fn` in
  stdlib.
- `Channel<T, N>` is the cooperative coordination primitive —
  async tasks `recv` / `send`; event loop parks on channel-state
  changes.
- Cancellation: explicit `CancelToken` passed by-ref; checked at
  each suspend point. NOT panic / unwind.

## Explicitly NOT shipping

- 🛑 `Pin<&mut Self>` self-references
- 🛑 Panic-based cancellation
- 🛑 Stackful coroutines / fibers
- 🛑 Async inside `parallel for` bodies (use `task` + `join` for
  parallelism; async-of-tasks if you need both)

## Dependency chain (L-tier multi-session arc)

1. Closures w/ captured state (Level 3 #17 in data-structures
   roadmap) — prerequisite.
2. `Future<T>` generic enum + Poll interface.
3. `async fn` parser + checker (state-machine transform at check
   time).
4. State-machine codegen on both backends (frame arena + `poll`
   dispatch).
5. Event-loop C runtime (epoll / kqueue / IOCP wrappers).
6. Non-blocking I/O primitives (file / socket / timer as
   `async fn` in stdlib).
7. `await` statement-or-expression sugar.
8. Cancellation via `CancelToken`.
9. `examples/async_io.vani` — timer fan-out + tiny TCP echo
   server; cross-backend parity.

## How to apply

- Async ships AFTER Level 3 closures. Until then, the README's
  *Memory & runtime model* says async is "queued."
- Reject any proposal to ship `Pin` / self-referential async;
  point at this memory + [[project-vani-affine-standing]].
- Condition variables ([[project-vani-condvar-design]]) are a
  useful building block for the eventual event-loop runtime —
  not a prerequisite, but they land first and naturally.

Cross-references: [[project-vani-affine-standing]],
[[project-vani-data-structures-roadmap]],
[[project-vani-container-affine-contract]],
[[project-vani-condvar-design]].
