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

#![deny(unsafe_code)]

//! The CLI contract of the `dist_probe` companion binary (the differential
//! harness's live two-node loopback driver — see `src/bin/dist_probe.rs`).
//!
//! Each report must produce **exactly one** stdout line (no trailing spaces, a
//! single `\n`) and exit `0`; an unknown or missing subcommand must exit `2` with
//! a usage line on stderr and no stdout. Cargo makes a package's own binary
//! available to its integration tests through `CARGO_BIN_EXE_<name>`.

use std::process::{Command, Output};

/// Runs the built `dist_probe` binary with `args`.
fn dist_probe(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dist_probe"))
        .args(args)
        .output()
        .expect("run the dist_probe binary")
}

/// The exact stdout of a successful run (one line, a single trailing newline).
fn stdout_line(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

#[test]
fn each_report_prints_exactly_one_expected_line_and_exits_zero() {
    for (report, expected) in [
        ("ping", "pong"),
        ("nodes", "1"),
        ("term", "836803610161026103"),
    ] {
        let output = dist_probe(&["loopback", report]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "`loopback {report}` exits 0 (stderr: {})",
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            stdout_line(&output),
            format!("{expected}\n"),
            "`loopback {report}` prints exactly `{expected}` and one newline",
        );
        assert!(
            output.stderr.is_empty(),
            "`loopback {report}` writes nothing to stderr: {}",
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

#[test]
fn an_unknown_or_missing_subcommand_exits_two_with_a_usage_line() {
    for args in [
        vec![],
        vec!["loopback"],
        vec!["loopback", "bogus"],
        vec!["bogus", "ping"],
        vec!["loopback", "ping", "extra"],
    ] {
        let output = dist_probe(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "`{args:?}` is a usage error",
        );
        assert!(
            output.stdout.is_empty(),
            "`{args:?}` prints no stdout on a usage error",
        );
        assert!(
            !output.stderr.is_empty(),
            "`{args:?}` prints a usage line on stderr",
        );
    }
}
