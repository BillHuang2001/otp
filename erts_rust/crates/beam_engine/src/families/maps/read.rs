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
//! Both read the source with [`beam_term::map::map_get`] — **one descent per key**,
//! the C's `erts_hashmap_get` (`emu/map_instrs.tab:49-122`): a hashed path walk for
//! a hashmap/HAMT and the bounded key scan for a flat map, so both frozen layouts
//! are read. Its key rule is `beam_term::cmp`'s `EQ` (the exact `=:=` ordering), so
//! `1` and `1.0` stay distinct keys and the engine still reimplements no term
//! ordering. The source is therefore **never enumerated**: no `[(Eterm, Eterm)]`
//! pair list is materialized, and a read costs O(k · log n) for k keys instead of
//! the old whole-map walk — which walked every HAMT node and grew a `Vec` per pair —
//! plus a linear scan per key.
//!
//! A non-map source takes the `fail` branch, or raises `{badmap, Source}` when the
//! label is `NO_FAIL`; an absent key takes the `fail` branch, or — when the label is
//! `NO_FAIL` (engine-defined: the ground truth's read failure sets no reason) — raises
//! the bare atom `function_clause` (the parent module docs carry the full semantics
//! and the ground truth).
//!
//! # Rooting order: the source word `map_get` consumes is read last
//!
//! `read_operand` can **allocate** — an `Operand::Integer`/`Character` beyond a small
//! int is materialized as a bignum ([`crate::operand::operand_slack_words`] is
//! non-zero only for those) — and an allocation may run a moving collector that
//! relocates any `Eterm` read before it. So no source-map word and no key word is
//! held across another operand read:
//!
//! 1. read the key (this is the iteration's allocating read);
//! 2. **re-read the source** — a register or literal word read, which cannot allocate
//!    (an allocating source operand is an integer, so the `is_map` check above already
//!    failed and never reached the loop), hence the key read in step 1 stays valid;
//! 3. take the `&AtomTable` from `ctx.atoms(module)` (it borrows `*ctx` immutably, so
//!    it cannot be held across `read_operand`'s `&mut` either);
//! 4. descend with `map_get`, which allocates nothing.
//!
//! Nothing after step 1 allocates, so the word handed to `map_get` is the
//! post-collection term. A malformed map's `TermError::BadMap` is reconciled into
//! `BeamError::BadArg` — the error the previous `map_entries` walk produced — never a
//! silent fallback and never a wrong value.

use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Label, Module, Operand};

use crate::context::ExecutionContext;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::predicates;
use crate::Step;

use super::{fail_or_function_clause, fail_or_raise};

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
        return fail_or_raise(machine, module, labels, ctx, fail, "badmap", source);
    }
    for (key_operand, dst_operand) in pairs {
        let key = read_operand(machine, ctx, key_operand)?;
        // Re-read the source AFTER the key read (see "Rooting order" above): the key
        // read may allocate, and an allocation may relocate the map word.
        let map = read_operand(machine, ctx, source)?;
        // The namespace is taken AFTER the operand reads (it borrows `*ctx`
        // immutably, so it must not be held across `read_operand`'s `&mut`).
        match beam_term::map::map_get(map, key, ctx.atoms(module))? {
            Some(value) => write_operand(machine, dst_operand, value)?,
            None => {
                return fail_or_function_clause(machine, module, labels, ctx, fail);
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
        return fail_or_raise(machine, module, labels, ctx, fail, "badmap", source);
    }
    for key_operand in keys {
        let key = read_operand(machine, ctx, key_operand)?;
        // Same rooting order as `get_map_elements`: the map word is read after the
        // iteration's allocating operand read.
        let map = read_operand(machine, ctx, source)?;
        if beam_term::map::map_get(map, key, ctx.atoms(module))?.is_none() {
            return fail_or_function_clause(machine, module, labels, ctx, fail);
        }
    }
    Ok(Step::Next)
}
