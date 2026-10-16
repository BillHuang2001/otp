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

//! Unit tests for the pure decoders the `bs_*` match opcodes call — their expected
//! values are the system `erl` oracle's answers for the same bit patterns — plus
//! [`empty_binary_bs_match_commands_and_the_writable_accumulator`], the
//! instruction-level regression for the **empty-binary match** path itself.
use super::super::BSF_LITTLE;
use super::*;

// The instruction-level regression drives whole instructions through the
// **family** dispatch ([`family_exec`], which routes to this module's [`exec`]
// and to `create::exec`), so it needs the machine host and the raw bitstring
// writers beside the decoders above.
use super::super::exec as family_exec;
use crate::context::MinimalContext;
use crate::families::bitstrings::raw;
use crate::interpreter::LabelMap;
use crate::machine::Machine;
use crate::terms::binary_bytes;
use crate::Step;
use beam_loader::{BsGetArgs, Instr, Label, Module, Operand, NO_FAIL};

// -- decode_utf8 (erts_bs_get_utf8) -------------------------------------
#[test]
fn utf8_decodes_each_sequence_length() {
    assert_eq!(decode_utf8(&[0x41], 0, 8), Some((0x41, 8)));
    assert_eq!(decode_utf8(&[0xC2, 0xA9], 0, 16), Some((0xA9, 16)));
    assert_eq!(decode_utf8(&[0xE2, 0x82, 0xAC], 0, 24), Some((0x20AC, 24)));
    assert_eq!(
        decode_utf8(&[0xF0, 0x9F, 0x98, 0x80], 0, 32),
        Some((0x1F600, 32))
    );
}
#[test]
fn utf8_accepts_the_range_boundaries() {
    // U+007F (1 byte), U+0080/U+07FF (2), U+0800/U+FFFF (3), U+10000/U+10FFFF (4).
    assert_eq!(decode_utf8(&[0x7F], 0, 8), Some((0x7F, 8)));
    assert_eq!(decode_utf8(&[0xC2, 0x80], 0, 16), Some((0x80, 16)));
    assert_eq!(decode_utf8(&[0xDF, 0xBF], 0, 16), Some((0x7FF, 16)));
    assert_eq!(decode_utf8(&[0xE0, 0xA0, 0x80], 0, 24), Some((0x800, 24)));
    assert_eq!(decode_utf8(&[0xEF, 0xBF, 0xBF], 0, 24), Some((0xFFFF, 24)));
    assert_eq!(
        decode_utf8(&[0xF0, 0x90, 0x80, 0x80], 0, 32),
        Some((0x10000, 32))
    );
    assert_eq!(
        decode_utf8(&[0xF4, 0x8F, 0xBF, 0xBF], 0, 32),
        Some((0x10FFFF, 32))
    );
}
#[test]
fn utf8_reads_at_a_non_byte_aligned_start() {
    // The decoder works at a *bit* offset (`read_uint_be`), so start need not be
    // byte-aligned: skip one byte of a 2-byte buffer.
    assert_eq!(decode_utf8(&[0x00, 0x41], 8, 16), Some((0x41, 8)));
}
#[test]
fn utf8_rejects_every_malformed_class() {
    assert_eq!(decode_utf8(&[], 0, 0), None); // bounded out
    assert_eq!(decode_utf8(&[0xC2], 0, 8), None); // truncated 2-byte
    assert_eq!(decode_utf8(&[0xE2, 0x82], 0, 16), None); // truncated 3-byte
    assert_eq!(decode_utf8(&[0x80], 0, 8), None); // continuation lead
    assert_eq!(decode_utf8(&[0xFF], 0, 8), None); // reserved lead
    assert_eq!(decode_utf8(&[0xE2, 0x28, 0xAC], 0, 24), None); // bad continuation
    assert_eq!(decode_utf8(&[0xC0, 0x80], 0, 16), None); // overlong U+0000
    assert_eq!(decode_utf8(&[0xC1, 0xBF], 0, 16), None); // overlong U+007F
    assert_eq!(decode_utf8(&[0xE0, 0x80, 0x80], 0, 24), None); // overlong U+0000
    assert_eq!(decode_utf8(&[0xF0, 0x80, 0x80, 0x80], 0, 32), None); // overlong
    assert_eq!(decode_utf8(&[0xED, 0xA0, 0x80], 0, 24), None); // surrogate D800
    assert_eq!(decode_utf8(&[0xED, 0xBF, 0xBF], 0, 24), None); // surrogate DFFF
    assert_eq!(decode_utf8(&[0xF4, 0x90, 0x80, 0x80], 0, 32), None); // >U+10FFFF
}
// -- decode_utf16 (erts_bs_get_utf16) -----------------------------------
#[test]
fn utf16_decodes_bmp_and_surrogate_pairs() {
    assert_eq!(decode_utf16(&[0x00, 0x41], 0, 16, 0), Some((0x41, 16)));
    assert_eq!(decode_utf16(&[0x20, 0xAC], 0, 16, 0), Some((0x20AC, 16)));
    // D7FF/E000 sit just outside the surrogate range and are plain scalars.
    assert_eq!(decode_utf16(&[0xD7, 0xFF], 0, 16, 0), Some((0xD7FF, 16)));
    assert_eq!(decode_utf16(&[0xE0, 0x00], 0, 16, 0), Some((0xE000, 16)));
    // D83D DE00 is U+1F600.
    assert_eq!(
        decode_utf16(&[0xD8, 0x3D, 0xDE, 0x00], 0, 32, 0),
        Some((0x1F600, 32))
    );
    // Little-endian words select `BSF_LITTLE`.
    assert_eq!(
        decode_utf16(&[0xAC, 0x20], 0, 16, BSF_LITTLE),
        Some((0x20AC, 16))
    );
    assert_eq!(
        decode_utf16(&[0x3D, 0xD8, 0x00, 0xDE], 0, 32, BSF_LITTLE),
        Some((0x1F600, 32))
    );
}
#[test]
fn utf16_rejects_lone_and_malformed_surrogates() {
    assert_eq!(decode_utf16(&[0xD8], 0, 8, 0), None); // bounded out
    assert_eq!(decode_utf16(&[0xD8, 0x3D], 0, 16, 0), None); // lone high, no low
    assert_eq!(decode_utf16(&[0xDE, 0x00], 0, 16, 0), None); // lone low surrogate
    assert_eq!(decode_utf16(&[0xD8, 0x3D, 0x00, 0x41], 0, 32, 0), None); // bad low
}
// -- decode_utf32 (i_bs_validate_unicode_retract) -----------------------
#[test]
fn utf32_decodes_valid_scalars_and_rejects_the_rest() {
    assert_eq!(
        decode_utf32(&[0x00, 0x00, 0x20, 0xAC], 0, 32, 0),
        Some((0x20AC, 32))
    );
    assert_eq!(
        decode_utf32(&[0x00, 0x10, 0xFF, 0xFF], 0, 32, 0),
        Some((0x10FFFF, 32))
    );
    assert_eq!(decode_utf32(&[0x00, 0x00, 0x00], 0, 24, 0), None); // bounded out
    assert_eq!(decode_utf32(&[0x00, 0x00, 0xD8, 0x00], 0, 32, 0), None); // surrogate
    assert_eq!(decode_utf32(&[0x00, 0x11, 0x00, 0x00], 0, 32, 0), None); // >U+10FFFF
}
// -- f16_to_f32 (erl_bits.c:508) ----------------------------------------
#[test]
fn f16_to_f32_widens_every_class() {
    assert_eq!(f16_to_f32(0x0000).to_bits(), 0x0000_0000); // +0
    assert_eq!(f16_to_f32(0x8000).to_bits(), 0x8000_0000); // -0
    assert_eq!(f16_to_f32(0x3C00).to_bits(), 0x3F80_0000); // 1.0
    assert_eq!(f16_to_f32(0x7C00).to_bits(), 0x7F80_0000); // +Inf
    assert_eq!(f16_to_f32(0xFC00).to_bits(), 0xFF80_0000); // -Inf
    assert_eq!(f16_to_f32(0x7E00).to_bits(), 0x7FC0_0000); // NaN
    assert_eq!(f16_to_f32(0x0001).to_bits(), 0x3380_0000); // min subnormal
    assert_eq!(f16_to_f32(0x03FF).to_bits(), 0x387F_C000); // max subnormal
    assert_eq!(f16_to_f32(0x0400).to_bits(), 0x3880_0000); // min normal (2^-14)
    assert_eq!(f16_to_f32(0x7BFF).to_bits(), 0x477F_E000); // 65504.0 (max)
}
// -- read_float_field (erts_bs_get_float_2) -----------------------------
#[test]
fn read_float_field_handles_every_valid_size_and_endianness() {
    assert_eq!(read_float_field(&[], 0, 0, 0).unwrap(), 0.0);
    assert_eq!(read_float_field(&[62, 0], 0, 16, 0).unwrap(), 1.5);
    assert_eq!(read_float_field(&[0, 62], 0, 16, BSF_LITTLE).unwrap(), 1.5);
    assert_eq!(read_float_field(&[63, 192, 0, 0], 0, 32, 0).unwrap(), 1.5);
    assert_eq!(
        read_float_field(&[0, 0, 192, 63], 0, 32, BSF_LITTLE).unwrap(),
        1.5
    );
    let pi = [64, 9, 33, 251, 84, 68, 45, 24];
    assert_eq!(
        read_float_field(&pi, 0, 64, 0).unwrap(),
        std::f64::consts::PI
    );
    let mut little = pi;
    little.reverse();
    assert_eq!(
        read_float_field(&little, 0, 64, BSF_LITTLE).unwrap(),
        std::f64::consts::PI
    );
}
#[test]
fn read_float_field_rejects_an_unsupported_size_and_an_out_of_range_field() {
    // Only 0/16/32/64 are valid; anything else is the typed `unsupported`.
    assert!(read_float_field(&[0, 0, 0, 0], 0, 8, 0).is_err());
    // Not an alignment rule — `read_uint_le_bits` accepts any bit offset; the
    // **range** check rejects this one (`4 + 32` bits exceed the 4-byte buffer).
    assert!(read_float_field(&[0, 0, 0, 0], 4, 32, BSF_LITTLE).is_err());
}
// -- ensure_at_least (the `bs_match` command) ---------------------------
#[test]
fn ensure_at_least_covers_every_documented_case() {
    assert!(ensure_at_least(8, 8, 1)); // >= Size
    assert!(!ensure_at_least(7, 8, 1));
    assert!(ensure_at_least(16, 8, 8)); // whole units after Size
    assert!(!ensure_at_least(15, 8, 8));
    assert!(ensure_at_least(16, 0, 8)); // byte alignment only
    assert!(!ensure_at_least(15, 0, 8));
    assert!(ensure_at_least(15, 0, 1)); // redundant no-op
    assert!(ensure_at_least(15, 0, 5)); // alignment to Unit
    assert!(!ensure_at_least(14, 0, 5));
}

