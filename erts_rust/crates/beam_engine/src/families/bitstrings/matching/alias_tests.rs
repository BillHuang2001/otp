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

//! Regression tests for **matched-out sub-binary aliasing**.
//!
//! When a match reads a window above [`beam_term::refc::ERL_ONHEAP_BITS_LIMIT`]
//! (512 bits) out of an off-heap (refc) subject, the result must be a sub-binary
//! naming the **parent's** payload — the C `erts_build_sub_bitstring` window arm —
//! so `binary:referenced_byte_size/1` of the result answers the parent's referenced
//! size, not the window's own byte size. At/below the limit, or over an on-heap
//! subject, the result stays a byte-faithful **copy** (its own size).
//!
//! The oracle figures are the system `erl`'s own answers for the same shapes; the
//! engine-side assertion is the structural identity: the result is a refc binary
//! whose `BinRef.val` (`beam_term::refc::refc_payload`) equals the subject's.
#![allow(unsafe_code)]

use beam_loader::{load_file, BsGetArgs, Instr, Module, Operand, NO_FAIL};
use beam_term::Eterm;

use super::super::run_tests::{bitstring, compile_source, erl_eval, oracle_export, refc_window};
use super::super::{exec as family_exec, raw};
use crate::context::MinimalContext;
use crate::interpreter::LabelMap;
use crate::machine::Machine;
use crate::Step;

/// The off-heap payload data pointer a refc binary's `BinRef.val` names.
fn payload_ptr(t: Eterm) -> *mut Eterm {
    // SAFETY: `t` is a live refc binary the engine just produced; `refc_payload`
    // dereferences only its header and `orig` slot.
    unsafe { beam_term::refc::refc_payload(t) }.expect("a refc binary")
}

/// Open a match context over the subject rooted in `x(0)`, into `x(1)`.
fn start_match(machine: &mut Machine, module: &Module, ctx: &mut MinimalContext) {
    let step = family_exec(
        machine,
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
        ctx,
    )
    .expect("bs_start_match3 reports no raise")
    .expect("the family claims bs_start_match3");
    assert_eq!(step, Step::Next);
}

/// Root a `len`-byte refc subject in `x(0)` and return it.
fn refc_subject(machine: &mut Machine, ctx: &mut MinimalContext, len: usize) -> Eterm {
    let payload = vec![1u8; len];
    let subject = raw::make_binary(machine, ctx, &payload, len * 8).expect("a refc subject");
    assert!(raw::is_refc_binary(subject));
    machine.set_x(0, subject).expect("root the subject");
    subject
}

/// `i_bs_get_tail` over a >64-byte refc subject aliases the parent's payload.
#[test]
fn bs_get_tail_over_a_refc_subject_aliases_the_parent_payload() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let module = Module::default();
    let subject = refc_subject(&mut machine, &mut ctx, 1000);
    start_match(&mut machine, &module, &mut ctx);

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

    let result = machine.x(0).expect("the matched tail");
    assert!(raw::is_refc_binary(result), "a 1000-byte tail is off-heap");
    assert_eq!(
        payload_ptr(result),
        payload_ptr(subject),
        "the tail shares the subject's payload"
    );
    assert_eq!(bitstring(result).1, 8000, "the whole 1000-byte subject");
}

