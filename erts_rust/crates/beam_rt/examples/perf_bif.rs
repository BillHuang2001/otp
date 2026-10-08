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

//! `perf_bif` - a dependency-free (std-only) **performance-regression harness** for the
//! `beam_rt` runtime: it measures the *marginal* per-iteration cost of a compiled Erlang
//! loop and prints it as a table, so a change to the runtime can be compared against a
//! recorded baseline instead of a feeling.
//!
//! # Recipe
//!
//! ```text
//! erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt
//! erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt --example perf_bif
//! erts_rust/scripts/cargo.sh run   --manifest-path erts_rust/Cargo.toml --release -p beam_rt --example perf_bif
//! ```
//!
//! The harness drives the `beam_rt` **binary** (not the example), so that binary must be
//! built too; both commands must run from the repository root so the example finds
//! `<target>/<profile>/beam_rt` next to itself.
//!
//! # The fixture
//!
//! ```text
//! add(0, A)  -> A;
//! add(N, A)  -> add(N-1, A).
//!
//! loop(0, A) -> A;
//! loop(N, A) -> loop(N-1, A+1).
//! ```
//!
//! `pc:add/2` performs **one** arithmetic operation per iteration, `pc:loop/2` **two**.
//! Both are tail-recursive, so an iteration is one trip around a compiled loop.
//!
//! # What the `ns/call` number is (and is NOT)
//!
//! Disassembling the compiled fixture (`beam_disasm:file/1`) shows that `N-1` / `N+1` do
//! **not** compile to a `call_ext erlang:'-'/2` BIF call. They compile to a `gc_bif`
//! arithmetic **instruction** (`{gc_bif,'-',{f,0},2,[{x,0},{integer,1}],{x,0}}`), and the
//! recursive call compiles to a local `call_only` tail call.
//!
//! `beam_rt run` enters the engine on the **baseline JIT tier** (`Tier::BaselineJit`,
//! always available). That tier's single bounded optimisation is the guarded small-int
//! `gc_bif` fast path for `+`/`-`/`*`: the instruction is emitted as a handful of native
//! instructions and the local `call_only` is linked natively. So `ns/call` here is the
//! cost of **one iteration of a natively compiled loop** - the guard `test`, the
//! instruction's arithmetic (inlined once for `add`, twice for `loop`) and the native tail
//! call.
//!
//! It is therefore **not** a registered-BIF dispatch round trip. The runtime only routes a
//! `gc_bif` through `ExecutionContext::call_bif` (NIF check -> native floor -> registry
//! lookup -> BIF) when the baseline tier cannot inline it; that path measures roughly two
//! orders of magnitude more per iteration (verified with a non-inlinable BIF in the same
//! loop shape). A regression in BIF dispatch / atom interning / term construction moves
//! this figure only once the tier stops inlining, i.e. as a step change rather than a
//! slope. The `pc:loop - pc:add` difference isolates **one further natively inlined
//! small-int arithmetic operation** (empirically ~0.1 ns, about a third of a cycle), not
//! one further BIF call.
//!
//! # Why a two-point slope (and not `t(N) - t(0)`)
//!
//! A single process pays ~15-20 ms of start-up (boot, module load, scheduling). Measuring
//! the workload at `N` and subtracting an `N = 0` run folds that start-up's *jitter*
//! (+/- several ms) straight into the result - and because the whole workload is often
//! *smaller* than that jitter, the figure is pure noise (it can even go negative). The
//! harness therefore cancels the constant start-up **exactly** with a slope across two
//! large points:
//!
//! ```text
//! ns/call = (t(N2) - t(N1)) / (N2 - N1)
//! ```
//!
//! The per-process cost is added to both `t(N1)` and `t(N2)` and cancels in the
//! difference. Both points are large enough that the *work* dominates the start-up jitter
//! (see [`DEFAULT_N1`]), and each point is the **fastest of [`BEST_OF`]** timed runs after
//! [`WARMUP`] discarded warm-up runs: a timing can only be perturbed upwards, so the
//! minimum is the best estimator available, and the warm-up absorbs the JIT / code-cache /
//! filesystem cost of the first run.
//!
//! # Never a negative slope
//!
//! If `t(N2) <= t(N1)` at a point, the workload is below this host's measurement
//! resolution there and the harness does **not** print a non-positive figure. It scales
//! both points up by [`GROWTH_FACTOR`] (at most [`MAX_GROWTH`] times, and never past
//! [`MAX_N1`]) and re-measures; if the slope is still not positive it **fails loudly**
//! (exit 1) rather than reporting a number that means nothing. The `pc:loop - pc:add`
//! "margin of one further arithmetic op" is computed the same way, from the two
//! calibrated slopes, and is reported as *below measurement resolution* when their
//! difference is not resolvable - never as a negative number.
//!
//! # Configuration
//!
//! * `<N1>` - the optional first argument: the **low** iteration count of the slope
//!   (default [`DEFAULT_N1`]); the high point is `N1 * SPAN_FACTOR`. A small `N1` is
//!   auto-scaled up (see above) and is more likely to be unresolvable.
//! * `$PERF_BASELINE_ADD_NS` / `$PERF_BASELINE_LOOP_NS` - a recorded `ns/call` number per
//!   workload. When either is set, the table gains the
//!   `baseline_ns | delta_ns | speedup` columns; this is the before/after mechanism.
//!   **Compare only against a baseline recorded with the same slope points:** the figure
//!   is not completely independent of `N` (the loop's memory footprint grows with the
//!   iteration count), so a run at other points is a different measurement.
//! * `$ERTS_STDLIB_DIR` - passed to the child when set, else discovered *cheaply* from the
//!   `erl` on `$PATH` (a filesystem walk - never a spawn), because an unset value costs
//!   the child one `erl` start-up per run.
//!
//! A missing prerequisite (`erlc`, a built runtime binary) is a **loud skip that exits 0**
//! - never a silent pass, and never a panic. A workload that does not print the value it
//! is supposed to return, or a child that fails, is a genuine error (exit 1, or 2 for a
//! malformed invocation).

