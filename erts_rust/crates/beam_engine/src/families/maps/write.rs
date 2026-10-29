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

//! Map update instructions — the `put_map_assoc` / `put_map_exact` side.
//!
//! Owns:
//! - `put_map_assoc` ([`beam_loader::Instr::PutMapAssoc`]) — build a **new** map from
//!   the source, inserting or overwriting each `(key, value)` pair;
//! - `put_map_exact` ([`beam_loader::Instr::PutMapExact`]) — as `put_map_assoc`, but
//!   every key must already exist.
//!
//! Maps are immutable, so the source is never mutated: the update produces a fresh
//! map. Keys are matched with `beam_term::cmp`, so `1` and `1.0` stay distinct keys.
//! The `Live` operand (the C collector's GC-root count) is unused at M1: every
//! reservation is made through the rooted seam with
//! [`RootSet::ALL_LIVE`](crate::context::RootSet::ALL_LIVE), so no operand-derived
//! live count is needed yet.
//!
//! # Two arms, chosen by the source's own layout
//!
//! * **Hash map (HAMT)** — the **path-copying** writer,
//!   [`beam_term::map::map_put_cow`] (via [`crate::terms::map_cow_rooted`] and its
//!   [`CowMapRegion`](crate::terms::CowMapRegion)): one put copies only the nodes on
//!   the key's hash path and every off-path subtree is shared **by word**, so an
//!   update costs Θ(log n) words instead of Θ(n) — the C's `erts_maps_put`
//!   (`erl_map.c:2105-2235`). The instruction reserves
//!   [`crate::terms::map_put_cow_capacity`]`(size)` words **per pair** and threads the
//!   live map through the puts. The source is **not enumerated at all**: each update
//!   descends the key's path, so no pair list is ever materialized.
//! * **Flat map** (≤ `MAP_SMALL_MAP_LIMIT` = 32 distinct keys) — the **rebuild**:
//!   merge into a fresh pair list and build it through [`crate::terms::make_map`],
//!   which delegates the layout choice to `beam_term::map::write_map` (flat at/below
//!   the limit, a HAMT above it). The flat arm keeps the rebuild deliberately: its
//!   source is at most 32 pairs, whose whole-map copy (`70` words) is below one hash
//!   path's bound (`293`), the merge is what performs the 33rd key's conversion to a
//!   hash map exactly as `write_map` does, and its no-change pre-walk
//!   ([`unchanged_after_update`]) prices the no-op at zero words.
//!
//! An empty pair list is the source map itself on **either** arm, and is answered
//! with **no reservation at all**.
//!
//! # Sharing, lifetime and the `hp` precondition
//!
//! The writer's `# Safety` contract asks for an `hp` that outlives the result and is
//! **not** the source map's own storage. Here that holds trivially: `hp` is the
//! process heap's allocation (`crate::terms::reserve`) and the source map lives in
//! the same heap's *earlier* bump region — two disjoint regions of one arena, and the
//! arena outlives every term it hands out. Sharing the source's off-path subtree
//! words is sound because terms are immutable and are never rewritten, relocated or
//! freed in place; no word is ever written in place **into** the source, so the
//! source map is bit-for-bit unchanged after the update (asserted by the family
//! tests).
//!
//! # The no-change answer, per arm (term identity)
//!
//! An update that changes nothing answers the **source map itself**, so
//! `erts_debug:same(M#{a => 1}, M)` is `true` when `M` already holds `a => 1` — the C's
//! `if (*old_vals != *hp) changed_values = 1;` … `return map;`
//! (`beam_common.c:2097,2142`, the flatmap route) and `if (CDR(ptr) == value) { *sz = 0
//! … }` ⇒ `res = map` (`erl_map.c:2493,2523-2526`, the HAMT route, which
//! `erts_gc_update_map_assoc`'s `is_not_flatmap` arm reaches).
//!
//! * **HAMT arm** — the writer's own zero-write arm *is* the no-change answer:
//!   `map_put_cow` returns its input map word wherever the stored value word already
//!   equals `value`, so a chain of such puts answers the source word having written
//!   **nothing** (the region's slots stay untouched). That is why the HAMT arm needs no
//!   pre-walk — it costs one reservation and writes nothing.
//! * **Flat arm** — [`unchanged_after_update`] decides it up front so the rebuild is
//!   never entered: one pure read, no reservation, and the source word is still live
//!   (see its docs). A pure read allocates nothing at all, which is what keeps
//!   `put_map_assoc_no_change_allocates_nothing` true for the flat layout.
//!
//! The decision is **`Eterm` word equality**, never `=:=`: the C compares raw words
//! (`*old_vals != *hp`, `CDR(ptr) == value`), so two numerically equal but separately
//! allocated bignums are a *change*. Keys are matched with the map's own key rule
//! (`beam_term::map::map_get`, i.e. `EQ`/`cmp == eq`), so a new key always changes the
//! map. One shape deliberately declines the flat pre-walk: an operand that is a
//! **non-small `Operand::Integer`** is materialized on the BEAM heap by its read
//! (`operand_slack_words`), so its word can never equal a stored one (and the read is an
//! allocating door while a raw map word is held). Such an update rebuilds the map —
//! correct contents, identity lost — which is also what the oracle answers on its
//! `-eval` route for the same reason.

