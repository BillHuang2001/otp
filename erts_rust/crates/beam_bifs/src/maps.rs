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

//! `maps` module BIFs over **both** map representations: `new/0`, `from_list/1`,
//! `to_list/1`, `get/2,3`, `find/2`, `put/3`, `update/3`, `remove/2`, `take/2`,
//! `from_keys/2`, `keys/1`, `values/1`, `size/1`, `is_key/2`, `merge/2`, `with/2`,
//! `without/2`, `iterator/1`, `next/1`, the `fun`-taking `map/2`, `fold/3`,
//! `filter/2` and `foreach/2`, plus the `erlang:map_*` entries `map_get/2`,
//! `map_size/1` and `is_map_key/2`.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_map.c` (the `maps_*`
//! BIFs and `erts_internal_map_next_3`), `../../lib/stdlib/src/maps.erl` (the
//! `new/0`/`to_list/1`/`iterator/1`/`next/1` wrappers) and
//! `../../erts/emulator/beam/bif.tab` (the roster: `bif maps:find/2`,
//! `maps:get/2`, `maps:from_list/1`, `maps:is_key/2`, `maps:keys/1`,
//! `maps:merge/2`, `maps:put/3`, `maps:remove/2`, `maps:update/3`,
//! `maps:values/1`; `ubif erlang:map_size/1`, `erlang:map_get/2`,
//! `erlang:is_map_key/2`).
//!
//! # The two map representations
//!
//! A map is a *flat* map when it holds at most `MAP_SMALL_MAP_LIMIT` (32) distinct
//! keys and a *HAMT* hash map above that (`erl_map.h`). Both layouts are owned by
//! `beam_term::map` (`write_map`/`map_pairs_storage_order`/`map_size`/`map_get`),
//! which this module reaches through [`crate::access`] — so every reader and every
//! producer below handles either representation, with no pair-count ceiling. The
//! writer ([`build`] → [`crate::access::make_map`] → `beam_term::map::write_map`)
//! picks the representation exactly as ERTS does (≤32 distinct keys ⇒ flat map with
//! the keys in `erts_cmp_flatmap_keys` order, more ⇒ a HAMT keyed by `erts_map_hash`)
//! and collapses duplicate keys **last value wins**.
//!
//! Reading a HAMT is the interesting half: an *existing* hash map reaches these BIFs
//! whenever `beam_codec` decodes a `MAP_EXT` blob with more than 32 distinct keys
//! (i.e. `binary_to_term/1`), or whenever one of the producers here returned one.
//!
//! # The `fun`-taking BIFs
//!
//! `map/2`, `fold/3`, `filter/2` and `foreach/2` are real `erl_map.c` BIFs, so
//! they are registered here rather than left to `maps.erl`. They call their fun
//! through [`BifContext::call_fun`] (the [`crate::FunCaller`] seam); with no caller
//! installed they raise `unsupported_bif`, and a fun's own failures propagate
//! verbatim.
//!
//! * `map(Fun, Map)` calls `Fun(Key, Value)` and stores the result — the keys are
//!   unchanged, the values are the fun's results, in the map's stored order.
//! * `fold(Fun, Init, Map)` is `Acc = Fun(Key, Value, Acc)`; `filter(Pred, Map)`
//!   keeps the pairs whose predicate returns `true`; `foreach(Fun, Map)` is called
//!   for its side effects and returns `ok`.
//! * A predicate result that is not a `true`/`false` atom is the oracle's
//!   `error:{case_clause, Value}` (see the structured-reason section below).
//! * The traversal order is the map's **stored** order (`map_pairs_storage_order`),
//!   so a `fold` that accumulates into a list sees the oracle's pair sequence.
//!
//! The fun argument is **not** arity-checked here — that is the runtime's job when
//! it invokes it — so `maps:map(5, #{})` returns `#{}` where the oracle raises
//! `badarg` (documented gap: a non-fun is only detected once a fun call happens).
//!
//! # Key identity is exact (`=:=`)
//!
//! Map keys are compared with the *exact* `=:=` order (`beam_term::compare::eq`,
//! via `beam_term::map`'s own lookup): `#{1.0 => a}` and `#{1 => a}` are therefore
//! distinct maps, and `maps:get(1, #{1.0 => a})` misses. Verified against the OTP 29
//! oracle: `maps:is_key(1, #{1.0 => a})` ⇒ `false`, while
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
//! # Iterators (`iterator/1`, `next/1`, `to_list/1`)
//!
//! `maps:iterator(Map)` is `[0 | Map]` — an improper cons whose head is the
//! **path** into the map and whose tail is the map itself (`maps.erl:1062`) —
//! *not* a `{Key, Value}` pair list. `maps:next(Iter)` returns the look-ahead form
//! `{Key, Value, NextIter}`. `maps:to_list/1` accepts **either** a map (its stored
//! order) **or** an iterator (the walk), and reports a bad iterator as the
//! structured `error:{badmap, Iter}` (`maps.erl:531-543`). Oracle-pinned (OTP 29 /
//! erts-17.1):
//!
//! * `maps:iterator(#{a=>1,b=>2,c=>3})` ⇒ `[0 | #{a=>1,b=>2,c=>3}]`.
//! * `maps:next([0 | #{c=>3,b=>2,a=>1}])` ⇒ `{c,3,{b,2,{a,1,none}}}` — a flat map's
//!   chain is emitted whole and terminates in `none`; the path is only bound-checked
//!   (`maps:next([3 | …])` gives the same chain, `[4 | …]` is `badarg`).
//! * `maps:next([Path | Map])` with a **list** path is the ordered form
//!   (`maps:iterator(M, ordered)`): `{K, maps:get(K, Map), …}` for each key of the
//!   path, `badarg` for a key the map lacks or an improper path tail, `none` at the
//!   end of the list.
//! * `maps:next({K,V,I})` ⇒ itself; `maps:next(none)` ⇒ `none`; anything else
//!   (`5`, `[]`, `[0 | 5]`, `[a | Map]`) is `badarg`.
//!
//! **Documented deviation (chain chunking).** For a HAMT with an *integer* path the
//! C emits at most `MAP_SMALL_MAP_LIMIT` pairs on the first (`path == 0`) call and
//! then `min(remaining reductions / 4, size)` pairs per later call, appending a
//! `[Path | Map]` continuation whose path is an internal trie-nibble encoding
//! (`erl_map.c:3783-3810`). The chunk size is therefore a function of the process's
//! remaining reduction budget — it changes between two identical calls in the same
//! process — and is not reproducible outside the emulator's reduction accounting.
//! This module emits the first call capped at [`MAP_SMALL_MAP_LIMIT`] pairs (matching
//! the oracle's deterministic `path == 0` case) and every later call to completion,
//! with the continuation path being this implementation's own opaque HAMT offset.
//! The **pair sequence**, the **order**, and the terminal `none` are identical to the
//! oracle's, which is all any walk (including `maps:to_list/1`) observes.
//!
//! # Order
//!
//! OTP documents map iteration order as unspecified, but the oracle is *not*
//! arbitrary: a flat map stores its keys in `erts_cmp_flatmap_keys` order (atom keys
//! by atom index, every other pair by the exact term order) and a HAMT in its
//! trie's slot-traversal order. [`crate::access::map_pairs_storage_order`] returns
//! exactly that order for either representation, and every order-defining reader
//! (`keys/1`, `values/1`, `to_list/1`, the iterator walk) reports it **unchanged** —
//! nothing here sorts a map's pairs. Oracle-pinned: `maps:keys/1` of the 33-key map
//! `maps:from_list([{N,N} || N <- lists:seq(1,33)])` is
//! `[18,4,12,19,29,13,2,7,31,8,10,23,9,15,32,1,25,28,20,6,11,17,24,14,33,3,16,30,21,5,27,26,22]`.
//!
//! The runtime's own atom indices are a documented gap (`beam_rt`'s `CORE_ATOMS` is
//! not the legacy `atom.names` order), so an *atom-keyed* flat map's order can still
//! differ from the oracle's while the ordering *rule* above is identical.

