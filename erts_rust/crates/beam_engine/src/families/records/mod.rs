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

//! Record instruction family.
//!
//! Owns every record instruction (per `../beam_loader/CONTEXT.md`, "The
//! instruction vocabulary"):
//! - the OTP-26 tagged-tuple form [`Instr::UpdateRecord`] — copy a record tuple
//!   with fields replaced
//! - the OTP-29/30 **native-record** set (generic opcodes 186-193):
//!   [`Instr::IsAnyNativeRecord`] (`is_record/1`), [`Instr::IsNativeRecord`]
//!   (`is_record/3`), [`Instr::GetRecordElements`], [`Instr::GetRecordElementsId`],
//!   [`Instr::PutRecord`], [`Instr::IsRecordAccessible`], [`Instr::GetRecordField`],
//!   [`Instr::UpdateRecordId`]
//!
//! The work is split by responsibility: this file holds [`exec`] (the dispatch),
//! the shared operand/failure helpers and the per-variant semantics; [`raw`] is the
//! family's single `#[allow(unsafe_code)]` module forwarding to the frozen
//! `beam_term::record` layout (the term-layout lowest common ancestor — this family
//! never re-derives it); `tests` is the per-variant unit suite (it would push this
//! file past the ~1000-line target, so it lives in its own submodule).
//!
//! Ground truth (read-only, never modified):
//! - tagged tuples: `emu/instrs.tab` `i_update_record_copy` (`:785`) /
//!   `i_update_record_in_place` (`:804`) / `i_update_record_continue` (`:836`) and
//!   `i_is_any_native_record` (`:1323`), the `update_record` transform + its
//!   generator in `emu/ops.tab:1611` and `emu/generators.tab:1028`.
//! - native records: `emu/instrs.tab:1323-1458` (`i_is_native_record`,
//!   `i_get_record_elements`, `i_create_native_record`, `i_update_native_record`,
//!   `i_is_record_accessible`, `i_is_wildcard_record_accessible`,
//!   `i_get_local_record_field`, `i_get_record_field`), their transforms in
//!   `emu/ops.tab:1634-1694`, and `erl_record.c`
//!   (`erl_is_native_record:214`, `erl_get_record_elements:223`,
//!   `erl_create_native_record:356`, `erl_update_native_record:693`,
//!   `erl_is_record_accessible:764`, `erl_is_wildcard_record_accessible:773`,
//!   `erl_get_local_record_field:782`, `erl_get_record_field:824`).
//!
//! # Semantics
//!
//! ## Tagged tuples — `update_record`
//! `UpdateRecord { hint, size, src, dst, updates }` is the OTP-26 `update_record`
//! instruction the compiler emits for `#r{field = Value}` record updates. `src`
//! names a tuple of arity `size` (the tagged record tuple), `updates` is the
//! `(offset, value)` list in file order and `dst` receives the updated tuple.
//!
//! - **`hint` is the compiler's destructive-update reuse hint** (`copy`/`reuse`/
//!   `inplace`, `lib/compiler/src/genop.tab`). Milestone 1 has **no** in-place
//!   reuse, so the hint is ignored and the result is *always* a fresh copy. This is
//!   observationally equivalent to C's in-place path: the compiler only emits
//!   `inplace` for a `src` tuple that is unique/dead at that point
//!   (`beam_ssa_destructive_update.erl`), so no live term ever observes the
//!   difference — while a copy can never clobber a tuple that is still shared.
//! - **`offset` is 1-based** (`HTOP[$Offset]` with `HTOP[0]` the arityval header,
//!   `emu/generators.tab:1043-1047`), i.e. tuple element `offset - 1`.
//! - **Copy-then-patch.** C copies the whole tuple up-front and then overwrites
//!   each replaced field (`i_update_record_copy`); that order matters because an
//!   update value operand may alias `dst` — which is exactly why the compiler
//!   stashes the old destination in `SCRATCH_X_REG` and rewrites later update
//!   operands to it (`emu/generators.tab:1064-1081`). Reading every source element
//!   before writing `dst` reproduces that order without a scratch register:
//!   an update value that reads `dst` still sees the *original* tuple.
//!
//! ## Native records — the OTP-29/30 set
//! A native record is a boxed term whose interior layout `beam_term::record` owns
//! (`ErtsRecordInstance` =
//! `[thing_word, definition][value in ascending-key order]…`, the definition =
//! `[arityval, field_order, hash, module, name, is_exported][key in
//! ascending-atom-index order]…`). Every field reference in these instructions is
//! an **atom name**, and every one of them is resolved against the record's *own*
//! definition — never against a compile-time table:
//!
//! - [`Instr::IsNativeRecord`] — `erl_is_native_record`: succeed iff the record's
//!   definition `module`/`name` are the two operand atoms.
//! - [`Instr::GetRecordElements`] / [`Instr::GetRecordElementsId`] — read each
//!   listed `(field name, destination)` pair's value out of the record and fall
//!   through. `get_record_elements_id`'s `Id` operand is **discarded** by
//!   `emu/ops.tab:1643-1645` (`get_record_elements_id Fail _Id Src … =>
//!   i_get_record_elements Fail Src …`), so the two share one implementation and a
//!   name the record does not have is the only failure (`erl_get_record_elements`
//!   returns `false` after scanning all fields).
//! - [`Instr::PutRecord`] — the *update* form (`Src` a register,
//!   `i_update_native_record`) copies the instance with the listed fields replaced;
//!   the *create* form (`Src` is the literal `nil`,
//!   `emu/ops.tab:1648` → `i_create_native_record`, the compiler's *local* case
//!   `put_record Fail Name=a nil …` → `create_local_record` →
//!   `i_create_local_native_record`) builds a fresh instance from the running
//!   module's own `Recs` definition (create-form semantics are documented in
//!   [`create_native_record`]). `emu/ops.tab:1653-1655` discards the `Id` on the
//!   update path (`put_record Fail=j _Id Src=xy … => i_update_native_record`): the
//!   record's identity comes from the source instance's own definition.
//! - [`Instr::UpdateRecordId`] — the OTP-30 `update_record_id`, which
//!   `emu/ops.tab:1689-1691` lowers to the *same* `i_update_native_record` with
//!   `Hint` and `Id` discarded (`Fail=j _Hint _Id Src Dst Live Size Updates`), so
//!   it shares [`Instr::PutRecord`]'s update path exactly.
//! - [`Instr::IsRecordAccessible`] — `scope` is the atom `external`
//!   (`erl_is_record_accessible`: the definition must be exported) or the running
//!   module (`emu/ops.tab:1678` substitutes `$MODULE` for any other scope atom →
//!   `erl_is_wildcard_record_accessible`: exported **or** created in that module).
//! - [`Instr::GetRecordField`] — the `Id` operand selects the C form: an **atom**
//!   is the record's name in the running module (`i_get_local_record_field` →
//!   `erl_get_local_record_field`, no export requirement, an `_` name skips the
//!   name test), a **`{Module,Name}` pair** is a foreign record
//!   (`i_get_record_field` → `erl_get_record_field`, which additionally requires
//!   the definition to be exported).
//! - `hint` on both update forms is the compiler's reuse hint: M1 never updates in
//!   place, so the result is always a fresh copy (as for
//!   [`Instr::UpdateRecord`]).
//! - `live` is carried by the update/create instructions (the C uses it to root the
//!   operands across the GC it may trigger); every record build here first reserves
//!   its block through the **rooted** allocation seam
//!   ([`crate::terms::reserve`] → `Machine::alloc_rooted`) with
//!   [`RootSet::ALL_LIVE`], so the whole X-register file plus the stack and the FR
//!   file are the standing root set and the count is not consulted — the same stance
//!   [`Instr::UpdateRecord`] takes.
//!
//! # Failure model (never a panic, never a wrong result)
//!
//! * A **record-semantics** failure the C reports by branching — a non-record
//!   source, a definition `module`/`name` mismatch, a field name the record does
//!   not have, a definition that is not exported where accessibility is required —
//!   **branches to the instruction's `fail` label** ([`fail_step`]), exactly the
//!   C's `$FAIL($Fail)` / `$FAIL_HEAD_OR_BODY($Fail)`. The C reaches several of
//!   these through an `ASSERT(is_record(src))` because the compiler always puts the
//!   corresponding guard in front; treating a non-record as a failed guard is the
//!   same decision that guard would have made and never fabricates a value.
//! * `fail == NO_FAIL` (the compiler promised the guard cannot fail) ⇒ the typed
//!   `error::unsupported("<instr>/no_fail")` — never a fall-through with a wrong
//!   result, and never a fabricated BEAM exception reason.
//! * A genuine **capability gap** ⇒ a typed `error::unsupported("<instr>: <what>")`.
//!   The one such gap here is the *create* form of [`Instr::PutRecord`] with an
//!   `Id` naming **another module's** record: the C resolves such a definition
//!   through the code server's record table (`erts_record_find_entry`,
//!   `erl_record.c:102-170`) or traps to `code:ensure_loaded/1`
//!   (`erl_create_native_record_jit`, `erl_record.c:412`), which this runtime does
//!   not have. A record defined in the running module needs no table — its `Recs`
//!   definition is right there — and the *update* form needs none either, because
//!   the source instance carries its own definition (see the crate `CONTEXT.md`,
//!   "Known Issues").
//! * `update_record`'s tagged-tuple form is only emitted behind an
//!   `is_tagged_tuple` test of the same fail label, so the C opcode reads the tuple
//!   unchecked; a source that is not a tuple, a `size`/arity mismatch, a non-integer
//!   or out-of-range offset is a documented typed `unsupported` (the same convention
//!   as `select_tuple_arity`'s non-tuple path). `read_operand`/`write_operand` and
//!   the `terms` readers/writers propagate their own typed errors.
#![allow(clippy::too_many_arguments)]
mod raw;
#[cfg(test)]
mod tests;

