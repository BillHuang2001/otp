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

//! The **full spawn-option set** this crate is responsible for, composed end-to-end and asserted
//! on the resulting process.
//!
//! `erlang:spawn_opt/2,4` is a `beam_bifs`/`beam_rt` concern, but every option it carries lands in
//! a `beam_process` accessor: the pid, the heap-size policy (`{min_heap_size, _}`,
//! `{min_bin_vheap_size, _}`, `{fullsweep_after, _}`), the scheduling `{priority, _}`, the
//! `trap_exit` flag and the `{link, _}`/`{monitor, _}` relationships. This file pins that
//! composition against the public API and is the contract the sibling nodes adopt.
//!
//! # The honest observable of an applied heap policy
//!
//! `Process::collector()` returns the [`beam_gc::Collector`] the process owns, and
//! `Collector::policy()` hands back the **exact** [`HeapPolicy`] the collector was built with. That
//! is the read-only, truthful proof that the spawn-option policy reached the collector (there is no
//! other public accessor for the applied policy, and `min_heap_size` has no observable *value* on a
//! fresh process beyond it). The policy's effect is also visible *behaviourally*: the collector's
//! own minor-GC count against its own policy decides the fullsweep cadence
//! ([`HeapPolicy::fullsweep_due`]), and the collector's initial spaces are sized from the same
//! policy's `initial_young_words`/`initial_mature_words`.

use beam_common::Result;
use beam_gc::HeapPolicy;
use beam_process::prelude::*;
use beam_term::prelude::AtomTable;

/// An empty table with a fresh atom table behind its signal atoms.
fn table() -> ProcessTable {
    ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
}

// ---------------------------------------------------------------------------
// beam_rt / beam_bifs adopt: the exact spawn_opt composition
// ---------------------------------------------------------------------------
//
// `beam_rt` / `beam_bifs` adopt (`erlang:spawn_opt/2,4` -> PCB):
//
//   1. let pid = table.alloc_pid()?;
//   2. let policy = HeapPolicy::from_spawn_opts(min_heap_size,
//          min_bin_vheap_size, fullsweep_after);
//   3. let process = Process::with_policy_and_priority(pid, policy, priority)?;
//   4. table.insert(process)?;
//   5. scheduler.enqueue_with_priority(pid, priority);   // {priority, P}
//      {link, Caller}    -> after step 4: table.link(caller, pid)?;
//      {monitor, Caller} -> after step 4: table.monitor(caller, pid, MonitorKind::Process)?;
//
// The pid is invisible to every scheduler until `insert`, so a process born at a non-normal
// priority is NEVER observable at the wrong level: nothing can requeue it before step 5.
// `ProcessTable::spawn`/`spawn_default` hard-set `Priority::Normal`, so a `spawn_opt` carrying
// `{priority, P}` must compose the five steps itself rather than calling `spawn`.
//
// `{monitor, Port}` is the one option that does NOT go through `ProcessTable::monitor`: a port
// target is a `PortId`, whose monitor is composed against `PortTable`/`ProcessTable::port_closed`
// (the port's incoming half), not a `ProcessId` monitor. `{trap_exit, B}` is `Process::set_trap_exit`.

/// `alloc_pid` + `with_policy_and_priority` + `insert` + `enqueue_with_priority` produce a process
/// born at the requested priority with `trap_exit` defaulting to `false`.
#[test]
fn spawn_options_compose_a_born_at_priority_process() {
    let mut table = table();
    let mut scheduler = Scheduler::new();

    let pid = table.alloc_pid().expect("a pid is available");
    let policy = HeapPolicy::from_spawn_opts(10_000, 20_000, 10);
    let process = Process::with_policy_and_priority(pid, policy, Priority::Low)
        .expect("the policy's initial spaces allocate");
    table.insert(process).expect("the pid is in range");
    scheduler.enqueue_with_priority(pid, Priority::Low);

    let pcb = table.get(pid).expect("the process is live");
    assert_eq!(pcb.pid(), pid);
    assert_eq!(pcb.priority(), Priority::Low, "born at the requested priority");
    assert_eq!(pcb.state(), ProcessState::Runnable, "schedulable");
    assert!(!pcb.is_trapping_exit(), "trap_exit defaults to false");
    assert_eq!(
        scheduler.run_queue().priority_of(pid),
        Some(Priority::Low),
        "enqueued at the requested priority"
    );
}

