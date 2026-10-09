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

//! `plb` — the Programming-Language-Benchmarks ("Benchmarks Game") compute kernels.
//!
//! Stages the checkout into a private scratch copy under `$TMPDIR` (the source repo is
//! configured **read-only**) and drives the repo's `dotnet`-based tool there. The tool has
//! to be run in **two stages** (`bench/tool/Program.cs`):
//!
//! 1. `--task build` compiles each kernel into a self-contained executable (for the
//!    `elixir` environment: a `mix escript.build` per problem, landing in
//!    `bench/build/<build-id>/`); and
//! 2. `--task bench` then *measures* those executables, writing one result JSON per test
//!    under `bench/build/_results/<lang>/` (`timeMS`, `memBytes`).
//!
//! Stage 2 reads stage 1's output, so it fails with `DirectoryNotFoundException` unless
//! the build ran first — both stages therefore run, in order, in the **same** scratch copy,
//! which is where the results are read from (never from the read-only source checkout).
//!
//! Every location the toolchain wants to write is redirected into that scratch copy
//! (`HOME`, `DOTNET_CLI_HOME`, `NUGET_PACKAGES`, `MIX_HOME`, `TMPDIR`): the sandbox `$HOME`
//! is read-only, so both the .NET SDK/NuGet first-run state and `mix local.hex`'s hex
//! archive must land somewhere writable that RAII removes afterwards. The elixir
//! environment is declared to run inside a `podman`/`docker` container, which this host
//! cannot start, so both stages are driven with `--no-docker`: the local `mix` toolchain
//! builds exactly the same escripts without a container.
//!
//! Repo: `$BENCH_PLB_REPO`, else `/home/bill/Source/Programming-Language-Benchmarks`.
//! `dotnet` is a hard prerequisite — when it is absent the workload is `skipped`
//! (`dotnet-absent`), never a fabricated number. The opt-in `nix shell` fallback (and its
//! `$BENCH_TRY_NIX` switch) is shared with the other tool-driven workloads; see
//! [`crate::workloads::tool`]. It too is validated with a bounded `--version` probe and,
//! if it fails, the honest `dotnet-absent` skip is kept.
//!
//! A run that yields **no** result JSON is recorded `failed` (`plb-tool-failed` when the
//! tool exited non-zero, `plb-no-results` when it exited `0` while benching nothing) —
//! never `ok`, because the only other number on such a row is the tool's own wall time,
//! which is not a benchmark result. A **build** stage that fails is recorded
//! `failed(plb-build-failed)` instead, so the row names the stage that broke; every failure
//! carries trimmed diagnostics in `notes` (the fatal .NET exception from stderr plus the
//! first error-looking line of the tool's own stdout log — or its tail when the log looks
//! clean), so the row says *which* command broke, not merely that something did.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::metrics::{Category, Metric, Probe, Runtimes, Status};
use crate::proc::{self, Completed};
use crate::report::WorkloadResult;
use crate::scratch::{self, ScratchDir};
use crate::workload::{RunContext, Workload};
use crate::workloads::{collect_files, resolve_repo, tool};

/// The documented default checkout path.
const DEFAULT_REPO: &str = "/home/bill/Source/Programming-Language-Benchmarks";

/// The external tool this workload drives.
const TOOL: &str = "dotnet";

/// The `nixpkgs` attribute that supplies [`TOOL`] for the opt-in `nix` fallback.
const TOOL_NIX_ATTR: &str = "dotnet-sdk";

/// The honest skip reason reported when [`TOOL`] cannot be resolved.
const TOOL_ABSENT: &str = "dotnet-absent";

/// The honest failure reason when the tool's **build** stage (stage 1) did not succeed.
const BUILD_FAILED: &str = "plb-build-failed";

/// The problem language the tool is driven with (`--langs`; matches `bench_elixir.yaml`).
const LANG: &str = "elixir";

/// The tool's stage that compiles each kernel into a runnable executable.
const TASK_BUILD: &str = "build";

/// The tool's stage that measures the executables [`TASK_BUILD`] produced.
const TASK_BENCH: &str = "bench";

/// How many characters of captured output a diagnostic `notes` entry may carry.
const NOTE_CHARS: usize = 400;

/// `.NET` CLI switches that keep the SDK quiet and keep its first-run experience from
/// writing anywhere outside the redirected home.
const DOTNET_FLAGS: &[(&str, &str)] = &[
    ("DOTNET_NOLOGO", "1"),
    ("DOTNET_CLI_TELEMETRY_OPTOUT", "1"),
    ("DOTNET_SKIP_FIRST_TIME_EXPERIENCE", "1"),
];