use crate::interpreter::LabelMap;

use beam_common::{ExceptionClass, Result};
use beam_loader::{
    GetRecordFieldArgs, Instr, Label, Module, Operand, PutRecordArgs, RecordDefinition,
    UpdateRecordArgs, UpdateRecordIdArgs, NO_FAIL,
};
use beam_term::{make_atom, make_small, Atom, Eterm, NIL};

use crate::context::{ExecutionContext, RootSet};
use crate::error;
use crate::interpreter::resolve;
use crate::machine::Machine;
use crate::operand::{operand_slack_words, read_operand, read_operand_slack, write_operand};
use crate::predicates;
use crate::terms;
use crate::Step;

/// Execute a record-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a record opcode this family executes,
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
        // ---------------------------------------------------------------------
        // Tagged tuples (OTP 26)
        // ---------------------------------------------------------------------
        Instr::UpdateRecord(b) => {
            let UpdateRecordArgs {
                // The reuse hint is intentionally ignored: M1 never updates in place
                // (see the module docs).
                hint: _,
                size,
                src,
                dst,
                updates,
            } = &**b;
            // The declared arity sizes the reserved block; the source tuple is read
            // *after* the (possibly collecting) reserve, from the machine's
            // post-collection state, so no `Eterm` copy is ever held across it.
            let arity = *size as usize;
            let extra: usize = operand_slack_words(src)
                + updates
                    .iter()
                    .map(|(_, value)| operand_slack_words(value))
                    .sum::<usize>();
            if arity == 0 {
                // `terms::make_tuple` returns the shared zero-arity literal without
                // invoking its reader, which would skip the source test the reader
                // performs. A record tuple always carries its name tag, so the
                // compiler never emits `update_record` for arity 0 — this keeps a
                // malformed module a typed error instead of a silent `{}`.
                let source = read_operand(machine, ctx, src)?;
                if terms::tuple_arity(source)
                    .map_err(|_| error::unsupported("update_record: non-tuple source"))?
                    != 0
                {
                    return Err(error::unsupported("update_record: size/arity mismatch"));
                }
                write_operand(machine, dst, beam_term::tuple0())?;
                return Ok(Some(Step::Next));
            }

            let result =
                terms::make_tuple_rooted(machine, ctx, RootSet::ALL_LIVE, arity, extra, |m, slack| {
                    let source = read_operand_slack(m, src, slack)?;
                    let found = terms::tuple_arity(source)
                        .map_err(|_| error::unsupported("update_record: non-tuple source"))?;
                    if found != arity {
                        return Err(error::unsupported("update_record: size/arity mismatch"));
                    }

                    // Copy the source tuple first (arities may exceed a fixed-size
                    // array, and this is the one place the family reads raw element
                    // slots). `src` is copied, never reused in place: M1 has no
                    // destructive update (see the module docs).
                    let mut elements = Vec::with_capacity(arity);
                    for index in 0..arity {
                        elements.push(terms::tuple_element(source, index)?);
                    }

                    for (offset, value) in updates {
                        let offset = match offset {
                            Operand::Integer(offset) => *offset,
                            _ => {
                                return Err(error::unsupported(
                                    "update_record: non-integer index",
                                ))
                            }
                        };
                        // `Offset` is 1-based: element `Offset - 1` of the tuple.
                        if offset < 1 || offset as usize > arity {
                            return Err(error::unsupported("update_record: index out of range"));
                        }
                        elements[offset as usize - 1] = read_operand_slack(m, value, slack)?;
                    }
                    Ok(elements)
                })?;
            write_operand(machine, dst, result)?;
            Ok(Some(Step::Next))
        }

        // ---------------------------------------------------------------------
        // Native records (OTP 29/30, generic opcodes 186-193)
        // ---------------------------------------------------------------------
        // `is_any_native_record(Fail, Src) { if (!is_record($Src)) $FAIL($Fail); }`
        // (`emu/instrs.tab:1323`): succeed (fall through) iff `Src` is a native
        // record, else branch to `Fail`.
        Instr::IsAnyNativeRecord { fail, src } => {
            let source = read_operand(machine, ctx, src)?;
            if predicates::is_record(source) {
                Ok(Some(Step::Next))
            } else {
                Ok(Some(fail_step(labels, *fail, "is_any_native_record")?))
            }
        }

        // `i_is_native_record(Fail, Src, Mod, Name) {
        //      if (!erl_is_native_record($Src, $Mod, $Name)) $FAIL($Fail); }`
        // (`emu/instrs.tab:1329`): the definition's module/name must be `Mod`/`Name`.
        Instr::IsNativeRecord {
            fail,
            src,
            module: owner,
            name,
        } => {
            let source = read_operand(machine, ctx, src)?;
            let owner = read_atom(machine, ctx, owner, "is_native_record: module")?;
            let name = read_atom(machine, ctx, name, "is_native_record: name")?;
            if raw::record_is_native(source, owner, name) {
                Ok(Some(Step::Next))
            } else {
                Ok(Some(fail_step(labels, *fail, "is_native_record")?))
            }
        }

        // `i_get_record_elements(Fail, Src, N)` (`emu/instrs.tab:1347`).
        Instr::GetRecordElements { fail, src, fields } => Ok(Some(get_record_elements(
            machine,
            labels,
            module,
            "get_record_elements",
            *fail,
            src,
            fields,
            ctx,
        )?)),

        // `get_record_elements_id Fail _Id Src Size Matches=* =>
        //      i_get_record_elements Fail Src Size Matches` (`emu/ops.tab:1643`):
        // the `Id` is discarded, so this *is* `get_record_elements`.
        Instr::GetRecordElementsId {
            fail,
            id: _,
            src,
            fields,
        } => Ok(Some(get_record_elements(
            machine,
            labels,
            module,
            "get_record_elements_id",
            *fail,
            src,
            fields,
            ctx,
        )?)),

        // `put_record Fail Id Src Dst Live Updates` (`emu/ops.tab:1648-1655`):
        // `Src = nil` is the *create* form (`i_create_native_record`, and for an
        // atom `Id` the compiler's local `put_record Fail Name=a nil …` →
        // `create_local_record` transform), any other `Src` is the *update* form
        // (`i_update_native_record`, `Id` discarded).
        Instr::PutRecord(b) => {
            let PutRecordArgs {
                fail,
                id,
                src,
                dst,
                live: _,
                updates,
            } = &**b;
            if matches!(src, Operand::Nil) {
                return Ok(Some(create_native_record(
                    machine,
                    labels,
                    module,
                    "put_record",
                    *fail,
                    id,
                    dst,
                    updates,
                    ctx,
                )?));
            }
            Ok(Some(update_native_record(
                machine,
                labels,
                "put_record",
                *fail,
                src,
                dst,
                updates,
                ctx,
            )?))
        }

        // `i_is_record_accessible(Fail, Src)` / `i_is_wildcard_record_accessible
        // (Fail, Src, Module)` selected by the `Scope` operand
        // (`emu/ops.tab:1672-1678`).
        Instr::IsRecordAccessible { fail, src, scope } => {
            let source = read_operand(machine, ctx, src)?;
            let scope = read_atom(machine, ctx, scope, "is_record_accessible: scope")?;
            let exported = raw::record_is_exported(source)?;
            let accessible = if module.atom_name(scope) == Some(EXTERNAL) {
                // `erl_is_record_accessible`: the definition must be exported.
                exported
            } else {
                // `erl_is_wildcard_record_accessible(src, $MODULE)`.
                exported || raw::record_definition_module_is(source, scope)?
            };
            if accessible {
                Ok(Some(Step::Next))
            } else {
                Ok(Some(fail_step(labels, *fail, "is_record_accessible")?))
            }
        }

        // `i_get_local_record_field(Fail, Src, Id, Field, Dst)` (`Id` an atom) or
        // `i_get_record_field(Fail, Src, Id, Field, Dst)` (`Id` a `{Module,Name}`
        // pair) (`emu/ops.tab:1680-1694`).
        Instr::GetRecordField(b) => {
            let GetRecordFieldArgs {
                fail,
                src,
                id,
                field,
                dst,
            } = &**b;
            Ok(Some(get_record_field(
                machine,
                labels,
                module,
                "get_record_field",
                *fail,
                src,
                id,
                field,
                dst,
                ctx,
            )?))
        }

        // `update_record_id Fail _Hint _Id Src Dst Live Size Updates=* =>
        //      i_update_native_record Fail Src Dst Live Size Updates`
        // (`emu/ops.tab:1689`): the same update path as `put_record`, with the
        // reuse `Hint` and the record `Id` discarded. M1 never updates in place, so
        // the result is always a fresh copy.
        Instr::UpdateRecordId(b) => {
            let UpdateRecordIdArgs {
                fail,
                hint: _,
                id: _,
                src,
                dst,
                live: _,
                updates,
            } = &**b;
            Ok(Some(update_native_record(
                machine,
                labels,
                "update_record_id",
                *fail,
                src,
                dst,
                updates,
                ctx,
            )?))
        }

        _ => Ok(None),
    }
}

