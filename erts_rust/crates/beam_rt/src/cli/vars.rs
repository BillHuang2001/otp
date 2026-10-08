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

//! The `ERL_*` environment variables **beyond** the `ERL_*FLAGS` merge chain.
//!
//! Real `erl` reads a second family of environment variables directly in the emulator's
//! start-up code rather than through `initial_argv_massage` (which is [`super::env`]'s
//! concern):
//!
//! | Variable | Read by |
//! |---|---|
//! | `ERL_MAX_ETS_TABLES` | `erts/emulator/beam/erl_init.c:1328-1331` (`erl_db.h:101`) |
//! | `ERL_FULLSWEEP_AFTER` | `erts/emulator/beam/erl_init.c:1334-1338` |
//! | `ERL_MAX_PORTS` | `erts/emulator/beam/erl_init.c:1341-1344` |
//! | `ERL_CRASH_DUMP` / `ERL_CRASH_DUMP_SECONDS` | `erts/emulator/beam/erl_crash_dump.c:427-457` |
//! | `ERL_INETRC` | `lib/kernel/src/inet_config.erl:436` |
//! | `ERL_ROOTDIR` | the `erl` **launcher script** (`erts/etc/unix/erl.src`), which relocates the OTP root |
//!
//! **None of them is an `init` argument.** Verified against the oracle (OTP 29.1.1 /
//! erts-17.1) with every variable set:
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
//! So — unlike the `ERL_*FLAGS` chain — nothing here may ever be recorded as an
//! [`InitArgs`](crate::config::InitArgs) occurrence.
//!
//! # Scope in Milestone 1
//!
//! This runtime has no crash-dump subsystem, no `inet` resolver and no launcher
//! relocation, so every variable below is **parsed, validated and stored** (observable
//! through [`RuntimeConfig::env_vars`](crate::config::RuntimeConfig::env_vars)) but
//! **inert**: it changes no behaviour, and it is never silently dropped and never
//! replaced by a wrong value. The one exception is `ERL_MAX_PORTS`, whose effect is
//! routed into the request channel the `+Q` switch uses (see [`has_plus_q_switch`] and
//! [`crate::cli::parse_with_env`]): the clamped value becomes the configuration's
//! port-table request, and the power-of-two **table sizing** on top of it is applied by
//! [`crate::info`]'s `port_limit`.

use super::flags::{ERTS_MAX_PORTS, ERTS_MIN_PORTS};
use super::Env;

/// The `ERL_MAX_PORTS` variable name (`erl_init.c:1341`).
const MAX_PORTS_VAR: &str = "ERL_MAX_PORTS";
/// The `ERL_MAX_ETS_TABLES` variable name (`erl_db.h:101`).
const MAX_ETS_TABLES_VAR: &str = "ERL_MAX_ETS_TABLES";
/// The `ERL_FULLSWEEP_AFTER` variable name (`erl_init.c:1334`).
const FULLSWEEP_AFTER_VAR: &str = "ERL_FULLSWEEP_AFTER";
/// The `ERL_CRASH_DUMP` variable name (`erl_crash_dump.c`).
const CRASH_DUMP_VAR: &str = "ERL_CRASH_DUMP";
/// The `ERL_CRASH_DUMP_SECONDS` variable name (`erl_crash_dump.c:451`).
const CRASH_DUMP_SECONDS_VAR: &str = "ERL_CRASH_DUMP_SECONDS";
/// The `ERL_INETRC` variable name (`inet_config.erl:436`).
const INETRC_VAR: &str = "ERL_INETRC";
/// The `ERL_ROOTDIR` variable name (the `erl` launcher script).
const ROOTDIR_VAR: &str = "ERL_ROOTDIR";