// -- a match result above the 64-byte on-heap limit ---------------------
/// A 70-byte subject the two result-size tests below read their tail out of.
fn refc_subject(machine: &mut Machine, ctx: &mut MinimalContext) -> Vec<u8> {
    let payload: Vec<u8> = (1..=70u8).collect();
    let subject = raw::make_binary(machine, ctx, &payload, payload.len() * 8)
        .expect("a 70-byte refc subject");
    assert!(
        raw::is_refc_binary(subject),
        "70 bytes is above the 64-byte on-heap limit"
    );
    machine.set_x(0, subject).expect("root the subject");
    payload
}

/// `bs_get_tail` over a subject whose tail is **above** `ERL_ONHEAP_BINARY_LIMIT`
/// (560 bits = 70 bytes) returns the off-heap refc form.
///
/// The C `i_bs_get_tail` builds an arbitrary bit count through
/// `erts_build_sub_bitstring` (no 64-byte limit), so the engine routes the result
/// through `raw::make_binary`, which picks the frozen on-heap/off-heap
/// representation by size; before that it was the typed
/// `unsupported("bitstring: result binary > 64 bytes")`.
#[test]
fn bs_get_tail_above_the_heap_limit_builds_an_off_heap_refc_result() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let payload = refc_subject(&mut machine, &mut ctx);
    let module = Module::default();

    // `bs_start_match3 {f,0} {x,0} {x,1}`.
    let step = family_exec(
        &mut machine,
        &module,
        &LabelMap::new(),
        &Instr::BsStartMatch3 {
            fail: NO_FAIL,
            bin: Operand::X(0),
            live: 1,
            dst: Operand::X(1),
        },
        0,
        0,
        &mut ctx,
    )
    .expect("bs_start_match3 reports no raise")
    .expect("the family claims bs_start_match3");
    assert_eq!(step, Step::Next);
    // `bs_get_tail {x,1} {x,0}`.
    let step = family_exec(
        &mut machine,
        &module,
        &LabelMap::new(),
        &Instr::BsGetTail {
            ctx: Operand::X(1),
            dst: Operand::X(0),
            live: 2,
        },
        0,
        0,
        &mut ctx,
    )
    .expect("bs_get_tail reports no raise")
    .expect("the family claims bs_get_tail");
    assert_eq!(step, Step::Next);

    let result = machine.x(0).expect("the tail");
    // The representation the result must have taken (see `raw::make_binary`).
    assert!(
        raw::is_refc_binary(result),
        "a 560-bit (70-byte) result is above the 64-byte on-heap limit"
    );
    assert_eq!(
        raw::heap_bits(result).map(|(_, bits, _)| bits),
        Some(560),
        "the tail is the whole 70-byte payload"
    );
    assert_eq!(
        binary_bytes(result).expect("a byte-aligned result"),
        payload.as_slice(),
        "the off-heap result reads back bit-exactly"
    );
}

