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
//! Synthetic-module unit tests for every fun opcode, built by hand (the loader
//! is not involved), plus the documented error paths. The integration test over
//! a real `.beam` lives in `tests/funs.rs`.

use super::*;
use beam_common::BeamError;
use beam_loader::{Export, FunDescriptor, Function, Label};
use beam_term::{make_small, signed_value, Atom};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use crate::context::{ExternalResolver, Mailbox, MinimalContext};
use crate::machine::{CatchFrame, Continuation};
use crate::RunOutcome;

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

    /// A fixture whose module is named `name` (the cross-module seam tests use a
    /// module per side, and the name is what a host matches on).
    fn named(name: &str) -> Fixture {
        let mut fixture = Fixture::new();
        fixture.module.name = fixture.atom(name);
        fixture
    }

    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    /// Export `name/arity` at `label` (what `resolve_external_call` answers with).
    fn add_export(&mut self, name: &str, arity: u32, label: Label) {
        let function = self.atom(name);
        self.module.exports.push(Export {
            function,
            arity,
            label,
        });
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
    // The target must be a **real** closure of the called arity: C's `call_fun`
    // classifies the target before dispatching (`is_not_any_fun` ⇒
    // `EXC_BADFUN`, a header-arity mismatch ⇒ `EXC_BADARITY`), so a small int in
    // `x(arity)` would now raise instead of reaching the hook.
    let fun = terms::make_fun(&mut ctx, 1, 0, 0, &[]).expect("materialize the closure");
    let result = machine
        .run_label(&module, entry, 2, &[make_small(4), fun], &mut ctx)
        .expect("call_fun");
    assert_eq!(result, make_small(5));
}

#[test]
fn call_fun_without_an_apply_hook_is_a_typed_unsupported() {
    // A host with neither the closure seam nor an apply hook cannot dispatch the
    // closure: the value-returning `apply_fun` fallback is the trait's typed
    // `unsupported`, routed through the machine as before.
    let mut fixture = Fixture::new();
    fixture.add_fun("lambda", 0, 0, 1);
    let entry = fixture.add_function(
        "f",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::MakeFun3 {
                index: 0,
                dst: beam_loader::Operand::X(0),
                env: Vec::new(),
            },
            Instr::CallFun { arity: 0 },
            Instr::Return,
        ],
    );
    let module = fixture.finish();

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let err = machine
        .run_label(&module, entry, 0, &[], &mut ctx)
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
    // A 0-ary closure, so the header arity matches the `call_fun2` call arity.
    let fun = terms::make_fun(&mut ctx, 0, 0, 0, &[]).expect("materialize the closure");
    let result = machine
        .run_label(&module, entry, 1, &[fun], &mut ctx)
        .expect("call_fun2");
    assert_eq!(result, make_small(42));
}

// -----------------------------------------------------------------------
// The structured target-shape reasons (`{badfun,_}` / `{badarity,_}`)
// -----------------------------------------------------------------------

/// Read the `{Tag, Value}` reason out of a landed raise and return `Value`,
/// asserting the tag atom.
///
/// The landing convention stores the reason in `x(1)` (`Machine::land_exception`).
fn reason_value(module: &Module, reason: Eterm, tag: &str) -> Eterm {
    let tag_atom = module
        .atom_table
        .lookup(tag)
        .expect("the fixture interns the reason tag");
    assert_eq!(
        terms::tuple_arity(reason).expect("the reason is a boxed tuple"),
        2,
        "the reason `{{{tag}, Value}}` must be a 2-tuple"
    );
    assert_eq!(
        terms::tuple_element(reason, 0).expect("element 0"),
        make_atom(tag_atom),
        "the reason tag"
    );
    terms::tuple_element(reason, 1).expect("element 1")
}

/// `f(F) -> F(1).` — the `call_fun 1` shape with the target in `x(1)`.
fn call_fun_caller(fixture: &mut Fixture) -> Label {
    fixture.add_function(
        "f",
        2,
        vec![
            Instr::Label(Label(1)),
            Instr::CallFun { arity: 1 },
            Instr::Return,
        ],
    )
}

#[test]
fn call_fun_with_a_non_fun_target_raises_badfun() {
    // The oracle (OTP 29.1.1): `f(F) -> F(1).` then `f(5)` ⇒ `{error,{badfun,5}}`
    // and `f(notafun)` ⇒ `{error,{badfun,notafun}}`.
    //
    // C's `call_fun` (`beam_common.c:1616-1619`) classifies the target before
    // dispatching: `is_not_any_fun(fun)` sets `freason = EXC_BADFUN` and puts the
    // offending term in `fvalue`, and `expand_error_value` (`:754-768`) wraps it as
    // `TUPLE2(am_badfun, fvalue)` under class `error`.
    let mut fixture = Fixture::new();
    fixture.atom("badfun");
    fixture.atom("error");
    let entry = call_fun_caller(&mut fixture);
    let module = fixture.finish();

    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(4, 6, 0));
    let mut ctx = MinimalContext::new();
    // x(1) = the target `5` (a small int, NOT a fun); x(0) = the call argument.
    machine.set_x(0, make_small(1)).unwrap();
    machine.set_x(1, make_small(5)).unwrap();

    let step = exec(
        &mut machine,
        &module,
        &module,
        &LabelMap::new(),
        &Instr::CallFun { arity: 1 },
        0,
        0,
        &mut ctx,
    )
    .expect("the raise lands in the live catch frame");
    assert_eq!(step, Some(Step::Goto { func: 4, pc: 6 }));
    assert_eq!(machine.x(0), Some(Eterm::non_value()), "x(0) = THE_NON_VALUE");
    assert_eq!(
        reason_value(&module, machine.x(1).unwrap(), "badfun"),
        make_small(5),
        "the offending TERM rides the reason (C's `fvalue`)"
    );
    let _ = entry;
}