/// The `$FAIL($Fail)` decision shared by every native-record guard.
///
/// A live label is a jump; [`NO_FAIL`] — the compiler's "this test cannot fail"
/// marker — is the typed `unsupported("<instruction>/no_fail")`, because falling
/// through would produce a *wrong result* and inventing a BEAM exception reason
/// would fabricate ground truth the C keeps in `p->freason`/`p->fvalue`.
fn fail_step(
    labels: &LabelMap,
    fail: Label,
    instruction: &str,
) -> Result<Step> {
    if fail == NO_FAIL {
        return Err(error::unsupported(&format!("{instruction}/no_fail")));
    }
    let (func, pc) = resolve(labels, fail)?;
    Ok(Step::Goto { func, pc })
}

/// The `Scope` atom of the strict `is_record_accessible` form
/// (`emu/ops.tab:1672`, `am_external`).
const EXTERNAL: &str = "external";

/// The atom `_` (`am_Underscore`), the wildcard record name
/// (`erl_record.c:800`, `:842`).
const UNDERSCORE: &str = "_";

/// Resolve an operand that must denote an atom — the `a` operands of
/// `emu/ops.tab`'s native-record instructions and a field-name operand.
///
/// Anything else (a register, a list, …) is a typed `unsupported`: the compiler
/// never emits one there, so reporting it is better than reading an arbitrary
/// register as a compile-time atom.
fn read_atom(
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
    operand: &Operand,
    what: &str,
) -> Result<Atom> {
    let term = read_operand(machine, ctx, operand)?;
    if beam_term::is_atom(term) {
        Ok(beam_term::atom_val(term))
    } else {
        Err(error::unsupported(what))
    }
}