#![deny(unsafe_code)]
#![warn(missing_docs)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::time::{Duration, Instant};

/// The fixture module, compiled at run time into the temp directory.
///
/// Both functions take the iteration count **and** the accumulator the tail call carries:
/// `run` requires the materialized argument count to equal the declared arity, and the
/// harness always passes [`ACCUMULATOR`]. `pc:add/2` then returns `0` and `pc:loop/2`
/// returns `N`, which is what the harness checks each run's stdout against - the workload
/// is its own self-check.
const PC_SOURCE: &str = "\
-module(pc).
-export([add/2, loop/2]).

add(0, A) -> A;
add(N, A) -> add(N-1, A).

loop(0, A) -> A;
loop(N, A) -> loop(N-1, A+1).
";

/// The default **low** iteration count of the slope (the first command-line argument
/// overrides it); the high point is `N1 * SPAN_FACTOR`.
///
/// Both points must be large enough that the run's *work* outweighs the ~15-20 ms of
/// per-process start-up whose jitter the slope has to survive. On the reference host one
/// iteration costs ~2.5 ns, so `N1 = 16_000_000` is ~40 ms of work against ~17 ms of
/// start-up and `N2 = 64_000_000` is ~160 ms; the 48M-iteration span makes the slope
/// reproducible to a few percent run to run. Tuned empirically: a `1_000_000 /
/// 4_000_000` pair leaves the work (a few ms) *below* the start-up jitter and the slope is
/// not reproducible at all.
const DEFAULT_N1: u64 = 16_000_000;
/// The **high** point of the slope is `N1 * SPAN_FACTOR`.
const SPAN_FACTOR: u64 = 4;
/// How many timed runs are kept at each point; the **fastest** is used (a timing can only
/// be perturbed upwards, so the minimum is the best estimate available).
const BEST_OF: usize = 5;
/// How many runs are discarded (untimed) at each point, to warm the JIT / code cache and
/// the filesystem before the measured samples.
const WARMUP: usize = 1;
/// How many times the slope's points may be scaled up when no positive slope is found.
const MAX_GROWTH: u32 = 3;
/// The factor each auto-scale multiplies the low point by.
const GROWTH_FACTOR: u64 = 4;
/// An absolute cap on the auto-scaled low point: a runaway scale would run for minutes.
const MAX_N1: u64 = 256_000_000;
/// The second argument every workload is called with (see [`PC_SOURCE`]).
const ACCUMULATOR: &str = "0";

