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
//! Pins for the **natively lowered `bs_create_bin` construction arm**
//! ([`super::emit::bitstr_inline::create`]): the emitted sequence, the exact bytes it
//! produces, and — the correctness crux — what happens on its one `fallback` edge.
//!
//! `bs_create_bin` is the second bitstring shape with a **mutation**: beyond the field bytes
//! it performs exactly one more write, C's `sb->end = new_position`
//! (`erl_bits.c:1975`), i.e. `ErlSubBits.end += tail_bits`. The arm's whole correctness
//! argument is the *order* of that write:
//!
//! ```text
//!   <dst's frame-window guard>                           ; a rejected slot writes nothing
//!   <the accumulator's frame-window guard, if Y>         ; guard ONLY, BEFORE any write
//!   <one guarded load + is_small guard per Field::Bits>  ; a non-integer field is the family's badarg
//!   <the helper call>                                    ; jit_open_append_bin (X) | jit_open_append_bin_y (Y)
//!   test rax, rax ; je fallback                          ; the LAST guard, still before every write
//!   <the field byte stores at the cursor in rax>         ; a Y field value is re-read (unguarded) per field
//!   <the accumulator re-read (unguarded), the end commit>; THE ONLY MUTATION, last
//!   <the accumulator re-read (unguarded) into Dst>       ; a Y Dst's address is re-derived too
//!   jmp fast_done
//! fallback:
//!   <one interpreter step>                               ; re-reads an untouched accumulator
//! fast_done:
//! ```
//!
//! A `fallback` is a **deopt**: `jit_step_one` re-executes the entire instruction in the
//! interpreter with the same operands and re-reads the machine's live state. So any word the
//! emitted sequence mutated *before* a fallback-capable guard would be visible to that
//! re-execution — an `end` committed twice would publish a window over bytes nobody wrote.
//! This file therefore pins the sequence's **idempotency** on every shape whose fallback it
//! cannot rule out (a plain accumulator, a one-byte-short capacity, a capacity crossed
//! mid-build), asserting the *bytes*, never merely term equality.
//!
//! One claim per concern:
//!
//! * **each in-scope shape is interpreter-identical, byte for byte.** A tier-agreement row
//!   alone also passes for a "lowering" that deopts every instruction, and a matched-but-wrong
//!   write is the failure mode an inline construction arm risks, so every value row asserts
//!   the built binary's payload against an expectation this file computes **in Rust from the
//!   input value** (`big_endian`, a plain two's-complement truncation) rather than copying
//!   from the implementation.
//! * **the arm actually emits.** The shapes are proved through a **cache-free** compile: the
//!   `#[cfg(test)]` emission counter [`bitstr_inline::inlined`] must move, the variant must
//!   be visited (so `"BsCreateBin"` is the census's real name) and it must record **zero**
//!   static deopts. Without that, "fixing" a wrong byte by dropping the arm would pass.
//! * **the boundaries.** An empty result, a zero-length `string` beside a real field, the
//!   widest (64-bit) field, a `string` past the 8-byte `movabs` chunk, an accumulator filled
//!   *exactly*, one byte *short* and *across* its capacity, and the built binary reused as an
//!   ordinary heap term afterwards (`put_tuple2`, then a fresh `bs_start_match3` /
//!   `bs_get_integer2` / `bs_test_tail2` pass over it, which fails unless `end` — and
//!   therefore the window — was committed exactly once).
//! * **the `Y` forms are lowered too, and their one up-front guard is what makes them
//!   sound.** Section A′ covers a frame-resident accumulator (the term crosses the block ABI
//!   to `jit_open_append_bin_y` as its **slot index**), a frame-resident field value, and a
//!   `Y` accumulator with two `Y` field values beside a `string` constant; section C(9) pins
//!   an **out-of-frame destination** slot, which the predicate accepts and the emitted guard
//!   rejects into the interpreter's own typed error; section E(c) pins the `Y`-accumulator
//!   one-byte-short capacity, whose fallback must leave the untouched slot untouched.
//! * **a declined shape emits nothing and stays on the family.** [`CreateBinShape::of`] must
//!   answer `None` for `binary all`, a 6-bit field, a float segment and a runtime size/unit;
//!   the emission counter must not move; and the family helper's **run-time** counter must
//!   move, so the row proves the family really executed it.
//!
//! No `.beam` fixture and no `erl`/`erlc` is involved: every row is a hand-built synthetic
//! module (the [`bitstr_inline_tests::bitstr_module`] route), driven on **both** tiers from
//! one module image by [`bitstr_tests::with_both_tiers_arg`]'s sibling
//! [`with_both_tiers_args`]. Nothing here skips.

use std::cell::Cell;

use beam_loader::{BsGetArgs, HeapNeed, Instr, Label, Module, Operand, Reg, NO_FAIL};
use beam_term::{Atom, Eterm};

use super::bitstr::bitstr_calls;
use super::bitstr_inline_tests::{
    atom, bitstr_module, emitted_shapes, static_deopts, visited_instances,
};
use super::counters;
use super::emit::bitstr_inline::{self, SHAPE_CREATE_BIN};
use super::tests::assert_same_outcome;
use crate::context::MinimalContext;
use crate::families::bitstrings::bit_size;
use crate::jit::Tier;
use crate::machine::{Machine, RunOutcome};

// ---------------------------------------------------------------------------
// Module building blocks
// ---------------------------------------------------------------------------
/// The type atoms every row's segment list can name. Interned up front because a segment
/// type is an `Operand::Atom`, so its id has to exist before the instruction stream is
/// written.
const BIN_ATOMS: [&str; 6] = [
    "private_append",
    "integer",
    "string",
    "binary",
    "float",
    "all",
];

/// A module whose `f/<arity>` (entry `Label(2)`) is `Label(1) Label(2) <build> return`.
///
/// `strings` is installed **after** assembly: it is the `StrT` table
/// [`seam::append_plan`](crate::families::bitstrings::create::seam::append_plan) resolves a
/// `string` segment's window out of, so it only has to be in place before the module is
/// compiled or run, never before its atoms are minted.
fn bin_module(
    name: &str,
    arity: u32,
    strings: &[u8],
    build: impl FnOnce(&Module) -> Vec<Instr>,
) -> Module {
    let mut module = bitstr_module(name, &BIN_ATOMS, arity, move |module| {
        let mut code = vec![Instr::Label(Label(1)), Instr::Label(Label(2))];
        code.extend(build(module));
        code.push(Instr::Return);
        code
    });
    module.strings = strings.to_vec();
    module
}

/// A `bs_create_bin` instruction with this fixture's frozen operand spelling.
fn create_bin(dst: Operand, segments: Vec<Operand>, live: u32) -> Instr {
    Instr::BsCreateBin {
        fail: NO_FAIL,
        alloc: 0,
        live,
        unit: 1,
        dst,
        segments,
    }
}

/// The first segment group, `private_append Acc` — the compiler's linear accumulator.
///
/// The unit is `8` and the size operand is `all`, exactly as
/// [`bitstr_inline_tests::bitstr_module`]'s own `bs_create_bin` fixture spells it.
fn append_seg(module: &Module, acc: Operand) -> Vec<Operand> {
    vec![
        Operand::Atom(atom(module, "private_append")),
        Operand::Integer(1),
        Operand::Integer(8),
        Operand::Nil,
        acc,
        Operand::Atom(atom(module, "all")),
    ]
}

/// An `integer Value Size` field group — `Size` is in bits, `unit` is `1`.
fn int_seg(module: &Module, value: Operand, size: i64) -> Vec<Operand> {
    vec![
        Operand::Atom(atom(module, "integer")),
        Operand::Integer(0),
        Operand::Integer(1),
        Operand::Nil,
        value,
        Operand::Integer(size),
    ]
}

/// A `string` group over the module's `StrT` table: the byte `offset` and the byte `len`.
fn str_seg(module: &Module, offset: i64, len: i64) -> Vec<Operand> {
    vec![
        Operand::Atom(atom(module, "string")),
        Operand::Integer(0),
        Operand::Integer(1),
        Operand::Nil,
        Operand::Integer(offset),
        Operand::Integer(len),
    ]
}

/// A `binary Value all` group — the whole remaining source.
fn bin_seg_all(module: &Module, value: Operand) -> Vec<Operand> {
    vec![
        Operand::Atom(atom(module, "binary")),
        Operand::Integer(0),
        Operand::Integer(1),
        Operand::Nil,
        value,
        Operand::Atom(atom(module, "all")),
    ]
}

/// A `float Value 64` group.
fn float_seg(module: &Module, value: Operand, size: i64) -> Vec<Operand> {
    vec![
        Operand::Atom(atom(module, "float")),
        Operand::Integer(0),
        Operand::Integer(1),
        Operand::Nil,
        value,
        Operand::Integer(size),
    ]
}

/// The operand count of one `bs_create_bin` segment group — the family's own
/// `prepared::SEGMENT_OPERANDS` (type atom, `Live`, `Unit`, `Flags`, the value, the size),
/// mirrored here because that constant is private to the family node.
const SEGMENT_OPERANDS: usize = 6;

