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

//! The `MAP_SUBTAG` arm of `erts_cmp_compound` — map comparison.
//!
//! Split out of [`super`] so the flat-map fast path and its document
//! ("no `Vec`, no sort, and exactly which producers store a sorted keys-tuple")
//! live under their own roof.
//!
//! Erlang map order is: **size first**, then the key/value pairs ordered by the
//! *flat-map key order* ([`cmp_flatmap_keys`]). The C compares the whole key
//! sequence before the value sequence (it pushes the value comparisons first and
//! the key comparisons last on its work stack, so keys are popped — and therefore
//! compared — first), which is exactly a lexicographic comparison of
//! `(key-list, value-list)`; both paths below reproduce that literally.
//!
//! # Two paths, one result
//!
//! * **Flat map vs flat map** — a flat map stores its keys inline and, in every
//!   producer that sorts, already in [`cmp_flatmap_keys`] order, so both operands
//!   can be compared straight out of their storage
//!   ([`crate::thing::flatmap_parts`]): no pair list is built, nothing is sorted,
//!   **nothing is allocated**.
//! * **Anything else** — the collecting fallback: both operands' pairs are
//!   gathered and sorted in [`cmp_flatmap_keys`] order by
//!   [`crate::map::map_pairs_flatmap_order`], which is the *definition* of the
//!   order a flat map's keys-tuple holds. The fast path is taken only when its
//!   precondition (keys non-descending in that order) holds, so the two paths
//!   always compare the same two key/value sequences and can never disagree.
//!
//! ## Which producers store a sorted keys-tuple
//!
//! `crate::thing::write_flatmap` **preserves the pair order it is handed** and does
//! not sort, so "sortedness" is a property of each caller. Verified in this tree:
//!
//! * `crate::map::write_map` — the map constructor; sorts a flat map in
//!   [`cmp_flatmap_keys`] order. Every in-tree *canonical* producer reaches one of
//!   these: `beam_codec::term::make_map` (the `MAP_EXT` decoder path,
//!   `beam_codec/src/decode/mod.rs`), `beam_engine::terms::{make_map, make_map_rooted}`
//!   (the `put_map_assoc`/`put_map_exact` families, `families/maps/write.rs`),
//!   `beam_bifs::access::make_map`, and `crate::map::{map_put, map_remove}`.
//! * `beam_codec::term::make_flatmap` — deliberately **order-preserving**: it exists
//!   to re-emit a *stored* pair order byte-exactly, and its inputs are already-sorted
//!   maps or a caller-declared image order (`make_map` is the decoder's path).
//! * `beam_bifs::access::make_flatmap` — order-preserving; its in-tree producers are
//!   hand-built option/proplist maps (`beam_bifs/src/{records,persistent_term,
//!   runtime/atomics}.rs`), whose key order is whatever the BIF interns, i.e. it is
//!   **not** guaranteed to be atom-index order.
//!
//! The last group is exactly why the precondition is *checked* rather than assumed:
//! a hand-built flat map whose keys-tuple is out of order still compares by the same
//! rule (it declines to the collecting fallback), and the check costs one
//! [`cmp_flatmap_keys`] per adjacent key pair — O(n) with no allocation, still far
//! cheaper than building and sorting two pair lists.

use core::cmp::Ordering;

use super::{cmp_flatmap_keys, cmp_impl, fallback_cmp};
use crate::atom::AtomTable;
use crate::boxed::{boxed_val, is_boxed_term, is_flatmap_header};
use crate::eterm::Eterm;
use crate::thing::flatmap_parts;

