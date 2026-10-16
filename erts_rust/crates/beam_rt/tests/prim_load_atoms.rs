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

//! `erlang:list_to_existing_atom/1` at the **executable** boundary: the **boot-time loaded
//! module set** (contract A.9) decides which atoms exist, cross-checked with the live system
//! `erl` oracle.
//!
//! `bootstrap/bin/start.boot` has three `{primLoad, …}` batches: one of 29 names **before** the
//! `{kernel_load_completed}` marker and two more (86 + 87 names) after it. `init` seeds
//! `#es{prim_load=true}` and resets it to `(Mode == embedded)` on that marker
//! (`erts/preloaded/src/init.erl:1206-1216,1337-1348`), so outside `embedded` mode — the
//! default — only the **first** batch is loaded eagerly; the code server loads the rest on
//! demand. Because loading a module interns its whole `Atoms` chunk into the node's single
//! shared atom table, that gate decides whether a post-marker batch's atoms **exist**:
//!
//! * `hello` rides `auth.beam` (a post-marker kernel batch name),
//! * `world` rides `net_adm.beam` (likewise),
//! * `error_handler` is in the pre-marker batch and is loaded by every boot.
//!
//! So a real `init` raises `badarg` for the first two and answers for the third. This suite
//! pins the same three answers here, measured through the built binary and through the oracle.
//!
//! The comparison is the exit code plus the **trimmed** stdout, exactly as the other suites in
//! this crate compare them: the oracle's stderr is its own crash report (`Runtime terminating
//! during boot (…)`), so it is not byte-comparable. Our own stderr *is* asserted to name
//! `badarg`, so a future change that reports some other reason fails here rather than passing
//! on the exit code alone.

// The shared harness is compiled in whole; this suite uses only its toolchain discovery, its
// bounded runner and its loud-skip helper, so the unused remainder is expected here — the same
// shape `tests/module_loaded.rs` uses.
#[allow(dead_code)]
mod support;

use std::path::Path;
use std::process::Output;

/// A probe's observable result: the exit code plus the **trimmed** stdout and stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Runs `program args` through the shared bounded runner, failing loudly when the probe could
/// not be run to completion at all.
fn probe(program: &Path, args: &[&str]) -> Run {
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

/// `beam_rt -noshell -eval <expr>` through the built binary. The action exits on its own.
fn beam_rt(expr: &str) -> Run {
    probe(
        Path::new(env!("CARGO_BIN_EXE_beam_rt")),
        &["-noshell", "-eval", expr],
    )
}

/// The same expression under the oracle, terminated by `-s init stop` (a bare
/// `erl -noshell` never exits on its own).
fn oracle(oracle: &Path, expr: &str) -> Run {
    probe(
        oracle,
        &["-noshell", "-eval", expr, "-s", "init", "stop"],
    )
}

// ---------------------------------------------------------------------------------------
// The programs both runtimes are given
// ---------------------------------------------------------------------------------------

/// `hello` rides `auth.beam`, a **post**-`{kernel_load_completed}` `{primLoad, …}` batch name.
const EXPR_HELLO: &str = "erlang:list_to_existing_atom(\"hello\").";

/// `world` rides `net_adm.beam`, likewise a post-marker batch name.
const EXPR_WORLD: &str = "erlang:list_to_existing_atom(\"world\").";

/// `error_handler` is in the batch **before** the marker, which every boot loads.
const EXPR_ERROR_HANDLER: &str = "erlang:list_to_existing_atom(\"error_handler\").";

/// The `-eval` route, under both runtimes, with no toolchain required.
#[test]
fn the_post_marker_batch_atoms_do_not_exist_here_either() {
    // The pre-marker batch's atom exists: the call succeeds, prints nothing and exits 0.
    let preloaded = beam_rt(EXPR_ERROR_HANDLER);
    assert_eq!(
        preloaded.code, 0,
        "`error_handler` is in the pre-marker batch: stderr {}",
        preloaded.err
    );
    assert_eq!(preloaded.out, "", "the call prints nothing");

    // The two post-marker atoms do not: `badarg`, exit 1, no stdout.
    for (label, expr) in [("hello", EXPR_HELLO), ("world", EXPR_WORLD)] {
        let failed = beam_rt(expr);
        assert_eq!(
            failed.code, 1,
            "`erlang:list_to_existing_atom(\"{label}\")` must be a badarg runtime error: \
             stdout [{}] stderr [{}]",
            failed.out, failed.err
        );
        assert_eq!(failed.out, "", "the failing call prints nothing");
        assert!(
            failed.err.contains("badarg"),
            "the reported reason must be `badarg`: {}",
            failed.err
        );
        assert!(
            !failed.err.contains("unsupported"),
            "a badarg is not a capability gap: {}",
            failed.err
        );
    }
}

/// The same three answers under the live oracle: the recorded exit codes must be what the
/// system `erl` really produces, not what this suite hopes for.
#[test]
fn the_oracle_agrees_on_all_three() {
    let Some(oracle_path) = support::require_oracle("prim_load_atoms") else {
        return;
    };

    let preloaded = oracle(&oracle_path, EXPR_ERROR_HANDLER);
    assert_eq!(
        preloaded.code, 0,
        "the oracle must resolve `error_handler`: stderr {}",
        preloaded.err
    );
    assert_eq!(preloaded.out, "", "the oracle's call prints nothing");

    for (label, expr) in [("hello", EXPR_HELLO), ("world", EXPR_WORLD)] {
        let failed = oracle(&oracle_path, expr);
        assert_eq!(
            failed.code, 1,
            "the oracle must fail on `{label}`: stdout [{}] stderr [{}]",
            failed.out, failed.err
        );
        assert_eq!(failed.out, "", "the oracle's failing call prints nothing");
    }
}

/// The cross-check proper: both runtimes answer the same exit code and the same (empty)
/// stdout for all three atoms, so the loaded-set fidelity is measured, not assumed.
#[test]
fn both_runtimes_report_the_same_exit_codes() {
    let Some(oracle_path) = support::require_oracle("prim_load_atoms") else {
        return;
    };
    for (label, expr) in [
        ("error_handler", EXPR_ERROR_HANDLER),
        ("hello", EXPR_HELLO),
        ("world", EXPR_WORLD),
    ] {
        let ours = beam_rt(expr);
        let theirs = oracle(&oracle_path, expr);
        assert_eq!(
            ours.code, theirs.code,
            "`{label}`: this runtime exits {} but the oracle exits {} \
             (beam_rt stderr [{}], oracle stderr [{}])",
            ours.code, theirs.code, ours.err, theirs.err
        );
        assert_eq!(
            ours.out, theirs.out,
            "`{label}`: this runtime printed [{}] but the oracle printed [{}]",
            ours.out, theirs.out
        );
    }
}

/// A missing toolchain is a **loud** skip in the two oracle-cross-checked tests above; this
/// test makes the skip observable even when it happens, so a silently-passing suite without
/// `erl` is impossible.
#[test]
fn a_missing_oracle_is_reported_loudly() {
    if support::require_oracle("prim_load_atoms").is_none() {
        support::skip_loudly(
            "prim_load_atoms",
            "no `erl` on $PATH; the two oracle-cross-checked probes were skipped",
        );
    }
}
