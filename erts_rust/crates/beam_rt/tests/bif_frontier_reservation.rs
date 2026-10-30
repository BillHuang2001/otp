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
//! The pooled BIF-heap **frontier reservation** — the invariant is that a *legal* call never
//! raises `error:system_limit` because the region the pool happened to hand over was nearly
//! spent ([`crate::context::calls::frontier`]).
//!
//! # The defect this net exists for
//!
//! Every registered BIF call takes a pooled region from `Runtime::take_bif_heap` and builds its
//! result by bumping inside it. The pool is **shared**, so the region handed back may be almost
//! used up — and one call may need far more than whatever fraction of the capacity a fixed
//! "nearly exhausted" test foresees. The rule before the fix was
//! `available() < capacity() / BIF_HEAP_GROWTH_DIVISOR` (16): a region with 1/16 of its capacity
//! free was accepted, and a call whose **single** transient result wanted megabytes then died of
//! `error:system_limit` — a raise the program never asked for, on a call the system Erlang oracle
//! completes. Nothing about the call is illegal: `length(lists:duplicate(1 bsl 21, gurka))` needs
//! 2^22 words (32 MiB) of transient list on the BIF heap, and the outermost region is 2^24 words.
//!
//! The current rule is a **need-aware reservation**
//! (`need = max(learned_at_this_depth, min(1 << 23 words, capacity / 4))`): the taker accepts the
//! region only when `available() >= need`, the growth arm raises the capacity to
//! `max(2 × capacity, need)` under a configured `+hmax`, and the call's tail **learns** the
//! demand it actually handed out (`used() - mark`) per depth. There is no mid-BIF retry: a
//! half-built result cannot be replayed in most of the registry (the `SendHook`/`PortHook`,
//! `ets`, `DictHook`/`PersistentTerm`/timer, `ProcessHook`, `NifLoader`/`OutputSink` and
//! `RuntimeHooks` families all mutate before they can fail), so **the reservation is what must be
//! right**.
//!
//! # The measured pre-fix failure signature
//!
//! Reproduced by hand on this host (P-core pinned, `/proc/loadavg` 2.97 before / 2.67 after), the
//! pre-fix rule rebuilt from source and compared with this tree's:
//!
//! | shape | pre-fix | now | oracle |
//! |---|---|---|---|
//! | the 50-round `1 bsl 21` loop in a spawned process, compiled `run` target | failed at round **4** in **5/5** outer runs | **0** failures, 5/5 | never fails |
//! | the same loop on the `-eval` route | failed at round **4** in 5/5 | 0 failures, 5/5 | never fails |
//! | the `2^15 … 2^20` band, 50 rounds each | never failed | never failed | never fails |
//!
//! so the `run` case below is the shape that *discriminates*, and the band is the neighbouring
//! sizes that must stay clean in both directions. Every default-suite case is the same shape with
//! a smaller round budget (see [`ROUNDS`]).
//!
//! # Why this is not a load-sensitive flake
//!
//! The failure mode is a **deterministic allocation demand** against a reservation derived only
//! from the expression, not a latency or a schedule: no case here asserts a wall time, a
//! reduction count, a thread count or a ratio. The assertion is the invariant itself — the loop
//! counts the rounds that raised and requires **0**, which held for 250 consecutive rounds on this
//! host before and after the change, at a round budget four times the observed pre-fix failure
//! point. The only nondeterminism the shape has is the very `error:system_limit` it forbids.
//!
//! # The nested first-call gap, and the one retried constructor
//!
//! A **nested** run whose single call needs more than its whole seeded region cannot be taught by
//! the learned figure — the learned demand saturates at the region's capacity, so the reservation
//! can never foresee a call the region cannot hold (e.g. `lists:map(fun(_) ->
//! length(lists:duplicate(1 bsl E, gurka)) end, lists:seq(1,8))` against the 2^17-word nested
//! seed). `lists:duplicate/2` is the one exception: the registered-call tail now **retries** that
//! single provably-pure constructor on a wider frontier when a **nested** call exhausts its region
//! (`is_retryable_pure_constructor` in `src/context/calls.rs`), so the D1 shape —
//! `lists:foreach(fun(_) -> length(lists:duplicate(N, x)) end, [1])` — returns `ok` for every `N`,
//! where it used to raise a catchable `error:system_limit` for `N >= 65537`. The cases here remain
//! all **outermost** calls, which is the shape the original defect report named; the nested retry
//! is pinned by `a_large_list_built_inside_a_lists_foreach_fun_returns_ok`.
//!
//! # Budget
//!
//! Dev profile (`cargo test`'s), one child per case: [`ROUNDS`] rounds 0.32 s in the `-eval`
//! process and 0.53 s spawned, the band 0.34 s, the maps case 0.54 s, the `+hmax` pair 0.2 s —
//! ~1 s of wall time in parallel. The full 50 rounds on the compiled `run` route are
//! [`the_fifty_round_loop_on_a_compiled_run_target_never_fails`], `#[ignore]`d only because it
//! needs `erlc` (1.2 s release when it runs).
#[allow(dead_code)]
mod support;

