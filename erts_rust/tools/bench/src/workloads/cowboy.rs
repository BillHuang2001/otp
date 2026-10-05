//! `cowboy` — the HTTP I/O-bound workload (the Cowboy `hello_world` example).
//!
//! Starts `make run` in `examples/hello_world` (which boots a Cowboy server on port
//! 8080), waits for the port to accept connections, drives it with the first available
//! load generator (`wrk` → `h2load` → `bombardier`), records latency/throughput, then
//! stops the server.
//!
//! Repo: `$BENCH_COWBOY_REPO`, else `/home/bill/Source/cowboy`. When **no** load
//! generator is installed the workload is `skipped` (`no-http-load-generator`) rather
//! than reporting a bogus number.

use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use crate::metrics::{Category, Metric, Probe, Runtimes};
use crate::proc;
use crate::report::WorkloadResult;
use crate::workload::{RunContext, Workload};
use crate::workloads::resolve_repo;

/// The documented default checkout path.
const DEFAULT_REPO: &str = "/home/bill/Source/cowboy";
/// The example URL driven by the load generator.
const URL: &str = "http://localhost:8080/";
/// How long to wait for the server to start accepting connections.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// A discovered HTTP load generator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Generator {
    Wrk,
    H2load,
    Bombardier,
}

impl Generator {
    fn name(&self) -> &'static str {
        match self {
            Generator::Wrk => "wrk",
            Generator::H2load => "h2load",
            Generator::Bombardier => "bombardier",
        }
    }

    /// Builds the timed-load command line for each generator.
    fn command(&self, program: &PathBuf) -> Command {
        let mut cmd = Command::new(program);
        match self {
            Generator::Wrk => {
                cmd.args(["-t", "2", "-c", "10", "-d", "5s", URL]);
            }
            Generator::H2load => {
                cmd.args(["-n", "10000", "-c", "10", URL]);
            }
            Generator::Bombardier => {
                cmd.args(["-c", "10", "-n", "10000", URL]);
            }
        }
        cmd
    }
}

/// The first available load generator, with its resolved path.
fn find_generator() -> Option<(Generator, PathBuf)> {
    for (gen, prog) in [
        (Generator::Wrk, "wrk"),
        (Generator::H2load, "h2load"),
        (Generator::Bombardier, "bombardier"),
    ] {
        if let Some(path) = proc::which(prog) {
            return Some((gen, path));
        }
    }
    None
}

/// The Cowboy HTTP workload.
pub struct Cowboy;

fn repo() -> PathBuf {
    resolve_repo("cowboy", DEFAULT_REPO)
}

impl Workload for Cowboy {
    fn id(&self) -> &'static str {
        "cowboy"
    }

    fn description(&self) -> &'static str {
        "Cowboy hello_world HTTP server driven by an external load generator"
    }

    fn category(&self) -> Category {
        Category::Io
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
        if proc::which("make").is_none() {
            return Probe::skipped("make-absent");
        }
        if find_generator().is_none() {
            return Probe::skipped("no-http-load-generator");
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
        let Some(make) = proc::which("make") else {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "make-absent",
            );
        };
        let Some((generator, generator_path)) = find_generator() else {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "no-http-load-generator",
            );
        };
        let example = r.join("examples/hello_world");
        if !example.is_dir() {
            return WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "example-dir-missing",
            );
        }

        let mut server_cmd = Command::new(&make);
        server_cmd.current_dir(&example).arg("run");
        let mut server: Child = match proc::spawn(&mut server_cmd) {
            Ok(c) => c,
            Err(e) => {
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    format!("server-spawn-failed: {e}"),
                )
            }
        };

        if !wait_for_port(8080, READY_TIMEOUT) {
            let _ = server.kill();
            let _ = server.wait();
            return WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "server-not-ready",
            );
        }

        let mut load_cmd = generator.command(&generator_path);
        let load = proc::run(&mut load_cmd);

        let _ = server.kill();
        let _ = server.wait();

        let out = match load {
            Ok(o) => o,
            Err(e) => {
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    format!("generator-spawn-failed: {e}"),
                )
            }
        };

        let mut result = WorkloadResult::new(self.id(), kind, self.category(), self.description());
        result
            .metrics
            .push(Metric::scalar("wall_ms", "ms", out.wall_ms));
        result
            .metrics
            .push(Metric::scalar("generator_exit", "code", out.exit_code.unwrap_or(-1) as f64));
        if let Some(rps) = parse_requests_per_sec(generator, &out.stdout) {
            result
                .metrics
                .push(Metric::scalar("requests_per_sec", "req/s", rps));
        } else {
            result
                .notes
                .push(format!("could not parse throughput from {} output", generator.name()));
        }
        result
    }
}

