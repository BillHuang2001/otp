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

//! Runtime resolution and self-check.
//!
//! `bench` drives two interchangeable runtimes as **drop-in `erl` binaries**
//! (contract A.6), never assuming internals:
//!
//! * the **oracle** (`sys`) — the real system Erlang: `$ERTS_ORACLE` if set, else
//!   `erl` resolved on `PATH`. It is mandatory for `run`/`baseline`, so a missing
//!   oracle is a loud [`BenchError::Env`] (process exit `2`);
//! * the **Rust runtime** (`rust`) — `$BEAM_RT` if set, else the workspace-relative
//!   `target/{release,debug}/beam_rt`. It is *optional*: when it cannot be resolved
//!   every rust-runtime cell degrades to `skipped(runtime-unsupported)`, never a
//!   crash and never a fabricated number.
//!
//! A runtime is considered *supported* only if it passes [`Runtime::self_check`] — the
//! canonical probe `-noshell -eval 'io:format("~p~n",[1+1]), halt().'` printing `2`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::error::{BenchError, Result};
use crate::proc;

/// Which runtime a result was produced by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    /// The system Erlang oracle.
    Sys,
    /// The Rust ERTS runtime (`beam_rt`).
    Rust,
}

impl RuntimeKind {
    /// The lowercase string used in the JSON report and the human table.
    pub fn as_str(&self) -> &'static str {
        match self {
            RuntimeKind::Sys => "sys",
            RuntimeKind::Rust => "rust",
        }
    }
}

/// The canonical self-check expression: must print exactly `2`.
pub const PROBE_EVAL: &str = "io:format(\"~p~n\",[1+1]), halt().";

/// A resolved (or explicitly absent) runtime: a kind plus the program path to run.
#[derive(Debug, Clone)]
pub struct Runtime {
    /// Which runtime this is.
    pub kind: RuntimeKind,
    /// The program to execute, or `None` when it could not be resolved.
    pub program: Option<PathBuf>,
}

impl Runtime {
    /// The program path, when resolved.
    pub fn program(&self) -> Option<&Path> {
        self.program.as_deref()
    }

    /// Runs the canonical probe, surfacing a genuine spawn error instead of hiding it.
    ///
    /// `Ok(false)` covers the non-error negative cases — the program is unresolved, the
    /// program path is absent (`NotFound`), or the program ran but did not print `2`. A
    /// genuine spawn failure (e.g. `PermissionDenied`, or an `ETXTBSY` that outlived all
    /// retries) is returned as `Err` so it is not silently swallowed.
    pub fn self_check_result(&self) -> std::io::Result<bool> {
        let Some(program) = &self.program else {
            return Ok(false);
        };
        let mut cmd = Command::new(program);
        cmd.args(["-noshell", "-eval", PROBE_EVAL]);
        match proc::run(&mut cmd) {
            Ok(out) => Ok(out.success && out.stdout_trim() == "2"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false), // genuine "missing program"
            Err(e) => Err(e),                                                // surfaced loudly
        }
    }

    /// Runs the canonical probe and returns whether it printed `2`.
    ///
    /// Returns `false` (never panics) when the program is unresolved or fails to run. A
    /// spawn error is treated as "not supported" here; callers that need to observe it
    /// should use [`Runtime::self_check_result`] instead.
    pub fn self_check(&self) -> bool {
        self.self_check_result().unwrap_or(false)
    }

    /// Best-effort version string (the OTP release, e.g. `29`).
    pub fn version(&self) -> Option<String> {
        let program = self.program.as_ref()?;
        let mut cmd = Command::new(program);
        cmd.args([
            "-noshell",
            "-eval",
            "io:format(\"~s\",[erlang:system_info(otp_release)]), halt().",
        ]);
        let out = proc::run(&mut cmd).ok()?;
        let v = out.stdout_trim();
        if v.is_empty() {
            None
        } else {
            Some(v.to_string())
        }
    }

