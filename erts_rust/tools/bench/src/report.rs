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

//! The result report: the stable, machine-readable JSON schema and the constructors
//! used by the runner.
//!
//! [`SCHEMA_VERSION`] pins the wire format; [`Report::from_json`] refuses anything that
//! is not a matching, internally-consistent report (so `compare` fails loudly on
//! garbage input instead of silently producing nonsense). Every `run`/`baseline`
//! writes one of these under the crate's `out/` directory.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{BenchError, Result};
use crate::metrics::{Category, Metric, Status};
use crate::runtime::{RuntimeInfo, RuntimeKind};

/// The current report schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// The tool name recorded in every report.
pub const TOOL: &str = "bench";

/// The benchmark profile: the default `--quick` and the milestone-only `--full`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Fast default profile.
    Quick,
    /// Milestone-only profile (longer runs, extra workloads).
    Full,
}

impl Profile {
    /// The lowercase string used in the JSON report and file names.
    pub fn as_str(&self) -> &'static str {
        match self {
            Profile::Quick => "quick",
            Profile::Full => "full",
        }
    }

    /// Number of startup-latency samples the `baseline` workload takes.
    pub fn startup_runs(&self) -> usize {
        match self {
            Profile::Quick => 10,
            Profile::Full => 50,
        }
    }

    /// Number of compute-loop samples the `baseline` workload takes.
    pub fn compute_runs(&self) -> usize {
        5
    }
}

/// A complete benchmark run: header plus one [`WorkloadResult`] per `(workload ×
/// runtime)` pair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// Wire-format version (always [`SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// Tool name (always `"bench"`).
    pub tool: String,
    /// Tool version (`CARGO_PKG_VERSION`).
    pub tool_version: String,
    /// Generation time, milliseconds since the Unix epoch.
    pub generated_at_unix_ms: u128,
    /// The profile this run used.
    pub profile: Profile,
    /// One entry per resolved runtime.
    pub runtimes: Vec<RuntimeInfo>,
    /// One entry per scheduled `(workload × runtime)` pair.
    pub results: Vec<WorkloadResult>,
}

impl Report {
    /// Builds a fresh report stamped with the current time.
    pub fn new(profile: Profile, runtimes: Vec<RuntimeInfo>, results: Vec<WorkloadResult>) -> Report {
        Report {
            schema_version: SCHEMA_VERSION,
            tool: TOOL.to_string(),
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            generated_at_unix_ms: unix_ms(),
            profile,
            runtimes,
            results,
        }
    }

    /// Serializes to pretty JSON.
    pub fn to_json_pretty(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Parses and validates a report from JSON.
    pub fn from_json(text: &str) -> Result<Report> {
        let report: Report = serde_json::from_str(text)?;
        report.validate()?;
        Ok(report)
    }

    /// Checks the schema version and the per-metric exactly-one-of invariant.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(BenchError::schema(format!(
                "unsupported schema_version {} (this tool understands {})",
                self.schema_version, SCHEMA_VERSION
            )));
        }
        for result in &self.results {
            for metric in &result.metrics {
                metric.validate()?;
            }
            match result.status {
                Status::Skipped if result.reason.is_none() => {
                    return Err(BenchError::schema(format!(
                        "skipped result `{}` has no reason",
                        result.workload
                    )));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Loads a report from a file path.
    pub fn load_from(path: &Path) -> Result<Report> {
        let text = std::fs::read_to_string(path)?;
        Report::from_json(&text)
    }

    /// Writes the report as pretty JSON, creating parent directories as needed.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(path, self.to_json_pretty()?)?;
        Ok(())
    }

    /// The default output file name for a profile: `bench-<profile>-<unix_ms>.json`.
    pub fn default_file_name(&self) -> String {
        format!("bench-{}-{}.json", self.profile.as_str(), self.generated_at_unix_ms)
    }

    /// Resolves `--out <dir>` into the concrete file path the report is written to.
    pub fn resolve_out_path(&self, out_dir: &Path) -> PathBuf {
        out_dir.join(self.default_file_name())
    }
}

/// The result of running one workload under one runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkloadResult {
    /// Workload id (e.g. `baseline`).
    pub workload: String,
    /// Which runtime produced it.
    pub runtime: RuntimeKind,
    /// The workload's benchmark category.
    pub category: Category,
    /// Human-readable description.
    pub description: String,
    /// Outcome.
    pub status: Status,
    /// The skip reason when `status == Skipped`.
    pub reason: Option<String>,
    /// Recorded metrics (empty on skip/failure).
    #[serde(default)]
    pub metrics: Vec<Metric>,
    /// Peak OS RSS in bytes, or `null` when unmeasurable.
    #[serde(default)]
    pub peak_memory_bytes: Option<u64>,
    /// Free-form notes (partial results, unmeasured metrics, …).
    #[serde(default)]
    pub notes: Vec<String>,
}

