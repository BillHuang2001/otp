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

//! Dependency-free measurement of the copy engine: ns per copied word and per copied object.
//!
//! Run it with output shown:
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message \
//!     --test perf -- --nocapture
//! ```
//!
//! The measurement uses `std::time::Instant` only (no `criterion`, no new dependency). Every test
//! here asserts **correctness only** - a copied graph is `=:=` its original and the word count is
//! exactly the preflighted one - and never asserts a timing threshold, so it can never fail
//! spuriously on a loaded machine. The printed numbers are the deliverable; the assertions are what
//! keep the measurement honest (a fast-but-wrong copier would fail them).
//!
//! What the shapes show:
//!
//! * `small tuple` / `flat map` - small objects, so ns/word is dominated by per-object setup;
//! * `deep list` - a homogeneous chain of cons cells, the shape the copy loop's tag dispatch is
//!   exercised on most;
//! * `HAMT map` - a trie of many small nodes, so both per-object and per-word costs are visible;
//! * `refc binary reuse` - the payload is shared by reference, so the *deep-copied* word count is
//!   just the 8-word on-heap pair (per-iteration cost is essentially the off-heap refcount).

use std::time::{Duration, Instant};

use beam_alloc::{Heap, RefcBinary};
use beam_message::off_heap::OffHeapTable;
use beam_message::{copy_struct, copy_struct_simple, size_of_copy, CopyContext};
use beam_term::prelude::*;
use beam_term::compare;

/// Builds the list `[e0, e1, ...]` on `heap`, tail-first.
fn build_list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for &elem in elems.iter().rev() {
        acc = heap.cons(elem, acc).expect("the cons fits");
    }
    acc
}

