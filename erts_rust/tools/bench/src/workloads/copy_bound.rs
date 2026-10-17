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

//! `copy_bound` — **the messaging / copy-bound metric**: the term-copy boundary run under
//! *both* runtimes (the system oracle and the Rust runtime `beam_rt`).
//!
//! Every repo-backed workload (Cowboy, Khepri, EMQX, PLB, the compilers) is `sys`-only, and
//! the other cross-runtime signals (`list_walk`/`foreign`) are **compute**-bound. This workload
//! supplies the *messaging/copy boundary* signal that `ERTS_RUST_REWRITE_TEST_PROJECTS.md`
//! names — the point where a term must leave its owning process.
//!
//! # The four cases (all in one embedded module)
//!
//! Each case copies the **same structured payload**, validates the round-trip with an Erlang
//! bound-variable pattern (a real equality test that raises on a mismatch) and adds `PER_OP`
//! per validated op, so a run at `N` ops prints exactly `N * PER_OP`:
//!
//! * `etf_roundtrip` — `term_to_binary/1` + `binary_to_term/1`, the **External Term Format
//!   wire codec** (contract A.5).
//! * `ets_copy` — `ets:insert/2` + `ets:lookup/2`, the **ETS copy boundary** (one table and
//!   key per process, so the table never grows: the work is the copy, not table growth).
//! * `send_recv` — a term to a second process, codec round-tripped there and sent back: **both
//!   directions** of the process copy boundary (an `after` guard makes a missing reply loud).
//! * `send_recv_plain` — the **same two-process shape, no codec call in the receiver** (the
//!   worker replies `From ! {self(), T}`), so `send_recv − send_recv_plain` attributes the
//!   codec component *inside* the two-process path.
//!
//! The payload is *generated from this crate's constants* (see [`module_source`]), so the
//! Rust and Erlang sides cannot drift.
//!
//! # The measurement (paired, drift-cancelling, per op)
//!
//! Each case declares a **per-runtime loop count** `N` (calibrated by the rule in the case
//! table's doc comment) and is sampled through the shared interleaved measurement
//! ([`crate::marginal`]): in **every round** the program runs at `N` ops
//! (`case_<name>_wall_ms`) and then, *immediately after*, at `0` ops
//! (`case_<name>_startup_ms` — the pure boot cost, which is **not** zero: the Rust runtime
//! loads its preloaded-module set and probes its system code path on disk
//! (`crates/beam_rt/src/codeserver.rs`); the system `erl` is only *run* as a last resort).
//!
//! ```text
//! case_<name>_ns_per_op = median(wall@N - wall@0) / N * 1e6
//! ```
//!
//! # Why the pairs are interleaved — the old schedule must never come back
//!
//! The boot cost cancels only if both samples of a **pair** see the same host conditions. The
//! earlier schedule collected *all* `N`-op samples first and *all* `0`-op samples afterwards
//! and used `mean(work) - mean(startup)`; on this shared, loaded host the boot cost drifts by
//! tens of milliseconds *between* the passes, so the difference absorbed the drift instead of
//! cancelling it (the only guard was refusing a negative result). Against an independent
//! interleaved measurement the published `etf_roundtrip` gap was inflated by ≈56× (`136.3×`
//! published where the interleaved figure is `2.42×`). So: one `Work` sample followed by its
//! `Startup` sample, round after round — **never** a whole pass at a time.
//!
//! The statistic is the **median** of the per-round differences (one contended round cannot
//! drag it, unlike a mean), with min/max spread reported; the figure is withheld — `marginal
//! below timer resolution`, **no** metric — when the median does not clear
//! [`crate::marginal::NOISE_MULTIPLE`] × `max(`[`crate::marginal::MIN_NOISE_MS`]`, 1.4826 ×
//! MAD)`. Never a fabricated, negative or drift-inflated number; every published figure carries
//! an auditable `paired median=… min=… noise=±… over k round(s)` note. The deterministic
//! `case_<name>_ops = N` companion is **contention-immune**, so the `N` behind every wall
//! figure is in the report (never compare raw `wall_ms` across sides with different `N`).
//!
//! # Honest reporting
//!
//! Every runtime's stdout must equal the deterministic `N * PER_OP` (a mismatch is a note). A
//! Rust exit `3` (`beam_rt: unsupported: …`) records the capability gap and emits **no** timing
//! for that case; no `erlc` ⇒ `skipped(erlc-absent)`; the embedded module failing to compile is
//! a **failure** (`module-compile-failed`), never a skip (the source is ours). Everything is
//! staged in a private, self-cleaning [`ScratchDir`] under `$TMPDIR`; the workload never writes
//! outside it and never panics.

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

