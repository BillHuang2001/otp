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

//! The reverse select family: `ets:select_reverse/1,2,3` — thin wrappers over
//! `beam_ets`'s native reverse primitives.
//!
//! # What "reverse" means (oracle-verified, erts-17.1)
//!
//! Only an **`ordered_set`** has a documented backward iteration order, and there
//! the results come in **descending key order**. For the hash kinds
//! (`set`/`bag`/`duplicate_bag`) the oracle leaves the order unspecified, so
//! [`Table::select_reverse`], which visits `beam_ets`' iteration order backwards
//! (the reverse of `ets:select/2`), is within the oracle's freedom.
//!
//! # The BIFs are thin wrappers over `beam_ets`
//!
//! `ets:select_reverse/2` is [`Table::select_reverse`], `/3` is
//! [`Table::select_reverse_limit`], and a resume is the dispatcher's
//! [`Table::select_reverse_continue`] (see [`super::dispatch_continuation`]).
//! [`beam_ets::MatchContinuation`] carries the reverse direction internally, so a
//! resumption keeps walking backward without this module holding any state — the
//! direction is stored alongside the state in the **shared** continuation registry
//! ([`super::Continuation`]) so `ets:select/1` and `ets:select_reverse/1` can be
//! one BIF following it.
//!
//! # Direction stickiness
//!
//! The oracle treats `ets:select/1` and `ets:select_reverse/1` as **one** BIF that
//! follows the direction stored in the continuation (verified: `ets:select/1` on a
//! reverse continuation walks backward, `ets:select_reverse/1` on a forward one
//! walks forward). Both entry points dispatch through
//! [`super::dispatch_continuation`] → [`super::resume_continuation`], which follows
//! the stored direction; a term that is no continuation at all is `badarg`.

use beam_common::Result;
use beam_ets::compile;
use beam_term::Eterm;

use crate::context::BifContext;
use crate::registry::BifRegistry;

use super::{
    copy_out_list, element_pos, finish_chunk, resolve, restore_table, take_table_gated,
};

/// Registers the reverse select family (`ets:select_reverse/1,2,3`).
pub(super) fn register(reg: &mut BifRegistry) {
    reg.register("ets", "select_reverse", 1, select_reverse_1);
    reg.register("ets", "select_reverse", 2, select_reverse_2);
    reg.register("ets", "select_reverse", 3, select_reverse_3);
}

/// `ets:select_reverse(Tab, MatchSpec)` — the body values of every selected
/// object, in descending key order for an `ordered_set` and in the reverse of
/// `ets:select/2`'s order for a hash kind; the empty list when nothing matches.
///
/// `MatchSpec` is a raw match-spec list compiled by [`beam_ets::compile`]; a spec
/// outside the supported subset is `badarg`, as is an unknown or non-table first
/// argument.
fn select_reverse_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let spec = compile(args[1], ctx.atoms())?;
    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    // `Table::select_reverse` needs the atom table *and* the caller heap at once;
    // the same clone pattern `super::select_2` uses (identical indices, read-only).
    let atoms = ctx.atoms().clone();
    let outcome = table.select_reverse(&spec, &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    copy_out_list(outcome?, ctx)
}

/// `ets:select_reverse(Tab, MatchSpec, Limit)` — as [`select_reverse_2`], but stop
/// after at most `Limit` results and yield `{Results, Continuation}`.
///
/// `Limit` must be a positive integer (`0`, a negative and a non-integer value are
/// `badarg`, matching `ets:select/3`); an exhausted traversal renders the same way
/// the forward path does. The minted continuation is recorded with the reverse
/// direction (see [`super::Continuation`]), so `ets:select/1` resuming it walks
/// backward just as the oracle does.
fn select_reverse_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let spec = compile(args[1], ctx.atoms())?;
    // A non-integer or a negative limit is `badarg` here; `0` is rejected by
    // `select_reverse_limit` (matching the oracle, like `ets:select/3`).
    let limit = element_pos(args[2])?;

    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    let atoms = ctx.atoms().clone();
    let outcome = table.select_reverse_limit(&spec, limit, &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    let (results, cont) = outcome?;
    finish_chunk(ctx, handle, results, cont, true)
}

