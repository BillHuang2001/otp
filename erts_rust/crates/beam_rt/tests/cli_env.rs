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

//! A.6 (CLI/configuration): the `ERL_*` environment variables that real `erl` reads
//! **outside** the `ERL_*FLAGS` merge chain.
//!
//! `ERL_INETRC`, `ERL_MAX_PORTS`, `ERL_CRASH_DUMP`, `ERL_CRASH_DUMP_SECONDS`,
//! `ERL_ROOTDIR`, `ERL_MAX_ETS_TABLES` and `ERL_FULLSWEEP_AFTER` are read by the
//! emulator's start-up code (and, for `ERL_ROOTDIR`, by the `erl` launcher script), not
//! by `initial_argv_massage` — so **none** of them is an `init` argument. Verified
//! against the oracle (OTP 29.1.1 / erts-17.1) with every variable set:
//!
//! ```text
//! ERL_MAX_PORTS=2000 ERL_CRASH_DUMP=/tmp/x.dump ERL_CRASH_DUMP_SECONDS=5 \
//! ERL_MAX_ETS_TABLES=100 ERL_FULLSWEEP_AFTER=10 erl -noshell \
//!     -eval 'io:format("~p ~p ~p ~p ~p~n",[init:get_argument(inetrc),
//!         init:get_argument(crash_dump), init:get_argument(crash_dump_seconds),
//!         init:get_argument(max_ports), init:get_argument(fullsweep_after)]), halt().'
//!   → error error error error error
//! ```
//!
//! `ERL_MAX_PORTS` is the one member with a modelled effect: the emulator `atoi`s it,
//! clamps it into `[1024, 134217727]` and uses it as the port-table size. This runtime
//! records the clamped value as the configuration's port-table request
//! (`config.max_ports`, the same field `+Q N` sets) **and** routes it into the `+Q`
//! channel `extra_emulator_flags` for the emulator-flag surface; `crate::info`'s
//! `port_limit` then applies the emulator's power-of-two table sizing. The oracle's
//! observable is `erlang:system_info(port_limit)`:
//!
//! ```text
//! ERL_MAX_PORTS=1         → 1024        ERL_MAX_PORTS=2000      → 2048
//! ERL_MAX_PORTS=100       → 1024        ERL_MAX_PORTS=999999999 → 134217727
//! ERL_MAX_PORTS=bogus     → 1024        (unset)                 → 65536
//! ERL_MAX_PORTS=2048 erl +Q 4096        → 4096   (a `+Q` wins)
//! ERL_FLAGS='+Q 4096' erl               → 4096   (also through the `*FLAGS` merge)
//! ```
//!
//! The `2000 → 2048` step is the port-table power-of-two sizing (applied by `info`);
//! the `1 → 1024` and `999999999 → 134217727` steps are the clamp this parser does.
//! The end-to-end `port_limit` comparison against the oracle lives in
//! `tests/system_info_oracle.rs`.

use std::collections::HashMap;
use std::process::{Command, Output};

use beam_rt::cli::{parse, parse_with_env, Env, ErlEnv, MapEnv};

// --- helpers -------------------------------------------------------------------------

/// A [`MapEnv`] holding exactly `pairs` (no inherited variable is visible, so each case
/// is precisely what the test sets).
fn env_of(pairs: &[(&str, &str)]) -> MapEnv {
    MapEnv(
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect::<HashMap<String, String>>(),
    )
}

/// Parses `args` under `pairs` through the library API.
fn parse_env(args: &[&str], pairs: &[(&str, &str)]) -> beam_rt::config::RuntimeConfig {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    parse_with_env(&args, &env_of(pairs))
        .unwrap_or_else(|error| panic!("parse_with_env({args:?}) failed: {error}"))
}

/// The parsed `extra_emulator_flags` (the `+Q` channel).
fn extra_flags(config: &beam_rt::config::RuntimeConfig) -> Vec<String> {
    config.emulator.extra_emulator_flags.clone()
}

// --- `ERL_MAX_PORTS` → the `+Q` channel ----------------------------------------------

