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

//! Distribution atom-cache send/receive integration.
//!
//! Exercises the two cache levels end to end over the value layer: a header built
//! by [`OutboundAtomCache`] is encoded with [`DistHeader::encode`], decoded and
//! applied by [`ReceiveAtomCache`], and the payload's `ATOM_CACHE_REF` (`82`) tags
//! then resolve through `beam_codec` (`decode_control`). Everything is hermetic —
//! no sockets, no running node.
//!
//! Ground truth: `erts/emulator/beam/external.c` (the `attab` resolution, the
//! sender's `atom2cix`/`insert_acache_map`/`get_iix_acache_map` and the
//! encode/finalize split), `erts/emulator/beam/external.h` and
//! `erts/doc/guides/erl_ext_dist.md` §"Normal Distribution Header".

use beam_alloc::Heap;
use beam_codec::{AtomEncoding, ATOM_CACHE_REF_EXT, SMALL_INTEGER_EXT, SMALL_TUPLE_EXT, VERSION_MAGIC};
use beam_common::{BeamError, Result};
use beam_dist::atom_cache::{
    apply_header, atom_to_cix, slot_index, OutboundAtomCache, ReceiveAtomCache, SegmentCache,
    ATOM_CACHE_SIZE, ENTRIES_PER_SEGMENT,
};
use beam_dist::control::{decode_control, encode_control_cached, ControlMessage};
use beam_dist::dist_header::{AtomCacheRef, DistHeader};
use beam_dist::flags::{Dop, DOP_REG_SEND, DOP_SEND};
use beam_dist::MemoryTransport;
use beam_term::prelude::{make_atom, make_small};
use beam_term::{Atom, AtomTable};

/// Encodes `header` exactly as it appears on the wire (`VERSION_MAGIC + 'D' + …`).
fn encode_header(header: &DistHeader, atoms: &AtomTable) -> Vec<u8> {
    let mut wire = MemoryTransport::new();
    header
        .encode(&mut wire, atoms, AtomEncoding::Utf8)
        .expect("a well-formed header must encode");
    wire.into_written()
}

/// A receiving node: its heap, its atom table and its atom cache.
struct Receiver {
    heap: Heap,
    atoms: AtomTable,
    cache: ReceiveAtomCache,
}

impl Receiver {
    fn new() -> Receiver {
        Receiver {
            heap: Heap::new(1024).expect("1024-word test heap"),
            atoms: AtomTable::new(),
            cache: ReceiveAtomCache::new(),
        }
    }

    /// Decodes `header_bytes` into this node, applies it to its atom cache and
    /// then decodes the control `body`, resolving `ATOM_CACHE_REF`s through the
    /// resulting per-message translation table.
    fn receive(&mut self, header_bytes: &[u8], body: &[u8]) -> Result<ControlMessage> {
        let header = DistHeader::decode(header_bytes, &mut self.atoms, AtomEncoding::Utf8)?;
        self.cache.apply(&header)?;
        let table = self.cache.table();
        decode_control(body, &mut self.heap, &mut self.atoms, Some(table))
    }
}

/// A `DOP_SEND` control message whose second argument is an `ATOM_CACHE_REF` at
/// `index` (the first argument is the plain small integer `7`). The message is the
/// real tuple form `{DOP_SEND, Arg1, Arg2}`.
fn send_body_with_ref(index: u8) -> Vec<u8> {
    vec![
        SMALL_TUPLE_EXT,
        3,
        SMALL_INTEGER_EXT,
        DOP_SEND,
        SMALL_INTEGER_EXT,
        7,
        ATOM_CACHE_REF_EXT,
        index,
    ]
}

/// A `DOP_SEND` control message whose first argument is an inline small integer.
fn send_body_with_int(value: u8) -> Vec<u8> {
    vec![
        SMALL_TUPLE_EXT,
        3,
        SMALL_INTEGER_EXT,
        DOP_SEND,
        SMALL_INTEGER_EXT,
        value,
        SMALL_INTEGER_EXT,
        1,
    ]
}

// ---------------------------------------------------------------------------
// The two cache levels
// ---------------------------------------------------------------------------

#[test]
fn cache_constants_describe_the_wire() {
    // 8 segments × 256 entries, the sender hashing into (a subset of) 2048 slots.
    assert_eq!(ATOM_CACHE_SIZE, 2048);
    assert_eq!(ENTRIES_PER_SEGMENT, 256);
    assert_eq!(ATOM_CACHE_SIZE, 8 * ENTRIES_PER_SEGMENT);
    assert_eq!(slot_index(7, 255).unwrap(), ATOM_CACHE_SIZE - 1);
    assert!(atom_to_cix(Atom::MAX) < ATOM_CACHE_SIZE);
}

