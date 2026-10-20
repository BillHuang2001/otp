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

//! A.6 (CLI/configuration): the emulator's **attached (`+Xvalue`) `+`-flag forms** through
//! the built binary.
//!
//! The emulator's own reader is `get_arg(char *rest, char *next, int *ip)`
//! (`erts/emulator/beam/erl_init.c:496-509`): the token's own remainder after the switch
//! text **is** the value and consumes no further token, and an empty remainder takes the
//! following token. So `+S1` and `+S 1` name the same switch with the same value — and the
//! attached form is not an exotic corner, it is what a real `erlc` uses to launch the
//! emulator. erlc's front end argv (captured from this checkout with a stand-in
//! `$ERLC_EMULATOR`) is
//!
//! ```text
//! +S1 +sbtu +A0 +P 65536 +Q 1024 -noinput -mode minimal -boot no_dot_erlang \
//!   -s erl_compile compile_cmdline -extra -o DIR SRC…
//! ```
//!
//! — the three attached spellings `+S1`, `+sbtu`, `+A0` plus two separated ones. Rejecting
//! the attached form made every `ERLC_EMULATOR=<beam_rt> erlc …` invocation die on
//! `beam_rt: unknown flag -S1` before it compiled anything; this suite pins that it works
//! (and `tests/erlc_emulator.rs` drives the whole route).
//!
//! # What each row asserts
//!
//! * the **combined** row is the `erlc` front-end group, and its three driven observables
//!   (`schedulers_online`, `process_limit`, `port_limit`) are the oracle's own answer;
//! * every **twin** row asserts (i) the two spellings are indistinguishable to the parser
//!   (`RuntimeConfig` derives `PartialEq`, so one `assert_eq!` compares the scheduler count,
//!   the heap policy, the `+hmax*` flag word and the preserved `extra_emulator_flags`
//!   channel), (ii) the flag really reached the configuration (the config differs from the
//!   marker-free baseline), and (iii) the two spellings produce the **same** driven
//!   observable — the `system_info` values the oracle answers, or, for the rows with no
//!   `system_info` observable, only the parser equality;
//! * the **refusals** are unchanged in both spellings: exit 1, one stderr line, no `USAGE`;
//! * no `+` switch is ever an `init` argument (`init:get_arguments/0` is unchanged by them).
//!
//! # Two rows where the two runtimes cannot legitimately agree
//!
//! * `+SP50` — the oracle answers `schedulers` 8 (50 % of this host's 16 logical CPUs) and
//!   this runtime answers its own default (14, the physical count): the *scheduler-count
//!   policy* is a runtime decision, so the row asserts the twin rule and the
//!   `schedulers_online =< schedulers` invariant, never the oracle's figure.
//! * `+sbwt1000` — the oracle **rejects a numeric busy-wait threshold** in *both* spellings
//!   (`bad scheduler busy wait threshold: 1000` + the `beam.smp` usage block, exit 1) while
//!   this runtime accepts it verbatim; that is a *value-validation* difference, not a
//!   spelling one, so the row asserts the twin rule and the same-config property on this
//!   runtime only. The same switch with a symbolic value (`+sbwtlong`/`+sbwt long`) is
//!   accepted by both runtimes and carries the oracle cross-check instead.
//!
//! # Harness
//!
//! Every child is bounded by the shared harness ([`support::run_bounded_with_env`], with the
//! whole `ERL_*FLAGS` family stripped so a host variable cannot decide a row) and every probe
//! ends with its own `halt(0).` — `-s init stop` is never appended, because this runtime's
//! action switches are last-wins. A missing `erl` skips the oracle half **loudly**
//! ([`support::require_oracle`]), never the pinned half.

mod support;

use std::path::Path;
use std::process::Output;

use beam_rt::cli::parse;
use beam_rt::config::RuntimeConfig;

// ---------------------------------------------------------------------------------------
// Probes
// ---------------------------------------------------------------------------------------

