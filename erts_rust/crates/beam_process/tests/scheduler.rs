//! Integration evidence for pid allocation and the round-robin run-queue order.
//!
//! Covers CONTEXT.md Test Strategy item 1: many spawns get distinct pids; a freed pid
//! number is reused with a **bumped serial** and the stale pid stops resolving; and a
//! recording [`ProcessRunner`] shows strict round-robin scheduling (each ready process runs
//! one slice in enqueue order, requeued at the tail on `Yield`), with a `Wait` process
//! leaving the queue and never being rerun.

use beam_common::Result;
use beam_process::prelude::*;
use beam_term::prelude::AtomTable;

/// An empty table with a fresh atom table behind its signal atoms.
fn table() -> ProcessTable {
    ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
}

/// A runner that records the order processes are run and yields — except the first time it
/// sees a process listed in `wait_for`, when it reports `Wait` (blocking).
struct Recorder {
    order: Vec<ProcessId>,
    wait_for: Vec<ProcessId>,
    seen: Vec<ProcessId>,
}

impl Recorder {
    fn new(wait_for: Vec<ProcessId>) -> Recorder {
        Recorder {
            order: Vec::new(),
            wait_for,
            seen: Vec::new(),
        }
    }
}

impl ProcessRunner for Recorder {
    fn run(
        &mut self,
        pid: ProcessId,
        _table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        self.order.push(pid);
        let first_sighting = !self.seen.contains(&pid);
        if first_sighting {
            self.seen.push(pid);
        }
        if first_sighting && self.wait_for.contains(&pid) {
            return Ok(RunOutcome::Wait);
        }
        Ok(RunOutcome::Yield)
    }
}

#[test]
fn many_spawns_get_distinct_pids() {
    let mut table = table();
    let mut pids = Vec::new();
    for _ in 0..100 {
        pids.push(table.spawn_default().expect("room in the pid space"));
    }

    let mut distinct = pids.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), pids.len(), "every spawn got a distinct pid");
    assert_eq!(table.len(), 100);
}

#[test]
fn a_freed_pid_number_is_reused_with_a_bumped_serial() {
    let mut table = table();
    let first = table.spawn_default().expect("room in the pid space");
    table.remove(first).expect("the pid was live");
    assert!(!table.contains(first));

    let reused = table.spawn_default().expect("the freed number is reused");
    assert_eq!(
        reused.number(),
        first.number(),
        "the freed number is handed out again"
    );
    assert!(
        reused.serial() > first.serial(),
        "reusing a slot bumps its serial"
    );
    assert!(table.contains(reused));
    // The stale pid names the previous occupant and must no longer resolve.
    assert!(!table.contains(first));
    assert!(table.get(first).is_none());
}

#[test]
fn run_ready_schedules_round_robin_in_enqueue_order() {
    let mut table = table();
    let mut scheduler = Scheduler::new();
    let a = table.spawn_default().expect("a");
    let b = table.spawn_default().expect("b");
    let c = table.spawn_default().expect("c");
    scheduler.enqueue(a);
    scheduler.enqueue(b);
    scheduler.enqueue(c);

    let mut runner = Recorder::new(vec![]);
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(3))
            .expect("the runner never fails"),
        3
    );
    assert_eq!(runner.order, vec![a, b, c], "one slice each, in enqueue order");

    // Every yield requeues at the tail, so the next three slices repeat the order.
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(3))
            .expect("the runner never fails"),
        3
    );
    assert_eq!(runner.order, vec![a, b, c, a, b, c]);
}

#[test]
fn a_waiting_process_leaves_the_run_queue_and_is_not_rerun() {
    let mut table = table();
    let mut scheduler = Scheduler::new();
    let a = table.spawn_default().expect("a");
    let b = table.spawn_default().expect("b");
    scheduler.enqueue(a);
    scheduler.enqueue(b);

    let mut runner = Recorder::new(vec![b]);
    // `a` yields (and is requeued); `b` waits (and leaves the queue).
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(2))
            .expect("the runner never fails"),
        2
    );
    assert_eq!(runner.order, vec![a, b]);
    assert!(!scheduler.run_queue().contains(b));
    assert!(scheduler.run_queue().contains(a));

    // Only `a` keeps running afterwards.
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(2))
            .expect("the runner never fails"),
        2
    );
    assert_eq!(runner.order, vec![a, b, a, a]);
}
