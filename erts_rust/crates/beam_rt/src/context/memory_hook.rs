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

//! The runtime's memory accounting for `erlang:memory/0,1` — the
//! [`MemoryHook`] seam implementation.
//!
//! `beam_bifs` registers `erlang:memory/0` and `erlang:memory/1` and owns the whole
//! *rendering* half (the `{Key, Bytes}` tuple/list construction, the requested key
//! order, the `badarg` for an unknown key, the atom interning). What no BIF can derive
//! is the **byte figures** — those are this runtime's own accounting — so the pairs are
//! injected through the optional [`MemoryHook`] seam and this module supplies them.
//!
//! # Nine keys, the oracle's order
//!
//! `erlang:memory/0` reports exactly these nine keys, in this order (verified against
//! the oracle, OTP 29.1.1 / erts-17.1 — see [`MEMORY_KEYS`]):
//!
//! ```text
//! [{total,N},{processes,N},{processes_used,N},{system,N},
//!  {atom,N},{atom_used,N},{binary,N},{code,N},{ets,N}]
//! ```
//!
//! and `total = processes + system`, the oracle's own identity — [`RuntimeMemory`]
//! preserves it by construction, so `total` is always the consistent sum of the two
//! reported top-level parts.
//!
//! # Every figure is measured, never hardcoded
//!
//! A hardcoded constant would be a lie about a different process (the same rule the
//! crate's `allocated_areas`/`{allocator, A}` keys follow — see the Design Decisions
//! in `../CONTEXT.md`). Each figure comes from this runtime's own bookkeeping:
//!
//! | key | source |
//! |---|---|
//! | `processes` | the live processes' collector capacities ([`Process::collector`] via [`beam_gc::HeapObservable`]) |
//! | `processes_used` | the live processes' used young-heap words ([`Process::heap_used_words`]) |
//! | `atom` / `atom_used` | the shared [`AtomTable`]'s index slots plus its name text |
//! | `binary` | `binary_alloc`'s live bytes ([`beam_alloc::registry`]) |
//! | `code` | the loaded modules' `.beam` image sizes, from the [`CodeServer`] |
//! | `ets` | the ETS registry's live tables, at one index word each |
//! | `system` | `atom + binary + code + ets` plus the emulator-side allocators' live bytes |
//!
//! A figure this runtime genuinely has no counterpart for is an honest `0`, never an
//! invented number.
//!
//! # Why the figures are snapshotted at construction
//!
//! The hook must not borrow `&mut Runtime` — the `FunBridge` arm of
//! [`RuntimeContext::call_registered`](super::RuntimeContext::call_registered) already
//! holds one — so [`RuntimeMemory`] is built from the cheap handles and read-only state
//! that are available *before* the call and lives alongside the other seam handles. It
//! is built **before** the shared ETS `RefCell` is borrowed for the call, which is why
//! the ETS figure is captured rather than read lazily: that cell is held mutably for the
//! whole registered call, so a later read would always fail.

use std::cell::RefCell;
use std::rc::Rc;

use beam_alloc::AllocType;
use beam_bifs::{MemoryHook, TableRegistry};
use beam_common::Result;
use beam_gc::HeapObservable;
use beam_process::ProcessTable;
use beam_term::{Atom, AtomTable};

use crate::codeserver::CodeServer;

/// The nine `erlang:memory/0` keys, in the oracle's order.
pub(crate) const MEMORY_KEYS: [&str; 9] = [
    "total",
    "processes",
    "processes_used",
    "system",
    "atom",
    "atom_used",
    "binary",
    "code",
    "ets",
];