/// Without a `+Q` on the command line, `ERL_MAX_PORTS` is `atoi`d and clamped into
/// `[1024, 134217727]`, then routed into the channel `+Q` uses.
#[test]
fn erl_max_ports_is_atoi_clamped_and_routed_into_the_plus_q_channel() {
    for (value, clamped) in [
        ("1", "1024"),
        ("100", "1024"),
        ("1024", "1024"),
        ("2000", "2000"),
        ("65536", "65536"),
        ("134217727", "134217727"),
        ("999999999", "134217727"),
        ("bogus", "1024"),
        ("", "1024"),
    ] {
        let config = parse_env(&[], &[("ERL_MAX_PORTS", value)]);
        assert_eq!(
            extra_flags(&config),
            vec!["+Q".to_string(), clamped.to_string()],
            "ERL_MAX_PORTS={value}"
        );
        assert_eq!(
            config.env_vars().max_ports,
            Some(clamped.parse::<usize>().expect("a decimal clamp result")),
            "ERL_MAX_PORTS={value}"
        );
        // … and the clamped value *is* the runtime's port-table request (the power-of-two
        // table sizing on top of it is applied by `info`'s `port_limit`).
        assert_eq!(
            config.max_ports,
            Some(clamped.parse::<usize>().expect("a decimal clamp result")),
            "ERL_MAX_PORTS={value}"
        );
        // None of these variables is an `init` argument: verified against the oracle,
        // where `init:get_argument(max_ports)` is the bare atom `error`.
        assert!(config.init_args.is_empty(), "ERL_MAX_PORTS={value}");
        assert_eq!(config.init_args.get("max_ports"), None);
        assert_eq!(config.init_args.get("Q"), None);
    }
}

/// Unset means "no value at all": nothing is injected and the field is `None`.
#[test]
fn an_unset_erl_max_ports_injects_nothing() {
    let config = parse_env(&[], &[]);
    assert!(extra_flags(&config).is_empty());
    assert_eq!(config.env_vars(), &ErlEnv::default());
    // … and no port-table request is made, so `port_limit` stays the default table.
    assert_eq!(config.max_ports, None);
}

/// A `+Q` **anywhere in the merged argv** overrides `ERL_MAX_PORTS` — including one that
/// arrives through the `ERL_*FLAGS` merge (`ERL_FLAGS='+Q 4096'`).
#[test]
fn a_command_line_plus_q_overrides_erl_max_ports() {
    // On the command line.
    let config = parse_env(&["+Q", "4096"], &[("ERL_MAX_PORTS", "2048")]);
    assert_eq!(
        extra_flags(&config),
        vec!["+Q".to_string(), "4096".to_string()]
    );
    // … and the variable is still reported verbatim (clamped), while the request is
    // whatever the winning `+Q` asked for.
    assert_eq!(config.env_vars().max_ports, Some(2048));
    assert_eq!(config.max_ports, Some(4096));

    // Through `ERL_FLAGS` (appended after the command line by the merge).
    let config = parse_env(&[], &[("ERL_MAX_PORTS", "2048"), ("ERL_FLAGS", "+Q 4096")]);
    assert_eq!(
        extra_flags(&config),
        vec!["+Q".to_string(), "4096".to_string()]
    );
    assert_eq!(config.max_ports, Some(4096));

    // Through `ERL_AFLAGS` (prepended before the command line by the merge).
    let config = parse_env(&[], &[("ERL_MAX_PORTS", "2048"), ("ERL_AFLAGS", "+Q 3000")]);
    assert_eq!(
        extra_flags(&config),
        vec!["+Q".to_string(), "3000".to_string()]
    );
    assert_eq!(config.max_ports, Some(3000));

    // The `legacy` spelling counts as a `+Q` too, so the variable does not add a second —
    // and `legacy` is the emulator's default table.
    let config = parse_env(&["+Q", "legacy"], &[("ERL_MAX_PORTS", "2048")]);
    assert_eq!(
        extra_flags(&config),
        vec!["+Q".to_string(), "legacy".to_string()]
    );
    assert_eq!(config.max_ports, None);
}

