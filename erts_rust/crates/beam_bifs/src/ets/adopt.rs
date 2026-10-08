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
//!   tuple** in place of the one-element list) and returns `true`. The caller must
//!   be the **owner** (`badarg` otherwise, even on a `public` table — verified);
//!   a duplicate `{heir, _}`, a `{heir, none, Data}` 3-tuple, a non-pid heir, a
//!   malformed option, an improper list or a non-list/non-tuple argument is
//!   `badarg`. Two C behaviours are modelled structurally: a **dead** heir pid is
//!   accepted and then **clears** the heir (`set_heir`, `erl_db.c:5324`) — that
//!   needs a `ProcessHook`, see below — and `{heir, Pid}` (no data) stores the
//!   gift data as `[]` (`THE_NON_VALUE` in the C, which is not a term at all).
//! * **`ets:give_away/3`** — transfers ownership to a live, *different* pid and
//!   returns `true`; `badarg` for a non-pid, a dead pid, the caller's own pid, a
//!   non-owner caller, or a table argument that names no table (`ets_give_away_3`,
//!   `erl_db.c:3092`). `ets:info(T, owner)` reports the new pid afterwards.
//! * **`ets:foldl/3`** / **`ets:foldr/3`** — stdlib `ets.erl` functions (not
//!   `bif.tab` entries). The argument order is `(Fun, Acc0, Tab)` and the
//!   accumulator is the fun's **last** argument. The oracle walks *keys*
//!   (`first_lookup`/`next_lookup`, `last_lookup`/`prev_lookup`) and applies
//!   `lists:foldl`/`lists:foldr` to each key's object list, so `foldl` visits
//!   every object in forward traversal order and `foldr` in the exact reverse
//!   (including the objects *within* one key: a `bag` holding `{1,a},{1,b}`
//!   yields `a,b` for `foldl` and `b,a` for `foldr`). An **empty table returns
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
//! * **`ets:setopts(T, {protection, P})`** — the C also accepts the
//!   `{protection, private|protected|public}` option (`am_protection` arm of
//!   `ets_setopts_2`) and changes `ets:info(T, protection)`. `beam_ets::Table`
//!   exposes `access()` but **no post-creation setter** (`TableOptions::set_access`
//!   only affects *creation*), so this one sub-option is the typed
//!   `unsupported_bif` gap (the whole option list is validated first, so a
//!   `{protection, _}` call leaves the table untouched). The missing API is
//!   `beam_ets::Table::set_access(&mut self, Access)` — or a `SetOpt::Access(Access)`
//!   variant handled by `Table::setopts` — carrying the same **owner write gate**
//!   `Table::setopts` already applies (the caller must be the table's owner; the C
//!   is `ets_setopts_2`'s `DB_WRITE` acquisition plus its owner test). Until such a
//!   setter exists there is no way to change the protection without fabricating it.
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
//! * **`ets:slot/2`** — the objects of an internal B-tree node / hash bucket. The
//!   workspace contract (§2) explicitly leaves the interior layout open, so the
//!   value is interior-specific and cannot be reproduced faithfully.
//! * **`ets:internal_request_all/0`** — an underscore-internal C helper: it answers
//!   this scheduler's *fan-in reference* (`#Ref<…>`), an implementation-specific
//!   value, not part of the documented API. (`internal_delete_all/2` and
//!   `internal_select_delete/2` are implemented in [`super::extra`].)
//!
//! # Runtime handoffs (not this crate's to do)
//!
//! * **`{'ETS-TRANSFER', Tid, FromPid, GiftData}`** — the oracle's
//!   `ets:give_away/3` (and an heir stepping in) **delivers** this message to the
//!   new owner (`send_ets_transfer_message`, `erl_db.c:4943`). This crate performs
//!   the observable ownership transfer only; the delivery is a message-passing
//!   concern (`beam_rt`). It is deliberately **not** fabricated here.
//! * **Process liveness** — the C's `give_away`/`set_heir` resolve the target with
//!   `erts_pid2proc`/`erts_proc_lookup`, so a dead pid is `badarg` for
//!   `give_away/3` and clears the heir for `setopts/2`. Liveness is asked of the
//!   optional [`ProcessHook`](crate::context::ProcessHook)
//!   ([`BifContext::proc_is_alive`]); with **no hook installed** it cannot be
//!   answered and the pid is taken structurally (documented M1 gap) —
//!   `beam_rt` installs the hook, so the real runtime is exact.

use beam_common::Result;
use beam_ets::{Heir, SetOpt, Table};
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