use beam_common::Result;
use beam_term::prelude::signed_value;
use beam_term::{atom_val, compare, Eterm};

use crate::access;
use crate::context::BifContext;
use crate::error::{badarg, structured_error};
use crate::print::proper_list;
use crate::registry::BifRegistry;

/// `MAP_SMALL_MAP_LIMIT` (`erl_map.h:96`): the largest flat-map pair count. ERTS
/// switches to a HAMT hash map above it. Mirroring
/// `beam_term::map::MAP_SMALL_MAP_LIMIT`, and used here only as the HAMT iterator's
/// per-call chunk cap (see the module docs).
const MAP_SMALL_MAP_LIMIT: usize = 32;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The `(key, value)` pairs of a *map* argument, in the map's **stored** order.
///
/// Both representations are handled: a flat map yields its keys-tuple order, a HAMT
/// its trie slot-traversal order ([`crate::access::map_pairs_storage_order`]) — the
/// order the oracle's `maps:keys/1`/`values/1`/`to_list/1` report.
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Term}` — an
/// [`ExitReason::Structured`](beam_common::ExitReason::Structured) raise carrying the
/// `{badmap, Term}` tuple as its live reason term — when `term` is not a map
/// (oracle: `maps:get(a, 1)` ⇒ `error:{badmap,1}`).
fn map_pairs(ctx: &mut BifContext<'_>, term: Eterm) -> Result<Vec<(Eterm, Eterm)>> {
    if !access::is_map(term) {
        return Err(structured_error(ctx, "badmap", &[term])?);
    }
    // `term` is a live map, so the accessor cannot fail (its only error is `BadMap`).
    access::map_pairs_storage_order(term)
}

/// Exact (`=:=`) lookup of `key` in the **map** `map` (`map` must already be a map).
///
/// # Errors
///
/// Propagates [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-map —
/// unreachable for a caller that classified first, but never swallowed.
fn map_lookup(ctx: &BifContext<'_>, map: Eterm, key: Eterm) -> Result<Option<Eterm>> {
    access::map_get(map, key, ctx.atoms())
}

/// Builds a map term from `pairs` on the context heap.
///
/// The representation (flat map ≤32 distinct keys, HAMT above) and the stored key
/// order are chosen by `beam_term::map::write_map`, which also collapses duplicate
/// keys **last value wins** — so callers may pass duplicates freely (the oracle's
/// `maps:from_list/1`, `merge/2`, `from_keys/2` and `put/3` all behave that way).
///
/// # Errors
///
/// Propagates the heap's allocation failure.
fn build(ctx: &mut BifContext<'_>, pairs: &[(Eterm, Eterm)]) -> Result<Eterm> {
    access::make_map(ctx.heap(), pairs)
}

/// Builds the proper list of `{Key, Value}` pair tuples for `pairs`.
fn pair_list(ctx: &mut BifContext<'_>, pairs: &[(Eterm, Eterm)]) -> Result<Eterm> {
    let mut out = Vec::with_capacity(pairs.len());
    for &(k, v) in pairs {
        out.push(ctx.tuple(&[k, v])?);
    }
    ctx.list(&out)
}

/// Whether `term` is exactly the atom `name`.
fn is_atom_named(ctx: &BifContext<'_>, term: Eterm, name: &str) -> bool {
    access::is_atom(term) && ctx.atoms().name(atom_val(term)) == Some(name)
}

// ---------------------------------------------------------------------------
// BIFs
// ---------------------------------------------------------------------------

/// `maps:new/0` — the empty map `#{}` (a three-word empty flat map).
fn new_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    build(ctx, &[])
}

