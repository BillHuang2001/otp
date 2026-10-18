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

//! Interior index capacity follows the **live** object count.
//!
//! [`Table::memory`] (the crate's `ets:info(Tab, memory)`) is the table-owned term
//! storage **plus** the interior's own index storage. Every interior gives index
//! capacity back as objects leave a table — the chained hash rebuilds its bucket
//! array once live occupancy falls below an eighth of it (never below its starting
//! count), the CHAMP `set` trims the slot vectors of the nodes a removal touched,
//! the B+-tree compacts its node arena once its retired slots outnumber its live
//! ones and trims each leaf it removes from, the sorted-`Vec` `ordered_set` releases
//! its surplus, and [`Table::take`] releases an interior it drained — so a table
//! that grew to N objects and then shrank to one reports (and holds) storage
//! proportional to what it still **stores**, not to its high-water mark.
//!
//! This file is the permanent regression test for that behaviour:
//!
//! * **the per-interior trajectory** — empty → 10 000 objects → 1 left → emptied,
//!   for every `(kind, interior)` shape;
//! * **the `take` drain** — draining through `ets:take/2` returns the empty floor;
//! * **the bulk removals** — `select_delete`/`match_delete` route through
//!   `delete_object`, so they release capacity too;
//! * **a stable-size churn** — the shrink hysteresis must neither grow the figure
//!   nor make it shrink-and-regrow round after round;
//! * **the oracle cross-check** — the live system `erl`'s own trajectory for `set`
//!   and `ordered_set`, compared for **direction and rough magnitude only**.
//!
//! # What is NOT asserted
//!
//! The interiors' layouts are explicitly *not* externally pinned (crate-layer
//! `CONTEXT.md` §2: bucket count, hash-map thresholds, tree fan-out and arena shape
//! are interior freedoms), and `ets:info(T, memory)` counts a different allocation
//! bookkeeping in ERTS than in this crate. So no assertion here names an exact word
//! count for a **non-empty** table, and no crate figure is compared with an oracle
//! figure. What is pinned is the **shape**: the figure falls with the live data,
//! returns to the exact fresh floor once the table is empty, and does not churn —
//! which is exactly the observable the oracle agrees with.
//!
//! The oracle is **required by default** (see `common::oracle`); a machine without
//! Erlang cannot turn this evidence into a vacuous pass. `BEAM_ETS_ALLOW_NO_ORACLE=1`
//! opts out with a loud skip.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use beam_ets::{
    compile, MatchSpec, OrderedInterior, SetInterior, Table, TableId, TableKind, TableOptions,
};
use beam_term::prelude::*;
use common::oracle;
use common::*;

/// The object count of the trajectory tables: large enough that every interior has
/// grown well past its starting allocation, so a removal has something to give back.
const POPULATE: i64 = 10_000;

/// The object count of the churn probe (small enough that 200 rounds are cheap and
/// large enough that the interior has grown).
const CHURN_KEYS: i64 = 1_000;

/// The rounds of the stable-size churn probe (delete half, re-insert half).
const CHURN_ROUNDS: usize = 200;

/// One measured `(kind, interior)` shape: a display name plus its constructor.
///
/// The constructor is a plain `fn` pointer, so the shape list stays a value the
/// test can iterate without capturing anything.
struct Shape {
    /// The display name (kind and interior), used in every failure message.
    name: &'static str,
    /// Build a fresh, empty table of this shape.
    build: fn() -> Table,
}

/// A fresh table of `kind` with the requested `set`/`ordered_set` interiors.
///
/// The interior selectors only apply to their own kind (`Table::with_set_interior`
/// and `Table::with_ordered_interior` document that), so `bag`/`duplicate_bag` keep
/// their chained index whatever is passed.
fn fresh(kind: TableKind, set: SetInterior, ordered: OrderedInterior) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(kind);
    let id = TableId::next();
    match kind {
        TableKind::Set => Table::with_set_interior(id, opts, set),
        TableKind::OrderedSet => Table::with_ordered_interior(id, opts, ordered),
        TableKind::Bag | TableKind::DuplicateBag => Table::new(id, opts),
    }
    .expect("a fresh table is constructible")
}

