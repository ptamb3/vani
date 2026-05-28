---
name: project-vani-condvar-design
description: "Condition variables for vāṇī — affine-compatible Condvar paired with Mutex<T> + Guard<T>; futex / WaitOnAddress / pthread-cond codegen"
metadata:
  node_type: memory
  type: project
---

User asked on 2026-05-27 to add condition variables to the roadmap.
Natural pairing with the existing `Mutex<T>` + `Guard<T>` story.
Fills the known gap: today the only blocking primitives are
`Channel<T, N>` recv and `Mutex` lock acquire — there is no "wait
until predicate becomes true" path.

**Affine flag: ✅ AFFINE.** Clean fit — the guard stays mut-borrowed
across `wait` (atomically released + re-acquired by the kernel),
so the predicate-check loop keeps using it without re-binding.

## Surface

```vani
let m: Mutex<i64> = mutex_new(0);
let cv: Condvar = condvar_new();
{
  let g: Guard<i64> = mutex_lock(m);
  while guard_get(ref g) < 10 {
    condvar_wait(ref cv, mut ref g);  // atomic release+wait+re-acquire
  }
  // predicate true here; g still owns the lock
}

// from another thread:
{
  let g: Guard<i64> = mutex_lock(m);
  guard_set(mut ref g, 10);
  condvar_notify_one(ref cv);
}
```

## API (builtins)

- `condvar_new() -> Condvar` — fresh affine handle, owns kernel
  waiter state.
- `condvar_wait(ref cv: Condvar, mut ref g: Guard<T>) -> ()` —
  atomically release the mutex behind `g`, park the caller, wake
  on notify, re-acquire the mutex, return. `g` stays mut-borrowed
  throughout — does NOT consume the guard. Caller MUST re-check
  the predicate (spurious wakeups are real).
- `condvar_wait_timeout(ref cv, mut ref g, timeout_ms: i64) -> bool`
  — same as `wait` but returns `false` on timeout, `true` on
  notify.
- `condvar_notify_one(ref cv: Condvar) -> ()` — wake exactly one
  waiter (FIFO not guaranteed; matches pthread / futex semantics).
- `condvar_notify_all(ref cv: Condvar) -> ()` — wake every waiter.

## Affine analysis

- `Condvar` itself is affine — only one binding owns it; copies
  rejected; scope-exit Drop frees the kernel handle. Mirrors
  `Mutex<T>` exactly.
- `wait` takes the guard by `mut ref`, not by-value, so the
  guard's affine lifetime extends through the wait. This is the
  key difference from a "consume the guard, return a new one"
  shape — that would force the user to re-bind on every loop
  iteration, which fights the natural
  `while !pred { cv.wait(&mut g) }` pattern.
- The kernel atomically releases + parks; from the checker's
  perspective, the guard's `locked` state never changes. Lock-graph
  analysis sees the guard as held continuously.

## Codegen

- **Linux** — futex on `cv.seq` counter:
  - `wait`: read seq, release mutex, `FUTEX_WAIT(addr=&cv.seq,
    val=seq_observed)`, re-acquire mutex on wake, loop.
  - `notify_one`: `cv.seq++; FUTEX_WAKE(addr=&cv.seq, n=1);`.
  - `notify_all`: `cv.seq++; FUTEX_WAKE(addr=&cv.seq,
    n=INT_MAX);`.
- **Windows** — `WaitOnAddress` / `WakeByAddressSingle` /
  `WakeByAddressAll` on the seq counter. Wrappers already exist
  alongside `Mutex` — reuse them.
- **Fallback (other Unix)** — pthread `pthread_cond_t` +
  `cond_wait` / `cond_signal` / `cond_broadcast`. Wrappers
  already declared in the C runtime; reuse.
- Runtime helpers go alongside `intent_mutex_lock` /
  `intent_mutex_unlock` in both backends' thread-runtime emit
  paths.

## What does NOT ship

- Statically tying a condvar to a specific mutex — matches
  pthread; user responsibility to pair them consistently.
- `Pin`-style self-referential coupling — unnecessary; the
  guard mut-ref model is the affine substitute.

## Effort + dependency

- **Effort: M (single session).** All runtime pieces already
  exist for `Mutex`.
- **Dependencies: none.** Independent of closures, generics,
  async — can ship before or in parallel with the data-structures
  roadmap.
- **Recommended slot:** after the mixed-payload-enum
  drop-dispatch follow-up, before Level 1 of the
  data-structures roadmap.

## Work breakdown

1. Lexer / parser — recognize the five builtins.
2. Type system — `Condvar` as a new affine builtin type (mirrors
   `Mutex`-without-payload shape).
3. Checker — signature pinning; affine tracking; lock-graph
   annotation (no-op for guard state).
4. Tree-C + tree-LLVM codegen — emit the futex / WaitOnAddress
   / pthread-cond paths.
5. SSA-C + SSA-LLVM codegen — mirror.
6. Tests — 2–3 lib tests (producer-consumer w/ bounded buffer,
   barrier-via-condvar).
7. Example — `examples/condvar_producer_consumer.vani`.
8. Cross-backend parity.

Cross-references: [[project-vani-affine-standing]],
[[project-vani-backend]] (concurrency section),
[[project-vani-async-design]] (condvar is also a building block
for the async event-loop runtime later).