/// How many arguments a segment list needs **beyond** `x(0)`: one per field group whose value
/// operand — group slot 4 — is an x-register, i.e. for a field whose value is a run-time
/// term (`integer`, `float`, `binary`). A `string` group's slot 4 is the literal's table
/// offset, a compile-time constant, so it takes no register — which is exactly why a
/// `string`-only (or all-zero-length) field list exports `f/1`.
///
/// `segments` is the **whole** operand list. The leading group is skipped: it is the
/// `private_append` accumulator, whose slot 4 names `x(0)`, not an argument.
fn field_argument_count(segments: &[Operand]) -> u32 {
    segments
        .chunks_exact(SEGMENT_OPERANDS)
        .skip(1)
        .filter(|group| {
            matches!(
                group[4],
                Operand::X(_) | Operand::Typed { reg: Reg::X(_), .. }
            )
        })
        .count() as u32
}

/// `bs_init_writable` + one `bs_create_bin` appending `fields` to `x(0)` — the shape every
/// value row of section A runs.
///
/// `f/(1 + count)`, where `count` is the number of field groups with an x-register value
/// operand ([`field_argument_count`]): `x(0)` is the capacity hint `bs_init_writable` reads
/// (C's `bin_size`, `erl_bits.c:2023`) and each such group's value operand is one more
/// argument, so `x(1..1 + count)` are the field values. A row whose fields hold no such
/// group — a `string`-only or empty field list — therefore exports `f/1`, and the same figure
/// is the instruction's `Live`.
///
/// The count is a fact about the assembled segment list, which needs the module's interned
/// atoms, so it is captured out of the build closure and the function's and the export's
/// arity are set from it afterwards — `bin_module`'s block decomposition is arity-independent.
fn accumulator_module(name: &str, strings: &[u8], fields: impl FnOnce(&Module) -> Vec<Operand>) -> Module {
    let arity = Cell::new(1u32);
    let counted = &arity;
    let mut module = bin_module(name, 1, strings, move |module| {
        let mut segments = append_seg(module, Operand::X(0));
        segments.extend(fields(module));
        counted.set(1 + field_argument_count(&segments));
        let live = counted.get();
        vec![
            Instr::BsInitWritable,
            create_bin(Operand::X(0), segments, live),
        ]
    });
    let arity = arity.get();
    module.functions[0].arity = arity;
    module.exports[0].arity = arity;
    module
}

/// `bs_init_writable` + one `bs_create_bin` whose accumulator — and optionally whose field
/// values — live in the current frame's **`Y` area**, the frame-resident shape a real
/// accumulator loop reaches once its accumulator has been `move`d into the frame (a
/// `call_last` callee's `{tr,{y,0},{t_bitstring,8,true}}`).
///
/// `f/<arity>`'s `x(0)` is the capacity hint `bs_init_writable` reads (and replaces with the
/// writable binary); `acc_in_y` decides whether that binary is then moved into `Y(0)` (or
/// stays in `x(0)`), and `slot_values` is how many of `x(1..)` the row also copies into
/// `Y(1..=slot_values)` so a field-value operand can name a stack slot. The frame is
/// `1 + slot_values` words (slot `Y(0)` is reserved for the accumulator even when the row
/// leaves it in `x(0)`, which keeps every row's value slots at the same indices).
///
/// The build is `fields`, whose `private_append` group names the accumulator's slot and
/// whose destination is that same slot; the accumulator is copied back into `x(0)` before
/// the frame is dropped, so the exported function returns the built binary.
fn frame_module(
    name: &str,
    strings: &[u8],
    arity: u32,
    acc_in_y: bool,
    slot_values: u32,
    fields: impl FnOnce(&Module) -> Vec<Operand>,
) -> Module {
    let slots = 1 + slot_values;
    bin_module(name, arity, strings, move |module| {
        let acc = if acc_in_y {
            Operand::Y(0)
        } else {
            Operand::X(0)
        };
        let mut segments = append_seg(module, acc.clone());
        segments.extend(fields(module));
        let mut code = vec![
            Instr::Allocate {
                stack_need: HeapNeed::Words(slots),
                live: arity,
            },
            Instr::BsInitWritable,
        ];
        if acc_in_y {
            code.push(Instr::Move {
                src: Operand::X(0),
                dst: Operand::Y(0),
            });
        }
        for slot in 1..=slot_values {
            code.push(Instr::Move {
                src: Operand::X(slot),
                dst: Operand::Y(slot),
            });
        }
        code.push(create_bin(acc.clone(), segments, arity));
        code.push(Instr::Move {
            src: acc,
            dst: Operand::X(0),
        });
        code.push(Instr::Deallocate(slots));
        code
    })
}

/// The module's single `bs_create_bin` operands — the shape predicate's own input.
fn create_bin_operands(module: &Module) -> (Operand, Vec<Operand>) {
    for instr in &module.functions[0].code {
        if let Instr::BsCreateBin { dst, segments, .. } = instr {
            return (dst.clone(), segments.clone());
        }
    }
    panic!(
        "{}: the module must hold a `bs_create_bin`",
        module.atom_name(module.name).unwrap_or("<unnamed>")
    );
}

// ---------------------------------------------------------------------------
// The byte oracle: two's-complement big-endian truncation, computed here
// ---------------------------------------------------------------------------
/// The bytes a whole-byte, unsigned, big-endian `nbytes`-byte integer field writes for
/// `value`: the low `8 * nbytes` bits of its two's-complement 64-bit form, most significant
/// byte first.
///
/// This is deliberately **independent** of the implementation (`(value as u64).to_be_bytes()`
/// truncated), so a wrong extraction inside an otherwise agreeing outcome fails.
fn big_endian(value: i64, nbytes: usize) -> Vec<u8> {
    let words = (value as u64).to_be_bytes();
    words[8 - nbytes..].to_vec()
}

// ---------------------------------------------------------------------------
// The harness: one module image, both tiers, per-side heaps
// ---------------------------------------------------------------------------
/// One argument a row passes to the exported function, built **per side** (each run owns its
/// own `MinimalContext` heap, so a term built for one run is not reusable in the other).
#[derive(Clone, Copy)]
enum ArgKind {
    /// A small integer.
    Int(i64),
    /// An on-heap binary (at most `ERL_ONHEAP_BINARY_LIMIT` bytes).
    Bytes(&'static [u8]),
    /// A float.
    Float(f64),
}

impl ArgKind {
    fn build(self, ctx: &mut MinimalContext) -> Eterm {
        match self {
            ArgKind::Int(value) => crate::terms::make_integer(ctx, value).expect("the argument builds"),
            ArgKind::Bytes(bytes) => {
                crate::terms::make_heap_binary(ctx, bytes).expect("the argument builds")
            }
            ArgKind::Float(value) => crate::terms::make_float(ctx, value).expect("the argument builds"),
        }
    }
}

/// Run exported `f/<args.len()>` with `args_spec` on **both** tiers, handing the two built
/// argument vectors and the two outcomes to `check`.
///
/// The comparison must happen *inside* this call: a returned boxed term (or an off-heap
/// payload) points into its own run's heap, and each `MinimalContext`/`Machine` pair stays
/// alive for the whole closure.
fn with_both_tiers_args_observed<R>(
    module: &Module,
    name: Atom,
    args_spec: &[ArgKind],
    check: impl FnOnce(&[Eterm], &beam_common::Result<RunOutcome>, &[Eterm], &beam_common::Result<RunOutcome>) -> R,
) -> R {
    let arity = args_spec.len() as u32;

    let mut interpreter_machine = Machine::new();
    let mut interpreter_ctx = MinimalContext::new();
    let interpreter_args: Vec<Eterm> = args_spec
        .iter()
        .map(|spec| spec.build(&mut interpreter_ctx))
        .collect();
    let interpreter_outcome = interpreter_machine.run_export_outcome_with_tier(
        module,
        name,
        arity,
        &interpreter_args,
        &mut interpreter_ctx,
        Tier::Interpreter,
    );

    let mut baseline_machine = Machine::new();
    let mut baseline_ctx = MinimalContext::new();
    let baseline_args: Vec<Eterm> = args_spec
        .iter()
        .map(|spec| spec.build(&mut baseline_ctx))
        .collect();
    let baseline_outcome = baseline_machine.run_export_outcome_with_tier(
        module,
        name,
        arity,
        &baseline_args,
        &mut baseline_ctx,
        Tier::BaselineJit,
    );

    check(
        &interpreter_args,
        &interpreter_outcome,
        &baseline_args,
        &baseline_outcome,
    )
}

/// [`with_both_tiers_args_observed`] without the built arguments.
fn with_both_tiers_args<R>(
    module: &Module,
    name: Atom,
    args_spec: &[ArgKind],
    check: impl FnOnce(&beam_common::Result<RunOutcome>, &beam_common::Result<RunOutcome>) -> R,
) -> R {
    with_both_tiers_args_observed(module, name, args_spec, |_, interpreter, _, baseline| {
        check(interpreter, baseline)
    })
}

/// The `RunOutcome::Returned` term of one side, or a panic naming the row and the side.
fn returned(label: &str, tier: &str, outcome: &beam_common::Result<RunOutcome>) -> Eterm {
    match outcome {
        Ok(RunOutcome::Returned(value)) => *value,
        Ok(other) => panic!("{label}: {tier}: unexpected outcome {other:?}"),
        Err(error) => panic!("{label}: {tier}: unexpected raise {error:?}"),
    }
}

/// The payload of one side's returned binary, or a panic naming the row and the side.
fn returned_bytes<'a>(
    label: &str,
    tier: &str,
    outcome: &'a beam_common::Result<RunOutcome>,
) -> &'a [u8] {
    let value = returned(label, tier, outcome);
    crate::terms::binary_bytes(value)
        .unwrap_or_else(|error| panic!("{label}: {tier}: the result is not a binary: {error:?}"))
}

