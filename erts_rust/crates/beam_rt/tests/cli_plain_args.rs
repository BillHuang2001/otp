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

//! A.6 (CLI/configuration): the **plain arguments** — `init:get_plain_arguments/0` — through
//! the built binary, end to end.
//!
//! Real `erl` gives two command-line markers a meaning `init` alone cannot express:
//!
//! * `-extra` — everything after it is a plain argument (`erts/preloaded/src/init.erl`, the
//!   `{extra, …}` arm), and flag parsing stops there: a `-eval` after `-extra` is data.
//! * `--` — the same, but the run ends at the first *switch-looking* token (so
//!   `-- a -noshell b` is `["a"]`, while `-extra a -noshell b` is `["a","-noshell","b"]`).
//!
//! Neither token is an `init` argument, and neither is any plain value: this is what makes
//! `erlc`'s own front end work, since it passes the file list as
//! `… -s erl_compile compile_cmdline -extra -o DIR SRC…` (see `tests/erlc_emulator.rs`).
//! The runtime's split lives in `src/cli/env.rs` (`split_plain` per merge entity,
//! `merge_argv_with_plain` for the whole `ERL_*FLAGS` merge) and the values land in
//! [`RuntimeConfig::plain_args`](beam_rt::config::RuntimeConfig::plain_args), which
//! `src/native/init.rs` reports.
//!
//! # The goldens are the oracle's own answers
//!
//! Every value pinned below was produced against the system `erl` (OTP 29.1.1 /
//! erts-17.1) from this checkout with
//!
//! ```text
//! erl -noshell -eval 'io:format("~p~n",[init:get_plain_arguments()]),halt(0).' <TAIL>
//!   -extra a b c                → ["a","b","c"]
//!   -extra a b -extra c         → ["a","b","-extra","c"]
//!   -extra                      → []
//!   -- a b                      → ["a","b"]
//!   -- a -noshell b             → ["a"]
//!   -- a -extra b               → ["a","b"]
//!   ERL_AFLAGS='-extra a' ERL_ZFLAGS='-extra d' … -extra b -extra c
//!                               → ["a","b","-extra","c","d"]
//! ```
//!
//! so the pinned half needs no toolchain at all; every cross-checkable row then runs the
//! **same command line** against the system `erl` and asserts both sides identical, and a
//! missing `erl` skips that half **loudly** ([`support::require_oracle`]).
//!
//! # Two things this suite deliberately does not do
//!
//! * It never appends `-s init stop` to an `-eval` line: this runtime's action switches are
//!   last-wins, so `-eval X -s init stop` would run the `-s` action and drop the eval. Every
//!   probe ends with its own `halt(0).` instead, and every child is bounded by the shared
//!   harness (`support::run_bounded_with_env`), so a blocked probe is killed and reaped
//!   rather than able to hang the suite.
//! * It does not run the oracle half of the `-extra`-terminates-parsing row: with no action
//!   left the oracle's `-noshell` node has nothing to do and waits for ever (`timeout 10 erl
//!   -noshell -extra a -eval 'io:format("x")'` → killed at the bound, i.e. it prints nothing
//!   and never exits). This runtime has no shell to wait for, so it exits **0** — which is
//!   what the row pins (empty stdout, empty stderr, exit 0), and the shared property (the
//!   `-eval` became a plain argument) is pinned on the library side as well.

mod support;

use std::path::Path;
use std::process::Output;

use beam_rt::cli::{parse, parse_with_env, MapEnv};

/// The probe: print `init:get_plain_arguments/0` with `~w`-style escaping and halt.
///
/// `~p` is what the recorded oracle probes used (a list of printable strings renders as
/// `["a","b"]`, which is both readable and unambiguous).
const PLAIN_PROBE: &str = "io:format(\"~p~n\",[init:get_plain_arguments()]),halt(0).";

/// The `-extra -o outdir` probe: the plain list **and** the two `init:get_argument/1` lookups
/// that must both be the bare atom `error` (neither `-extra` nor `-o` is an init argument).
const EXTRA_O_PROBE: &str = "io:format(\"~p ~p ~p~n\",[init:get_plain_arguments(),\
                             init:get_argument(extra),init:get_argument(o)]),halt(0).";

// ---------------------------------------------------------------------------------------
// Running one command line
// ---------------------------------------------------------------------------------------

/// One finished child: its exit code and its trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

impl Run {
    /// The trimmed view of a captured [`Output`].
    fn of(output: &Output) -> Self {
        Run {
            code: output.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        }
    }
}

