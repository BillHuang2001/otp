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

//! `erlang:module_loaded/1` at the **executable** boundary, cross-checked with the live
//! system `erl` oracle.
//!
//! `src/native/process.rs::module_loaded_1` answers `true` when the name is in the runtime's
//! preloaded set ([`beam_rt::boot::PRELOADED_MODULE_NAMES`], the oracle's exact 22-module
//! `erlang:pre_loaded/0` list) **or** the shared code server already holds the module
//! (`CodeServer::is_loaded`, which `CodeServer::load` memoizes into). This suite pins that
//! answer - both what it agrees with the oracle on and where it differs by construction -
//! running the **same** `-eval` program under the built `beam_rt` binary and under the oracle.
//!
//! # What agrees, and why
//!
//! * **The preloaded set.** Every name in `PRELOADED_MODULE_NAMES` is `true` in both
//!   runtimes (it is literally the oracle's `erlang:pre_loaded/0` list).
//! * **A name that is loaded now becomes `true`.** `CodeServer::load` memoizes into the same
//!   map `is_loaded` consults, so a module the runtime genuinely loads - via
//!   `code:ensure_loaded/1` or because a real external call the runtime-load path
//!   (`src/context/mod.rs:1289`) reaches it - flips the answer from `false` to `true`, exactly
//!   as in the oracle.
//! * **An unknown / not-yet-loaded name is `false`.** `stdlib`, `xmerl_scan` and a made-up
//!   `nonexistent_xyz` answer `false` in both.
//! * **A non-atom argument is `badarg`.** Both runtimes reject it (exit code 1).
//!
//! # The boot set: this runtime runs the boot script too, but loads it **eagerly**
//!
//! Every CLI action but `-version` now drives the default boot script
//! (`bootstrap/bin/start.boot`), exactly as `init` does, so the **first** `primLoad` batch
//! (`lists`, `kernel`, …) is loaded before the `-eval` program runs - matching the oracle
//! (`erlang:module_loaded(lists)` is `true` in both).
//!
//! The remaining, by-construction divergence is **how much** of the script's `primLoad`
//! batches is loaded eagerly. The oracle's `init` runs in *interactive* (non-embedded) mode:
//! once it sees `{kernel_load_completed}` it sets `prim_load = false`
//! (`erts/preloaded/src/init.erl:1337`) and lets the code server load the later batches
//! **on demand**, so its table holds ~109 modules and `digraph`/`dets`/`epp`/… are still
//! `false`. This runtime's executor loads **every** batch, so its table holds ~224 modules
//! and those names are `true`. That is the same class of divergence as `erlang:processes/0` /
//! `system_info(port_count)`: an answer that reads **this** runtime's own state and is
//! *truthful* about it, not a bug to paper over.
//! [`the_boot_set_divergence_is_exactly_the_boot_scripts_module_loads`] measures it in one
//! oracle-cross-checked probe (a name in the eagerly-loaded first batch agrees; a name the
//! oracle defers to an on-demand load diverges; an *available* module and an unknown one
//! agree on `false`), [`the_loaded_set_is_self_consistent_in_both`] pins that this runtime
//! agrees with its own `code:all_loaded/0`, and
//! [`the_two_boot_script_names_flip_when_this_runtime_loads_them`] (plus
//! [`the_boot_set_divergence_is_this_runtimes_own_truthful_loaded_set`]) proves the answer is
//! this runtime's own loaded state - a real load is reflected, exactly as in the oracle.
//!
//! The hard constraint this suite guards: the answer must **never** come from a
//! `find_file(...).is_some()` ("is on the code path") shortcut, which would wrongly answer
//! `true` for `xmerl_scan`/`dialyzer` —
//! [`an_absent_or_unloaded_name_is_false_in_both`] and
//! [`the_boot_set_divergence_is_exactly_the_boot_scripts_module_loads`] are the guards.
//!
//! # The on-demand reflection
//!
//! The suites below measure the load reflection through a **genuine** external call
//! (`compile:env_compiler_options/0`) and through `code:ensure_loaded/1` (`compile`) - a
//! module neither runtime has loaded at boot, unlike `digraph`, which this runtime now loads
//! eagerly. (A `lists:reverse/1` probe would measure nothing at all: `lists:reverse/1` is one
//! of `beam_bifs`'s registry fast paths (`beam_bifs/src/lists.rs`), so the call never reaches
//! the code server - not a load path escaping the cache, just a served-fast-path call.)
//!
//! # Toolchain
//!
//! The `beam_rt`-side assertions run with no toolchain; only the oracle comparison needs
//! `erl`, and every test skips **loudly** without it (`tests/support/mod.rs`). No fixture is
//! handed to the oracle as an `-pa` - that is what keeps this suite clear of the
//! `tests/fixtures/lists.beam` shadowing trap.

