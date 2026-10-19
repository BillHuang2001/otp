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

//! The `ets` **table-alias witness** seam (`BifContext::with_table_aliasing`).
//!
//! The `ets` copy-out helpers (`copy_out`/`copy_out_list` in `src/ets.rs`) move a
//! table-aliasing term onto the caller's pooled `BifContext` heap. A host that
//! copies the result out itself (onto the process heap) can install a
//! [`Cell<bool>`] witness instead: the helpers then hand back the table's own
//! terms — setting the cell — so the host's copy is the only copy.
//!
//! Every case here is driven through the public [`BifRegistry::call`] over a real
//! [`TableRegistry`] and asserts, for both modes:
//!
//! * **no witness ⇒ unchanged**: the same fresh copy onto the pool heap (the same
//!   pool-word figure, the same term value, and a *different* address than the
//!   table's own term), so the seam is inert;
//! * **witness ⇒ aliasing**: the cell is set, the returned term is the very word
//!   the table holds (pointer identity, proven without any crate-private helper),
//!   and the pool only gained the list **spine** (2 words per object).
//!
//! Terms are compared structurally with [`beam_term::compare::eq`]; list/tuple
//! destructuring goes through the registered `erlang` BIFs (`hd/1`, `tl/1`,
//! `element/2`), so this suite uses only the public API and never `erl`.

use std::cell::Cell;

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, TableRegistry};
use beam_common::Result;
use beam_process::ProcessId;
use beam_term::{compare, AtomTable, Eterm, NIL};

/// The atom names the `ets` BIFs and the destructuring helpers need.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "set",
        "ordered_set",
        "undefined",
        "$end_of_table",
        "my_tab",
        "k",
        "nested",
        "a",
        "b",
        "c",
        "absent",
    ]);
    atoms
}

/// The pool words one BIF call wrote: the bump offset and the high-water mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cost {
    used: usize,
    peak: usize,
}

/// One table plus the `(heap, atoms, registry)` a [`BifContext`] is built over, so a
/// test re-creates the context per call exactly as the host does.
struct Fixture {
    bifs: BifRegistry,
    heap: Heap,
    atoms: AtomTable,
    tables: TableRegistry,
    handle: Eterm,
}

impl Fixture {
    /// A fixture whose one table (named `my_tab`, unnamed handle) is of `kind`.
    fn new(kind: &str) -> Fixture {
        let mut fixture = Fixture {
            bifs: default_registry(),
            heap: Heap::new(1 << 16).expect("test heap"),
            atoms: seeded_atoms(),
            tables: TableRegistry::new(),
            handle: NIL,
        };
        let handle = fixture.with_ctx(None, |bifs, ctx| {
            let name = ctx.atom("my_tab").expect("seeded");
            let kind = ctx.atom(kind).expect("seeded");
            let options = ctx.list(&[kind]).expect("fits");
            bifs.call("ets", "new", 2, ctx, &[name, options])
                .expect("ets:new")
        });
        fixture.handle = handle;
        fixture
    }

