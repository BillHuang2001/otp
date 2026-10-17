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

//! Sampling one `foreign` case under one runtime, and folding the interleaved pass pairs into
//! a per-case outcome.
//!
//! Extracted from [`super::foreign`] — which owns the case table, the staging and the report
//! row — so that file stays inside the crate's ~1000-line convention. Nothing here decides
//! *what* is measured: `super::foreign`'s `Case` supplies the driver function, the per-call
//! increment and the per-runtime loop count, its `argv_for` builds the argv, and this module
//! only runs the two passes ([`Pass::Work`] at `N`, [`Pass::Startup`] at `0`) **interleaved**,
//! checks the deterministic stdout, classifies failures, and hands the paired wall times to
//! [`crate::marginal`].

use std::path::Path;
use std::process::Command;

use crate::marginal::{resolve, sample_pairs, Marginal, PairedRuns, Pass, PassError};
use crate::proc::{self, Completed};
use crate::runtime::RuntimeKind;

use super::foreign::{argv_for, Case};

/// The samples collected for one case under one runtime.
pub(super) struct CaseOutcome {
    /// The loop count used.
    pub(super) n: i64,
    /// Wall-clock samples (ms) of the `n`-iteration runs, in interleaved round order.
    pub(super) wall: Vec<f64>,
    /// Wall-clock samples (ms) of the `0`-iteration (startup-only) runs, one per work sample.
    pub(super) startup: Vec<f64>,
    /// Peak-RSS samples (bytes) of the `n`-iteration (work) runs only.
    pub(super) rss: Vec<f64>,
    /// The stdout observed on the first successful `n`-iteration run.
    pub(super) observed: Option<String>,
    /// Whether the runtime reported the case as `unsupported` (Rust exit `3`).
    pub(super) unsupported: bool,
    /// Notes about failures/variations (never silent).
    pub(super) notes: Vec<String>,
    /// The paired, drift-cancelling marginal the per-call figure is resolved from.
    pub(super) marginal: Marginal,
}

impl CaseOutcome {
    /// Whether the case produced at least one usable timing.
    pub(super) fn ok(&self) -> bool {
        !self.wall.is_empty()
    }
}

/// One completed sample of a case: the wall time, the poller's peak RSS, the trimmed stdout
/// and any note about this particular run.
pub(super) struct CaseSample {
    /// End-to-end process wall time, in milliseconds.
    pub(super) wall_ms: f64,
    /// The process's peak RSS, when the poller could read it.
    pub(super) peak_memory_bytes: Option<u64>,
    /// The run's trimmed stdout.
    pub(super) stdout: String,
    /// Per-sample notes (e.g. an unexpected stdout on this pass).
    pub(super) notes: Vec<String>,
}

/// Runs **one** sample of `case` — the `Work` pass (`n` iterations) or the `Startup` pass
/// (`0` iterations, i.e. the same program's pure boot cost) — returning the sample, or a
/// [`PassError`] carrying the classified note when the run fails. The Rust runtime's `exit 3`
/// capability gap is flagged `unsupported` **only** while no work sample has completed: once
/// the case has produced a timing, a later failure is a real one.
fn sample_once(
    program: &Path, kind: RuntimeKind, dir: &Path, case: &Case, n: i64, pass: Pass,
    completed_work_samples: usize,
) -> std::result::Result<CaseSample, PassError> {
    let loops = n * pass.units();
    let mut cmd = Command::new(program);
    cmd.args(argv_for(kind, dir, case, loops));
    let completed = proc::run(&mut cmd).map_err(|e| PassError {
        note: format!("case {} ({}): spawn failed: {e}", case.name, kind.as_str()),
        unsupported: false,
    })?;
    if !completed.success {
        return Err(PassError {
            note: classify_failure(kind, case, &completed),
            unsupported: kind == RuntimeKind::Rust
                && completed.exit_code == Some(3)
                && completed_work_samples == 0,
        });
    }
    // The driver's result is linear in the loop count, so the expected stdout is exact on both
    // passes (`0` on the startup pass). A mismatch is a note, never a fatal error: the timing
    // is still real.
    let stdout = completed.stdout_trim().to_string();
    let expected = (loops * case.per_call).to_string();
    let mut notes = Vec::new();
    if stdout != expected {
        notes.push(format!(
            "case {}: unexpected stdout on the {} pass `{stdout}` (expected `{expected}`)",
            case.name,
            pass.as_str()
        ));
    }
    Ok(CaseSample {
        wall_ms: completed.wall_ms,
        peak_memory_bytes: completed.peak_memory_bytes,
        stdout,
        notes,
    })
}

