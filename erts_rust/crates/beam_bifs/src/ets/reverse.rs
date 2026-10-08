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

//! The reverse select family: `ets:select_reverse/1,2,3`.
//!
//! # What "reverse" means (oracle-verified, erts-17.1)
//!
//! Only an **`ordered_set`** has a backward iteration order, and there the
//! results come in **descending key order**. For the hash kinds
//! (`set`/`bag`/`duplicate_bag`) the oracle *ignores* the reverse flag — the
//! order is the one `ets:select/2` produces (unspecified), verified by comparing
//! `ets:select/2` and `ets:select_reverse/2` on the same table. The flag is
//! therefore applied here, in the BIF layer ([`ordered_results`]), by reversing
//! the result vector of [`Table::select`] exactly when the kind is ordered.
//!
//! # The continuation (why this module owns a registry)
//!
//! [`beam_ets::MatchContinuation`] is opaque and forward-only: its fields are
//! private and there is no constructor, so a *backward* position cannot be
//! expressed with one — and it cannot be fabricated. This module therefore keeps
//! its own continuation registry (the `OnceLock<Mutex<HashMap<..>>>` id-registry
//! pattern [`super::specs`] and `src/binary.rs` use), keyed by a ref id from the
//! disjoint [`REVERSE_CONTINUATION_ID_BASE`] namespace, holding the table handle,
//! the **raw** spec term to recompile, the next position and the limit.
//!
//! A resume recomputes the whole reverse-ordered result list from the *current*
//! table (a continuation never holds table-owned terms across calls) and then
//! applies the stored `position`/`limit` — the same chunking rule
//! [`super::finish_chunk`] renders: a full chunk yields `{Results, <fresh cont>}`,
//! an exhausted traversal `{Results, '$end_of_table'}`, and an exhausted traversal
//! with no results the **bare** `'$end_of_table'` atom (verified: a 4-object table
//! with limit 2 renders the third chunk as the bare atom). A `/2` traversal that
//! matches nothing is the empty list, never `'$end_of_table'` (verified).
//!
//! The stored continuation is **immutable**: a resume mints a fresh entry for the
//! next chunk and leaves the offered one intact, so resuming the same
//! continuation twice reproduces the same chunk, exactly as the oracle's immutable
//! magic reference does. The registry therefore grows with the number of minted
//! continuations (a documented M1 leak, like the compiled-spec registry), never
//! with a wrong result.
//!
//! # Direction stickiness
//!
//! The oracle treats `ets:select/1` and `ets:select_reverse/1` as **one** BIF that
//! follows the direction stored in the continuation (verified: `ets:select/1` on a
//! reverse continuation walks backward, `ets:select_reverse/1` on a forward one
//! walks forward). Both entry points dispatch through
//! [`super::dispatch_continuation`], which offers the term to [`try_resume`] first
//! and falls back to the forward path; a term that is no continuation at all is
//! `badarg`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use beam_common::Result;
use beam_ets::{compile, Table};
use beam_term::Eterm;

use crate::access::{make_ref_thing, ref_thing_id};
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::registry::BifRegistry;

use super::{
    copy_out_list, element_pos, resolve, restore_table, take_table_gated, END_OF_TABLE,
};

/// The base of the reverse-continuation id namespace.
///
/// Disjoint from the table-handle ids ([`super::TableId`] counts up from `1`), the
/// forward continuation ids ([`super::CONTINUATION_ID_BASE`] = `1 << 40`) and the
/// compiled-spec ids ([`super::SPEC_ID_BASE`] = `1 << 48`), so a reverse
/// continuation ref never compares `=:=` to a table handle, a forward continuation
/// or a compiled spec — and, because a reverse entry is looked up only in this
/// module's registry, an id from any other namespace can never resume here.
const REVERSE_CONTINUATION_ID_BASE: u64 = 1 << 41;