fn write_map_term(heap: &mut Heap, pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Eterm {
    // SAFETY: `map_words_capacity` is a sound upper bound on the words `write_map` writes.
    unsafe {
        let hp = heap
            .alloc_words(map_words_capacity(pairs.len()))
            .expect("the map fits");
        write_map(hp, pairs, atoms).expect("the map builds")
    }
}

/// Times `iterations` copies of `term` into a reused destination heap and prints ns/word, ns/object
/// and ns/iteration. Asserts the copy is correct (so the timing measures real work).
///
/// Returns the mean ns/iteration, so a caller can compare two variants of the same shape.
fn measure(name: &str, term: Eterm, objects: usize, iterations: u32) -> f64 {
    measure_into(name, term, objects, iterations, 0)
}

/// Consumes `words` (uninitialised) words of `dst`, making it a **dirty** destination.
///
/// One dummy word is enough to take the copier's preflight path whatever the copier's
/// clean-destination fast path is: the word is never read or written by the copy, it only makes
/// `used() > 0`. That is what makes the `precharge` variant of a measurement the *two-pass*
/// reference point: the difference between it and the clean variant is the Pass-1 (preflight)
/// share.
fn precharge(dst: &mut Heap, words: usize) {
    if words > 0 {
        // SAFETY: the words are inside this heap's own region and are never read; they exist only
        // to advance `used`.
        unsafe { dst.alloc_words(words).expect("the precharge fits") };
    }
}

/// Times `iterations` copies of `term` into a destination pre-charged with `precharge` words,
/// printing ns/word, ns/object, ns/iteration and the best iteration; returns the mean ns/iteration.
///
/// A `precharge` of `0` measures a **clean** destination; every other value measures a dirty one
/// (which is forced through the preflight/sized path). The destination's head-room is `words + 64`
/// in both cases, so the two variants are compared on equal terms.
fn measure_into(
    name: &str,
    term: Eterm,
    objects: usize,
    iterations: u32,
    precharge_words: usize,
) -> f64 {
    let words = size_of_copy(term);
    let atoms = AtomTable::new();
    let total_words = words + precharge_words;

    // Warm up (also validates correctness before the timed loop).
    let mut dst = Heap::new(words + 64 + precharge_words).expect("destination heap");
    precharge(&mut dst, precharge_words);
    let copy = copy_struct_simple(term, &mut dst).expect("the copy succeeds");
    assert!(
        compare::eq(term, copy, &atoms),
        "{name}: the measured copy must be correct"
    );
    assert_eq!(
        dst.used(),
        total_words,
        "{name}: the measured copy writes the sized count"
    );

    let mut total = Duration::ZERO;
    let mut best = Duration::MAX;
    for _ in 0..iterations {
        dst.reset();
        precharge(&mut dst, precharge_words);
        let start = Instant::now();
        let copy = copy_struct_simple(term, &mut dst).expect("the copy succeeds");
        let elapsed = start.elapsed();
        total += elapsed;
        if elapsed < best {
            best = elapsed;
        }
        // Keep the result observable so the copy is not optimised away, and re-check correctness.
        assert!(!is_immediate(copy));
        assert_eq!(dst.used(), total_words);
    }

    let per_iter = total.as_nanos() as f64 / f64::from(iterations);
    let ns_per_word = per_iter / words as f64;
    let ns_per_object = per_iter / objects as f64;
    println!(
        "{name:<40} words={words:<6} ns/iter={per_iter:>10.1} \
         ns/word={ns_per_word:>6.2} ns/object={ns_per_object:>8.1} best_ns={:>9}",
        best.as_nanos()
    );
    per_iter
}

/// Times `iterations` copies of a refc binary through a real copy boundary (so the off-heap handle
/// is resolved and refcounted) and prints the numbers; the deep-copied word count is the 8-word
/// on-heap pair, since the payload is shared by reference.
fn measure_refc(name: &str, iterations: u32) {
    let bytes: Vec<u8> = (0..512usize).map(|i| ((i * 7) % 251) as u8).collect();
    let mut src_off = OffHeapTable::new();
    src_off.push_binary(RefcBinary::new(&bytes).expect("the payload allocates"));
    let data_ptr = src_off.binaries()[0].data_ptr();

    let mut src = Heap::new(64).expect("source heap");
    // SAFETY: 8 words are reserved for the pair; `src_off` owns the payload, which outlives it.
    let term = unsafe {
        let hp = src.alloc_words(ERL_REFC_BITS_SIZE).expect("the pair fits");
        write_refc_binary(
            hp,
            data_ptr as *mut Eterm,
            core::ptr::null_mut(),
            data_ptr,
            0,
            bytes.len() * 8,
        )
    };

    let words = size_of_copy(term);
    assert_eq!(words, ERL_REFC_BITS_SIZE, "only the on-heap pair is deep-copied");

    let mut dst = Heap::new(words + 8).expect("destination heap");
    let mut total = Duration::ZERO;
    for _ in 0..iterations {
        dst.reset();
        let mut dst_off = OffHeapTable::new();
        let start = Instant::now();
        let copy = {
            let mut ctx = CopyContext::new(&src_off, &mut dst_off);
            copy_struct(term, &mut dst, &mut ctx).expect("the copy succeeds")
        };
        total += start.elapsed();
        assert!(unsafe { is_refc_binary(copy) });
        assert_eq!(dst.used(), words);
        assert_eq!(dst_off.binaries().len(), 1);
        assert_eq!(dst_off.binaries()[0].data_ptr(), data_ptr, "the payload is shared");
    }

    let per_iter = total.as_nanos() as f64 / f64::from(iterations);
    println!(
        "{name:<34} words={words:<7} objects={:<7} ns/iter={per_iter:>10.1} \
         ns/word={:>6.2} ns/object={:>8.1} (payload shared by reference)",
       1,
        per_iter / words as f64,
        per_iter
    );
}

/// The smallest realistic message: a 3-element tuple (header + 3 immediate elements).
fn small_tuple(heap: &mut Heap) -> Eterm {
    heap.tuple(&[make_small(1), make_small(2), make_small(3)])
        .expect("the tuple fits")
}

/// A flat map (small, one object).
fn flat_map(heap: &mut Heap, atoms: &AtomTable) -> Eterm {
    let pairs: Vec<(Eterm, Eterm)> = (0..8)
        .map(|i| (make_small(i), make_small(i * 10 + 1)))
        .collect();
    write_map_term(heap, &pairs, atoms)
}

#[test]
fn report_copy_throughput_per_size_class() {
    const ITERS: u32 = 20_000;

    let atoms = AtomTable::new();
    let mut src = Heap::new(1 << 16).expect("source heap");

    // (1) Small object: header + 3 immediate elements.
    let small = small_tuple(&mut src);
    measure("small tuple (4 words)", small, 1, ITERS);

    // (2) A homogeneous chain of cons cells: 2000 cells, one object per cell.
    let deep = build_list(
        &mut src,
        &(0..2000).map(|i| make_small(i % 97)).collect::<Vec<_>>(),
    );
    measure("deep list (2000 cons cells)", deep, 2000, ITERS);

    // (3) A flat map (small, one object).
    let flat = flat_map(&mut src, &atoms);
    measure("flat map (8 keys)", flat, 1, ITERS);

    // (4) A HAMT map: many small trie nodes plus the leaf cons cells.
    let hamt_pairs: Vec<(Eterm, Eterm)> = (0..64)
        .map(|i| (make_small(i * 3), make_small(i * 1000 + 7)))
        .collect();
    let hamt = write_map_term(&mut src, &hamt_pairs, &atoms);
    // Rough object count: the nodes plus one leaf cons cell per key.
    let hamt_objects = 1 + 64;
    measure("HAMT map (64 keys)", hamt, hamt_objects, ITERS / 4);

    // (5) Refc-binary reuse: the payload is shared, so almost no words are deep-copied.
    measure_refc("refc binary reuse (512 B)", ITERS);
}

/// Reports the **Pass-1 (preflight) share** of a small copy, the figure the single-pass fast path
/// removes.
///
/// Each shape is copied into two destinations with identical head-room (`size_of_copy(term) + 64`
/// words of free space in both cases):
///
/// * **clean** - `used() == 0`, the case the copier's fast path serves with a single traversal;
/// * **pre-charged** - one dummy word consumed first, so the destination is *dirty* and the copy
///   goes through the preflight/sized path (one sizing walk, then the writing walk) whatever the
///   fast path's gate is.
///
/// The difference between the two rows is therefore the cost of the extra traversal (`Pass 1`)
/// that the fast path elides for a clean destination. Both rows assert correctness (the copy is
/// `=:=` the original and writes exactly `size_of_copy(term)` words plus the pre-charge), so the
/// comparison can never be won by doing less work.
#[test]
fn report_the_preflight_share_of_a_small_copy() {
    const ITERS: u32 = 100_000;

    let atoms = AtomTable::new();
    let mut src = Heap::new(1 << 16).expect("source heap");

    for (name, term) in [
        ("small tuple (4 words)", small_tuple(&mut src)),
        ("flat map (8 keys)", flat_map(&mut src, &atoms)),
    ] {
        let clean = measure_into(&format!("{name} [clean]"), term, 1, ITERS, 0);
        let dirty = measure_into(&format!("{name} [pre-charged]"), term, 1, ITERS, 1);
        println!(
            "{name}: preflight share = {:.1} ns/iter ({:.0}% of the clean copy's {:.1} ns)",
            dirty - clean,
            100.0 * (dirty - clean) / clean,
            clean
        );
    }
}
