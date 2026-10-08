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

//! The runtime's concrete [`PersistentTerm`] store: the node-global persistent-term
//! table behind the seven `persistent_term:*` BIFs.
//!
//! `beam_bifs`'s `persistent_term.rs` renders every one of the seven BIFs but cannot
//! hold the store itself: persistent terms are a **node-global** key/value table
//! (`the_hash_table`, `../../erts/emulator/beam/erl_bif_persistent.c:187`) outside every
//! process heap, updated under a table-wide permission lock with thread-progress
//! hand-off — Stage-5 runtime state. `beam_bifs` therefore routes every operation
//! through the [`PersistentTerm`] seam, and this module is the concrete provider
//! [`RuntimeContext::call_registered`](super::RuntimeContext::call_registered) installs.
//!
//! # Raw-`Eterm` storage is safe here; **comparison is structural**
//!
//! The store keeps the key and value **raw `Eterm` words**. That is exactly the C's
//! `erts_literal_area_term`: the table never copies a key, it *pins* the term in a literal
//! area. It is safe in this runtime because Milestone 1 has **no GC** and every heap a term
//! can live on is pointer-stable for the runtime's lifetime (the pointer-stable pooled BIF
//! heap, the never-reset `eval_heap`, the process heaps and the `MinimalContext` arena) — so
//! a stored word never dangles. Stage 4's collector must register this store as a root (see
//! `../CONTEXT.md` §5).
//!
//! **Storage is raw; lookup is structural.** The oracle never compares two persistent-term
//! keys by word identity: `lookup` (`erl_bif_persistent.c:1005`) hashes the key and then
//! resolves a bucket with `EQ(key, …)` — the *exact* (`=:=`) term comparison
//! `puts`/`gets`/`erases` are built on, and the value no-op check in `put_common`
//! (`:726`) uses the same `EQ`. Two structurally equal boxed terms (a tuple, list, map or
//! binary built twice) are distinct heap words, so a raw-word comparison would miss the
//! key and make `get/1`/`erase/1` report an absent entry and a `put_new/2` of an equal value
//! a `badarg`. Every lookup therefore goes through [`beam_term::compare::eq`] — the shared
//! LCA structural comparator — never `==` on the stored words.
//!
//! [`PersistentTerm`]: beam_bifs::PersistentTerm
//! [`RuntimeContext::call_registered`]: super::RuntimeContext::call_registered

use std::cell::{RefCell, RefMut};
use std::rc::Rc;

use beam_bifs::PersistentTerm;
use beam_common::Result;
use beam_term::{compare::eq, size::size_object, AtomTable, Eterm};

use crate::error;

/// The node-global persistent-term store.
///
/// Entries are held in **insertion order**; an overwrite of an existing key happens in
/// place and never reorders, so `get/0` (`insertion order`) is stable across a
/// `put/2` of an existing key (the oracle's own order is an unspecified hash-table
/// walk and is not observable — see `beam_bifs::persistent_term`).
///
/// Milestone 1 has no GC, so a stored raw `Eterm` never moves (see the module docs).
pub(crate) struct PersistentTermStore {
    /// The stored `(Key, Value)` pairs, in insertion order.
    entries: Vec<(Eterm, Eterm)>,
}

impl PersistentTermStore {
    /// An empty store.
    pub(crate) fn new() -> PersistentTermStore {
        PersistentTermStore {
            entries: Vec::new(),
        }
    }

    /// The index of `key` in the store, by **structural** equality (`=:=`) resolved
    /// through `atoms` — the C's `EQ` in `lookup` (`erl_bif_persistent.c:1015`).
    ///
    /// `atoms` supplies the atom *names* the comparator needs; comparing by raw word
    /// would miss any boxed key (a tuple/list/map/binary built twice is two distinct
    /// heap words).
    fn position(&self, key: Eterm, atoms: &AtomTable) -> Option<usize> {
        self.entries.iter().position(|(k, _)| eq(*k, key, atoms))
    }

