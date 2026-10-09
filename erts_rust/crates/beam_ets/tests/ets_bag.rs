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

//! `bag` / `duplicate_bag` semantics over the shared hash interior.
//!
//! The focused tests pin one rule at a time; the **multi-kind battery** at the
//! bottom of the file drives all four kinds through the same script (duplicates,
//! `delete_object/2` on a structurally-equal *copy*, an upsert/append under an
//! existing key, `size`/`delete` accounting, `insert_new/2`, `insert_list/2`
//! atomicity, `select_delete/2`/`match_delete/2` counts, a `{keypos,2}` table and
//! the `ordered_set` replace-key-term rule) and checks the crate's facts against
//! the live system `erl` **and** against the oracle figures recorded in
//! [`EXPECTED`], so the rules stay pinned with no oracle on `$PATH`.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use beam_common::BeamError;
use beam_ets::{compile, Table, TableId, TableKind, TableOptions};
use beam_term::prelude::*;
use common::*;
#[test]
fn bag_stores_distinct_objects_per_key() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    let mut t = new_table(TableKind::Bag);

    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 1, 20), &atoms).unwrap();
    // An identical object is a no-op for a bag.
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    assert_eq!(t.size(), 2);
    let looked = t.lookup(make_small(1), &atoms);
    assert_eq!(looked.len(), 2);
    assert!(looked
        .iter()
        .any(|o| eq(*o, object(&mut heap, 1, 10), &atoms)));
    assert!(looked
        .iter()
        .any(|o| eq(*o, object(&mut heap, 1, 20), &atoms)));

    // `delete` removes the whole key (all its objects).
    t.delete(make_small(1), &atoms).unwrap();
    assert_eq!(t.size(), 0);
    assert!(t.is_empty());
}

#[test]
fn bag_duplicate_insert_does_not_leak_a_copy() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Bag);

    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    let one = t.memory();

    // An identical object is a no-op: the interior rejects the deep copy, so the
    // table reclaims it and the memory figure does not grow — no orphaned copy is
    // left behind for `delete_all_objects` to clean up.
    for _ in 0..20 {
        t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    }
    assert_eq!(
        t.memory(),
        one,
        "a rejected `bag` duplicate must not leak a copy"
    );
    assert_eq!(t.size(), 1);

    // A structurally *different* object under the same key IS stored and does grow.
    t.insert(object(&mut heap, 1, 20), &atoms).unwrap();
    assert!(
        t.memory() > one,
        "a distinct object is stored and grows the memory count"
    );
    assert_eq!(t.size(), 2);
}

#[test]
fn insert_new_on_an_existing_key_does_not_leak_a_copy() {
    for kind in [
        TableKind::Set,
        TableKind::OrderedSet,
        TableKind::Bag,
        TableKind::DuplicateBag,
    ] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut t = new_table(kind);

        assert!(t.insert_new(object(&mut heap, 1, 10), &atoms).unwrap());
        let one = t.memory();

        // Every refused `insert_new` deep-copies then discards the object; the copy
        // must be reclaimed, so the memory figure stays put.
        for _ in 0..20 {
            assert!(!t.insert_new(object(&mut heap, 1, 20), &atoms).unwrap());
        }
        assert_eq!(
            t.memory(),
            one,
            "{kind:?}: a refused `insert_new` must not leak a copy"
        );
        assert_eq!(t.size(), 1);
    }
}

