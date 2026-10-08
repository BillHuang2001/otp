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

//! `slice` — the same Erlang **module-run** workloads under *both* runtimes.
//!
//! Where the other workloads measure a single (mostly oracle-only) external target, this
//! workload runs a fixed table of small, deterministic **module-run cases** under *whatever
//! runtime it is scheduled on* and records the **per-case wall time and peak RSS**. Run
//! under both runtimes it is a real **apples-to-apples speed + memory comparison** of the
//! same programs — the same `.beam` executing the same function with the same arguments.
//!
//! The cases are:
//!
//! * three functions of the committed fixture module `slice` (`add/2`, `fib/1`, `pick/1`),
//!   resolved from `<CARGO_MANIFEST_DIR>/../../crates/beam_loader/tests/fixtures/slice.beam`
//!   (the differential corpus' real `erlc`-compiled `.beam`); and
//! * five tiny stdlib/kernel-wrapper modules compiled from **embedded** Erlang source with
//!   the system `erlc` (mirroring the differential corpus' `mexec` style): each takes an
//!   int and returns an int, calling a *real* system module (`calendar`, `queue`,
//!   `gb_trees`, `base64`, `ordsets`) so the Rust runtime must load and execute the real
//!   system module, exactly as the oracle does.
//!
//! Every case is verified to **agree** (stdout) under both runtimes before being listed;
//! the per-runtime `observed stdout` note makes that agreement inspectable in the report.
//!
//! # Honest reporting (never a fabricated number)
//!
//! * **Rust side** — exit `0` ⇒ record the metrics; exit `3` ⇒ an `unsupported: <reason>`
//!   note and **no** timing for that case; any other exit (or a spawn error) ⇒ a failure
//!   note and no timing.
//! * **Sys side** — exit `0` ⇒ record the metrics; otherwise a note.
//! * Missing prerequisites (`erlc` absent, or the committed fixture missing and
//!   unrebuildable) ⇒ a `skipped(erlc-absent)` / `skipped(fixture-missing)`.
//! * Memory is measured **externally** as peak RSS (`VmHWM`, via [`crate::proc::run`]);
//!   the Rust runtime has no in-runtime memory introspection to query.
//!
//! Everything is staged in a private, self-cleaning [`ScratchDir`] under `$TMPDIR`, so the
//! read-only checkouts are never written to. The workload never panics.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::metrics::{Category, Metric, Probe, Runtimes, Status};
use crate::proc::{self, Completed};
use crate::report::{Profile, WorkloadResult};
use crate::runtime::RuntimeKind;
use crate::scratch::ScratchDir;
use crate::workload::{RunContext, Workload};

/// The committed fixture module, relative to `CARGO_MANIFEST_DIR`
/// (`erts_rust/tools/bench`).
const FIXTURE_REL: &str = "../../crates/beam_loader/tests/fixtures/slice.beam";

/// An embedded equivalent of the fixture module, used **only** when the committed `.beam`
/// is missing: the workload rebuilds it with `erlc` rather than fabricating a result.
const SLICE_SOURCE: &str = "-module(slice).\n\
     -export([add/2, fib/1, pick/1]).\n\
     add(A, B) -> A + B.\n\
     fib(N) -> fib(N, 0, 1).\n\
     fib(0, A, _B) -> A;\n\
     fib(N, A, B) -> fib(N-1, B, A+B).\n\
     pick(a) -> 1;\n\
     pick(_) -> 2.\n";

/// The `calendar` wrapper — integer arithmetic and tuple access inside the real stdlib
/// module (`calendar:last_day_of_the_month/2`).
const SRC_CALENDAR: &str = "-module(bslice_calendar).\n\
     -export([last_day/2]).\n\
     last_day(Y, M) -> calendar:last_day_of_the_month(Y, M).\n";

/// The `queue` wrapper — the real functional FIFO (`queue:from_list/1`, `len/1`).
const SRC_QUEUE: &str = "-module(bslice_queue).\n\
     -export([from_size/1]).\n\
     from_size(N) -> queue:len(queue:from_list([N, N + 1, N + 2])).\n";

/// The `gb_trees` wrapper — the real balanced tree (a `foldl` build then `size/1`).
const SRC_GB_TREES: &str = "-module(bslice_gb_trees).\n\
     -export([size/1]).\n\
     size(N) -> gb_trees:size(lists:foldl(fun(X, T) -> gb_trees:insert(X, X * 10, T) end,\n\
     \x20                                    gb_trees:empty(), lists:seq(1, N))).\n";

