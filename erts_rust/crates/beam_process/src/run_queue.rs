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
pub struct RunQueue {
    ready: VecDeque<ProcessId>,
}

impl RunQueue {
    /// An empty run queue.
    pub fn new() -> RunQueue {
        RunQueue {
            ready: VecDeque::new(),
        }
    }

    /// Appends `pid` at the tail.
    pub fn enqueue(&mut self, pid: ProcessId) {
        self.ready.push_back(pid);
    }

    /// Removes and returns the pid at the head, if any.
    pub fn dequeue(&mut self) -> Option<ProcessId> {
        self.ready.pop_front()
    }

    /// The number of enqueued pids.
    pub fn len(&self) -> usize {
        self.ready.len()
    }

    /// Whether the queue holds no pids.
    pub fn is_empty(&self) -> bool {
        self.ready.is_empty()
    }

    /// Whether `pid` is enqueued.
    pub fn contains(&self, pid: ProcessId) -> bool {
        self.ready.contains(&pid)
    }

    /// Removes `pid`, reporting whether it was present.
    pub fn remove(&mut self, pid: ProcessId) -> bool {
        match self.ready.iter().position(|&queued| queued == pid) {
            Some(index) => {
                self.ready.remove(index);
                true
            }
            None => false,
        }
    }

    /// Iterates the enqueued pids in FIFO order.
    pub fn iter(&self) -> impl Iterator<Item = ProcessId> + '_ {
        self.ready.iter().copied()
    }

    /// Drops every enqueued pid.
    pub fn clear(&mut self) {
        self.ready.clear();
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the FIFO order and the `contains`/`remove` membership operations.

    use super::*;

    fn pid(number: u64) -> ProcessId {
        ProcessId::new(number, 0)
    }

    #[test]
    fn fresh_queue_is_empty() {
        let queue = RunQueue::new();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.iter().count(), 0);
    }

    #[test]
    fn fifo_order_is_preserved() {
        let mut queue = RunQueue::new();
        queue.enqueue(pid(1));
        queue.enqueue(pid(2));
        queue.enqueue(pid(3));

        assert_eq!(queue.len(), 3);
        assert_eq!(queue.iter().collect::<Vec<_>>(), vec![pid(1), pid(2), pid(3)]);
        assert_eq!(queue.dequeue(), Some(pid(1)));
        assert_eq!(queue.dequeue(), Some(pid(2)));
        assert_eq!(queue.dequeue(), Some(pid(3)));
        assert_eq!(queue.dequeue(), None);
        assert!(queue.is_empty());
    }

    #[test]
    fn contains_and_remove_track_membership() {
        let mut queue = RunQueue::new();
        queue.enqueue(pid(1));
        queue.enqueue(pid(2));

        assert!(queue.contains(pid(1)));
        assert!(!queue.contains(pid(9)));
        assert!(queue.remove(pid(1)));
        assert!(!queue.remove(pid(1)), "removing twice reports the second miss");
        assert!(!queue.contains(pid(1)));
        assert_eq!(queue.iter().collect::<Vec<_>>(), vec![pid(2)]);

        queue.clear();
        assert!(queue.is_empty());
        assert!(!queue.contains(pid(2)));
    }
}