/// `bs_get_binary2 … Size=all` over the same 70-byte subject: the C
/// `erts_bs_get_binary_2` builds its window through `erts_build_sub_bitstring`
/// for an arbitrary size, so the engine's `raw::make_binary` gives the off-heap
/// refc form — never the `< 64`-byte refusal.
#[test]
fn bs_get_binary2_above_the_heap_limit_builds_an_off_heap_refc_result() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let payload = refc_subject(&mut machine, &mut ctx);

    let mut module = Module::default();
    let all = module.atom_table.intern("all");

    // `bs_start_match3 {f,0} {x,0} {x,1}`.
    let step = family_exec(
        &mut machine,
        &module,
        &LabelMap::new(),
        &Instr::BsStartMatch3 {
            fail: NO_FAIL,
            bin: Operand::X(0),
            live: 1,
            dst: Operand::X(1),
        },
        0,
        0,
        &mut ctx,
    )
    .expect("bs_start_match3 reports no raise")
    .expect("the family claims bs_start_match3");
    assert_eq!(step, Step::Next);
    // `bs_get_binary2 {f,0} {x,1} 2 {atom,all} 8 {x,0}`.
    let step = family_exec(
        &mut machine,
        &module,
        &LabelMap::new(),
        &Instr::BsGetBinary2(Box::new(BsGetArgs {
            fail: NO_FAIL,
            ctx: Operand::X(1),
            live: 2,
            size: Operand::Atom(all),
            unit: 8,
            flags: 0,
            dst: Operand::X(0),
        })),
        0,
        0,
        &mut ctx,
    )
    .expect("bs_get_binary2 reports no raise")
    .expect("the family claims bs_get_binary2");
    assert_eq!(step, Step::Next);

    let result = machine.x(0).expect("the matched window");
    assert!(
        raw::is_refc_binary(result),
        "a 560-bit (70-byte) `all` window is above the 64-byte on-heap limit"
    );
    assert_eq!(
        raw::heap_bits(result).map(|(_, bits, _)| bits),
        Some(560),
        "`all` reads the whole 70-byte payload"
    );
    assert_eq!(
        binary_bytes(result).expect("a byte-aligned result"),
        payload.as_slice(),
        "the off-heap result reads back bit-exactly"
    );
}