/// `i_bs_get_binary_2` over a >64-byte refc subject aliases the parent's payload,
/// at a **non-zero** match position (`<<_:8, C:1023/binary>>`).
#[test]
fn bs_get_binary2_over_a_refc_subject_aliases_the_parent_payload() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let module = Module::default();
    let subject = refc_subject(&mut machine, &mut ctx, 1024);
    start_match(&mut machine, &module, &mut ctx);

    let step = family_exec(
        &mut machine,
        &module,
        &LabelMap::new(),
        &Instr::BsSkipBits2 {
            fail: NO_FAIL,
            ctx: Operand::X(1),
            size: Operand::Integer(8),
            unit: 1,
            flags: 0,
        },
        0,
        0,
        &mut ctx,
    )
    .expect("bs_skip_bits2 reports no raise")
    .expect("the family claims bs_skip_bits2");
    assert_eq!(step, Step::Next);

    let step = family_exec(
        &mut machine,
        &module,
        &LabelMap::new(),
        &Instr::BsGetBinary2(Box::new(BsGetArgs {
            fail: NO_FAIL,
            ctx: Operand::X(1),
            live: 2,
            size: Operand::Integer(1023),
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
    assert!(raw::is_refc_binary(result), "a 1023-byte window is off-heap");
    assert_eq!(
        payload_ptr(result),
        payload_ptr(subject),
        "the window shares the subject's payload"
    );
    assert_eq!(bitstring(result).1, 1023 * 8, "the 1023-byte window");
}

/// The `bs_match` `binary` command over a >64-byte refc subject aliases the payload.
#[test]
fn bs_match_binary_over_a_refc_subject_aliases_the_parent_payload() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let mut module = Module::default();
    let binary = Operand::Atom(module.atom_table.intern("binary"));
    let all = Operand::Atom(module.atom_table.intern("all"));
    let subject = refc_subject(&mut machine, &mut ctx, 1000);
    start_match(&mut machine, &module, &mut ctx);

    let step = family_exec(
        &mut machine,
        &module,
        &LabelMap::new(),
        &Instr::BsMatch {
            fail: NO_FAIL,
            ctx: Operand::X(1),
            commands: vec![
                binary,
                Operand::Integer(2), // Live
                Operand::Nil,        // Flags
                all,                 // Size = all
                Operand::Integer(1), // Unit
                Operand::X(0),       // Dst
            ],
        },
        0,
        0,
        &mut ctx,
    )
    .expect("bs_match reports no raise")
    .expect("the family claims bs_match");
    assert_eq!(step, Step::Next);

    let result = machine.x(0).expect("the matched window");
    assert!(raw::is_refc_binary(result), "a 1000-byte `all` window is off-heap");
    assert_eq!(
        payload_ptr(result),
        payload_ptr(subject),
        "the `binary` window shares the subject's payload"
    );
    assert_eq!(bitstring(result).1, 8000, "`all` reads the whole subject");
}

/// The `bs_match` `get_tail` command over a >64-byte refc subject aliases the payload.
#[test]
fn bs_match_get_tail_over_a_refc_subject_aliases_the_parent_payload() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let mut module = Module::default();
    let get_tail = Operand::Atom(module.atom_table.intern("get_tail"));
    let subject = refc_subject(&mut machine, &mut ctx, 1000);
    start_match(&mut machine, &module, &mut ctx);

    let step = family_exec(
        &mut machine,
        &module,
        &LabelMap::new(),
        &Instr::BsMatch {
            fail: NO_FAIL,
            ctx: Operand::X(1),
            commands: vec![
                get_tail,
                Operand::Integer(2), // Live
                Operand::Integer(1), // Unit
                Operand::X(0),       // Dst
            ],
        },
        0,
        0,
        &mut ctx,
    )
    .expect("bs_match reports no raise")
    .expect("the family claims bs_match");
    assert_eq!(step, Step::Next);

    let result = machine.x(0).expect("the matched tail");
    assert!(raw::is_refc_binary(result), "a 1000-byte tail is off-heap");
    assert_eq!(
        payload_ptr(result),
        payload_ptr(subject),
        "the `get_tail` result shares the subject's payload"
    );
    assert_eq!(bitstring(result).1, 8000, "the whole 1000-byte subject");
}

/// A matched-out tail over an on-heap (<=64-byte) subject still **copies**.
#[test]
fn bs_get_tail_over_an_on_heap_subject_copies() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let module = Module::default();
    let subject = raw::make_binary(&mut machine, &mut ctx, &[1u8; 20], 20 * 8)
        .expect("a 20-byte heap subject");
    assert!(!raw::is_refc_binary(subject), "20 bytes stays on the heap");
    machine.set_x(0, subject).expect("root the subject");
    start_match(&mut machine, &module, &mut ctx);

    let step = family_exec(
        &mut machine,
        &module,
        &LabelMap::new(),
        &Instr::BsSkipBits2 {
            fail: NO_FAIL,
            ctx: Operand::X(1),
            size: Operand::Integer(8),
            unit: 1,
            flags: 0,
        },
        0,
        0,
        &mut ctx,
    )
    .expect("bs_skip_bits2 reports no raise")
    .expect("the family claims bs_skip_bits2");
    assert_eq!(step, Step::Next);

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

    let result = machine.x(0).expect("the matched tail");
    assert!(
        !raw::is_refc_binary(result),
        "a 19-byte tail over a heap subject is an on-heap copy"
    );
    assert_eq!(bitstring(result).1, 19 * 8, "the 19-byte tail");
}

/// The aliasing is correct for a subject whose own window is **not byte-aligned**:
/// the child's window offset is the subject's window offset plus the match position,
/// never the sub-byte remainder of `subject_bytes`.
#[test]
fn bs_get_tail_over_a_non_byte_aligned_refc_subject_aliases_the_parent_payload() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let module = Module::default();
    // A 1000-byte payload read through a window of bits `1..1+7999`.
    let payload: Vec<u8> = (0u8..=255u8).cycle().take(1000).collect();
    let (subject, _backing) = refc_window(&mut ctx, &payload, 1, 7999);
    assert!(raw::is_refc_binary(subject));
    machine.set_x(0, subject).expect("root the subject");
    start_match(&mut machine, &module, &mut ctx);

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

    let result = machine.x(0).expect("the matched tail");
    assert!(raw::is_refc_binary(result));
    assert_eq!(
        payload_ptr(result),
        payload_ptr(subject),
        "the tail shares the subject's payload"
    );
    assert_eq!(
        bitstring(result),
        bitstring(subject),
        "the tail is bit-exactly the subject's own (non-byte-aligned) window"
    );
}