/// Every shape the memory behaviour is proven for: both `set` interiors, both
/// `ordered_set` interiors, and the two chained-hash multi kinds.
fn shapes() -> Vec<Shape> {
    vec![
        Shape {
            name: "set/champ",
            build: || fresh(TableKind::Set, SetInterior::Champ, OrderedInterior::BPlusTree),
        },
        Shape {
            name: "set/chained",
            build: || fresh(TableKind::Set, SetInterior::Chained, OrderedInterior::BPlusTree),
        },
        Shape {
            name: "ordered_set/b+tree",
            build: || fresh(TableKind::OrderedSet, SetInterior::Champ, OrderedInterior::BPlusTree),
        },
        Shape {
            name: "ordered_set/sorted",
            build: || fresh(TableKind::OrderedSet, SetInterior::Champ, OrderedInterior::Sorted),
        },
        Shape {
            name: "bag",
            build: || fresh(TableKind::Bag, SetInterior::Champ, OrderedInterior::BPlusTree),
        },
        Shape {
            name: "duplicate_bag",
            build: || fresh(TableKind::DuplicateBag, SetInterior::Champ, OrderedInterior::BPlusTree),
        },
    ]
}

/// The atoms every trajectory needs: the payload value `x`.
fn trajectory_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.intern("x");
    atoms
}

/// A `{Key, x}` object on `heap` (keypos 1) — the object shape the trajectory
/// inserts. `x` is the atom payload, so the object is three store words.
fn capacity_object(heap: &mut Heap, key: i64, x: Eterm) -> Eterm {
    tuple_of(heap, &[make_small(key), x])
}

/// One shape's measured trajectory under [`Table::memory`].
///
/// Every field is a measurement of the *same* table, so the assertions compare a
/// shape against itself — never against another interior's absolute words.
#[derive(Debug)]
struct Trajectory {
    /// Words of a freshly created, still empty table.
    empty: usize,
    /// Words after `POPULATE` distinct keys were inserted.
    populated: usize,
    /// Words after `POPULATE - 1` of those keys were deleted (one object left).
    one_left: usize,
    /// Words after the **last** key was deleted one at a time.
    emptied_by_delete: usize,
    /// Words after re-populating and then `delete_all_objects`.
    emptied_by_delete_all: usize,
}

/// Walk one shape's trajectory: empty → `POPULATE` objects → 1 left → emptied.
fn trajectory(shape: &Shape) -> Trajectory {
    let mut atoms = trajectory_atoms();
    let x = interned_atom(&mut atoms, "x");
    // Two populate passes of three-word objects, plus slack.
    let mut heap = Heap::new(8 * POPULATE as usize);
    let mut t = (shape.build)();
    let empty = t.memory();

    for key in 0..POPULATE {
        let object = capacity_object(&mut heap, key, x);
        t.insert(object, &atoms)
            .expect("a distinct-key insert into a fresh table succeeds");
    }
    assert_eq!(
        t.size(),
        POPULATE as usize,
        "{}: every key is stored once",
        shape.name
    );
    let populated = t.memory();

    // Delete all but the last key: the interior has to give its index capacity back
    // as the live set collapses.
    for key in 0..POPULATE - 1 {
        t.delete(make_small(key), &atoms)
            .expect("a present key is deletable");
    }
    assert_eq!(t.size(), 1, "{}: one object left", shape.name);
    let one_left = t.memory();

    // Deleting the last object empties the table, which must land on the fresh floor.
    t.delete(make_small(POPULATE - 1), &atoms)
        .expect("the last key is deletable");
    assert_eq!(t.size(), 0, "{}: the table drained", shape.name);
    let emptied_by_delete = t.memory();

    // The `delete_all_objects` route to the same floor.
    for key in 0..POPULATE {
        let object = capacity_object(&mut heap, key, x);
        t.insert(object, &atoms)
            .expect("re-populating an emptied table succeeds");
    }
    let repopulated = t.memory();
    t.delete_all_objects();
    assert_eq!(t.size(), 0, "{}: delete_all_objects drained", shape.name);
    let emptied_by_delete_all = t.memory();

    let measured = Trajectory {
        empty,
        populated,
        one_left,
        emptied_by_delete,
        emptied_by_delete_all,
    };
    println!("[ets_capacity] {:<22} {measured:?} (re-populated {repopulated})", shape.name);
    measured
}

