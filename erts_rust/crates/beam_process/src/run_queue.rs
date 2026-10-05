//! The ready queue: [`RunQueue`].
//!
//! Milestone-1 keeps a single FIFO of runnable pids; the legacy C's max/high/normal/low
//! priority queues (`erl_process.c`, `ErtsRunQueue`) and its lock-free threaded variants
//! are a documented follow-up (see the crate `CONTEXT.md`, Known Issues).

use std::collections::VecDeque;

use crate::pid::ProcessId;

/// A FIFO of runnable process ids.
///
/// The process itself is *not* stored here — the run queue holds pids and the scheduler
/// looks the PCB up in the [`crate::ptab::ProcessTable`], so a single queue can never
/// double-own a process.
//
// TODO(impl): implement `RunQueue` (all bodies are stubs).
pub struct RunQueue {
    ready: VecDeque<ProcessId>,
}

impl RunQueue {
    /// An empty run queue.
    pub fn new() -> RunQueue {
        unimplemented!("stub — to be implemented")
    }

    /// Appends `pid` at the tail.
    pub fn enqueue(&mut self, pid: ProcessId) {
        unimplemented!("stub — to be implemented")
    }

    /// Removes and returns the pid at the head, if any.
    pub fn dequeue(&mut self) -> Option<ProcessId> {
        unimplemented!("stub — to be implemented")
    }

    /// The number of enqueued pids.
    pub fn len(&self) -> usize {
        unimplemented!("stub — to be implemented")
    }

    /// Whether the queue holds no pids.
    pub fn is_empty(&self) -> bool {
        unimplemented!("stub — to be implemented")
    }

    /// Whether `pid` is enqueued.
    pub fn contains(&self, pid: ProcessId) -> bool {
        unimplemented!("stub — to be implemented")
    }

    /// Removes `pid`, reporting whether it was present.
    pub fn remove(&mut self, pid: ProcessId) -> bool {
        unimplemented!("stub — to be implemented")
    }

    /// Iterates the enqueued pids in FIFO order.
    pub fn iter(&self) -> impl Iterator<Item = ProcessId> + '_ {
        crate::unimplemented_iter()
    }

    /// Drops every enqueued pid.
    pub fn clear(&mut self) {
        unimplemented!("stub — to be implemented")
    }
}