/// `ets:lookup_element/3` on a `bag` returns the **list** of the `Pos`-th
/// elements of *all* objects under `Key` (which is why the crate's scalar
/// [`Table::lookup_element`] refuses the multi kinds and this
/// [`Table::lookup_element_all`] exists).
///
/// Verified against the system `erl` (OTP 29 / erts-17.1): a lone object yields a
/// one-element list, duplicates are preserved, an absent key is `badarg`, `Pos == 0`
/// is `badarg`, and one object shorter than `Pos` makes the whole call `badarg`.
#[test]
fn lookup_element_all_returns_the_element_list_for_the_multi_kinds() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(512);

    // `{k, Value, Tag}` — three objects under the same key.
    let mut bag = new_table(TableKind::Bag);
    bag.insert(tuple_of(&mut heap, &[make_small(1), make_small(10), make_small(100)]), &atoms)
        .unwrap();
    bag.insert(tuple_of(&mut heap, &[make_small(1), make_small(20), make_small(200)]), &atoms)
        .unwrap();
    bag.insert(tuple_of(&mut heap, &[make_small(1), make_small(30), make_small(300)]), &atoms)
        .unwrap();

    let values = bag.lookup_element_all(make_small(1), 2, &atoms).unwrap();
    assert_eq!(values.len(), 3, "one element per object under the key");
    for (got, want) in values.iter().zip([10, 20, 30]) {
        assert!(eq(*got, make_small(want), &atoms), "expected {want}, got {got:?}");
    }
    // The key element repeats once per object.
    let keys = bag.lookup_element_all(make_small(1), 1, &atoms).unwrap();
    assert_eq!(keys.len(), 3);
    assert!(keys.iter().all(|k| eq(*k, make_small(1), &atoms)));

    // A key holding a single object still yields a one-element list.
    bag.insert(object(&mut heap, 2, 7), &atoms).unwrap();
    let lone = bag.lookup_element_all(make_small(2), 2, &atoms).unwrap();
    assert_eq!(lone.len(), 1);
    assert!(eq(lone[0], make_small(7), &atoms));

    // `duplicate_bag` keeps identical objects, so identical elements repeat.
    let mut dup = new_table(TableKind::DuplicateBag);
    dup.insert(object(&mut heap, 5, 1), &atoms).unwrap();
    dup.insert(object(&mut heap, 5, 1), &atoms).unwrap();
    dup.insert(object(&mut heap, 5, 2), &atoms).unwrap();
    let elements = dup.lookup_element_all(make_small(5), 2, &atoms).unwrap();
    assert_eq!(elements.len(), 3);
    assert!(eq(elements[0], make_small(1), &atoms));
    assert!(eq(elements[1], make_small(1), &atoms));
    assert!(eq(elements[2], make_small(2), &atoms));

    // The single-object kinds store one object per key, so their list has one
    // element — the same one the scalar form returns.
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let mut t = new_table(kind);
        t.insert(object(&mut heap, 3, 33), &atoms).unwrap();
        let one = t.lookup_element_all(make_small(3), 2, &atoms).unwrap();
        assert_eq!(one.len(), 1, "{kind:?}");
        assert!(eq(one[0], t.lookup_element(make_small(3), 2, &atoms).unwrap(), &atoms));
    }
}

/// The element-list projection fails exactly where the oracle fails, and the `/4`
/// form reports an absent key as `None` (the caller substitutes the default).
#[test]
fn lookup_element_all_reports_the_oracle_failures() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut bag = new_table(TableKind::Bag);
    bag.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    assert!(matches!(
        bag.lookup_element_all(make_small(1), 0, &atoms),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        bag.lookup_element_all(make_small(99), 1, &atoms),
        Err(BeamError::BadArg)
    ));
    // `{1,10}` has arity 2, so position 3 is out of range for the whole call.
    assert!(matches!(
        bag.lookup_element_all(make_small(1), 3, &atoms),
        Err(BeamError::BadArg)
    ));

    let default = make_small(-1);
    assert_eq!(
        bag.lookup_element_all_or(make_small(99), 9, default, &atoms)
            .unwrap(),
        None
    );
    assert!(matches!(
        bag.lookup_element_all_or(make_small(99), 0, default, &atoms),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        bag.lookup_element_all_or(make_small(1), 3, default, &atoms),
        Err(BeamError::BadArg)
    ));
    let present = bag
        .lookup_element_all_or(make_small(1), 2, default, &atoms)
        .unwrap()
        .expect("a present key yields its elements");
    assert_eq!(present.len(), 1);
    assert!(eq(present[0], make_small(10), &atoms));

    // The scalar form must never fabricate one of those elements.
    let err = bag
        .lookup_element(make_small(1), 2, &atoms)
        .expect_err("a scalar answer is undefined for a bag");
    let raise = err
        .into_raise()
        .expect("the multi-kind refusal is an explicit raise");
    assert_eq!(raise.class, beam_common::ExceptionClass::Error);
    assert!(
        raise
            .reason
            .as_atom()
            .expect("the reason is an atom")
            .starts_with("unsupported_ets: ets:lookup_element/3,4"),
        "the raise names the unsupported scalar lookup"
    );

    // The table is still usable (nothing was mutated by the refusals).
    assert_eq!(bag.size(), 1);
    bag.insert(object(&mut heap, 1, 20), &atoms).unwrap();
    assert_eq!(bag.lookup_element_all(make_small(1), 2, &atoms).unwrap().len(), 2);
}

/// A `{keypos, 2}` `bag`: the projected position is independent of `keypos`.
#[test]
fn lookup_element_all_honours_a_custom_keypos() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::Bag);
    opts.set_keypos(2).unwrap();
    let mut t = beam_ets::Table::new(beam_ets::TableId::next(), opts).unwrap();

    // `{Tag, Key}` objects: element 2 is the key, element 1 the tag.
    t.insert(tuple_of(&mut heap, &[make_small(1), make_small(50)]), &atoms)
        .unwrap();
    t.insert(tuple_of(&mut heap, &[make_small(2), make_small(50)]), &atoms)
        .unwrap();

    let keys = t.lookup_element_all(make_small(50), 2, &atoms).unwrap();
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().all(|k| eq(*k, make_small(50), &atoms)));
    let tags = t.lookup_element_all(make_small(50), 1, &atoms).unwrap();
    assert_eq!(tags.len(), 2);
    assert!(eq(tags[0], make_small(1), &atoms) && eq(tags[1], make_small(2), &atoms));
}

