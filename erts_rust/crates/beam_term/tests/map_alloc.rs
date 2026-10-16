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

//! Deterministic allocation evidence for the two map fast paths: the
//! flat-map-vs-flat-map comparison ([`cmp`]'s `MAP_SUBTAG` arm) and the
//! flat-map branch of [`beam_term::map::write_map`].
//!
//! This binary carries its own counting `#[global_allocator]` (the pattern of the
//! sibling `tests/atom_witness.rs`): a wrapper around `std::alloc::System` whose two
//! counters are armed **per thread**, so the libtest harness and any sibling test can
//! never perturb a measurement. An allocation either happens or it does not, so every
//! assertion here is host-load independent — nothing is timed.
//!
//! Four properties are pinned:
//!
//! 1. **`cmp` of two flat maps allocates nothing.** Both operands' keys/values are
//!    read straight out of the terms (`thing::flatmap_parts`), so the comparison needs
//!    neither a `Vec` nor a sort. The measurement is proven *sensitive* in the same
//!    harness by a positive control: the same comparison on two hash maps (> 32 keys)
//!    still takes the collecting fallback path and must allocate.
//! 2. **`write_map` of a 32-pair input allocates exactly one buffer.** The input arity
//!    alone fixes the layout (dedup can only shrink a <= 32-pair input), so the flat
//!    branch dedups and sorts **one** `Vec` instead of hash-deduping into one `Vec` and
//!    collecting a second — and it never calls `erts_map_hash`.
//! 3. **No observable change.** The `write_map` output is compared **word for word**
//!    against `thing::write_flatmap` fed the explicitly deduplicated, sorted pair list,
//!    and both `write_map` and the reference report the same
//!    [`beam_term::map::map_pairs_storage_order`]. The duplicate-key rule (last value
//!    wins) and the input-order independence are pinned separately.
//! 4. **The flat fast path is exact even for a hand-built unsorted flat map.**
//!    `thing::write_flatmap` preserves the pair order it is handed, so a caller can hold
//!    a flat map whose keys-tuple is *not* in `cmp_flatmap_keys` order. The comparison
//!    must then give the same answer as the sorted pair lists do; the fast path checks
//!    that precondition in O(n) and declines to the collecting path when it fails.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_term::map;
use beam_term::prelude::*;

// ---------------------------------------------------------------------------
// The counting allocator
// ---------------------------------------------------------------------------

/// Number of `alloc`/`realloc` calls made on an armed thread.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// Number of bytes requested on an armed thread.
static BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set only on the thread that is currently measuring. The counters are
    /// process-global, but *arming* is per-thread, so the test harness (which allocates
    /// while collecting results and formatting output) and every sibling test running on
    /// another thread can never perturb a measurement. That is what makes the allocation
    /// evidence deterministic rather than merely usually-zero.
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

/// Records one allocation of `bytes` on the current thread while it is armed.
///
/// `try_with` tolerates a thread whose TLS is being torn down; with a `const`-init
/// `Cell` there is no lazy allocation and no destructor to run, so this is safe to call
/// from inside the allocator.
#[inline]
fn record(bytes: usize) {
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(bytes, Ordering::Relaxed);
        }
    });
}

