# `beam_process/tests` — integration evidence for the crate

## Intent
The cross-module integration tests that pin this crate's Milestone-1 guarantees (the unit tests live
in each `src/*.rs` module). Nothing here is production code; each file is one topic and states, in
its module docs, the guarantee it pins and how it is made non-flaky.

## API Surface (what each file pins)
Single-threaded scheduler, mailbox, signals, GC roots, PCB state:
1. `scheduler.rs` — pid allocation (+ serial bump on reuse, stale-pid rejection); round-robin
   scheduling order (one slice each, requeued at the tail on `Yield`, `Wait` leaves the queue);
   resume-on-send (blocked receiver re-enqueued in the same slice) + a two-process counter ping-pong.
2. `messaging.rs` — send into a receiver's mailbox, selective `receive` (FIFO, non-matches stay
   queued), `DeliverOutcome::Woke` vs `Enqueued`.
3. `signals.rs` — link exit propagation (non-trapping dies, trapping survives with
   `{'EXIT', A, Reason}`), monitor down-message, reason ownership (a structured reason survives the
   recipient's `Process::collect`); trapping-peer, cascaded A→B→C link, `{'DOWN'}`, self-signal. Also
   `a_targets_death_resolves_the_watchers_outgoing_monitor` (a target's death drops the watcher's
   outgoing `Monitor`, so a later `demonitor` reports it gone) and
   `link_to_a_non_live_pid_signals_noproc_from_it` (a non-live `link` target signals `noproc` from it:
   a trapping caller is handed `{'EXIT', Target, noproc}`, a non-trapping one is terminated with
   `noproc`).
4. `gc.rs` — safepoint GC over the process root set (x-register + stack + mailbox payload relocate
   while garbage is reclaimed).
5. `priority.rs` — four-level order (MAX → NORMAL → LOW, FIFO within a level), requeue at **own**
   priority on `Yield`, woken process resumes at its own priority, `process_flag` PCB support
   (`priority`/`sensitive`/`save_calls`).
6. `pending_signals.rs` — deferred (schedule-in) vs eager application, FIFO across senders, a
   structured reason surviving `collect`.
7. `preemption.rs` — reduction-quantum fairness with **exact measured slice counts** (2/3/4
   processes interleave strictly round-robin, within one slice of each other; `Wait` leaves the
   ready queue; `Yield` requeues at the process's own **current** priority). Also pinpoints the truth
   that Milestone-1 pre-emption is **cooperative only**: the schedule is invariant under the
   reduction budget, and the "a runner never returns hangs the scheduler" failure mode is stated as
   an `#[ignore]`d test (it cannot be exercised without hanging the suite).
8. `dict.rs` — `Dict` semantics on the PCB + a structured dict value surviving `collect`.
9. `fragments.rs` — saved args surviving `collect` + all sub-rootsets relocating in one collect.
10. `dict_oracle.rs` (12 tests) — the process dictionary **cross-checked against the real `erl`**
    (OTP 29.1.1 on `PATH`; the suite **skips loudly** when it is absent): the boxed-key reproducer
    (`put({a},1), get({a})` ⇒ `1`) plus tuple/list/map/binary/atom/integer/float keys, `erase` by a
    structurally equal key, repeated puts replacing in place, `get_keys_for_value` with a boxed value,
    a nested composite key, similar-but-unequal keys staying distinct, and `get_keys` key-set
    agreement. Each assertion is the oracle's own answer (a tiny text protocol over `erl -noshell
    -eval`), so it pins the C's `EQ` key/value equality, not just internal consistency.
11. `registry_monitors.rs` — the local name registry (`register`/`unregister`/`whereis`/`registered`/
    `name_of`, incl. the `badarg` for a dead pid or an unregistered name), liveness
    (`is_process_alive`) and exit-reason observability, a monitor of a dead/never-existing target
    (immediate `noproc` DOWN), a monitor of a registered **name** (the `{Name, Node}` target and the
    no-local-node typed diagnostic), and the flushed demonitor. The death-side resolution of a
    watcher's outgoing monitor is asserted for a **named** monitor too (the `{Name, node}` descriptor
    survives until its DOWN materialises). The `oracle_*` tests drive the real `erl` and skip loudly
    when it is absent, including `oracle_a_targets_death_resolves_the_watchers_outgoing_monitor` and
    `oracle_link_to_a_non_live_pid_signals_noproc_from_it`.
12. `capability_inventory.rs` — the crate's **typed-error / capability inventory** (objective items
    2/6/8): the atom-target monitor with no local node installed ⇒ the exact `unsupported`; the
    `erlang:monitor/2` **type dispatch** (`time_offset`/unknown ⇒ no kind ⇒ the oracle's `badarg`);
    a non-pid/non-atom/non-port target ⇒ `badarg`; `monitor(port, NotAPort)` ⇒ `badarg`; external-pid
    operations with **no** `DistSignalRouter` ⇒ documented record-only behaviour (no fabricated
    message); a relationship mutation on an **unallocated** port ⇒ `badarg` with no side effect; and
    the live port monitor recorded on both halves (`demonitor_port` removes them; `port_closed`
    delivers `{'DOWN', Ref, port, Port, Reason}`).
13. `process_info_source.rs` — the `erlang:process_info/2` **item → accessor** table, derived from
    this crate's public API alone: `status`/`registered_name`/`message_queue_len`/`links`/`monitors`/
    `monitored_by`/`trap_exit`/`priority`/`reductions`, with the oracle's `[]`/`undefined`
    conventions (e.g. `registered_name` is `[]` when unregistered, `status` is `undefined` for a dead
    pid).
14. `spawn_options.rs` — the **full spawn-option composition** this crate is responsible for:
    `alloc_pid` + `HeapPolicy::from_spawn_opts(min_heap, min_bin_vheap, fullsweep)` +
    `Process::with_policy_and_priority` + `ProcessTable::insert` + priority-correct enqueue, and
    `{link,_}`/`{monitor,_}` through the table. The applied heap policy is proven to have reached the
    collector honestly via `Collector::policy()` (and its `fullsweep_due`/initial-space sizing). This
    is the contract the sibling `beam_rt`/`beam_bifs` spawn-option path adopts.
15. `ports.rs` — `PortTable` relationship round-trips + `ProcessTable::port_closed` (`{'DOWN', …}` for
    a monitor, `{'EXIT', Port, Reason}` for a trapping linked process, termination for a non-trapping
    one, unknown port rejected).
16. `remote.rs` — an external pid links/monitors via the same table entry points, is recorded
    separately, and every change is handed to a recording `DistSignalRouter`; `node_down` resolves then
    purges.
SMP pool:
17. `smp.rs` (11 tests) — the SMP pool: N-on-M-scheduler progress (M ∈ {1,4}, 200 pids each, each
    process running `YIELDS + 1` runner invocations and **one** recorded slice); work stealing (a
    parked peer steals half of 128 woken receivers); a timer into a blocked receiver;
    `deliver_simple` wake; a dirty `Reschedule` round trip; lifecycle/observables
    (`stats`/`scheduler_stats` one entry per scheduler, `run_queue_len` under a gated runner,
    `shutdown`+`join` returns); a one-scheduler config runs to completion.
18. `lock_scope.rs` (3 tests) — the **bounded-quantum table lock**: a slice is serviced as
    `ceil(reductions_per_slice / lock_quantum)` runner invocations (one slice, not one per
    invocation; `lock_quantum == budget` reproduces the coarse one-lock-hold-per-slice behavior); a
    cross-thread send is serviced inside another process's slice and the woken peer runs **inside**
    that slice (both interleaving assertions fail under a whole-slice lock); and the same program
    keeps the same per-sender message order and `{'EXIT', …}` placement on 1 and 4 schedulers.
19. `timers.rs` — fire-into-sink with the exact target/message, deadline order (equal deadlines in
    insertion order), `cancel` prevents + is idempotent, `pending`/`next_deadline`, pre-`start`
    scheduling, `shutdown`/`join`.
20. `dirty.rs` — jobs run on dirty worker threads and every completion is routed verbatim
    (`Reschedule`/`Deliver`), `pending` rises/drains, N-worker concurrency (a `Barrier`), lifecycle +
    submit-after-shutdown, a 0-worker pool buffers and discards at join, sink-after-submit, a
    panicking job does not kill the pool, and a repeated `DirtyPool::start(1, 0)` + `join()` loop.
21. `progress.rs` — `wait` returns only after every active thread reaches the generation (also
    immediately when already there, and invoking the waker), inactive-thread exclusion (reactivation
    re-includes), concurrent `bump`s each happen once, `block`/`unblock` nest, `safepoint` never
    blocks on the caller's own slot, a clone shares the registry/generation/blocking state.
22. `monitor_tag.rs` (8 tests) — the `erlang:monitor/3` **user tag** cross-checked against the real
    `erl` (skips loudly when absent): a tagged live/exit/`noproc` process monitor delivers
    `{Tag, Ref, process, Target, Reason}`, the untagged `monitor/2` DOWN is unchanged, a tagged named
    monitor reports the `{Name, Node}` target, a tagged port monitor delivers `{Tag, Ref, port, Port,
    Reason}`, and a non-immediate tag is this crate's typed `unsupported` (the oracle accepts any term).
23. `split_borrow.rs` (5 tests) — the two `Process` split borrows. `heap_and_off_heap_mut`: one call hands
    out the process's OWN young heap + off-heap table (identity pinned with `core::ptr::eq` against
    `heap_mut()`/`off_heap_mut()`), and a `beam_message::Fragments` chain is moved onto that heap with
    `move_into(heap, off_heap)` (the heap grows by exactly the copied cons-cell count). `heap_and_mailbox_mut`:
    identity pinned the same way, and a `beam_message::send_fragmented` call through the split borrow lands
    the message in the process's OWN mailbox — both the ordinary copy (payload words counted on the young
    heap) and the forced fragment fallback (a payload ≫ the default young heap).
24. `process_exit_observer.rs` (10 tests) — the `ProcessExitObserver` process-death seam: a registered
    observer is called **exactly once per death** with the right `pid`/`reason` for a normal exit, a `kill`,
    and a link cascade (A links B ⇒ one call per death), the `name`/`port_monitors` payload fields are the
    dying process's own, an **absent** observer leaves `terminate` unchanged, and a **panicking** observer
    cannot corrupt the table (the pid is still reclaimed, links/monitors stay consistent, spawn/deliver still
    work) and its panic never escapes `terminate`.
25. `port_monitor_cleanup.rs` (8 tests) — the death-side port-monitor cleanup: after a port-watcher dies
    through `terminate_with_ports`, the port's incoming `PortMonitor` is gone and the watcher's outgoing
    `port_monitors` is drained; the link-cascade and `service_signals_with_ports` schedule-in cases are
    covered, `port_closed`'s nested peer cleans too, and the port-table-less `terminate` leaves the port side
    alone (documented delegation). Two `erl`-cross-checked `oracle_*` tests (skipping loudly when `erl` is
    absent) pin the same end state against the real system — a process `monitor(port, P)` is listed by
    `port_info(P, monitored_by)` while alive and gone after it dies (`{spawn, Cmd}` uses
    `exec sleep 30 >/dev/null 2>&1` so the OS child never holds the probe's stdout pipe).
26. `scheduler_ports.rs` (12 tests) — the port-aware scheduler slice. A death during a slice cleans the
    dangling incoming `PortMonitor` **through the default `run_slice`/`run_ready` when the scheduler owns
    a `PortTable`** (`Scheduler::with_ports`/`set_ports`), and the same holds for a multi-slice
    `run_ready` loop and for a fatal signal deferred while the target was `Running` (terminated at
    schedule-in, the runner never entered); the explicit `run_slice_with_ports`/`run_ready_with_ports`
    forms are still pinned (an explicitly passed table takes precedence over the owned one); a scheduler
    **without** a `PortTable` is the port-less control (the incoming entry stays); the owned-table
    accessors round-trip; and the wake list is absorbed after the slice and on the empty-queue early
    return.
27. `exhaustion.rs` (4 tests) — `Process::deliver`'s collect-grow-and-retry over the **bounded**
    nursery: a payload larger than the whole nursery is refused, the single retry collects once and
    grows the nursery on demand to `next_heap_size(payload, 0)` (`minor_gcs == 1`) and the payload is
    enqueued whole and exactly once, FIFO preserved for a second delivery; `ensure_capacity` is pinned
    directly as the on-demand growth seam (empty nursery ⇒ `available_words() >= need_words`); and the
    typed `Err(BeamError::Alloc(AllocKind::Message))` path (a `from_min_max(cap, cap)` policy cannot
    grow ⇒ the growth is refused **fail-fast** with `minor_gcs == 0`, mailbox untouched, no partial
    write, no panic, and a fitting payload still delivered afterwards).
28. `gc_exemption.rs` (4 tests) — the **host-controllable GC relocation exemption**
    (`Process::set_relocation_exempt`/`is_relocation_exempt` + the `ProcessTable` convenience), the
    white-box evidence that it *works*: while exempt, a delivery too large for the nursery returns the
    **raw** `Err(BeamError::Alloc(AllocKind::Heap | Message))` and neither `minor_gcs()` nor the nursery
    `base()` moves, and a Rust-local term written into that nursery (the host's unpublished live root) is
    still structurally intact afterwards; the **counterpart** pins that the *same* delivery on an
    **unexempt** process **does** collect + grow (`minor_gcs` up, `base` moved, message enqueued whole);
    the defensive entries (`collect_if_needed` ⇒ `Ok(false)`; `collect`/`ensure_capacity` ⇒ the typed
    `relocation_exempt` raise, reversible by clearing the flag); and the `ProcessTable` delegation
    (unknown pid ⇒ `TermNotFound`). **Discriminating by construction: it FAILS the moment `deliver`
    collects despite the exemption** (removing the `deliver` short-circuit *and* the `ensure_capacity`
    guard makes both the no-relocation assertions and the defensive-entry assertions fail — verified
    during development, then restored). Deterministic — every size from the public heap-policy arithmetic,
    no sleeps / wall-clock bounds.
29. `signal_exhaustion.rs` (4 tests) — the **signal builders' exhaustion policy**: the
    `copy_reason_onto`/`build_signal_term`/`deliver_head_signal` paths (through the shared
    `grow_receiver_for_signal`) treat a full receiver heap exactly as `Process::deliver` does. A
    `{'EXIT', From, Reason}` whose copied reason is one word **larger** than the receiver's whole
    bounded nursery is delivered: the refused copy collected once (`minor_gcs` up), grew the nursery on
    demand to `next_heap_size(payload, 0)`, moved the base, and lands whole and exactly once in FIFO
    order ahead of a following delivery (`an_oversized_exit_reason_grows_the_receivers_nursery_and_is_delivered`);
    the same for a monitor `{'DOWN', …}` carrying a large reason
    (`an_oversized_down_reason_grows_the_watchers_nursery_and_is_delivered`). A **relocation-exempt**
    receiver instead surfaces the **raw** `Err(BeamError::Alloc(AllocKind::Heap | Message))` and neither
    `minor_gcs()` nor the nursery `base()` moves, nothing is written and nothing is enqueued
    (`a_relocation_exempt_receiver_surfaces_the_raw_refusal_without_relocating`); and the typed failure
    leaves **no partial enqueue** (the signal is not queued, the mailbox is empty) with the receiver
    still delivering a fitting signal afterwards
    (`a_refused_signal_leaves_no_partial_state_and_the_receiver_still_delivers`). **Discriminating by
    construction: the two delivery tests FAIL (a typed `Alloc` out of `terminate`) the moment the
    builders only collect without growing, and the exemption test FAILS (a `relocation_exempt` raise
    instead of the raw `Alloc`) the moment the short-circuit is removed.** Deterministic — every size
    from the public heap-policy arithmetic, no sleeps / wall-clock bounds / oracle.
30. `gc_refc_binary_message.rs` (1 test) — the collection's **mailbox rebuild preserves a queued
    message's off-heap ownership**. A 200-byte payload (> `ERL_ONHEAP_BINARY_LIMIT` = 64, so its bytes
    live in an off-heap counted `beam_alloc::RefcBinary`) is written as the 8-word `[BinRef][ErlSubBits]`
    pair, its handle registered in the source `OffHeapTable`, and delivered through
    `ProcessTable::deliver(to, from, term, &src_off_heap)`; the copy shares the payload, so the queued
    message owns a handle (refcount 2). The sender's table is then dropped, leaving the **message the
    sole owner** (refcount 1), and `Process::collect` runs on the receiver while the message is queued.
    The test asserts the rebuilt envelope still carries the counted binary (`binaries().len() == 1`,
    same `data_ptr`, refcount unchanged, `logical_binary_bytes()` = the payload length) and that the
    relocated payload is still a readable refc binary with the original bytes whose (post-copy)
    `BinRef.val` — reached through the `ErlSubBits.orig` fix-up, not by assuming adjacency — is the very
    same off-heap pointer. **Discriminating by construction: FAILS (an empty `binaries()`, the payload
    already freed) the moment the rebuild goes back to `Message::new(...)` plus the token/off-heap
    builders instead of `Message::with_payload`, which keeps the whole envelope.** The handle assertion
    is checked before the payload is read, so a pre-fix run never dereferences the freed bytes;
    deterministic — a fixed payload pattern and an explicit `collect`, no sleeps / wall-clock bounds /
    oracle.
31. `common/mod.rs` — the **shared oracle driver** used by `registry_monitors.rs`, `dict_oracle.rs`,
    `monitor_tag.rs`, `lifecycle_oracle.rs`, `capability_inventory.rs` and `port_monitor_cleanup.rs`:
    `ERL_TIMEOUT`, `erl_stdout`, `expect_oracle` and `strip_logger_reports` (+ 4 unit tests for the
    normalizer). It runs
    `erl -noshell -kernel logger_level none -eval …` (the `none` level deterministically suppresses the
    `=ERROR REPORT====` block a crashed probe helper would otherwise race onto **stdout**) and normalizes
    the captured stdout (a report block — a line starting with `=`, through the next blank line — is
    removed), so a comparison is exact and never flaky. A support module (subdirectory files are not
    compiled as test targets); it carries `#![allow(dead_code)]` because not every including binary uses
    every helper.
32. `lifecycle_oracle.rs` (7 tests) — the **lifecycle / process-control semantics** cross-checked against
    the real `erl` (skips loudly when absent), filling the cases the other `oracle_*` suites leave
    unpinned: link exit propagation from a **live** linked origin (a non-trapping peer dies, a trapping
    peer survives with `{'EXIT', A, {bad,1}}`, and the watcher's DOWN carries the same structured
    reason); **`kill` under `trap_exit`** (an untrappable `killed` death of a trapping process, with a
    trapping link peer receiving `{'EXIT', P, killed}` and a `killed` DOWN); the `noproc`/`normal`/
    `killed` exit-reason **shapes** at a trapping peer (incl. an explicit `killed` being *trappable*,
    unlike the `kill` sentinel); `erlang:exit/2` propagating a structured reason verbatim with `From` the
    signaller; the registry's `register`/`whereis`/`registered`/`unregister` round trip; `demonitor`
    `[info]` bool semantics (with the born-down divergence **documented, not asserted for parity**); and
    `demonitor(Ref, [flush])` removing a queued DOWN for a **pid** target. Every oracle expression is
    synchronized on a message (no sleeps); variable names are kept distinct per receive in the `-eval`
    text so no earlier binding is silently re-used as a pattern.
33. `terminate_best_effort.rs` (2 tests) — the **best-effort termination cascade** of
    `ProcessTable::terminate`/`terminate_with_ports`: a dying process is reclaimed *first*, so a
    per-peer signalling failure is dropped-and-logged (ERTS's `erts_dsig_drop`) and the cascade
    continues, letting the process-death observer and the port-monitor cleanup always run. Two
    failures are forced, each hitting a different step: a `DistSignalRouter` whose transport always
    fails (`Err(BeamError::Io(NotConnected))`) despite which the routing is **attempted once per
    remote link partner** — asserting the second partner was still routed after the first failed —
    and a relocation-exempt link peer whose bounded nursery refuses the exit-reason copy (raw
    `Alloc`), after which the *next* link partner is still signalled. In both cases `terminate`
    returns `Ok`, the observer fired **exactly once**, the local watcher still got its
    `{'DOWN', …}`, and the dead watcher's incoming `PortMonitor` is gone. **Discriminating by
    construction: both tests FAIL (a typed `Io` / `Alloc` out of `terminate`, observer unrun, port
    monitor still listed) the moment the per-peer `?`-propagation is restored.** Deterministic —
    the refusal size is the exempt peer's own bounded nursery capacity + 1 word, no sleeps /
    wall-clock bounds / oracle.
33. `message_relocation.rs` (4 tests) — the collection's **queued-message re-install**: a collection
    (`Process::collect`/`ensure_capacity` via `run_rooted_collection`) must carry a queued message's
    **whole envelope** — it re-installs each message with `Message::with_payload`, so a fragment-resident
    payload keeps its `Fragments` chain and a heap-resident one keeps its refc-binary/node lists. A
    `Message::new(root, from).with_fragments(Fragments)` (root copied via `HeapFragment::copy_root`) is
    queued, the nursery is filled with unrooted garbage, `Process::collect` runs, and the message still
    has `fragments().is_some()`, `fragment.contains(payload)` still true, `from` intact and the payload
    readable (`a_fragment_resident_message_survives_a_collection_with_its_fragment_chain`); a
    `{RefcBinary, ExternalPid}` payload delivered through `Process::deliver` keeps `binaries().len()==1`
    with the shared-owner refcount at 3 (one handle each for the test, the source table and the message —
    **neither dropped early nor registered twice**) and `nodes().len()==1`
    (`an_off_heap_payload_keeps_its_binary_and_node_lists_across_a_collection`); mixed
    heap/fragment/heap messages keep FIFO order (`a_collection_preserves_mailbox_fifo_order_across_mixed_message_kinds`);
    and a `relocation_exempt` receiver refuses the collection (the typed `relocation_exempt` raise,
    `collect_if_needed` ⇒ `Ok(false)`, `minor_gcs()==0`) leaving the queued message untouched
    (`an_exempt_process_refuses_the_collection_and_leaves_the_queued_message_untouched`).
    **Discriminating by construction: 3 of the 4 tests FAIL on the pre-fix rebuild** (the
    `Message::new(payload, from).with_token(..).with_off_heap(..)` form, which drops `fragments`/
    `binaries`/`nodes`) — verified by temporarily restoring step 6, then reverting. Deterministic — the
    fragment/ownership assertions, no sleeps / wall-clock bounds / oracle.
34. `off_heap_send_isolation.rs` (2 tests) — the **send-path staging-table isolation**: `Process::deliver`
    stages its copy through a fresh per-send `OffHeapTable`, so a send must NOT drain the receiver's
    **persistent** off-heap table (a queued signal's reason objects / a host's moved-fragment objects). A
    200-byte refc binary pushed onto `process.off_heap_mut()` stays present with the same
    `data_ptr`/refcount after a delivery to that same process, and the queued message owns only what it
    copied (a payload that really references an off-heap binary is still captured). **Both tests FAIL on
    the pre-fix code** (which passed `self.off_heap` as the drained destination) — verified by reverting.
35. `message_from_is_immediate.rs` (6 tests) — every signal path's `Message::from` is an **immediate**
    (the C's `is_immed(ERL_MESSAGE_FROM)` invariant) and survives a collection: a named-monitor DOWN
    (`from == target.to_eterm()`, body still `{Name,Node}`), an ordinary-monitor DOWN, a failed-by-name
    named-monitor DOWN (`from == system`), a remote DOWN and a remote EXIT (`from == node`, body still the
    external pid), and the collection case (a nursery GC then `is_immediate(from)` + `from` unchanged,
    the relocated payload still carrying `{Name,Node}`). **5 of the 6 FAIL on the pre-fix code** (which
    stored an on-heap `from`) — verified by reverting.## Constraints
- **Anti-flakiness (BINDING).** Every assertion is forced by **synchronization**, never a bare
  `std::thread::sleep`: `mpsc` channels + `recv_timeout` carry the completion signals,
  `std::sync::Barrier`/gates coordinate concurrency, and `SchedulerPool::wait_idle(Some(timeout))` is
  the pool's converging idle wait (it also requires the in-flight count to be zero, so counters read
  after it are exact). A wedged pool fails with a clear timeout instead of hanging the suite.
- `lock_scope.rs` is the **only** file with wall-clock bounds; they are deliberately loose and
  two-sided (a cross-thread send must be serviced far below the slice's duration, and the slice must
  last far above that bound — the anti-vacuity guard).
- The `oracle_*` suites (`dict_oracle.rs`, `registry_monitors.rs`, `monitor_tag.rs`,
  `lifecycle_oracle.rs`, and the `erl`
  cross-checks in `capability_inventory.rs`/`port_monitor_cleanup.rs`) go through the **shared driver**
  `common/mod.rs`, **skip loudly** when `erl` is absent, and must never spawn a child process that
  inherits `erl`'s stderr (a captured-stderr pipe blocks until the child dies — the probes are written
  child-free).
- Run the whole crate gate with
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_process` (never `cd`,
  sandbox `$HOME` is read-only). Cross-checks against the real system ERTS belong to the sibling
  `../../../tools/differential/` harness, not here. Current: **417 passed, 0 failed, 1 ignored**.

## Routing Table
- `scheduler.rs`, `messaging.rs`, `signals.rs`, `gc.rs`, `priority.rs`, `pending_signals.rs`,
  `preemption.rs`, `dict.rs`, `fragments.rs`, `scheduler_ports.rs`, `exhaustion.rs`,
  `signal_exhaustion.rs`, `gc_exemption.rs`, `message_relocation.rs`, `gc_refc_binary_message.rs` →
  the single-threaded scheduler (incl. the port-aware slice cleanup), mailbox, signals, GC roots, PCB state,
  pre-emption/fairness, the collect-grow-and-retry exhaustion path of `Process::deliver` and the
  `{'EXIT',…}`/`{'DOWN',…}` signal builders, the host-controllable GC relocation exemption, and the
  collection's queued-message re-install (a fragment-resident/off-heap-owning message keeps its whole
  envelope across a collect)
- `registry_monitors.rs`, `monitor_tag.rs`, `capability_inventory.rs`, `process_info_source.rs`,
  `spawn_options.rs`, `lifecycle_oracle.rs` → the process-control surface: name registry + monitors (incl.
  the `monitor/3` user tag), the typed-error/capability inventory, the `process_info` item→accessor
  mapping, the spawn-option composition, and the lifecycle semantics (link exit propagation, `kill` under
  `trap_exit`, exit-reason shapes, `erlang:exit/2` propagation, the registry round trip, `demonitor`
  `[info]`/`[flush]`)
- `split_borrow.rs` → the two `Process` split borrows (`heap_and_off_heap_mut` + `Fragments::move_into`;
  `heap_and_mailbox_mut` + `send_fragmented`)
- `process_exit_observer.rs`, `port_monitor_cleanup.rs` → the process-death observer seam and the
  death-side port-monitor cleanup
- `terminate_best_effort.rs` → the best-effort termination cascade (a failing `DistSignalRouter` /
  an unreachable peer heap must not abort the death; the observer and the port-monitor cleanup still
  run)
- `dict_oracle.rs`, `registry_monitors.rs`, `monitor_tag.rs`, `lifecycle_oracle.rs`,
  `port_monitor_cleanup.rs` (oracle tests) →
  `erl`-cross-checked process-dictionary, process-control, lifecycle and port-monitor evidence (loud skip
  when `erl` is absent); their driver is the shared `common/mod.rs`
- `ports.rs`, `remote.rs` → the port table and distributed links/monitors
- `smp.rs`, `lock_scope.rs`, `timers.rs`, `dirty.rs`, `progress.rs` → the SMP pool (see
  `../src/smp/CONTEXT.md`)
