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

//! The map **read** instructions (`get_map_elements` / `has_map_fields`) over both
//! frozen layouts: the size sweep.
//!
//! A sibling of `tests.rs` (that file is already a comprehensive module): these rows
//! pin the read model — **one `beam_term::map::map_get` descent per key, no
//! materialized pair list** — across the whole size range
//! (`n ∈ {0, 1, 31, 32, 33, 34, 250, 2 500}`, i.e. both sides of the flat/hash-map
//! boundary at `MAP_SMALL_MAP_LIMIT` = 32), the value each fetched key lands in its
//! `x(n)` destination, `has_map_fields` writing no register at all, the absent-key
//! failure set (the `fail` label, and the engine-defined bare `function_clause` under
//! `NO_FAIL` with its typed-gap residual) and the non-map source's
//! `{badmap, Source}`.
//!
//! The *allocation* half of the same model — the read path builds no structure
//! proportional to the map size — is host-independent and lives in
//! `../../../tests/map_assoc_alloc.rs`, next to the write side's pin.

use super::*;
use crate::terms::test_support::make_float;
use beam_term::map::{map_size, MAP_SMALL_MAP_LIMIT};
use beam_term::NIL;

/// The swept source sizes: the empty map, one key, both sides of the flat/hash
/// boundary (`31`/`32`/`33`/`34`) and two hash-map sizes.
const SIZES: [usize; 8] = [0, 1, 31, 32, 33, 34, 250, 2_500];

/// The `n`-key source's pairs: `k => 3k` over the integer keys `0..n`.
///
/// Integer keys only, so no row depends on the atom namespace, and every value is a
/// distinct small int, so a fetched value identifies its own key.
fn source_pairs(n: usize) -> Vec<(Eterm, Eterm)> {
    (0..n as i64)
        .map(|i| (make_small(i), make_small(3 * i)))
        .collect()
}

/// The `n`-key source map, built with the general builder so it takes the **frozen**
/// layout for its size: flat at/below `MAP_SMALL_MAP_LIMIT`, a hash map above it.
fn source_map(ctx: &mut MinimalContext, n: usize, atoms: &AtomTable) -> Eterm {
    let map = terms::make_map(ctx, &source_pairs(n), atoms).expect("a well-formed map");
    assert_eq!(
        terms::is_hashmap(map),
        n > MAP_SMALL_MAP_LIMIT,
        "n = {n}: the source must take the frozen layout for its size"
    );
    assert_eq!(
        map_size(map, atoms).unwrap(),
        n,
        "n = {n}: the source's size word"
    );
    map
}

/// `count` **distinct** present keys of an `n`-key source: the first and the last,
/// plus the middle one for the three-key rows.
fn present_keys(n: usize, count: usize) -> Vec<i64> {
    match count {
        2 => vec![0, n as i64 - 1],
        _ => vec![0, n as i64 / 2, n as i64 - 1],
    }
}

/// The value `source_pairs` binds to `key`.
fn value_of(key: i64) -> Eterm {
    make_small(3 * key)
}

// -- the happy path ------------------------------------------------------