/// The `base64` wrapper — the real codec, which builds an off-heap binary (`encode/1`).
const SRC_BASE64: &str = "-module(bslice_base64).\n\
     -export([encode_size/1]).\n\
     encode_size(N) -> byte_size(base64:encode(list_to_binary(lists:seq(1, N)))).\n";

/// The `ordsets` wrapper — the real ordered-set module, which itself calls `lists`
/// (`ordsets:from_list/1`).
const SRC_ORDSETS: &str = "-module(bslice_ordsets).\n\
     -export([from_size/1]).\n\
     from_size(N) -> length(ordsets:from_list([1, 2, 2, N])).\n";

/// The `string` wrapper — the real Unicode-aware `string` module (`length/1`), whose
/// argument is a **char list** (the `run` string-arg form).
const SRC_STRING: &str = "-module(bslice_string).\n\
     -export([str_length/1]).\n\
     str_length(S) -> string:length(S).\n";

/// One module-run argument, rendered per runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arg {
    /// A signed small integer.
    Int(i64),
    /// A bare atom name.
    Atom(&'static str),
    /// A double-quoted string (a char list on both sides).
    Str(&'static str),
}

impl Arg {
    /// The `beam_rt run` argv token (the grammar of the Rust runtime's `run` action).
    fn rust_token(&self) -> String {
        match self {
            Arg::Int(v) => v.to_string(),
            Arg::Atom(a) => (*a).to_string(),
            Arg::Str(s) => format!("\"{}\"", escape(s)),
        }
    }

    /// The Erlang source literal used inside the oracle's `-eval` expression.
    fn erl_literal(&self) -> String {
        match self {
            Arg::Int(v) => v.to_string(),
            Arg::Atom(a) => (*a).to_string(),
            Arg::Str(s) => format!("\"{}\"", escape(s)),
        }
    }
}

/// Escapes a string for both the `beam_rt` `"…"` token and an Erlang string literal (the
/// two grammars share the same escape set).
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

/// One fixed module-run case: a module, an exported function, its arguments and the
/// stdout both runtimes must print.
struct Case {
    /// Stable, short case name (used in the metric names, e.g. `case_<name>_wall_ms`).
    name: &'static str,
    /// The Erlang module.
    module: &'static str,
    /// The exported function.
    function: &'static str,
    /// The function's arity (must equal `args.len()`).
    arity: u32,
    /// The call arguments.
    args: &'static [Arg],
    /// The stdout (trailing-whitespace-trimmed) both runtimes must print.
    expected: &'static str,
    /// Embedded source compiled with `erlc`; `None` ⇒ the committed fixture module.
    source: Option<&'static str>,
}

/// The fixed case table, in report order.
static CASES: &[Case] = &[
    Case {
        name: "add",
        module: "slice",
        function: "add",
        arity: 2,
        args: &[Arg::Int(2), Arg::Int(3)],
        expected: "5",
        source: None,
    },
    Case {
        name: "fib",
        module: "slice",
        function: "fib",
        arity: 1,
        args: &[Arg::Int(20)],
        expected: "6765",
        source: None,
    },
    Case {
        name: "pick",
        module: "slice",
        function: "pick",
        arity: 1,
        args: &[Arg::Atom("hello")],
        expected: "2",
        source: None,
    },
    Case {
        name: "calendar_last_day",
        module: "bslice_calendar",
        function: "last_day",
        arity: 2,
        args: &[Arg::Int(2024), Arg::Int(2)],
        expected: "29",
        source: Some(SRC_CALENDAR),
    },
    Case {
        name: "queue_from_size",
        module: "bslice_queue",
        function: "from_size",
        arity: 1,
        args: &[Arg::Int(5)],
        expected: "3",
        source: Some(SRC_QUEUE),
    },
    Case {
        name: "gb_trees_size",
        module: "bslice_gb_trees",
        function: "size",
        arity: 1,
        args: &[Arg::Int(5)],
        expected: "5",
        source: Some(SRC_GB_TREES),
    },
    Case {
        name: "base64_encode_size",
        module: "bslice_base64",
        function: "encode_size",
        arity: 1,
        args: &[Arg::Int(5)],
        expected: "8",
        source: Some(SRC_BASE64),
    },
    Case {
        name: "ordsets_from_size",
        module: "bslice_ordsets",
        function: "from_size",
        arity: 1,
        args: &[Arg::Int(5)],
        expected: "3",
        source: Some(SRC_ORDSETS),
    },
    Case {
        name: "string_str_length",
        module: "bslice_string",
        function: "str_length",
        arity: 1,
        args: &[Arg::Str("hello")],
        expected: "5",
        source: Some(SRC_STRING),
    },
];

/// The `slice` module-run workload.
pub struct Slice;

/// The number of timed runs per case for a profile (quick ≈ 5, full ≈ 25).
fn runs_for(profile: Profile) -> usize {
    match profile {
        Profile::Quick => 5,
        Profile::Full => 25,
    }
}

/// The committed fixture's path.
fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_REL)
}

