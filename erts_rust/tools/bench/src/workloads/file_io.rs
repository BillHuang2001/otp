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

//! `file_io` — **the I/O-bound workload class**: file I/O in a loop, under *both* runtimes
//! (the system oracle and the Rust runtime `beam_rt`).
//!
//! `ERTS_RUST_REWRITE_EVALUATION.md` §G.7 names four workload classes the rewrite must be
//! measured on — compute/JIT-bound, messaging/copy-bound, GC-bound and **I/O-bound** — plus
//! startup and p99 latency. Until this workload existed the driver had *no* I/O-bound signal
//! that ran under both runtimes: `cowboy` (the only other I/O row) is `Runtimes::SysOnly`,
//! because it builds and boots a whole release. This workload closes that gap with an
//! embedded Erlang module compiled once by the system `erlc` and then run **identically**
//! under each scheduled runtime, so the only thing that can differ between the two rows is
//! how the runtime services the file-system calls.
//!
//! # The cases (all in one embedded module)
//!
//! Every case reads back **exactly** the bytes it wrote — the payload is bound and the
//! read result is matched against it (`{ok, Payload}`), which is a real equality test that
//! raises on a truncated, empty or corrupted read. A lost write therefore fails the run
//! loudly instead of quietly measuring less work. Each validated iteration adds [`PER_OP`],
//! so a run at `N` iterations prints exactly `N * PER_OP`.
//!
//! * `write_read` — `file:write_file/2` then `file:read_file/1`: the **whole-file** view
//!   (open + write + close + open + read + close, driven through OTP's `file_server`).
//! * `handle_rw` — the **raw-handle** view: `file:open(Path, [write, raw, binary])` +
//!   `file:write/2` + `file:close/1`, then `file:open(Path, [read, raw, binary])` +
//!   `file:read(Fd, byte_size(Payload))` + `file:close/1`. The same full-equality check,
//!   so a short read is a failure.
//! * `ram_file` (**optional**) — `file:open(Name, [ram, read, write, binary])` + write +
//!   reposition + read + close, the `ram_file_drv` path. It is optional because a runtime
//!   may legitimately not serve it; see *the optional case* below.
//!
//! The payload ([`PAYLOAD_BYTES`] bytes, built by interpreted recursion from this crate's
//! constants — the Erlang source is generated from them, so the sides cannot drift) is
//! built **once per process**, outside the timed loop, so the loop measures the file-system
//! round trip rather than payload construction.
//!
//! # The scratch path reaches the module through the child environment
//!
//! The `run` action carries only integers, so a path cannot be an argument: the module
//! reads [`ENV_DIR`] with `os:getenv/1`, and the driver sets that variable **on the spawned
//! child** ([`command_for`]) to a directory inside a [`ScratchDir`] (whose `Drop` removes it).
//! Nothing is ever written outside that directory, and no absolute worktree path is baked
//! into this file.
//!
//! # The optional case: `ram_file`
//!
//! `ram_file` is the one case that may legitimately refuse to run. It therefore **verifies
//! itself** before the loop: it opens the RAM file, writes the payload, repositions, reads
//! it back, closes, and then asserts that the name is **gone** (`{error, enoent}`). A
//! runtime that silently ignores the `ram` option would instead materialise a host file,
//! and its time would be an ordinary disk figure mislabelled as a RAM one — that
//! verification turns exactly that into a loud `{ram_not_ram, Name}` exit. A case that
//! refuses on a runtime is reported **absent on that runtime with its own reason** and
//! contributes **no** metric (see [`Case::optional`]); it never fabricates a number.
//!
//! # The measurement (paired, drift-cancelling, per op)
//!
//! Each case declares a **per-runtime loop count** `N` (calibrated by the rule in the case
//! table's doc comment) and is sampled through the shared interleaved measurement
//! ([`crate::marginal`]): in **every round** the program runs at `N` iterations
//! (`case_<name>_wall_ms`) and then, *immediately after*, at `0` iterations
//! (`case_<name>_startup_ms` — the pure boot cost, which is **not** zero: the Rust runtime
//! loads its preloaded-module set and probes its system code path on disk; the system `erl`
//! is only *run* as a last resort).
//!
//! ```text
//! case_<name>_ns_per_op = median(wall@N - wall@0) / N * 1e6
//! ```
//!
//! The pairing is not cosmetic: the earlier schedule collected *all* `N`-op samples first
//! and *all* `0`-op samples afterwards and used `mean(work) - mean(startup)`, which on this
//! shared host absorbed the *drift* of the boot cost instead of cancelling it (an
//! independent interleaved measurement of the sibling `copy_bound` workload shows the
//! failure mode is worth a ≈56× inflation of a published gap). **The sequential
//! all-N-then-all-0 schedule must never come back**; the rules live in [`crate::marginal`],
//! which every per-unit workload shares.
//!
//! A figure whose median paired difference does not clear
//! `NOISE_MULTIPLE × max(MIN_NOISE_MS, 1.4826 × MAD)` is **withheld** (recorded as a
//! `marginal below timer resolution` note, with no metric): no number is better than a
//! drift artifact. Every published figure carries its auditable
//! `paired median=… min=… max=… noise=±… over k round(s)` note, and every row also emits
//! the deterministic, contention-immune companion `case_<name>_ops = N`.
//!
//! # Honest reporting
//!
//! * Every runtime's stdout must equal the deterministic `N * PER_OP`; a mismatch is
//!   recorded as a note (that run's timing is still real).
//! * A Rust exit `3` (`beam_rt: unsupported: …`) records the capability gap and emits
//!   **no** timing — the row is `skipped(unsupported-case-set)`, never a fake number.
//! * `erlc` unavailable ⇒ an honest `skipped(erlc-absent)`.
//! * The embedded module failing to compile is a **failure** (`module-compile-failed`),
//!   never a skip: the source is ours, so it cannot legitimately be missing.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::marginal::{self, Marginal, Pass, PassError};
use crate::metrics::{Category, Metric, Probe, Runtimes, Status};
use crate::proc::{self, Completed};
use crate::report::{Profile, WorkloadResult};
use crate::runtime::RuntimeKind;
use crate::scratch::ScratchDir;
use crate::workload::{RunContext, Workload};
use crate::workloads::slice;

