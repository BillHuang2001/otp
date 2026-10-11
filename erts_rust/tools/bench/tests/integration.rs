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

//! Hermetic integration tests: the JSON report schema round-trips through
//! serialize → reload unchanged, and `compare` produces the expected deltas — with no
//! dependency on any Erlang runtime.

use bench::compare::compare_labeled;
use bench::metrics::{Category, Metric, Runtimes, Status};
use bench::report::{Profile, Report, WorkloadResult, SCHEMA_VERSION};
use bench::runtime::{RuntimeInfo, RuntimeKind};
use bench::workload;

fn report(results: Vec<WorkloadResult>, profile: Profile) -> Report {
    let info = RuntimeInfo {
        kind: RuntimeKind::Sys,
        program: Some("/nix/store/erl".into()),
        version: Some("29".into()),
        supported: true,
        notes: vec![],
    };
    Report::new(profile, vec![info], results)
}

fn baseline_ok(startup: f64) -> WorkloadResult {
    WorkloadResult::new(
        "baseline",
        RuntimeKind::Sys,
        Category::Startup,
        "system-ERTS startup latency + fixed compute loop",
    )
    .with_metric(Metric::samples("startup_ms", "ms", &[startup, startup + 1.0, startup + 2.0]))
    .with_metric(Metric::scalar("compute_ops_per_sec", "ops/s", 9_000_000.0))
    .with_peak_memory(Some(34_000_000))
}

#[test]
fn synthetic_report_round_trips_through_json() {
    let results = vec![
        baseline_ok(100.0),
        WorkloadResult::skipped(
            "cowboy",
            RuntimeKind::Sys,
            Category::Io,
            "http",
            "no-http-load-generator",
        ),
        WorkloadResult::failed("plb", RuntimeKind::Sys, Category::Compute, "kernels", "dotnet-failed"),
    ];
    let report = report(results, Profile::Quick);

    let json = report.to_json_pretty().expect("serialize");
    let reloaded = Report::from_json(&json).expect("deserialize + validate");

    assert_eq!(report, reloaded);
    assert_eq!(reloaded.schema_version, SCHEMA_VERSION);
    assert_eq!(reloaded.profile, Profile::Quick);
    assert_eq!(reloaded.results.len(), 3);
    assert_eq!(reloaded.results[0].status, Status::Ok);
    assert_eq!(reloaded.results[1].status, Status::Skipped);
    assert_eq!(reloaded.results[2].status, Status::Failed);
}

#[test]
fn synthetic_reports_compare_with_deltas() {
    let a = report(vec![baseline_ok(100.0)], Profile::Quick);
    let b = report(vec![baseline_ok(150.0)], Profile::Quick);

    let cmp = compare_labeled(&a, &b, "a.json", "b.json");
    assert_eq!(cmp.a, "a.json");
    assert_eq!(cmp.b, "b.json");

    let startup = cmp
        .rows
        .iter()
        .find(|r| r.metric.as_deref() == Some("startup_ms"))
        .expect("startup_ms row");
    // Mean of [100,101,102] = 101; mean of [150,151,152] = 151.
    assert_eq!(startup.a_value, Some(101.0));
    assert_eq!(startup.b_value, Some(151.0));
    assert_eq!(startup.abs_delta, Some(50.0));
    assert!((startup.pct_delta.unwrap() - 49.504_950_495).abs() < 1e-6);

    // The comparison itself is JSON-stable.
    let json = cmp.to_json_pretty().expect("serialize comparison");
    let back: bench::compare::Comparison = serde_json::from_str(&json).unwrap();
    assert_eq!(back, cmp);
}

#[test]
fn comparison_flags_presence_and_status_changes() {
    let a = report(vec![baseline_ok(100.0)], Profile::Quick);
    let b = report(
        vec![WorkloadResult::skipped(
            "baseline",
            RuntimeKind::Sys,
            Category::Startup,
            "d",
            "runtime-unsupported",
        )],
        Profile::Quick,
    );
    let cmp = compare_labeled(&a, &b, "a", "b");
    assert!(cmp
        .rows
        .iter()
        .any(|r| r.note.as_deref() == Some("status ok -> skipped")));
}