/// The record identity an `Id` operand denotes.
enum RecordId {
    /// An **atom**: the record's name in the running module
    /// (`i_get_local_record_field`); the atom [`UNDERSCORE`] is the wildcard form,
    /// which skips the name test.
    Local(Atom),
    /// A **`{Module,Name}` pair** literal: a record created in another module
    /// (`i_get_record_field`), which additionally requires the definition to be
    /// exported.
    Foreign { module: Atom, name: Atom },
}

/// Resolve an `Id` operand to the record identity it denotes
/// (`get_record_field Fail Src Id Field Dst`, `emu/ops.tab:1680-1694`, and the
/// create form's `put_record Fail Id nil …`).
///
/// The same operand carries both C forms: the transform sends an atom `Id` to
/// `i_get_local_record_field` (or, for create, to the local
/// `create_local_record`) and any other `Id` to the two `i_*_record_field` /
/// `i_create_native_record` forms.
fn read_record_id(
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
    operand: &Operand,
    instruction: &str,
) -> Result<RecordId> {
    let term = read_operand(machine, ctx, operand)?;
    if beam_term::is_atom(term) {
        return Ok(RecordId::Local(beam_term::atom_val(term)));
    }
    if matches!(terms::tuple_arity(term), Ok(2)) {
        let owner = terms::tuple_element(term, 0)?;
        let name = terms::tuple_element(term, 1)?;
        if beam_term::is_atom(owner) && beam_term::is_atom(name) {
            return Ok(RecordId::Foreign {
                module: beam_term::atom_val(owner),
                name: beam_term::atom_val(name),
            });
        }
    }
    Err(error::unsupported(&format!(
        "{instruction}: record id is neither an atom nor a {{Module,Name}} pair"
    )))
}