/// The upper bound a "one object left" figure must stay under — an absolute
/// sanity floor far below the 10 000-object figure (the measured after-values are
/// two orders of magnitude smaller: 9–50 words).
const ONE_LEFT_CEILING: usize = 10_000;

/// The per-interior trajectory: the reported words follow the live object count and
/// an emptied table returns to its exact fresh floor.
#[test]
fn interior_capacity_tracks_the_live_object_count_for_every_shape() {
    for shape in shapes() {
        let measured = trajectory(&shape);
        let what = shape.name;
        // Sanity: a loaded table really does own a large index, so the "falls" half
        // of the contract is a real drop rather than noise.
        assert!(
            measured.populated > measured.empty + 1_000,
            "{what}: the 10k-object figure dominates the empty floor \
             ({} -> {})",
            measured.empty,
            measured.populated
        );
        // One object left: the interior released the capacity the 9 999 removed
        // objects justified.
        assert!(
            measured.one_left < ONE_LEFT_CEILING,
            "{what}: one object left must not keep the 10k-object index \
             ({} words, ceiling {ONE_LEFT_CEILING}, populated {})",
            measured.one_left,
            measured.populated
        );
        assert!(
            measured.one_left * 50 < measured.populated,
            "{what}: the one-object figure is far below the loaded one \
             ({} vs {})",
            measured.one_left,
            measured.populated
        );
        // Emptied: exactly the freshly created table's footprint, by both routes.
        assert_eq!(
            measured.emptied_by_delete, measured.empty,
            "{what}: deleting the last object must return the fresh empty floor"
        );
        assert_eq!(
            measured.emptied_by_delete_all, measured.empty,
            "{what}: delete_all_objects must return the fresh empty floor"
        );
    }
}

/// Draining a table through [`Table::take`] (which hands the objects to the caller)
/// releases the index exactly like a delete does.
#[test]
fn draining_a_table_through_take_returns_it_to_the_empty_floor() {
    const N: i64 = 2_000;
    for shape in shapes() {
        let mut atoms = trajectory_atoms();
        let x = interned_atom(&mut atoms, "x");
        let mut heap = Heap::new(8 * N as usize);
        let mut t = (shape.build)();
        let empty = t.memory();

        for key in 0..N {
            let object = capacity_object(&mut heap, key, x);
            t.insert(object, &atoms).expect("insert into a fresh table");
        }
        let populated = t.memory();
        assert!(populated > empty, "{}: inserting grows the figure", shape.name);

        for key in 0..N {
            let taken = t
                .take(make_small(key), &atoms)
                .expect("taking a present key succeeds");
            assert_eq!(
                taken.len(),
                1,
                "{}: one object is taken out per key",
                shape.name
            );
        }
        assert_eq!(t.size(), 0, "{}: the table drained", shape.name);
        let drained = t.memory();
        println!(
            "[ets_capacity] {:<22} take drain: empty={empty} populated={populated} drained={drained}",
            shape.name
        );
        assert!(drained * 10 < populated, "{}: take released the index", shape.name);
        assert_eq!(
            drained, empty,
            "{}: a drained-by-take table returns to the fresh empty floor",
            shape.name
        );
    }
}

/// The number of objects the bulk-removal test leaves behind: the two erase paths
/// each remove the other 9 000, so the table stays loaded (the shrink must come from
/// the index, not from `release_empty_interior` rebuilding it).
const BULK_KEPT: i64 = 1_000;