/// `get_map_elements` fetches **every** present key of both layouts, each value
/// landing in its own destination register.
///
/// Two-key and three-key rows at every size: the source is read key by key, so the
/// number of destinations is unrelated to the map's size (at `n = 2 500` two keys are
/// two hashed descents, not a 2 500-pair walk).
#[test]
fn get_map_elements_fetches_every_present_key_on_both_layouts() {
    for n in SIZES {
        for count in [2usize, 3] {
            if n < count {
                continue;
            }
            let keys = present_keys(n, count);
            let pairs: Vec<(Operand, Operand)> = keys
                .iter()
                .enumerate()
                .map(|(slot, key)| (Operand::Integer(*key), Operand::X(1 + slot as u32)))
                .collect();
            let mut fixture = Fixture::new(&[]);
            let code = with_fail_body(
                vec![Instr::GetMapElements {
                    fail: Label(2),
                    source: Operand::X(0),
                    pairs,
                }],
                Label(2),
            );
            // Arity = one argument per destination plus the source, so the
            // destinations are real registers the instruction must fill.
            let arity = 1 + count as u32;
            let entry = fixture.add_function("f", arity, code);
            let module = fixture.finish();
            let atoms = &module.atom_table;
            let mut ctx = MinimalContext::new();
            let source =
                source_map(&mut ctx, n, atoms);
            let args: Vec<Eterm> = std::iter::once(source).chain(keys.iter().map(|_| NIL)).collect();
            let (result, machine) = run(&mut ctx, &module, entry, &args);
            assert_eq!(
                result.unwrap(),
                source,
                "n = {n}, count = {count}: the happy path falls through"
            );
            for (slot, key) in keys.iter().enumerate() {
                assert_eq!(
                    machine.x(1 + slot as u32),
                    Some(value_of(*key)),
                    "n = {n}, count = {count}: x({}) must hold key {key}'s value",
                    1 + slot
                );
            }
            assert_eq!(
                map_size(source, atoms).unwrap(),
                n,
                "n = {n}: a read leaves the source map intact"
            );
        }
    }
}

/// `has_map_fields` finds every present key of both layouts and **writes no
/// register**: the two sentinel arguments stay in `x(1)`/`x(2)`.
#[test]
fn has_map_fields_finds_every_present_key_and_writes_nothing() {
    let (sentinel_a, sentinel_b) = (make_small(1_000), make_small(2_000));
    for n in SIZES {
        for count in [2usize, 3] {
            if n < count {
                continue;
            }
            let keys = present_keys(n, count);
            let mut fixture = Fixture::new(&[]);
            let code = with_fail_body(
                vec![Instr::HasMapFields {
                    fail: Label(2),
                    source: Operand::X(0),
                    keys: keys.iter().map(|key| Operand::Integer(*key)).collect(),
                }],
                Label(2),
            );
            let entry = fixture.add_function("f", 3, code);
            let module = fixture.finish();
            let atoms = &module.atom_table;
            let mut ctx = MinimalContext::new();
            let source = source_map(&mut ctx, n, atoms);
            let (result, machine) =
                run(&mut ctx, &module, entry, &[source, sentinel_a, sentinel_b]);
            assert_eq!(
                result.unwrap(),
                source,
                "n = {n}, count = {count}: every key is present ⇒ fall through"
            );
            assert_eq!(
                machine.x(1),
                Some(sentinel_a),
                "n = {n}, count = {count}: the test writes no register (x(1))"
            );
            assert_eq!(
                machine.x(2),
                Some(sentinel_b),
                "n = {n}, count = {count}: the test writes no register (x(2))"
            );
            assert_eq!(
                map_size(source, atoms).unwrap(),
                n,
                "n = {n}: a test leaves the source map intact"
            );
        }
    }
}

/// The flat layout's upper bound, with **atom** keys: `32` keys stays a flat map, and
/// both instructions read it key by key through the flat scan's `cmp` key rule.
#[test]
fn a_32_key_flat_source_is_read_key_by_key() {
    let mut fixture = Fixture::new(&[]);
    let names: Vec<String> = (0..MAP_SMALL_MAP_LIMIT).map(|i| format!("k{i}")).collect();
    let atoms: Vec<Atom> = names.iter().map(|name| fixture.atom(name)).collect();
    // Both instructions over the same 32-key atom map: fetch two keys, then test two.
    let code = with_fail_body(
        vec![
            Instr::GetMapElements {
                fail: Label(2),
                source: Operand::X(0),
                pairs: vec![
                    (Operand::Atom(atoms[7]), Operand::X(1)),
                    (Operand::Atom(atoms[31]), Operand::X(2)),
                ],
            },
            Instr::HasMapFields {
                fail: Label(2),
                source: Operand::X(0),
                keys: vec![Operand::Atom(atoms[0]), Operand::Atom(atoms[31])],
            },
        ],
        Label(2),
    );
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let pairs: Vec<(Eterm, Eterm)> = atoms
        .iter()
        .enumerate()
        .map(|(i, atom)| (make_atom(*atom), make_small(3 * i as i64)))
        .collect();
    let source = terms::make_map(&mut ctx, &pairs, &module.atom_table).expect("a flat map");
    assert!(
        !terms::is_hashmap(source),
        "32 keys is the flat layout's upper bound"
    );

    let (result, machine) = run(&mut ctx, &module, entry, &[source]);
    assert_eq!(result.unwrap(), source, "every key is present ⇒ fall through");
    assert_eq!(machine.x(1), Some(make_small(21)), "k7 => 21");
    assert_eq!(machine.x(2), Some(make_small(93)), "k31 => 93");
}

