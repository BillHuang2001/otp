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

//! The shared machinery behind the two **pure-Erlang application test-suite** workloads
//! (`jsone` — JSON, `bbmustache` — Mustache): one parameterized stage → compile → run →
//! compare implementation, two registrations.
//!
//! Each project is described by a [`Project`] constant (repo root, compiler flags, the
//! source/test directories to stage, the eunit suite modules, the *intended* test count).
//! [`jsone`](super::jsone) and [`bbmustache`](super::bbmustache) are then thin
//! [`Workload`](crate::workload::Workload) impls delegating to [`probe`] and [`run`] —
//! there is exactly one copy of the stage/compile/run logic.
//!
//! ## Where these two workloads sit in the benchmark suite
//!
//! Both projects are real, popular, **pure-Erlang** libraries whose own `eunit` suites are
//! public entry points: a whole-application Erlang program, exercised end to end, run under
//! whichever runtime is scheduled. Their value is that the *same* `.beam` files and the
//! *same* argv run under both runtimes, so a divergence is a runtime divergence — no
//! per-side argv (unlike [`slice`](super::slice)) and no per-side build.
//!
//! **Category: [`Category::Compute`].** A pure-Erlang test suite is CPU-bound Erlang-code
//! execution: term building, pattern matching, binaries, maps, recursion — no process
//! boundary being copied (that would be `messaging`), no file/network I/O, no collection
//! regime deliberately triggered (that would be `gc`). [`Category::Compiler`] would be
//! wrong: the `erlc` invocation is a one-off *setup* stage, measured separately as
//! `compile_ms`, while the timed metric is the whole `eunit:test/1` run. No new category is
//! warranted — this is exactly the "compute/JIT-bound" class the evaluation names.
//!
//! **Contracts: `A.1` + `A.7`.** A.1 (language semantics) because the suites exercise real
//! Erlang — binaries, maps, unicode, exceptions, comprehensions, recursion, `-D`-gated
//! code paths. A.7 (OTP app contracts) because a real `eunit` run needs the code path
//! (`-pa`), module loading, `io` output and the eunit framework itself inside the runtime.
//! Deliberately **not** A.9: nothing here builds or boots the runtime, and the compile is a
//! plain external `erlc` process, not an in-runtime build.
//!
//! ## Honesty rules (the crate's, applied here)
//!
//! * A missing repo, missing `erlc` or a wrong-shaped checkout is a `skipped` — never a
//!   fabricated number.
//! * A compile that fails, a suite that exits non-zero, a missing eunit summary or a test
//!   count that is not the intended one is a `failed` — never a skip.
//! * The published `tests_passed` is the **parsed, verified** count, never the `expected_tests`
//!   constant; `expected_tests` only decides whether a count is acceptable.
//! * The two sides must agree: the report builds the workload boxes once and iterates the
//!   runtimes over them, so a [`SideLedger`] carried by the workload compares the second
//!   side's count with the first side's and fails the later side on a mismatch.
//! * A Rust exit code `3` carrying the runtime's documented `beam_rt: unsupported: ` stderr
//!   line is a `skipped(runtime-unsupported)` (the crate's capability-gap convention), with
//!   a loud note because it regresses this project's verified-supported status.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use crate::metrics::{Category, Metric, Probe};
use crate::proc;
use crate::report::{Profile, WorkloadResult};
use crate::runtime::RuntimeKind;
use crate::scratch::{self, ScratchDir};
use crate::workload::RunContext;
use crate::workloads::{collect_files, resolve_repo, slice, tool};

pub(crate) use crate::workloads::eunit_ledger::{
    lock, shared_ledger, LedgerVerdict, SharedLedger, SideLedger, SideObservation,
};

/// The stderr prefix the Rust runtime uses for its documented capability-gap signal.
const CAPABILITY_GAP_PREFIX: &str = "beam_rt: unsupported: ";

/// A parameterized benchmark target: one pure-Erlang project with an `eunit` suite.
///
/// Everything the shared machinery needs is here, so a project is a table entry rather
/// than a new implementation. `expected_tests` is the **intended** count — a run whose
/// parsed count differs is a failure, and the intended number is never published as a
/// measurement.
#[derive(Debug)]
pub(crate) struct Project {
    /// The workload id (also the `--repo` override key and the scratch-dir tag).
    pub id: &'static str,
    /// The documented default checkout root (`$BENCH_<ID>_REPO` overrides it).
    pub default_repo: &'static str,
    /// The exact `erlc` flag set the project's own test profile uses.
    pub erlc_flags: &'static [&'static str],
    /// Source directories to stage and compile.
    pub src_dirs: &'static [&'static str],
    /// Test directories to stage and compile (the eunit suites live here).
    pub test_dirs: &'static [&'static str],
    /// The suite modules handed to `eunit:test/1`, in order.
    pub eunit_modules: &'static [&'static str],
    /// The intended number of tests in the suite.
    pub expected_tests: usize,
    /// The benchmark category.
    pub category: Category,
    /// A one-line human description.
    pub description: &'static str,
}

