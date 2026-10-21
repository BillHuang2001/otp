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

//! An in-process A/B benchmark for [`beam_term::map::write_map`]: the live builder
//! against a **frozen copy of the pre-change builder**, over a corpus shaped like the
//! maps a compiler builds.
//!
//! The point of the exercise is the duplicate-key collapse, which the current
//! implementation does with a per-insertion linear `eq` scan — O(n²) on both branches.
//! Both sides here write into the **same pre-allocated buffer** and consume the **same
//! pre-built `pairs: Vec<(Eterm, Eterm)>`**, so what is timed is the builder itself:
//! no output allocation, no input construction, no BEAM heap.
//!
//! Absolute latencies on a shared host are meaningless on their own; the number to read
//! is the **ratio between the two columns**, which is a same-run, same-host comparison.
//! The two sides are interleaved **per map** (one new build, then one retired build,
//! alternating) so a load burst cannot bias one column, and the corpus holds only
//! *distinct* keys — a duplicate-free map is the common case in a compiler, and it is
//! also the case in which the scan cost is fully exposed (with duplicates the scan
//! finds a hit early).
//!
//! ```text
//! erts_rust/scripts/cargo.sh run --release --manifest-path erts_rust/Cargo.toml \
//!     -p beam_term --example map_build_bench -- [maps]
//! ```
//!
//! * `maps` — total maps timed per side (default 200 000). Each corpus entry is built
//!   `maps / corpus.len()` times, rounded up.
//!
//! Columns:
//! * `maps`          — maps timed in this bucket (per side);
//! * `live ns/map`   — [`beam_term::map::write_map`];
//! * `retired ns/map`— the pre-change quadratic builder (the local [`retired`] copy);
//! * `live/retired`  — ≈ 1.0 while both are the same algorithm; < 1.0 once the fix lands;
//! * `retired/live`  — the same ratio inverted, i.e. the speed-up of the fix.

use std::hint::black_box;
use std::time::{Duration, Instant};

use beam_term::atom::{Atom, AtomTable};
use beam_term::eterm::Eterm;
use beam_term::immediate::{make_atom, make_small};
use beam_term::map;

// ---------------------------------------------------------------------------
// The retired algorithm — the A/B baseline (a frozen copy of the pre-change builder)
// ---------------------------------------------------------------------------

/// A **frozen, verbatim copy** of the pre-change `map::write_map` and its private trie
/// helpers, kept here as the benchmark baseline. It is test-only evidence, never used
/// by the crate; do not "improve" it (the sibling
/// `tests/map_build_equivalence.rs` carries the same copy as the word-equality oracle).
mod retired {
    use beam_term::atom::AtomTable;
    use beam_term::boxed::{make_arityval, make_boxed};
    use beam_term::compare::{cmp, cmp_flatmap_keys, eq};
    use beam_term::error::TermResult;
    use beam_term::eterm::Eterm;
    use beam_term::ihash::{erts_map_hash, hashmap_nibble, swizzle_map_hash};
    use beam_term::list::{car, make_list, CAR_OFFSET, CDR_OFFSET};
    use beam_term::map::{make_map_header, HAMT_MAX_LEVEL, MAP_SMALL_MAP_LIMIT};
    use beam_term::tag::{
        MAP_HEADER_TAG_HAMT_HEAD_ARRAY, MAP_HEADER_TAG_HAMT_HEAD_BITMAP,
        MAP_HEADER_TAG_HAMT_NODE_BITMAP,
    };
    use beam_term::thing::write_flatmap;

    /// The (inclusive) deepest level a node may occupy (`src/map.rs`).
    const HAMT_LAST_NODE_LEVEL: usize = HAMT_MAX_LEVEL - 1;

    /// A fully populated 16-slot bitmap (`src/map.rs`).
    const HAMT_FULL_BITMAP: u16 = 0xffff;

    /// One slot of a [`TrieNode`] (`src/map.rs`).
    enum TrieChild {
        Leaf(Eterm),
        Collision(Vec<Eterm>),
        Node(Box<TrieNode>),
    }

