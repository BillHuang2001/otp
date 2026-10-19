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

//! The **whole-byte `integer` accumulator** fast path: what it serves, what it declines,
//! and that the bits it writes are the generic walk's bits.
//!
//! Every expected byte vector here is either the **copy-rebuild** route's own output (the
//! reference the in-place arm must stay identical to) or a value taken from the live
//! oracle (system OTP 29.1.1 / erts-17.1) — never an invented one. The last case compiles
//! a real accumulator loop with the system `erlc` and asserts the fast path built it.
//!
//! [`fast_builds`] is the thread-local tally these cases use to tell the two routes apart;
//! it is a `#[cfg(test)]` instrument with no production call site.

use std::path::PathBuf;
use std::process::Command;

use beam_common::BeamError;
use beam_loader::{load_file, Label, Module, Operand, NO_FAIL};
use beam_term::Eterm;

use crate::context::{ExecutionContext, MinimalContext};
use crate::families::bitstrings::{raw, Built};
use crate::interpreter::LabelMap;
use crate::machine::Machine;
use crate::terms::{binary_bytes, make_heap_binary};
use crate::Step;

use super::super::{assemble, built_bytes};
use super::{fast_builds, plan};

// ---------------------------------------------------------------------------
// Harness: segment-group builders
// ---------------------------------------------------------------------------

/// A module naming every atom these cases use.
fn test_module() -> Module {
    let mut module = Module::default();
    for name in ["private_append", "append", "integer", "binary", "all"] {
        module.atom_table.intern(name);
    }
    module
}

/// The `Operand::Atom` for `name` in `module`'s atom table.
fn atom(module: &Module, name: &str) -> Operand {
    Operand::Atom(
        module
            .atom_table
            .lookup(name)
            .unwrap_or_else(|| panic!("the test module interns {name:?}")),
    )
}

/// One flat segment group `[Type, Index, Unit, Flags, Value, Size]`.
fn group(
    module: &Module,
    kind: &str,
    index: i64,
    unit: Operand,
    flags: Operand,
    value: Operand,
    size: Operand,
) -> Vec<Operand> {
    vec![atom(module, kind), Operand::Integer(index), unit, flags, value, size]
}

/// The accumulator group: `private_append Unit nil {x,0} all`.
fn accumulator_group(module: &Module, kind: &str, unit: i64) -> Vec<Operand> {
    group(
        module,
        kind,
        1,
        Operand::Integer(unit),
        Operand::Nil,
        Operand::X(0),
        atom(module, "all"),
    )
}

/// One `integer` field group: `integer 1 nil Value Bits`.
fn int_field(module: &Module, value: Operand, bits: i64) -> Vec<Operand> {
    group(
        module,
        "integer",
        2,
        Operand::Integer(1),
        Operand::Nil,
        value,
        Operand::Integer(bits),
    )
}

/// Run the walk and return the build's `(bytes, nbits)` view.
fn build_bytes(
    module: &Module,
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    segments: &[Operand],
) -> (Vec<u8>, usize) {
    let built = assemble(machine, module, &LabelMap::default(), NO_FAIL, segments, ctx)
        .expect("assemble")
        .expect("the build succeeds");
    built_bytes(built)
}

/// A fresh writable accumulator in `x(0)` — what `bs_init_writable` leaves behind.
fn accumulator(machine: &mut Machine, ctx: &mut dyn ExecutionContext, capacity: usize) -> Eterm {
    let acc = raw::make_writable(machine, ctx, capacity).expect("a writable accumulator");
    machine.set_x(0, acc).expect("x(0) is a register");
    acc
}

/// The typed **error** a failing build reports (the `NO_FAIL` route), or a panic naming
/// what happened instead.
fn build_error(
    module: &Module,
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    segments: &[Operand],
) -> BeamError {
    match assemble(machine, module, &LabelMap::default(), NO_FAIL, segments, ctx) {
        Err(error) => error,
        Ok(Err(step)) => panic!("the build reported the failure step {step:?}, not an error"),
        Ok(Ok(_)) => panic!("the build succeeded but a failure was expected"),
    }
}