/// The embedded module's name.
const MODULE: &str = "bench_file_io";

/// Every exported case function's arity (the loop count).
const ARITY: u32 = 1;

/// The size of the payload each iteration writes and reads back.
///
/// One page: large enough that the round trip is real file-system work rather than
/// per-call overhead, small enough that a run at a calibrated `N` stays a few hundred
/// milliseconds. It is the *only* place the size is written down — the Erlang source is
/// generated from it.
const PAYLOAD_BYTES: i64 = 4_096;

/// How much one validated I/O iteration adds to a case's accumulator, so a run at `N`
/// iterations prints `N * PER_OP`. The Erlang source is generated from it.
const PER_OP: i64 = 1;

/// The environment variable carrying the scratch directory to the child runtime.
///
/// The `run` action passes only integers, so the path travels in the **child environment**
/// (exactly as `gc_class`'s fixtures read `BEAM_RT_GCH_SRC`/`BEAM_RT_GCH_OUT`).
const ENV_DIR: &str = "BENCH_FILE_IO_DIR";

/// The exported `write_read` case.
const WRITE_READ: &str = "write_read";
/// The exported `handle_rw` case.
const HANDLE_RW: &str = "handle_rw";
/// The exported (optional) `ram_file` case.
const RAM_FILE: &str = "ram_file";

/// One file-I/O case: the embedded function that loops over one file-system round trip.
struct Case {
    /// Stable, short case name (used in the metric names, e.g. `case_<name>_wall_ms`).
    name: &'static str,
    /// The exported case function (arity 1: the loop count).
    function: &'static str,
    /// Whether the case is **optional**: one a runtime may legitimately refuse to serve.
    ///
    /// Only `ram_file` is optional. An optional case that refuses on a runtime is reported
    /// *absent on that runtime* with its own reason and contributes **no** metric — never a
    /// number, and never a failure of the workload as a whole.
    optional: bool,
    /// Loop count calibrated for the Rust runtime (≥0.5 s of loop work, re-derived whenever
    /// the measured per-op cost changes materially).
    n_rust: i64,
    /// Loop count calibrated for the system-runtime oracle (a marginal well above its own
    /// run-to-run startup noise).
    n_sys: i64,
}

impl Case {
    /// The loop count this runtime should use.
    fn n_for(&self, kind: RuntimeKind) -> i64 {
        match kind {
            RuntimeKind::Rust => self.n_rust,
            RuntimeKind::Sys => self.n_sys,
        }
    }
}

/// The fixed case table.
///
/// The `n_rust`/`n_sys` values are **constants, not measurements**, calibrated on the
/// reference host (a shared machine — see the crate CONTEXT) under one rule: **a runtime's
/// timed loop must do enough work that its paired marginal stands clear of that runtime's
/// own run-to-run boot/startup jitter** (measured at ≈±0.1 s on the rust boot). The rust
/// floor is therefore **≥0.5 s of loop work**. The measured rust per-op cost on that host
/// (2026-10-23, `/proc/loadavg` ≈5, `--quick`, default baseline-JIT tier) is
/// **≈98 µs** (`write_read`), **≈1.51 ms** (`handle_rw`); `ram_file` is refused by `beam_rt`
/// (absent), so its cost is the earlier estimate. The counts below are ≈1.95 s, ≈1.51 s and
/// ≈1.04 s of loop work — ≳20× the paired estimator's
/// measured residual noise band. The oracle's much smaller startup jitter needs a
/// correspondingly smaller marginal, so its counts are an order of magnitude larger.
///
/// **Maintenance rule:** when a case's measured per-op cost changes materially, re-derive
/// `n_rust` so the loop work stays ≥0.5 s; the pinning test
/// (`the_rust_loop_counts_clear_the_boot_jitter_band`) enforces the floor against those
/// cost constants.
///
/// The reported metric is normalized per op, so the loop counts only have to keep both
/// marginals measurable — not to equalize the two runtimes' totals. A loop count whose
/// marginal falls below the timer noise yields **no** per-op metric (an honest note), never a
/// fabricated one.
static CASES: &[Case] = &[
    Case {
        name: WRITE_READ,
        function: WRITE_READ,
        optional: false,
        n_rust: 20_000,
        n_sys: 200_000,
    },
    Case {
        name: HANDLE_RW,
        function: HANDLE_RW,
        optional: false,
        n_rust: 1_000,
        n_sys: 200_000,
    },
    Case {
        name: RAM_FILE,
        function: RAM_FILE,
        optional: true,
        n_rust: 20_000,
        n_sys: 200_000,
    },
];

/// The `file_io` workload.
pub struct FileIo;

/// The number of timed samples per case for a profile.
fn runs_for(profile: Profile) -> usize {
    match profile {
        Profile::Quick => 3,
        Profile::Full => 10,
    }
}

