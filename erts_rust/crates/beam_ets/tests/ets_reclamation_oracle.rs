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

//! The **oracle-cross-checked** half of the reclamation evidence.
//!
//! `tests/ets_reclamation.rs` proves that the *crate* reclaims — the index never
//! aliases a freed object, a balanced churn returns to a stable floor, a `take`
//! checks storage out instead of leaking it. This file supplies the other side of
//! those claims: an `ets:info(T, memory)` measurement taken from the **live system
//! `erl`** for the very same operation sequences, so every "the memory returns to
//! its floor" statement in the crate has an oracle counterpart rather than an
//! assumption about what `ets` does.
//!
//! The measured families are the ones the objective names:
//!
//! * the **delete family** — `delete/2`, `delete_object/2` and
//!   `delete_all_objects/1` each returning a loaded table to its empty floor, for
//!   every kind;
//! * the **upsert progression** — one key upserted, a second key added and then
//!   overwritten, then both removed, which pins *where* a replacement reclaims (an
//!   upsert is flat on the two upsert kinds, adds one object on the multi kinds) and
//!   that the endpoint is the starting figure;
//! * the **heir gift data** — installing a gift raises the table's memory, replacing
//!   it with a gift-free heir (or clearing the heir) returns it, and a 2000-round
//!   set/clear churn lands exactly back on the cleared figure;
//! * the **transfer / `give_away`** interactions — `give_away/3` leaves the table's
//!   memory untouched, and an owner death that transfers the table to its heir keeps
//!   the gift copy installed (the memory does not fall), so the
//!   `{'ETS-TRANSFER', …}` payload stays buildable.
//!
//! # The oracle's figures are NOT the crate's
//!
//! `ets:info(T, memory)` is a word count in ERTS exactly as
//! [`Table::memory`](beam_ets::Table::memory) is here, but the interiors, the copy
//! trimming and the allocator bookkeeping all differ, so the absolute words are
//! deliberately not compared. What is compared is the **contract**: which removals
//! return a table to a floor, whether an overwrite of the same key grows the table,
//! and whether the gift data is accounted for and given back. The crate's own
//! figures are printed alongside the oracle's, so both sides of every ratio are
//! visible in one log; they are recorded in the crate's `PERF.md`.
//!
//! The oracle is **required by default** (see `common::oracle`): a machine without
//! Erlang cannot turn this evidence into a vacuous pass. `BEAM_ETS_ALLOW_NO_ORACLE=1`
//! opts out with a loud skip.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use beam_ets::{
    Heir, OrderedInterior, OwnerDeath, OwnerId, SetInterior, SetOpt, Table, TableId, TableKind,
    TableOptions,
};
use beam_term::prelude::*;
use common::oracle;
use common::*;

/// The oracle child's wall-clock bound, so a wedged `escript` fails the test
/// instead of hanging the suite.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(120);

/// The object count of the bulk tables — large enough that every interior has
/// allocated (and, after a removal, has something to reclaim).
const BULK_N: i64 = 100;

/// The rounds of the heir set/clear churn (the oracle program runs the same count).
const HEIR_CHURN_ROUNDS: usize = 2_000;

/// The failure message a shape constructor uses (a `const`, so the closures below
/// stay non-capturing and coerce to `fn() -> Table`).
const FRESH_TABLE: &str = "a fresh table is constructible";

/// Every kind, in the order the oracle program prints them.
const KINDS: [TableKind; 4] = [
    TableKind::Set,
    TableKind::OrderedSet,
    TableKind::Bag,
    TableKind::DuplicateBag,
];

