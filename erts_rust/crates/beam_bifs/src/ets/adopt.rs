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

//! The **adopted** `ets:*` verbs: the roster (`ets:all/0`), the ownership verbs
//! (`ets:setopts/2`, `ets:give_away/3`) and the fun-folds (`ets:foldl/3`,
//! `ets:foldr/3`).
//!
//! They share the parent module's handle contract
//! ([`crate::access::make_ref_thing`] as the M1 tid), its [`super::TableRegistry`],
//! its helpers ([`super::take_table_gated`]/[`super::restore_table`]) and the
//! shared table-argument resolver ([`super::resolve::resolve_handle`]) plus the
//! owner bridge ([`super::resolve::owner_of`]/[`super::resolve::caller_owner`]), so
//! they are a child of `ets`.
//!
//! # Implemented here (every one probed against the system `erl`, OTP 29.1.1 /
//! erts-17.1)
//!
//! * **`ets:all/0`** — a stdlib `ets.erl` function (not a `bif.tab` entry;
//!   `all() -> ets:internal_request_all().`). The oracle answers a list of tids:
//!   the **name atom** for a table created with `named_table`, otherwise the
//!   `#Ref<…>` handle (`is_reference` true only for the latter). The list contains
//!   every table of the node, including the system tables (`code_server`,
//!   `inet_db`, …, `logger`, `ac_tab`); this crate answers its own registry. The
//!   order is the ascending table id — deterministic, and the oracle guarantees
//!   none.
//! * **`ets:setopts/2`** — `{heir, none}` / `{heir, Pid}` / `{heir, Pid, Data}`
//!   (any option list shape the C `ets_setopts_2` accepts, including a **bare
//!   tuple** in place of the one-element list) plus
//!   `{protection, public|protected|private}`, and returns `true`. The caller must
//!   be the **owner** (`badarg` otherwise, even on a `public` table — verified);
//!   a duplicate `{heir, _}`, a duplicate `{protection, _}`, a `{heir, none, Data}`
//!   3-tuple, a non-pid heir, an unknown `protection` mode, a malformed option, an
//!   improper list or a non-list/non-tuple argument is `badarg`. Two C behaviours
//!   are modelled structurally: a **dead** heir pid is accepted and then **clears**
//!   the heir (`set_heir`, `erl_db.c:5324`) — that needs a `ProcessHook`, see
//!   below — and the arity-2 `{heir, Pid}` form carries **no** gift data (`None`,
//!   the C's `THE_NON_VALUE`, which is not a term at all). An install goes through
//!   `Table::install_heir`, so the arity-3 gift data is **deep-copied into
//!   table-owned storage** and survives the caller's heap (unchanged for an
//!   immediate, `badarg` for a term outside the store's copyable subset). The
//!   `protection` mode is applied through `Table::setopts(Some(owner),
//!   SetOpt::Access(Access))` — the owner-gated access setter — so
//!   `ets:info(T, protection)` reflects it immediately and the change does **not**
//!   re-normalize the creation-time concurrency flags (the oracle's rule: a
//!   `public` table created with `{write_concurrency, true}` switched to `private`
//!   still reports `write_concurrency => true`).
//! * **`ets:give_away/3`** — transfers ownership to a live, *different* pid and
//!   returns `true`; `badarg` for a non-pid, a dead pid, the caller's own pid, a
//!   non-owner caller, or a table argument that names no table (`ets_give_away_3`,
//!   `erl_db.c:3092`). `ets:info(T, owner)` reports the new pid afterwards.
//! * **`ets:foldl/3`** / **`ets:foldr/3`** — stdlib `ets.erl` functions (not
//!   `bif.tab` entries). The argument order is `(Fun, Acc0, Tab)` and the
//!   accumulator is the fun's **last** argument. The oracle walks *keys*
//!   (`first_lookup`/`next_lookup`, resp. `last_lookup`/`prev_lookup`) and applies
//!   `lists:foldl`/`lists:foldr` to each key's object list. `foldr` therefore
//!   reverses each key's object list for every kind, but walks the keys backwards
//!   only where the interior can — `beam_ets`'s hash interiors define `last`/`prev`
//!   as `first`/`next` (`erl_db_hash.c:853-856`), so for a **hash** kind
//!   (`set`/`bag`/`duplicate_bag`) `foldr` visits the keys in the *same* order as
//!   `foldl` and only the objects within each key are reversed (a `bag` holding
//!   `{1,a},{1,b}` yields `a,b` for `foldl` and `b,a` for `foldr`); an
//!   `ordered_set` additionally walks its keys last-to-first, so there `foldr` is
//!   the exact reverse of `foldl`. An **empty table returns
//!   `Acc0` without ever calling the fun** (so a non-fun is only observable on a
//!   non-empty table). A table argument that names no table — including a name
//!   atom that is not a `named_table` — is `badarg`. The fun is applied through
//!   [`BifContext::call_fun`], i.e. the optional [`FunCaller`] seam; with no hook
//!   installed a non-empty fold is the typed `unsupported_bif` gap, never a
//!   fabricated accumulator.
//!
//! # Honest typed gaps (never a wrong result)
//!
//! Each gap below answers the typed `unsupported_bif` raise; this list records
//! *why*, and the exact `beam_ets` API that would close it. The related verbs that
//! are served by a sibling child module rather than here are `ets:rename/2`
//! ([`super::extra`]), `ets:select_reverse/1,2,3` ([`super::reverse`]),
//! `ets:match_spec_run_r/3` ([`super::specs`]) and the `*_lookup` family
//! ([`super::nav`]).
//!
//! * **`ets:fun2ms/1`** (a stdlib function) — it needs the *literal* fun written
//!   at the call site (`ms_transform:transform_from_shell/3` → `erl_eval:fun_data/1`
//!   → `erl_parse`'s abstract format). A [`BifFn`](crate::registry::BifFn) is a
//!   plain `fn` pointer: it sees only the *evaluated* fun term and has no access to
//!   the call site's AST, and the [`FunCaller`] seam can only *call* a fun. Nothing
//!   short of an `ms_transform`/`erl_parse` port can serve it.
//! * **`ets:repair_continuation/2`** (a stdlib function) — it pattern-matches the
//!   *shape* of a hash-kind continuation tuple `{Tab, N1, N2, MSRef, L, N3}`,
//!   recompiling the embedded raw spec (`ets:match_spec_compile/1`) unless that
//!   slot already holds a compiled one; a non-continuation is `function_clause`
//!   (not `badarg`) and `'$end_of_table'` passes through. This crate's
//!   continuations are **opaque ref things** over a registry key, so the shape the
//!   oracle rewrites has no counterpart here (`ets:is_compiled_ms/1` is the
//!   registered predicate). Closing it needs a continuation representation with a
//!   raw-spec slot plus a `function_clause` raise.
//! * **`ets:internal_request_all/0`** — an underscore-internal C helper: it answers
//!   this scheduler's *fan-in reference* (`#Ref<…>`), an implementation-specific
//!   value, not part of the documented API. (`internal_delete_all/2` and
//!   `internal_select_delete/2` are implemented in [`super::extra`].)
//!
//! # Runtime handoffs (not this crate's to do)
//!
//! * **`{'ETS-TRANSFER', Tid, FromPid, GiftData}`** — the oracle's
//!   `ets:give_away/3` and an **heir stepping in on the owner's death** both
//!   **deliver** this message to the new owner (`send_ets_transfer_message`,
//!   `erl_db.c:4943`). It is sent **only** when the heir was installed by the
//!   arity-3 `{heir, Pid, Data}` form; an arity-2 `{heir, Pid}` heir inherits in
//!   **silence**. This crate performs the observable ownership transfer and stores
//!   the gift (`Heir::data`, the table-owned copy `Table::install_heir` made); the
//!   delivery itself is a message-passing concern (`beam_rt`) and is deliberately
//!   **not** fabricated here.
//! * **Process liveness** — the C's `give_away`/`set_heir` resolve the target with
//!   `erts_pid2proc`/`erts_proc_lookup`, so a dead pid is `badarg` for
//!   `give_away/3` and clears the heir for `setopts/2`. Liveness is asked of the
//!   optional [`ProcessHook`](crate::context::ProcessHook)
//!   ([`BifContext::proc_is_alive`]); with **no hook installed** it cannot be
//!   answered and the pid is taken structurally (documented M1 gap) —
//!   `beam_rt` installs the hook, so the real runtime is exact.