/// The embedded module's name (also used as the ETS table's label).
const MODULE: &str = "bench_copy_bound";

/// The `term_to_binary/1` + `binary_to_term/1` case's exported function.
const ETF: &str = "etf_roundtrip";

/// The `ets:insert/2` + `ets:lookup/2` case's exported function.
const ETS: &str = "ets_copy";

/// The two-process `send`/`receive` case's exported function.
const SR: &str = "send_recv";

/// The two-process `send`/`receive` **delivery-only** case's exported function (no ETF
/// codec in the receiver: the isolation case that attributes the codec component of
/// [`SR`]).
const SR_PLAIN: &str = "send_recv_plain";

/// The exported functions' shared arity (the loop count).
const ARITY: u32 = 1;

/// The copied payload (generated into `payload/0`, so the two sides cannot drift): a
/// `PAYLOAD_LIST_LEN`-element list, a tuple (atom + binary + float), a `PAYLOAD_BIN_BYTES`-byte
/// binary, a char-list string and a negative integer.
const PAYLOAD_LIST_LEN: i64 = 10;
const PAYLOAD_BIN_BYTES: i64 = 12;
const PAYLOAD_FLOAT: f64 = 3.5;
const PAYLOAD_STRING: &str = "a char list";
const PAYLOAD_NEG: i64 = -42;
/// The number of top-level keys in the copied payload (the map built by `payload/0`).
const PAYLOAD_KEYS: usize = 5;
/// How much one validated copy round-trip adds to a case's accumulator, so a run at `N` ops
/// prints `N * PER_OP`. The Erlang source is generated from it.
const PER_OP: i64 = 1;

/// How long the two-process case waits for the worker's reply before failing loudly.
///
/// A missing reply must abort the case (the run exits non-zero and is reported), never hang
/// the harness: the driver invokes the runtime with no process timeout of its own.
const RECV_TIMEOUT_MS: u32 = 30_000;

