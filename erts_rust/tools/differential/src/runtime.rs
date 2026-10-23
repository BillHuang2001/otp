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

//! Runtime resolution and process observation.
//!
//! Two runtimes are observed and compared (contract A.6: `beam_rt` is a drop-in `erl`
//! for the expression cases — the *only* difference there is the program path):
//!
//! * the **oracle** — `$ERTS_ORACLE` if set (the path must exist), otherwise `erl`
//!   resolved by scanning `$PATH`. Nothing found ⇒ [`Error::Env`] (exit 2).
//! * the **Rust runtime** — `$BEAM_RT` if set (the path must exist, else [`Error::Env`]),
//!   otherwise the workspace-relative `target/{release,debug}/beam_rt`. Neither exists ⇒
//!   `Ok(None)`; the harness then reports the Rust side as *unsupported* instead of
//!   inventing a result.
//! * the **distribution helper** (`dist_probe`, the Rust side of a live two-node loopback
//!   case) — `$BEAM_DIST_PROBE` if set (the path must exist, else [`Error::Env`]), otherwise
//!   the workspace-relative `target/{release,debug}/dist_probe`; neither exists ⇒ `Ok(None)`
//!   and the case is *skipped*. It is resolved by [`resolve_helper`]/[`resolve_helper_from`]
//!   and reported with [`RuntimeId::Rust`].
//!
//! Resolution is a pure lookup and never mutates anything; the **binary** additionally makes
//! the live two-node loopback cases runnable in a tree where only `beam_rt` and `differential`
//! were built: [`ensure_helper`] resolves the helper and, when nothing was found, builds the
//! `beam_dist` package **once** (through the committed `scripts/cargo.sh` wrapper) and
//! re-resolves. Setting `$DIFFERENTIAL_NO_BUILD` (to any non-empty value) switches that build
//! off — the cases then stay a loud *skipped* naming the reason, exactly as before. A failed
//! build is never fatal either: it keeps the loud *skipped* and carries cargo's diagnosis.
//!
//! **Every spawned child runs in the OTP tree root** — see [`otp_root`]. A handful of
//! contract-A.9 cases address the tree **repo-relatively** (`compile:file("lib/stdlib/src/timer.erl")`,
//! `epp:parse_file("lib/stdlib/src/ordsets.erl", [])`, an `{i, "lib/xmerl/include"}` include
//! path, …), so a child's working directory must never depend on where the harness happened to
//! be invoked from: `gate.sh` runs it from `$ERL_TOP`, `cargo test` from the crate directory,
//! and a plain `differential run` from wherever the user is. Off the tree root **both** sides
//! would fail to find the source, the oracle would stop reproducing its golden, and
//! [`crate::compare::verdict_with`] would (correctly, but spuriously) report a **mismatch** for
//! a case that is really fine. The root is derived from this crate's position in the tree and
//! overridable with `$DIFFERENTIAL_OTP_ROOT`; nothing is baked in, and a binary that was moved
//! out of its tree degrades to inheriting the harness's own working directory.
//!
//! The workspace default searches `release` **before** `debug` on purpose: it is what the
//! real CLI and the CI `gate.sh` want, so the fastest artifact present is measured. That
//! order is *wrong* under `cargo test`, where only the `debug` artifacts are (re)built — a
//! stale `target/release/beam_rt` could then shadow the fresh build. The integration tests
//! therefore inject `$BEAM_RT` (the fresh debug `beam_rt` sibling of the test binary; see
//! `../../tests/common/mod.rs`), so a stale release binary is never observed by a test.
//! This is purely a resolution *preference*: the order itself is never changed here.
//!
//! The harness never inspects runtime internals: it only spawns the program, feeds it
//! argv with `stdin` closed, and records the observed stdout/stderr/exit status. The two
//! runtimes receive **identical** argv for the expression corpus cases but **different**
//! argv for the module-run cases (the oracle's `-eval` versus the Rust `run` action), so
//! each [`Observation`] records the argv it was actually spawned with. An environment
//! case ([`CaseKind::EvalEnv`](crate::corpus::CaseKind::EvalEnv),
//! [`CaseKind::ModuleEnv`](crate::corpus::CaseKind::ModuleEnv)) additionally spawns both
//! runtimes with an explicit environment ([`run_with_env`]), so their `ERL_*FLAGS` merge —
//! and, for a module-run case, the flag's *application* to the code path — is what is
//! compared.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;

use crate::error::{Error, Result};

/// The exit code the Rust runtime uses to report a **capability gap** (an operation
/// Milestone-1 does not implement) — distinct from `1` (a genuine runtime error) and `2`
/// (a usage error).
///
/// This is the documented external signal the harness keys off: an exit-`3` observation is
/// classified as [`SideResult::Unsupported`](crate::compare::SideResult), never as a
/// differing observable result, so an unimplemented feature is `unsupported`, not a
/// `mismatch`. The signal accompanies exactly one `beam_rt: unsupported: <reason>` stderr
/// line (see `../../crates/beam_rt/src/cli.rs`).
pub const UNSUPPORTED_EXIT_CODE: i32 = 3;

/// Which of the two runtimes an observation came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeId {
    /// The real system ERTS — the compatibility oracle.
    Oracle,
    /// The Rust runtime (`beam_rt`).
    Rust,
}

impl RuntimeId {
    /// Stable identifier used in reports (`"oracle"` / `"rust"`).
    pub fn id(&self) -> &'static str {
        match self {
            RuntimeId::Oracle => "oracle",
            RuntimeId::Rust => "rust",
        }
    }
}

/// How a runtime's program path was obtained (reported, so results are reproducible).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeSource {
    /// An environment variable override, named with its `$` prefix.
    Env(&'static str),
    /// Resolved by scanning `$PATH`.
    Path,
    /// The documented workspace-relative default (`erts_rust/target/...`).
    WorkspaceDefault,
}

impl RuntimeSource {
    /// Stable identifier used in reports.
    pub fn as_str(&self) -> &'static str {
        match self {
            RuntimeSource::Env(name) => name,
            RuntimeSource::Path => "$PATH",
            RuntimeSource::WorkspaceDefault => "workspace-default",
        }
    }
}

/// A resolved runtime: what to spawn and why that program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSpec {
    /// Which side of the comparison this is.
    pub id: RuntimeId,
    /// The executable to spawn.
    pub program: PathBuf,
    /// How `program` was resolved.
    pub source: RuntimeSource,
}

/// One observed run of a runtime: exactly what the harness fed in and saw come out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// Which runtime was observed.
    pub runtime: RuntimeId,
    /// The program that was spawned.
    pub program: PathBuf,
    /// The argv the program was spawned with (per runtime — the expression cases share
    /// one argv, the module-run cases differ).
    pub argv: Vec<String>,
    /// Captured standard output (raw bytes — never re-encoded).
    pub stdout: Vec<u8>,
    /// Captured standard error (raw bytes).
    pub stderr: Vec<u8>,
    /// Process exit code, or `None` when the process was terminated by a signal.
    pub exit_code: Option<i32>,
}

/// Resolve the oracle runtime from the real environment.
pub fn resolve_oracle() -> Result<RuntimeSpec> {
    resolve_oracle_from(std::env::var_os("ERTS_ORACLE"), std::env::var_os("PATH"))
}

/// Resolve the oracle from explicit inputs (the injectable core of [`resolve_oracle`]).
///
/// * `erts_oracle` — the value of `$ERTS_ORACLE`, if set. It must name an existing
///   executable, otherwise this is a loud [`Error::Env`].
/// * `path_var` — the value of `$PATH` used to locate `erl` when `$ERTS_ORACLE` is unset.
pub fn resolve_oracle_from(
    erts_oracle: Option<OsString>,
    path_var: Option<OsString>,
) -> Result<RuntimeSpec> {
    if let Some(value) = erts_oracle {
        let program = PathBuf::from(&value);
        if !is_executable(&program) {
            return Err(Error::env(format!(
                "$ERTS_ORACLE is set to '{}', which is not an existing executable",
                program.display()
            )));
        }
        return Ok(RuntimeSpec {
            id: RuntimeId::Oracle,
            program,
            source: RuntimeSource::Env("$ERTS_ORACLE"),
        });
    }

    if let Some(path_var) = path_var {
        for dir in std::env::split_paths(&path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            for name in oracle_program_names() {
                let candidate = dir.join(name);
                if is_executable(&candidate) {
                    return Ok(RuntimeSpec {
                        id: RuntimeId::Oracle,
                        program: candidate,
                        source: RuntimeSource::Path,
                    });
                }
            }
        }
    }

    Err(Error::env(
        "cannot locate the oracle runtime: $ERTS_ORACLE is unset and no 'erl' was found on $PATH \
         (set $ERTS_ORACLE to the oracle erl executable)",
    ))
}

