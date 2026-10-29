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

//! A deterministic, fuzz-shaped sweep over the **checked** bitstring readers
//! ([`try_bitstring_parts`], [`try_sub_bits_parts`], [`try_bit_at`]).
//!
//! The property under test is the one `beam_message`'s copier relies on: for *any* word
//! the readers are handed, they either answer a typed [`TermError`] or an `Ok` whose parts
//! are self-consistent — never a panic, never a wild pointer.
//!
//! # Why the sweep never manufactures a pointer outside its own buffer
//!
//! The check covers **shape**, not **provenance** (`beam_term/src/bitstring/checked.rs`,
//! module docs): a block that has been freed or moved is indistinguishable from a live one. So a
//! sweep that fed the readers *raw* randomly-generated words would, on the day the shape
//! check is correct, still dereference an arbitrary address the moment a random word
//! happens to look like a boxed term — that is an unavoidable consequence of the honest
//! limit, and it would make the test a segfault generator rather than a checker.
//!
//! The sweep is therefore split in two, and every pointer is confined to a buffer the test
//! owns (so the maximum dereference depth — `t` → its `orig` slot — is always in-bounds):
//!
//! 1. **raw words.** 20 000 LCG-drawn words are handed to the readers *as terms*. Every
//!    one that is not a boxed term (`is_boxed_term`, a pure word test, is the same guard
//!    the readers use) must be refused as [`TermError::NotATerm`]; the boxed-shaped ones
//!    cannot be dereferenced, and are covered by phase 2 instead.
//! 2. **mutated blocks.** 20 000 iterations build a real block in the buffer from a header
//!    menu (`HEADER_SUB_BITS`, heap-bits headers with agreeing and disagreeing arities, a
//!    tuple header, arbitrary words) with randomly mutated payload words, and every
//!    `orig`/`base_flags` pointer either null, poisoned, an arbitrary value, or an address
//!    inside the buffer. Each iteration then asserts the invariants below.
//!
//! Nothing here asserts on wall-clock time (the host is shared); the run is fully
//! deterministic — the LCG seed is fixed, so a failure reproduces exactly.

use beam_term::prelude::*;

/// Words in the sweep's scratch buffer.
const SLOTS: usize = 128;
/// Offset of the block under test (`[header, payload…]`).
const T: usize = 0;
/// Offset of the block a `SUB_BITS`'s `orig` slot may point at.
const O: usize = 16;
/// Offset of the payload byte area a `SUB_BITS`'s `base_flags` may point at.
const P: usize = 32;
/// Bytes of payload region (`(SLOTS - P) * 8`).
const P_BYTES: usize = (SLOTS - P) * 8;
/// The poison base: it must never become a read.
const POISON: usize = 0xDEAD_BEEF_DEAD_BEEF;
/// How many raw words phase 1 draws.
const RAW_WORDS: usize = 20_000;
/// How many mutated blocks phase 2 builds.
const BLOCKS: usize = 20_000;

/// The sweep's deterministic generator (a plain LCG; only reproducibility matters here).
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Lcg {
        Lcg(seed)
    }

    /// The next 64 pseudo-random bits (the high half, where an LCG's entropy is).
    fn bits(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }

    fn below(&mut self, n: u64) -> u64 {
        self.bits() % n
    }

    fn word(&mut self) -> Eterm {
        Eterm::from_raw(self.bits() as usize)
    }
}

/// Writes `w` at `slot` of the scratch buffer.
///
/// # Safety
///
/// `slot` must be inside the buffer `base` was derived from.
unsafe fn put(base: *mut Eterm, slot: usize, w: Eterm) {
    // SAFETY: the caller keeps `slot` inside the buffer.
    unsafe { *base.add(slot) = w };
}

/// A random word that is safe to store in a term slot: an arbitrary word when it is not a
/// boxed term, else a pointer into the buffer's `O` block (so the readers' one dereference
/// stays in-bounds).
fn safe_term_word(rng: &mut Lcg, base: *mut Eterm) -> Eterm {
    let w = rng.word();
    if is_boxed_term(w) {
        // A boxed-shaped word names an arbitrary address; substitute one we own.
        let slot = O + 8 * rng.below(4) as usize;
        // SAFETY: `slot` is inside the buffer.
        make_boxed(unsafe { base.add(slot) })
    } else {
        w
    }
}