// ---------------------------------------------------------------------------
// The multi-kind battery (oracle-verified, then pinned)
// ---------------------------------------------------------------------------

/// The four kinds, each with the name the oracle prints for it (`~p`).
const KINDS: [(TableKind, &str); 4] = [
    (TableKind::Set, "set"),
    (TableKind::OrderedSet, "ordered_set"),
    (TableKind::Bag, "bag"),
    (TableKind::DuplicateBag, "duplicate_bag"),
];

/// A table of `kind` with the crate's default interiors and an optional explicit
/// `keypos`.
fn table(kind: TableKind, keypos: Option<usize>) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(kind);
    if let Some(keypos) = keypos {
        opts.set_keypos(keypos).expect("a keypos >= 1");
    }
    Table::new(TableId::next(), opts).expect("a fresh table is constructible")
}

/// Canonical rendering of the term classes the battery is built from — small
/// integers, atoms, floats, tuples and proper lists — matching the oracle's `~p`
/// for those classes. Anything else renders as an unmatchable marker, so a stray
/// term shows up as a loud diff rather than a plausible string.
fn render(term: Eterm, atoms: &AtomTable) -> String {
    if is_small(term) {
        return signed_value(term).to_string();
    }
    if is_atom(term) {
        return match atoms.name(atom_val(term)) {
            Some(name) => name.to_string(),
            None => format!("#atom{}", atom_val(term).raw()),
        };
    }
    // SAFETY: `is_float` classifies `term` through its tag/header word only.
    if unsafe { is_float(term) } {
        // SAFETY: the check above established a live `[HEADER_FLONUM, bits]` block.
        return format!("{:?}", unsafe { float_value(term) });
    }
    if is_nil(term) {
        return "[]".to_string();
    }
    if is_list(term) {
        let mut parts = Vec::new();
        let mut cursor = term;
        while is_list(cursor) {
            // SAFETY: `is_list` established a live cons cell, so both `[CAR, CDR]`
            // words are initialised.
            let (head, tail) = unsafe { (car(cursor), cdr(cursor)) };
            parts.push(render(head, atoms));
            cursor = tail;
        }
        if !is_nil(cursor) {
            parts.push(format!("|{}", render(cursor, atoms)));
        }
        return format!("[{}]", parts.join(","));
    }
    // SAFETY: `is_tuple` classifies `term` by its tag/header word and only reads
    // further once the boxed arityval shape is established.
    if unsafe { is_tuple(term) } {
        // SAFETY: the check above established a live tuple whose header word is a
        // valid arityval, so its arity is readable.
        let arity = unsafe { tuple_size(term) };
        let parts: Vec<String> = (0..arity)
            // SAFETY: `i < arity == tuple_size(term)` is `tuple_element`'s precondition.
            .map(|i| render(unsafe { tuple_element(term, i) }, atoms))
            .collect();
        return format!("{{{}}}", parts.join(","));
    }
    format!("#term{:#x}", term.raw())
}

/// The rendered elements of `terms`, in the structural term order — the oracle's
/// `lists:sort/1` (compared with [`cmp`], not string order).
fn sorted(terms: &[Eterm], atoms: &AtomTable) -> String {
    let mut copy = terms.to_vec();
    copy.sort_by(|&a, &b| cmp(a, b, atoms));
    let parts: Vec<String> = copy.iter().map(|&t| render(t, atoms)).collect();
    format!("[{}]", parts.join(","))
}

/// A `{'_', value}` match pattern — the `head` of a `select_delete/2` specification
/// and the argument of `match_delete/2`.
fn pattern_value(heap: &mut Heap, underscore: Eterm, value: i64) -> Eterm {
    tuple_of(heap, &[underscore, make_small(value)])
}

/// The match specification `[{{'_',10},[],[true]}]` on `heap`.
fn spec_value_10(heap: &mut Heap, underscore: Eterm, true_atom: Eterm) -> Eterm {
    let head = pattern_value(heap, underscore, 10);
    let body = proper_list(heap, &[true_atom]);
    let clause = tuple_of(heap, &[head, NIL, body]);
    proper_list(heap, &[clause])
}

/// `Ok` renders as `true` (`ets:insert/2`'s result); any failure is checked to be
/// the oracle's `badarg`, so a different failure would be printed loudly.
fn result_fact(result: beam_common::Result<()>) -> String {
    match result {
        Ok(()) => "true".to_string(),
        Err(BeamError::BadArg) => "badarg".to_string(),
        Err(other) => format!("#error{other:?}"),
    }
}