// -- the absent-key failure set -----------------------------------------

/// An absent key jumps to the `fail` label on both layouts; a value already written by
/// an earlier present key stays in its destination (a partial write before a failure
/// is harmless — the clause matcher restarts at `fail`).
#[test]
fn an_absent_key_jumps_to_fail_on_both_layouts() {
    let (sentinel_b, absent) = (make_small(2_000), 999_999);
    for n in SIZES {
        if n == 0 {
            // An empty map has no present key to write first; the fail label is still
            // taken (pinned by `get_map_elements_empty_map_jumps_to_fail`).
            continue;
        }
        let mut fixture = Fixture::new(&[]);
        let code = with_fail_body(
            vec![Instr::GetMapElements {
                fail: Label(2),
                source: Operand::X(0),
                pairs: vec![
                    (Operand::Integer(0), Operand::X(1)),
                    (Operand::Integer(absent), Operand::X(2)),
                ],
            }],
            Label(2),
        );
        let entry = fixture.add_function("f", 3, code);
        let module = fixture.finish();
        let atoms = &module.atom_table;
        let mut ctx = MinimalContext::new();
        let source = source_map(&mut ctx, n, atoms);
        let (result, machine) = run(&mut ctx, &module, entry, &[source, NIL, sentinel_b]);
        assert_eq!(
            result.unwrap(),
            make_small(99),
            "n = {n}: an absent key jumps to the fail label"
        );
        assert_eq!(
            machine.x(1),
            Some(value_of(0)),
            "n = {n}: the earlier present key's write is harmless"
        );
        assert_eq!(
            machine.x(2),
            Some(sentinel_b),
            "n = {n}: the absent key's destination is never written"
        );

        // The same source, same absent key, through `has_map_fields`.
        let mut fixture = Fixture::new(&[]);
        let code = with_fail_body(
            vec![Instr::HasMapFields {
                fail: Label(2),
                source: Operand::X(0),
                keys: vec![Operand::Integer(absent)],
            }],
            Label(2),
        );
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();
        let mut ctx2 = MinimalContext::new();
        let source = source_map(&mut ctx2, n, &module.atom_table);
        let (result, _machine) = run(&mut ctx2, &module, entry, &[source]);
        assert_eq!(
            result.unwrap(),
            make_small(99),
            "n = {n}: has_map_fields takes the fail label for an absent key"
        );
    }
}

/// Under `NO_FAIL` the absent key raises the engine-defined **bare atom**
/// `function_clause` (class `error`) on both layouts — the parent module docs carry the
/// ground truth and the reason-atom contract.
#[test]
fn an_absent_key_without_a_fail_label_raises_the_bare_function_clause() {
    let absent = 999_999;
    for n in SIZES {
        for fields in [false, true] {
            let mut fixture = Fixture::new(&["function_clause", "error"]);
            let function_clause = fixture.atom("function_clause");
            let instruction = if fields {
                Instr::HasMapFields {
                    fail: NO_FAIL,
                    source: Operand::X(0),
                    keys: vec![Operand::Integer(absent)],
                }
            } else {
                Instr::GetMapElements {
                    fail: NO_FAIL,
                    source: Operand::X(0),
                    pairs: vec![(Operand::Integer(absent), Operand::X(1))],
                }
            };
            let code = with_catch(vec![instruction]);
            let entry = fixture.add_function("f", 1, code);
            let module = fixture.finish();
            let atoms = &module.atom_table;
            let mut ctx = MinimalContext::new();
            let source = source_map(&mut ctx, n, atoms);
            let (result, machine) = run(&mut ctx, &module, entry, &[source]);
            assert_eq!(machine.catch_frame_depth(), 0, "the raise was caught");
            assert_eq!(
                result.unwrap(),
                make_atom(function_clause),
                "n = {n}, has_map_fields = {fields}: the landed reason is the bare atom"
            );
        }
    }
}

