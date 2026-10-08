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

//! The key/navigation `*_lookup` family: `ets:first_lookup/1`,
//! `ets:last_lookup/1`, `ets:next_lookup/2` and `ets:prev_lookup/2`.
//!
//! Each is one navigation step (`first`/`last`/`next`/`prev`) **paired with the
//! objects of the key it landed on**: the answer is the 2-tuple `{Key, [Objects]}`
//! — `Key` from the navigation, `Objects` from `Table::lookup(Key)` — or the bare
//! atom `'$end_of_table'` when the walk is exhausted. The family is read-gated
//! like its `first`/`next` siblings, and an unknown, deleted or non-table argument
//! is `badarg`.
//!
//! # The hash-kind aliases (oracle-verified, erts-17.1)
//!
//! A hash interior (`set`/`bag`/`duplicate_bag`) has no backward walk, and the C
//! `ets:last_lookup`/`ets:prev_lookup` do not pretend otherwise — they run the
//! *forward* primitives:
//!
//! * `last_lookup(Tab)` ≡ `first_lookup(Tab)` for a hash kind (with the keys
//!   `{1,10}`,`{2,20}`,`{3,30}` in a `set`, `ets:last_lookup/1` answers
//!   `{1,[{1,10}]}`);
//! * `prev_lookup(Tab, Key)` ≡ `next_lookup(Tab, Key)` for a hash kind (so
//!   `ets:prev_lookup(T, 1)` answers `{2,[{2,20}]}` and `ets:prev_lookup(T, 3)`
//!   answers `'$end_of_table'` on the same table, and an absent anchor is `badarg`
//!   exactly as for `next`).
//!
//! An `ordered_set` walks its real order: the same three keys give `first_lookup`
//! `{1,[{1,10}]}`, `last_lookup` `{3,[{3,30}]}`, `prev_lookup(T, 3)`
//! `{2,[{2,20}]}` and `prev_lookup(T, 99)` `{3,[{3,30}]}` (the `next`/`prev`
//! range-bound rule: an absent key is answered by the nearest stored key, not
//! rejected).
//!
//! The alias is applied here, in the BIF layer, and never by changing
//! `beam_ets::Table::last`/`prev` — those keep the interior's own semantics, which
//! the `ets:last/1`/`ets:prev/2` BIFs build on.
//!
//! # The multi-object answer
//!
//! `Objects` is exactly `ets:lookup/2`'s list, so a `bag`/`duplicate_bag` key
//! holding several objects reports them all (`{1,[{1,10},{1,20}]}`), and a
//! `duplicate_bag` keeps its duplicates.

use beam_common::Result;
use beam_ets::Table;
use beam_term::Eterm;

use crate::context::BifContext;
use crate::registry::BifRegistry;

use super::{copy_out, copy_out_list, restore_table, resolve, take_table_gated, END_OF_TABLE};

/// Registers the `*_lookup` BIFs (the keys `src/runtime/other.rs` leaves as typed
/// `RuntimeHooks` gaps until a host adopts them).
pub(super) fn register(reg: &mut BifRegistry) {
    reg.register("ets", "first_lookup", 1, first_lookup_1);
    reg.register("ets", "last_lookup", 1, last_lookup_1);
    reg.register("ets", "next_lookup", 2, next_lookup_2);
    reg.register("ets", "prev_lookup", 2, prev_lookup_2);
}

/// The `{Key, [Objects]}` result of a navigation step that produced `key` — or the
/// `'$end_of_table'` atom when the step was exhausted.
///
/// `key` compares the way the navigation primitives do (`=:=` through the
/// interior's own key identity), and both halves may alias table storage, so both
/// are copied onto the caller's heap ([`copy_out`]/[`copy_out_list`]) before the
/// table goes back into the registry.
fn key_objects(
    ctx: &mut BifContext<'_>,
    handle: u64,
    table: Table,
    key: Option<Eterm>,
) -> Result<Eterm> {
    let Some(key) = key else {
        restore_table(ctx, handle, table)?;
        return ctx.intern_atom(END_OF_TABLE);
    };
    let objects = table.lookup(key, ctx.atoms());
    restore_table(ctx, handle, table)?;
    let key_term = copy_out(key, ctx)?;
    let list = copy_out_list(objects, ctx)?;
    ctx.tuple(&[key_term, list])
}

