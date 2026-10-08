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

//! The distribution atom-cache / segment machinery on the payload side.
//!
//! The atom cache brackets `beam_codec`'s payload codec: an *outbound* half that
//! records which atoms a message teaches or re-uses and emits the distribution
//! header's [`AtomCacheRef`]s (send side), and an *inbound* half that applies a
//! decoded [`DistHeader`] so the payload's `ATOM_CACHE_REF` (`82`) tags resolve
//! to atoms (receive side).
//!
//! # The two cache levels — do not conflate them
//!
//! 1. **The persistent, per-connection segmented cache.** ERTS's `ErtsAtomCache`
//!    (`erts/emulator/beam/external.h`: `in_arr`/`out_arr`) with
//!    `ERTS_ATOM_CACHE_SIZE = 2048` slots = 8 segments × 256 entries. A slot is
//!    addressed by an 11-bit index `cix = (segment << 8) | index`, where the
//!    header's 3-bit `SegmentIndex` supplies the high bits and its one-byte
//!    `InternalSegmentIndex` the low bits. It survives across messages. This
//!    module models it with [`SegmentCache`].
//!
//! 2. **The per-message translation table.** ERTS's `ErtsDistExternal.attab`
//!    (`ERTS_MAX_INTERNAL_ATOM_CACHE_ENTRIES = 255` entries). It is addressed by
//!    the payload's one-byte `AtomCacheReferenceIndex`, which is the **position of
//!    the corresponding ref in the header's `AtomCacheRefs` list** — *not* a
//!    segment index. It is rebuilt for every header and is what
//!    [`beam_codec::AtomCache`] models.
//!
//! The distinction is load-bearing. `erl_ext_dist.md` §"Normal Distribution
//! Header"/"Atom Cache" contains a worked example whose ref list is
//! `[(Cached seg 4 idx 10), (Cached seg 0 idx 5), (New seg 1 idx 236 `"reg"`),
//! …]` and whose control message references `82,2`; index 2 is the *third* header
//! ref (the `New` one that installed `"reg"` at slot `(1,236)`). So the payload
//! byte is the ref position, and `beam_codec::AtomCache`'s slot `i` must hold the
//! atom of header ref `i`. ERTS does exactly this on both sides: `enc_atom`
//! writes `get_iix_acache_map(…)` which returns `cache[ix].iix`, the *sequential*
//! position assigned by `insert_acache_map` (`external.c`), and the decoder writes
//! `edep->attab.atom[tix]` and resolves `82 n` as `attab.atom[n]`
//! (`external.c` `dec_atom`/`dec_term`).
//!
//! # Reconciliation with `beam_codec::AtomCache` — the precise mismatch
//!
//! `beam_codec::AtomCache`'s own docs describe it as "8 segments × 256 = 2048
//! slots", but its public API is `set(index: u8, atom)` / `get(index: u8)`: a
//! single byte, so only slots `0..=255` are ever addressable (its backing array
//! is 2048 wide, but `SEGMENTS` is never used for addressing). It therefore
//! **cannot** express the persistent 2048-slot `(segment, index)` address space
//! and must not be used as the persistent cache.
//!
//! It *exactly* models the per-message translation table: at most 255 entries
//! (`ERTS_MAX_INTERNAL_ATOM_CACHE_ENTRIES`), addressed by the payload's one-byte
//! ref. This module fills it that way — slot `i` = the atom of header ref `i` —
//! and keeps the 8-segment space entirely inside [`SegmentCache`], which
//! `beam_codec` never needs to see. Nothing is lost; the "8 segments" are a
//! property of the header's address, not of `beam_codec`.
//!
//! Applying a header therefore means: for each ref, in order, resolve its atom
//! (`New` installs it at its `(segment, index)` slot in the persistent cache,
//! `Cached` reads it back and is `BadArg` when the slot is unset) and store it at
//! the ref's position in the translation table. See [`apply_header`].
//!
//! # Handling & semantics
//!
//! * A `New` ref **installs** its atom into the persistent slot the ref addresses.
//! * A `Cached` ref **selects** an existing persistent slot; an unset slot (or an
//!   out-of-range `SegmentIndex`) is [`BeamError::BadArg`], never a panic.
//! * A header may carry at most [`MAX_ATOM_CACHE_REFS`] (255) refs; more is
//!   [`BeamError::SystemLimit`].
//! * There is **no per-message segment rotation**. ERTS derives an atom's segment
//!   from a *fixed* hash of the atom — `atom2cix(atom) = atom_val(atom) % 2039`
//!   (`ERTS_USE_ATOM_CACHE_SIZE`, `external.c`), whose high 3 bits are the
//!   `SegmentIndex`. The 2048-slot space is therefore consumed by hashing, not by
//!   a rotating counter. [`OutboundAtomCache::advance_segment`] is the ERTS
//!   per-message reset (`erts_reset_atom_cache_map`); it starts a new message and
//!   does not move any slot.
//!
//! Ground truth (read, never modified): `erts/emulator/beam/external.c`
//! (`atom2cix`, `insert_acache_map`/`get_iix_acache_map`,
//! `erts_reset_atom_cache_map`/`erts_finalize_atom_cache_map`,
//! `erts_encode_ext_dist_header_finalize`, and the decode-side `attab`), `external.h`
//! and `erts/doc/guides/erl_ext_dist.md`.
//!
//! # The `beam_codec` seam: `AtomCacheWriter`
//!
//! `beam_codec` must not depend on `beam_dist` (the DAG is `beam_dist →
//! beam_codec`), so the send-side payload emission is a narrow trait that
//! `beam_codec` owns — [`beam_codec::AtomCacheWriter`] — and [`OutboundAtomCache`]
//! implements ([`intern`](beam_codec::AtomCacheWriter::intern)). The encoder
//! installs it through
//! [`Encoder::with_atom_cache`](beam_codec::Encoder::with_atom_cache); from
//! then on every atom it writes is offered to
//! [`intern`](beam_codec::AtomCacheWriter::intern) and, when that returns an index,
//! is emitted as `ATOM_CACHE_REF` (`82`) + the one-byte index instead of the atom
//! text (C `enc_atom`, `external.c:2877`: an inline atom only when the cache map
//! yields no index).
//!
//! The encoder walks the message in payload order and `intern` gives each newly
//! referenced atom the next sequential position, so those positions *are* the
//! header's `AtomCacheReferenceIndex` order — the caller emits the matching
//! [`DistHeader`] from [`take_header`](OutboundAtomCache::take_header) and a
//! receiver resolves each `82 n` to header ref `n` (see [`apply_header`]).
//!
//! [`OutboundAtomCache`]'s [`intern`](beam_codec::AtomCacheWriter::intern) performs
//! the same classification as [`ref_for`](OutboundAtomCache::ref_for) and returns
//! the bare [`payload_index`](OutboundAtomCache::payload_index). The
//! [`ref_for`](OutboundAtomCache::ref_for)/[`payload_ref_for`](OutboundAtomCache::payload_ref_for)
//! pair remains the manual API for callers that build the payload themselves. No
//! other `beam_codec` change is needed: the *decoder* already resolves `82` through
//! `Decoder::with_atom_cache`, and the `LongAtoms` / per-message budget / collision
//! rules live here, not in the codec.
//!
//! The legacy `NEW_CACHE` / `NEW_ATOM_CACHE_REF` tag (`78`) is **not** emitted:
//! `external.h` defines it (`#define NEW_CACHE 'N'`) but ERTS's encoder never
//! writes it — new atoms travel in the distribution *header*, and the payload only
//! ever carries `82`.

