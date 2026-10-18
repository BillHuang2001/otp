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

//! The authoritative, code-derived **BIF roster audit** (compatibility contract A.7).
//!
//! `tests/bif_tab.rs` pins the *table* against the registry. This file pins the third
//! leg: the **seam union** — the set of registered BIFs that consume one of the
//! optional [`BifContext`](beam_bifs::BifContext) host seams — and its relationship to
//! the declared surface.
//!
//! Everything here is *derived from the code*, never restated: the union is built by
//! iterating the 14 public tables (`beam_bifs::<NAME>_BIFS`), the declared sets come
//! from [`bif_tab::read_real_bif_tab`], and the registry from
//! [`default_registry`]. The numeric pins below are therefore *witnesses* of the
//! partition the code produces, and a failing count names the direction that drifted.
//!
//! The four derived quantities (after the `FUN_CALLER_BIFS` fix in `src/hook_sets.rs`):
//!
//! | quantity | value | how it is derived |
//! |---|---|---|
//! | declared (`bif.tab`) | 556 | `bif 476 + ubif 73 + hbif 7` |
//! | registry | 648 | 556 declared + 92 stdlib fast paths |
//! | seam union | 217 | the 14 tables, deduped (`221 raw − 4 shared`) |
//! | declared seam-backed | 187 | union members the table declares |
//! | declared in-crate | 369 | `556 − 187`, no table lists them |
//! | undeclared seam members | 30 | the union members registered as stdlib fast paths |
//!
//! "Adopted" is *not* the same as "registered": a union member returns the typed
//! `unsupported_bif` until its host hook is installed. This audit pins only the
//! registry-side facts — which key reads which seam, and how that partitions the
//! declared surface.
use std::collections::{BTreeMap, BTreeSet};

use beam_bifs::bif_tab::{self, BifKind, BifTab, Census};
use beam_bifs::default_registry;
use beam_bifs::{
    needs_dict_hook, needs_export_gate, needs_fun_caller, needs_local_fun_identity,
    needs_local_node, needs_memory_hook, needs_module_info, needs_nif_loader,
    needs_persistent_term, needs_port_hook, needs_process_hook, needs_record_source,
    needs_runtime_hooks, needs_send_hook, DICT_HOOK_BIFS, EXPORT_GATE_BIFS, FUN_CALLER_BIFS,
    LOCAL_FUN_IDENTITY_BIFS, LOCAL_NODE_BIFS, MEMORY_HOOK_BIFS, MODULE_INFO_BIFS, NIF_LOADER_BIFS,
    PERSISTENT_TERM_BIFS, PORT_HOOK_BIFS, PROCESS_HOOK_BIFS, RECORD_SOURCE_BIFS,
    RUNTIME_HOOKS_BIFS, SEND_HOOK_BIFS,
};

/// A `needs_*` predicate.
type Pred = fn(&str, &str, u32) -> bool;

/// The 14 exported seam tables with their predicates, in the canonical partition
/// order: a key listed by two tables is credited to the **first** one here.
const TABLES: &[(&str, &[(&str, &str, u32)], Pred)] = &[
    ("MEMORY_HOOK_BIFS", MEMORY_HOOK_BIFS, needs_memory_hook),
    (
        "LOCAL_FUN_IDENTITY_BIFS",
        LOCAL_FUN_IDENTITY_BIFS,
        needs_local_fun_identity,
    ),
    ("DICT_HOOK_BIFS", DICT_HOOK_BIFS, needs_dict_hook),
    (
        "PROCESS_HOOK_BIFS",
        PROCESS_HOOK_BIFS,
        needs_process_hook,
    ),
    ("PORT_HOOK_BIFS", PORT_HOOK_BIFS, needs_port_hook),
    ("SEND_HOOK_BIFS", SEND_HOOK_BIFS, needs_send_hook),
    ("NIF_LOADER_BIFS", NIF_LOADER_BIFS, needs_nif_loader),
    ("EXPORT_GATE_BIFS", EXPORT_GATE_BIFS, needs_export_gate),
    (
        "RECORD_SOURCE_BIFS",
        RECORD_SOURCE_BIFS,
        needs_record_source,
    ),
    ("MODULE_INFO_BIFS", MODULE_INFO_BIFS, needs_module_info),
    (
        "PERSISTENT_TERM_BIFS",
        PERSISTENT_TERM_BIFS,
        needs_persistent_term,
    ),
    ("FUN_CALLER_BIFS", FUN_CALLER_BIFS, needs_fun_caller),
    ("LOCAL_NODE_BIFS", LOCAL_NODE_BIFS, needs_local_node),
    (
        "RUNTIME_HOOKS_BIFS",
        RUNTIME_HOOKS_BIFS,
        needs_runtime_hooks,
    ),
];