/// Phase 1: every non-boxed word is refused as `NotATerm`.
fn raw_words_are_refused() {
    let mut rng = Lcg::new(0x0123_4567_89AB_CDEF);
    let mut refused = 0usize;
    for _ in 0..RAW_WORDS {
        let w = rng.word();
        if is_boxed_term(w) {
            // Names an arbitrary address: no dereference is legal, so phase 2 covers the
            // boxed shapes. (That it *is* boxed-shaped is itself the guard the readers
            // apply first.)
            continue;
        }
        refused += 1;
        assert_eq!(
            unsafe { try_bitstring_parts(w) },
            Err(TermError::NotATerm),
            "{w:?}"
        );
        assert_eq!(
            unsafe { try_sub_bits_parts(w) },
            Err(TermError::NotATerm),
            "{w:?}"
        );
        assert_eq!(unsafe { try_bit_at(w, 0) }, Err(TermError::NotATerm), "{w:?}");
        assert_eq!(
            unsafe { try_bit_at(w, usize::MAX) },
            Err(TermError::NotATerm),
            "{w:?}"
        );
    }
    // Sanity: the raw-word population is not degenerate (most words are immediates or
    // headers, but the boxed ones must be a real share of it).
    assert!(refused > RAW_WORDS / 2, "only {refused} of {RAW_WORDS} were tested");
}

#[test]
fn raw_words_are_refused_without_a_dereference() {
    raw_words_are_refused();
}