/// **The value row driver**: run `f/<args.len()>` on both tiers, assert they agree, and
/// assert **each** side's built binary payload equals `expected`.
fn assert_construction_row(
    label: &str,
    module: &Module,
    args_spec: &[ArgKind],
    expected: &[u8],
) {
    let name = atom(module, "f");
    with_both_tiers_args(module, name, args_spec, |interpreter, baseline| {
        assert_same_outcome(label, interpreter, baseline, &module.atom_table);
        assert_eq!(
            returned_bytes(label, "interpreter", interpreter),
            expected,
            "{label}: the interpreter's bytes"
        );
        assert_eq!(
            returned_bytes(label, "baseline", baseline),
            expected,
            "{label}: the baseline tier's bytes"
        );
    });
}

/// What one counted run of a row reported, extracted into owned data so it can outlive the
/// run's heap.
struct RunReport {
    /// The returned binary's payload, when the result is a byte-aligned binary.
    bytes: Option<Vec<u8>>,
    /// The returned term's bit size, when it is a bitstring.
    bits: Option<usize>,
    /// The thread-local baseline counters after the run.
    counters: counters::Counters,
    /// How many times `jit_bitstr` (the family helper) ran during the run.
    bitstr_calls: u64,
}

/// One counted run of exported `f/<args.len()>` on `tier`, with the baseline counters and the
/// family helper's run-time counter taken around it.
fn run_counting(
    module: &Module,
    name: Atom,
    args_spec: &[ArgKind],
    tier: Tier,
) -> RunReport {
    let arity = args_spec.len() as u32;
    counters::reset();
    counters::set_enabled(true);
    let calls_before = bitstr_calls();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let args: Vec<Eterm> = args_spec.iter().map(|spec| spec.build(&mut ctx)).collect();
    let outcome = machine.run_export_outcome_with_tier(module, name, arity, &args, &mut ctx, tier);
    let calls = bitstr_calls() - calls_before;
    counters::set_enabled(false);
    let snapshot = counters::snapshot();
    // Extract everything the assertions need into owned data while the heap is still alive.
    let bytes = match &outcome {
        Ok(RunOutcome::Returned(term)) => crate::terms::binary_bytes(*term).ok().map(<[u8]>::to_vec),
        _ => None,
    };
    let bits = match &outcome {
        Ok(RunOutcome::Returned(term)) => bit_size(*term),
        _ => None,
    };
    RunReport {
        bytes,
        bits,
        counters: snapshot,
        bitstr_calls: calls,
    }
}

// ---------------------------------------------------------------------------
// A. Interpreter parity per inlined shape (value AND bytes)
// ---------------------------------------------------------------------------
/// **`integer 8` — the single-byte field.**
#[test]
fn an_integer_8_field_is_written_as_one_big_endian_byte() {
    let module = accumulator_module("bscb_int8", &[], |module| {
        int_seg(module, Operand::X(1), 8)
    });
    for value in [0i64, 1, 0x41, 0xFF] {
        assert_construction_row(
            &format!("integer 8 of {value:#x}"),
            &module,
            &[ArgKind::Int(64), ArgKind::Int(value)],
            &big_endian(value, 1),
        );
    }
}

/// **`integer 16` — two big-endian bytes.**
#[test]
fn an_integer_16_field_is_written_as_two_big_endian_bytes() {
    let module = accumulator_module("bscb_int16", &[], |module| {
        int_seg(module, Operand::X(1), 16)
    });
    for value in [0i64, 0x0102, 0xBEEF, 0xFFFF] {
        assert_construction_row(
            &format!("integer 16 of {value:#x}"),
            &module,
            &[ArgKind::Int(64), ArgKind::Int(value)],
            &big_endian(value, 2),
        );
    }
}

/// **`integer 24` — three big-endian bytes, the compile-time width whose store is
/// byte-by-byte (there is no 24-bit store).**
#[test]
fn an_integer_24_field_is_written_as_three_big_endian_bytes() {
    let module = accumulator_module("bscb_int24", &[], |module| {
        int_seg(module, Operand::X(1), 24)
    });
    for value in [0i64, 0x010203, 0xABCDEF] {
        assert_construction_row(
            &format!("integer 24 of {value:#x}"),
            &module,
            &[ArgKind::Int(64), ArgKind::Int(value)],
            &big_endian(value, 3),
        );
    }
}

/// **`integer 32` — four big-endian bytes.**
#[test]
fn an_integer_32_field_is_written_as_four_big_endian_bytes() {
    let module = accumulator_module("bscb_int32", &[], |module| {
        int_seg(module, Operand::X(1), 32)
    });
    for value in [0i64, 0x0102_0304, 0xDEAD_BEEFu32 as i64, 0xFFFF_FFFF] {
        assert_construction_row(
            &format!("integer 32 of {value:#x}"),
            &module,
            &[ArgKind::Int(64), ArgKind::Int(value)],
            &big_endian(value, 4),
        );
    }
}

/// **`integer 64` — the widest field the sequence lowers, and the peak shift count
/// (`4 + 8 * 7 = 60`).**
#[test]
fn an_integer_64_field_is_written_as_eight_big_endian_bytes() {
    let module = accumulator_module("bscb_int64", &[], |module| {
        int_seg(module, Operand::X(1), 64)
    });
    // Both live inside the 60-bit small-integer range, so the arm's `is_small` guard accepts
    // them and the width/endianness logic — not the bignum fallback — is what is measured.
    for value in [0i64, 1, 0x0123_4567_89AB_CDEF, -0x0123_4567_89AB_CDEF] {
        assert_construction_row(
            &format!("integer 64 of {value:#x}"),
            &module,
            &[ArgKind::Int(64), ArgKind::Int(value)],
            &big_endian(value, 8),
        );
    }
}

/// **A `string` constant field** — the `ConstBytes` path, whose bytes are baked into the code
/// as an immediate (never as a `StrT` pointer, which a cached module's string area would
/// invalidate).
#[test]
fn a_string_constant_field_writes_its_literal_bytes() {
    let literal: &[u8] = b"hey";
    let module = accumulator_module("bscb_string", literal, |module| {
        str_seg(module, 0, 3)
    });
    assert_construction_row(
        "string \"hey\"",
        &module,
        &[ArgKind::Int(64)],
        literal,
    );
}

/// **A mixed shape: an `integer 8` field followed by a `string` constant** — two different
/// write forms at two different offsets from the same cursor.
#[test]
fn a_mixed_integer_8_and_string_shape_writes_both_fields_in_order() {
    let literal: &[u8] = b"abc";
    let module = accumulator_module("bscb_mixed", literal, |module| {
        let mut fields = int_seg(module, Operand::X(1), 8);
        fields.extend(str_seg(module, 0, 3));
        fields
    });
    for value in [0i64, 0x5A, 0xFF] {
        let mut expected = big_endian(value, 1);
        expected.extend_from_slice(literal);
        assert_construction_row(
            &format!("integer 8 of {value:#x} then \"abc\""),
            &module,
            &[ArgKind::Int(64), ArgKind::Int(value)],
            &expected,
        );
    }
}

