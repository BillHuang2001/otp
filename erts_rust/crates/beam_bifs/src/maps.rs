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
//! `map/2`, `fold/3`, `filter/2` and `foreach/2` are stdlib `maps.erl` functions
//! (`bif.tab` declares none of them), so they are registered here as fast paths
//! mirroring that module's clause structure. They call their fun through
//! [`BifContext::call_fun`] (the [`crate::FunCaller`] seam); with no caller
//! installed they raise `unsupported_bif`, and a fun's own failures propagate
//! verbatim.
//!
//! Their second/third argument is a **map _or_ a `maps` iterator** (`MapOrIter` in
//! `maps.erl`'s specs, `:836-846`, `:887-897`, `:932-944`): a map is walked in its
//! **stored** order (`map_pairs_storage_order`), and any other value is walked as an
//! iterator exactly as `maps:next/1` would. Both routes are the one
//! [`walk_map_or_iter`], so `maps:fold(F, 0, maps:iterator(M, ordered))` —
//! `beam_ssa_recv:plan_clears/2`'s shape — traverses exactly what `M` itself does.
//!
//! * `map(Fun, MapOrIter)` calls `Fun(Key, Value)` and stores the result — the keys
//!   are unchanged, the values are the fun's results, in the map's stored order.
//! * `fold(Fun, Init, MapOrIter)` is `Acc = Fun(Key, Value, Acc)`;
//!   `filter(Pred, MapOrIter)` keeps the pairs whose predicate returns `true`;
//!   `foreach(Fun, MapOrIter)` is called for its side effects and returns `ok`.
//! * A predicate result that is not a `true`/`false` atom is the oracle's
//!   `error:{case_clause, Value}` (see the structured-reason section below).
//! * The fun for pair *i* runs **before** the `next/1` that yields pair *i+1*
//!   (`maps.erl:948`'s `[{K, Fun(K, V)} | map_1(Fun, try_next(Iter, Tag), Tag)]`), so
//!   the fun sees the pairs before an abandoned walk and a `Fun` that raises aborts at
//!   the oracle's own pair.
//! * The atom `none` is a valid `MapOrIter` and simply holds no pair (`fold` answers
//!   `Init`, `map`/`filter` `#{}`, `foreach` `ok`); `[]` is not (`error:{badmap, []}`).
//!
//! Every one of them first applies the oracle's `is_function(Fun, Arity)`
//! clause-head guard ([`require_fun`]). `maps.erl`'s fallback clause raises the
//! **plain atom** `badarg` — not the `function_clause` `lists.erl`'s guards raise —
//! and the guard fires before the map argument is inspected or walked, so a non-fun
//! (or a fun of the wrong arity) is `badarg` even for an empty map or a non-map
//! second argument. A **valid** fun paired with an argument that is neither a map nor
//! a walkable iterator is instead the structured `error:{badmap, Term}` from
//! [`walk_map_or_iter`], naming the original argument.
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
//! `{Key, Value, NextIter}`. `maps:to_list/1` and the four `fun`-taking BIFs all
//! accept **either** a map (its stored order) **or** an iterator (the walk) through
//! the one [`walk_map_or_iter`], and report a bad iterator as the structured
//! `error:{badmap, Iter}` naming the **original** argument (`maps.erl:531-543`,
//! `:1326-1335`). Oracle-pinned (OTP 29 / erts-17.1):
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

use beam_common::{BeamError, Result};
use beam_term::prelude::signed_value;
use beam_term::{atom_val, compare, Eterm};

use crate::access;
use crate::context::BifContext;
use crate::error::{badarg, function_clause, structured_error};
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

