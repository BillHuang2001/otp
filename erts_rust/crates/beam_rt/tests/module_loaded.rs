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
//! answer - both what it agrees with the oracle on and the one place it differs by
//! construction - running the **same** `-eval` program under the built `beam_rt` binary and
//! under the oracle.
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
//! # The one by-construction divergence: the **boot set size**
//!
//! The oracle's node start-up runs `bootstrap/bin/start.boot`, whose three `primLoad` batches
//! (29 + 86 + 87 names) put `lists`, `kernel` and ~200 other modules in its code table, so
//! `erlang:module_loaded(lists)` is `true` there the moment the node starts. This runtime
//! loads only its 22 preloaded `.beam`s at boot (its `-eval`/`run` boot does not run the boot
//! script), so the same probes answer `false` until those modules are actually loaded. That is
//! the same class of divergence as `erlang:processes/0` / `system_info(port_count)`: an answer
//! that reads **this** runtime's own state and is *truthful* about it, not a bug to paper
//! over. [`the_boot_set_divergence_is_exactly_the_boot_scripts_module_loads`] measures it in
//! one oracle-cross-checked probe (the two boot-script names differ; an *available* module and
//! an unknown one do not), [`the_loaded_set_is_self_consistent_in_both`] pins that this
//! runtime agrees with its own `code:all_loaded/0`, and
//! [`the_two_boot_script_names_flip_when_this_runtime_loads_them`] (plus
//! [`the_boot_set_divergence_is_this_runtimes_own_truthful_loaded_set`]) proves the `false` is
//! a truthful "not loaded", not a missing lookup — a real load flips it, exactly as in the
//! oracle.
//!
//! The hard constraint this suite guards: the answer must **never** come from a
//! `find_file(...).is_some()` ("is on the code path") shortcut, which would wrongly answer
//! `true` for `xmerl_scan`/`dialyzer`/`digraph` —
//! [`an_absent_or_unloaded_name_is_false_in_both`] and
//! [`the_boot_set_divergence_is_exactly_the_boot_scripts_module_loads`] are the guards.
//!
//! # A `lists:reverse` gotcha worth naming
//!
//! The obvious probe - `lists:reverse([1,2,3]), erlang:module_loaded(lists)` - does **not**
//! force `lists` to load here: `lists:reverse/1` is one of `beam_bifs`'s registry fast paths
//! (`beam_bifs/src/lists.rs`), so the call never reaches the code server and `lists` genuinely
//! stays unloaded. That is not a load path escaping the cache; it is a served-fast-path call.
//! The suites below therefore measure the load reflection through a **genuine** external call
//! (`digraph:new/0`) and through `code:ensure_loaded/1`, both of which do load the module.
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
/// after - the reflection of the code server's own cache, in both runtimes (`digraph` is not
/// in the boot set of either).
const EXPR_ENSURE_LOADED: &str = "\
Before = erlang:module_loaded(digraph), \
code:ensure_loaded(digraph), \
After = erlang:module_loaded(digraph), \
io:format(\"~p~n\", [{Before, After}]), halt(0).";

/// A module becomes loaded because a **real** external call reaches it (`digraph:new/0`
/// resolves through the code server, unlike the `lists` fast paths): `false` before, `true`
/// after, in both runtimes.
const EXPR_CALL_LOADED: &str = "\
Before = erlang:module_loaded(digraph), \
digraph:new(), \
After = erlang:module_loaded(digraph), \
io:format(\"~p~n\", [{Before, After}]), halt(0).";

/// The two on-demand load flows both runtimes must agree on: a label, the program, and the
/// `{Before, After}` pair it prints.
const LOAD_FLOWS: [(&str, &str, &str); 2] = [
    ("code:ensure_loaded/1", EXPR_ENSURE_LOADED, "{false,true}"),
    ("a real external call", EXPR_CALL_LOADED, "{false,true}"),
];

/// The boot-set divergence: `lists` is loaded by the oracle's boot script but not by this
/// runtime's 22-module preloaded boot, so the two runtimes answer differently.
const EXPR_LISTS: &str = "io:format(\"~p~n\", [erlang:module_loaded(lists)]), halt(0).";

/// This runtime's own answer for `EXPR_LISTS` - the 22-module preloaded boot loads no
/// `lists.beam`.
const LISTS_OURS: &str = "false";

/// `lists` becomes `true` here too once it is genuinely loaded - proof that `LISTS_OURS` is
/// the truthful "not loaded yet", not a missing lookup.
const EXPR_LISTS_AFTER_LOAD: &str = "\
code:ensure_loaded(lists), \
io:format(\"~p~n\", [erlang:module_loaded(lists)]), halt(0).";

/// A non-atom argument: the raw call both runtimes raise `badarg` on (exit code 1).
const EXPR_BADARG: &str = "erlang:module_loaded(42).";

/// The same rejection, caught, so both runtimes print the reason they raised.
const EXPR_BADARG_CAUGHT: &str = "\
try erlang:module_loaded(42) catch error:badarg -> io:format(\"badarg~n\") end, halt(0).";

