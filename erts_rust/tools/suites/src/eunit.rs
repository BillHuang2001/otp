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
//! Parsing eunit's human-readable output.
//!
//! The drive expression is exactly `eunit:test(Module, [verbose])`, so the counts come from
//! the summary eunit prints. Three shapes matter (all observed on OTP 29 and reproduced by
//! the Rust runtime):
//!
//! ```text
//!   2 tests passed.                                  % success
//!   Failed: 1.  Skipped: 0.  Passed: 2.              % at least one failure
//!   There were no tests to run.                      % module has no eunit tests
//! ```
//!
//! plus the *cancelled* shapes, which must never be read as a 0/0 pass:
//! `*** context setup failed ***`, `*** test module not found ***` and the trailing
//! `One or more tests were cancelled.` line. Older eunit spells the success line
//! `All 2 tests passed.`, so both are accepted.
use crate::report::{Outcome, Status};
/// What eunit's output said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EunitStatus {
    /// Every test that ran passed.
    Passed,
    /// At least one test failed.
    Failed,
    /// The run was cancelled (setup crash, missing module, aborted group) — the summary may
    /// say `Passed: 0`, but nothing was actually measured.
    Cancelled,
    /// The module holds no eunit tests (e.g. a `common_test` suite or a helper module).
    NoTests,
    /// Output did not carry any recognisable summary.
    Unknown,
}
impl EunitStatus {
    /// The status token used in reports.
    pub fn as_str(&self) -> &'static str {
        match self {
            EunitStatus::Passed => "passed",
            EunitStatus::Failed => "failed",
            EunitStatus::Cancelled => "cancelled",
            EunitStatus::NoTests => "no_tests",
            EunitStatus::Unknown => "unparsed",
        }
    }
}
/// The parsed eunit summary for one module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EunitSummary {
    /// What happened.
    pub status: EunitStatus,
    /// Tests that passed (0 when the output carried no counts).
    pub passed: u32,
    /// Tests that failed.
    pub failed: u32,
    /// Tests that were skipped by eunit itself.
    pub skipped: u32,
    /// The first failing test (or the cancellation reason), for the report.
    pub detail: String,
}
impl EunitSummary {
    /// Converts the summary into the crate-wide [`Outcome`] used by every row.
    pub fn outcome(&self) -> Outcome {
        let status = match self.status {
            EunitStatus::Passed => Status::Pass,
            EunitStatus::Failed => Status::Fail,
            EunitStatus::Cancelled => Status::Cancelled,
            EunitStatus::NoTests => Status::NoTests,
            EunitStatus::Unknown => Status::Error,
        };
        Outcome {
            status,
            passed: self.passed,
            failed: self.failed,
            skipped: self.skipped,
            detail: self.detail.clone(),
        }
    }
}
/// Parses eunit's verbose output for one module.
pub fn parse(stdout: &str) -> EunitSummary {
    let mut counts: Option<(u32, u32, u32)> = None;
    let mut simple_passed: Option<u32> = None;
    let mut cancelled = false;
    let mut no_tests = false;
    for line in stdout.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("Failed:") {
            // "Failed: 1.  Skipped: 0.  Passed: 2."
            let rest = rest.trim_start();
            let (failed, consumed) = leading_number(rest);
            let rest = &rest[consumed..];
            let skipped = rest
                .split_once("Skipped:")
                .map(|(_, r)| leading_number(r.trim_start()).0)
                .unwrap_or(0);
            let passed = rest
                .split_once("Passed:")
                .map(|(_, r)| leading_number(r.trim_start()).0)
                .unwrap_or(0);
            counts = Some((failed, skipped, passed));
        } else if t == "There were no tests to run." {
            no_tests = true;
        } else if t.contains("tests were cancelled") || t.contains("test module not found") || t.contains("context setup failed") {
            cancelled = true;
        } else if let Some(rest) = t.strip_prefix("All ") {
            // "All 2 tests passed." (older eunit spelling).
            if rest.ends_with("tests passed.") {
                simple_passed = Some(leading_number(rest).0);
            }
        } else if rest_is_tests_passed(t) {
            simple_passed = Some(leading_number(t).0);
        }
    }
    let detail = first_failure(stdout).or_else(|| first_star_message(stdout)).unwrap_or_default();
    if cancelled {
        let (failed, skipped, passed) = counts.unwrap_or((0, 0, 0));
        return EunitSummary { status: EunitStatus::Cancelled, passed, failed, skipped, detail };
    }
    if no_tests {
        return EunitSummary { status: EunitStatus::NoTests, passed: 0, failed: 0, skipped: 0, detail };
    }
    if let Some((failed, skipped, passed)) = counts {
        let status = if failed > 0 {
            EunitStatus::Failed
        } else if passed > 0 {
            EunitStatus::Passed
        } else if skipped > 0 {
            // Everything was skipped: nothing ran, so it is not a pass.
            EunitStatus::Cancelled
        } else {
            EunitStatus::Cancelled
        };
        return EunitSummary { status, passed, failed, skipped, detail };
    }
    if let Some(passed) = simple_passed {
        return EunitSummary { status: EunitStatus::Passed, passed, failed: 0, skipped: 0, detail };
    }
    EunitSummary { status: EunitStatus::Unknown, passed: 0, failed: 0, skipped: 0, detail }
}
/// True for the line `"2 tests passed."` (the modern success summary).
fn rest_is_tests_passed(line: &str) -> bool {
    if !line.ends_with("tests passed.") {
        return false;
    }
    let (_, digits) = leading_number(line);
    digits > 0 && line[digits..].starts_with(" tests passed.")
}
/// Reads the leading run of ASCII digits, returning `(value, bytes_consumed)`.
fn leading_number(s: &str) -> (u32, usize) {
    let bytes = s.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    (s[..idx].parse().unwrap_or(0), idx)
}
/// The first `module: test...*failed*` line plus the `**error:…`/`**exit:…` reason that
/// follows it — the smallest diagnostic a failed row can carry.
pub fn first_failure(stdout: &str) -> Option<String> {
    let lines: Vec<&str> = stdout.lines().collect();
    let idx = lines.iter().position(|l| l.contains("*failed*"))?;
    let label = lines[idx].trim().replace("...*failed*", "");
    let reason = lines[idx + 1..]
        .iter()
        .take(60)
        .find(|l| l.trim_start().starts_with("**"))
        .map(|l| l.trim().to_string())
        .unwrap_or_default();
    Some(if reason.is_empty() { label } else { format!("{label}: {reason}") })
}
/// The first `**error:…`/`**exit:…`/`**throw:…` line (used for cancellations, where no test
/// is named). A bare `*** context setup failed ***` banner is skipped in favour of the real
/// reason underneath it.
pub fn first_star_message(stdout: &str) -> Option<String> {
    let starred: Vec<&str> = stdout.lines().map(str::trim).filter(|l| l.starts_with("**")).collect();
    starred
        .iter()
        .find(|l| l.starts_with("**error:") || l.starts_with("**exit:") || l.starts_with("**throw:"))
        .or_else(|| starred.first())
        .map(|l| l.to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_modern_pass_summary() {
        let out = "======================== EUnit ========================\nmodule 't_pass'\n  t_pass: ok_test...ok\n  [done in 0.006 s]\n=======================================================\n  2 tests passed.\n";
        let s = parse(out);
        assert_eq!(s.status, EunitStatus::Passed);
        assert_eq!((s.passed, s.failed, s.skipped), (2, 0, 0));
    }
    #[test]
    fn parses_legacy_all_tests_passed() {
        let s = parse("  All 7 tests passed.\n");
        assert_eq!(s.status, EunitStatus::Passed);
        assert_eq!(s.passed, 7);
    }
    #[test]
    fn parses_failure_and_names_the_first_test() {
        let out = "module 't_fail'\n  t_fail: ok_test...ok\n  t_fail: bad_test...*failed*\nin function t_fail:bad_test/0 (t_fail.erl, line 4)\n**error:{assertEqual,[{module,t_fail},{line,4}]}\n  [done in 0.009 s]\n  Failed: 1.  Skipped: 0.  Passed: 2.\n";
        let s = parse(out);
        assert_eq!(s.status, EunitStatus::Failed);
        assert_eq!((s.passed, s.failed, s.skipped), (2, 1, 0));
        assert!(s.detail.starts_with("t_fail: bad_test"), "{}", s.detail);
        assert!(s.detail.contains("**error:"), "{}", s.detail);
    }
    #[test]
    fn no_tests_is_its_own_status() {
        let s = parse("module 't_none'\n  There were no tests to run.\n");
        assert_eq!(s.status, EunitStatus::NoTests);
        assert_eq!(s.outcome().status, Status::NoTests);
    }
    #[test]
    fn setup_crash_is_cancelled_not_a_zero_pass() {
        let out = "module 't_setup'\n  *** context setup failed ***\n**in function t_setup:'-setup_test_/0-fun-3-'/0 (t_setup.erl, line 3)\n**error:boom\n  Failed: 0.  Skipped: 0.  Passed: 0.\nOne or more tests were cancelled.\n";
        let s = parse(out);
        assert_eq!(s.status, EunitStatus::Cancelled);
        assert_eq!(s.outcome().status, Status::Cancelled);
        assert!(s.detail.contains("boom"), "{}", s.detail);
    }
    #[test]
    fn missing_module_is_cancelled() {
        let out = "module 'nope'\nundefined\n*** test module not found ***\n**nope\n  Failed: 0.  Skipped: 0.  Passed: 0.\nOne or more tests were cancelled.\n";
        let s = parse(out);
        assert_eq!(s.status, EunitStatus::Cancelled);
    }
    #[test]
    fn unparsed_output_is_an_error() {
        let s = parse("beam_rt: unsupported: unsupported_opcode: 123\n");
        assert_eq!(s.status, EunitStatus::Unknown);
        assert_eq!(s.outcome().status, Status::Error);
    }
}
