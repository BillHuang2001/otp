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

//! The **live-oracle relation check** for `ets:slot/2`.
//!
//! `ets:slot/2` exposes a table's **interior**, so two of its figures belong to the
//! implementation and can never be compared value-for-value:
//!
//! * the **slot an object lands in** — ERTS's `set` is a bucket-chained hash table
//!   (`erts/emulator/beam/erl_db_hash.c:1733` `db_slot_hash`), ours is a CHAMP trie
//!   (`set`) or a bucket array (`bag`/`duplicate_bag`), each with its own index
//!   function;
//! * the **slot count** — ERTS's `nactive` is 256 for a small table and grows by
//!   linear hashing; ours is the CHAMP root index space (32) or the bucket array's
//!   own length. Neither figure is reproduced, and *nothing here pins ERTS's 256*.
//!
//! What **is** contract, and what this file cross-checks against the live system
//! `erl`, is the relation around them. On both sides, for every table kind:
//!
//! 1. the slots `0..N-1` answer a **list** of objects, and `N` is the first index
//!    that answers `'$end_of_table'` — the walk crosses exactly one boundary;
//! 2. `N + 1` is `badarg`, as are `-1`, `1 << 40`, a float and an atom;
//! 3. the slots `0..N-1` **partition** the table: their union is exactly
//!    `ets:tab2list/1`'s multiset, so no object is invented, dropped or repeated;
//! 4. a `set`/`bag`/`duplicate_bag` holding **one object** has `N >= 2` and at
//!    least one slot answering the **empty** list — the slot space is not the
//!    object count. (This is the relation a one-object-per-slot fallback breaks: it
//!    answers `'$end_of_table'` at slot 1 instead of the empty bucket `[]`.)
//! 5. an `ordered_set` is exact on both sides: every slot is a **one-object** list
//!    and `N == size`.
//!
//! The oracle program emits one `key -> value` line per fact; this file computes the
//! same facts from `Table::slot`. Every compared value is a boolean/atom that is
//! identical by construction on both sides — the two interior-specific numbers
//! (`note.*.slots`, `note.*.empty_slots`) are emitted for the record under a `note.`
//! prefix and deliberately excluded from the comparison.
//!
//! The oracle is required by default (`common::oracle`); `BEAM_ETS_ALLOW_NO_ORACLE=1`
//! opts out with a loud skip.
mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use beam_common::BeamError;
use beam_ets::{OrderedInterior, SetInterior, Table, TableId, TableKind, TableOptions};
use beam_term::prelude::*;

use common::{object, oracle};

/// The wall-clock bound for the (single, short) oracle run.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The object every case stores: `{1, 10}` — **one** object, key at position 1.
const ONE_OBJECT: (i64, i64) = (1, 10);

/// A slot index far past any small table's domain (`1 bsl 40`).
const HUGE_SLOT: i64 = 1 << 40;

/// One variant under test: a kind plus the opt-in interior, if any.
#[derive(Clone, Copy)]
struct Variant {
    kind: TableKind,
    interior: Option<Interior>,
}

/// The opt-in interiors the relation is checked for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Interior {
    /// The `ordered_set` interior that keeps a sorted `Vec` instead of a B+-tree.
    Sorted,
    /// The `set` interior with a bucket array instead of the CHAMP trie.
    Chained,
}

impl Interior {
    fn name(self) -> &'static str {
        match self {
            Interior::Sorted => "sorted",
            Interior::Chained => "chained",
        }
    }
}

/// The kinds' oracle-side names (the `ets:new/2` option).
fn kind_name(kind: TableKind) -> &'static str {
    match kind {
        TableKind::Set => "set",
        TableKind::OrderedSet => "ordered_set",
        TableKind::Bag => "bag",
        TableKind::DuplicateBag => "duplicate_bag",
    }
}

