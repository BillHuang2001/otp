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

//! Crash dumps (contract A.8): the ERTS-format crash-dump writer behind `erlang:halt/…`
//! (the string / `abort` shapes) and the `ERL_CRASH_DUMP` / `ERL_CRASH_DUMP_SECONDS`
//! environment path.
//!
//! The emitter reproduces the oracle's **section vocabulary and order** exactly as
//! documented in the mandate: `=erl_crash_dump:0.5`, the ctime line, `Slogan:`,
//! `System version:`, `Taints:`, `Atoms:`, `Calling Thread:`, `=memory`,
//! `=hash_table:atom_tab`, `=index_table:atom_tab`, `=allocated_areas`, one `=proc:<pid>`
//! block per process followed by `=proc_messages:<pid>`, an optional
//! `=proc_dictionary:<pid>`, `=proc_stack:<pid>` and `=proc_heap:<pid>`, then
//! `=node:'<n>'`/`=no_distribution`, `=loaded_modules`, one `=mod:<name>` per module, the
//! `=atoms` list and `=end`. There is deliberately **no** `=proc_memory` section and no
//! `=erl_crash_dump_config` tag: neither exists anywhere in OTP (`crashdump_viewer.erl`
//! warns on unknown tags), so inventing them would be a lie about the format.
//!
//! # Two documented beam_rt extensions of the oracle's raw-word format
//!
//! The oracle is C and dumps the *raw heap words* of a process. This crate is
//! `#![deny(unsafe_code)]`, so a scheduler-side module cannot read another process's heap
//! payload words. Two consequences are documented rather than papered over:
//!
//! * `=proc_stack:<pid>` renders the process's **engine X registers** (reachable through
//!   the runtime's read-only runner seam); `=proc_heap:<pid>` renders the process-owned
//!   **boxed terms** (dictionary values and mailbox payloads) it can reach safely.
//! * a term is rendered **line-oriented**: an *immediate* as `=I<hex>` (its tagged word)
//!   and a *boxed* term as `=<words>:<hex>` where `<words>` is its flat word count
//!   ([`beam_term::size_object`]) and `<hex>` is its tagged root word. The oracle prints
//!   the full payload word range; that range is unreachable here without `unsafe`.
//!
//! Every numeric section is derived from **this runtime's own** counters, never from a
//! hardcoded oracle figure: [`snapshot`] reads `system_info/1` (the `allocated_areas`,
//! `system_version`, count keys), the process table and the atom table.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use beam_common::{BeamError, Result};
use beam_term::{is_immediate, size_object, Eterm};

use crate::cli::ErlEnv;
use crate::info::{self, InfoValue};
use crate::runtime::Runtime;

/// One process's entry in the dump (`=proc:<pid>` plus its four companion sections).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcessSection {
    /// The pid, rendered `<0.Number.Serial>`.
    pub pid: String,
    /// The scheduler-visible state (`Running`/`Scheduled`/`Waiting`/`Suspended`/…).
    pub state: String,
    /// The locally registered name, when the process has one.
    pub registered: Option<String>,
    /// What the process was spawned as (its module export, when known).
    pub spawned_as: String,
    /// Who spawned it (`<unknown>` where this runtime does not track it).
    pub spawned_by: String,
    /// The mailbox depth.
    pub message_queue_len: usize,
    /// Reductions consumed.
    pub reductions: u64,
    /// The heap capacity in words (`Stack+heap:`).
    pub stack_heap: usize,
    /// The heap's unused words (`Heap unused:`).
    pub heap_unused: usize,
    /// The heap footprint in bytes (`Memory:`).
    pub memory: usize,
    /// The queued messages, rendered one per line (`=proc_messages`).
    pub messages: Vec<String>,
    /// The process dictionary, `(key, value)` rendered (`=proc_dictionary`).
    pub dictionary: Vec<(String, String)>,
    /// The live X registers, rendered (`=proc_stack`).
    pub stack: Vec<String>,
    /// The reachable boxed terms, rendered (`=proc_heap`).
    pub heap: Vec<String>,
}

