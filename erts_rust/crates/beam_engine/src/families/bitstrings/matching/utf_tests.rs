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

//! **Instruction-arm regressions for the UTF match reads' endianness flags.**
//!
//! `bs_get_utf32`/`bs_skip_utf32` are rewritten by `emu/ops.tab:1296-1307` into
//! `bs_get_integer2 … 32 1 Flags` + `i_bs_validate_unicode_retract`, so their `Flags`
//! operand is a **raw `u32` bitmask** (the loader decodes it with `as_u32`, and the
//! compiler's `beam_asm:flag_to_bit/1` encodes `little` as `16#02` and `native` as
//! `16#10`) — both bits select the byte order, exactly as in C. These tests drive the
//! instructions through the family's own [`exec`](super::exec) over a hand-built heap
//! subject and pin the system `erl` oracle's answers for the same bit patterns.
//!
//! `BsSkipUtf32` must advance by the same number of bits `BsGetUtf32` consumes; a
//! **failed** read must leave the context where it was (the half-open restore
//! pattern).

use beam_common::{BeamError, Result as BResult};
use beam_loader::{Instr, Module, Operand, NO_FAIL};

use crate::context::MinimalContext;
use crate::families::bitstrings::{exec as family_exec, raw, BSF_LITTLE, BSF_NATIVE};
use crate::interpreter::LabelMap;
use crate::machine::Machine;
use crate::Step;

/// A module whose atom table names `badmatch`, the tag the `fail == NO_FAIL` raise
/// path (`fail_step` → `badmatch_reason_rooted`) resolves from the running module.
fn module() -> Module {
    let mut module = Module::default();
    module.atom_table.intern("badmatch");
    module
}

