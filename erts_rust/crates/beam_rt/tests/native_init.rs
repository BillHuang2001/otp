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

//! `init:stop/0,1` and the automatic prefix of `init:get_arguments/0` at the **executable**
//! boundary (contracts A.6/A.7).
//!
//! `src/native/init.rs`'s unit tests pin the BIF layer against the runtime's own state;
//! this suite pins what a user sees: the built `beam_rt` binary, the runtime-native floor,
//! the `-eval` driver and the CLI's exit-code mapping, with the live system `erl` oracle
//! running the **same** program.
//!
//! # Oracle ground truth (system `erl`, OTP 29 / `erts-17.1`)
//!
//! Verified (and re-asserted below):
//!
//! ```text
//! init:stop(0)                                     -> exit 0
//! init:stop(3)                                     -> exit 3
//! init:stop(normal) / -1 / {x} / [1,2,300]         -> exit 1   (erlang:error(badarg), catchable)
//! init:stop("hello") / init:stop([])               -> exit 1   (the crash-dump slogan form)
//! io:format("a~n"), init:stop(), io:format("b~n")  -> stdout "a\nb", exit 0
//! ```
//!
//! `init:stop/0,1` does **not** terminate the caller: `init.erl:537,570` sends the request to
//! the `init` process and returns `ok`, so the expression that made the call keeps running
//! and the *node* halts afterwards with the requested status.
//!
//! `init:get_arguments/0` begins with the emulator's own automatic flags, in `init.erl`'s
//! order (`root`, `bindir`, `progname`, `home`), each `{Flag, [[Value]]}` (one occurrence,
//! one string), before the command-line flags. A value the runtime has none of omits the
//! entry entirely — `env -u HOME erl …` prints the list without `{home,…}`.
//!
//! A missing `erl` is a **loud** skip, never a silent pass (`tests/support/mod.rs`).

// The shared harness is compiled in whole, but this suite uses only its toolchain discovery
// and probe bound (the probes here need each child's exit code, which `support::oracle_eval`
// folds away), so the unused remainder is expected here.
#[allow(dead_code)]
mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------
// The bounded child runner
// ---------------------------------------------------------------------------------------

/// One finished probe: its exit code and its raw stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// A scratch `ERL_CRASH_DUMP` path, unique per probe.
///
/// Several programs below exit **abnormally** — the `badarg` rejects and the crash-dump
/// slogan forms — and a dump would otherwise be written to `erl_crash.dump` in the crate
/// directory. Redirecting it keeps the suite from leaving files in the source tree.
fn scratch_dump() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "beam_rt_native_init.{}.{}.dump",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Runs `program args` with `remove` unset, `/dev/null` as stdin, output captured and
/// bounded by [`support::PROBE_TIMEOUT`] — a probe that blocks is killed and reaped instead
/// of hanging the suite.
///
/// The bounded runner lives here rather than using `support::run_bounded` for two reasons:
/// the probes must have `ERL_CRASH_DUMP` redirected (see [`scratch_dump`]) and the `$HOME`
/// probe must *unset* a variable, and this suite needs each child's **exit code**, which is
/// what the contract below is about.
fn run_env(program: &str, args: &[&str], remove: &[&str]) -> Run {
    let dump = scratch_dump();
    let mut command = Command::new(program);
    command
        .args(args)
        .env("ERL_CRASH_DUMP", &dump)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in remove {
        command.env_remove(name);
    }
    let output = run_bounded(command, program, args);
    let _ = std::fs::remove_file(&dump);
    let output = output.unwrap_or_else(|| panic!("`{program}` {args:?} could not be run"));
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).into_owned(),
        err: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Runs `command`, polling for exit so a probe that blocks is killed and reaped at
/// [`support::PROBE_TIMEOUT`]. `None` when the child could not be spawned, polled or
/// collected, or when it was killed at the bound.
fn run_bounded(mut command: Command, program: &str, args: &[&str]) -> Option<Output> {
    let mut child: Child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("warning: cannot spawn `{program}`: {error}");
            return None;
        }
    };
    let deadline = Instant::now() + support::PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    eprintln!("warning: `{program}` {args:?} outlived the probe bound");
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

/// The built runtime, running `expr` as its `-eval` program.
fn beam_rt(expr: &str) -> Run {
    beam_rt_with(expr, &[])
}

/// As [`beam_rt`], with `remove` unset in the child's environment.
fn beam_rt_with(expr: &str, remove: &[&str]) -> Run {
    run_env(
        env!("CARGO_BIN_EXE_beam_rt"),
        &["-noshell", "-eval", expr],
        remove,
    )
}

/// The same program under the oracle, terminated by `-s init stop` (an `erl -noshell` never
/// exits on its own).
fn oracle_eval(oracle: &Path, expr: &str) -> Run {
    oracle_eval_with(oracle, expr, &[])
}