/// A `{Name, Size, Used, Objs, Depth}` hash-table row (`=hash_table:atom_tab`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HashTableSection {
    /// The table's name.
    pub name: String,
    /// The slot count.
    pub size: usize,
    /// Used slots.
    pub used: usize,
    /// Live objects.
    pub objs: usize,
    /// The bucket-tree depth.
    pub depth: usize,
}

/// A `{Name, Size, Limit, Entries}` index-table row (`=index_table:atom_tab`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexTableSection {
    /// The table's name.
    pub name: String,
    /// The slot count.
    pub size: usize,
    /// The grow limit.
    pub limit: usize,
    /// Live entries.
    pub entries: usize,
}

/// One `=allocated_areas` line: a name and its integer value(s).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AreaRow {
    /// The area's name.
    pub name: String,
    /// Its value; `atom_space` carries two.
    pub values: Vec<i64>,
}

/// One loaded module (`=mod:<name>`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModuleSection {
    /// The module name.
    pub name: String,
    /// The module's function count (this runtime's own figure).
    pub functions: usize,
}

/// The complete, borrow-free content of one crash dump.
///
/// Built by [`snapshot`] and rendered by [`write_dump`]; holding it as plain data keeps the
/// emitter a pure function of its input and makes every section unit-testable.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DumpInput {
    /// The slogan (empty for an abnormal exit, the `halt("Slogan")` argument otherwise).
    pub slogan: String,
    /// The requested write budget (`ERL_CRASH_DUMP_SECONDS`); recorded, not enforced — the
    /// dump is built in memory and written in one call, so there is no partial state a
    /// budget would bound.
    pub seconds: Option<i64>,
    /// The `ctime`-style timestamp line.
    pub ctime: String,
    /// The system version (`system_info(system_version)`).
    pub system_version: String,
    /// The atom count.
    pub atom_count: usize,
    /// The `=memory` rows, in the oracle's order.
    pub memory: Vec<(String, i64)>,
    /// The `=hash_table:atom_tab` row.
    pub hash_table: HashTableSection,
    /// The `=index_table:atom_tab` row.
    pub index_table: IndexTableSection,
    /// The `=allocated_areas` rows.
    pub allocated_areas: Vec<AreaRow>,
    /// The per-process sections, in `ProcessTable::pids()` order.
    pub processes: Vec<ProcessSection>,
    /// The node name, or `None` for `=no_distribution`.
    pub node: Option<String>,
    /// The loaded modules.
    pub modules: Vec<ModuleSection>,
    /// The atom names, in table order.
    pub atoms: Vec<String>,
}

/// The path a crash dump is written to, or `None` when dumping is disabled.
///
/// `ERL_CRASH_DUMP` names the file; unset (or empty) is the emulator's `erl_crash.dump` in
/// the working directory. `ERL_CRASH_DUMP_SECONDS == 0` disables dumping entirely
/// (`erl_crash_dump.c:451-457`), which is the one value that makes this function return
/// `None`.
pub fn crash_dump_path(env: &ErlEnv) -> Option<PathBuf> {
    if env.crash_dump_seconds == Some(0) {
        return None;
    }
    match env.crash_dump.as_deref() {
        Some(name) if !name.is_empty() => Some(PathBuf::from(name)),
        _ => Some(PathBuf::from("erl_crash.dump")),
    }
}

/// Renders `dump` and writes it to `path` in one shot.
///
/// # Errors
///
/// [`BeamError::Io`] when the file cannot be written (an unwritable directory, a missing
/// parent, …). The caller treats a failed write as best-effort: the process still exits
/// with the status the crash implies.
pub fn write_dump(path: &Path, dump: &DumpInput) -> Result<()> {
    std::fs::write(path, render(dump)).map_err(BeamError::Io)
}