/// One measured workload: the fixture function and how to recognize its result.
struct Workload {
    /// The name the table prints, e.g. `pc:add`.
    label: &'static str,
    /// The `module:function/arity` spec `run` is given, e.g. `pc:add/2`.
    spec: &'static str,
    /// The environment variable holding this workload's recorded baseline (ns/call).
    baseline_var: &'static str,
    /// The stdout `run` must print for `N` iterations.
    expected: fn(u64) -> String,
}

/// The two workloads. `add` does one arithmetic op per iteration, `loop` two, so the
/// difference of their slopes is the marginal cost of a single further arithmetic op.
const WORKLOADS: [Workload; 2] = [
    Workload {
        label: "pc:add",
        spec: "pc:add/2",
        baseline_var: "PERF_BASELINE_ADD_NS",
        expected: |_n| "0".to_string(),
    },
    Workload {
        label: "pc:loop",
        spec: "pc:loop/2",
        baseline_var: "PERF_BASELINE_LOOP_NS",
        expected: |n| n.to_string(),
    },
];

/// One workload's calibrated slope.
struct Row {
    /// The workload.
    workload: &'static Workload,
    /// The low iteration count (`t(N1)`).
    n1: u64,
    /// The high iteration count (`t(N2)`).
    n2: u64,
    /// The fastest measured wall clock at `N1`.
    at_n1: Duration,
    /// The fastest measured wall clock at `N2`.
    at_n2: Duration,
    /// The recorded baseline for this workload, when the environment supplies one.
    baseline: Option<f64>,
}

impl Row {
    /// The wall-clock span between the two points (`t(N2) > t(N1)` by construction).
    fn span(&self) -> Duration {
        self.at_n2.saturating_sub(self.at_n1)
    }

    /// The marginal cost of one iteration, in nanoseconds.
    ///
    /// [`measure`] only returns a row once `t(N2) > t(N1)` and `N2 > N1`, so the span is at
    /// least one nanosecond and this is finite and strictly positive - the harness never
    /// prints a non-positive slope.
    fn ns_per_call(&self) -> f64 {
        self.span().as_nanos() as f64 / (self.n2 - self.n1) as f64
    }
}

/// How the harness ended. A prerequisite that is absent is a `Skip` (a loud banner and
/// exit 0); everything else is a failure the caller reports.
enum Failure {
    /// A prerequisite is missing - loud skip, exit 0, never a silent pass.
    Skip(String),
    /// The invocation itself was malformed - exit 2 with the usage text.
    Usage(String),
    /// `-h`/`--help` - print the usage text and exit 0.
    Help,
    /// The measurement could not be taken or the workload misbehaved - exit 1.
    Error(String),
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(Failure::Skip(why)) => skip_loudly(&why),
        Err(Failure::Help) => print_usage(),
        Err(Failure::Usage(why)) => {
            eprintln!("perf_bif: {why}");
            print_usage();
            process::exit(2);
        }
        Err(Failure::Error(why)) => {
            eprintln!("perf_bif: {why}");
            process::exit(1);
        }
    }
}

