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

//! Map instruction family.
//!
//! Owns the map-access/update instructions (per `../beam_loader/CONTEXT.md`, "The
//! instruction vocabulary"):
//! - [`Instr::GetMapElements`] — fetch map elements (failing when a key is absent)
//! - [`Instr::HasMapFields`] — test for the presence of map keys
//! - [`Instr::PutMapAssoc`] — update/build a map (associating new keys)
//! - [`Instr::PutMapExact`] — update a map (failing on an unknown key)
//!
//! Map terms are read/built with the `crate::terms` map helpers
//! (`map_entries` / `make_map`); key matching uses `beam_term::cmp` (the exact
//! `=:=` ordering, i.e. `erts_cmp` with `exact=1`) so the engine never reimplements
//! term ordering and maps keep their `=:=` key semantics (`1` and `1.0` are
//! distinct keys).
//!
//! `map_entries` reads **both** frozen map layouts — a flat map and a hashmap
//! (HAMT, walked node by node) — so the read instructions
//! ([`Instr::GetMapElements`] / [`Instr::HasMapFields`]) handle a hashmap source
//! with a plain `beam_term::cmp` scan and no hash function. *Building* a map goes
//! through [`crate::terms::make_map`] (`beam_term::map::write_map`), which picks
//! the frozen layout by size: a **flat** map at/below `MAP_SMALL_MAP_LIMIT` (32)
//! distinct keys, a **hashmap** (HAMT) above it — exactly the oracle's choice, so a
//! map grown past the limit by compiled code is a correct hashmap.
//!
//! # Layout
//!
//! The work is split by responsibility: this file holds [`exec`] (the dispatch), the
//! key-lookup helpers and the failure paths shared by both halves; [`read`] holds the
//! read side (`get_map_elements` / `has_map_fields`); [`write`] holds the update side
//! (`put_map_assoc` / `put_map_exact`). The comprehensive unit suite (every opcode
//! variant, both map layouts incl. the 32/33-key boundary, the failure paths and the
//! dispatch contract) lives in the sibling `tests` module.
//!
//! # Ground truth (read-only)
//!
//! * `emu/map_instrs.tab:49-122` — `i_get_map_element` / `i_get_map_elements`
//!   (flatmap scan + hashmap `erts_hashmap_get`; a missing key is `$FAIL`).
//! * `emu/map_instrs.tab:124-173` — `update_map_assoc` / `update_map_exact`
//!   (`erts_gc_update_map_assoc` builds a *new* map; the exact variant fails when
//!   the new key is not already present).
//! * `beam_common.c:2237-2268` — `erts_gc_update_map_exact` sets
//!   `p->freason = BADKEY` and `p->fvalue = key` before returning non-value, so the
//!   observable failure reason is `{badkey, Key}` (verified against the oracle with
//!   `maps:from_list([{b,1}])#{a := 2}` ⇒ `error:{badkey,a}`).
//! * `beam/generators.tab:230-253` — `has_map_fields Fail Src Size Rest` lowers to
//!   `get_map_elements` with `SCRATCH_X_REG` destinations: "every key must be
//!   present, the value is discarded". Same failure set as `get_map_elements`.
//! * `emu/ops.tab:1364-1424` — the transforms (`put_map_assoc` "can never fail and
//!   does not need any failure label").
//! * `emu/beam_emu.c:578-583` (`i_func_info_IaaI`) + `beam_common.c:448-544`
//!   (`handle_error`) — the `function_clause` reason for a head/guard/pattern
//!   failure with no branch, the reason this engine gives the absent-key `NO_FAIL`
//!   branch (see "The absent-key `NO_FAIL` reason" below).
//!
//! # Semantics
//!
//! * `GetMapElements { fail, source, pairs }` — read `source`; if it is not a map
//!   jump to `fail` (or raise `{badmap, Source}` when `fail` is
//!   [`NO_FAIL`] — the reason the compiler's own `is_map` guard branch raises). For
//!   each `(key, dst)` pair, look `key` up; **a missing key jumps to `fail`**, or —
//!   when `fail` is [`NO_FAIL`] — raises the bare atom `function_clause` (the
//!   engine-defined reason for this branchless failure, see below), otherwise the
//!   value is written to `dst`. Partial writes before a failure are harmless (the
//!   clause matcher restarts at `fail`).
//! * `HasMapFields { fail, source, keys }` — every key must be present, else jump
//!   to `fail`, or raise the same bare `function_clause` when `fail` is
//!   [`NO_FAIL`]. No writes.
//! * `PutMapAssoc { fail, source, dst, live, pairs }` — build a **new** map =
//!   `source` with each `(key, value)` inserted or overwritten, then write it to
//!   `dst`. Maps are immutable, so the source is never mutated in place. The result
//!   is a flat map (≤ 32 distinct keys) or a hashmap above that, per the frozen
//!   layout rules (see [`crate::terms::make_map`]).
//! * `PutMapExact { fail, source, dst, live, pairs }` — as `PutMapAssoc`, but every
//!   key must already exist; a missing key jumps to `fail` (or raises
//!   `{badkey, Key}` when `fail` is [`NO_FAIL`], the ground-truth reason above).
//!
//! `live` (the count of live X registers the C collector treats as GC roots across
//! the update) is **unused at M1**: `MinimalContext` has no moving collector, so the
//! update cannot relocate terms. It stays in the IR for the Stage-4 collector.
//!
//! # The absent-key `NO_FAIL` reason (engine-defined)
//!
//! The read path's C failure is the plain `$FAIL($Fail)` (`emu/map_instrs.tab:49-122`
//! → `emu/macros.tab:56-60`), which sets **no** exception reason: only
//! `erts_gc_update_map_exact` sets `BADKEY` (`beam_common.c:2265`). The installed
//! `erlc` never emits [`NO_FAIL`] for the matching forms (both `get_map_elements` and
//! `has_map_fields` always carry a fail label), so a **missing key with
//! `fail == NO_FAIL`** has no ground truth. It is therefore engine-defined and raises
//! class `error` with the **bare atom** `function_clause` — the reason C gives a
//! head/guard/pattern failure that finds no branch (`emu/beam_emu.c:578-583`
//! `i_func_info_IaaI`, `beam_common.c:448-544`). The raise is the engine's single
//! failed-clause definition, [`raise_function_clause_no_fail`] (in
//! `crate::interpreter::dispatch`), which `fail_or_function_clause` calls with
//! `what = "maps"`; that helper's doc carries the full ground truth and the
//! by-name/typed-gap contract (the tag is resolved **only** from the running module's
//! atom table, a module that cannot name it yielding the documented typed
//! `error::unsupported`, never a substituted atom). The non-map `source` case
//! is unrelated and stays `{badmap, Source}` (the compiler's own `is_map` guard
//! reason).
//!
//! # Milestone-1 limits (all documented, never a wrong result)
//!
//! * **Atom key order follows the module-local atom table.** The frozen flat-map
//!   key order (`erts_cmp_flatmap_keys`) sorts atoms by their *global* atom index
//!   (`erl_utils.h:267-272`); this engine's atoms are module-local indices, so a
//!   flat map's atom keys are sorted by *those*. Map lookups compare keys with
//!   `beam_term::cmp`, never positionally, so this only affects `term_to_binary`
//!   byte-for-byte comparison of atom-keyed maps, never map semantics.
//! * A non-map `source` that the ground truth does not route to `fail` (e.g. a
//!   `put_map_assoc` whose dropped fail label is always `NO_FAIL`) raises
//!   `{badmap, Source}` rather than mis-reading the term.
//! * A **missing key with `fail == NO_FAIL`** is engine-defined (see the section
//!   above): it raises the bare atom `function_clause`, or the typed
//!   `error::unsupported` when the module cannot name it.