#[test]
fn two_refs_with_the_same_index_in_different_segments_are_distinct() {
    // `(segment, index)` addresses the persistent cache; the *translation table*
    // is addressed by position. Same index, different segment => different atoms.
    let mut persistent = SegmentCache::new();
    let mut table = beam_codec::AtomCache::new();
    let header = DistHeader {
        refs: vec![
            AtomCacheRef::New { segment: 0, index: 5, atom: Atom::from_raw(100) },
            AtomCacheRef::New { segment: 1, index: 5, atom: Atom::from_raw(200) },
        ],
        long_atoms: false,
    };

    apply_header(&header, &mut persistent, &mut table).expect("two installable refs");

    assert_eq!(table.get(0), Some(Atom::from_raw(100)));
    assert_eq!(table.get(1), Some(Atom::from_raw(200)));
    assert_eq!(persistent.get(slot_index(0, 5).unwrap()), Some(Atom::from_raw(100)));
    assert_eq!(persistent.get(slot_index(1, 5).unwrap()), Some(Atom::from_raw(200)));
}

// ---------------------------------------------------------------------------
// Receive side
// ---------------------------------------------------------------------------

#[test]
fn a_new_ref_teaches_the_receiver_an_atom_a_payload_ref_resolves() {
    let mut sender_atoms = AtomTable::new();
    let hello = sender_atoms.intern("hello");

    let mut outbound = OutboundAtomCache::new();
    let cache_ref = outbound.ref_for(hello, &sender_atoms).expect("first use is cacheable");
    assert!(cache_ref.is_new(), "the first message must teach the atom");
    let header_bytes = encode_header(&outbound.take_header(), &sender_atoms);

    let mut rx = Receiver::new();
    let message = rx
        .receive(&header_bytes, &send_body_with_ref(0))
        .expect("the payload ref must resolve");

    assert_eq!(message.dop, Dop::Send);
    let expected = rx.atoms.lookup("hello").expect("the header interned `hello`");
    assert_eq!(message.terms, vec![make_small(7), make_atom(expected)]);
}

#[test]
fn a_later_header_re_uses_the_atoms_of_an_earlier_one() {
    let mut sender_atoms = AtomTable::new();
    let alpha = sender_atoms.intern("alpha");
    let beta = sender_atoms.intern("beta");

    let mut outbound = OutboundAtomCache::new();
    assert!(outbound.ref_for(alpha, &sender_atoms).unwrap().is_new());
    assert!(outbound.ref_for(beta, &sender_atoms).unwrap().is_new());
    let first_header = encode_header(&outbound.take_header(), &sender_atoms);

    // A second message: both atoms are now in the persistent cache, so the header
    // only carries `Cached` refs (no atom text).
    outbound.advance_segment();
    assert!(!outbound.ref_for(alpha, &sender_atoms).unwrap().is_new());
    assert!(!outbound.ref_for(beta, &sender_atoms).unwrap().is_new());
    let second_header = encode_header(&outbound.take_header(), &sender_atoms);
    assert!(
        second_header.len() < first_header.len(),
        "a cached-only header carries no atom text"
    );

    let mut rx = Receiver::new();
    // Warm the persistent cache with the first header, then use the second: the
    // second header carries no atom text, so only the cache can resolve them.
    rx.receive(&first_header, &send_body_with_int(1))
        .expect("first message decodes");

    // `DOP_SEND` has arity 2: both cached refs must resolve.
    let body = vec![
        SMALL_TUPLE_EXT,
        3,
        SMALL_INTEGER_EXT,
        DOP_SEND,
        ATOM_CACHE_REF_EXT,
        0,
        ATOM_CACHE_REF_EXT,
        1,
    ];
    let message = rx
        .receive(&second_header, &body)
        .expect("both cached refs resolve");
    assert_eq!(
        message.terms,
        vec![
            make_atom(rx.atoms.lookup("alpha").unwrap()),
            make_atom(rx.atoms.lookup("beta").unwrap()),
        ]
    );
}

#[test]
fn a_cached_ref_to_an_unset_slot_is_bad_arg() {
    // Nothing has ever been installed at slot (4, 9).
    let header = DistHeader {
        refs: vec![AtomCacheRef::Cached { segment: 4, index: 9 }],
        long_atoms: false,
    };
    let header_bytes = encode_header(&header, &AtomTable::new());

    let mut rx = Receiver::new();
    let err = rx
        .receive(&header_bytes, &send_body_with_ref(0))
        .expect_err("a cached ref to an unset slot is malformed input");
    assert!(matches!(err, BeamError::BadArg), "got {err:?}");
}

