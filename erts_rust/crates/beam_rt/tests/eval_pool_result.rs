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
//! The `-eval` route's **BIF-result store**, end to end (contract A.6/A.1).
//!
//! `RuntimeContext::call_registered` hands a container a registered BIF built off the pooled
//! BIF heap through to the caller. With no scheduled process to move the result onto — every
//! `-eval`/`-s`/`-run` route — a result whose whole graph is pointer-stable on the pooled region
//! is now handed through **by reference** (`Rehomed::KeepPool`, commit `c45f9a5c5b`): it stays
//! where the BIF built it and the pool is **not** rewound, so the pool frontier grows by the
//! result's words each call. The three cases below make that observable from outside the process:
//!
//! 1. a container a BIF built is *used later* in the same expression and printed at the end —
//!    so a rewind (or a pool that over-wrote a live word) would print garbage;
//! 2. a run of a **thousand** container BIF calls under a small `+hmax`: the pool frontier now
//!    accumulates one result per call, so the run **overruns** the cap (`system_limit`) instead of
//!    completing — the KNOWN, expected hand-through leak — while a bounded run still renders (see
//!    [`the_unrewound_pool_overruns_a_heap_cap`] and
//!    [`a_bounded_run_of_container_calls_still_renders`]);
//! 3. a container **retained across sub-expressions** while hundreds of later BIF calls keep
//!    their own results on the pool past it.
//!
//! Every case is compared with the system Erlang oracle by **trimmed stdout equality plus the
//! exit code**, and the expected rendering is pinned as a golden too, so a regression fails
//! even where the toolchain is absent. The oracle is discovered by filesystem only (never by
//! running it — `erl -noshell` blocks forever without `-s init stop`), and a missing toolchain
//! is a **loud skip**, never a silent pass (see `tests/support/mod.rs`).
#[allow(dead_code)]
mod support;

use std::process::Command;

/// Case 1 — a BIF-built container used later in the same expression, printed at the end.
const CONTAINER_USED_LATER: &str = "L = lists:seq(1,50), T = {hd(L), length(L), lists:last(L)}, \
     io:format(\"~w~n\",[T]).";

/// Case 2 — a thousand container BIF calls behind a fixed 20-element list. Under hand-through
/// every `list_to_tuple/1` result stays on the pool, so paired with the `+hmax 8192` cap
/// ([`HEAP_CAP`]) the pool cannot grow its way out and the run **overruns** it (`system_limit`)
/// instead of completing.
const MANY_CONTAINER_CALLS: &str = "S = lists:seq(1,20), \
     N = length([list_to_tuple(S) || _ <- lists:seq(1,1000)]), io:format(\"~w~n\",[N]).";

/// Case 3 — a container built first and read back after 500 further BIF calls have each rewound
/// the pool past it.
const CONTAINER_RETAINED: &str = "L = lists:seq(1,30), A = lists:last(L), \
     Tuples = [list_to_tuple(lists:seq(1,10)) || _ <- lists:seq(1,500)], \
     io:format(\"~w~n\",[{hd(L), A, length(Tuples)}]).";

/// The heap cap case 2 runs under: `+hmax N` sizes the **pooled BIF heap**. It is a *process-heap*
/// limit (`max_heap_size`) and deliberately does not bound the runtime's `-eval` store, which is a
/// runtime-level region no process owns (see `Runtime::eval_heap_ceiling`); the window below is
/// therefore the pool's.
///
/// Under hand-through the pool is **not** rewound, so the frontier accumulates one result per call:
/// the thousand `list_to_tuple/1` tuples (21 words each) plus the 1000-element result list total
/// ≈ 23 000 words, which overruns `+hmax 8192` (and even `+hmax 16384`) — the run raises
/// `system_limit` (exit 1) instead of completing. This is the KNOWN, expected hand-through leak;
/// reclaiming kept pool words is seam (b), which needs `beam_alloc`'s pool frontier and is out of
/// scope here.
const HEAP_CAP: &[&str] = &["+hmax", "8192"];
/// Runs `beam_rt -noshell <extra…> -eval <expr>` and returns `(trimmed stdout, exit code)`.
///
/// The invocation is the CLI's documented `-eval` route (contract A.6): the expression is
/// evaluated and prints nothing itself, so every observation below is what the expression
/// printed.
fn beam_eval(expr: &str, extra: &[&str]) -> (String, i32) {
    let mut args: Vec<&str> = Vec::with_capacity(3 + extra.len());
    args.push("-noshell");
    args.extend_from_slice(extra);
    args.push("-eval");
    args.push(expr);
    let output = Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args(&args)
        .output()
        .unwrap_or_else(|error| panic!("spawning beam_rt for {expr:?} failed: {error}"));
    (
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
        output.status.code().unwrap_or(-1),
    )
}

