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

//! **Moving-collector regression tests for the allocating bitstring reads**
//! (Handoff #11).
//!
//! [`BsGetInteger2`]/[`BsGetBinary2`]/[`BsGetFloat2`] — and the allocating commands of
//! the flattened [`bs_match`](run_commands) interpreter — read their match context,
//! take an **allocating** field read (`raw::make_big`, `raw::make_binary`,
//! `crate::terms::make_float` — each a rooted reservation), and then commit the
//! advanced position back into the context. Because the reserve can run a **moving**
//! collector
//! ([`ExecutionContext::alloc_words_rooted`](crate::context::ExecutionContext::alloc_words_rooted))
//! that relocates every block reachable from the machine's root set and rewrites
//! `machine.x`/`stack`/`fr` in place, the context read *before* the allocation is a
//! stale pointer afterwards — so each opcode re-reads the context operand
//! (`reread_ctx`) before the `set_ctx_start`. It also re-reads it **before** it binds
//! the read's destination: the C advances `sb->start` inside the read
//! (`i_bs_get_fixed_integer`/`i_bs_get_fixed_binary`/`i_bs_read_bits`) *before*
//! `$Dst = result`, and a destination may be the register the context lives in
//! (`bs_match`'s `get_tail` binds the tail into it).
//!
//! These tests drive all three opcodes through
//! [`exec`](super::exec) over the crate's mock semi-space collector
//! ([`MovingContext`](crate::terms::alloc_tests::MovingContext)) and assert:
//!
//! 1. the collector ran and **relocated the context block**
//!    ([`MovingContext::moved_any`](crate::terms::alloc_tests::MovingContext::moved_any)
//!    plus a raw-address comparison) — the allocating read really did collect;
//! 2. the returned value is correct, so the field was read from the (moved) subject;
//! 3. the **advanced position is committed to the moved context** — a write into the
//!    stale pre-collection block would leave the machine's context at position 0 and
//!    fail this assertion.
//!
//! [`bs_match_commits_each_advance_before_it_binds_the_context_register`] is the same
//! discipline for the flattened command interpreter, in the shape the compiler emits
//! for `coverage:bits/1`: an allocating `integer` command followed by a `get_tail` that
//! binds the tail into the context's own register.
//!
//! The fixtures are hand-built devices: a heap binary and its match context carved
//! into the moving region through the family's own `raw` writers, and a hand-built
//! [`Instr`] driven straight through `exec`. No loader, no oracle.

use beam_loader::{BsGetArgs, Instr, Module, Operand, NO_FAIL};
use beam_term::Eterm;

use crate::families::bitstrings::raw;
use crate::interpreter::LabelMap;
use crate::machine::Machine;
use crate::terms::alloc_tests::MovingContext;
use crate::terms::{binary_bytes, float_value};
use crate::Step;

use super::{enter_match, exec, run_commands};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Build a heap binary over `bytes` in the moving region, enter a match context over
/// it, and leave the **context** rooted at `x(0)` (the binary stays reachable through
/// the context's `orig` word, which the collector's transitive closure keeps live).
fn context_over(machine: &mut Machine, ctx: &mut MovingContext, bytes: &[u8]) -> Eterm {
    let bits = bytes.len() * 8;
    let subject = raw::make_bits(machine, ctx, bytes, bits).expect("heap binary");
    machine.set_x(0, subject).expect("root the subject");
    let context = enter_match(machine, ctx, &Operand::X(0))
        .expect("no raise")
        .expect("a bitstring subject enters a match");
    machine.set_x(0, context).expect("root the context");
    context
}

/// Drive one bitstring-matching instruction, asserting the family claims it and
/// reports no raise.
fn drive(machine: &mut Machine, instr: &Instr, ctx: &mut MovingContext) -> Step {
    let module = Module::default();
    exec(machine, &module, &LabelMap::new(), instr, 0, 0, ctx)
        .expect("the bitstrings family reports no raise")
        .expect("the bitstrings family claims the variant")
}

/// The `(start, end)` bit position of the match context rooted at `x(0)`.
fn position(machine: &Machine) -> (usize, usize) {
    let (_, start, end) =
        raw::ctx_parts(machine.x(0).expect("the context root")).expect("a match context");
    (start, end)
}

/// The `BsGetArgs` of a `Sz * Unit`-bit field read into `x(1)` from the context at
/// `x(0)`.
fn read_args(size: i64, unit: u32) -> BsGetArgs {
    BsGetArgs {
        fail: NO_FAIL,
        ctx: Operand::X(0),
        live: 1,
        size: Operand::Integer(size),
        unit,
        flags: 0,
        dst: Operand::X(1),
    }
}

