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

//! The CLI contract the performance-regression harness (`examples/perf_bif.rs`) is built
//! on, pinned at the executable boundary (`CARGO_BIN_EXE_beam_rt`) so it cannot drift
//! silently.
//!
//! The harness measures `<beam_rt> -pa <dir> run <file> <spec> <N> 0` at each of two large
//! iteration counts (a two-point slope, fastest of k with a discarded warm-up), and
//! *verifies every run* - warm-ups included: it compares the child's stdout with the value
//! the fixture is expected to return (`0` for `pc:add/2`, `N` for `pc:loop/2`). Two facts
//! make that possible, and both are asserted here:
//!
//! 1. `run` **prints the returned value** (`~w~n`), so a run that did not execute the
//!    workload is distinguishable from a fast one - the self-check the harness relies on.
//! 2. `run` requires **one argument per declared arity** (a mismatch is a usage error,
//!    exit 2), which is why the harness passes the accumulator `0` along with `N` and
//!    never a bare `pc:add/2 <N>`.
//!
//! The module is the committed `beam_loader` oracle fixture (`slice.beam`, `add/2`), reused
//! by relative path so there is a single source of truth for it - no `erlc` is needed, so
//! these assertions run everywhere.

use std::process::{Command, Output};

/// The `beam_loader` oracle fixture (reused, never copied).
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

/// Runs the built `beam_rt` binary with `args` and captures its output.
fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args(args)
        .output()
        .expect("spawn the beam_rt executable")
}

/// A `run` that succeeds prints the function's return value - the harness's self-check.
#[test]
fn run_prints_the_returned_value_so_a_workload_can_self_check() {
    let out = run(&["run", SLICE_BEAM, "slice:add/2", "2", "3"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "`run slice:add/2 2 3` must exit 0, got {:?}; stderr: {stderr}",
        out.status.code()
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "5",
        "`run` prints the result with `~w~n`"
    );
}

/// The argument count must equal the declared arity - the reason `perf_bif` calls its
/// fixture with `N` **and** the accumulator rather than with `N` alone.
#[test]
fn run_requires_one_argument_per_declared_arity() {
    let out = run(&["run", SLICE_BEAM, "slice:add/2", "2"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a missing argument is a usage error: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // ... and the same binary run with the full argument list succeeds, so the exit 2 above
    // is about the arity and not about the module or the file.
    assert!(run(&["run", SLICE_BEAM, "slice:add/2", "2", "3"])
        .status
        .success());
}
