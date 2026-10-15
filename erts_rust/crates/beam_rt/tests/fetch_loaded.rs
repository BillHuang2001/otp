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

//! `init:fetch_loaded/0` at the **executable** boundary, cross-checked against the live
//! system Erlang (contract A.7).
//!
//! The oracle's `init` process accumulates a `loaded` list while it runs the boot script's
//! `{primLoad, …}` batches and `{ensure_loaded, Module}` requests, and `code_server:init/3`
//! — its only consumer (`lib/kernel/src/code_server.erl:100`) — drains and clears that list
//! at boot (`erts/preloaded/src/init.erl:720-722`, the `boot_loop` `{From,fetch_loaded}` arm:
//! *"Fetch and reset initially loaded modules"*). Every **post-boot** fetch therefore answers
//! the empty list (`init.erl:792-795`, the main `loop`'s arm), however many modules the node
//! has loaded: `code:ensure_loaded/1`, a `-pa` directory and a later `code:load_file/1` all
//! go to the code server and never re-enter `init`'s list.
//!
//! This runtime has no live `init` process (its `init` is the runtime-native floor, see
//! `src/native/init.rs`), so the answer is the empty list too — the same observable value,
//! reached by folding the code server's consumption into the boot path.
//!
//! Each program below is run under **both** runtimes and their trimmed stdout is compared
//! byte-for-byte; a missing `erl` (or, for the compiled-fixture case, `erlc`) is a **loud**
//! skip via `tests/support/mod.rs`, never a silent pass.

// The shared harness is compiled in whole, but this suite uses only its toolchain discovery
// and its bounded runner — the probes here need each child's exit code, which the harness's
// own `oracle_eval` folds away — so the unused remainder is expected here.
#[allow(dead_code)]
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

/// The loud-skip banner (a missing toolchain must announce itself).
const BANNER: &str = "fetch_loaded";

/// The module the `-pa` case loads: a unique name so it can only come from the added
/// directory, and so it is provably outside the boot script's own load set.
const MARKER: &str = "fetch_loaded_marker";

// ---------------------------------------------------------------------------------------
// The probes
// ---------------------------------------------------------------------------------------