use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Label, Module, Operand};
use beam_term::Eterm;

use crate::context::{ExecutionContext, RootSet};
use crate::error;
use crate::machine::Machine;
use crate::operand::{operand_slack_words, read_operand, read_operand_slack, write_operand};
use crate::predicates;
use crate::terms;
use crate::Step;

use super::{fail_or_raise, position};

/// The key count of the map `map` — O(1), from the map head node's own size word
/// (`beam_term::map::map_size`, the C `erts_map_size`), never a walk of the pair
/// list.
///
/// This is the *capacity* the rebuild arm reserves: the builder deduplicates, so the
/// produced map holds at most `key count + new pairs`. The C update sizes its
/// reservation the same way, from the header's size word alone — `num_old =
/// flatmap_get_size(old_mp)` then `need = 2*(num_old+num_updates) + 1 + …`
/// (`beam_common.c:2002,2017`). It is also the `size` the path-copying arm's
/// per-put capacity figure is derived from. The namespace argument of the accessor
/// is unused for counting; it is taken from the host so the count is read in the
/// same atom namespace as every other map operation.
fn key_count(ctx: &dyn ExecutionContext, module: &Module, map: Eterm) -> Result<usize> {
    Ok(beam_term::map::map_size(map, ctx.atoms(module))?)
}

/// Whether the source's own layout puts the update on the **path-copying** arm.
///
/// A hash map of more than `MAP_SMALL_MAP_LIMIT` keys is the shape every crate
/// builder produces above the flat limit, and the one the writer's size-only
/// capacity bound (`293` words) is documented to be sound for. A hash map at or
/// below the limit cannot come from those builders (they write a *flat* map), and
/// the capacity figure would then be the *flat* one — an under-reservation of a
/// node spine — so such a source keeps the rebuild arm rather than risking a write
/// past the reservation.
fn takes_cow_arm(map: Eterm, size: usize) -> bool {
    terms::is_hashmap(map) && size > beam_term::map::MAP_SMALL_MAP_LIMIT
}

/// The spacer words the reserve must add for the `Eterm` operands a map update
/// re-reads after the reserve: the source operand plus every key/value operand
/// (each a bignum only when it is a non-small integer immediate).
fn merge_slack_words(
    source: &Operand,
    pairs: &[(Operand, Operand)],
) -> usize {
    operand_slack_words(source)
        + pairs
            .iter()
            .map(|(k, v)| operand_slack_words(k) + operand_slack_words(v))
            .sum::<usize>()
}