/// Resolves `erlc`: `$ERLC` when set (and existing), else `erlc` on `PATH`.
///
/// Shared with the `foreign` workload (the other workload that compiles real `.erl`
/// sources), so the two agree on the toolchain lookup exactly.
pub(crate) fn resolve_erlc() -> Option<PathBuf> {
    match std::env::var_os("ERLC") {
        Some(value) if !value.is_empty() => {
            let path = PathBuf::from(value);
            if path.is_file() {
                Some(path)
            } else {
                None
            }
        }
        _ => proc::which("erlc"),
    }
}

/// Builds the per-side argv for one case (everything after the program path).
fn argv_for(kind: RuntimeKind, dir: &Path, case: &Case) -> Vec<String> {
    match kind {
        RuntimeKind::Rust => {
            let beam = dir.join(format!("{}.beam", case.module));
            let mut argv = vec![
                "run".to_string(),
                beam.display().to_string(),
                format!("{}:{}/{}", case.module, case.function, case.arity),
            ];
            argv.extend(case.args.iter().map(Arg::rust_token));
            argv
        }
        RuntimeKind::Sys => {
            let args: Vec<String> = case.args.iter().map(Arg::erl_literal).collect();
            let call = format!("{}:{}({})", case.module, case.function, args.join(", "));
            let expr = format!("io:format(\"~w~n\",[{call}]), halt(0).");
            vec![
                "-noshell".to_string(),
                "-pa".to_string(),
                dir.display().to_string(),
                "-eval".to_string(),
                expr,
            ]
        }
    }
}

/// Compiles `source` (module `module`) into a `.beam` under `dir`, returning its path.
///
/// Mirrors the differential corpus' recipe exactly: write `<dir>/<module>.erl`, run
/// `erlc -o <dir> <dir>/<module>.erl`.
fn compile_module(dir: &Path, erlc: &Path, module: &str, source: &str) -> Result<PathBuf, String> {
    let src = dir.join(format!("{module}.erl"));
    std::fs::write(&src, source).map_err(|e| format!("write {}: {e}", src.display()))?;
    let mut cmd = Command::new(erlc);
    cmd.arg("-o").arg(dir).arg(&src);
    let out = proc::run(&mut cmd).map_err(|e| format!("erlc spawn failed: {e}"))?;
    let beam = dir.join(format!("{module}.beam"));
    if out.success && beam.is_file() {
        Ok(beam)
    } else {
        Err(format!(
            "erlc exited {:?}: {}",
            out.exit_code,
            out.stderr.trim()
        ))
    }
}

/// Stages every runnable case's `.beam` into `dir`, returning the cases that are ready.
///
/// The fixture module is copied from the committed `.beam` (or rebuilt from the embedded
/// equivalent); each embedded wrapper is compiled once with `erlc`. Cases that cannot be
/// staged are reported in `notes` — never silently dropped.
fn stage(dir: &Path, erlc: Option<&Path>, notes: &mut Vec<String>) -> Vec<&'static Case> {
    let mut ready: Vec<&'static Case> = Vec::new();

    // --- the fixture module ----------------------------------------------------------
    let fixture_beam = dir.join("slice.beam");
    if std::fs::copy(fixture_path(), &fixture_beam).is_err() {
        match erlc {
            Some(erlc) => {
                if let Err(e) = compile_module(dir, erlc, "slice", SLICE_SOURCE) {
                    notes.push(format!("fixture module not rebuilt: {e}"));
                }
            }
            None => notes.push(format!(
                "fixture {} missing and erlc unavailable: cannot rebuild",
                fixture_path().display()
            )),
        }
    }

    // --- the embedded stdlib wrappers ------------------------------------------------
    let mut attempted: Vec<&'static str> = Vec::new();
    for case in CASES {
        let beam = dir.join(format!("{}.beam", case.module));
        if case.source.is_none() {
            if beam.is_file() {
                ready.push(case);
            }
            continue;
        }
        if beam.is_file() {
            ready.push(case);
            continue;
        }
        if attempted.contains(&case.module) {
            continue; // a previous case already tried (and failed) to compile this module
        }
        attempted.push(case.module);
        let Some(erlc) = erlc else {
            notes.push(format!(
                "case {}: erlc unavailable, cannot compile {}",
                case.name, case.module
            ));
            continue;
        };
        let source = case.source.unwrap_or_default();
        if let Err(e) = compile_module(dir, erlc, case.module, source) {
            notes.push(format!("case {}: compile failed: {e}", case.name));
            continue;
        }
        ready.push(case);
    }

    ready
}