    /// Runs `f` with a fresh context, optionally carrying the table-alias witness.
    fn with_ctx<T>(
        &mut self,
        witness: Option<&Cell<bool>>,
        f: impl FnOnce(&BifRegistry, &mut BifContext<'_>) -> T,
    ) -> T {
        let bifs = &self.bifs;
        let ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0))
            .with_tables(&mut self.tables);
        let mut ctx = match witness {
            Some(witness) => ctx.with_table_aliasing(witness),
            None => ctx,
        };
        f(bifs, &mut ctx)
    }

    /// One BIF call, returning the result and the pool words it cost.
    fn run(
        &mut self,
        witness: Option<&Cell<bool>>,
        (module, function, arity): (&str, &str, u32),
        args: &[Eterm],
    ) -> (Result<Eterm>, Cost) {
        let used = self.heap.used();
        let peak = self.heap.peak_used();
        let result =
            self.with_ctx(witness, |bifs, ctx| bifs.call(module, function, arity, ctx, args));
        (
            result,
            Cost {
                used: self.heap.used() - used,
                peak: self.heap.peak_used() - peak,
            },
        )
    }

    /// A seeded atom's term (an immediate, so it costs no heap).
    fn atom(&mut self, name: &str) -> Eterm {
        self.with_ctx(None, |_, ctx| ctx.atom(name).expect("seeded"))
    }

    /// A small integer term.
    fn small(&mut self, value: i64) -> Eterm {
        self.with_ctx(None, |_, ctx| ctx.int(value).expect("fits"))
    }

    /// Inserts `object` through `ets:insert/2`.
    fn insert(&mut self, object: Eterm) {
        let handle = self.handle;
        let (outcome, _) = self.run(None, ("ets", "insert", 2), &[handle, object]);
        outcome.expect("ets:insert");
    }

    /// The object `{Key, Value::small int}` on the pool heap.
    fn pair(&mut self, key: &str, value: i64) -> Eterm {
        self.with_ctx(None, |_, ctx| {
            let k = ctx.atom(key).expect("seeded");
            let v = ctx.int(value).expect("fits");
            ctx.tuple(&[k, v]).expect("fits")
        })
    }

    /// The object `{Key, {nested, 1}}` on the pool heap — a *boxed* element 2.
    fn nested(&mut self, key: &str) -> Eterm {
        self.with_ctx(None, |_, ctx| {
            let k = ctx.atom(key).expect("seeded");
            let tag = ctx.atom("nested").expect("seeded");
            let one = ctx.int(1).expect("fits");
            let inner = ctx.tuple(&[tag, one]).expect("fits");
            ctx.tuple(&[k, inner]).expect("fits")
        })
    }

    fn lookup(&mut self, witness: Option<&Cell<bool>>, key: Eterm) -> (Result<Eterm>, Cost) {
        let handle = self.handle;
        self.run(witness, ("ets", "lookup", 2), &[handle, key])
    }

    fn lookup_element(
        &mut self,
        witness: Option<&Cell<bool>>,
        key: Eterm,
        pos: i64,
    ) -> (Result<Eterm>, Cost) {
        let handle = self.handle;
        let pos = self.small(pos);
        self.run(witness, ("ets", "lookup_element", 3), &[handle, key, pos])
    }

    fn tab2list(&mut self, witness: Option<&Cell<bool>>) -> (Result<Eterm>, Cost) {
        let handle = self.handle;
        self.run(witness, ("ets", "tab2list", 1), &[handle])
    }

    /// `erlang:hd/1`, `erlang:tl/1`, `erlang:element/2` — raw reads: they neither
    /// copy nor write a pool word.
    fn hd(&mut self, list: Eterm) -> Eterm {
        let (outcome, _) = self.run(None, ("erlang", "hd", 1), &[list]);
        outcome.expect("hd of a non-empty list")
    }

    fn tl(&mut self, list: Eterm) -> Eterm {
        let (outcome, _) = self.run(None, ("erlang", "tl", 1), &[list]);
        outcome.expect("tl of a non-empty list")
    }

    fn element(&mut self, pos: i64, tuple: Eterm) -> Eterm {
        let pos = self.small(pos);
        let (outcome, _) = self.run(None, ("erlang", "element", 2), &[pos, tuple]);
        outcome.expect("element of a tuple")
    }

    fn eq(&self, a: Eterm, b: Eterm) -> bool {
        compare::eq(a, b, &self.atoms)
    }
}

/// A 2-tuple `{k, 42}` is `[header, k, 42]`, so a copy costs 3 pool words; a list
/// spine over `n` objects costs 2 words per object.
const PAIR_WORDS: usize = 3;
const SPINE_WORDS: usize = 2;

