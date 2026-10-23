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
//! [`Table::stats`] — the interior-level contract behind `ets:info(Tab, stats)`.
//!
//! Three things are pinned here, and they are deliberately kept apart:
//!
//! 1. **The kind → answer mapping.** The oracle's `db_get_info` `stats` arm
//!    (`../../erts/emulator/beam/erl_db.c:5585-5629`) dispatches on the table's
//!    *type*: a hash table (`set`/`bag`/`duplicate_bag`) answers the eight-element
//!    occupancy tuple, a fine-locked `ordered_set` (a catree) answers the
//!    three-element node/depth tuple, and **every other `ordered_set` answers the
//!    atom `false`** — measured on the system `erl` (OTP 29.1.1 / erts-17.1) at 0, 5,
//!    1000 and 100 000 objects.
//! 2. **The interior-level invariants.** These interiors are *not* ERTS's (a CHAMP
//!    tree / a chained bucket array / a K-ary B+-tree against ERTS's bucket-chained
//!    hash table and catree), so the metric *values* legitimately differ and are
//!    never compared to the oracle's integers here. What must hold is that the
//!    figures are a real measurement of the interior: `sum(chain_len)` is the object
//!    count exactly once, `min <= avg <= max`, and the "expected" deviation is the
//!    binomial `sqrt(avg * (1 - 1/buckets))` for the reported bucket count.
//! 3. **The precision model.** ERTS types the three floats as `float`
//!    (`../../erts/emulator/beam/erl_db_hash.h:121-129`) and stores each result back
//!    into that field, so every observable is an `f32` (`db_get_info` widens it to a
//!    double for the tuple, `erl_db.c:5594-5607`). [`beam_ets::BucketChains::hash_stats`]
//!    reproduces the rounding; the same arithmetic applied to ERTS's own five-object
//!    histogram (5 objects over 256 buckets) must print the oracle's exact digits.
//!
//! The *live* oracle comparison of the tuple's shape and of the two elements that
//! agree by value (`kept_items`, `nlocks`) lives one layer up, in
//! `beam_bifs/tests/ets_info_stats.rs`, where the terms are built and rendered.
mod common;

use beam_ets::{
    Access, Table, TableId, TableKind, TableOptions, TableStats, WriteConcurrency,
};
use beam_term::prelude::*;
use common::*;

/// A table of `kind` with the default options.
fn table(kind: TableKind) -> Table {
    new_table(kind)
}

/// A table of `kind` carrying `mode` as its parsed `{write_concurrency, …}`.
fn wc_table(kind: TableKind, mode: WriteConcurrency) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(kind).set_write_concurrency_mode(mode);
    Table::new(TableId::next(), opts).expect("a fresh table is constructible")
}

/// Insert `n` distinct-key objects (`{K, 0}` … `{K+n-1, 0}`) into `t`.
fn fill(t: &mut Table, atoms: &AtomTable, heap: &mut beam_term::heap::Heap, n: i64) {
    for key in 0..n {
        t.insert(object(heap, key, 0), atoms).expect("insert");
    }
}

/// The `Hash` arm of `t.stats()`, with the label for the assertion messages.
fn hash_stats(t: &Table, label: &str) -> beam_ets::HashStats {
    match t.stats() {
        TableStats::Hash(stats) => stats,
        other => panic!("{label}: expected TableStats::Hash, got {other:?}"),
    }
}

/// The `Tree` arm of `t.stats()`.
fn tree_stats(t: &Table, label: &str) -> beam_ets::TreeStats {
    match t.stats() {
        TableStats::Tree(stats) => stats,
        other => panic!("{label}: expected TableStats::Tree, got {other:?}"),
    }
}