/// On a hash-map source the same raise keeps the documented typed gap when the module
/// never names `function_clause` (the engine never substitutes a foreign atom index).
#[test]
fn the_absent_key_raise_on_a_hash_map_stays_the_typed_gap_without_the_atom() {
    // No `function_clause` in this module's table.
    for fields in [false, true] {
        let mut fixture = Fixture::new(&[]);
        let instruction = if fields {
            Instr::HasMapFields {
                fail: NO_FAIL,
                source: Operand::X(0),
                keys: vec![Operand::Integer(999_999)],
            }
        } else {
            Instr::GetMapElements {
                fail: NO_FAIL,
                source: Operand::X(0),
                pairs: vec![(Operand::Integer(999_999), Operand::X(1))],
            }
        };
        let code = with_fail_body(vec![instruction], Label(2));
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();
        let atoms = &module.atom_table;
        let mut ctx = MinimalContext::new();
        // n = 33: the smallest hash map.
        let source = source_map(&mut ctx, 33, atoms);
        let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
        assert_unsupported_reason(
            result.unwrap_err(),
            "unsupported_opcode: maps: no 'function_clause' atom in the module",
        );
    }
}

/// A non-map source jumps to `fail` when the instruction has a fail label, and raises
/// `{badmap, Source}` when it does not — for **both** read instructions.
#[test]
fn a_non_map_source_jumps_to_fail_or_raises_badmap() {
    for fields in [false, true] {
        // (1) A real fail label.
        let mut fixture = Fixture::new(&[]);
        let instruction = if fields {
            Instr::HasMapFields {
                fail: Label(2),
                source: Operand::X(0),
                keys: vec![Operand::Integer(1)],
            }
        } else {
            Instr::GetMapElements {
                fail: Label(2),
                source: Operand::X(0),
                pairs: vec![(Operand::Integer(1), Operand::X(1))],
            }
        };
        let code = with_fail_body(vec![instruction], Label(2));
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();
        let mut ctx = MinimalContext::new();
        let (result, _machine) = run(&mut ctx, &module, entry, &[make_small(7)]);
        assert_eq!(
            result.unwrap(),
            make_small(99),
            "has_map_fields = {fields}: a non-map source jumps to fail"
        );

        // (2) `NO_FAIL`: the landed reason is `{badmap, Source}`.
        let mut fixture = Fixture::new(&["badmap", "error"]);
        let badmap = fixture.atom("badmap");
        let instruction = if fields {
            Instr::HasMapFields {
                fail: NO_FAIL,
                source: Operand::X(0),
                keys: vec![Operand::Integer(1)],
            }
        } else {
            Instr::GetMapElements {
                fail: NO_FAIL,
                source: Operand::X(0),
                pairs: vec![(Operand::Integer(1), Operand::X(1))],
            }
        };
        let code = with_catch(vec![instruction]);
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();
        let mut ctx = MinimalContext::new();
        let source = make_small(7);
        let (result, machine) = run(&mut ctx, &module, entry, &[source]);
        assert_eq!(machine.catch_frame_depth(), 0);
        let expected = make_tuple(&mut ctx, &[make_atom(badmap), source]).unwrap();
        assert!(
            cmp(result.unwrap(), expected, &module.atom_table).is_eq(),
            "has_map_fields = {fields}: the landed reason is {{badmap, Source}}"
        );
    }
}

