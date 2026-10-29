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

//! The **shape-validating** half of the bitstring readers.
//!
//! [`crate::bitstring`]'s [`sub_bits_parts`], [`bitstring_parts`] and [`bit_at`]
//! (`../bitstring/mod.rs`) are the *unchecked* readers: an `ErlSubBits`'s `base_flags`
//! word becomes a `*const u8` with no test at all, so a corrupt or stale payload word
//! turns into an arbitrary-address read. The `try_*` siblings defined here — the ones
//! re-exported as `beam_term::bitstring::try_*` — run the same reads *after* a shape
//! check and answer a typed error instead, and they are what a copier, a GC pass or a
//! message encoder should call on a term whose provenance it does not own.
//!
//! # The invariant the check enforces
//!
//! * a word that is not a term ([`crate::tag::is_term_word`] false — `THE_NON_VALUE`, a
//!   bare `HEADER` word) and a non-boxed term (an immediate or a cons cell) is
//!   [`TermError::NotATerm`];
//! * an `ErlSubBits` (`HEADER_SUB_BITS`) must have `start <= end`, a **non-null** masked
//!   base pointer, and an `orig` that is either a boxed `BinRef` with a non-null `val` or
//!   a boxed **on-heap** bitstring whose header arity agrees with its own bit-size word
//!   (the C's `ASSERT(erl_sub_bits_is_match_context(_sb) || _br)`,
//!   `erts/emulator/beam/erl_bits.h:385`, as a typed refusal);
//! * an on-heap bitstring (`HEAP_BITS_SUBTAG`) must satisfy
//!   `arityval(header) + 1 == heap_bits_size_words(size_word)` — its own arity must agree
//!   with its own bit count, which is what stops a word misclassified as an `ErlHeapBits`
//!   from becoming a base pointer into arbitrary storage;
//! * every other boxed header (tuple, map, float, bignum, fun, record, `BinRef`, …) is
//!   [`TermError::BadBitstring`], and [`try_bit_at`] additionally refuses `i >= size_bits`.
//!
//! # The honest limit is provenance
//!
//! The check covers *shape* only: it can prove that a readable block never yields a wild
//! pointer or a bogus bit, but a block that has been freed, recycled or moved is
//! word-for-word indistinguishable from a live one — this crate has no region registry and
//! none of its accessors validates provenance (`beam_term/CONTEXT.md`, *Notes for Agents*).
//! That is why the `try_*` forms stay `unsafe`: their `# Safety` is narrowed to "the boxed
//! pointer must be readable".

use crate::boxed::{
    arityval, boxed_val, header_subtag, is_boxed_term, ERL_HEAP_BITS_SIZE, HEADER_BIN_REF,
    HEADER_SUB_BITS,
};
use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::tag::{is_term_word, HEAP_BITS_SUBTAG};

use super::{
    heap_bits_size_words, raw_bit, ERL_SUB_BITS_FLAG_MASK, SUB_BITS_BASE_WORD, SUB_BITS_END_WORD,
    SUB_BITS_ORIG_WORD, SUB_BITS_START_WORD,
};

/// The validated shape of a bitstring: where its bits live and how many there are.
///
/// Produced only by [`checked_bits`], whose arms are the only constructors, so a
/// `CheckedBits` value *is* the proof that the shape rules of the module docs hold.
enum CheckedBits {
    /// `[HEADER_SUB_BITS, base_flags, start, end, orig]` with `start <= end`, a non-null
    /// masked base pointer and a recognised `orig`.
    Sub {
        /// `base_flags & !ERL_SUB_BITS_FLAG_MASK` — never null.
        base: *const u8,
        /// The first bit of the window, inside the base.
        start: usize,
        /// One past the last bit of the window; `start <= end`.
        end: usize,
    },
    /// `[thing_word, n_bits, data…]` whose header arity agrees with `n_bits`.
    Heap {
        /// The first payload byte (word `ERL_HEAP_BITS_SIZE` of the block).
        base: *const u8,
        /// The bit count — the block's own `size` word.
        size: usize,
    },
}

