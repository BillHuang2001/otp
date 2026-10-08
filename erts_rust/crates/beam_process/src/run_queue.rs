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

//! The ready queue: [`RunQueue`] and the four scheduling [`Priority`] levels.
//!
//! The run queue holds one FIFO per ERTS scheduling priority (`max`, `high`, `normal`,
//! `low`) and [`RunQueue::dequeue`] always serves the highest-priority non-empty FIFO
//! first, mirroring the legacy C `pick_next_process` (`erl_process.c`).
//!
//! # Fairness deviation from the legacy C
//!
//! The C keeps **MAX** and **HIGH** on dedicated physical queues but lets **NORMAL and LOW
//! share one physical queue**; a LOW process carries a `schedule_count` (`RESCHEDULE_LOW =
//! 8`, `erl_process.h`) that defers it to the tail of that shared queue up to eight times
//! before it is picked, so LOW and NORMAL processes interleave. This crate deliberately
//! approximates that with **four physically distinct FIFOs**: a LOW process therefore runs
//! only once the MAX/HIGH/NORMAL queues are all empty and never interleaves with NORMAL
//! processes inside a shared queue. That is a documented approximation — the observable
//! difference is the interleaving ratio between LOW and NORMAL, not their relative
//! precedence (LOW still runs last).
//!
//! The legacy C's lock-free threaded run-queue variants remain a documented follow-up (see
//! the crate `CONTEXT.md`, Known Issues).

use std::collections::VecDeque;

use crate::pid::ProcessId;

/// The four ERTS scheduling-priority levels (the legacy C `PRIORITY_*`).
///
/// The numeric order is the C encoding: a *lower* index is scheduled *first*, so the
/// derived `Ord` gives `Max < High < Normal < Low` (do NOT read that as scheduling
/// precedence — that ordering is what [`RunQueue`] implements by iterating [`Priority::LEVELS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Priority {
    /// The `max` priority (`PRIORITY_MAX`) — scheduled first.
    Max = 0,
    /// The `high` priority (`PRIORITY_HIGH`).
    High = 1,
    /// The `normal` priority (`PRIORITY_NORMAL`) — the default.
    Normal = 2,
    /// The `low` priority (`PRIORITY_LOW`) — scheduled last.
    Low = 3,
}

impl Priority {
    /// The four levels in scheduling precedence order (highest first):
    /// `[Max, High, Normal, Low]`.
    pub const LEVELS: [Priority; 4] = [
        Priority::Max,
        Priority::High,
        Priority::Normal,
        Priority::Low,
    ];

    /// The C numeric value (`PRIORITY_MAX` = 0 … `PRIORITY_LOW` = 3).
    pub const fn index(self) -> usize {
        self as u8 as usize
    }

    /// The priority with C numeric value `index`, or `None` when out of range.
    pub const fn from_index(index: u8) -> Option<Priority> {
        match index {
            0 => Some(Priority::Max),
            1 => Some(Priority::High),
            2 => Some(Priority::Normal),
            3 => Some(Priority::Low),
            _ => None,
        }
    }

    /// The atom name of this priority (`"max" | "high" | "normal" | "low"`).
    pub const fn name(self) -> &'static str {
        match self {
            Priority::Max => "max",
            Priority::High => "high",
            Priority::Normal => "normal",
            Priority::Low => "low",
        }
    }

    /// Parses the atom name of a priority (`"max" | "high" | "normal" | "low"`), or `None`.
    pub fn from_name(name: &str) -> Option<Priority> {
        match name {
            "max" => Some(Priority::Max),
            "high" => Some(Priority::High),
            "normal" => Some(Priority::Normal),
            "low" => Some(Priority::Low),
            _ => None,
        }
    }
}

impl Default for Priority {
    /// [`Priority::Normal`] — a freshly spawned process's priority.
    fn default() -> Priority {
        Priority::Normal
    }
}

/// A FIFO of runnable process ids.
///
/// Internally this is one FIFO per [`Priority`] level; [`RunQueue::dequeue`] takes the front
/// of the highest-priority non-empty queue. The process itself is *not* stored here — the
/// run queue holds pids and the scheduler looks the PCB up in the
/// [`crate::ptab::ProcessTable`], so a single queue can never double-own a process.
pub struct RunQueue {
    /// One ready FIFO per priority, indexed by [`Priority::index`].
    queues: [VecDeque<ProcessId>; 4],
}

