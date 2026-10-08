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

//! Diffing two [`Report`]s into per-`(workload, runtime, metric)` rows.
//!
//! Used by `bench compare <a.json> <b.json>`. Rows also flag *presence/absence*
//! (a workload or metric that exists in only one report) and *status differences*, so
//! a skip that turned into a run — or vice versa — is never silently hidden behind a
//! missing number. `a` is treated as the baseline: `abs_delta = b - a`.
//!
//! The same module also provides the *cross-runtime* comparison: a single report
//! produced by `bench run --runtime all` is scanned for the `sys` (baseline) and `rust`
//! rows of each workload, and they are paired per `(workload, metric)` — that is what
//! `bench compare <report.json>` (one positional path) renders. See
//! [`cross_compare`] / [`CrossComparison`].

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::metrics::{Metric, Status};
use crate::report::{Report, WorkloadResult};
use crate::runtime::RuntimeKind;

/// One comparison row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompareRow {
    /// The workload id.
    pub workload: String,
    /// `sys` or `rust`.
    pub runtime: String,
    /// The metric name, or `null` for a status/presence row.
    pub metric: Option<String>,
    /// The value under report `a` (baseline), if present.
    pub a_value: Option<f64>,
    /// The value under report `b`, if present.
    pub b_value: Option<f64>,
    /// `b - a`, when both values are present.
    pub abs_delta: Option<f64>,
    /// `100 * (b - a) / a`, when both are present and `a != 0`.
    pub pct_delta: Option<f64>,
    /// A note for presence/absence or status-difference rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The full comparison of two reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    /// Label for report `a` (its file name or a caller-supplied string).
    pub a: String,
    /// Label for report `b`.
    pub b: String,
    /// The comparison rows, ordered by (workload, runtime, metric).
    pub rows: Vec<CompareRow>,
}

fn key(result: &WorkloadResult) -> (String, String) {
    (result.workload.clone(), result.runtime.as_str().to_string())
}

fn index(report: &Report) -> BTreeMap<(String, String), &WorkloadResult> {
    report.results.iter().map(|r| (key(r), r)).collect()
}

/// Compares two reports; `a` is the baseline.
pub fn compare(a: &Report, b: &Report) -> Comparison {
    compare_labeled(a, b, "a", "b")
}

/// Like [`compare`], but with explicit labels for the two reports.
pub fn compare_labeled(a: &Report, b: &Report, label_a: &str, label_b: &str) -> Comparison {
    let ia = index(a);
    let ib = index(b);

    let mut keys: BTreeSet<(String, String)> = BTreeSet::new();
    keys.extend(ia.keys().cloned());
    keys.extend(ib.keys().cloned());

    let mut rows = Vec::new();
    for (workload, runtime) in keys {
        match (ia.get(&(workload.clone(), runtime.clone())), ib.get(&(workload.clone(), runtime.clone()))) {
            (Some(_), None) => rows.push(CompareRow {
                workload,
                runtime,
                metric: None,
                a_value: None,
                b_value: None,
                abs_delta: None,
                pct_delta: None,
                note: Some("only-in-a".into()),
            }),
            (None, Some(_)) => rows.push(CompareRow {
                workload,
                runtime,
                metric: None,
                a_value: None,
                b_value: None,
                abs_delta: None,
                pct_delta: None,
                note: Some("only-in-b".into()),
            }),
            (Some(ra), Some(rb)) => {
                if ra.status != rb.status {
                    rows.push(CompareRow {
                        workload: workload.clone(),
                        runtime: runtime.clone(),
                        metric: None,
                        a_value: None,
                        b_value: None,
                        abs_delta: None,
                        pct_delta: None,
                        note: Some(format!(
                            "status {} -> {}",
                            ra.status.as_str(),
                            rb.status.as_str()
                        )),
                    });
                }
                compare_metrics(&workload, &runtime, &ra.metrics, &rb.metrics, &mut rows);
            }
            (None, None) => unreachable!("key came from a union of both indexes"),
        }
    }

    Comparison {
        a: label_a.to_string(),
        b: label_b.to_string(),
        rows,
    }
}

