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
//! The path-copying (COW) update arm of the map family's unit suite.
//!
//! A sibling of `tests.rs` (that file is already a comprehensive module): the
//! `put_map_assoc` / `put_map_exact` rows that pin the hash-map arm against the
//! flat arm's rebuild, the exact-key no-write failures, the no-change identity
//! answers and the reservation's size independence.
// ---------------------------------------------------------------------------
// The path-copying (COW) update arm
// ---------------------------------------------------------------------------
//
// `put_map_assoc`/`put_map_exact` take `beam_term::map::map_put_cow` (through
// `crate::terms::map_cow_rooted`) on a **hash-map** source and keep the
// `terms::make_map` rebuild on a flat one. These rows pin that the two arms answer
// the same map at every size across the layout boundary — `=:=`, the size word, the
// canonical key sequence and the ETF bytes — and that the reservation does not grow
// with `n`.
use super::*;
use beam_term::map::{map_pairs, map_size, MAP_SMALL_MAP_LIMIT};

/// The sizes the equivalence rows sweep: the empty map, one pair, both sides of
/// the flat/hash boundary (`31`/`32`/`33`/`34`) and two hash-map sizes.
const SIZES: [usize; 8] = [0, 1, 31, 32, 33, 34, 250, 2_500];

/// The source map's `n` pairs: `k => 3k` over the integer keys `0..n`.
///
/// Integer keys only, so the rows are independent of the atom namespace: two
/// fixtures' tables can be compared with `cmp` without an index-order caveat.
fn source_pairs(n: usize) -> Vec<(Eterm, Eterm)> {
    (0..n as i64)
        .map(|i| (make_small(i), make_small(3 * i)))
        .collect()
}

/// `pairs` with `updates` applied — replace an existing key's value, else append —
/// the merge the rebuild arm performs, written independently of the instruction.
fn merge(
    pairs: &[(Eterm, Eterm)],
    updates: &[(i64, i64)],
    atoms: &AtomTable,
) -> Vec<(Eterm, Eterm)> {
    let mut merged = pairs.to_vec();
    for (key, value) in updates {
        let key = make_small(*key);
        let value = make_small(*value);
        match merged
            .iter()
            .position(|(candidate, _)| cmp(*candidate, key, atoms).is_eq())
        {
            Some(index) => merged[index].1 = value,
            None => merged.push((key, value)),
        }
    }
    merged
}

/// Assert `got` and `want` name the same map: `=:=`, the same size word, and the
/// same **canonical** key sequence (`map_pairs`) — the invariant the frozen layout
/// guarantees and the internal HAMT node order does not.
fn assert_same_map(got: Eterm, want: Eterm, atoms: &AtomTable, what: &str) {
    assert!(cmp(got, want, atoms).is_eq(), "{what}: the maps are not =:=");
    assert_eq!(
        map_size(got, atoms).unwrap(),
        map_size(want, atoms).unwrap(),
        "{what}: the size words differ"
    );
    let got_pairs = map_pairs(got, atoms).unwrap();
    let want_pairs = map_pairs(want, atoms).unwrap();
    assert_eq!(got_pairs.len(), want_pairs.len(), "{what}: pair counts differ");
    for (index, (got, want)) in got_pairs.iter().zip(want_pairs.iter()).enumerate() {
        assert!(
            cmp(got.0, want.0, atoms).is_eq() && cmp(got.1, want.1, atoms).is_eq(),
            "{what}: canonical pair {index} differs: {got:?} vs {want:?}"
        );
    }
}

/// One `put_map_assoc` with the integer `updates` over an `n`-key source, compared
/// with the rebuild of the same merge.
fn check_assoc(n: usize, updates: &[(i64, i64)]) {
    let mut fixture = Fixture::new(&[]);
    let operands = updates
        .iter()
        .map(|(key, value)| (Operand::Integer(*key), Operand::Integer(*value)))
        .collect();
    let (entry, _name) = assoc_function(&mut fixture, operands);
    let module = fixture.finish();
    let atoms = &module.atom_table;

    let mut ctx = MinimalContext::new();
    let pairs = source_pairs(n);
    let source = terms::make_map(&mut ctx, &pairs, atoms).unwrap();
    assert_eq!(
        terms::is_hashmap(source),
        n > MAP_SMALL_MAP_LIMIT,
        "n = {n}: the layout boundary must stay at MAP_SMALL_MAP_LIMIT"
    );

    let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
    let result = result.expect("a well-formed put_map_assoc");
    let reference = terms::make_map(&mut ctx, &merge(&pairs, updates, atoms), atoms).unwrap();
    assert_same_map(
        result,
        reference,
        atoms,
        &format!("put_map_assoc n = {n}, updates = {updates:?}"),
    );
    assert_eq!(
        map_size(source, atoms).unwrap(),
        n,
        "n = {n}: the source map is immutable"
    );
}