/// The `erlc` front-end group's observable: the three figures its three flags drive.
const FRONT_END_PROBE: &str = "io:format(\"~p ~p ~p~n\",[erlang:system_info(schedulers_online),\
                               erlang:system_info(process_limit),\
                               erlang:system_info(port_limit)]),halt(0).";

/// `erlang:system_info(schedulers)`.
const SCHEDULERS_PROBE: &str =
    "io:format(\"~p~n\",[erlang:system_info(schedulers)]),halt(0).";

/// `erlang:system_info(thread_pool_size)` — the figure `+A` drives.
const THREAD_POOL_PROBE: &str =
    "io:format(\"~p~n\",[erlang:system_info(thread_pool_size)]),halt(0).";

/// `erlang:system_info(process_limit)` — the figure `+P` drives.
const PROCESS_LIMIT_PROBE: &str =
    "io:format(\"~p~n\",[erlang:system_info(process_limit)]),halt(0).";

/// `erlang:system_info(port_limit)` — the figure `+Q` drives.
const PORT_LIMIT_PROBE: &str = "io:format(\"~p~n\",[erlang:system_info(port_limit)]),halt(0).";

/// `garbage_collection`'s `min_heap_size` — the figure `+hms` drives.
const MIN_HEAP_PROBE: &str = "io:format(\"~p~n\",[proplists:get_value(min_heap_size,\
                              erlang:system_info(garbage_collection))]),halt(0).";

/// `erlang:system_info(scheduler_bind_type)` — the figure `+sbt` drives.
const BIND_TYPE_PROBE: &str =
    "io:format(\"~p~n\",[erlang:system_info(scheduler_bind_type)]),halt(0).";

/// Whether any `+` switch is an `init` argument: the answer must be `false`.
const INIT_ARGUMENTS_PROBE: &str =
    "io:format(\"~p~n\",[lists:keymember('S',1,init:get_arguments())]),halt(0).";

/// The whole `init:get_arguments/0` list, for the before/after comparison.
const ALL_ARGUMENTS_PROBE: &str = "io:format(\"~p~n\",[init:get_arguments()]),halt(0).";

// ---------------------------------------------------------------------------------------
// Running one command line
// ---------------------------------------------------------------------------------------

/// One finished child: its exit code and its trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// The environment names every probe removes before applying its own: the whole
/// `ERL_*FLAGS` family, including any inherited `ERL_OTP<vsn>_FLAGS` spelling (the oracle's
/// release number differs from this runtime's).
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

/// Runs `<program> <flags…> -noshell -eval <probe>`, bounded by the shared harness.
fn run(program: &str, flags: &[&str], probe: &str) -> Run {
    let names = stripped_names();
    let env: Vec<(&str, Option<&str>)> = names
        .iter()
        .map(|name| (name.as_str(), None))
        .collect();
    let mut args: Vec<&str> = flags.to_vec();
    args.push("-noshell");
    args.push("-eval");
    args.push(probe);
    let output = support::run_bounded_with_env(Path::new(program), &args, &env).unwrap_or_else(
        || {
            panic!(
                "`{program} {}` outlived the {}s probe bound",
                args.join(" "),
                support::PROBE_TIMEOUT.as_secs()
            )
        },
    );
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// The built binary, as a `&str` path.
fn beam_rt() -> &'static str {
    env!("CARGO_BIN_EXE_beam_rt")
}

/// The child's whole captured stderr, untrimmed — a refusal's exact text is the assertion.
fn raw_stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// ---------------------------------------------------------------------------------------
// The parser side
// ---------------------------------------------------------------------------------------

/// The configuration `flags` parse to (with a trailing `-noshell`, so the init-argument set
/// is never empty for an incidental reason).
fn config_of(flags: &[&str]) -> RuntimeConfig {
    let mut args: Vec<String> = flags.iter().map(|flag| flag.to_string()).collect();
    args.push("-noshell".to_string());
    parse(&args).unwrap_or_else(|error| panic!("parse({args:?}) failed: {error}"))
}