#[test]
fn a_payload_ref_beyond_the_header_ref_count_is_bad_arg() {
    let mut sender_atoms = AtomTable::new();
    let a = sender_atoms.intern("a");
    let b = sender_atoms.intern("b");

    let mut outbound = OutboundAtomCache::new();
    outbound.ref_for(a, &sender_atoms).unwrap();
    outbound.ref_for(b, &sender_atoms).unwrap();
    let two_refs = encode_header(&outbound.take_header(), &sender_atoms);

    let mut rx = Receiver::new();
    // Index 1 resolves while the two-ref header is current ...
    rx.receive(&two_refs, &send_body_with_ref(1))
        .expect("index 1 resolves against a two-ref header");

    // ... but a one-ref header must not leave the stale slot 1 reachable.
    outbound.advance_segment();
    outbound.ref_for(a, &sender_atoms).unwrap();
    let one_ref = encode_header(&outbound.take_header(), &sender_atoms);

    let err = rx
        .receive(&one_ref, &send_body_with_ref(1))
        .expect_err("index 1 is beyond a one-ref header");
    assert!(matches!(err, BeamError::BadArg), "got {err:?}");
}

#[test]
fn an_empty_header_makes_payload_refs_unresolvable() {
    let empty = encode_header(&DistHeader::empty(), &AtomTable::new());
    let mut rx = Receiver::new();
    let err = rx
        .receive(&empty, &send_body_with_ref(0))
        .expect_err("no refs were taught");
    assert!(matches!(err, BeamError::BadArg), "got {err:?}");
    assert_eq!(rx.cache.ref_count(), 0);
}

#[test]
fn apply_rejects_an_out_of_range_segment() {
    // A wire-decoded ref always has a 3-bit segment; a hand-built one need not.
    let mut persistent = SegmentCache::new();
    let mut table = beam_codec::AtomCache::new();
    let header = DistHeader {
        refs: vec![AtomCacheRef::New { segment: 9, index: 0, atom: Atom::from_raw(1) }],
        long_atoms: false,
    };
    assert!(matches!(
        apply_header(&header, &mut persistent, &mut table),
        Err(BeamError::BadArg)
    ));
}

// ---------------------------------------------------------------------------
// Send side
// ---------------------------------------------------------------------------

#[test]
fn an_outbound_header_round_trips_over_the_wire() {
    let mut atoms = AtomTable::new();
    let a = atoms.intern("alpha");
    let b = atoms.intern("beta");

    let mut outbound = OutboundAtomCache::new();
    outbound.ref_for(a, &atoms).unwrap();
    outbound.ref_for(b, &atoms).unwrap();
    let header = outbound.take_header();

    let bytes = encode_header(&header, &atoms);
    assert_eq!(bytes[0], VERSION_MAGIC);
    assert_eq!(bytes[1], 68, "DIST_HEADER");

    // Decoding into the same table yields the very same refs (same atom handles).
    let decoded = DistHeader::decode(&bytes, &mut atoms, AtomEncoding::Utf8).expect("decodes");
    assert_eq!(decoded, header);
}

#[test]
fn outbound_refs_cross_the_segment_boundary_and_still_resolve() {
    // `atom2cix` is the identity for raw indices below 2039, so raw 255 is the
    // last slot of segment 0 and raw 256 the first of segment 1.
    let mut atoms = AtomTable::new();
    for i in 0..=256 {
        atoms.intern(&format!("n{i}"));
    }
    let last_of_segment_0 = Atom::from_raw(255);
    let first_of_segment_1 = Atom::from_raw(256);

    let mut outbound = OutboundAtomCache::new();
    assert_eq!(
        outbound.ref_for(last_of_segment_0, &atoms).unwrap(),
        AtomCacheRef::New { segment: 0, index: 255, atom: last_of_segment_0 }
    );
    assert_eq!(
        outbound.ref_for(first_of_segment_1, &atoms).unwrap(),
        AtomCacheRef::New { segment: 1, index: 0, atom: first_of_segment_1 }
    );
    let header_bytes = encode_header(&outbound.take_header(), &atoms);

    let mut rx = Receiver::new();
    let body = [
        SMALL_TUPLE_EXT,
        3,
        SMALL_INTEGER_EXT,
        DOP_SEND,
        ATOM_CACHE_REF_EXT, 0,
        ATOM_CACHE_REF_EXT, 1,
    ];
    let message = rx.receive(&header_bytes, &body).expect("both refs resolve");
    assert_eq!(
        message.terms,
        vec![
            make_atom(rx.atoms.lookup("n255").unwrap()),
            make_atom(rx.atoms.lookup("n256").unwrap()),
        ]
    );
    // ... and they landed in the right persistent slots of the receiver's cache.
    let persistent = rx.cache.persistent();
    assert_eq!(persistent.get(slot_index(0, 255).unwrap()), rx.atoms.lookup("n255"));
    assert_eq!(persistent.get(slot_index(1, 0).unwrap()), rx.atoms.lookup("n256"));
}

