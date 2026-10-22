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

//! `map_words_capacity` is a **sound upper bound**, and this file measures how loose it
//! is: the reservation a hash-map build asks for vs the words the produced map actually
//! occupies.
//!
//! # Why this measurement exists
//!
//! [`beam_term::map::map_words_capacity`]`(n)` answers `21n + 18` for a hash map (`n >
//! MAP_SMALL_MAP_LIMIT`). It is derived from the trie shape and is *sound* (never an
//! under-estimate), but every caller — `beam_engine`'s `make_map_rooted`, hence every
//! `put_map_assoc` — reserves that many words from a **pointer-stable, never-freed**
//! arena. So the ratio `capacity / written` is an arena-footprint multiplier, not an
//! implementation detail: an in-process compile of `lib/kernel/src/inet_parse.erl`
//! attributes 69.7 % of its 1 328 040 786 arena words to `put_map_assoc` reservations of
//! exactly this `21n + 18` shape.
//!
//! # What is pinned (the memory contract) and what is only reported
//!
//! Pinned: **`written <= map_words_capacity(n)`** for every measured `n` and both key
//! distributions, verified *twice* and independently —
//!
//! 1. [`size_object`] over the produced map (the on-heap word count of the graph), and
//! 2. the **sentinel sweep**: the reservation is pre-filled with a marker word, so the
//!    length of the mutated prefix is the exact extent `write_map` wrote, and every word
//!    of `[written, capacity)` must still hold the marker. This is the stronger claim the
//!    bound's *cost model* rests on — a loose reservation that only writes its written
//!    prefix costs address space and untouched pages, not resident memory — so it is
//!    asserted, not assumed.
//!
//! Reported (with `-- --nocapture`): the ratio table per `n` and distribution, split into
//! the `2n` leaf words and the trie's own words.
//!
//! # The two distributions
//!
//! * **uniform** — keys `1..=n` (small integers). `erts_map_hash` of an immediate is
//!   `ihash_mix64(raw word)`, the splitmix64 finalizer, so these key hashes are uniformly
//!   spread over all 64 bits and all 16 trie levels. This is what *any* run of small
//!   integers looks like, i.e. what `inet_parse.erl`'s keys resemble.
//! * **deep-prefix** (adversarial) — keys grouped in pairs whose `erts_map_hash` values
//!   agree on the **low 56 bits**, so a pair walks ~14 consecutive single-slot inner
//!   nodes before it diverges. This is the trie's worst shape for *node* count: node
//!   words are the only quantity the `21n + 18` derivation can be loose about (the `2n`
//!   leaf words and the one-slot-per-node rule are exact), so this measures how close the
//!   bound can actually be driven.
//!
//! The adversarial keys are *constructed*, not searched: `ihash_mix64` is a bijection on
//! `u64`, so its inverse maps any chosen 64-bit hash to a unique raw word, and a raw word
//! whose low nibble is `0xF` is exactly a small-integer term. [`key_with_hash`] does that
//! (and re-derives the hash to prove it), so no 64-bit collision search is needed.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_term::map::{map_size, map_words_capacity, write_map};
use beam_term::prelude::*;

/// The marker left in the reservation's untouched tail. Not a valid term word, and not
/// `make_small`/atom-shaped, so no written word of any map in this file can collide with
/// it (see [`build_and_measure`], which fails loudly if one ever does).
const SENTINEL: Eterm = Eterm::from_raw(0xDEAD_BEEF_DEAD_BEEF);

// ---------------------------------------------------------------------------
// The counting allocator (the pattern of the sibling `tests/map_alloc.rs`)
// ---------------------------------------------------------------------------

/// Number of `alloc`/`realloc`-growth calls made on an armed thread.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// Number of bytes requested on an armed thread.
static BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set only on the thread that is currently measuring: the counters are process-global
    /// but *arming* is per-thread, so the libtest harness (which allocates while collecting
    /// results and formatting output) and every sibling test on another thread can never
    /// perturb a measurement.
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