/// `get_record_elements` / `get_record_elements_id` (`emu/instrs.tab:1347-1356`)
/// — read each listed field's value into its destination register and fall through.
///
/// C scans the *record's* fields and consumes a `(name, register)` pair whenever the
/// current field's key matches, which requires the pair list to be in the record's
/// ascending-key order (`emu/ops.tab`'s `map_key_sort` guarantees it). Resolving each
/// pair by name instead writes exactly the same value into exactly the same register
/// — the register is part of the pair — for any list the compiler can emit, and a
/// name the record does not have is the C's `return false` ⇒ `$FAIL($Fail)`.
fn get_record_elements(
    machine: &mut Machine,
    labels: &LabelMap,
    module: &Module,
    instruction: &str,
    fail: Label,
    src: &Operand,
    fields: &[(Operand, Operand)],
    ctx: &mut dyn ExecutionContext,
) -> Result<Step> {
    let source = read_operand(machine, ctx, src)?;
    if !predicates::is_record(source) {
        return fail_step(labels, fail, instruction);
    }
    for (field, dst) in fields {
        let key = read_atom(machine, ctx, field, instruction)?;
        match raw::record_get(source, key, &module.atom_table)? {
            Some(value) => write_operand(machine, dst, value)?,
            None => return fail_step(labels, fail, instruction),
        }
    }
    Ok(Step::Next)
}

/// `i_get_local_record_field` / `i_get_record_field` (`emu/instrs.tab:1433-1458`)
/// — read one field of a native record into `Dst`.
///
/// Reproduces `erl_get_local_record_field` (`erl_record.c:782-822`, the `Id` is an
/// atom naming the record in the running module) and `erl_get_record_field`
/// (`erl_record.c:824-880`, the `Id` is a `{Module,Name}` pair and the definition
/// must be exported) branch for branch: a non-record, a definition mismatch, a
/// non-exported foreign record or an absent field name is the C's `badrecord`/
/// `badfield` ⇒ `$FAIL_HEAD_OR_BODY($Fail)`.
fn get_record_field(
    machine: &mut Machine,
    labels: &LabelMap,
    module: &Module,
    instruction: &str,
    fail: Label,
    src: &Operand,
    id: &Operand,
    field: &Operand,
    dst: &Operand,
    ctx: &mut dyn ExecutionContext,
) -> Result<Step> {
    let source = read_operand(machine, ctx, src)?;
    if !predicates::is_record(source) {
        // `is_not_record(src)` ⇒ `badrecord` ⇒ `$FAIL_HEAD_OR_BODY`.
        return fail_step(labels, fail, instruction);
    }
    match read_record_id(machine, ctx, id, instruction)? {
        RecordId::Local(name) => {
            // `if (name != am_Underscore && defp->name != name) goto badrecord;`
            if module.atom_name(name) != Some(UNDERSCORE) && !raw::record_name_is(source, name) {
                return fail_step(labels, fail, instruction);
            }
        }
        RecordId::Foreign { module: owner, name } => {
            // `if (defp->module != module || defp->name != name) goto badrecord;`
            if !raw::record_is_native(source, owner, name) {
                return fail_step(labels, fail, instruction);
            }
            // `if (defp->is_exported == am_false) goto badrecord;`
            if !raw::record_is_exported(source)? {
                return fail_step(labels, fail, instruction);
            }
        }
    }
    let field = read_atom(machine, ctx, field, instruction)?;
    match raw::record_get(source, field, &module.atom_table)? {
        Some(value) => {
            write_operand(machine, dst, value)?;
            Ok(Step::Next)
        }
        // The C's `badfield` after scanning every key.
        None => fail_step(labels, fail, instruction),
    }
}

