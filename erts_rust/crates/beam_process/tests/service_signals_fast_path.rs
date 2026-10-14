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
//! Integration evidence for the **quiet-process fast path** of the per-schedule-in signal
//! servicing ([`ProcessTable::service_signals`](beam_process::ProcessTable::service_signals)) and
//! for the capacity-retaining wake-list hand-over
//! ([`ProcessTable::drain_woken_into`](beam_process::ProcessTable::drain_woken_into)).
//!
//! Both are pure work reductions on the delivery path — the fast path skips the
//! `drain_signals` worklist for a process whose signal queue is empty, and `drain_woken_into`
//! moves the wake list into a caller-owned buffer instead of handing the buffer over. Neither may
//! change a single observable, so this suite pins the observables on both sides of the split:
//!
//! * a **quiet** schedule-in is a no-op — the process survives, keeps its state, mailbox,
//!   links and heap, queues no wake and runs no collection;
//! * an **unknown** or **stale** pid is a no-op, and a stale pid must not disturb the process that
//!   now owns its recycled slot;
//! * a **queued** signal is still applied in full, so the fast path is a shortcut for the empty
//!   case only;
//! * a queued **fatal** signal still drives the whole link cascade — the fast path cannot shorten
//!   it, because the worklist grows from a signal encountered *while servicing*;
//! * `drain_woken_into` and the take-based `drain_woken` agree on the same scenario, and the
//!   reused buffer is cleared rather than appended to;
//! * the `Scheduler`'s reusable wake scratch never double-enqueues a woken process.
use beam_common::Result;
use beam_message::OffHeapTable;
use beam_process::prelude::*;
use beam_term::prelude::{is_atom, make_atom, make_small, tuple_element, tuple_size, Atom, AtomTable, Eterm};

/// An empty table with a fresh atom table behind its signal atoms.
fn table() -> ProcessTable {
    ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
}

/// A table plus the atom indices of the well-known signal atoms it builds messages with.
fn table_with_atoms() -> (ProcessTable, Atom) {
    let mut names = AtomTable::new();
    let atoms = SignalAtoms::from_table(&mut names);
    let exit = atoms.exit();
    (ProcessTable::new(atoms), exit)
}

/// A runner that counts how many times it was asked to execute a process and leaves the queue
/// (`Wait`) instead of requeueing, so a slice cannot mask a duplicate run-queue entry.
#[derive(Default)]
struct Counter {
    runs: usize,
}

impl ProcessRunner for Counter {
    fn run(
        &mut self,
        _pid: ProcessId,
        _table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        self.runs += 1;
        Ok(RunOutcome::Wait)
    }
}

/// A pid nothing has spawned (the slot does not exist).
fn unslotted() -> ProcessId {
    ProcessId::new(9_000_001, 0)
}

#[test]
fn a_quiet_schedule_in_is_a_no_op() -> Result<()> {
    let mut table = table();
    let quiet = table.spawn_default()?;
    let peer = table.spawn_default()?;
    table.link(quiet, peer)?;
    // The single scheduler marks the executing process `Running`; a quiet schedule-in must leave
    // even that state alone.
    let quiet_gc_before = table.get(quiet).expect("quiet is live").collector().minor_gcs();
    table.get_mut(quiet).expect("quiet is live").set_state(ProcessState::Running);

    for _ in 0..3 {
        table.service_signals(quiet)?;
        assert!(table.contains(quiet), "a quiet process survives its schedule-in");
        let process = table.get(quiet).expect("quiet is live");
        assert_eq!(
            process.state(),
            ProcessState::Running,
            "servicing a quiet process does not touch its state"
        );
        assert_eq!(process.pending_signals(), 0, "nothing was queued");
        assert!(process.mailbox().is_empty(), "nothing was delivered");
        assert_eq!(
            process.collector().minor_gcs(),
            quiet_gc_before,
            "no collection ran"
        );
        assert_eq!(process.links().len(), 1, "the link is untouched");
    }
    assert!(table.woken().is_empty(), "a quiet schedule-in queues no wake");
    assert_eq!(
        table.pending_signals(peer)?,
        0,
        "the untouched peer has no signal either"
    );
    Ok(())
}

