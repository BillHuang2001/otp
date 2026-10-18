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

//! **Exception/control-flow variants on real compiler output**: `Badmatch`,
//! `CaseEnd`, `IfEnd`, `Raise`, `TryCase`, `TryCaseEnd`, `TryEnd`, `CatchEnd`.
//!
//! The census counts an instruction **before** its body runs, so a raising
//! variant (`Badmatch`, `CaseEnd`, `IfEnd`, `Raise`, `TryCaseEnd`) is counted as
//! executed *and* its raise is the run's observable — compared against the
//! oracle's exception reason. A raise that lands in a live in-machine `try`/`catch`
//! comes back `Ok`, counted as executed but not as an error, which is how the
//! `TryCase`/`TryEnd`/`CatchEnd` rows read it.
//!
//! # The fixture
//!
//! Every row drives `tests/fixtures/vb_exc.beam` — a real `erlc +deterministic`
//! module (see `vb_exc.erl` for the per-export source shape and the regeneration
//! recipe in `tests/fixtures/README.md`). Each export exists to make the installed
//! compiler emit exactly one exception-family instruction:
//!
//! | export | instruction | why the compiler emits it |
//! |---|---|---|
//! | `badmatch/1` | `Badmatch` | `ok = X` |
//! | `case_end/1` | `CaseEnd` | a `case` with no matching clause |
//! | `if_end/1` | `IfEnd` | an `if` whose guards are all false |
//! | `rethrow/1`, `rethrow_after/1`, `rethrow_caught/1` | `Raise` | a `try … catch` whose clauses do **not** match (the compiler's fallback clause re-raises the pending exception), and a `try … after` |
//! | `recover/1` | `TryCase` | the exceptional handler entry of a `try … catch` |
//! | `try_clause/1` | `TryCaseEnd` | a `try … of` value matching no `of` clause |
//! | `normal_try/1` | `TryEnd` | the normal exit of a `try … of` |
//! | `old_catch/1` | `CatchEnd` | old-style `catch Expr` |
//!
//! `Raise` is the **`raise` opcode** (`{bif,raise,{f,0},[Trace,Value],{x,0}}` in
//! `beam_disasm`'s rendering, i.e. `Instr::Raise`), *not* `raw_raise` (the separate
//! `RawRaise` variant that a `catch C:R:S -> erlang:raise(C,R,S)` handler folds
//! into). The installed OTP 29.1.1 compiler emits it **only** from the
//! `lib/compiler/src/v3_core.erl` `raise(Info, Value)` primop — the
//! `try_exception/2` fallback clause (`v3_core.erl:1424-1441`) and
//! `after_block/2` (`v3_core.erl:1505-1518`) — which reach `cg_instr(resume, …)`
//! (`beam_ssa_codegen.erl:2471`) and thence `beam_asm`'s `raise` opcode.
//! That shape is ordinary OTP-library output, not a fixture-only curiosity:
//! **1816** static occurrences over the installed corpus (1366 `.beam` under
//! `code:lib_dir()`), 1811 of them in the 1321 `*/ebin/*.beam` modules alone —
//! heaviest `crypto.beam` 41 (one is the tail of `crypto:hash_init/1`),
//! `prim_file.beam` 36, `erlang.beam` 35 — and
//! `tests/census.rs::corpus_reachability_census` pins the same 1816.
//! Method, and the trap: `beam_disasm:file/1` renders the opcode as the **5-tuple**
//! `{bif,raise,{f,0},[R1,R2],{x,0}}` (`beam_disasm.erl:1096`), so a `{raise,_,_}`
//! pattern — or a filter on the atom table, where an opcode name never appears —
//! counts zero by construction.
//! `vb_exc` is used anyway because it isolates one raise per export, so each row's
//! observable is a single oracle-comparable exception.
//!
//! Comparison discipline: a returned value is rendered with
//! [`render`](crate::harness::render) and compared with
//! [`oracle_call`](crate::harness::oracle_call); a raise is folded into the
//! `{Class, Reason}` pair the oracle produces for the same call wrapped in
//! `try … catch C:R -> {C,R} end`, and compared with
//! [`oracle_w`](crate::harness::oracle_w) of that expression. Nothing is baked in.
//!
//! See the crate docs of `real_beam_variant_proofs.rs` for the proof shape and
//! `harness` for the shared implementation.

use beam_common::BeamError;
use beam_term::{make_small, Eterm};

