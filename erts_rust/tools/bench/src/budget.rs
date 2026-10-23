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

//! Latency budgets: explicit, recorded limits and a per-budget verdict.
//!
//! `ERTS_RUST_REWRITE_EVALUATION.md` §G.7/§I and `ERTS_RUST_REWRITE_DEEPDIVE.md`
//! mandate **startup latency and p99 tail-latency budgets**, but they name no numbers,
//! and the driver otherwise only *records* what it measured — it never says whether a
//! figure is acceptable. This module supplies the missing half: [`BUDGETS`] is a small
//! hand-pinned table of one budget per quantity worth guarding (each with the sentence
//! saying why that limit, and what the limit is *not*), and [`evaluate`] turns a
//! finished [`Report`] into one [`BudgetVerdict`] per row.
//!
//! # Rules
//!
//! * **Pure.** [`evaluate`] takes `&Report` and does no I/O, no spawning, no clock
//!   reads — so an archived report can be judged offline, and so can a report written
//!   by an older driver.
//! * **Three outcomes, one of which is honest silence.** [`BudgetVerdict::Met`],
//!   [`BudgetVerdict::Exceeded`] and [`BudgetVerdict::NotMeasured`]. A row whose
//!   `(workload, runtime, metric)` is not in the report (not selected, skipped,
//!   unsupported runtime, a workload that does not exist yet, or a marginal withheld
//!   inside the noise band) is `NotMeasured` **with a specific reason** — never a pass
//!   and never a failure, because fabricating either would be worse than saying nothing.
//! * **Ratio rows need both sides in one report.** [`Limit::RatioAtMost`] divides the
//!   row's own measurement by the *`sys`* measurement of the same metric+stat. A
//!   `--runtime rust`-only run therefore leaves every ratio row `NotMeasured` (explicit
//!   reason), and a ratio row declared on the `sys` runtime is refused as a
//!   self-comparison.
//! * **Inclusive bounds.** `measured <= limit` is `Met`; a ratio is no different.
//!
//! # How the numbers were chosen
//!
//! Every limit is a **regression alarm that passes today**, tuned against the recorded
//! figures in `RESULTS.md` / the current session's `results/m19-*.json` (with the m17
//! figures quoted where they are still the wider range) with headroom above the worst
//! recorded round, and each rationale says so and says what the limit
//! is *not* (never an SLO for this shared host; never a parity target). Where the
//! illustrative constants in the task brief sat *below* what has actually been
//! measured on this host — the absolute `startup_ms` ceilings, and a `4.0` startup
//! ratio that m16 reached `3.79` against — the limit was raised (to `800`/`2500` ms and
//! `6.0`), because an alarm that fires on today's data is a broken alarm, not a budget.

use serde::{Deserialize, Serialize};

use crate::metrics::{Metric, Status};
use crate::report::{Report, WorkloadResult};
use crate::runtime::RuntimeKind;

/// Which statistic of a metric a budget constrains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stat {
    /// The metric's scalar `value` (the per-unit figures: `ns_per_op`, …).
    Value,
    /// The `p50` of the metric's sampled `summary` (the startup median).
    P50,
    /// The `p99` of the metric's sampled `summary` (the tail).
    P99,
}

impl Stat {
    /// The lowercase string used in the JSON report and the human line.
    pub fn as_str(&self) -> &'static str {
        match self {
            Stat::Value => "value",
            Stat::P50 => "p50",
            Stat::P99 => "p99",
        }
    }

    /// The statistic read out of `metric`, or `None` when the metric does not carry it
    /// (a `Value` stat against a sampled metric, or a percentile against a scalar).
    pub fn extract(&self, metric: &Metric) -> Option<f64> {
        match self {
            Stat::Value => metric.value,
            Stat::P50 => metric.summary.as_ref().map(|s| s.p50),
            Stat::P99 => metric.summary.as_ref().map(|s| s.p99),
        }
    }
}

/// The comparison a budget row makes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Limit {
    /// Absolute ceiling on the measured statistic, in the metric's own unit.
    AtMost(f64),
    /// Ceiling on `rust / sys` of the same metric+stat (dimensionless).
    RatioAtMost(f64),
}

