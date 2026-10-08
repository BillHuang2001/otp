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

//! Raw tag constants, masks, shifts and the primary/boxed tag enums.
//!
//! Ground truth: `erts/emulator/beam/erl_term.h` (and `erl_map.h` for the map
//! header tags). Every value is copied verbatim — the C macro it mirrors is named
//! in a comment — so that the Rust core and the retained C periphery agree
//! bit-for-bit (Level-1 drop-in; workspace `CONTEXT.md` §2).

use crate::eterm::Eterm;

// ---------------------------------------------------------------------------
// Primary tag (erl_term.h: _TAG_PRIMARY_*)
// ---------------------------------------------------------------------------

/// Number of bits in the primary tag (`_TAG_PRIMARY_SIZE`).
pub const TAG_PRIMARY_SIZE: u32 = 2;
/// Mask selecting the primary tag (`_TAG_PRIMARY_MASK`).
pub const TAG_PRIMARY_MASK: usize = 0x3;
/// `TAG_PRIMARY_HEADER` — a header word (never a standalone term).
pub const TAG_PRIMARY_HEADER: usize = 0x0;
/// `TAG_PRIMARY_LIST` — a cons cell.
pub const TAG_PRIMARY_LIST: usize = 0x1;
/// `TAG_PRIMARY_BOXED` — a boxed pointer.
pub const TAG_PRIMARY_BOXED: usize = 0x2;
/// `TAG_PRIMARY_IMMED1` — an immediate (level 1).
pub const TAG_PRIMARY_IMMED1: usize = 0x3;

// ---------------------------------------------------------------------------
// Pointer tag (erl_term.h: TAG_PTR_MASK__ / TAG_LITERAL_PTR)
// ---------------------------------------------------------------------------

/// Mask of the pointer tag bits (64-bit: `TAG_PTR_MASK__` == `0x7`).
pub const TAG_PTR_MASK: usize = 0x7;
/// Literal-area marker ORed into BOXED/LIST words (`TAG_LITERAL_PTR`).
///
/// The C header defines this on 64-bit when there is no OS physical-memory
/// reservation, or under DEBUG. Level-1 fidelity: we always define it.
pub const TAG_LITERAL_PTR: usize = 0x4;

// ---------------------------------------------------------------------------
// Immediate level 1 (erl_term.h: _TAG_IMMED1_*)
// ---------------------------------------------------------------------------

/// `_TAG_IMMED1_SIZE`.
pub const TAG_IMMED1_SIZE: u32 = 4;
/// `_TAG_IMMED1_MASK`.
pub const TAG_IMMED1_MASK: usize = 0xF;
/// `_TAG_IMMED1_PID`.
pub const TAG_IMMED1_PID: usize = (0x0 << TAG_PRIMARY_SIZE) | TAG_PRIMARY_IMMED1;
/// `_TAG_IMMED1_PORT`.
pub const TAG_IMMED1_PORT: usize = (0x1 << TAG_PRIMARY_SIZE) | TAG_PRIMARY_IMMED1;
/// `_TAG_IMMED1_IMMED2`.
pub const TAG_IMMED1_IMMED2: usize = (0x2 << TAG_PRIMARY_SIZE) | TAG_PRIMARY_IMMED1;
/// `_TAG_IMMED1_SMALL`.
pub const TAG_IMMED1_SMALL: usize = (0x3 << TAG_PRIMARY_SIZE) | TAG_PRIMARY_IMMED1;

// ---------------------------------------------------------------------------
// Immediate level 2 (erl_term.h: _TAG_IMMED2_*)
// ---------------------------------------------------------------------------

/// `_TAG_IMMED2_SIZE`.
pub const TAG_IMMED2_SIZE: u32 = 6;
/// `_TAG_IMMED2_MASK`.
pub const TAG_IMMED2_MASK: usize = 0x3F;
/// `_TAG_IMMED2_ATOM`.
pub const TAG_IMMED2_ATOM: usize = (0x0 << TAG_IMMED1_SIZE) | TAG_IMMED1_IMMED2;
/// `_TAG_IMMED2_CATCH`.
pub const TAG_IMMED2_CATCH: usize = (0x1 << TAG_IMMED1_SIZE) | TAG_IMMED1_IMMED2;
/// `_TAG_IMMED2_NIL`.
pub const TAG_IMMED2_NIL: usize = (0x3 << TAG_IMMED1_SIZE) | TAG_IMMED1_IMMED2;