/// The interior invariants every `Hash` answer must satisfy, checked against the
/// table's own object count (`size`, which is what `sum(chain_len)` must be).
///
/// * `sum(chain_len) == size` — every object counted exactly once. The average is
///   `sum / buckets` rounded through `f32`, so it is checked with an `f32`-sized
///   relative tolerance rather than by exact equality.
/// * `min <= avg <= max`.
/// * `std_dev_expected == sqrt(avg * (1 - 1/buckets))` — **exactly**, both rounded
///   through `f32` like the C's `float` fields.
/// * every reported float is exactly representable in `f32` (the precision model).
fn check_hash_invariants(stats: &beam_ets::HashStats, size: usize, label: &str) {
    let n = stats.active_buckets as f64;
    assert!(n >= 1.0, "{label}: no bucket space");
    let sum = stats.avg_chain_len * n;
    // `f32` carries ~7 decimal digits; allow the rounding of `sum / n` and back.
    let tolerance = size.max(1) as f64 * 1.0e-5 + 1.0e-6;
    assert!(
        (sum - size as f64).abs() <= tolerance,
        "{label}: avg {n} buckets = {sum}, expected the object count {size}"
    );
    assert!(
        stats.min_chain_len as f64 <= stats.avg_chain_len,
        "{label}: min {} > avg {}",
        stats.min_chain_len,
        stats.avg_chain_len
    );
    assert!(
        stats.avg_chain_len <= stats.max_chain_len as f64,
        "{label}: avg {} > max {}",
        stats.avg_chain_len,
        stats.max_chain_len
    );
    for (name, value) in [
        ("avg_chain_len", stats.avg_chain_len),
        ("std_dev_chain_len", stats.std_dev_chain_len),
        ("std_dev_expected", stats.std_dev_expected),
    ] {
        assert_eq!(
            value as f32 as f64,
            value,
            "{label}: {name} {value} is not an f32-rounded figure"
        );
    }
    // The "expected" deviation is the binomial formula over the reported bucket
    // count (`db_calc_stats_hash`: `sqrt(avg * (1 - 1.0/NACTIVE))`).
    let expected = (stats.avg_chain_len * (1.0 - 1.0 / n)).max(0.0).sqrt();
    assert_eq!(
        stats.std_dev_expected,
        expected as f32 as f64,
        "{label}: std_dev_expected is not sqrt(avg * (1 - 1/buckets))"
    );
    // The real deviation is a standard deviation, so it is never negative.
    assert!(
        stats.std_dev_chain_len >= 0.0,
        "{label}: negative real deviation"
    );
}

/// Every hash kind answers [`TableStats::Hash`], and its occupancy accounts for
/// every object exactly once — for an empty table, a handful, and a table large
/// enough to make the chain histogram non-trivial.
#[test]
fn every_hash_kind_measures_its_own_occupancy() {
    let atoms = AtomTable::new();
    let mut heap = beam_term::heap::Heap::new(1 << 20);
    for kind in [
        TableKind::Set,
        TableKind::Bag,
        TableKind::DuplicateBag,
    ] {
        for size in [0i64, 5, 500] {
            let mut t = table(kind);
            fill(&mut t, &atoms, &mut heap, size);
            assert_eq!(t.size(), size as usize, "{kind:?}/{size}: size");
            let stats = hash_stats(&t, &format!("{kind:?}/{size}"));
            check_hash_invariants(&stats, size as usize, &format!("{kind:?}/{size}"));
            if size == 0 {
                // An empty table has no objects to average and no chain at all.
                assert_eq!(stats.avg_chain_len, 0.0, "{kind:?}: empty avg");
                assert_eq!(stats.std_dev_chain_len, 0.0, "{kind:?}: empty real");
                assert_eq!(stats.std_dev_expected, 0.0, "{kind:?}: empty expected");
                assert_eq!(stats.min_chain_len, 0, "{kind:?}: empty min");
                assert_eq!(stats.max_chain_len, 0, "{kind:?}: empty max");
            } else {
                assert_eq!(stats.min_chain_len, 0, "{kind:?}/{size}: an empty bucket exists");
                assert!(stats.max_chain_len >= 1, "{kind:?}/{size}: max");
            }
        }
    }
}