/// Regression guard for the **cross-runtime measurement** contract: the driver must be
/// able to measure the Rust runtime on *real foreign target-project code*, not only the
/// system ERTS. The `foreign` workload is the one that runs the same real Cowboy/EMQX
/// `.beam` under *both* runtimes, so it must stay `Runtimes::Both` (schedulable under the
/// `rust` runtime) and keep declaring the contracts it exercises. If it is ever flipped
/// back to `SysOnly` or removed, this fails loudly — no silent loss of Rust coverage.
///
/// The same guard covers the three other cross-runtime signals: `list_walk` (compute),
/// `copy_bound` (messaging/copy) and `gc_bound` (GC).
#[test]
fn a_real_foreign_workload_is_measured_under_both_runtimes() {
    let foreign = workload::find("foreign").expect("the `foreign` workload must be registered");

    assert!(
        matches!(foreign.runtimes(), Runtimes::Both),
        "`foreign` must run under both runtimes, got {:?}",
        foreign.runtimes()
    );
    assert!(foreign.runtimes().allows(RuntimeKind::Rust));
    assert!(foreign.runtimes().allows(RuntimeKind::Sys));

    // Real `.beam` modules (A.2) executing real Erlang semantics (A.1).
    assert!(foreign.contracts().contains(&"A.1"), "contracts: {:?}", foreign.contracts());
    assert!(foreign.contracts().contains(&"A.2"), "contracts: {:?}", foreign.contracts());

    // At least one registered workload must be Rust-schedulable, and `foreign` is it.
    let rust_capable: Vec<&str> = workload::registry()
        .iter()
        .filter(|w| w.runtimes().allows(RuntimeKind::Rust))
        .map(|w| w.id())
        .collect();
    assert!(
        rust_capable.contains(&"foreign"),
        "the rust-schedulable workload set must include `foreign`; got {rust_capable:?}"
    );

    // `list_walk` is the in-tree interpreter-throughput signal: it must stay registered,
    // run under *both* runtimes, and stay a `compute` workload.
    let list_walk =
        workload::find("list_walk").expect("the `list_walk` workload must be registered");
    assert!(
        matches!(list_walk.runtimes(), Runtimes::Both),
        "`list_walk` must run under both runtimes, got {:?}",
        list_walk.runtimes()
    );
    assert_eq!(list_walk.category(), Category::Compute);
    assert!(
        list_walk.contracts().contains(&"A.1") && list_walk.contracts().contains(&"A.2"),
        "contracts: {:?}",
        list_walk.contracts()
    );
    assert!(
        rust_capable.contains(&"list_walk"),
        "the rust-schedulable workload set must include `list_walk`; got {rust_capable:?}"
    );

    // `copy_bound` (messaging/copy-bound) and `gc_bound` (GC-bound) close the two
    // non-compute cross-runtime gaps: both must stay registered, run under *both* runtimes,
    // and keep their category/contracts. If either is ever flipped back to `SysOnly` or
    // removed, this fails loudly — no silent loss of Rust coverage.
    let copy_bound =
        workload::find("copy_bound").expect("the `copy_bound` workload must be registered");
    assert!(
        matches!(copy_bound.runtimes(), Runtimes::Both),
        "`copy_bound` must run under both runtimes, got {:?}",
        copy_bound.runtimes()
    );
    assert_eq!(copy_bound.category(), Category::Messaging);
    assert!(
        copy_bound.contracts().contains(&"A.1") && copy_bound.contracts().contains(&"A.5"),
        "contracts: {:?}",
        copy_bound.contracts()
    );
    assert!(
        rust_capable.contains(&"copy_bound"),
        "the rust-schedulable workload set must include `copy_bound`; got {rust_capable:?}"
    );

    let gc_bound = workload::find("gc_bound").expect("the `gc_bound` workload must be registered");
    assert!(
        matches!(gc_bound.runtimes(), Runtimes::Both),
        "`gc_bound` must run under both runtimes, got {:?}",
        gc_bound.runtimes()
    );
    assert_eq!(gc_bound.category(), Category::Gc);
    assert!(
        gc_bound.contracts().contains(&"A.1"),
        "contracts: {:?}",
        gc_bound.contracts()
    );
    assert!(
        rust_capable.contains(&"gc_bound"),
        "the rust-schedulable workload set must include `gc_bound`; got {rust_capable:?}"
    );
}
