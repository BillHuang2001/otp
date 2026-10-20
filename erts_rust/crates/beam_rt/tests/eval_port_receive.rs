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

//! A port message is a **mailbox message**, so a `-eval` `receive … after T` is **selective**
//! over it: a port frame that matches no clause stays queued for a later `receive`, and the
//! frames drained in the same port round as a matching one are **not** discarded with it.
//!
//! # Why this is pinned here
//!
//! The `-eval` route acquires port messages itself (the scheduler is only pumped without
//! blocking on that route, so the pass's own port half delivers nothing there). An earlier
//! version tried each drained frame against the current clauses **inline**, which lost two
//! classes of message: a frame no clause matched was consumed by the matcher and never queued,
//! and every frame drained *after* the one that matched went away with the `return`. Both are
//! silent wrong answers — the oracle's stdout carries the message, and this runtime's carried
//! its own `after` atom instead. The fix delivers each drained frame into the owning process's
//! mailbox through the runtime's one delivery path, so the loop head's mailbox scan does the
//! matching and everything it does not take stays where the next `receive` will find it.
//!
//! The two shapes below are the corpus cases `driver/line_full_line_noeol_flush` and the
//! selective half of the same port family, driven through the CLI exactly as the differential
//! harness drives them (`-noshell -eval <expr>`), so a regression is caught with no harness and
//! no `erl`: every expectation is a **golden** this file pins, and the system `erl` is asked to
//! confirm the same bytes whenever it is installed ([`support::require_oracle`], a loud skip
//! otherwise).
mod support;

use std::path::Path;
use std::process::Output;

/// The executable under test, exactly as the other contract suites drive it.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The `erl`-facing banner for the loud skip.
const BANNER: &str = "eval_port_receive";

/// The six newline-free bytes `printf` writes, framed by `{line,4}` into a full 4-byte frame
/// and a 2-byte tail — the child every case below opens.
const CHILD: &str = r#"printf abcdef"#;

/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    /// The child's exit code (`-1` when it was signalled).
    code: i32,
    /// Trimmed stdout.
    out: String,
    /// Trimmed stderr.
    err: String,
}

/// Runs `program args` under the shared bounded runner and fails loudly when it could not run.
fn run(program: &Path, args: &[&str]) -> Run {
    let output: Output = support::run_bounded(program, args).unwrap_or_else(|| {
        panic!("`{}` {args:?} could not be run to completion", program.display())
    });
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// `beam_rt -noshell -eval <expr>` through the built binary.
fn beam_rt_eval(expr: &str) -> Run {
    run(Path::new(BINARY), &["-noshell", "-eval", expr])
}

/// The pinned cases: `(label, expression, the exact stdout both runtimes print)`.
///
/// Both probes end with `io:format` + `ok` because `-eval` itself prints nothing on success, so
/// the printed term **is** the whole observable answer. Each `receive` is bounded by a 2000 ms
/// `after` arm that answers a distinct atom, so a lost frame is reported as that atom rather
/// than as a timeout of the suite.
fn cases() -> [(&'static str, String, &'static str); 2] {
    [
        (
            "two consecutive receives on one port see both frames",
            format!(
                r#"begin P = open_port({{spawn,"{CHILD}"}},[binary,{{line,4}}]),
                          A = receive {{P,{{data,Da}}}} -> Da after 2000 -> nf1 end,
                          B = receive {{P,{{data,Db}}}} -> Db after 2000 -> nf2 end,
                          io:format("~w", [{{A,B}}]), ok end"#
            ),
            "{{noeol,<<97,98,99,100>>},{noeol,<<101,102>>}}",
        ),
        (
            "a frame no clause matches is still there for the next receive",
            format!(
                r#"begin P = open_port({{spawn,"{CHILD}"}},[binary,{{line,4}}]),
                          A = receive {{P,{{data,{{eol,L}}}}}} -> {{eol,L}} after 300 -> nomatch end,
                          B = receive {{P,{{data,{{noeol,M}}}}}} -> {{noeol,M}} after 2000 -> nf end,
                          io:format("~w", [{{A,B}}]), ok end"#
            ),
            "{nomatch,{noeol,<<97,98,99,100>>}}",
        ),
    ]
}

/// The built binary must keep both frames of a port round reachable: the first `receive` takes
/// the frame it matched, and the frames queued with it stay in the mailbox.
#[test]
fn a_port_frame_matched_by_the_first_receive_does_not_swallow_the_others() {
    for (label, expr, expected) in cases() {
        let ours = beam_rt_eval(&expr);
        assert_eq!(
            ours.code, 0,
            "{label}: `beam_rt -eval {expr:?}` must exit 0, stderr — {}",
            ours.err
        );
        assert_eq!(ours.out, expected, "{label}: beam_rt's answer");
        assert!(
            ours.err.is_empty(),
            "{label}: a successful probe is silent on stderr — {}",
            ours.err
        );
    }
}

/// The system `erl` runs the **same** probes and must print the **same** bytes, so the pinned
/// answers are `erl`'s own selective-receive behaviour and not this runtime's habit.
///
/// A missing `erl` is a loud skip ([`support::skip_loudly`]); a probe the oracle cannot answer is
/// reported, never silently treated as agreement.
#[test]
fn the_system_oracle_keeps_the_same_frames_in_the_mailbox() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    for (label, expr, expected) in cases() {
        let Some(theirs) = support::oracle_eval(&oracle, None, &expr) else {
            eprintln!("warning: the oracle could not answer {label} — not compared");
            continue;
        };
        assert_eq!(
            theirs, expected,
            "{label}: the system erl must print the recorded answer"
        );
        let ours = beam_rt_eval(&expr);
        assert_eq!(ours.code, 0, "{label}: beam_rt exited {}: {}", ours.code, ours.err);
        assert_eq!(ours.out, theirs, "{label}: the two runtimes must print the same bytes");
    }
}