/// `ets:first_lookup(Tab)` — the first key and its objects, or `'$end_of_table'`.
fn first_lookup_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    let key = table.first(ctx.atoms());
    key_objects(ctx, handle, table, key)
}

/// `ets:last_lookup(Tab)` — the last key and its objects, or `'$end_of_table'`.
///
/// A **hash** kind has no backward walk, and the oracle aliases this to
/// `first_lookup` there (see the module docs); only an `ordered_set` takes
/// [`Table::last`].
fn last_lookup_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    let key = if table.kind().is_ordered() {
        table.last(ctx.atoms())
    } else {
        table.first(ctx.atoms())
    };
    key_objects(ctx, handle, table, key)
}

/// `ets:next_lookup(Tab, Key)` — the key following `Key` and its objects, or
/// `'$end_of_table'`.
///
/// [`Table::next`]'s `badarg` propagates unchanged: a hash kind requires the
/// anchor key to be present (as does the `'$end_of_table'` anchor itself), while
/// an `ordered_set` reads it as a range bound.
fn next_lookup_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let key = args[1];
    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    let outcome = table.next(key, ctx.atoms());
    match outcome {
        Ok(next) => key_objects(ctx, handle, table, next),
        // The table must not be lost on the error path.
        Err(err) => {
            restore_table(ctx, handle, table)?;
            Err(err)
        }
    }
}

