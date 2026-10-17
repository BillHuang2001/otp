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
//! `RuntimeContext::call_registered` moves a container a registered BIF built off the pooled
//! BIF heap and then rewinds the pool to that call's mark ([`crate::context::pool`]). With no
//! scheduled process to move the result onto — every `-eval`/`-s`/`-run` route — the
//! destination is the runtime's long-lived store (`Runtime::eval_heap`, whose off-heap payload
//! owner is `Runtime::eval_off_heap`). The rewind is only sound **because** the copy is
//! complete and pointer-stable, and the three cases below are what makes that observable from
//! outside the process:
//!
//! 1. a container a BIF built is *used later* in the same expression and printed at the end —
//!    so a rewind that recycled a live word would print garbage;
//! 2. a run of a **thousand** container BIF calls under a small `+hmax`: each call hands its
//!    words back, so the pool's high-water stays at one call's worth. This is the case the
//!    rewind exists for, and it is a real discriminator: the same expression without the
//!    rewind raises `system_limit` at every cap up to `+hmax 16384` (measured), because the
//!    pool's frontier marches to the cap instead of returning to the mark;
//! 3. a container **retained across sub-expressions** while hundreds of later BIF calls rewind
//!    the pool past it.
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

/// Case 2 — a thousand container BIF calls behind a fixed 20-element list. Paired with the
/// `+hmax 8192` cap ([`HEAP_CAP`]) the pool cannot grow its way out, so the run completes only
/// because every call gives its words back.
const MANY_CONTAINER_CALLS: &str = "S = lists:seq(1,20), \
     N = length([list_to_tuple(S) || _ <- lists:seq(1,1000)]), io:format(\"~w~n\",[N]).";

/// Case 3 — a container built first and read back after 500 further BIF calls have each rewound
/// the pool past it.
const CONTAINER_RETAINED: &str = "L = lists:seq(1,30), A = lists:last(L), \
     Tuples = [list_to_tuple(lists:seq(1,10)) || _ <- lists:seq(1,500)], \
     io:format(\"~w~n\",[{hd(L), A, length(Tuples)}]).";

/// The heap cap case 2 runs under: `+hmax N` sizes the pooled BIF heap **and** the store.
///
/// The window is measured, not guessed. Case 2 needs at least `+hmax 4096` on this runtime (at
/// 2048 the cap binds for an unrelated reason) and the *un-rewound* pool needs more than
/// `+hmax 16384` to finish the same expression — so 8192 discriminates with a 2× margin on
/// both sides.
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

/// A thousand container calls under a heap cap: the pool's frontier returns to the call's mark
/// every time, so the run fits a cap the un-rewound pool overruns (see [`HEAP_CAP`]).
#[test]
fn a_thousand_container_calls_fit_a_heap_cap_the_unrewound_pool_overruns() {
    assert_agrees_with_the_oracle(
        "eval_pool_result",
        MANY_CONTAINER_CALLS,
        HEAP_CAP,
        "1000",
    );
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