#[test]
fn lookup_without_a_witness_copies_the_object_onto_the_pool_heap() {
    let mut fixture = Fixture::new("set");
    let key = fixture.atom("k");
    let value = fixture.small(42);
    let object = fixture.pair("k", 42);
    fixture.insert(object);

    let (outcome, cost) = fixture.lookup(None, key);
    let result = outcome.expect("ets:lookup");
    assert_eq!(
        cost,
        Cost {
            used: PAIR_WORDS + SPINE_WORDS,
            peak: PAIR_WORDS + SPINE_WORDS,
        },
        "one object copy plus a one-cons spine"
    );

    let object = fixture.hd(result);
    assert_eq!(fixture.element(1, object), key);
    assert_eq!(fixture.element(2, object), value);
    assert_eq!(fixture.tl(result), NIL, "one object, one cons cell");

    // Every call copies: the second lookup's object is a different address.
    let (again, cost) = fixture.lookup(None, key);
    assert_eq!(cost.used, PAIR_WORDS + SPINE_WORDS);
    assert_ne!(
        fixture.hd(again.expect("ets:lookup")),
        object,
        "each call copies — no returned term aliases table storage"
    );
}

#[test]
fn lookup_with_a_witness_aliases_table_storage_and_writes_only_the_spine() {
    let mut fixture = Fixture::new("set");
    let key = fixture.atom("k");
    let object = fixture.pair("k", 42);
    fixture.insert(object);

    let witness = Cell::new(false);
    let (outcome, cost) = fixture.lookup(Some(&witness), key);
    let aliased = outcome.expect("ets:lookup");
    assert_eq!(
        cost,
        Cost {
            used: SPINE_WORDS,
            peak: SPINE_WORDS,
        },
        "the spine only — the object is not copied"
    );
    assert!(witness.get(), "the witness is set");

    assert_eq!(fixture.tl(aliased), NIL);
    let aliased_object = fixture.hd(aliased);

    // A copying lookup returns a *fresh* tuple with the same value …
    let (copied, cost) = fixture.lookup(None, key);
    assert_eq!(cost.used, PAIR_WORDS + SPINE_WORDS);
    let copied_object = fixture.hd(copied.expect("ets:lookup"));
    assert_ne!(aliased_object, copied_object, "the copy has its own address");
    assert!(fixture.eq(aliased_object, copied_object));

    // … while two aliasing lookups agree on the address, i.e. the returned term is
    // the table's own word, not a per-call copy.
    let (again, _) = fixture.lookup(Some(&witness), key);
    assert_eq!(fixture.hd(again.expect("ets:lookup")), aliased_object);
}

#[test]
fn a_missed_lookup_is_identical_in_both_modes() {
    let mut fixture = Fixture::new("set");
    let key = fixture.atom("k");
    let absent = fixture.atom("absent");
    let object = fixture.pair("k", 42);
    fixture.insert(object);

    let (missed, cost) = fixture.lookup(None, absent);
    assert_eq!(missed.expect("ets:lookup"), NIL);
    assert_eq!(cost, Cost { used: 0, peak: 0 }, "NIL is an immediate");

    let witness = Cell::new(false);
    let (missed, cost) = fixture.lookup(Some(&witness), absent);
    assert_eq!(missed.expect("ets:lookup"), NIL);
    assert_eq!(cost, Cost { used: 0, peak: 0 });
    // The witness is set unconditionally, so the host's copy-out never has to
    // guess (copying `NIL` out is a no-op).
    assert!(witness.get());

    // The live key still resolves in the aliasing mode.
    let (present, _) = fixture.lookup(Some(&witness), key);
    assert_ne!(present.expect("ets:lookup"), NIL);
}