/// The runtime's memory accounting behind `erlang:memory/0,1`.
///
/// A cheap, non-runtime-borrowing handle: the shared atom table (a live `Rc`) plus the
/// captured byte figures that need state only reachable before the call
/// (the process table, the code server and the ETS registry).
pub(crate) struct RuntimeMemory {
    /// The runtime's one shared atom table (read live; it is only immutably borrowed
    /// for the duration of the call, so it stays readable).
    atoms: Rc<AtomTable>,
    /// The live processes' heap capacity, in bytes.
    processes: u64,
    /// The part of [`Self::processes`] actually in use, in bytes.
    processes_used: u64,
    /// The loaded modules' code footprint, in bytes.
    code: u64,
    /// The ETS tables' footprint, in bytes.
    ets: u64,
}

impl RuntimeMemory {
    /// Snapshots the parts that need state reachable only outside the BIF call.
    ///
    /// `atoms` is kept as a live handle; `processes`, `code_server` and `tables` are
    /// read once here (see the module docs for why the ETS figure cannot be read
    /// lazily).
    pub(crate) fn new(
        atoms: Rc<AtomTable>,
        processes: &ProcessTable,
        code_server: &CodeServer,
        tables: &Rc<RefCell<TableRegistry>>,
    ) -> RuntimeMemory {
        let word = WORD_BYTES;
        let mut process_bytes = 0u64;
        let mut process_used = 0u64;
        for pid in processes.pids() {
            if let Some(process) = processes.get(pid) {
                process_bytes += process.collector().total_heap_size_words() as u64 * word;
                process_used += process.heap_used_words() as u64 * word;
            }
        }
        RuntimeMemory {
            atoms,
            processes: process_bytes,
            processes_used: process_used,
            code: code_bytes(code_server),
            // `try_borrow` (never `borrow`): an unexpected outer borrow degrades to an
            // honest `0`, never a panic. The cell is free here in every real path (a
            // fun-taking BIF does not install the tables, see `call_registered`).
            ets: match tables.try_borrow() {
                Ok(registry) => registry.len() as u64 * word,
                Err(_) => 0,
            },
        }
    }
}

impl MemoryHook for RuntimeMemory {
    fn memory(&mut self) -> Result<Vec<(&'static str, u64)>> {
        let atom = atom_bytes(&self.atoms);
        let binary = allocator_live_bytes(|ty| *ty == AllocType::Binary);
        // The emulator-side allocators: everything the runtime allocates *outside* a
        // process heap. `std_alloc` backs the process heaps here (`beam_alloc::Heap`'s
        // default allocator — already counted in `processes`) and `eheap_alloc` is the
        // nominal process-heap allocator, so both are excluded to avoid counting the
        // same memory twice; `binary_alloc` is reported as `binary` itself.
        let emulator = allocator_live_bytes(|ty| {
            !matches!(*ty, AllocType::Binary | AllocType::Standard | AllocType::EHeap)
        });
        // The oracle's identity: `total` is the sum of the two top-level parts.
        let system = atom + binary + self.code + self.ets + emulator;
        let total = self.processes + system;
        // The nine figures, in [`MEMORY_KEYS`] order — the key set and its order have
        // exactly one definition. `atom_used = atom` because the table is a dense index:
        // every slot is live, exactly as in the oracle (where
        // `erlang:memory(atom) =:= erlang:memory(atom_used)`).
        let figures: [u64; 9] = [
            total,
            self.processes,
            self.processes_used,
            system,
            atom,
            atom,
            binary,
            self.code,
            self.ets,
        ];
        Ok(MEMORY_KEYS.into_iter().zip(figures).collect())
    }
}

/// The byte width of one term word (`erts` words are pointer-sized).
const WORD_BYTES: u64 = std::mem::size_of::<usize>() as u64;

/// The shared atom table's footprint in bytes: one index slot per interned atom
/// (`Vec` slot + `HashMap` entry) plus the name text itself.
///
/// The index is a dense vector, so every slot is allocated; the name text is what the
/// table actually stores per entry.
fn atom_bytes(atoms: &AtomTable) -> u64 {
    let mut bytes = WORD_BYTES * atoms.len() as u64;
    for index in 0..atoms.len() {
        if let Some(name) = atoms.name(Atom::from_raw(index as u32)) {
            bytes += name.len() as u64;
        }
    }
    bytes
}

