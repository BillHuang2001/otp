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

//! Micro-benchmark: the interpreter against the native JIT tiers.
//!
//! The Milestone-1 JIT tiers are **mechanisms**, not optimisers (Part II §F):
//! correctness first, performance work follows. This test is the honest measurement of
//! where they stand today — it prints `ns/op` (and `ops/s`) per workload and tier, and
//! asserts **only correctness** (every tier's result equals the interpreter's, and is
//! the value the workload is *supposed* to compute). It never asserts that a JIT is
//! faster: that is environment-dependent and would be flaky; the printed numbers are
//! the evidence.
//!
//! The harness and the nine workloads live in `beam_engine::jit::bench` (the crate's
//! single JIT-measurement module — see its `CONTEXT.md` for the workload set, the
//! method and the caveats). This file owns the two `#[test]` entry points:
//!
//! * [`interpreter_and_native_tiers_agree_and_report_timings`] — the correctness half,
//!   in the ordinary `cargo test` run.
//! * [`interpreter_versus_native_tiers_measurement_table`] — the `#[ignore]`d timing
//!   table. Run it explicitly with `--nocapture` (CI does not run `#[ignore]`d tests):
//!   `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine
//!   --test jit_bench -- --ignored --nocapture`.
//!
//! Both tiers allocate in the context's arena, which is never freed, so read the
//! numbers as order-of-magnitude evidence on a **shared, high-load** sandbox — a range,
//! not a single figure. The one-time `JitBackend::compile` is measured separately and
//! excluded from the timed loop.

use beam_engine::jit::bench::{assert_tier_agreement, run_measurement_table};

#[test]
fn interpreter_and_native_tiers_agree_and_report_timings() {
    // One test (not several) so the workload timings never run concurrently and
    // contaminate each other; every workload is still measured and asserted.
    assert_tier_agreement();
}

#[test]
#[ignore = "measurement table: run with --ignored --nocapture"]
fn interpreter_versus_native_tiers_measurement_table() {
    run_measurement_table();
}
