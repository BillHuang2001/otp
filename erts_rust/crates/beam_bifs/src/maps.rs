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

//! `maps` module BIFs over flat maps: `new/0`, `from_list/1`, `to_list/1`,
//! `get/2,3`, `find/2`, `put/3`, `update/3`, `remove/2`, `keys/1`, `values/1`,
//! `size/1`, `is_key/2`, `merge/2`, `with/2`, `without/2`, `iterator/1`, `next/1`,
//! the `fun`-taking `map/2`, `fold/3`, `filter/2` and `foreach/2`, plus the
//! `erlang:map_*` entries `map_get/2`, `map_size/1` and `is_map_key/2`.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_map.c` (the
//! `maps_*` BIFs), `../../lib/stdlib/src/maps.erl` (the `new/0`/`to_list/1`/
//! `get/3`/`size/1`/`with/2`/`without/2`/`iterator/1`/`next/1` wrappers) and
//! `../../erts/emulator/beam/bif.tab` (the roster: `bif maps:find/2`,
//! `maps:get/2`, `maps:from_list/1`, `maps:is_key/2`, `maps:keys/1`,
//! `maps:merge/2`, `maps:put/3`, `maps:remove/2`, `maps:update/3`,
//! `maps:values/1`; `ubif erlang:map_size/1`, `erlang:map_get/2`,
//! `erlang:is_map_key/2`). Flat-map layout is owned by `beam_term::thing`
//! (`write_flatmap`/`flatmap_parts`) and reached through [`crate::access`].
//!
//! # The `fun`-taking BIFs
//!
//! `map/2`, `fold/3`, `filter/2` and `foreach/2` are real `erl_map.c` BIFs, so
//! they are registered here rather than left to `maps.erl`. They call their fun
//! through [`BifContext::call_fun`] (the [`crate::FunCaller`] seam); with no caller
//! installed (Milestone-1 has no in-crate evaluator) they raise `unsupported_bif`,
//! and a fun's own failures propagate verbatim.
//!
//! * `map(Fun, Map)` calls `Fun(Key, Value)` and stores the result — the keys are
//!   unchanged, the values are the fun's results, in the map's stored order.
//! * `fold(Fun, Init, Map)` is `Acc = Fun(Key, Value, Acc)`; `filter(Pred, Map)`
//!   keeps the pairs whose predicate returns `true`; `foreach(Fun, Map)` is called
//!   for its side effects and returns `ok`.
//! * A predicate result that is not a `true`/`false` atom is the oracle's
//!   `error:{case_clause, Value}` (see the structured-reason section below).
//! * The traversal order is the map's **stored** order — the oracle's canonical
//!   flat-map key order (see the section below) — so a `fold` that accumulates into
//!   a list sees the oracle's pair sequence for any map this crate built.
//!
//! The fun argument is **not** arity-checked here — that is the runtime's job when
//! it invokes it — so `maps:map(5, #{})` returns `#{}` where the oracle raises
//! `badarg` (documented Milestone-1 gap: a non-fun is only detected once a fun call
//! actually happens).
//!
//! # Key identity is exact (`=:=`)
//!
//! Map keys are compared with [`beam_term::compare::eq`] — the *exact* `=:=` order.
//! `#{1.0 => a}` and `#{1 => a}` are therefore distinct maps, and
//! `maps:get(1, #{1.0 => a})` misses (see [`find_key`]). This is verified against
//! the OTP 29 oracle: `maps:is_key(1, #{1.0 => a})` ⇒ `false`, while
//! `maps:get(1.0, #{1.0 => a})` ⇒ `a`.
//!
//! # Structured reasons carry their term
//!
//! A missing key or a non-map operand raises the *structured* reasons the oracle
//! raises — `error:{badkey, Key}` (`maps:get/2`, `erlang:map_get/2`, `maps:update/3`)
//! and `error:{badmap, Map}` (every BIF here whose map argument is not a map) — as an
//! [`ExitReason::Structured`](beam_common::ExitReason::Structured) raise whose live
//! reason term is that tuple ([`crate::error::structured_error`]).
//! `beam_common::ExitReason` is term-independent, so the tuple itself travels in the
//! opaque [`Raise::live_reason`](beam_common::Raise::live_reason) slot; the raise's
//! `reason` stays `Structured` and `BeamError::as_atom` answers `None`.
//!
//! All *other* failure shapes match the oracle's *atoms* exactly: e.g.
//! `maps:from_list/1` really does raise the atom `badarg` for a non-list or a
//! malformed pair, and that is reproduced exactly. At the pinning oracle
//! (`erl -noshell -eval 'try maps:get(a,1) catch C:R -> io:format("~p:~p~n",[C,R]) end, halt().'`)
//! ⇒ `error:{badmap,1}`, and
//! `... try maps:get(a,#{}) ...` ⇒ `error:{badkey,a}`.
//!
//! # Milestone-1 deviations (documented, never a silently wrong result)
//!
//! * **A map with more than 32 pairs is `system_limit`**
//!   ([`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit)).
//!   Every Milestone-1 map is a *flat* map, and `beam_term::thing::write_flatmap`
//!   only writes up to `MAP_SMALL_MAP_LIMIT` (32) pairs — a 33-pair map must be a
//!   HAMT hashmap, which no Milestone-1 code can build. Producing such a map
//!   (`from_list`, `put`, `merge`, …) therefore reports `system_limit` rather than
//!   a wrong map. Reading an *existing* hashmap term (an `is_hashmap` operand) is
//!   likewise `system_limit` ([`map_parts`]); no Milestone-1 producer creates one,
//!   so that branch is defensive.
//! * **`iterator/1` yields the map's key/value list.** Real ERTS returns an opaque
//!   iterator; Milestone-1 uses the documented stand-in of the map's `{Key, Value}`
//!   pair list in stored order ([`iterator_1`]). [`next_1`] then peels one pair and
//!   returns `{Key, Value, NextIter}` exactly as the oracle does, where `NextIter`
//!   is the *remaining* pair list or the atom `none` when the list is exhausted;
//!   an exhausted iterator (`none`, or the empty list) yields `none`. (The oracle
//!   raises `badarg` for a bare `[]`, which in this representation *is* the
//!   exhausted iterator for `#{}`.)
//!
//! # Order
//!
//! OTP documents map iteration order as unspecified, but the oracle is *not*
//! arbitrary: every flat map it holds — one from a literal, one from
//! `maps:from_list/1`, one rebuilt by `maps:put/3` — stores its keys in
//! `erts_cmp_flatmap_keys` order (`erts/emulator/beam/erl_utils.h:267-272`;
//! `erts_validate_and_sort_flatmap`): **atom keys by atom index, every other pair by
//! the exact term order**. That is the order `maps:keys/1`/`maps:to_list/1` report and
//! the order the `term_to_binary/1` image carries, so it is observable.
//!
//! [`build_map`] — the writer behind every producer here (`from_list`, `put`,
//! `update`, `remove`, `take`, `from_keys`, `merge`, `with`, `without`, `map`,
//! `filter`) — therefore sorts the pairs with [`compare::cmp_flatmap_keys`] before
//! handing them to [`crate::access::make_flatmap`], and every reader returns the
//! map's **stored** order unchanged. Oracle-pinned:
//! `maps:from_list([{3,a},{1,b},{2,c}])` ⇒ `#{1 => b, 2 => c, 3 => a}` (keys
//! `[1,2,3]`), and atom-keyed order follows the *index* rather than insertion or name
//! (`X = list_to_atom("zzzq"), Y = list_to_atom("aaaq")` ⇒
//! `maps:keys(maps:from_list([{X,1},{Y,2}]))` = `[zzzq,aaaq]`, while adding `a` gives
//! `[a,zzzq,aaaq]`). The runtime's own atom indices are a documented Milestone-1 gap
//! (`beam_rt`'s `CORE_ATOMS` is not the legacy `atom.names` order), so an
//! *atom-keyed* order can still differ from the oracle's while the ordering *rule*
//! above is identical.
//!
//! The `fun`-taking `map/2`/`fold/3`/`filter/2`/`foreach/2` traverse (and, where they
//! rebuild, store) that same order, so a `fold` that accumulates into a list sees the
//! oracle's pair sequence.

