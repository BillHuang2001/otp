//! `perf_bif` - a dependency-free (std-only) **performance-regression harness** for the
//! `beam_rt` runtime: it measures the *marginal* per-call cost of a compiled Erlang loop
//! and prints it as a table, so a change to the runtime can be compared against a recorded
//! baseline instead of a feeling.
//!
//! # Recipe
//!
//! ```text
//! erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt --example perf_bif
//! erts_rust/scripts/cargo.sh run   --manifest-path erts_rust/Cargo.toml --release -p beam_rt --example perf_bif
//! ```
//!
//! The `build` line is also what produces the runtime binary this harness drives (the
//! `run` line rebuilds and runs it); both must be run from the repository root so that the
//! example finds `<target>/<profile>/beam_rt` next to itself.
//!
//! # What is measured, and why the figure is *marginal*
//!
//! The fixture module - embedded below as a string and compiled at run time with `erlc`
//! into a temp directory, so no path resolution is needed - is a tail-recursive counter:
//!
//! * `pc:add/2` performs **one** arithmetic operation per iteration (`N-1`);
//! * `pc:loop/2` performs **two** (`N-1` and `A+1`).
//!
//! Each iteration is therefore one registered-BIF dispatch round trip for `add` and two
//! for `loop`, and the *slope difference* between the two is the marginal cost of one
//! BIF call - the number that moves when dispatch, atom interning or term construction
//! regress.
//!
//! Every workload is run at `N = 0` and at `N = <large>`, best-of-k wall clock, and the
//! reported figure is
//!
//! ```text
//! ns/call = (t_large - t_0) / (N_large - N_0)
//! ```
//!
//! The `N = 0` run pays a full process start-up (boot, module load, scheduling) and
//! nothing else, so subtracting it removes the ~10-150 ms of per-process overhead from
//! the per-call figure instead of folding it into it.
//!
//! # Configuration
//!
//! * `<N>` - the optional first argument: the iteration count (default `200_000`).
//! * `$PERF_BASELINE_ADD_NS` / `$PERF_BASELINE_LOOP_NS` - a recorded `ns/call` integer per
//!   workload. When either is set, the table gains the
//!   `baseline_ns | delta_ns | speedup` columns; this is the before/after mechanism.
//!   **Compare only against a baseline recorded at the same `N`:** the per-call figure is
//!   not completely `N`-independent (the loop's memory footprint grows with the
//!   iteration count), so a run at another `N` is a different measurement.
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

/// The default iteration count (the first command-line argument overrides it).
const DEFAULT_N: u64 = 200_000;
/// How often each measurement is repeated; the **fastest** run is reported (a timing can
/// only be perturbed upwards by the host, so the minimum is the best estimate available).
const BEST_OF: usize = 3;
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
/// difference of their slopes is the marginal cost of a single BIF call.
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

/// One workload's measurements.
struct Row {
    /// The workload.
    workload: &'static Workload,
    /// The calibration run's wall clock (`N = 0`): per-process overhead only.
    at_zero: Duration,
    /// The measured run's wall clock (`N = n`).
    at_n: Duration,
    /// The recorded baseline for this workload, when the environment supplies one.
    baseline: Option<f64>,
}

