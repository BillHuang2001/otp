// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%
//! Integration evidence for the **receive-scan** signal entry point
//! ([`ProcessTable::service_receive_signals`](beam_process::ProcessTable::service_receive_signals)).
//!
//! A receive scan runs *inside* the receiving process, so it may only apply the signals whose
//! application yields a mailbox message it can match in the same reduction slice, and must leave a
//! **terminating** signal for the schedule-out net. The legacy C is the specification:
//! `i_loop_rec` (`erts/emulator/beam/emu/msg_instrs.tab:60-140`) calls
//! `erts_proc_sig_receive_helper` (`erl_proc_sig_queue.c:7729`) on an exhausted message queue and
//! re-peeks; the helper handles the process's own incoming signals
//! (`erts_proc_sig_handle_incoming`, `:6229`) — a trapped exit becomes `{'EXIT', …}`, a
//! monitor-down becomes `{'DOWN', …}`, a `normal` exit to a non-trapping receiver is dropped — but a
//! signal that would terminate only sets `ERTS_PSFLG_EXITING` and reports `get_out > 0`
//! (`:7860-7872`), so the instruction jumps to `do_schedule` instead of applying it.
//!
//! What this suite pins:
//!
//! * a trapped **link** exit at the head is applied into the mailbox (queue emptied, one message);
//! * a `kill` **link** exit is trappable while an explicit `erlang:exit/2` `kill` is not — the
//!   applied/stopped split is cross-checked against `exit_policy`, so the receive scan cannot drift
//!   from the propagation rule;
//! * a monitor `'DOWN'` is applied (plain and `{tag, T}` forms);
//! * with both kinds queued, the **mailbox order equals the queue's FIFO order**, in both
//!   arrangements;
//! * a **terminating** head stops the scan: nothing applied, nothing popped, the process alive and
//!   its signal still queued — and the same fixture proves the schedule-in
//!   [`service_signals`](beam_process::ProcessTable::service_signals) *does* apply it (the regression
//!   pin for the entry point that owns termination);
//! * an **ignored** head (`normal` to a non-trapping receiver) is discarded and the next signal is
//!   still delivered;
//! * a quiet, unknown and stale pid is a no-op (the fast path);
//! * a delivered message wakes a blocked receiver (the shared `deliver_head_signal` tail);
//! * a **`Running`** receiver's in-scan delivery takes the heap-fragment route, because a running
//!   process has unpublished live roots — no collection, no nursery move, nothing written on its
//!   heap (the same machinery as `tests/signal_fragment_fallback.rs`).
//!
//! # Determinism limits (and why the C, not the oracle, is the ordering source)
//!
//! An `oracle_*` probe is only written where the oracle's answer cannot race. The **FIFO order of
//! two independently generated signals** (an `{'EXIT', …}` from a dying linked peer and a
//! `{'DOWN', …}` from a dying monitored target) is **not** one of them: Erlang guarantees ordering
//! only per `(sender, receiver)` pair, so which of the two arrives first in a probe is a scheduling
//! accident. The crate's order is nevertheless exact — one FIFO per receiver
//! (`src/signal_queue.rs`), which is the C's per-process queue (`erl_proc_sig_queue.c`) and
//! strictly stronger than the pairwise guarantee — so the ordering cases below are pinned against
//! the queue, and the probe that would compare two independent signals against the oracle is
//! deliberately **not** written.
//!
//! One probe observes the **absence** of a message (a `normal` link exit ignored by a
//! non-trapping process). That needs a timeout to terminate at all; the signal is generated at the
//! linked peer's exit, i.e. in the first microseconds of a window measured in hundreds of
//! milliseconds, so the answer does not race in practice. It is the only negative probe here and it
//! is documented as such.
//!
//! The `Running` case is a **fixture**, not a timing argument: the receiver's nursery is sized so
//! the message build is refused by capacity (see [`assert_head_cannot_fit`]).
use beam_alloc::Heap;
use beam_common::Result;
use beam_gc::HeapPolicy;
use beam_process::prelude::*;
use beam_term::compare;
use beam_term::prelude::{make_atom, make_small, AtomTable, Eterm};