use std::process::Command;
/// Rounds per default-suite case. The pre-fix rule failed at round **4** in 5/5 outer runs, so
/// 12 keeps a 3× margin over the observed failure point while staying ~0.3–0.5 s per case.
const ROUNDS: u32 = 12;
/// Rounds per size of the `2^15 … 2^20` band (the sizes just below the discriminating one).
const BAND_ROUNDS: u32 = 6;
/// What one failed round means: the loop returns the 1-based numbers that raised, so a clean run
/// prints `0` failures. This is the invariant, not a magic count.
const NO_FAILURES: &str = "0";
/// The loop body shared by the `-eval` and spawned cases: one BIF call that builds 2^21 list
/// cells (2^22 words = 32 MiB) as a transient result, [`ROUNDS`] times, counting the rounds that
/// raised. `F0` accumulates the failures so the expression itself prints them.
fn loop_body(variable: &str) -> String {
    format!(
        "F = fun G(0, F0) -> F0; \
              G(N, F0) -> try length(lists:duplicate(1 bsl 21, gurka)) of \
                              2097152 -> G(N - 1, F0); \
                              _ -> G(N - 1, F0 + 1) \
                          catch _:_ -> G(N - 1, F0 + 1) end \
         end, {variable}"
    )
}
/// The loop run in the `-eval` route's own process (the outermost BIF depth).
fn eval_loop_expr(rounds: u32) -> String {
    loop_body(&format!("io:format(\"~p~n\", [F({rounds}, 0)])."))
}
/// The same loop in a **spawned** process, which is where the defect report observed it: the
/// child takes a pooled region of its own, and the parent waits for the count.
fn spawned_loop_expr(rounds: u32) -> String {
    loop_body(&format!(
        "Self = self(), \
         spawn(fun() -> Self ! {{done, F({rounds}, 0)}} end), \
         receive {{done, R}} -> io:format(\"~p~n\", [R]) end."
    ))
}
/// `Band` sizes 2^15 … 2^20 (`1 bsl (14 + N)` for `N = BAND_ROUNDS … 1`), each for
/// [`BAND_ROUNDS`] rounds — the neighbourhood that must stay clean, ascending so a case that
/// under-reserved would show up as a later, larger failure.
fn band_expr(rounds: u32) -> String {
    format!(
        "F = fun G(0, F0) -> F0; \
              G(N, F0) -> try length(lists:duplicate(1 bsl (14 + N), gurka)) of \
                              _ -> G(N - 1, F0) \
                          catch _:_ -> G(N - 1, F0 + 1) end \
         end, io:format(\"~p~n\", [F({rounds}, 0)])."
    )
}
/// The `maps_SUITE` symptoms the same frontier defect was reported through: a large map built by
/// `maps:from_list/1`, walked by `maps:to_list/1` **and** by `maps:iterator/1` + `maps:next/2`,
/// rebuilt from its own pairs and folded — one expression, one printed tuple, compared with the
/// oracle element for element.
const LARGE_MAP: &str = "N = 1 bsl 12, M = maps:from_list([{I, I} || I <- lists:seq(1, N)]), \
     L = maps:to_list(M), \
     W = fun W(It0, Acc) -> case maps:next(It0) of \
             {K, V, Next} -> W(Next, [{K, V} | Acc]); \
             none -> Acc \
         end end, \
     E = W(maps:iterator(M), []), \
     M2 = maps:from_list(E), \
     io:format(\"~p~n\", [{maps:size(M), length(L), length(E), \
                          lists:sort(L) =:= lists:sort(E), M2 =:= M, \
                          maps:fold(fun(_, V, A) -> A + V end, 0, M)}]).";