/// Measures every workload and prints the slope table.
fn run() -> Result<(), Failure> {
    let n1 = low_iteration_count()?;

    let erlc = find_on_path("erlc").ok_or_else(|| {
        Failure::Skip(
            "no `erlc` on $PATH, so the fixture module cannot be compiled".to_string(),
        )
    })?;

    let dir = TempDir::create()?;
    let beam = compile_fixture(&erlc, dir.path())?;
    let runtime = locate_runtime()?;
    let stdlib = discover_stdlib();

    print_header(&runtime, &beam, n1, stdlib.as_deref());

    let mut rows = Vec::with_capacity(WORKLOADS.len());
    for workload in &WORKLOADS {
        rows.push(measure(
            &runtime,
            dir.path(),
            &beam,
            workload,
            n1,
            stdlib.as_deref(),
        )?);
    }

    print_table(&rows);
    print_marginal(&rows);
    if rows.iter().any(|row| row.baseline.is_some()) {
        println!(
            "speedup > 1.00x means faster than the recorded baseline; delta_ns is measured - \
             baseline, so only compare runs with the same slope points. The two slopes are the \
             LOW/N2 points named per row and may differ if a row was auto-scaled."
        );
    }

    Ok(())
}

/// Prints the run's configuration and how to read the `ns/call` column.
fn print_header(runtime: &Path, beam: &Path, n1: u64, stdlib: Option<&Path>) {
    println!("beam_rt performance-regression harness - marginal per-iteration cost");
    println!("  runtime:          {}", runtime.display());
    println!("  fixture:          {}", beam.display());
    println!(
        "  slope:            ns/call = (t(N2) - t(N1)) / (N2 - N1), with N2 = {SPAN_FACTOR} x N1"
    );
    println!("  requested N1:     {n1} (auto-scaled up when the slope is not positive)");
    println!(
        "  samples:          fastest of {BEST_OF} timed runs at each point ({WARMUP} warm-up discarded)"
    );
    println!(
        "  ERTS_STDLIB_DIR:  {}",
        match stdlib {
            Some(dir) => dir.display().to_string(),
            None => "<unset: the child spawns `erl` once per run>".to_string(),
        }
    );
    println!();
    println!("  pc.beam lowers `N-1`/`N+1` to a gc_bif arithmetic INSTRUCTION (not a `call_ext` BIF");
    println!("  call) and the recursion to a local `call_only`. `run` executes on the baseline JIT");
    println!("  tier, which natively inlines the small-int gc_bif fast path - so `ns/call` is the");
    println!("  cost of one iteration of a NATIVE loop, not a registered-BIF dispatch round trip.");
    println!();
}

/// Prints the slope table (one row per workload).
fn print_table(rows: &[Row]) {
    let with_baseline = rows.iter().any(|row| row.baseline.is_some());
    if with_baseline {
        println!(
            "{:<10} {:>12} {:>12} {:>10} {:>10} {:>10} {:>12} {:>12} {:>12} {:>9}",
            "module",
            "N1",
            "N2",
            "t(N1)_ms",
            "t(N2)_ms",
            "span_ms",
            "ns/call",
            "baseline_ns",
            "delta_ns",
            "speedup"
        );
    } else {
        println!(
            "{:<10} {:>12} {:>12} {:>10} {:>10} {:>10} {:>12}",
            "module", "N1", "N2", "t(N1)_ms", "t(N2)_ms", "span_ms", "ns/call"
        );
    }
    for row in rows {
        let ns_per_call = row.ns_per_call();
        let columns = if with_baseline {
            baseline_columns(ns_per_call, row.baseline)
        } else {
            String::new()
        };
        println!(
            "{:<10} {:>12} {:>12} {:>10.1} {:>10.1} {:>10.1} {:>12.2}{columns}",
            row.workload.label,
            row.n1,
            row.n2,
            milliseconds(row.at_n1),
            milliseconds(row.at_n2),
            milliseconds(row.span()),
            ns_per_call,
        );
    }
    println!();
}