/// Records one allocation of `bytes` on the current thread while it is armed.
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

/// The `n` values the objective asks for: just past the flat-map limit, three ordinary
/// hash-map sizes and the size the compile's own top requests name (`~2 775` pairs).
const SIZES: [usize; 5] = [33, 100, 1_000, 2_775, 10_000];

/// The measured sizes of `n` distinct keys, in words.
#[derive(Clone, Copy)]
struct Measured {
    /// `map_words_capacity(n)` — the reservation every caller makes.
    capacity: usize,
    /// The words the produced map occupies (`size_object`).
    written: usize,
    /// The words the map's `n` `[Key|Value]` cons cells occupy (`2n`).
    leaves: usize,
}

impl Measured {
    /// The trie's own words: the head node plus every inner/collision node.
    fn trie(&self) -> usize {
        self.written - self.leaves
    }
    /// `capacity / written`, the arena multiplier this reservation costs.
    fn ratio(&self) -> f64 {
        self.capacity as f64 / self.written as f64
    }
    /// The words reserved and never written.
    fn unwritten(&self) -> usize {
        self.capacity - self.written
    }
}

// ---------------------------------------------------------------------------
// The measurement
// ---------------------------------------------------------------------------

/// Builds `pairs` through the real [`write_map`] into a marker-filled reservation of
/// exactly [`map_words_capacity`]`(pairs.len())` words and measures the produced map.
///
/// Asserts, for this `n`:
/// * the build stays inside the reservation (the contract), and
/// * the reservation's tail past the written prefix is **untouched** — the marker
///   survives, which is what makes the loose bound cost address space rather than RSS.
fn build_and_measure(pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Measured {
    let n = pairs.len();
    let capacity = map_words_capacity(n);
    // `MAP_SMALL_MAP_LIMIT` and below is an exact flat-map fit, so only the hash branch is
    // interesting; the 64-word tail beyond `capacity` is slack so an overflow cannot
    // corrupt the allocator before the assertion below reports it.
    assert!(n > MAP_SMALL_MAP_LIMIT, "measure a hash map (n > 32)");
    let mut buf = vec![SENTINEL; capacity + 64];
    let base = buf.as_mut_ptr();
    // SAFETY: `buf` owns `capacity + 64` words and `base` names the first of them, so the
    // `map_words_capacity(n)` words `write_map` may write are exclusively owned, writable
    // and alive for as long as `map` is read below.
    let map = unsafe { write_map(base, pairs, atoms) }.expect("a well-formed map");

    // The map is a hash map of `pairs.len()` (distinct) keys.
    assert_eq!(map_size(map, atoms).expect("a map"), n);

    // 1. The bound, from the frozen on-heap layout.
    let written = size_object(map);

    // 2. The bound, from the write itself: `write_map` lays the cons cells at the base and
    //    then serializes the trie contiguously after them, so the mutated words form the
    //    prefix `[0, written)` and nothing past it may be written at all.
    let touched = buf
        .iter()
        .rposition(|w| *w != SENTINEL)
        .map_or(0, |i| i + 1);
    assert_eq!(
        touched, written,
        "n={n}: write_map touched {touched} words but size_object reports {written}"
    );
    for (i, word) in buf[written..].iter().enumerate() {
        assert_eq!(
            *word,
            SENTINEL,
            "n={n}: word {i} past the written prefix was written (reserved-but-unwritten \
             words must stay untouched)"
        );
    }
    assert!(
        written <= capacity,
        "n={n}: the map occupies {written} words but map_words_capacity({n}) is only {capacity}"
    );

    Measured {
        capacity,
        written,
        leaves: 2 * n,
    }
}

// ---------------------------------------------------------------------------
// Key distributions
// ---------------------------------------------------------------------------

/// Distribution (a): `n` distinct small integers `1..=n`.
///
/// A small integer hashes as `ihash_mix64(raw)`, a bit-mixer, so this is the evenly-spread
/// case — and exactly the shape a compiled module's integer keys produce.
fn uniform_keys(n: usize) -> Vec<Eterm> {
    (1..=n as i64).map(make_small).collect()
}

/// Distribution (b): pairs of keys whose `erts_map_hash` agrees on the low **56** bits.
///
/// Each pair therefore shares trie levels 0..13 (the level-`L` selector is the `L`-th
/// least-significant nibble) and diverges only at level 14/15, which is the deepest
/// fan-out a 64-bit hash permits: the pair's words are dominated by ~14 single-slot inner
/// nodes (2 words each) instead of the uniform ~3.1 words per pair-key.
fn deep_prefix_keys(n: usize) -> Vec<Eterm> {
    let mut keys: Vec<Eterm> = Vec::with_capacity(n);
    // Each iteration yields a pair sharing one 56-bit hash tail; the pairing is what makes
    // the chains long (a shared tail across *all* keys would share one chain instead).
    for pair in 0..n.div_ceil(2) {
        let tail = (pair as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C1F) & 0x00FF_FFFF_FFFF_FFFF;
        let mut members: Vec<Eterm> = Vec::with_capacity(2);
        // `hi` is the pair's free byte (trie levels 14/15). A target hash has ~1/32 chance
        // of being a canonical small int, so ~8 of the 256 candidates qualify; a candidate
        // tail that somehow yields fewer than two is perturbed by one bit of level 12 and
        // retried, so the construction always terminates deterministically.
        for attempt in 0..64u64 {
            let base = tail ^ (attempt << 48);
            members.clear();
            for hi in 0u64..=0xFF {
                if keys.len() + members.len() >= n {
                    break;
                }
                if let Some(key) = key_with_hash(base | (hi << 56)) {
                    members.push(key);
                    if members.len() == 2 {
                        break;
                    }
                }
            }
            if members.len() == 2 || keys.len() + members.len() >= n {
                break;
            }
        }
        assert!(
            !members.is_empty(),
            "no small-integer key found for the adversarial hash tail {tail:#x}"
        );
        keys.extend(members.iter().copied());
        // A pair that lost its partner (only reachable on an odd `n`) still contributes its
        // one key; the caller truncates to `n` below.
        keys.truncate(n);
    }
    keys.truncate(n);
    assert_eq!(keys.len(), n);
    keys
}

/// The small-integer key whose [`erts_map_hash`] is exactly `target`, if `target` has one.
///
/// `ihash_mix64` is a bijection on `u64` (two odd-constant multiplies and three invertible
/// xor-shifts), so `target` has exactly one preimage; it is a small-integer term iff its
/// low nibble is `0xF` (every small int's raw word is `value << 4 | 0xF`) and the remaining
/// 60 bits fit `MAX_SMALL`.
fn key_with_hash(target: u64) -> Option<Eterm> {
    let raw = unmix64(target);
    // `make_small`'s canonical form: `(value << 4) | 0xF`, value <= MAX_SMALL = 2^59 - 1.
    if raw & 0xF != 0xF || raw >= 1 << 63 {
        return None;
    }
    let key = make_small((raw >> 4) as i64);
    // The witness: the key really does hash to the target, so the adversary is exact.
    assert_eq!(
        erts_map_hash(key),
        target,
        "the constructed key must hash to the requested value"
    );
    Some(key)
}

/// The inverse of [`ihash_mix64`] (`erl_term_hashing.c:1747-1767`):
/// `h ^= h >> 33; h *= M1; h ^= h >> 33; h *= M2; h ^= h >> 33`, inverted step by step.
fn unmix64(hash: u64) -> u64 {
    const M1: u64 = 0xFF51_AFD7_ED55_8CCD;
    const M2: u64 = 0xC4CE_B9FE_1A85_EC53;
    let h = unxorshift_right(hash, 33);
    let h = h.wrapping_mul(odd_inverse(M2));
    let h = unxorshift_right(h, 33);
    let h = h.wrapping_mul(odd_inverse(M1));
    unxorshift_right(h, 33)
}

/// The inverse of `x -> x ^ (x >> shift)`: applying the forward map `64 / shift + 1` times
/// recovers `x` (each pass resolves the next `shift` higher bits).
fn unxorshift_right(y: u64, shift: u32) -> u64 {
    let mut x = y;
    for _ in 0..(64 / shift + 1) {
        x = y ^ (x >> shift);
    }
    x
}

/// The multiplicative inverse of an odd `u64` modulo 2^64 (Newton's iteration, doubling the
/// correct low-bit count each pass: 1 → 2 → 4 → 8 → 16 → 32 → 64).
fn odd_inverse(a: u64) -> u64 {
    assert!(a & 1 == 1, "only an odd value is invertible modulo 2^64");
    let mut inv = 1u64;
    for _ in 0..6 {
        inv = inv.wrapping_mul(2u64.wrapping_sub(a.wrapping_mul(inv)));
    }
    inv
}

// ---------------------------------------------------------------------------
// The pinned regression test
// ---------------------------------------------------------------------------

/// **The memory contract**: `map_words_capacity(n)` is a sound upper bound on the words
/// [`write_map`] writes, for every measured `n` and both key distributions — and the
/// words it does not write are left untouched.
///
/// Also prints the ratio table (run with `-- --nocapture`): for each `n`, the reservation,
/// the written words, the `capacity / written` multiplier and the split between the `2n`
/// leaf words and the trie's own words.
#[test]
fn map_words_capacity_is_a_bound_and_the_written_size_is_reported() {
    let atoms = AtomTable::new();
    let uniform_header = "uniform (keys 1..=n)";
    eprintln!();
    eprintln!(
        "{:<26} {:>6} {:>10} {:>9} {:>7} {:>9} {:>9} {:>8}",
        "distribution", "n", "capacity", "written", "ratio", "leaves", "trie", "unused"
    );
    let mut results: Vec<(&str, usize, Measured)> = Vec::new();
    for (name, make_keys) in [
        (uniform_header, uniform_keys as fn(usize) -> Vec<Eterm>),
        ("deep-prefix (56-bit tail)", deep_prefix_keys),
    ] {
        for n in SIZES {
            let pairs: Vec<(Eterm, Eterm)> = make_keys(n)
                .into_iter()
                .enumerate()
                .map(|(i, k)| (k, make_small(i as i64)))
                .collect();
            let m = build_and_measure(&pairs, &atoms);
            eprintln!(
                "{:<26} {:>6} {:>10} {:>9} {:>6.2}x {:>9} {:>9} {:>8}",
                name,
                n,
                m.capacity,
                m.written,
                m.ratio(),
                m.leaves,
                m.trie(),
                m.unwritten()
            );
            results.push((name, n, m));
        }
    }

    // The reported table is data, not decoration: pin the shape the bound has to explain so
    // a change in the trie layout (or in `map_words_capacity`) cannot slip past unmeasured.
    for (name, n, m) in &results {
        assert!(
            m.written <= m.capacity,
            "{name} n={n}: written {} exceeds capacity {}",
            m.written,
            m.capacity
        );
        // The `2n` cons cells are exact and forced — every key needs one `[K|V]` cell.
        assert_eq!(m.leaves, 2 * n, "{name} n={n}: one two-word cell per key");
    }
    // The bound's looseness is *distribution*-dependent, and both ends are pinned:
    // * a uniformly spread key set leaves more than half the reservation unwritten;
    // * the deep-chain adversary drives the written size to within half of the bound,
    //   i.e. the `21n + 18` derivation stays sound even in the trie's worst shape.
    // (A change on either side is a behaviour change: re-measure and update this table.)
    for (_, n, m) in results.iter().filter(|(name, _, _)| *name == uniform_header) {
        assert!(
            m.written * 2 < m.capacity,
            "uniform n={n}: written {} is not under half of capacity {}",
            m.written,
            m.capacity
        );
    }
    for (_, n, m) in results.iter().filter(|(name, _, _)| *name != uniform_header) {
        assert!(
            m.written * 2 > m.capacity,
            "deep-prefix n={n}: written {} should stay above half of capacity {} — the \
             adversary is supposed to make the bound tight, not slack",
            m.written,
            m.capacity
        );
    }
    // ... and the uniform case is the *tightest* observable shape: the trie's own words are
    // a small fraction of the leaves (one slot per node is forced, deep chains are not).
    let (_, _, uniform_1000) = results[2];
    assert!(
        uniform_1000.trie() < uniform_1000.leaves,
        "uniform n=1000: trie words {} should stay well under the {} leaf words",
        uniform_1000.trie(),
        uniform_1000.leaves
    );
    // The adversary works: deep chains cost strictly more node words than uniform spread.
    let (_, _, deep_1000) = results[7];
    assert!(
        deep_1000.trie() > 4 * uniform_1000.trie(),
        "deep-prefix n=1000 must be a materially worse trie shape than uniform \
         ({} vs {} trie words)",
        deep_1000.trie(),
        uniform_1000.trie()
    );
}

/// The **overwrite** shape a `put_map_assoc` reservation actually has: the caller reserves
/// for `distinct + updates` pairs (it cannot know how many update keys are new), so an
/// update that only overwrites existing keys reserves for a map that never materializes.
///
/// Reported only; the same contract assertions apply.
#[test]
fn an_overwrite_only_update_reserves_for_pairs_it_never_writes() {
    let atoms = AtomTable::new();
    // `n` source pairs plus `n/2` updates that all hit existing keys.
    const N: usize = 1_000;
    let base = uniform_keys(N);
    let mut pairs: Vec<(Eterm, Eterm)> = base
        .iter()
        .enumerate()
        .map(|(i, k)| (*k, make_small(i as i64)))
        .collect();
    for (i, key) in base.iter().take(N / 2).enumerate() {
        pairs.push((*key, make_small(10_000 + i as i64)));
    }
    let capacity = map_words_capacity(pairs.len());
    let mut buf = vec![SENTINEL; capacity + 64];
    // SAFETY: as in `build_and_measure` — `buf` owns `capacity + 64` words.
    let map = unsafe { write_map(buf.as_mut_ptr(), &pairs, &atoms) }.expect("a well-formed map");
    let written = size_object(map);
    assert_eq!(
        map_size(map, &atoms).expect("a map"),
        N,
        "an overwrite-only update keeps the source's key count"
    );
    assert_eq!(
        buf.iter().rposition(|w| *w != SENTINEL).map_or(0, |i| i + 1),
        written,
        "write_map writes exactly one contiguous prefix"
    );
    assert!(written <= capacity, "written {written} <= capacity {capacity}");
    eprintln!(
        "\noverwrite: {} input pairs -> {} distinct keys | capacity {} (reserved for the \
         input) vs written {} = {:.2}x, {} words reserved for pairs that never exist",
        pairs.len(),
        N,
        capacity,
        written,
        capacity as f64 / written as f64,
        map_words_capacity(pairs.len()) - map_words_capacity(N)
    );
}

/// The **Rust-heap** half of the same question: what a single [`write_map`] costs in
/// `malloc`/`free` traffic, as a function of `n`.
///
/// `write_map` builds a temporary trie in Rust memory (`TrieNode`/`TrieChild` with a
/// `Vec`/`Box` per node) and two pair buffers before serializing into the caller's words,
/// so its cost is dominated by per-node allocations, not by a fixed number of buffers.
/// The measured count is pinned against the map's own node count — `nodes = (written -
/// 3n) / 2`, since `written = 2n` (cons cells) `+ 2·nodes + n` (one slot per node plus one
/// per node header, and `Σslots = nodes - 1 + n`) — with a band that admits the `Box`, the
/// child `Vec`'s doublings and the serializer's per-node slot buffer:
/// `nodes <= allocations <= 6·nodes`. Printed for the record (run with `--nocapture`).
#[test]
fn write_map_rust_heap_allocation_count_is_reported() {
    let atoms = AtomTable::new();
    eprintln!();
    eprintln!(
        "{:>6} {:>9} {:>7} {:>8} {:>12} {:>10}",
        "n", "written", "nodes", "allocs", "bytes", "bytes/alloc"
    );
    for n in SIZES {
        let pairs: Vec<(Eterm, Eterm)> = uniform_keys(n)
            .into_iter()
            .enumerate()
            .map(|(i, k)| (k, make_small(i as i64)))
            .collect();
        // The buffer and the pair list are built *outside* the measurement window, so the
        // window covers exactly the builder's own scratch memory.
        let capacity = map_words_capacity(n);
        let mut buf = vec![SENTINEL; capacity + 64];
        let base = buf.as_mut_ptr();
        let (allocs, bytes, map) = allocations_in(|| {
            // SAFETY: `buf` owns `capacity + 64` words and `base` names the first of them,
            // so `map_words_capacity(n)` words are exclusively owned and writable.
            unsafe { write_map(base, &pairs, &atoms) }.expect("a well-formed map")
        });
        let written = size_object(map);
        assert!(written >= 3 * n, "n={n}: written {written} < 3n");
        let nodes = (written - 3 * n).div_ceil(2);
        eprintln!(
            "{:>6} {:>9} {:>7} {:>8} {:>12} {:>10.0}",
            n,
            written,
            nodes,
            allocs,
            bytes,
            bytes as f64 / allocs as f64
        );
        assert!(
            allocs >= nodes,
            "n={n}: {allocs} allocations for {nodes} trie nodes — every node owns a Box"
        );
        assert!(
            allocs <= 6 * nodes,
            "n={n}: {allocs} allocations for {nodes} trie nodes — more than the trie's own \
             four per-node buffers would explain"
        );
    }
}

/// The construction behind the adversarial distribution is exact, so prove both halves:
/// [`unmix64`] inverts `ihash_mix64` on a spread of values, and [`key_with_hash`]'s keys
/// really hash to the requested target (including the boundary shapes).
#[test]
fn the_adversarial_key_construction_is_exact() {
    // `unmix64` is `ihash_mix64`'s inverse on every input, not just the ones that happen to
    // produce a small integer.
    for h in [0u64, 1, 0xF, 0xDEAD_BEEF, !0, 0x8000_0000_0000_0000, 1 << 63, 3 << 62] {
        assert_eq!(ihash_mix64(unmix64(h)), h, "unmix64 inverts ihash_mix64");
    }
    let mut state = 0x243F_6A88_85A3_08D3u64;
    for _ in 0..64 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        assert_eq!(ihash_mix64(unmix64(state)), state);
    }
    // A constructed deep-prefix pair really agrees on the low 56 hash bits and really
    // diverges at the hash's top byte — the property the trie shape depends on.
    let keys = deep_prefix_keys(4);
    let hs: Vec<u64> = keys.iter().map(|k| erts_map_hash(*k)).collect();
    assert_eq!(hs[0] & 0x00FF_FFFF_FFFF_FFFF, hs[1] & 0x00FF_FFFF_FFFF_FFFF);
    assert_ne!(hs[0] >> 56, hs[1] >> 56);
    // Distinct keys, all of them.
    let mut sorted = keys.clone();
    sorted.sort_by_key(|k| k.raw());
    sorted.dedup();
    assert_eq!(sorted.len(), keys.len(), "the constructed keys are distinct");
}