/// The failure **`Step`** a build reports against a live `fail` label, or a panic naming
/// what happened instead.
fn build_fail_step(
    module: &Module,
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    fail: Label,
    segments: &[Operand],
) -> Step {
    let mut labels = LabelMap::new();
    labels.insert(fail, (0, 0));
    match assemble(machine, module, &labels, fail, segments, ctx) {
        Ok(Err(step)) => step,
        Ok(Ok(_)) => panic!("the build succeeded but a failure was expected"),
        Err(error) => panic!("the build raised {error:?} instead of branching"),
    }
}

// ---------------------------------------------------------------------------
// 1. The compiler's shape is built by the fast path
// ---------------------------------------------------------------------------

/// The `to_lower`/`capitalize_token` shape — the `private_append 8 all` group plus one
/// `integer 1 nil {tr,{x,N},…} {integer,8}` per element — is written by the fast path, in
/// place, publishing the accumulator term itself with a bumped window.
#[test]
fn the_compiler_accumulator_shape_is_written_by_the_fast_path() {
    let module = test_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let acc = accumulator(&mut machine, &mut ctx, 32);
    let before = fast_builds();
    // Three elements in ONE instruction, the way an `expand`-style group does it.
    let mut segments = accumulator_group(&module, "private_append", 8);
    for byte in [0x41u8, 0x42, 0x43] {
        segments.extend(int_field(&module, Operand::Integer(i64::from(byte)), 8));
    }
    let built = assemble(
        &mut machine,
        &module,
        &LabelMap::default(),
        NO_FAIL,
        &segments,
        &mut ctx,
    )
    .expect("assemble")
    .expect("the build succeeds");
    assert_eq!(fast_builds(), before + 1, "the fast path built it");
    assert!(
        matches!(built, Built::Term(term) if term == acc),
        "an in-place build publishes the accumulator term itself"
    );
    assert_eq!(built_bytes(built), (b"ABC".to_vec(), 24));
    // The next element continues at the bumped window end, still in place.
    let mut next = accumulator_group(&module, "private_append", 8);
    next.extend(int_field(&module, Operand::Integer(0x44), 8));
    let built = assemble(
        &mut machine,
        &module,
        &LabelMap::default(),
        NO_FAIL,
        &next,
        &mut ctx,
    )
    .expect("assemble")
    .expect("the build succeeds");
    assert_eq!(fast_builds(), before + 2, "the second element too");
    assert_eq!(built_bytes(built), (b"ABCD".to_vec(), 32));
    assert_eq!(
        machine.refc_binaries().len(),
        1,
        "one payload backed both builds"
    );
}

/// A four-field group (the `long_append:expand/1` shape) is written in place, and the
/// **copy-rebuild** route over a source that is not writable produces the same bytes.
#[test]
fn a_four_field_group_is_byte_identical_to_the_copy_rebuild_route() {
    let module = test_module();
    // In place, over a writable accumulator.
    let mut in_machine = Machine::new();
    let mut in_ctx = MinimalContext::new();
    accumulator(&mut in_machine, &mut in_ctx, 16);
    let before = fast_builds();
    let mut segments = accumulator_group(&module, "private_append", 8);
    for byte in [1u8, 2, 3, 4] {
        segments.extend(int_field(&module, Operand::Integer(i64::from(byte)), 8));
    }
    let in_place = build_bytes(&module, &mut in_machine, &mut in_ctx, &segments);
    assert_eq!(fast_builds(), before + 1, "the fast path built it");
    assert_eq!(in_place, (vec![1, 2, 3, 4], 32));
    // The same four bytes through the copy-rebuild route: a heap-binary seed with the
    // `bs_init_writable` accumulator's own **0-bit** window (and never a writable target),
    // plus the same field groups.
    let mut cb_machine = Machine::new();
    let mut cb_ctx = MinimalContext::new();
    let seed = raw::make_binary(&mut cb_machine, &mut cb_ctx, &[], 0).expect("a heap binary seed");
    cb_machine.set_x(0, seed).expect("x(0)");
    assert!(
        raw::writable_append_target(&cb_machine, seed).is_none(),
        "a `make_binary` result is never a writable target"
    );
    let before = fast_builds();
    let mut copy_segments = accumulator_group(&module, "private_append", 8);
    for byte in [1u8, 2, 3, 4] {
        copy_segments.extend(int_field(&module, Operand::Integer(i64::from(byte)), 8));
    }
    let copy_rebuild = build_bytes(&module, &mut cb_machine, &mut cb_ctx, &copy_segments);
    assert_eq!(fast_builds(), before, "the copy-rebuild route is the generic walk");
    assert_eq!(copy_rebuild, in_place, "byte-identical to the in-place build");
    // A plain `append` of the same group is byte-identical too (never in place).
    let mut ap_machine = Machine::new();
    let mut ap_ctx = MinimalContext::new();
    let seed = raw::make_binary(&mut ap_machine, &mut ap_ctx, &[], 0).expect("a heap binary seed");
    ap_machine.set_x(0, seed).expect("x(0)");
    let mut plain = accumulator_group(&module, "append", 8);
    for byte in [1u8, 2, 3, 4] {
        plain.extend(int_field(&module, Operand::Integer(i64::from(byte)), 8));
    }
    assert_eq!(
        build_bytes(&module, &mut ap_machine, &mut ap_ctx, &plain),
        (vec![1, 2, 3, 4], 32)
    );
}

