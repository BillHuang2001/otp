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

//! Tuple instruction family.
//!
//! Owns the tuple-construction/-inspection instructions (per
//! `../beam_loader/CONTEXT.md`, "The instruction vocabulary"):
//! - [`Instr::PutTuple2`] — build a tuple from its element operands
//! - [`Instr::GetTupleElement`] — fetch one element of a tuple
//! - [`Instr::SetTupleElement`] — overwrite one element of a tuple **in place**
//! - [`Instr::SelectTupleArity`] — switch on a tuple's arity
//!
//! Ground truth (read-only): `emu/instrs.tab` `put_tuple2` (`:746`),
//! `i_get_tuple_element` (`:416`) and `set_tuple_element` (`:845-851`), and
//! `emu/select_instrs.tab` `i_select_tuple_arity` (`:111`, the
//! `select_val_lin`/`get_arity` path).
//!
//! ## Semantics
//! - `PutTuple2 { dst, elements }` — read every element operand, build the tuple
//!   (an `arityval` header + the elements) and store it in `dst`.
//! - `GetTupleElement { source, element, dst }` — `source` must be a tuple with
//!   `arity > element`; the C opcode trusts a preceding type test and reads the
//!   word unchecked, so this is the **safety net**: anything else raises
//!   `{badmatch, Source}` (class `error`).
//! - `SetTupleElement { element, tuple, offset }` — overwrite the `tuple`'s element
//!   word `offset` with `element`, **mutating the tuple object in place** (the
//!   destructive C `set_tuple_element s S P`, whose loader pre-scales the index to a
//!   byte offset: element `offset` is the word `get_tuple_element` with the same
//!   index reads). The compiler emits it only for a tuple it has proven uniquely
//!   owned; this engine keeps the C's `ASSERT(is_tuple(...))` as a real check — a
//!   non-tuple or an out-of-range `offset` raises the same `{badmatch, Source}` the
//!   read path raises, and an unwritable target (a term in the module's immutable
//!   literal area) is the typed
//!   `error::unsupported` ([`crate::terms::set_tuple_element`]) — never an
//!   out-of-bounds write.
//! - `SelectTupleArity { tuple, fail, destinations }` — switch on the source
//!   tuple's arity. A non-tuple source has no arity at all (C's `get_arity` sets the
//!   switch value to `NIL`, which no integer alternative can equal), so it matches
//!   nothing and takes exactly the same `$FAIL(Fail)` path as an unmatched arity;
//!   `select_tuple_arity` is only emitted after an `is_tuple` test of the same fail
//!   label, so that path is unreachable for compiler output. The matching
//!   `(Integer(n), Label)` alternative wins; with no match it falls to `fail`, or —
//!   when `fail` is `NO_FAIL` (an exhaustive switch) — raises `{badmatch, Tuple}`.
//!
//! Every failure is a typed [`beam_common::BeamError`]: `write_operand`/
//! `read_operand` propagate, an unsupported operand is `error::unsupported`, and
//! the `{badmatch, V}` raise is `crate::terms::badmatch_reason` +
//! [`Machine::throw_error`] (a `Step::Goto` into a live catch frame, or a typed
//! `Err` when there is none). Never a panic.

use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Instr, Module, Operand, NO_FAIL};

use crate::context::{ExecutionContext, RootSet};
use crate::machine::Machine;
use crate::operand::{
    operand_slack_words, read_operand, read_operand_slack, read_operands_slack, write_operand,
};
use crate::terms;
use crate::Step;

