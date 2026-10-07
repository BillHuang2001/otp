# `beam_process/tests` — integration evidence for the crate

## Intent
The cross-module integration tests that pin this crate's Milestone-1 guarantees (the unit tests live
in each `src/*.rs` module). Nothing here is production code; each file is one topic and states, in
its module docs, the guarantee it pins and how it is made non-flaky.

## API Surface (what each file pins)
1. `scheduler.rs` — pid allocation (+ serial bump on reuse, stale-pid rejection); round-robin
   scheduling order (one slice each, requeued at the tail on `Yield`, `Wait` leaves the queue);
   resume-on-send (blocked receiver re-enqueued in the same slice) + a two-process counter ping-pong.
2. `messaging.rs` — send into a receiver's mailbox, selective `receive` (FIFO, non-matches stay
   queued), `DeliverOutcome::Woke` vs `Enqueued`.
3. `signals.rs` — link exit propagation (non-trapping dies, trapping survives with
   `{'EXIT', A, Reason}`), monitor down-message, reason ownership (a structured reason survives the
   recipient's `Process::collect`); trapping-peer, cascaded A→B→C link, `{'DOWN'}`, self-signal.
4. `gc.rs` — safepoint GC over the process root set (x-register + stack + mailbox payload relocate
   while garbage is reclaimed).
5. `priority.rs` — four-level order (MAX → NORMAL → LOW, FIFO within a level), requeue at **own**
   priority on `Yield`, woken process resumes at its own priority, `process_flag` PCB support
   (`priority`/`sensitive`/`save_calls`).
6. `pending_signals.rs` — deferred (schedule-in) vs eager application, FIFO across senders, a
   structured reason surviving `collect`.
7. `ports.rs` — `PortTable` relationship round-trips + `ProcessTable::port_closed` (`{'DOWN', …}` for
   a monitor, `{'EXIT', Port, Reason}` for a trapping linked process, termination for a non-trapping
   one, unknown port rejected).
8. `remote.rs` — an external pid links/monitors via the same table entry points, is recorded
   separately, and every change is handed to a recording `DistSignalRouter`; `node_down` resolves then
   purges.
9. `dict.rs` — `Dict` semantics on the PCB + a structured dict value surviving `collect`.
10. `fragments.rs` — saved args surviving `collect` + all sub-rootsets relocating in one collect.
11. `smp.rs` (11 tests) — the SMP pool: N-on-M-scheduler progress (M ∈ {1,4}, 200 pids each, each
    process running `YIELDS + 1` runner invocations and **one** recorded slice); work stealing (a
    parked peer steals half of 128 woken receivers); a timer into a blocked receiver;
    `deliver_simple` wake; a dirty `Reschedule` round trip; lifecycle/observables
    (`stats`/`scheduler_stats` one entry per scheduler, `run_queue_len` under a gated runner,
    `shutdown`+`join` returns); a one-scheduler config runs to completion.
12. `lock_scope.rs` (3 tests) — the **bounded-quantum table lock**: a slice is serviced as
    `ceil(reductions_per_slice / lock_quantum)` runner invocations (one slice, not one per
    invocation; `lock_quantum == budget` reproduces the coarse one-lock-hold-per-slice behavior); a
    cross-thread send is serviced inside another process's slice and the woken peer runs **inside**
    that slice (both interleaving assertions fail under a whole-slice lock); and the same program
    keeps the same per-sender message order and `{'EXIT', …}` placement on 1 and 4 schedulers.
13. `timers.rs` — fire-into-sink with the exact target/message, deadline order (equal deadlines in
    insertion order), `cancel` prevents + is idempotent, `pending`/`next_deadline`, pre-`start`
    scheduling, `shutdown`/`join`.
14. `dirty.rs` — jobs run on dirty worker threads and every completion is routed verbatim
    (`Reschedule`/`Deliver`), `pending` rises/drains, N-worker concurrency (a `Barrier`), lifecycle +
    submit-after-shutdown, a 0-worker pool buffers and discards at join, sink-after-submit, a
    panicking job does not kill the pool, and a repeated `DirtyPool::start(1, 0)` + `join()` loop.
15. `progress.rs` — `wait` returns only after every active thread reaches the generation (also
    immediately when already there, and invoking the waker), inactive-thread exclusion (reactivation
    re-includes), concurrent `bump`s each happen once, `block`/`unblock` nest, `safepoint` never
    blocks on the caller's own slot, a clone shares the registry/generation/blocking state.

## Constraints
- **Anti-flakiness (BINDING).** Every assertion is forced by **synchronization**, never a bare
  `std::thread::sleep`: `mpsc` channels + `recv_timeout` carry the completion signals,
  `std::sync::Barrier`/gates coordinate concurrency, and `SchedulerPool::wait_idle(Some(timeout))` is
  the pool's converging idle wait (it also requires the in-flight count to be zero, so counters read
  after it are exact). A wedged pool fails with a clear timeout instead of hanging the suite.
- `lock_scope.rs` is the **only** file with wall-clock bounds; they are deliberately loose and
  two-sided (a cross-thread send must be serviced far below the slice's duration, and the slice must
  last far above that bound — the anti-vacuity guard).
- Run the whole crate gate with
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_process` (never `cd`,
  sandbox `$HOME` is read-only). Cross-checks against the real system ERTS belong to the sibling
  `../../../tools/differential/` harness, not here.

## Routing Table
- `scheduler.rs`, `messaging.rs`, `signals.rs`, `gc.rs`, `priority.rs`, `pending_signals.rs`, `dict.rs`, `fragments.rs` → the single-threaded scheduler, mailbox, signals, GC roots and PCB state
- `ports.rs`, `remote.rs` → the port table and distributed links/monitors
- `smp.rs`, `lock_scope.rs`, `timers.rs`, `dirty.rs`, `progress.rs` → the SMP pool (see `../src/smp/CONTEXT.md`)
