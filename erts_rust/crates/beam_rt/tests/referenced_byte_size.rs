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

//! The compiled `binary:referenced_byte_size/1` figures from
//! `lib/stdlib/test/binary_module_SUITE.erl`'s `referenced` and `doctests` cases, pinned on the
//! scheduled `run` route.
//!
//! `binary:referenced_byte_size/1` answers the size of the **backing** binary a (possibly sub-)
//! binary references, not the sub-binary's own `byte_size`. The fixture
//! (`tests/fixtures/referenced_byte_size_fixture.erl`) reproduces the two suite bodies'
//! expressions verbatim and returns every figure in the suite's order, so
//! `beam_rt run <fixture>.beam referenced_byte_size_fixture:go/0` prints the pinned list with
//! `~w~n`.
//!
//! These figures are **compiled-code** semantics. In particular `A = <<0:(1024 * 8)>>` in a
//! compiled module has `referenced_byte_size(A) == 1024` (exact, via the compiler's known-size
//! path), whereas the same expression on the `-eval` route reports the 2× growth of that
//! evaluator's heap allocation — the two routes must not be confused.
//!
//! A missing `erl`/`erlc` is a **loud** skip, never a silent pass.

use std::path::Path;
use std::process::Output;

mod support;

/// The suite's name in the loud-skip banner.
const SUITE: &str = "referenced_byte_size";

/// The compiled fixture module, compiled at test time by the system `erlc`.
const MODULE: &str = "referenced_byte_size_fixture";

/// The committed fixture source (the single artifact; never committed as a `.beam`).
const FIXTURE: &str = include_str!("fixtures/referenced_byte_size_fixture.erl");

/// The `~w` rendering `beam_rt run` must print for `go/0`.
const EXPECTED: &str = "[1024,1024000,1024,1023,1023,1024,1024000,128,128,1024000,{990,1000}]";

/// Runs `referenced_byte_size_fixture:go/0` through the scheduled `run` route.
fn run_go(fixture: &support::ErlangFixture) -> Output {
    let beam = fixture.beam();
    let target = format!("{MODULE}:go/0");
    support::run_bounded(
        Path::new(env!("CARGO_BIN_EXE_beam_rt")),
        &[
            "run",
            beam.to_str()
                .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", beam.display())),
            &target,
        ],
    )
    .unwrap_or_else(|| panic!("the runtime did not finish `run {target}` within the probe bound"))
}

#[test]
fn the_compiled_referenced_byte_size_figures_match_binary_module_suite() {
    let Some(fixture) =
        support::ErlangFixture::compile(SUITE, "referenced_byte_size", MODULE, FIXTURE)
    else {
        return;
    };
    let observed = run_go(&fixture);
    let stdout = String::from_utf8_lossy(&observed.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&observed.stderr).into_owned();
    assert_eq!(
        observed.status.code(),
        Some(0),
        "`run {MODULE}:go/0` must exit 0\nstdout: {stdout:?}\nstderr: {stderr:?}"
    );
    assert_eq!(
        stdout.trim(),
        EXPECTED,
        "the compiled fixture must print the suite's `referenced_byte_size/1` figures"
    );
}