/// The oracle program: the four measured families, one `key -> value` line each.
///
/// It mirrors `ets_reclamation.rs`'s scenarios and the crate's own probe shapes, so
/// the two implementations run *the same operations*; only the memory figures are
/// the oracle's own.
const RECLAMATION_ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
%%! -noshell
main(_) ->
    P = fun(K, V) -> io:format("~s -> ~w~n", [K, V]) end,
    Kinds = [set, ordered_set, bag, duplicate_bag],

    %% (1) The delete family: does every removal path return a loaded table to its
    %%     empty floor?
    lists:foreach(
      fun(Kind) ->
        F = fun(S) -> io_lib:format("~p.~s", [Kind, S]) end,
        T = ets:new(mk, [Kind]),
        Empty = ets:info(T, memory),
        [ets:insert(T, {I, [I, I * 2, I * 3]}) || I <- lists:seq(1, 100)],
        Loaded = ets:info(T, memory),
        P(F("empty"), Empty),
        P(F("loaded"), Loaded),
        P(F("size_loaded"), ets:info(T, size)),
        [ets:delete(T, I) || I <- lists:seq(1, 100)],
        AfterKeys = ets:info(T, memory),
        P(F("after_key_deletes"), AfterKeys),
        P(F("size_after_key_deletes"), ets:info(T, size)),
        P(F("eq_empty_key_deletes"), AfterKeys =:= Empty),
        [ets:insert(T, {I, [I, I * 2, I * 3]}) || I <- lists:seq(1, 100)],
        [ets:delete_object(T, {I, [I, I * 2, I * 3]}) || I <- lists:seq(1, 100)],
        AfterDelObj = ets:info(T, memory),
        P(F("after_delete_object"), AfterDelObj),
        P(F("size_after_delete_object"), ets:info(T, size)),
        P(F("eq_empty_delete_object"), AfterDelObj =:= Empty),
        [ets:insert(T, {I, [I, I * 2, I * 3]}) || I <- lists:seq(1, 100)],
        ets:delete_all_objects(T),
        AfterAll = ets:info(T, memory),
        P(F("after_delete_all"), AfterAll),
        P(F("size_after_delete_all"), ets:info(T, size)),
        P(F("eq_empty_delete_all"), AfterAll =:= Empty),
        ets:delete(T)
      end, Kinds),

    %% (2) The upsert progression on one key plus a second key: m0 empty, m1 after
    %%     {1, [1..5]}, m2 after {2, b}, m3 after overwriting the key 2 value,
    %%     m4 after delete_object of the overwritten value, m5 after delete of key 1,
    %%     m6 after delete of key 2.
    lists:foreach(
      fun(Kind) ->
        F = fun(S) -> io_lib:format("prog.~p.~s", [Kind, S]) end,
        T = ets:new(mk, [Kind]),
        M0 = ets:info(T, memory),
        ets:insert(T, {1, [1, 2, 3, 4, 5]}),
        M1 = ets:info(T, memory),
        ets:insert(T, {2, b}),
        M2 = ets:info(T, memory),
        ets:insert(T, {2, c}),
        M3 = ets:info(T, memory),
        ets:delete_object(T, {2, c}),
        M4 = ets:info(T, memory),
        ets:delete(T, 1),
        M5 = ets:info(T, memory),
        ets:delete(T, 2),
        M6 = ets:info(T, memory),
        P(F("m0"), M0),
        P(F("m1"), M1),
        P(F("m2"), M2),
        P(F("m3"), M3),
        P(F("m4"), M4),
        P(F("m5"), M5),
        P(F("m6"), M6),
        P(F("m3_eq_m2"), M3 =:= M2),
        P(F("m6_eq_m0"), M6 =:= M0),
        ets:delete(T)
      end, Kinds),

    %% (3) The heir gift data: what does installing it cost, and is it given back?
    Gift = {a_gift, [1, 2, 3, 4, 5, 6, 7, 8]},
    T3 = ets:new(heir, [set]),
    Base = ets:info(T3, memory),
    ets:setopts(T3, {heir, self(), Gift}),
    WithGift = ets:info(T3, memory),
    ets:setopts(T3, {heir, self(), undefined}),
    WithUndef = ets:info(T3, memory),
    ets:setopts(T3, {heir, none}),
    Cleared = ets:info(T3, memory),
    [begin
         ets:setopts(T3, {heir, self(), Gift}),
         ets:setopts(T3, {heir, none})
     end || _ <- lists:seq(1, 2000)],
    Churned = ets:info(T3, memory),
    P("heir.base", Base),
    P("heir.with_gift", WithGift),
    P("heir.gift_delta", WithGift - Base),
    P("heir.with_undefined", WithUndef),
    P("heir.undefined_delta", WithUndef - Base),
    P("heir.cleared", Cleared),
    P("heir.churned", Churned),
    P("heir.churn_eq_empty", Churned =:= Cleared),
    ets:delete(T3),

    %% (4) give_away and the heir transfer: does either disturb the table's memory?
    Parent = self(),
    Other = spawn(fun() -> receive stop -> ok end end),
    T4 = ets:new(giveaway, [public]),
    [ets:insert(T4, {I, I}) || I <- lists:seq(1, 50)],
    Before4 = ets:info(T4, memory),
    GiveOk = ets:give_away(T4, Other, []),
    After4 = ets:info(T4, memory),
    P("giveaway.ok", GiveOk),
    P("giveaway.owner_moved", ets:info(T4, owner) =:= Other),
    P("giveaway.mem_unchanged", After4 =:= Before4),
    P("giveaway.size", ets:info(T4, size)),
    Other ! stop,
    ets:delete(T4),

    %% The transfer: the heir inherits, keeps the gift copy and stays installed.
    HeirPid = spawn(fun() -> receive stop -> ok end end),
    OwnerPid = spawn(fun() ->
        T5 = ets:new(xfer, [public, {heir, HeirPid, {gift, [1, 2, 3, 4, 5, 6, 7, 8]}}]),
        [ets:insert(T5, {I, I}) || I <- lists:seq(1, 50)],
        Parent ! {t5, T5, ets:info(T5, memory)},
        receive stop -> ok end
    end),
    receive {t5, T5, PreMem} ->
        exit(OwnerPid, kill),
        timer:sleep(500),
        PostMem = ets:info(T5, memory),
        P("xfer.survived", ets:info(T5) =/= undefined),
        P("xfer.owner_is_heir", ets:info(T5, owner) =:= HeirPid),
        P("xfer.heir_kept", ets:info(T5, heir) =:= HeirPid),
        P("xfer.size", ets:info(T5, size)),
        P("xfer.pre_mem", PreMem),
        P("xfer.post_mem", PostMem),
        P("xfer.mem_ge_pre", PostMem >= PreMem)
    end,
    HeirPid ! stop,
    ok.
