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

//! The **CLI contract** of the `dist_probe` helper (`crates/beam_dist/src/bin/dist_probe.rs`),
//! pinned from the differential node — the half of the contract the corpus cannot express.
//!
//! A [`CaseKind::Command`](differential::corpus::CaseKind::Command) corpus case must have a
//! **non-empty** golden stdout and a **zero** golden exit code (its oracle side has to
//! *observe* the golden, and the corpus validator rejects a non-zero command golden), so the
//! helper's **failure** behaviour — exit `2` with a usage line on stderr and no stdout — has
//! no corpus representation. This test drives the helper **directly** instead:
//!
//! * every `loopback <report>` prints exactly its expected one-line stdout (a single trailing
//!   `\n`), writes nothing to stderr and exits `0` — the same golden bytes the
//!   [`dist`](differential::corpus) corpus group compares against the system `erl`;
//! * a missing / unknown / over-long argv exits `2` with a usage line on stderr and empty
//!   stdout.
//!
//! The helper is resolved exactly as the harness resolves it — `$BEAM_DIST_PROBE` (which must
//! name an existing file, else a loud error) else a built `dist_probe` — and the test **skips
//! loudly** (prints a reason, never fails) when no helper is built, so it is honest about not
//! having run rather than fabricating a pass.
//!
//! ## Exit `3` is not reachable through argv (and so is not asserted)
//!
//! The helper also has a capability-gap exit (`dist_probe: unsupported: <reason>` on stderr,
//! exit `3`) that the harness classifies as `unsupported`, never a mismatch. It is emitted
//! only when a report's loopback genuinely fails to produce its observable — `main` validates
//! the argv **first**, so every argv either selects one of the eleven valid reports or is a
//! usage error (exit `2`). No argv can force the exit-`3` path, so there is nothing to select
//! it and nothing to assert; pinning it would require injecting a loopback failure, which the
//! helper exposes no seam for.
//!
//! This test is a **read-only observer**: it never mutates the helper or the runtime.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;

/// The eleven reports, each with its exact expected stdout (one line, no trailing newline).
///
/// The `ping` / `nodes` / `term` / `utf8` goldens are the ones the `dist/` corpus group
/// compares against the system `erl`; the remaining seven encode helper-internal detail the
/// oracle cannot observe, so they are pinned here against the helper alone (see the `dist`
/// group's module doc for why they are not corpus cases).
const REPORTS: [(&str, &str); 11] = [
    ("ping", "pong"),
    ("nodes", "1"),
    ("term", "836803610161026103"),
    (
        "nodeid",
        "nodeid flags=6D07DF7FBC peer=beta@localhost creation=2 visibility=hidden",
    ),
    (
        "handshake",
        "handshake flags=6D07DF7FBC \
         client=001D4E0000006D07DF7FBC00000002000E62657461406C6F63616C686F7374\
         001572600DF00D4EA14A42245C55671C6C25D9BC6C77F9 \
         server=0003736F6B00224E0000006D07DF7FBC0BADC0DE00000001000F616C706861406C6F63616C686F7374\
         001161725ACE098AC503DC07BEB5941CD244DE",
    ),
    ("badcookie", "badcookie server=badarg client=badarg sent=Nr received=sN"),
    ("frag", "frag bytes=200000 sum=DC029F2D frames=4"),
    ("alias", "alias from=1001 to=3003 msg=42"),
    (
        "monitor",
        "monitor watcher=1001 watched=5005 ref=9001 \
         link from=1001 to=5006 exit from=1001 to=5007 reason=3003",
    ),
    ("regsend_missing", "regsend target=missing outcome=nosuchprocess"),
    ("utf8", "837702CEA9"),
];

/// Resolve the `dist_probe` helper the way the harness does, or `None` when none is built.
///
/// `$BEAM_DIST_PROBE` wins and must name an existing file (a loud error otherwise) — exactly
/// [`differential::runtime::resolve_helper`]. Otherwise the fresh debug sibling cargo just
/// built next to this test binary is preferred (so the test never pins a stale
/// `target/release` artifact — the same rationale `common/mod.rs` documents for `$BEAM_RT`),
/// with the harness's workspace default (`target/{release,debug}/dist_probe`, release first)
/// as the final fallback.
fn resolve_helper() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("BEAM_DIST_PROBE") {
        let program = PathBuf::from(value);
        assert!(
            program.is_file(),
            "$BEAM_DIST_PROBE is set to '{}', which is not an existing file",
            program.display()
        );
        return Some(program);
    }
    if let Some(fresh) = common::fresh_helper() {
        return Some(fresh);
    }
    match differential::runtime::resolve_helper() {
        Ok(spec) => spec.map(|spec| spec.program),
        Err(error) => panic!("the dist_probe helper resolution failed: {error}"),
    }
}

/// The helper for a test, or `None` with a loud skip when none is built.
fn helper_or_skip(test: &str) -> Option<PathBuf> {
    match resolve_helper() {
        Some(program) => Some(program),
        None => {
            eprintln!(
                "SKIP: {test}: no dist_probe helper found ($BEAM_DIST_PROBE unset and no built \
                 dist_probe under the workspace target/{{release,debug}}); build it with \
                 `cargo build -p beam_dist --bin dist_probe`"
            );
            None
        }
    }
}

/// Run the helper with `args`.
fn run_helper(program: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("spawn {}: {error}", program.display()))
}

/// Every report prints exactly its expected one-line stdout, nothing on stderr, and exits `0`.
#[test]
fn each_report_prints_exactly_its_expected_line_and_exits_zero() {
    let Some(program) = helper_or_skip("each_report") else {
        return;
    };
    for (report, expected) in REPORTS {
        let output = run_helper(&program, &["loopback", report]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "`loopback {report}` exits 0 (stderr: {})",
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8"),
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

/// A missing / unknown / over-long argv exits `2` with a usage line on stderr and no stdout.
#[test]
fn a_missing_or_unknown_argv_exits_two_with_a_usage_line() {
    let Some(program) = helper_or_skip("a_missing_or_unknown_argv") else {
        return;
    };
    for args in [
        Vec::new(),
        vec!["loopback"],
        vec!["loopback", "bogus"],
        vec!["bogus", "ping"],
        vec!["loopback", "ping", "extra"],
    ] {
        let output = run_helper(&program, &args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "`{args:?}` is a usage error (stderr: {})",
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            output.stdout.is_empty(),
            "`{args:?}` prints no stdout on a usage error, got: {}",
            String::from_utf8_lossy(&output.stdout),
        );
        assert!(
            !output.stderr.is_empty(),
            "`{args:?}` prints a usage line on stderr",
        );
    }
}

/// A set-but-unusable `$BEAM_DIST_PROBE` is a loud error, never a silent skip: the resolution
/// validation the harness relies on is pinned here too.
#[test]
fn a_set_but_missing_helper_override_is_a_loud_error() {
    // The assertion lives in `resolve_helper`, which panics on a non-file override. Drive it
    // through `differential::runtime::resolve_helper_from` so the check is exercised without
    // mutating this process's environment (which is shared across test threads).
    let error = differential::runtime::resolve_helper_from(
        Some(std::ffi::OsString::from("/nonexistent/dist_probe")),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
    .expect_err("a non-file $BEAM_DIST_PROBE must be a loud error");
    let message = error.to_string();
    assert!(
        message.contains("/nonexistent/dist_probe"),
        "the error must name the offending path: {message}"
    );
}
