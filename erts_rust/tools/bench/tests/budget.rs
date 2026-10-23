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

//! Integration tests for the latency-budget mechanism: they judge **real archived
//! reports** (`results/m17-xab-{rust,sys}-r1.json`), which is the strongest available
//! evidence that the pinned budgets are alarms that pass today — and that a report
//! written before the mechanism existed still loads, compares and can be re-judged.
//!
//! If the archived files are absent the tests are a **no-op skip** (they never fabricate
//! a verdict), so `cargo test -p bench` stays green in any checkout.

use std::path::{Path, PathBuf};

use bench::budget::{self, BudgetVerdict};
use bench::compare;
use bench::report::{Profile, Report};
use bench::runtime::RuntimeKind;

fn archive(name: &str) -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("results").join(name);
    if path.is_file() {
        Some(path)
    } else {
        eprintln!("skipping: archived report {} is absent", path.display());
        None
    }
}

fn load(name: &str) -> Option<Report> {
    archive(name).map(|p| Report::load_from(&p).expect("an archived report must load"))
}

/// The two halves of one matched `m17` round, merged into a single report (the shape a
/// `bench run --runtime all` produces, and what a ratio budget needs).
fn merged_round() -> Option<Report> {
    let rust = load("m17-xab-rust-r1.json")?;
    let sys = load("m17-xab-sys-r1.json")?;
    let mut results = rust.results;
    results.extend(sys.results);
    let mut runtimes = rust.runtimes;
    runtimes.extend(sys.runtimes);
    Some(Report::new(Profile::Quick, runtimes, results))
}

#[test]
fn an_archived_pre_budget_report_loads_with_an_empty_budget_section() {
    let Some(report) = load("m17-xab-rust-r1.json") else {
        return;
    };
    // Written before the mechanism existed: no `budgets` key in the file at all.
    assert!(report.budgets.is_empty(), "pre-budget reports carry no verdicts");
    assert!(!report.results.is_empty());
    assert!(report.results.iter().all(|r| r.runtime == RuntimeKind::Rust));
}

#[test]
fn an_archived_report_still_compares() {
    let (Some(a), Some(b)) = (load("m17-xab-rust-r1.json"), load("m17-xab-sys-r1.json")) else {
        return;
    };
    // Two-file mode (a = baseline): the budget section must not disturb it.
    let cmp = compare::compare_labeled(&a, &b, "m17-rust-r1", "m17-sys-r1");
    assert!(!cmp.rows.is_empty(), "a matched round has comparable rows");
    // Cross-runtime mode inside one merged report.
    let Some(merged) = merged_round() else {
        return;
    };
    let cross = compare::cross_compare_labeled(&merged, "m17-r1-merged");
    assert!(cross.numeric_rows() > 0, "merged report has sys-vs-rust rows");
}

#[test]
fn the_merged_m17_round_meets_every_budget_it_can_measure() {
    let Some(report) = merged_round() else {
        return;
    };
    let verdicts = budget::evaluate(&report);
    assert_eq!(verdicts.len(), budget::BUDGETS.len(), "one verdict per row");
    let exceeded: Vec<&BudgetVerdict> = verdicts.iter().filter(|v| v.is_exceeded()).collect();
    assert!(
        exceeded.is_empty(),
        "the current figures must sit inside the pinned budgets, these do not: {:#?}",
        exceeded.iter().map(|v| v.id().to_string()).collect::<Vec<_>>()
    );
    assert!(!budget::any_exceeded(&verdicts));

    // The rows that must be judgeable from a real matched round are judged, not silent.
    for (workload, runtime) in [
        ("baseline", RuntimeKind::Rust),
        ("baseline", RuntimeKind::Sys),
        ("copy_bound", RuntimeKind::Rust),
        ("gc_bound", RuntimeKind::Rust),
        ("list_walk", RuntimeKind::Rust),
    ] {
        let v = verdicts
            .iter()
            .find(|v| v.id().workload == workload && v.id().runtime == Some(runtime))
            .unwrap_or_else(|| panic!("no verdict for {workload}/{runtime:?}"));
        assert!(
            v.measured().is_some(),
            "{workload}/{runtime:?} should have been measured: {v:?}"
        );
        assert_eq!(v.outcome(), "met", "{workload}/{runtime:?} is inside its budget");
    }

    // The workloads a sibling agent is adding are absent here, so their rows are
    // honestly not-measured (never a pass, never a failure).
    for workload in ["file_io", "gc_class"] {
        let v = verdicts
            .iter()
            .find(|v| v.id().workload == workload)
            .unwrap_or_else(|| panic!("no verdict row for {workload}"));
        assert_eq!(v.outcome(), "not-measured");
        assert!(v.reason().expect("an explicit reason").contains(workload));
    }
}