/// `{4096,4096,4096,true,true,8390656}` — the oracle's answer for [`LARGE_MAP`] (4096 entries,
/// every walk the same length, both walks the same pairs, the rebuilt map equal, Σ1..4096).
const LARGE_MAP_GOLDEN: &str = "{4096,4096,4096,true,true,8390656}";
/// Asserts the probe succeeded, printed `golden`, and (through [`support::differential_eval`])
/// already agreed with the system oracle on exit code and stdout bytes.
fn assert_prints(probe: &support::EvalProbe, golden: &str, what: &str) {
    assert_eq!(
        probe.code,
        0,
        "{what}: `-eval` must succeed — a legal call may not raise; exit {} with stderr {:?}",
        probe.code,
        probe.stderr_text()
    );
    assert_eq!(
        probe.stdout_text().trim(),
        golden,
        "{what}: the printed answer must be the run's own count/term"
    );
}
/// The `-eval` route's own process: the BIF result is transient, the region is replenished
/// between calls, and the run must print [`NO_FAILURES`].
#[test]
fn a_large_transient_bif_result_in_the_eval_process_never_raises_system_limit() {
    const BANNER: &str = "bif_frontier_reservation";
    let expr = eval_loop_expr(ROUNDS);
    let probe = support::differential_eval(
        BANNER,
        "a large transient BIF result in the -eval process",
        &expr,
    );
    assert_prints(&probe, NO_FAILURES, "the -eval loop");
}
/// The same shape in a **spawned** process — the shape the defect report observed, where the
/// child's region comes out of the shared pool in whatever state the parent left it.
#[test]
fn the_same_shape_in_a_spawned_process_never_raises_system_limit() {
    const BANNER: &str = "bif_frontier_reservation";
    let expr = spawned_loop_expr(ROUNDS);
    let probe = support::differential_eval(
        BANNER,
        "a large transient BIF result in a spawned process",
        &expr,
    );
    assert_prints(&probe, NO_FAILURES, "the spawned loop");
}
/// The neighbouring sizes: 2^15 … 2^20 elements, each for [`BAND_ROUNDS`] rounds. This band
/// never failed pre-fix, so it is the "no overshoot in the other direction" half — a reservation
/// that demanded a region the pool cannot supply would turn these into failures.
#[test]
fn the_large_result_band_up_to_two_pow_twenty_never_raises_system_limit() {
    const BANNER: &str = "bif_frontier_reservation";
    let expr = band_expr(BAND_ROUNDS);
    let probe = support::differential_eval(
        BANNER,
        "the 2^15..2^20 band of large transient BIF results",
        &expr,
    );
    assert_prints(&probe, NO_FAILURES, "the band");
}
/// A configured `+hmax` is the one refusal the reservation must **not** widen: the heap policy is
/// the user's, so a call that cannot fit under it still raises `error:system_limit` — catchable
/// from the program, and the CLI's runtime-error exit code (1) when it escapes.
///
/// The oracle is deliberately not compared here: `+hmax` sizes a *process* heap limit that this
/// runtime also applies to its pooled BIF region, and the oracle completes the same call under
/// its collector. This is a documented own-state difference, asserted on this side alone.
#[test]
fn a_configured_hmax_still_refuses_a_call_that_cannot_fit() {
    const BANNER: &str = "bif_frontier_reservation";
    let caught = "try length(lists:duplicate(1 bsl 21, gurka)) of \
                      2097152 -> io:format(\"fits~n\"); \
                      _ -> io:format(\"fits~n\") \
                  catch C:R -> io:format(\"~p:~p~n\", [C, R]) end.";
    let observed = match support::run_runtime(&["-noshell", "+hmax", "8192", "-eval", caught]) {
        Some(observed) => observed,
        None => {
            support::skip_loudly(BANNER, "the `+hmax 8192` probe could not run to completion");
            return;
        }
    };
    assert_eq!(
        observed.code, 0,
        "a caught `system_limit` is an ordinary raise: exit {} with stderr {:?}",
        observed.code,
        observed.stderr.trim()
    );
    assert_eq!(
        observed.stdout.trim(),
        "error:system_limit",
        "the configured cap must still refuse the call, and the program must see it"
    );
    let uncaught = "length(lists:duplicate(1 bsl 21, gurka)).";
    let observed = match support::run_runtime(&["-noshell", "+hmax", "8192", "-eval", uncaught]) {
        Some(observed) => observed,
        None => {
            support::skip_loudly(BANNER, "the uncaught `+hmax 8192` probe could not run");
            return;
        }
    };
    assert_eq!(
        observed.code, 1,
        "an escaped `system_limit` is a runtime error (exit 1), never a capability gap: stderr {:?}",
        observed.stderr.trim()
    );
    assert!(
        observed.stderr.contains("system_limit"),
        "the CLI's diagnostic must name the reason: {:?}",
        observed.stderr.trim()
    );
}
/// The `maps` iterator/`to_list`/`from_list` shapes the same defect surfaced as. A large map
/// build is one big BIF allocation, and the walks are `maps:*` calls over the result.
#[test]
fn a_large_map_iterates_and_folds_like_the_oracle() {
    const BANNER: &str = "bif_frontier_reservation";
    let probe = support::differential_eval(
        BANNER,
        "a 4096-entry map walked by to_list and by iterator/next",
        LARGE_MAP,
    );
    assert_prints(&probe, LARGE_MAP_GOLDEN, "the large-map walk");
}
/// The D1 shape: a large list built **inside a fun invoked by a higher-order `lists` BIF** — the
/// nested-region first-call gap. `lists:foreach/2` takes the outermost region, its fun's
/// `lists:duplicate/2` takes a fresh 1 MiB nested region, and a `200000`-element result (400 000
/// words) needs more than the whole seed at once: the pure-constructor retry re-runs it on a wider
/// frontier, so the call returns `ok` where it used to raise a catchable `error:system_limit` for
/// `N >= 65537`. The oracle already returns `ok`, so `differential_eval` pins agreement in both
/// exit code and stdout bytes.
#[test]
fn a_large_list_built_inside_a_lists_foreach_fun_returns_ok() {
    const BANNER: &str = "bif_frontier_reservation";
    let expr = "io:format(\"~p~n\", \
                 [lists:foreach(fun(_) -> length(lists:duplicate(200000, x)) end, [1])]).";
    let probe = support::differential_eval(
        BANNER,
        "a 200000-element list built inside a lists:foreach fun",
        expr,
    );
    assert_prints(&probe, "ok", "the nested lists:foreach/duplicate shape");
}
/// The fixture the full 50-round probe runs: the pre-fix failure at round 4 came from exactly
/// this shape (a spawned process, one `1 bsl 21` call per round, the failing iterations returned
/// so a regression reports *which* round instead of a bare `false`).
const RUN_FIXTURE: &str = "-module(bif_frontier).\n\
     -export([run/0, rounds/1]).\n\
     \n\
     run() -> rounds(50).\n\
     \n\
     rounds(N) ->\n\
     \x20   Parent = self(),\n\
     \x20   {Pid, Ref} = spawn_monitor(fun() -> Parent ! {self(), loop(N, 1, [])} end),\n\
     \x20   receive\n\
     \x20       {Pid, Result} -> Result;\n\
     \x20       {'DOWN', Ref, process, Pid, Other} -> {died, Other}\n\
     \x20   end.\n\
     \n\
     loop(0, _I, Failed) -> lists:reverse(Failed);\n\
     loop(N, I, Failed) ->\n\
     \x20   case attempt(1 bsl 21) of\n\
     \x20       ok -> loop(N - 1, I + 1, Failed);\n\
     \x20       failed -> loop(N - 1, I + 1, [I | Failed])\n\
     \x20   end.\n\
     \n\
     attempt(Want) ->\n\
     \x20   try length(lists:duplicate(Want, gurka)) of\n\
     \x20       Want -> ok;\n\
     \x20       _Other -> failed\n\
     \x20   catch\n\
     \x20       _Class:_Reason -> failed\n\
     \x20   end.\n";
