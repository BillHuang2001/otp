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

//! **Oracle-cross-checked stress shapes** for the `ordered_set` K-ary B+-tree
//! interior.
//!
//! `tests/ets_ordered_oracle.rs` drives a *randomised* operation trace and
//! `tests/ets_fixtable.rs` pins the split-crossing **walks**; neither grows the
//! tree to a scale where dozens of node levels form, nor tears one all the way
//! back down. This file is the missing **scale** evidence: eight deterministic
//! shapes, each grown to 10 000 keys (or torn down from that size), every
//! observable compared fact-for-fact against the live system `erl`
//! (OTP 29.1.1 / erts-17.1).
//!
//! Every table is a real [`Table`] of [`TableKind::OrderedSet`] on its **default**
//! interior — [`OrderedInterior::BPlusTree`] (asserted, so a future change of the
//! default cannot silently retarget this file).
//!
//! # The shapes, and what each one forces
//!
//! | scenario | shape |
//! |---|---|
//! | `asc` | 10 000 ascending `{I,I}` inserts — right-edge splits at every level, including root splits |
//! | `desc` | 10 000 descending inserts — the left-edge split path |
//! | `rand` | 10 000 inserts in a deterministic Fisher–Yates permutation drawn from a fixed-seed SplitMix64 (additive-LCG) stream — splits interleaved across the whole key space |
//! | `merge` | delete a 10 000-key tree back down in ascending, descending and permuted order — the borrow-or-merge unwind, checkpointed every 2 000 deletes |
//! | `dups` | the arithmetic-duplicate keys (`1`/`1.0`, `2`/`2.0`, `-1`/`-1.0`) interleaved with bignums, floats, atoms, tuples, lists and binaries, keyed by pool index |
//! | `boundary` | `first`+`next` / `last`+`prev` walks over key counts 31…100 (just below/at/above multiples of `ORDER = 32`), incl. a walk started from an **absent** anchor (a range bound) |
//! | `select` | a match spec that returns every object (must come back in ascending key order), `select_reverse`, `select_count`, `match_object`, a guarded `select`, and `select_delete` on `I rem 3 == 0` |
//! | `keypos2` | the ascending-10k shape on a `{keypos,2}` table of `{tag,I}` objects |
//!
//! # How a fact is compared
//!
//! Both sides render every observable as **canonical text** and the two fact
//! lists are compared **name by name, in order** — a missing, extra or renamed
//! fact is a failure, not a silently skipped comparison. The renderings are the
//! subset of `erl`'s `~w` these shapes need (a list of small integers, a list of
//! two-element tuples, a key or the `'$end_of_table'` atom), so an ordering
//! divergence is visible directly in the diff. The `dups` shape cannot use that
//! rendering — its keys are floats, bignums, atoms, tuples, lists and binaries —
//! so it renders every key as its **1-based index in a key pool written out
//! identically (index for index) on both sides**, exactly like
//! `ets_ordered_oracle.rs`: the index is a *term identity*, never a coercion.
//!
//! # Crate-only assertions
//!
//! `ets:info(T, memory)` has no comparable oracle figure (the two runtimes
//! account allocation differently — see `tests/ets_capacity.rs`), so the
//! reported words are asserted **crate-side only**: positive, monotone in the
//! live key count, bounded by [`WORDS_PER_ENTRY_MAX`] words per live entry, and
//! falling back to the fresh empty floor once a bulk teardown has removed every
//! key (a teardown may not keep the high-water mark).
//!
//! # Running it
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_ets --test ets_ordered_stress_oracle --no-fail-fast
//! ```
//!
//! The oracle is **required by default** (see `common::oracle`): a machine
//! without Erlang fails these tests rather than passing them vacuously;
//! `BEAM_ETS_ALLOW_NO_ORACLE=1` is the explicit opt-out (a loud skip). Every
//! scenario is an always-on `#[test]`; the whole file runs in a few seconds next
//! to the oracle.
//!
//! # Why this file exceeds the crate's ~1000-line convention
//!
//! It is one *comprehensive test module* (the documented exception: the Rust half
//! of each shape and the escript half that re-derives its facts must be read — and
//! kept — in lockstep, so splitting them across files would make a one-line
//! divergence a two-file edit). The bulk is the verbatim
//! [`STRESS_ORACLE_PROGRAM`] fixture (~380 lines, the same eight shapes written
//! in Erlang); the Rust half is eight small scenario builders over shared
//! rendering, walk and permutation helpers.

mod common;

use std::time::Duration;

use beam_ets::{
    compile, key_of, MatchSpec, OrderedInterior, Table, TableId, TableKind, TableOptions,
};
use beam_term::prelude::*;
use common::oracle;
use common::*;

/// The key count of the big shapes: far past the B+-tree's `ORDER = 32`, so the
/// tree has several levels and a delete really cascades.
const BIG: i64 = 10_000;

/// The `merge` shape's checkpoint stride (delete count between measurements).
const MERGE_STRIDE: i64 = 2_000;

/// The key count of the `select` shape (prime, and far past `ORDER`).
const SELECT_N: i64 = 997;

/// The seed of the deterministic insert/delete permutation.
const PERM_SEED: u64 = 0x5EED_1234;

/// The B+-tree fan-out (`ordered_interior::node::ORDER`, crate-private). The
/// `boundary` shape walks key counts just below/at/above its multiples.
const ORDER: i64 = 32;

/// The `first`+`next` probe stride: every `PROBE_STRIDE`-th ascending key is
/// looked up (plus the boundaries and absent keys around them).
const PROBE_STRIDE: i64 = 997;

/// The upper bound `ets:info(T, memory)` must respect per **live** entry. The
/// bound is deliberately coarse — the B+-tree measures ≈ 8 words per entry, so a
/// per-entry leak or a tree of half-empty nodes (twice the nodes, each with its
/// own `Vec`) fails it, while the arena's spare capacity does not.
const WORDS_PER_ENTRY_MAX: usize = 64;

/// Wall-clock bound for one oracle scenario (the biggest writes ~500 kB of facts
/// and builds two 10 000-key tables: seconds, not minutes).
const ORACLE_TIMEOUT: Duration = Duration::from_secs(180);

/// One scenario's observable facts, in the order both sides emit them.
type Facts = Vec<(String, String)>;

// ---------------------------------------------------------------------------
// Table construction
// ---------------------------------------------------------------------------

/// A fresh `ordered_set` table on the **default** interior, which this file
/// asserts is the K-ary B+-tree (the subject of every shape here).
fn ordered_table() -> Table {
    let table = new_table(TableKind::OrderedSet);
    assert_eq!(
        table.ordered_interior(),
        OrderedInterior::BPlusTree,
        "these stress shapes must drive the default B+-tree interior"
    );
    table
}

/// A fresh `{keypos,2}` `ordered_set` table, again on the default B+-tree.
fn ordered_table_keypos2() -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::OrderedSet);
    opts.set_keypos(2).expect("keypos 2 is valid");
    let table = Table::new(TableId::next(), opts).expect("a fresh table is constructible");
    assert_eq!(
        table.ordered_interior(),
        OrderedInterior::BPlusTree,
        "the keypos-2 stress shape must drive the default B+-tree interior"
    );
    assert_eq!(table.keypos(), 2, "the key is element 2 of the object");
    table
}

/// A heap with room for `tables` full 10 000-object fills (three store words per
/// object; the bump heap never reallocates).
fn insert_heap(tables: usize) -> Heap {
    Heap::new(4 * tables * BIG as usize)
}

// ---------------------------------------------------------------------------
// Term rendering — the canonical form both sides agree on
// ---------------------------------------------------------------------------

/// A bare small-integer **term** (a key as the interior hands it back) as `i64`.
fn int_value(term: Eterm) -> i64 {
    assert!(
        is_small(term),
        "every key of these shapes is a small integer"
    );
    signed_value(term)
}

/// A small-integer element of a term, read through the crate's **safe** object
/// accessor ([`key_of`]): every object in these shapes is a two-element tuple of
/// small integers, so the accessor's arity validation always succeeds.
fn int_element(object: Eterm, pos: usize) -> i64 {
    int_value(key_of(object, pos).expect("every object here is a {Key, Value} tuple"))
}

/// The `(key, value)` of a `{Key, Value}` object.
fn pair_of_object(object: Eterm) -> (i64, i64) {
    (int_element(object, 1), int_element(object, 2))
}

/// `[(k,v),…]` — the `~w` rendering of a list of `{Key, Value}` objects.
fn render_objects(objects: &[Eterm]) -> String {
    render_pairs(&objects.iter().map(|o| pair_of_object(*o)).collect::<Vec<_>>())
}

