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

//! `khepri` — the GC/data-structure-bound workload (Khepri's test suite).
//!
//! Stages the Khepri checkout into a private scratch copy under `$TMPDIR` (the source
//! repo is configured **read-only**) and runs `rebar3 eunit` there, recording wall time
//! (and peak memory when measurable). Repo: `$BENCH_KHEPRI_REPO`, else
//! `/home/bill/Source/khepri`.
//!
//! `rebar3` is a hard prerequisite — absent ⇒ `skipped` (`rebar3-absent`). The opt-in
//! `nix shell` fallback (and its `$BENCH_TRY_NIX` switch) is shared with the other
//! tool-driven workloads; see [`crate::workloads::tool`]. It too is validated with a
//! bounded `--version` probe and, if it fails, the honest `rebar3-absent` skip is kept
//! (never a fabricated number).
//!
//! **The suite's Erlang is `rebar3`'s, not necessarily the reported oracle's.** `rebar3` is
//! an escript, so the Erlang that compiles and runs the suite is the one named by its `#!`
//! line; a Nix-store `rebar3` therefore runs the suite under whatever OTP *it* was built
//! against (e.g. OTP 29.1.2) while the report's `runtimes[]` row describes the driver's
//! resolved oracle (`erl` on `PATH`, e.g. OTP 29.1.1). The row records both — the resolved
//! `rebar3`, its shebang interpreter, and the eunit summary it printed — so a wall time is
//! never quoted against an Erlang release nobody verified.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::metrics::{Category, Metric, Probe, Runtimes, Status};
use crate::proc;
use crate::report::WorkloadResult;
use crate::scratch::{self, ScratchDir};
use crate::workload::{RunContext, Workload};
use crate::workloads::{resolve_repo, tool};

/// The documented default checkout path.
const DEFAULT_REPO: &str = "/home/bill/Source/khepri";

/// The external tool this workload drives.
const TOOL: &str = "rebar3";

/// The `nixpkgs` attribute that supplies [`TOOL`] for the opt-in `nix` fallback.
const TOOL_NIX_ATTR: &str = "rebar3";

/// The honest skip reason reported when [`TOOL`] cannot be resolved.
const TOOL_ABSENT: &str = "rebar3-absent";

/// The Khepri workload.
pub struct Khepri;

fn repo() -> PathBuf {
    resolve_repo("khepri", DEFAULT_REPO)
}

/// The interpreter named by a script's `#!` line — the Erlang a `rebar3` escript actually
/// runs its node with. `None` when `path` cannot be read or carries no shebang (a compiled
/// `rebar3`, e.g. a `rebar3` release binary).
///
/// Only the first line is read, and it is decoded lossily: an escript is a **binary** (its
/// `#!` line is followed by a zipped archive), so reading the whole file as UTF-8 — as
/// `fs::read_to_string` would — fails outright and would misreport every escript as
/// shebang-less.
fn shebang_interpreter(path: &Path) -> Option<String> {
    /// Plenty for a `#!` line and far less than any escript body.
    const HEAD: usize = 256;
    let mut head = Vec::with_capacity(HEAD);
    std::fs::File::open(path)
        .ok()?
        .take(HEAD as u64)
        .read_to_end(&mut head)
        .ok()?;
    let text = String::from_utf8_lossy(&head);
    let line = text.lines().next()?.trim();
    let rest = line.strip_prefix("#!")?;
    rest.split_whitespace().next().map(str::to_string)
}

/// The `rebar3` provenance note: the resolved program and — for an escript — the interpreter
/// its shebang names, with the explicit warning that this need not be the driver's oracle.
fn tool_note(tool: &Command) -> String {
    let program = tool.get_program();
    let program = Path::new(program);
    match shebang_interpreter(program) {
        Some(interpreter) => format!(
            "rebar3 resolved at {} — an escript whose shebang runs {interpreter}, so the suite's \
             Erlang is that one and may differ from this report's runtime row (the driver's \
             `erl` on PATH)",
            program.display()
        ),
        None => format!(
            "rebar3 resolved at {} (no shebang: a compiled rebar3, so the suite runs under the \
             Erlang that binary runs)",
            program.display()
        ),
    }
}