use std::collections::HashMap;

use beam_codec::AtomCache;
use beam_common::{BeamError, Result};
use beam_term::{Atom, AtomTable};

use crate::dist_header::{AtomCacheRef, DistHeader, ATOM_CACHE_SEGMENTS, MAX_ATOM_CACHE_REFS};

/// Entries per segment: the `InternalSegmentIndex` on the wire is one byte, so a
/// segment holds `256` atoms (`erl_ext_dist.md` §"Normal Distribution Header").
pub const ENTRIES_PER_SEGMENT: usize = 256;

/// `ERTS_ATOM_CACHE_SIZE` (`erts/emulator/beam/external.h`): the total number of
/// persistent cache slots, `8 segments × 256 entries = 2048`.
pub const ATOM_CACHE_SIZE: usize = ATOM_CACHE_SEGMENTS as usize * ENTRIES_PER_SEGMENT;

/// `ERTS_USE_ATOM_CACHE_SIZE` (`erts/emulator/beam/external.c`): the modulus the
/// *sender* hashes an atom into to choose its persistent slot (`atom2cix`).
///
/// It is deliberately smaller than [`ATOM_CACHE_SIZE`] and **not** a power of two,
/// so the sender's slot assignment only approximates a uniform spread across the
/// eight segments — exactly as ERTS does.
pub const USE_ATOM_CACHE_SIZE: usize = 2039;

/// Converts a header ref's `(SegmentIndex, InternalSegmentIndex)` address into a
/// [`SegmentCache`] slot index: `(segment << 8) | index`.
///
/// A `SegmentIndex` above [`ATOM_CACHE_SEGMENTS`]-1 cannot be produced by the wire
/// decoder (it masks the 3-bit field) and yields [`BeamError::BadArg`] rather than
/// a wild slot index.
pub fn slot_index(segment: u8, index: u8) -> Result<usize> {
    if usize::from(segment) >= usize::from(ATOM_CACHE_SEGMENTS) {
        return Err(BeamError::BadArg);
    }
    Ok((usize::from(segment) << 8) | usize::from(index))
}

/// The sender's atom → persistent-slot hash, `atom2cix`
/// (`erts/emulator/beam/external.c`): `atom_val(atom) % ERTS_USE_ATOM_CACHE_SIZE`.
///
/// The result is `0..USE_ATOM_CACHE_SIZE`; the header ref's `SegmentIndex` is its
/// high 3 bits and its `InternalSegmentIndex` the low 8 bits.
pub fn atom_to_cix(atom: Atom) -> usize {
    atom.raw() as usize % USE_ATOM_CACHE_SIZE
}

/// The persistent, per-connection distribution atom cache: 2048 slots addressed
/// by [`slot_index`] (send side: [`atom_to_cix`]).
///
/// Mirrors `ErtsAtomCache`'s `in_arr`/`out_arr` (`erts/emulator/beam/external.h`)
/// — a flat array an `(segment, index)` address indexes into. It is *per
/// connection*: unlike the per-message translation table it is not cleared between
/// messages, which is exactly what lets a later header refer to an earlier one's
/// atoms with a `Cached` ref.
#[derive(Clone, Debug)]
pub struct SegmentCache {
    /// One optional atom per slot (`None` = unset).
    slots: [Option<Atom>; ATOM_CACHE_SIZE],
}