    /// A trie node (`src/map.rs`).
    struct TrieNode {
        bitmap: u16,
        children: Vec<TrieChild>,
    }

    /// `src/map.rs::build_trie`.
    fn build_trie(leaves: &[(u64, Eterm)], level: usize, atoms: &AtomTable) -> TrieNode {
        let mut bitmap = 0u16;
        let mut children: Vec<TrieChild> = Vec::new();
        let mut i = 0;
        while i < leaves.len() {
            let nib = hashmap_nibble(leaves[i].0, level);
            let mut j = i + 1;
            while j < leaves.len() && hashmap_nibble(leaves[j].0, level) == nib {
                j += 1;
            }
            bitmap |= 1u16 << nib;
            if j == i + 1 {
                children.push(TrieChild::Leaf(leaves[i].1));
            } else if level == HAMT_LAST_NODE_LEVEL {
                let mut cells: Vec<Eterm> = leaves[i..j].iter().map(|(_, cell)| *cell).collect();
                // SAFETY: the cells are live cons cells; `cmp` reads their keys.
                cells.sort_by(|a, b| cmp(unsafe { car(*a) }, unsafe { car(*b) }, atoms));
                children.push(TrieChild::Collision(cells));
            } else {
                children.push(TrieChild::Node(Box::new(build_trie(
                    &leaves[i..j],
                    level + 1,
                    atoms,
                ))));
            }
            i = j;
        }
        TrieNode { bitmap, children }
    }

    /// `src/map.rs::trie_words`.
    fn trie_words(node: &TrieNode, level: usize) -> usize {
        let nslots = node.bitmap.count_ones() as usize;
        let mut words = if level == 0 { 2 + nslots } else { 1 + nslots };
        for child in &node.children {
            match child {
                TrieChild::Leaf(_) => {}
                TrieChild::Collision(cells) => words += 1 + cells.len(),
                TrieChild::Node(sub) => words += trie_words(sub, level + 1),
            }
        }
        words
    }

