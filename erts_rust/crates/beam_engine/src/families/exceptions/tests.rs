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

//! Unit tests for the exception / raise instruction family.
//!
//! Split by responsibility across submodules: [`handlers`] (the `try`/`catch`
//! frame push, the handler opcodes and `catch_end`), [`raising`] (the raising
//! opcodes, the re-raise opcode and `raw_raise`), [`stacktrace`]
//! (`build_stacktrace`) and [`end_to_end`] (whole synthetic sequences). The
//! shared fixture scaffolding lives here.

use super::*;
use crate::context::MinimalContext;
use beam_common::{BeamError, ExceptionClass, ExitReason};
use beam_loader::{Function, HeapNeed, Label, LineEntry};
use beam_term::{cmp, make_small};
use std::collections::BTreeMap;

// The production builders take the `Machine` plus a reader closure so a moving
// collector can relocate operands between the reserve and the read; these tests
// build from already-materialized terms against a non-collecting arena, so alias
// `terms` to the test-only wrappers that keep the old ergonomic arity. (An explicit
// import shadows the glob-imported `terms` above.)
use crate::terms::test_support as terms;

/// A tiny module builder with an owned atom table (mirrors the `Fixture`
/// scaffolding in `tests/opcodes.rs`, kept local so the unit tests here do not
/// depend on integration-test helpers).
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new(extra_atoms: &[&str]) -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern("m");
        for name in extra_atoms {
            module.atom_table.intern(name);
        }
        Fixture { module }
    }

    /// Append a function whose code starts with `entry`; returns its label.
    fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
        let name = self.module.atom_table.intern(name);
        let module = self.module.name;
        let mut labels = BTreeMap::new();
        for (index, instr) in code.iter().enumerate() {
            if let Instr::Label(label) = instr {
                labels.insert(*label, index);
            }
        }
        let entry = labels.keys().next().copied().unwrap_or(Label(0));
        self.module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module,
            name,
            arity,
            code,
            blocks: Vec::new(),
            labels,
        });
        entry
    }

    fn finish(self) -> Module {
        self.module
    }
}

/// Empty label map; fine for instructions that raise (they do not jump).
fn no_labels() -> LabelMap {
    LabelMap::new()
}

/// Run one instruction body with a machine that has a bottom frame.
///
/// `module` is both the active module and the run's entry module (context `0`):
/// these helpers execute a single instruction body in one module, so the two
/// coincide. A cross-module context is exercised through
/// [`exec_one_with_entry`].
fn exec_one(
    module: &Module,
    instr: &Instr,
    machine: &mut Machine,
    ctx: &mut MinimalContext,
) -> Result<Option<Step>> {
    exec_one_with_entry(module, module, instr, machine, ctx)
}

/// [`exec_one`] with the run's **entry** module handed in separately from the
/// **active** module (the callee a non-zero module context is executing).
fn exec_one_with_entry(
    entry_module: &Module,
    module: &Module,
    instr: &Instr,
    machine: &mut Machine,
    ctx: &mut MinimalContext,
) -> Result<Option<Step>> {
    exec(machine, entry_module, module, &no_labels(), instr, 0, 0, ctx)
}

/// Assert that `reason` (an `Eterm`) equals the tuple `[tag, elem]`.
fn assert_reason_tuple(
    cmp_ctx: &mut MinimalContext,
    module: &Module,
    reason: Eterm,
    tag: &str,
    elem: Eterm,
) {
    let expected = terms::make_tuple(
        cmp_ctx,
        &[make_atom(module.atom_table.lookup(tag).unwrap()), elem],
    )
    .unwrap();
    assert!(
        cmp(reason, expected, &module.atom_table).is_eq(),
        "reason does not equal {{{tag}, ..}}"
    );
}

mod callee;
mod cross_module;
mod end_to_end;
mod handlers;
mod oracle;
mod raising;
mod stacktrace;