/// The **bulk removals** release index capacity too: `select_delete` and
/// `match_delete` erase their objects through `delete_object`, so the same
/// per-removal accounting applies. Each path removes 9 000 of the 10 000 objects —
/// far past every interior's shrink threshold — and the table is then drained to
/// check the exact empty floor.
#[test]
fn bulk_select_and_match_delete_release_the_index_capacity() {
    /// The group value of the 9 000 objects that are bulk-removed.
    const REMOVED_GROUP: i64 = 0;
    /// The group value of the [`BULK_KEPT`] objects that survive.
    const KEPT_GROUP: i64 = 1;

    for shape in shapes() {
        let mut atoms = AtomTable::new();
        for name in ["$1", "$2", "<", "true", "_"] {
            atoms.intern(name);
        }
        // Every atom is interned up front, so the closures below only hold an
        // immutable borrow of the table.
        let atom = |name: &str| make_atom(atoms.lookup(name).expect("interned above"));
        let mut heap = Heap::new(16 * POPULATE as usize);
        let mut t = (shape.build)();
        let empty = t.memory();

        // `{Key, Group}`: the low `POPULATE - BULK_KEPT` keys carry `REMOVED_GROUP`,
        // the high `BULK_KEPT` keys `KEPT_GROUP`. One value convention lets *both*
        // erase paths name the same 9 000-object subset — a key-range guard for
        // `select_delete`, a head pattern for `match_delete`. The terms are built
        // once (they are immutable; the table deep-copies on insert).
        let first_kept = POPULATE - BULK_KEPT;
        let group_of = |key: i64| make_small(if key < first_kept { REMOVED_GROUP } else { KEPT_GROUP });
        let low: Vec<Eterm> = (0..first_kept)
            .map(|key| tuple_of(&mut heap, &[make_small(key), group_of(key)]))
            .collect();
        let high: Vec<Eterm> = (first_kept..POPULATE)
            .map(|key| tuple_of(&mut heap, &[make_small(key), group_of(key)]))
            .collect();
        let populate = |t: &mut Table, objects: &[Eterm]| {
            for &object in objects {
                t.insert(object, &atoms)
                    .expect("insert into a fresh table");
            }
        };
        populate(&mut t, &low);
        populate(&mut t, &high);
        let populated = t.memory();
        assert_eq!(t.size(), POPULATE as usize, "{}: loaded", shape.name);

        // Phase 1 — `[{{'$1','$2'}, [{'<','$1',9000}], [true]}]`: `select_delete` the
        // low key range.
        let head = tuple_of(&mut heap, &[atom("$1"), atom("$2")]);
        let guard = tuple_of(&mut heap, &[atom("<"), atom("$1"), make_small(first_kept)]);
        let guards = proper_list(&mut heap, &[guard]);
        let body = proper_list(&mut heap, &[atom("true")]);
        let clause = tuple_of(&mut heap, &[head, guards, body]);
        let spec_term = proper_list(&mut heap, &[clause]);
        let spec: MatchSpec = compile(spec_term, &atoms).expect("the range-guard spec compiles");
        let mut out = out_heap();
        let deleted = t
            .select_delete(&spec, &atoms, &mut out)
            .expect("select_delete with a `true` body");
        assert_eq!(
            deleted,
            (POPULATE - BULK_KEPT) as usize,
            "{}: select_delete removed the low key range",
            shape.name
        );
        assert_eq!(t.size(), BULK_KEPT as usize, "{}: a tenth left", shape.name);
        let after_select_delete = t.memory();
        println!(
            "[ets_capacity] {:<22} select_delete 90%: empty={empty} populated={populated} \
             after={after_select_delete}",
            shape.name
        );
        // The surviving tenth must not keep paying for the tenfold index. A quarter of
        // the loaded figure is far more than the freed 9 000 three-word objects alone
        // could account for, so this can only hold if the index gave capacity back.
        assert!(
            after_select_delete * 4 < populated,
            "{}: select_delete released no index capacity \
             ({populated} -> {after_select_delete})",
            shape.name
        );

        // Phase 2 — repopulate the erased key range and `match_delete({'_', 0})`: the
        // same 9 000 objects erased through the pattern path.
        populate(&mut t, &low);
        assert_eq!(t.size(), POPULATE as usize, "{}: reloaded", shape.name);
        let pattern = tuple_of(&mut heap, &[atom("_"), make_small(REMOVED_GROUP)]);
        let removed = t
            .match_delete(pattern, &atoms, &mut out)
            .expect("match_delete on a `{'_', 0}` pattern");
        assert_eq!(
            removed,
            (POPULATE - BULK_KEPT) as usize,
            "{}: match_delete removed the low-key group",
            shape.name
        );
        assert_eq!(t.size(), BULK_KEPT as usize, "{}: a tenth left", shape.name);
        let after_match_delete = t.memory();
        println!(
            "[ets_capacity] {:<22} match_delete 90%: after={after_match_delete}",
            shape.name
        );
        assert!(
            after_match_delete * 4 < populated,
            "{}: match_delete released no index capacity \
             ({populated} -> {after_match_delete})",
            shape.name
        );

        // Drain the survivors: the table must land on its exact fresh floor.
        let kept = tuple_of(&mut heap, &[atom("_"), make_small(KEPT_GROUP)]);
        let removed = t
            .match_delete(kept, &atoms, &mut out)
            .expect("match_delete on the surviving group");
        assert_eq!(removed, BULK_KEPT as usize, "{}: drained", shape.name);
        assert_eq!(t.size(), 0, "{}: drained", shape.name);
        assert_eq!(
            t.memory(),
            empty,
            "{}: a table drained by select_delete/match_delete returns to the fresh \
             empty floor",
            shape.name
        );
    }
}