/// Resolve the Rust runtime from the real environment.
///
/// `Ok(None)` means "there is no Rust runtime to observe" — the caller must report the
/// Rust side as *unsupported*, never as agreement.
pub fn resolve_rust() -> Result<Option<RuntimeSpec>> {
    resolve_rust_from(
        std::env::var_os("BEAM_RT"),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
}

/// Resolve the Rust runtime from explicit inputs (the injectable core of [`resolve_rust`]).
///
/// * `beam_rt` — the value of `$BEAM_RT`, if set. It must name an existing file,
///   otherwise this is a loud [`Error::Env`].
/// * `manifest_dir` — the crate directory (`$CARGO_MANIFEST_DIR`); the workspace default
///   is `<manifest_dir>/../../target/{release,debug}/beam_rt`.
///
/// The `release` profile is searched before `debug` deliberately: the real CLI and
/// `gate.sh` want the fastest artifact available. Integration tests must **not** rely on
/// this default — they inject `$BEAM_RT` pointing at the freshly built debug `beam_rt`
/// (see `../../tests/common/mod.rs`), so a stale `release` binary can never shadow a fresh
/// `debug` build. The preference order is intentionally left unchanged.
pub fn resolve_rust_from(
    beam_rt: Option<OsString>,
    manifest_dir: &Path,
) -> Result<Option<RuntimeSpec>> {
    if let Some(value) = beam_rt {
        let program = PathBuf::from(&value);
        if !program.is_file() {
            return Err(Error::env(format!(
                "$BEAM_RT is set to '{}', which is not an existing file",
                program.display()
            )));
        }
        return Ok(Some(RuntimeSpec {
            id: RuntimeId::Rust,
            program,
            source: RuntimeSource::Env("$BEAM_RT"),
        }));
    }

    let target = manifest_dir
        .parent()
        .and_then(Path::parent)
        .map(|workspace| workspace.join("target"))
        .ok_or_else(|| {
            Error::env(format!(
                "cannot derive the workspace target directory from CARGO_MANIFEST_DIR='{}'",
                manifest_dir.display()
            ))
        })?;

    for profile in ["release", "debug"] {
        let candidate = target.join(profile).join(rust_program_name());
        if candidate.is_file() {
            return Ok(Some(RuntimeSpec {
                id: RuntimeId::Rust,
                program: candidate,
                source: RuntimeSource::WorkspaceDefault,
            }));
        }
    }
    Ok(None)
}

/// Resolve the distribution helper (`dist_probe`) from the real environment.
///
/// The helper is the **Rust side** of a live two-node distribution loopback case
/// ([`CaseKind::Command`](crate::corpus::CaseKind::Command)); it is driven exactly like the
/// runtime — `$BEAM_DIST_PROBE` if set (the path must exist, else [`Error::Env`]), otherwise
/// the workspace-relative `target/{release,debug}/dist_probe`. Neither exists ⇒ `Ok(None)`;
/// the harness then reports the Rust side as *unsupported* instead of inventing a result.
pub fn resolve_helper() -> Result<Option<RuntimeSpec>> {
    resolve_helper_from(
        std::env::var_os("BEAM_DIST_PROBE"),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
}

/// Resolve the distribution helper from explicit inputs (the injectable core of
/// [`resolve_helper`]).
///
/// * `beam_dist_probe` — the value of `$BEAM_DIST_PROBE`, if set. It must name an existing
///   file, otherwise this is a loud [`Error::Env`].
/// * `manifest_dir` — the crate directory (`$CARGO_MANIFEST_DIR`); the workspace default is
///   `<manifest_dir>/../../target/{release,debug}/dist_probe`.
///
/// It is the **Rust side** of the comparison (the helper speaks for the Rust runtime's
/// distribution stack), so it is reported with [`RuntimeId::Rust`] and no new
/// [`RuntimeId`]/[`RuntimeSource`] variant is introduced. The `release`-before-`debug`
/// preference mirrors [`resolve_rust_from`] and is left unchanged for the same reason.
pub fn resolve_helper_from(
    beam_dist_probe: Option<OsString>,
    manifest_dir: &Path,
) -> Result<Option<RuntimeSpec>> {
    if let Some(value) = beam_dist_probe {
        let program = PathBuf::from(&value);
        if !program.is_file() {
            return Err(Error::env(format!(
                "$BEAM_DIST_PROBE is set to '{}', which is not an existing file",
                program.display()
            )));
        }
        return Ok(Some(RuntimeSpec {
            id: RuntimeId::Rust,
            program,
            source: RuntimeSource::Env("$BEAM_DIST_PROBE"),
        }));
    }

    let target = manifest_dir
        .parent()
        .and_then(Path::parent)
        .map(|workspace| workspace.join("target"))
        .ok_or_else(|| {
            Error::env(format!(
                "cannot derive the workspace target directory from CARGO_MANIFEST_DIR='{}'",
                manifest_dir.display()
            ))
        })?;

    for profile in ["release", "debug"] {
        let candidate = target.join(profile).join(helper_program_name());
        if candidate.is_file() {
            return Ok(Some(RuntimeSpec {
                id: RuntimeId::Rust,
                program: candidate,
                source: RuntimeSource::WorkspaceDefault,
            }));
        }
    }
    Ok(None)
}

/// The reason recorded for the Rust side of a command case when no `dist_probe` helper could
/// be found.
///
/// When the binary's on-demand build ([`ensure_helper`]) ran, the reason also carries *why*
/// it did not produce a helper — either cargo's diagnosis or the `$DIFFERENTIAL_NO_BUILD`
/// opt-out — so a skip is always self-explaining and never silent.
pub fn helper_unsupported_reason() -> String {
    let mut reason =
        "no distribution helper found: $BEAM_DIST_PROBE is unset and no built dist_probe exists \
         under the workspace target/{release,debug} directory"
            .to_string();
    if no_build_requested(std::env::var_os(NO_BUILD_ENV).as_deref()) {
        reason.push_str(&format!(
            " (the on-demand build is disabled by ${NO_BUILD_ENV})"
        ));
    } else if let Some(HelperBuild::Failed(detail)) = helper_build_outcome() {
        reason.push_str(&format!(" (the on-demand build failed: {detail})"));
    }
    reason
}

/// The environment variable that switches the **on-demand helper build** off.
///
/// Set it (to any non-empty value) in a tree that must never invoke the build system: the
/// live-distribution command cases then report the loud `skipped` documented on
/// [`helper_unsupported_reason`] instead of building `dist_probe`.
pub const NO_BUILD_ENV: &str = "DIFFERENTIAL_NO_BUILD";

/// The workspace package that produces the `dist_probe` helper binary.
const HELPER_PACKAGE: &str = "beam_dist";

/// How much of a failed build's `stderr` is kept in the skip reason (cargo's diagnosis is at
/// the end of its output, so the **tail** is kept).
const BUILD_DETAIL_MAX: usize = 2000;

/// What [`ensure_helper`] found — or, when nothing was resolvable, built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperStatus {
    /// A `dist_probe` helper is available.
    Available {
        /// The resolved helper (the Rust side of the live loopback cases).
        spec: RuntimeSpec,
        /// `true` when this process had to run the one-shot on-demand build to obtain it,
        /// `false` when it was already resolvable.
        built_on_demand: bool,
    },
    /// No helper could be obtained; `reason` is the loud skip reason to report — never a
    /// fabricated pass and never agreement.
    Unavailable(String),
}

/// Resolve the distribution helper, building it **once** on demand when it is missing.
///
/// This is the *binary's* entry point (`run`/`suite` of a case that needs the helper); the
/// library's [`resolve_helper`] stays a pure lookup, which is what the integration tests rely
/// on (they inject `$BEAM_DIST_PROBE`).
///
/// Policy, in order:
///
/// 1. a resolvable helper ([`resolve_helper`]) always wins — the environment override and the
///    workspace `target/{release,debug}` artifacts are never overridden by a build;
/// 2. otherwise, if `$DIFFERENTIAL_NO_BUILD` is set to a non-empty value, nothing is built and
///    the result is [`HelperStatus::Unavailable`] (a loud *skipped*);
/// 3. otherwise `scripts/cargo.sh build --manifest-path <workspace>/Cargo.toml -p beam_dist`
///    runs **at most once per process** (the outcome is cached in a [`OnceLock`]), with its
///    stdin closed and its output captured, and the helper is re-resolved afterwards.
///
/// The build is *not* an error path: a failed build — for any reason, including a broken build
/// environment — yields [`HelperStatus::Unavailable`] carrying cargo's diagnosis, so the
/// affected cases stay a loud *skipped*. Nothing here can turn a missing helper into agreement.
pub fn ensure_helper() -> Result<HelperStatus> {
    let resolved = resolve_helper()?;
    let no_build = std::env::var_os(NO_BUILD_ENV);
    match helper_plan(resolved.is_some(), no_build.as_deref()) {
        HelperPlan::UseResolved => Ok(HelperStatus::Available {
            spec: resolved.expect("`helper_plan` reports `UseResolved` only for a resolved helper"),
            built_on_demand: false,
        }),
        HelperPlan::BuildDisabled => Ok(HelperStatus::Unavailable(helper_unsupported_reason())),
        HelperPlan::Build => match helper_build_slot()
            .get_or_init(|| run_helper_build(Path::new(env!("CARGO_MANIFEST_DIR"))))
        {
            HelperBuild::Failed(_) => Ok(HelperStatus::Unavailable(helper_unsupported_reason())),
            HelperBuild::Built => match resolve_helper()? {
                Some(spec) => Ok(HelperStatus::Available {
                    spec,
                    built_on_demand: true,
                }),
                None => Ok(HelperStatus::Unavailable(format!(
                    "{} (the on-demand `cargo build -p {HELPER_PACKAGE}` succeeded but produced no \
                     dist_probe artifact under the workspace target directory — is \
                     $CARGO_TARGET_DIR redirected?)",
                    helper_unsupported_reason()
                ))),
            },
        },
    }
}

/// What to do about the helper before the corpus runs (the pure core of [`ensure_helper`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelperPlan {
    /// A helper is already resolvable — use it and never build.
    UseResolved,
    /// No helper and `$DIFFERENTIAL_NO_BUILD` forbids building — skip loudly.
    BuildDisabled,
    /// No helper and building is allowed — build it (at most once per process).
    Build,
}

/// Decide whether a resolved helper is used, the build is refused, or the helper is built.
///
/// * `helper_resolved` — whether [`resolve_helper`] found a helper;
/// * `no_build` — the value of `$DIFFERENTIAL_NO_BUILD`, if set.
///
/// A resolved helper always wins (the build is a *last resort*, never a way to replace what the
/// environment or the workspace already provides), and the opt-out is honoured only when there
/// is nothing to use anyway.
fn helper_plan(helper_resolved: bool, no_build: Option<&OsStr>) -> HelperPlan {
    if helper_resolved {
        HelperPlan::UseResolved
    } else if no_build_requested(no_build) {
        HelperPlan::BuildDisabled
    } else {
        HelperPlan::Build
    }
}