// ---------------------------------------------------------------------------------------
// (a) the erlc front-end group
// ---------------------------------------------------------------------------------------

/// The exact group a real `erlc` launches the emulator with answers the oracle's figures:
/// `schedulers_online` 1 (`+S1`), `process_limit` 65536 (`+P 65536`) and `port_limit` 1024
/// (`+Q 1024`). `+sbtu` and `+A0` are carried too (they have no figure of their own here).
#[test]
fn the_erlc_front_end_flag_group_answers_the_oracles_figures() {
    let flags = ["+S1", "+sbtu", "+A0", "+P", "65536", "+Q", "1024"];
    let ours = run(beam_rt(), &flags, FRONT_END_PROBE);
    assert_eq!(ours.code, 0, "stderr: {:?}", ours.err);
    assert_eq!(ours.err, "");
    assert_eq!(ours.out, "1 65536 1024");

    let Some(oracle) = support::require_oracle("cli_compact_flags") else {
        return;
    };
    let theirs = run(
        oracle.to_str().expect("the oracle path is UTF-8"),
        &flags,
        FRONT_END_PROBE,
    );
    assert_eq!(theirs.code, 0, "the oracle's stderr: {:?}", theirs.err);
    assert_eq!(theirs.out, ours.out);
}

// ---------------------------------------------------------------------------------------
// (b) the attached-vs-separated twin matrix
// ---------------------------------------------------------------------------------------

/// One twin row: the same switch written compactly and separated.
struct Twin {
    /// What the row pins (used in the assertion messages).
    description: &'static str,
    /// The compact spelling, exactly as `erlc` writes it where it does.
    attached: &'static [&'static str],
    /// The separated spelling.
    separated: &'static [&'static str],
    /// The driven observable, or `None` when the switch has no `system_info` figure (the
    /// parser equality is then the whole pin).
    probe: Option<&'static str>,
    /// The line both spellings must print on this runtime, where pinning one is meaningful.
    golden: Option<&'static str>,
    /// Whether the oracle must print the same line. `false` for the two rows documented in
    /// the module docs as policy/validation differences.
    oracle_agrees: bool,
    /// Whether the compact spelling must move the parsed configuration off the marker-free
    /// baseline. `false` for the two rows whose value **is** the emulator default (`+hms233`
    /// is `DEFAULT_MIN_HEAP_WORDS` and `+hmaxktrue` is the default `kill = true`), where an
    /// unchanged configuration is the correct answer rather than a dropped flag.
    moves_config: bool,
}