#[test]
fn call_fun2_with_a_non_fun_operand_raises_badfun() {
    // The `call_fun2` route reads the target from its `Func` operand; the same
    // structured `{badfun, Fun}` reason applies (the operand sink must re-read the
    // operand through the slack, not hold the pre-reserve term).
    let mut fixture = Fixture::new();
    let safe = fixture.atom("safe");
    fixture.atom("badfun");
    fixture.atom("error");
    fixture.add_function(
        "f",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::CallFun2 {
                tag: beam_loader::Operand::Atom(safe),
                arity: 0,
                func: beam_loader::Operand::X(1),
            },
            Instr::Return,
        ],
    );
    let module = fixture.finish();

    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(4, 6, 0));
    let mut ctx = MinimalContext::new();
    machine.set_x(1, make_small(9)).unwrap();

    let step = exec(
        &mut machine,
        &module,
        &module,
        &LabelMap::new(),
        &Instr::CallFun2 {
            tag: beam_loader::Operand::Atom(safe),
            arity: 0,
            func: beam_loader::Operand::X(1),
        },
        0,
        0,
        &mut ctx,
    )
    .expect("the raise lands in the live catch frame");
    assert_eq!(step, Some(Step::Goto { func: 4, pc: 6 }));
    assert_eq!(
        reason_value(&module, machine.x(1).unwrap(), "badfun"),
        make_small(9)
    );
}

#[test]
fn call_fun_with_a_wrong_arity_fun_raises_badarity() {
    // The oracle (OTP 29.1.1): `am1(F, A) -> F(A).` then
    // `am1(fun(X, Y) -> X end, one)` ⇒ `{error,{badarity,{#Fun<…>,[one]}}}` — C's
    // arity mismatch (`beam_common.c:1668-1674`) sets `fvalue = TUPLE2(fun, args)`
    // with the **actual** argument list, wrapped as `{badarity, _}`.
    let mut fixture = Fixture::new();
    fixture.atom("badarity");
    fixture.atom("error");
    let one = fixture.atom("one");
    let entry = call_fun_caller(&mut fixture);
    let module = fixture.finish();

    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(4, 6, 0));
    // The closure-apply seam must never be reached: the arity guard precedes it, so a
    // mismatched-arity closure can never be entered with the wrong argument count.
    // (The resolver is installed to *prove* that: it counts its own consultations.)
    let consulted = Rc::new(RefCell::new(0u32));
    let counter = Rc::clone(&consulted);
    let mut ctx = MinimalContext::with_local_fun_resolver(Box::new(move |_fun, _args| {
        *counter.borrow_mut() += 1;
        Ok(None)
    }));
    // A genuine 2-ary local closure (the `make_fun3` header arity is the descriptor
    // arity minus the captured variables: 2 - 0), called with ONE argument.
    let fun = terms::make_fun(&mut ctx, 2, 0, 0, &[]).expect("materialize the closure");
    machine.set_x(0, make_atom(one)).unwrap();
    machine.set_x(1, fun).unwrap();

    let step = exec(
        &mut machine,
        &module,
        &module,
        &LabelMap::new(),
        &Instr::CallFun { arity: 1 },
        0,
        0,
        &mut ctx,
    )
    .expect("the raise lands in the live catch frame");
    assert_eq!(step, Some(Step::Goto { func: 4, pc: 6 }));
    assert_eq!(*consulted.borrow(), 0, "the seam was never consulted");
    assert_eq!(machine.x(0), Some(Eterm::non_value()), "x(0) = THE_NON_VALUE");
    let inner = reason_value(&module, machine.x(1).unwrap(), "badarity");
    assert_eq!(
        terms::tuple_arity(inner).expect("`{Fun, Args}` is a 2-tuple"),
        2
    );
    assert_eq!(
        terms::tuple_element(inner, 0).expect("the fun"),
        fun,
        "the offending FUN rides the reason"
    );
    assert_eq!(
        terms::list_elements(terms::tuple_element(inner, 1).expect("the args list"))
            .expect("a proper list"),
        vec![make_atom(one)],
        "the ACTUAL argument list rides the reason"
    );
    let _ = entry;
}