/// The eunit summary `rebar3` printed — the `N tests, M failures` line, with the
/// `Finished in … seconds` line above it — so the row records what the wall time covers.
fn eunit_summary(stdout: &str) -> Option<String> {
    let lines: Vec<&str> = stdout.lines().map(str::trim).collect();
    let counts = lines
        .iter()
        .rposition(|line| line.contains("tests,") && line.contains("failure"))?;
    let finished = lines[..counts]
        .iter()
        .rev()
        .find(|line| line.starts_with("Finished in "));
    Some(match finished {
        Some(finished) => format!("{} ({finished})", lines[counts]),
        None => lines[counts].to_string(),
    })
}

impl Workload for Khepri {
    fn id(&self) -> &'static str {
        "khepri"
    }

    fn description(&self) -> &'static str {
        "Khepri test suite (rebar3 eunit) — GC/data-structure bound"
    }

    fn category(&self) -> Category {
        Category::Gc
    }

    fn contracts(&self) -> &'static [&'static str] {
        &["A.1", "A.9"]
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
        let Some(mut cmd) = tool::command_for(TOOL, TOOL_NIX_ATTR) else {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                TOOL_ABSENT,
            );
        };

        // Stage the read-only checkout into a private scratch copy and run there, so the
        // build never writes build artifacts into the source repository.
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

        cmd.current_dir(&work).arg("eunit");
        // The tool's provenance is captured *before* the run, so even a failing sweep names
        // the `rebar3` (and therefore the Erlang) it used.
        let provenance = tool_note(&cmd);
        let out = match proc::run(&mut cmd) {
            Ok(o) => o,
            Err(e) => {
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    format!("rebar3-spawn-failed: {e}"),
                )
            }
        };

        let mut result = WorkloadResult::new(self.id(), kind, self.category(), self.description());
        result
            .metrics
            .push(Metric::scalar("wall_ms", "ms", out.wall_ms));
        result.peak_memory_bytes = out.peak_memory_bytes;
        result.notes.push(provenance);
        if let Some(summary) = eunit_summary(&out.stdout) {
            result.notes.push(format!("eunit: {summary}"));
        } else {
            result
                .notes
                .push("eunit printed no `N tests, M failures` summary".to_string());
        }
        if !out.success {
            result.status = Status::Failed;
            result.reason = Some("rebar3-eunit-failed".to_string());
            result.notes.push(format!("stderr: {}", out.stderr.trim()));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_stable() {
        let w = Khepri;
        assert_eq!(w.id(), "khepri");
        assert_eq!(w.category(), Category::Gc);
        assert_eq!(w.contracts(), &["A.1", "A.9"]);
        assert!(matches!(w.runtimes(), Runtimes::SysOnly));
    }

    #[test]
    fn probe_reports_rebar3_absence() {
        let p = Khepri.probe();
        if repo().is_dir() && !tool::available(TOOL) {
            assert_eq!(p.reason(), Some(TOOL_ABSENT));
        }
    }

    /// When the checkout **and** `rebar3` are both present the workload must be *ready* — the
    /// newly-enabled target must not silently regress to `skipped(rebar3-absent)`, and the
    /// resolved tool must be a real executable on disk. Early-returns when either prerequisite
    /// is absent, so CI (no checkout, no `rebar3`) stays green.
    #[test]
    fn probe_is_ready_when_the_checkout_and_rebar3_are_present() {
        if !(repo().is_dir() && tool::available(TOOL)) {
            return;
        }
        assert_eq!(
            Khepri.probe(),
            Probe::Ready,
            "a present checkout + rebar3 must probe ready"
        );
        if let Some(cmd) = tool::command_for(TOOL, TOOL_NIX_ATTR) {
            let program = cmd.get_program();
            assert!(
                std::path::Path::new(program).is_file(),
                "the resolved {TOOL} ({}) must exist on disk",
                program.to_string_lossy()
            );
        }
    }

    /// A self-cleaning directory holding one fixture file.
    fn fixture_file(tag: &str, name: &str, body: &str) -> PathBuf {
        fixture_bytes(tag, name, body.as_bytes())
    }

    /// A self-cleaning directory holding one fixture file with arbitrary (possibly non-UTF-8)
    /// contents.
    fn fixture_bytes(tag: &str, name: &str, body: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bench-khepri-{}-{}-{:?}",
            std::process::id(),
            tag,
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write fixture");
        path
    }

    /// `rebar3` is an escript, so its `#!` line names the Erlang the suite really runs on —
    /// the fact the row must record, because that release need not be the driver's oracle.
    /// The body after the `#!` line is a **binary** escript archive, which must not stop the
    /// shebang from being read.
    #[test]
    fn shebang_interpreter_names_the_erlang_a_rebar3_escript_runs_under() {
        let escript = b"#!/nix/store/y5db8dvib9fr41l53qrhgzpgx6sbb04b-erlang-29.1.2/bin/escript\n%% Rebar3 3.27.3\nPK\x03\x04\x80\x81\xff".to_vec();
        let path = fixture_bytes("shebang", "rebar3", &escript);
        assert_eq!(
            shebang_interpreter(&path).as_deref(),
            Some("/nix/store/y5db8dvib9fr41l53qrhgzpgx6sbb04b-erlang-29.1.2/bin/escript")
        );

        // The same head, read the naive way, is not even valid UTF-8 — the reason only the
        // first line is decoded (and lossily).
        assert!(std::fs::read_to_string(&path).is_err());

        // A compiled `rebar3` has no shebang, and an unreadable path has no answer at all —
        // both are `None` (the note then says so instead of inventing an interpreter).
        let compiled = fixture_bytes("no-shebang", "rebar3", b"\x7fELF\xff\xfe");
        assert_eq!(shebang_interpreter(&compiled), None);
        assert_eq!(shebang_interpreter(Path::new("/nonexistent/xyzzy")), None);
    }

    /// The tool note carries the resolved program and — for an escript — the interpreter plus
    /// the explicit "may differ from the report's runtime row" caveat.
    #[test]
    fn the_tool_note_records_the_escript_interpreter_and_its_caveat() {
        let path = fixture_file(
            "note",
            "rebar3",
            "#!/nix/store/abc-erlang-29.1.2/bin/escript\n",
        );
        let note = tool_note(&Command::new(&path));
        assert!(note.contains("rebar3 resolved at"), "{note}");
        assert!(note.contains("erlang-29.1.2/bin/escript"), "{note}");
        assert!(note.contains("may differ from this report's runtime row"), "{note}");

        let compiled = fixture_file("note-compiled", "rebar3", "\u{7f}ELF");
        let note = tool_note(&Command::new(&compiled));
        assert!(note.contains("no shebang"), "{note}");
    }

    /// The row records the eunit summary, so its wall time is never quoted without the test
    /// count behind it.
    #[test]
    fn eunit_summary_reads_the_test_count_and_the_finish_line() {
        let log = "===> Performing EUnit tests...\n\
                   ....\n\
                   Finished in 76.515 seconds\n\
                   1501 tests, 0 failures\n";
        assert_eq!(
            eunit_summary(log).as_deref(),
            Some("1501 tests, 0 failures (Finished in 76.515 seconds)")
        );

        // A failing sweep still yields its counts (the row is `failed` around them).
        assert_eq!(
            eunit_summary("Finished in 3.5 seconds\n2 tests, 1 failures\n").as_deref(),
            Some("2 tests, 1 failures (Finished in 3.5 seconds)")
        );
        // Counts without a finish line keep just the counts.
        assert_eq!(
            eunit_summary("7 tests, 0 failures").as_deref(),
            Some("7 tests, 0 failures")
        );
        // A log with no summary at all is `None`, never an invented one.
        assert_eq!(eunit_summary("===> Compiling khepri\n"), None);
        assert_eq!(eunit_summary(""), None);
    }
}
