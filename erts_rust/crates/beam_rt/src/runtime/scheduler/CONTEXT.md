# `beam_rt::runtime::scheduler` — the scheduler-pass family

## Intent
`../scheduler.rs` (with `./tests.rs`) is the **scheduler-pass family**: the inherent `Runtime` methods driving processes → ports → timers → distribution in bounded steps, and the one shared body `scheduler_pass` they all funnel through.
`scheduler_pass(max_slices, resume_first, wait) -> Result<(u64, usize)>` boots the runtime once (`boot_if_needed`, its first statement) then runs zero or more **rounds**.
Before its first round it moves three tables **out** — the process table (`take_processes` swaps in a fresh empty `ProcessTable`), the `Scheduler` (`Scheduler::new()`) and the `RunnerState` (`mem::take`) — because the runner must hold `&mut Runtime` while the scheduler holds `&mut ProcessTable`; all three return **unconditionally** before `outcome?`.
The distribution half is **four** steps: the accepting node's `pump_dist`/`dispatch_dist_inbound` and the connector's `pump_dist_client`/`dispatch_dist_client_inbound`.

## The entry points and their wait policies
`TimerWait` has **five** variants (`Never`, `Deadline`, `AtMost(_)`, `AtMostWhenIdle(_)`, `ImminentWithin(_)`); `TimerWait::allows(waits)` answers `Never => false`, `Deadline => true`, the rest `waits == 0`, so **every bounded pass sleeps at most once** and cannot spin.
Constants: `TIMER_PUMP_SLICE = 100 ms`, `RUN_DEADLINE = 60 s`, `RUN_STEP_SLICES = 1_000`.
| Entry | Policy | Caller |
|---|---|---|
| `run_one_slice()` → `run_slices(1)` → `run_pass(Some(1))` | `Never` | the stepping/observation hook |
| `run_slices(max)`, `run_scheduler()` (`run_pass(None)`) | `Never`, `Deadline` | drive N slices / run out |
| `pump_scheduler(max)` | `AtMost(100 ms)` | the `-eval` bare `receive`, `port/busy_wait.rs`, `context/pump.rs`'s port half |
| `pump_scheduler_now(max)` | `ImminentWithin(ZERO)` | the `-eval` `receive … after T` loop |
| `pump_scheduler_boot(max)` | `ImminentWithin(100 ms)` | the boot drives (`../../boot/drive.rs`) |
| `wait_for_timer(until, max)` | `ImminentWithin(until - now)` | the `-eval` `receive … after T` wait |
| `run_scheduled(…)` → `pump_run_to_completion` | `AtMostWhenIdle(25 ms)` | the CLI `run` action |
The four pump/`wait_for_timer` entries pass `resume_first = true` (absorb the caller's pre-pass wake list through `Scheduler::resume_woken`); `run_pass` passes `false`.
- **`pump_scheduler`** is the **blocking** bounded step (answers `ran + delivered`): its timer half sleeps `min(to the nearest deadline, 100 ms)` and its port half blocks one `PORT_POLL_SLICE` on a zero-slice round.
  Its callers are the `-eval` **bare** `receive`, the busy-port wait (`BUSY_PORT_PUMP_SLICES = 64`) and the nested cross-module pump's port half (`RUN_EXPORT_PUMP_SLICES`).
- **`pump_scheduler_now`** has a **zero** window, so it never sleeps while a due deadline still fires (`fire_due_timers` precedes `timer_wait`); `ImminentWithin` also zeroes the port half when no port owes a reply, and whatever must block stays with the caller.
- **`pump_scheduler_boot`** sleeps the exact remaining time when the earliest deadline is **within** the 100 ms window and returns **without sleeping** otherwise; `BEAM_RT_BOOT_TRACE` logs each decision (`timer wait: policy=… remaining=… wait=…`).
- **`wait_for_timer`** sleeps to `min(earliest pending deadline, until)` then runs one pass under `ImminentWithin(until - now)`, so no pass it starts outlives the caller's window; it answers whether anything ran or was delivered (`false` = the window was slept out).

## Quiescence (`is_quiescent`)
Four required terms; when all hold no process can run, no port can deliver and no timer can fire, so the honest answer for a blocked process is `beam_engine::blocked()`:
1. an **empty run queue**;
2. `PortRegistry::has_nothing_to_poll()` — no live port, no armed `tty_select/2` interest and no **fired, undrained** signal self-pipe; an armed-but-idle `os:set_signal/2` self-pipe is readable only when a signal arrives, so it is not work (every action but `-version` boots a script that arms one), and a table that cannot be borrowed counts as **not** quiescent (`Err ⇒ false`);
3. the **timer** term — `timers.next_deadline().is_none()` or, on a `run`-route pass (`run_wake_pid`), only a deadline that can wake that one target (`TimerWakes::can_wake`), so a housekeeping deadline never keeps the pass alive;
4. the **distribution** term — `DistNode::is_idle` (no live **link**: the listener and a pending accept are not work) and neither `DistClient::has_links` (a **borrowed** cell counts as "has links") nor `DistClient::has_pending_outbound` (a queued routed signal, or bytes a link's driver has not written).
A `run`-route process parked on a bare `receive` **with a live link** therefore keeps pumping, bounded by `RUN_DEADLINE`'s "did not terminate" rather than exiting 3; a runtime with no node name carries neither half (`None`) and a node with no link is idle.

## `run_scheduled` (the CLI `run` action)
`run_scheduled(path, function, arity, args)` boots, loads the file through the code server, checks the arity, spawns the export, records the target in `run_wake_pid` for the whole drive and pumps to completion.
`pump_run_to_completion` drives one target at `RUN_STEP_SLICES = 1_000` slices per pass with `DEFAULT_REDUCTIONS_PER_SLICE = CONTEXT_REDS = 4_000` (`beam_process`) under `AtMostWhenIdle(25 ms)`, checking in order: `process_result` → `Ok(x(0))`; `take_process_error` → the **original** `BeamError`; `process_exit` → the reason term rendered by `format_term_w` as a raise (the `max_heap_size` kill drain neither returns nor raises, and this arm keeps it from surfacing as `blocked`); `RUN_DEADLINE`.
`ran == 0 && delivered == 0 && is_quiescent()` ends it with the typed `beam_engine::blocked()` instead of spinning to the deadline, and the exit-code classification is the single-pass path's (`../../cli.rs`).

## The round's per-step accounting
Prologue: `boot_if_needed` · `take_processes` · swap the scheduler · `mem::take` the `RunnerState` · one `pass start: …` debug line · the caller's wake list when `resume_first` · `absorb_fragment_wakes` · seed `ran`/`delivered`/`waits`.
1. derive the budget `max_slices - ran`; `Some(0)` **ends the pass** at the top.
2. `scheduler.run_ready(&mut table, &mut runner, budget)` — slices become `ran`/`round_slices`.
3. `note_context_switches(executed)`.
4. derive `progressed` and `port_timeout`.
5. `prune_gc_requests(&table)` — a request whose process died is dropped.
6. `state.prune_dead(…)` — cancel each dead pid's receive timeout.
7. `deliver_gc_completions(&mut table)` — the `{async, ReqId}` half of `erlang:garbage_collect/2`.
8. `service_file_requests(&mut table)` — `file_server_2` and its io-devices.
9. `service_net_kernel(&mut table)` — the registered `net_kernel` service.
10. `drain_port_monitor_exits()` — the dead watchers' `(port, reference)` pairs.
11. `drain_ets_owner_exits(&mut table, &mut scheduler)` — the ETS owner-death obligation.
12. `absorb_pending_spawns(…)` — a spawn from the slices.
13. `absorb_fragment_wakes(…)` — yields the round's `woke`.
14. `drain_driver_monitor_exits(&table)` — mark each `driver_monitor_process` target the table lost, so this round's port tasks fire `process_exit`.
15. `deliver_port_messages(&mut table, port_timeout)` — the **port half**.
16. `drain_nif_thread_sends(Some(&mut table))` — the **foreign-thread NIF half**.
17. `pump_dist()` — the accepting handshake/accept step + `sync_dist_creation`.
18. `bind_dist_peers()` — bind each freshly established accepted link to its node atom term.
19. `service_node_monitors(&mut table)` — `erlang:monitor_node/2`'s `{nodeup, Node}`/`{nodedown, Node}`.
20. `dispatch_dist_inbound(&mut table)` — route what the accepted links decoded.
21. `pump_dist_client()` — the connector's outbound step, flushing the controls the process layer queued.
22. `dispatch_dist_client_inbound(&mut table)` — route what the outbound links decoded.
23. `service_lost_dist_peers(&mut table)` — the peer-loss half.
24. `fire_due_timers(&mut table)` — the **timer half**, with `absorb_timer_wakes` **inside** this arm.
25. a round that absorbed a fragment wake (`woke > 0`) continues.
26. `timer_wait(wait, waits, progressed)` — `Some(d)` → `waits += 1`, sleep `d`, continue; `None` **ends the pass**.
Steps 7, 8, 9, 11, 15, 16, 19, 20, 22, 23 and 24 count into `delivered` (with `resume_woken` on 7, 8, 9, 15, 16 and 24) and continue; 12 continues when it absorbed any spawn, **uncounted**; 17 and 21 only tally into a total and fall through; 10, 14 and 18 may break the pass on `Err`.
Epilogue: a second `state.prune_dead(...)`, then the three tables are restored and only then is the outcome propagated; a step's `Err` is the pass's `Err`, never swallowed (the two monitor drains, 10 and 14, break and propagate it).
Order invariants: the foreign-thread NIF step sits **right after** the port step and **before** `pump_dist`; `service_node_monitors` runs **after** `bind_dist_peers`, whose binding its transition needs; `service_lost_dist_peers` sits **after both** inbound drains and **before** the timer half, so a dying link becomes `ProcessTable::node_down` before the round can end on a timer; `drain_driver_monitor_exits` sits **immediately before** `deliver_port_messages`, so the same round's port-task probe fires each driver's `process_exit`; `fire_due_timers` precedes `timer_wait`, so an already-passed deadline fires on the round that then declines to sleep.

## `delivered` does not count ordinary `send`
Only **port/timer/file-service/ETS/distribution** deliveries are counted, so a pass whose only progress is a plain message wake can look like `delivered == 0`; the stall test is therefore `ran == 0 && delivered == 0 && is_quiescent()`, and the fragment wake count `woke` is a separate progress term.

## The progress-aware wait (both halves)
`progressed` is `round_slices > 0 || !self.fragment_wakes.is_empty()`, and **both** halves read it.
The fragment term is load-bearing: a delivery that did not fit the receiver's heap takes `beam_message::send_fragmented`, which owns its own wake channel (`Runtime::note_fragment_wake`) instead of the table's wake list and is absorbed **later in the same round**, so without it a fragment-only round looks idle, sleeps a full poll slice and inflates the loop's wall time.
- **Timer half.** `timer_wait` answers `None` when `!policy.allows(waits)`, and `None` when a `run`-route pid is set and `TimerWakes::can_wake` refuses it; else `remaining = timers.next_deadline()? - now` maps through the policy (`Never => None`, `Deadline => Some(remaining)`, `AtMost(max) => Some(min)`, `AtMostWhenIdle(max) => (!progressed).then(...)`, `ImminentWithin(w) => (remaining <= w).then_some(remaining)`), so a round that ran **at least one slice** never sleeps even with its budget exhausted and the run queue still full.
- **Port half.** `deliver_port_messages` gets `PORT_POLL_SLICE` only for a round that ran **nothing** and `Duration::ZERO` for a progressed one — it still polls (a driver may have bytes queued), it just does not block.
  Under `ImminentWithin(_)` the timeout is also zeroed when neither `awaits_port_reply` (some port with `output > input`) nor `ports_awaiting_async` holds, keeping the boot's live-but-idle `inet_gethost` child off the poll budget; the gate is deliberately **not** widened for an OS child that has not spoken yet, because that patient poll is what a later unsolicited write needs.
Measured (retired instructions and `strace`, shared host): `send_recv:plain/1` ping-pong at N = 20 000 issues **0** `clock_nanosleep` and 6 blocking waits (≈137 ms, the boot's own idle rounds) on both tiers; `file_io/ram_file` sat at a fixed ≈12.9 ms/op without the fragment term; `strace -f -e trace=epoll_wait` over 100 `run`-route ops shows **1** `epoll_wait(…, 25)` against **49** without it, and the same ping-pong takes 1 063 waits / 26.9 s unless the port half is progress-aware; an idle pass is unaffected (bare boot: 0 timing-out polls, a `receive … after 500` still blocks).
Both halves are pinned structurally, never by wall clock, through `#[cfg(test)]` per-thread counters (`PASS_SLEEPS`/`pass_sleeps`/`reset_pass_sleeps`, `BLOCKING_PORT_ROUNDS`/`blocking_port_rounds`/`reset_blocking_port_rounds`) by `the_run_route_never_sleeps_a_poll_slice_while_it_still_has_slices_to_run` (`pass_sleeps() == 0`), `the_run_route_still_sleeps_to_a_parked_receive_timeout` (≥ 1), `the_idle_only_wait_policy_skips_the_wait_when_the_round_ran_slices` and `the_port_poll_blocks_only_for_a_round_that_ran_nothing`, and end to end by `../../tests/run_route_sleep.rs`.
Caveat: `tools/bench`'s `copy_bound` `send_recv_plain` ratio does not reflect this wait (owner: `tools/bench`).

## Port-aware scheduling (A.4) — design decision & handoff
The pass runs the **port-less** `Scheduler::run_ready`; the port half owns all polling, and the port-aware slice's obligations are discharged **at the pass boundary**, where the registry is free.
**Covered:** a dead process's outgoing `(PortId, MonitorRef)` `PortWatch` pairs are swept by `Runtime::drain_port_monitor_exits` (`../ets_owner.rs`), and `Runtime::drain_driver_monitor_exits(&mut self, &ProcessTable)` (`../../port.rs`) runs immediately before `deliver_port_messages`, sweeping every live C-driver port's monitor tree (`PortRegistry::note_driver_monitor_exits`) and marking each watched pid the table lost so the same round's port-task probe fires `process_exit` — a scan of the live table, not a second `set_process_exit_observer` (the slot stays `EtsExitObserver`), deferring with `Ok(0)` on a live borrow.
**Not covered:** `ProcessTable::port_closed` (a *closing* port notifying its monitors/links) can only run **at the close site, while the id is still in the table**, since `PortRegistry::close` releases the id and `PortTable::close` is `BadArg` afterwards; that site is `../../port.rs`'s close path (`RuntimePorts` ← `PortHook::close_port`), outside this node.
`Runtime::with_port_table(&self, &mut ProcessTable, f)` is the intended **short-borrow seam**: it `Rc::clone`s the port field so both tables are borrowable at once, and a nested borrow stays the typed `unsupported: the port table is already in use`, never a panic.
**Two routes are unsound and must not be retried:** holding the `Rc<RefCell<PortRegistry>>` borrow across `run_ready_with_ports` (in-slice port BIFs `try_borrow_mut` the same cell, so every one degrades to the typed `unsupported` gap), and `mem::replace`-ing the `PortTable` out (no `Clone`, and `PortRegistry::open`/`register` insert into the table they were given, so a nested `open_port` would overwrite a live port — an empty placeholder does not free number 0).
**The crate change that would make the literal routing sound:** `beam_io::PortRegistry` exposing `ids` as its own `Rc<RefCell<PortTable>>`, and/or its close path driving `ProcessTable::port_closed` **before** `ids.release(id)`; the `beam_process::PortTable` seam itself suffices.

## Process termination — where a scheduled process is reclaimed
A normally-returning scheduled process is reclaimed by **`beam_process`'s schedule-out**, not by any `beam_rt` code: `RuntimeRunner::run`'s `Returned(value)` arm records `x(0)` and returns `RunOutcome::Exit(normal)`, and `Scheduler::run_slice_inner` dispatches to `slice_end`/`slice_end_with_ports`, whose `Exit` arm calls `ProcessTable::terminate`/`terminate_with_ports`.
A **never-enqueued** `Runnable` process (the `-eval`/`-s`/`-run` action child) is ended by its own teardown: `../runtime.rs::as_action_process` copies an `Ok` result onto the pointer-stable store (`stabilize_term_with` over `source_payloads`, as the runner does for a scheduled `x(0)`) and then calls `ProcessTable::terminate` with `normal` — the `None`-ports branch, since this pass runs the port-less `run_ready`.
It must be `terminate`, not a silent `ProcessTable::remove`, which would skip the exit observer's ETS-owner drain and the name/link/monitor teardown; the result copy is what keeps the returned `Eterm` from dangling.
`ProcessTable::spawn` installs a **naked** process and `spawn_process_with_policy` adds only a group-leader entry, so nothing binds the action child to `init` (the root process does **not** trap exits).
**No path in `beam_rt` closes a dying process's *owned* ports:** `terminate_into` drains links, monitors and port *monitors*, never an owned port, where the oracle closes them with reason `normal`; closing one pid's ports needs an enumeration `beam_process::PortTable` lacks — `owners: HashMap<u64, ProcessId>` with a forward `owner(id)` but **no reverse index and no allocated-id iterator** — so a caller adds the reverse index, then the runtime close path plus `ProcessTable::port_closed` per port.

## Re-entrancy — a nested pass cannot run the run queue
Because the tables are extracted and restored unconditionally, a pass invoked *from inside a slice* would drive the placeholders and have its writes silently discarded by the outer restore.
The guard is **cooperative only**: `RuntimeContext::may_pump_scheduler` (`false` in `for_process*`) is checked at **exactly one** site, `pump_once`'s first line (`../../context/pump.rs`), where it yields the typed `blocked()`; no `in_pass` flag, debug assertion or `RefCell` exists, so a nested pass would corrupt silently rather than trip anything.
The only borrow-level brakes are the registry's `try_borrow_mut` and `is_quiescent`'s `Err ⇒ not quiescent`, and `&mut Runtime` is **owned** by the `RuntimeContext` for the whole slice, so `self.runtime.pump_scheduler(..)` from inside `call_bif` does not even typecheck; a mid-slice caller reaches the pass's table only through `RuntimeContext::mailbox_table`, and the executing pid is out of `RunnerState::engines` with its `Process` `Running`, so every delivery into it takes the fragment route and no collection may move its heap.
**Every service step IS separately callable** (`service_file_requests`, `deliver_port_messages`, `fire_due_timers`, `deliver_gc_completions`, `service_net_kernel`, `service_node_monitors`, `pump_dist`/`pump_dist_client`, plus the non-delivering drains), each taking `&mut self` and at most `&mut ProcessTable`; what a service-only nested pass lacks is a driver sequencing them (that sequencing lives inline in the pass's `loop`) and the wake-list bookkeeping, which belongs to the outer `Scheduler`.

## Outbound distribution bytes: the three write moments
**(a) Synchronously inside the call** — `dist_send_reg_send`/`dist_send_message_pid`/`dist_send_alias` (`../runtime.rs`) and anything reaching `DistLinks::route_control`/`route_message`/`route_alias` write on the spot (`DistConnection::send_control` → `flush_output`).
**(b) Queued, flushed by a pump** — a signal the process layer routes (`DistSignalRouter::route`) only materialises onto the client's scratch heap and pushes onto `DistClientInner::pending`; `flush_pending` writes it, and its **only** caller is `DistClient::pump` (`pump_dist_client`, and `Runtime::dist_connect`'s handshake loop).
**(c) Reads only** — `DistNode::pump` drives the accepted links' handshake and reads and writes no queued control.
So a signal queued in a slice is normally written later in the same pass, but a round that `continue`s at any step, or a pass that ends first, defers it to the next (no drain at the pass tail, none before a `break`); quiescence sees both the link and what the wire is owed, so a `pending` control keeps the pass alive until the flush — and a link retired meanwhile makes the flush drop the item with a recorded `last_error` (`../../dist_client/CONTEXT.md`).

## Distribution (A.5) — inbound validation
Both production drives (`dispatch_dist_inbound` → `DistNode::dispatch_inbound`, `dispatch_dist_client_inbound` → `DistClient::dispatch_inbound`) drain each link through `beam_dist::ConnectionDriver::dispatch_inbound_checked`, so every control message is validated against **that link's own** `peer_name()`/`our_name()` (`LinkIdentity`) before it can reach a mailbox.
`Delivery::classify_validated` reproduces the C's per-opcode `invalid_message` shape checks (`dist.c`) plus the anti-spoof check `dep != external_pid_dist_entry(from)`, with the identity interned into the **same** `AtomTable` the message was decoded against, so the node comparison is a raw term `==`.
A failure is an `Err` — a link-level failure that never reaches `beam_dist::InboundSink`: the drive retires **just that link** and records `DistNode::last_link_error`, never a pass failure.
The check cannot live in `LocalDeliverySink`, built once per pass and shared by every link, which has no peer to compare a `From` against; `Runtime::dispatch_inbound` (the explicit single-driver seam) stays on the *classify-only* `dispatch_inbound`, its caller supplying a hand-built driver whose handshake need not name this node.
The drives are gated by `Runtime::inbound_delivery` (default **ON**) and early-return unless the runtime **is** a distribution node **and** a half has an established link, so a `nonode@nohost` run pays one bool test and allocates nothing.
**Envelope rule:** the mailbox `from` is always an **immediate** — the sender peer's node atom when the wire carried a sender pid, else the word the peer put on the wire (`''` for the plain `DOP_SEND*` forms) — because `beam_message` gathers only each queued message's payload as a GC root.
There is **no** `{setup,…}`/`{nodeup,…}` control exchange: after the final `$a` handshake ACK the link carries `DOP_*` immediately and node-up is local bookkeeping (`net_kernel:mark_nodeup`).

## The inbound family table
`LocalDeliverySink` (`../dist.rs`) routes the message families plus the **route-free receipt** families — each installs or drops the local record and then follows the C's `break; /* Done */` (`dist.c:2258`) without echoing the opcode — and answers the typed `DeliveryOutcome::Unsupported` for the rest, with the reason stated at the arm.
| Inbound family (C arm) | Receipt API, or why the arm is unrouted |
|---|---|
| `Link` | `record_remote_link(to, remote)` — record-only, never echoed |
| `Exit`/`Exit2`/`PayloadExit`/`PayloadExit2` | `apply_remote_exit_signal(to, from, reason, untrappable)`, `untrappable = true` for the `Exit2`/`PayloadExit2` family |
| `UnlinkId`/`UnlinkIdAck` | **partial** — `unlink_from_remote`/`unlink_ack_from_remote`; an `id != 0` `DOP_UNLINK_ID` owes an ACK this sink cannot emit, so the record is dropped and the wire half is `Unsupported`; an `id == 0` (or non-uint-id) one is a **wire error** refused by `classify_validated` before any sink, retiring just that link |
| `Monitor`/`Demonitor` | `monitor_from_remote_ref(watched, watcher, reference, kind)` / `demonitor_from_remote_ref(watched, reference)`, keyed by the **whole** peer-minted wire reference (`RemoteWatcherRef::Remote`, all-immediate, so two peer references never collapse) |
| `MonitorExit`/`PayloadMonitorExit` | `remote_monitor_down(watcher, watched, reference, reason)` for a DOWN whose reference *this* node minted; a name-targeted DOWN or a foreign reference stays `Unsupported` |
| `GroupLeader` | `set_group_leader_from_remote(to, from)` |
| `SpawnRequest`/`SpawnReply` | **gap** — the `ProcessTable::{reserve_remote, reserve_identity, spawn_reserved, cancel_reservation}` surface exists; missing is the responder, a pending-spawn table keyed by `(peer, Ref)`, and any `DOP_SPAWN_REPLY` channel (`InboundSink` has none) |
| `Other(Dop::Unlink)` | **gap** — `DOP_UNLINK` is not a v6 opcode; the C's `invalid_message` arm kills the connection, which the link driver owns |
Evidence: `../../tests/dist_delivery.rs`'s "(6b) Every inbound delivery is validated against **its own link**" — a forged `DOP_REG_SEND` and a `DOP_LINK` naming a third node are each refused, reach no mailbox and retire their link with a recorded `last_link_error`, while a well-formed message on the same link still delivers; the zero-id `DOP_UNLINK_ID` pair is refused the same way on both halves.

## The scheduled path's fixed costs
Per pass `take_processes` builds a **fresh empty `ProcessTable`** (ten `HashMap::new()`, its `Vec`s, a `NEXT_TAG.fetch_add`, a fresh `RefAllocator`) and `mem::replace`s it in; `Scheduler::new()` replaces the old scheduler; `put_processes` is a plain assignment.
The per-slice machine/context churn (engine out of and back into its map, a fresh `RuntimeContext` with its `DispatchMemo`, `enter_module`'s `String`, the park hand-off's `copy_from_slice`, skipped for an exempt park) is `../../runner/CONTEXT.md` *Per-slice (not per-instruction) host cost*.
This node's own addition is the receive-deadline bookkeeping: `schedule_receive_timeout`/`cancel_receive_timeout`/`take_timer_resume` add and remove one timer-heap entry **and** one `TimerWakes` entry, so a finite `receive … after T` pays it every iteration (`Timers` reclaims a non-head cancellation by a watermark rebuild, `COMPACT_SLACK = 64`, so no permanent tombstone); every pid/`Eterm`-keyed map the hot path touches is `crate::hasher`'s, not SipHash.
Per round `service_file_requests` allocates one `Vec::with_capacity(1 + devices + retired)`, `deliver_port_messages` early-returns `Ok(0)` on `has_nothing_to_poll()`, each distribution half is one `Option` match with no node, and `service_lost_dist_peers` is `Ok(0)` after one borrow and one `is_empty`.
`O(heap)`/`O(#mailbox)` work happens only when triggered: `ensure_heap` short-circuits on occupancy but still pays a `current_module` `String` clone and two valve lookups, and the `O(live)` descent is gated per pid on the shared `ForeignLatch` cell the table's `RuntimeCollectionGate` also holds, so a clear bit answers `false` with no descent.
Out of this node: the per-slice machine re-entry/resume (`beam_engine`'s `resume_outcome*`) and `slice_begin`/`slice_end`.

## Test Strategy
`../scheduler.rs` carries **5** in-file `#[test]`s in `mod quiescence_and_port_table_tests` over a real `Runtime`: the quiescence predicate at each corner (`an_idle_signal_watch_is_quiescent_but_a_caught_one_is_not`, `a_non_distributed_runtime_is_quiescent_with_no_distribution_term`, `a_node_with_no_accepted_link_stays_quiescent`, `a_client_holding_a_link_is_not_quiescent`) and `the_short_borrow_seam_hands_both_tables_out_and_a_nested_borrow_is_a_typed_gap`.
`./tests.rs` holds **34** tests over a real booted `Runtime` in five groups: **step budget and progress** (round-robin completion, the context-switch credit, `a_bounded_step_does_not_wait_for_a_pending_deadline`, the `a_recorded_spawn_*` priority pair); **fragment wakes** (`a_fragment_note_for_an_engine_less_pid_does_not_take_the_pass_down`, `a_fragment_note_alone_does_not_enqueue_a_parked_receiver`); **the timer half** (the wake/drop pair, the `-eval` timer pair, the finite-receive matrix, the `the_imminent_only_wait_policy_*` and `a_boot_drive_step_*` policy cases); **the `run` action and its wait accounting** (the `run_scheduled_*` family plus the four sleep pins above); and **the distribution halves** (`the_outbound_distribution_client_is_wired_and_driven_by_the_runtime`, `the_outbound_half_creation_follows_the_nodes_live_creation`).
`zz_experiment_boot_deadlines` is an `eprintln!`-only probe over the real `bootstrap/bin/start.boot`; integration routing is `../../tests/CONTEXT.md` and the gate commands are `../../TEST_STRATEGY.md`'s.