/// `[k,…]` — the `~w` rendering of a list of small-integer keys.
fn render_keys(keys: &[Eterm]) -> String {
    render_ints(&keys.iter().map(|k| int_value(*k)).collect::<Vec<_>>())
}

/// `[{tag,i},…]` — the `~w` rendering of the `{keypos,2}` shape's objects.
fn render_tag_objects(objects: &[Eterm], tag: Eterm, atoms: &AtomTable) -> String {
    let items: Vec<String> = objects
        .iter()
        .map(|object| {
            let element = key_of(*object, 1).expect("a {tag, Key} tuple");
            assert!(
                eq(element, tag, atoms),
                "every object of the keypos-2 shape is tagged `tag`"
            );
            format!("{{tag,{}}}", int_element(*object, 2))
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// A key, or the `'$end_of_table'` sentinel the crate models as `None` —
/// rendered without quotes so it compares against the escript's own literal.
fn render_nav(key: Option<Eterm>) -> String {
    match key {
        Some(key) => int_value(key).to_string(),
        None => "$end_of_table".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Navigation and insertion drivers
// ---------------------------------------------------------------------------

/// The two navigation directions the walk helpers chain.
#[derive(Clone, Copy, Debug)]
enum Direction {
    Next,
    Prev,
}

impl Direction {
    /// One navigation step, with the `'$end_of_table'` sentinel as `None`. An
    /// `ordered_set` reads an **absent** anchor as a range bound, so this never
    /// fails for any anchor these shapes use.
    fn step(self, table: &Table, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        let stepped = match self {
            Direction::Next => table.next(key, atoms),
            Direction::Prev => table.prev(key, atoms),
        };
        stepped.unwrap_or_else(|error| panic!("ordered_set {self:?} from {key:?}: {error:?}"))
    }

    /// The walk's first key: `first`/`last` for the whole-table walks.
    fn start(self, table: &Table, atoms: &AtomTable) -> Option<Eterm> {
        match self {
            Direction::Next => table.first(atoms),
            Direction::Prev => table.last(atoms),
        }
    }
}

/// Chain `advance` from `seed` until the sentinel, collecting every key in visit
/// order — THE walk helper every walk fact here shares (the escript's `walk/2`
/// mirrors it exactly).
fn walk(seed: Option<Eterm>, mut advance: impl FnMut(Eterm) -> Option<Eterm>) -> Vec<Eterm> {
    let mut keys = Vec::new();
    let mut cursor = seed;
    while let Some(key) = cursor {
        keys.push(key);
        cursor = advance(key);
    }
    keys
}

/// A whole-table walk in `dir` (the `first`+`next` / `last`+`prev` chains).
fn walk_table(table: &Table, atoms: &AtomTable, dir: Direction) -> Vec<Eterm> {
    let seed = dir.start(table, atoms);
    walk(seed, |key| dir.step(table, key, atoms))
}

/// The first `limit` keys a whole-table walk in `dir` visits (the escript's
/// `head_of_walk/3`).
fn walk_head(table: &Table, atoms: &AtomTable, dir: Direction, limit: usize) -> Vec<Eterm> {
    let mut keys = Vec::new();
    let mut cursor = dir.start(table, atoms);
    while let (Some(key), true) = (cursor, keys.len() < limit) {
        keys.push(key);
        cursor = dir.step(table, key, atoms);
    }
    keys
}

/// Insert the `{key, value}` object (keypos 1) into `table`.
fn insert_pair(table: &mut Table, heap: &mut Heap, atoms: &AtomTable, key: i64, value: i64) {
    let object = tuple_of(heap, &[make_small(key), make_small(value)]);
    table
        .insert(object, atoms)
        .unwrap_or_else(|error| panic!("inserting key {key} into an ordered_set: {error:?}"));
}

/// Insert `keys` as `{key, key}` objects, recording `(size, memory words)` at
/// every `stride`-th insert — the crate-only `ets:info(T, memory)` trajectory.
fn fill_measured(
    table: &mut Table,
    heap: &mut Heap,
    atoms: &AtomTable,
    keys: &[i64],
    stride: usize,
) -> Vec<(usize, usize)> {
    let mut points = Vec::new();
    for (index, key) in keys.iter().enumerate() {
        insert_pair(table, heap, atoms, *key, *key);
        if (index + 1) % stride == 0 {
            points.push((table.size(), table.memory()));
        }
    }
    let last = (table.size(), table.memory());
    if points.last() != Some(&last) {
        points.push(last);
    }
    points
}

// ---------------------------------------------------------------------------
// The `ets:info(T, memory)` assertions (crate-only — no oracle counterpart)
// ---------------------------------------------------------------------------

/// Assert the reported words are positive, monotone in the live key count, within
/// [`WORDS_PER_ENTRY_MAX`] per live entry, and rising overall.
fn assert_memory_trajectory(shape: &str, empty: usize, points: &[(usize, usize)]) {
    assert!(
        !points.is_empty(),
        "{shape}: the memory trajectory recorded no checkpoint"
    );
    for &(size, words) in points {
        assert!(words > 0, "{shape}: memory must be positive, got {words}");
        let bound = WORDS_PER_ENTRY_MAX * size + empty;
        assert!(
            words <= bound,
            "{shape}: {words} words for {size} live entries exceeds the \
             {WORDS_PER_ENTRY_MAX}-words-per-entry bound ({bound} with the {empty}-word \
             empty floor)"
        );
    }
    for pair in points.windows(2) {
        let ((smaller, before), (larger, after)) = (pair[0], pair[1]);
        assert!(
            before <= after,
            "{shape}: the figure fell from {before} to {after} words while the table \
             grew from {smaller} to {larger} entries"
        );
    }
    let (first_size, first_words) = points[0];
    let (last_size, last_words) = points[points.len() - 1];
    assert!(
        last_words > first_words,
        "{shape}: the loaded figure ({last_words} words for {last_size} entries) never \
         rose above the {first_words}-word figure for {first_size} entries"
    );
}

/// Assert a drained table gave its index capacity back: the figure returned to
/// the fresh empty floor, and is far below the loaded one (no high-water mark).
fn assert_teardown_released_memory(shape: &str, empty: usize, loaded: usize, drained: usize) {
    assert_eq!(
        drained, empty,
        "{shape}: a fully drained table must report the fresh empty floor \
         ({empty} words), not {drained}"
    );
    assert!(
        drained * 10 < loaded,
        "{shape}: the drained figure ({drained}) did not fall well below the loaded \
         one ({loaded})"
    );
}

// ---------------------------------------------------------------------------
// The per-key probes (`next`/`prev`/`member`/`lookup` on present and absent keys)
// ---------------------------------------------------------------------------

/// A probe key: an exact (small-integer) key or a floating-point **bound**.
#[derive(Clone, Copy)]
enum ProbeKey {
    Int(i64),
    Float(f64),
}

/// The keys every full-shape scenario probes, with the fact-name component both
/// sides spell identically. Absent integers (`0`, `10001`, `12345`, `-3`), floats
/// that fall *between* two stored keys (`0.5`, `997.5`, `10000.5`) and the
/// arithmetic-equal `PROBE_STRIDE`-th keys are all covered.
fn probe_keys() -> Vec<(String, ProbeKey)> {
    let mut probes: Vec<(String, ProbeKey)> = vec![
        ("1".to_string(), ProbeKey::Int(1)),
        (PROBE_STRIDE.to_string(), ProbeKey::Int(PROBE_STRIDE)),
        ("5000".to_string(), ProbeKey::Int(5000)),
        ("9999".to_string(), ProbeKey::Int(9999)),
        ("10000".to_string(), ProbeKey::Int(10_000)),
        ("0".to_string(), ProbeKey::Int(0)),
        ("10001".to_string(), ProbeKey::Int(10_001)),
        ("12345".to_string(), ProbeKey::Int(12_345)),
        ("-3".to_string(), ProbeKey::Int(-3)),
        ("0.5".to_string(), ProbeKey::Float(0.5)),
        ("997.5".to_string(), ProbeKey::Float(997.5)),
        ("10000.5".to_string(), ProbeKey::Float(10_000.5)),
    ];
    for multiple in 1..=10 {
        probes.push((
            (PROBE_STRIDE * multiple).to_string(),
            ProbeKey::Int(PROBE_STRIDE * multiple),
        ));
    }
    probes
}

/// The term of a probe key.
fn probe_term(key: ProbeKey) -> Eterm {
    match key {
        ProbeKey::Int(value) => make_small(value),
        ProbeKey::Float(value) => flonum(value),
    }
}

/// Record the four per-key probes for every [`probe_keys`] entry.
fn record_probes(facts: &mut Facts, prefix: &str, table: &Table, atoms: &AtomTable) {
    for (name, key) in probe_keys() {
        let term = probe_term(key);
        facts.push((format!("{prefix}.next.{name}"), render_nav(Direction::Next.step(table, term, atoms))));
        facts.push((format!("{prefix}.prev.{name}"), render_nav(Direction::Prev.step(table, term, atoms))));
        facts.push((format!("{prefix}.member.{name}"), table.member(term, atoms).to_string()));
        facts.push((format!("{prefix}.lookup.{name}"), render_objects(&table.lookup(term, atoms))));
    }
}

// ---------------------------------------------------------------------------
// The deterministic permutation
// ---------------------------------------------------------------------------

/// A SplitMix64 stream: the Fisher–Yates draws of [`shuffled`], reproduced
/// bit-for-bit by the escript's `sm_next/1` (`(…) band 16#FFFFFFFFFFFFFFFF` is
/// what Rust's `wrapping_*` compute).
struct SplitMix64(u64);

impl SplitMix64 {
    /// Advance the stream and return the well-mixed value.
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A draw in `0..bound`.
    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// A Fisher–Yates permutation of `1..=n`, seeded deterministically. Both sides
/// generate it with the same draw order, so the insert (and the `merge` shape's
/// delete) orders stay in lockstep index for index.
fn shuffled(n: u64, seed: u64) -> Vec<i64> {
    let mut keys: Vec<i64> = (1..=n as i64).collect();
    let mut rng = SplitMix64(seed);
    for i in (1..n as usize).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        keys.swap(i, j);
    }
    keys
}

// ---------------------------------------------------------------------------
// Shapes 1–3: the full 10 000-key builds
// ---------------------------------------------------------------------------

/// The six full-table observables every big shape compares.
fn record_full_shapes(facts: &mut Facts, prefix: &str, table: &Table, atoms: &AtomTable) {
    facts.push((format!("{prefix}.size"), table.size().to_string()));
    facts.push((format!("{prefix}.first"), render_nav(table.first(atoms))));
    facts.push((format!("{prefix}.last"), render_nav(table.last(atoms))));
    facts.push((format!("{prefix}.tab2list"), render_objects(&table.tab2list())));
    facts.push((
        format!("{prefix}.walk_next"),
        render_keys(&walk_table(table, atoms, Direction::Next)),
    ));
    facts.push((
        format!("{prefix}.walk_prev"),
        render_keys(&walk_table(table, atoms, Direction::Prev)),
    ));
}

/// Run one full-shape build: `keys` are inserted as `{key, key}` objects, then
/// the six full-table observables and the per-key probes are recorded, and the
/// crate-only memory trajectory is asserted.
fn full_shape_facts(
    facts: &mut Facts,
    prefix: &str,
    keys: &[i64],
    before_fill: impl FnOnce(&mut Table, &mut Heap, &AtomTable, &mut Facts),
) {
    let atoms = AtomTable::new();
    let mut heap = insert_heap(1);
    let mut table = ordered_table();
    let empty = table.memory();
    before_fill(&mut table, &mut heap, &atoms, facts);
    let measured_from = table.size();
    let points = fill_measured(&mut table, &mut heap, &atoms, keys, 1_000);
    assert_eq!(
        table.size(),
        keys.len() + measured_from,
        "{prefix}: every key is stored exactly once"
    );
    record_full_shapes(facts, prefix, &table, &atoms);
    record_probes(facts, prefix, &table, &atoms);
    assert_memory_trajectory(prefix, empty, &points);
}

/// Shape 1 — 10 000 **ascending** inserts.
fn ascending_facts() -> Facts {
    let mut facts = Vec::new();
    let keys: Vec<i64> = (1..=BIG).collect();
    full_shape_facts(&mut facts, "asc", &keys, |_, _, _, _| {});
    facts
}

/// Shape 2 — 10 000 **descending** inserts.
fn descending_facts() -> Facts {
    let mut facts = Vec::new();
    let keys: Vec<i64> = (1..=BIG).rev().collect();
    full_shape_facts(&mut facts, "desc", &keys, |_, _, _, _| {});
    facts
}

/// Shape 3 — 10 000 inserts in the deterministic **permutation** order,
/// checkpointed at 100 keys so the permutation itself is compared, not just its
/// (order-independent) result.
fn randomised_facts() -> Facts {
    let permutation = shuffled(BIG as u64, PERM_SEED);
    let mut facts = Vec::new();
    let (head, tail) = permutation.split_at(100);
    full_shape_facts(&mut facts, "rand", tail, |table, heap, atoms, facts| {
        for key in head {
            insert_pair(table, heap, atoms, *key, *key);
        }
        facts.push(("rand.ck100.tab2list".to_string(), render_objects(&table.tab2list())));
        facts.push(("rand.perm.head".to_string(), render_ints(&permutation[..8])));
        facts.push((
            "rand.perm.tail".to_string(),
            render_ints(&permutation[permutation.len() - 8..]),
        ));
        facts.push(("rand.perm.in_order".to_string(), render_ints(head)));
    });
    facts
}

// ---------------------------------------------------------------------------
// Shape 4: the merge cascade — delete the tree back down
// ---------------------------------------------------------------------------

/// The three teardown orders the `merge` shape runs.
fn delete_orders() -> [(&'static str, Vec<i64>); 3] {
    [
        ("asc", (1..=BIG).collect()),
        ("desc", (1..=BIG).rev().collect()),
        ("rand", shuffled(BIG as u64, PERM_SEED)),
    ]
}

/// Shape 4 — grow the ascending 10 000-key tree, then delete every key in
/// ascending / descending / permuted order, comparing the table's shape at every
/// [`MERGE_STRIDE`] deletion and asserting the crate's own figure falls back to
/// the empty floor.
fn merge_facts() -> Facts {
    let mut facts = Vec::new();
    for (order, deletes) in delete_orders() {
        let atoms = AtomTable::new();
        let mut heap = insert_heap(1);
        let mut table = ordered_table();
        let empty = table.memory();
        for key in 1..=BIG {
            insert_pair(&mut table, &mut heap, &atoms, key, key);
        }
        let loaded = table.memory();
        let gone = probe_term(ProbeKey::Int(deletes[0]));
        let mut trajectory = vec![(table.size(), loaded)];

        for (index, key) in deletes.iter().enumerate() {
            table
                .delete(make_small(*key), &atoms)
                .unwrap_or_else(|error| panic!("deleting key {key} from an ordered_set: {error:?}"));
            let deleted = index as i64 + 1;
            if deleted % MERGE_STRIDE != 0 {
                continue;
            }
            let checkpoint = format!("merge.{order}.{deleted}");
            // `ets:lookup(T, ets:first(T))` reads `'$end_of_table'`'s own answer
            // (`[]`) once the table has drained, so the empty case renders the
            // empty list rather than inventing a key.
            let lookup_at = |key: Option<Eterm>| match key {
                Some(key) => render_objects(&table.lookup(key, &atoms)),
                None => render_objects(&[]),
            };
            facts.push((format!("{checkpoint}.size"), table.size().to_string()));
            facts.push((format!("{checkpoint}.first"), render_nav(table.first(&atoms))));
            facts.push((format!("{checkpoint}.last"), render_nav(table.last(&atoms))));
            facts.push((
                format!("{checkpoint}.lookup_first"),
                lookup_at(table.first(&atoms)),
            ));
            facts.push((
                format!("{checkpoint}.lookup_last"),
                lookup_at(table.last(&atoms)),
            ));
            facts.push((
                format!("{checkpoint}.lookup_gone"),
                render_objects(&table.lookup(gone, &atoms)),
            ));
            facts.push((
                format!("{checkpoint}.walk_head"),
                render_keys(&walk_head(&table, &atoms, Direction::Next, 5)),
            ));
            trajectory.push((table.size(), table.memory()));
        }

        facts.push((format!("merge.{order}.empty.size"), table.size().to_string()));
        facts.push((format!("merge.{order}.empty.first"), render_nav(table.first(&atoms))));
        facts.push((format!("merge.{order}.empty.last"), render_nav(table.last(&atoms))));
        facts.push((format!("merge.{order}.empty.tab2list"), render_objects(&table.tab2list())));
        for key in 1..=5 {
            insert_pair(&mut table, &mut heap, &atoms, key, key);
        }
        facts.push((format!("merge.{order}.reuse"), render_objects(&table.tab2list())));

        // The teardown released the index capacity: the figure follows the live
        // key count down (never up) and lands on the fresh empty floor once the
        // table is drained. `reuse` re-inserted five keys, so measure the
        // trajectory's last point, which was taken with the table empty.
        for pair in trajectory.windows(2) {
            let ((more, before), (fewer, after)) = (pair[0], pair[1]);
            assert!(
                after <= before,
                "merge/{order}: the figure rose from {before} to {after} words while the \
                 table fell from {more} to {fewer} entries"
            );
        }
        let ((drained_size, drained), _) = trajectory
            .split_last()
            .expect("the trajectory has at least the loaded point");
        let (drained_size, drained) = (*drained_size, *drained);
        assert_eq!(drained_size, 0, "merge/{order}: the table drained");
        assert_teardown_released_memory(&format!("merge/{order}"), empty, loaded, drained);
    }
    facts
}

// ---------------------------------------------------------------------------
// Shape 5: the duplicate-boundary keys over a mixed-type pool
// ---------------------------------------------------------------------------

/// A binary term on `heap`, byte-aligned (the `dups` pool's two binaries).
fn binary(heap: &mut Heap, bytes: &[u8]) -> Eterm {
    let words = heap_bits_size_words(bytes.len() * 8);
    // SAFETY: `alloc_words(words)` hands back exactly the `heap_bits_size_words(n_bits)`
    // words `write_binary` writes; the heap outlives the term (it is never reset
    // here) and `bytes` is a byte-aligned payload.
    unsafe {
        let hp = heap
            .alloc_words(words)
            .expect("the dups heap has room for a binary");
        write_binary(hp, bytes).expect("a byte-aligned binary")
    }
}

/// The `dups` key pool, in the escript's `keys/0` order: 21 `=:=`-distinct terms
/// that cover every term **class** the interior's `cmp_arith` routing orders —
/// numbers (with the arithmetic-duplicate pairs `1`/`1.0`, `2`/`2.0`,
/// `-1`/`-1.0`), bignums, atoms, tuples, lists and binaries.
fn dups_pool(heap: &mut Heap, atoms: &mut AtomTable) -> Vec<Eterm> {
    let a = interned_atom(atoms, "a");
    vec![
        make_small(-1),
        make_small(0),
        make_small(1),
        flonum(1.0),
        make_small(2),
        flonum(2.0),
        flonum(1.5),
        flonum(-2.5),
        big(heap, false, &[1u64 << 60]),
        big(heap, false, &[(1u64 << 60) + 1]),
        // `-(1 bsl 70)`: two 64-bit limbs, so digit 0 is zero and digit 1 is
        // `2^6` (`64 * 2^64 == 2^70`).
        big(heap, true, &[0, 1u64 << 6]),
        interned_atom(atoms, "foo"),
        interned_atom(atoms, "bar"),
        binary(heap, &[1, 2, 3]),
        binary(heap, &[]),
        tuple_of(heap, &[a, make_small(1)]),
        tuple_of(heap, &[]),
        proper_list(heap, &[make_small(1), make_small(2)]),
        NIL,
        flonum(3.5),
        flonum(-1.0),
    ]
}

/// The `dups` script's operations, in the escript's `Ops` order.
#[derive(Clone, Copy)]
enum DupOp {
    Insert(usize, i64),
    InsertNew(usize, i64),
    Delete(usize),
    DeleteObject(usize, i64),
    Lookup(usize),
    Member(usize),
    List,
    SelectKeys,
    SelectReverseKeys,
    Size,
    First,
    Last,
    Next(usize),
    Prev(usize),
}

/// The 61-step `dups` script: build the pool up, cross the arithmetic-duplicate
/// boundaries, probe the navigation around them, upsert an equal key (which
/// replaces the stored **key term**, not only the value), and delete by each
/// boundary key.
const DUPS_OPS: &[DupOp] = &[
    DupOp::Insert(3, 10),
    DupOp::List,
    DupOp::Insert(4, 20),
    DupOp::List,
    DupOp::Lookup(3),
    DupOp::Lookup(4),
    DupOp::Size,
    DupOp::First,
    DupOp::Last,
    DupOp::Insert(5, 30),
    DupOp::Insert(6, 40),
    DupOp::Insert(1, 50),
    DupOp::Insert(21, 60),
    DupOp::Insert(7, 70),
    DupOp::Insert(8, 80),
    DupOp::Insert(9, 90),
    DupOp::Insert(10, 91),
    DupOp::Insert(11, 92),
    DupOp::Insert(12, 100),
    DupOp::Insert(13, 101),
    DupOp::Insert(14, 102),
    DupOp::Insert(15, 103),
    DupOp::Insert(16, 104),
    DupOp::Insert(17, 105),
    DupOp::Insert(18, 106),
    DupOp::Insert(19, 107),
    DupOp::Insert(20, 108),
    DupOp::List,
    DupOp::Size,
    DupOp::First,
    DupOp::Last,
    DupOp::Next(1),
    DupOp::Prev(3),
    DupOp::Next(2),
    DupOp::Prev(2),
    DupOp::Next(19),
    DupOp::Prev(15),
    DupOp::Next(17),
    DupOp::Next(11),
    DupOp::Prev(14),
    DupOp::InsertNew(12, 200),
    DupOp::InsertNew(13, 201),
    DupOp::InsertNew(2, 202),
    DupOp::List,
    DupOp::Member(2),
    DupOp::Insert(3, 300),
    DupOp::List,
    DupOp::Lookup(4),
    DupOp::SelectKeys,
    DupOp::SelectReverseKeys,
    DupOp::Delete(4),
    DupOp::Delete(3),
    DupOp::List,
    DupOp::Size,
    DupOp::Insert(4, 400),
    DupOp::Next(3),
    DupOp::First,
    DupOp::Last,
    DupOp::List,
    DupOp::DeleteObject(16, 104),
    DupOp::List,
    DupOp::Size,
];

/// The `dups` shape's read-only context: the pool (indexed for rendering) plus
/// the scratch heap the select family builds its results on.
struct Dups<'a> {
    pool: &'a [Eterm],
    atoms: &'a AtomTable,
    key_spec: &'a MatchSpec,
    out: beam_alloc::Heap,
}

impl Dups<'_> {
    /// A key term as its **1-based** pool index (`?` if it is outside the pool,
    /// which is itself a divergence and therefore shows up as a line mismatch).
    fn key(&self, key: Eterm) -> String {
        match self
            .pool
            .iter()
            .position(|candidate| eq(key, *candidate, self.atoms))
        {
            Some(index) => (index + 1).to_string(),
            None => "?".to_string(),
        }
    }

    /// An object as `{<key index>,<value>}`, a list of them as `[{…},…]`.
    fn objects(&self, objects: &[Eterm]) -> String {
        let items: Vec<String> = objects
            .iter()
            .map(|object| {
                let key = key_of(*object, 1).expect("every dups object is a {Key, Value} tuple");
                format!("{{{},{}}}", self.key(key), int_element(*object, 2))
            })
            .collect();
        format!("[{}]", items.join(","))
    }

    /// A key or the `'$end_of_table'` sentinel, as its pool index.
    fn nav(&self, key: Option<Eterm>) -> String {
        match key {
            Some(key) => self.key(key),
            None => "$end_of_table".to_string(),
        }
    }
}

/// Shape 5 — the duplicate-boundary keys, step by step.
fn duplicates_facts() -> Facts {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 12);
    let pool = dups_pool(&mut heap, &mut atoms);
    let key_spec = key_spec(&mut heap, &mut atoms);
    let mut dups = Dups {
        pool: &pool,
        atoms: &atoms,
        key_spec: &key_spec,
        out: beam_alloc::Heap::new(1 << 16).expect("a select heap is allocatable"),
    };
    let mut table = ordered_table();
    let mut facts = Vec::new();
    for (index, op) in DUPS_OPS.iter().enumerate() {
        let term = |pool_index: usize| pool[pool_index - 1];
        let result = match *op {
            DupOp::Insert(pool_index, value) => {
                let object = tuple_of(&mut heap, &[term(pool_index), make_small(value)]);
                match table.insert(object, dups.atoms) {
                    Ok(()) => "ok".to_string(),
                    Err(error) => panic!("dups step {index} insert: {error:?}"),
                }
            }
            DupOp::InsertNew(pool_index, value) => {
                let object = tuple_of(&mut heap, &[term(pool_index), make_small(value)]);
                match table.insert_new(object, dups.atoms) {
                    Ok(inserted) => inserted.to_string(),
                    Err(error) => panic!("dups step {index} insert_new: {error:?}"),
                }
            }
            DupOp::Delete(pool_index) => match table.delete(term(pool_index), dups.atoms) {
                Ok(()) => "ok".to_string(),
                Err(error) => panic!("dups step {index} delete: {error:?}"),
            },
            DupOp::DeleteObject(pool_index, value) => {
                let object = tuple_of(&mut heap, &[term(pool_index), make_small(value)]);
                match table.delete_object(object, dups.atoms) {
                    Ok(()) => "ok".to_string(),
                    Err(error) => panic!("dups step {index} delete_object: {error:?}"),
                }
            }
            DupOp::Lookup(pool_index) => dups.objects(&table.lookup(term(pool_index), dups.atoms)),
            DupOp::Member(pool_index) => table.member(term(pool_index), dups.atoms).to_string(),
            DupOp::List => dups.objects(&table.tab2list()),
            DupOp::SelectKeys | DupOp::SelectReverseKeys => {
                let selected = if matches!(*op, DupOp::SelectKeys) {
                    table.select(dups.key_spec, dups.atoms, &mut dups.out)
                } else {
                    table.select_reverse(dups.key_spec, dups.atoms, &mut dups.out)
                };
                match selected {
                    Ok(keys) => {
                        let items: Vec<String> = keys.iter().map(|key| dups.key(*key)).collect();
                        format!("[{}]", items.join(","))
                    }
                    Err(error) => panic!("dups step {index} select: {error:?}"),
                }
            }
            DupOp::Size => table.size().to_string(),
            DupOp::First => dups.nav(table.first(dups.atoms)),
            DupOp::Last => dups.nav(table.last(dups.atoms)),
            DupOp::Next(pool_index) => {
                dups.nav(Direction::Next.step(&table, term(pool_index), dups.atoms))
            }
            DupOp::Prev(pool_index) => {
                dups.nav(Direction::Prev.step(&table, term(pool_index), dups.atoms))
            }
        };
        facts.push((format!("dups.{index}"), result));
    }
    facts
}

// ---------------------------------------------------------------------------
// Shape 6: iteration across the split boundary
// ---------------------------------------------------------------------------

/// The key counts the `boundary` shape walks: just below, at and above the
/// multiples of `ORDER` (31/32/33, 63/64/65) plus two deeper trees.
const BOUNDARY_SIZES: &[i64] = &[31, 32, 33, 63, 64, 65, 96, 100];

/// Shape 6 — `first`+`next` and `last`+`prev` over split-boundary key counts,
/// including walks started from an **absent** anchor (a range bound).
fn boundary_facts() -> Facts {
    // The sizes must straddle the fan-out — just below, at and above a multiple of
    // `ORDER` — so an edited list cannot quietly stop testing a split boundary;
    // `96` is a third full level and `100` a deeper tree.
    for size in [
        ORDER - 1,
        ORDER,
        ORDER + 1,
        2 * ORDER - 1,
        2 * ORDER,
        2 * ORDER + 1,
    ] {
        assert!(
            BOUNDARY_SIZES.contains(&size),
            "the boundary key counts must include {size}"
        );
    }
    let atoms = AtomTable::new();
    let mut heap = insert_heap(1);
    let mut facts = Vec::new();
    for &size in BOUNDARY_SIZES {
        let mut table = ordered_table();
        for key in 1..=size {
            insert_pair(&mut table, &mut heap, &atoms, key, key);
        }
        let prefix = format!("b{size}");
        assert_eq!(table.size(), size as usize);
        facts.push((format!("{prefix}.size"), size.to_string()));
        facts.push((format!("{prefix}.first"), render_nav(table.first(&atoms))));
        facts.push((format!("{prefix}.last"), render_nav(table.last(&atoms))));
        facts.push((format!("{prefix}.tab2list"), render_objects(&table.tab2list())));
        facts.push((
            format!("{prefix}.walk_next"),
            render_keys(&walk_table(&table, &atoms, Direction::Next)),
        ));
        facts.push((
            format!("{prefix}.walk_prev"),
            render_keys(&walk_table(&table, &atoms, Direction::Prev)),
        ));
        facts.push((
            format!("{prefix}.next_above"),
            render_nav(Direction::Next.step(&table, make_small(size + 1), &atoms)),
        ));
        facts.push((
            format!("{prefix}.prev_below"),
            render_nav(Direction::Prev.step(&table, make_small(0), &atoms)),
        ));
        // An anchor strictly between two stored keys: `first`/`next` reads it as a
        // range bound, so the walk covers the suffix (resp. prefix) around the gap.
        let anchor = flonum(((size + 1) / 2) as f64 + 0.5);
        for (suffix, dir) in [("abs_next_walk", Direction::Next), ("abs_prev_walk", Direction::Prev)] {
            let seed = dir.step(&table, anchor, &atoms);
            let walked = walk(seed, |key| dir.step(&table, key, &atoms));
            facts.push((format!("{prefix}.{suffix}"), render_keys(&walked)));
        }
    }
    facts
}

// ---------------------------------------------------------------------------
// Shape 7: select in key order
// ---------------------------------------------------------------------------

/// Shape 7 — `select`/`select_reverse`/`select_count`/`match_object`/a guarded
/// `select`, then `select_delete` on `I rem 3 == 0` with the remaining table.
fn selection_facts() -> Facts {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 14);
    let key_spec = key_spec(&mut heap, &mut atoms);
    let count_spec = {
        let underscore = interned_atom(&mut atoms, "_");
        let head = tuple_of(&mut heap, &[underscore, underscore]);
        let body = proper_list(&mut heap, &[interned_atom(&mut atoms, "true")]);
        let clause = tuple_of(&mut heap, &[head, NIL, body]);
        compile(proper_list(&mut heap, &[clause]), &atoms).expect("the count spec compiles")
    };
    // `[{{'_','$1'},[{'=:=',{'rem','$1',3},0}],[true]}]` — the `I rem 3 == 0`
    // predicate of `ets:select_delete/2`.
    let delete_spec = {
        let underscore = interned_atom(&mut atoms, "_");
        let dollar1 = interned_atom(&mut atoms, "$1");
        let three = make_small(3);
        let rem = tuple_of(&mut heap, &[interned_atom(&mut atoms, "rem"), dollar1, three]);
        let equals = tuple_of(
            &mut heap,
            &[interned_atom(&mut atoms, "=:="), rem, make_small(0)],
        );
        let guard = proper_list(&mut heap, &[equals]);
        let head = tuple_of(&mut heap, &[underscore, dollar1]);
        let body = proper_list(&mut heap, &[interned_atom(&mut atoms, "true")]);
        let clause = tuple_of(&mut heap, &[head, guard, body]);
        compile(proper_list(&mut heap, &[clause]), &atoms).expect("the rem guard compiles")
    };
    // `[{{'$1','_'},[{'>','$1',990}],['$1']}]` — a lower-bound guard.
    let gt_spec = {
        let dollar1 = interned_atom(&mut atoms, "$1");
        let underscore = interned_atom(&mut atoms, "_");
        let gt = tuple_of(
            &mut heap,
            &[interned_atom(&mut atoms, ">"), dollar1, make_small(990)],
        );
        let guard = proper_list(&mut heap, &[gt]);
        let head = tuple_of(&mut heap, &[dollar1, underscore]);
        let body = proper_list(&mut heap, &[dollar1]);
        let clause = tuple_of(&mut heap, &[head, guard, body]);
        compile(proper_list(&mut heap, &[clause]), &atoms).expect("the range guard compiles")
    };
    let any_object = {
        let underscore = interned_atom(&mut atoms, "_");
        tuple_of(&mut heap, &[underscore, underscore])
    };

    let mut table = ordered_table();
    for key in 1..=SELECT_N {
        insert_pair(&mut table, &mut heap, &atoms, key, key * 2);
    }
    let mut out = beam_alloc::Heap::new(1 << 18).expect("a select heap is allocatable");
    let mut facts: Facts = vec![("sel.size".to_string(), table.size().to_string())];
    facts.push((
        "sel.select_keys".to_string(),
        render_keys(
            &table
                .select(&key_spec, &atoms, &mut out)
                .expect("the key spec selects"),
        ),
    ));
    facts.push((
        "sel.select_reverse_keys".to_string(),
        render_keys(
            &table
                .select_reverse(&key_spec, &atoms, &mut out)
                .expect("the key spec selects in reverse"),
        ),
    ));
    facts.push((
        "sel.select_count".to_string(),
        table
            .select_count(&count_spec, &atoms, &mut out)
            .expect("the count spec counts")
            .to_string(),
    ));
    facts.push((
        "sel.match_object".to_string(),
        render_objects(
            &table
                .match_object(any_object, &atoms, &mut out)
                .expect("the match spec matches every object"),
        ),
    ));
    facts.push((
        "sel.select_gt".to_string(),
        render_keys(
            &table
                .select(&gt_spec, &atoms, &mut out)
                .expect("the range guard selects"),
        ),
    ));
    let deleted = table
        .select_delete(&delete_spec, &atoms, &mut out)
        .expect("the rem predicate deletes");
    facts.push(("sel.select_delete".to_string(), deleted.to_string()));
    facts.push(("sel.after.size".to_string(), table.size().to_string()));
    facts.push(("sel.after.first".to_string(), render_nav(table.first(&atoms))));
    facts.push(("sel.after.last".to_string(), render_nav(table.last(&atoms))));
    facts.push((
        "sel.after.tab2list".to_string(),
        render_objects(&table.tab2list()),
    ));
    facts.push((
        "sel.after.walk_next".to_string(),
        render_keys(&walk_table(&table, &atoms, Direction::Next)),
    ));
    facts.push((
        "sel.after.walk_prev".to_string(),
        render_keys(&walk_table(&table, &atoms, Direction::Prev)),
    ));
    facts.push((
        "sel.after.select_count".to_string(),
        table
            .select_count(&count_spec, &atoms, &mut out)
            .expect("the count spec still counts")
            .to_string(),
    ));
    facts
}

// ---------------------------------------------------------------------------
// Shape 8: the {keypos, 2} table
// ---------------------------------------------------------------------------

/// The keys the `keypos2` shape probes (the last one is absent).
const KEYPOS2_PROBES: &[i64] = &[1, 997, 5000, 10_000, 10_001];

/// Shape 8 — the ascending 10 000-key shape on a `{keypos,2}` table of
/// `{tag, I}` objects, so key extraction/read path is stress-covered.
fn keypos2_facts() -> Facts {
    let mut atoms = AtomTable::new();
    let tag = interned_atom(&mut atoms, "tag");
    let mut heap = insert_heap(1);
    let mut table = ordered_table_keypos2();
    let empty = table.memory();
    let mut points = Vec::new();
    for key in 1..=BIG {
        let object = tuple_of(&mut heap, &[tag, make_small(key)]);
        table
            .insert(object, &atoms)
            .unwrap_or_else(|error| panic!("inserting keypos-2 key {key}: {error:?}"));
        if key % 1_000 == 0 {
            points.push((table.size(), table.memory()));
        }
    }
    assert_eq!(table.size(), BIG as usize);

    let mut facts: Facts = vec![("kp.size".to_string(), table.size().to_string())];
    facts.push(("kp.first".to_string(), render_nav(table.first(&atoms))));
    facts.push(("kp.last".to_string(), render_nav(table.last(&atoms))));
    facts.push((
        "kp.tab2list".to_string(),
        render_tag_objects(&table.tab2list(), tag, &atoms),
    ));
    facts.push((
        "kp.walk_next".to_string(),
        render_keys(&walk_table(&table, &atoms, Direction::Next)),
    ));
    facts.push((
        "kp.walk_prev".to_string(),
        render_keys(&walk_table(&table, &atoms, Direction::Prev)),
    ));
    for key in KEYPOS2_PROBES {
        let name = key.to_string();
        facts.push((
            format!("kp.lookup.{name}"),
            render_tag_objects(&table.lookup(make_small(*key), &atoms), tag, &atoms),
        ));
        let element = table.lookup_element(make_small(*key), 1, &atoms);
        facts.push((
            format!("kp.elem.{name}"),
            match element {
                Ok(term) if eq(term, tag, &atoms) => "tag".to_string(),
                Ok(_) => "?".to_string(),
                Err(error) => format!("{error:?}").to_lowercase(),
            },
        ));
        facts.push((
            format!("kp.next.{name}"),
            render_nav(Direction::Next.step(&table, make_small(*key), &atoms)),
        ));
    }
    assert_memory_trajectory("keypos2", empty, &points);
    facts
}

// ---------------------------------------------------------------------------
// The live oracle
// ---------------------------------------------------------------------------

/// Compare one scenario's facts against the live oracle, name by name and in
/// order.
///
/// The oracle is **required by default** (`common::oracle`): a machine without
/// Erlang fails the calling test. The escript writes its facts to a file rather
/// than to `stdout` — `run_bounded` reads the child's pipes only after it exits,
/// so a few hundred kB of facts on `stdout` would fill the pipe buffer and
/// deadlock the child.
fn compare_with_oracle(scenario: &str, ours: &Facts) {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            scenario,
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable \
             `escript` on $PATH)",
        );
        return;
    };

    let scratch = oracle::Scratch::new(&format!("ordered_stress_{scenario}"));
    let facts_path = scratch.path().join("facts.txt");
    let args = vec![
        scenario.to_string(),
        facts_path.to_string_lossy().into_owned(),
    ];
    oracle::run_escript(
        &escript,
        STRESS_ORACLE_PROGRAM,
        &args,
        &format!("ordered_stress_{scenario}"),
        ORACLE_TIMEOUT,
    );

    let text = std::fs::read_to_string(&facts_path).unwrap_or_else(|error| {
        panic!(
            "cannot read the oracle facts {}: {error}",
            facts_path.display()
        )
    });
    let theirs = parse_facts(&text);
    assert!(
        !theirs.is_empty(),
        "{scenario}: the oracle produced no facts — did the escript reject the scenario name?"
    );
    assert_eq!(
        ours.len(),
        theirs.len(),
        "{scenario}: the crate produced {} facts and the oracle {} — the two halves are \
         out of lockstep\n crate names: {:?}\n oracle names: {:?}",
        ours.len(),
        theirs.len(),
        clip_list(ours),
        clip_list(&theirs),
    );
    for (index, ((name, got), (theirs_name, want))) in ours.iter().zip(&theirs).enumerate() {
        assert_eq!(
            name, theirs_name,
            "{scenario}: fact #{index} is `{name}` on the crate side but `{theirs_name}` \
             on the oracle side"
        );
        assert_eq!(
            got,
            want,
            "{scenario}: fact `{name}` disagrees between the crate and the live oracle\n\
             \x20 crate: {}\n\x20oracle: {}",
            clip(got),
            clip(want),
        );
    }
    println!(
        "[ordered_stress/{scenario}] compared {} facts against the live oracle",
        ours.len()
    );
}

