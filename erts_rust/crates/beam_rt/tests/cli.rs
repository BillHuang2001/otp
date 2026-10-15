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

//! The **executable** end-to-end: spawn `beam_rt` and run the vertical slice.
//!
//! `tests/slice.rs` proves the library path; this test proves the *binary* — the CLI
//! parser, the driver, the exit codes and the stdout/stderr of a real process. The
//! `.beam` fixture is passed as an absolute path resolved from `CARGO_MANIFEST_DIR`
//! (the test process's working directory is the crate directory, so a relative path
//! would not resolve), and it is **reused** from `beam_loader`, never copied.
//!
//! The exit-code contract under test (contract A.6, see `cli.rs`):
//! `0` success, `1` a genuine runtime error, `2` a usage error (with `Usage:`) and
//! `3` an unsupported capability gap (one `beam_rt: unsupported: …` line, no usage text).
//! `-eval`/`-s`/`-run`/`run` must be **silent on stderr** on success.
//!
//! # Why a stdout-asserting invocation passes `-noshell`
//!
//! The ERTS-native actions (`-eval`, `-s`, `-run` and a bare shell) drive the default boot
//! script, so a booted kernel prints the OTP banner (`Erlang/OTP 30 [erts-17.1] [source]
//! [64-bit]`) to **stdout** on any such invocation **without** `-noshell` — byte-faithful to
//! the oracle, which also prints the banner without `-noshell` and is silent with it
//! (`erl -noshell -eval 'io:format("x~n"), halt(0).'` → `x\n`; the same without `-noshell`
//! prepends the banner). The `run File Mod:Func/Arity` action is **banner-free by
//! construction** (it records the `noshell` init argument), so a `run`-action assertion needs
//! no `-noshell`. A test that asserts an **exact** stdout therefore invokes the binary with
//! `-noshell` on the banner-printing routes, exactly as the oracle side of every cross-check
//! does.
use std::path::Path;
use std::process::{Command, Output};

/// The committed structured-raise fixture: `badkey/0` = `maps:get(nokey, #{})`.
const RAISE_STRUCTURED_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/raise_structured.beam"
);

/// The `beam_loader` oracle fixture (reused, never copied).
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

/// Runs the built `beam_rt` binary with `args`.
fn beam_rt(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("spawning beam_rt {args:?} failed: {error}"))
}

