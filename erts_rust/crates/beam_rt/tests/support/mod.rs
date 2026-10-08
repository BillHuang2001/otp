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

//! Shared plumbing for the `beam_rt` integration suites that drive a real
//! [`Runtime`](beam_rt::Runtime) and, where the toolchain allows, cross-check this
//! runtime's answers against the **system Erlang oracle**.
//!
//! # Why the toolchain probe must never *run* `erl`
//!
//! An earlier version of this harness decided whether the toolchain existed with
//! `Command::new("erl").arg("-noshell").output()` and then waited for the child to exit.
//! `erl -noshell` starts a **long-running node**: with no `-eval`/`-s init stop` to
//! terminate it, it never exits (`timeout 5 erl -noshell` is killed at 5 s, i.e. it blocks
//! for ever), so that call blocked for ever — and because cargo runs the tests of one
//! binary in parallel threads, *every* test in the binary hung on its own blocked `erl`
//! child. Two rules therefore hold here, and the helpers below enforce both:
//!
//! 1. **Existence is a filesystem question.** [`resolve_oracle`] only looks for an
//!    executable file on `$PATH` (or `$ERTS_ORACLE`); it never runs the program. Same
//!    shape as the crate's other harnesses (`src/context/test_support.rs`,
//!    `tests/position.rs`).
//! 2. **Every probe terminates and is bounded.** [`oracle_eval`] always appends
//!    `-s init stop` (so the node shuts down even when the expression raises), hands the
//!    child `/dev/null` as stdin, and is **killed and reaped** if it outlives
//!    [`PROBE_TIMEOUT`]. A probe can therefore never hang the suite.
//!
//! A missing toolchain — or a probe that cannot answer — is a **loud skip**, never a
//! silent pass.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use beam_common::BeamError;

/// Assert `error` is the oracle's plain `exception error: bad argument` — the bare reason
/// atom `badarg` — which is the rejection every BIF-level failure in these suites reports.
///
/// [`BeamError::as_atom`] covers both the dedicated `BadArg` reason variant and a `Raise`
/// whose reason *is* the atom, so both shapes are accepted while a structured (non-bare)
/// reason still fails loudly — the oracle's `badarg` is never a tuple.
///
/// `#[allow(dead_code)]`: this shared module is compiled into **every** integration-test
/// binary, and the ones that never assert a `badarg` would otherwise warn.
#[allow(dead_code)]
pub fn assert_badarg(error: BeamError) {
    assert_eq!(
        error.as_atom(),
        Some("badarg"),
        "expected error:badarg, got {error:?}"
    );
}
/// The longest a single external probe may run before it is killed. Every probe here is a
/// `-noshell` program that must terminate on its own; the bound exists so that a probe
/// which nevertheless blocks (the exact failure mode documented above) can never hang the
/// suite.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// How often a running probe's exit is polled.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

// ---------------------------------------------------------------------------------------
// Toolchain discovery (filesystem only — never executes the tool)
// ---------------------------------------------------------------------------------------

/// Whether `p` is an existing executable file.
pub fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(p) {
            Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// The first `name` on `$PATH` that is an executable file.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// The oracle: `$ERTS_ORACLE` when it is an executable, else `erl` on `$PATH` (the
/// convention `tools/differential` uses). Resolved **without running it**.
pub fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        return is_executable(&program).then_some(program);
    }
    find_on_path("erl")
}

/// Prints the loud skip banner — a missing toolchain is never a silent pass.
///
/// `banner` names the suite and `why` the missing tool.
pub fn skip_loudly(banner: &str, why: &str) {
    eprintln!();
    eprintln!("=============== beam_rt {banner} SKIPPED ===============");
    eprintln!("{why}: the oracle comparison cannot run.");
    eprintln!("Set $ERTS_ORACLE (or install Erlang/OTP) and re-run. NOT a pass.");
    eprintln!("========================================================");
    eprintln!();
}

/// The resolved oracle, or `None` after a loud skip naming `banner`.
///
/// Callers do `let Some(oracle) = support::require_oracle("module_info") else { return };`
/// and the test then reports as skipped (loudly) instead of passing silently.
pub fn require_oracle(banner: &str) -> Option<PathBuf> {
    match resolve_oracle() {
        Some(oracle) => Some(oracle),
        None => {
            skip_loudly(banner, "no `erl` on $PATH (and no executable $ERTS_ORACLE)");
            None
        }
    }
}

// ---------------------------------------------------------------------------------------
// Bounded child execution
// ---------------------------------------------------------------------------------------

/// How a bounded probe ended.
enum Probe {
    /// The child exited; its status and captured output.
    Exited(Output),
    /// The child was killed after [`PROBE_TIMEOUT`] and reaped.
    TimedOut,
    /// The child could not be spawned, polled or collected.
    Failed,
}

/// Runs `command` with `/dev/null` as stdin and captured stdout/stderr.
///
/// The child is **polled** rather than waited on, so a probe that blocks is killed and
/// reaped at [`PROBE_TIMEOUT`] instead of hanging the caller. The pipes are drained after
/// the child exits, so a probe must not write more than a pipe buffer (~64 KiB) before it
/// exits — every probe here prints a handful of bytes.
fn run_bounded(mut command: Command) -> Probe {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("warning: cannot spawn `{program}`: {error}");
            return Probe::Failed;
        }
    };
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            // The child has exited: collect what the pipes hold (and reap it).
            Ok(Some(_)) => match child.wait_with_output() {
                Ok(output) => return Probe::Exited(output),
                Err(error) => {
                    eprintln!("warning: cannot collect `{program}`'s output: {error}");
                    return Probe::Failed;
                }
            },
            Ok(None) => {
                if Instant::now() >= deadline {
                    eprintln!(
                        "warning: `{program}` outlived the {}s probe bound and was killed",
                        PROBE_TIMEOUT.as_secs()
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    return Probe::TimedOut;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(error) => {
                eprintln!("warning: cannot poll `{program}`: {error}");
                let _ = child.kill();
                let _ = child.wait();
                return Probe::Failed;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// The oracle probe
// ---------------------------------------------------------------------------------------

/// Runs `expr` under the oracle and returns its trimmed stdout, or `None` when the probe
/// could not answer.
///
/// The command line is `erl -noshell [-pa code_dir] -eval expr -s init stop`, with the
/// `-pa` **before** `-eval` (erl evaluates `-eval` in command-line order, so a code path
/// added afterwards is too late — proved against the system oracle), stdin at `/dev/null`,
/// and a `-s init stop` terminator so the node shuts down even when `expr` raises.
///
/// `expr` should still `halt(0).` when it succeeds (that is what the recorded probes do);
/// `-s init stop` is the backstop. A failed or timed-out probe prints a diagnostic and
/// yields `None` — the caller then reports a loud skip.
pub fn oracle_eval(oracle: &Path, code_dir: Option<&Path>, expr: &str) -> Option<String> {
    let mut command = Command::new(oracle);
    command.arg("-noshell");
    if let Some(dir) = code_dir {
        command.arg("-pa").arg(dir);
    }
    command
        .arg("-eval")
        .arg(expr)
        .arg("-s")
        .arg("init")
        .arg("stop");
    match run_bounded(command) {
        Probe::Exited(output) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        Probe::Exited(output) => {
            eprintln!(
                "warning: the oracle exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{expr}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            );
            None
        }
        Probe::TimedOut | Probe::Failed => None,
    }
}
