# Prepared repo-root `CHANGE.md` entry — Milestone 5 (`beam_rt`)

This file holds the entry the **repo-root owner** should insert into `/CHANGE.md`, directly below the
`Format: … Newest first.` line and above the `## 2026-10-08 — Milestone 4: …` section.
`beam_rt` is scoped to `erts_rust/crates/beam_rt`, so it cannot write the repo-root `CHANGE.md`
itself; the text is staged here verbatim for a one-shot paste.

```markdown
## 2026-10-09 — Milestone 5: in-machine cross-module calls (a callee suspends), owner-death reclamation, runtime GC adoption
Scope: the `beam_rt` runtime's **call seam** and **lifetime management** — a `receive` inside a
cross-module callee now suspends instead of failing, the runtime reclaims a dead process's ETS
tables and port monitors, and `beam_gc` is driven from the process run loop.
- **Area:** `erts_rust/crates/beam_rt` (the `ExecutionContext` glue + runner), over the seam `beam_engine` exposes
- **Change:** `RuntimeContext` implements the engine's `ExecutionContext::resolve_external_call`, so a
  `call_ext*` callee that is runnable in this machine runs **in the caller's machine** — the engine
  stores the caller's return address and enters the callee's export label — rather than recursing
  through the synchronous `call_bif` → `run_export`. The override returns the *same* `Arc<Module>` the
  `CodeServer` holds (contexts dedupe by pointer identity) and the export label after `func_info`,
  loads an uncached callee on demand, and answers `Ok(None)` — preserving the synchronous path
  byte-for-byte — for every import it must not intercept (a native-floor module, a NIF-loaded module,
  a `beam_bifs` registry key, a module whose code observes its own call site, a foreign or unknown
  export). A **fun-bodied** process entry gets the same treatment: it resolves its own target once and
  enters/resumes through the label path. Consequence: `proc_lib:start/4` / `proc_lib:start_link/4` —
  and hence `gen_server`-class startup — park in `receive` and resume instead of folding into a
  `blocked` error.
- **Area:** `erts_rust/crates/beam_rt` (`runtime/ets_owner.rs`), over the `beam_process` `ProcessExitObserver`
- **Change:** The runtime installs a process-exit observer and gains two scheduler-pass **drains**. A
  dead process's ETS tables are released — dropped, or handed to the installed heir with the oracle's
  `{'ETS-TRANSFER', Tid, FromPid, Data}` — and its dangling incoming **port monitors** (an entry a
  *watcher* leaves on a port it will never see close) are swept. The observer only *records*
  obligations into its own cell, because a death can be reached while a BIF still holds a registry
  borrow; the drains discharge them with `try_borrow_mut` and defer — never panic, never drop — while
  that borrow is live. The one remaining gap is an **unnamed** ETS table, whose handle this runtime
  cannot prove at `ets:new/2` time.
- **Area:** `erts_rust/crates/beam_rt` (`context/gc.rs`, `runtime/gc.rs`)
- **Change:** The runtime now **drives `beam_gc`**: a scheduled process allocates on its own young heap
  and is collected at the engine's heap-admission point (`ensure_heap`), with the executing machine's
  `x[0..live)` plus the Erlang stack as the root set. Collection is armed per call by the slice's own
  budget context, so a nested machine can never clobber the outer roots; a module containing a
  float-family instruction is never collected over (`Machine::fr` is outside the root set); a failed
  collection halts that process's collection rather than the program; and a runtime-level store whose
  term cannot be copied onto the arena trips a global valve that freezes every process heap. An
  `alloc_words`-only workload (no `test_heap` admission point) still falls back to the pointer-stable
  arena and is not collected — reaching it needs a mutable root-set hook on `Machine`.
- **Area:** `erts_rust/crates/beam_rt` (`context/mod.rs`, `context/bridges.rs`)
- **Change:** Oversized-message delivery is delegated to `beam_message::send_fragmented` over the
  receiver's own heap + mailbox (`Process::heap_and_mailbox_mut`) instead of a hand-composed fragment
  sequence, and the host-seam gates delegate to `beam_bifs::hook_sets`' exported `needs_*`/`*_BIFS`
  tables — so a new host-seam consumer in `beam_bifs` can no longer be starved of its hook by a
  per-crate predicate that was not updated in step.
- **Why:** These are the runtime-side halves of the blocking-call and lifetime contracts: a suspendable
  cross-module call is the difference between a runtime that can start OTP applications and one that
  cannot, and an owner's death must not leave live ETS tables or dead port monitors behind. The GC
  adoption makes a long-running scheduled process's peak memory track its live set instead of its loop
  count. The differential gate stays at **0 mismatches** throughout (agree 785 → 787).
```