impl SegmentCache {
    /// Creates an empty cache (every slot unset).
    pub fn new() -> SegmentCache {
        SegmentCache { slots: [None; ATOM_CACHE_SIZE] }
    }

    /// The atom at slot `slot`, or `None` when the slot is unset or out of range.
    pub fn get(&self, slot: usize) -> Option<Atom> {
        self.slots.get(slot).copied().flatten()
    }

    /// Records `atom` at slot `slot`.
    ///
    /// Slots are the raw `0..ATOM_CACHE_SIZE` indices produced by [`slot_index`]
    /// (receive side) or [`atom_to_cix`] (send side); both are always in range, so
    /// an out-of-range `slot` can only come from a hand-built address and is simply
    /// ignored rather than panicking.
    pub fn set(&mut self, slot: usize, atom: Atom) {
        if let Some(entry) = self.slots.get_mut(slot) {
            *entry = Some(atom);
        }
    }

    /// Unsets every slot (a full connection reset, not a per-message one).
    pub fn clear(&mut self) {
        self.slots = [None; ATOM_CACHE_SIZE];
    }

    /// The number of set slots. Intended for diagnostics and tests.
    pub fn occupied(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }
}

/// Applies a decoded [`DistHeader`] to the receive side.
///
/// Walks the header's refs in order. For each ref `i`:
/// * [`AtomCacheRef::New`] **installs** its atom at the persistent slot the ref
///   addresses (its `segment`/`index`), then
/// * [`AtomCacheRef::Cached`] **selects** that slot, which must already hold an
///   atom,
///
/// and stores the resulting atom at slot `i` of `table`. `table` is the
/// per-message translation table (a [`beam_codec::AtomCache`]) that the caller then
/// hands to `beam_codec::Decoder::with_atom_cache` / `beam_dist::decode_control`
/// while decoding the payload: a payload `ATOM_CACHE_REF` byte `n` resolves to
/// `table` slot `n`.
///
/// `table` is **cleared first** (the table is per message): a stale atom from an
/// earlier header can otherwise be reached by a ref index beyond this header's ref
/// count — a silent wrong result.
///
/// Errors: more than [`MAX_ATOM_CACHE_REFS`] refs ⇒ [`BeamError::SystemLimit`]; a
/// `Cached` ref addressing an unset slot, or a `SegmentIndex` outside
/// `0..=7` ⇒ [`BeamError::BadArg`]. Never panics on header data.
pub fn apply_header(
    header: &DistHeader,
    persistent: &mut SegmentCache,
    table: &mut AtomCache,
) -> Result<()> {
    if header.refs.len() > MAX_ATOM_CACHE_REFS {
        return Err(BeamError::SystemLimit);
    }

    // The translation table is rebuilt for every header.
    table.clear();

    for (tix, cache_ref) in header.refs.iter().enumerate() {
        let slot = slot_index(cache_ref.segment(), cache_ref.index())?;
        let atom = match *cache_ref {
            AtomCacheRef::New { atom, .. } => {
                persistent.set(slot, atom);
                atom
            }
            AtomCacheRef::Cached { .. } => persistent.get(slot).ok_or(BeamError::BadArg)?,
        };
        // `tix < header.refs.len() <= MAX_ATOM_CACHE_REFS` (255), so the cast is
        // lossless; the checked form keeps the invariant explicit.
        let index = u8::try_from(tix).map_err(|_| BeamError::SystemLimit)?;
        table.set(index, atom);
    }

    Ok(())
}

/// The receive-side atom cache: the persistent [`SegmentCache`] plus the
/// per-message translation table handed to `beam_codec`.
///
/// `apply` a decoded header, then pass [`table`](Self::table) to
/// `beam_dist::decode_control` (or `beam_codec::Decoder::with_atom_cache`) while
/// decoding the payload. The type is the natural owner of the per-connection
/// state: create one per distribution link and keep re-applying headers to it.
pub struct ReceiveAtomCache {
    /// The persistent per-connection cache (survives across headers).
    persistent: SegmentCache,
    /// The per-message translation table (rebuilt by every [`apply`](Self::apply)).
    table: AtomCache,
    /// Number of refs in the most recently applied header.
    refs: usize,
}

impl ReceiveAtomCache {
    /// Creates a cache with an empty persistent store and an empty table.
    pub fn new() -> ReceiveAtomCache {
        ReceiveAtomCache { persistent: SegmentCache::new(), table: AtomCache::new(), refs: 0 }
    }

    /// Applies a decoded distribution header (see [`apply_header`]).
    pub fn apply(&mut self, header: &DistHeader) -> Result<()> {
        apply_header(header, &mut self.persistent, &mut self.table)?;
        self.refs = header.refs.len();
        Ok(())
    }

    /// The per-message translation table: pass it to `beam_codec` to resolve the
    /// payload's `ATOM_CACHE_REF` (`82`) tags.
    pub fn table(&self) -> &AtomCache {
        &self.table
    }

    /// The persistent per-connection cache (the `(segment, index)` atoms that
    /// later `Cached` refs select).
    pub fn persistent(&self) -> &SegmentCache {
        &self.persistent
    }