// -- the empty-binary match (the `base64:encode(<<>>)` shape) -----------
/// The **empty-binary match**: the two `bs_match` command lists the installed
/// `erlc` emits for `base64:encode_binary/4`'s first and empty clauses, driven as
/// whole instructions over a 0-bit subject, plus the 0-bit `bs_init_writable`
/// accumulator the empty clause returns.
///
/// `base64:encode/2` opens its accumulator with `move {integer,256} -> x(0)` +
/// `bs_init_writable` and tail-calls `encode_binary/4` with that accumulator in
/// `x(3)`; `beam_disasm:file/1` shows `encode_binary/4` (base64.erl:218) as
///
/// ```text
/// {label,19},
/// {bs_start_match4,{atom,no_fail},4,{x,2},{x,2}},
/// {bs_get_position,{x,2},{x,4},4},
/// {bs_match,{f,20},{x,2},
///     {commands,[{ensure_at_least,48,1},
///                {integer,4,{literal,[]},6,1,{x,4}},
///                … {integer,11,{literal,[]},6,1,{x,11}}]}},
/// …
/// {label,20},                                        % <fail>: the empty clause
/// {bs_set_position,{x,2},{x,4}},
/// {bs_match,{f,21},{x,2},{commands,[{ensure_exactly,0}]}},
/// {move,{x,3},{x,0}},
/// return
/// ```
///
/// The loop body appends with a `bs_create_bin` whose first segment is
/// `{atom,private_append},1,1,nil,{tr,{x,3},{t_bs_matchable,1}},{atom,all}` (the
/// accumulator in `x(3)` again).
///
/// On `<<>>` the first `bs_match` must take the fail label (that is the first
/// clause refusing an empty subject) and the second must fall through — which is
/// what makes the accumulator `bs_init_writable` built the value returned. The
/// oracle prints that accumulator as `<<>>`:
///
/// ```text
/// $ erl -noshell -eval 'io:format("~p~n",[base64:encode(<<>>)]),halt().'
/// <<>>
/// ```
///
/// The end-to-end counterpart (the real installed `base64:encode/1` over the whole
/// sweep host) lives in `tests/stdlib_sweep.rs`.
#[test]
fn empty_binary_bs_match_commands_and_the_writable_accumulator() {
    let mut machine = Machine::with_max_register(4);
    let mut ctx = MinimalContext::new();

    // The command/segment atoms the family reads from the **running module** (a
    // real module's atom table carries them; a synthetic one must intern them).
    let mut module = Module::default();
    for name in [
        "no_fail",
        "ensure_exactly",
        "ensure_at_least",
        "private_append",
        "all",
    ] {
        module.atom_table.intern(name);
    }
    let atom = |module: &Module, name: &str| {
        Operand::Atom(
            module
                .atom_table
                .lookup(name)
                .unwrap_or_else(|| panic!("atom {name:?} interned above")),
        )
    };

    // The empty subject `<<>>` `encode_binary/4` is called on: a 0-bit on-heap
    // binary, exactly what `terms::make_heap_binary(ctx, &[])` builds.
    let subject = raw::make_bits(&mut machine, &mut ctx, &[], 0).expect("a 0-bit binary");
    machine.set_x(0, subject).expect("root the subject");

    // `bs_start_match4 {atom,no_fail},4,{x,0},{x,1}` — the match entry. Nothing can
    // fail here (`no_fail`), and `<<>>` still enters a (0..0) match context.
    let step = family_exec(
        &mut machine,
        &module,
        &LabelMap::new(),
        &Instr::BsStartMatch4 {
            fail: atom(&module, "no_fail"),
            live: 4,
            src: Operand::X(0),
            dst: Operand::X(1),
        },
        0,
        0,
        &mut ctx,
    )
    .expect("bs_start_match4 reports no raise")
    .expect("the family claims bs_start_match4");
    assert_eq!(step, Step::Next, "`<<>>` enters a match with `no_fail`");

    // The live fail label both `bs_match`es branch to (the empty clause's entry).
    const FAIL: Label = Label(1);
    let mut labels = LabelMap::new();
    labels.insert(FAIL, (0, 7));

    // (1) The empty clause: `{commands,[{ensure_exactly,0}]}` MATCHES `<<>>` — the
    // engine reports `Next` (the instruction after `bs_match`), never the fail
    // label. (Both clauses share the context at `x(1)`; `ensure_exactly` only
    // tests, so it does not advance it.)
    let step = family_exec(
        &mut machine,
        &module,
        &labels,
        &Instr::BsMatch {
            fail: FAIL,
            ctx: Operand::X(1),
            commands: vec![atom(&module, "ensure_exactly"), Operand::Integer(0)],
        },
        0,
        0,
        &mut ctx,
    )
    .expect("bs_match reports no raise")
    .expect("the family claims bs_match");
    assert_eq!(step, Step::Next, "`ensure_exactly 0` matches `<<>>`");

    // (2) The first clause: `{commands,[{ensure_at_least,48,1}]}` FAILS on `<<>>`
    // (there are 0 of the 48 required bits), so the engine branches to the live
    // fail label instead of falling through.
    let step = family_exec(
        &mut machine,
        &module,
        &labels,
        &Instr::BsMatch {
            fail: FAIL,
            ctx: Operand::X(1),
            commands: vec![
                atom(&module, "ensure_at_least"),
                Operand::Integer(48),
                Operand::Integer(1),
            ],
        },
        0,
        0,
        &mut ctx,
    )
    .expect("bs_match reports no raise")
    .expect("the family claims bs_match");
    assert_eq!(
        step,
        Step::Goto { func: 0, pc: 7 },
        "`ensure_at_least 48` refuses `<<>>` and takes the fail label"
    );

    // (3) The accumulator the empty clause returns: `move {integer,256} -> x(0)`
    // followed by `bs_init_writable` (the C `erts_bs_init_writable` keeps the
    // capacity hint in `x(0)` and reports a **zero-length** window).
    machine
        .set_x(0, beam_term::make_small(256))
        .expect("the capacity hint");
    let step = family_exec(
        &mut machine,
        &module,
        &labels,
        &Instr::BsInitWritable,
        0,
        0,
        &mut ctx,
    )
    .expect("bs_init_writable reports no raise")
    .expect("the family claims bs_init_writable");
    assert_eq!(step, Step::Next);
    let accumulator = machine.x(0).expect("the accumulator");
    assert_eq!(
        raw::heap_bits(accumulator).map(|(_, bits, _)| bits),
        Some(0),
        "`bs_init_writable` returns a 0-bit bitstring"
    );
    assert_eq!(
        binary_bytes(accumulator).expect("a byte binary"),
        &[] as &[u8],
        "the accumulator the empty clause returns reads as `<<>>`"
    );

    // `call_last 4 {base64,encode_binary,4}` hands that accumulator to the next
    // clause in `x(3)` (the C argument-register convention), which is where the
    // loop body's `bs_create_bin` keeps it.
    machine.set_x(3, accumulator).expect("the accumulator in x(3)");

    // The loop body's `{atom,private_append},1,1,nil,{x,3},{atom,all}` segment
    // seeded from that accumulator appends nothing, so the built result is still
    // `<<>>` and the frozen readers accept it. (`alloc`/`live`/`unit` are the
    // operands `base64.beam` emits; the private_append arm reads neither.)
    let step = family_exec(
        &mut machine,
        &module,
        &labels,
        &Instr::BsCreateBin {
            fail: NO_FAIL,
            alloc: 0,
            live: 12,
            unit: 32,
            dst: Operand::X(3),
            segments: vec![
                atom(&module, "private_append"),
                Operand::Integer(1), // Index (a compiler-side identifier)
                Operand::Integer(1), // Unit
                Operand::Nil,        // Flags (no little/signed/native)
                Operand::X(3),       // the `bs_init_writable` accumulator
                atom(&module, "all"), // Size
            ],
        },
        0,
        0,
        &mut ctx,
    )
    .expect("bs_create_bin reports no raise")
    .expect("the family claims bs_create_bin");
    assert_eq!(step, Step::Next);
    let built = machine.x(3).expect("the built binary");
    assert_eq!(
        raw::heap_bits(built).map(|(_, bits, _)| bits),
        Some(0),
        "a `private_append` of nothing onto `<<>>` is `<<>>`"
    );
    assert_eq!(
        binary_bytes(built).expect("a byte binary"),
        &[] as &[u8],
        "the built result reads as `<<>>`"
    );
}

