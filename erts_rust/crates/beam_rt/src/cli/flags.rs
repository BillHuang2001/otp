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

//! The emulator switches this runtime recognizes and **validates**, whether or not they
//! have a runtime-visible effect.
//!
//! These are the `+`/`-` switches real `erl` hands to the emulator rather than to
//! `init` (contract A.6). They are recognized, **validated** against the legacy C
//! ground truth and preserved verbatim in
//! [`extra_emulator_flags`](beam_common::RuntimeConfig::extra_emulator_flags) so that
//! nothing is lost — but they are **not init arguments**: in the oracle *every*
//! `+`-flag is an emulator flag, so `init:get_argument('S'|'A'|'P'|'Q'|'K'|'hms'|'sbt'|'t'|'e'|'B')`
//! is the bare atom `error` (verified against OTP 29.1.1 / erts-17.1). See
//! [`crate::cli::parse`].
//!
//! A preserved flag with a modelled effect is read back out of that channel by the
//! `system_info/1` key it drives — `+A` → `thread_pool_size` (`crate::info::schedulers`),
//! `+t` → `atom_limit` / `+e` → `ets_limit` (`crate::info::limits`), `+Bi` →
//! `break_ignored` (`crate::info::system`) and `+sbt` → `scheduler_bind_type`
//! (`crate::info::schedulers`) — through
//! [`RuntimeConfig::preserved_emulator_flag`](crate::config::RuntimeConfig::preserved_emulator_flag).
//! The remaining flags here (`+Q`/`+K`/`+M*`/…) are validated and preserved but drive
//! nothing today.
//!
//! # Ground truth
//!
//! * `+P`/`+Q` — `erts/emulator/beam/erl_init.c:1870-1905`: `strtol` + the range
//!   check `[ERTS_MIN_PORTS|PROCESSES, ERTS_MAX_PORTS|PROCESSES]`, or the literal
//!   `legacy`. `ERTS_MIN_*` = 1024 (`erl_port.h:52`, `erl_process.h:3021`);
//!   `ERTS_MAX_*` = `ERTS_PTAB_MAX_SIZE - 1` = 134 217 727 (`erl_ptab.h:174`,
//!   `erl_node_container_utils.h:180,248`).
//! * `+sbt` — `erts/emulator/beam/erl_cpu_topology.c:623-647`: the nine bind types.
//! * `+t`/`+e` — `erl_init.c:2217-2234` (`strtol` + `[MIN_ATOM_TABLE_SIZE,
//!   MAX_ATOM_TABLE_SIZE]`) and `erl_init.c:1696-1707` (`atoi`, negative rejected).
//! * `-M*`/`+M*` — `erts/emulator/beam/erl_alloc.c:handle_args` (1556-1879) and
//!   `handle_au_arg` (1282-1553).
//! * `+K` — `erts/emulator/sys/common/erl_check_io.c:2467`: the emulator only
//!   `get_arg`s the value; there is **no** validation. `beam_rt` accepts the two
//!   documented literals instead (a documented divergence).
//!
//! # Malformed values (the oracle's exit 1 — matched)
//!
//! A malformed value of a recognized emulator switch is neither a CLI usage error nor a
//! capability gap: the oracle's `beam.smp` prints `bad …` plus its own usage text and exits
//! **1**, so [`error::bad_switch_value`] carries the flag-rejection kind and
//! [`crate::cli::run`] reports that same exit status with this runtime's one-line
//! diagnostic (the 7331-byte `beam.smp` usage block is deliberately not reproduced). The
//! C's numeric laxness (`atoi`/`ErtsStrToSint`, which silently ignores trailing garbage)
//! **is** reproduced for the switches whose reader is a bare `atoi` (`+A`,
//! `+hms`/`+hmbs`, `+hmax`, `+e`, `+K`), while the `strtol`-checked ones (`+t`, `+P`,
//! `+Q`, `+S`, `+sbt`) stay strict.
//!
//! `-M*` is a further, deliberate divergence. Through the `erl` wrapper a `-M*` flag
//! is **not** forwarded to the emulator at all (`erlexec.c:865`: `-M…` falls to
//! `add_arg`), so the oracle treats it as an opaque init argument and never validates
//! it; only `+M*` is rewritten to `-M*` and validated
//! (`erlexec.c:1009-1028`). `beam_rt` recognizes the whole `-M*`/`+M*` family,
//! validates `+M*` against the emulator grammar, and accepts `-M*` without validating
//! it (oracle parity) — but records **neither** as an init argument.

use beam_common::Result;

use crate::error;

/// `ERTS_MIN_PORTS` (`erts/emulator/beam/erl_port.h:52`).
pub(super) const ERTS_MIN_PORTS: i64 = 1024;
/// `ERTS_MAX_PORTS = ERTS_PTAB_MAX_SIZE - 1` (`erl_node_container_utils.h:248`).
pub(super) const ERTS_MAX_PORTS: i64 = 134_217_727;
/// `ERTS_MIN_PROCESSES` (`erts/emulator/beam/erl_process.h:3021`).
pub(super) const ERTS_MIN_PROCESSES: i64 = 1024;
/// `ERTS_MAX_PROCESSES = ERTS_PTAB_MAX_SIZE - 1` (`erl_node_container_utils.h:180`).
pub(super) const ERTS_MAX_PROCESSES: i64 = 134_217_727;

/// Whether `argument` is an `-M*`/`+M*` allocator switch (`erlexec.c:1009-1028`).
pub(super) fn is_allocator_switch(argument: &str) -> bool {
    argument.starts_with("+M")
}

/// `+Q N` — the maximum number of ports: `legacy` (`None`, the emulator's default
/// table), or an integer in `[ERTS_MIN_PORTS, ERTS_MAX_PORTS]` (`erl_init.c:1888-1905`).
///
/// The returned number is the **requested** table size, not the one the oracle reports
/// through `system_info(port_limit)`: the emulator additionally rounds the table up to a
/// power of two (`+Q 2000` observes 2048, `+Q 134217726` observes 134217727). That
/// sizing step is applied by [`crate::info`]'s `port_limit`, where the request becomes
/// observable — exactly as it is for `ERL_MAX_PORTS` (see [`crate::cli::vars`]).
pub(super) fn parse_max_ports(switch: &str, value: &str) -> Result<Option<usize>> {
    if value == "legacy" {
        return Ok(None);
    }
    match value.parse::<i64>() {
        Ok(number) if (ERTS_MIN_PORTS..=ERTS_MAX_PORTS).contains(&number) => {
            Ok(Some(number as usize))
        }
        _ => Err(error::bad_switch_value(switch, value)),
    }
}

