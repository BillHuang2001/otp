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

//! **Call/frame-control variants on real compiler output**: `Call`, `CallLast`,
//! `CallOnly`, `CallExt`, `CallExtOnly`, `CallExtLast`, `DeallocateReturn`,
//! `InitYregs`, `FuncInfo`, `Bif0`, `Move`, `Trim`.
//!
//! These are the highest-weight instruction variants in the whole OTP corpus
//! (`FuncInfo` 103 749 static occurrences, `CallExt` 75 795, `Call` 82 110,
//! `DeallocateReturn` 54 479, `CallExtOnly` 36 692, `InitYregs` 34 782,
//! `CallLast` 25 778, `CallExtLast` 20 659, `Bif0` 2 687), so every one of them
//! needs a real-`.beam` proof rather than a probe. Note that `FuncInfo` only
//! *executes* when control enters a function at the entry label — an export entry
//! and an ordinary `call` target the label **after** it — so the row for it must
//! drive a shape that really enters there (see the row's doc comment).
//!
//! # One fixture, one export per variant
//!
//! Every row below drives `tests/fixtures/vb_calls.beam`, the committed
//! `erlc +deterministic` output of `vb_calls.erl` (OTP 29 / `erts-17.1`), whose
//! exports each force **one** of these instructions (the fixture's own header
//! records the compiled shape of every export). No row hand-builds a
//! `beam_loader::Module`: the instruction under test is the compiler's.
//!
//! | row | export (args) | variant(s) required |
//! |---|---|---|
//! | `call_…` | `call_nontail(3)` | `Call` |
//! | `deallocate_return_…` | `call_nontail(3)` | `DeallocateReturn` |
//! | `call_only_…` | `call_only(4)` | `CallOnly` |
//! | `call_last_…` | `call_last(1)` | `CallLast` |
//! | `call_ext_…` | `ext_nontail(2)` | `CallExt` |
//! | `call_ext_only_…` | `ext_only([1,2,3])` | `CallExtOnly` |
//! | `call_ext_last_…` | `ext_last([5])` | `CallExtLast` |
//! | `init_yregs_…` | `yregs(7)` | `InitYregs` |
//! | `func_info_…` | `clause_catch(a)` | `FuncInfo` |
//! | `bif0_…` | `node_bif()` | `Bif0` |
//! | `trim_and_move_…` | `trimming(2)` | `Trim`, `Move` |
//!
//! Each row's doc comment records the **measured** `exec_census` count of its
//! variant (OTP 29.1.1 / `erts-17.1`, the reference oracle). The census covers the
//! whole run, so a row that calls an imported stdlib function also counts *that*
//! module's own instructions — e.g. `ext_only/1`'s `CallExtOnly = 2` is the
//! fixture's tail call plus `lists:reverse/1`'s own tail call into the
//! `lists:reverse/2` import. The assertions are `> 0` (robust across oracle
//! versions); only the two semantically exact counts (`FuncInfo` once per entered
//! clause, `Deallocate` never in the corpus) are pinned exactly.
//!
//! See the crate docs of `real_beam_variant_proofs.rs` for the proof shape and
//! `harness` for the shared implementation.

use beam_term::make_small;

use crate::harness::{host, oracle_call, render, run, skip_if_unavailable};
use crate::support::{int_list, term_atom};

/// `Call` — a **non-tail local call**, from real compiler output.
///
/// `vb_calls:call_nontail/1` is `Y = sub(X), Y * 2.`: the result of `sub/1` is
/// consumed by a `gc_bif` after the call, so the call cannot be a tail call and
/// the compiler emits `call 1 {vb_calls,sub,1}` after `allocate 0,1`.
///
/// Observed census: `Call = 1` (the `sub/1` call), with `GcBif = 2` (that call's
/// `'+'` inside `sub/1`, then the `'*'` in `call_nontail/1`) and `Return = 1`
/// (`sub/1`'s own return — `call_nontail/1` returns through
/// `DeallocateReturn`).
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_calls:call_nontail(3)]),halt(0).'`
/// → `8`.
#[test]
fn call_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("vb_calls", "vb_calls.beam")]);
    let proof = run(&mut host, "vb_calls", "call_nontail", &[make_small(3)]);
    proof.require(&["Call"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_calls", "call_nontail", "3")
    );
}

/// `DeallocateReturn` — the loader's `deallocate N; return` fuse, from real
/// compiler output.
///
/// The `.beam` carries the pair `deallocate 0` / `return` at the end of
/// `vb_calls:call_nontail/1` (after the call and the `gc_bif`), and
/// `beam_loader::lower::fuse` fuses it into the single `DeallocateReturn` the
/// engine executes. This is the same run as the `Call` row — the two variants are
/// adjacent in one compiled function — so the row exists to pin the fuse, not to
/// find a second shape.
///
/// Observed census: `DeallocateReturn = 1` and `Deallocate = 0` — the latter
/// asserted exactly, because the fuse is why `Deallocate` is a `ZERO_CORPUS`
/// variant in `tests/census.rs` (no corpus `deallocate` survives lowering).
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_calls:call_nontail(3)]),halt(0).'`
/// → `8`.
#[test]
fn deallocate_return_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("vb_calls", "vb_calls.beam")]);
    let proof = run(&mut host, "vb_calls", "call_nontail", &[make_small(3)]);
    proof.require(&["DeallocateReturn"]);
    proof.require_exactly("Deallocate", 0);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_calls", "call_nontail", "3")
    );
}

