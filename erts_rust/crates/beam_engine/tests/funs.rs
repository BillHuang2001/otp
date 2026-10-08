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

//! Fun / closure integration test over a real `.beam`.
//!
//! Loads the committed oracle fixture `tests/fixtures/funs.beam` (produced by the
//! system `erlc`) and drives `Machine::run_export` over the functions that exercise
//! the `funs` instruction family:
//!
//! | export | opcodes | result |
//! |---|---|---|
//! | `test/0` | `make_fun3` (no free vars) + `call_fun` + `call_fun2` | `7` |
//! | `adder/1` | `make_fun3` (one free var) | the closure |
//! | `capture/0` | `make_fun3` (free var) + `call_fun2` | `15` |
//! | `dynamic/4` | `apply 2` | `{ok,5}` |
//! | `tail_dynamic/4` | `apply_last 2 0` | `5` |
//!
//! The engine keeps no closure registry, so the closures are applied through the
//! context's **apply hook**: the test installs a hook that reads the closure's
//! `fun_index`/`fun_free` and computes the lambda body (`X + 1`, `X + N`). The
//! closure bodies' `erlang:'+'/2` — and the `apply` target — are served by the BIF
//! hook.
//!
//! Boxed results (the closure `adder/1` builds, the `{ok,…}` tuple) are allocated
//! in the context's arena, so every test keeps its `MinimalContext` alive for as
//! long as it inspects them.

use beam_common::BeamError;
use beam_engine::terms;
use beam_engine::{Machine, MinimalContext};
use beam_loader::{load_file, Module};
use beam_term::{cmp, is_small, make_atom, make_small, signed_value, Atom, Eterm};

/// The committed oracle fixture (a real `.beam` from the system `erlc`).
const FUNS_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/funs.beam");

/// Resolve a name interned in the module's atom table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
}

fn funs_module() -> Module {
    load_file(FUNS_BEAM).expect("load funs.beam")
}

/// `erlang:'+'/2` over small integers — the closure bodies' and the `apply`
/// target's implementation.
fn arith_hook(
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    if module != "erlang" || function != "+" || arity != 2 || args.len() != 2 {
        return Err(BeamError::BadArg);
    }
    if !is_small(args[0]) || !is_small(args[1]) {
        return Err(BeamError::BadArith);
    }
    Ok(make_small(signed_value(args[0]) + signed_value(args[1])))
}

/// The stand-in closure registry.
///
/// The fixture's two lambdas are identified by their fun-table `index`: `index 0`
/// is `fun(X) -> X + 1 end` (`test/0`), `index 1` is `fun(X) -> X + N end`
/// (`adder/1`) with `N` as its sole free variable.
fn apply_hook(fun: Eterm, args: &[Eterm]) -> Result<Eterm, BeamError> {
    if !terms::is_fun(fun) {
        return Err(BeamError::BadArg);
    }
    if args.len() != terms::fun_arity(fun)? as usize {
        return Err(BeamError::BadArity);
    }
    match terms::fun_index(fun)? {
        // `fun(X) -> X + 1 end`
        0 => Ok(make_small(signed_value(args[0]) + 1)),
        // `fun(X) -> X + N end` — N is the closure's only free variable.
        1 => {
            let free = terms::fun_free(fun)?;
            Ok(make_small(signed_value(args[0]) + signed_value(free[0])))
        }
        _ => Err(BeamError::BadArg),
    }
}

/// A context with both hooks installed.
fn context() -> MinimalContext {
    let mut ctx = MinimalContext::with_apply_hook(Box::new(apply_hook));
    ctx.set_bif_hook(Box::new(arith_hook));
    ctx
}

/// Run `function/args.len()`; the caller owns `ctx` so boxed results stay valid.
fn run(ctx: &mut MinimalContext, function: &str, args: &[Eterm]) -> Result<Eterm, BeamError> {
    let module = funs_module();
    let mut machine = Machine::new();
    machine.run_export(
        &module,
        atom(&module, function),
        args.len() as u32,
        args,
        ctx,
    )
}

#[test]
fn module_exposes_the_expected_funs() {
    let module = funs_module();
    assert_eq!(module.atom_name(module.name), Some("funs"));
    assert_eq!(module.funs.len(), 2, "two lambdas in the FunT chunk");
    for name in ["test", "adder", "capture", "dynamic", "tail_dynamic"] {
        assert!(
            module.atom_table.lookup(name).is_some(),
            "{name} must be interned"
        );
    }
    // The closure bodies call `erlang:'+'/2`, so the apply target's atoms exist.
    assert!(module.atom_table.lookup("erlang").is_some());
    assert!(module.atom_table.lookup("+").is_some());
}

#[test]
fn test_builds_a_closure_and_calls_it_twice() {
    // test() -> apply_twice(fun(X) -> X + 1 end, 5)  ==  (5 + 1) + 1
    let mut ctx = context();
    assert_eq!(run(&mut ctx, "test", &[]).expect("test/0"), make_small(7));
}

#[test]
fn adder_builds_a_closure_capturing_one_free_variable() {
    let mut ctx = context();
    // The `make_fun3` result: arity 1 (2 total - 1 captured), one free variable.
    let fun = run(&mut ctx, "adder", &[make_small(10)]).expect("adder(10)");
    assert!(terms::is_fun(fun), "adder/1 returns a fun");
    assert_eq!(terms::fun_arity(fun).unwrap(), 1);
    assert_eq!(terms::fun_num_free(fun).unwrap(), 1);
    assert_eq!(terms::fun_free(fun).unwrap(), &[make_small(10)]);
    assert_eq!(terms::fun_uniq(fun).unwrap(), 15_347_931);
    assert_eq!(terms::fun_index(fun).unwrap(), 1);
}

#[test]
fn capture_applies_a_captured_variable_closure() {
    // capture() -> (funs:adder(10))(5)  ==  15
    let mut ctx = context();
    assert_eq!(
        run(&mut ctx, "capture", &[]).expect("capture/0"),
        make_small(15)
    );
}

#[test]
fn dynamic_apply_dispatches_the_named_function() {
    // dynamic(Mod, Fun, A, B) -> {ok, Mod:Fun(A, B)}
    let module = funs_module();
    let mut ctx = context();
    let result = run(
        &mut ctx,
        "dynamic",
        &[
            make_atom(atom(&module, "erlang")),
            make_atom(atom(&module, "+")),
            make_small(2),
            make_small(3),
        ],
    )
    .expect("dynamic(erlang, '+', 2, 3)");
    let expected = terms::make_tuple(&mut ctx, &[make_atom(atom(&module, "ok")), make_small(5)])
        .expect("build {ok,5}");
    assert!(cmp(result, expected, &module.atom_table).is_eq());
}

#[test]
fn tail_dynamic_apply_last_returns_the_result() {
    // tail_dynamic(Mod, Fun, A, B) -> Mod:Fun(A, B)
    let module = funs_module();
    let mut ctx = context();
    let mut machine = Machine::new();
    let result = machine
        .run_export(
            &module,
            atom(&module, "tail_dynamic"),
            4,
            &[
                make_atom(atom(&module, "erlang")),
                make_atom(atom(&module, "+")),
                make_small(2),
                make_small(3),
            ],
            &mut ctx,
        )
        .expect("tail_dynamic(erlang, '+', 2, 3)");
    assert_eq!(result, make_small(5));
    assert_eq!(machine.frame_depth(), 1, "the tail apply popped its frame");
}