/// The environment names every probe removes before applying its own: the whole
/// `ERL_*FLAGS` family, including any `ERL_OTP<vsn>_FLAGS` the host happens to carry (this
/// runtime's own name from [`beam_rt::cli::otp_flags_var`] plus every inherited spelling,
/// because the oracle's release number differs). A case is then exactly what the test sets.
fn stripped_names() -> Vec<String> {
    let mut names = vec![
        "ERL_AFLAGS".to_string(),
        "ERL_FLAGS".to_string(),
        "ERL_ZFLAGS".to_string(),
        beam_rt::cli::otp_flags_var(),
    ];
    for (name, _) in std::env::vars_os() {
        if let Some(name) = name.to_str() {
            if name.starts_with("ERL_OTP") && name.ends_with("_FLAGS") {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Runs `args` under `program` with every inherited `ERL_*FLAGS` variable removed and
/// `envs` applied, bounded by the shared harness so a blocked probe cannot hang the suite.
fn run(program: &str, args: &[&str], envs: &[(&str, &str)]) -> Run {
    let names = stripped_names();
    let mut env: Vec<(&str, Option<&str>)> = names
        .iter()
        .map(|name| (name.as_str(), None))
        .collect();
    for (name, value) in envs {
        env.push((name, Some(*value)));
    }
    let output = support::run_bounded_with_env(Path::new(program), args, &env).unwrap_or_else(|| {
        panic!(
            "`{program} {}` outlived the {}s probe bound (stderr was unavailable)",
            args.join(" "),
            support::PROBE_TIMEOUT.as_secs()
        )
    });
    Run::of(&output)
}

// ---------------------------------------------------------------------------------------
// The matrix
// ---------------------------------------------------------------------------------------

/// Whether the oracle half of a row can be run at all, and how.
enum OracleHalf {
    /// The same command line is runnable under `erl` and must answer identically.
    CrossCheck,
    /// The oracle cannot be driven through this row; `why` says so.
    NotRun(&'static str),
}

/// One row of the plain-argument matrix.
struct Case {
    /// What the row pins (used in the assertion messages).
    description: &'static str,
    /// The **whole** argv after the program name.
    args: &'static [&'static str],
    /// The `ERL_*FLAGS` variables the row sets.
    envs: &'static [(&'static str, &'static str)],
    /// The oracle's verbatim answer (trimmed).
    expected: &'static str,
    /// Whether/how the oracle half runs.
    oracle_half: OracleHalf,
}

/// The matrix: every shape of the `-extra`/`--` rule, each with the oracle's own answer.
const CASES: &[Case] = &[
    Case {
        description: "`-extra a b c` selects three plain arguments",
        args: &["-noshell", "-eval", PLAIN_PROBE, "-extra", "a", "b", "c"],
        envs: &[],
        expected: "[\"a\",\"b\",\"c\"]",
        oracle_half: OracleHalf::CrossCheck,
    },
    Case {
        description: "a second `-extra` is an ordinary plain value, not a new marker",
        args: &["-noshell", "-eval", PLAIN_PROBE, "-extra", "a", "b", "-extra", "c"],
        envs: &[],
        expected: "[\"a\",\"b\",\"-extra\",\"c\"]",
        oracle_half: OracleHalf::CrossCheck,
    },
    Case {
        description: "a bare trailing `-extra` selects nothing",
        args: &["-noshell", "-eval", PLAIN_PROBE, "-extra"],
        envs: &[],
        expected: "[]",
        oracle_half: OracleHalf::CrossCheck,
    },
    Case {
        description: "`-- a b` selects two plain arguments",
        args: &["-noshell", "-eval", PLAIN_PROBE, "--", "a", "b"],
        envs: &[],
        expected: "[\"a\",\"b\"]",
        oracle_half: OracleHalf::CrossCheck,
    },
    Case {
        description: "a `--` run ends at the first switch-looking token",
        args: &["-noshell", "-eval", PLAIN_PROBE, "--", "a", "-noshell", "b"],
        envs: &[],
        expected: "[\"a\"]",
        oracle_half: OracleHalf::CrossCheck,
    },
    Case {
        description: "a `--` run keeps a following `-extra` and its values",
        args: &["-noshell", "-eval", PLAIN_PROBE, "--", "a", "-extra", "b"],
        envs: &[],
        expected: "[\"a\",\"b\"]",
        oracle_half: OracleHalf::CrossCheck,
    },
    Case {
        description: "the ERL_AFLAGS/ERL_ZFLAGS tails merge around the command line's own",
        args: &["-noshell", "-eval", PLAIN_PROBE, "-extra", "b", "-extra", "c"],
        envs: &[("ERL_AFLAGS", "-extra a"), ("ERL_ZFLAGS", "-extra d")],
        expected: "[\"a\",\"b\",\"-extra\",\"c\",\"d\"]",
        oracle_half: OracleHalf::CrossCheck,
    },
    Case {
        description: "`-extra -o outdir` makes neither `extra` nor `o` an init argument",
        args: &["-noshell", "-eval", EXTRA_O_PROBE, "-extra", "-o", "outdir"],
        envs: &[],
        expected: "[\"-o\",\"outdir\"] error error",
        oracle_half: OracleHalf::CrossCheck,
    },
    Case {
        description: "`-extra` terminates flag parsing (the `-eval` becomes a plain value)",
        args: &["-noshell", "-extra", "a", "-eval", PLAIN_PROBE],
        envs: &[],
        expected: "",
        oracle_half: OracleHalf::NotRun(
            "the oracle's `-noshell` node has no action left and waits for ever \
             (`timeout 10 erl -noshell -extra a -eval 'io:format(\"x\")'` is killed at the bound)",
        ),
    },
];

/// Every row: the pinned answer on the built binary, then — where the row is drivable — the
/// identical command line under the system oracle, asserting both sides byte-identical.
#[test]
fn the_plain_argument_matrix_agrees_with_the_oracle() {
    let oracle = support::require_oracle("cli_plain_args");
    let beam = env!("CARGO_BIN_EXE_beam_rt");
    let mut failures: Vec<String> = Vec::new();
    for case in CASES {
        let ours = run(beam, case.args, case.envs);
        if ours.code != 0 {
            failures.push(format!(
                "{}: beam_rt exited {:?} (stderr {:?})",
                case.description, ours.code, ours.err
            ));
            continue;
        }
        if ours.out != case.expected {
            failures.push(format!(
                "{}: beam_rt answered {:?} != the oracle's {:?}",
                case.description, ours.out, case.expected
            ));
        }
        if !ours.err.is_empty() {
            failures.push(format!(
                "{}: beam_rt wrote to stderr: {:?}",
                case.description, ours.err
            ));
        }
        let Some(oracle) = oracle.as_deref() else {
            continue;
        };
        match case.oracle_half {
            OracleHalf::CrossCheck => {
                let theirs = run(
                    oracle.to_str().expect("the oracle path is UTF-8"),
                    case.args,
                    case.envs,
                );
                if theirs.out != ours.out {
                    failures.push(format!(
                        "{}: beam_rt {:?} != oracle {:?}",
                        case.description, ours.out, theirs.out
                    ));
                }
                if theirs.code != 0 {
                    failures.push(format!(
                        "{}: the oracle exited {:?} (stderr {:?})",
                        case.description, theirs.code, theirs.err
                    ));
                }
            }
            OracleHalf::NotRun(why) => println!(
                "note cli_plain_args: the oracle half of {:?} is not run: {why}",
                case.description
            ),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The `-extra` marker stops flag parsing for **this** runtime too: nothing is printed, the
/// exit status is 0 and stderr is empty (see the module docs for why the oracle has no
/// equivalent drivable shape).
#[test]
fn extra_terminates_flag_parsing() {
    let ours = run(
        env!("CARGO_BIN_EXE_beam_rt"),
        &[
            "-noshell",
            "-extra",
            "a",
            "-eval",
            "io:format(\"this must not run\"),halt(0).",
        ],
        &[],
    );
    assert_eq!(ours.code, 0, "stderr: {:?}", ours.err);
    assert_eq!(ours.out, "", "the `-eval` became a plain argument");
    assert_eq!(ours.err, "");
}

// ---------------------------------------------------------------------------------------
// A marker-free invocation
// ---------------------------------------------------------------------------------------

/// With no `-extra` and no `--` the list is empty on **both** runtimes — the baseline the
/// rest of the matrix is a delta from. The oracle half goes through the shared
/// [`support::oracle_eval`] helper (bounded, `-s init stop`-terminated, loud skip without a
/// toolchain); the probe halts itself, so the appended `-s init stop` is only the backstop.
#[test]
fn a_marker_free_invocation_selects_no_plain_arguments() {
    let ours = run(
        env!("CARGO_BIN_EXE_beam_rt"),
        &["-noshell", "-eval", PLAIN_PROBE],
        &[],
    );
    assert_eq!(ours.code, 0, "stderr: {:?}", ours.err);
    assert_eq!(ours.out, "[]");
    assert_eq!(ours.err, "");

    let Some(oracle) = support::require_oracle("cli_plain_args") else {
        return;
    };
    let theirs = support::oracle_eval(&oracle, None, PLAIN_PROBE)
        .expect("the live oracle must answer the plain-argument probe");
    assert_eq!(theirs, ours.out);
}

// ---------------------------------------------------------------------------------------
// The library side: the split is in the parser, not in the CLI driver
// ---------------------------------------------------------------------------------------
/// Parses `args` with no environment at all.
fn parsed(args: &[&str]) -> beam_rt::config::RuntimeConfig {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    parse(&args).unwrap_or_else(|error| panic!("parse({args:?}) failed: {error}"))
}

/// Parses `args` under exactly `pairs` (no inherited variable is visible).
fn parsed_with_env(args: &[&str], pairs: &[(&str, &str)]) -> beam_rt::config::RuntimeConfig {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let env = MapEnv(
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
    );
    parse_with_env(&args, &env)
        .unwrap_or_else(|error| panic!("parse_with_env({args:?}, {pairs:?}) failed: {error}"))
}

/// `RuntimeConfig::plain_args` is exactly the matrix's value, and **neither** the marker nor
/// a plain token is recorded as an init argument — the split is pinned at the parser level,
/// so a CLI-driver regression cannot hide it.
#[test]
fn the_parser_splits_plain_arguments_out_of_the_init_arguments() {
    // `-extra` and `--`: the plain tail is the field's whole content …
    for (args, expected) in [
        (&["-noshell", "-extra", "a", "b", "c"][..], &["a", "b", "c"][..]),
        (
            &["-noshell", "-extra", "a", "b", "-extra", "c"][..],
            &["a", "b", "-extra", "c"][..],
        ),
        (&["-noshell", "-extra"][..], &[][..]),
        (&["-noshell", "--", "a", "b"][..], &["a", "b"][..]),
        (&["-noshell", "--", "a", "-noshell", "b"][..], &["a"][..]),
        (&["-noshell", "--", "a", "-extra", "b"][..], &["a", "b"][..]),
        (
            &["-noshell", "-eval", PLAIN_PROBE, "-extra", "-o", "outdir"][..],
            &["-o", "outdir"][..],
        ),
        // A marker-free stream selects nothing at all.
        (&["-noshell", "-eval", PLAIN_PROBE][..], &[][..]),
    ] {
        let config = parsed(args);
        let plain: Vec<&str> = config.plain_args.iter().map(String::as_str).collect();
        assert_eq!(plain, expected, "{args:?}");
        // … and no plain token — and no marker — is an init argument.
        for name in ["extra", "o", "a", "b", "c", "outdir", "-o"] {
            assert_eq!(config.init_args.get(name), None, "{args:?}: {name}");
        }
        assert!(
            config.init_args.get("noshell").is_some(),
            "{args:?}: `-noshell` is still an init argument"
        );
    }

    // The `-extra -o outdir` row again, field by field: the whole init-argument set is
    // `{noshell, []}` — the oracle answers the bare atom `error` for `extra` and for `o`.
    let config = parsed(&["-noshell", "-eval", PLAIN_PROBE, "-extra", "-o", "outdir"]);
    assert_eq!(config.plain_args, vec!["-o".to_string(), "outdir".to_string()]);
    assert_eq!(config.init_args.len(), 1);
    assert!(config.init_args.get("noshell").is_some());
    assert_eq!(config.init_args.get("extra"), None);
    assert_eq!(config.init_args.get("o"), None);
}

/// The same split through the `ERL_*FLAGS` merge: each entity splits at its **own** marker,
/// and the plain tails land in the merge order (AFLAGS, the command line, ZFLAGS) — the
/// `["a","b","-extra","c","d"]` the oracle answers.
#[test]
fn the_merge_contributes_each_entitys_own_plain_tail() {
    let config = parsed_with_env(
        &["-noshell", "-eval", PLAIN_PROBE, "-extra", "b", "-extra", "c"],
        &[("ERL_AFLAGS", "-extra a"), ("ERL_ZFLAGS", "-extra d")],
    );
    assert_eq!(
        config.plain_args,
        vec![
            "a".to_string(),
            "b".to_string(),
            "-extra".to_string(),
            "c".to_string(),
            "d".to_string(),
        ]
    );
    // None of the five is an init argument, and the marker itself never is one either.
    assert_eq!(config.init_args.get("extra"), None);
    assert_eq!(config.init_args.get("a"), None);
    assert_eq!(config.init_args.get("b"), None);
    assert_eq!(config.init_args.get("d"), None);
    assert!(config.init_args.get("noshell").is_some());

    // An `-extra`-borne flag is data too: it does not become a flag, so `-noshell` arriving
    // that way is a plain value (the oracle's rule — `-extra -noshell` never silences the
    // shell, it selects the string `"-noshell"`).
    let config = parsed_with_env(&[], &[("ERL_AFLAGS", "-extra -noshell")]);
    assert_eq!(config.plain_args, vec!["-noshell".to_string()]);
    assert_eq!(config.init_args.get("noshell"), None);
}
