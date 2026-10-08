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

//! `is_record_accessible` (opcode 189).
//!
//! The `scope` operand selects the two accessibility rules: the atom `external`
//! is the strict `erl_is_record_accessible` (the definition must be exported),
//! any other atom is the `$MODULE` wildcard
//! `erl_is_wildcard_record_accessible` (exported **or** created in that module).

use crate::interpreter::LabelMap;

use super::*;

#[test]
fn is_record_accessible_external_scope_accepts_an_exported_record() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (m, r, a, external) = (
        fx.atom("m"),
        fx.atom("r"),
        fx.atom("a"),
        fx.atom("external"),
    );
    let rec = record(&mut ctx, m, r, true, &[(a, make_small(1))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &Instr::IsRecordAccessible {
            fail: NO_FAIL,
            src: Operand::X(0),
            scope: Operand::Atom(external),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
}

#[test]
fn is_record_accessible_external_scope_rejects_a_non_exported_record() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (m, r, a, external) = (
        fx.atom("m"),
        fx.atom("r"),
        fx.atom("a"),
        fx.atom("external"),
    );
    let rec = record(&mut ctx, m, r, false, &[(a, make_small(1))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::IsRecordAccessible {
            fail: Label(7),
            src: Operand::X(0),
            scope: Operand::Atom(external),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn is_record_accessible_wildcard_scope_accepts_a_record_created_in_that_module() {
    // Any scope atom other than `external` is the `$MODULE` wildcard: exported
    // **or** created in that module.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (m, r, a) = (fx.atom("m"), fx.atom("r"), fx.atom("a"));
    let rec = record(&mut ctx, m, r, false, &[(a, make_small(1))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &Instr::IsRecordAccessible {
            fail: NO_FAIL,
            src: Operand::X(0),
            scope: Operand::Atom(m),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
}

#[test]
fn is_record_accessible_wildcard_scope_rejects_a_record_from_another_module() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (m, r, a, other) = (fx.atom("m"), fx.atom("r"), fx.atom("a"), fx.atom("other"));
    // Created in `other`, not exported.
    let rec = record(&mut ctx, other, r, false, &[(a, make_small(1))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::IsRecordAccessible {
            fail: Label(7),
            src: Operand::X(0),
            scope: Operand::Atom(m),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn is_record_accessible_with_no_fail_is_a_capability_gap() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (m, r, a, external) = (
        fx.atom("m"),
        fx.atom("r"),
        fx.atom("a"),
        fx.atom("external"),
    );
    let rec = record(&mut ctx, m, r, false, &[(a, make_small(1))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let reason = unsupported_reason(run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &Instr::IsRecordAccessible {
            fail: NO_FAIL,
            src: Operand::X(0),
            scope: Operand::Atom(external),
        },
        &mut ctx,
    ));
    assert_eq!(reason, "unsupported_opcode: is_record_accessible/no_fail");
}