#[test]
fn a_rust_only_report_leaves_the_ratio_rows_not_measured_with_a_reason() {
    let Some(report) = load("m17-xab-rust-r1.json") else {
        return;
    };
    let verdicts = budget::evaluate(&report);
    let ratios: Vec<&BudgetVerdict> = verdicts
        .iter()
        .filter(|v| v.limit().is_ratio() && v.id().workload == "baseline")
        .collect();
    assert_eq!(ratios.len(), 2, "the p50 and p99 startup ratios");
    for v in ratios {
        assert_eq!(v.outcome(), "not-measured", "{v:?}");
        let reason = v.reason().expect("a reason");
        assert!(reason.contains("sys"), "the reason must name the missing side: {reason}");
    }
    // The absolute rows *are* judgeable from a rust-only report.
    let absolute = verdicts
        .iter()
        .find(|v| v.id().workload == "baseline" && !v.limit().is_ratio() && v.id().stat.as_str() == "p50")
        .expect("the absolute p50 row");
    assert_eq!(absolute.outcome(), "met", "{absolute:?}");
}

#[test]
fn recorded_verdicts_survive_a_save_and_reload() {
    let Some(report) = merged_round() else {
        return;
    };
    let mut judged = report.clone();
    judged.budgets = budget::evaluate(&judged);

    let dir = std::env::temp_dir().join(format!("bench-budget-{}", std::process::id()));
    let path = dir.join("judged.json");
    judged.save_to(&path).expect("save");
    let back = Report::load_from(&path).expect("load");
    assert_eq!(back.budgets, judged.budgets, "verdicts round-trip through JSON");
    assert_eq!(back.budgets.len(), budget::BUDGETS.len());
    // The reloaded verdicts render without any access to the original report.
    let block = budget::render_block(&back.budgets);
    assert!(block.starts_with("latency budgets — "), "{block}");
    assert_eq!(block.lines().count(), back.budgets.len() + 1);
    let _ = std::fs::remove_dir_all(&dir);
}


// ---------------------------------------------------------------------------
// Unit tests for the budget vocabulary and the evaluator (kept out of
// `src/budget.rs` so that module stays inside the file-size convention).
// ---------------------------------------------------------------------------

use bench::budget::{
    any_exceeded, evaluate, exceeded_count, render_block, render_line, BudgetId, Limit, Stat,
    BUDGETS,
};
use bench::metrics::Metric;

fn id(workload: &str, runtime: RuntimeKind) -> BudgetId {
    BudgetId {
        workload: workload.to_string(),
        runtime: Some(runtime),
        metric: "startup_ms".to_string(),
        stat: Stat::P50,
    }
}

#[test]
fn stat_extracts_the_right_statistic() {
    let scalar = Metric::scalar("x", "ns", 7.0);
    let sampled = Metric::samples("y", "ms", &[1.0, 2.0, 3.0, 4.0]);
    assert_eq!(Stat::Value.extract(&scalar), Some(7.0));
    assert_eq!(Stat::Value.extract(&sampled), None);
    assert_eq!(Stat::P50.extract(&sampled), sampled.summary.as_ref().map(|s| s.p50));
    assert_eq!(Stat::P99.extract(&sampled), sampled.summary.as_ref().map(|s| s.p99));
    assert_eq!(Stat::P50.extract(&scalar), None);
    assert_eq!(Stat::P50.as_str(), "p50");
    assert_eq!(Stat::Value.as_str(), "value");
}