/// A stable-size churn must neither grow the reported figure nor make it
/// shrink-and-regrow: every interior's shrink is measurement-derived hysteresis
/// (1/8 load for the bucket array, a 4× slack for the vector trims, dead-slots-
/// outweigh-live for the arena), not a per-removal recomputation.
///
/// The bounds are deterministic: no timing and no allocation addresses enter them.
#[test]
fn a_stable_size_churn_neither_grows_nor_thrashes_the_index() {
    for shape in shapes() {
        let mut atoms = trajectory_atoms();
        let x = interned_atom(&mut atoms, "x");
        let mut heap = Heap::new(4 * CHURN_KEYS as usize);
        let mut t = (shape.build)();
        let empty = t.memory();

        // The objects are built once: terms are immutable, so every round can
        // re-insert the very same ones (the table deep-copies on insert).
        let objects: Vec<Eterm> = (0..CHURN_KEYS)
            .map(|key| capacity_object(&mut heap, key, x))
            .collect();
        let keys: Vec<Eterm> = (0..CHURN_KEYS).map(make_small).collect();

        // Warm up with one insert of every object, so the store and the index reach
        // their steady state before the first measurement.
        for &object in &objects {
            t.insert(object, &atoms).expect("insert");
        }
        assert_eq!(t.size(), CHURN_KEYS as usize, "the table is loaded once");
        let base = t.memory();

        // One round: remove an object under every other key and immediately insert it
        // again, so the live object count never leaves its steady state (the churn a
        // shrink heuristic that reacted per removal would thrash on). `duplicate_bag`
        // is safe here for the same reason: a deleted key holds no object when the
        // replacement goes in.
        let round = |t: &mut Table| {
            for (index, key) in keys.iter().enumerate().step_by(2) {
                t.delete(*key, &atoms).expect("delete");
                t.insert(objects[index], &atoms).expect("re-insert");
            }
            assert_eq!(t.size(), CHURN_KEYS as usize, "the round ends at a stable size");
        };

        let mut min = base;
        let mut max = base;
        for _ in 0..CHURN_ROUNDS {
            round(&mut t);
            let now = t.memory();
            min = min.min(now);
            max = max.max(now);
        }
        println!(
            "[ets_capacity] {:<22} churn {CHURN_ROUNDS} rounds at {CHURN_KEYS} keys: \
             empty={empty} base={base} min={min} max={max}",
            shape.name
        );
        assert!(
            max <= base,
            "{}: the churn grew the reported words above the warm-up figure \
             (base {base}, max {max})",
            shape.name
        );
        assert!(
            min * 8 >= base * 7,
            "{}: the churn shrank the figure by more than an eighth below the warm-up \
             basis, which is the shrink/re-grow thrash the hysteresis exists to \
             prevent (base {base}, min {min})",
            shape.name
        );
    }
}

// ---------------------------------------------------------------------------
// The oracle cross-check
// ---------------------------------------------------------------------------

