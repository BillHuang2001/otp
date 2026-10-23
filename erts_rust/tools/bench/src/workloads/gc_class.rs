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

//! `gc_class` — **the GC-bound workload *classes***: the three shapes the GC-strategy A/B
//! harness (`erts_rust/crates/beam_rt/tests/gc_strategy_classes.rs`) measures, driven under
//! *both* runtimes (the system oracle and the Rust runtime `beam_rt`).
//!
//! The crate's other GC row, `gc_bound`, is an *allocation loop*: it measures how a runtime
//! pays for allocate-and-abandon garbage. That is one shape, not the class. This workload
//! supplies the **classes** — the shapes a real process actually puts on its heap — so a
//! collection cost can be read off workloads that are not allocation loops at all:
//!
//! * **C3 messaging/copy-bound** — `gc_class_copy_bound:send_recv/1` (a genuine two-process
//!   ping-pong: the term crosses the process copy boundary in both directions) and
//!   `gc_class_copy_bound:ets_copy/1` (the ETS copy boundary).
//! * **C4 a real in-process compile** — `gc_class_compile:run/1` compiles one real
//!   `lib/*/src/*.erl` source N times **inside the calling process**, so the compiler's own
//!   readers, passes and code emission allocate on the heap being measured.
//!
//! ## The payload is deliberately *above* the default nursery
//!
//! `copy_bound` copies a 70-word map: a wrapper around a module literal, small enough that a
//! runtime can serve the whole round trip without admitting a collection at all (that row is
//! about the *copy*, and its docs say so). The fixtures here are used **unchanged** and their
//! payload is `{K, [{K, I, I + K} || I <- 1..64]}` — 64 4-word tuples behind 64 cons cells,
//! ~390 words, comfortably **above the default 233-word nursery**, rebuilt every round and
//! dying at the next one. A round therefore asks a heap-admission door for more than a whole
//! nursery, so the collector really runs — which is exactly what makes these rows a *GC*
//! measurement rather than another copy measurement.
//!
//! ## The fixtures are read-only inputs, copied into a ScratchDir at run time
//!
//! The two fixture sources live with the `beam_rt` test harness
//! (`erts_rust/crates/beam_rt/tests/fixtures/{gc_class_copy_bound,gc_class_compile}.erl`) and
//! are **never edited here**. At run time they are copied into a [`ScratchDir`] and compiled
//! with the system `erlc` together with a small **bench-owned driver** ([`driver_source`])
//! that calls the fixtures' exported functions, validates what comes back and prints the
//! deterministic stdout. The fixtures carry no paths: the compile case's inputs arrive
//! through the **child environment** ([`ENV_SRC`]/[`ENV_OUT`], read with `os:getenv/1`), just
//! as the sibling harness drives them.
//!
//! ## The compile-attribution guard (anti-`-pa`)
//!
//! A wrong `-pa` (or an inherited `ERL_FLAGS=-pa <system ebin>`) makes an in-process compile
//! be served by the **system OTP compiler instead of the tree's compiler**, which moves every
//! figure by roughly a factor of two. The compile case's driver therefore prints
//! `code:which(compile)` **in the same process that runs the compile**, as its own stdout
//! line, and the row records it as `code:which(compile)=<path>`; when the `rust` side's answer
//! resolves into the **system OTP's** `lib/compiler-*/ebin` (i.e. neither this tree's
//! `bootstrap/lib/compiler/ebin` nor somewhere else in this checkout) the row also carries a
//! loud `WARNING:` note, so the figure can never be quoted without its attribution. See
//! [`compiler_origin`] / [`which_note`]; the two existing compile rows (`erlc_compiler`,
//! `elixir_compiler`) record the same guard through [`erl_compiler_which_note`].
//!
//! # The measurement (paired, drift-cancelling, per op)
//!
//! Each case declares a **per-runtime loop count** `N` (calibrated by the rule in the case
//! table's doc comment) and is sampled through the shared interleaved measurement
//! ([`crate::marginal`]): in **every round** the program runs at `N` rounds of the class and
//! then, *immediately after*, at `0` rounds — back to back — and the per-op figure is the
//! **median of the paired differences**:
//!
//! ```text
//! case_<name>_ns_per_op = median(wall@N - wall@0) / N * 1e6
//! ```
//!
//! The pairing is not cosmetic. An earlier schedule in this family took *all* `N`-op samples
//! and then *all* `0`-op samples and used `mean(work) - mean(startup)`; on this shared host
//! the boot cost drifts by tens of milliseconds *between* the passes, so the difference
//! absorbed the drift instead of cancelling it (the failure mode is worth a ≈56× inflation of
//! a published gap on the sibling `copy_bound` workload). **The sequential all-N-then-all-0
//! schedule must never come back**; the rules live in [`crate::marginal`].
//!
//! A figure whose median paired difference does not clear
//! `NOISE_MULTIPLE × max(MIN_NOISE_MS, 1.4826 × MAD)` is **withheld** (an honest
//! `marginal below timer resolution` note, no metric), and every published figure carries its
//! auditable `paired median=… min=… max=… noise=±… over k round(s)` note plus the
//! deterministic, contention-immune companion `case_<name>_ops = N`.
//!
//! # Honest reporting
//!
//! * Every runtime's stdout must carry the deterministic `N * PER_OP` line; a mismatch — or,
//!   for the compile case, a missing `which=` line — is recorded as a note on that run.
//! * A Rust exit `3` (`beam_rt: unsupported: …`) records the capability gap and emits **no**
//!   timing: the row is `skipped(unsupported-case-set)`, never a fake number.
//! * `erlc` unavailable ⇒ `skipped(erlc-absent)`; a fixture source absent from this checkout
//!   ⇒ `skipped(fixtures-missing)`; the compile source absent ⇒
//!   `skipped(compile-source-missing)`.
//! * A non-compiling fixture or driver is a **failure** (`module-compile-failed`), never a
//!   skip: the driver is ours, and the fixtures are committed.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::marginal::{self, Marginal, Pass, PassError};
use crate::metrics::{Category, Metric, Probe, Runtimes, Status};
use crate::proc::{self, Completed};
use crate::report::{Profile, WorkloadResult};
use crate::runtime::RuntimeKind;
use crate::scratch::ScratchDir;
use crate::workload::{RunContext, Workload};
use crate::workloads::{erl_top, slice};

/// The bench-owned driver module compiled next to the fixtures.
const DRIVER_MODULE: &str = "bench_gc_class_driver";

/// The fixture module holding the two copy-bound shapes.
const COPY_BOUND_MODULE: &str = "gc_class_copy_bound";