/// The same comparison for `put_map_exact`, whose keys must all be present.
fn check_exact(n: usize, updates: &[(i64, i64)]) {
    let mut fixture = Fixture::new(&[]);
    let operands = updates
        .iter()
        .map(|(key, value)| (Operand::Integer(*key), Operand::Integer(*value)))
        .collect();
    let (entry, _name) = exact_function(&mut fixture, operands);
    let module = fixture.finish();
    let atoms = &module.atom_table;

    let mut ctx = MinimalContext::new();
    let pairs = source_pairs(n);
    let source = terms::make_map(&mut ctx, &pairs, atoms).unwrap();
    assert_eq!(
        terms::is_hashmap(source),
        n > MAP_SMALL_MAP_LIMIT,
        "n = {n}: the layout boundary must stay at MAP_SMALL_MAP_LIMIT"
    );

    let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
    let result = result.expect("a well-formed put_map_exact");
    let reference = terms::make_map(&mut ctx, &merge(&pairs, updates, atoms), atoms).unwrap();
    assert_same_map(
        result,
        reference,
        atoms,
        &format!("put_map_exact n = {n}, updates = {updates:?}"),
    );
    assert_eq!(
        map_size(source, atoms).unwrap(),
        n,
        "n = {n}: the source map is immutable"
    );
}

#[test]
fn put_map_assoc_matches_the_rebuild_on_both_layouts() {
    for n in SIZES {
        // (a) one replaced existing key (the empty map has none).
        if n > 0 {
            check_assoc(n, &[(n as i64 - 1, -1)]);
        }
        // (b) one replaced key plus one new key.
        if n > 0 {
            check_assoc(n, &[(n as i64 - 1, -1), (n as i64 + 5, 7)]);
        }
        // (c) one new key only.
        check_assoc(n, &[(n as i64 + 5, 7)]);
    }
}

#[test]
fn put_map_exact_matches_the_rebuild_on_both_layouts() {
    for n in SIZES {
        if n == 0 {
            // An exact update over an empty map has no key to replace; its
            // `{badkey,Key}` behaviour is pinned separately.
            continue;
        }
        // (a) one replaced existing key.
        check_exact(n, &[(n as i64 - 1, -1)]);
        // Two replaced keys — the multi-pair chain of the path-copying arm.
        if n > 1 {
            check_exact(n, &[(n as i64 - 1, -1), (n as i64 - 2, -2)]);
        }
    }
}

/// The absent-key path on a **hash-map** source: the fail label, with the source's
/// word and contents untouched (the validation is a pure `map_get` descent decided
/// before a single word is written).
#[test]
fn put_map_exact_missing_key_on_a_hash_map_jumps_to_fail_without_writing() {
    let mut fixture = Fixture::new(&[]);
    let code = with_fail_body(
        vec![Instr::PutMapExact(Box::new(MapWriteArgs {
            fail: Label(2),
            source: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            pairs: vec![(Operand::Integer(40), Operand::Integer(-1))],
        }))],
        Label(2),
    );
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();
    let atoms = &module.atom_table;

    let mut ctx = MinimalContext::new();
    let source = terms::make_map(&mut ctx, &source_pairs(40), atoms).unwrap();
    assert!(terms::is_hashmap(source));
    let before = map_pairs(source, atoms).unwrap();

    let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
    assert_eq!(result.unwrap(), make_small(99), "the fail label was taken");

    let after = map_pairs(source, atoms).unwrap();
    assert_eq!(before.len(), after.len());
    assert_eq!(after.len(), 40, "the source still holds its 40 pairs");
    for (got, want) in after.iter().zip(before.iter()) {
        assert!(cmp(got.0, want.0, atoms).is_eq() && cmp(got.1, want.1, atoms).is_eq());
    }
}

