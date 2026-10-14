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

//! The `term_to_binary/2` `deterministic` map path (`DFLAG_DETERMINISTIC`).
//!
//! With the option, the C encoder collects **all** of a map's `(key, value)` pairs
//! into an array, sorts it by the key with `map_key_compare`
//! (`external.c:3217-3227`, `CMP_TERM` = `erts_cmp(a, b, exact=1, eq_only=0)`) and
//! emits `K1,V1,…,Kn,Vn` in that order — for a ≤ 32-pair **flat** map
//! (`external.c:3757-3764`) and for a > 32-pair **hash** map / HAMT
//! (`external.c:3783-3807`) alike. This is what makes the image independent of the
//! map's internal storage order.
//!
//! This module owns that one responsibility: the shared sort + emit used by both
//! emitters in the parent module. It is safe Rust.
//!
//! `erts_cmp` in exact mode is reproduced by [`beam_term::compare::cmp`] (the
//! lowest-common-ancestor ordering), and the key is the **only** sort field, so a
//! map's values never influence the order. Two `=:=`-distinct keys can never
//! compare `Equal`, so the sort is deterministic.

use beam_common::Result;
use beam_term::compare;
use beam_term::{AtomTable, Eterm};

use super::Encoder;

/// Sorts `(key, value)` pairs in place by their **key** in full Erlang term order.
///
/// This is the Rust form of the C `qsort(map_array, n/2, 2*sizeof(Eterm),
/// map_key_compare)` behind `DFLAG_DETERMINISTIC` (`external.c:3444-3462`). The
/// comparison is the exact `=:=` order (`erts_cmp(a, b, 1, 0)`), so
/// `beam_term::compare::cmp` is used rather than a raw-word or name comparison.
/// A pair of `=:=`-distinct keys never compares `Equal`; should the (impossible)
/// tie arise the sort stays stable and well-defined.
pub(crate) fn sort_pairs_by_key(pairs: &mut [(Eterm, Eterm)], atoms: &AtomTable) {
    pairs.sort_by(|a, b| compare::cmp(a.0, b.0, atoms));
}

impl<'a> Encoder<'a> {
    /// Encodes `pairs` as a `MAP_EXT` with the keys sorted in full term order — the
    /// shared `deterministic` emission for both the flat-map and HAMT paths.
    ///
    /// `pairs` is copied (the caller's slice may be a live term's storage) and the
    /// copy sorted; the pair count must fit the `arityval` 24-bit field, else
    /// `SystemLimit` (checked by [`Encoder::encode_map_header`]).
    pub(super) fn encode_pairs_sorted_by_key(&mut self, pairs: &[(Eterm, Eterm)]) -> Result<()> {
        let mut sorted = pairs.to_vec();
        sort_pairs_by_key(&mut sorted, self.atoms);
        self.encode_map_header(sorted.len())?;
        for (key, value) in &sorted {
            self.encode(*key)?;
            self.encode(*value)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use beam_alloc::Heap;
    use beam_term::prelude::{make_atom, make_small, AtomTable};

    #[test]
    fn sorting_uses_full_term_order_not_the_input_order() {
        let mut atoms = AtomTable::new();
        let a = make_atom(atoms.intern("a"));
        let b = make_atom(atoms.intern("b"));
        let c = make_atom(atoms.intern("c"));
        let mut pairs = vec![(c, make_small(3)), (a, make_small(1)), (b, make_small(2))];
        sort_pairs_by_key(&mut pairs, &atoms);
        assert_eq!(
            pairs,
            vec![(a, make_small(1)), (b, make_small(2)), (c, make_small(3))]
        );
    }

    #[test]
    fn sorting_integers_is_numeric_ascending() {
        let atoms = AtomTable::new();
        let mut pairs = vec![
            (make_small(10), make_small(0)),
            (make_small(-1), make_small(0)),
            (make_small(2), make_small(0)),
        ];
        sort_pairs_by_key(&mut pairs, &atoms);
        assert_eq!(
            pairs.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            vec![make_small(-1), make_small(2), make_small(10)]
        );
    }

    #[test]
    fn an_empty_pair_list_sorts_trivially() {
        let atoms = AtomTable::new();
        let mut pairs: Vec<(Eterm, Eterm)> = Vec::new();
        sort_pairs_by_key(&mut pairs, &atoms);
        assert!(pairs.is_empty());
    }

    #[test]
    fn the_value_does_not_influence_the_order() {
        // The comparator reads only the key, so equal keys keep their relative order
        // (a stable no-op) and the values never move a pair.
        let mut atoms = AtomTable::new();
        let k = make_atom(atoms.intern("k"));
        let mut pairs = vec![(k, make_small(1)), (k, make_small(2))];
        sort_pairs_by_key(&mut pairs, &atoms);
        assert_eq!(pairs, vec![(k, make_small(1)), (k, make_small(2))]);
    }

    #[test]
    fn a_heap_is_unused_by_the_sort() {
        // `sort_pairs_by_key` allocates nothing from a heap; this pins that the helper
        // is pure over its slices (a term's validity is the caller's invariant).
        let mut atoms = AtomTable::new();
        let _heap = Heap::new(16).expect("test heap");
        let a = make_atom(atoms.intern("a"));
        let mut pairs = vec![(a, a)];
        sort_pairs_by_key(&mut pairs, &atoms);
        assert_eq!(pairs, vec![(a, a)]);
    }
}