// ---------------------------------------------------------------------------
// Headers (erl_term.h: _TAG_HEADER_MASK / _HEADER_ARITY_OFFS / MAX_ARITYVAL)
// ---------------------------------------------------------------------------

/// `_TAG_HEADER_MASK` — 6 bits: 4 subtag bits above the 2 primary bits.
pub const TAG_HEADER_MASK: usize = 0x3F;
/// `_HEADER_ARITY_OFFS` — the arity field starts here.
pub const HEADER_ARITY_OFFS: u32 = 6;
/// `_HEADER_SUBTAG_MASK` — 4 bits of subtag.
pub const HEADER_SUBTAG_MASK: usize = 0x3C;
/// `MAX_ARITYVAL` — 24-bit arity ceiling.
pub const MAX_ARITYVAL: usize = (1 << 24) - 1;
/// `_TRANSPARENT_TAG_MASK` — used by `header_is_transparent`.
pub const TRANSPARENT_TAG_MASK: usize = (!(0x1usize << TAG_PRIMARY_SIZE)) & TAG_HEADER_MASK;
/// `_BIG_TAG_MASK` — used to test both bignum headers at once.
pub const BIG_TAG_MASK: usize = (!(0x1usize << TAG_PRIMARY_SIZE)) & TAG_HEADER_MASK;
/// `_BIG_SIGN_BIT`.
pub const BIG_SIGN_BIT: usize = 0x1 << TAG_PRIMARY_SIZE;
/// `_BITSTRING_TAG_MASK`.
pub const BITSTRING_TAG_MASK: usize = (!(0x1usize << TAG_PRIMARY_SIZE)) & TAG_HEADER_MASK;
/// `_EXTERNAL_TAG_MASK` — matches external pid/port/ref headers.
pub const EXTERNAL_TAG_MASK: usize = (!(0x3usize << TAG_PRIMARY_SIZE)) & TAG_HEADER_MASK;

// ---------------------------------------------------------------------------
// Header subtags (erl_term.h: *_SUBTAG, which already include the primary bits)
// ---------------------------------------------------------------------------

/// `ARITYVAL_SUBTAG` — tuple header.
pub const ARITYVAL_SUBTAG: usize = 0x00;
/// `POS_BIG_SUBTAG`.
pub const POS_BIG_SUBTAG: usize = 0x08;
/// `NEG_BIG_SUBTAG`.
pub const NEG_BIG_SUBTAG: usize = 0x0C;
/// `REF_SUBTAG`.
pub const REF_SUBTAG: usize = 0x10;
/// `FUN_SUBTAG`.
pub const FUN_SUBTAG: usize = 0x14;
/// `FLOAT_SUBTAG`.
pub const FLOAT_SUBTAG: usize = 0x18;
/// `RECORD_SUBTAG`.
pub const RECORD_SUBTAG: usize = 0x1C;
/// `HEAP_BITS_SUBTAG`.
pub const HEAP_BITS_SUBTAG: usize = 0x20;
/// `SUB_BITS_SUBTAG`.
pub const SUB_BITS_SUBTAG: usize = 0x24;
/// `BIN_REF_SUBTAG`.
pub const BIN_REF_SUBTAG: usize = 0x28;
/// `MAP_SUBTAG`.
pub const MAP_SUBTAG: usize = 0x2C;
/// `EXTERNAL_PID_SUBTAG`.
pub const EXTERNAL_PID_SUBTAG: usize = 0x30;
/// `EXTERNAL_PORT_SUBTAG`.
pub const EXTERNAL_PORT_SUBTAG: usize = 0x34;
/// `EXTERNAL_REF_SUBTAG`.
pub const EXTERNAL_REF_SUBTAG: usize = 0x38;

// ---------------------------------------------------------------------------
// Map header layout (erl_term.h: MAP_HEADER_* / erl_map.h: HAMT_SUBTAG_*)
// ---------------------------------------------------------------------------

