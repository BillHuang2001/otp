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

//! The remaining faithful `ets:*` verbs: the fix flag (`safe_fixtable/2`), the
//! name registry (`whereis/1`, `rename/2`), the defaulted element update
//! (`update_element/4`), the compiled-spec predicate (`is_compiled_ms/1`) and the
//! two underscore-internal count verbs (`internal_delete_all/2`,
//! `internal_select_delete/2`).
//!
//! They share the parent module's handle contract ([`crate::access::make_ref_thing`]
//! as the M1 tid, [`super::take_table`]/[`super::restore_table`] around every op)
//! and its [`super::TableRegistry`], so they are a child of `ets`.
//!
//! # The table argument is a handle *or* a `named_table` name
//!
//! The oracle accepts either a tid or, for a table created with `named_table`, its
//! **name atom** as the table argument of every verb (`db_get_table` consults the
//! meta name table for an atom). [`super::resolve::resolve_handle`] reproduces that
//! for the verbs registered here *and* for the parent module's keys: a handle ref
//! thing resolves through [`crate::access::ref_thing_id`], an atom through
//! [`super::TableRegistry::lookup_name`], and anything else — including the atom
//! *name field* of a table that was not created with `named_table`, or the
//! `{Name, Node}` 2-tuple of an older OTP — is `badarg` (verified:
//! `ets:safe_fixtable(justname, true)` is `badarg` for `ets:new(justname, [set])`,
//! while it is `true` for a `named_table`).
//!
//! # Access gating
//!
//! `safe_fixtable/2` is a **read** while `update_element/4`, `rename/2`,
//! `internal_delete_all/2` and `internal_select_delete/2` are **writes** (verified
//! against the oracle: for the non-owner of a `private` table all four are
//! `badarg`; for the non-owner of a `protected` table `safe_fixtable/2` succeeds
//! while e.g. `rename/2` is `badarg`), so every one of them takes its table through
//! [`super::take_table_gated`] as the calling process with the matching
//! [`super::resolve::Gate`]. `is_compiled_ms/1` names no table at all (its argument
//! is a compiled spec), and `whereis/1` only maps a name to a tid — neither touches
//! a table.
//!
//! # The three count/rename verbs
//!
//! * **`rename/2`** — see [`rename_2`]: the name write goes through
//!   [`beam_ets::Table::rename`], which owns the conflict rule, while this module
//!   owns the name-registry rekey (the `named_table` case) and the oracle's
//!   asymmetric return value.
//! * **`internal_delete_all/2`** / **`internal_select_delete/2`** — the two
//!   underscore-internal helpers that the stdlib `ets:delete_all_objects/1` and
//!   `ets:select_delete/2` are wrappers over; both answer the number of deleted
//!   objects and are write-gated.
//!
//! # The verbs served by sibling child modules, and the remaining gaps
//!
//! The verbs that are not registered here are either served by a sibling child
//! module or an honest typed gap:
//!
//! * `ets:select_reverse/1,2,3` lives in [`super::reverse`];
//! * the `*_lookup` family (`ets:first_lookup/1`, `ets:last_lookup/1`,
//!   `ets:next_lookup/2`, `ets:prev_lookup/2`) lives in [`super::nav`];
//! * `ets:match_spec_run_r/3` lives in [`super::specs`];
//! * the ownership verbs `ets:give_away/3` and `ets:setopts/2` live in
//!   [`super::adopt`].
//!
//! The remaining key is a genuine typed gap:
//!
//! * **`ets:internal_request_all/0`** — the helper `ets.erl`'s own `ets:all/0`
//!   drives: it mints a fresh `make_ref()` and has **each scheduler** reply
//!   `{Ref, Tables}`, so `ets.erl`'s `receive_all/3` waits for
//!   `system_info(schedulers)` messages. A `BifFn` can neither mint a reference nor
//!   send messages, and a single reply would leave that `receive` **blocked
//!   forever** — so it stays a typed gap. (`ets:all/0` itself is served directly in
//!   [`super::adopt`].)
//!
//! It is registered through the `RuntimeHooks` stub in `src/runtime/other.rs`.
//! `ets:setopts/2`'s `{protection, P}` sub-option is **not** a gap: [`super::adopt`]
//! applies it through the owner-gated `Table::setopts(Some(owner), SetOpt::Access(P))`.
//!
//! `ets:fun2ms/1` and `ets:repair_continuation/2` are **not** `bif.tab` entries:
//! they are pure `stdlib/src/ets.erl` functions (a `parse_transform` helper and a
//! continuation-tuple rewriter), so the registry can never dispatch them and they
//! stay unregistered.
//!
//! Everything registered here is pinned against the system `erl` (OTP 29.1.1 /
//! erts-17.1) — see the in-module tests below and `tests/ets_extra.rs`.

use beam_common::Result;
use beam_ets::{compile, RenameOutcome, Table, TableKind};
use beam_term::{atom_val, Eterm};

