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

//! Integration evidence for the **decoupled, ordered** signal queue.
//!
//! A signal generated for a process that is not currently executing is applied **eagerly**, at
//! the end of the sending operation; a signal generated for the process that *is* executing
//! (`ProcessState::Running` — the single scheduler sets it before running a process) is
//! **deferred** on its pending-signal queue and applied at its next schedule-in, in FIFO order.
//!
//! The last test pins the GC contract of the queue: a signal's `reason` lives in the
//! **receiver's** own heap (it is copied there at enqueue time), so it is an extra in-heap
//! sub-rootset that `Process::collect` must relocate — a raw word left pointing into the old
//! semispace would dangle here.
use beam_common::Result;
use beam_process::prelude::*;
use beam_term::prelude::{make_small, tuple_element, tuple_size, AtomTable, Eterm};
/// An empty table with a fresh atom table behind its signal atoms.
fn table() -> ProcessTable {
    ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
}
/// A runner that records how many times it was asked to execute a process and always yields.
///
/// It never runs a process the schedule-in servicing already terminated, which is what the
/// deferred-signal test asserts.
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
        Ok(RunOutcome::Yield)
    }
}
/// Builds a tuple on `pid`'s own heap, for use as a *structured* exit reason.
fn tuple_in(table: &mut ProcessTable, pid: ProcessId, elems: &[Eterm]) -> Eterm {
    table
        .get_mut(pid)
        .expect("the holder is live")
        .heap_mut()
        .tuple(elems)
        .expect("the tuple fits the process's young heap")
}
#[test]
fn a_signal_for_a_running_process_stays_pending_until_its_schedule_in() {
    let mut table = table();
    let target = table.spawn_default().expect("target");
    let sender = table.spawn_default().expect("sender");
    // The single scheduler marks the executing process `Running`; mimic that here.
    table
        .get_mut(target)
        .expect("target")
        .set_state(ProcessState::Running);

    let effect = table
        .apply_exit_signal(
            target,
            ExitSignal {
                from: sender,
                reason: make_small(7),
            },
        )
        .expect("a live target");
    // The effect is *predicted* for a deferred signal (the target is non-trapping here).
    assert_eq!(effect, ExitEffect::Terminated);
    assert!(
        table.contains(target),
        "a running target keeps its signal pending"
    );
    assert_eq!(table.pending_signals(target).expect("a live target"), 1);

    // Schedule-in: the pending signal is applied before the runner runs.
    let mut scheduler = Scheduler::new();
    scheduler.enqueue(target);
    let mut runner = Counter::default();
    assert_eq!(
        scheduler
            .run_slice(&mut table, &mut runner)
            .expect("one slice"),
        Some(target)
    );
    assert!(
        !table.contains(target),
        "the deferred signal terminated the target at schedule-in"
    );
    assert_eq!(runner.runs, 0, "the terminated process was never run");
}
#[test]
fn multiple_queued_signals_are_applied_in_fifo_order() {
    let mut table = table();
    let target = table.spawn_default().expect("target");
    let first = table.spawn_default().expect("first");
    let second = table.spawn_default().expect("second");
    table
        .get_mut(target)
        .expect("target")
        .set_trap_exit(true);
    table
        .get_mut(target)
        .expect("target")
        .set_state(ProcessState::Running);

    table
        .apply_exit_signal(
            target,
            ExitSignal {
                from: first,
                reason: make_small(1),
            },
        )
        .expect("a live target");
    table
        .apply_exit_signal(
            target,
            ExitSignal {
                from: second,
                reason: make_small(2),
            },
        )
        .expect("a live target");
    assert_eq!(
        table.pending_signals(target).expect("a live target"),
        2,
        "both signals are queued, not applied"
    );
    let queued: Vec<u64> = table
        .signals(target)
        .expect("a live target")
        .iter()
        .filter_map(|signal| match signal.reason() {
            reason if beam_term::prelude::is_immediate(reason) => {
                Some(beam_term::prelude::signed_value(reason) as u64)
            }
            _ => None,
        })
        .collect();
    assert_eq!(queued, vec![1, 2], "the queue preserves enqueue order");

    // Schedule-in applies them in FIFO order, so the mailbox holds them in that order too.
    let mut scheduler = Scheduler::new();
    scheduler.enqueue(target);
    let mut runner = Counter::default();
    scheduler
        .run_slice(&mut table, &mut runner)
        .expect("one slice");
    assert!(table.contains(target), "a trapping target survives");
    let mailbox = table.get(target).expect("a live target").mailbox();
    assert_eq!(mailbox.len(), 2, "both trapped exits were delivered");
    let first_message = table
        .get_mut(target)
        .expect("a live target")
        .receive(|_| true)
        .expect("the first queued signal");
    let second_message = table
        .get_mut(target)
        .expect("a live target")
        .receive(|_| true)
        .expect("the second queued signal");
    // SAFETY: both payloads are the live 3-tuples built by the signal builder.
    unsafe {
        assert_eq!(tuple_size(first_message.payload()), 3);
        assert_eq!(tuple_element(first_message.payload(), 1), first.to_eterm());
        assert_eq!(tuple_element(first_message.payload(), 2), make_small(1));
        assert_eq!(tuple_element(second_message.payload(), 1), second.to_eterm());
        assert_eq!(tuple_element(second_message.payload(), 2), make_small(2));
    }
}
#[test]
fn a_signal_for_a_non_running_process_is_applied_eagerly() {
    let mut table = table();
    let a = table.spawn_default().expect("a");
    let b = table.spawn_default().expect("b");
    table.link(a, b).expect("both pids are live");
    // `b` is not the executing process, so `a`'s death is serviced before `terminate` returns.
    table.terminate(a, make_small(7)).expect("a is live");
    assert!(!table.contains(b), "the non-running peer was terminated eagerly");
}
#[test]
fn a_queued_structured_reason_survives_the_targets_collection() {
    let mut table = table();
    let target = table.spawn_default().expect("target");
    let sender = table.spawn_default().expect("sender");
    // The reason lives in the *sender's* heap: enqueueing the signal must copy it into the
    // target's own heap, where it becomes a collector-visible root.
    let reason = tuple_in(&mut table, sender, &[make_small(1), make_small(2)]);
    table
        .get_mut(target)
        .expect("target")
        .set_state(ProcessState::Running);
    let effect = table
        .apply_exit_signal(target, ExitSignal { from: sender, reason })
        .expect("a live target");
    assert_eq!(effect, ExitEffect::Terminated);
    assert_eq!(table.pending_signals(target).expect("a live target"), 1);

    // A safepoint collection relocates the queued reason with the rest of the root set.
    let _ = table
        .get_mut(target)
        .expect("target")
        .collect()
        .expect("a collection of the target's heap");

    let queued = table
        .signals(target)
        .expect("a live target")
        .iter()
        .next()
        .expect("one pending signal")
        .reason();
    // SAFETY: the queued reason is the live 2-tuple copied into the target's own heap.
    unsafe {
        assert_eq!(tuple_size(queued), 2);
        assert_eq!(tuple_element(queued, 0), make_small(1));
        assert_eq!(tuple_element(queued, 1), make_small(2));
    }
    assert_eq!(
        table.pending_signals(target).expect("a live target"),
        1,
        "the signal still awaits its schedule-in"
    );
}