/// `MAP_HEADER_TAG_SZ`.
pub const MAP_HEADER_TAG_SZ: u32 = 2;
/// `MAP_HEADER_ARITY_SZ`.
pub const MAP_HEADER_ARITY_SZ: u32 = 8;
/// `MAP_HEADER_VAL_SZ`.
pub const MAP_HEADER_VAL_SZ: u32 = 16;
/// `MAP_HEADER_TAG_FLATMAP_HEAD`.
pub const MAP_HEADER_TAG_FLATMAP_HEAD: usize = 0x0;
/// `MAP_HEADER_TAG_HAMT_NODE_BITMAP`.
pub const MAP_HEADER_TAG_HAMT_NODE_BITMAP: usize = 0x1;
/// `MAP_HEADER_TAG_HAMT_HEAD_ARRAY`.
pub const MAP_HEADER_TAG_HAMT_HEAD_ARRAY: usize = 0x2;
/// `MAP_HEADER_TAG_HAMT_HEAD_BITMAP`.
pub const MAP_HEADER_TAG_HAMT_HEAD_BITMAP: usize = 0x3;
/// `_HEADER_MAP_SUBTAG_MASK`.
pub const HEADER_MAP_SUBTAG_MASK: usize = (3 << HEADER_ARITY_OFFS) | HEADER_SUBTAG_MASK;
/// `_HEADER_MAP_HASHMAP_HEAD_MASK`.
pub const HEADER_MAP_HASHMAP_HEAD_MASK: usize =
    (!(1usize << HEADER_ARITY_OFFS)) & HEADER_MAP_SUBTAG_MASK;
/// `HAMT_SUBTAG_NODE_BITMAP`.
pub const HAMT_SUBTAG_NODE_BITMAP: usize =
    (MAP_HEADER_TAG_HAMT_NODE_BITMAP << HEADER_ARITY_OFFS) | MAP_SUBTAG;
/// `HAMT_SUBTAG_HEAD_ARRAY`.
pub const HAMT_SUBTAG_HEAD_ARRAY: usize =
    (MAP_HEADER_TAG_HAMT_HEAD_ARRAY << HEADER_ARITY_OFFS) | MAP_SUBTAG;
/// `HAMT_SUBTAG_HEAD_BITMAP`.
pub const HAMT_SUBTAG_HEAD_BITMAP: usize =
    (MAP_HEADER_TAG_HAMT_HEAD_BITMAP << HEADER_ARITY_OFFS) | MAP_SUBTAG;
/// `HAMT_SUBTAG_HEAD_FLATMAP`.
pub const HAMT_SUBTAG_HEAD_FLATMAP: usize =
    (MAP_HEADER_TAG_FLATMAP_HEAD << HEADER_ARITY_OFFS) | MAP_SUBTAG;

// ---------------------------------------------------------------------------
// Tag enums and classifiers
// ---------------------------------------------------------------------------

/// The 2-bit primary tag (erl_term.h: `TAG_PRIMARY_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrimaryTag {
    /// `TAG_PRIMARY_HEADER` — a header word, not a value.
    Header,
    /// `TAG_PRIMARY_LIST` — a cons cell.
    List,
    /// `TAG_PRIMARY_BOXED` — a boxed pointer.
    Boxed,
    /// `TAG_PRIMARY_IMMED1` — an immediate.
    Immed1,
}

/// Boxed/thing header subtag (erl_term.h: the `*_SUBTAG` group).
///
/// The discriminants are the raw header subtag values (`Hdr & 0x3F`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum BoxedTag {
    /// Tuple (`ARITYVAL_SUBTAG`).
    ArityVal = 0x00,
    /// Positive bignum (`POS_BIG_SUBTAG`).
    PosBig = 0x08,
    /// Negative bignum (`NEG_BIG_SUBTAG`).
    NegBig = 0x0C,
    /// Reference (`REF_SUBTAG`).
    Ref = 0x10,
    /// Fun/lambda (`FUN_SUBTAG`).
    Fun = 0x14,
    /// Float (`FLOAT_SUBTAG`).
    Float = 0x18,
    /// Native record (`RECORD_SUBTAG`).
    Record = 0x1C,
    /// Heap bitstring (`HEAP_BITS_SUBTAG`).
    HeapBits = 0x20,
    /// Sub-bitstring (`SUB_BITS_SUBTAG`).
    SubBits = 0x24,
    /// Off-heap binary reference (`BIN_REF_SUBTAG`).
    BinRef = 0x28,
    /// Map (`MAP_SUBTAG`).
    Map = 0x2C,
    /// External pid (`EXTERNAL_PID_SUBTAG`).
    ExternalPid = 0x30,
    /// External port (`EXTERNAL_PORT_SUBTAG`).
    ExternalPort = 0x34,
    /// External reference (`EXTERNAL_REF_SUBTAG`).
    ExternalRef = 0x38,
}

