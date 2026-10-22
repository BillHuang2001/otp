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

//! The `put_record` **create** form (`Src` is the literal `nil`).
//!
//! [`Instr::PutRecord`](beam_loader::Instr::PutRecord)'s create form is the one
//! record instruction that must *resolve a definition* before it can build
//! anything, and `emu/ops.tab:1648-1651` distinguishes two `Id` shapes for it:
//!
//! * an **atom** `Id` — the running module's own record (`ops.tab:1648`'s
//!   `put_record j Name=a n …` → `create_local_record` →
//!   `i_create_local_native_record`, `emu/instrs.tab:1358` →
//!   `erl_create_local_native_record`, `erl_record.c:254-354`). **No export
//!   requirement**: C reads the definition straight out of the `Recs`-derived
//!   load-time literal.
//! * a **`{Module,Name}`** pair — the generic `i_create_native_record`
//!   (`emu/instrs.tab:1373`) → `erl_create_native_record` (`erl_record.c:356-410`),
//!   which resolves the definition through the code server's record table
//!   (`erts_record_find_entry`, `:376`) and **requires `is_exported == am_true`**
//!   (`:401`); otherwise it raises `badrecord` with the `Id` term as the reason
//!   (`:407-409`). When the `Module` **is** the running module, C still takes this
//!   path — so an own record that is not exported (or not defined) is a
//!   `badrecord`, never a silent create.
//!
//! Split out of `mod.rs` (which would otherwise pass its ~1000-line target) and
//! kept together because the pieces — the definition resolver, the instance writer
//! and the two failure builders — are one story. The shared operand/failure helpers
//! ([`fail_step`], [`read_atom`], [`read_record_id`]) stay in `mod.rs`.
//!
//! # Deviations from C (documented, never silent)
//!
//! There is **no `code:ensure_loaded/1` trap** here (`erl_create_native_record_jit`,
//! `erl_record.c:412-507`, and its continuation `erl_finish_create_record_jit`,
//! `:509-565`): that is the "the defining module is not loaded yet ⇒ load it, then
//! retry the create" half, and this runtime has no code server on the execution
//! path for it to call. The equivalent question — "can anyone answer for this
//! `{owner, name}`?" — is asked of the host's record table through
//! [`ExecutionContext::foreign_record_definition`], and a host that cannot answer
//! (`Ok(None)`) leaves the family's honest typed
//! `error::unsupported("… create: no record definition table for another module")`
//! in place. A definition is **never** guessed.
//!
//! # The atom namespace
//!
//! [`beam_term::Atom`] is a **table-local index**, and a definition the host answers
//! comes from the *host's* record table — not from the running module's load-time
//! `Recs` snapshot. Every name the family is about to write into a term (the
//! definition's record name and each field name) is therefore re-resolved in the
//! running process's **live** atom namespace first ([`live_atom`]); a name that
//! namespace cannot spell is a typed error, so a foreign table's index is never
//! written into a term (see `ExecutionContext::foreign_record_definition`).

use beam_common::Result;
use beam_loader::{Label, Module, Operand, RecordDefinition, NO_FAIL};
use beam_term::{make_atom, make_small, Atom, Eterm, NIL};

use super::failure::{fail_step_with_reason, RecordReason, BADFIELD};
use super::{fail_step, raw, read_atom, read_record_id, RecordId};
use crate::context::{ExecutionContext, RootSet};
use crate::error;
use crate::interpreter::LabelMap;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::Step;

/// The atom `novalue` (`am_novalue`, `EXC_NOVALUE`, `error.h:174` /
/// `error_atom[22]`, `atom.names:539`).
const NOVALUE: &str = "novalue";

