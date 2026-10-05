//! Hermetic integration tests: the JSON report schema round-trips through
//! serialize → reload unchanged, and `compare` produces the expected deltas — with no
//! dependency on any Erlang runtime.

use bench::compare::compare_labeled;
use bench::metrics::{Category, Metric, Status};
use bench::report::{Profile, Report, WorkloadResult, SCHEMA_VERSION};
use bench::runtime::{RuntimeInfo, RuntimeKind};

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