impl WorkloadResult {
    /// A blank result for a workload, defaulted to `ok`.
    pub fn new(
        workload: impl Into<String>,
        runtime: RuntimeKind,
        category: Category,
        description: impl Into<String>,
    ) -> WorkloadResult {
        WorkloadResult {
            workload: workload.into(),
            runtime,
            category,
            description: description.into(),
            status: Status::Ok,
            reason: None,
            metrics: Vec::new(),
            peak_memory_bytes: None,
            notes: Vec::new(),
        }
    }

    /// A skipped result carrying a short kebab-case reason.
    pub fn skipped(
        workload: impl Into<String>,
        runtime: RuntimeKind,
        category: Category,
        description: impl Into<String>,
        reason: impl Into<String>,
    ) -> WorkloadResult {
        let mut r = WorkloadResult::new(workload, runtime, category, description);
        r.status = Status::Skipped;
        r.reason = Some(reason.into());
        r
    }

    /// A failed result carrying the failure description in `reason`.
    pub fn failed(
        workload: impl Into<String>,
        runtime: RuntimeKind,
        category: Category,
        description: impl Into<String>,
        reason: impl Into<String>,
    ) -> WorkloadResult {
        let mut r = WorkloadResult::new(workload, runtime, category, description);
        r.status = Status::Failed;
        r.reason = Some(reason.into());
        r
    }

    /// Builder: append a metric.
    pub fn with_metric(mut self, metric: Metric) -> WorkloadResult {
        self.metrics.push(metric);
        self
    }

    /// Builder: set the peak memory.
    pub fn with_peak_memory(mut self, bytes: Option<u64>) -> WorkloadResult {
        self.peak_memory_bytes = bytes;
        self
    }

    /// Builder: append a note.
    pub fn with_note(mut self, note: impl Into<String>) -> WorkloadResult {
        self.notes.push(note.into());
        self
    }
}

/// Milliseconds since the Unix epoch (saturating to `0` before 1970).
pub fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Category;

    fn sample_report() -> Report {
        let info = RuntimeInfo {
            kind: RuntimeKind::Sys,
            program: Some("/usr/bin/erl".into()),
            version: Some("29".into()),
            supported: true,
            notes: vec![],
        };
        let ok = WorkloadResult::new("baseline", RuntimeKind::Sys, Category::Startup, "desc")
            .with_metric(Metric::scalar("startup_ms", "ms", 10.0))
            .with_peak_memory(Some(1024));
        let skipped = WorkloadResult::skipped(
            "cowboy",
            RuntimeKind::Sys,
            Category::Io,
            "http",
            "no-http-load-generator",
        );
        Report::new(Profile::Quick, vec![info], vec![ok, skipped])
    }

    #[test]
    fn new_sets_header_fields() {
        let r = sample_report();
        assert_eq!(r.schema_version, SCHEMA_VERSION);
        assert_eq!(r.tool, "bench");
        assert_eq!(r.tool_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(r.profile, Profile::Quick);
        assert!(r.generated_at_unix_ms > 0);
    }

    #[test]
    fn json_round_trip_is_stable() {
        let r = sample_report();
        let json = r.to_json_pretty().unwrap();
        let back = Report::from_json(&json).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn schema_field_names_are_stable() {
        let value: serde_json::Value = serde_json::to_value(sample_report()).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["tool"], "bench");
        assert_eq!(value["profile"], "quick");
        assert_eq!(value["results"][0]["workload"], "baseline");
        assert_eq!(value["results"][0]["runtime"], "sys");
        assert_eq!(value["results"][0]["category"], "startup");
        assert_eq!(value["results"][0]["status"], "ok");
        assert_eq!(value["results"][0]["peak_memory_bytes"], 1024);
        assert_eq!(value["results"][1]["status"], "skipped");
        assert_eq!(value["results"][1]["reason"], "no-http-load-generator");
    }

    #[test]
    fn validate_rejects_wrong_schema_version() {
        let mut r = sample_report();
        r.schema_version = 999;
        assert!(matches!(r.validate(), Err(BenchError::Schema(_))));
    }

    #[test]
    fn validate_rejects_skipped_without_reason() {
        let mut r = sample_report();
        r.results[1].reason = None;
        assert!(matches!(r.validate(), Err(BenchError::Schema(_))));
    }

    #[test]
    fn save_and_load_round_trip_creates_parents() {
        let dir = std::env::temp_dir().join(format!("bench-report-{}", std::process::id()));
        let nested = dir.join("a").join("b");
        let path = nested.join("report.json");
        let r = sample_report();
        r.save_to(&path).unwrap();
        assert!(path.is_file());
        assert_eq!(Report::load_from(&path).unwrap(), r);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_file_name_contains_profile_and_timestamp() {
        let r = sample_report();
        let name = r.default_file_name();
        assert!(name.starts_with("bench-quick-"));
        assert!(name.ends_with(".json"));
        assert_eq!(
            r.resolve_out_path(Path::new("/tmp/out")),
            Path::new("/tmp/out").join(name)
        );
    }
}
