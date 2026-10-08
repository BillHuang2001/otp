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

//! `proc_lib:start/3,4` — **the A.9 self-hosted-boot blocker**.
//!
//! `init`'s `{kernelProcess, application_controller, {application_controller,start}}`
//! directive is what stops `bootstrap/bin/start.boot` at instruction 16: the boot script's
//! `{apply, {application, load, …}}` then raises `exit noproc`, because
//! `application_controller:start/1` — reached through `proc_lib:start_link/4` — crashed
//! during the `{kernelProcess, …}` burst instead of blocking.
//!
//! This suite is the committed, oracle-cross-checked bisection of *why*, and it is the
//! regression gate for the fix. `tests/fixtures/plstart.erl` is one shape of "spawn a child,
//! wait for its ack" per entry point and `tests/fixtures/plwait.erl` holds the waiting
//! `receive` that lives **one module boundary away** (exactly as `proc_lib:sync_start/2`
//! does from `plstart`'s entry points). Both are compiled at test time with the system
//! `erlc` into one scratch directory, which is the single `-pa` handed to this runtime and
//! to the live `erl` oracle alike.
//!
//! # The bisection (verified against the live oracle)
//!
//! | Entry | Where the waiting `receive` lives | This runtime | The oracle |
//! |---|---|---|---|
//! | `inline` / `inline_hard` | inline in the entry function | `{ok,pid}` | `{ok,pid}` |
//! | `sep` / `sep_hard` | a *local* function of the entry module | `{ok,pid}` | `{ok,pid}` |
//! | `plshaped` | `plsync/2`, a local function (the `proc_lib` shape) | `{ok,pid}` | `{ok,pid}` |
//! | `pure` | no receive at all, just a cross-module call | `42` | `42` |
//! | `cross` / `cross_hard` | `plwait:wait[_hard]/2`, a **different module** | typed gap | `{ok,pid}` |
//! | `run` / `startlink` | `proc_lib:sync_start/2`, a **different module** | typed gap | `{ok,pid}` |
//!
//! So neither "a receive in a non-entry frame" (H1) nor "a register-held `after Timeout`"
//! (H2) is the discriminator: both work inside one module. The discriminator is the
//! **cross-module `call_ext` boundary** — see the root cause below.
//!
//! # The root cause (in `beam_engine`, not in `beam_rt`)
//!
//! `beam_engine`'s `Instr::CallExt` arm
//! (`erts_rust/crates/beam_engine/src/interpreter/dispatch.rs:301-307`) dispatches an
//! external call **synchronously** through `ExecutionContext::call_bif`,
//! `beam_rt`'s implementation of which serves a module export by loading it and running it
//! on a **fresh, non-blocking** machine:
//! `RuntimeContext::call_bif` → `RuntimeContext::run_export` (`src/context/mod.rs`) →
//! `Machine::new().run_export(...)`. `Machine::run_export` folds a
//! `RunOutcome::Wait` into the typed `Err(error::blocked())`
//! (`beam_engine/src/machine/run.rs:51-78`, `fold_outcome`), so a callee that blocks in its
//! own `receive` cannot suspend the *process*: the block surfaces as a raise at the call
//! site and the process dies.
//!
//! The real BEAM does not do this: `i_call_ext` saves a continuation pointer and **jumps
//! into the callee's code in the same process frame**
//! (`erts/emulator/beam/emu/instrs.tab:191`, `ops.tab:923`), so the callee's `receive`
//! suspends the process and the scheduler resumes it there.
//!
//! # Why `beam_rt` cannot close it alone
//!
//! Reaching the `Wait` from the *caller* requires knowing where the caller's `call_ext`
//! is — and `RuntimeContext` has no handle on the running `Machine`: the runner owns it
//! (`src/runner/run.rs` takes `engine.machine` out of the [`RunnerState`] map and hands it
//! to the engine by `&mut`), so a `call_bif` nested inside its dispatch loop cannot reach
//! it (`#![deny(unsafe_code)]`). Making the boundary blocking-aware therefore needs an
//! engine-side seam: an `ExecutionContext` method that resolves an external call to a
//! `(Arc<Module>, func, pc)` *entry* so the `CallExt` arm can push a frame and
//! `Step::Goto` into the callee — the C-faithful shape — instead of recursing.
//!
//! The `FRONTIER` table below is the honest pin of the current behaviour: exactly the typed
//! capability gap, never a wrong result and never a hang, with the oracle's real answer
//! recorded beside it.
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

mod support;

/// The committed fixture sources (compiled at test time — never committed as `.beam`).
const FIXTURES: [&str; 2] = [
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/plstart.erl"),
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/plwait.erl"),
];

/// The fixture module the entry points live in.
const MODULE: &str = "plstart";