/// `System` plus per-thread opt-in counting.
struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Count only the growth: a shrink is not an allocation event.
        if new_size > layout.size() {
            record(new_size - layout.size());
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Runs `body` with the counters armed on this thread and returns
/// `(allocations, bytes, result)`.
fn allocations_in<T>(body: impl FnOnce() -> T) -> (usize, usize, T) {
    // Force the TLS to initialize *before* the window opens.
    ARMED.with(|armed| armed.set(false));
    let before_alloc = ALLOCATIONS.load(Ordering::Relaxed);
    let before_bytes = BYTES.load(Ordering::Relaxed);
    ARMED.with(|armed| armed.set(true));
    let result = body();
    ARMED.with(|armed| armed.set(false));
    let allocs = ALLOCATIONS.load(Ordering::Relaxed).wrapping_sub(before_alloc);
    let bytes = BYTES.load(Ordering::Relaxed).wrapping_sub(before_bytes);
    (allocs, bytes, result)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// `n` distinct atom keys (interned in index order) plus their small-int values.
struct Fixture {
    atoms: AtomTable,
    pairs: Vec<(Eterm, Eterm)>,
}

impl Fixture {
    fn new(n: usize) -> Fixture {
        let mut atoms = AtomTable::new();
        let keys: Vec<Eterm> = (0..n)
            .map(|i| make_atom(atoms.intern(&format!("map_alloc_key_{i:02}"))))
            .collect();
        let pairs = keys
            .into_iter()
            .enumerate()
            .map(|(i, k)| (k, make_small(i as i64)))
            .collect();
        Fixture { atoms, pairs }
    }

    /// The same key set with every value bumped by `delta`.
    fn bumped(&self, delta: i64) -> Vec<(Eterm, Eterm)> {
        self.pairs
            .iter()
            .map(|(k, v)| (*k, make_small(signed_value(*v) + delta)))
            .collect()
    }

    /// A `Heap` with room for one map of `n` input pairs (plus slack).
    fn heap(&self) -> Heap {
        Heap::new(2 * map_words_capacity(self.pairs.len()) + 64)
    }
}

/// Builds a canonical (sorted, deduplicated) flat map from `pairs` with `write_map`.
fn build_map(heap: &mut Heap, pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Eterm {
    // SAFETY: the heap has room for `map_words_capacity(pairs.len())` words (the fixture
    // over-allocates) and `write_map` writes every word of the produced map.
    unsafe { map::write_map(heap.hp(), pairs, atoms) }.expect("a well-formed map")
}

/// Builds an order-preserving **flat** map from `pairs` with `write_flatmap`.
///
/// `write_flatmap` never sorts, so the storage order is exactly `pairs`'s order — that
/// is what makes it the right tool for a hand-built unsorted flat map.
fn build_raw_flatmap(heap: &mut Heap, pairs: &[(Eterm, Eterm)]) -> Eterm {
    // SAFETY: the heap has room for `flatmap_size_words(pairs.len())` words (the fixture
    // over-allocates) and `pairs.len() <= MAP_SMALL_MAP_LIMIT`.
    unsafe { write_flatmap(heap.hp(), pairs) }
}

/// The stored `(keys, values)` of a flat map.
fn flat_parts(t: Eterm) -> (Vec<Eterm>, Vec<Eterm>) {
    // SAFETY: every map in this file is a flat map built by `write_map`/`write_flatmap`.
    let (keys, values) = unsafe { flatmap_parts(t) };
    (keys.to_vec(), values.to_vec())
}

/// Asserts that two flat maps occupy **word-for-word** the same block, comparing the
/// keys-tuple word by the block-relative offset it encodes (the term points back at its
/// own block, so its raw word is block-address specific).
fn assert_same_flatmap_block(actual: Eterm, expected: Eterm, n: usize) {
    assert!(
        unsafe { is_flatmap(actual) } && unsafe { is_flatmap(expected) },
        "both operands must be flat maps"
    );
    let words = flatmap_size_words(n);
    let a_base = unsafe { boxed_val(actual).sub(1 + n) };
    let b_base = unsafe { boxed_val(expected).sub(1 + n) };
    // SAFETY: both are live `4 + 2n`-word flat-map blocks rooted at their own head word.
    let (a, b) = unsafe {
        (
            core::slice::from_raw_parts(a_base, words),
            core::slice::from_raw_parts(b_base, words),
        )
    };
    for i in 0..words {
        if i == 3 + n {
            // The keys-tuple term: compare its **offset**, not its address.
            assert_eq!(
                boxed_val(a[i]),
                a_base,
                "word {i}: the keys tuple points at the block base"
            );
            assert_eq!(
                boxed_val(b[i]),
                b_base,
                "word {i}: the keys tuple points at the block base"
            );
        } else {
            assert_eq!(a[i], b[i], "flat-map word {i}");
        }
    }
}

// ---------------------------------------------------------------------------
// 1. `cmp` of two flat maps allocates nothing
// ---------------------------------------------------------------------------

#[test]
fn comparing_two_flat_maps_allocates_nothing() {
    const N: usize = 32;
    let f = Fixture::new(N);
    let mut ha = f.heap();
    let mut hb = f.heap();
    let map_a = build_map(&mut ha, &f.pairs, &f.atoms);
    let map_b = build_map(&mut hb, &f.bumped(1), &f.atoms);
    assert!(unsafe { is_flatmap(map_a) } && unsafe { is_flatmap(map_b) });

    // An independently built copy of `map_a` (distinct heap, same content) so the
    // measurement below covers a comparison that must run to `Equal` — not the
    // identical-word fast path.
    let mut hc = f.heap();
    let map_c = build_map(&mut hc, &f.pairs, &f.atoms);

    // Warm the path up outside the window, then measure the steady-state call.
    assert_eq!(cmp(map_a, map_b, &f.atoms), core::cmp::Ordering::Less);
    assert_eq!(cmp(map_a, map_c, &f.atoms), core::cmp::Ordering::Equal);
    let (allocs, bytes, ord) = allocations_in(|| cmp(map_a, map_b, &f.atoms));
    let (allocs_rev, _, ord_rev) = allocations_in(|| cmp(map_b, map_a, &f.atoms));
    let (allocs_eq, _, ord_eq) = allocations_in(|| cmp(map_a, map_c, &f.atoms));

    eprintln!(
        "map_alloc: cmp(flat32, flat32) allocations={allocs} bytes={bytes} \
         | reversed={allocs_rev} | distinct-heap-equal={allocs_eq}"
    );

    assert_eq!(ord, core::cmp::Ordering::Less);
    assert_eq!(ord_rev, core::cmp::Ordering::Greater);
    assert_eq!(ord_eq, core::cmp::Ordering::Equal, "a copy compares equal");
    assert_eq!(
        allocs, 0,
        "cmp of two flat maps must not allocate (it reads flatmap_parts directly)"
    );
    assert_eq!(allocs_rev, 0, "cmp of two flat maps must not allocate");
    assert_eq!(
        allocs_eq, 0,
        "cmp of two flat maps must not allocate, even when they compare equal"
    );
}

/// Positive control: the same harness **does** see the collecting fallback path
/// allocate, so the zero above cannot be an accidentally-narrow window.
#[test]
fn the_allocation_harness_is_sensitive() {
    const N: usize = 40; // > MAP_SMALL_MAP_LIMIT => hash maps
    let f = Fixture::new(N);
    let mut ha = f.heap();
    let mut hb = f.heap();
    let map_a = build_map(&mut ha, &f.pairs, &f.atoms);
    let map_b = build_map(&mut hb, &f.bumped(1), &f.atoms);
    assert!(unsafe { is_hashmap(map_a) } && unsafe { is_hashmap(map_b) });

    assert_eq!(cmp(map_a, map_b, &f.atoms), core::cmp::Ordering::Less);
    let (allocs, bytes, ord) = allocations_in(|| cmp(map_a, map_b, &f.atoms));
    eprintln!("map_alloc: cmp(hash40, hash40) allocations={allocs} bytes={bytes} (fallback path)");
    assert_eq!(ord, core::cmp::Ordering::Less);
    assert!(
        allocs > 0,
        "the hash-map fallback collects both operands into Vecs; if this is 0 the \
         harness cannot see allocations at all and the flat-map assertion is vacuous"
    );
}

// ---------------------------------------------------------------------------
// 2. `write_map` of a 32-pair input allocates exactly one buffer
// ---------------------------------------------------------------------------

#[test]
fn a_thirty_two_pair_write_map_allocates_one_buffer() {
    const N: usize = 32;
    let f = Fixture::new(N);
    // Input in a deliberately *reversed* order, so the sort really has work to do.
    let mut input = f.pairs.clone();
    input.reverse();

    let mut heap = f.heap();
    // Warm up (and prove the call works) outside the window, then rewind the arena so
    // the measured call starts from the same place.
    let _ = build_map(&mut heap, &input, &f.atoms);
    heap.reset();

    let (allocs, bytes, map) =
        allocations_in(|| build_map(&mut heap, &input, &f.atoms));

    // The reference: the same pairs, explicitly deduplicated and sorted, written by the
    // raw flat-map writer (no `Vec` bookkeeping, no hashing).
    let mut sorted = input.clone();
    sorted.sort_by(|a, b| cmp_flatmap_keys(a.0, b.0, &f.atoms));
    let mut reference_heap = f.heap();
    let reference = build_raw_flatmap(&mut reference_heap, &sorted);

    eprintln!(
        "map_alloc: write_map(32 pairs) allocations={allocs} bytes={bytes} \
         (pre-fast-path: one dedup Vec + one collect Vec)"
    );

    assert_same_flatmap_block(map, reference, N);
    assert_eq!(
        map_pairs_storage_order(map).expect("a flat map"),
        map_pairs_storage_order(reference).expect("a flat map"),
    );
    let (keys, values) = flat_parts(map);
    assert_eq!(keys.len(), N);
    assert_eq!(keys, sorted.iter().map(|p| p.0).collect::<Vec<_>>());
    assert_eq!(values, sorted.iter().map(|p| p.1).collect::<Vec<_>>());
    assert_eq!(map_size(map, &f.atoms).expect("a map"), N);

    assert_eq!(
        allocs, 1,
        "the flat branch must allocate exactly the single dedup/sort buffer"
    );
}

/// The duplicate-key rule (last value wins) and the input-order independence must not
/// change: the produced flat map is word-for-word the reference.
#[test]
fn write_map_still_dedups_last_value_wins() {
    const N: usize = 32;
    let f = Fixture::new(N);
    // 32 distinct keys (reversed, so the sort has work), then a repeat of key 5 with a
    // sentinel value **last**, which is therefore the winner.
    let mut input = f.pairs.clone();
    input.reverse();
    input.push((f.pairs[5].0, make_small(999)));

    let mut heap = f.heap();
    let map = build_map(&mut heap, &input, &f.atoms);

    let mut expected: Vec<(Eterm, Eterm)> = f.pairs.clone();
    expected[5].1 = make_small(999);
    expected.sort_by(|a, b| cmp_flatmap_keys(a.0, b.0, &f.atoms));
    let mut reference_heap = f.heap();
    let reference = build_raw_flatmap(&mut reference_heap, &expected);

    assert_same_flatmap_block(map, reference, N);
    assert_eq!(map_size(map, &f.atoms).expect("a map"), N);
    let (keys, values) = flat_parts(map);
    assert_eq!(values[5], make_small(999), "the last value wins");
    assert_eq!(keys, expected.iter().map(|p| p.0).collect::<Vec<_>>());
}

/// Above `MAP_SMALL_MAP_LIMIT` the hash path is untouched: still a HAMT, still
/// input-order independent, and still `Equal` to an independently built copy.
#[test]
fn the_hash_path_is_unchanged() {
    const N: usize = 40;
    let f = Fixture::new(N);
    let mut reversed = f.pairs.clone();
    reversed.reverse();

    let mut ha = f.heap();
    let mut hb = f.heap();
    let map_a = build_map(&mut ha, &f.pairs, &f.atoms);
    let map_b = build_map(&mut hb, &reversed, &f.atoms);

    assert!(unsafe { is_hashmap(map_a) } && unsafe { is_hashmap(map_b) });
    assert_eq!(map_size(map_a, &f.atoms).expect("a map"), N);
    assert_eq!(cmp(map_a, map_b, &f.atoms), core::cmp::Ordering::Equal);
    assert_eq!(
        map_pairs_storage_order(map_a).expect("a map"),
        map_pairs_storage_order(map_b).expect("a map"),
        "a hash map's storage order is independent of the input pair order"
    );
}

// ---------------------------------------------------------------------------
// 3. The flat fast path is exact for a hand-built *unsorted* flat map
// ---------------------------------------------------------------------------

#[test]
fn an_unsorted_flat_map_still_compares_exactly() {
    const N: usize = 32;
    let f = Fixture::new(N);

    // A flat map holding exactly `f.pairs`, but stored in reverse (`write_flatmap`
    // preserves the order it is handed, so the keys-tuple is NOT in
    // `cmp_flatmap_keys` order) — the fast path's O(n) precondition must decline it.
    let mut reversed = f.pairs.clone();
    reversed.reverse();
    let mut h_raw = f.heap();
    let unsorted = build_raw_flatmap(&mut h_raw, &reversed);
    let (raw_keys, _) = flat_parts(unsorted);
    assert_eq!(raw_keys, reversed.iter().map(|p| p.0).collect::<Vec<_>>());
    assert!(
        raw_keys.windows(2).any(|w| cmp_flatmap_keys(w[0], w[1], &f.atoms).is_gt()),
        "the hand-built map must really be unsorted"
    );

    // The canonical map over the same pairs, plus a value-bumped variant.
    let mut h_sorted = f.heap();
    let sorted = build_map(&mut h_sorted, &f.pairs, &f.atoms);
    let mut h_bumped = f.heap();
    let bumped = build_map(&mut h_bumped, &f.bumped(1), &f.atoms);

    // Same content, different storage order => still equal, in both directions.
    assert_eq!(cmp(unsorted, sorted, &f.atoms), core::cmp::Ordering::Equal);
    assert_eq!(cmp(sorted, unsorted, &f.atoms), core::cmp::Ordering::Equal);
    assert!(eq(unsorted, sorted, &f.atoms));

    // ... and the value comparison still runs in the *sorted* index order (key 5 is the
    // first index at which the two disagree, the bumped value being larger).
    assert_eq!(cmp(unsorted, bumped, &f.atoms), core::cmp::Ordering::Less);
    assert_eq!(cmp(bumped, unsorted, &f.atoms), core::cmp::Ordering::Greater);

    // A differently *permuted* hand-built map over the same pairs is equal too, and a
    // hand-built map with one key dropped orders by size (size first).
    let mut rotated = f.pairs.clone();
    rotated.rotate_left(7);
    let mut h_rot = f.heap();
    let permuted = build_raw_flatmap(&mut h_rot, &rotated);
    assert_eq!(cmp(unsorted, permuted, &f.atoms), core::cmp::Ordering::Equal);

    let mut h_short = f.heap();
    let shorter = build_raw_flatmap(&mut h_short, &reversed[1..]);
    assert_eq!(cmp(unsorted, shorter, &f.atoms), core::cmp::Ordering::Greater);
    assert_eq!(cmp(shorter, unsorted, &f.atoms), core::cmp::Ordering::Less);
}