/// `jsone` — pure-Erlang JSON encoder/decoder (rebar3 `test` profile flags).
pub(crate) const JSONE: Project = Project {
    id: "jsone",
    default_repo: "/home/bill/Source/jsone",
    erlc_flags: &["-DTEST", "-DMAP_ITER_ORDERED", "-DTIME_MODULE=test_time_module"],
    src_dirs: &["src"],
    test_dirs: &["test"],
    eunit_modules: &["jsone_decode_tests", "jsone_encode_tests", "jsone_inet_tests"],
    expected_tests: 90,
    category: Category::Compute,
    description: "jsone test suite (eunit) — pure-Erlang JSON encode/decode, compute bound",
};

/// `bbmustache` — pure-Erlang Mustache templates (rebar3 `test` profile flags).
pub(crate) const BBMUSTACHE: Project = Project {
    id: "bbmustache",
    default_repo: "/home/bill/Source/bbmustache",
    erlc_flags: &["+export_all", "-Dnamespaced_types", "-Dunicode_supported"],
    src_dirs: &["src"],
    test_dirs: &["test"],
    eunit_modules: &["bbmustache_tests"],
    expected_tests: 79,
    category: Category::Compute,
    description: "bbmustache test suite (eunit) — pure-Erlang Mustache rendering, compute bound",
};

/// The resolved repository root for `p`: `$BENCH_<ID>_REPO`, else the documented default.
pub(crate) fn repo(p: &Project) -> PathBuf {
    resolve_repo(p.id, p.default_repo)
}

/// The probe's pure decision: is the checkout there and is there an `erlc` to compile with?
///
/// Split out from [`probe`] so both honest skip reasons are unit-testable without an
/// environment.
pub(crate) fn probe_of(repo: &Path, erlc: Option<&Path>) -> Probe {
    if !repo.is_dir() {
        return Probe::skipped("repo-missing");
    }
    if erlc.is_none() {
        return Probe::skipped("erlc-absent");
    }
    Probe::Ready
}

/// The honest prerequisite check for `p`.
pub(crate) fn probe(p: &Project) -> Probe {
    probe_of(&repo(p), slice::resolve_erlc().as_deref())
}

/// How many times a side runs the whole suite: `--quick` 3, `--full` 10 (the house rule for
/// whole-run-suite sampling).
pub(crate) fn runs_for(profile: Profile) -> usize {
    match profile {
        Profile::Quick => 3,
        Profile::Full => 10,
    }
}

/// The `eunit:test/1` expression both runtimes are given.
///
/// The `case` form is deliberate: `eunit:test/1` returns `ok` or a non-`ok` error term, so
/// a failing suite must exit non-zero instead of falling through to a happy `halt()`.
fn eunit_expr(modules: &[&str]) -> String {
    format!(
        "case eunit:test([{}]) of ok -> halt(0); _ -> halt(1) end.",
        modules.join(",")
    )
}

/// The argv (everything after the program path) that runs the suite.
///
/// Both runtimes receive the **identical** argv: `beam_rt` is a drop-in `erl` (contract
/// A.6), and this workload's whole point is that only the program path differs. The
/// `RuntimeKind` parameter exists so that identity is a pinned test instead of an
/// unstated assumption — a future divergence has to be written down here.
pub(crate) fn argv_for(kind: RuntimeKind, ebin: &Path, modules: &[&str]) -> Vec<String> {
    match kind {
        RuntimeKind::Sys | RuntimeKind::Rust => vec![
            "-noshell".to_string(),
            "-pa".to_string(),
            ebin.display().to_string(),
            "-eval".to_string(),
            eunit_expr(modules),
        ],
    }
}

/// What a side's sampled runs said about the test count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CountVerdict {
    /// Every sampled round reported the intended count; the count may be published.
    Agrees(usize),
    /// No sampled round printed an eunit summary at all.
    NoSummary,
    /// Two sampled rounds of the same side disagree with each other.
    RoundsDisagree {
        /// The first round's count.
        first: usize,
        /// The round that disagreed.
        other: usize,
        /// That round's summary line, verbatim (trimmed).
        line: String,
    },
    /// The suite ran and printed a count, but it is not the intended one.
    Unexpected {
        /// The intended count from the project table.
        intended: usize,
        /// The observed count.
        observed: usize,
        /// The observed summary line, verbatim (trimmed).
        line: String,
    },
}