/// Every whole-byte field width (`8..=64` bits) round-trips big-endian, the value masked
/// into the field — a negative one included, as its two's complement.
#[test]
fn every_whole_byte_field_width_round_trips() {
    let module = test_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    accumulator(&mut machine, &mut ctx, 64);
    let before = fast_builds();
    let widths: [i64; 8] = [8, 16, 24, 32, 40, 48, 56, 64];
    let values: [i64; 8] = [0x11, 0x2233, 0x445566, 0x778899AA, 0x0, 1, 0x7F, -2];
    let mut segments = accumulator_group(&module, "private_append", 1);
    for (value, bits) in values.iter().zip(widths) {
        segments.extend(int_field(&module, Operand::Integer(*value), bits));
    }
    let (bytes, nbits) = build_bytes(&module, &mut machine, &mut ctx, &segments);
    assert_eq!(fast_builds(), before + 1, "one fast build for all eight");
    assert_eq!(nbits, 8 + 16 + 24 + 32 + 40 + 48 + 56 + 64);
    let mut expected: Vec<u8> = Vec::new();
    for (value, bits) in values.iter().zip(widths) {
        let width = (bits / 8) as usize;
        expected.extend_from_slice(&(*value as u64).to_be_bytes()[8 - width..]);
    }
    assert_eq!(bytes, expected, "each field's low bits, big-endian");
}

// ---------------------------------------------------------------------------
// 2. Every runtime deviation declines — the generic walk's own result stands
// ---------------------------------------------------------------------------

/// A **non-integer** field value declines the fast path and still raises the bare
/// `badarg` the generic walk reports (`$BS_FAIL_INFO`, `emu/bs_instrs.tab:347`), leaving
/// the accumulator's window unbumped.
#[test]
fn a_non_integer_field_value_declines_and_still_raises_badarg() {
    let module = test_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    accumulator(&mut machine, &mut ctx, 16);
    let before = fast_builds();
    let mut segments = accumulator_group(&module, "private_append", 8);
    segments.extend(int_field(&module, atom(&module, "integer"), 8));
    let error = build_error(&module, &mut machine, &mut ctx, &segments);
    assert!(
        matches!(error, BeamError::BadArg),
        "the bare `badarg`, got {error:?}"
    );
    assert_eq!(fast_builds(), before, "the fast path declined");
    let (_, nbits, _) = raw::heap_bits(machine.x(0).expect("x(0)")).expect("a bitstring");
    assert_eq!(nbits, 0, "a failed build leaves the window unbumped");
}

