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

//! The `ERL_*FLAGS` environment-variable surface **end-to-end**.
//!
//! Real `erl` folds four environment variables into the command line before `init` sees
//! it (`erts/etc/common/erlexec.c`, `initial_argv_massage`):
//!
//! ```text
//! [ERL_OTP<sysver>_FLAGS] [ERL_AFLAGS] [command line] [ERL_FLAGS] [ERL_ZFLAGS]
//! ```
//!
//! The merge and its tokenizer are unit-tested in `src/cli/env.rs`; this suite proves the
//! **built binary** honours them: the child `beam_rt` is spawned with the variables set on
//! its process environment and the flags must become indistinguishable from command-line
//! flags as observed through `init:get_argument/1` (contract A.6).
//!
//! The golden values are the **oracle's own** output, produced with the same probes
//! against the system `erl` (OTP 29.1.1 / erts-17.1):
//!
//! ```text
//! ERL_FLAGS='-setcookie envcookie' erl -noshell -eval '<probe>'
//!   → ok {ok,[[[101,110,118,99,111,111,107,105,101]]]}
//! ERL_AFLAGS='-setcookie acookie' ERL_FLAGS='-setcookie zcookie' erl …
//!   → ok {ok,[[[97,99,111,111,107,105,101]],[[122,99,111,111,107,105,101]]]}
//! ```
//!
//! so they are pinned here and the suite runs without the oracle. One additional test
//! cross-checks the two runtimes directly and **skips loudly** when `erl` is unavailable.
use std::path::PathBuf;
use std::process::{Command, Output};

/// The self-terminating probe: print `init:get_argument(setcookie)` with `~w` and halt.
///
/// This is the differential corpus's `try … catch` wrapper, so an uncaught failure is
/// observable as `exception <class> <reason>` with exit 1 instead of silent stdout.
const PROBE: &str = concat!(
    "try (init:get_argument(setcookie)) of V -> io:format(\"ok ~w~n\", [V]), halt(0) ",
    "catch C:R -> io:format(\"exception ~w ~w~n\", [C, R]), halt(1) end."
);

/// The `ERL_OTP<sysver>_FLAGS` variable name this runtime reads (`ERL_OTP30_FLAGS`).
fn otp_flags_var() -> String {
    beam_rt::cli::otp_flags_var()
}

/// Spawns `program` (either the built `beam_rt` or the oracle `erl`) with the probe,
/// stripping every inherited `ERL_*FLAGS` variable and then applying `envs`.
///
/// Both runtimes share the `-noshell [-flag val…] -eval <probe>` shape and the same
/// argument grammar (`erl`'s `-eval` subset), so one helper drives both. The command-line
/// args (`extra`) sit between `-noshell` and `-eval`, mirroring the `[cmdline]` slot the
/// merge wraps.
fn invoke(program: &str, envs: &[(String, String)], extra: &[&str]) -> Output {
    run_probe(program, envs, extra, PROBE)
}

/// Spawns `program` as `-noshell <extra> -eval <probe>`, stripping every inherited
/// `ERL_*FLAGS` variable and then applying `envs`.
///
/// The same shape drives both the built `beam_rt` and the oracle `erl` (they share the
/// `-noshell … -eval <expr>` command line and the `-eval` expression subset the probes
/// use).
fn run_probe(program: &str, envs: &[(String, String)], extra: &[&str], probe: &str) -> Output {
    let mut command = Command::new(program);
    // Strip any inherited variable so each case is exactly what the test sets.
    for name in ["ERL_AFLAGS", "ERL_FLAGS", "ERL_ZFLAGS"] {
        command.env_remove(name);
    }
    command.env_remove(otp_flags_var());
    command.args(["-noshell"]);
    command.args(extra);
    command.args(["-eval", probe]);
    for (name, value) in envs {
        command.env(name, value);
    }
    command
        .output()
        .unwrap_or_else(|error| panic!("spawning {program} failed: {error}"))
}

/// Whether `program` accepts `-noshell <args> -eval 'halt(0).'` (a clean exit 0).
fn accepts(program: &str, args: &[&str]) -> bool {
    run_probe(program, &[], args, "halt(0).").status.success()
}

/// Spawns the built `beam_rt` with `envs`.
fn beam_rt(envs: &[(&str, &str)]) -> Output {
    let pairs: Vec<(String, String)> = envs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    invoke(env!("CARGO_BIN_EXE_beam_rt"), &pairs, &[])
}