/// Prints the slope difference between the two workloads.
///
/// `pc:loop` runs exactly one more arithmetic op per iteration than `pc:add`, so the
/// difference of their (robust) slopes isolates a single further inlined arithmetic op.
/// Because both slopes are calibrated the same way, the difference never goes negative by
/// itself; if it is not resolvable the harness says so instead of printing a number.
fn print_marginal(rows: &[Row]) {
    if let [add, lp] = rows {
        let difference = lp.ns_per_call() - add.ns_per_call();
        if difference > 0.0 {
            println!(
                "marginal cost of one further arithmetic op (pc:loop - pc:add): {difference:.2} ns/call"
            );
        } else {
            println!(
                "marginal cost of one further arithmetic op (pc:loop - pc:add): below measurement \
                 resolution at these points (the two slopes differ by {difference:.2} ns/call)"
            );
        }
    }
}

/// Prints the recorded baseline for `var`, or `None` when it is unset or unparsable.
///
/// A malformed baseline is reported and ignored - the measurement itself is still valid,
/// and inventing a comparison value would be worse than omitting the column for that row.
fn baseline_ns(var: &str) -> Option<f64> {
    let raw = match env::var(var) {
        Ok(raw) => raw,
        Err(env::VarError::NotPresent) => return None,
        Err(env::VarError::NotUnicode(_)) => {
            eprintln!("warning: ${var} is not valid UTF-8; ignoring it");
            return None;
        }
    };
    match raw.trim().parse::<f64>() {
        Ok(value) if value.is_finite() => Some(value),
        _ => {
            eprintln!("warning: ${var}={raw:?} is not a nanosecond count; ignoring it");
            None
        }
    }
}

/// The `baseline_ns | delta_ns | speedup` columns for one row (right-aligned), or dashes
/// when no baseline is recorded for this workload.
fn baseline_columns(measured_ns: f64, baseline: Option<f64>) -> String {
    match baseline {
        Some(baseline) => format!(
            " {:>12.1} {:>12.1} {:>9}",
            baseline,
            measured_ns - baseline,
            format!("{:.2}x", baseline / measured_ns)
        ),
        None => format!(" {:>12} {:>12} {:>9}", "-", "-", "-"),
    }
}

/// One timed run of the runtime.
struct Timed {
    /// Wall clock of the whole child process.
    wall: Duration,
    /// The child's trimmed stdout.
    stdout: String,
}

/// Calibrates `workload`'s slope, auto-scaling the points up when no positive slope can be
/// measured.
///
/// Returns a [`Row`] only for a **strictly positive** slope: `t(N2) > t(N1)`. A flat or
/// negative slope means the work is below the host's measurement resolution at the tried
/// points, so rather than folding that into a meaningless figure the points are scaled up
/// (by [`GROWTH_FACTOR`], at most [`MAX_GROWTH`] times, never past [`MAX_N1`]) and
/// re-measured. If every tried point is flat the measurement **fails loudly**.
fn measure(
    runtime: &Path,
    code_dir: &Path,
    beam: &Path,
    workload: &'static Workload,
    base_n1: u64,
    stdlib: Option<&Path>,
) -> Result<Row, Failure> {
    let mut n1 = base_n1;
    let mut growth = 0u32;
    loop {
        let n2 = n1.checked_mul(SPAN_FACTOR).ok_or_else(|| {
            Failure::Error(format!(
                "{}: N1={n1} x {SPAN_FACTOR} overflows a 64-bit iteration count",
                workload.label
            ))
        })?;
        let at_n1 = best_of(runtime, code_dir, beam, workload, n1, stdlib)?;
        let at_n2 = best_of(runtime, code_dir, beam, workload, n2, stdlib)?;
        if at_n2 > at_n1 {
            return Ok(Row {
                workload,
                n1,
                n2,
                at_n1,
                at_n2,
                baseline: baseline_ns(workload.baseline_var),
            });
        }
        let grown = n1.saturating_mul(GROWTH_FACTOR);
        if growth >= MAX_GROWTH || grown <= n1 || grown > MAX_N1 {
            return Err(Failure::Error(format!(
                "{}: no positive slope at any iteration count tried (up to N1={n1}): t(N2)={:.3} \
                 ms never exceeded t(N1)={:.3} ms. The workload is below this host's measurement \
                 resolution; refusing to report a non-positive slope.",
                workload.label,
                milliseconds(at_n2),
                milliseconds(at_n1),
            )));
        }
        eprintln!(
            "note: {}: t(N2) ({:.3} ms) did not exceed t(N1) ({:.3} ms) at N1={n1}; scaling the \
             slope's points up to N1={grown}",
            workload.label,
            milliseconds(at_n2),
            milliseconds(at_n1),
        );
        n1 = grown;
        growth += 1;
    }
}