/// Whether `$DIFFERENTIAL_NO_BUILD` forbids the on-demand build (`set and non-empty`).
fn no_build_requested(no_build: Option<&OsStr>) -> bool {
    no_build.is_some_and(|value| !value.is_empty())
}

/// The one-shot outcome of the on-demand helper build.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HelperBuild {
    /// `cargo build -p beam_dist` succeeded (the artifact is re-resolved by the caller).
    Built,
    /// It failed — the message is cargo's diagnosis, kept for the skip reason.
    Failed(String),
}

/// The process-wide slot holding the on-demand build's outcome.
fn helper_build_slot() -> &'static OnceLock<HelperBuild> {
    static BUILD: OnceLock<HelperBuild> = OnceLock::new();
    &BUILD
}

/// The cached build outcome **without** triggering a build (`None` = never attempted).
fn helper_build_outcome() -> Option<&'static HelperBuild> {
    helper_build_slot().get()
}

/// Build the `dist_probe` helper through the committed workspace cargo wrapper.
///
/// `manifest_dir` is this crate's directory (`$CARGO_MANIFEST_DIR`), so the workspace root and
/// the wrapper are derived, never baked in. The child runs with its stdin closed and its output
/// captured: nothing is streamed into the harness's own stdout/stderr, and the caller decides
/// what to report.
fn run_helper_build(manifest_dir: &Path) -> HelperBuild {
    let Some(workspace) = workspace_root(manifest_dir) else {
        return HelperBuild::Failed(format!(
            "cannot derive the erts_rust workspace root from CARGO_MANIFEST_DIR='{}'",
            manifest_dir.display()
        ));
    };
    let wrapper = workspace.join("scripts").join("cargo.sh");
    if !wrapper.is_file() {
        return HelperBuild::Failed(format!(
            "the workspace cargo wrapper '{}' does not exist",
            wrapper.display()
        ));
    }
    let manifest = workspace.join("Cargo.toml");
    let mut cmd = Command::new(&wrapper);
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("-p")
        .arg(HELPER_PACKAGE)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match output_with_retry(&mut cmd) {
        Ok(output) if output.status.success() => HelperBuild::Built,
        Ok(output) => HelperBuild::Failed(build_failure_detail(&output)),
        Err(err) => HelperBuild::Failed(format!("failed to run '{}': {err}", wrapper.display())),
    }
}

/// The workspace root (`…/erts_rust`) this crate lives in — the same derivation
/// [`resolve_helper_from`] uses for `target/`.
fn workspace_root(manifest_dir: &Path) -> Option<&Path> {
    manifest_dir.parent().and_then(Path::parent)
}

/// A bounded, human-readable summary of a failed build: the exit status plus the tail of the
/// build's `stderr` (cargo's diagnosis), so the skip reason stays readable in a report.
fn build_failure_detail(output: &std::process::Output) -> String {
    let status = output
        .status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_string());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        return format!("`cargo build -p {HELPER_PACKAGE}` exited {status} with no stderr");
    }
    format!(
        "`cargo build -p {HELPER_PACKAGE}` exited {status}: {}",
        tail(stderr, BUILD_DETAIL_MAX)
    )
}