use beam_common::Result;
use beam_ets::{Access, OwnerId, SetOpt, Table};
use beam_process::ProcessId;
use beam_term::{atom_val, is_internal_pid, make_atom, AtomTable, Eterm};

use crate::access::{
    is_atom, is_list, is_nil, is_tuple, make_ref_thing, tuple_element, tuple_size,
};
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::print::proper_list;
use crate::registry::BifRegistry;

use super::resolve::{caller_owner, owner_of, resolve_handle, Gate};
use super::{copy_out, restore_table, take_table_gated};

/// Registers the adopted `ets:*` verbs (see the module docs for the scope and for
/// the honest gaps).
pub(super) fn register(reg: &mut BifRegistry) {
    reg.register("ets", "all", 0, all_0);
    reg.register("ets", "setopts", 2, setopts_2);
    reg.register("ets", "give_away", 3, give_away_3);
    reg.register("ets", "foldl", 3, foldl_3);
    reg.register("ets", "foldr", 3, foldr_3);
}

// ------------------------------------------------------------------- all/0

/// `ets:all()` — every table of the node: the **name atom** for a table created
/// with `named_table`, otherwise its handle reference.
///
/// The oracle delegates to `ets:internal_request_all/0` and answers the tids of
/// every table, system tables included; here the registry *is* the node's table
/// set. The order is ascending table id (the oracle guarantees none).
fn all_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let tids = {
        let tables = ctx.tables().ok_or_else(unsupported_bif)?;
        tables.all_tids()
    };
    let mut terms = Vec::with_capacity(tids.len());
    for (handle, name) in tids {
        match name {
            Some(atom) => terms.push(make_atom(atom)),
            None => terms.push(make_ref_thing(ctx.heap(), handle)?),
        }
    }
    ctx.list(&terms)
}

// ---------------------------------------------------------------- setopts/2

/// The parsed `{heir, …}` half of an `ets:setopts/2` option list.
enum HeirOpt {
    /// `{heir, none}` — clear the heir.
    Clear,
    /// `{heir, Pid}` / `{heir, Pid, Data}` — install the heir.
    ///
    /// `data` is `None` for the 2-tuple form (`{heir, Pid}`), which the C stores
    /// as `THE_NON_VALUE`: the heir inherits silently, with no
    /// `{'ETS-TRANSFER', …}` message. The 3-tuple form carries `Some(data)`.
    Install {
        pid: ProcessId,
        data: Option<Eterm>,
    },
}

