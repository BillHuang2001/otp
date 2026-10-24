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

//! The fixtures behind `tests/region_relocation.rs`: a **foreign** word region the test owns, the
//! readers that walk a relocated graph back, and the two-region grid every case is built on.
//!
//! Kept in a child module so the acceptance driver stays under the crate's ~1000-line file target;
//! it holds no assertions of its own beyond the fixture's own invariants. The region shape here is
//! the one a host really has — a pre-reserved `Vec<Eterm>` and a `beam_alloc::ReservedHeap` (the
//! engine arena's shape) — handed to the copier only as `(base address, used words)`, which is
//! exactly `FromSet::from_ranges`'s argument, so nothing in this module assumes a process heap.

use beam_alloc::ReservedHeap;
use beam_gc::prelude::FromSet;
use beam_term::prelude::{
    boxed_val, flatmap_size_words, fun_words, header_arity, is_header, is_list, list_val,
    make_arityval, make_boxed, make_list, make_small, write_fun_local, write_map, AtomTable, Eterm,
    FunKind, NIL,};

/// The two backings a host region really has: a pre-reserved `Vec<Eterm>` and a
/// `beam_alloc::ReservedHeap` (the engine arena's shape). Both are handed to the copier the
/// same way — as `(base address, used words)`.
pub enum Backing {
    Vec(Vec<Eterm>),
    Reserved(ReservedHeap),
}

/// One **foreign** word area: the `FromSet::from_ranges`/`LinearDest` argument shape.
///
/// Every word starts as an immediate, so even a mistaken read of an unwritten word can never
/// dereference anything (the guard the crate's own fixtures use).
pub struct Region {
    backing: Backing,
    /// The live prefix in words — what the host filled, and therefore exactly the extent the
    /// from-range claims.
    used: usize,
}

impl Region {
    /// A `capacity`-word `Vec<Eterm>`-backed region.
    pub fn vec(capacity: usize) -> Region {
        Region {
            backing: Backing::Vec(vec![make_small(0); capacity]),
            used: 0,
        }
    }

    /// A `capacity`-word `ReservedHeap`-backed region (reservation and commitment both
    /// `capacity`), pre-filled with immediates so its whole extent is initialised.
    pub fn reserved(capacity: usize) -> Region {
        let heap = ReservedHeap::new(capacity, capacity)
            .expect("the default allocator satisfies the reservation");
        let region = Region {
            backing: Backing::Reserved(heap),
            used: 0,
        };
        // SAFETY: the reservation is `capacity` exclusively-owned, writable words that this
        // region owns for its whole lifetime; writing an immediate over every one of them
        // means a mistaken read of an unused word can never dereference anything.
        unsafe {
            for offset in 0..capacity {
                *region.base().add(offset) = make_small(0);
            }
        }
        region
    }

    /// The first word of the region.
    pub fn base(&self) -> *mut Eterm {
        match &self.backing {
            Backing::Vec(words) => words.as_ptr() as *mut Eterm,
            Backing::Reserved(heap) => heap.base(),
        }
    }

    /// The region's size in term words.
    pub fn capacity_words(&self) -> usize {
        match &self.backing {
            Backing::Vec(words) => words.len(),
            Backing::Reserved(heap) => heap.reserved_words(),
        }
    }

    /// The live prefix in words.
    pub fn used(&self) -> usize {
        self.used
    }

    /// Reserves `n` words at the bump pointer (the host's own allocation step) and returns
    /// their address; the live prefix therefore stays one contiguous run.
    pub fn alloc(&mut self, n: usize) -> *mut Eterm {
        assert!(
            self.used + n <= self.capacity_words(),
            "the fixture's region of {} words is big enough for {n} more",
            self.capacity_words()
        );
        let at = self.base().wrapping_add(self.used);
        self.used += n;
        at
    }