/// Snapshots `rt` into a [`DumpInput`].
///
/// # Errors
///
/// A typed error only when a `system_info/1` read backing the dump fails (an already-borrowed
/// table registry, say); every other figure is derived from state that always answers.
pub fn snapshot(rt: &Runtime, slogan: &str, seconds: Option<i64>) -> Result<DumpInput> {
    let atom_count = info_int(rt, "atom_count").unwrap_or(0) as usize;
    let atom_names: Vec<String> = (0..atom_count)
        .filter_map(|index| rt.atoms().name(beam_term::Atom::from_raw(index as u32)))
        .map(str::to_string)
        .collect();
    let system_version = match info::system_info_value("system_version", rt) {
        Ok(InfoValue::Str(text)) => text,
        _ => "unknown".to_string(),
    };

    let mut memory = Vec::new();
    let mut processes = Vec::new();
    let mut process_bytes: i64 = 0;
    let mut process_used: i64 = 0;
    for pid in rt.processes().pids() {
        let Some(process) = rt.processes().get(pid) else {
            continue;
        };
        process_bytes += (process.heap().capacity() as i64) * 8;
        process_used += (process.heap().used() as i64) * 8;
        processes.push(process_section(rt, pid)?);
    }

    let slot = std::mem::size_of::<usize>() as i64;
    let ports = info_int(rt, "port_count").unwrap_or(0);
    let ets = info_int(rt, "ets_count").unwrap_or(0);
    let modules: Vec<ModuleSection> = rt
        .loaded_modules()
        .into_iter()
        .map(|name| ModuleSection {
            functions: rt.module(&name).map(|m| m.functions.len()).unwrap_or(0),
            name,
        })
        .collect();
    let code = slot * modules.len() as i64;
    let atom_bytes = slot * atom_count as i64;
    let ets_bytes = slot * ets;
    let port_bytes = slot * ports;
    let system = atom_bytes + code + ets_bytes + port_bytes;
    for (key, value) in [
        ("total", system + process_bytes),
        ("processes", process_bytes),
        ("processes_used", process_used),
        ("system", system),
        ("atom", atom_bytes),
        ("atom_used", atom_bytes),
        ("binary", 0),
        ("code", code),
        ("ets", ets_bytes),
    ] {
        memory.push((key.to_string(), value));
    }

    let size = atom_count.next_power_of_two().max(1);
    let hash_table = HashTableSection {
        name: "atom_tab".to_string(),
        size,
        used: atom_count,
        objs: atom_count,
        depth: size.trailing_zeros() as usize,
    };
    let index_table = IndexTableSection {
        name: "atom_tab".to_string(),
        size,
        limit: info_int(rt, "atom_limit").unwrap_or(0) as usize,
        entries: atom_count,
    };

    let allocated_areas = match info::system_info_value("allocated_areas", rt) {
        Ok(InfoValue::List(rows)) => rows.iter().filter_map(area_row).collect(),
        _ => Vec::new(),
    };

    Ok(DumpInput {
        slogan: slogan.to_string(),
        seconds,
        ctime: ctime_now(),
        system_version,
        atom_count,
        memory,
        hash_table,
        index_table,
        allocated_areas,
        processes,
        node: rt.node_name().map(str::to_string),
        modules,
        atoms: atom_names,
    })
}

/// Builds one process's section, reading its state, mailbox, dictionary and engine.
fn process_section(rt: &Runtime, pid: beam_process::ProcessId) -> Result<ProcessSection> {
    let process = rt
        .processes()
        .get(pid)
        .ok_or_else(|| BeamError::BadArg)?;
    let registered = rt
        .processes()
        .registered()
        .find(|(_, owner)| *owner == pid)
        .and_then(|(atom, _)| rt.atoms().name(atom))
        .map(str::to_string);
    let spawned_as = match rt.runner_state().engine_at(pid) {
        Some((module, _)) => module
            .atom_name(module.name)
            .map(|name| format!("{name}:<export>"))
            .unwrap_or_else(|| "<unknown>".to_string()),
        None => "<unknown>".to_string(),
    };
    let messages = process
        .mailbox()
        .iter()
        .enumerate()
        .map(|(index, message)| {
            format!(
                "{index}: from {} payload {}",
                render_term(message.from()),
                render_term(message.payload())
            )
        })
        .collect();
    let dictionary: Vec<(String, String)> = process
        .dict()
        .entries()
        .map(|(key, value)| (render_term(key), render_term(value)))
        .collect();
    let mut heap: Vec<String> = process
        .dict()
        .entries()
        .map(|(_, value)| render_term(value))
        .collect();
    heap.extend(process.mailbox().iter().map(|m| render_term(m.payload())));
    let stack = register_lines(rt, pid);

    Ok(ProcessSection {
        pid: render_pid(pid),
        state: state_name(process.state()).to_string(),
        registered,
        spawned_as,
        spawned_by: "<unknown>".to_string(),
        message_queue_len: process.mailbox().len(),
        reductions: process.reductions(),
        stack_heap: process.heap().capacity(),
        heap_unused: process.heap().available(),
        memory: process.heap().capacity() * std::mem::size_of::<Eterm>(),
        messages,
        dictionary,
        stack,
        heap,
    })
}

