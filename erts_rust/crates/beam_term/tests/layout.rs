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

//! Bit-pattern pins for the frozen BEAM term layout (`tests/layout.rs`).
//!
//! Ground truth: `erts/emulator/beam/erl_term.h`. Every expected value below is
//! the exact expansion of the corresponding C macro on a 64-bit build, so this
//! file is the executable spec of the Level-1 drop-in term layout (workspace
//! `CONTEXT.md` §2). It complements the per-module unit tests by consuming the
//! crate *only* through its public API (`beam_term::prelude::*`), thereby
//! validating the flat re-exports and the prelude as well.

use beam_term::prelude::*;

// ---------------------------------------------------------------------------
// Tag constants (erl_term.h: _TAG_PRIMARY_* / TAG_PTR_MASK__ / TAG_LITERAL_PTR)
// ---------------------------------------------------------------------------

#[test]
fn primary_and_pointer_tag_constants() {
    assert_eq!(TAG_PRIMARY_SIZE, 2);
    assert_eq!(TAG_PRIMARY_MASK, 0x3);
    assert_eq!(TAG_PRIMARY_HEADER, 0x0);
    assert_eq!(TAG_PRIMARY_LIST, 0x1);
    assert_eq!(TAG_PRIMARY_BOXED, 0x2);
    assert_eq!(TAG_PRIMARY_IMMED1, 0x3);

    // 64-bit build: TAG_PTR_MASK__ == 0x7, TAG_LITERAL_PTR == 0x4
    assert_eq!(TAG_PTR_MASK, 0x7);
    assert_eq!(TAG_LITERAL_PTR, 0x4);
    // The literal bit sits inside the pointer-tag mask.
    assert_eq!(TAG_LITERAL_PTR & TAG_PTR_MASK, TAG_LITERAL_PTR);
}

#[test]
fn immed1_and_immed2_tag_values() {
    assert_eq!(TAG_IMMED1_SIZE, 4);
    assert_eq!(TAG_IMMED1_MASK, 0xF);
    assert_eq!(TAG_IMMED1_PID, 0x3);
    assert_eq!(TAG_IMMED1_PORT, 0x7);
    assert_eq!(TAG_IMMED1_IMMED2, 0xB);
    assert_eq!(TAG_IMMED1_SMALL, 0xF);

    assert_eq!(TAG_IMMED2_SIZE, 6);
    assert_eq!(TAG_IMMED2_MASK, 0x3F);
    assert_eq!(TAG_IMMED2_ATOM, 0xB);
    assert_eq!(TAG_IMMED2_CATCH, 0x1B);
    assert_eq!(TAG_IMMED2_NIL, 0x3B);

    // Consistency of the layered tags (the C macros build these from each other).
    assert_eq!(TAG_IMMED1_SMALL, TAG_IMMED1_MASK);
    assert_eq!(TAG_IMMED1_PID, TAG_PRIMARY_IMMED1);
    assert_eq!(TAG_IMMED1_IMMED2, TAG_IMMED2_ATOM);
    assert_eq!(TAG_IMMED2_SIZE, TAG_IMMED1_SIZE + TAG_PRIMARY_SIZE);
}

#[test]
fn header_layout_constants() {
    assert_eq!(TAG_HEADER_MASK, 0x3F);
    assert_eq!(HEADER_ARITY_OFFS, 6);
    assert_eq!(HEADER_SUBTAG_MASK, 0x3C);
    assert_eq!(MAX_ARITYVAL, (1 << 24) - 1);
    // The arity field starts right above the 6-bit header tag word.
    assert_eq!(HEADER_ARITY_OFFS, TAG_IMMED2_SIZE);
    assert_eq!(HEADER_SUBTAG_MASK, TAG_HEADER_MASK & !TAG_PRIMARY_MASK);
}

// ---------------------------------------------------------------------------
// Small integers (erl_term.h: SMALL_BITS / MAX_SMALL / MIN_SMALL / make_small)
// ---------------------------------------------------------------------------

#[test]
fn small_integer_layout_is_pinned() {
    assert_eq!(SMALL_BITS, 60);
    assert_eq!(MAX_SMALL, (1i64 << 59) - 1);
    assert_eq!(MIN_SMALL, -(1i64 << 59));

    assert_eq!(make_small(0).raw(), 0xF);
    assert_eq!(make_small(1).raw(), 0x1F);
    assert_eq!(make_small(2).raw(), 0x2F);
    assert_eq!(make_small(-1).raw(), ((-1isize as usize) << 4) | 0xF);
    // Encode = (value << _TAG_IMMED1_SIZE) + _TAG_IMMED1_SMALL.
    assert_eq!(make_small(42).raw(), (42usize << 4) + 0xF);

    assert_eq!(make_small(0).raw() & TAG_IMMED1_MASK, TAG_IMMED1_SMALL);
    assert_eq!(primary_tag(make_small(0)), PrimaryTag::Immed1);
}