    /// The number of refs in the most recently applied header, i.e. the number of
    /// distinct payload `ATOM_CACHE_REF` indices that can currently resolve.
    pub fn ref_count(&self) -> usize {
        self.refs
    }

    /// Drops all state (a full connection reset): both the persistent cache and
    /// the translation table.
    pub fn clear(&mut self) {
        self.persistent.clear();
        self.table.clear();
        self.refs = 0;
    }
}

/// The send-side atom cache: the persistent per-connection [`SegmentCache`] plus
/// the per-message list of [`AtomCacheRef`]s a header must carry.
///
/// Mirrors ERTS's `ErtsAtomCacheMap` (`erts/emulator/beam/external.c`): the caller
/// offers each atom the message references, in order, via [`ref_for`](Self::ref_for);
/// the returned refs are collected by [`header`](Self::header) into a
/// [`DistHeader`], and [`advance_segment`](Self::advance_segment) starts the next
/// message.
///
/// ## Classification
///
/// `ref_for` reproduces ERTS's rules exactly:
/// * an atom already referenced *in this message* returns the same ref again (the
///   payload reuses its `AtomCacheReferenceIndex`),
/// * a second atom hashing to a slot already claimed by the message is **not**
///   cacheable (`None` — emit it inline), as `insert_acache_map` refuses it,
/// * otherwise the atom's slot is `atom_to_cix(atom)`; it is [`New`] when the
///   persistent slot does not already hold it and [`Cached`] when it does, and
/// * once the message holds [`MAX_ATOM_CACHE_REFS`] (255) refs, further atoms are
///   not cacheable (`None`).
///
/// [`New`]: AtomCacheRef::New
/// [`Cached`]: AtomCacheRef::Cached
#[derive(Debug)]
pub struct OutboundAtomCache {
    /// The persistent per-connection cache (`out_arr`).
    persistent: SegmentCache,
    /// The current message's refs, in first-reference order.
    refs: Vec<AtomCacheRef>,
    /// For the message in progress: persistent slot → (owning atom, ref position).
    assigned: HashMap<usize, (Atom, u8)>,
    /// `LongAtoms` for the message in progress (set on a `New` atom longer than
    /// 255 bytes; see `erts_finalize_atom_cache_map`).
    long_atoms: bool,
}

impl OutboundAtomCache {
    /// Creates an outbound cache with an empty persistent store.
    pub fn new() -> OutboundAtomCache {
        OutboundAtomCache {
            persistent: SegmentCache::new(),
            refs: Vec::new(),
            assigned: HashMap::new(),
            long_atoms: false,
        }
    }

    /// Registers a reference to `atom` in the message being built and returns its
    /// [`AtomCacheRef`] — or `None` when the atom cannot be cached in this message
    /// and the caller must write the atom text inline.
    ///
    /// The returned ref's *position* in [`header`](Self::header)`().refs` is the
    /// atom's payload `AtomCacheReferenceIndex`; [`payload_index`](Self::payload_index)
    /// (or [`payload_ref_for`](Self::payload_ref_for)) returns it directly. Repeating
    /// an atom returns the identical ref.
    ///
    /// `atoms` is consulted only to decide the header's `LongAtoms` flag (an atom
    /// whose name is longer than 255 bytes forces a 2-byte length field).
    pub fn ref_for(&mut self, atom: Atom, atoms: &AtomTable) -> Option<AtomCacheRef> {
        let slot = atom_to_cix(atom);

        // Already referenced in this message?
        if let Some(&(owner, position)) = self.assigned.get(&slot) {
            return if owner == atom {
                Some(self.refs[usize::from(position)])
            } else {
                // A different atom hashed to the same slot: ERTS's
                // `insert_acache_map` refuses it and `get_iix_acache_map`
                // reports -1, so it is written inline.
                None
            };
        }

        // The per-message ref budget (`ERTS_MAX_INTERNAL_ATOM_CACHE_ENTRIES`).
        if self.refs.len() >= MAX_ATOM_CACHE_REFS {
            return None;
        }

        let segment = (slot >> 8) as u8;
        let index = (slot & 0xff) as u8;
        let cache_ref = match self.persistent.get(slot) {
            Some(cached) if cached == atom => AtomCacheRef::Cached { segment, index },
            _ => {
                self.persistent.set(slot, atom);
                AtomCacheRef::New { segment, index, atom }
            }
        };

        // `refs.len() < MAX_ATOM_CACHE_REFS` (255), so the position fits a byte.
        let position = self.refs.len() as u8;
        self.refs.push(cache_ref);
        self.assigned.insert(slot, (atom, position));

        // `LongAtoms` covers *every* ref of the message, `New` ones included
        // (`erts_finalize_atom_cache_map` scans all inserted atoms).
        if atoms.name(atom).is_some_and(|name| name.len() > u8::MAX as usize) {
            self.long_atoms = true;
        }

        Some(cache_ref)
    }

    /// The payload `AtomCacheReferenceIndex` assigned to `atom` in the message
    /// being built, or `None` when the atom is not cacheable (write it inline).
    pub fn payload_index(&self, atom: Atom) -> Option<u8> {
        match self.assigned.get(&atom_to_cix(atom)) {
            Some(&(owner, position)) if owner == atom => Some(position),
            _ => None,
        }
    }

