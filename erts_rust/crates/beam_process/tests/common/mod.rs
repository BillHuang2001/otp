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

//! The shared **oracle driver** for this crate's integration tests that cross-check the real system
//! Erlang (`erl`, OTP 29.1.1 / erts-17.1).
//!
//! A test file pulls this in with `mod common;` (a `tests/common/mod.rs` subdirectory file is not
//! compiled as a test target of its own, so it is never run as a suite) and calls [`erl_stdout`]
//! and/or [`expect_oracle`]. Five files share it: `registry_monitors.rs`, `dict_oracle.rs`,
//! `monitor_tag.rs`, `capability_inventory.rs` and `port_monitor_cleanup.rs` (the last uses
//! [`erl_stdout`] directly because its probes compare more than a single trimmed line).
//!
//! # Protocol
//!
//! Each check runs an equivalent Erlang expression under the system `erl` as
//! `erl -noshell -kernel logger_level none -eval '<expr>, halt(0).'`, capturing both streams
//! (`stdout`/`stderr` are never inherited) on a worker thread with a [`ERL_TIMEOUT`] wall-clock
//! budget, so a wedged `erl` fails with a clear timeout instead of hanging the suite. A *spawned*
//! `erl` that exits non-zero is a bug in the embedded expression, so that is asserted rather than
//! skipped; `erl` that cannot be spawned at all (not on `PATH`) — or that does not answer in time —
//! **skips loudly** and returns, never failing the suite.
//!
//! # Why the logger is silenced, and why stdout is normalised
//!
//! A probe whose helper process crashes (the motivating case: `link(DeadPid)` signals `noproc`
//! from the dead pid, killing a non-trapping child) makes the default `erl` logger print a run-time
//! **error report** block on **stdout**, racing the probe's own `io:format`/`halt(0)`. Whether that
//! block is emitted before the emulator halts is non-deterministic, so the captured stdout is
//! sometimes just the probe's answer and sometimes that answer followed by
//! `=ERROR REPORT==== <timestamp> ===\n…\n` — and `str::trim` cannot remove it (it is not merely
//! trailing whitespace). Two independent defences:
//!
//! 1. **Suppress at the source:** `-kernel logger_level none` disables the logger outright (the
//!    deterministic fix; verified on OTP 29.1.1 / erts-17.1).
//! 2. **Normalise afterwards:** [`strip_logger_reports`] removes any residual report block — a line
//!    starting with `=`, through and including the following blank line — from the captured stdout,
//!    so a host whose `erl` ignores the flag still compares the probe's own output **exactly**.

#![allow(dead_code)]

use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Wall-clock budget for a single `erl` invocation before the oracle is declared unusable and the
/// affected check skips.
pub const ERL_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout, with any `erl` logger
/// report block stripped (see [`strip_logger_reports`]).
///
/// Returns `None` — after printing a clear skip message — when `erl` cannot be spawned or does not
/// answer within [`ERL_TIMEOUT`]. A *spawned* `erl` that exits non-zero is a bug in the embedded
/// expression, so that is asserted rather than skipped.
pub fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    let (tx, rx) = mpsc::channel();
    let code_for_thread = code.clone();
    thread::spawn(move || {
        let out = Command::new("erl")
            .arg("-noshell")
            // Deterministic suppression of the run-time error-report blocks a crashing helper
            // process would otherwise print on stdout, racing the probe's own output.
            .arg("-kernel")
            .arg("logger_level")
            .arg("none")
            .arg("-eval")
            .arg(&code_for_thread)
            .output();
        // The receiver may have timed out and gone away; that is fine.
        let _ = tx.send(out);
    });

    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!("skipping oracle check: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(e)) => {
            eprintln!("skipping oracle check: could not spawn `erl` ({e}); is Erlang on PATH?");
            None
        }
        Ok(Ok(out)) => {
            assert!(
                out.status.success(),
                "`erl` exited with {} for expression:\n{code}\nstderr:\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr),
            );
            Some(strip_logger_reports(&String::from_utf8_lossy(&out.stdout)))
        }
    }
}

/// Runs `expr` under the oracle and asserts the printed result is `expected`.
///
/// Returns `false` (after the driver's loud skip message) when the oracle is unusable, so a
/// caller can `return` early; an oracle that *answers with something else* is a failure.
pub fn expect_oracle(expr: &str, expected: &str) -> bool {
    let Some(out) = erl_stdout(expr) else {
        return false;
    };
    assert_eq!(
        out.trim(),
        expected,
        "the oracle disagrees with the expected result for:\n{expr}"
    );
    true
}

/// Removes every `erl` logger **report block** from `stdout`.
///
/// A report block starts with a line beginning `=` (e.g. `=ERROR REPORT==== <timestamp> ===`,
/// `=SUPERVISOR REPORT==== …`) and runs through and including the following blank line. Anything
/// else — the probe's own printed observables — is preserved verbatim, so the comparison stays
/// exact. A block that reaches end-of-input without a trailing blank line is dropped to the end.
pub fn strip_logger_reports(stdout: &str) -> String {
    let mut kept = String::with_capacity(stdout.len());
    let mut in_report = false;
    for chunk in stdout.split_inclusive('\n') {
        let body = chunk.strip_suffix('\n').unwrap_or(chunk);
        if in_report {
            // The block ends at (and includes) the next blank line.
            if body.is_empty() {
                in_report = false;
            }
            continue;
        }
        if body.starts_with('=') {
            in_report = true;
            continue;
        }
        kept.push_str(chunk);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::strip_logger_reports;

    #[test]
    fn a_trailing_error_report_block_is_stripped() {
        let raw = "true|true|true|true\n\
                   =ERROR REPORT==== 10-Oct-2026::03:39:20.833664 ===\n\
                   Error in process <0.85.0> with exit value:\n\
                   {noproc,[{erlang,link,\n\
                   \x20                [<0.83.0>],\n\
                   \x20                [{error_info,#{module => erl_erts_errors}}]}]}\n\n";
        assert_eq!(strip_logger_reports(raw), "true|true|true|true\n");
    }

    #[test]
    fn a_report_without_a_trailing_blank_line_is_stripped_to_eof() {
        let raw = "1|2|3\n=ERROR REPORT==== 10-Oct-2026 ===\nboom";
        assert_eq!(strip_logger_reports(raw), "1|2|3\n");
    }

    #[test]
    fn two_report_blocks_are_both_stripped() {
        let raw = "a\n=REPORT==== x ===\nbody\n\nb\n=REPORT==== y ===\nbody2\n\nc\n";
        assert_eq!(strip_logger_reports(raw), "a\nb\nc\n");
    }

    #[test]
    fn clean_output_is_untouched() {
        let raw = "ok ok\n";
        assert_eq!(strip_logger_reports(raw), "ok ok\n");
        // A `=` inside a line (not at its start) is ordinary output.
        let raw = "a=b|c\n";
        assert_eq!(strip_logger_reports(raw), "a=b|c\n");
    }
}