/// The chained interior's chain for one key is the key's whole object list: five
/// `duplicate_bag` objects under a single key all hash to the same bucket, so the
/// longest chain is exactly `5` whatever the bucket count is.
///
/// This is the property that makes the histogram a real measurement of *this*
/// interior: ERTS's chain counts one `DbTerm` per object the same way, which is why
/// the same table on the oracle reports a longest chain of `5`
/// (`{256,0.01953125,0.31188905239105225,0.13948102295398712,0,5,0,1}`).
#[test]
fn one_key_is_one_chain_in_the_chained_interior() {
    let atoms = AtomTable::new();
    let mut heap = beam_term::heap::Heap::new(1 << 16);
    let mut t = table(TableKind::DuplicateBag);
    for _ in 0..5 {
        t.insert(object(&mut heap, 1, 0), &atoms).expect("insert");
    }
    assert_eq!(t.size(), 5);
    let stats = hash_stats(&t, "duplicate_bag/5-under-one-key");
    check_hash_invariants(&stats, 5, "duplicate_bag/5-under-one-key");
    assert_eq!(stats.max_chain_len, 5, "all five objects share one bucket");
    assert_eq!(stats.min_chain_len, 0, "the other buckets are empty");
    // One chain of five over the bucket array is a far wider spread than a uniform
    // load would give, which is exactly what the pair of deviations is for.
    assert!(
        stats.std_dev_chain_len > stats.std_dev_expected,
        "a single five-long chain is skewed: {} vs {}",
        stats.std_dev_chain_len,
        stats.std_dev_expected
    );
}

/// The CHAMP `set` never chains (one object per key, one slot per entry), so its
/// longest chain is `1` for distinct keys — and its `sum(chain_len)` still counts
/// every object exactly once.
#[test]
fn the_champ_set_measures_one_slot_per_object() {
    let atoms = AtomTable::new();
    let mut heap = beam_term::heap::Heap::new(1 << 20);
    for size in [1i64, 5, 1000] {
        let mut t = table(TableKind::Set);
        fill(&mut t, &atoms, &mut heap, size);
        let stats = hash_stats(&t, &format!("set/{size}"));
        check_hash_invariants(&stats, size as usize, &format!("set/{size}"));
        assert_eq!(stats.max_chain_len, 1, "set/{size}: one object per slot");
        assert_eq!(stats.min_chain_len, 0, "set/{size}: free slots exist");
    }
}

/// A plain `ordered_set` answers [`TableStats::Absent`] — the oracle's atom
/// `false` — at **every** size, because only a fine-locked `ordered_set` is a
/// catree. Measured on the oracle at 0, 5, 1000 and 100 000 objects.
#[test]
fn a_plain_ordered_set_has_no_statistics() {
    let atoms = AtomTable::new();
    let mut heap = beam_term::heap::Heap::new(1 << 20);
    for size in [0i64, 5, 1000] {
        let mut t = table(TableKind::OrderedSet);
        fill(&mut t, &atoms, &mut heap, size);
        assert_eq!(
            t.stats(),
            TableStats::Absent,
            "ordered_set/{size} must answer false"
        );
    }
}

/// A `private` table's `{write_concurrency, true}` is normalized away, so its
/// `ordered_set` is **not** a catree: the oracle answers `false` there too
/// (measured: `ets:new(t, [ordered_set, private, {write_concurrency, true}])` then
/// `ets:info(t, stats)` ⇒ `false`).
#[test]
fn a_private_write_concurrent_ordered_set_is_not_a_catree() {
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::OrderedSet)
        .set_access(Access::Private)
        .set_write_concurrency(true);
    let t = Table::new(TableId::next(), opts).expect("a fresh table");
    assert!(!t.info().write_concurrency, "private normalizes to off");
    assert_eq!(t.stats(), TableStats::Absent);
}

