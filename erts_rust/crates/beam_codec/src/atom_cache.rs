//! The distribution atom cache.
//!
//! Mirrors `erl_ext_dist.md` §Distribution Header: 8 segments of 256 entries
//! (2048 slots in total). The *distribution header* that carries the segment
//! updates is owned by `beam_dist`; `beam_dist` fills this cache and the decoder
//! resolves an `ATOM_CACHE_REF` (`82`) through it. Milestone 1 never *emits*
//! `82`/`78`.
//!
//! This module is safe Rust.

use beam_term::Atom;

/// Number of atom-cache segments (the distribution header rotates through them).
const SEGMENTS: usize = 8;

/// Number of entries per segment (the on-wire index is a single byte).
const SLOTS_PER_SEGMENT: usize = 256;

/// Total number of cache slots: 8 segments × 256 entries = 2048.
const SLOTS: usize = SEGMENTS * SLOTS_PER_SEGMENT;

/// The distribution atom cache: an index → [`Atom`] table.
///
/// `set`/`get` address a slot by the one-byte index carried on the wire; the
/// segment rotation between the 8 × 256 slots is driven by `beam_dist` when it
/// parses a distribution header.
pub struct AtomCache {
    /// One optional atom per cache slot.
    slots: [Option<Atom>; SLOTS],
}

impl AtomCache {
    /// Creates an empty cache (every slot unset).
    pub fn new() -> AtomCache {
        unimplemented!()
    }

    /// Records `atom` at `index`, replacing any previous entry.
    pub fn set(&mut self, index: u8, atom: Atom) {
        unimplemented!()
    }

    /// The atom recorded at `index`, or `None` when the slot is unset.
    ///
    /// The decoder turns an unset slot into `BeamError::BadArg`.
    pub fn get(&self, index: u8) -> Option<Atom> {
        unimplemented!()
    }

    /// Clears every slot.
    pub fn clear(&mut self) {
        unimplemented!()
    }
}