#[test]
fn tab2list_covers_the_list_helper_in_both_modes() {
    const OBJECTS: usize = 3;

    let mut fixture = Fixture::new("set");
    for (key, value) in [("a", 1), ("b", 2), ("c", 3)] {
        let object = fixture.pair(key, value);
        fixture.insert(object);
    }
    let a = fixture.atom("a");
    let b = fixture.atom("b");
    let c = fixture.atom("c");

    // Copying mode: every object is copied, then the spine is built over the copies.
    let (copied, cost) = fixture.tab2list(None);
    let copied = copied.expect("ets:tab2list");
    assert_eq!(cost.used, OBJECTS * (PAIR_WORDS + SPINE_WORDS));

    // Aliasing mode: only the spine.
    let witness = Cell::new(false);
    let (aliased, cost) = fixture.tab2list(Some(&witness));
    let aliased = aliased.expect("ets:tab2list");
    assert_eq!(cost.used, OBJECTS * SPINE_WORDS, "the spine only");
    assert!(witness.get());

    // `ets:lookup` in the same mode hands back the very same words, so a per-object
    // copy-out would show up as a distinct address here.
    let (a_object, _) = fixture.lookup(Some(&witness), a);
    let (b_object, _) = fixture.lookup(Some(&witness), b);
    let (c_object, _) = fixture.lookup(Some(&witness), c);
    let expected = [
        fixture.hd(a_object.expect("ets:lookup")),
        fixture.hd(b_object.expect("ets:lookup")),
        fixture.hd(c_object.expect("ets:lookup")),
    ];

    let mut aliased_heads = Vec::new();
    let mut rest = aliased;
    for _ in 0..OBJECTS {
        aliased_heads.push(fixture.hd(rest));
        rest = fixture.tl(rest);
    }
    assert_eq!(rest, NIL, "exactly {OBJECTS} objects");
    // `tab2list`'s iteration order is the table's, not the insertion order, so the
    // pointer-identity comparison is order-insensitive (a `set` is unordered).
    let mut aliased_sorted = aliased_heads.clone();
    aliased_sorted.sort_by_key(|term| term.raw());
    let mut expected_sorted = expected.to_vec();
    expected_sorted.sort_by_key(|term| term.raw());
    assert_eq!(
        aliased_sorted, expected_sorted,
        "the tab2list heads are pointer-identical to the lookup aliases"
    );

    // Both modes agree on the values, element for element, in the table's order.
    let mut copied_heads = Vec::new();
    let mut rest = copied;
    for _ in 0..OBJECTS {
        copied_heads.push(fixture.hd(rest));
        rest = fixture.tl(rest);
    }
    assert_eq!(rest, NIL);
    for (copied_head, aliased_head) in copied_heads.iter().zip(&aliased_heads) {
        assert_ne!(copied_head, aliased_head, "the copy has its own address");
        assert!(fixture.eq(*copied_head, *aliased_head));
    }
}

#[test]
fn lookup_element_covers_the_single_term_helper_in_both_modes() {
    let mut fixture = Fixture::new("set");
    let key = fixture.atom("k");
    let object = fixture.nested("k");
    fixture.insert(object);

    // Element 2 is the boxed `{nested, 1}` tuple: 3 words to copy, nothing to alias.
    let (copied, cost) = fixture.lookup_element(None, key, 2);
    let copied = copied.expect("ets:lookup_element/3");
    assert_eq!(cost.used, PAIR_WORDS, "the whole element is copied");

    let witness = Cell::new(false);
    let (aliased, cost) = fixture.lookup_element(Some(&witness), key, 2);
    let aliased = aliased.expect("ets:lookup_element/3");
    assert_eq!(cost, Cost { used: 0, peak: 0 }, "nothing is written");
    assert!(witness.get());

    // The aliased element is the word the table's own object holds: read it out of
    // the object `ets:lookup` returns in the same mode (which itself wrote only the
    // spine) and compare the addresses.
    let (object, cost) = fixture.lookup(Some(&witness), key);
    assert_eq!(cost.used, SPINE_WORDS);
    let object = fixture.hd(object.expect("ets:lookup"));
    let from_object = fixture.element(2, object);
    assert_eq!(
        aliased, from_object,
        "the aliased element is the table's own term"
    );
    assert_ne!(copied, from_object, "the copying mode returns a fresh copy");
    assert!(fixture.eq(copied, from_object));

    // The copied object is a separate graph from the aliased one.
    let (copied_object, _) = fixture.lookup(None, key);
    assert_ne!(
        fixture.hd(copied_object.expect("ets:lookup")),
        object,
        "the copying mode's object is not the table's own word"
    );
}
