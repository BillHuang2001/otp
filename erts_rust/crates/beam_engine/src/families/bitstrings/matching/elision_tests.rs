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

//! Tests for the **allocation-aware context re-read elision** in the match side.
//!
//! `run_commands` captures the match context (and the subject's byte view) once and
//! re-derives it only after a command that may have allocated through the rooted seam:
//! `ensure_at_least`/`ensure_exactly`/`skip`/`=:=` never do, an `integer` command only
//! when its field does not fit a small integer, and `binary`/`get_tail` always do
//! (they build a bitstring every time). These tests pin the two halves that a
//! correctness bug would show up in: the **results and position advances** are exactly
//! the always-re-read ones, and the allocation witness
//! ([`read_integer_field_tracked`]'s flag) agrees with the term the read actually built.
//!
//! A `MinimalContext` never relocates, so the elision's *soundness* (a stale capture
//! would be UB, not a wrong value) is not observable here; what is observable is that
//! taking the elided route changes nothing a caller can see.
#![allow(clippy::type_complexity)]

use super::super::BSF_SIGNED;
use super::match_ctx::read_ctx;
use super::reads::read_integer_field_tracked;
use crate::context::MinimalContext;
use crate::families::bitstrings::exec as family_exec;
use crate::families::bitstrings::raw;
use crate::interpreter::LabelMap;
use crate::machine::Machine;
use crate::terms::binary_bytes;
use crate::Step;
use beam_loader::{Instr, Label, Module, Operand, NO_FAIL};

/// The live fail label every `bs_match` below branches to (a live label, so a plain
/// match failure is distinguishable from a typed error).
const FAIL: Label = Label(1);

/// A machine, module and context whose `x(0)` holds `subject` and whose `x(1)` holds
/// an open match context over it.
///
/// `regs` must exceed the highest register the test writes.
fn fixture(subject: &[u8], bits: usize, regs: u32) -> (Machine, Module, MinimalContext) {
    let mut machine = Machine::with_max_register(regs);
    let mut ctx = MinimalContext::new();
    let mut module = Module::default();
    for name in [
        "no_fail",
        "ensure_at_least",
        "ensure_exactly",
        "skip",
        "integer",
        "binary",
        "get_tail",
        "=:=",
        "all",
    ] {
        module.atom_table.intern(name);
    }
    let subject = raw::make_bits(&mut machine, &mut ctx, subject, bits).expect("a bitstring");
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
    assert_eq!(step, Step::Next, "a bitstring subject enters a match");
    (machine, module, ctx)
}

/// Run one `bs_match` over the fixture's context (`x(1)`).
fn run_match(
    machine: &mut Machine,
    module: &Module,
    ctx: &mut MinimalContext,
    commands: Vec<Operand>,
) -> Step {
    family_exec(
        machine,
        module,
        &{
            let mut labels = LabelMap::new();
            labels.insert(FAIL, (0, 9));
            labels
        },
        &Instr::BsMatch {
            fail: FAIL,
            ctx: Operand::X(1),
            commands,
        },
        0,
        0,
        ctx,
    )
    .expect("bs_match reports no raise")
    .expect("the family claims bs_match")
}

/// The context's live position (`sb->start`).
fn position(machine: &Machine, ctx: &mut MinimalContext) -> usize {
    let (_, _, start, _) = read_ctx(machine, ctx, &Operand::X(1), "test").expect("a live context");
    start
}

fn atom(module: &Module, name: &str) -> Operand {
    Operand::Atom(
        module
            .atom_table
            .lookup(name)
            .unwrap_or_else(|| panic!("atom {name:?} interned by the fixture")),
    )
}

/// The hot shape — `ensure_at_least 8 1` then `integer … 8 1 Dst`, neither of which
/// allocates — runs off one capture and produces the always-re-read result: `Dst` is
/// the first byte and the context advanced by exactly 8 bits.
#[test]
fn a_non_allocating_command_list_elides_the_reread_and_advances_correctly() {
    let (mut machine, module, mut ctx) = fixture(&[0x01, 0x02, 0x03, 0x04], 32, 6);
    let step = run_match(
        &mut machine,
        &module,
        &mut ctx,
        vec![
            atom(&module, "ensure_at_least"),
            Operand::Integer(8),
            Operand::Integer(1),
            atom(&module, "integer"),
            Operand::Integer(0), // Live
            Operand::Nil,        // Flags (big-endian, unsigned)
            Operand::Integer(8), // Size
            Operand::Integer(1), // Unit
            Operand::X(2),       // Dst
        ],
    );
    assert_eq!(step, Step::Next, "the command list matches");
    let value = machine.x(2).expect("x(2) is bound");
    assert!(beam_term::is_small(value), "8 bits fit a small integer");
    assert_eq!(beam_term::signed_value(value), 0x01);
    assert_eq!(position(&machine, &mut ctx), 8, "the context advanced by 8 bits");

    // A second command list over the same (already advanced) context reads the next
    // byte: the elided capture is re-used within the list, never across one.
    let step = run_match(
        &mut machine,
        &module,
        &mut ctx,
        vec![
            atom(&module, "integer"),
            Operand::Integer(0),
            Operand::Nil,
            Operand::Integer(8),
            Operand::Integer(1),
            Operand::X(2),
        ],
    );
    assert_eq!(step, Step::Next);
    assert_eq!(beam_term::signed_value(machine.x(2).expect("x(2)")), 0x02);
    assert_eq!(position(&machine, &mut ctx), 16);
}