#[test]
fn long_atoms_headers_round_trip_and_apply() {
    let name = "L".repeat(300);
    let mut atoms = AtomTable::new();
    let long = atoms.intern(&name);

    let mut outbound = OutboundAtomCache::new();
    outbound.ref_for(long, &atoms).unwrap();
    let header = outbound.take_header();
    assert!(header.long_atoms, "a 300-byte name forces the 2-byte length field");

    let bytes = encode_header(&header, &atoms);
    let decoded = DistHeader::decode(&bytes, &mut atoms, AtomEncoding::Utf8).expect("decodes");
    assert_eq!(decoded, header);

    let mut rx = Receiver::new();
    let message = rx
        .receive(&bytes, &send_body_with_ref(0))
        .expect("the long atom resolves");
    assert_eq!(
        message.terms,
        vec![make_small(7), make_atom(rx.atoms.lookup(&name).unwrap())]
    );
}

#[test]
fn an_uncacheable_atom_is_emitted_inline_instead() {
    // The message budget is 255 refs; the 256th distinct atom is not cacheable.
    let mut atoms = AtomTable::new();
    for i in 0..=255 {
        atoms.intern(&format!("a{i}"));
    }

    let mut outbound = OutboundAtomCache::new();
    for raw in 0..255u32 {
        assert!(
            outbound.payload_ref_for(Atom::from_raw(raw), &atoms).is_some(),
            "raw {raw} is within the budget"
        );
    }
    assert_eq!(outbound.len(), 255);
    // Raw 255 hashes to slot 255 — a fresh, in-range slot — but the message is
    // full, so the caller must fall back to writing the atom text inline.
    assert_eq!(outbound.payload_ref_for(Atom::from_raw(255), &atoms), None);
    assert_eq!(outbound.len(), 255);
}

// ---------------------------------------------------------------------------
// The encoder seam: `OutboundAtomCache` drives `encode_control_cached`.
// ---------------------------------------------------------------------------

#[test]
fn encode_control_cached_writes_the_refs_its_header_teaches() {
    // The real `OutboundAtomCache`, installed on the encoder as the
    // `beam_codec::AtomCacheWriter`, must produce a payload the header it yields
    // resolves — i.e. `82, n` in the payload is header ref `n`.
    let mut sender_atoms = AtomTable::new();
    let from = make_atom(sender_atoms.intern("from"));
    let cookie = make_atom(sender_atoms.intern("cookie"));
    let to_name = make_atom(sender_atoms.intern("to_name"));

    let mut outbound = OutboundAtomCache::new();
    let body = encode_control_cached(
        Dop::RegSend,
        &[from, cookie, to_name],
        &sender_atoms,
        Some(&mut outbound),
    )
    .expect("encode with the outbound cache");
    let header_bytes = encode_header(&outbound.take_header(), &sender_atoms);

    // Every atom became an `ATOM_CACHE_REF` (`82`) + its sequential index; no atom
    // text is left in the payload.
    assert_eq!(
        body,
        vec![
            SMALL_TUPLE_EXT,
            4,
            SMALL_INTEGER_EXT,
            DOP_REG_SEND,
            ATOM_CACHE_REF_EXT,
            0,
            ATOM_CACHE_REF_EXT,
            1,
            ATOM_CACHE_REF_EXT,
            2,
        ]
    );

    // The receiver's header refs and the payload indices agree.
    let mut rx = Receiver::new();
    let message = rx.receive(&header_bytes, &body).expect("the refs resolve");
    assert_eq!(message.dop, Dop::RegSend);
    assert_eq!(
        message.terms,
        vec![
            make_atom(rx.atoms.lookup("from").unwrap()),
            make_atom(rx.atoms.lookup("cookie").unwrap()),
            make_atom(rx.atoms.lookup("to_name").unwrap()),
        ]
    );
}