#[test]
fn a_wrong_arity_fun_never_reaches_the_seam_when_the_module_cannot_name_badarity() {
    // The arity test is **unconditional** — only the structured *reason* depends on
    // the module naming `badarity` (`error_atom` is a global table in C; this engine's
    // atoms are module-local indices). An atom-poor module therefore still refuses the
    // mismatched closure: the guard reports the typed `unsupported` naming the missing
    // atom, and the closure-apply seam is never consulted. Gating the *comparison* on
    // the lookup instead (the shape this pins against) would hand the host a
    // wrong-arity closure and, with a host that does not re-validate, enter the lambda
    // body with the wrong argument count — a wrong value, not a typed error.
    let mut fixture = Fixture::new();
    // Deliberately NO `badarity` atom: the module cannot name the reason tag.
    let entry = call_fun_caller(&mut fixture);
    let module = fixture.finish();
    assert!(
        module.atom_table.lookup("badarity").is_none(),
        "the fixture must not name the reason tag"
    );

    // The resolver records that it was consulted; the guard must keep it at zero.
    let consulted = Rc::new(RefCell::new(0u32));
    let counter = Rc::clone(&consulted);
    let mut ctx = MinimalContext::with_local_fun_resolver(Box::new(move |_fun, _args| {
        *counter.borrow_mut() += 1;
        Ok(None)
    }));

    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(4, 6, 0));
    // A genuine 2-ary closure called with ONE argument.
    let fun = terms::make_fun(&mut ctx, 2, 0, 0, &[]).expect("materialize the closure");
    machine.set_x(0, make_small(1)).unwrap();
    machine.set_x(1, fun).unwrap();

    let err = exec(
        &mut machine,
        &module,
        &module,
        &LabelMap::new(),
        &Instr::CallFun { arity: 1 },
        0,
        0,
        &mut ctx,
    )
    .expect_err("an unnameable `badarity` degrades the reason to a typed error");
    assert_eq!(
        *consulted.borrow(),
        0,
        "the arity guard runs before the closure-apply seam, so the seam is never consulted"
    );
    match err {
        BeamError::Raise(raise) => assert_eq!(
            raise.reason.as_atom(),
            Some("unsupported_opcode: call_fun: no 'badarity' atom in the module"),
            "the refusal names the missing atom; it never substitutes one"
        ),
        other => panic!("expected the typed unsupported, got {other:?}"),
    }
    assert_eq!(
        machine.catch_frame_depth(),
        1,
        "a capability gap cannot land in a catch frame"
    );
    let _ = entry;
}

#[test]
fn call_fun_with_a_matching_arity_still_reaches_the_apply_fun_fallback() {
    // The shape guard must not reject a well-formed closure: a 1-ary fun called
    // with one argument reaches the value-returning `apply_fun` fallback exactly as
    // before (the regression guard for the structured-reason check).
    let mut fixture = Fixture::new();
    fixture.atom("badfun");
    fixture.atom("badarity");
    let entry = call_fun_caller(&mut fixture);
    let module = fixture.finish();

    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    let mut ctx = MinimalContext::with_apply_hook(Box::new(|fun, args| {
        assert!(terms::is_fun(fun), "the fallback receives the genuine fun");
        assert_eq!(args, [make_small(7)]);
        Ok(make_small(41))
    }));
    let fun = terms::make_fun(&mut ctx, 1, 0, 0, &[]).expect("materialize the closure");
    machine.set_x(0, make_small(7)).unwrap();
    machine.set_x(1, fun).unwrap();

    let step = exec(
        &mut machine,
        &module,
        &module,
        &LabelMap::new(),
        &Instr::CallFun { arity: 1 },
        0,
        0,
        &mut ctx,
    )
    .expect("a matching arity dispatches");
    assert_eq!(step, Some(Step::Next));
    assert_eq!(machine.x(0), Some(make_small(41)));
    let _ = entry;
}

#[test]
fn call_fun_without_the_badfun_atom_declines_to_the_apply_fun_fallback() {
    // The engine's atom-independence convention: a running module that cannot name
    // the reason atom never substitutes a different atom. There is no structured
    // reason to raise, so the target-shape classification declines and the call
    // falls through to the host closure-apply seam exactly as before the shape
    // check existed — a non-fun target over a `MinimalContext` (no hook) therefore
    // reaches the trait-default `apply_fun` ⇒ the typed `unsupported_opcode:
    // apply_fun`.
    let mut fixture = Fixture::new();
    let entry = call_fun_caller(&mut fixture);
    let module = fixture.finish();

    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(4, 6, 0));
    machine.set_x(0, make_small(1)).unwrap();
    machine.set_x(1, make_small(5)).unwrap();
    let mut ctx = MinimalContext::new();

    let err = exec(
        &mut machine,
        &module,
        &module,
        &LabelMap::new(),
        &Instr::CallFun { arity: 1 },
        0,
        0,
        &mut ctx,
    )
    .expect_err("the module cannot name `badfun`, so the fallback is consulted");
    let got = match err {
        BeamError::Raise(raise) => raise
            .reason
            .as_atom()
            .unwrap_or("<non-atom reason>")
            .to_owned(),
        other => panic!("expected the typed unsupported, got {other:?}"),
    };
    assert_eq!(
        got, "unsupported_opcode: apply_fun",
        "an unnameable reason tag declines to the `apply_fun` fallback"
    );
    assert_eq!(machine.catch_frame_depth(), 1, "the handler was not used");
    let _ = entry;
}

