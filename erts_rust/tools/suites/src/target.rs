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
//! Shared machinery for both targets: staged copies, parallel module runs, and the
//! "identical invocation on both sides" helper.
//!
//! The read-only rule is structural here: every target is *copied* into
//! `$TMPDIR/erts-suites-stage/<target>` and the build/runs only ever touch the copy, so a
//! checkout under `$HOME` is never written to.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use crate::error::{Result, SuitesError};
use crate::proc::{self, Completed};
use crate::runtime::Runtime;
/// Knobs shared by both targets.
#[derive(Debug, Clone)]
pub struct TargetOptions {
    /// The read-only source checkout (or test directory).
    pub source: PathBuf,
    /// Restrict to these names (empty = the target's full/default set).
    pub only: Vec<String>,
    /// Per-module / per-case timeout.
    pub timeout: Duration,
    /// Whole-suite budget (stdlib only): how long one suite's *process* may spend launching
    /// cases before it stops and reports the rest as not measured.
    pub suite_budget: Duration,
    /// Concurrent module runs.
    pub jobs: usize,
    /// Stream commands as they run.
    pub verbose: bool,
    /// Rebuild the staged copy even when a completed build exists.
    pub rebuild: bool,
    /// Keep the staged tree (the default; only the tests set this false to clean up).
    pub keep_stage: bool,
}
/// Where staged copies live: `$TMPDIR/erts-suites-stage`.
pub fn stage_root() -> PathBuf {
    crate::runtime::tmp_root().join("erts-suites-stage")
}
/// Recursively copies `src` into `dst` (created when missing), skipping `.git`.
///
/// This is a full copy on purpose: the upstream checkout must stay byte-identical, and
/// `rebar3`/`erlc` need a writable tree.
pub fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_tree(&from, &to)?;
        } else if ty.is_file() {
            std::fs::copy(&from, &to)?;
        }
        // Symlinks are skipped deliberately: a staged tree with dangling links is worse than
        // a missing one, and neither target ships symlinks in its sources/test data.
    }
    Ok(())
}
/// Copies the entries of `src` matching `keep` into `dst`.
pub fn copy_filtered(src: &Path, dst: &Path, keep: &dyn Fn(&Path) -> bool) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        if !keep(&from) {
            continue;
        }
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
/// Removes a directory tree, ignoring "not found".
pub fn remove_tree(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(SuitesError::Io(err)),
    }
}
/// A simple work-stealing parallel map: `jobs` threads pull indices off a shared counter.
///
/// Results come back in input order, so a report is byte-stable regardless of scheduling.
/// Every item runs in its own OS process (a runtime invocation), so `jobs > 1` only changes
/// wall time — the counts are unaffected.
pub fn par_map<T, R, F>(jobs: usize, items: &[T], f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    let jobs = jobs.max(1).min(items.len().max(1));
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<R>>> = (0..items.len()).map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| loop {
                let idx = next.fetch_add(1, Ordering::Relaxed);
                if idx >= items.len() {
                    break;
                }
                let value = f(&items[idx]);
                if let Ok(mut slot) = slots[idx].lock() {
                    *slot = Some(value);
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|slot| match slot.into_inner() {
            Ok(Some(value)) => value,
            // A poisoned slot means a worker panicked; re-running the item would mask it, so
            // this cannot be reached with the closures used here (they never panic).
            _ => unreachable!("par_map slot was never filled"),
        })
        .collect()
}
/// Runs `-noshell <pa…> -eval <expr>` on one side and returns the captured run.
///
/// Both sides are built by this one function with the *same* `-pa` list and the *same*
/// `-eval` text — that identity is the whole point of the harness.
pub fn invoke(
    runtime: &Runtime,
    code_path: &[PathBuf],
    eval: &str,
    timeout: Duration,
    extra_args: &[String],
    cwd: Option<&Path>,
) -> std::result::Result<Completed, String> {
    let program = runtime
        .program()
        .ok_or_else(|| runtime.absent_reason.clone().unwrap_or_else(|| "runtime absent".into()))?;
    let mut cmd = Command::new(program);
    if let Some(dir) = cwd {
        std::fs::create_dir_all(dir).map_err(|e| format!("create cwd {}: {e}", dir.display()))?;
        cmd.current_dir(dir);
    }
    cmd.arg("-noshell");
    for dir in code_path {
        cmd.arg("-pa").arg(dir);
    }
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd.arg("-eval").arg(eval);
    if let Some(tier) = std::env::var_os("BEAM_RT_TIER") {
        cmd.env("BEAM_RT_TIER", tier);
    }
    proc::run_with_timeout(&mut cmd, Some(timeout)).map_err(|e| format!("spawn: {e}"))
}
/// Classifies a run whose output could not be parsed: a `beam_rt: unsupported: …` line on
/// stderr becomes `unsupported` (a capability gap), a timeout becomes `timeout`, and
/// anything else is `error` with the raw tail.
pub fn unclassified(run: &Completed) -> crate::report::Outcome {
    use crate::report::{Outcome, Status};
    if run.timed_out {
        return Outcome::bare(Status::Timeout, format!("killed after {:?}", run.wall_ms));
    }
    for line in run.stderr.lines().chain(run.stdout.lines()) {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("beam_rt: unsupported:") {
            return Outcome::bare(Status::Unsupported, rest.trim());
        }
        if t.starts_with("unsupported") || t.starts_with("unsupported_opcode") || t.starts_with("unsupported_bif") {
            return Outcome::bare(Status::Unsupported, t);
        }
    }
    Outcome::bare(Status::Error, run.tail(6))
}
/// The command line a run would use, for the report (without running it).
pub fn describe(
    runtime: &Runtime,
    code_path: &[PathBuf],
    eval: &str,
    extra_args: &[String],
    cwd: Option<&Path>,
) -> String {
    let program = runtime.program().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "<absent>".into());
    let mut cmd = Command::new(program);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.arg("-noshell");
    for dir in code_path {
        cmd.arg("-pa").arg(dir);
    }
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd.arg("-eval").arg(eval);
    let line = proc::cmdline(&cmd);
    match cwd {
        // `Command` renders the program and args only; the working directory is part of the
        // invocation (khepri's tests create `_test.<name>` dirs relative to it), so it is
        // prefixed explicitly.
        Some(dir) => format!("cd {} && {line}", dir.display()),
        None => line,
    }
}
/// Creates a row's working directory, discarding anything a *previous* run left behind.
///
/// Real suites write data directories relative to the cwd (`khepri`'s `_test.<name>`,
/// `stdlib`'s `data_dir` copies), so a stale tree from an earlier sweep makes fixtures
/// fail on *both* sides and silently turns a green row into a red one.
/// Both sides of one row still share the freshly created directory.
pub fn fresh_dir(dir: &Path) -> std::result::Result<(), String> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("clear cwd {}: {e}", dir.display())),
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("create cwd {}: {e}", dir.display()))
}
/// Writes `text` to `path`, creating the parent directory.
pub fn write_file(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn par_map_preserves_order_and_covers_every_item() {
        let items: Vec<u64> = (0..37).collect();
        let out = par_map(4, &items, |n| n * 2);
        assert_eq!(out, items.iter().map(|n| n * 2).collect::<Vec<_>>());
    }
    #[test]
    fn par_map_handles_empty_and_single() {
        let empty: Vec<u64> = Vec::new();
        assert!(par_map(4, &empty, |n: &u64| *n).is_empty());
        assert_eq!(par_map(8, &[7u64], |n| *n), vec![7]);
    }
    #[test]
    fn fresh_dir_discards_a_previous_runs_state() {
        let root = crate::runtime::tmp_root().join(format!("suites-fresh-{}", std::process::id()));
        let dir = root.join("cwd").join("module");
        std::fs::create_dir_all(dir.join("_test.leftover")).expect("mkdir");
        std::fs::write(dir.join("_test.leftover/x"), "stale").expect("write");
        fresh_dir(&dir).expect("fresh");
        assert!(dir.is_dir(), "the directory is recreated");
        assert!(!dir.join("_test.leftover").exists(), "stale state is gone");
        fresh_dir(&dir).expect("a second call on an existing empty dir is fine");
        let _ = remove_tree(&root);
    }
    #[test]
    fn copy_tree_skips_git_and_nests() {
        let root = crate::runtime::tmp_root().join(format!("suites-copy-{}", std::process::id()));
        let _ = remove_tree(&root);
        let src = root.join("src");
        std::fs::create_dir_all(src.join("nested")).expect("mkdir");
        std::fs::create_dir_all(src.join(".git")).expect("mkdir");
        std::fs::write(src.join("a.txt"), "a").expect("write");
        std::fs::write(src.join("nested/b.txt"), "b").expect("write");
        std::fs::write(src.join(".git/HEAD"), "x").expect("write");
        let dst = root.join("dst");
        copy_tree(&src, &dst).expect("copy");
        assert!(dst.join("a.txt").is_file());
        assert!(dst.join("nested/b.txt").is_file());
        assert!(!dst.join(".git").exists());
        remove_tree(&root).expect("cleanup");
    }
}