/// The process's live X registers, rendered as terms.
fn register_lines(rt: &Runtime, pid: beam_process::ProcessId) -> Vec<String> {
    let Some((_, machine)) = rt.runner_state().engine_at(pid) else {
        return Vec::new();
    };
    (0..64)
        .map_while(|index| machine.x(index).map(|term| format!("x{index} {}", render_term(term))))
        .collect()
}

/// The oracle's `State:` spelling for a scheduler state.
fn state_name(state: beam_process::ProcessState) -> &'static str {
    use beam_process::ProcessState::*;
    match state {
        Runnable => "Scheduled",
        Running => "Running",
        Waiting => "Waiting",
        Suspended => "Suspended",
        Exiting => "Exiting",
        Terminated => "Terminated",
    }
}

/// A pid's `<0.Number.Serial>` rendering.
fn render_pid(pid: beam_process::ProcessId) -> String {
    format!("<0.{}.{}>", pid.number(), pid.serial())
}

/// One term, line-oriented (see the module docs).
fn render_term(term: Eterm) -> String {
    if is_immediate(term) {
        format!("=I{:x}", term.raw())
    } else {
        format!("={}:{:x}", size_object(term).max(1), term.raw())
    }
}

/// A single `=allocated_areas` line from one `InfoValue` row.
fn area_row(value: &InfoValue) -> Option<AreaRow> {
    let InfoValue::Tupled(items) = value else {
        return None;
    };
    let mut iter = items.iter();
    let InfoValue::Atom(name) = iter.next()? else {
        return None;
    };
    let values = iter
        .filter_map(|item| match item {
            InfoValue::Int(value) => Some(*value),
            _ => None,
        })
        .collect();
    Some(AreaRow {
        name: (*name).to_string(),
        values,
    })
}

/// `system_info(Key)` as an integer, or `None` when the key does not answer with one.
fn info_int(rt: &Runtime, key: &str) -> Option<i64> {
    match info::system_info_value(key, rt) {
        Ok(InfoValue::Int(value)) => Some(value),
        _ => None,
    }
}