/// `i_create_local_native_record` (`emu/instrs.tab:1354-1369`) — the **create**
/// form of `put_record` (`Src` is the literal `nil`, `emu/ops.tab:1648`).
///
/// Reproduces `erl_create_local_native_record` (`erl_record.c:254-353`) branch for
/// branch, on the running module's own `Recs` definition
/// ([`beam_loader::Module::records`]) rather than on C's load-time `def_literal`
/// (`beam_file.c:1077-1300`, which this loader does not materialize — the loader
/// exposes the same data structurally instead):
///
/// * the definition's fields are walked in **ascending key order** (`defp->keys[i]`,
///   the atom-index order `beam_term::record::write_record` uses), matched against
///   the next entry of the ascending-sorted update list (`ops.tab:1648`'s
///   `map_key_sort`);
/// * a field the update names takes the update value and consumes the entry;
/// * a field the update does not name takes its **default**; a field whose default
///   is the *absent* sentinel (`-record #r{f}` — the file's `u,0`,
///   [`beam_loader::RecordField::default`] `== None`) stores `NIL` and, only when
///   no earlier `no_value` error is already pending, remembers its key as the
///   delayed error field — so only the **first** such field is remembered
///   (`erl_record.c:318-326`);
/// * after the walk an **unconsumed** update entry (its key is not a field of the
///   definition) is the `badfield` error, which **outranks** `no_value`, and the
///   error field is that unconsumed key (`erl_record.c:331-337`).
///
/// Success writes the fresh instance to `Dst`; an error takes the instruction's
/// failure path ([`create_failure`]).
///
/// # The record's identity (`Id`)
///
/// Only a record **defined in the running module** is creatable. C reads the
/// definition out of the `Recs`-derived load-time literal for the local form and,
/// for a foreign `{Module,Name}` id, out of the code server's record table
/// (`erts_record_find_entry`), potentially trapping to `code:ensure_loaded/1` for a
/// not-yet-loaded module (`erl_create_native_record_jit`, `erl_record.c:412-500`).
/// This runtime has no record table, so an `Id` naming another module stays a typed
/// `error::unsupported` — never a guessed definition. Two `Id` shapes are accepted:
/// an **atom** (the running module's own record) and a **`{Module,Name}` pair**
/// whose `Module` **is** the running module (the same record, reached through the
/// generic form). A `Module` the caller's own module does not define is a typed
/// `unsupported` too, since the compiler only emits an `Id` its record table can
/// resolve.
fn create_native_record(
    machine: &mut Machine,
    labels: &LabelMap,
    module: &Module,
    instruction: &str,
    fail: Label,
    id: &Operand,
    dst: &Operand,
    updates: &[(Operand, Operand)],
    ctx: &mut dyn ExecutionContext,
) -> Result<Step> {
    let definition = local_definition(machine, ctx, module, id, instruction)?;
    let fields = &definition.fields;

    // Ascending key order — `defp->keys[i]`, the same atom-index order
    // `beam_term::record::write_record` sorts the block's keys into.
    let mut order: Vec<usize> = (0..fields.len()).collect();
    order.sort_by_key(|&index| fields[index].name);

    // The update list in the ascending order `ops.tab:1648`'s `map_key_sort`
    // guarantees. Reading each field-name operand here (before any value operand)
    // mirrors the C, which receives the field names as resolved `a` operands. An
    // atom operand is an *immediate* and `sorted` otherwise holds `&Operand`s into
    // the module's decoded IR — neither is a machine-heap term, so this walk is
    // collection-invariant and may run before the reservation below.
    let mut sorted: Vec<(Atom, &Operand)> = Vec::with_capacity(updates.len());
    for (field, value) in updates {
        sorted.push((read_atom(machine, ctx, field, instruction)?, value));
    }
    sorted.sort_by_key(|&(key, _)| key);

    // **Reserve, then read, then write.** The construction block is reserved with
    // ONE rooted allocation *before* any field value is read: `reserve` may run a
    // moving collector, so an `Eterm` read into `values` before it would be a stale
    // pointer afterwards. The block's size is fixed by the definition's field count
    // (known above, before any value is read), so a single reservation suffices.
    let hp = crate::terms::reserve(
        machine,
        ctx,
        beam_term::record_block_words(fields.len()),
        RootSet::ALL_LIVE,
    )?;

    // `values[declaration index]` — the block is built in declaration order and
    // `write_record` re-sorts it exactly as the C's `qsort` does. Every update value
    // is read from the machine's POST-collection state (i.e. after the reservation).
    let mut values: Vec<Eterm> = vec![NIL; fields.len()];
    let mut absent: Option<Atom> = None;
    let mut position = 0usize;
    for &index in &order {
        let field = fields[index];
        if position < sorted.len() && sorted[position].0 == field.name {
            // `GetSource(new_p[1], *hp)` — the value is read only once its field
            // resolved, exactly as the C skips the other branch.
            values[index] = read_operand(machine, ctx, sorted[position].1)?;
            position += 1;
            continue;
        }
        match field.default {
            // A default is either an immediate or a term in the module's immutable
            // literal area (`beam_loader::LiteralArea`, its own `beam_alloc::Heap`) —
            // never on the process heap — so no collection relocates it and the value
            // copied here is valid before or after the reservation.
            Some(value) => values[index] = value,
            None => {
                // `if (is_value(res)) { p->fvalue = defp->keys[i]; … }`
                absent = absent.or(Some(field.name));
                values[index] = NIL;
            }
        }
    }
    // `badfield` outranks `no_value` (`erl_record.c:331-337`): the entry that was
    // never consumed is `new_p`, which the walk above left at the first one.
    // The failure reason is built by [`create_failure`], which takes its **own**
    // rooted reservation — the block reserved above is simply abandoned (its pointer
    // is not a root, and no term in it was published), never partially written.
    if position < sorted.len() {
        return create_failure(
            machine,
            module,
            labels,
            ctx,
            instruction,
            fail,
            BADFIELD,
            definition,
            sorted[position].0,
        );
    }
    if let Some(field) = absent {
        return create_failure(
            machine,
            module,
            labels,
            ctx,
            instruction,
            fail,
            NOVALUE,
            definition,
            field,
        );
    }

    let fields: Vec<(Eterm, Eterm)> = fields
        .iter()
        .zip(&values)
        .map(|(field, value)| (make_atom(field.name), *value))
        .collect();
    // `hp` is still valid: nothing has allocated from the BEAM heap since the
    // reservation (every `read_operand` above is a pure machine read, save for a
    // non-small integer operand's **unrooted** `alloc_words`, which never collects).
    let record = write_local_record(module, definition, &fields, hp)?;
    write_operand(machine, dst, record)?;
    Ok(Step::Next)
}

/// The atom `badfield` (`am_badfield`, `EXC_BADFIELD`, `error.h:172` /
/// `error_atom[21]`, `atom.names:127`).
const BADFIELD: &str = "badfield";

/// The atom `novalue` (`am_novalue`, `EXC_NOVALUE`, `error.h:174` /
/// `error_atom[22]`, `atom.names:539`).
const NOVALUE: &str = "novalue";

/// The running module's own record definition an `Id` operand names, or a typed
/// `error::unsupported` for an id this runtime cannot resolve (another module's
/// record — see [`create_native_record`] — or a name the module does not define).
fn local_definition<'a>(
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
    module: &'a Module,
    id: &Operand,
    instruction: &str,
) -> Result<&'a RecordDefinition> {
    let name = match read_record_id(machine, ctx, id, instruction)? {
        RecordId::Local(name) => name,
        RecordId::Foreign { module: owner, name } => {
            if owner != module.name {
                return Err(error::unsupported(&format!(
                    "{instruction}: create: no record definition table for another module"
                )));
            }
            name
        }
    };
    module.record(name).ok_or_else(|| {
        let name = module.atom_name(name).unwrap_or("<unnamed atom>");
        error::unsupported(&format!(
            "{instruction}: create: the module defines no record named '{name}'"
        ))
    })
}