/// Whether `orig` is one of the two origins an `ErlSubBits` may name.
///
/// That is the C's assertion `ASSERT(erl_sub_bits_is_match_context(_sb) || _br)`
/// (`erts/emulator/beam/erl_bits.h:385`) turned into a predicate: `orig` is either a
/// boxed on-heap bitstring (a match context, `erts_bs_start_match_3`, `erl_bits.c:95-102`)
/// or a boxed `BinRef` whose `val` word is the off-heap payload's data pointer. Anything
/// else means the block is not a bitstring this runtime produced.
///
/// # Safety
///
/// If `orig` is a boxed term, the header word it points at must be readable. Shape beyond
/// the header word is not validated (the provenance limit, module docs).
unsafe fn is_recognised_origin(orig: Eterm) -> bool {
    if !is_boxed_term(orig) {
        return false;
    }
    let p = boxed_val(orig);
    // SAFETY: `orig` is a boxed term, so its header word is initialised.
    let hdr = unsafe { *p };
    if hdr == HEADER_BIN_REF {
        // SAFETY: the `BinRef` header fixes the 3-word block, so word 1 (`val`) is live.
        return unsafe { *p.add(1) }.raw() != 0;
    }
    if header_subtag(hdr) as usize == HEAP_BITS_SUBTAG {
        // SAFETY: a heap-bits header fixes `[thing_word, n_bits, data…]`, so word 1 is
        // the bit-size word.
        let size = unsafe { *p.add(1) }.raw();
        return arityval(hdr) + 1 == heap_bits_size_words(size);
    }
    false
}

/// Validates a bitstring term's **shape** and reports where its bits live.
///
/// The single place the shape rules live; every `try_*` reader below routes here, so the
/// invariant can never drift between them.
///
/// # Safety
///
/// If `t` is a boxed term, the header word it points at must be readable. (That the
/// header word *is* readable is the caller's invariant — the provenance limit, module
/// docs — but every word read after it is checked against it.)
unsafe fn checked_bits(t: Eterm) -> TermResult<CheckedBits> {
    if !is_term_word(t) || !is_boxed_term(t) {
        // A non-term word (`THE_NON_VALUE`, a bare `HEADER`) or a non-boxed term (an
        // immediate, a cons cell) has no header word to read at all.
        return Err(TermError::NotATerm);
    }
    let p = boxed_val(t);
    // SAFETY: `t` is a boxed term, so its header word is initialised.
    let hdr = unsafe { *p };
    if hdr == HEADER_SUB_BITS {
        // SAFETY: the canonical `HEADER_SUB_BITS` word fixes the 5-word `ErlSubBits`
        // block (`erl_bits.h:60-82`), so words 1..4 are live.
        let base_flags = unsafe { *p.add(SUB_BITS_BASE_WORD) }.raw();
        let start = unsafe { *p.add(SUB_BITS_START_WORD) }.raw();
        let end = unsafe { *p.add(SUB_BITS_END_WORD) }.raw();
        let orig = unsafe { *p.add(SUB_BITS_ORIG_WORD) };
        let base = base_flags & !ERL_SUB_BITS_FLAG_MASK;
        // `base == 0` means "no storage" and `start > end` would make the window size
        // underflow; both are refused here, *before* the base is ever used as a pointer.
        // (`start == end` is a legal zero-bit window.)
        if base == 0 || start > end {
            return Err(TermError::BadBitstring);
        }
        // SAFETY: `orig` is the block's word 4, a term slot the caller says is live.
        if !unsafe { is_recognised_origin(orig) } {
            return Err(TermError::BadBitstring);
        }
        Ok(CheckedBits::Sub {
            base: base as *const u8,
            start,
            end,
        })
    } else if header_subtag(hdr) as usize == HEAP_BITS_SUBTAG {
        // SAFETY: a heap-bits header fixes `[thing_word, n_bits, data…]`, so word 1 is
        // the bit-size word.
        let size = unsafe { *p.add(1) }.raw();
        // The block's own arity must agree with its own bit count — that is what stops a
        // word misclassified as an `ErlHeapBits` from becoming a base pointer into
        // arbitrary storage. `heap_bits_size_words` is overflow-free for every input, so
        // a corrupt `size` word cannot wrap its way to a match.
        if arityval(hdr) + 1 != heap_bits_size_words(size) {
            return Err(TermError::BadBitstring);
        }
        // SAFETY: the arity check proves the block is `2 + ceil(n_bytes / 8)` words, so
        // word `ERL_HEAP_BITS_SIZE` is its first payload byte.
        Ok(CheckedBits::Heap {
            base: unsafe { p.add(ERL_HEAP_BITS_SIZE) }.cast::<u8>(),
            size,
        })
    } else {
        // Every other boxed header — tuple, map, float, bignum, fun, record, `BinRef`,
        // an `ErlSubBits` whose header arity is not the canonical one, … — is not a
        // bitstring this crate wrote.
        Err(TermError::BadBitstring)
    }
}

