//! The Cheney copy engine: [`Copier`] and the [`collect`] entry point.
//!
//! `unsafe` is confined to this module's word-level copier, which mirrors the legacy
//! C `move_boxed`/`move_cons`/`sweep` (`erts/emulator/beam/erl_gc.c`) — in-place
//! forwarding plus a Cheney scan of the freshly-copied to-space.

use beam_common::Result;
use beam_term::prelude::Eterm;

use crate::space::{FromSet, HeapSpace};
use crate::stats::GcStats;

/// The Cheney copier: evacuates live terms from a [`FromSet`] into a [`HeapSpace`].
///
/// A boxed object's header word is overwritten with the new BOXED-tagged pointer and
/// a cons cell's car/cdr with the forwarding marker, exactly as in C; the `scan`
/// pointer walks the to-space to completion.
pub struct Copier<'a> {
    from: &'a FromSet,
    to: &'a mut HeapSpace,
    scan: *mut Eterm,
    stats: GcStats,
}

impl<'a> Copier<'a> {
    /// Creates a copier evacuating `from` into `to`, recording the to-space scan origin.
    pub fn new(from: &'a FromSet, to: &'a mut HeapSpace) -> Copier<'a> {
        let _ = (from, to);
        unimplemented!("scaffold")
    }

    /// Forwards each live root word in `roots` in place (updating it to its new address).
    pub fn forward_roots(&mut self, roots: &mut [Eterm]) -> Result<()> {
        let _ = roots;
        unimplemented!("scaffold")
    }

    /// Runs the Cheney scan of the to-space to completion and returns the statistics.
    pub fn complete(self) -> Result<GcStats> {
        unimplemented!("scaffold")
    }

    /// Forwards the roots and completes the scan in one call.
    pub fn run(self, roots: &mut [Eterm]) -> Result<GcStats> {
        let _ = roots;
        unimplemented!("scaffold")
    }
}

/// The primitive young-generation collection entry: evacuate the live terms reachable
/// from `roots` out of `from` into `to`, updating `roots` in place.
pub fn collect(from: &HeapSpace, to: &mut HeapSpace, roots: &mut [Eterm]) -> Result<GcStats> {
    let _ = (from, to, roots);
    unimplemented!("scaffold")
}