/// `+P N` — the maximum number of processes: `legacy` (`None`), or an integer in
/// `[ERTS_MIN_PROCESSES, ERTS_MAX_PROCESSES]` (`erl_init.c:1870-1886`).
pub(super) fn parse_max_processes(switch: &str, value: &str) -> Result<Option<usize>> {
    if value == "legacy" {
        return Ok(None);
    }
    match value.parse::<i64>() {
        Ok(number) if (ERTS_MIN_PROCESSES..=ERTS_MAX_PROCESSES).contains(&number) => {
            Ok(Some(number as usize))
        }
        _ => Err(error::bad_switch_value(switch, value)),
    }
}

/// `+K [true|false]` — the kernel-poll switch. The emulator itself does not validate
/// the value (`erl_check_io.c:2467`): it only `get_arg`s it, so **any** value is accepted
/// here too (`+K bogus` exits 0 in the oracle) and preserved verbatim.
pub(super) fn parse_kernel_poll(switch: &str, value: &str) -> Result<()> {
    let _ = (switch, value);
    Ok(())
}

/// The nine `+sbt` scheduler bind types (`erl_cpu_topology.c:628-647`).
pub(super) const SCHEDULER_BIND_TYPES: [&str; 9] =
    ["u", "db", "s", "ps", "ts", "tnnps", "nnps", "nnts", "ns"];
/// The `+s` sub-options with a value (`erlexec.c:129-150` `pluss_val_switches`, plus
/// `early_init`'s `+ssrct`, `erl_init.c:943-954`).
const EMULATOR_S_SUB_OPTIONS: &[&str] = &[
    "sbt", "sbwtdcpu", "sbwtdio", "sbwt", "scl", "sct", "secio", "sfwi", "stbt", "swct",
    "swtdcpu", "swtdio", "swt", "sws", "sss", "sssdcpu", "sssdio", "spp", "sub", "srct",
];

/// Whether the emulator's own argument loop recognizes the flag spelling `name` — the
/// recognition rule the oracle's `+X` fallthrough depends on.
///
/// `erlexec` rewrites an **unrecognized** `+X` to `-X` and hands it to `beam.smp`
/// (`erts/etc/common/erlexec.c:1098-1101`), whose argument loop has one `case` per
/// recognized flag letter and otherwise prints `<progname> unknown flag -X` and exits
/// **1** (`erts/emulator/beam/erl_init.c:1370-2491`, `default:` at 2488). So an
/// unrecognized `+X` is accepted **iff the emulator recognizes `-X`** — which is why
/// `+Weird` exits 0 (`-Weird` is the `-W` warning-map switch, whose inline value `eird`
/// starts with `e`) while `+k`, `+YY` and `+Z` exit 1 (no `-k`/`-Y`/`-Z` case), and why
/// `+z` exits 1 even though `-z` *is* a case: that case dispatches on a sub-option and
/// rejects a bare spelling. Verified against OTP 29.1.1 / erts-17.1.
///
/// The letters whose `case` dispatches on a **name** (`has_prefix`) carry their accepted
/// sub-options here. The emulator's *value* validation for an unmodeled flag
/// (`+Wx` ⇒ `unrecognized warning_map option x`, `+Cfoo` ⇒ `Invalid time warp mode`) is
/// deliberately **not** modelled: a recognized name is accepted, and this runtime reports
/// no divergence rather than guessing at a value grammar the CLI has no other use for.
///
/// `++…` is accepted too: `erlexec`'s `case '+'` (`erlexec.c:1086-1096`) forwards it
/// (`++S` is the documented command-line `-S` passthrough), never the emulator's
/// `unknown flag` arm.
pub(super) fn emulator_flag_recognized(name: &str) -> bool {
    let Some(first) = name.chars().next() else {
        // A bare `+`: `erlexec`'s default rewrites it to `-`, which has no case.
        return false;
    };
    let rest = &name[first.len_utf8()..];
    match first {
        // The `++…` passthrough (and `++S`, the documented `-S` spelling).
        '+' => true,
        // Cases dispatched purely on the letter: any spelling is accepted.
        'L' | 'v' | 'V' | 'H' | 'i' | 'e' | 'P' | 'Q' | 't' | 'T' | 'R' | 'A' | 'a' | 'C'
        | 'c' => true,
        // `-p` requires `pc`/`pad` (`erl_init.c:1377-1409`).
        'p' => matches!(rest, "c" | "ad"),
        // `-f` requires the `fn` filename-encoding option (`erl_init.c:1411-1477`).
        'f' => rest.starts_with('n'),
        // `-h` requires one of its named sub-options, or the numeric backward-compatibility
        // form (`erl_init.c:1539-1651`).
        'h' => {
            ["ms", "mbs", "pds", "mqd", "max", "maxk", "maxel", "maxib"]
                .iter()
                .any(|option| rest.starts_with(option))
                || rest.chars().next().is_some_and(|c| c.is_ascii_digit())
        }
        // `-D`/`-Dibpl` (`erl_init.c:1653-1679`).
        'D' => rest.is_empty() || rest == "ibpl",
        // `-d`/`-dcg` (`erl_init.c:893-909`, `1681-1690`).
        'd' => rest.is_empty() || rest.starts_with("cg"),
        // `-J` JIT options (`erl_init.c:1714-1821`).
        'J' => [
            "Ddump",
            "Pperf",
            "Pperfdirectory",
            "Pcover",
            "Msingle",
        ]
        .iter()
        .any(|option| rest.starts_with(option)),
        // `-B`/`-Bi`/`-Bc`/`-Bd` (`erl_init.c:1823-1845`).
        'B' => matches!(rest, "" | "i" | "c" | "d"),
        // `-n` requires `ns`/`na`/`nd` (`erl_init.c:1846-1869`).
        'n' => ["s", "a", "d"].iter().any(|option| rest.starts_with(option)),
        // The `-s` scheduler sub-options (`erl_init.c:1922`).
        's' => EMULATOR_S_SUB_OPTIONS
            .iter()
            .any(|option| rest.starts_with(option)),
        // `-S`/`-SP`/`-SD{cpu,Pcpu,io}` (the early-init scheduler switches,
        // `erl_init.c:955-...`); the bare `+S` is consumed by the CLI's own arm.
        'S' => ["P", "Dcpu", "DPcpu", "Dio"].contains(&rest),
        // `-r`/`-rg` (`erl_init.c:911-928`, `2299`).
        'r' => rest.is_empty() || rest.starts_with('g'),
        // `-W{i,w,e}`: the warning-map switch reads its value inline, so the flag is only
        // recognized when the spelling carries one (`erl_init.c:2356-2372`).
        'W' => matches!(rest.chars().next(), Some('i' | 'w' | 'e')),
        // `-z`'s named sub-options (`erl_init.c:2374-2485`, `plusz_val_switches`).
        'z' => ["dbbl", "dntgc", "ebwt", "osrl", "hft", "mml", "mel"]
            .iter()
            .any(|option| rest.starts_with(option)),
        // `-IOt`/`-IOPt`/`-IOs` (the `erlexec` `+I` family, `erlexec.c:904-937`).
        'I' => matches!(rest, "Ot" | "OPt" | "Os"),
        // Everything else — `b`, `g`, `j`, `k`, `l`, `m` (the allocator surface reaches
        // the emulator as `-M`, handled before this fallthrough), `o`, `q`, `u`, `w`, `x`,
        // `y`, `E`, `F`, `G`, `K`(the CLI's own arm), `N`, `O`, `U`, `X`, `Y`, `Z`, … —
        // has no emulator `case`, so the emulator rejects it.
        _ => false,
    }
}
/// `+sbt <type>` — the scheduler bind type (`erl_cpu_topology.c:623-647`).
pub(super) fn parse_sbt(switch: &str, value: &str) -> Result<()> {
    if SCHEDULER_BIND_TYPES.contains(&value) {
        Ok(())
    } else {
        Err(error::bad_switch_value(switch, value))
    }
}