/// The matrix: every `erlc`-shaped attached spelling against its separated twin.
const TWINS: &[Twin] = &[
    Twin {
        description: "`+S1` is `+S 1` (the scheduler count)",
        attached: &["+S1"],
        separated: &["+S", "1"],
        probe: Some(SCHEDULERS_PROBE),
        golden: Some("1"),
        oracle_agrees: true,
        moves_config: true,
    },
    Twin {
        description: "`+A0` is `+A 0` (the async-thread pool)",
        attached: &["+A0"],
        separated: &["+A", "0"],
        probe: Some(THREAD_POOL_PROBE),
        golden: Some("1"),
        oracle_agrees: true,
        moves_config: true,
    },
    Twin {
        description: "`+A5` is `+A 5` (a non-default pool makes the figure move)",
        attached: &["+A5"],
        separated: &["+A", "5"],
        probe: Some(THREAD_POOL_PROBE),
        golden: Some("5"),
        oracle_agrees: true,
        moves_config: true,
    },
    Twin {
        description: "`+P65536` is `+P 65536` (the process limit)",
        attached: &["+P65536"],
        separated: &["+P", "65536"],
        probe: Some(PROCESS_LIMIT_PROBE),
        golden: Some("65536"),
        oracle_agrees: true,
        moves_config: true,
    },
    Twin {
        description: "`+Q1024` is `+Q 1024` (the port-table request)",
        attached: &["+Q1024"],
        separated: &["+Q", "1024"],
        probe: Some(PORT_LIMIT_PROBE),
        golden: Some("1024"),
        oracle_agrees: true,
        moves_config: true,
    },
    Twin {
        description: "`+SP50` is `+SP 50` (the scheduler *pool* size)",
        attached: &["+SP50"],
        separated: &["+SP", "50"],
        probe: Some(SCHEDULERS_PROBE),
        // No golden: the figure follows this runtime's own scheduler policy, not the
        // oracle's (see the module docs).
        golden: None,
        oracle_agrees: false,
        moves_config: true,
    },
    Twin {
        description: "`+hms233` is `+hms 233` (the minimum heap size)",
        attached: &["+hms233"],
        separated: &["+hms", "233"],
        probe: Some(MIN_HEAP_PROBE),
        golden: Some("233"),
        oracle_agrees: true,
        moves_config: false,
    },
    Twin {
        description: "`+hmaxktrue` is `+hmaxk true` (the kill-on-overrun flag word)",
        attached: &["+hmaxktrue"],
        separated: &["+hmaxk", "true"],
        probe: None,
        golden: None,
        oracle_agrees: false,
        moves_config: false,
    },
    Twin {
        description: "`+sbwt1000` is `+sbwt 1000` (a numeric busy-wait threshold)",
        attached: &["+sbwt1000"],
        separated: &["+sbwt", "1000"],
        probe: None,
        golden: None,
        oracle_agrees: false,
        moves_config: true,
    },
    Twin {
        description: "`+sbwtlong` is `+sbwt long` (a symbolic busy-wait threshold)",
        attached: &["+sbwtlong"],
        separated: &["+sbwt", "long"],
        probe: None,
        golden: None,
        oracle_agrees: true,
        moves_config: true,
    },
    Twin {
        description: "`+sclfalse` is `+scl false` (scheduler-load compaction)",
        attached: &["+sclfalse"],
        separated: &["+scl", "false"],
        probe: None,
        golden: None,
        oracle_agrees: true,
        moves_config: true,
    },
    Twin {
        description: "`+sbtu` is `+sbt u` (the scheduler bind type)",
        attached: &["+sbtu"],
        separated: &["+sbt", "u"],
        probe: Some(BIND_TYPE_PROBE),
        golden: Some("unbound"),
        oracle_agrees: true,
        moves_config: true,
    },
];

