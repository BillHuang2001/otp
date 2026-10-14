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
//! Every repo-backed workload (Cowboy, Khepri, EMQX, PLB, the compilers) is `sys`-only, so
//! the only cross-runtime signals were `list_walk`/`foreign`, both **compute**-bound. The
//! rewrite surface that `ERTS_RUST_REWRITE_TEST_PROJECTS.md` calls the *messaging/copy
//! boundary* — the point where a term must leave its owning process (ETF encoding, ETS,
//! process-to-process send) — had no cross-runtime measurement at all. This workload
//! supplies it.
//!
//! # The four cases (all in one embedded module)
//!
//! Each case performs the copy round-trip of the **same structured payload**, verifies the
//! result with an Erlang bound-variable pattern (a real equality test that raises on a
//! mismatch), and adds `PER_OP` to an accumulator, so a run at `N` ops prints exactly
//! `N * PER_OP`:
//!
//! * `etf_roundtrip` — `term_to_binary/1` + `binary_to_term/1`: the **External Term Format
//!   wire codec** (contract A.5) — the encoding a distributed node puts on the line.
//! * `ets_copy` — `ets:insert/2` + `ets:lookup/2`: the **ETS copy boundary** (the term is
//!   copied into the table and copied back out). One table and one key per process, so the
//!   table never grows and the measured work is the copy, not table growth.
//! * `send_recv` — a term is sent to a second process, round-tripped through
//!   `term_to_binary/1` there, and sent back: **both directions** of the process copy
//!   boundary. An `after` guard turns a missing reply into a loud failure, never a hang.
//! * `send_recv_plain` — the **same two-process delivery shape as `send_recv` with *no*
//!   ETF codec call in the receiver**: the worker replies the payload *directly*
//!   (`From ! {self(), T}`). This isolates pure two-way process copy/delivery from the
//!   codec, so `send_recv − send_recv_plain` attributes the ETF component *inside* the
//!   two-process path (it mirrors the `beam_process`/`beam_message` delivery shape the
//!   sibling crates asked for). Identical bound-variable validation and identical
//!   `after`-guard: a missing reply is a loud `exit(message_timeout)`, never a hang.
//!
//! The payload is *generated from this crate's constants* (see [`module_source`]), so the
//! Rust and Erlang sides cannot drift.
//!
//! # The measurement (startup-cancelled, per op)
//!
//! The two runtimes differ by one to two orders of magnitude per op, so one fixed loop
//! count cannot give both a measurable marginal: an `N` that is a real signal for the Rust
//! runtime is below the oracle's own run-to-run startup noise. Each case therefore declares
//! a **per-runtime loop count** `N` and the workload runs the identical program twice per
//! sample:
//!
//! * at `N` ops ⇒ `case_<name>_wall_ms` (end-to-end process wall time),
//! * at `0` ops ⇒ `case_<name>_startup_ms` (the same program's pure startup/boot cost — the
//!   Rust runtime resolves its system code path by spawning `erl` once, so this is not zero),
//!
//! and derives the throughput metric
//!
//! ```text
//! case_<name>_ns_per_op = (mean(wall_ms) - mean(startup_ms)) / N * 1e6
//! ```
//!
//! which cancels the fixed process-startup cost from both sides. It is emitted only when
//! the marginal is positive; otherwise a note records that the marginal is below timer
//! resolution — never a fabricated or negative number.
//!
//! Because the *timing* is only as stable as the host, the workload also emits
//! `case_<name>_ops = N`, the deterministic number of copy round-trips the figure was
//! derived from. That metric is **contention-immune**: it stays correct and comparable on
//! a loaded machine (a shared benchmark host moves the wall-clock rows several-fold).
//!
//! # Honest reporting
//!
//! * Every runtime's stdout must equal the deterministic `N * PER_OP`; a mismatch is
//!   recorded as a note.
//! * A Rust exit `3` (`beam_rt: unsupported: …`) records the capability gap and emits
//!   **no** timing for that case — the row is never a fabricated number.
//! * `erlc` unavailable ⇒ an honest `skipped(erlc-absent)`.
//! * The embedded module failing to compile is a **failure**
//!   (`module-compile-failed`), never a skip: the source is ours.
//!
//! Everything is staged in a private, self-cleaning [`ScratchDir`] under `$TMPDIR`; the
//! workload never writes outside it and never panics.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::metrics::{Category, Metric, Probe, Runtimes, Status};
use crate::proc::{self, Completed};
use crate::report::{Profile, WorkloadResult};
use crate::runtime::RuntimeKind;
use crate::scratch::ScratchDir;
use crate::stats::mean;
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