/// Runs one workload `WARMUP + BEST_OF` times and returns the fastest **timed** run.
///
/// Every run (including the discarded warm-ups) is also the self-check: `run` prints the
/// value the fixture returns (`0` for `pc:add/2`, `N` for `pc:loop/2`), so a run printing
/// anything else is a genuine failure rather than a slow measurement.
fn best_of(
    runtime: &Path,
    code_dir: &Path,
    beam: &Path,
    workload: &Workload,
    n: u64,
    stdlib: Option<&Path>,
) -> Result<Duration, Failure> {
    let expected = (workload.expected)(n);
    let mut best: Option<Duration> = None;
    for index in 0..(WARMUP + BEST_OF) {
        let timed = run_once(runtime, code_dir, beam, workload.spec, n, stdlib)?;
        if timed.stdout != expected {
            return Err(Failure::Error(format!(
                "`{}` printed {:?} for N={n}, expected {expected:?} - the workload did not \
                 run as intended, so its timing means nothing",
                workload.spec, timed.stdout
            )));
        }
        if index >= WARMUP {
            best = Some(match best {
                Some(current) if current <= timed.wall => current,
                _ => timed.wall,
            });
        }
    }
    best.ok_or_else(|| Failure::Error("internal: no timed run was recorded".to_string()))
}

/// Runs the runtime once and times it.
///
/// The command is `<beam_rt> -pa <dir> run <dir>/pc.beam <spec> <N> 0`. The `run` action is
/// itself bounded by the runtime's own `RUN_DEADLINE`, so a child always terminates; a
/// non-zero exit is reported with the child's stderr.
fn run_once(
    runtime: &Path,
    code_dir: &Path,
    beam: &Path,
    spec: &str,
    n: u64,
    stdlib: Option<&Path>,
) -> Result<Timed, Failure> {
    let mut command = Command::new(runtime);
    command
        .arg("-pa")
        .arg(code_dir)
        .arg("run")
        .arg(beam)
        .arg(spec)
        .arg(n.to_string())
        .arg(ACCUMULATOR)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = stdlib {
        command.env("ERTS_STDLIB_DIR", dir);
    }

    let started = Instant::now();
    let output = command.output();
    let wall = started.elapsed();

    let output = output.map_err(|error| {
        Failure::Error(format!("cannot run `{}`: {error}", runtime.display()))
    })?;
    if !output.status.success() {
        return Err(Failure::Error(format!(
            "`{} -pa {} run {} {spec} {n} {ACCUMULATOR}` exited with {:?}\n--- stderr ---\n{}",
            runtime.display(),
            code_dir.display(),
            beam.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim_end(),
        )));
    }
    Ok(Timed {
        wall,
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
    })
}