/// The embedded Erlang module, generated from the constants above.
///
/// Deterministic: `case(N)` returns `N * PER_OP`, and each iteration writes a
/// [`PAYLOAD_BYTES`]-byte payload and matches the read-back bytes against it exactly.
fn module_source() -> String {
    format!(
        "\
-module({MODULE}).
-export([{WRITE_READ}/{ARITY}, {HANDLE_RW}/{ARITY}, {RAM_FILE}/{ARITY}]).

%% The scratch directory arrives through the CHILD ENVIRONMENT: the `run` action carries
%% only integers, so a path cannot be an argument. An unset variable is a loud exit, never
%% a write into whatever the current directory happens to be.
dir() ->
    case os:getenv(\"{ENV_DIR}\") of
        false -> exit({{missing_env, \"{ENV_DIR}\"}});
        D -> D
    end.

%% The {PAYLOAD_BYTES}-byte payload, built once per process by interpreted recursion so the
%% two sides cannot drift and so it is not one native allocator call.
payload() -> list_to_binary(build_bytes({PAYLOAD_BYTES}, [])).

build_bytes(0, Acc) -> Acc;
build_bytes(N, Acc) -> build_bytes(N - 1, [N rem 256 | Acc]).

%% file:write_file/2 + file:read_file/1: the whole-file view (open + write + close + open +
%% read + close through OTP's file server). The bound `Payload` in `{{ok, Payload}}` is a real
%% equality test, so a truncated, empty or corrupted read exits instead of passing.
{WRITE_READ}(N) ->
    Path = dir() ++ \"/{WRITE_READ}.bin\",
    Payload = payload(),
    {WRITE_READ}(N, Path, Payload, 0).

{WRITE_READ}(0, _Path, _Payload, Acc) -> Acc;
{WRITE_READ}(N, Path, Payload, Acc) ->
    ok = file:write_file(Path, Payload),
    case file:read_file(Path) of
        {{ok, Payload}} -> {WRITE_READ}(N - 1, Path, Payload, Acc + {PER_OP});
        Other -> exit({{write_read_mismatch, Other}})
    end.

%% The raw-handle view: open/write/close then open/read/close, reading back exactly
%% byte_size(Payload) bytes and matching them against the payload.
{HANDLE_RW}(N) ->
    Path = dir() ++ \"/{HANDLE_RW}.bin\",
    Payload = payload(),
    {HANDLE_RW}(N, Path, Payload, byte_size(Payload), 0).

{HANDLE_RW}(0, _Path, _Payload, _Size, Acc) -> Acc;
{HANDLE_RW}(N, Path, Payload, Size, Acc) ->
    {{ok, Out}} = file:open(Path, [write, raw, binary]),
    ok = file:write(Out, Payload),
    ok = file:close(Out),
    {{ok, In}} = file:open(Path, [read, raw, binary]),
    Result = file:read(In, Size),
    ok = file:close(In),
    case Result of
        {{ok, Payload}} -> {HANDLE_RW}(N - 1, Path, Payload, Size, Acc + {PER_OP});
        Other -> exit({{handle_rw_mismatch, Other}})
    end.

%% The RAM-file view (the optional case): file:open(Name, [ram, read, write, binary]).
%% It verifies itself once before the loop — see verify_ram/3.
{RAM_FILE}(N) ->
    Name = dir() ++ \"/{RAM_FILE}.bin\",
    Payload = payload(),
    Size = byte_size(Payload),
    verify_ram(Name, Payload, Size),
    {RAM_FILE}(N, Name, Payload, Size, 0).

%% A real RAM file leaves no host file behind. A runtime that silently ignores the `ram`
%% option materialises one, and its timing would be an ordinary disk figure mislabelled as a
%% RAM one: the `{{error, enoent}}` assertion is what turns that into a loud failure.
verify_ram(Name, Payload, Size) ->
    {{ok, Fd}} = file:open(Name, [ram, read, write, binary]),
    ok = file:write(Fd, Payload),
    {{ok, 0}} = file:position(Fd, bof),
    {{ok, Payload}} = file:read(Fd, Size),
    ok = file:close(Fd),
    case file:read_file_info(Name) of
        {{error, enoent}} -> ok;
        {{ok, _}} -> exit({{ram_not_ram, Name}});
        Other -> exit({{ram_probe_failed, Other}})
    end.

{RAM_FILE}(0, _Name, _Payload, _Size, Acc) -> Acc;
{RAM_FILE}(N, Name, Payload, Size, Acc) ->
    {{ok, Fd}} = file:open(Name, [ram, read, write, binary]),
    ok = file:write(Fd, Payload),
    {{ok, 0}} = file:position(Fd, bof),
    {{ok, Payload}} = file:read(Fd, Size),
    ok = file:close(Fd),
    {RAM_FILE}(N - 1, Name, Payload, Size, Acc + {PER_OP}).
"
    )
}

/// The number of validated file-system round trips a case's `n`-iteration run performs.
fn ops(n: i64) -> i64 {
    n
}

/// The stdout a run at `n` iterations must print (the deterministic constant).
fn expected_stdout(_case: &Case, n: i64) -> String {
    (n * PER_OP).to_string()
}

/// The per-side argv for one case at `n` iterations (everything after the program path).
fn argv_for(kind: RuntimeKind, dir: &Path, case: &Case, n: i64) -> Vec<String> {
    let beam = dir.join(format!("{MODULE}.beam")).display().to_string();
    let dir = dir.display().to_string();
    match kind {
        RuntimeKind::Rust => vec![
            "-pa".to_string(),
            dir,
            "run".to_string(),
            beam,
            format!("{MODULE}:{}/1", case.function),
            n.to_string(),
        ],
        RuntimeKind::Sys => vec![
            "-noshell".to_string(),
            "-pa".to_string(),
            dir,
            "-eval".to_string(),
            format!(
                "io:format(\"~w~n\",[{MODULE}:{}({n})]), halt(0).",
                case.function
            ),
        ],
    }
}

/// The prepared command for one pass: the program, the per-side argv, and the scratch
/// directory in the **child environment** (the only route a path has into the module).
fn command_for(program: &Path, kind: RuntimeKind, dir: &Path, case: &Case, n: i64) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(argv_for(kind, dir, case, n));
    cmd.env(ENV_DIR, dir);
    cmd
}

/// Writes the embedded module into `dir` and compiles it with `erlc`.
///
/// Returns `Err(reason)` — never a silent `false` — when the source cannot be written or the
/// module does not compile, so the caller can report a real failure.
fn stage(dir: &Path, erlc: &Path) -> Result<PathBuf, String> {
    let source = dir.join(format!("{MODULE}.erl"));
    std::fs::write(&source, module_source())
        .map_err(|e| format!("write {}: {e}", source.display()))?;

    let mut cmd = Command::new(erlc);
    cmd.arg("-o").arg(dir).arg(&source);
    match proc::run(&mut cmd) {
        Ok(out) if out.success => {}
        Ok(out) => {
            return Err(format!(
                "erlc exited {:?}: {}",
                out.exit_code,
                out.stderr.trim()
            ))
        }
        Err(e) => return Err(format!("erlc spawn failed: {e}")),
    }

    let beam = dir.join(format!("{MODULE}.beam"));
    if beam.is_file() {
        Ok(beam)
    } else {
        Err(format!("{} missing after erlc exited 0", beam.display()))
    }
}

/// One pass sample: what a single run of the case's program contributes — the wall time (ms),
/// the peak RSS (bytes; only `Work` passes feed [`CaseOutcome::rss`]), the trimmed stdout, and
/// any non-fatal notes (a stdout mismatch is recorded here, never as an abort).
struct Sample {
    wall_ms: f64,
    peak_memory_bytes: Option<u64>,
    stdout: String,
    notes: Vec<String>,
}

/// The samples collected for one case under one runtime: the loop count `n`, the per-round
/// `(work_ms, startup_ms)` pairs behind `marginal` (in round order), the `Work`-pass peak-RSS
/// samples, the first `Work`-pass stdout, the capability-gap flag, the *absence* reason of an
/// optional case, the failure/variation notes, and the resolved [`Marginal`].
struct CaseOutcome {
    n: i64,
    pairs: Vec<(f64, f64)>,
    rss: Vec<f64>,
    observed: Option<String>,
    unsupported: bool,
    absent: Option<String>,
    notes: Vec<String>,
    marginal: Marginal,
}

impl CaseOutcome {
    /// Whether the case produced at least one completed round.
    fn ok(&self) -> bool {
        !self.pairs.is_empty()
    }
}

/// Classifies a non-zero / killed run into a human note.
fn classify_failure(kind: RuntimeKind, case: &Case, completed: &Completed) -> String {
    let stderr = completed.stderr.trim();
    match (kind, completed.exit_code) {
        (RuntimeKind::Rust, Some(3)) => format!(
            "case {}: unsupported: {}",
            case.name,
            stderr.strip_prefix("beam_rt: unsupported: ").unwrap_or(stderr)
        ),
        (_, Some(code)) => format!(
            "case {} ({}): exited {code}: {stderr}",
            case.name,
            kind.as_str()
        ),
        (_, None) => format!(
            "case {} ({}): terminated by signal: {stderr}",
            case.name,
            kind.as_str()
        ),
    }
}

/// Drives `runs` interleaved rounds through [`marginal::sample_pairs`] — the `Work` sample then
/// its `Startup` sample, back to back, every round — and folds the samples into a
/// [`CaseOutcome`]: the per-round wall pairs, the `Work`-pass peak-RSS samples, the first
/// `Work`-pass stdout, the per-sample notes, and the resolved [`Marginal`].
///
/// The per-pass runner is a parameter so the schedule, the note folding and the metric decision
/// can be tested without spawning a process.
fn measure_case<F>(n: i64, runs: usize, sample: F) -> CaseOutcome
where
    F: FnMut(Pass) -> Result<Sample, PassError>,
{
    let sampled = marginal::sample_pairs(runs, sample);
    let mut notes = Vec::new();
    let mut rss = Vec::new();
    let pairs: Vec<(f64, f64)> = sampled
        .pairs
        .iter()
        .map(|(w, s)| (w.wall_ms, s.wall_ms))
        .collect();
    for (work, startup) in &sampled.pairs {
        notes.extend(work.notes.iter().chain(startup.notes.iter()).cloned());
        rss.extend(work.peak_memory_bytes.map(|bytes| bytes as f64));
    }
    let aborted = sampled.aborted.as_ref();
    notes.extend(aborted.map(|abort| abort.note.clone()));
    CaseOutcome {
        n,
        marginal: marginal::resolve(&pairs, ops(n)),
        pairs,
        rss,
        observed: sampled.pairs.first().map(|(work, _)| work.stdout.clone()),
        unsupported: aborted.is_some_and(|abort| abort.unsupported),
        absent: None,
        notes,
    }
}

/// Runs one case under one runtime: `runs` interleaved rounds of an `n`-iteration sample and a
/// `0`-iteration sample, per [`marginal`]'s sanctioned schedule.
fn execute_case(
    program: &Path,
    kind: RuntimeKind,
    dir: &Path,
    case: &Case,
    runs: usize,
) -> CaseOutcome {
    let n = case.n_for(kind);
    // A Rust exit `3` marks the whole case `unsupported` only while nothing is measured yet.
    let mut work_sample_seen = false;
    let mut outcome = measure_case(n, runs, |pass| {
        let count = n * pass.units();
        let mut cmd = command_for(program, kind, dir, case, count);
        let (pass_name, kind_name) = (pass.as_str(), kind.as_str());
        let completed = proc::run(&mut cmd).map_err(|e| PassError {
            note: format!(
                "case {} ({kind_name}): {pass_name}-pass spawn failed: {e}",
                case.name
            ),
            unsupported: false,
        })?;
        if !completed.success {
            let mut note = classify_failure(kind, case, &completed);
            if pass == Pass::Startup {
                note.push_str(" (startup pass)");
            }
            let unsupported =
                kind == RuntimeKind::Rust && completed.exit_code == Some(3) && !work_sample_seen;
            return Err(PassError { note, unsupported });
        }
        let expected = expected_stdout(case, count);
        let got = completed.stdout_trim().to_string();
        let mut notes = Vec::new();
        if got != expected {
            notes.push(format!(
                "case {}: the {pass_name} run printed `{got}` (expected `{expected}`)",
                case.name
            ));
        }
        work_sample_seen |= pass == Pass::Work;
        Ok(Sample {
            wall_ms: completed.wall_ms,
            peak_memory_bytes: completed.peak_memory_bytes,
            stdout: got,
            notes,
        })
    });
    // An **optional** case that never completed a round refused on this runtime: record it as
    // absent with the verbatim reason and drop the capability-gap flag (it is not a gap in the
    // required surface — the case is declared optional).
    if case.optional && !outcome.ok() {
        outcome.absent = Some(if outcome.notes.is_empty() {
            format!("case {}: did not run", case.name)
        } else {
            outcome.notes.join("; ")
        });
        outcome.unsupported = false;
    }
    outcome
}

/// Emits one case's metric rows: the wall / startup / peak-RSS sample rows, the per-op
/// marginal (or the note explaining why there is none) and the deterministic `ops` companion.
fn push_case_metrics(result: &mut WorkloadResult, case: &Case, outcome: &CaseOutcome) {
    let wall: Vec<f64> = outcome.pairs.iter().map(|&(work_ms, _)| work_ms).collect();
    let startup: Vec<f64> = outcome.pairs.iter().map(|&(_, startup_ms)| startup_ms).collect();
    let rows = [
        ("wall_ms", "ms", &wall),
        ("startup_ms", "ms", &startup),
        ("peak_rss_bytes", "bytes", &outcome.rss),
    ];
    for (suffix, unit, values) in rows {
        if !values.is_empty() {
            result
                .metrics
                .push(Metric::samples(format!("case_{}_{suffix}", case.name), unit, values));
        }
    }
    match outcome.marginal.unit_cost_ns() {
        Some(ns) => {
            result
                .metrics
                .push(Metric::scalar(format!("case_{}_ns_per_op", case.name), "ns", ns));
            let detail = outcome.marginal.detail();
            result.notes.push(format!("case {}: {detail}", case.name));
        }
        None => {
            let note = outcome.marginal.below_resolution_note();
            result.notes.push(format!("case {}: {note}", case.name));
        }
    }
    // The deterministic, contention-immune companion: the round trips the timing came from.
    result.metrics.push(Metric::scalar(
        format!("case_{}_ops", case.name),
        "ops",
        ops(outcome.n) as f64,
    ));
}

impl Workload for FileIo {
    fn id(&self) -> &'static str {
        "file_io"
    }

    fn description(&self) -> &'static str {
        "file write/read round trips under each runtime (per-op I/O cost)"
    }

    fn category(&self) -> Category {
        Category::Io
    }

    fn contracts(&self) -> &'static [&'static str] {
        // Real Erlang semantics over the OTP file/IO-server application contract (A.7):
        // the same compiled module runs on both runtimes.
        &["A.1", "A.7"]
    }

    fn runtimes(&self) -> Runtimes {
        // The whole point: both runtimes execute the same compiled I/O program.
        Runtimes::Both
    }

    fn probe(&self) -> Probe {
        if slice::resolve_erlc().is_none() {
            return Probe::skipped("erlc-absent");
        }
        Probe::Ready
    }

    fn run(&self, ctx: &RunContext) -> WorkloadResult {
        let kind = ctx.runtime.kind;
        let (id, category, description) = (self.id(), self.category(), self.description());
        let Some(program) = ctx.runtime.program() else {
            return WorkloadResult::skipped(id, kind, category, description, "runtime-unsupported");
        };
        let Some(erlc) = slice::resolve_erlc() else {
            return WorkloadResult::skipped(id, kind, category, description, "erlc-absent");
        };

        let scratch = match ScratchDir::new(id) {
            Ok(dir) => dir,
            Err(e) => {
                let reason = format!("scratch-dir-failed: {e}");
                return WorkloadResult::failed(id, kind, category, description, reason);
            }
        };
        let dir = scratch.path();

        // The module source is ours, so a staging/compile failure is a real failure, never a
        // missing prerequisite.
        let staged = match stage(dir, &erlc) {
            Ok(beam) => format!(
                "staged {} (embedded source, payload={PAYLOAD_BYTES} bytes, per-op increment={PER_OP}, erlc={})",
                beam.display(),
                erlc.display()
            ),
            Err(reason) => {
                let mut result = WorkloadResult::failed(
                    id,
                    kind,
                    category,
                    description,
                    "module-compile-failed",
                );
                result.notes.push(reason);
                return result;
            }
        };

        let runs = runs_for(ctx.profile);
        let mut result = WorkloadResult::new(id, kind, category, description);
        result.notes.push(staged);

        let mut overall_peak: Option<u64> = None;
        let mut observed_all: Vec<String> = Vec::new();
        let mut ok_cases = 0usize;
        let mut unsupported_cases = 0usize;
        let mut absent_cases = 0usize;

        for case in CASES {
            let outcome = execute_case(program, kind, dir, case, runs);
            if outcome.ok() {
                ok_cases += 1;
                push_case_metrics(&mut result, case, &outcome);
                for &bytes in &outcome.rss {
                    overall_peak =
                        Some(overall_peak.map_or(bytes as u64, |peak| peak.max(bytes as u64)));
                }
                if let Some(value) = &outcome.observed {
                    observed_all.push(format!("{}={value}", case.name));
                }
                result.notes.push(format!(
                    "case {}: N={} ({}) file-system round trips of the {PAYLOAD_BYTES}-byte payload in {}",
                    case.name,
                    outcome.n,
                    kind.as_str(),
                    dir.display()
                ));
            } else if let Some(reason) = &outcome.absent {
                absent_cases += 1;
                result.notes.push(format!(
                    "case {}: absent on {} ({reason}) — no metric recorded",
                    case.name,
                    kind.as_str()
                ));
            } else if outcome.unsupported {
                unsupported_cases += 1;
            }
            result.notes.extend(outcome.notes);
        }

        result.peak_memory_bytes = overall_peak;
        if !observed_all.is_empty() {
            let observed = observed_all.join(" ");
            result
                .notes
                .push(format!("observed stdout: {observed} (each case prints N * {PER_OP})"));
        }
        result.notes.push(
            "method: ns/op = median(wall at N ops - wall at 0 ops) / N * 1e6, from interleaved \
             paired rounds (each round runs the N-op sample and then the 0-op sample back to \
             back, so a drifting boot cost cancels inside the pair); N is per runtime so both \
             marginals are measurable, and the fixed process startup (incl. the Rust runtime's \
             code-path resolution) cancels out of every pair; the figure is withheld when the \
             median does not clear the noise band; every iteration matches the read-back bytes \
             against the written payload exactly, so a lost or truncated read fails the run"
                .to_string(),
        );
        result.notes.push(
            "note: the scratch directory reaches the module through the child environment \
             (BENCH_FILE_IO_DIR), so nothing is written outside the run's ScratchDir; the op \
             count is deterministic and contention-immune while the wall-clock rows move with \
             host load"
                .to_string(),
        );
        result.notes.push(format!(
            "cases: {ok_cases} ok, {absent_cases} absent (optional), {unsupported_cases} \
             unsupported (of {})",
            CASES.len()
        ));

        if ok_cases == 0 && absent_cases == 0 {
            let (status, reason) = if unsupported_cases == CASES.len() {
                (Status::Skipped, "unsupported-case-set")
            } else {
                (Status::Failed, "no-file-io-case-ran")
            };
            result.status = status;
            result.reason = Some(reason.to_string());
        } else if ok_cases == 0 {
            // Every case refused on this runtime: nothing was measured, so this is a skip with
            // the reason recorded per case above — never an `ok` row.
            result.status = Status::Skipped;
            result.reason = Some("no-file-io-case-ran".to_string());
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn metadata_is_stable() {
        let w = FileIo;
        assert_eq!(w.id(), "file_io");
        assert_eq!(w.category(), Category::Io);
        assert_eq!(w.category().as_str(), "io");
        assert_eq!(w.contracts(), &["A.1", "A.7"]);
        assert!(matches!(w.runtimes(), Runtimes::Both));
        assert!(w.runtimes().allows(RuntimeKind::Rust));
        assert!(w.runtimes().allows(RuntimeKind::Sys));
        assert!(!w.description().is_empty());
    }

    #[test]
    fn every_case_declares_a_calibrated_n() {
        assert_eq!(CASES.len(), 3, "write_read + handle_rw + ram_file");
        let mut names: Vec<&str> = CASES.iter().map(|c| c.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), CASES.len(), "case names must be distinct");
        for case in CASES {
            assert!(!case.name.is_empty(), "a case has no name");
            assert!(
                module_source().contains(&format!("{}(N) ->", case.function)),
                "{}",
                case.name
            );
            assert!(
                case.n_for(RuntimeKind::Rust) > 0 && case.n_for(RuntimeKind::Sys) > 0,
                "{}",
                case.name
            );
            // The oracle is the faster runtime, so it always gets the larger loop count.
            assert!(
                case.n_sys > case.n_rust,
                "{}: the oracle loop must be the larger one",
                case.name
            );
        }
        // Only the RAM-file case is optional.
        let optional: Vec<&str> = CASES
            .iter()
            .filter(|c| c.optional)
            .map(|c| c.name)
            .collect();
        assert_eq!(optional, vec![RAM_FILE]);
    }

    /// Every rust loop count must do ≥0.5 s of work so its paired marginal stands clear of the
    /// measurement noise, at the per-op cost measured on the reference host.
    #[test]
    fn the_rust_loop_counts_clear_the_boot_jitter_band() {
        // A conservative bound on the noise a paired marginal must clear: the measured MAD of
        // the paired differences is a few milliseconds, well under this 50 ms floor.
        const NOISE_FLOOR_S: f64 = 0.05;
        // Per-op costs measured on the reference host (2026-10-23, `--quick`, paired-median
        // marginals, default baseline-JIT tier) — see the case table's doc comment.
        const COST_NS: &[(&str, f64, i64)] = &[
            (WRITE_READ, 98_000.0, 20_000),
            (HANDLE_RW, 1_512_000.0, 1_000),
            (RAM_FILE, 52_000.0, 20_000),
        ];
        assert_eq!(COST_NS.len(), CASES.len());
        for (name, ns_per_op, n_rust) in COST_NS {
            let case = CASES.iter().find(|c| c.name == *name).expect("case");
            assert_eq!(case.n_rust, *n_rust, "{name}: the recalibrated count");
            let work_s = *n_rust as f64 * ns_per_op / 1e9;
            assert!((0.5..=5.0).contains(&work_s), "{name}: {work_s} s of work");
            assert!(
                work_s >= 3.0 * NOISE_FLOOR_S,
                "{name}: {work_s} s of work must clear 3x the {NOISE_FLOOR_S} s noise floor"
            );
        }
        // The Payload must stay one page-ish: a case table drifting to a huge payload would
        // turn the loop into a bandwidth test rather than a round-trip test.
        assert!((1_024..=65_536).contains(&PAYLOAD_BYTES), "{PAYLOAD_BYTES}");
    }

    #[test]
    fn argv_carries_the_loop_count_and_the_pa_dir_for_both_runtimes() {
        let dir = Path::new("/tmp/xyz");
        let case = &CASES[0];

        let rust = argv_for(RuntimeKind::Rust, dir, case, 7);
        assert_eq!(rust[0], "-pa");
        assert_eq!(rust[1], "/tmp/xyz");
        assert_eq!(rust[2], "run");
        assert_eq!(rust[3], format!("/tmp/xyz/{MODULE}.beam"));
        assert_eq!(rust[4], format!("{MODULE}:{}/1", case.function));
        assert_eq!(rust[5], "7");

        let sys = argv_for(RuntimeKind::Sys, dir, case, 9);
        assert_eq!(sys[0], "-noshell");
        assert!(sys.contains(&"-pa".to_string()));
        assert!(sys.contains(&"/tmp/xyz".to_string()));
        assert!(sys
            .iter()
            .any(|a| a.contains(&format!("{MODULE}:{}(9)", case.function))));
    }

    /// The path can only reach the module through the child environment, so every spawned
    /// command must carry it — for both runtimes.
    #[test]
    fn the_scratch_dir_reaches_the_module_through_the_child_environment() {
        let dir = Path::new("/tmp/file-io-env");
        for kind in [RuntimeKind::Rust, RuntimeKind::Sys] {
            let cmd = command_for(Path::new("/bin/true"), kind, dir, &CASES[0], 5);
            let envs: Vec<(&OsStr, Option<&OsStr>)> = cmd.get_envs().collect();
            assert!(
                envs.iter().any(|(name, value)| {
                    *name == OsStr::new(ENV_DIR) && *value == Some(dir.as_os_str())
                }),
                "{kind:?}: {ENV_DIR} must be set to the scratch dir, got {envs:?}"
            );
        }
    }

    /// A fake pass sample: `work_ms` on the `Work` pass, `startup_ms` on the `Startup` pass,
    /// the deterministic stdout of `units × n`, no RSS, no notes.
    fn fake_sample(pass: Pass, work_ms: f64, startup_ms: f64, n: i64) -> Sample {
        let wall_ms = if pass == Pass::Work {
            work_ms
        } else {
            startup_ms
        };
        Sample {
            wall_ms,
            peak_memory_bytes: None,
            stdout: (n * pass.units() * PER_OP).to_string(),
            notes: Vec::new(),
        }
    }

    /// `CASES[0]`'s metrics pushed into a fresh result, from a steady fake clock whose `Work`
    /// pass costs `delta_ms` (ms) more than its `Startup` pass, over `runs` rounds.
    fn measured(delta_ms: f64, runs: usize) -> WorkloadResult {
        let outcome = measure_case(1_000, runs, |pass| {
            Ok(fake_sample(pass, 100.0 + delta_ms, 100.0, 1_000))
        });
        let mut result = WorkloadResult::new("file_io", RuntimeKind::Rust, Category::Io, "test");
        push_case_metrics(&mut result, &CASES[0], &outcome);
        result
    }

    /// The `case_<CASES[0]>_<suffix>` metric, if the case emitted it.
    fn case_metric<'a>(result: &'a WorkloadResult, suffix: &str) -> Option<&'a Metric> {
        let name = format!("case_{}_{suffix}", CASES[0].name);
        result.metrics.iter().find(|m| m.name == name)
    }

    /// The note starting with `case <CASES[0]>: <prefix>`.
    fn case_note<'a>(result: &'a WorkloadResult, prefix: &str) -> Option<&'a String> {
        let prefix = format!("case {}: {prefix}", CASES[0].name);
        result.notes.iter().find(|n| n.starts_with(&prefix))
    }

    /// The schedule `file_io` uses: every round is a `Work` sample *immediately* followed by
    /// its `Startup` sample — never a whole pass at a time (the old sequential
    /// all-`N`-then-all-`0` schedule is what let host drift inflate a published gap ≈56×).
    #[test]
    fn rounds_are_paired_work_then_startup() {
        let mut order = Vec::new();
        let outcome = measure_case(1_000, 3, |pass| {
            order.push(pass);
            Ok(fake_sample(pass, 110.0, 100.0, 1_000))
        });
        assert_eq!(
            order,
            [Pass::Work, Pass::Startup].repeat(3),
            "one round is Work then Startup"
        );
        assert_eq!(outcome.pairs, vec![(110.0, 100.0); 3]);
        assert!(outcome.ok() && outcome.notes.is_empty() && !outcome.unsupported);
        // 10 ms of work over 1 000 iterations = 10 000 ns/op, well clear of the noise band.
        assert_eq!(
            (outcome.marginal.rounds, outcome.marginal.unit_cost_ns()),
            (3, Some(10_000.0))
        );

        let result = measured(10.0, 3);
        let ns = case_metric(&result, "ns_per_op").expect("a clear marginal must publish");
        assert_eq!((ns.unit.as_str(), ns.value), ("ns", Some(10_000.0)));
        let note = case_note(&result, "paired median=").expect("the figure must cite its evidence");
        assert!(note.contains("over 3 round(s)"), "{note}");
        for suffix in ["ops", "wall_ms", "startup_ms"] {
            assert!(case_metric(&result, suffix).is_some(), "{suffix}");
        }
        // No `Work`-pass peak RSS was supplied here, so no RSS row is emitted.
        assert!(case_metric(&result, "peak_rss_bytes").is_none());
    }

    /// A failing startup pass keeps the pairs already completed, reports where it stopped, and
    /// carries the capability-gap flag through untouched.
    #[test]
    fn a_failing_startup_pass_keeps_the_completed_pairs() {
        let mut samples = 0;
        let outcome = measure_case(1_000, 3, |pass| {
            samples += 1;
            // Sample 4 is round 2's `Startup` pass (the order is W,S,W,S,...).
            if samples == 4 {
                assert_eq!(pass, Pass::Startup);
                let note = "case write_read (rust): exited 1: boom (startup pass)".to_string();
                return Err(PassError {
                    note,
                    unsupported: false,
                });
            }
            Ok(fake_sample(pass, 110.0, 100.0, 1_000))
        });
        assert_eq!(outcome.pairs, vec![(110.0, 100.0)], "round 1 only");
        assert!(outcome.ok(), "one completed round is still a usable sample");
        assert_eq!(outcome.marginal.rounds, 1);
        assert!(!outcome.unsupported);
        assert!(
            outcome.notes.iter().any(|n| n.contains("boom")),
            "{:?}",
            outcome.notes
        );

        // A capability gap on the very first `Work` pass: nothing measured ⇒ unsupported.
        let gap = || PassError {
            note: "case write_read: unsupported: unsupported_bif".into(),
            unsupported: true,
        };
        let outcome = measure_case(1_000, 2, |_pass| Err::<Sample, PassError>(gap()));
        assert!(outcome.pairs.is_empty() && !outcome.ok() && outcome.unsupported);
        assert_eq!(outcome.marginal.unit_cost_ns(), None);
        assert!(outcome
            .notes
            .iter()
            .any(|n| n.contains("unsupported_bif")));
    }

    /// A noisy sample set publishes **no** `ns_per_op` metric — only the withholding note — so
    /// a drift artifact can never reach the report.
    #[test]
    fn the_metric_is_withheld_inside_the_noise_band() {
        // A steady 1 ms of "work" per round is below NOISE_MULTIPLE × MIN_NOISE_MS.
        let result = measured(1.0, 3);
        assert!(
            case_metric(&result, "ns_per_op").is_none(),
            "no figure inside the noise band"
        );
        // The deterministic companion and the raw sample rows are still published.
        for suffix in ["ops", "wall_ms", "startup_ms"] {
            assert!(case_metric(&result, suffix).is_some(), "{suffix}");
        }
        let note =
            case_note(&result, "marginal below timer resolution").expect("the withholding note");
        assert!(note.contains("paired median=1.000 ms"), "{note}");
    }

    /// An **optional** case that refuses on a runtime is recorded absent with its own reason
    /// and emits no metric — the row still reports the required cases.
    #[test]
    fn an_optional_case_that_refuses_is_absent_with_a_reason() {
        let dir = Path::new("/tmp/file-io-absent");
        // A program that does not exist makes every pass fail with a spawn error.
        let outcome = execute_case(Path::new("/nonexistent/beam_rt"), RuntimeKind::Rust, dir, &CASES[2], 1);
        assert!(!outcome.ok(), "nothing was measured");
        assert!(!outcome.unsupported, "an optional case is not a capability gap");
        let reason = outcome.absent.expect("the refusal must be recorded");
        assert!(
            reason.contains("spawn failed") && reason.contains(RAM_FILE),
            "{reason}"
        );

        // A *required* case failing is not silently downgraded to absent.
        let required = execute_case(
            Path::new("/nonexistent/beam_rt"),
            RuntimeKind::Rust,
            dir,
            &CASES[0],
            1,
        );
        assert!(required.absent.is_none(), "only an optional case may be absent");
        assert!(!required.ok());
    }

    #[test]
    fn the_op_count_is_the_deterministic_loop_count() {
        assert_eq!(ops(0), 0);
        assert_eq!(ops(1), 1);
        let case = &CASES[0];
        assert_eq!(ops(case.n_for(RuntimeKind::Rust)), case.n_rust);
        assert_eq!(ops(case.n_for(RuntimeKind::Sys)), case.n_sys);
        // The deterministic stdout: a run at N iterations printed N * PER_OP.
        assert_eq!(expected_stdout(case, 0), "0");
        assert_eq!(expected_stdout(case, 1), PER_OP.to_string());
        assert_eq!(expected_stdout(case, 3), (3 * PER_OP).to_string());
    }

    /// The generated module pins the shared constants and keeps the full-equality read-back
    /// checks that make a lost or truncated read a loud failure.
    #[test]
    fn the_embedded_module_pins_the_shared_constants() {
        let src = module_source();
        assert!(src.starts_with(&format!("-module({MODULE}).")), "{src}");
        assert!(src.contains(&format!(
            "-export([{WRITE_READ}/{ARITY}, {HANDLE_RW}/{ARITY}, {RAM_FILE}/{ARITY}])."
        )));
        assert!(src.contains(&format!("os:getenv(\"{ENV_DIR}\")")));
        assert!(src.contains(&format!("build_bytes({PAYLOAD_BYTES}, [])")));
        assert!(src.contains(&format!("Acc + {PER_OP}")));
        for needle in [
            "file:write_file(Path, Payload)",
            "file:read_file(Path)",
            "{ok, Payload} ->",
            "file:open(Path, [write, raw, binary])",
            "file:write(Out, Payload)",
            "file:read(In, Size)",
            "file:close(In)",
            "file:open(Name, [ram, read, write, binary])",
            "file:read_file_info(Name)",
            "{error, enoent} -> ok;",
            "{ram_not_ram, Name}",
        ] {
            assert!(src.contains(needle), "missing `{needle}`");
        }
        // No case may accept a read it did not verify: every read-back is matched against the
        // bound payload, and a mismatch exits.
        assert!(!src.contains("file:read_file(Path),\n    {WRITE_READ}"));
        for exit in ["write_read_mismatch", "handle_rw_mismatch"] {
            assert!(src.contains(exit), "missing `{exit}`");
        }
    }

    #[test]
    fn stage_reports_a_write_failure_instead_of_silently_failing() {
        let missing = std::env::temp_dir().join(format!(
            "bench-file-io-absent-{}-xyzzy",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&missing);
        let err = stage(&missing, Path::new("/nonexistent/erlc"))
            .expect_err("a missing dir must be reported");
        assert!(err.contains("write"), "{err}");
    }

    /// Oracle-gated smoke test: with `erlc` and a resolvable runtime present, the required
    /// cases must produce real timings (or an honest note). No-op when prerequisites are
    /// missing.
    #[test]
    fn smoke_runs_on_each_resolvable_runtime() {
        let Some(erlc) = slice::resolve_erlc() else {
            return;
        };
        let runtimes = [
            crate::runtime::resolve_oracle().ok(),
            Some(crate::runtime::resolve_rust()),
        ];
        for runtime in runtimes.into_iter().flatten() {
            if !runtime.self_check() {
                continue;
            }
            let scratch = ScratchDir::new("bench-file-io-smoke").expect("scratch");
            let beam = stage(scratch.path(), &erlc).expect("the embedded module must compile");
            assert!(beam.is_file());
            let program = runtime.program().expect("a self-checking runtime resolves");
            let outcome = execute_case(program, runtime.kind, scratch.path(), &CASES[0], 1);
            // Either a real timing, or an honest note (e.g. the Rust runtime's gap).
            assert!(
                outcome.ok() || !outcome.notes.is_empty(),
                "runtime {:?}: neither samples nor notes",
                runtime.kind
            );
            if outcome.ok() {
                assert_eq!(
                    outcome.observed.as_deref(),
                    Some(expected_stdout(&CASES[0], outcome.n).as_str()),
                    "runtime {:?}: the case must print the deterministic constant",
                    runtime.kind
                );
            }
        }
    }
}