/// `maps:from_list/1` — a map from a list of `{Key, Value}` tuples.
///
/// A later duplicate key wins (`maps:from_list([{1,a},{1,b}])` ⇒ `#{1 => b}`,
/// verified against the oracle), which `write_map`'s last-wins dedup gives directly.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when the argument is not a proper list, or an element is
/// not a 2-tuple (both exactly the oracle's `badarg`).
fn from_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let elems = proper_list(args[0]).ok_or_else(badarg)?;
    let mut pairs: Vec<(Eterm, Eterm)> = Vec::with_capacity(elems.len());
    for elem in elems {
        if !access::is_tuple(elem) || access::tuple_size(elem) != 2 {
            return Err(badarg());
        }
        pairs.push((access::tuple_element(elem, 0), access::tuple_element(elem, 1)));
    }
    build(ctx, &pairs)
}

/// `maps:to_list/1` — the `{Key, Value}` pairs of a **map** (stored order) or of an
/// **iterator** (the walk).
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Iter}` when the argument is neither a map
/// nor a walkable iterator (`maps.erl:533-538`; any failure of the underlying
/// `maps:next/1` is reported against the *original* argument, not the sub-iterator).
/// Oracle-pinned: `maps:to_list(5)` ⇒ `error:{badmap,5}`,
/// `maps:to_list({a,b,c})` ⇒ `error:{badmap,{a,b,c}}` (its third element `c` is not an
/// iterator), `maps:to_list([])` ⇒ `error:{badmap,[]}`.
fn to_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let arg = args[0];
    if access::is_map(arg) {
        let pairs = access::map_pairs_storage_order(arg)?;
        return pair_list(ctx, &pairs);
    }
    // Iterator path: `to_list_from_iterator(next(Iter))`.
    let badmap = |ctx: &mut BifContext<'_>| structured_error(ctx, "badmap", &[arg]).map(Err);
    let mut acc: Vec<Eterm> = Vec::new();
    let mut cur = match next_1(ctx, &[arg]) {
        Ok(v) => v,
        Err(_) => return badmap(ctx)?,
    };
    loop {
        if is_atom_named(ctx, cur, "none") {
            break;
        }
        if !(access::is_tuple(cur) && access::tuple_size(cur) == 3) {
            return badmap(ctx)?;
        }
        let key = access::tuple_element(cur, 0);
        let value = access::tuple_element(cur, 1);
        let next_iter = access::tuple_element(cur, 2);
        acc.push(ctx.tuple(&[key, value])?);
        cur = match next_1(ctx, &[next_iter]) {
            Ok(v) => v,
            Err(_) => return badmap(ctx)?,
        };
    }
    ctx.list(&acc)
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
    let map = args[1];
    if !access::is_map(map) {
        return Err(structured_error(ctx, "badmap", &[map])?);
    }
    match map_lookup(ctx, map, args[0])? {
        Some(value) => Ok(value),
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
    let map = args[1];
    if !access::is_map(map) {
        return Err(structured_error(ctx, "badmap", &[map])?);
    }
    Ok(map_lookup(ctx, map, args[0])?.unwrap_or(args[2]))
}

/// `maps:find/2` — `{ok, Value}` when `Key` is present, else the atom `error`.
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Map}` when `Map` is not a map (the live
/// reason term of the raise); an absent `Key` is not an error.
fn find_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let map = args[1];
    if !access::is_map(map) {
        return Err(structured_error(ctx, "badmap", &[map])?);
    }
    match map_lookup(ctx, map, args[0])? {
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
/// reason term of the raise).
fn put_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let mut pairs = map_pairs(ctx, args[2])?;
    // Appending makes `write_map`'s last-wins dedup pick the new value.
    pairs.push((args[0], args[1]));
    build(ctx, &pairs)
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
    let pairs = map_pairs(ctx, args[2])?;
    let mut out: Vec<(Eterm, Eterm)> = Vec::with_capacity(pairs.len());
    let mut found = false;
    for (k, v) in pairs {
        if compare::eq(k, args[0], ctx.atoms()) {
            out.push((k, args[1]));
            found = true;
        } else {
            out.push((k, v));
        }
    }
    if !found {
        return Err(structured_error(ctx, "badkey", &[args[0]])?);
    }
    build(ctx, &out)
}

/// `maps:remove/2` — a new map without `Key` (a no-op when it is absent).
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Map}` when `Map` is not a map (the live
/// reason term of the raise).
fn remove_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let pairs = map_pairs(ctx, args[1])?;
    let out: Vec<(Eterm, Eterm)> = pairs
        .into_iter()
        .filter(|&(k, _)| !compare::eq(k, args[0], ctx.atoms()))
        .collect();
    build(ctx, &out)
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
/// reason term of the raise).
fn take_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let pairs = map_pairs(ctx, args[1])?;
    let Some(index) = pairs
        .iter()
        .position(|&(k, _)| compare::eq(k, args[0], ctx.atoms()))
    else {
        return ctx.atom("error");
    };
    let value = pairs[index].1;
    let rest: Vec<(Eterm, Eterm)> = pairs
        .into_iter()
        .enumerate()
        .filter(|&(j, _)| j != index)
        .map(|(_, pair)| pair)
        .collect();
    let rest = build(ctx, &rest)?;
    ctx.tuple(&[value, rest])
}

/// `maps:from_keys/2` — a map with every key of the list `Keys` bound to `Value`.
///
/// A duplicate key collapses to one pair (`maps:from_keys([a,a], 2)` ⇒ `#{a => 2}`),
/// and the empty list gives `#{}`. The keys may be any terms, and two keys that are
/// *distinct* under `=:=` but equal under `==` stay separate
/// (`maps:from_keys([1,1.0], v)` ⇒ `#{1 => v, 1.0 => v}`) — both follow from
/// `write_map`'s exact-key, last-wins dedup.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `Keys` is not a proper list (the oracle's `badarg`).
fn from_keys_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let keys = proper_list(args[0]).ok_or_else(badarg)?;
    let pairs: Vec<(Eterm, Eterm)> = keys.into_iter().map(|k| (k, args[1])).collect();
    build(ctx, &pairs)
}