/// The oracle cross-check for the objective's two shapes, plus the engine-side
/// aliasing over the same compiled functions. Skips loudly without `erlc`/`erl`.
#[test]
fn the_oracle_reports_the_parents_referenced_size_and_the_engine_aliases() {
    const ALIAS_SRC: &str = "\
-module(bitsalias).
-export([binary2/1, tail/1, match_bin/1, match_rest/1, onheap_tail/1]).

binary2(Bin) ->
    <<_:8, C:1023/binary>> = Bin,
    C.

tail(Bin) ->
    <<_:8, C/binary>> = Bin,
    C.

match_bin(Bin) ->
    <<B:100/binary, _/binary>> = Bin,
    B.

match_rest(Bin) ->
    <<B:10/binary, C/binary>> = Bin,
    C.

onheap_tail(Bin) ->
    <<_:8, C/binary>> = Bin,
    C.
";
    let Some(dir) = compile_source("alias", "bitsalias", ALIAS_SRC) else {
        eprintln!("SKIP: no usable `erlc` on PATH -- sub-binary aliasing cross-check skipped");
        return;
    };
    let module = load_file(dir.join("bitsalias.beam"))
        .unwrap_or_else(|e| panic!("load bitsalias.beam: {e:?}"));

    // The system `erl`'s own answers for the exact objective shapes.
    let eval1 = "\
Bin = <<0:(1024*8)>>, <<_:8, C:1023/binary>> = Bin, \
io:format(\"~p\", [binary:referenced_byte_size(C)]), halt(0).";
    match erl_eval(&dir, eval1) {
        Some(out) => assert_eq!(
            out.trim(),
            "1024",
            "the oracle answers the parent's referenced size for `<<_:8, C:1023/binary>>`"
        ),
        None => eprintln!("SKIP: no system `erl` on PATH -- referenced_byte_size oracle skipped"),
    }
    let eval2 = "\
Bin = binary:copy(<<1>>,1000), <<B:10/binary, C/binary>> = Bin, \
io:format(\"~p\", [{byte_size(C), binary:referenced_byte_size(C)}]), halt(0).";
    match erl_eval(&dir, eval2) {
        Some(out) => assert_eq!(
            out.trim(),
            "{990,1000}",
            "the oracle answers the pair (990,1000) for `<<B:10/binary, C/binary>>` over 1000 bytes"
        ),
        None => eprintln!(
            "SKIP: no system `erl` on PATH -- byte_size/referenced_byte_size oracle skipped"
        ),
    }

    // `binary2/1`: bs_skip_bits2 + bs_get_binary2 over a 1024-byte refc subject.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let subject = refc_subject(&mut machine, &mut ctx, 1024);
    let result = machine
        .run_export(&module, oracle_export(&module, "binary2"), 1, &[subject], &mut ctx)
        .expect("bitsalias:binary2/1");
    assert!(raw::is_refc_binary(result));
    assert_eq!(payload_ptr(result), payload_ptr(subject));
    assert_eq!(bitstring(result).1, 1023 * 8);

    // `tail/1`: bs_skip_bits2 + bs_get_tail over a 1024-byte refc subject.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let subject = refc_subject(&mut machine, &mut ctx, 1024);
    let result = machine
        .run_export(&module, oracle_export(&module, "tail"), 1, &[subject], &mut ctx)
        .expect("bitsalias:tail/1");
    assert!(raw::is_refc_binary(result));
    assert_eq!(payload_ptr(result), payload_ptr(subject));
    assert_eq!(bitstring(result).1, (1024 - 1) * 8);

    // `match_rest/1`: bs_match `binary` (10 bytes) + `get_tail` over a 1000-byte subject.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let subject = refc_subject(&mut machine, &mut ctx, 1000);
    let result = machine
        .run_export(&module, oracle_export(&module, "match_rest"), 1, &[subject], &mut ctx)
        .expect("bitsalias:match_rest/1");
    assert!(raw::is_refc_binary(result));
    assert_eq!(payload_ptr(result), payload_ptr(subject));
    assert_eq!(bitstring(result).1, 990 * 8);

    // `match_bin/1`: bs_match `binary` (100 bytes, >512 bits) over a 1000-byte subject.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let subject = refc_subject(&mut machine, &mut ctx, 1000);
    let result = machine
        .run_export(&module, oracle_export(&module, "match_bin"), 1, &[subject], &mut ctx)
        .expect("bitsalias:match_bin/1");
    assert!(raw::is_refc_binary(result));
    assert_eq!(payload_ptr(result), payload_ptr(subject));
    assert_eq!(bitstring(result).1, 100 * 8);

    // `onheap_tail/1`: bs_skip_bits2 + bs_get_tail over a 10-byte on-heap subject.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let subject = raw::make_binary(&mut machine, &mut ctx, &[1u8; 10], 10 * 8)
        .expect("a 10-byte heap subject");
    let result = machine
        .run_export(&module, oracle_export(&module, "onheap_tail"), 1, &[subject], &mut ctx)
        .expect("bitsalias:onheap_tail/1");
    assert!(!raw::is_refc_binary(result), "a 9-byte on-heap tail is a copy");
    assert_eq!(bitstring(result).1, 9 * 8);
}