#[test]
fn small_integer_boundary_sweep() {
    // A dense interior run plus the exact boundaries and their neighbours.
    let mut values: Vec<i64> = (-300..=300).collect();
    values.extend_from_slice(&[
        MIN_SMALL,
        MIN_SMALL + 1,
        MIN_SMALL + 2,
        -256,
        -255,
        -1,
        0,
        1,
        255,
        256,
        MAX_SMALL - 2,
        MAX_SMALL - 1,
        MAX_SMALL,
    ]);

    for &v in &values {
        let t = make_small(v);
        assert!(is_small(t), "not small: {v}");
        assert!(!is_not_small(t));
        assert!(is_immediate(t));
        assert_eq!(t.raw() & TAG_IMMED1_MASK, TAG_IMMED1_SMALL);
        assert_eq!(t.raw(), ((v as usize) << TAG_IMMED1_SIZE) + TAG_IMMED1_SMALL);
        assert_eq!(signed_value(t), v, "signed round-trip: {v}");
        // `unsigned_value` is the raw 60-bit payload (no sign extension).
        assert_eq!(
            unsigned_value(t),
            (v as u64) & ((1u64 << SMALL_BITS) - 1),
            "unsigned payload: {v}"
        );
    }
}

#[test]
fn signed_and_unsigned_value_are_complementary() {
    // For non-negative values the unsigned payload equals the value itself.
    for v in [0i64, 1, 2, 255, 256, MAX_SMALL] {
        let t = make_small(v);
        assert_eq!(unsigned_value(t), v as u64);
        assert_eq!(signed_value(t), v);
    }
    // For negative values the unsigned payload is the low 60 bits of the
    // two's-complement encoding.
    for v in [-1i64, -2, -256, MIN_SMALL] {
        let t = make_small(v);
        assert_eq!(unsigned_value(t), (v as u64) & ((1u64 << SMALL_BITS) - 1));
        assert_eq!(signed_value(t), v);
    }
}

#[test]
fn try_make_small_rejects_out_of_range() {
    assert_eq!(try_make_small(0).unwrap(), make_small(0));
    assert_eq!(try_make_small(MAX_SMALL).unwrap(), make_small(MAX_SMALL));
    assert_eq!(try_make_small(MIN_SMALL).unwrap(), make_small(MIN_SMALL));
    assert_eq!(
        try_make_small(MAX_SMALL + 1),
        Err(TermError::SmallIntOutOfRange)
    );
    assert_eq!(
        try_make_small(MIN_SMALL - 1),
        Err(TermError::SmallIntOutOfRange)
    );
    assert_eq!(try_make_small(i64::MAX), Err(TermError::SmallIntOutOfRange));
    assert_eq!(try_make_small(i64::MIN), Err(TermError::SmallIntOutOfRange));
}

#[test]
fn is_byte_range() {
    for v in 0..=255i64 {
        assert!(is_byte(make_small(v)), "byte {v} must be a byte");
    }
    assert!(!is_byte(make_small(256)));
    assert!(!is_byte(make_small(-1)));
    assert!(!is_byte(NIL));
}

// ---------------------------------------------------------------------------
// Atoms (erl_term.h: _TAG_IMMED2_ATOM / make_atom / MAX_ATOM_INDEX)
// ---------------------------------------------------------------------------

#[test]
fn atom_immediate_layout_is_pinned() {
    assert_eq!(ATOM_INDEX_BITS, 6);
    assert_eq!(ATOM_INDEX_BITS, TAG_IMMED2_SIZE);

    assert_eq!(make_atom(Atom::from_raw(0)).raw(), 0xB);
    assert_eq!(make_atom(Atom::from_raw(0)).raw(), TAG_IMMED2_ATOM);
    assert_eq!(make_atom(Atom::from_raw(1)).raw(), 0x4B);
    assert_eq!(make_atom(Atom::from_raw(2)).raw(), 0x8B);

    // encode = (index << _TAG_IMMED2_SIZE) + _TAG_IMMED2_ATOM, and it round-trips.
    for idx in [0u32, 1, 2, 42, 1000, u32::MAX] {
        let a = Atom::from_raw(idx);
        let t = make_atom(a);
        assert_eq!(t.raw(), ((idx as usize) << TAG_IMMED2_SIZE) + TAG_IMMED2_ATOM);
        assert_eq!(t.raw() & TAG_IMMED2_MASK, TAG_IMMED2_ATOM);
        assert!(is_atom(t));
        assert_eq!(atom_val(t), a);
    }
    assert!(!is_atom(NIL));
    assert!(!is_atom(make_small(0)));
}