/// The heir change `ets:setopts/2` must apply, resolved **before** the table is
/// taken out of the registry (the dead-heir clear needs the [`ProcessHook`]
/// question answered through the whole context).
///
/// [`ProcessHook`]: crate::context::ProcessHook
enum HeirChange {
    /// No `{heir, …}` option was given: nothing to change (the owner check still
    /// runs).
    Nothing,
    /// Clear the heir: `{heir, none}`, or an install onto a **dead** pid (the C
    /// `set_heir` clears it, `erl_db.c:5324`).
    Clear,
    /// Install `owner` as the heir. `data` is the option's arity — `None` for
    /// `{heir, Pid}`, `Some(term)` for `{heir, Pid, Data}`.
    Install {
        owner: OwnerId,
        data: Option<Eterm>,
    },
}

/// `ets:setopts(Tab, Options)` — installs or clears the table's heir, changes its
/// `protection` and returns `true`.
///
/// Reproduces the accept/reject matrix of the C `ets_setopts_2`
/// (`erl_db.c:3151`): the second argument is a proper list of options, **or a
/// single option tuple** (the C wraps a tuple in a one-element list), every
/// element being a tuple of arity ≥ 2 whose first element is a **known** option
/// tag. `heir` accepts `none` (arity 2) or an internal pid (arity 2 ⇒ no gift
/// data, the C's `THE_NON_VALUE`; arity 3 ⇒ the given data); a second `heir`
/// option is `badarg`. An install runs through [`Table::install_heir`], so the
/// gift data is **deep-copied into table-owned storage** (the A.1 copy boundary,
/// the C `set_heir`'s `DbTerm`) and cannot dangle when the caller's heap is reused;
/// a dead heir pid is accepted and clears the heir. `protection` accepts exactly
/// `public`/`protected`/`private` (a second `protection` option and any other
/// value are `badarg`) and is applied through `Table::setopts(Some(owner),
/// SetOpt::Access(Access))`, the owner-gated access setter.
///
/// The C applies the options only after the whole list has been validated and the
/// **owner** check has passed, so a caller who is not the table's owner gets
/// `badarg` (the access gate denies a non-owner of a `protected`/`private` table,
/// and the owner check the rest).
fn setopts_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve_handle(ctx, args[0])?;
    let (heir, access) = parse_setopts(ctx.atoms(), args[1])?;
    // Resolve the `{heir, …}` option before taking the table: the dead-heir clear
    // asks the `ProcessHook` through the whole context.
    let change = match heir {
        None => HeirChange::Nothing,
        Some(HeirOpt::Clear) => HeirChange::Clear,
        Some(HeirOpt::Install { pid, data }) => {
            // `set_heir` (`erl_db.c:5324`) clears the heir when the pid is not
            // alive; liveness is only answerable with a `ProcessHook` installed.
            if pid_liveness(ctx, pid)? == Some(false) {
                HeirChange::Clear
            } else {
                HeirChange::Install {
                    owner: owner_of(pid),
                    data,
                }
            }
        }
    };
    let caller = caller_owner(ctx);
    let mut table = take_table_gated(ctx, handle, Gate::Write)?;
    let outcome = if table.owner() != Some(caller) {
        // Even an empty option list requires the owner (`ets_setopts_2` checks
        // `tb->common.owner != BIF_P->common.id` unconditionally).
        Err(badarg())
    } else {
        // The heir change first (`Table::install_heir` deep-copies the gift into
        // table-owned storage — the A.1 copy boundary, the C `set_heir`'s `DbTerm`
        // — so a non-immediate `data` does not alias the caller's heap), then the
        // access change through the owner-gated `Table::setopts`.
        let heir_result = match change {
            HeirChange::Nothing => Ok(()),
            // The owner-level clear reclaims the outgoing heir's table-owned gift
            // copy (the C `free_heir_data`).
            HeirChange::Clear => {
                table.set_heir(None);
                Ok(())
            }
            HeirChange::Install { owner, data } => table.install_heir(owner, data),
        };
        heir_result.and_then(|()| match access {
            Some(access) => table.setopts(Some(caller), SetOpt::Access(access)),
            None => Ok(()),
        })
    };
    restore_table(ctx, handle, table)?;
    outcome?;
    ctx.boolean(true)
}

/// Parses an `ets:setopts/2` option list (see [`setopts_2`]), returning the
/// `{heir, …}` option and the `{protection, …}` access mode (when one was given).
fn parse_setopts(atoms: &AtomTable, options: Eterm) -> Result<(Option<HeirOpt>, Option<Access>)> {
    let elems = if is_tuple(options) {
        vec![options]
    } else if is_list(options) || is_nil(options) {
        proper_list(options).ok_or_else(badarg)?
    } else {
        return Err(badarg());
    };
    let mut heir: Option<HeirOpt> = None;
    let mut access: Option<Access> = None;
    for elem in elems {
        if !is_tuple(elem) || tuple_size(elem) < 2 {
            return Err(badarg());
        }
        let tag = tuple_element(elem, 0);
        if !is_atom(tag) {
            return Err(badarg());
        }
        match atoms.name(atom_val(tag)) {
            Some("heir") => {
                if heir.is_some() {
                    return Err(badarg());
                }
                let target = tuple_element(elem, 1);
                let arity = tuple_size(elem);
                if is_atom(target) && atoms.name(atom_val(target)) == Some("none") {
                    if arity != 2 {
                        return Err(badarg());
                    }
                    heir = Some(HeirOpt::Clear);
                } else if is_internal_pid(target) {
                    let pid = ProcessId::from_eterm(target).map_err(|_| badarg())?;
                    // `{heir, Pid}` carries no data in the C (`THE_NON_VALUE`, not a
                    // term) => `None`, observable as the absent `{'ETS-TRANSFER', …}`
                    // message; the 3-tuple form carries `Some(data)`.
                    let data = (arity == 3).then(|| tuple_element(elem, 2));
                    heir = Some(HeirOpt::Install { pid, data });
                } else {
                    return Err(badarg());
                }
            }
            Some("protection") => {
                if access.is_some() || tuple_size(elem) != 2 {
                    return Err(badarg());
                }
                let value = tuple_element(elem, 1);
                let name = if is_atom(value) {
                    atoms.name(atom_val(value))
                } else {
                    None
                };
                // The three mode atoms are the only accepted values (`Access`'s own
                // parser is the single source of the spellings, so a
                // `{protection, bogus}` cannot be half-accepted).
                access = name.and_then(Access::from_atom_name);
                if access.is_none() {
                    return Err(badarg());
                }
            }
            _ => return Err(badarg()),
        }
    }
    Ok((heir, access))
}