/// Walks a **map _or_ a `maps` iterator** argument — the single traversal shared by
/// the `fun`-taking BIFs and `maps:to_list/1`.
///
/// A bare map is walked in its **stored** order
/// ([`crate::access::map_pairs_storage_order`]); every other `arg` is walked as an
/// iterator through [`next_1`], exactly as `maps.erl` walks it (the `iter` clauses of
/// `map/2`/`fold/3`/`filter/2`/`foreach/2`, `maps.erl:836-846`, `:887-897`,
/// `:932-944`, and `to_list/1`'s own catch, `:531-543`). `step(key, value)` is called
/// for the look-ahead pair **before** the `next_1` that yields the following pair —
/// `maps.erl:948`'s `[{K, Fun(K, V)} | map_1(Fun, try_next(Iter, Tag), Tag)]` — so a
/// walk cut short by a bad iterator has already applied `step` to the pairs before the
/// failure, and a `step` failure aborts at the oracle's own pair. The walk terminates
/// at the atom `none`.
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Arg}` — naming the **original** argument,
/// never the sub-iterator the failing `next/1` ran on (`maps.erl:1332-1335`'s
/// `try_next/2` re-raised by the clause's `catch error:ErrorTag`) — for every way the
/// argument fails to be a map or a walkable iterator: a `next_1` failure, or a
/// look-ahead that is neither a `{Key, Value, Iter}` tuple nor `none`. `[]` is *not*
/// an iterator (only `none` terminates). A failure of `step` itself (the fun's own
/// error, or `filter/2`'s `{case_clause, Value}`) is propagated **unchanged**.
fn walk_map_or_iter<S>(ctx: &mut BifContext<'_>, arg: Eterm, mut step: S) -> Result<()>
where
    S: FnMut(&mut BifContext<'_>, Eterm, Eterm) -> Result<()>,
{
    if access::is_map(arg) {
        for (key, value) in access::map_pairs_storage_order(arg)? {
            step(ctx, key, value)?;
        }
        return Ok(());
    }
    let mut cur = match next_1(ctx, &[arg]) {
        Ok(cur) => cur,
        Err(_) => return Err(structured_error(ctx, "badmap", &[arg])?),
    };
    loop {
        if is_atom_named(ctx, cur, "none") {
            return Ok(());
        }
        if !access::is_tuple(cur) || access::tuple_size(cur) != 3 {
            return Err(structured_error(ctx, "badmap", &[arg])?);
        }
        let next_iter = access::tuple_element(cur, 2);
        step(ctx, access::tuple_element(cur, 0), access::tuple_element(cur, 1))?;
        cur = match next_1(ctx, &[next_iter]) {
            Ok(cur) => cur,
            Err(_) => return Err(structured_error(ctx, "badmap", &[arg])?),
        };
    }
}

/// Erlang's `is_list/1`: a cons cell **or** `NIL`.
///
/// The raw [`crate::access::is_list`] mirrors erts' cons-only `is_list`, so the guard
/// of `maps.erl`'s clause heads (`is_list(Ks)`) needs the extra `NIL` arm — Erlang's
/// `is_list([])` is `true` while `is_list([a|b])` is `true` too (any cons cell).
fn is_erl_list(x: Eterm) -> bool {
    access::is_list(x) || access::is_nil(x)
}

/// The oracle's `error_type(M)` (`maps.erl:1288-1289`): a **map** argument yields the
/// plain atom [`badarg`], anything else the structured `{badmap, M}`.
///
/// This is the reason `with/2`/`without/2`'s single fallback clause
/// (`error_with_info(error_type(M), [Ks, M])`) raises once its `is_list(Ks), is_map(M)`
/// guard has failed, so the **map** argument — and only it — decides the class there.
///
/// # Errors
///
/// Propagates a heap-allocation failure while building the `{badmap, M}` tuple; an
/// uninternable `badmap` tag degrades to [`badarg`] inside [`structured_error`].
fn error_type(ctx: &mut BifContext<'_>, map: Eterm) -> Result<BeamError> {
    if access::is_map(map) {
        Ok(badarg())
    } else {
        structured_error(ctx, "badmap", &[map])
    }
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
/// Both routes are [`walk_map_or_iter`], so a map and its own `maps:iterator/1` yield
/// the same list.
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
    let mut acc: Vec<Eterm> = Vec::new();
    walk_map_or_iter(ctx, args[0], |ctx, key, value| {
        let pair = ctx.tuple(&[key, value])?;
        acc.push(pair);
        Ok(())
    })?;
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
        None => ctx.intern_atom("error"),
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
        return ctx.intern_atom("error");
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
/// Mirrors `maps.erl:1167-1177`: the clause head is
/// `with(Ks, Map1) when is_list(Ks), is_map(Map1)` and the module's single fallback
/// clause is `error_with_info(error_type(M), [Ks, M])`. The two arguments are therefore
/// classified **together**, and once either fails that guard the *map* argument alone
/// decides the class:
///
/// * a **non-map** `Map` ⇒ the structured `error:{badmap, Map}` **whatever** `Keys` is
///   — checked before `Keys` is inspected at all;
/// * a **non-list** `Keys` with a map `Map` ⇒ the plain atom `error:badarg`
///   (`error_type/1`'s `is_map` arm);
/// * `Keys` a list (cons **or** `[]`) with a map `Map` ⇒ clause 1 runs. An **improper**
///   `Keys` tail reaches `with_1/2`'s last clause and raises the bare
///   `error:function_clause`; a proper `Keys` filters (a key absent from `Map` is
///   simply ignored).
///
/// Oracle-recorded (OTP 29.1.1 / erts-17.1; every probe wrapped in
/// `try F() catch C:R -> {C,R} end`):
///
/// ```text
/// maps:with([a], y)         => {error,{badmap,y}}
/// maps:with(y, #{a=>1})     => {error,badarg}
/// maps:with(1, y)           => {error,{badmap,y}}
/// maps:with({a,b}, y)       => {error,{badmap,y}}
/// maps:with([a|b], y)       => {error,{badmap,y}}
/// maps:with(y, z)           => {error,{badmap,z}}
/// maps:with(1, 2)           => {error,{badmap,2}}
/// maps:with([a|b], 2)       => {error,{badmap,2}}
/// maps:with([], y)          => {error,{badmap,y}}
/// maps:with(1, #{a=>1})     => {error,badarg}
/// maps:with({a}, #{a=>1})   => {error,badarg}
/// maps:with([a|b], #{a=>1}) => {error,function_clause}
/// maps:with([a,b], #{a=>1}) => #{a => 1}
/// ```
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Map}` (live reason term `{badmap, Map}`) for
/// a non-map `Map`; the bare atom `error:badarg` for a non-list `Keys`; the bare atom
/// `error:function_clause` for an improper `Keys` (both with a map `Map`).
fn with_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (keys, map) = (args[0], args[1]);
    if !is_erl_list(keys) || !access::is_map(map) {
        // The clause-1 guard failed: `error_type(M)` decides (a map ⇒ `badarg`, else
        // `{badmap, M}`). `Keys` is deliberately never inspected here.
        return Err(error_type(ctx, map)?);
    }
    // The guard held, so `with_1(Ks, Map)` is entered; only an improper tail fails.
    let wanted = proper_list(keys).ok_or_else(function_clause)?;
    let pairs = map_pairs(ctx, map)?;
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
/// Same clause structure as [`with_2`] (`maps.erl:1141-1144`), so the argument
/// classification is identical — a non-map `Map` ⇒ `error:{badmap, Map}` first, a
/// non-list `Keys` with a map ⇒ `error:badarg`, and an improper `Keys` with a map ⇒ the
/// bare `error:function_clause` (`lists:foldl/3` reaches its own last clause on the
/// improper tail).
///
/// Oracle-recorded (OTP 29.1.1 / erts-17.1):
///
/// ```text
/// maps:without([a], y)         => {error,{badmap,y}}
/// maps:without(y, #{a=>1})     => {error,badarg}
/// maps:without(1, y)           => {error,{badmap,y}}
/// maps:without({a}, y)         => {error,{badmap,y}}
/// maps:without([a|b], y)       => {error,{badmap,y}}
/// maps:without(y, z)           => {error,{badmap,z}}
/// maps:without(1, 2)           => {error,{badmap,2}}
/// maps:without([a|b], 2)       => {error,{badmap,2}}
/// maps:without([], y)          => {error,{badmap,y}}
/// maps:without({a}, #{a=>1})   => {error,badarg}
/// maps:without([a|b], #{a=>1}) => {error,function_clause}
/// maps:without([a], #{a=>1})   => #{}
/// ```
///
/// # Errors
///
/// The oracle's structured `error:{badmap, Map}` (live reason term `{badmap, Map}`) for
/// a non-map `Map`; the bare atom `error:badarg` for a non-list `Keys`; the bare atom
/// `error:function_clause` for an improper `Keys` (both with a map `Map`).
fn without_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (keys, map) = (args[0], args[1]);
    if !is_erl_list(keys) || !access::is_map(map) {
        // As in `with_2`: the fallback clause classifies by the *map* argument only.
        return Err(error_type(ctx, map)?);
    }
    let unwanted = proper_list(keys).ok_or_else(function_clause)?;
    let pairs = map_pairs(ctx, map)?;
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
        return ctx.intern_atom("none");
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
        let none = ctx.intern_atom("none")?;
        return chain(ctx, &pairs, 0, n, none);
    }
    // A HAMT: chunk at `MAP_SMALL_MAP_LIMIT`, continuing at `[start + chunk | Map]`.
    let end = core::cmp::min(start + MAP_SMALL_MAP_LIMIT, n);
    let terminal = if end < n {
        let next_path = ctx.int(end as i64)?;
        ctx.cons(next_path, map)?
    } else {
        ctx.intern_atom("none")?
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
    let none = ctx.intern_atom("none")?;
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

/// The oracle's `is_function(Fun, Arity)` clause-head guard for the `fun`-taking
/// `maps` BIFs.
///
/// `maps.erl`'s fallback clause raises the **plain atom** `error:badarg` — not the
/// `function_clause` the `lists.erl` guards raise — for a non-fun or a fun of the
/// wrong arity (`map/2` at `maps.erl:943`, `foreach/2` at `:845`, `fold/3` at
/// `:896`, `filter/2` at `:739`). The guard sits on the *first* clause head, so it
/// fires **before** the map argument is inspected or the map is walked: a bad fun
/// is `badarg` even for an empty map or a non-map second argument, and the map is
/// never a fabricated success. Oracle-pinned (OTP 29 / erts-17.1):
/// `maps:map(5, #{})`, `maps:map(fun(_K) -> ok end, #{})` and
/// `maps:map(5, not_a_map)` ⇒ `error:badarg`, while
/// `maps:map(fun(_,_) -> ok end, not_a_map)` ⇒ `error:{badmap,not_a_map}` (the
/// guard passes first, then the map is not a map).
///
/// Not shared with `lists.rs`'s `require_fun`: that item is private (`fn`, not
/// `pub(crate)`) *and* it raises the wrong reason (`error:function_clause`) for
/// `maps`, so promoting it would not produce the oracle's `badarg`.
fn require_fun(fun: Eterm, arity: u32) -> Result<()> {
    match access::fun_arity(fun) {
        Some(n) if n == arity => Ok(()),
        _ => Err(badarg()),
    }
}

/// `maps:map(Fun, MapOrIter)` — `Fun(Key, Value)` supplies each value.
///
/// The keys and their order are unchanged; only the values are the fun's results.
///
/// # Errors
///
/// The plain `error:badarg` for a bad `Fun` ([`require_fun`]), the structured
/// `error:{badmap, MapOrIter}` for an argument that is neither a map nor a walkable
/// iterator, or whatever `Fun` raises.
fn map_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    // `map(Fun, Map) when is_map(Map), is_function(Fun, 2)` … else `badarg`, then the
    // `map(Fun, Iter)` clause walks a non-map through `try_next/2`.
    require_fun(fun, 2)?;
    let mut out: Vec<(Eterm, Eterm)> = Vec::new();
    walk_map_or_iter(ctx, args[1], |ctx, key, value| {
        let mapped = ctx.call_fun(fun, &[key, value])?;
        out.push((key, mapped));
        Ok(())
    })?;
    build(ctx, &out)
}