/// The pinned shape of the real `bif.tab` (see `tests/bif_tab.rs`).
const DECLARED_BIF: usize = 476;
const DECLARED_UBIF: usize = 73;
const DECLARED_HBIF: usize = 7;
const DECLARED_TOTAL: usize = 556;
/// The pinned census of the registry against the table.
const REGISTRY_TOTAL: usize = 648;
const FAST_PATHS: usize = 92;
const MISSING_DECLARED: usize = 0;

/// The seam union and the partition of the declared surface it induces.
const SEAM_UNION: usize = 217;
const DECLARED_SEAM_BACKED: usize = 187;
const DECLARED_IN_CRATE: usize = 369;
const UNDECLARED_SEAM_MEMBERS: usize = 30;
/// The keys two tables share — the four external-term encoders, which read the
/// fun-identity **and** the local-node seams.
const SHARED_UNION_KEYS: usize = 4;

/// `(table, total keys, declared members owned by it)`.
///
/// The declared column is a **disjoint** partition of [`DECLARED_SEAM_BACKED`] in
/// [`TABLES`] order; the total column sums to [`SEAM_UNION`] + [`SHARED_UNION_KEYS`].
const TABLE_PINS: &[(&str, usize, usize)] = &[
    ("MEMORY_HOOK_BIFS", 2, 0),
    ("LOCAL_FUN_IDENTITY_BIFS", 4, 4),
    ("DICT_HOOK_BIFS", 7, 7),
    ("PROCESS_HOOK_BIFS", 11, 11),
    ("PORT_HOOK_BIFS", 4, 0),
    ("SEND_HOOK_BIFS", 4, 4),
    ("NIF_LOADER_BIFS", 1, 1),
    ("EXPORT_GATE_BIFS", 1, 1),
    ("RECORD_SOURCE_BIFS", 1, 1),
    ("MODULE_INFO_BIFS", 2, 2),
    ("PERSISTENT_TERM_BIFS", 7, 7),
    ("FUN_CALLER_BIFS", 24, 0),
    ("LOCAL_NODE_BIFS", 6, 2),
    ("RUNTIME_HOOKS_BIFS", 147, 147),
];

/// The declared entries per module, recomputed from the parsed table and pinned in
/// byte order (a `BTreeMap`'s order — `_` sorts before `a`, so `erl_ddll` precedes
/// `erlang`), so an entry mis-filed under `erts_internal` instead of `erlang` (or a
/// dropped line) shows up as two counts moving in opposite directions.
const DECLARED_PER_MODULE: &[(&str, usize)] = &[
    ("atomics", 7),
    ("binary", 22),
    ("code", 7),
    ("erl_ddll", 7),
    ("erl_debugger", 13),
    ("erlang", 268),
    ("error_logger", 1),
    ("erts_debug", 20),
    ("erts_internal", 71),
    ("erts_literal_area_collector", 2),
    ("erts_trace_cleaner", 2),
    ("ets", 51),
    ("file", 1),
    ("io", 1),
    ("lists", 5),
    ("maps", 12),
    ("math", 24),
    ("net_kernel", 1),
    ("os", 10),
    ("persistent_term", 7),
    ("prim_file", 4),
    ("re", 8),
    ("records", 8),
    ("string", 1),
    ("unicode", 3),
];