    /// Builds the serializable [`RuntimeInfo`] for the report header.
    pub fn info(&self) -> RuntimeInfo {
        let supported = self.self_check();
        let mut notes = Vec::new();
        if self.program.is_none() {
            notes.push(match self.kind {
                RuntimeKind::Sys => "oracle not resolved ($ERTS_ORACLE / `erl` on PATH)".to_string(),
                RuntimeKind::Rust => {
                    "beam_rt not resolved (set $BEAM_RT or build the workspace)".to_string()
                }
            });
        } else if !supported {
            notes.push("self-check failed: runtime did not execute the canonical probe".to_string());
        }
        let version = if supported { self.version() } else { None };
        RuntimeInfo {
            kind: self.kind,
            program: self.program.as_ref().map(|p| p.display().to_string()),
            version,
            supported,
            notes,
        }
    }
}

/// Resolves the mandatory system-Erlang oracle.
///
/// `$ERTS_ORACLE` wins; otherwise `erl` is searched on `PATH`. An oracle that cannot be
/// *located* (unset and absent from `PATH`, or `$ERTS_ORACLE` pointing at a file that
/// does not exist) is a loud environment error (the binary maps it to exit code `2`).
pub fn resolve_oracle() -> Result<Runtime> {
    oracle_from(non_empty_env("ERTS_ORACLE"))
}

/// The env-independent core of [`resolve_oracle`], kept separate so it is testable
/// without mutating the process environment (which is racy under `cargo test`).
pub(crate) fn oracle_from(env_value: Option<String>) -> Result<Runtime> {
    if let Some(path) = env_value {
        let program = PathBuf::from(&path);
        if !program.is_file() {
            return Err(BenchError::env(format!(
                "$ERTS_ORACLE is set but does not exist: {path}"
            )));
        }
        return Ok(Runtime {
            kind: RuntimeKind::Sys,
            program: Some(program),
        });
    }
    match proc::which("erl") {
        Some(path) => Ok(Runtime {
            kind: RuntimeKind::Sys,
            program: Some(path),
        }),
        None => Err(BenchError::env(
            "oracle runtime not found: set $ERTS_ORACLE or put `erl` on PATH",
        )),
    }
}

/// Resolves the optional Rust runtime.
///
/// `$BEAM_RT` wins; otherwise the workspace-relative
/// `target/{release,debug}/beam_rt` is probed (relative to `CARGO_MANIFEST_DIR`). When
/// nothing is found a placeholder [`Runtime`] with `program: None` is returned, which
/// self-checks as unsupported — so it still yields honest `runtime-unsupported` rows.
pub fn resolve_rust() -> Runtime {
    if let Some(path) = non_empty_env("BEAM_RT") {
        return Runtime {
            kind: RuntimeKind::Rust,
            program: Some(PathBuf::from(path)),
        };
    }
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    for profile in ["release", "debug"] {
        let candidate = base.join(profile).join("beam_rt");
        if candidate.is_file() {
            return Runtime {
                kind: RuntimeKind::Rust,
                program: Some(candidate),
            };
        }
    }
    Runtime {
        kind: RuntimeKind::Rust,
        program: None,
    }
}

