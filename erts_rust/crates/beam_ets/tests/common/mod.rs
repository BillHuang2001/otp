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

//! Shared helpers for the `beam_ets` integration tests.
//!
//! This module is compiled separately into every integration-test binary, so an
//! unused helper would otherwise trip the `dead_code` lint in that binary. The
//! `allow` below keeps the build warning-free.

#![allow(dead_code)]

pub mod oracle;

use beam_ets::{Table, TableId, TableKind, TableOptions};
use beam_term::prelude::*;

/// A `{Key, Value}` ETS object on `heap` (keypos 1).
pub fn object(heap: &mut Heap, key: i64, value: i64) -> Eterm {
    heap.tuple(&[make_small(key), make_small(value)])
        .expect("test heap has room")
}

/// An empty table of `kind`.
pub fn new_table(kind: TableKind) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(kind);
    Table::new(TableId::next(), opts).expect("a fresh table is constructible")
}

/// A heap-allocated flonum term (`[HEADER_FLONUM, bits]`). `beam_term` has no
/// heap-based float constructor, so the two words are leaked for the test's life.
pub fn flonum(value: f64) -> Eterm {
    let words: Box<[Eterm; 2]> =
        Box::new([HEADER_FLONUM, Eterm::from_raw(value.to_bits() as usize)]);
    let leaked: &'static [Eterm; 2] = Box::leak(words);
    make_boxed(leaked.as_ptr())
}

/// The term of the atom `name`, interned in `atoms`.
pub fn interned_atom(atoms: &mut AtomTable, name: &str) -> Eterm {
    make_atom(atoms.intern(name))
}

/// A proper list of `elements` on `heap`.
pub fn proper_list(heap: &mut Heap, elements: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for element in elements.iter().rev() {
        acc = heap.cons(*element, acc).expect("test heap has room");
    }
    acc
}

/// The value of the `field` atom within an `ets:info/1` entry list.
pub fn info_entry(entries: &[(Atom, Eterm)], atoms: &AtomTable, field: &str) -> Option<Eterm> {
    let want = atoms.lookup(field)?;
    entries.iter().find(|(a, _)| *a == want).map(|(_, v)| *v)
}

/// The bump heap the `ets:update_*` operations rebuild objects on (a
/// `beam_alloc::Heap`, distinct from the `beam_term::heap::Heap` test arena the
/// *source* terms are built in).
pub fn rebuild_heap() -> beam_alloc::Heap {
    rebuild_heap_words(1024)
}

/// A rebuild heap with room for `words` term words — use it when a test performs
/// many `ets:update_*` calls: the interiors' default rebuild-on-heap path allocates
/// a fresh object (3 words for a `{Key, Value}` tuple) per call and the heap never
/// reallocates, so the default 1024 words is not enough for thousands of updates.
pub fn rebuild_heap_words(words: usize) -> beam_alloc::Heap {
    beam_alloc::Heap::new(words).expect("a test heap is allocatable")
}

/// A tuple object on `heap` built from `elements`.
pub fn tuple_of(heap: &mut Heap, elements: &[Eterm]) -> Eterm {
    heap.tuple(elements).expect("test heap has room")
}

/// A tuple object `{a, b, ...}` of small integers on `heap`.
pub fn int_tuple(heap: &mut Heap, elements: &[i64]) -> Eterm {
    let terms: Vec<Eterm> = elements.iter().copied().map(make_small).collect();
    tuple_of(heap, &terms)
}

/// A bignum term (`write_big`), leaked for the test's life.
///
/// `beam_term::heap::Heap` has no bignum constructor, so the `[header, digit]`
/// block is leaked and `write_big` fills it in.
pub fn bignum_term() -> Eterm {
    let words: Box<[Eterm; 2]> = Box::new([Eterm::from_raw(0), Eterm::from_raw(0)]);
    let leaked: &'static mut [Eterm; 2] = Box::leak(words);
    // SAFETY: `leaked` reserves `BIG_NEED_SIZE(1)` == 2 words (header + one
    // `ErtsDigit`), exactly what `write_big` writes for a one-digit magnitude, and
    // the digit slice is non-empty as `write_big` requires.
    unsafe { write_big(leaked.as_mut_ptr(), false, &[1u64 << 60]) }
}

/// A bignum of `digits` (least significant first, non-empty) built on `heap`.
///
/// The counter tests use it to name the *exact* expected result of an
/// arbitrary-precision sum, independent of the small/bignum encoding: two bignums
/// with the same sign and digits are `=:=` (`beam_term::compare::cmp` compares
/// their limbs).
pub fn big(heap: &mut Heap, negative: bool, digits: &[u64]) -> Eterm {
    // SAFETY: `alloc_words` hands out exactly `big_size_words(n)` uninitialised
    // words, which `write_big` fills completely.
    let hp = unsafe { heap.alloc_words(big_size_words(digits.len())).expect("test heap has room") };
    // SAFETY: `hp` addresses `big_size_words(len)` words and `digits` is non-empty.
    unsafe { write_big(hp, negative, digits) }
}