#[test]
fn call_fun_with_a_non_fun_without_a_catch_frame_keeps_the_live_reason() {
    // Uncaught, the structured reason propagates on the raise's live-reason slot
    // (as `{badmatch,_}`/`{badrecord,_}` do) — never collapsed to a bare atom.
    let mut fixture = Fixture::new();
    fixture.atom("badfun");
    fixture.atom("error");
    let entry = call_fun_caller(&mut fixture);
    let module = fixture.finish();

    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    let mut ctx = MinimalContext::new();
    machine.set_x(0, make_small(1)).unwrap();
    machine.set_x(1, make_small(5)).unwrap();

    let err = exec(
        &mut machine,
        &module,
        &module,
        &LabelMap::new(),
        &Instr::CallFun { arity: 1 },
        0,
        0,
        &mut ctx,
    )
    .expect_err("no catch frame");
    match err {
        BeamError::Raise(raise) => {
            assert_eq!(raise.class, ExceptionClass::Error, "class `error`");
            let reason = raise
                .live_reason()
                .and_then(|live| live.downcast_ref::<Eterm>())
                .copied()
                .expect("the live reason term");
            assert_eq!(reason_value(&module, reason, "badfun"), make_small(5));
        }
        other => panic!("expected a class-`error` raise, got {other:?}"),
    }
    let _ = entry;
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
    let step = exec(&mut machine, &module, &module, &labels, &instr, 0, 0, &mut ctx).unwrap();
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
    let step = exec(&mut machine, &module, &module, &labels, &instr, 0, 0, &mut ctx).unwrap();
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
    let err = exec(&mut machine, &module, &module, &labels, &instr, 0, 0, &mut ctx)
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
    let err = exec(&mut machine, &module, &module, &labels, &instr, 0, 0, &mut ctx)
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
        exec(&mut machine, &module, &module, &labels, &instr, 0, 0, &mut ctx).expect("apply_last runs");
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

// -----------------------------------------------------------------------
// Apply / ApplyLast through the cross-module call seam
// (`ExecutionContext::resolve_external_call`)
// -----------------------------------------------------------------------

/// The cross-module `apply` callee `c:g/2`: a bare `receive` that finds nothing in
/// an empty mailbox, suspends, and returns the message it later matched.
///
/// ```text
/// c:g/2:
///     label 10
///     loop_rec 11 x0       ; the candidate, or jump to the wait
///     remove_message
///     return
///     label 11
///     wait 10              ; resume at the loop_rec once a message arrives
/// ```
fn cross_callee() -> Arc<Module> {
    let mut fixture = Fixture::named("c");
    let g = fixture.add_function(
        "g",
        2,
        vec![
            Instr::Label(Label(10)),
            Instr::LoopRec {
                fail: Label(11),
                dst: beam_loader::Operand::X(0),
            },
            Instr::RemoveMessage,
            Instr::Return,
            Instr::Label(Label(11)),
            Instr::Wait { target: Label(10) },
        ],
    );
    fixture.add_export("g", 2, g);
    Arc::new(fixture.finish())
}

/// The callee's `c:g/2` export entry — the `Label` the seam answers with (the
/// label *after* `func_info`, i.e. exactly the label `run_export` would enter).
fn cross_entry(callee: &Arc<Module>) -> Label {
    let g = callee.atom_table.lookup("g").expect("`g` is interned");
    callee.export(g, 2).expect("`c:g/2` is exported")
}

/// Intern the `c`/`g` atoms in a caller fixture's own table.
fn cross_atoms(fixture: &mut Fixture) -> (Atom, Atom) {
    (fixture.atom("c"), fixture.atom("g"))
}

/// A resolver that answers `c:g/2` with the callee handle and its export entry and
/// declines everything else (`Ok(None)` — the synchronous `call_bif` route).
fn cross_resolver(callee: Arc<Module>, label: Label) -> ExternalResolver {
    Box::new(move |module, function, arity| {
        if module == "c" && function == "g" && arity == 2 {
            Ok(Some((Arc::clone(&callee), label)))
        } else {
            Ok(None)
        }
    })
}

/// A caller `f/4` that performs `apply c:g/2` with `x(2)`/`x(3)` = the `c`/`g`
/// atoms — the `fixed_apply` register shape.
fn apply_caller(fixture: &mut Fixture) -> Label {
    let (c, g) = cross_atoms(fixture);
    fixture.add_function(
        "f",
        4,
        vec![
            Instr::Label(Label(1)),
            Instr::Move {
                src: beam_loader::Operand::Atom(c),
                dst: beam_loader::Operand::X(2),
            },
            Instr::Move {
                src: beam_loader::Operand::Atom(g),
                dst: beam_loader::Operand::X(3),
            },
            Instr::Apply { arity: 2 },
            Instr::Return,
        ],
    )
}

/// A caller `run/0` that allocates a frame (so the `apply`'s return address is
/// written there), sets up the `apply c:g/2` target and returns through its frame.
fn apply_run_caller() -> (Module, Label) {
    let mut fixture = Fixture::new();
    let (c, g) = cross_atoms(&mut fixture);
    let entry = fixture.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: beam_loader::HeapNeed::Words(0),
                live: 4,
            },
            Instr::Move {
                src: beam_loader::Operand::Atom(c),
                dst: beam_loader::Operand::X(2),
            },
            Instr::Move {
                src: beam_loader::Operand::Atom(g),
                dst: beam_loader::Operand::X(3),
            },
            Instr::Apply { arity: 2 },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    (fixture.finish(), entry)
}