mod common;

/// A table plus the atom table its [`SignalAtoms`] were interned from (so tests can compare
/// terms structurally with `compare::eq`) and the atoms themselves.
fn table_with_names() -> (ProcessTable, AtomTable, SignalAtoms) {
    let mut names = AtomTable::new();
    let atoms = SignalAtoms::from_table(&mut names);
    (ProcessTable::new(atoms), names, atoms)
}

/// A pid nothing has spawned (the slot does not exist).
fn unslotted() -> ProcessId {
    ProcessId::new(9_000_001, 0)
}

/// The payload of `pid`'s `index`-th queued message (mailbox order == receive order).
fn payload_at(table: &ProcessTable, pid: ProcessId, index: usize) -> Eterm {
    table
        .get(pid)
        .expect("the receiver is live")
        .mailbox()
        .iter()
        .nth(index)
        .expect("a message is queued at that position")
        .payload()
}

/// The number of messages queued on `pid`.
fn mailbox_len(table: &ProcessTable, pid: ProcessId) -> usize {
    table.get(pid).expect("the receiver is live").mailbox().len()
}

/// The source arena the expected terms are built on.
fn source() -> Heap {
    Heap::new(64).expect("the source arena allocates")
}

/// `{'EXIT', From, Reason}`, built on `arena`.
fn expected_exit(
    arena: &mut Heap,
    atoms: &SignalAtoms,
    from: ProcessId,
    reason: Eterm,
) -> Result<Eterm> {
    arena.tuple(&[make_atom(atoms.exit()), from.to_eterm(), reason])
}

/// `{'DOWN', Ref, Kind, Target, Reason}`, built on `arena`.
fn expected_down(
    arena: &mut Heap,
    atoms: &SignalAtoms,
    reference: MonitorRef,
    kind: MonitorKind,
    target: ProcessId,
    reason: Eterm,
) -> Result<Eterm> {
    let reference = reference.to_eterm(arena)?;
    let kind = match kind {
        MonitorKind::Process => atoms.process(),
        MonitorKind::Port => atoms.port(),
    };
    arena.tuple(&[
        make_atom(atoms.down()),
        reference,
        make_atom(kind),
        target.to_eterm(),
        reason,
    ])
}

/// Marks `pid` as the currently executing process, which is what makes a signal generated for it
/// **deferred** to its queue instead of serviced eagerly (`schedule_service`) — the state a
/// receive scan always runs in.
fn make_running(table: &mut ProcessTable, pid: ProcessId) {
    table
        .get_mut(pid)
        .expect("the receiver is live")
        .set_state(ProcessState::Running);
}

#[test]
fn a_trapped_link_exit_at_the_head_becomes_an_exit_message() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut arena = source();
    let dying = table.spawn_default()?;
    let receiver = table.spawn_default()?;
    table.get_mut(receiver).expect("live").set_trap_exit(true);
    table.link(dying, receiver)?;
    make_running(&mut table, receiver);
    let reason = make_small(11);
    // The signal is generated while the receiver is executing, so it is queued, not applied.
    table.terminate(dying, reason)?;
    assert_eq!(
        table.pending_signals(receiver)?,
        1,
        "the running receiver's signal is deferred to its queue"
    );
    assert_eq!(mailbox_len(&table, receiver), 0, "nothing applied yet");

    table.service_receive_signals(receiver)?;

    assert_eq!(
        table.pending_signals(receiver)?,
        0,
        "the applied signal left the queue"
    );
    assert_eq!(mailbox_len(&table, receiver), 1, "exactly one message");
    let expected = expected_exit(&mut arena, &atoms, dying, reason)?;
    assert!(
        compare::eq(payload_at(&table, receiver, 0), expected, &names),
        "the in-scan delivery is the same {{'EXIT', From, Reason}} the schedule-in path builds"
    );
    assert!(table.contains(receiver), "a trapped exit does not kill");
    assert!(
        table.woken().is_empty(),
        "a Running receiver is not blocked, so the delivery wakes nobody"
    );
    Ok(())
}

