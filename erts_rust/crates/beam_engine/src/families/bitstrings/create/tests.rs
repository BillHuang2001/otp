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

//! Unit tests for the `bs_create_bin` segment helpers and encoders.

use super::*;
use beam_common::BeamError;
use beam_term::{make_atom, make_small, Atom};

use crate::context::MinimalContext;

use super::segments::{f64_to_f16, put_float, put_utf16, put_utf32, put_utf8, string_source};
use super::size::{float_nbits, raise_seg_fail, seg_nbits, segment_size, SegReason, SegSize};
use super::wide_tests::{flags_etf, literal_module};
use super::super::{decode_flags_slow_calls, BSF_NATIVE, BSF_SIGNED};

// --- `decode_flags` (the segment flags operand) ------------------------

/// The typed reason atom of an engine diagnostic, for the exact-string pins below.
fn reason_atom(error: BeamError) -> String {
    match error {
        BeamError::Raise(raise) => raise
            .reason
            .as_atom()
            .expect("an engine diagnostic is a reason atom")
            .to_string(),
        other => panic!("expected an `unsupported` raise, got {other:?}"),
    }
}

/// **`{literal,[]}` is answered by the inlined shell, exactly as `Nil` is.**
///
/// The OTP compiler spells a `bs_match` field's "no flags" as `{literal,[]}` — an
/// `Operand::Literal` over the NIL literal (`{integer,4,{literal,[]},6,1,{x,4}}`,
/// confirmed on `tests/fixtures/{bitstrings,bsbig,bssmall}.beam`) — while
/// `bs_create_bin`'s segments and `'=:='` carry the `a,0`/`Operand::Nil` form. Both
/// mean "no flags", so both are the shell's own arms: neither may enter the
/// out-of-line `decode_flags_slow` body (a call, a literal-area read and a
/// `list_elements` walk) merely to discover that.
#[test]
fn an_empty_list_literal_answers_no_flags_on_the_fast_path() {
    let module = literal_module(&[flags_etf(&[])]);
    let literal = Operand::Literal(0);
    assert!(
        module.literal(0).is_some_and(beam_term::is_nil),
        "the fixture's literal 0 must be the empty list"
    );

    let before = decode_flags_slow_calls();
    for _ in 0..8 {
        assert_eq!(
            decode_flags(&module, &literal).expect("no flags"),
            0,
            "`{{literal,[]}}` means no flags"
        );
    }
    assert_eq!(
        decode_flags_slow_calls(),
        before,
        "the empty-list literal must be answered by the shell, not the slow body"
    );

    // The `a,0` spelling of the same thing stays on the fast path too (and gives
    // the same answer), so the two spellings are indistinguishable to a caller.
    assert_eq!(decode_flags(&module, &Operand::Nil).expect("no flags"), 0);
    assert_eq!(decode_flags_slow_calls(), before);
}

/// A **non-empty** flags literal keeps the slow body and the bits it already
/// answered: `little`/`signed` contribute their own bit, `big`/`unsigned` are the
/// defaults (zero) and `native` is the **target** order (`$NativeEndian`), i.e.
/// `BSF_LITTLE` on this little-endian host and nothing on a big-endian one.
#[test]
fn a_non_empty_flags_literal_keeps_the_slow_path_and_its_bits() {
    let native = if cfg!(target_endian = "little") {
        BSF_LITTLE
    } else {
        0
    };
    let cases: &[(&[&str], u32)] = &[
        (&["little"], BSF_LITTLE),
        (&["signed"], BSF_SIGNED),
        (&["big"], 0),
        (&["unsigned"], 0),
        (&["little", "signed"], BSF_LITTLE | BSF_SIGNED),
        (&["native"], native),
    ];
    for (names, expected) in cases {
        let module = literal_module(&[flags_etf(names)]);
        let before = decode_flags_slow_calls();
        assert_eq!(
            decode_flags(&module, &Operand::Literal(0)).expect("a known flag list"),
            *expected,
            "{names:?}"
        );
        assert_eq!(
            decode_flags_slow_calls(),
            before + 1,
            "{names:?}: a real flag list still reaches the slow body"
        );
        // `native` must never survive as a distinct `BSF_NATIVE` on this route.
        assert_eq!(*expected & BSF_NATIVE, 0, "{names:?}");
    }
}