/// Drive every kind through the shared script and collect its `key -> value`
/// facts (rendered exactly like the oracle program's `~p` lines).
fn battery() -> BTreeMap<String, String> {
    let mut atoms = AtomTable::new();
    let underscore = interned_atom(&mut atoms, "_");
    let true_atom = interned_atom(&mut atoms, "true");
    let atom_a = interned_atom(&mut atoms, "a");
    let atom_b = interned_atom(&mut atoms, "b");
    let mut heap = Heap::new(1 << 16);
    let mut facts = BTreeMap::new();

    // A helper keeping the `insert`/`delete` call sites terse.
    macro_rules! put {
        ($key:expr, $value:expr) => {
            facts.insert($key, $value);
        };
    }

    for (kind, name) in KINDS {
        let f = |suffix: &str| format!("{name}.{suffix}");

        // (1) duplicate objects: a `bag` rejects an identical object, a
        // `duplicate_bag` keeps every insert, and the single-object kinds upsert.
        let mut t = table(kind, None);
        t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
        t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
        t.insert(object(&mut heap, 1, 20), &atoms).unwrap();
        put!(f("dup.size"), t.size().to_string());
        put!(f("dup.lookup"), sorted(&t.lookup(make_small(1), &atoms), &atoms));

        // (2) `delete_object/2` on a structurally-equal **copy** (a separately
        // built object, so it is never the stored one) removes every equal object.
        let mut t = table(kind, None);
        t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
        t.insert(object(&mut heap, 1, 20), &atoms).unwrap();
        t.delete_object(object(&mut heap, 1, 10), &atoms).unwrap();
        put!(f("delo.size"), t.size().to_string());
        put!(f("delo.lookup"), sorted(&t.lookup(make_small(1), &atoms), &atoms));

        // (3) the same key with a different non-key content: an upsert for the
        // single-object kinds, an append for the multi kinds.
        let mut t = table(kind, None);
        t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
        t.insert(object(&mut heap, 1, 20), &atoms).unwrap();
        put!(f("upsert.size"), t.size().to_string());
        put!(f("upsert.list"), sorted(&t.tab2list(), &atoms));

        // (4) `size/1` counts objects, not keys; `delete/2` drops the whole key.
        let mut t = table(kind, None);
        for (key, value) in [(1, 10), (1, 20), (2, 30), (2, 40)] {
            t.insert(object(&mut heap, key, value), &atoms).unwrap();
        }
        put!(f("four.size"), t.size().to_string());
        t.delete(make_small(2), &atoms).unwrap();
        put!(f("del.size"), t.size().to_string());
        put!(f("del.list"), sorted(&t.tab2list(), &atoms));
        t.delete(make_small(99), &atoms).unwrap();
        put!(f("del.absent.size"), t.size().to_string());

        // (5) `insert_new/2` refuses an existing key for **every** kind (including
        // `duplicate_bag`), whatever the object's non-key content.
        let mut t = table(kind, None);
        put!(f("insnew.first"), t.insert_new(object(&mut heap, 1, 10), &atoms).unwrap().to_string());
        put!(f("insnew.dup"), t.insert_new(object(&mut heap, 1, 20), &atoms).unwrap().to_string());
        put!(f("insnew.size"), t.size().to_string());
        put!(f("insnew.newkey"), t.insert_new(object(&mut heap, 2, 20), &atoms).unwrap().to_string());
        put!(f("insnew.size2"), t.size().to_string());

        // (6) `insert_list/2` is atomic for the multi kinds too: one bad element
        // rejects the whole list and leaves the table unchanged.
        let mut t = table(kind, None);
        let ok = t.insert_list(
            &[
                object(&mut heap, 1, 10),
                object(&mut heap, 2, 20),
                object(&mut heap, 1, 11),
            ],
            &atoms,
        );
        put!(f("inslist.ok"), result_fact(ok));
        put!(f("inslist.ok.size"), t.size().to_string());
        put!(f("inslist.ok.list"), sorted(&t.tab2list(), &atoms));
        let bad = t.insert_list(
            &[
                object(&mut heap, 3, 30),
                make_small(42),
                object(&mut heap, 4, 40),
            ],
            &atoms,
        );
        put!(f("inslist.bad"), result_fact(bad));
        put!(f("inslist.bad.size"), t.size().to_string());
        put!(f("inslist.bad.list"), sorted(&t.tab2list(), &atoms));

        // (7) `select_delete/2` counts **objects** deleted on the multi kinds.
        let mut t = table(kind, None);
        for (key, value) in [(1, 10), (1, 20), (2, 10)] {
            t.insert(object(&mut heap, key, value), &atoms).unwrap();
        }
        let spec = compile(spec_value_10(&mut heap, underscore, true_atom), &atoms).unwrap();
        let count = t
            .select_delete(&spec, &atoms, &mut rebuild_heap_words(1 << 12))
            .unwrap();
        put!(f("seldel.count"), count.to_string());
        put!(f("seldel.size"), t.size().to_string());
        put!(f("seldel.list"), sorted(&t.tab2list(), &atoms));

        // (8) `match_delete/2`'s deleted-object count (the oracle returns the atom
        // `true`; its size delta is the count this crate reports instead).
        let mut t = table(kind, None);
        for (key, value) in [(1, 10), (1, 20), (2, 10)] {
            t.insert(object(&mut heap, key, value), &atoms).unwrap();
        }
        put!(f("matchdel.before"), t.size().to_string());
        let count = t
            .match_delete(pattern_value(&mut heap, underscore, 10), &atoms, &mut rebuild_heap_words(1 << 12))
            .unwrap();
        put!(f("matchdel.count"), count.to_string());
        put!(f("matchdel.after.size"), t.size().to_string());
        put!(f("matchdel.after.list"), sorted(&t.tab2list(), &atoms));

        // (9) a `{keypos, 2}` table: the key is element 2, and a
        // `delete_object/2` on it still removes exactly the equal object.
        let mut t = table(kind, Some(2));
        t.insert(tuple_of(&mut heap, &[make_small(10), make_small(1)]), &atoms).unwrap();
        t.insert(tuple_of(&mut heap, &[make_small(20), make_small(1)]), &atoms).unwrap();
        put!(f("kp2.size"), t.size().to_string());
        put!(f("kp2.lookup"), sorted(&t.lookup(make_small(1), &atoms), &atoms));
        t.delete_object(tuple_of(&mut heap, &[make_small(10), make_small(1)]), &atoms).unwrap();
        put!(f("kp2.after_delo.list"), sorted(&t.tab2list(), &atoms));

        // (10) `insert_new/2` on a `{keypos, 2}` table refuses the *key* element.
        let mut t = table(kind, Some(2));
        put!(f("kp2.insnew.first"), t.insert_new(tuple_of(&mut heap, &[make_small(10), make_small(1)]), &atoms).unwrap().to_string());
        put!(f("kp2.insnew.dup"), t.insert_new(tuple_of(&mut heap, &[make_small(99), make_small(1)]), &atoms).unwrap().to_string());
        put!(f("kp2.insnew.size"), t.size().to_string());

        // (11) duplicates **plus** `select_delete/2`: the count is the number of
        // objects removed, including identical ones for a `duplicate_bag`.
        let mut t = table(kind, None);
        t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
        t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
        t.insert(object(&mut heap, 2, 10), &atoms).unwrap();
        let spec = compile(spec_value_10(&mut heap, underscore, true_atom), &atoms).unwrap();
        let count = t
            .select_delete(&spec, &atoms, &mut rebuild_heap_words(1 << 12))
            .unwrap();
        put!(f("dupseldel.count"), count.to_string());
        put!(f("dupseldel.size"), t.size().to_string());
        put!(f("dupseldel.list"), sorted(&t.tab2list(), &atoms));
    }

    // (12) the `ordered_set` replace-key-term rule: an insert whose key is
    // `cmp_arith`-equal to a stored key replaces the stored KEY term too, so
    // navigation observes the last-inserted term (`1` vs `1.0`).
    let mut t = table(TableKind::OrderedSet, None);
    t.insert(tuple_of(&mut heap, &[make_small(1), atom_a]), &atoms).unwrap();
    t.insert(tuple_of(&mut heap, &[flonum(1.0), atom_b]), &atoms).unwrap();
    put!("ord.keyterm.size".to_string(), t.size().to_string());
    put!("ord.keyterm.first".to_string(), render(t.first(&atoms).unwrap(), &atoms));
    put!("ord.keyterm.last".to_string(), render(t.last(&atoms).unwrap(), &atoms));
    put!("ord.keyterm.list".to_string(), sorted(&t.tab2list(), &atoms));
    put!("ord.keyterm.lookup1".to_string(), sorted(&t.lookup(make_small(1), &atoms), &atoms));
    put!("ord.keyterm.lookupf".to_string(), sorted(&t.lookup(flonum(1.0), &atoms), &atoms));

    let mut t = table(TableKind::OrderedSet, None);
    t.insert(tuple_of(&mut heap, &[flonum(1.0), atom_b]), &atoms).unwrap();
    t.insert(tuple_of(&mut heap, &[make_small(1), atom_a]), &atoms).unwrap();
    put!("ord.keyterm.rev.first".to_string(), render(t.first(&atoms).unwrap(), &atoms));
    put!("ord.keyterm.rev.list".to_string(), sorted(&t.tab2list(), &atoms));

    // (13) a `bag` no-op duplicate: a third identical object changes nothing.
    let mut t = table(TableKind::Bag, None);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 1, 20), &atoms).unwrap();
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    put!("bag.noop.size".to_string(), t.size().to_string());
    put!("bag.noop.list".to_string(), sorted(&t.tab2list(), &atoms));

    facts
}