/// Splits an `ErlSubBits` into `(base_flags, start, end, orig)` after **validating** the
/// block's shape.
///
/// The checked counterpart of `crate::bitstring::sub_bits_parts`: same answer, same word
/// order, but a mis-shaped block is a typed error instead of a tuple built from garbage.
/// It requires the canonical `HEADER_SUB_BITS` word — a **heap** bitstring has no such
/// words, so it is [`TermError::BadBitstring`] here (use [`try_bitstring_parts`] for
/// either form).
///
/// `base_flags` still carries the flag bits; mask with [`ERL_SUB_BITS_FLAG_MASK`] before
/// using it as a pointer (`start <= end` is guaranteed, but the base's *contents* are the
/// caller's responsibility — see the module docs' provenance limit).
///
/// # Errors
///
/// [`TermError::NotATerm`] if `t` is not a boxed term at all;
/// [`TermError::BadBitstring`] if it is boxed but not a well-shaped `ErlSubBits` (see the
/// module docs for the exact rules) — including a **heap** bitstring, which has no such
/// words (use [`try_bitstring_parts`] for either form).
///
/// # Safety
///
/// `t` must — if it is a boxed term — point at a readable header word. This reader
/// validates **shape**, never **provenance**: a freed or recycled block whose words still
/// look well formed is indistinguishable from a live one (module docs).
pub unsafe fn try_sub_bits_parts(t: Eterm) -> TermResult<(Eterm, Eterm, Eterm, Eterm)> {
    match unsafe { checked_bits(t) }? {
        CheckedBits::Sub { .. } => {
            // SAFETY: `checked_bits` proved word 0 is the canonical `HEADER_SUB_BITS`, so
            // words 1..4 are this block's `base_flags`/`start`/`end`/`orig`.
            unsafe {
                let p = boxed_val(t);
                Ok((
                    *p.add(SUB_BITS_BASE_WORD),
                    *p.add(SUB_BITS_START_WORD),
                    *p.add(SUB_BITS_END_WORD),
                    *p.add(SUB_BITS_ORIG_WORD),
                ))
            }
        }
        CheckedBits::Heap { .. } => Err(TermError::BadBitstring),
    }
}

/// Decomposes a bitstring into `(data base byte pointer, start bit offset, bit size)`
/// after **validating** the block's shape.
///
/// The checked counterpart of `crate::bitstring::bitstring_parts`: both representations
/// are handled and the answer has the same meaning (bit `i` is bit `(offset + i) % 8`,
/// MSB-first, of `base[(offset + i) / 8]`), but a mis-shaped block is a typed error
/// rather than a pointer taken from a garbage word. `offset + size` never overflows for a
/// validated sub-bits (it is bounded by the block's own `end`).
///
/// # Errors
///
/// As [`try_sub_bits_parts`], plus: a boxed header that is neither the canonical
/// `HEADER_SUB_BITS` nor a `HEAP_BITS_SUBTAG` header whose arity agrees with its bit-size
/// word is [`TermError::BadBitstring`].
///
/// # Safety
///
/// `t` must — if it is a boxed term — point at a readable header word. **Shape is
/// validated, not provenance**: the returned base still borrows storage that a freed or
/// moved block no longer owns, so the caller must keep the owning heap (or the underlying
/// payload) alive and unmoved while it uses the pointer.
pub unsafe fn try_bitstring_parts(t: Eterm) -> TermResult<(*const u8, usize, usize)> {
    match unsafe { checked_bits(t) }? {
        // SAFETY: no further read — `end >= start` was validated, so the subtraction
        // cannot underflow (saturating keeps even a future drift panic-free).
        CheckedBits::Sub { base, start, end } => Ok((base, start, end.saturating_sub(start))),
        CheckedBits::Heap { base, size } => Ok((base, 0, size)),
    }
}

