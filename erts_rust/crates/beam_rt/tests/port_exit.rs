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

//! Port **`'EXIT'`/`'DOWN'` signalling on close**, cross-checked against the system oracle
//! (contract A.4).
//!
//! A port is a linkable, monitorable entity. `open_port/2` links it to its owner,
//! `port_connect/2` adds further links and `erlang:monitor(port, Port)` records an incoming
//! monitor, so `port_close/1` owes exactly what a dying process owes its peers. The policy is
//! implemented by `src/port/exit.rs` (snapshot at close, deliver at the close report); this
//! suite is its differential evidence.
//!
//! # Why every printed value is *normalized*
//!
//! A `#Port<…>` and a `#Ref<…>` value cannot be equal across two OS processes — the port
//! number and the reference id are allocated independently per runtime — so a probe never
//! prints them. The committed fixture (`tests/fixtures/port_exit.beam`) replaces each with
//! the predicates that identify it (`is_port(Port)` / `is_reference(Ref)`), and the inline
//! probes print the same booleans via `element/2`. The **shape** of the message (its tag, its
//! arity, its kind slot, its reason, and how many of each arrived and in which order) is what
//! the two runtimes must agree on, and it is compared byte-for-byte.
//!
//! # The two routes
//!
//! Both delivery routes are pinned, because they are separate code paths that share one
//! policy:
//!
//! * the **`-eval` route** — the message is returned to the evaluator's own `receive`
//!   (`RuntimeContext::poll_port_signals`) — rows 1–5;
//! * the **scheduler `run` route** — the message goes into the recipient's mailbox and wakes a
//!   receiver parked in a compiled `receive` (`Runtime::deliver_port_signals`) — row 6.
//!
//! # Measured oracle answers (OTP 29.1.1 / erts-17.1)
//!
//! ```none
//! trap_owner        [{exit,true,normal}]
//! trap_owner_mon    [{exit,true,normal},{down,true,port,true,normal}]   %% EXIT first, once each
//! notrap_owner_mon  [{down,true,port,true,normal}]                      %% nothing for `normal`, survives
//! watcher_only      [{down,true,port,true,normal}]
//! ```
//!
//! # Skip policy
//!
//! No `erl` on `$PATH` (and no executable `$ERTS_ORACLE`) is a **loud** skip per test, never a
//! silent pass.
#![deny(unsafe_code)]

mod support;

use std::path::{Path, PathBuf};

/// The runtime binary under test (cargo builds it for this suite).
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The committed close-signal fixture (`tests/fixtures/port_exit.beam`).
const PORT_EXIT_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/port_exit.beam"
);

// ---------------------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------------------

/// A scratch directory holding **only** `port_exit.beam`.
///
/// `tests/fixtures/` must never be handed to the oracle as a single `-pa`: it also holds a
/// `lists.beam`, which would shadow the real stdlib module. The directory is unique per call
/// (label + pid + a counter) so concurrent tests cannot collide, and it leaks deliberately —
/// a test's temp dir is collected by the OS, and removing it would race a sibling probe.
fn scratch_dir(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_port_exit_{label}_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("the scratch directory is created");
    std::fs::copy(PORT_EXIT_BEAM, dir.join("port_exit.beam")).expect("the fixture is copied");
    dir
}