/// Every fact the battery must produce — the figures printed by the system `erl`
/// (OTP 29.1.1 / erts-17.1) for the identical script. Recording them here keeps
/// the rules pinned even with no `erl` on `$PATH`; the oracle test below
/// re-derives them from the live oracle.
#[rustfmt::skip]
const EXPECTED: &[(&str, &str)] = &[
    // (1) duplicate objects
    ("set.dup.size", "1"), ("set.dup.lookup", "[{1,20}]"),
    ("ordered_set.dup.size", "1"), ("ordered_set.dup.lookup", "[{1,20}]"),
    ("bag.dup.size", "2"), ("bag.dup.lookup", "[{1,10},{1,20}]"),
    ("duplicate_bag.dup.size", "3"), ("duplicate_bag.dup.lookup", "[{1,10},{1,10},{1,20}]"),
    // (2) delete_object on a non-anchor copy
    ("set.delo.size", "1"), ("set.delo.lookup", "[{1,20}]"),
    ("ordered_set.delo.size", "1"), ("ordered_set.delo.lookup", "[{1,20}]"),
    ("bag.delo.size", "1"), ("bag.delo.lookup", "[{1,20}]"),
    ("duplicate_bag.delo.size", "1"), ("duplicate_bag.delo.lookup", "[{1,20}]"),
    // (3) same key, different non-key content
    ("set.upsert.size", "1"), ("set.upsert.list", "[{1,20}]"),
    ("ordered_set.upsert.size", "1"), ("ordered_set.upsert.list", "[{1,20}]"),
    ("bag.upsert.size", "2"), ("bag.upsert.list", "[{1,10},{1,20}]"),
    ("duplicate_bag.upsert.size", "2"), ("duplicate_bag.upsert.list", "[{1,10},{1,20}]"),
    // (4) size counts objects; delete by key
    ("set.four.size", "2"), ("set.del.size", "1"), ("set.del.list", "[{1,20}]"), ("set.del.absent.size", "1"),
    ("ordered_set.four.size", "2"), ("ordered_set.del.size", "1"), ("ordered_set.del.list", "[{1,20}]"), ("ordered_set.del.absent.size", "1"),
    ("bag.four.size", "4"), ("bag.del.size", "2"), ("bag.del.list", "[{1,10},{1,20}]"), ("bag.del.absent.size", "2"),
    ("duplicate_bag.four.size", "4"), ("duplicate_bag.del.size", "2"), ("duplicate_bag.del.list", "[{1,10},{1,20}]"), ("duplicate_bag.del.absent.size", "2"),
    // (5) insert_new refuses an existing key for every kind
    ("set.insnew.first", "true"), ("set.insnew.dup", "false"), ("set.insnew.size", "1"), ("set.insnew.newkey", "true"), ("set.insnew.size2", "2"),
    ("ordered_set.insnew.first", "true"), ("ordered_set.insnew.dup", "false"), ("ordered_set.insnew.size", "1"), ("ordered_set.insnew.newkey", "true"), ("ordered_set.insnew.size2", "2"),
    ("bag.insnew.first", "true"), ("bag.insnew.dup", "false"), ("bag.insnew.size", "1"), ("bag.insnew.newkey", "true"), ("bag.insnew.size2", "2"),
    ("duplicate_bag.insnew.first", "true"), ("duplicate_bag.insnew.dup", "false"), ("duplicate_bag.insnew.size", "1"), ("duplicate_bag.insnew.newkey", "true"), ("duplicate_bag.insnew.size2", "2"),
    // (6) insert_list atomicity
    ("set.inslist.ok", "true"), ("set.inslist.ok.size", "2"), ("set.inslist.ok.list", "[{1,11},{2,20}]"), ("set.inslist.bad", "badarg"), ("set.inslist.bad.size", "2"), ("set.inslist.bad.list", "[{1,11},{2,20}]"),
    ("ordered_set.inslist.ok", "true"), ("ordered_set.inslist.ok.size", "2"), ("ordered_set.inslist.ok.list", "[{1,11},{2,20}]"), ("ordered_set.inslist.bad", "badarg"), ("ordered_set.inslist.bad.size", "2"), ("ordered_set.inslist.bad.list", "[{1,11},{2,20}]"),
    ("bag.inslist.ok", "true"), ("bag.inslist.ok.size", "3"), ("bag.inslist.ok.list", "[{1,10},{1,11},{2,20}]"), ("bag.inslist.bad", "badarg"), ("bag.inslist.bad.size", "3"), ("bag.inslist.bad.list", "[{1,10},{1,11},{2,20}]"),
    ("duplicate_bag.inslist.ok", "true"), ("duplicate_bag.inslist.ok.size", "3"), ("duplicate_bag.inslist.ok.list", "[{1,10},{1,11},{2,20}]"), ("duplicate_bag.inslist.bad", "badarg"), ("duplicate_bag.inslist.bad.size", "3"), ("duplicate_bag.inslist.bad.list", "[{1,10},{1,11},{2,20}]"),
    // (7) select_delete counts objects
    ("set.seldel.count", "1"), ("set.seldel.size", "1"), ("set.seldel.list", "[{1,20}]"),
    ("ordered_set.seldel.count", "1"), ("ordered_set.seldel.size", "1"), ("ordered_set.seldel.list", "[{1,20}]"),
    ("bag.seldel.count", "2"), ("bag.seldel.size", "1"), ("bag.seldel.list", "[{1,20}]"),
    ("duplicate_bag.seldel.count", "2"), ("duplicate_bag.seldel.size", "1"), ("duplicate_bag.seldel.list", "[{1,20}]"),
    // (8) match_delete count
    ("set.matchdel.before", "2"), ("set.matchdel.count", "1"), ("set.matchdel.after.size", "1"), ("set.matchdel.after.list", "[{1,20}]"),
    ("ordered_set.matchdel.before", "2"), ("ordered_set.matchdel.count", "1"), ("ordered_set.matchdel.after.size", "1"), ("ordered_set.matchdel.after.list", "[{1,20}]"),
    ("bag.matchdel.before", "3"), ("bag.matchdel.count", "2"), ("bag.matchdel.after.size", "1"), ("bag.matchdel.after.list", "[{1,20}]"),
    ("duplicate_bag.matchdel.before", "3"), ("duplicate_bag.matchdel.count", "2"), ("duplicate_bag.matchdel.after.size", "1"), ("duplicate_bag.matchdel.after.list", "[{1,20}]"),
    // (9) keypos 2
    ("set.kp2.size", "1"), ("set.kp2.lookup", "[{20,1}]"), ("set.kp2.after_delo.list", "[{20,1}]"),
    ("ordered_set.kp2.size", "1"), ("ordered_set.kp2.lookup", "[{20,1}]"), ("ordered_set.kp2.after_delo.list", "[{20,1}]"),
    ("bag.kp2.size", "2"), ("bag.kp2.lookup", "[{10,1},{20,1}]"), ("bag.kp2.after_delo.list", "[{20,1}]"),
    ("duplicate_bag.kp2.size", "2"), ("duplicate_bag.kp2.lookup", "[{10,1},{20,1}]"), ("duplicate_bag.kp2.after_delo.list", "[{20,1}]"),
    // (10) keypos 2 insert_new
    ("set.kp2.insnew.first", "true"), ("set.kp2.insnew.dup", "false"), ("set.kp2.insnew.size", "1"),
    ("ordered_set.kp2.insnew.first", "true"), ("ordered_set.kp2.insnew.dup", "false"), ("ordered_set.kp2.insnew.size", "1"),
    ("bag.kp2.insnew.first", "true"), ("bag.kp2.insnew.dup", "false"), ("bag.kp2.insnew.size", "1"),
    ("duplicate_bag.kp2.insnew.first", "true"), ("duplicate_bag.kp2.insnew.dup", "false"), ("duplicate_bag.kp2.insnew.size", "1"),
    // (11) duplicates + select_delete
    ("set.dupseldel.count", "2"), ("set.dupseldel.size", "0"), ("set.dupseldel.list", "[]"),
    ("ordered_set.dupseldel.count", "2"), ("ordered_set.dupseldel.size", "0"), ("ordered_set.dupseldel.list", "[]"),
    ("bag.dupseldel.count", "2"), ("bag.dupseldel.size", "0"), ("bag.dupseldel.list", "[]"),
    ("duplicate_bag.dupseldel.count", "3"), ("duplicate_bag.dupseldel.size", "0"), ("duplicate_bag.dupseldel.list", "[]"),
    // (12) the ordered_set replace-key-term rule
    ("ord.keyterm.size", "1"), ("ord.keyterm.first", "1.0"), ("ord.keyterm.last", "1.0"),
    ("ord.keyterm.list", "[{1.0,b}]"), ("ord.keyterm.lookup1", "[{1.0,b}]"), ("ord.keyterm.lookupf", "[{1.0,b}]"),
    ("ord.keyterm.rev.first", "1"), ("ord.keyterm.rev.list", "[{1,a}]"),
    // (13) the bag no-op duplicate
    ("bag.noop.size", "2"), ("bag.noop.list", "[{1,10},{1,20}]"),
];