use beam_common::Result;
use beam_term::{atom_val, compare, AtomTable, Eterm};

use crate::access;
use crate::context::BifContext;
use crate::error::{badarg, structured_error, system_limit};
use crate::print::proper_list;
use crate::registry::BifRegistry;

/// `MAP_SMALL_MAP_LIMIT` — the largest flat-map pair count, mirroring
/// `beam_term::thing`'s (private) constant of the same name. ERTS switches to a
/// hashmap above it (`erl_map.h:MAP_SMALL_MAP_LIMIT`); Milestone-1 has no HAMT
/// builder, so a larger map is
/// [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit) (see the module docs).
const MAP_SMALL_MAP_LIMIT: usize = 32;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Splits a *map* argument into its `(keys, values)`, in stored order.
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Term}` — an
/// [`ExitReason::Structured`](beam_common::ExitReason::Structured) raise carrying the
/// `{badmap, Term}` tuple as its live reason term — when `term` is not a map
/// (oracle: `maps:get(a, 1)` ⇒ `error:{badmap,1}`);
/// [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit) when `term` is a hashmap (a >32-pair map, which the
/// Milestone-1 flat-map layer cannot represent).
fn map_parts(ctx: &mut BifContext<'_>, term: Eterm) -> Result<(Vec<Eterm>, Vec<Eterm>)> {
    if !access::is_map(term) {
        return Err(structured_error(ctx, "badmap", &[term])?);
    }
    if !access::is_flatmap(term) {
        // Only flat maps and hashmaps are `is_map`; this is the hashmap case.
        return Err(system_limit());
    }
    access::flatmap_pairs(term)
}

/// The index of `key` within `keys` under exact (`=:=`) equality, if present.
///
/// The comparison is [`compare::eq`] — the same exact order `=:=` uses — so `1`
/// and `1.0` are different keys.
fn find_key(atoms: &AtomTable, keys: &[Eterm], key: Eterm) -> Option<usize> {
    keys.iter().position(|&k| compare::eq(k, key, atoms))
}

/// Builds a flat map from parallel key/value vectors, storing the keys in the
/// oracle's canonical **flat-map key order**.
///
/// ERTS keeps a flat map's key tuple sorted by `erts_cmp_flatmap_keys`
/// (`erts/emulator/beam/erl_utils.h:267-272`, `erts_validate_and_sort_flatmap`):
/// **two atom keys order by atom index, any other pair by the exact term order** —
/// reproduced by [`compare::cmp_flatmap_keys`]. That stored order is observable
/// through `maps:keys/1`, `maps:to_list/1`, `maps:iterator/1` and the
/// `term_to_binary/1` image (the encoder emits the pairs in stored order), so an
/// insertion-order writer diverges from the oracle. Oracle-pinned:
/// `maps:from_list([{3,a},{1,b},{2,c}])` ⇒ `#{1 => b, 2 => c, 3 => a}` (keys
/// `[1,2,3]`, image `<<131,116,0,0,0,3,97,1,119,1,98,97,2,119,1,99,97,3,119,1,97>>`).
///
/// Every caller passes already-deduplicated keys (each insert collapses a duplicate
/// last-wins), so this sort is the only normalization the writer needs.
///
/// # Errors
///
/// [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit) when there are more than [`MAP_SMALL_MAP_LIMIT`]
/// pairs (a HAMT map, which Milestone-1 cannot build); otherwise propagates the
/// heap's allocation failure.
fn build_map(ctx: &mut BifContext<'_>, keys: &[Eterm], values: &[Eterm]) -> Result<Eterm> {
    if keys.len() > MAP_SMALL_MAP_LIMIT {
        return Err(system_limit());
    }
    let mut pairs: Vec<(Eterm, Eterm)> =
        keys.iter().copied().zip(values.iter().copied()).collect();
    pairs.sort_by(|a, b| compare::cmp_flatmap_keys(a.0, b.0, ctx.atoms()));
    access::make_flatmap(ctx.heap(), &pairs)
}

/// Builds the proper list of `{Key, Value}` pair tuples for a map argument.
///
/// This is both `maps:to_list/1`'s result and the Milestone-1 iterator value (see
/// the module docs).
fn pair_list_of(ctx: &mut BifContext<'_>, map: Eterm) -> Result<Eterm> {
    let (keys, values) = map_parts(ctx, map)?;
    let mut pairs = Vec::with_capacity(keys.len());
    for (k, v) in keys.iter().zip(values.iter()) {
        pairs.push(ctx.tuple(&[*k, *v])?);
    }
    ctx.list(&pairs)
}

/// Whether `term` is exactly the atom `name`.
fn is_atom_named(ctx: &BifContext<'_>, term: Eterm, name: &str) -> bool {
    access::is_atom(term) && ctx.atoms().name(atom_val(term)) == Some(name)
}

/// The keys of `map` that occur in `wanted` (a list of candidate keys).
///
/// Collects the `(key, value)` pairs of `map` whose key is `=:=` to one of
/// `wanted` — the shared core of `maps:with/2`.
fn selected_pairs(
    atoms: &AtomTable,
    keys: &[Eterm],
    values: &[Eterm],
    wanted: &[Eterm],
) -> (Vec<Eterm>, Vec<Eterm>) {
    let mut out_keys = Vec::new();
    let mut out_values = Vec::new();
    for (&k, &v) in keys.iter().zip(values.iter()) {
        if wanted.iter().any(|&w| compare::eq(w, k, atoms)) {
            out_keys.push(k);
            out_values.push(v);
        }
    }
    (out_keys, out_values)
}

// ---------------------------------------------------------------------------
// BIFs
// ---------------------------------------------------------------------------

/// `maps:new/0` — the empty map `#{}` (a three-word empty flat map).
fn new_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    access::make_flatmap(ctx.heap(), &[])
}

/// `maps:from_list/1` — a map from a list of `{Key, Value}` tuples.
///
/// A later duplicate key wins (`maps:from_list([{1,a},{1,b}])` ⇒ `#{1 => b}`,
/// verified against the oracle).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when the argument is not a proper list, or an element is
/// not a 2-tuple (both exactly the oracle's `badarg`);
/// [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit)
/// when the result needs a hashmap (>32 distinct keys).
fn from_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let elems = proper_list(args[0]).ok_or_else(badarg)?;
    let mut keys: Vec<Eterm> = Vec::new();
    let mut values: Vec<Eterm> = Vec::new();
    for elem in elems {
        if !access::is_tuple(elem) || access::tuple_size(elem) != 2 {
            return Err(badarg());
        }
        let key = access::tuple_element(elem, 0);
        let value = access::tuple_element(elem, 1);
        match find_key(ctx.atoms(), &keys, key) {
            Some(i) => values[i] = value,
            None => {
                keys.push(key);
                values.push(value);
            }
        }
    }
    build_map(ctx, &keys, &values)
}