impl Row {
    /// The marginal cost of one iteration, in nanoseconds.
    ///
    /// `N >= 1` is enforced by [`iteration_count`], so this is always finite.
    fn ns_per_call(&self, n: u64) -> f64 {
        let delta = self.at_n.saturating_sub(self.at_zero);
        delta.as_nanos() as f64 / n as f64
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

/// Measures every workload and prints the two tables.
fn run() -> Result<(), Failure> {
    let n = iteration_count()?;

    let erlc = find_on_path("erlc").ok_or_else(|| {
        Failure::Skip(
            "no `erlc` on $PATH, so the fixture module cannot be compiled".to_string(),
        )
    })?;

    let dir = TempDir::create()?;
    let beam = compile_fixture(&erlc, dir.path())?;
    let runtime = locate_runtime()?;
    let stdlib = discover_stdlib();

    println!("beam_rt performance-regression harness - marginal per-call cost");
    println!("  runtime:          {}", runtime.display());
    println!("  fixture:          {}", beam.display());
    println!("  iterations:       {n} (best of {BEST_OF})");
    println!(
        "  ERTS_STDLIB_DIR:  {}",
        match stdlib.as_deref() {
            Some(dir) => dir.display().to_string(),
            None => "<unset: the child spawns `erl` once per run>".to_string(),
        }
    );
    println!();

    let mut rows = Vec::with_capacity(WORKLOADS.len());
    for workload in &WORKLOADS {
        let at_zero = best_of(&runtime, dir.path(), &beam, workload, 0, stdlib.as_deref())?;
        let at_n = best_of(&runtime, dir.path(), &beam, workload, n, stdlib.as_deref())?;
        rows.push(Row {
            workload,
            at_zero: at_zero.wall,
            at_n: at_n.wall,
            baseline: baseline_ns(workload.baseline_var),
        });
    }

    // The calibration block: what the `N = 0` run pays (per-process overhead), so the
    // subtraction behind every `ns/call` figure is visible rather than implied.
    println!("calibration (N=0: process start-up only, subtracted from every slope)");
    println!("{:<10} {:>8} {:>12}", "module", "N", "wall_ms");
    for row in &rows {
        println!(
            "{:<10} {:>8} {:>12.1}",
            row.workload.label,
            0,
            milliseconds(row.at_zero)
        );
    }
    println!();

    let with_baseline = rows.iter().any(|row| row.baseline.is_some());
    if with_baseline {
        println!(
            "{:<10} {:>8} {:>12} {:>14} {:>14} {:>14} {:>9}",
            "module", "N", "wall_ms", "ns/call", "baseline_ns", "delta_ns", "speedup"
        );
    } else {
        println!(
            "{:<10} {:>8} {:>12} {:>14}",
            "module", "N", "wall_ms", "ns/call"
        );
    }
    for row in &rows {
        let ns_per_call = row.ns_per_call(n);
        let columns = if with_baseline {
            baseline_columns(ns_per_call, row.baseline)
        } else {
            String::new()
        };
        println!(
            "{:<10} {:>8} {:>12.1} {:>14.1}{columns}",
            row.workload.label,
            n,
            milliseconds(row.at_n),
            ns_per_call,
        );
    }
    println!();

    // The slope difference: `loop` runs exactly one more arithmetic BIF per iteration
    // than `add`, so this isolates a single BIF-dispatch round trip.
    if let [add, lp] = rows.as_slice() {
        let difference = lp.ns_per_call(n) - add.ns_per_call(n);
        println!(
            "marginal cost of one further arithmetic BIF (pc:loop - pc:add): {difference:.1} ns/call"
        );
    }
    if with_baseline {
        println!(
            "speedup > 1.00x means faster than the recorded baseline; delta_ns is measured - \
             baseline, so only compare runs at the same N."
        );
    }

    Ok(())
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
            "{:>14.1} {:>14.1} {:>9}",
            baseline,
            measured_ns - baseline,
            format!("{:.2}x", baseline / measured_ns)
        ),
        None => format!("{:>14} {:>14} {:>9}", "-", "-", "-"),
    }
}

/// One timed run of the runtime.
struct Timed {
    /// Wall clock of the whole child process.
    wall: Duration,
    /// The child's trimmed stdout.
    stdout: String,
}

/// Runs one workload `BEST_OF` times and returns the fastest run.
///
/// Each run is also the self-check: `run` prints the value the fixture returns (`0` for
/// `pc:add/2`, `N` for `pc:loop/2`), so a run printing anything else is a genuine failure
/// rather than a slow measurement.
fn best_of(
    runtime: &Path,
    code_dir: &Path,
    beam: &Path,
    workload: &Workload,
    n: u64,
    stdlib: Option<&Path>,
) -> Result<Timed, Failure> {
    let expected = (workload.expected)(n);
    let mut best: Option<Timed> = None;
    for _ in 0..BEST_OF {
        let timed = run_once(runtime, code_dir, beam, workload.spec, n, stdlib)?;
        if timed.stdout != expected {
            return Err(Failure::Error(format!(
                "`{}` printed {:?} for N={n}, expected {expected:?} - the workload did not \
                 run as intended, so its timing means nothing",
                workload.spec, timed.stdout
            )));
        }
        if best.as_ref().map_or(true, |current| timed.wall < current.wall) {
            best = Some(timed);
        }
    }
    best.ok_or_else(|| Failure::Error("internal: no measurement was taken".to_string()))
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

/// The iteration count: the first argument, or [`DEFAULT_N`].
fn iteration_count() -> Result<u64, Failure> {
    let mut args = env::args().skip(1);
    let raw = match args.next() {
        Some(raw) => raw,
        None => return Ok(DEFAULT_N),
    };
    if raw == "-h" || raw == "--help" {
        return Err(Failure::Help);
    }
    if args.next().is_some() {
        return Err(Failure::Usage(
            "too many arguments (only the iteration count is accepted)".to_string(),
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
    println!("usage: perf_bif [N]");
    println!();
    println!("  N  iterations per workload, at least 1 (default {DEFAULT_N})");
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