/// The stdout of a finished run, as text.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The stderr of a finished run, as text.
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The oracle's `~w` rendering of `init:get_argument(setcookie)` when the variable holds
/// one occurrence per element of `values`.
///
/// `init:get_argument/1` answers `{ok, [Occurrence]}` where each occurrence is a list of
/// the flag's values, so `-setcookie x` is `{ok,[["x"]]}`. `~w` does **not** use the
/// printable-string heuristic, so each value renders as its Erlang byte list (verified:
/// `erl … -differential_test_flag hello …` prints `ok {ok,[[[104,101,108,108,111]]]}`).
fn expected(values: &[&str]) -> String {
    let occurrences: Vec<String> = values
        .iter()
        .map(|value| {
            let bytes: Vec<String> = value.bytes().map(|byte| byte.to_string()).collect();
            format!("[[{}]]", bytes.join(","))
        })
        .collect();
    format!("ok {{ok,[{}]}}\n", occurrences.join(","))
}

/// Runs `beam_rt` with `envs` and asserts the exact `init:get_argument(setcookie)` output.
fn assert_probe(envs: &[(&str, &str)], values: &[&str]) {
    let output = beam_rt(envs);
    assert_eq!(
        output.status.code(),
        Some(0),
        "env {envs:?}: stderr {}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), expected(values), "env {envs:?}");
    assert!(
        output.stderr.is_empty(),
        "env {envs:?}: {}",
        stderr(&output)
    );
}

#[test]
fn erl_flags_reaches_the_runtime() {
    assert_probe(&[("ERL_FLAGS", "-setcookie envcookie")], &["envcookie"]);
}

#[test]
fn erl_aflags_reaches_the_runtime() {
    assert_probe(&[("ERL_AFLAGS", "-setcookie acookie")], &["acookie"]);
}

#[test]
fn erl_zflags_reaches_the_runtime() {
    assert_probe(&[("ERL_ZFLAGS", "-setcookie zcookie")], &["zcookie"]);
}

#[test]
fn the_otp_flags_variable_reaches_the_runtime() {
    // The OTP-internal, undocumented variable is named for the *system* version.
    let otp = otp_flags_var();
    assert_eq!(otp, "ERL_OTP30_FLAGS");
    assert_probe(&[(otp.as_str(), "-setcookie otpcookie")], &["otpcookie"]);
}

/// With both variables set, the occurrences appear in real `erl`'s merge order:
/// `ERL_AFLAGS` **before** the command line, `ERL_FLAGS` **after** it — so the later
/// occurrence (`ERL_FLAGS`'s `zcookie`) wins.
#[test]
fn erl_flags_wins_over_erl_aflags_in_the_merged_stream() {
    assert_probe(
        &[
            ("ERL_AFLAGS", "-setcookie acookie"),
            ("ERL_FLAGS", "-setcookie zcookie"),
        ],
        &["acookie", "zcookie"],
    );
}

/// The full order: `[ERL_OTP<sysver>_FLAGS] [ERL_AFLAGS] [command line] [ERL_FLAGS]
/// [ERL_ZFLAGS]`.
#[test]
fn the_full_merge_order_is_otp_aflags_command_line_flags_zflags() {
    let otp = otp_flags_var();
    let output = invoke(
        env!("CARGO_BIN_EXE_beam_rt"),
        &[
            (otp, "-setcookie otpcookie".to_string()),
            ("ERL_AFLAGS".to_string(), "-setcookie acookie".to_string()),
            ("ERL_FLAGS".to_string(), "-setcookie fcookie".to_string()),
            ("ERL_ZFLAGS".to_string(), "-setcookie zcookie".to_string()),
        ],
        &["-setcookie", "cmdcookie"],
    );
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(
        stdout(&output),
        expected(&["otpcookie", "acookie", "cmdcookie", "fcookie", "zcookie"])
    );
    assert!(output.stderr.is_empty(), "stderr: {}", stderr(&output));
}

/// A command-line flag and an environment flag are the same kind of occurrence: the
/// command line's comes first (it sits between `ERL_AFLAGS` and `ERL_FLAGS`).
#[test]
fn a_command_line_flag_precedes_erl_flags() {
    let output = invoke(
        env!("CARGO_BIN_EXE_beam_rt"),
        &[("ERL_FLAGS".to_string(), "-setcookie envcookie".to_string())],
        &["-setcookie", "cmdcookie"],
    );
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), expected(&["cmdcookie", "envcookie"]));
    assert!(output.stderr.is_empty(), "stderr: {}", stderr(&output));
}