/// A fine-locked `ordered_set` answers [`TableStats::Tree`]: the B+-tree's routing
/// nodes, its leaves and its deepest routing level.
///
/// The oracle's catree reports `{0,1,0}` at every size (its base node absorbs the
/// whole table); this crate's tree is a K-ary B+-tree, so the *values* are its own —
/// the **shape** is what is pinned here: exactly one base node for an empty or small
/// tree, routing nodes only once the root has split, and `max_depth == 0` iff there
/// are no routing nodes.
#[test]
fn a_fine_locked_ordered_set_reports_its_tree_shape() {
    let atoms = AtomTable::new();
    let mut heap = beam_term::heap::Heap::new(1 << 20);
    let t = wc_table(TableKind::OrderedSet, WriteConcurrency::On);
    // An empty tree is a single empty leaf — the oracle's own `{0,1,0}`.
    let empty = tree_stats(&t, "ordered_set+wc/0");
    assert_eq!(empty.base_nodes, 1, "an empty tree is one empty leaf");
    assert_eq!(empty.route_nodes, 0, "an empty tree routes nowhere");
    assert_eq!(empty.max_depth, 0, "an empty tree has depth 0");

    for size in [5i64, 1000] {
        let mut t = wc_table(TableKind::OrderedSet, WriteConcurrency::On);
        fill(&mut t, &atoms, &mut heap, size);
        let shape = tree_stats(&t, &format!("ordered_set+wc/{size}"));
        assert!(shape.base_nodes >= 1, "{size}: at least one leaf");
        if shape.route_nodes == 0 {
            assert_eq!(shape.max_depth, 0, "{size}: no routing levels without branches");
            assert_eq!(shape.base_nodes, 1, "{size}: a branch-less tree is one leaf");
        } else {
            assert!(
                shape.max_depth >= 1,
                "{size}: a branching tree has a routing level"
            );
            // Every routing level but the deepest carries at least two children, so
            // the depth is bounded by the number of routing nodes.
            assert!(
                shape.max_depth <= shape.route_nodes,
                "{size}: depth {} exceeds the routing node count {}",
                shape.max_depth,
                shape.route_nodes
            );
        }
    }
}

/// A table large enough to split must have produced routing nodes (the tree's
/// `MAX_OCCUPANCY` is far below 1000 keys), so the previous test's
/// "routes nowhere" arm is not the only one ever taken.
#[test]
fn a_large_ordered_set_has_routing_nodes() {
    let atoms = AtomTable::new();
    let mut heap = beam_term::heap::Heap::new(1 << 20);
    let mut t = wc_table(TableKind::OrderedSet, WriteConcurrency::On);
    fill(&mut t, &atoms, &mut heap, 1000);
    let shape = tree_stats(&t, "ordered_set+wc/1000");
    assert!(
        shape.route_nodes >= 1,
        "1000 keys must have split the root, got {shape:?}"
    );
    assert!(shape.base_nodes >= 2, "a split root has two leaves");
    // The split cascades (a 1000-key tree is two routing levels deep on this
    // B+-tree's order), so the depth is bounded by the routing node count rather than
    // fixed — the shape, not the tree's fan-out, is what is pinned.
    assert!(
        (1..=shape.route_nodes).contains(&shape.max_depth),
        "depth {} vs {} routing nodes",
        shape.max_depth,
        shape.route_nodes
    );
}

/// `kept_items` is the table's fixed-table retention count
/// ([`Table::deferred_objects`]) — the oracle's `kept_items`.
///
/// Measured on the oracle: a fixed five-object `set` with one object deleted reports
/// `{256,0.01953125,0.13838273286819458,0.13948102295398712,0,1,1,1}` — the deleted
/// object is still held (the seventh element is `1`, `size` is `4`) — and the same
/// table unfixed reports `0` there.
#[test]
fn kept_items_counts_the_fixed_tables_held_objects() {
    let atoms = AtomTable::new();
    let mut heap = beam_term::heap::Heap::new(1 << 16);
    let mut t = table(TableKind::Set);
    fill(&mut t, &atoms, &mut heap, 5);
    assert_eq!(hash_stats(&t, "unfixed").kept_items, 0);

    t.set_fixation(true);
    t.delete(make_small(0), &atoms).expect("delete");
    assert_eq!(t.size(), 4);
    assert_eq!(t.deferred_objects(), 1);
    assert_eq!(
        hash_stats(&t, "fixed, one deleted").kept_items,
        1,
        "the deleted object's storage is held while fixed"
    );

    // Unfixing releases the held storage, so the count returns to zero.
    t.set_fixation(false);
    assert_eq!(hash_stats(&t, "unfixed again").kept_items, 0);
}