impl Limit {
    /// The numeric bound (absolute, or the ratio bound).
    pub fn value(&self) -> f64 {
        match self {
            Limit::AtMost(v) | Limit::RatioAtMost(v) => *v,
        }
    }

    /// Whether this is the dimensionless `rust`-vs-`sys` ratio form.
    pub fn is_ratio(&self) -> bool {
        matches!(self, Limit::RatioAtMost(_))
    }

    /// A human rendering of the bound, e.g. `<= 800` or `rust/sys <= 6`.
    pub fn describe(&self) -> String {
        match self {
            Limit::AtMost(v) => format!("<= {v}"),
            Limit::RatioAtMost(v) => format!("rust/sys <= {v}"),
        }
    }
}

/// One budget row: what is constrained, by how much, and why.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    /// The workload id the row constrains (`baseline`, `cowboy`, …).
    pub workload: &'static str,
    /// The runtime the row applies to, or `None` for "every runtime present".
    pub runtime: Option<RuntimeKind>,
    /// The metric name the row reads (exactly as the workload records it).
    pub metric: &'static str,
    /// Which statistic of that metric.
    pub stat: Stat,
    /// The limit and the kind of comparison.
    pub limit: Limit,
    /// One sentence: why this limit, and what the limit is *not*.
    pub rationale: &'static str,
}

impl Budget {
    /// The row's identity as it is copied into every verdict.
    pub fn id(&self) -> BudgetId {
        BudgetId {
            workload: self.workload.to_string(),
            runtime: self.runtime,
            metric: self.metric.to_string(),
            stat: self.stat,
        }
    }
}

/// The identity of a budget row, carried by every verdict so that a *saved* report is
/// self-describing (a reader of the JSON needs no copy of this table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetId {
    /// The workload id.
    pub workload: String,
    /// The runtime selector (`null` = every runtime present).
    pub runtime: Option<RuntimeKind>,
    /// The metric name.
    pub metric: String,
    /// Which statistic.
    pub stat: Stat,
}

impl std::fmt::Display for BudgetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.runtime {
            Some(k) => write!(f, "{}[{}] {} {}", self.workload, k.as_str(), self.metric, self.stat.as_str()),
            None => write!(f, "{}[*] {} {}", self.workload, self.metric, self.stat.as_str()),
        }
    }
}

/// The judge of a report: one verdict per budget row (and, for a `runtime: None` row,
/// one per runtime the report actually contains).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum BudgetVerdict {
    /// The measurement was present and inside the limit.
    Met {
        /// The budget row's identity.
        id: BudgetId,
        /// The limit that was applied.
        limit: Limit,
        /// The measured statistic (an absolute value, or the `rust/sys` ratio).
        measured: f64,
        /// The row's rationale, so the verdict stands on its own.
        rationale: String,
    },
    /// The measurement was present and outside the limit.
    Exceeded {
        /// The budget row's identity.
        id: BudgetId,
        /// The limit that was applied.
        limit: Limit,
        /// The measured statistic (an absolute value, or the `rust/sys` ratio).
        measured: f64,
        /// The row's rationale, so the verdict stands on its own.
        rationale: String,
    },
    /// There was nothing to judge, with an explicit reason — never a pass, never a
    /// failure.
    NotMeasured {
        /// The budget row's identity.
        id: BudgetId,
        /// The limit that *would* have been applied.
        limit: Limit,
        /// Why there is no verdict (specific: which row/metric/runtime is missing).
        reason: String,
        /// The row's rationale, so the verdict stands on its own.
        rationale: String,
    },
}

impl BudgetVerdict {
    /// The budget row this verdict is about.
    pub fn id(&self) -> &BudgetId {
        match self {
            BudgetVerdict::Met { id, .. }
            | BudgetVerdict::Exceeded { id, .. }
            | BudgetVerdict::NotMeasured { id, .. } => id,
        }
    }

    /// The applied (or would-be) limit.
    pub fn limit(&self) -> &Limit {
        match self {
            BudgetVerdict::Met { limit, .. }
            | BudgetVerdict::Exceeded { limit, .. }
            | BudgetVerdict::NotMeasured { limit, .. } => limit,
        }
    }

    /// The row's rationale.
    pub fn rationale(&self) -> &str {
        match self {
            BudgetVerdict::Met { rationale, .. }
            | BudgetVerdict::Exceeded { rationale, .. }
            | BudgetVerdict::NotMeasured { rationale, .. } => rationale,
        }
    }

