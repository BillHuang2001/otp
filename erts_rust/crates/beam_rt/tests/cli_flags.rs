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
    let mut command = Command::new(program);
    // Strip any inherited variable so each case is exactly what the test sets.
    for name in ["ERL_AFLAGS", "ERL_FLAGS", "ERL_ZFLAGS"] {
        command.env_remove(name);
    }
    command.env_remove(otp_flags_var());
    command.args(["-noshell"]);
    command.args(extra);
    command.args(["-eval", PROBE]);
    for (name, value) in envs {
        command.env(name, value);
    }
    command
        .output()
        .unwrap_or_else(|error| panic!("spawning {program} failed: {error}"))
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
    assert!(output.stderr.is_empty(), "env {envs:?}: {}", stderr(&output));
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
        expected(&[
            "otpcookie",
            "acookie",
            "cmdcookie",
            "fcookie",
            "zcookie"
        ])
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
    assert_probe(
        &[("ERL_FLAGS", "-noshell -setcookie multi")],
        &["multi"],
    );
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