/// The `ERL_*` environment variables the emulator (and the `erl` launcher) read
/// **outside** the `ERL_*FLAGS` merge chain.
///
/// Every field is `None` when its variable is unset, and the numeric fields hold exactly
/// what the C's `atoi` would have returned (see [`c_atoi`]) — `ERL_MAX_PORTS` additionally
/// **clamped** into `[ERTS_MIN_PORTS, ERTS_MAX_PORTS]`, because that is what the emulator
/// does with the value it reads. An **empty** but set variable is `Some("")`/`Some(0)`,
/// not `None`: the C distinguishes "unset" from "set to the empty string".
///
/// Nothing here is an `init` argument (module docs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErlEnv {
    /// `ERL_INETRC` — the inet-configuration file path the `inet_config` module loads
    /// (`inet_config.erl:436`). **Inert in M1** (no resolver).
    pub inetrc: Option<String>,
    /// `ERL_MAX_PORTS` — the requested port-table size, `atoi`d and **clamped** into
    /// `[1024, 134217727]` (`erl_init.c:1341-1344`).
    ///
    /// The clamp is all this models. The emulator additionally rounds the table to a power
    /// of two (so `ERL_MAX_PORTS=2000` observes a `port_limit` of 2048) — that is
    /// port-table sizing, an emulator concern applied where the value becomes observable
    /// ([`crate::info`]'s `port_limit`), not a property of the variable.
    pub max_ports: Option<usize>,
    /// `ERL_CRASH_DUMP` — the crash-dump **file name** (the emulator's default is
    /// `erl_crash.dump`). **Inert in M1** (no crash-dump subsystem).
    pub crash_dump: Option<String>,
    /// `ERL_CRASH_DUMP_SECONDS` — the crash-dump time budget, `atoi`d
    /// (`erl_crash_dump.c:451-457`): `0` = write no dump and break immediately, `< 0` =
    /// write the dump until it is complete (no alarm), `> 0` = bound the dump by that many
    /// seconds. A set-but-unparsable value is `atoi`'s `0` (no dump). **Inert in M1**.
    pub crash_dump_seconds: Option<i64>,
    /// `ERL_ROOTDIR` — the OTP-root relocation the `erl` **launcher script** honours.
    /// **Inert in M1** (this binary is not relocatable).
    pub rootdir: Option<String>,
    /// `ERL_MAX_ETS_TABLES` — the maximum number of ETS tables, `atoi`d
    /// (`erl_init.c:1328-1331`); `0` (the emulator default) means "unlimited". **Inert in
    /// M1** (the ETS node here has no table limit).
    pub max_ets_tables: Option<i64>,
    /// `ERL_FULLSWEEP_AFTER` — the generational-GC full-sweep interval, `atoi`d
    /// (`erl_init.c:1334-1338`); the emulator stores it in a `Uint16`. **Inert in M1**
    /// (there is no gen-GC yet) and stored **untruncated** — the value this runtime keeps
    /// is the raw `atoi` result, and no consumer exists to be given a truncated one.
    pub fullsweep_after: Option<i64>,
}

/// Reads every variable of [`ErlEnv`] from `env`.
///
/// This is the only entry point the CLI uses ([`crate::cli::parse_with_env`]); each
/// variable is parsed by its own helper so it can be unit-tested in isolation.
///
/// ```
/// use beam_rt::cli::{from_env, MapEnv};
/// use std::collections::HashMap;
///
/// let env = MapEnv(HashMap::from([(
///     "ERL_MAX_PORTS".to_string(),
///     "2000".to_string(),
/// )]));
/// assert_eq!(from_env(&env).max_ports, Some(2000));
/// ```
pub fn from_env(env: &dyn Env) -> ErlEnv {
    ErlEnv {
        inetrc: env.get(INETRC_VAR),
        max_ports: parse_max_ports(env),
        crash_dump: env.get(CRASH_DUMP_VAR),
        crash_dump_seconds: parse_atoi_var(env, CRASH_DUMP_SECONDS_VAR),
        rootdir: env.get(ROOTDIR_VAR),
        max_ets_tables: parse_atoi_var(env, MAX_ETS_TABLES_VAR),
        fullsweep_after: parse_atoi_var(env, FULLSWEEP_AFTER_VAR),
    }
}