// ---------------------------------------------------------------------------
// bs_get_integer2
// ---------------------------------------------------------------------------

/// A 64-bit field whose value needs a bignum makes the read allocate; the advanced
/// position must land in the **relocated** context.
#[test]
fn bs_get_integer2_commits_the_advanced_position_to_the_moved_context() {
    let mut machine = Machine::with_max_register(4);
    let mut ctx = MovingContext::new();

    // `2^60` big-endian — outside the 60-bit small range, so the read builds a bignum
    // through `raw::make_big` (a rooted reservation that collects).
    context_over(&mut machine, &mut ctx, &[0x10, 0, 0, 0, 0, 0, 0, 0]);
    let before = machine.x(0).expect("the context root").raw();

    let step = drive(
        &mut machine,
        &Instr::BsGetInteger2(Box::new(read_args(64, 1))),
        &mut ctx,
    );
    assert_eq!(step, Step::Next);

    // (1) The bignum read collected and relocated the context block.
    assert!(ctx.moved_any, "the mock collector must have relocated a block");
    assert!(ctx.collections >= 1, "a collection must have run");
    assert_ne!(
        machine.x(0).expect("the context root").raw(),
        before,
        "the allocating read must have moved the context"
    );

    // (2) The value is the exact bignum `2^60`.
    let (negative, digits) = raw::big_parts_safe(machine.x(1).expect("the value")).expect("a bignum");
    assert!(!negative, "the field is unsigned");
    assert_eq!(digits, vec![1u64 << 60]);

    // (3) The advanced position was written into the MOVED context (a write into the
    // stale pre-collection block would leave `start` at 0).
    assert_eq!(position(&machine), (64, 64));
}

/// A field that fits a small integer allocates nothing (no collection), and the
/// advanced position is still committed on the very same context block.
#[test]
fn bs_get_integer2_commits_the_advanced_position_when_it_does_not_collect() {
    let mut machine = Machine::with_max_register(4);
    let mut ctx = MovingContext::new();

    // `1` as a 64-bit big-endian field: a small integer, so no rooted allocation runs
    // and the context cannot have moved.
    context_over(&mut machine, &mut ctx, &[0, 0, 0, 0, 0, 0, 0, 1]);
    let before = machine.x(0).expect("the context root").raw();

    let step = drive(
        &mut machine,
        &Instr::BsGetInteger2(Box::new(read_args(64, 1))),
        &mut ctx,
    );
    assert_eq!(step, Step::Next);
    assert_eq!(machine.x(1).expect("the value"), beam_term::make_small(1));
    assert_eq!(
        machine.x(0).expect("the context root").raw(),
        before,
        "a small-integer read allocates nothing, so nothing could move"
    );
    assert_eq!(position(&machine), (64, 64));
}

// ---------------------------------------------------------------------------
// bs_get_float2
// ---------------------------------------------------------------------------

/// `make_float` reserves through the rooted seam, so the float read collects.
#[test]
fn bs_get_float2_commits_the_advanced_position_to_the_moved_context() {
    let mut machine = Machine::with_max_register(4);
    let mut ctx = MovingContext::new();

    // `1.5` as an IEEE-754 binary64 big-endian field.
    context_over(&mut machine, &mut ctx, &[0x3F, 0xF8, 0, 0, 0, 0, 0, 0]);
    let before = machine.x(0).expect("the context root").raw();

    let step = drive(
        &mut machine,
        &Instr::BsGetFloat2(Box::new(read_args(64, 1))),
        &mut ctx,
    );
    assert_eq!(step, Step::Next);

    assert!(ctx.moved_any, "the mock collector must have relocated a block");
    assert!(ctx.collections >= 1, "a collection must have run");
    assert_ne!(
        machine.x(0).expect("the context root").raw(),
        before,
        "the allocating float read must have moved the context"
    );

    assert_eq!(
        float_value(machine.x(1).expect("the value")).expect("a float"),
        1.5
    );
    assert_eq!(position(&machine), (64, 64));
}

// ---------------------------------------------------------------------------
// bs_get_binary2
// ---------------------------------------------------------------------------

/// `make_binary` reserves through the rooted seam, so the binary read collects.
#[test]
fn bs_get_binary2_commits_the_advanced_position_to_the_moved_context() {
    let mut machine = Machine::with_max_register(4);
    let mut ctx = MovingContext::new();

    context_over(&mut machine, &mut ctx, &[1, 2, 3, 4]);
    let before = machine.x(0).expect("the context root").raw();

    let step = drive(
        &mut machine,
        &Instr::BsGetBinary2(Box::new(read_args(32, 1))),
        &mut ctx,
    );
    assert_eq!(step, Step::Next);

    assert!(ctx.moved_any, "the mock collector must have relocated a block");
    assert!(ctx.collections >= 1, "a collection must have run");
    assert_ne!(
        machine.x(0).expect("the context root").raw(),
        before,
        "the allocating binary read must have moved the context"
    );

    assert_eq!(
        binary_bytes(machine.x(1).expect("the value")).expect("a binary"),
        &[1u8, 2, 3, 4][..]
    );
    assert_eq!(position(&machine), (32, 32));
}