/// The fixture module holding the in-process compile.
const COMPILE_MODULE: &str = "gc_class_compile";

/// Every exported driver function's arity (the loop count).
const ARITY: u32 = 1;

/// How much one validated round adds to a case's accumulator, so a run at `N` rounds prints
/// `N * PER_OP`. The driver source is generated from it.
const PER_OP: i64 = 1;

/// The `send_recv` driver function.
const SEND_RECV: &str = "send_recv";
/// The `ets_copy` driver function.
const ETS_COPY: &str = "ets_copy";
/// The `compile` driver function.
const COMPILE: &str = "compile";

/// The directory (relative to `$ERL_TOP`) holding the committed fixture sources.
const FIXTURE_SUBDIR: &str = "erts_rust/crates/beam_rt/tests/fixtures";

/// The copy-bound fixture's file name.
const COPY_BOUND_FIXTURE: &str = "gc_class_copy_bound.erl";

/// The compile fixture's file name.
const COMPILE_FIXTURE: &str = "gc_class_compile.erl";

/// The real OTP source the compile class compiles, relative to `$ERL_TOP`.
///
/// `lib/stdlib/src/ordsets.erl` is the module the sibling A/B harness already compiles in
/// process (`gc_strategy_classes.rs`'s C4 class), it includes nothing (so the compile depends
/// on no include path), and it is a mid-sized, stable stdlib module rather than a toy.
const COMPILE_SOURCE_REL: &str = "lib/stdlib/src/ordsets.erl";

/// The payload element count the copy-bound fixture builds (`?PAYLOAD_LEN`), read back by the
/// driver so a silently truncated payload fails.
const PAYLOAD_LENGTH: usize = 64;

/// `?RECV_TIMEOUT_MS` is the fixture's own; the driver validates the *returned* payload only,
/// so a lost message is the fixture's loud `exit({no_reply, N})`, never a hang.

/// The environment variable carrying the real OTP source the compile class compiles.
const ENV_SRC: &str = "BEAM_RT_GCH_SRC";

/// The environment variable carrying the compile class's `{outdir, …}` root.
const ENV_OUT: &str = "BEAM_RT_GCH_OUT";

/// The prefix of the `code:which(compile)` line the compile case's driver prints.
const WHICH_PREFIX: &str = "which=";

/// The label the recorded attribution note uses.
const WHICH_LABEL: &str = "code:which(compile)=";

/// The `-eval` expression the two existing compile rows use to attribute their compiler.
///
/// `code:which/1` answers a charlist for a findable module and an atom (`preloaded`,
/// `non_existing`) otherwise; the expression normalises both to a string so `~ts` never
/// raises, and it runs in the node that would run the compiler, inheriting this process's
/// environment exactly as `erlc` propagates `ERL_FLAGS`/`ERL_COMPILER_OPTIONS` to it.
const WHICH_EXPR: &str = "io:format(\"~ts~n\",[case code:which(compile) of \
                          P when is_list(P) -> P; A -> atom_to_list(A) end]), halt(0).";

/// One GC-class case: the driver function that drives one shape of the fixture.
struct Case {
    /// Stable, short case name (used in the metric names, e.g. `case_<name>_wall_ms`).
    name: &'static str,
    /// The exported driver function (arity 1: the loop count).
    function: &'static str,
    /// Whether this is the in-process compile case: it needs the fixture's `BEAM_RT_GCH_SRC`/
    /// `BEAM_RT_GCH_OUT` environment and prints `code:which(compile)` in the compiling process.
    compiles: bool,
    /// Loop count calibrated for the Rust runtime (≥0.5 s of loop work, re-derived whenever
    /// the measured per-op cost changes materially).
    n_rust: i64,
    /// Loop count calibrated for the system-runtime oracle.
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

    /// Whether this case's run must print the `code:which(compile)` attribution line.
    fn prints_which(&self) -> bool {
        self.compiles
    }
}

/// The fixed case table.
///
/// The `n_rust`/`n_sys` values are **constants, not measurements**, calibrated on the
/// reference host under one rule: **a runtime's timed loop must do enough work that its paired
/// marginal stands clear of that runtime's own run-to-run boot/startup jitter** (measured at
/// ≈±0.1 s on the rust boot). The rust floor is therefore **≥0.5 s of loop work**, computed at
/// the per-op cost measured on that host (2026-10-23, `/proc/loadavg` ≈5, `--quick`, default
/// baseline-JIT tier): see the pinning test's cost table for the exact figures behind each
/// count (≈25 µs/round trip for `send_recv`, ≈13 µs for `ets_copy`, ≈0.35 s per in-process
/// compile).
///
/// **Maintenance rule:** when a case's measured per-op cost changes materially, re-derive
/// `n_rust` so the loop work stays ≥0.5 s; the pinning test
/// (`the_rust_loop_counts_clear_the_boot_jitter_band`) enforces the floor against those cost
/// constants. The metric is normalized per op, so the counts only have to keep both marginals
/// measurable — never compare raw `wall_ms` across sides with different `N`.
static CASES: &[Case] = &[
    Case {
        name: SEND_RECV,
        function: SEND_RECV,
        compiles: false,
        n_rust: 25_000,
        n_sys: 200_000,
    },
    Case {
        name: ETS_COPY,
        function: ETS_COPY,
        compiles: false,
        n_rust: 40_000,
        n_sys: 400_000,
    },
    Case {
        name: COMPILE,
        function: COMPILE,
        compiles: true,
        n_rust: 2,
        n_sys: 8,
    },
];

/// The `gc_class` workload.
pub struct GcClass;

/// The number of timed samples per case for a profile.
fn runs_for(profile: Profile) -> usize {
    match profile {
        Profile::Quick => 3,
        Profile::Full => 10,
    }
}

/// The fixture sources' directory in this checkout.
fn fixture_dir() -> PathBuf {
    erl_top().join(FIXTURE_SUBDIR)
}

/// The real `lib/*/src/*.erl` source the compile case compiles.
fn compile_source() -> PathBuf {
    erl_top().join(COMPILE_SOURCE_REL)
}