/// Writes [`PC_SOURCE`] into `dir` and compiles it there with `erlc`.
///
/// A non-zero `erlc` status is an error (the toolchain exists but rejected the fixture -
/// that is a real problem, not a skip), reported with `erlc`'s stderr.
fn compile_fixture(erlc: &Path, dir: &Path) -> Result<PathBuf, Failure> {
    let source = dir.join("pc.erl");
    fs::write(&source, PC_SOURCE)
        .map_err(|error| Failure::Error(format!("cannot write {}: {error}", source.display())))?;

    let output = Command::new(erlc)
        .arg("-o")
        .arg(dir)
        .arg(&source)
        .output()
        .map_err(|error| Failure::Error(format!("cannot run `{}`: {error}", erlc.display())))?;
    if !output.status.success() {
        return Err(Failure::Error(format!(
            "`{} -o {} {}` exited with {:?}\n--- stderr ---\n{}",
            erlc.display(),
            dir.display(),
            source.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim_end(),
        )));
    }

    let beam = dir.join("pc.beam");
    if !beam.is_file() {
        return Err(Failure::Error(format!(
            "`erlc` reported success but {} does not exist",
            beam.display()
        )));
    }
    Ok(beam)
}

/// The built `beam_rt` **binary** for the profile this example was built in.
///
/// The example's own path is `<target>/<profile>/examples/perf_bif`, so the binary is
/// `<target>/<profile>/beam_rt`; the other profile is tried as a fallback (a debug
/// example still runs against a release runtime when the debug binary has not been
/// built). This works for `--target <triple>` and a custom `CARGO_TARGET_DIR` too, since
/// the path is derived from the running executable rather than assumed.
fn locate_runtime() -> Result<PathBuf, Failure> {
    let mut candidates = Vec::new();
    if let Ok(example) = env::current_exe() {
        if let Some(profile_dir) = example.parent().and_then(Path::parent) {
            candidates.push(profile_dir.join("beam_rt"));
            if let Some(target_dir) = profile_dir.parent() {
                candidates.push(target_dir.join(other_profile(profile_dir)).join("beam_rt"));
            }
        }
    }
    if let Some(candidate) = candidates.iter().find(|path| is_executable(path)) {
        return Ok(candidate.clone());
    }
    let looked_for: Vec<String> = candidates
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    Err(Failure::Skip(format!(
        "no `beam_rt` runtime binary found (looked for: {})",
        if looked_for.is_empty() {
            "<cannot resolve this example's own path>".to_string()
        } else {
            looked_for.join(", ")
        }
    )))
}

/// `debug` for a `release` profile directory and vice versa (anything else pairs with
/// `release`), so the example can fall back to the other profile's runtime.
fn other_profile(profile_dir: &Path) -> &'static str {
    match profile_dir.file_name().and_then(|name| name.to_str()) {
        Some("release") => "debug",
        _ => "release",
    }
}

/// A `stdlib` application directory the child can be pointed at with `$ERTS_STDLIB_DIR`,
/// when one is discoverable **without running `erl`** - that spawn is precisely the
/// start-up cost this avoids.
///
/// `$ERTS_STDLIB_DIR` wins when it is already set and holds `lists.beam`; otherwise the
/// `erl` on `$PATH` is resolved to a filesystem path (never executed) and its ancestors
/// are scanned for `lib/stdlib-*/ebin/lists.beam` - the layout both a distro installation
/// and a Nix store use. `$ERL_TOP` is tried last. `None` means the child resolves the
/// code path itself (one `erl` spawn per run).
fn discover_stdlib() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("ERTS_STDLIB_DIR") {
        let path = PathBuf::from(dir);
        if has_lists_beam(&path) {
            return Some(path);
        }
        eprintln!(
            "warning: $ERTS_STDLIB_DIR ({}) does not contain lists.beam; leaving it to the runtime",
            path.display()
        );
        return None;
    }

    let mut roots = Vec::new();
    if let Some(erl) = find_on_path("erl") {
        roots.extend(ancestors(erl.parent().unwrap_or(Path::new(".")), 8));
    }
    if let Ok(example) = env::current_exe() {
        roots.extend(ancestors(example.parent().unwrap_or(Path::new(".")), 8));
    }
    if let Some(erl_top) = env::var_os("ERL_TOP") {
        roots.push(PathBuf::from(erl_top));
    }

    roots.into_iter().find_map(|root| stdlib_under(&root))
}

