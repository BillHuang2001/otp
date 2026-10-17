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

//! `erlang:monitor/2,3` and `erlang:demonitor/1,2` at the **executable** boundary.
//!
//! `src/native/process/monitor.rs`'s unit tests pin the BIF layer against the runtime's own
//! process table; this suite pins the path a user sees: the built `beam_rt` binary, the
//! runtime-native floor and the `-eval`/`receive` machinery, compared with the live system
//! `erl` oracle running the **same** expression.
//!
//! # Why `-eval` and not a compiled fixture
//!
//! A monitor only makes sense for a process that can receive its `{'DOWN', …}`, and the two
//! ways to get such a process are a `spawn` or a second scheduler process. Every flow below
//! is therefore an `-eval` program that spawns a helper, monitors it and prints the observed
//! facts — the same program both runtimes are given, with no fixture to compile. (`beam_rt
//! run <file>` spawns correctly too; see `tests/spawn_from_slice.rs`.)
//!
//! # Oracle ground truth (system `erl`, OTP 29 / `erts-17.1`)
//!
//! The three flows print exactly (re-probed and re-asserted by
//! [`the_monitor_flows_match_the_live_oracle`]):
//!
//! ```text
//! {true,true,true}    %% monitor/2 on a pid and on a registered name both answer a ref
//! {true,false,true}   %% demonitor/2 [info]: true then false; demonitor/1: true
//! {true,none}         %% demonitor/2 [flush]: true, and the queued DOWN is gone
//! ```
//!
//! A missing `erl` is a **loud** skip, never a silent pass (`tests/support/mod.rs`).

// The shared harness is compiled in whole, but this suite uses only its toolchain
// discovery and probe bound (the error-case probes need the exit code, which
// `support::oracle_eval` folds away), so the unused remainder is expected here.
#[allow(dead_code)]
mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------
// The bounded child runner
// ---------------------------------------------------------------------------------------