/// `maps:keys/1` — the map's keys, in stored order.
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Term}` when the argument is not a map.
fn keys_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let pairs = map_pairs(ctx, args[0])?;
    let keys: Vec<Eterm> = pairs.into_iter().map(|(k, _)| k).collect();
    ctx.list(&keys)
}

/// `maps:values/1` — the map's values, in stored order.
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Term}` when the argument is not a map.
fn values_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let pairs = map_pairs(ctx, args[0])?;
    let values: Vec<Eterm> = pairs.into_iter().map(|(_, v)| v).collect();
    ctx.list(&values)
}

/// `maps:size/1` and `erlang:map_size/1` — the number of pairs.
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Term}` when the argument is not a map.
fn size_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_map(args[0]) {
        return Err(structured_error(ctx, "badmap", &[args[0]])?);
    }
    let n = access::map_size(args[0])?;
    ctx.int(n as i64)
}

/// `maps:is_key/2` and `erlang:is_map_key/2` — whether `Key` is present.
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Map}` when `Map` is not a map (the live
/// reason term of the raise).
fn is_key_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let map = args[1];
    if !access::is_map(map) {
        return Err(structured_error(ctx, "badmap", &[map])?);
    }
    let present = map_lookup(ctx, map, args[0])?.is_some();
    ctx.boolean(present)
}