/// `MIN_ATOM_TABLE_SIZE` (`erts/emulator/beam/atom.h:33`).
///
/// `pub(crate)` because the `+t` effect (`crate::info::limits`) reads a preserved value
/// back through the same accepted range before it reports `atom_limit`.
pub(crate) const MIN_ATOM_TABLE_SIZE: i64 = 8_192;
/// `MAX_ATOM_TABLE_SIZE = MAX_ATOM_INDEX + 1` clamped to `(1 << 31) - 1`
/// (`erts/emulator/beam/atom.h:41`) — the largest `+t` value the oracle accepts.
pub(crate) const MAX_ATOM_TABLE_SIZE: i64 = (1 << 31) - 1;

/// Validates a `+t N` atom-table size, exactly as the C does
/// (`erts/emulator/beam/erl_init.c:2217-2234`): `strtol` the value and reject it unless it
/// lies in `[MIN_ATOM_TABLE_SIZE, MAX_ATOM_TABLE_SIZE]`.
///
/// Verified against the oracle (OTP 29.1.1 / erts-17.1):
///
/// ```text
/// +t 100        → exit 1, `bad atom table size 100`
/// +t bogus      → exit 1, `bad atom table size bogus`   (strtol → 0)
/// +t -1         → exit 1, `bad atom table size -1`
/// +t 4000000000 → exit 1, `bad atom table size 4000000000`  (> MAX)
/// +t 8192 / +t 1048576 / +t 2000000000 → accepted, and `atom_limit` follows the value
/// ```
///
/// `beam_rt` reports the emulator's own exit status (1) with its uniform one-line
/// malformed-value diagnostic instead of the oracle's `bad atom table size …` + usage.
pub(super) fn parse_atom_table_size(switch: &str, value: &str) -> Result<()> {
    match value.parse::<i64>() {
        Ok(size) if (MIN_ATOM_TABLE_SIZE..=MAX_ATOM_TABLE_SIZE).contains(&size) => Ok(()),
        _ => Err(error::bad_switch_value(switch, value)),
    }
}

/// Validates a `+e N` ETS-table count, mirroring the C's single check
/// (`erts/emulator/beam/erl_init.c:1696-1707`): the value is `atoi`'d and only a
/// **negative** result is rejected; a non-numeric value `atoi`s to `0` and is accepted.
///
/// The effective `ets_limit` is `max(atoi(N), DB_DEF_MAX_TABS)` — `erl_db.c:4789-4792` —
/// which is why `+e 100`, `+e 0` and `+e abc` all report `8192` while `+e 16384` reports
/// `16384` (verified against OTP 29.1.1 / erts-17.1).
pub(super) fn parse_ets_tables(switch: &str, value: &str) -> Result<()> {
    if atoi(value) < 0 {
        Err(error::bad_switch_value(switch, value))
    } else {
        Ok(())
    }
}

/// The C `atoi` used by the emulator's numeric switch readers: an optional sign and the
/// leading digits, `0` for anything else (`abc`, ``, `12abc` → `0`/`0`/`12`). A value too
/// large to fit an `i64` saturates, which still lands on the same accept/reject side.
///
/// `pub(crate)` because the `+e` effect (`crate::info::limits`) must reproduce the C's own
/// lax parse — the value `ets_limit` reports is `max(atoi(<the +e value>), 8192)`
/// (`erl_init.c:1699`, `erl_db.c:4789-4792`).
pub(crate) fn atoi(value: &str) -> i64 {
    let (sign, digits) = match value.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None => (1i64, value.strip_prefix('+').unwrap_or(value)),
    };
    let mut result: i64 = 0;
    for byte in digits.bytes() {
        if !byte.is_ascii_digit() {
            break;
        }
        result = result.saturating_mul(10).saturating_add(i64::from(byte - b'0'));
    }
    sign.saturating_mul(result)
}

/// `-emu_flavor F` → the [`Flavor`](beam_common::Flavor) it selects.
///
/// `emu` ⇒ [`Flavor::Emu`]; `jit`/`smp` ⇒ [`Flavor::Jit`]. **Divergence** from the
/// oracle, whose `erlexec` accepts only `smp` and rejects `emu`/`jit`/anything else
/// with `erlexec: Invalid emulator type or flavor…` (exit 1): `beam_rt` accepts the
/// three so that [`Flavor::Emu`] is reachable, and rejects every other value as a
/// usage error.
pub(super) fn parse_emu_flavor(switch: &str, value: &str) -> Result<beam_common::Flavor> {
    match value {
        "emu" => Ok(beam_common::Flavor::Emu),
        "jit" | "smp" => Ok(beam_common::Flavor::Jit),
        _ => Err(error::bad_switch_value(switch, value)),
    }
}