impl RunQueue {
    /// An empty run queue.
    pub fn new() -> RunQueue {
        RunQueue {
            queues: [
                VecDeque::new(),
                VecDeque::new(),
                VecDeque::new(),
                VecDeque::new(),
            ],
        }
    }

    /// Appends `pid` at the tail of the **normal**-priority FIFO.
    pub fn enqueue(&mut self, pid: ProcessId) {
        self.enqueue_with_priority(pid, Priority::Normal);
    }

    /// Appends `pid` at the tail of the FIFO for `priority`.
    pub fn enqueue_with_priority(&mut self, pid: ProcessId, priority: Priority) {
        self.queues[priority.index()].push_back(pid);
    }

    /// Removes and returns the pid at the head of the highest-priority non-empty FIFO,
    /// if any (MAX → HIGH → NORMAL → LOW).
    pub fn dequeue(&mut self) -> Option<ProcessId> {
        self.dequeue_with_priority().map(|(pid, _)| pid)
    }

    /// Removes and returns the pid at the head of the highest-priority non-empty FIFO
    /// together with the [`Priority`] level it came from, if any.
    pub fn dequeue_with_priority(&mut self) -> Option<(ProcessId, Priority)> {
        for priority in Priority::LEVELS {
            if let Some(pid) = self.queues[priority.index()].pop_front() {
                return Some((pid, priority));
            }
        }
        None
    }

    /// The total number of enqueued pids across all four levels.
    pub fn len(&self) -> usize {
        self.queues.iter().map(VecDeque::len).sum()
    }

    /// The number of enqueued pids at `priority`.
    pub fn len_at(&self, priority: Priority) -> usize {
        self.queues[priority.index()].len()
    }

    /// Whether the queue holds no pids at any level.
    pub fn is_empty(&self) -> bool {
        self.queues.iter().all(VecDeque::is_empty)
    }

    /// Whether `pid` is enqueued at any level.
    pub fn contains(&self, pid: ProcessId) -> bool {
        self.priority_of(pid).is_some()
    }

    /// The level holding `pid`, or `None` when it is not enqueued.
    pub fn priority_of(&self, pid: ProcessId) -> Option<Priority> {
        Priority::LEVELS
            .into_iter()
            .find(|&priority| self.queues[priority.index()].contains(&pid))
    }

    /// Removes `pid` from whichever level holds it, reporting whether it was present.
    pub fn remove(&mut self, pid: ProcessId) -> bool {
        match self.priority_of(pid) {
            Some(priority) => {
                let queue = &mut self.queues[priority.index()];
                if let Some(index) = queue.iter().position(|&queued| queued == pid) {
                    queue.remove(index);
                }
                true
            }
            None => false,
        }
    }

