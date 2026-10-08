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

//! Result vocabulary: status, category, probe, runtime-set and the [`Metric`] type.
//!
//! These types are the stable, machine-readable heart of the report schema. In
//! particular [`Metric`] serializes to `{name, unit, value?|summary?}` with **exactly
//! one** of `value` / `summary` present — enforced both by construction
//! ([`Metric::scalar`] / [`Metric::samples`]) and by [`Metric::validate`].

use serde::{Deserialize, Serialize};

use crate::error::{BenchError, Result};
use crate::stats::{summarize, Summary};

/// Outcome of a single `(workload × runtime)` pair.
///
/// `skipped` is a first-class, *honest* outcome: a workload that cannot run is
/// reported as skipped with a short kebab-case reason — never as `ok`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The workload ran and produced metrics.
    Ok,
    /// The workload did not run (with a `reason`).
    Skipped,
    /// The workload was attempted but errored.
    Failed,
}

impl Status {
    /// The lowercase string used in the JSON report and the human table.
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Skipped => "skipped",
            Status::Failed => "failed",
        }
    }
}

/// The benchmark category a workload belongs to (Part I §C grouping).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    /// Startup / boot latency.
    Startup,
    /// Compiler throughput.
    Compiler,
    /// Compute / JIT-bound.
    Compute,
    /// GC-bound.
    Gc,
    /// I/O-bound.
    Io,
    /// Messaging / copy-bound.
    Messaging,
}

impl Category {
    /// The lowercase string used in the JSON report and the human table.
    pub fn as_str(&self) -> &'static str {
        match self {
            Category::Startup => "startup",
            Category::Compiler => "compiler",
            Category::Compute => "compute",
            Category::Gc => "gc",
            Category::Io => "io",
            Category::Messaging => "messaging",
        }
    }
}

/// The honest prerequisite check a workload performs before it is scheduled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// All prerequisites are present; the workload may run.
    Ready,
    /// A prerequisite is missing; carries the short kebab-case reason string.
    Skipped(String),
}

impl Probe {
    /// Builds a `Skipped` probe from a kebab-case reason.
    pub fn skipped(reason: impl Into<String>) -> Probe {
        Probe::Skipped(reason.into())
    }

    /// Whether the workload is ready to run.
    pub fn is_ready(&self) -> bool {
        matches!(self, Probe::Ready)
    }

    /// The reason string when skipped, else `None`.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Probe::Ready => None,
            Probe::Skipped(r) => Some(r),
        }
    }

    /// A human label, e.g. `ready` or `skipped(dotnet-absent)`.
    pub fn label(&self) -> String {
        match self {
            Probe::Ready => "ready".to_string(),
            Probe::Skipped(r) => format!("skipped({r})"),
        }
    }
}

/// Which runtimes a workload may be scheduled under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtimes {
    /// Usable under both the system oracle and the Rust runtime.
    Both,
    /// Oracle-only (the Rust runtime cannot host it yet).
    SysOnly,
}

impl Runtimes {
    /// Whether a workload with this set may run under the given runtime kind.
    pub fn allows(&self, kind: crate::runtime::RuntimeKind) -> bool {
        match self {
            Runtimes::Both => true,
            Runtimes::SysOnly => kind == crate::runtime::RuntimeKind::Sys,
        }
    }
}

/// A single measured quantity: either a scalar `value` or a sampled `summary`.
///
/// The two inner `Option`s are mutually exclusive; JSON serialization omits the
/// absent one (`skip_serializing_if`), so a metric object always has exactly one of
/// `value` / `summary`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    /// Metric name (snake_case, e.g. `startup_ms`).
    pub name: String,
    /// Unit string (e.g. `ms`, `ops/s`, `files/s`, `bytes`).
    pub unit: String,
    /// A single measured value (scalar metric).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// A summarized distribution (sampled metric).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Summary>,
}

impl Metric {
    /// A scalar metric (`{name, unit, value}`).
    pub fn scalar(name: impl Into<String>, unit: impl Into<String>, value: f64) -> Metric {
        Metric {
            name: name.into(),
            unit: unit.into(),
            value: Some(value),
            summary: None,
        }
    }

    /// A sampled metric (`{name, unit, summary}`).
    pub fn samples(name: impl Into<String>, unit: impl Into<String>, samples: &[f64]) -> Metric {
        Metric {
            name: name.into(),
            unit: unit.into(),
            value: None,
            summary: Some(summarize(samples)),
        }
    }