/// The key rule is `cmp`'s `EQ`, i.e. `=:=`: a `1.0` key is **not** the integer key
/// `1`, so a source keyed by the float does not answer an integer read.
#[test]
fn a_float_key_is_not_an_integer_key() {
    for fields in [false, true] {
        let mut fixture = Fixture::new(&[]);
        let instruction = if fields {
            Instr::HasMapFields {
                fail: Label(2),
                source: Operand::X(0),
                keys: vec![Operand::Integer(1)],
            }
        } else {
            Instr::GetMapElements {
                fail: Label(2),
                source: Operand::X(0),
                pairs: vec![(Operand::Integer(1), Operand::X(1))],
            }
        };
        let code = with_fail_body(vec![instruction], Label(2));
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();
        let mut ctx = MinimalContext::new();
        let float_one = make_float(&mut ctx, 1.0).unwrap();
        let source = make_flatmap(&mut ctx, &[(float_one, make_small(7))]).unwrap();
        let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
        assert_eq!(
            result.unwrap(),
            make_small(99),
            "has_map_fields = {fields}: 1.0 and 1 are distinct keys ⇒ fail"
        );
    }
}

// -- destination registers that alias the source register -----------------

/// A destination register that **aliases the source** must not disturb a later pair:
/// the C's `i_get_map_elements` reads `map = $Src` into a local and descends that
/// local for every key (`emu/map_instrs.tab:64-122`), so the whole instruction sees
/// the map the source register held at entry.
///
/// This is not a corner case: `erlc` reuses the source register as the **first**
/// pair's destination whenever the map is dead after the match — the shape
/// `khepri_tree:gather_node_props/2` lowers to (`khepri_tree.erl:268`,
/// `get_map_elements {f,36} {tr,{x,2},…} [{payload_version,x,2},{child_list_version,x,3}]`),
/// pinned end to end by
/// `../../../tests/maps.rs::a_destination_that_aliases_the_source_reads_the_original_map`.
#[test]
fn an_aliasing_destination_reads_the_original_map() {
    for n in SIZES {
        if n < 2 {
            continue;
        }
        let (first, last) = (0i64, n as i64 - 1);
        // Pair 1's destination is the source (the compiler's shape) …
        assert_alias_row(n, &[(first, 0), (last, 1)]);
        // … and pair 2's destination is, so a *later* pair still descends the map.
        assert_alias_row(n, &[(first, 1), (last, 0)]);
        // Three pairs with the alias in the middle: the pairs after it must still
        // read the original map.
        assert_alias_row(n, &[(first, 1), (n as i64 / 2, 0), (last, 2)]);
    }
}

/// One row of [`an_aliasing_destination_reads_the_original_map`]: `pairs` gives
/// `(key, destination register)` with the source in `x(0)` throughout, and every
/// destination must end up holding **its own key's** value (a stale descent of a
/// clobbered register would give the wrong one — or a `badarg`).
fn assert_alias_row(n: usize, pairs: &[(i64, u32)]) {
    let mut fixture = Fixture::new(&[]);
    let code = with_fail_body(
        vec![Instr::GetMapElements {
            fail: Label(2),
            source: Operand::X(0),
            pairs: pairs
                .iter()
                .map(|(key, dst)| (Operand::Integer(*key), Operand::X(*dst)))
                .collect(),
        }],
        Label(2),
    );
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();
    let atoms = &module.atom_table;
    let mut ctx = MinimalContext::new();
    let source = source_map(&mut ctx, n, atoms);
    let (result, machine) = run(&mut ctx, &module, entry, &[source]);
    assert_eq!(
        machine.x(1).is_some() && machine.x(2).is_some(),
        true,
        "the destination registers exist"
    );
    for (key, dst) in pairs {
        assert_eq!(
            machine.x(*dst),
            Some(value_of(*key)),
            "n = {n}, pairs = {pairs:?}: x({dst}) must hold key {key}'s value, \
             not an earlier pair's"
        );
    }
    // The happy path returns `x(0)`, i.e. the value of the pair whose destination is
    // the source register — or the untouched source when no destination aliases it.
    let aliased = pairs
        .iter()
        .find(|(_, dst)| *dst == 0)
        .map(|(key, _)| value_of(*key));
    assert_eq!(
        result.unwrap(),
        aliased.unwrap_or(source),
        "n = {n}, pairs = {pairs:?}: x(0) holds its own pair's value, never a stale word"
    );
}