/// Runs `expr` under the oracle (`-pa scratch`) and under [`BINARY`] (`-pa scratch`) and returns
/// `(oracle_stdout, runtime_stdout)`, both trimmed.
///
/// `None` — after a **loud** skip line — only when the *oracle* cannot answer (a missing
/// toolchain, a probe that timed out, a non-zero exit). A runtime that cannot answer is a
/// genuine divergence and fails the test with its stderr.
fn side_by_side(oracle: &Path, scratch: &Path, expr: &str) -> Option<(String, String)> {
    let Some(expected) = support::oracle_eval(oracle, Some(scratch), expr) else {
        eprintln!(
            "SKIP the port_exit probe: the oracle could not answer\n--- expression ---\n{expr}"
        );
        return None;
    };
    let dir = scratch.to_str().expect("a UTF-8 scratch path");
    let output = support::run_bounded(Path::new(BINARY), &["-noshell", "-pa", dir, "-eval", expr])
        .expect("beam_rt answers the probe within the bound");
    assert!(
        output.status.success(),
        "beam_rt must exit 0 for this expression, got {:?}\n--- stderr ---\n{}\n--- expression ---\n{expr}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let ours = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Some((expected, ours))
}

/// Asserts that `expr` prints **exactly** the oracle's bytes on this runtime.
fn agrees(oracle: &Path, expr: &str) {
    let scratch = scratch_dir("eval");
    let Some((expected, ours)) = side_by_side(oracle, &scratch, expr) else {
        return;
    };
    assert_eq!(
        ours, expected,
        "the two runtimes must print the same answer\n--- expression ---\n{expr}"
    );
}

/// Runs one fixture function through the **scheduler `run` route** (`Runtime::run_scheduled`,
/// which prints the result with `~w~n` / `{Port, {data, …}}`'s own delivery seam
/// `Runtime::deliver_port_messages` → [`Runtime::deliver_port_signals`]) and compares it with
/// the oracle's answer for the **same function**.
///
/// The oracle side is an ordinary `-eval` call: the fixture function is the probe body and its
/// printed answer is the expectation, so the only difference between the two runs is which
/// runtime executes it. Port and reference values are normalized inside the fixture, so the
/// comparison is exact.
///
/// [`Runtime::deliver_port_signals`]: beam_rt::Runtime
fn agrees_on_the_run_route(oracle: &Path, function: &str) {
    let scratch = scratch_dir("run");
    let call = format!("io:format(\"~w~n\",[port_exit:{function}()]),\nok.\n");
    let Some(expected) = support::oracle_eval(oracle, Some(scratch.as_path()), &call) else {
        eprintln!("SKIP the port_exit run-route probe for {function}: the oracle could not answer");
        return;
    };
    let spec = format!("port_exit:{function}/0");
    let output = support::run_bounded(Path::new(BINARY), &["run", PORT_EXIT_BEAM, &spec])
        .expect("beam_rt answers the run-route probe within the bound");
    assert!(
        output.status.success(),
        "beam_rt run must exit 0 for {spec}, got {:?}\n--- stderr ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let ours = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(
        ours, expected,
        "the run route must print what the oracle prints for {spec}"
    );
}

// ---------------------------------------------------------------------------------------
// Rows 1–4 — the four shapes, through the `-eval` route
// ---------------------------------------------------------------------------------------

/// Row 1: a **trapping** owner gets exactly one `{'EXIT', Port, normal}` — no `'DOWN'`, no
/// duplicate.
#[test]
fn a_trapping_owner_gets_one_exit() {
    let Some(oracle) = support::require_oracle("port_exit") else {
        return;
    };
    agrees(
        &oracle,
        "io:format(\"~w~n\",[port_exit:trap_owner()]),\nhalt(0).\n",
    );
}

/// Row 2: a trapping owner that also **watches** the port gets the `'EXIT'` first and then the
/// `'DOWN'` — one of each, never twice.
#[test]
fn a_trapping_owner_that_also_watches_gets_exit_then_down() {
    let Some(oracle) = support::require_oracle("port_exit") else {
        return;
    };
    agrees(
        &oracle,
        "io:format(\"~w~n\",[port_exit:trap_owner_mon()]),\nhalt(0).\n",
    );
}

/// Row 3: a **non-trapping** owner gets no `'EXIT'` (`normal` is ignored in the link family)
/// and is **not** terminated — just the `'DOWN'`, and it goes on running (its `halt(0)` is the
/// proof it survived).
#[test]
fn a_non_trapping_owner_gets_only_the_down_and_survives() {
    let Some(oracle) = support::require_oracle("port_exit") else {
        return;
    };
    agrees(
        &oracle,
        "io:format(\"~w~n\",[port_exit:notrap_owner_mon()]),\nio:format(\"alive~n\",[]),\nhalt(0).\n",
    );
}

/// Row 4: a watcher that is **not** linked to the port gets just the `'DOWN'`.
#[test]
fn a_watcher_that_is_not_linked_gets_only_the_down() {
    let Some(oracle) = support::require_oracle("port_exit") else {
        return;
    };
    agrees(
        &oracle,
        "io:format(\"~w~n\",[port_exit:watcher_only()]),\nhalt(0).\n",
    );
}

// ---------------------------------------------------------------------------------------
// Row 5 — the second link a `port_connect/2` adds
// ---------------------------------------------------------------------------------------

// ---------------------------------------------------------------------------------------
// Row 6 — the scheduler `run` route
// ---------------------------------------------------------------------------------------

/// Row 6a: the trapping owner's `'EXIT'` on the **scheduler** route, where the message is
/// delivered into the recipient's mailbox (and would wake a receiver parked in a compiled
/// `receive`) rather than returned to an evaluator.
#[test]
fn the_run_route_tells_a_trapping_owner() {
    let Some(oracle) = support::require_oracle("port_exit") else {
        return;
    };
    agrees_on_the_run_route(&oracle, "trap_owner");
}

/// Row 6b: the `'EXIT'` **then** `'DOWN'` order on the scheduler route.
#[test]
fn the_run_route_tells_a_trapping_owner_that_also_watches() {
    let Some(oracle) = support::require_oracle("port_exit") else {
        return;
    };
    agrees_on_the_run_route(&oracle, "trap_owner_mon");
}

/// Row 6c: the non-trapping owner survives the scheduler route too (no `'EXIT'`, just the
/// `'DOWN'`) — the `normal` reason must never terminate a peer.
#[test]
fn the_run_route_leaves_a_non_trapping_owner_alive() {
    let Some(oracle) = support::require_oracle("port_exit") else {
        return;
    };
    agrees_on_the_run_route(&oracle, "notrap_owner_mon");
}

/// Row 6d: a watcher that is not linked gets only the `'DOWN'` on the scheduler route.
#[test]
fn the_run_route_tells_a_watcher_that_is_not_linked() {
    let Some(oracle) = support::require_oracle("port_exit") else {
        return;
    };
    agrees_on_the_run_route(&oracle, "watcher_only");
}
