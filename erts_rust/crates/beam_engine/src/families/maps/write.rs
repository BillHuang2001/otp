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

use crate::context::ExecutionContext;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::predicates;
use crate::terms;
use crate::Step;

use super::{fail_or_raise, position};

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
    // `live` is the C collector's GC-root count; M1 has no moving
    // collector (see the module docs), so the update cannot relocate terms.
    let _ = live;
    let map = read_operand(machine, ctx, source)?;
    if !predicates::is_map(map) {
        return fail_or_raise(machine, module, labels, ctx, fail, "badmap", map);
    }
    let mut merged = terms::map_entries(map)?;
    for (key_operand, value_operand) in pairs {
        let key = read_operand(machine, ctx, key_operand)?;
        let value = read_operand(machine, ctx, value_operand)?;
        match position(&merged, key, &module.atom_table) {
            // Overwrite the existing binding…
            Some(index) => merged[index].1 = value,
            // …or associate the new key (maps are immutable; this is a
            // fresh copy, never an in-place mutation of `source`).
            None => merged.push((key, value)),
        }
    }
    let result = terms::make_map(ctx, &merged, &module.atom_table)?;
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
    // As `PutMapAssoc`: `live` is unused at M1.
    let _ = live;
    let map = read_operand(machine, ctx, source)?;
    if !predicates::is_map(map) {
        return fail_or_raise(machine, module, labels, ctx, fail, "badmap", map);
    }
    let mut merged = terms::map_entries(map)?;
    for (key_operand, value_operand) in pairs {
        let key = read_operand(machine, ctx, key_operand)?;
        let Some(index) = position(&merged, key, &module.atom_table) else {
            // Exact update on an absent key: `{badkey, Key}` on the
            // no-fail-label path (`beam_common.c:2237-2268`).
            return fail_or_raise(machine, module, labels, ctx, fail, "badkey", key);
        };
        let value = read_operand(machine, ctx, value_operand)?;
        merged[index].1 = value;
    }
    let result = terms::make_map(ctx, &merged, &module.atom_table)?;
    write_operand(machine, dst, result)?;
    Ok(Step::Next)
}