/// The create form's failure path.
///
/// A live `fail` label is the C's `$FAIL($Fail)` branch (the record error is
/// discarded); [`NO_FAIL`] is the C's `goto find_func_info`, which raises
/// `{Tag, {{Module,Name}, Field}}` — the C builds `TUPLE2(TUPLE2(Module, Name),
/// Field)` into `p->fvalue` (`erl_record.c:339-351`) and `expand_error_value`
/// (`beam_common.c:754-768`) wraps it with `error_atom[r]`, i.e. `badfield`
/// (`EXC_BADFIELD` = index 21) or `novalue` (`EXC_NOVALUE` = index 22), as a
/// **class `error`** exception.
///
/// The tag atom is resolved from the running module's atom table; a module that
/// never mentions it cannot name the reason, so a missing atom is a documented
/// typed `error::unsupported` (never a substituted, wrong atom) — the same
/// convention the maps family's `{badmap,_}`/`{badkey,_}` reasons follow.
///
/// The whole reason graph is laid out as **one** 9-word block reserved with a
/// single rooted allocation ([`crate::terms::reserve`]) and carved by
/// [`raw::write_create_failure_reason`]: three successive allocations would strand
/// the earlier tuples under a moving collector, and the graph's elements are all
/// immediates (atoms and `arityval` headers), so it needs no spare tail.
fn create_failure(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    ctx: &mut dyn ExecutionContext,
    instruction: &str,
    fail: Label,
    tag: &str,
    definition: &RecordDefinition,
    field: Atom,
) -> Result<Step> {
    if fail != NO_FAIL {
        return fail_step(labels, fail, instruction);
    }
    let tag = module.atom_table.lookup(tag).ok_or_else(|| {
        error::unsupported(&format!(
            "{instruction}: create: no '{tag}' atom in the module"
        ))
    })?;
    // `{Tag, {{Module, Name}, Field}}` in one reservation (see the doc above).
    let reason =
        raw::write_create_failure_reason(machine, ctx, module.name, definition.name, field, tag)?;
    if let Some(class_atom) = module.atom_table.lookup(ExceptionClass::Error.as_str()) {
        machine.set_class_atom(ExceptionClass::Error, make_atom(class_atom));
    }
    machine.throw_error(reason)
}

/// Build a fresh native-record instance from the running module's record
/// `definition` — the `erl_create_local_native_record` result — into the
/// **already-reserved** construction block at `hp`.
///
/// `fields` are the `(key atom term, value)` pairs in **declaration** order, the
/// shape [`raw::write_record`] takes. `hp` must point at
/// [`beam_term::record_block_words`]`(fields.len())` exclusively-owned, writable
/// words the caller reserved with one rooted allocation ([`crate::terms::reserve`]);
/// taking `hp` rather than reserving here is what lets [`create_native_record`]
/// reserve **before** it reads the field values (the frozen *reserve, then read, then
/// write* ordering).
///
/// The definition's cached hash is a word of the very block it describes, so the
/// block is written once with a `0` placeholder to obtain a *valid* definition
/// term, the hash is computed from it (it reads only the module, the name, the
/// `is_exported` flag and the declaration-ordered keys — never the hash word), and
/// the **same block** is written again with the real hash. The value
/// (`make_hash2({Module,Name,IsExported,Keys})`, `beam_file.c:1240-1256`) is
/// observable — `erlang:phash2/1` of a record mixes definition word 2
/// (`beam_term::hashing::make_hash2`) — so it must never be a placeholder; it is
/// computed by the bit-exact `beam_term::hashing::compute_record_definition_hash`.
///
/// `hp` stays valid across the second write because neither `raw::record_definition`
/// nor `compute_record_definition_hash` allocates from the BEAM heap.
fn write_local_record(
    module: &Module,
    definition: &RecordDefinition,
    fields: &[(Eterm, Eterm)],
    hp: *mut Eterm,
) -> Result<Eterm> {
    let (owner, name, exported) = (module.name, definition.name, definition.is_exported);
    // `hp` owns `record_block_words(fields.len())` exclusively-owned, writable words
    // and `fields` are the declaration-order `(key, value)` pairs `write_record`
    // documents. `0` is the placeholder for the cached definition hash, which cannot
    // exist before the block does.
    let instance = raw::write_record(hp, owner, name, exported, fields, make_small(0))?;
    let definition_term = raw::record_definition(instance)
        .ok_or_else(|| error::unsupported("put_record: create: malformed record block"))?;
    let hash = beam_term::hashing::compute_record_definition_hash(
        definition_term,
        &module.atom_table,
    )?;
    // The same block at `hp` — nothing allocated it away in between (neither
    // `record_definition` nor `compute_record_definition_hash` allocates) — and the
    // same inputs, plus the real cached definition hash; every word is rewritten.
    raw::write_record(
        hp,
        owner,
        name,
        exported,
        fields,
        make_small(hash as i64),
    )
}