// ---------------------------------------------------------------------------
// NIL (erl_term.h: NIL == _TAG_IMMED2_NIL)
// ---------------------------------------------------------------------------

#[test]
fn nil_layout_is_pinned() {
    assert_eq!(NIL.raw(), 0x3B);
    assert_eq!(NIL.raw(), TAG_IMMED2_NIL);
    assert!(is_nil(NIL));
    assert!(!is_not_nil(NIL));
    assert!(is_immediate(NIL));
    assert!(!is_small(NIL));
    assert!(!is_atom(NIL));
    // `is_list(NIL)` is false: NIL is an IMMED2 immediate, not a cons cell.
    assert!(!is_list(NIL));
    assert_eq!(primary_tag(NIL), PrimaryTag::Immed1);
}

// ---------------------------------------------------------------------------
// Catch (erl_term.h: make_catch / _TAG_IMMED2_CATCH)
// ---------------------------------------------------------------------------

#[test]
fn catch_layout_is_pinned() {
    assert_eq!(make_catch(0).raw(), TAG_IMMED2_CATCH);
    assert_eq!(TAG_IMMED2_CATCH, 0x1B);
    for x in [0u64, 1, 42, (1u64 << 57) - 1] {
        let t = make_catch(x);
        assert!(is_catch(t));
        assert_eq!(catch_val(t), x);
        assert_eq!(t.raw(), ((x as usize) << TAG_IMMED2_SIZE) | TAG_IMMED2_CATCH);
    }
    assert!(!is_catch(NIL));
}

// ---------------------------------------------------------------------------
// Internal pid / port immediates
// ---------------------------------------------------------------------------

#[test]
fn pid_and_port_immediate_layout() {
    let pid = make_internal_pid(make_pid_data(3, 5));
    assert_eq!(pid.raw() & TAG_IMMED1_MASK, TAG_IMMED1_PID);
    assert!(is_internal_pid(pid));
    assert!(!is_internal_port(pid));
    assert_eq!(pid_number(pid), 5);
    assert_eq!(pid_serial(pid), 3);

    let port = make_internal_port(make_port_data(9));
    assert_eq!(port.raw() & TAG_IMMED1_MASK, TAG_IMMED1_PORT);
    assert!(is_internal_port(port));
    assert!(!is_internal_pid(port));
    assert_eq!(port_number(port), 9);
}

// ---------------------------------------------------------------------------
// Arityvals (erl_term.h: make_arityval / MAX_ARITYVAL)
// ---------------------------------------------------------------------------

#[test]
fn arityval_layout_is_pinned() {
    // The zero-arity header encodes to the raw word 0. `make_arityval`'s contract
    // is `sz > 0`, so arity 0 is produced by `make_arityval_zero` /
    // `make_arityval_unchecked(0)`; all three encode the same word here.
    assert_eq!(make_arityval_zero().raw(), 0);
    assert_eq!(make_arityval_unchecked(0).raw(), 0);
    assert_eq!(make_header(0, 0).raw(), 0);

    assert_eq!(make_arityval(1).raw(), 0x40);
    assert_eq!(make_arityval(2).raw(), 0x80);
    assert_eq!(make_arityval(1).raw(), 1usize << HEADER_ARITY_OFFS);

    // encode = size << _HEADER_ARITY_OFFS (subtag 0).
    assert_eq!(
        make_arityval_unchecked(MAX_ARITYVAL).raw(),
        MAX_ARITYVAL << HEADER_ARITY_OFFS
    );
    assert_eq!(
        arityval(make_arityval_unchecked(MAX_ARITYVAL)),
        MAX_ARITYVAL
    );
    assert_eq!((MAX_ARITYVAL << HEADER_ARITY_OFFS) >> HEADER_ARITY_OFFS, MAX_ARITYVAL);
}

#[test]
fn arityval_boundary_sweep() {
    let values = [
        0usize,
        1,
        2,
        3,
        255,
        256,
        (1 << 16),
        MAX_ARITYVAL - 1,
        MAX_ARITYVAL,
    ];
    for &n in &values {
        let t = make_arityval_unchecked(n);
        assert_eq!(arityval(t), n, "arityval round-trip: {n}");
        assert_eq!(t.raw(), n << HEADER_ARITY_OFFS);
        assert!(is_arity_value(t), "is_arity_value: {n}");
        assert!(is_sane_arity_value(t), "is_sane_arity_value: {n}");
        assert_eq!(thing_arityval(t), n);
        // Primary tag is HEADER (the arityval tag is subtag 0).
        assert_eq!(primary_tag(t), PrimaryTag::Header);
        // Same encoding via the non-zero constructor.
        if n > 0 {
            assert_eq!(make_arityval(n), t);
        }
    }
}

