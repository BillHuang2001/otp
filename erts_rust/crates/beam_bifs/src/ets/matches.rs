//! The **match family** of the `ets` bridge: `ets:match/1,2,3`,
//! `ets:match_object/1,2,3` and the count/delete verbs
//! (`ets:select_count/2`, `ets:select_delete/2`, `ets:match_delete/2`).
//!
//! These BIFs are the pattern-based half of the `ets` API. A *pattern* is a term
//! whose atoms `'$N'` are variables and whose `'_'` is a wildcard; `beam_ets`
//! compiles it into the one-clause spec it is defined by and runs the same
//! executor the `ets:select` family uses:
//!
//! * `ets:match(T, P)` is `ets:select(T, [{P, [], ['$$']}])` — per matching object,
//!   the ascending list of its bound `'$N'` variables (`erl_db.c` `ets_match_2`);
//! * `ets:match_object(T, P)` is `ets:select(T, [{P, [], ['$_']}])` — the whole
//!   object (`ets_match_object_2`);
//! * `ets:match_delete(T, P)` is `ets:select_delete(T, [{P, [], [true]}])` — its
//!   body is the **literal `true`**, not `'$_'` (an `'$_'` body would never satisfy
//!   `select_delete`'s "the body value is `true`" test and would delete nothing).
//!
//! The `/3` forms take the oracle's `limit` and produce a continuation when the
//! limit is reached; the `/1` forms resume it. Both are shared verbatim with
//! `ets:select/3`+`ets:select/1` through [`super::finish_chunk`] and
//! [`super::resume_continuation`], so the chunk rendering (including the **bare**
//! `'$end_of_table'` rule) has exactly one implementation.
//!
//! # Return values (verified against the system `erl`, OTP 29 / erts-17.1)
//!
//! * `ets:match/2` and `ets:match_object/2` return a list of results — the empty
//!   list, never `'$end_of_table'`, when nothing matched.
//! * `ets:select_count/2` returns the number of objects whose clause value is
//!   `true`; `ets:select_delete/2` returns the number of objects deleted.
//! * `ets:match_delete/2` returns the atom **`true`** — not a count. It is a
//!   *stdlib* function, not a BIF (`bif.tab` has only `ets:internal_select_delete/2`;
//!   `lib/stdlib/src/ets.erl:2777` is
//!   `match_delete(T, P) -> ets:select_delete(T, [{P,[],[true]}]), true.`), so the
//!   deleted count [`Table::match_delete`] reports is discarded here. Registering
//!   `ets:match_delete/2` as a Rust fast path is the same "stdlib export, identical
//!   observable result" convention the crate's `lists`/`ets:select_delete` entries
//!   follow.
//!
//! `limit 0` is `badarg` (rejected by `beam_ets`' `select_limit`), as is a
//! negative or non-integer limit ([`super::element_pos`]) and a non-continuation
//! handed to a `/1` form.

use beam_common::Result;
use beam_ets::{compile, Table};
use beam_term::Eterm;

use super::{
    copy_out_list, element_pos, finish_chunk, restore_table, resume_continuation, take_table,
};
use crate::access::ref_thing_id;
use crate::context::BifContext;
use crate::error::system_limit;
use crate::registry::BifRegistry;
/// Registers the match family and the count/delete BIFs.
pub(super) fn register(reg: &mut BifRegistry) {
    reg.register("ets", "match", 1, match_1);
    reg.register("ets", "match", 2, match_2);
    reg.register("ets", "match", 3, match_3);
    reg.register("ets", "match_object", 1, match_object_1);
    reg.register("ets", "match_object", 2, match_object_2);
    reg.register("ets", "match_object", 3, match_object_3);
    reg.register("ets", "select_count", 2, select_count_2);
    reg.register("ets", "select_delete", 2, select_delete_2);
    reg.register("ets", "match_delete", 2, match_delete_2);
}

// -------------------------------------------------------------------- match

/// `ets:match(Tab, Pattern)` — for every matching object, the list of its bound
/// `'$N'` variables, ascending.
///
/// The results are the oracle's **lists**, and the caller's iteration order is the
/// table's (an unspecified order for the hash kinds — `ordered_set` is key order).
fn match_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let table = take_table(ctx, handle)?;
    // `Table::match_` needs the atom table *and* the caller heap at once; the
    // context lends them through two conflicting borrows, so it is given a clone
    // of the atom table (identical indices, and the call only reads it — the same
    // pattern `select_2`/`update_counter` use).
    let atoms = ctx.atoms().clone();
    let outcome = table.match_(args[1], &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    copy_out_list(outcome?, ctx)
}