/// A live `ets:select_reverse/3` continuation (see the module docs).
#[derive(Clone, Copy)]
struct ReverseContinuation {
    /// The registry handle of the table the traversal runs against.
    table: u64,
    /// The **raw** match-spec clause list the traversal recompiles on every resume
    /// (the same M1 substitution [`super::specs`] documents: the term comes from
    /// the originating caller's heap).
    raw_spec: Eterm,
    /// The index into the reverse-ordered result list the next resume continues at.
    position: usize,
    /// The limit the originating `/3` call validated (reused by `/1`; never `0`).
    limit: usize,
}

/// The minted continuations, keyed by the ref id that names them.
fn continuations() -> &'static Mutex<HashMap<u64, ReverseContinuation>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, ReverseContinuation>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The next reverse-continuation id to hand out.
fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(REVERSE_CONTINUATION_ID_BASE);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Locks the registry, recovering a poisoned mutex rather than panicking.
fn lock_continuations() -> MutexGuard<'static, HashMap<u64, ReverseContinuation>> {
    continuations()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Registers the reverse select family (the keys `src/runtime/other.rs` leaves as
/// typed `RuntimeHooks` gaps until a host adopts them).
pub(super) fn register(reg: &mut BifRegistry) {
    reg.register("ets", "select_reverse", 1, select_reverse_1);
    reg.register("ets", "select_reverse", 2, select_reverse_2);
    reg.register("ets", "select_reverse", 3, select_reverse_3);
}

/// The full reverse-ordered result list of `raw_spec` against the current table
/// under `handle`, read-gated and copied out of table storage by the caller.
///
/// The table is taken and restored around the scan; the reverse flag is applied
/// only for an `ordered_set` (see the module docs).
fn ordered_results(
    ctx: &mut BifContext<'_>,
    handle: u64,
    raw_spec: Eterm,
) -> Result<Vec<Eterm>> {
    let spec = compile(raw_spec, ctx.atoms())?;
    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    let ordered = table.kind().is_ordered();
    // `Table::select` needs the atom table *and* the caller heap at once; the same
    // clone pattern `super::select_2` uses (identical indices, read-only).
    let atoms = ctx.atoms().clone();
    let outcome = table.select(&spec, &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    let mut results = outcome?;
    if ordered {
        results.reverse();
    }
    Ok(results)
}

/// Renders one chunk of the reverse traversal, minting a fresh continuation when
/// the chunk was full (see the module docs for the three shapes).
fn emit_chunk(
    ctx: &mut BifContext<'_>,
    continuation: ReverseContinuation,
    mut all: Vec<Eterm>,
) -> Result<Eterm> {
    let ReverseContinuation {
        table,
        raw_spec,
        position,
        limit,
    } = continuation;
    let remaining = all.len().saturating_sub(position);
    let take = limit.min(remaining);
    let chunk: Vec<Eterm> = all.drain(position..position + take).collect();
    if take == limit {
        // A full chunk yields a continuation, even when it landed on the last
        // result (the oracle does; the next resume then renders the bare atom).
        let handle = {
            let mut entries = lock_continuations();
            let id = next_id();
            entries.insert(
                id,
                ReverseContinuation {
                    table,
                    raw_spec,
                    position: position + take,
                    limit,
                },
            );
            id
        };
        let cont = make_ref_thing(ctx.heap(), handle)?;
        let list = copy_out_list(chunk, ctx)?;
        ctx.tuple(&[list, cont])
    } else if chunk.is_empty() {
        ctx.intern_atom(END_OF_TABLE)
    } else {
        let list = copy_out_list(chunk, ctx)?;
        let end = ctx.intern_atom(END_OF_TABLE)?;
        ctx.tuple(&[list, end])
    }
}

/// `ets:select_reverse(Tab, MatchSpec)` — the body values of every selected
/// object, in descending key order for an `ordered_set` and in `ets:select/2`'s
/// order for a hash kind; the empty list when nothing matches.
///
/// `MatchSpec` is a raw match-spec list compiled by [`beam_ets::compile`]; a spec
/// outside the supported subset is `badarg`, as is an unknown or non-table first
/// argument.
fn select_reverse_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let all = ordered_results(ctx, handle, args[1])?;
    copy_out_list(all, ctx)
}

/// `ets:select_reverse(Tab, MatchSpec, Limit)` — as [`select_reverse_2`], but stop
/// after at most `Limit` results and yield `{Results, Continuation}`.
///
/// `Limit` must be a positive integer (`0`, a negative and a non-integer value are
/// `badarg`, matching `ets:select/3`); an exhausted traversal renders the same way
/// the forward path does.
fn select_reverse_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let raw_spec = args[1];
    let limit = element_pos(args[2])?;
    if limit == 0 {
        return Err(badarg());
    }
    let all = ordered_results(ctx, handle, raw_spec)?;
    emit_chunk(
        ctx,
        ReverseContinuation {
            table: handle,
            raw_spec,
            position: 0,
            limit,
        },
        all,
    )
}