/// The whole dump as text (newline-terminated lines, no blank lines between sections).
fn render(dump: &DumpInput) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "=erl_crash_dump:0.5");
    let _ = writeln!(out, "{}", dump.ctime);
    let _ = writeln!(out, "Slogan: {}", dump.slogan);
    let _ = writeln!(out, "System version: {}", dump.system_version);
    let _ = writeln!(out, "Taints: ");
    let _ = writeln!(out, "Atoms: {}", dump.atom_count);
    let _ = writeln!(out, "Calling Thread: main");
    let _ = writeln!(out, "=memory");
    for (key, value) in &dump.memory {
        let _ = writeln!(out, "{key}: {value}");
    }
    let _ = writeln!(out, "=hash_table:{}", dump.hash_table.name);
    let _ = writeln!(out, "size: {}", dump.hash_table.size);
    let _ = writeln!(out, "used: {}", dump.hash_table.used);
    let _ = writeln!(out, "objs: {}", dump.hash_table.objs);
    let _ = writeln!(out, "depth: {}", dump.hash_table.depth);
    let _ = writeln!(out, "=index_table:{}", dump.index_table.name);
    let _ = writeln!(out, "size: {}", dump.index_table.size);
    let _ = writeln!(out, "limit: {}", dump.index_table.limit);
    let _ = writeln!(out, "entries: {}", dump.index_table.entries);
    let _ = writeln!(out, "=allocated_areas");
    for row in &dump.allocated_areas {
        let values = row
            .values
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        if values.is_empty() {
            let _ = writeln!(out, "{}:", row.name);
        } else {
            let _ = writeln!(out, "{}: {values}", row.name);
        }
    }
    for process in &dump.processes {
        let _ = writeln!(out, "=proc:{}", process.pid);
        let _ = writeln!(out, "State: {}", process.state);
        if let Some(name) = &process.registered {
            let _ = writeln!(out, "Registered name: {name}");
        }
        let _ = writeln!(out, "Spawned as: {}", process.spawned_as);
        let _ = writeln!(out, "Spawned by: {}", process.spawned_by);
        let _ = writeln!(
            out,
            "Message queue length: {}",
            process.message_queue_len
        );
        let _ = writeln!(out, "Reductions: {}", process.reductions);
        let _ = writeln!(out, "Stack+heap: {}", process.stack_heap);
        let _ = writeln!(out, "Heap unused: {}", process.heap_unused);
        let _ = writeln!(out, "Memory: {}", process.memory);
        let _ = writeln!(out, "=proc_messages:{}", process.pid);
        for line in &process.messages {
            let _ = writeln!(out, "{line}");
        }
        if !process.dictionary.is_empty() {
            let _ = writeln!(out, "=proc_dictionary:{}", process.pid);
            for (key, value) in &process.dictionary {
                let _ = writeln!(out, "{key} => {value}");
            }
        }
        let _ = writeln!(out, "=proc_stack:{}", process.pid);
        for line in &process.stack {
            let _ = writeln!(out, "{line}");
        }
        let _ = writeln!(out, "=proc_heap:{}", process.pid);
        for line in &process.heap {
            let _ = writeln!(out, "{line}");
        }
    }
    match &dump.node {
        Some(node) => {
            let _ = writeln!(out, "=node:'{node}'");
        }
        None => {
            let _ = writeln!(out, "=no_distribution");
        }
    }
    let _ = writeln!(out, "=loaded_modules");
    let _ = writeln!(out, "Current code: {} modules", dump.modules.len());
    for module in &dump.modules {
        let _ = writeln!(out, "=mod:{}", module.name);
        let _ = writeln!(out, "Current size: {}", module.functions);
    }
    let _ = writeln!(out, "=atoms");
    for atom in &dump.atoms {
        let _ = writeln!(out, "'{atom}'");
    }
    let _ = writeln!(out, "=end");
    out
}

/// The current time as a `ctime`-style string (`Wed Jun 11 12:34:56 2025`), in UTC.
fn ctime_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_secs() as i64)
        .unwrap_or(0);
    format_ctime(seconds)
}