    /// Registers `atom` and returns the `ATOM_CACHE_REF` (`82`) payload bytes for
    /// it — `[82, index]` — or `None` when the atom is not cacheable and must be
    /// written inline.
    ///
    /// This is the explicit form of the send-side payload emission, for a caller
    /// that builds the payload itself; the [`beam_codec::AtomCacheWriter`] impl
    /// below performs the same step inline while `beam_codec`'s encoder writes the
    /// payload. The header must still be written from
    /// [`take_header`](Self::take_header) — the payload only carries the index.
    pub fn payload_ref_for(&mut self, atom: Atom, atoms: &AtomTable) -> Option<[u8; 2]> {
        self.ref_for(atom, atoms)?;
        let index = self.payload_index(atom)?;
        Some([beam_codec::ATOM_CACHE_REF_EXT, index])
    }

    /// The distribution header for the message being built.
    pub fn header(&self) -> DistHeader {
        DistHeader { refs: self.refs.clone(), long_atoms: self.long_atoms }
    }

    /// Takes the distribution header for the message being built and starts a new
    /// message (see [`advance_segment`](Self::advance_segment)).
    pub fn take_header(&mut self) -> DistHeader {
        let header = self.header();
        self.advance_segment();
        header
    }

    /// Ends the current message: clears the per-message ref list so the next
    /// [`header`](Self::header) describes a fresh message.
    ///
    /// This is ERTS's per-message reset, `erts_reset_atom_cache_map`. The persistent
    /// per-connection cache is **not** touched: in the ground-truth model the
    /// `SegmentIndex` is a fixed function of the atom hash
    /// ([`atom_to_cix`]), so segments do not rotate per message.
    pub fn advance_segment(&mut self) {
        self.refs.clear();
        self.assigned.clear();
        self.long_atoms = false;
    }

    /// Drops all state (a full connection reset): the per-message ref list *and*
    /// the persistent cache.
    pub fn clear(&mut self) {
        self.advance_segment();
        self.persistent.clear();
    }

    /// The number of refs accumulated for the message being built.
    pub fn len(&self) -> usize {
        self.refs.len()
    }

    /// Whether the message being built has no refs yet.
    pub fn is_empty(&self) -> bool {
        self.refs.is_empty()
    }

    /// The persistent per-connection cache (`out_arr`).
    pub fn persistent(&self) -> &SegmentCache {
        &self.persistent
    }
}