/// **A negative field is its two's-complement truncation, and an oversized value is
/// truncated to the field width** — the two value rows that catch a shift that assumed a
/// non-negative, in-range operand.
#[test]
fn a_negative_and_an_oversized_value_are_truncated_to_the_field_width() {
    // `<<(-1):16>>` is `<<0xFF,0xFF>>`.
    let sixteen = accumulator_module("bscb_negative16", &[], |module| {
        int_seg(module, Operand::X(1), 16)
    });
    assert_eq!(
        big_endian(-1, 2),
        vec![0xFF, 0xFF],
        "the row's own expectation must be the two's-complement form"
    );
    assert_construction_row(
        "integer 16 of -1",
        &sixteen,
        &[ArgKind::Int(64), ArgKind::Int(-1)],
        &[0xFF, 0xFF],
    );
    for value in [-2i64, -0x0102, i16::MIN as i64] {
        assert_construction_row(
            &format!("integer 16 of {value:#x}"),
            &sixteen,
            &[ArgKind::Int(64), ArgKind::Int(value)],
            &big_endian(value, 2),
        );
    }

    // `<<0x1FF:8>>` is `<<0xFF>>` — `push_uint` masks nothing, the low byte wins.
    let eight = accumulator_module("bscb_trunc8", &[], |module| {
        int_seg(module, Operand::X(1), 8)
    });
    assert_construction_row(
        "integer 8 of 0x1ff",
        &eight,
        &[ArgKind::Int(64), ArgKind::Int(0x1FF)],
        &[0xFF],
    );
    // …and a value wider than the field is truncated the same way at every width.
    let thirty_two = accumulator_module("bscb_trunc32", &[], |module| {
        int_seg(module, Operand::X(1), 32)
    });
    assert_construction_row(
        "integer 32 of 0x1_2345_6789",
        &thirty_two,
        &[ArgKind::Int(64), ArgKind::Int(0x1_2345_6789)],
        &big_endian(0x1_2345_6789, 4),
    );
}

// ---------------------------------------------------------------------------
// B. Non-vacuity: the arm emits, and stays native
// ---------------------------------------------------------------------------
/// **The mandatory instrument for every in-scope shape: a cache-free compile moves the
/// `bs_create_bin` emission counter, visits the variant, and records no static deopt.**
///
/// A warm-process compile cannot carry this claim — the process-wide content-keyed
/// [`crate::jit::cache::CodeCache`] serves a cached artifact and re-emits nothing, so the
/// counter would read `0 -> 0`. [`emitted_shapes`] therefore clears the cache and calls
/// [`super::emit::compile_function`] directly.
#[test]
fn every_in_scope_construction_shape_stays_native_on_a_cache_free_compile() {
    let literal: &[u8] = b"abc";
    let cases: Vec<(&str, Module)> = vec![
        (
            "integer 8",
            accumulator_module("bscb_native8", &[], |m| int_seg(m, Operand::X(1), 8)),
        ),
        (
            "integer 16",
            accumulator_module("bscb_native16", &[], |m| int_seg(m, Operand::X(1), 16)),
        ),
        (
            "integer 24",
            accumulator_module("bscb_native24", &[], |m| int_seg(m, Operand::X(1), 24)),
        ),
        (
            "integer 32",
            accumulator_module("bscb_native32", &[], |m| int_seg(m, Operand::X(1), 32)),
        ),
        (
            "integer 64",
            accumulator_module("bscb_native64", &[], |m| int_seg(m, Operand::X(1), 64)),
        ),
        (
            "string constant",
            accumulator_module("bscb_nativestr", literal, |m| str_seg(m, 0, 3)),
        ),
        (
            "mixed integer 8 + string",
            accumulator_module("bscb_nativemix", literal, |m| {
                let mut fields = int_seg(m, Operand::X(1), 8);
                fields.extend(str_seg(m, 0, 3));
                fields
            }),
        ),
    ];
    for (label, module) in &cases {
        let module = module;
        let (emitted, snapshot) = emitted_shapes(module);
        assert!(
            emitted.contains(&SHAPE_CREATE_BIN),
            "{label}: a cache-free compile must emit the `bs_create_bin` arm (emitted \
             {emitted:?}); a tier-agreement row alone would also pass for an arm that deopts \
             everything"
        );
        assert!(
            visited_instances(&snapshot, "BsCreateBin") >= 1,
            "{label}: `BsCreateBin` must be the census's variant name (visited {:?}); a wrong \
             name would make the deopt assertion below vacuous",
            snapshot.instr_instances
        );
        assert_eq!(
            static_deopts(&snapshot, "BsCreateBin"),
            0,
            "{label}: the instruction must be lowered, never statically deopted (deopts {:?})",
            snapshot.instr_deopts
        );
    }
}

// ---------------------------------------------------------------------------
// A′. The Y forms: a frame-resident accumulator and frame-resident field values
// ---------------------------------------------------------------------------
/// **A′(1) A `Y`-slot accumulator — lowered, byte-exact, and the family never runs.**
///
/// The accumulator crosses the block ABI as its **slot index**, not a word
/// (`emit_helper_invoke` can only add `movabs` immediates), so the arm carries it to
/// `jit_open_append_bin_y` and re-reads the slot behind the one up-front frame guard. This
/// row is the shape's whole justification: the bytes must be the interpreter's, and the
/// `bs_create_bin` must **not** reach the family.
///
/// The family-helper counter is asserted at its exact figure rather than at zero:
/// `bs_init_writable` has no native arm, so it contributes exactly one `jit_bitstr` call per
/// baseline run, and the construction must contribute **none**. A run of the interpreter
/// tier never enters the tier helper at all, which is the pair that makes the figure
/// meaningful.
#[test]
fn a_y_slot_accumulator_is_lowered_and_writes_the_same_bytes() {
    let module = frame_module("bscb_y_acc", &[], 2, true, 0, |module| {
        int_seg(module, Operand::X(1), 16)
    });

    // (1) The shape predicate accepts it, and the compile-time addressability gate is what
    // it went through (a `Y` slot is not an x-register, so `registers()` must not charge it).
    let (dst, segments) = create_bin_operands(&module);
    let shape = bitstr_inline::CreateBinShape::of(&module, &dst, &segments)
        .expect("a Y-slot accumulator must be an inlinable shape");
    assert!(shape.uses_y(), "the shape must declare its Y access");
    assert_eq!(
        shape.registers(),
        vec![1],
        "only the field value is an x-register; the accumulator's Y slot must never reach the \
         register cache"
    );

    for value in [0i64, 0x0102, 0xFFFF] {
        assert_construction_row(
            &format!("Y accumulator, integer 16 of {value:#x}"),
            &module,
            &[ArgKind::Int(64), ArgKind::Int(value)],
            &big_endian(value, 2),
        );
    }

    // (2) The arm emitted, exactly once, and records no static deopt.
    let name = atom(&module, "f");
    let report = run_counting(
        &module,
        name,
        &[ArgKind::Int(64), ArgKind::Int(0xBEEF)],
        Tier::BaselineJit,
    );
    assert_eq!(
        report.bytes.as_deref(),
        Some(&big_endian(0xBEEF, 2)[..]),
        "the Y-slot accumulator must build the interpreter's exact bytes"
    );
    let (emitted, snapshot) = emitted_shapes(&module);
    assert!(
        emitted.contains(&SHAPE_CREATE_BIN),
        "the Y-slot shape must emit (emitted {emitted:?})"
    );
    assert_eq!(
        static_deopts(&snapshot, "BsCreateBin"),
        0,
        "the Y-slot shape is lowered at compile time, never deopted"
    );
    // The non-vacuity pin: exactly the one `bs_init_writable` helper step, nothing for the
    // construction itself. A construction that silently fell to `jit_bitstr` would read 2.
    assert_eq!(
        report.bitstr_calls, 1,
        "only `bs_init_writable` (which has no native arm) may reach the family helper; the \
         `bs_create_bin` itself must be the emitted arm"
    );
    let interpreter = run_counting(
        &module,
        name,
        &[ArgKind::Int(64), ArgKind::Int(0xBEEF)],
        Tier::Interpreter,
    );
    assert_eq!(
        interpreter.bitstr_calls, 0,
        "the interpreter tier never enters the tier helper, so the figure above is the tier's"
    );
    assert_eq!(
        interpreter.bytes, report.bytes,
        "the two tiers must build the same bytes"
    );
}

/// **A′(2) A `Y`-slot field value — the value's guard is its guarded load, its byte stores
/// re-read the slot.**
///
/// The accumulator stays in `x(0)`, so this row isolates the *value* half of the widening: a
/// non-integer in the slot must reach the family's `badarg` through the `fallback` (the
/// `Loc::load` guard), never a fabricated byte.
#[test]
fn a_y_slot_field_value_is_lowered_and_writes_the_same_bytes() {
    let module = frame_module("bscb_y_field", &[], 2, false, 1, |module| {
        int_seg(module, Operand::Y(1), 16)
    });

    let (dst, segments) = create_bin_operands(&module);
    let shape = bitstr_inline::CreateBinShape::of(&module, &dst, &segments)
        .expect("a Y-slot field value must be an inlinable shape");
    assert!(shape.uses_y(), "the shape must declare its Y access");
    assert_eq!(
        shape.registers(),
        vec![0],
        "only the accumulator is an x-register; the field value's Y slot is not charged"
    );

    for value in [0i64, 0x0102, 0xABCD] {
        assert_construction_row(
            &format!("X accumulator, Y field value, integer 16 of {value:#x}"),
            &module,
            &[ArgKind::Int(64), ArgKind::Int(value)],
            &big_endian(value, 2),
        );
    }

    let name = atom(&module, "f");
    let report = run_counting(
        &module,
        name,
        &[ArgKind::Int(64), ArgKind::Int(0x1234)],
        Tier::BaselineJit,
    );
    assert_eq!(report.bytes.as_deref(), Some(&big_endian(0x1234, 2)[..]));
    assert_eq!(
        report.bitstr_calls, 1,
        "the construction must not reach the family helper (only `bs_init_writable` does)"
    );
    let (emitted, snapshot) = emitted_shapes(&module);
    assert!(emitted.contains(&SHAPE_CREATE_BIN));
    assert_eq!(static_deopts(&snapshot, "BsCreateBin"), 0);
}