    /// `src/map.rs::serialize_node`.
    ///
    /// # Safety
    ///
    /// The caller must have reserved the words [`trie_words`] reports for this node and
    /// its sub-nodes (plus, for the head, its `size` word), contiguously from `*hp`.
    unsafe fn serialize_node(
        node: &TrieNode,
        level: usize,
        total_keys: usize,
        hp: &mut *mut Eterm,
    ) -> Eterm {
        let mut slots: Vec<Eterm> = Vec::with_capacity(node.children.len());
        for child in &node.children {
            let term = match child {
                TrieChild::Leaf(cell) => *cell,
                TrieChild::Collision(cells) => {
                    let n = cells.len();
                    let p = *hp;
                    // SAFETY: the caller reserved the `n + 1` words of a collision tuple.
                    unsafe {
                        *p = make_arityval(n);
                        for (i, cell) in cells.iter().enumerate() {
                            *p.add(1 + i) = *cell;
                        }
                    }
                    *hp = unsafe { p.add(1 + n) };
                    make_boxed(p)
                }
                TrieChild::Node(sub) => unsafe { serialize_node(sub, level + 1, total_keys, hp) },
            };
            slots.push(term);
        }

        let p = *hp;
        let bitmap = node.bitmap;
        let nslots = bitmap.count_ones() as usize;
        let (header, slots_off, words) = if level == 0 {
            if bitmap == HAMT_FULL_BITMAP {
                let hdr = make_map_header(MAP_HEADER_TAG_HAMT_HEAD_ARRAY, 1, 0xffff);
                (hdr, 2, 2 + 16)
            } else {
                let hdr = make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, bitmap as usize);
                (hdr, 2, 2 + nslots)
            }
        } else {
            let hdr = make_map_header(MAP_HEADER_TAG_HAMT_NODE_BITMAP, 0, bitmap as usize);
            (hdr, 1, 1 + nslots)
        };
        debug_assert_eq!(nslots, slots.len(), "one slot per set bitmap bit");
        // SAFETY: the caller reserved `words` words at `p`; the header, the head's `size`
        // word and every slot are written before the pointer escapes.
        unsafe {
            *p = header;
            if level == 0 {
                *p.add(1) = Eterm::from_raw(total_keys);
            }
            for (i, slot) in slots.iter().enumerate() {
                *p.add(slots_off + i) = *slot;
            }
        }
        *hp = unsafe { p.add(words) };
        make_boxed(p)
    }

    /// The pre-change `map::write_map`: the quadratic per-insertion `eq` scan on both
    /// branches.
    ///
    /// # Safety
    ///
    /// `hp` must point at [`beam_term::map::map_words_capacity`]`(pairs.len())`
    /// exclusively-owned, writable words.
    pub unsafe fn write_map(
        hp: *mut Eterm,
        pairs: &[(Eterm, Eterm)],
        atoms: &AtomTable,
    ) -> TermResult<Eterm> {
        if pairs.len() <= MAP_SMALL_MAP_LIMIT {
            let mut flat: Vec<(Eterm, Eterm)> = Vec::with_capacity(pairs.len());
            for &(key, value) in pairs {
                match flat.iter_mut().find(|(k, _)| eq(*k, key, atoms)) {
                    Some(entry) => entry.1 = value,
                    None => flat.push((key, value)),
                }
            }
            flat.sort_by(|a, b| cmp_flatmap_keys(a.0, b.0, atoms));
            // SAFETY: the caller reserved `flatmap_size_words(pairs.len())` words.
            return Ok(unsafe { write_flatmap(hp, &flat) });
        }

        let mut entries: Vec<(u64, Eterm, Eterm)> = Vec::with_capacity(pairs.len());
        for &(key, value) in pairs {
            let hash = erts_map_hash(key);
            match entries
                .iter_mut()
                .find(|(h, k, _)| *h == hash && eq(*k, key, atoms))
            {
                Some(entry) => entry.2 = value,
                None => entries.push((hash, key, value)),
            }
        }
        let n = entries.len();

        if n <= MAP_SMALL_MAP_LIMIT {
            entries.sort_by(|a, b| cmp_flatmap_keys(a.1, b.1, atoms));
            let flat: Vec<(Eterm, Eterm)> = entries.iter().map(|(_, k, v)| (*k, *v)).collect();
            // SAFETY: the caller reserved `flatmap_size_words(pairs.len())` words.
            return Ok(unsafe { write_flatmap(hp, &flat) });
        }
        entries.sort_by(|a, b| {
            swizzle_map_hash(a.0)
                .cmp(&swizzle_map_hash(b.0))
                .then_with(|| cmp(a.1, b.1, atoms))
        });
        let mut cur = hp;
        let mut leaves: Vec<(u64, Eterm)> = Vec::with_capacity(n);
        for &(hash, key, value) in &entries {
            let cell = cur;
            // SAFETY: words `0..2n` of the reserved block hold the `n` cons cells.
            unsafe {
                *cell.add(CAR_OFFSET) = key;
                *cell.add(CDR_OFFSET) = value;
                cur = cur.add(2);
            }
            leaves.push((hash, make_list(cell as *const Eterm)));
        }

        let trie = build_trie(&leaves, 0, atoms);
        debug_assert!(
            2 * n + trie_words(&trie, 0) <= beam_term::map::map_words_capacity(n),
            "map_words_capacity({n}) is not an upper bound",
        );
        Ok(unsafe { serialize_node(&trie, 0, n, &mut cur) })
    }
}

// ---------------------------------------------------------------------------
// The corpus: the map shapes a compiler builds
// ---------------------------------------------------------------------------

/// Total maps timed per side (overridable as the first argument).
const DEFAULT_MAPS: usize = 200_000;

/// How many atoms the fixture table holds — the compiler's atom-key range.
const N_ATOMS: usize = 512;

/// A deterministic xorshift64 generator, so the corpus is identical on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// One benchmark bucket: a label, and a list of distinct-key `pairs` inputs.
struct Bucket {
    label: &'static str,
    maps: Vec<Vec<(Eterm, Eterm)>>,
}

