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
    // Any scope atom other than `external` selects the `$MODULE` wildcard: exported
    // **or** created in the **running** module (`emu/ops.tab:1677` substitutes
    // `$MODULE`, not the scope operand).
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
fn is_record_accessible_auto_local_scope_accepts_a_non_exported_record_of_the_running_module() {
    // `auto_local` is the scope atom the real compiler writes for a record that is
    // used locally (`lib/compiler/src/beam_core_to_ssa.erl:904-909`). The C's test is
    // `defp->is_exported == am_true || defp->module == <running module>`
    // (`erl_record.c:773-780`), so a NON-exported record defined in the running
    // module is accessible — comparing the definition's module against the *scope*
    // atom would never match, because `auto_local` is not a module name.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (m, r, a) = (fx.atom("m"), fx.atom("r"), fx.atom("a"));
    let auto_local = fx.atom("auto_local");
    assert_ne!(auto_local, m, "the scope atom is not the module's name");
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
            scope: Operand::Atom(auto_local),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
}

#[test]
fn is_record_accessible_auto_local_scope_rejects_a_record_of_another_module() {
    // The same `auto_local` scope, but the definition belongs to another module and
    // is not exported: the wildcard's `defp->module == <running module>` half fails
    // too, so the guard branches.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (r, a, other) = (fx.atom("r"), fx.atom("a"), fx.atom("other"));
    let auto_local = fx.atom("auto_local");
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
            scope: Operand::Atom(auto_local),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
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
fn is_record_accessible_with_no_fail_raises_function_clause() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    fx.atom("function_clause");
    let (m, r, a, external) = (
        fx.atom("m"),
        fx.atom("r"),
        fx.atom("a"),
        fx.atom("external"),
    );
    let rec = record(&mut ctx, m, r, false, &[(a, make_small(1))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let reason = raise_reason_atom(
        fx.module(),
        run(
            fx.module(),
            &LabelMap::new(),
            &mut machine,
            &Instr::IsRecordAccessible {
                fail: NO_FAIL,
                src: Operand::X(0),
                scope: Operand::Atom(external),
            },
            &mut ctx,
        ),
    );
    assert_eq!(reason, "function_clause");
}