    /// The measured statistic, or `None` when nothing was measured.
    pub fn measured(&self) -> Option<f64> {
        match self {
            BudgetVerdict::Met { measured, .. } | BudgetVerdict::Exceeded { measured, .. } => {
                Some(*measured)
            }
            BudgetVerdict::NotMeasured { .. } => None,
        }
    }

    /// Why there is no verdict, when there is none.
    pub fn reason(&self) -> Option<&str> {
        match self {
            BudgetVerdict::NotMeasured { reason, .. } => Some(reason),
            _ => None,
        }
    }

    /// The machine token: `met`, `exceeded` or `not_measured`.
    pub fn outcome(&self) -> &'static str {
        match self {
            BudgetVerdict::Met { .. } => "met",
            BudgetVerdict::Exceeded { .. } => "exceeded",
            BudgetVerdict::NotMeasured { .. } => "not-measured",
        }
    }

    /// Whether the measurement is outside its limit (the only failure signal).
    pub fn is_exceeded(&self) -> bool {
        matches!(self, BudgetVerdict::Exceeded { .. })
    }
}

/// Formats a measured statistic: the ratio form is dimensionless and gets an `x`.
fn fmt_kind(value: f64, is_ratio: bool) -> String {
    if is_ratio {
        format!("{value:.2}x")
    } else {
        format!("{value:.2}")
    }
}

/// The runner's compact block: a header with the tally, then one line per budget.
pub fn render_block(verdicts: &[BudgetVerdict]) -> String {
    let exceeded = exceeded_count(verdicts);
    let not_measured = verdicts
        .iter()
        .filter(|v| matches!(v, BudgetVerdict::NotMeasured { .. }))
        .count();
    let mut out = format!(
        "latency budgets — {} met / {exceeded} exceeded / {not_measured} not measured",
        verdicts.len() - exceeded - not_measured
    );
    for v in verdicts {
        out.push('\n');
        out.push_str(&render_line(v));
    }
    out
}

/// One line: `met|EXCEEDED|not-measured`, the identity, the measured value, the limit
/// and the rationale.
pub fn render_line(v: &BudgetVerdict) -> String {
    let id = v.id();
    let limit = v.limit().describe();
    let ratio = v.limit().is_ratio();
    let rationale = v.rationale();
    match v {
        BudgetVerdict::Met { measured, .. } => format!(
            "  met            {id} = {}  (limit {limit}) — {rationale}",
            fmt_kind(*measured, ratio)
        ),
        BudgetVerdict::Exceeded { measured, .. } => format!(
            "  EXCEEDED       {id} = {}  (limit {limit}) — {rationale}",
            fmt_kind(*measured, ratio)
        ),
        BudgetVerdict::NotMeasured { reason, .. } => format!(
            "  not-measured   {id}  (limit {limit}) — {reason} — {rationale}"
        ),
    }
}

/// How many verdicts breached their limit.
pub fn exceeded_count(verdicts: &[BudgetVerdict]) -> usize {
    verdicts.iter().filter(|v| v.is_exceeded()).count()
}

/// Whether any verdict breached its limit (`NotMeasured` is never a breach).
pub fn any_exceeded(verdicts: &[BudgetVerdict]) -> bool {
    verdicts.iter().any(|v| v.is_exceeded())
}

/// Evaluates every row of [`BUDGETS`] against `report`. Pure: no I/O, no clock.
pub fn evaluate(report: &Report) -> Vec<BudgetVerdict> {
    BUDGETS.iter().flat_map(|b| b.evaluate(report)).collect()
}

impl Budget {
    /// One verdict per runtime the row applies to (a `runtime: None` row applies to
    /// every runtime that has a row for its workload).
    fn evaluate(&self, report: &Report) -> Vec<BudgetVerdict> {
        let kinds: Vec<RuntimeKind> = match self.runtime {
            Some(kind) => vec![kind],
            None => runtimes_present(report, self.workload),
        };
        if kinds.is_empty() {
            return vec![self.not_measured(
                format!(
                    "no `{}` result in this report (the workload is not registered, or was \
                     not selected by --workloads)",
                    self.workload
                ),
            )];
        }
        kinds
            .into_iter()
            .map(|kind| self.evaluate_one(report, kind))
            .collect()
    }