/// A create form's resolved definition, plus the **owner** atom its instance's
/// definition block must carry.
///
/// The owner is not a field of [`RecordDefinition`]: for the atom-`Id` (local) form
/// it is the running module, and for the `{Module,Name}` form it is the `Id`'s own
/// `Module` atom — which is what C stores in `defp->module`
/// (`erl_create_local_native_record`, `erl_record.c:345`).
struct Resolved {
    owner: Atom,
    definition: RecordDefinition,
}

/// The outcome of resolving a create form's definition.
enum Resolution {
    /// A definition the instance can be built from.
    Create(Resolved),
    /// The C's `badrecord`, or a live `fail` label — the final [`Step`].
    Failed(Step),
}

/// `i_create_local_native_record` (`emu/instrs.tab:1358-1371`) and
/// `i_create_native_record` (`:1373-1395`) — the **create** form of `put_record`
/// (`Src` is the literal `nil`, `emu/ops.tab:1648`).
///
/// Reproduces `erl_create_local_native_record` (`erl_record.c:254-353`) branch for
/// branch, on the resolved definition rather than on C's `def_literal`
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
/// failure path ([`create_failure`] / [`create_badrecord`]).
///
/// # The record's identity (`Id`)
///
/// [`resolve`] reproduces `emu/ops.tab:1648-1651`'s two shapes and C's export rules;
/// see the module docs. A definition no host can answer for is a typed
/// `error::unsupported`, never a guessed definition.
pub(super) fn create_native_record(
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
    let Resolved { owner, definition } = match resolve(
        machine, module, labels, ctx, instruction, fail, id,
    )? {
        Resolution::Create(resolved) => resolved,
        Resolution::Failed(step) => return Ok(step),
    };

    // The record name and every field name are re-resolved in the running
    // process's live atom namespace **before** any of them can reach a term (see
    // [`live_atom`]). `declared` holds the definition's fields in **declaration**
    // order (the order `write_record` takes), with the validated key atom.
    live_atom(ctx, module, definition.name, instruction, "name")?;
    let mut declared: Vec<(Atom, Option<Eterm>)> = Vec::with_capacity(definition.fields.len());
    for field in &definition.fields {
        declared.push((
            live_atom(ctx, module, field.name, instruction, "field name")?,
            field.default,
        ));
    }

    // Ascending key order — `defp->keys[i]`, the same atom-index order
    // `beam_term::record::write_record` sorts the block's keys into.
    let mut order: Vec<usize> = (0..declared.len()).collect();
    order.sort_by_key(|&index| declared[index].0);

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
        beam_term::record_block_words(declared.len()),
        RootSet::ALL_LIVE,
    )?;

    // `values[declaration index]` — the block is built in declaration order and
    // `write_record` re-sorts it exactly as the C's `qsort` does. Every update value
    // is read from the machine's POST-collection state (i.e. after the reservation).
    let mut values: Vec<Eterm> = vec![NIL; declared.len()];
    let mut absent: Option<Atom> = None;
    let mut position = 0usize;
    for &index in &order {
        let (key, default) = declared[index];
        if position < sorted.len() && sorted[position].0 == key {
            // `GetSource(new_p[1], *hp)` — the value is read only once its field
            // resolved, exactly as the C skips the other branch.
            values[index] = read_operand(machine, ctx, sorted[position].1)?;
            position += 1;
            continue;
        }
        match default {
            // A default is either an immediate or a term in the module's immutable
            // literal area (`beam_loader::LiteralArea`, its own `beam_alloc::Heap`) —
            // never on the process heap — so no collection relocates it and the value
            // copied here is valid before or after the reservation.
            Some(value) => values[index] = value,
            None => {
                // `if (is_value(res)) { p->fvalue = defp->keys[i]; … }`
                absent = absent.or(Some(key));
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
            owner,
            definition.name,
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
            owner,
            definition.name,
            field,
        );
    }

    let fields: Vec<(Eterm, Eterm)> = declared
        .iter()
        .zip(&values)
        .map(|((key, _), value)| (make_atom(*key), *value))
        .collect();
    // `hp` is still valid: nothing has allocated from the BEAM heap since the
    // reservation (every `read_operand` above is a pure machine read, save for a
    // non-small integer operand's **unrooted** `alloc_words`, which never collects).
    let record = write_created_record(module, owner, &definition, &fields, hp)?;
    write_operand(machine, dst, record)?;
    Ok(Step::Next)
}