/// **A′(3) A `Y` accumulator AND two `Y` field values plus a `string` constant** — every
/// offset form at once, so an offset that ignored a `Y` field's width shows up.
///
/// The three fields land at byte offsets `0`, `1` and `3` from the cursor the helper
/// returns; a re-read that used the wrong slot or a store that used the wrong offset
/// produces a payload this row's own Rust-computed expectation rejects.
#[test]
fn a_y_accumulator_with_two_y_field_values_writes_every_field_in_order() {
    let literal: &[u8] = b"xyz";
    let module = frame_module("bscb_y_both", literal, 3, true, 2, |module| {
        let mut fields = int_seg(module, Operand::Y(1), 8);
        fields.extend(int_seg(module, Operand::Y(2), 16));
        fields.extend(str_seg(module, 0, literal.len() as i64));
        fields
    });

    let (dst, segments) = create_bin_operands(&module);
    let shape = bitstr_inline::CreateBinShape::of(&module, &dst, &segments)
        .expect("a Y accumulator with Y field values must be an inlinable shape");
    assert!(shape.uses_y());
    assert!(
        shape.registers().is_empty(),
        "no operand of this shape is an x-register (registers were {:?})",
        shape.registers()
    );

    for (head, tail) in [(0i64, 0i64), (0x5Ai64, 0xBEEF), (0xFF, 0x0102)] {
        let mut expected = big_endian(head, 1);
        expected.extend(big_endian(tail, 2));
        expected.extend_from_slice(literal);
        assert_construction_row(
            &format!("Y accumulator, integer 8 of {head:#x} + integer 16 of {tail:#x} + \"xyz\""),
            &module,
            &[ArgKind::Int(64), ArgKind::Int(head), ArgKind::Int(tail)],
            &expected,
        );
    }

    let name = atom(&module, "f");
    let report = run_counting(
        &module,
        name,
        &[ArgKind::Int(64), ArgKind::Int(0x11), ArgKind::Int(0x2233)],
        Tier::BaselineJit,
    );
    let mut expected = big_endian(0x11, 1);
    expected.extend(big_endian(0x2233, 2));
    expected.extend_from_slice(literal);
    assert_eq!(
        report.bytes.as_deref(),
        Some(&expected[..]),
        "every field must land at its own offset"
    );
    assert_eq!(report.bitstr_calls, 1);
    let (emitted, snapshot) = emitted_shapes(&module);
    assert!(emitted.contains(&SHAPE_CREATE_BIN));
    assert_eq!(static_deopts(&snapshot, "BsCreateBin"), 0);
}

// ---------------------------------------------------------------------------
// C. Boundaries
// ---------------------------------------------------------------------------/// **C(1) An empty result.** A `string` field of length zero appends no bytes, so the
/// accumulator's window commits `end += 0` and the answer is `<<>>`.
///
/// The zero-length `string` field is load-bearing, not decoration: the truly **zero-field**
/// shape (the `private_append` group alone) is deliberately *not* inlinable — `append_plan`
/// answers `None` for a plan with no fields, because a build that writes nothing has nothing
/// to emit — so that shape takes the family route. This row pins an empty *result* on the
/// lowered path, which is why it carries one field whose width is zero (and why the
/// `static_deopts == 0` assertion below is meaningful). The no-field family route is not
/// exercised here.
#[test]
fn an_empty_result_is_the_empty_binary() {
    let module = accumulator_module("bscb_empty", &[], |module| str_seg(module, 0, 0));
    let name = atom(&module, "f");
    let report = run_counting(&module, name, &[ArgKind::Int(64)], Tier::BaselineJit);
    assert_eq!(
        report.bytes.as_deref(),
        Some(&[][..]),
        "a zero-length field list must build the empty binary"
    );
    assert_construction_row("zero-length string field", &module, &[ArgKind::Int(64)], &[]);
    // The shape is still lowered: `tail_bits == 0` is inside the arm's accept set.
    let (emitted, snapshot) = emitted_shapes(&module);
    assert!(
        emitted.contains(&SHAPE_CREATE_BIN),
        "a zero-tail construction must still be emitted (emitted {emitted:?})"
    );
    assert_eq!(static_deopts(&snapshot, "BsCreateBin"), 0);
}

/// **C(2) A zero-length `string` field beside a real one.** The offsets must not shift: the
/// real field keeps offset `0` and the commit covers only the bytes actually written.
#[test]
fn a_zero_length_string_beside_a_real_field_does_not_shift_the_offsets() {
    let module = accumulator_module("bscb_zerostr", &[], |module| {
        let mut fields = int_seg(module, Operand::X(1), 8);
        fields.extend(str_seg(module, 0, 0));
        fields
    });
    for value in [0i64, 0x7F, 0xFF] {
        assert_construction_row(
            &format!("integer 8 of {value:#x} then a zero-length string"),
            &module,
            &[ArgKind::Int(64), ArgKind::Int(value)],
            &big_endian(value, 1),
        );
    }
    let (emitted, snapshot) = emitted_shapes(&module);
    assert!(
        emitted.contains(&SHAPE_CREATE_BIN),
        "the mixed zero-length shape must still be emitted (emitted {emitted:?})"
    );
    assert_eq!(static_deopts(&snapshot, "BsCreateBin"), 0);
}

/// **C(3) The widest field: 64 bits, both signs, all eight bytes exactly.**
///
/// The peak shift count is `4 + 8 * 7 = 60`, and the top byte of a negative value is what the
/// **arithmetic** (sign-filling) shift leaves behind — the raw word's bit 63 *is* the small
/// value's sign bit, so `sar` reproduces `(value as i64 as u64).to_be_bytes()[0]` where a
/// logical shift's zero fill would write `0F` for `-1`. A shift or mask that is off by one
/// word bit shows up here and nowhere else.
#[test]
fn a_sixty_four_bit_field_writes_all_eight_bytes() {
    let module = accumulator_module("bscb_wide64", &[], |module| {
        int_seg(module, Operand::X(1), 64)
    });
    for value in [i64::MIN / 8, -1, 0, 1, (1i64 << 59) - 1] {
        assert_construction_row(
            &format!("integer 64 of {value:#x}"),
            &module,
            &[ArgKind::Int(64), ArgKind::Int(value)],
            &big_endian(value, 8),
        );
    }
    let (emitted, snapshot) = emitted_shapes(&module);
    assert!(emitted.contains(&SHAPE_CREATE_BIN));
    assert_eq!(static_deopts(&snapshot, "BsCreateBin"), 0);
}

/// **C(4) A `string` longer than eight bytes** — the `movabs` 8-byte chunk loop followed by
/// the byte tail (eleven bytes: one chunk plus three single-byte stores).
#[test]
fn a_string_longer_than_eight_bytes_uses_the_chunk_and_tail_path() {
    let literal: &[u8] = b"hello world";
    // Nine bytes is the shortest input with a tail, eleven exercises one chunk + three bytes.
    let short_tail: &[u8] = b"012345678";
    let module = accumulator_module("bscb_longstr", literal, |module| {
        str_seg(module, 0, literal.len() as i64)
    });
    assert_construction_row("string \"hello world\"", &module, &[ArgKind::Int(64)], literal);

    let tail_only = accumulator_module("bscb_tailstr", short_tail, |module| {
        str_seg(module, 0, short_tail.len() as i64)
    });
    assert_construction_row(
        "string \"012345678\"",
        &tail_only,
        &[ArgKind::Int(64)],
        short_tail,
    );
    let (emitted, snapshot) = emitted_shapes(&module);
    assert!(emitted.contains(&SHAPE_CREATE_BIN));
    assert_eq!(static_deopts(&snapshot, "BsCreateBin"), 0);
}

/// **C(5) The accumulator filled to its capacity exactly — accepted natively.**
///
/// The seam's room check is `end + tail <= capacity`, so a tail that lands exactly on the
/// capacity is still a native build. The run-time `jit_step_one` count is asserted **zero**
/// here: this is the one capacity row whose fallback must not fire.
#[test]
fn an_accumulator_reaching_its_capacity_exactly_stays_native() {
    let module = accumulator_module("bscb_exactcap", &[], |module| {
        int_seg(module, Operand::X(1), 16)
    });
    let name = atom(&module, "f");
    // A two-byte capacity for a two-byte field.
    let report = run_counting(&module, name, &[ArgKind::Int(2), ArgKind::Int(0xCAFE)], Tier::BaselineJit);
    assert_eq!(
        report.bytes.as_deref(),
        Some(&big_endian(0xCAFE, 2)[..]),
        "a build that lands exactly on the capacity must still write the field"
    );
    assert_eq!(
        report.counters.step_one, 0,
        "an exactly-fitting build must not take the fallback (the seam accepts it)"
    );
    assert_construction_row(
        "capacity 2, integer 16",
        &module,
        &[ArgKind::Int(2), ArgKind::Int(0xCAFE)],
        &big_endian(0xCAFE, 2),
    );
    let (emitted, snapshot) = emitted_shapes(&module);
    assert!(emitted.contains(&SHAPE_CREATE_BIN));
    assert_eq!(static_deopts(&snapshot, "BsCreateBin"), 0);
}

