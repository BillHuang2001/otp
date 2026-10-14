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

//! Integration tests for the [`BifRegistry`] seam (`beam_bifs`'s public API).
//!
//! These drive the registry exactly as `beam_rt` will: build a [`BifContext`] over
//! a real `beam_alloc::Heap` + a seeded `beam_term::AtomTable`, then resolve and
//! call BIFs by `(module, function, arity)`. The crate's own unit tests cover each
//! implementation in isolation; these tests pin the *seam* — the full core-BIF
//! roster, the arity check and the unknown-BIF contract.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_common::{BeamError, ExitReason};
use beam_process::ProcessId;
use beam_term::{make_small, AtomTable};

/// The Milestone-1 core BIF roster: every key `default_registry()` must contain.
///
/// Names/arities are verbatim from the crate `CONTEXT.md` "implemented BIF list"
/// (which reproduces `../../erts/emulator/beam/bif.tab`); operator BIFs keep their
/// quoted names. Kept unsorted here — the test sorts before comparing.
fn core_bif_names() -> Vec<(&'static str, &'static str, u32)> {
    let mut names = vec![
        // arith
        ("erlang", "+", 2),
        ("erlang", "-", 2),
        ("erlang", "*", 2),
        ("erlang", "/", 2),
        ("erlang", "div", 2),
        ("erlang", "rem", 2),
        ("erlang", "abs", 1),
        ("erlang", "-", 1),
        ("erlang", "+", 1),
        // compare
        ("erlang", "==", 2),
        ("erlang", "/=", 2),
        ("erlang", "=:=", 2),
        ("erlang", "=/=", 2),
        ("erlang", "<", 2),
        ("erlang", ">", 2),
        ("erlang", "=<", 2),
        ("erlang", ">=", 2),
        ("erts_internal", "cmp_term", 2),
        // type tests
        ("erlang", "is_atom", 1),
        ("erlang", "is_list", 1),
        ("erlang", "is_tuple", 1),
        ("erlang", "is_float", 1),
        ("erlang", "is_integer", 1),
        ("erlang", "is_number", 1),
        ("erlang", "is_pid", 1),
        ("erlang", "is_port", 1),
        ("erlang", "is_reference", 1),
        ("erlang", "is_binary", 1),
        ("erlang", "is_bitstring", 1),
        ("erlang", "is_map", 1),
        ("erlang", "is_boolean", 1),
        ("erlang", "is_function", 1),
        // term ops
        ("erlang", "element", 2),
        ("erlang", "setelement", 3),
        ("erlang", "tuple_size", 1),
        ("erlang", "tuple_to_list", 1),
        ("erlang", "list_to_tuple", 1),
        ("erlang", "length", 1),
        ("erlang", "hd", 1),
        ("erlang", "tl", 1),
        ("erlang", "size", 1),
        // process
        ("erlang", "self", 0),
        ("erlang", "send", 2),
        ("erlang", "!", 2),
        // raise helpers
        ("erlang", "error", 1),
        ("erlang", "error", 2),
        ("erlang", "exit", 1),
        ("erlang", "throw", 1),
        // io
        ("io", "format", 1),
        ("io", "format", 2),
        // ets
        ("ets", "new", 2),
        ("ets", "insert", 2),
        ("ets", "lookup", 2),
        ("ets", "delete", 1),
    ];
    names.sort();
    names
}

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "a", "b"]);
    atoms
}

#[test]
fn default_registry_contains_the_core_bifs() {
    let reg = default_registry();

    // The registry is a *superset* of the Milestone-1 core roster (the
    // conversion/lists/maps/binary/unicode/time modules extend it).
    assert!(reg.len() >= 54, "the core roster is a subset of the registry");
    assert!(!reg.is_empty());
    assert_eq!(reg.len(), BifRegistry::with_core_bifs().len());

    // Every core key must be present, and `names()` must stay sorted.
    let present: std::collections::HashSet<(String, String, u32)> =
        reg.names().into_iter().collect();
    for (module, function, arity) in core_bif_names() {
        assert!(
            present.contains(&(module.to_string(), function.to_string(), arity)),
            "missing {module}:{function}/{arity}"
        );
    }
    let names = reg.names();
    assert!(names.windows(2).all(|w| w[0] <= w[1]), "names() is sorted");

    // Every key is individually resolvable, and `entries()` agrees with `len()`.
    for (module, function, arity) in core_bif_names() {
        assert!(
            reg.contains(module, function, arity),
            "missing {module}:{function}/{arity}"
        );
        assert!(
            reg.lookup(module, function, arity).is_some(),
            "no impl for {module}:{function}/{arity}"
        );
    }
    assert_eq!(reg.entries().count(), reg.len());
}