/// The last `max` bytes of `text`, cut on a char boundary (keeping the tail, where cargo puts
/// its diagnosis).
fn tail(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Build the argv for an evaluation: `["-noshell", <extra…>, "-eval", <expr>]`.
///
/// The expression corpus cases hand this same argv to both runtimes; module-run cases
/// build their argv per side (see [`crate::corpus::ModuleRun`]).
pub fn eval_argv(expr: &str, extra: &[&str]) -> Vec<String> {
    let extra: Vec<String> = extra.iter().map(|arg| (*arg).to_string()).collect();
    eval_argv_owned(expr, &extra)
}

/// [`eval_argv`] for dynamically built extra arguments (e.g. repeated `-pa <dir>`).
pub fn eval_argv_owned(expr: &str, extra: &[String]) -> Vec<String> {
    let mut argv = Vec::with_capacity(extra.len() + 3);
    argv.push("-noshell".to_string());
    argv.extend(extra.iter().cloned());
    argv.push("-eval".to_string());
    argv.push(expr.to_string());
    argv
}

/// How many times a child spawn is attempted before a transient `ETXTBSY` is surfaced.
///
/// The race is short-lived (it lasts only until the thread that is writing the script
/// closes its descriptor), so a handful of attempts suffice. The bound matters: a
/// *permanently* write-held file (or any other failure) is returned, loudly, after this
/// many attempts — the retry can never spin forever or swallow an error.
const SPAWN_ATTEMPTS: u32 = 6;

/// The first backoff before retrying a transient `ETXTBSY` spawn.
const SPAWN_BACKOFF: std::time::Duration = std::time::Duration::from_millis(5);

/// The cap the `ETXTBSY` backoff grows to (it doubles after each attempt).
///
/// With [`SPAWN_ATTEMPTS`] `= 6` and a 5 ms first backoff, the total bounded window is
/// `5 + 10 + 20 + 40 + 80 = 155 ms`: long enough that a writer held open by a *sibling*
/// thread under heavy parallel load (a testing/pre-building process) has released it,
/// short enough that a genuine, permanent failure is still surfaced promptly.
const SPAWN_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_millis(80);

/// Spawn `cmd` and collect its output, retrying **only** the transient `ETXTBSY` race.
///
/// Both spawn sites in this crate legitimately execute a *freshly written* `#!/bin/sh`
/// script: `$ERTS_ORACLE`/`$BEAM_RT` may name a wrapper script, and `module_build`-style
/// stub compilers are scripts too. When one thread `fork()`s (via `Command::output`) while
/// another thread still holds an open descriptor **for writing** that script, the child
/// transiently inherits the descriptor; if the other thread then `exec`s the script the
/// kernel refuses with `ETXTBSY` ("Text file busy", errno 26). This is a pure scheduling
/// artifact — the file is fully written and executable — so retrying after a short pause
/// yields exactly the result the first attempt would have produced had the other thread
/// closed its descriptor first. It is therefore *not* error swallowing: every non-`ETXTBSY`
/// error, and the `ETXTBSY` that survives all [`SPAWN_ATTEMPTS`] attempts, is returned
/// unchanged for the caller to surface loudly.
///
/// The backoff **doubles** after each attempt, capped at [`SPAWN_BACKOFF_MAX`], so the
/// total wait (155 ms) comfortably outlasts a writer held open by a concurrent `fork()`
/// under parallel test load while staying bounded.
pub(crate) fn output_with_retry(cmd: &mut Command) -> std::io::Result<std::process::Output> {
    retry_transient(|| cmd.output())
}

/// Spawn `cmd` (without waiting for it), retrying **only** the transient `ETXTBSY` race — the
/// same bounded policy [`output_with_retry`] applies to a collected run.
///
/// A peer leg is spawned long enough before its first output that a transiently busy program
/// would otherwise turn a healthy two-VM case into a skip; the retry is bounded and every
/// other error is returned unchanged, exactly like [`output_with_retry`].
fn spawn_with_retry(cmd: &mut Command) -> std::io::Result<Child> {
    retry_transient(|| cmd.spawn())
}

/// Run `attempt` until it succeeds, retrying **only** a transient `ETXTBSY` (see
/// [`output_with_retry`]) with a bounded, doubling backoff; every other error, and an
/// `ETXTBSY` that survives all [`SPAWN_ATTEMPTS`] attempts, is returned unchanged.
fn retry_transient<T>(
    mut attempt: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let mut tries = 1;
    let mut backoff = SPAWN_BACKOFF;
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(err) if tries < SPAWN_ATTEMPTS && is_transient_text_file_busy(&err) => {
                std::thread::sleep(backoff);
                backoff = backoff.saturating_mul(2).min(SPAWN_BACKOFF_MAX);
                tries += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Whether `err` is the transient `ETXTBSY` ("Text file busy") fork/exec race described on
/// [`output_with_retry`].
fn is_transient_text_file_busy(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::ExecutableFileBusy {
        return true;
    }
    // Belt and suspenders: accept the raw errno too, in case a platform/libc mapping does
    // not surface `ETXTBSY` as `ErrorKind::ExecutableFileBusy`.
    #[cfg(unix)]
    {
        const ETXTBSY: i32 = 26;
        if err.raw_os_error() == Some(ETXTBSY) {
            return true;
        }
    }
    false
}

/// The environment variable that **overrides** the OTP tree root a child runtime runs in.
///
/// When set it must name an existing directory, otherwise resolution is a loud
/// [`Error::Env`] — mirroring `$ERTS_ORACLE`/`$BEAM_RT`/`$BEAM_DIST_PROBE`, so the harness stays
/// "overridable, no baked absolute paths". Setting it to a foreign directory is honoured as
/// given (it is the one way to run the corpus against a *different* copy of the tree).
pub const OTP_ROOT_ENV: &str = "DIFFERENTIAL_OTP_ROOT";

/// Derive the OTP tree root (`$ERL_TOP`) from this crate's directory.
///
/// `manifest_dir` is `$CARGO_MANIFEST_DIR`, i.e. `…/erts_rust/tools/differential`, so the tree
/// root is its **third** parent — `differential` → `tools` → `erts_rust` → `$ERL_TOP` (the
/// crate lives exactly three levels below the root, next to `lib/`, `erts/` and
/// `bootstrap/`). The derivation is *checked*: `Some` only when the derived directory exists.
/// A binary that was moved out of its tree therefore degrades to inheriting the harness's own
/// working directory instead of handing a child a directory that does not exist (which would
/// fail every spawn outright).
pub fn otp_root_from(manifest_dir: &Path) -> Option<PathBuf> {
    let root = workspace_root(manifest_dir)?.parent()?;
    root.is_dir().then(|| root.to_path_buf())
}

/// Resolve the OTP tree root a child runtime must run in, from the real environment.
///
/// `$DIFFERENTIAL_OTP_ROOT` wins when set (and must name an existing directory, else a loud
/// [`Error::Env`] ⇒ exit 2); otherwise the root is derived from this crate's position
/// ([`otp_root_from`]). `Ok(None)` means "no tree root could be derived" — the child then
/// simply inherits the harness's working directory, which is the pre-existing behaviour.
pub fn otp_root() -> Result<Option<PathBuf>> {
    otp_root_with(
        std::env::var_os(OTP_ROOT_ENV),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
}

/// Resolve the OTP tree root from explicit inputs (the injectable core of [`otp_root`]).
///
/// * `override_root` — the value of `$DIFFERENTIAL_OTP_ROOT`, if set. It must name an
///   existing **directory**; anything else (a file, a path that is not there, an empty value)
///   is a loud [`Error::Env`], never a silent fallback.
/// * `manifest_dir` — the crate directory (`$CARGO_MANIFEST_DIR`), used for the derivation
///   when there is no override.
pub fn otp_root_with(
    override_root: Option<OsString>,
    manifest_dir: &Path,
) -> Result<Option<PathBuf>> {
    if let Some(value) = override_root {
        let root = PathBuf::from(&value);
        if !root.is_dir() {
            return Err(Error::env(format!(
                "${OTP_ROOT_ENV} is set to '{}', which is not an existing directory",
                root.display()
            )));
        }
        return Ok(Some(root));
    }
    Ok(otp_root_from(manifest_dir))
}

/// Spawn `spec.program` with `argv` and the inherited environment, with `stdin` closed,
/// and capture its output.
pub fn run(spec: &RuntimeSpec, argv: &[String]) -> Result<Observation> {
    run_with_env(spec, argv, &[])
}
/// [`run`] with an explicit child **environment** (contract A.6).
///
/// Each `(name, value)` is set on the child *in addition to* the inherited environment —
/// the harness never clears it, so the runtime's own resolution (`$PATH`, `$TMPDIR`, …)
/// stays undisturbed. This is how an `ERL_*FLAGS` corpus case
/// ([`CaseKind::EvalEnv`](crate::corpus::CaseKind::EvalEnv),
/// [`CaseKind::ModuleEnv`](crate::corpus::CaseKind::ModuleEnv)) pins the environment-variable
/// merge — `[ERL_OTP<sysver>_FLAGS] [ERL_AFLAGS] [command line] [ERL_FLAGS] [ERL_ZFLAGS]`
/// — against the oracle: both runtimes are spawned with the identical argv **and** the
/// identical environment, so only their behavior can differ.
///
/// The pairs are **owned** (`String`) because a module-run case's value may embed the
/// compiled module's temp directory, which is only known at run time
/// ([`crate::corpus::expand_env`]); [`own_env`] converts a case's `'static` slice.
///
/// The child's working directory is the OTP tree root ([`otp_root`]) — **not** the harness's
/// own: the contract-A.9 `selfhost*` cases address the tree repo-relatively, so the child must
/// see the tree no matter where the harness was invoked from. A root that cannot be derived
/// leaves the child inheriting the harness's directory (the pre-existing behaviour).
pub fn run_with_env(
    spec: &RuntimeSpec,
    argv: &[String],
    env: &[(String, String)],
) -> Result<Observation> {
    let root = otp_root()?;
    run_with_env_in(root.as_deref(), spec, argv, env)
}

/// [`run_with_env`] with the child's working directory passed in explicitly (the injectable
/// core, so a test can observe the cwd without touching the process-wide environment).
///
/// `root` is the OTP tree root the child runs in; `None` means "inherit the harness's own
/// working directory" (what a binary that was relocated out of its tree does).
pub(crate) fn run_with_env_in(
    root: Option<&Path>,
    spec: &RuntimeSpec,
    argv: &[String],
    env: &[(String, String)],
) -> Result<Observation> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(root) = root {
        cmd.current_dir(root);
    }
    for (name, value) in env {
        cmd.env(name, value);
    }
    let output = output_with_retry(&mut cmd).map_err(|source| {
        Error::io(
            format!(
                "failed to execute the {} runtime '{}' with argv {:?}",
                spec.id.id(),
                spec.program.display(),
                argv
            ),
            source,
        )
    })?;

    Ok(Observation {
        runtime: spec.id,
        program: spec.program.clone(),
        argv: argv.to_vec(),
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code: output.status.code(),
    })
}

/// Own the `(name, value)` pairs of a case's `'static` environment slice, so it can be
/// handed to [`run_with_env`].
///
/// A [`CaseKind::EvalEnv`](crate::corpus::CaseKind::EvalEnv) or
/// [`CaseKind::ModuleEnv`](crate::corpus::CaseKind::ModuleEnv) case stores its environment as
/// `&'static [(&'static str, &'static str)]` (so a [`crate::corpus::Case`] stays `Copy`);
/// [`run_with_env`] takes owned pairs. A `ModuleEnv` value that embeds the compiled module's
/// temp dir goes through [`crate::corpus::expand_env`] instead, which substitutes the
/// directory at the same time.
pub fn own_env(env: &[(&str, &str)]) -> Vec<(String, String)> {
    env.iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect()
}

/// How long a two-VM case's **peer leg** may take to print its ready line.
///
/// It is generous (a peer is a whole runtime boot) but **bounded**: the wait is what keeps a
/// broken peer from hanging the suite, so it must never be unbounded.
pub const PEER_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How often the bounded ready wait re-reads the peer's stdout.
const PEER_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// How much of a failed peer's captured output is kept in the skip reason.
const PEER_DETAIL_MAX: usize = 2000;

/// The outcome of starting one side's **peer leg** (see [`crate::corpus::PeerRun`]).
#[derive(Debug)]
pub enum PeerStart {
    /// The peer is up: it printed its ready line, and it is killed + reaped when the returned
    /// [`PeerProcess`] is dropped.
    Ready(PeerProcess),
    /// The peer never became ready. This is a **loud skip reason** — never a pass, never
    /// agreement — carrying what the peer printed and why the wait ended.
    NotReady(String),
}

/// One side's running **peer leg**: a second process of that side's own runtime.
///
/// The peer exists only for the duration of the side's own run: dropping this value kills and
/// reaps it (and removes its scratch directory), so **every** path — including an error or a
/// panic in the caller — leaves no stray VM behind. Each peer is started in its **own process
/// group** (an Erlang VM signals its own process group) so it can never signal the harness.
#[derive(Debug)]
pub struct PeerProcess {
    child: Child,
    /// The scratch directory holding the peer's captured stdout/stderr (removed on drop).
    dir: PathBuf,
    /// The ready line the peer printed (kept for diagnostics and tests).
    ready_line: String,
}

impl PeerProcess {
    /// The peer's process id (diagnostics; the harness itself never needs it).
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The stdout line the peer printed to announce that it is up.
    pub fn ready_line(&self) -> &str {
        &self.ready_line
    }
}

impl Drop for PeerProcess {
    fn drop(&mut self) {
        // A peer that already exited (it halts itself in the normal flow) is only reaped;
        // one that is still running is killed first. Either way the directory goes away.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Start `spec.program` with `argv` as one side's **peer leg** and wait, **bounded** by
/// `timeout`, for it to print a line starting with `ready_prefix`.
///
/// The peer runs in its own process group with its stdin closed and its stdout/stderr
/// redirected to files in a private scratch directory: readiness is *polled* from the file, so
/// nothing in this path can block (no reader thread has to outlive the case), and the peer's
/// own output never reaches the harness's stdout/stderr. A peer that exits before it is ready,
/// or that is still silent when the timeout expires, yields [`PeerStart::NotReady`] naming the
/// argv, what it printed and how the wait ended — a loud skip, never a fabricated pass.
pub fn spawn_peer(
    spec: &RuntimeSpec,
    argv: &[String],
    ready_prefix: &str,
    timeout: std::time::Duration,
) -> PeerStart {
    let dir = match peer_scratch_dir() {
        Ok(dir) => dir,
        Err(reason) => return PeerStart::NotReady(reason),
    };
    let stdout_path = dir.join("stdout");
    let stderr_path = dir.join("stderr");
    let mut child = match start_peer_child(spec, argv, &stdout_path, &stderr_path) {
        Ok(child) => child,
        Err(reason) => {
            let _ = std::fs::remove_dir_all(&dir);
            return PeerStart::NotReady(reason);
        }
    };

    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(line) = ready_line_in(&stdout_path, ready_prefix) {
            return PeerStart::Ready(PeerProcess {
                child,
                dir,
                ready_line: line,
            });
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let situation = format!(
                    "exited ({}) before printing its ready line ('{ready_prefix}')",
                    exit_description(&status)
                );
                return abandon_peer(&mut child, &dir, &peer_skip_reason(spec, argv, &situation, &stdout_path, &stderr_path));
            }
            Ok(None) => {}
            Err(err) => {
                let situation = format!("could not be polled: {err}");
                return abandon_peer(&mut child, &dir, &peer_skip_reason(spec, argv, &situation, &stdout_path, &stderr_path));
            }
        }
        if std::time::Instant::now() >= deadline {
            let situation = format!(
                "printed no ready line ('{ready_prefix}') within {} ms",
                timeout.as_millis()
            );
            return abandon_peer(&mut child, &dir, &peer_skip_reason(spec, argv, &situation, &stdout_path, &stderr_path));
        }
        std::thread::sleep(PEER_POLL);
    }
}

/// A per-invocation unique suffix for the node names of a two-VM case's peer leg.
///
/// `epmd`'s name space is **machine-global** and shared with every other VM on the host —
/// including other worktrees and the 16-way parallel corpus sweep — so a corpus case must
/// never use a fixed node name. The suffix folds in a short hash of the case id (so the names
/// are recognisable in `epmd -names`), the process id and a process-wide counter (so two runs
/// *in the same process* differ), and the side (the oracle and the Rust leg of one case get
/// different names, so a name is never reused before its previous owner has unregistered).
///
/// The result is lowercase alphanumeric only, which `-sname` (and an Erlang atom in a `-eval`
/// expression) requires.
pub fn peer_nonce(case_id: &str, side: RuntimeId) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tag = match side {
        RuntimeId::Oracle => 'o',
        RuntimeId::Rust => 'r',
    };
    format!(
        "{}{:x}{:x}{tag}",
        case_id_tag(case_id),
        std::process::id(),
        seq
    )
}

/// A fixed-width lowercase-hex tag derived from a case id (the readable part of [`peer_nonce`]).
fn case_id_tag(case_id: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in case_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{:06x}", hash & 0x00ff_ffff)
}

/// Create the private scratch directory of one peer leg.
fn peer_scratch_dir() -> std::result::Result<PathBuf, String> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "differential-peer-{}-{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|err| {
        format!(
            "cannot create the peer leg's scratch directory '{}': {err}",
            dir.display()
        )
    })?;
    Ok(dir)
}