// ---------------------------------------------------------------------------
// bs_match — the flattened command interpreter
// ---------------------------------------------------------------------------

/// The `integer`/`get_tail` command list `coverage:bits/1` compiles to, driven under
/// the mock moving collector.
///
/// The `integer` command reads a 64-bit field, so its rooted reservation builds a
/// bignum and collects, relocating the context; the `get_tail` that follows binds the
/// tail **into the register the context lives in** (`bits/1`'s `get_tail … {x,1}`).
/// That is why every advancing command must mirror its advance into the context as it
/// runs: a single write-back after the loop would have to re-read a register a command
/// had already overwritten — the typed `bs_match: operand is not a match context`
/// raised from [`read_ctx`](super::match_ctx::read_ctx), which is exactly the failure
/// this test pins.
#[test]
fn bs_match_commits_each_advance_before_it_binds_the_context_register() {
    let mut machine = Machine::with_max_register(4);
    let mut ctx = MovingContext::new();

    // `2^60` (a bignum read) followed by the distinctive tail `<<1,…,8>>`.
    let bytes = [0x10, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
    let subject = raw::make_bits(&mut machine, &mut ctx, &bytes, 128).expect("heap binary");
    machine.set_x(0, subject).expect("root the subject");
    let context = enter_match(&mut machine, &mut ctx, &Operand::X(0))
        .expect("no raise")
        .expect("a bitstring subject enters a match");
    // Root the context at `x(0)` (the operand `bs_match` reads) *and* at `x(2)`, so its
    // committed position stays observable after `get_tail` overwrites `x(0)`.
    machine.set_x(0, context).expect("root the context");
    machine.set_x(2, context).expect("a second root for the context");

    let mut module = Module::default();
    let integer = module.atom_table.intern("integer");
    let get_tail = module.atom_table.intern("get_tail");
    let commands = [
        // `integer Live Flags Size Unit Dst` — the leading 64-bit field into `x(1)`.
        Operand::Atom(integer),
        Operand::Integer(1), // Live
        Operand::Nil,        // Flags (no `little`/`signed`/`native`)
        Operand::Integer(64),
        Operand::Integer(1),
        Operand::X(1), // Dst
        // `get_tail Live Unit Dst` — the remaining bits into the **context's** register.
        Operand::Atom(get_tail),
        Operand::Integer(1), // Live
        Operand::Integer(8), // Unit
        Operand::X(0),       // Dst
    ];

    // A run that reaches `Step::Next` at all is the first half of the witness: the
    // buggy single trailing write-back re-read the overwritten `x(0)` here and raised
    // `unsupported_opcode: bs_match: operand is not a match context`.
    let step = run_commands(
        &mut machine,
        &module,
        &LabelMap::new(),
        NO_FAIL,
        &Operand::X(0),
        &commands,
        &mut ctx,
    )
    .expect("the flattened command list reports no raise")
    .expect("the flattened command list reports a step");
    assert_eq!(step, Step::Next);

    // (1) Both allocating commands ran the collector, so the context was relocated
    // while the list was executing. (The mock collector alternates two fixed regions,
    // so the *address* returns to its starting value after the even number of
    // collections this list performs — the committed position below is the reliable
    // witness that the write landed in the relocated block.)
    assert!(ctx.moved_any, "the mock collector must have relocated a block");
    assert!(ctx.collections >= 1, "a collection must have run");

    // (2) The field and the tail were read from the (moved) subject.
    let (negative, digits) =
        raw::big_parts_safe(machine.x(1).expect("the field")).expect("a bignum");
    assert!(!negative, "the field is unsigned");
    assert_eq!(digits, vec![1u64 << 60]);
    assert_eq!(
        binary_bytes(machine.x(0).expect("the tail")).expect("a binary"),
        &[1u8, 2, 3, 4, 5, 6, 7, 8][..]
    );

    // (3) The advanced position was mirrored into the context as the `integer` command
    // ran — via the second root, which `get_tail` did not overwrite. A write into the
    // stale pre-collection block would leave `start` at 0 here.
    let (_, start, end) =
        raw::ctx_parts(machine.x(2).expect("the second context root")).expect("a match context");
    assert_eq!((start, end), (64, 128));
}