/// The stdout of a finished run, as text.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The stderr of a finished run, as text.
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn run_add_prints_five() {
    let output = beam_rt(&["-noshell", "run", SLICE_BEAM, "slice:add/2", "2", "3"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "5\n");
    assert!(output.stderr.is_empty(), "stderr: {}", stderr(&output));
}

#[test]
fn run_fib_prints_fifty_five() {
    let output = beam_rt(&["-noshell", "run", SLICE_BEAM, "slice:fib/1", "10"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "55\n");
}

#[test]
fn run_accepts_a_negative_integer_argument() {
    let output = beam_rt(&["-noshell", "run", SLICE_BEAM, "slice:add/2", "2", "-3"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "-1\n");
}

#[test]
fn run_accepts_atom_and_list_arguments() {
    // `slice:pick(a)` is 1 and the catch-all clause is 2 — the atom grammar is the
    // only way to reach either.
    let output = beam_rt(&["-noshell", "run", SLICE_BEAM, "slice:pick/1", "a"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "1\n");

    let output = beam_rt(&["-noshell", "run", SLICE_BEAM, "slice:pick/1", "[a]"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "2\n");
}

#[test]
fn version_prints_the_banner() {
    for flag in ["--version", "-version"] {
        let output = beam_rt(&[flag]);
        assert!(output.status.success(), "stderr: {}", stderr(&output));
        let banner = stdout(&output);
        assert!(banner.contains("30.0-rc0"), "{banner}");
        assert!(banner.contains("erts-17.1"), "{banner}");
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn eval_is_silent() {
    // Real `erl -eval '2+3'` prints nothing and exits 0. (`-noshell` keeps the booted
    // kernel's OTP banner off stdout, exactly as the oracle does.)
    let output = beam_rt(&["-noshell", "-eval", "2+3"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(output.stdout.is_empty(), "stdout: {}", stdout(&output));
    assert!(output.stderr.is_empty(), "stderr: {}", stderr(&output));

    // A trailing `.` (the shell-style terminator) is tolerated.
    let output = beam_rt(&["-noshell", "-eval", "(1+2)*3."]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(output.stdout.is_empty(), "stdout: {}", stdout(&output));
    assert!(output.stderr.is_empty(), "stderr: {}", stderr(&output));
}

#[test]
fn an_unknown_switch_is_a_recorded_init_argument() {
    // Real `erl` hands unmodeled switches to `init:get_argument/1`, so this is *not* a
    // capability gap: the runtime boots and exits 0.
    let output = beam_rt(&["-noshell", "--nope", "hello", "--nope2"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(output.stdout.is_empty(), "stdout: {}", stdout(&output));
    assert!(output.stderr.is_empty(), "stderr: {}", stderr(&output));
}

#[test]
fn a_bad_eval_expression_is_unsupported() {
    // A genuinely unparseable `-eval` expression is a capability gap (exit 3), not a
    // genuine runtime error (exit 1) and not a usage error. (`length([a,b,c])`,
    // `2/3`, `try(1)end.` now *parse and evaluate*, so they are no longer gaps.)
    for expr in ["1+", "(1", "foo("] {
        let output = beam_rt(&["-eval", expr]);
        assert_eq!(
            output.status.code(),
            Some(3),
            "{expr}: stderr {}",
            stderr(&output)
        );
        let message = stderr(&output);
        assert_eq!(
            message.lines().count(),
            1,
            "{expr}: exactly one stderr line: {message}"
        );
        assert!(
            message.starts_with("beam_rt: unsupported: "),
            "{expr}: {message}"
        );
        assert!(!message.contains("Usage:"), "{expr}: {message}");
    }
}

#[test]
fn a_missing_switch_operand_is_a_usage_error() {
    for args in [&["-eval"][..], &["-pa"][..], &["-name"][..]] {
        let output = beam_rt(args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: stderr {}",
            stderr(&output)
        );
        assert!(stderr(&output).contains("Usage:"), "{args:?}");
    }
}

#[test]
fn a_stray_positional_argument_is_a_usage_error() {
    let output = beam_rt(&["junk"]);
    assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));
    assert!(stderr(&output).contains("Usage:"), "{}", stderr(&output));
}

#[test]
fn a_malformed_run_spec_is_a_usage_error() {
    for args in [
        &["run", SLICE_BEAM, "slice-add-2"][..],
        &["run", SLICE_BEAM, "slice:add/2", "2"][..],
        &["run", SLICE_BEAM, "slice:add/2", "2", "-pa"][..],
    ] {
        let output = beam_rt(args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: stderr {}",
            stderr(&output)
        );
        assert!(stderr(&output).contains("Usage:"), "{args:?}");
    }
}

#[test]
fn a_missing_file_fails() {
    let output = beam_rt(&["run", "/nonexistent/nope.beam", "nope:f/0"]);
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(!stderr(&output).is_empty());
    assert!(!stderr(&output).contains("Usage:"));
}

#[test]
fn the_s_action_applies_with_atom_arguments_and_discards_the_result() {
    let fixtures = Path::new(SLICE_BEAM)
        .parent()
        .expect("the fixture has a parent directory")
        .to_str()
        .expect("the path is UTF-8")
        .to_string();
    let output = beam_rt(&["-noshell", "-pa", &fixtures, "-s", "slice", "pick", "a"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).is_empty(), "the result is discarded");
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
}

#[test]
fn the_code_path_is_searched_for_the_module() {
    // Without `-pa`, the module cannot be found: the oracle's `error:undef`, a runtime
    // error (exit 1), not a panic and not a usage error.
    let output = beam_rt(&["-s", "slice", "pick", "a"]);
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    let message = stderr(&output);
    assert!(message.contains("undef"), "{message}");
    assert!(!message.contains("Usage:"), "{message}");
}

/// `run` renders the **real** reason term of an uncaught structured raise.
///
/// `raise_structured:badkey/0` is `maps:get(nokey, #{})`, so the oracle's uncaught
/// reason is `{badkey,nokey}`: the diagnostic must print that term, not the
/// `Display` placeholder `<structured reason>` a term-free `beam_common::Raise` renders.
#[test]
fn run_prints_the_real_reason_of_a_structured_raise() {
    let output = beam_rt(&[
        "-noshell",
        "run",
        RAISE_STRUCTURED_BEAM,
        "raise_structured:badkey/0",
    ]);
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(stdout(&output).is_empty(), "stdout: {}", stdout(&output));
    let message = stderr(&output);
    assert_eq!(message, "beam_rt: error {badkey,nokey}\n", "{message}");
    assert!(!message.contains("<structured reason>"), "{message}");
    // A genuine runtime error, never a capability gap (exit 3) and no usage text.
    assert!(!message.contains("unsupported"), "{message}");
    assert!(!message.contains("Usage:"), "{message}");
}

/// An **atom**-reason raise is unchanged: the reason is the bare atom.
///
/// `raise_structured:nope/0` does not exist, so the raise is the oracle's `error:undef`
/// (reason atom `undef`, the same bare atom the run path raises for a missing export):
/// the diagnostic stays `error undef` — never a message and never the placeholder.
#[test]
fn run_keeps_an_atom_reason_a_bare_atom() {
    let output = beam_rt(&[
        "run",
        RAISE_STRUCTURED_BEAM,
        "raise_structured:nope/0",
    ]);
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    let message = stderr(&output);
    assert_eq!(message, "beam_rt: error undef\n", "{message}");
    assert!(!message.contains("<structured reason>"), "{message}");
}