/// Entry points that **agree** with the live oracle: `(label, function, expected line)`.
///
/// Every one of them spawns a child and waits for its ack *inside the fixture's own
/// module*, which is why the block is reported and resumed correctly today.
const AGREE: [(&str, &str, &str); 6] = [
    ("an inline receive", "inline", "{ok,pid}"),
    ("an inline receive with a hardcoded after", "inline_hard", "{ok,pid}"),
    ("a receive in a local non-entry function", "sep", "{ok,pid}"),
    (
        "a receive in a local non-entry function with after",
        "sep_hard",
        "{ok,pid}",
    ),
    ("the proc_lib structure, reproduced locally", "plshaped", "{ok,pid}"),
    ("a cross-module call that does not block", "pure", "42"),
];

/// The **current frontier**: `(label, function, the oracle's answer)`.
///
/// Each of these waits for its ack across a **module boundary** — the shape
/// `proc_lib:start/4`/`start_link/4` uses. The live oracle answers `{ok,pid}`; this runtime
/// reports the typed capability gap below instead. They are asserted as a gap (not as a
/// pass): the moment the engine's external-call boundary becomes blocking-aware, these
/// entries must be moved into [`AGREE`].
const FRONTIER: [(&str, &str, &str); 4] = [
    ("proc_lib:start/4", "run", "{ok,pid}"),
    ("proc_lib:start_link/4", "startlink", "{ok,pid}"),
    ("a receive in a foreign module", "cross", "{ok,pid}"),
    ("a receive in a foreign module with after", "cross_hard", "{ok,pid}"),
];

/// The typed diagnostic a cross-module blocking call currently reports (exit code 3).
const BLOCKED_GAP: &str = "unsupported: opcode: run: process blocked (no message available)";

/// The CLI's `EXIT_UNSUPPORTED`.
const EXIT_UNSUPPORTED: i32 = 3;

// ---------------------------------------------------------------------------------------
// Toolchain discovery + the loud skip
// ---------------------------------------------------------------------------------------

/// The compiler: a sibling of the resolved oracle first (the same installation), then `$PATH`.
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(dir) = support::resolve_oracle().as_deref().and_then(Path::parent) {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}

/// Prints the loud skip banner — a missing compiler is never a silent pass.
fn skip_loudly(why: &str) {
    eprintln!();
    eprintln!("=============== beam_rt proc_lib:start SKIPPED ===============");
    eprintln!("{why}: the A.9 boot blocker cannot be reproduced.");
    eprintln!("Set $ERTS_ORACLE (or install Erlang/OTP) and re-run. NOT a pass.");
    eprintln!("=============================================================");
    eprintln!();
}

// ---------------------------------------------------------------------------------------
// The compiled fixtures
// ---------------------------------------------------------------------------------------