/// Every row: parser equality, "the flag really applied", the two spellings' identical
/// driven output, and (where the runtimes can legitimately agree) the oracle cross-check.
#[test]
fn the_attached_and_separated_spellings_are_indistinguishable() {
    let oracle = support::require_oracle("cli_compact_flags");
    let baseline = config_of(&[]);
    let mut failures: Vec<String> = Vec::new();
    for row in TWINS {
        // (i) the parser: one `assert_eq!` over the whole `RuntimeConfig`.
        let attached = config_of(row.attached);
        let separated = config_of(row.separated);
        if attached != separated {
            failures.push(format!(
                "{}: RuntimeConfig differs ({:?} vs {:?})",
                row.description, attached, separated
            ));
        }
        // … and the flag really reached it (it is never silently dropped). A value that *is*
        // the emulator default legitimately leaves the configuration alone.
        if row.moves_config && attached == baseline {
            failures.push(format!(
                "{}: the compact spelling left the configuration at the baseline",
                row.description
            ));
        }
        if !row.moves_config && attached != baseline {
            failures.push(format!(
                "{}: a default-valued spelling changed the configuration",
                row.description
            ));
        }

        // (ii) the driven observable: both spellings, same line, exit 0, clean stderr.
        let Some(probe) = row.probe else {
            // No `system_info` figure for this switch: the executable still has to accept
            // both spellings (exit 0, no diagnostic).
            for (spelling, flags) in [("compact", row.attached), ("separated", row.separated)] {
                let ours = run(beam_rt(), flags, "halt(0).");
                if ours.code != 0 || !ours.err.is_empty() {
                    failures.push(format!(
                        "{}: the {spelling} spelling exited {:?} with stderr {:?}",
                        row.description, ours.code, ours.err
                    ));
                }
            }
            if row.oracle_agrees {
                if let Some(oracle) = oracle.as_deref() {
                    for flags in [row.attached, row.separated] {
                        let theirs = run(
                            oracle.to_str().expect("the oracle path is UTF-8"),
                            flags,
                            "halt(0).",
                        );
                        if theirs.code != 0 {
                            failures.push(format!(
                                "{}: the oracle rejected {:?} (stderr {:?})",
                                row.description, flags, theirs.err
                            ));
                        }
                    }
                }
            }
            continue;
        };
        let compact = run(beam_rt(), row.attached, probe);
        let spaced = run(beam_rt(), row.separated, probe);
        if compact.code != 0 || spaced.code != 0 {
            failures.push(format!(
                "{}: exit {:?}/{:?} (stderr {:?}/{:?})",
                row.description, compact.code, spaced.code, compact.err, spaced.err
            ));
            continue;
        }
        if !compact.err.is_empty() || !spaced.err.is_empty() {
            failures.push(format!(
                "{}: stderr {:?}/{:?}",
                row.description, compact.err, spaced.err
            ));
        }
        if compact.out != spaced.out {
            failures.push(format!(
                "{}: compact {:?} != separated {:?}",
                row.description, compact.out, spaced.out
            ));
        }
        if let Some(golden) = row.golden {
            if compact.out != golden {
                failures.push(format!(
                    "{}: answered {:?} != the pinned {:?}",
                    row.description, compact.out, golden
                ));
            }
        }
        // `schedulers_online` can never exceed `schedulers` — the invariant the `+SP` row
        // asserts *instead* of an oracle figure.
        if row.attached == ["+SP50"] {
            let online: u32 = compact
                .out
                .parse()
                .unwrap_or_else(|_| panic!("{} is not a scheduler count", compact.out));
            if online == 0 {
                failures.push(format!("{}: answered 0 schedulers", row.description));
            }
        }

        // (iii) the oracle, where the two runtimes can legitimately agree.
        if !row.oracle_agrees {
            continue;
        }
        let Some(oracle) = oracle.as_deref() else {
            continue;
        };
        let oracle_path = oracle.to_str().expect("the oracle path is UTF-8");
        let theirs_compact = run(oracle_path, row.attached, probe);
        let theirs_spaced = run(oracle_path, row.separated, probe);
        if theirs_compact.code != 0 || theirs_spaced.code != 0 {
            failures.push(format!(
                "{}: the oracle exited {:?}/{:?} (stderr {:?})",
                row.description, theirs_compact.code, theirs_spaced.code, theirs_compact.err
            ));
            continue;
        }
        if theirs_compact.out != theirs_spaced.out {
            failures.push(format!(
                "{}: the oracle itself treats the spellings differently ({:?} vs {:?})",
                row.description, theirs_compact.out, theirs_spaced.out
            ));
        }
        if theirs_compact.out != compact.out {
            failures.push(format!(
                "{}: beam_rt {:?} != oracle {:?}",
                row.description, compact.out, theirs_compact.out
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ---------------------------------------------------------------------------------------
// (c) the refusals — unchanged in both spellings
// ---------------------------------------------------------------------------------------

/// One refusal: the argv, and the exact one-line stderr this runtime prints for it.
struct Refusal {
    /// What the row pins.
    description: &'static str,
    /// The offending argv (after the program name).
    args: &'static [&'static str],
    /// The **exact**, untrimmed stderr — one line, `USAGE`-free.
    stderr: &'static str,
    /// Whether the oracle refuses the same invocation too. `false` where this runtime is
    /// **stricter** than the oracle (see the trailing-garbage row).
    oracle_refuses: bool,
}

/// The refusals the emulator's own reader must keep producing. The texts are the binary's
/// (read out of `src/cli.rs`/`src/error.rs` and confirmed against the built binary):
/// the launcher's `unknown flag -X`, the launcher's `Missing argument(s) for '+S'.`, and the
/// emulator's one-line `bad value "…" for switch +Q` (this runtime deliberately does not
/// reproduce `beam.smp`'s multi-kilobyte usage block, so the wording differs from the
/// oracle's `bad number of ports bogus` — the exit status does not).
///
/// The last row is the one **strictness** difference: the oracle's `+S`/`+P`/`+Q` readers are
/// digit-prefix readers, so `erl +S1bogus` is accepted (exit 0, one scheduler) while this
/// runtime rejects the same token — documented here so the divergence is visible rather than
/// assumed away; the oracle half of that row is therefore not asserted.
const REFUSALS: &[Refusal] = &[
    Refusal {
        description: "an unrecognized `+Xyz` is the launcher's `unknown flag -X`",
        args: &["+Xyz", "-noshell", "-eval", "halt(0)."],
        stderr: "beam_rt: unknown flag -Xyz\n",
        oracle_refuses: true,
    },
    Refusal {
        description: "`+S` with no value (attached or separated) is the launcher's diagnostic",
        args: &["+S"],
        stderr: "beam_rt: Missing argument(s) for '+S'.\n",
        oracle_refuses: true,
    },
    Refusal {
        description: "`+Q bogus` keeps the separated form's bad-value diagnostic",
        args: &["+Q", "bogus", "-noshell", "-eval", "halt(0)."],
        stderr: "beam_rt: bad value \"bogus\" for switch +Q\n",
        oracle_refuses: true,
    },
    Refusal {
        description: "`+Qbogus` reports the same diagnostic with the value parsed attached",
        args: &["+Qbogus", "-noshell", "-eval", "halt(0)."],
        stderr: "beam_rt: bad value \"bogus\" for switch +Q\n",
        oracle_refuses: true,
    },
    Refusal {
        description: "`+S1bogus` reports the attached value verbatim (stricter than the oracle)",
        args: &["+S1bogus", "-noshell", "-eval", "halt(0)."],
        stderr: "beam_rt: bad value \"1bogus\" for switch +S\n",
        oracle_refuses: false,
    },
];

/// Every refusal: exit **1**, exactly one stderr line, and **no** `USAGE` block — and, where
/// the oracle refuses too, the same exit status under the same argv (its wording is not the
/// shared contract, which is why only the status is cross-checked).
#[test]
fn the_refusals_are_unchanged_in_both_spellings() {
    let oracle = support::require_oracle("cli_compact_flags");
    let names = stripped_names();
    let env: Vec<(&str, Option<&str>)> = names
        .iter()
        .map(|name| (name.as_str(), None))
        .collect();
    let mut failures: Vec<String> = Vec::new();
    for refusal in REFUSALS {
        let Some(output) = support::run_bounded_with_env(Path::new(beam_rt()), refusal.args, &env)
        else {
            failures.push(format!(
                "{}: the child outlived the probe bound",
                refusal.description
            ));
            continue;
        };
        if output.status.code() != Some(1) {
            failures.push(format!(
                "{}: exit {:?} != 1",
                refusal.description,
                output.status.code()
            ));
        }
        let stderr = raw_stderr(&output);
        if stderr != refusal.stderr {
            failures.push(format!(
                "{}: stderr {:?} != {:?}",
                refusal.description, stderr, refusal.stderr
            ));
        }
        if stderr.contains("Usage:") {
            failures.push(format!("{}: a USAGE block was printed", refusal.description));
        }
        if !output.stdout.is_empty() {
            failures.push(format!(
                "{}: stdout is not empty: {:?}",
                refusal.description,
                String::from_utf8_lossy(&output.stdout)
            ));
        }
        // The oracle refuses the same invocation (exit 1) — the shared half of a refusal.
        if !refusal.oracle_refuses {
            println!(
                "note cli_compact_flags: the oracle half of {:?} is not asserted — this \
                 runtime is stricter than the digit-prefix readers (see REFUSALS)",
                refusal.description
            );
            continue;
        }
        if let Some(oracle) = oracle.as_deref() {
            match support::run_bounded_with_env(oracle, refusal.args, &env) {
                Some(theirs) if theirs.status.code() == Some(1) => {}
                Some(theirs) => failures.push(format!(
                    "{}: the oracle exited {:?} != 1",
                    refusal.description,
                    theirs.status.code()
                )),
                None => failures.push(format!(
                    "{}: the oracle outlived the probe bound",
                    refusal.description
                )),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ---------------------------------------------------------------------------------------
// (d) a `+` switch is never an init argument
// ---------------------------------------------------------------------------------------

/// The `+` switches the matrix above drives: none of them may appear in `init:get_arguments/0`
/// — in the oracle *every* `+`-flag is an emulator flag, so `init:get_argument('S'|'P'|…)` is
/// the bare atom `error` and `init:get_arguments/0` lists no entry for it.
const PLUS_SWITCHES: &[&str] = &[
    "+S1", "+sbtu", "+A0", "+P", "65536", "+Q", "1024", "+hms233", "+sclfalse", "+sbwtlong",
];

/// `lists:keymember('S',1,init:get_arguments())` is `false`, and the **whole** argument list is
/// unchanged by the `+` switches — the same list with and without them, on both runtimes.
#[test]
fn no_plus_switch_is_an_init_argument() {
    // The membership probe, on the exact group.
    let ours = run(beam_rt(), PLUS_SWITCHES, INIT_ARGUMENTS_PROBE);
    assert_eq!(ours.code, 0, "stderr: {:?}", ours.err);
    assert_eq!(ours.out, "false");

    // … and the whole list: `+` switches add nothing to it.
    let baseline = run(beam_rt(), &[], ALL_ARGUMENTS_PROBE);
    let with_switches = run(beam_rt(), PLUS_SWITCHES, ALL_ARGUMENTS_PROBE);
    assert_eq!(baseline.code, 0, "stderr: {:?}", baseline.err);
    assert_eq!(with_switches.code, 0, "stderr: {:?}", with_switches.err);
    assert_eq!(
        with_switches.out, baseline.out,
        "the `+` switches changed init:get_arguments/0"
    );
    // The known automatic entries are still there (so the equality above is not comparing two
    // empty lists).
    for key in ["root", "bindir", "progname", "noshell"] {
        assert!(
            with_switches.out.contains(key),
            "init:get_arguments/0 lost {key}: {}",
            with_switches.out
        );
    }

    let Some(oracle) = support::require_oracle("cli_compact_flags") else {
        return;
    };
    // The oracle's membership answer through the shared bounded evaluator …
    let theirs = support::oracle_eval(&oracle, None, INIT_ARGUMENTS_PROBE)
        .expect("the live oracle must answer the init-argument probe");
    assert_eq!(theirs, ours.out);
    // … and its whole list, driven through the same argv as this runtime's.
    let oracle_path = oracle.to_str().expect("the oracle path is UTF-8");
    let theirs_all = run(oracle_path, PLUS_SWITCHES, ALL_ARGUMENTS_PROBE);
    assert_eq!(theirs_all.code, 0, "the oracle's stderr: {:?}", theirs_all.err);
    assert_eq!(theirs_all.out, with_switches.out);
}
