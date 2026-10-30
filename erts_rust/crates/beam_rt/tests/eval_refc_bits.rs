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

//! The `-eval` evaluator's **refc-binary** construction at the CLI boundary: a `<<…>>`
//! result past the 64-byte on-heap limit is materialized with a grown-capacity payload,
//! so `binary:referenced_byte_size/1` answers the C writable accumulator's `orig_size`
//! rather than the apparent byte size.
//!
//! These are **deterministic tree values** — no oracle cross-check. The two shapes pin
//! the C's growth rule against this runtime's own stdout, through the same route every
//! `-eval` test uses (`env!("CARGO_BIN_EXE_beam_rt")` → CLI parser → evaluator → `~w`
//! printer → exit code).

use std::path::Path;
use std::process::Output;

mod support;

/// Runs `expr` through the built binary's `-eval` route and returns the trimmed
/// `(stdout, exit, stderr)`, panicking if the child could not answer within the bound or
/// was killed by a signal.
fn run_eval(expr: &str) -> (String, i32, String) {
    let binary = Path::new(env!("CARGO_BIN_EXE_beam_rt"));
    let output = support::run_bounded(binary, &["-noshell", "-eval", expr])
        .unwrap_or_else(|| panic!("beam_rt could not answer {expr:?} within the bound"));
    observe(&output).unwrap_or_else(|| panic!("beam_rt was killed by a signal: {expr:?}"))
}

/// `(trimmed_stdout, exit, trimmed_stderr)`, or `None` when the child was killed by a
/// signal (so no exit code exists).
fn observe(output: &Output) -> Option<(String, i32, String)> {
    Some((
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string(),
        output.status.code()?,
        String::from_utf8_lossy(&output.stderr)
            .trim_end()
            .to_string(),
    ))
}

/// A 1025-byte `<<…>>` result: apparent size 1025, referenced (grown-capacity) size
/// 2050 — the C `create_binary` accumulator's `2× used` growth of a 1025-byte payload.
#[test]
fn binary_construction_reports_grown_capacity() {
    let (stdout, exit, stderr) = run_eval(
        "B = <<\"z\",(binary:copy(<<255>>,1024))/binary>>, \
         io:format(\"~w~n\",[{byte_size(B), binary:referenced_byte_size(B)}]), halt(0).",
    );
    assert_eq!(stderr, "", "unexpected stderr {stderr:?}");
    assert_eq!(stdout, "{1025,2050}");
    assert_eq!(exit, 0);
}

/// A 510-byte window over that same payload still answers the parent payload's grown
/// capacity (2050), while its own byte size is 510.
#[test]
fn binary_part_window_reports_parent_capacity() {
    let (stdout, exit, stderr) = run_eval(
        "B = <<\"z\",(binary:copy(<<255>>,1024))/binary>>, \
         C = binary:part(B, 1, 510), \
         io:format(\"~w~n\",[{byte_size(C), binary:referenced_byte_size(C)}]), halt(0).",
    );
    assert_eq!(stderr, "", "unexpected stderr {stderr:?}");
    assert_eq!(stdout, "{510,2050}");
    assert_eq!(exit, 0);
}