    /// The verdict for one runtime.
    fn evaluate_one(&self, report: &Report, kind: RuntimeKind) -> BudgetVerdict {
        let row = match find_row(report, self.workload, kind) {
            Some(row) => row,
            None if !workload_present(report, self.workload) => {
                return self.not_measured(format!(
                    "no `{}` result in this report (the workload is not registered, or was not \
                     selected by --workloads)",
                    self.workload
                ))
            }
            None => {
                return self.not_measured(format!(
                    "no `{}` row for `{}` in this report (that runtime was not selected, or is \
                     unsupported here)",
                    kind.as_str(),
                    self.workload
                ))
            }
        };
        if row.status != Status::Ok {
            let why = row.reason.as_deref().unwrap_or("no reason recorded");
            return self.not_measured(format!(
                "the `{}` row for `{}` is `{}` ({why}), so it carries no measurement",
                kind.as_str(),
                self.workload,
                row.status.as_str()
            ));
        }
        let Some(metric) = row.metrics.iter().find(|m| m.name == self.metric) else {
            let hint = row
                .notes
                .iter()
                .find(|n| n.contains(self.metric))
                .map(|n| format!("; row note: {n}"))
                .unwrap_or_default();
            return self.not_measured(format!(
                "the `{}` row for `{}` carries no `{}` metric{hint}",
                kind.as_str(),
                self.workload,
                self.metric
            ));
        };
        let Some(measured) = self.stat.extract(metric) else {
            let shape = if metric.summary.is_some() {
                "a sampled summary"
            } else {
                "a scalar"
            };
            return self.not_measured(format!(
                "`{}` on the `{}` row for `{}` is {shape} and carries no `{}` value",
                self.metric,
                kind.as_str(),
                self.workload,
                self.stat.as_str()
            ));
        };
        if !measured.is_finite() {
            return self.not_measured(format!(
                "the `{}` {} on the `{}` row for `{}` is not finite ({measured})",
                self.metric,
                self.stat.as_str(),
                kind.as_str(),
                self.workload
            ));
        }
        match self.limit {
            Limit::AtMost(bound) => self.verdict(measured, bound),
            Limit::RatioAtMost(bound) => {
                if kind == RuntimeKind::Sys {
                    return self.not_measured(
                        "a ratio budget compares `rust` against `sys`; this row's runtime is \
                         `sys`, so the ratio would be a self-comparison"
                            .to_string(),
                    );
                }
                match self.sys_measurement(report) {
                    Ok(baseline) => self.verdict(measured / baseline, bound),
                    Err(why) => self.not_measured(format!(
                        "the ratio needs a usable `sys` `{}` {} measurement and there is none \
                         ({why}); the `{}` side measured {measured:.4}",
                        self.metric,
                        self.stat.as_str(),
                        kind.as_str()
                    )),
                }
            }
        }
    }

    /// The `sys` denominator of a ratio row.
    fn sys_measurement(&self, report: &Report) -> std::result::Result<f64, String> {
        let row = find_row(report, self.workload, RuntimeKind::Sys)
            .ok_or_else(|| "no `sys` row for this workload in the report".to_string())?;
        if row.status != Status::Ok {
            return Err(format!(
                "the `sys` row is `{}` ({})",
                row.status.as_str(),
                row.reason.as_deref().unwrap_or("no reason recorded")
            ));
        }
        let metric = row
            .metrics
            .iter()
            .find(|m| m.name == self.metric)
            .ok_or_else(|| format!("the `sys` row carries no `{}` metric", self.metric))?;
        let value = self.stat.extract(metric).ok_or_else(|| {
            format!(
                "the `sys` `{}` carries no `{}` value",
                self.metric,
                self.stat.as_str()
            )
        })?;
        if !value.is_finite() || value <= 0.0 {
            return Err(format!(
                "the `sys` `{}` {} is not a usable denominator ({value})",
                self.metric,
                self.stat.as_str()
            ));
        }
        Ok(value)
    }