/// Validates a `+M*` allocator switch (`erl_alloc.c:handle_args`), returning the
/// index of the next switch (the value may be the following token).
///
/// Only a `+M*` flag reaches this path: `erlexec` rewrites `+M*` to an **emulator**
/// flag (`argv[i][0] = '-'` + `add_Eargs`, `erlexec.c:886-892`), which the emulator's
/// allocator parser validates. A user-supplied `-M*` is never rewritten — it is an
/// ordinary unknown `-flag`, so [`is_allocator_switch`] does not match it and the
/// generic path records it as an init argument (`-Mfoo 1` ⇒ `init:get_argument('Mfoo')`
/// = `{ok,[["1"]]}`; verified OTP 29.1.1 / erts-17.1).
pub(super) fn parse_allocator(
    switch: &str,
    args: &[String],
    index: usize,
) -> Result<usize> {
    validate_allocator(switch, args, index)
}

/// The per-argv-token value reader: `erl_alloc.c:get_value` (1115-1131).
///
/// The text after the option name **within the same token** is the value when it is
/// non-empty, else the next command-line token; `=` is never a separator, and a next
/// token of exactly `--` is rejected.
struct Reader<'a> {
    args: &'a [String],
    index: usize,
}

impl<'a> Reader<'a> {
    /// The value of an option whose remaining in-token text is `rest`.
    fn value(&mut self, rest: &str, switch: &str) -> Result<String> {
        if !rest.is_empty() {
            return Ok(rest.to_string());
        }
        match self.args.get(self.index + 1) {
            Some(next) if next != "--" => {
                self.index += 1;
                Ok(next.clone())
            }
            // The allocator switches (`+M*`) are emulator flags, so a missing operand is
            // the emulator/launcher diagnostic (`Missing argument(s) for '+M...'.`, exit 1),
            // not this CLI's usage status.
            _ => Err(error::missing_switch_operand(switch)),
        }
    }
}

/// The per-allocator capability gates (`erl_alloc.c:676-760`, `set_default_*_opts`).
#[derive(Clone, Copy)]
struct Capabilities {
    /// `carrier_migration_allowed` — gates `ac*`/`cp` (1296, 1423).
    carrier_migration: bool,
    /// `disable_allowed` — gates `e false` (1454).
    disable: bool,
    /// `thr_spec_allowed` — gates `t true` (1531).
    thr_spec: bool,
}

/// The capability gates of an alloc-util allocator letter.
///
/// `literal_alloc` (`I`) allows none of the three; `temp_alloc` (`T`) disallows being
/// disabled and carrier migration; every other alloc-util allocator allows all three.
fn capabilities(letter: char) -> Capabilities {
    match letter {
        'I' => Capabilities {
            carrier_migration: false,
            disable: false,
            thr_spec: false,
        },
        'T' => Capabilities {
            carrier_migration: false,
            disable: false,
            thr_spec: true,
        },
        _ => Capabilities {
            carrier_migration: true,
            disable: true,
            thr_spec: true,
        },
    }
}

/// The lax integer parse of `ErtsStrToSint`: an optional sign, at least one digit, and
/// any trailing text ignored (`0x10` ⇒ 0, `10abc` ⇒ 10).
fn lax_int(text: &str) -> Option<i64> {
    let trimmed = text.trim_start();
    let (negative, digits) = match trimmed.as_bytes().first()? {
        b'-' => (true, &trimmed[1..]),
        b'+' => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    let number: String = digits.chars().take_while(|c| c.is_ascii_digit()).collect();
    if number.is_empty() {
        return None;
    }
    number.parse::<i64>().ok().map(|v| if negative { -v } else { v })
}

/// A non-negative integer value (`get_kb_value`/`get_amount_value`/`get_mb_value`).
fn take_non_negative(rest: &str, reader: &mut Reader, switch: &str) -> Result<i64> {
    let value = reader.value(rest, switch)?;
    match lax_int(&value) {
        Some(number) if number >= 0 => Ok(number),
        _ => Err(error::bad_switch_value(switch, &value)),
    }
}

/// A `true`/`false` value (`get_bool_value`).
fn take_bool(rest: &str, reader: &mut Reader, switch: &str) -> Result<bool> {
    let value = reader.value(rest, switch)?;
    match value.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(error::bad_switch_value(switch, &value)),
    }
}

/// `acul`/`acful`: `de`, or an integer in `0..=100` (`get_acul_value`).
fn take_percentage(rest: &str, reader: &mut Reader, switch: &str) -> Result<()> {
    let value = reader.value(rest, switch)?;
    if value == "de" {
        return Ok(());
    }
    match lax_int(&value) {
        Some(number) if (0..=100).contains(&number) => Ok(()),
        _ => Err(error::bad_switch_value(switch, &value)),
    }
}

/// `as` — the allocation strategy (`erl_alloc.c:1324-1380`).
fn take_strategy(rest: &str, reader: &mut Reader, switch: &str, test_alloc: bool) -> Result<()> {
    let value = reader.value(rest, switch)?;
    let known = [
        "bf",
        "aobf",
        "gf",
        "af",
        "aoff",
        "aoffcbf",
        "aoffcaobf",
        "ageffcaoff",
        "ageffcbf",
        "ageffcaobf",
    ];
    if known.contains(&value.as_str()) || (test_alloc && value == "chaosff") {
        Ok(())
    } else {
        Err(error::bad_switch_value(switch, &value))
    }
}

/// `atags` — `true`/`false`/`code` (`erl_alloc.c:1385-1414`).
fn take_atags(rest: &str, reader: &mut Reader, switch: &str) -> Result<()> {
    let value = reader.value(rest, switch)?;
    match value.as_str() {
        "true" | "false" | "code" => Ok(()),
        _ => Err(error::bad_switch_value(switch, &value)),
    }
}

