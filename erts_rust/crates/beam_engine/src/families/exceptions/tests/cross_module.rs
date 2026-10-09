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

//! Cross-module exception landing: a raise inside a callee this machine entered
//! through the cross-module-call seam must land in the **caller's** live handler
//! even though that handler lives in another module context.
//!
//! The shape is the boot path's: `proc_lib:init_p_do_apply/3` is
//!
//! ```text
//! try apply(M, F, A) catch Class:Reason:Stacktrace -> exit_p(...) end
//! ```
//!
//! so the `try` pushes its catch frame in **`proc_lib`'s** module context while
//! the `apply` (or the `call_ext` it compiles to) runs the callee in a *different*
//! one. A `throw` from the callee must unwind back into `proc_lib`'s handler —
//! C: `handle_error` → `next_catch` finds the catch tag in the *frame*, and a code
//! address identifies its own module, so the transfer crosses modules freely
//! (`../../erts/emulator/beam/beam_common.c:580-679`).
use super::*;
use crate::context::MinimalContext;
use crate::machine::RunOutcome;
use beam_loader::{Atom, Export};
use std::sync::Arc;

/// One shared atom universe: every module is built from the **same** table, so a
/// name has the same index everywhere. This is what `beam_rt`'s code server does
/// for the whole loaded corpus (one shared `AtomTable`,
/// `../../beam_rt/src/codeserver.rs`) — and what makes the exception *class* atoms
/// comparable across modules, exactly as the C's globally interned `am_thrown` etc.
/// are.
struct Universe {
    atoms: beam_term::AtomTable,
}

impl Universe {
    fn new(names: &[&str]) -> Universe {
        let mut atoms = beam_term::AtomTable::new();
        for name in names {
            atoms.intern(name);
        }
        Universe { atoms }
    }

    fn atom(&self, name: &str) -> Atom {
        self.atoms
            .lookup(name)
            .unwrap_or_else(|| panic!("{name} is not seeded"))
    }

    /// A module whose atom table starts as the shared prefix (indices stay global).
    fn module(&self, name: &str) -> Module {
        let mut module = Module::default();
        module.atom_table = self.atoms.clone();
        module.name = self.atom(name);
        module
    }
}

/// Append a function; returns its entry label (the first label in `code`).
fn add_function(module: &mut Module, name: &str, arity: u32, code: Vec<Instr>) -> Label {
    let name = module.atom_table.intern(name);
    let module_name = module.name;
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    let entry = labels.keys().next().copied().unwrap_or(Label(0));
    module.functions.push(Function {
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

/// The `proc_lib` shape: `run/0` guards a cross-module target with a `try`/`catch`
/// whose handler returns the landed reason from `x(1)`.
///
/// ```text
/// run/0:
///     allocate 1
///     try y(0) 20              ; the catch frame belongs to THIS module
///     move c -> x(0)           ; `apply c:throwing/0`
///     move throwing -> x(1)
///     apply 0
///     try_end y(0)
///     deallocate 1
///     return
/// 20: try_case y(0)            ; x(0) = the class atom
///     move x(1) -> x(0)        ; the reason
///     deallocate 1
///     return
/// ```
fn guarded_caller(universe: &Universe, callee: &str, target: &str) -> (Module, Module, Label) {
    let mut caller = universe.module("m");
    let callee_atom = universe.atom(callee);
    let target_atom = universe.atom(target);
    let entry = add_function(
        &mut caller,
        "run",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 0,
            },
            Instr::Try {
                frame: Operand::Y(0),
                fail: Label(20),
            },
            Instr::Move {
                src: Operand::Atom(callee_atom),
                dst: Operand::X(0),
            },
            Instr::Move {
                src: Operand::Atom(target_atom),
                dst: Operand::X(1),
            },
            Instr::Apply { arity: 0 },
            Instr::TryEnd {
                frame: Operand::Y(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
            Instr::Label(Label(20)),
            Instr::TryCase {
                frame: Operand::Y(0),
            },
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::X(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
        ],
    );
    let mut callee_module = universe.module(callee);
    let throwing = add_function(
        &mut callee_module,
        target,
        0,
        vec![
            Instr::Label(Label(1)),
            // `throw(42)` — `raw_raise` with x(0) = class, x(1) = value, x(2) = [].
            Instr::Move {
                src: Operand::Atom(universe.atom("throw")),
                dst: Operand::X(0),
            },
            Instr::Move {
                src: Operand::Integer(42),
                dst: Operand::X(1),
            },
            Instr::Move {
                src: Operand::Nil,
                dst: Operand::X(2),
            },
            Instr::RawRaise,
        ],
    );
    callee_module.exports.push(Export {
        function: target_atom,
        arity: 0,
        label: throwing,
    });
    (caller, callee_module, entry)
}

/// A resolver that answers the seam for `c:throwing/0` with the callee's export.
fn resolver_for(callee: Arc<Module>, function: &str) -> crate::context::ExternalResolver {
    let function = function.to_string();
    Box::new(move |module, target_function, arity| {
        if module != "c" || target_function != function || arity != 0 {
            return Ok(None);
        }
        let Some(target_atom) = callee.atom_table.lookup(target_function) else {
            return Ok(None);
        };
        match callee.export(target_atom, arity) {
            Some(label) => Ok(Some((Arc::clone(&callee), label))),
            None => Ok(None),
        }
    })
}

#[test]
fn a_throw_in_a_cross_module_apply_callee_lands_in_the_callers_handler() {
    let universe = Universe::new(&["m", "c", "run", "throwing", "throw", "error", "EXIT"]);
    let (caller, callee, entry) = guarded_caller(&universe, "c", "throwing");
    let target = Arc::new(callee);
    let mut ctx = MinimalContext::with_external_resolver(resolver_for(Arc::clone(&target), "throwing"));
    let mut machine = Machine::new();

    let outcome = machine
        .run_label_outcome(&caller, entry, 0, &[], &mut ctx)
        .expect("the cross-module throw must be contained by the caller's catch");

    assert_eq!(
        outcome,
        RunOutcome::Returned(make_small(42)),
        "the thrown value reaches the caller's handler"
    );
    assert_eq!(machine.frame_depth(), 1, "the run is back at the bottom frame");
    assert_eq!(machine.catch_frame_depth(), 0, "the handler consumed its frame");
    println!("machine context after landing: {:?}", machine.current_code());
    let _ = &ctx;
}