#[test]
fn a_kill_link_exit_is_trapped_by_a_trapping_receiver() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut arena = source();
    let dying = table.spawn_default()?;
    let receiver = table.spawn_default()?;
    table.get_mut(receiver).expect("live").set_trap_exit(true);
    table.link(dying, receiver)?;
    make_running(&mut table, receiver);
    let kill = atoms.kill_term();
    // The cross-check: a LINK-family `kill` is trapped (never rewritten to `killed`).
    assert_eq!(
        receive_applicability(
            PendingSignal::exit(dying, kill, ExitFamily::Linked),
            true,
            &atoms
        ),
        ReceiveApplicability::Apply
    );
    table.terminate(dying, kill)?;

    table.service_receive_signals(receiver)?;

    assert_eq!(table.pending_signals(receiver)?, 0);
    let expected = expected_exit(&mut arena, &atoms, dying, kill)?;
    assert!(
        compare::eq(payload_at(&table, receiver, 0), expected, &names),
        "a trapping receiver sees {{'EXIT', Dying, kill}}, not `killed`"
    );
    assert!(table.contains(receiver), "the trapping receiver survived");
    Ok(())
}

#[test]
fn an_untrappable_explicit_kill_at_the_head_stops_the_scan() -> Result<()> {
    let (mut table, _names, atoms) = table_with_names();
    let signaller = table.spawn_default()?;
    let receiver = table.spawn_default()?;
    table.get_mut(receiver).expect("live").set_trap_exit(true);
    make_running(&mut table, receiver);
    let kill = atoms.kill_term();
    // An explicit `erlang:exit/2` `kill` terminates even a trapping receiver (`killed`).
    assert_eq!(
        receive_applicability(
            PendingSignal::exit(signaller, kill, ExitFamily::Explicit),
            true,
            &atoms
        ),
        ReceiveApplicability::Stop
    );
    table.send_exit(signaller, receiver, kill)?;
    assert_eq!(table.pending_signals(receiver)?, 1);

    table.service_receive_signals(receiver)?;

    assert_eq!(
        table.pending_signals(receiver)?,
        1,
        "the terminating signal is left queued, untouched"
    );
    assert_eq!(
        mailbox_len(&table, receiver),
        0,
        "no message: an untrappable kill yields none"
    );
    assert!(
        table.contains(receiver),
        "a receive scan must never kill the process it is executing in"
    );
    Ok(())
}

#[test]
fn a_monitor_down_at_the_head_becomes_a_down_message() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut arena = source();
    let watcher = table.spawn_default()?;
    let target = table.spawn_default()?;
    let reference = table.monitor(watcher, target, MonitorKind::Process)?;
    make_running(&mut table, watcher);
    let reason = make_small(13);
    table.terminate(target, reason)?;
    assert_eq!(table.pending_signals(watcher)?, 1);

    table.service_receive_signals(watcher)?;

    assert_eq!(table.pending_signals(watcher)?, 0);
    let expected = expected_down(
        &mut arena,
        &atoms,
        reference,
        MonitorKind::Process,
        target,
        reason,
    )?;
    assert!(
        compare::eq(payload_at(&table, watcher, 0), expected, &names),
        "the in-scan delivery is the ordinary five-element {{'DOWN', …}}"
    );
    Ok(())
}

#[test]
fn a_tagged_monitor_down_keeps_its_user_tag() -> Result<()> {
    let (mut table, mut names, atoms) = table_with_names();
    let tag = make_atom(names.intern("mytag"));
    let mut arena = source();
    let watcher = table.spawn_default()?;
    let target = table.spawn_default()?;
    let reference =
        table.monitor_with_tag(watcher, target, MonitorKind::Process, Some(tag))?;
    make_running(&mut table, watcher);
    let reason = make_small(17);
    table.terminate(target, reason)?;

    table.service_receive_signals(watcher)?;

    assert_eq!(table.pending_signals(watcher)?, 0);
    // The user tag replaces the `'DOWN'` head atom; every other element is unchanged.
    let reference_term = reference.to_eterm(&mut arena)?;
    let expected = arena.tuple(&[
        tag,
        reference_term,
        make_atom(atoms.process()),
        target.to_eterm(),
        reason,
    ])?;
    assert!(
        compare::eq(payload_at(&table, watcher, 0), expected, &names),
        "the `{{tag, T}}` form is delivered verbatim"
    );
    Ok(())
}