/// Judges one side's per-round counts against the intended count.
///
/// Pure, so every branch is unit-tested; the caller only maps a non-agreeing verdict to a
/// `failed` result (never a skip) and never publishes a count it did not verify.
pub(crate) fn count_verdict(expected: usize, summaries: &[(usize, String)]) -> CountVerdict {
    let Some((first, line)) = summaries.first() else {
        return CountVerdict::NoSummary;
    };
    if let Some((other, other_line)) = summaries.iter().find(|(count, _)| count != first) {
        return CountVerdict::RoundsDisagree {
            first: *first,
            other: *other,
            line: other_line.clone(),
        };
    }
    if *first != expected {
        return CountVerdict::Unexpected {
            intended: expected,
            observed: *first,
            line: line.clone(),
        };
    }
    CountVerdict::Agrees(*first)
}

/// The eunit summary line, parsed from a run's stdout.
///
/// Returns the observed test **count** (total tests, so a failing summary counts its
/// failures too) and the summary line verbatim (trimmed), or `None` when the run printed
/// no summary — a runtime/framework failure the caller must not mistake for a pass.
///
/// Recognised shapes (real eunit output, indented by eunit):
/// * `All 90 tests passed.`
/// * `Failed: 1.  Skipped: 0.  Passed: 2.`
/// * `There were 3 tests, 1 failures`
pub(crate) fn parse_summary(stdout: &str) -> Option<(usize, String)> {
    stdout.lines().rev().find_map(|raw| {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("All ") {
            if let Some(number) = rest.strip_suffix(" tests passed.") {
                return number.trim().parse().ok().map(|n| (n, line.to_string()));
            }
        }
        if let Some((failed, passed, skipped)) = failure_counts(line) {
            return Some((failed + passed + skipped, line.to_string()));
        }
        if let Some(rest) = line.strip_prefix("There were ") {
            let number = rest.split(" tests,").next()?.trim();
            return number.parse().ok().map(|n| (n, line.to_string()));
        }
        None
    })
}

/// Parses eunit's `Failed: F.  Skipped: S.  Passed: P.` line (`Skipped:` may be absent).
fn failure_counts(line: &str) -> Option<(usize, usize, usize)> {
    if !(line.contains("Failed:") && line.contains("Passed:")) {
        return None;
    }
    let failed = count_after(line, "Failed:")?;
    let passed = count_after(line, "Passed:")?;
    let skipped = count_after(line, "Skipped:").unwrap_or(0);
    Some((failed, passed, skipped))
}

/// The integer that follows `key` in `line`, if any.
fn count_after(line: &str, key: &str) -> Option<usize> {
    let rest = line.split_once(key)?.1.trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The compile stage's verdict from the observable facts.
///
/// `erlc` exits `0` for a compile that only *warns* (jsone emits a
/// `nowarn_deprecated_catch` deprecation warning on stderr while succeeding), so a warning
/// can never fail the stage; a non-zero exit, or a source that produced no `.beam`, is a
/// failure and must be reported as one — never downgraded to a skip.
pub(crate) fn compile_verdict(success: bool, missing_beams: &[String]) -> Result<(), String> {
    if !success {
        return Err("module-compile-failed".to_string());
    }
    if !missing_beams.is_empty() {
        return Err(format!(
            "module-compile-failed: no .beam produced for {}",
            missing_beams.join(", ")
        ));
    }
    Ok(())
}

/// The Rust runtime's documented capability-gap signal, if this exit carries it.
///
/// Pure, so the classification is unit-tested: only the Rust runtime, only exit code `3`,
/// and only with the documented `beam_rt: unsupported: <reason>` stderr line. Anything
/// else is a genuine failure, never a skip.
pub(crate) fn capability_gap(kind: RuntimeKind, exit_code: Option<i32>, stderr: &str) -> Option<String> {
    if kind != RuntimeKind::Rust || exit_code != Some(3) {
        return None;
    }
    stderr.lines().find_map(|line| {
        line.trim()
            .strip_prefix(CAPABILITY_GAP_PREFIX)
            .map(|reason| reason.trim().to_string())
    })
}

/// Adds the owner's write bit to `path`, returning whether it changed anything.
///
/// Never follows a symlink (the caller skips symlinks, and `metadata` is only applied to
/// regular files/directories it enumerated).
fn add_write_bit(path: &Path) -> std::io::Result<bool> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        const OWNER_WRITE: u32 = 0o200;
        let mode = permissions.mode();
        if mode & OWNER_WRITE != 0 {
            return Ok(false);
        }
        permissions.set_mode(mode | OWNER_WRITE);
    }
    #[cfg(not(unix))]
    {
        if !permissions.readonly() {
            return Ok(false);
        }
        permissions.set_readonly(false);
    }
    std::fs::set_permissions(path, permissions)?;
    Ok(true)
}