/// Whether `pairs` would leave `map` **unchanged**, i.e. every pair replaces an
/// existing key with the identical value *word* — the C update's "nothing changed"
/// fast path, whose answer is the source map itself (see the module docs).
///
/// Allocates **nothing** (no map reservation, no materialized operand, and not even a
/// pair-list `Vec`: each key is looked up with [`beam_term::map::map_get`], which is a
/// pure read of whichever frozen layout `map` has), so no collection can run and
/// `map` — a raw word from before this call — cannot have moved. That is what makes
/// writing it to `dst` sound under a collecting host.
///
/// The value test is **word equality** of the term (`Eterm`'s own `==`), never `=:=`:
/// the C compares `*old_vals != *hp` (`beam_common.c:2097`, flatmap) and
/// `CDR(ptr) == value` (`erl_map.c:2523`, HAMT), so two numerically equal but separately
/// allocated bignums are a *change*. The key test is the map's own key rule, so `1` and
/// `1.0` stay distinct keys, and a key that is absent is a change (a new key always
/// yields a new map).
///
/// An operand that would be **materialized** on the BEAM heap by its read (a non-small
/// `Operand::Integer`, see [`operand_slack_words`]) declines the fast path up front:
/// its fresh word can never equal a stored one, and reading it is an allocating door
/// while a raw map word is held.
fn unchanged_after_update(
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
    module: &Module,
    map: Eterm,
    pairs: &[(Operand, Operand)],
) -> Result<bool> {
    if pairs
        .iter()
        .any(|(key, value)| operand_slack_words(key) != 0 || operand_slack_words(value) != 0)
    {
        return Ok(false);
    }
    for (key_operand, value_operand) in pairs {
        let key = read_operand(machine, ctx, key_operand)?;
        // The namespace is taken AFTER the operand read (it borrows `*ctx` immutably,
        // so it must not be held across `read_operand`'s `&mut`), and it is the same
        // one the merge below rebuilds with.
        let stored = beam_term::map::map_get(map, key, ctx.atoms(module))?;
        let Some(stored) = stored else {
            return Ok(false); // an absent key is a change, never a no-op
        };
        if read_operand(machine, ctx, value_operand)? != stored {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The **path-copying** arm shared by both update instructions: ONE rooted
/// reservation of [`crate::terms::map_put_cow_capacity`]`(size) * pairs.len()` words
/// and then one chained put per pair, each writing into its own slot of the
/// reservation ([`crate::terms::map_cow_rooted`]).
///
/// `size` is the source's key count — read *before* the reserve, from the head node's
/// size word, and **not** re-read after it: the figure sizes the reservation, and the
/// writer's capacity bound is documented as sound for every map it accepts, so an
/// over-estimate costs address space while an under-estimate would corrupt. Nothing
/// in the closure may change it (the source is immutable).
///
/// `require_present` is `put_map_exact`'s exact-key re-check: `map_put_cow` has
/// *assoc* semantics (it inserts an absent key), so the key's presence is re-tested
/// on the live map immediately before each put — the interleaved state, which is
/// what makes the check meaningful. That can only answer "absent" if a key the
/// pre-reserve validation accepted vanished across a reserve that changes nothing;
/// it is the same typed `unsupported` today's merge raises, never a silent insert.
fn cow_update(
    machine: &mut Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    source: &Operand,
    pairs: &[(Operand, Operand)],
    size: usize,
    require_present: bool,
) -> Result<Eterm> {
    let per = terms::map_put_cow_capacity(size);
    let words = per.checked_mul(pairs.len()).ok_or_else(|| {
        error::unsupported("map update: the reservation overflows a word count")
    })?;
    let extra = merge_slack_words(source, pairs);
    terms::map_cow_rooted(
        machine,
        module,
        ctx,
        RootSet::ALL_LIVE,
        words,
        per,
        extra,
        |m, region, slack, atoms| {
            // Phase B: re-read the source (and every pair) from the machine's
            // post-collection state, then thread the live map through the puts.
            let mut map = read_operand_slack(m, source, slack)?;
            for (index, (key_operand, value_operand)) in pairs.iter().enumerate() {
                let key = read_operand_slack(m, key_operand, slack)?;
                let value = read_operand_slack(m, value_operand, slack)?;
                if require_present && beam_term::map::map_get(map, key, atoms)?.is_none() {
                    return Err(error::unsupported(
                        "put_map_exact: key vanished across the reserve",
                    ));
                }
                map = region.put(index, map, key, value, atoms)?;
            }
            Ok(map)
        },
    )
}

/// `put_map_assoc Fail Src Dst Live {Key,Value}*` — build a new map with each pair
/// associated, then write it to `Dst`.
pub(super) fn put_map_assoc(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    ctx: &mut dyn ExecutionContext,
    fail: Label,
    source: &Operand,
    dst: &Operand,
    live: u32,
    pairs: &[(Operand, Operand)],
) -> Result<Step> {
    let _ = live;
    let map = read_operand(machine, ctx, source)?;
    if !predicates::is_map(map) {
        return fail_or_raise(machine, module, labels, ctx, fail, "badmap", source);
    }
    // No pairs: the update *is* the source map, and no reservation is made for it.
    if pairs.is_empty() {
        write_operand(machine, dst, map)?;
        return Ok(Step::Next);
    }
    // Phase A on the path-copying arm: nothing but the O(1) size word, then one
    // reservation and one chained put per pair. The no-change answer is the writer's
    // own zero-write arm (see the module docs), so no pre-walk is needed.
    let size = key_count(ctx, module, map)?;
    if takes_cow_arm(map, size) {
        let result = cow_update(machine, module, ctx, source, pairs, size, false)?;
        write_operand(machine, dst, result)?;
        return Ok(Step::Next);
    }
    // The flat (rebuild) arm: Phase A (nothing retained past the reserve) is the
    // no-change fast path — every pair a replacement with the identical value word, so
    // the source map *is* the update's result. Pure read, no reservation (see
    // [`unchanged_after_update`]), so the source word is still live.
    if unchanged_after_update(machine, ctx, module, map, pairs)? {
        write_operand(machine, dst, map)?;
        return Ok(Step::Next);
    }
    let capacity = size + pairs.len();
    // Phase B: reserve the whole map block (one rooted allocation a moving collector
    // may run under), then re-read the source and every key/value operand from the
    // post-collection state and merge into a pair list that is only ever read.
    let extra = merge_slack_words(source, pairs);
    let result = terms::make_map_rooted(
        machine,
        module,
        ctx,
        RootSet::ALL_LIVE,
        capacity,
        extra,
        |m, slack, atoms| {
            let map = read_operand_slack(m, source, slack)?;
            let mut merged = terms::map_entries(map)?;
            for (key_operand, value_operand) in pairs {
                let key = read_operand_slack(m, key_operand, slack)?;
                let value = read_operand_slack(m, value_operand, slack)?;
                match position(&merged, key, atoms) {
                    Some(index) => merged[index].1 = value,
                    None => merged.push((key, value)),
                }
            }
            Ok(merged)
        },
    )?;
    write_operand(machine, dst, result)?;
    Ok(Step::Next)
}

/// `put_map_exact Fail Src Dst Live {Key,Value}*` — as `put_map_assoc`, but an
/// absent key fails.
///
/// The no-change answer is the same per arm: `if (*hp != *old_vals) changed = 1;` …
/// `p->htop = old_hp; return map;` (`beam_common.c:2309,2325-2326`) on the rebuild arm,
/// the writer's own zero-write arm on the path-copying one.
pub(super) fn put_map_exact(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    ctx: &mut dyn ExecutionContext,
    fail: Label,
    source: &Operand,
    dst: &Operand,
    live: u32,
    pairs: &[(Operand, Operand)],
) -> Result<Step> {
    let _ = live;
    let map = read_operand(machine, ctx, source)?;
    if !predicates::is_map(map) {
        return fail_or_raise(machine, module, labels, ctx, fail, "badmap", source);
    }
    // No pairs: the update *is* the source map, with no reservation and — unlike the
    // assoc form — nothing to validate.
    if pairs.is_empty() {
        write_operand(machine, dst, map)?;
        return Ok(Step::Next);
    }
    let size = key_count(ctx, module, map)?;
    if takes_cow_arm(map, size) {
        // Phase A on the path-copying arm: the `{badkey,Key}` validation, decided
        // BEFORE a single word is written. Each key is looked up with
        // `beam_term::map::map_get` — a hashed descent of the (immutable) source, not a
        // materialized pair list — so the failure path writes nothing at all and the
        // source map's word and contents are untouched.
        //
        // Rooting order (the reads' rule): the key read is the iteration's one
        // **allocating** read (a non-small `Operand::Integer` is materialized as a
        // bignum — `operand_slack_words`), and one allocation is one possible
        // collection, so the source word is read **last**, from the machine's
        // post-collection state. The word read before the loop is dead from then on.
        for (key_operand, _) in pairs {
            let key = read_operand(machine, ctx, key_operand)?;
            let map = read_operand(machine, ctx, source)?;
            // The namespace is taken AFTER both operand reads (it borrows `*ctx`
            // immutably, so it must not be held across `read_operand`'s `&mut`).
            if beam_term::map::map_get(map, key, ctx.atoms(module))?.is_none() {
                return fail_or_raise(machine, module, labels, ctx, fail, "badkey", key_operand);
            }
        }
        let result = cow_update(machine, module, ctx, source, pairs, size, true)?;
        write_operand(machine, dst, result)?;
        return Ok(Step::Next);
    }
    // The flat (rebuild) arm. The no-change fast path comes BEFORE the `{badkey,Key}`
    // loop and subsumes it for its own answer: it only answers `true` when every pair's
    // key is already present with the identical value word, so a missing key can never
    // be a no-op and falls through to the validation (which raises `{badkey,Key}`
    // exactly as before).
    if unchanged_after_update(machine, ctx, module, map, pairs)? {
        write_operand(machine, dst, map)?;
        return Ok(Step::Next);
    }
    let capacity = size + pairs.len();
    // The flat arm's `{badkey,Key}` validation, decided before the reserve exactly as
    // the path-copying arm's is: the same `map_get`-per-key shape, so nothing is
    // materialized and nothing is allocated. Rooting order is the reads' rule too —
    // the key read is the iteration's one allocating read, so the source word is
    // re-read after it (a snapshot pair list taken before it would have every word
    // stale after the collecting allocation the key read may run).
    for (key_operand, _) in pairs {
        let key = read_operand(machine, ctx, key_operand)?;
        let map = read_operand(machine, ctx, source)?;
        // The namespace is taken AFTER both operand reads (it borrows `*ctx`
        // immutably, so it must not be held across `read_operand`'s `&mut`).
        if beam_term::map::map_get(map, key, ctx.atoms(module))?.is_none() {
            return fail_or_raise(machine, module, labels, ctx, fail, "badkey", key_operand);
        }
    }
    // Phase B: reserve, then re-read and merge (the pre-validated keys are re-read so
    // the merge uses the post-collection terms).
    let extra = merge_slack_words(source, pairs);
    let result = terms::make_map_rooted(
        machine,
        module,
        ctx,
        RootSet::ALL_LIVE,
        capacity,
        extra,
        |m, slack, atoms| {
            let map = read_operand_slack(m, source, slack)?;
            let mut merged = terms::map_entries(map)?;
            for (key_operand, value_operand) in pairs {
                let key = read_operand_slack(m, key_operand, slack)?;
                let value = read_operand_slack(m, value_operand, slack)?;
                match position(&merged, key, atoms) {
                    Some(index) => merged[index].1 = value,
                    None => {
                        return Err(error::unsupported(
                            "put_map_exact: key vanished across the reserve",
                        ))
                    }
                }
            }
            Ok(merged)
        },
    )?;
    write_operand(machine, dst, result)?;
    Ok(Step::Next)
}