/// Resolve the create form's `Id` operand to the definition it names — the branch
/// structure of `emu/ops.tab:1648-1651` on top of
/// `erl_create_local_native_record` / `erl_create_native_record`
/// (`erl_record.c:254-410`).
///
/// 1. An **atom** `Id` is the running module's own record
///    (`Module::record`), with **no** export requirement (the local opcode,
///    `erl_record.c:254`); a name the module does not define keeps the family's
///    typed `error::unsupported`.
/// 2. A **`{Module,Name}`** `Id` whose `Module` **is** the running module goes
///    through the generic form, which C answers from the record table and gates on
///    `defp->is_exported == am_true` (`:401`) — so a non-exported **or absent** own
///    record here is a `badrecord`, never a silent create.
/// 3. A **`{Module,Name}`** naming **another** module asks the host
///    ([`ExecutionContext::foreign_record_definition`]): `Ok(Some(def))` with
///    `def.is_exported` builds, `Ok(Some(def))` without it is the C's `badrecord`
///    (`:407-409`), and `Ok(None)` — no record table on this host, or a module it
///    cannot answer for — is the honest typed
///    `error::unsupported("… create: no record definition table for another
///    module")` (the `code:ensure_loaded/1` half of C has no counterpart here; see
///    the module docs).
fn resolve(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    ctx: &mut dyn ExecutionContext,
    instruction: &str,
    fail: Label,
    id: &Operand,
) -> Result<Resolution> {
    match read_record_id(machine, ctx, id, instruction)? {
        RecordId::Local(name) => {
            let definition = module.record(name).cloned().ok_or_else(|| {
                let name = module.atom_name(name).unwrap_or("<unnamed atom>");
                error::unsupported(&format!(
                    "{instruction}: create: the module defines no record named '{name}'"
                ))
            })?;
            Ok(Resolution::Create(Resolved {
                owner: module.name,
                definition,
            }))
        }
        RecordId::Foreign { module: owner, name } => {
            let found = if owner == module.name {
                // The running module's own `Recs` table is exactly what C's record
                // table holds for it, including the `is_exported` flag.
                module.record(name).cloned()
            } else {
                match ctx.foreign_record_definition(owner, name)? {
                    Some(definition) => Some(definition),
                    None => {
                        return Err(error::unsupported(&format!(
                            "{instruction}: create: no record definition table for another module"
                        )))
                    }
                }
            };
            match found {
                Some(definition) if definition.is_exported => Ok(Resolution::Create(Resolved {
                    owner,
                    definition,
                })),
                // `defp->is_exported != am_true`, or no definition at all for a
                // module that *is* loaded: `p->fvalue = id; p->freason =
                // EXC_BADRECORD` (`erl_record.c:407-409`) ⇒ a live `fail` label
                // branches, and `NO_FAIL` raises class `error` with the reason
                // **`{badrecord, Id}`** — the `Id` term itself, deliberately not the
                // `{Tag, {{Module,Name}, Field}}` shape [`create_failure`] builds.
                _ => fail_step_with_reason(
                    machine,
                    module,
                    labels,
                    fail,
                    instruction,
                    RecordReason::BadRecord { src: id },
                    ctx,
                )
                .map(Resolution::Failed),
            }
        }
    }
}