/// `i_update_native_record` (`emu/instrs.tab:1418-1431`) — copy the native record
/// in `Src` with the listed fields replaced, into `Dst`.
///
/// Reproduces `erl_update_native_record` (`erl_record.c:693-762`): the copy keeps the
/// source's definition and every value the update does not mention, and a field name
/// the record does not have is the C's `badfield` ⇒ `$FAIL_HEAD_OR_BODY($Fail)`.
/// The `hint` operand is deliberately not part of this signature — M1 has no
/// in-place reuse, so the result is always a fresh copy (observationally equivalent
/// for the `reuse`/`inplace` a unique source authorises; see the module docs).
///
/// Every update value is read **before** `Dst` is written, so an update value that
/// aliases the destination still observes the original record — the same ordering
/// [`Instr::UpdateRecord`] documents.
fn update_native_record(
    machine: &mut Machine,
    labels: &LabelMap,
    instruction: &str,
    fail: Label,
    src: &Operand,
    dst: &Operand,
    updates: &[(Operand, Operand)],
    ctx: &mut dyn ExecutionContext,
) -> Result<Step> {
    // A pure machine read (no allocation) that only *sizes* the reservation: the
    // source's field count is metadata of the record's own definition.
    let probe = read_operand(machine, ctx, src)?;
    if !predicates::is_record(probe) {
        return fail_step(labels, fail, instruction);
    }
    let count = raw::definition_field_count(record_definition(probe, instruction)?)?;

    // **Reserve, then read, then write.** The construction block is reserved with ONE
    // rooted allocation *before* any field value is read: `reserve` may run a moving
    // collector, so the source record (`probe` above is only a copy, and it is dead
    // once the collector relocates it) and every value derived from it must be read
    // from the machine's POST-collection state below. The block's size is fixed by the
    // source's field count, which is available before any value is read.
    let hp = crate::terms::reserve(
        machine,
        ctx,
        beam_term::record_block_words(count),
        RootSet::ALL_LIVE,
    )?;

    let source = read_operand(machine, ctx, src)?;
    if !predicates::is_record(source) {
        // Unreachable — `probe` already tested this very term — but reported rather
        // than dereferenced as a record.
        return fail_step(labels, fail, instruction);
    }
    let definition = record_definition(source, instruction)?;
    let keys = raw::definition_keys(definition)?;
    let mut values = raw::record_values(source)?;

    for (field, value) in updates {
        let key = read_atom(machine, ctx, field, instruction)?;
        let index = match finding(keys.iter(), key) {
            Some(index) => index,
            None => return fail_step(labels, fail, instruction),
        };
        // Read the value only once its field resolved, and before `Dst` is written.
        values[index] = read_operand(machine, ctx, value)?;
    }

    // `hp` (and `definition`/`values`) are still valid: nothing has allocated from the
    // BEAM heap since the reservation — the update reads are pure machine reads, save
    // for a non-small integer operand's **unrooted** `alloc_words`, which never
    // collects.
    let result = write_record_instance(definition, &values, instruction, hp)?;
    write_operand(machine, dst, result)?;
    Ok(Step::Next)
}

/// The position of the field keyed `key` in the definition's ascending-key order.
fn finding<'a>(keys: impl Iterator<Item = &'a Eterm>, key: Atom) -> Option<usize> {
    keys.enumerate().find_map(|(index, k)| {
        (beam_term::is_atom(*k) && beam_term::atom_val(*k) == key).then_some(index)
    })
}

/// The source instance's definition, or the typed `unsupported` error when the
/// term is not a native record (unreachable after the caller's
/// [`predicates::is_record`] test — reported rather than guessed).
fn record_definition(source: Eterm, instruction: &str) -> Result<Eterm> {
    raw::record_definition(source)
        .ok_or_else(|| error::unsupported(&format!("{instruction}: non-record source")))
}

/// Build a fresh native-record instance carrying `definition`'s identity and
/// `values` (in ascending-key order) — the `erl_update_native_record` result.
///
/// `beam_term::write_record` rebuilds the whole construction block
/// (`[field_order tuple][definition][instance]`), so the definition is
/// **structurally identical** to the source's: the declaration order comes from
/// `field_order`, the keys re-sort to the same ascending order, and the cached
/// `hash` word is copied verbatim. Only the definition's *address* differs — the
/// frozen layout (and therefore `beam_term`'s readers, `beam_gc`'s sizing) is
/// unchanged, and no `beam_term` writer exists for an instance alone (the layout is
/// owned there; this family never re-derives it).
///
/// The construction block is passed in as `hp` ([`beam_term::record_block_words`]`(count)`
/// exclusively-owned words): [`update_native_record`] reserves it with **one** rooted
/// allocation **before** it reads the source, so this function must not allocate — it
/// only reads `definition`'s metadata (which lives in the source's block) and writes
/// `hp`.
fn write_record_instance(
    definition: Eterm,
    values: &[Eterm],
    instruction: &str,
    hp: *mut Eterm,
) -> Result<Eterm> {
    let count = raw::definition_field_count(definition)?;
    if count != values.len() {
        return Err(error::unsupported(&format!(
            "{instruction}: field count mismatch"
        )));
    }
    let declared = raw::declaration_keys(definition)?;
    let order = raw::definition_field_order(definition)?;
    let owner = raw::definition_module(definition)?;
    let name = raw::definition_name(definition)?;
    let is_exported = raw::definition_is_exported(definition)?;
    let hash = raw::definition_hash(definition)?;

    // `field_order[declared]` is the rank of that field's key in the ascending-key
    // order the instance stores its values in.
    let mut fields = Vec::with_capacity(count);
    for index in 0..count {
        let malformed = || error::unsupported(&format!("{instruction}: malformed definition"));
        let entry = order.get(index).copied().filter(|e| beam_term::is_small(*e)).ok_or_else(malformed)?;
        let value = values
            .get(beam_term::unsigned_value(entry) as usize)
            .copied()
            .ok_or_else(malformed)?;
        let key = declared.get(index).copied().ok_or_else(malformed)?;
        fields.push((key, value));
    }

    raw::write_record(hp, owner, name, is_exported, &fields, hash)
}
