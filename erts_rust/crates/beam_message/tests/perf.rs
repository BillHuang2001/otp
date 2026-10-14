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
//! exactly `size_of_copy(term)` - and never asserts a timing threshold, so it can never fail
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
use beam_gc::Mso;
use beam_message::off_heap::OffHeapTable;
use beam_message::{copy_struct, size_of_copy, CopyContext};
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
    measure_into(name, term, objects, iterations, Path::SinglePass)
}

/// Which copier path a measurement drives.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Path {
    /// A destination whose off-heap table is empty: the copier's **single-pass** fast path — the
    /// writing traversal alone, with no sizing walk in front of it. A destination *heap* that has
    /// already handed words out does **not** disqualify a copy from this path; only the off-heap
    /// table's emptiness decides.
    SinglePass,
    /// A destination whose off-heap table already holds one object: the copier's **sized** path (a
    /// `preflight_words` traversal, then the writing one). This is the only way left to produce a
    /// two-traversal reference point — a merely pre-charged heap is served by the fast path now.
    Sized,
}

/// The destination off-heap table a measurement stages for `path`.
fn staged_off_heap(path: Path) -> OffHeapTable {
    let mut off = OffHeapTable::new();
    if path == Path::Sized {
        assert_eq!(off.push(Mso::new(0xC0FF_EE00_u32, 8)), 0);
    }
    off
}

/// Times `iterations` copies of `term` into a destination heap of `size_of_copy(term) + 64` words,
/// driving the copier path `path` names, and prints ns/word, ns/object, ns/iteration and the best
/// iteration; returns the mean ns/iteration.
///
/// The two [`Path`] variants have identical head-room, so they are compared on equal terms. Both
/// assert correctness (the copy is `=:=` the original and writes exactly `size_of_copy(term)`),
/// so the comparison can never be won by doing less work.
fn measure_into(
    name: &str,
    term: Eterm,
    objects: usize,
    iterations: u32,
    path: Path,
) -> f64 {
    let words = size_of_copy(term);
    let atoms = AtomTable::new();
    let src_off = OffHeapTable::new();

    // Warm up (also validates correctness before the timed loop).
    let mut dst = Heap::new(words + 64).expect("destination heap");
    let copy = {
        let mut dst_off = staged_off_heap(path);
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        copy_struct(term, &mut dst, &mut ctx).expect("the copy succeeds")
    };
    assert!(
        compare::eq(term, copy, &atoms),
        "{name}: the measured copy must be correct"
    );
    assert_eq!(
        dst.used(),
        words,
        "{name}: the measured copy writes the sized count"
    );

    let mut total = Duration::ZERO;
    let mut best = Duration::MAX;
    for _ in 0..iterations {
        dst.reset();
        // Staging is outside the timed region: only the copy itself is measured.
        let mut dst_off = staged_off_heap(path);
        let start = Instant::now();
        let copy = {
            let mut ctx = CopyContext::new(&src_off, &mut dst_off);
            copy_struct(term, &mut dst, &mut ctx).expect("the copy succeeds")
        };
        let elapsed = start.elapsed();
        total += elapsed;
        if elapsed < best {
            best = elapsed;
        }
        // Keep the result observable so the copy is not optimised away, and re-check correctness.
        assert!(!is_immediate(copy));
        assert_eq!(dst.used(), words);
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

/// Reports the **Pass-1 (sizing) share** of a small copy — the figure the single-pass fast path
/// removes.
///
/// Each shape is copied into two destinations with identical head-room (`size_of_copy(term) + 64`
/// words of free space in both cases):
///
/// * **`[single-pass]`** — an **empty** destination off-heap table, the case the copier's fast path
///   serves with one traversal;
/// * **`[sized]`** — a destination off-heap table holding one object, which closes the gate and
///   forces the preflight/sized path (one sizing walk, then the writing walk).
///
/// The difference between the two rows is therefore the cost of the extra traversal (`Pass 1`) that
/// the fast path elides. Note that a merely **pre-charged heap** cannot produce the two-pass row any
/// more: a dirty destination heap with an empty off-heap table is served by the fast path too (see
/// `src/copy.rs`'s `is_fast_path_eligible`), so the reference point has to be a non-empty off-heap
/// table. Both rows assert correctness (the copy is `=:=` the original and writes exactly
/// `size_of_copy(term)` words), so the comparison can never be won by doing less work.
#[test]
fn report_the_preflight_share_of_a_small_copy() {
    const ITERS: u32 = 100_000;

    let atoms = AtomTable::new();
    let mut src = Heap::new(1 << 16).expect("source heap");

    for (name, term) in [
        ("small tuple (4 words)", small_tuple(&mut src)),
        ("flat map (8 keys)", flat_map(&mut src, &atoms)),
    ] {
        let single = measure_into(&format!("{name} [single-pass]"), term, 1, ITERS, Path::SinglePass);
        let sized = measure_into(&format!("{name} [sized]"), term, 1, ITERS, Path::Sized);
        println!(
            "{name}: preflight share = {:.1} ns/iter ({:.0}% of the single-pass copy's {:.1} ns)",
            sized - single,
            100.0 * (sized - single) / single,
            single
        );
    }
}