/// Spawn the peer child with its stdin closed and its stdout/stderr going to `stdout_path`
/// and `stderr_path`.
///
/// The peer runs in the OTP tree root ([`otp_root`]), exactly like a directly observed child
/// ([`run_with_env`]): both legs of a case must see the same working directory, or a
/// tree-relative `selfhost*` subject would resolve differently on the two sides. An
/// unresolvable root (`$DIFFERENTIAL_OTP_ROOT` naming no directory) is a loud skip reason, not
/// a peer started in an arbitrary place.
fn start_peer_child(
    spec: &RuntimeSpec,
    argv: &[String],
    stdout_path: &Path,
    stderr_path: &Path,
) -> std::result::Result<Child, String> {
    let reason = |what: &str, err: &dyn std::fmt::Display| {
        format!(
            "cannot start the peer leg of the {} runtime '{}' with argv {:?}: {what}: {err}",
            spec.id.id(),
            spec.program.display(),
            argv
        )
    };
    let stdout = std::fs::File::create(stdout_path)
        .map_err(|err| reason(&format!("cannot create '{}'", stdout_path.display()), &err))?;
    let stderr = std::fs::File::create(stderr_path)
        .map_err(|err| reason(&format!("cannot create '{}'", stderr_path.display()), &err))?;

    let mut cmd = Command::new(&spec.program);
    cmd.args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    // Same working directory as a directly observed child: the OTP tree root (see
    // `start_peer_child`'s doc comment).
    match otp_root() {
        Ok(Some(root)) => {
            cmd.current_dir(root);
        }
        Ok(None) => {}
        Err(err) => return Err(reason("cannot resolve the OTP tree root", &err)),
    }    // An Erlang VM signals its own **process group** (SIGUSR1 for the break handler, and
    // `erl_child_setup` on the way down), so the peer gets a group of its own: it can never
    // signal the harness or a sibling case.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    spawn_with_retry(&mut cmd).map_err(|err| reason("spawn failed", &err))
}

/// Kill and reap a peer that will not be used, remove its scratch directory, and answer the
/// loud skip reason. Used on every failed [`spawn_peer`] path, so no stray VM is left behind.
fn abandon_peer(child: &mut Child, dir: &Path, reason: &str) -> PeerStart {
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(dir);
    PeerStart::NotReady(reason.to_string())
}

/// The loud skip reason for a peer leg that never became ready.
fn peer_skip_reason(
    spec: &RuntimeSpec,
    argv: &[String],
    situation: &str,
    stdout_path: &Path,
    stderr_path: &Path,
) -> String {
    format!(
        "the peer leg of the {} runtime '{}' {situation}; argv {:?}; peer stdout {:?}; peer \
         stderr {:?}",
        spec.id.id(),
        spec.program.display(),
        argv,
        captured_tail(stdout_path),
        captured_tail(stderr_path),
    )
}

/// The tail of a captured peer stream, for a skip reason (never fabricated: a missing or
/// unreadable file is reported as such).
fn captured_tail(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            tail(text.trim_end(), PEER_DETAIL_MAX).to_string()
        }
        Err(err) => format!("<unreadable: {err}>"),
    }
}

/// The first **complete** line of `path` that starts with `prefix`.
///
/// Only newline-terminated lines are considered: a peer that is mid-write must not satisfy the
/// prefix with half a line (readiness has to be a whole line the peer really printed).
fn ready_line_in(path: &Path, prefix: &str) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let complete = text.rsplit_once('\n')?.0;
    complete
        .lines()
        .find(|line| line.starts_with(prefix))
        .map(str::to_string)
}

/// The exit status of a peer, as it goes into a skip reason.
fn exit_description(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit status {code}"),
        None => "a signal".to_string(),
    }
}

/// The file names tried when scanning `$PATH` for the oracle.
fn oracle_program_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["erl.exe", "erl.cmd"]
    }
    #[cfg(not(windows))]
    {
        &["erl"]
    }
}

/// The file name of the Rust runtime binary inside `target/<profile>/`.
fn rust_program_name() -> &'static str {
    #[cfg(windows)]
    {
        "beam_rt.exe"
    }
    #[cfg(not(windows))]
    {
        "beam_rt"
    }
}

/// The file name of the distribution helper binary inside `target/<profile>/`.
fn helper_program_name() -> &'static str {
    #[cfg(windows)]
    {
        "dist_probe.exe"
    }
    #[cfg(not(windows))]
    {
        "dist_probe"
    }
}