/// `maps:to_list/1` — the map's `{Key, Value}` pairs in stored order.
fn to_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    pair_list_of(ctx, args[0])
}

/// `maps:get/2` and `erlang:map_get/2` — the value for `Key`.
///
/// # Errors
///
/// A structured raise when `Map` is not a map (`error:{badmap, Map}`) or `Key` is
/// absent (`error:{badkey, Key}`) — the oracle's own reasons, carried as the live
/// reason term of the [`ExitReason::Structured`](beam_common::ExitReason::Structured)
/// raise. Pinned by the oracle:
/// `erl -noshell -eval 'try maps:get(a,1) catch C:R -> io:format("~p:~p~n",[C,R]) end, halt().'`
/// ⇒ `error:{badmap,1}`; the same with `maps:get(a,#{})` ⇒ `error:{badkey,a}`.
fn get_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (keys, values) = map_parts(ctx, args[1])?;
    match find_key(ctx.atoms(), &keys, args[0]) {
        Some(i) => Ok(values[i]),
        None => Err(structured_error(ctx, "badkey", &[args[0]])?),
    }
}

/// `maps:get/3` — the value for `Key`, or the default `Default` when absent.
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Map}` when `Map` is not a map (the live
/// reason term of the raise).
fn get_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (keys, values) = map_parts(ctx, args[1])?;
    match find_key(ctx.atoms(), &keys, args[0]) {
        Some(i) => Ok(values[i]),
        None => Ok(args[2]),
    }
}

/// `maps:find/2` — `{ok, Value}` when `Key` is present, else the atom `error`.
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Map}` when `Map` is not a map (the live
/// reason term of the raise); an absent `Key` is not an error.
fn find_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let found = {
        let (keys, values) = map_parts(ctx, args[1])?;
        find_key(ctx.atoms(), &keys, args[0]).map(|i| values[i])
    };
    match found {
        Some(value) => {
            let ok = ctx.ok()?;
            ctx.tuple(&[ok, value])
        }
        None => ctx.atom("error"),
    }
}

/// `maps:put/3` — a new map with `Key` bound to `Value` (inserting if absent).
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Map}` when `Map` is not a map (the live
/// reason term of the raise);
/// [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit) when
/// inserting a 33rd key needs a hashmap.
fn put_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (mut keys, mut values) = map_parts(ctx, args[2])?;
    match find_key(ctx.atoms(), &keys, args[0]) {
        Some(i) => values[i] = args[1],
        None => {
            keys.push(args[0]);
            values.push(args[1]);
        }
    }
    build_map(ctx, &keys, &values)
}

/// `maps:update/3` — a new map with `Key`'s value replaced by `Value`.
///
/// Unlike `put/3` this does **not** insert: a missing `Key` is the oracle's
/// `error:{badkey, Key}` (the live reason term of the structured raise). Pinned by the
/// oracle: `erl -noshell -eval 'try maps:update(a,1,#{}) catch C:R ->
/// io:format("~p:~p~n",[C,R]) end, halt().'` ⇒ `error:{badkey,a}`.
///
/// # Errors
///
/// A structured raise when `Map` is not a map (`error:{badmap, Map}`) or `Key` is absent
/// (`error:{badkey, Key}`).
fn update_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (keys, mut values) = map_parts(ctx, args[2])?;
    match find_key(ctx.atoms(), &keys, args[0]) {
        Some(i) => {
            values[i] = args[1];
            build_map(ctx, &keys, &values)
        }
        None => Err(structured_error(ctx, "badkey", &[args[0]])?),
    }
}

/// `maps:remove/2` — a new map without `Key` (a no-op when it is absent).
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Map}` when `Map` is not a map (the live
/// reason term of the raise).
fn remove_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (keys, values) = map_parts(ctx, args[1])?;
    let removed = find_key(ctx.atoms(), &keys, args[0]);
    let mut out_keys = Vec::with_capacity(keys.len());
    let mut out_values = Vec::with_capacity(values.len());
    for (i, (&k, &v)) in keys.iter().zip(values.iter()).enumerate() {
        if Some(i) != removed {
            out_keys.push(k);
            out_values.push(v);
        }
    }
    build_map(ctx, &out_keys, &out_values)
}

/// `maps:take/2` — `{Value, MapWithoutKey}` when `Key` is present, else `error`.
///
/// The key match is exact (`=:=`), like every other key lookup here: the oracle
/// gives `maps:take(1.0, #{1 => x})` ⇒ `error` (and `maps:take(1, #{1.0 => x})`
/// ⇒ `error`).
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Map}` when `Map` is not a map (the live
/// reason term of the raise); [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit) can only
/// arise from a hashmap operand (defensive).
fn take_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (keys, values) = map_parts(ctx, args[1])?;
    let Some(i) = find_key(ctx.atoms(), &keys, args[0]) else {
        return ctx.atom("error");
    };
    let value = values[i];
    let mut out_keys = Vec::with_capacity(keys.len());
    let mut out_values = Vec::with_capacity(values.len());
    for (j, (&k, &v)) in keys.iter().zip(values.iter()).enumerate() {
        if j != i {
            out_keys.push(k);
            out_values.push(v);
        }
    }
    let rest = build_map(ctx, &out_keys, &out_values)?;
    ctx.tuple(&[value, rest])
}

/// `maps:from_keys/2` — a map with every key of the list `Keys` bound to `Value`.
///
/// A duplicate key collapses to one pair (`maps:from_keys([a,a], 2)` ⇒ `#{a => 2}`),
/// and the empty list gives `#{}`. The keys may be any terms, and two keys that are
/// *distinct* under `=:=` but equal under `==` stay separate
/// (`maps:from_keys([1,1.0], v)` ⇒ `#{1 => v, 1.0 => v}`).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `Keys` is not a proper list (the oracle's `badarg`);
/// [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit) when more than 32 *distinct* keys would need a
/// hashmap (a Milestone-1 gap — see the module docs).
fn from_keys_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let keys = proper_list(args[0]).ok_or_else(badarg)?;
    let mut out_keys: Vec<Eterm> = Vec::new();
    for key in keys {
        if find_key(ctx.atoms(), &out_keys, key).is_none() {
            out_keys.push(key);
        }
    }
    let values = vec![args[1]; out_keys.len()];
    build_map(ctx, &out_keys, &values)
}

/// `maps:keys/1` — the map's keys, in stored order.
fn keys_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (keys, _) = map_parts(ctx, args[0])?;
    ctx.list(&keys)
}

/// `maps:values/1` — the map's values, in stored order.
fn values_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (_, values) = map_parts(ctx, args[0])?;
    ctx.list(&values)
}

/// `maps:size/1` and `erlang:map_size/1` — the number of pairs.
fn size_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (keys, _) = map_parts(ctx, args[0])?;
    ctx.int(keys.len() as i64)
}

