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

//! Integration tests for the crate's **M1 internal-reference thing** layout
//! ([`write_internal_ref`] / [`internal_ref_id`]) — the frozen 4-word
//! `[make_header(3, REF_SUBTAG), node, serial, id]` block.
//!
//! These exercise the public surface only (the writer, the accessor, the layout
//! constants and the term-ordering predicates), and they live here rather than in
//! `src/boxed.rs`'s in-module test module to keep that production file under the
//! ~1000-line guideline — the same reason `tests/compare_terms.rs` is separate.
//!
//! `beam_term` is a dependency sink, so the byte-exact shape is pinned here as
//! literal words rather than by cross-checking the sibling producers
//! (`beam_process`'s `MonitorRef::to_eterm`, `beam_bifs`'s `make_ref_thing`, and
//! `beam_rt`'s native floor) — a drift in any of them shows up as a failure here.

use core::cmp::Ordering;

use beam_term::boxed::{
    internal_ref_id, is_internal_ref, make_arityval, make_boxed, make_header, write_internal_ref,
    INTERNAL_REF_PAYLOAD_WORDS, INTERNAL_REF_WORDS,
};
use beam_term::immediate::{make_small, signed_value, MAX_SMALL, NIL};use beam_term::ops::THE_NON_VALUE;
use beam_term::size::size_object;
use beam_term::tag::REF_SUBTAG;
use beam_term::{cmp, eq, AtomTable, Eterm, TermError};

/// Builds an M1 internal reference with `id` over a caller-owned block.
///
/// # Safety
///
/// Not `unsafe` itself: the block is a local `[Eterm; INTERNAL_REF_WORDS]` the
/// caller keeps alive, which is exactly the writer's contract.
fn m1_ref(words: &mut [Eterm; INTERNAL_REF_WORDS], id: u64) -> Eterm {
    // SAFETY: `words` is a live, aligned, exclusively-owned array of exactly
    // `INTERNAL_REF_WORDS` terms — the writer's documented requirement.
    unsafe { write_internal_ref(words.as_mut_ptr(), id) }.expect("id fits a small integer")
}

/// Reads the id of a term built by [`m1_ref`].
fn id_of(t: Eterm) -> u64 {
    // SAFETY: every `t` handed to this helper comes from `m1_ref`.
    unsafe { internal_ref_id(t) }.expect("an M1 internal reference")
}

/// The writer pins the exact frozen words, byte for byte.
#[test]
fn internal_ref_writer_pins_the_exact_frozen_words() {
    assert_eq!(INTERNAL_REF_PAYLOAD_WORDS, 3);
    assert_eq!(INTERNAL_REF_WORDS, 4);

    let mut words = [Eterm::default(); INTERNAL_REF_WORDS];
    let t = m1_ref(&mut words, 7);

    // The whole point: the four words are literally this — header, node, serial, id.
    assert_eq!(
        words,
        [
            make_header(3, REF_SUBTAG),
            make_small(0),
            make_small(0),
            make_small(7)
        ]
    );
    // ...and the raw encodings, so a constant drift is caught even if both sides move.
    assert_eq!(words[0].raw(), 0xD0); // (3 << 6) | REF_SUBTAG(0x10)
    assert_eq!(words[1].raw() & 0xF, 0xF); // TAG_IMMED1_SMALL
    assert_eq!(words[2].raw() & 0xF, 0xF);
    assert_eq!(signed_value(words[3]), 7);

    // The writer hands back the tagged root of the block it filled.
    assert_eq!(t, make_boxed(words.as_ptr()));
    // ...and the loose subtag-only predicate accepts it (which is exactly why
    // `internal_ref_id` also checks the header arity; see the layout docs).
    assert!(unsafe { is_internal_ref(t) });
    assert_eq!(beam_term::boxed::header_arity(words[0]), INTERNAL_REF_PAYLOAD_WORDS);
    assert_eq!(id_of(t), 7);
}

/// Writer → accessor round-trip over the id range, boundaries included.
#[test]
fn internal_ref_id_round_trips_over_the_id_range() {
    let ids = [
        0u64,
        1,
        2,
        255,
        256,
        4096,
        1 << 30,
        MAX_SMALL as u64 - 1,
        MAX_SMALL as u64,
    ];
    for id in ids {
        let mut words = [Eterm::default(); INTERNAL_REF_WORDS];
        let t = m1_ref(&mut words, id);
        assert_eq!(id_of(t), id, "round-trip of id {id}");
        // The id always lands in the last word; node and serial stay zeroed.
        assert_eq!(signed_value(words[3]), id as i64);
        assert_eq!(words[1], make_small(0));
        assert_eq!(words[2], make_small(0));
    }
}