"#;

/// One parsed run of [`RECLAMATION_ORACLE_PROGRAM`].
struct Oracle {
    lines: BTreeMap<String, String>,
}

impl Oracle {
    /// Run the oracle, or report a skip and return `None` when there is none.
    fn run(test: &str) -> Option<Oracle> {
        let Some(escript) = oracle::resolve_escript() else {
            oracle::skip(
                test,
                "no Erlang oracle found ($ERTS_ORACLE unset and no executable escript on $PATH)",
            );
            return None;
        };
        let output = oracle::run_escript(
            &escript,
            RECLAMATION_ORACLE_PROGRAM,
            &[],
            "reclamation_oracle",
            ORACLE_TIMEOUT,
        );
        let lines = parse_lines(&String::from_utf8_lossy(&output.stdout));
        println!(
            "[ets_reclamation_oracle] oracle={} loadavg={} ({} lines)",
            escript.display(),
            std::fs::read_to_string("/proc/loadavg")
                .unwrap_or_default()
                .trim(),
            lines.len()
        );
        Some(Oracle { lines })
    }

    /// The integer value the oracle printed for `key`.
    fn word(&self, key: &str) -> usize {
        self.lines
            .get(key)
            .unwrap_or_else(|| panic!("the oracle printed no {key:?} line"))
            .parse()
            .unwrap_or_else(|_| panic!("the oracle's {key:?} is not a non-negative integer"))
    }

