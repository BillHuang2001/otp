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

//! Bidirectional links: [`Links`].
//!
//! A link is symmetric: `A` linking `B` puts `B` in `A`'s link set and `A` in `B`'s. The
//! [`crate::ptab::ProcessTable`] performs the two-sided update; each `Process` only holds
//! its own half.
//!
//! Milestone-1 keeps the set semantics of `erlang:link/1`/`erlang:unlink/1` for local
//! processes: re-linking an already-linked pair is a no-op and unlinking removes once. The
//! legacy C refcounts each link and also carries the node part of a distributed link
//! (`erl_monitor_link.c`) — a documented follow-up.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_monitor_link.c`.

use crate::pid::ProcessId;

/// The set of processes linked to one process.
pub struct Links {
    pids: Vec<ProcessId>,
}

impl Links {
    /// An empty link set.
    pub fn new() -> Links {
        Links { pids: Vec::new() }
    }

    /// The number of links.
    pub fn len(&self) -> usize {
        self.pids.len()
    }

    /// Whether the process has no links.
    pub fn is_empty(&self) -> bool {
        self.pids.is_empty()
    }

    /// Adds `pid`, returning `false` when it was already linked (the C refcount case that
    /// Milestone-1 collapses to a set).
    pub fn add(&mut self, pid: ProcessId) -> bool {
        if self.pids.contains(&pid) {
            return false;
        }
        self.pids.push(pid);
        true
    }

    /// Removes `pid`, reporting whether it was linked.
    pub fn remove(&mut self, pid: ProcessId) -> bool {
        match self.pids.iter().position(|&linked| linked == pid) {
            Some(index) => {
                self.pids.remove(index);
                true
            }
            None => false,
        }
    }

    /// Whether `pid` is linked.
    pub fn contains(&self, pid: ProcessId) -> bool {
        self.pids.contains(&pid)
    }

    /// Iterates the linked pids.
    pub fn iter(&self) -> impl Iterator<Item = ProcessId> + '_ {
        self.pids.iter().copied()
    }

    /// Drops every link.
    pub fn clear(&mut self) {
        self.pids.clear();
    }

    /// Takes every linked pid, leaving the set empty.
    pub fn drain(&mut self) -> Vec<ProcessId> {
        core::mem::take(&mut self.pids)
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for `add` idempotence, `remove`, `contains`, iteration and `drain`.

    use super::*;

    fn pid(number: u64) -> ProcessId {
        ProcessId::new(number, 0)
    }

    #[test]
    fn add_is_idempotent_and_remove_reports_presence() {
        let mut links = Links::new();
        assert!(links.is_empty());
        assert!(!links.contains(pid(1)));

        assert!(links.add(pid(1)), "the first link is new");
        assert!(!links.add(pid(1)), "re-linking is a no-op");
        assert_eq!(links.len(), 1);
        assert!(links.contains(pid(1)));

        assert!(links.remove(pid(1)));
        assert!(!links.remove(pid(1)));
        assert!(links.is_empty());
    }

    #[test]
    fn iter_and_drain_reflect_the_set() {
        let mut links = Links::new();
        links.add(pid(1));
        links.add(pid(2));
        assert_eq!(links.iter().collect::<Vec<_>>(), vec![pid(1), pid(2)]);

        let drained = links.drain();
        assert_eq!(drained, vec![pid(1), pid(2)]);
        assert!(links.is_empty());

        links.add(pid(3));
        links.clear();
        assert!(links.is_empty());
        assert_eq!(links.iter().count(), 0);
    }
}