/// The total `live_bytes` of the registered allocators selected by `include`
/// ([`beam_alloc::registry::all_stats`]).
fn allocator_live_bytes(include: impl Fn(&AllocType) -> bool) -> u64 {
    beam_alloc::registry::all_stats()
        .into_iter()
        .filter(|(ty, _)| include(ty))
        .map(|(_, stats)| stats.live_bytes as u64)
        .sum()
}

/// The loaded modules' code footprint in bytes: the size of each module's `.beam`
/// image, as the code server recorded the path it was loaded from.
///
/// A module supplied in memory has no path (and a `.beam` that cannot be stat'ed adds
/// nothing) — an honest `0` for that module rather than an invented size.
fn code_bytes(code_server: &CodeServer) -> u64 {
    let mut bytes = 0u64;
    for name in code_server.loaded() {
        if let Some(path) = code_server.loaded_path(&name) {
            if let Ok(meta) = std::fs::metadata(path) {
                bytes += meta.len();
            }
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_engine::{terms, ExecutionContext};
    use beam_term::{atom_val, signed_value};

    use crate::config::RuntimeConfig;
    use crate::context::RuntimeContext;
    use crate::runtime::Runtime;

    fn runtime() -> Runtime {
        Runtime::new(RuntimeConfig::default()).expect("the default runtime")
    }

    /// `erlang:memory/0` through the real runtime glue, as `(name, bytes)` pairs.
    fn memory_0(rt: &mut Runtime) -> Vec<(String, i64)> {
        let mut ctx = RuntimeContext::new(rt);
        let term = ctx
            .call_bif("erlang", "memory", 0, &[])
            .expect("erlang:memory/0");
        pairs(&ctx, term)
    }

    /// The `{Key, Bytes}` pairs of a `memory/0,1` result, named through the shared table.
    fn pairs(ctx: &RuntimeContext<'_>, term: beam_term::Eterm) -> Vec<(String, i64)> {
        let atoms = ctx.runtime.atoms();
        terms::list_elements(term)
            .expect("a list")
            .into_iter()
            .map(|pair| {
                let key = terms::tuple_element(pair, 0).expect("a key element");
                let value = terms::tuple_element(pair, 1).expect("a value element");
                (
                    atoms.name(atom_val(key)).expect("a named key").to_string(),
                    signed_value(value),
                )
            })
            .collect()
    }

    #[test]
    fn memory_0_reports_the_nine_keys_in_the_oracle_order() {
        let mut rt = runtime();
        let reported = memory_0(&mut rt);
        let names: Vec<&str> = reported.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, MEMORY_KEYS);
    }

    #[test]
    fn memory_0_total_is_the_sum_of_its_two_top_level_parts() {
        let mut rt = runtime();
        let reported = memory_0(&mut rt);
        let get = |key: &str| {
            reported
                .iter()
                .find(|(name, _)| name == key)
                .unwrap_or_else(|| panic!("{key} is reported"))
                .1
        };
        assert_eq!(get("total"), get("processes") + get("system"));
        // The sub-parts never exceed their parent.
        assert!(get("processes_used") <= get("processes"));
        assert!(get("atom_used") <= get("atom"));
        // Every figure is a non-negative small integer (they are byte counts).
        for (name, bytes) in &reported {
            assert!(*bytes >= 0, "{name} must be non-negative, got {bytes}");
        }
    }

    #[test]
    fn memory_1_follows_the_requested_order_and_returns_a_plain_integer() {
        let mut rt = runtime();
        let mut ctx = RuntimeContext::new(&mut rt);
        let total = ctx.atom("total").expect("total");
        let single = ctx
            .call_bif("erlang", "memory", 1, &[total])
            .expect("erlang:memory(total)");
        assert!(
            beam_term::is_small(single) && signed_value(single) >= 0,
            "memory(atom) is a plain non-negative integer, got {single:?}"
        );

        let ets = ctx.atom("ets").expect("ets");
        let atom = ctx.atom("atom").expect("atom");
        let requested = terms::make_list(&mut ctx, &[ets, atom]).expect("the request list");
        let ordered = ctx
            .call_bif("erlang", "memory", 1, &[requested])
            .expect("erlang:memory([ets,atom])");
        let named = pairs(&ctx, ordered);
        assert_eq!(
            named.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            ["ets", "atom"],
            "memory/1 follows the requested key order"
        );
    }

    #[test]
    fn the_ets_figure_tracks_the_live_table_registry() {
        // Two `ets:new/2` tables raise the ETS figure by two index words — the same
        // per-live-table measure `system_info(allocated_areas)`'s `ets_misc` uses.
        let mut rt = runtime();
        let before = memory_0(&mut rt)
            .into_iter()
            .find(|(name, _)| name == "ets")
            .expect("ets")
            .1;
        let mut ctx = RuntimeContext::new(&mut rt);
        for _ in 0..2 {
            let name = ctx.atom("mytab").expect("mytab");
            let opts = terms::make_list(&mut ctx, &[]).expect("[]");
            ctx.call_bif("ets", "new", 2, &[name, opts])
                .expect("ets:new/2");
        }
        drop(ctx);
        let after = memory_0(&mut rt)
            .into_iter()
            .find(|(name, _)| name == "ets")
            .expect("ets")
            .1;
        assert_eq!(
            after - before,
            2 * WORD_BYTES as i64,
            "two live tables add two index words"
        );
    }

    /// A loud skip: a missing oracle is never a silent pass.
    fn skip_oracle(why: &str) {
        eprintln!();
        eprintln!("=============== beam_rt erlang:memory SKIPPED ===============");
        eprintln!("{why}: the erlang:memory key-order comparison cannot run.");
        eprintln!("Set $ERTS_ORACLE (or install Erlang/OTP) and re-run. NOT a pass.");
        eprintln!("=============================================================");
        eprintln!();
    }

    /// The oracle's probe output, one parsed Erlang list per printed line.
    fn oracle_lists(eval: &str) -> Vec<Vec<String>> {
        let Some(oracle) = super::super::test_support::resolve_oracle() else {
            skip_oracle("no `erl` on $PATH");
            return Vec::new();
        };
        let output = std::process::Command::new(&oracle)
            .arg("-noshell")
            .arg("-eval")
            .arg(eval)
            .arg("-s")
            .arg("init")
            .arg("stop")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
        assert!(output.status.success(), "the oracle failed: {output:?}");
        let stdout = String::from_utf8(output.stdout).expect("the oracle prints UTF-8");
        stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| {
                // Each probe prints one Erlang list `[a,b,...]`; strip the brackets and
                // split on the commas (the key atoms contain no commas).
                assert!(
                    line.starts_with('[') && line.ends_with(']'),
                    "the oracle did not print a list: {line:?}"
                );
                line[1..line.len() - 1]
                    .split(',')
                    .map(|name| name.trim().to_string())
                    .filter(|name| !name.is_empty())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn the_key_order_matches_the_live_oracle() {
        let oracle = oracle_lists(
            "io:format(\"~p~n\", [[K || {K, _} <- erlang:memory()]]), \
             io:format(\"~p~n\", [[K || {K, _} <- erlang:memory([ets, atom])]]), \
             io:format(\"~p~n\", [[is_integer(erlang:memory(total))]])",
        );
        if oracle.is_empty() {
            return; // loud skip already printed
        }
        assert_eq!(oracle.len(), 3, "three probe lines: {oracle:?}");

        // Our own `memory/0` key order, in the same shape.
        let mut rt = runtime();
        let ours: Vec<String> = memory_0(&mut rt)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(ours, oracle[0], "our key order matches the oracle's");

        // `memory([ets,atom])` follows the requested order in both runtimes, and the
        // oracle's single-atom form is an integer.
        assert_eq!(oracle[1], ["ets", "atom"]);
        assert_eq!(oracle[2], ["true"]);
    }
}