/// Seed the `apply c:g/2` argument registers `x(0)`/`x(1)`.
fn seed_apply_args(machine: &mut Machine) {
    machine.set_x(0, make_small(7)).unwrap();
    machine.set_x(1, make_small(8)).unwrap();
}

/// Seed the `apply c:g/2` target registers `x(2)`/`x(3)`.
fn seed_apply_target(machine: &mut Machine, c: Atom, g: Atom) {
    machine.set_x(2, make_atom(c)).unwrap();
    machine.set_x(3, make_atom(g)).unwrap();
}

#[test]
fn apply_enters_a_resolved_callee_in_the_same_machine() {
    // A host that answers `resolve_external_call` runs the applied callee in THIS
    // machine: `apply c:g/2` registers the callee's module context, stores the
    // caller's return address `(func, pc + 1)` in the live frame, and reports
    // `Step::Enter` — exactly the local-call discipline, so a `receive` inside the
    // callee suspends *this* process instead of folding into `error::blocked()` at
    // the call site.
    let callee = cross_callee();
    let entry = cross_entry(&callee);
    let mut fixture = Fixture::new();
    let (c, g) = cross_atoms(&mut fixture);
    let label = apply_caller(&mut fixture);
    assert_eq!(label, Label(1));
    let module = fixture.finish();

    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    seed_apply_args(&mut machine);
    seed_apply_target(&mut machine, c, g);

    let mut ctx = MinimalContext::with_external_resolver(cross_resolver(callee, entry));
    let labels = LabelMap::new();
    // The caller's own position is `(func 5, pc 7)`, so its return address is the
    // `(5, 8)` the Apply must store.
    let step = exec(
        &mut machine,
        &module,
        &module,
        &labels,
        &Instr::Apply { arity: 2 },
        5,
        7,
        &mut ctx,
    )
    .expect("the applied target resolves");
    assert_eq!(
        step,
        Some(Step::Enter {
            context: 1,
            func: 0,
            pc: 0,
        }),
        "the callee is entered in its own registered context"
    );
    assert_eq!(
        machine.continuation().unwrap(),
        Continuation::Address {
            func: 5,
            pc: 8,
            context: 0,
        },
        "the caller's return address is stored in the live frame"
    );
}

#[test]
fn apply_into_a_cross_module_receive_suspends_and_resumes() {
    // End to end through the driver loop: `apply c:g/2` enters the callee, whose
    // bare `receive` finds an empty mailbox and suspends THIS process — reported as
    // `RunOutcome::Wait` at the callee's own address, never as the folded
    // `error::blocked()` the non-blocking entries surface. A message then resumes
    // the process and the callee's `return` unwinds into the caller's frame.
    let callee = cross_callee();
    let entry = cross_entry(&callee);
    let (module, run) = apply_run_caller();

    let mut ctx = MinimalContext::new();
    ctx.set_external_resolver(cross_resolver(callee, entry));
    let messages = Rc::new(RefCell::new(Vec::new()));
    ctx.set_mailbox(Box::new(TestMailbox::new(Rc::clone(&messages))));

    let mut machine = Machine::new();
    let suspended = machine
        .run_label_outcome(&module, run, 0, &[], &mut ctx)
        .expect("a suspend is not an error");
    assert_eq!(
        suspended,
        RunOutcome::Wait { func: 0, pc: 0 },
        "the block is the callee's, not an error at the call site"
    );

    messages.borrow_mut().push(make_small(42));
    let resumed = machine
        .resume_outcome(&module, 0, 0, &mut ctx)
        .expect("the resume completes");
    assert_eq!(
        resumed,
        RunOutcome::Returned(make_small(42)),
        "the callee returns the matched message into the caller"
    );
}

#[test]
fn apply_without_the_seam_keeps_the_synchronous_bif_path() {
    // A host that does not implement `resolve_external_call` (the trait's
    // `Ok(None)` default) is byte-identical to before: the applied result lands in
    // `x(0)`, the instruction is a plain `Next`, and the live frame is untouched
    // (no return address is written).
    let mut fixture = Fixture::new();
    let (c, g) = cross_atoms(&mut fixture);
    let label = apply_caller(&mut fixture);
    assert_eq!(label, Label(1));
    let module = fixture.finish();

    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    seed_apply_args(&mut machine);
    seed_apply_target(&mut machine, c, g);

    // No resolver installed => `resolve_external_call` stays the `Ok(None)` default.
    let mut ctx = MinimalContext::with_bif_hook(Box::new(|module, function, arity, args| {
        assert_eq!((module, function, arity), ("c", "g", 2));
        assert_eq!(args, &[make_small(7), make_small(8)][..]);
        Ok(make_small(99))
    }));
    let labels = LabelMap::new();
    let step = exec(
        &mut machine,
        &module,
        &module,
        &labels,
        &Instr::Apply { arity: 2 },
        5,
        7,
        &mut ctx,
    )
    .expect("the synchronous target runs");
    assert_eq!(
        step,
        Some(Step::Next),
        "the synchronous route reports no context switch"
    );
    assert_eq!(machine.x(0), Some(make_small(99)));
    assert_eq!(
        machine.continuation().unwrap(),
        Continuation::Halt,
        "the synchronous route leaves the frame's continuation untouched"
    );
}