/// A flags literal that is **not** a proper list of flag atoms keeps exactly the
/// answer this route gave before the shell learned the empty-list arm — the shell's
/// new guard only fires on the NIL literal.
#[test]
fn a_flags_literal_that_is_not_a_proper_list_keeps_its_answer() {
    // A bare `SMALL_INTEGER_EXT` 5 …
    let not_a_list = vec![131u8, 97, 5];
    // … and an improper list: `flags_etf`'s trailing `NIL_EXT` replaced by a tail
    // atom, i.e. `[little | big]`.
    let mut improper = flags_etf(&["little"]);
    assert_eq!(improper.pop(), Some(106), "`flags_etf` ends with NIL_EXT");
    improper.extend_from_slice(&[100, 0, 3, b'b', b'i', b'g']); // ATOM_EXT "big"

    for payload in [not_a_list, improper] {
        let module = literal_module(&[payload]);
        let err = decode_flags(&module, &Operand::Literal(0))
            .expect_err("a non-list / improper-list flags literal is not readable");
        assert!(
            matches!(err, BeamError::BadArg),
            "a non-proper-list flags literal stays `BadArg`, got {err:?}"
        );
    }

    // An unknown flag atom inside an otherwise valid list is the family's own
    // diagnostic.
    let module = literal_module(&[flags_etf(&["sideways"])]);
    assert_eq!(
        reason_atom(
            decode_flags(&module, &Operand::Literal(0)).expect_err("an unknown flag atom")
        ),
        "unsupported_opcode: bitstring: unknown segment flag"
    );

    // A literal index the module does not have is **not** swallowed by the shell's
    // new arm: it still reaches the slow body's own out-of-range diagnostic.
    let empty = literal_module(&[]);
    assert_eq!(
        reason_atom(decode_flags(&empty, &Operand::Literal(0)).expect_err("no literal 0")),
        "unsupported_opcode: bitstring: flags literal index out of range"
    );
}// --- `string_source` (the `StrT` constant segment) ---------------------

/// The fixture's `StrT` table (`bsstring.beam`, from
/// `beam_lib:chunks(F, ["StrT"])`), used so the offsets are the oracle's.
const STRT: &[u8] = b"abhicdabczxyQ";

fn strt_module() -> Module {
    let mut module = Module::default();
    module.strings = STRT.to_vec();
    module
}

#[test]
fn string_source_slices_the_strt_table() {
    let module = strt_module();
    let machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let mut slice = |offset: i64, size: i64| {
        string_source(
            &module,
            &Operand::Integer(offset),
            &Operand::Integer(size),
            &machine,
            &mut ctx,
        )
    };
    assert_eq!(slice(0, 2).unwrap(), b"ab");
    assert_eq!(slice(4, 2).unwrap(), b"cd");
    assert_eq!(slice(9, 1).unwrap(), b"z");
    assert_eq!(slice(12, 1).unwrap(), b"Q");
    // A zero-length slice resolves to the empty slice, never an error.
    assert_eq!(slice(3, 0).unwrap(), b"");
}

#[test]
fn string_source_rejects_a_value_that_is_not_a_strt_offset() {
    let module = strt_module();
    let machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let size = Operand::Integer(2);
    for value in [Operand::Nil, Operand::Atom(module.name), Operand::X(0)] {
        let err = string_source(&module, &value, &size, &machine, &mut ctx).unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)), "{value:?}: got {err:?}");
    }
}