/// `ets:prev_lookup(Tab, Key)` — the key preceding `Key` and its objects, or
/// `'$end_of_table'`.
///
/// Mirroring [`next_lookup_2`]'s alias: a **hash** kind answers with the *forward*
/// step ([`Table::next`] — the oracle's behaviour), only an `ordered_set` with
/// [`Table::prev`]. The chosen primitive's `badarg` propagates as the oracle's.
fn prev_lookup_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let key = args[1];
    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    let outcome = if table.kind().is_ordered() {
        table.prev(key, ctx.atoms())
    } else {
        table.next(key, ctx.atoms())
    };
    match outcome {
        Ok(prev) => key_objects(ctx, handle, table, prev),
        Err(err) => {
            restore_table(ctx, handle, table)?;
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{atom_val, signed_value, AtomTable};

    use crate::access::{is_atom, is_list, is_tuple, tuple_element, tuple_size};
    use crate::print::proper_list;

    use super::super::{delete_1, insert_2, new_2, TableRegistry};

    /// The atom names the `ets` BIFs and the assertions need.
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
            "id",
            "name",
            "type",
            "protection",
            "size",
            "memory",
            "owner",
            "compressed",
            "read_concurrency",
            "write_concurrency",
            "decentralized_counters",
            "heir",
            "none",
            "node",
            "nonode@nohost",
            "mytab",
            "dupb",
            "named_one",
            "undefined",
        ]);
        atoms
    }

    /// The per-test `(heap, atoms, registry)` triple a `BifContext` is built over.
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

    /// The `{Key, Value}` object tuple in `ctx`.
    fn object(ctx: &mut BifContext<'_>, key: i64, value: i64) -> Eterm {
        let k = ctx.int(key).expect("fits");
        let v = ctx.int(value).expect("fits");
        ctx.tuple(&[k, v]).expect("fits")
    }

    /// Inserts the `{K, K * 10}` objects for `keys`, in order.
    fn insert_all(ctx: &mut BifContext<'_>, handle: Eterm, keys: &[i64]) {
        for &key in keys {
            let obj = object(ctx, key, key * 10);
            let inserted = insert_2(ctx, &[handle, obj]).expect("insert");
            assert_eq!(ctx.atoms().name(atom_val(inserted)), Some("true"));
        }
    }

    /// Whether `term` is the atom `'$end_of_table'`.
    fn is_end(atoms: &AtomTable, term: Eterm) -> bool {
        is_atom(term) && atoms.name(atom_val(term)) == Some(END_OF_TABLE)
    }

    /// The `{Key, [Objects]}` result rendered as `(key, [(k, v), …])` — the shape
    /// the live oracle prints.
    fn rows(term: Eterm) -> (i64, Vec<(i64, i64)>) {
        assert!(is_tuple(term), "a found result is a 2-tuple");
        assert_eq!(tuple_size(term), 2);
        let key = tuple_element(term, 0);
        let objects = tuple_element(term, 1);
        assert!(is_list(objects), "the second element is the object list");
        let rows = proper_list(objects)
            .expect("a proper object list")
            .into_iter()
            .map(|obj| {
                assert!(is_tuple(obj) && tuple_size(obj) == 2);
                (
                    signed_value(tuple_element(obj, 0)),
                    signed_value(tuple_element(obj, 1)),
                )
            })
            .collect();
        (signed_value(key), rows)
    }

    /// An `ordered_set` of `{K, K*10}`: every direction of the family, including
    /// the range-bound reading of an absent `next`/`prev` anchor — the
    /// oracle-verified (`erts-17.1`) matrix.
    #[test]
    fn ordered_set_walks_both_directions() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab", "ordered_set");
        let one = ctx.int(1).expect("fits");

        // An empty table is exhausted in every direction (the `'$end_of_table'`
        // atom for the `/1` forms, the same for the `/2` forms on a bound that has
        // no neighbour).
        for term in [
            first_lookup_1(&mut ctx, &[handle]).expect("first_lookup"),
            last_lookup_1(&mut ctx, &[handle]).expect("last_lookup"),
            next_lookup_2(&mut ctx, &[handle, one]).expect("next_lookup"),
            prev_lookup_2(&mut ctx, &[handle, one]).expect("prev_lookup"),
        ] {
            assert!(is_end(&atoms, term), "an empty table is exhausted");
        }

        insert_all(&mut ctx, handle, &[1, 2, 3]);
        let three = ctx.int(3).expect("fits");
        let past = ctx.int(99).expect("fits");

        assert_eq!(
            rows(first_lookup_1(&mut ctx, &[handle]).expect("first_lookup")),
            (1, vec![(1, 10)])
        );
        assert_eq!(
            rows(last_lookup_1(&mut ctx, &[handle]).expect("last_lookup")),
            (3, vec![(3, 30)])
        );
        assert_eq!(
            rows(next_lookup_2(&mut ctx, &[handle, one]).expect("next_lookup(1)")),
            (2, vec![(2, 20)])
        );
        assert_eq!(
            rows(prev_lookup_2(&mut ctx, &[handle, three]).expect("prev_lookup(3)")),
            (2, vec![(2, 20)])
        );

        // An absent key is a range bound for an `ordered_set`.
        assert!(is_end(&atoms,
            next_lookup_2(&mut ctx, &[handle, past]).expect("next_lookup(99)")
        ));
        assert_eq!(
            rows(prev_lookup_2(&mut ctx, &[handle, past]).expect("prev_lookup(99)")),
            (3, vec![(3, 30)])
        );

        // … and the two ends of the order.
        assert!(is_end(&atoms,
            next_lookup_2(&mut ctx, &[handle, three]).expect("next_lookup(3)")
        ));
        assert!(is_end(&atoms,
            prev_lookup_2(&mut ctx, &[handle, one]).expect("prev_lookup(1)")
        ));
    }

    /// The hash kinds alias `last_lookup` to `first` and `prev_lookup` to `next`
    /// (the oracle's behaviour: a hash interior has no backward walk). The hash
    /// *order* is unspecified, so the aliases are asserted for every stored key
    /// rather than against a fixed key order.
    #[test]
    fn hash_kinds_alias_last_to_first_and_prev_to_next() {
        for kind in ["set", "bag", "duplicate_bag"] {
            let (mut heap, atoms, mut reg) = parts();
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
            let handle = new_table(&mut ctx, "mytab", kind);
            insert_all(&mut ctx, handle, &[1, 2, 3]);

            let first = first_lookup_1(&mut ctx, &[handle]).expect("first_lookup");
            let last = last_lookup_1(&mut ctx, &[handle]).expect("last_lookup");
            assert!(
                beam_term::compare::eq(first, last, ctx.atoms()),
                "{kind}: last_lookup must alias first_lookup"
            );

            for key in [1, 2, 3] {
                let key_term = ctx.int(key).expect("fits");
                let next = next_lookup_2(&mut ctx, &[handle, key_term]).expect("next_lookup");
                let prev = prev_lookup_2(&mut ctx, &[handle, key_term]).expect("prev_lookup");
                assert!(
                    beam_term::compare::eq(next, prev, ctx.atoms()),
                    "{kind}: prev_lookup({key}) must alias next_lookup({key})"
                );
            }

            // An absent anchor and the `'$end_of_table'` anchor are `badarg` for
            // both directions — and the table survives each failure.
            let missing = ctx.int(99).expect("fits");
            let end = ctx.atom(END_OF_TABLE).expect("seeded");
            for key in [missing, end] {
                assert!(matches!(
                    next_lookup_2(&mut ctx, &[handle, key]),
                    Err(BeamError::BadArg)
                ));
                assert!(matches!(
                    prev_lookup_2(&mut ctx, &[handle, key]),
                    Err(BeamError::BadArg)
                ));
            }
            assert_eq!(
                rows(first_lookup_1(&mut ctx, &[handle]).expect("still live")),
                (first_key(first), first_objects(first))
            );
        }
    }

    /// The key of a `{Key, [Objects]}` result, without re-asserting the shape.
    fn first_key(term: Eterm) -> i64 {
        signed_value(tuple_element(term, 0))
    }

    /// The objects of a `{Key, [Objects]}` result.
    fn first_objects(term: Eterm) -> Vec<(i64, i64)> {
        let objects = tuple_element(term, 1);
        proper_list(objects)
            .expect("a proper object list")
            .into_iter()
            .map(|obj| {
                (
                    signed_value(tuple_element(obj, 0)),
                    signed_value(tuple_element(obj, 1)),
                )
            })
            .collect()
    }

    /// Every object of a key is reported, in `ets:lookup/2` order — the
    /// `bag`/`duplicate_bag` answer (`ets:first_lookup(Bag)` ⇒
    /// `{1,[{1,10},{1,20}]}`).
    #[test]
    fn multi_kinds_report_every_object_of_the_key() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        // A `bag` drops the duplicate `{1,10}`; a `duplicate_bag` keeps it.
        let bag = new_table(&mut ctx, "mytab", "bag");
        let dup = new_table(&mut ctx, "dupb", "duplicate_bag");
        for handle in [bag, dup] {
            for (key, value) in [(1, 10), (1, 20), (1, 10)] {
                let obj = object(&mut ctx, key, value);
                insert_2(&mut ctx, &[handle, obj]).expect("insert");
            }
        }

        let first = first_lookup_1(&mut ctx, &[bag]).expect("first_lookup(bag)");
        assert_eq!(rows(first), (1, vec![(1, 10), (1, 20)]));
        let first = first_lookup_1(&mut ctx, &[dup]).expect("first_lookup(dup_bag)");
        assert_eq!(rows(first), (1, vec![(1, 10), (1, 20), (1, 10)]));
    }

    /// A bad table argument (a non-table term, an unknown name, a deleted table)
    /// is `badarg` for every arity, and a `badarg` never destroys a live table.
    #[test]
    fn bad_table_arguments_are_badarg() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab", "set");
        insert_all(&mut ctx, handle, &[1]);
        let live = first_lookup_1(&mut ctx, &[handle]).expect("first_lookup");

        let not_a_table = ctx.int(42).expect("fits");
        let unknown_name = ctx.atom("undefined").expect("seeded");
        let key = ctx.int(1).expect("fits");
        for arg in [not_a_table, unknown_name] {
            assert!(matches!(
                first_lookup_1(&mut ctx, &[arg]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                last_lookup_1(&mut ctx, &[arg]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                next_lookup_2(&mut ctx, &[arg, key]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                prev_lookup_2(&mut ctx, &[arg, key]),
                Err(BeamError::BadArg)
            ));
        }

        // A deleted table is `badarg`, and the failed calls above left the table
        // in the registry.
        let after = first_lookup_1(&mut ctx, &[handle]).expect("still live");
        assert!(beam_term::compare::eq(live, after, ctx.atoms()));
        assert_eq!(
            ctx.atoms()
                .name(atom_val(delete_1(&mut ctx, &[handle]).expect("delete"))),
            Some("true")
        );
        assert!(matches!(
            first_lookup_1(&mut ctx, &[handle]),
            Err(BeamError::BadArg)
        ));
    }

    /// A `named_table` resolves through its **name atom**, exactly as the existing
    /// navigation BIFs do.
    #[test]
    fn a_named_table_resolves_by_name() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let name = ctx.atom("named_one").expect("seeded");
        let kind = ctx.atom("ordered_set").expect("seeded");
        let named = ctx.atom("named_table").expect("seeded");
        let opts = ctx.list(&[kind, named]).expect("fits");
        let handle = new_2(&mut ctx, &[name, opts]).expect("ets:new");
        assert!(is_atom(handle), "a named_table's handle is its name atom");
        insert_all(&mut ctx, handle, &[7]);

        let by_name = first_lookup_1(&mut ctx, &[name]).expect("first_lookup by name");
        assert_eq!(rows(by_name), (7, vec![(7, 70)]));
        let last = last_lookup_1(&mut ctx, &[name]).expect("last_lookup by name");
        assert_eq!(rows(last), (7, vec![(7, 70)]));
    }

    /// The read gate: a non-owner may not walk a `private` table (the walk is
    /// `badarg` and the table stays put).
    #[test]
    fn a_non_owner_cannot_walk_a_private_table() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let name = ctx.atom("mytab").expect("seeded");
        let kind = ctx.atom("ordered_set").expect("seeded");
        let private = ctx.atom("private").expect("seeded");
        let opts = ctx.list(&[kind, private]).expect("fits");
        let handle = new_2(&mut ctx, &[name, opts]).expect("ets:new");
        insert_all(&mut ctx, handle, &[1]);

        // The owner reads it.
        assert_eq!(
            rows(first_lookup_1(&mut ctx, &[handle]).expect("owner")),
            (1, vec![(1, 10)])
        );

        // A different process is denied, and the table remains.
        {
            let mut other =
                BifContext::new(&mut heap, &atoms, ProcessId::new(2, 0)).with_tables(&mut reg);
            assert!(matches!(
                first_lookup_1(&mut other, &[handle]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                next_lookup_2(&mut other, &[handle, ctx.int(1).unwrap()]),
                Err(BeamError::BadArg)
            ));
        }
        assert_eq!(
            rows(first_lookup_1(&mut ctx, &[handle]).expect("owner again")),
            (1, vec![(1, 10)])
        );
    }

    /// The live-oracle cross-check. The golden block below is what the system
    /// `erl` printed when these goldens were measured (OTP 29.1.1 / erts-17.1); the
    /// test re-runs the oracle, so a future OTP change cannot silently invalidate
    /// them, and asserts the crate's own BIFs against the same goldens. It skips
    /// (loudly) when no `erl` is on `PATH`.
    #[test]
    fn live_oracle_matches_the_pinned_goldens() {
        let script = concat!(
            "T = ets:new(t, [ordered_set]), ",
            "[ets:insert(T, O) || O <- [{1,10},{2,20},{3,30}]], ",
            "io:format(\"~p~n~p~n~p~n~p~n~p~n~p~n~p~n\", ",
            "[ets:first_lookup(T), ets:last_lookup(T), ",
            "ets:next_lookup(T,1), ets:prev_lookup(T,3), ",
            "ets:next_lookup(T,99), ets:prev_lookup(T,99), ets:prev_lookup(T,1)])"
        );
        let golden = concat!(
            "{1,[{1,10}]}\n",
            "{3,[{3,30}]}\n",
            "{2,[{2,20}]}\n",
            "{2,[{2,20}]}\n",
            "'$end_of_table'\n",
            "{3,[{3,30}]}\n",
            "'$end_of_table'"
        );
        if let Some(actual) = super::super::oracle(script) {
            assert_eq!(actual, golden, "the system `erl` no longer matches the goldens");
        }

        // The same goldens, asserted against this crate's own BIFs.
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab", "ordered_set");
        insert_all(&mut ctx, handle, &[1, 2, 3]);
        let one = ctx.int(1).expect("fits");
        let three = ctx.int(3).expect("fits");
        let past = ctx.int(99).expect("fits");
        assert_eq!(
            rows(first_lookup_1(&mut ctx, &[handle]).unwrap()),
            (1, vec![(1, 10)])
        );
        assert_eq!(
            rows(last_lookup_1(&mut ctx, &[handle]).unwrap()),
            (3, vec![(3, 30)])
        );
        assert_eq!(
            rows(next_lookup_2(&mut ctx, &[handle, one]).unwrap()),
            (2, vec![(2, 20)])
        );
        assert_eq!(
            rows(prev_lookup_2(&mut ctx, &[handle, three]).unwrap()),
            (2, vec![(2, 20)])
        );
        assert!(is_end(&atoms,
            next_lookup_2(&mut ctx, &[handle, past]).unwrap()
        ));
        assert_eq!(
            rows(prev_lookup_2(&mut ctx, &[handle, past]).unwrap()),
            (3, vec![(3, 30)])
        );
        assert!(is_end(&atoms,
            prev_lookup_2(&mut ctx, &[handle, one]).unwrap()
        ));
    }
}