#[test]
fn the_mailbox_order_is_the_signal_queues_fifo_order() -> Result<()> {
    // Arrangement A: the link exit is enqueued first (`terminate_into` handles links before
    // monitors), so the mailbox must show `{'EXIT', …}` then `{'DOWN', …}`.
    let (mut table, names, atoms) = table_with_names();
    let mut arena = source();
    let dying = table.spawn_default()?;
    let target = table.spawn_default()?;
    let receiver = table.spawn_default()?;
    table.get_mut(receiver).expect("live").set_trap_exit(true);
    table.link(dying, receiver)?;
    let reference = table.monitor(receiver, target, MonitorKind::Process)?;
    make_running(&mut table, receiver);
    let exit_reason = make_small(21);
    let down_reason = make_small(22);
    table.terminate(dying, exit_reason)?;
    table.terminate(target, down_reason)?;
    assert_eq!(
        table.pending_signals(receiver)?,
        2,
        "both signals are queued in generation order"
    );

    table.service_receive_signals(receiver)?;

    assert_eq!(table.pending_signals(receiver)?, 0);
    assert_eq!(mailbox_len(&table, receiver), 2);
    let first = expected_exit(&mut arena, &atoms, dying, exit_reason)?;
    let second = expected_down(
        &mut arena,
        &atoms,
        reference,
        MonitorKind::Process,
        target,
        down_reason,
    )?;
    assert!(
        compare::eq(payload_at(&table, receiver, 0), first, &names),
        "the exit signal is first, exactly as it was queued"
    );
    assert!(
        compare::eq(payload_at(&table, receiver, 1), second, &names),
        "the DOWN follows it"
    );

    // Arrangement B: the DOWN is generated first, and the mailbox order follows the queue.
    let (mut table, names, atoms) = table_with_names();
    let mut arena = source();
    let dying = table.spawn_default()?;
    let target = table.spawn_default()?;
    let receiver = table.spawn_default()?;
    table.get_mut(receiver).expect("live").set_trap_exit(true);
    table.link(dying, receiver)?;
    let reference = table.monitor(receiver, target, MonitorKind::Process)?;
    make_running(&mut table, receiver);
    table.terminate(target, down_reason)?;
    table.terminate(dying, exit_reason)?;

    table.service_receive_signals(receiver)?;

    assert_eq!(mailbox_len(&table, receiver), 2);
    let down = expected_down(
        &mut arena,
        &atoms,
        reference,
        MonitorKind::Process,
        target,
        down_reason,
    )?;
    let exit = expected_exit(&mut arena, &atoms, dying, exit_reason)?;
    assert!(
        compare::eq(payload_at(&table, receiver, 0), down, &names),
        "the DOWN is first this time: the order is the queue's, not the kind's"
    );
    assert!(compare::eq(payload_at(&table, receiver, 1), exit, &names));
    Ok(())
}

#[test]
fn a_terminating_head_stops_the_scan_and_leaves_everything_queued() -> Result<()> {
    let (mut table, _names, atoms) = table_with_names();
    let dying = table.spawn_default()?;
    let receiver = table.spawn_default()?;
    table.link(dying, receiver)?;
    make_running(&mut table, receiver);
    let boom = make_small(31);
    // The cross-check: a non-trapping receiver of a link exit terminates with the reason.
    assert_eq!(
        receive_applicability(
            PendingSignal::exit(dying, boom, ExitFamily::Linked),
            false,
            &atoms
        ),
        ReceiveApplicability::Stop
    );
    table.terminate(dying, boom)?;

    table.service_receive_signals(receiver)?;

    assert_eq!(
        table.pending_signals(receiver)?,
        1,
        "the terminating signal stays queued"
    );
    assert_eq!(mailbox_len(&table, receiver), 0, "nothing was delivered");
    assert!(
        table.contains(receiver),
        "the receive scan leaves the termination to the schedule-out net"
    );
    assert_eq!(
        table.signals(receiver)?.peek().copied(),
        Some(PendingSignal::exit(dying, boom, ExitFamily::Linked)),
        "and it is untouched, not merely re-queued"
    );
    Ok(())
}