/// An id that does not fit the single small-integer word is a typed error.
#[test]
fn internal_ref_writer_refuses_an_id_that_does_not_fit_a_small() {
    for bad in [MAX_SMALL as u64 + 1, 1u64 << 59, 1u64 << 63, u64::MAX] {
        let mut words = [Eterm::default(); INTERNAL_REF_WORDS];
        // SAFETY: the block is the writer's documented size/aliveness contract.
        let result = unsafe { write_internal_ref(words.as_mut_ptr(), bad) };
        assert_eq!(
            result,
            Err(TermError::SmallIntOutOfRange),
            "id {bad} must not be silently truncated"
        );
    }
}

/// The accessor refuses every shape that is not an M1 internal reference.
#[test]
fn internal_ref_id_refuses_every_other_term_shape() {
    // Not a term at all, and immediates.
    assert_eq!(unsafe { internal_ref_id(THE_NON_VALUE) }, Err(TermError::BadRef));
    assert_eq!(unsafe { internal_ref_id(NIL) }, Err(TermError::BadRef));
    assert_eq!(unsafe { internal_ref_id(make_small(1)) }, Err(TermError::BadRef));

    // A boxed term of another kind (a tuple) — never dereferenced for an id.
    let tuple_words = [make_arityval(1), make_small(3)];
    let tuple = make_boxed(tuple_words.as_ptr());
    assert_eq!(unsafe { internal_ref_id(tuple) }, Err(TermError::BadRef));

    // A **C-layout** ref block (`ErtsORefThing`: header arity 2, three words, the
    // shape `internal_ref_numbers` reads). `is_internal_ref` accepts it (subtag
    // only), but its M1 id word does not exist — the accessor must refuse rather
    // than read past the block.
    let c_words = [
        make_header(2, REF_SUBTAG),
        Eterm::from_raw(0xffff_ffff_0000_0000),
        Eterm::from_raw(0x0000_0002_0000_0001),
    ];
    let c_layout = make_boxed(c_words.as_ptr());
    assert!(unsafe { is_internal_ref(c_layout) });
    assert_eq!(unsafe { internal_ref_id(c_layout) }, Err(TermError::BadRef));

    // An M1 block (arity 3) whose id word is not a non-negative small integer:
    // `NIL` (an immediate, but not a small), a negative small, and a boxed word.
    for bad_id in [NIL, make_small(-1), make_boxed(c_words.as_ptr())] {
        let words = [
            make_header(INTERNAL_REF_PAYLOAD_WORDS, REF_SUBTAG),
            make_small(0),
            make_small(0),
            bad_id,
        ];
        let t = make_boxed(words.as_ptr());
        assert!(unsafe { is_internal_ref(t) });
        assert_eq!(unsafe { internal_ref_id(t) }, Err(TermError::BadRef));
    }
}
/// Two references with the same id are `=:=` (so a `Ref` survives a copy and a
/// `term_to_binary`/`binary_to_term` round-trip); different ids are ordered.
#[test]
fn internal_refs_with_the_same_id_are_exactly_equal() {
    let atoms = AtomTable::new();
    let mut a = [Eterm::default(); INTERNAL_REF_WORDS];
    let mut b = [Eterm::default(); INTERNAL_REF_WORDS];
    let mut c = [Eterm::default(); INTERNAL_REF_WORDS];
    let ra = m1_ref(&mut a, 42);
    let rb = m1_ref(&mut b, 42);
    let rc = m1_ref(&mut c, 43);

    // Distinct blocks (different addresses), same id: `=:=` (`eq` is `=:=`).
    assert_ne!(ra.raw(), rb.raw());
    assert!(eq(ra, rb, &atoms));
    assert_eq!(cmp(ra, rb, &atoms), Ordering::Equal);
    assert_eq!(cmp(rb, ra, &atoms), Ordering::Equal);
    assert_eq!(id_of(ra), id_of(rb));

    // Different id: never equal, and ordered by the id word (compared first).
    assert!(!eq(ra, rc, &atoms));
    assert_eq!(cmp(ra, rc, &atoms), Ordering::Less);
    assert_eq!(cmp(rc, ra, &atoms), Ordering::Greater);
    assert_eq!(cmp(ra, rc, &atoms), cmp(rc, ra, &atoms).reverse());
    assert_eq!(cmp(ra, ra, &atoms), Ordering::Equal);
}

/// The block is the frozen four words for the object-size walk too.
#[test]
fn internal_ref_block_sizes_as_the_frozen_four_words() {
    let mut words = [Eterm::default(); INTERNAL_REF_WORDS];
    let t = m1_ref(&mut words, 1);
    assert_eq!(size_object(t), INTERNAL_REF_WORDS);
}