// -- bs_match_string: the `StrT` offset guard ---------------------------
/// The `fail` label every `bs_match_string` test below branches to (a live
/// label, so a plain match failure is distinguishable from the typed error).
const FAIL: Label = Label(7);

/// Build a 2-byte subject (`<<16#AB,16#CD>>`) in `x(0)`, start a match over it
/// into `x(1)`, and a module whose `StrT` table is `"abc"`. Returns the
/// `(machine, module, labels, ctx)` the three `bs_match_string` tests drive.
#[allow(clippy::type_complexity)]
fn match_string_fixture() -> (Machine, Module, LabelMap, MinimalContext) {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let mut module = Module::default();
    module.strings = b"abc".to_vec();
    let subject =
        raw::make_bits(&mut machine, &mut ctx, &[0xAB, 0xCD], 16).expect("a 2-byte subject");
    machine.set_x(0, subject).expect("root the subject");
    let step = family_exec(
        &mut machine,
        &module,
        &LabelMap::new(),
        &Instr::BsStartMatch3 {
            fail: NO_FAIL,
            bin: Operand::X(0),
            live: 1,
            dst: Operand::X(1),
        },
        0,
        0,
        &mut ctx,
    )
    .expect("bs_start_match3 reports no raise")
    .expect("the family claims bs_start_match3");
    assert_eq!(step, Step::Next);
    let mut labels = LabelMap::new();
    labels.insert(FAIL, (0, 9));
    (machine, module, labels, ctx)
}