#[test]
fn a_trapped_exit_is_applied_before_a_terminating_head_stops_the_scan() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut arena = source();
    let dying = table.spawn_default()?;
    let signaller = table.spawn_default()?;
    let receiver = table.spawn_default()?;
    table.get_mut(receiver).expect("live").set_trap_exit(true);
    table.link(dying, receiver)?;
    make_running(&mut table, receiver);
    let boom = make_small(41);
    let kill = atoms.kill_term();
    // Queue order: the trappable link exit, then an untrappable explicit kill.
    table.terminate(dying, boom)?;
    table.send_exit(signaller, receiver, kill)?;
    assert_eq!(table.pending_signals(receiver)?, 2);

    table.service_receive_signals(receiver)?;

    assert_eq!(
        table.pending_signals(receiver)?,
        1,
        "the scan stopped at the terminating signal"
    );
    assert_eq!(mailbox_len(&table, receiver), 1, "the trapped exit was applied");
    let expected = expected_exit(&mut arena, &atoms, dying, boom)?;
    assert!(compare::eq(payload_at(&table, receiver, 0), expected, &names));
    assert!(table.contains(receiver), "the receiver is still alive");
    assert_eq!(
        table.signals(receiver)?.peek().copied(),
        Some(PendingSignal::exit(signaller, kill, ExitFamily::Explicit)),
        "the untrappable kill is still at the head, waiting for the schedule-out net"
    );
    Ok(())
}

#[test]
fn an_ignored_head_is_discarded_and_the_next_signal_is_delivered() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut arena = source();
    let dying = table.spawn_default()?;
    let target = table.spawn_default()?;
    let receiver = table.spawn_default()?;
    // Not trapping: a `normal` link exit is ignored, a DOWN is still delivered.
    table.link(dying, receiver)?;
    let reference = table.monitor(receiver, target, MonitorKind::Process)?;
    make_running(&mut table, receiver);
    let normal = atoms.normal_term();
    let down_reason = make_small(51);
    assert_eq!(
        receive_applicability(
            PendingSignal::exit(dying, normal, ExitFamily::Linked),
            false,
            &atoms
        ),
        ReceiveApplicability::Discard
    );
    table.terminate(dying, normal)?;
    table.terminate(target, down_reason)?;
    assert_eq!(table.pending_signals(receiver)?, 2);

    table.service_receive_signals(receiver)?;

    assert_eq!(
        table.pending_signals(receiver)?,
        0,
        "the ignored signal was dropped and the DOWN delivered"
    );
    assert_eq!(
        mailbox_len(&table, receiver),
        1,
        "exactly one message: the ignored `normal` exit produced none"
    );
    let expected = expected_down(
        &mut arena,
        &atoms,
        reference,
        MonitorKind::Process,
        target,
        down_reason,
    )?;
    assert!(compare::eq(payload_at(&table, receiver, 0), expected, &names));
    assert!(table.contains(receiver));
    Ok(())
}