/// One finished probe: its exit code and its trimmed stdout / raw stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Runs `program args` through the shared bounded runner (a probe that blocks is killed and
/// reaped rather than hanging the suite) and collects its exit code and output.
fn run(program: &str, args: &[&str]) -> Run {
    let output = support::run_bounded(Path::new(program), args)
        .unwrap_or_else(|| panic!("`{program}` {args:?} could not be run"));
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The built runtime, running `expr` as its `-eval` program, with `-pa dir` before it when a
/// directory is given (the code-path switch is applied before anything runs — contract A.6).
fn beam_rt(pa: Option<&Path>, expr: &str) -> Run {
    let mut args = vec!["-noshell"];
    let dir;
    if let Some(dir_path) = pa {
        dir = dir_path.display().to_string();
        args.extend_from_slice(&["-pa", &dir]);
    }
    args.extend_from_slice(&["-eval", expr]);
    run(env!("CARGO_BIN_EXE_beam_rt"), &args)
}

/// The **same** program under the oracle, terminated by `-s init stop` (an `erl -noshell`
/// never exits on its own).
fn oracle_eval(oracle: &Path, pa: Option<&Path>, expr: &str) -> Run {
    let program = oracle
        .to_str()
        .unwrap_or_else(|| panic!("the oracle path is not UTF-8: {}", oracle.display()));
    let mut args = vec!["-noshell"];
    let dir;
    if let Some(dir_path) = pa {
        dir = dir_path.display().to_string();
        args.extend_from_slice(&["-pa", &dir]);
    }
    args.extend_from_slice(&["-eval", expr, "-s", "init", "stop"]);
    run(program, &args)
}

/// Both runtimes given the **same** `expr` (and the same `-pa` directory, if any), with their
/// stdout and exit codes asserted equal — the comparison every test below makes.
///
/// Returns `None` (after a loud skip) when no system `erl` is available.
fn both(pa: Option<&Path>, expr: &str) -> Option<(Run, Run)> {
    let oracle = support::require_oracle(BANNER)?;
    Some((beam_rt(pa, expr), oracle_eval(&oracle, pa, expr)))
}

/// Asserts both runtimes exited 0 and printed exactly `expected`.
fn assert_both_print((ours, theirs): (Run, Run), expr: &str, expected: &str) {
    assert_eq!(theirs.code, 0, "oracle `{expr}`: {:?}", theirs.err);
    assert_eq!(ours.code, 0, "beam_rt `{expr}`: {}", ours.err);
    assert_eq!(theirs.out, expected, "oracle `{expr}`: {:?}", theirs.err);
    assert_eq!(
        ours.out, theirs.out,
        "beam_rt must agree with the oracle on `{expr}`"
    );
}

/// The `-eval` program that prints the answer of `init:fetch_loaded/0`.
const PRINT_FETCH_LOADED: &str = "io:format(\"~p~n\",[init:fetch_loaded()]), halt(0).";

// ---------------------------------------------------------------------------------------
// The post-boot answer
// ---------------------------------------------------------------------------------------

/// After boot, `init:fetch_loaded/0` is the empty list — the load record was handed to the
/// code server at boot and cleared (`init.erl:720-722`). A non-empty answer (the code
/// server's own module database) is exactly the divergence this pins.
#[test]
fn fetch_loaded_is_empty_after_boot_like_the_oracle() {
    let Some(runs) = both(None, PRINT_FETCH_LOADED) else {
        return;
    };
    assert_both_print(runs, PRINT_FETCH_LOADED, "[]");
}

/// The empty answer is not vacuous: the runtime really has modules loaded — its code
/// server's database (`code:all_loaded/0`) is non-empty — while `init:fetch_loaded/0` still
/// answers `[]`, under both runtimes.
#[test]
fn the_runtime_has_modules_loaded_but_fetch_loaded_is_empty() {
    let expr = "io:format(\"~p ~p~n\",[length(code:all_loaded()) > 0, init:fetch_loaded()]), halt(0).";
    let Some(runs) = both(None, expr) else {
        return;
    };
    assert_both_print(runs, expr, "true []");
}

/// Loading another module cannot put it into `init`'s (already consumed) record: `code:
/// ensure_loaded/1` goes to the code server, so the post-boot answer stays `[]`.
#[test]
fn fetch_loaded_stays_empty_after_a_module_is_ensured_loaded() {
    let expr = "code:ensure_loaded(lists), io:format(\"~p~n\",[init:fetch_loaded()]), halt(0).";
    let Some(runs) = both(None, expr) else {
        return;
    };
    assert_both_print(runs, expr, "[]");
}

/// A module loaded from an extra `-pa` code-path directory is reported by `code:ensure_loaded/1`
/// as loaded (`{module, MARKER}`) yet still absent from `init:fetch_loaded/0`, which answers
/// `[]` — byte-identically to the oracle. Printing the pair as one tuple keeps the comparison
/// non-vacuous: a runtime that failed to load the marker would print `{error, …}` instead and
/// fail the assertion.
#[test]
fn fetch_loaded_stays_empty_for_a_module_loaded_from_an_extra_code_path() {
    let Some(dir) = build_marker() else {
        return;
    };
    let expr = format!(
        "io:format(\"~p~n\",[{{code:ensure_loaded({MARKER}), init:fetch_loaded()}}]), halt(0)."
    );
    let Some(runs) = both(Some(dir.path()), &expr) else {
        return;
    };
    assert_both_print(runs, &expr, &format!("{{{{module,{MARKER}}},[]}}"));
}

// ---------------------------------------------------------------------------------------
// The `-pa` fixture: a uniquely named module compiled on the fly
// ---------------------------------------------------------------------------------------

/// The compiled marker's scratch directory, removed on drop.
struct MarkerDir {
    root: PathBuf,
}

impl MarkerDir {
    /// The directory holding the compiled marker (the `-pa` directory).
    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for MarkerDir {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.root) {
            eprintln!(
                "warning: could not remove the fixture root {}: {error}",
                self.root.display()
            );
        }
    }
}

/// The compiler: a sibling of the resolved oracle first (the same installation), then
/// `$PATH` — the rule the other compiled-fixture suites use.
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(dir) = support::resolve_oracle().as_deref().and_then(Path::parent) {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}

/// Compiles [`MARKER`] into a fresh scratch directory, or `None` after a loud skip when
/// `erlc` is unavailable or the compile fails.
fn build_marker() -> Option<MarkerDir> {
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly(BANNER, "no `erlc` on $PATH (and none beside the oracle)");
        return None;
    };
    let root = std::env::temp_dir().join(format!(
        "beam_rt_fetch_loaded_{}_{}",
        std::process::id(),
        MARKER
    ));
    // A stale directory from a killed run must not decide the result: start clean.
    match std::fs::remove_dir_all(&root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            support::skip_loudly(BANNER, &format!("cannot clear {}: {error}", root.display()));
            return None;
        }
    }
    if let Err(error) = std::fs::create_dir_all(&root) {
        support::skip_loudly(BANNER, &format!("cannot create {}: {error}", root.display()));
        return None;
    }
    let source = root.join(format!("{MARKER}.erl"));
    let body = format!("%% Generated by `beam_rt/tests/fetch_loaded.rs` (never committed).\n\
                        -module({MARKER}).\n\
                        -export([ok/0]).\n\
                        ok() -> ok.\n");
    if let Err(error) = std::fs::write(&source, body) {
        support::skip_loudly(BANNER, &format!("cannot write {}: {error}", source.display()));
        return None;
    }
    // `+deterministic` keeps the image path-free, like the committed fixtures.
    let output = match Command::new(&erlc)
        .arg("+deterministic")
        .arg("-o")
        .arg(&root)
        .arg(&source)
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
    if !root.join(format!("{MARKER}.beam")).is_file() {
        support::skip_loudly(
            BANNER,
            &format!("`erlc` produced no {}/{MARKER}.beam", root.display()),
        );
        return None;
    }
    Some(MarkerDir { root })
}