#[test]
fn limit_describes_absolute_and_ratio_forms() {
    assert_eq!(Limit::AtMost(800.0).describe(), "<= 800");
    assert_eq!(Limit::RatioAtMost(6.0).describe(), "rust/sys <= 6");
    assert_eq!(Limit::AtMost(800.0).value(), 800.0);
    assert!(Limit::RatioAtMost(6.0).is_ratio());
    assert!(!Limit::AtMost(800.0).is_ratio());
}

#[test]
fn verdict_accessors_and_tokens() {
    let met = BudgetVerdict::Met {
        id: id("baseline", RuntimeKind::Rust),
        limit: Limit::AtMost(800.0),
        measured: 300.0,
        rationale: "r".into(),
    };
    assert_eq!(met.outcome(), "met");
    assert_eq!(met.measured(), Some(300.0));
    assert_eq!(met.limit(), &Limit::AtMost(800.0));
    assert_eq!(met.rationale(), "r");
    assert_eq!(met.reason(), None);
    assert!(!met.is_exceeded());

    let over = BudgetVerdict::Exceeded {
        id: id("baseline", RuntimeKind::Rust),
        limit: Limit::AtMost(800.0),
        measured: 900.0,
        rationale: "r".into(),
    };
    assert!(over.is_exceeded());
    assert_eq!(over.outcome(), "exceeded");

    let missing = BudgetVerdict::NotMeasured {
        id: id("file_io", RuntimeKind::Rust),
        limit: Limit::AtMost(200000.0),
        reason: "no row".into(),
        rationale: "r".into(),
    };
    assert!(!missing.is_exceeded(), "not-measured is never a breach");
    assert_eq!(missing.measured(), None);
    assert_eq!(missing.reason(), Some("no row"));
    assert_eq!(missing.outcome(), "not-measured");
}

#[test]
fn verdicts_round_trip_through_json() {
    for v in [
        BudgetVerdict::Met {
            id: id("baseline", RuntimeKind::Rust),
            limit: Limit::RatioAtMost(6.0),
            measured: 2.5,
            rationale: "r".into(),
        },
        BudgetVerdict::NotMeasured {
            id: id("gc_class", RuntimeKind::Rust),
            limit: Limit::AtMost(200000.0),
            reason: "not in this report".into(),
            rationale: "r".into(),
        },
    ] {
        let value = serde_json::to_value(&v).unwrap();
        assert!(value["verdict"].is_string());
        assert_eq!(value["id"]["runtime"], "rust");
        assert!(
            value["limit"].get("at_most").or_else(|| value["limit"].get("ratio_at_most")).is_some(),
            "the limit form is snake_case and self-describing: {}",
            value["limit"]
        );
        let back: BudgetVerdict = serde_json::from_value(value).unwrap();
        assert_eq!(back, v);
    }
}

#[test]
fn render_lines_name_the_outcome_measured_limit_and_rationale() {
    let met = BudgetVerdict::Met {
        id: id("baseline", RuntimeKind::Rust),
        limit: Limit::AtMost(800.0),
        measured: 305.75,
        rationale: "a regression alarm".into(),
    };
    let line = render_line(&met);
    assert!(line.contains("met"), "{line}");
    assert!(line.contains("baseline[rust] startup_ms p50"), "{line}");
    assert!(line.contains("305.75"), "{line}");
    assert!(line.contains("<= 800"), "{line}");
    assert!(line.contains("a regression alarm"), "{line}");

    let ratio = BudgetVerdict::Met {
        id: id("baseline", RuntimeKind::Rust),
        limit: Limit::RatioAtMost(6.0),
        measured: 2.3625,
        rationale: "r".into(),
    };
    assert!(render_line(&ratio).contains("2.36x"), "{}", render_line(&ratio));

    let over = BudgetVerdict::Exceeded {
        id: id("copy_bound", RuntimeKind::Rust),
        limit: Limit::AtMost(20000.0),
        measured: 21000.0,
        rationale: "r".into(),
    };
    assert!(render_line(&over).starts_with("  EXCEEDED"), "{}", render_line(&over));

    let missing = BudgetVerdict::NotMeasured {
        id: id("file_io", RuntimeKind::Rust),
        limit: Limit::AtMost(200000.0),
        reason: "workload absent".into(),
        rationale: "r".into(),
    };
    let line = render_line(&missing);
    assert!(line.contains("not-measured"), "{line}");
    assert!(line.contains("workload absent"), "{line}");
}