#[test]
fn an_unknown_or_stale_pid_is_a_no_op() -> Result<()> {
    let mut table = table();
    // Nothing has ever been spawned at this number.
    table.service_signals(unslotted())?;
    assert!(table.woken().is_empty());

    // A stale pid (a freed slot's previous occupant) must not be serviced, and must not disturb
    // the process that now owns the recycled number.
    let first = table.spawn_default()?;
    table.remove(first).expect("the pid was live");
    let reused = table.spawn_default()?;
    assert_eq!(reused.number(), first.number(), "the number is recycled");
    assert!(reused.serial() > first.serial(), "with a bumped serial");
    table.get_mut(reused).expect("reused is live").set_state(ProcessState::Running);

    table.service_signals(first)?;
    assert!(table.contains(reused), "the recycled slot's occupant survives");
    assert_eq!(table.get(reused).expect("reused is live").state(), ProcessState::Running);
    assert!(table.woken().is_empty());
    Ok(())
}

#[test]
fn a_queued_signal_is_still_applied_at_the_schedule_in() -> Result<()> {
    let (mut table, exit) = table_with_atoms();
    let target = table.spawn_default()?;
    let sender = table.spawn_default()?;
    table.get_mut(target).expect("target is live").set_trap_exit(true);
    table.get_mut(target).expect("target is live").set_state(ProcessState::Running);

    let reason = make_small(11);
    // A signal for the *running* process is deferred, so it is still queued at the schedule-in.
    let effect = table.apply_exit_signal(
        target,
        ExitSignal {
            from: sender,
            reason,
        },
    )?;
    assert_eq!(effect, ExitEffect::Trapped, "the predicted effect");
    assert_eq!(table.pending_signals(target)?, 1, "deferred, not applied");

    table.service_signals(target)?;

    assert!(table.contains(target), "the trapping target survives");
    assert_eq!(table.pending_signals(target)?, 0, "the queued signal was applied");
    let payload = table
        .get(target)
        .expect("target is live")
        .mailbox()
        .peek()
        .expect("the trapped EXIT was delivered")
        .payload();
    // SAFETY: `payload` is the live `{'EXIT', Sender, Reason}` tuple the table built on the
    // target's heap; the three element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert!(is_atom(tuple_element(payload, 0)));
        assert_eq!(tuple_element(payload, 0), make_atom(exit));
        assert_eq!(tuple_element(payload, 1), sender.to_eterm());
        assert_eq!(tuple_element(payload, 2), reason);
    }
    Ok(())
}

#[test]
fn a_queued_fatal_signal_still_drives_the_whole_link_cascade() -> Result<()> {
    let (mut table, exit) = table_with_atoms();
    let head = table.spawn_default()?;
    let middle = table.spawn_default()?;
    let tail = table.spawn_default()?;
    let signaller = table.spawn_default()?;
    table.link(head, middle)?;
    table.link(middle, tail)?;
    table.get_mut(tail).expect("tail is live").set_trap_exit(true);
    // `head` is executing, so its fatal signal is queued rather than applied eagerly; `middle` and
    // `tail` are blocked and therefore serviced by the cascade itself.
    table.get_mut(head).expect("head is live").set_state(ProcessState::Running);
    table.get_mut(middle).expect("middle is live").set_state(ProcessState::Waiting);
    table.get_mut(tail).expect("tail is live").set_state(ProcessState::Waiting);

    let reason = make_small(7);
    table.apply_exit_signal(
        head,
        ExitSignal {
            from: signaller,
            reason,
        },
    )?;
    assert_eq!(table.pending_signals(head)?, 1, "the fatal signal is queued");

    // One schedule-in of `head` runs the whole chain: `head` dies, its link exit kills the
    // non-trapping `middle`, and the trapping `tail` receives `{'EXIT', Middle, Reason}`.
    table.service_signals(head)?;

    assert!(!table.contains(head), "the queued fatal signal terminated the head");
    assert!(!table.contains(middle), "the link cascade reached the middle");
    assert!(table.contains(tail), "the trapping tail survived");
    assert_eq!(
        table.pending_signals(tail)?,
        0,
        "the cascade serviced the tail rather than leaving its signal queued"
    );
    let payload = table
        .get(tail)
        .expect("tail is live")
        .mailbox()
        .peek()
        .expect("the cascaded EXIT was delivered")
        .payload();
    // SAFETY: `payload` is the live `{'EXIT', Middle, Reason}` tuple the table built on the tail's
    // heap; the three element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), make_atom(exit));
        assert_eq!(tuple_element(payload, 1), middle.to_eterm());
        assert_eq!(tuple_element(payload, 2), reason);
    }
    Ok(())
}