#[test]
fn a_quiet_unknown_or_stale_pid_is_a_no_op() -> Result<()> {
    let (mut table, _names, _atoms) = table_with_names();

    // A pid nothing has spawned, and a live process with nothing queued: both are the fast path.
    table.service_receive_signals(unslotted())?;
    assert!(table.woken().is_empty());

    let quiet = table.spawn_default()?;
    let peer = table.spawn_default()?;
    table.link(quiet, peer)?;
    make_running(&mut table, quiet);
    let gc_before = table.get(quiet).expect("live").collector().minor_gcs();
    for _ in 0..3 {
        table.service_receive_signals(quiet)?;
        let process = table.get(quiet).expect("quiet is live");
        assert_eq!(
            process.state(),
            ProcessState::Running,
            "a quiet scan touches no state"
        );
        assert_eq!(process.pending_signals(), 0);
        assert!(process.mailbox().is_empty());
        assert_eq!(
            process.collector().minor_gcs(),
            gc_before,
            "no collection ran (and the path allocated no worklist)"
        );
        assert_eq!(process.links().len(), 1, "the link is untouched");
    }
    assert!(table.woken().is_empty());

    // A stale pid (a freed slot's previous occupant) must not be serviced, and must not disturb
    // the process that now owns the recycled number.
    table.remove(peer).expect("the peer was live");
    let reused = table.spawn_default()?;
    assert_eq!(reused.number(), peer.number(), "the number is recycled");
    assert!(reused.serial() > peer.serial(), "with a bumped serial");
    make_running(&mut table, reused);
    table.service_receive_signals(peer)?;
    assert!(table.contains(reused));
    assert_eq!(
        table.get(reused).expect("live").state(),
        ProcessState::Running
    );
    assert_eq!(table.pending_signals(reused)?, 0);
    assert!(table.woken().is_empty());
    Ok(())
}

#[test]
fn a_delivered_message_wakes_a_blocked_receiver() -> Result<()> {
    // A signal generated for a **non-running** receiver is serviced eagerly, so a queued signal on
    // a blocked one cannot be produced through the public producers: it is enqueued directly here
    // (the queue is a documented seam). The payload is an immediate, so it needs no heap copy.
    let (mut table, names, atoms) = table_with_names();
    let mut arena = source();
    let signaller = table.spawn_default()?;
    let receiver = table.spawn_default()?;
    table.get_mut(receiver).expect("live").set_trap_exit(true);
    table
        .get_mut(receiver)
        .expect("live")
        .set_state(ProcessState::Waiting);
    let boom = make_small(61);
    table
        .get_mut(receiver)
        .expect("live")
        .signals_mut()
        .push(PendingSignal::exit(signaller, boom, ExitFamily::Linked));

    table.service_receive_signals(receiver)?;

    assert_eq!(table.pending_signals(receiver)?, 0);
    let expected = expected_exit(&mut arena, &atoms, signaller, boom)?;
    assert!(compare::eq(payload_at(&table, receiver, 0), expected, &names));
    assert_eq!(
        table.get(receiver).expect("live").state(),
        ProcessState::Runnable,
        "the delivery woke the blocked receiver"
    );
    assert_eq!(
        table.woken(),
        &[receiver],
        "and recorded it on the wake list, exactly as the schedule-in path does"
    );
    Ok(())
}

/// The words a `{'EXIT', From, Reason}` tuple occupies — and the words the `{'DOWN', …}` build
/// allocates **first** (its boxed four-word monitor reference), so one number covers "what the
/// head build needs before it can fail".
const HEAD_WORDS: usize = 4;

/// The free words the receiver's nursery is drained to: one short of the head build, so the build
/// is refused **by capacity** at its first allocation and therefore writes nothing.
const FREE_WORDS: usize = HEAD_WORDS - 1;

/// The receiver's nursery: the filler ([`HEAD_WORDS`]) plus [`FREE_WORDS`].
const NURSERY_WORDS: usize = HEAD_WORDS + FREE_WORDS;

/// The heap policy of the fragment fixture: a [`NURSERY_WORDS`]-word nursery, everything else at
/// the BEAM defaults.
fn small_nursery() -> HeapPolicy {
    HeapPolicy {
        initial_young_words: NURSERY_WORDS,
        ..HeapPolicy::default()
    }
}

/// The filler payload: a [`HEAD_WORDS`]-word tuple of distinct small integers, so a truncated or
/// duplicated copy cannot pass a structural comparison.
fn filler_term(src: &mut Heap) -> Eterm {
    src.tuple(&[make_small(0), make_small(1), make_small(2)])
        .expect("the filler fits the source arena")
}