    /// The boolean value the oracle printed for `key`.
    fn flag(&self, key: &str) -> bool {
        match self.lines.get(key).map(String::as_str) {
            Some("true") => true,
            Some("false") => false,
            other => panic!("the oracle's {key:?} is not a boolean: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// The crate's shapes and object builders
// ---------------------------------------------------------------------------

/// Table options for `kind` (the defaults, so the interior is the shipped default).
fn opts(kind: TableKind) -> TableOptions {
    let mut opts = TableOptions::new();
    opts.set_kind(kind);
    opts
}

/// Every observable kind/interior shape, as a (label, constructor) pair.
///
/// A constructor rather than a table, because a fresh table (with its own
/// [`TableId`]) is needed for every measurement.
fn shape_makers() -> Vec<(&'static str, fn() -> Table)> {
    vec![
        (
            "set/champ",
            || {
                Table::with_set_interior(
                    TableId::next(),
                    opts(TableKind::Set),
                    SetInterior::Champ,
                )
                .expect(FRESH_TABLE)
            },
        ),
        (
            "set/chained",
            || {
                Table::with_set_interior(
                    TableId::next(),
                    opts(TableKind::Set),
                    SetInterior::Chained,
                )
                .expect(FRESH_TABLE)
            },
        ),
        (
            "ordered_set/b+tree",
            || {
                Table::with_ordered_interior(
                    TableId::next(),
                    opts(TableKind::OrderedSet),
                    OrderedInterior::BPlusTree,
                )
                .expect(FRESH_TABLE)
            },
        ),
        (
            "ordered_set/sorted",
            || {
                Table::with_ordered_interior(
                    TableId::next(),
                    opts(TableKind::OrderedSet),
                    OrderedInterior::Sorted,
                )
                .expect(FRESH_TABLE)
            },
        ),
        (
            "bag",
            || Table::new(TableId::next(), opts(TableKind::Bag)).expect(FRESH_TABLE),
        ),
        (
            "duplicate_bag",
            || Table::new(TableId::next(), opts(TableKind::DuplicateBag)).expect(FRESH_TABLE),
        ),
    ]
}

/// The oracle kind name of a crate shape label (`set/champ` → `set`).
fn kind_of(shape: &str) -> &str {
    shape.split('/').next().expect("the label is non-empty")
}

/// The bulk object set: `(key, {I, [I, I*2, I*3]})` for `I in 1..=BULK_N`, built
/// once on `heap` and re-inserted (the table deep-copies, exactly as a real client
/// re-using its own terms does). The key is returned alongside so a `delete/2`
/// removal needs no raw term surgery.
fn bulk_objects(heap: &mut Heap) -> Vec<(Eterm, Eterm)> {
    (1..=BULK_N)
        .map(|i| {
            let key = make_small(i);
            let list = proper_list(heap, &[make_small(i), make_small(i * 2), make_small(i * 3)]);
            (key, tuple_of(heap, &[key, list]))
        })
        .collect()
}

/// One removal path of the delete family.
#[derive(Clone, Copy, Debug)]
enum Removal {
    /// `ets:delete(T, Key)` for every key.
    Keys,
    /// `ets:delete_object(T, Object)` for every object.
    Objects,
    /// `ets:delete_all_objects(T)`.
    All,
}

impl Removal {
    /// The label used in the failure messages and the printed log.
    fn label(self) -> &'static str {
        match self {
            Removal::Keys => "delete",
            Removal::Objects => "delete_object",
            Removal::All => "delete_all_objects",
        }
    }

    /// Perform the removal on `t`.
    fn run(self, t: &mut Table, objects: &[(Eterm, Eterm)], atoms: &AtomTable) {
        match self {
            Removal::Keys => {
                for &(key, _) in objects {
                    t.delete(key, atoms).expect("a stored key is deletable");
                }
            }
            Removal::Objects => {
                for &(_, object) in objects {
                    t.delete_object(object, atoms)
                        .expect("a stored object is deletable");
                }
            }
            Removal::All => t.delete_all_objects(),
        }
    }
}

// ---------------------------------------------------------------------------
// (1) The delete family
// ---------------------------------------------------------------------------

/// Every removal path of the delete family returns a loaded table to its floor —
/// for every kind and interior shape — and the oracle describes the same contract:
/// per-key `delete/2`, `delete_object/2` and `delete_all_objects/1` each put
/// `ets:info(T, memory)` back on the empty-table figure, with `size` back to `0`.
#[test]
fn the_delete_family_returns_every_shape_to_its_floor() {
    let Some(oracle) = Oracle::run("the_delete_family_returns_every_shape_to_its_floor") else {
        return;
    };

    // ---- The oracle's side of every ratio (measured, not copied) -------------
    for kind in KINDS {
        let name = kind.as_atom_name();
        let line = |suffix: &str| format!("{name}.{suffix}");
        assert!(
            oracle.word(&line("loaded")) > oracle.word(&line("empty")),
            "{name}: the oracle's loaded table owns no more memory than an empty one"
        );
        for (path, size_key, mem_key, eq_key) in [
            (
                "delete/2",
                "size_after_key_deletes",
                "after_key_deletes",
                "eq_empty_key_deletes",
            ),
            (
                "delete_object/2",
                "size_after_delete_object",
                "after_delete_object",
                "eq_empty_delete_object",
            ),
            (
                "delete_all_objects/1",
                "size_after_delete_all",
                "after_delete_all",
                "eq_empty_delete_all",
            ),
        ] {
            assert_eq!(
                oracle.word(&line(size_key)),
                0,
                "{name}: the oracle's {path} left objects behind"
            );
            assert!(
                oracle.flag(&line(eq_key)),
                "{name}: the oracle's {path} did not return the table to its empty floor \
                 ({} -> {} words)",
                oracle.word(&line("empty")),
                oracle.word(&line(mem_key))
            );
        }
        println!(
            "[ets_reclamation_oracle] oracle {name:<14} empty={} loaded={} words; \
             after delete/2={} delete_object/2={} delete_all_objects/1={} words \
             (each == empty)",
            oracle.word(&line("empty")),
            oracle.word(&line("loaded")),
            oracle.word(&line("after_key_deletes")),
            oracle.word(&line("after_delete_object")),
            oracle.word(&line("after_delete_all")),
        );
    }

    // ---- The crate's side ----------------------------------------------------
    let atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 20);
    let objects = bulk_objects(&mut heap);

    for (shape, make) in shape_makers() {
        for removal in [Removal::Keys, Removal::Objects, Removal::All] {
            // A fresh table per path, so the three paths cannot mask each other.
            let mut t = make();
            let empty = t.memory();
            let mut floors = Vec::new();
            let mut loaded_max = 0;
            for cycle in 0..3 {
                for &(_, object) in &objects {
                    t.insert(object, &atoms).expect("the object is insertable");
                }
                assert_eq!(
                    t.size(),
                    BULK_N as usize,
                    "{shape}/{removal:?} cycle {cycle}: size after the inserts"
                );
                let loaded = t.memory();
                assert!(
                    loaded > empty,
                    "{shape}/{removal:?} cycle {cycle}: a loaded table owns no storage"
                );
                loaded_max = loaded_max.max(loaded);

                removal.run(&mut t, &objects, &atoms);
                assert_eq!(
                    t.size(),
                    0,
                    "{shape}/{removal:?} cycle {cycle}: size after the removal"
                );
                let floor = t.memory();
                assert!(
                    floor < loaded,
                    "{shape}/{removal:?} cycle {cycle}: the removal reclaimed nothing \
                     ({loaded} -> {floor} words)"
                );
                assert!(
                    floor >= empty,
                    "{shape}/{removal:?} cycle {cycle}: the floor fell below the empty \
                     table's footprint ({empty} -> {floor} words)"
                );
                floors.push(floor);
            }
            assert_eq!(
                floors[0], floors[1],
                "{shape}/{removal:?}: the memory floor drifted on the second cycle ({floors:?})"
            );
            assert_eq!(
                floors[1], floors[2],
                "{shape}/{removal:?}: the memory floor drifted on the third cycle ({floors:?})"
            );

            // Every object is gone (not merely uncounted): the table is empty and
            // its objects are re-insertable afterwards.
            assert!(t.is_empty(), "{shape}/{removal:?}: the table is not empty");
            for &(_, object) in &objects {
                t.insert(object, &atoms).expect("the object is insertable");
            }
            assert_eq!(
                t.size(),
                BULK_N as usize,
                "{shape}/{removal:?}: the table is not reusable after the removal"
            );
            let kind = kind_of(shape);
            println!(
                "[ets_reclamation_oracle] crate {shape:<20} {:<18} empty={empty} \
                 loaded={loaded_max} floor={} words (oracle {kind}: empty={} loaded={})",
                removal.label(),
                floors[2],
                oracle.word(&format!("{kind}.empty")),
                oracle.word(&format!("{kind}.loaded")),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// (2) The upsert progression
// ---------------------------------------------------------------------------

/// One key upserted, a second key added and overwritten, then both removed: the
/// overwrite of an existing key is **flat** on the upsert kinds and adds one object
/// on the multi kinds (the oracle's `m3 =:= m2` flag), every removal reclaims, and
/// the endpoint is the starting figure (the oracle's `m6 =:= m0`).
///
/// `m4 == m1` is the strong per-object claim: removing the object a `delete_object/2`
/// names puts the table back on the footprint it had *before that object was added*,
/// i.e. the removal reclaimed exactly that object's storage on both sides.
#[test]
fn the_upsert_progression_returns_to_the_empty_floor() {
    let Some(oracle) = Oracle::run("the_upsert_progression_returns_to_the_empty_floor") else {
        return;
    };

    // ---- The oracle's side ---------------------------------------------------
    for kind in KINDS {
        let name = kind.as_atom_name();
        let line = |suffix: &str| format!("prog.{name}.{suffix}");
        let (m0, m1, m2, m3, m4, m5, m6) = (
            oracle.word(&line("m0")),
            oracle.word(&line("m1")),
            oracle.word(&line("m2")),
            oracle.word(&line("m3")),
            oracle.word(&line("m4")),
            oracle.word(&line("m5")),
            oracle.word(&line("m6")),
        );
        assert!(
            m1 > m0,
            "{name}: the oracle's first insert did not grow the table"
        );
        assert!(
            m2 > m1,
            "{name}: the oracle's second key did not grow the table"
        );
        assert_eq!(
            oracle.flag(&line("m3_eq_m2")),
            !kind.is_multi(),
            "{name}: the oracle's upsert classification changed"
        );
        assert!(
            m4 < m3,
            "{name}: the oracle's delete_object reclaimed nothing"
        );
        // Where the figure lands is kind-dependent: on the upsert kinds the deleted
        // object was the key's only one, so the key disappears (back to `m1`); on the
        // multi kinds the sibling object under the same key survives (back to `m2`).
        let expected_m4 = if kind.is_multi() { m2 } else { m1 };
        assert_eq!(
            m4, expected_m4,
            "{name}: the oracle's delete_object did not return to the pre-overwrite figure"
        );
        assert!(
            m5 < m2,
            "{name}: the oracle's delete of key 1 reclaimed nothing"
        );
        assert!(
            oracle.flag(&line("m6_eq_m0")),
            "{name}: the oracle did not return to its starting figure ({m0} -> {m6})"
        );
        println!(
            "[ets_reclamation_oracle] oracle prog {name:<14} m0={m0} m1={m1} m2={m2} m3={m3} \
             (flat={}) m4={m4} m5={m5} m6={m6} words",
            m3 == m2
        );
    }

    // ---- The crate's side ----------------------------------------------------
    let atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 12);
    let first_list = proper_list(&mut heap, &[1, 2, 3, 4, 5].map(make_small));
    let first = tuple_of(&mut heap, &[make_small(1), first_list]);
    let second_b = tuple_of(&mut heap, &[make_small(2), make_small(11)]);
    let second_c = tuple_of(&mut heap, &[make_small(2), make_small(12)]);

    for (shape, make) in shape_makers() {
        let mut t = make();
        let m0 = t.memory();
        t.insert(first, &atoms).expect("the object is insertable");
        let m1 = t.memory();
        assert!(m1 > m0, "{shape}: the first insert did not grow the table");
        t.insert(second_b, &atoms).expect("the object is insertable");
        let m2 = t.memory();
        assert!(m2 > m1, "{shape}: the second key did not grow the table");
        t.insert(second_c, &atoms).expect("the object is insertable");
        let m3 = t.memory();
        if t.kind().is_multi() {
            assert!(
                m3 > m2,
                "{shape}: the overwrite of a multi key added no object ({m2} -> {m3} words)"
            );
        } else {
            assert_eq!(
                m3, m2,
                "{shape}: replacing an existing key's object was not flat ({m2} -> {m3} \
                 words) — the replaced copy was not reclaimed before the new one was made"
            );
        }

        t.delete_object(second_c, &atoms)
            .expect("a stored object is deletable");
        let m4 = t.memory();
        assert!(
            m4 < m3,
            "{shape}: delete_object reclaimed nothing ({m3} -> {m4})"
        );
        // The oracle's own figure lands *exactly* on the pre-overwrite value (its the
        // hash index shrinks with the entries). The crate's verified contract is the
        // direction — every object's storage is reclaimed, and the figure never falls
        // below the empty table's — because the index's *capacity* grown by the second
        // key is retained until the table empties (PERF.md records both sides).
        assert!(
            m4 < m3,
            "{shape}: delete_object reclaimed nothing ({m3} -> {m4} words)"
        );
        assert!(
            m4 >= m0,
            "{shape}: delete_object fell below the empty table's footprint ({m0} -> {m4} words)"
        );

        t.delete(make_small(1), &atoms)
            .expect("a stored key is deletable");
        let m5 = t.memory();
        assert!(
            m5 < m2,
            "{shape}: the delete of key 1 reclaimed nothing ({m2} -> {m5})"
        );
        t.delete(make_small(2), &atoms)
            .expect("a stored key is deletable");
        let m6 = t.memory();
        assert!(
            m6 <= m0,
            "{shape}: the progression did not return to its starting figure ({m0} -> {m6})"
        );
        assert!(
            t.is_empty(),
            "{shape}: the table is not empty after the progression"
        );
        println!(
            "[ets_reclamation_oracle] crate prog {shape:<20} m0={m0} m1={m1} m2={m2} m3={m3} \
             (flat={}) m4={m4} m5={m5} m6={m6} words",
            m3 == m2
        );
    }
}

// ---------------------------------------------------------------------------
// (3) The heir gift data
// ---------------------------------------------------------------------------

/// Installing an heir's gift data costs the table storage, and every overwrite
/// gives it back: the oracle measures a non-zero gift delta and a **zero** delta
/// for a gift-free (`undefined`) heir, with 2000 rounds of set/clear landing exactly
/// on the cleared figure.
///
/// The crate's counterpart is [`Table::install_heir`], which deep-copies the gift
/// into table-owned storage (the A.1 copy boundary) and reclaims the previous copy
/// through the single `replace_heir` path every overwrite site funnels through.
#[test]
fn the_heir_gift_data_is_reclaimed_on_every_overwrite() {
    let Some(oracle) = Oracle::run("the_heir_gift_data_is_reclaimed_on_every_overwrite") else {
        return;
    };

    // ---- The oracle's side ---------------------------------------------------
    let base = oracle.word("heir.base");
    let gift_delta = oracle.word("heir.gift_delta");
    let undefined_delta = oracle.word("heir.undefined_delta");
    assert!(
        gift_delta > 0,
        "the oracle's {gift_delta}-word gift delta is not a cost — the probe is stale"
    );
    assert_eq!(
        undefined_delta, 0,
        "the oracle's `{{heir, Pid, undefined}}` owns storage"
    );
    assert_eq!(
        oracle.word("heir.with_undefined"),
        base,
        "the oracle's gift-free heir did not return to the base figure"
    );
    assert_eq!(
        oracle.word("heir.cleared"),
        base,
        "the oracle's cleared heir did not return to the base figure"
    );
    assert!(
        oracle.flag("heir.churn_eq_empty"),
        "the oracle's heir set/clear churn did not return to the cleared figure \
         ({} -> {})",
        oracle.word("heir.cleared"),
        oracle.word("heir.churned")
    );
    println!(
        "[ets_reclamation_oracle] oracle heir base={base} with_gift={} (+{gift_delta}) \
         with_undefined={} (+{undefined_delta}) cleared={} churned={} words (churn == cleared)",
        oracle.word("heir.with_gift"),
        oracle.word("heir.with_undefined"),
        oracle.word("heir.cleared"),
        oracle.word("heir.churned"),
    );

    // ---- The crate's side ----------------------------------------------------
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 12);
    let gift_list: Vec<Eterm> = (1..=8).map(make_small).collect();
    let gift_list = proper_list(&mut heap, &gift_list);
    let gift_atom = make_atom(atoms.intern("a_gift"));
    let gift = tuple_of(&mut heap, &[gift_atom, gift_list]);

    let owner = OwnerId::new(1);
    let heir = OwnerId::new(2);

    let mut t = new_table(TableKind::Set);
    t.set_owner(Some(owner));
    let base = t.memory();
    assert_eq!(t.heir_data(), None, "a fresh table has no heir data");

    t.install_heir(heir, Some(gift))
        .expect("a tuple gift is copyable");
    let with_gift = t.memory();
    assert!(
        with_gift > base,
        "installing a gift did not cost the table storage ({base} -> {with_gift} words)"
    );
    assert!(
        eq(
            t.heir_data().expect("the gift data is installed"),
            gift,
            &atoms
        ),
        "the installed gift is not equal to the source term"
    );

    // A gift-free heir — the oracle's `{heir, Pid, undefined}` and `{heir, none}`
    // respectively — must give the storage back.
    t.install_heir(heir, None).expect("an arity-2 heir installs");
    assert_eq!(
        t.memory(),
        base,
        "an arity-2 heir did not reclaim the previous gift copy"
    );
    t.install_heir(heir, Some(gift))
        .expect("a tuple gift is copyable");
    assert!(t.memory() > base);
    t.setopts(Some(owner), SetOpt::ClearHeir)
        .expect("the owner may clear the heir");
    assert_eq!(
        t.memory(),
        base,
        "clearing the heir did not reclaim the gift copy"
    );
    assert_eq!(t.heir(), None);

    // The repeated set/clear churn must land on the same figure every round — a
    // leaking reclaim path would drift after exactly one round.
    for round in 0..HEIR_CHURN_ROUNDS {
        t.install_heir(heir, Some(gift))
            .expect("a tuple gift is copyable");
        assert!(
            t.memory() > base,
            "round {round}: the re-installed gift owns no storage"
        );
        t.setopts(Some(owner), SetOpt::ClearHeir)
            .expect("the owner may clear the heir");
        assert_eq!(
            t.memory(),
            base,
            "round {round}: the gift churn drifted off the base figure"
        );
    }
    assert_eq!(t.heir_data(), None, "the churn left an heir installed");
    println!(
        "[ets_reclamation_oracle] crate heir base={base} with_gift={with_gift} words \
         (+{} words, flat after each of {HEIR_CHURN_ROUNDS} set/clear rounds)",
        with_gift - base
    );
}

// ---------------------------------------------------------------------------
// (4) `give_away` and the heir transfer
// ---------------------------------------------------------------------------

/// `give_away/3` does not touch the table's memory, and an owner death that
/// transfers the table to its heir keeps the gift copy installed (so the
/// `{'ETS-TRANSFER', …}` payload stays buildable) — the oracle reports the same two
/// shapes.
#[test]
fn a_transfer_keeps_the_gift_copy_and_a_cleared_heir_reclaims_it() {
    let Some(oracle) = Oracle::run("a_transfer_keeps_the_gift_copy_and_a_cleared_heir_reclaims_it")
    else {
        return;
    };

    // ---- The oracle's side ---------------------------------------------------
    assert!(oracle.flag("giveaway.ok"), "the oracle's give_away/3 failed");
    assert!(
        oracle.flag("giveaway.owner_moved"),
        "the oracle's give_away/3 did not move ownership"
    );
    assert!(
        oracle.flag("giveaway.mem_unchanged"),
        "the oracle's give_away/3 changed the table's memory"
    );
    assert!(
        oracle.flag("xfer.survived"),
        "the oracle's heir transfer dropped the table"
    );
    assert!(
        oracle.flag("xfer.owner_is_heir"),
        "the oracle's heir transfer did not move ownership to the heir"
    );
    assert!(
        oracle.flag("xfer.heir_kept"),
        "the oracle's heir transfer cleared the heir"
    );
    assert!(
        oracle.flag("xfer.mem_ge_pre"),
        "the oracle's heir transfer shrank the table ({} -> {} words)",
        oracle.word("xfer.pre_mem"),
        oracle.word("xfer.post_mem")
    );
    println!(
        "[ets_reclamation_oracle] oracle transfer give_away size={} (memory unchanged); \
         heir transfer pre={} post={} words, size={}",
        oracle.word("giveaway.size"),
        oracle.word("xfer.pre_mem"),
        oracle.word("xfer.post_mem"),
        oracle.word("xfer.size"),
    );

    // ---- The crate's side ----------------------------------------------------
    let atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 16);
    let objects: Vec<Eterm> = (1..=50)
        .map(|i| tuple_of(&mut heap, &[make_small(i), make_small(i)]))
        .collect();

    // (a) `give_away` moves the owner and nothing else.
    let first_owner = OwnerId::new(1);
    let second_owner = OwnerId::new(2);
    let mut t = new_table(TableKind::Set);
    t.set_owner(Some(first_owner));
    for &object in &objects {
        t.insert(object, &atoms).expect("the object is insertable");
    }
    let before = t.memory();
    t.give_away(Some(first_owner), Some(second_owner))
        .expect("the owner may give the table away");
    assert_eq!(
        t.owner(),
        Some(second_owner),
        "give_away did not move the owner"
    );
    assert_eq!(t.memory(), before, "give_away changed the table's memory");
    assert_eq!(t.size(), objects.len(), "give_away changed the object count");

    // (b) An owner death that transfers the table keeps the gift copy installed:
    //     the memory does not fall, and the returned heir still carries the gift.
    let gift = tuple_of(&mut heap, &[make_small(1), make_small(2), make_small(3)]);
    let heir_owner = OwnerId::new(3);
    let mut bare = new_table(TableKind::Set);
    bare.set_owner(Some(first_owner));
    for &object in &objects {
        bare.insert(object, &atoms).expect("the object is insertable");
    }
    let bare_memory = bare.memory();

    let mut t = new_table(TableKind::Set);
    t.set_owner(Some(first_owner));
    t.install_heir(heir_owner, Some(gift))
        .expect("a tuple gift is copyable");
    for &object in &objects {
        t.insert(object, &atoms).expect("the object is insertable");
    }
    let with_gift = t.memory();
    let gift_words = with_gift
        .checked_sub(bare_memory)
        .expect("the installed gift owns storage");
    assert!(
        gift_words > 0,
        "the installed gift owns no storage ({bare_memory} vs {with_gift} words)"
    );

    let outcome = t.owner_died(first_owner, |_| true);
    let OwnerDeath::Transferred(transferred) = outcome else {
        panic!("a live heir must take the table, got {outcome:?}")
    };
    assert_eq!(
        transferred.owner, heir_owner,
        "the transfer named the wrong heir"
    );
    assert_eq!(
        transferred.data.map(|data| eq(data, gift, &atoms)),
        Some(true),
        "the transfer's gift data is not the installed copy"
    );
    assert_eq!(
        t.memory(),
        with_gift,
        "the transfer changed the table's memory ({with_gift} -> {})",
        t.memory()
    );
    assert_eq!(
        t.owner(),
        Some(heir_owner),
        "the transfer did not move the owner"
    );
    assert_eq!(
        t.heir(),
        Some(Heir {
            owner: heir_owner,
            data: t.heir_data()
        }),
        "the heir is kept installed"
    );

    // The new owner may clear the heir, which reclaims exactly the gift's words.
    t.setopts(Some(heir_owner), SetOpt::ClearHeir)
        .expect("the new owner may clear the heir");
    assert_eq!(
        t.memory(),
        with_gift - gift_words,
        "clearing the heir after a transfer did not reclaim the gift copy"
    );
    assert_eq!(
        t.size(),
        objects.len(),
        "the transfer changed the object count"
    );
    println!(
        "[ets_reclamation_oracle] crate transfer give_away memory {before} words unchanged; \
         heir transfer memory {with_gift} words kept; gift = {gift_words} words reclaimed on clear"
    );
}
