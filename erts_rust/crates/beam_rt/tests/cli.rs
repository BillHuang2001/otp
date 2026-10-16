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
    // The CLI's own `-`-switches report their own usage status (2, with the block).
    for args in [&["-eval"][..], &["-pa"][..], &["-pz"][..], &["-config"][..]] {
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
fn a_missing_operand_of_a_launcher_switch_is_a_flag_rejection() {
    // `-name`/`-sname` and every `+`-switch are consumed by the `erl` launcher/emulator,
    // whose `NEXT_ARG_CHECK` diagnostic is `Missing argument(s) for '<sw>'.` and exit
    // **1**, with no `beam_rt` usage block.
    for switch in [
        "-name", "-sname", "+K", "+Q", "+A", "+P", "+S", "+t", "+e", "+hms", "+hmbs",
        "+hmax", "+hmaxk", "+JPperf", "+JPperfdirectory",
    ] {
        let output = beam_rt(&[switch]);
        assert_eq!(output.status.code(), Some(1), "{switch}: {}", stderr(&output));
        let message = format!("Missing argument(s) for '{switch}'.");
        assert!(
            stderr(&output).contains(&message),
            "{switch}: {}",
            stderr(&output)
        );
        assert!(!stderr(&output).contains("Usage:"), "{switch}");
    }
}

#[test]
fn a_stray_positional_argument_is_accepted() {
    // The oracle's launcher forwards a plain token to `init`, so a stray positional is
    // **not** a usage error (`erl junk -noshell …` exits 0). This runtime ignores it.
    let output = beam_rt(&["junk"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
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

// --- `-env` and the emulator-switch exit-code family -----------------------------------------

/// Whether the oracle `erl` is runnable (`erl -version` prints the banner and exits 0).
fn erl_available() -> bool {
    Command::new("erl")
        .arg("-version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Spawns `program` with `args` after stripping every inherited `ERL_*FLAGS` / dump variable
/// and then applying `envs` — the same isolation both the built binary and the oracle need.
fn spawn_env(program: &str, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(program);
    for name in [
        "ERL_AFLAGS",
        "ERL_FLAGS",
        "ERL_ZFLAGS",
        "ERL_CRASH_DUMP",
        "ERL_CRASH_DUMP_SECONDS",
        "ERL_MAX_PORTS",
    ] {
        command.env_remove(name);
    }
    command.env_remove(beam_rt::cli::otp_flags_var());
    command.args(args);
    for (name, value) in envs {
        command.env(name, value);
    }
    command
        .output()
        .unwrap_or_else(|error| panic!("spawning {program} {args:?} failed: {error}"))
}

/// `-env VAR VALUE` is applied to the **child's environment** by the `erl` launcher, before the
/// emulator starts: `os:getenv/1` observes it, a repeated name keeps the last value, the first
/// `=` splits the name (`-env A=B v` sets `A` to `B=v`, as `putenv` does), it is **not** an
/// init argument, and — because it precedes the environment read — `ERL_MAX_PORTS` reaches
/// `system_info(port_limit)`.
///
/// Oracle (OTP 29.1.1 / erts-17.1), the answers this test pins:
///
/// ```text
/// erl -env BEAM_RT_ENV_TEST hello -noshell -eval 'io:format("~s~n",[os:getenv("BEAM_RT_ENV_TEST")]),halt(0).' → hello
/// erl -env K first -env K second  (the same probe, name K)  → second
/// erl -env K=tail val             (the same probe, name K)  → tail=val
/// erl -env FOO bar -noshell -eval 'io:format("~p~n",[init:get_argument(env)]),halt(0).'    → error
/// erl -env ERL_MAX_PORTS 4096 -noshell -eval 'io:format("~p~n",[erlang:system_info(port_limit)]),halt(0).' → 4096
/// ```
#[test]
fn env_switches_become_part_of_the_childs_environment() {
    let cases: &[(&str, &[&str], &str)] = &[
        (
            "a single pair is visible to os:getenv/1",
            &[
                "-env",
                "BEAM_RT_ENV_TEST",
                "hello",
                "-noshell",
                "-eval",
                "io:format(\"~s~n\",[os:getenv(\"BEAM_RT_ENV_TEST\")]),halt(0).",
            ],
            "hello\n",
        ),
        (
            "a repeated name keeps the last value",
            &[
                "-env",
                "BEAM_RT_ENV_TEST",
                "first",
                "-env",
                "BEAM_RT_ENV_TEST",
                "second",
                "-noshell",
                "-eval",
                "io:format(\"~s~n\",[os:getenv(\"BEAM_RT_ENV_TEST\")]),halt(0).",
            ],
            "second\n",
        ),
        (
            "the first `=` splits the name (as putenv does)",
            &[
                "-env",
                "BEAM_RT_ENV_TEST=tail",
                "val",
                "-noshell",
                "-eval",
                "io:format(\"~s~n\",[os:getenv(\"BEAM_RT_ENV_TEST\")]),halt(0).",
            ],
            "tail=val\n",
        ),
        (
            "`-env` is not an init argument",
            &[
                "-env",
                "BEAM_RT_ENV_TEST",
                "hello",
                "-noshell",
                "-eval",
                "io:format(\"~p~n\",[init:get_argument(env)]),halt(0).",
            ],
            "error\n",
        ),
        (
            "an applied ERL_MAX_PORTS reaches system_info(port_limit)",
            &[
                "-env",
                "ERL_MAX_PORTS",
                "4096",
                "-noshell",
                "-eval",
                "io:format(\"~p~n\",[erlang:system_info(port_limit)]),halt(0).",
            ],
            "4096\n",
        ),
    ];
    let oracle_available = erl_available();
    if !oracle_available {
        println!("SKIP cli: `erl` is not runnable — only the beam_rt half is pinned");
    }
    let mut failures: Vec<String> = Vec::new();
    for (description, args, expected) in cases {
        let rust = spawn_env(env!("CARGO_BIN_EXE_beam_rt"), args, &[]);
        if rust.status.code() != Some(0) {
            failures.push(format!(
                "{description}: beam_rt exited {:?} (stderr {})",
                rust.status.code(),
                stderr(&rust)
            ));
        }
        if stdout(&rust) != *expected {
            failures.push(format!(
                "{description}: beam_rt printed {:?} != {expected:?}",
                stdout(&rust)
            ));
        }
        if oracle_available {
            let oracle = spawn_env("erl", args, &[]);
            if oracle.status.code() != Some(0) || stdout(&oracle) != *expected {
                failures.push(format!(
                    "{description}: the oracle exited {:?} printing {:?}",
                    oracle.status.code(),
                    stdout(&oracle)
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A trailing `-env` (nothing at all, or a name with no value) is the **launcher**'s
/// rejection: exit 1 with the one-line `beam_rt: Missing argument(s) for '-env'.` and **no**
/// usage block. (The oracle's `erl` launcher prints its own short usage text after the same
/// first line; this runtime reproduces only the diagnostic, like the `beam.smp` usage block.)
#[test]
fn a_trailing_env_switch_is_the_launchers_diagnostic() {
    for args in [
        &["-env"][..],
        &["-env", "NAME"][..],
        &["-noshell", "-env", "NAME"][..],
    ] {
        let output = spawn_env(env!("CARGO_BIN_EXE_beam_rt"), args, &[]);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}: stderr {}",
            stderr(&output)
        );
        assert_eq!(
            stderr(&output),
            "beam_rt: Missing argument(s) for '-env'.\n",
            "{args:?}"
        );
        assert!(!stderr(&output).contains("Usage:"), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}

/// A rejected **emulator-switch value** exits **1** like the oracle's `beam.smp`: exactly one
/// `beam_rt: bad value "…" for switch +X` line on stderr, no usage block, no stdout. This is
/// the emulator's own rejection, never the CLI's usage status 2.
///
/// Oracle (OTP 29.1.1 / erts-17.1) rejects every one of these with exit 1 (its own `bad …`
/// diagnostic plus its usage text).
#[test]
fn a_rejected_emulator_switch_value_exits_one_with_no_usage() {
    let rejected: &[&[&str]] = &[
        &["+t", "2"],
        &["+t", "1000"],
        &["+t", "abc"],
        &["+P", "0"],
        &["+Q", "0"],
        &["+hms", "0"],
        &["+sbt", "zz"],
        &["+e", "-1"],
        &["+hmax", "-1"],
        &["+hmaxk", "x"],
        &["+S", "abc"],
        &["+S", "4:5"],
    ];
    for args in rejected {
        let mut full: Vec<&str> = args.to_vec();
        full.extend(["-noshell", "-eval", "halt(0)."]);
        let output = spawn_env(env!("CARGO_BIN_EXE_beam_rt"), &full, &[]);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}: stderr {}",
            stderr(&output)
        );
        let message = stderr(&output);
        assert_eq!(
            message.lines().count(),
            1,
            "{args:?}: exactly one stderr line: {message}"
        );
        assert!(
            message.starts_with("beam_rt: bad value "),
            "{args:?}: {message}"
        );
        assert!(
            message.contains(&format!("for switch {}", args[0])),
            "{args:?}: {message}"
        );
        assert!(!message.contains("Usage:"), "{args:?}: {message}");
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}

/// A value the emulator's lax readers accept **runs the action** (exit 0, empty stderr, the
/// action's own output): the `atoi`-based switches ignore trailing junk (`+hms 100abc` is
/// 100, `+hmax abc`/`+A abc`/`+e x` are 0) and `+K`/`+B` never validate their value.
///
/// Oracle (OTP 29.1.1 / erts-17.1): every one of these exits 0.
#[test]
fn an_ignored_emulator_switch_value_still_runs_the_action() {
    let accepted: &[&[&str]] = &[
        &["+hmax", "x"],
        &["+hmax", "abc"],
        &["+hms", "100abc"],
        &["+A", "abc"],
        &["+K", "x"],
        &["+K", "bogus"],
        &["+B", "x"],
        &["+B", "foo"],
        &["+e", "x"],
    ];
    for args in accepted {
        let mut full: Vec<&str> = args.to_vec();
        full.extend(["-noshell", "-eval", "io:format(\"ran~n\",[]),halt(0)."]);
        let output = spawn_env(env!("CARGO_BIN_EXE_beam_rt"), &full, &[]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{args:?}: stderr {}",
            stderr(&output)
        );
        assert_eq!(stdout(&output), "ran\n", "{args:?}");
        assert!(output.stderr.is_empty(), "{args:?}: {}", stderr(&output));
    }
}

/// A `-config` the boot cannot read reproduces the oracle's **stderr byte-for-byte** (`\r\n`
/// endings included) and exits 1, and really writes the crash dump named by `ERL_CRASH_DUMP`:
///
/// ```text
/// Could not start kernel pid (application_controller) ("error in config file \"<p>.config\" (none): configuration file not found")\r\n\r\nCrash dump is being written to: <dump>...done\r\n
/// ```
///
/// The tree is hermetic: `ERL_CRASH_DUMP` points at a scratch file (removed on the way out),
/// so neither runtime writes into the crate directory.
#[test]
fn a_missing_config_file_reproduces_the_oracles_stderr() {
    let dir = std::env::temp_dir().join(format!("beam_rt_cli_config_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let absent = dir.join("beam_rt_absent");
    let dump = dir.join("beam_rt_absent.dump");
    let absent = absent.to_str().expect("a UTF-8 temp path").to_string();
    let dump_path = dump.to_str().expect("a UTF-8 temp path").to_string();

    let expected = format!(
        "Could not start kernel pid (application_controller) \
         (\"error in config file \\\"{absent}.config\\\" (none): configuration file not found\")\
         \r\n\r\nCrash dump is being written to: {dump_path}...done\r\n"
    );

    let args = ["-noshell", "-config", absent.as_str(), "-eval", "ok"];
    let envs = [("ERL_CRASH_DUMP", dump_path.as_str())];
    let _ = std::fs::remove_file(&dump);
    let rust = spawn_env(env!("CARGO_BIN_EXE_beam_rt"), &args, &envs);
    assert_eq!(rust.status.code(), Some(1), "stderr {}", stderr(&rust));
    assert!(rust.stdout.is_empty(), "stdout {}", stdout(&rust));
    assert_eq!(stderr(&rust), expected, "beam_rt stderr");
    assert!(
        dump.is_file(),
        "beam_rt must really write the crash dump at {dump_path}"
    );

    if erl_available() {
        let _ = std::fs::remove_file(&dump);
        let oracle = spawn_env("erl", &args, &envs);
        assert_eq!(oracle.status.code(), Some(1), "the oracle exit status");
        assert!(oracle.stdout.is_empty(), "the oracle stdout");
        assert_eq!(stderr(&oracle), expected, "oracle stderr");
    } else {
        println!("SKIP cli: `erl` is not runnable — the oracle cross-check is skipped");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
