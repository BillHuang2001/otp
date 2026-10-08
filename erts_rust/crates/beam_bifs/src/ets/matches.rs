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

//! The **match family** of the `ets` bridge: `ets:match/1,2,3`,
//! `ets:match_object/1,2,3` and the verb BIFs (`ets:select_count/2`,
//! `ets:select_delete/2`, `ets:select_replace/2`, `ets:match_delete/2`).
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
//!   `true`; `ets:select_delete/2` returns the number of objects deleted;
//!   `ets:select_replace/2` returns the number of objects replaced (and is
//!   `badarg` on a `bag` before the spec or the contents are looked at).
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
//!
//! Every table argument is resolved through [`super::resolve::resolve_handle`] (the
//! handle ref thing or a `named_table`'s name atom) and the operation is
//! access-gated as the calling process like the `ets:select` family: the traversals
//! (`match/2,3`, `match_object/2,3`, the `/1` continuations and `select_count/2`)
//! require **read** access, the deleting/replacing verbs (`select_delete/2`,
//! `select_replace/2`, `match_delete/2`) **write** access. A denied call is `badarg`
//! and leaves the table — and an unconsumed continuation — in place.

use beam_common::Result;
use beam_ets::{compile, Table};
use beam_term::Eterm;

use super::{
    copy_out_list, element_pos, finish_chunk, resolve, restore_table, resume_continuation,
    take_table_gated,
};
use crate::context::BifContext;
use crate::error::system_limit;
use crate::registry::BifRegistry;

/// Registers the match family and the count/delete/replace BIFs.
pub(super) fn register(reg: &mut BifRegistry) {
    reg.register("ets", "match", 1, match_1);
    reg.register("ets", "match", 2, match_2);
    reg.register("ets", "match", 3, match_3);
    reg.register("ets", "match_object", 1, match_object_1);
    reg.register("ets", "match_object", 2, match_object_2);
    reg.register("ets", "match_object", 3, match_object_3);
    reg.register("ets", "select_count", 2, select_count_2);
    reg.register("ets", "select_delete", 2, select_delete_2);
    reg.register("ets", "select_replace", 2, select_replace_2);
    reg.register("ets", "match_delete", 2, match_delete_2);
}

// -------------------------------------------------------------------- match

/// `ets:match(Tab, Pattern)` — for every matching object, the list of its bound
/// `'$N'` variables, ascending.
///
/// The results are the oracle's **lists**, and the caller's iteration order is the
/// table's (an unspecified order for the hash kinds — `ordered_set` is key order).
fn match_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
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
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let limit = element_pos(args[2])?;
    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
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
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    let atoms = ctx.atoms().clone();
    let outcome = table.match_object(args[1], &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    copy_out_list(outcome?, ctx)
}

/// `ets:match_object(Tab, Pattern, Limit)` — as [`match_object_2`], limit-chunked.
fn match_object_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let limit = element_pos(args[2])?;
    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
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
/// supported subset is `badarg`. Like every other traversal it requires **read**
/// access to the table (verified: it is `badarg` for a non-owner of a `private`
/// table and allowed for a non-owner of a `protected` one).
fn select_count_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let spec = compile(args[1], ctx.atoms())?;
    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
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
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let spec = compile(args[1], ctx.atoms())?;
    let mut table = take_table_gated(ctx, handle, resolve::Gate::Write)?;
    let atoms = ctx.atoms().clone();
    let outcome = table.select_delete(&spec, &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    count_term(ctx, outcome?)
}

/// `ets:select_replace(Tab, MatchSpec)` — replace every object the spec selects
/// with the value of its clause's body, returning how many objects were replaced.
///
/// `MatchSpec` is a raw match-spec list compiled by [`compile`]; a **compiled**
/// spec from `ets:match_spec_compile/1` is *not* accepted here — the oracle raises
/// `badarg` (verified against OTP 29 / erts-17.1), and [`compile`] rejects the ref
/// thing for the same reason it rejects any non-list spec. A `bag` table is
/// `badarg` before the spec or the contents are looked at
/// (`beam_ets::Table::select_replace`); a replacement that does not keep the
/// matched object's key makes the whole call `badarg` with the table unchanged.
fn select_replace_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let spec = compile(args[1], ctx.atoms())?;
    let mut table = take_table_gated(ctx, handle, resolve::Gate::Write)?;
    let atoms = ctx.atoms().clone();
    let outcome = table.select_replace(&spec, &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    count_term(ctx, outcome?)
}