/// One fact of the battery (panics — loudly — on a missing key).
fn fact<'a>(facts: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    facts
        .get(key)
        .unwrap_or_else(|| panic!("the battery printed no `{key}` fact"))
        .as_str()
}

/// The crate's rules, pinned to the recorded oracle figures (no oracle needed).
#[test]
fn multi_kind_battery_matches_the_recorded_oracle_figures() {
    let facts = battery();
    for (key, want) in EXPECTED {
        assert_eq!(fact(&facts, key), *want, "fact `{key}`");
    }
    assert_eq!(
        facts.len(),
        EXPECTED.len(),
        "the battery printed a different number of facts than recorded"
    );
}

// ---------------------------------------------------------------------------
// The live oracle cross-check
// ---------------------------------------------------------------------------

/// Wall-clock bound for the oracle program.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Run the oracle battery once and return its `key -> value` facts. Returns
/// `None` (after a **loud** skip, or a failure under `BEAM_ETS_REQUIRE_ORACLE=1`)
/// when no Erlang oracle is available.
fn oracle_facts(test: &str) -> Option<BTreeMap<String, String>> {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            test,
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable `erl`/`escript` on $PATH)",
        );
        return None;
    };
    let output = oracle::run_escript(&escript, BAG_ORACLE_PROGRAM, &[], test, ORACLE_TIMEOUT);
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut facts = BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(" -> ") {
            facts.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    // The battery prints one line per fact; a short read means the program died
    // half way through and every missing key would otherwise be reported as one
    // confusing assertion failure.
    assert!(
        facts.len() >= EXPECTED.len(),
        "the oracle program printed only {} facts (expected at least {}):\n{text}",
        facts.len(),
        EXPECTED.len()
    );
    Some(facts)
}