#[test]
fn render_block_tallies_all_three_outcomes() {
    let verdicts = vec![
        BudgetVerdict::Met {
            id: id("a", RuntimeKind::Rust),
            limit: Limit::AtMost(1.0),
            measured: 0.5,
            rationale: "r".into(),
        },
        BudgetVerdict::Exceeded {
            id: id("b", RuntimeKind::Rust),
            limit: Limit::AtMost(1.0),
            measured: 2.0,
            rationale: "r".into(),
        },
        BudgetVerdict::NotMeasured {
            id: id("c", RuntimeKind::Rust),
            limit: Limit::AtMost(1.0),
            reason: "absent".into(),
            rationale: "r".into(),
        },
    ];
    let block = render_block(&verdicts);
    assert!(block.starts_with("latency budgets — 1 met / 1 exceeded / 1 not measured"), "{block}");
    assert_eq!(block.lines().count(), 4);
    assert_eq!(exceeded_count(&verdicts), 1);
    assert!(any_exceeded(&verdicts));
    assert!(!any_exceeded(&verdicts[..1]));
}

#[test]
fn budget_table_is_coherent() {
    assert!(!BUDGETS.is_empty());
    for b in BUDGETS {
        assert!(!b.workload.is_empty());
        assert!(!b.metric.is_empty());
        assert!(b.limit.value() > 0.0, "{} has a non-positive limit", b.metric);
        assert!(
            b.rationale.split_whitespace().count() >= 15,
            "{} needs a substantive rationale",
            b.metric
        );
        // A ratio row is meaningless on the `sys` side (it would be sys/sys).
        if b.limit.is_ratio() {
            assert_ne!(b.runtime, Some(RuntimeKind::Sys), "{} is a sys self-ratio", b.metric);
        }
    }
    // No two rows may constrain the same quantity with the same kind of limit.
    let mut seen: Vec<(&str, Option<RuntimeKind>, &str, Stat, bool)> = Vec::new();
    for b in BUDGETS {
        let key = (b.workload, b.runtime, b.metric, b.stat, b.limit.is_ratio());
        assert!(!seen.contains(&key), "duplicate budget row {key:?}");
        seen.push(key);
    }
}

#[test]
fn every_mandated_quantity_has_a_row() {
    // The two module-level mandated budgets (startup latency, p99 tail) plus one row
    // per benchmark category that names a per-unit figure.
    assert!(BUDGETS.iter().any(|b| b.stat == Stat::P99));
    assert!(BUDGETS.iter().any(|b| b.limit.is_ratio()));
    for (workload, metric) in [
        ("baseline", "startup_ms"),
        ("cowboy", "latency_ms"),
        ("list_walk", "case_list_walk_ns_per_element"),
        ("copy_bound", "case_send_recv_ns_per_op"),
        ("gc_bound", "case_churn_ns_per_op"),
        ("file_io", "case_write_read_ns_per_op"),
        ("gc_class", "case_send_recv_ns_per_op"),
    ] {
        assert!(
            BUDGETS
                .iter()
                .any(|b| b.workload == workload && b.metric == metric),
            "missing budget row for {workload}/{metric}"
        );
    }
}

#[test]
fn an_absent_workload_is_not_measured_never_a_panic_or_failure() {
    let report = Report::new(Profile::Quick, vec![], vec![]);
    let verdicts = evaluate(&report);
    assert_eq!(verdicts.len(), BUDGETS.len(), "one verdict per row");
    assert_eq!(exceeded_count(&verdicts), 0);
    for v in &verdicts {
        assert_eq!(v.outcome(), "not-measured");
        let reason = v.reason().expect("an explicit reason");
        assert!(!reason.is_empty());
    }
    let file_io = verdicts
        .iter()
        .find(|v| v.id().workload == "file_io")
        .expect("the file_io row is still judged");
    assert!(file_io.reason().unwrap().contains("no `file_io` result"), "{file_io:?}");
}
