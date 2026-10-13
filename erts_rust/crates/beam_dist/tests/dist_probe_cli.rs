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

/// Asserts that `dist_probe loopback <report>` prints exactly `expected` on stdout
/// (one line, a single trailing newline), nothing on stderr, and exits `0`.
fn assert_report(report: &str, expected: &str) {
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

/// `badcookie` — the connector presents the wrong cookie, so the acceptor refuses
/// and both sides observe the refusal (`badarg`); the recorded traffic shows the
/// connector's `Name` + `ChallengeReply` (`Nr`) and the acceptor's `Status` +
/// `Challenge` (`sN`) with **no** `ChallengeAck`.
#[test]
fn badcookie_reports_the_refused_handshake_shape() {
    assert_report(
        "badcookie",
        "badcookie server=badarg client=badarg sent=Nr received=sN",
    );
}

/// `nodeid` — the negotiated capability word plus node A's view of its peer.
#[test]
fn nodeid_reports_the_negotiated_flags_and_peer_identity() {
    assert_report(
        "nodeid",
        "nodeid flags=6D07DF7FBC peer=beta@localhost creation=2 visibility=hidden",
    );
}

/// `handshake` — a byte-exact transcript of the `'N'` handshake, both directions
/// (challenges pinned, so the digests are constant across runs).
#[test]
fn handshake_reports_the_pinned_byte_transcript() {
    assert_report(
        "handshake",
        "handshake flags=6D07DF7FBC \
         client=001D4E0000006D07DF7FBC00000002000E62657461406C6F63616C686F7374\
         001572600DF00D4EA14A42245C55671C6C25D9BC6C77F9 \
         server=0003736F6B00224E0000006D07DF7FBC0BADC0DE00000001000F616C706861406C6F63616C686F7374\
         001161725ACE098AC503DC07BEB5941CD244DE",
    );
}

/// `frag` — a >[`DIST_FRAGMENT_SIZE`] payload round-trips and node A really wrote
/// 4 `'E'`/`'F'` fragment frames (1 `'E'` + 3 `'F'`).
#[test]
fn frag_reports_the_round_tripped_payload_and_frame_count() {
    assert_report("frag", "frag bytes=200000 sum=DC029F2D frames=4");
}

/// `alias` — a `DOP_ALIAS_SEND` control round trip, decoded on the peer.
#[test]
fn alias_reports_the_decoded_alias_send() {
    assert_report("alias", "alias from=1001 to=3003 msg=42");
}

/// `monitor` — the `DOP_MONITOR_P` / `DOP_LINK` / `DOP_EXIT` control round trips.
#[test]
fn monitor_reports_the_decoded_monitor_link_and_exit() {
    assert_report(
        "monitor",
        "monitor watcher=1001 watched=5005 ref=9001 \
         link from=1001 to=5006 exit from=1001 to=5007 reason=3003",
    );
}

/// `regsend_missing` — a `DOP_REG_SEND` to an unregistered name is locally a
/// no-recipient drop (ground truth `dist.c`: the C silently discards it).
#[test]
fn regsend_missing_reports_the_no_recipient_drop() {
    assert_report("regsend_missing", "regsend target=missing outcome=nosuchprocess");
}

/// `utf8` — a non-Latin-1 atom crosses the wire and comes back as the
/// `SMALL_ATOM_UTF8_EXT` ETF image (`83 77 02 CE A9`).
#[test]
fn utf8_reports_the_non_latin1_atom_image() {
    assert_report("utf8", "837702CEA9");
}

/// Every report is host-independent: two runs print byte-identical stdout.
#[test]
fn every_report_is_deterministic_across_runs() {
    for report in [
        "ping",
        "nodes",
        "term",
        "badcookie",
        "nodeid",
        "frag",
        "alias",
        "monitor",
        "regsend_missing",
        "handshake",
        "utf8",
    ] {
        let first = dist_probe(&["loopback", report]);
        let second = dist_probe(&["loopback", report]);
        assert_eq!(
            first.status.code(),
            Some(0),
            "`loopback {report}` exits 0 (stderr: {})",
            String::from_utf8_lossy(&first.stderr),
        );
        assert_eq!(
            first.stdout, second.stdout,
            "`loopback {report}` prints identical bytes on repeated runs",
        );
        assert!(
            !first.stdout.is_empty(),
            "`loopback {report}` prints a line",
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