/// The outcome of running one case several times under one runtime.
struct CaseOutcome {
    /// Wall-clock samples (ms), one per successful run.
    wall: Vec<f64>,
    /// Peak-RSS samples (bytes), one per run whose `VmHWM` was readable.
    rss: Vec<f64>,
    /// The stdout observed on the first successful run.
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

/// Runs one case `runs` times under `program` and collects its timings and notes.
fn execute_case(
    program: &Path,
    kind: RuntimeKind,
    dir: &Path,
    case: &Case,
    runs: usize,
) -> CaseOutcome {
    let mut outcome = CaseOutcome {
        wall: Vec::new(),
        rss: Vec::new(),
        observed: None,
        unsupported: false,
        notes: Vec::new(),
    };

    for _ in 0..runs {
        let mut cmd = Command::new(program);
        cmd.args(argv_for(kind, dir, case));
        match proc::run(&mut cmd) {
            Ok(completed) if completed.success => {
                outcome.wall.push(completed.wall_ms);
                if let Some(bytes) = completed.peak_memory_bytes {
                    outcome.rss.push(bytes as f64);
                }
                let got = completed.stdout_trim().to_string();
                match &outcome.observed {
                    None => {
                        if got != case.expected {
                            outcome.notes.push(format!(
                                "case {}: unexpected stdout `{got}` (expected `{}`)",
                                case.name, case.expected
                            ));
                        }
                        outcome.observed = Some(got);
                    }
                    Some(previous) if *previous != got => outcome.notes.push(format!(
                        "case {}: stdout varied (`{previous}` vs `{got}`)",
                        case.name
                    )),
                    Some(_) => {}
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

    outcome
}

impl Slice {
    /// The core of the workload, parameterized by the per-case run count (so tests stay
    /// fast). [`Workload::run`] calls it with the profile's run count.
    fn run_with_runs(&self, ctx: &RunContext, runs: usize) -> WorkloadResult {
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

        let scratch = match ScratchDir::new("slice") {
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
        let erlc = resolve_erlc();

        // Stage the `.beam`s; nothing runnable ⇒ an honest skip, never a fake number.
        let mut staging_notes: Vec<String> = Vec::new();
        let ready = stage(dir, erlc.as_deref(), &mut staging_notes);
        if ready.is_empty() {
            let reason = if erlc.is_none() {
                "erlc-absent"
            } else {
                "fixture-missing"
            };
            let mut result = WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                reason,
            );
            result.notes = staging_notes;
            return result;
        }

        let mut result = WorkloadResult::new(self.id(), kind, self.category(), self.description());
        result.notes = staging_notes;

        let mut overall_peak: Option<u64> = None;
        let mut observed_all: Vec<String> = Vec::new();
        let mut attempted = 0usize;
        let mut ok_cases = 0usize;
        let mut unsupported_cases = 0usize;

        for case in ready {
            let outcome = execute_case(program, kind, dir, case, runs);
            attempted += 1;

            if outcome.ok() {
                ok_cases += 1;
                result.metrics.push(Metric::samples(
                    format!("case_{}_wall_ms", case.name),
                    "ms",
                    &outcome.wall,
                ));
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
                if let Some(value) = &outcome.observed {
                    observed_all.push(format!("{}={value}", case.name));
                }
            } else if outcome.unsupported {
                unsupported_cases += 1;
            }

            result.notes.extend(outcome.notes);
        }

        if !observed_all.is_empty() {
            result
                .notes
                .push(format!("observed stdout: {}", observed_all.join(" ")));
        }
        result.peak_memory_bytes = overall_peak;

        if ok_cases == 0 {
            if attempted > 0 && unsupported_cases == attempted {
                result.status = Status::Skipped;
                result.reason = Some("unsupported-case-set".to_string());
            } else {
                result.status = Status::Failed;
                result.reason = Some("no-case-executed".to_string());
            }
        }

        result
    }
}

impl Workload for Slice {
    fn id(&self) -> &'static str {
        "slice"
    }

    fn description(&self) -> &'static str {
        "the same Erlang module runs under both runtimes (per-case wall time + peak RSS)"
    }

    fn category(&self) -> Category {
        Category::Compute
    }

    fn contracts(&self) -> &'static [&'static str] {
        &["A.1", "A.2", "A.7"]
    }

    fn runtimes(&self) -> Runtimes {
        Runtimes::Both
    }

    fn probe(&self) -> Probe {
        Probe::Ready
    }

    fn run(&self, ctx: &RunContext) -> WorkloadResult {
        self.run_with_runs(ctx, runs_for(ctx.profile))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Runtime;

    #[test]
    fn metadata_is_stable() {
        let w = Slice;
        assert_eq!(w.id(), "slice");
        assert_eq!(w.category(), Category::Compute);
        assert_eq!(w.runtimes(), Runtimes::Both);
        assert_eq!(w.contracts(), &["A.1", "A.2", "A.7"]);
        assert!(w.probe().is_ready());
        assert!(!w.description().is_empty());
    }

    #[test]
    fn case_table_is_internally_consistent() {
        assert!(!CASES.is_empty());
        let mut names: Vec<&str> = CASES.iter().map(|c| c.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "case names must be unique");

        for case in CASES {
            assert_eq!(
                case.arity as usize,
                case.args.len(),
                "case {} arity {} != {} args",
                case.name,
                case.arity,
                case.args.len()
            );
            assert!(!case.module.is_empty(), "case {} has no module", case.name);
            assert!(
                !case.function.is_empty(),
                "case {} has no function",
                case.name
            );
            assert!(
                !case.expected.is_empty(),
                "case {} has no golden",
                case.name
            );
            if let Some(source) = case.source {
                assert!(
                    source.contains(&format!("-module({}).", case.module)),
                    "case {} source does not declare module {}",
                    case.name,
                    case.module
                );
                assert!(
                    source.contains(&format!("{}/{}", case.function, case.arity)),
                    "case {} source does not export {}/{}",
                    case.name,
                    case.function,
                    case.arity
                );
            }
        }
    }

    #[test]
    fn fixture_backed_and_embedded_cases_are_both_present() {
        assert!(CASES.iter().any(|c| c.source.is_none()));
        assert!(CASES.iter().any(|c| c.source.is_some()));
    }

    #[test]
    fn arg_tokens_render_for_both_runtimes() {
        assert_eq!(Arg::Int(2).rust_token(), "2");
        assert_eq!(Arg::Int(-3).rust_token(), "-3");
        assert_eq!(Arg::Atom("hello").rust_token(), "hello");
        assert_eq!(Arg::Str("hi").rust_token(), "\"hi\"");

        assert_eq!(Arg::Int(2).erl_literal(), "2");
        assert_eq!(Arg::Int(-3).erl_literal(), "-3");
        assert_eq!(Arg::Atom("hello").erl_literal(), "hello");
        assert_eq!(Arg::Str("hi").erl_literal(), "\"hi\"");

        // The two grammars share the escape set.
        let tricky = Arg::Str("a\"b\\c\n\t\r");
        assert_eq!(tricky.rust_token(), tricky.erl_literal());
        assert_eq!(tricky.rust_token(), "\"a\\\"b\\\\c\\n\\t\\r\"");
    }

    #[test]
    fn argv_shape_differs_per_runtime() {
        let case = &CASES[0]; // slice:add/2 with (2, 3)
        let rust = argv_for(RuntimeKind::Rust, Path::new("/d"), case);
        assert_eq!(rust[0], "run");
        assert_eq!(rust[1], "/d/slice.beam");
        assert_eq!(rust[2], "slice:add/2");
        assert_eq!(&rust[3..], &["2", "3"]);

        let sys = argv_for(RuntimeKind::Sys, Path::new("/d"), case);
        assert_eq!(sys[0], "-noshell");
        assert_eq!(sys[1], "-pa");
        assert_eq!(sys[2], "/d");
        assert_eq!(sys[3], "-eval");
        assert_eq!(sys[4], "io:format(\"~w~n\",[slice:add(2, 3)]), halt(0).");

        // The atom case renders bare on both sides.
        let atom_case = CASES
            .iter()
            .find(|c| c.name == "pick")
            .expect("the pick case");
        let rust = argv_for(RuntimeKind::Rust, Path::new("/d"), atom_case);
        assert_eq!(&rust[3..], &["hello"]);
        let sys = argv_for(RuntimeKind::Sys, Path::new("/d"), atom_case);
        assert!(sys[4].contains("slice:pick(hello)"));
    }

    #[test]
    fn runs_for_profile_is_bounded() {
        assert_eq!(runs_for(Profile::Quick), 5);
        assert_eq!(runs_for(Profile::Full), 25);
        assert!(runs_for(Profile::Quick) < runs_for(Profile::Full));
    }

    #[test]
    fn committed_fixture_exists_in_this_checkout() {
        assert!(
            fixture_path().is_file(),
            "expected the committed fixture at {}",
            fixture_path().display()
        );
    }

    /// The two capability-gap signals the Rust runtime raises on a stub exit must be
    /// classified as `unsupported`, and any other exit as a plain failure.
    #[test]
    fn rust_exit_classification() {
        let case = &CASES[0];
        let unsupported = Completed {
            exit_code: Some(3),
            success: false,
            stdout: String::new(),
            stderr: "beam_rt: unsupported: unsupported_opcode: bs_create_bin".to_string(),
            wall_ms: 1.0,
            peak_memory_bytes: None,
        };
        let note = classify_failure(RuntimeKind::Rust, case, &unsupported);
        assert_eq!(
            note,
            "case add: unsupported: unsupported_opcode: bs_create_bin"
        );

        let runtime_error = Completed {
            exit_code: Some(1),
            success: false,
            stdout: String::new(),
            stderr: "beam_rt: undef".to_string(),
            wall_ms: 1.0,
            peak_memory_bytes: None,
        };
        let note = classify_failure(RuntimeKind::Rust, case, &runtime_error);
        assert!(note.contains("exited 1"), "{note}");
        assert!(note.contains("beam_rt: undef"), "{note}");
    }

    #[test]
    fn unresolved_runtime_is_skipped_not_run() {
        let rt = Runtime {
            kind: RuntimeKind::Rust,
            program: None,
        };
        let ctx = RunContext::new(&rt, Profile::Quick, Path::new("/tmp"));
        let result = Slice.run_with_runs(&ctx, 1);
        assert_eq!(result.status, Status::Skipped);
        assert_eq!(result.reason.as_deref(), Some("runtime-unsupported"));
        assert!(result.metrics.is_empty());
    }

    /// When the fixture cannot be staged and `erlc` is unavailable, nothing is runnable:
    /// `stage` returns no cases and records *why* (never a silent empty result).
    #[test]
    fn stage_yields_nothing_when_fixture_and_erlc_are_both_unavailable() {
        let missing =
            std::env::temp_dir().join(format!("bench-slice-absent-{}-xyzzy", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        assert!(!missing.exists());

        let mut notes = Vec::new();
        let ready = stage(&missing, None, &mut notes);

        assert!(ready.is_empty());
        assert!(
            !notes.is_empty(),
            "the reason must be recorded, not swallowed"
        );
    }

    /// Oracle-gated smoke test: runs each resolvable runtime once and asserts a real
    /// measurement (or an honest skip). No-op when neither runtime resolves.
    #[test]
    fn smoke_runs_on_each_resolvable_runtime() {
        let runtimes = [
            crate::runtime::resolve_oracle().ok(),
            Some(crate::runtime::resolve_rust()),
        ];
        let out_dir = std::env::temp_dir();
        for runtime in runtimes.into_iter().flatten() {
            if !runtime.self_check() {
                continue;
            }
            let ctx = RunContext::new(&runtime, Profile::Quick, out_dir.as_path());
            let result = Slice.run_with_runs(&ctx, 1);
            // `erlc-absent` / `fixture-missing` are honest skips, not failures.
            if result.status == Status::Skipped {
                continue;
            }
            assert_eq!(
                result.status,
                Status::Ok,
                "runtime {:?} produced notes: {:?}",
                runtime.kind,
                result.notes
            );
            assert!(
                result.metrics.iter().any(|m| m.name == "case_add_wall_ms"),
                "expected a wall-time metric for `add`"
            );
            assert!(
                result
                    .notes
                    .iter()
                    .any(|n| n.starts_with("observed stdout:")),
                "expected an observed-stdout note"
            );
        }
    }
}