/// `maps:is_key/2` and `erlang:is_map_key/2` — whether `Key` is present.
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Map}` when `Map` is not a map (the live
/// reason term of the raise).
fn is_key_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (keys, _) = map_parts(ctx, args[1])?;
    let present = find_key(ctx.atoms(), &keys, args[0]).is_some();
    ctx.boolean(present)
}

/// `maps:merge/2` — the union of two maps; `Map2`'s value wins on a shared key.
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Map}` naming whichever argument is not a map
/// (the live reason term of the raise);
/// [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit) when the union needs a hashmap (>32 pairs).
fn merge_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (mut keys, mut values) = map_parts(ctx, args[0])?;
    let (keys2, values2) = map_parts(ctx, args[1])?;
    for (&k, &v) in keys2.iter().zip(values2.iter()) {
        match find_key(ctx.atoms(), &keys, k) {
            Some(i) => values[i] = v,
            None => {
                keys.push(k);
                values.push(v);
            }
        }
    }
    build_map(ctx, &keys, &values)
}

/// `maps:with/2` — the entries of `Map` whose key is in `Keys`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `Keys` is not a proper list (the oracle's atom
/// `badarg`); the oracle's structured `error:{badmap, Map}` when `Map` is not a map.
fn with_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let wanted = proper_list(args[0]).ok_or_else(badarg)?;
    let (keys, values) = map_parts(ctx, args[1])?;
    let (out_keys, out_values) = selected_pairs(ctx.atoms(), &keys, &values, &wanted);
    build_map(ctx, &out_keys, &out_values)
}

/// `maps:without/2` — the entries of `Map` whose key is **not** in `Keys`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `Keys` is not a proper list (the oracle's atom
/// `badarg`); the oracle's structured `error:{badmap, Map}` when `Map` is not a map.
fn without_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let unwanted = proper_list(args[0]).ok_or_else(badarg)?;
    let (keys, values) = map_parts(ctx, args[1])?;
    let mut out_keys = Vec::new();
    let mut out_values = Vec::new();
    for (&k, &v) in keys.iter().zip(values.iter()) {
        if !unwanted.iter().any(|&w| compare::eq(w, k, ctx.atoms())) {
            out_keys.push(k);
            out_values.push(v);
        }
    }
    build_map(ctx, &out_keys, &out_values)
}

/// `maps:iterator/1` — the Milestone-1 iterator: the map's key/value list.
///
/// See the module docs; real ERTS returns an opaque iterator term.
fn iterator_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    pair_list_of(ctx, args[0])
}

/// `maps:next/1` — `{Key, Value, NextIter}`, or the atom `none` when exhausted.
///
/// `NextIter` is the remaining key/value list, or `none` once the last pair has been
/// returned; an already-exhausted iterator (the atom `none`, or the empty list the
/// [`iterator_1`] of `#{}` produces) yields `none`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when the argument is neither `none`, the empty list, nor a
/// list of 2-tuples (a malformed iterator).
fn next_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let iter = args[0];
    if access::is_nil(iter) || is_atom_named(ctx, iter, "none") {
        return ctx.atom("none");
    }
    if !access::is_list(iter) {
        return Err(badarg());
    }
    let pair = access::car(iter);
    if !access::is_tuple(pair) || access::tuple_size(pair) != 2 {
        return Err(badarg());
    }
    let key = access::tuple_element(pair, 0);
    let value = access::tuple_element(pair, 1);
    let rest = access::cdr(iter);
    let next_iter = if access::is_nil(rest) {
        ctx.atom("none")?
    } else {
        rest
    };
    ctx.tuple(&[key, value, next_iter])
}

// ---------------------------------------------------------------------------
// The `fun`-taking BIFs (via the `FunCaller` seam)
// ---------------------------------------------------------------------------

/// The boolean value of a `true`/`false` **atom** predicate result, else the oracle's
/// `error:{case_clause, Value}`.
///
/// `maps:filter/2` reads its predicate through the oracle's `case Pred(K, V) of
/// true -> …; false -> … end`, so a non-boolean is an error and not a truthy value.
/// The reason is the structured `{case_clause, Value}` (the live reason term of the
/// raise). Pinned by the oracle:
/// `erl -noshell -eval 'try maps:filter(fun(_,_) -> 5 end, #{a => 1}) catch C:R ->
/// io:format("~p:~p~n",[C,R]) end, halt().'` ⇒ `error:{case_clause,5}`.
fn predicate_holds(ctx: &mut BifContext<'_>, result: Eterm) -> Result<bool> {
    if is_atom_named(ctx, result, "true") {
        Ok(true)
    } else if is_atom_named(ctx, result, "false") {
        Ok(false)
    } else {
        Err(structured_error(ctx, "case_clause", &[result])?)
    }
}

/// `maps:map(Fun, Map)` — `Fun(Key, Value)` supplies each value.
///
/// The keys and their order are unchanged; only the values are the fun's results.
/// A `>32`-pair result is [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit)
/// like every other map producer here.
fn map_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let (keys, values) = map_parts(ctx, args[1])?;
    let mut mapped = Vec::with_capacity(values.len());
    for (&key, &value) in keys.iter().zip(values.iter()) {
        mapped.push(ctx.call_fun(fun, &[key, value])?);
    }
    build_map(ctx, &keys, &mapped)
}

/// `maps:fold(Fun, Init, Map)` — `Acc = Fun(Key, Value, Acc)` over every pair.
fn fold_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let mut acc = args[1];
    let (keys, values) = map_parts(ctx, args[2])?;
    for (&key, &value) in keys.iter().zip(values.iter()) {
        acc = ctx.call_fun(fun, &[key, value, acc])?;
    }
    Ok(acc)
}

/// `maps:filter(Pred, Map)` — the `{Key, Value}` pairs whose predicate is `true`.
fn filter_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let (keys, values) = map_parts(ctx, args[1])?;
    let mut kept_keys = Vec::new();
    let mut kept_values = Vec::new();
    for (&key, &value) in keys.iter().zip(values.iter()) {
        let result = ctx.call_fun(fun, &[key, value])?;
        if predicate_holds(ctx, result)? {
            kept_keys.push(key);
            kept_values.push(value);
        }
    }
    build_map(ctx, &kept_keys, &kept_values)
}

