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

//! In-module unit suite for the map family.
//!
//! Covers every opcode variant (`get_map_elements` / `has_map_fields` /
//! `put_map_assoc` / `put_map_exact`), both frozen map layouts (flat and HAMT,
//! including the 32/33-key boundary), the failure paths (`fail` label, `{badmap,
//! Source}`, `{badkey, Key}` and the documented typed-`unsupported` gaps) and the
//! dispatch contract. The instructions are hand-built [`Instr`]s driven straight
//! through the family `exec` over a [`Machine`] and [`MinimalContext`] — the loader
//! is not involved.

use super::*;
use crate::context::MinimalContext;
use beam_common::BeamError;
use beam_loader::{Atom, Function, Operand};
use beam_term::make_small;
use std::collections::BTreeMap;

/// A tiny module builder with an owned atom table (mirrors the `Fixture`
/// scaffolding in `tests/opcodes.rs`, kept local so the unit tests here do not
/// depend on integration-test helpers).
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new(extra_atoms: &[&str]) -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern("m");
        for name in extra_atoms {
            module.atom_table.intern(name);
        }
        Fixture { module }
    }

    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    /// Append a function whose code starts with `entry`; returns its label.
    fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
        let name = self.module.atom_table.intern(name);
        let module = self.module.name;
        let mut labels = BTreeMap::new();
        for (index, instr) in code.iter().enumerate() {
            if let Instr::Label(label) = instr {
                labels.insert(*label, index);
            }
        }
        let entry = labels.keys().next().copied().unwrap_or(Label(0));
        self.module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module,
            name,
            arity,
            code,
            blocks: Vec::new(),
            labels,
        });
        entry
    }

    fn finish(self) -> Module {
        self.module
    }
}

/// Run `entry` with the given arguments over `ctx`, returning the result and the
/// machine.
///
/// `ctx` is threaded in (rather than created here) so it outlives the run: the
/// terms the map opcodes build live in its arena, and `MinimalContext` frees the
/// arena on drop, so a result read after the context is gone would dangle.
fn run(
    ctx: &mut MinimalContext,
    module: &Module,
    entry: Label,
    args: &[Eterm],
) -> (Result<Eterm>, Machine) {
    let mut machine = Machine::new();
    let result = machine.run_label(module, entry, args.len() as u32, args, ctx);
    (result, machine)
}

/// `entry: move x(0), x(0) (translate result); goto fail ⇒ return 99`.
///
/// The happy path returns `x(0)` unchanged; the `fail` label returns `99`, so a
/// test can distinguish "jumped to fail" from "fell through".
fn with_fail_body(body: Vec<Instr>, fail: Label) -> Vec<Instr> {
    let mut code = vec![Instr::Label(Label(1))];
    code.extend(body);
    code.push(Instr::Move {
        src: Operand::X(0),
        dst: Operand::X(0),
    });
    code.push(Instr::Return);
    code.push(Instr::Label(fail));
    code.push(Instr::Move {
        src: Operand::Integer(99),
        dst: Operand::X(0),
    });
    code.push(Instr::Return);
    code
}

/// Wrap `body` in `try … catch _ -> Reason end`; the handler returns `x(1)`
/// (the landed reason) as `x(0)`.
fn with_catch(body: Vec<Instr>) -> Vec<Instr> {
    let mut code = vec![
        Instr::Label(Label(1)),
        Instr::Try {
            frame: Operand::X(9),
            fail: Label(3),
        },
    ];
    code.extend(body);
    code.extend([
        Instr::TryEnd {
            frame: Operand::X(9),
        },
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(Label(3)),
        Instr::TryCase {
            frame: Operand::X(9),
        },
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Return,
    ]);
    code
}

// -- get_map_elements ----------------------------------------------------