#[test]
fn string_source_rejects_an_all_size() {
    let mut module = strt_module();
    let all = Operand::Atom(module.atom_table.intern("all"));
    let machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let err =
        string_source(&module, &Operand::Integer(0), &all, &machine, &mut ctx).unwrap_err();
    assert!(matches!(err, BeamError::Raise(_)), "got {err:?}");
}

#[test]
fn string_source_rejects_a_negative_offset_and_a_bad_size() {
    let module = strt_module();
    let machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let err = string_source(
        &module,
        &Operand::Integer(-1),
        &Operand::Integer(1),
        &machine,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, BeamError::Raise(_)), "got {err:?}");
    // A negative size is not a byte count.
    let err = string_source(
        &module,
        &Operand::Integer(0),
        &Operand::Integer(-1),
        &machine,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, BeamError::Raise(_)), "got {err:?}");
}

#[test]
fn string_source_rejects_a_slice_outside_the_table() {
    let module = strt_module();
    let machine = Machine::new();
    let mut ctx = MinimalContext::new();
    // Offset 2 + size 2 runs past the end of a 3-byte table.
    let mut short = Module::default();
    short.strings = b"abc".to_vec();
    let err = string_source(
        &short,
        &Operand::Integer(2),
        &Operand::Integer(2),
        &machine,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, BeamError::Raise(_)), "got {err:?}");
    // A start past the end of the whole table is rejected too.
    let err = string_source(
        &module,
        &Operand::Integer(999),
        &Operand::Integer(1),
        &machine,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, BeamError::Raise(_)), "got {err:?}");
}

// --- `seg_nbits` (the `$BS_SAFE_MUL` + signed-fit check) ---------------

#[test]
fn seg_nbits_multiplies_and_flags_a_word_overflow() {
    assert_eq!(seg_nbits(8, 1), Ok(8));
    assert_eq!(seg_nbits(0, 4096), Ok(0));
    assert_eq!(seg_nbits(1 << 40, 8), Ok(1 << 43));
    assert_eq!(seg_nbits((1 << 63) - 1, 1), Ok((1 << 63) - 1));
    // A `size*unit` multiply that overflows a word (`:424`) ...
    assert_eq!(seg_nbits(usize::MAX, 2), Err(SegReason::SystemLimit));
    assert_eq!(seg_nbits(1 << 62, 4), Err(SegReason::SystemLimit));
    // ... and a product that fits a word but not a *signed* one, i.e. with bit
    // 63 set (`:425`-`:427`), are both `system_limit`.
    assert_eq!(seg_nbits(1 << 63, 1), Err(SegReason::SystemLimit));
    assert_eq!(seg_nbits(1 << 62, 2), Err(SegReason::SystemLimit));
}

// --- `float_nbits` (the `BSC_FLOAT` size arm + `erts_bs_put_float`) -----

#[test]
fn float_nbits_accepts_only_16_32_64() {
    // `erts_bs_put_float` (`erl_bits.c:1341`) accepts exactly these three widths;
    // `<<2.5:2/float-unit:8>>` is `16` (the size operand is 2, the unit 8), so the
    // unit multiply has to happen here:
    //   `erl -noshell -eval 'io:format("~w~n",[<<2.5:2/float-unit:8>>]),halt().'`
    //   ⇒ `<<65,0>>` (= binary16 0x4100).
    assert_eq!(float_nbits(16, 1), Ok(16));
    assert_eq!(float_nbits(32, 1), Ok(32));
    assert_eq!(float_nbits(64, 1), Ok(64));
    assert_eq!(float_nbits(2, 8), Ok(16));
    assert_eq!(float_nbits(4, 8), Ok(32));
    assert_eq!(float_nbits(8, 8), Ok(64));
}