/// `ets:match(Tab, Pattern, Limit)` — as [`match_2`], but stop after at most
/// `Limit` results, yielding `{Results, Continuation}` when the limit is reached.
///
/// A non-integer or negative `Limit` is `badarg` here; `0` is rejected by
/// `beam_ets`' `select_limit` (matching the oracle).
fn match_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let limit = element_pos(args[2])?;
    let table = take_table(ctx, handle)?;
    let atoms = ctx.atoms().clone();
    let outcome = table.match_limit(args[1], limit, &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    let (results, cont) = outcome?;
    finish_chunk(ctx, handle, results, cont)
}

/// `ets:match(Continuation)` — resumes a [`match_3`] traversal.
///
/// The oracle accepts *any* continuation ref here, including one produced by
/// `ets:select/3` (verified: `ets:match(SelectContinuation)` resumes it and returns
/// the spec's own body values), which falls out of the continuation carrying its
/// compiled spec.
fn match_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    resume_continuation(ctx, args[0], Table::match_continue)
}

// ------------------------------------------------------------- match_object

/// `ets:match_object(Tab, Pattern)` — every matching object, whole.
fn match_object_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let table = take_table(ctx, handle)?;
    let atoms = ctx.atoms().clone();
    let outcome = table.match_object(args[1], &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    copy_out_list(outcome?, ctx)
}

/// `ets:match_object(Tab, Pattern, Limit)` — as [`match_object_2`], limit-chunked.
fn match_object_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let limit = element_pos(args[2])?;
    let table = take_table(ctx, handle)?;
    let atoms = ctx.atoms().clone();
    let outcome = table.match_object_limit(args[1], limit, &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    let (results, cont) = outcome?;
    finish_chunk(ctx, handle, results, cont)
}

/// `ets:match_object(Continuation)` — resumes a [`match_object_3`] traversal.
fn match_object_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    resume_continuation(ctx, args[0], Table::match_object_continue)
}

// --------------------------------------------------------- count and delete

/// `ets:select_count(Tab, MatchSpec)` — how many objects the spec's body counts
/// as `true`, as a small integer.
///
/// `MatchSpec` is a raw match-spec list compiled by [`compile`]; a spec outside the
/// supported subset is `badarg`.
fn select_count_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let spec = compile(args[1], ctx.atoms())?;
    let table = take_table(ctx, handle)?;
    let atoms = ctx.atoms().clone();
    let outcome = table.select_count(&spec, &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    count_term(ctx, outcome?)
}

/// `ets:select_delete(Tab, MatchSpec)` — deletes every object whose clause value is
/// `true` and returns how many were deleted.
///
/// The oracle's `ets:select_delete/2` is the stdlib wrapper over
/// `ets:internal_select_delete/2`; its special case for the match-all spec
/// (`ets.erl:1865`) routes through `ets:internal_delete_all/2`, which returns the
/// same count this traversal does.
fn select_delete_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let spec = compile(args[1], ctx.atoms())?;
    let mut table = take_table(ctx, handle)?;
    let atoms = ctx.atoms().clone();
    let outcome = table.select_delete(&spec, &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    count_term(ctx, outcome?)
}

/// `ets:match_delete(Tab, Pattern)` — deletes every object matching `Pattern` and
/// returns the atom `true` (see the module docs: the oracle's stdlib wrapper
/// discards the deleted count).
fn match_delete_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let mut table = take_table(ctx, handle)?;
    let atoms = ctx.atoms().clone();
    let outcome = table.match_delete(args[1], &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    // The count is not observable through this BIF (the oracle returns `true`), but
    // the call's failure is: propagate `badarg` rather than reporting a deletion.
    outcome?;
    ctx.boolean(true)
}

/// The `usize` a count/delete BIF returns, as a small integer.
///
/// A count above the small-int range is unrepresentable, so it is `system_limit`
/// rather than a truncated value (the `arith::int_from_i64` convention).
fn count_term(ctx: &mut BifContext<'_>, count: usize) -> Result<Eterm> {
    let count = i64::try_from(count).map_err(|_| system_limit())?;
    ctx.int(count)
}