#[test]
fn get_map_elements_reads_present_keys() {
    let mut fixture = Fixture::new(&["a", "b"]);
    let (a, b) = (fixture.atom("a"), fixture.atom("b"));
    let code = with_fail_body(
        vec![Instr::GetMapElements {
            fail: Label(2),
            source: Operand::X(0),
            pairs: vec![
                (Operand::Atom(a), Operand::X(1)),
                (Operand::Atom(b), Operand::X(2)),
            ],
        }],
        Label(2),
    );
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let map = terms::make_flatmap(
        &mut ctx,
        &[
            (make_atom(a), make_small(10)),
            (make_atom(b), make_small(20)),
        ],
    )
    .unwrap();

    // x(1) holds the value fetched for key `a`.
    let (result, machine) = run(&mut ctx, &module, entry, &[map]);
    assert_eq!(
        result.unwrap(),
        map,
        "the happy path returns the input register"
    );
    let _ = machine;
}

#[test]
fn get_map_elements_writes_each_value_to_its_destination() {
    let mut fixture = Fixture::new(&["a", "b"]);
    let (a, b) = (fixture.atom("a"), fixture.atom("b"));
    let code = with_fail_body(
        vec![
            Instr::GetMapElements {
                fail: Label(2),
                source: Operand::X(0),
                pairs: vec![
                    (Operand::Atom(a), Operand::X(1)),
                    (Operand::Atom(b), Operand::X(2)),
                ],
            },
            Instr::Move {
                src: Operand::X(2),
                dst: Operand::X(0),
            },
        ],
        Label(2),
    );
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let map = terms::make_flatmap(
        &mut ctx,
        &[
            (make_atom(b), make_small(20)),
            (make_atom(a), make_small(10)),
        ],
    )
    .unwrap();

    let (result, _machine) = run(&mut ctx, &module, entry, &[map]);
    assert_eq!(
        result.unwrap(),
        make_small(20),
        "key `b` maps to x(2) then x(0)"
    );
}

#[test]
fn get_map_elements_missing_key_jumps_to_fail() {
    let mut fixture = Fixture::new(&["a", "b"]);
    let (a, b) = (fixture.atom("a"), fixture.atom("b"));
    let code = with_fail_body(
        vec![Instr::GetMapElements {
            fail: Label(2),
            source: Operand::X(0),
            pairs: vec![(Operand::Atom(a), Operand::X(1))],
        }],
        Label(2),
    );
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let map = terms::make_flatmap(&mut ctx, &[(make_atom(b), make_small(1))]).unwrap();

    let (result, _machine) = run(&mut ctx, &module, entry, &[map]);
    assert_eq!(
        result.unwrap(),
        make_small(99),
        "key `a` is absent ⇒ fail label"
    );
}

#[test]
fn get_map_elements_empty_map_jumps_to_fail() {
    let mut fixture = Fixture::new(&["a"]);
    let a = fixture.atom("a");
    let code = with_fail_body(
        vec![Instr::GetMapElements {
            fail: Label(2),
            source: Operand::X(0),
            pairs: vec![(Operand::Atom(a), Operand::X(1))],
        }],
        Label(2),
    );
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let empty = terms::make_flatmap(&mut ctx, &[]).unwrap();

    let (result, _machine) = run(&mut ctx, &module, entry, &[empty]);
    assert_eq!(result.unwrap(), make_small(99));
}

#[test]
fn get_map_elements_non_map_jumps_to_fail() {
    let mut fixture = Fixture::new(&["a"]);
    let a = fixture.atom("a");
    let code = with_fail_body(
        vec![Instr::GetMapElements {
            fail: Label(2),
            source: Operand::X(0),
            pairs: vec![(Operand::Atom(a), Operand::X(1))],
        }],
        Label(2),
    );
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let (result, _machine) = run(&mut ctx, &module, entry, &[make_small(7)]);
    assert_eq!(
        result.unwrap(),
        make_small(99),
        "a non-map source jumps to fail"
    );
}