/// **C(6) The accumulator one byte short of its capacity — the arm emits, the run must
/// `fallback`, and the answer must be the family's exact bytes.**
///
/// This is the row that pins the mutation order: the room check is the *last* guard, before
/// every write, so the interpreter step re-executes an accumulator whose window end (and
/// payload) are untouched.
#[test]
fn an_accumulator_one_byte_short_falls_back_and_is_correct() {
    let module = accumulator_module("bscb_shortcap", &[], |module| {
        int_seg(module, Operand::X(1), 16)
    });
    let name = atom(&module, "f");
    let report = run_counting(&module, name, &[ArgKind::Int(1), ArgKind::Int(0xBEEF)], Tier::BaselineJit);
    assert_eq!(
        report.bytes.as_deref(),
        Some(&big_endian(0xBEEF, 2)[..]),
        "the one-byte-short build must still produce the family's exact bytes"
    );
    assert!(
        report.counters.step_one >= 1,
        "a one-byte-short capacity must really take the fallback (jit_step_one ran {} times); \
         otherwise this row does not exercise the deopt it exists to pin",
        report.counters.step_one
    );
    assert_construction_row(
        "capacity 1, integer 16",
        &module,
        &[ArgKind::Int(1), ArgKind::Int(0xBEEF)],
        &big_endian(0xBEEF, 2),
    );
    // The shape is still *emitted*: the refusal is a run-time one, never a compile-time
    // deopt (that distinction is what makes the fallback path idempotent-critical).
    let (emitted, snapshot) = emitted_shapes(&module);
    assert!(
        emitted.contains(&SHAPE_CREATE_BIN),
        "the capacity refusal is a run-time guard, so the arm must still emit (emitted \
         {emitted:?})"
    );
    assert_eq!(static_deopts(&snapshot, "BsCreateBin"), 0);
}

/// **C(7) The accumulator crossed mid-build: two constructions, the first fitting and the
/// second not.**
///
/// The first instruction builds natively (a one-byte tail into a two-byte capacity); the
/// second needs two more bytes, finds only one left and takes the family's growth path. The
/// regression this pins is a native `end` commit that the interpreter step then re-applies:
/// the result would grow a byte too far and its tail would be wrong.
#[test]
fn an_accumulator_crossing_its_capacity_mid_build_falls_back_and_is_correct() {
    let module = bin_module("bscb_crosscap", 2, &[], |module| {
        let mut first = append_seg(module, Operand::X(0));
        first.extend(int_seg(module, Operand::X(1), 8));
        let mut second = append_seg(module, Operand::X(0));
        second.extend(int_seg(module, Operand::X(1), 16));
        vec![
            Instr::BsInitWritable,
            create_bin(Operand::X(0), first, 2),
            create_bin(Operand::X(0), second, 2),
        ]
    });
    let name = atom(&module, "f");
    // Capacity 2: the first (1-byte) build fits, the second (2-byte) one does not.
    let value: i64 = 0xA1B2C3;
    let mut expected = big_endian(value, 1);
    expected.extend(big_endian(value, 2));
    let report = run_counting(&module, name, &[ArgKind::Int(2), ArgKind::Int(value)], Tier::BaselineJit);
    assert_eq!(
        report.bytes.as_deref(),
        Some(&expected[..]),
        "the crossing build must grow and reproduce both fields in order"
    );
    assert!(
        report.counters.step_one >= 1,
        "the second build must really take the fallback (jit_step_one ran {} times)",
        report.counters.step_one
    );
    assert_construction_row(
        "capacity 2, integer 8 then integer 16",
        &module,
        &[ArgKind::Int(2), ArgKind::Int(value)],
        &expected,
    );
}

/// **C(8) The built binary is an ordinary heap term afterwards.**
///
/// It is stored into a tuple, read back out, and re-entered with `bs_start_match3` /
/// `bs_get_integer2` / `bs_test_tail2`. The trailing `bs_test_tail2` (an **empty** tail) is
/// the "`end` was committed exactly once" assertion: a double commit would leave a 16-bit
/// tail behind, fail the match and answer the sentinel `-1` instead of the tuple.
#[test]
fn the_built_binary_is_usable_as_a_heap_term_afterwards() {
    let module = bin_module("bscb_reuse", 2, &[], |module| {
        let mut segments = append_seg(module, Operand::X(0));
        segments.extend(int_seg(module, Operand::X(1), 16));
        vec![
            Instr::BsInitWritable,
            create_bin(Operand::X(0), segments, 3),
            // Re-enter the result as a subject and consume exactly its width.
            Instr::BsStartMatch3 {
                fail: Label(3),
                bin: Operand::X(0),
                live: 3,
                dst: Operand::X(2),
            },
            Instr::BsGetInteger2(Box::new(BsGetArgs {
                fail: Label(3),
                ctx: Operand::X(2),
                live: 3,
                size: Operand::Integer(16),
                unit: 1,
                flags: 0,
                dst: Operand::X(3),
            })),
            Instr::BsTestTail2 {
                fail: Label(3),
                ctx: Operand::X(2),
                bits: 0,
            },
            // `{Built, ReRead}` — the binary itself, and the field read back out of it.
            Instr::PutTuple2 {
                dst: Operand::X(4),
                elements: vec![Operand::X(0), Operand::X(3)],
            },
            Instr::Move {
                src: Operand::X(4),
                dst: Operand::X(0),
            },
        ]
    });
    let name = atom(&module, "f");
    let value: i64 = 0x1234;
    let expected = big_endian(value, 2);
    with_both_tiers_args(
        &module,
        name,
        &[ArgKind::Int(64), ArgKind::Int(value)],
        |interpreter, baseline| {
            assert_same_outcome("reuse as a heap term", interpreter, baseline, &module.atom_table);
            for (tier, outcome) in [("interpreter", interpreter), ("baseline", baseline)] {
                let tuple = returned("reuse as a heap term", tier, outcome);
                assert_eq!(
                    crate::terms::tuple_arity(tuple)
                        .unwrap_or_else(|error| panic!("{tier}: not a tuple: {error:?}")),
                    2,
                    "{tier}: the fail arm answers `-1`, so a tuple means the subject was \
                     exactly two bytes wide and the match succeeded"
                );
                let built = crate::terms::tuple_element(tuple, 0)
                    .unwrap_or_else(|error| panic!("{tier}: element 0: {error:?}"));
                assert_eq!(
                    crate::terms::binary_bytes(built)
                        .unwrap_or_else(|error| panic!("{tier}: element 0 bytes: {error:?}")),
                    &expected[..],
                    "{tier}: the built binary's payload"
                );
                let reread = crate::terms::tuple_element(tuple, 1)
                    .unwrap_or_else(|error| panic!("{tier}: element 1: {error:?}"));
                assert_eq!(
                    beam_term::signed_value(reread),
                    value,
                    "{tier}: the field read back out of the built binary"
                );
            }
        },
    );
}