    /// Whether this metric is a scalar.
    pub fn is_scalar(&self) -> bool {
        self.value.is_some()
    }

    /// A single representative number for comparison: the scalar `value`, or the
    /// `mean` of a sampled summary.
    pub fn representative_value(&self) -> Option<f64> {
        match (&self.value, &self.summary) {
            (Some(v), None) => Some(*v),
            (None, Some(s)) => Some(s.mean),
            _ => None,
        }
    }

    /// Enforces the "exactly one of `value` / `summary`" invariant.
    pub fn validate(&self) -> Result<()> {
        match (&self.value, &self.summary) {
            (Some(_), None) | (None, Some(_)) => Ok(()),
            (Some(_), Some(_)) => Err(BenchError::schema(format!(
                "metric `{}` has both `value` and `summary`",
                self.name
            ))),
            (None, None) => Err(BenchError::schema(format!(
                "metric `{}` has neither `value` nor `summary`",
                self.name
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_and_category_render_lowercase() {
        assert_eq!(Status::Ok.as_str(), "ok");
        assert_eq!(Status::Skipped.as_str(), "skipped");
        assert_eq!(Status::Failed.as_str(), "failed");
        assert_eq!(Category::Gc.as_str(), "gc");
        assert_eq!(Category::Io.as_str(), "io");
        assert_eq!(Category::Messaging.as_str(), "messaging");
    }

    #[test]
    fn probe_helpers() {
        assert!(Probe::Ready.is_ready());
        assert_eq!(Probe::Ready.reason(), None);
        assert_eq!(Probe::Ready.label(), "ready");
        let p = Probe::skipped("rebar3-absent");
        assert!(!p.is_ready());
        assert_eq!(p.reason(), Some("rebar3-absent"));
        assert_eq!(p.label(), "skipped(rebar3-absent)");
    }

    #[test]
    fn runtimes_gate_scheduling() {
        use crate::runtime::RuntimeKind;
        assert!(Runtimes::Both.allows(RuntimeKind::Sys));
        assert!(Runtimes::Both.allows(RuntimeKind::Rust));
        assert!(Runtimes::SysOnly.allows(RuntimeKind::Sys));
        assert!(!Runtimes::SysOnly.allows(RuntimeKind::Rust));
    }

    #[test]
    fn scalar_serializes_with_value_and_no_summary() {
        let m = Metric::scalar("startup_ms", "ms", 12.5);
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["name"], "startup_ms");
        assert_eq!(json["unit"], "ms");
        assert_eq!(json["value"], 12.5);
        assert!(json.get("summary").is_none(), "summary must be omitted");
        assert!(m.is_scalar());
        assert_eq!(m.representative_value(), Some(12.5));
        m.validate().unwrap();
    }

    #[test]
    fn samples_serialize_with_summary_and_no_value() {
        let m = Metric::samples("startup_ms", "ms", &[1.0, 2.0, 3.0, 4.0]);
        let json = serde_json::to_value(&m).unwrap();
        assert!(json.get("value").is_none(), "value must be omitted");
        assert_eq!(json["summary"]["count"], 4);
        assert_eq!(json["summary"]["min"], 1.0);
        assert_eq!(json["summary"]["max"], 4.0);
        assert!(!m.is_scalar());
        assert_eq!(m.representative_value(), Some(2.5));
        m.validate().unwrap();
    }

    #[test]
    fn round_trips_through_json() {
        for m in [
            Metric::scalar("a", "ms", 1.0),
            Metric::samples("b", "ms", &[1.0, 2.0]),
        ] {
            let s = serde_json::to_string(&m).unwrap();
            let back: Metric = serde_json::from_str(&s).unwrap();
            assert_eq!(back, m);
        }
    }

    #[test]
    fn validate_rejects_both_and_neither() {
        let both = Metric {
            name: "x".into(),
            unit: "ms".into(),
            value: Some(1.0),
            summary: Some(Summary::EMPTY),
        };
        assert!(matches!(both.validate(), Err(BenchError::Schema(_))));
        let neither = Metric {
            name: "y".into(),
            unit: "ms".into(),
            value: None,
            summary: None,
        };
        assert!(matches!(neither.validate(), Err(BenchError::Schema(_))));
    }
}