#[test]
fn get_map_elements_non_map_without_fail_raises_badmap() {
    let mut fixture = Fixture::new(&["a", "badmap", "error"]);
    let a = fixture.atom("a");
    let badmap = fixture.atom("badmap");
    // `try`-wrapped so the landed reason is observable.
    let code = with_catch(vec![Instr::GetMapElements {
        fail: NO_FAIL,
        source: Operand::X(0),
        pairs: vec![(Operand::Atom(a), Operand::X(1))],
    }]);
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let source = make_small(7);
    let mut ctx = MinimalContext::new();
    let (result, machine) = run(&mut ctx, &module, entry, &[source]);
    assert_eq!(machine.catch_frame_depth(), 0);
    let expected = terms::make_tuple(&mut ctx, &[make_atom(badmap), source]).unwrap();
    assert!(
        cmp(result.unwrap(), expected, &module.atom_table).is_eq(),
        "the landed reason is {{badmap, Source}}"
    );
}

#[test]
fn get_map_elements_missing_key_without_fail_is_a_typed_error() {
    let mut fixture = Fixture::new(&["a", "b"]);
    let (a, b) = (fixture.atom("a"), fixture.atom("b"));
    let code = with_fail_body(
        vec![Instr::GetMapElements {
            fail: NO_FAIL,
            source: Operand::X(0),
            pairs: vec![(Operand::Atom(a), Operand::X(1))],
        }],
        Label(2),
    );
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let map = terms::make_flatmap(&mut ctx, &[(make_atom(b), make_small(1))]).unwrap();

    let (result, _machine) = run(&mut ctx, &module, entry, &[map]);
    assert!(
        matches!(result.unwrap_err(), BeamError::Raise(_)),
        "an absent key with no fail label is a documented typed error"
    );
}

// -- has_map_fields ------------------------------------------------------

#[test]
fn has_map_fields_all_present_falls_through() {
    let mut fixture = Fixture::new(&["a", "b"]);
    let (a, b) = (fixture.atom("a"), fixture.atom("b"));
    let code = with_fail_body(
        vec![Instr::HasMapFields {
            fail: Label(2),
            source: Operand::X(0),
            keys: vec![Operand::Atom(a), Operand::Atom(b)],
        }],
        Label(2),
    );
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let map = terms::make_flatmap(
        &mut ctx,
        &[(make_atom(a), make_small(1)), (make_atom(b), make_small(2))],
    )
    .unwrap();

    let (result, _machine) = run(&mut ctx, &module, entry, &[map]);
    assert_eq!(result.unwrap(), map, "every key is present ⇒ fall through");
}

#[test]
fn has_map_fields_missing_key_jumps_to_fail() {
    let mut fixture = Fixture::new(&["a", "b"]);
    let (a, b) = (fixture.atom("a"), fixture.atom("b"));
    let code = with_fail_body(
        vec![Instr::HasMapFields {
            fail: Label(2),
            source: Operand::X(0),
            keys: vec![Operand::Atom(a), Operand::Atom(b)],
        }],
        Label(2),
    );
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let map = terms::make_flatmap(&mut ctx, &[(make_atom(a), make_small(1))]).unwrap();

    let (result, _machine) = run(&mut ctx, &module, entry, &[map]);
    assert_eq!(
        result.unwrap(),
        make_small(99),
        "key `b` is absent ⇒ fail label"
    );
}

#[test]
fn has_map_fields_non_map_jumps_to_fail() {
    let mut fixture = Fixture::new(&["a"]);
    let a = fixture.atom("a");
    let code = with_fail_body(
        vec![Instr::HasMapFields {
            fail: Label(2),
            source: Operand::X(0),
            keys: vec![Operand::Atom(a)],
        }],
        Label(2),
    );
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let (result, _machine) = run(&mut ctx, &module, entry, &[make_small(0)]);
    assert_eq!(result.unwrap(), make_small(99));
}

// -- put_map_assoc -------------------------------------------------------

/// `f/1` that runs `PutMapAssoc` with `pairs`, leaving the new map in `x(0)`.
fn assoc_function(fixture: &mut Fixture, pairs: Vec<(Operand, Operand)>) -> (Label, Atom) {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::PutMapAssoc {
            fail: NO_FAIL,
            source: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            pairs,
        },
        Instr::Return,
    ];
    let entry = fixture.add_function("f", 1, code);
    (entry, fixture.module.name)
}