/// The oracle child's wall-clock bound, so a wedged `escript` fails the test rather
/// than hanging the suite.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(120);

/// How many times its own empty floor a runtime's one-object table may report and
/// still count as "a small multiple" rather than "the loaded figure".
///
/// This is the only quantitative bridge between the two sides: ERTS's interiors keep
/// some of their bucket/tree allocation around a single object (`set` measured 2 624
/// words against an empty floor of 306 on OTP 29.1.1, `ordered_set` 148 against 140),
/// and this crate keeps less, so the *absolute* figures differ by an order of
/// magnitude while the *ratio to each runtime's own floor* is the same kind of
/// number. Comparing the ratios — never the words — is what makes the cross-check
/// meaningful across OTP releases.
const ONE_LEFT_SMALL_MULTIPLE: i64 = 16;

/// The oracle program: the same trajectory, run under the live system `erl`.
///
/// It prints one `key -> value` line per figure, with the kind in the key, so the
/// crate's figures and the oracle's are never confused for each other.
const CAPACITY_ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
%%! -noshell
main(_) ->
    P = fun(K, V) -> io:format("~s -> ~w~n", [K, V]) end,
    lists:foreach(
      fun(Kind) ->
        F = fun(S) -> lists:flatten(io_lib:format("~p.~s", [Kind, S])) end,
        T = ets:new(capacity, [Kind, public]),
        P(F("empty"), ets:info(T, memory)),
        [ets:insert(T, {I, x}) || I <- lists:seq(0, 9999)],
        P(F("populated"), ets:info(T, memory)),
        P(F("size_populated"), ets:info(T, size)),
        [ets:delete(T, I) || I <- lists:seq(0, 9998)],
        P(F("one_left"), ets:info(T, memory)),
        P(F("size_one_left"), ets:info(T, size)),
        ets:delete_all_objects(T),
        P(F("after_delete_all"), ets:info(T, memory)),
        P(F("size_after_delete_all"), ets:info(T, size)),
        ets:delete(T)
      end, [set, ordered_set]),
    ok.
"#;

/// One side of the cross-check: the figures a runtime reported for one kind.
struct Side {
    empty: i64,
    populated: i64,
    one_left: i64,
    after_delete_all: i64,
}

/// Read `prefix`'s four figures out of a `key -> value` line map.
fn side(lines: &BTreeMap<String, String>, prefix: &str) -> Side {
    let word = |field: &str| -> i64 {
        let key = format!("{prefix}.{field}");
        lines
            .get(&key)
            .unwrap_or_else(|| panic!("no {key:?} line (lines: {lines:?})"))
            .parse()
            .unwrap_or_else(|_| panic!("{key:?} is not an integer"))
    };
    Side {
        empty: word("empty"),
        populated: word("populated"),
        one_left: word("one_left"),
        after_delete_all: word("after_delete_all"),
    }
}

/// The crate's own figures for one kind, measured with the *same* trajectory the
/// oracle program runs.
fn crate_side(kind: TableKind, ordered: OrderedInterior) -> Side {
    let mut atoms = trajectory_atoms();
    let x = interned_atom(&mut atoms, "x");
    let mut heap = Heap::new(8 * POPULATE as usize);
    let mut opts = TableOptions::new();
    opts.set_kind(kind);
    let mut t = if kind == TableKind::OrderedSet {
        Table::with_ordered_interior(TableId::next(), opts, ordered)
    } else {
        Table::new(TableId::next(), opts)
    }
    .expect("a fresh table is constructible");
    let empty = t.memory() as i64;
    for key in 0..POPULATE {
        let object = capacity_object(&mut heap, key, x);
        t.insert(object, &atoms).expect("insert");
    }
    let populated = t.memory() as i64;
    for key in 0..POPULATE - 1 {
        t.delete(make_small(key), &atoms).expect("delete");
    }
    let one_left = t.memory() as i64;
    t.delete_all_objects();
    let after_delete_all = t.memory() as i64;
    Side {
        empty,
        populated,
        one_left,
        after_delete_all,
    }
}

