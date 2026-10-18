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
//! **A retained `erlang:system_info/1` value must survive the next key's fetch** (A.1/A.8).
//!
//! `Runtime::info_heap` is the runtime's **reset-per-call scratch** region
//! (`Runtime::reset_info_scratch`): `system_info`/`format_term` rewind it at entry. A value
//! that `erlang:system_info/1` hands to Erlang code therefore has to be **relocated** off it
//! first (`RuntimeContext::relocate_off_scratch`) — onto the running process's scanned young
//! heap, or the runtime's never-reset store on the table-less `-eval` route.
//!
//! Reading two keys in a row while the first is still bound is ordinary Erlang —
//! `epp:predef_macros/2` does exactly that (`machine`, then `otp_release`), and the compiler
//! reaches it for every module it compiles. When the value was left in place, the second call's
//! materialization recycled the first one's words, which showed up in two ways, both measured
//! against the pre-fix runtime:
//!
//! * a **wrong value** — `A = system_info(version), system_info(allocated_areas),
//!   system_info(machine), io:format("~p~n", [A])` printed `"BEAM"` (the third key's atom)
//!   instead of `"17.1"`;
//! * a **crash** — `A = system_info(otp_release), system_info(allocated_areas),
//!   io:format("~p~n", [A])` aborted the process (`SIGSEGV`, exit `139`) reading a
//!   recycled cons cell whose `car` was a tuple pointer, the pointer-level twin of
//!   `beam_term::ops::class`'s `invalid boxed header subtag` abort.
//!
//! Every case below is compared with the system Erlang oracle by **trimmed stdout equality plus
//! the exit code**, with the rendering pinned as a golden too, so a regression fails even where
//! the toolchain is absent. The probes read only keys whose value is the same in both runtimes
//! (the erts `version` string, `{wordsize, internal}`, and a *predicate* over `otp_release`)
//! — never `otp_release`'s digits themselves, which are this tree's release and not the
//! oracle's. A missing toolchain is a **loud skip**, never a silent pass.
#[allow(dead_code)]
mod support;
use std::process::Command;
/// A `version` string retained across the fetch of `allocated_areas`.
///
/// `allocated_areas` is the widest value the surface answers, so it overwrites every word the
/// version string occupied. Golden and oracle: `A="17.1"` (both runtimes are erts `17.1`).
const VERSION_ACROSS_ALLOCATED_AREAS: &str = "A = erlang:system_info(version), \
     _ = erlang:system_info(allocated_areas), io:format(\"A=~p~n\", [A]).";
/// The `epp:predef_macros/2` shape: a retained value, a wide key, then an **immediate** key.
///
/// Before the fix this printed the atom the third call answered (`A="BEAM"`) — a
/// well-formed, wrong value, which is worse than a crash because nothing reports it.
const VERSION_ACROSS_WIDE_AND_IMMEDIATE: &str = "A = erlang:system_info(version), \
     _ = erlang:system_info(allocated_areas), _ = erlang:system_info(machine), \
     io:format(\"A=~p~n\", [A]).";
/// The exact shape that **crashed** (exit `139`) before the fix.
///
/// The retained key is `otp_release`, so the assertion is made on a **predicate** over the
/// value — "it is a non-empty list" — rather than on its digits: the release string is this
/// tree's own (`30`), the oracle's is not (`29`), and both answer `true`.
const OTP_RELEASE_ACROSS_ALLOCATED_AREAS: &str = "A = erlang:system_info(otp_release), \
     _ = erlang:system_info(allocated_areas), io:format(\"~p~n\", [is_list(A) andalso \
     length(A) > 0]).";
/// The `{wordsize, internal}` tuple key — the other container shape the surface answers.
///
/// `8` on both runtimes, so it is directly comparable.
const WORDSIZE_TUPLE_ACROSS_ALLOCATED_AREAS: &str = "A = erlang:system_info({wordsize, \
     internal}), _ = erlang:system_info(allocated_areas), io:format(\"~w~n\", [A]).";

/// Runs `beam_rt -noshell -eval <expr>` and returns `(trimmed stdout, exit code)`.
///
/// The invocation is the CLI's documented `-eval` route (contract A.6): the expression prints
/// nothing itself, so every observation below is what the expression printed.
fn beam_eval(expr: &str) -> (String, i32) {
    let output = Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args(["-noshell", "-eval", expr])
        .output()
        .unwrap_or_else(|error| panic!("spawning beam_rt for {expr:?} failed: {error}"));
    (
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
        output.status.code().unwrap_or(-1),
    )
}

/// Asserts this runtime prints `golden` for `expr` (exit 0) and that the oracle agrees.
fn assert_agrees_with_the_oracle(banner: &str, expr: &str, golden: &str) {
    let (stdout, exit) = beam_eval(expr);
    assert_eq!(
        exit, 0,
        "`-eval` must succeed: a retained system_info value that was recycled aborts the \
         process on a recycled header word (stdout {stdout:?})"
    );
    assert_eq!(
        stdout, golden,
        "the printed value must be the one the first `system_info` call built — a repeated key \
         exercised instead of the retained value shows up here"
    );
    let Some(oracle) = support::require_oracle(banner) else {
        return;
    };
    match support::oracle_eval(&oracle, None, expr) {
        Some(expected) => assert_eq!(
            stdout, expected,
            "the retained value must be byte-identical to the system oracle's"
        ),
        None => eprintln!("warning: the oracle could not answer `{expr}`"),
    }
}

/// A retained `version` string across the widest key: the value must not be recycled.
#[test]
fn a_retained_version_string_survives_a_wide_key() {
    assert_agrees_with_the_oracle(
        "system_info_retained",
        VERSION_ACROSS_ALLOCATED_AREAS,
        "A=\"17.1\"",
    );
}

/// The `epp:predef_macros/2` shape — the one that printed a silently wrong value.
#[test]
fn a_retained_version_string_is_not_overwritten_by_a_later_immediate_key() {
    assert_agrees_with_the_oracle(
        "system_info_retained",
        VERSION_ACROSS_WIDE_AND_IMMEDIATE,
        "A=\"17.1\"",
    );
}

/// The exact shape that crashed the runtime before the fix.
#[test]
fn a_retained_otp_release_survives_a_wide_key() {
    assert_agrees_with_the_oracle(
        "system_info_retained",
        OTP_RELEASE_ACROSS_ALLOCATED_AREAS,
        "true",
    );
}

/// The `{wordsize, internal}` tuple key is a container too, and must survive as well.
#[test]
fn a_retained_wordsize_tuple_survives_a_wide_key() {
    assert_agrees_with_the_oracle(
        "system_info_retained",
        WORDSIZE_TUPLE_ACROSS_ALLOCATED_AREAS,
        "8",
    );
}