use crate::harness::{host, oracle_call, oracle_w, render, run, skip_if_unavailable, Proof};
use crate::support::{term_atom, ModuleHost};

/// Registers the fixture and runs `vb_exc:function(Args)` on it, after the
/// caller's `skip_if_unavailable` guard.
fn vb_host() -> ModuleHost {
    host(&[("vb_exc", "vb_exc.beam")])
}

/// The engine's uncaught raise rendered as the oracle's `{Class, Reason}` pair.
///
/// The engine's uncaught raise is [`ExitReason::Structured`] with the live reason
/// *term* in [`Raise::live_reason`], so the reason is rendered with
/// [`render`](crate::harness::render) (the same `~w`-style notation as the
/// oracle); an atom reason spelled through [`ExitReason::Atom`] instead is
/// interned into the host's shared table and rendered the same way, so both
/// spellings produce one comparable string.
///
/// Panics when the run did not raise, so a row can never pass by observing a
/// different outcome than the one it pins.
///
/// [`ExitReason::Structured`]: beam_common::ExitReason::Structured
/// [`ExitReason::Atom`]: beam_common::ExitReason::Atom
/// [`Raise::live_reason`]: beam_common::Raise::live_reason
fn uncaught_pair(host: &mut ModuleHost, error: &BeamError) -> String {
    let BeamError::Raise(raise) = error else {
        panic!("expected an uncaught raise, got {error:?}");
    };
    let class = raise.class.as_str();
    let reason = match raise
        .live_reason()
        .and_then(|live| live.downcast_ref::<Eterm>())
        .copied()
    {
        Some(term) => render(host, term),
        None => {
            let name = raise
                .reason
                .as_atom()
                .unwrap_or_else(|| panic!("a raise with neither a live term nor an atom reason"));
            let term = host.intern(name);
            render(host, term)
        }
    };
    format!("{{{class},{reason}}}")
}

/// The `{Class, Reason}` pair the real `erl` raises for `vb_exc:function(args)`.
///
/// The oracle expression wraps the call in `try … catch C:R -> {C,R} end` — the
/// comparable shape for a raise (a bare call would die and print
/// `** exception error: …` on stderr instead of a `~w` term).
fn oracle_pair(function: &str, args: &str) -> String {
    oracle_w(&format!(
        "try vb_exc:{function}({args}) catch C:R -> {{C,R}} end"
    ))
}

/// Runs `vb_exc:function(args)` on the committed fixture and asserts the raise's
/// `{Class, Reason}` pair equals the oracle's for the same call.
///
/// `make_args` builds the argument terms from the **same** host the run uses: an
/// `Eterm` atom is an index into one host's shared atom table, so a term interned
/// by a different `ModuleHost` would name a different atom here (the atom index is
/// per-table, not global).
#[track_caller]
fn assert_raise_matches_oracle(
    variant: &str,
    function: &str,
    args: &str,
    make_args: impl FnOnce(&mut ModuleHost) -> Vec<Eterm>,
) -> Proof {
    let mut host = vb_host();
    let arg_list = make_args(&mut host);
    let proof = run(&mut host, "vb_exc", function, &arg_list);
    proof.require(&[variant]);
    let pair = uncaught_pair(&mut host, proof.error());
    assert_eq!(
        pair,
        oracle_pair(function, args),
        "the {{Class,Reason}} pair of `vb_exc:{function}({args})` differs from the oracle's\n{}",
        proof.report()
    );
    proof
}

// ---------------------------------------------------------------------------
// `badmatch` — `ok = X`
// ---------------------------------------------------------------------------
/// `Badmatch`: `ok = X` on a non-`ok` argument raises `error:{badmatch, a}`.
///
/// `vb_exc.beam:badmatch/1` is the compiler's `test is_eq_exact {f,_}` + a
/// `badmatch {x,0}` fail branch (verified with `beam_disasm:file/1`).
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[try
/// vb_exc:badmatch(a) catch C:R -> {C,R} end]),halt(0).'` → `{error,{badmatch,a}}`.
#[test]
fn badmatch_raises_the_oracles_reason_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    assert_raise_matches_oracle("Badmatch", "badmatch", "a", |host| {
        vec![term_atom(host, "a")]
    });
}

