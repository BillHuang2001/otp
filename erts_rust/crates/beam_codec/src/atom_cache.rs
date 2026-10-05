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
        AtomCache { slots: [None; SLOTS] }
    }

    /// Records `atom` at `index`, replacing any previous entry.
    pub fn set(&mut self, index: u8, atom: Atom) {
        self.slots[usize::from(index)] = Some(atom);
    }

    /// The atom recorded at `index`, or `None` when the slot is unset.
    ///
    /// The decoder turns an unset slot into `BeamError::BadArg`.
    pub fn get(&self, index: u8) -> Option<Atom> {
        self.slots[usize::from(index)]
    }

    /// Clears every slot.
    pub fn clear(&mut self) {
        self.slots = [None; SLOTS];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an `Atom` from a raw index for test purposes.
    ///
    /// `Atom` is an opaque `Copy` wrapper around a `u32`, so distinct raw indexes
    /// are distinct atoms for equality purposes without needing an `AtomTable`.
    fn atom(index: u32) -> Atom {
        Atom::from_raw(index)
    }

    #[test]
    fn slot_counts_match_the_wire_format() {
        assert_eq!(SEGMENTS, 8);
        assert_eq!(SLOTS_PER_SEGMENT, 256);
        assert_eq!(SLOTS, 2048);
    }

    #[test]
    fn fresh_cache_has_every_slot_unset() {
        let cache = AtomCache::new();
        // Index 0 is a perfectly ordinary slot and must start out unset too.
        assert_eq!(cache.get(0), None);
        assert_eq!(cache.get(1), None);
        assert_eq!(cache.get(127), None);
        assert_eq!(cache.get(255), None);
        for index in 0..=u8::MAX {
            assert_eq!(cache.get(index), None, "index {index}");
        }
    }

    #[test]
    fn set_then_get_round_trips() {
        let mut cache = AtomCache::new();
        let first = atom(1);
        let last = atom(2);
        cache.set(0, first);
        cache.set(u8::MAX, last);
        assert_eq!(cache.get(0), Some(first));
        assert_eq!(cache.get(u8::MAX), Some(last));
        // Untouched slots stay unset.
        assert_eq!(cache.get(1), None);
    }

    #[test]
    fn set_overwrites_a_previous_entry() {
        let mut cache = AtomCache::new();
        let first = atom(10);
        let second = atom(20);
        cache.set(7, first);
        assert_eq!(cache.get(7), Some(first));
        cache.set(7, second);
        assert_eq!(cache.get(7), Some(second));
    }

    #[test]
    fn slots_are_independent() {
        let mut cache = AtomCache::new();
        for index in 0..=u8::MAX {
            cache.set(index, atom(u32::from(index) + 1000));
        }
        for index in 0..=u8::MAX {
            assert_eq!(
                cache.get(index),
                Some(atom(u32::from(index) + 1000)),
                "index {index}"
            );
        }
    }

    #[test]
    fn clear_resets_every_slot() {
        let mut cache = AtomCache::new();
        cache.set(0, atom(1));
        cache.set(42, atom(2));
        cache.set(u8::MAX, atom(3));
        cache.clear();
        assert_eq!(cache.get(0), None);
        assert_eq!(cache.get(42), None);
        assert_eq!(cache.get(1), None);
        assert_eq!(cache.get(u8::MAX), None);
        for index in 0..=u8::MAX {
            assert_eq!(cache.get(index), None, "index {index}");
        }
    }

    #[test]
    fn cache_is_reusable_after_clear() {
        let mut cache = AtomCache::new();
        let reusing = atom(99);
        cache.set(5, atom(1));
        cache.clear();
        cache.set(5, reusing);
        assert_eq!(cache.get(5), Some(reusing));
    }
}