/// Makes a staged tree writable (files **and** directories), returning how many entries
/// were changed.
///
/// The configured checkouts can arrive with read-only modes; `scratch::copy_tree` preserves
/// the source mode, so the staged copy must be opened up before anything builds or before
/// the RAII cleanup can remove it. Errors — including a missing tree — are returned, never
/// swallowed.
pub(crate) fn make_tree_writable(root: &Path) -> std::io::Result<usize> {
    let mut changed = 0;
    if add_write_bit(root)? {
        changed += 1;
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if add_write_bit(&path)? {
            changed += 1;
        }
        if file_type.is_dir() {
            changed += make_tree_writable(&path)?;
        }
    }
    Ok(changed)
}

/// A skipped result carrying the base notes gathered so far.
fn skipped_with(
    p: &Project,
    kind: RuntimeKind,
    reason: impl Into<String>,
    notes: &[String],
) -> WorkloadResult {
    let mut result = WorkloadResult::skipped(p.id, kind, p.category, p.description, reason);
    result.notes.extend_from_slice(notes);
    result
}

/// A failed result carrying the base notes gathered so far.
fn failed_with(
    p: &Project,
    kind: RuntimeKind,
    reason: impl Into<String>,
    notes: &[String],
) -> WorkloadResult {
    let mut result = WorkloadResult::failed(p.id, kind, p.category, p.description, reason);
    result.notes.extend_from_slice(notes);
    result
}