#[test]
fn apply_last_pops_its_frame_then_enters_a_resolved_callee() {
    // The tail discipline with the seam: the frame is popped FIRST (`i_apply_last`'s
    // `$deallocate` precedes `fixed_apply`), then the callee is entered as
    // `Step::Enter`. The callee therefore runs on the CALLER's frame, so its
    // `return` follows that frame's (possibly cross-context) continuation — never
    // the popped frame's.
    let callee = cross_callee();
    let entry = cross_entry(&callee);
    let mut fixture = Fixture::new();
    let (c, g) = cross_atoms(&mut fixture);
    let label = fixture.add_function(
        "f",
        4,
        vec![
            Instr::Label(Label(1)),
            Instr::ApplyLast {
                arity: 2,
                deallocate: 0,
            },
        ],
    );
    assert_eq!(label, Label(1));
    let module = fixture.finish();

    let mut machine = Machine::new();
    // The CALLER's frame: its continuation is an address in module context `0`,
    // the context the tail `apply_last` must return into.
    machine.push_frame(0).unwrap();
    machine
        .set_continuation(Continuation::Address {
            func: 9,
            pc: 4,
            context: 0,
        })
        .unwrap();
    // The `apply_last`'s own frame — popped by the instruction.
    machine.push_frame(0).unwrap();
    seed_apply_args(&mut machine);
    seed_apply_target(&mut machine, c, g);

    let mut ctx = MinimalContext::with_external_resolver(cross_resolver(callee, entry));
    let labels = LabelMap::new();
    let step = exec(
        &mut machine,
        &module,
        &module,
        &labels,
        &Instr::ApplyLast {
            arity: 2,
            deallocate: 0,
        },
        0,
        1,
        &mut ctx,
    )
    .expect("the tail apply resolves");
    assert_eq!(
        step,
        Some(Step::Enter {
            context: 1,
            func: 0,
            pc: 0,
        }),
        "the callee is entered directly, with no return address stored"
    );
    assert_eq!(machine.frame_depth(), 1, "the apply_last frame is popped");
    assert_eq!(
        machine.continuation().unwrap(),
        Continuation::Address {
            func: 9,
            pc: 4,
            context: 0,
        },
        "the callee's return follows the caller's continuation"
    );
}

// -----------------------------------------------------------------------
// The closure-apply seam (`ExecutionContext::resolve_local_fun`)
// -----------------------------------------------------------------------

/// A resolver that answers **any** fun with the given callee handle and lambda
/// entry label (the in-machine closure-apply seam).
///
/// The seam is handed the fun term and the call args; this test host ignores the
/// term and always answers with the closure module/label it was built for — the
/// engine reads the free variables from the term itself.
fn local_fun_resolver(callee: Arc<Module>, label: Label) -> crate::context::LocalFunResolver {
    Box::new(move |_fun, _args| Ok(Some((Arc::clone(&callee), label))))
}

#[test]
fn resolve_local_fun_defaults_to_none() {
    // The trait's default body declines every closure — a host that does not
    // override it keeps the value-returning `apply_fun` route unchanged.
    let mut ctx = MinimalContext::new();
    assert!(ctx
        .resolve_local_fun(make_small(0), &[make_small(1)])
        .unwrap()
        .is_none());
}

#[test]
fn call_fun_enters_a_resolved_local_closure_in_the_same_machine() {
    // A host that answers `resolve_local_fun` runs the closure **in this machine**:
    // the engine registers the closure's module context, places the call arguments
    // (already in `x(0..arity)`) and the closure's free variables at `x(arity..)`
    // (C's `reg[i + arity] = funp->env[i]`), stores the caller's return address in
    // the live frame, and reports `Step::Enter`.
    //
    // The lambda entry `g/2` accepts `arity + num_free` = 2 args: `x(0)` is the call
    // argument, `x(1)` the free variable. `g` returns `x(1)` verbatim, so the test
    // proves the *free variable* reached `x(1)` by comparing it after the call.
    let mut callee = Fixture::named("c");
    // `g/2` returns its **second** register (`x(1)` — the free variable), so a
    // correct placement yields the free variable's value, not the call argument.
    let g = callee.add_function(
        "g",
        2,
        vec![
            Instr::Label(Label(20)),
            Instr::Move {
                src: beam_loader::Operand::X(1),
                dst: beam_loader::Operand::X(0),
            },
            Instr::Return,
        ],
    );
    let callee = Arc::new(callee.finish());
    let module = Fixture::new().finish();

    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    let mut ctx = MinimalContext::with_local_fun_resolver(local_fun_resolver(callee, g));
    // x(0) = the call argument (7); x(1) = a one-free-variable local closure whose
    // capture is 99 — the `call_fun Arity` shape puts the fun in `x(arity)`.
    let fun = terms::make_fun(&mut ctx, 1, 0, 0, &[make_small(99)]).expect("materialize the closure");
    machine.set_x(0, make_small(7)).unwrap();
    machine.set_x(1, fun).unwrap();

    let labels = LabelMap::new();
    let step = exec(
        &mut machine,
        &module,
        &module,
        &labels,
        &Instr::CallFun { arity: 1 },
        5,
        7,
        &mut ctx,
    )
    .expect("the closure resolves");
    assert_eq!(
        step,
        Some(Step::Enter {
            context: 1,
            func: 0,
            pc: 0,
        }),
        "the closure body is entered in its own registered context"
    );
    // The free variable landed at `x(arity)` = `x(1)`, the call argument stayed in x(0).
    assert_eq!(machine.x(0), Some(make_small(7)), "the call argument stayed put");
    assert_eq!(machine.x(1), Some(make_small(99)), "the free variable landed at x(arity)");
    assert_eq!(
        machine.continuation().unwrap(),
        Continuation::Address {
            func: 5,
            pc: 8,
            context: 0,
        },
        "the caller's return address is stored in the live frame"
    );
}

