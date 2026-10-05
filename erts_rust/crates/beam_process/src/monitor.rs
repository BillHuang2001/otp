//! Monitors and monitor references: [`Monitor`], [`MonitoredBy`], [`Monitors`],
//! [`MonitorRef`] and [`RefAllocator`].
//!
//! Monitors are one-directional and carry a reference: `A` monitoring `B` stores an
//! outgoing [`Monitor`] in `A` and an incoming [`MonitoredBy`] in `B`, and `B`'s
//! termination sends `A` a `{'DOWN', Ref, process, B, Reason}` message carrying that same
//! reference.
//!
//! # Monitor references
//!
//! `beam_term` has no `make_ref` in Milestone-1, so a reference is represented as the
//! frozen 4-word ref thing `[make_header(3, REF_SUBTAG), node(0), serial, id]`, built here
//! with `beam_term`'s public `make_header` (see the crate `CONTEXT.md`, Design Decisions).
//! `beam_message::layout::object_words` copies such a thing verbatim and
//! `beam_term::compare` compares internal refs by payload words, so a `{'DOWN', …}` message
//! round-trips and matches; when `beam_term` gains a proper `make_ref`, only
//! [`MonitorRef::to_eterm`] changes.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_monitor_link.c`.

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::{make_header, Eterm, REF_SUBTAG};

use crate::pid::ProcessId;

/// A monitor reference: the identity a `{'DOWN', …}` message carries.
//
// TODO(impl): implement `MonitorRef` (all bodies are stubs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MonitorRef {
    id: u64,
}

impl MonitorRef {
    /// The numeric reference id.
    pub fn id(self) -> u64 {
        unimplemented!("stub — to be implemented")
    }

    /// Builds the on-heap ref thing for this reference (see the module docs).
    ///
    /// # Errors
    ///
    /// Propagates `BeamError::Alloc` when `heap` cannot fit the ref thing.
    pub fn to_eterm(self, heap: &mut Heap) -> Result<Eterm> {
        unimplemented!("stub — to be implemented")
    }
}

/// What a monitor targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MonitorKind {
    /// A process monitor.
    Process,
    /// A port monitor.
    Port,
}

/// An outgoing monitor: this process watches `target`.
//
// TODO(impl): implement `Monitor` (all bodies are stubs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    /// The reference the `{'DOWN', …}` message will carry.
    pub reference: MonitorRef,
    /// The watched process.
    pub target: ProcessId,
    /// Whether a process or a port is watched.
    pub kind: MonitorKind,
}

/// An incoming monitor: `watcher` watches this process.
//
// TODO(impl): implement `MonitoredBy` (all bodies are stubs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitoredBy {
    /// The reference the `{'DOWN', …}` message must carry.
    pub reference: MonitorRef,
    /// The watching process.
    pub watcher: ProcessId,
    /// Whether a process or a port is watched.
    pub kind: MonitorKind,
}

/// The outgoing-monitor set of one process.
//
// TODO(impl): implement `Monitors` (all bodies are stubs).
pub struct Monitors {
    entries: Vec<Monitor>,
}

impl Monitors {
    /// An empty monitor set.
    pub fn new() -> Monitors {
        unimplemented!("stub — to be implemented")
    }

    /// The number of outgoing monitors.
    pub fn len(&self) -> usize {
        unimplemented!("stub — to be implemented")
    }

    /// Whether the process has no outgoing monitors.
    pub fn is_empty(&self) -> bool {
        unimplemented!("stub — to be implemented")
    }

    /// Adds an outgoing monitor.
    pub fn push(&mut self, monitor: Monitor) {
        unimplemented!("stub — to be implemented")
    }

    /// Removes and returns the monitor with `reference`, if present.
    pub fn remove(&mut self, reference: MonitorRef) -> Option<Monitor> {
        unimplemented!("stub — to be implemented")
    }

    /// Iterates the outgoing monitors.
    pub fn iter(&self) -> impl Iterator<Item = &Monitor> + '_ {
        crate::unimplemented_iter()
    }

    /// Borrows the monitor with `reference`, if present.
    pub fn find(&self, reference: MonitorRef) -> Option<&Monitor> {
        unimplemented!("stub — to be implemented")
    }

    /// Takes every outgoing monitor, leaving the set empty.
    pub fn drain(&mut self) -> Vec<Monitor> {
        unimplemented!("stub — to be implemented")
    }
}

/// The monotonic reference allocator, owned by the
/// [`ProcessTable`](crate::ptab::ProcessTable).
///
/// A reference id is never reused, so a stale `{'DOWN', …}` can never be confused with a
/// live monitor.
//
// TODO(impl): implement `RefAllocator` (all bodies are stubs).
pub struct RefAllocator {
    next: u64,
}

impl RefAllocator {
    /// An allocator that will hand out its first reference on the first call.
    pub fn new() -> RefAllocator {
        unimplemented!("stub — to be implemented")
    }

    /// Hands out the next never-reused reference.
    pub fn next(&mut self) -> MonitorRef {
        unimplemented!("stub — to be implemented")
    }
}
