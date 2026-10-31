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

//! The D2 end-to-end pin: a compiled `binary:part/3` window over an **engine-built** off-heap
//! (refc) parent must keep the parent's backing payload, so `binary:referenced_byte_size/1`
//! answers the parent's byte size (1025) rather than the window's own 510.
//!
//! The fixture (`tests/fixtures/binary_part_refc_window_fixture.erl`) builds
//! `B = <<"z", (binary:copy(<<255>>, 1024))/binary>>` — a 1025-byte off-heap binary the engine
//! materialises through its `bs_create_bin` path — then takes `C = binary:part(B, 1, 510)`.
//! On the engine's plain (non-in-machine) BIF path the machine hands its refc reservoir
//! (`Machine::refc_binaries`) through the `call_bif_outcome_with_off_heap` seam; the host must
//! thread that slice into its copy-out source table so `copy_struct` **shares** the parent
//! payload instead of flattening the `[BinRef][ErlSubBits]` window into on-heap bits.
//! `beam_rt run <fixture>.beam binary_part_refc_window_fixture:go/0` prints the result with
//! `~w~n`.
//!
//! A missing `erl`/`erlc` is a **loud** skip, never a silent pass.

use std::path::Path;
use std::process::Output;

mod support;

/// The suite's name in the loud-skip banner.
const SUITE: &str = "binary_part_refc_window";

/// The compiled fixture module, compiled at test time by the system `erlc`.
const MODULE: &str = "binary_part_refc_window_fixture";

/// The committed fixture source (the single artifact; never committed as a `.beam`).
const FIXTURE: &str = include_str!("fixtures/binary_part_refc_window_fixture.erl");

/// The `~w` rendering `beam_rt run` must print for `go/0`: the window's own `byte_size` (510)
/// and the **parent's** `referenced_byte_size` (1025).
const EXPECTED: &str = "{510,1025}";

/// Runs `binary_part_refc_window_fixture:go/0` through the scheduled `run` route.
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
fn a_compiled_binary_part_window_over_an_engine_built_parent_shares_the_parent_payload() {
    let Some(fixture) = support::ErlangFixture::compile(SUITE, "binary_part_refc_window", MODULE, FIXTURE)
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
        "the compiled fixture must print the window size and the parent's `referenced_byte_size/1`"
    );
}
