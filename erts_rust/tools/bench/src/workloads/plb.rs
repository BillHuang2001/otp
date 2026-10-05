//! `plb` — the Programming-Language-Benchmarks ("Benchmarks Game") compute kernels.
//!
//! Drives the repo's `dotnet`-based tool
//! (`dotnet run -c Release --project tool -- --task bench --langs elixir`) from its
//! `bench/` directory and reads the machine-readable results it emits under
//! `bench/build/_results/**/*.json` (`timeMS`, `memBytes`), which are summarized here.
//!
//! Repo: `$BENCH_PLB_REPO`, else `/home/bill/Source/Programming-Language-Benchmarks`.
//! `dotnet` is a hard prerequisite — when it is absent the workload is `skipped`
//! (`dotnet-absent`), never a fabricated number.

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

use crate::metrics::{Category, Metric, Probe, Runtimes};
use crate::proc;
use crate::report::WorkloadResult;
use crate::workload::{RunContext, Workload};
use crate::workloads::{collect_files, resolve_repo};

/// The documented default checkout path.
const DEFAULT_REPO: &str = "/home/bill/Source/Programming-Language-Benchmarks";

/// The PLB compute-kernel workload.
pub struct Plb;

fn repo() -> PathBuf {
    resolve_repo("plb", DEFAULT_REPO)
}

fn dotnet() -> Option<PathBuf> {
    proc::which("dotnet")
}

impl Workload for Plb {
    fn id(&self) -> &'static str {
        "plb"
    }

    fn description(&self) -> &'static str {
        "Programming-Language-Benchmarks kernels via the dotnet tool"
    }

    fn category(&self) -> Category {
        Category::Compute
    }

    fn contracts(&self) -> &'static [&'static str] {
        &["A.1", "A.6"]
    }

    fn runtimes(&self) -> Runtimes {
        Runtimes::SysOnly
    }

    fn probe(&self) -> Probe {
        if !repo().is_dir() {
            return Probe::skipped("repo-missing");
        }
        if dotnet().is_none() {
            return Probe::skipped("dotnet-absent");
        }
        Probe::Ready
    }

    fn run(&self, ctx: &RunContext) -> WorkloadResult {
        let kind = ctx.runtime.kind;
        let r = repo();
        if !r.is_dir() {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "repo-missing",
            );
        }
        let Some(dotnet) = dotnet() else {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "dotnet-absent",
            );
        };
        let bench_dir = r.join("bench");
        if !bench_dir.is_dir() {
            return WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "bench-dir-missing",
            );
        }

        let mut cmd = Command::new(&dotnet);
        cmd.current_dir(&bench_dir)
            .args(["run", "-c", "Release", "--project", "tool", "--"])
            .args(["--task", "bench", "--langs", "elixir"]);

        let out = match proc::run(&mut cmd) {
            Ok(o) => o,
            Err(e) => {
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    format!("dotnet-spawn-failed: {e}"),
                )
            }
        };

        let mut result = WorkloadResult::new(self.id(), kind, self.category(), self.description());
        result
            .metrics
            .push(Metric::scalar("wall_ms", "ms", out.wall_ms));
        result.peak_memory_bytes = out.peak_memory_bytes;

        let (times, mems) = read_results(&bench_dir.join("build/_results"));
        if times.is_empty() {
            result
                .notes
                .push(format!("no result JSON found (dotnet exit {:?})", out.exit_code));
            if !out.success {
                result.notes.push(format!("stderr: {}", out.stderr.trim()));
            }
        } else {
            result
                .metrics
                .push(Metric::samples("test_time_ms", "ms", &times));
            if let Some(max_mem) = mems.iter().copied().fold(None, |a: Option<f64>, b| {
                Some(match a {
                    Some(a) => a.max(b),
                    None => b,
                })
            }) {
                result
                    .metrics
                    .push(Metric::scalar("test_mem_bytes", "bytes", max_mem));
            }
        }
        result
    }
}

/// Reads `timeMS`/`memBytes` fields from every result JSON under `dir`.
fn read_results(dir: &std::path::Path) -> (Vec<f64>, Vec<f64>) {
    let mut times = Vec::new();
    let mut mems = Vec::new();
    for path in collect_files(dir, "json") {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        collect_number(&value, "timeMS", &mut times);
        collect_number(&value, "memBytes", &mut mems);
    }
    (times, mems)
}

/// Recursively collects numeric values stored under `key`.
fn collect_number(value: &Value, key: &str, out: &mut Vec<f64>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if k == key {
                    if let Some(n) = v.as_f64() {
                        out.push(n);
                    }
                }
                collect_number(v, key, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_number(item, key, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_stable() {
        let w = Plb;
        assert_eq!(w.id(), "plb");
        assert_eq!(w.category(), Category::Compute);
        assert!(matches!(w.runtimes(), Runtimes::SysOnly));
    }

    #[test]
    fn probe_reports_dotnet_absence() {
        let p = Plb.probe();
        if dotnet().is_none() && repo().is_dir() {
            assert_eq!(p.reason(), Some("dotnet-absent"));
        }
    }

    #[test]
    fn extracts_numbers_recursively() {
        let v: Value = serde_json::from_str(
            r#"{"results":[{"timeMS":12.5,"memBytes":100},{"timeMS":7.5,"nested":{"timeMS":3.0}}]}"#,
        )
        .unwrap();
        let mut times = Vec::new();
        collect_number(&v, "timeMS", &mut times);
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(times, vec![3.0, 7.5, 12.5]);
        let mut mems = Vec::new();
        collect_number(&v, "memBytes", &mut mems);
        assert_eq!(mems, vec![100.0]);
    }

    #[test]
    fn read_results_handles_missing_dir() {
        let (t, m) = read_results(std::path::Path::new("/nonexistent/xyzzy"));
        assert!(t.is_empty() && m.is_empty());
    }
}