/// Parse `name -> value` lines into ordered pairs.
fn parse_facts(text: &str) -> Facts {
    text.lines()
        .filter_map(|line| {
            let (name, value) = line.trim().split_once(" -> ")?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

/// A rendering clipped to its first [`CLIP`] bytes, so a mismatch on a 10 000
/// object line stays readable (the whole line is still in the fact itself).
fn clip(value: &str) -> String {
    const CLIP: usize = 300;
    if value.len() <= CLIP {
        return value.to_string();
    }
    let mut end = CLIP;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes total)", &value[..end], value.len())
}

/// The names of a fact list, clipped to the first ten (the lockstep diagnostic).
fn clip_list(facts: &Facts) -> Vec<&str> {
    facts
        .iter()
        .take(10)
        .map(|(name, _)| name.as_str())
        .collect()
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// Shape 1 — 10 000 ascending inserts: the right-edge split cascade, including
/// root splits.
#[test]
fn ascending_ten_thousand_inserts_match_the_live_oracle() {
    compare_with_oracle("asc", &ascending_facts());
}

/// Shape 2 — 10 000 descending inserts: the left-edge split cascade.
#[test]
fn descending_ten_thousand_inserts_match_the_live_oracle() {
    compare_with_oracle("desc", &descending_facts());
}

/// Shape 3 — 10 000 inserts in a deterministic permutation: splits interleaved
/// across the whole key space.
#[test]
fn permuted_ten_thousand_inserts_match_the_live_oracle() {
    compare_with_oracle("rand", &randomised_facts());
}

/// Shape 4 — deleting the whole tree back down, in three orders: the
/// borrow-or-merge cascade, checkpointed every [`MERGE_STRIDE`] deletions.
#[test]
fn deleting_the_tree_back_down_matches_the_live_oracle() {
    compare_with_oracle("merge", &merge_facts());
}

/// Shape 5 — the arithmetic-duplicate keys across every term class.
#[test]
fn duplicate_boundary_keys_match_the_live_oracle() {
    compare_with_oracle("dups", &duplicates_facts());
}

/// Shape 6 — walks over key counts just below/at/above the fan-out multiples,
/// including walks started from an absent anchor.
#[test]
fn walks_across_the_split_boundary_match_the_live_oracle() {
    compare_with_oracle("boundary", &boundary_facts());
}

/// Shape 7 — the select family in key order, ending in a `select_delete`.
#[test]
fn select_in_key_order_matches_the_live_oracle() {
    compare_with_oracle("select", &selection_facts());
}

/// Shape 8 — the `{keypos,2}` table at 10 000 keys.
#[test]
fn a_keypos_two_ten_thousand_key_tree_matches_the_live_oracle() {
    compare_with_oracle("keypos2", &keypos2_facts());
}

/// The scenario's fact writer, reproduced index for index on both sides.
pub const STRESS_ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
%%! -noshell
-define(LB, 123).
-define(RB, 125).

main([Which, OutPath]) ->
    {ok, F} = file:open(OutPath, [write, raw, binary]),
    P = fun(Name, Value) -> ok = file:write(F, [Name, " -> ", Value, $\n]) end,
    case Which of
        "asc" -> ascending(P);
        "desc" -> descending(P);
        "rand" -> randomised(P);
        "merge" -> merge(P);
        "dups" -> duplicates(P);
        "boundary" -> boundary(P);
        "select" -> selection(P);
        "keypos2" -> keypos2(P);
        Other -> file:close(F), io:format("unknown scenario ~p~n", [Other]), halt(2)
    end,
    file:close(F),
    ok.

%% --- rendering --------------------------------------------------------
r_ints(L) -> [$[, lists:join(",", [integer_to_list(I) || I <- L]), $]].
r_objs(L) ->
    [$[, lists:join(",", [[?LB, integer_to_list(K), $,, integer_to_list(V), ?RB]
                          || {K, V} <- L]), $]].
r_tags(L) ->
    [$[, lists:join(",", [[?LB, "tag,", integer_to_list(I), ?RB] || {tag, I} <- L]), $]].
r_end('$end_of_table') -> "$end_of_table";
r_end(I) -> integer_to_list(I).
idx(K, [K | _], I) -> integer_to_list(I);
idx(K, [_ | T], I) -> idx(K, T, I + 1).
r_idxlist(L, Keys) -> [$[, lists:join(",", [idx(K, Keys, 1) || K <- L]), $]].
r_idxobjs(L, Keys) ->
    [$[, lists:join(",", [[?LB, idx(K, Keys, 1), $,, integer_to_list(V), ?RB]
                          || {K, V} <- L]), $]].
r_idxnav('$end_of_table', _Keys) -> "$end_of_table";
r_idxnav(K, Keys) -> idx(K, Keys, 1).
safe(F) -> try F() catch error:badarg -> "badarg" end.

walk('$end_of_table', _Next) -> [];
walk(K, Next) -> [K | walk(Next(K), Next)].
head_of_walk(_Next, 0, _T) -> [];
head_of_walk('$end_of_table', _N, _T) -> [];
head_of_walk(K, N, T) -> [K | head_of_walk(ets:next(T, K), N - 1, T)].

mk() -> ets:new(ot, [ordered_set, public]).
fill(T, Keys) -> [ets:insert(T, {I, I}) || I <- Keys], T.

full_shapes(P, Pre, T) ->
    P(Pre ++ ".size", integer_to_list(ets:info(T, size))),
    P(Pre ++ ".first", r_end(ets:first(T))),
    P(Pre ++ ".last", r_end(ets:last(T))),
    P(Pre ++ ".tab2list", r_objs(ets:tab2list(T))),
    P(Pre ++ ".walk_next", r_ints(walk(ets:first(T), fun(K) -> ets:next(T, K) end))),
    P(Pre ++ ".walk_prev", r_ints(walk(ets:last(T), fun(K) -> ets:prev(T, K) end))).

probes(P, Pre, T) ->
    Fixed = [{"1", 1}, {"997", 997}, {"5000", 5000}, {"9999", 9999},
             {"10000", 10000}, {"0", 0}, {"10001", 10001}, {"12345", 12345},
             {"-3", -3}, {"0.5", 0.5}, {"997.5", 997.5}, {"10000.5", 10000.5}],
    Multiples = [{integer_to_list(997 * I), 997 * I} || I <- lists:seq(1, 10)],
    lists:foreach(
      fun({N, K}) ->
          P(Pre ++ ".next." ++ N, r_end(ets:next(T, K))),
          P(Pre ++ ".prev." ++ N, r_end(ets:prev(T, K))),
          P(Pre ++ ".member." ++ N, atom_to_list(ets:member(T, K))),
          P(Pre ++ ".lookup." ++ N, r_objs(ets:lookup(T, K)))
      end, Fixed ++ Multiples).

%% --- 1..3: the full 10 000-key builds ---------------------------------
ascending(P) ->
    T = fill(mk(), lists:seq(1, 10000)),
    full_shapes(P, "asc", T),
    probes(P, "asc", T),
    ets:delete(T).

descending(P) ->
    T = fill(mk(), lists:seq(10000, 1, -1)),
    full_shapes(P, "desc", T),
    probes(P, "desc", T),
    ets:delete(T).

randomised(P) ->
    Perm = shuffled(10000, 16#5EED1234),
    {Head, Tail} = lists:split(100, Perm),
    T = fill(mk(), Head),
    P("rand.ck100.tab2list", r_objs(ets:tab2list(T))),
    P("rand.perm.head", r_ints(lists:sublist(Perm, 8))),
    P("rand.perm.tail", r_ints(lists:nthtail(length(Perm) - 8, Perm))),
    P("rand.perm.in_order", r_ints(Head)),
    fill(T, Tail),
    full_shapes(P, "rand", T),
    probes(P, "rand", T),
    ets:delete(T).

%% --- 4: the merge cascade ---------------------------------------------
merge(P) ->
    lists:foreach(
      fun({Order, Deletes}) -> teardown(P, Order, Deletes) end,
      [{"asc", lists:seq(1, 10000)},
       {"desc", lists:seq(10000, 1, -1)},
       {"rand", shuffled(10000, 16#5EED1234)}]).

teardown(P, Order, Deletes) ->
    T = fill(mk(), lists:seq(1, 10000)),
    Gone = hd(Deletes),
    _ = lists:foldl(
          fun(K, N0) ->
              ets:delete(T, K),
              N = N0 + 1,
              case N rem 2000 of
                  0 ->
                      Pre = "merge." ++ Order ++ "." ++ integer_to_list(N),
                      P(Pre ++ ".size", integer_to_list(ets:info(T, size))),
                      P(Pre ++ ".first", r_end(ets:first(T))),
                      P(Pre ++ ".last", r_end(ets:last(T))),
                      P(Pre ++ ".lookup_first", r_objs(ets:lookup(T, ets:first(T)))),
                      P(Pre ++ ".lookup_last", r_objs(ets:lookup(T, ets:last(T)))),
                      P(Pre ++ ".lookup_gone", r_objs(ets:lookup(T, Gone))),
                      P(Pre ++ ".walk_head", r_ints(head_of_walk(ets:first(T), 5, T)));
                  _ -> ok
              end,
              N
          end, 0, Deletes),
    End = "merge." ++ Order,
    P(End ++ ".empty.size", integer_to_list(ets:info(T, size))),
    P(End ++ ".empty.first", r_end(ets:first(T))),
    P(End ++ ".empty.last", r_end(ets:last(T))),
    P(End ++ ".empty.tab2list", r_objs(ets:tab2list(T))),
    fill(T, lists:seq(1, 5)),
    P(End ++ ".reuse", r_objs(ets:tab2list(T))),
    ets:delete(T).

%% --- 5: the duplicate-boundary keys ------------------------------------
keys() ->
    [-1, 0, 1, 1.0, 2, 2.0, 1.5, -2.5,
     (1 bsl 60), (1 bsl 60) + 1, -(1 bsl 70),
     foo, bar,
     <<1,2,3>>, <<>>,
     {a,1}, {},
     [1,2], [],
     3.5, -1.0].

duplicates(P) ->
    Keys = keys(),
    T = ets:new(dt, [ordered_set, public]),
    Ops = [{insert, 3, 10}, {list}, {insert, 4, 20}, {list}, {lookup, 3}, {lookup, 4},
           {size}, {first}, {last},
           {insert, 5, 30}, {insert, 6, 40}, {insert, 1, 50}, {insert, 21, 60},
           {insert, 7, 70}, {insert, 8, 80}, {insert, 9, 90}, {insert, 10, 91},
           {insert, 11, 92}, {insert, 12, 100}, {insert, 13, 101}, {insert, 14, 102},
           {insert, 15, 103}, {insert, 16, 104}, {insert, 17, 105}, {insert, 18, 106},
           {insert, 19, 107}, {insert, 20, 108},
           {list}, {size}, {first}, {last},
           {next, 1}, {prev, 3}, {next, 2}, {prev, 2}, {next, 19}, {prev, 15},
           {next, 17}, {next, 11}, {prev, 14},
           {insert_new, 12, 200}, {insert_new, 13, 201}, {insert_new, 2, 202},
           {list}, {member, 2},
           {insert, 3, 300}, {list}, {lookup, 4}, {select_keys}, {select_reverse_keys},
           {delete, 4}, {delete, 3}, {list}, {size},
           {insert, 4, 400}, {next, 3}, {first}, {last}, {list},
           {delete_object, 16, 104}, {list}, {size}],
    _ = lists:foldl(
          fun(Op, N) ->
              P("dups." ++ integer_to_list(N), dup_op(T, Keys, Op)),
              N + 1
          end, 0, Ops),
    ets:delete(T).

dup_op(T, Keys, {insert, IX, V}) ->
    ets:insert(T, {lists:nth(IX, Keys), V}), "ok";
dup_op(T, Keys, {insert_new, IX, V}) ->
    atom_to_list(ets:insert_new(T, {lists:nth(IX, Keys), V}));
dup_op(T, Keys, {delete, IX}) ->
    ets:delete(T, lists:nth(IX, Keys)), "ok";
dup_op(T, Keys, {delete_object, IX, V}) ->
    ets:delete_object(T, {lists:nth(IX, Keys), V}), "ok";
dup_op(T, Keys, {lookup, IX}) -> r_idxobjs(ets:lookup(T, lists:nth(IX, Keys)), Keys);
dup_op(T, Keys, {member, IX}) -> atom_to_list(ets:member(T, lists:nth(IX, Keys)));
dup_op(T, Keys, {list}) -> r_idxobjs(ets:tab2list(T), Keys);
dup_op(T, Keys, {select_keys}) ->
    r_idxlist(ets:select(T, [{{'$1','_'},[],['$1']}]), Keys);
dup_op(T, Keys, {select_reverse_keys}) ->
    r_idxlist(ets:select_reverse(T, [{{'$1','_'},[],['$1']}]), Keys);
dup_op(T, _Keys, {size}) -> integer_to_list(ets:info(T, size));
dup_op(T, Keys, {first}) -> r_idxnav(ets:first(T), Keys);
dup_op(T, Keys, {last}) -> r_idxnav(ets:last(T), Keys);
dup_op(T, Keys, {next, IX}) -> r_idxnav(ets:next(T, lists:nth(IX, Keys)), Keys);
dup_op(T, Keys, {prev, IX}) -> r_idxnav(ets:prev(T, lists:nth(IX, Keys)), Keys).

%% --- 6: the split-boundary walks --------------------------------------
boundary(P) ->
    lists:foreach(
      fun(N) ->
          T = fill(mk(), lists:seq(1, N)),
          Pre = "b" ++ integer_to_list(N),
          P(Pre ++ ".size", integer_to_list(ets:info(T, size))),
          P(Pre ++ ".first", r_end(ets:first(T))),
          P(Pre ++ ".last", r_end(ets:last(T))),
          P(Pre ++ ".tab2list", r_objs(ets:tab2list(T))),
          P(Pre ++ ".walk_next", r_ints(walk(ets:first(T), fun(K) -> ets:next(T, K) end))),
          P(Pre ++ ".walk_prev", r_ints(walk(ets:last(T), fun(K) -> ets:prev(T, K) end))),
          P(Pre ++ ".next_above", r_end(ets:next(T, N + 1))),
          P(Pre ++ ".prev_below", r_end(ets:prev(T, 0))),
          Anchor = ((N + 1) div 2) + 0.5,
          P(Pre ++ ".abs_next_walk",
            r_ints(walk(ets:next(T, Anchor), fun(K) -> ets:next(T, K) end))),
          P(Pre ++ ".abs_prev_walk",
            r_ints(walk(ets:prev(T, Anchor), fun(K) -> ets:prev(T, K) end))),
          ets:delete(T)
      end, [31, 32, 33, 63, 64, 65, 96, 100]).

%% --- 7: the select family in key order --------------------------------
selection(P) ->
    T = ets:new(st, [ordered_set, public]),
    [ets:insert(T, {I, I * 2}) || I <- lists:seq(1, 997)],
    KeySpec = [{{'$1','_'},[],['$1']}],
    P("sel.size", integer_to_list(ets:info(T, size))),
    P("sel.select_keys", r_ints(ets:select(T, KeySpec))),
    P("sel.select_reverse_keys", r_ints(ets:select_reverse(T, KeySpec))),
    P("sel.select_count", integer_to_list(ets:select_count(T, [{{'_','_'},[],[true]}]))),
    P("sel.match_object", r_objs(ets:match_object(T, {'_','_'}))),
    P("sel.select_gt", r_ints(ets:select(T, [{{'$1','_'},[{'>','$1',990}],['$1']}]))),
    P("sel.select_delete",
      integer_to_list(ets:select_delete(T, [{{'_','$1'},[{'=:=',{'rem','$1',3},0}],[true]}]))),
    P("sel.after.size", integer_to_list(ets:info(T, size))),
    P("sel.after.first", r_end(ets:first(T))),
    P("sel.after.last", r_end(ets:last(T))),
    P("sel.after.tab2list", r_objs(ets:tab2list(T))),
    P("sel.after.walk_next", r_ints(walk(ets:first(T), fun(K) -> ets:next(T, K) end))),
    P("sel.after.walk_prev", r_ints(walk(ets:last(T), fun(K) -> ets:prev(T, K) end))),
    P("sel.after.select_count", integer_to_list(ets:select_count(T, [{{'_','_'},[],[true]}]))),
    ets:delete(T).

%% --- 8: the {keypos,2} table ------------------------------------------
keypos2(P) ->
    T = ets:new(kt, [ordered_set, public, {keypos, 2}]),
    [ets:insert(T, {tag, I}) || I <- lists:seq(1, 10000)],
    P("kp.size", integer_to_list(ets:info(T, size))),
    P("kp.first", r_end(ets:first(T))),
    P("kp.last", r_end(ets:last(T))),
    P("kp.tab2list", r_tags(ets:tab2list(T))),
    P("kp.walk_next", r_ints(walk(ets:first(T), fun(K) -> ets:next(T, K) end))),
    P("kp.walk_prev", r_ints(walk(ets:last(T), fun(K) -> ets:prev(T, K) end))),
    lists:foreach(
      fun(K) ->
          N = integer_to_list(K),
          P("kp.lookup." ++ N, r_tags(ets:lookup(T, K))),
          P("kp.elem." ++ N, safe(fun() -> atom_to_list(ets:lookup_element(T, K, 1)) end)),
          P("kp.next." ++ N, r_end(ets:next(T, K)))
      end, [1, 997, 5000, 10000, 10001]),
    ets:delete(T).

%% --- the deterministic permutation (SplitMix64 + Fisher-Yates) ---------
shuffled(N, Seed) ->
    Tup0 = list_to_tuple(lists:seq(1, N)),
    {Tup1, _} = lists:foldl(
                  fun(I, {Tup, X}) ->
                      {R, X1} = sm_next(X),
                      J = R rem (I + 1),
                      {swap(Tup, I, J), X1}
                  end, {Tup0, Seed}, lists:seq(N - 1, 1, -1)),
    tuple_to_list(Tup1).

%% Swap the 0-based positions I and J of a 1-based tuple.
swap(Tup, I, I) -> Tup;
swap(Tup, I, J) ->
    A = element(I + 1, Tup),
    B = element(J + 1, Tup),
    setelement(I + 1, setelement(J + 1, Tup, A), B).

%% SplitMix64: advance the state, then finalise it (the crate's `SplitMix64`).
sm_next(X0) ->
    M = 16#FFFFFFFFFFFFFFFF,
    X1 = (X0 + 16#9E3779B97F4A7C15) band M,
    Z2 = ((X1 bxor (X1 bsr 30)) * 16#BF58476D1CE4E5B9) band M,
    Z3 = ((Z2 bxor (Z2 bsr 27)) * 16#94D049BB133111EB) band M,
    {Z3 bxor (Z3 bsr 31), X1}.
"#;