/// Runs `program args` with `/dev/null` as stdin, polling so a probe that blocks is killed
/// and reaped at [`support::PROBE_TIMEOUT`] instead of hanging the suite. `None` when the
/// child could not be spawned, polled or collected.
///
/// The bounded runner lives here rather than in `tests/support/mod.rs` because every probe
/// in this suite is a program that terminates on its own; the polling guard is the belt to
/// that suspenders, and keeping it local avoids editing the shared harness.
fn run_bounded(program: &str, args: &[&str]) -> Option<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + support::PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    eprintln!("warning: `{program}` outlived the probe bound and was killed");
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                eprintln!("warning: cannot poll `{program}`: {error}");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Runs `program args`, failing loudly when the probe could not be run at all.
fn run(program: &str, args: &[&str]) -> Run {
    let output = run_bounded(program, args)
        .unwrap_or_else(|| panic!("`{program}` {args:?} could not be run to completion"));
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// `beam_rt -noshell -eval <expr>` through the built binary.
fn beam_rt_eval(expr: &str) -> Run {
    run(env!("CARGO_BIN_EXE_beam_rt"), &["-noshell", "-eval", expr])
}

/// The same expression under the oracle, terminated by `-s init stop` (an `erl -noshell`
/// never exits on its own).
fn oracle_eval(oracle: &Path, expr: &str) -> Run {
    let program = oracle
        .to_str()
        .unwrap_or_else(|| panic!("the oracle path is not UTF-8: {}", oracle.display()));
    run(program, &["-noshell", "-eval", expr, "-s", "init", "stop"])
}

// ---------------------------------------------------------------------------------------
// The programs both runtimes are given
// ---------------------------------------------------------------------------------------

/// `monitor/2` on a live pid and on a registered name: both answer a fresh reference.
const EXPR_REFS: &str = "\
P = spawn(fun() -> receive stop -> ok end end), \
true = register(monslave, P), \
RP = erlang:monitor(process, P), \
RN = erlang:monitor(process, monslave), \
io:format(\"~p~n\", [{is_reference(RP), is_reference(RN), RP =/= RN}]), \
halt(0).";

/// `demonitor/2`'s `[info]` answers the **real** boolean: `true` while the monitor was on
/// record, `false` once it is gone; `demonitor/1` always answers `true`.
const EXPR_LIFECYCLE: &str = "\
P = spawn(fun() -> receive stop -> ok end end), \
R = erlang:monitor(process, P), \
First = erlang:demonitor(R, [info]), \
Second = erlang:demonitor(R, [info]), \
Plain = erlang:demonitor(R), \
io:format(\"~p~n\", [{First, Second, Plain}]), \
halt(0).";

/// `demonitor/2`'s `[flush]` removes a queued `{'DOWN', …}`: the helper is stopped, the
/// probing process blocks on an unrelated message (which lets the helper run to completion
/// and the DOWN land in the mailbox), then the flush removes it and the follow-up receive
/// of the DOWN finds nothing.
const EXPR_FLUSH: &str = "\
P = spawn(fun() -> receive stop -> ok end end), \
R = erlang:monitor(process, P), \
P ! stop, \
receive never -> ok after 50 -> ok end, \
Flushed = erlang:demonitor(R, [flush]), \
Leftover = receive {'DOWN', R, _, _, _} -> down after 0 -> none end, \
io:format(\"~p~n\", [{Flushed, Leftover}]), \
halt(0).";

/// The three flows and the value both runtimes must print for them.
const FLOWS: [(&str, &str, &str); 3] = [
    ("monitor/2 on a pid and on a registered name", EXPR_REFS, "{true,true,true}"),
    (
        "demonitor/1 and demonitor/2 [info]",
        EXPR_LIFECYCLE,
        "{true,false,true}",
    ),
    (
        "demonitor/2 [flush] drops a queued DOWN",
        EXPR_FLUSH,
        "{true,none}",
    ),
];

/// The malformed calls both runtimes reject with `badarg`.
const REJECTIONS: [(&str, &str); 11] = [
    ("monitor(process, 42)", "erlang:monitor(process, 42)"),
    ("monitor(bogus, self())", "erlang:monitor(bogus, self())"),
    ("monitor(time_offset, self())", "erlang:monitor(time_offset, self())"),
    (
        "monitor(process, Port)",
        "erlang:monitor(process, open_port({spawn,\"true\"}, []))",
    ),
    ("monitor/3 option [foo]", "erlang:monitor(process, self(), [foo])"),
    ("monitor/3 non-list options", "erlang:monitor(process, self(), foo)"),
    (
        "monitor/3 option {alias, bogus}",
        "erlang:monitor(process, self(), [{alias, bogus}])",
    ),
    (
        "monitor/3 option 3-tuple {tag, x, extra}",
        "erlang:monitor(process, self(), [{tag, x, extra}])",
    ),
    ("demonitor(42)", "erlang:demonitor(42)"),
    (
        "demonitor/2 option [bogus]",
        "erlang:demonitor(make_ref(), [bogus])",
    ),
    (
        "demonitor/2 non-list options",
        "erlang:demonitor(make_ref(), foo)",
    ),
];

/// The target/option forms the oracle accepts and this runtime **adopts**: a port
/// monitor (recorded on the runtime's authoritative port table — `beam_io`'s
/// `PortRegistry`, the one `open_port/2` allocates from), a per-monitor `{tag, _}`, and
/// the three `{alias, _}` modes whose monitor reference doubles as a **reference alias**
/// (a valid `send` destination — pinned end to end by `tests/alias.rs`).
const ADOPTED_TARGETS: [(&str, &str); 5] = [
    (
        "monitor(port, Port)",
        "erlang:monitor(port, open_port({spawn,\"true\"}, []))",
    ),
    (
        "monitor/3 option {tag, x}",
        "erlang:monitor(process, self(), [{tag, x}])",
    ),
    (
        "monitor/3 option {alias, explicit_unalias}",
        "erlang:monitor(process, self(), [{alias, explicit_unalias}])",
    ),
    (
        "monitor/3 option {alias, demonitor}",
        "erlang:monitor(process, self(), [{alias, demonitor}])",
    ),
    (
        "monitor/3 option {alias, reply_demonitor}",
        "erlang:monitor(process, self(), [{alias, reply_demonitor}])",
    ),
];

// ---------------------------------------------------------------------------------------
// The compiled fixture (a `-eval` term is heap-resident; only a module carries a literal)
// ---------------------------------------------------------------------------------------

/// The `{tag, Tag}` fixture.
///
/// `lit/0`'s `{lit, tag}` is a **constant** term, so the compiler places it in the module's
/// literal area — the one shape `-eval` cannot express, because every term the evaluator builds
/// lives on a process heap. `heap/0` builds its tag at run time (`{self()}`), which is exactly
/// the heap-resident shape.
const MONTAG_SRC: &str = r#"
-module(montag).
-export([imm/0, lit/0, heap/0]).

%% An immediate tag (an atom): the DOWN message's head is the tag itself.
imm() ->
    P = spawn(fun() -> ok end),
    R = erlang:monitor(process, P, [{tag, imm}]),
    receive {imm, R, process, P, _} -> {imm, true}
    after 1000 -> {imm, timeout}
    end.

%% A module-literal tag: `{lit, tag}` is a constant, so it lives in the literal area.
lit() ->
    P = spawn(fun() -> ok end),
    R = erlang:monitor(process, P, [{tag, {lit, tag}}]),
    receive {{lit, tag}, R, process, P, _} -> {{lit, tag}, true}
    after 1000 -> {{lit, tag}, timeout}
    end.

%% A heap-resident tag: built at run time, so it is neither an immediate nor a literal.
heap() ->
    P = spawn(fun() -> ok end),
    Tag = {self()},
    R = erlang:monitor(process, P, [{tag, Tag}]),
    receive {Tag, R, process, P, _} -> heap_ok
    after 1000 -> heap_timeout
    end.
"#;

/// `erlc` beside `$ERTS_ORACLE` or on `$PATH` — discovered by **filesystem** only, never by
/// running it.
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(dir) = support::resolve_oracle().as_deref().and_then(Path::parent) {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}

/// The compiled fixture module in one scratch directory (removed on drop).
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

/// Writes and compiles the fixture with `erlc`; `None` after a loud skip when the toolchain
/// cannot produce it.
fn build_fixtures(erlc: &Path) -> Option<Fixtures> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // `$TMPDIR` (via `std::env::temp_dir`); the dir is handed to the oracle as a single `-pa`,
    // so it must not contain a `lists.beam` (this crate's `tests/fixtures/` does; this does not).
    let dir = std::env::temp_dir().join(format!("beam_rt_montag_{}_{n}", std::process::id()));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        support::skip_loudly("monitor", &format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    let source = dir.join("montag.erl");
    if let Err(error) = std::fs::write(&source, MONTAG_SRC) {
        support::skip_loudly("monitor", &format!("cannot write {}: {error}", source.display()));
        return None;
    }
    let output = match Command::new(erlc)
        .arg("-o")
        .arg(&dir)
        .arg("montag.erl")
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            support::skip_loudly("monitor", &format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        support::skip_loudly(
            "monitor",
            &format!(
                "`erlc` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        );
        return None;
    }
    if !dir.join("montag.beam").is_file() {
        support::skip_loudly("monitor", "`erlc` produced no montag.beam");
        return None;
    }
    Some(Fixtures { dir })
}

/// `beam_rt -noshell -pa DIR run DIR/montag.beam montag:<fun>/0` — the scheduled-process route,
/// which prints the result as `~w~n`.
fn run_compiled(fixtures: &Fixtures, function: &str) -> Run {
    let beam = fixtures.beam("montag");
    let beam = beam
        .to_str()
        .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", beam.display()));
    let spec = format!("montag:{function}/0");
    run(
        env!("CARGO_BIN_EXE_beam_rt"),
        &["-noshell", "-pa", fixtures.dir_str(), "run", beam, &spec],
    )
}

/// The live oracle's line for `montag:<fun>/0`, in the shape the `run` action prints.
fn oracle_compiled(oracle: &Path, fixtures: &Fixtures, function: &str) -> Option<String> {
    support::oracle_eval(
        oracle,
        Some(&fixtures.dir),
        &format!("io:format(\"~w~n\",[montag:{function}()]), halt(0)."),
    )
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------
/// Every flow prints exactly what the live oracle prints for the same expression: the
/// reference-answering forms, `demonitor/2`'s real `[info]` boolean and `[flush]`'s removal
/// of a queued `{'DOWN', …}`.
#[test]
fn the_monitor_flows_match_the_live_oracle() {
    let Some(oracle) = support::require_oracle("monitor") else {
        return;
    };
    for (label, expr, expected) in FLOWS {
        let ours = beam_rt_eval(expr);
        assert_eq!(ours.code, 0, "{label}: exit {} stderr {}", ours.code, ours.err);
        assert!(ours.err.is_empty(), "{label}: stderr {}", ours.err);
        assert_eq!(ours.out, expected, "{label}: beam_rt's answer");

        let theirs = oracle_eval(&oracle, expr);
        assert_eq!(
            theirs.code, 0,
            "{label}: the oracle failed: stderr {}",
            theirs.err
        );
        assert_eq!(
            theirs.out, expected,
            "{label}: the recorded value must be the live oracle's answer"
        );
    }
}

/// Every malformed call is the oracle's `badarg` here too: exit 1 with `beam_rt: badarg`,
/// never the `unsupported` capability signal — and the live oracle rejects the same
/// expression, so the rejection is the oracle's own, not a local interpretation.
#[test]
fn malformed_monitor_and_demonitor_calls_are_badarg_in_both_runtimes() {
    let Some(oracle) = support::require_oracle("monitor") else {
        return;
    };
    for (label, expr) in REJECTIONS {
        let ours = beam_rt_eval(expr);
        assert_eq!(ours.code, 1, "{label}: expected exit 1, stderr {}", ours.err);
        assert!(ours.err.contains("badarg"), "{label}: stderr {}", ours.err);
        assert!(
            !ours.err.contains("unsupported"),
            "{label}: a badarg is not a capability gap: {}",
            ours.err
        );

        let theirs = oracle_eval(&oracle, expr);
        assert_ne!(theirs.code, 0, "{label}: the oracle accepted it");
        assert!(
            theirs.err.contains("badarg"),
            "{label}: the oracle's reason should be badarg: {}",
            theirs.err
        );
    }
}

/// The forms the oracle accepts and this runtime now **adopts** — a port monitor, a per-monitor
/// `{tag, _}` and the three `{alias, _}` modes — succeed silently (exit 0, nothing printed), and
/// the live oracle answers a reference for each, so the two runtimes agree rather than both
/// rejecting. (The alias modes are exercised as *send* destinations end to end by
/// `tests/alias.rs`; here only their acceptance is pinned.)
#[test]
fn the_formerly_inexpressible_targets_are_adopted_like_the_oracle() {
    let Some(oracle) = support::require_oracle("monitor") else {
        return;
    };
    for (label, expr) in ADOPTED_TARGETS {
        let ours = beam_rt_eval(expr);
        assert_eq!(ours.code, 0, "{label}: expected exit 0, stderr {}", ours.err);
        assert_eq!(ours.out, "", "{label}: a monitor prints nothing");
    }
    // The oracle answers a reference for both, which is what these cases compare against.
    for (label, expr) in ADOPTED_TARGETS {
        let accepted = format!("io:format(\"~p~n\", [is_reference({expr})]), halt(0).");
        let theirs = oracle_eval(&oracle, &accepted);
        assert_eq!(
            theirs.code, 0,
            "{label}: the oracle should accept it: stderr {}",
            theirs.err
        );
        assert_eq!(theirs.out, "true", "{label}: the oracle answers a reference");
    }

    // A `{Name, Node}` pair naming **another** node needs distribution (contract A.5),
    // which this runtime does not implement: the same typed gap, never a local monitor.
    let remote = beam_rt_eval("erlang:monitor(process, {some_name, 'other@host'})");
    assert_eq!(remote.code, 3, "a remote monitor: stderr {}", remote.err);
    assert!(
        remote.err
            .contains("unsupported: erlang:monitor/2 on a {Name, Node} of another node"),
        "a remote monitor is a capability gap: {}",
        remote.err
    );
}

/// The forms that *are* adopted succeed silently (exit 0, nothing printed) — a monitor on
/// the caller itself and on an unregistered name (the oracle still answers a reference,
/// which then fires `noproc`).
#[test]
fn the_adopted_targets_succeed_silently() {
    for expr in [
        "erlang:monitor(process, self())",
        "erlang:monitor(process, some_unregistered_name)",
        "erlang:monitor(process, self(), [])",
        "erlang:demonitor(make_ref())",
        "erlang:demonitor(make_ref(), [])",
    ] {
        let run = beam_rt_eval(expr);
        assert_eq!(run.code, 0, "{expr}: stderr {}", run.err);
        assert!(run.out.is_empty(), "{expr}: stdout {}", run.out);
        assert!(run.err.is_empty(), "{expr}: stderr {}", run.err);
    }
}

/// `erlang:monitor/3`'s `{tag, T}` is served for an **immediate** (an atom here) and for a
/// **module-literal** tag: both runtimes deliver the `{'DOWN', …}` with that term as its head.
/// A **heap-resident** tag — a term built on a process heap, so neither immediate nor literal —
/// is this runtime's typed capability gap (exit 3, one `unsupported:` line), where the oracle
/// performs the monitor; the acceptance rule is `beam_process`'s `is_heap_independent`, which a
/// monitor record's plain, unscanned value requires.
///
/// The case must run **compiled**: every term the `-eval` evaluator builds is heap-resident, so
/// it cannot express the literal-area shape at all.
#[test]
fn a_tag_is_an_immediate_or_a_module_literal_and_a_heap_resident_one_is_the_typed_gap() {
    let Some(oracle) = support::require_oracle("monitor") else {
        return;
    };
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly("monitor", "no `erlc` available (and none beside $ERTS_ORACLE)");
        return;
    };
    let Some(fixtures) = build_fixtures(&erlc) else {
        return;
    };

    for (label, function, expected) in [
        ("an immediate (atom) tag", "imm", "{imm,true}"),
        ("a module-literal tag", "lit", "{{lit,tag},true}"),
    ] {
        let ours = run_compiled(&fixtures, function);
        assert_eq!(
            ours.code, 0,
            "{label}: `beam_rt run montag:{function}/0` must exit 0, got {}: {}",
            ours.code, ours.err
        );
        assert!(
            !ours.err.contains("unsupported"),
            "{label}: an accepted tag is not a capability gap: {}",
            ours.err
        );
        assert_eq!(ours.out, expected, "{label}: beam_rt's answer");

        let theirs = oracle_compiled(&oracle, &fixtures, function)
            .unwrap_or_else(|| panic!("the live oracle could not answer montag:{function}/0"));
        assert_eq!(
            theirs, expected,
            "{label}: the recorded value must be the live oracle's answer"
        );
    }

    // A heap-resident tag. The oracle hands it over; this runtime refuses it with the typed
    // capability gap rather than storing a word a later collection would invalidate.
    let heap = run_compiled(&fixtures, "heap");
    assert_eq!(
        heap.code, 3,
        "a heap-resident tag is a capability gap, stderr {}",
        heap.err
    );
    assert!(
        heap.err.contains("unsupported: monitor/3: tag must be an immediate"),
        "the gap is named: {}",
        heap.err
    );
    assert!(heap.out.is_empty(), "nothing is printed: {}", heap.out);
    let theirs = oracle_compiled(&oracle, &fixtures, "heap")
        .expect("the live oracle could not answer montag:heap/0");
    assert_eq!(
        theirs, "heap_ok",
        "the oracle accepts a heap-resident tag, so the refusal is this runtime's own gap"
    );
}