/// The heap-size policy from the spawn options reaches the collector, and the collector's spaces
/// are sized from that same policy.
#[test]
fn the_heap_policy_reaches_the_collector() {
    let mut table = table();
    let pid = table.alloc_pid().expect("a pid is available");
    // Both sizes are above their respective floors, so both are rounded through the growth table.
    let policy = HeapPolicy::from_spawn_opts(10_000, 100_000, 10);
    let process = Process::with_policy_and_priority(pid, policy.clone(), Priority::Normal)
        .expect("the policy's initial spaces allocate");
    table.insert(process).expect("the pid is in range");

    let collector = table.get(pid).expect("the process is live").collector();

    // The honest read-only observable: the collector hands back the EXACT policy it was built with.
    assert_eq!(
        collector.policy(),
        &policy,
        "the spawn-option policy reached the collector verbatim"
    );

    // ... and the policy's own fields are the rounded spawn options.
    assert_eq!(policy.min_heap_words, beam_gc::next_heap_size(10_000, 0));
    assert_eq!(
        policy.min_bin_vheap_words,
        beam_gc::next_heap_size(100_000, 0)
    );
    assert_eq!(policy.fullsweep_after, 10);

    // The initial spaces are sized from the same policy.
    assert_eq!(collector.young().capacity_words(), policy.initial_young_words);
    assert_eq!(
        collector.mature().capacity_words(),
        policy.initial_mature_words
    );
}

/// The applied policy is the collector's **live** policy: the collector's own minor-GC count
/// against its own policy decides the fullsweep cadence.
#[test]
fn the_applied_policy_drives_the_collectors_fullsweep_cadence() {
    let mut table = table();
    let pid = table.alloc_pid().expect("a pid is available");
    // `fullsweep_after = 0` is the oracle's "a fullsweep on every collection".
    let process = Process::with_policy_and_priority(
        pid,
        HeapPolicy::from_spawn_opts(233, 20_000, 0),
        Priority::Normal,
    )
    .expect("the policy's initial spaces allocate");
    table.insert(process).expect("the pid is in range");

    let collector = table.get(pid).expect("the process is live").collector();
    assert_eq!(collector.minor_gcs(), 0);

    // The collector's OWN state (its minor-GC count) against its OWN policy: due immediately.
    assert!(
        collector.policy().fullsweep_due(collector.minor_gcs()),
        "a 0 fullsweep cadence is due at once"
    );

    // The default cadence is NOT due at the same point -- so the difference is the applied policy.
    let default_policy = HeapPolicy::default();
    assert_eq!(default_policy.fullsweep_after, 65_535);
    assert!(!default_policy.fullsweep_due(collector.minor_gcs()));
}