/// The crate's facts equal the live oracle's, key by key (a divergence names the
/// exact rule rather than dumping two maps).
#[test]
fn multi_kind_battery_matches_the_live_oracle() {
    let Some(oracle) = oracle_facts("multi-kind") else {
        return;
    };
    let ours = battery();
    for (key, want) in &oracle {
        assert_eq!(
            fact(&ours, key),
            want.as_str(),
            "fact `{key}`: the crate and the live oracle disagree"
        );
    }
    assert_eq!(
        ours.len(),
        oracle.len(),
        "the crate battery and the oracle battery printed different fact counts"
    );
}

// ---------------------------------------------------------------------------
// The randomized operation trace
// ---------------------------------------------------------------------------
//
// The battery pins one shape at a time; this drives a deterministic pseudo-random
// *sequence* of insert/insert_new/delete/delete_object/delete_all_objects/
// match_delete/insert_list/lookup steps through every kind and compares the
// `{size, sorted tab2list}` after **every** step with the live oracle. It is the
// state-interaction net for the multi-kind storage rules (an entry whose key or
// survivor was reclaimed shows up as a size or a term that stops matching).

/// How many steps the trace drives (per kind). With this seed the tables reach 16
/// objects and hold identical objects (for a `duplicate_bag`) in many states.
const TRACE_STEPS: usize = 120;