use crate::access::{
    is_atom, is_tuple, make_ref_thing, ref_thing_id, tuple_element, tuple_size,
};
use crate::context::BifContext;
use crate::error::{badarg, system_limit, unsupported_bif};
use crate::registry::BifRegistry;

use super::resolve::{resolve_handle, Gate};
use super::{parse_updates, restore_table, take_table_gated};

/// Registers the remaining faithful `ets:*` verbs (see the module docs for the
/// scope and for the unregistered list).
///
/// The `rename/2`, `internal_delete_all/2` and `internal_select_delete/2`
/// registrations here are the live ones: `src/runtime/other.rs` registers only the
/// `ets:internal_request_all/0` typed gap, so no key registered
/// here is shadowed.
pub(super) fn register(reg: &mut BifRegistry) {
    reg.register("ets", "safe_fixtable", 2, safe_fixtable_2);
    reg.register("ets", "whereis", 1, whereis_1);
    reg.register("ets", "rename", 2, rename_2);
    reg.register("ets", "update_element", 4, update_element_4);
    reg.register("ets", "is_compiled_ms", 1, is_compiled_ms_1);
    reg.register("ets", "internal_delete_all", 2, internal_delete_all_2);
    reg.register("ets", "internal_select_delete", 2, internal_select_delete_2);
}

/// The boolean argument of `ets:safe_fixtable/2`.
///
/// The oracle compares the argument by **atom identity** against `true`/`false`
/// (the C `BIF_ARG_2 == am_true`), so any other term — including another atom — is
/// `badarg`, never a coerced `false`.
fn boolean_arg(ctx: &BifContext<'_>, term: Eterm) -> Result<bool> {
    if !is_atom(term) {
        return Err(badarg());
    }
    match ctx.atoms().name(atom_val(term)) {
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        _ => Err(badarg()),
    }
}

// -------------------------------------------------------------- safe_fixtable

/// `ets:safe_fixtable(Tab, Bool)` — sets (or clears) the table's *fix* flag and
/// returns `true`.
///
/// While fixed, the storage of every object removed from the table is held until
/// the table is unfixed, so a traversal that still holds a deleted key's term never
/// observes reclaimed words ([`Table::set_fixation`] owns that machinery). The
/// return value is the atom `true` for **both** booleans (verified), and a
/// non-boolean second argument — or a table argument that names no table — is
/// `badarg`. Read-gated as the calling process: only a `private` table denies a
/// non-owner (verified — a `protected` table's non-owner may fix it).
fn safe_fixtable_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve_handle(ctx, args[0])?;
    let fix = boolean_arg(ctx, args[1])?;
    let mut table = take_table_gated(ctx, handle, Gate::Read)?;
    table.set_fixation(fix);
    restore_table(ctx, handle, table)?;
    ctx.boolean(true)
}

// -------------------------------------------------------------------- whereis

/// `ets:whereis(Name)` — the tid of the `named_table` registered under `Name`, or
/// the atom `undefined`.
///
/// Only tables created with the `named_table` option are registered (the oracle's
/// meta name table): a table whose `name` field is `Name` but which was created
/// without `named_table` is **not** found (verified). A non-atom argument is
/// `badarg`.
///
/// The tid is the same handle ref thing the table's BIFs take and the same term
/// `ets:info(T, id)` renders, so `ets:whereis(Name) =:= ets:info(T, id)` holds, as
/// in the oracle (verified via `erl`). (The oracle's own
/// `ets:new(Name, [named_table])` returns the *name atom* while this BIF answers
/// its **tid** — exactly the oracle's asymmetry: there `new/2` returns the name
/// atom too and `whereis/1` the tid reference.)
fn whereis_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let term = args[0];
    if !is_atom(term) {
        return Err(badarg());
    }
    let name = atom_val(term);
    let handle = {
        let tables = ctx.tables().ok_or_else(unsupported_bif)?;
        tables.lookup_name(name)
    };
    match handle {
        Some(handle) => make_ref_thing(ctx.heap(), handle),
        None => ctx.intern_atom("undefined"),
    }
}

// ---------------------------------------------------------------- rename/2