#[test]
fn put_map_assoc_adds_and_overwrites() {
    let mut fixture = Fixture::new(&["a", "b"]);
    let (a, b) = (fixture.atom("a"), fixture.atom("b"));
    let (entry, _name) = assoc_function(
        &mut fixture,
        vec![
            (Operand::Atom(b), Operand::Integer(2)),
            (Operand::Atom(a), Operand::Integer(9)),
        ],
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let source = terms::make_flatmap(&mut ctx, &[(make_atom(a), make_small(1))]).unwrap();

    let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
    let result = result.unwrap();
    assert_ne!(result, source, "an immutable update builds a fresh map");
    let entries = terms::map_entries(result).unwrap();
    assert_eq!(
        lookup(&entries, make_atom(a), &module.atom_table),
        Some(make_small(9))
    );
    assert_eq!(
        lookup(&entries, make_atom(b), &module.atom_table),
        Some(make_small(2))
    );
    assert_eq!(entries.len(), 2);
}

#[test]
fn put_map_assoc_on_an_empty_map_builds_a_new_map() {
    let mut fixture = Fixture::new(&["a"]);
    let a = fixture.atom("a");
    let (entry, _name) =
        assoc_function(&mut fixture, vec![(Operand::Atom(a), Operand::Integer(7))]);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let source = terms::make_flatmap(&mut ctx, &[]).unwrap();

    let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
    let entries = terms::map_entries(result.unwrap()).unwrap();
    assert_eq!(entries, vec![(make_atom(a), make_small(7))]);
}

#[test]
fn put_map_assoc_non_map_raises_badmap() {
    let mut fixture = Fixture::new(&["a", "badmap", "error"]);
    let a = fixture.atom("a");
    let badmap = fixture.atom("badmap");
    let code = with_catch(vec![Instr::PutMapAssoc {
        fail: NO_FAIL,
        source: Operand::X(0),
        dst: Operand::X(0),
        live: 1,
        pairs: vec![(Operand::Atom(a), Operand::Integer(1))],
    }]);
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let source = make_small(7);
    let mut ctx = MinimalContext::new();
    let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
    let expected = terms::make_tuple(&mut ctx, &[make_atom(badmap), source]).unwrap();
    assert!(cmp(result.unwrap(), expected, &module.atom_table).is_eq());
}

#[test]
fn put_map_assoc_at_the_flat_map_limit_stays_flat() {
    // 31 source keys plus one new key = exactly 32 distinct keys, still at the
    // flat-map limit, so the update must stay a flat map.
    let mut fixture = Fixture::new(&[]);
    let (entry, _name) = assoc_function(
        &mut fixture,
        vec![(Operand::Integer(31), Operand::Integer(31))],
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let source: Vec<(Eterm, Eterm)> = (0..31).map(|i| (make_small(i), make_small(i))).collect();
    let source = terms::make_flatmap(&mut ctx, &source).unwrap();

    let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
    let result = result.unwrap();
    assert!(!terms::is_hashmap(result), "32 pairs is still a flat map");
    assert_eq!(terms::map_entries(result).unwrap().len(), 32);
    let entries = terms::map_entries(result).unwrap();
    for i in 0..32i64 {
        assert_eq!(
            lookup(&entries, make_small(i), &module.atom_table),
            Some(make_small(i)),
            "key {i}"
        );
    }
}

#[test]
fn put_map_assoc_over_the_flat_map_limit_builds_a_hashmap() {
    // A 32-key flat source plus a 33rd key must become a *hashmap*, exactly as
    // the oracle builds it.
    let mut fixture = Fixture::new(&[]);
    let (entry, _name) = assoc_function(
        &mut fixture,
        vec![(Operand::Integer(1000), Operand::Integer(1000))],
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let source: Vec<(Eterm, Eterm)> = (0..32).map(|i| (make_small(i), make_small(i))).collect();
    let source = terms::make_flatmap(&mut ctx, &source).unwrap();

    let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
    let result = result.unwrap();
    assert!(terms::is_hashmap(result), "33 pairs must build a hashmap");
    assert_eq!(terms::map_entries(result).unwrap().len(), 33);
    assert_eq!(
        beam_term::map::map_size(result, &module.atom_table).unwrap(),
        33
    );
    // Every key/value is retrievable through the frozen reader …
    let entries = terms::map_entries(result).unwrap();
    for i in 0..32i64 {
        assert_eq!(
            lookup(&entries, make_small(i), &module.atom_table),
            Some(make_small(i)),
            "key {i}"
        );
    }
    assert_eq!(
        lookup(&entries, make_small(1000), &module.atom_table),
        Some(make_small(1000)),
        "the key that forced the hashmap"
    );
    // … and through the `beam_term` HAMT lookup.
    for i in 0..32i64 {
        assert_eq!(
            beam_term::map::map_get(result, make_small(i), &module.atom_table).unwrap(),
            Some(make_small(i))
        );
    }
    assert_eq!(
        beam_term::map::map_get(result, make_small(1000), &module.atom_table).unwrap(),
        Some(make_small(1000))
    );
}

#[test]
fn put_map_assoc_with_33_pairs_on_an_empty_map_builds_a_hashmap() {
    // A single `put_map_assoc` that associates 33 distinct keys: the compiled
    // shape `#{...}#{...}` from a large map literal.
    let mut fixture = Fixture::new(&[]);
    let pairs: Vec<(Operand, Operand)> = (0..33i64)
        .map(|i| (Operand::Integer(i * 7), Operand::Integer(i)))
        .collect();
    let (entry, _name) = assoc_function(&mut fixture, pairs);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let empty = terms::make_flatmap(&mut ctx, &[]).unwrap();

    let (result, _machine) = run(&mut ctx, &module, entry, &[empty]);
    let result = result.unwrap();
    assert!(terms::is_hashmap(result));
    let entries = terms::map_entries(result).unwrap();
    assert_eq!(entries.len(), 33);
    for i in 0..33i64 {
        assert_eq!(
            lookup(&entries, make_small(i * 7), &module.atom_table),
            Some(make_small(i)),
            "key {}",
            i * 7
        );
    }
}

#[test]
fn put_map_assoc_growing_past_the_limit_enumerates_exactly_the_inserted_pairs() {
    // `put_map_assoc` twice on a growing map, the second crossing the limit: the
    // result must enumerate exactly the inserted pairs (as a set — the order is
    // implementation-defined), matching keys with `beam_term::cmp`.
    let mut fixture = Fixture::new(&[]);
    let first: Vec<(Operand, Operand)> = (0..16i64)
        .map(|i| (Operand::Integer(i), Operand::Integer(i * 2)))
        .collect();
    let second: Vec<(Operand, Operand)> = (16..40i64)
        .map(|i| (Operand::Integer(i), Operand::Integer(i * 2)))
        .collect();
    // `f/1`: assoc 16 keys, then assoc 24 more (40 total ⇒ hashmap), return x(0).
    let code = vec![
        Instr::Label(Label(1)),
        Instr::PutMapAssoc {
            fail: NO_FAIL,
            source: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            pairs: first,
        },
        Instr::PutMapAssoc {
            fail: NO_FAIL,
            source: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            pairs: second,
        },
        Instr::Return,
    ];
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let empty = terms::make_flatmap(&mut ctx, &[]).unwrap();
    let (result, _machine) = run(&mut ctx, &module, entry, &[empty]);
    let result = result.unwrap();
    assert!(terms::is_hashmap(result));

    let expected: Vec<(Eterm, Eterm)> = (0..40i64)
        .map(|i| (make_small(i), make_small(i * 2)))
        .collect();
    let entries = terms::map_entries(result).unwrap();
    assert_eq!(entries.len(), expected.len(), "exactly the inserted pairs");
    // Compare as a set: every expected pair is present exactly once.
    for (key, value) in &expected {
        let found: Vec<&(Eterm, Eterm)> = entries
            .iter()
            .filter(|(candidate, _)| cmp(*candidate, *key, &module.atom_table).is_eq())
            .collect();
        assert_eq!(found.len(), 1, "key {key:?} appears exactly once");
        assert!(cmp(found[0].1, *value, &module.atom_table).is_eq());
    }
}
// -- put_map_exact -------------------------------------------------------

#[test]
fn put_map_exact_updates_an_existing_key() {
    let mut fixture = Fixture::new(&["a", "b"]);
    let (a, b) = (fixture.atom("a"), fixture.atom("b"));
    let code = vec![
        Instr::Label(Label(1)),
        Instr::PutMapExact {
            fail: NO_FAIL,
            source: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            pairs: vec![(Operand::Atom(a), Operand::Integer(42))],
        },
        Instr::Return,
    ];
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let source = terms::make_flatmap(
        &mut ctx,
        &[(make_atom(a), make_small(1)), (make_atom(b), make_small(2))],
    )
    .unwrap();

    let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
    let entries = terms::map_entries(result.unwrap()).unwrap();
    assert_eq!(
        lookup(&entries, make_atom(a), &module.atom_table),
        Some(make_small(42))
    );
    assert_eq!(
        lookup(&entries, make_atom(b), &module.atom_table),
        Some(make_small(2))
    );
}

#[test]
fn put_map_exact_missing_key_jumps_to_fail() {
    let mut fixture = Fixture::new(&["a", "b"]);
    let (a, b) = (fixture.atom("a"), fixture.atom("b"));
    let code = with_fail_body(
        vec![Instr::PutMapExact {
            fail: Label(2),
            source: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            pairs: vec![(Operand::Atom(b), Operand::Integer(2))],
        }],
        Label(2),
    );
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let source = terms::make_flatmap(&mut ctx, &[(make_atom(a), make_small(1))]).unwrap();

    let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
    assert_eq!(result.unwrap(), make_small(99));
}

#[test]
fn put_map_exact_missing_key_without_fail_raises_badkey() {
    let mut fixture = Fixture::new(&["a", "b", "badkey", "error"]);
    let (a, b) = (fixture.atom("a"), fixture.atom("b"));
    let badkey = fixture.atom("badkey");
    let code = with_catch(vec![Instr::PutMapExact {
        fail: NO_FAIL,
        source: Operand::X(0),
        dst: Operand::X(0),
        live: 1,
        pairs: vec![(Operand::Atom(b), Operand::Integer(2))],
    }]);
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let source = terms::make_flatmap(&mut ctx, &[(make_atom(a), make_small(1))]).unwrap();

    let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
    let expected = terms::make_tuple(&mut ctx, &[make_atom(badkey), make_atom(b)]).unwrap();
    assert!(
        cmp(result.unwrap(), expected, &module.atom_table).is_eq(),
        "the landed reason is {{badkey, Key}}"
    );
}

#[test]
fn put_map_exact_empty_map_without_fail_raises_badkey() {
    // `beam_common.c:2263-2267`: an empty source fails immediately with BADKEY
    // and the *first* new key as `fvalue`.
    let mut fixture = Fixture::new(&["a", "badkey", "error"]);
    let (a, badkey) = (fixture.atom("a"), fixture.atom("badkey"));
    let code = with_catch(vec![Instr::PutMapExact {
        fail: NO_FAIL,
        source: Operand::X(0),
        dst: Operand::X(0),
        live: 1,
        pairs: vec![(Operand::Atom(a), Operand::Integer(1))],
    }]);
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let empty = terms::make_flatmap(&mut ctx, &[]).unwrap();

    let (result, _machine) = run(&mut ctx, &module, entry, &[empty]);
    let expected = terms::make_tuple(&mut ctx, &[make_atom(badkey), make_atom(a)]).unwrap();
    assert!(cmp(result.unwrap(), expected, &module.atom_table).is_eq());
}

// -- dispatch contract ---------------------------------------------------

#[test]
fn exec_claims_only_the_map_variants() {
    // A direct `exec` call on a non-map instruction must report "not mine".
    let module = Module::default();
    let labels = LabelMap::new();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let claimed = exec(
        &mut machine,
        &module,
        &labels,
        &Instr::Return,
        0,
        0,
        &mut ctx,
    )
    .unwrap();
    assert!(claimed.is_none());
}