/// The trace's LCG. `band 16#7FFFFFFF` after an exact product is reproduced here
/// by masking the low 31 bits of the wrapping 64-bit product (the low 31 bits of
/// a product do not depend on the width).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12_345) & 0x7FFF_FFFF;
        self.0
    }

    /// The next draw in `1..=bound`.
    fn draw(&mut self, bound: u64) -> i64 {
        (self.next() % bound) as i64 + 1
    }
}

/// Drive the trace on the crate's table and collect `"<kind>.<step>" -> "{S,L}"`.
fn trace() -> BTreeMap<String, String> {
    let mut atoms = AtomTable::new();
    let underscore = interned_atom(&mut atoms, "_");
    let mut heap = Heap::new(1 << 18);
    let mut facts = BTreeMap::new();

    for (kind, name) in KINDS {
        let mut t = table(kind, None);
        let mut lcg = Lcg(1);
        for step in 1..=TRACE_STEPS {
            // Always consume exactly three draws, whatever the operation, so the
            // crate and the oracle stay in lockstep. Inserts dominate the op space
            // (a destructive-only script never reaches an interesting state).
            let op = lcg.next() % 12;
            let key = lcg.draw(4);
            let value = lcg.draw(3);
            match op {
                0 | 1 | 2 | 11 => t.insert(object(&mut heap, key, value), &atoms).unwrap(),
                3 => {
                    t.insert_new(object(&mut heap, key, value), &atoms).unwrap();
                }
                4 | 5 => t
                    .delete_object(object(&mut heap, key, value), &atoms)
                    .unwrap(),
                6 => t.delete(make_small(key), &atoms).unwrap(),
                7 => {
                    t.match_delete(
                        pattern_value(&mut heap, underscore, value),
                        &atoms,
                        &mut rebuild_heap_words(1 << 12),
                    )
                    .unwrap();
                }
                8 => t
                    .insert_list(
                        &[
                            object(&mut heap, key, value),
                            object(&mut heap, key + 10, value + 10),
                        ],
                        &atoms,
                    )
                    .unwrap(),
                9 => {
                    let _ = t.lookup(make_small(key), &atoms);
                }
                10 => t.delete_all_objects(),
                _ => unreachable!("`op` is drawn modulo 12"),
            }
            facts.insert(
                format!("{name}.{step}"),
                format!("{{{},{}}}", t.size(), sorted(&t.tab2list(), &atoms)),
            );
        }
    }
    facts
}

/// The crate's trace equals the live oracle's, step by step.
#[test]
fn randomised_multi_kind_trace_matches_the_live_oracle() {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "trace",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable `erl`/`escript` on $PATH)",
        );
        return;
    };
    let output = oracle::run_escript(&escript, BAG_TRACE_ORACLE_PROGRAM, &[], "trace", ORACLE_TIMEOUT);
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut oracle = BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(" -> ") {
            oracle.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    assert!(
        oracle.len() >= TRACE_STEPS,
        "the oracle trace printed only {} steps:\n{text}",
        oracle.len()
    );

    let ours = trace();
    for (key, want) in &oracle {
        assert_eq!(
            fact(&ours, key),
            want.as_str(),
            "step `{key}`: the crate and the live oracle disagree"
        );
    }
    assert_eq!(ours.len(), oracle.len(), "different step counts");
}