/// Reads + parses the real table and computes the census, or `None` when this crate is
/// built outside an OTP source tree (then the tests skip **loudly**).
fn census_or_skip() -> Option<(BifTab, Census)> {
    let Some(text) = bif_tab::read_real_bif_tab() else {
        eprintln!(
            "SKIP: {} is not present — run these tests inside the OTP checkout",
            bif_tab::bif_tab_path().display()
        );
        return None;
    };
    let tab = BifTab::parse(&text).expect("the committed bif.tab parses");
    let census = bif_tab::census(&tab, &default_registry());
    Some((tab, census))
}

/// The deduped seam union, sorted — built by iterating the 13 public tables, never
/// hard-coded.
fn union_keys() -> Vec<(String, String, u32)> {
    let mut set: BTreeSet<(String, String, u32)> = BTreeSet::new();
    for (_, table, _) in TABLES {
        for &(module, function, arity) in table.iter() {
            set.insert((module.to_string(), function.to_string(), arity));
        }
    }
    set.into_iter().collect()
}

/// The keys listed by more than one table.
fn shared_union_keys() -> Vec<(String, String, u32)> {
    let mut seen: BTreeSet<(String, String, u32)> = BTreeSet::new();
    let mut shared: BTreeSet<(String, String, u32)> = BTreeSet::new();
    for (_, table, _) in TABLES {
        for &(module, function, arity) in table.iter() {
            let key = (module.to_string(), function.to_string(), arity);
            if !seen.insert(key.clone()) {
                shared.insert(key);
            }
        }
    }
    shared.into_iter().collect()
}

/// The pinned `/arity`-independent key lookup, for assertions that need "is this
/// declared?" without re-parsing.
fn is_declared(tab: &BifTab, key: &(String, String, u32)) -> bool {
    tab.contains(&key.0, &key.1, key.2)
}

#[test]
fn the_declared_surface_and_the_registry_are_pinned() {
    let Some((tab, census)) = census_or_skip() else {
        return;
    };
    let reg = default_registry();

    assert_eq!(
        tab.count(BifKind::Bif),
        DECLARED_BIF,
        "the declared `bif` count changed — update DECLARED_BIF and CONTEXT.md"
    );
    assert_eq!(
        tab.count(BifKind::Ubif),
        DECLARED_UBIF,
        "the declared `ubif` count changed — update DECLARED_UBIF and CONTEXT.md"
    );
    assert_eq!(
        tab.count(BifKind::Hbif),
        DECLARED_HBIF,
        "the declared `hbif` count changed — update DECLARED_HBIF and CONTEXT.md"
    );
    assert_eq!(
        census.declared_total(),
        DECLARED_TOTAL,
        "the declared total changed — update DECLARED_TOTAL and CONTEXT.md"
    );
    assert_eq!(
        census.implemented_count(),
        DECLARED_TOTAL,
        "the registry must implement every declared entry (missing = {})",
        census.missing_count()
    );
    assert_eq!(
        census.missing_count(),
        MISSING_DECLARED,
        "a declared entry stopped being registered ({} missing: {:?})",
        census.missing_count(),
        census.missing_by_module()
    );
    assert_eq!(
        census.fast_path_count(),
        FAST_PATHS,
        "the stdlib fast-path count changed — update FAST_PATHS and CONTEXT.md"
    );
    assert_eq!(
        census.registry_total(),
        REGISTRY_TOTAL,
        "the registry size changed — update REGISTRY_TOTAL and CONTEXT.md"
    );
    assert_eq!(
        reg.len(),
        REGISTRY_TOTAL,
        "the live registry disagrees with the census"
    );
    assert_eq!(
        census.registry_total(),
        reg.len(),
        "declared-implemented + fast paths must partition the registry"
    );
    assert_eq!(
        census.declared_total(),
        census.implemented_count() + census.missing_count(),
        "declared must partition into implemented + missing"
    );
}