#[test]
fn try_make_arityval_range() {
    assert_eq!(try_make_arityval(0).unwrap(), make_arityval_zero());
    assert_eq!(
        try_make_arityval(MAX_ARITYVAL).unwrap(),
        make_arityval_unchecked(MAX_ARITYVAL)
    );
    assert_eq!(
        try_make_arityval(MAX_ARITYVAL + 1),
        Err(TermError::ArityOutOfRange)
    );
    assert_eq!(try_make_arityval(usize::MAX), Err(TermError::ArityOutOfRange));
}

// ---------------------------------------------------------------------------
// Literal-bit masking (erl_term.h: TAG_LITERAL_PTR / *_val)
// ---------------------------------------------------------------------------

#[test]
fn literal_bit_is_masked_off_boxed_and_list() {
    assert_eq!(TAG_LITERAL_PTR, 0x4);

    // A boxed tuple on the stack.
    let mut tuple_words = [make_arityval(1), make_small(7)];
    let tuple_base = tuple_words.as_mut_ptr() as usize;
    let boxed = make_boxed(tuple_words.as_ptr());
    assert_eq!(boxed.raw() & TAG_PTR_MASK, TAG_PRIMARY_BOXED);
    assert_eq!(boxed_val(boxed) as usize, tuple_base);
    assert!(!is_literal(boxed));

    let lit_boxed = Eterm::from_raw(boxed.raw() | TAG_LITERAL_PTR);
    assert_eq!(lit_boxed.raw() & TAG_PTR_MASK, TAG_PRIMARY_BOXED | TAG_LITERAL_PTR);
    assert!(is_literal(lit_boxed));
    // `boxed_val` removes the literal bit (masks with !TAG_PTR_MASK).
    assert_eq!(boxed_val(lit_boxed) as usize, tuple_base);
    assert_eq!(make_literal(boxed), lit_boxed);
    assert_eq!(clear_literal(lit_boxed), boxed);

    // A cons cell on the stack.
    let mut cons = [make_small(1), NIL];
    let cons_base = cons.as_mut_ptr() as usize;
    let list = make_list(cons.as_ptr());
    assert_eq!(list.raw() & TAG_PTR_MASK, TAG_PRIMARY_LIST);
    assert_eq!(list_val(list) as usize, cons_base);

    let lit_list = Eterm::from_raw(list.raw() | TAG_LITERAL_PTR);
    assert_eq!(lit_list.raw() & TAG_PTR_MASK, TAG_PRIMARY_LIST | TAG_LITERAL_PTR);
    assert_eq!(list_val(lit_list) as usize, cons_base);
    assert_eq!(ptr_val(lit_list) as usize, cons_base);
}

#[test]
fn pointer_value_masks_every_low_tag_bit() {
    // For an 8-byte-aligned base, ORing in any of the 8 possible low-3-bit tag
    // patterns must still yield the base pointer once masked.
    let base = 0x1000usize;
    for low in 0..8usize {
        let w = Eterm::from_raw(base | low);
        assert_eq!(ptr_val(w) as usize, base, "ptr_val low={low}");
        assert_eq!(boxed_val(w) as usize, base, "boxed_val low={low}");
        assert_eq!(list_val(w) as usize, base, "list_val low={low}");
    }
}

// ---------------------------------------------------------------------------
// Primary-tag classification, cross-checked with the bit layout
// ---------------------------------------------------------------------------

#[test]
fn primary_tag_classification_and_predicates() {
    let cases = [
        (0x0usize, PrimaryTag::Header),
        (0x1, PrimaryTag::List),
        (0x2, PrimaryTag::Boxed),
        (0x3, PrimaryTag::Immed1),
        (0xF, PrimaryTag::Immed1),
        (0x3B, PrimaryTag::Immed1),
    ];
    for (raw, expected) in cases {
        let t = Eterm::from_raw(raw);
        assert_eq!(primary_tag(t), expected, "primary tag of {raw:#x}");
        assert_eq!(is_header(t), expected == PrimaryTag::Header);
        assert_eq!(t.raw() & TAG_PRIMARY_MASK, raw & TAG_PRIMARY_MASK);
    }

    // Classification of constructed terms.
    let mut tuple_words = [make_arityval(1), NIL];
    let tuple = make_tuple(tuple_words.as_mut_ptr());
    let mut cons = [make_small(0), NIL];
    let list = make_list(cons.as_mut_ptr());
    assert_eq!(primary_tag(tuple), PrimaryTag::Boxed);
    assert_eq!(primary_tag(list), PrimaryTag::List);
    assert_eq!(primary_tag(make_small(0)), PrimaryTag::Immed1);
    assert!(is_boxed(tuple) && is_list(list));
}