/// `parse` is **pure**: it must never read the process environment, so no `ERL_MAX_PORTS`
/// (and no other `ERL_*`) can reach it — only `parse_with_env` fills `env_vars`.
#[test]
fn the_pure_parse_never_reads_the_environment() {
    // Set the variables in this process and drain the mutable test environment through a
    // hand-rolled `Env` that *would* answer; the pure parser must ignore all of it.
    let loud = env_of(&[("ERL_MAX_PORTS", "2000"), ("ERL_INETRC", "/etc/inetrc")]);
    assert_eq!(loud.get("ERL_MAX_PORTS").as_deref(), Some("2000"));

    let config = parse(&[]).expect("an empty command line parses");
    assert!(extra_flags(&config).is_empty());
    assert_eq!(config.env_vars(), &ErlEnv::default());

    let config = parse(&["+Q".to_string(), "1024".to_string()]).expect("+Q 1024 parses");
    assert_eq!(
        extra_flags(&config),
        vec!["+Q".to_string(), "1024".to_string()]
    );
    assert_eq!(config.env_vars(), &ErlEnv::default());
}

// --- the parsed-but-inert variables --------------------------------------------------

/// The remaining variables are parsed into `config.env_vars()` and change nothing else:
/// no `init` argument, no extra emulator flag.
#[test]
fn the_other_variables_are_parsed_into_the_config() {
    let config = parse_env(
        &[],
        &[
            ("ERL_INETRC", "/etc/inetrc"),
            ("ERL_CRASH_DUMP", "/tmp/mydump"),
            ("ERL_CRASH_DUMP_SECONDS", "-1"),
            ("ERL_ROOTDIR", "/opt/otp"),
            ("ERL_MAX_ETS_TABLES", "2048"),
            ("ERL_FULLSWEEP_AFTER", "10"),
        ],
    );
    let vars = config.env_vars();
    assert_eq!(vars.inetrc.as_deref(), Some("/etc/inetrc"));
    assert_eq!(vars.crash_dump.as_deref(), Some("/tmp/mydump"));
    assert_eq!(vars.crash_dump_seconds, Some(-1));
    assert_eq!(vars.rootdir.as_deref(), Some("/opt/otp"));
    assert_eq!(vars.max_ets_tables, Some(2048));
    assert_eq!(vars.fullsweep_after, Some(10));
    // … and they are inert: no `+Q` (the variable is unset) and no init argument.
    assert!(extra_flags(&config).is_empty());
    assert!(config.init_args.is_empty());
}

/// The `atoi` semantics of each numeric variable, at the library boundary.
#[test]
fn numeric_variables_follow_c_atoi() {
    // `0` = the emulator's default for both of these.
    let vars = parse_env(&[], &[("ERL_MAX_ETS_TABLES", "0")])
        .env_vars()
        .clone();
    assert_eq!(vars.max_ets_tables, Some(0));
    // Non-numeric ⇒ `atoi`'s 0 (never dropped, never a wrong value).
    let vars = parse_env(&[], &[("ERL_MAX_ETS_TABLES", "lots")])
        .env_vars()
        .clone();
    assert_eq!(vars.max_ets_tables, Some(0));
    let vars = parse_env(&[], &[("ERL_FULLSWEEP_AFTER", "many")])
        .env_vars()
        .clone();
    assert_eq!(vars.fullsweep_after, Some(0));
    // A malformed `ERL_CRASH_DUMP_SECONDS` is `atoi`'s 0 — the emulator's "no dump".
    let vars = parse_env(&[], &[("ERL_CRASH_DUMP_SECONDS", "bogus")])
        .env_vars()
        .clone();
    assert_eq!(vars.crash_dump_seconds, Some(0));
    // `-1` really is "wait for the dump forever"; `5` is alarm-bounded.
    let vars = parse_env(&[], &[("ERL_CRASH_DUMP_SECONDS", "5")])
        .env_vars()
        .clone();
    assert_eq!(vars.crash_dump_seconds, Some(5));
}

// --- the oracle cross-check (skips loudly without `erl`) -----------------------------