fn compare_metrics(
    workload: &str,
    runtime: &str,
    ma: &[Metric],
    mb: &[Metric],
    rows: &mut Vec<CompareRow>,
) {
    let map_a: BTreeMap<&str, &Metric> = ma.iter().map(|m| (m.name.as_str(), m)).collect();
    let map_b: BTreeMap<&str, &Metric> = mb.iter().map(|m| (m.name.as_str(), m)).collect();
    let mut names: BTreeSet<&str> = BTreeSet::new();
    names.extend(map_a.keys().copied());
    names.extend(map_b.keys().copied());

    for name in names {
        let row = match (map_a.get(name), map_b.get(name)) {
            (Some(_), None) => CompareRow {
                workload: workload.to_string(),
                runtime: runtime.to_string(),
                metric: Some(name.to_string()),
                a_value: None,
                b_value: None,
                abs_delta: None,
                pct_delta: None,
                note: Some("metric-only-in-a".into()),
            },
            (None, Some(_)) => CompareRow {
                workload: workload.to_string(),
                runtime: runtime.to_string(),
                metric: Some(name.to_string()),
                a_value: None,
                b_value: None,
                abs_delta: None,
                pct_delta: None,
                note: Some("metric-only-in-b".into()),
            },
            (Some(ma), Some(mb)) => {
                let a = ma.representative_value();
                let b = mb.representative_value();
                let abs = match (a, b) {
                    (Some(a), Some(b)) => Some(b - a),
                    _ => None,
                };
                let pct = match (a, b) {
                    (Some(a), Some(b)) if a != 0.0 => Some(100.0 * (b - a) / a),
                    _ => None,
                };
                CompareRow {
                    workload: workload.to_string(),
                    runtime: runtime.to_string(),
                    metric: Some(name.to_string()),
                    a_value: a,
                    b_value: b,
                    abs_delta: abs,
                    pct_delta: pct,
                    note: if a.is_none() || b.is_none() {
                        Some("value-missing".into())
                    } else {
                        None
                    },
                }
            }
            (None, None) => unreachable!("name came from a union of both maps"),
        };
        rows.push(row);
    }
}

impl Comparison {
    /// Serializes the comparison to pretty JSON.
    pub fn to_json_pretty(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// How many rows carry a real numeric delta.
    pub fn numeric_rows(&self) -> usize {
        self.rows.iter().filter(|r| r.abs_delta.is_some()).count()
    }

    /// How many rows are presence/absence or status notes rather than deltas.
    pub fn note_rows(&self) -> usize {
        self.rows.iter().filter(|r| r.note.is_some()).count()
    }
}

/// The metric name used for the synthetic peak-memory row.
pub const PEAK_METRIC: &str = "peak_memory_bytes";

/// One row of a *cross-runtime* comparison: the system (`sys`, the baseline `a`) versus
/// the Rust runtime (`rust`, `b`) for a single `(workload, metric)` pair.
///
/// A row with `metric == None` is a workload-level status row (a side was skipped/failed,
/// a workload was present on only one side, or a workload had no comparable metric); its
/// [`note`](CrossRow::note) explains why no delta could be produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrossRow {
    /// The workload id.
    pub workload: String,
    /// The metric name, or `null` for a workload-level status row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<String>,
    /// The metric unit (`ms`, `bytes`, …), or `null` for a status row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// The system-runtime value (the baseline `a`), if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sys: Option<f64>,
    /// The Rust-runtime value (`b`), if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rust: Option<f64>,
    /// `rust - sys`, when both values are present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abs_delta: Option<f64>,
    /// `100 * (rust - sys) / sys`, when both are present and `sys != 0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pct_delta: Option<f64>,
    /// `sys / rust` (a speedup for time metrics, a ratio for others) when `rust != 0`;
    /// `> 1.0` means the Rust runtime is the smaller — hence faster/leaner — of the two.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f64>,
    /// A presence/absence or status note when no delta could be computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A cross-runtime comparison: every workload's `sys` row paired with its `rust` row
/// within a *single* report. Produced by [`cross_compare`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrossComparison {
    /// Label for the report that was compared (its file name, or a caller string).
    pub report: String,
    /// The comparison rows, ordered by workload then metric name.
    pub rows: Vec<CrossRow>,
}