/// Builds the corpus.
///
/// Sizes follow what the compiler actually constructs — mostly tiny maps (2–6 pairs:
/// annotations, option maps, `#{line => N}`), a middle band (8–16), the largest flat
/// maps (21–32), and the smallest hash maps (33–64). Every map has **distinct** keys:
/// a duplicate-free map is both the common case and the one that exposes the full scan
/// cost (a duplicate is found early and short-circuits the per-insertion scan).
///
/// Half the keys are atoms (as a compiler's maps mostly are) and half are small ints,
/// so the comparison cost is representative; the values are small ints.
fn corpus() -> Vec<Bucket> {
    let mut rng = Rng(0x243F_6A88_85A3_08D3);

    let tiny: Vec<_> = (0..60).map(|i| make_map(&mut rng, 2 + i % 5)).collect();
    let middle: Vec<_> = (0..40)
        .map(|i| make_map(&mut rng, 8 + (i % 5) * 2))
        .collect();
    let flat_max: Vec<_> = (0..30).map(|i| make_map(&mut rng, 21 + i % 12)).collect();
    let hash_min: Vec<_> = [33usize, 40, 48, 56, 64]
        .iter()
        .map(|&n| make_map(&mut rng, n))
        .collect();

    vec![
        Bucket {
            label: "2-6",
            maps: tiny,
        },
        Bucket {
            label: "8-16",
            maps: middle,
        },
        Bucket {
            label: "21-32",
            maps: flat_max,
        },
        Bucket {
            label: "33-64",
            maps: hash_min,
        },
    ]
}

/// A novel atom-or-small-int key that is not already in `distinct`.
fn next_key(rng: &mut Rng, distinct: &mut Vec<Eterm>) -> Eterm {
    loop {
        let pick = rng.next() as usize;
        let key = if pick & 1 == 0 {
            make_atom(Atom::from_raw(((pick >> 1) % N_ATOMS) as u32))
        } else {
            make_small(((pick >> 1) % 100_000) as i64)
        };
        if !distinct.contains(&key) {
            distinct.push(key);
            return key;
        }
    }
}

/// `n` distinct-key pairs with small-int values.
fn make_map(rng: &mut Rng, n: usize) -> Vec<(Eterm, Eterm)> {
    let mut distinct = Vec::with_capacity(n);
    (0..n)
        .map(|i| (next_key(rng, &mut distinct), make_small(i as i64)))
        .collect()
}
// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

/// The accumulation for one side.
#[derive(Default, Clone, Copy)]
struct Side {
    nanos: u128,
    maps: usize,
}

impl Side {
    fn ns_per_map(&self) -> f64 {
        if self.maps == 0 {
            f64::NAN
        } else {
            self.nanos as f64 / self.maps as f64
        }
    }
}

/// Benchmarks one bucket: `rounds` passes over `bucket.maps`, timing the live writer and
/// the retired writer **interleaved per map** into the same pre-allocated buffer.
fn bench_bucket(bucket: &Bucket, rounds: usize, atoms: &AtomTable) -> (Side, Side) {
    let max_cap = bucket
        .maps
        .iter()
        .map(|p| map::map_words_capacity(p.len()))
        .max()
        .unwrap_or(0);
    // One output buffer for both sides, reused across every iteration: the measurement
    // is the builder, not the allocation of its output block.
    let mut buf: Vec<Eterm> = vec![Eterm::from_raw(0); max_cap];
    let base = buf.as_mut_ptr();

    let mut live = Side::default();
    let mut old = Side::default();
    // Warm up (page in the buffer, let the branch predictors settle) outside the window.
    for pairs in &bucket.maps {
        // SAFETY: `buf` holds `map_words_capacity(pairs.len())` words for every corpus
        // entry, and both writers are called with their documented preconditions.
        unsafe {
            black_box(map::write_map(base, pairs, atoms).unwrap());
            black_box(retired::write_map(base, pairs, atoms).unwrap());
        }
    }

    for _ in 0..rounds {
        for pairs in &bucket.maps {
            // SAFETY: as above. Each call fully overwrites the block it needs.
            let t0 = Instant::now();
            let m = unsafe { map::write_map(base, pairs, atoms) }.unwrap();
            black_box(m);
            let t_live = t0.elapsed();

            // SAFETY: as above.
            let t1 = Instant::now();
            let m = unsafe { retired::write_map(base, pairs, atoms) }.unwrap();
            black_box(m);
            let t_old = t1.elapsed();

            live.nanos += t_live.as_nanos();
            live.maps += 1;
            old.nanos += t_old.as_nanos();
            old.maps += 1;
        }
    }
    // Keep the buffer observably live so nothing above can be elided.
    black_box(&buf[..]);
    (live, old)
}