/// A `Waiting` receiver with `sender` delivering `payload` to it, so a wake is queued.
fn wake_pending(table: &mut ProcessTable, payload: Eterm) -> Result<(ProcessId, ProcessId)> {
    let receiver = table.spawn_default()?;
    let sender = table.spawn_default()?;
    table.get_mut(receiver).expect("receiver is live").set_state(ProcessState::Waiting);
    assert_eq!(
        table.deliver(receiver, sender.to_eterm(), payload, &OffHeapTable::new())?,
        DeliverOutcome::Woke,
        "a blocked receiver is woken by the delivery"
    );
    Ok((receiver, sender))
}

#[test]
fn drain_woken_into_hands_over_the_wake_list_and_clears_the_buffer() -> Result<()> {
    let mut table = table();
    let (receiver, sender) = wake_pending(&mut table, make_small(1))?;
    assert_eq!(table.woken(), &[receiver], "the delivery queued one wake");

    let mut scratch = vec![unslotted(), sender];
    table.drain_woken_into(&mut scratch);
    assert_eq!(
        scratch,
        vec![receiver],
        "the buffer's stale contents are cleared and the wake list moved in"
    );
    assert!(table.woken().is_empty(), "the table's wake list was emptied");

    // The same buffer is reusable: an empty sweep clears it and reports nothing.
    table.drain_woken_into(&mut scratch);
    assert!(scratch.is_empty(), "an empty sweep leaves an empty buffer");

    // The take-based form agrees on the identical scenario.
    let (receiver, _sender) = wake_pending(&mut table, make_small(2))?;
    assert_eq!(table.drain_woken(), vec![receiver]);
    assert!(table.woken().is_empty());
    assert!(table.drain_woken().is_empty(), "the wake list stays empty");
    Ok(())
}

#[test]
fn the_scheduler_wake_scratch_never_double_enqueues() -> Result<()> {
    let mut table = table();
    let mut scheduler = Scheduler::new();
    let (receiver, _sender) = wake_pending(&mut table, make_small(1))?;
    assert_eq!(scheduler.resume_woken(&mut table), 1, "the wake became a run-queue entry");
    // A second sweep reuses the scheduler's own scratch; the buffer is empty, so it finds nothing
    // and cannot enqueue a duplicate.
    assert_eq!(scheduler.resume_woken(&mut table), 0, "the scratch is empty after the sweep");

    let mut runner = Counter::default();
    assert_eq!(
        scheduler.run_slice(&mut table, &mut runner)?,
        Some(receiver),
        "the woken receiver runs exactly once"
    );
    assert_eq!(runner.runs, 1);
    assert_eq!(
        scheduler.run_slice(&mut table, &mut runner)?,
        None,
        "no duplicate entry was left behind"
    );
    assert_eq!(runner.runs, 1, "the runner was not asked to run it twice");
    Ok(())
}