/// The per-module shape of the assembled registry: every module in
/// `default_registry()` with its key count, verbatim from the crate `CONTEXT.md`
/// "the implemented BIFs (647)" census (27 modules).
///
/// This is the public-API complement of the crate-internal shadowing guard
/// (`src/hook_sets.rs::tests::no_registration_body_shadows_another`). The internal
/// guard proves the per-`register`-body rosters are pairwise disjoint;
/// `BifRegistry::register` keeps the *last* implementation for a key, so an overlap
/// would silently shadow the earlier one — and this pins each module's reachable key
/// count in the assembled registry, so a key that stops being reachable under its own
/// module, a wholesale rename, or a roster drifting from the documented census fails
/// here rather than at runtime.
const MODULE_KEY_COUNTS: &[(&str, usize)] = &[
    ("atomics", 7),
    ("binary", 25),
    ("code", 7),
    ("erl_ddll", 7),
    ("erl_debugger", 13),
    ("erlang", 282),
    ("error_logger", 1),
    ("erts_debug", 20),
    ("erts_internal", 71),
    ("erts_literal_area_collector", 2),
    ("erts_trace_cleaner", 2),
    ("ets", 59),
    ("file", 1),
    ("io", 3),
    ("io_lib", 3),
    ("lists", 45),
    ("maps", 24),
    ("math", 24),
    ("net_kernel", 1),
    ("os", 10),
    ("persistent_term", 7),
    ("prim_file", 4),
    ("re", 8),
    ("records", 8),
    ("string", 1),
    ("unicode", 6),
    ("zlib", 6),
];

#[test]
fn the_registry_matches_the_documented_per_module_census() {
    let reg = default_registry();

    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (module, _, _) in reg.names() {
        *counts.entry(module).or_insert(0) += 1;
    }
    let expected: std::collections::BTreeMap<String, usize> = MODULE_KEY_COUNTS
        .iter()
        .map(|(module, count)| (module.to_string(), *count))
        .collect();

    assert_eq!(
        counts, expected,
        "the per-module key census drifted from CONTEXT.md"
    );
    // The pinned census is self-consistent with the registry size.
    assert_eq!(
        MODULE_KEY_COUNTS.iter().map(|(_, n)| n).sum::<usize>(),
        reg.len()
    );
    assert_eq!(reg.len(), 647);
}

/// The registry resolves every key it advertises: `names()` (the enumeration a host
/// gates on) and `lookup`/`contains` (the dispatch path) agree key-for-key, and no key
/// is counted twice. A duplicate registration kept only the last implementation, so a
/// *missing* advertised key is the observable symptom of a shadowed body.
#[test]
fn every_advertised_key_resolves_and_none_is_duplicated() {
    let reg = default_registry();
    let names = reg.names();

    let mut distinct = std::collections::HashSet::new();
    for (module, function, arity) in &names {
        assert!(
            distinct.insert((module.clone(), function.clone(), *arity)),
            "{module}:{function}/{arity} is enumerated twice"
        );
        assert!(
            reg.contains(module, function, *arity),
            "{module}:{function}/{arity} is advertised but `contains` rejects it"
        );
        assert!(
            reg.lookup(module, function, *arity).is_some(),
            "{module}:{function}/{arity} is advertised but has no implementation"
        );
    }
    assert_eq!(distinct.len(), reg.len(), "names() and len() must agree");
}

#[test]
fn a_fresh_registry_is_empty_and_register_reports_the_previous_impl() {
    let mut reg = BifRegistry::new();
    assert!(reg.is_empty());
    assert_eq!(reg.len(), 0);
    assert!(reg.names().is_empty());

    let plus = default_registry()
        .lookup("erlang", "+", 2)
        .expect("erlang:'+'/2 is a core BIF");

    assert!(reg.register("erlang", "+", 2, plus).is_none());
    assert_eq!(reg.len(), 1);
    // Re-registering the same key returns the implementation it replaces.
    assert!(reg.register("erlang", "+", 2, plus).is_some());
    assert_eq!(reg.len(), 1);
}

#[test]
fn lookup_is_exact_on_module_function_and_arity() {
    let reg = default_registry();

    assert!(reg.lookup("erlang", "+", 2).is_some());
    // A different arity (note `+`/1 *is* registered — unary plus), function or
    // module is a miss. `++`/2 is registered (the list operators); `++`/3 is not.
    assert!(reg.lookup("erlang", "*", 1).is_none());
    assert!(reg.lookup("erlang", "++", 3).is_none());
    assert!(reg.lookup("no_such_module", "+", 2).is_none());
    assert!(!reg.contains("erlang", "abs", 2));
}

#[test]
fn call_reports_unknown_bifs_and_bad_arities() {
    let reg = default_registry();
    let mut heap = Heap::new(256).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // An unknown `(module, function, arity)` raises `undef` (never a silent 0).
    match reg.call("erlang", "no_such_bif", 1, &mut ctx, &[make_small(1)]) {
        Err(BeamError::Raise(raise)) => {
            assert_eq!(raise.reason, ExitReason::atom("undef"));
        }
        other => panic!("expected an undef raise, got {other:?}"),
    }

    // A known BIF called with the wrong number of arguments is `badarity`.
    assert!(matches!(
        reg.call("erlang", "+", 2, &mut ctx, &[make_small(1)]),
        Err(BeamError::BadArity)
    ));
    assert!(matches!(
        reg.call("erlang", "abs", 1, &mut ctx, &[]),
        Err(BeamError::BadArity)
    ));
    assert!(matches!(
        reg.call("erlang", "abs", 1, &mut ctx, &[make_small(1), make_small(2)]),
        Err(BeamError::BadArity)
    ));
}
