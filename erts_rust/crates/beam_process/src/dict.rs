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

//! The process dictionary: [`Dict`] — the per-process `put/2`/`get/1`/`erase/1` store.
//!
//! Every Erlang process owns a small key/value store reachable through the `erlang:put/2`,
//! `erlang:get/1`, `erlang:get/0`, `erlang:erase/1`, `erlang:erase/0`, `erlang:get_keys/0`
//! and `erlang:get_keys/1` BIFs. This module models **only that store**: a list of
//! `(Key, Value)` pairs with put/get/erase/get_keys semantics matching the legacy C
//! (`erts/emulator/beam/erl_process_dict.c`). It is deliberately *not* a hash table — the C
//! keeps a hash array for O(1) lookup, but the observable behaviour is the pair set, and a
//! small ordered pair list is the simplest implementation that reproduces it (a
//! behavioural, not a structural, port; the hash array is a documented future optimisation).
//!
//! # Term ownership
//!
//! A [`Dict`] holds **terms resident in THIS process's young heap** — exactly like
//! [`crate::link::Links`] and [`crate::monitor::Monitors`] hold caller-owned terms. The BIF
//! layer is responsible for copying a key/value into the process heap before `put`ting it
//! (the store never allocates; it only records words), so the dictionary is a genuine
//! **sub-rootset** of the collector: every stored key and value must be scanned by
//! [`crate::process::Process::collect`] or the copying GC would leave a dangling reference.
//! [`Dict::gather_roots`]/[`Dict::scatter_roots`] are that seam, mirroring
//! [`crate::registers::Registers`].
//!
//! # `THE_NON_VALUE` and the `Option` design
//!
//! The C signals "no such key" with the sentinel `THE_NON_VALUE`/`am_undefined`
//! (`erl_process_dict.c`). This crate instead returns `Option<Eterm>`: `None` means *absent*
//! and the BIF layer maps it to the `undefined` atom. Storing an `undefined` atom inside the
//! [`Dict`] would be a layout assumption this crate must not make, so it never does.
//!
//! # Key equality — the C's `EQ`
//!
//! Keys — and, for `get_keys/1`, **values** — are compared with
//! [`beam_term::compare::eq`], the structural `=:=` comparator that the C's `EQ` macro
//! implements (`erts/emulator/beam/erl_utils.h:113`: `EQ(x,y) = ((x) == (y)) ||
//! (is_not_both_immed(x,y) && eq(x,y))` — a raw-word fast path for immediates, a structural
//! walk for boxed terms).
//! `erlang:put({a},1), erlang:get({a})` therefore returns `1` even though the two `{a}`
//! tuples are two *distinct* heap words: `erl_process_dict.c` uses `EQ` for the key
//! (`:400`, `:410`, `:478`, `:482`) and for the value (`:559`, `:566`), never raw word
//! equality alone.
//!
//! [`Dict`] carries the [`AtomTable`] the comparator resolves atom *names* through, as a
//! cheap [`Rc`] handle — the same convention `beam_rt`'s `CodeServer` (one shared table
//! for every loaded module) and its persistent-term store use. [`Dict::new`] starts with
//! an **empty** table: an unknown name falls back to the raw atom index, which is exactly
//! what the C does for two immediates and is exact while the runtime keeps one append-only
//! atom-index space (it does — `CodeServer` interns every module into one table). An owner
//! holding that shared table hands it over with [`Dict::with_atoms`], so the comparator
//! resolves names instead of relying on the fallback.
//!
//! # Iteration order (documented deviation, unchanged by the equality fix)
//!
//! `erlang:get/0`, `erlang:get_keys/0`, `erlang:erase/0` and `process_info(P, dictionary)`
//! report the pairs in **insertion order** here. The C instead walks a hash array
//! (`pd_hash_get_all`, `erl_process_dict.c:579`), whose order is an unspecified artifact of
//! the hash slots and of the array's grow/shrink history — and, probed on OTP 29.1.1, is
//! **not stable across VM starts**: `put(a,1), put(b,2), get()` reports `[{b,2},{a,1}]` in
//! most starts and `[{a,1},{b,2}]` in others, and `put(a,1), put(b,2), put(c,3), get()`
//! varies between `[{a,1},{c,3},{b,2}]` and `[{a,1},{b,2},{c,3}]`. The C's order therefore
//! cannot be reproduced (nor pinned by a differential case), so the crate-local oracle
//! cross-check (`tests/dict_oracle.rs`) compares `get/0` as a **set** (`lists:sort/1`) and
//! this crate fixes the one order it can offer deterministically: insertion order.
//! Reproducing the C's would mean porting the hash array — the O(1)-lookup optimization
//! this module deliberately defers (see the top of the module). The **value** semantics are
//! unaffected and are pinned by tests: `put` replaces in place, returns the replaced value
//! (last put wins), `get` returns the last `put`'s value and `erase` returns the erased
//! value.

