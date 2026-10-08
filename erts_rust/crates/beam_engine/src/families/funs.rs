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

//! Fun / closure instruction family.
//!
//! Owns the closure and fun-application instructions (per
//! `../beam_loader/CONTEXT.md`, "The instruction vocabulary"):
//! - [`Instr::MakeFun3`] — build a local closure from its `FunT` entry
//! - [`Instr::CallFun`] — call the fun in `x(Arity)`
//! - [`Instr::CallFun2`] — call the fun held in a register
//! - [`Instr::Apply`] — apply an `M,F` in `x(Arity)`/`x(Arity+1)` with `Arity` args
//! - [`Instr::ApplyLast`] — a tail `apply`
//!
//! Ground truth (read-only): `emu/instrs.tab` `i_make_fun3` (`:545`),
//! `i_call_fun`/`i_call_fun_last` (`:363`/`:371`), `apply`/`apply_last` (`:298`/
//! `:306`), and `beam_common.c` `call_fun` (`:1604`), `fixed_apply` (`:1528`),
//! `apply` (`:1433`).
//!
//! ## Semantics (the register conventions, verified against the C and the oracle)
//! - `MakeFun3 { index, dst, env }` — `index` is the **fun-table index** (the
//!   `FunT` entry *position*); `Module::fun(index)` yields `{function, arity,
//!   label, index, num_free, uniq}`, `env` holds `num_free` free-variable
//!   operands. The closure is built with
//!   [`terms::make_fun`](crate::terms::make_fun) and stored in `dst`. The header
//!   arity is `arity - num_free` — the *closure's* arity, i.e. the lambda's total
//!   arity minus the captured ones (`emu_load.c:645-650` passes
//!   `lambda->arity - lambda->num_free` to `erts_fun_entry_put`, and
//!   `emu/instrs.tab:565` asserts the header arity equals that).
//! - `CallFun { arity }` — the fun is in `x(arity)`; the arguments are
//!   `x(0)..x(arity-1)` (`beam_common.c:1615` `fun = reg[arity]`).
//! - `CallFun2 { tag, arity, func }` — the fun is the `func` operand (a register,
//!   normally `x(arity)`); the arguments are `x(0)..x(arity-1)`. `tag` is the
//!   compiler's type hint (`safe`/`unsafe`, or a fun-table index) and is not needed
//!   by the interpreter (`emu/generators.tab:26-49` rewrites `call_fun2` to
//!   `call_fun` with a `move`).
//! - `Apply { arity }` — `fixed_apply` (`beam_common.c:1536-1537`): the module is
//!   `x(arity)`, the function is `x(arity+1)`, and the arguments are
//!   `x(0)..x(arity-1)`. Both must be atoms; the call is dispatched through
//!   [`ExecutionContext::call_bif`](crate::context::ExecutionContext::call_bif).
//! - `ApplyLast { arity, deallocate }` — as `Apply`, but the frame is popped first
//!   (`i_apply_last`: `$deallocate($Deallocate)`), then control follows the
//!   machine's continuation **context-aware** via
//!   [`Machine::continuation_step`](crate::machine::Machine::continuation_step):
//!   `Continuation::Halt` ⇒ `Step::Done`, an address in the **current** module
//!   context ⇒ `Step::Goto`, and an address in **another** context ⇒
//!   `Step::Enter { context, .. }` so the driver re-binds the module before
//!   executing the return address. This matters because the current function may
//!   have been entered through the cross-module
//!   [`resolve_external_call`](crate::ExecutionContext::resolve_external_call)
//!   seam: its tail `apply` must land back in the **caller's** module, never in the
//!   callee's. The shape is the same the core `CallExtLast` arm uses.
//!
//! Fun application (the `call_fun`/`call_fun2` path) reaches the host through
//! [`ExecutionContext::apply_fun`](crate::context::ExecutionContext::apply_fun)
//! (default: a typed `unsupported`); `apply`/`apply_last` name a `module:function`
//! directly and go through `call_bif`.
//!
//! Every failure is a typed [`beam_common::BeamError`]: `read_operand`/
//! `write_operand` propagate, an out-of-range register is
//! [`crate::error::bad_register`], a `make_fun3` with no matching descriptor is
//! [`crate::error::unsupported`], and a non-atom `apply` target is the C's
//! `badarg` **raise** — catchable through [`raise_badarg`] (landing in a live
//! `try`/`catch`), or the typed [`BeamError::BadArg`] when there is no catch
//! frame to land in (or the module cannot name `badarg`). Never a panic and never
//! a wrong result.