/// **C(9) A destination slot outside the published frame: the arm's first guard rejects, and
/// the answer is the interpreter's own error — never a partially built binary.**
///
/// The shape predicate *accepts* it (a `Y` index the `disp32` can address is a compile-time
/// fact; whether the **frame** holds that slot is a run-time one), so the arm emits and its
/// destination guard — the very first thing the sequence emits — is what refuses. The
/// refusal is the `fallback`, i.e. exactly one interpreter step, which re-runs the whole
/// instruction and reports `bad_register(64)`.
///
/// The observable claims, all of them load-bearing: the shape predicate answers `Some` and
/// the arm emitted with zero static deopts (so the rejection really is a **run-time** guard);
/// the baseline run took the fallback (`jit_step_one` moved) instead of writing the field
/// bytes into a slot it could not address; and both tiers raised the **same** typed reason
/// and neither returned bytes. The accumulator's window is not observable after a raise, so
/// what stands in for "wrote nothing" is that last pair — a run that produced bytes would
/// have had to write them somewhere.
#[test]
fn an_out_of_frame_destination_slot_rejects_and_raises_the_interpreters_error() {
    const FAR_SLOT: u32 = 64;
    let module = bin_module("bscb_y_oob_dst", 2, &[], |module| {
        let mut segments = append_seg(module, Operand::X(0));
        segments.extend(int_seg(module, Operand::X(1), 16));
        vec![
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 2,
            },
            Instr::BsInitWritable,
            create_bin(Operand::Y(FAR_SLOT), segments, 2),
            Instr::Deallocate(1),
        ]
    });

    // (1) Compile time: accepted, and emitted.
    let (dst, segments) = create_bin_operands(&module);
    let shape = bitstr_inline::CreateBinShape::of(&module, &dst, &segments)
        .expect("an addressable-but-out-of-frame Y destination is a compile-time-valid shape");
    assert!(shape.uses_y());
    let (emitted, snapshot) = emitted_shapes(&module);
    assert!(
        emitted.contains(&SHAPE_CREATE_BIN),
        "the out-of-frame destination is a run-time guard, so the arm must still emit \
         (emitted {emitted:?})"
    );
    assert_eq!(
        static_deopts(&snapshot, "BsCreateBin"),
        0,
        "the shape must not be statically deopted: the rejection is the emitted guard's"
    );

    // (2) Run time: the guard rejects, so the fallback interpreter step runs the whole
    // instruction and raises with the interpreter's exact reason.
    let name = atom(&module, "f");
    let args = [ArgKind::Int(64), ArgKind::Int(0xBEEF)];
    let report = run_counting(&module, name, &args, Tier::BaselineJit);
    assert!(
        report.bytes.is_none(),
        "a rejected destination must not produce a binary (got {:?})",
        report.bytes
    );
    assert!(
        report.counters.step_one >= 1,
        "the destination guard must really take the fallback (jit_step_one ran {} times); \
         without that this row does not exercise the guard it exists to pin",
        report.counters.step_one
    );
    with_both_tiers_args(&module, name, &args, |interpreter, baseline| {
        assert_same_outcome(
            "out-of-frame destination",
            interpreter,
            baseline,
            &module.atom_table,
        );
        assert!(
            matches!(interpreter, Err(_)),
            "the interpreter itself must raise here (got {interpreter:?}); the row's premise \
             is that the arm reproduces the interpreter's own error"
        );
        assert!(
            matches!(baseline, Err(_)),
            "the baseline tier must raise, never return a partially built binary (got \
             {baseline:?})"
        );
    });
}

// ---------------------------------------------------------------------------
// D. Declined shapes — zero bytes emitted, the family's exact result
// ---------------------------------------------------------------------------
/// A declined shape's row: its module plus the arguments it needs.
struct DeclinedRow {
    label: &'static str,
    module: Module,
    args: Vec<ArgKind>,
    /// The built bitstring's width in bits (these rows are not all byte-aligned).
    bits: usize,
    /// The built bitstring's payload when it is byte-aligned.
    bytes: Option<Vec<u8>>,
}

/// **Every shape the sequence cannot reproduce exactly is declined at compile time and runs
/// on the family, unchanged.**
///
/// "Zero bytes emitted" is proved three ways per row: [`bitstr_inline::CreateBinShape::of`]
/// answers `None`, the cache-free emission counter does not move the `bs_create_bin` shape,
/// and the family helper's **run-time** counter moves when the row executes — so the row
/// cannot pass on an arm that silently started lowering a shape it must not.
#[test]
fn declined_shapes_emit_nothing_and_run_on_the_family() {
    let subject: &[u8] = b"hi";

    let mut rows = vec![
        // `binary all` — a `PreparedSeg::Binary` with an `All` width: no static field list.
        DeclinedRow {
            label: "binary all",
            module: bin_module("bscb_dec_binary", 2, &[], move |module| {
                let mut segments = append_seg(module, Operand::X(0));
                segments.extend(bin_seg_all(module, Operand::X(1)));
                vec![Instr::BsInitWritable, create_bin(Operand::X(0), segments, 2)]
            }),
            args: vec![ArgKind::Int(64), ArgKind::Bytes(subject)],
            bits: 16,
            bytes: Some(subject.to_vec()),
        },
        // A 6-bit integer — not a whole number of bytes.
        DeclinedRow {
            label: "integer 6",
            module: accumulator_module("bscb_dec_int6", &[], |module| {
                int_seg(module, Operand::X(1), 6)
            }),
            args: vec![ArgKind::Int(64), ArgKind::Int(0b10_1011)],
            bits: 6,
            bytes: None,
        },
        // A float segment.
        DeclinedRow {
            label: "float 64",
            module: accumulator_module("bscb_dec_float", &[], |module| {
                float_seg(module, Operand::X(1), 64)
            }),
            args: vec![ArgKind::Int(64), ArgKind::Float(1.5)],
            bits: 64,
            bytes: Some(1.5f64.to_be_bytes().to_vec()),
        },
    ];

    // A runtime `size` operand: `Width::Dynamic`, so the field width is not a compile-time
    // fact and the plan answers `None`.
    rows.push(DeclinedRow {
        label: "runtime size",
        module: bin_module("bscb_dec_dynsize", 3, &[], |module| {
            let mut segments = append_seg(module, Operand::X(0));
            segments.extend(int_seg(module, Operand::X(1), 0).into_iter().enumerate().map(
                |(index, operand)| {
                    // Replace the `Size` operand (the last one) with the runtime register.
                    if index == 5 {
                        Operand::X(2)
                    } else {
                        operand
                    }
                },
            ).collect::<Vec<_>>());
            vec![Instr::BsInitWritable, create_bin(Operand::X(0), segments, 3)]
        }),
        args: vec![ArgKind::Int(64), ArgKind::Int(0x5A), ArgKind::Int(8)],
        bits: 8,
        bytes: Some(vec![0x5A]),
    });

    // A runtime `unit` operand: `prepare` cannot resolve the group's unit, so it declines.
    rows.push(DeclinedRow {
        label: "runtime unit",
        module: bin_module("bscb_dec_dynunit", 3, &[], |module| {
            let mut segments = append_seg(module, Operand::X(0));
            let mut field = int_seg(module, Operand::X(1), 8);
            // `Unit` is the third operand of the group.
            field[2] = Operand::X(2);
            segments.extend(field);
            vec![Instr::BsInitWritable, create_bin(Operand::X(0), segments, 3)]
        }),
        args: vec![ArgKind::Int(64), ArgKind::Int(0x5A), ArgKind::Int(1)],
        bits: 8,
        bytes: Some(vec![0x5A]),
    });

    for row in rows {
        let label = row.label;
        // (1) The shape predicate declines.
        let (dst, segments) = create_bin_operands(&row.module);
        assert!(
            bitstr_inline::CreateBinShape::of(&row.module, &dst, &segments).is_none(),
            "{label}: the arm must decline the shape (a shape it cannot reproduce exactly \
             must stay on the family)"
        );
        // (2) The emitter emitted zero bytes for it.
        let (emitted, snapshot) = emitted_shapes(&row.module);
        assert!(
            !emitted.contains(&SHAPE_CREATE_BIN),
            "{label}: the emitting sequence must write zero bytes (emitted {emitted:?})"
        );
        assert!(
            visited_instances(&snapshot, "BsCreateBin") >= 1,
            "{label}: `BsCreateBin` must still be visited, so the check above is not vacuous \
             (visited {:?})",
            snapshot.instr_instances
        );
        // (3) The family really executed it, and both tiers agree with the family's value.
        let name = atom(&row.module, "f");
        let report = run_counting(&row.module, name, &row.args, Tier::BaselineJit);
        assert!(
            report.bitstr_calls >= 1,
            "{label}: the family helper (`jit_bitstr`) must run — the declined shape belongs to \
             the family, never to a deopt or a fabricated value"
        );
        assert_eq!(
            report.bits,
            Some(row.bits),
            "{label}: the built bitstring's width"
        );
        if let Some(expected) = row.bytes {
            assert_eq!(
                report.bytes.as_deref(),
                Some(&expected[..]),
                "{label}: the built payload"
            );
        }
        with_both_tiers_args(&row.module, name, &row.args, |interpreter, baseline| {
            assert_same_outcome(label, interpreter, baseline, &row.module.atom_table);
        });
    }
}