/// `handle_au_arg` (`erl_alloc.c:1282-1553`): one alloc-util allocator's sub-switch.
fn alloc_util(
    sub: &str,
    capabilities: Capabilities,
    test_alloc: bool,
    u_switch: bool,
    switch: &str,
    reader: &mut Reader,
) -> Result<()> {
    let bad = || error::bad_switch_value(switch, sub);
    let (carrier_migration, disable, thr_spec) = (
        capabilities.carrier_migration,
        capabilities.disable,
        capabilities.thr_spec,
    );
    match sub.chars().next() {
        Some('a') => {
            if sub.starts_with("ac") {
                if !carrier_migration && !u_switch {
                    return Err(bad());
                }
                if let Some(rest) = sub.strip_prefix("acul") {
                    take_percentage(rest, reader, switch)
                } else if let Some(rest) = sub.strip_prefix("acnl") {
                    take_non_negative(rest, reader, switch).map(|_| ())
                } else if let Some(rest) = sub.strip_prefix("acfml") {
                    take_non_negative(rest, reader, switch).map(|_| ())
                } else if let Some(rest) = sub.strip_prefix("acful") {
                    take_percentage(rest, reader, switch)
                } else {
                    Err(bad())
                }
            } else if let Some(rest) = sub.strip_prefix("asbcst") {
                take_non_negative(rest, reader, switch).map(|_| ())
            } else if let Some(rest) = sub.strip_prefix("as") {
                take_strategy(rest, reader, switch, test_alloc)
            } else if let Some(rest) = sub.strip_prefix("atags") {
                take_atags(rest, reader, switch)
            } else {
                Err(bad())
            }
        }
        Some('c') => match sub.strip_prefix("cp") {
            Some(rest) => {
                if !carrier_migration && !u_switch {
                    return Err(bad());
                }
                let value = reader.value(rest, switch)?;
                let accepted = value.len() == 1
                    && matches!(
                        value.as_bytes()[0],
                        b'B' | b'D' | b'E' | b'F' | b'H' | b'L' | b'R' | b'S' | b'@' | b':'
                    );
                if accepted {
                    Ok(())
                } else {
                    Err(error::bad_switch_value(switch, &value))
                }
            }
            None => Err(bad()),
        },
        Some('e') => {
            let enabled = take_bool(&sub[1..], reader, switch)?;
            if !enabled && !disable && !u_switch {
                Err(bad())
            } else {
                Ok(())
            }
        }
        Some('l') => match sub.strip_prefix("lmbcs") {
            Some(rest) => take_non_negative(rest, reader, switch).map(|_| ()),
            None => Err(bad()),
        },
        Some('m') => {
            if let Some(rest) = sub.strip_prefix("mbcgs") {
                take_non_negative(rest, reader, switch).map(|_| ())
            } else if let Some(rest) = sub.strip_prefix("mbsd") {
                take_non_negative(rest, reader, switch).map(|_| ())
            } else if let Some(rest) = sub.strip_prefix("mmbcs") {
                take_non_negative(rest, reader, switch).map(|_| ())
            } else if let Some(rest) = sub.strip_prefix("mmmbc") {
                take_non_negative(rest, reader, switch).map(|_| ())
            } else if let Some(rest) = sub.strip_prefix("mmsbc") {
                take_non_negative(rest, reader, switch).map(|_| ())
            } else {
                Err(bad())
            }
        }
        Some('r') => {
            if let Some(rest) = sub.strip_prefix("rsbcmt") {
                take_non_negative(rest, reader, switch).map(|_| ())
            } else if let Some(rest) = sub.strip_prefix("rsbcst") {
                take_non_negative(rest, reader, switch).map(|_| ())
            } else if let Some(rest) = sub.strip_prefix("rmbcmt") {
                take_non_negative(rest, reader, switch).map(|_| ())
            } else if let Some(rest) = sub.strip_prefix("ramv") {
                take_bool(rest, reader, switch).map(|_| ())
            } else {
                Err(bad())
            }
        }
        Some('s') => {
            if let Some(rest) = sub.strip_prefix("sbct") {
                take_non_negative(rest, reader, switch).map(|_| ())
            } else if let Some(rest) = sub.strip_prefix("smbcs") {
                take_non_negative(rest, reader, switch).map(|_| ())
            } else {
                Err(bad())
            }
        }
        Some('t') => {
            let enabled = take_bool(&sub[1..], reader, switch)?;
            if enabled && !thr_spec && !u_switch {
                Err(bad())
            } else {
                Ok(())
            }
        }
        _ => Err(bad()),
    }
}

/// `handle_args`' `-MY*` case — the system allocator (`erl_alloc.c:1688-1731`).
fn system_alloc(sub: &str, switch: &str, reader: &mut Reader) -> Result<()> {
    let bad = || error::bad_switch_value(switch, sub);
    if let Some(rest) = sub.strip_prefix("tt") {
        match take_non_negative(rest, reader, switch) {
            Ok(number) if number <= (i32::MAX as i64) / 1024 => Ok(()),
            _ => Err(bad()),
        }
    } else if let Some(rest) = sub.strip_prefix("tp") {
        match take_non_negative(rest, reader, switch) {
            Ok(number) if number <= (i32::MAX as i64) / 1024 => Ok(()),
            _ => Err(bad()),
        }
    } else if let Some(rest) = sub.strip_prefix("m") {
        // "Has been handled by erlexec" — the value is consumed and ignored.
        reader.value(rest, switch).map(|_| ())
    } else if let Some(rest) = sub.strip_prefix("e") {
        let value = reader.value(rest, switch)?;
        if value == "true" {
            Ok(())
        } else {
            Err(error::bad_switch_value(switch, &value))
        }
    } else {
        Err(bad())
    }
}

/// `handle_args`' `-MM*` case — the memory segment (mseg) options
/// (`erl_alloc.c:1612-1675`).
fn mseg(sub: &str, switch: &str, reader: &mut Reader) -> Result<()> {
    let bad = || error::bad_switch_value(switch, sub);
    if let Some(rest) = sub.strip_prefix("amcbf") {
        take_non_negative(rest, reader, switch).map(|_| ())
    } else if let Some(rest) = sub.strip_prefix("rmcbf") {
        take_non_negative(rest, reader, switch).map(|_| ())
    } else if let Some(rest) = sub.strip_prefix("mcs") {
        take_non_negative(rest, reader, switch).map(|_| ())
    } else if let Some(rest) = sub.strip_prefix("scs") {
        take_non_negative(rest, reader, switch).map(|_| ())
    } else if let Some(rest) = sub.strip_prefix("sco") {
        take_bool(rest, reader, switch).map(|_| ())
    } else if let Some(rest) = sub.strip_prefix("scrpm") {
        take_bool(rest, reader, switch).map(|_| ())
    } else if let Some(rest) = sub.strip_prefix("scrfsd") {
        take_non_negative(rest, reader, switch).map(|_| ())
    } else if let Some(rest) = sub.strip_prefix("lp") {
        let value = reader.value(rest, switch)?;
        match value.as_str() {
            "on" | "off" => Ok(()),
            _ => Err(error::bad_switch_value(switch, &value)),
        }
    } else {
        Err(bad())
    }
}

