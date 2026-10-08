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

//! The `function_clause` raise end to end: a function whose head clause never matches must
//! terminate with the oracle's `error:function_clause` — not a capability gap, not a hang.
//!
//! A function's **callable** entry label is the label *after* its `func_info` instruction
//! (`../beam_loader/CONTEXT.md`), so a normal call never executes `func_info`; the only way
//! to reach it is a JUMP from a failed head-pattern/guard `fail` label. C
//! (`i_func_info_IaaI`, `erts/emulator/beam/emu/beam_emu.c:578-583`) then unconditionally
//! raises class `error` with the **bare atom** `function_clause` (the arguments ride in the
//! stacktrace, not the reason).
//!
//! That reason is not in the compiled module's own atom table (`function_clause_fixture.beam`
//! holds only `function_clause_fixture`, `f`, `a`, `ok`, `module_info`, `erlang`,
//! `get_module_info` — verified with `beam_lib:chunks/2`), so the runtime's boot seed
//! (`crate::boot::CORE_ATOMS`) is what makes the name resolvable at all.
//!
//! Oracle (OTP 29 / `erts-17.1`, verified with `erl -noshell`):
//!
//! ```erl
//! function_clause_fixture:f(a).                              %% ok
//! function_clause_fixture:f(b).                              %% error:function_clause
//! try function_clause_fixture:f(b) catch C:R -> {C,R} end.   %% {error,function_clause}
//! ```
//!
//! The committed fixture is loaded by relative path (a single source of truth — it is never
//! copied), so this file needs no system toolchain and cannot hang: the raise *is* the fix.

use beam_common::ExceptionClass;
use beam_engine::terms;
use beam_rt::{error, Runtime};
use beam_term::{is_atom, Eterm};

/// This crate's failed-head-clause fixture (`tests/fixtures/function_clause_fixture.beam`).
const FUNCTION_CLAUSE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/function_clause_fixture.beam"
);

/// A fresh runtime with the fixture loaded through the code server (so the module resolves
/// from its cache exactly as a `-pa` directory would resolve it).
fn runtime() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(FUNCTION_CLAUSE_BEAM)
        .expect("the committed fixture loads");
    rt
}

/// The live reason term of `raise`, which is what a live catch frame lands in `x(1)`.
fn live_reason(raise: &beam_common::Raise) -> Option<Eterm> {
    raise
        .live_reason()
        .and_then(|live| live.downcast_ref::<Eterm>())
        .copied()
}

#[test]
fn a_failed_function_head_clause_raises_the_bare_function_clause_atom() {
    let mut rt = runtime();

    // The happy path never touches `func_info` and must not raise.
    let a = rt.intern("a");
    let matched = rt
        .run_function(FUNCTION_CLAUSE_BEAM, "f", 1, &[a])
        .expect("f(a) matches its only clause");
    assert_eq!(rt.atom_name_of(matched), Some("ok"));
    // `f(b)` matches no clause: the head test jumps to the function's `func_info` label.
    // Terminating at all is part of the evidence — treating `func_info` as a no-op would
    // fall through to the entry label and re-run the clause test forever.
    let b = rt.intern("b");
    let err = rt
        .run_function(FUNCTION_CLAUSE_BEAM, "f", 1, &[b])
        .expect_err("f(b) matches no clause and must raise");

    assert!(err.is_raise(), "{err:?} must be a raise");
    assert_eq!(err.as_atom(), Some("function_clause"), "{err:?}");
    assert_eq!(err.to_string(), "error function_clause");
    // A genuine Erlang exception, never a Milestone-1 capability gap (which would be the
    // exit-3 `beam_rt: unsupported: …` signal).
    assert_eq!(error::unsupported_reason(&err), None, "{err:?}");

    let raise = err.into_raise().expect("a raise");
    assert_eq!(raise.class, ExceptionClass::Error);
    assert_eq!(raise.reason.as_atom(), Some("function_clause"));
}

#[test]
fn the_raise_reason_is_the_bare_atom_not_a_function_clause_tuple() {
    let mut rt = runtime();
    let b = rt.intern("b");
    let err = rt
        .run_function(FUNCTION_CLAUSE_BEAM, "f", 1, &[b])
        .expect_err("f(b) matches no clause and must raise");

    // The **live** reason term is the atom itself — C's `EXC_FUNCTION_CLAUSE` reason, with
    // the call arguments in the stacktrace. A `{function_clause, Args}` tuple would be a
    // different (wrong) reason.
    let raise = err.into_raise().expect("a raise");
    let reason = live_reason(&raise).expect("the raise carries its live reason term");
    assert!(is_atom(reason), "the reason term is not an atom");
    assert_eq!(rt.atom_name_of(reason), Some("function_clause"));}

// -- The `proplists:get_value(a, undefined)` case of the original report --------------------
//
// `proplists:get_value/3` has no clause for a non-list argument, so the call raises
// `function_clause` from *inside* a real system stdlib module — the shape that made the
// missing seed atom observable in the first place. It needs the system stdlib, so both tests
// **skip loudly** (never silently pass) when it is unavailable; the committed-fixture tests
// above are the fast, dependency-free guards.

/// Whether the system stdlib is resolvable; prints the reason (and returns `false`) when it is
/// not.
fn system_stdlib_available() -> bool {
    let mut server = beam_rt::CodeServer::new();
    match server.add_system_paths() {
        Ok(()) if server.find_file("proplists").is_some() => true,
        Ok(()) => {
            println!("SKIP function_clause: the system code path holds no proplists.beam");
            false
        }
        Err(error) => {
            println!("SKIP function_clause: no system stdlib ({error})");
            false
        }
    }
}

#[test]
fn proplists_get_value_on_a_non_list_raises_function_clause() {
    if !system_stdlib_available() {
        return;
    }
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    let key = rt.intern("a");
    let undefined = rt.intern("undefined");

    let err = rt
        .run_named("proplists", "get_value", 2, &[key, undefined])
        .expect_err("proplists:get_value(a, undefined) must raise");
    assert!(err.is_raise(), "{err:?} must be a raise");
    assert_eq!(err.as_atom(), Some("function_clause"), "{err:?}");
    assert_eq!(err.to_string(), "error function_clause");
    assert_eq!(error::unsupported_reason(&err), None, "{err:?}");
}

#[test]
fn proplists_get_value_on_a_non_list_is_catchable_as_function_clause() {
    if !system_stdlib_available() {
        return;
    }
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    // `try proplists:get_value(a, undefined) catch C:R -> {C,R} end` ⇒ `{error,function_clause}`
    // (the oracle's exact value).
    let caught = rt
        .eval("try proplists:get_value(a, undefined) catch C:R -> {C,R} end")
        .expect("the raise is catchable");

    assert_eq!(terms::tuple_arity(caught).expect("a tuple"), 2);
    let class = terms::tuple_element(caught, 0).expect("the class");
    assert_eq!(rt.atom_name_of(class), Some("error"));
    let reason = terms::tuple_element(caught, 1).expect("the reason");
    assert!(is_atom(reason), "the reason term is not an atom");
    assert_eq!(rt.atom_name_of(reason), Some("function_clause"));
}