/// Reads bit `i` (0-based, most significant bit of the first byte first) of a bitstring
/// after **validating** the block's shape and the index.
///
/// The checked counterpart of `crate::bitstring::bit_at`: identical bit order
/// (`(base[(offset + i) / 8] >> (7 - ((offset + i) % 8))) & 1`), but the term must be a
/// well-shaped bitstring and `i` must be inside its bit range, so the byte the read
/// touches is proven to belong to the block's own window.
///
/// # Errors
///
/// [`TermError::NotATerm`] / [`TermError::BadBitstring`] from [`try_bitstring_parts`],
/// plus [`TermError::BadBitstring`] when `i >= size_bits` (including `i == size_bits`, one
/// past the last bit) — refused *before* the byte is touched.
///
/// # Safety
///
/// `t` must — if it is a boxed term — point at a readable header word, and the storage
/// the block names must still be live and unmoved (**shape** is validated, not
/// provenance; module docs).
pub unsafe fn try_bit_at(t: Eterm, i: usize) -> TermResult<bool> {
    let (base, offset, size) = unsafe { try_bitstring_parts(t) }?;
    if i >= size {
        return Err(TermError::BadBitstring);
    }
    // `i < size` and `offset + size <= end` (a validated sub-bits) or `offset == 0` (heap
    // bits), so this cannot overflow; saturating keeps it panic-free anyway.
    let bit = offset.saturating_add(i);
    // SAFETY: bit `offset + i` is inside the validated `start..end` window (or inside the
    // heap-bits block for `offset == 0, i < size`), so the byte is in range.
    unsafe { Ok(raw_bit(base, bit) != 0) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitstring::{
        bit_at, bitstring_parts, write_heap_bits, write_sub_bits, ERL_SUB_BITS_FLAG_MUTABLE,
        ERL_SUB_BITS_FLAG_VOLATILE,
    };
    use crate::boxed::{make_arityval, make_boxed, make_header};
    use crate::heap::Heap;
    use crate::immediate::{make_small, NIL};
    use crate::ops::THE_NON_VALUE;
    use crate::thing::write_float;

    fn new_heap() -> Heap {
        Heap::new(256)
    }

    /// The poison base the corruption tests use: it must never become a read.
    const POISON: usize = 0xDEAD_BEEF_DEAD_BEEF;

    /// Writes an `ErlSubBits` with the caller's four words and returns its root.
    fn sub_bits(heap: &mut Heap, base_flags: usize, start: usize, end: usize, orig: Eterm) -> Eterm {
        // SAFETY: `sub_bits_size_words()` (5) words are reserved and all are written.
        unsafe {
            let hp = heap
                .alloc_words(crate::refc::sub_bits_size_words())
                .unwrap();
            write_sub_bits(
                hp,
                Eterm::from_raw(base_flags),
                Eterm::from_raw(start),
                Eterm::from_raw(end),
                orig,
            )
        }
    }

    /// A real on-heap bitstring `<<0b1010_1010>>` — the `orig` a *match context* carries.
    fn match_origin(heap: &mut Heap) -> Eterm {
        // SAFETY: the block is sized by `heap_bits_size_words(8)`.
        unsafe {
            let hp = heap.alloc_words(heap_bits_size_words(8)).unwrap();
            write_heap_bits(hp, &[0b1010_1010], 8).unwrap()
        }
    }

    /// A 2-tuple `{1, 2}` — a boxed term that is neither a `BinRef` nor a bitstring.
    fn tuple_term_in_heap(heap: &mut Heap) -> Eterm {
        // SAFETY: 3 words for `[arityval(2), 1, 2]`.
        unsafe {
            let hp = heap.alloc_words(3).unwrap();
            *hp = make_arityval(2);
            *hp.add(1) = make_small(1);
            *hp.add(2) = make_small(2);
            crate::tuple::make_tuple(hp)
        }
    }

    #[test]
    fn checked_readers_refuse_a_poison_base_before_using_it() {
        let mut heap = new_heap();
        // A poisoned `base_flags` with a non-bitstring `orig`: the *origin* test refuses
        // the block, so `0xDEAD_BEEF_DEAD_BEEF` is never turned into a read.
        let t = sub_bits(&mut heap, POISON, 0, 8, make_small(7));
        assert_eq!(
            unsafe { try_sub_bits_parts(t) },
            Err(TermError::BadBitstring)
        );
        assert_eq!(
            unsafe { try_bitstring_parts(t) },
            Err(TermError::BadBitstring)
        );
        assert_eq!(unsafe { try_bit_at(t, 0) }, Err(TermError::BadBitstring));

        // With a *recognised* origin the same poison base passes: the check covers shape,
        // not provenance (module docs). Nothing below dereferences that pointer.
        let orig = match_origin(&mut heap);
        let t = sub_bits(&mut heap, POISON, 0, 8, orig);
        let (base, off, size) = unsafe { try_bitstring_parts(t) }.unwrap();
        assert_eq!(
            (base as usize, off, size),
            (POISON & !ERL_SUB_BITS_FLAG_MASK, 0, 8)
        );

        // A null masked base is refused on its own, whatever the origin says.
        let orig = match_origin(&mut heap);
        let t = sub_bits(&mut heap, ERL_SUB_BITS_FLAG_MUTABLE, 0, 8, orig);
        assert_eq!(
            unsafe { try_bitstring_parts(t) },
            Err(TermError::BadBitstring)
        );
    }

    #[test]
    fn checked_readers_refuse_end_before_start_and_the_unchecked_ones_do_not_panic() {
        #[repr(align(8))]
        struct Storage([u8; 4]);
        let storage = Storage([0b1010_1010, 0b1100_1111, 0, 0]);
        let mut heap = new_heap();
        let orig = match_origin(&mut heap);
        let t = sub_bits(&mut heap, storage.0.as_ptr() as usize, 16, 8, orig);
        assert_eq!(
            unsafe { try_sub_bits_parts(t) },
            Err(TermError::BadBitstring)
        );
        assert_eq!(
            unsafe { try_bitstring_parts(t) },
            Err(TermError::BadBitstring)
        );
        assert_eq!(unsafe { try_bit_at(t, 0) }, Err(TermError::BadBitstring));
        // The unchecked readers stay panic-free on the same block (a debug-profile panic on
        // runtime data is forbidden): the window saturates to empty.
        assert_eq!(unsafe { bitstring_parts(t) }.2, 0);
        assert!(!unsafe { bit_at(t, 0) });

        // `start == end` is a legal zero-bit window, not a corruption.
        let orig = match_origin(&mut heap);
        let t = sub_bits(&mut heap, storage.0.as_ptr() as usize, 8, 8, orig);
        assert_eq!(
            unsafe { try_bitstring_parts(t) }.map(|p| (p.1, p.2)),
            Ok((8, 0))
        );
        assert_eq!(unsafe { try_bit_at(t, 0) }, Err(TermError::BadBitstring));
    }

    #[test]
    fn checked_readers_refuse_a_sub_bits_whose_origin_is_not_recognised() {
        let mut heap = new_heap();
        // A plain tuple, a float, an immediate: none is a `BinRef` or a heap bitstring.
        let tuple = tuple_term_in_heap(&mut heap);
        // SAFETY: the float block is sized by `FLOAT_SIZE_WORDS`.
        let float = unsafe {
            let hp = heap.alloc_words(crate::thing::FLOAT_SIZE_WORDS).unwrap();
            write_float(hp, 1.5)
        };
        for orig in [tuple, float, make_small(0), NIL] {
            let t = sub_bits(&mut heap, POISON, 0, 8, orig);
            assert_eq!(
                unsafe { try_bitstring_parts(t) },
                Err(TermError::BadBitstring),
                "orig {orig:?}"
            );
        }
        // A `BinRef` whose `val` (the payload data pointer) is null is refused too.
        // SAFETY: 3 words for `[HEADER_BIN_REF, val, next]`.
        let null_bin_ref = unsafe {
            let hp = heap.alloc_words(crate::refc::bin_ref_size_words()).unwrap();
            crate::refc::write_bin_ref(hp, core::ptr::null_mut(), core::ptr::null_mut())
        };
        let t = sub_bits(&mut heap, POISON, 0, 8, null_bin_ref);
        assert_eq!(
            unsafe { try_bitstring_parts(t) },
            Err(TermError::BadBitstring)
        );
    }

    #[test]
    fn checked_readers_refuse_a_heap_bitstring_whose_arity_disagrees() {
        let mut heap = new_heap();
        // A well-formed 16-bit block: 3 words, header arity 2 — accepted.
        // SAFETY: `heap_bits_size_words(16)` (3) words are reserved and all written.
        let t = unsafe {
            let hp = heap.alloc_words(heap_bits_size_words(16)).unwrap();
            *hp = make_header(heap_bits_size_words(16) - 1, HEAP_BITS_SUBTAG);
            *hp.add(1) = Eterm::from_raw(16);
            *hp.add(2) = Eterm::from_raw(0xABCD);
            make_boxed(hp)
        };
        assert_eq!(
            unsafe { try_bitstring_parts(t) }.map(|p| (p.1, p.2)),
            Ok((0, 16))
        );
        // Header arity 5 for a 3-word block: refused, so a misclassified word can never
        // become a base pointer.
        // SAFETY: the block is the live 3-word one just written; only word 0 changes.
        unsafe { *boxed_val(t) = make_header(5, HEAP_BITS_SUBTAG) };
        assert_eq!(
            unsafe { try_bitstring_parts(t) },
            Err(TermError::BadBitstring)
        );
        // A size word the arity does not cover: refused as well.
        // SAFETY: as above, words 0..1 of the same live block.
        unsafe {
            *boxed_val(t) = make_header(2, HEAP_BITS_SUBTAG);
            *boxed_val(t).add(1) = Eterm::from_raw(1000);
        }
        assert_eq!(
            unsafe { try_bitstring_parts(t) },
            Err(TermError::BadBitstring)
        );
        assert_eq!(unsafe { try_bit_at(t, 0) }, Err(TermError::BadBitstring));
    }

    #[test]
    fn checked_readers_refuse_every_non_bitstring_word() {
        let mut heap = new_heap();
        let tuple = tuple_term_in_heap(&mut heap);
        // SAFETY: the float block is sized by `FLOAT_SIZE_WORDS`.
        let float = unsafe {
            let hp = heap.alloc_words(crate::thing::FLOAT_SIZE_WORDS).unwrap();
            write_float(hp, 1.5)
        };
        // An empty flat map is the 3-word `[arityval(2), MAP_HEADER_FLATMAP, 0]`-shaped
        // block (`flatmap_size_words(0)`).
        // SAFETY: `map_words_capacity(0)` (3) words are reserved for the writer.
        let map = unsafe {
            let hp = heap.alloc_words(crate::map::map_words_capacity(0)).unwrap();
            crate::map::write_map(hp, &[], &crate::atom::AtomTable::new()).unwrap()
        };
        // A `FUN_SUBTAG` header word: a fun, not a bitstring.
        // SAFETY: a zero-arity fun header is a one-word block.
        let fun = unsafe {
            let hp = heap.alloc_words(1).unwrap();
            *hp = make_header(0, crate::tag::FUN_SUBTAG);
            make_boxed(hp)
        };
        for t in [tuple, float, map, fun] {
            assert_eq!(
                unsafe { try_bitstring_parts(t) },
                Err(TermError::BadBitstring),
                "{t:?}"
            );
            assert_eq!(
                unsafe { try_sub_bits_parts(t) },
                Err(TermError::BadBitstring),
                "{t:?}"
            );
            assert_eq!(
                unsafe { try_bit_at(t, 0) },
                Err(TermError::BadBitstring),
                "{t:?}"
            );
        }

        // Words that are not terms at all, and terms that are not boxed ones.
        // SAFETY: 2 words for the `[CAR, CDR]` cons cell, both written before escape.
        let cons = unsafe {
            let hp = heap.alloc_words(2).unwrap();
            *hp.add(crate::list::CAR_OFFSET) = make_small(1);
            *hp.add(crate::list::CDR_OFFSET) = NIL;
            crate::list::make_list(hp)
        };
        for t in [NIL, THE_NON_VALUE, HEADER_SUB_BITS, make_small(1), cons] {
            assert_eq!(
                unsafe { try_bitstring_parts(t) },
                Err(TermError::NotATerm),
                "{t:?}"
            );
            assert_eq!(
                unsafe { try_sub_bits_parts(t) },
                Err(TermError::NotATerm),
                "{t:?}"
            );
            assert_eq!(
                unsafe { try_bit_at(t, 0) },
                Err(TermError::NotATerm),
                "{t:?}"
            );
        }
    }

    #[test]
    fn checked_readers_accept_a_real_heap_bitstring() {
        let mut heap = new_heap();
        let t = match_origin(&mut heap); // `<<0b1010_1010>>`
        assert_eq!(
            unsafe { try_bitstring_parts(t) }.map(|p| (p.1, p.2)),
            Ok((0, 8))
        );
        assert_eq!(unsafe { try_bit_at(t, 0) }, Ok(true));
        assert_eq!(unsafe { try_bit_at(t, 1) }, Ok(false));
        assert_eq!(unsafe { try_bit_at(t, 2) }, Ok(true));
        assert_eq!(unsafe { try_bit_at(t, 7) }, Ok(false));
        // `i == size` is one past the last bit: refused, never a read.
        assert_eq!(unsafe { try_bit_at(t, 8) }, Err(TermError::BadBitstring));
        assert_eq!(
            unsafe { try_bit_at(t, usize::MAX) },
            Err(TermError::BadBitstring)
        );
        // A heap bitstring has no `ErlSubBits` words to split.
        assert_eq!(
            unsafe { try_sub_bits_parts(t) },
            Err(TermError::BadBitstring)
        );
    }

    #[test]
    fn checked_readers_accept_a_refc_window_and_a_match_context() {
        #[repr(align(8))]
        struct Payload([u8; 8]);
        let payload = Payload([0b1010_1010, 0b1100_1111, 0, 0, 0, 0, 0, 0]);
        let mut heap = new_heap();
        // SAFETY: `ERL_REFC_BITS_SIZE` (8) words are reserved and fully written; the
        // payload outlives the term and is word-aligned.
        let t = unsafe {
            let hp = heap.alloc_words(crate::refc::ERL_REFC_BITS_SIZE).unwrap();
            crate::refc::write_refc_binary(
                hp,
                payload.0.as_ptr() as *mut Eterm,
                core::ptr::null_mut(),
                payload.0.as_ptr(),
                3,
                9,
            )
        };
        let (base_flags, start, end, _orig) = unsafe { try_sub_bits_parts(t) }.unwrap();
        assert_eq!(base_flags.raw(), payload.0.as_ptr() as usize);
        assert_eq!((start.raw(), end.raw()), (3, 12));
        assert!(
            unsafe { crate::refc::is_refc_binary(t) },
            "the origin is the pair's BinRef"
        );
        let (base, off, size) = unsafe { try_bitstring_parts(t) }.unwrap();
        assert_eq!(
            (base as usize, off, size),
            (payload.0.as_ptr() as usize, 3, 9)
        );
        // Non-byte-aligned window: bits 3..12 of `1010_1010 1100_1111`.
        let bits: Vec<bool> = (0..9)
            .map(|i| unsafe { try_bit_at(t, i) }.unwrap())
            .collect();
        assert_eq!(
            bits,
            [false, true, false, true, false, true, true, false, false]
        );
        assert_eq!(unsafe { try_bit_at(t, 9) }, Err(TermError::BadBitstring));

        // A match context: `orig` is an on-heap bitstring, the window starts at bit 0.
        let hb = match_origin(&mut heap);
        let t = sub_bits(&mut heap, payload.0.as_ptr() as usize, 0, 8, hb);
        assert_eq!(
            unsafe { try_bitstring_parts(t) }.map(|p| (p.1, p.2)),
            Ok((0, 8))
        );
        assert_eq!(unsafe { try_bit_at(t, 0) }, Ok(true));
        assert_eq!(unsafe { try_bit_at(t, 1) }, Ok(false));

        // Flag bits stay masked off the reported base.
        let hb = match_origin(&mut heap);
        let flags = payload.0.as_ptr() as usize | ERL_SUB_BITS_FLAG_VOLATILE;
        let t = sub_bits(&mut heap, flags, 0, 4, hb);
        assert_eq!(
            unsafe { try_bitstring_parts(t) }.map(|p| (p.0 as usize, p.1, p.2)),
            Ok((payload.0.as_ptr() as usize, 0, 4))
        );
    }
}