/// Builds the workload index: one `(sys, rust)` pair per workload id.
fn index_by_workload(report: &Report) -> BTreeMap<&str, (Option<&WorkloadResult>, Option<&WorkloadResult>)> {
    let mut map: BTreeMap<&str, (Option<&WorkloadResult>, Option<&WorkloadResult>)> = BTreeMap::new();
    for r in &report.results {
        let entry = map.entry(r.workload.as_str()).or_insert((None, None));
        match r.runtime {
            RuntimeKind::Sys => entry.0 = Some(r),
            RuntimeKind::Rust => entry.1 = Some(r),
        }
    }
    map
}

/// A human label for a side's status: `ok`, `skipped(<reason>)` or `failed(<reason>)`.
fn status_note(prefix: &str, r: &WorkloadResult) -> String {
    let detail = r.reason.as_deref().unwrap_or("unspecified");
    match r.status {
        Status::Ok => format!("{prefix}:ok"),
        Status::Skipped => format!("{prefix}:skipped({detail})"),
        Status::Failed => format!("{prefix}:failed({detail})"),
    }
}

/// A workload-level status row (no metric, no values).
fn status_row(workload: &str, note: impl Into<String>) -> CrossRow {
    CrossRow {
        workload: workload.to_string(),
        metric: None,
        unit: None,
        sys: None,
        rust: None,
        abs_delta: None,
        pct_delta: None,
        ratio: None,
        note: Some(note.into()),
    }
}

/// The note for a metric present on only one side.
fn one_sided(present_sys: bool, present_rust: bool) -> Option<String> {
    match (present_sys, present_rust) {
        (true, false) => Some("only-sys".to_string()),
        (false, true) => Some("only-rust".to_string()),
        _ => None,
    }
}

/// Builds a metric row from the two representative values.
fn delta_row(
    workload: &str,
    metric: &str,
    unit: &str,
    sys: Option<f64>,
    rust: Option<f64>,
    one_sided_note: Option<String>,
) -> CrossRow {
    let abs_delta = match (sys, rust) {
        (Some(a), Some(b)) => Some(b - a),
        _ => None,
    };
    let pct_delta = match (sys, rust) {
        (Some(a), Some(b)) if a != 0.0 => Some(100.0 * (b - a) / a),
        _ => None,
    };
    let ratio = match (sys, rust) {
        (Some(a), Some(b)) if b != 0.0 => Some(a / b),
        _ => None,
    };
    let note = if sys.is_none() || rust.is_none() {
        one_sided_note
    } else {
        None
    };
    CrossRow {
        workload: workload.to_string(),
        metric: Some(metric.to_string()),
        unit: Some(unit.to_string()),
        sys,
        rust,
        abs_delta,
        pct_delta,
        ratio,
        note,
    }
}

/// `sys` vs `rust` within one report, with no report label.
pub fn cross_compare(report: &Report) -> CrossComparison {
    cross_compare_labeled(report, "")
}

/// `sys` vs `rust` within one report, labelled (e.g. with the report's file name).
///
/// For every workload the `sys` row is the baseline (`a`) and the `rust` row is `b`:
/// `abs_delta = b - a`, `pct_delta = 100*(b-a)/a` (when `a != 0`) and `ratio = a / b`
/// (when `b != 0`). Metrics present on only one side carry an `only-sys`/`only-rust`
/// note; a side that was `skipped`/`failed` yields a workload-level note
/// (`sys:skipped(<reason>)` / `rust:skipped(<reason>)`, or `failed(...)`) and no
/// fabricated delta. `peak_memory_bytes` (a scalar field, not a metric) is included as a
/// synthetic `peak_memory_bytes`/`bytes` row so memory is compared too.
pub fn cross_compare_labeled(report: &Report, label: &str) -> CrossComparison {
    let mut rows = Vec::new();
    for (workload, (sys, rust)) in index_by_workload(report) {
        cross_workload(workload, sys, rust, &mut rows);
    }
    CrossComparison {
        report: label.to_string(),
        rows,
    }
}

