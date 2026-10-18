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

//! **Fun/apply variants on real compiler output**: `MakeFun3`, `CallFun`,
//! `CallFun2`, `Apply`, `ApplyLast`.
//!
//! The closure rows must run a **compiled** lambda body (the engine enters it
//! through the host's `apply_fun`/`resolve_local_fun` handling of the real
//! `funs.beam`-style fun table), not a test hook that reimplements the body.
//!
//! # The fixture
//!
//! Every row drives the committed `erlc` fixture `tests/fixtures/vb_funs.beam`
//! (`vb_funs.erl`), whose exports are ordinary Erlang chosen so the OTP
//! compiler — never this suite — emits the instruction under proof. The
//! disassembly that each row relies on (`erlc +deterministic` + the system
//! `beam_disasm`, OTP 29.1.1 / `erts-17.1`):
//!
//! | export | emitted shape |
//! |---|---|
//! | `add_n/1` (`fun(X) -> X + N end`) | `test_heap` + `make_fun3 {vb_funs,'-add_n/1-fun-0-',2},0,…` (env = 1 slot) |
//! | `env2/2` (`fun(X) -> X + A + B end`) | `test_heap` + `make_fun3 {vb_funs,'-env2/2-fun-0-',3},1,…` (env = 2 slots) |
//! | `apply1/2` (`F(X)`) | `allocate` + `call_fun,1` |
//! | `twice/2` (`F(F(X))`) | `call_fun,1` then `call_fun2,{atom,safe},1,…` |
//! | `map_with/2` (`lists:map(F, L)`) | `call_ext_only {lists,map,2}` — the fun is applied by **real `lists.beam`**'s own `call_fun2` |
//! | `dyn/4` (`{ok, M:F(A, B)}`) | `allocate` + `apply,2` |
//! | `dyn_tail/4` (`M:F(A, B)`) | `allocate` + `apply_last,2,0` |
//!
//! # Why the closures are applied through the host, not re-implemented
//!
//! `ModuleHost::apply_fun` resolves a `.beam` fun's `uniq`+`index` to the
//! module's `FunT` descriptor and runs its entry label in a fresh `Machine`, so
//! the lambda body's own instructions (`func_info`, `gc_bif '+'`) really execute
//! — the engine's `call_fun`/`call_fun2` arm does the register marshalling
//! (arguments in `x(0..arity)`, captures after them, C's `call_fun` order) and
//! the fun-family seam supplies the callee. Nothing here hands the engine a
//! precomputed value.
//!
//! See the crate docs of `real_beam_variant_proofs.rs` for the proof shape and
//! `harness` for the shared implementation.

use beam_term::make_small;

use crate::harness::{apply, host, oracle_call, oracle_w, render, run, skip_if_unavailable};
use crate::support::{int_list, term_atom};

/// The committed compiler fixture every row in this file drives.
const FIXTURE: (&str, &str) = ("vb_funs", "vb_funs.beam");

// ---------------------------------------------------------------------------
// MakeFun3 — building a closure from the module's `FunT` entry
// ---------------------------------------------------------------------------

/// `make_fun3` with a **one-slot environment**, from real compiler output, and
/// the closure's compiled body executed for real.
///
/// `vb_funs:add_n(N)` is `fun(X) -> X + N end`, so the compiler emits
/// `test_heap` + `make_fun3 {vb_funs,'-add_n/1-fun-0-',2},0,<uniq>,{x,0},{list,[{x,0}]}`
/// — the closure arity (1) is the lambda's arity (2) minus its single captured
/// free variable.
///
/// Oracle — the closure itself is a fun (unrenderable), so the observable is
/// applying it, exactly as the oracle does:
/// `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[(vb_funs:add_n(10))(5)]),halt(0).'`
/// → `15`.
#[test]
fn make_fun3_builds_a_one_capture_closure_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[FIXTURE]);
    let built = run(&mut host, "vb_funs", "add_n", &[make_small(10)]);
    built.require_exactly("MakeFun3", 1);
    let fun = built.value();

    // The proof that the **compiled lambda body** ran (not a stand-in): the host
    // resolves the closure's `uniq`+`index` to the module's own `FunT` descriptor
    // and enters its label (the entry *after* `func_info`, which is why the
    // applied run's census shows the body's `gc_bif '+'` and its `return`, not a
    // `FuncInfo`), with the argument in `x(0)` and the capture in `x(1)`.
    let applied = apply(&mut host, fun, &[make_small(5)]);
    applied.require_exactly("GcBif", 1);
    assert_eq!(
        render(&host, applied.value()),
        oracle_w("(vb_funs:add_n(10))(5)")
    );
}