/// The same absent key under `NO_FAIL`: the `{badkey,Key}` raise, again with the
/// source's contents untouched.
#[test]
fn put_map_exact_missing_key_on_a_hash_map_without_fail_raises_badkey() {
    let mut fixture = Fixture::new(&["badkey", "error"]);
    let badkey = fixture.atom("badkey");
    let code = with_catch(vec![Instr::PutMapExact(Box::new(MapWriteArgs {
        fail: NO_FAIL,
        source: Operand::X(0),
        dst: Operand::X(0),
        live: 1,
        pairs: vec![(Operand::Integer(40), Operand::Integer(-1))],
    }))]);
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();
    let atoms = &module.atom_table;

    let mut ctx = MinimalContext::new();
    let source = terms::make_map(&mut ctx, &source_pairs(40), atoms).unwrap();
    assert!(terms::is_hashmap(source));
    let before = map_pairs(source, atoms).unwrap();

    let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
    let expected = make_tuple(&mut ctx, &[make_atom(badkey), make_small(40)]).unwrap();
    assert!(
        cmp(result.unwrap(), expected, atoms).is_eq(),
        "the landed reason is {{badkey, Key}}"
    );

    let after = map_pairs(source, atoms).unwrap();
    assert_eq!(after.len(), 40, "the source still holds its 40 pairs");
    for (got, want) in after.iter().zip(before.iter()) {
        assert!(cmp(got.0, want.0, atoms).is_eq() && cmp(got.1, want.1, atoms).is_eq());
    }
}

