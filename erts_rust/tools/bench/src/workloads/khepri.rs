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

use std::path::PathBuf;

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
}