/// A **bignum** field value declines the fast path (it needs the wide writer) and is
/// masked into the field exactly as the oracle does:
/// `erl -noshell -eval 'io:format("~p~n",[<<(1 bsl 70):32>>]),halt().'` ⇒ `<<0,0,0,0>>`
/// (the value taken modulo `2^32`).
#[test]
fn a_bignum_field_value_declines_and_is_masked_into_the_field() {
    let module = test_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    accumulator(&mut machine, &mut ctx, 16);
    // `64 * 2^64 == 2^70`, a bignum (digits are least-significant first).
    let big = raw::make_big(&mut machine, &mut ctx, false, &[0, 64]).expect("a bignum");
    machine.set_x(1, big).expect("x(1) is a register");
    let before = fast_builds();
    let mut segments = accumulator_group(&module, "private_append", 8);
    segments.extend(int_field(&module, Operand::Integer(7), 8));
    segments.extend(group(
        &module,
        "integer",
        3,
        Operand::Integer(1),
        Operand::Nil,
        Operand::X(1),
        Operand::Integer(32),
    ));
    let (bytes, nbits) = build_bytes(&module, &mut machine, &mut ctx, &segments);
    assert_eq!(fast_builds(), before, "the fast path declined the bignum");
    assert_eq!(nbits, 40, "one accumulator byte plus the 32-bit field");
    assert_eq!(bytes, vec![7, 0, 0, 0, 0], "2^70 mod 2^32 == 0");
}

/// A **non-writable** accumulator (an ordinary heap binary) declines: the build takes the
/// copy-rebuild route with identical bytes.
#[test]
fn a_non_writable_accumulator_declines_to_the_copy_rebuild_route() {
    let module = test_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let seed = raw::make_binary(&mut machine, &mut ctx, &[9], 8).expect("a heap binary");
    machine.set_x(0, seed).expect("x(0)");
    let before = fast_builds();
    let mut segments = accumulator_group(&module, "private_append", 8);
    segments.extend(int_field(&module, Operand::Integer(1), 8));
    let (bytes, nbits) = build_bytes(&module, &mut machine, &mut ctx, &segments);
    assert_eq!(fast_builds(), before, "the fast path declined");
    assert_eq!((bytes, nbits), (vec![9, 1], 16));
}

/// A build position that is **not byte-aligned** (`<<Acc/binary, 3:4>>` first) declines:
/// only the generic walk's bit-granular writers are sound there.
#[test]
fn a_non_byte_aligned_accumulator_declines() {
    let module = test_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    accumulator(&mut machine, &mut ctx, 16);
    // A 4-bit field, declined by the static shape check (not a whole byte).
    let mut nibble = accumulator_group(&module, "private_append", 8);
    nibble.extend(int_field(&module, Operand::Integer(3), 4));
    assert_eq!(
        build_bytes(&module, &mut machine, &mut ctx, &nibble),
        (vec![0x30], 4),
        "the oracle's `<<3:4>>`"
    );
    // A whole-byte field over that 4-bit window: the shape passes, the position does not.
    // The accumulator group carries unit **1** (`<<Acc/bitstring, …>>`): a 4-bit window
    // under unit 8 would fail `erts_bs_append_checked`'s multiple-of-unit check instead of
    // reaching the alignment test.
    let before = fast_builds();
    let mut segments = accumulator_group(&module, "private_append", 1);
    segments.extend(int_field(&module, Operand::Integer(0xAB), 8));
    let (bytes, nbits) = build_bytes(&module, &mut machine, &mut ctx, &segments);
    assert_eq!(fast_builds(), before, "the fast path declined");
    // Oracle: `erl -noshell -eval 'io:format("~p~n",[<<<<3:4>>/bitstring,16#AB:8>>]),halt().'`
    // ⇒ `<<58,176>>` = `<<0x3A,0xB0>>` (12 bits).
    assert_eq!((bytes, nbits), (vec![0x3A, 0xB0], 12));
}

/// A value operand naming an out-of-range register keeps exactly the typed error the
/// generic walk reports: `read_value` is shared, so the fast path cannot widen it.
#[test]
fn a_bad_value_register_keeps_the_typed_error() {
    let module = test_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    accumulator(&mut machine, &mut ctx, 16);
    let before = fast_builds();
    let mut segments = accumulator_group(&module, "private_append", 8);
    segments.extend(int_field(&module, Operand::X(4095), 8));
    let error = build_error(&module, &mut machine, &mut ctx, &segments);
    assert!(
        matches!(error, BeamError::BadArg),
        "the register error is `BadArg`, got {error:?}"
    );
    assert_eq!(fast_builds(), before, "the fast path declined");
}

