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

//! Map read instructions — the `get_map_elements` / `has_map_fields` side.
//!
//! Owns:
//! - `get_map_elements` ([`beam_loader::Instr::GetMapElements`]) — fetch map
//!   elements, failing when a key is absent;
//! - `has_map_fields` ([`beam_loader::Instr::HasMapFields`]) — test for the presence
//!   of map keys.
//!
//! Both read the source through [`crate::terms::map_entries`], which handles **both**
//! frozen map layouts (flat map and hashmap/HAMT, walked node by node), and match
//! keys with `beam_term::cmp` — no hash function, no reimplemented term ordering. A
//! non-map source takes the `fail` branch, or raises `{badmap, Source}` when the
//! label is `NO_FAIL`; an absent key takes the `fail` branch (the parent module docs
//! carry the full semantics and the ground truth).

use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Label, Module, Operand};

use crate::context::ExecutionContext;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::predicates;
use crate::terms;
use crate::Step;

use super::{fail_or_raise, jump_or_unsupported, lookup};

/// `get_map_elements Fail Src {Key,Dst}*` — write each key's value to its
/// destination; a non-map source or an absent key fails.
pub(super) fn get_map_elements(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    ctx: &mut dyn ExecutionContext,
    fail: Label,
    source: &Operand,
    pairs: &[(Operand, Operand)],
) -> Result<Step> {
    let map = read_operand(machine, ctx, source)?;
    if !predicates::is_map(map) {
        return fail_or_raise(machine, module, labels, ctx, fail, "badmap", map);
    }
    let entries = terms::map_entries(map)?;
    for (key_operand, dst_operand) in pairs {
        let key = read_operand(machine, ctx, key_operand)?;
        match lookup(&entries, key, &module.atom_table) {
            Some(value) => write_operand(machine, dst_operand, value)?,
            None => {
                return jump_or_unsupported(
                    labels,
                    fail,
                    "get_map_elements: key absent with no fail label",
                );
            }
        }
    }
    Ok(Step::Next)
}

/// `has_map_fields Fail Src {Key}*` — every key must be present, else fail. No
/// writes.
pub(super) fn has_map_fields(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    ctx: &mut dyn ExecutionContext,
    fail: Label,
    source: &Operand,
    keys: &[Operand],
) -> Result<Step> {
    let map = read_operand(machine, ctx, source)?;
    if !predicates::is_map(map) {
        return fail_or_raise(machine, module, labels, ctx, fail, "badmap", map);
    }
    let entries = terms::map_entries(map)?;
    for key_operand in keys {
        let key = read_operand(machine, ctx, key_operand)?;
        if lookup(&entries, key, &module.atom_table).is_none() {
            return jump_or_unsupported(
                labels,
                fail,
                "has_map_fields: key absent with no fail label",
            );
        }
    }
    Ok(Step::Next)
}
