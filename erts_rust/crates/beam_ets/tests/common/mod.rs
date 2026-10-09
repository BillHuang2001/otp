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

//! Shared helpers for the `beam_ets` integration tests.
//!
//! This module is compiled separately into every integration-test binary, so an
//! unused helper would otherwise trip the `dead_code` lint in that binary. The
//! `allow` below keeps the build warning-free.

#![allow(dead_code)]

pub mod oracle;

use std::collections::BTreeMap;

use beam_ets::{compile, MatchSpec, Table, TableId, TableKind, TableOptions};
use beam_term::prelude::*;

/// A `{Key, Value}` ETS object on `heap` (keypos 1).
pub fn object(heap: &mut Heap, key: i64, value: i64) -> Eterm {
    heap.tuple(&[make_small(key), make_small(value)])
        .expect("test heap has room")
}

/// An empty table of `kind`.
pub fn new_table(kind: TableKind) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(kind);
    Table::new(TableId::next(), opts).expect("a fresh table is constructible")
}

/// A heap-allocated flonum term (`[HEADER_FLONUM, bits]`). `beam_term` has no
/// heap-based float constructor, so the two words are leaked for the test's life.
pub fn flonum(value: f64) -> Eterm {
    let words: Box<[Eterm; 2]> =
        Box::new([HEADER_FLONUM, Eterm::from_raw(value.to_bits() as usize)]);
    let leaked: &'static [Eterm; 2] = Box::leak(words);
    make_boxed(leaked.as_ptr())
}

/// The term of the atom `name`, interned in `atoms`.
pub fn interned_atom(atoms: &mut AtomTable, name: &str) -> Eterm {
    make_atom(atoms.intern(name))
}

/// A proper list of `elements` on `heap`.
pub fn proper_list(heap: &mut Heap, elements: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for element in elements.iter().rev() {
        acc = heap.cons(*element, acc).expect("test heap has room");
    }
    acc
}

/// The value of the `field` atom within an `ets:info/1` entry list.
pub fn info_entry(entries: &[(Atom, Eterm)], atoms: &AtomTable, field: &str) -> Option<Eterm> {
    let want = atoms.lookup(field)?;
    entries.iter().find(|(a, _)| *a == want).map(|(_, v)| *v)
}

/// The bump heap the `ets:update_*` operations rebuild objects on (a
/// `beam_alloc::Heap`, distinct from the `beam_term::heap::Heap` test arena the
/// *source* terms are built in).
pub fn rebuild_heap() -> beam_alloc::Heap {
    rebuild_heap_words(1024)
}

/// A rebuild heap with room for `words` term words — use it when a test performs
/// many `ets:update_*` calls: the interiors' default rebuild-on-heap path allocates
/// a fresh object (3 words for a `{Key, Value}` tuple) per call and the heap never
/// reallocates, so the default 1024 words is not enough for thousands of updates.
pub fn rebuild_heap_words(words: usize) -> beam_alloc::Heap {
    beam_alloc::Heap::new(words).expect("a test heap is allocatable")
}

/// A tuple object on `heap` built from `elements`.
pub fn tuple_of(heap: &mut Heap, elements: &[Eterm]) -> Eterm {
    heap.tuple(elements).expect("test heap has room")
}

/// A tuple object `{a, b, ...}` of small integers on `heap`.
pub fn int_tuple(heap: &mut Heap, elements: &[i64]) -> Eterm {
    let terms: Vec<Eterm> = elements.iter().copied().map(make_small).collect();
    tuple_of(heap, &terms)
}

/// A bignum term (`write_big`), leaked for the test's life.
///
/// `beam_term::heap::Heap` has no bignum constructor, so the `[header, digit]`
/// block is leaked and `write_big` fills it in.
pub fn bignum_term() -> Eterm {
    let words: Box<[Eterm; 2]> = Box::new([Eterm::from_raw(0), Eterm::from_raw(0)]);
    let leaked: &'static mut [Eterm; 2] = Box::leak(words);
    // SAFETY: `leaked` reserves `BIG_NEED_SIZE(1)` == 2 words (header + one
    // `ErtsDigit`), exactly what `write_big` writes for a one-digit magnitude, and
    // the digit slice is non-empty as `write_big` requires.
    unsafe { write_big(leaked.as_mut_ptr(), false, &[1u64 << 60]) }
}