/// Spawns the fragment fixture's receiver and drains its nursery to exactly [`FREE_WORDS`] free
/// words with a delivered (and therefore queue-resident) filler message.
fn drained_receiver(table: &mut ProcessTable, filler: Eterm) -> Result<ProcessId> {
    let receiver = table.spawn(small_nursery())?;
    table.deliver_simple(receiver, ProcessId::new(99, 0).to_eterm(), filler)?;
    let process = table.get(receiver).expect("the receiver is live");
    assert_eq!(
        process.heap_used_words(),
        HEAD_WORDS,
        "the filler drained the nursery to exactly FREE_WORDS free words"
    );
    assert_eq!(
        process.collector().young().available_words(),
        FREE_WORDS,
        "the fixture's arithmetic holds: one word short of the head build"
    );
    Ok(receiver)
}

/// Asserts the drained receiver's nursery **cannot** take the four-word head — the precondition
/// that makes the fragment fallback necessary rather than incidental — and that a refused request
/// writes nothing.
fn assert_head_cannot_fit(table: &mut ProcessTable, pid: ProcessId) {
    let process = table.get_mut(pid).expect("the receiver is live");
    let used = process.heap_used_words();
    assert!(
        process
            .heap_mut()
            .tuple(&[make_small(0), make_small(0), make_small(0), make_small(0)])
            .is_err(),
        "the fixture's nursery really cannot take the four-word head build"
    );
    assert_eq!(
        process.heap_used_words(),
        used,
        "a refused request writes nothing"
    );
}

/// The receiver's nursery observables: minor-GC count, base pointer and used words.
fn nursery(table: &ProcessTable, pid: ProcessId) -> (u32, *mut Eterm, usize) {
    let process = table.get(pid).expect("the receiver is live");
    (
        process.collector().minor_gcs(),
        process.collector().young().base(),
        process.heap_used_words(),
    )
}

#[test]
fn a_running_receiver_gets_its_in_scan_down_through_a_heap_fragment() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut arena = source();
    let reason = atoms.noproc_term();
    let receiver = drained_receiver(&mut table, filler_term(&mut arena))?;
    assert_head_cannot_fit(&mut table, receiver);
    let target = table.spawn_default()?;
    let reference = table.monitor(receiver, target, MonitorKind::Process)?;
    make_running(&mut table, receiver);
    assert!(
        !table.get(receiver).expect("live").is_relocation_exempt(),
        "not exempt: the gate here is the *running* state"
    );
    let before = nursery(&table, receiver);
    let mature_before = table
        .get(receiver)
        .expect("live")
        .collector()
        .mature_used_words();

    // The DOWN is queued (an immediate reason copies without allocating) and deferred, because the
    // receiver is the executing process.
    table.terminate(target, reason)?;
    assert_eq!(table.pending_signals(receiver)?, 1);
    assert_eq!(
        nursery(&table, receiver),
        before,
        "the deferred signal did not touch the running receiver's heap"
    );

    // The in-scan application cannot build the message on that heap, and the heap must not move
    // (a running process's live roots are not all in the crate root set), so the DOWN is built
    // into a heap fragment.
    table.service_receive_signals(receiver)?;

    assert_eq!(table.pending_signals(receiver)?, 0, "the signal was applied");
    let after = nursery(&table, receiver);
    assert_eq!(after.0, before.0, "a running receiver must not collect");
    assert_eq!(
        after.1, before.1,
        "a running receiver's nursery must not be relocated or grown"
    );
    assert_eq!(
        after.2, before.2,
        "the fragment-built DOWN wrote nothing on the receiver's heap"
    );
    assert_eq!(
        table
            .get(receiver)
            .expect("live")
            .collector()
            .mature_used_words(),
        mature_before,
        "nothing was promoted off the nursery either"
    );
    // ... and it is nevertheless delivered, whole, behind the fixture's filler.
    assert_eq!(mailbox_len(&table, receiver), 2, "the filler and the DOWN");
    let message = table
        .get(receiver)
        .expect("live")
        .mailbox()
        .iter()
        .nth(1)
        .expect("the DOWN is queued behind the filler");
    let fragments = message
        .fragments()
        .expect("the running receiver's DOWN took the fragment route");
    assert!(
        fragments
            .iter()
            .next()
            .expect("one fragment")
            .contains(message.payload()),
        "the payload root lives inside the fragment"
    );
    let expected = expected_down(
        &mut arena,
        &atoms,
        reference,
        MonitorKind::Process,
        target,
        reason,
    )?;
    assert!(
        compare::eq(message.payload(), expected, &names),
        "the fragment-resident DOWN equals the expected five-tuple element for element"
    );
    Ok(())
}