/// `maps:merge/2` — the union of two maps; `Map2`'s value wins on a shared key.
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Map}` naming whichever argument is not a map
/// (the live reason term of the raise).
fn merge_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let mut pairs = map_pairs(ctx, args[0])?;
    // Appending `Map2`'s pairs makes its value win on a shared key (last wins).
    pairs.extend(map_pairs(ctx, args[1])?);
    build(ctx, &pairs)
}

/// `maps:with/2` — the entries of `Map` whose key is in `Keys`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `Keys` is not a proper list (the oracle's atom
/// `badarg`); the oracle's structured `error:{badmap, Map}` when `Map` is not a map.
fn with_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let wanted = proper_list(args[0]).ok_or_else(badarg)?;
    let pairs = map_pairs(ctx, args[1])?;
    let mut out: Vec<(Eterm, Eterm)> = Vec::new();
    for (k, v) in pairs {
        if wanted.iter().any(|&w| compare::eq(w, k, ctx.atoms())) {
            out.push((k, v));
        }
    }
    build(ctx, &out)
}

/// `maps:without/2` — the entries of `Map` whose key is **not** in `Keys`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `Keys` is not a proper list (the oracle's atom
/// `badarg`); the oracle's structured `error:{badmap, Map}` when `Map` is not a map.
fn without_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let unwanted = proper_list(args[0]).ok_or_else(badarg)?;
    let pairs = map_pairs(ctx, args[1])?;
    let mut out: Vec<(Eterm, Eterm)> = Vec::new();
    for (k, v) in pairs {
        if !unwanted.iter().any(|&w| compare::eq(w, k, ctx.atoms())) {
            out.push((k, v));
        }
    }
    build(ctx, &out)
}

/// `maps:iterator/1` — the oracle's `[0 | Map]` improper-cons iterator.
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Map}` when the argument is not a map
/// (`maps.erl:983-984`); pinned by the oracle: `maps:iterator(1)` ⇒
/// `error:{badmap,1}`.
fn iterator_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let map = args[0];
    if !access::is_map(map) {
        return Err(structured_error(ctx, "badmap", &[map])?);
    }
    let path = ctx.int(0)?;
    ctx.cons(path, map)
}