/// Emits the rows for one workload.
fn cross_workload(
    workload: &str,
    sys: Option<&WorkloadResult>,
    rust: Option<&WorkloadResult>,
    rows: &mut Vec<CrossRow>,
) {
    let start = rows.len();

    // A present-but-not-`ok` side is reported as a workload-level note; its metrics (if
    // any) are never trusted, so no delta is fabricated against them.
    let mut status_parts = Vec::new();
    if let Some(r) = sys {
        if r.status != Status::Ok {
            status_parts.push(status_note("sys", r));
        }
    }
    if let Some(r) = rust {
        if r.status != Status::Ok {
            status_parts.push(status_note("rust", r));
        }
    }
    if !status_parts.is_empty() {
        rows.push(status_row(workload, status_parts.join(", ")));
    }

    // Only `ok` sides contribute comparable metrics.
    let sys_ok = sys.filter(|r| r.status == Status::Ok);
    let rust_ok = rust.filter(|r| r.status == Status::Ok);

    let sys_metrics: BTreeMap<&str, &Metric> = sys_ok
        .map(|r| r.metrics.iter().map(|m| (m.name.as_str(), m)).collect())
        .unwrap_or_default();
    let rust_metrics: BTreeMap<&str, &Metric> = rust_ok
        .map(|r| r.metrics.iter().map(|m| (m.name.as_str(), m)).collect())
        .unwrap_or_default();

    let mut names: BTreeSet<&str> = BTreeSet::new();
    names.extend(sys_metrics.keys().copied());
    names.extend(rust_metrics.keys().copied());

    for name in names.iter().copied() {
        let sm = sys_metrics.get(name).copied();
        let rm = rust_metrics.get(name).copied();
        let unit = sm
            .map(|m| m.unit.clone())
            .or_else(|| rm.map(|m| m.unit.clone()))
            .unwrap_or_default();
        let sys_value = sm.and_then(|m| m.representative_value());
        let rust_value = rm.and_then(|m| m.representative_value());
        let note = one_sided(sm.is_some(), rm.is_some());
        rows.push(delta_row(workload, name, &unit, sys_value, rust_value, note));
    }

    // `peak_memory_bytes` is a scalar field, not a metric — compare it as a synthetic row.
    let sys_peak = sys_ok.and_then(|r| r.peak_memory_bytes).map(|v| v as f64);
    let rust_peak = rust_ok.and_then(|r| r.peak_memory_bytes).map(|v| v as f64);
    if sys_peak.is_some() || rust_peak.is_some() {
        let note = one_sided(sys_peak.is_some(), rust_peak.is_some());
        rows.push(delta_row(workload, PEAK_METRIC, "bytes", sys_peak, rust_peak, note));
    }

    // A workload that produced no rows at all must still be visible.
    if rows.len() == start {
        let note = match (sys.is_some(), rust.is_some()) {
            (true, false) => "only-sys",
            (false, true) => "only-rust",
            (true, true) => "no-comparable-metrics",
            (false, false) => unreachable!("workload came from the results index"),
        };
        rows.push(status_row(workload, note));
    }
}