/// `ERL_MAX_PORTS`, `atoi`d and clamped into `[ERTS_MIN_PORTS, ERTS_MAX_PORTS]`.
///
/// The emulator clamps silently (unlike the `+Q` switch, which *rejects* an out-of-range
/// value): verified against the oracle (OTP 29.1.1 / erts-17.1), where
/// `ERL_MAX_PORTS` of `1`/`100` observes a `port_limit` of 1024, `2000` observes 2048,
/// `999999999` observes 134217727 and a non-numeric value (`bogus`) observes the 1024
/// floor — while unset observes the 65536 default.
///
/// The returned value is the **clamped request**; the `2000 ⇒ 2048` step is the
/// port-table power-of-two sizing, which [`crate::info`]'s `port_limit` applies to every
/// request regardless of whether it came from here or from `+Q`.
fn parse_max_ports(env: &dyn Env) -> Option<usize> {
    parse_atoi_var(env, MAX_PORTS_VAR)
        .map(|value| value.clamp(ERTS_MIN_PORTS, ERTS_MAX_PORTS) as usize)
}

/// `env.get(name)`, `atoi`d — `None` when the variable is unset.
fn parse_atoi_var(env: &dyn Env, name: &str) -> Option<i64> {
    env.get(name).map(|value| c_atoi(&value))
}

/// C `atoi`: the leading decimal integer of `value`, or `0` when there is none.
///
/// `atoi(v)` is `(int)strtol(v, NULL, 10)`, so the rule is: skip leading ASCII
/// whitespace, take an optional `+`/`-`, then as many decimal digits as follow and stop at
/// the first non-digit. A value with **no** digits at all (`""`, `"bogus"`, `"x7"`) is `0`
/// — which is why a malformed `ERL_MAX_PORTS` silently becomes the 1024 floor and a
/// malformed `ERL_CRASH_DUMP_SECONDS` means "write no dump".
///
/// Out-of-range magnitudes **saturate** at `i64::MAX` instead of wrapping: the C leaves
/// that case undefined, and saturating keeps the clamp monotone. A negative sign is then
/// applied with a saturating negation, so the most negative result is `-i64::MAX`.
fn c_atoi(value: &str) -> i64 {
    let bytes = value.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    let negative = match bytes.get(index) {
        Some(b'-') => {
            index += 1;
            true
        }
        Some(b'+') => {
            index += 1;
            false
        }
        _ => false,
    };
    let digits_start = index;
    let mut magnitude: i64 = 0;
    while let Some(digit) = bytes.get(index).filter(|byte| byte.is_ascii_digit()) {
        magnitude = magnitude
            .saturating_mul(10)
            .saturating_add(i64::from(digit - b'0'));
        index += 1;
    }
    if index == digits_start {
        // No digits: `strtol` reports 0 (the `endptr` never left the start).
        return 0;
    }
    if negative {
        magnitude.saturating_neg()
    } else {
        magnitude
    }
}