/// `ets:match_delete(Tab, Pattern)` — deletes every object matching `Pattern` and
/// returns the atom `true` (see the module docs: the oracle's stdlib wrapper
/// discards the deleted count).
fn match_delete_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let mut table = take_table_gated(ctx, handle, resolve::Gate::Write)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{atom_val, signed_value, AtomTable};

    use crate::access::{ref_thing_id, tuple_element};
    use crate::ets::{insert_2, lookup_2, new_2, TableRegistry};
    use crate::print::proper_list;

    /// The atom names the tests below need.
    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "true",
            "set",
            "bag",
            "duplicate_bag",
            "mytab",
            "bagtab",
            "duptab",
            "a",
            "b",
            "z",
            "$1",
            "$2",
            "$end_of_table",
        ]);
        atoms
    }

    fn parts() -> (Heap, AtomTable, TableRegistry) {
        (
            Heap::new(65536).expect("test heap"),
            seeded_atoms(),
            TableRegistry::new(),
        )
    }

    /// Creates a `set` table through the `ets:new/2` BIF and returns its handle.
    fn new_table(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
        new_table_kind(ctx, name, "set")
    }

    /// Creates a table of `kind` through the `ets:new/2` BIF.
    fn new_table_kind(ctx: &mut BifContext<'_>, name: &str, kind: &str) -> Eterm {
        let name = ctx.atom(name).expect("seeded");
        let kind = ctx.atom(kind).expect("seeded");
        let opts = ctx.list(&[kind]).expect("fits");
        new_2(ctx, &[name, opts]).expect("ets:new")
    }

    /// The 2-tuple `{Key, Value}` the tests insert.
    fn pair(ctx: &mut BifContext<'_>, key: &str, value: i64) -> Eterm {
        let k = ctx.atom(key).expect("seeded");
        let v = ctx.int(value).expect("fits");
        ctx.tuple(&[k, v]).expect("fits")
    }

    /// The `{'$1','$2'}` pattern (both variables bound).
    fn both_vars(ctx: &mut BifContext<'_>) -> Eterm {
        let v1 = ctx.atom("$1").expect("seeded");
        let v2 = ctx.atom("$2").expect("seeded");
        ctx.tuple(&[v1, v2]).expect("fits")
    }

    /// The `[Key, Value]` binding list `ets:match/2` yields for `key`/`value`.
    fn expected_pair(ctx: &mut BifContext<'_>, key: &str, value: i64) -> Eterm {
        let k = ctx.atom(key).expect("seeded");
        let v = ctx.int(value).expect("fits");
        ctx.list(&[k, v]).expect("fits")
    }

    /// Inserts `{a,1}` and `{b,2}` into `handle`.
    fn two_objects(ctx: &mut BifContext<'_>, handle: Eterm) {
        for (key, value) in [("a", 1), ("b", 2)] {
            let obj = pair(ctx, key, value);
            insert_2(ctx, &[handle, obj]).expect("insert");
        }
    }

    /// The counting spec `[{{'$1','$2'}, [], [true]}]`.
    fn true_spec(ctx: &mut BifContext<'_>) -> Eterm {
        let v1 = ctx.atom("$1").expect("seeded");
        let v2 = ctx.atom("$2").expect("seeded");
        let head = ctx.tuple(&[v1, v2]).expect("fits");
        let guards = ctx.list(&[]).expect("fits");
        let body = ctx.list(&[ctx.atom("true").expect("seeded")]).expect("fits");
        let clause = ctx.tuple(&[head, guards, body]).expect("fits");
        ctx.list(&[clause]).expect("fits")
    }

    /// The replacing spec `[{{'$1','$2'}, [], [{{'$1',b}}]}]` — matches every
    /// 2-tuple and replaces it with `{Key, b}`, keeping the key at position 1.
    fn replace_spec(ctx: &mut BifContext<'_>) -> Eterm {
        let v1 = ctx.atom("$1").expect("seeded");
        let v2 = ctx.atom("$2").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let head = ctx.tuple(&[v1, v2]).expect("fits");
        let guards = ctx.list(&[]).expect("fits");
        // The `{{ … }}` construction form: a 1-tuple holding the tuple to build.
        let constructed = ctx.tuple(&[v1, b]).expect("fits");
        let body_item = ctx.tuple(&[constructed]).expect("fits");
        let body = ctx.list(&[body_item]).expect("fits");
        let clause = ctx.tuple(&[head, guards, body]).expect("fits");
        ctx.list(&[clause]).expect("fits")
    }

    /// `[{{z}, [], [true]}]` — a spec whose head matches no 2-tuple object.
    fn no_match_spec(ctx: &mut BifContext<'_>) -> Eterm {
        let z = ctx.atom("z").expect("seeded");
        let head = ctx.tuple(&[z]).expect("fits");
        let guards = ctx.list(&[]).expect("fits");
        let body = ctx.list(&[ctx.atom("true").expect("seeded")]).expect("fits");
        let clause = ctx.tuple(&[head, guards, body]).expect("fits");
        ctx.list(&[clause]).expect("fits")
    }

    #[test]
    fn count_term_is_a_small_int_and_system_limit_beyond_the_small_range() {
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

    #[test]
    fn match_and_match_object_bind_every_variable() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");
        two_objects(&mut ctx, handle);

        // `ets:match/2` ⇒ one `[Key, Value]` list per matching object, in the
        // pattern's ascending variable order. A `set` iterates in an unspecified
        // order, so the assertion checks membership.
        let pattern = both_vars(&mut ctx);
        let got = match_2(&mut ctx, &[handle, pattern]).expect("ets:match/2");
        let rows = proper_list(got).expect("a proper list of binding lists");
        assert_eq!(rows.len(), 2);
        let a = expected_pair(&mut ctx, "a", 1);
        let b = expected_pair(&mut ctx, "b", 2);
        assert!(
            beam_term::compare::eq(rows[0], a, &atoms) || beam_term::compare::eq(rows[0], b, &atoms),
            "the first row bound neither a/1 nor b/2"
        );

        // `ets:match_object/2` ⇒ the objects themselves.
        let pattern = both_vars(&mut ctx);
        let got = match_object_2(&mut ctx, &[handle, pattern]).expect("ets:match_object/2");
        let objects = proper_list(got).expect("a proper list of objects");
        assert_eq!(objects.len(), 2);
        let a_object = pair(&mut ctx, "a", 1);
        assert!(
            beam_term::compare::eq(objects[0], a_object, &atoms)
                || beam_term::compare::eq(objects[1], a_object, &atoms),
            "neither object was the `{{a,1}}` tuple"
        );

        // Neither `/2` form yields `'$end_of_table'`: a pattern matching nothing
        // yields the empty list.
        let z = ctx.atom("z").expect("seeded");
        let no_match = ctx.tuple(&[z, z]).expect("fits");
        let empty = match_2(&mut ctx, &[handle, no_match]).expect("ets:match/2 no match");
        assert_eq!(proper_list(empty).expect("list").len(), 0);
        let empty =
            match_object_2(&mut ctx, &[handle, no_match]).expect("ets:match_object/2 no match");
        assert_eq!(proper_list(empty).expect("list").len(), 0);
    }

    #[test]
    fn match_3_chunks_and_the_resume_renders_the_exhausted_chunk_bare() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");
        two_objects(&mut ctx, handle);
        let two = ctx.int(2).expect("fits");

        // `limit` 2 on a 2-object table produces a chunk **with** a continuation
        // (the oracle returns one whenever exactly `limit` results were produced),
        // and the chunk is the 2-tuple `{Results, Cont}`.
        let pattern = both_vars(&mut ctx);
        let chunk = match_3(&mut ctx, &[handle, pattern, two]).expect("ets:match/3");
        assert_eq!(proper_list(tuple_element(chunk, 0)).expect("list").len(), 2);
        let cont = tuple_element(chunk, 1);
        assert!(ref_thing_id(cont).is_ok(), "the chunk carries no continuation");

        // Resuming finds nothing left, which the oracle renders as the **bare**
        // `'$end_of_table'` atom (not a 1-tuple or a 2-tuple).
        let tail = match_1(&mut ctx, &[cont]).expect("ets:match/1");
        assert_eq!(atoms.name(atom_val(tail)), Some("$end_of_table"));

        // The `match_object` chunking forms behave identically.
        let pattern = both_vars(&mut ctx);
        let chunk = match_object_3(&mut ctx, &[handle, pattern, two]).expect("ets:match_object/3");
        let cont = tuple_element(chunk, 1);
        let tail = match_object_1(&mut ctx, &[cont]).expect("ets:match_object/1");
        assert_eq!(atoms.name(atom_val(tail)), Some("$end_of_table"));

        // `limit == 0` is `badarg` (rejected by `beam_ets`' `select_limit`).
        let pattern = both_vars(&mut ctx);
        let zero = ctx.int(0).expect("fits");
        assert!(matches!(
            match_3(&mut ctx, &[handle, pattern, zero]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn the_count_and_delete_bifs_report_the_oracle_values() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");
        two_objects(&mut ctx, handle);

        let spec = true_spec(&mut ctx);
        assert_eq!(
            signed_value(select_count_2(&mut ctx, &[handle, spec]).expect("count")),
            2
        );
        assert_eq!(
            signed_value(select_delete_2(&mut ctx, &[handle, spec]).expect("delete")),
            2
        );
        assert_eq!(
            signed_value(select_count_2(&mut ctx, &[handle, spec]).expect("count")),
            0
        );

        // `ets:match_delete/2` returns the atom `true` — never the deleted count.
        two_objects(&mut ctx, handle);
        let pattern = both_vars(&mut ctx);
        let result = match_delete_2(&mut ctx, &[handle, pattern]).expect("ets:match_delete/2");
        assert_eq!(atoms.name(atom_val(result)), Some("true"));
        assert_eq!(
            signed_value(select_count_2(&mut ctx, &[handle, spec]).expect("count")),
            0
        );
    }

    #[test]
    fn select_replace_swaps_kept_key_objects_and_rejects_a_bag() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");
        two_objects(&mut ctx, handle);

        // On a `set`: both objects match, both replacements keep the key, so the
        // count is 2 and `{a,1}`/`{b,2}` become `{a,b}`/`{b,b}` (verified).
        let spec = replace_spec(&mut ctx);
        let count = select_replace_2(&mut ctx, &[handle, spec]).expect("ets:select_replace/2");
        assert_eq!(signed_value(count), 2);
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let a_b = ctx.tuple(&[a, b]).expect("fits");
        let b_b = ctx.tuple(&[b, b]).expect("fits");
        let stored = proper_list(lookup_2(&mut ctx, &[handle, a]).expect("lookup a")).expect("list");
        assert_eq!(stored.len(), 1);
        assert!(beam_term::compare::eq(stored[0], a_b, &atoms));
        let stored = proper_list(lookup_2(&mut ctx, &[handle, b]).expect("lookup b")).expect("list");
        assert_eq!(stored.len(), 1);
        assert!(beam_term::compare::eq(stored[0], b_b, &atoms));

        // A spec that matches nothing replaces nothing (the oracle's count is 0).
        let spec = no_match_spec(&mut ctx);
        let count = select_replace_2(&mut ctx, &[handle, spec]).expect("no match");
        assert_eq!(signed_value(count), 0);

        // On a `bag` the whole call is `badarg` — before the spec or the contents
        // are looked at, so a never-matching spec is `badarg` too (verified).
        let bag = new_table_kind(&mut ctx, "bagtab", "bag");
        let spec = replace_spec(&mut ctx);
        assert!(matches!(
            select_replace_2(&mut ctx, &[bag, spec]),
            Err(BeamError::BadArg)
        ));
        let spec = no_match_spec(&mut ctx);
        assert!(matches!(
            select_replace_2(&mut ctx, &[bag, spec]),
            Err(BeamError::BadArg)
        ));

        // A `duplicate_bag` is supported (`{a,1}` ⇒ `{a,b}`).
        let dup = new_table_kind(&mut ctx, "duptab", "duplicate_bag");
        let obj = pair(&mut ctx, "a", 1);
        insert_2(&mut ctx, &[dup, obj]).expect("insert");
        let spec = replace_spec(&mut ctx);
        let count = select_replace_2(&mut ctx, &[dup, spec]).expect("duplicate_bag replace");
        assert_eq!(signed_value(count), 1);
        let stored = proper_list(lookup_2(&mut ctx, &[dup, a]).expect("lookup")).expect("list");
        assert_eq!(stored.len(), 1);
        assert!(beam_term::compare::eq(stored[0], a_b, &atoms));

        // A malformed spec and a non-ref handle are `badarg`.
        let one = ctx.int(1).expect("fits");
        assert!(matches!(
            select_replace_2(&mut ctx, &[handle, one]),
            Err(BeamError::BadArg)
        ));
        let spec = replace_spec(&mut ctx);
        assert!(matches!(
            select_replace_2(&mut ctx, &[one, spec]),
            Err(BeamError::BadArg)
        ));

        // A *compiled* spec (a ref thing) is not a select-family spec: the oracle
        // raises `badarg`, and so does this — the ref fails `compile`. A table
        // handle is exactly such a ref thing the specs registry does not know.
        assert!(matches!(
            select_replace_2(&mut ctx, &[handle, handle]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn this_module_registers_its_ten_keys() {
        let reg = BifRegistry::with_core_bifs();
        for (name, arity) in [
            ("match", 1u32),
            ("match", 2),
            ("match", 3),
            ("match_object", 1),
            ("match_object", 2),
            ("match_object", 3),
            ("select_count", 2),
            ("select_delete", 2),
            ("select_replace", 2),
            ("match_delete", 2),
        ] {
            assert!(reg.contains("ets", name, arity), "ets:{name}/{arity}");
        }
        // The registry is the *composition* of this module's roster with the parent
        // module's, so an `ets:new/2` key is present too.
        assert!(reg.contains("ets", "new", 2));
    }
}