// ---------------------------------------------------------------------------
// `case_end` — a `case` with no matching clause
// ---------------------------------------------------------------------------
/// `CaseEnd`: an unmatched `case` raises `error:{case_clause, b}`.
///
/// `vb_exc.beam:case_end/1` lowers to `test is_eq_exact {f,_}` + a
/// `case_end {x,0}` fail branch.
///
/// Oracle: `… [try vb_exc:case_end(b) catch C:R -> {C,R} end] …` →
/// `{error,{case_clause,b}}`.
#[test]
fn case_end_raises_the_oracles_reason_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    assert_raise_matches_oracle("CaseEnd", "case_end", "b", |host| {
        vec![term_atom(host, "b")]
    });
}

// ---------------------------------------------------------------------------
// `if_end` — an `if` whose guards are all false
// ---------------------------------------------------------------------------
/// `IfEnd`: an `if` with no true guard raises `error:if_clause`.
///
/// `vb_exc.beam:if_end/1` lowers to `test is_lt {f,_} [0, X]` + a bare `if_end`.
///
/// Oracle: `… [try vb_exc:if_end(-1) catch C:R -> {C,R} end] …` →
/// `{error,if_clause}`.
#[test]
fn if_end_raises_the_oracles_reason_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    assert_raise_matches_oracle("IfEnd", "if_end", "-1", |_host| vec![make_small(-1)]);
}

// ---------------------------------------------------------------------------
// `raise` — the no-catch-clause-matched re-raise
// ---------------------------------------------------------------------------
/// `Raise` (uncaught): `try bump(X) catch throw:nope -> caught end` — the raised
/// `badarith` matches no catch clause, so the compiler's fallback clause
/// re-raises it with the `raise` opcode. The exception escapes with its ORIGINAL
/// class and reason: `error:badarith`.
///
/// `vb_exc.beam:rethrow/1` label 13 is
/// `{bif,raise,{f,0},[{x,2},{x,1}],{x,0}}` — `x(1)` the pending reason, `x(2)` its
/// trace — the exact shape `v3_core`'s `try_exception/2` fallback emits.
///
/// Oracle: `… [try vb_exc:rethrow(a) catch C:R -> {C,R} end] …` →
/// `{error,badarith}`.
#[test]
fn raise_re_raises_the_unmatched_exception_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    assert_raise_matches_oracle("Raise", "rethrow", "a", |host| vec![term_atom(host, "a")]);
}

/// `Raise` (landing in an outer `try`): the same re-raise, caught one frame out,
/// so the re-raise is observable as a **returned value** as well as a census
/// count — `reraised`.
///
/// Oracle: `… [vb_exc:rethrow_caught(a)] …` → `reraised`.
#[test]
fn raise_lands_in_an_outer_catch_frame_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = vb_host();
    let arg = term_atom(&mut host, "a");
    let proof = run(&mut host, "vb_exc", "rethrow_caught", &[arg]);
    proof.require(&["Raise", "TryCase"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_exc", "rethrow_caught", "a"),
        "{}",
        proof.report()
    );
}

/// `Raise` from a `try … after`: the after-path re-raises the pending exception
/// (`v3_core`'s `after_block/2`), which `beam_disasm` shows as a `try_case` whose
/// whole body is `{bif,raise,…}` — `error:badarith`.
///
/// Oracle: `… [try vb_exc:rethrow_after(a) catch C:R -> {C,R} end] …` →
/// `{error,badarith}`.
#[test]
fn raise_from_a_try_after_re_raises_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    assert_raise_matches_oracle("Raise", "rethrow_after", "a", |host| {
        vec![term_atom(host, "a")]
    });
}

// ---------------------------------------------------------------------------
// `try_case` — the exceptional handler entry
// ---------------------------------------------------------------------------
/// `TryCase`: the exceptional path of `try bump(X) catch error:badarith -> … end`
/// hands the class to `try_case`, the clause matches, and the run returns
/// `recovered` (an executed, non-error census count — the raise itself was
/// already folded into the handler).
///
/// Oracle: `… [vb_exc:recover(a)] …` → `recovered`.
#[test]
fn try_case_handles_the_exception_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = vb_host();
    let arg = term_atom(&mut host, "a");
    let proof = run(&mut host, "vb_exc", "recover", &[arg]);
    proof.require(&["TryCase"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_exc", "recover", "a"),
        "{}",
        proof.report()
    );
}