/// The bench-owned driver module, generated from this file's constants.
///
/// It only *drives* the fixtures and validates what they return: the payload of the last round
/// is checked element by element (a real defensive check that raises on a corrupted copy), and
/// the compile case prints `code:which(compile)` **in the calling process** before compiling.
/// A zero loop count is answered without touching the fixture, so a startup (`0`-round) sample
/// stays a pure startup sample (the fixtures require `N > 0`).
fn driver_source() -> String {
    format!(
        "\
-module({DRIVER_MODULE}).
-export([{SEND_RECV}/{ARITY}, {ETS_COPY}/{ARITY}, {COMPILE}/{ARITY}]).

%% Two-process ping-pong over the process copy boundary (the fixture spawns the peer and
%% joins it through a monitor). The returned payload is validated, then N * {PER_OP} is
%% returned so the run prints the deterministic constant.
{SEND_RECV}(0) -> 0;
{SEND_RECV}(N) when is_integer(N) ->
    Payload = {COPY_BOUND_MODULE}:{SEND_RECV}(N),
    ok = validate(Payload),
    N * {PER_OP}.

%% The ETS copy boundary: one table, one key, a fresh payload in and out per round.
{ETS_COPY}(0) -> 0;
{ETS_COPY}(N) when is_integer(N) ->
    Payload = {COPY_BOUND_MODULE}:{ETS_COPY}(N),
    ok = validate(Payload),
    N * {PER_OP}.

%% A real in-process compile: the paths come from the CHILD ENVIRONMENT (the fixture reads
%% them with os:getenv/1), and code:which(compile) is printed HERE, in the same process that
%% runs the compile, so a wrong -pa can never be attributed silently.
{COMPILE}(0) -> 0;
{COMPILE}(N) when is_integer(N) ->
    io:format(\"which=~ts~n\", [which_text(code:which(compile))]),
    case {COMPILE_MODULE}:run(N) of
        {{N, Module}} when is_atom(Module) -> N * {PER_OP};
        Other -> exit({{compile_shape, Other}})
    end.

which_text(Path) when is_list(Path) -> Path;
which_text(Path) -> atom_to_list(Path).

%% The copy fixtures return the payload of their LAST round, which counts down to 1: the key
%% is 1 and the list holds ?PAYLOAD_LEN elements.
validate({{1, Elements}}) when is_list(Elements) ->
    case length(Elements) of
        {PAYLOAD_LENGTH} -> ok;
        Other -> exit({{payload_length, Other}})
    end;
validate(Other) ->
    exit({{payload_shape, Other}}).
"
    )
}

/// The number of rounds a case's `n`-round run performs (the class's own unit of work).
fn ops(n: i64) -> i64 {
    n
}

/// The deterministic last stdout line a run at `n` rounds must print.
fn expected_stdout(n: i64) -> String {
    (n * PER_OP).to_string()
}

/// Splits a run's stdout into its trailing ops line and (when the case prints one) the
/// `which=` attribution line.
///
/// The attribution line is *required* on the compile case: a run that compiled something
/// without saying which compiler it used cannot be quoted, so a missing line is a note on that
/// run — never a silently accepted figure.
fn parse_stdout(case: &Case, stdout: &str) -> Result<(String, Option<String>), String> {
    let lines: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let Some((last, head)) = lines.split_last() else {
        return Err("printed nothing".to_string());
    };
    let which = head
        .iter()
        .find_map(|line| line.strip_prefix(WHICH_PREFIX))
        .map(str::to_string);
    if case.prints_which() {
        if which.is_none() {
            return Err(format!(
                "printed no `{WHICH_PREFIX}<path>` line (its compile attribution): {head:?}"
            ));
        }
    } else if !head.is_empty() {
        return Err(format!("printed unexpected extra lines: {head:?}"));
    }
    Ok(((*last).to_string(), which))
}

/// The per-side argv for one case at `n` rounds (everything after the program path).
fn argv_for(kind: RuntimeKind, dir: &Path, case: &Case, n: i64) -> Vec<String> {
    let beam = dir.join(format!("{DRIVER_MODULE}.beam")).display().to_string();
    let dir = dir.display().to_string();
    match kind {
        RuntimeKind::Rust => vec![
            "-pa".to_string(),
            dir,
            "run".to_string(),
            beam,
            format!("{DRIVER_MODULE}:{}/1", case.function),
            n.to_string(),
        ],
        // The `run` action spells the call with its arity; an `-eval` expression must not.
        RuntimeKind::Sys => vec![
            "-noshell".to_string(),
            "-pa".to_string(),
            dir,
            "-eval".to_string(),
            format!(
                "io:format(\"~w~n\",[{DRIVER_MODULE}:{}({n})]), halt(0).",
                case.function
            ),
        ],
    }
}

/// The prepared command for one pass: the program, the per-side argv, and — for the compile
/// case — the fixture's `BEAM_RT_GCH_SRC`/`BEAM_RT_GCH_OUT` pair in the **child environment**
/// (the only route the fixture has to its inputs: it reads them with `os:getenv/1`).
fn command_for(
    program: &Path,
    kind: RuntimeKind,
    dir: &Path,
    case: &Case,
    n: i64,
    compile_env: &CompileEnv,
) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(argv_for(kind, dir, case, n));
    if case.compiles {
        cmd.env(ENV_SRC, &compile_env.source);
        cmd.env(ENV_OUT, &compile_env.out_root);
    }
    cmd
}

/// The compile case's inputs: the real OTP source, and the `{outdir, …}` root whose
/// `out1..outN` the harness pre-creates (`compile:file/2` does not create its `{outdir, Dir}`).
struct CompileEnv {
    source: PathBuf,
    out_root: PathBuf,
}

/// Creates `<root>/out1..out<n>` so every compile of a pass has its own existing output
/// directory. Returns `Err(reason)` — never a silent partial setup.
fn prepare_out_dirs(root: &Path, n: i64) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|e| format!("create {}: {e}", root.display()))?;
    for index in 1..=n {
        let dir = root.join(format!("out{index}"));
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    Ok(())
}

/// Why staging failed.
///
/// A **missing fixture** is a missing prerequisite (`skipped(fixtures-missing)`): the two
/// `.erl` sources belong to the `beam_rt` test harness and may legitimately not be in a
/// stripped checkout. A source we *did* stage that does not compile is a **real failure**
/// (`module-compile-failed`) — the driver is ours, so it cannot legitimately be broken.
#[derive(Debug, PartialEq, Eq)]
enum StageError {
    /// A committed fixture source is not present in this checkout.
    FixturesMissing(String),
    /// The staged sources (or the bench-owned driver) did not compile.
    Compile(String),
}

