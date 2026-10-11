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

//! The **scheduled-process** `call_fun`/`call_fun2` apply seam.
//!
//! `beam_engine` executes `Instr::CallFun`/`Instr::CallFun2` by consulting
//! `ExecutionContext::resolve_local_fun` **first**: on `Some((module, label))` it enters the
//! closure body **in the calling machine**, so a `receive` inside it suspends the process through
//! the ordinary `RunOutcome::Wait` path and it is resumed by a real send. Only when the resolver
//! declines (`Ok(None)`) does the engine fall back to the value-returning
//! `ExecutionContext::apply_fun`, whose nested `Machine` cannot re-enter the scheduler (a slice's
//! `may_pump_scheduler()` is `false`) and so folds a suspension into the typed capability gap
//! `unsupported_opcode: run: process blocked (no message available)`.
//!
//! The seam is **closed**: the scheduled slice runs its machine on `runner::budget::BudgetContext`,
//! a wrapper that forwards every `ExecutionContext` method to the slice's real `RuntimeContext` —
//! `resolve_local_fun` included — so the scheduled path reaches `RuntimeContext`'s closure resolver
//! exactly as the `-eval`/single-pass/boot routes do. A scheduled process that applies a blocking
//! closure therefore answers what the live oracle answers.
//!
//! The shapes matter because `gen:call/4` for a non-pid server id (a registered atom name or
//! `{Name,Node}`) builds a closure and applies it through `do_for_proc/2`'s `call_fun`, so **every**
//! `gen_server:call(Name, Req)` to a registered server made from a scheduled process — the shape
//! `beam_rt run <file.beam> <mod>:<fun>/<arity>` uses — goes through this seam.
//!
//! [`a_scheduled_process_applies_a_blocking_closure_and_answers_the_oracle`] asserts the target
//! behaviour for both shapes: the scheduled `run` exits `0`, names no `unsupported` gap, and prints
//! the live oracle's own line, and the `-eval` counterpart of the same workload agrees with the
//! oracle as well.
//!
//! The three fixture modules are compiled **at test time** with the system `erlc` into one
//! scratch directory (a safe single `-pa` for both runtimes — it never holds a
//! `lists.beam`); a missing `erl`/`erlc` is a **loud** skip, never a silent pass.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

mod support;

/// The `erl`-facing banner for the loud skip.
const BANNER: &str = "scheduled apply seam";

/// The executable under test.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// `saphelper:call/1` — the module the `{call_fun,0}` instruction lives in. Handing the
/// closure to a **second** module is what defeats the compiler's fun inlining (which would
/// turn the `call_fun` into a direct local call and bypass [`apply_fun`] entirely).
const SAPHELPER_SRC: &str = r#"
-module(saphelper).
-export([call/1]).
call(F) -> F().
"#;

/// The closure blocks in a bare `receive`, and a spawned child delivers the message it waits
/// for — so on a route that can pump (or once the engine enters the closure in-machine) the
/// process suspends and is resumed by a real send.
const SAPWAIT_SRC: &str = r#"
-module(sapwait).
-export([wait/0, sender/2]).
wait() ->
    spawn(sapwait, sender, [self(), 42]),
    saphelper:call(fun() -> receive X -> X end end).
sender(To, Msg) -> To ! Msg.
"#;

/// `gen:call/4` with a **registered name** server id: it builds a closure and applies it
/// through `do_for_proc/2`'s `call_fun`, and the closure's body blocks in `gen:do_call/4`'s
/// `receive`. This is the real-world shape (`gen_server:call(Name, Req)`) the gap breaks.
const SAPNAME_SRC: &str = r#"
-module(sapname).
-export([run/0, loop/0]).
run() ->
    Pid = spawn(sapname, loop, []),
    register(sapname_loop, Pid),
    gen:call(sapname_loop, tag, hello, infinity).
loop() ->
    receive
        {tag, {From, Mref}, Req} -> From ! {Mref, Req}, loop()
    end.
"#;

/// The three fixture modules as `(module, source)` — written to the scratch dir and compiled
/// together, so cross-module calls resolve the way a real application's would.
const FIXTURES: [(&str, &str); 3] = [
    ("saphelper", SAPHELPER_SRC),
    ("sapwait", SAPWAIT_SRC),
    ("sapname", SAPNAME_SRC),
];