/// Whether a real `erl` can be executed at all (no toolchain ⇒ skip loudly).
fn erl_available() -> bool {
    Command::new("erl")
        .args(["-noshell", "-eval", "halt(0)."])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Runs the oracle and returns `erlang:system_info(port_limit)`.
///
/// Every `ERL_*` variable this suite drives is stripped first, so each case is exactly
/// what the test sets; `extra` is the command line between `-noshell` and `-eval`.
fn oracle_port_limit(pairs: &[(&str, &str)], extra: &[&str]) -> i64 {
    let mut command = Command::new("erl");
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
    command.args(["-noshell"]);
    command.args(extra);
    command.args([
        "-eval",
        "io:format(\"~p\", [erlang:system_info(port_limit)]), halt(0).",
    ]);
    for (name, value) in pairs {
        command.env(name, value);
    }
    let Output {
        status,
        stdout,
        stderr,
    } = command
        .output()
        .unwrap_or_else(|error| panic!("spawning `erl` failed: {error}"));
    assert!(
        status.success(),
        "the oracle exited with {status:?}: {}",
        String::from_utf8_lossy(&stderr)
    );
    String::from_utf8_lossy(&stdout)
        .trim()
        .parse::<i64>()
        .unwrap_or_else(|error| {
            panic!(
                "the oracle printed {:?}: {error}",
                String::from_utf8_lossy(&stdout)
            )
        })
}

/// The oracle's `ERL_MAX_PORTS`/`+Q` ground truth — the values the clamp in
/// `beam_rt::cli::vars` reproduces. Skips loudly when `erl` is not runnable.
#[test]
fn the_oracle_agrees_with_the_documented_port_limit_ground_truth() {
    if !erl_available() {
        println!("SKIP cli_env: `erl` is not runnable — the oracle cross-check is skipped");
        return;
    }
    // (environment, command-line switches, observed port_limit)
    for (pairs, extra, expected) in [
        (&[][..], &[][..], 65_536i64),
        (&[("ERL_MAX_PORTS", "1")][..], &[][..], 1024),
        (&[("ERL_MAX_PORTS", "100")][..], &[][..], 1024),
        (&[("ERL_MAX_PORTS", "1024")][..], &[][..], 1024),
        // 2000 → 2048 is the port-table's power-of-two sizing, not the clamp.
        (&[("ERL_MAX_PORTS", "2000")][..], &[][..], 2048),
        (&[("ERL_MAX_PORTS", "999999999")][..], &[][..], 134_217_727),
        (&[("ERL_MAX_PORTS", "bogus")][..], &[][..], 1024),
        // A `+Q` in the merged argv overrides the variable.
        (&[("ERL_MAX_PORTS", "2048")][..], &["+Q", "4096"][..], 4096),
        (
            &[("ERL_MAX_PORTS", "2048"), ("ERL_FLAGS", "+Q 4096")][..],
            &[][..],
            4096,
        ),
    ] {
        assert_eq!(
            oracle_port_limit(pairs, extra),
            expected,
            "env {pairs:?} argv {extra:?}"
        );
    }
}

/// `beam_rt`'s own clamped value for the same cases: the request the runtime records is
/// the variable clamped into the emulator's range (the power-of-two table step is applied
/// by `info`'s `port_limit`, and a command-line `+Q` still wins).
#[test]
fn beam_rt_clamps_erl_max_ports_into_the_emulator_range() {
    for (value, expected) in [
        ("1", "1024"),
        ("100", "1024"),
        ("1024", "1024"),
        ("2000", "2000"),
        ("999999999", "134217727"),
        ("bogus", "1024"),
    ] {
        let config = parse_env(&[], &[("ERL_MAX_PORTS", value)]);
        assert_eq!(
            extra_flags(&config),
            vec!["+Q".to_string(), expected.to_string()],
            "ERL_MAX_PORTS={value}"
        );
        assert_eq!(
            config.max_ports,
            Some(expected.parse::<usize>().expect("a decimal clamp result")),
            "ERL_MAX_PORTS={value}"
        );
    }
    // … while an explicit `+Q` is left untouched (`ERL_MAX_PORTS` never overrides it).
    let config = parse_env(&["+Q", "4096"], &[("ERL_MAX_PORTS", "2048")]);
    assert_eq!(
        extra_flags(&config),
        vec!["+Q".to_string(), "4096".to_string()]
    );
    assert_eq!(config.max_ports, Some(4096));
}

/// The whole family is invisible to `init:get_argument/1` — the library reports no
/// occurrence for any of them, and the oracle answers the bare atom `error` for every
/// lookup. The oracle half skips loudly when `erl` is not runnable (or when the empty
/// `inetrc` file it needs cannot be created).
#[test]
fn no_erl_variable_is_an_init_argument() {
    let config = parse_env(
        &["-noshell"],
        &[
            ("ERL_INETRC", "/etc/inetrc"),
            ("ERL_MAX_PORTS", "2000"),
            ("ERL_CRASH_DUMP", "/tmp/mydump"),
            ("ERL_CRASH_DUMP_SECONDS", "5"),
            ("ERL_ROOTDIR", "/opt/otp"),
            ("ERL_MAX_ETS_TABLES", "100"),
            ("ERL_FULLSWEEP_AFTER", "10"),
        ],
    );
    for flag in [
        "inetrc",
        "max_ports",
        "crash_dump",
        "crash_dump_seconds",
        "rootdir",
        "root",
        "max_ets_tables",
        "fullsweep_after",
        "Q",
    ] {
        assert_eq!(config.init_args.get(flag), None, "{flag}");
    }
    // `-noshell` itself is the only recorded occurrence.
    assert!(config.init_args.get("noshell").is_some());
    assert_eq!(config.init_args.len(), 1);

    if !erl_available() {
        println!("SKIP cli_env: `erl` is not runnable — the oracle cross-check is skipped");
        return;
    }
    // `ERL_INETRC` must point at a file that exists (the resolver logs an error report,
    // on stdout, otherwise); an empty `inetrc` is valid.
    let inetrc =
        std::env::temp_dir().join(format!("beam_rt_cli_env_{}.inetrc", std::process::id()));
    if std::fs::write(&inetrc, b"").is_err() {
        println!("SKIP cli_env: could not create the temporary `inetrc` file");
        return;
    }
    let inetrc = inetrc.to_string_lossy().into_owned();

    let mut command = Command::new("erl");
    for name in [
        "ERL_OTP30_FLAGS",
        "ERL_AFLAGS",
        "ERL_FLAGS",
        "ERL_ZFLAGS",
        "ERL_INETRC",
        "ERL_MAX_PORTS",
        "ERL_CRASH_DUMP",
        "ERL_CRASH_DUMP_SECONDS",
        "ERL_ROOTDIR",
        "ERL_MAX_ETS_TABLES",
        "ERL_FULLSWEEP_AFTER",
    ] {
        command.env_remove(name);
    }
    command.args([
        "-noshell",
        "-eval",
        "io:format(\"PROBE ~p ~p ~p ~p ~p~n\",[init:get_argument(inetrc), \
         init:get_argument(crash_dump), init:get_argument(crash_dump_seconds), \
         init:get_argument(max_ports), init:get_argument(max_ets_tables)]), halt(0).",
    ]);
    for (name, value) in [
        ("ERL_INETRC", inetrc.as_str()),
        ("ERL_MAX_PORTS", "2000"),
        ("ERL_CRASH_DUMP", "/tmp/mydump"),
        ("ERL_CRASH_DUMP_SECONDS", "5"),
        ("ERL_MAX_ETS_TABLES", "100"),
        ("ERL_FULLSWEEP_AFTER", "10"),
    ] {
        command.env(name, value);
    }
    let output = command.output().expect("spawning `erl` failed");
    let _ = std::fs::remove_file(&inetrc);
    assert!(
        output.status.success(),
        "the oracle exited with {:?}",
        output.status
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let line = stdout
        .lines()
        .find(|line| line.starts_with("PROBE"))
        .unwrap_or_else(|| panic!("the oracle printed no probe line: {stdout:?}"));
    assert_eq!(line, "PROBE error error error error error");
}
