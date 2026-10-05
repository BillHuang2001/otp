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
//
// TODO(impl): implement `Links` (all bodies are stubs).
pub struct Links {
    pids: Vec<ProcessId>,
}

impl Links {
    /// An empty link set.
    pub fn new() -> Links {
        unimplemented!("stub — to be implemented")
    }

    /// The number of links.
    pub fn len(&self) -> usize {
        unimplemented!("stub — to be implemented")
    }

    /// Whether the process has no links.
    pub fn is_empty(&self) -> bool {
        unimplemented!("stub — to be implemented")
    }

    /// Adds `pid`, returning `false` when it was already linked (the C refcount case that
    /// Milestone-1 collapses to a set).
    pub fn add(&mut self, pid: ProcessId) -> bool {
        unimplemented!("stub — to be implemented")
    }

    /// Removes `pid`, reporting whether it was linked.
    pub fn remove(&mut self, pid: ProcessId) -> bool {
        unimplemented!("stub — to be implemented")
    }

    /// Whether `pid` is linked.
    pub fn contains(&self, pid: ProcessId) -> bool {
        unimplemented!("stub — to be implemented")
    }

    /// Iterates the linked pids.
    pub fn iter(&self) -> impl Iterator<Item = ProcessId> + '_ {
        crate::unimplemented_iter()
    }

    /// Drops every link.
    pub fn clear(&mut self) {
        unimplemented!("stub — to be implemented")
    }

    /// Takes every linked pid, leaving the set empty.
    pub fn drain(&mut self) -> Vec<ProcessId> {
        unimplemented!("stub — to be implemented")
    }
}
