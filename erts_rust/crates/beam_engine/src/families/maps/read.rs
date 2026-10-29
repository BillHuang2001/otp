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
//! Both read the source operand **once**, before the loop, and descend that one held
//! word per key with [`beam_term::map::map_get`] — the C's `i_get_map_elements`
//! reading `map = $Src` into a local and then calling `erts_hashmap_get` per key
//! (`emu/map_instrs.tab:64-122`). Holding the word is what makes the instruction
//! **atomic**: a destination register that aliases the source register (`erlc` emits
//! `get_map_elements` with `Src == Dst` for the first pair, e.g. `khepri_tree`'s
//! `gather_node_props/2`) cannot affect a later descent. `map_get` answers a
//! hashmap/HAMT by a hashed path walk and a flat map by its bounded key scan, so both
//! frozen layouts are read. Its key rule is `beam_term::cmp`'s `EQ` (the exact `=:=`
//! ordering), so `1` and `1.0` stay distinct keys and the engine still reimplements no
//! term ordering. The source is therefore **never enumerated**: no `[(Eterm, Eterm)]`
//! pair list is materialized.
//!
//! # Error → outcome table (the whole instruction)
//!
//! | Condition | `fail != NO_FAIL` | `fail == NO_FAIL` |
//! |---|---|---|
//! | source operand is not a map (checked on the held word, before any write) | jump to `fail` | raise `{badmap, Source}` |
//! | a key is absent (`map_get` → `Ok(None)`) | jump to `fail` | raise the **bare atom** `function_clause` |
//! | the held map's *interior* is malformed (`map_get` → `Err(BadMap)` on a word that already passed `is_map`) | typed `BeamError::BadArg` | typed `BeamError::BadArg` |
//!
//! The first two rows are the ground truth's own outcomes: the C's plain
//! `$FAIL($Fail)` on a non-map or a missing key (`emu/map_instrs.tab:49-122` →
//! `emu/macros.tab:56-60`), and — for the `NO_FAIL` non-map case, which only
//! hand-built IR can reach, since the compiler emits its own `test,is_map` guard first
//! — the `{badmap, Source}` the module docs' "absent-key `NO_FAIL` reason" section and
//! `tests_read.rs::a_non_map_source_jumps_to_fail_or_raises_badmap` pin.
//! The third row is the malformed-map residual the retired `terms::map_entries` walk
//! answered with the same `BeamError::BadArg`: it is reachable **only** from that
//! cause, because the source is validated once on the very word every descent
//! consumes, so `BadMap` can no longer mean "this register is not a map" — that is
//! the entry check's outcome. See [`descend`].
//!
//! # Rooting order: the source word is held, and nothing in the loop allocates
//!
//! `read_operand` can **allocate** — an `Operand::Integer`/`Character` beyond a small
//! int is materialized as a bignum ([`crate::operand::operand_slack_words`] is
//! non-zero only for those) — and an allocation may run a moving collector that
//! relocates any `Eterm` held across it. The held source word is therefore made safe
//! **by construction**, never by re-reading the operand (a destination write may have
//! clobbered it):
//!
//! 1. [`load`] asks [`operand_slack_words`] of every key operand. The compiler's
//!    shapes answer `0` for all of them (keys are atoms, small ints or literals), so
//!    the loop is **allocation-free** and the word read at entry is the word every
//!    descent consumes.
//! 2. When some key operand *does* need heap words, **every** such key is materialized
//!    up front inside ONE rooted reservation ([`crate::terms::reserve_slack`], a
//!    single [`crate::context::ExecutionContext::alloc_words_rooted`] call over the
//!    whole `RootSet`), and only then is the source operand read — after the last
//!    allocation the instruction can perform. The keys live inside that reservation
//!    and nothing afterwards allocates, so neither they nor the held source word can
//!    move.
//!
//! Invariants this establishes, on both paths:
//!
//! (a) no descent reads a word a destination write of this same instruction
//!     clobbered — the word is read before the first write and never re-read;
//! (b) no held raw `Eterm` survives a possibly-collecting read unrelocated;
//! (c) a destination write stores the value the source held at instruction entry.
//!
//! # Cost
//!
//! One `map_get` descent per key — O(log n) word reads for a hashmap, at most
//! `MAP_SMALL_MAP_LIMIT` (32) `cmp` comparisons for a flat map — and **nothing
//! materialized**: no pair list, no `Vec` over the source, no map block. The
//! allocating-key path additionally reserves one bignum's words per such key, in the
//! one reservation described above.

use crate::interpreter::LabelMap;

use beam_common::{BeamError, Result};
use beam_loader::{Label, Module, Operand};
use beam_term::{AtomTable, Eterm, TermError};

use crate::context::ExecutionContext;
use crate::machine::Machine;
use crate::operand::{operand_slack_words, read_operand, read_operand_slack, write_operand};
use crate::predicates;
use crate::terms;
use crate::Step;