/// As [`oracle_eval`], with `remove` unset in the child's environment.
fn oracle_eval_with(oracle: &Path, expr: &str, remove: &[&str]) -> Run {
    let program = oracle
        .to_str()
        .unwrap_or_else(|| panic!("the oracle path is not UTF-8: {}", oracle.display()));
    run_env(
        program,
        &["-noshell", "-eval", expr, "-s", "init", "stop"],
        remove,
    )
}

/// Both runtimes, given the **same** `expr` — the oracle comparison every test below makes.
///
/// Returns `None` (after a loud skip) when no system `erl` is available.
fn both(expr: &str) -> Option<(Run, Run)> {
    let oracle = support::require_oracle("native_init")?;
    Some((beam_rt(expr), oracle_eval(&oracle, expr)))
}

/// The atom names of an Erlang `[a,b,c]` printing (what `io:format("~p~n", [[a,b,c]])`
/// produces), so the two runtimes' flag lists can be compared without a term parser.
fn atoms_of(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or_else(|| panic!("not an Erlang list: {text:?}"));
    inner
        .split(',')
        .map(str::trim)
        .filter(|atom| !atom.is_empty())
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------------------
// init:stop/0,1
// ---------------------------------------------------------------------------------------

/// `init:stop(Status)` asks the **node** to halt with that status while the caller keeps
/// running, so the program's exit code carries the status (`init:stop(0)` ⇒ 0,
/// `init:stop(3)` ⇒ 3). The oracle is the ground truth for each program.
#[test]
fn init_stop_records_the_exit_status_of_the_node() {
    for (expr, status) in [("init:stop(0)", 0), ("init:stop(3)", 3), ("init:stop()", 0)] {
        let Some((ours, theirs)) = both(expr) else {
            return;
        };
        assert_eq!(theirs.code, status, "oracle `{expr}`: {:?}", theirs.err);
        assert_eq!(ours.code, status, "beam_rt `{expr}`: {}", ours.err);
        assert!(
            ours.out.is_empty(),
            "`{expr}` prints nothing: {:?}",
            ours.out
        );
    }
}

/// `init:stop()` does not interrupt the running expression: the statement after it still
/// runs (`init.erl:537` returns `ok` at once), and the node exits 0 once the program is
/// done.
#[test]
fn init_stop_does_not_interrupt_the_running_expression() {
    let expr = "io:format(\"a~n\"), init:stop(), io:format(\"b~n\")";
    let Some((ours, theirs)) = both(expr) else {
        return;
    };
    assert_eq!(theirs.out.trim(), "a\nb", "oracle: {:?}", theirs.err);
    assert_eq!(ours.out.trim(), "a\nb", "beam_rt: {}", ours.err);
    assert_eq!(theirs.code, 0);
    assert_eq!(ours.code, 0);
}

/// Every argument that is neither a non-negative integer nor a byte list is
/// `erlang:error(badarg)` **raised in the caller** — so it is catchable, and an uncaught one
/// exits 1. The atom `normal` is not a "stop normally" spelling.
#[test]
fn init_stop_rejects_a_non_status_argument_with_badarg() {
    for expr in [
        "init:stop(normal)",
        "init:stop(-1)",
        "init:stop({x})",
        "init:stop([1,2,300])",
    ] {
        let Some((ours, theirs)) = both(expr) else {
            return;
        };
        assert_eq!(theirs.code, 1, "oracle `{expr}`: {:?}", theirs.err);
        assert_eq!(ours.code, 1, "beam_rt `{expr}`: {}", ours.err);
        assert!(
            ours.err.contains("badarg"),
            "`{expr}` reports the badarg raise: {}",
            ours.err
        );
    }

    // The raise is the caller's, not the node's: a `try` catches it and the program goes on
    // to exit 0 — under both runtimes.
    let expr = "io:format(\"~p~n\", [try init:stop(normal) catch error:badarg -> caught end])";
    let Some((ours, theirs)) = both(expr) else {
        return;
    };
    assert_eq!(theirs.out.trim(), "caught", "oracle: {:?}", theirs.err);
    assert_eq!(ours.out.trim(), "caught", "beam_rt: {}", ours.err);
    assert_eq!(theirs.code, 0);
    assert_eq!(ours.code, 0);
}

/// A **byte list** (including `[]`) takes the crash-dump slogan form —
/// `erlang:halt(Slogan)` — which prints the slogan, writes a dump and exits 1.
#[test]
fn init_stop_of_a_byte_list_is_the_crash_dump_slogan() {
    for expr in ["init:stop(\"hello\")", "init:stop([])"] {
        let Some((ours, theirs)) = both(expr) else {
            return;
        };
        assert_eq!(theirs.code, 1, "oracle `{expr}`: {:?}", theirs.err);
        assert_eq!(ours.code, 1, "beam_rt `{expr}`: {}", ours.err);
    }
}

// ---------------------------------------------------------------------------------------
// init:get_arguments/0 — the automatic prefix
// ---------------------------------------------------------------------------------------

/// The emulator's automatic flags, in `init.erl`'s order (`init.erl:363-388`).
const CANONICAL: [&str; 4] = ["root", "bindir", "progname", "home"];

/// The flag names of `init:get_arguments()`, in order — the program both runtimes get.
const FLAG_NAMES_EXPR: &str =
    "io:format(\"~p~n\", [[element(1, P) || P <- init:get_arguments()]]), halt(0).";

/// The leading run of automatic flags in a flag-name list.
fn automatic_prefix(flags: &[String]) -> Vec<String> {
    flags
        .iter()
        .take_while(|flag| CANONICAL.contains(&flag.as_str()))
        .cloned()
        .collect()
}

/// `init:get_arguments/0` leads with the emulator's four automatic flags, in `init.erl`'s
/// order, and only then lists the command line.
///
/// The comparison is made on the **flag names** (the oracle's installation paths must not be
/// baked into a golden): the automatic prefix must be the same sequence under both runtimes,
/// it must be a prefix of `root, bindir, progname, home`, and the command-line flags must
/// follow it.
#[test]
fn init_get_arguments_prepends_the_automatic_flags() {
    let Some((ours, theirs)) = both(FLAG_NAMES_EXPR) else {
        return;
    };
    assert_eq!(theirs.code, 0, "oracle: {:?}", theirs.err);
    assert_eq!(ours.code, 0, "beam_rt: {}", ours.err);

    let our_flags = atoms_of(&ours.out);
    let oracle_flags = atoms_of(&theirs.out);
    let ours_prefix = automatic_prefix(&our_flags);
    let oracle_prefix = automatic_prefix(&oracle_flags);

    // Same automatic prefix under both runtimes (an unset `$HOME` omits `home` for both),
    // it begins with `root`, and it follows the canonical order.
    assert_eq!(
        ours_prefix, oracle_prefix,
        "the automatic prefix must match the oracle's: {our_flags:?} vs {oracle_flags:?}"
    );
    assert!(
        !ours_prefix.is_empty() && ours_prefix[0] == "root",
        "the automatic flags lead the list: {our_flags:?}"
    );
    assert_eq!(
        ours_prefix,
        CANONICAL[..ours_prefix.len()].to_vec(),
        "the automatic flags follow init.erl's order: {our_flags:?}"
    );
    // … and the command-line flags follow them, exactly as before this change.
    assert_eq!(
        our_flags[ours_prefix.len()..].to_vec(),
        vec!["noshell".to_string()],
        "the command line follows the automatic flags: {our_flags:?}"
    );
    assert_eq!(
        oracle_flags[oracle_prefix.len()..].to_vec(),
        vec!["noshell".to_string()],
        "the oracle lists the command line after them: {oracle_flags:?}"
    );

    // `root` is the installation root, i.e. the same value `code:root_dir/0` answers (the
    // entry holds one occurrence of one string, hence `{ok, [[R]]}`).
    let expr = "io:format(\"~p~n\", [case init:get_argument(root) of \
                {ok, [[R]]} -> R =:= code:root_dir(); _ -> false end]), halt(0).";
    let Some((ours, theirs)) = both(expr) else {
        return;
    };
    assert_eq!(theirs.out.trim(), "true", "oracle: {:?}", theirs.err);
    assert_eq!(ours.out.trim(), "true", "beam_rt: {}", ours.err);
}

/// An automatic flag this runtime has **no value for** is omitted from the list entirely —
/// never reported with a fabricated value. With `$HOME` unset, both runtimes drop the
/// `{home,…}` entry (the oracle does: `env -u HOME erl …` prints the list without it).
#[test]
fn init_get_arguments_omits_an_unavailable_automatic_flag() {
    let Some(oracle) = support::require_oracle("native_init") else {
        return;
    };
    let ours = beam_rt_with(FLAG_NAMES_EXPR, &["HOME"]);
    let theirs = oracle_eval_with(&oracle, FLAG_NAMES_EXPR, &["HOME"]);
    assert_eq!(theirs.code, 0, "oracle: {:?}", theirs.err);
    assert_eq!(ours.code, 0, "beam_rt: {}", ours.err);

    let our_flags = atoms_of(&ours.out);
    let oracle_flags = atoms_of(&theirs.out);
    assert!(
        !oracle_flags.contains(&"home".to_string()),
        "the oracle omits `home` without `$HOME`: {oracle_flags:?}"
    );
    assert_eq!(
        our_flags, oracle_flags,
        "an unavailable value omits the entry, exactly as the oracle does"
    );
    assert!(our_flags.contains(&"noshell".to_string()), "{our_flags:?}");
}