use std::rc::Rc;

use beam_term::compare;
use beam_term::prelude::{AtomTable, Eterm};

/// The per-process dictionary: an ordered set of `(Key, Value)` pairs.
///
/// The pairs are kept in **insertion order**, with `put` on an existing key replacing the
/// value in place (last put wins) so the order is stable across overwrites. See the module
/// docs for the semantics, the term-ownership contract, the [`beam_term::compare::eq`] key
/// equality and the documented iteration-order deviation.
pub struct Dict {
    entries: Vec<(Eterm, Eterm)>,
    /// The atom table the structural comparison resolves atom names through (see the module
    /// docs). A cheap [`Rc`] handle, shared with every other holder of the table.
    atoms: Rc<AtomTable>,
}

impl Dict {
    /// An empty dictionary.
    ///
    /// Its comparisons run against an **empty** atom table, so an atom's name is unknown and
    /// the comparator falls back to the raw atom index — exactly the C's `EQ` behaviour for
    /// two immediates, and exact while the runtime keeps one append-only atom-index space.
    /// Use [`Dict::with_atoms`] to compare through a real (shared) table.
    pub fn new() -> Dict {
        Dict {
            entries: Vec::new(),
            atoms: Rc::new(AtomTable::new()),
        }
    }

    /// An empty dictionary comparing terms through `atoms`.
    ///
    /// The owner that holds the runtime's one shared atom table (the table every loaded module
    /// and every BIF interns into) passes a handle to it here, so the structural comparison
    /// resolves atom *names* rather than raw indices.
    pub fn with_atoms(atoms: Rc<AtomTable>) -> Dict {
        Dict {
            entries: Vec::new(),
            atoms,
        }
    }

    /// The C's `EQ` (`erts/emulator/beam/erl_utils.h:113`): whether `a` and `b` are *the same
    /// term* (`=:=`), structural for boxed terms, through the dictionary's atom table.
    fn eq(&self, a: Eterm, b: Eterm) -> bool {
        compare::eq(a, b, &self.atoms)
    }

    /// The index of the pair whose key equals `key`, if any.
    fn find(&self, key: Eterm) -> Option<usize> {
        self.entries.iter().position(|&(stored, _)| self.eq(stored, key))
    }

    /// Stores `value` under `key`, returning the **replaced** value.
    ///
    /// An existing key's value is replaced **in place** (the insertion order is preserved,
    /// last put wins); an absent key is appended. Returns `None` when the key was absent —
    /// the C's `am_undefined`, which the BIF layer surfaces as the `undefined` atom.
    pub fn put(&mut self, key: Eterm, value: Eterm) -> Option<Eterm> {
        match self.find(key) {
            Some(index) => Some(core::mem::replace(&mut self.entries[index].1, value)),
            None => {
                self.entries.push((key, value));
                None
            }
        }
    }

    /// The value stored under `key`, or `None` when the key is absent.
    pub fn get(&self, key: Eterm) -> Option<Eterm> {
        self.find(key).map(|index| self.entries[index].1)
    }

    /// Removes `key`, returning the value that was stored under it (or `None` when absent).
    pub fn erase(&mut self, key: Eterm) -> Option<Eterm> {
        let index = self.find(key)?;
        Some(self.entries.remove(index).1)
    }

    /// Removes every pair, returning the removed `(Key, Value)` pairs in insertion order.
    ///
    /// This is the storage half of `erlang:erase/0`; the BIF layer turns the returned pairs
    /// into the result list.
    pub fn erase_all(&mut self) -> Vec<(Eterm, Eterm)> {
        core::mem::take(&mut self.entries)
    }