#[test]
fn call_fun_without_the_seam_uses_the_value_returning_fallback() {
    // A host with no `resolve_local_fun` hook falls back to `apply_fun`, which is
    // byte-identical to the pre-seam behaviour: the hook's value is stored in
    // `x(0)` and the instruction reports `Step::Next`.
    let mut fixture = Fixture::new();
    fixture.add_fun("lambda", 1, 0, 1);
    let entry = fixture.add_function(
        "f",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::MakeFun3 {
                index: 0,
                dst: beam_loader::Operand::X(1),
                env: Vec::new(),
            },
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
        .expect("the fallback applies the closure");
    assert_eq!(result, make_small(5));
}

#[test]
fn call_fun2_enters_a_resolved_local_closure_via_its_operand() {
    // `call_fun2` reads the closure from its `Func` operand and uses the same
    // in-machine seam as `call_fun`.
    let mut callee = Fixture::named("c");
    let g = callee.add_function(
        "g",
        1,
        vec![
            Instr::Label(Label(30)),
            Instr::Move {
                src: beam_loader::Operand::X(0),
                dst: beam_loader::Operand::X(0),
            },
            Instr::Return,
        ],
    );
    let callee = Arc::new(callee.finish());

    let mut fixture = Fixture::new();
    let unspec = fixture.atom("unsafe");
    let module = fixture.finish();

    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    let mut ctx = MinimalContext::with_local_fun_resolver(local_fun_resolver(callee, g));
    // The closure (no free variables) sits in `x(1)`, named by the `Func` operand.
    let fun = terms::make_fun(&mut ctx, 1, 0, 0, &[]).expect("materialize the closure");
    machine.set_x(1, fun).unwrap();
    // `unsafe` is the compiler's type hint (a fun-table index or a safety atom); the
    // interpreter ignores it (the loader rewrites `call_fun2` to `call_fun` + `move`).
    let _ = unspec;

    let labels = LabelMap::new();
    let step = exec(
        &mut machine,
        &module,
        &module,
        &labels,
        &Instr::CallFun2 {
            tag: beam_loader::Operand::Atom(unspec),
            arity: 1,
            func: beam_loader::Operand::X(1),
        },
        2,
        3,
        &mut ctx,
    )
    .expect("the closure resolves");
    assert_eq!(
        step,
        Some(Step::Enter {
            context: 1,
            func: 0,
            pc: 0,
        })
    );
    assert_eq!(
        machine.continuation().unwrap(),
        Continuation::Address {
            func: 2,
            pc: 4,
            context: 0,
        }
    );
}

#[test]
fn a_resolve_local_fun_error_lands_in_a_live_catch_frame() {
    // The third defect: a raise from the seam routes through the catch-aware funnel
    // (`Machine::raise_bif_error` → `Machine::throw_with_trace`), so it lands in the
    // caller's live `try`/`catch` exactly as C's `handle_error` → `next_catch` does,
    // instead of escaping as a `?`-propagated typed error.
    //
    // The target must be a **matching-arity fun** so the call reaches the seam: a
    // non-fun (or a wrong-arity fun) is classified by `call_fun` itself and never
    // consults `resolve_local_fun` (see
    // `call_fun_with_a_non_fun_target_raises_badfun`).
    let mut fixture = Fixture::new();
    fixture.atom("badarg");
    fixture.atom("error");
    fixture.add_fun("lambda", 1, 0, 1);
    let _entry = fixture.add_function(
        "f",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::MakeFun3 {
                index: 0,
                dst: beam_loader::Operand::X(1),
                env: Vec::new(),
            },
            Instr::CallFun { arity: 1 },
            Instr::Return,
        ],
    );
    let module = fixture.finish();
    let badarg = module.atom_table.lookup("badarg").expect("interned");

    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(4, 6, 0));
    let mut ctx = MinimalContext::new();
    ctx.set_local_fun_resolver(Box::new(|_fun, _args| Err(BeamError::BadArg)));
    // x(1) = a 1-ary closure, so the call arity matches and the seam is consulted.
    let fun = terms::make_fun(&mut ctx, 1, 0, 0, &[]).expect("materialize the closure");
    machine.set_x(1, fun).unwrap();

    let step = exec(
        &mut machine,
        &module,
        &module,
        &LabelMap::new(),
        &Instr::CallFun { arity: 1 },
        0,
        0,
        &mut ctx,
    )
    .expect("the seam's error is routable");
    assert_eq!(step, Some(Step::Goto { func: 4, pc: 6 }));
    // The C landing convention: x0 = THE_NON_VALUE, x1 = the reason `badarg`.
    assert_eq!(machine.x(0), Some(Eterm::non_value()));
    assert_eq!(machine.x(1), Some(make_atom(badarg)));
}