/// The number of list elements in the copied payload.
const PAYLOAD_LIST_LEN: i64 = 10;

/// The number of bytes in the copied payload's binary.
const PAYLOAD_BIN_BYTES: i64 = 12;

/// The copied payload's float (the ETF codec's only non-immediate numeric case).
const PAYLOAD_FLOAT: f64 = 3.5;

/// The copied payload's char-list string.
const PAYLOAD_STRING: &str = "a char list";

/// The copied payload's negative small integer.
const PAYLOAD_NEG: i64 = -42;

/// The number of top-level keys in the copied payload (the map built by `payload/0`).
const PAYLOAD_KEYS: usize = 5;
/// How much one validated copy round-trip adds to a case's accumulator, so a run at `N`
/// ops prints `N * PER_OP`. The Erlang source is generated from it.
const PER_OP: i64 = 1;

/// How long the two-process case waits for the worker's reply before failing loudly.
///
/// A missing reply must abort the case (the run exits non-zero and is reported), never
/// hang the harness: the driver invokes the runtime with no process timeout of its own.
const RECV_TIMEOUT_MS: u32 = 30_000;

/// One copy-bound case: the embedded driver function that loops over the copy round-trip.
struct Case {
    /// Stable, short case name (used in the metric names, e.g. `case_<name>_wall_ms`).
    name: &'static str,
    /// The exported driver function (arity 1: the loop count).
    function: &'static str,
    /// Loop count calibrated for the Rust runtime (a few hundred ms per run).
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
/// `n_rust`/`n_sys` were calibrated on the reference host (see the crate CONTEXT — a
/// heavily loaded shared machine) so that one run's *marginal* cost is a few hundred
/// milliseconds on the Rust runtime and ~0.5–2 s on the oracle, i.e. far above the
/// oracle's run-to-run startup noise. They are **constants, not measurements**, and the
/// reported metric is normalized per op; a loop count whose marginal falls below the timer
/// noise yields **no** per-op metric (an honest note), never a fabricated one.
static CASES: &[Case] = &[
    Case {
        name: "etf_roundtrip",
        function: ETF,
        n_rust: 5_000,
        n_sys: 1_500_000,
    },
    Case {
        name: "ets_copy",
        function: ETS,
        n_rust: 100_000,
        n_sys: 2_000_000,
    },
    Case {
        name: "send_recv",
        function: SR,
        n_rust: 2_000,
        n_sys: 500_000,
    },
    Case {
        name: "send_recv_plain",
        function: SR_PLAIN,
        n_rust: 2_000,
        n_sys: 1_000_000,
    },
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

%% The one structured payload every case copies: a map holding a {PAYLOAD_LIST_LEN}-element
%% list, a tuple (an atom + a binary + a float), a {PAYLOAD_BIN_BYTES}-byte binary, a
%% char-list string and a negative integer. It is generated from this crate's constants, so
%% the Rust and Erlang sides cannot drift.
payload() ->
    #{{a => [{list_items}],
      b => {{ok, <<\"hello world\">>, {PAYLOAD_FLOAT}}},
      c => <<{bin_bytes}>>,
      d => \"{PAYLOAD_STRING}\",
      e => {PAYLOAD_NEG}}}.

%% term_to_binary/1 + binary_to_term/1: the External Term Format copy boundary (A.5). The
%% bound `T` in the case pattern is an equality test, so a corrupted round-trip raises.
%% Each validated round-trip adds {PER_OP}, so {ETF}(N) == N * {PER_OP}.
{ETF}(N) -> {ETF}(N, 0).

{ETF}(0, Acc) -> Acc;
{ETF}(N, Acc) ->
    T = payload(),
    B = term_to_binary(T),
    case binary_to_term(B) of
        T -> {ETF}(N - 1, Acc + {PER_OP})
    end.

%% ets:insert/2 + ets:lookup/2: the ETS copy boundary (copied into the table, copied back
%% out). One table and one key per process, so the table never grows.
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
%% directions of the process copy boundary are exercised. An `after` guard makes a missing
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

%% The delivery-only two-process case: identical shape to {SR}/1, but the worker replies
%% the payload DIRECTLY (no term_to_binary/binary_to_term), so the case measures pure
%% two-way process copy/delivery. Subtracting it from {SR} attributes the ETF codec
%% component inside the two-process path. The same `after` guard applies.
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
    let beam = dir.join(format!("{MODULE}.beam"));
    match kind {
        RuntimeKind::Rust => vec![
            "-pa".to_string(),
            dir.display().to_string(),
            "run".to_string(),
            beam.display().to_string(),
            format!("{MODULE}:{}/1", case.function),
            n.to_string(),
        ],
        RuntimeKind::Sys => vec![
            "-noshell".to_string(),
            "-pa".to_string(),
            dir.display().to_string(),
            "-eval".to_string(),
            format!(
                "io:format(\"~w~n\",[{MODULE}:{}({n})]), halt(0).",
                case.function
            ),
        ],
    }
}

/// Writes the embedded module into `dir` and compiles it with `erlc`.
///
/// Returns `Err(reason)` — never a silent `false` — when the source cannot be written or
/// the module does not compile, so the caller can report a real failure.
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

/// The samples collected for one case under one runtime.
struct CaseOutcome {
    /// The loop count used.
    n: i64,
    /// Wall-clock samples (ms) of the `n`-op runs.
    wall: Vec<f64>,
    /// Wall-clock samples (ms) of the `0`-op (startup-only) runs.
    startup: Vec<f64>,
    /// Peak-RSS samples (bytes) of the `n`-op runs.
    rss: Vec<f64>,
    /// The stdout observed on the first successful `n`-op run.
    observed: Option<String>,
    /// Whether the runtime reported the case as `unsupported` (Rust exit `3`).
    unsupported: bool,
    /// Notes about failures/variations (never silent).
    notes: Vec<String>,
}

impl CaseOutcome {
    /// Whether the case produced at least one usable timing.
    fn ok(&self) -> bool {
        !self.wall.is_empty()
    }
}

/// The startup-cancelled marginal cost per op, in nanoseconds — `None` when either sample
/// set is unusable or the marginal is not positive (below timer resolution).
fn ns_per_op(wall: &[f64], startup: &[f64], n: i64) -> Option<f64> {
    if wall.is_empty() || startup.is_empty() || n <= 0 {
        return None;
    }
    let marginal = mean(wall) - mean(startup);
    if marginal <= 0.0 {
        None
    } else {
        Some(marginal / n as f64 * 1_000_000.0)
    }
}

/// Classifies a non-zero / killed run into a human note.
fn classify_failure(kind: RuntimeKind, case: &Case, completed: &Completed) -> String {
    let stderr = completed.stderr.trim();
    match (kind, completed.exit_code) {
        (RuntimeKind::Rust, Some(3)) => {
            let reason = stderr
                .strip_prefix("beam_rt: unsupported: ")
                .unwrap_or(stderr);
            format!("case {}: unsupported: {reason}", case.name)
        }
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

/// Runs one case at `n` ops and at `0` ops, `runs` times each, collecting samples.
fn execute_case(
    program: &Path,
    kind: RuntimeKind,
    dir: &Path,
    case: &Case,
    runs: usize,
) -> CaseOutcome {
    let n = case.n_for(kind);
    let mut outcome = CaseOutcome {
        n,
        wall: Vec::new(),
        startup: Vec::new(),
        rss: Vec::new(),
        observed: None,
        unsupported: false,
        notes: Vec::new(),
    };
    let expected = expected_stdout(n);

    // Pass 1: the `n`-op runs (the measured work).
    for _ in 0..runs {
        let mut cmd = Command::new(program);
        cmd.args(argv_for(kind, dir, case, n));
        match proc::run(&mut cmd) {
            Ok(completed) if completed.success => {
                outcome.wall.push(completed.wall_ms);
                if let Some(bytes) = completed.peak_memory_bytes {
                    outcome.rss.push(bytes as f64);
                }
                let got = completed.stdout_trim().to_string();
                if got != expected {
                    outcome.notes.push(format!(
                        "case {}: unexpected stdout `{got}` (expected `{expected}`)",
                        case.name
                    ));
                }
                if outcome.observed.is_none() {
                    outcome.observed = Some(got);
                }
            }
            Ok(completed) => {
                if kind == RuntimeKind::Rust
                    && completed.exit_code == Some(3)
                    && outcome.wall.is_empty()
                {
                    outcome.unsupported = true;
                }
                outcome.notes.push(classify_failure(kind, case, &completed));
                break;
            }
            Err(e) => {
                outcome.notes.push(format!(
                    "case {} ({}): spawn failed: {e}",
                    case.name,
                    kind.as_str()
                ));
                break;
            }
        }
    }

    // Pass 2: the `0`-op runs (startup only). Skipped when pass 1 already failed — a
    // broken case must not be timed at all.
    if outcome.ok() {
        let expected_startup = expected_stdout(0);
        for _ in 0..runs {
            let mut cmd = Command::new(program);
            cmd.args(argv_for(kind, dir, case, 0));
            match proc::run(&mut cmd) {
                Ok(completed) if completed.success => {
                    outcome.startup.push(completed.wall_ms);
                    let got = completed.stdout_trim().to_string();
                    if got != expected_startup {
                        outcome.notes.push(format!(
                            "case {}: the startup run printed `{got}` (expected `{expected_startup}`)",
                            case.name
                        ));
                    }
                }
                Ok(completed) => {
                    outcome.notes.push(format!(
                        "case {}: startup run failed: {}",
                        case.name,
                        classify_failure(kind, case, &completed)
                    ));
                    break;
                }
                Err(e) => {
                    outcome.notes.push(format!(
                        "case {}: startup spawn failed: {e}",
                        case.name
                    ));
                    break;
                }
            }
        }
    }

    outcome
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
        let Some(program) = ctx.runtime.program() else {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "runtime-unsupported",
            );
        };
        let Some(erlc) = slice::resolve_erlc() else {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "erlc-absent",
            );
        };

        let scratch = match ScratchDir::new(self.id()) {
            Ok(dir) => dir,
            Err(e) => {
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    format!("scratch-dir-failed: {e}"),
                )
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
                let mut result = WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    "module-compile-failed",
                );
                result.notes.push(reason);
                return result;
            }
        };

        let runs = runs_for(ctx.profile);
        let mut result = WorkloadResult::new(self.id(), kind, self.category(), self.description());
        result.notes.push(staged);

        let mut overall_peak: Option<u64> = None;
        let mut observed_all: Vec<String> = Vec::new();
        let mut ok_cases = 0usize;
        let mut unsupported_cases = 0usize;

        for case in CASES {
            let outcome = execute_case(program, kind, dir, case, runs);
            if outcome.ok() {
                ok_cases += 1;
                result.metrics.push(Metric::samples(
                    format!("case_{}_wall_ms", case.name),
                    "ms",
                    &outcome.wall,
                ));
                if !outcome.startup.is_empty() {
                    result.metrics.push(Metric::samples(
                        format!("case_{}_startup_ms", case.name),
                        "ms",
                        &outcome.startup,
                    ));
                }
                if !outcome.rss.is_empty() {
                    result.metrics.push(Metric::samples(
                        format!("case_{}_peak_rss_bytes", case.name),
                        "bytes",
                        &outcome.rss,
                    ));
                    for &bytes in &outcome.rss {
                        let bytes = bytes as u64;
                        overall_peak = Some(overall_peak.map_or(bytes, |peak| peak.max(bytes)));
                    }
                }
                match ns_per_op(&outcome.wall, &outcome.startup, outcome.n) {
                    Some(ns) => result.metrics.push(Metric::scalar(
                        format!("case_{}_ns_per_op", case.name),
                        "ns",
                        ns,
                    )),
                    None => result.notes.push(format!(
                        "case {}: marginal below timer resolution; no ns_per_op metric",
                        case.name
                    )),
                }
                // The deterministic, contention-immune companion metric: how many copy
                // round-trips the timing above was derived from.
                result.metrics.push(Metric::scalar(
                    format!("case_{}_ops", case.name),
                    "ops",
                    ops(outcome.n) as f64,
                ));
                if let Some(value) = &outcome.observed {
                    observed_all.push(format!("{}={value}", case.name));
                }
                result.notes.push(format!(
                    "case {}: N={} ({}) copy round-trips of the {}-key payload",
                    case.name,
                    outcome.n,
                    kind.as_str(),
                    PAYLOAD_KEYS
                ));
            } else if outcome.unsupported {
                unsupported_cases += 1;
            }
            result.notes.extend(outcome.notes);
        }

        result.peak_memory_bytes = overall_peak;
        if !observed_all.is_empty() {
            result.notes.push(format!(
                "observed stdout: {} (each case prints N * {PER_OP})",
                observed_all.join(" ")
            ));
        }
        result.notes.push(
            "method: ns/op = (mean wall at N ops - mean wall at 0 ops) / N * 1e6; N is per \
             runtime so both marginals are measurable, and the fixed process startup (incl. \
             the Rust runtime's code-path resolution) cancels out of both sides; every case \
             validates its copy round-trip with a bound-variable pattern"
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
            if unsupported_cases == CASES.len() {
                result.status = Status::Skipped;
                result.reason = Some("unsupported-case-set".to_string());
            } else {
                result.status = Status::Failed;
                result.reason = Some("no-copy-bound-case-ran".to_string());
            }
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
        for case in CASES {
            assert!(!case.name.is_empty(), "a case has no name");
            assert!(!case.function.is_empty(), "{} has no function", case.name);
            assert!(case.n_for(RuntimeKind::Rust) > 0, "{}", case.name);
            assert!(case.n_for(RuntimeKind::Sys) > 0, "{}", case.name);
            // The oracle is the faster runtime, so it always gets the larger loop count.
            assert!(
                case.n_sys > case.n_rust,
                "{}: the oracle loop must be the larger one",
                case.name
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
        assert!(sys
            .iter()
            .any(|a| a.contains(&format!("{MODULE}:{}(9)", case.function))));
    }

    #[test]
    fn each_case_has_a_distinct_function_and_the_expected_arity() {
        let mut functions: Vec<&str> = CASES.iter().map(|c| c.function).collect();
        functions.sort_unstable();
        functions.dedup();
        assert_eq!(functions.len(), CASES.len(), "case functions must be distinct");
        for f in functions {
            assert!(module_source().contains(&format!("{f}(N) ->")));
        }
    }

    #[test]
    fn the_delivery_only_case_mirrors_send_recv_without_the_codec() {
        // `send_recv_plain` is the isolation case that attributes the ETF codec component
        // of `send_recv` *inside* the two-process path. It must mirror `send_recv`'s
        // bound-variable validation and its `after` timeout guard exactly, and its worker
        // must reply directly (no codec call in the receiver).
        let src = module_source();
        let plain = src
            .split(&format!("{SR_PLAIN}(N) ->"))
            .nth(1)
            .expect("the delivery-only case is generated");
        assert!(plain.contains("Worker ! {self(), T},"));
        assert!(plain.contains("case T2 of"));
        assert!(plain.contains(&format!("{SR_PLAIN}(0, _Worker, Acc) -> Acc;")));
        assert!(plain.contains(&format!(r"after {RECV_TIMEOUT_MS} ->")));
        assert!(plain.contains("exit(message_timeout)"));
        assert!(
            !plain.contains("term_to_binary"),
            "the delivery-only case must perform no ETF codec call"
        );
        // It is a sibling of `send_recv` in the fixed table, with a calibrated loop count
        // for each runtime.
        let case = CASES
            .iter()
            .find(|c| c.name == "send_recv_plain")
            .expect("the delivery-only case is in the fixed table");
        assert_eq!(case.function, SR_PLAIN);
        assert!(case.n_for(RuntimeKind::Rust) > 0);
        assert!(case.n_for(RuntimeKind::Sys) > case.n_for(RuntimeKind::Rust));
    }

    #[test]
    fn ns_per_op_cancels_startup_and_rejects_unusable_samples() {
        // 100 ms at N=1000 ops, 10 ms at 0 ⇒ 90 ms of copying over 1000 ops ⇒ 90 µs/op.
        assert_eq!(ns_per_op(&[100.0], &[10.0], 1_000), Some(90_000.0));
        // Equal or inverted (noise) ⇒ no metric, never a negative number.
        assert_eq!(ns_per_op(&[100.0], &[100.0], 1_000), None);
        assert_eq!(ns_per_op(&[90.0], &[100.0], 1_000), None);
        // Missing sample sets / a non-positive count ⇒ no metric.
        assert_eq!(ns_per_op(&[], &[1.0], 1_000), None);
        assert_eq!(ns_per_op(&[1.0], &[], 1_000), None);
        assert_eq!(ns_per_op(&[1.0], &[0.0], -1), None);
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

    #[test]
    fn the_module_source_pins_the_shared_constants() {
        let src = module_source();
        assert!(src.starts_with(&format!("-module({MODULE}).")), "{src}");
        assert!(src.contains(&format!(
            "-export([{ETF}/{ARITY}, {ETS}/{ARITY}, {SR}/{ARITY}, {SR_PLAIN}/{ARITY}])."
        )));
        // The payload is generated from the crate constants.
        assert!(src.contains("10,11,12"), "the binary bytes come from the constant");
        assert!(src.contains(PAYLOAD_STRING));
        assert!(src.contains(&PAYLOAD_NEG.to_string()));
        assert!(src.contains(&format!("{PAYLOAD_FLOAT}")));
        assert!(src.contains(&format!("Acc + {PER_OP}")));
        assert!(src.contains(&format!("after {RECV_TIMEOUT_MS} ->")));
        // Each case must be present as a top-level clause pair.
        assert!(src.contains(&format!("{ETF}(0, Acc) -> Acc;")));
        assert!(src.contains(&format!("{ETS}(0, _Tab, Acc) -> Acc;")));
        assert!(src.contains(&format!("{SR}(0, _Worker, Acc) -> Acc;")));
        assert!(src.contains(&format!("{SR_PLAIN}(0, _Worker, Acc) -> Acc;")));
        // The delivery-only worker replies the payload directly, with no codec call in the
        // receiver; the codec round-trip stays confined to the `send_recv` worker.
        assert!(src.contains("From ! {self(), T},"));
        assert!(src.contains("From ! {self(), binary_to_term(term_to_binary(T))},"));
    }

    #[test]
    fn the_module_source_uses_only_supported_constructs() {
        // The four copies are exactly the constructs `beam_rt` supports today: the ETF
        // codec, ETS insert/lookup, and two-process send/receive (with and without the
        // codec in the receiver).
        let src = module_source();
        for needle in [
            "term_to_binary(",
            "binary_to_term(",
            "ets:new(",
            "ets:insert(",
            "ets:lookup(",
            "ets:delete(",
            "spawn(",
            "!",
            "receive",
        ] {
            assert!(src.contains(needle), "missing construct `{needle}`");
        }
    }

    #[test]
    fn the_payload_has_the_declared_number_of_keys() {
        // The note reports PAYLOAD_KEYS, so the generated map must actually have that many
        // top-level entries (the payload's body ends at the first `}.`).
        let src = module_source();
        let body = src
            .split("payload() ->")
            .nth(1)
            .expect("the payload clause is generated")
            .split("}.")
            .next()
            .unwrap();
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