/// The compiled fixtures in one scratch directory (removed on drop).
struct Fixtures {
    /// The scratch directory; a safe single `-pa` for both runtimes (no `lists.beam`).
    dir: PathBuf,
    /// The compiled `plstart.beam`.
    beam: PathBuf,
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// Compiles both committed fixture modules into one scratch directory, or `None` with a
/// loud skip when the toolchain cannot do it.
fn build_fixtures(erlc: &Path) -> Option<Fixtures> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("beam_rt_plstart_{}_{n}", std::process::id()));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        skip_loudly(&format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    for source_path in FIXTURES {
        let name = Path::new(source_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {source_path}"));
        let bytes = match std::fs::read(source_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                skip_loudly(&format!("cannot read {source_path}: {error}"));
                return None;
            }
        };
        if let Err(error) = std::fs::write(dir.join(name), &bytes) {
            skip_loudly(&format!("cannot write {name}: {error}"));
            return None;
        }
    }
    let names: Vec<&str> = FIXTURES
        .iter()
        .filter_map(|source| {
            Path::new(source)
                .file_name()
                .and_then(|name| name.to_str())
        })
        .collect();
    let output = match Command::new(erlc)
        .arg("-o")
        .arg(&dir)
        .args(&names)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            skip_loudly(&format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        skip_loudly(&format!(
            "`erlc` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
        return None;
    }
    let beam = dir.join("plstart.beam");
    if !beam.is_file() {
        skip_loudly(&format!("`erlc` produced no {}", beam.display()));
        return None;
    }
    Some(Fixtures { dir, beam })
}

// ---------------------------------------------------------------------------------------
// The bounded probe
// ---------------------------------------------------------------------------------------

/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Runs `program args` with `/dev/null` as stdin, polling so a probe that blocks is killed
/// and reaped at [`support::PROBE_TIMEOUT`] instead of hanging the suite.
fn run_bounded(program: &Path, args: &[&str]) -> Run {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("cannot spawn `{}`: {error}", program.display()));
    let deadline = Instant::now() + support::PROBE_TIMEOUT;
    let output: Output = loop {
        match child.try_wait() {
            Ok(Some(_)) => break child
                .wait_with_output()
                .unwrap_or_else(|error| panic!("cannot collect the probe's output: {error}")),
            Ok(None) => {
                assert!(
                    Instant::now() < deadline,
                    "`{}` outlived the {}s probe bound",
                    program.display(),
                    support::PROBE_TIMEOUT.as_secs()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("cannot poll `{}`: {error}", program.display()),
        }
    };
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// The live oracle's line for `MODULE:function/0`.
fn oracle_line(oracle: &Path, dir: &Path, function: &str) -> String {
    let expr = format!("io:format(\"~p~n\", [{MODULE}:{function}()]), halt(0).");
    support::oracle_eval(oracle, Some(dir), &expr)
        .unwrap_or_else(|| panic!("the live oracle could not answer {function}/0"))
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// Every shape whose waiting `receive` lives **inside the fixture's own module** answers
/// exactly what the live oracle answers — so a `receive` in a non-entry frame (H1) and a
/// register-held `after Timeout` (H2) are both already supported, and the `call_ext`
/// boundary is the only discriminator left.
#[test]
fn same_module_receives_agree_with_the_oracle() {
    let Some(oracle) = support::require_oracle("proc_lib_start") else {
        return;
    };
    let Some(erlc) = resolve_erlc() else {
        skip_loudly("no `erlc` available (and none beside $ERTS_ORACLE)");
        return;
    };
    let Some(fixtures) = build_fixtures(&erlc) else {
        return;
    };
    let beam = fixtures
        .beam
        .to_str()
        .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", fixtures.beam.display()));

    for (label, function, expected) in AGREE {
        let dir = fixtures.dir.to_str().expect("the scratch dir is not UTF-8");
        let spec = format!("{MODULE}:{function}/0");
        let ours = run_bounded(
            Path::new(env!("CARGO_BIN_EXE_beam_rt")),
            &["-pa", dir, "run", beam, &spec],
        );
        assert_eq!(
            ours.code, 0,
            "{label}: `beam_rt run {beam} {spec}` exited {}: {}",
            ours.code, ours.err
        );
        assert!(
            !ours.err.contains("unsupported"),
            "{label}: a same-module receive is not a capability gap: {}",
            ours.err
        );
        assert_eq!(ours.out, expected, "{label}: beam_rt's answer");
        assert_eq!(
            oracle_line(&oracle, &fixtures.dir, function),
            expected,
            "{label}: the recorded answer must be the live oracle's"
        );
    }
}

/// The A.9 boot blocker, pinned as a typed capability gap: a `receive` reached across a
/// **module boundary** (`plwait:wait/2`, and so `proc_lib:sync_start/2`) reports the
/// engine's unchanged `blocked` diagnostic on exit code 3 — never a wrong result, never a
/// hang — while the live oracle answers `{ok,pid}` for the very same call.
///
/// This test is the reproduction the fix must flip: when the cross-module call boundary
/// suspends the process instead of raising, move these four entries into [`AGREE`] and
/// delete [`FRONTIER`] / [`BLOCKED_GAP`].
#[test]
fn cross_module_blocking_calls_report_the_typed_boot_gap() {
    let Some(oracle) = support::require_oracle("proc_lib_start") else {
        return;
    };
    let Some(erlc) = resolve_erlc() else {
        skip_loudly("no `erlc` available (and none beside $ERTS_ORACLE)");
        return;
    };
    let Some(fixtures) = build_fixtures(&erlc) else {
        return;
    };
    let beam = fixtures
        .beam
        .to_str()
        .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", fixtures.beam.display()));
    let dir = fixtures.dir.to_str().expect("the scratch dir is not UTF-8");

    for (label, function, oracle_answer) in FRONTIER {
        let spec = format!("{MODULE}:{function}/0");
        let ours = run_bounded(
            Path::new(env!("CARGO_BIN_EXE_beam_rt")),
            &["-pa", dir, "run", beam, &spec],
        );
        assert_eq!(
            ours.code, EXIT_UNSUPPORTED,
            "{label}: `beam_rt run {beam} {spec}` must be the honest capability gap (exit \
             {EXIT_UNSUPPORTED}), got exit {}: {} / {}",
            ours.code, ours.out, ours.err
        );
        assert!(
            ours.err.contains(BLOCKED_GAP),
            "{label}: the gap must be the engine's own blocked diagnostic, got: {}",
            ours.err
        );
        assert_eq!(
            oracle_line(&oracle, &fixtures.dir, function),
            oracle_answer,
            "{label}: the live oracle really does answer — so this is a gap, not a fixture bug"
        );
    }
}
