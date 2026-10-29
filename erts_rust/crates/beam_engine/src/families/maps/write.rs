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
//! Maps are immutable, so the source is never mutated: the update merges into a fresh
//! pair list and rebuilds through [`crate::terms::make_map`], which delegates the
//! layout choice to `beam_term::map::write_map` — a **flat** map at/below
//! `MAP_SMALL_MAP_LIMIT` (32) distinct keys, a **hashmap** (HAMT) above it. Keys are
//! matched with `beam_term::cmp`, so `1` and `1.0` stay distinct keys. The `Live`
//! operand (the C collector's GC-root count) is unused at M1: there is no moving
//! collector to relocate terms (the parent module docs carry the full ground truth).
//!
//! # The no-change fast path (term identity)
//!
//! An update that changes nothing answers the **source map itself**, so
//! `erts_debug:same(M#{a => 1}, M)` is `true` when `M` already holds `a => 1` — the C's
//! `if (*old_vals != *hp) changed_values = 1;` … `return map;`
//! (`beam_common.c:2097,2142`, the flatmap route) and `if (CDR(ptr) == value) { *sz = 0
//! … }` ⇒ `res = map` (`erl_map.c:2493,2523-2526`, the HAMT route, which
//! `erts_gc_update_map_assoc`'s `is_not_flatmap` arm reaches).
//!
//! The decision is [`unchanged_after_update`]: every pair must be a **replacement whose
//! value word equals the stored word** (`Eterm` word equality, never `=:=` — the C
//! compares raw words). Keys are matched with the map's own key rule
//! (`beam_term::map::map_get`, i.e. `EQ`/`cmp == eq`), so a new key always changes the
//! map. A pure read, and the reason it is sound under a collecting host: it allocates
//! nothing at all — no reservation, no materialized operand — so no collection can run
//! and the source map word it copies to `dst` cannot have moved.
//!
//! One shape deliberately declines the fast path: an operand that is a **non-small
//! `Operand::Integer`** is materialized on the BEAM heap by its read
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
/// This is the *capacity* the update reserves: the builder deduplicates, so the
/// produced map holds at most `key count + new pairs`. The C update sizes its
/// reservation the same way, from the header's size word alone — `num_old =
/// flatmap_get_size(old_mp)` then `need = 2*(num_old+num_updates) + 1 + …`
/// (`beam_common.c:2002,2017`). The namespace argument of the accessor is unused
/// for counting; it is taken from the host so the count is read in the same atom
/// namespace as every other map operation.
fn key_count(ctx: &dyn ExecutionContext, module: &Module, map: Eterm) -> Result<usize> {
    Ok(beam_term::map::map_size(map, ctx.atoms(module))?)
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
    // Phase A (nothing retained past the reserve): the `badmap` check, the no-change
    // fast path and the entry count that sizes the reservation. All three are pure
    // reads, and the count is the head node's own size word ([`key_count`]) — not a walk
    // of the pair list: the source is enumerated exactly once, by the merge below, and
    // never here.
    let map = read_operand(machine, ctx, source)?;
    if !predicates::is_map(map) {
        return fail_or_raise(machine, module, labels, ctx, fail, "badmap", source);
    }
    // Phase A (continued): the no-change fast path — every pair a replacement with the
    // identical value word, so the source map *is* the update's result. Pure read, no
    // reservation (see [`unchanged_after_update`]), so the source word is still live.
    if unchanged_after_update(machine, ctx, module, map, pairs)? {
        write_operand(machine, dst, map)?;
        return Ok(Step::Next);
    }
    let capacity = key_count(ctx, module, map)? + pairs.len();
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
/// The no-change answer is the same fast path: `if (*hp != *old_vals) changed = 1;` …
/// `p->htop = old_hp; return map;` (`beam_common.c:2309,2325-2326`).
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
    // Phase A: the `badmap` check, the no-change fast path, the `{badkey,Key}` validation
    // and the entry count that sizes the reservation — all pure reads, nothing retained
    // past the reserve. The count is the head node's own size word ([`key_count`]), so it
    // costs no walk; the pair list is materialized once here (the validation needs it)
    // and once by the merge below.
    let map = read_operand(machine, ctx, source)?;
    if !predicates::is_map(map) {
        return fail_or_raise(machine, module, labels, ctx, fail, "badmap", source);
    }
    // The no-change fast path comes BEFORE the `{badkey,Key}` loop and subsumes it for
    // its own answer: it only answers `true` when every pair's key is already present
    // with the identical value word, so a missing key can never be a no-op and falls
    // through to the validation (which raises `{badkey,Key}` exactly as before).
    if unchanged_after_update(machine, ctx, module, map, pairs)? {
        write_operand(machine, dst, map)?;
        return Ok(Step::Next);
    }
    let capacity = key_count(ctx, module, map)? + pairs.len();
    let existing = terms::map_entries(map)?;
    for (key_operand, _) in pairs {
        let key = read_operand(machine, ctx, key_operand)?;
        // The namespace is taken AFTER the operand read (it borrows `*ctx`
        // immutably, so it must not be held across `read_operand`'s `&mut`).
        if position(&existing, key, ctx.atoms(module)).is_none() {
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