#[test]
fn float_nbits_treats_a_zero_size_as_the_badarg_width_failure() {
    // `<<F:0/float>>` is emitted by `erlc` with only a "bad float bit size"
    // warning; the oracle rejects it at run time with the bare `badarg`:
    //   `erl -noshell -eval 'io:format("~p~n",[try <<1.0:0/float>> catch C:E -> {C,E} end]),halt().'`
    //   ⇒ `{error,badarg}`
    // The old `n.checked_mul(unit) { Some(m) if m != 0 => m, _ => 64 }` fallback
    // silently turned this into a 64-bit float instead — a wrong result.
    assert_eq!(float_nbits(0, 1), Err(SegReason::BadArg));
    assert_eq!(float_nbits(0, 8), Err(SegReason::BadArg));
    assert_eq!(float_nbits(0, usize::MAX), Err(SegReason::BadArg));
}

#[test]
fn float_nbits_rejects_every_width_other_than_16_32_64() {
    // `<<1.0:8/float>>` / `<<1.0:24/float>>` / `<<1.0:1000000/float>>` are all
    // `{error,badarg}` in the oracle (`erl_bits.c:1449`, `emu/bs_instrs.tab:664`),
    // e.g.
    //   `erl -noshell -eval 'io:format("~p~n",[try <<1.0:8/float>> catch C:E -> {C,E} end]),halt().'`
    //   ⇒ `{error,badarg}`
    for width in [1usize, 8, 15, 17, 24, 31, 33, 63, 65, 128, 1000000] {
        assert_eq!(
            float_nbits(width, 1),
            Err(SegReason::BadArg),
            "float_nbits({width}, 1)"
        );
    }
}

#[test]
fn float_nbits_reports_a_size_product_overflow_as_system_limit() {
    // The `$BS_SAFE_MUL` arm (`emu/bs_instrs.tab:424`) plus the signed-fit check
    // (`:425`) run **before** the width test, so an overflowing `size*unit` is
    // `system_limit` and not `badarg`:
    //   `erl -noshell -eval 'io:format("~p~n",[try <<1.0:(1 bsl 63)/float-unit:2>> catch C:E -> {C,E} end]),halt().'`
    //   ⇒ `{error,system_limit}`
    assert_eq!(float_nbits(usize::MAX, 2), Err(SegReason::SystemLimit));
    assert_eq!(float_nbits(1 << 62, 4), Err(SegReason::SystemLimit));
    assert_eq!(float_nbits(1 << 63, 1), Err(SegReason::SystemLimit));
    assert_eq!(float_nbits(1 << 62, 2), Err(SegReason::SystemLimit));
    // A 16/32/64 product is still accepted, so the overflow guard does not
    // swallow the valid widths.
    assert_eq!(float_nbits(16, 1), Ok(16));
}
// --- `segment_size` (the size-validation reasons) ----------------------

/// Read a dynamic size from `x(0)` holding `value`.
fn size_of(ctx: &mut MinimalContext, value: Eterm) -> SegSize {
    let mut machine = Machine::new();
    machine.set_x(0, value).expect("x0 in range");
    segment_size(&machine, ctx, &Operand::X(0)).expect("segment_size")
}

#[test]
fn segment_size_accepts_a_non_negative_small() {
    let mut ctx = MinimalContext::new();
    assert_eq!(size_of(&mut ctx, make_small(8)), SegSize::Small(8));
    assert_eq!(size_of(&mut ctx, make_small(0)), SegSize::Small(0));
}

#[test]
fn segment_size_rejects_a_negative_small_and_a_non_integer() {
    let mut ctx = MinimalContext::new();
    assert_eq!(
        size_of(&mut ctx, make_small(-1)),
        SegSize::Fail(SegReason::BadArg)
    );
    // `erl -noshell -eval 'io:format("~p~n",[try <<1:x/integer>> catch C:E -> {C,E} end]),halt().'`
    // ⇒ `{error,badarg}` (`emu/bs_instrs.tab:444`).
    assert_eq!(
        size_of(&mut ctx, make_atom(Atom::from_raw(1))),
        SegSize::Fail(SegReason::BadArg)
    );
}