/// The boot-set divergence in **one** probe: two names the oracle's node start-up loads
/// (`bootstrap/bin/start.boot`'s three `primLoad` batches — 29 + 86 + 87 module names) and
/// two it does not. `digraph` is only *available* there (`code:which/1` resolves it) and
/// `nonexistent_xyz` does not exist at all, so both runtimes answer `false` for them.
const EXPR_BOOT_SET: &str = "\
io:format(\"~p~n\", [{erlang:module_loaded(lists), erlang:module_loaded(kernel), \
erlang:module_loaded(digraph), erlang:module_loaded(nonexistent_xyz)}]), halt(0).";

/// This runtime's own answer for [`EXPR_BOOT_SET`]: its `-eval` boot loads only the 22
/// preloaded `.beam`s, so neither `lists` nor `kernel` is loaded here.
const BOOT_SET_OURS: &str = "{false,false,false,false}";

/// The live oracle's answer for [`EXPR_BOOT_SET`] (OTP 29.1.1 / erts-17.1): `lists` and
/// `kernel` are in its code table before `-eval` runs; `digraph`/`nonexistent_xyz` are not.
const BOOT_SET_ORACLE: &str = "{true,true,false,false}";

/// This runtime's loaded set checked against **its own** `code:all_loaded/0`: `D` (the
/// modules that listing disagrees with `erlang:module_loaded/1` about) must be none, and the
/// two boot-script names are not loaded here.
const EXPR_SELF_CONSISTENT: &str = "\
L = [M || {M, _} <- code:all_loaded()], \
D = [M || M <- L, not erlang:module_loaded(M)], \
io:format(\"~p~n\", [{D, erlang:module_loaded(lists), erlang:module_loaded(kernel)}]), \
halt(0).";

/// The oracle's answer for [`EXPR_SELF_CONSISTENT`]: its loaded set is self-consistent too,
/// and its boot script has already loaded both names.
const SELF_CONSISTENT_ORACLE: &str = "{[],true,true}";

/// The same two names, **after** this runtime has genuinely loaded them through
/// `code:ensure_loaded/1`: both flip to `true`, and the whole 4-tuple then matches the
/// oracle's answer for the same program — so the `false` above is "not loaded here", never
/// "unknown".
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

/// The **boot-set divergence**, documented honestly. This runtime's own value is asserted
/// (`false`); the oracle's differing value is **recorded**, not pinned, because the whole
/// point is that the two runtimes start from different loaded sets - a future OTP whose boot
/// set changes must not fail this suite.
#[test]
fn the_boot_set_divergence_is_this_runtimes_own_truthful_loaded_set() {
    let ours = beam_rt_eval(EXPR_LISTS);
    assert_eq!(ours.code, 0, "beam_rt: exit {} stderr {}", ours.code, ours.err);
    assert_eq!(
        ours.out, LISTS_OURS,
        "this runtime loads only its 22 preloaded modules at boot, so `lists` is not loaded"
    );

    // The value is truthful, not a missing lookup: a genuine load flips it.
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
    // RECORDED, NOT ASSERTED - the oracle's boot script loads ~109 modules, so it answers
    // `true` here where this runtime answers `false`. The divergence is by construction
    // (this runtime's loaded set is its own), so its exact value is recorded rather than
    // pinned.
    eprintln!(
        "module_loaded(lists): beam_rt={} oracle={} \
         (boot-set divergence, by construction: 22 preloaded vs the oracle's boot script)",
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

/// The boot-set divergence, in a single oracle-cross-checked probe: of `lists` (loaded by
/// the oracle's boot script), `kernel` (likewise), `digraph` (only *available* — `code:which/1`
/// resolves it) and an unknown name, exactly the two boot-script names differ. Both runtimes
/// agree that "resolvable on the code path" and "unknown" are **not** "loaded", which is the
/// guard against closing this gap with a code-path shortcut.
#[test]
fn the_boot_set_divergence_is_exactly_the_boot_scripts_module_loads() {
    let ours = beam_rt_eval(EXPR_BOOT_SET);
    assert_eq!(ours.code, 0, "beam_rt: exit {} stderr {}", ours.code, ours.err);
    assert_eq!(
        ours.out, BOOT_SET_OURS,
        "this runtime's boot loads only the 22 preloaded modules, so neither `lists` nor \
         `kernel` is loaded here"
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
         (the first two are in the oracle's `start.boot` primLoad batches and absent from \
         this runtime's preloaded-only boot; the last two are loaded in neither)",
        ours.out, theirs.out
    );
}

/// This runtime's own loaded set is **self-consistent**: every module its `code:all_loaded/0`
/// lists answers `true`, and the two boot-script names it has not loaded answer `false`. The
/// oracle is self-consistent too and has both names loaded, so the two answers differ in
/// exactly those two positions — the same divergence, measured without trusting either
/// runtime's boot set.
#[test]
fn the_loaded_set_is_self_consistent_in_both() {
    let ours = beam_rt_eval(EXPR_SELF_CONSISTENT);
    assert_eq!(ours.code, 0, "beam_rt: exit {} stderr {}", ours.code, ours.err);
    assert_eq!(
        ours.out, "{[],false,false}",
        "every module this runtime lists as loaded must answer `true`, and `lists`/`kernel` \
         are not loaded here"
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

/// `lists`/`kernel` flip to `true` once this runtime genuinely loads them, and the whole
/// 4-tuple then matches the oracle's answer for the same program: the `false` this runtime
/// answers above is "not loaded here", never "unknown".
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