/// `ets:select_reverse(Continuation)` — the `/1` resume.
///
/// Shares [`super::dispatch_continuation`] with `ets:select/1`, so the direction
/// stored in the continuation decides the walk (see the module docs).
fn select_reverse_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    super::dispatch_continuation(ctx, args[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{atom_val, signed_value, AtomTable};

    use crate::access::{is_atom, is_nil, is_tuple, make_ref_thing, tuple_element, tuple_size};
    use crate::print::proper_list;

    use super::super::{insert_2, new_2, TableRegistry};

    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "true",
            "false",
            "ok",
            "set",
            "ordered_set",
            "bag",
            "duplicate_bag",
            "keypos",
            "public",
            "protected",
            "private",
            "named_table",
            "$end_of_table",
            "mytab",
            "dupb",
            "$1",
            "$2",
            "$_",
            "auto",
            "none",
            "node",
            "nonode@nohost",
            "undefined",
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

    /// Creates a table of `kind` named `name`, returning its handle term.
    fn new_table(ctx: &mut BifContext<'_>, name: &str, kind: &str) -> Eterm {
        let name = ctx.atom(name).expect("seeded");
        let kind = ctx.atom(kind).expect("seeded");
        let opts = ctx.list(&[kind]).expect("fits");
        new_2(ctx, &[name, opts]).expect("ets:new")
    }

    /// Inserts the `{K, K * 10}` objects for `keys`, in order.
    fn insert_all(ctx: &mut BifContext<'_>, handle: Eterm, keys: &[i64]) {
        for &key in keys {
            let k = ctx.int(key).expect("fits");
            let v = ctx.int(key * 10).expect("fits");
            let obj = ctx.tuple(&[k, v]).expect("fits");
            insert_2(ctx, &[handle, obj]).expect("insert");
        }
    }

    /// The raw spec `[{{'$1','$2'},[],['$1']}]` — the body is the key.
    fn key_spec(ctx: &mut BifContext<'_>) -> Eterm {
        let v1 = ctx.atom("$1").expect("seeded");
        let v2 = ctx.atom("$2").expect("seeded");
        let pattern = ctx.tuple(&[v1, v2]).expect("fits");
        let guards = ctx.list(&[]).expect("fits");
        let body = ctx.list(&[v1]).expect("fits");
        let clause = ctx.tuple(&[pattern, guards, body]).expect("fits");
        ctx.list(&[clause]).expect("fits")
    }

    /// The result list as `i64`s.
    fn ints(term: Eterm) -> Vec<i64> {
        proper_list(term)
            .expect("a proper result list")
            .into_iter()
            .map(signed_value)
            .collect()
    }

    /// The `{Results, Continuation}` chunk as `(ints, continuation?)`.
    fn chunk(term: Eterm) -> (Vec<i64>, Option<Eterm>) {
        assert!(is_tuple(term), "a chunk is a 2-tuple");
        assert_eq!(tuple_size(term), 2);
        let results = ints(tuple_element(term, 0));
        let second = tuple_element(term, 1);
        let second = if is_atom(second) { None } else { Some(second) };
        (results, second)
    }

    fn is_end(atoms: &AtomTable, term: Eterm) -> bool {
        is_atom(term) && atoms.name(atom_val(term)) == Some(super::super::END_OF_TABLE)
    }

    /// An `ordered_set` **descends**; a hash kind is left unspecified by the oracle,
    /// so the native form (the reverse of `ets:select/2`) is only required to be the
    /// same result multiset.
    #[test]
    fn ordered_set_descends_and_hash_kinds_hold_the_same_results() {
        for kind in ["ordered_set", "set", "bag", "duplicate_bag"] {
            let (mut heap, atoms, mut reg) = parts();
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
            let handle = new_table(&mut ctx, "mytab", kind);
            insert_all(&mut ctx, handle, &[1, 2, 3, 4]);
            let spec = key_spec(&mut ctx);

            let forward = super::super::select_2(&mut ctx, &[handle, spec]).expect("select/2");
            let reverse = select_reverse_2(&mut ctx, &[handle, spec]).expect("select_reverse/2");

            if kind == "ordered_set" {
                assert_eq!(ints(reverse), vec![4, 3, 2, 1], "{kind} descends");
            } else {
                // The hash-kind order is unspecified by the oracle, so only the
                // multiset is pinned (the native reverse is `select`'s reverse).
                let mut forward = ints(forward);
                let mut reverse = ints(reverse);
                forward.sort_unstable();
                reverse.sort_unstable();
                assert_eq!(forward, reverse, "{kind}: the same result multiset");
            }
        }
    }

    /// `/2` on an empty (or non-matching) table is the empty list.
    #[test]
    fn an_empty_result_is_the_empty_list() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab", "ordered_set");
        let spec = key_spec(&mut ctx);
        let empty = select_reverse_2(&mut ctx, &[handle, spec]).expect("select_reverse/2");
        assert!(is_nil(empty), "an empty result is the empty list");
        assert_eq!(ints(empty), Vec::<i64>::new());

        // …and `/3` on the empty table is the bare atom.
        let limit = ctx.int(3).expect("fits");
        let chunked =
            select_reverse_3(&mut ctx, &[handle, spec, limit]).expect("select_reverse/3");
        assert!(is_end(&atoms, chunked), "an empty chunk is '$end_of_table'");
    }

    /// The `/3` chunk rendering: a full chunk yields a continuation even at the
    /// last object, and the resume then renders the bare `'$end_of_table'`.
    #[test]
    fn select_reverse_3_chunks_like_the_forward_path() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab", "ordered_set");
        insert_all(&mut ctx, handle, &[1, 2, 3, 4]);
        let spec = key_spec(&mut ctx);
        let two = ctx.int(2).expect("fits");

        // Chunk 1: the two highest keys, plus a continuation.
        let (first, cont1) = chunk(select_reverse_3(&mut ctx, &[handle, spec, two]).expect("c1"));
        assert_eq!(first, vec![4, 3]);
        let cont1 = cont1.expect("chunk 1 produced a continuation");

        // Chunk 2, off the continuation.
        let (second, cont2) = chunk(select_reverse_1(&mut ctx, &[cont1]).expect("c2"));
        assert_eq!(second, vec![2, 1]);
        let cont2 = cont2.expect("chunk 2 produced a continuation");

        // Chunk 3: exhausted with no results ⇒ the bare atom.
        let third = select_reverse_1(&mut ctx, &[cont2]).expect("c3");
        assert!(is_end(&atoms, third), "the final chunk is the bare atom");

        // A limit past the end yields the results with the atom, not a continuation.
        let five = ctx.int(5).expect("fits");
        let (all, none) =
            chunk(select_reverse_3(&mut ctx, &[handle, spec, five]).expect("short chunk"));
        assert_eq!(all, vec![4, 3, 2, 1]);
        assert!(none.is_none(), "an exhausted traversal has no continuation");

        // A limit that lands exactly on the last object still yields one.
        let four = ctx.int(4).expect("fits");
        let (all, cont) = chunk(select_reverse_3(&mut ctx, &[handle, spec, four]).expect("full"));
        assert_eq!(all, vec![4, 3, 2, 1]);
        let cont = cont.expect("a full chunk always yields a continuation");
        assert!(is_end(&atoms,
            select_reverse_1(&mut ctx, &[cont]).expect("resume after a full chunk")
        ));
    }

    /// `ets:select/1` and `ets:select_reverse/1` share one dispatcher following the
    /// continuation's stored direction.
    #[test]
    fn the_two_1_forms_follow_the_stored_direction() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab", "ordered_set");
        insert_all(&mut ctx, handle, &[1, 2, 3, 4]);
        let spec = key_spec(&mut ctx);
        let two = ctx.int(2).expect("fits");

        // A forward continuation, resumed through `ets:select_reverse/1`: forward.
        let forward_cont = chunk(super::super::select_3(&mut ctx, &[handle, spec, two]).expect("f"))
            .1
            .expect("forward continuation");
        let (next, _) = chunk(select_reverse_1(&mut ctx, &[forward_cont]).expect("forward"));
        assert_eq!(next, vec![3, 4], "a forward continuation walks forward");

        // A reverse continuation, resumed through `ets:select/1`: backward.
        let reverse_cont =
            chunk(select_reverse_3(&mut ctx, &[handle, spec, two]).expect("r"))
                .1
                .expect("reverse continuation");
        let (next, _) =
            chunk(super::super::select_1(&mut ctx, &[reverse_cont]).expect("backward"));
        assert_eq!(next, vec![2, 1], "a reverse continuation walks backward");
    }

    /// A reverse continuation of a hash kind keeps walking backward too.
    #[test]
    fn a_hash_kind_reverse_continuation_walks_backward() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab", "set");
        insert_all(&mut ctx, handle, &[1, 2, 3, 4]);
        let spec = key_spec(&mut ctx);

        // The forward and reverse full scans are reverses of one another.
        let forward = ints(super::super::select_2(&mut ctx, &[handle, spec]).expect("select/2"));
        let reverse = ints(select_reverse_2(&mut ctx, &[handle, spec]).expect("select_reverse/2"));
        let mut expected = forward.clone();
        expected.reverse();
        assert_eq!(reverse, expected, "a hash-kind reverse is select's reverse");

        // Chunking walks the same reverse order across resumptions.
        let two = ctx.int(2).expect("fits");
        let (first, cont) = chunk(select_reverse_3(&mut ctx, &[handle, spec, two]).expect("c1"));
        let cont = cont.expect("a continuation");
        let (second, _) = chunk(select_reverse_1(&mut ctx, &[cont]).expect("c2"));
        let mut joined = first.clone();
        joined.extend(second);
        assert_eq!(joined, reverse, "the resumption continues the reverse walk");
    }

    /// The oracle's `badarg`s.
    #[test]
    fn bad_arguments_are_badarg() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab", "ordered_set");
        insert_all(&mut ctx, handle, &[1, 2, 3]);
        let spec = key_spec(&mut ctx);

        // Limit `0` (and a negative / non-integer limit).
        let zero = ctx.int(0).expect("fits");
        assert!(matches!(
            select_reverse_3(&mut ctx, &[handle, spec, zero]),
            Err(BeamError::BadArg)
        ));
        let minus = ctx.int(-1).expect("fits");
        assert!(matches!(
            select_reverse_3(&mut ctx, &[handle, spec, minus]),
            Err(BeamError::BadArg)
        ));
        let float = ctx.atom("auto").expect("seeded");
        assert!(matches!(
            select_reverse_3(&mut ctx, &[handle, spec, float]),
            Err(BeamError::BadArg)
        ));

        // A non-spec second argument.
        let bogus = ctx.int(42).expect("fits");
        assert!(matches!(
            select_reverse_2(&mut ctx, &[handle, bogus]),
            Err(BeamError::BadArg)
        ));

        // A `/1` argument that is not a live continuation.
        assert!(matches!(
            select_reverse_1(&mut ctx, &[bogus]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            select_reverse_1(&mut ctx, &[handle]),
            Err(BeamError::BadArg)
        ));

        // An unknown table.
        assert!(matches!(
            select_reverse_2(&mut ctx, &[bogus, spec]),
            Err(BeamError::BadArg)
        ));

        // A forged reference names no continuation.
        let forged = make_ref_thing(ctx.heap(), 1 << 45).expect("forged");
        assert!(matches!(
            select_reverse_1(&mut ctx, &[forged]),
            Err(BeamError::BadArg)
        ));

        // The failures never destroyed the table.
        assert_eq!(ints(select_reverse_2(&mut ctx, &[handle, spec]).unwrap()), vec![3, 2, 1]);
    }

    /// The live-oracle cross-check. The golden is what the system `erl` printed
    /// when it was measured (OTP 29.1.1 / erts-17.1); the test re-runs the oracle
    /// so a future OTP change cannot silently invalidate it, and asserts this
    /// crate's BIFs against the same golden. It skips (loudly) without `erl`.
    #[test]
    fn live_oracle_matches_the_pinned_goldens() {
        let script = concat!(
            "O = ets:new(o,[ordered_set],[]), ",
            "[ets:insert(O,{X,X*10}) || X <- [1,2,3,4]], ",
            "Spec = [{{'$1','$2'},[],['$1']}], ",
            "C1 = ets:select_reverse(O, Spec, 2), ",
            "C1r = ets:select_reverse(element(2, C1)), ",
            "C1rr = ets:select_reverse(element(2, C1r)), ",
            "Full = ets:select_reverse(O, Spec, 4), ",
            "Fat = ets:select_reverse(O, Spec, 9), ",
            "io:format(\"~p~n~p~n~p~n~p~n~p~n~p~n\", ",
            "[ets:select_reverse(O, Spec), C1, C1r, C1rr, Full, Fat])"
        );
        let golden = concat!(
            "[4,3,2,1]\n",
            "{[4,3],", "<CONT>", "}\n",
            "{[2,1],", "<CONT>", "}\n",
            "'$end_of_table'\n",
            "{[4,3,2,1],", "<CONT>", "}\n",
            "{[4,3,2,1],'$end_of_table'}"
        );
        if let Some(actual) = super::super::oracle(script) {
            // The continuation refs are runtime-unique, so the golden's `~p` image
            // is normalized: every `#Ref<...>` becomes the `<CONT>` placeholder.
            let normalized = normalize_refs(&actual);
            assert_eq!(
                normalized, golden,
                "the system `erl` no longer matches the goldens"
            );
        }

        // The same shape, asserted against this crate's own BIFs.
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab", "ordered_set");
        insert_all(&mut ctx, handle, &[1, 2, 3, 4]);
        let spec = key_spec(&mut ctx);

        assert_eq!(
            ints(select_reverse_2(&mut ctx, &[handle, spec]).unwrap()),
            vec![4, 3, 2, 1]
        );
        let two = ctx.int(2).expect("fits");
        let (first, cont1) = chunk(select_reverse_3(&mut ctx, &[handle, spec, two]).unwrap());
        assert_eq!(first, vec![4, 3]);
        let cont1 = cont1.expect("continuation");
        let (second, cont2) = chunk(select_reverse_1(&mut ctx, &[cont1]).unwrap());
        assert_eq!(second, vec![2, 1]);
        let cont2 = cont2.expect("continuation");
        assert!(is_end(&atoms, select_reverse_1(&mut ctx, &[cont2]).unwrap()));
        let four = ctx.int(4).expect("fits");
        assert!(chunk(select_reverse_3(&mut ctx, &[handle, spec, four]).unwrap())
            .1
            .is_some());
        let nine = ctx.int(9).expect("fits");
        let (all, none) = chunk(select_reverse_3(&mut ctx, &[handle, spec, nine]).unwrap());
        assert_eq!(all, vec![4, 3, 2, 1]);
        assert!(none.is_none());
    }

    /// Replaces every `#Ref<...>` in `text` with the `<CONT>` placeholder.
    fn normalize_refs(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("#Ref<") {
            out.push_str(&rest[..start]);
            let after = &rest[start..];
            match after.find('>') {
                Some(end) => {
                    out.push_str("<CONT>");
                    rest = &after[end + 1..];
                }
                None => {
                    out.push_str(after);
                    rest = "";
                }
            }
        }
        out.push_str(rest);
        out
    }
}