#[test]
fn segment_size_distinguishes_positive_and_negative_bignums() {
    // Oracle, from a clean directory (no `-pa tests/fixtures/`):
    //   X = <<1:4>>, <<X:(1 bsl 59)/binary>>      ⇒ {error,system_limit}
    //   X = <<1:4>>, <<X:(-(1 bsl 59))/binary>>   ⇒ {error,badarg}
    // 2^59 = MAX_SMALL + 1, so both sizes are bignums.
    let mut ctx = MinimalContext::new();
    let positive = crate::terms::test_support::make_integer(&mut ctx, 1i64 << 59).expect("bignum");
    assert!(
        !beam_term::is_small(positive),
        "the fixture must exercise the bignum arm"
    );
    assert_eq!(
        size_of(&mut ctx, positive),
        SegSize::Fail(SegReason::SystemLimit)
    );
    let negative = crate::terms::test_support::make_integer(&mut ctx, -(1i64 << 59)).expect("bignum");
    assert_eq!(
        size_of(&mut ctx, negative),
        SegSize::Fail(SegReason::BadArg)
    );
}

// --- `raise_seg_fail` (the reason atom, branch vs raise) ---------------

/// A module whose atom table pins the reasons and the `error` class atom.
fn naming_module() -> Module {
    let mut module = Module::default();
    module.atom_table.intern("badarg");
    module.atom_table.intern("system_limit");
    module.atom_table.intern("error");
    module
}

#[test]
fn seg_reason_names_and_typed_errors_match_the_oracle_atoms() {
    assert_eq!(SegReason::BadArg.atom_name(), "badarg");
    assert_eq!(SegReason::SystemLimit.atom_name(), "system_limit");
    assert!(matches!(SegReason::BadArg.typed(), BeamError::BadArg));
    assert!(matches!(
        SegReason::SystemLimit.typed(),
        BeamError::SystemLimit
    ));
    assert_eq!(BeamError::SystemLimit.as_atom(), Some("system_limit"));
}

#[test]
fn raise_seg_fail_without_a_catch_frame_is_the_typed_error() {
    let mut machine = Machine::new();
    let module = naming_module();
    assert!(matches!(
        raise_seg_fail(&mut machine, &module, SegReason::BadArg).unwrap_err(),
        BeamError::BadArg
    ));
    assert!(matches!(
        raise_seg_fail(&mut machine, &module, SegReason::SystemLimit).unwrap_err(),
        BeamError::SystemLimit
    ));
}

#[test]
fn raise_seg_fail_lands_the_reason_atom_in_a_catch_frame() {
    let mut machine = Machine::new();
    machine.push_frame(0).expect("bottom frame");
    machine
        .push_catch_frame(crate::machine::CatchFrame::new(3, 7, 0));
    let module = naming_module();
    assert_eq!(
        raise_seg_fail(&mut machine, &module, SegReason::SystemLimit).expect("lands"),
        Step::Goto { func: 3, pc: 7 }
    );
    let reason = make_atom(module.atom_table.lookup("system_limit").expect("interned"));
    assert_eq!(machine.x(1), Some(reason));
}

#[test]
fn raise_seg_fail_never_substitutes_an_atom_the_module_cannot_name() {
    // `Module::default()` has an empty atom table: neither the reason atom nor
    // `error` can be named, so the raise must fall back to the typed error
    // rather than inventing a *different* atom.
    let mut machine = Machine::new();
    machine.push_frame(0).expect("bottom frame");
    machine
        .push_catch_frame(crate::machine::CatchFrame::new(3, 7, 0));
    let module = Module::default();
    assert!(matches!(
        raise_seg_fail(&mut machine, &module, SegReason::SystemLimit).unwrap_err(),
        BeamError::SystemLimit
    ));
}