// ---------------------------------------------------------------------------
// `try_case_end` — a `try … of` value matching no `of` clause
// ---------------------------------------------------------------------------
/// `TryCaseEnd`: `try X of 1 -> one …` with `X = b` raises
/// `error:{try_clause, b}`. The body cannot raise, so the compiler drops the
/// `try` frame entirely (there is no `try_case` in the disassembly) and the
/// `try_clause` raise is uncaught — exactly as the oracle's.
///
/// Oracle: `… [try vb_exc:try_clause(b) catch C:R -> {C,R} end] …` →
/// `{error,{try_clause,b}}`.
#[test]
fn try_case_end_raises_the_oracles_try_clause_reason_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    assert_raise_matches_oracle("TryCaseEnd", "try_clause", "b", |host| {
        vec![term_atom(host, "b")]
    });
}

// ---------------------------------------------------------------------------
// `try_end` — the normal exit of a `try … of`
// ---------------------------------------------------------------------------
/// `TryEnd`: `try bump(X) of V -> {ok,V} catch error:_ -> failed end` on
/// `X = 1` takes the normal path, which the compiler closes with `try_end`.
///
/// Oracle: `… [vb_exc:normal_try(1)] …` → `{ok,2}`.
#[test]
fn try_end_runs_on_the_normal_exit_of_a_try_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = vb_host();
    let proof = run(&mut host, "vb_exc", "normal_try", &[make_small(1)]);
    proof.require(&["TryEnd"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_exc", "normal_try", "1"),
        "{}",
        proof.report()
    );
    // The exceptional path of the same function (`bump(a)` raises `badarith`)
    // returns `failed`, so the fixture's `try_end` is not the only way through.
    let arg = term_atom(&mut host, "a");
    let failed = run(&mut host, "vb_exc", "normal_try", &[arg]);
    failed.require(&["TryCase"]);
    assert_eq!(
        render(&host, failed.value()),
        oracle_call("vb_exc", "normal_try", "a"),
        "{}",
        failed.report()
    );
}

// ---------------------------------------------------------------------------
// `catch_end` — old-style `catch`
// ---------------------------------------------------------------------------
/// `CatchEnd`: `catch erlang:raise(throw, X, [])` is a class-`throw` exception
/// under an old-style `catch`, so `catch_end`'s three-way fold returns the thrown
/// value **bare** (`ball`) — no `{'EXIT',…}` wrapper.
///
/// `vb_exc.beam:old_catch/1` is `{'catch',N,{f,_}} … call_ext erlang:raise/3 …
/// catch_end {y,0}`, the shape `catchclasses.beam` documents for the class-`throw`
/// fold. `erlang:raise/3` (not `throw/1`) is used because it is the raise BIF the
/// compiler lowers the `throw(X)` sugar to, and the only class-carrying raise the
/// test host's BIF floor serves.
///
/// Oracle: `… [vb_exc:old_catch(ball)] …` → `ball`; `… [vb_exc:old_catch(7)] …` →
/// `7`.
#[test]
fn catch_end_folds_a_throw_class_exception_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = vb_host();
    let ball = term_atom(&mut host, "ball");
    let proof = run(&mut host, "vb_exc", "old_catch", &[ball]);
    proof.require(&["CatchEnd"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_exc", "old_catch", "ball"),
        "{}",
        proof.report()
    );
    // A non-atom thrown value folds the same way (`catch_end` is value-agnostic).
    let caught_int = run(&mut host, "vb_exc", "old_catch", &[make_small(7)]);
    caught_int.require(&["CatchEnd"]);
    assert_eq!(
        render(&host, caught_int.value()),
        oracle_call("vb_exc", "old_catch", "7"),
        "{}",
        caught_int.report()
    );
}

/// The five raising rows above all raise **uncaught**, so the engine's raise
/// (`Err(BeamError::Raise(..))`) is what they assert; this test pins that the
/// helper they share really is fed a raise and not, say, a silent `Ok`. It is
/// deliberately driven through one of the same fixture calls so a future change
/// that turns a raise into a fabricated value cannot pass the suite.
#[test]
fn the_uncaught_rows_really_observe_a_raise() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = vb_host();
    let arg = term_atom(&mut host, "a");
    let proof = run(&mut host, "vb_exc", "badmatch", &[arg]);
    assert!(
        matches!(proof.value, Err(BeamError::Raise(_))),
        "an uncaught `badmatch` must be a `BeamError::Raise`\n{}",
        proof.report()
    );
    assert!(
        proof.census.total_errors > 0,
        "the raise must be recorded in the error census too\n{}",
        proof.report()
    );
}