/// The oracle's `ets:info(T, memory)` trajectory agrees with the crate's in
/// **direction and rough magnitude**: both report a loaded table owning far more
/// words than a one-object table, both bring the one-object table back to a small
/// multiple of the empty floor, and both return to the empty floor when drained.
///
/// The absolute words are deliberately never compared (the interiors and the
/// allocation bookkeeping differ) — see the module docs.
#[test]
fn the_oracles_memory_trajectory_agrees_in_direction_and_magnitude() {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "the_oracles_memory_trajectory_agrees_in_direction_and_magnitude",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable escript on $PATH)",
        );
        return;
    };
    let output = oracle::run_escript(
        &escript,
        CAPACITY_ORACLE_PROGRAM,
        &[],
        "ets_capacity",
        ORACLE_TIMEOUT,
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let lines = parse_lines(&stdout);
    assert!(
        lines.len() >= 12,
        "the oracle printed too few figures:\n{stdout}"
    );

    // The oracle's own size counters, so a trajectory that silently inserted or
    // deleted the wrong number of objects cannot be mistaken for a memory result.
    for (kind, ordered) in [
        ("set", OrderedInterior::BPlusTree),
        ("ordered_set", OrderedInterior::BPlusTree),
    ] {
        assert_eq!(
            lines.get(&format!("{kind}.size_populated")).map(String::as_str),
            Some("10000"),
            "{kind}: the oracle stored the whole populate pass"
        );
        assert_eq!(
            lines.get(&format!("{kind}.size_one_left")).map(String::as_str),
            Some("1"),
            "{kind}: the oracle deleted all but one key"
        );

        let theirs = side(&lines, kind);
        let ours = crate_side(if kind == "set" {
            TableKind::Set
        } else {
            TableKind::OrderedSet
        }, ordered);
        println!(
            "[ets_capacity] {kind:<12} oracle: empty={} populated={} one_left={} \
             after_delete_all={} | crate: empty={} populated={} one_left={} \
             after_delete_all={}",
            theirs.empty,
            theirs.populated,
            theirs.one_left,
            theirs.after_delete_all,
            ours.empty,
            ours.populated,
            ours.one_left,
            ours.after_delete_all,
        );

        // 1. A loaded table owns far more than an empty one, on both sides.
        assert!(
            theirs.populated > theirs.empty,
            "{kind}: the oracle's loaded figure ({}) never rose above its empty one ({})",
            theirs.populated,
            theirs.empty
        );
        assert!(
            ours.populated > ours.empty,
            "{kind}: the crate's loaded figure ({}) never rose above its empty one ({})",
            ours.populated,
            ours.empty
        );
        // 2. The 10k -> 1-left drop is large on both sides (the same DIRECTION).
        assert!(
            theirs.one_left * 10 < theirs.populated,
            "{kind}: the oracle does not release index capacity on deletion \
             ({} -> {})",
            theirs.populated,
            theirs.one_left
        );
        assert!(
            ours.one_left * 10 < ours.populated,
            "{kind}: the crate does not release index capacity on deletion \
             ({} -> {})",
            ours.populated,
            ours.one_left
        );
        // 3. … and lands the one-object table in the same ROUGH MAGNITUDE: a small
        //    multiple of that runtime's own empty floor rather than the 10k figure.
        //    (The two sides' absolute words are never compared.)
        assert!(
            theirs.one_left <= theirs.empty * ONE_LEFT_SMALL_MULTIPLE,
            "{kind}: the oracle's one-object figure ({}) is not a small multiple of \
             its empty floor ({})",
            theirs.one_left,
            theirs.empty
        );
        assert!(
            ours.one_left <= ours.empty * ONE_LEFT_SMALL_MULTIPLE,
            "{kind}: the crate's one-object figure ({}) is not a small multiple of \
             its empty floor ({})",
            ours.one_left,
            ours.empty
        );
        // 4. Draining returns to the empty floor on both sides.
        assert_eq!(
            theirs.after_delete_all, theirs.empty,
            "{kind}: the oracle's table did not return to its empty floor on \
             delete_all_objects"
        );
        assert_eq!(
            ours.after_delete_all, ours.empty,
            "{kind}: the crate's table did not return to its empty floor on \
             delete_all_objects"
        );
    }
}