/// The spawn options apply the C `H_MIN_SIZE` rounding rules: a sub-minimum request is replaced by
/// the minimum, anything else is rounded up through the curated growth table to a fixed point.
#[test]
fn spawn_options_apply_the_c_minimum_heap_rounding() {
    use beam_gc::{next_heap_size, DEFAULT_MIN_HEAP_WORDS};

    // A request below the minimum is replaced by it, not rounded past it.
    let floored = HeapPolicy::from_spawn_opts(0, 0, 100);
    assert_eq!(floored.min_heap_words, DEFAULT_MIN_HEAP_WORDS);
    assert_eq!(floored.min_heap_words, 233);

    // A request at/above the minimum is rounded up to a growth-table fixed point.
    let rounded = HeapPolicy::from_spawn_opts(10_000, 0, 100);
    assert_eq!(rounded.min_heap_words, next_heap_size(10_000, 0));
    assert!(
        rounded.min_heap_words >= 10_000,
        "rounding never shrinks below the request"
    );
    assert_eq!(
        next_heap_size(rounded.min_heap_words, 0),
        rounded.min_heap_words,
        "the stored minimum is a fixed point of the growth table"
    );

    // `min_bin_vheap_words` follows the same rule against the C default.
    let binary = HeapPolicy::from_spawn_opts(233, 0, 100);
    assert_eq!(
        binary.min_bin_vheap_words,
        HeapPolicy::default().min_bin_vheap_words
    );
    assert_eq!(binary.min_bin_vheap_words, 46_422, "the oracle's min_bin_vheap_size");

    // `fullsweep_after` is stored verbatim.
    assert_eq!(rounded.fullsweep_after, 100);
}

/// `{link, Caller}` and `{monitor, Caller}` compose through the process table for a live target.
#[test]
fn link_and_monitor_options_compose_through_the_table() {
    let mut table = table();
    let caller = table.spawn_default().expect("the spawning process");
    let pid = table.alloc_pid().expect("a pid is available");
    let process = Process::with_policy_and_priority(pid, HeapPolicy::default(), Priority::Normal)
        .expect("the policy's initial spaces allocate");
    table.insert(process).expect("the pid is in range");

    // {link, Caller} -- symmetric.
    table.link(caller, pid).expect("both pids are live");
    assert!(table.get(caller).expect("live").links().contains(pid));
    assert!(table.get(pid).expect("live").links().contains(caller));

    // {monitor, Caller} -- outgoing on the caller, incoming on the target.
    let reference = table
        .monitor(caller, pid, MonitorKind::Process)
        .expect("both pids are live");
    let outgoing = table
        .get(caller)
        .expect("live")
        .monitors()
        .find(reference)
        .expect("the outgoing half is recorded");
    assert_eq!(outgoing.target, pid);
    assert_eq!(outgoing.kind, MonitorKind::Process);

    let incoming = table.get(pid).expect("live").monitored_by();
    assert_eq!(incoming.len(), 1);
    assert_eq!(incoming[0].watcher, caller);
    assert_eq!(incoming[0].reference, reference);
}

/// A runner that records the order processes are run and then **blocks** (leaves the run queue).
struct BlockingRecorder {
    order: Vec<ProcessId>,
}

impl ProcessRunner for BlockingRecorder {
    fn run(
        &mut self,
        pid: ProcessId,
        _table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        self.order.push(pid);
        Ok(RunOutcome::Wait)
    }
}

/// The `{priority, P}` spawn option, carried by `enqueue_with_priority`, determines the run order:
/// a NORMAL spawn-option process precedes a LOW one even when enqueued second.
#[test]
fn enqueue_with_priority_carries_the_born_priority_into_the_run_order() {
    let mut table = table();
    let mut scheduler = Scheduler::new();

    let low = table.alloc_pid().expect("low pid");
    let process = Process::with_policy_and_priority(low, HeapPolicy::default(), Priority::Low)
        .expect("spaces allocate");
    table.insert(process).expect("in range");

    let normal = table.alloc_pid().expect("normal pid");
    let process = Process::with_policy_and_priority(normal, HeapPolicy::default(), Priority::Normal)
        .expect("spaces allocate");
    table.insert(process).expect("in range");

    // Enqueue lowest-precedence first: the level, not insertion order, must win.
    scheduler.enqueue_with_priority(low, Priority::Low);
    scheduler.enqueue_with_priority(normal, Priority::Normal);

    let mut runner = BlockingRecorder { order: Vec::new() };
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(2))
            .expect("the runner never fails"),
        2
    );
    assert_eq!(
        runner.order,
        vec![normal, low],
        "the NORMAL spawn-option process runs before the LOW one"
    );
}