    /// Moves `pid` to the tail of `priority`'s FIFO **iff** it is currently enqueued at a
    /// different level, reporting whether it was moved.
    ///
    /// This is the run-queue half of a priority change (`process_flag(priority, P)` /
    /// `spawn_opt(..., [{priority, P}])`). A pid that is **not** enqueued at all (it is running,
    /// in flight, waiting, suspended or terminated) or is already at `priority` is left
    /// untouched, so this never double-enqueues and never inserts a pid that was not queued.
    pub fn reprioritize(&mut self, pid: ProcessId, priority: Priority) -> bool {
        match self.priority_of(pid) {
            Some(current) if current != priority => {
                self.remove(pid);
                self.enqueue_with_priority(pid, priority);
                true
            }
            _ => false,
        }
    }
    /// Iterates every enqueued pid in the order [`RunQueue::dequeue`] would return them
    /// (MAX, then HIGH, then NORMAL, then LOW, FIFO within each level).
    pub fn iter(&self) -> impl Iterator<Item = ProcessId> + '_ {
        self.queues.iter().flat_map(|queue| queue.iter().copied())
    }

    /// Iterates the pids enqueued at `priority`, in FIFO order.
    pub fn iter_at(&self, priority: Priority) -> impl Iterator<Item = ProcessId> + '_ {
        self.queues[priority.index()].iter().copied()
    }

    /// Removes and returns up to half of this queue's pids (so a work-stealing victim keeps at
    /// least half), serving the highest priority first. The returned pids are in the order
    /// [`RunQueue::dequeue`] would have returned them. A queue of 0 or 1 pids yields an empty
    /// `Vec`.
    ///
    /// This is intended as the victim half of a multi-scheduler work-stealing steal: the
    /// returned pids can be handed to another scheduler's run queue, and the remainder stays
    /// here for the victim to serve.
    pub fn split_off_half(&mut self) -> Vec<ProcessId> {
        // `take <= len`, so the `dequeue`s below cannot run dry; the `Some` guard is the
        // total, never-panicking form of an invariant that holds by construction.
        let take = self.len() / 2;
        let mut stolen = Vec::with_capacity(take);
        for _ in 0..take {
            if let Some(pid) = self.dequeue() {
                stolen.push(pid);
            }
        }
        stolen
    }
    /// Drops every enqueued pid, at every level.
    pub fn clear(&mut self) {
        for queue in &mut self.queues {
            queue.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the FIFO order, the four-level priority selection and the
    //! `contains`/`remove` membership operations.

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
        for priority in Priority::LEVELS {
            assert_eq!(queue.len_at(priority), 0);
        }
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

    #[test]
    fn enqueue_defaults_to_normal() {
        let mut queue = RunQueue::new();
        queue.enqueue(pid(1));
        assert_eq!(queue.priority_of(pid(1)), Some(Priority::Normal));
        assert_eq!(queue.len_at(Priority::Normal), 1);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn dequeue_serves_the_highest_priority_first() {
        let mut queue = RunQueue::new();
        // Enqueue in "lowest precedence first" order to prove the level wins, not insertion.
        queue.enqueue_with_priority(pid(1), Priority::Low);
        queue.enqueue_with_priority(pid(2), Priority::Normal);
        queue.enqueue_with_priority(pid(3), Priority::High);
        queue.enqueue_with_priority(pid(4), Priority::Max);

        assert_eq!(queue.dequeue_with_priority(), Some((pid(4), Priority::Max)));
        assert_eq!(queue.dequeue_with_priority(), Some((pid(3), Priority::High)));
        assert_eq!(
            queue.dequeue_with_priority(),
            Some((pid(2), Priority::Normal))
        );
        assert_eq!(queue.dequeue_with_priority(), Some((pid(1), Priority::Low)));
        assert_eq!(queue.dequeue_with_priority(), None);
    }

    #[test]
    fn fifo_within_each_level() {
        let mut queue = RunQueue::new();
        queue.enqueue_with_priority(pid(1), Priority::High);
        queue.enqueue_with_priority(pid(2), Priority::High);
        queue.enqueue_with_priority(pid(3), Priority::High);

        assert_eq!(queue.len_at(Priority::High), 3);
        assert_eq!(
            queue.iter_at(Priority::High).collect::<Vec<_>>(),
            vec![pid(1), pid(2), pid(3)]
        );
        assert_eq!(queue.dequeue(), Some(pid(1)));
        assert_eq!(queue.dequeue(), Some(pid(2)));
        assert_eq!(queue.dequeue(), Some(pid(3)));
        assert!(queue.is_empty());
    }

    #[test]
    fn iter_walks_every_level_in_scheduling_order() {
        let mut queue = RunQueue::new();
        queue.enqueue_with_priority(pid(1), Priority::Low);
        queue.enqueue_with_priority(pid(2), Priority::Normal);
        queue.enqueue_with_priority(pid(3), Priority::Normal);
        queue.enqueue_with_priority(pid(4), Priority::Max);

        assert_eq!(
            queue.iter().collect::<Vec<_>>(),
            vec![pid(4), pid(2), pid(3), pid(1)],
            "MAX first, then NORMAL in FIFO order, then LOW"
        );
    }

    #[test]
    fn priority_of_and_remove_span_all_levels() {
        let mut queue = RunQueue::new();
        queue.enqueue_with_priority(pid(1), Priority::Max);
        queue.enqueue_with_priority(pid(2), Priority::Normal);
        queue.enqueue_with_priority(pid(3), Priority::Low);

        assert_eq!(queue.priority_of(pid(1)), Some(Priority::Max));
        assert_eq!(queue.priority_of(pid(2)), Some(Priority::Normal));
        assert_eq!(queue.priority_of(pid(3)), Some(Priority::Low));
        assert_eq!(queue.priority_of(pid(9)), None);

        assert!(queue.remove(pid(1)));
        assert!(queue.remove(pid(3)));
        assert!(!queue.remove(pid(1)));
        assert_eq!(queue.priority_of(pid(1)), None);
        assert_eq!(queue.priority_of(pid(3)), None);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.iter().collect::<Vec<_>>(), vec![pid(2)]);

        queue.clear();
        assert!(queue.is_empty());
        assert_eq!(queue.priority_of(pid(2)), None);
    }

    #[test]
    fn split_off_half_keeps_at_least_half() {
        // An empty queue has nothing to steal and is unchanged.
        let mut empty = RunQueue::new();
        assert!(empty.split_off_half().is_empty());
        assert!(empty.is_empty());

        // A single-pid queue is not split — the victim keeps its only process.
        let mut single = RunQueue::new();
        single.enqueue(pid(1));
        assert!(single.split_off_half().is_empty());
        assert_eq!(single.iter().collect::<Vec<_>>(), vec![pid(1)]);

        // Four pids on a uniform priority: two are stolen (FIFO, as `dequeue` serves them)
        // and two stay; the total is preserved.
        let mut four = RunQueue::new();
        for number in 1..=4 {
            four.enqueue(pid(number));
        }
        let stolen = four.split_off_half();
        assert_eq!(stolen, vec![pid(1), pid(2)]);
        assert_eq!(four.iter().collect::<Vec<_>>(), vec![pid(3), pid(4)]);
        assert_eq!(stolen.len() + four.len(), 4, "the total is preserved");

        // Five pids: half rounds down, so two are stolen and three stay.
        let mut five = RunQueue::new();
        for number in 1..=5 {
            five.enqueue(pid(number));
        }
        let stolen = five.split_off_half();
        assert_eq!(stolen, vec![pid(1), pid(2)]);
        assert_eq!(
            five.iter().collect::<Vec<_>>(),
            vec![pid(3), pid(4), pid(5)]
        );
        assert_eq!(stolen.len() + five.len(), 5);

        // A mixed-priority queue steals the highest-precedence pids first.
        let mut mixed = RunQueue::new();
        mixed.enqueue_with_priority(pid(1), Priority::Low);
        mixed.enqueue_with_priority(pid(2), Priority::Normal);
        mixed.enqueue_with_priority(pid(3), Priority::Max);
        mixed.enqueue_with_priority(pid(4), Priority::Normal);
        assert_eq!(mixed.split_off_half(), vec![pid(3), pid(2)]);
        assert_eq!(mixed.iter().collect::<Vec<_>>(), vec![pid(4), pid(1)]);
    }

    #[test]
    fn reprioritize_moves_only_a_differently_queued_pid() {
        let mut queue = RunQueue::new();
        queue.enqueue_with_priority(pid(1), Priority::Normal);
        queue.enqueue_with_priority(pid(2), Priority::Normal);

        // A promotion moves the pid to the tail of the new level; the total is unchanged.
        assert!(queue.reprioritize(pid(1), Priority::High));
        assert_eq!(queue.priority_of(pid(1)), Some(Priority::High));
        assert_eq!(queue.len(), 2, "no duplicate is created");

        // Already at the requested level: nothing to do (and no duplicate).
        assert!(!queue.reprioritize(pid(1), Priority::High));
        assert_eq!(queue.priority_of(pid(1)), Some(Priority::High));
        assert_eq!(queue.len(), 2);

        // A pid that is not queued is never inserted.
        assert!(!queue.reprioritize(pid(9), Priority::Max));
        assert_eq!(queue.priority_of(pid(9)), None);
        assert_eq!(queue.len(), 2);

        // The HIGH pid is now served before the NORMAL one.
        assert_eq!(queue.dequeue(), Some(pid(1)));
        assert_eq!(queue.dequeue(), Some(pid(2)));

        // A demotion also moves.
        let mut queue = RunQueue::new();
        queue.enqueue_with_priority(pid(3), Priority::Max);
        assert!(queue.reprioritize(pid(3), Priority::Low));
        assert_eq!(queue.priority_of(pid(3)), Some(Priority::Low));
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn priority_names_and_indices_round_trip() {
        assert_eq!(Priority::default(), Priority::Normal);
        assert_eq!(Priority::LEVELS, [Priority::Max, Priority::High, Priority::Normal, Priority::Low]);
        for priority in Priority::LEVELS {
            assert_eq!(Priority::from_index(priority.index() as u8), Some(priority));
            assert_eq!(Priority::from_name(priority.name()), Some(priority));
        }
        assert_eq!(Priority::Max.index(), 0);
        assert_eq!(Priority::Low.index(), 3);
        assert_eq!(Priority::from_index(4), None);
        assert_eq!(Priority::from_name("bogus"), None);
        assert!(Priority::Max < Priority::Low, "Ord follows the C numeric encoding");
    }
}