    /// Word `offset` of the region (inside the allocation, written or not).
    pub fn word(&self, offset: usize) -> Eterm {
        assert!(offset < self.capacity_words(), "offset {offset} is in bounds");
        // SAFETY: `offset` is inside this region's own allocation, whose every word was
        // initialised at construction and is written only through this region's pointers.
        unsafe { *self.base().add(offset) }
    }

    /// The whole word run, for byte-identity assertions.
    pub fn snapshot(&self) -> Vec<Eterm> {
        (0..self.capacity_words()).map(|o| self.word(o)).collect()
    }

    /// The list `[values…]` as one chain of cons cells; returns the head term.
    pub fn cons_list(&mut self, values: &[i64]) -> Eterm {
        let mut tail = NIL;
        for &value in values.iter().rev() {
            let at = self.alloc(2);
            // SAFETY: `at` is a freshly reserved, exclusively-owned 2-word cons cell of this
            // region; both of its words are written before the term escapes.
            unsafe {
                *at = make_small(value);
                *at.add(1) = tail;
            }
            tail = make_list(at as *const Eterm);
        }
        tail
    }

    /// The tuple `{values…}`; returns its BOXED term.
    pub fn tuple(&mut self, values: &[Eterm]) -> Eterm {
        let at = self.alloc(values.len() + 1);
        // SAFETY: `at` is a freshly reserved tuple block of `len + 1` exclusively-owned words;
        // the header and every element are written before the term escapes.
        unsafe {
            *at = make_arityval(values.len());
            for (index, &value) in values.iter().enumerate() {
                *at.add(index + 1) = value;
            }
        }
        make_boxed(at as *const Eterm)
    }

    /// The flat map over `pairs`; returns its BOXED root (the head word, *after* the
    /// back-pointing keys tuple of the same block).
    pub fn flat_map(&mut self, pairs: &[(Eterm, Eterm)]) -> Eterm {
        let at = self.alloc(flatmap_size_words(pairs.len()));
        // SAFETY: `flatmap_size_words(len)` is exactly the block `write_map` writes for a flat
        // map (at most `MAP_SMALL_MAP_LIMIT` pairs) and every pair is a live term of this
        // test's graph.
        unsafe { write_map(at, pairs, &AtomTable::new()) }.expect("a flat map of live terms")
    }

    /// The local fun `[header][uniq][index][env…]`; returns its BOXED term.
    pub fn local_fun(&mut self, arity: usize, uniq: u64, index: u32, env: &[Eterm]) -> Eterm {
        let at = self.alloc(fun_words(FunKind::Local, env.len()));
        // SAFETY: `fun_words(Local, num_free)` is exactly the block `write_fun_local` writes,
        // and every env entry is a live term (or an immediate).
        unsafe { write_fun_local(at, arity, uniq, index, env) }.expect("a local fun")
    }
}

/// The [`FromSet`] over the live prefixes of `regions` — the host's own assertion that these
/// are real, disjoint, filled word areas.
pub fn from_set(regions: &[&Region]) -> FromSet {
    let ranges: Vec<(usize, usize)> = regions
        .iter()
        .map(|region| (region.base() as usize, region.used()))
        .collect();
    // SAFETY: every entry is this test's own region — word-aligned (`Vec<Eterm>` / a
    // `ReservedHeap` reservation), readable and writable for as long as the set is alive, a
    // real filled prefix (`used`), pairwise disjoint (distinct, simultaneously live
    // allocations) and untouched by any other thread. Completeness (no live term outside the
    // ranges the caller cares about) is the assertion the roots below are built from.
    unsafe { FromSet::from_ranges(&ranges) }
}

/// The header word of a live boxed term of a graph the test owns.
pub fn header(term: Eterm) -> Eterm {
    // SAFETY: `term` is a live boxed term whose block the owning region keeps unmoved.
    unsafe { *boxed_val(term) }
}