/// Folds the interleaved runs into a [`CaseOutcome`]: the paired wall times, the work-pass peak
/// RSS, the first observed stdout, every per-sample note, the resolved marginal and the abort
/// (if any — the rounds completed before it are **kept** and still reported).
pub(super) fn outcome_from(n: i64, sampled: PairedRuns<CaseSample>) -> CaseOutcome {
    let PairedRuns { pairs, aborted } = sampled;
    let (mut wall, mut startup) = (Vec::with_capacity(pairs.len()), Vec::with_capacity(pairs.len()));
    let (mut rss, mut observed, mut notes) = (Vec::new(), None, Vec::new());
    for (work, baseline) in pairs {
        wall.push(work.wall_ms);
        startup.push(baseline.wall_ms);
        if let Some(bytes) = work.peak_memory_bytes {
            rss.push(bytes as f64);
        }
        if observed.is_none() {
            observed = Some(work.stdout);
        }
        notes.extend(work.notes);
        notes.extend(baseline.notes);
    }
    let deltas: Vec<(f64, f64)> = wall.iter().copied().zip(startup.iter().copied()).collect();
    // The closure already decided whether an abort was a capability gap (`exit 3` with no
    // completed work sample), so that classification carries over untouched.
    let unsupported = matches!(aborted.as_ref(), Some(abort) if abort.unsupported);
    if let Some(abort) = aborted {
        notes.push(abort.note);
    }
    CaseOutcome {
        n,
        wall,
        startup,
        rss,
        observed,
        unsupported,
        notes,
        marginal: resolve(&deltas, n),
    }
}

/// Classifies a non-zero / killed run into a human note.
fn classify_failure(kind: RuntimeKind, case: &Case, completed: &Completed) -> String {
    let stderr = completed.stderr.trim();
    match (kind, completed.exit_code) {
        (RuntimeKind::Rust, Some(3)) => {
            let reason = stderr.strip_prefix("beam_rt: unsupported: ").unwrap_or(stderr);
            format!("case {}: unsupported: {reason}", case.name)
        }
        (_, Some(code)) => format!("case {} ({}): exited {code}: {stderr}", case.name, kind.as_str()),
        (_, None) => format!("case {} ({}): terminated by signal: {stderr}", case.name, kind.as_str()),
    }
}

/// Runs one case `runs` times **interleaved**: every round samples the `n`-iteration pass and
/// then the `0`-iteration pass back to back, so the boot cost drifts *inside* a pair rather than
/// between two passes (see the module docs and `crate::marginal`). A failing sample aborts the
/// measurement; the rounds already completed are kept and reported, and a case whose **first
/// work** pass failed is flagged `unsupported` instead of timed.
pub(super) fn execute_case(
    program: &Path,
    kind: RuntimeKind,
    dir: &Path,
    case: &Case,
    runs: usize,
) -> CaseOutcome {
    let n = case.n_for(kind);
    let mut completed_work_samples = 0usize;
    let sampled = sample_pairs(runs, |pass| {
        let sample = sample_once(program, kind, dir, case, n, pass, completed_work_samples)?;
        if pass == Pass::Work {
            completed_work_samples += 1;
        }
        Ok(sample)
    });
    outcome_from(n, sampled)
}