/// A multi-pair **all-no-op** update over a hash map answers the source word: the
/// writer's own zero-write arm per put, chained.
#[test]
fn put_map_assoc_multi_pair_no_op_on_a_hash_map_returns_the_source_word() {
    let mut fixture = Fixture::new(&[]);
    let (entry, _name) = assoc_function(
        &mut fixture,
        vec![
            (Operand::Integer(7), Operand::Integer(21)),
            (Operand::Integer(11), Operand::Integer(33)),
        ],
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let source = terms::make_map(&mut ctx, &source_pairs(40), &module.atom_table).unwrap();
    assert!(terms::is_hashmap(source));

    let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
    assert_eq!(
        result.unwrap(),
        source,
        "two value-identical replacements must answer the source map's own word"
    );
}

/// The same update with **one** changed value: a fresh map, with both pairs applied.
#[test]
fn put_map_assoc_multi_pair_with_one_change_on_a_hash_map_is_not_the_source_word() {
    let mut fixture = Fixture::new(&[]);
    let (entry, _name) = assoc_function(
        &mut fixture,
        vec![
            (Operand::Integer(7), Operand::Integer(21)),
            (Operand::Integer(11), Operand::Integer(-1)),
        ],
    );
    let module = fixture.finish();
    let atoms = &module.atom_table;

    let mut ctx = MinimalContext::new();
    let pairs = source_pairs(40);
    let source = terms::make_map(&mut ctx, &pairs, atoms).unwrap();
    assert!(terms::is_hashmap(source));

    let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
    let result = result.unwrap();
    assert_ne!(result, source, "one changed pair builds a fresh map");
    assert_eq!(map_size(result, atoms).unwrap(), 40);
    assert_eq!(
        beam_term::map::map_get(result, make_small(7), atoms).unwrap(),
        Some(make_small(21))
    );
    assert_eq!(
        beam_term::map::map_get(result, make_small(11), atoms).unwrap(),
        Some(make_small(-1))
    );
    let reference =
        terms::make_map(&mut ctx, &merge(&pairs, &[(7, 21), (11, -1)], atoms), atoms).unwrap();
    assert_same_map(result, reference, atoms, "multi-pair one change");
}

/// `put_map_exact`'s multi-pair all-no-op on a hash map: the source word again.
#[test]
fn put_map_exact_multi_pair_no_op_on_a_hash_map_returns_the_source_word() {
    let mut fixture = Fixture::new(&[]);
    let (entry, _name) = exact_function(
        &mut fixture,
        vec![
            (Operand::Integer(7), Operand::Integer(21)),
            (Operand::Integer(11), Operand::Integer(33)),
        ],
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let source = terms::make_map(&mut ctx, &source_pairs(40), &module.atom_table).unwrap();
    assert!(terms::is_hashmap(source));

    let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
    assert_eq!(result.unwrap(), source);
}

/// … and with one changed value it is a fresh map.
#[test]
fn put_map_exact_multi_pair_with_one_change_on_a_hash_map_is_not_the_source_word() {
    let mut fixture = Fixture::new(&[]);
    let (entry, _name) = exact_function(
        &mut fixture,
        vec![
            (Operand::Integer(7), Operand::Integer(21)),
            (Operand::Integer(11), Operand::Integer(-1)),
        ],
    );
    let module = fixture.finish();
    let atoms = &module.atom_table;

    let mut ctx = MinimalContext::new();
    let source = terms::make_map(&mut ctx, &source_pairs(40), atoms).unwrap();
    let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
    let result = result.unwrap();
    assert_ne!(result, source);
    assert_eq!(
        beam_term::map::map_get(result, make_small(11), atoms).unwrap(),
        Some(make_small(-1))
    );
    assert_eq!(
        beam_term::map::map_get(result, make_small(7), atoms).unwrap(),
        Some(make_small(21))
    );
}

/// The words one single-pair update reserves over an `n`-key hash map: the arena's
/// `live_words` delta of one call, after a first call has opened the region the
/// arena grows into. No wall clock and no host load enters the figure.
fn reserved_words(n: usize, exact: bool) -> usize {
    let mut fixture = Fixture::new(&[]);
    let operands = vec![(Operand::Integer(n as i64 - 1), Operand::Integer(-1))];
    let (entry, _name) = if exact {
        exact_function(&mut fixture, operands)
    } else {
        assoc_function(&mut fixture, operands)
    };
    let module = fixture.finish();
    let atoms = &module.atom_table;

    let mut ctx = MinimalContext::new();
    let source = terms::make_map(&mut ctx, &source_pairs(n), atoms).unwrap();
    assert!(terms::is_hashmap(source), "n = {n}: the source must be a hash map");
    let (warm, _machine) = run(&mut ctx, &module, entry, &[source]);
    assert_ne!(warm.unwrap(), source, "the update must change something");
    let before = ctx.heap_stats().live_words;
    let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
    assert_ne!(result.unwrap(), source);
    ctx.heap_stats().live_words - before
}

/// The reservation of a single-pair hash-map update is **independent of the map's
/// size** and bounded by the per-put capacity figure at the layout boundary — an
/// in-code derivation, not a magic constant.
#[test]
fn a_hash_map_update_reserves_a_size_independent_number_of_words() {
    let bound = terms::map_put_cow_capacity(MAP_SMALL_MAP_LIMIT + 1);
    for exact in [false, true] {
        let at_250 = reserved_words(250, exact);
        let at_6_250 = reserved_words(6_250, exact);
        assert_eq!(
            at_250, at_6_250,
            "exact = {exact}: one put over a 250-key and a 6 250-key hash map must \
             reserve the same words (the path, not the map, is copied)"
        );
        assert!(
            at_250 <= bound,
            "exact = {exact}: {at_250} words exceed the per-put capacity {bound} at the \
             layout boundary"
        );
    }
}

/// The ETF image of a path-copied result is byte-identical to the rebuild's, so a
/// map that lost identity still serializes to the same term.
#[test]
fn a_hash_map_update_encodes_to_the_rebuild_bytes() {
    let mut fixture = Fixture::new(&[]);
    let (entry, _name) =
        assoc_function(&mut fixture, vec![(Operand::Integer(7), Operand::Integer(-1))]);
    let module = fixture.finish();
    let atoms = &module.atom_table;

    let mut ctx = MinimalContext::new();
    let pairs = source_pairs(250);
    let source = terms::make_map(&mut ctx, &pairs, atoms).unwrap();
    assert!(terms::is_hashmap(source));

    let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
    let result = result.unwrap();
    let reference =
        terms::make_map(&mut ctx, &merge(&pairs, &[(7, -1)], atoms), atoms).unwrap();
    assert_same_map(result, reference, atoms, "250-key put_map_assoc");
    assert_eq!(
        beam_codec::encode_term(result, atoms).expect("the ETF encoder accepts the map"),
        beam_codec::encode_term(reference, atoms).expect("the ETF encoder accepts the reference"),
        "the path-copied result must serialize to the rebuild's bytes"
    );
}
