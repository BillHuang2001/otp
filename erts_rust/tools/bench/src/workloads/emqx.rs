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

//! `emqx` — the messaging/copy-bound workload (the EMQX broker build).
//!
//! Stages the EMQX checkout into a private scratch copy under `$TMPDIR` (the source
//! repo is configured **read-only**) and runs `rebar3 compile` there, recording wall
//! time (and peak memory when measurable). Repo: `$BENCH_EMQX_REPO`, else
//! `/home/bill/Source/emqx`.
//!
//! `rebar3` is a hard prerequisite — absent ⇒ `skipped` (`rebar3-absent`). The opt-in
//! `nix shell` fallback (and its `$BENCH_TRY_NIX` switch) is shared with the other
//! tool-driven workloads; see [`crate::workloads::tool`]. It too is validated with a
//! bounded `--version` probe and, if it fails, the honest `rebar3-absent` skip is kept
//! (never a fabricated number).
//!
//! The checkout must also actually **be** a `rebar3` project (carry a root
//! `rebar.config`). EMQX is a `make`/`mix` project, so `rebar3 compile` at its root
//! would print `===> Verifying dependencies...`, compile *nothing*, and still exit `0`
//! — a silent no-op that would otherwise be recorded as a fabricated broker-build
//! number. That case is an honest `skipped(not-a-rebar3-project)`.

use std::path::{Path, PathBuf};

use crate::metrics::{Category, Metric, Probe, Runtimes, Status};
use crate::proc;
use crate::report::WorkloadResult;
use crate::scratch::{self, ScratchDir};
use crate::workload::{RunContext, Workload};
use crate::workloads::{resolve_repo, tool};

/// The documented default checkout path.
const DEFAULT_REPO: &str = "/home/bill/Source/emqx";

/// The external tool this workload drives.
const TOOL: &str = "rebar3";

/// The `nixpkgs` attribute that supplies [`TOOL`] for the opt-in `nix` fallback.
const TOOL_NIX_ATTR: &str = "rebar3";

/// The honest skip reason reported when [`TOOL`] cannot be resolved.
const TOOL_ABSENT: &str = "rebar3-absent";

/// The honest skip reason reported when the checkout is not a `rebar3` project (no root
/// `rebar.config`), so `rebar3 compile` would be a silent no-op.
const NOT_A_REBAR3_PROJECT: &str = "not-a-rebar3-project";

/// The EMQX workload.
pub struct Emqx;

fn repo() -> PathBuf {
    resolve_repo("emqx", DEFAULT_REPO)
}

/// Whether `repo` is a `rebar3` project — it carries a root `rebar.config`. EMQX is a
/// `make`/`mix` project, so `rebar3 compile` there builds nothing yet exits `0`; this
/// guard turns that into an honest skip rather than a fabricated build number.
fn is_rebar3_project(repo: &Path) -> bool {
    repo.join("rebar.config").is_file()
}

impl Workload for Emqx {
    fn id(&self) -> &'static str {
        "emqx"
    }

    fn description(&self) -> &'static str {
        "EMQX broker build (rebar3 compile) — messaging/copy bound"
    }

    fn category(&self) -> Category {
        Category::Messaging
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
        if !is_rebar3_project(&repo()) {
            return Probe::skipped(NOT_A_REBAR3_PROJECT);
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
        // A checkout without a root `rebar.config` is not a `rebar3` project: `rebar3
        // compile` would succeed while compiling nothing, so report the honest skip
        // instead of recording a meaningless wall time.
        if !is_rebar3_project(&r) {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                NOT_A_REBAR3_PROJECT,
            );
        }

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

        cmd.current_dir(&work).arg("compile");
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
            result.reason = Some("rebar3-compile-failed".to_string());
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
        let w = Emqx;
        assert_eq!(w.id(), "emqx");
        assert_eq!(w.category(), Category::Messaging);
        assert!(matches!(w.runtimes(), Runtimes::SysOnly));
    }

    #[test]
    fn probe_reports_rebar3_absence() {
        let p = Emqx.probe();
        if repo().is_dir() && !tool::available(TOOL) {
            assert_eq!(p.reason(), Some(TOOL_ABSENT));
        }
    }

    /// The root-`rebar.config` guard: a plain directory is not a `rebar3` project, a
    /// directory carrying `rebar.config` is. This is what keeps EMQX's no-op
    /// `rebar3 compile` an honest skip rather than a fabricated build number.
    #[test]
    fn is_rebar3_project_requires_a_root_rebar_config() {
        let dir = std::env::temp_dir().join(format!("bench-emqx-proj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");

        assert!(
            !is_rebar3_project(&dir),
            "an empty directory is not a rebar3 project"
        );

        std::fs::write(dir.join("rebar.config"), "{deps, []}.\n").expect("write rebar.config");
        assert!(
            is_rebar3_project(&dir),
            "a root rebar.config makes it a rebar3 project"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