/// `ets:setopts(Tab, Options)` — installs or clears the table's heir and returns
/// `true`.
///
/// Reproduces the accept/reject matrix of the C `ets_setopts_2`
/// (`erl_db.c:3151`): the second argument is a proper list of options, **or a
/// single option tuple** (the C wraps a tuple in a one-element list), every
/// element being a tuple of arity ≥ 2 whose first element is a **known** option
/// tag. `heir` accepts `none` (arity 2) or an internal pid (arity 2 ⇒ the gift
/// data is `[]`, arity 3 ⇒ the given data); a second `heir` option is `badarg`.
/// `protection` is recognised (so a malformed one is still `badarg`) but **cannot
/// be applied** — see the module docs — and answers the typed `unsupported_bif`.
///
/// The C applies the options only after the whole list has been validated and the
/// **owner** check has passed, so a caller who is not the table's owner gets
/// `badarg` (the access gate denies a non-owner of a `protected`/`private` table,
/// and the owner check the rest).
fn setopts_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve_handle(ctx, args[0])?;
    let (heir, protection) = parse_setopts(ctx.atoms(), args[1])?;
    if protection {
        // The C changes `protection`; `beam_ets::Table` has no access setter, so
        // the whole call is the typed gap and the table is left untouched.
        return Err(unsupported_bif());
    }
    let caller = caller_owner(ctx);
    let opt = match heir {
        None => None,
        Some(HeirOpt::Clear) => Some(SetOpt::ClearHeir),
        Some(HeirOpt::Install { pid, data }) => {
            // `set_heir` (`erl_db.c:5324`) clears the heir when the pid is not
            // alive; liveness is only answerable with a `ProcessHook` installed.
            if pid_liveness(ctx, pid)? == Some(false) {
                Some(SetOpt::ClearHeir)
            } else {
                Some(SetOpt::Heir(Heir {
                    owner: owner_of(pid),
                    data,
                }))
            }
        }
    };
    let mut table = take_table_gated(ctx, handle, Gate::Write)?;
    let outcome = if table.owner() != Some(caller) {
        // Even an empty option list requires the owner (`ets_setopts_2` checks
        // `tb->common.owner != BIF_P->common.id` unconditionally).
        Err(badarg())
    } else if let Some(opt) = opt {
        table.as_requester(Some(caller)).setopts(opt)
    } else {
        Ok(())
    };
    restore_table(ctx, handle, table)?;
    outcome?;
    ctx.boolean(true)
}

/// Parses an `ets:setopts/2` option list (see [`setopts_2`]), returning the
/// `{heir, …}` option and whether a `{protection, _}` option appeared.
fn parse_setopts(atoms: &AtomTable, options: Eterm) -> Result<(Option<HeirOpt>, bool)> {
    let elems = if is_tuple(options) {
        vec![options]
    } else if is_list(options) || is_nil(options) {
        proper_list(options).ok_or_else(badarg)?
    } else {
        return Err(badarg());
    };
    let mut heir: Option<HeirOpt> = None;
    let mut protection = false;
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
                if protection || tuple_size(elem) != 2 {
                    return Err(badarg());
                }
                let value = tuple_element(elem, 1);
                if !is_atom(value)
                    || !matches!(
                        atoms.name(atom_val(value)),
                        Some("private") | Some("protected") | Some("public")
                    )
                {
                    return Err(badarg());
                }
                protection = true;
            }
            _ => return Err(badarg()),
        }
    }
    Ok((heir, protection))
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

/// `ets:foldr(Fun, Acc0, Tab)` — as [`foldl_3`], but the objects are visited in
/// the exact reverse order (the oracle walks the keys last-to-first and each
/// key's object list right-to-left).
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
    let mut acc = acc0;
    if reverse {
        for &object in objects.iter().rev() {
            acc = ctx.call_fun(fun, &[object, acc])?;
        }
    } else {
        for &object in objects.iter() {
            acc = ctx.call_fun(fun, &[object, acc])?;
        }
    }
    Ok(acc)
}

/// Collects every object of `table` in forward traversal order (keys through
/// `first`/`next`, each key's objects in lookup order) onto the caller's heap.
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
        for object in table.lookup(current, atoms) {
            out.push(copy_out(object, ctx)?);
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

    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_term::NIL;

    use crate::context::BifContext;
    use crate::registry::BifRegistry;

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
        let (opt, prot) = parse_setopts(ctx.atoms(), list).expect("{heir, none}");
        assert!(matches!(opt, Some(HeirOpt::Clear)));
        assert!(!prot);

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

        // A `{protection, public}` option parses but is reported unservable.
        let list = heir_list(&mut ctx, &[protection, public]);
        let (opt, prot) = parse_setopts(ctx.atoms(), list).expect("{protection, public}");
        assert!(opt.is_none());
        assert!(prot);

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
        let (opt, prot) = parse_setopts(ctx.atoms(), NIL).expect("[]");
        assert!(opt.is_none() && !prot);
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
}
