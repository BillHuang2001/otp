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

use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Label, Module, Operand};

use crate::context::{ExecutionContext, RootSet};
use crate::error;
use crate::machine::Machine;
use crate::operand::{operand_slack_words, read_operand, read_operand_slack, write_operand};
use crate::predicates;
use crate::terms;
use crate::Step;

use super::{fail_or_raise, position};

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
    // Phase A (nothing retained past the reserve): the `badmap` check and the entry
    // count that sizes the reservation. Both are pure reads.
    let map = read_operand(machine, ctx, source)?;
    if !predicates::is_map(map) {
        return fail_or_raise(machine, module, labels, ctx, fail, "badmap", source);
    }
    let capacity = terms::map_entries(map)?.len() + pairs.len();
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
    // Phase A: the `badmap` check, the entry count, and the `{badkey,Key}` validation
    // — all pure reads, nothing retained past the reserve.
    let map = read_operand(machine, ctx, source)?;
    if !predicates::is_map(map) {
        return fail_or_raise(machine, module, labels, ctx, fail, "badmap", source);
    }
    let existing = terms::map_entries(map)?;
    for (key_operand, _) in pairs {
        let key = read_operand(machine, ctx, key_operand)?;
        // The namespace is taken AFTER the operand read (it borrows `*ctx`
        // immutably, so it must not be held across `read_operand`'s `&mut`).
        if position(&existing, key, ctx.atoms(module)).is_none() {
            return fail_or_raise(machine, module, labels, ctx, fail, "badkey", key_operand);
        }
    }
    let capacity = existing.len() + pairs.len();
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
