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

//! The `bs_create_bin` **`integer` segment in little-endian order**, at every field
//! width — in particular the **bit-granular** widths (`nbits % 8 != 0`) the standard
//! compiler emits (`<<V:12/little>>`, `<<V:20/little>>`, `<<V:4/little>>`, …).
//!
//! # The C rule
//!
//! `erts_bs_put_integer_le` (`erl_bits.c:1031`) formats the field through
//! `fmt_small_le`/`fmt_big_le` (`:686`/`:812`) as the value's little-endian bytes,
//! each byte written **most-significant bit first**, with the final partial group
//! (`nbits % 8` bits) carrying the field's **remaining high bits**
//! (`value >> 8*(nbytes-1)`'s low bits) **left-aligned** in the last byte. `-little`
//! never changes the numeric rounding: the field holds `value mod 2^nbits` (a
//! negative value's `nbits`-bit two's complement), exactly as the big-endian default.
//! A field that does not start on a byte boundary is that same bit sequence placed at
//! the current bit position (`restore_and_shift`, `erl_bits.c:891`), so the layout is
//! independent of the destination's bit offset.
//!
//! # Oracle vectors
//!
//! Every expected byte below is the **live oracle's** (system OTP 29.1.1 /
//! erts-17.1, a little-endian host):
//! ```text
//! {<<16#0102:12/little>>, <<16#0102:20/little>>, <<16#0102:4/little>>,
//!  <<16#ABC:12/little>>, <<16#1ABC:12/little>>, <<16#0102:12>>,
//!  <<1:1,16#ABC:12/little>>, <<1:3,16#ABC:12/little>>, <<16#ABC:12/little,1:1>>,
//!  <<(-1):12/little>>, <<(-3):4/little>>, <<200:4/little>>,
//!  <<16#0102:8/little>>, <<16#0102:16/little>>, <<16#0102:24/little>>,
//!  <<16#0102:32/little>>}
//! ⇒ {<<2,1:4>>, <<2,1,0:4>>, <<2:4>>, <<188,10:4>>, <<188,10:4>>, <<16,2:4>>,
//!    <<222,10:5>>, <<55,74:7>>, <<188,21:5>>, <<255,15:4>>, <<13:4>>, <<8:4>>,
//!    <<2>>, <<2,1>>, <<2,1,0>>, <<2,1,0,0>>}
//! ```
//! and, at field widths wider than a machine word,
//! ```text
//! X = (1 bsl 70) + 16#1ABC, Y = (1 bsl 70) + 16#ABC, B = 16#1122334455667788
//! {<<X:12/little>>, <<X:20/little>>, <<X:4/little>>, <<X:1/little>>,
//!  <<(-X):12/little>>, <<Y:72/little>>, <<Y:72>>, <<B:72>>, <<B:72/little>>,
//!  <<(1 bsl 100):72>>, <<(1 bsl 100):72/little>>}
//! ⇒ {<<188,10:4>>, <<188,26,0:4>>, <<12:4>>, <<0:1>>, <<68,5:4>>,
//!    <<188,10,0,0,0,0,0,0,64>>, <<64,0,0,0,0,0,0,10,188>>,
//!    <<0,17,34,51,68,85,102,119,136>>, <<136,119,102,85,68,51,34,17,0>>,
//!    <<0,0,0,0,0,0,0,0,0>>, <<0,0,0,0,0,0,0,0,0>>}
//! ```
//! The failure classes are the same at every width: a float, an atom or a list value
//! is `{error,badarg}` (`<<3.14:72>>`, `<<a:72>>`, `<<[1]:72>>`), because
//! `erts_bs_put_integer_le` returns 0 for a non-integer (`erl_bits.c:1152-1155`).

use std::collections::BTreeMap;

use super::wide_tests::{bignum, build, flag_names, flag_operand, seg_module};
use super::*;
use beam_loader::{Export, Function, NO_FAIL};
use beam_term::{make_atom, make_small, Atom};

use crate::context::MinimalContext;
use crate::terms::binary_bytes;

/// The `(bytes, bit count)` a **small** `value` builds in a `nbits`-bit field with
/// `little`/`signed` flags, through the segment assembler.
fn build_small(value: i64, nbits: i64, little: bool, signed: bool) -> (Vec<u8>, usize) {
    let names = flag_names(little, signed);
    let module = seg_module(&names);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    machine.set_x(0, make_small(value)).expect("x(0) in range");
    build(
        &module,
        &mut machine,
        &mut ctx,
        1,
        flag_operand(&names),
        Operand::X(0),
        Operand::Integer(nbits),
    )
    .expect("assemble")
    .expect("the build succeeds")
}

