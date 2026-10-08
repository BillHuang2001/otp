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

//! Regression: an **allocation-heavy loop must complete**, and a workload that exceeds the
//! configured term-heap limit must raise the emulator's documented `error:system_limit` —
//! never the host-level `beam_rt: allocation failure: heap (N bytes)` diagnostic.
//!
//! Background (the bug this pins): Milestone 1 has no collector and no heap-growth path
//! (growth-by-collection is Stage 4), so the term-construction heaps are plain bump arenas
//! whose *capacity* is the memory limit. The default was 64 Ki words (512 KiB), which a
//! realistic compute loop — the benchmark's `lists:sum(lists:seq(1, 1_000_000))`, 2 Mi
//! words of cons cells — overran, aborting the runtime with the internal allocator
//! diagnostic and producing no benchmark metric. The fix raises the default
//! ([`beam_rt::DEFAULT_HEAP_WORDS`], lazily mapped so the address space costs no RSS) and
//! translates heap exhaustion into the emulator's own `system_limit`.
//!
//! These tests drive the **executable** (`CARGO_BIN_EXE_beam_rt`), i.e. the whole
//! CLI → runtime → BIF → heap path, so they pin the observable contract (stdout, stderr,
//! exit code) rather than an internal API.

use std::process::Command;

/// Runs the built `beam_rt` binary with `args` and returns its captured output.
fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args(args)
        .output()
        .expect("spawn the beam_rt executable")
}

/// The benchmark's compute loop (`tools/bench`'s `baseline` `COMPUTE_EVAL`, minus the
/// `erlang:memory(total)` half) must run to completion and print the right answer.
#[test]
fn the_allocation_heavy_compute_loop_completes() {
    let out = run(&[
        "-noshell",
        "-eval",
        r#"S = lists:sum(lists:seq(1,1000000)), io:format("~p~n",[S]), halt()."#,
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the compute loop must exit 0, got {:?}; stderr: {stderr}",
        out.status.code()
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "500000500000");
    assert!(
        !stderr.contains("allocation failure"),
        "no host-level allocation abort: {stderr}"
    );
}

/// The same loop under a smaller **configured** limit really is limited: `+hmax 1000000`
/// caps the term heap at 1 Mi words, and the loop's 2 Mi words of cons cells exceed it.
/// The documented behaviour is the emulator's `error:system_limit` (exit 1), **not** the
/// internal `allocation failure` diagnostic.
#[test]
fn exceeding_the_configured_heap_limit_raises_system_limit() {
    let out = run(&[
        "+hmax",
        "1000000",
        "-noshell",
        "-eval",
        r#"S = lists:sum(lists:seq(1,1000000)), io:format("~p~n",[S]), halt()."#,
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(1),
        "exceeding max_heap_size is a runtime error (exit 1); stderr: {stderr}"
    );
    assert!(
        stderr.contains("system_limit"),
        "the documented limit reason is `system_limit`, got: {stderr}"
    );
    assert!(
        !stderr.contains("allocation failure"),
        "never the host-level allocation diagnostic: {stderr}"
    );
}

/// A workload that fits under the default limit is not affected by the cap: the same
/// sequence with a small `+hmax` still runs when it fits.
#[test]
fn a_workload_within_the_limit_is_unaffected() {
    let out = run(&[
        "+hmax",
        "1000000",
        "-noshell",
        "-eval",
        r#"S = lists:sum(lists:seq(1,1000)), io:format("~p~n",[S]), halt()."#,
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a small loop must still run, got {:?}; stderr: {stderr}",
        out.status.code()
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "500500");
}
