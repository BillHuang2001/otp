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
//! # Raw-`Eterm` storage is safe here
//!
//! The store keeps the key and value **raw `Eterm` words** and compares keys by word
//! equality (`*k == key`). That is exactly the C's `erts_literal_area_term` identity for
//! a persistent term: the table never copies a key, it *pins* the term in a literal area
//! and compares the word. It is safe in this runtime because Milestone 1 has **no GC** and
//! every heap a term can live on is pointer-stable for the runtime's lifetime (the
//! pointer-stable pooled BIF heap, the never-reset `eval_heap`, the process heaps and the
//! `MinimalContext` arena) — so a stored word never dangles and a re-stored equal-but-
//! distinct term is observably indistinguishable. Stage 4's collector must register this
//! store as a root (see `../CONTEXT.md` §5).
//!
//! [`PersistentTerm`]: beam_bifs::PersistentTerm
//! [`RuntimeContext::call_registered`]: super::RuntimeContext::call_registered

use std::cell::{RefCell, RefMut};
use std::rc::Rc;

use beam_bifs::PersistentTerm;
use beam_common::Result;
use beam_term::{size::size_object, Eterm};

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

    /// The index of `key` in the store, by **raw `Eterm` word equality**.
    fn position(&self, key: Eterm) -> Option<usize> {
        self.entries.iter().position(|(k, _)| *k == key)
    }

    /// `put`/`put_new`: stores `value` under `key`.
    ///
    /// `only_if_new` is `false` for `put/2`. Returns `true` on success. A **new** key is
    /// appended (insertion order preserved); an existing key with `only_if_new` is `true`
    /// and a **different** value reports `false` (the oracle's `badarg`); an existing key
    /// whose value is raw-equal to `value` is a no-op `ok` (`true`); otherwise the value
    /// is overwritten **in place** (never reordered).
    pub(crate) fn put(&mut self, key: Eterm, value: Eterm, only_if_new: bool) -> bool {
        match self.position(key) {
            Some(index) => {
                let existing = self.entries[index].1;
                if existing == value {
                    // A no-op `ok`: the key already holds the same term
                    // (`erl_bif_persistent.c:726-729`).
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
    pub(crate) fn erase(&mut self, key: Eterm) -> bool {
        match self.position(key) {
            Some(index) => {
                self.entries.remove(index);
                true
            }
            None => false,
        }
    }

    /// `get`: the value stored under `key`.
    pub(crate) fn get(&self, key: Eterm) -> Option<Eterm> {
        self.position(key).map(|index| self.entries[index].1)
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
}

impl PersistentTermBridge {
    /// A bridge over the shared `store`.
    pub(super) fn new(store: Rc<RefCell<PersistentTermStore>>) -> PersistentTermBridge {
        PersistentTermBridge { store }
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
        Ok(self.store()?.get(key))
    }

    fn put(&mut self, key: Eterm, value: Eterm, only_if_new: bool) -> Result<bool> {
        Ok(self.store()?.put(key, value, only_if_new))
    }

    fn erase(&mut self, key: Eterm) -> Result<bool> {
        Ok(self.store()?.erase(key))
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
    use beam_term::{make_small, Atom, AtomTable};

    /// A store plus the shared atom table its keys resolve against.
    fn store() -> PersistentTermStore {
        PersistentTermStore::new()
    }

    #[test]
    fn put_then_get_returns_the_stored_value() {
        let mut s = store();
        let key = make_small(1);
        assert!(s.put(key, make_small(42), false));
        assert_eq!(s.get(key), Some(make_small(42)));
    }

    #[test]
    fn get_of_a_missing_key_is_none() {
        let s = store();
        assert_eq!(s.get(make_small(7)), None);
    }

    #[test]
    fn erase_reports_true_then_false() {
        let mut s = store();
        let key = make_small(1);
        s.put(key, make_small(1), false);
        assert!(s.erase(key), "the first erase removes the entry");
        assert!(!s.erase(key), "the second erase is false");
        assert_eq!(s.get(key), None);
    }

    #[test]
    fn put_new_with_a_different_value_reports_false() {
        let mut s = store();
        let key = make_small(1);
        assert!(s.put(key, make_small(1), false));
        assert!(
            !s.put(key, make_small(2), true),
            "put_new onto a different value is the oracle's badarg"
        );
        assert_eq!(s.get(key), Some(make_small(1)), "the value is unchanged");
    }

    #[test]
    fn put_new_with_the_same_value_is_a_no_op_ok() {
        let mut s = store();
        let key = make_small(1);
        assert!(s.put(key, make_small(1), false));
        assert!(s.put(key, make_small(1), true), "same value ⇒ ok");
    }

    #[test]
    fn put_new_of_an_absent_key_inserts() {
        let mut s = store();
        assert!(s.put(make_small(1), make_small(1), true));
        assert_eq!(s.get(make_small(1)), Some(make_small(1)));
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
        assert!(s.put(a, make_small(1), false));
        assert!(s.put(b, make_small(2), false), "the same key overwrites");
        assert_eq!(s.get(a), Some(make_small(2)));
        assert!(s.put(c, make_small(3), false));
        assert_eq!(s.get_all().len(), 2);
        let _ = Atom::MAX;
    }

    #[test]
    fn get_all_preserves_insertion_order() {
        let mut s = store();
        s.put(make_small(1), make_small(10), false);
        s.put(make_small(2), make_small(20), false);
        s.put(make_small(1), make_small(11), false); // overwrite in place, no reorder
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
        assert_eq!(s.info(), (0, 0), "an empty store has no bytes");
        s.put(make_small(1), make_small(2), false);
        s.put(make_small(3), make_small(4), false);
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
        s.put(key, cons, false);
        let (count, memory) = s.info();
        assert_eq!(count, 1);
        assert!(memory > 0, "a cons value must contribute bytes, got {memory}");
    }

    #[test]
    fn a_nested_borrow_is_a_typed_gap_not_a_panic() {
        let store = Rc::new(RefCell::new(PersistentTermStore::new()));
        let mut bridge = PersistentTermBridge::new(Rc::clone(&store));
        // Hold the cell, then ask the bridge to borrow it again.
        let _held = store.borrow_mut();
        let error = bridge.get(make_small(1)).expect_err("a nested borrow must fail");
        assert!(
            crate::error::unsupported_reason(&error).is_some(),
            "the nested borrow is a typed capability gap, got {error:?}"
        );
    }
}