/// `CallOnly` — a **tail local call from a frameless function**.
///
/// `vb_calls:call_only/1` is `sub(X).` with no Y registers and no catch frame, so
/// the compiler's whole body is `call_only 1 {vb_calls,sub,1}`.
///
/// Observed census: `CallOnly = 1`.
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_calls:call_only(4)]),halt(0).'`
/// → `5`.
#[test]
fn call_only_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("vb_calls", "vb_calls.beam")]);
    let proof = run(&mut host, "vb_calls", "call_only", &[make_small(4)]);
    proof.require(&["CallOnly"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_calls", "call_only", "4")
    );
}

/// `CallLast` — a **tail local call that deallocates a frame**.
///
/// `vb_calls:call_last/1` keeps `Y` in `y(0)` across the middle `sub/1` call, so
/// its last call is `call_last 1 {vb_calls,sub,1}, 1` (the `1` is the frame the
/// tail call pops after the callee returns).
///
/// Observed census: `CallLast = 1`, plus `Call = 2` for the two earlier non-tail
/// calls of the same function, `InitYregs = 1` for the frame's slot and
/// `Move = 2` (the `y(0)` spill of the call result and its restore into `x(0)`).
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_calls:call_last(1)]),halt(0).'`
/// → `3`.
#[test]
fn call_last_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("vb_calls", "vb_calls.beam")]);
    let proof = run(&mut host, "vb_calls", "call_last", &[make_small(1)]);
    proof.require(&["CallLast"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_calls", "call_last", "1")
    );
}

/// `CallExt` — a **non-tail call over an import**.
///
/// `vb_calls:ext_nontail/1` is `Y = lists:reverse([X]), {Y, X}.`: the imported
/// call's result is used after the call, so the compiler emits
/// `call_ext 1 {extfunc,lists,reverse,1}` (the host resolves `lists` from the
/// system stdlib `ebin` the harness put on the code path — the same module the
/// oracle runs).
///
/// Observed census: `CallExt = 1`. The run's other instructions are the fixture's
/// own `Move = 1` (`X` is saved before `put_list` overwrites `x(0)`), `PutList = 1`
/// and `PutTuple2 = 1`, plus `lists:reverse/1`'s body for a one-element list
/// (`GetList = 1`, `Test(IsNonemptyList) = 2`, `Test(IsNil) = 1`, `Return = 1`).
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_calls:ext_nontail(2)]),halt(0).'`
/// → `{[2],2}`.
#[test]
fn call_ext_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("vb_calls", "vb_calls.beam")]);
    let proof = run(&mut host, "vb_calls", "ext_nontail", &[make_small(2)]);
    proof.require(&["CallExt"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_calls", "ext_nontail", "2")
    );
}

/// `CallExtOnly` — a **tail call over an import from a frameless function**.
///
/// `vb_calls:ext_only/1` is `lists:reverse(X).` with no Y registers, so the whole
/// body is `call_ext_only 1 {extfunc,lists,reverse,1}`.
///
/// Observed census: `CallExtOnly = 2` — the fixture's own tail call, plus
/// `lists:reverse/1`'s tail call into the `lists:reverse/2` **import** for a
/// three-element list (both are `call_ext_only` because that helper is an import,
/// not a same-module function).
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_calls:ext_only([1,2,3])]),halt(0).'`
/// → `[3,2,1]`.
#[test]
fn call_ext_only_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("vb_calls", "vb_calls.beam")]);
    let list = int_list(&mut host, &[1, 2, 3]);
    let proof = run(&mut host, "vb_calls", "ext_only", &[list]);
    proof.require(&["CallExtOnly"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_calls", "ext_only", "[1,2,3]")
    );
}

/// `CallExtLast` — a **tail call over an import that deallocates a frame**.
///
/// `vb_calls:ext_last/1` keeps `Y` in `y(0)` across the middle import call, so its
/// last call is `call_ext_last 1 {extfunc,lists,reverse,1}, 1`.
///
/// Observed census: `CallExtLast = 1`, with `CallExt = 2` for the two earlier
/// imports, `InitYregs = 1` for the frame's slot and `Move = 2` (the spill and the
/// restore of the `y(0)` value).
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_calls:ext_last([5])]),halt(0).'`
/// → `[[5]]`.
#[test]
fn call_ext_last_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("vb_calls", "vb_calls.beam")]);
    let list = int_list(&mut host, &[5]);
    let proof = run(&mut host, "vb_calls", "ext_last", &[list]);
    proof.require(&["CallExtLast"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_calls", "ext_last", "[5]")
    );
}