/// The serializable per-runtime header entry of a report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    /// `sys` or `rust`.
    pub kind: RuntimeKind,
    /// The resolved program path, or `null`.
    pub program: Option<String>,
    /// A best-effort version string, or `null`.
    pub version: Option<String>,
    /// Whether the runtime passed its self-check.
    pub supported: bool,
    /// Human-readable notes (e.g. why the runtime is unsupported).
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Reads an env var, treating empty as unset.
fn non_empty_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Writes an executable shell script that prints `body`, for self-check tests.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "{body}").unwrap();
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bench-runtime-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn self_check_accepts_a_program_printing_two() {
        let dir = tmpdir("ok");
        let p = script(&dir, "erl-ok", "echo 2");
        let rt = Runtime {
            kind: RuntimeKind::Rust,
            program: Some(p),
        };
        assert!(rt.self_check());
    }

    #[test]
    fn self_check_rejects_wrong_output_and_unresolved_programs() {
        let dir = tmpdir("bad");
        let p = script(&dir, "erl-bad", "echo 3");
        assert!(!Runtime {
            kind: RuntimeKind::Rust,
            program: Some(p),
        }
        .self_check());
        assert!(!Runtime {
            kind: RuntimeKind::Rust,
            program: None,
        }
        .self_check());
        assert!(!Runtime {
            kind: RuntimeKind::Rust,
            program: Some(dir.join("does-not-exist")),
        }
        .self_check());
    }

    /// `self_check_result` keeps `self_check`'s `Ok(false)` semantics for the non-error
    /// negative cases and the positive case, but surfaces a genuine spawn error as `Err`.
    #[test]
    fn self_check_result_distinguishes_negative_cases_from_a_genuine_spawn_error() {
        let dir = tmpdir("result");
        let good = script(&dir, "rl-ok", "echo 2");
        let wrong = script(&dir, "rl-wrong", "echo 3");

        // Positive: a program printing exactly `2`.
        assert!(Runtime {
            kind: RuntimeKind::Rust,
            program: Some(good),
        }
        .self_check_result()
        .expect("runs"));

        // Negative, no error: unresolved, absent, and wrong-output.
        assert!(!Runtime {
            kind: RuntimeKind::Rust,
            program: None,
        }
        .self_check_result()
        .expect("none is not an error"));
        assert!(!Runtime {
            kind: RuntimeKind::Rust,
            program: Some(dir.join("does-not-exist")),
        }
        .self_check_result()
        .expect("NotFound is not surfaced"));
        assert!(!Runtime {
            kind: RuntimeKind::Rust,
            program: Some(wrong),
        }
        .self_check_result()
        .expect("wrong output is not an error"));

        // Genuine error: a write-open-held script makes every `exec` fail with `ETXTBSY`,
        // which outlives the bounded retries and must be surfaced (not swallowed).
        #[cfg(unix)]
        {
            let held_path = script(&dir, "rl-held-open", "echo 2");
            let held = std::fs::OpenOptions::new()
                .write(true)
                .open(&held_path)
                .expect("open the script for writing");
            let rt = Runtime {
                kind: RuntimeKind::Rust,
                program: Some(held_path),
            };
            let err = rt
                .self_check_result()
                .expect_err("a write-open file cannot be exec'd");
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::ExecutableFileBusy,
                "{err:?}"
            );
            // The `bool` wrapper still degrades this to `false`, never a panic.
            assert!(!rt.self_check());
            drop(held);
        }
    }

    #[test]
    fn info_marks_unresolved_rust_as_unsupported_with_a_note() {
        let rt = Runtime {
            kind: RuntimeKind::Rust,
            program: None,
        };
        let info = rt.info();
        assert!(!info.supported);
        assert_eq!(info.program, None);
        assert_eq!(info.kind, RuntimeKind::Rust);
        assert!(!info.notes.is_empty());
    }

    #[test]
    fn runtime_kind_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&RuntimeKind::Sys).unwrap(), "\"sys\"");
        assert_eq!(
            serde_json::to_string(&RuntimeKind::Rust).unwrap(),
            "\"rust\""
        );
    }

    #[test]
    fn oracle_resolution_is_env_overridable() {
        let dir = tmpdir("oracle");
        let p = script(&dir, "fake-erl", "echo 2");
        let rt = oracle_from(Some(p.display().to_string())).unwrap();
        assert_eq!(rt.kind, RuntimeKind::Sys);
        assert_eq!(rt.program(), Some(p.as_path()));
        assert!(rt.self_check());
    }

    #[test]
    fn oracle_env_pointing_at_a_missing_file_is_an_env_error() {
        let err = oracle_from(Some("/definitely/not/here/erl".to_string())).unwrap_err();
        assert!(matches!(err, BenchError::Env(_)));
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn oracle_from_none_follows_path() {
        match oracle_from(None) {
            Ok(rt) => {
                assert_eq!(rt.kind, RuntimeKind::Sys);
                assert!(rt.program().is_some());
            }
            Err(e) => assert!(matches!(e, BenchError::Env(_))),
        }
    }
}