/// Execute a tuple-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a tuple opcode this family executes,
/// `Ok(None)` when `instr` is not this family's variant, and `Err` on a raise.
pub(crate) fn exec(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    instr: &Instr,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    let _ = (func, pc);
    match instr {
        Instr::PutTuple2 { dst, elements } => {
            // Reserve first (one rooted allocation a moving collector may run under),
            // then read every element operand from the post-collection machine state.
            let n = elements.len();
            let extra: usize = elements.iter().map(operand_slack_words).sum();
            let tuple =
                terms::make_tuple_rooted(machine, ctx, RootSet::ALL_LIVE, n, extra, |m, slack| {
                    read_operands_slack(m, elements, slack)
                })?;
            write_operand(machine, dst, tuple)?;
            Ok(Some(Step::Next))
        }

        Instr::GetTupleElement {
            source,
            element,
            dst,
        } => {
            let value = read_operand(machine, ctx, source)?;
            match terms::tuple_arity(value) {
                Ok(arity) if (*element as usize) < arity => {
                    let elem = terms::tuple_element(value, *element as usize)?;
                    write_operand(machine, dst, elem)?;
                    Ok(Some(Step::Next))
                }
                // Not a tuple, or the element index is out of range. The reason's
                // value is re-read from `source` after the reason block's reserve.
                _ => {
                    let reason = terms::badmatch_reason_rooted(
                        machine,
                        ctx,
                        RootSet::ALL_LIVE,
                        module,
                        |m, s| read_operand_slack(m, source, s),
                    )?;
                    machine.throw_error(reason).map(Some)
                }
            }
        }

        Instr::SetTupleElement {
            element,
            tuple,
            offset,
        } => {
            // An element operand that materializes to an exact bignum needs heap
            // words, and that allocation **collects**, so it must happen before the
            // tuple is read — a moving collector would relocate the `Eterm` we
            // captured. That is the family's frozen reserve → read → write ordering;
            // with no materialization to do there is no allocation at all.
            let extra = operand_slack_words(element);
            let (target, value) = if extra == 0 {
                (
                    read_operand(machine, ctx, tuple)?,
                    read_operand(machine, ctx, element)?,
                )
            } else {
                let mut slack = terms::reserve_slack(machine, ctx, extra)?;
                let target = read_operand(machine, ctx, tuple)?;
                let value = read_operand_slack(machine, element, &mut slack)?;
                (target, value)
            };
            // `set_tuple_element` is emitted only for a tuple the compiler proved
            // uniquely owned and sized, so a non-tuple or an out-of-range `offset` is
            // unreachable for real code; C asserts it (`ASSERT(is_tuple($Tuple))`),
            // and this raises the same `{badmatch, Source}` the read path raises
            // rather than writing out of bounds. The reason's value is re-read from
            // `tuple` after the reason block's reserve.
            match terms::tuple_arity(target) {
                Ok(arity) if (*offset as usize) < arity => {}
                _ => {
                    let reason = terms::badmatch_reason_rooted(
                        machine,
                        ctx,
                        RootSet::ALL_LIVE,
                        module,
                        |m, s| read_operand_slack(m, tuple, s),
                    )?;
                    return machine.throw_error(reason).map(Some);
                }
            }
            // The destructive write itself (C's `*p = $Element` at
            // `tuple_val(Tuple) + (Offset + 1) * sizeof(Eterm)`): it can only fail on
            // an unwritable target — a term in the module's immutable literal area —
            // which is the typed `error::unsupported`.
            terms::set_tuple_element(target, *offset as usize, value)?;
            Ok(Some(Step::Next))
        }

        Instr::SelectTupleArity {
            tuple,
            fail,
            destinations,
        } => {
            let value = read_operand(machine, ctx, tuple)?;
            // C's `select_val_lin.get_arity` sets the switch value of a non-tuple
            // source to `NIL` (`select_instrs.tab:122-128`), and `execute` then finds
            // no alternative equal to it and takes the ordinary `$FAIL($Fail)` jump
            // (`:130-153`) — so a non-tuple behaves exactly like an arity that matches
            // no destination.
            let arity = terms::tuple_arity(value).ok();
            let mut target = None;
            for (candidate, label) in destinations {
                // Arities are decoded as `{integer, N}` operands; a non-integer
                // alternative can never equal an arity and is skipped. A non-tuple
                // source has no arity and so matches nothing.
                if let (Some(arity), Operand::Integer(n)) = (arity, candidate) {
                    if *n >= 0 && *n as usize == arity {
                        target = Some(*label);
                        break;
                    }
                }
            }
            let label = match target {
                Some(label) => label,
                None if *fail == NO_FAIL => {
                    let reason = terms::badmatch_reason_rooted(
                        machine,
                        ctx,
                        RootSet::ALL_LIVE,
                        module,
                        |m, s| read_operand_slack(m, tuple, s),
                    )?;
                    return machine.throw_error(reason).map(Some);
                }
                None => *fail,
            };
            let (next_func, next_pc) = labels.resolve(label)?;
            Ok(Some(Step::Goto {
                func: next_func,
                pc: next_pc,
            }))
        }

        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MinimalContext;
    use beam_common::{BeamError, ExceptionClass};
    use beam_loader::{Atom, Function, Label};
    use beam_term::{make_atom, make_small, Eterm, NIL};
    use std::collections::BTreeMap;

    /// A tiny module builder with an owned atom table (mirrors the `Fixture`
    /// scaffolding in `tests/tuples_lists.rs`/`tests/opcodes.rs`, kept local so the
    /// unit tests here do not depend on integration-test helpers).
    struct Fixture {
        module: Module,
    }

    impl Fixture {
        fn new() -> Fixture {
            let mut module = Module::default();
            module.name = module.atom_table.intern("m");
            // The raise path resolves `badmatch` from the running module's table; a
            // module without it is the bare `BeamError::BadMatch` (also covered).
            module.atom_table.intern("badmatch");
            Fixture { module }
        }

        /// Append `f/arity` whose code starts at `Label(1)`; returns its entry label.
        fn add_function(&mut self, arity: u32, code: Vec<Instr>) -> Label {
            let name = self.module.atom_table.intern("f");
            let module = self.module.name;
            let mut labels = BTreeMap::new();
            for (index, instr) in code.iter().enumerate() {
                if let Instr::Label(label) = instr {
                    labels.insert(*label, index);
                }
            }
            let entry = Label(1);
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

    /// Run `entry` with `args` over `ctx`, returning the result and the machine (the
    /// machine carries the recorded exception, so it must outlive the assertion).
    fn run(
        ctx: &mut MinimalContext,
        module: &Module,
        entry: Label,
        args: &[Eterm],
    ) -> (Result<Eterm>, Machine) {
        let mut machine = Machine::new();
        let result = machine.run_label(module, entry, args.len() as u32, args, ctx);
        (result, machine)
    }

    /// Build a `{1, 2, 3}` tuple in `ctx`.
    fn triple(ctx: &mut MinimalContext) -> Eterm {
        terms::make_tuple(ctx, &[make_small(1), make_small(2), make_small(3)]).expect("a 3-tuple")
    }

    /// `set_tuple_element Element x(0) Offset; return` — the tuple arrives as the
    /// single argument.
    fn write_module(element: Operand, offset: u32) -> Module {
        finish_write_module(vec![
            Instr::Label(Label(1)),
            Instr::SetTupleElement {
                element,
                tuple: Operand::X(0),
                offset,
            },
            Instr::Return,
        ])
    }

    /// `set_tuple_element Element x(0) Offset; get_tuple_element x(0) Offset x(0);
    /// return` — the write followed by a read of the same word, so the export returns
    /// the value the write stored (and proves it landed in the tuple's own word).
    fn write_read_module(element: Operand, offset: u32) -> Module {
        finish_write_module(vec![
            Instr::Label(Label(1)),
            Instr::SetTupleElement {
                element,
                tuple: Operand::X(0),
                offset,
            },
            Instr::GetTupleElement {
                source: Operand::X(0),
                element: offset,
                dst: Operand::X(0),
            },
            Instr::Return,
        ])
    }

    /// Wrap `code` into a one-function `f/1` module built by [`Fixture`].
    fn finish_write_module(code: Vec<Instr>) -> Module {
        let mut fixture = Fixture::new();
        fixture.add_function(1, code);
        fixture.finish()
    }

    #[test]
    fn a_write_is_visible_in_place_through_the_same_tuple() {
        // The strongest evidence for the destructive semantics: the caller's own term
        // object changes — not a rebuilt tuple — and a `get_tuple_element` on it
        // reads the new value.
        let mut ctx = MinimalContext::new();
        let tuple = triple(&mut ctx);
        let module = write_read_module(Operand::Integer(99), 1);
        let (result, _machine) = run(&mut ctx, &module, Label(1), &[tuple]);
        assert_eq!(result.expect("the write then read must run"), make_small(99));
        // The tuple the caller still holds is the object that was written.
        assert_eq!(terms::tuple_element(tuple, 1).unwrap(), make_small(99));
        assert_eq!(terms::tuple_element(tuple, 0).unwrap(), make_small(1));
        assert_eq!(terms::tuple_element(tuple, 2).unwrap(), make_small(3));
        assert_eq!(terms::tuple_arity(tuple).unwrap(), 3);
    }

    #[test]
    fn offset_zero_writes_the_first_element_word() {
        // The raw index is the C loader's pre-scaling base (`(Off + 1) * 8`), so
        // index 0 is the first **data** word — the same word `get_tuple_element 0`
        // reads — and never the arityval header that precedes it.
        let mut ctx = MinimalContext::new();
        let tuple = triple(&mut ctx);
        let module = write_read_module(Operand::X(1), 0);
        let (result, _machine) = run(&mut ctx, &module, Label(1), &[tuple, make_small(7)]);
        assert_eq!(result.expect("offset 0 write then read"), make_small(7));
        assert_eq!(terms::tuple_element(tuple, 0).unwrap(), make_small(7));
        // The header survived: the tuple is still an arity-3 tuple.
        assert_eq!(terms::tuple_arity(tuple).unwrap(), 3);
    }

    #[test]
    fn a_non_tuple_source_raises_badmatch() {
        let mut ctx = MinimalContext::new();
        let module = write_module(Operand::Integer(1), 0);
        for (what, source) in [
            ("the empty list", NIL),
            ("a small integer", make_small(3)),
            ("an atom", make_atom(Atom::from_raw(0))),
        ] {
            let (result, machine) = run(&mut ctx, &module, Label(1), &[source]);
            assert!(
                matches!(result, Err(BeamError::Raise(_))),
                "{what}: expected a raise, got {result:?}"
            );
            let (class, reason) = machine.pending_exception().expect("a recorded exception");
            assert_eq!(class, ExceptionClass::Error);
            assert_eq!(terms::tuple_arity(reason).unwrap(), 2);
            assert_eq!(
                terms::tuple_element(reason, 0).unwrap(),
                make_atom(module.atom_table.lookup("badmatch").unwrap())
            );
            assert_eq!(terms::tuple_element(reason, 1).unwrap(), source);
        }
    }

    #[test]
    fn an_out_of_range_offset_raises_badmatch() {
        // `set_tuple_element` carries no fail label (`ops.tab:245`), so — like every
        // other no-fail opcode — a rejected write is a *raise*, not a branch. C would
        // write out of bounds; this engine refuses.
        let mut ctx = MinimalContext::new();
        for (what, arity_of_source, offset) in [
            ("offset 3 of a 3-tuple", 3usize, 3u32),
            ("offset 4 of a 3-tuple", 3, 4),
            ("offset 0 of an empty tuple", 0, 0),
        ] {
            let source = if arity_of_source == 0 {
                beam_term::tuple0()
            } else {
                triple(&mut ctx)
            };
            let module = write_module(Operand::Integer(1), offset);
            let (result, machine) = run(&mut ctx, &module, Label(1), &[source]);
            assert!(
                matches!(result, Err(BeamError::Raise(_))),
                "{what}: expected a raise, got {result:?}"
            );
            let (class, reason) = machine.pending_exception().expect("a recorded exception");
            assert_eq!(class, ExceptionClass::Error);
            assert_eq!(terms::tuple_element(reason, 1).unwrap(), source);
            // Nothing was written.
            if arity_of_source == 3 {
                assert_eq!(terms::tuple_element(source, 0).unwrap(), make_small(1));
                assert_eq!(terms::tuple_element(source, 1).unwrap(), make_small(2));
                assert_eq!(terms::tuple_element(source, 2).unwrap(), make_small(3));
            }
        }
    }

    #[test]
    fn a_module_without_the_badmatch_atom_reports_the_bare_error() {
        // The reason's tag is resolved from the *running* module's atom table (never
        // substituted from another), so a module that cannot name `badmatch` reports
        // the canonical typed error instead of a fabricated atom.
        let mut fixture = Fixture::new();
        let name = fixture.module.atom_table.intern("f");
        let module_name = fixture.module.name;
        let code = vec![
            Instr::Label(Label(1)),
            Instr::SetTupleElement {
                element: Operand::Integer(1),
                tuple: Operand::X(0),
                offset: 0,
            },
            Instr::Return,
        ];
        let mut labels = BTreeMap::new();
        labels.insert(Label(1), 0);
        fixture.module.functions.push(Function {
            func_label: Label(1),
            entry_label: Label(1),
            module: module_name,
            name,
            arity: 1,
            code,
            blocks: Vec::new(),
            labels,
        });
        let mut module = fixture.finish();
        // Drop the interned `badmatch` the default fixture seeds: an unseeded module
        // holds only the atoms its own code names.
        module.atom_table = beam_term::AtomTable::new();
        module.name = module.atom_table.intern("m");
        let function_name = module.atom_table.intern("f");
        module.functions[0].name = function_name;
        module.functions[0].module = module.name;

        let mut ctx = MinimalContext::new();
        let (result, _machine) = run(&mut ctx, &module, Label(1), &[NIL]);
        assert!(
            matches!(result, Err(BeamError::BadMatch)),
            "expected the bare BeamError::BadMatch, got {result:?}"
        );
    }
}