/// Asserts this runtime prints `golden` for `expr` (exit 0) and that the oracle agrees.
///
/// `banner` names the suite in the loud skip; `extra` is the argv this runtime alone needs
/// (case 2's heap cap, which the oracle does not share — the oracle has a collector).
fn assert_agrees_with_the_oracle(
    banner: &str,
    expr: &str,
    extra: &[&str],
    golden: &str,
) {
    let (stdout, exit) = beam_eval(expr, extra);
    assert_eq!(
        exit, 0,
        "`-eval` must succeed (the pool rewind must not disturb a live result): stderr/exit \
         {exit} with stdout {stdout:?}"
    );
    assert_eq!(
        stdout, golden,
        "the printed value must be the one the BIFs built — a rewind that recycled a live \
         word shows up here"
    );
    let Some(oracle) = support::require_oracle(banner) else {
        return;
    };
    match support::oracle_eval(&oracle, None, expr) {
        Some(expected) => assert_eq!(
            stdout, expected,
            "the `-eval` result must be byte-identical to the system oracle's"
        ),
        None => eprintln!("warning: the oracle could not answer `{expr}`"),
    }
}

/// A container a registered BIF built is **used later** in the same expression and printed —
/// so it has to survive the rewind of the pool it was built on.
#[test]
fn a_bif_container_used_later_in_the_expression_matches_the_oracle() {
    assert_agrees_with_the_oracle(
        "eval_pool_result",
        CONTAINER_USED_LATER,
        &[],
        "{1,50,50}",
    );
}

/// A thousand container calls under a heap cap: the pool frontier now accumulates one result per
/// call (the rewind is gone), so the run **overruns** the cap (`system_limit`, exit 1) instead of
/// completing. This is the KNOWN, expected hand-through leak — see [`HEAP_CAP`].
#[test]
fn the_unrewound_pool_overruns_a_heap_cap() {
    let (stdout, exit) = beam_eval(MANY_CONTAINER_CALLS, HEAP_CAP);
    assert_eq!(
        exit, 1,
        "the unrewound pool must overrun the cap (`system_limit`), but the run exited {exit}"
    );
    assert_eq!(stdout, "", "an overrun prints nothing before it raises");
}

/// A bounded number of container calls still completes and renders correctly under the same cap:
/// the hand-through is by-reference (no dangle), and a handful of results fit the pool frontier.
#[test]
fn a_bounded_run_of_container_calls_still_renders() {
    let bounded = "S = lists:seq(1,20), \
         N = length([list_to_tuple(S) || _ <- lists:seq(1,50)]), io:format(\"~w~n\",[N]).";
    assert_agrees_with_the_oracle("eval_pool_result", bounded, HEAP_CAP, "50");
}

/// A container retained across sub-expressions, with 500 later BIF calls rewinding the pool past
/// it — the hazard the "rewind only after a complete copy" rule exists for.
#[test]
fn a_container_retained_across_later_calls_matches_the_oracle() {
    assert_agrees_with_the_oracle(
        "eval_pool_result",
        CONTAINER_RETAINED,
        &[],
        "{1,30,500}",
    );
}
