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
use beam_term::prelude::{make_boxed, make_header, make_small, Eterm, REF_SUBTAG};

use crate::pid::ProcessId;

/// The number of payload words of the ref thing: `node`, `serial` and `id`.
const REF_PAYLOAD_WORDS: usize = 3;

/// A monitor reference: the identity a `{'DOWN', …}` message carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MonitorRef {
    id: u64,
}

impl MonitorRef {
    /// The numeric reference id.
    pub fn id(self) -> u64 {
        self.id
    }

    /// Builds the on-heap ref thing for this reference (see the module docs).
    ///
    /// The thing is `[make_header(3, REF_SUBTAG), node, serial, id]`; Milestone-1 has a
    /// single monotonic id space, so `node` and `serial` are zero and the whole id lives in
    /// the last word (a `small`).
    ///
    /// # Errors
    ///
    /// Propagates `BeamError::Alloc` when `heap` cannot fit the ref thing.
    pub fn to_eterm(self, heap: &mut Heap) -> Result<Eterm> {
        // SAFETY: `alloc_words` returns four exclusively-owned, writable words.
        let p = unsafe { heap.alloc_words(REF_PAYLOAD_WORDS + 1)? };
        // SAFETY: `p` addresses the four words just handed out; every one of them is
        // written before the pointer escapes this block.
        unsafe {
            *p = make_header(REF_PAYLOAD_WORDS, REF_SUBTAG);
            *p.add(1) = make_small(0); // node: the local node
            *p.add(2) = make_small(0); // serial: one id space in M1
            *p.add(3) = make_small(self.id as i64);
        }
        Ok(make_boxed(p as *const Eterm))
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
pub struct Monitors {
    entries: Vec<Monitor>,
}

impl Monitors {
    /// An empty monitor set.
    pub fn new() -> Monitors {
        Monitors {
            entries: Vec::new(),
        }
    }

    /// The number of outgoing monitors.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the process has no outgoing monitors.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Adds an outgoing monitor.
    pub fn push(&mut self, monitor: Monitor) {
        self.entries.push(monitor);
    }

    /// Removes and returns the monitor with `reference`, if present.
    pub fn remove(&mut self, reference: MonitorRef) -> Option<Monitor> {
        let index = self.index_of(reference)?;
        Some(self.entries.remove(index))
    }

    /// Iterates the outgoing monitors.
    pub fn iter(&self) -> impl Iterator<Item = &Monitor> + '_ {
        self.entries.iter()
    }

    /// Borrows the monitor with `reference`, if present.
    pub fn find(&self, reference: MonitorRef) -> Option<&Monitor> {
        self.entries
            .iter()
            .find(|monitor| monitor.reference == reference)
    }

    /// Takes every outgoing monitor, leaving the set empty.
    pub fn drain(&mut self) -> Vec<Monitor> {
        core::mem::take(&mut self.entries)
    }

    /// The position of the monitor with `reference`, if present.
    fn index_of(&self, reference: MonitorRef) -> Option<usize> {
        self.entries
            .iter()
            .position(|monitor| monitor.reference == reference)
    }
}

/// The monotonic reference allocator, owned by the
/// [`ProcessTable`](crate::ptab::ProcessTable).
///
/// A reference id is never reused, so a stale `{'DOWN', …}` can never be confused with a
/// live monitor.
pub struct RefAllocator {
    next: u64,
}

impl RefAllocator {
    /// An allocator that will hand out its first reference on the first call.
    pub fn new() -> RefAllocator {
        RefAllocator { next: 0 }
    }

    /// Hands out the next never-reused reference.
    pub fn next(&mut self) -> MonitorRef {
        let id = self.next;
        // `wrapping_add` only matters after 2^64 references; a real runtime can never
        // reach that, and wrapping keeps the allocator total rather than panicking.
        self.next = self.next.wrapping_add(1);
        MonitorRef { id }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for `RefAllocator` monotonicity, the `Monitors` set operations and the
    //! frozen shape of the on-heap ref thing [`MonitorRef::to_eterm`] builds.

    use beam_common::BeamError;
    use beam_term::prelude::{boxed_val, header_subtag, is_boxed, signed_value, REF_SUBTAG};

    use super::*;

    fn pid(number: u64) -> ProcessId {
        ProcessId::new(number, 0)
    }

    #[test]
    fn ref_allocator_hands_out_distinct_monotonic_ids() {
        let mut refs = RefAllocator::new();
        let a = refs.next();
        let b = refs.next();
        let c = refs.next();
        assert!(a.id() < b.id());
        assert!(b.id() < c.id());
        assert_ne!(a, b);
        assert_ne!(b, c);
    }

    #[test]
    fn monitors_push_find_remove_and_drain() {
        let mut refs = RefAllocator::new();
        let r1 = refs.next();
        let r2 = refs.next();

        let mut monitors = Monitors::new();
        assert!(monitors.is_empty());
        assert_eq!(monitors.len(), 0);
        assert!(monitors.find(r1).is_none());

        monitors.push(Monitor {
            reference: r1,
            target: pid(1),
            kind: MonitorKind::Process,
        });
        monitors.push(Monitor {
            reference: r2,
            target: pid(2),
            kind: MonitorKind::Port,
        });
        assert_eq!(monitors.len(), 2);
        assert_eq!(monitors.find(r1).map(|m| m.target), Some(pid(1)));
        assert_eq!(monitors.find(r2).map(|m| m.kind), Some(MonitorKind::Port));
        assert_eq!(
            monitors.iter().map(|m| m.reference).collect::<Vec<_>>(),
            vec![r1, r2]
        );

        let removed = monitors.remove(r1).expect("r1 is present");
        assert_eq!(removed.target, pid(1));
        assert!(monitors.remove(r1).is_none());
        assert_eq!(monitors.len(), 1);

        let all = monitors.drain();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].reference, r2);
        assert!(monitors.is_empty());
    }

    #[test]
    fn monitor_ref_to_eterm_builds_the_frozen_thing() {
        let mut refs = RefAllocator::new();
        let reference = refs.next();
        // Skip one id so the encoded id is not simply zero.
        let _ = refs.next();
        let second = refs.next();

        let mut heap = Heap::new(8).expect("test heap");
        let term = reference.to_eterm(&mut heap).expect("the ref thing fits");
        assert!(is_boxed(term));
        assert_eq!(heap.used(), 4, "a ref thing is four words");

        // SAFETY: `term` is a live, four-word heap object; words 0..4 are in bounds and
        // were all written by `to_eterm`.
        unsafe {
            let p = boxed_val(term);
            assert_eq!(header_subtag(*p), REF_SUBTAG as u32);
            assert_eq!(signed_value(*p.add(1)), 0, "node is local (0)");
            assert_eq!(signed_value(*p.add(2)), 0, "one id space in M1");
            assert_eq!(signed_value(*p.add(3)), reference.id() as i64);
            // Two distinct references encode to distinct last words.
            let other = second.to_eterm(&mut heap).expect("the ref thing fits");
            assert_ne!(*p.add(3), *boxed_val(other).add(3));
        }
    }

    #[test]
    fn to_eterm_reports_exhaustion_rather_than_panicking() {
        let mut heap = Heap::new(3).expect("a three-word heap");
        let reference = RefAllocator::new().next();
        assert!(matches!(
            reference.to_eterm(&mut heap),
            Err(BeamError::Alloc(_))
        ));
    }
}