/// Run the segment walk over a multi-segment list with `NO_FAIL` (so a segment
/// failure raises).
fn run_segments(
    module: &Module,
    machine: &mut Machine,
    ctx: &mut MinimalContext,
    segments: &[Operand],
) -> (Vec<u8>, usize) {
    assemble(
        machine,
        module,
        &LabelMap::default(),
        NO_FAIL,
        segments,
        ctx,
    )
    .map(|built| built.map(built_bytes))
    .expect("assemble")
    .expect("the build succeeds")
}

/// One `integer` segment `[type, index, unit, flags, value, size]`.
fn int_segment(ty: Atom, index: i64, flags: Operand, value: Operand, size: Operand) -> [Operand; 6] {
    [
        Operand::Atom(ty),
        Operand::Integer(index),
        Operand::Integer(1),
        flags,
        value,
        size,
    ]
}

/// The `little` flag literal a segment needs [`seg_module`] to hold.
fn little_flag() -> Operand {
    flag_operand(&["little"])
}

/// The oracle's little-endian field layout, field width by field width, for a
/// **small** value: the least-significant 8-bit group first (each group written
/// most-significant bit first), the final partial group carrying the field's
/// remaining high bits left-aligned in the last byte.
#[test]
fn a_little_endian_field_is_bit_granular_like_the_oracle() {
    // (value, nbits, little, signed, oracle bytes)
    let cases: &[(i64, i64, bool, bool, &[u8])] = &[
        // `<<16#0102:12/little>>` = <<2,1:4>> — the final 4 bits are the *low* nibble
        // of the value's second byte.
        (0x0102, 12, true, false, &[0x02, 0x10]),
        (0x0102, 20, true, false, &[0x02, 0x01, 0x00]),
        (0x0102, 4, true, false, &[0x20]),
        (0x0102, 1, true, false, &[0x00]),
        (0x0102, 12, true, true, &[0x02, 0x10]),
        // The big-endian default for contrast (`<<16#0102:12>>` = <<16,2:4>>).
        (0x0102, 12, false, false, &[0x10, 0x20]),
        // `<<16#ABC:12/little>>` = <<188,10:4>>; a *high* nibble reading would give
        // <<188,1:4>>.
        (0xABC, 12, true, false, &[0xBC, 0xA0]),
        (0xABC, 12, true, true, &[0xBC, 0xA0]),
        (0xABC, 20, true, false, &[0xBC, 0x0A, 0x00]),
        (0xABC, 4, true, false, &[0xC0]),
        (0xABC, 1, true, false, &[0x00]),
        // `<<16#1ABC:12/little>>` = <<188,10:4>>: the field keeps bits 0..11, so the
        // dropped bit 12 never reappears in the partial group.
        (0x1ABC, 12, true, false, &[0xBC, 0xA0]),
        // Masking/rounding, unchanged by the endianness (`<<200:4/little>>` and
        // `<<200:4>>` are both <<8:4>>), and a negative value's two's complement.
        (200, 4, true, false, &[0x80]),
        (200, 4, false, false, &[0x80]),
        (-3, 4, true, false, &[0xD0]),
        (-1, 12, true, false, &[0xFF, 0xF0]),
        (-1, 12, true, true, &[0xFF, 0xF0]),
        (-1, 20, true, false, &[0xFF, 0xFF, 0xF0]),
        (0, 12, true, false, &[0x00, 0x00]),
        (0x0102, 0, true, false, &[]),
        // Byte-aligned widths stay byte-identical (regression guard).
        (0x0102, 8, true, false, &[0x02]),
        (0x0102, 16, true, false, &[0x02, 0x01]),
        (0x0102, 24, true, false, &[0x02, 0x01, 0x00]),
        (0x0102, 32, true, false, &[0x02, 0x01, 0x00, 0x00]),
        (0x0102, 16, true, true, &[0x02, 0x01]),
        // A `native` segment is the machine endianness — on this little-endian host
        // the little-endian layout, at every width.
        (0x0102, 12, true, false, &[0x02, 0x10]),
    ];
    for &(value, nbits, little, signed, expected) in cases {
        let (bytes, out_bits) = build_small(value, nbits, little, signed);
        assert_eq!(out_bits, nbits as usize, "the field is exactly {nbits} bits");
        assert_eq!(
            bytes, expected,
            "value={value:#x}, little={little}, signed={signed}, {nbits} bits"
        );
    }
}