/// One copy-bound case: the embedded driver function that loops over the copy round-trip.
struct Case {
    /// Stable, short case name (used in the metric names, e.g. `case_<name>_wall_ms`).
    name: &'static str,
    /// The exported driver function (arity 1: the loop count).
    function: &'static str,
    /// Loop count calibrated for the Rust runtime (≥0.5 s of loop work, re-derived whenever the
    /// measured per-op cost changes materially).
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
/// timed loop must do enough work that its paired marginal stands clear of the measurement
/// noise**. The samples are interleaved (see the module docs), so the residual noise is the MAD
/// of the paired differences — a few milliseconds — and a uniform **≥0.5 s of loop work** on the
/// rust side clears it by orders of magnitude. Measured rust per-op cost (2026-10-17,
/// `/proc/loadavg` 5.7–7.7, `--full`, paired-median marginals, default baseline-JIT tier):
/// **1 441.1 ns** (`etf_roundtrip`), **2 409.7 ns** (`ets_copy`), **7 984.0 ns** (`send_recv`),
/// **2 065.3 ns** (`send_recv_plain`), so the counts below are ≈0.60–0.72 s of work per case:
/// `etf_roundtrip` 500 000 → ≈721 ms, `ets_copy` 250 000 → ≈602 ms, `send_recv` 90 000 →
/// ≈719 ms, `send_recv_plain` 300 000 → ≈620 ms. The oracle's `n_sys` stays far larger (its own
/// marginals are ≈0.9–1.2 s). The metric is normalized **per op**, so the counts only have to
/// keep both marginals measurable, and the deterministic `case_<name>_ops` companion records the
/// `N` behind every figure — never compare raw `wall_ms` across sides with different `N`.
/// Maintenance rule: **when a case's measured per-op cost changes materially, re-derive `n_rust`
/// so the loop work stays ≥0.5 s**. A loop count whose marginal falls below the timer noise
/// yields **no** per-op metric (an honest note), never a number.
static CASES: &[Case] = &[
    Case { name: "etf_roundtrip", function: ETF, n_rust: 500_000, n_sys: 1_500_000 },
    Case { name: "ets_copy", function: ETS, n_rust: 250_000, n_sys: 2_000_000 },
    Case { name: "send_recv", function: SR, n_rust: 90_000, n_sys: 500_000 },
    Case { name: "send_recv_plain", function: SR_PLAIN, n_rust: 300_000, n_sys: 1_000_000 },
];

/// The `copy_bound` workload.
pub struct CopyBound;

/// The number of timed samples per case for a profile.
fn runs_for(profile: Profile) -> usize {
    match profile {
        Profile::Quick => 3,
        Profile::Full => 10,
    }
}

/// The embedded Erlang module, generated from the constants above.
///
/// Every case validates its copy round-trip with a bound-variable pattern (`case … of T ->`
/// / `[{k, T}] ->`) — a real equality test that raises on a corrupted copy — and adds
/// `PER_OP` per validated op, so `f(N) == N * PER_OP` on both runtimes.
fn module_source() -> String {
    let list_items: Vec<String> = (1..=PAYLOAD_LIST_LEN).map(|n| n.to_string()).collect();
    let list_items = list_items.join(",");
    let bin_bytes: Vec<String> = (1..=PAYLOAD_BIN_BYTES).map(|n| n.to_string()).collect();
    let bin_bytes = bin_bytes.join(",");
    format!(
        "\
-module({MODULE}).
-export([{ETF}/{ARITY}, {ETS}/{ARITY}, {SR}/{ARITY}, {SR_PLAIN}/{ARITY}]).

%% The one structured payload every case copies — a map with a {PAYLOAD_LIST_LEN}-element list,
%% a tuple (atom + binary + float), a {PAYLOAD_BIN_BYTES}-byte binary, a char-list string and a
%% negative integer. Generated from this crate's constants, so the sides cannot drift.
payload() ->
    #{{a => [{list_items}],
      b => {{ok, <<\"hello world\">>, {PAYLOAD_FLOAT}}},
      c => <<{bin_bytes}>>,
      d => \"{PAYLOAD_STRING}\",
      e => {PAYLOAD_NEG}}}.

%% term_to_binary/1 + binary_to_term/1: the External Term Format copy boundary (A.5). The bound
%% `T` is an equality test, so a corrupted round-trip raises; {ETF}(N) == N * {PER_OP}.
{ETF}(N) -> {ETF}(N, 0).

{ETF}(0, Acc) -> Acc;
{ETF}(N, Acc) ->
    T = payload(),
    B = term_to_binary(T),
    case binary_to_term(B) of
        T -> {ETF}(N - 1, Acc + {PER_OP})
    end.

%% ets:insert/2 + ets:lookup/2: the ETS copy boundary, with one table and key per process so the
%% table never grows (the work is the copy, not table growth).
{ETS}(N) ->
    Tab = ets:new({MODULE}, [set, public]),
    Result = {ETS}(N, Tab, 0),
    ets:delete(Tab),
    Result.

{ETS}(0, _Tab, Acc) -> Acc;
{ETS}(N, Tab, Acc) ->
    T = payload(),
    true = ets:insert(Tab, {{k, T}}),
    case ets:lookup(Tab, k) of
        [{{k, T}}] -> {ETS}(N - 1, Tab, Acc + {PER_OP})
    end.

%% Two-process send/receive: the term is copied onto the worker's heap and back, so both
%% directions of the process copy boundary are exercised. The `after` guard makes a missing
%% reply a loud failure instead of a hang.
{SR}(N) ->
    Worker = spawn(fun() -> worker() end),
    Result = {SR}(N, Worker, 0),
    Worker ! stop,
    Result.

{SR}(0, _Worker, Acc) -> Acc;
{SR}(N, Worker, Acc) ->
    T = payload(),
    Worker ! {{self(), T}},
    receive
        {{Worker, T2}} ->
            case T2 of
                T -> {SR}(N - 1, Worker, Acc + {PER_OP})
            end
    after {RECV_TIMEOUT_MS} ->
        exit(message_timeout)
    end.

worker() ->
    receive
        {{From, T}} ->
            From ! {{self(), binary_to_term(term_to_binary(T))}},
            worker();
        stop ->
            ok
    end.

%% The delivery-only sibling: identical shape, but the worker replies the payload DIRECTLY (no
%% term_to_binary/binary_to_term), so subtracting it from {SR} attributes the codec component
%% inside the two-process path. Same bound-variable validation and `after` guard.
{SR_PLAIN}(N) ->
    Worker = spawn(fun() -> worker_plain() end),
    Result = {SR_PLAIN}(N, Worker, 0),
    Worker ! stop,
    Result.

{SR_PLAIN}(0, _Worker, Acc) -> Acc;
{SR_PLAIN}(N, Worker, Acc) ->
    T = payload(),
    Worker ! {{self(), T}},
    receive
        {{Worker, T2}} ->
            case T2 of
                T -> {SR_PLAIN}(N - 1, Worker, Acc + {PER_OP})
            end
    after {RECV_TIMEOUT_MS} ->
        exit(message_timeout)
    end.

worker_plain() ->
    receive
        {{From, T}} ->
            From ! {{self(), T}},
            worker_plain();
        stop ->
            ok
    end.
"
    )
}

/// The number of copy round-trips one case's `n`-op run performs.
fn ops(n: i64) -> i64 {
    n
}

/// The stdout a run at `n` ops must print (the deterministic constant).
fn expected_stdout(n: i64) -> String {
    (n * PER_OP).to_string()
}

/// The per-side argv for one case at `n` ops (everything after the program path).
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
            format!("io:format(\"~w~n\",[{MODULE}:{}({n})]), halt(0).", case.function),
        ],
    }
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
        Ok(out) => return Err(format!("erlc exited {:?}: {}", out.exit_code, out.stderr.trim())),
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
/// samples, the first `Work`-pass stdout, the capability-gap flag, the failure/variation notes,
/// and the resolved [`Marginal`] (the per-op figure, or why there is none).
struct CaseOutcome {
    n: i64,
    pairs: Vec<(f64, f64)>,
    rss: Vec<f64>,
    observed: Option<String>,
    unsupported: bool,
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
        (_, Some(code)) => format!("case {} ({}): exited {code}: {stderr}", case.name, kind.as_str()),
        (_, None) => format!("case {} ({}): terminated by signal: {stderr}", case.name, kind.as_str()),
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
    let pairs: Vec<(f64, f64)> = sampled.pairs.iter().map(|(w, s)| (w.wall_ms, s.wall_ms)).collect();
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
        notes,
    }
}

