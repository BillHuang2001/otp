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

//! `proc_lib:start/3,4` — a `receive` that lives **one module boundary away**, and its
//! regression gate.
//!
//! `init`'s `{kernelProcess, application_controller, {application_controller,start}}`
//! boot-script directive, and a plain `proc_lib:start[_link]/4`, both reach
//! `application_controller:start/1` through `proc_lib:start_link/4`, which waits in
//! `proc_lib:sync_start/2`'s bare `receive` — a **different module** from the entry point.
//! Suspending the process across that boundary is what makes the start answer `{ok,pid}`
//! instead of a capability gap.
//!
//! This suite is the committed, oracle-cross-checked bisection of *why*, and it is the
//! regression gate for that seam. `tests/fixtures/plstart.erl` is one shape of "spawn a child,
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
//! | `cross` / `cross_hard` | `plwait:wait[_hard]/2`, a **different module** | `{ok,pid}` | `{ok,pid}` |
//! | `run` / `startlink` | `proc_lib:sync_start/2`, a **different module** | `{ok,pid}` | `{ok,pid}` |
//!
//! So neither "a receive in a non-entry frame" (H1) nor "a register-held `after Timeout`"
//! (H2) is the discriminator: both work inside one module. The discriminator is the
//! **cross-module `call_ext` boundary** — see below.
//!
//! # The two seams that suspend the process
//!
//! * **Inside a running machine** — the engine's cross-module call seam
//!   ([`ExecutionContext::resolve_external_call`]): a `call_ext*` the host resolves to an
//!   `(Arc<Module>, entry_label)` is entered **in the calling machine**, the C-faithful shape
//!   (`i_call_ext` saves a continuation pointer and jumps into the callee's code in the same
//!   process frame — `erts/emulator/beam/emu/instrs.tab:191`, `ops.tab:923`), so the callee's
//!   registers, stack and frames *are* the process's and its `receive` suspends the process.
//! * **Through `ExecutionContext::call_bif`** (the `-eval` evaluator's route, an `apply`, a
//!   `{kernelProcess, …}`/`{apply, …}` boot directive) — `beam_rt`'s blocking-aware direct
//!   entries, `RuntimeContext::run_export_blocking`/`run_label_blocking`
//!   (`src/context/mod.rs`): a callee that reports `RunOutcome::Wait` is resumed after the
//!   scheduler has been pumped.
//!
//! `beam_rt` implements the engine seam in `src/context/mod.rs::resolve_external_call`. It is
//! consulted before every `call_ext*`, so it answers `Ok(None)` for anything the synchronous
//! path serves better (the runtime-native floor's namespaces, a NIF-backed module, a
//! registered `beam_bifs` name, a float-using module, an unknown export) and it **loads the
//! callee on demand** when the module is not cached yet — otherwise the *first* call (exactly
//! what a `proc_lib` start is) would keep the synchronous path.
//!
//! [`ExecutionContext::resolve_external_call`]: beam_engine::ExecutionContext::resolve_external_call

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
/// The first six spawn a child and wait for its ack *inside the fixture's own module*; the
/// last four wait across a **module boundary** (`plwait:wait[_hard]/2`, or `proc_lib`'s own
/// `sync_start/2`), which is the shape `proc_lib:start[_link]/4` uses — and the shape the
/// cross-module call seam ([`beam_engine::ExecutionContext::resolve_external_call`], which
/// `beam_rt` implements in `src/context/mod.rs`) now runs **in the calling machine**, so the
/// callee's `receive` suspends the process instead of folding into `error::blocked()`.
const AGREE: [(&str, &str, &str); 10] = [
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
    ("proc_lib:start/4", "run", "{ok,pid}"),
    ("proc_lib:start_link/4", "startlink", "{ok,pid}"),
    ("a receive in a foreign module", "cross", "{ok,pid}"),
    ("a receive in a foreign module with after", "cross_hard", "{ok,pid}"),
];
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
    eprintln!("{why}: the cross-module receive regression gate cannot be exercised.");
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

/// The A.9 boot blocker, **closed**: every shape — including the four that wait across a
/// **module boundary** (`plwait:wait[_hard]/2`, and `proc_lib:start[_link]/4` through
/// `proc_lib:sync_start/2`) — answers exactly what the live oracle answers.
///
/// The cross-module call seam runs the callee **in the calling machine**, and the on-demand
/// load makes that true for the *first* call to a module too — which is what a `proc_lib`
/// start does (nothing has loaded `proc_lib` beforehand). So the callee's `receive` suspends
/// the process, the scheduler resumes it when the child acks, and the answer is `{ok,pid}`
/// instead of the engine's old typed `blocked` diagnostic.
#[test]
fn cross_module_receives_agree_with_the_oracle() {
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

    // The four that cross a module boundary are the ones this test is about: run them by
    // name so a future reshuffle of `AGREE` cannot silently drop the discrimination.
    for function in ["run", "startlink", "cross", "cross_hard"] {
        let spec = format!("{MODULE}:{function}/0");
        let ours = run_bounded(
            Path::new(env!("CARGO_BIN_EXE_beam_rt")),
            &["-pa", dir, "run", beam, &spec],
        );
        eprintln!("{spec} -> exit {} out={:?} err={:?}", ours.code, ours.out, ours.err);
        assert_eq!(
            ours.code, 0,
            "{spec}: a cross-module receive is no longer a capability gap: {}",
            ours.err
        );
        assert!(
            !ours.err.contains("unsupported"),
            "{spec}: still reported as a capability gap: {}",
            ours.err
        );
        assert_eq!(ours.out, "{ok,pid}", "{spec}: beam_rt's answer");
        assert_eq!(
            oracle_line(&oracle, &fixtures.dir, function),
            "{ok,pid}",
            "{spec}: the live oracle's answer"
        );
    }
}