/// The two closed-over blocking shapes, described by what they prove, the defining module and
/// the exported entry point. The expected line is the **live oracle's** own, probed by the
/// tests rather than hard-coded.
const VARIANTS: [(&str, &str, &str); 2] = [
    (
        "a closure that blocks in a bare `receive`, applied by a foreign module",
        "sapwait",
        "wait",
    ),
    (
        "`gen:call/4`'s closure for a registered server name",
        "sapname",
        "run",
    ),
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

// ---------------------------------------------------------------------------------------
// The compiled fixtures
// ---------------------------------------------------------------------------------------

/// The compiled fixtures in one scratch directory (removed on drop).
struct Fixtures {
    /// The scratch directory; a safe single `-pa` for both runtimes (no `lists.beam`).
    dir: PathBuf,
}

impl Fixtures {
    /// The compiled `<module>.beam` inside the scratch directory.
    fn beam(&self, module: &str) -> PathBuf {
        self.dir.join(format!("{module}.beam"))
    }

    /// The scratch directory as a `&str` for `-pa`.
    fn dir_str(&self) -> &str {
        self.dir
            .to_str()
            .unwrap_or_else(|| panic!("the scratch dir is not UTF-8: {}", self.dir.display()))
    }
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        // Best effort, but never silent: a leftover scratch dir must not fail the test, yet a
        // cleanup failure is reported rather than swallowed.
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// Writes and compiles the three fixture modules into one scratch directory, or `None` with a
/// loud skip when the toolchain cannot do it.
fn build_fixtures(erlc: &Path) -> Option<Fixtures> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // The scratch dir is handed to the oracle as a single `-pa`, so it must not contain a
    // `lists.beam` (this crate's `tests/fixtures/` does; the compiled modules here do not).
    let dir = std::env::temp_dir().join(format!("beam_rt_sapseam_{}_{n}", std::process::id()));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        support::skip_loudly(BANNER, &format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    for (module, source) in FIXTURES {
        if let Err(error) = std::fs::write(dir.join(format!("{module}.erl")), source) {
            support::skip_loudly(BANNER, &format!("cannot write {module}.erl: {error}"));
            return None;
        }
    }
    let names: Vec<String> = FIXTURES.iter().map(|(m, _)| format!("{m}.erl")).collect();
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
            support::skip_loudly(BANNER, &format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        support::skip_loudly(
            BANNER,
            &format!(
                "`erlc` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        );
        return None;
    }
    for (module, _) in FIXTURES {
        let beam = dir.join(format!("{module}.beam"));
        if !beam.is_file() {
            support::skip_loudly(BANNER, &format!("`erlc` produced no {}", beam.display()));
            return None;
        }
    }
    Some(Fixtures { dir })
}

/// Compiles the fixtures or reports the loud skip; both tests below need them.
fn fixtures_or_skip() -> Option<(PathBuf, Fixtures)> {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return None;
    };
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly(BANNER, "no `erlc` available (and none beside $ERTS_ORACLE)");
        return None;
    };
    let Some(fixtures) = build_fixtures(&erlc) else {
        return None;
    };
    Some((oracle, fixtures))
}

// ---------------------------------------------------------------------------------------
// The bounded probes
// ---------------------------------------------------------------------------------------

/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Runs `program args` with `/dev/null` as stdin, polling so a probe that blocks is killed and
/// reaped at [`support::PROBE_TIMEOUT`] instead of hanging the suite (the `run` action is
/// itself wall-clock-bounded, but the polling guard is the belt to that suspenders).
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

/// The **scheduled** invocation of `module:function/0` through the real executable — the shape
/// whose `ExecutionContext` may not pump.
fn run_scheduled(fixtures: &Fixtures, module: &str, function: &str) -> Run {
    let beam = fixtures.beam(module);
    let beam = beam
        .to_str()
        .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", beam.display()));
    let spec = format!("{module}:{function}/0");
    run_bounded(
        Path::new(BINARY),
        &["-pa", fixtures.dir_str(), "run", beam, &spec],
    )
}

/// The same workload on the **`-eval`/single-pass** route — a context that *may* pump. Prints
/// the value exactly as the oracle does.
fn run_eval_route(fixtures: &Fixtures, module: &str, function: &str) -> Run {
    let expr = format!("X = {module}:{function}(), io:format(\"~w~n\",[X]).");
    run_bounded(
        Path::new(BINARY),
        &["-pa", fixtures.dir_str(), "-eval", &expr],
    )
}

/// The live oracle's line for `module:function/0`, as the executable prints it — a probe that
/// cannot answer is a **loud failure**, never a silent pass.
fn oracle_line(oracle: &Path, fixtures: &Fixtures, module: &str, function: &str) -> String {
    let expr = format!("io:format(\"~w~n\",[{module}:{function}()]), halt(0).");
    support::oracle_eval(oracle, Some(&fixtures.dir), &expr)
        .unwrap_or_else(|| panic!("the live oracle could not answer {module}:{function}/0"))
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// A scheduled process that applies a blocking closure answers exactly what the live oracle
/// answers.
///
/// For both shapes it asserts `beam_rt run` exits `0`, names no `unsupported` capability gap, and
/// prints the oracle's own line, and that the `-eval` counterpart of the same workload agrees with
/// the oracle too. The seam is closed: `BudgetContext` forwards `resolve_local_fun` to the slice's
/// real `RuntimeContext`, so the engine enters the closure body in the scheduled machine and a
/// `receive` in it suspends the process instead of folding into the typed `blocked()` gap.
#[test]
fn a_scheduled_process_applies_a_blocking_closure_and_answers_the_oracle() {
    let Some((oracle, fixtures)) = fixtures_or_skip() else {
        return;
    };

    for (label, module, function) in VARIANTS {
        let expected = oracle_line(&oracle, &fixtures, module, function);

        let ours = run_scheduled(&fixtures, module, function);
        assert_eq!(
            ours.code, 0,
            "{label}: `beam_rt run {module}:{function}/0` must exit 0 once the closure is \
             entered in-machine, got {}: {}",
            ours.code, ours.err
        );
        assert!(
            !ours.err.contains("unsupported"),
            "{label}: applying a blocking closure from a scheduled process is not a capability \
             gap: {}",
            ours.err
        );
        assert_eq!(ours.out, expected, "{label}: the scheduled run's answer");

        // The `-eval`/single-pass route (a context that *may* pump) is a cross-check: both
        // routes must answer what the live oracle answers.
        let eval = run_eval_route(&fixtures, module, function);
        assert_eq!(
            eval.code, 0,
            "{label}: the `-eval` route must exit 0, got {}: {}",
            eval.code, eval.err
        );
        assert_eq!(eval.out, expected, "{label}: the `-eval` route's answer");
    }
}
