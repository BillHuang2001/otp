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

//! `is_native_record` (opcode 186) — the definition-identity test.
//!
//! `Instr::IsNativeRecord` is `erl_is_native_record`: it succeeds iff the
//! record's own definition carries the two operand atoms as its `module`/`name`,
//! and fails (or reports the `NO_FAIL` capability gap) otherwise.

use crate::interpreter::LabelMap;

use super::*;

#[test]
fn is_native_record_succeeds_on_a_matching_definition() {
    let mut fx = Fixture::new();
    let (m, r, a) = (fx.atom("m"), fx.atom("r"), fx.atom("a"));
    let mut ctx = MinimalContext::new();
    let rec = record(&mut ctx, m, r, false, &[(a, make_small(1))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &Instr::IsNativeRecord {
            fail: NO_FAIL,
            src: Operand::X(0),
            module: Operand::Atom(m),
            name: Operand::Atom(r),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
}

#[test]
fn is_native_record_branches_to_fail_on_a_module_mismatch() {
    let mut fx = Fixture::new();
    let (m, r, a, other) = (fx.atom("m"), fx.atom("r"), fx.atom("a"), fx.atom("other"));
    let mut ctx = MinimalContext::new();
    let rec = record(&mut ctx, m, r, false, &[(a, make_small(1))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    // The record lives in `m`; asking for it in `other` is `erl_is_native_record`
    // == false ⇒ `$FAIL($Fail)`.
    let step = run(
        fx.module(),
        &labels_with(Label(7), 2, 9),
        &mut machine,
        &Instr::IsNativeRecord {
            fail: Label(7),
            src: Operand::X(0),
            module: Operand::Atom(other),
            name: Operand::Atom(r),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 2, pc: 9 }));
}

#[test]
fn is_native_record_branches_to_fail_on_a_name_mismatch() {
    let mut fx = Fixture::new();
    let (m, r, s, a) = (fx.atom("m"), fx.atom("r"), fx.atom("s"), fx.atom("a"));
    let mut ctx = MinimalContext::new();
    let rec = record(&mut ctx, m, r, false, &[(a, make_small(1))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::IsNativeRecord {
            fail: Label(7),
            src: Operand::X(0),
            module: Operand::Atom(m),
            name: Operand::Atom(s),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn is_native_record_branches_to_fail_on_a_non_record() {
    let mut fx = Fixture::new();
    let (m, r) = (fx.atom("m"), fx.atom("r"));
    let mut ctx = MinimalContext::new();
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, make_small(5)).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::IsNativeRecord {
            fail: Label(7),
            src: Operand::X(0),
            module: Operand::Atom(m),
            name: Operand::Atom(r),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn is_native_record_with_no_fail_is_a_capability_gap() {
    let mut fx = Fixture::new();
    let (m, r, other) = (fx.atom("m"), fx.atom("r"), fx.atom("other"));
    let mut ctx = MinimalContext::new();
    let rec = record(&mut ctx, m, r, false, &[]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let reason = unsupported_reason(run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &Instr::IsNativeRecord {
            fail: NO_FAIL,
            src: Operand::X(0),
            module: Operand::Atom(other),
            name: Operand::Atom(r),
        },
        &mut ctx,
    ));
    assert_eq!(reason, "unsupported_opcode: is_native_record/no_fail");
}

#[test]
fn is_native_record_reads_the_module_and_name_operands_as_atoms() {
    // A register `module` operand is not a compile-time atom: the family reports it
    // rather than reading an arbitrary register as an atom.
    let mut fx = Fixture::new();
    let (m, r) = (fx.atom("m"), fx.atom("r"));
    let mut ctx = MinimalContext::new();
    let rec = record(&mut ctx, m, r, false, &[]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();
    machine.set_x(1, make_small(3)).unwrap();

    let reason = unsupported_reason(run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &Instr::IsNativeRecord {
            fail: NO_FAIL,
            src: Operand::X(0),
            module: Operand::X(1),
            name: Operand::Atom(r),
        },
        &mut ctx,
    ));
    assert_eq!(reason, "unsupported_opcode: is_native_record: module");
}