/// The `stdlib-*` application directory under `root` (checked as `root/lib/stdlib-*` and
/// as `root/stdlib-*`), when its `ebin` really holds `lists.beam`.
fn stdlib_under(root: &Path) -> Option<PathBuf> {
    if has_lists_beam(root) {
        return Some(root.to_path_buf());
    }
    for lib in [root.to_path_buf(), root.join("lib")] {
        let entries = match fs::read_dir(&lib) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        let found = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("stdlib-"))
            })
            .find(|path| has_lists_beam(path));
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Whether `dir` holds `lists.beam` - directly or under `ebin/` (the two forms
/// `$ERTS_STDLIB_DIR` accepts).
fn has_lists_beam(dir: &Path) -> bool {
    dir.join("lists.beam").is_file() || dir.join("ebin").join("lists.beam").is_file()
}

/// `path` and its parents, at most `limit` of them.
fn ancestors(path: &Path, limit: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut current = Some(path);
    while let Some(dir) = current {
        out.push(dir.to_path_buf());
        if out.len() >= limit {
            break;
        }
        current = dir.parent();
    }
    out
}

/// The **low** iteration count of the slope: the first argument, or [`DEFAULT_N1`].
fn low_iteration_count() -> Result<u64, Failure> {
    let mut args = env::args().skip(1);
    let raw = match args.next() {
        Some(raw) => raw,
        None => return Ok(DEFAULT_N1),
    };
    if raw == "-h" || raw == "--help" {
        return Err(Failure::Help);
    }
    if args.next().is_some() {
        return Err(Failure::Usage(
            "too many arguments (only the low iteration count is accepted)".to_string(),
        ));
    }
    let n = raw
        .parse::<u64>()
        .map_err(|_| Failure::Usage(format!("{raw:?} is not an iteration count")))?;
    if n == 0 {
        return Err(Failure::Usage(
            "the iteration count must be at least 1 (a zero-length slope has no per-call cost)"
                .to_string(),
        ));
    }
    Ok(n)
}

/// The loud skip banner - a missing prerequisite is never a silent pass.
fn skip_loudly(why: &str) {
    eprintln!();
    eprintln!("============ beam_rt perf_bif harness SKIPPED ============");
    eprintln!("{why}");
    eprintln!("Build the runtime and re-run:");
    eprintln!("  erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml \\");
    eprintln!("      --release -p beam_rt --example perf_bif");
    eprintln!("NOT a pass: nothing was measured.");
    eprintln!("==========================================================");
    eprintln!();
}

/// The usage text (`-h`/`--help`, and on a malformed invocation).
fn print_usage() {
    println!("usage: perf_bif [N1]");
    println!();
    println!(
        "  N1  the LOW iteration count of the slope, at least 1 (default {DEFAULT_N1});"
    );
    println!("      the high point is N1 * {SPAN_FACTOR}");
    println!();
    println!("environment:");
    println!("  PERF_BASELINE_ADD_NS   recorded ns/call for pc:add/2 (adds the comparison columns)");
    println!("  PERF_BASELINE_LOOP_NS  recorded ns/call for pc:loop/2 (adds the comparison columns)");
    println!("  ERTS_STDLIB_DIR        the system stdlib directory, else discovered from `erl`");
}

/// `duration` in milliseconds.
fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Whether `path` is an existing executable file.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match fs::metadata(path) {
            Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// The first `name` on `$PATH` that is an executable file (a filesystem probe - the
/// program is never run here).
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// The fixture directory, removed when the harness ends (including on an early skip).
struct TempDir(PathBuf);

impl TempDir {
    /// Creates `<tmp>/beam_rt-perf-bif-<pid>/`.
    fn create() -> Result<TempDir, Failure> {
        let base = env::temp_dir().join(format!("beam_rt-perf-bif-{}", process::id()));
        fs::create_dir_all(&base).map_err(|error| {
            Failure::Error(format!("cannot create {}: {error}", base.display()))
        })?;
        Ok(TempDir(base))
    }

    /// The directory.
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