/// `maps:foreach(Fun, Map)` — `Fun(Key, Value)` for its side effects; `ok`.
fn foreach_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let (keys, values) = map_parts(ctx, args[1])?;
    for (&key, &value) in keys.iter().zip(values.iter()) {
        ctx.call_fun(fun, &[key, value])?;
    }
    ctx.ok()
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Registers the `maps` module BIFs and the `erlang:map_*` entries.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("maps", "new", 0, new_0);
    reg.register("maps", "from_list", 1, from_list_1);
    reg.register("maps", "to_list", 1, to_list_1);
    reg.register("maps", "get", 2, get_2);
    reg.register("maps", "get", 3, get_3);
    reg.register("maps", "find", 2, find_2);
    reg.register("maps", "put", 3, put_3);
    reg.register("maps", "update", 3, update_3);
    reg.register("maps", "remove", 2, remove_2);
    reg.register("maps", "take", 2, take_2);
    reg.register("maps", "from_keys", 2, from_keys_2);
    reg.register("maps", "keys", 1, keys_1);
    reg.register("maps", "values", 1, values_1);
    reg.register("maps", "size", 1, size_1);
    reg.register("maps", "is_key", 2, is_key_2);
    reg.register("maps", "merge", 2, merge_2);
    reg.register("maps", "with", 2, with_2);
    reg.register("maps", "without", 2, without_2);
    reg.register("maps", "iterator", 1, iterator_1);
    reg.register("maps", "next", 1, next_1);
    // The `fun`-taking BIFs: they call their fun through `BifContext::call_fun`.
    reg.register("maps", "map", 2, map_2);
    reg.register("maps", "fold", 3, fold_3);
    reg.register("maps", "filter", 2, filter_2);
    reg.register("maps", "foreach", 2, foreach_2);
    // The `erlang:map_*` entries of `bif.tab` (`ubif erlang:map_size/1`,
    // `erlang:map_get/2`, `erlang:is_map_key/2`).
    reg.register("erlang", "map_get", 2, get_2);
    reg.register("erlang", "map_size", 1, size_1);
    reg.register("erlang", "is_map_key", 2, is_key_2);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable, NIL};

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "true", "false", "ok", "error", "none", "a", "b", "c", "badmap", "badkey",
            "case_clause",
        ]);
        atoms
    }

    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(8192).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    /// Asserts that `result` is the oracle's `error:{Tag, Value}` **structured** raise:
    /// the class is `error`, the reason is `ExitReason::Structured` (so
    /// `BeamError::as_atom` is `None`) and the live reason term is the 2-tuple
    /// `{Tag, Value}`.
    #[track_caller]
    fn assert_structured(
        result: Result<Eterm>,
        ctx: &BifContext<'_>,
        tag: &str,
        value: Eterm,
    ) {
        match result {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(raise.class, beam_common::ExceptionClass::Error);
                assert_eq!(raise.reason, beam_common::ExitReason::Structured);
                assert_eq!(raise.reason.as_atom(), None);
                let term = *raise
                    .live_reason()
                    .expect("a structured reason carries its live term")
                    .downcast_ref::<Eterm>()
                    .expect("the payload is the live reason term");
                assert_eq!(access::tuple_size(term), 2, "{tag} reason arity");
                assert_eq!(
                    access::tuple_element(term, 0),
                    ctx.atom(tag).expect("a seeded tag atom"),
                    "{tag} reason tag"
                );
                assert!(
                    compare::eq(access::tuple_element(term, 1), value, ctx.atoms()),
                    "{tag} reason value"
                );
            }
            other => panic!("expected the structured {tag} raise, got {other:?}"),
        }
    }

    /// A flat map with `pairs` on the context heap.
    fn map(ctx: &mut BifContext<'_>, pairs: &[(Eterm, Eterm)]) -> Eterm {
        access::make_flatmap(ctx.heap(), pairs).expect("the map fits")
    }

    /// The stored keys of a flat map.
    fn keys_of(term: Eterm) -> Vec<Eterm> {
        access::flatmap_pairs(term).expect("a flat map").0
    }

    /// The stored values of a flat map.
    fn values_of(term: Eterm) -> Vec<Eterm> {
        access::flatmap_pairs(term).expect("a flat map").1
    }

    #[test]
    fn new_builds_the_empty_map() {
        with_ctx(|ctx| {
            let m = new_0(ctx, &[]).expect("maps:new/0");
            assert!(access::is_map(m) && access::is_flatmap(m));
            assert!(keys_of(m).is_empty());
            assert_eq!(size_1(ctx, &[m]).unwrap(), make_small(0));
            assert_eq!(keys_1(ctx, &[m]).unwrap(), NIL);
            assert_eq!(values_1(ctx, &[m]).unwrap(), NIL);
            assert_eq!(to_list_1(ctx, &[m]).unwrap(), NIL);
        });
    }

    #[test]
    fn from_list_and_to_list_round_trip() {
        with_ctx(|ctx| {
            let one = ctx.int(1).unwrap();
            let two = ctx.int(2).unwrap();
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();
            let p1 = ctx.tuple(&[one, a]).unwrap();
            let p2 = ctx.tuple(&[two, b]).unwrap();
            let list = ctx.list(&[p1, p2]).unwrap();

            let m = from_list_1(ctx, &[list]).expect("maps:from_list/1");
            assert!(access::is_flatmap(m));
            assert_eq!(keys_of(m), vec![one, two]);
            assert_eq!(values_of(m), vec![a, b]);

            // `to_list/1` rebuilds the `{K, V}` list in stored order.
            let back = to_list_1(ctx, &[m]).expect("maps:to_list/1");
            let elems = proper_list(back).expect("a proper list");
            assert_eq!(elems.len(), 2);
            assert_eq!(access::tuple_element(elems[0], 0), one);
            assert_eq!(access::tuple_element(elems[0], 1), a);
            assert_eq!(access::tuple_element(elems[1], 0), two);
            assert_eq!(access::tuple_element(elems[1], 1), b);
        });
    }

    #[test]
    fn from_list_duplicate_key_later_wins() {
        with_ctx(|ctx| {
            let one = ctx.int(1).unwrap();
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();
            let p1 = ctx.tuple(&[one, a]).unwrap();
            let p2 = ctx.tuple(&[one, b]).unwrap();
            let list = ctx.list(&[p1, p2]).unwrap();

            // Oracle: `maps:from_list([{1,a},{1,b}])` ⇒ `#{1 => b}`.
            let m = from_list_1(ctx, &[list]).expect("a two-element list");
            assert_eq!(size_1(ctx, &[m]).unwrap(), make_small(1));
            assert_eq!(keys_of(m), vec![one]);
            assert_eq!(values_of(m), vec![b]);
        });
    }

    #[test]
    fn from_list_stores_keys_in_the_canonical_flatmap_key_order() {
        with_ctx(|ctx| {
            // Oracle pin:
            //   erl -noshell -eval 'io:format("~w~n",[maps:keys(maps:from_list([{3,a},{1,b},{2,c}]))]), halt().'
            //     => [1,2,3]
            //   erl -noshell -eval 'io:format("~w~n",[term_to_binary(maps:from_list([{3,a},{1,b},{2,c}]))]), halt().'
            //     => <<131,116,0,0,0,3,97,1,119,1,98,97,2,119,1,99,97,3,119,1,97>>
            // i.e. `#{1 => b, 2 => c, 3 => a}`: the keys are stored sorted, so the
            // *values* follow the keys, not the input list.
            let one = ctx.int(1).unwrap();
            let two = ctx.int(2).unwrap();
            let three = ctx.int(3).unwrap();
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();
            let c = ctx.atom("c").unwrap();
            let p3 = ctx.tuple(&[three, a]).unwrap();
            let p1 = ctx.tuple(&[one, b]).unwrap();
            let p2 = ctx.tuple(&[two, c]).unwrap();
            let list = ctx.list(&[p3, p1, p2]).unwrap();

            let m = from_list_1(ctx, &[list]).expect("maps:from_list/1");
            // The insertion order would be [3, 1, 2]; the oracle stores [1, 2, 3].
            assert_eq!(keys_of(m), vec![one, two, three]);
            assert_eq!(values_of(m), vec![b, c, a]);

            // `to_list/1` reports the same stored order.
            let back = to_list_1(ctx, &[m]).expect("maps:to_list/1");
            let elems = proper_list(back).expect("a proper list");
            assert_eq!(access::tuple_element(elems[0], 0), one);
            assert_eq!(access::tuple_element(elems[0], 1), b);
            assert_eq!(access::tuple_element(elems[2], 0), three);
            assert_eq!(access::tuple_element(elems[2], 1), a);
        });
    }

    #[test]
    fn atom_keys_order_by_the_flatmap_key_rule_not_by_insertion() {
        with_ctx(|ctx| {
            // Oracle: atom keys are stored by *atom index* (`erts_cmp_flatmap_keys`),
            // never by insertion order or by name:
            //   erl -noshell -eval 'X=list_to_atom("zzzq"), Y=list_to_atom("aaaq"),
            //     io:format("~w~n",[maps:keys(maps:from_list([{X,1},{Y,2}]))]), halt().'
            //     => [zzzq,aaaq]   (X was interned first)
            //   erl -noshell -eval 'io:format("~w~n",[maps:keys(maps:from_list([{a,1},{b,2},{c,3}]))]), halt().'
            //     => [c,b,a]       (in ERTS, index(c) < index(b) < index(a))
            // This test table seeds `a`, `b`, `c` in that order, so here the index
            // order *is* [a, b, c] and an insertion-order writer would give [c, b, a].
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();
            let c = ctx.atom("c").unwrap();
            let pairs: Vec<Eterm> = [c, b, a]
                .iter()
                .map(|&k| ctx.tuple(&[k, make_small(1)]).unwrap())
                .collect();
            let list = ctx.list(&pairs).unwrap();

            let m = from_list_1(ctx, &[list]).expect("maps:from_list/1");
            assert_eq!(keys_of(m), vec![a, b, c]);
            // Exactly the order the shared flat-map key comparator defines.
            let mut expected = vec![a, b, c];
            expected.sort_by(|x, y| compare::cmp_flatmap_keys(*x, *y, ctx.atoms()));
            assert_eq!(keys_of(m), expected);
        });
    }

    #[test]
    fn every_producer_stores_keys_in_the_canonical_order() {
        with_ctx(|ctx| {
            let zero = ctx.int(0).unwrap();
            let one = ctx.int(1).unwrap();
            let two = ctx.int(2).unwrap();
            let a = ctx.atom("a").unwrap();

            let m = map(ctx, &[(one, a), (two, a)]);
            // Inserting a key that sorts *first* stores it first (the oracle's
            // `erts_maps_put` inserts into the sorted key tuple).
            let inserted = put_3(ctx, &[zero, a, m]).unwrap();
            assert_eq!(keys_of(inserted), vec![zero, one, two]);

            // `merge/2` re-stores the union in the same order.
            let first = map(ctx, &[(zero, a)]);
            let merged = merge_2(ctx, &[m, first]).unwrap();
            assert_eq!(keys_of(merged), vec![zero, one, two]);

            // `remove/2` of an absent key still yields the canonical order.
            let absent = remove_2(ctx, &[zero, m]).unwrap();
            assert_eq!(keys_of(absent), vec![one, two]);
        });
    }

    #[test]
    fn from_list_rejects_non_list_and_malformed_pairs() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").unwrap();

            // Not a list at all: oracle `badarg`.
            assert!(matches!(from_list_1(ctx, &[make_small(1)]), Err(BeamError::BadArg)));

            // A non-tuple element: oracle `badarg`.
            let not_a_pair = ctx.list(&[make_small(1)]).unwrap();
            assert!(matches!(
                from_list_1(ctx, &[not_a_pair]),
                Err(BeamError::BadArg)
            ));

            // A 3-tuple element: oracle `badarg` (only 2-tuples are pairs).
            let three = ctx.tuple(&[make_small(1), make_small(2), make_small(3)]).unwrap();
            let has_three = ctx.list(&[three]).unwrap();
            assert!(matches!(from_list_1(ctx, &[has_three]), Err(BeamError::BadArg)));

            // An improper list: the oracle's `function_clause` is reported as `badarg`.
            let improper = ctx.cons(make_small(1), a).unwrap();
            assert!(matches!(from_list_1(ctx, &[improper]), Err(BeamError::BadArg)));
        });
    }

    #[test]
    fn get_2_and_get_3() {
        with_ctx(|ctx| {
            let one = ctx.int(1).unwrap();
            let two = ctx.int(2).unwrap();
            let m = map(ctx, &[(one, two)]);

            assert_eq!(get_2(ctx, &[one, m]).unwrap(), two);
            // A missing key is the oracle's `error:{badkey, 9}`.
            //   erl -noshell -eval 'try maps:get(9,#{1=>2}) catch C:R ->
            //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{badkey,9}
            let miss = get_2(ctx, &[make_small(9), m]);
            assert_structured(miss, ctx, "badkey", make_small(9));
            // `get/3` returns the default on a miss.
            assert_eq!(get_3(ctx, &[make_small(9), m, make_small(7)]).unwrap(), make_small(7));
            assert_eq!(get_3(ctx, &[one, m, make_small(7)]).unwrap(), two);

            // A non-map second argument is the oracle's `error:{badmap, 9}`.
            //   erl -noshell -eval 'try maps:get(1,9) catch C:R ->
            //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{badmap,9}
            let bad_map = get_2(ctx, &[one, make_small(9)]);
            assert_structured(bad_map, ctx, "badmap", make_small(9));
            let bad_map_3 = get_3(ctx, &[one, make_small(9), make_small(7)]);
            assert_structured(bad_map_3, ctx, "badmap", make_small(9));
        });
    }

    #[test]
    fn key_identity_is_exact_so_one_is_not_one_point_zero() {
        with_ctx(|ctx| {
            let one = ctx.int(1).unwrap();
            let float_one = ctx.make_float(1.0).unwrap();
            let a = ctx.atom("a").unwrap();
            let m = map(ctx, &[(float_one, a)]);

            assert_eq!(get_2(ctx, &[float_one, m]).unwrap(), a);
            // `1` is not the key `1.0`: both miss with the oracle's `error:{badkey, 1}`.
            //   erl -noshell -eval 'try maps:update(1,a,#{1.0=>a}) catch C:R ->
            //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{badkey,1}
            let miss = get_2(ctx, &[one, m]);
            assert_structured(miss, ctx, "badkey", one);
            let miss_update = update_3(ctx, &[one, a, m]);
            assert_structured(miss_update, ctx, "badkey", one);
            assert_eq!(is_key_2(ctx, &[one, m]).unwrap(), ctx.boolean(false).unwrap());
            assert_eq!(is_key_2(ctx, &[float_one, m]).unwrap(), ctx.boolean(true).unwrap());
            // `put(1, ...)` adds a second, distinct key.
            let two_keys = put_3(ctx, &[one, a, m]).unwrap();
            assert_eq!(size_1(ctx, &[two_keys]).unwrap(), make_small(2));
        });
    }

    #[test]
    fn find_2_returns_ok_or_error() {
        with_ctx(|ctx| {
            let one = ctx.int(1).unwrap();
            let two = ctx.int(2).unwrap();
            let m = map(ctx, &[(one, two)]);

            let hit = find_2(ctx, &[one, m]).unwrap();
            assert!(access::is_tuple(hit) && access::tuple_size(hit) == 2);
            assert_eq!(access::tuple_element(hit, 0), ctx.ok().unwrap());
            assert_eq!(access::tuple_element(hit, 1), two);

            let miss = find_2(ctx, &[make_small(9), m]).unwrap();
            assert!(is_atom_named(ctx, miss, "error"));

            // A non-map operand is the oracle's `error:{badmap, 9}`.
            //   erl -noshell -eval 'try maps:find(1,9) catch C:R ->
            //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{badmap,9}
            let bad_map = find_2(ctx, &[one, make_small(9)]);
            assert_structured(bad_map, ctx, "badmap", make_small(9));
        });
    }

    #[test]
    fn put_inserts_and_replaces_without_mutating_the_input() {
        with_ctx(|ctx| {
            let one = ctx.int(1).unwrap();
            let two = ctx.int(2).unwrap();
            let nine = ctx.int(9).unwrap();
            let m = map(ctx, &[(one, two)]);

            let replaced = put_3(ctx, &[one, nine, m]).unwrap();
            assert_eq!(keys_of(replaced), vec![one]);
            assert_eq!(values_of(replaced), vec![nine]);
            // The original map is unchanged.
            assert_eq!(values_of(m), vec![two]);

            let inserted = put_3(ctx, &[nine, one, m]).unwrap();
            assert_eq!(keys_of(inserted), vec![one, nine]);
            assert_eq!(values_of(inserted), vec![two, one]);

            // A non-map third argument is the oracle's `error:{badmap, 9}`.
            //   erl -noshell -eval 'try maps:put(1,2,9) catch C:R ->
            //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{badmap,9}
            let bad_map = put_3(ctx, &[one, two, nine]);
            assert_structured(bad_map, ctx, "badmap", nine);
        });
    }

    #[test]
    fn update_replaces_but_rejects_an_absent_key() {
        with_ctx(|ctx| {
            let one = ctx.int(1).unwrap();
            let two = ctx.int(2).unwrap();
            let nine = ctx.int(9).unwrap();
            let m = map(ctx, &[(one, two)]);

            let updated = update_3(ctx, &[one, nine, m]).unwrap();
            assert_eq!(keys_of(updated), vec![one]);
            assert_eq!(values_of(updated), vec![nine]);

            // It does not insert: a missing key is the oracle's `error:{badkey, 9}`.
            //   erl -noshell -eval 'try maps:update(9,1,#{1=>2}) catch C:R ->
            //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{badkey,9}
            let miss = update_3(ctx, &[nine, one, m]);
            assert_structured(miss, ctx, "badkey", nine);
            let bad_map = update_3(ctx, &[one, one, nine]);
            assert_structured(bad_map, ctx, "badmap", nine);
        });
    }

    #[test]
    fn remove_drops_a_key_and_is_a_no_op_when_absent() {
        with_ctx(|ctx| {
            let one = ctx.int(1).unwrap();
            let two = ctx.int(2).unwrap();
            let nine = ctx.int(9).unwrap();
            let m = map(ctx, &[(one, two), (nine, one)]);

            let removed = remove_2(ctx, &[one, m]).unwrap();
            assert_eq!(keys_of(removed), vec![nine]);
            assert_eq!(values_of(removed), vec![one]);

            // Removing an absent key returns the map unchanged.
            let absent = remove_2(ctx, &[make_small(7), m]).unwrap();
            assert_eq!(keys_of(absent), vec![one, nine]);

            // A non-map second argument is the oracle's `error:{badmap, 9}`.
            //   erl -noshell -eval 'try maps:remove(1,9) catch C:R ->
            //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{badmap,9}
            let bad_map = remove_2(ctx, &[one, nine]);
            assert_structured(bad_map, ctx, "badmap", nine);
        });
    }

    #[test]
    fn keys_values_size_and_is_key() {
        with_ctx(|ctx| {
            let one = ctx.int(1).unwrap();
            let two = ctx.int(2).unwrap();
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();
            let m = map(ctx, &[(one, a), (two, b)]);

            let ks = keys_1(ctx, &[m]).unwrap();
            assert_eq!(proper_list(ks).unwrap(), vec![one, two]);
            let vs = values_1(ctx, &[m]).unwrap();
            assert_eq!(proper_list(vs).unwrap(), vec![a, b]);
            assert_eq!(size_1(ctx, &[m]).unwrap(), make_small(2));

            assert_eq!(is_key_2(ctx, &[one, m]).unwrap(), ctx.boolean(true).unwrap());
            assert_eq!(is_key_2(ctx, &[a, m]).unwrap(), ctx.boolean(false).unwrap());

            // Every non-map operand is the oracle's `{badmap, Operand}`:
            //   erl -noshell -eval 'lists:foreach(fun(F) -> try F() catch C:R ->
            //     io:format("~p:~p~n",[C,R]) end end, [fun() -> maps:keys(1) end,
            //     fun() -> maps:values(a) end, fun() -> maps:size(1) end,
            //     fun() -> maps:is_key(1,1) end]), halt().'
            //   =>  error:{badmap,1} / error:{badmap,a} / error:{badmap,1} / error:{badmap,1}
            for bad in [one, a] {
                let keys = keys_1(ctx, &[bad]);
                assert_structured(keys, ctx, "badmap", bad);
                let values = values_1(ctx, &[bad]);
                assert_structured(values, ctx, "badmap", bad);
                let size = size_1(ctx, &[bad]);
                assert_structured(size, ctx, "badmap", bad);
            }
            let not_a_map = is_key_2(ctx, &[one, one]);
            assert_structured(not_a_map, ctx, "badmap", one);
        });
    }

    #[test]
    fn merge_is_the_union_with_the_second_map_winning() {
        with_ctx(|ctx| {
            let one = ctx.int(1).unwrap();
            let two = ctx.int(2).unwrap();
            let nine = ctx.int(9).unwrap();
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();

            let m1 = map(ctx, &[(one, a), (two, b)]);
            let m2 = map(ctx, &[(two, a), (nine, b)]);

            let merged = merge_2(ctx, &[m1, m2]).unwrap();
            assert_eq!(keys_of(merged), vec![one, two, nine]);
            // `two`'s value comes from the second map.
            assert_eq!(values_of(merged), vec![a, a, b]);

            // A non-map operand is the oracle's `{badmap, Operand}` — naming whichever
            // argument is not a map.
            //   erl -noshell -eval 'try maps:merge(#{},1) catch C:R ->
            //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{badmap,1}
            let bad_second = merge_2(ctx, &[m1, one]);
            assert_structured(bad_second, ctx, "badmap", one);
            let bad_first = merge_2(ctx, &[one, m1]);
            assert_structured(bad_first, ctx, "badmap", one);
        });
    }

    #[test]
    fn with_and_without_filter_by_key_list() {
        with_ctx(|ctx| {
            let one = ctx.int(1).unwrap();
            let two = ctx.int(2).unwrap();
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();
            let c = ctx.atom("c").unwrap();
            let m = map(ctx, &[(one, a), (two, b)]);

            // `maps:with([1, 9], #{1=>a,2=>b})` ⇒ `#{1=>a}`.
            let wanted = ctx.list(&[one, make_small(9)]).unwrap();
            let kept = with_2(ctx, &[wanted, m]).unwrap();
            assert_eq!(keys_of(kept), vec![one]);
            assert_eq!(values_of(kept), vec![a]);

            // `maps:with([], _)` and a duplicate key list are harmless.
            let empty = ctx.list(&[]).unwrap();
            assert!(keys_of(with_2(ctx, &[empty, m]).unwrap()).is_empty());
            let dup = ctx.list(&[one, one]).unwrap();
            assert_eq!(keys_of(with_2(ctx, &[dup, m]).unwrap()), vec![one]);

            // `maps:without([1], #{1=>a,2=>b})` ⇒ `#{2=>b}`.
            let drop = ctx.list(&[one]).unwrap();
            let rest = without_2(ctx, &[drop, m]).unwrap();
            assert_eq!(keys_of(rest), vec![two]);
            assert_eq!(values_of(rest), vec![b]);

            // A non-list key argument is the oracle's atom `badarg`; a non-map argument
            // is its structured `{badmap, C}`.
            //   erl -noshell -eval 'try maps:with(a,#{}) catch C:R ->
            //     io:format("~p:~p~n",[C,R]) end, halt().'    =>  error:badarg
            //   erl -noshell -eval 'try maps:with([],c) catch C:R ->
            //     io:format("~p:~p~n",[C,R]) end, halt().'    =>  error:{badmap,c}
            assert!(matches!(with_2(ctx, &[c, m]), Err(BeamError::BadArg)));
            assert!(matches!(without_2(ctx, &[c, m]), Err(BeamError::BadArg)));
            let bad_map = with_2(ctx, &[empty, c]);
            assert_structured(bad_map, ctx, "badmap", c);
            let bad_map = without_2(ctx, &[empty, c]);
            assert_structured(bad_map, ctx, "badmap", c);
        });
    }

    #[test]
    fn iterator_and_next_walk_the_pairs() {
        with_ctx(|ctx| {
            let one = ctx.int(1).unwrap();
            let two = ctx.int(2).unwrap();
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();
            let m = map(ctx, &[(one, a), (two, b)]);

            // The M1 iterator is the `{K, V}` list in stored order.
            let iter = iterator_1(ctx, &[m]).unwrap();
            let elems = proper_list(iter).expect("a proper list");
            assert_eq!(elems.len(), 2);
            assert_eq!(access::tuple_element(elems[0], 0), one);
            assert_eq!(access::tuple_element(elems[1], 0), two);

            // Pull the first pair: `{1, a, <remaining list>}`.
            let first = next_1(ctx, &[iter]).unwrap();
            assert!(access::is_tuple(first) && access::tuple_size(first) == 3);
            assert_eq!(access::tuple_element(first, 0), one);
            assert_eq!(access::tuple_element(first, 1), a);
            let rest = access::tuple_element(first, 2);
            assert_eq!(proper_list(rest).unwrap().len(), 1);

            // The last pair carries the terminal iterator `none`.
            let second = next_1(ctx, &[rest]).unwrap();
            assert_eq!(access::tuple_element(second, 0), two);
            assert_eq!(access::tuple_element(second, 1), b);
            assert!(is_atom_named(ctx, access::tuple_element(second, 2), "none"));

            // An exhausted iterator (the empty list, or the atom `none`) ⇒ `none`.
            let exhausted = next_1(ctx, &[NIL]).unwrap();
            assert!(is_atom_named(ctx, exhausted, "none"));
            let none_atom = ctx.atom("none").unwrap();
            let from_none = next_1(ctx, &[none_atom]).unwrap();
            assert!(is_atom_named(ctx, from_none, "none"));
            // `#{}`'s iterator is the empty list, which is already exhausted.
            let empty_map = new_0(ctx, &[]).unwrap();
            let empty_iter = iterator_1(ctx, &[empty_map]).unwrap();
            let exhausted_from_empty = next_1(ctx, &[empty_iter]).unwrap();
            assert!(is_atom_named(ctx, exhausted_from_empty, "none"));

            // A malformed iterator is `badarg`.
            assert!(matches!(next_1(ctx, &[make_small(1)]), Err(BeamError::BadArg)));
            let bad = ctx.list(&[make_small(1)]).unwrap();
            assert!(matches!(next_1(ctx, &[bad]), Err(BeamError::BadArg)));
            // `iterator/1` of a non-map is the oracle's `{badmap, 1}`:
            //   erl -noshell -eval 'try maps:iterator(1) catch C:R ->
            //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{badmap,1}
            let bad_map = iterator_1(ctx, &[make_small(1)]);
            assert_structured(bad_map, ctx, "badmap", make_small(1));
        });
    }

    #[test]
    fn erlang_map_entries_are_the_same_bifs() {
        with_ctx(|ctx| {
            let one = ctx.int(1).unwrap();
            let two = ctx.int(2).unwrap();
            let m = map(ctx, &[(one, two)]);

            assert_eq!(get_2(ctx, &[one, m]).unwrap(), two);
            assert_eq!(size_1(ctx, &[m]).unwrap(), make_small(1));
            assert_eq!(is_key_2(ctx, &[one, m]).unwrap(), ctx.boolean(true).unwrap());
        });
    }

    #[test]
    fn more_than_32_pairs_is_a_typed_error() {
        with_ctx(|ctx| {
            // 33 distinct keys need a hashmap the Milestone-1 layer cannot build.
            let pairs: Vec<Eterm> = (0..33)
                .map(|i| {
                    let k = ctx.int(i).unwrap();
                    ctx.tuple(&[k, k]).unwrap()
                })
                .collect();
            let list = ctx.list(&pairs).unwrap();
            assert!(matches!(from_list_1(ctx, &[list]), Err(BeamError::SystemLimit)));

            // A full 32-pair map is fine; adding a 33rd key overflows to hashmap.
            let full: Vec<(Eterm, Eterm)> = (0..32)
                .map(|i| {
                    let k = ctx.int(i).unwrap();
                    (k, k)
                })
                .collect();
            let m = map(ctx, &full);
            assert_eq!(size_1(ctx, &[m]).unwrap(), make_small(32));
            let extra = ctx.int(100).unwrap();
            assert!(matches!(put_3(ctx, &[extra, extra, m]), Err(BeamError::SystemLimit)));
            // Removing a key first keeps the result a flat map.
            let shrunk = remove_2(ctx, &[ctx.int(0).unwrap(), m]).unwrap();
            assert!(put_3(ctx, &[extra, extra, shrunk]).is_ok());
        });
    }

    #[test]
    fn register_installs_every_maps_bif() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        for (module, name, arity) in [
            ("maps", "new", 0),
            ("maps", "from_list", 1),
            ("maps", "to_list", 1),
            ("maps", "get", 2),
            ("maps", "get", 3),
            ("maps", "find", 2),
            ("maps", "put", 3),
            ("maps", "update", 3),
            ("maps", "remove", 2),
            ("maps", "take", 2),
            ("maps", "from_keys", 2),
            ("maps", "keys", 1),
            ("maps", "values", 1),
            ("maps", "size", 1),
            ("maps", "is_key", 2),
            ("maps", "merge", 2),
            ("maps", "with", 2),
            ("maps", "without", 2),
            ("maps", "iterator", 1),
            ("maps", "next", 1),
            // The `fun`-taking BIFs (backed by the `FunCaller` seam).
            ("maps", "map", 2),
            ("maps", "fold", 3),
            ("maps", "filter", 2),
            ("maps", "foreach", 2),
            ("erlang", "map_get", 2),
            ("erlang", "map_size", 1),
            ("erlang", "is_map_key", 2),
        ] {
            assert!(
                reg.contains(module, name, arity),
                "missing {module}:{name}/{arity}"
            );
        }
        assert_eq!(reg.len(), 27);
    }

    #[test]
    fn registered_bifs_run_through_the_registry() {
        let reg = {
            let mut reg = BifRegistry::new();
            register(&mut reg);
            reg
        };
        with_ctx(|ctx| {
            let empty = reg.call("maps", "new", 0, ctx, &[]).expect("maps:new/0");
            assert!(keys_of(empty).is_empty());
            assert_eq!(
                reg.call("erlang", "map_size", 1, ctx, &[empty]).expect("map_size/1"),
                make_small(0)
            );
        });
    }
}