/// The defect report's exact shape: **50 rounds** of `length(lists:duplicate(1 bsl 21, gurka))`
/// in a spawned process on a compiled `run` target, which must print `[]` (no failing round).
///
/// `#[ignore]`d because it compiles a fixture with the system `erlc` (a loud skip without the
/// toolchain) and is the long form of the default cases above; it takes 1.2 s in `--release` when
/// it runs. Recipe:
/// `cargo test -p beam_rt --test bif_frontier_reservation -- --ignored`.
///
/// The pre-fix rule answers `[4]` (round 4) on this fixture in 5/5 outer runs; this tree answers
/// `[]` in 5/5.
#[test]
#[ignore = "needs erlc; the long form (50 rounds) of the default cases above"]
fn the_fifty_round_loop_on_a_compiled_run_target_never_fails() {
    const BANNER: &str = "bif_frontier_reservation";
    let fixture =
        match support::ErlangFixture::compile(BANNER, "bif_frontier", "bif_frontier", RUN_FIXTURE)
        {
            Some(fixture) => fixture,
            None => return,
        };
    let beam = fixture.beam();
    let output = Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .arg("run")
        .arg(&beam)
        .arg("bif_frontier:run/0")
        .output()
        .unwrap_or_else(|error| panic!("{BANNER}: spawning `run` for {} failed: {error}", beam.display()));
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{BANNER}: the 50-round run must complete — stdout {stdout:?} stderr {stderr:?}"
    );
    assert_eq!(
        stdout, "[]",
        "{BANNER}: a failing round is listed by number (the pre-fix rule answers `[4]`)"
    );
}