use super::{fail_or_function_clause, fail_or_raise};

/// The instruction's **held** source word plus the key terms whose read would have
/// needed heap words.
struct HeldSource {
    /// The source word, read once after the instruction's last allocation and
    /// descended for every key.
    map: Eterm,
    /// The materialized keys, in the instruction's key order — `None` means every key
    /// read is allocation-free and the loop reads each one through [`read_operand`].
    keys: Option<Vec<Eterm>>,
}

/// Read the instruction's source operand as the word the descents consume, or
/// `Ok(None)` when it is not a map (the caller takes the documented `badmap`
/// outcome).
///
/// Every key whose read would allocate is materialized first, in ONE rooted
/// reservation, so nothing the descent loop does can allocate or relocate the held
/// word ("Rooting order" in the module docs).
fn load<'a>(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    source: &Operand,
    keys: impl Iterator<Item = &'a Operand> + Clone,
) -> Result<Option<HeldSource>> {
    let map = read_operand(machine, ctx, source)?;
    if !predicates::is_map(map) {
        // Decided before the reservation, so a non-map source materializes no key.
        return Ok(None);
    }
    let slack: usize = keys.clone().map(operand_slack_words).sum();
    if slack == 0 {
        // The compiler's shapes: no key read can allocate, so the word read here is
        // the word every descent consumes (invariant (a)) and it cannot move
        // (invariant (b)).
        return Ok(Some(HeldSource { map, keys: None }));
    }
    let mut region = terms::reserve_slack(machine, ctx, slack)?;
    // `read_operand_slack` only carves the reservation (a pure machine read for every
    // non-immediate operand), so this loop allocates nothing.
    let keys = keys
        .map(|key| read_operand_slack(machine, key, &mut region))
        .collect::<Result<Vec<Eterm>>>()?;
    // The reservation may have relocated the source: read it again, on the
    // post-collection state, after the instruction's last allocation.
    let map = read_operand(machine, ctx, source)?;
    Ok(Some(HeldSource {
        map,
        keys: Some(keys),
    }))
}

/// The key term at `index`: from the reservation when the instruction materialized
/// its keys up front, else an allocation-free [`read_operand`] (guaranteed by the
/// `slack == 0` decision in [`load`]).
fn key_at(
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
    held: &HeldSource,
    index: usize,
    key_operand: &Operand,
) -> Result<Eterm> {
    match &held.keys {
        Some(keys) => Ok(keys[index]),
        None => read_operand(machine, ctx, key_operand),
    }
}

/// One descent of the held source word, with the error split recorded.
///
/// `map` has already passed [`predicates::is_map`] (in [`load`]), so the only
/// `TermError::BadMap` [`beam_term::map::map_get`] can still answer is a malformed
/// **interior** — an unknown HAMT node header — which is the residual the retired
/// `terms::map_entries` walk reported as `BeamError::BadArg`. "This word is not a
/// map" never reaches here; that is the entry check's outcome.
fn descend(map: Eterm, key: Eterm, atoms: &AtomTable) -> Result<Option<Eterm>> {
    match beam_term::map::map_get(map, key, atoms) {
        Ok(found) => Ok(found),
        Err(TermError::BadMap) => Err(BeamError::BadArg),
        Err(other) => Err(other.into()),
    }
}

/// `get_map_elements Fail Src {Key,Dst}*` — write each key's value to its
/// destination; a non-map source or an absent key fails.
///
/// The source is held once, so a `Dst` that aliases `Src` leaves every later pair
/// reading the original map (the C's `i_get_map_elements`).
pub(super) fn get_map_elements(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    ctx: &mut dyn ExecutionContext,
    fail: Label,
    source: &Operand,
    pairs: &[(Operand, Operand)],
) -> Result<Step> {
    let held = match load(machine, ctx, source, pairs.iter().map(|(key, _)| key))? {
        Some(held) => held,
        None => return fail_or_raise(machine, module, labels, ctx, fail, "badmap", source),
    };
    for (index, (key_operand, dst_operand)) in pairs.iter().enumerate() {
        let key = key_at(machine, ctx, &held, index, key_operand)?;
        // The namespace is taken after the key read (it borrows `*ctx` immutably, so
        // it must not be held across `read_operand`'s `&mut`).
        match descend(held.map, key, ctx.atoms(module))? {
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
    let held = match load(machine, ctx, source, keys.iter())? {
        Some(held) => held,
        None => return fail_or_raise(machine, module, labels, ctx, fail, "badmap", source),
    };
    for (index, key_operand) in keys.iter().enumerate() {
        let key = key_at(machine, ctx, &held, index, key_operand)?;
        if descend(held.map, key, ctx.atoms(module))?.is_none() {
            return fail_or_function_clause(machine, module, labels, ctx, fail);
        }
    }
    Ok(Step::Next)
}