/// Formats Unix `seconds` as a `ctime`-style string in UTC.
fn format_ctime(seconds: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let weekday = DAYS[(days + 4).rem_euclid(7) as usize];
    let month_name = MONTHS[(month - 1) as usize];
    format!(
        "{weekday} {month_name} {day:2} {:02}:{:02}:{:02} {year}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Converts days since the Unix epoch to `(year, month, day)` (Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DumpInput {
        DumpInput {
            slogan: "probe".to_string(),
            atom_count: 3,
            system_version: "17.1".to_string(),
            ctime: "Thu Jan  1 00:00:00 1970".to_string(),
            processes: vec![ProcessSection {
                pid: "<0.1.0>".to_string(),
                state: "Waiting".to_string(),
                ..ProcessSection::default()
            }],
            atoms: vec!["a".to_string(), "b".to_string()],
            ..DumpInput::default()
        }
    }

    #[test]
    fn the_header_and_the_end_marker_are_present() {
        let text = render(&sample());
        assert!(text.starts_with("=erl_crash_dump:0.5\n"));
        assert!(text.ends_with("=end\n"));
        assert!(text.contains("Slogan: probe\n"));
        assert!(text.contains("System version: 17.1\n"));
        assert!(text.contains("Taints: \n"));
        assert!(text.contains("Atoms: 3\n"));
    }

    #[test]
    fn every_section_tag_is_well_formed() {
        // A section header is a line that starts with `=`; each must be one of the
        // documented tags, and there must be no `=proc_memory`/`=erl_crash_dump_config`.
        let text = render(&sample());
        let mut sections = 0usize;
        for line in text.lines() {
            let Some(rest) = line.strip_prefix('=') else {
                continue;
            };
            sections += 1;
            assert!(
                rest.starts_with("erl_crash_dump")
                    || rest.starts_with("memory")
                    || rest.starts_with("hash_table:")
                    || rest.starts_with("index_table:")
                    || rest.starts_with("allocated_areas")
                    || rest.starts_with("proc:")
                    || rest.starts_with("proc_messages:")
                    || rest.starts_with("proc_dictionary:")
                    || rest.starts_with("proc_stack:")
                    || rest.starts_with("proc_heap:")
                    || rest.starts_with("node:")
                    || rest.starts_with("no_distribution")
                    || rest.starts_with("loaded_modules")
                    || rest.starts_with("mod:")
                    || rest.starts_with("atoms")
                    || rest.starts_with("end"),
                "unexpected section tag: {line}"
            );
            assert!(!rest.starts_with("proc_memory"), "no =proc_memory section");
        }
        assert!(sections >= 6, "the sample emits several sections: {text}");
    }

    #[test]
    fn the_proc_section_count_matches_the_snapshot() {
        let mut dump = sample();
        dump.processes.push(ProcessSection {
            pid: "<0.2.0>".to_string(),
            ..ProcessSection::default()
        });
        let text = render(&dump);
        let procs = text
            .lines()
            .filter(|line| line.starts_with("=proc:") && !line.starts_with("=proc_"))
            .count();
        assert_eq!(procs, dump.processes.len());
    }

    #[test]
    fn the_path_honours_the_environment() {
        // Unset: the emulator default.
        assert_eq!(
            crash_dump_path(&ErlEnv::default()),
            Some(PathBuf::from("erl_crash.dump"))
        );
        // Named file.
        let named = ErlEnv {
            crash_dump: Some("/tmp/named.dump".to_string()),
            ..ErlEnv::default()
        };
        assert_eq!(
            crash_dump_path(&named),
            Some(PathBuf::from("/tmp/named.dump"))
        );
        // A seconds budget of 2 still dumps (the budget is a bound, not a switch).
        let budgeted = ErlEnv {
            crash_dump_seconds: Some(2),
            ..ErlEnv::default()
        };
        assert!(crash_dump_path(&budgeted).is_some());
        // `ERL_CRASH_DUMP_SECONDS=0` disables dumping outright.
        let disabled = ErlEnv {
            crash_dump: Some("/tmp/named.dump".to_string()),
            crash_dump_seconds: Some(0),
            ..ErlEnv::default()
        };
        assert_eq!(crash_dump_path(&disabled), None);
    }

    #[test]
    fn immediates_and_boxed_terms_render_in_hex() {
        // An immediate is rendered as its **tagged word** in hex (module docs): the small
        // integer 7 is `(7 << 4) | TAG_IMMED1_SMALL` = 0x7f.
        assert_eq!(render_term(beam_term::make_small(7)), "=I7f");
        assert_eq!(render_term(beam_term::make_small(0)), "=If");
        assert!(render_term(beam_term::NIL).starts_with("=I"));
    }

    #[test]
    fn the_ctime_formatter_reproduces_a_known_instant() {
        // 0 = Thu Jan 1 00:00:00 1970 UTC.
        assert_eq!(format_ctime(0), "Thu Jan  1 00:00:00 1970");
        // 1_000_000_000 = Sun Sep 9 01:46:40 2001 UTC.
        assert_eq!(format_ctime(1_000_000_000), "Sun Sep  9 01:46:40 2001");
    }
}
