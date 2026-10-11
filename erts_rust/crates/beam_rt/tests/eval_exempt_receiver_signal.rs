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

//! A **control signal** (`{'DOWN', …}`/`{'EXIT', …}`) aimed at the `-eval` route's own root
//! process, whose nursery has run out of room.
//!
//! # Why this is pinned here
//!
//! The `-eval` driver runs its whole expression with the process marked **relocation-exempt**
//! ([`RuntimeContext::with_relocation_exempt`]), because the evaluator keeps live terms in Rust
//! locals the process root set cannot publish, and because a message payload the evaluator has
//! bound — removed from the mailbox — points into the root's young heap. An exempt process
//! never collects and never grows (`Process::ensure_capacity` refuses), so its nursery
//! **monotonically fills** with delivered payloads and is never reclaimed.
//!
//! Both delivery paths keep working on a full nursery, each with its own fallback:
//!
//! * a **message** delivery whose payload does not fit is routed to the heap-fragment model and
//!   still arrives (`RuntimeContext::deliver_message_with_off_heap` keys on the copier's raw
//!   `AllocKind::Message`), so the mailbox keeps working;
//! * a **control signal** is built on a different path: `beam_process`'s signal application
//!   allocates the `{'DOWN', …}`/`{'EXIT', …}` message *directly on the receiver's young heap*
//!   (`beam_process/src/ptab.rs::deliver_head_signal` → `build_signal_message`), and when that
//!   build is refused for a relocation-exempt receiver, `ptab.rs::deliver_head_signal` routes it
//!   to `ptab.rs::enqueue_fragment_signal` instead of `ptab.rs::grow_receiver_for_signal`: the
//!   payload is built on a scratch arena and copied into a fresh heap fragment the enqueued
//!   message owns, so no collection runs and no heap word moves under the host's unpublished
//!   roots. The `'DOWN'` still reaches the mailbox.
//!
//! The two cases below drive the same monitor probe with a **roomy** and with a **saturated**
//! nursery; both print `DOWN ok` (exit 0), and both are cross-checked against the system `erl`
//! oracle when one is present.
//!
//! [`RuntimeContext::with_relocation_exempt`]: beam_rt::RuntimeContext

mod support;

use std::path::{Path, PathBuf};
use std::process::Output;

/// The executable under test, exactly as the other contract suites drive it.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// How many `self() ! [1]` messages the **saturated** probe sends before it monitors a dying
/// process.
///
/// Each one is copied into the root's own young heap (2 words), and the heap is never
/// reclaimed, so the nursery the default policy sizes to (233 words) is saturated long before
/// this count is reached; from then on the deliveries themselves fall back to the
/// heap-fragment model, while the heap stays full. The count is deliberately far above the
/// saturation point so the probe stays deterministic for any nursery the default policy
/// plausibly sizes to, instead of encoding "233 words" as a magic constant.
const SATURATING_PAD: usize = 500;

/// How many `self() ! [1]` messages the **control** probe sends: enough to deliver and bind a
/// few messages, nowhere near the nursery's capacity.
const ROOMY_PAD: usize = 5;

/// One finished probe: its exit code and its trimmed stdout/stderr.
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
        panic!(
            "`{}` {args:?} could not be run to completion",
            program.display()
        )
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

/// The probe expression: send `pad` single-element messages to **self** (each one copied onto
/// the `-eval` root's own young heap), then monitor a process that terminates immediately and
/// wait for the `'DOWN'` the monitor fires.
///
/// The `'DOWN'` is built on the **receiving** process's heap
/// (`beam_process/src/ptab.rs::deliver_head_signal` → `build_signal_message`), which is exactly
/// the allocation under test. `spawn(lists, reverse, [[a]])` keeps the probe free of any
/// evaluator closure.
fn probe(pad: usize) -> String {
    format!(
        "lists:foreach(fun(I)-> self() ! [1] end, lists:seq(1,{pad})), \
         P=spawn(lists,reverse,[[a]]), \
         M=erlang:monitor(process,P), \
         receive {{'DOWN',M,process,P,_}} -> io:format(\"DOWN ok~n\",[]) end, \
         halt(0)."
    )
}

/// The system oracle, resolved like the other suites resolve it (skipping loudly when the
/// sandbox has no `erl`).
fn oracle() -> Option<PathBuf> {
    support::require_oracle("eval exempt receiver signal")
}

/// Asserts `output` is the oracle's successful rendering of the probe and returns nothing:
/// exit `0`, stdout exactly `"DOWN ok"` and an empty stderr.
fn assert_down_delivered(output: &Run, what: &str) {
    assert_eq!(
        output.code,
        0,
        "{what}: the probe must exit 0, stderr — {}",
        output.err
    );
    assert_eq!(output.out, "DOWN ok", "{what}: the probe's stdout");
    assert!(
        output.err.is_empty(),
        "{what}: a successful probe is silent on stderr — {}",
        output.err
    );
}

// ---------------------------------------------------------------------------------------
// Test A — the control: a `'DOWN'` reaches the `-eval` root while its nursery has room
// ---------------------------------------------------------------------------------------

/// With the root's nursery nowhere near full, the monitor's `'DOWN'` is delivered normally and
/// the probe prints `DOWN ok`, agreeing with the system oracle.
///
/// This is the control for the saturated case below: it pins that the signal path itself works on
/// the `-eval` route, so the saturated case is about heap capacity, not a broken monitor.
#[test]
fn a_down_signal_reaches_the_eval_root_while_its_nursery_has_room() {
    let expr = probe(ROOMY_PAD);
    let output = beam_rt_eval(&expr);
    assert_down_delivered(&output, "a roomy `-eval` root");

    let Some(oracle) = oracle() else {
        return;
    };
    // The shared oracle probe **trims** its stdout, and `halt(0)` makes it succeed.
    let Some(theirs) = support::oracle_eval(&oracle, None, &expr) else {
        eprintln!("warning: the oracle could not answer the roomy probe — not compared");
        return;
    };
    assert_eq!(theirs, "DOWN ok", "the system erl's rendering");
}

// ---------------------------------------------------------------------------------------
// Test B — the same `'DOWN'` with a saturated nursery
// ---------------------------------------------------------------------------------------

/// The **same** monitor `'DOWN'`, now with the root's young heap already saturated by the
/// messenger's own deliveries, prints `DOWN ok` (exit 0) — as the system oracle does.
///
/// The refusal to build the `'DOWN'` on the receiver's young heap is a normal condition for a
/// relocation-exempt receiver: `beam_process`'s signal application routes it to the
/// heap-fragment fallback (`beam_process/src/ptab.rs::enqueue_fragment_signal`, reached from
/// `ptab.rs::deliver_head_signal`), so the message is carried by a heap fragment it owns and the
/// `-eval` root's heap is neither collected nor grown
/// (`beam_rt/src/eval/mod.rs::eval` marks the root exempt for the whole run).
#[test]
fn a_down_signal_reaches_the_eval_root_once_its_nursery_is_full() {
    let expr = probe(SATURATING_PAD);
    let output = beam_rt_eval(&expr);
    assert_down_delivered(&output, "a saturated `-eval` root");

    let Some(oracle) = oracle() else {
        return;
    };
    // The shared oracle probe **trims** its stdout, and `halt(0)` makes it succeed.
    let Some(theirs) = support::oracle_eval(&oracle, None, &expr) else {
        eprintln!("warning: the oracle could not answer the saturated probe — not compared");
        return;
    };
    assert_eq!(theirs, "DOWN ok", "the system erl's rendering");
}