/// Re-resolve one of a definition's name atoms in the running process's **live
/// atom namespace**, returning the caller's own index.
///
/// [`beam_term::Atom`] is a *table-local index*, and the definition a host answers
/// through [`ExecutionContext::foreign_record_definition`] comes from **that host's**
/// record table — a different table from the running module's load-time snapshot.
/// Writing such an index into a term unverified could name a different atom
/// entirely, so the family asks the live namespace seam
/// ([`ExecutionContext::atoms`], with the running module's snapshot as the fallback
/// `namespace_atom` also uses) to spell every name first. A name neither source can
/// spell is the typed [`crate::error::unsupported`] — never a substituted atom.
///
/// `role` names what is being resolved (`"name"`, `"field name"`) for the error.
fn live_atom(
    ctx: &dyn ExecutionContext,
    module: &Module,
    atom: Atom,
    instruction: &str,
    role: &str,
) -> Result<Atom> {
    if ctx
        .atoms(module)
        .name(atom)
        .or_else(|| module.atom_table.name(atom))
        .is_some()
    {
        return Ok(atom);
    }
    Err(error::unsupported(&format!(
        "{instruction}: create: a record definition {role} is not in the caller's atom namespace"
    )))
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
/// `owner`/`name` are the **definition's** identity — the running module for the
/// atom-`Id` form, the `Id`'s own `Module` when the definition came from the host —
/// so the reason names the record that actually failed. `tag` names the reason's own
/// tag (`badfield`/`novalue`).
///
/// The mechanics — resolving the tag and class atoms through the live namespace
/// (with the running module's snapshot as fallback) and laying the whole reason
/// graph out as **one** 9-word rooted reservation — are
/// [`fail_step_with_reason`]'s (`RecordReason::BadField`), shared with the update
/// and field-reader opcodes that raise the same shape.
fn create_failure(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    ctx: &mut dyn ExecutionContext,
    instruction: &str,
    fail: Label,
    tag: &str,
    owner: Atom,
    name: Atom,
    field: Atom,
) -> Result<Step> {
    if fail != NO_FAIL {
        return fail_step(machine, module, labels, fail, instruction, ctx);
    }
    // The create form's diagnostic names the `create` path it failed in
    // (`put_record: create: no 'novalue' atom in the module`).
    fail_step_with_reason(
        machine,
        module,
        labels,
        NO_FAIL,
        &format!("{instruction}: create"),
        RecordReason::BadField {
            tag,
            owner,
            name,
            field,
        },
        ctx,
    )
}

/// Build a fresh native-record instance from the resolved `definition` — the
/// `erl_create_local_native_record` result — into the **already-reserved**
/// construction block at `hp`, carrying `owner` as the definition's module.
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
/// computed by the bit-exact `beam_term::hashing::compute_record_definition_hash`
/// over the caller's atom table (the atoms in the block are caller-namespace
/// indices, whatever module the definition came from).
///
/// `hp` stays valid across the second write because neither `raw::record_definition`
/// nor `compute_record_definition_hash` allocates from the BEAM heap.
fn write_created_record(
    module: &Module,
    owner: Atom,
    definition: &RecordDefinition,
    fields: &[(Eterm, Eterm)],
    hp: *mut Eterm,
) -> Result<Eterm> {
    let (name, exported) = (definition.name, definition.is_exported);
    // `hp` owns `record_block_words(fields.len())` exclusively-owned, writable words
    // and `fields` are the declaration-order `(key, value)` pairs `write_record`
    // documents. `0` is the placeholder for the cached definition hash, which cannot
    // exist before the block does.
    let instance = raw::write_record(hp, owner, name, exported, fields, make_small(0))?;
    let definition_term = raw::record_definition(instance)
        .ok_or_else(|| error::unsupported("put_record: create: malformed record block"))?;
    let hash =
        beam_term::hashing::compute_record_definition_hash(definition_term, &module.atom_table)?;
    // The same block at `hp` — nothing allocated it away in between (neither
    // `record_definition` nor `compute_record_definition_hash` allocates) — and the
    // same inputs, plus the real cached definition hash; every word is rewritten.
    raw::write_record(hp, owner, name, exported, fields, make_small(hash as i64))
}