// ---------------------------------------------------------------------------
// E. Deopt idempotency
// ---------------------------------------------------------------------------
/// **(a) A run-time `open_append` decline: the accumulator is not a writable payload of this
/// machine.**
///
/// The source here is a plain on-heap binary (`ErlHeapBits`), so
/// [`seam::open_append`](crate::families::bitstrings::create::seam::open_append)'s
/// `raw::writable_append_target` test fails at its first clause (not an off-heap refc binary)
/// and the helper answers `0`. Every other guard passes — the shape is emitted, the field is
/// a small integer — so the emitted sequence really reaches `je fallback` and exactly one
/// interpreter step re-executes the whole instruction.
///
/// Beyond the bytes, the row asserts the **source term's bit size is unchanged**: on the
/// deopting path the accumulator's window was not advanced, which is the property that makes
/// the re-execution the family's own build and not a second append.
#[test]
fn a_plain_binary_accumulator_declines_and_is_idempotent() {
    let subject: &'static [u8] = &[0x11, 0x22, 0x33];
    let module = bin_module("bscb_deopt_plain", 2, &[], |module| {
        let mut segments = append_seg(module, Operand::X(0));
        segments.extend(int_seg(module, Operand::X(1), 8));
        vec![create_bin(Operand::X(0), segments, 2)]
    });
    let name = atom(&module, "f");
    let value: i64 = 0xAB;
    let mut expected = subject.to_vec();
    expected.extend(big_endian(value, 1));

    // The arm is emitted (a run-time refusal, not a compile-time deopt).
    let (emitted, snapshot) = emitted_shapes(&module);
    assert!(
        emitted.contains(&SHAPE_CREATE_BIN),
        "the plain-accumulator shape must still be emitted — the refusal is the helper's, at \
         run time (emitted {emitted:?})"
    );
    assert_eq!(static_deopts(&snapshot, "BsCreateBin"), 0);

    let report = run_counting(
        &module,
        name,
        &[ArgKind::Bytes(subject), ArgKind::Int(value)],
        Tier::BaselineJit,
    );
    assert_eq!(
        report.bytes.as_deref(),
        Some(&expected[..]),
        "the plain-accumulator build must reproduce the family's bytes"
    );
    assert!(
        report.counters.step_one >= 1,
        "a non-writable accumulator must really take the fallback (jit_step_one ran {} times)",
        report.counters.step_one
    );

    // Both tiers agree, and the source term's window is untouched on the baseline path.
    with_both_tiers_args_observed(
        &module,
        name,
        &[ArgKind::Bytes(subject), ArgKind::Int(value)],
        |interpreter_args, interpreter, baseline_args, baseline| {
            assert_same_outcome("plain accumulator", interpreter, baseline, &module.atom_table);
            assert_eq!(
                returned_bytes("plain accumulator", "interpreter", interpreter),
                &expected[..],
            );
            assert_eq!(
                returned_bytes("plain accumulator", "baseline", baseline),
                &expected[..],
            );
            for (tier, args) in [("interpreter", interpreter_args), ("baseline", baseline_args)] {
                assert_eq!(
                    bit_size(args[0]),
                    Some(subject.len() * 8),
                    "{tier}: the deopting path must leave the accumulator's own window alone — \
                     an advanced `end` here is what a non-idempotent commit looks like"
                );
            }
        },
    );
}

/// **(a′) The same decline with a *copied off-heap* accumulator.**
///
/// The first construction concatenates a 70-byte `string` onto the plain argument, which the
/// family answers with a fresh **off-heap refc binary** (`raw::make_binary`, never registered
/// as writable). The second construction then finds an off-heap source that is still not this
/// machine's writable payload — `writable_append_target`'s registry clause, not its
/// representation clause — and declines for that reason. Both instructions therefore take the
/// fallback and the answer must be the family's byte-exact concatenation.
#[test]
fn a_copied_off_heap_accumulator_declines_and_is_idempotent() {
    // 70 bytes pushes the first result past `ERL_ONHEAP_BINARY_LIMIT` (64), so it is stored
    // off-heap.
    let literal: &[u8] = &[b'x'; 70];
    let subject: &'static [u8] = b"ab";
    let module = bin_module("bscb_deopt_offheap", 2, literal, |module| {
        let mut first = append_seg(module, Operand::X(0));
        first.extend(str_seg(module, 0, literal.len() as i64));
        let mut second = append_seg(module, Operand::X(0));
        second.extend(int_seg(module, Operand::X(1), 8));
        vec![
            create_bin(Operand::X(0), first, 2),
            create_bin(Operand::X(0), second, 2),
        ]
    });
    let name = atom(&module, "f");
    let value: i64 = 0x7E;
    let mut expected = subject.to_vec();
    expected.extend_from_slice(literal);
    expected.extend(big_endian(value, 1));

    let (emitted, snapshot) = emitted_shapes(&module);
    assert!(
        emitted.contains(&SHAPE_CREATE_BIN),
        "both constructions must be emitted (emitted {emitted:?})"
    );
    assert_eq!(static_deopts(&snapshot, "BsCreateBin"), 0);

    let report = run_counting(
        &module,
        name,
        &[ArgKind::Bytes(subject), ArgKind::Int(value)],
        Tier::BaselineJit,
    );
    assert_eq!(
        report.bytes.as_deref(),
        Some(&expected[..]),
        "the copied off-heap accumulator must reproduce the family's bytes"
    );
    assert!(
        report.counters.step_one >= 1,
        "a copied off-heap accumulator must really take the fallback (jit_step_one ran {} \
         times)",
        report.counters.step_one
    );
    assert_construction_row(
        "copied off-heap accumulator",
        &module,
        &[ArgKind::Bytes(subject), ArgKind::Int(value)],
        &expected,
    );
}

/// **(b) The one-byte-short capacity, re-pinned as an idempotency row.**
///
/// Same module and input as C(6); the point here is the *mechanism*. The room check is the
/// last guard before the field stores, so the interpreter step sees an accumulator whose
/// `end` is still the pre-build position and appends the tail exactly once. A commit moved
/// above the helper would answer four bytes (the tail twice) or lose the field entirely.
#[test]
fn a_one_byte_short_capacity_declines_and_is_idempotent() {
    let module = accumulator_module("bscb_deopt_shortcap", &[], |module| {
        int_seg(module, Operand::X(1), 32)
    });
    let name = atom(&module, "f");
    let value: i64 = 0x0102_0304;
    let expected = big_endian(value, 4);
    // Capacity 3 for a 4-byte tail.
    let report = run_counting(
        &module,
        name,
        &[ArgKind::Int(3), ArgKind::Int(value)],
        Tier::BaselineJit,
    );
    assert_eq!(
        report.bytes.as_deref(),
        Some(&expected[..]),
        "the tail must be appended exactly once, to the family's bytes"
    );
    assert_eq!(
        report.bits,
        Some(32),
        "a doubled commit would publish a 64-bit window here"
    );
    assert!(
        report.counters.step_one >= 1,
        "the short capacity must really take the fallback (jit_step_one ran {} times)",
        report.counters.step_one
    );
    assert_construction_row(
        "capacity 3, integer 32",
        &module,
        &[ArgKind::Int(3), ArgKind::Int(value)],
        &expected,
    );
}

/// **(c) The one-byte-short capacity with a `Y`-slot accumulator, as an idempotency row.**
///
/// The `Y` form of C(6)/(b), and the row that pins the *frame* half of the mutation order:
/// the arm hands `jit_open_append_bin_y` the accumulator's **slot index**, the helper reads
/// `Machine::y` and answers `0` because the tail does not fit, and the emitted `je fallback`
/// takes exactly one interpreter step — which re-executes the whole instruction against an
/// accumulator whose window end (and payload) are untouched, because nothing the sequence
/// emits before that test writes anything.
///
/// What the row proves, and what would fail without it:
///
/// * the payload is the family's **exact** bytes — a losing order that committed the window
///   and then fell back would append the tail twice (`BE EF BE EF`);
/// * the result's bit size is the expected figure, not twice it — a doubled commit also
///   publishes a window over bytes nobody wrote;
/// * the fallback really happened (`jit_step_one` moved) while the arm was still **emitted**
///   (`emitted_shapes` + zero static deopts), so this is a run-time guard on a lowered
///   sequence and not a compile-time refusal;
/// * the family helper counter stays at exactly the one `bs_init_writable` step that has no
///   native arm — the re-execution goes through `jit_step_one`, never `jit_bitstr`, which is
///   how the row distinguishes "the arm's guard rejected" from "the shape was never lowered".
#[test]
fn a_y_slot_accumulator_one_byte_short_declines_and_is_idempotent() {
    let module = frame_module("bscb_y_deopt_shortcap", &[], 2, true, 0, |module| {
        int_seg(module, Operand::X(1), 16)
    });
    let name = atom(&module, "f");
    let value: i64 = 0xBEEF;
    let expected = big_endian(value, 2);
    // Capacity 1 for a 2-byte tail: `open_append`'s room check refuses, so the `Y` door
    // answers 0 and the fallback runs.
    let args = [ArgKind::Int(1), ArgKind::Int(value)];

    let report = run_counting(&module, name, &args, Tier::BaselineJit);
    assert_eq!(
        report.bytes.as_deref(),
        Some(&expected[..]),
        "the tail must be appended exactly once, to the family's bytes"
    );
    assert_eq!(
        report.bits,
        Some(16),
        "a doubled window commit would publish a 32-bit window here"
    );
    assert!(
        report.counters.step_one >= 1,
        "the short capacity must really take the fallback (jit_step_one ran {} times)",
        report.counters.step_one
    );
    assert_eq!(
        report.bitstr_calls, 1,
        "only `bs_init_writable` may reach the family helper: the re-execution is the \
         fallback's own interpreter step, so a construction that had silently stayed on the \
         family would read 2"
    );
    let (emitted, snapshot) = emitted_shapes(&module);
    assert!(
        emitted.contains(&SHAPE_CREATE_BIN),
        "the capacity refusal is a run-time guard, so the arm must still emit (emitted \
         {emitted:?})"
    );
    assert_eq!(static_deopts(&snapshot, "BsCreateBin"), 0);
    assert_construction_row("capacity 1, Y accumulator, integer 16", &module, &args, &expected);
}