/// `make_fun3` with a **two-slot environment**: `vb_funs:env2(A, B)` is
/// `fun(X) -> X + A + B end`, so the emitted `make_fun3` carries a two-element
/// env list and the lambda's compiled body reads both captures (`x(1)`, `x(2)`
/// after the argument in `x(0)`).
///
/// The observable distinguishes the two rows: `1 + 2 + 3` can only come out
/// right if both captured values were stored in the closure block in the
/// compiler's order.
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[(vb_funs:env2(2,3))(1)]),halt(0).'`
/// → `6`.
#[test]
fn make_fun3_builds_a_two_capture_closure_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[FIXTURE]);
    let built = run(
        &mut host,
        "vb_funs",
        "env2",
        &[make_small(2), make_small(3)],
    );
    built.require_exactly("MakeFun3", 1);
    let fun = built.value();

    let applied = apply(&mut host, fun, &[make_small(1)]);
    assert_eq!(
        render(&host, applied.value()),
        oracle_w("(vb_funs:env2(2,3))(1)")
    );
}

// ---------------------------------------------------------------------------
// CallFun / CallFun2 — applying a compiled closure from compiled code
// ---------------------------------------------------------------------------

/// `call_fun` (the arity-form opcode) on a closure passed as a parameter.
///
/// `vb_funs:apply1(F, X)` is `F(X)` over an opaque `F`, so the compiler cannot
/// prove fun-ness and emits `allocate` + `call_fun,1` (the C `call_fun`, fun in
/// `x(arity)`) rather than the `call_fun2 {atom,safe}` form. The value therefore
/// comes out of the engine's own `Instr::CallFun` arm, and the applied body is
/// the real `'-add_n/1-fun-0-'/2`.
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_funs:apply1(vb_funs:add_n(2),5)]),halt(0).'`
/// → `7`.
#[test]
fn call_fun_applies_a_compiled_closure_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[FIXTURE]);
    // The closure is built first (that run is not the one under proof).
    let fun = run(&mut host, "vb_funs", "add_n", &[make_small(2)]).value();

    let proof = run(&mut host, "vb_funs", "apply1", &[fun, make_small(5)]);
    proof.require_exactly("CallFun", 1);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_funs", "apply1", "vb_funs:add_n(2), 5")
    );
}

/// `call_fun2` (`{atom,safe}` form) — twice the same closure, through compiled
/// code that receives it as a parameter.
///
/// `vb_funs:twice(F, X)` is `F(F(X))`: the compiler emits `call_fun,1` for the
/// inner call and `call_fun2,{atom,safe},1,…` for the outer one, so this run
/// exercises **both** closure opcodes (the `CallFun` row above isolates the
/// first on `apply1/2`, where no `call_fun2` exists).
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_funs:twice(vb_funs:add_n(2),5)]),halt(0).'`
/// → `9`.
#[test]
fn call_fun2_applies_a_compiled_closure_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[FIXTURE]);
    let fun = run(&mut host, "vb_funs", "add_n", &[make_small(2)]).value();

    let proof = run(&mut host, "vb_funs", "twice", &[fun, make_small(5)]);
    // One inner `F(F(X))` step is `call_fun`, the outer one `call_fun2`; the
    // row's claim is the `call_fun2` half, the `CallFun` row owns the other.
    proof.require_exactly("CallFun2", 1);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_funs", "twice", "vb_funs:add_n(2), 5")
    );
}