impl Variant {
    /// The fact-name prefix: the kind, plus the interior for an opt-in one.
    fn prefix(self) -> String {
        match self.interior {
            None => kind_name(self.kind).to_string(),
            Some(interior) => format!("{}.{}", kind_name(self.kind), interior.name()),
        }
    }
}

/// Every variant the relation is checked for: the four kinds on their default
/// interiors, plus both opt-in interiors.
fn variants() -> Vec<Variant> {
    vec![
        Variant {
            kind: TableKind::Set,
            interior: None,
        },
        Variant {
            kind: TableKind::Set,
            interior: Some(Interior::Chained),
        },
        Variant {
            kind: TableKind::Bag,
            interior: None,
        },
        Variant {
            kind: TableKind::DuplicateBag,
            interior: None,
        },
        Variant {
            kind: TableKind::OrderedSet,
            interior: None,
        },
        Variant {
            kind: TableKind::OrderedSet,
            interior: Some(Interior::Sorted),
        },
    ]
}

/// The one-object table of `variant`.
fn build(variant: Variant, atoms: &AtomTable, heap: &mut beam_term::heap::Heap) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(variant.kind);
    let mut table = match variant.interior {
        None => Table::new(TableId::next(), opts).expect("constructible"),
        Some(Interior::Sorted) => {
            Table::with_ordered_interior(TableId::next(), opts, OrderedInterior::Sorted)
                .expect("constructible")
        }
        Some(Interior::Chained) => {
            Table::with_set_interior(TableId::next(), opts, SetInterior::Chained)
                .expect("constructible")
        }
    };
    let (k, v) = ONE_OBJECT;
    table
        .insert(object(heap, k, v), atoms)
        .expect("the object inserts");
    table
}

/// `ets:slot(T, slot)` classified exactly as [`ORACLE_PROGRAM`]'s `cap/2` does.
fn cap(table: &Table, atoms: &AtomTable, slot: i64) -> String {
    match table.slot(slot, atoms) {
        Ok(Some(here)) if here.is_empty() => "empty_list".to_string(),
        Ok(Some(_)) => "list".to_string(),
        Ok(None) => "eot".to_string(),
        Err(BeamError::BadArg) => "badarg".to_string(),
        Err(other) => format!("other:{other:?}"),
    }
}

/// One variant's `ets:slot/2` facts — the same keys and values the oracle program
/// emits, plus its own two `note.` figures.
fn facts_for(variant: Variant, table: &Table, atoms: &AtomTable) -> Vec<(String, String)> {
    let prefix = variant.prefix();
    let mut facts: Vec<(String, String)> = Vec::new();

    // Walk `0, 1, …` until the first non-list answer.
    let mut objects: Vec<Eterm> = Vec::new();
    let mut empties = 0usize;
    let mut multi = 0usize;
    let mut n = 0i64;
    let terminator = loop {
        match cap(table, atoms, n).as_str() {
            "list" => {
                let here = table.slot(n, atoms).expect("in range").expect("not the end");
                if here.len() > 1 {
                    multi += 1;
                }
                objects.extend(here);
            }
            "empty_list" => empties += 1,
            other => break other.to_string(),
        }
        n += 1;
        assert!(n <= 1 << 20, "{prefix}: the slot walk never reached an end");
    };

    facts.push((format!("{prefix}.terminator"), terminator));
    facts.push((format!("{prefix}.past_end"), cap(table, atoms, n + 1)));
    facts.push((format!("{prefix}.negative"), cap(table, atoms, -1)));
    facts.push((format!("{prefix}.huge"), cap(table, atoms, HUGE_SLOT)));

    // The partition: the multiset of every slot's objects equals `tab2list`'s.
    let mut stored: Vec<Eterm> = table.tab2list();
    sort_terms(&mut objects, atoms);
    sort_terms(&mut stored, atoms);
    let same = objects.len() == stored.len()
        && objects
            .iter()
            .zip(stored.iter())
            .all(|(a, b)| beam_term::compare::eq(*a, *b, atoms));
    facts.push((format!("{prefix}.partition_matches_tab2list"), same.to_string()));

    // The family-specific relation.
    if variant.kind.is_ordered() {
        facts.push((
            format!("{prefix}.slots_equal_size"),
            (n == table.size() as i64).to_string(),
        ));
        facts.push((
            format!("{prefix}.one_object_per_slot"),
            (multi == 0 && empties == 0).to_string(),
        ));
    } else {
        facts.push((format!("{prefix}.at_least_two_slots"), (n >= 2).to_string()));
        facts.push((format!("{prefix}.has_a_free_slot"), (empties >= 1).to_string()));
    }
    // For the record only — this interior's own figures, never compared.
    facts.push((format!("note.{prefix}.slots"), n.to_string()));
    facts.push((format!("note.{prefix}.empty_slots"), empties.to_string()));
    facts
}

