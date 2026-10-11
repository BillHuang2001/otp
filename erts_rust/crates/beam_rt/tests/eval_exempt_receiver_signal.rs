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
//! ([`RuntimeContext::with_relocation_exempt`]): the evaluator keeps live terms in Rust locals
//! the process root set cannot publish, and a message payload it has bound — removed from the
//! mailbox — points into the root's young heap. An exempt process never collects
//! (`Process::collect`/`Process::ensure_capacity` refuse it), so its nursery monotonically fills
//! with delivered payloads and is never reclaimed.
//!
//! That must not break message delivery. A delivery whose payload does not fit is routed to the
//! heap-fragment model and still arrives (`RuntimeContext::deliver_message_with_off_heap` keys on
//! the copier's raw `AllocKind::Message`). A **control signal** is built on its own path —
//! `beam_process`'s signal builders allocate the `{'DOWN', …}`/`{'EXIT', …}` message on the
//! receiver's young heap — and routes a **relocation-exempt** receiver to the same heap-fragment
//! model instead (`beam_process/src/ptab.rs::build_signal_message` → `enqueue_fragment_signal`,
//! the local analogue of the C's `erts_alloc_message(hsz,&hp)`), so the message simply does not
//! live on the exempt heap and the signal still arrives.
//!
//! The two probes below pin both halves: the `'DOWN'` reaches the `-eval` root while its nursery
//! has room, and again — still `DOWN ok`, exit 0 — once the messenger's own deliveries have
//! saturated that nursery. The roomy probe is also cross-checked against the system oracle.
//!
//! [`RuntimeContext::with_relocation_exempt`]: beam_rt::RuntimeContext

mod support;

use std::path::{Path, PathBuf};
use std::process::Output;

/// The executable under test, exactly as the other contract suites drive it.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// How many `self() ! [1]` messages the **failing** probe sends before it monitors a dying
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
/// (`beam_process/src/ptab.rs::build_and_enqueue_down`), which is exactly the allocation under
/// test. `spawn(lists, reverse, [[a]])` keeps the probe free of any evaluator closure.
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
/// This is the control for the ignored case below: it pins that the signal path itself works on
/// the `-eval` route, so the failure there is heap capacity, not a broken monitor.
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
/// messenger's own deliveries.
///
/// The exempt nursery never reclaims, so the deliveries themselves fall back to the
/// heap-fragment model, and the monitor's `'DOWN'` — built by `beam_process`'s signal
/// application — is routed to that same fragment model for the exempt receiver
/// (`beam_process/src/ptab.rs::build_signal_message` → `enqueue_fragment_signal`). The probe
/// therefore prints `DOWN ok` (exit 0), exactly as the system oracle does, with no collection
/// and no heap relocation.
#[test]
fn a_down_signal_reaches_the_eval_root_once_its_nursery_is_full() {
    let expr = probe(SATURATING_PAD);
    let output = beam_rt_eval(&expr);
    assert_down_delivered(&output, "a saturated `-eval` root");
}
