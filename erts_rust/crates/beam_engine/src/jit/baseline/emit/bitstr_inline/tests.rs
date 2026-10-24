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

//! The bitstring-inline unit suite: the flag/width rules, the shape predicates' refusals,
//! and tier agreement on real `erlc`-compiled fixtures.
//!
//! The emitted-byte and per-shape counter evidence lives one level up in
//! `jit::baseline::bitstr_inline_tests` (the crate's cache-free pins).

use beam_loader::Operand;

use super::*;
use crate::jit::baseline::bitstr_tests::{export, fixture, with_both_tiers_arg, Arg};
use crate::jit::baseline::tests::assert_same_outcome;
use crate::machine::RunOutcome;

/// The re-declared `BSF_*` values are the frozen C ones (`erts/emulator/beam/erl_bits.h:461-465`),
/// which is what makes [`field_is_little`] and the shape predicate faithful rather than a
/// second policy.
#[test]
fn the_flag_bits_match_the_frozen_c_constants() {
    assert_eq!(BSF_LITTLE, 2, "BSF_LITTLE");
    assert_eq!(BSF_SIGNED, 4, "BSF_SIGNED");
    assert_eq!(BSF_NATIVE, 16, "BSF_NATIVE");
    // `native` on this target is little-endian, so both bits ask for a byte-swapping read.
    assert!(field_is_little(BSF_LITTLE));
    assert!(field_is_little(BSF_NATIVE));
    assert!(!field_is_little(0));
    assert!(!field_is_little(8)); // BSF_EXACT
}

/// The compile-time predicate: only a constant, unsigned, big-endian field of a width the
/// extractor covers is lowered.
#[test]
fn only_a_constant_unsigned_big_endian_width_is_inlined() {
    let x = |i| Operand::X(i);
    let ok = |size: Operand, unit: u32, flags: u32| {
        GetInteger2Shape::of(&x(3), &size, unit, flags, &x(0)).is_some()
    };
    assert!(ok(Operand::Integer(8), 1, 0));
    assert!(ok(Operand::Integer(16), 1, 0));
    assert!(ok(Operand::Integer(16), 2, 0)); // <<X:16/unit:2>> -> 32 bits
    assert!(ok(Operand::Integer(4), 1, 0)); // granular
    assert!(ok(Operand::Integer(2), 4, 0)); // 8 bits via a non-unit width
    assert!(ok(Operand::Integer(0), 1, 0)); // the zero-bit field
    assert!(ok(Operand::Character(8), 1, 0)); // read_size's other accept arm
    // A runtime `size`, a width above 32 that is not 8/16/32, and > MAX_SMALL all refuse.
    assert!(!ok(x(1), 1, 0));
    assert!(!ok(Operand::Integer(24), 1, 0));
    assert!(!ok(Operand::Integer(64), 1, 0));
    assert!(!ok(Operand::Integer(1000), 1, 0));
    // Signed and little/native endianness are the family's general path.
    assert!(!ok(Operand::Integer(8), 1, BSF_SIGNED));
    assert!(!ok(Operand::Integer(8), 1, BSF_LITTLE));
    assert!(!ok(Operand::Integer(8), 1, BSF_NATIVE));
    // ...but `aligned`/`exact` change nothing about the value the family's reader produces.
    assert!(ok(Operand::Integer(8), 1, 1 | 8));
}

/// **Tier agreement on a real `erlc`-compiled shape.** `bitstrings:first8/1` is a committed
/// `bs_start_match3` + `bs_get_integer2` (`<<X:8, _/binary>>`-shaped): the baseline tier answers
/// the interpreter's own value and its own error, so the emitted arm never fabricates one.
///
/// The **emitted-arm** proof is not asserted here: `inlined()` counts the emitter's decision,
/// and the cross-run `CodeCache` is content-keyed and process-wide, so a suite that entered this
/// module on another tier/warmed it first serves the artifact and re-emits nothing (the counter
/// reads `0 -> 0`). A cache-free pin compiles through `emit::compile_function` — the stage-1
/// route (`call_tests.rs::the_frame_preserving_local_call_records_no_static_deopt`) and the
/// follow-up `bitstr_inline_tests` round.
#[test]
fn bs_get_integer2_is_inlined_and_agrees_with_the_interpreter() {
    let module = fixture("bitstrings");
    let atom = export(&module, "bitstrings", "first8");
    with_both_tiers_arg(
        &module,
        atom,
        Arg::Bytes(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]),
        |interpreter, baseline| {
            assert_same_outcome("bitstrings:first8/1", interpreter, baseline, &module.atom_table);
            let returned = baseline.as_ref().unwrap_or_else(|error| panic!("first8/1: {error:?}"));
            let RunOutcome::Returned(value) = returned else {
                panic!("first8/1: unexpected outcome {returned:?}");
            };
            assert_eq!(beam_term::signed_value(*value), 1, "first8/1 read the wrong byte");
        },
    );
}

/// **The overrun reaches the fail label.** `first8/1` on an empty subject has `end - start < 8`,
/// so the emitted `cmp`/`jb` takes the linker exit to the `fail` label and the function's own
/// failure clause reports exactly what the interpreter reports — no trap, no wrong class.
#[test]
fn an_overrun_field_takes_the_fail_label_and_agrees_with_the_interpreter() {
    let module = fixture("bitstrings");
    let atom = export(&module, "bitstrings", "first8");
    with_both_tiers_arg(&module, atom, Arg::Bytes(&[]), |interpreter, baseline| {
        assert_same_outcome(
            "first8/1 on an empty subject",
            interpreter,
            baseline,
            &module.atom_table,
        );
        assert!(baseline.is_err(), "an empty subject cannot satisfy <<X:8>>");
    });
}
