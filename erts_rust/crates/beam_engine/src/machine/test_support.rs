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

//! **Test-only** scaffolding shared by the `machine` unit tests.
//!
//! The synthetic-module builder ([`Fixture`]) and the cross-module seam context
//! ([`seam`]) are used by more than one test module — the driver's `Step::Enter`
//! contract tests (`drive.rs`) and the raise-funnel end-to-end tests (`tests.rs`)
//! — so they live here once instead of being copy-pasted per test module. None of
//! this is reachable from production code: the module is declared `#[cfg(test)]`.

use std::collections::BTreeMap;
use std::sync::Arc;

use beam_common::Result;
use beam_loader::{Atom, Export, Function, Import, ImportRef, Instr, Label, Module};
use beam_term::{Eterm, NIL};

use crate::context::{Mailbox, MinimalContext};

/// Map every `Instr::Label` in `code` to its index.
fn labels_of(code: &[Instr]) -> BTreeMap<Label, usize> {
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    labels
}

/// One synthetic module with its own atom table.
pub(crate) struct Fixture {
    module: Module,
}

impl Fixture {
    /// A module named `name` with no functions yet.
    pub(crate) fn new(name: &str) -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern(name);
        Fixture { module }
    }

    /// Intern `name` in the module's atom table and return the atom.
    pub(crate) fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    /// Append an import to `module:function/arity` and return its reference.
    pub(crate) fn import(&mut self, module: &str, function: &str, arity: u32) -> ImportRef {
        let module = self.atom(module);
        let function = self.atom(function);
        self.module.imports.push(Import {
            module,
            function,
            arity,
        });
        ImportRef((self.module.imports.len() - 1) as u32)
    }

    /// Append a function whose entry label is the first `Instr::Label` of `code`
    /// (the k-th function added is function index k); returns that entry label.
    pub(crate) fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
        let name = self.atom(name);
        let module_name = self.module.name;
        let labels = labels_of(&code);
        let entry = labels.keys().next().copied().unwrap_or(Label(0));
        self.module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module: module_name,
            name,
            arity,
            code,
            blocks: Vec::new(),
            labels,
        });
        entry
    }

    /// Export `name/arity` at `label` (so `run_export_outcome` can enter it).
    pub(crate) fn add_export(&mut self, name: &str, arity: u32, label: Label) {
        let function = self.atom(name);
        self.module.exports.push(Export {
            function,
            arity,
            label,
        });
    }

    /// The finished module.
    pub(crate) fn finish(self) -> Module {
        self.module
    }
}

/// A mailbox that is always empty: `loop_rec` takes its fail label immediately.
pub(crate) struct EmptyMailbox;

impl Mailbox for EmptyMailbox {
    fn send_message(&mut self, _to: Eterm, _message: Eterm) -> Result<()> {
        Ok(())
    }
    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        Ok(None)
    }
    fn receive_skip(&mut self) -> Result<()> {
        Ok(())
    }
    fn receive_remove(&mut self) -> Result<()> {
        Ok(())
    }
    fn receive_reset(&mut self) -> Result<()> {
        Ok(())
    }
    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        Ok(NIL)
    }
    fn receive_marker_bind(&mut self, _marker: Eterm, _reference: Eterm) -> Result<()> {
        Ok(())
    }
    fn receive_marker_use(&mut self, _reference: Eterm) -> Result<()> {
        Ok(())
    }
    fn receive_marker_clear(&mut self, _reference: Eterm) -> Result<()> {
        Ok(())
    }
}

/// A `MinimalContext` whose cross-module seam resolves the named callees and
/// whose mailbox is empty.
pub(crate) fn seam(callees: Vec<(String, Arc<Module>)>) -> MinimalContext {
    let mut ctx =
        MinimalContext::with_external_resolver(Box::new(move |module, function, arity| {
            for (name, callee) in &callees {
                if name != module {
                    continue;
                }
                let Some(function) = callee.atom_table.lookup(function) else {
                    continue;
                };
                if let Some(label) = callee.export(function, arity) {
                    return Ok(Some((Arc::clone(callee), label)));
                }
            }
            Ok(None)
        }));
    ctx.set_mailbox(Box::new(EmptyMailbox));
    ctx
}