/// A non-bitstring accumulator keeps both the generic walk's `badarg` **and** its `fail`
/// branch: with no fail label it raises, with a live one it branches.
#[test]
fn a_non_bitstring_accumulator_keeps_the_fail_branch() {
    let module = test_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    machine.set_x(0, beam_term::make_small(17)).expect("x(0)");
    let mut segments = accumulator_group(&module, "private_append", 8);
    segments.extend(int_field(&module, Operand::Integer(1), 8));
    let error = build_error(&module, &mut machine, &mut ctx, &segments);
    assert!(
        matches!(error, BeamError::BadArg),
        "a non-bitstring accumulator raises `badarg`, got {error:?}"
    );
    // A live fail label turns the same failure into the branch.
    let step = build_fail_step(&module, &mut machine, &mut ctx, Label(7), &segments);
    assert!(
        matches!(step, Step::Goto { pc: 0, .. }),
        "the failure branches to the fail label, got {step:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. The guard itself
// ---------------------------------------------------------------------------

/// Every deviation from the guard declines in [`plan`] — so the generic walk reports it
/// with its own typed error — and the compiler's own shapes are accepted.
#[test]
fn the_guard_accepts_only_the_whole_byte_integer_accumulator_shape() {
    let module = test_module();
    let mut accepted = accumulator_group(&module, "private_append", 8);
    accepted.extend(int_field(&module, Operand::Integer(0), 8));
    accepted.extend(int_field(&module, Operand::Integer(0), 16));
    assert!(plan(&module, &accepted).is_some(), "the compiler's shape");
    let mut unit_one = accumulator_group(&module, "private_append", 1);
    unit_one.extend(int_field(&module, Operand::Integer(0), 8));
    assert!(plan(&module, &unit_one).is_some(), "a unit-1 accumulator");

    let field = |kind: &str, unit: Operand, flags: Operand, size: Operand| {
        let mut list = accumulator_group(&module, "private_append", 8);
        list.extend(group(
            &module,
            kind,
            2,
            unit,
            flags,
            Operand::Integer(0),
            size,
        ));
        list
    };
    // A truncated segment list (not a whole number of groups).
    let mut truncated = accepted.clone();
    truncated.pop();
    // Only the accumulator group: nothing to write.
    let only_accumulator = accumulator_group(&module, "private_append", 8);
    // `append` rather than `private_append`.
    let mut plain_append = accumulator_group(&module, "append", 8);
    plain_append.extend(int_field(&module, Operand::Integer(0), 8));
    // An accumulator size operand that is not `all`.
    let mut not_all = accumulator_group(&module, "private_append", 8);
    not_all[5] = Operand::Integer(8);
    not_all.extend(int_field(&module, Operand::Integer(0), 8));
    // Flags on the accumulator group.
    let mut acc_flags = accumulator_group(&module, "private_append", 8);
    acc_flags[3] = Operand::Literal(0);
    acc_flags.extend(int_field(&module, Operand::Integer(0), 8));
    // A non-static (register) accumulator unit.
    let mut dynamic_unit = accumulator_group(&module, "private_append", 8);
    dynamic_unit[2] = Operand::X(3);
    dynamic_unit.extend(int_field(&module, Operand::Integer(0), 8));
    let binary_field = field("binary", Operand::Integer(1), Operand::Nil, Operand::Integer(8));
    let unit_eight_field = field("integer", Operand::Integer(8), Operand::Nil, Operand::Integer(8));
    let field_flags = field("integer", Operand::Integer(1), Operand::Literal(0), Operand::Integer(8));
    let dynamic_size = field("integer", Operand::Integer(1), Operand::Nil, Operand::X(2));
    let nibble = field("integer", Operand::Integer(1), Operand::Nil, Operand::Integer(4));
    let wide = field("integer", Operand::Integer(1), Operand::Nil, Operand::Integer(72));
    // Nine fields: past the fast path's segment budget.
    let mut too_many = accumulator_group(&module, "private_append", 8);
    for _ in 0..9 {
        too_many.extend(int_field(&module, Operand::Integer(0), 8));
    }
    // A second field naming a different type atom.
    let mut mixed = accumulator_group(&module, "private_append", 8);
    mixed.extend(int_field(&module, Operand::Integer(0), 8));
    mixed.extend(group(
        &module,
        "binary",
        3,
        Operand::Integer(1),
        Operand::Nil,
        Operand::Integer(0),
        Operand::Integer(8),
    ));

    let declined: [(&str, &Vec<Operand>); 14] = [
        ("truncated list", &truncated),
        ("accumulator only", &only_accumulator),
        ("plain append", &plain_append),
        ("size not all", &not_all),
        ("accumulator flags", &acc_flags),
        ("dynamic accumulator unit", &dynamic_unit),
        ("binary field", &binary_field),
        ("field unit 8", &unit_eight_field),
        ("field flags", &field_flags),
        ("dynamic field size", &dynamic_size),
        ("4-bit field", &nibble),
        ("72-bit field", &wide),
        ("nine fields", &too_many),
        ("mixed field atoms", &mixed),
    ];
    for (what, segments) in declined {
        assert!(
            plan(&module, segments).is_none(),
            "{what}: the fast path must decline"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. A real `erlc`-compiled accumulator loop
// ---------------------------------------------------------------------------

/// A binary comprehension compiled by the **system `erlc`** builds through the fast path:
/// the compiled `bs_create_bin` group is the guard's shape, and the result is the
/// lowercased binary.
///
/// `lower/1` is a clause per letter rather than `C + 32` so the fixture needs **no BIF at
/// all**: `MinimalContext` installs no BIF hook, so arithmetic would abort the run with
/// `unsupported_opcode: bif erlang:+` before the accumulator loop is reached.
///
/// Skips loudly (never silently) when no `erlc` can be spawned.
#[test]
fn the_real_compiled_accumulator_loop_takes_the_fast_path() {
    let mut source = String::from(
        "-module(wholeacc).\n\
         -export([to_lower/1]).\n\
         to_lower(Bin) ->\n\
         \x20   << <<(lower(C))>> || <<C>> <= Bin >>.\n",
    );
    for (upper, lower) in ('A'..='Z').zip('a'..='z') {
        source.push_str(&format!("lower(${upper}) -> ${lower};\n"));
    }
    source.push_str("lower(C) -> C.\n");
    let Some(beam) = compile(&source) else {
        println!("SKIP: no `erlc` on PATH -- the compiled accumulator loop did not run");
        return;
    };
    let module =
        load_file(&beam).unwrap_or_else(|error| panic!("load {}: {error:?}", beam.display()));
    let entry = module
        .atom_table
        .lookup("to_lower")
        .expect("the fixture exports to_lower/1");
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let input = b"Ab1z Hello";
    let argument = make_heap_binary(&mut ctx, input).expect("a heap binary");
    let before = fast_builds();
    let result = machine
        .run_export(&module, entry, 1, &[argument], &mut ctx)
        .expect("to_lower/1 runs");
    let built = fast_builds() - before;
    assert!(
        built >= input.len(),
        "the compiled accumulator ran one fast build per element: {built} for {} elements",
        input.len()
    );
    assert_eq!(
        binary_bytes(result).expect("a binary result").to_vec(),
        b"ab1z hello".to_vec(),
        "the lowercased input"
    );
}

/// Compile `source` into a private temp directory with the system `erlc`; `None` when no
/// `erlc` can be spawned (the caller skips loudly).
fn compile(source: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("beam_engine_whole_acc_{}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
    std::fs::write(dir.join("wholeacc.erl"), source)
        .unwrap_or_else(|error| panic!("write wholeacc.erl: {error}"));
    let output = Command::new("erlc")
        .arg("+deterministic")
        .arg("wholeacc.erl")
        .current_dir(&dir)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erlc` exited {} compiling the fixture\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Some(dir.join("wholeacc.beam"))
}