    /// Compares a measured statistic against an absolute bound.
    fn verdict(&self, measured: f64, bound: f64) -> BudgetVerdict {
        let id = self.id();
        let rationale = self.rationale.to_string();
        if measured <= bound {
            BudgetVerdict::Met {
                id,
                limit: self.limit,
                measured,
                rationale,
            }
        } else {
            BudgetVerdict::Exceeded {
                id,
                limit: self.limit,
                measured,
                rationale,
            }
        }
    }

    /// A `NotMeasured` verdict carrying this row's identity and limit.
    fn not_measured(&self, reason: String) -> BudgetVerdict {
        BudgetVerdict::NotMeasured {
            id: self.id(),
            limit: self.limit,
            reason,
            rationale: self.rationale.to_string(),
        }
    }
}

/// The row for `(workload, runtime)`, if the report has one.
fn find_row<'a>(
    report: &'a Report,
    workload: &str,
    kind: RuntimeKind,
) -> Option<&'a WorkloadResult> {
    report
        .results
        .iter()
        .find(|r| r.workload == workload && r.runtime == kind)
}

/// Whether the report has any row at all for `workload`.
fn workload_present(report: &Report, workload: &str) -> bool {
    report.results.iter().any(|r| r.workload == workload)
}

/// The distinct runtimes that have a row for `workload`, in report order.
fn runtimes_present(report: &Report, workload: &str) -> Vec<RuntimeKind> {
    let mut kinds: Vec<RuntimeKind> = Vec::new();
    for r in report.results.iter().filter(|r| r.workload == workload) {
        if !kinds.contains(&r.runtime) {
            kinds.push(r.runtime);
        }
    }
    kinds
}