/// A little-endian field **follows the bits already built**: the field's bit
/// sequence is placed at the current position (C's `restore_and_shift`,
/// `erl_bits.c:891`), so a preceding segment simply prefix it and a following
/// segment continues at the next bit.
#[test]
fn a_little_endian_field_continues_at_the_current_bit_position() {
    let module = seg_module(&["little"]);
    let ty = module.atom_table.lookup("integer").expect("the type atom");
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    machine.set_x(0, make_small(0xABC)).expect("x(0) in range");
    machine.set_x(1, make_small(1)).expect("x(1) in range");

    // `<<1:1, 16#ABC:12/little>>` = <<222,10:5>>: 1 bit, then the 12-bit field.
    let segments: Vec<Operand> = int_segment(ty, 1, Operand::Nil, Operand::X(1), Operand::Integer(1))
        .into_iter()
        .chain(int_segment(
            ty,
            2,
            little_flag(),
            Operand::X(0),
            Operand::Integer(12),
        ))
        .collect();
    assert_eq!(
        run_segments(&module, &mut machine, &mut ctx, &segments),
        (vec![0xDE, 0x50], 13)
    );

    // `<<1:3, 16#ABC:12/little>>` = <<55,74:7>>: a 3-bit prefix pushes the field's
    // bits one byte later, mid-byte.
    let segments: Vec<Operand> = int_segment(ty, 1, Operand::Nil, Operand::X(1), Operand::Integer(3))
        .into_iter()
        .chain(int_segment(
            ty,
            2,
            little_flag(),
            Operand::X(0),
            Operand::Integer(12),
        ))
        .collect();
    assert_eq!(
        run_segments(&module, &mut machine, &mut ctx, &segments),
        (vec![0x37, 0x94], 15)
    );

    // `<<16#ABC:12/little, 1:1>>` = <<188,21:5>>: a segment *after* the partial group
    // continues at the very next bit — the group is not padded to a byte boundary.
    let segments: Vec<Operand> = int_segment(
        ty,
        1,
        little_flag(),
        Operand::X(0),
        Operand::Integer(12),
    )
    .into_iter()
    .chain(int_segment(ty, 2, Operand::Nil, Operand::X(1), Operand::Integer(1)))
    .collect();
    assert_eq!(
        run_segments(&module, &mut machine, &mut ctx, &segments),
        (vec![0xBC, 0xA8], 13)
    );
}

/// The field width may come from a runtime `size(Expr)` operand (an x-register) —
/// including a width that is not a multiple of 8, which is the shape the standard
/// compiler emits for a runtime-sized little-endian segment (`<<V:N/little>>`).
#[test]
fn a_runtime_sized_little_endian_field_matches_the_oracle() {
    for (value, nbits, expected) in [
        (0x0102i64, 4i64, &[0x20u8][..]),
        (0x0102, 12, &[0x02, 0x10]),
        (0x0102, 20, &[0x02, 0x01, 0x00]),
        (0xABC, 12, &[0xBC, 0xA0]),
    ] {
        let module = seg_module(&["little"]);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        machine.set_x(0, make_small(value)).expect("x(0) in range");
        machine
            .set_x(1, make_small(nbits))
            .expect("x(1) in range");
        let (bytes, out_bits) = build(
            &module,
            &mut machine,
            &mut ctx,
            1,
            little_flag(),
            Operand::X(0),
            Operand::X(1),
        )
        .expect("assemble")
        .expect("the build succeeds");
        assert_eq!((bytes, out_bits), (expected.to_vec(), nbits as usize));
    }
}