fn fmt_ratio(a: f64, b: f64) -> String {
    if b == 0.0 {
        "n/a".to_string()
    } else {
        format!("{:.3}x", a / b)
    }
}

fn main() {
    let rounds_maps: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(DEFAULT_MAPS);

    let loadavg = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let load = loadavg.split_whitespace().next().unwrap_or("?").to_string();

    let mut atoms = AtomTable::new();
    let names: Vec<String> = (0..N_ATOMS).map(|i| format!("map_key_{i}")).collect();
    atoms.seed(names.iter().map(String::as_str));
    let atoms = atoms;

    let buckets = corpus();

    // One correctness guard outside the timed windows: both sides must build the same
    // *size* map for every corpus entry, so the timing compares like with like.
    let mut guard_buf = vec![
        Eterm::from_raw(0);
        buckets
            .iter()
            .flat_map(|b| b.maps.iter())
            .map(|p| map::map_words_capacity(p.len()))
            .max()
            .unwrap_or(0)
    ];
    for bucket in &buckets {
        for pairs in &bucket.maps {
            let base = guard_buf.as_mut_ptr();
            // SAFETY: `guard_buf` is sized for the largest corpus entry.
            let a = unsafe { map::write_map(base, pairs, &atoms) }.unwrap();
            let a_size = map::map_size(a, &atoms).unwrap();
            // SAFETY: as above.
            let b = unsafe { retired::write_map(base, pairs, &atoms) }.unwrap();
            let b_size = map::map_size(b, &atoms).unwrap();
            assert_eq!(
                a_size,
                pairs.len(),
                "the corpus holds distinct keys, so every map is full-size"
            );
            assert_eq!(
                a_size, b_size,
                "the two builders must agree on the map size"
            );
        }
    }

    println!(
        "map_build_bench: maps/side={rounds_maps} corpus={} shapes loadavg={load}",
        buckets.iter().map(|b| b.maps.len()).sum::<usize>()
    );
    println!(
        "{:>8}  {:>8}  {:>12}  {:>15}  {:>13}  {:>13}",
        "bucket", "maps", "live ns/map", "retired ns/map", "live/retired", "retired/live"
    );

    let mut total_live = Side::default();
    let mut total_old = Side::default();
    let mut timings: Vec<(String, Duration)> = Vec::new();
    for bucket in &buckets {
        let rounds = rounds_maps.div_ceil(bucket.maps.len());
        let t0 = Instant::now();
        let (live, old) = bench_bucket(bucket, rounds, &atoms);
        timings.push((bucket.label.to_string(), t0.elapsed()));

        println!(
            "{:>8}  {:>8}  {:>12.3}  {:>15.3}  {:>13}  {:>13}",
            bucket.label,
            live.maps,
            live.ns_per_map(),
            old.ns_per_map(),
            fmt_ratio(live.ns_per_map(), old.ns_per_map()),
            fmt_ratio(old.ns_per_map(), live.ns_per_map()),
        );
        total_live.nanos += live.nanos;
        total_live.maps += live.maps;
        total_old.nanos += old.nanos;
        total_old.maps += old.maps;
    }

    println!(
        "{:>8}  {:>8}  {:>12.3}  {:>15.3}  {:>13}  {:>13}",
        "ALL",
        total_live.maps,
        total_live.ns_per_map(),
        total_old.ns_per_map(),
        fmt_ratio(total_live.ns_per_map(), total_old.ns_per_map()),
        fmt_ratio(total_old.ns_per_map(), total_live.ns_per_map()),
    );
    for (label, took) in &timings {
        println!("  side bucket {label}: wall {:?}", took);
    }
}