/// The element words of a live tuple.
pub fn tuple_elements(term: Eterm) -> Vec<Eterm> {
    let hdr = header(term);
    assert!(is_header(hdr), "the fixture builds tuples");
    let arity = header_arity(hdr);
    let base = boxed_val(term);
    let mut out = Vec::with_capacity(arity);
    for index in 1..=arity {
        // SAFETY: `term` heads a live tuple of `arity` elements, so word `index` is in bounds.
        out.push(unsafe { *base.add(index) });
    }
    out
}

/// The car words of a live, proper cons list (`[NIL]` for the empty list).
pub fn list_cells(head: Eterm) -> Vec<Eterm> {
    let mut out = Vec::new();
    let mut cell = head;
    while is_list(cell) {
        assert!(out.len() < 64, "the fixture's list is short and acyclic");
        let base = list_val(cell);
        // SAFETY: `cell` is a live cons cell of the fixture's own graph, so both of its words
        // are in bounds; the car is an immediate and the cdr is the next cell or `NIL`.
        let (car, cdr) = unsafe { (*base, *base.add(1)) };
        out.push(car);
        cell = cdr;
    }
    assert_eq!(cell, NIL, "the fixture builds proper lists");
    out
}

/// The words of the destination run `[0, used)`, in reservation order.
pub fn copied_run(target: &Region, used: usize) -> Vec<Eterm> {
    (0..used).map(|offset| target.word(offset)).collect()
}

/// Distinct sentinel immediates carried by the two **dead** objects, so "was a dead object
/// copied" is answerable by word comparison alone.
pub const DEAD_A: i64 = 0x5EED_0001;
pub const DEAD_B: i64 = 0x5EED_0002;
pub const DEAD_C: i64 = 0x5EED_0003;

/// The grid's exact live geometry, pinned so a change to the canonical per-object layout (or
/// to `move_size`) shows up as a count rather than as a silent shift:
///
/// `LIVE_WORDS` = the list (2 cells × 2 = 4) + `tup` (arity 2 → 3) + `tuple` (arity 2 → 3) +
/// the fun block (`1 + 2` descriptor + `2` env = 5) + the map head (size 1 → `1 + 3` = 4) +
/// the map's keys tuple (`arityval(1)` + 1 key = 2) = **21**.
///
/// `OBJECTS` = 2 cons cells + `tup`, `tuple`, the fun, the map head and the keys tuple = **7**.
///
/// `DEAD_WORDS` = the two unreferenced 3-word tuples = **6**.
pub const LIVE_WORDS: usize = 21;
pub const OBJECTS: usize = 7;
pub const DEAD_WORDS: usize = 6;

/// The two-region fixture: region `b` (`Vec`-backed) holds the leaves, region `a`
/// (`ReservedHeap`-backed) holds objects that reference them — so the graph crosses the region
/// boundary in the direction the copier has to follow — and each region also holds one
/// unreachable object.
pub struct Grid {
    /// Region A: `tuple`, `fun`, `map`, one dead tuple.
    pub a: Region,
    /// Region B: `list`, `tup`, one dead tuple.
    pub b: Region,
    /// The three roots: the cross-region tuple, the fun, the map.
    pub roots: [Eterm; 3],
}

/// Builds the grid. Deterministic, so two calls produce structurally identical graphs — which
/// is what lets a read-only account be compared against a destructive one.
pub fn build_grid() -> Grid {
    let mut b = Region::vec(64);
    let list = b.cons_list(&[10, 20]);
    let tup = b.tuple(&[make_small(30), make_small(40)]);
    let _dead_b = b.tuple(&[make_small(DEAD_B), make_small(DEAD_C)]);

    let mut a = Region::reserved(64);
    let tuple = a.tuple(&[list, tup]);
    let fun = a.local_fun(2, 0xDEAD_BEEF, 11, &[tup, make_small(5)]);
    let map = a.flat_map(&[(make_small(1), tup)]);
    let _dead_a = a.tuple(&[make_small(DEAD_A), make_small(DEAD_A)]);

    Grid {
        a,
        b,
        roots: [tuple, fun, map],
    }
}