/// The `+M*` allocator-flag validator: the letter set + the value syntax of
/// `erl_alloc.c:handle_args` (1556-1883).
fn validate_allocator(switch: &str, args: &[String], index: usize) -> Result<usize> {
    let body = &switch[2..]; // strip the leading `-M`/`+M`
    let letter = body
        .chars()
        .next()
        .ok_or_else(|| error::bad_switch_value(switch, body))?;
    let sub = &body[1..];
    let mut reader = Reader { args, index };
    match letter {
        'B' | 'D' | 'E' | 'F' | 'H' | 'L' | 'R' | 'S' | 'T' | 'Z' => alloc_util(
            sub,
            capabilities(letter),
            letter == 'Z',
            false,
            switch,
            &mut reader,
        )?,
        'I' => {
            if let Some(rest) = sub.strip_prefix("scs") {
                take_non_negative(rest, &mut reader, switch).map(|_| ())?;
            } else {
                alloc_util(sub, capabilities('I'), false, false, switch, &mut reader)?;
            }
        }
        'M' => mseg(sub, switch, &mut reader)?,
        'Y' => system_alloc(sub, switch, &mut reader)?,
        // `argv[i][3] == 'a'`: the `e` is already stripped from `sub`, so only the
        // trailing `a` of the `ea` option name remains to be consumed
        // (`erl_alloc.c:1733-1783`).
        'e' => {
            let rest = sub
                .strip_prefix("a")
                .ok_or_else(|| error::bad_switch_value(switch, sub))?;
            let value = reader.value(rest, switch)?;
            match value.as_str() {
                "min" | "max" | "r9c" | "r10b" | "r11b" => {}
                _ => return Err(error::bad_switch_value(switch, &value)),
            }
        }
        'l' => {
            let rest = sub
                .strip_prefix("pm")
                .ok_or_else(|| error::bad_switch_value(switch, sub))?;
            let value = reader.value(rest, switch)?;
            match value.as_str() {
                "all" | "no" => {}
                _ => return Err(error::bad_switch_value(switch, &value)),
            }
        }
        'd' => {
            let rest = sub
                .strip_prefix("ai")
                .ok_or_else(|| error::bad_switch_value(switch, sub))?;
            let value = reader.value(rest, switch)?;
            if value == "max" || lax_int(&value).is_some_and(|number| number >= 0) {
                // accepted
            } else {
                return Err(error::bad_switch_value(switch, &value));
            }
        }
        'u' => {
            if let Some(rest) = sub.strip_prefix("ycs") {
                take_non_negative(rest, &mut reader, switch).map(|_| ())?;
            } else if let Some(rest) = sub.strip_prefix("mmc") {
                take_non_negative(rest, &mut reader, switch).map(|_| ())?;
            } else if let Some(rest) = sub.strip_prefix("sac") {
                take_bool(rest, &mut reader, switch).map(|_| ())?;
            } else if let Some(rest) = sub.strip_prefix("madtn") {
                take_bool(rest, &mut reader, switch).map(|_| ())?;
            } else {
                // `-Mu<sub>` applies `<sub>` to every alloc-util allocator, so the
                // capability gates are bypassed (`u_switch = 1`).
                alloc_util(
                    sub,
                    Capabilities {
                        carrier_migration: true,
                        disable: true,
                        thr_spec: true,
                    },
                    false,
                    true,
                    switch,
                    &mut reader,
                )?;
            }
        }
        _ => return Err(error::bad_switch_value(switch, body)),
    }
    Ok(reader.index + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The argv a switch is parsed from, as the parser sees it.
    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|argument| argument.to_string()).collect()
    }

    /// `+Q N`: `legacy` (the default table, `None`), or an integer in
    /// `[ERTS_MIN_PORTS, ERTS_MAX_PORTS]` (`erl_init.c:1888-1905`).
    ///
    /// Oracle (OTP 29.1.1 / erts-17.1):
    /// `erl +Q 1024|134217727|legacy -noshell -eval 'halt(0).'` exits 0;
    /// `+Q 1000|0|134217728|abc` exits 1 (`bad number of ports <v>` + usage).
    #[test]
    fn max_ports_accepts_legacy_and_the_oracle_range() {
        assert_eq!(parse_max_ports("+Q", "1024").expect("valid"), Some(1024));
        assert_eq!(
            parse_max_ports("+Q", "134217727").expect("valid"),
            Some(134_217_727)
        );
        // `legacy` keeps the emulator's default table — no request at all.
        assert_eq!(parse_max_ports("+Q", "legacy").expect("valid"), None);
        for value in [
            "1000",
            "0",
            "1023",
            "134217728",
            "-1",
            "abc",
            "",
            // `ErtsStrToSint` silently ignores a trailing suffix; this parser does not
            // (a documented divergence — the oracle accepts `+Q 1024abc` as 1024).
            "1024abc",
        ] {
            assert!(
                parse_max_ports("+Q", value).is_err(),
                "{value:?} must be rejected"
            );
        }
    }

    /// `+P N`: the same range (`erl_init.c:1870-1886`); `legacy` keeps the default
    /// (`None`).
    ///
    /// Oracle: `+P 1024|134217727|legacy` exits 0; `+P 1000|0|134217728|abc` exits 1
    /// (`bad number of processes <v>` + usage).
    #[test]
    fn max_processes_accepts_legacy_and_the_oracle_range() {
        assert_eq!(
            parse_max_processes("+P", "1024").expect("valid"),
            Some(1024)
        );
        assert_eq!(
            parse_max_processes("+P", "134217727").expect("valid"),
            Some(134_217_727)
        );
        assert_eq!(parse_max_processes("+P", "legacy").expect("valid"), None);
        for value in ["1000", "0", "1023", "134217728", "-1", "abc", ""] {
            assert!(
                parse_max_processes("+P", value).is_err(),
                "{value:?} must be rejected"
            );
        }
    }

    /// `+K [true|false]` — and, as the oracle does, **any** other value.
    ///
    /// The oracle does **not** validate this value at all (`erl +K bogus -noshell
    /// -eval 'halt(0).'` exits 0 — `erl_check_io.c:2467` merely `get_arg`s it, verified
    /// against OTP 29.1.1 / erts-17.1 for `true`, `false`, `bogus`, `TRUE`, `1`, ``),
    /// so `parse_kernel_poll` accepts every value and preserves it verbatim.
    #[test]
    fn kernel_poll_accepts_any_value() {
        for value in ["true", "false", "bogus", "TRUE", "", "1"] {
            assert!(parse_kernel_poll("+K", value).is_ok(), "{value:?}");
        }
    }

    /// `+sbt <type>`: the nine bind types of `erl_cpu_topology.c:628-647`.
    ///
    /// Oracle: all nine exit 0; `+sbt bogus` exits 1.
    #[test]
    fn sbt_accepts_the_nine_bind_types() {
        assert_eq!(
            SCHEDULER_BIND_TYPES,
            ["u", "db", "s", "ps", "ts", "tnnps", "nnps", "nnts", "ns"]
        );
        for value in SCHEDULER_BIND_TYPES {
            assert!(parse_sbt("+sbt", value).is_ok(), "{value:?}");
        }
        for value in ["bogus", "", "U", "nnts ", "ts2"] {
            assert!(parse_sbt("+sbt", value).is_err(), "{value:?}");
        }
    }

    /// `-emu_flavor F`: `emu` ⇒ [`Flavor::Emu`], `jit`/`smp` ⇒ [`Flavor::Jit`].
    ///
    /// Divergence: the oracle's `erlexec` accepts only `smp` (`-emu_flavor emu` ⇒
    /// `erlexec: Invalid emulator type or flavor.`, exit 1); accepting `emu`/`jit` is
    /// what makes [`Flavor::Emu`] reachable.
    #[test]
    fn emu_flavor_selects_the_documented_values() {
        fn flavor(value: &str) -> beam_common::Flavor {
            parse_emu_flavor("-emu_flavor", value).expect("a documented flavor")
        }
        assert_eq!(flavor("emu"), beam_common::Flavor::Emu);
        assert_eq!(flavor("jit"), beam_common::Flavor::Jit);
        assert_eq!(flavor("smp"), beam_common::Flavor::Jit);
        for value in ["bogus", "", "EMU", "emu "] {
            assert!(parse_emu_flavor("-emu_flavor", value).is_err(), "{value:?}");
        }
    }

    /// The `-M*`/`+M*` recognition (`erlexec.c:1009-1028`).
    #[test]
    fn is_allocator_switch_matches_only_the_plus_form() {
        // Only a `+M*` flag is rewritten by `erlexec` to an emulator flag and validated.
        for argument in ["+M", "+Mfoo", "+Mea", "+MMscs", "+MBas"] {
            assert!(is_allocator_switch(argument), "{argument:?}");
        }
        // A `-M*` flag is an ordinary unknown `-flag` (recorded as an init argument),
        // so it must NOT be claimed by the allocator path.
        for argument in [
            "-M", "-Mfoo", "-Mea", "-m", "+m", "-eval", "+S", "M", "-", "--M", "-+M", "",
        ] {
            assert!(!is_allocator_switch(argument), "{argument:?}");
        }
    }

    /// The `+M*` grammar's **accepted** spellings — both value forms (in-token text and
    /// the following token), for each allocator letter family.
    ///
    /// Oracle: every one of these exits 0.
    #[test]
    fn allocator_accepts_the_documented_spellings() {
        // (switch token, following token, expected next index)
        let cases: &[(&str, Option<&str>, usize)] = &[
            // `+MM*` — the memory segment (mseg) options.
            ("+MMscs", Some("1024"), 2),
            ("+MMscs1024", None, 1),
            ("+MMamcbf", Some("100"), 2),
            ("+MMamcbf100", None, 1),
            // `lp` is the one mseg option whose name is two characters longer than the
            // `-MM` prefix, so `-MMlp on` / `-MMlpon` are the accepted spellings
            // (`erl_alloc.c:1682-1706` reads `argv[i]+5`).
            ("+MMlp", Some("on"), 2),
            ("+MMlpon", None, 1),
            // `+MB*` — one alloc-util allocator (`binary_alloc`).
            ("+MBas", Some("aobf"), 2),
            ("+MBasbf", None, 1),
            // `+MZ*` — `test_alloc`, the only allocator that knows `chaosff`.
            ("+MZas", Some("chaosff"), 2),
            ("+MZaschaosff", None, 1),
            // `+MIscs` — the `literal_alloc`-specific mseg option.
            ("+MIscs", Some("100"), 2),
            ("+MIscs100", None, 1),
            // `+MYe` — the system allocator.
            ("+MYe", Some("true"), 2),
            ("+MYetrue", None, 1),
            // `+Mlpm` — lock all physical memory.
            ("+Mlpmno", None, 1),
        ];
        for (switch, next, expected) in cases {
            let mut argv = vec![switch.to_string()];
            if let Some(next) = next {
                argv.push(next.to_string());
            }
            let result = parse_allocator(switch, &argv, 0);
            assert_eq!(
                result.expect("a documented `+M*` spelling must be accepted"),
                *expected,
                "{argv:?} must consume {expected} token(s)"
            );
        }
    }

    /// The `+M*` grammar's **rejected** letters and values.
    ///
    /// Oracle: `+Mfoo`, `+M`, `+MMscs bogus`, `+MBas chaosff` all exit 1.
    #[test]
    fn allocator_rejects_unknown_letters_and_values() {
        for (switch, next) in [
            ("+Mfoo", None),
            ("+M", None),
            ("+MMscs", Some("bogus")),
            ("+MBas", Some("chaosff")),
            ("+MYe", Some("false")),
            ("+Mlpm", Some("maybe")),
        ] {
            let mut argv = vec![switch.to_string()];
            if let Some(next) = next {
                argv.push(next.to_string());
            }
            assert!(
                parse_allocator(switch, &argv, 0).is_err(),
                "{argv:?} must be rejected"
            );
        }
    }

    /// The five per-allocator **capability gates** (`erl_alloc.c:676-760`,
    /// `1296,1423,1454,1531`).
    ///
    /// Oracle: `+MIe false`, `+MTe false`, `+MIt true`, `+MIcp B`, `+MTacul 50` all
    /// exit 1 — `literal_alloc` allows none of the three, `temp_alloc` only `t true`.
    #[test]
    fn allocator_capability_gates_reject_what_the_c_c_rejects() {
        let cases: &[(&str, &str)] = &[
            // `literal_alloc` (`I`) cannot be disabled …
            ("+MIe", "false"),
            // … nor can `temp_alloc` (`T`).
            ("+MTe", "false"),
            // `literal_alloc` does not allow the thread-specific `t true`.
            ("+MIt", "true"),
            // `literal_alloc` disallows carrier migration (`cp`).
            ("+MIcp", "B"),
            // `temp_alloc` disallows carrier migration (`acul`).
            ("+MTacul", "50"),
        ];
        for (switch, value) in cases {
            let argv = args(&[switch, value]);
            assert!(
                parse_allocator(switch, &argv, 0).is_err(),
                "[{switch} {value}] must be rejected by the capability gate"
            );
        }
        // The same values are accepted where the capability *is* allowed: a normal
        // alloc-util allocator (`B`) may be disabled / migrated.
        assert_eq!(
            parse_allocator("+MBe", &args(&["+MBe", "false"]), 0).expect("allowed"),
            2
        );
        assert_eq!(
            parse_allocator("+MBcp", &args(&["+MBcp", "B"]), 0).expect("allowed"),
            2
        );
        // … and `-Mu*` bypasses the gates (`u_switch = 1`).
        assert_eq!(
            parse_allocator("+Mue", &args(&["+Mue", "false"]), 0).expect("allowed"),
            2
        );
    }

    /// The per-token value reader (`erl_alloc.c:get_value`, 1115-1131): the text after
    /// the option name **within the same token** is the value; only when it is empty is
    /// the next token used, and a next token of exactly `--` is never a value.
    #[test]
    fn allocator_reads_the_in_token_value_before_the_next_token() {
        let next_index = |switch: &str, argv: &[&str]| {
            parse_allocator(switch, &args(argv), 0)
                .expect("the documented spelling must be accepted")
        };
        // In-token text wins and consumes no extra token.
        assert_eq!(next_index("+MMscs1024", &["+MMscs1024"]), 1);
        // An empty in-token remainder falls through to the next token.
        assert_eq!(next_index("+MMscs", &["+MMscs", "1024"]), 2);
        // A missing value (end of argv, or `--`) is an error, never an empty value.
        assert!(parse_allocator("+MMscs", &args(&["+MMscs"]), 0).is_err());
        assert!(parse_allocator("+MMscs", &args(&["+MMscs", "--"]), 0).is_err());
    }

    /// `-M*` is **accepted unvalidated** (the oracle's `erlexec` never forwards it to the
    /// emulator: `erlexec.c:865`), and — unlike a `+M*` value — its next token is **not**
    /// consumed.
    ///
    /// Only a `+M*` flag reaches [`parse_allocator`] (a user `-M*` is an ordinary unknown
    /// `-flag`, recorded by the generic path), so this pins the `+M*` value-consumption
    /// contract: a value in the following token is consumed (`erl_alloc.c`'s `get_value`).
    #[test]
    fn allocator_plus_m_consumes_a_following_token_value() {
        assert_eq!(
            parse_allocator("+MMscs", &args(&["+MMscs", "1024"]), 0).expect("validated"),
            2
        );
        // The in-token form advances only past the switch itself.
        assert_eq!(
            parse_allocator("+MMscs1024", &args(&["+MMscs1024"]), 0).expect("validated"),
            1
        );
    }

    /// `+Mea <value>` — the emulator-wide allocator preset (`erl_alloc.c:1733-1783`):
    /// `argv[i][3] == 'a'` confirms the option is `ea`, and the value follows it.
    ///
    /// Oracle: `erl -noshell +Mea min -eval 'io:format("ok~n"),halt().'` exits 0 and
    /// prints `ok`; so do `max`/`r9c`/`r10b`/`r11b`. A missing value (`+Mea`) and an
    /// unknown value (`+Mea 1024`) are both rejected.
    #[test]
    fn allocator_mea_accepts_the_documented_presets() {
        for value in ["min", "max", "r9c", "r10b", "r11b"] {
            // Both spellings: the value in the next token, and concatenated.
            let split = args(&["+Mea", value]);
            assert_eq!(
                parse_allocator("+Mea", &split, 0)
                    .unwrap_or_else(|e| panic!("`+Mea {value}` must be accepted: {e}")),
                2,
                "{split:?} must consume the value token"
            );
            let joined = args(&[&format!("+Mea{value}")]);
            assert_eq!(
                parse_allocator(&joined[0], &joined, 0)
                    .unwrap_or_else(|e| panic!("`+Mea{value}` must be accepted: {e}")),
                1,
                "{joined:?} must consume no extra token"
            );
        }
        // A missing value is an error, never an empty one …
        assert!(parse_allocator("+Mea", &args(&["+Mea"]), 0).is_err());
        // … and an unknown preset is rejected too.
        for value in ["1024", "", "MIN", "min "] {
            let argv = args(&["+Mea", value]);
            assert!(
                parse_allocator("+Mea", &argv, 0).is_err(),
                "{argv:?} must be rejected"
            );
        }
        let argv = args(&["+Mea1024"]);
        assert!(parse_allocator("+Mea1024", &argv, 0).is_err(), "{argv:?}");
    }

    /// The sibling `-Mdai`/`-Mlpm` arms share the "already-stripped letter" shape and
    /// must stay accepted after the `+Mea` fix.
    ///
    /// Oracle: `+Mdai max` and `+Mlpm all` both exit 0.
    #[test]
    fn allocator_sibling_letter_arms_stay_accepted() {
        assert_eq!(
            parse_allocator("+Mdai", &args(&["+Mdai", "max"]), 0).expect("accepted"),
            2
        );
        assert_eq!(
            parse_allocator("+Mlpm", &args(&["+Mlpm", "all"]), 0).expect("accepted"),
            2
        );
        assert_eq!(
            parse_allocator("+Mdai1000", &args(&["+Mdai1000"]), 0).expect("accepted"),
            1
        );
        assert_eq!(
            parse_allocator("+Mlpmall", &args(&["+Mlpmall"]), 0).expect("accepted"),
            1
        );
    }
}