    /// Every key, in insertion order (`erlang:get_keys/0`).
    pub fn get_keys(&self) -> Vec<Eterm> {
        self.entries.iter().map(|&(key, _)| key).collect()
    }

    /// Every key whose value equals `value`, in insertion order (`erlang:get_keys/1`).
    pub fn get_keys_for_value(&self, value: Eterm) -> Vec<Eterm> {
        self.entries
            .iter()
            .filter(|&&(_, stored)| self.eq(stored, value))
            .map(|&(key, _)| key)
            .collect()
    }

    /// The number of stored pairs.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the dictionary holds no pairs.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates the stored `(Key, Value)` pairs in insertion order.
    pub fn entries(&self) -> impl Iterator<Item = (Eterm, Eterm)> + '_ {
        self.entries.iter().copied()
    }

    /// Appends every stored key then value (in item order) to `out` and returns the **start
    /// offset** — the index in `out` at which this container's words begin.
    ///
    /// Pairing with [`Dict::scatter_roots`] with the same `start` makes the two a clean
    /// round-trip: `gather_roots` appends `2 * len()` words, `scatter_roots` writes exactly
    /// those back.
    pub fn gather_roots(&self, out: &mut Vec<Eterm>) -> usize {
        let start = out.len();
        for &(key, value) in &self.entries {
            out.push(key);
            out.push(value);
        }
        start
    }

    /// Writes the (relocated) key/value words back, starting at `start` — the offset returned
    /// by [`Dict::gather_roots`].
    ///
    /// Defensive and total: it never panics. A `roots` slice too short to cover the whole
    /// container, or a `start` past its end, simply leaves the uncovered tail untouched
    /// (in practice the collector preserves the slice and the offsets are exact).
    pub fn scatter_roots(&mut self, roots: &[Eterm], start: usize) {
        let Some(available) = roots.len().checked_sub(start) else {
            // `start` is past the end of the slice: nothing to write, no panic.
            return;
        };
        let count = (available / 2).min(self.entries.len());
        for (index, entry) in self.entries.iter_mut().take(count).enumerate() {
            entry.0 = roots[start + 2 * index];
            entry.1 = roots[start + 2 * index + 1];
        }
    }
}