/// `ets:select_reverse(Continuation)` — the `/1` resume.
///
/// Shares [`super::dispatch_continuation`] with `ets:select/1`, so the direction
/// stored in the continuation decides the walk (see the module docs).
fn select_reverse_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    super::dispatch_continuation(ctx, args[0])
}

/// Offers `cont_term` to the reverse registry: `Some(result)` when it names a live
/// reverse continuation, `Ok(None)` when it does not (so the forward path — which
/// owns the oracle's `badarg` for every other term — handles it).
///
/// The resume is **read**-gated as the calling process against the continuation's
/// table, and the check runs before anything is recomputed, so a denied resume
/// gives the oracle's `badarg` and leaves the continuation resumable.
pub(super) fn try_resume(ctx: &mut BifContext<'_>, cont_term: Eterm) -> Result<Option<Eterm>> {
    let Ok(id) = ref_thing_id(cont_term) else {
        return Ok(None);
    };
    let entry = {
        let entries = lock_continuations();
        entries.get(&id).copied()
    };
    let Some(entry) = entry else {
        return Ok(None);
    };

    let owner = Some(resolve::caller_owner(ctx));
    {
        let tables = ctx.tables().ok_or_else(unsupported_bif)?;
        let table = tables.get(entry.table).ok_or_else(badarg)?;
        resolve::Gate::Read.check(table, owner)?;
    }

    let all = ordered_results(ctx, entry.table, entry.raw_spec)?;
    emit_chunk(ctx, entry, all).map(Some)
}

/// The limit test shared by the entry points and the tests.
#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{atom_val, signed_value, AtomTable};

    use crate::access::{is_atom, is_list, is_tuple, ref_thing_id, tuple_element, tuple_size};
    use crate::print::proper_list;

    use super::super::{insert_2, new_2, select_3, TableRegistry};

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
            END_OF_TABLE,
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
        (results, if is_atom(second) { None } else { Some(second) })
    }

    fn is_end(atoms: &AtomTable, term: Eterm) -> bool {
        is_atom(term) && atoms.name(atom_val(term)) == Some(END_OF_TABLE)
    }

    /// An `ordered_set` descends and a hash kind keeps `ets:select`'s order.
    #[test]
    fn ordered_set_descends_and_hash_kinds_match_select() {
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
                assert!(
                    beam_term::compare::eq(forward, reverse, ctx.atoms()),
                    "{kind}: the reverse flag is ignored"
                );
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
        assert!(is_list(empty));
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

        // The continuation is immutable: resuming it again reproduces chunk 2.
        let (again, _) = chunk(select_reverse_1(&mut ctx, &[cont1]).expect("c2 again"));
        assert_eq!(again, vec![2, 1]);

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

        // A forged reverse-namespace reference names no continuation.
        let forged =
            make_ref_thing(ctx.heap(), REVERSE_CONTINUATION_ID_BASE + 4096).expect("forged");
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

    /// The registry is the module's own; `ref_thing_id` is what names an entry.
    #[test]
    fn continuations_come_from_the_reverse_id_namespace() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab", "ordered_set");
        insert_all(&mut ctx, handle, &[1, 2]);
        let spec = key_spec(&mut ctx);
        let two = ctx.int(2).expect("fits");
        let cont = chunk(select_reverse_3(&mut ctx, &[handle, spec, two]).unwrap())
            .1
            .expect("continuation");
        assert!(ref_thing_id(cont).expect("a ref thing") >= REVERSE_CONTINUATION_ID_BASE);
    }
}