/// Runs one case under one runtime: `runs` interleaved rounds of an `n`-op sample and a `0`-op
/// sample, per [`marginal`]'s sanctioned schedule.
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
    measure_case(n, runs, |pass| {
        let count = n * pass.units();
        let mut cmd = Command::new(program);
        cmd.args(argv_for(kind, dir, case, count));
        let (pass_name, kind_name) = (pass.as_str(), kind.as_str());
        let completed = proc::run(&mut cmd).map_err(|e| PassError {
            note: format!("case {} ({kind_name}): {pass_name}-pass spawn failed: {e}", case.name),
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
        let expected = expected_stdout(count);
        let got = completed.stdout_trim().to_string();
        let mut notes = Vec::new();
        if got != expected {
            notes.push(format!("case {}: the {pass_name} run printed `{got}` (expected `{expected}`)", case.name));
        }
        work_sample_seen |= pass == Pass::Work;
        Ok(Sample {
            wall_ms: completed.wall_ms,
            peak_memory_bytes: completed.peak_memory_bytes,
            stdout: got,
            notes,
        })
    })
}

/// Emits one case's metric rows: the wall / startup / peak-RSS sample rows, the per-op
/// marginal (or the note explaining why there is none) and the deterministic `ops` companion.
fn push_case_metrics(result: &mut WorkloadResult, case: &Case, outcome: &CaseOutcome) {
    let wall: Vec<f64> = outcome.pairs.iter().map(|&(work_ms, _)| work_ms).collect();
    let startup: Vec<f64> = outcome.pairs.iter().map(|&(_, startup_ms)| startup_ms).collect();
    // One `Metric::samples` row per non-empty sample set (a round can be cut short, so only push
    // what we have).
    let rows = [("wall_ms", "ms", &wall), ("startup_ms", "ms", &startup), ("peak_rss_bytes", "bytes", &outcome.rss)];
    for (suffix, unit, values) in rows {
        if !values.is_empty() {
            result.metrics.push(Metric::samples(format!("case_{}_{suffix}", case.name), unit, values));
        }
    }
    match outcome.marginal.unit_cost_ns() {
        Some(ns) => {
            result.metrics.push(Metric::scalar(format!("case_{}_ns_per_op", case.name), "ns", ns));
            let detail = outcome.marginal.detail();
            result.notes.push(format!("case {}: {detail}", case.name));
        }
        None => {
            let note = outcome.marginal.below_resolution_note();
            result.notes.push(format!("case {}: {note}", case.name));
        }
    }
    // The deterministic, contention-immune companion: the copy round-trips the timing came from.
    result.metrics.push(Metric::scalar(format!("case_{}_ops", case.name), "ops", ops(outcome.n) as f64));
}

impl Workload for CopyBound {
    fn id(&self) -> &'static str {
        "copy_bound"
    }

    fn description(&self) -> &'static str {
        "term-copy/ETF round-trips under each runtime (per-op copy cost)"
    }

    fn category(&self) -> Category {
        Category::Messaging
    }

    fn contracts(&self) -> &'static [&'static str] {
        // Real Erlang semantics (A.1) over the External Term Format wire codec (A.5).
        &["A.1", "A.5"]
    }

    fn runtimes(&self) -> Runtimes {
        // The whole point: both runtimes execute the same compiled copy programs.
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

        // The module source is ours, so a staging/compile failure is a real failure, never
        // a missing prerequisite.
        let staged = match stage(dir, &erlc) {
            Ok(beam) => format!(
                "staged {} (embedded source, per-op increment={PER_OP}, erlc={})",
                beam.display(),
                erlc.display()
            ),
            Err(reason) => {
                let mut result =
                    WorkloadResult::failed(id, kind, category, description, "module-compile-failed");
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

        for case in CASES {
            let outcome = execute_case(program, kind, dir, case, runs);
            if outcome.ok() {
                ok_cases += 1;
                push_case_metrics(&mut result, case, &outcome);
                for &bytes in &outcome.rss {
                    overall_peak = Some(overall_peak.map_or(bytes as u64, |peak| peak.max(bytes as u64)));
                }
                if let Some(value) = &outcome.observed {
                    observed_all.push(format!("{}={value}", case.name));
                }
                result.notes.push(format!(
                    "case {}: N={} ({}) copy round-trips of the {PAYLOAD_KEYS}-key payload",
                    case.name,
                    outcome.n,
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
            result.notes.push(format!("observed stdout: {observed} (each case prints N * {PER_OP})"));
        }
        result.notes.push(
            "method: ns/op = median(wall at N ops - wall at 0 ops) / N * 1e6, from \
             interleaved paired rounds (each round runs the N-op sample and then the 0-op \
             sample back to back, so a drifting boot cost cancels inside the pair); N is per \
             runtime so both marginals are measurable, and the fixed process startup (incl. \
             the Rust runtime's code-path resolution) cancels out of every pair; the figure is \
             withheld when the median does not clear the noise band; every case validates its \
             copy round-trip with a bound-variable pattern"
                .to_string(),
        );
        result.notes.push(
            "note: the op count is deterministic and contention-immune; the wall-clock rows \
             move with host load"
                .to_string(),
        );
        result.notes.push(format!(
            "cases: {ok_cases} ok, {unsupported_cases} unsupported (of {})",
            CASES.len()
        ));

        if ok_cases == 0 {
            let (status, reason) = if unsupported_cases == CASES.len() {
                (Status::Skipped, "unsupported-case-set")
            } else {
                (Status::Failed, "no-copy-bound-case-ran")
            };
            result.status = status;
            result.reason = Some(reason.to_string());
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_stable() {
        let w = CopyBound;
        assert_eq!(w.id(), "copy_bound");
        assert_eq!(w.category(), Category::Messaging);
        assert_eq!(w.category().as_str(), "messaging");
        assert_eq!(w.contracts(), &["A.1", "A.5"]);
        assert!(matches!(w.runtimes(), Runtimes::Both));
        assert!(w.runtimes().allows(RuntimeKind::Rust));
        assert!(w.runtimes().allows(RuntimeKind::Sys));
        assert!(!w.description().is_empty());
    }

    #[test]
    fn every_case_declares_a_calibrated_n() {
        assert_eq!(CASES.len(), 4, "etf + ets + send/receive + delivery-only send/receive");
        let mut functions: Vec<&str> = CASES.iter().map(|c| c.function).collect();
        functions.sort_unstable();
        functions.dedup();
        assert_eq!(functions.len(), CASES.len(), "case functions must be distinct");
        for case in CASES {
            assert!(!case.name.is_empty(), "a case has no name");
            assert!(module_source().contains(&format!("{}(N) ->", case.function)), "{}", case.name);
            assert!(case.n_for(RuntimeKind::Rust) > 0 && case.n_for(RuntimeKind::Sys) > 0, "{}", case.name);
            // The oracle is the faster runtime, so it always gets the larger loop count.
            assert!(case.n_sys > case.n_rust, "{}: the oracle loop must be the larger one", case.name);
        }
    }

    /// Every rust loop count must do ≥0.5 s of work so its paired marginal stands clear of the
    /// measurement noise, and ≥3× the conservative noise floor below.
    ///
    /// The floor is uniform: with the interleaved (paired) schedule the residual noise is the MAD of
    /// the paired differences — a few milliseconds — so no case needs extra headroom. Do **not**
    /// reintroduce a per-case special case (`etf_roundtrip` used to demand ≳1.5 s, which existed
    /// only because the *old sequential* schedule's marginal collapsed inside the boot-jitter band).
    #[test]
    fn the_rust_loop_counts_clear_the_boot_jitter_band() {
        // A conservative bound on the noise a paired marginal must clear: the measured MAD of the
        // paired differences is a few milliseconds, well under this 50 ms floor.
        const NOISE_FLOOR_S: f64 = 0.05;
        // Per-op costs measured on the reference host (2026-10-17, `--full`, paired-median
        // marginals, default baseline-JIT tier) — see the case table's doc comment.
        const COST_NS: &[(&str, f64, i64)] = &[
            ("etf_roundtrip", 1_441.1, 500_000),
            ("ets_copy", 2_409.7, 250_000),
            ("send_recv", 7_984.0, 90_000),
            ("send_recv_plain", 2_065.3, 300_000),
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
        assert!(sys.iter().any(|a| a.contains(&format!("{MODULE}:{}(9)", case.function))));
    }    /// `send_recv_plain` is the isolation case that attributes the ETF codec component of
    /// `send_recv` *inside* the two-process path: it mirrors `send_recv`'s bound-variable
    /// validation and `after` guard exactly, performs **no** codec call, and is a sibling of it
    /// in the fixed table with a calibrated loop count per runtime.
    #[test]
    fn the_delivery_only_case_mirrors_send_recv_without_the_codec() {
        let src = module_source();
        let plain = src
            .split(&format!("{SR_PLAIN}(N) ->"))
            .nth(1)
            .expect("the delivery-only case is generated");
        for needle in ["Worker ! {self(), T},", "case T2 of", "exit(message_timeout)"] {
            assert!(plain.contains(needle), "missing `{needle}`");
        }
        assert!(!plain.contains("term_to_binary"), "no ETF codec call may remain");
        let case = CASES
            .iter()
            .find(|c| c.name == "send_recv_plain")
            .expect("the delivery-only case is in the fixed table");
        assert_eq!(case.function, SR_PLAIN);
        assert!(case.n_for(RuntimeKind::Sys) > case.n_for(RuntimeKind::Rust));
    }

    /// A fake pass sample: `work_ms` on the `Work` pass, `startup_ms` on the `Startup` pass,
    /// the deterministic stdout of `units × n`, no RSS, no notes.
    fn fake_sample(pass: Pass, work_ms: f64, startup_ms: f64, n: i64) -> Sample {
        let wall_ms = if pass == Pass::Work { work_ms } else { startup_ms };
        Sample { wall_ms, peak_memory_bytes: None, stdout: expected_stdout(n * pass.units()), notes: Vec::new() }
    }

    /// `CASES[0]`'s metrics pushed into a fresh result, from a steady fake clock whose `Work`
    /// pass costs `delta_ms` (ms) more than its `Startup` pass, over `runs` rounds.
    fn measured(delta_ms: f64, runs: usize) -> WorkloadResult {
        let outcome = measure_case(1_000, runs, |pass| Ok(fake_sample(pass, 100.0 + delta_ms, 100.0, 1_000)));
        let mut result = WorkloadResult::new("copy_bound", RuntimeKind::Rust, Category::Messaging, "test");
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

    /// The schedule `copy_bound` uses: every round is a `Work` sample *immediately* followed by
    /// its `Startup` sample — never a whole pass at a time (the old sequential all-`N`-then-
    /// all-`0` schedule is what let host drift inflate the published gap ≈56×). A clear
    /// marginal then publishes its figure together with the auditable paired evidence.
    #[test]
    fn rounds_are_paired_work_then_startup() {
        let mut order = Vec::new();
        let outcome = measure_case(1_000, 3, |pass| {
            order.push(pass);
            Ok(fake_sample(pass, 110.0, 100.0, 1_000))
        });
        assert_eq!(order, [Pass::Work, Pass::Startup].repeat(3), "one round is Work then Startup");
        assert_eq!(outcome.pairs, vec![(110.0, 100.0); 3]);
        assert!(outcome.ok() && outcome.notes.is_empty() && !outcome.unsupported);
        // 10 ms of work over 1 000 ops = 10 000 ns/op, well clear of the noise band.
        assert_eq!((outcome.marginal.rounds, outcome.marginal.unit_cost_ns()), (3, Some(10_000.0)));

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
                let note = "case etf_roundtrip (rust): exited 1: boom (startup pass)".to_string();
                return Err(PassError { note, unsupported: false });
            }
            Ok(fake_sample(pass, 110.0, 100.0, 1_000))
        });
        assert_eq!(outcome.pairs, vec![(110.0, 100.0)], "round 1 only");
        assert!(outcome.ok(), "one completed round is still a usable sample");
        assert_eq!(outcome.marginal.rounds, 1);
        assert!(!outcome.unsupported);
        assert!(outcome.notes.iter().any(|n| n.contains("boom")), "{:?}", outcome.notes);

        // A capability gap on the very first `Work` pass: nothing measured ⇒ unsupported.
        let gap = || PassError { note: "case etf_roundtrip: unsupported: unsupported_bif".into(), unsupported: true };
        let outcome = measure_case(1_000, 2, |_pass| Err::<Sample, PassError>(gap()));
        assert!(outcome.pairs.is_empty() && !outcome.ok() && outcome.unsupported);
        assert_eq!(outcome.marginal.unit_cost_ns(), None);
        assert!(outcome.notes.iter().any(|n| n.contains("unsupported_bif")));
    }

    /// A noisy sample set publishes **no** `ns_per_op` metric — only the withholding note — so
    /// a drift artifact can never reach the report.
    #[test]
    fn the_metric_is_withheld_inside_the_noise_band() {
        // A steady 1 ms of "work" per round is below NOISE_MULTIPLE × MIN_NOISE_MS.
        let result = measured(1.0, 3);
        assert!(case_metric(&result, "ns_per_op").is_none(), "no figure inside the noise band");
        // The deterministic companion and the raw sample rows are still published.
        for suffix in ["ops", "wall_ms", "startup_ms"] {
            assert!(case_metric(&result, suffix).is_some(), "{suffix}");
        }
        let note = case_note(&result, "marginal below timer resolution").expect("the withholding note");
        assert!(note.contains("paired median=1.000 ms"), "{note}");
    }

    #[test]
    fn the_op_count_is_the_deterministic_loop_count() {
        assert_eq!(ops(0), 0);
        assert_eq!(ops(1), 1);
        assert_eq!(ops(CASES[0].n_for(RuntimeKind::Rust)), CASES[0].n_rust);
        assert_eq!(ops(CASES[0].n_for(RuntimeKind::Sys)), CASES[0].n_sys);
        // The deterministic stdout: a run at N ops printed N * PER_OP.
        assert_eq!(expected_stdout(0), "0");
        assert_eq!(expected_stdout(1), PER_OP.to_string());
        assert_eq!(expected_stdout(3), (3 * PER_OP).to_string());
    }

    /// The generated module pins the shared constants, each case is a top-level clause pair,
    /// and it uses only the constructs `beam_rt` supports today (the ETF codec, ETS
    /// insert/lookup and two-process send/receive with and without the codec in the receiver).
    /// The delivery-only worker replies the payload directly; the map it copies has exactly
    /// `PAYLOAD_KEYS` top-level entries (the number the per-case note reports).
    #[test]
    fn the_embedded_module_pins_the_shared_constants() {
        let src = module_source();
        assert!(src.starts_with(&format!("-module({MODULE}).")), "{src}");
        assert!(src.contains(&format!(
            "-export([{ETF}/{ARITY}, {ETS}/{ARITY}, {SR}/{ARITY}, {SR_PLAIN}/{ARITY}])."
        )));
        for needle in [
            "10,11,12".to_string(),
            PAYLOAD_STRING.to_string(),
            PAYLOAD_NEG.to_string(),
            PAYLOAD_FLOAT.to_string(),
            format!("Acc + {PER_OP}"),
            format!("after {RECV_TIMEOUT_MS} ->"),
            format!("{ETF}(0, Acc) -> Acc;"),
            format!("{ETS}(0, _Tab, Acc) -> Acc;"),
            format!("{SR}(0, _Worker, Acc) -> Acc;"),
            format!("{SR_PLAIN}(0, _Worker, Acc) -> Acc;"),
            // The delivery-only worker replies the payload directly; the codec round trip stays
            // confined to the `send_recv` worker.
            "From ! {self(), T},".to_string(),
            "From ! {self(), binary_to_term(term_to_binary(T))},".to_string(),
            "term_to_binary(".to_string(),
            "binary_to_term(".to_string(),
            "ets:new(".to_string(),
            "ets:insert(".to_string(),
            "ets:lookup(".to_string(),
            "ets:delete(".to_string(),
            "spawn(".to_string(),
            "!".to_string(),
            "receive".to_string(),
        ] {
            assert!(src.contains(&needle), "missing `{needle}`");
        }
        // The payload's body ends at the first `}.`, with one `=> ` per top-level key.
        let body = src
            .split("payload() ->")
            .nth(1)
            .expect("the payload clause is generated")
            .split("}.")
            .next()
            .expect("the payload body is terminated");
        assert_eq!(body.matches("=> ").count(), PAYLOAD_KEYS, "{body}");
    }

    #[test]
    fn stage_reports_a_write_failure_instead_of_silently_failing() {
        let missing = std::env::temp_dir().join(format!(
            "bench-copy-bound-absent-{}-xyzzy",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&missing);
        let err = stage(&missing, Path::new("/nonexistent/erlc"))
            .expect_err("a missing dir must be reported");
        assert!(err.contains("write"), "{err}");
    }

    /// Oracle-gated smoke test: with `erlc` and a resolvable runtime present, the cases
    /// must produce real timings (or an honest note). No-op when prerequisites are missing.
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
            let scratch = ScratchDir::new("bench-copy-bound-smoke").expect("scratch");
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
                    Some(expected_stdout(outcome.n).as_str()),
                    "runtime {:?}: the copy case must print the deterministic constant",
                    runtime.kind
                );
            }
        }
    }
}