/// Drive one `bs_match_string` over the fixture's context.
fn match_string(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    ctx: &mut MinimalContext,
    bits: u32,
    offset: u32,
) -> beam_common::Result<Option<Step>> {
    family_exec(
        machine,
        module,
        labels,
        &Instr::BsMatchString {
            fail: FAIL,
            ctx: Operand::X(1),
            bits,
            offset,
        },
        0,
        0,
        ctx,
    )
}

/// `StrT` offsets the module does not have are the typed error, never a silent
/// degradation to an empty pattern (which would merely fail the match).
///
/// The C `i_bs_match_string` reads an already-resolved absolute pointer into a
/// copy of the whole `StrT` chunk, so an out-of-range offset is undefined
/// behaviour there with no catchable answer; the engine's guard is therefore a
/// deliberate, stricter-than-C typed error.
#[test]
fn bs_match_string_rejects_a_str_table_offset_past_the_end() {
    let (mut machine, module, labels, mut ctx) = match_string_fixture();
    // `strings` is 3 bytes; offset 10 is strictly beyond the end.
    let err = match_string(&mut machine, &module, &labels, &mut ctx, 8, 10)
        .expect_err("an out-of-range StrT offset is the typed error");
    match err {
        beam_common::BeamError::Raise(raise) => assert_eq!(
            raise.reason.as_atom(),
            Some("unsupported_opcode: bs_match_string: string table offset out of range"),
        ),
        other => panic!("expected the typed unsupported error, got {other:?}"),
    }
}