use crate::interpreter::LabelMap;

use beam_common::{BeamError, ExceptionClass, Result};
use beam_loader::{Instr, Module};
use beam_term::{atom_val, is_atom, make_atom, Eterm};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::terms;
use crate::Step;

/// Execute a fun-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a fun opcode this family executes,
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
    let _ = (labels, func, pc);
    match instr {
        // `make_fun3 Index Dst Env` — build a closure from the `FunT` entry.
        Instr::MakeFun3 { index, dst, env } => {
            let descriptor = module
                .fun(*index)
                .ok_or_else(|| error::unsupported("make_fun3: fun-table index out of range"))?;
            // The `Env` operand list must carry exactly the lambda's free
            // variables; a mismatch is a malformed module, not a runtime value.
            if descriptor.num_free as usize != env.len() {
                return Err(error::unsupported(
                    "make_fun3: free-variable operand count mismatch",
                ));
            }
            let mut free = Vec::with_capacity(env.len());
            for operand in env {
                free.push(read_operand(machine, ctx, operand)?);
            }
            // Header arity is the *closure* arity: total lambda arity minus the
            // captured free variables (`emu_load.c`, `emu/instrs.tab:565`).
            let arity = descriptor
                .arity
                .checked_sub(descriptor.num_free)
                .ok_or_else(|| error::unsupported("make_fun3: num_free exceeds arity"))?;
            let fun = terms::make_fun(ctx, arity, descriptor.uniq as u64, descriptor.index, &free)?;
            write_operand(machine, dst, fun)?;
            Ok(Some(Step::Next))
        }

        // `call_fun Arity` — the fun is in `x(arity)`, arguments in `x(0..arity)`.
        Instr::CallFun { arity } => {
            let fun = machine
                .x(*arity)
                .ok_or_else(|| error::bad_register(*arity))?;
            let args = register_args(machine, *arity)?;
            // The engine passes the arguments in `x(0..arity)` and delegates the
            // closure's free-variable layout (`x(arity..)`) to the context — see
            // [`ExecutionContext::apply_fun`](crate::context::ExecutionContext::apply_fun).
            let result = ctx.apply_fun(fun, &args)?;
            machine.set_x(0, result)?;
            Ok(Some(Step::Next))
        }

        // `call_fun2 Tag Arity Func` — the fun is the `Func` operand, arguments in
        // `x(0..arity)`. The `Tag` hint (an atom or a fun-table index) is ignored.
        Instr::CallFun2 { arity, func, .. } => {
            let fun = read_operand(machine, ctx, func)?;
            let args = register_args(machine, *arity)?;
            // As for `call_fun`: arguments in `x(0..arity)`, captures laid out by
            // the context (`ExecutionContext::apply_fun`).
            let result = ctx.apply_fun(fun, &args)?;
            machine.set_x(0, result)?;
            Ok(Some(Step::Next))
        }

        // `apply Arity` — module = `x(arity)`, function = `x(arity+1)`, arguments
        // in `x(0..arity)` (`fixed_apply`).
        Instr::Apply { arity } => match apply_target(machine, module, *arity, ctx)? {
            ApplyTarget::Called(result) => {
                machine.set_x(0, result)?;
                Ok(Some(Step::Next))
            }
            // A non-atom target is the catchable `badarg` raise: the landing
            // already wrote x(0..=3), so x(0) must not be overwritten here.
            ApplyTarget::Raised(step) => Ok(Some(step)),
        },

        // `apply_last Arity Deallocate` — pop the frame, then apply and continue
        // through the machine's continuation (a tail call).
        Instr::ApplyLast { arity, deallocate } => {
            machine.pop_frame_checked(*deallocate)?;
            match apply_target(machine, module, *arity, ctx)? {
                ApplyTarget::Called(result) => {
                    machine.set_x(0, result)?;
                    // Follow the frame's continuation **context-aware**: this
                    // function may have been entered through the cross-module
                    // `resolve_external_call` seam, so its return address can name a
                    // *different* module. `continuation_step` is the one place that
                    // distinguishes a `Halt`, an intra-context `Goto` and a
                    // cross-context `Enter` (the discipline the `call_ext*`
                    // synchronous fallbacks in the interpreter already follow).
                    Ok(Some(machine.continuation_step()?))
                }
                ApplyTarget::Raised(step) => Ok(Some(step)),
            }
        }

        // Not this family's variant.
        _ => Ok(None),
    }
}

