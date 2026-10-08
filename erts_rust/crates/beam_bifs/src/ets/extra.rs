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
//! name registry (`whereis/1`), the defaulted element update
//! (`update_element/4`) and the compiled-spec predicate (`is_compiled_ms/1`).
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
//! `safe_fixtable/2` is a **read** and `update_element/4` a **write** (verified
//! against the oracle: for the non-owner of a `private` table both are `badarg`;
//! for the non-owner of a `protected` table `safe_fixtable/2` succeeds while
//! `update_element/4` is `badarg`), so both take their table through
//! [`super::take_table_gated`] as the calling process. `is_compiled_ms/1` names no
//! table at all (its argument is a compiled spec), and `whereis/1` only maps a name
//! to a tid — neither touches a table.
//!
//! # Deliberately not registered (honest gaps)
//!
//! * **`ets:rename/2`** — the oracle answers the table's **tid** (the new name atom
//!   for a `named_table`, otherwise the handle it was given), not `true`; and a
//!   faithful rename must also rewrite the table's own name field
//!   (`ets:info(T, name)` reports the new name afterwards, verified). `beam_ets`
//!   exposes `Table::name()` but no setter, so the name field cannot be changed
//!   from here. Owner: a `beam_ets` task adding `Table::set_name`.
//! * **`ets:select_reverse/1,2,3`** — a *reverse* traversal of the table (the oracle
//!   walks an `ordered_set` in descending key order). `beam_ets` has no reverse
//!   scan (`select_limit`/`select_continue` are forward-only), so the descending
//!   order could only be fabricated.
//! * **`ets:give_away/3` / `ets:setopts/2`** — ownership verbs: the oracle takes a
//!   `pid()`. M1 has no pid term (`ets:info(T, owner)` already renders `undefined`),
//!   and `setopts/2`'s only modelled option (`{heir, …}`) needs one too.
//! * **`ets:slot/2`** — returns the objects of an internal tree node / hash bucket
//!   slot; the contract pins the *interior layout*, which the crate contract
//!   explicitly leaves open (§2), so the value cannot be reproduced faithfully.
//! * **`ets:first_lookup/1`, `last_lookup/1`, `next_lookup/2`, `prev_lookup/2`** —
//!   the paired *value* traversal of the navigation family; for the hash kinds the
//!   C walks interior buckets, and the shapes it yields (empty-slot skips,
//!   `'$end_of_table'` vs `[]`) are interior-specific.
//! * **`ets:internal_request_all/0`, `internal_delete_all/2`,
//!   `internal_select_delete/2`, `match_spec_run_r/3`** — underscore-internal
//!   helpers of the C implementation (they hand out table internals, e.g.
//!   `internal_delete_all/2` returns the table's tid); implementation-specific, and
//!   not part of the documented `ets` surface.
//!
//! Everything registered here is pinned against the system `erl` (OTP 29.1.1 /
//! erts-17.1) — see `tests/ets_extra.rs`.

use beam_common::Result;
use beam_ets::{Table, TableKind};
use beam_term::{atom_val, Eterm};

use crate::access::{
    is_atom, is_tuple, make_ref_thing, ref_thing_id, tuple_element, tuple_size,
};
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::registry::BifRegistry;

use super::resolve::{resolve_handle, Gate};
use super::{parse_updates, restore_table, take_table_gated};

/// Registers the remaining faithful `ets:*` verbs (see the module docs for the
/// scope and for the deliberately unregistered list).
pub(super) fn register(reg: &mut BifRegistry) {
    reg.register("ets", "safe_fixtable", 2, safe_fixtable_2);
    reg.register("ets", "whereis", 1, whereis_1);
    reg.register("ets", "update_element", 4, update_element_4);
    reg.register("ets", "is_compiled_ms", 1, is_compiled_ms_1);
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