#[test]
fn mutated_blocks_are_typed_or_self_consistent_and_never_panic() {
    let mut buf: Vec<Eterm> = vec![Eterm::from_raw(0); SLOTS];
    let base = buf.as_mut_ptr();
    // SAFETY: `buf` is never reallocated, so `base` stays the buffer's address.
    let payload = unsafe { base.add(P) } as usize;
    let mut rng = Lcg::new(0xC0FF_EE00_1234_5678);
    let mut oks = 0usize;

    for _ in 0..BLOCKS {
        // -- the header ------------------------------------------------------
        let hdr = match rng.below(12) {
            0..=3 => HEADER_SUB_BITS,
            4..=6 => make_header(rng.below(6) as usize, HEAP_BITS_SUBTAG),
            7 => make_header(rng.below(8) as usize, SUB_BITS_SUBTAG),
            8 => make_header(rng.below(4) as usize, rng.below(16) as usize),
            9 => make_header(rng.below(8) as usize, ARITYVAL_SUBTAG),
            _ => rng.word(),
        };
        // SAFETY: T is inside the buffer.
        unsafe {
            put(base, T, hdr);
            // Randomise the whole payload first; the shape menus below then overwrite the
            // words that matter for the shape they select ("valid header, mutated payload").
            for i in 1..8 {
                put(base, T + i, rng.word());
            }
        }

        // -- the shape's own words -------------------------------------------
        let mut probe = false;
        if hdr == HEADER_SUB_BITS {
            let (base_flags, in_buffer) = match rng.below(6) {
                0 => (0, false),          // null masked base: refused
                1 => (POISON, false),     // poison, non-null: shape-valid, provenance not
                2 => (payload, true),     // in-buffer, probe-safe
                3 => (payload | ERL_SUB_BITS_FLAG_VOLATILE, true),
                4 => (rng.word().raw(), false),
                _ => (payload + rng.below(64) as usize, true),
            };
            let (start, end) = match rng.below(4) {
                0 => (0, 64),
                1 => {
                    let a = rng.below(2048) as usize;
                    (a, a + rng.below(2048) as usize)
                }
                2 => (rng.below(2048) as usize, rng.below(2048) as usize), // may reverse
                _ => (rng.bits() as usize, rng.bits() as usize),
            };
            let orig = match rng.below(4) {
                // A `BinRef`-shaped origin, its `val` word sometimes null.
                0 | 1 => {
                    let val = if rng.below(2) == 0 { 0 } else { rng.bits() as usize };
                    // SAFETY: O..O+3 is inside the buffer.
                    unsafe {
                        put(base, O, HEADER_BIN_REF);
                        put(base, O + 1, Eterm::from_raw(val));
                        put(base, O + 2, Eterm::from_raw(0));
                    }
                    // SAFETY: the `O` block is in-bounds.
                    make_boxed(unsafe { base.add(O) })
                }
                // A heap-bits-shaped origin, its arity agreeing or not.
                2 => {
                    let arity = rng.below(6) as usize;
                    let size = if rng.below(2) == 0 {
                        arity * 64 // `heap_bits_size_words(arity * 64) == arity + 2`
                    } else {
                        rng.below(4096) as usize
                    };
                    // SAFETY: O..O+2 is inside the buffer.
                    unsafe {
                        put(base, O, make_header(arity, HEAP_BITS_SUBTAG));
                        put(base, O + 1, Eterm::from_raw(size));
                    }
                    // SAFETY: the `O` block is in-bounds.
                    make_boxed(unsafe { base.add(O) })
                }
                // An arbitrary word (redirected into the buffer when it is boxed-shaped).
                _ => safe_term_word(&mut rng, base),
            };
            // SAFETY: words 1..4 of the `T` block are inside the buffer.
            unsafe {
                put(base, T + 1, Eterm::from_raw(base_flags));
                put(base, T + 2, Eterm::from_raw(start));
                put(base, T + 3, Eterm::from_raw(end));
                put(base, T + 4, orig);
            }
            probe = in_buffer;
        } else if header_subtag(hdr) as usize == HEAP_BITS_SUBTAG {
            // `heap_bits_size_words(arity * 64) == arity + 2`, i.e. arity `arity + 1` — a
            // matching size half the time, a random one otherwise.
            let size = if rng.below(2) == 0 {
                arityval(hdr) * 64
            } else {
                rng.below(4096) as usize
            };
            // SAFETY: word 1 of the `T` block is inside the buffer.
            unsafe { put(base, T + 1, Eterm::from_raw(size)) };
            probe = true;
        }

        // -- the assertions ---------------------------------------------------
        // SAFETY: the whole `T` block is inside the buffer, so the readers' single
        // dereference (the header word) and — if accepted — one more (the `orig` block's
        // header word at `O`) are both in-bounds.
        let term = unsafe { make_boxed(base.add(T)) };
        match unsafe { try_bitstring_parts(term) } {
            Ok((p, offset, size)) => {
                oks += 1;
                assert!(!p.is_null(), "an accepted block never reports a null base");
                assert!(
                    offset.checked_add(size).is_some(),
                    "offset {offset} + size {size} must not overflow"
                );
                // An out-of-range index is refused *before* any read.
                assert_eq!(unsafe { try_bit_at(term, size) }, Err(TermError::BadBitstring));
                assert_eq!(
                    unsafe { try_bit_at(term, usize::MAX) },
                    Err(TermError::BadBitstring)
                );
                // The unchecked reader agrees on an accepted block, and is panic-free.
                let (ubase, uoff, usize_) = unsafe { bitstring_parts(term) };
                assert_eq!((ubase as usize, uoff, usize_), (p as usize, offset, size));
                // Probe the bits only where the base demonstrably points into our payload,
                // and only up to the payload region's end.
                if probe && offset.saturating_add(size) <= (P_BYTES * 8) {
                    for i in 0..size {
                        let want = unsafe { bit_at(term, i) };
                        assert_eq!(
                            unsafe { try_bit_at(term, i) },
                            Ok(want),
                            "bit {i} of {size} at offset {offset}"
                        );
                    }
                }
                // A canonical `HEADER_SUB_BITS` block splits into the same four words.
                if hdr == HEADER_SUB_BITS {
                    assert_eq!(unsafe { try_sub_bits_parts(term) }, Ok(unsafe {
                        sub_bits_parts(term)
                    }));
                }
            }
            Err(e) => {
                assert!(
                    matches!(e, TermError::NotATerm | TermError::BadBitstring),
                    "unexpected refusal {e:?}"
                );
                // A refused block is refused by every checked reader.
                assert!(unsafe { try_sub_bits_parts(term) }.is_err(), "{e:?}");
                assert_eq!(unsafe { try_bit_at(term, 0) }, Err(e));
                // The unchecked reader stays panic-free on the same block.
                let _ = unsafe { bitstring_parts(term) };
            }
        }
    }

    // Sanity: the sweep reaches both verdicts (a generator that only ever refused would
    // prove nothing about the acceptance path).
    assert!(oks > BLOCKS / 20, "only {oks} of {BLOCKS} blocks were accepted");
    assert!(oks < BLOCKS, "the sweep must also exercise refusals");
}