/// Collect the `arity` register arguments `x(0)..x(arity-1)`.
fn register_args(machine: &Machine, arity: u32) -> Result<Vec<Eterm>> {
    let mut args = Vec::with_capacity(arity as usize);
    for i in 0..arity {
        args.push(machine.x(i).ok_or_else(|| error::bad_register(i))?);
    }
    Ok(args)
}

/// The outcome of an `apply`/`apply_last` target dispatch ([`apply_target`]).
enum ApplyTarget {
    /// The target was resolved to a `module:function` and dispatched; the applied
    /// result value is in hand (the caller stores it in `x(0)`).
    Called(Eterm),
    /// The target shape was invalid — a non-atom module/function — and the C's
    /// `badarg` raise was routed through the machine (see [`raise_badarg`]). The
    /// `Step` is the handler landing the raise produced (a live catch frame); the
    /// landed registers (`x(0) = THE_NON_VALUE`, `x(1..=3)`) must not be clobbered.
    Raised(Step),
}

/// Dispatch the `fixed_apply` shape: `module = x(arity)`, `function = x(arity+1)`,
/// arguments `x(0)..x(arity-1)`.
///
/// Both the module and the function must be atoms (the C raises `badarg` with the
/// argument list otherwise — see [`raise_badarg`]); the target is then applied
/// through [`ExecutionContext::call_bif`]. This also covers the C's `apply of
/// apply/3` special case (`beam_common.c:1554-1557`): `x(0)`/`x(1)`/`x(2)` are then
/// handed straight to `erlang:apply/3` as its `M,F,Args` arguments, which is
/// exactly what the recursion into `apply()` computes.
fn apply_target(
    machine: &mut Machine,
    module: &Module,
    arity: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<ApplyTarget> {
    let module_term = machine.x(arity).ok_or_else(|| error::bad_register(arity))?;
    let function_term = machine
        .x(arity + 1)
        .ok_or_else(|| error::bad_register(arity + 1))?;
    // `fixed_apply` (`beam_common.c:1539-1552`) checks the *function* first, then
    // the *module*; both a non-atom `module` and a non-atom `function` set
    // `p->freason = BADARG` and return NULL into `handle_error` → `next_catch`, so
    // the `badarg` is a **catchable raise**, not a bare return.
    let (module_name, function_name) = match (
        atom_name(module, module_term),
        atom_name(module, function_term),
    ) {
        (Ok(module_name), Ok(function_name)) => (module_name, function_name),
        _ => return raise_badarg(machine, module).map(ApplyTarget::Raised),
    };
    let args = register_args(machine, arity)?;
    ctx.call_bif(module_name, function_name, arity, &args)
        .map(ApplyTarget::Called)
}

/// Raise the `badarg` semantic error through the machine, landing it in a live
/// catch frame (or a typed [`BeamError::BadArg`] when there is none).
///
/// C's `fixed_apply` (`beam_common.c:1539-1552`) sets `p->freason = BADARG` and
/// returns NULL into `handle_error` → `next_catch`
/// (`../../erts/emulator/beam/beam_common.c:580-679`), so the raise **is
/// catchable** by an enclosing `try`/`catch` (verified against the oracle: `try
/// apply(1, f, []) catch C:R -> {C,R} end` yields `{badarg,...}` with class
/// `error`). The engine's raise/unwind entry point is [`Machine::throw_error`],
/// with the reason atom `badarg` resolved from the running module's atom table
/// (the engine never invents an atom the module cannot name).
///
/// A module whose atom table lacks `badarg` cannot name the reason, so the typed
/// [`BeamError::BadArg`] is returned instead — never a substituted, wrong atom.
/// With **no live catch frame** the raise cannot land, so it likewise propagates
/// unchanged as the typed [`BeamError::BadArg`] (the same uncaught-error contract
/// the interpreter's `raise_bif_error` follows). The `error` class atom is
/// installed when the module has it, which is what a landing places in `x(3)`.
fn raise_badarg(machine: &mut Machine, module: &Module) -> Result<Step> {
    if machine.catch_frame_depth() == 0 {
        return Err(BeamError::BadArg);
    }
    let atom = module
        .atom_table
        .lookup("badarg")
        .ok_or(BeamError::BadArg)?;
    if let Some(class_atom) = module.atom_table.lookup(ExceptionClass::Error.as_str()) {
        machine.set_class_atom(ExceptionClass::Error, make_atom(class_atom));
    }
    machine.throw_error(make_atom(atom))
}

/// The name of an atom term, resolved through the running module's atom table.
///
/// A non-atom term, or an atom index absent from the table, is a typed
/// [`BeamError::BadArg`] (the C's `BADARG`); the engine never substitutes a
/// *different* atom.
fn atom_name<'a>(module: &'a Module, term: Eterm) -> Result<&'a str> {
    if !is_atom(term) {
        return Err(BeamError::BadArg);
    }
    module.atom_name(atom_val(term)).ok_or(BeamError::BadArg)
}