#[test]
fn the_seam_union_partitions_the_declared_surface() {
    let Some((tab, census)) = census_or_skip() else {
        return;
    };

    let union = union_keys();
    let raw: usize = TABLES.iter().map(|(_, table, _)| table.len()).sum();
    let shared = shared_union_keys();
    assert_eq!(
        shared.len(),
        SHARED_UNION_KEYS,
        "the tables share {} keys (only the four external-term encoders should overlap \
         LOCAL_FUN_IDENTITY with LOCAL_NODE): {shared:?}",
        shared.len()
    );
    assert_eq!(
        raw - shared.len(),
        union.len(),
        "raw sum {raw} minus the {} shared keys must equal the deduped union",
        shared.len()
    );
    assert_eq!(
        union.len(),
        SEAM_UNION,
        "the seam union moved (raw {raw} − {} shared): a table gained/lost keys — update \
         SEAM_UNION, the TABLE_PINS and CONTEXT.md",
        shared.len()
    );

    // The union keys the table declares are the "seam-backed" declared surface; the rest
    // are served by an unconditional in-crate implementation.
    let declared_seam_backed = union.iter().filter(|key| is_declared(&tab, key)).count();
    let declared_in_crate = census.declared_total() - declared_seam_backed;
    assert_eq!(
        declared_seam_backed,
        DECLARED_SEAM_BACKED,
        "the declared seam-backed count moved (union {}) — update DECLARED_SEAM_BACKED",
        union.len()
    );
    assert_eq!(
        declared_in_crate,
        DECLARED_IN_CRATE,
        "the declared in-crate count moved — update DECLARED_IN_CRATE"
    );
    assert_eq!(
        declared_in_crate + declared_seam_backed,
        census.declared_total(),
        "in-crate + seam-backed must partition the {} declared entries",
        census.declared_total()
    );

    let undeclared = union.len() - declared_seam_backed;
    assert_eq!(
        undeclared,
        UNDECLARED_SEAM_MEMBERS,
        "the undeclared seam-member count moved — update UNDECLARED_SEAM_MEMBERS"
    );

    // "In-crate" must mean exactly "no seam table lists it": no predicate may fire on a
    // declared key the union does not hold.
    for entry in &census.implemented {
        let key = entry.owned_key();
        if union.binary_search(&key).is_ok() {
            continue;
        }
        for (name, table, predicate) in TABLES {
            assert!(
                !predicate(&key.0, &key.1, key.2),
                "`{}` is declared in-crate but {}'s predicate claims it ({table:?})",
                entry.spec(),
                name
            );
        }
    }
}

#[test]
fn the_undeclared_seam_members_are_stdlib_fast_paths() {
    let Some((tab, census)) = census_or_skip() else {
        return;
    };

    let union = union_keys();
    let undeclared: Vec<&(String, String, u32)> =
        union.iter().filter(|key| !is_declared(&tab, key)).collect();
    assert_eq!(
        undeclared.len(),
        UNDECLARED_SEAM_MEMBERS,
        "the undeclared seam-member set changed: {undeclared:?}"
    );
    // Each is a registry key the table does not declare, so the census must already have
    // classified it as a stdlib fast path (the two views cannot disagree).
    for key in &undeclared {
        assert!(
            census.fast_paths.contains(key),
            "`{}:{}`/{} reads a seam but is neither declared nor a census fast path",
            key.0,
            key.1,
            key.2
        );
    }
    // Exactly the three wholly-undeclared tables contribute: the memory pair, the four
    // `erlang` port operations (the table declares the `erts_internal` ones) and the
    // fun-taking surface.
    let expected: BTreeSet<(String, String, u32)> = [
        ("MEMORY_HOOK_BIFS", MEMORY_HOOK_BIFS),
        ("PORT_HOOK_BIFS", PORT_HOOK_BIFS),
        ("FUN_CALLER_BIFS", FUN_CALLER_BIFS),
    ]
    .iter()
    .flat_map(|(_, table)| {
        table
            .iter()
            .map(|&(m, f, a)| (m.to_string(), f.to_string(), a))
    })
    .collect();
    let actual: BTreeSet<(String, String, u32)> = undeclared.into_iter().cloned().collect();
    assert_eq!(
        actual, expected,
        "the undeclared seam members must be exactly MEMORY ∪ PORT ∪ FUN_CALLER"
    );

    // Every union member is registered, whatever side of the declared line it sits on.
    let reg = default_registry();
    for (module, function, arity) in &union {
        assert!(
            reg.contains(module, function, *arity),
            "{module}:{function}/{arity} reads a seam but is not registered"
        );
    }
}