/// The PLB compute-kernel workload.
pub struct Plb;

fn repo() -> PathBuf {
    resolve_repo("plb", DEFAULT_REPO)
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
        if !tool::available(TOOL) {
            return Probe::skipped(TOOL_ABSENT);
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
        let Some(base) = tool::command_for(TOOL, TOOL_NIX_ATTR) else {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                TOOL_ABSENT,
            );
        };

        // Stage the read-only checkout into a private scratch copy and run there, so the
        // `dotnet` build never writes build artifacts (or `_results`) into the source repo.
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
        let work = scratch.path().join(self.id());
        if let Err(e) = scratch::copy_tree(&r, &work, tool::REPO_SKIP_NAMES) {
            return WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                format!("scratch-copy-failed: {e}"),
            );
        }

        let bench_dir = work.join("bench");
        if !bench_dir.is_dir() {
            return WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "bench-dir-missing",
            );
        }

        // A writable toolchain home and temp dir, inside the scratch copy: `$HOME` is
        // read-only on this host, so the .NET SDK/NuGet state and `mix`'s hex archive must
        // be redirected here (and the tool's per-build `$TMPDIR` staging with them).
        let home = scratch.path().join("home");
        let tmp = scratch.path().join("tmp");
        for dir in [&home, &tmp] {
            if let Err(e) = std::fs::create_dir_all(dir) {
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    format!("scratch-dir-failed: {e}"),
                );
            }
        }

        // Stage 1 — build. This must precede the bench stage and share its scratch copy:
        // `--task bench` only *measures* the executables this stage produced.
        let build = match run_stage(&base, &bench_dir, TASK_BUILD, &home, &tmp) {
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
        if !build.success {
            // No measurement exists, so there is nothing to report but *why*: name the
            // failing stage and carry the toolchain's own diagnostic.
            let mut result = WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                BUILD_FAILED,
            );
            result.notes.push(format!(
                "build stage (dotnet --task build) exit {:?}",
                build.exit_code
            ));
            failure_notes(&build, &mut result.notes);
            return result;
        }

        // Stage 2 — bench the executables stage 1 produced.
        let out = match run_stage(&base, &bench_dir, TASK_BENCH, &home, &tmp) {
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
        result.notes.push(format!(
            "build stage ok (dotnet --task build took {:.0} ms)",
            build.wall_ms
        ));

        // Read the result JSON from the *scratch* copy, never the (read-only) source repo.
        let (times, mems) = read_results(&bench_dir.join("build/_results"));
        if times.is_empty() {
            // No measurement was produced, so the `wall_ms` recorded above is the *tool's*
            // own runtime and not a benchmark result: the row must never read as `ok`.
            // A non-zero exit is the tool failing; a zero exit with no results means it
            // benched nothing (a silent no-op), which is just as honest a failure.
            result.status = Status::Failed;
            result.reason = Some(no_results_reason(out.success).to_string());
            result.notes.push(format!(
                "no result JSON found (bench stage, dotnet exit {:?})",
                out.exit_code
            ));
            failure_notes(&out, &mut result.notes);
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

/// The `dotnet run …` argument list for one tool stage.
///
/// `--project tool` selects `bench/tool/BenchTool.csproj`, so the working directory must be
/// the staged checkout's `bench/`; everything after `--` is BenchTool's own argv. The
/// `--no-docker` switch is deliberate: `bench_elixir.yaml` declares a `podman` container
/// for the elixir environment, which this host cannot start, while the local `mix`
/// toolchain builds the same escripts without one.
fn stage_args(task: &str) -> Vec<String> {
    [
        "run",
        "-c",
        "Release",
        "--project",
        "tool",
        "--",
        "--task",
        task,
        "--langs",
        LANG,
        "--no-docker",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// A fresh command reproducing `base`'s program and *prefix* arguments, with the argv for
/// `task` appended.
///
/// [`tool::command_for`] may hand back a `nix shell nixpkgs#dotnet-sdk --command dotnet`
/// shim (four prefix arguments) instead of a bare binary, so each stage is rebuilt from
/// `get_program()`/`get_args()` rather than reused: a [`Command`] only ever *accumulates*
/// arguments, so running two stages off one instance would append two `--task` flags.
fn rebase(base: &Command, task: &str) -> Command {
    let mut cmd = Command::new(base.get_program());
    cmd.args(base.get_args());
    cmd.args(stage_args(task));
    cmd
}

/// Every writable location the toolchain needs, redirected into the scratch copy.
///
/// `$HOME` (read-only on this host), the .NET SDK's `DOTNET_CLI_HOME`, NuGet's package
/// cache, Elixir's `MIX_HOME` (where `mix local.hex` installs the hex archive) and the
/// tool's own `$TMPDIR`-rooted per-build staging folder all default to locations outside
/// the scratch tree; pointing them inside it is what lets the bootstrap succeed without
/// touching anything the workload does not own.
fn toolchain_env(home: &Path, tmp: &Path) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("HOME", home.to_path_buf()),
        ("DOTNET_CLI_HOME", home.to_path_buf()),
        ("NUGET_PACKAGES", home.join(".nuget/packages")),
        ("MIX_HOME", home.join(".mix")),
        ("TMPDIR", tmp.to_path_buf()),
    ]
}

/// Applies the redirected toolchain environment and the quiet-SDK switches to `cmd`.
fn apply_toolchain_env(cmd: &mut Command, home: &Path, tmp: &Path) {
    for (key, value) in toolchain_env(home, tmp) {
        cmd.env(key, value);
    }
    for (key, value) in DOTNET_FLAGS {
        cmd.env(key, value);
    }
}

/// Runs one BenchTool stage inside `bench_dir` with the redirected environment.
fn run_stage(
    base: &Command,
    bench_dir: &Path,
    task: &str,
    home: &Path,
    tmp: &Path,
) -> std::io::Result<Completed> {
    let mut cmd = rebase(base, task);
    cmd.current_dir(bench_dir);
    apply_toolchain_env(&mut cmd, home, tmp);
    proc::run(&mut cmd)
}

/// The honest failure reason when the tool produced **no** benchmark results.
///
/// A non-zero exit is the tool failing outright; a zero exit that still yields no result
/// JSON is a silent no-op (e.g. a `dotnet` build that "succeeded" without benching
/// anything). Either way there is no measurement, so the row is `failed` — never `ok`.
fn no_results_reason(success: bool) -> &'static str {
    if success {
        "plb-no-results"
    } else {
        "plb-tool-failed"
    }
}

/// Collapses `text` to a single line (every whitespace run becomes one space).
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A bounded single-line excerpt of captured output, for the result's `notes`.
///
/// A fatal .NET exception names the problem on its *first* stderr line, whereas the tool's
/// chronological stdout log puts the interesting detail *last*, so the caller chooses which
/// end to keep. Truncation counts characters (not bytes) so a multi-byte log line can never
/// split a `char`.
fn excerpt(text: &str, max: usize, from_tail: bool) -> String {
    let line = collapse(text);
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= max {
        return line;
    }
    if from_tail {
        let mut out = String::from("…");
        out.extend(&chars[chars.len() - max..]);
        out
    } else {
        let mut out: String = chars[..max].iter().collect();
        out.push('…');
        out
    }
}

/// Markers that identify the explanatory line in the tool's own log.
const DIAGNOSTIC_MARKERS: &[&str] = &[
    "error",
    "exception",
    "failure",
    "failed",
    "cannot",
    "not found",
    "no such",
];

/// The first line of the tool's log that looks like a diagnosis of the failure.
///
/// The log is chronological, so the *earliest* error-ish line is the closest thing to the
/// root cause (for a broken toolchain: the `mix` invocation and what it printed); the last
/// lines are the aggregate exception's stack trace, which says only *that* something broke.
fn first_diagnostic_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| {
            let lower = line.to_ascii_lowercase();
            DIAGNOSTIC_MARKERS.iter().any(|marker| lower.contains(marker))
        })
        .map(collapse)
}