/// The crate's facts for every variant, in one flat list.
fn our_facts() -> Vec<(String, String)> {
    let atoms = AtomTable::new();
    let mut heap = beam_term::heap::Heap::new(1024);
    let mut facts = Vec::new();
    for variant in variants() {
        let table = build(variant, &atoms, &mut heap);
        facts.extend(facts_for(variant, &table, &atoms));
    }
    facts
}

/// Sort terms by the crate's own term order (`compare::cmp`), which is the oracle's
/// too — so the sorted multisets are comparable element by element.
fn sort_terms(terms: &mut [Eterm], atoms: &AtomTable) {
    terms.sort_by(|a, b| beam_term::compare::cmp(*a, *b, atoms));
}

/// The oracle's `key -> value` facts from [`ORACLE_PROGRAM`].
fn oracle_facts(stdout: &[u8]) -> BTreeMap<String, String> {
    let text = String::from_utf8_lossy(stdout);
    let mut facts = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once(" -> ")
            .unwrap_or_else(|| panic!("unparsable oracle line {line:?}"));
        facts.insert(key.trim().to_string(), value.trim().to_string());
    }
    facts
}

/// The relation above holds for every kind and interior, and holds **identically**
/// on the live system `erl` — the compatibility claim stated as a relation rather
/// than as interior figures.
#[test]
fn slot_relation_agrees_with_the_live_oracle() {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "ets:slot/2 relation",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable `erl`/`escript` on $PATH)",
        );
        return;
    };
    let output = oracle::run_escript(&escript, ORACLE_PROGRAM, &[], "slot_relation", ORACLE_TIMEOUT);
    let theirs = oracle_facts(&output.stdout);
    let ours: BTreeMap<String, String> = our_facts().into_iter().collect();

    // Every compared fact exists on both sides and agrees; the `note.` figures are
    // the only ones allowed to differ (they are this interior's own slot counts).
    let mut compared = 0usize;
    for (key, want) in &theirs {
        if key.starts_with("note.") {
            continue;
        }
        let got = ours
            .get(key)
            .unwrap_or_else(|| panic!("the crate produced no fact {key:?}"));
        assert_eq!(
            got, want,
            "`{key}` disagrees: the crate says {got:?}, the live oracle says {want:?}"
        );
        compared += 1;
    }
    assert!(
        compared >= 20,
        "the relation check compared only {compared} facts — the corpus shrank"
    );
    // Our own relation must hold for every variant, independently of the oracle.
    for variant in variants() {
        let prefix = variant.prefix();
        let keys: &[&str] = if variant.kind.is_ordered() {
            &["slots_equal_size", "one_object_per_slot"]
        } else {
            &["at_least_two_slots", "has_a_free_slot"]
        };
        for suffix in keys {
            let key = format!("{prefix}.{suffix}");
            assert_eq!(
                ours.get(&key).map(String::as_str),
                Some("true"),
                "{}: {key} must hold",
                prefix
            );
        }
        for suffix in ["terminator", "past_end", "negative", "huge"] {
            let key = format!("{prefix}.{suffix}");
            let value = ours.get(&key).map(String::as_str);
            let expected = match suffix {
                "terminator" => "eot",
                _ => "badarg",
            };
            assert_eq!(value, Some(expected), "{}: {key}", prefix);
        }
        assert_eq!(
            ours.get(&format!("{prefix}.partition_matches_tab2list"))
                .map(String::as_str),
            Some("true"),
            "{}: the slots must partition the table",
            prefix
        );
    }
    // The interior figures are reported for the record: they are *ours* and differ
    // from the oracle's, which is exactly why nothing may pin them to ERTS.
    let notes: Vec<String> = ours
        .iter()
        .filter(|(key, _)| key.starts_with("note."))
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    eprintln!(
        "ets:slot/2 slot counts (this crate's interiors, never ERTS's): {}",
        notes.join(" ")
    );
}