#[test]
fn a_resolve_local_fun_error_without_a_catch_frame_is_the_typed_error() {
    // With no live `try`/`catch` the raise cannot land, so the typed error
    // propagates unchanged (the uncaught-error contract).
    let mut fixture = Fixture::new();
    fixture.atom("badarg");
    fixture.atom("error");
    fixture.add_fun("lambda", 1, 0, 1);
    let _entry = fixture.add_function(
        "f",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::MakeFun3 {
                index: 0,
                dst: beam_loader::Operand::X(1),
                env: Vec::new(),
            },
            Instr::CallFun { arity: 1 },
            Instr::Return,
        ],
    );
    let module = fixture.finish();

    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    let mut ctx = MinimalContext::new();
    ctx.set_local_fun_resolver(Box::new(|_fun, _args| Err(BeamError::BadArg)));
    // A matching-arity closure, so the seam (not the shape check) is the source of
    // the error under test.
    let fun = terms::make_fun(&mut ctx, 1, 0, 0, &[]).expect("materialize the closure");
    machine.set_x(1, fun).unwrap();

    let err = exec(
        &mut machine,
        &module,
        &module,
        &LabelMap::new(),
        &Instr::CallFun { arity: 1 },
        0,
        0,
        &mut ctx,
    )
    .expect_err("no catch frame");
    assert!(matches!(err, BeamError::BadArg));
}

#[test]
fn an_unsupported_capability_gap_from_the_seam_stays_uncatchable() {
    // A genuine capability gap (`error::unsupported`, whose reason atom the running
    // module cannot name) must NOT be swallowed by the catch-aware funnel: it stays
    // the uncatchable typed error even with a live catch frame.
    let mut fixture = Fixture::new();
    fixture.atom("badarg");
    fixture.atom("error");
    fixture.add_fun("lambda", 1, 0, 1);
    let entry = fixture.add_function(
        "f",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::MakeFun3 {
                index: 0,
                dst: beam_loader::Operand::X(1),
                env: Vec::new(),
            },
            Instr::CallFun { arity: 1 },
            Instr::Return,
        ],
    );
    let module = fixture.finish();

    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(4, 6, 0));
    let mut ctx = MinimalContext::new();
    ctx.set_local_fun_resolver(Box::new(|_fun, _args| {
        Err(crate::error::unsupported("resolve_local_fun: no module"))
    }));
    // A matching-arity closure, so the seam's `unsupported` (not the shape check) is
    // the error whose catchability is under test.
    let fun = terms::make_fun(&mut ctx, 1, 0, 0, &[]).expect("materialize the closure");
    machine.set_x(1, fun).unwrap();

    let err = exec(
        &mut machine,
        &module,
        &module,
        &LabelMap::new(),
        &Instr::CallFun { arity: 1 },
        0,
        0,
        &mut ctx,
    )
    .expect_err("a capability gap stays uncatchable");
    assert!(matches!(err, BeamError::Raise(_)));
    assert_eq!(machine.catch_frame_depth(), 1, "the handler was not used");
    let _ = entry;
}

// -----------------------------------------------------------------------
// The `Mailbox` seam the cross-module receive test drives
// -----------------------------------------------------------------------

/// A minimal mailbox: a shared message `Vec` plus a scan position — enough to
/// drive `loop_rec` / `remove_message` / `wait` through the engine's receive seam.
///
/// The `Vec` is shared with the test (`Rc<RefCell<…>>`) so a message can be queued
/// *after* the process has suspended in `wait`, which is what the resume path needs.
struct TestMailbox {
    messages: Rc<RefCell<Vec<Eterm>>>,
    scan: usize,
}

impl TestMailbox {
    fn new(messages: Rc<RefCell<Vec<Eterm>>>) -> TestMailbox {
        TestMailbox { messages, scan: 0 }
    }
}

impl Mailbox for TestMailbox {
    fn send_message(&mut self, _to: Eterm, message: Eterm) -> Result<()> {
        self.messages.borrow_mut().push(message);
        Ok(())
    }

    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        Ok(self.messages.borrow().get(self.scan).copied())
    }

    fn receive_skip(&mut self) -> Result<()> {
        self.scan += 1;
        Ok(())
    }

    fn receive_remove(&mut self) -> Result<()> {
        let mut messages = self.messages.borrow_mut();
        if self.scan < messages.len() {
            messages.remove(self.scan);
        }
        self.scan = 0;
        Ok(())
    }

    fn receive_reset(&mut self) -> Result<()> {
        self.scan = 0;
        Ok(())
    }

    // The receive-marker bookkeeping is not exercised by these `apply` seam tests.
    // Reporting the typed `unsupported` keeps the seam's "this host does not serve
    // it" contract explicit rather than pretending a marker was reserved.
    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        Err(error::unsupported("receive_marker_reserve"))
    }

    fn receive_marker_bind(&mut self, _marker: Eterm, _reference: Eterm) -> Result<()> {
        Err(error::unsupported("receive_marker_bind"))
    }

    fn receive_marker_use(&mut self, _reference: Eterm) -> Result<()> {
        Err(error::unsupported("receive_marker_use"))
    }

    fn receive_marker_clear(&mut self, _reference: Eterm) -> Result<()> {
        Err(error::unsupported("receive_marker_clear"))
    }
}