/// Polls `127.0.0.1:<port>` until it accepts a connection or the timeout elapses.
fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Best-effort throughput extraction from the generator's output.
fn parse_requests_per_sec(generator: Generator, stdout: &str) -> Option<f64> {
    match generator {
        Generator::Wrk => {
            // wrk prints e.g. "Requests/sec:   123456.78".
            let line = stdout
                .lines()
                .find(|l| l.trim_start().starts_with("Requests/sec:"))?;
            line.split_whitespace().last()?.parse().ok()
        }
        Generator::H2load => {
            // h2load prints "finished in 5.00s, 2000.00 req/s, 1.00MB/s".
            let idx = stdout.find(" req/s")?;
            let head = &stdout[..idx];
            let start = head.rfind(|c: char| c == ',' || c.is_whitespace()).map(|i| i + 1).unwrap_or(0);
            head[start..].parse().ok()
        }
        Generator::Bombardier => {
            // bombardier prints e.g. "  Reqs/sec     12345.67    100.0    20000.0"
            // (Avg, Stdev, Max); the average is the first number after the label.
            let line = stdout
                .lines()
                .find(|l| l.trim_start().starts_with("Reqs/sec"))?;
            line.split_whitespace().nth(1)?.parse().ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_stable() {
        let w = Cowboy;
        assert_eq!(w.id(), "cowboy");
        assert_eq!(w.category(), Category::Io);
        assert!(matches!(w.runtimes(), Runtimes::SysOnly));
    }

    #[test]
    fn probe_reports_missing_generator() {
        let p = Cowboy.probe();
        if repo().is_dir() && proc::which("make").is_some() && find_generator().is_none() {
            assert_eq!(p.reason(), Some("no-http-load-generator"));
        }
    }

    #[test]
    fn parses_wrk_throughput() {
        let sample = "Running 5s test @ http://localhost:8080/\n  2 threads and 10 connections\nRequests/sec:  98765.43\nTransfer/sec: 12.34MB\n";
        assert_eq!(
            parse_requests_per_sec(Generator::Wrk, sample),
            Some(98765.43)
        );
        assert_eq!(parse_requests_per_sec(Generator::Wrk, "no data"), None);
    }

    #[test]
    fn parses_bombardier_throughput() {
        let sample = "Statistics        Avg      Stdev        Max\n  Reqs/sec     12345.67    100.0    20000.0\n";
        assert_eq!(
            parse_requests_per_sec(Generator::Bombardier, sample),
            Some(12345.67)
        );
    }

    #[test]
    fn parses_h2load_throughput() {
        let sample = "finished in 5.00s, 2000.00 req/s, 1.00MB/s\n";
        assert_eq!(
            parse_requests_per_sec(Generator::H2load, sample),
            Some(2000.00)
        );
    }

    #[test]
    fn wait_for_port_times_out_on_a_closed_port() {
        // Port 1 is essentially never open; a 200ms timeout keeps the test fast.
        assert!(!wait_for_port(1, Duration::from_millis(200)));
    }
}