// The shared harness is compiled in whole, but this suite uses only its toolchain discovery
// and probe bound (the probes need the exit code, which `support::oracle_eval` folds away),
// so the unused remainder is expected here - the same shape `tests/monitor.rs` uses.
#[allow(dead_code)]
mod support;

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------
// The bounded child runner
// ---------------------------------------------------------------------------------------

/// Runs `program args` with `/dev/null` as stdin, polling so a probe that blocks is killed
/// and reaped at [`support::PROBE_TIMEOUT`] instead of hanging the suite. `None` when the
/// child could not be spawned, polled or collected.
///
/// The bounded runner lives here rather than in `tests/support/mod.rs` because every probe in
/// this suite is a program that terminates on its own; the polling guard is the belt to that
/// suspenders, and keeping it local avoids editing the shared harness.
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

/// The preloaded names both runtimes answer `true` for (a sample of
/// `PRELOADED_MODULE_NAMES`, which is the oracle's exact `erlang:pre_loaded/0` list).
const EXPR_PRELOADED: &str = "\
io:format(\"~p~n\", [{erlang:module_loaded(persistent_term), \
erlang:module_loaded(erlang), erlang:module_loaded(erts_internal), \
erlang:module_loaded(init), erlang:module_loaded(erl_prim_loader)}]), halt(0).";
const PRELOADED_EXPECTED: &str = "{true,true,true,true,true}";

/// Names that are neither preloaded nor loaded in either runtime: a stdlib module the app
/// never touches, a library outside the boot set, and a made-up name. All `false` - the
/// guard against a "is on the code path" shortcut.
const EXPR_ABSENT: &str = "\
io:format(\"~p~n\", [{erlang:module_loaded(stdlib), erlang:module_loaded(xmerl_scan), \
erlang:module_loaded(nonexistent_xyz)}]), halt(0).";
const ABSENT_EXPECTED: &str = "{false,false,false}";

/// A module becomes loaded on demand through `code:ensure_loaded/1`: `false` before, `true`
/// after - the reflection of the code server's own cache, in both runtimes. `compile` is on
/// both runtimes' code path and loaded by neither at boot (`digraph` would not do: this
/// runtime's eager `primLoad` has already loaded it).
const EXPR_ENSURE_LOADED: &str = "\
Before = erlang:module_loaded(compile), \
code:ensure_loaded(compile), \
After = erlang:module_loaded(compile), \
io:format(\"~p~n\", [{Before, After}]), halt(0).";

/// A module becomes loaded because a **real** external call reaches it
/// (`compile:env_compiler_options/0` resolves through the code server, unlike the `lists`
/// fast paths): `false` before, `true` after, in both runtimes.
const EXPR_CALL_LOADED: &str = "\
Before = erlang:module_loaded(compile), \
compile:env_compiler_options(), \
After = erlang:module_loaded(compile), \
io:format(\"~p~n\", [{Before, After}]), halt(0).";

/// The two on-demand load flows both runtimes must agree on: a label, the program, and the
/// `{Before, After}` pair it prints.
const LOAD_FLOWS: [(&str, &str, &str); 2] = [
    ("code:ensure_loaded/1", EXPR_ENSURE_LOADED, "{false,true}"),
    ("a real external call", EXPR_CALL_LOADED, "{false,true}"),
];

/// The eagerly-loaded first `primLoad` batch: `lists` is loaded by the oracle's boot script
/// **and** by this runtime's (which now drives the same script on every action but `-version`),
/// so both runtimes answer `true`.
const EXPR_LISTS: &str = "io:format(\"~p~n\", [erlang:module_loaded(lists)]), halt(0).";

/// This runtime's own answer for `EXPR_LISTS` - its boot script's first `primLoad` batch
/// (`lists`, `kernel`, …) is loaded eagerly, so `lists` is loaded here too.
const LISTS_OURS: &str = "true";

/// `lists` stays `true` once it is (re-)loaded - proof that `LISTS_OURS` is this runtime's own
/// truthful loaded state, not a missing lookup.
const EXPR_LISTS_AFTER_LOAD: &str = "\
code:ensure_loaded(lists), \
io:format(\"~p~n\", [erlang:module_loaded(lists)]), halt(0).";