/// The same layout at a **bignum** value, narrow field and wide field alike: the
/// bignum route (`fmt_big_le`) shares the little-endian rule, so a bignum whose
/// partial group has to drop its high bits (and one whose wide field is a whole
/// number of bytes) is written exactly as the oracle does.
#[test]
fn a_bignum_little_endian_field_is_bit_granular_like_the_oracle() {
    // X = (1 bsl 70) + 16#1ABC: `2^70` is digit 1 (`1 << 6`), the low 16 bits are
    // `0x1ABC`.
    let x_digits: [u64; 2] = [0x1ABC, 1 << 6];
    // Y = (1 bsl 70) + 16#ABC — the 72-bit vectors the oracle names.
    let y_digits: [u64; 2] = [0xABC, 1 << 6];
    let cases: &[(bool, [u64; 2], i64, bool, &[u8])] = &[
        // <<X:12/little>> = <<188,10:4>> (bits 12..15 are dropped, so the partial
        // group is the *low* nibble of byte 1 = 0xA).
        (false, x_digits, 12, true, &[0xBC, 0xA0]),
        (false, x_digits, 20, true, &[0xBC, 0x1A, 0x00]),
        (false, x_digits, 4, true, &[0xC0]),
        (false, x_digits, 1, true, &[0x00]),
        // <<(-X):12/little>> = <<68,5:4>>: the nbits-bit two's complement first.
        (true, x_digits, 12, true, &[0x44, 0x50]),
        // <<X:12>> = <<171,12:4>> — the big-endian default for contrast.
        (false, x_digits, 12, false, &[0xAB, 0xC0]),
        // Wide fields: <<Y:72/little>> = <<188,10,0,0,0,0,0,0,64>> and the BE twin.
        (false, y_digits, 72, true, &[0xBC, 0x0A, 0, 0, 0, 0, 0, 0, 0x40]),
        (false, y_digits, 72, false, &[0x40, 0, 0, 0, 0, 0, 0, 0x0A, 0xBC]),
    ];
    for &(negative, digits, nbits, little, expected) in cases {
        let names = flag_names(little, false);
        let module = seg_module(&names);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let value = bignum(&mut machine, &mut ctx, negative, &digits);
        machine.set_x(0, value).expect("x(0) in range");
        let out = build(
            &module,
            &mut machine,
            &mut ctx,
            1,
            flag_operand(&names),
            Operand::X(0),
            Operand::Integer(nbits),
        )
        .expect("assemble")
        .expect("the build succeeds");
        assert_eq!(
            out,
            (expected.to_vec(), nbits as usize),
            "negative={negative}, little={little}, {nbits} bits"
        );
    }
}

/// A non-integer value is the bare `badarg` at **every** field width — including the
/// wide/`little` route — and a valid wide value still builds (the positive controls
/// are the oracle's `<<16#1122334455667788:72>>` and `<<(1 bsl 100):72>>`).
#[test]
fn a_non_integer_value_is_badarg_at_every_width_like_the_oracle() {
    let mut module = seg_module(&["little"]);
    let atom = module.atom_table.intern("a");
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let float = crate::terms::test_support::make_float(&mut ctx, 3.14).expect("a float");
    let list = crate::terms::make_list(&mut ctx, &[make_small(1)]).expect("a list");
    for (label, value) in [
        ("a float", float),
        ("a list", list),
        ("an atom", make_atom(atom)),
    ] {
        for (nbits, little) in [(72i64, false), (72, true), (12, false), (12, true)] {
            machine.set_x(0, value).expect("x(0) in range");
            let names: &[&str] = if little { &["little"] } else { &[] };
            let err = build(
                &module,
                &mut machine,
                &mut ctx,
                1,
                flag_operand(names),
                Operand::X(0),
                Operand::Integer(nbits),
            )
            .expect_err("a non-integer value must fail the segment");
            assert_eq!(
                err.as_atom(),
                Some("badarg"),
                "{label} in a {nbits}-bit little={little} field: {err:?}"
            );
        }
    }
    // An `Operand::Atom` value (the operand form, not an x-register) is rejected too.
    let err = build(
        &module,
        &mut machine,
        &mut ctx,
        1,
        Operand::Nil,
        Operand::Atom(atom),
        Operand::Integer(72),
    )
    .expect_err("an atom operand must fail the segment");
    assert_eq!(err.as_atom(), Some("badarg"), "an atom operand: {err:?}");

    // The positive controls: values that need a bignum build at 72 bits, big- and
    // little-endian, exactly as the oracle does. `0x1122334455667788` is one digit,
    // `1 bsl 100` two (`2^100 = 2^64 * 2^36`), i.e. nine bytes with zero high digits.
    const BIG_BE: [u8; 9] = [0, 17, 34, 51, 68, 85, 102, 119, 136];
    const BIG_LE: [u8; 9] = [136, 119, 102, 85, 68, 51, 34, 17, 0];
    const ZEROS: [u8; 9] = [0; 9];
    for (digits, expected_be, expected_le) in [
        (&[0x1122_3344_5566_7788u64][..], &BIG_BE, &BIG_LE),
        (&[0u64, 1 << 36][..], &ZEROS, &ZEROS),
    ] {
        for (little, expected) in [(false, expected_be), (true, expected_le)] {
            let names = flag_names(little, false);
            let module = seg_module(&names);
            let mut machine = Machine::new();
            let mut ctx = MinimalContext::new();
            let value = bignum(&mut machine, &mut ctx, false, digits);
            machine.set_x(0, value).expect("x(0) in range");
            let out = build(
                &module,
                &mut machine,
                &mut ctx,
                1,
                flag_operand(&names),
                Operand::X(0),
                Operand::Integer(72),
            )
            .expect("assemble")
            .expect("the build succeeds");
            assert_eq!(out, (expected.to_vec(), 72), "little={little}");
        }
    }
}