#[test]
fn the_per_table_declared_partition_is_pinned() {
    let Some((tab, _)) = census_or_skip() else {
        return;
    };

    let mut owned: BTreeSet<(String, String, u32)> = BTreeSet::new();
    let mut seen: BTreeSet<(String, String, u32)> = BTreeSet::new();
    let mut totals: Vec<(&str, usize, usize)> = Vec::new();
    let mut declared_sum = 0usize;
    let mut total_sum = 0usize;
    for (name, table, _) in TABLES {
        let mut declared_here = 0usize;
        for &(module, function, arity) in table.iter() {
            let key = (module.to_string(), function.to_string(), arity);
            if seen.insert(key.clone()) && is_declared(&tab, &key) {
                // Credited to the first table that lists it (the disjoint partition).
                owned.insert(key);
                declared_here += 1;
            }
        }
        declared_sum += declared_here;
        total_sum += table.len();
        totals.push((name, table.len(), declared_here));
    }

    assert_eq!(
        totals,
        TABLE_PINS,
        "the per-table (total, declared) pins drifted — a table gained/lost keys, or a \
         declared entry moved between seams"
    );
    assert_eq!(
        declared_sum, DECLARED_SEAM_BACKED,
        "the per-table declared counts must partition the {DECLARED_SEAM_BACKED} \
         seam-backed declared entries"
    );
    assert_eq!(
        total_sum - shared_union_keys().len(),
        SEAM_UNION,
        "the per-table totals minus the shared keys must equal the deduped union"
    );
    assert_eq!(
        owned.len(),
        declared_sum,
        "the declared partition must be disjoint (no key credited twice)"
    );
}

#[test]
fn every_union_member_answers_its_own_predicate() {
    for (name, table, predicate) in TABLES {
        for &(module, function, arity) in table.iter() {
            assert!(
                predicate(module, function, arity),
                "{name}'s `needs_*` predicate must accept its own listed key \
                 {module}:{function}/{arity}"
            );
        }
    }
    // The reverse direction for the whole union: a key some table lists is accepted by
    // that table's predicate (the tables and the predicates cannot drift apart).
    for (module, function, arity) in union_keys() {
        let claiming: Vec<&str> = TABLES
            .iter()
            .filter(|(_, _, predicate)| predicate(&module, &function, arity))
            .map(|(name, _, _)| *name)
            .collect();
        let listing: Vec<&str> = TABLES
            .iter()
            .filter(|(_, table, _)| {
                table.contains(&(module.as_str(), function.as_str(), arity))
            })
            .map(|(name, _, _)| *name)
            .collect();
        assert_eq!(
            claiming, listing,
            "{module}:{function}/{arity} is listed by {listing:?} but claimed by {claiming:?}"
        );
    }
}

#[test]
fn the_declared_entries_per_module_are_pinned() {
    let Some((tab, _)) = census_or_skip() else {
        return;
    };

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for entry in tab.entries() {
        *counts.entry(entry.module.clone()).or_default() += 1;
    }
    let derived: Vec<(&str, usize)> = counts
        .iter()
        .map(|(module, count)| (module.as_str(), *count))
        .collect();
    assert_eq!(
        derived, DECLARED_PER_MODULE,
        "the declared entries per module changed — update DECLARED_PER_MODULE and CONTEXT.md"
    );
    assert_eq!(
        derived.iter().map(|(_, count)| count).sum::<usize>(),
        DECLARED_TOTAL,
        "the per-module counts must sum to the {DECLARED_TOTAL} declared entries"
    );
}