/// A bignum of `digits` (least significant first, non-empty) built on `heap`.
///
/// The counter tests use it to name the *exact* expected result of an
/// arbitrary-precision sum, independent of the small/bignum encoding: two bignums
/// with the same sign and digits are `=:=` (`beam_term::compare::cmp` compares
/// their limbs).
pub fn big(heap: &mut Heap, negative: bool, digits: &[u64]) -> Eterm {
    // SAFETY: `alloc_words` hands out exactly `big_size_words(n)` uninitialised
    // words, which `write_big` fills completely.
    let hp = unsafe { heap.alloc_words(big_size_words(digits.len())).expect("test heap has room") };
    // SAFETY: `hp` addresses `big_size_words(len)` words and `digits` is non-empty.
    unsafe { write_big(hp, negative, digits) }
}

// ---------------------------------------------------------------------------
// The match-spec / traversal-order helpers shared by `ets_select.rs`,
// `ets_order.rs` and `ets_nav.rs` (single definitions, never copy-pasted).
// ---------------------------------------------------------------------------

/// A fresh output heap for a select call.
pub fn out_heap() -> beam_alloc::Heap {
    beam_alloc::Heap::new(8192).expect("an output heap is allocatable")
}

/// `[{'$1', [], ['$1']}]` — a bare-variable head that binds the whole object and
/// returns it.
pub fn whole_object_spec(heap: &mut Heap, atoms: &mut AtomTable) -> MatchSpec {
    let head = interned_atom(atoms, "$1");
    let body = proper_list(heap, &[interned_atom(atoms, "$1")]);
    let clause = heap.tuple(&[head, NIL, body]).unwrap();
    let spec = proper_list(heap, &[clause]);
    compile(spec, atoms).expect("the quoted spec compiles")
}

/// `[{{'$1','_'}, [], ['$1']}]` — bind the key and return it (one small integer
/// per object).
pub fn key_spec(heap: &mut Heap, atoms: &mut AtomTable) -> MatchSpec {
    let head = heap
        .tuple(&[interned_atom(atoms, "$1"), interned_atom(atoms, "_")])
        .unwrap();
    let body = proper_list(heap, &[interned_atom(atoms, "$1")]);
    let clause = heap.tuple(&[head, NIL, body]).unwrap();
    let spec = proper_list(heap, &[clause]);
    compile(spec, atoms).expect("the quoted spec compiles")
}

/// The `{Key, Value}` objects `{1,10}..{5,50}` on `heap` — the shared fixture of
/// the traversal-order and delete batteries.
pub fn objects_1_to_5(heap: &mut Heap) -> Vec<Eterm> {
    (1..=5i64).map(|key| object(heap, key, key * 10)).collect()
}

/// The `(key, value)` of a `{K, V}` small-integer tuple.
pub fn pair_of(object: Eterm) -> (i64, i64) {
    // SAFETY: every term read here is a `{Key, Value}` two-word tuple of small
    // integers built by `common::object`.
    unsafe {
        (
            signed_value(tuple_element(object, 0)),
            signed_value(tuple_element(object, 1)),
        )
    }
}

/// Render a list of small integers the way `erl`'s `~p` does: `[1,2,3]`.
pub fn render_ints(values: &[i64]) -> String {
    let joined = values
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("[{joined}]")
}