// -------------------------------------------------------------- give_away/3

/// `ets:give_away(Tab, Pid, GiftData)` — transfers the table to `Pid` and returns
/// `true`.
///
/// Mirrors `ets_give_away_3` (`erl_db.c:3092`): a table argument that names no
/// table, a non-pid (or dead) target, a caller that is not the current owner, and
/// the caller's own pid are each `badarg` (all four are the C's `BADARG`, so their
/// relative order is unobservable). The `{protection, _}` mode is *not* consulted
/// (`give_away` is an ownership operation, not a read/write one) — the write gate
/// plus the owner check reproduce the C's `DB_WRITE` acquisition + owner test.
///
/// The `{'ETS-TRANSFER', Tid, FromPid, GiftData}` delivery to the new owner is a
/// runtime concern and is deliberately not fabricated here (see the module docs).
fn give_away_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve_handle(ctx, args[0])?;
    let target = pid_arg(args[1])?;
    // The C resolves the target with `erts_pid2proc`, which is NULL for a dead
    // process; liveness is only answerable with a `ProcessHook` installed.
    if pid_liveness(ctx, target)? == Some(false) {
        return Err(badarg());
    }
    let caller = caller_owner(ctx);
    let mut table = take_table_gated(ctx, handle, Gate::Write)?;
    let outcome = table
        .as_requester(Some(caller))
        .give_away(Some(owner_of(target)));
    restore_table(ctx, handle, table)?;
    outcome?;
    ctx.boolean(true)
}

// ------------------------------------------------------------ foldl/3 foldr/3

/// `ets:foldl(Fun, Acc0, Tab)` — `Acc = Fun(Object, Acc)` over every object of
/// `Tab`, in traversal order.
fn foldl_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    fold(ctx, args, false)
}

/// `ets:foldr(Fun, Acc0, Tab)` — as [`foldl_3`], but the objects are visited right
/// to left (see [`collect_objects`] for what that means per kind).
fn foldr_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    fold(ctx, args, true)
}

/// The body of [`foldl_3`]/[`foldr_3`]: the argument order is `(Fun, Acc0, Tab)`
/// and `reverse` selects the fold direction.
fn fold(ctx: &mut BifContext<'_>, args: &[Eterm], reverse: bool) -> Result<Eterm> {
    let fun = args[0];
    let acc0 = args[1];
    let handle = resolve_handle(ctx, args[2])?;
    let table = take_table_gated(ctx, handle, Gate::Read)?;
    let mut objects = Vec::new();
    // `collect_objects` walks the table *and* the caller heap, and the context
    // lends the atom table through a conflicting borrow — so it is given a clone
    // (identical indices, read-only; the same pattern the sibling `select`/the
    // `update_counter` BIFs use).
    let atoms = ctx.atoms().clone();
    let collected = collect_objects(&table, &atoms, reverse, &mut objects, ctx);
    restore_table(ctx, handle, table)?;
    collected?;
    if objects.is_empty() {
        // The oracle's `do_foldl`/`do_foldr` answer `Accu` for `'$end_of_table'`
        // without calling the fun, so the fun is never validated on an empty table.
        return Ok(acc0);
    }
    // `objects` is already in *visit* order — the fold direction was applied while
    // collecting, exactly as the oracle's `do_foldl`/`do_foldr` walk.
    let mut acc = acc0;
    for &object in objects.iter() {
        acc = ctx.call_fun(fun, &[object, acc])?;
    }
    Ok(acc)
}

/// Collects every object of `table` in **visit** order — the order the fold's fun
/// is applied in — onto the caller's heap.
///
/// The oracle's `ets:foldl/3`/`ets:foldr/3` (`stdlib/src/ets.erl:2510,2552`) are
/// `first_lookup`/`next_lookup` (resp. `last_lookup`/`prev_lookup`) walks over the
/// *keys*, each key's object list folded with `lists:foldl`/`lists:foldr`. That
/// gives two independent directions:
///
/// * the **key walk** — a `foldr` starts at [`Table::last`] and steps with
///   [`Table::prev`]; only an `ordered_set` actually walks backwards, because a
///   hash interior's `last`/`prev` **are** its `first`/`next`
///   (`erl_db_hash.c:853-856`), so `foldr` visits a hash kind's keys in the very
///   same order as `foldl`;
/// * the **object list of each key** — reversed for `foldr` (the `lists:foldr`
///   over `Objects`), for every kind.
///
/// The copies go through [`super::copy_out`] so the fun never sees a term aliasing
/// table storage (the fun may allocate on the caller heap).
fn collect_objects(
    table: &Table,
    atoms: &AtomTable,
    reverse: bool,
    out: &mut Vec<Eterm>,
    ctx: &mut BifContext<'_>,
) -> Result<()> {
    let mut key = if reverse {
        table.last(atoms)
    } else {
        table.first(atoms)
    };
    while let Some(current) = key {
        let objects = table.lookup(current, atoms);
        if reverse {
            for object in objects.iter().rev() {
                out.push(copy_out(*object, ctx)?);
            }
        } else {
            for object in objects {
                out.push(copy_out(object, ctx)?);
            }
        }
        key = match if reverse {
            table.prev(current, atoms)
        } else {
            table.next(current, atoms)
        } {
            Ok(next) => next,
            Err(err) => return Err(err),
        };
    }
    Ok(())
}