mod read;
#[cfg(test)]
mod tests;
mod write;

use crate::interpreter::{raise_function_clause_no_fail, LabelMap};

use beam_common::{ExceptionClass, Result};
use beam_loader::{Instr, Label, MapWriteArgs, Module, Operand, NO_FAIL};
use beam_term::{cmp, make_atom, AtomTable, Eterm};

use crate::context::{ExecutionContext, RootSet};
use crate::error;
use crate::machine::Machine;
use crate::operand::{operand_slack_words, read_operand_slack};
use crate::terms;
use crate::Step;

/// Execute a map-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a map opcode this family executes,
/// `Ok(None)` when `instr` is not this family's variant, and `Err` on a raise.
pub(crate) fn exec(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    instr: &Instr,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    let _ = (func, pc);
    match instr {
        Instr::GetMapElements {
            fail,
            source,
            pairs,
        } => read::get_map_elements(machine, module, labels, ctx, *fail, source, pairs).map(Some),

        Instr::HasMapFields { fail, source, keys } => {
            read::has_map_fields(machine, module, labels, ctx, *fail, source, keys).map(Some)
        }

        Instr::PutMapAssoc(b) => {
            let MapWriteArgs {
                fail,
                source,
                dst,
                live,
                pairs,
            } = &**b;
            write::put_map_assoc(machine, module, labels, ctx, *fail, source, dst, *live, pairs)
                .map(Some)
        }

        Instr::PutMapExact(b) => {
            let MapWriteArgs {
                fail,
                source,
                dst,
                live,
                pairs,
            } = &**b;
            write::put_map_exact(machine, module, labels, ctx, *fail, source, dst, *live, pairs)
                .map(Some)
        }

        // Not this family's variant.
        _ => Ok(None),
    }
}