impl CrossComparison {
    /// Serializes the comparison to pretty JSON (a stable, additive shape).
    pub fn to_json_pretty(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// How many distinct workloads appear in the comparison.
    pub fn workload_count(&self) -> usize {
        self.rows
            .iter()
            .map(|r| r.workload.as_str())
            .collect::<BTreeSet<_>>()
            .len()
    }

    /// How many rows carry a real numeric delta.
    pub fn numeric_rows(&self) -> usize {
        self.rows.iter().filter(|r| r.abs_delta.is_some()).count()
    }

    /// How many rows are presence/absence or status notes rather than deltas.
    pub fn note_rows(&self) -> usize {
        self.rows.iter().filter(|r| r.note.is_some()).count()
    }

    /// Renders the comparison as a human-readable, fixed-width table.
    pub fn render_table(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{:<18} {:<24} {:<7} {:>14} {:>14} {:>14} {:>11} {:>8}\n",
            "workload", "metric", "unit", "sys", "rust", "delta", "delta%", "speedup"
        ));
        out.push_str(&"-".repeat(116));
        out.push('\n');
        for row in &self.rows {
            let metric = row.metric.as_deref().unwrap_or("-");
            let unit = row.unit.as_deref().unwrap_or("-");
            match (&row.sys, &row.rust) {
                (Some(s), Some(r)) => {
                    let delta = row
                        .abs_delta
                        .map(|d| format!("{d:+.4}"))
                        .unwrap_or_else(|| "-".to_string());
                    let pct = row
                        .pct_delta
                        .map(|p| format!("{p:+.2}%"))
                        .unwrap_or_else(|| "n/a".to_string());
                    let ratio = row
                        .ratio
                        .map(|x| format!("{x:.4}"))
                        .unwrap_or_else(|| "n/a".to_string());
                    out.push_str(&format!(
                        "{:<18} {:<24} {:<7} {s:>14.4} {r:>14.4} {delta:>14} {pct:>11} {ratio:>8}\n",
                        row.workload, metric, unit
                    ));
                }
                _ => {
                    let sys = row
                        .sys
                        .map(|v| format!("{v:.4}"))
                        .unwrap_or_else(|| "-".to_string());
                    let rust = row
                        .rust
                        .map(|v| format!("{v:.4}"))
                        .unwrap_or_else(|| "-".to_string());
                    let note = row.note.as_deref().unwrap_or("no-comparable-value");
                    out.push_str(&format!(
                        "{:<18} {:<24} {:<7} {sys:>14} {rust:>14} note={note}\n",
                        row.workload, metric, unit
                    ));
                }
            }
        }
        out
    }
}#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{Category, Metric, Status};
    use crate::report::{Profile, Report, WorkloadResult};
    use crate::runtime::{RuntimeInfo, RuntimeKind};

    fn report(results: Vec<WorkloadResult>) -> Report {
        let info = RuntimeInfo {
            kind: RuntimeKind::Sys,
            program: Some("erl".into()),
            version: Some("29".into()),
            supported: true,
            notes: vec![],
        };
        Report::new(Profile::Quick, vec![info], results)
    }

    fn ok(workload: &str, metrics: Vec<Metric>) -> WorkloadResult {
        let mut r = WorkloadResult::new(workload, RuntimeKind::Sys, Category::Startup, "d");
        r.metrics = metrics;
        r
    }

    #[test]
    fn numeric_delta_and_percent() {
        let a = report(vec![ok("baseline", vec![Metric::scalar("startup_ms", "ms", 100.0)])]);
        let b = report(vec![ok("baseline", vec![Metric::scalar("startup_ms", "ms", 150.0)])]);
        let c = compare(&a, &b);
        let row = c.rows.iter().find(|r| r.metric.as_deref() == Some("startup_ms")).unwrap();
        assert_eq!(row.a_value, Some(100.0));
        assert_eq!(row.b_value, Some(150.0));
        assert_eq!(row.abs_delta, Some(50.0));
        assert_eq!(row.pct_delta, Some(50.0));
        assert_eq!(c.numeric_rows(), 1);
    }

    #[test]
    fn zero_baseline_has_no_percent() {
        let a = report(vec![ok("x", vec![Metric::scalar("m", "u", 0.0)])]);
        let b = report(vec![ok("x", vec![Metric::scalar("m", "u", 5.0)])]);
        let c = compare(&a, &b);
        let row = &c.rows[0];
        assert_eq!(row.abs_delta, Some(5.0));
        assert_eq!(row.pct_delta, None);
    }

    #[test]
    fn presence_and_absence_rows() {
        let a = report(vec![ok("only_a", vec![])]);
        let b = report(vec![ok("only_b", vec![])]);
        let c = compare(&a, &b);
        let notes: Vec<_> = c.rows.iter().filter_map(|r| r.note.clone()).collect();
        assert!(notes.contains(&"only-in-a".to_string()));
        assert!(notes.contains(&"only-in-b".to_string()));
    }

    #[test]
    fn status_difference_is_flagged() {
        let a = report(vec![ok("w", vec![Metric::scalar("m", "u", 1.0)])]);
        let b = report(vec![WorkloadResult::skipped(
            "w",
            RuntimeKind::Sys,
            Category::Startup,
            "d",
            "rebar3-absent",
        )]);
        let c = compare(&a, &b);
        assert!(c
            .rows
            .iter()
            .any(|r| r.note.as_deref() == Some("status ok -> skipped")));
        // The metric now exists only in a.
        assert!(c
            .rows
            .iter()
            .any(|r| r.note.as_deref() == Some("metric-only-in-a")));
    }

    #[test]
    fn identical_reports_have_no_notes() {
        let a = report(vec![ok("w", vec![Metric::samples("s", "ms", &[1.0, 2.0])])]);
        let b = a.clone();
        let c = compare(&a, &b);
        assert_eq!(c.note_rows(), 0);
        assert_eq!(c.numeric_rows(), 1);
        assert_eq!(c.rows[0].abs_delta, Some(0.0));
    }

    #[test]
    fn json_round_trip() {
        let a = report(vec![ok("w", vec![Metric::scalar("m", "u", 1.0)])]);
        let b = report(vec![ok("w", vec![Metric::scalar("m", "u", 3.0)])]);
        let c = compare(&a, &b);
        let json = c.to_json_pretty().unwrap();
        let back: Comparison = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn skipped_reason_is_not_a_status_change_when_both_skipped() {
        let a = report(vec![WorkloadResult::skipped(
            "w",
            RuntimeKind::Sys,
            Category::Startup,
            "d",
            "r1",
        )]);
        let mut b = a.clone();
        b.results[0].status = Status::Skipped;
        let c = compare(&a, &b);
        assert_eq!(c.note_rows(), 0);
    }

    // ---- cross-runtime comparison (sys vs rust within one report) ----

    fn cross_report(results: Vec<WorkloadResult>) -> Report {
        let sys = RuntimeInfo {
            kind: RuntimeKind::Sys,
            program: Some("erl".into()),
            version: Some("29".into()),
            supported: true,
            notes: vec![],
        };
        let rust = RuntimeInfo {
            kind: RuntimeKind::Rust,
            program: Some("beam_rt".into()),
            version: Some("30".into()),
            supported: true,
            notes: vec![],
        };
        Report::new(Profile::Quick, vec![sys, rust], results)
    }

    fn row(workload: &str, runtime: RuntimeKind, metrics: Vec<Metric>) -> WorkloadResult {
        let mut r = WorkloadResult::new(workload, runtime, Category::Compute, "d");
        r.metrics = metrics;
        r
    }

    #[test]
    fn cross_compare_computes_delta_pct_and_ratio() {
        let rpt = cross_report(vec![
            row("baseline", RuntimeKind::Sys, vec![Metric::scalar("startup_ms", "ms", 100.0)]),
            row("baseline", RuntimeKind::Rust, vec![Metric::scalar("startup_ms", "ms", 150.0)]),
        ]);
        let c = cross_compare(&rpt);
        assert_eq!(c.workload_count(), 1);
        let row = c.rows.iter().find(|r| r.metric.as_deref() == Some("startup_ms")).unwrap();
        assert_eq!(row.sys, Some(100.0));
        assert_eq!(row.rust, Some(150.0));
        assert_eq!(row.abs_delta, Some(50.0));
        assert_eq!(row.pct_delta, Some(50.0));
        assert!((row.ratio.unwrap() - 100.0 / 150.0).abs() < 1e-12);
        assert_eq!(row.note, None);
        assert_eq!(c.numeric_rows(), 1);
    }

    #[test]
    fn cross_compare_uses_summary_mean_as_representative() {
        let rpt = cross_report(vec![
            row("w", RuntimeKind::Sys, vec![Metric::samples("startup_ms", "ms", &[10.0, 20.0, 30.0])]),
            row("w", RuntimeKind::Rust, vec![Metric::scalar("startup_ms", "ms", 30.0)]),
        ]);
        let c = cross_compare(&rpt);
        let row = &c.rows[0];
        assert_eq!(row.sys, Some(20.0)); // mean(10,20,30)
        assert_eq!(row.rust, Some(30.0));
        assert_eq!(row.abs_delta, Some(10.0));
        assert_eq!(row.pct_delta, Some(50.0));
    }

    #[test]
    fn cross_compare_flags_one_sided_metrics() {
        let rpt = cross_report(vec![
            row("w", RuntimeKind::Sys, vec![Metric::scalar("only_sys", "u", 1.0)]),
            row("w", RuntimeKind::Rust, vec![Metric::scalar("only_rust", "u", 2.0)]),
        ]);
        let c = cross_compare(&rpt);
        let sys_only = c.rows.iter().find(|r| r.metric.as_deref() == Some("only_sys")).unwrap();
        assert_eq!(sys_only.note.as_deref(), Some("only-sys"));
        assert_eq!(sys_only.sys, Some(1.0));
        assert_eq!(sys_only.rust, None);
        assert_eq!(sys_only.abs_delta, None);
        let rust_only = c.rows.iter().find(|r| r.metric.as_deref() == Some("only_rust")).unwrap();
        assert_eq!(rust_only.note.as_deref(), Some("only-rust"));
        assert_eq!(c.numeric_rows(), 0);
    }

    #[test]
    fn cross_compare_reports_skipped_side_without_fabricating() {
        let sys = row("w", RuntimeKind::Sys, vec![Metric::scalar("m", "u", 3.0)]);
        let rust = WorkloadResult::skipped(
            "w",
            RuntimeKind::Rust,
            Category::Compute,
            "d",
            "runtime-unsupported",
        );
        let c = cross_compare(&cross_report(vec![sys, rust]));
        // A workload-level status row names the skipped side.
        let status = c.rows.iter().find(|r| r.metric.is_none()).unwrap();
        assert_eq!(status.note.as_deref(), Some("rust:skipped(runtime-unsupported)"));
        assert_eq!(status.sys, None);
        assert_eq!(status.rust, None);
        // The sys metric is one-sided; no delta is fabricated.
        let metric = c.rows.iter().find(|r| r.metric.as_deref() == Some("m")).unwrap();
        assert_eq!(metric.sys, Some(3.0));
        assert_eq!(metric.rust, None);
        assert_eq!(metric.abs_delta, None);
        assert_eq!(metric.note.as_deref(), Some("only-sys"));
    }

    #[test]
    fn cross_compare_reports_failed_side() {
        let sys = row("w", RuntimeKind::Sys, vec![]);
        let rust = WorkloadResult::failed("w", RuntimeKind::Rust, Category::Compute, "d", "boom");
        let c = cross_compare(&cross_report(vec![sys, rust]));
        let status = c.rows.iter().find(|r| r.metric.is_none()).unwrap();
        assert_eq!(status.note.as_deref(), Some("rust:failed(boom)"));
        assert_eq!(c.numeric_rows(), 0);
    }

    #[test]
    fn cross_compare_both_skipped_emits_a_single_status_row() {
        let sys = WorkloadResult::skipped("w", RuntimeKind::Sys, Category::Compute, "d", "r1");
        let rust = WorkloadResult::skipped("w", RuntimeKind::Rust, Category::Compute, "d", "r2");
        let c = cross_compare(&cross_report(vec![sys, rust]));
        assert_eq!(c.rows.len(), 1);
        let note = c.rows[0].note.as_deref().unwrap();
        assert!(note.contains("sys:skipped(r1)"), "{note}");
        assert!(note.contains("rust:skipped(r2)"), "{note}");
    }

    #[test]
    fn cross_compare_includes_peak_memory_as_a_synthetic_row() {
        let mut sys = row("w", RuntimeKind::Sys, vec![Metric::scalar("m", "u", 1.0)]);
        sys.peak_memory_bytes = Some(1_000);
        let mut rust = row("w", RuntimeKind::Rust, vec![Metric::scalar("m", "u", 1.0)]);
        rust.peak_memory_bytes = Some(2_500);
        let c = cross_compare(&cross_report(vec![sys, rust]));
        let mem = c.rows.iter().find(|r| r.metric.as_deref() == Some(PEAK_METRIC)).unwrap();
        assert_eq!(mem.unit.as_deref(), Some("bytes"));
        assert_eq!(mem.sys, Some(1000.0));
        assert_eq!(mem.rust, Some(2500.0));
        assert_eq!(mem.abs_delta, Some(1500.0));
        assert_eq!(mem.pct_delta, Some(150.0));
    }

    #[test]
    fn cross_compare_marks_a_workload_with_no_comparable_metrics() {
        let sys = row("w", RuntimeKind::Sys, vec![]);
        let rust = row("w", RuntimeKind::Rust, vec![]);
        let c = cross_compare(&cross_report(vec![sys, rust]));
        assert_eq!(c.rows.len(), 1);
        assert_eq!(c.rows[0].metric, None);
        assert_eq!(c.rows[0].note.as_deref(), Some("no-comparable-metrics"));
    }

    #[test]
    fn cross_compare_marks_a_workload_present_on_only_one_side() {
        let rust = row("w", RuntimeKind::Rust, vec![]);
        let c = cross_compare(&cross_report(vec![rust]));
        assert_eq!(c.rows.len(), 1);
        assert_eq!(c.rows[0].note.as_deref(), Some("only-rust"));
    }

    #[test]
    fn cross_compare_labels_and_json_round_trip() {
        let rpt = cross_report(vec![
            row("w", RuntimeKind::Sys, vec![Metric::scalar("m", "u", 1.0)]),
            row("w", RuntimeKind::Rust, vec![Metric::scalar("m", "u", 3.0)]),
        ]);
        let c = cross_compare_labeled(&rpt, "report.json");
        assert_eq!(c.report, "report.json");
        let json = c.to_json_pretty().unwrap();
        let back: CrossComparison = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn cross_compare_json_shape_is_stable_and_additive() {
        let rpt = cross_report(vec![
            row("w", RuntimeKind::Sys, vec![Metric::scalar("m", "ms", 100.0)]),
            row("w", RuntimeKind::Rust, vec![Metric::scalar("m", "ms", 150.0)]),
        ]);
        let c = cross_compare_labeled(&rpt, "r.json");
        let v: serde_json::Value = serde_json::to_value(&c).unwrap();
        assert_eq!(v["report"], "r.json");
        assert_eq!(v["rows"][0]["workload"], "w");
        assert_eq!(v["rows"][0]["metric"], "m");
        assert_eq!(v["rows"][0]["unit"], "ms");
        assert_eq!(v["rows"][0]["sys"], 100.0);
        assert_eq!(v["rows"][0]["rust"], 150.0);
        assert_eq!(v["rows"][0]["abs_delta"], 50.0);
        assert_eq!(v["rows"][0]["pct_delta"], 50.0);
        // Absent fields are omitted, keeping the shape additive/stable.
        assert!(v["rows"][0].get("note").is_none());
    }

    #[test]
    fn cross_compare_render_table_has_a_header_and_the_values() {
        let rpt = cross_report(vec![
            row("w", RuntimeKind::Sys, vec![Metric::scalar("m", "ms", 100.0)]),
            WorkloadResult::skipped("cowboy", RuntimeKind::Rust, Category::Io, "d", "deps-unavailable"),
        ]);
        let table = cross_compare(&rpt).render_table();
        assert!(table.contains("workload"));
        assert!(table.contains("speedup"));
        assert!(table.contains("100.0000"));
        assert!(table.contains("rust:skipped(deps-unavailable)"));
    }
}