/// A **missing** key after a destination already wrote over the source register must
/// still take the `fail` label — the descent reads the held map, not the clobbered
/// register (which the retired re-read made a `badarg`).
#[test]
fn an_absent_key_after_an_aliasing_write_takes_the_fail_label() {
    let absent = 999_999;
    for n in SIZES {
        if n == 0 {
            continue;
        }
        let mut fixture = Fixture::new(&[]);
        let code = with_fail_body(
            vec![
                // The source lives in `X(5)`, out of the fail body's reach (it writes
                // `x(0)`), so the register is still observable after the jump.
                Instr::Move {
                    src: Operand::X(0),
                    dst: Operand::X(5),
                },
                Instr::GetMapElements {
                    fail: Label(2),
                    source: Operand::X(5),
                    pairs: vec![
                        // Pair 1 is present and its destination IS the source register.
                        (Operand::Integer(0), Operand::X(5)),
                        // Pair 2's key is absent: `fail`, never a raise.
                        (Operand::Integer(absent), Operand::X(1)),
                    ],
                },
            ],
            Label(2),
        );
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();
        let atoms = &module.atom_table;
        let mut ctx = MinimalContext::new();
        let source = source_map(&mut ctx, n, atoms);
        let (result, machine) = run(&mut ctx, &module, entry, &[source]);
        assert_eq!(
            result.unwrap(),
            make_small(99),
            "n = {n}: an absent key jumps to the fail label, not a badarg"
        );
        assert_eq!(
            machine.x(5),
            Some(value_of(0)),
            "n = {n}: the aliasing pair's own write is the only one to the source slot"
        );
        assert_eq!(
            machine.x(1),
            Some(NIL),
            "n = {n}: the absent key's destination is never written"
        );
    }
}

/// A missing key mid-way through a multi-pair read leaves the **source operand's
/// word** untouched: no destination aliases it, so the failure path writes nothing.
#[test]
fn an_absent_key_mid_way_leaves_the_source_word_unchanged() {
    let absent = 999_999;
    for n in SIZES {
        if n < 2 {
            continue;
        }
        let (first, last) = (0i64, n as i64 - 1);
        let mut fixture = Fixture::new(&[]);
        let code = with_fail_body(
            vec![
                // The source lives in `X(5)`, out of the fail body's reach.
                Instr::Move {
                    src: Operand::X(0),
                    dst: Operand::X(5),
                },
                Instr::GetMapElements {
                    fail: Label(2),
                    source: Operand::X(5),
                    pairs: vec![
                        (Operand::Integer(first), Operand::X(1)),
                        (Operand::Integer(absent), Operand::X(2)),
                        (Operand::Integer(last), Operand::X(3)),
                    ],
                },
            ],
            Label(2),
        );
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();
        let atoms = &module.atom_table;
        let mut ctx = MinimalContext::new();
        let source = source_map(&mut ctx, n, atoms);
        let (result, machine) = run(&mut ctx, &module, entry, &[source]);
        assert_eq!(result.unwrap(), make_small(99), "n = {n}: the fail label");
        assert_eq!(
            machine.x(5),
            Some(source),
            "n = {n}: the source operand's word is unchanged by a failed read"
        );
        assert_eq!(machine.x(1), Some(value_of(first)), "n = {n}: pair 1 wrote");
        assert_eq!(machine.x(2), Some(NIL), "n = {n}: the absent key wrote nothing");
        assert_eq!(
            machine.x(3),
            Some(NIL),
            "n = {n}: the pair after the failure never ran"
        );
    }
}