#[cfg(test)]
mod tests {
    //! Synthetic-module unit tests for every fun opcode, built by hand (the loader
    //! is not involved), plus the documented error paths. The integration test over
    //! a real `.beam` lives in `tests/funs.rs`.

    use super::*;
    use beam_common::BeamError;
    use beam_loader::{FunDescriptor, Function, Label};
    use beam_term::{make_small, signed_value, Atom};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use crate::context::MinimalContext;
    use crate::machine::{CatchFrame, Continuation};

    /// A one-function synthetic module with an owned atom table.
    struct Fixture {
        module: Module,
    }

    impl Fixture {
        fn new() -> Fixture {
            let mut module = Module::default();
            module.name = module.atom_table.intern("m");
            Fixture { module }
        }

        fn atom(&mut self, name: &str) -> Atom {
            self.module.atom_table.intern(name)
        }

        /// Register a `FunT` descriptor; its position is the fun-table index.
        fn add_fun(&mut self, name: &str, arity: u32, num_free: u32, uniq: u32) -> u32 {
            let function = self.atom(name);
            let index = self.module.funs.len() as u32;
            self.module.funs.push(FunDescriptor {
                function,
                arity,
                label: Label(index + 1),
                index,
                num_free,
                uniq,
            });
            index
        }

        /// Append a function; returns its entry label (the first label in `code`).
        fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
            let name = self.atom(name);
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

    /// The `arith_hook` of the other tests: `erlang:'+'/2` over small ints.
    fn plus_hook(module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
        if module != "erlang" || function != "+" || arity != 2 || args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        Ok(make_small(signed_value(args[0]) + signed_value(args[1])))
    }

    // -----------------------------------------------------------------------
    // MakeFun3
    // -----------------------------------------------------------------------

    #[test]
    fn make_fun3_builds_a_closure_with_the_descriptor_fields() {
        let mut fixture = Fixture::new();
        let index = fixture.add_fun("lambda", 2, 1, 0xABCD);
        // `make_fun3 Index Dst {x,0}` — capture x(0) as the free variable.
        let entry = fixture.add_function(
            "f",
            1,
            vec![
                Instr::Label(Label(1)),
                Instr::MakeFun3 {
                    index,
                    dst: beam_loader::Operand::X(1),
                    env: vec![beam_loader::Operand::X(0)],
                },
                Instr::Move {
                    src: beam_loader::Operand::X(1),
                    dst: beam_loader::Operand::X(0),
                },
                Instr::Return,
            ],
        );
        let module = fixture.finish();

        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let result = machine
            .run_label(&module, entry, 1, &[make_small(7)], &mut ctx)
            .expect("make_fun3");
        assert!(terms::is_fun(result));
        // Closure arity = descriptor arity (2) - num_free (1).
        assert_eq!(terms::fun_arity(result).unwrap(), 1);
        assert_eq!(terms::fun_num_free(result).unwrap(), 1);
        assert_eq!(terms::fun_uniq(result).unwrap(), 0xABCD);
        assert_eq!(terms::fun_index(result).unwrap(), index);
        assert_eq!(terms::fun_free(result).unwrap(), &[make_small(7)]);
    }

    #[test]
    fn make_fun3_with_no_free_variables() {
        let mut fixture = Fixture::new();
        let index = fixture.add_fun("lambda", 1, 0, 1);
        let entry = fixture.add_function(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::MakeFun3 {
                    index,
                    dst: beam_loader::Operand::X(0),
                    env: vec![],
                },
                Instr::Return,
            ],
        );
        let module = fixture.finish();

        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let result = machine
            .run_label(&module, entry, 0, &[], &mut ctx)
            .expect("make_fun3");
        assert!(terms::is_fun(result));
        assert_eq!(terms::fun_arity(result).unwrap(), 1);
        assert_eq!(terms::fun_num_free(result).unwrap(), 0);
        assert_eq!(terms::fun_free(result).unwrap(), &[] as &[Eterm]);
    }