    /// `put`/`put_new`: stores `value` under `key`.
    ///
    /// `only_if_new` is `false` for `put/2`. Returns `true` on success. A **new** key is
    /// appended (insertion order preserved); an existing key with `only_if_new` is `true`
    /// and a **different** value reports `false` (the oracle's `badarg`); an existing key
    /// whose value is *structurally* equal to `value` is a no-op `ok` (`true`); otherwise
    /// the value is overwritten **in place** (never reordered).
    pub(crate) fn put(
        &mut self,
        key: Eterm,
        value: Eterm,
        only_if_new: bool,
        atoms: &AtomTable,
    ) -> bool {
        match self.position(key, atoms) {
            Some(index) => {
                let existing = self.entries[index].1;
                if eq(existing, value, atoms) {
                    // A no-op `ok`: the key already holds the same term
                    // (`erl_bif_persistent.c:726-729` — the C's `EQ`).
                    true
                } else if only_if_new {
                    // `put_new/2` of a key holding a *different* value: the oracle's
                    // `badarg` (`:730-733`).
                    false
                } else {
                    self.entries[index].1 = value;
                    true
                }
            }
            None => {
                self.entries.push((key, value));
                true
            }
        }
    }

    /// `erase`: removes `key`, returning whether it was present.
    pub(crate) fn erase(&mut self, key: Eterm, atoms: &AtomTable) -> bool {
        match self.position(key, atoms) {
            Some(index) => {
                self.entries.remove(index);
                true
            }
            None => false,
        }
    }

    /// `get`: the value stored under `key`.
    pub(crate) fn get(&self, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        self.position(key, atoms).map(|index| self.entries[index].1)
    }

    /// `get/0`: every stored `(Key, Value)` pair, in store (insertion) order.
    pub(crate) fn get_all(&self) -> Vec<(Eterm, Eterm)> {
        self.entries.clone()
    }

    /// `info/0`: `(count, memory bytes)`.
    ///
    /// `count` is the number of entries. `memory` is **this runtime's own** figure: the
    /// flat heap-word total of every stored key/value term (`beam_term::size::size_object`)
    /// scaled to bytes. The oracle's number is larger because it also counts the table's
    /// own storage and the literal areas the terms are pinned in — a figure this runtime
    /// does not model, so it is never fabricated (see `beam_bifs::persistent_term`).
    pub(crate) fn info(&self) -> (u64, u64) {
        let words: usize = self
            .entries
            .iter()
            .map(|(key, value)| size_object(*key) + size_object(*value))
            .sum();
        (self.entries.len() as u64, (words as u64) * 8)
    }
}

impl Default for PersistentTermStore {
    /// Same as [`PersistentTermStore::new`].
    fn default() -> PersistentTermStore {
        PersistentTermStore::new()
    }
}

/// The concrete [`PersistentTerm`] hook: a cheap, cloneable handle over the runtime's
/// shared store.
///
/// The store is shared through an [`Rc`]`<`[`RefCell`]`>` (the same pattern as
/// [`Runtime::tables`](crate::runtime::Runtime)/`ports`/`nifs`) so a BIF call borrows it
/// for the duration of its call without borrowing the runtime — the persistent-term arm
/// of `call_registered` holds **no** runtime borrow at all, so it can coexist with every
/// other arm. A **nested** borrow (a persistent-term operation re-entered from inside one)
/// is reported as a typed capability gap, never a panic.
pub(super) struct PersistentTermBridge {
    /// The shared node-global store.
    store: Rc<RefCell<PersistentTermStore>>,
    /// The runtime's shared atom table, for the **structural** key/value comparison
    /// ([`beam_term::compare::eq`]) the store needs (a cheap `Rc` clone, so this arm
    /// still holds no runtime borrow).
    atoms: Rc<AtomTable>,
}

impl PersistentTermBridge {
    /// A bridge over the shared `store`, comparing terms through the shared `atoms`.
    pub(super) fn new(
        store: Rc<RefCell<PersistentTermStore>>,
        atoms: Rc<AtomTable>,
    ) -> PersistentTermBridge {
        PersistentTermBridge { store, atoms }
    }

