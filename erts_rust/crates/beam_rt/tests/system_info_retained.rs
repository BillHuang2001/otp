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
//! the toolchain is absent. The probes read only keys whose value is comparable across hosts
//! (`{wordsize, internal}`, a *predicate* over `otp_release`, and the erts `version` string) —
//! never `otp_release`'s digits themselves, which are this tree's release and not the oracle's.
//!
//! **This runtime pins the tree's frozen erts version; the oracle may be a different erts
//! *patch*.** `beam_common::ERTS_VERSION` is a compile-time constant, so this runtime answers it
//! on every host, while `erlang:system_info(version)` under the system oracle answers whatever
//! erts the installed OTP carries (e.g. `"17.1.1"` on an erts-17.1.1 host). The golden assertion
//! therefore stays unconditional — the frozen value is the correct answer — and the oracle
//! comparison tolerates **exactly one** difference: the oracle's own version literal, measured
//! per run by [`assert_agrees_with_the_oracle`]. Any other divergence, including a different
//! version *number* on this side, still fails.
//!
//! A missing toolchain is a **loud skip**, never a silent pass.
#[allow(dead_code)]
mod support;
use std::path::Path;
use std::process::Command;
/// A `version` string retained across the fetch of `allocated_areas`.
///
/// `allocated_areas` is the widest value the surface answers, so it overwrites every word the
/// version string occupied. Golden: `A="17.1"` — the tree's frozen erts version
/// (`beam_common::ERTS_VERSION`), which this runtime answers on **every** host. The oracle
/// answers *its own* erts version, which may be a different **patch** (`A="17.1.1"` on an
/// erts-17.1.1 host); only that literal is accommodated in the oracle comparison.
const VERSION_ACROSS_ALLOCATED_AREAS: &str = "A = erlang:system_info(version), \
     _ = erlang:system_info(allocated_areas), io:format(\"A=~p~n\", [A]).";
/// The `epp:predef_macros/2` shape: a retained value, a wide key, then an **immediate** key.
///
/// Before the fix this printed the atom the third call answered (`A="BEAM"`) — a
/// well-formed, wrong value, which is worse than a crash because nothing reports it.
///
/// Golden `A="17.1"` for the same reason as above: the frozen erts version is the answer here,
/// and the oracle's own patch literal is the one tolerated difference.
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

/// The oracle's own erts `version` string, as `erlang:system_info(version)` renders it there.
///
/// Measured per run rather than assumed: the system oracle is whatever OTP the host carries, so
/// its erts **patch** may differ from the tree's frozen [`beam_common::ERTS_VERSION`] (a host
/// whose `erts-17.1.1` answers `"17.1.1"` where this tree pins `"17.1"`). `None` when the probe
/// cannot answer — the caller then compares plainly instead of tolerating anything.
fn oracle_erts_version(oracle: &Path) -> Option<String> {
    let version = support::oracle_eval(
        oracle,
        None,
        "io:format(\"~s~n\", [erlang:system_info(version)]).",
    )?;
    if version.is_empty() {
        eprintln!("warning: the oracle answered an empty `system_info(version)`");
        return None;
    }
    Some(version)
}

/// What this runtime's `stdout` must look like once the oracle's own erts patch is accounted for.
///
/// The runtime answers the tree's **frozen** erts version (`beam_common::ERTS_VERSION`) on every
/// host, while the oracle answers its own — possibly a different *patch*. The single tolerated
/// difference is that literal: when the oracle's version differs from the frozen one, the frozen
/// version literal in `stdout` is substituted with the oracle's own, and everything else must
/// still match byte for byte. When the versions are equal this is `stdout` unchanged, and when
/// the oracle's version cannot be measured it is `stdout` unchanged too (a loud warning, never
/// silence) — a genuinely different value fails either way, because only the version literal is
/// rewritten.
fn expected_under_the_oracle(stdout: &str, oracle: &Path) -> String {
    match oracle_erts_version(oracle) {
        Some(version) if version != beam_common::ERTS_VERSION => {
            let frozen = format!("\"{}\"", beam_common::ERTS_VERSION);
            let observed = format!("\"{version}\"");
            stdout.replace(&frozen, &observed)
        }
        Some(_) => stdout.to_string(),
        None => {
            eprintln!(
                "warning: the oracle's `system_info(version)` could not be measured; comparing \
                 plainly, so an erts patch difference on this host will show up as a failure"
            );
            stdout.to_string()
        }
    }
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
            expected_under_the_oracle(&stdout, &oracle),
            expected,
            "the retained value must be byte-identical to the system oracle's, apart from the \
             oracle's own erts version literal (this runtime pins the tree's frozen \
             `beam_common::ERTS_VERSION`; the oracle may be a different erts patch)"
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