/// The mandate made explicit: one row per budgeted quantity.
///
/// Every limit is a **regression alarm that passes today** (the recorded figures it was
/// tuned against are quoted in each rationale), not an SLO and not a parity target. The
/// rust-vs-sys startup rows are the load-robust instrument; the absolute rows exist to
/// catch a hang and to keep a host artifact from being misread as a Rust regression.
pub const BUDGETS: &[Budget] = &[
    Budget {
        workload: "baseline",
        runtime: Some(RuntimeKind::Rust),
        metric: "startup_ms",
        stat: Stat::P50,
        limit: Limit::AtMost(800.0),
        rationale: "A gross-regression alarm for the Rust boot median (the six m19 matched \
                    rounds read 260.3-261.4 ms at a 1-min load of 0.5-4.5; m17's loaded rounds \
                    read 289-546 ms at load 3-92), not an SLO — a shared host can double it \
                    without a code change, so the load-robust judgement is the rust/sys ratio \
                    row.",
    },
    Budget {
        workload: "baseline",
        runtime: Some(RuntimeKind::Rust),
        metric: "startup_ms",
        stat: Stat::P99,
        limit: Limit::AtMost(2500.0),
        rationale: "Catches a hang or an order-of-magnitude tail regression (the six m19 \
                    matched rounds read 261.9-267.6 ms p99; m17 read up to 485 ms, with a \
                    single 1702 ms outlier on a heavily loaded A/B round), not an SLO: a boot's \
                    p99 is a handful of samples, so this row only notices a gross failure.",
    },
    Budget {
        workload: "baseline",
        runtime: Some(RuntimeKind::Rust),
        metric: "startup_ms",
        stat: Stat::P50,
        limit: Limit::RatioAtMost(6.0),
        rationale: "The load-robust startup judgement: the six m19 matched rounds read \
                    2.51-2.53x (m16 reached 3.79x), so 6x is a regression alarm with headroom \
                    above every recorded round — not a target, and deliberately not so tight \
                    that ordinary host drift trips it.",
    },
    Budget {
        workload: "baseline",
        runtime: Some(RuntimeKind::Rust),
        metric: "startup_ms",
        stat: Stat::P99,
        limit: Limit::RatioAtMost(6.0),
        rationale: "The same instrument for the tail; the p99 ratios track the p50 ones \
                    (2.35-2.58x across the six m19 matched rounds) because a boot's p99 is a \
                    handful of samples, so the same 6x alarm applies — not an SLO for a shared \
                    host.",
    },
    Budget {
        workload: "baseline",
        runtime: Some(RuntimeKind::Sys),
        metric: "startup_ms",
        stat: Stat::P99,
        limit: Limit::AtMost(400.0),
        rationale: "The oracle's own tail, recorded at 103.5-113.3 ms across the six m19 \
                    matched rounds (m17 read 105-153 ms): a host artifact big enough to move the \
                    far less load-sensitive `sys` side shows up here too, so it cannot be \
                    misread as a Rust regression — and 400 ms is \
                    deliberately loose because this row exists to attribute, not to gate.",
    },
    Budget {
        workload: "cowboy",
        runtime: Some(RuntimeKind::Sys),
        metric: "latency_ms",
        stat: Stat::P99,
        limit: Limit::AtMost(5.0),
        rationale: "The request-tail budget on the I/O-bound target, recorded at 0.11-0.16 ms \
                    p99 while serving ~119-124k req/s through a real Cowboy release across the \
                    three m19 external reps (m17's contended pass read 3.03-3.59 ms at ~38k \
                    req/s): 5 ms leaves room for a loaded host yet still flags a real latency \
                    regression; it reads the summary the cowboy row actually records \
                    (`latency_ms` — there is no separate p99 metric) and it is sys-only because \
                    the Rust runtime cannot boot an OTP release.",
    },
    Budget {
        workload: "list_walk",
        runtime: Some(RuntimeKind::Rust),
        metric: "case_list_walk_ns_per_element",
        stat: Stat::Value,
        limit: Limit::AtMost(2000.0),
        rationale: "The per-element dispatch budget for the standing per-unit gap, recorded at \
                    18.4-18.7 ns/element across the six m19 matched rounds (the oracle reads \
                    2.30-2.34; m17's loaded rounds read up to 48.2): over a hundred times \
                    today's figure, so it is a regression alarm for the engine's \
                    dispatch path and not a parity target — the gap itself is `beam_engine`'s \
                    to close.",
    },
    Budget {
        workload: "copy_bound",
        runtime: Some(RuntimeKind::Rust),
        metric: "case_send_recv_ns_per_op",
        stat: Stat::Value,
        limit: Limit::AtMost(20000.0),
        rationale: "The messaging budget for the two-process send/receive round trip, recorded \
                    at 6242-6338 ns/op across the six m19 matched rounds (the oracle reads \
                    1487-1531; m17's worst round read 12450): ~3.2x above today's figure and \
                    still ~1.6x above m17's worst, so it is a regression alarm rather than a \
                    parity target, and it is not a hard ceiling on a contended host.",
    },
    Budget {
        workload: "gc_bound",
        runtime: Some(RuntimeKind::Rust),
        metric: "case_churn_ns_per_op",
        stat: Stat::Value,
        limit: Limit::AtMost(200000.0),
        rationale: "The GC-churn budget for allocate-and-abandon, recorded at 59.6k-60.3k ns/op \
                    across the six m19 matched rounds (the oracle reads 4.24k-4.27k; m17's worst \
                    round read 106k): ~3.3x above today's figure, a regression alarm for a figure \
                    whose collection count the row does not claim — not a parity target.",
    },
    Budget {
        workload: "file_io",
        runtime: Some(RuntimeKind::Rust),
        metric: "case_write_read_ns_per_op",
        stat: Stat::Value,
        limit: Limit::AtMost(200000.0),
        rationale: "The file-I/O budget for the write-then-read round trip of a 4096-byte \
                    payload, recorded at 97.6k-103.1k ns/op across the six m19 matched rounds \
                    (median 98 643) while the oracle reads 133.9k-135.6k — i.e. the Rust runtime \
                    is already ~1.36x cheaper on this case, so the limit sits ~2x above the \
                    measured figure as a regression alarm for the file path, not a parity \
                    target and not a clean cross-runtime ratio.",
    },
    Budget {
        workload: "gc_class",
        runtime: Some(RuntimeKind::Rust),
        metric: "case_send_recv_ns_per_op",
        stat: Stat::Value,
        limit: Limit::AtMost(200000.0),
        rationale: "The GC-class budget for the two-process round trip of a ~390-word payload \
                    (deliberately above the nursery, so a collection is admitted), recorded at \
                    51.0k-51.4k ns/op across the six m19 matched rounds (median 51 282) against \
                    the oracle's 3.19k-3.27k: the limit is ~3.9x the measured figure, a \
                    regression alarm for the copy-plus-collect path — the ~15.8x gap itself is \
                    the engine's to close, not what this row gates.",
    },
];
