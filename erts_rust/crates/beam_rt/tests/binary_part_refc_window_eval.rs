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

//! The D2 end-to-end pin on the `-eval` **tree-walker** route: a compiled
//! `binary_part_refc_window_fixture:parent/0` returns an **engine-built** off-heap (refc)
//! parent binary, the tree-walker then takes a 510-byte `binary:part/3` window over it, and
//! the window must keep the parent's backing payload — so `binary:referenced_byte_size/1`
//! answers the parent's byte size (1025), not the window's own 510.
//!
//! This is the third leg of the D2 matrix, beside the scheduled `run` route
//! (`tests/binary_part_refc_window.rs`, the same 1025) and the inline tree-walker route
//! (`tests/eval_refc_bits.rs::binary_part_window_reports_parent_capacity`, whose 2050 is the
//! tree-walker's grown-capacity accumulator). Here the parent is built by a compiled module,
//! so it is exact-size (1025), and the value is cross-checked against the system oracle.
//!
//! A missing `erl`/`erlc` is a **loud** skip, never a silent pass.

use std::path::Path;
use std::process::Output;

mod support;

/// The suite's name in the loud-skip banner.
const SUITE: &str = "binary_part_refc_window_eval";

/// The compiled fixture module, compiled at test time by the system `erlc`.
const MODULE: &str = "binary_part_refc_window_fixture";

/// The committed fixture source (shared with `tests/binary_part_refc_window.rs`).
const FIXTURE: &str = include_str!("fixtures/binary_part_refc_window_fixture.erl");

/// The `~w` rendering both runtimes must print: the window's own `byte_size` (510) and the
/// **parent's** `referenced_byte_size` (1025).
const EXPECTED: &str = "{510,1025}";

/// The `-eval` expression under test: an engine-built parent via `parent/0`, then the window.
const EVAL_EXPR: &str = "B = binary_part_refc_window_fixture:parent(), \
     C = binary:part(B, 1, 510), \
     io:format(\"~w~n\", [{byte_size(C), binary:referenced_byte_size(C)}]), halt(0).";

/// `<beam_rt> -noshell -pa dir -eval expr` through the built binary (the `-pa` entry comes
/// before `-eval`, so the code path is in place when the expression runs).
fn run_eval(fixture: &support::ErlangFixture) -> Output {
    let dir = fixture
        .dir()
        .to_str()
        .unwrap_or_else(|| panic!("the fixture dir is not UTF-8: {}", fixture.dir().display()));
    support::run_bounded(
        Path::new(env!("CARGO_BIN_EXE_beam_rt")),
        &["-noshell", "-pa", dir, "-eval", EVAL_EXPR],
    )
    .unwrap_or_else(|| panic!("the runtime did not finish the `-eval` probe within the bound"))
}

#[test]
fn an_engine_built_parent_window_on_the_eval_route_reports_the_parent_payload() {
    let Some(fixture) =
        support::ErlangFixture::compile(SUITE, "binary_part_refc_window_eval", MODULE, FIXTURE)
    else {
        return;
    };
    let observed = run_eval(&fixture);
    let stdout = String::from_utf8_lossy(&observed.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&observed.stderr).into_owned();
    assert_eq!(
        observed.status.code(),
        Some(0),
        "the `-eval` probe must exit 0\nstdout: {stdout:?}\nstderr: {stderr:?}"
    );
    assert_eq!(stderr, "", "the `-eval` probe must be silent on stderr");
    assert_eq!(
        stdout.trim(),
        EXPECTED,
        "the compiled fixture must print the window size and the parent's `referenced_byte_size/1`"
    );

    // The same expression under the system oracle.
    let Some(oracle) = support::require_oracle(SUITE) else {
        return;
    };
    let Some(theirs) = support::oracle_eval(&oracle, Some(fixture.dir()), EVAL_EXPR) else {
        support::skip_loudly(
            SUITE,
            "the oracle could not answer the engine-built-parent `-eval` probe",
        );
        return;
    };
    assert_eq!(
        theirs, EXPECTED,
        "the system erl must print {EXPECTED} for the engine-built-parent window"
    );
}