/// The value bound to `key` in `entries`, using exact key equality.
///
/// The comparison is delegated to `beam_term::cmp` (the exact `=:=` ordering) so
/// map keys keep `erts_cmp`'s semantics — in particular `1` and `1.0` are distinct
/// keys — and the engine never reimplements term ordering.
fn lookup(entries: &[(Eterm, Eterm)], key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
    entries
        .iter()
        .find(|(candidate, _)| cmp(*candidate, key, atoms).is_eq())
        .map(|(_, value)| *value)
}

/// The index of `key`'s binding in `entries`, using exact key equality (for the
/// in-place overwrite of the freshly built copy, never the shared source).
fn position(entries: &[(Eterm, Eterm)], key: Eterm, atoms: &AtomTable) -> Option<usize> {
    entries
        .iter()
        .position(|(candidate, _)| cmp(*candidate, key, atoms).is_eq())
}

/// Jump to `label` (the ground-truth failure path).
fn goto(labels: &LabelMap, label: Label) -> Result<Step> {
    let (func, pc) = labels.resolve(label)?;
    Ok(Step::Goto { func, pc })
}

/// The `{Tag,Value}`-with-a-fail-label failure path: jump to `fail`, or — when
/// `fail` is [`NO_FAIL`] (no branch for this failure) — raise `{Tag,Value}` as a
/// class-`error` exception. `value` is the **operand** whose term is the reason's
/// second element, re-read after the reason block's reserve.
fn fail_or_raise(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    ctx: &mut dyn ExecutionContext,
    fail: Label,
    tag: &str,
    value: &Operand,
) -> Result<Step> {
    if fail == NO_FAIL {
        raise_error_tuple(machine, module, ctx, tag, value)
    } else {
        goto(labels, fail)
    }
}

/// The fail-label-only failure path (the read instructions' absent key): jump to
/// `fail`, or — when `fail` is [`NO_FAIL`] — raise the bare atom `function_clause`
/// through the engine's single failed-clause definition,
/// [`raise_function_clause_no_fail`] (which carries the ground truth and the
/// by-name/typed-gap contract; see the "absent-key `NO_FAIL` reason" section in the
/// module docs).
fn fail_or_function_clause(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    ctx: &mut dyn ExecutionContext,
    fail: Label,
) -> Result<Step> {
    if fail == NO_FAIL {
        raise_function_clause_no_fail(machine, module, "maps", ctx)
    } else {
        goto(labels, fail)
    }
}

/// Raise `{Tag,Value}` as a class-`error` exception (the `{badmap,Source}` /
/// `{badkey,Key}` reasons).
///
/// The tag atom is resolved from the running module's atom table; a module that
/// never mentions it cannot name the reason, so a missing atom is a documented
/// typed `error::unsupported` (never a substituted, wrong atom). The `error` class
/// atom is installed when the module has it, so a landing can place it in `x(3)`.
fn raise_error_tuple(
    machine: &mut Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    tag: &str,
    value: &Operand,
) -> Result<Step> {
    let tag_atom = module
        .atom_table
        .lookup(tag)
        .ok_or_else(|| error::unsupported(&format!("maps: no '{tag}' atom in the module")))?;
    // Reserve the reason block first, then re-read the value operand from the
    // post-collection machine state (a moving collector may run in the reserve).
    let extra = operand_slack_words(value);
    let reason = terms::make_tuple_rooted(machine, ctx, RootSet::ALL_LIVE, 2, extra, |m, s| {
        Ok(vec![make_atom(tag_atom), read_operand_slack(m, value, s)?])
    })?;
    if let Some(class_atom) = module.atom_table.lookup(ExceptionClass::Error.as_str()) {
        machine.set_class_atom(ExceptionClass::Error, make_atom(class_atom));
    }
    machine.throw_error(reason)
}