/// Whether the **merged** argv contains the `+Q` emulator switch — the one thing that
/// overrides `ERL_MAX_PORTS`.
///
/// Verified against the oracle (OTP 29.1.1 / erts-17.1): `ERL_MAX_PORTS=2048 erl +Q 4096`
/// observes `port_limit` 4096, and so does `ERL_FLAGS='+Q 4096' erl`, so the override is
/// decided on the argv **after** the `ERL_*FLAGS` merge. A `+Q` is recognized by token
/// equality, exactly as [`crate::cli::parse`] recognizes it (an inline `+Q4096` is an
/// unmodeled `+`-flag there, so it is not a `+Q` here either).
pub(super) fn has_plus_q_switch(merged: &[String]) -> bool {
    merged.iter().any(|token| token == "+Q")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// A [`MapEnv`](super::super::MapEnv) from `(name, value)` pairs.
    fn env(pairs: &[(&str, &str)]) -> super::super::MapEnv {
        super::super::MapEnv(
            pairs
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect::<HashMap<String, String>>(),
        )
    }

    #[test]
    fn an_unset_environment_yields_every_field_none() {
        assert_eq!(from_env(&env(&[])), ErlEnv::default());
    }

    #[test]
    fn string_variables_are_stored_verbatim() {
        let vars = from_env(&env(&[
            ("ERL_INETRC", "/etc/inetrc"),
            ("ERL_CRASH_DUMP", "/tmp/mydump"),
            ("ERL_ROOTDIR", "/opt/otp"),
        ]));
        assert_eq!(vars.inetrc.as_deref(), Some("/etc/inetrc"));
        assert_eq!(vars.crash_dump.as_deref(), Some("/tmp/mydump"));
        assert_eq!(vars.rootdir.as_deref(), Some("/opt/otp"));
        // A set-but-empty variable is `Some("")`, not `None` (the C distinguishes them).
        let empty = from_env(&env(&[("ERL_INETRC", "")]));
        assert_eq!(empty.inetrc.as_deref(), Some(""));
    }

    /// The `ERL_MAX_PORTS` clamp, pinned to the oracle's observed `port_limit` for each
    /// value (the port-table power-of-two rounding is applied downstream, by
    /// [`crate::info`]'s `port_limit`):
    ///
    /// ```text
    /// ERL_MAX_PORTS=1         erl … system_info(port_limit) → 1024
    /// ERL_MAX_PORTS=100       erl … system_info(port_limit) → 1024
    /// ERL_MAX_PORTS=2000      erl … system_info(port_limit) → 2048  (table rounding)
    /// ERL_MAX_PORTS=999999999 erl … system_info(port_limit) → 134217727
    /// ERL_MAX_PORTS=bogus     erl … system_info(port_limit) → 1024
    /// (unset)                 erl … system_info(port_limit) → 65536
    /// ```
    #[test]
    fn max_ports_is_atoi_clamped_into_the_emulators_range() {
        for (value, clamped) in [
            ("1", 1024usize),
            ("100", 1024),
            ("1024", 1024),
            ("2000", 2000),
            ("65536", 65_536),
            ("134217727", 134_217_727),
            ("999999999", 134_217_727),
            ("bogus", 1024),
            ("", 1024),
            ("-5", 1024),
            ("2000abc", 2000),
        ] {
            let vars = from_env(&env(&[(MAX_PORTS_VAR, value)]));
            assert_eq!(vars.max_ports, Some(clamped), "ERL_MAX_PORTS={value}");
        }
        // Unset is `None` — no value at all, so the emulator's own default stands.
        assert_eq!(from_env(&env(&[])).max_ports, None);
    }

    #[test]
    fn crash_dump_seconds_keeps_atois_sign_and_zero() {
        // 0 = no dump, <0 = wait forever, >0 = alarm-bounded; unset is `None`.
        assert_eq!(from_env(&env(&[])).crash_dump_seconds, None);
        for (value, seconds) in [
            ("0", 0i64),
            ("5", 5),
            ("-1", -1),
            ("-30", -30),
            ("1abc", 1),
            (" 7", 7),
            ("+9", 9),
        ] {
            let vars = from_env(&env(&[(CRASH_DUMP_SECONDS_VAR, value)]));
            assert_eq!(vars.crash_dump_seconds, Some(seconds), "{value}");
        }
        // A malformed value is `atoi`'s 0 — the emulator's "write no dump, break now".
        assert_eq!(
            from_env(&env(&[(CRASH_DUMP_SECONDS_VAR, "bogus")])).crash_dump_seconds,
            Some(0)
        );
    }

    #[test]
    fn max_ets_tables_and_fullsweep_after_are_plain_atoi() {
        assert_eq!(from_env(&env(&[])).max_ets_tables, None);
        assert_eq!(from_env(&env(&[])).fullsweep_after, None);
        // 0 is the emulator's "use the default".
        assert_eq!(
            from_env(&env(&[(MAX_ETS_TABLES_VAR, "0")])).max_ets_tables,
            Some(0)
        );
        assert_eq!(
            from_env(&env(&[(MAX_ETS_TABLES_VAR, "2048")])).max_ets_tables,
            Some(2048)
        );
        assert_eq!(
            from_env(&env(&[(FULLSWEEP_AFTER_VAR, "10")])).fullsweep_after,
            Some(10)
        );
        // Non-numeric ⇒ `atoi`'s 0 (never a wrong value, never a dropped variable).
        assert_eq!(
            from_env(&env(&[(MAX_ETS_TABLES_VAR, "lots")])).max_ets_tables,
            Some(0)
        );
        assert_eq!(
            from_env(&env(&[(FULLSWEEP_AFTER_VAR, "many")])).fullsweep_after,
            Some(0)
        );
    }

    /// `c_atoi` is `(int)strtol(v, NULL, 10)`: leading whitespace, an optional sign, then
    /// digits — stopping at the first non-digit, and `0` when there are none.
    #[test]
    fn c_atoi_follows_strtol() {
        assert_eq!(c_atoi("42"), 42);
        assert_eq!(c_atoi("-42"), -42);
        assert_eq!(c_atoi("+42"), 42);
        assert_eq!(c_atoi("  42"), 42);
        assert_eq!(c_atoi("\t42\n"), 42);
        assert_eq!(c_atoi("42abc"), 42);
        assert_eq!(c_atoi("4 2"), 4);
        assert_eq!(c_atoi(""), 0);
        assert_eq!(c_atoi("   "), 0);
        assert_eq!(c_atoi("abc"), 0);
        assert_eq!(c_atoi("-"), 0);
        assert_eq!(c_atoi("+"), 0);
        assert_eq!(c_atoi("0"), 0);
        // Out-of-range magnitudes saturate (the C's behaviour is undefined there).
        assert_eq!(c_atoi(&"9".repeat(40)), i64::MAX);
        assert_eq!(c_atoi(&format!("-{}", "9".repeat(40))), -i64::MAX);
    }

    #[test]
    fn the_plus_q_override_is_detected_by_token_equality() {
        let tokens =
            |values: &[&str]| -> Vec<String> { values.iter().map(|v| v.to_string()).collect() };
        assert!(has_plus_q_switch(&tokens(&["-noshell", "+Q", "4096"])));
        assert!(has_plus_q_switch(&tokens(&["+Q", "1024"])));
        assert!(!has_plus_q_switch(&tokens(&["-noshell"])));
        assert!(!has_plus_q_switch(&[]));
        // An inline `+Q4096` is not the `+Q` switch (the parser treats it as an
        // unmodeled `+`-flag), so it does not suppress the `ERL_MAX_PORTS` default.
        assert!(!has_plus_q_switch(&tokens(&["+Q4096"])));
        // A `-q` is a different token entirely.
        assert!(!has_plus_q_switch(&tokens(&["-Q", "4096"])));
    }

    /// Every field is populated when every variable is set — the end-to-end shape the CLI
    /// stores on [`RuntimeConfig::env_vars`](crate::config::RuntimeConfig::env_vars).
    #[test]
    fn every_variable_is_read() {
        let vars = from_env(&env(&[
            ("ERL_INETRC", "/etc/inetrc"),
            ("ERL_MAX_PORTS", "2048"),
            ("ERL_CRASH_DUMP", "/tmp/dump"),
            ("ERL_CRASH_DUMP_SECONDS", "-1"),
            ("ERL_ROOTDIR", "/opt/otp"),
            ("ERL_MAX_ETS_TABLES", "100"),
            ("ERL_FULLSWEEP_AFTER", "10"),
        ]));
        assert_eq!(
            vars,
            ErlEnv {
                inetrc: Some("/etc/inetrc".to_string()),
                max_ports: Some(2048),
                crash_dump: Some("/tmp/dump".to_string()),
                crash_dump_seconds: Some(-1),
                rootdir: Some("/opt/otp".to_string()),
                max_ets_tables: Some(100),
                fullsweep_after: Some(10),
            }
        );
    }
}