/// The env string is tokenized exactly like the shell would: whitespace separates, a
/// quoted group is one token, `\` escapes and `#` is an ordinary character.
#[test]
fn an_env_value_is_tokenized_into_argv() {
    // A quoted group with a space is one flag value.
    assert_probe(&[("ERL_FLAGS", "-setcookie \"a b\"")], &["a b"]);
    // `#` is not a comment.
    assert_probe(&[("ERL_FLAGS", "-setcookie x#y")], &["x#y"]);
    // A backslash escapes the following space, keeping the value one token.
    assert_probe(&[("ERL_FLAGS", "-setcookie one\\ two")], &["one two"]);
    // Several switches in one variable all land in argv.
    assert_probe(&[("ERL_FLAGS", "-noshell -setcookie multi")], &["multi"]);
}

/// An unset variable contributes nothing: the flag is absent (`init:get_argument/1` is
/// the bare atom `error`).
#[test]
fn an_unset_variable_contributes_nothing() {
    let output = beam_rt(&[]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "ok error\n");
    assert!(output.stderr.is_empty(), "stderr: {}", stderr(&output));
}

// --- The oracle cross-check (skips loudly without `erl`) ------------------------------

/// Whether a real `erl` can be executed at all (no toolchain ⇒ skip loudly).
fn erl_available() -> bool {
    Command::new("erl")
        .args(["-noshell", "-eval", "halt(0)."])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// The oracle's own `ERL_OTP<sysver>_FLAGS` name — its `erlang:system_info(otp_release)`,
/// which differs from this runtime's for any release mismatch.
fn oracle_otp_flags_var() -> Option<String> {
    let output = Command::new("erl")
        .args([
            "-noshell",
            "-eval",
            "io:format(\"~s\", [erlang:system_info(otp_release)]), halt(0).",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let release = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if release.is_empty() {
        return None;
    }
    Some(format!("ERL_OTP{release}_FLAGS"))
}

/// One case applied identically to both runtimes; `role` is `otp`/`aflags`/`flags`/`zflags`
/// so the runtime-specific OTP variable name can be substituted.
struct Case {
    description: &'static str,
    envs: &'static [(&'static str, &'static str)],
    extra: &'static [&'static str],
}

const CASES: &[Case] = &[
    Case {
        description: "ERL_FLAGS reaches init:get_argument/1",
        envs: &[("flags", "-setcookie envcookie")],
        extra: &[],
    },
    Case {
        description: "ERL_AFLAGS reaches init:get_argument/1",
        envs: &[("aflags", "-setcookie acookie")],
        extra: &[],
    },
    Case {
        description: "ERL_ZFLAGS reaches init:get_argument/1",
        envs: &[("zflags", "-setcookie zcookie")],
        extra: &[],
    },
    Case {
        description: "ERL_AFLAGS before, ERL_FLAGS after the command line",
        envs: &[
            ("aflags", "-setcookie acookie"),
            ("flags", "-setcookie zcookie"),
        ],
        extra: &[],
    },
    Case {
        description: "the full OTP/AFLAGS/cmdline/FLAGS/ZFLAGS order",
        envs: &[
            ("otp", "-setcookie otpcookie"),
            ("aflags", "-setcookie acookie"),
            ("flags", "-setcookie fcookie"),
            ("zflags", "-setcookie zcookie"),
        ],
        extra: &["-setcookie", "cmdcookie"],
    },
    Case {
        description: "a quoted value is one token",
        envs: &[("flags", "-setcookie \"a b\"")],
        extra: &[],
    },
    Case {
        description: "`#` is an ordinary character, not a comment",
        envs: &[("flags", "-setcookie x#y")],
        extra: &[],
    },
    Case {
        description: "a backslash escapes the following character",
        envs: &[("flags", "-setcookie one\\ two")],
        extra: &[],
    },
    Case {
        description: "an unset variable contributes nothing",
        envs: &[],
        extra: &[],
    },
];

/// Resolves a case's role-tagged variables into concrete `(name, value)` pairs.
fn resolve(envs: &[(&str, &str)], otp_var: &str) -> Vec<(String, String)> {
    envs.iter()
        .map(|(role, value)| {
            let name = match *role {
                "otp" => otp_var.to_string(),
                "aflags" => "ERL_AFLAGS".to_string(),
                "flags" => "ERL_FLAGS".to_string(),
                "zflags" => "ERL_ZFLAGS".to_string(),
                other => panic!("unknown environment role {other}"),
            };
            (name, value.to_string())
        })
        .collect()
}

/// The two runtimes produce **identical** `init:get_argument/1` output for every env-flag
/// case. Skips loudly when `erl` is unavailable.
#[test]
fn every_env_flag_case_matches_the_oracle() {
    if !erl_available() {
        println!("SKIP cli_flags: `erl` is not runnable — the oracle cross-check is skipped");
        return;
    }
    let Some(oracle_otp) = oracle_otp_flags_var() else {
        println!("SKIP cli_flags: could not read the oracle's otp_release — cross-check skipped");
        return;
    };
    let beam_otp = otp_flags_var();

    let mut failures: Vec<String> = Vec::new();
    for case in CASES {
        let oracle = invoke("erl", &resolve(case.envs, &oracle_otp), case.extra);
        let rust = invoke(
            env!("CARGO_BIN_EXE_beam_rt"),
            &resolve(case.envs, &beam_otp),
            case.extra,
        );
        let oracle_out = stdout(&oracle);
        let rust_out = stdout(&rust);
        if oracle_out != rust_out {
            failures.push(format!(
                "{}: rust {:?} != oracle {:?}",
                case.description, rust_out, oracle_out
            ));
        }
        if rust.status.code() != Some(0) {
            failures.push(format!(
                "{}: rust exit {:?} (stderr {})",
                case.description,
                rust.status.code(),
                stderr(&rust)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// --- The A.6 emulator switches added with the environment-flag work ----------------------
//
// The new spellings (`-setcookie`/`-config`/`-boot`/`-emu_flavor`, `+Q`/`+P`/`+K`/`+sbt`,
// the `-M*`/`+M*` allocator family) are unit-tested in `src/cli.rs` and
// `src/cli/flags.rs`; this section pins their **external** behaviour: the value
// `init:get_argument/1` answers, and whether the oracle accepts each spelling.
//
// Oracle values (OTP 29.1.1 / erts-17.1), observed with
// `erl -noshell <flag> <value> -eval 'halt(0).'`:
//
// * `-setcookie`/`-config`/`-boot`/`-emu_flavor` are `-`-switches: accepted and recorded
//   (`-emu_flavor` only for `smp`, its one accepted value).
// * `+Q|+P 1024|134217727|legacy` → exit 0; `+Q|+P 1000|0|134217728|abc` → exit 1 (the
//   `bad number of ports|processes …` usage error).
// * `+K <anything>` → exit 0 (the emulator never validates the value).
// * all nine `+sbt` types → exit 0, `+sbt bogus` → exit 1.
// * the validated `+M*` spellings → exit 0, the bad ones → exit 1; `-M*` is never
//   forwarded to the emulator, so it always exits 0.
//
// `beam_rt` keeps its uniform four-exit-code contract, so a value the oracle rejects with
// exit 1 is a **usage** error here (exit 2) — `mismatch=0` at the differential gate. What
// the table below compares is therefore **acceptance** (`exit 0` vs not), with
// `beam_rt`'s own exact code pinned alongside.

/// The self-terminating probe for `init:get_argument(FLAG)`.
fn argument_probe(flag: &str) -> String {
    format!(
        "try (init:get_argument({flag})) of V -> io:format(\"ok ~w~n\", [V]), halt(0) \
         catch C:R -> io:format(\"exception ~w ~w~n\", [C, R]), halt(1) end."
    )
}

/// Writes an empty (`[]`) Erlang config file into a fresh temporary directory and returns
/// its path **without** the `.config` suffix — the spelling `-config` itself takes.
fn write_temp_config() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("beam_rt_cli_flags_{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("beam_rt_test_config");
    std::fs::write(path.with_extension("config"), "[].\n").ok()?;
    Some(path)
}

/// The oracle's own boot script, resolved through the oracle (`-boot` needs a file that
/// really loads).
fn oracle_boot_script() -> Option<PathBuf> {
    let output = Command::new("erl")
        .args([
            "-noshell",
            "-eval",
            "io:format(\"~s\", [code:root_dir()]), halt(0).",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        return None;
    }
    let script = PathBuf::from(root).join("bin").join("no_dot_erlang");
    script.with_extension("boot").is_file().then_some(script)
}

/// The `-`-switches record their value in `init:get_argument/1`, identically in both
/// runtimes. Skips loudly when `erl` (or the temporary files it needs) is unavailable.
#[test]
fn the_new_configuration_switches_reach_init_get_argument_like_the_oracle() {
    if !erl_available() {
        println!("SKIP cli_flags: `erl` is not runnable — the oracle cross-check is skipped");
        return;
    }
    let Some(config) = write_temp_config() else {
        println!("SKIP cli_flags: could not create a temporary config file");
        return;
    };
    let Some(boot) = oracle_boot_script() else {
        println!("SKIP cli_flags: could not resolve the oracle's boot script");
        return;
    };
    let config = config.to_str().expect("a UTF-8 temp path").to_string();
    let boot = boot.to_str().expect("a UTF-8 boot path").to_string();

    let cases: &[(&str, &[&str])] = &[
        ("setcookie", &["-setcookie", "abc"]),
        ("config", &["-config", config.as_str()]),
        ("boot", &["-boot", boot.as_str()]),
        ("emu_flavor", &["-emu_flavor", "smp"]),
    ];

    let mut failures: Vec<String> = Vec::new();
    for (flag, args) in cases {
        let probe = argument_probe(flag);
        let oracle = run_probe("erl", &[], args, &probe);
        let rust = run_probe(env!("CARGO_BIN_EXE_beam_rt"), &[], args, &probe);
        if oracle.status.code() != Some(0) {
            failures.push(format!(
                "{flag}: the oracle exited {:?} (stderr {})",
                oracle.status.code(),
                stderr(&oracle)
            ));
        }
        if rust.status.code() != Some(0) {
            failures.push(format!(
                "{flag}: beam_rt exited {:?} (stderr {})",
                rust.status.code(),
                stderr(&rust)
            ));
        }
        if stdout(&oracle) != stdout(&rust) {
            failures.push(format!(
                "{flag}: beam_rt {:?} != oracle {:?}",
                stdout(&rust),
                stdout(&oracle)
            ));
        }
        // Sanity: the probe really answered `{ok,[…]}` (not a swallow-everything `error`).
        if !stdout(&rust).starts_with("ok {ok,[[") {
            failures.push(format!(
                "{flag}: unexpected probe output {:?}",
                stdout(&rust)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// One new-flag invocation whose **acceptance** must match the oracle; `rust_code` pins
/// `beam_rt`'s exact exit code (0 accepted, 2 usage error).
struct FlagCase {
    description: &'static str,
    args: &'static [&'static str],
    rust_code: i32,
}

/// Every new-flag spelling where the two runtimes **agree** on acceptance (both accept, or
/// both reject). `+Q|+P legacy` and the allocator long-forms are accepted; the
/// out-of-range/unknown values are rejected.
const AGREEING_CASES: &[FlagCase] = &[
    FlagCase {
        description: "+Q 1024 (the low end of the port range)",
        args: &["+Q", "1024"],
        rust_code: 0,
    },
    FlagCase {
        description: "+Q 134217727 (the high end of the port range)",
        args: &["+Q", "134217727"],
        rust_code: 0,
    },
    FlagCase {
        description: "+Q legacy",
        args: &["+Q", "legacy"],
        rust_code: 0,
    },
    FlagCase {
        description: "+Q 1000 (below ERTS_MIN_PORTS)",
        args: &["+Q", "1000"],
        rust_code: 2,
    },
    FlagCase {
        description: "+Q 0",
        args: &["+Q", "0"],
        rust_code: 2,
    },
    FlagCase {
        description: "+Q 134217728 (above ERTS_MAX_PORTS)",
        args: &["+Q", "134217728"],
        rust_code: 2,
    },
    FlagCase {
        description: "+Q abc",
        args: &["+Q", "abc"],
        rust_code: 2,
    },
    FlagCase {
        description: "+P 1024 (the low end of the process range)",
        args: &["+P", "1024"],
        rust_code: 0,
    },
    FlagCase {
        description: "+P 134217727 (the high end of the process range)",
        args: &["+P", "134217727"],
        rust_code: 0,
    },
    FlagCase {
        description: "+P legacy",
        args: &["+P", "legacy"],
        rust_code: 0,
    },
    FlagCase {
        description: "+P 1000 (below ERTS_MIN_PROCESSES)",
        args: &["+P", "1000"],
        rust_code: 2,
    },
    FlagCase {
        description: "+P 0",
        args: &["+P", "0"],
        rust_code: 2,
    },
    FlagCase {
        description: "+P 134217728 (above ERTS_MAX_PROCESSES)",
        args: &["+P", "134217728"],
        rust_code: 2,
    },
    FlagCase {
        description: "+K true",
        args: &["+K", "true"],
        rust_code: 0,
    },
    FlagCase {
        description: "+K false",
        args: &["+K", "false"],
        rust_code: 0,
    },
    FlagCase {
        description: "+sbt u",
        args: &["+sbt", "u"],
        rust_code: 0,
    },
    FlagCase {
        description: "+sbt db",
        args: &["+sbt", "db"],
        rust_code: 0,
    },
    FlagCase {
        description: "+sbt s",
        args: &["+sbt", "s"],
        rust_code: 0,
    },
    FlagCase {
        description: "+sbt ps",
        args: &["+sbt", "ps"],
        rust_code: 0,
    },
    FlagCase {
        description: "+sbt ts",
        args: &["+sbt", "ts"],
        rust_code: 0,
    },
    FlagCase {
        description: "+sbt tnnps",
        args: &["+sbt", "tnnps"],
        rust_code: 0,
    },
    FlagCase {
        description: "+sbt nnps",
        args: &["+sbt", "nnps"],
        rust_code: 0,
    },
    FlagCase {
        description: "+sbt nnts",
        args: &["+sbt", "nnts"],
        rust_code: 0,
    },
    FlagCase {
        description: "+sbt ns",
        args: &["+sbt", "ns"],
        rust_code: 0,
    },
    FlagCase {
        description: "+sbt bogus",
        args: &["+sbt", "bogus"],
        rust_code: 2,
    },
    FlagCase {
        description: "+MMscs 1024 (the mseg carrier size)",
        args: &["+MMscs", "1024"],
        rust_code: 0,
    },
    FlagCase {
        description: "+MBas aobf (an alloc-util allocation strategy)",
        args: &["+MBas", "aobf"],
        rust_code: 0,
    },
    FlagCase {
        description: "+MBasbf (the same strategy, spelled in-token)",
        args: &["+MBasbf"],
        rust_code: 0,
    },
    FlagCase {
        description: "+Mlpm no",
        args: &["+Mlpm", "no"],
        rust_code: 0,
    },
    FlagCase {
        description: "+MYe true (the system allocator)",
        args: &["+MYe", "true"],
        rust_code: 0,
    },
    FlagCase {
        description: "+MIscs 100 (the literal allocator's carrier size)",
        args: &["+MIscs", "100"],
        rust_code: 0,
    },
    FlagCase {
        description: "+MMamcbf 100",
        args: &["+MMamcbf", "100"],
        rust_code: 0,
    },
    FlagCase {
        description: "+MZas chaosff (test_alloc's own strategy)",
        args: &["+MZas", "chaosff"],
        rust_code: 0,
    },
    FlagCase {
        description: "+Mfoo (an unknown allocator letter)",
        args: &["+Mfoo"],
        rust_code: 2,
    },
    FlagCase {
        description: "+M (no allocator letter at all)",
        args: &["+M"],
        rust_code: 2,
    },
    FlagCase {
        description: "+MMscs bogus (a non-numeric carrier size)",
        args: &["+MMscs", "bogus"],
        rust_code: 2,
    },
    FlagCase {
        description: "+MBas chaosff (chaosff is test_alloc-only)",
        args: &["+MBas", "chaosff"],
        rust_code: 2,
    },
    FlagCase {
        description: "+MYe false (the system allocator only accepts true)",
        args: &["+MYe", "false"],
        rust_code: 2,
    },
    FlagCase {
        description: "+Mea min (the emulator-wide allocator preset)",
        args: &["+Mea", "min"],
        rust_code: 0,
    },
    FlagCase {
        description: "+Mea 1024 (an unknown -Mea value)",
        args: &["+Mea", "1024"],
        rust_code: 2,
    },
    FlagCase {
        description: "+MIe false (literal_alloc cannot be disabled)",
        args: &["+MIe", "false"],
        rust_code: 2,
    },
    FlagCase {
        description: "+MTe false (temp_alloc cannot be disabled)",
        args: &["+MTe", "false"],
        rust_code: 2,
    },
    FlagCase {
        description: "+MIt true (literal_alloc has no thread-specific instance)",
        args: &["+MIt", "true"],
        rust_code: 2,
    },
    FlagCase {
        description: "+MIcp B (literal_alloc cannot migrate carriers)",
        args: &["+MIcp", "B"],
        rust_code: 2,
    },
    FlagCase {
        description: "+MTacul 50 (temp_alloc cannot migrate carriers)",
        args: &["+MTacul", "50"],
        rust_code: 2,
    },
    FlagCase {
        description: "-Mfoo (a -M* flag is an ordinary unknown -flag)",
        args: &["-Mfoo"],
        rust_code: 0,
    },
    FlagCase {
        description: "-M (the bare -M* prefix)",
        args: &["-M"],
        rust_code: 0,
    },
    FlagCase {
        description: "-Mea 1024 (a -M* value is consumed as the init argument's value)",
        args: &["-Mea", "1024"],
        rust_code: 0,
    },
    FlagCase {
        description: "-emu_flavor smp (the oracle's only accepted flavor)",
        args: &["-emu_flavor", "smp"],
        rust_code: 0,
    },
    FlagCase {
        description: "-setcookie abc",
        args: &["-setcookie", "abc"],
        rust_code: 0,
    },
];

/// The two runtimes agree on which of the new flag spellings are accepted — and
/// `beam_rt`'s exact exit code is the one pinned in the table. Skips loudly without `erl`.
#[test]
fn every_new_emulator_flag_agrees_with_the_oracle_on_acceptance() {
    if !erl_available() {
        println!("SKIP cli_flags: `erl` is not runnable — the oracle cross-check is skipped");
        return;
    }
    let beam = env!("CARGO_BIN_EXE_beam_rt");
    let mut failures: Vec<String> = Vec::new();
    for case in AGREEING_CASES {
        let oracle_ok = accepts("erl", case.args);
        let rust = run_probe(beam, &[], case.args, "halt(0).");
        let rust_ok = rust.status.success();
        if oracle_ok != rust_ok {
            failures.push(format!(
                "{}: the oracle accepts={oracle_ok} but beam_rt accepts={rust_ok} \
                 (stderr {})",
                case.description,
                stderr(&rust)
            ));
        }
        if rust.status.code() != Some(case.rust_code) {
            failures.push(format!(
                "{}: beam_rt exit {:?} != {} (stderr {})",
                case.description,
                rust.status.code(),
                case.rust_code,
                stderr(&rust)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The **documented divergences** from the oracle, pinned so a future change cannot
/// silently move them. Each entry is `(description, args, oracle_accepts, beam_rt_code)`.
const DIVERGENT_CASES: &[(&str, &[&str], bool, i32)] = &[
    (
        "-emu_flavor emu (the oracle's `erlexec` accepts only `smp`)",
        &["-emu_flavor", "emu"],
        false,
        0,
    ),
    (
        "+K bogus (the emulator never validates the +K value)",
        &["+K", "bogus"],
        true,
        2,
    ),
];

/// `beam_rt`'s side of each divergence is pinned always; the oracle's own value is
/// checked when `erl` is available (the `beam_rt` half still runs without it).
#[test]
fn the_documented_oracle_divergences_are_pinned() {
    let beam = env!("CARGO_BIN_EXE_beam_rt");
    let oracle_available = erl_available();
    if !oracle_available {
        println!("SKIP cli_flags: `erl` is not runnable — only the beam_rt half is pinned");
    }
    let mut failures: Vec<String> = Vec::new();
    for (description, args, oracle_accepts, rust_code) in DIVERGENT_CASES {
        let rust = run_probe(beam, &[], args, "halt(0).");
        if rust.status.code() != Some(*rust_code) {
            failures.push(format!(
                "{description}: beam_rt exit {:?} != {rust_code} (stderr {})",
                rust.status.code(),
                stderr(&rust)
            ));
        }
        if oracle_available && accepts("erl", args) != *oracle_accepts {
            failures.push(format!(
                "{description}: the oracle accepts={} (expected {oracle_accepts})",
                accepts("erl", args)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // The divergences must stay **disjoint** from the agreeing table: none of them may be
    // silently promoted into it.
    for (description, args, _, _) in DIVERGENT_CASES {
        assert!(
            !AGREEING_CASES.iter().any(|case| case.args == *args),
            "{description} must not also be listed as agreeing"
        );
    }
}
