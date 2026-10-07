//! The emulator switches that have **no runtime-visible effect** in this runtime.
//!
//! These are the `+`/`-` switches real `erl` hands to the emulator rather than to
//! `init` (contract A.6). They are recognized, **validated** against the legacy C
//! ground truth and preserved verbatim in
//! [`extra_emulator_flags`](beam_common::RuntimeConfig::extra_emulator_flags) so that
//! nothing is lost — but they are **not init arguments**: in the oracle *every*
//! `+`-flag is an emulator flag, so `init:get_argument('S'|'A'|'P'|'Q'|'K'|'hms'|'sbt')`
//! is the bare atom `error` (verified against OTP 29.1.1 / erts-17.1). See
//! [`crate::cli::parse`].
//!
//! # Ground truth
//!
//! * `+P`/`+Q` — `erts/emulator/beam/erl_init.c:1870-1905`: `strtol` + the range
//!   check `[ERTS_MIN_PORTS|PROCESSES, ERTS_MAX_PORTS|PROCESSES]`, or the literal
//!   `legacy`. `ERTS_MIN_*` = 1024 (`erl_port.h:52`, `erl_process.h:3021`);
//!   `ERTS_MAX_*` = `ERTS_PTAB_MAX_SIZE - 1` = 134 217 727 (`erl_ptab.h:174`,
//!   `erl_node_container_utils.h:180,248`).
//! * `+sbt` — `erts/emulator/beam/erl_cpu_topology.c:623-647`: the nine bind types.
//! * `-M*`/`+M*` — `erts/emulator/beam/erl_alloc.c:handle_args` (1556-1879) and
//!   `handle_au_arg` (1282-1553).
//! * `+K` — `erts/emulator/sys/common/erl_check_io.c:2467`: the emulator only
//!   `get_arg`s the value; there is **no** validation. `beam_rt` accepts the two
//!   documented literals instead (a documented divergence).
//!
//! # Documented divergences (oracle exit 1 vs `beam_rt` exit 2)
//!
//! The oracle prints `bad …` + its usage text and exits **1** for a malformed
//! emulator value; `beam_rt` keeps its uniform four-code contract and reports a
//! **usage** error (exit 2 + [`USAGE`](crate::cli::USAGE)) instead. The C's numeric
//! laxness (`ErtsStrToSint`, which silently ignores trailing garbage) is not
//! reproduced either — `+Q 1024abc` is rejected here and accepted by the oracle.
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
    argument.starts_with("-M") || argument.starts_with("+M")
}

/// `+Q N` — the maximum number of ports: `legacy`, or an integer in
/// `[ERTS_MIN_PORTS, ERTS_MAX_PORTS]` (`erl_init.c:1888-1905`).
pub(super) fn parse_max_ports(switch: &str, value: &str) -> Result<()> {
    if value == "legacy" {
        return Ok(());
    }
    match value.parse::<i64>() {
        Ok(number) if (ERTS_MIN_PORTS..=ERTS_MAX_PORTS).contains(&number) => Ok(()),
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

/// `+K true|false` — the kernel-poll switch. The emulator itself does not validate
/// the value (`erl_check_io.c:2467`); this runtime accepts the two documented
/// literals and rejects anything else (a documented divergence, above).
pub(super) fn parse_kernel_poll(switch: &str, value: &str) -> Result<()> {
    match value {
        "true" | "false" => Ok(()),
        _ => Err(error::bad_switch_value(switch, value)),
    }
}

/// The nine `+sbt` scheduler bind types (`erl_cpu_topology.c:628-647`).
pub(super) const SCHEDULER_BIND_TYPES: [&str; 9] =
    ["u", "db", "s", "ps", "ts", "tnnps", "nnps", "nnts", "ns"];

/// `+sbt <type>` — the scheduler bind type (`erl_cpu_topology.c:623-647`).
pub(super) fn parse_sbt(switch: &str, value: &str) -> Result<()> {
    if SCHEDULER_BIND_TYPES.contains(&value) {
        Ok(())
    } else {
        Err(error::bad_switch_value(switch, value))
    }
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
/// A `-M*` switch is accepted without validation (the oracle never validates it —
/// see the module docs); only `+M*` reaches [`validate_allocator`].
pub(super) fn parse_allocator(
    switch: &str,
    args: &[String],
    index: usize,
) -> Result<usize> {
    if switch.starts_with("-M") {
        // Parity with `erl`: a `-M*` flag is passed through unvalidated, and its value
        // (if any) is *not* consumed here — `-Mfoo 1` leaves `1` as a stray argument,
        // exactly as the oracle's `add_arg` pass-through does not eat it either.
        return Ok(index + 1);
    }
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
            _ => Err(error::bad_switch_value(switch, "<missing value>")),
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
        'e' => {
            let rest = sub
                .strip_prefix("ea")
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