/// The concrete [`beam_codec::AtomCacheWriter`] for the outbound cache.
///
/// [`Encoder::with_atom_cache`](beam_codec::Encoder::with_atom_cache) installs it,
/// making the encoder emit `ATOM_CACHE_REF` (`82`) + the returned index for every
/// atom the message references instead of the inline atom text. It performs the
/// exact classification of [`ref_for`](OutboundAtomCache::ref_for) — registering
/// the ref (deciding [`New`](AtomCacheRef::New) vs [`Cached`](AtomCacheRef::Cached),
/// enforcing the per-message ref budget and refusing a slot collision) — and then
/// returns the resulting [`payload_index`](OutboundAtomCache::payload_index). A
/// [`None`] return means "not cacheable — write the atom text inline", which is C's
/// `get_iix_acache_map` returning `-1`.
///
/// The index it returns is the atom's position in
/// [`header`](OutboundAtomCache::header)`().refs`, i.e. exactly the payload
/// `AtomCacheReferenceIndex` a receiver resolves after applying that header. Because
/// the encoder walks the payload in order, the positions are assigned in that order.
impl beam_codec::AtomCacheWriter for OutboundAtomCache {
    fn intern(&mut self, atom: Atom, atoms: &AtomTable) -> Option<u8> {
        self.ref_for(atom, atoms)?;
        self.payload_index(atom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an `Atom` from a raw index. `Atom` is an opaque `Copy` wrapper over
    /// the atom-table index, so distinct raw values are distinct atoms without
    /// needing a table.
    fn atom(raw: u32) -> Atom {
        Atom::from_raw(raw)
    }

    fn bad_arg<T: std::fmt::Debug>(res: Result<T>) -> bool {
        matches!(res, Err(BeamError::BadArg))
    }

    // -----------------------------------------------------------------------
    // Address arithmetic
    // -----------------------------------------------------------------------

    #[test]
    fn constants_match_erts() {
        assert_eq!(ENTRIES_PER_SEGMENT, 256);
        assert_eq!(ATOM_CACHE_SIZE, 2048);
        assert_eq!(ATOM_CACHE_SEGMENTS, 8);
        assert_eq!(USE_ATOM_CACHE_SIZE, 2039);
        assert!(USE_ATOM_CACHE_SIZE < ATOM_CACHE_SIZE);
    }

    #[test]
    fn slot_index_is_segment_major() {
        assert_eq!(slot_index(0, 0).unwrap(), 0);
        assert_eq!(slot_index(0, 255).unwrap(), 255);
        assert_eq!(slot_index(1, 0).unwrap(), 256);
        assert_eq!(slot_index(3, 42).unwrap(), (3 << 8) | 42);
        assert_eq!(slot_index(7, 255).unwrap(), ATOM_CACHE_SIZE - 1);
    }

    #[test]
    fn slot_index_rejects_a_segment_out_of_range() {
        assert!(bad_arg(slot_index(8, 0)));
        assert!(bad_arg(slot_index(200, 0)));
        assert!(slot_index(7, 0).is_ok());
    }

    #[test]
    fn atom_to_cix_is_the_erts_hash() {
        // The fixed hash `atom_val % ERTS_USE_ATOM_CACHE_SIZE`.
        assert_eq!(atom_to_cix(atom(0)), 0);
        assert_eq!(atom_to_cix(atom(255)), 255);
        assert_eq!(atom_to_cix(atom(256)), 256);
        assert_eq!(atom_to_cix(atom(2039)), 0);
        assert_eq!(atom_to_cix(atom(2040)), 1);
        assert!(atom_to_cix(Atom::MAX) < USE_ATOM_CACHE_SIZE);
    }

    #[test]
    fn segment_cache_round_trips_and_clears() {
        let mut cache = SegmentCache::new();
        assert_eq!(cache.occupied(), 0);
        assert_eq!(cache.get(slot_index(1, 7).unwrap()), None);

        cache.set(slot_index(1, 7).unwrap(), atom(1));
        cache.set(slot_index(0, 0).unwrap(), atom(2));
        assert_eq!(cache.get(slot_index(1, 7).unwrap()), Some(atom(1)));
        assert_eq!(cache.get(slot_index(0, 0).unwrap()), Some(atom(2)));
        assert_eq!(cache.occupied(), 2);

        // Overwrite, then reset.
        cache.set(slot_index(1, 7).unwrap(), atom(3));
        assert_eq!(cache.get(slot_index(1, 7).unwrap()), Some(atom(3)));
        cache.clear();
        assert_eq!(cache.get(slot_index(1, 7).unwrap()), None);
        assert_eq!(cache.occupied(), 0);

        // An out-of-range slot reads as unset and is ignored by `set` (no panic).
        assert_eq!(cache.get(ATOM_CACHE_SIZE), None);
        assert_eq!(cache.get(usize::MAX), None);
        cache.set(usize::MAX, atom(9));
        assert_eq!(cache.occupied(), 0);
    }

    // -----------------------------------------------------------------------
    // Receive side
    // -----------------------------------------------------------------------

    #[test]
    fn apply_installs_new_refs_at_their_position() {
        let mut persistent = SegmentCache::new();
        let mut table = AtomCache::new();
        let header = DistHeader {
            refs: vec![
                AtomCacheRef::New { segment: 3, index: 42, atom: atom(7) },
                AtomCacheRef::New { segment: 0, index: 1, atom: atom(8) },
            ],
            long_atoms: false,
        };

        apply_header(&header, &mut persistent, &mut table).expect("well-formed header");

        // The translation table is indexed by ref *position*.
        assert_eq!(table.get(0), Some(atom(7)));
        assert_eq!(table.get(1), Some(atom(8)));
        // ... and the atoms were installed at their addressed slots.
        assert_eq!(persistent.get(slot_index(3, 42).unwrap()), Some(atom(7)));
        assert_eq!(persistent.get(slot_index(0, 1).unwrap()), Some(atom(8)));
    }

    #[test]
    fn apply_cached_ref_addresses_the_persistent_cache() {
        let mut persistent = SegmentCache::new();
        let mut table = AtomCache::new();

        let first = DistHeader {
            refs: vec![AtomCacheRef::New { segment: 5, index: 200, atom: atom(11) }],
            long_atoms: false,
        };
        apply_header(&first, &mut persistent, &mut table).expect("new ref");

        // A later header may *select* the slot with a `Cached` ref.
        let second = DistHeader {
            refs: vec![AtomCacheRef::Cached { segment: 5, index: 200 }],
            long_atoms: false,
        };
        apply_header(&second, &mut persistent, &mut table).expect("cached ref");
        assert_eq!(table.get(0), Some(atom(11)));
    }

    #[test]
    fn apply_cached_ref_to_an_unset_slot_is_bad_arg() {
        let mut persistent = SegmentCache::new();
        let mut table = AtomCache::new();
        let header = DistHeader {
            refs: vec![AtomCacheRef::Cached { segment: 2, index: 9 }],
            long_atoms: false,
        };
        assert!(bad_arg(apply_header(&header, &mut persistent, &mut table)));
    }

    #[test]
    fn apply_rejects_a_segment_beyond_the_cache() {
        // Hand-built refs are the only way to get a 3-bit field out of range.
        let mut persistent = SegmentCache::new();
        let mut table = AtomCache::new();
        let header = DistHeader {
            refs: vec![AtomCacheRef::New { segment: 8, index: 0, atom: atom(1) }],
            long_atoms: false,
        };
        assert!(bad_arg(apply_header(&header, &mut persistent, &mut table)));
    }

    #[test]
    fn apply_rejects_a_header_with_too_many_refs() {
        let mut persistent = SegmentCache::new();
        let mut table = AtomCache::new();
        let refs = (0..=MAX_ATOM_CACHE_REFS)
            .map(|i| AtomCacheRef::Cached { segment: 0, index: i as u8 })
            .collect();
        let header = DistHeader { refs, long_atoms: false };
        assert!(matches!(
            apply_header(&header, &mut persistent, &mut table),
            Err(BeamError::SystemLimit)
        ));
    }

    #[test]
    fn applying_a_header_clears_stale_table_slots() {
        let mut cache = ReceiveAtomCache::new();
        let two = DistHeader {
            refs: vec![
                AtomCacheRef::New { segment: 0, index: 0, atom: atom(1) },
                AtomCacheRef::New { segment: 0, index: 1, atom: atom(2) },
            ],
            long_atoms: false,
        };
        cache.apply(&two).expect("two refs");
        assert_eq!(cache.ref_count(), 2);
        assert_eq!(cache.table().get(1), Some(atom(2)));

        // A one-ref header must not leave ref 0's sibling (slot 1) reachable.
        let one = DistHeader {
            refs: vec![AtomCacheRef::New { segment: 0, index: 0, atom: atom(3) }],
            long_atoms: false,
        };
        cache.apply(&one).expect("one ref");
        assert_eq!(cache.ref_count(), 1);
        assert_eq!(cache.table().get(0), Some(atom(3)));
        assert_eq!(cache.table().get(1), None);

        // An empty header empties the table and the persistent store stays put.
        cache.apply(&DistHeader::empty()).expect("empty header");
        assert_eq!(cache.ref_count(), 0);
        assert_eq!(cache.table().get(0), None);
        assert_eq!(cache.persistent().get(slot_index(0, 0).unwrap()), Some(atom(3)));

        cache.clear();
        assert_eq!(cache.persistent().get(slot_index(0, 0).unwrap()), None);
        assert_eq!(cache.ref_count(), 0);
    }

    // -----------------------------------------------------------------------
    // Send side
    // -----------------------------------------------------------------------

    #[test]
    fn outbound_first_use_is_new_and_a_later_message_is_cached() {
        let atoms = AtomTable::new();
        let mut outbound = OutboundAtomCache::new();

        let first = outbound.ref_for(atom(100), &atoms).expect("first use is cacheable");
        let slot = atom_to_cix(atom(100));
        assert_eq!(
            first,
            AtomCacheRef::New {
                segment: (slot >> 8) as u8,
                index: (slot & 0xff) as u8,
                atom: atom(100),
            }
        );
        assert_eq!(outbound.payload_index(atom(100)), Some(0));
        assert_eq!(outbound.header().refs, vec![first]);

        // A new message re-references the same atom: same slot, now `Cached`.
        outbound.advance_segment();
        let second = outbound.ref_for(atom(100), &atoms).expect("re-use is cacheable");
        assert_eq!(
            second,
            AtomCacheRef::Cached { segment: (slot >> 8) as u8, index: (slot & 0xff) as u8 }
        );
        assert_eq!(outbound.header().refs, vec![second]);
    }

    #[test]
    fn outbound_dedups_repeated_atoms_within_a_message() {
        let atoms = AtomTable::new();
        let mut outbound = OutboundAtomCache::new();
        let first = outbound.ref_for(atom(1), &atoms).unwrap();
        let again = outbound.ref_for(atom(1), &atoms).unwrap();
        assert_eq!(first, again);
        assert_eq!(outbound.len(), 1, "a repeated atom does not add a second ref");
        assert_eq!(outbound.payload_index(atom(1)), Some(0));
    }

    #[test]
    fn outbound_reports_the_segment_boundary() {
        // `atom_to_cix` is the identity for the first 2039 raw indices, so raw 255
        // inhabits the last slot of segment 0 and raw 256 the first of segment 1.
        let atoms = AtomTable::new();
        let mut outbound = OutboundAtomCache::new();
        assert_eq!(
            outbound.ref_for(atom(255), &atoms).unwrap(),
            AtomCacheRef::New { segment: 0, index: 255, atom: atom(255) }
        );
        assert_eq!(
            outbound.ref_for(atom(256), &atoms).unwrap(),
            AtomCacheRef::New { segment: 1, index: 0, atom: atom(256) }
        );
        // Feeding >256 distinct atoms crosses the 8×256 space's first boundary and
        // keeps assigning correctly.
        assert_eq!(
            outbound.ref_for(atom(511), &atoms).unwrap(),
            AtomCacheRef::New { segment: 1, index: 255, atom: atom(511) }
        );
        assert_eq!(
            outbound.ref_for(atom(512), &atoms).unwrap(),
            AtomCacheRef::New { segment: 2, index: 0, atom: atom(512) }
        );
    }

    #[test]
    fn outbound_refuses_a_slot_collision() {
        // 0 and 2039 both hash to slot 0 (2039 % 2039 == 0).
        let atoms = AtomTable::new();
        let mut outbound = OutboundAtomCache::new();
        assert!(outbound.ref_for(atom(0), &atoms).is_some());
        assert_eq!(outbound.ref_for(atom(2039), &atoms), None);
        assert_eq!(outbound.len(), 1);
        assert_eq!(outbound.payload_index(atom(2039)), None);
        // The first atom still resolves to its own index.
        assert_eq!(outbound.payload_index(atom(0)), Some(0));
    }

    #[test]
    fn outbound_stops_at_the_ref_budget() {
        let atoms = AtomTable::new();
        let mut outbound = OutboundAtomCache::new();
        // 255 distinct atoms with distinct slots (raw 0..=254 hash to themselves).
        for raw in 0..MAX_ATOM_CACHE_REFS as u32 {
            assert!(outbound.ref_for(atom(raw), &atoms).is_some(), "raw {raw}");
        }
        assert_eq!(outbound.len(), MAX_ATOM_CACHE_REFS);
        // The next distinct atom cannot be cached; it must be written inline.
        assert_eq!(outbound.ref_for(atom(255), &atoms), None);
        assert_eq!(outbound.len(), MAX_ATOM_CACHE_REFS);
        // An atom already in the message is still returned.
        assert!(outbound.ref_for(atom(0), &atoms).is_some());
    }

    #[test]
    fn outbound_payload_ref_for_emits_the_wire_bytes() {
        let atoms = AtomTable::new();
        let mut outbound = OutboundAtomCache::new();
        assert_eq!(outbound.payload_ref_for(atom(5), &atoms), Some([82, 0]));
        assert_eq!(outbound.payload_ref_for(atom(6), &atoms), Some([82, 1]));
        // A repeated atom reuses its index.
        assert_eq!(outbound.payload_ref_for(atom(5), &atoms), Some([82, 0]));
    }

    #[test]
    fn outbound_sets_long_atoms_for_an_over_255_byte_name() {
        let mut atoms = AtomTable::new();
        let short = atoms.intern("short");
        let long = atoms.intern(&"a".repeat(300));

        let mut outbound = OutboundAtomCache::new();
        outbound.ref_for(short, &atoms).unwrap();
        assert!(!outbound.header().long_atoms);

        outbound.advance_segment();
        outbound.ref_for(long, &atoms).unwrap();
        assert!(outbound.header().long_atoms);

        // A 255-byte name still fits the 1-byte length field.
        let mut atoms255 = AtomTable::new();
        let edge = atoms255.intern(&"b".repeat(255));
        let mut edge_out = OutboundAtomCache::new();
        edge_out.ref_for(edge, &atoms255).unwrap();
        assert!(!edge_out.header().long_atoms);
    }

    #[test]
    fn take_header_advances_the_message_but_keeps_the_persistent_cache() {
        let atoms = AtomTable::new();
        let mut outbound = OutboundAtomCache::new();
        let new = outbound.ref_for(atom(42), &atoms).unwrap();
        let header = outbound.take_header();
        assert_eq!(header.refs, vec![new]);
        assert!(outbound.is_empty());

        // The persistent cache survived: the next message sees a `Cached` ref.
        let cached = outbound.ref_for(atom(42), &atoms).unwrap();
        assert!(!cached.is_new());
        assert_eq!(cached.segment(), new.segment());
        assert_eq!(cached.index(), new.index());

        // A full reset drops it, so the atom is learned anew.
        outbound.clear();
        assert_eq!(outbound.ref_for(atom(42), &atoms).unwrap(), new);
        assert_eq!(outbound.persistent().occupied(), 1);
    }

    // -----------------------------------------------------------------------
    // The `beam_codec::AtomCacheWriter` impl (`intern`)
    // -----------------------------------------------------------------------

    /// Calls [`beam_codec::AtomCacheWriter::intern`] without importing the trait.
    fn intern(cache: &mut OutboundAtomCache, atom: Atom, atoms: &AtomTable) -> Option<u8> {
        beam_codec::AtomCacheWriter::intern(cache, atom, atoms)
    }

    #[test]
    fn intern_returns_the_ref_position_and_repeats_reuse_it() {
        let atoms = AtomTable::new();
        let mut outbound = OutboundAtomCache::new();

        // The returned index is the ref's *position* in `header().refs`, in the
        // order the encoder consults the cache (which is payload order).
        assert_eq!(intern(&mut outbound, atom(5), &atoms), Some(0));
        assert_eq!(intern(&mut outbound, atom(6), &atoms), Some(1));
        // A repeated atom re-uses its original position.
        assert_eq!(intern(&mut outbound, atom(5), &atoms), Some(0));
        assert_eq!(outbound.len(), 2);

        let header = outbound.header();
        assert_eq!(header.refs.len(), 2);
        assert_eq!(header.refs[0], AtomCacheRef::New { segment: 0, index: 5, atom: atom(5) });
        assert_eq!(header.refs[1], AtomCacheRef::New { segment: 0, index: 6, atom: atom(6) });
    }

    #[test]
    fn intern_refuses_a_slot_collision() {
        let atoms = AtomTable::new();
        let mut outbound = OutboundAtomCache::new();
        // 0 and 2039 both hash to slot 0; the second cannot be cached in this message.
        assert_eq!(intern(&mut outbound, atom(0), &atoms), Some(0));
        assert_eq!(intern(&mut outbound, atom(2039), &atoms), None);
        assert_eq!(outbound.len(), 1);
        // The first atom keeps its position.
        assert_eq!(intern(&mut outbound, atom(0), &atoms), Some(0));
    }

    #[test]
    fn intern_stops_at_the_ref_budget() {
        let atoms = AtomTable::new();
        let mut outbound = OutboundAtomCache::new();
        // 255 distinct atoms with distinct slots (raw 0..=254 hash to themselves)
        // get positions 0..=254.
        for raw in 0..MAX_ATOM_CACHE_REFS as u32 {
            assert_eq!(intern(&mut outbound, atom(raw), &atoms), Some(raw as u8), "raw {raw}");
        }
        assert_eq!(outbound.len(), MAX_ATOM_CACHE_REFS);
        // The 256th distinct atom cannot be cached; the caller writes it inline.
        assert_eq!(intern(&mut outbound, atom(255), &atoms), None);
        assert_eq!(outbound.len(), MAX_ATOM_CACHE_REFS);
        // An atom already in the message is still returned.
        assert_eq!(intern(&mut outbound, atom(0), &atoms), Some(0));
    }
}
