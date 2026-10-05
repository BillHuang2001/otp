//! Diffing two [`Report`]s into per-`(workload, runtime, metric)` rows.
//!
//! Used by `bench compare <a.json> <b.json>`. Rows also flag *presence/absence*
//! (a workload or metric that exists in only one report) and *status differences*, so
//! a skip that turned into a run — or vice versa — is never silently hidden behind a
//! missing number. `a` is treated as the baseline: `abs_delta = b - a`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::metrics::Metric;
use crate::report::{Report, WorkloadResult};

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

#[cfg(test)]
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
}