/// `nlocks` is the effective `{write_concurrency, …}` mode's lock count
/// ([`WriteConcurrency::lock_count`]), and it agrees with the oracle **by value**:
/// `1` for a plain table, `64` for `true`/`auto`, the normalized count for the fixed
/// form, and `1` for a `private` table whatever was requested.
#[test]
fn nlocks_reports_the_effective_lock_count() {
    let cases = [
        (WriteConcurrency::Off, 1u32),
        (WriteConcurrency::On, 64),
        (WriteConcurrency::Auto, 64),
        (WriteConcurrency::FixedLocks(4), 64),
        (WriteConcurrency::FixedLocks(100), 64),
        (WriteConcurrency::FixedLocks(1024), 1024),
        (WriteConcurrency::FixedLocks(32768), 32768),
    ];
    for (mode, expected) in cases {
        // Every hash kind carries the same lock count for the same mode (the oracle's
        // five-object figures are identical for `set`/`bag`/`duplicate_bag`).
        for kind in [TableKind::Set, TableKind::Bag, TableKind::DuplicateBag] {
            let t = wc_table(kind, mode);
            assert_eq!(
                hash_stats(&t, &format!("{kind:?}/{mode:?}")).nlocks,
                expected,
                "{kind:?}/{mode:?}"
            );
        }
    }
    // The `DB_PRIVATE` gate: a private table never gets the fine-locked flag, so its
    // `nlocks` is `1` however the option asked.
    let mut opts = TableOptions::new();
    opts.set_access(Access::Private)
        .set_write_concurrency_mode(WriteConcurrency::FixedLocks(1024));
    let t = Table::new(TableId::next(), opts).expect("a fresh table");
    assert_eq!(hash_stats(&t, "private/fixed1024").nlocks, 1);
}

/// The precision model, end to end on ERTS's own five-object histogram: 5 objects
/// spread one per bucket over 256 buckets.
///
/// `db_calc_stats_hash` (`../../erts/emulator/beam/erl_db_hash.c:4072-4114`) computes
/// `avg = (float)sum / NACTIVE`, then each deviation from that **rounded** average
/// and stores the result back into a `float` field — so the three observables are
/// `f32`. Applying that model to ERTS's histogram must yield the digits the oracle
/// prints; the *double-only* arithmetic yields visibly different ones, which is why
/// the rounding is load-bearing rather than cosmetic.
///
/// The same model is applied by [`beam_ets::BucketChains::hash_stats`] to this
/// crate's own histograms (pinned in `src/stats.rs`'s unit tests, where the
/// histogram can be fed directly); this test pins the arithmetic itself so the
/// expectation is documented in the integration suite too.
#[test]
fn the_float_model_reproduces_the_oracles_five_object_digits() {
    let nactive = 256.0f64;
    let sum = 5.0f64;
    let sq_sum = 5.0f64;

    let avg = (sum / nactive) as f32 as f64;
    let real = ((sq_sum - avg * sum) / nactive).sqrt();
    let expected = (avg * (1.0 - 1.0 / nactive)).sqrt();

    assert_eq!(avg, 0.01953125, "the oracle's average chain length");
    assert_eq!(
        real as f32 as f64,
        0.138_382_732_868_194_58,
        "the oracle's real standard deviation"
    );
    assert_eq!(
        expected as f32 as f64,
        0.139_481_022_953_987_12,
        "the oracle's expected standard deviation"
    );
    // Without the `f32` rounding the printed digits would differ — the point of the
    // model.
    assert_ne!(real, 0.138_382_732_868_194_58);
    assert_ne!(expected, 0.139_481_022_953_987_12);
}