/// `maps:next/1` — `{Key, Value, NextIter}`, or the atom `none` when exhausted.
///
/// Mirrors `maps.erl:1104-1117` / `erts_internal_map_next_3`:
///
/// * `{K, V, I}` ⇒ itself (the look-ahead chain is already built).
/// * `none` ⇒ `none`.
/// * `[Path | Map]`, `Map` a map and `Path` an integer or a list ⇒ the chain; see the
///   module docs for the path semantics and the documented chunking deviation.
/// * Anything else ⇒ `badarg`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a malformed iterator — a non-map tail, a
/// non-integer/non-list path, an out-of-range integer path, an ordered path that names a
/// key the map lacks, or an improper ordered path tail (each exactly the oracle's
/// `badarg`).
fn next_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let iter = args[0];
    if is_atom_named(ctx, iter, "none") {
        return ctx.atom("none");
    }
    if access::is_tuple(iter) && access::tuple_size(iter) == 3 {
        return Ok(iter);
    }
    // `[]` is not a cons, so it falls through to `badarg` — the oracle's answer for a
    // bare empty list (only `none` terminates).
    if !access::is_list(iter) {
        return Err(badarg());
    }
    let path = access::car(iter);
    let map = access::cdr(iter);
    if !access::is_map(map) {
        return Err(badarg());
    }
    if access::is_list(path) || access::is_nil(path) {
        // Ordered iterator (`maps:iterator(M, ordered)`): the path is the key list.
        return ordered_next(ctx, path, map);
    }
    if !access::is_small(path) {
        // A bignum path is a documented gap (the C decodes its digits as the trie
        // path); a negative small int is out of range below.
        return Err(badarg());
    }
    let start = signed_value(path);
    if start < 0 {
        return Err(badarg());
    }
    let start = start as usize;
    let pairs = access::map_pairs_storage_order(map)?;
    let n = pairs.len();
    if start > n {
        return Err(badarg());
    }
    if !access::is_hashmap(map) {
        // A flat map ignores the path beyond the bound check and emits the whole chain.
        let none = ctx.atom("none")?;
        return chain(ctx, &pairs, 0, n, none);
    }
    // A HAMT: chunk at `MAP_SMALL_MAP_LIMIT`, continuing at `[start + chunk | Map]`.
    let end = core::cmp::min(start + MAP_SMALL_MAP_LIMIT, n);
    let terminal = if end < n {
        let next_path = ctx.int(end as i64)?;
        ctx.cons(next_path, map)?
    } else {
        ctx.atom("none")?
    };
    chain(ctx, &pairs, start, end, terminal)
}