impl BoxedTag {
    /// Decode a header subtag value (as returned by `header_subtag`).
    ///
    /// Returns `None` for the reserved subtags (e.g. `0x04`, `0x3C`), matching
    /// the fact that C's `tag_val_def` aborts on them — we expose the failure
    /// instead of guessing (workspace `CONTEXT.md` §3).
    pub const fn from_raw(subtag: u32) -> Option<BoxedTag> {
        Some(match subtag {
            0x00 => BoxedTag::ArityVal,
            0x08 => BoxedTag::PosBig,
            0x0C => BoxedTag::NegBig,
            0x10 => BoxedTag::Ref,
            0x14 => BoxedTag::Fun,
            0x18 => BoxedTag::Float,
            0x1C => BoxedTag::Record,
            0x20 => BoxedTag::HeapBits,
            0x24 => BoxedTag::SubBits,
            0x28 => BoxedTag::BinRef,
            0x2C => BoxedTag::Map,
            0x30 => BoxedTag::ExternalPid,
            0x34 => BoxedTag::ExternalPort,
            0x38 => BoxedTag::ExternalRef,
            _ => return None,
        })
    }
}

/// `primary_tag(x)` — the 2-bit primary tag of a term word.
pub const fn primary_tag(x: Eterm) -> PrimaryTag {
    match x.raw() & TAG_PRIMARY_MASK {
        TAG_PRIMARY_HEADER => PrimaryTag::Header,
        TAG_PRIMARY_LIST => PrimaryTag::List,
        TAG_PRIMARY_BOXED => PrimaryTag::Boxed,
        _ => PrimaryTag::Immed1,
    }
}

/// `is_header(x)` — true for a header word.
pub const fn is_header(x: Eterm) -> bool {
    (x.raw() & TAG_PRIMARY_MASK) == TAG_PRIMARY_HEADER
}