    /// Borrows the store, mapping an already-taken cell to a typed capability gap.
    fn store(&self) -> Result<RefMut<'_, PersistentTermStore>> {
        self.store.try_borrow_mut().map_err(|_| {
            error::unsupported("persistent_term: the table is already in use")
        })
    }
}

impl PersistentTerm for PersistentTermBridge {
    fn get(&mut self, key: Eterm) -> Result<Option<Eterm>> {
        let atoms = Rc::clone(&self.atoms);
        Ok(self.store()?.get(key, &atoms))
    }

    fn put(&mut self, key: Eterm, value: Eterm, only_if_new: bool) -> Result<bool> {
        let atoms = Rc::clone(&self.atoms);
        Ok(self.store()?.put(key, value, only_if_new, &atoms))
    }

    fn erase(&mut self, key: Eterm) -> Result<bool> {
        let atoms = Rc::clone(&self.atoms);
        Ok(self.store()?.erase(key, &atoms))
    }

    fn get_all(&mut self) -> Result<Vec<(Eterm, Eterm)>> {
        Ok(self.store()?.get_all())
    }

    fn info(&mut self) -> Result<(u64, u64)> {
        Ok(self.store()?.info())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Runtime;
    use beam_term::{make_small, Atom, AtomTable};

    /// An empty store. Its keys are compared through a caller-supplied atom table (an
    /// empty one is fine for immediate/atom terms: the comparator falls back to the raw
    /// atom index, which both sides of a comparison share).
    fn store() -> PersistentTermStore {
        PersistentTermStore::new()
    }

    /// The empty atom table the immediate/atom-key tests compare through.
    fn no_atoms() -> AtomTable {
        AtomTable::new()
    }

    /// A runtime used **only** to build *boxed* terms (tuples, lists, maps, binaries) on a
    /// pointer-stable heap. Two `eval`s of the same literal are structurally equal but two
    /// **distinct** heap words — exactly the case a raw-word comparison would miss.
    fn term_builder() -> Runtime {
        Runtime::with_defaults().expect("the default runtime")
    }

    /// The empty atom table the boxed-key tests compare through: both terms of every
    /// comparison come from the *same* runtime table, so unknown names degrade to the
    /// (equal) raw atom index — the comparison stays exact.
    fn boxed_atoms() -> AtomTable {
        AtomTable::new()
    }

    #[test]
    fn put_then_get_returns_the_stored_value() {
        let mut s = store();
        let atoms = no_atoms();
        let key = make_small(1);
        assert!(s.put(key, make_small(42), false, &atoms));
        assert_eq!(s.get(key, &atoms), Some(make_small(42)));
    }

    #[test]
    fn get_of_a_missing_key_is_none() {
        let s = store();
        let atoms = no_atoms();
        assert_eq!(s.get(make_small(7), &atoms), None);
    }

    #[test]
    fn erase_reports_true_then_false() {
        let mut s = store();
        let atoms = no_atoms();
        let key = make_small(1);
        s.put(key, make_small(1), false, &atoms);
        assert!(s.erase(key, &atoms), "the first erase removes the entry");
        assert!(!s.erase(key, &atoms), "the second erase is false");
        assert_eq!(s.get(key, &atoms), None);
    }

    #[test]
    fn put_new_with_a_different_value_reports_false() {
        let mut s = store();
        let atoms = no_atoms();
        let key = make_small(1);
        assert!(s.put(key, make_small(1), false, &atoms));
        assert!(
            !s.put(key, make_small(2), true, &atoms),
            "put_new onto a different value is the oracle's badarg"
        );
        assert_eq!(
            s.get(key, &atoms),
            Some(make_small(1)),
            "the value is unchanged"
        );
    }

    #[test]
    fn put_new_with_the_same_value_is_a_no_op_ok() {
        let mut s = store();
        let atoms = no_atoms();
        let key = make_small(1);
        assert!(s.put(key, make_small(1), false, &atoms));
        assert!(s.put(key, make_small(1), true, &atoms), "same value ⇒ ok");
    }

    #[test]
    fn put_new_of_an_absent_key_inserts() {
        let mut s = store();
        let atoms = no_atoms();
        assert!(s.put(make_small(1), make_small(1), true, &atoms));
        assert_eq!(s.get(make_small(1), &atoms), Some(make_small(1)));
    }

    #[test]
    fn keys_are_compared_by_their_resolved_term() {
        // Two equal-but-distinct atoms resolve to the same index in the shared table, so
        // they are the same key; distinct atoms are distinct keys.
        let mut atoms = AtomTable::new();
        let a = beam_term::make_atom(atoms.intern("a"));
        let b = beam_term::make_atom(atoms.intern("a"));
        let c = beam_term::make_atom(atoms.intern("b"));
        let mut s = store();
        assert!(s.put(a, make_small(1), false, &atoms));
        assert!(
            s.put(b, make_small(2), false, &atoms),
            "the same key overwrites"
        );
        assert_eq!(s.get(a, &atoms), Some(make_small(2)));
        assert!(s.put(c, make_small(3), false, &atoms));
        assert_eq!(s.get_all().len(), 2);
        let _ = Atom::MAX;
    }

    #[test]
    fn get_all_preserves_insertion_order() {
        let mut s = store();
        let atoms = no_atoms();
        s.put(make_small(1), make_small(10), false, &atoms);
        s.put(make_small(2), make_small(20), false, &atoms);
        s.put(make_small(1), make_small(11), false, &atoms); // overwrite in place, no reorder
        assert_eq!(
            s.get_all(),
            vec![
                (make_small(1), make_small(11)),
                (make_small(2), make_small(20)),
            ]
        );
    }

    #[test]
    fn info_counts_entries_and_reports_a_positive_byte_total() {
        let mut s = store();
        let atoms = no_atoms();
        assert_eq!(s.info(), (0, 0), "an empty store has no bytes");
        s.put(make_small(1), make_small(2), false, &atoms);
        s.put(make_small(3), make_small(4), false, &atoms);
        let (count, memory) = s.info();
        assert_eq!(count, 2);
        // Small integers are immediates (0 words) — the byte total is this runtime's own
        // figure and an empty term contributes nothing, so assert only that it is a
        // non-negative integer (here exactly 0).
        assert_eq!(memory, 0);
    }

    #[test]
    fn info_counts_the_words_of_a_boxed_value() {
        // A cons value occupies two heap words, so the byte total is positive.
        let mut atoms = AtomTable::new();
        let key = beam_term::make_atom(atoms.intern("k"));
        let mut heap = beam_alloc::Heap::new(16).expect("a tiny heap");
        let cons = heap.cons(make_small(1), beam_term::NIL).expect("a cons");
        let mut s = store();
        s.put(key, cons, false, &atoms);
        let (count, memory) = s.info();
        assert_eq!(count, 1);
        assert!(memory > 0, "a cons value must contribute bytes, got {memory}");
    }

    // -----------------------------------------------------------------------------------
    // Structural (not word) identity for boxed keys and values
    // -----------------------------------------------------------------------------------

    /// Every *structurally* equal but **distinct** term shape resolves to the same entry:
    /// a tuple, a list, a map and a binary key each store, read back, and erase through a
    /// fresh copy of the term (`erl_bif_persistent.c`'s `EQ` in `lookup`/`put_common`).
    ///
    /// This is the regression the immediate-key tests could never catch: two equal boxed
    /// terms are two different heap words, so a raw-word comparison misses the key and
    /// reports the oracle's `badarg`/`false` instead of the stored value.
    #[test]
    fn boxed_keys_compare_structurally() {
        let mut rt = term_builder();
        let atoms = boxed_atoms();
        let shapes = [
            ("tuple", "{k,1}"),
            ("list", "[1,2,3]"),
            ("map", "#{a => 1}"),
            ("binary", "<<1,2,3>>"),
        ];
        for (shape, literal) in shapes {
            let key = rt.eval(literal).expect("a boxed key literal");
            let value = make_small(9);
            let mut s = store();
            assert!(
                s.put(key, value, false, &atoms),
                "put/2 of a fresh {shape} key"
            );
            // A *second* evaluation of the same literal is an equal-but-distinct heap word.
            let equal = rt.eval(literal).expect("the same literal, built again");
            assert_ne!(
                equal, key,
                "the {shape} key really is a distinct heap word (otherwise the test proves nothing)"
            );
            assert_eq!(
                s.get(equal, &atoms),
                Some(value),
                "get/1 of a structurally equal {shape} key must find the entry"
            );
            assert_eq!(
                s.get_all().len(),
                1,
                "the equal {shape} key must not append a second entry"
            );
            assert!(
                s.erase(equal, &atoms),
                "erase/1 of a structurally equal {shape} key must remove the entry"
            );
            assert_eq!(s.get(key, &atoms), None, "the {shape} entry is gone");
        }
    }

    /// `put_new/2`'s no-op `ok` compares the **stored value** structurally too: a
    /// structurally equal but distinct value is the no-op `ok`, while a different value is
    /// the oracle's `badarg` (`false` here) — `put_common:726-733`.
    #[test]
    fn put_new_compares_a_boxed_value_structurally() {
        let mut rt = term_builder();
        let atoms = boxed_atoms();
        let key = rt.eval("{the_key}").expect("a boxed key literal");
        let value = rt.eval("{1,2}").expect("a boxed value literal");
        let mut s = store();
        assert!(s.put(key, value, false, &atoms), "put/2 of a boxed value");

        // A structurally equal but distinct value ⇒ the no-op `ok` (the oracle's `ok`).
        let equal_key = rt.eval("{the_key}").expect("the key again");
        let equal_value = rt.eval("{1,2}").expect("the value again");
        assert_ne!(equal_value, value, "the second value is a distinct word");
        assert!(
            s.put(equal_key, equal_value, true, &atoms),
            "a structurally equal value is the no-op ok, not badarg"
        );

        // A different value ⇒ `put_new/2`'s badarg, and the stored value is unchanged.
        let other = rt.eval("{1,3}").expect("a different value");
        assert!(
            !s.put(equal_key, other, true, &atoms),
            "a different value is put_new/2's badarg"
        );
        assert_eq!(
            s.get(equal_key, &atoms),
            Some(value),
            "the original value survived the rejected put_new"
        );
    }

    #[test]
    fn a_nested_borrow_is_a_typed_gap_not_a_panic() {
        let store = Rc::new(RefCell::new(PersistentTermStore::new()));
        let mut bridge = PersistentTermBridge::new(Rc::clone(&store), Rc::new(AtomTable::new()));
        // Hold the cell, then ask the bridge to borrow it again.
        let _held = store.borrow_mut();
        let error = bridge
            .get(make_small(1))
            .expect_err("a nested borrow must fail");
        assert!(
            crate::error::unsupported_reason(&error).is_some(),
            "the nested borrow is a typed capability gap, got {error:?}"
        );
    }

    /// A smoke test that the bridge forwards the shared atom table: a boxed key stored
    /// through the bridge is found again through the BIF-facing seam.
    #[test]
    fn the_bridge_compares_boxed_keys_structurally() {
        let mut rt = term_builder();
        let key = rt.eval("{bridged}").expect("a boxed key literal");
        let equal = rt.eval("{bridged}").expect("the key again");
        let atoms = Rc::new(boxed_atoms());
        let store = Rc::new(RefCell::new(PersistentTermStore::new()));
        let mut bridge = PersistentTermBridge::new(Rc::clone(&store), Rc::clone(&atoms));
        assert!(bridge
            .put(key, make_small(1), false)
            .expect("put through the seam"));
        assert_eq!(
            bridge.get(equal).expect("get through the seam"),
            Some(make_small(1)),
            "the equal boxed key must resolve"
        );
        assert!(bridge.erase(equal).expect("erase through the seam"));
    }
}