/// Copies the two committed fixture sources into `dir` and compiles them and the bench-owned
/// driver there with `erlc`, returning the driver's `.beam`.
fn stage(dir: &Path, erlc: &Path) -> Result<PathBuf, StageError> {
    let fixtures = fixture_dir();
    for fixture in [COPY_BOUND_FIXTURE, COMPILE_FIXTURE] {
        let from = fixtures.join(fixture);
        let to = dir.join(fixture);
        std::fs::copy(&from, &to).map_err(|e| {
            StageError::FixturesMissing(format!("copy {}: {e}", from.display()))
        })?;
    }

    let driver = dir.join(format!("{DRIVER_MODULE}.erl"));
    std::fs::write(&driver, driver_source())
        .map_err(|e| StageError::Compile(format!("write {}: {e}", driver.display())))?;

    let mut sources: Vec<PathBuf> = vec![driver];
    for fixture in [COPY_BOUND_FIXTURE, COMPILE_FIXTURE] {
        sources.push(dir.join(fixture));
    }
    let mut cmd = Command::new(erlc);
    cmd.arg("-o").arg(dir).args(&sources);
    match proc::run(&mut cmd) {
        Ok(out) if out.success => {}
        Ok(out) => {
            return Err(StageError::Compile(format!(
                "erlc exited {:?}: {}",
                out.exit_code,
                out.stderr.trim()
            )))
        }
        Err(e) => return Err(StageError::Compile(format!("erlc spawn failed: {e}"))),
    }

    let beam = dir.join(format!("{DRIVER_MODULE}.beam"));
    if beam.is_file() {
        Ok(beam)
    } else {
        Err(StageError::Compile(format!(
            "{} missing after erlc exited 0",
            beam.display()
        )))
    }
}

/// One pass sample: the wall time (ms), the peak RSS (bytes; only `Work` passes feed
/// [`CaseOutcome::rss`]), the trimmed stdout, the `code:which(compile)` line (compile case),
/// and any non-fatal notes.
struct Sample {
    wall_ms: f64,
    peak_memory_bytes: Option<u64>,
    stdout: String,
    which: Option<String>,
    notes: Vec<String>,
}

/// The samples collected for one case under one runtime.
struct CaseOutcome {
    n: i64,
    pairs: Vec<(f64, f64)>,
    rss: Vec<f64>,
    observed: Option<String>,
    /// The `code:which(compile)` the compile case's driver reported (first `Work` pass).
    which: Option<String>,
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

/// Drives `runs` interleaved rounds through [`marginal::sample_pairs`] and folds the samples
/// into a [`CaseOutcome`]: the per-round wall pairs, the `Work`-pass peak-RSS samples, the
/// first `Work`-pass stdout and `which=` line, the per-sample notes and the resolved marginal.
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
        which: sampled.pairs.iter().find_map(|(work, _)| work.which.clone()),
        unsupported: aborted.is_some_and(|abort| abort.unsupported),
        notes,
    }
}

/// Runs one case under one runtime: `runs` interleaved rounds of an `n`-round sample and a
/// `0`-round sample, per [`marginal`]'s sanctioned schedule.
fn execute_case(
    program: &Path,
    kind: RuntimeKind,
    dir: &Path,
    case: &Case,
    compile_env: &CompileEnv,
    runs: usize,
) -> CaseOutcome {
    let n = case.n_for(kind);
    // A Rust exit `3` marks the whole case `unsupported` only while nothing is measured yet.
    let mut work_sample_seen = false;
    measure_case(n, runs, |pass| {
        let count = n * pass.units();
        let mut cmd = command_for(program, kind, dir, case, count, compile_env);
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
        let got = completed.stdout_trim().to_string();
        let mut notes = Vec::new();
        let (ops_line, which) = match parse_stdout(case, &got) {
            Ok(parsed) => parsed,
            Err(reason) => {
                notes.push(format!(
                    "case {}: the {pass_name} run {reason}",
                    case.name
                ));
                (got.clone(), None)
            }
        };
        let expected = expected_stdout(count);
        if ops_line != expected {
            notes.push(format!(
                "case {}: the {pass_name} run printed `{ops_line}` (expected `{expected}`)",
                case.name
            ));
        }
        work_sample_seen |= pass == Pass::Work;
        Ok(Sample {
            wall_ms: completed.wall_ms,
            peak_memory_bytes: completed.peak_memory_bytes,
            stdout: got,
            which,
            notes,
        })
    })
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
    result.metrics.push(Metric::scalar(
        format!("case_{}_ops", case.name),
        "ops",
        ops(outcome.n) as f64,
    ));
}

// -------------------------------------------------------------------------------------------
// The compile-attribution guard (anti-`-pa`), shared by all three compile rows
// -------------------------------------------------------------------------------------------

/// Where the `compile` module a run actually used came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompilerOrigin {
    /// This tree's own bootstrap compiler (`<ERL_TOP>/bootstrap/lib/compiler/ebin`).
    TreeBootstrap,
    /// Somewhere else inside this checkout (the runtime's own release).
    RuntimeRelease,
    /// The **system OTP** installation's compiler (`…/lib/compiler-*/ebin`), i.e. not this
    /// tree's compiler — the case the guard exists for.
    SystemOtp,
    /// Not recognisable (`preloaded`, `non_existing`, an empty string, an unknown layout).
    Unrecognised,
}

impl CompilerOrigin {
    /// The short label recorded next to the path.
    pub(crate) fn label(self) -> &'static str {
        match self {
            CompilerOrigin::TreeBootstrap => "this tree's bootstrap/lib/compiler/ebin",
            CompilerOrigin::RuntimeRelease => "this checkout",
            CompilerOrigin::SystemOtp => "the system OTP installation",
            CompilerOrigin::Unrecognised => "unrecognised",
        }
    }
}

/// Whether `path` names a `compile.beam` inside a `compiler`/`compiler-<vsn>` ebin directory.
fn is_compiler_ebin(path: &Path) -> bool {
    let named_compile = path.file_name().and_then(|name| name.to_str()) == Some("compile.beam");
    named_compile
        && path.components().any(|component| {
            let name = component.as_os_str().to_string_lossy();
            name == "compiler" || name.starts_with("compiler-")
        })
}

/// Classifies a `code:which(compile)` answer against this checkout (`$ERL_TOP`).
///
/// Both sides are canonicalised when the filesystem can do it, so a symlinked `$TMPDIR`, a
/// worktree path or a `/nix/store` indirection cannot make an in-tree compiler look foreign
/// (or the reverse). A path that cannot be canonicalised is compared as written.
pub(crate) fn compiler_origin(which: &str) -> CompilerOrigin {
    let trimmed = which.trim();
    if trimmed.is_empty() || trimmed == "preloaded" || trimmed == "non_existing" {
        return CompilerOrigin::Unrecognised;
    }
    let root = erl_top();
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let path = PathBuf::from(trimmed);
    let path = std::fs::canonicalize(&path).unwrap_or(path);

    if path.starts_with(root.join("bootstrap/lib/compiler")) {
        return CompilerOrigin::TreeBootstrap;
    }
    if path.starts_with(&root) {
        return CompilerOrigin::RuntimeRelease;
    }
    if is_compiler_ebin(&path) {
        return CompilerOrigin::SystemOtp;
    }
    CompilerOrigin::Unrecognised
}