/// Stages, compiles and runs `p`'s eunit suite under `ctx`'s runtime, then compares the
/// observed test count with `expected_tests` and with the other side via `ledger`.
pub(crate) fn run(p: &Project, ctx: &RunContext, ledger: &Mutex<SideLedger>) -> WorkloadResult {
    let kind = ctx.runtime.kind;
    let Some(program) = ctx.runtime.program() else {
        return skipped_with(p, kind, "runtime-unsupported", &[]);
    };
    let repo_root = repo(p);
    if !repo_root.is_dir() {
        return skipped_with(p, kind, "repo-missing", &[]);
    }
    let Some(erlc) = slice::resolve_erlc() else {
        return skipped_with(p, kind, "erlc-absent", &[]);
    };

    let mut notes = vec![
        format!("repo: {}", repo_root.display()),
        format!("erlc: {}", erlc.display()),
        format!("compile flags: {}", p.erlc_flags.join(" ")),
        format!("argv: eunit:test([{}])", p.eunit_modules.join(",")),
    ];

    // Stage the read-only checkout into a private scratch copy and compile there.
    let scratch = match ScratchDir::new(p.id) {
        Ok(dir) => dir,
        Err(e) => return failed_with(p, kind, format!("scratch-dir-failed: {e}"), &notes),
    };
    let work = scratch.path().join(p.id);
    let mut sources: Vec<PathBuf> = Vec::new();
    let mut dirs: Vec<&str> = Vec::new();
    for dir in p.src_dirs.iter().copied().chain(p.test_dirs.iter().copied()) {
        let from = repo_root.join(dir);
        if !from.is_dir() {
            notes.push(format!("missing expected directory: {}", from.display()));
            return skipped_with(p, kind, "sources-missing", &notes);
        }
        let to = work.join(dir);
        if let Err(e) = scratch::copy_tree(&from, &to, tool::REPO_SKIP_NAMES) {
            return failed_with(p, kind, format!("scratch-copy-failed: {e}"), &notes);
        }
        match make_tree_writable(&to) {
            Ok(opened) => {
                if opened > 0 {
                    notes.push(format!("opened {opened} read-only entries under {dir}/"));
                }
            }
            Err(e) => return failed_with(p, kind, format!("stage-chmod-failed: {e}"), &notes),
        }
        let mut found = collect_files(&to, "erl");
        notes.push(format!("{dir}/: {} .erl source(s)", found.len()));
        sources.append(&mut found);
        dirs.push(dir);
    }
    sources.sort();
    if sources.is_empty() {
        notes.push("no .erl sources were staged".to_string());
        return skipped_with(p, kind, "sources-missing", &notes);
    }
    notes.push(format!(
        "staged {} .erl sources from {}/{{{}}}",
        sources.len(),
        repo_root.display(),
        dirs.join(",")
    ));

    let ebin = work.join("ebin");
    if let Err(e) = std::fs::create_dir_all(&ebin) {
        return failed_with(p, kind, format!("scratch-dir-failed: {e}"), &notes);
    }
    let mut compile_cmd = Command::new(&erlc);
    compile_cmd
        .arg("-o")
        .arg(&ebin)
        .args(p.erlc_flags)
        .args(&sources);
    let compiled = match proc::run(&mut compile_cmd) {
        Ok(out) => out,
        Err(e) => return failed_with(p, kind, format!("erlc-spawn-failed: {e}"), &notes),
    };
    // A warning is not a failure (jsone's deprecation warning lands here); keep it verbatim.
    let compile_stderr = compiled.stderr.trim();
    if !compile_stderr.is_empty() {
        notes.push(format!("erlc stderr: {compile_stderr}"));
    }
    let missing_beams: Vec<String> = sources
        .iter()
        .filter_map(|src| src.file_stem().map(|stem| stem.to_string_lossy().into_owned()))
        .filter(|module| !ebin.join(format!("{module}.beam")).is_file())
        .collect();
    if let Err(reason) = compile_verdict(compiled.success, &missing_beams) {
        notes.push(format!(
            "compile: exit {:?}, {} source(s), {} missing .beam",
            compiled.exit_code,
            sources.len(),
            missing_beams.len()
        ));
        return failed_with(p, kind, reason, &notes);
    }

    // Run the identical argv `runs` times; every round must print the same intended count.
    let argv = argv_for(kind, &ebin, p.eunit_modules);
    let runs = runs_for(ctx.profile);
    let mut walls = Vec::with_capacity(runs);
    let mut peak: Option<u64> = None;
    let mut summaries: Vec<(usize, String)> = Vec::new();
    for round in 1..=runs {
        let mut cmd = Command::new(program);
        cmd.args(&argv);
        let out = match proc::run(&mut cmd) {
            Ok(out) => out,
            Err(e) => return failed_with(p, kind, format!("run-spawn-failed: {e}"), &notes),
        };
        if let Some(reason) = capability_gap(kind, out.exit_code, &out.stderr) {
            notes.push(format!("beam_rt refused the suite: unsupported: {reason}"));
            notes.push(
                "this regresses the verified-supported status of this project: the same suite \
                 completes on the system runtime"
                    .to_string(),
            );
            return skipped_with(p, kind, "runtime-unsupported", &notes);
        }
        let summary = parse_summary(&out.stdout);
        if !out.success {
            notes.push(format!(
                "round {round}/{runs}: exit {:?}, stderr: {}",
                out.exit_code,
                out.stderr.trim()
            ));
            if let Some((_, line)) = &summary {
                notes.push(format!("round {round}/{runs} eunit: {line}"));
            }
            return failed_with(p, kind, "eunit-failed", &notes);
        }
        walls.push(out.wall_ms);
        peak = peak.max(out.peak_memory_bytes);
        if let Some(s) = summary {
            summaries.push(s);
        }
    }

    let observed = match count_verdict(p.expected_tests, &summaries) {
        CountVerdict::Agrees(count) => count,
        CountVerdict::NoSummary => {
            notes.push(format!(
                "none of {runs} eunit run(s) printed a test summary (stdout was consumed by the \
                 suite; a missing summary is a failure, never a pass)"
            ));
            return failed_with(p, kind, "no-eunit-summary", &notes);
        }
        CountVerdict::RoundsDisagree { first, other, line } => {
            notes.push(format!(
                "intended {expected} tests; rounds disagree: {first} vs {other} ({line})",
                expected = p.expected_tests
            ));
            return failed_with(p, kind, "unexpected-test-count", &notes);
        }
        CountVerdict::Unexpected {
            intended,
            observed,
            line,
        } => {
            notes.push(format!(
                "intended {intended} tests, observed {observed} ({line})"
            ));
            return failed_with(p, kind, "unexpected-test-count", &notes);
        }
    };

    // Both sides must report the same count; the later side is the one that fails.
    let observation = SideObservation {
        kind,
        count: observed,
        summary: summaries
            .first()
            .map(|(_, line)| line.clone())
            .unwrap_or_default(),
    };
    if let LedgerVerdict::Disagrees { this, other } = lock(ledger).record(observation) {
        notes.push(format!(
            "{} reported {} tests ({}), {} reported {} tests ({})",
            other.kind.as_str(),
            other.count,
            other.summary,
            this.kind.as_str(),
            this.count,
            this.summary
        ));
        return failed_with(
            p,
            kind,
            format!("tests-count-disagrees-with-{}", other.kind.as_str()),
            &notes,
        );
    }

    let mut result = WorkloadResult::new(p.id, kind, p.category, p.description);
    result
        .metrics
        .push(Metric::samples("wall_ms", "ms", &walls));
    result
        .metrics
        .push(Metric::scalar("compile_ms", "ms", compiled.wall_ms));
    result
        .metrics
        .push(Metric::scalar("tests_passed", "tests", observed as f64));
    result.peak_memory_bytes = peak;
    notes.push(format!(
        "eunit: {}",
        summaries
            .first()
            .map(|(_, line)| line.as_str())
            .unwrap_or("(no summary)")
    ));
    notes.push(format!("rounds: {runs} whole-process runs of the same argv"));
    notes.push(format!(
        "per-round wall_ms: [{}]",
        walls
            .iter()
            .map(|w| format!("{w:.3}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    notes.push(format!(
        "peak_memory_bytes is the max observed across the {runs} run(s); compile stage peak: {:?}",
        compiled.peak_memory_bytes
    ));
    result.notes.extend(notes);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Status;

    /// A `Project` for the negative-path tests: a repo root that cannot exist.
    const ABSENT: Project = Project {
        id: "eunit_probe_absent_xyzzy",
        default_repo: "/nonexistent/eunit-project-xyzzy",
        erlc_flags: &[],
        src_dirs: &["src"],
        test_dirs: &["test"],
        eunit_modules: &["none"],
        expected_tests: 0,
        category: Category::Compute,
        description: "test-only project with no checkout",
    };

    #[test]
    fn project_consts_are_pinned_field_by_field() {
        assert_eq!(JSONE.id, "jsone");
        assert_eq!(JSONE.default_repo, "/home/bill/Source/jsone");
        assert_eq!(
            JSONE.erlc_flags,
            &["-DTEST", "-DMAP_ITER_ORDERED", "-DTIME_MODULE=test_time_module"]
        );
        assert_eq!(JSONE.src_dirs, &["src"]);
        assert_eq!(JSONE.test_dirs, &["test"]);
        assert_eq!(
            JSONE.eunit_modules,
            &["jsone_decode_tests", "jsone_encode_tests", "jsone_inet_tests"]
        );
        assert_eq!(JSONE.expected_tests, 90);
        assert_eq!(JSONE.category, Category::Compute);
        assert!(!JSONE.description.is_empty());

        assert_eq!(BBMUSTACHE.id, "bbmustache");
        assert_eq!(BBMUSTACHE.default_repo, "/home/bill/Source/bbmustache");
        assert_eq!(
            BBMUSTACHE.erlc_flags,
            &["+export_all", "-Dnamespaced_types", "-Dunicode_supported"]
        );
        assert_eq!(BBMUSTACHE.src_dirs, &["src"]);
        assert_eq!(BBMUSTACHE.test_dirs, &["test"]);
        assert_eq!(BBMUSTACHE.eunit_modules, &["bbmustache_tests"]);
        assert_eq!(BBMUSTACHE.expected_tests, 79);
        assert_eq!(BBMUSTACHE.category, Category::Compute);
        assert!(!BBMUSTACHE.description.is_empty());

        assert_ne!(JSONE.id, BBMUSTACHE.id);
    }

    /// The property this whole design rests on: both runtimes get the same argv, so a
    /// difference in behaviour is a runtime difference.
    #[test]
    fn both_runtimes_get_the_identical_argv() {
        let ebin = Path::new("/tmp/bench-ebin");
        for p in [&JSONE, &BBMUSTACHE] {
            let sys = argv_for(RuntimeKind::Sys, ebin, p.eunit_modules);
            let rust = argv_for(RuntimeKind::Rust, ebin, p.eunit_modules);
            assert_eq!(sys, rust, "{} must not diverge per runtime", p.id);

            assert_eq!(sys[0], "-noshell");
            assert_eq!(sys[1], "-pa");
            assert_eq!(sys[2], "/tmp/bench-ebin");
            assert_eq!(sys[3], "-eval");
            let expr = &sys[4];
            assert!(expr.starts_with("case eunit:test(["), "{expr}");
            assert!(expr.contains("ok -> halt(0)"), "{expr}");
            assert!(expr.contains("_ -> halt(1)"), "{expr}");
            for module in p.eunit_modules {
                assert!(expr.contains(module), "{expr} must name {module}");
            }
        }
        // The oracle form of the exact expression both sides are given.
        assert_eq!(
            eunit_expr(&["a", "b"]),
            "case eunit:test([a,b]) of ok -> halt(0); _ -> halt(1) end."
        );
    }

    #[test]
    fn summary_parser_recognises_every_eunit_shape() {
        // The passing shape, with eunit's two-space indent.
        assert_eq!(
            parse_summary("  All 90 tests passed.\n"),
            Some((90, "All 90 tests passed.".to_string()))
        );
        // Trailing blank lines and surrounding noise must not matter.
        assert_eq!(
            parse_summary(".\n..\n  All 79 tests passed.\n\n"),
            Some((79, "All 79 tests passed.".to_string()))
        );
        // The failing shape: total = failed + skipped + passed.
        assert_eq!(
            parse_summary("  Failed: 1.  Skipped: 0.  Passed: 2.\n"),
            Some((3, "Failed: 1.  Skipped: 0.  Passed: 2.".to_string()))
        );
        assert_eq!(
            parse_summary("  Failed: 1.  Skipped: 2.  Passed: 3.\n"),
            Some((6, "Failed: 1.  Skipped: 2.  Passed: 3.".to_string()))
        );
        // The `There were N tests, M failures` shape.
        assert_eq!(
            parse_summary("There were 3 tests, 1 failures\n"),
            Some((3, "There were 3 tests, 1 failures".to_string()))
        );
        // A log with no summary is `None`, never an invented count.
        assert_eq!(parse_summary(""), None);
        assert_eq!(parse_summary("===> Compiling jsone\n"), None);
        assert_eq!(parse_summary("All tests passed.\n"), None);
    }

    #[test]
    fn a_count_that_disagrees_with_the_intended_one_fails_without_a_metric() {
        let ok = [(90, "All 90 tests passed.".to_string())];
        assert_eq!(count_verdict(90, &ok), CountVerdict::Agrees(90));

        let short = [(89, "All 89 tests passed.".to_string())];
        assert_eq!(
            count_verdict(90, &short),
            CountVerdict::Unexpected {
                intended: 90,
                observed: 89,
                line: "All 89 tests passed.".to_string(),
            }
        );

        assert_eq!(count_verdict(90, &[]), CountVerdict::NoSummary);

        let disagreeing = [
            (90, "All 90 tests passed.".to_string()),
            (91, "All 91 tests passed.".to_string()),
        ];
        assert_eq!(
            count_verdict(90, &disagreeing),
            CountVerdict::RoundsDisagree {
                first: 90,
                other: 91,
                line: "All 91 tests passed.".to_string(),
            }
        );

        // Every non-agreeing verdict is a failure reason, never a skip.
        for summaries in [short.to_vec(), disagreeing.to_vec(), Vec::new()] {
            assert!(!matches!(count_verdict(90, &summaries), CountVerdict::Agrees(_)));
        }
    }

    #[test]
    fn probe_is_honest_about_a_missing_repo_and_a_missing_erlc() {
        let erlc = slice::resolve_erlc();
        assert_eq!(
            probe_of(Path::new(ABSENT.default_repo), erlc.as_deref()),
            Probe::skipped("repo-missing")
        );
        let existing = std::env::temp_dir();
        assert_eq!(
            probe_of(&existing, None),
            Probe::skipped("erlc-absent")
        );
        if erlc.is_some() {
            assert_eq!(probe_of(&existing, erlc.as_deref()), Probe::Ready);
        }
        // The real probe must agree with the pure decision.
        assert_eq!(probe(&ABSENT), Probe::skipped("repo-missing"));
    }

    /// A capability gap is a skip; every other non-zero exit is a failure. Only the Rust
    /// runtime, only exit `3`, and only with the documented stderr line.
    #[test]
    fn a_gap_exit_is_a_skip_and_anything_else_is_a_failure() {
        let gap = "beam_rt: unsupported: unsupported_opcode: 42\n";
        assert_eq!(
            capability_gap(RuntimeKind::Rust, Some(3), gap).as_deref(),
            Some("unsupported_opcode: 42")
        );
        // Not the Rust runtime: the oracle exiting 3 is just a failing run.
        assert_eq!(capability_gap(RuntimeKind::Sys, Some(3), gap), None);
        // Not exit 3.
        assert_eq!(capability_gap(RuntimeKind::Rust, Some(1), gap), None);
        // Exit 3 without the documented line is an unexplained failure, not a gap.
        assert_eq!(
            capability_gap(RuntimeKind::Rust, Some(3), "some other crash\n"),
            None
        );
        assert_eq!(capability_gap(RuntimeKind::Rust, None, gap), None);
    }

    #[test]
    fn a_compile_warning_is_not_a_failure_and_a_missing_beam_is() {
        // jsone's deprecation warning arrives on stderr while the compile exits 0.
        assert_eq!(compile_verdict(true, &[]), Ok(()));
        assert!(compile_verdict(false, &[]).is_err());
        let err = compile_verdict(true, &["jsone_decode_tests".to_string()]).unwrap_err();
        assert!(err.starts_with("module-compile-failed"), "{err}");
        assert!(err.contains("jsone_decode_tests"), "{err}");
    }

    /// End to end: a checkout that cannot compile is a `failed(module-compile-failed)` — never
    /// a skip, and with no metric published.
    ///
    /// The project id is unique to this test, so its `BENCH_<ID>_REPO` override cannot be seen
    /// by any other test (the crate's `--repo` mechanism is exactly this env var).
    #[test]
    fn a_checkout_that_cannot_compile_fails_instead_of_skipping() {
        use crate::runtime::Runtime;
        use crate::workloads::{repo_env_var, set_repo_override};

        if slice::resolve_erlc().is_none() {
            return;
        }
        const BROKEN: Project = Project {
            id: "eunit_broken_src_xyzzy",
            default_repo: "/nonexistent/eunit-broken-xyzzy",
            erlc_flags: &[],
            src_dirs: &["src"],
            test_dirs: &[],
            eunit_modules: &["broken"],
            expected_tests: 0,
            category: Category::Compute,
            description: "test-only project whose source does not compile",
        };
        let root = std::env::temp_dir().join(format!(
            "bench-eunit-broken-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let src = root.join("src");
        std::fs::create_dir_all(&src).expect("create fixture src");
        std::fs::write(src.join("broken.erl"), "-module(broken).\nf() -> .\n").expect("write");

        set_repo_override(BROKEN.id, &root);
        let runtime = Runtime {
            kind: RuntimeKind::Rust,
            program: Some(PathBuf::from("/bin/true")),
        };
        let out_dir = std::env::temp_dir();
        let result = run(
            &BROKEN,
            &RunContext::new(&runtime, Profile::Quick, &out_dir),
            &Mutex::new(SideLedger::new()),
        );
        std::env::remove_var(repo_env_var(BROKEN.id));
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(result.status, Status::Failed, "notes: {:?}", result.notes);
        let reason = result.reason.as_deref().unwrap_or_default();
        assert!(reason.starts_with("module-compile-failed"), "{reason}");
        assert!(result.metrics.is_empty(), "a failed compile publishes no metric");
        assert!(
            result.notes.iter().any(|n| n.contains("erlc stderr")),
            "the compiler's own stderr must be quoted: {:?}",
            result.notes
        );
    }

    #[test]
    fn make_tree_writable_opens_a_read_only_tree_and_reports_a_missing_one() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "bench-eunit-chmod-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let nested = root.join("src");
        std::fs::create_dir_all(&nested).expect("create fixture tree");
        let file = nested.join("a.erl");
        std::fs::write(&file, "-module(a).\n").expect("write fixture");

        fn mode(path: &Path) -> u32 {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path).expect("stat").permissions().mode()
        }
        // Directories need their execute/search bit to be traversable at all, so the
        // read-only fixture uses 0o555 for them and 0o444 for the file.
        for (path, read_only) in [(&file, 0o444), (&nested, 0o555), (&root, 0o555)] {
            let mut permissions = std::fs::metadata(path).expect("stat").permissions();
            permissions.set_mode(read_only);
            std::fs::set_permissions(path, permissions).expect("chmod read-only");
        }
        assert_eq!(mode(&file) & 0o200, 0);

        let changed = make_tree_writable(&root).expect("the chmod helper must open the tree");
        assert!(changed >= 3, "expected the file and both dirs to change: {changed}");
        assert_ne!(mode(&file) & 0o200, 0, "the .erl must be writable");
        assert_ne!(mode(&nested) & 0o200, 0);

        // A missing tree is reported, not swallowed.
        let missing = root.join("does-not-exist");
        assert!(make_tree_writable(&missing).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn runs_follow_the_profile() {
        assert_eq!(runs_for(Profile::Quick), 3);
        assert_eq!(runs_for(Profile::Full), 10);
    }

    /// End-to-end smoke test against the real checkouts and the real oracle: the whole
    /// machinery must produce an `ok` row with the verified count. Self-skipping, so a
    /// checkout-less or oracle-less environment stays green.
    #[test]
    fn the_real_suites_run_under_the_oracle_when_present() {
        if slice::resolve_erlc().is_none() {
            return;
        }
        let Ok(runtime) = crate::runtime::resolve_oracle() else {
            return;
        };
        if !runtime.self_check() {
            return;
        }
        let out_dir = std::env::temp_dir();
        let ctx = RunContext::new(&runtime, Profile::Quick, &out_dir);
        for p in [&JSONE, &BBMUSTACHE] {
            if !repo(p).is_dir() {
                continue;
            }
            let ledger = Mutex::new(SideLedger::new());
            let result = run(p, &ctx, &ledger);
            assert_eq!(
                result.status,
                Status::Ok,
                "{} must run under the oracle: {:?} {}",
                p.id,
                result.reason,
                result.notes.join(" | ")
            );
            let tests = result
                .metrics
                .iter()
                .find(|m| m.name == "tests_passed")
                .and_then(Metric::representative_value)
                .expect("a verified tests_passed metric");
            assert_eq!(tests as usize, p.expected_tests, "{} count", p.id);
            assert!(result
                .notes
                .iter()
                .any(|n| n.starts_with("eunit: All ")));
            assert_eq!(ledger.lock().expect("ledger").sides().len(), 1);
        }
    }
}