/// Builds `{k[i], v[i], …}` for `i` in `from..end` (ascending), ending in `terminal`.
fn chain(
    ctx: &mut BifContext<'_>,
    pairs: &[(Eterm, Eterm)],
    from: usize,
    end: usize,
    terminal: Eterm,
) -> Result<Eterm> {
    let mut result = terminal;
    for i in (from..end).rev() {
        result = ctx.tuple(&[pairs[i].0, pairs[i].1, result])?;
    }
    Ok(result)
}

/// The ordered-iterator walk (`erts_internal_map_next_3`'s list-path arm): the chain
/// is `{K, maps:get(K, Map), …}` for every key of `keys`, terminating in `none`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when a key of `keys` is absent from `map`, or when
/// `keys` has an improper tail (the C's `ordered_badarg`).
fn ordered_next(ctx: &mut BifContext<'_>, keys_list: Eterm, map: Eterm) -> Result<Eterm> {
    let mut keys: Vec<Eterm> = Vec::new();
    let mut cur = keys_list;
    loop {
        if access::is_nil(cur) {
            break;
        }
        if !access::is_list(cur) {
            // An improper path tail: the C jumps to `ordered_badarg`.
            return Err(badarg());
        }
        keys.push(access::car(cur));
        cur = access::cdr(cur);
    }
    let mut values: Vec<Eterm> = Vec::with_capacity(keys.len());
    for &key in &keys {
        match map_lookup(ctx, map, key)? {
            Some(value) => values.push(value),
            None => return Err(badarg()),
        }
    }
    let none = ctx.atom("none")?;
    let mut result = none;
    for i in (0..keys.len()).rev() {
        result = ctx.tuple(&[keys[i], values[i], result])?;
    }
    Ok(result)
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
fn map_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let pairs = map_pairs(ctx, args[1])?;
    let mut out: Vec<(Eterm, Eterm)> = Vec::with_capacity(pairs.len());
    for (key, value) in pairs {
        let mapped = ctx.call_fun(fun, &[key, value])?;
        out.push((key, mapped));
    }
    build(ctx, &out)
}

/// `maps:fold(Fun, Init, Map)` — `Acc = Fun(Key, Value, Acc)` over every pair.
fn fold_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let mut acc = args[1];
    let pairs = map_pairs(ctx, args[2])?;
    for (key, value) in pairs {
        acc = ctx.call_fun(fun, &[key, value, acc])?;
    }
    Ok(acc)
}

/// `maps:filter(Pred, Map)` — the `{Key, Value}` pairs whose predicate is `true`.
fn filter_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let pairs = map_pairs(ctx, args[1])?;
    let mut out: Vec<(Eterm, Eterm)> = Vec::new();
    for (key, value) in pairs {
        let result = ctx.call_fun(fun, &[key, value])?;
        if predicate_holds(ctx, result)? {
            out.push((key, value));
        }
    }
    build(ctx, &out)
}

/// `maps:foreach(Fun, Map)` — `Fun(Key, Value)` for its side effects; `ok`.
fn foreach_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let pairs = map_pairs(ctx, args[1])?;
    for (key, value) in pairs {
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