/// Whether `x` is a **term word** at all — i.e. not a bare header word.
///
/// A word whose primary tag is `HEADER` is not a term: the C ABI's
/// `THE_NON_VALUE` (`0` in release, `make_header(0, FLOAT_SUBTAG)` in a debug
/// build — the word a NIF/`enif_*` callback hands back to mean "no term") and
/// every other bare header word. **No valid term** has the `HEADER` primary tag:
/// a cons cell is `LIST`, a boxed pointer is `BOXED`, every immediate is
/// `IMMED1`. So this is the crate's single "is this a term?" test, and it is the
/// guard every path that is about to read `*boxed_val(x)` must apply.
///
/// It is needed *because* [`crate::boxed::is_boxed`] mirrors the C bit trick
/// `(x & (_TAG_PRIMARY_MASK - TAG_PRIMARY_BOXED)) == 0`, which is **also true**
/// for the `HEADER` words: a guard written as `if is_boxed(x)` alone lets a
/// stray non-term through and dereferences a garbage (in release, `NULL`)
/// `boxed_val` pointer. Use [`crate::boxed::is_boxed_term`] where both the
/// "boxed" and the "is a term" conditions are wanted.
///
/// Note the near-identical name: [`crate::ops::is_value`] is a **different**
/// predicate (`x != THE_NON_VALUE`), which is true for every valid term *and*
/// for a bare header word other than the debug `THE_NON_VALUE`. This one is the
/// broader "this word is a term" test.
#[inline]
pub const fn is_term_word(x: Eterm) -> bool {
    !is_header(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_constants_match_erl_term_h() {
        assert_eq!(TAG_PRIMARY_MASK, 0x3);
        assert_eq!(TAG_PRIMARY_HEADER, 0x0);
        assert_eq!(TAG_PRIMARY_LIST, 0x1);
        assert_eq!(TAG_PRIMARY_BOXED, 0x2);
        assert_eq!(TAG_PRIMARY_IMMED1, 0x3);
        assert_eq!(TAG_PTR_MASK, 0x7);
        assert_eq!(TAG_LITERAL_PTR, 0x4);
    }

    #[test]
    fn immediate_constants_match_erl_term_h() {
        assert_eq!(TAG_IMMED1_MASK, 0xF);
        assert_eq!(TAG_IMMED1_PID, 0x3);
        assert_eq!(TAG_IMMED1_PORT, 0x7);
        assert_eq!(TAG_IMMED1_IMMED2, 0xB);
        assert_eq!(TAG_IMMED1_SMALL, 0xF);
        assert_eq!(TAG_IMMED2_MASK, 0x3F);
        assert_eq!(TAG_IMMED2_ATOM, 0xB);
        assert_eq!(TAG_IMMED2_CATCH, 0x1B);
        assert_eq!(TAG_IMMED2_NIL, 0x3B);
    }

    #[test]
    fn header_constants_match_erl_term_h() {
        assert_eq!(TAG_HEADER_MASK, 0x3F);
        assert_eq!(HEADER_ARITY_OFFS, 6);
        assert_eq!(HEADER_SUBTAG_MASK, 0x3C);
        assert_eq!(MAX_ARITYVAL, (1 << 24) - 1);
        assert_eq!(TRANSPARENT_TAG_MASK, 0x3B);
        assert_eq!(BIG_TAG_MASK, 0x3B);
        assert_eq!(BIG_SIGN_BIT, 0x4);
        assert_eq!(BITSTRING_TAG_MASK, 0x3B);
        assert_eq!(EXTERNAL_TAG_MASK, 0x33);
    }

    #[test]
    fn subtag_values_match_erl_term_h() {
        assert_eq!(ARITYVAL_SUBTAG, 0x00);
        assert_eq!(POS_BIG_SUBTAG, 0x08);
        assert_eq!(NEG_BIG_SUBTAG, 0x0C);
        assert_eq!(REF_SUBTAG, 0x10);
        assert_eq!(FUN_SUBTAG, 0x14);
        assert_eq!(FLOAT_SUBTAG, 0x18);
        assert_eq!(RECORD_SUBTAG, 0x1C);
        assert_eq!(HEAP_BITS_SUBTAG, 0x20);
        assert_eq!(SUB_BITS_SUBTAG, 0x24);
        assert_eq!(BIN_REF_SUBTAG, 0x28);
        assert_eq!(MAP_SUBTAG, 0x2C);
        assert_eq!(EXTERNAL_PID_SUBTAG, 0x30);
        assert_eq!(EXTERNAL_PORT_SUBTAG, 0x34);
        assert_eq!(EXTERNAL_REF_SUBTAG, 0x38);
    }

    #[test]
    fn map_constants_match_erl_map_h() {
        assert_eq!(HEADER_MAP_SUBTAG_MASK, 0xFC);
        assert_eq!(HEADER_MAP_HASHMAP_HEAD_MASK, 0xBC);
        assert_eq!(HAMT_SUBTAG_NODE_BITMAP, 0x6C);
        assert_eq!(HAMT_SUBTAG_HEAD_ARRAY, 0xAC);
        assert_eq!(HAMT_SUBTAG_HEAD_BITMAP, 0xEC);
        assert_eq!(HAMT_SUBTAG_HEAD_FLATMAP, 0x2C);
    }

    #[test]
    fn primary_tag_classification() {
        assert_eq!(primary_tag(Eterm::from_raw(0x0)), PrimaryTag::Header);
        assert_eq!(primary_tag(Eterm::from_raw(0x1)), PrimaryTag::List);
        assert_eq!(primary_tag(Eterm::from_raw(0x2)), PrimaryTag::Boxed);
        assert_eq!(primary_tag(Eterm::from_raw(0x3)), PrimaryTag::Immed1);
        assert_eq!(primary_tag(Eterm::from_raw(0x3B)), PrimaryTag::Immed1);
        assert!(is_header(Eterm::from_raw(0)));
        assert!(!is_header(Eterm::from_raw(1)));
    }

    #[test]
    fn boxed_tag_from_raw() {
        assert_eq!(BoxedTag::from_raw(0x00), Some(BoxedTag::ArityVal));
        assert_eq!(BoxedTag::from_raw(0x18), Some(BoxedTag::Float));
        assert_eq!(BoxedTag::from_raw(0x38), Some(BoxedTag::ExternalRef));
        assert_eq!(BoxedTag::from_raw(0x04), None);
        assert_eq!(BoxedTag::from_raw(0x3C), None);
    }
}
