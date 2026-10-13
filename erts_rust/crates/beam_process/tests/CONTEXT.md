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
    collector honestly via `AnyCollector::policy()` (and its `fullsweep_due`/initial-space sizing). This
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
29. `signal_exhaustion.rs` (4 tests) — the **signal builders' exhaustion policy** on the
    *enqueue* side: `copy_reason_onto` (the reason copy a signal's enqueue performs into the
    receiver's own heap) and the shared `grow_receiver_for_signal` treat a full receiver heap
    exactly as `Process::deliver` does. A
    `{'EXIT', From, Reason}` whose copied reason is one word **larger** than the receiver's whole
    bounded nursery is delivered: the refused copy collected once (`minor_gcs` up), grew the nursery on
    demand to `next_heap_size(payload, 0)`, moved the base, and lands whole and exactly once in FIFO
    order ahead of a following delivery (`an_oversized_exit_reason_grows_the_receivers_nursery_and_is_delivered`);
    the same for a monitor `{'DOWN', …}` carrying a large reason
    (`an_oversized_down_reason_grows_the_watchers_nursery_and_is_delivered`). A **relocation-exempt**
    receiver can never take that copy (its heap must not move, so it is never collected or grown), and
    the refusal that is *still* surfaced for one is the exit **policy**: both exemption cases here are
    **non-trapping**, so the exit signal would have to *terminate* them — a termination is the caller's
    cascade rather than a message a fragment could carry — and the raw
    `Err(BeamError::Alloc(AllocKind::Heap | Message))` surfaces out of the **enqueue-time**
    `copy_reason_onto`: neither `minor_gcs()` nor the nursery `base()` moves, nothing is written or
    enqueued
    (`a_relocation_exempt_receiver_surfaces_the_raw_refusal_without_relocating`); and the typed failure
    leaves **no partial enqueue** (the signal is not queued, the mailbox is empty, and the receiver is
    still alive, i.e. the refused signal did not terminate it) with a fitting signal received normally
    afterwards
    (`a_refused_signal_leaves_no_partial_state_and_the_receiver_still_delivers`). **Discriminating by
    construction: the two delivery tests FAIL (a typed `Alloc` out of `terminate`) the moment the
    builders only collect without growing**; the exemption pair pins the no-collect / no-grow /
    nothing-written contract (the refusal is enforced both at the reason copy, which pre-tests
    `is_relocation_exempt`, and in the shared `grow_receiver_for_signal`, which refuses identically).
    A **trapping** exempt receiver's refusal, by contrast, *is* a message and is delivered
    fragment-resident — the enqueue-time route of entry 39. Deterministic — sizes from the public
    heap-policy arithmetic, no sleeps / oracle.
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
34. `message_relocation.rs` (4 tests) — the collection's **queued-message re-install**: a collection
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
35. `off_heap_send_isolation.rs` (2 tests) — the **send-path staging-table isolation**: `Process::deliver`
    stages its copy through a fresh per-send `OffHeapTable`, so a send must NOT drain the receiver's
    **persistent** off-heap table (a queued signal's reason objects / a host's moved-fragment objects). A
    200-byte refc binary pushed onto `process.off_heap_mut()` stays present with the same
    `data_ptr`/refcount after a delivery to that same process, and the queued message owns only what it
    copied (a payload that really references an off-heap binary is still captured). **Both tests FAIL on
    the pre-fix code** (which passed `self.off_heap` as the drained destination) — verified by reverting.
36. `message_from_is_immediate.rs` (6 tests) — every signal path's `Message::from` is an **immediate**
    (the C's `is_immed(ERL_MESSAGE_FROM)` invariant) and survives a collection: a named-monitor DOWN
    (`from == target.to_eterm()`, body still `{Name,Node}`), an ordinary-monitor DOWN, a failed-by-name
    named-monitor DOWN (`from == system`), a remote DOWN and a remote EXIT (`from == node`, body still the
    external pid), and the collection case (a nursery GC then `is_immediate(from)` + `from` unchanged,
    the relocated payload still carrying `{Name,Node}`). **5 of the 6 FAIL on the pre-fix code** (which
    stored an on-heap `from`) — verified by reverting.
37. `collector_dispatch.rs` (4 tests) — the **GC strategy dispatch on the production `Process`
    path**: `Process` owns a `beam_gc::AnyCollector`, so the mature generation a real process collects
    with is the one its heap policy resolves to. (i) `a_runtime_spawned_process_selects_the_strategy_its_policy_implies`
    spawns through the production entry point (`ProcessTable::spawn(policy)`) and asserts
    `collector().kind()` — `MatureKind::Immix` for `initial_mature_words >= beam_gc::IMMIX_BREAK_EVEN_WORDS`
    (with `mature_choice().source == Automatic`), `Copying` one word below and for the default policy;
    **this assertion has no pre-change counterpart** (the field was the concrete `Collector`, so no
    strategy question could be asked of a process). (ii)
    `the_default_process_keeps_the_pre_change_collection_cadence` pins the default's
    `CollectionKind` sequence literally (`[Minor × 4]`, `minor_gcs == 4`, zero majors — the default
    `fullsweep_after = 65_535` never fires) and the growth seam's kind. (iii)
    `the_selected_strategy_collects_through_the_process` plants live x-register/stack/mailbox roots and
    asserts they survive structurally across three `collect()`s + `ensure_capacity` +
    `collect_if_needed` + a later delivery, for **both** a default and an Immix-configured process; and
    `an_immix_process_survives_a_fullsweep_through_the_process` gives the Immix process
    `fullsweep_after = 0` (the rule reads the mature *size*, so the policy still resolves to Immix) and
    drives three real **GenImmix fullsweeps** through `Process::collect` (`CollectionKind::Major`).
    Deterministic — the strategy/kind assertions and the fixed collect counts, no sleeps / wall-clock
    bounds / oracle.
38. `off_heap_binaries.rs` (9 tests) — the **process-owned off-heap refc-binary list and its sweep**
    (`Process::register_off_heap_binary`/`off_heap_binary`/`off_heap_binaries`/`off_heap_binary_count`/
    `off_heap_binary_bytes`/`declare_off_heap_binary_dead`/`sweep_off_heap_binaries`/
    `retain_off_heap_binaries`, over the `beam_gc::OffHeap` the process owns). **(Acceptance 1)**
    `a_live_owned_binary_survives_a_sweep_and_a_collection`: a 200-byte off-heap payload named by a real
    on-heap `[BinRef][ErlSubBits]` pair rooted in `x(0)` (so a genuine reference is relocated) and also
    held by a second counted handle survives both a sweep and a `Process::collect` **byte-intact**
    (`as_bytes()` unchanged, same `data_ptr`) at a consistent refcount (neither over- nor
    under-decremented). **(Acceptance 2)** `a_declared_dead_owned_binary_is_released_exactly_once`: on an
    **injected** `AllocRef` the declared-dead payload's block/byte accounting drops to 0
    (`live_blocks`/`live_bytes`), a second sweep of the same slot releases nothing and the destructor
    never runs again. **(Acceptance 3)** `slot_indices_stay_valid_across_a_sweep`: a surviving entry
    keeps its original slot (intact), the released slot resolves to `None` — never to an unrelated
    shifted-in entry (the list never shifts/reorders/reuses a slot) — and a fresh registration appends at
    a new index. Plus: `a_sweep_never_frees_a_payload_another_owner_still_holds` (the no-over-decrement
    half), `a_collection_drives_the_off_heap_sweep` (a rooted collection reclaims a declared death on its
    own, leaving an undeclared sibling untouched — its kept payload is rooted via a real
    `[BinRef][ErlSubBits]` pair in `x(0)`, so the reachability half keeps it), `registering_a_payload_twice_keeps_one_slot_and_one_reference`
    (the `MSO(p)` one-reference-per-object invariant), `a_dropped_process_releases_the_binaries_it_still_owns`
    (the process-death reclamation endpoint of the conservative fallback), `a_faithful_liveness_predicate_can_drive_the_sweep`
    (the `retain_off_heap_binaries` seam for a party that *can* walk the heap) and
    `a_collection_releases_nothing_that_is_still_reachable` (a collection leaves a still-rooted payload
    alone — the payload is rooted in `x(0)`; the reachability-driven half is in entry 41). **Every assertion
    fails to compile on the pre-change code** — HEAD had no process-owned off-heap list and no sweep on
    the process path, so the registration/read/sweep API does not exist; the released-count and
    slot-resolution assertions have no old-code counterpart (there was no list to release from).
    Deterministic — a fixed position-dependent payload, an injected allocator, explicit
    `collect()`/sweep calls; no sleeps / wall-clock bounds / oracle.

39. `signal_fragment_fallback.rs` (8 tests) — the **heap-fragment fallback of the signal-delivery
    path** at **both** refusal sites for a receiver with **unpublished live roots** — a
    **relocation-exempt** process, or a **`Running`** one (`Process::has_unpublished_roots`): the
    **apply-time** message
    build (`build_signal_message` / the queued `deliver_head_signal`, via `enqueue_fragment_signal`) and
    the **enqueue-time** reason copy (`copy_reason_onto`, via `enqueue_down_signal`'s
    `deliver_down_fragment` / `enqueue_exit_signal`'s `deliver_exit_fragment`). A refused payload is
    built on a 64-word scratch arena and copied **whole** into a fresh `beam_message::HeapFragment` the
    queued `Message` owns (the shared `enqueue_fragment_payload`, which copies through the **source**
    process's off-heap table) rather than being lost — the C's model (`erl_proc_sig_queue.c` builds
    every signal message into the message's own fragment). A **controlled experiment** on a 7-word
    nursery drained to one word short of the four-word head build, each case first asserting that
    refusal and that a refused request writes nothing: (1) an exempt watcher still receives its
    `{'DOWN', Ref, process, Target, noproc}` — exactly once, FIFO behind the fixture's filler, the
    five-tuple structurally equal to the expected one (`compare::eq`, so the boxed reference compares by
    content), `fragments().is_some()` with `fragment.contains(payload)`, while
    `minor_gcs()`/`young().base()`/`heap_used_words()` are **unchanged**; (2) the same for a trapping
    receiver's `{'EXIT', Dying, normal}`; (3)/(4) the same **enqueue-time** route for a reason one word
    larger than the receiver's **whole** nursery, so the reason copy itself is the refused request and
    the signal is never queued (`pending_signals` stays 0, the nursery/mature observables are unchanged)
    while the DOWN / the oversized trapping `{'EXIT', …}` is still delivered whole, fragment-resident,
    with a **blocked** receiver woken by the delivery itself (`drain_woken()` = the receiver — the
    queued form's wake happens in `deliver_head_signal`, which this route bypasses); (5) the
    **non-exempt contrast** — the identical refusal takes the other arm: `minor_gcs()` up by exactly
    one, the nursery grown to `next_heap_size(reserve, 0)` and relocated (`base()` moved), the filler
    promoted to mature, the EXIT built on the **new** nursery (`fragments()` is `None`,
    `contains_term(payload)` young, not mature) — so the fallback did not replace the grow-and-retry
    path. (6) the **message-delivery** half of the gate: `Process::deliver` to a **`Running`** (not
    exempt) receiver surfaces the raw refusal and enqueues nothing, leaving
    `minor_gcs()`/`young().base()`/`heap_used_words()`/mature usage untouched, and the host's fragment
    fallback (`beam_message::send_fragmented` through `Process::heap_and_mailbox_mut`) still delivers
    the message whole and fragment-resident (`compare::eq` against the source term) with the fixture's
    filler keeping its FIFO place; (7) the **schedule-in** half, the realistic `Running` sequence — a
    queued DOWN with the immediate `noproc` reason (so the enqueue-time copy allocates nothing and the
    signal *is* queued, deferred because the receiver is the executing process, `pending_signals` = 1)
    is refused at the **apply-time** build at `service_signals`, so the DOWN lands fragment-resident
    (structurally equal to the expected five-tuple) with the nursery untouched and the queue drained;
    (8) the **control** — the identical refusal on the identical receiver in `ProcessState::Waiting`
    takes the collect-and-grow arm (one collection, the nursery grown and relocated, the filler
    promoted to mature, the DOWN built on the receiver's **new** nursery with no fragment, and — unlike
    the running case — the signal serviced **eagerly**, `drain_woken()` = the receiver) — pinning that
    the gate is the *running* state, not a blanket refusal.
    **Discriminating by construction: against the pre-fix commit `360a4c6536`, cases 1 and 2 FAIL
    with the raw `Alloc(AllocFailure { requested: 32, kind: Heap })` out of `terminate` (signal lost)
    while case 5 passes; before the enqueue-time route, cases 3 and 4 FAIL (the drop is logged and no
    message arrives).** An exit signal to a **non-trapping** exempt receiver is *not* covered by the
    fallback (it would have to terminate the receiver) and stays a surfaced refusal — entry 29.
    Deterministic — sizes from the public heap-policy arithmetic, no sleeps / wall-clock bounds /
    oracle.

40. `decline_gate.rs` (4 tests) — the **host collection gate** (`CollectionGate`) of `ProcessTable`.
    A host-installed `declines_relocation` predicate is consulted first at every signal site (OR-ed
    with the crate-owned `Process::has_unpublished_roots`) and at `ProcessTable::deliver`, and it
    declines only the **relocation** of a receiver's heap, so the notification is still delivered
    through the same heap-fragment route the `Running`/relocation-exempt receiver uses. The cases:
    (1) `a_host_gate_declines_an_oversized_link_exit_into_a_heap_fragment` — a **trapping**,
    **non-exempt**, **non-`Running`** receiver linked to a dying peer with a reason larger than its
    whole nursery: the enqueue-time reason copy is refused, the signal never reaches the queue, and
    the `{'EXIT', …}` is delivered fragment-resident with the nursery neither collected nor moved
    (`minor_gcs`, the young space's base/used words and the mature used words all unchanged) and the
    blocked receiver woken; (2) `a_host_gate_declines_an_oversized_monitor_down_into_a_heap_fragment`
    — the same for a **watcher**'s oversized `{'DOWN', …}`; (3)
    `a_host_gate_declines_the_delivery_collect_and_the_flag_is_restored` — the **message-delivery**
    half: `ProcessTable::deliver` surfaces the copy engine's **raw** `AllocKind::Heap`/
    `AllocKind::Message` refusal with the nursery untouched (the gate marks the receiver
    relocation-exempt for exactly one `Process::deliver`, and the prior exemption-flag value is
    restored afterwards); (4) `without_the_gate_the_same_signals_collect_and_grow` — the **control**:
    with no gate installed the identical refusal takes the ordinary arm (one collection, an on-demand
    nursery growth and a relocation), so the gate is an *additional* decline, not a replacement.
    Deterministic — every size from the public heap-policy arithmetic, every "did nothing" claim
    checked against the receiver's own nursery observables (`minor_gcs`, the young space's
    base/used words, the mature generation's used words); no sleeps / wall-clock bounds / oracle.

41. `reference_ids.rs` (4 tests) — **PIN — HAZARD 2, the two reference-id spaces colliding on `=:=`**
    (a documented divergence, **not closable from this crate**): the crate's [`RefAllocator`] is
    monotone from `0`, so two independent instances hand out the same ids
    (`two_independent_reference_allocators_start_at_the_same_zero`); a `ProcessTable`-minted
    `erlang:alias/1` reference and an **independent** allocator's (`beam_rt::Runtime::refs`'s shape)
    `make_ref/0` reference carry the same id and are the **byte-identical** four-word block
    (`the_process_table_and_an_independent_allocator_mint_the_same_reference_for_the_same_id`); the
    runtime's `beam_term::boxed::write_internal_ref` block (`beam_bifs`'s `make_ref_thing` shape) is
    the same term as a `MonitorRef::to_eterm` block with the same id
    (`a_runtime_minted_bare_reference_and_a_monitor_reference_with_equal_ids_are_exactly_equal`);
    and the comparison mechanism — `beam_term::compare::cmp_refs` keys on the payload words
    (`[arity, node, serial, id]`), and both producers pin `node`/`serial` to `0`, so the shared id is
    the only distinguishing word
    (`the_reference_payload_is_compared_word_for_word_leaving_only_the_id`, which also shows a block
    differing only in `node` **would** be distinguished). The module docs state the exact reason it
    cannot be closed here (the id-only `beam_codec` internal-reference codec; the two independent
    allocators; `beam_rt`'s monotone-from-0 dependency) and name the owner (a cross-crate change:
    extend the ref codec to preserve the whole payload, or unify the allocators via `beam_rt`). No
    oracle cross-check — the oracle has **one** id space, so it cannot produce the colliding pair.
    Deterministic — no sleeps / wall-clock bounds / oracle.
42. `off_heap_reachability.rs` (9 tests) — the **reachability-driven** liveness decision for the
    **process-owned** off-heap refc binaries (`Process::sweep_off_heap_binaries_reachable`, over
    `beam_gc::AnyCollector::for_each_reachable_object` + the read-only `gather_scan_roots` root set;
    `Process::run_rooted_collection` runs it automatically over the roots the collection just
    gathered). `a_payload_reachable_through_an_on_heap_handle_is_retained`: a 200-byte payload named
    by a real on-heap `[BinRef][ErlSubBits]` pair rooted in `x(0)` is kept — the sweep returns 0, the
    slot resolves with the same `data_ptr`/bytes/refcount, and the injected `AllocRef`'s `live_blocks`
    stays 1. `an_unreachable_payload_is_reclaimed`: a payload whose registration moved the **sole**
    handle into the process and that no on-heap `BinRef` names is freed at reference count 0 (the
    injected allocator's `live_blocks`/`live_bytes` drop to 0), its slot answers `None`, and a second
    sweep releases nothing (no double free).
    `a_payload_reached_only_through_a_nested_container_is_retained`: the walk descends the container
    shapes — a payload reached only through a **tuple**, a **list** cell, or a **flat map** value
    (`write_map`) survives, while a fourth payload registered but rooted nowhere is reclaimed in the
    same sweep. `a_payload_reached_through_any_root_region_is_retained`: a payload reached only through
    one of the process's **in-heap sub-rootsets** — a queued `Message` payload, a dict value, a saved
    BIF/NIF arg, or a `PendingSignal` reason — is kept (so the scan-side `gather_scan_roots` cannot lag
    the collection's own gather), and only the unrooted one is released.
    `a_process_with_unpublished_roots_is_not_reachability_swept`: a **relocation-exempt** process and a
    `Running` one each return `Ok(0)` and retain every owned payload (the **decline valve** — an
    incomplete root set could miss a live `BinRef` and dangle); `a_declined_collection_still_honours_a_declared_death`:
    the conservative declared-dead sweep still fires for such a process.
    `the_reachability_sweep_keeps_dedup_and_slot_stability`: the `MSO(p)` one-reference-per-payload
    dedup and slot stability hold across a reachability sweep;
    `a_declared_death_is_honoured_over_the_reachability_verdict`: a caller's `declare_off_heap_binary_dead`
    wins even when the handle is still on the heap (the conservative half runs after the reachability
    half); `a_collection_that_relocates_the_handle_keeps_the_payload`: a `Process::collect` that
    relocates the heap (`handle_before != handle_after`) keeps the payload at the same `data_ptr`.
    Deterministic — a fixed 200-byte position-dependent payload (> `ERL_ONHEAP_BINARY_LIMIT`), an
    **injected** `AllocRef` so a payload block's alloc/free is observable in isolation, and explicit
    `sweep`/`collect` calls; no sleeps / wall-clock bounds / oracle.
43. `decline_gate_builders.rs` (13 tests) — the **collection-decline path of the six
    `build_signal_message`-only signal builders**, the gap the crate `CONTEXT.md`'s Known Issues
    recorded. Each builder is exercised through its public trigger with a saturated nursery (three
    free words — one short of the four-word first allocation every one of the six builds makes) under
    **both** valves, then asserted to deliver the notification **fragment-resident** with the
    receiver's nursery untouched (`minor_gcs`, the young space's `base`/`used_words` and the mature
    used words all unchanged): (1) `deliver_noproc_down` — a born-down `monitor(process, P)` on a
    dead/never-existing pid, `{'DOWN', Ref, process, P, noproc}` (`envelope from` = the dead pid);
    (2) `deliver_named_noproc_down` — `monitor_named(process, Name, Node)` on an unregistered name,
    `{'DOWN', Ref, process, {Name, Node}, noproc}` (`from` = the `system` atom, the `{Name,Node}`
    target materialised inside the fragment's build); (3) `enqueue_remote_down_message` —
    `node_down` for a recorded remote monitor, `{'DOWN', Ref, process, RemotePid, Reason}` (`from` =
    the node atom); (4) `enqueue_remote_exit_message` — `node_down` for a **trapping** remote link,
    `{'EXIT', RemotePid, Reason}`; (5) `enqueue_port_down_message` — `port_closed` for a port
    monitor, `{'DOWN', Ref, port, Port, Reason}` (`from` = the port); (6)
    `enqueue_port_exit_message` — `port_closed` for a **trapping** port link, `{'EXIT', Port,
    Reason}`. Each of the six gets one case for the **crate-owned** valve (a **relocation-exempt**
    receiver) and one for the **host** `CollectionGate` (with the crate-owned predicate asserted
    false, so only the host could have declined), every case also asserting the delivered term's
    whole shape against the expected tuple (`compare::eq`) and FIFO behind the fixture's filler. The
    thirteenth test `without_a_decline_each_builder_collects_and_grows` is the **shared control**:
    the identical trigger with no gate and no exemption takes the ordinary arm for all six builders
    (one collection, an on-heap message, no fragment), so the fragment route is an *additional*
    decline, never a replacement. Deterministic — no sleeps / wall-clock bounds / oracle; each test
    was verified sensitive to the guard (dropping a builder's `decline` argument flips its gate case
    to "a declined relocation must not collect").

## Constraints
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
  `../../../tools/differential/` harness, not here. Current: **511 passed, 0 failed, 1 ignored** (179 in-module unit tests + 331
  across the 43 integration targets here + 1 doctest; the single ignore is the documented
  non-yielding-runner hang failure mode in `preemption.rs`).

## Routing Table
- `scheduler.rs`, `messaging.rs`, `signals.rs`, `gc.rs`, `priority.rs`, `pending_signals.rs`,
  `preemption.rs`, `dict.rs`, `fragments.rs`, `scheduler_ports.rs`, `exhaustion.rs`,
  `signal_exhaustion.rs`, `signal_fragment_fallback.rs`, `decline_gate.rs`, `decline_gate_builders.rs`, `gc_exemption.rs`, `message_relocation.rs`,
  `gc_refc_binary_message.rs`,
  `off_heap_send_isolation.rs`, `message_from_is_immediate.rs`, `off_heap_binaries.rs`,
  `off_heap_reachability.rs` →
  the single-threaded scheduler (incl. the port-aware slice cleanup), mailbox, signals, GC roots, PCB state,
  pre-emption/fairness, the collect-grow-and-retry exhaustion path of `Process::deliver` and the
  `{'EXIT',…}`/`{'DOWN',…}` signal builders (both the enqueue-time reason copy and the
  heap-fragment fallback for a receiver whose relocation is declined — the crate-owned unpublished
  roots or the host `CollectionGate` — at either refusal site), the host-controllable GC relocation
  exemption, the
  collection's queued-message re-install (a fragment-resident/off-heap-owning message keeps its whole
  envelope across a collect), and the **process-owned off-heap refc-binary list** (registration, slot
  stability, the conservative declared-dead sweep on the collection path, and the
  **reachability-driven** sweep — its root set, its `HEADER_BIN_REF` payload-pointer test, the
  nested-container descent, and its unpublished-roots decline valve)
- `registry_monitors.rs`, `monitor_tag.rs`, `capability_inventory.rs`, `process_info_source.rs`,
  `spawn_options.rs`, `collector_dispatch.rs`, `lifecycle_oracle.rs` → the process-control surface: name registry + monitors (incl.
  the `monitor/3` user tag), the typed-error/capability inventory, the `process_info` item→accessor
  mapping, the spawn-option composition and the GC **strategy dispatch** (the strategy a process's
  policy resolves to, the unchanged default cadence, and collection through the process root set), and
  the lifecycle semantics (link exit propagation, `kill` under
  `trap_exit`, exit-reason shapes, `erlang:exit/2` propagation, the registry round trip, `demonitor`
  `[info]`/`[flush]`)
- `split_borrow.rs` → the two `Process` split borrows (`heap_and_off_heap_mut` + `Fragments::move_into`;
  `heap_and_mailbox_mut` + `send_fragmented`)
- `process_exit_observer.rs`, `port_monitor_cleanup.rs` → the process-death observer seam and the
  death-side port-monitor cleanup
- `terminate_best_effort.rs` → the best-effort termination cascade (a failing `DistSignalRouter` /
  an unreachable peer heap must not abort the death; the observer and the port-monitor cleanup still
  run)
- `reference_ids.rs` → the **HAZARD-2 pin**: the two independent reference-id spaces
  (`ProcessTable`'s `RefAllocator` vs `beam_rt::Runtime::refs`) collide on `=:=` — a documented
  divergence, not closable from this crate (the owner and the blocked avenues are named in the
  file's module docs)
- `dict_oracle.rs`, `registry_monitors.rs`, `monitor_tag.rs`, `lifecycle_oracle.rs`,
  `port_monitor_cleanup.rs` (oracle tests) →
  `erl`-cross-checked process-dictionary, process-control, lifecycle and port-monitor evidence (loud skip
  when `erl` is absent); their driver is the shared `common/mod.rs`
- `ports.rs`, `remote.rs` → the port table and distributed links/monitors
- `smp.rs`, `lock_scope.rs`, `timers.rs`, `dirty.rs`, `progress.rs` → the SMP pool (see
  `../src/smp/CONTEXT.md`)