// -------------------------------------------------------------------- helpers

/// The internal pid a `pid()` argument names (`is_internal_pid`, as the C's
/// `ets_give_away_3` uses); anything else is `badarg`.
fn pid_arg(term: Eterm) -> Result<ProcessId> {
    if !is_internal_pid(term) {
        return Err(badarg());
    }
    ProcessId::from_eterm(term).map_err(|_| badarg())
}

/// The liveness of `pid`, or `None` when no [`ProcessHook`] can answer it.
///
/// [`ProcessHook`]: crate::context::ProcessHook
fn pid_liveness(ctx: &mut BifContext<'_>, pid: ProcessId) -> Result<Option<bool>> {
    if !ctx.has_process_hook() {
        return Ok(None);
    }
    Ok(Some(ctx.proc_is_alive(pid)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;
    use std::rc::Rc;

    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::PortId;
    use beam_term::{atom_val, Atom, AtomTable, NIL};

    use crate::access::ref_thing_id;
    use crate::context::{BifContext, ProcessHook, ProcessInfoItem};
    use crate::registry::BifRegistry;
    use crate::TableRegistry;

    /// The option list of a single `{heir, …}` option, on `ctx`'s heap.
    fn heir_list(ctx: &mut BifContext<'_>, elems: &[Eterm]) -> Eterm {
        let opt = ctx.tuple(elems).expect("option tuple");
        ctx.list(&[opt]).expect("option list")
    }

    /// `ets:setopts/2` accepts the three `{heir, …}` shapes the C accepts and
    /// rejects the malformed ones (see the module docs for the oracle probes).
    #[test]
    fn setopts_parses_the_c_option_matrix() {
        let mut atoms = beam_term::AtomTable::new();
        atoms.seed([
            "heir",
            "none",
            "protection",
            "public",
            "undefined",
            "bogus",
        ]);
        let mut heap = Heap::new(1 << 12).expect("heap");
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let heir = ctx.atom("heir").expect("seeded");
        let none = ctx.atom("none").expect("seeded");
        let protection = ctx.atom("protection").expect("seeded");
        let public = ctx.atom("public").expect("seeded");
        let bogus = ctx.atom("bogus").expect("seeded");
        let pid = ProcessId::new(3, 0).to_eterm();

        // `{heir, none}` clears; `{heir, Pid}` installs with no gift data.
        let list = heir_list(&mut ctx, &[heir, none]);
        let (opt, access) = parse_setopts(ctx.atoms(), list).expect("{heir, none}");
        assert!(matches!(opt, Some(HeirOpt::Clear)));
        assert!(access.is_none());

        let list = heir_list(&mut ctx, &[heir, pid]);
        let (opt, _) = parse_setopts(ctx.atoms(), list).expect("{heir, Pid}");
        match opt {
            Some(HeirOpt::Install { pid: parsed, data }) => {
                assert_eq!(parsed, ProcessId::new(3, 0));
                assert_eq!(data, None);
            }
            _ => panic!("expected an installed heir"),
        }

        let data = ctx.int(7).expect("small int");
        let list = heir_list(&mut ctx, &[heir, pid, data]);
        let (opt, _) = parse_setopts(ctx.atoms(), list).expect("{heir, Pid, Data}");
        match opt {
            Some(HeirOpt::Install { data: parsed, .. }) => assert_eq!(parsed, Some(data)),
            _ => panic!("expected an installed heir"),
        }

        // A `{protection, public}` option parses to the access mode.
        let list = heir_list(&mut ctx, &[protection, public]);
        let (opt, access) = parse_setopts(ctx.atoms(), list).expect("{protection, public}");
        assert!(opt.is_none());
        assert_eq!(access, Some(Access::Public));

        // Malformed shapes: a non-pid heir, `{heir, none, Data}`, a duplicate
        // `{heir, _}`, a duplicate `{protection, _}`, an unknown tag, a non-atom
        // tag, a too-short tuple, a bare atom and an improper list.
        let small = ctx.int(5).expect("small int");
        let bad_pid = heir_list(&mut ctx, &[heir, small]);
        let bad_heir_none = heir_list(&mut ctx, &[heir, none, data]);
        let dup_heir = {
            let opt = ctx.tuple(&[heir, pid]).expect("tuple");
            ctx.list(&[opt, opt]).expect("list")
        };
        let dup_protection = {
            let one = ctx.tuple(&[protection, public]).expect("tuple");
            ctx.list(&[one, one]).expect("list")
        };
        let unknown_tag = heir_list(&mut ctx, &[bogus, small]);
        let non_atom_tag = ctx.tuple(&[small, small]).expect("tuple");
        let too_short = ctx.tuple(&[heir]).expect("tuple");
        let improper = ctx.cons(pid, data).expect("cons");
        for bad in [
            bad_pid,
            bad_heir_none,
            dup_heir,
            dup_protection,
            unknown_tag,
            non_atom_tag,
            too_short,
            bogus,
            small,
            improper,
        ] {
            let outcome = parse_setopts(ctx.atoms(), bad);
            assert!(
                matches!(outcome, Err(BeamError::BadArg)),
                "expected badarg for {bad:?}"
            );
        }
        // An improper list is rejected too.
        assert!(matches!(parse_setopts(ctx.atoms(), improper), Err(BeamError::BadArg)));

        // An empty list is accepted (the owner check happens elsewhere).
        let (opt, access) = parse_setopts(ctx.atoms(), NIL).expect("[]");
        assert!(opt.is_none() && access.is_none());
    }

    /// A single option **tuple** is accepted in place of the one-element list
    /// (the C wraps a tuple in a `fakelist`).
    #[test]
    fn setopts_accepts_a_bare_option_tuple() {
        let mut atoms = beam_term::AtomTable::new();
        atoms.seed(["heir", "none"]);
        let mut heap = Heap::new(1 << 12).expect("heap");
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let opt = ctx
            .tuple(&[
                ctx.atom("heir").expect("seeded"),
                ctx.atom("none").expect("seeded"),
            ])
            .expect("option tuple");
        let (parsed, _) = parse_setopts(ctx.atoms(), opt).expect("a bare tuple");
        assert!(matches!(parsed, Some(HeirOpt::Clear)));
    }

    /// The adopted keys are registered exactly once each and reachable.
    #[test]
    fn the_adopted_keys_are_registered() {
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        for key in [
            ("all", 0),
            ("setopts", 2),
            ("give_away", 3),
            ("foldl", 3),
            ("foldr", 3),
        ] {
            assert!(reg.contains("ets", key.0, key.1), "missing ets:{}", key.0);
        }
        assert_eq!(reg.len(), 5);
    }

    // ------------------------------------------------- setopts: heir lifetime

    /// A [`ProcessHook`] whose only meaningful answer is `is_process_alive`
    /// (`alive`); `ets:setopts/2` needs nothing else.
    struct LivenessHook {
        alive: Rc<Cell<bool>>,
    }

    impl ProcessHook for LivenessHook {
        fn whereis(&mut self, _name: Atom) -> Result<Option<Eterm>> {
            Ok(None)
        }
        fn register(&mut self, _name: Atom, _id: Eterm) -> Result<bool> {
            Ok(false)
        }
        fn unregister(&mut self, _name: Atom) -> Result<bool> {
            Ok(false)
        }
        fn registered(&mut self) -> Result<Vec<Atom>> {
            Ok(Vec::new())
        }
        fn is_process_alive(&mut self, _pid: ProcessId) -> Result<bool> {
            Ok(self.alive.get())
        }
        fn processes(&mut self) -> Result<Vec<ProcessId>> {
            Ok(Vec::new())
        }
        fn ports(&mut self) -> Result<Vec<PortId>> {
            Ok(Vec::new())
        }
        fn process_info_keys(&mut self, _pid: ProcessId) -> Result<Option<Vec<Atom>>> {
            Ok(None)
        }
        fn process_info_item(
            &mut self,
            _heap: &mut Heap,
            _atoms: &AtomTable,
            _pid: ProcessId,
            _item: Atom,
        ) -> Result<ProcessInfoItem> {
            Ok(ProcessInfoItem::Unknown)
        }
        fn process_flag(
            &mut self,
            _heap: &mut Heap,
            _atoms: &AtomTable,
            _flag: Atom,
            _value: Eterm,
        ) -> Result<Option<Eterm>> {
            Ok(None)
        }
        fn group_leader(&mut self) -> Result<ProcessId> {
            Ok(ProcessId::new(1, 0))
        }
    }

    /// The atoms the `ets:new`/`ets:setopts` fixtures need.
    fn setopts_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "set",
            "heir",
            "none",
            "true",
            "t",
            "owner",
            "protection",
            "public",
            "protected",
            "private",
        ]);
        atoms
    }

    /// `ets:setopts/2`'s arity-3 `{heir, Pid, Data}` form **deep-copies** the gift
    /// into table-owned storage (the A.1 copy boundary) — the raw `SetOpt::Heir`
    /// primitive would alias the caller's heap, so the BIF goes through
    /// [`beam_ets::Table::install_heir`]. The arity-2 `{heir, Pid}` form installs a
    /// **silent** heir (`Heir::data == None`) and `{heir, none}` clears it.
    #[test]
    fn setopts_installs_a_table_owned_copy_of_the_gift_data() {
        let bifs = crate::default_registry();
        let atoms = setopts_atoms();
        let mut heap = Heap::new(1 << 14).expect("heap");
        let mut tables = TableRegistry::new();
        let pid = ProcessId::new(1, 0);

        let mut ctx = BifContext::new(&mut heap, &atoms, pid).with_tables(&mut tables);
        let name = ctx.atom("t").expect("seeded");
        let set = ctx.atom("set").expect("seeded");
        let set_opts = ctx.list(&[set]).expect("option list");
        let table = bifs
            .call("ets", "new", 2, &mut ctx, &[name, set_opts])
            .expect("ets:new");
        let handle = ref_thing_id(table).expect("an unnamed table handle");

        // A boxed gift term on the caller's (process-like) heap.
        let source = {
            let a = ctx.int(1).expect("small int");
            let b = ctx.int(2).expect("small int");
            let c = ctx.int(3).expect("small int");
            ctx.tuple(&[a, b, c]).expect("a boxed gift")
        };
        let heir = ctx.atom("heir").expect("seeded");
        let opt = ctx
            .tuple(&[heir, pid.to_eterm(), source])
            .expect("the {heir, Pid, Data} option");
        let opts = ctx.list(&[opt]).expect("option list");
        assert_eq!(
            bifs.call("ets", "setopts", 2, &mut ctx, &[table, opts])
                .expect("ets:setopts"),
            ctx.atom("true").expect("seeded")
        );

        let stored = ctx
            .tables()
            .expect("tables")
            .get(handle)
            .expect("a live table")
            .heir_data()
            .expect("the arity-3 gift is installed");
        assert_ne!(
            stored.raw(),
            source.raw(),
            "the gift is deep-copied, never aliased into the caller's heap"
        );
        assert!(
            beam_term::compare::eq(stored, source, ctx.atoms()),
            "the table-owned copy is structurally the gift"
        );

        // The arity-2 form is a **silent** gift: an heir with no gift data.
        let opt = ctx
            .tuple(&[heir, pid.to_eterm()])
            .expect("the {heir, Pid} option");
        let opts = ctx.list(&[opt]).expect("option list");
        bifs.call("ets", "setopts", 2, &mut ctx, &[table, opts])
            .expect("ets:setopts");
        let installed = ctx
            .tables()
            .expect("tables")
            .get(handle)
            .expect("a live table")
            .heir()
            .expect("the arity-2 heir is installed");
        assert_eq!(installed.owner, owner_of(pid));
        assert_eq!(
            installed.data, None,
            "an arity-2 heir carries no gift data (and inherits in silence)"
        );

        // `{heir, none}` clears the heir (and reclaims the copy).
        let none = ctx.atom("none").expect("seeded");
        let opt = ctx.tuple(&[heir, none]).expect("the {heir, none} option");
        let opts = ctx.list(&[opt]).expect("option list");
        bifs.call("ets", "setopts", 2, &mut ctx, &[table, opts])
            .expect("ets:setopts");
        assert_eq!(
            ctx.tables()
                .expect("tables")
                .get(handle)
                .expect("a live table")
                .heir(),
            None,
            "`{{heir, none}}` clears the heir"
        );
    }

    /// A **dead** heir pid is accepted and then **clears** the heir (the C
    /// `set_heir`, `erl_db.c:5324`), and a non-owner caller is `badarg` (the owner
    /// check `ets_setopts_2` performs unconditionally).
    #[test]
    fn setopts_clears_on_a_dead_heir_and_requires_the_owner() {
        let bifs = crate::default_registry();
        let atoms = setopts_atoms();
        let mut heap = Heap::new(1 << 14).expect("heap");
        let mut tables = TableRegistry::new();
        let owner = ProcessId::new(1, 0);
        let other = ProcessId::new(2, 0);
        let heir_pid = ProcessId::new(9, 0);
        let alive = Rc::new(Cell::new(true));
        let mut hook = LivenessHook {
            alive: Rc::clone(&alive),
        };

        let (table, handle) = {
            let mut ctx = BifContext::new(&mut heap, &atoms, owner)
                .with_tables(&mut tables)
                .with_process_hook(&mut hook);
            let name = ctx.atom("t").expect("seeded");
            let set = ctx.atom("set").expect("seeded");
            let set_opts = ctx.list(&[set]).expect("option list");
            let table = bifs
                .call("ets", "new", 2, &mut ctx, &[name, set_opts])
                .expect("ets:new");
            let handle = ref_thing_id(table).expect("an unnamed table handle");
            (table, handle)
        };

        // (a) A **live** heir pid installs the heir.
        {
            let mut ctx = BifContext::new(&mut heap, &atoms, owner)
                .with_tables(&mut tables)
                .with_process_hook(&mut hook);
            let heir = ctx.atom("heir").expect("seeded");
            let opt = ctx.tuple(&[heir, heir_pid.to_eterm()]).expect("option");
            let opts = ctx.list(&[opt]).expect("option list");
            bifs.call("ets", "setopts", 2, &mut ctx, &[table, opts])
                .expect("ets:setopts");
        }
        assert_eq!(
            tables.get(handle).expect("a live table").heir().map(|h| h.owner),
            Some(owner_of(heir_pid)),
            "a live heir is installed"
        );

        // (b) The same pid after it dies: accepted, and **clears** the heir.
        alive.set(false);
        {
            let mut ctx = BifContext::new(&mut heap, &atoms, owner)
                .with_tables(&mut tables)
                .with_process_hook(&mut hook);
            let heir = ctx.atom("heir").expect("seeded");
            let opt = ctx.tuple(&[heir, heir_pid.to_eterm()]).expect("option");
            let opts = ctx.list(&[opt]).expect("option list");
            bifs.call("ets", "setopts", 2, &mut ctx, &[table, opts])
                .expect("ets:setopts");
        }
        assert_eq!(
            tables.get(handle).expect("a live table").heir(),
            None,
            "a dead heir is accepted and clears the heir"
        );

        // (c) A non-owner caller is `badarg` and leaves the heir untouched.
        {
            let mut ctx = BifContext::new(&mut heap, &atoms, other)
                .with_tables(&mut tables)
                .with_process_hook(&mut hook);
            let heir = ctx.atom("heir").expect("seeded");
            let opt = ctx.tuple(&[heir, heir_pid.to_eterm()]).expect("option");
            let opts = ctx.list(&[opt]).expect("option list");
            assert!(matches!(
                bifs.call("ets", "setopts", 2, &mut ctx, &[table, opts]),
                Err(BeamError::BadArg)
            ));
        }
        assert_eq!(
            tables.get(handle).expect("a live table").heir(),
            None,
            "the denied call left the heir untouched"
        );
    }

    /// A valid `{protection, P}` option changes the table's access through
    /// `Table::setopts(Some(owner), SetOpt::Access(_))` (the owner-gated setter) and
    /// returns `true`; `ets:info(T, protection)` reflects each change immediately. An
    /// empty option list by the owner is `true` (nothing is changed); a malformed
    /// value, a duplicate `{protection, _}` and a **non-owner** caller (even on a
    /// `public` table) are the oracle's `badarg`.
    ///
    /// Oracle probes (`erl -noshell`, erts-17.1):
    ///
    /// ```text
    /// 1> T = ets:new(t, [set, public]),
    ///    {ets:setopts(T, [{protection, protected}]), ets:info(T, protection)}.
    /// {true, protected}
    /// 2> {ets:setopts(T, [{protection, private}]), ets:info(T, protection)}.
    /// {true, private}
    /// 3> {ets:setopts(T, [{protection, public}]), ets:info(T, protection)}.
    /// {true, public}
    /// 4> ets:setopts(T, []).                        % the owner, nothing to change
    /// true
    /// 5> ets:setopts(T, [{protection, bogus}]).
    /// ** exception error: bad argument
    /// 6> ets:setopts(T, [{protection, public}, {protection, public}]).
    /// ** exception error: bad argument
    /// 7> % a non-owner of the public table (from a spawned process):
    ///    catch ets:setopts(T, [{protection, private}]).
    /// {'EXIT', {badarg, …}}                         % info(T, protection) stays public
    /// ```
    #[test]
    fn setopts_applies_the_protection_option() {
        let bifs = crate::default_registry();
        let atoms = setopts_atoms();
        let mut heap = Heap::new(1 << 14).expect("heap");
        let mut tables = TableRegistry::new();
        let owner = ProcessId::new(1, 0);
        let other = ProcessId::new(2, 0);

        // Build the table as its owner and walk the three modes, checking
        // `ets:info(T, protection)` after every change (probes 1–3).
        let (table, protection) = {
            let mut ctx = BifContext::new(&mut heap, &atoms, owner).with_tables(&mut tables);
            let name = ctx.atom("t").expect("seeded");
            let set = ctx.atom("set").expect("seeded");
            let public = ctx.atom("public").expect("seeded");
            let set_opts = ctx.list(&[set, public]).expect("option list");
            let table = bifs
                .call("ets", "new", 2, &mut ctx, &[name, set_opts])
                .expect("ets:new");

            let protection = ctx.atom("protection").expect("seeded");
            for mode in ["protected", "private", "public"] {
                let value = ctx.atom(mode).expect("seeded");
                let opt = ctx.tuple(&[protection, value]).expect("option");
                let opts = ctx.list(&[opt]).expect("option list");
                let truth = bifs
                    .call("ets", "setopts", 2, &mut ctx, &[table, opts])
                    .expect("ets:setopts");
                assert_eq!(ctx.atoms().name(atom_val(truth)), Some("true"));
                let info = bifs
                    .call("ets", "info", 2, &mut ctx, &[table, protection])
                    .expect("ets:info/2");
                assert_eq!(
                    ctx.atoms().name(atom_val(info)),
                    Some(mode),
                    "the protection is applied"
                );
            }
            (table, protection)
        };

        // Probe 4: an empty option list is accepted for the owner and returns `true`.
        {
            let mut ctx = BifContext::new(&mut heap, &atoms, owner).with_tables(&mut tables);
            let empty = ctx.list(&[]).expect("[]");
            let truth = bifs
                .call("ets", "setopts", 2, &mut ctx, &[table, empty])
                .expect("ets:setopts");
            assert_eq!(ctx.atoms().name(atom_val(truth)), Some("true"));
        }

        // Probe 5: a malformed `{protection, <non-access atom>}` is `badarg`.
        // Probe 6: a duplicate `{protection, _}` is `badarg`, even when both entries
        // are equal (the C validates the whole list first).
        {
            let mut ctx = BifContext::new(&mut heap, &atoms, owner).with_tables(&mut tables);
            let public = ctx.atom("public").expect("seeded");
            let bad_value = {
                let opt = ctx
                    .tuple(&[protection, ctx.atom("heir").expect("seeded")])
                    .expect("option");
                ctx.list(&[opt]).expect("option list")
            };
            assert!(matches!(
                bifs.call("ets", "setopts", 2, &mut ctx, &[table, bad_value]),
                Err(BeamError::BadArg)
            ));
            let duplicate = {
                let one = ctx.tuple(&[protection, public]).expect("option");
                ctx.list(&[one, one]).expect("option list")
            };
            assert!(matches!(
                bifs.call("ets", "setopts", 2, &mut ctx, &[table, duplicate]),
                Err(BeamError::BadArg)
            ));
        }

        // Probe 7: a non-owner caller is `badarg`. The table is `public`, so the write
        // gate admits the caller and the *owner* check is what denies it.
        {
            let mut ctx = BifContext::new(&mut heap, &atoms, other).with_tables(&mut tables);
            let private = ctx.atom("private").expect("seeded");
            let opt = ctx.tuple(&[protection, private]).expect("option");
            let opts = ctx.list(&[opt]).expect("option list");
            assert!(matches!(
                bifs.call("ets", "setopts", 2, &mut ctx, &[table, opts]),
                Err(BeamError::BadArg)
            ));
        }
        {
            let mut ctx = BifContext::new(&mut heap, &atoms, owner).with_tables(&mut tables);
            let info = bifs
                .call("ets", "info", 2, &mut ctx, &[table, protection])
                .expect("ets:info/2");
            assert_eq!(
                ctx.atoms().name(atom_val(info)),
                Some("public"),
                "a denied call leaves the protection unchanged"
            );
        }
    }
}