/// Compare two maps — the `MAP_SUBTAG` arm of `erts_cmp_compound`
/// (`erts/emulator/beam/utils.c:1955-2118`).
///
/// Both operands are, by the caller invariant, well-formed maps. A malformed map
/// (only reachable if the term was forged outside this crate) falls back to the
/// deterministic [`super::fallback_cmp`] rather than panicking.
pub(super) fn cmp_maps(a: Eterm, b: Eterm, atoms: &AtomTable, exact: bool) -> Ordering {
    if let (Some((ka, va)), Some((kb, vb))) = (
        sorted_flatmap_parts(a, atoms),
        sorted_flatmap_parts(b, atoms),
    ) {
        return cmp_sorted_flatmaps(ka, va, kb, vb, atoms, exact);
    }

    let (pa, pb) = match (
        crate::map::map_pairs_flatmap_order(a, atoms),
        crate::map::map_pairs_flatmap_order(b, atoms),
    ) {
        (Ok(pa), Ok(pb)) => (pa, pb),
        _ => return fallback_cmp(a, b),
    };
    if pa.len() != pb.len() {
        return pa.len().cmp(&pb.len());
    }
    for i in 0..pa.len() {
        let ord = cmp_flatmap_keys(pa[i].0, pb[i].0, atoms);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    for i in 0..pa.len() {
        // The value terms of two live, well-formed maps.
        let ord = cmp_impl(pa[i].1, pb[i].1, atoms, exact);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Compare two flat maps read directly out of their storage.
///
/// `ka`/`va` and `kb`/`vb` are the keys-tuples and value sequences of two flat maps
/// whose keys are already non-descending in [`cmp_flatmap_keys`] order, so this is
/// exactly what [`cmp_maps`]'s collecting fallback computes: size first, then the
/// key sequence, then the value sequence.
fn cmp_sorted_flatmaps(
    ka: &[Eterm],
    va: &[Eterm],
    kb: &[Eterm],
    vb: &[Eterm],
    atoms: &AtomTable,
    exact: bool,
) -> Ordering {
    if ka.len() != kb.len() {
        return ka.len().cmp(&kb.len());
    }
    for i in 0..ka.len() {
        let ord = cmp_flatmap_keys(ka[i], kb[i], atoms);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    for i in 0..va.len() {
        // The value terms of two live, well-formed flat maps.
        let ord = cmp_impl(va[i], vb[i], atoms, exact);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// The `(keys, values)` of a live **flat** map whose keys are already in
/// [`cmp_flatmap_keys`] order, borrowed from its storage; `None` otherwise.
///
/// See the module docs for which producers store a sorted keys-tuple and why the
/// order is *checked* here rather than assumed (`crate::thing::write_flatmap` itself
/// preserves the order it is handed). An immediate or a non-flat term — including a
/// hash map, which has no keys-tuple — returns `None` without dereferencing anything
/// it has not first classified, so a forged operand can only cost a fallback.
fn sorted_flatmap_parts<'a>(t: Eterm, atoms: &AtomTable) -> Option<(&'a [Eterm], &'a [Eterm])> {
    // An immediate can never be a map; guard before dereferencing.
    if !is_boxed_term(t) {
        return None;
    }
    // SAFETY: `is_boxed_term` established that `t` is a live boxed term, so
    // `boxed_val(t)` is its header word and reading it is in bounds.
    let header = unsafe { *boxed_val(t) };
    if !is_flatmap_header(header) {
        return None;
    }
    // SAFETY: the header says `t` is a live flat map, so the keys-tuple and the value
    // words the slices cover are live and in bounds; the returned slices borrow `t`'s
    // storage, which the caller keeps alive and unmoved for `'a`.
    let (keys, values) = unsafe { flatmap_parts(t) };
    if is_sorted_by_flatmap_keys(keys, atoms) {
        Some((keys, values))
    } else {
        None
    }
}

/// Whether `keys` is non-descending in [`cmp_flatmap_keys`] order.
///
/// Exactly the precondition that makes `sort_by(cmp_flatmap_keys)` the identity on
/// `keys`, which is what lets the fast path and the collecting fallback be compared
/// index by index. O(n) and allocation-free.
fn is_sorted_by_flatmap_keys(keys: &[Eterm], atoms: &AtomTable) -> bool {
    keys.windows(2)
        .all(|w| cmp_flatmap_keys(w[0], w[1], atoms) != Ordering::Greater)
}