/// `ets:rename(Tab, Name)` — gives the table `Name` and answers the table's
/// **identifier after the rename**: the new name atom for a `named_table`, and the
/// unchanged handle for a table created without `named_table`.
///
/// The name write itself (the conflict rule, the `named_table` flag check and the
/// name-field rewrite) is [`beam_ets::Table::rename`]; this BIF owns the parts that
/// need the crate's name registry and the argument/return contract:
///
/// * `Name` must be an atom (`badarg` otherwise, and the table is untouched);
/// * `Tab` must resolve to a live table — a handle ref thing, or the name atom of a
///   `named_table` — else `badarg` (verified: a deleted table's tid, an unknown name
///   atom and a non-table term are all `badarg`);
/// * the call is **write**-gated as the calling process (the C's
///   `DB_BIF_GET_TABLE(tb, DB_WRITE, …)`), so a non-owner may rename a `public`
///   table but not a `protected`/`private` one (verified: a non-owner's rename of a
///   protected `named_table` is `badarg` and nothing changes);
/// * on [`RenameOutcome::Named`] the registry is **rekeyed** — the previous name is
///   dropped (so `ets:whereis(Old)` becomes `undefined` and a fresh
///   `ets:new(Old, [named_table])` may take it) and the new name maps to the same
///   table id — and the new **name atom** is returned;
/// * on [`RenameOutcome::Unnamed`] the registry is left alone (the table was never
///   in it) and the **original handle term** is returned, so `R =:= Tab` holds
///   (verified) while `ets:info(Tab, name)` reports the new name and
///   `ets:whereis(New)` stays `undefined`.
///
/// The `taken` predicate handed to [`Table::rename`] is the name registry's current
/// membership: the oracle's `insert_named_tab` fails on *any* registry hit,
/// including this table's own name (verified: renaming a `named_table` to the name
/// it already has, or to another live `named_table`'s name, is `badarg` and leaves
/// both tables and both registrations untouched). The table is taken out of the
/// table registry but its **name entry is deliberately still there** when the
/// predicate is asked, so a self-rename is caught by the predicate as well as by
/// [`Table::rename`]'s own check.
fn rename_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve_handle(ctx, args[0])?;
    let name_term = args[1];
    if !is_atom(name_term) {
        return Err(badarg());
    }
    let name = atom_val(name_term);
    let taken = {
        let tables = ctx.tables().ok_or_else(unsupported_bif)?;
        tables.lookup_name(name).is_some()
    };
    let mut table = take_table_gated(ctx, handle, Gate::Write)?;
    // The table is put back on the error path too (never lost): a refused rename
    // changes nothing but must still leave the table in the registry.
    match table.rename(name, |_| taken) {
        Ok(RenameOutcome::Named { previous_name }) => {
            {
                let tables = ctx.tables().ok_or_else(unsupported_bif)?;
                if let Some(previous_name) = previous_name {
                    tables.remove_name(previous_name);
                }
                tables.insert_name(name, handle);
            }
            restore_table(ctx, handle, table)?;
            Ok(name_term)
        }
        Ok(RenameOutcome::Unnamed) => {
            restore_table(ctx, handle, table)?;
            Ok(args[0])
        }
        Err(err) => {
            restore_table(ctx, handle, table)?;
            Err(err)
        }
    }
}
// ---------------------------------------------------------- update_element /4
/// `ets:update_element(Tab, Key, Spec, Default)` — as `ets:update_element/3`, but
/// `Default` supplies the object to update when `Key` is absent.
///
/// Mirrors the C `ets_update_element_4` + `do_update_element` (erts-17.1):
///
/// * `Default` must be a tuple with an arity of at least the table's `keypos`, and
///   an invalid one is `badarg` **even when `Key` exists**;
/// * only `set`/`ordered_set` hold updatable objects (`bag`/`duplicate_bag` are
///   `badarg`);
/// * with `Key` present, `Spec` is validated and applied to the stored object and
///   the result is `true`;
/// * with `Key` **absent**, the object deposited is `Default` with the element at
///   `keypos` replaced by `Key` (the default's own key slot is ignored — verified),
///   `Spec` is validated against *that* object's arity, and the result is `true`
///   (unlike `/3`, which answers `false` for an absent key);
/// * a malformed `Spec` (a non-list, an improper list, a non-2-tuple entry, a
///   non-integer position), `Pos == keypos`, `Pos == 0` or `Pos` beyond the object's
///   arity leaves the table untouched and is `badarg`.
///
/// Write-gated as the calling process: a non-owner is denied by a `protected` or
/// `private` table (verified).
fn update_element_4(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve_handle(ctx, args[0])?;
    let mut table = take_table_gated(ctx, handle, Gate::Write)?;
    let outcome = update_element_with_default(&mut table, args[1], args[2], args[3], ctx);
    // The table is restored on the error path too (never lost).
    restore_table(ctx, handle, table)?;
    match outcome {
        Ok(updated) => ctx.boolean(updated),
        Err(err) => Err(err),
    }
}

/// The body of [`update_element_4`], over an owned table.
fn update_element_with_default(
    table: &mut Table,
    key: Eterm,
    spec: Eterm,
    default: Eterm,
    ctx: &mut BifContext<'_>,
) -> Result<bool> {
    let keypos = table.keypos();
    // The default is validated first, as the C does — an invalid one fails the call
    // even when `key` is present.
    if !is_tuple(default) || tuple_size(default) < keypos {
        return Err(badarg());
    }
    match table.kind() {
        TableKind::Set | TableKind::OrderedSet => {}
        TableKind::Bag | TableKind::DuplicateBag => return Err(badarg()),
    }
    let updates = parse_updates(spec)?;
    // `Table::update_element` needs the atom table *and* the caller heap at once;
    // the context lends them through two conflicting borrows, so it is given a
    // clone of the atom table (identical indices, and the call only reads it — the
    // same pattern the sibling `update_element/3` uses).
    let atoms = ctx.atoms().clone();
    if table.member(key, &atoms) {
        return table.update_element(key, &updates, &atoms, ctx.heap());
    }
    // Absent key: the default object (with its key slot replaced by `key`) is what
    // gets updated and deposited. Positions are checked against *its* arity, and
    // nothing is written until every one of them has validated.
    let arity = tuple_size(default);
    for &(pos, _) in &updates {
        if pos == keypos || pos == 0 || pos > arity {
            return Err(badarg());
        }
    }
    let mut elems: Vec<Eterm> = (0..arity).map(|i| tuple_element(default, i)).collect();
    elems[keypos - 1] = key;
    for &(pos, value) in &updates {
        elems[pos - 1] = value;
    }
    let object = ctx.tuple(&elems)?;
    table.insert(object, ctx.atoms())?;
    Ok(true)
}