/// The note recorded for a run's `code:which(compile)` answer.
///
/// The path is always recorded, whatever it is — the guard never fabricates and never omits
/// it. On the **rust** side a path that resolves into the system OTP's compiler is additionally
/// a loud `WARNING:` line: that figure was produced by the *system* compiler, not this tree's,
/// and must never be quoted as an in-tree-compiler measurement.
pub(crate) fn which_note(kind: RuntimeKind, which: &str) -> String {
    let origin = compiler_origin(which);
    let base = format!("{WHICH_LABEL}{} ({})", which.trim(), origin.label());
    if kind == RuntimeKind::Rust && origin == CompilerOrigin::SystemOtp {
        format!(
            "WARNING: {base} — this compile figure is served by the SYSTEM OTP compiler, not \
             by this tree's bootstrap/lib/compiler/ebin: never quote it as an in-tree-compiler \
             measurement"
        )
    } else {
        base
    }
}

/// The `code:which(compile)` the system `erl` resolves, or `Err(reason)` — a real probe.
fn erl_compiler_which() -> Result<String, String> {
    let Some(erl) = proc::which("erl") else {
        return Err("erl-absent".to_string());
    };
    let mut cmd = Command::new(erl);
    cmd.args(["-noshell", "-eval", WHICH_EXPR]);
    match proc::run(&mut cmd) {
        Ok(out) if out.success => {
            let text = out.stdout_trim().trim().to_string();
            if text.is_empty() {
                Err(format!(
                    "erl exited {:?} with no code:which/1 output",
                    out.exit_code
                ))
            } else {
                Ok(text)
            }
        }
        Ok(out) => Err(format!(
            "erl exited {:?}: {}",
            out.exit_code,
            out.stderr.trim()
        )),
        Err(e) => Err(format!("erl spawn failed: {e}")),
    }
}

/// The attribution note the two existing compile rows (`erlc_compiler`, `elixir_compiler`)
/// record: `code:which(compile)=<path>` when the probe answers, else an explicit
/// `code:which(compile)=unknown (<reason>)` — **never omitted silently**.
///
/// The probe inherits this process's environment (`Command` does, and nothing is overridden),
/// which is exactly how `erlc` propagates `ERL_FLAGS`/`ERL_COMPILER_OPTIONS` into the node
/// that runs the compiler: a stray `ERL_FLAGS=-pa <system ebin>` is the failure this guard
/// attributes.
pub(crate) fn erl_compiler_which_note() -> String {
    match erl_compiler_which() {
        Ok(which) => format!(
            "{WHICH_LABEL}{which} ({})",
            compiler_origin(&which).label()
        ),
        Err(reason) => format!("{WHICH_LABEL}unknown ({reason})"),
    }
}