#[test]
fn the_schedule_in_path_still_applies_the_terminating_head() -> Result<()> {
    // The regression pin for the difference between the two entry points: the same fixture that
    // `service_receive_signals` leaves alone is applied — and cascaded — by `service_signals`.
    let (mut table, _names, _atoms) = table_with_names();
    let dying = table.spawn_default()?;
    let receiver = table.spawn_default()?;
    table.link(dying, receiver)?;
    make_running(&mut table, receiver);
    table.terminate(dying, make_small(71))?;

    table.service_receive_signals(receiver)?;
    assert!(table.contains(receiver), "the receive scan never terminates");

    table.service_signals(receiver)?;
    assert!(
        !table.contains(receiver),
        "the schedule-in entry point applies the terminating signal"
    );
    Ok(())
}

// --- oracle cross-checks ----------------------------------------------------
//
// Each probe is written only where the oracle's answer cannot race (see the module docs). The
// driver (`tests/common/mod.rs`) runs `erl -noshell -kernel logger_level none -eval '<expr>,
// halt(0).'` and skips loudly when `erl` is absent.

#[test]
fn oracle_a_trapped_link_exit_is_delivered_through_the_receive() {
    // The receive is the only consumer of the message queue here, so the `{'EXIT', …}` the scan
    // fetches is the one this entry point reproduces (the receive *waits* for it: without the
    // in-scan application the probe would time out).
    assert!(common::expect_oracle(
        "process_flag(trap_exit,true), \
         P=spawn_link(fun()->exit(boom) end), \
         receive {'EXIT',P,R} -> io:format(\"~p\",[R]) \
         after 5000 -> io:format(\"timeout\") end",
        "boom",
    ));
}

#[test]
fn oracle_a_trapping_receiver_is_not_killed_by_a_link_kill() {
    // A LINK-family `kill` is trappable and is not rewritten: the `{'EXIT', P, kill}` arrives as
    // an ordinary message (and the process survives to print it).
    assert!(common::expect_oracle(
        "process_flag(trap_exit,true), \
         P=spawn_link(fun()->exit(kill) end), \
         receive {'EXIT',P,R} -> io:format(\"~p|~p\",[R, is_process_alive(P)]) \
         after 5000 -> io:format(\"timeout\") end",
        "kill|false",
    ));
}

#[test]
fn oracle_an_explicit_kill_terminates_a_trapping_process_with_killed() {
    // The untrappable case this crate's scan *stops* on: the process dies with `killed` and no
    // `{'EXIT', …}` message is ever produced.
    assert!(common::expect_oracle(
        "P=spawn(fun()->process_flag(trap_exit,true), \
                    receive _ -> ok after 5000 -> ok end end), \
         R=monitor(process,P), exit(P,kill), \
         receive {'DOWN',R,process,P,W} -> io:format(\"~p\",[W]) \
         after 5000 -> io:format(\"timeout\") end",
        "killed",
    ));
}

#[test]
fn oracle_a_monitor_down_carries_the_targets_reason() {
    assert!(common::expect_oracle(
        "P=spawn(fun()->ok end), R=monitor(process,P), \
         receive {'DOWN',R,process,P,W} -> io:format(\"~p\",[W]) \
         after 5000 -> io:format(\"timeout\") end",
        "normal",
    ));
}

#[test]
fn oracle_a_normal_link_exit_is_ignored_by_a_non_trapping_receiver() {
    // The one *negative* probe here: it observes the **absence** of a message, so it needs a
    // timeout to terminate at all. The signal is generated at the linked peer's exit — in the
    // first microseconds of a five-second window — so the answer does not race in practice.
    assert!(common::expect_oracle(
        "process_flag(trap_exit,false), P=spawn_link(fun()->ok end), \
         receive M -> io:format(\"got ~p\",[M]) \
         after 5000 -> io:format(\"ignored\") end",
        "ignored",
    ));
}