// ------------------------------------------------------------- is_compiled_ms

/// `ets:is_compiled_ms(Term)` — whether `Term` is a compiled match spec, i.e. an
/// `ets:match_spec_compile/1` result.
///
/// Every other term is the atom `false`, exactly as the oracle's
/// `erts_db_get_match_prog_binary` predicate (verified: a raw spec list, an atom,
/// a small integer, a plain reference and a table tid are all `false`).
fn is_compiled_ms_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    // A non-handle is not a compiled spec; only a reference can be one. (No
    // registry installed means no compiled spec exists, so the handle cannot be
    // classified — the module's usual typed raise, never a fabricated `false`.)
    let Ok(handle) = ref_thing_id(args[0]) else {
        return ctx.boolean(false);
    };
    let is_spec = {
        let tables = ctx.tables().ok_or_else(unsupported_bif)?;
        tables.contains_spec(handle)
    };
    ctx.boolean(is_spec)
}

// ------------------------------------------- internal_delete_all / select_delete

/// `ets:internal_delete_all(Tab, Holder)` — the underscore-internal helper the
/// stdlib `ets:delete_all_objects/1` is a wrapper over, plus the tiny **trap
/// protocol** the C uses for huge tables.
///
/// Mirrors `ets_internal_delete_all_2` (`erl_db.c:3239`) exactly:
///
/// * `Holder` is the atom `undefined` ⇒ the table is emptied and the **number of
///   deleted objects** is returned as a small integer (verified: 3 objects ⇒ `3`,
///   an empty table ⇒ `0`);
/// * `Holder` is **any other term** ⇒ nothing is touched and that term is returned
///   **verbatim** (the C's `nitems_holder`: a trapped call re-enters with the count
///   it had already computed, and the caller of the trap sees the value again) —
///   verified: `internal_delete_all(T, 42)` ⇒ `42` with the table untouched, and an
///   atom holder ⇒ that atom;
/// * the table is resolved with `DB_WRITE` (`erl_db.c:3247`), so an unknown/deleted
///   table is `badarg` **even when the holder is not `undefined`** (verified), and a
///   non-owner of a `protected`/`private` table is denied.
///
/// The object count is read **before** the clear ([`Table::size`] then
/// [`Table::delete_all_objects`]), which is the count the C reports.
fn internal_delete_all_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve_handle(ctx, args[0])?;
    if !is_undefined_atom(ctx, args[1]) {
        // The trap protocol: a holder that is not `undefined` is the answer.
        return Ok(args[1]);
    }
    let mut table = take_table_gated(ctx, handle, Gate::Write)?;
    let deleted = table.size();
    table.delete_all_objects();
    restore_table(ctx, handle, table)?;
    count_term(ctx, deleted)
}

/// `ets:internal_select_delete(Tab, MatchSpec)` — the underscore-internal helper
/// the stdlib `ets:select_delete/2` is a wrapper over: it deletes every object the
/// raw match spec selects with the body `true` and returns how many were deleted
/// (`ets_internal_select_delete_2`, `erl_db.c:3438`).
///
/// `MatchSpec` is a **raw** clause list compiled by [`beam_ets::compile`] — a
/// compiled spec from `ets:match_spec_compile/1` is not accepted (the oracle's
/// `badarg`), and a spec outside the supported subset is `badarg` too. The table is
/// resolved with `DB_WRITE`, so an unknown/deleted table or a denied write is
/// `badarg`. Verified: three objects and
/// `[{{1, '$1'}, [], [true]}]` ⇒ `1` with `{1, a}` gone, and re-running it ⇒ `0`.
fn internal_select_delete_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    // The spec's guards/bodies may compute arithmetic; install this crate's
    // arbitrary-precision `ArithHook` over `beam_ets` for the traversal
    // (`super::arith_hook`).
    let _arith = super::arith_hook::PrecisionArith::install();
    let handle = resolve_handle(ctx, args[0])?;
    let spec = compile(args[1], ctx.atoms())?;
    let mut table = take_table_gated(ctx, handle, Gate::Write)?;
    // `Table::select_delete` needs the atom table *and* the caller heap at once; the
    // context lends them through two conflicting borrows, so it is given a clone of
    // the atom table (identical indices, and the call only reads it — the same
    // pattern `ets:select_delete/2` in the `matches` sibling uses).
    let atoms = ctx.atoms().clone();
    let outcome = table.select_delete(&spec, &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    count_term(ctx, outcome?)
}