/// Export `name/arity` at the (single) entry label of `code` and append it as a
/// function. The `run_tests` fixture pattern, replicated here so this node owns its
/// own end-to-end entry without reaching into a sibling test module's `Fixture`.
fn exported_function(module: &mut Module, name: &str, arity: u32, code: Vec<Instr>) -> Atom {
    let function = module.atom_table.intern(name);
    let module_name = module.name;
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    let entry = labels.keys().next().copied().unwrap_or(Label(0));
    module.exports.push(Export {
        function,
        arity,
        label: entry,
    });
    module.functions.push(Function {
        func_label: entry,
        entry_label: entry,
        module: module_name,
        name: function,
        arity,
        code,
        blocks: Vec::new(),
        labels,
    });
    function
}

/// The bit-granular little-endian field **end-to-end through the real public entry**
/// ([`Machine::run_export`]): a full `bs_create_bin` instruction
/// (`exec` → `assemble` → `raw::make_binary` → `write_operand`) builds the oracle's
/// exact bytes.
///
/// A following `0:4` padding segment makes the result byte-aligned so the safe
/// [`binary_bytes`] reader can inspect it (a raw-bit reader would need `unsafe`,
/// confined to `raw.rs`); the little-endian field itself is still bit-granular.
/// `<<16#0102:12/little, 0:4>>` = `<<2,16>>` and `<<16#ABC:12/little, 0:4>>` =
/// `<<188,160>>` are the live oracle's; the `:8` case is the byte-aligned regression.
#[test]
fn a_bit_granular_little_endian_field_builds_through_run_export() {
    for (value, nbits, expected) in [
        (0x0102i64, 12i64, &[0x02u8, 0x10][..]),
        (0xABC, 12, &[0xBC, 0xA0]),
        // A byte-aligned field is unchanged (regression guard).
        (0x0102, 8, &[0x02]),
    ] {
        let mut module = seg_module(&["little"]);
        let ty = module.atom_table.lookup("integer").expect("the type atom");
        let mut segments: Vec<Operand> =
            int_segment(ty, 1, little_flag(), Operand::X(0), Operand::Integer(nbits)).to_vec();
        // Pad to a byte boundary with `0:4` (or nothing for the `:8` case).
        let pad = (8 - nbits % 8) % 8;
        if pad != 0 {
            segments.extend(int_segment(
                ty,
                2,
                Operand::Nil,
                Operand::Integer(0),
                Operand::Integer(pad),
            ));
        }
        let code = vec![
            Instr::Label(Label(1)),
            Instr::BsCreateBin {
                fail: NO_FAIL,
                alloc: 0,
                live: 1,
                unit: 1,
                dst: Operand::X(0),
                segments,
            },
            Instr::Return,
        ];
        let function = exported_function(&mut module, "build", 1, code);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let out = machine
            .run_export(&module, function, 1, &[make_small(value)], &mut ctx)
            .expect("the `bs_create_bin` build returns");
        // The `0:pad` segment only fills the field's own trailing bits of the last
        // byte with zeros, so the byte image is unchanged (the oracle's already
        // has those bits zero).
        assert_eq!(
            binary_bytes(out).expect("a byte-aligned result"),
            expected,
            "value={value:#x}, {nbits} bits (+{pad} pad)"
        );
    }
}