#[cfg(unix)]
pub(crate) fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    match std::fs::metadata(path) {
        Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
pub(crate) fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::{
        case_id_tag, eval_argv, eval_argv_owned, is_transient_text_file_busy, otp_root,
        otp_root_from, otp_root_with, output_with_retry, own_env, peer_nonce, ready_line_in,
        resolve_helper_from, resolve_oracle_from, resolve_rust_from, run, run_with_env,
        run_with_env_in, spawn_peer, PeerStart, RuntimeId, RuntimeSource, RuntimeSpec,
        OTP_ROOT_ENV,
    };
    use crate::error::Error;
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    /// A scratch directory that removes itself when dropped.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "differential-runtime-{}-{}-{:?}",
                std::process::id(),
                tag,
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Scratch { dir }
        }

        fn file(&self, relative: &str) -> PathBuf {
            let path = self.dir.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create parent dir");
            }
            std::fs::write(&path, b"#!/bin/sh\nexit 0\n").expect("write scratch file");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn executable(dir: &Scratch, relative: &str) -> PathBuf {
        let path = dir.file(relative);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }
        path
    }

    #[test]
    fn run_with_env_sets_the_child_environment_and_keeps_the_inherited_one() {
        // A short-lived `sh` is the portable way to observe the child environment: it
        // prints the variable the harness set, and an empty line when it is unset.
        #[cfg(unix)]
        {
            let spec = RuntimeSpec {
                id: RuntimeId::Rust,
                program: PathBuf::from("sh"),
                source: RuntimeSource::Path,
            };
            let argv = vec![
                "-c".to_string(),
                String::from("printf %s \"$DIFFERENTIAL_RUN_ENV_PROBE\""),
            ];
            let with_env = run_with_env(
                &spec,
                &argv,
                &[("DIFFERENTIAL_RUN_ENV_PROBE".to_string(), "hello-env".to_string())],
            )
            .expect("spawn sh with the probe variable");
            assert_eq!(with_env.exit_code, Some(0));
            assert_eq!(with_env.stdout, b"hello-env");
            assert_eq!(
                with_env.argv, argv,
                "the observed argv is recorded verbatim"
            );

            // `run` is `run_with_env(…, &[])`: the variable is unset, so the shell prints
            // nothing — the inherited environment is otherwise untouched.
            let without_env = run(&spec, &argv).expect("spawn sh without the probe variable");
            assert_eq!(without_env.stdout, b"");
        }
    }

    /// A `RuntimeSpec` for a portable "print my working directory" child (`/bin/sh` is what
    /// the crate's other spawn tests use too; `pwd -P` ignores an inherited `$PWD`).
    #[cfg(unix)]
    fn pwd_spec() -> (RuntimeSpec, Vec<String>) {
        (
            RuntimeSpec {
                id: RuntimeId::Rust,
                program: PathBuf::from("/bin/sh"),
                source: RuntimeSource::Path,
            },
            vec!["-c".to_string(), "pwd -P".to_string()],
        )
    }

    /// The stdout of a `pwd -P` child, resolved through the filesystem (so a symlinked
    /// `$TMPDIR` cannot make a correct child look wrong).
    #[cfg(unix)]
    fn child_cwd(observation: &super::Observation) -> PathBuf {
        assert_eq!(observation.exit_code, Some(0), "child must succeed");
        let printed = String::from_utf8(observation.stdout.clone()).expect("pwd prints utf-8");
        let printed = printed.trim_end_matches('\n');
        std::fs::canonicalize(printed)
            .unwrap_or_else(|err| panic!("cannot canonicalize the child's cwd '{printed}': {err}"))
    }

    #[test]
    fn the_otp_tree_root_is_the_third_parent_of_the_crate_directory() {
        // The crate sits at `<root>/erts_rust/tools/differential`, so `$ERL_TOP` is exactly
        // three parents up: differential → tools → erts_rust → root.
        let scratch = Scratch::new("otp-root-derive");
        let manifest_dir = scratch.dir.join("erts_rust/tools/differential");
        std::fs::create_dir_all(&manifest_dir).expect("create a fake crate directory");
        assert_eq!(
            otp_root_from(&manifest_dir),
            Some(scratch.dir.clone()),
            "three parents above the crate directory is the tree root"
        );

        // The real crate directory derives this checkout's own `$ERL_TOP`, which is where
        // `lib/stdlib/src` (the tree-relative selfhost subject) lives.
        let real_manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let derived = otp_root_from(real_manifest).expect("this crate always sits in its tree");
        assert_eq!(
            derived,
            real_manifest
                .ancestors()
                .nth(3)
                .expect("the crate is three levels below $ERL_TOP"),
            "the derivation walks exactly three parents up from $CARGO_MANIFEST_DIR"
        );
        assert!(
            derived.join("lib/stdlib/src/ordsets.erl").is_file(),
            "'{}' must be the OTP tree root (it has to contain the tree-relative selfhost \
             subjects)",
            derived.display()
        );
    }

    #[test]
    fn a_missing_or_shallow_derivation_yields_no_root() {
        // A derived root that is not there: the manifest directory need not even exist, only
        // the *derived* directory is checked.
        let scratch = Scratch::new("otp-root-absent");
        let absent = scratch.dir.join("gone/erts_rust/tools/differential");
        assert_eq!(
            otp_root_from(&absent),
            None,
            "a derived root that does not exist is not used (the child then inherits the cwd)"
        );

        // Too shallow to have three parents at all.
        assert_eq!(otp_root_from(Path::new("erts_rust/tools")), None);
    }

    #[test]
    fn the_otp_root_override_wins_and_must_name_an_existing_directory() {
        let scratch = Scratch::new("otp-root-override");
        let real_manifest = Path::new(env!("CARGO_MANIFEST_DIR"));

        // An existing directory wins over the derivation, even a derived-from candidate that
        // would have resolved to this checkout's own root.
        assert_eq!(
            otp_root_with(Some(scratch.dir.clone().into_os_string()), real_manifest)
                .expect("the override is an existing directory"),
            Some(scratch.dir.clone())
        );

        // A non-existent path is a loud env error (exit 2), never a silent fallback.
        let err = otp_root_with(
            Some(OsString::from("/nonexistent/differential-otp-root")),
            real_manifest,
        )
        .expect_err("a missing override must not resolve");
        assert!(matches!(err, Error::Env(_)), "{err:?}");
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.to_string().contains(OTP_ROOT_ENV),
            "the error names the variable: {err}"
        );

        // A *file* is not a directory either.
        let file = scratch.file("not-a-directory");
        let err = otp_root_with(Some(file.into_os_string()), real_manifest)
            .expect_err("a file is not a tree root");
        assert!(matches!(err, Error::Env(_)), "{err:?}");
        assert!(err.to_string().contains(OTP_ROOT_ENV), "{err}");
    }

    /// The spawn itself honours the root: a child gets the OTP tree root as its working
    /// directory, never the directory the harness process happens to run in.
    #[cfg(unix)]
    #[test]
    fn a_spawned_child_runs_in_the_otp_tree_root_not_the_harness_cwd() {
        let scratch = Scratch::new("otp-root-spawn");
        let root = scratch.dir.join("otp-tree");
        std::fs::create_dir_all(root.join("lib/stdlib/src")).expect("create the fake tree root");
        let (spec, argv) = pwd_spec();

        let observed =
            run_with_env_in(Some(&root), &spec, &argv, &[]).expect("spawn the pwd child");
        assert_eq!(
            child_cwd(&observed),
            std::fs::canonicalize(&root).expect("canonicalize the fake root"),
            "the child's cwd is the tree root it was given"
        );

        // The harness's own working directory is *not* what the child saw: the spawn must not
        // depend on where the harness was invoked from. (The test process's cwd is shared with
        // every parallel test, so it is never mutated here — only compared.)
        let harness_cwd = std::env::current_dir().expect("the harness has a working directory");
        assert_ne!(
            child_cwd(&observed),
            std::fs::canonicalize(&harness_cwd).expect("canonicalize the harness cwd"),
            "the child must not inherit the harness's working directory"
        );

        // No root ⇒ the pre-existing behaviour: the child inherits the harness's directory.
        let inherited = run_with_env_in(None, &spec, &argv, &[]).expect("spawn without a root");
        assert_eq!(
            child_cwd(&inherited),
            std::fs::canonicalize(&harness_cwd).expect("canonicalize the harness cwd"),
            "an unresolvable root degrades to inheriting the harness's cwd"
        );
    }

    /// [`run_with_env`] (the real spawn path) wires the **derived** root in — the property the
    /// contract-A.9 tree-relative cases depend on when `cargo test` runs the harness from the
    /// crate directory instead of `$ERL_TOP`.
    #[cfg(unix)]
    #[test]
    fn run_with_env_spawns_the_child_in_the_resolved_otp_tree_root() {
        if std::env::var_os(OTP_ROOT_ENV).is_some() {
            eprintln!("SKIP: ${OTP_ROOT_ENV} is set in this environment — the derivation is not used");
            return;
        }
        let root = otp_root().expect("resolution cannot fail without an override").expect(
            "this crate always sits in its tree, so a root is always derivable under cargo test",
        );
        let (spec, argv) = pwd_spec();
        let observed = run_with_env(&spec, &argv, &[]).expect("spawn the pwd child");
        assert_eq!(
            child_cwd(&observed),
            std::fs::canonicalize(&root).expect("canonicalize the derived tree root"),
            "run_with_env must run the child in the derived OTP tree root"
        );
    }

    #[test]
    fn own_env_copies_the_pairs_verbatim() {
        assert_eq!(
            own_env(&[("ERL_AFLAGS", "-pa {dir}"), ("ERL_ZFLAGS", "+S 2")]),
            vec![
                ("ERL_AFLAGS".to_string(), "-pa {dir}".to_string()),
                ("ERL_ZFLAGS".to_string(), "+S 2".to_string()),
            ],
            "the placeholder is left for the harness to expand"
        );
        assert!(own_env(&[]).is_empty());
    }

    #[test]
    fn eval_argv_matches_the_drop_in_erl_shape() {
        assert_eq!(
            eval_argv("EXPR", &[]),
            vec!["-noshell", "-eval", "EXPR"],
            "the expression-case probe argv is the drop-in erl shape"
        );
        assert_eq!(
            eval_argv("EXPR", &["-differential_test_flag", "hello"]),
            vec![
                "-noshell",
                "-differential_test_flag",
                "hello",
                "-eval",
                "EXPR"
            ]
        );
        assert_eq!(
            eval_argv_owned("E", &["-pa".to_string(), "/tmp/ebin".to_string()]),
            vec!["-noshell", "-pa", "/tmp/ebin", "-eval", "E"]
        );
    }

    #[test]
    fn oracle_env_override_is_used_when_it_exists() {
        let dir = Scratch::new("oracle-env");
        let erl = executable(&dir, "bin/erl");
        let spec = resolve_oracle_from(Some(erl.clone().into_os_string()), None).expect("resolve");
        assert_eq!(spec.id, RuntimeId::Oracle);
        assert_eq!(spec.program, erl);
        assert_eq!(spec.source, RuntimeSource::Env("$ERTS_ORACLE"));
    }

    #[test]
    fn oracle_env_override_that_does_not_exist_is_a_loud_env_error() {
        let err = resolve_oracle_from(
            Some(OsString::from("/nonexistent/erl-differential-test")),
            None,
        )
        .expect_err("must not resolve");
        assert!(matches!(err, Error::Env(_)), "{err:?}");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("$ERTS_ORACLE"));
    }

    #[test]
    fn oracle_falls_back_to_scanning_path() {
        let dir = Scratch::new("oracle-path");
        let erl = executable(&dir, "erl");
        let other = dir.dir.join("nothing-here");
        let path = std::env::join_paths([other, dir.dir.clone()]).expect("join paths");
        let spec = resolve_oracle_from(None, Some(path)).expect("resolve");
        assert_eq!(spec.program, erl);
        assert_eq!(spec.source, RuntimeSource::Path);
    }

    #[test]
    fn oracle_without_env_or_path_is_a_loud_env_error() {
        let dir = Scratch::new("oracle-none");
        let path = std::env::join_paths([dir.dir.clone()]).expect("join paths");
        let err = resolve_oracle_from(None, Some(path))
            .expect_err("must not resolve a non-existing erl");
        assert!(matches!(err, Error::Env(_)), "{err:?}");
        assert!(err.to_string().contains("$ERTS_ORACLE is unset"));
    }

    #[test]
    fn rust_env_override_is_used_when_it_exists() {
        let dir = Scratch::new("rust-env");
        let beam_rt = dir.file("beam_rt");
        let spec = resolve_rust_from(Some(beam_rt.clone().into_os_string()), &dir.dir)
            .expect("resolve")
            .expect("present");
        assert_eq!(spec.id, RuntimeId::Rust);
        assert_eq!(spec.program, beam_rt);
        assert_eq!(spec.source, RuntimeSource::Env("$BEAM_RT"));
    }

    #[test]
    fn rust_env_override_that_does_not_exist_is_a_loud_env_error() {
        let dir = Scratch::new("rust-env-missing");
        let err = resolve_rust_from(Some(OsString::from("/nonexistent/beam_rt")), &dir.dir)
            .expect_err("must not resolve");
        assert!(matches!(err, Error::Env(_)), "{err:?}");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("$BEAM_RT"));
    }

    #[test]
    fn rust_defaults_to_the_workspace_target_and_is_absent_when_not_built() {
        let dir = Scratch::new("rust-default");
        let manifest_dir = dir.dir.join("erts_rust/tools/differential");
        std::fs::create_dir_all(&manifest_dir).expect("create manifest dir");
        assert_eq!(resolve_rust_from(None, &manifest_dir).expect("resolve"), None);

        let beam_rt = dir.file("erts_rust/target/debug/beam_rt");
        let spec = resolve_rust_from(None, &manifest_dir)
            .expect("resolve")
            .expect("present");
        assert_eq!(spec.program, beam_rt);
        assert_eq!(spec.source, RuntimeSource::WorkspaceDefault);

        // release wins over debug.
        let release = dir.file("erts_rust/target/release/beam_rt");
        let spec = resolve_rust_from(None, &manifest_dir)
            .expect("resolve")
            .expect("present");
        assert_eq!(spec.program, release);
    }

    #[test]
    fn helper_env_override_is_used_when_it_exists() {
        let dir = Scratch::new("helper-env");
        let probe = dir.file("dist_probe");
        let spec = resolve_helper_from(Some(probe.clone().into_os_string()), &dir.dir)
            .expect("resolve")
            .expect("present");
        assert_eq!(spec.id, RuntimeId::Rust);
        assert_eq!(spec.program, probe);
        assert_eq!(spec.source, RuntimeSource::Env("$BEAM_DIST_PROBE"));
    }

    #[test]
    fn helper_env_override_that_does_not_exist_is_a_loud_env_error() {
        let dir = Scratch::new("helper-env-missing");
        let err = resolve_helper_from(Some(OsString::from("/nonexistent/dist_probe")), &dir.dir)
            .expect_err("must not resolve");
        assert!(matches!(err, Error::Env(_)), "{err:?}");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("$BEAM_DIST_PROBE"));
    }

    #[test]
    fn helper_defaults_to_the_workspace_target_and_is_absent_when_not_built() {
        let dir = Scratch::new("helper-default");
        let manifest_dir = dir.dir.join("erts_rust/tools/differential");
        std::fs::create_dir_all(&manifest_dir).expect("create manifest dir");
        assert_eq!(resolve_helper_from(None, &manifest_dir).expect("resolve"), None);

        let probe = dir.file("erts_rust/target/debug/dist_probe");
        let spec = resolve_helper_from(None, &manifest_dir)
            .expect("resolve")
            .expect("present");
        assert_eq!(spec.program, probe);
        assert_eq!(spec.source, RuntimeSource::WorkspaceDefault);

        // release wins over debug.
        let release = dir.file("erts_rust/target/release/dist_probe");
        let spec = resolve_helper_from(None, &manifest_dir)
            .expect("resolve")
            .expect("present");
        assert_eq!(spec.program, release);
    }

    #[test]
    fn the_helper_unsupported_reason_names_the_override_and_the_binary() {
        let reason = super::helper_unsupported_reason();
        assert!(reason.contains("$BEAM_DIST_PROBE"), "{reason}");
        assert!(reason.contains("dist_probe"), "{reason}");
    }

    /// The build policy: a resolved helper always wins, the build is a last resort, and
    /// `$DIFFERENTIAL_NO_BUILD` (set and non-empty) refuses it.
    ///
    /// These are the decisions [`super::ensure_helper`] makes; the build itself is never
    /// exercised here (a unit test must not invoke cargo).
    #[test]
    fn a_resolved_helper_is_never_replaced_by_an_on_demand_build() {
        use super::{helper_plan, HelperPlan};
        let opt_out = OsString::from("1");
        assert_eq!(helper_plan(true, None), HelperPlan::UseResolved);
        assert_eq!(
            helper_plan(true, Some(opt_out.as_os_str())),
            HelperPlan::UseResolved,
            "the opt-out is irrelevant when a helper is already resolvable"
        );
    }

    #[test]
    fn the_on_demand_build_runs_only_without_a_helper_and_without_the_opt_out() {
        use super::{helper_plan, no_build_requested, HelperPlan};
        assert_eq!(
            helper_plan(false, None),
            HelperPlan::Build,
            "no helper and no opt-out ⇒ build it (once)"
        );
        assert_eq!(
            helper_plan(false, Some(OsStr::new(""))),
            HelperPlan::Build,
            "an empty $DIFFERENTIAL_NO_BUILD is 'not set'"
        );
        assert_eq!(
            helper_plan(false, Some(OsStr::new("1"))),
            HelperPlan::BuildDisabled
        );
        assert_eq!(
            helper_plan(false, Some(OsStr::new("yes-anything"))),
            HelperPlan::BuildDisabled,
            "any non-empty value switches the build off"
        );

        assert!(!no_build_requested(None));
        assert!(!no_build_requested(Some(OsStr::new(""))));
        assert!(no_build_requested(Some(OsStr::new(" "))));
        assert!(no_build_requested(Some(OsStr::new("0"))));
    }

    /// A multi-byte character straddling the cut is dropped whole, never split — the tail is
    /// always a valid `&str`.
    #[test]
    fn tail_keeps_the_end_of_the_text_on_a_char_boundary() {
        use super::tail;
        assert_eq!(tail("abc", 8), "abc", "short text is returned unchanged");
        assert_eq!(tail("abcdef", 3), "def");
        let umlauts = "äöü"; // two bytes per character
        assert_eq!(tail(umlauts, 5), "öü");
        assert_eq!(tail(umlauts, 3), "ü");
    }

    /// A failed build must be reported with cargo's own diagnosis (the **tail** of its stderr,
    /// where the diagnosis sits) and with a bounded reason: loud, never silent, never huge.
    #[cfg(unix)]
    #[test]
    fn a_failed_build_is_reported_with_the_bounded_tail_of_cargos_diagnosis() {
        use super::{build_failure_detail, BUILD_DETAIL_MAX};
        use std::os::unix::process::ExitStatusExt;

        let stderr = format!("{}\ndiagnosis: missing dependency\n", "x".repeat(4000));
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(101 << 8),
            stdout: Vec::new(),
            stderr: stderr.clone().into_bytes(),
        };
        let detail = build_failure_detail(&output);
        assert!(
            detail.starts_with("`cargo build -p beam_dist` exited 101: "),
            "{detail}"
        );
        assert!(
            detail.ends_with("diagnosis: missing dependency"),
            "the tail (where cargo puts the diagnosis) is kept: {detail}"
        );
        assert!(
            detail.len() < stderr.len(),
            "the reason is bounded: {} bytes for a {}-byte stderr",
            detail.len(),
            stderr.len()
        );
        assert!(BUILD_DETAIL_MAX < stderr.len());

        let silent = std::process::Output {
            status: std::process::ExitStatus::from_raw(3 << 8),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert_eq!(
            build_failure_detail(&silent),
            "`cargo build -p beam_dist` exited 3 with no stderr",
            "a silent failure is still explicit about the exit status"
        );
    }

    #[test]
    fn run_captures_stdout_stderr_and_exit_status() {
        let dir = Scratch::new("run");
        let script = executable(&dir, "probe");
        std::fs::write(&script, b"#!/bin/sh\necho out\necho err 1>&2\nexit 7\n")
            .expect("write script");
        let spec = RuntimeSpec {
            id: RuntimeId::Rust,
            program: script.clone(),
            source: RuntimeSource::WorkspaceDefault,
        };
        let argv = vec!["-noshell".to_string(), "-eval".to_string()];
        let observation = run(&spec, &argv).expect("run");
        assert_eq!(observation.runtime, RuntimeId::Rust);
        assert_eq!(observation.program, script);
        assert_eq!(observation.argv, argv);
        assert_eq!(observation.stdout, b"out\n");
        assert_eq!(observation.stderr, b"err\n");
        assert_eq!(observation.exit_code, Some(7));
    }

    #[test]
    fn run_reports_a_missing_program_as_an_io_error() {
        let spec = RuntimeSpec {
            id: RuntimeId::Oracle,
            program: PathBuf::from("/nonexistent/erl-differential-test"),
            source: RuntimeSource::Path,
        };
        let err = run(&spec, &["-noshell".to_string()]).expect_err("must fail to spawn");
        assert!(matches!(err, Error::Io { .. }), "{err:?}");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("oracle"));
    }

    #[test]
    fn output_with_retry_passes_a_successful_run_through_unchanged() {
        let dir = Scratch::new("retry-ok");
        let script = executable(&dir, "probe");
        std::fs::write(&script, b"#!/bin/sh\necho out\nexit 0\n").expect("write script");
        let mut cmd = Command::new(&script);
        let output = output_with_retry(&mut cmd).expect("spawn");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"out\n");
    }

    #[test]
    fn output_with_retry_surfaces_a_non_transient_error_loudly() {
        // A missing executable is not an ETXTBSY race: it must come straight back, so the
        // bounded retry can never mask (or delay) a genuine spawn failure.
        let mut cmd = Command::new("/nonexistent/erl-differential-retry-test");
        let err = output_with_retry(&mut cmd).expect_err("must fail to spawn");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err:?}");
    }

    #[test]
    fn is_transient_text_file_busy_matches_only_etxtbsy() {
        assert!(is_transient_text_file_busy(&std::io::Error::from(
            std::io::ErrorKind::ExecutableFileBusy
        )));
        assert!(is_transient_text_file_busy(
            &std::io::Error::from_raw_os_error(26)
        ));
        assert!(!is_transient_text_file_busy(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
        assert!(!is_transient_text_file_busy(
            &std::io::Error::from_raw_os_error(13)
        ));
    }

    /// Deterministically exercises the `ETXTBSY` path the retry exists for.
    ///
    /// Linux refuses to `exec` a file that **any** process (including this one) holds open
    /// for writing, with `ETXTBSY` — exactly the transient error a concurrent `fork()` can
    /// cause for a freshly written script. Holding the descriptor open therefore makes
    /// *every* attempt fail, which pins down two behaviors a timing-dependent test could
    /// not: the retry is **bounded** (it gives up and returns the error instead of spinning
    /// or swallowing it), and once the writer is gone the very same command runs.
    #[cfg(unix)]
    #[test]
    fn output_with_retry_is_bounded_on_etxtbsy_and_surfaces_it() {
        let dir = Scratch::new("retry-etxtbsy");
        let script = executable(&dir, "held-open");
        std::fs::write(&script, b"#!/bin/sh\necho out\nexit 0\n").expect("write script");
        let held = std::fs::OpenOptions::new()
            .write(true)
            .open(&script)
            .expect("open the script for writing");

        let mut cmd = Command::new(&script);
        let err = output_with_retry(&mut cmd).expect_err("a write-open file cannot be exec'd");
        assert!(
            is_transient_text_file_busy(&err),
            "expected ETXTBSY, got {err:?}"
        );

        drop(held);
        let output = output_with_retry(&mut cmd).expect("spawn once the writer closed");
        assert_eq!(output.stdout, b"out\n");
    }

    /// The retry must actually **recover** when the transient writer goes away.
    ///
    /// [`output_with_retry_is_bounded_on_etxtbsy_and_surfaces_it`] pins the "gives up
    /// loudly" half; this pins the "succeeds once the race resolves" half, which is what
    /// makes the suite deterministic under parallel load. The file is held open for
    /// writing by *another thread* (mirroring a concurrent `fork()` inheriting a writer's
    /// descriptor) and released while the spawn is already retrying, so on Linux the first
    /// attempts deterministically fail with `ETXTBSY` and a later attempt succeeds.
    #[cfg(unix)]
    #[test]
    fn output_with_retry_recovers_once_the_transient_writer_closes() {
        let dir = Scratch::new("retry-recover");
        let script = executable(&dir, "settle");
        std::fs::write(&script, b"#!/bin/sh\necho out\nexit 0\n").expect("write script");

        let held = std::fs::OpenOptions::new()
            .write(true)
            .open(&script)
            .expect("open the script for writing");
        let release = std::thread::spawn(move || {
            // Released well within the 155 ms bounded retry window.
            std::thread::sleep(std::time::Duration::from_millis(25));
            drop(held);
        });

        let mut cmd = Command::new(&script);
        let output = output_with_retry(&mut cmd)
            .expect("the bounded retry must recover once the writer closes");
        assert_eq!(output.stdout, b"out\n");
        release.join().expect("writer thread");
    }

    #[test]
    fn default_resolvers_read_the_real_environment() {
        // Not asserting success: the point is that they must not panic and that the
        // workspace default is derived from this crate's own manifest directory.
        let _ = super::resolve_oracle();
        let _ = super::resolve_rust();
        assert!(Path::new(env!("CARGO_MANIFEST_DIR")).is_dir());
    }

    /// A `sh` runtime spec: the portable way to drive a peer leg without a whole Erlang VM.
    fn sh_spec() -> RuntimeSpec {
        RuntimeSpec {
            id: RuntimeId::Rust,
            program: PathBuf::from("sh"),
            source: RuntimeSource::Path,
        }
    }

    #[test]
    fn case_id_tag_is_a_stable_six_char_lowercase_hex_tag() {
        let tag = case_id_tag("dist/live_self_pair_handshake_and_message");
        assert_eq!(tag.len(), 6, "fixed width keeps the node names short: {tag}");
        assert!(
            tag.chars()
                .all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch)),
            "lowercase hex only (a `-sname` name and an atom must accept it): {tag}"
        );
        assert_eq!(
            tag,
            case_id_tag("dist/live_self_pair_handshake_and_message"),
            "the tag is a pure function of the case id, so the names are recognisable"
        );
        assert_ne!(
            tag,
            case_id_tag("dist/live_handshake_ping"),
            "different cases must get different tags"
        );
    }

    #[test]
    fn peer_nonce_is_lowercase_alnum_and_unique_per_side_and_invocation() {
        let case_id = "dist/live_self_pair_handshake_and_message";
        let oracle = peer_nonce(case_id, RuntimeId::Oracle);
        let rust = peer_nonce(case_id, RuntimeId::Rust);
        let oracle_again = peer_nonce(case_id, RuntimeId::Oracle);
        assert!(
            oracle
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit()),
            "`-sname` and a bare atom accept only lowercase alphanumerics: {oracle}"
        );
        assert!(
            oracle.starts_with(&case_id_tag(case_id)),
            "the readable part identifies the case: {oracle}"
        );
        assert!(
            oracle.ends_with('o') && rust.ends_with('r'),
            "the two legs must never share a name: {oracle} vs {rust}"
        );
        assert_ne!(oracle, rust, "a side's names must differ");
        assert_ne!(
            oracle, oracle_again,
            "each invocation must be unique: epmd names are machine-global"
        );
    }

    #[test]
    fn ready_line_in_only_counts_a_complete_matching_line() {
        let dir = Scratch::new("ready-line");
        let stdout = dir.dir.join("peer-stdout");
        // A half-written line must never satisfy readiness.
        std::fs::write(&stdout, b"PEER-READY").expect("write a partial line");
        assert_eq!(ready_line_in(&stdout, "PEER-READY"), None);
        std::fs::write(&stdout, b"PEER-READY now\n").expect("write a full line");
        assert_eq!(
            ready_line_in(&stdout, "PEER-READY"),
            Some("PEER-READY now".to_string())
        );
        // The first matching complete line wins, noise before it is ignored …
        std::fs::write(&stdout, b"noise\nPEER-READY second\nmore\n").expect("write three lines");
        assert_eq!(
            ready_line_in(&stdout, "PEER-READY"),
            Some("PEER-READY second".to_string())
        );
        // … and a complete-but-different line is not readiness.
        std::fs::write(&stdout, b"BOOTED\n").expect("write another line");
        assert_eq!(ready_line_in(&stdout, "PEER-READY"), None);
        // A missing stream is simply "not ready" — never a fabricated line.
        assert_eq!(ready_line_in(&dir.dir.join("absent"), "PEER-READY"), None);
    }

    #[cfg(unix)]
    #[test]
    fn spawn_peer_accepts_a_ready_peer_and_reaps_it_on_drop() {
        let spec = sh_spec();
        let argv = vec![
            "-c".to_string(),
            String::from("echo 'PEER-READY one two'; sleep 30"),
        ];
        let peer = match spawn_peer(&spec, &argv, "PEER-READY", Duration::from_secs(20)) {
            PeerStart::Ready(peer) => peer,
            PeerStart::NotReady(reason) => panic!("the peer must become ready: {reason}"),
        };
        assert_eq!(peer.ready_line(), "PEER-READY one two");
        assert!(peer.pid() > 0, "the peer is a real child process");
        // Dropping kills + reaps: it must return promptly rather than block on the peer's
        // 30 s sleep. (The peer's own process group keeps that signal off the harness.)
        let started = std::time::Instant::now();
        drop(peer);
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "drop must not wait for the peer to finish on its own"
        );
    }

    /// A peer that outlived its usefulness — one that has already exited on its own — is left
    /// as an unreaped zombie until [`PeerProcess::drop`] collects it. `/proc` makes both halves
    /// observable: the zombie's entry exists before the drop and is gone after it.
    #[cfg(target_os = "linux")]
    #[test]
    fn dropping_an_exited_peer_reaps_it_instead_of_leaving_a_zombie() {
        let spec = sh_spec();
        let argv = vec![
            "-c".to_string(),
            String::from("echo PEER-READY; sleep 0.2; exit 7"),
        ];
        let peer = match spawn_peer(&spec, &argv, "PEER-READY", Duration::from_secs(20)) {
            PeerStart::Ready(peer) => peer,
            PeerStart::NotReady(reason) => panic!("the peer must become ready: {reason}"),
        };
        let entry = PathBuf::from(format!("/proc/{}", peer.pid()));
        // The script exits 0.2 s after it announced itself; the child is not waited on in
        // between, so its process entry survives as a zombie.
        std::thread::sleep(Duration::from_millis(1000));
        assert!(
            entry.exists(),
            "an unreaped child must still have a /proc entry: {}",
            entry.display()
        );
        drop(peer);
        assert!(
            !entry.exists(),
            "drop must reap the peer, not leave a zombie: {}",
            entry.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn spawn_peer_reports_a_silent_peer_as_not_ready_and_kills_it() {
        let spec = sh_spec();
        let argv = vec!["-c".to_string(), String::from("sleep 30")];
        let started = std::time::Instant::now();
        let reason = match spawn_peer(&spec, &argv, "PEER-READY", Duration::from_millis(250)) {
            PeerStart::Ready(peer) => panic!("a silent peer must never be ready: {peer:?}"),
            PeerStart::NotReady(reason) => reason,
        };
        assert!(
            reason.contains("printed no ready line ('PEER-READY')"),
            "the reason must name the wait that expired: {reason}"
        );
        assert!(
            reason.contains("sleep 30"),
            "the reason must name the peer's argv: {reason}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the ready wait is bounded, so a broken peer cannot hang the suite"
        );
    }

    #[cfg(unix)]
    #[test]
    fn spawn_peer_reports_an_early_exit_as_not_ready() {
        let spec = sh_spec();
        let argv = vec![
            "-c".to_string(),
            String::from("echo 'not the prefix'; exit 3"),
        ];
        let reason = match spawn_peer(&spec, &argv, "PEER-READY", Duration::from_secs(20)) {
            PeerStart::Ready(peer) => {
                panic!("a peer that exits before its ready line must never be ready: {peer:?}")
            }
            PeerStart::NotReady(reason) => reason,
        };
        assert!(
            reason.contains("exited (exit status 3) before printing its ready line"),
            "the reason must report the early exit: {reason}"
        );
        assert!(
            reason.contains("not the prefix"),
            "the peer's own output must be quoted, not fabricated: {reason}"
        );
    }

    #[test]
    fn spawn_peer_reports_an_unspawnable_program_as_not_ready() {
        let spec = RuntimeSpec {
            id: RuntimeId::Rust,
            program: PathBuf::from("/nonexistent/differential-peer-program"),
            source: RuntimeSource::Path,
        };
        let reason = match spawn_peer(&spec, &[], "PEER-READY", Duration::from_secs(5)) {
            PeerStart::Ready(peer) => panic!("a missing program cannot be ready: {peer:?}"),
            PeerStart::NotReady(reason) => reason,
        };
        assert!(
            reason.contains("spawn failed"),
            "the reason must report the spawn failure: {reason}"
        );
        assert!(
            reason.contains("/nonexistent/differential-peer-program"),
            "the reason must name the program: {reason}"
        );
    }
}