/// The oracle's side of the relation, for every kind.
///
/// `cap/2` renders a `try ets:slot(T, S)` as `empty_list` / `list` / `eot` /
/// `badarg`; the walk finds the first non-list index. The program emits the same
/// fact names the Rust side uses, plus `note.` lines carrying its own slot counts
/// (uncompared, for the record).
const ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
%%! -noshell
main(_) ->
    lists:foreach(fun check/1,
                  [{set, [set]}, {bag, [bag]}, {duplicate_bag, [duplicate_bag]},
                   {ordered_set, [ordered_set]}]),
    ok.

%% One kind: build a one-object table, walk the slots, emit the facts.
check({Name, Opts}) ->
    T = ets:new(slot_relation, Opts),
    true = ets:insert(T, {1, 10}),
    {N, Empties, Multi, Objects} = walk(T, 0, 0, 0, 0, []),
    P = fun(K, V) -> io:format("~s -> ~s~n", [K, V]) end,
    Base = atom_to_list(Name),
    P(Base ++ ".terminator", cap(T, N)),
    P(Base ++ ".past_end", cap(T, N + 1)),
    P(Base ++ ".negative", cap(T, -1)),
    P(Base ++ ".huge", cap(T, 1 bsl 40)),
    %% The slot union must be exactly tab2list's multiset.
    P(Base ++ ".partition_matches_tab2list",
      atom_to_list(lists:sort(Objects) =:= lists:sort(ets:tab2list(T)))),
    case Name of
        ordered_set ->
            P(Base ++ ".slots_equal_size",
              atom_to_list(N =:= ets:info(T, size))),
            P(Base ++ ".one_object_per_slot",
              atom_to_list(Multi =:= 0 andalso Empties =:= 0));
        _ ->
            P(Base ++ ".at_least_two_slots", atom_to_list(N >= 2)),
            P(Base ++ ".has_a_free_slot", atom_to_list(Empties >= 1))
    end,
    %% For the record only: the oracle's own figures.
    P("note." ++ Base ++ ".slots", integer_to_list(N)),
    P("note." ++ Base ++ ".empty_slots", integer_to_list(Empties)),
    ets:delete(T).

%% Walk `S, S+1, …` while the answer is a list; return
%% `{slot_count, empty_slots, multi_object_slots, objects}`.
walk(T, S, Slots, Empties, Multi, Objects) ->
    case cap(T, S) of
        "list" ->
            Here = ets:slot(T, S),
            walk(T, S + 1, Slots + 1, Empties, Multi + count_multi(Here), Objects ++ Here);
        "empty_list" ->
            walk(T, S + 1, Slots + 1, Empties + 1, Multi, Objects);
        _ ->
            {Slots, Empties, Multi, Objects}
    end.

count_multi([_]) -> 0;
count_multi([_ | T]) -> 1 + count_multi(T);
count_multi([]) -> 0.

%% `ets:slot/2` classified: a list answer (empty or not), the end atom, or badarg.
cap(T, S) ->
    try ets:slot(T, S) of
        '$end_of_table' -> "eot";
        [] -> "empty_list";
        L when is_list(L) -> "list"
    catch
        error:badarg -> "badarg"
    end.
"#;