/// Enter a match context over the byte-aligned subject `bytes` (rooted at `x(1)`),
/// with the subject itself rooted at `x(0)` — the compiler's own match preamble.
fn enter(module: &Module, bytes: &[u8]) -> (Machine, MinimalContext) {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let subject = raw::make_binary(&mut machine, &mut ctx, bytes, bytes.len() * 8)
        .expect("a heap-binary subject");
    machine.set_x(0, subject).expect("root the subject");
    let step = family_exec(
        &mut machine,
        module,
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
    (machine, ctx)
}

/// The match context's current bit position (the context is rooted at `x(1)`).
fn position(machine: &Machine) -> usize {
    raw::ctx_parts(machine.x(1).expect("the context root"))
        .expect("a match context")
        .1
}

/// Drive one `bs_get_utf32` over `bytes` with the raw `flags` mask, returning the
/// matched scalar (`Err` when the read failed the match) and the context's position
/// afterwards.
fn get_utf32(bytes: &[u8], flags: u32) -> (BResult<i64>, usize) {
    let module = module();
    let (mut machine, mut ctx) = enter(&module, bytes);
    let result = family_exec(
        &mut machine,
        &module,
        &LabelMap::new(),
        &Instr::BsGetUtf32 {
            fail: NO_FAIL,
            ctx: Operand::X(1),
            live: 2,
            flags,
            dst: Operand::X(0),
        },
        0,
        0,
        &mut ctx,
    )
    .map(|step| {
        assert_eq!(step, Some(Step::Next), "bs_get_utf32 is claimed");
        beam_term::signed_value(machine.x(0).expect("the matched scalar"))
    });
    (result, position(&machine))
}

/// Drive one `bs_skip_utf32` over `bytes` with the raw `flags` mask, returning
/// `Ok(())`/the raise and the context's position afterwards.
fn skip_utf32(bytes: &[u8], flags: u32) -> (BResult<()>, usize) {
    let module = module();
    let (mut machine, mut ctx) = enter(&module, bytes);
    let result = family_exec(
        &mut machine,
        &module,
        &LabelMap::new(),
        &Instr::BsSkipUtf32 {
            fail: NO_FAIL,
            ctx: Operand::X(1),
            live: 2,
            flags,
        },
        0,
        0,
        &mut ctx,
    )
    .map(|step| {
        assert_eq!(step, Some(Step::Next), "bs_skip_utf32 is claimed");
    });
    (result, position(&machine))
}

/// A match failure is the `{badmatch, Subject}` raise `fail_step` builds, never a
/// capability error and never a wrong scalar.
fn assert_match_failure(result: &BResult<i64>) {
    match result {
        Err(err) => assert!(
            matches!(err, BeamError::Raise(_)),
            "a failed utf32 read raises; got {err:?}"
        ),
        Ok(value) => panic!("expected a match failure, read {value}"),
    }
}

// ---------------------------------------------------------------------------
// BsGetUtf32 / BsSkipUtf32
// ---------------------------------------------------------------------------

/// No flags is the big-endian default: `<<0,0,32,172>>` is U+20AC (€).
/// Oracle: `<<X/utf32>> = <<0,0,32,172>>` ⇒ `8364`.
#[test]
fn utf32_defaults_to_big_endian() {
    let (value, pos) = get_utf32(&[0, 0, 32, 172], 0);
    assert_eq!(value.expect("a valid scalar"), 0x20AC);
    assert_eq!(pos, 32, "the read consumes 32 bits");
}

/// `-little` (`BSF_LITTLE`) selects the little-endian byte order.
/// Oracle: `<<X/utf32-little>> = <<172,32,0,0>>` ⇒ `8364`, while the mirrored
/// big-endian subject FAILS the match.
#[test]
fn utf32_little_flag_selects_little_endian() {
    let (value, pos) = get_utf32(&[172, 32, 0, 0], BSF_LITTLE);
    assert_eq!(value.expect("a valid little-endian scalar"), 0x20AC);
    assert_eq!(pos, 32, "the read consumes 32 bits");

    let (big, pos) = get_utf32(&[0, 0, 32, 172], BSF_LITTLE);
    assert_match_failure(&big);
    assert_eq!(pos, 0, "a failed read must not advance the context");
}

/// `-native` (`BSF_NATIVE`) means "the target's own order", so on a little-endian
/// target it reads exactly like `-little` and on a big-endian target like the big
/// default. Oracle (little-endian host): `<<X/utf32-native>> = <<172,32,0,0>>` ⇒
/// `8364`, and the mirrored subject FAILS.
#[test]
fn utf32_native_flag_follows_the_target_byte_order() {
    if cfg!(target_endian = "little") {
        // Oracle (little-endian host): `<<X/utf32-native>> = <<172,32,0,0>>` ⇒ `8364`.
        let (value, pos) = get_utf32(&[172, 32, 0, 0], BSF_NATIVE);
        assert_eq!(value.expect("a valid native scalar"), 0x20AC);
        assert_eq!(pos, 32, "the read consumes 32 bits");
        // `native` and `little` read the same bytes the same way on this target.
        let (little, _) = get_utf32(&[172, 32, 0, 0], BSF_LITTLE);
        assert_eq!(little.expect("a valid little-endian scalar"), 0x20AC);
        // …and the mirrored subject fails for both.
        let (mirrored, _) = get_utf32(&[0, 0, 32, 172], BSF_NATIVE);
        assert_match_failure(&mirrored);
    } else {
        let (value, pos) = get_utf32(&[0, 0, 32, 172], BSF_NATIVE);
        assert_eq!(value.expect("a valid native scalar"), 0x20AC);
        assert_eq!(pos, 32, "the read consumes 32 bits");
    }
}

/// Surrogates and scalars above U+10FFFF are a plain **match failure** in either
/// byte order (never an error and never a wrong scalar). Oracle:
/// `<<X/utf32>> = <<0,0,216,0>>` ⇒ badmatch; `<<X/utf32-little>> = <<0,216,0,0>>`
/// ⇒ badmatch; `<<X/utf32>> = <<0,17,0,0>>` ⇒ badmatch;
/// `<<X/utf32-little>> = <<0,0,17,0>>` ⇒ badmatch.
#[test]
fn utf32_rejects_surrogates_and_out_of_range_scalars_in_both_byte_orders() {
    // D800 big- and little-endian.
    let (surrogate_big, pos) = get_utf32(&[0, 0, 0xD8, 0], 0);
    assert_match_failure(&surrogate_big);
    assert_eq!(pos, 0);
    let (surrogate_little, pos) = get_utf32(&[0, 0xD8, 0, 0], BSF_LITTLE);
    assert_match_failure(&surrogate_little);
    assert_eq!(pos, 0);
    // 0x00110000 (> U+10FFFF) big- and little-endian.
    let (over_big, _) = get_utf32(&[0, 0x11, 0, 0], 0);
    assert_match_failure(&over_big);
    let (over_little, _) = get_utf32(&[0, 0, 0x11, 0], BSF_LITTLE);
    assert_match_failure(&over_little);
    // The exact valid boundaries decode in both orders (U+10FFFF, U+0000).
    let (max_big, _) = get_utf32(&[0x00, 0x10, 0xFF, 0xFF], 0);
    assert_eq!(max_big.expect("U+10FFFF big-endian"), 0x0010_FFFF);
    let (max_little, _) = get_utf32(&[0xFF, 0xFF, 0x10, 0x00], BSF_LITTLE);
    assert_eq!(max_little.expect("U+10FFFF little-endian"), 0x0010_FFFF);
}

/// A subject shorter than the 32 bits a UTF-32 scalar needs fails the match in both
/// byte orders. Oracle: `<<X/utf32>> = <<0,0,32>>` ⇒ badmatch;
/// `<<X/utf32-little>> = <<172,32,0>>` ⇒ badmatch.
#[test]
fn utf32_fails_on_a_subject_shorter_than_32_bits() {
    let (short_big, pos) = get_utf32(&[0, 0, 32], 0);
    assert_match_failure(&short_big);
    assert_eq!(pos, 0, "a failed read must not advance the context");
    let (short_little, pos) = get_utf32(&[172, 32, 0], BSF_LITTLE);
    assert_match_failure(&short_little);
    assert_eq!(pos, 0);
}

/// `bs_skip_utf32` advances by the 32 bits `bs_get_utf32` would consume, in both byte
/// orders. Oracle: `<<_/utf32-little, R/binary>> = <<172,32,0,0,9>>` ⇒ `R = <<9>>`;
/// the same is true big-endian over `<<0,0,32,172,9>>`, while the big-endian read of
/// the little-endian subject FAILS.
#[test]
fn skip_utf32_advances_32_bits_in_both_byte_orders() {
    let (skipped, pos) = skip_utf32(&[172, 32, 0, 0, 9], BSF_LITTLE);
    assert!(skipped.is_ok(), "a valid little-endian scalar skips");
    assert_eq!(pos, 32, "the skip consumes exactly 32 bits");

    let (skipped, pos) = skip_utf32(&[0, 0, 32, 172, 9], 0);
    assert!(skipped.is_ok(), "a valid big-endian scalar skips");
    assert_eq!(pos, 32);

    let module = module();
    let (mut machine, mut ctx) = enter(&module, &[172, 32, 0, 0, 9]);
    let err = family_exec(
        &mut machine,
        &module,
        &LabelMap::new(),
        &Instr::BsSkipUtf32 {
            fail: NO_FAIL,
            ctx: Operand::X(1),
            live: 2,
            flags: 0,
        },
        0,
        0,
        &mut ctx,
    )
    .expect_err("the big-endian read of the little-endian subject fails the match");
    assert!(matches!(err, BeamError::Raise(_)));
    assert_eq!(position(&machine), 0, "a failed skip must not advance");
}