/// An offset **exactly at** the end of `StrT` is a legitimately empty pattern:
/// a 0-bit `bs_match_string` matches empty input and advances nothing.
#[test]
fn bs_match_string_accepts_an_offset_exactly_at_the_end_as_an_empty_pattern() {
    let (mut machine, module, labels, mut ctx) = match_string_fixture();
    assert_eq!(module.strings.len(), 3, "the fixture's StrT table");
    let step = match_string(&mut machine, &module, &labels, &mut ctx, 0, 3)
        .expect("an at-end offset is not an error")
        .expect("the family claims bs_match_string");
    assert_eq!(
        step,
        Step::Next,
        "an empty pattern at the end of StrT matches empty input"
    );
    // The context stayed put (0 bits consumed) and is still readable.
    let (_, _, start, _) = read_ctx(&machine, &mut ctx, &Operand::X(1), "test").expect("ctx");
    assert_eq!(start, 0);
}

/// A genuinely in-range offset still matches bit-for-bit, and a mismatch is a
/// plain match failure (the fail label), not an error.
#[test]
fn bs_match_string_matches_an_in_range_pattern_and_fails_a_mismatch() {
    let (mut machine, module, labels, mut ctx) = match_string_fixture();
    // `StrT` is `"abc"`; offset 00 with bits=0xAB00... no: compare the subject's
    // first 8 bits (`0xAB`) against offset 0 (`0x61` = `a`) — a mismatch, which
    // must take the fail label rather than raise.
    let step = match_string(&mut machine, &module, &labels, &mut ctx, 8, 0)
        .expect("a mismatch is not an error")
        .expect("the family claims bs_match_string");
    assert_eq!(step, Step::Goto { func: 0, pc: 9 }, "`0xAB` != `a`");

    // Offset 0 with bits=0 is the empty pattern again: it matches, and because
    // `bits` is 0 nothing is compared or consumed.
    let step = match_string(&mut machine, &module, &labels, &mut ctx, 0, 0)
        .expect("the empty pattern is not an error")
        .expect("the family claims bs_match_string");
    assert_eq!(step, Step::Next);
}

/// A **below-range** offset (already `Some`) keeps working: an in-range string
/// that *does* match the subject succeeds and advances the position.
#[test]
fn bs_match_string_matches_an_in_range_string_that_does_match() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let mut module = Module::default();
    module.strings = b"abc".to_vec();
    let subject =
        raw::make_bits(&mut machine, &mut ctx, &[0x61, 0x62], 16).expect("a 2-byte subject");
    machine.set_x(0, subject).expect("root the subject");
    let mut labels = LabelMap::new();
    labels.insert(FAIL, (0, 9));
    let step = family_exec(
        &mut machine,
        &module,
        &labels,
        &Instr::BsStartMatch3 {
            fail: NO_FAIL,
            bin: Operand::X(0),
            live: 1,
            dst: Operand::X(1),
        },
        0,
        0,
        &mut ctx,
    )
    .expect("bs_start_match3 reports no raise")
    .expect("the family claims bs_start_match3");
    assert_eq!(step, Step::Next);
    let step = match_string(&mut machine, &module, &labels, &mut ctx, 16, 0)
        .expect("an in-range match is not an error")
        .expect("the family claims bs_match_string");
    assert_eq!(step, Step::Next, "`\"ab\"` matches the subject");
}