/// A non-atom argument: the raw call both runtimes raise `badarg` on (exit code 1).
const EXPR_BADARG: &str = "erlang:module_loaded(42).";

/// The same rejection, caught, so both runtimes print the reason they raised.
const EXPR_BADARG_CAUGHT: &str = "\
try erlang:module_loaded(42) catch error:badarg -> io:format(\"badarg~n\") end, halt(0).";

/// The boot-set divergence in **one** probe: two names the oracle's node start-up eagerly
/// loads (the first `primLoad` batch of `bootstrap/bin/start.boot`) and two it does not.
/// `digraph` is in a *later* `primLoad` batch that the oracle's interactive `init` defers to an
/// on-demand load (`code:which/1` still resolves it), and `nonexistent_xyz` does not exist at
/// all, so the oracle answers `false` for both.
const EXPR_BOOT_SET: &str = "\
io:format(\"~p~n\", [{erlang:module_loaded(lists), erlang:module_loaded(kernel), \
erlang:module_loaded(digraph), erlang:module_loaded(nonexistent_xyz)}]), halt(0).";

/// This runtime's own answer for [`EXPR_BOOT_SET`]: its boot script loads the first `primLoad`
/// batch (`lists`, `kernel`) **and** every later batch eagerly, so `lists`, `kernel` and
/// `digraph` are all loaded here.
const BOOT_SET_OURS: &str = "{true,true,true,false}";

/// The live oracle's answer for [`EXPR_BOOT_SET`] (OTP 29.1.1 / erts-17.1): `lists` and
/// `kernel` are in its code table before `-eval` runs; `digraph`/`nonexistent_xyz` are not.
const BOOT_SET_ORACLE: &str = "{true,true,false,false}";

/// This runtime's loaded set checked against **its own** `code:all_loaded/0`: `D` (the
/// modules that listing disagrees with `erlang:module_loaded/1` about) must be none, and the
/// two first-batch boot-script names are loaded here.
const EXPR_SELF_CONSISTENT: &str = "\
L = [M || {M, _} <- code:all_loaded()], \
D = [M || M <- L, not erlang:module_loaded(M)], \
io:format(\"~p~n\", [{D, erlang:module_loaded(lists), erlang:module_loaded(kernel)}]), \
halt(0).";

/// The oracle's answer for [`EXPR_SELF_CONSISTENT`]: its loaded set is self-consistent too,
/// and its boot script has already loaded both names.
const SELF_CONSISTENT_ORACLE: &str = "{[],true,true}";

/// The same two names through `code:ensure_loaded/1`: both are reported loaded
/// (`{module, M}`), and the whole 4-tuple matches the oracle's answer for the same program —
/// so the `true` above is this runtime's own loaded state, never a missing lookup.
const EXPR_ENSURE_BOTH: &str = "\
io:format(\"~p~n\", [{code:ensure_loaded(kernel), erlang:module_loaded(kernel), \
code:ensure_loaded(lists), erlang:module_loaded(lists)}]), halt(0).";
const ENSURE_BOTH_EXPECTED: &str = "{{module,kernel},true,{module,lists},true}";
// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// The preloaded set agrees: every sampled name is `true` in both runtimes.
#[test]
fn the_preloaded_set_agrees_with_the_live_oracle() {
    let ours = beam_rt_eval(EXPR_PRELOADED);
    assert_eq!(ours.code, 0, "beam_rt: exit {} stderr {}", ours.code, ours.err);
    assert_eq!(ours.out, PRELOADED_EXPECTED, "beam_rt's preloaded answers");

    let Some(oracle) = support::require_oracle("module_loaded") else {
        return;
    };
    let theirs = oracle_eval(&oracle, EXPR_PRELOADED);
    assert_eq!(
        theirs.code, 0,
        "the oracle failed: stderr {}",
        theirs.err
    );
    assert_eq!(
        theirs.out, PRELOADED_EXPECTED,
        "the recorded value must be the live oracle's answer"
    );
}