    #[test]
    fn make_fun3_with_a_missing_descriptor_is_a_typed_error() {
        let mut fixture = Fixture::new();
        let entry = fixture.add_function(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::MakeFun3 {
                    index: 9,
                    dst: beam_loader::Operand::X(0),
                    env: vec![],
                },
                Instr::Return,
            ],
        );
        let module = fixture.finish();

        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let err = machine
            .run_label(&module, entry, 0, &[], &mut ctx)
            .expect_err("a missing descriptor must fail");
        assert!(matches!(err, BeamError::Raise(_)));
    }

    #[test]
    fn make_fun3_with_a_free_variable_count_mismatch_is_a_typed_error() {
        let mut fixture = Fixture::new();
        // The descriptor captures one free variable, but `env` is empty.
        let index = fixture.add_fun("lambda", 1, 1, 1);
        let entry = fixture.add_function(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::MakeFun3 {
                    index,
                    dst: beam_loader::Operand::X(0),
                    env: vec![],
                },
                Instr::Return,
            ],
        );
        let module = fixture.finish();

        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let err = machine
            .run_label(&module, entry, 0, &[], &mut ctx)
            .expect_err("a free-variable count mismatch must fail");
        assert!(matches!(err, BeamError::Raise(_)));
    }

    // -----------------------------------------------------------------------
    // CallFun / CallFun2
    // -----------------------------------------------------------------------

    #[test]
    fn call_fun_dispatches_the_fun_in_x_arity() {
        // `f(F, A) -> F(A)` with the fun in x(1) and the argument in x(0).
        let mut fixture = Fixture::new();
        let entry = fixture.add_function(
            "f",
            2,
            vec![
                Instr::Label(Label(1)),
                Instr::CallFun { arity: 1 },
                Instr::Return,
            ],
        );
        let module = fixture.finish();

        let mut machine = Machine::new();
        let mut ctx = MinimalContext::with_apply_hook(Box::new(|_fun, args| {
            Ok(make_small(signed_value(args[0]) + 1))
        }));
        let result = machine
            .run_label(&module, entry, 2, &[make_small(4), make_small(0)], &mut ctx)
            .expect("call_fun");
        assert_eq!(result, make_small(5));
    }

    #[test]
    fn call_fun_without_an_apply_hook_is_a_typed_unsupported() {
        let mut fixture = Fixture::new();
        let entry = fixture.add_function(
            "f",
            1,
            vec![
                Instr::Label(Label(1)),
                Instr::CallFun { arity: 0 },
                Instr::Return,
            ],
        );
        let module = fixture.finish();

        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let err = machine
            .run_label(&module, entry, 1, &[make_small(0)], &mut ctx)
            .expect_err("no apply hook must fail");
        assert!(matches!(err, BeamError::Raise(_)));
    }

    #[test]
    fn call_fun_with_an_out_of_range_fun_register_is_a_typed_error() {
        // `call_fun 1` needs `x(1)`, which is beyond a 1-register file.
        let mut fixture = Fixture::new();
        let entry = fixture.add_function(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::CallFun { arity: 1 },
                Instr::Return,
            ],
        );
        let module = fixture.finish();

        let mut machine = Machine::with_max_register(1);
        let mut ctx = MinimalContext::new();
        let err = machine
            .run_label(&module, entry, 0, &[], &mut ctx)
            .expect_err("x(1) is out of range");
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn call_fun2_dispatches_the_fun_in_its_operand() {
        // `f(F) -> F()` with the fun in x(0) and no arguments.
        let mut fixture = Fixture::new();
        let unsafe_atom = fixture.atom("unsafe");
        let entry = fixture.add_function(
            "f",
            1,
            vec![
                Instr::Label(Label(1)),
                Instr::CallFun2 {
                    tag: beam_loader::Operand::Atom(unsafe_atom),
                    arity: 0,
                    func: beam_loader::Operand::X(0),
                },
                Instr::Return,
            ],
        );
        let module = fixture.finish();

        let mut machine = Machine::new();
        let mut ctx = MinimalContext::with_apply_hook(Box::new(|_fun, args| {
            assert!(args.is_empty());
            Ok(make_small(42))
        }));
        let result = machine
            .run_label(&module, entry, 1, &[make_small(0)], &mut ctx)
            .expect("call_fun2");
        assert_eq!(result, make_small(42));
    }

    // -----------------------------------------------------------------------
    // Apply / ApplyLast
    // -----------------------------------------------------------------------

    /// Build a module whose `f/3` performs `apply 2` with `x(0)=A, x(1)=B,
    /// x(2)=Mod, x(3)=Fun`, returning the applied result in `x(0)`.
    fn apply_module(fixture: &mut Fixture) -> Label {
        fixture.add_function(
            "f",
            4,
            vec![
                Instr::Label(Label(1)),
                Instr::Apply { arity: 2 },
                Instr::Return,
            ],
        )
    }

    #[test]
    fn apply_dispatches_module_and_function_from_x_arity() {
        let mut fixture = Fixture::new();
        let entry = apply_module(&mut fixture);
        let erlang = fixture.atom("erlang");
        let plus = fixture.atom("+");
        let module = fixture.finish();

        let mut machine = Machine::new();
        let mut ctx = MinimalContext::with_bif_hook(Box::new(plus_hook));
        let result = machine
            .run_label(
                &module,
                entry,
                4,
                &[
                    make_small(2),
                    make_small(3),
                    beam_term::make_atom(erlang),
                    beam_term::make_atom(plus),
                ],
                &mut ctx,
            )
            .expect("apply 2");
        assert_eq!(result, make_small(5));
    }

    #[test]
    fn apply_with_a_non_atom_module_is_a_typed_error() {
        let mut fixture = Fixture::new();
        let entry = apply_module(&mut fixture);
        let plus = fixture.atom("+");
        let module = fixture.finish();

        let mut machine = Machine::new();
        let mut ctx = MinimalContext::with_bif_hook(Box::new(plus_hook));
        let err = machine
            .run_label(
                &module,
                entry,
                4,
                &[
                    make_small(2),
                    make_small(3),
                    make_small(0),
                    beam_term::make_atom(plus),
                ],
                &mut ctx,
            )
            .expect_err("a non-atom module must fail");
        assert!(matches!(err, BeamError::BadArg));
    }

    // -- catchability of the `badarg` raise (non-atom `apply` target) ---------

    #[test]
    fn apply_badarg_lands_in_a_live_catch_frame() {
        // `fixed_apply` (`beam_common.c:1539-1552`) sets `p->freason = BADARG` and
        // returns NULL into `handle_error` → `next_catch`, so an enclosing
        // `try`/`catch` catches it. Oracle cross-check: `try apply(1, f, [])
        // catch C:R -> {C,R} end` yields class `error`, reason `badarg`.
        let mut fixture = Fixture::new();
        fixture.atom("badarg");
        fixture.atom("error");
        let plus = fixture.atom("+");
        let module = fixture.finish();
        let badarg = module.atom_table.lookup("badarg").expect("interned");
        let error = module.atom_table.lookup("error").expect("interned");

        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        // The bottom frame "owns" the handler at (func 4, pc 6).
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(4, 6, 0));
        machine.set_x(0, make_small(2)).unwrap();
        machine.set_x(1, make_small(3)).unwrap();
        machine.set_x(2, make_small(0)).unwrap(); // a non-atom module
        machine.set_x(3, make_atom(plus)).unwrap(); // a valid atom function

        let instr = Instr::Apply { arity: 2 };
        let labels = LabelMap::new();
        let step = exec(&mut machine, &module, &labels, &instr, 0, 0, &mut ctx).unwrap();
        assert_eq!(step, Some(Step::Goto { func: 4, pc: 6 }));

        // The C landing convention (beam_common.c:535-544): x0 = THE_NON_VALUE,
        // x1 = the reason (`badarg`), x3 = the exception class (`error`).
        assert_eq!(machine.x(0), Some(Eterm::non_value()));
        assert_eq!(machine.x(1), Some(make_atom(badarg)));
        assert_eq!(machine.x(3), Some(make_atom(error)));
        // The raise consults the innermost entry but does not pop it.
        assert_eq!(machine.catch_frame_depth(), 1);
    }

    #[test]
    fn apply_last_badarg_lands_in_a_live_catch_frame() {
        // The `apply_last` variant routes the same `badarg` raise, after popping its
        // own frame (`i_apply_last`'s `$deallocate` precedes `fixed_apply`).
        let mut fixture = Fixture::new();
        fixture.atom("badarg");
        fixture.atom("error");
        let plus = fixture.atom("+");
        let module = fixture.finish();
        let badarg = module.atom_table.lookup("badarg").expect("interned");

        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        machine.push_frame(0).unwrap(); // the frame owning the handler
        machine.push_frame(0).unwrap(); // the frame `apply_last` pops
        machine.push_catch_frame(CatchFrame::new(4, 6, 0));
        machine.set_x(2, make_small(0)).unwrap(); // a non-atom module
        machine.set_x(3, make_atom(plus)).unwrap(); // a valid atom function

        let instr = Instr::ApplyLast {
            arity: 2,
            deallocate: 0,
        };
        let labels = LabelMap::new();
        let step = exec(&mut machine, &module, &labels, &instr, 0, 0, &mut ctx).unwrap();
        assert_eq!(step, Some(Step::Goto { func: 4, pc: 6 }));
        assert_eq!(machine.x(1), Some(make_atom(badarg)));
        assert_eq!(machine.frame_depth(), 1, "the `apply_last` frame is popped");
    }

    #[test]
    fn apply_badarg_without_a_catch_frame_is_the_typed_error() {
        // The same module (the `badarg`/`error` atoms exist) but with no live `try`
        // frame: the raise cannot land, so the typed `BeamError::BadArg` propagates
        // unchanged (the uncaught-error contract).
        let mut fixture = Fixture::new();
        fixture.atom("badarg");
        fixture.atom("error");
        let plus = fixture.atom("+");
        let module = fixture.finish();

        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        machine.push_frame(0).unwrap();
        machine.set_x(2, make_small(0)).unwrap();
        machine.set_x(3, make_atom(plus)).unwrap();

        let instr = Instr::Apply { arity: 2 };
        let labels = LabelMap::new();
        let err = exec(&mut machine, &module, &labels, &instr, 0, 0, &mut ctx)
            .expect_err("a non-atom module must fail");
        assert!(matches!(err, BeamError::BadArg));
        assert_eq!(machine.catch_frame_depth(), 0);
    }

    #[test]
    fn apply_badarg_without_the_badarg_atom_falls_back_to_the_typed_error() {
        // A live catch frame, but the module cannot name `badarg` (the atom is
        // absent from its table): rather than substitute a *different, wrong* atom
        // the typed `BeamError::BadArg` is returned.
        let mut fixture = Fixture::new();
        let plus = fixture.atom("+");
        let module = fixture.finish();

        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(4, 6, 0));
        machine.set_x(2, make_small(0)).unwrap();
        machine.set_x(3, make_atom(plus)).unwrap();

        let instr = Instr::Apply { arity: 2 };
        let labels = LabelMap::new();
        let err = exec(&mut machine, &module, &labels, &instr, 0, 0, &mut ctx)
            .expect_err("no `badarg` atom must fall back to the typed error");
        assert!(matches!(err, BeamError::BadArg));
        // The raise never started: the catch frame is untouched and nothing pending.
        assert_eq!(machine.catch_frame_depth(), 1);
        assert_eq!(machine.pending_exception(), None);
    }

    #[test]
    fn apply_last_pops_the_frame_and_returns_through_the_continuation() {
        // `caller/0` calls `f/4`; `f` allocates a frame and its `apply_last 2 0`
        // tail-applies `erlang:'+'/2`, then returns to `caller` through the frame
        // it popped (`Continuation::Address`).
        let mut fixture = Fixture::new();
        let callee = fixture.add_function(
            "f",
            4,
            vec![
                Instr::Label(Label(2)),
                Instr::Allocate {
                    stack_need: beam_loader::HeapNeed::Words(0),
                    live: 4,
                },
                Instr::ApplyLast {
                    arity: 2,
                    deallocate: 0,
                },
            ],
        );
        let erlang = fixture.atom("erlang");
        let plus = fixture.atom("+");
        let entry = fixture.add_function(
            "caller",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Allocate {
                    stack_need: beam_loader::HeapNeed::Words(0),
                    live: 4,
                },
                Instr::Move {
                    src: beam_loader::Operand::Integer(2),
                    dst: beam_loader::Operand::X(0),
                },
                Instr::Move {
                    src: beam_loader::Operand::Integer(3),
                    dst: beam_loader::Operand::X(1),
                },
                Instr::Move {
                    src: beam_loader::Operand::Atom(erlang),
                    dst: beam_loader::Operand::X(2),
                },
                Instr::Move {
                    src: beam_loader::Operand::Atom(plus),
                    dst: beam_loader::Operand::X(3),
                },
                Instr::Call {
                    arity: 4,
                    target: Label(2),
                },
                Instr::Deallocate(0),
                Instr::Return,
            ],
        );
        assert_eq!(callee, Label(2));
        let module = fixture.finish();

        let mut machine = Machine::new();
        let mut ctx = MinimalContext::with_bif_hook(Box::new(plus_hook));
        let result = machine
            .run_label(&module, entry, 0, &[], &mut ctx)
            .expect("apply_last");
        assert_eq!(result, make_small(5));
        assert_eq!(machine.frame_depth(), 1, "the tail call popped its frame");
    }

    #[test]
    fn apply_last_reports_a_context_switch_when_the_continuation_is_in_another_module() {
        // A tail `apply_last` whose frame's continuation names a DIFFERENT module
        // context must be reported as `Step::Enter` so the driver re-binds that
        // module before executing `(func, pc)`. Reporting a bare `Step::Goto` would
        // make the driver resolve those indices in the module currently executing —
        // the shape reached when the function was entered through the cross-module
        // `resolve_external_call` seam, whose return address belongs to the caller's
        // module.
        let mut fixture = Fixture::new();
        let erlang = fixture.atom("erlang");
        let self_atom = fixture.atom("self");
        let module = fixture.finish();

        let mut machine = Machine::new();
        // The CALLER's frame: its continuation is an address in module context `0`.
        machine.push_frame(0).unwrap();
        machine
            .set_continuation(Continuation::Address {
                func: 7,
                pc: 3,
                context: 0,
            })
            .unwrap();
        // The `apply_last`'s own frame — popped by the instruction.
        machine.push_frame(0).unwrap();
        // Execute in a registered callee context (id `1`), so the current context
        // differs from the continuation's `0`.
        let callee = Arc::new(Module::default());
        let id = machine.register_context(&callee);
        assert_eq!(id, 1, "the first registered context is id 1");
        machine.switch_context(id).unwrap();
        // `apply_last 0` reads x(0) = module, x(1) = function.
        machine.set_x(0, make_atom(erlang)).unwrap();
        machine.set_x(1, make_atom(self_atom)).unwrap();

        let mut ctx = MinimalContext::with_bif_hook(Box::new(|m, f, a, args| {
            assert_eq!((m, f, a, args.len()), ("erlang", "self", 0, 0));
            Ok(make_small(9))
        }));
        let instr = Instr::ApplyLast {
            arity: 0,
            deallocate: 0,
        };
        let labels = LabelMap::new();
        let step =
            exec(&mut machine, &module, &labels, &instr, 0, 0, &mut ctx).expect("apply_last runs");
        assert_eq!(
            step,
            Some(Step::Enter {
                context: 0,
                func: 7,
                pc: 3,
            }),
            "the continuation lives in context 0, not the callee's context 1"
        );
        assert_eq!(machine.x(0), Some(make_small(9)));
        assert_eq!(machine.frame_depth(), 1, "the `apply_last` frame is popped");
    }

    #[test]
    fn apply_last_at_the_bottom_frame_finishes_the_run() {
        // `f/2` allocates its own frame and its `apply_last 0 0` pops it, leaving
        // the bottom frame whose continuation is `Continuation::Halt` ⇒ `Step::Done`.
        let mut fixture = Fixture::new();
        let erlang = fixture.atom("erlang");
        let self_atom = fixture.atom("self");
        let entry = fixture.add_function(
            "f",
            2,
            vec![
                Instr::Label(Label(1)),
                Instr::Allocate {
                    stack_need: beam_loader::HeapNeed::Words(0),
                    live: 2,
                },
                Instr::ApplyLast {
                    arity: 0,
                    deallocate: 0,
                },
            ],
        );
        let module = fixture.finish();

        let mut machine = Machine::new();
        let mut ctx = MinimalContext::with_bif_hook(Box::new(|m, f, a, args| {
            assert_eq!((m, f, a, args.len()), ("erlang", "self", 0, 0));
            Ok(make_small(7))
        }));
        // `apply_last 0` reads x(0) = module, x(1) = function.
        let result = machine
            .run_label(
                &module,
                entry,
                2,
                &[
                    beam_term::make_atom(erlang),
                    beam_term::make_atom(self_atom),
                ],
                &mut ctx,
            )
            .expect("apply_last 0");
        assert_eq!(result, make_small(7));
        assert_eq!(machine.frame_depth(), 1, "only the bottom frame remains");
    }

    #[test]
    fn non_fun_variants_return_none() {
        let mut fixture = Fixture::new();
        fixture.add_function("f", 0, vec![Instr::Label(Label(1)), Instr::Return]);
        let module = fixture.finish();

        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let labels = LabelMap::new();
        let outcome = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::Return,
            0,
            0,
            &mut ctx,
        )
        .expect("return is not this family's variant");
        assert!(outcome.is_none());
    }
}