/// Render `{Key, Value}` pairs the way `erl`'s `~p` does: `[{1,10},{2,20}]`.
pub fn render_pairs(pairs: &[(i64, i64)]) -> String {
    let joined = pairs
        .iter()
        .map(|(key, value)| format!("{{{key},{value}}}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{joined}]")
}

/// The keys of `table`, ascending (the order-insensitive rendering of a hash
/// kind).
pub fn sorted_keys(table: &Table) -> Vec<i64> {
    let mut keys: Vec<i64> = table.tab2list().iter().map(|o| pair_of(*o).0).collect();
    keys.sort();
    keys
}

/// Parse `name -> rendering` lines into pairs, tolerating leading/trailing noise.
pub fn parse_lines(stdout: &str) -> BTreeMap<String, String> {
    stdout
        .lines()
        .filter_map(|line| {
            let (name, rendered) = line.trim().split_once(" -> ")?;
            Some((name.trim().to_string(), rendered.trim().to_string()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The embedded oracle programs of the multi-kind (`bag`/`duplicate_bag`)
// battery (`ets_bag.rs`). They are large verbatim `escript` fixtures, so they
// live here to keep that integration-test binary under the ~1000-line
// convention; the Rust half of each battery stays next to its assertions.
// ---------------------------------------------------------------------------

/// The battery above, as an `escript`, printing one `key -> value` line per fact
/// (so the system `erl` re-derives every recorded figure from the live oracle).
pub const BAG_ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
%%! -noshell
main(_) ->
    P = fun(K, V) -> io:format("~s -> ~p~n", [K, V]) end,
    C = fun(E) -> try {ok, E()} catch error:badarg -> badarg; Class:Reason -> {Class, Reason} end end,
    R = fun({ok, V}) -> V; (X) -> X end,
    Mk = fun(Kind, Opts) -> ets:new(mt, [Kind, public | Opts]) end,
    lists:foreach(
      fun(Kind) ->
        F = fun(S) -> lists:flatten(io_lib:format("~p.~s", [Kind, S])) end,
        %% (1) duplicate objects
        T1 = Mk(Kind, []),
        ets:insert(T1, {1,10}), ets:insert(T1, {1,10}), ets:insert(T1, {1,20}),
        P(F("dup.size"), ets:info(T1, size)),
        P(F("dup.lookup"), lists:sort(ets:lookup(T1, 1))),
        ets:delete(T1),
        %% (2) delete_object on a non-anchor copy
        T2 = Mk(Kind, []),
        ets:insert(T2, {1,10}), ets:insert(T2, {1,20}),
        ets:delete_object(T2, {1,10}),
        P(F("delo.size"), ets:info(T2, size)),
        P(F("delo.lookup"), lists:sort(ets:lookup(T2, 1))),
        ets:delete(T2),
        %% (3) same key, different non-key content
        T3 = Mk(Kind, []),
        ets:insert(T3, {1,10}), ets:insert(T3, {1,20}),
        P(F("upsert.size"), ets:info(T3, size)),
        P(F("upsert.list"), lists:sort(ets:tab2list(T3))),
        ets:delete(T3),
        %% (4) size counts objects; delete by key
        T4 = Mk(Kind, []),
        ets:insert(T4, {1,10}), ets:insert(T4, {1,20}),
        ets:insert(T4, {2,30}), ets:insert(T4, {2,40}),
        P(F("four.size"), ets:info(T4, size)),
        ets:delete(T4, 2),
        P(F("del.size"), ets:info(T4, size)),
        P(F("del.list"), lists:sort(ets:tab2list(T4))),
        ets:delete(T4, 99),
        P(F("del.absent.size"), ets:info(T4, size)),
        ets:delete(T4),
        %% (5) insert_new refuses an existing key
        T5 = Mk(Kind, []),
        P(F("insnew.first"), ets:insert_new(T5, {1,10})),
        P(F("insnew.dup"), ets:insert_new(T5, {1,20})),
        P(F("insnew.size"), ets:info(T5, size)),
        P(F("insnew.newkey"), ets:insert_new(T5, {2,20})),
        P(F("insnew.size2"), ets:info(T5, size)),
        ets:delete(T5),
        %% (6) insert_list atomicity
        T6 = Mk(Kind, []),
        P(F("inslist.ok"), R(C(fun() -> ets:insert(T6, [{1,10},{2,20},{1,11}]) end))),
        P(F("inslist.ok.size"), ets:info(T6, size)),
        P(F("inslist.ok.list"), lists:sort(ets:tab2list(T6))),
        P(F("inslist.bad"), R(C(fun() -> ets:insert(T6, [{3,30}, not_a_tuple, {4,40}]) end))),
        P(F("inslist.bad.size"), ets:info(T6, size)),
        P(F("inslist.bad.list"), lists:sort(ets:tab2list(T6))),
        ets:delete(T6),
        %% (7) select_delete counts objects
        T7 = Mk(Kind, []),
        ets:insert(T7, {1,10}), ets:insert(T7, {1,20}), ets:insert(T7, {2,10}),
        P(F("seldel.count"), ets:select_delete(T7, [{{'_',10},[],[true]}])),
        P(F("seldel.size"), ets:info(T7, size)),
        P(F("seldel.list"), lists:sort(ets:tab2list(T7))),
        ets:delete(T7),
        %% (8) match_delete count (from the size delta; the BIF returns true)
        T8 = Mk(Kind, []),
        ets:insert(T8, {1,10}), ets:insert(T8, {1,20}), ets:insert(T8, {2,10}),
        B8 = ets:info(T8, size),
        P(F("matchdel.before"), B8),
        _ = ets:match_delete(T8, {'_',10}),
        P(F("matchdel.count"), B8 - ets:info(T8, size)),
        P(F("matchdel.after.size"), ets:info(T8, size)),
        P(F("matchdel.after.list"), lists:sort(ets:tab2list(T8))),
        ets:delete(T8),
        %% (9) keypos 2
        T9 = Mk(Kind, [{keypos,2}]),
        ets:insert(T9, {10,1}), ets:insert(T9, {20,1}),
        P(F("kp2.size"), ets:info(T9, size)),
        P(F("kp2.lookup"), lists:sort(ets:lookup(T9, 1))),
        ets:delete_object(T9, {10,1}),
        P(F("kp2.after_delo.list"), lists:sort(ets:tab2list(T9))),
        ets:delete(T9),
        %% (10) keypos 2 insert_new
        T10 = Mk(Kind, [{keypos,2}]),
        P(F("kp2.insnew.first"), ets:insert_new(T10, {10,1})),
        P(F("kp2.insnew.dup"), ets:insert_new(T10, {99,1})),
        P(F("kp2.insnew.size"), ets:info(T10, size)),
        ets:delete(T10),
        %% (11) duplicates + select_delete
        T11 = Mk(Kind, []),
        ets:insert(T11, {1,10}), ets:insert(T11, {1,10}), ets:insert(T11, {2,10}),
        P(F("dupseldel.count"), ets:select_delete(T11, [{{'_',10},[],[true]}])),
        P(F("dupseldel.size"), ets:info(T11, size)),
        P(F("dupseldel.list"), lists:sort(ets:tab2list(T11))),
        ets:delete(T11)
      end, [set, ordered_set, bag, duplicate_bag]),

    %% (12) the ordered_set replace-key-term rule
    O1 = Mk(ordered_set, []),
    ets:insert(O1, {1,a}), ets:insert(O1, {1.0,b}),
    P("ord.keyterm.size", ets:info(O1, size)),
    P("ord.keyterm.first", ets:first(O1)),
    P("ord.keyterm.last", ets:last(O1)),
    P("ord.keyterm.list", lists:sort(ets:tab2list(O1))),
    P("ord.keyterm.lookup1", lists:sort(ets:lookup(O1, 1))),
    P("ord.keyterm.lookupf", lists:sort(ets:lookup(O1, 1.0))),
    ets:delete(O1),
    O2 = Mk(ordered_set, []),
    ets:insert(O2, {1.0,b}), ets:insert(O2, {1,a}),
    P("ord.keyterm.rev.first", ets:first(O2)),
    P("ord.keyterm.rev.list", lists:sort(ets:tab2list(O2))),
    ets:delete(O2),

    %% (13) the bag no-op duplicate
    B = Mk(bag, []),
    ets:insert(B, {1,10}), ets:insert(B, {1,20}), ets:insert(B, {1,10}),
    P("bag.noop.size", ets:info(B, size)),
    P("bag.noop.list", lists:sort(ets:tab2list(B))),
    ets:delete(B),
    ok.
"#;

/// The oracle's trace (the identical script), printing `"<kind>.<step>" -> "{S,L}"`.
///
/// The value is printed with `~w` (not `~p`): a large table's list would otherwise
/// be line-wrapped and every wrapped line would be a spurious mismatch.
pub const BAG_TRACE_ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
%%! -noshell
main(_) ->
    P = fun(K, V) -> io:format("~s -> ~w~n", [K, V]) end,
    Next = fun(X) -> (X * 1103515245 + 12345) band 16#7FFFFFFF end,
    Seed = 1,
    lists:foreach(
      fun(Kind) ->
        T = ets:new(mt, [Kind, public]),
        {_, _} = lists:foldl(
          fun(Step, {X0, _}) ->
            X1 = Next(X0), Op = X1 rem 12,
            X2 = Next(X1), K = 1 + (X2 rem 4),
            X3 = Next(X2), V = 1 + (X3 rem 3),
            _ = case Op of
                  0 -> ets:insert(T, {K, V});
                  1 -> ets:insert(T, {K, V});
                  2 -> ets:insert(T, {K, V});
                  3 -> ets:insert_new(T, {K, V});
                  4 -> ets:delete_object(T, {K, V});
                  5 -> ets:delete_object(T, {K, V});
                  6 -> ets:delete(T, K);
                  7 -> ets:match_delete(T, {'_', V});
                  8 -> ets:insert(T, [{K, V}, {K + 10, V + 10}]);
                  9 -> ets:lookup(T, K);
                  10 -> ets:delete_all_objects(T);
                  11 -> ets:insert(T, {K, V})
                end,
            P(lists:flatten(io_lib:format("~p.~b", [Kind, Step])),
              {ets:info(T, size), lists:sort(ets:tab2list(T))}),
            {X3, 0}
          end, {Seed, 0}, lists:seq(1, 120)),
        ets:delete(T)
      end, [set, ordered_set, bag, duplicate_bag]),
    ok.
"#;