/// `call_fun2` **inside a real stdlib module**: the fixture hands its fun to
/// the installed `lists:map/2`, whose own compiled code applies it.
///
/// `vb_funs:map_with(F, L)` is `lists:map(F, L)`, i.e. a `call_ext_only` into
/// `lists.beam`; the engine then runs the *system* `lists`' `map/2`, which
/// executes its own `call_fun2,{atom,safe},1,…` per element (three elements →
/// three `call_fun2` executions). This is the cross-module form of the proof:
/// the closure's `uniq`/`index` resolve back to `vb_funs`' fun table while the
/// `call_fun2` itself comes from OTP's compiled `lists`.
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_funs:map_with(vb_funs:add_n(3),[1,2,3])]),halt(0).'`
/// → `[4,5,6]`.
#[test]
fn call_fun2_applies_a_closure_inside_the_real_lists_module() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[FIXTURE]);
    let fun = run(&mut host, "vb_funs", "add_n", &[make_small(3)]).value();
    let list = int_list(&mut host, &[1, 2, 3]);

    let proof = run(&mut host, "vb_funs", "map_with", &[fun, list]);
    // One `call_fun2` per element, executed by the real `lists:map/2` code.
    proof.require_exactly("CallFun2", 3);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_funs", "map_with", "vb_funs:add_n(3), [1,2,3]")
    );
}

// ---------------------------------------------------------------------------
// Apply / ApplyLast — the dynamic `M:F(A, …)` forms
// ---------------------------------------------------------------------------

/// `apply` — a dynamic `M:F(A, B)` whose `M`/`F` are **not** static atoms.
///
/// `vb_funs:dyn(M, F, A, B)` is `{ok, M:F(A, B)}`, so the result is captured and
/// the compiler emits `allocate` + `apply,2` (`fixed_apply`: module in
/// `x(arity)`, function in `x(arity+1)`, arguments below them). The row names a
/// **real compiled module function** at run time (`vb_funs:sum2/2`), so the
/// dynamic dispatch resolves through the host's cross-module call path and the
/// callee's own `gc_bif '+'` produces the value.
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_funs:dyn(vb_funs,sum2,2,3)]),halt(0).'`
/// → `{ok,5}`.
#[test]
fn apply_calls_a_compiled_module_function_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[FIXTURE]);
    let module = term_atom(&mut host, "vb_funs");
    let function = term_atom(&mut host, "sum2");

    let proof = run(
        &mut host,
        "vb_funs",
        "dyn",
        &[module, function, make_small(2), make_small(3)],
    );
    proof.require_exactly("Apply", 1);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_funs", "dyn", "vb_funs, sum2, 2, 3")
    );
}

/// `apply_last` — the same dynamic call in **tail position**.
///
/// `vb_funs:dyn_tail(M, F, A, B)` is `M:F(A, B)` as the function's last
/// expression, so the compiler emits `allocate` + `apply_last,2,0` (C's
/// `i_apply_last`: the frame is popped *before* the target runs). This row names
/// a **BIF** target (`erlang:'+'/2`), which is the C's `fixed_apply` → BIF case:
/// the callee runs with the caller's frame already gone and its value returns
/// through the machine's continuation.
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_funs:dyn_tail(erlang,'"'"'+'"'"',2,3)]),halt(0).'`
/// → `5`.
#[test]
fn apply_last_calls_a_bif_in_tail_position_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[FIXTURE]);
    let module = term_atom(&mut host, "erlang");
    let function = term_atom(&mut host, "+");

    let proof = run(
        &mut host,
        "vb_funs",
        "dyn_tail",
        &[module, function, make_small(2), make_small(3)],
    );
    proof.require_exactly("ApplyLast", 1);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_funs", "dyn_tail", "erlang, '+', 2, 3")
    );
}