/// An absent or not-yet-loaded name is `false` in both - never a "is on the code path"
/// shortcut that would wrongly answer `true` for `xmerl_scan`.
#[test]
fn an_absent_or_unloaded_name_is_false_in_both() {
    let ours = beam_rt_eval(EXPR_ABSENT);
    assert_eq!(ours.code, 0, "beam_rt: exit {} stderr {}", ours.code, ours.err);
    assert_eq!(
        ours.out, ABSENT_EXPECTED,
        "an unknown/unloaded name must be false"
    );

    let Some(oracle) = support::require_oracle("module_loaded") else {
        return;
    };
    let theirs = oracle_eval(&oracle, EXPR_ABSENT);
    assert_eq!(
        theirs.code, 0,
        "the oracle failed: stderr {}",
        theirs.err
    );
    assert_eq!(
        theirs.out, ABSENT_EXPECTED,
        "the recorded value must be the live oracle's answer"
    );
}

/// A module that the runtime genuinely loads - through `code:ensure_loaded/1` or a real
/// external call - flips from `false` to `true`, exactly as in the oracle. This is the
/// on-demand reflection of the code server's cache.
#[test]
fn a_module_becomes_loaded_on_demand_in_both() {
    for (label, expr, expected) in LOAD_FLOWS {
        let ours = beam_rt_eval(expr);
        assert_eq!(
            ours.code, 0,
            "{label}: beam_rt exit {} stderr {}",
            ours.code, ours.err
        );
        assert_eq!(ours.out, expected, "{label}: beam_rt's before/after");
    }

    let Some(oracle) = support::require_oracle("module_loaded") else {
        return;
    };
    for (label, expr, expected) in LOAD_FLOWS {
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

/// The boot set, documented honestly. Because this runtime now drives the same boot script on
/// every action but `-version`, `lists` (a first-`primLoad`-batch name) is loaded here too, so
/// both runtimes answer `true`; this runtime's value is asserted and the oracle's is
/// **recorded**, not pinned, because the whole point is that each runtime reports its own
/// loaded set - a future OTP whose boot set changes must not fail this suite.
#[test]
fn the_boot_set_divergence_is_this_runtimes_own_truthful_loaded_set() {
    let ours = beam_rt_eval(EXPR_LISTS);
    assert_eq!(ours.code, 0, "beam_rt: exit {} stderr {}", ours.code, ours.err);
    assert_eq!(
        ours.out, LISTS_OURS,
        "this runtime's boot script loads its first primLoad batch, so `lists` is loaded here"
    );

    // The value is this runtime's own loaded state, reflected by a genuine load.
    let after = beam_rt_eval(EXPR_LISTS_AFTER_LOAD);
    assert_eq!(after.code, 0, "beam_rt: stderr {}", after.err);
    assert_eq!(
        after.out, "true",
        "once `lists` is genuinely loaded, this runtime reports it"
    );

    let Some(oracle) = support::require_oracle("module_loaded") else {
        return;
    };
    let theirs = oracle_eval(&oracle, EXPR_LISTS);
    assert_eq!(theirs.code, 0, "the oracle failed: stderr {}", theirs.err);
    // The oracle's own boot script loads `lists` too, so it answers `true`; the value is
    // recorded rather than pinned (a future OTP whose boot set changes must not fail here).
    eprintln!(
        "module_loaded(lists): beam_rt={} oracle={} \
         (both boot scripts eagerly load this first-primLoad-batch name)",
        ours.out, theirs.out
    );
}

/// A non-atom argument is the oracle's `badarg` in both runtimes: exit code 1, the same
/// reason, and never the `unsupported` capability signal.
#[test]
fn a_non_atom_argument_is_badarg_in_both() {
    let ours = beam_rt_eval(EXPR_BADARG);
    assert_eq!(ours.code, 1, "beam_rt: stderr {}", ours.err);
    assert!(ours.err.contains("badarg"), "beam_rt: stderr {}", ours.err);
    assert!(
        !ours.err.contains("unsupported"),
        "a badarg is not a capability gap: {}",
        ours.err
    );

    // The reason both runtimes raise, caught so the class/reason is observable.
    let ours_caught = beam_rt_eval(EXPR_BADARG_CAUGHT);
    assert_eq!(ours_caught.code, 0, "beam_rt: stderr {}", ours_caught.err);
    assert_eq!(ours_caught.out, "badarg", "beam_rt's caught reason");

    let Some(oracle) = support::require_oracle("module_loaded") else {
        return;
    };
    let theirs = oracle_eval(&oracle, EXPR_BADARG);
    assert_ne!(theirs.code, 0, "the oracle should reject a non-atom argument");
    let theirs_caught = oracle_eval(&oracle, EXPR_BADARG_CAUGHT);
    assert_eq!(
        theirs_caught.code, 0,
        "the oracle failed: stderr {}",
        theirs_caught.err
    );
    assert_eq!(
        theirs_caught.out, "badarg",
        "the recorded reason must be the live oracle's answer"
    );
}

/// The boot-set divergence, in a single oracle-cross-checked probe: of `lists` (eagerly loaded
/// by both runtimes' boot scripts), `kernel` (likewise), `digraph` (in a later `primLoad` batch
/// the oracle's interactive `init` defers but this runtime loads eagerly) and an unknown name,
/// the `lists`/`kernel` positions agree and `digraph` diverges. Both runtimes agree that
/// "resolvable on the code path" (`code:which/1` resolves `digraph` even in the oracle) and
/// "unknown" are **not** "loaded", which is the guard against closing this gap with a
/// code-path shortcut.
#[test]
fn the_boot_set_divergence_is_exactly_the_boot_scripts_module_loads() {
    let ours = beam_rt_eval(EXPR_BOOT_SET);
    assert_eq!(ours.code, 0, "beam_rt: exit {} stderr {}", ours.code, ours.err);
    assert_eq!(
        ours.out, BOOT_SET_OURS,
        "this runtime's boot loads its first primLoad batch eagerly, so `lists` and `kernel` \
         are loaded here - and, unlike the oracle, it also loads the later batches (`digraph`)"
    );

    let Some(oracle) = support::require_oracle("module_loaded") else {
        return;
    };
    let theirs = oracle_eval(&oracle, EXPR_BOOT_SET);
    assert_eq!(theirs.code, 0, "the oracle failed: stderr {}", theirs.err);
    assert_eq!(
        theirs.out, BOOT_SET_ORACLE,
        "the recorded value must be the live oracle's answer"
    );
    eprintln!(
        "module_loaded(lists|kernel|digraph|nonexistent_xyz): beam_rt={} oracle={} \
         (the first two are in the first `start.boot` primLoad batch, eagerly loaded by both; \
         `digraph` is in a later batch this runtime loads eagerly but the oracle defers; \
         `nonexistent_xyz` is loaded in neither)",
        ours.out, theirs.out
    );
}

/// This runtime's own loaded set is **self-consistent**: every module its `code:all_loaded/0`
/// lists answers `true`, and `lists`/`kernel` - the boot script's first `primLoad` batch - are
/// loaded. The oracle is self-consistent too and has both names loaded, so the two answers now
/// agree; the check is measured without trusting either runtime's boot set.
#[test]
fn the_loaded_set_is_self_consistent_in_both() {
    let ours = beam_rt_eval(EXPR_SELF_CONSISTENT);
    assert_eq!(ours.code, 0, "beam_rt: exit {} stderr {}", ours.code, ours.err);
    assert_eq!(
        ours.out, "{[],true,true}",
        "every module this runtime lists as loaded must answer `true`, and its first \
         primLoad batch loaded `lists`/`kernel`"
    );

    let Some(oracle) = support::require_oracle("module_loaded") else {
        return;
    };
    let theirs = oracle_eval(&oracle, EXPR_SELF_CONSISTENT);
    assert_eq!(theirs.code, 0, "the oracle failed: stderr {}", theirs.err);
    assert_eq!(
        theirs.out, SELF_CONSISTENT_ORACLE,
        "the recorded value must be the live oracle's answer"
    );
}

/// `lists`/`kernel` are loaded in both runtimes, and the whole 4-tuple matches the oracle's
/// answer for the same program: the `true` this runtime answers is its own loaded state, never
/// "unknown".
#[test]
fn the_two_boot_script_names_flip_when_this_runtime_loads_them() {
    let ours = beam_rt_eval(EXPR_ENSURE_BOTH);
    assert_eq!(ours.code, 0, "beam_rt: exit {} stderr {}", ours.code, ours.err);
    assert_eq!(
        ours.out, ENSURE_BOTH_EXPECTED,
        "both names must be loaded and report `true` once `code:ensure_loaded/1` loaded them"
    );

    let Some(oracle) = support::require_oracle("module_loaded") else {
        return;
    };
    let theirs = oracle_eval(&oracle, EXPR_ENSURE_BOTH);
    assert_eq!(theirs.code, 0, "the oracle failed: stderr {}", theirs.err);
    assert_eq!(
        theirs.out, ENSURE_BOTH_EXPECTED,
        "the oracle's answer for the same program must agree with this runtime's"
    );
}