/// A bignum-producing `integer` command reports "allocated", so the commit re-reads
/// the context and the position still advances by the whole field.
#[test]
fn a_bignum_integer_command_still_commits_the_full_advance() {
    let subject = [0xFFu8; 20];
    let (mut machine, module, mut ctx) = fixture(&subject, subject.len() * 8, 6);
    let step = run_match(
        &mut machine,
        &module,
        &mut ctx,
        vec![
            atom(&module, "ensure_at_least"),
            Operand::Integer(128),
            Operand::Integer(1),
            atom(&module, "integer"),
            Operand::Integer(0),
            Operand::Nil,
            Operand::Integer(128), // Size: > 64 bits, so the read builds a bignum
            Operand::Integer(1),
            Operand::X(2),
        ],
    );
    assert_eq!(step, Step::Next);
    let value = machine.x(2).expect("x(2) is bound");
    assert!(
        !beam_term::is_small(value),
        "a 128-bit field does not fit a small integer, so the read allocated"
    );
    assert_eq!(
        position(&machine, &mut ctx),
        128,
        "the commit after the bignum read still advances the context by the whole field"
    );
}

/// A `binary` command binds the remaining bytes and advances the context; a
/// `get_tail` binds the tail (position unchanged) and may target the context's own
/// operand — the commit must happen **before** that binding.
#[test]
fn binary_and_get_tail_commands_keep_the_dirty_commit_and_its_order() {
    let (mut machine, module, mut ctx) = fixture(&[0x01, 0x02, 0x03, 0x04], 32, 6);
    let step = run_match(
        &mut machine,
        &module,
        &mut ctx,
        vec![
            atom(&module, "integer"),
            Operand::Integer(0),
            Operand::Nil,
            Operand::Integer(8),
            Operand::Integer(1),
            Operand::X(2),
            atom(&module, "get_tail"),
            Operand::Integer(0), // Live
            Operand::Integer(1), // Unit
            Operand::X(1),       // Dst == the context's own operand
        ],
    );
    assert_eq!(step, Step::Next, "the mixed list matches");
    assert_eq!(beam_term::signed_value(machine.x(2).expect("x(2)")), 0x01);
    let tail = machine.x(1).expect("x(1) is the tail now");
    assert_eq!(
        binary_bytes(tail).expect("a byte-aligned tail"),
        &[0x02u8, 0x03, 0x04],
        "get_tail binds the remaining bits even when Dst is the context operand"
    );

    // The same for a `binary` command (which always allocates): the advance lands in
    // the context, then Dst is bound.
    let (mut machine, module, mut ctx) = fixture(&[0x0A, 0x0B, 0x0C, 0x0D], 32, 6);
    let step = run_match(
        &mut machine,
        &module,
        &mut ctx,
        vec![
            atom(&module, "binary"),
            Operand::Integer(0), // Live
            Operand::Nil,        // Flags
            atom(&module, "all"), // Size
            Operand::Integer(1), // Unit
            Operand::X(2),       // Dst
        ],
    );
    assert_eq!(step, Step::Next);
    assert_eq!(
        binary_bytes(machine.x(2).expect("x(2)")).expect("a byte binary"),
        &[0x0Au8, 0x0B, 0x0C, 0x0D]
    );
    assert_eq!(position(&machine, &mut ctx), 32, "binary advances by its size");
}

/// `skip` and `=:=` allocate nothing, so they take the elided commit — and the
/// `=:=` comparison still consumes its bits.
#[test]
fn skip_and_eq_commands_elide_the_reread() {
    let (mut machine, module, mut ctx) = fixture(&[0xAB, 0xCD, 0xEF], 24, 6);
    let step = run_match(
        &mut machine,
        &module,
        &mut ctx,
        vec![
            atom(&module, "skip"),
            Operand::Integer(8),
            atom(&module, "=:="),
            Operand::Nil,
            Operand::Integer(8),
            Operand::Integer(0xCD),
        ],
    );
    assert_eq!(step, Step::Next, "skip 8 then `=:=` 16#CD over 8 bits");
    assert_eq!(position(&machine, &mut ctx), 16);
}

/// The allocation witness is exactly "the read built a non-small term": small fields
/// (of any width that fits, signed or not) report `false`, and a field that does not
/// fit reports `true`.
#[test]
fn the_allocation_witness_agrees_with_the_term_the_read_built() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();

    // 8 bits, unsigned: small.
    let (value, allocated) =
        read_integer_field_tracked(&mut machine, &mut ctx, &[0x7F], 0, 8, 0).expect("8-bit read");
    assert!(beam_term::is_small(value) && !allocated, "8 bits fit");

    // 8 bits, signed negative: still small (sign extension of a small field).
    let (value, allocated) =
        read_integer_field_tracked(&mut machine, &mut ctx, &[0x80], 0, 8, BSF_SIGNED)
            .expect("signed 8-bit read");
    assert!(beam_term::is_small(value) && !allocated, "a signed byte fits");
    assert_eq!(beam_term::signed_value(value), -128);

    // 56 bits, all ones: 2^56 - 1 still fits a small integer (60-bit immediate).
    let (value, allocated) =
        read_integer_field_tracked(&mut machine, &mut ctx, &[0xFF; 7], 0, 56, 0)
            .expect("56-bit read");
    assert!(beam_term::is_small(value) && !allocated, "2^56 - 1 fits");

    // 64 bits, all ones: a bignum, so the read reserved through the rooted seam.
    let (value, allocated) =
        read_integer_field_tracked(&mut machine, &mut ctx, &[0xFF; 8], 0, 64, 0)
            .expect("64-bit read");
    assert!(
        !beam_term::is_small(value) && allocated,
        "2^64 - 1 needs a bignum, which is the one allocating arm"
    );
}