// --- `put_float` -------------------------------------------------------
#[test]
fn put_float_is_big_endian_for_every_width() {
    assert_eq!(put_float(0.0, 16, false), vec![0x00, 0x00]);
    assert_eq!(put_float(-0.0, 16, false), vec![0x80, 0x00]);
    assert_eq!(put_float(1.0, 16, false), vec![0x3C, 0x00]);
    assert_eq!(put_float(1.5, 16, false), vec![0x3E, 0x00]);
    assert_eq!(put_float(-1.5, 16, false), vec![0xBE, 0x00]);
    assert_eq!(put_float(1.0, 32, false), vec![0x3F, 0x80, 0x00, 0x00]);
    assert_eq!(put_float(1.5, 32, false), vec![0x3F, 0xC0, 0x00, 0x00]);
    assert_eq!(
        put_float(1.5, 64, false),
        vec![0x3F, 0xF8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
    );
    assert_eq!(
        put_float(-1.0, 64, false),
        vec![0xBF, 0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
    );
}

#[test]
fn put_float_little_endian_reverses_the_bytes() {
    assert_eq!(put_float(-0.0, 16, true), vec![0x00, 0x80]);
    assert_eq!(put_float(1.5, 16, true), vec![0x00, 0x3E]);
    assert_eq!(put_float(1.5, 32, true), vec![0x00, 0x00, 0xC0, 0x3F]);
    assert_eq!(
        put_float(1.5, 64, true),
        vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF8, 0x3F]
    );
}

// --- `f64_to_f16` (`erl_bits.c:431`) -----------------------------------

#[test]
fn f64_to_f16_exact_values() {
    assert_eq!(f64_to_f16(0.0), 0x0000);
    assert_eq!(f64_to_f16(-0.0), 0x8000);
    assert_eq!(f64_to_f16(1.0), 0x3C00);
    assert_eq!(f64_to_f16(-1.0), 0xBC00);
    assert_eq!(f64_to_f16(1.5), 0x3E00);
    assert_eq!(f64_to_f16(-1.5), 0xBE00);
    assert_eq!(f64_to_f16(0.1), 0x2E66);
    assert_eq!(f64_to_f16(6.103515625e-5), 0x0400); // 2^-14, smallest normal
    assert_eq!(f64_to_f16(5.960464477539063e-8), 0x0001); // 2^-24, smallest subnormal
    assert_eq!(f64_to_f16(65504.0), 0x7BFF); // largest finite binary16
}

#[test]
fn f64_to_f16_overflow_becomes_infinity() {
    // Beyond the binary16 range the exponent field saturates.
    assert_eq!(f64_to_f16(65520.0), 0x7C00);
    assert_eq!(f64_to_f16(100000.0), 0x7C00);
    assert_eq!(f64_to_f16(-100000.0), 0xFC00);
    // The C body's overflow branch also catches a non-finite input; Erlang
    // floats are always finite, so this only documents port fidelity.
    assert_eq!(f64_to_f16(f64::INFINITY), 0x7C00);
    assert_eq!(f64_to_f16(f64::NEG_INFINITY), 0xFC00);
}

#[test]
fn f64_to_f16_rounds_to_nearest_even() {
    // 2^-25 is exactly between 0 and 2^-24; ties go to the even significand.
    assert_eq!(f64_to_f16(2.9802322387695312e-8), 0x0000);
    assert_eq!(f64_to_f16(-2.9802322387695312e-8), 0x8000);
    // 3*2^-25 is exactly between 2^-24 and 2^-23; ties go to 2^-23 (even).
    assert_eq!(f64_to_f16(8.940696716308594e-8), 0x0002);
    // Ties in the normal range: 1 + 2^-11 rounds down to 1.0 (even), while
    // 1 + 3*2^-11 rounds up to 1.0009765625 = 0x3C02 (even).
    assert_eq!(f64_to_f16(1.00048828125), 0x3C00);
    assert_eq!(f64_to_f16(1.00146484375), 0x3C02);
    // A subnormal double is far below the binary16 subnormal range.
    assert_eq!(f64_to_f16(5.0e-324), 0x0000);
    assert_eq!(f64_to_f16(-5.0e-324), 0x8000);
}

// --- `put_utf8` --------------------------------------------------------

#[test]
fn put_utf8_encodes_each_length() {
    assert_eq!(put_utf8(0), Some(vec![0x00]));
    assert_eq!(put_utf8(0x7F), Some(vec![0x7F]));
    assert_eq!(put_utf8(0x80), Some(vec![0xC2, 0x80]));
    assert_eq!(put_utf8(0x7FF), Some(vec![0xDF, 0xBF]));
    assert_eq!(put_utf8(0x800), Some(vec![0xE0, 0xA0, 0x80]));
    assert_eq!(put_utf8(0xFFFF), Some(vec![0xEF, 0xBF, 0xBF]));
    assert_eq!(put_utf8(0x10000), Some(vec![0xF0, 0x90, 0x80, 0x80]));
    assert_eq!(put_utf8(0x10FFFF), Some(vec![0xF4, 0x8F, 0xBF, 0xBF]));
}

#[test]
fn put_utf8_rejects_non_scalars() {
    assert_eq!(put_utf8(-1), None);
    assert_eq!(put_utf8(0xD7FF + 1), None); // first surrogate
    assert_eq!(put_utf8(0xDFFF), None); // last surrogate
    assert_eq!(put_utf8(0x110000), None);
}

// --- `put_utf16` -------------------------------------------------------

#[test]
fn put_utf16_encodes_bmp_and_surrogate_pairs() {
    assert_eq!(put_utf16(0, false), Some(vec![0x00, 0x00]));
    assert_eq!(put_utf16(0x7F, false), Some(vec![0x00, 0x7F]));
    assert_eq!(put_utf16(0xFFFF, false), Some(vec![0xFF, 0xFF]));
    assert_eq!(put_utf16(0x10000, false), Some(vec![0xD8, 0x00, 0xDC, 0x00]));
    assert_eq!(put_utf16(0x10FFFF, false), Some(vec![0xDB, 0xFF, 0xDF, 0xFF]));
}

#[test]
fn put_utf16_little_endian_swaps_each_code_unit() {
    assert_eq!(put_utf16(0, true), Some(vec![0x00, 0x00]));
    assert_eq!(put_utf16(0x7F, true), Some(vec![0x7F, 0x00]));
    assert_eq!(put_utf16(0x1234, true), Some(vec![0x34, 0x12]));
    assert_eq!(put_utf16(0x10000, true), Some(vec![0x00, 0xD8, 0x00, 0xDC]));
    assert_eq!(put_utf16(0x10FFFF, true), Some(vec![0xFF, 0xDB, 0xFF, 0xDF]));
}

#[test]
fn put_utf16_rejects_non_scalars() {
    assert_eq!(put_utf16(-1, false), None);
    assert_eq!(put_utf16(0xD800, false), None);
    assert_eq!(put_utf16(0xDFFF, false), None);
    assert_eq!(put_utf16(0x110000, false), None);
}

// --- `put_utf32` -------------------------------------------------------

#[test]
fn put_utf32_encodes_big_endian_fixed_width() {
    assert_eq!(put_utf32(0), Some(vec![0x00, 0x00, 0x00, 0x00]));
    assert_eq!(put_utf32(1), Some(vec![0x00, 0x00, 0x00, 0x01]));
    assert_eq!(put_utf32(0x10000), Some(vec![0x00, 0x01, 0x00, 0x00]));
    assert_eq!(put_utf32(0x10FFFF), Some(vec![0x00, 0x10, 0xFF, 0xFF]));
}

#[test]
fn put_utf32_rejects_non_scalars() {
    assert_eq!(put_utf32(-1), None);
    assert_eq!(put_utf32(0xD800), None);
    assert_eq!(put_utf32(0xDFFF), None);
    assert_eq!(put_utf32(0x110000), None);
}
