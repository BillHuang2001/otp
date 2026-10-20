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

//! The **plain-argument** surface (contract A.6) through the built executable.
//!
//! `init:get_plain_arguments/0` is the argument list the real OTP `erlc` front end depends
//! on: it invokes the emulator as `… -s erl_compile compile_cmdline -extra +deterministic -o
//! <D> <src>` and `erl_compile:compile_cmdline/0` (`lib/stdlib/src/erl_compile.erl:53-56`)
//! reads the file list straight out of that call. The C `init` derives the list from its own
//! boot-argument parse (`erts/preloaded/src/init.erl:1669-1710`): a stray **positional**
//! token is an `arg` and `-extra` is `start_extra_arg` — everything from there on, verbatim
//! and in order — so the answered list is `lists:reverse(As, Bs)`.
//!
//! Each case runs the **same argv** against the built `beam_rt` and against the system `erl`
//! (the oracle cross-check in [`the_oracle_agrees_on_the_plain_argument_list`] — it skips
//! loudly when `erl` is unavailable, the idiom `tests/cli_flags.rs`/`tests/cli_env.rs` use),
//! and every `beam_rt`-only assertion still runs without the toolchain.
use std::process::{Command, Output};

/// The probe: print `init:get_plain_arguments/0` and halt. The same expression both runtimes
/// accept, so one helper drives each.
const PROBE: &str = "io:format(\"~p~n\",[init:get_plain_arguments()]),halt(0).";

/// Runs the built `beam_rt` binary with `args`.
///
/// The `ERL_*FLAGS` merge chain is stripped first, so a case is exactly what the test sets
/// (`beam_rt` folds those variables into its command line through
/// [`beam_rt::cli::parse_with_env`], exactly as the `erl` launcher does).
fn beam_rt(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_beam_rt"));
    command.args(args);
    strip_erl_flags(&mut command);
    command
        .output()
        .unwrap_or_else(|error| panic!("spawning beam_rt {args:?} failed: {error}"))
}

/// Runs the oracle `erl` with the identical `args` (under the same environment).
fn erl(args: &[&str]) -> Output {
    let mut command = Command::new("erl");
    command.args(args);
    strip_erl_flags(&mut command);
    command
        .output()
        .unwrap_or_else(|error| panic!("spawning `erl` {args:?} failed: {error}"))
}

/// Removes every `ERL_*` variable that either runtime folds into its command line.
fn strip_erl_flags(command: &mut Command) {
    for name in [
        "ERL_OTP30_FLAGS",
        "ERL_AFLAGS",
        "ERL_FLAGS",
        "ERL_ZFLAGS",
        "ERL_MAX_PORTS",
        "ERL_INETRC",
        "ERL_CRASH_DUMP",
        "ERL_CRASH_DUMP_SECONDS",
        "ERL_ROOTDIR",
        "ERL_MAX_ETS_TABLES",
        "ERL_FULLSWEEP_AFTER",
    ] {
        command.env_remove(name);
    }
}