/// The diagnostic `notes` for a failed (or result-less) tool stage.
///
/// The two streams carry different evidence and both are worth keeping: the fatal .NET
/// exception lands on stderr and *names* the failure, while the tool's own chronological
/// log on stdout shows *which* command it ran and what that command printed (the exception
/// alone does not). Each note is bounded, and an empty stream contributes nothing.
fn failure_notes(out: &Completed, notes: &mut Vec<String>) {
    if !out.stderr.trim().is_empty() {
        notes.push(format!(
            "stderr: {}",
            excerpt(&out.stderr, NOTE_CHARS, false)
        ));
    }
    if !out.stdout.trim().is_empty() {
        let log = match first_diagnostic_line(&out.stdout) {
            Some(line) => excerpt(&line, NOTE_CHARS, false),
            // No line looked like a diagnosis: the log tail is the only evidence left.
            None => excerpt(&out.stdout, NOTE_CHARS, true),
        };
        notes.push(format!("stdout: {log}"));
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

    /// The arguments of `cmd`, as owned strings.
    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn completed(stdout: &str, stderr: &str, success: bool) -> Completed {
        Completed {
            exit_code: Some(if success { 0 } else { 1 }),
            success,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            wall_ms: 1.0,
            peak_memory_bytes: None,
        }
    }

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
        if repo().is_dir() && !tool::available(TOOL) {
            assert_eq!(p.reason(), Some(TOOL_ABSENT));
        }
    }

    /// The tool needs a build stage *and* a bench stage, in that order, and both must be
    /// told the language and to stay out of docker.
    #[test]
    fn stage_argv_pins_the_two_stage_flow() {
        assert_eq!(
            stage_args(TASK_BUILD),
            vec![
                "run",
                "-c",
                "Release",
                "--project",
                "tool",
                "--",
                "--task",
                "build",
                "--langs",
                "elixir",
                "--no-docker"
            ]
        );
        assert_eq!(
            stage_args(TASK_BENCH),
            vec![
                "run",
                "-c",
                "Release",
                "--project",
                "tool",
                "--",
                "--task",
                "bench",
                "--langs",
                "elixir",
                "--no-docker"
            ]
        );
        // Distinct tasks: a shared argv would run the same stage twice.
        assert_ne!(stage_args(TASK_BUILD), stage_args(TASK_BENCH));
        assert!(stage_args(TASK_BUILD).contains(&"--no-docker".to_string()));
    }

    /// Each stage starts from a clean argv: the `nix shell` shim's prefix survives, and the
    /// previous stage's arguments do not.
    #[test]
    fn rebase_keeps_a_shim_prefix_and_starts_from_a_clean_argv() {
        let shim = tool::nix_shell(Path::new("/usr/bin/nix"), "dotnet", "dotnet-sdk");
        let build = rebase(&shim, TASK_BUILD);
        assert_eq!(build.get_program().to_string_lossy(), "/usr/bin/nix");
        let args = args_of(&build);
        assert_eq!(
            args[..4],
            ["shell", "nixpkgs#dotnet-sdk", "--command", "dotnet"]
        );
        assert_eq!(args[4..], stage_args(TASK_BUILD)[..]);
        assert_eq!(args.iter().filter(|a| *a == "--task").count(), 1);

        // A bare binary (the `PATH` case) carries no prefix at all.
        let bare = rebase(&Command::new("/usr/bin/dotnet"), TASK_BENCH);
        assert_eq!(bare.get_program().to_string_lossy(), "/usr/bin/dotnet");
        assert_eq!(args_of(&bare), stage_args(TASK_BENCH));
    }

    /// Every writable location the bootstrap needs lives inside the scratch tree — the
    /// sandbox `$HOME` is read-only, so a single one left pointing outside would break it.
    #[test]
    fn toolchain_env_points_every_writable_location_into_the_scratch() {
        let home = Path::new("/scratch/plb/home");
        let tmp = Path::new("/scratch/plb/tmp");
        let env = toolchain_env(home, tmp);
        let value = |key: &str| {
            env.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
        };

        assert_eq!(value("HOME"), Some(home.to_path_buf()));
        assert_eq!(value("DOTNET_CLI_HOME"), Some(home.to_path_buf()));
        assert_eq!(value("NUGET_PACKAGES"), Some(home.join(".nuget/packages")));
        assert_eq!(value("MIX_HOME"), Some(home.join(".mix")));
        assert_eq!(value("TMPDIR"), Some(tmp.to_path_buf()));
        for (key, path) in &env {
            assert!(
                path.starts_with("/scratch/plb"),
                "{key}={path:?} escapes the scratch tree"
            );
        }
    }

    #[test]
    fn apply_toolchain_env_sets_the_redirected_home_on_the_command() {
        let home = Path::new("/scratch/plb/home");
        let tmp = Path::new("/scratch/plb/tmp");
        let mut cmd = Command::new("/usr/bin/dotnet");
        apply_toolchain_env(&mut cmd, home, tmp);

        let envs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(envs.contains(&("HOME".to_string(), Some("/scratch/plb/home".to_string()))));
        assert!(envs.contains(&("TMPDIR".to_string(), Some("/scratch/plb/tmp".to_string()))));
        assert!(envs.contains(&("MIX_HOME".to_string(), Some("/scratch/plb/home/.mix".to_string()))));
        assert!(envs.contains(&("DOTNET_NOLOGO".to_string(), Some("1".to_string()))));
        assert_eq!(
            envs.len(),
            toolchain_env(home, tmp).len() + DOTNET_FLAGS.len()
        );
    }

    #[test]
    fn excerpts_are_single_line_and_bounded() {
        assert_eq!(excerpt("a\n\n b\tc ", 40, false), "a b c");
        assert_eq!(excerpt("hello", 40, true), "hello");
        // Truncated from the requested end, counting characters (not bytes).
        assert_eq!(excerpt("αααααααα", 3, false), "ααα…");
        assert_eq!(excerpt("αααααααα", 3, true), "…ααα");
        assert_eq!(excerpt("", 4, false), "");
        assert_eq!(excerpt("", 4, true), "");
    }

    #[test]
    fn failure_notes_carry_both_streams_bounded() {
        // stderr names the failure (kept from its head) and stdout is searched for the line
        // that explains it, so a failed run carries evidence from each.
        let both = completed(
            "starting…\nCommand:: mix local.hex --force\nmix: simulated failure\n   at Boom()",
            "Unhandled\nexception: bad",
            false,
        );
        let mut notes = Vec::new();
        failure_notes(&both, &mut notes);
        assert_eq!(
            notes,
            vec![
                "stderr: Unhandled exception: bad",
                "stdout: mix: simulated failure",
            ]
        );

        // Without a diagnostic-looking line the log tail is the only evidence left.
        let tail_only = completed("first line\nlast line", "   \n", true);
        let mut notes = Vec::new();
        failure_notes(&tail_only, &mut notes);
        assert_eq!(notes, vec!["stdout: first line last line"]);

        let mut notes = Vec::new();
        failure_notes(&completed("", "", true), &mut notes);
        assert!(notes.is_empty());

        // Both notes are bounded to one line each, however noisy the tool is.
        let noisy = completed(&"x".repeat(5_000), &"y".repeat(5_000), false);
        let mut notes = Vec::new();
        failure_notes(&noisy, &mut notes);
        assert_eq!(notes.len(), 2);
        for note in &notes {
            // `«stream»: ` + at most `NOTE_CHARS` characters + the truncation ellipsis.
            assert_eq!(note.chars().count(), "stderr: ".len() + NOTE_CHARS + 1);
        }
        // The head is kept for stderr (where .NET states the exception); a stdout log with
        // no diagnostic-looking line keeps its tail (its chronological end).
        assert!(notes[0].starts_with("stderr: yyy") && notes[0].ends_with('…'));
        assert!(notes[1].starts_with("stdout: …xxx") && notes[1].ends_with('x'));
    }

    #[test]
    fn the_first_diagnostic_line_is_picked_from_a_noisy_log() {
        let log = "An issue was encountered verifying workloads.\n\
                   Command:: mix local.hex --force\n\
                   mix: simulated failure (toolchain broken)\n\
                   Command:: mv app out\n\
                   mv: cannot stat 'app': No such file or directory\n\
                   System.IO.DirectoryNotFoundException: /tmp/build\n   at X()";
        assert_eq!(
            first_diagnostic_line(log),
            Some("mix: simulated failure (toolchain broken)".to_string()),
            "the *first* error-ish line is the closest thing to the root cause"
        );
        // Markers are matched case-insensitively …
        assert_eq!(
            first_diagnostic_line("Nothing here\nNO SUCH FILE found"),
            Some("NO SUCH FILE found".to_string())
        );
        // … and a clean log yields nothing, so the caller falls back to the tail.
        assert_eq!(first_diagnostic_line("all good\nTask build finished"), None);
        assert_eq!(first_diagnostic_line(""), None);
    }

    /// The build stage failing is a *distinguishable* outcome from the tool producing no
    /// results, so the row names the stage that broke.
    #[test]
    fn a_failed_build_stage_has_its_own_reason() {
        assert_eq!(BUILD_FAILED, "plb-build-failed");
        assert_ne!(BUILD_FAILED, no_results_reason(true));
        assert_ne!(BUILD_FAILED, no_results_reason(false));
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

    /// A run that produced no result JSON is a **failure** — the recorded `wall_ms` is the
    /// tool's own runtime, so it must never be reported as an `ok` benchmark row. The
    /// reason distinguishes "the tool exited non-zero" from "it exited 0 and benched
    /// nothing".
    #[test]
    fn a_run_without_results_is_a_failed_row_not_an_ok_one() {
        assert_eq!(no_results_reason(false), "plb-tool-failed");
        assert_eq!(no_results_reason(true), "plb-no-results");
        assert_ne!(no_results_reason(true), no_results_reason(false));
    }
}