/// `maps:fold(Fun, Init, MapOrIter)` — `Acc = Fun(Key, Value, Acc)` over every pair.
///
/// # Errors
///
/// The plain `error:badarg` for a bad `Fun` ([`require_fun`]) — checked before
/// `Init`/`MapOrIter` are looked at — the structured `error:{badmap, MapOrIter}` for
/// an argument that is neither a map nor a walkable iterator, or whatever `Fun`
/// raises.
fn fold_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    // `fold(Fun, Init, Map) when is_map(Map), is_function(Fun, 3)` … else `badarg`,
    // then the `fold(Fun, Init, Iter)` clause walks a non-map through `try_next/2`.
    require_fun(fun, 3)?;
    let mut acc = args[1];
    walk_map_or_iter(ctx, args[2], |ctx, key, value| {
        acc = ctx.call_fun(fun, &[key, value, acc])?;
        Ok(())
    })?;
    Ok(acc)
}

/// `maps:filter(Pred, MapOrIter)` — the `{Key, Value}` pairs whose predicate is `true`.
///
/// # Errors
///
/// The plain `error:badarg` for a bad `Pred` ([`require_fun`]), the structured
/// `error:{badmap, MapOrIter}` for an argument that is neither a map nor a walkable
/// iterator, the structured `error:{case_clause, Value}` for a non-boolean predicate
/// result, or whatever `Pred` raises.
fn filter_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    // `filter(Pred, Map) when is_map(Map), is_function(Pred, 2)` … else `badarg`,
    // then the `filter(Pred, Iter)` clause walks a non-map through `try_next/2`.
    require_fun(fun, 2)?;
    let mut out: Vec<(Eterm, Eterm)> = Vec::new();
    walk_map_or_iter(ctx, args[1], |ctx, key, value| {
        let result = ctx.call_fun(fun, &[key, value])?;
        if predicate_holds(ctx, result)? {
            out.push((key, value));
        }
        Ok(())
    })?;
    build(ctx, &out)
}

/// `maps:foreach(Fun, MapOrIter)` — `Fun(Key, Value)` for its side effects; `ok`.
///
/// # Errors
///
/// The plain `error:badarg` for a bad `Fun` ([`require_fun`]), the structured
/// `error:{badmap, MapOrIter}` for an argument that is neither a map nor a walkable
/// iterator, or whatever `Fun` raises.
fn foreach_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    // `foreach(Fun, Map) when is_map(Map), is_function(Fun, 2)` … else `badarg`, then
    // the `foreach(Fun, Iter)` clause walks a non-map through `try_next/2`.
    require_fun(fun, 2)?;
    walk_map_or_iter(ctx, args[1], |ctx, key, value| {
        ctx.call_fun(fun, &[key, value])?;
        Ok(())
    })?;
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