impl Workload for GcClass {
    fn id(&self) -> &'static str {
        "gc_class"
    }

    fn description(&self) -> &'static str {
        "GC-bound workload classes (process copy, ETS copy, in-process compile) under each \
         runtime"
    }

    fn category(&self) -> Category {
        Category::Gc
    }

    fn contracts(&self) -> &'static [&'static str] {
        // Real Erlang semantics (A.1) running real `.beam` images (A.2), over the ETF copy
        // boundary (A.5), with the compile class exercising in-process compilation (A.9).
        &["A.1", "A.2", "A.5", "A.9"]
    }

    fn runtimes(&self) -> Runtimes {
        // The whole point: both runtimes execute the same compiled classes.
        Runtimes::Both
    }

    fn probe(&self) -> Probe {
        if slice::resolve_erlc().is_none() {
            return Probe::skipped("erlc-absent");
        }
        let fixtures = fixture_dir();
        for fixture in [COPY_BOUND_FIXTURE, COMPILE_FIXTURE] {
            if !fixtures.join(fixture).is_file() {
                return Probe::skipped("fixtures-missing");
            }
        }
        if !compile_source().is_file() {
            return Probe::skipped("compile-source-missing");
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

        // The fixtures are read-only inputs from the sibling node; a stripped checkout that
        // lacks them cannot run this workload at all (a prerequisite, not a failure).
        let staged = match stage(dir, &erlc) {
            Ok(beam) => beam,
            Err(StageError::FixturesMissing(reason)) => {
                let mut result = WorkloadResult::skipped(
                    id,
                    kind,
                    category,
                    description,
                    "fixtures-missing",
                );
                result.notes.push(reason);
                return result;
            }
            Err(StageError::Compile(reason)) => {
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

        // The compile case's inputs: the real OTP source and the `{outdir, …}` root with
        // `out1..out<max N>` pre-created (`compile:file/2` does not create its outdir).
        let source = compile_source();
        if !source.is_file() {
            return WorkloadResult::skipped(
                id,
                kind,
                category,
                description,
                "compile-source-missing",
            );
        }
        let out_root = dir.join("compile_out");
        // Only the **compile** case consumes `out1..out<n>` — and only up to its own loop count.
        // (`max` over the whole table would ask for one directory per copy-bound round: 400 000
        // of them.) One set large enough for either runtime's compile count is enough, because
        // the fixture indexes the directories from 1 for every pass.
        let out_dirs_needed = CASES
            .iter()
            .filter(|case| case.compiles)
            .map(|case| case.n_sys.max(case.n_rust))
            .max()
            .unwrap_or(0);
        if let Err(reason) = prepare_out_dirs(&out_root, out_dirs_needed) {
            return WorkloadResult::failed(id, kind, category, description, reason);
        }
        let compile_env = CompileEnv {
            source: source.clone(),
            out_root: out_root.clone(),
        };

        let runs = runs_for(ctx.profile);
        let mut result = WorkloadResult::new(id, kind, category, description);
        result.notes.push(format!(
            "staged {} (bench driver + the committed {COPY_BOUND_FIXTURE}/{COMPILE_FIXTURE} \
             copied read-only into the scratch dir; erlc={})",
            staged.display(),
            erlc.display()
        ));
        result.notes.push(format!(
            "compile case: source={} (a real lib/*/src/*.erl), outdir root={}, \
             BEAM_RT_GCH_SRC/BEAM_RT_GCH_OUT set on the child",
            source.display(),
            out_root.display()
        ));

        let mut overall_peak: Option<u64> = None;
        let mut observed_all: Vec<String> = Vec::new();
        let mut ok_cases = 0usize;
        let mut unsupported_cases = 0usize;

        for case in CASES {
            let outcome = execute_case(program, kind, dir, case, &compile_env, runs);
            if outcome.ok() {
                ok_cases += 1;
                push_case_metrics(&mut result, case, &outcome);
                for &bytes in &outcome.rss {
                    overall_peak =
                        Some(overall_peak.map_or(bytes as u64, |peak| peak.max(bytes as u64)));
                }
                if let Some(value) = &outcome.observed {
                    observed_all.push(format!("{}={}", case.name, value.lines().last().unwrap_or("")));
                }
                if case.prints_which() {
                    match &outcome.which {
                        // The guard: record what the runtime actually reported, and warn when
                        // the rust side used the system OTP compiler.
                        Some(which) => result.notes.push(which_note(kind, which)),
                        None => result.notes.push(format!(
                            "{WHICH_LABEL}unknown (the {} run printed no `{WHICH_PREFIX}` line)",
                            kind.as_str()
                        )),
                    }
                }
                result.notes.push(format!(
                    "case {}: N={} ({}) rounds of the class {}",
                    case.name,
                    outcome.n,
                    kind.as_str(),
                    if case.compiles {
                        "an in-process compile of the real source"
                    } else {
                        "a copy-bound round trip of the ~390-word payload (above the 233-word \
                         nursery, so a collection is admitted)"
                    }
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
            "method: ns/op = median(wall at N rounds - wall at 0 rounds) / N * 1e6, from \
             interleaved paired rounds (each round runs the N-round sample and then the 0-round \
             sample back to back, so a drifting boot cost cancels inside the pair); N is per \
             runtime so both marginals are measurable, and the fixed process startup (incl. the \
             Rust runtime's code-path resolution) cancels out of every pair; the figure is \
             withheld when the median does not clear the noise band"
                .to_string(),
        );
        result.notes.push(
            "note: the classes are the sibling harness's own fixtures, run unchanged — the \
             payload is deliberately above the default nursery so a collection is admitted \
             (unlike copy_bound's 70-word wrapper); the op count is deterministic and \
             contention-immune while the wall-clock rows move with host load"
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
                (Status::Failed, "no-gc-class-case-ran")
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
    use std::ffi::OsStr;

    #[test]
    fn metadata_is_stable() {
        let w = GcClass;
        assert_eq!(w.id(), "gc_class");
        assert_eq!(w.category(), Category::Gc);
        assert_eq!(w.category().as_str(), "gc");
        assert_eq!(w.contracts(), &["A.1", "A.2", "A.5", "A.9"]);
        assert!(matches!(w.runtimes(), Runtimes::Both));
        assert!(w.runtimes().allows(RuntimeKind::Rust));
        assert!(w.runtimes().allows(RuntimeKind::Sys));
        assert!(!w.description().is_empty());
    }

    #[test]
    fn every_case_declares_a_calibrated_n() {
        assert_eq!(CASES.len(), 3, "send_recv + ets_copy + compile");
        let mut names: Vec<&str> = CASES.iter().map(|c| c.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), CASES.len(), "case names must be distinct");
        for case in CASES {
            assert!(!case.name.is_empty(), "a case has no name");
            assert!(
                driver_source().contains(&format!("{}(N) when is_integer(N) ->", case.function)),
                "{}",
                case.name
            );
            assert!(
                case.n_for(RuntimeKind::Rust) > 0 && case.n_for(RuntimeKind::Sys) > 0,
                "{}",
                case.name
            );
            assert!(
                case.n_sys > case.n_rust,
                "{}: the oracle loop must be the larger one",
                case.name
            );
        }
        // Only the compile case prints the attribution line and needs the fixture env.
        let compiling: Vec<&str> = CASES
            .iter()
            .filter(|c| c.prints_which())
            .map(|c| c.name)
            .collect();
        assert_eq!(compiling, vec![COMPILE]);
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
            (SEND_RECV, 50_721.0, 25_000),
            (ETS_COPY, 13_808.0, 40_000),
            (COMPILE, 324_895_719.0, 2),
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
        assert_eq!(rust[3], format!("/tmp/xyz/{DRIVER_MODULE}.beam"));
        assert_eq!(rust[4], format!("{DRIVER_MODULE}:{}/1", case.function));
        assert_eq!(rust[5], "7");

        let sys = argv_for(RuntimeKind::Sys, dir, case, 9);
        assert_eq!(sys[0], "-noshell");
        assert!(sys.contains(&"-pa".to_string()));
        assert!(sys.contains(&"/tmp/xyz".to_string()));
        assert!(sys
            .iter()
            .any(|a| a.contains(&format!("{DRIVER_MODULE}:{}(9)", case.function))));
    }

    /// The compile fixture's inputs reach the child through its **environment** (the fixture
    /// reads `os:getenv/1`), and only the compile case's command carries them.
    #[test]
    fn the_compile_case_receives_its_inputs_through_the_child_environment() {
        let dir = Path::new("/tmp/gc-class-env");
        let env = CompileEnv {
            source: PathBuf::from("/repo/lib/stdlib/src/ordsets.erl"),
            out_root: PathBuf::from("/tmp/gc-class-env/compile_out"),
        };
        let compile = CASES.iter().find(|c| c.compiles).expect("the compile case");
        for kind in [RuntimeKind::Rust, RuntimeKind::Sys] {
            let cmd = command_for(Path::new("/bin/true"), kind, dir, compile, 3, &env);
            let envs: Vec<(&OsStr, Option<&OsStr>)> = cmd.get_envs().collect();
            assert!(
                envs.iter()
                    .any(|(name, value)| *name == OsStr::new(ENV_SRC)
                        && *value == Some(env.source.as_os_str())),
                "{kind:?}: {ENV_SRC} must be set to the real source, got {envs:?}"
            );
            assert!(
                envs.iter()
                    .any(|(name, value)| *name == OsStr::new(ENV_OUT)
                        && *value == Some(env.out_root.as_os_str())),
                "{kind:?}: {ENV_OUT} must be set to the outdir root, got {envs:?}"
            );
            // ... and the non-compile cases stay clean.
            let other = command_for(Path::new("/bin/true"), kind, dir, &CASES[0], 3, &env);
            assert_eq!(other.get_envs().count(), 0, "{kind:?}");
        }
    }

    #[test]
    fn the_outdirs_are_pre_created_for_every_loop_count() {
        let base = std::env::temp_dir().join(format!("bench-gc-class-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("compile_out");
        prepare_out_dirs(&root, 3).expect("the outdirs must be created");
        for index in 1..=3 {
            assert!(root.join(format!("out{index}")).is_dir(), "out{index}");
        }
        assert!(!root.join("out4").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The fixture sources are only *copied*; a checkout without them is a missing
    /// prerequisite, while a source that fails to compile is a real failure.
    #[test]
    fn stage_distinguishes_missing_fixtures_from_a_compile_failure() {
        let dir = std::env::temp_dir().join(format!("bench-gc-class-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A directory that is not writable cannot receive the fixtures: reported, never silent.
        let err = stage(
            &dir.join("nope/deeper"),
            Path::new("/nonexistent/erlc"),
        )
        .expect_err("a missing destination must be reported");
        assert!(
            matches!(err, StageError::FixturesMissing(_)),
            "expected a missing-fixture error, got {err:?}"
        );

        // With the fixtures copied, a bogus `erlc` is a compile failure.
        if fixture_dir().join(COPY_BOUND_FIXTURE).is_file() {
            let err = stage(&dir, Path::new("/nonexistent/erlc"))
                .expect_err("a broken erlc must be reported");
            assert!(matches!(err, StageError::Compile(_)), "{err:?}");
            assert!(dir.join(COPY_BOUND_FIXTURE).is_file(), "the fixture was copied");
            assert!(
                dir.join(format!("{DRIVER_MODULE}.erl")).is_file(),
                "the driver source was written"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_stdout_parser_requires_the_attribution_line_on_the_compile_case() {
        let compile = CASES.iter().find(|c| c.compiles).expect("compile case");
        let before = expected_stdout(3);
        let (ops, which) =
            parse_stdout(compile, &format!("which=/x/lib/compiler-8.6/ebin/compile.beam\n{before}\n"))
                .expect("a well-formed compile stdout parses");
        assert_eq!(ops, before);
        assert_eq!(which.as_deref(), Some("/x/lib/compiler-8.6/ebin/compile.beam"));

        // No attribution line ⇒ an explicit error (the figure cannot be quoted without it).
        let err = parse_stdout(compile, &before).expect_err("the attribution line is required");
        assert!(err.contains("compile attribution"), "{err}");

        // A non-compile case prints exactly one line.
        let (ops, which) = parse_stdout(&CASES[0], "7\n").expect("one line is enough");
        assert_eq!((ops.as_str(), which), ("7", None));
        assert!(parse_stdout(&CASES[0], "which=/x\n7\n").is_err(), "no extra lines");
        assert!(parse_stdout(&CASES[0], "").is_err(), "an empty stdout is an error");
    }

    /// The guard itself: an in-tree bootstrap compiler, another path in this checkout, the
    /// system OTP's compiler and an unreadable answer are four different origins, and only the
    /// system compiler on the **rust** side raises the loud warning.
    #[test]
    fn the_compile_attribution_guard_classifies_and_warns() {
        let root = erl_top();
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let bootstrap = root.join("bootstrap/lib/compiler/ebin/compile.beam");
        let in_tree = root.join("lib/stdlib/ebin/lists.beam");
        let system = "/nix/store/xyz-erlang-29.1.1/lib/erlang/lib/compiler-8.6/ebin/compile.beam";

        assert_eq!(
            compiler_origin(&bootstrap.display().to_string()),
            CompilerOrigin::TreeBootstrap
        );
        assert_eq!(
            compiler_origin(&in_tree.display().to_string()),
            CompilerOrigin::RuntimeRelease
        );
        assert_eq!(compiler_origin(system), CompilerOrigin::SystemOtp);
        for unrecognised in ["", "  ", "preloaded", "non_existing"] {
            assert_eq!(
                compiler_origin(unrecognised),
                CompilerOrigin::Unrecognised,
                "{unrecognised}"
            );
        }

        // Both runtimes record the path; only the rust side screams about the system compiler.
        for kind in [RuntimeKind::Rust, RuntimeKind::Sys] {
            let note = which_note(kind, system);
            assert!(note.contains(&format!("{WHICH_LABEL}{system}")), "{note}");
        }
        assert!(
            which_note(RuntimeKind::Rust, system).starts_with("WARNING:"),
            "the rust side must warn"
        );
        assert!(
            !which_note(RuntimeKind::Sys, system).starts_with("WARNING:"),
            "the system runtime measures its own compiler, so it must not warn"
        );
        let tree = bootstrap.display().to_string();
        assert!(!which_note(RuntimeKind::Rust, &tree).starts_with("WARNING:"));
        assert!(which_note(RuntimeKind::Rust, &tree).contains(&tree));
    }

    /// The shared probe's note is always produced: a path when `erl` answers, an explicit
    /// `unknown (<reason>)` otherwise — never an omitted line.
    #[test]
    fn the_shared_compiler_probe_always_records_a_note() {
        let note = erl_compiler_which_note();
        assert!(note.starts_with(WHICH_LABEL), "{note}");
        if proc::which("erl").is_some() {
            assert!(note.contains("compile.beam") || note.contains("unknown ("), "{note}");
        } else {
            assert!(note.contains("unknown (erl-absent)"), "{note}");
        }
    }

    /// A fake pass sample: `work_ms` on the `Work` pass, `startup_ms` on the `Startup` pass, the
    /// deterministic stdout of `units × n`, no RSS, no `which=` line, no notes.
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
            which: if n > 0 {
                Some("/x/lib/compiler-8.6/ebin/compile.beam".to_string())
            } else {
                Some("/x/lib/compiler-8.6/ebin/compile.beam".to_string())
            },
            notes: Vec::new(),
        }
    }

    /// `CASES[0]`'s metrics pushed into a fresh result, from a steady fake clock whose `Work`
    /// pass costs `delta_ms` (ms) more than its `Startup` pass, over `runs` rounds.
    fn measured(delta_ms: f64, runs: usize) -> WorkloadResult {
        let outcome = measure_case(1_000, runs, |pass| {
            Ok(fake_sample(pass, 100.0 + delta_ms, 100.0, 1_000))
        });
        let mut result = WorkloadResult::new("gc_class", RuntimeKind::Rust, Category::Gc, "test");
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

    /// The schedule `gc_class` uses: every round is a `Work` sample *immediately* followed by its
    /// `Startup` sample — never a whole pass at a time.
    #[test]
    fn rounds_are_paired_work_then_startup() {
        let mut order = Vec::new();
        let mut loop_counts = Vec::new();
        let outcome = measure_case(1_000, 3, |pass| {
            order.push(pass);
            loop_counts.push(1_000 * pass.units());
            Ok(fake_sample(pass, 110.0, 100.0, 1_000))
        });
        assert_eq!(
            order,
            [Pass::Work, Pass::Startup].repeat(3),
            "one round is Work then Startup"
        );
        assert_eq!(loop_counts, [1000, 0, 1000, 0, 1000, 0], "the work pass runs N rounds, the startup pass 0");
        assert_eq!(outcome.pairs, vec![(110.0, 100.0); 3]);
        assert!(outcome.ok() && outcome.notes.is_empty() && !outcome.unsupported);
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
        assert!(case_metric(&result, "peak_rss_bytes").is_none());
    }

    #[test]
    fn a_failing_startup_pass_keeps_the_completed_pairs() {
        let mut samples = 0;
        let outcome = measure_case(1_000, 3, |pass| {
            samples += 1;
            if samples == 4 {
                assert_eq!(pass, Pass::Startup);
                return Err(PassError {
                    note: "case send_recv (rust): exited 1: boom (startup pass)".to_string(),
                    unsupported: false,
                });
            }
            Ok(fake_sample(pass, 110.0, 100.0, 1_000))
        });
        assert_eq!(outcome.pairs, vec![(110.0, 100.0)], "round 1 only");
        assert!(outcome.ok());
        assert_eq!(outcome.marginal.rounds, 1);
        assert!(!outcome.unsupported);
        assert!(
            outcome.notes.iter().any(|n| n.contains("boom")),
            "{:?}",
            outcome.notes
        );

        let gap = || PassError {
            note: "case send_recv: unsupported: unsupported_bif".into(),
            unsupported: true,
        };
        let outcome = measure_case(1_000, 2, |_pass| Err::<Sample, PassError>(gap()));
        assert!(outcome.pairs.is_empty() && !outcome.ok() && outcome.unsupported);
        assert_eq!(outcome.marginal.unit_cost_ns(), None);
        assert!(outcome.notes.iter().any(|n| n.contains("unsupported_bif")));
    }

    #[test]
    fn the_metric_is_withheld_inside_the_noise_band() {
        let result = measured(1.0, 3);
        assert!(
            case_metric(&result, "ns_per_op").is_none(),
            "no figure inside the noise band"
        );
        for suffix in ["ops", "wall_ms", "startup_ms"] {
            assert!(case_metric(&result, suffix).is_some(), "{suffix}");
        }
        let note =
            case_note(&result, "marginal below timer resolution").expect("the withholding note");
        assert!(note.contains("paired median=1.000 ms"), "{note}");
    }

    #[test]
    fn the_op_count_is_the_deterministic_loop_count() {
        assert_eq!(ops(0), 0);
        assert_eq!(ops(1), 1);
        let case = &CASES[0];
        assert_eq!(ops(case.n_for(RuntimeKind::Rust)), case.n_rust);
        assert_eq!(ops(case.n_for(RuntimeKind::Sys)), case.n_sys);
        assert_eq!(expected_stdout(0), "0");
        assert_eq!(expected_stdout(1), PER_OP.to_string());
        assert_eq!(expected_stdout(3), (3 * PER_OP).to_string());
    }

    /// The generated driver pins the shared constants, validates the fixture's payload, and
    /// prints the attribution line in the compiling process.
    #[test]
    fn the_driver_source_pins_the_shared_constants() {
        let src = driver_source();
        assert!(src.starts_with(&format!("-module({DRIVER_MODULE}).")), "{src}");
        assert!(src.contains(&format!(
            "-export([{SEND_RECV}/{ARITY}, {ETS_COPY}/{ARITY}, {COMPILE}/{ARITY}])."
        )));
        for needle in [
            "gc_class_copy_bound:send_recv(N)",
            "gc_class_copy_bound:ets_copy(N)",
            "gc_class_compile:run(N)",
            "io:format(\"which=~ts~n\", [which_text(code:which(compile))])",
            "which_text(Path) when is_list(Path) -> Path;",
            "N * 1",
            "{1, Elements}",
            "length(Elements)",
            "{payload_length, Other}",
            "{payload_shape, Other}",
        ] {
            assert!(
                src.contains(needle),
                "missing `{needle}` in:\n{src}"
            );
        }
        // A zero loop count never touches the fixture: the startup sample stays pure.
        for case in CASES {
            assert!(
                src.contains(&format!("{}(0) -> 0;", case.function)),
                "{}",
                case.name
            );
        }
        // Three case clauses return `N * {PER_OP}` (one per case) plus the send_recv comment
        // that documents the same expression.
        assert_eq!(src.matches("N * 1").count(), 4);
    }

    /// Oracle-gated smoke test: with `erlc` and a resolvable runtime present, the cases must
    /// produce real timings (or an honest note). No-op when prerequisites are missing.
    #[test]
    fn smoke_runs_on_each_resolvable_runtime() {
        let Some(erlc) = slice::resolve_erlc() else {
            return;
        };
        if !fixture_dir().join(COPY_BOUND_FIXTURE).is_file() || !compile_source().is_file() {
            return;
        }
        let runtimes = [
            crate::runtime::resolve_oracle().ok(),
            Some(crate::runtime::resolve_rust()),
        ];
        for runtime in runtimes.into_iter().flatten() {
            if !runtime.self_check() {
                continue;
            }
            let scratch = ScratchDir::new("bench-gc-class-smoke").expect("scratch");
            stage(scratch.path(), &erlc).expect("the fixtures + driver must compile");
            let out_root = scratch.path().join("compile_out");
            prepare_out_dirs(&out_root, 1).expect("outdirs");
            let env = CompileEnv {
                source: compile_source(),
                out_root,
            };
            let program = runtime.program().expect("a self-checking runtime resolves");
            for case in CASES {
                let outcome = execute_case(program, runtime.kind, scratch.path(), case, &env, 1);
                assert!(
                    outcome.ok() || !outcome.notes.is_empty(),
                    "runtime {:?} case {}: neither samples nor notes",
                    runtime.kind,
                    case.name
                );
                if outcome.ok() {
                    let expected = expected_stdout(outcome.n);
                    let stdout = outcome.observed.as_deref().unwrap_or_default();
                    let (ops, _) = parse_stdout(case, stdout).unwrap_or_else(|e| {
                        panic!("runtime {:?} case {}: {e}", runtime.kind, case.name)
                    });
                    assert_eq!(
                        ops, expected,
                        "runtime {:?} case {}: the deterministic constant",
                        runtime.kind, case.name
                    );
                }
            }
        }
    }
}