/// Whether `term` is the atom `undefined` — the C's `BIF_ARG_2 == am_undefined`
/// identity test (no other term, including another atom, is the undefined atom).
fn is_undefined_atom(ctx: &BifContext<'_>, term: Eterm) -> bool {
    is_atom(term) && ctx.atoms().name(atom_val(term)) == Some("undefined")
}

/// A deleted-object count as a small integer — the C's `make_small(nitems)` in both
/// internal verbs above (and the same rendering the sibling `matches` module gives
/// `ets:select_delete/2`'s count). An unrepresentable count is the typed
/// [`system_limit`], never a truncated value.
fn count_term(ctx: &mut BifContext<'_>, count: usize) -> Result<Eterm> {
    let count = i64::try_from(count).map_err(|_| system_limit())?;
    ctx.int(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{signed_value, AtomTable, NIL};

    use crate::access::is_reference;
    use crate::ets::TableRegistry;
    /// The names the tests below need. The **first** name is the harmless sentinel
    /// at atom index `0` (`atom_val` of a term that is not an atom aliases index `0`,
    /// so no real name may live there).
    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "sentinel",
            "true",
            "false",
            "undefined",
            "set",
            "named_table",
            "public",
            "old",
            "new",
            "othertab",
            "plain",
            "whatever",
            "mytab",
            "dead",
            "pub",
            "nosuch",
            "some_trap",
            "a",
            "id",
            "name",
            "size",
            "$1",
        ]);
        atoms
    }

    /// The per-test `(heap, atoms, registry)` a `BifContext` is built over.
    ///
    /// Returned as three owned values (rather than a struct holding the context) so a
    /// test keeps the shared `&atoms` borrow available alongside `ctx` — the
    /// atom-name assertions read the atom table directly.
    fn parts() -> (Heap, AtomTable, TableRegistry) {
        (
            Heap::new(65536).expect("test heap"),
            seeded_atoms(),
            TableRegistry::new(),
        )
    }

    /// `ets:new(Name, [set | Extra…])` through the parent module's `new_2`, returning
    /// the handle term the oracle `ets:new/2` would hand out: the **name atom** for a
    /// `named_table`, a handle reference otherwise.
    fn new_table(ctx: &mut BifContext<'_>, name: &str, extra: &[&str]) -> Eterm {
        let name = ctx.atom(name).expect("seeded");
        let mut options = vec![ctx.atom("set").expect("seeded")];
        for option in extra {
            options.push(ctx.atom(option).expect("seeded"));
        }
        let options = ctx.list(&options).expect("fits");
        super::super::new_2(ctx, &[name, options]).expect("ets:new")
    }

    /// The `{Key, Value}` 2-tuple the delete specs below match.
    fn int_pair(ctx: &mut BifContext<'_>, key: i64, value: &str) -> Eterm {
        let key = ctx.int(key).expect("fits");
        let value = ctx.atom(value).expect("seeded");
        ctx.tuple(&[key, value]).expect("fits")
    }

    /// Inserts one `{Key, a}` object per entry of `keys`.
    fn insert_objects(ctx: &mut BifContext<'_>, handle: Eterm, keys: &[i64]) {
        let objects: Vec<Eterm> = keys.iter().map(|&key| int_pair(ctx, key, "a")).collect();
        let list = ctx.list(&objects).expect("fits");
        super::super::insert_2(ctx, &[handle, list]).expect("ets:insert");
    }

    /// The value of one `ets:info/2` item (the parent module's `info_2`).
    fn info(ctx: &mut BifContext<'_>, handle: Eterm, item: &str) -> Eterm {
        let item = ctx.atom(item).expect("seeded");
        super::super::info_2(ctx, &[handle, item]).expect("ets:info/2")
    }

    /// The raw match spec `[{{1, '$1'}, [], [true]}]` — the oracle probe
    /// `ets:internal_select_delete(T, Spec)` on a set holding `{1,a},{2,a},{3,a}`
    /// answers `1` and leaves `{2,a},{3,a}`.
    fn delete_key1_spec(ctx: &mut BifContext<'_>) -> Eterm {
        let key = ctx.int(1).expect("fits");
        let var = ctx.atom("$1").expect("seeded");
        let head = ctx.tuple(&[key, var]).expect("fits");
        let yes = ctx.atom("true").expect("seeded");
        let body = ctx.list(&[yes]).expect("fits");
        let clause = ctx.tuple(&[head, NIL, body]).expect("fits");
        ctx.list(&[clause]).expect("fits")
    }

    /// Every key this module registers is reachable, exactly once each.
    #[test]
    fn the_registered_keys_include_the_new_verbs() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        for (function, arity) in [
            ("safe_fixtable", 2),
            ("whereis", 1),
            ("rename", 2),
            ("update_element", 4),
            ("is_compiled_ms", 1),
            ("internal_delete_all", 2),
            ("internal_select_delete", 2),
        ] {
            assert!(
                reg.contains("ets", function, arity),
                "missing ets:{function}/{arity}"
            );
        }
        assert_eq!(reg.len(), 7);
    }

    /// Oracle (probed live): for `T = ets:new(old, [set, named_table])`,
    /// `ets:rename(T, new)` answers the atom `new`, `ets:whereis(old)` becomes
    /// `undefined`, the new name resolves to the *same* table, and the table keeps
    /// its objects, its `name` field and its `named_table` flag. The freed old name
    /// can be reused for a fresh `named_table`.
    #[test]
    fn renaming_a_named_table_rekeys_the_name_registry() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        let old = ctx.atom("old").expect("seeded");
        let handle = new_table(&mut ctx, "old", &["named_table"]);
        assert_eq!(
            handle, old,
            "ets:new/2 answers the name atom for a named_table"
        );
        insert_objects(&mut ctx, handle, &[1, 2]);

        let new = ctx.atom("new").expect("seeded");
        let renamed = rename_2(&mut ctx, &[handle, new]).expect("ets:rename/2");
        assert_eq!(renamed, new, "a named rename answers the new name atom");

        // The old name left the registry (and, exactly as the oracle, the old name
        // atom is no longer a table argument at all)...
        let gone = whereis_1(&mut ctx, &[old]).expect("ets:whereis/1");
        assert_eq!(atoms.name(atom_val(gone)), Some("undefined"));
        let stale = info(&mut ctx, old, "name");
        assert_eq!(atoms.name(atom_val(stale)), Some("undefined"));
        // ...while the new name resolves to the very same table id.
        let found = whereis_1(&mut ctx, &[new]).expect("ets:whereis/1");
        assert!(is_reference(found), "the tid is a handle reference");
        let id = info(&mut ctx, new, "id");
        assert_eq!(ref_thing_id(id).expect("the tid"), ref_thing_id(found).expect("the tid"));
        // The rename changed nothing but the name.
        assert_eq!(atoms.name(atom_val(info(&mut ctx, new, "name"))), Some("new"));
        assert_eq!(
            atoms.name(atom_val(info(&mut ctx, new, "named_table"))),
            Some("true")
        );
        assert_eq!(signed_value(info(&mut ctx, new, "size")), 2);

        // The freed name may be taken by a fresh `named_table` (returns the atom).
        let fresh = new_table(&mut ctx, "old", &["named_table"]);
        assert_eq!(fresh, old);
    }

    /// Oracle (probed live): an unnamed table's rename **always** succeeds — it
    /// answers the *same* handle (`R =:= Tab`), rewrites the recorded name, and
    /// neither registers the table's name nor sets `named_table`. Renaming it to the
    /// name it already has is allowed too (there is no registry entry to conflict
    /// with).
    #[test]
    fn renaming_an_unnamed_table_answers_the_unchanged_handle() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        let handle = new_table(&mut ctx, "plain", &[]);
        assert!(
            is_reference(handle),
            "an unnamed table gets a handle reference"
        );

        let whatever = ctx.atom("whatever").expect("seeded");
        let renamed = rename_2(&mut ctx, &[handle, whatever]).expect("ets:rename/2");
        assert_eq!(renamed, handle, "R =:= Tab for a table without named_table");
        assert_eq!(
            atoms.name(atom_val(info(&mut ctx, handle, "name"))),
            Some("whatever")
        );
        assert_eq!(
            atoms.name(atom_val(info(&mut ctx, handle, "named_table"))),
            Some("false")
        );
        // Still not registered: `whereis/1` stays `undefined`.
        let miss = whereis_1(&mut ctx, &[whatever]).expect("ets:whereis/1");
        assert_eq!(atoms.name(atom_val(miss)), Some("undefined"));

        // A self-rename of an unnamed table is fine (no registry conflict at all).
        let again = rename_2(&mut ctx, &[handle, whatever]).expect("self rename");
        assert_eq!(again, handle);
    }

    /// Oracle (probed live): renaming a `named_table` to its **own** name, or to the
    /// name of **another live** `named_table`, is `badarg` and changes nothing
    /// (the C `insert_named_tab` fails on any registry hit).
    #[test]
    fn a_taken_name_is_badarg_and_changes_nothing() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        let old = ctx.atom("old").expect("seeded");
        let handle = new_table(&mut ctx, "old", &["named_table"]);
        assert!(matches!(
            rename_2(&mut ctx, &[handle, old]),
            Err(BeamError::BadArg)
        ));

        let othertab = ctx.atom("othertab").expect("seeded");
        let other = new_table(&mut ctx, "othertab", &["named_table"]);
        assert!(matches!(
            rename_2(&mut ctx, &[handle, othertab]),
            Err(BeamError::BadArg)
        ));

        // Neither the failed attempts nor the second table disturbed anything: the
        // first table is still registered under `old` and reachable by it.
        assert_eq!(atoms.name(atom_val(info(&mut ctx, old, "name"))), Some("old"));
        let found = whereis_1(&mut ctx, &[old]).expect("ets:whereis/1");
        assert!(is_reference(found));
        let still = whereis_1(&mut ctx, &[othertab]).expect("ets:whereis/1");
        assert!(is_reference(still));
        assert_eq!(
            atoms.name(atom_val(info(&mut ctx, other, "name"))),
            Some("othertab")
        );
    }

    /// Oracle (probed live): a name merely **recorded** by a table created *without*
    /// `named_table` is not a conflict — a `named_table` may take it (both tables then
    /// report the same `name`, while only the named one answers `whereis/1`).
    #[test]
    fn a_name_recorded_by_an_unnamed_table_is_not_taken() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        let plain = new_table(&mut ctx, "plain", &[]);
        let handle = new_table(&mut ctx, "old", &["named_table"]);
        let plain_atom = ctx.atom("plain").expect("seeded");

        let renamed = rename_2(&mut ctx, &[handle, plain_atom]).expect("ets:rename/2");
        assert_eq!(renamed, plain_atom, "the new name atom is the answer");
        assert_eq!(
            atoms.name(atom_val(info(&mut ctx, plain, "name"))),
            Some("plain"),
            "the unnamed table keeps its recorded name"
        );
        assert_eq!(
            atoms.name(atom_val(info(&mut ctx, plain_atom, "name"))),
            Some("plain"),
            "the name now resolves to the renamed named_table"
        );
        let found = whereis_1(&mut ctx, &[plain_atom]).expect("ets:whereis/1");
        assert!(is_reference(found));
    }

    /// Oracle (probed live): a non-atom name, and a table argument that names no live
    /// table — a bare unknown name, a non-table term, a deleted handle or a deleted
    /// `named_table`'s name — are all `badarg`, and a failed call leaves the surviving
    /// table untouched.
    #[test]
    fn a_non_atom_name_or_a_table_that_names_nothing_is_badarg() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        let handle = new_table(&mut ctx, "mytab", &[]);
        let name = ctx.atom("new").expect("seeded");
        let not_an_atom = ctx.int(42).expect("fits");
        let nosuch = ctx.atom("nosuch").expect("seeded");

        assert!(matches!(
            rename_2(&mut ctx, &[handle, not_an_atom]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            rename_2(&mut ctx, &[nosuch, name]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            rename_2(&mut ctx, &[not_an_atom, name]),
            Err(BeamError::BadArg)
        ));

        // A deleted table's handle, and a deleted `named_table`'s name, are `badarg`.
        let dead_handle = new_table(&mut ctx, "dead", &[]);
        super::super::delete_1(&mut ctx, &[dead_handle]).expect("ets:delete/1");
        assert!(matches!(
            rename_2(&mut ctx, &[dead_handle, name]),
            Err(BeamError::BadArg)
        ));
        let pub_atom = ctx.atom("pub").expect("seeded");
        let named_handle = new_table(&mut ctx, "pub", &["named_table"]);
        super::super::delete_1(&mut ctx, &[named_handle]).expect("ets:delete/1");
        assert!(matches!(
            rename_2(&mut ctx, &[pub_atom, name]),
            Err(BeamError::BadArg)
        ));

        // Every failed attempt left the surviving table exactly as it was.
        assert_eq!(
            atoms.name(atom_val(info(&mut ctx, handle, "name"))),
            Some("mytab")
        );
        assert_eq!(ref_thing_id(handle).expect("the handle"), {
            let id = info(&mut ctx, handle, "id");
            ref_thing_id(id).expect("the tid")
        });
    }

    /// The rename is **write**-gated, not owner-gated (verified live: a non-owner's
    /// `ets:rename(prot, other)` on a protected `named_table` is `badarg` and
    /// `ets:info(prot, name)` stays `prot`, while the owner may rename it). A
    /// `public` table accepts **any** writer, which is what distinguishes the write
    /// gate from an ownership check.
    #[test]
    fn a_non_owner_may_not_rename_a_protected_table() {
        let (mut heap, atoms, mut reg) = parts();
        let (protected, public) = {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
            (
                new_table(&mut ctx, "mytab", &[]),
                new_table(&mut ctx, "pub", &["public"]),
            )
        };

        // A different calling process.
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(2, 0)).with_tables(&mut reg);
        let name = ctx.atom("new").expect("seeded");
        assert!(matches!(
            rename_2(&mut ctx, &[protected, name]),
            Err(BeamError::BadArg)
        ));
        assert_eq!(
            atoms.name(atom_val(info(&mut ctx, protected, "name"))),
            Some("mytab"),
            "the protected table was left untouched"
        );
        // The write gate (not ownership) is the rule: a `public` table admits any
        // caller that may write it.
        let renamed = rename_2(&mut ctx, &[public, name]).expect("public rename");
        assert_eq!(renamed, public);
    }

    /// Oracle (probed live): `ets:internal_delete_all(T, undefined)` empties the table
    /// and answers the number of deleted objects (3 objects ⇒ `3`, an emptied table ⇒
    /// `0`), while **any other** holder is answered verbatim with the table untouched
    /// (the C's trap protocol). The table is resolved with write access, so an
    /// unknown/deleted table is `badarg` even when the holder is not `undefined`.
    #[test]
    fn internal_delete_all_answers_the_deleted_count_or_the_holder() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        let handle = new_table(&mut ctx, "mytab", &[]);
        insert_objects(&mut ctx, handle, &[1, 2, 3]);
        let undefined = ctx.atom("undefined").expect("seeded");
        let count =
            internal_delete_all_2(&mut ctx, &[handle, undefined]).expect("internal_delete_all");
        assert_eq!(signed_value(count), 3);
        assert_eq!(signed_value(info(&mut ctx, handle, "size")), 0);
        // An empty table answers 0.
        let count =
            internal_delete_all_2(&mut ctx, &[handle, undefined]).expect("internal_delete_all");
        assert_eq!(signed_value(count), 0);

        // The trap protocol: a holder other than `undefined` is the answer, verbatim.
        insert_objects(&mut ctx, handle, &[1]);
        let holder = ctx.int(42).expect("fits");
        assert_eq!(
            internal_delete_all_2(&mut ctx, &[handle, holder]).expect("trap holder"),
            holder
        );
        let trap = ctx.atom("some_trap").expect("seeded");
        assert_eq!(
            internal_delete_all_2(&mut ctx, &[handle, trap]).expect("trap holder"),
            trap
        );
        assert_eq!(
            signed_value(info(&mut ctx, handle, "size")),
            1,
            "a trap holder leaves the table untouched"
        );

        // An unknown name and a deleted handle are `badarg`, holder or not.
        let nosuch = ctx.atom("nosuch").expect("seeded");
        assert!(matches!(
            internal_delete_all_2(&mut ctx, &[nosuch, undefined]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            internal_delete_all_2(&mut ctx, &[nosuch, holder]),
            Err(BeamError::BadArg)
        ));
        let dead = new_table(&mut ctx, "dead", &[]);
        super::super::delete_1(&mut ctx, &[dead]).expect("ets:delete/1");
        assert!(matches!(
            internal_delete_all_2(&mut ctx, &[dead, undefined]),
            Err(BeamError::BadArg)
        ));
    }

    /// Oracle (probed live): `ets:internal_select_delete(T, [{{1,'$1'},[],[true]}])` on
    /// a set holding `{1,a},{2,a},{3,a}` answers `1` and leaves the other two objects;
    /// re-running it answers `0`. A table that names nothing, and a spec that is not a
    /// raw clause list, are `badarg`.
    #[test]
    fn internal_select_delete_answers_the_deleted_count() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        let handle = new_table(&mut ctx, "mytab", &[]);
        insert_objects(&mut ctx, handle, &[1, 2, 3]);
        let spec = delete_key1_spec(&mut ctx);

        let count =
            internal_select_delete_2(&mut ctx, &[handle, spec]).expect("internal_select_delete");
        assert_eq!(signed_value(count), 1);
        assert_eq!(signed_value(info(&mut ctx, handle, "size")), 2);
        // The surviving objects are exactly the untouched keys 2 and 3.
        let listed = super::super::tab2list_1(&mut ctx, &[handle]).expect("ets:tab2list/1");
        let objects = crate::print::proper_list(listed).expect("a proper list");
        let keys: Vec<i64> = objects
            .iter()
            .map(|&object| signed_value(tuple_element(object, 0)))
            .collect();
        assert_eq!(keys, vec![2, 3]);

        // Nothing left to delete.
        let count =
            internal_select_delete_2(&mut ctx, &[handle, spec]).expect("internal_select_delete");
        assert_eq!(signed_value(count), 0);

        // A table that names nothing, a spec that is not a raw clause list, and a
        // deleted table are `badarg`.
        let nosuch = ctx.atom("nosuch").expect("seeded");
        assert!(matches!(
            internal_select_delete_2(&mut ctx, &[nosuch, spec]),
            Err(BeamError::BadArg)
        ));
        let not_a_spec = ctx.int(1).expect("fits");
        assert!(matches!(
            internal_select_delete_2(&mut ctx, &[handle, not_a_spec]),
            Err(BeamError::BadArg)
        ));
        let dead = new_table(&mut ctx, "dead", &[]);
        super::super::delete_1(&mut ctx, &[dead]).expect("ets:delete/1");
        assert!(matches!(
            internal_select_delete_2(&mut ctx, &[dead, spec]),
            Err(BeamError::BadArg)
        ));
    }

    /// A count that cannot be a small integer is the typed `system_limit`, never a
    /// truncated value.
    #[test]
    fn an_unrepresentable_count_is_system_limit() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        assert_eq!(signed_value(count_term(&mut ctx, 0).expect("fits")), 0);
        assert_eq!(signed_value(count_term(&mut ctx, 7).expect("fits")), 7);
        assert!(matches!(
            count_term(&mut ctx, usize::MAX),
            Err(BeamError::SystemLimit)
        ));
    }
}