/// `InitYregs` — **Y-register initialization**, from real compiler output.
///
/// `vb_calls:yregs/1` holds `Y` for the `try`'s handler, so the compiler emits
/// `allocate_heap 2,2,1` followed by `init_yregs [y0,y1]` before the `try` body:
/// the handler may run while the slots still hold stale words, and a GC must see
/// initialized terms.
///
/// Observed census: `InitYregs = 1` (the entry one; the `catch` arm keeps the
/// frame, so `beam_trim` leaves the instruction in place rather than shrinking).
/// The run's other instructions are the fixture's own (`CallExt = 2`,
/// `PutList = 2`, `PutTuple2 = 1`, `Try`/`TryEnd` = 1 each).
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_calls:yregs(7)]),halt(0).'`
/// → `{ok,[[7]]}`.
#[test]
fn init_yregs_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("vb_calls", "vb_calls.beam")]);
    let proof = run(&mut host, "vb_calls", "yregs", &[make_small(7)]);
    proof.require(&["InitYregs"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_calls", "yregs", "7")
    );
}

/// `FuncInfo` — the **failed-head-clause raise**, the only route real compiler
/// output takes into this instruction.
///
/// A function's callable entry label is the label *after* its `func_info`, so an
/// ordinary call never executes it. The exception is a `match_fail
/// function_clause` in a frame-less function: `beam_ssa_codegen:cg_match_fail/3`
/// jumps to the **`func_info` label** (its `fc_label`) instead of rewriting the
/// failure as `erlang:error(function_clause, Args)`, and C's `i_func_info_IaaI`
/// then raises `function_clause`. `vb_calls:clause/1` (`clause(X) when
/// is_integer(X) -> X * 2.`) has exactly that shape — its body is
/// `test is_integer, {f,<func-info-label>}, [x0]` — and `vb_calls:clause_catch/1`
/// catches the raise in a live `try`, so the run returns normally and the reason
/// is observable as a term.
///
/// Observed census: `FuncInfo = 1` — the one entering instruction of the failing
/// clause; it is also counted as an *error* of the funnel and then folded into the
/// live catch frame, so the run still returns `Ok`. The rest of the run is the
/// caller's own frame and handler (`Allocate = 1`, `Call = 1`, `Try = 1`,
/// `TryCase = 1`, `Test(IsInteger) = 1` from `clause/1`'s guard,
/// `Test(IsEqExact) = 1` for the handler's `error` match).
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_calls:clause_catch(a)]),halt(0).'`
/// → `{error,function_clause}`.
#[test]
fn func_info_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("vb_calls", "vb_calls.beam")]);
    let atom = term_atom(&mut host, "a");
    let proof = run(&mut host, "vb_calls", "clause_catch", &[atom]);
    proof.require(&["FuncInfo"]);
    proof.require_exactly("FuncInfo", 1);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_calls", "clause_catch", "a")
    );
}

/// `Bif0` — a **zero-argument BIF with no fail label**.
///
/// `vb_calls:node_bif/0` is `node().`, which `beam_asm:make_op/2` encodes as
/// `bif0 {extfunc,erlang,node,0} {x,0}` ("BIFs without arguments cannot fail").
/// The harness's BIF floor serves `erlang:node/0` as `nonode@nohost`, which is
/// what a non-distributed `erl -noshell` answers too.
///
/// Observed census: `Bif0 = 1`.
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_calls:node_bif()]),halt(0).'`
/// → `nonode@nohost`.
#[test]
fn bif0_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("vb_calls", "vb_calls.beam")]);
    let proof = run(&mut host, "vb_calls", "node_bif", &[]);
    proof.require(&["Bif0"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_calls", "node_bif", "")
    );
}

/// `Trim` **and** `Move` — a frame shrink plus a register move, in one compiled
/// function (one run covers both variants, so one row requires both).
///
/// `vb_calls:trimming/1` keeps `N` in `y(0)` across the `hold/1` call; in the `2`
/// arm `N` is dead, so `beam_trim` shrinks the frame to 0 and the compiled arm is
/// `trim 1,0` · `move {literal,[5]},{x,0}` · `call_ext lists:reverse/1`. The
/// `move` is the same arm's literal load; the function's entry also executes
/// `move {x,0},{y,0}` (the `y(0)` spill of `N`).
///
/// Observed census: `Trim = 1` and `Move = 3` — the entry's `move {x,0},{y,0}`
/// (the `y(0)` spill of `N`), the trimmed arm's `move {literal,[5]},{x,0}`, and
/// `hold/1`'s own `move {atom,ok},{x,0}`. The same arm also executes `Call = 1`
/// (the `hold/1` call), `SelectVal = 1` and `CallExt = 1` (`lists:reverse/1`).
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_calls:trimming(2)]),halt(0).'`
/// → `{two,[5]}`.
#[test]
fn trim_and_move_execute_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("vb_calls", "vb_calls.beam")]);
    let proof = run(&mut host, "vb_calls", "trimming", &[make_small(2)]);
    proof.require(&["Trim", "Move"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_calls", "trimming", "2")
    );
}
