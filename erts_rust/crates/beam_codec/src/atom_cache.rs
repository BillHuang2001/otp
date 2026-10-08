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

//! The distribution atom cache — both halves of the send/receive seam.
//!
//! * **Read side** — [`AtomCache`]. Mirrors `erl_ext_dist.md` §Distribution Header:
//!   8 segments of 256 entries (2048 slots in total). The *distribution header* that
//!   carries the segment updates is owned by `beam_dist`; `beam_dist` fills this cache
//!   and the decoder resolves an `ATOM_CACHE_REF` (`82`) through it.
//! * **Write side** — [`AtomCacheWriter`]. The narrow trait an outbound cache
//!   (`beam_dist::OutboundAtomCache`) implements so the encoder can emit
//!   `[ATOM_CACHE_REF (82), index]` instead of the atom name. The encoder never emits
//!   the legacy `NEW_CACHE` (`78`) tag: `external.h` defines it, but ERTS's encoder
//!   never writes it — new atoms travel in the distribution *header* and the payload
//!   only ever carries `82`.
//!
//! This module is safe Rust.

use beam_term::{Atom, AtomTable};

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

/// The outbound (send-side) atom-cache hook.
///
/// `beam_dist` classifies each outbound atom (`New` vs `Cached`) and addresses it in
/// the distribution header, but it must not be depended on by this crate (the DAG is
/// `beam_dist → beam_codec`). The encoder therefore exposes this narrow trait and
/// `beam_dist::OutboundAtomCache` implements it.
///
/// [`Encoder::with_atom_cache`](crate::Encoder::with_atom_cache) installs an
/// implementation: from then on [`Encoder::encode`](crate::Encoder::encode) consults
/// [`intern`](AtomCacheWriter::intern) for **every** atom and, when it returns an
/// index, writes `[ATOM_CACHE_REF_EXT (82), index]` instead of the inline
/// `SMALL_ATOM_UTF8_EXT`/`ATOM_UTF8_EXT` (or Latin-1) form.
///
/// Ground truth — C `enc_atom` (`erts/emulator/beam/external.c:2877`):
///
/// ```c
/// iix = get_iix_acache_map(acache, atom, &cached);
/// if (iix >= 0) { *ep++ = ATOM_CACHE_REF; put_int8(ep, iix); return ep; }
/// /* iix == -1 (no cache map, or the atom is not cacheable) ⇒ inline atom */
/// ```
///
/// A [`None`] return is exactly C's `-1`: the atom is written inline. The encoder
/// performs **no** name-length validation on the cached path (the C does none either —
/// the name travels in the distribution header, not in the payload).
pub trait AtomCacheWriter {
    /// Registers `atom` with the outbound cache for the message being encoded and
    /// returns its payload `ATOM_CACHE_REF` index, or [`None`] when the atom cannot
    /// be cached (write it inline, exactly as `enc_atom` does when the cache lookup
    /// returns `-1`).
    ///
    /// `atoms` is the encoder's [`AtomTable`], supplied so an implementation can read
    /// the name (for example to apply the `LongAtoms` / per-message budget /
    /// collision rules, all of which live in `beam_dist`, not in the codec).
    fn intern(&mut self, atom: Atom, atoms: &AtomTable) -> Option<u8>;
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

    /// A minimal [`AtomCacheWriter`] for the trait's unit tests: it records every
    /// atom it is asked about and returns a fixed index for one chosen atom.
    struct RecordingWriter {
        /// The one atom this writer caches.
        chosen: Atom,
        /// The index returned for `chosen`.
        index: u8,
        /// Every atom the encoder asked about, in order.
        seen: Vec<Atom>,
    }

    impl AtomCacheWriter for RecordingWriter {
        fn intern(&mut self, atom: Atom, _atoms: &AtomTable) -> Option<u8> {
            self.seen.push(atom);
            (atom == self.chosen).then_some(self.index)
        }
    }

    #[test]
    fn a_writer_implementation_receives_the_atom_and_the_table() {
        let atoms = AtomTable::new();
        let chosen = atom(7);
        let mut writer = RecordingWriter {
            chosen,
            index: 5,
            seen: Vec::new(),
        };

        assert_eq!(writer.intern(chosen, &atoms), Some(5));
        assert_eq!(writer.intern(atom(8), &atoms), None);
        assert_eq!(writer.seen, vec![chosen, atom(8)]);
    }

    #[test]
    fn the_read_and_write_halves_meet_at_a_slot() {
        // A `Some(index)` from the write side is exactly what the read side resolves:
        // `set(index, atom)` + `get(index)` recover the atom the encoder referenced.
        let atoms = AtomTable::new();
        let chosen = atom(11);
        let mut writer = RecordingWriter {
            chosen,
            index: 200,
            seen: Vec::new(),
        };
        let index = writer.intern(chosen, &atoms).expect("the chosen atom is cached");

        let mut cache = AtomCache::new();
        cache.set(index, chosen);
        assert_eq!(cache.get(index), Some(chosen));
    }
}
