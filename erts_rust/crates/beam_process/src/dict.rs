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
//! # Key equality (documented Milestone-1 limitation)
//!
//! Keys are compared by **raw `Eterm` word equality**. That is exact for every immediate —
//! small integers, atoms, local pids/ports, `NIL` — which is what process dictionaries hold
//! in practice, and for the *same* boxed object. Structural equality of two distinct but
//! equal *boxed* terms (e.g. two separately built `{a,b}` tuples) needs
//! [`beam_term::compare::eq`], which takes an [`beam_term::AtomTable`]; this crate keeps
//! [`Dict`] term-agnostic and atom-table-free by contract, so that case is a documented
//! follow-up rather than a hidden dependency.

use beam_term::prelude::Eterm;

/// The per-process dictionary: an ordered set of `(Key, Value)` pairs.
///
/// The pairs are kept in **insertion order**, with `put` on an existing key replacing the
/// value in place (last put wins) so the order is stable across overwrites — matching the C,
/// which reuses the slot of an existing key. See the module docs for the semantics and the
/// term-ownership contract.
pub struct Dict {
    entries: Vec<(Eterm, Eterm)>,
}

impl Dict {
    /// An empty dictionary.
    pub fn new() -> Dict {
        Dict {
            entries: Vec::new(),
        }
    }

    /// The index of the pair whose key raw-equals `key`, if any.
    fn find(&self, key: Eterm) -> Option<usize> {
        self.entries.iter().position(|&(stored, _)| stored == key)
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

    /// Every key whose value raw-equals `value`, in insertion order (`erlang:get_keys/1`).
    pub fn get_keys_for_value(&self, value: Eterm) -> Vec<Eterm> {
        self.entries
            .iter()
            .filter(|&&(_, stored)| stored == value)
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
    use beam_term::prelude::make_small;

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
