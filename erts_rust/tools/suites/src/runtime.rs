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
//! Runtime resolution — the two sides every row compares.
//!
//! * **oracle** (`sys`) = `$ERTS_ORACLE` if set, else `erl` on `$PATH` (never the in-tree
//!   `erts/` C build). It is mandatory: a missing oracle is a loud [`SuitesError::Env`].
//! * **Rust runtime** (`rust`) = `$BEAM_RT` if set, else the workspace-relative
//!   `target/{release,debug}/beam_rt`. It is optional: when absent, every Rust cell is
//!   recorded as `not_run(runtime-missing)` rather than crashing the sweep.
//!
//! Both are driven as **drop-in `erl` binaries** (compat contract A.6): nothing here
//! assumes anything about internals, and the two sides get the identical `-pa` set and the
//! identical `-eval` expression.
use std::path::{Path, PathBuf};
use std::process::Command;
use crate::error::{Result, SuitesError};
use crate::proc;
/// Which side of the comparison a result came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeKind {
    /// The system Erlang (the oracle).
    Sys,
    /// The Rust ERTS runtime (`beam_rt`).
    Rust,
}
impl RuntimeKind {
    /// The lowercase id used in JSON and the human tables.
    pub fn as_str(&self) -> &'static str {
        match self {
            RuntimeKind::Sys => "sys",
            RuntimeKind::Rust => "rust",
        }
    }
}
/// The canonical self-check: both runtimes must print `2`.
pub const SELF_CHECK_EVAL: &str = "io:format(\"~p~n\",[1+1]), halt(0).";
/// A resolved (or explicitly absent) runtime.
#[derive(Debug, Clone)]
pub struct Runtime {
    /// Which side this is.
    pub kind: RuntimeKind,
    /// The program to execute, or `None` when it could not be resolved.
    pub program: Option<PathBuf>,
    /// Why it is absent (only meaningful when `program` is `None`).
    pub absent_reason: Option<String>,
}
impl Runtime {
    /// Resolves the oracle (mandatory) or the Rust runtime (optional).
    pub fn resolve(kind: RuntimeKind) -> Result<Runtime> {
        match kind {
            RuntimeKind::Sys => {
                let (program, source) = match std::env::var_os("ERTS_ORACLE") {
                    Some(v) if !v.is_empty() => {
                        let p = PathBuf::from(&v);
                        if !p.is_file() {
                            return Err(SuitesError::Env(format!(
                                "$ERTS_ORACLE={} is not a file",
                                p.display()
                            )));
                        }
                        (p, "$ERTS_ORACLE")
                    }
                    _ => match proc::which("erl") {
                        Some(p) => (p, "PATH"),
                        None => {
                            return Err(SuitesError::Env(
                                "no oracle runtime: set $ERTS_ORACLE or put `erl` on $PATH".into(),
                            ))
                        }
                    },
                };
                Ok(Runtime {
                    kind,
                    program: Some(program),
                    absent_reason: Some(format!("resolved via {source}")),
                })
            }
            RuntimeKind::Rust => {
                if let Some(v) = std::env::var_os("BEAM_RT") {
                    if !v.is_empty() {
                        let p = PathBuf::from(&v);
                        if p.is_file() {
                            return Ok(Runtime {
                                kind,
                                program: Some(p),
                                absent_reason: Some("resolved via $BEAM_RT".into()),
                            });
                        }
                        return Ok(Runtime {
                            kind,
                            program: None,
                            absent_reason: Some(format!("$BEAM_RT={} is not a file", p.display())),
                        });
                    }
                }
                let root = workspace_root();
                for profile in ["release", "debug"] {
                    let candidate = root.join("target").join(profile).join("beam_rt");
                    if candidate.is_file() {
                        return Ok(Runtime {
                            kind,
                            program: Some(candidate.clone()),
                            absent_reason: Some(format!("resolved as {}", candidate.display())),
                        });
                    }
                }
                Ok(Runtime {
                    kind,
                    program: None,
                    absent_reason: Some(format!(
                        "no beam_rt under {}/target/{{release,debug}} (build it, or set $BEAM_RT)",
                        root.display()
                    )),
                })
            }
        }
    }
    /// The program path, when resolved.
    pub fn program(&self) -> Option<&Path> {
        self.program.as_deref()
    }
    /// Runs the canonical self-check and returns the reported version line on success.
    pub fn self_check(&self) -> std::result::Result<String, String> {
        let Some(program) = self.program() else {
            return Err(self.absent_reason.clone().unwrap_or_else(|| "absent".into()));
        };
        let mut cmd = Command::new(program);
        cmd.arg("-noshell").arg("-eval").arg(SELF_CHECK_EVAL);
        let done = proc::run_with_timeout(&mut cmd, Some(std::time::Duration::from_secs(60)))
            .map_err(|e| format!("spawn {}: {e}", program.display()))?;
        if done.stdout_trim() != "2" {
            return Err(format!(
                "self-check printed {:?} (exit {:?})",
                done.stdout_trim(),
                done.exit_code
            ));
        }
        Ok(self.version_line().unwrap_or_else(|| "unknown".into()))
    }
    /// `"<otp_release> <erts_version>"` for the report header.
    pub fn version_line(&self) -> Option<String> {
        let program = self.program()?;
        let mut cmd = Command::new(program);
        cmd.arg("-noshell")
            .arg("-eval")
            .arg("io:format(\"~s/~s~n\",[erlang:system_info(otp_release),erlang:system_info(version)]), halt(0).");
        let done = proc::run_with_timeout(&mut cmd, Some(std::time::Duration::from_secs(60))).ok()?;
        let line = done.stdout_trim().to_string();
        if line.is_empty() {
            None
        } else {
            Some(line)
        }
    }
}
/// The `erts_rust/` workspace root, derived from this crate's manifest directory
/// (`erts_rust/tools/suites` → `erts_rust`).
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}
/// The repository root (`erts_rust/..`), used for the default OTP stdlib test source and for
/// the git sha recorded in every report.
pub fn repo_root() -> PathBuf {
    workspace_root().parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(".."))
}
/// `git rev-parse HEAD` in `dir` — the commit the measurement was taken at.
///
/// This is recorded, never guessed: if `git` is unavailable the report says `null`.
pub fn git_sha(dir: &Path) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).arg("rev-parse").arg("HEAD");
    let done = proc::run_with_timeout(&mut cmd, Some(std::time::Duration::from_secs(30))).ok()?;
    let sha = done.stdout_trim().to_string();
    if done.success && sha.len() >= 7 {
        Some(sha)
    } else {
        None
    }
}
/// `TMPDIR` (or `/tmp`) — where every staged checkout lives, so the real project trees stay
/// strictly read-only.
pub fn tmp_root() -> PathBuf {
    std::env::var_os("TMPDIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}