/// The stdout of a finished run, as text.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The stderr of a finished run, as text.
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Whether a real `erl` can be executed at all (no toolchain ⇒ skip loudly).
fn erl_available() -> bool {
    Command::new("erl")
        .args(["-noshell", "-eval", "halt(0)."])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// The recorded divergence (`tools/differential/divergences.md`, A.6): the oracle answers
/// `["a","b"]` where this runtime answered `[]`. `-extra` collects **every** remaining token,
/// verbatim and in order, and is itself no init argument.
#[test]
fn extra_arguments_are_the_plain_arguments() {
    let output = beam_rt(&["-noshell", "-eval", PROBE, "-extra", "a", "b"]);
    assert!(
        output.status.success(),
        "exit {:?}, stderr: {}",
        output.status,
        stderr(&output)
    );
    assert_eq!(stdout(&output), "[\"a\",\"b\"]\n");
    assert!(output.stderr.is_empty(), "stderr: {}", stderr(&output));
}

/// The tail the real `erlc` front end appends: `-extra` then its own compile arguments. The
/// flags-like tokens (`-o`) are plain arguments, never switches.
#[test]
fn the_erlc_front_end_tail_is_the_plain_argument_list() {
    let output = beam_rt(&[
        "-noshell",
        "-eval",
        PROBE,
        "-extra",
        "+deterministic",
        "-o",
        "/tmp/x",
        "/tmp/s.erl",
    ]);
    assert!(
        output.status.success(),
        "exit {:?}, stderr: {}",
        output.status,
        stderr(&output)
    );
    assert_eq!(
        stdout(&output),
        "[\"+deterministic\",\"-o\",\"/tmp/x\",\"/tmp/s.erl\"]\n"
    );
}

/// Nothing after `-extra` is a switch or a Start instruction any more: the trailing `-eval`
/// never runs (the first one halts with status `0`, not with the second one's `5`).
#[test]
fn nothing_after_extra_is_a_switch_any_more() {
    let output = beam_rt(&["-noshell", "-eval", PROBE, "-extra", "-eval", "halt(5)."]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "[\"-eval\",\"halt(5).\"]\n",
        "stderr: {}",
        stderr(&output)
    );
}

/// A stray positional token is the C `init`'s `arg`: it lands in the plain-argument list, in
/// command-line order, ahead of the `-extra` tail.
#[test]
fn a_stray_positional_argument_is_a_plain_argument() {
    let output = beam_rt(&["stray", "-noshell", "-eval", PROBE, "-extra", "a", "b"]);
    assert!(
        output.status.success(),
        "exit {:?}, stderr: {}",
        output.status,
        stderr(&output)
    );
    assert_eq!(stdout(&output), "[\"stray\",\"a\",\"b\"]\n");
}

/// A command line with neither a stray positional nor `-extra` answers the empty list — what
/// the oracle answers for the argv of every `-eval`/`-s`/`-run` invocation.
#[test]
fn a_command_line_without_a_plain_argument_answers_the_empty_list() {
    let output = beam_rt(&["-noshell", "-eval", PROBE]);
    assert!(
        output.status.success(),
        "exit {:?}, stderr: {}",
        output.status,
        stderr(&output)
    );
    assert_eq!(stdout(&output), "[]\n");
}

/// The oracle cross-check: the **same** argv through the built `beam_rt` and the system `erl`
/// must agree on stdout and exit status, per case. Skips loudly without a runnable `erl`.
#[test]
fn the_oracle_agrees_on_the_plain_argument_list() {
    if !erl_available() {
        println!("SKIP cli_plain_args: `erl` is not runnable — the oracle cross-check is skipped");
        return;
    }
    // The observed ground truth (OTP 29.1.1 / erts-17.1), recorded per case; the oracle is
    // still asked, so a corpus that drifts is caught rather than trusted.
    for (argv, expected) in [
        (
            &["-noshell", "-eval", PROBE, "-extra", "a", "b"][..],
            "[\"a\",\"b\"]\n",
        ),
        (
            &[
                "-noshell",
                "-eval",
                PROBE,
                "-extra",
                "+deterministic",
                "-o",
                "/tmp/x",
                "/tmp/s.erl",
            ][..],
            "[\"+deterministic\",\"-o\",\"/tmp/x\",\"/tmp/s.erl\"]\n",
        ),
        (
            &["stray", "-noshell", "-eval", PROBE, "-extra", "a", "b"][..],
            "[\"stray\",\"a\",\"b\"]\n",
        ),
        (&["-noshell", "-eval", PROBE][..], "[]\n"),
        (
            &["-noshell", "-eval", PROBE, "-extra", "-eval", "halt(5)."][..],
            "[\"-eval\",\"halt(5).\"]\n",
        ),
    ] {
        let rust = beam_rt(argv);
        let oracle = erl(argv);
        assert_eq!(
            stdout(&oracle),
            expected,
            "the oracle's own answer drifted for {argv:?}: {}",
            stderr(&oracle)
        );
        assert_eq!(
            stdout(&rust),
            stdout(&oracle),
            "plain-argument divergence for {argv:?} (beam_rt stderr: {})",
            stderr(&rust)
        );
        assert_eq!(
            rust.status.code(),
            oracle.status.code(),
            "exit-status divergence for {argv:?} (beam_rt stderr: {})",
            stderr(&rust)
        );
    }
}

/// `-extra` is not an init argument: `init:get_argument(extra)` is the bare atom `error` in
/// both runtimes, while the tokens after it are the plain arguments.
#[test]
fn extra_is_not_an_init_argument() {
    const ARGUMENT_PROBE: &str = "io:format(\"~p~n\",[init:get_argument(extra)]),halt(0).";
    let argv = ["-noshell", "-eval", ARGUMENT_PROBE, "-extra", "a", "b"];
    let rust = beam_rt(&argv);
    assert!(
        rust.status.success(),
        "exit {:?}, stderr: {}",
        rust.status,
        stderr(&rust)
    );
    assert_eq!(stdout(&rust), "error\n");

    if !erl_available() {
        println!("SKIP cli_plain_args: `erl` is not runnable — the oracle cross-check is skipped");
        return;
    }
    let oracle = erl(&argv);
    assert_eq!(stdout(&oracle), "error\n", "{}", stderr(&oracle));
}