impl Default for Dict {
    fn default() -> Dict {
        Dict::new()
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the dictionary's put/get/erase/get_keys logic and the
    //! `gather_roots`/`scatter_roots` round-trip.

    use super::*;
    use beam_term::prelude::{make_atom, make_small, AtomTable, Heap, NIL};
    use std::rc::Rc;

    #[test]
    fn a_boxed_key_matches_a_distinct_but_equal_term() {
        // `erlang:put({a},1), erlang:get({a})` -> `1`: the two `{a}` tuples are two distinct
        // heap words, so a raw-word key comparison would report the key as absent.
        let mut atoms = AtomTable::new();
        let a = make_atom(atoms.intern("a"));
        let mut heap = Heap::new(64);
        let stored = heap.tuple(&[a]).expect("the key fits");
        let looked_up = heap.tuple(&[a]).expect("the second key fits");
        assert_ne!(stored, looked_up, "the two tuples are distinct heap words");

        let mut dict = Dict::with_atoms(Rc::new(atoms));
        assert_eq!(dict.put(stored, make_small(1)), None);
        assert_eq!(dict.get(looked_up), Some(make_small(1)));
        assert_eq!(dict.len(), 1);

        // Erasing by the *other* equal term removes the pair.
        assert_eq!(dict.erase(looked_up), Some(make_small(1)));
        assert!(dict.is_empty());
    }

    #[test]
    fn a_nested_boxed_key_matches() {
        // A key whose value is a composite: `{[1], {a, 2}}`.
        let mut atoms = AtomTable::new();
        let a = make_atom(atoms.intern("a"));
        let mut heap = Heap::new(64);
        let build = |heap: &mut Heap| {
            let list = heap.cons(make_small(1), NIL).expect("the list fits");
            let inner = heap.tuple(&[a, make_small(2)]).expect("the inner tuple fits");
            heap.tuple(&[list, inner]).expect("the outer tuple fits")
        };
        let stored = build(&mut heap);
        let looked_up = build(&mut heap);
        assert_ne!(stored, looked_up);

        let mut dict = Dict::new();
        dict.put(stored, make_small(9));
        assert_eq!(dict.get(looked_up), Some(make_small(9)));
    }

    #[test]
    fn a_different_boxed_term_is_not_the_same_key() {
        let mut atoms = AtomTable::new();
        let a = make_atom(atoms.intern("a"));
        let mut heap = Heap::new(64);
        let one = heap.tuple(&[a, make_small(1)]).expect("the key fits");
        let two = heap.tuple(&[a, make_small(2)]).expect("the other key fits");
        let other_name = {
            let b = make_atom(atoms.intern("b"));
            heap.tuple(&[b, make_small(1)]).expect("the third key fits")
        };

        let mut dict = Dict::new();
        dict.put(one, make_small(10));
        assert_eq!(dict.get(two), None, "a different element is a different key");
        assert_eq!(dict.get(other_name), None, "a different atom differs");
        // Both distinct keys coexist: the failed lookups did not append anything.
        assert_eq!(dict.len(), 1);
    }

    #[test]
    fn a_boxed_value_is_found_by_get_keys_for_value() {
        // `erlang:get_keys({v})` matches by *term*, not by word: the stored and the searched
        // `{v}` tuples are distinct heap words.
        let mut atoms = AtomTable::new();
        let v = make_atom(atoms.intern("v"));
        let mut heap = Heap::new(64);
        let stored = heap.tuple(&[v]).expect("the value fits");
        let searched = heap.tuple(&[v]).expect("the search value fits");
        assert_ne!(stored, searched);

        let mut dict = Dict::new();
        dict.put(make_small(1), stored);
        dict.put(make_small(2), stored);
        dict.put(make_small(3), make_small(0));
        assert_eq!(
            dict.get_keys_for_value(searched),
            vec![make_small(1), make_small(2)]
        );
        assert_eq!(dict.get_keys_for_value(make_small(0)), vec![make_small(3)]);
        assert!(dict.get_keys_for_value(make_small(7)).is_empty());
    }

    #[test]
    fn atom_keys_agree_whether_or_not_a_table_is_supplied() {
        // The runtime interns every module and every BIF-built term into ONE append-only atom
        // table, so two atoms with the same name are the same word. Comparing through the
        // shared table (which resolves the name) and through an empty one (which falls back to
        // the raw index — the C's `EQ` for two immediates) therefore agree, and neither ever
        // matches a *different* atom.
        let mut atoms = AtomTable::new();
        let a = make_atom(atoms.intern("a"));
        let b = make_atom(atoms.intern("b"));
        let mut heap = Heap::new(64);
        let key = heap.tuple(&[a]).expect("the key fits");
        let equal_key = heap.tuple(&[a]).expect("the equal key fits");
        let other_key = heap.tuple(&[b]).expect("the other key fits");

        for mut dict in [Dict::new(), Dict::with_atoms(Rc::new(atoms.clone()))] {
            dict.put(key, make_small(1));
            assert_eq!(dict.get(equal_key), Some(make_small(1)), "same atom index");
            assert_eq!(dict.get(other_key), None, "a different atom is a different key");
        }
    }

    #[test]
    fn put_get_erase_round_trip() {
        let mut dict = Dict::new();
        assert!(dict.is_empty());
        assert_eq!(dict.len(), 0);
        assert_eq!(dict.get(make_small(1)), None);

        // First put of a key returns `None` (it was absent).
        assert_eq!(dict.put(make_small(1), make_small(10)), None);
        assert_eq!(dict.get(make_small(1)), Some(make_small(10)));
        assert_eq!(dict.len(), 1);

        // Overwriting returns the OLD value and preserves the slot.
        assert_eq!(dict.put(make_small(1), make_small(11)), Some(make_small(10)));
        assert_eq!(dict.get(make_small(1)), Some(make_small(11)));
        assert_eq!(dict.len(), 1, "overwrite does not grow the dictionary");

        // Erase returns the stored value; a second erase is absent.
        assert_eq!(dict.erase(make_small(1)), Some(make_small(11)));
        assert_eq!(dict.erase(make_small(1)), None);
        assert!(dict.is_empty());
    }

    #[test]
    fn replace_in_place_keeps_insertion_order() {
        let mut dict = Dict::new();
        dict.put(make_small(1), make_small(10));
        dict.put(make_small(2), make_small(20));
        // Overwrite key 1: last put wins, but key 1 stays first in order.
        dict.put(make_small(1), make_small(99));
        assert_eq!(dict.get_keys(), vec![make_small(1), make_small(2)]);
        assert_eq!(dict.get(make_small(1)), Some(make_small(99)));
    }

    #[test]
    fn get_keys_and_get_keys_for_value() {
        let mut dict = Dict::new();
        dict.put(make_small(1), make_small(100));
        dict.put(make_small(2), make_small(100));
        dict.put(make_small(3), make_small(200));

        assert_eq!(
            dict.get_keys(),
            vec![make_small(1), make_small(2), make_small(3)]
        );
        assert_eq!(
            dict.get_keys_for_value(make_small(100)),
            vec![make_small(1), make_small(2)]
        );
        assert_eq!(
            dict.get_keys_for_value(make_small(200)),
            vec![make_small(3)]
        );
        assert!(dict.get_keys_for_value(make_small(7)).is_empty());
    }

    #[test]
    fn erase_all_returns_the_pairs_and_empties_the_dict() {
        let mut dict = Dict::new();
        dict.put(make_small(1), make_small(10));
        dict.put(make_small(2), make_small(20));
        let removed = dict.erase_all();
        assert_eq!(
            removed,
            vec![(make_small(1), make_small(10)), (make_small(2), make_small(20))]
        );
        assert!(dict.is_empty());
        assert_eq!(dict.erase_all(), Vec::new());
    }

    #[test]
    fn entries_iterates_in_order() {
        let mut dict = Dict::new();
        dict.put(make_small(1), make_small(10));
        dict.put(make_small(2), make_small(20));
        let collected: Vec<_> = dict.entries().collect();
        assert_eq!(
            collected,
            vec![(make_small(1), make_small(10)), (make_small(2), make_small(20))]
        );
    }

    #[test]
    fn gather_and_scatter_round_trip() {
        let mut dict = Dict::new();
        dict.put(make_small(1), make_small(10));
        dict.put(make_small(2), make_small(20));

        let mut roots = vec![make_small(-1)]; // caller-owned prefix is preserved
        let start = dict.gather_roots(&mut roots);
        assert_eq!(start, 1);
        assert_eq!(
            &roots[start..],
            &[make_small(1), make_small(10), make_small(2), make_small(20)]
        );

        // Simulate the collector relocating every root.
        roots[start..].copy_from_slice(&[
            make_small(101),
            make_small(110),
            make_small(202),
            make_small(220),
        ]);
        dict.scatter_roots(&roots, start);
        assert_eq!(dict.get(make_small(101)), Some(make_small(110)));
        assert_eq!(dict.get(make_small(202)), Some(make_small(220)));
        assert_eq!(dict.len(), 2);
    }

    #[test]
    fn gather_of_an_empty_dict_returns_the_current_len() {
        let dict = Dict::new();
        let mut roots = vec![make_small(7)];
        let start = dict.gather_roots(&mut roots);
        assert_eq!(start, 1);
        assert_eq!(roots, vec![make_small(7)]);
        // Scattering nothing is a no-op and never panics.
        let mut dict = dict;
        dict.scatter_roots(&roots, start);
        assert!(dict.is_empty());
    }

    #[test]
    fn scatter_is_total_for_a_mismatched_slice() {
        let mut dict = Dict::new();
        dict.put(make_small(1), make_small(10));
        dict.put(make_small(2), make_small(20));

        // A slice that covers only the first pair: the second stays untouched, no panic.
        let short = [make_small(101), make_small(110)];
        dict.scatter_roots(&short, 0);
        assert_eq!(dict.get(make_small(101)), Some(make_small(110)));
        assert_eq!(dict.get(make_small(2)), Some(make_small(20)));

        // A `start` past the end is likewise a no-op.
        dict.scatter_roots(&short, 99);
        assert_eq!(dict.get(make_small(101)), Some(make_small(110)));
        assert_eq!(dict.get(make_small(2)), Some(make_small(20)));
    }
}
