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

//! Shared support for the `differential` integration tests.
//!
//! Files under `tests/common/` are **not** compiled as their own test binary — each
//! integration test pulls this module in with `mod common;`.
//!
//! The one thing they need in common is a way to point the harness at the **freshly built**
//! `beam_rt` instead of the workspace default. [`differential::runtime::resolve_rust`]
//! derives `<workspace>/target/{release,debug}/beam_rt` and deliberately prefers the
//! `release` profile — which is right for the real CLI and for `gate.sh`, but wrong under
//! `cargo test`: only the **debug** artifacts are (re)built there, so a stale
//! `target/release/beam_rt` could shadow the fresh build and make a test observe a binary
//! that nobody just compiled (e.g. a stale release printing `ok ok` where the fresh debug
//! build prints `ok true`).
//!
//! Cargo builds the `differential` test binary and `beam_rt` into the *same* profile
//! directory, so the fresh runtime is simply the sibling of `CARGO_BIN_EXE_differential`.
//! `CARGO_BIN_EXE_beam_rt` would be the direct way to get it, but Cargo only defines that
//! for binaries of the *package under test* (not for another workspace package's binary), so
//! it is not available here; the sibling lookup is exact, not heuristic — it names the same
//! profile directory cargo just built both binaries into.

#![allow(dead_code)] // Each test binary includes this module; not every one uses every helper.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// A **worktree-local** scratch root for the integration tests, created if needed.
///
/// Historically the tests used [`std::env::temp_dir`] (i.e. `$TMPDIR`, `/tmp`). That is the
/// wrong place to keep test fixtures: it is a *shared* directory, and in a sandbox it can be
/// cleaned or reused by a sibling worker/task between the moment a fixture is written and the
/// moment it is read (this flaked once with a transient `ENOENT` when the child tried to
/// `exec` a fake `$BEAM_RT` written under `$TMPDIR`). `CARGO_TARGET_TMPDIR` is Cargo's
/// answer: a per-test-binary, per-worktree temp root (`<worktree>/erts_rust/target/tmp`),
/// private to this build and not touched by anyone else.
///
/// It is a compile-time constant (`env!`), so the directory is known without any environment
/// lookup at run time. Creation failure is **loud** (a panic with the offending path): a
/// scratch root that cannot be created means the test cannot run honestly, never that it
/// should silently pass.
pub fn scratch_root() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&root)
        .unwrap_or_else(|err| panic!("cannot create the scratch root {}: {err}", root.display()));
    root
}

/// The freshly built `beam_rt` sitting next to this crate's test binary, if it exists.
///
/// Returned only when the file actually exists. The name uses
/// [`std::env::consts::EXE_SUFFIX`] so it stays correct if the tests are built on Windows.
/// `None` means the runtime was not built alongside this test binary (the caller must then
/// report the Rust side honestly as `unsupported`, never as a fabricated pass).
pub fn fresh_rust_runtime() -> Option<PathBuf> {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_differential"));
    let dir = exe.parent()?;
    let candidate = dir.join(format!("beam_rt{}", std::env::consts::EXE_SUFFIX));
    candidate.is_file().then_some(candidate)
}

/// A `RuntimeSpec` for the fresh runtime, or `None` when it was not built.
///
/// Resolved through [`differential::runtime::resolve_rust_from`] with the fresh path passed
/// as the `$BEAM_RT` value, so the spec reports `RuntimeSource::Env("$BEAM_RT")` — exactly as
/// a real injection would — and can never fall back to the workspace default (a stale
/// `release` build). The resolution cannot fail because [`fresh_rust_runtime`] only returns
/// an existing file.
pub fn fresh_rust_spec() -> Option<differential::runtime::RuntimeSpec> {
    let path = fresh_rust_runtime()?;
    differential::runtime::resolve_rust_from(
        Some(path.into_os_string()),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
    .expect("the fresh beam_rt exists, so $BEAM_RT resolution cannot fail")
}

/// The freshly built `dist_probe` helper sitting next to this crate's test binary, if it
/// exists.
///
/// The helper is a `beam_dist` binary, so `CARGO_BIN_EXE_dist_probe` is not defined for this
/// package; the sibling lookup is exact — it names the same profile directory cargo built
/// both binaries into. `None` means the helper was not built alongside this test binary (the
/// caller must then report the Rust side of a command case honestly as a skip, never a
/// fabricated pass). The name uses [`std::env::consts::EXE_SUFFIX`] so it stays correct on
/// Windows.
pub fn fresh_helper() -> Option<PathBuf> {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_differential"));
    let dir = exe.parent()?;
    let candidate = dir.join(format!("dist_probe{}", std::env::consts::EXE_SUFFIX));
    candidate.is_file().then_some(candidate)
}

/// A `RuntimeSpec` for the fresh helper, or `None` when it was not built.
///
/// Resolved through [`differential::runtime::resolve_helper_from`] with the fresh path passed
/// as the `$BEAM_DIST_PROBE` value, so the spec reports `RuntimeSource::Env("$BEAM_DIST_PROBE")`
/// — exactly as a real injection would — and can never fall back to the workspace default.
pub fn fresh_helper_spec() -> Option<differential::runtime::RuntimeSpec> {
    let path = fresh_helper()?;
    differential::runtime::resolve_helper_from(
        Some(path.into_os_string()),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
    .expect("the fresh dist_probe exists, so $BEAM_DIST_PROBE resolution cannot fail")
}

/// Serializes the integration tests that run the **real conformance corpus** under the
/// oracle.
///
/// The harness sweeps the corpus *sequentially* ([`differential::harness::run_suite`]), but
/// `cargo test` runs the tests of one binary on **parallel threads**. Several tests in
/// `tests/cli.rs` each drive a full `suite` child process, so without serialization they
/// overlap and spawn many oracle `erl` processes at once. Two corpus cases —
/// `flags/sname_switch_value` (`-sname mynode`) and `flags/name_switch_value`
/// (`-name node@host`) — start distribution and therefore register their node name with
/// `epmd`; when two such `erl` processes run concurrently on the same host, the second loses
/// the name race and exits non-zero with
/// `Protocol 'inet_tcp': the name mynode@… seems to be in use by another Erlang node`.
///
/// The oracle then no longer reproduces the case golden, and
/// [`differential::compare::verdict_with`] turns an oracle golden miss into a
/// [`differential::compare::Verdict::Mismatch`] — a **spurious** divergence that has nothing
/// to do with the Rust runtime (the same case agrees when run alone, and the real gate
/// reports `mismatch=0`). The flake is `epmd`/hostname contention, not a runtime bug: two
/// concurrent `erl -sname mynode` reliably collide on this host, while 20 *sequential* runs
/// are clean.
///
/// The case id, node name and goldens are **frozen corpus data** (`src/corpus/flags.rs`), so
/// a test cannot hand the case a unique node name; the only faithful fix is to ensure that at
/// most one corpus sweep runs at a time. Every test that drives a full `suite` holds this
/// lock for the whole invocation. Cargo runs one binary's tests on parallel threads, so a
/// **process-wide** lock is exactly the right scope; it also runs the test *binaries*
/// sequentially, so nothing outside this process can contend.
///
/// **Do not remove this lock, and do not narrow it around a single invocation:** it must
/// span every oracle spawn of a name-bearing case within a test, or the epmd collision
/// returns. A poisoned lock is recovered (the panicking test already reported its failure);
/// the guard is intentionally *not* released early on a panic.
pub fn corpus_run_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
