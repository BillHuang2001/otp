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

//! **Word-equality regression harness for [`beam_term::map::write_map`].**
//!
//! `write_map` is O(n²) on *both* branches: it deduplicates its input with a
//! per-insertion linear scan — `flat.iter_mut().find(|(k, _)| eq(*k, key, atoms))`
//! on the flat branch, the same shape with a `u64` hash pre-filter on the hash
//! branch. The planned replacement is **one stable sort followed by one linear
//! collapse of adjacent `eq`-equal runs** (keeping each run's first key word and
//! last value).
//!
//! That rewrite may only be a *representation-preserving* change, so this file
//! pins the observable output **word for word** before the change. Every matrix row
//! is built twice:
//!
//! * by the live [`map::write_map`], and
//! * by [`retired::write_map`], a **verbatim copy of today's algorithm kept here as
//!   the equivalence oracle**.
//!
//! The duplication is deliberate, not drift: a reference implementation has to be
//! frozen to be a reference. The copy is a test-only oracle — it is never used by
//! the crate — and it will diverge from `src/map.rs` by design once the fix lands,
//! which is exactly what makes it a regression net.
//!
//! The two buffers live at different heap addresses, so they are compared after
//! [`normalize`] rewrites every in-buffer tagged pointer to its block-relative
//! offset; every other word must be bit-identical. Each row additionally re-checks
//! the overflow sentinel beyond [`map_words_capacity`], the produced shape
//! (flat vs HAMT), [`map_size`], `map_get` for every key, [`map_pairs`] (the
//! [`cmp`] order) and [`map_pairs_storage_order`] (each key exactly once).
//!
//! Three assertions in here are the *proof obligations* of the coming rewrite:
//!
//! 1. `assert_same_words` on the whole matrix — the new algorithm must keep
//!    producing the same bytes.
//! 2. the hard-coded [`GOLDEN_TWO_KEYS`]/[`GOLDEN_THREE_KEYS`]/[`GOLDEN_REVERSED`]
//!    vectors, captured from the *current* implementation, so the test is an oracle
//!    independent of both implementations.
//! 3. [`eq_equal_keys_are_adjacent_after_the_branch_sort_passes`], which is the
//!    premise the adjacent-run collapse relies on: for either branch's sort key,
//!    `eq`-equal keys must compare `Equal` (so a *stable* sort leaves them adjacent
//!    and in input order).

use beam_term::atom::AtomTable;
use beam_term::bitstring::{heap_bits_size_words, write_binary};
use beam_term::boxed::{is_flatmap, is_hashmap};
use beam_term::compare::{cmp, cmp_flatmap_keys, eq};
use beam_term::error::TermResult;
use beam_term::eterm::Eterm;
use beam_term::heap::Heap;
use beam_term::ihash::{erts_map_hash, swizzle_map_hash};
use beam_term::immediate::{make_atom, make_small, NIL};
use beam_term::map::{
    self, map_get, map_pairs, map_pairs_storage_order, map_size, map_words_capacity,
    MAP_SMALL_MAP_LIMIT,
};
use beam_term::refc::write_refc_binary;
use beam_term::thing::write_float;

// ---------------------------------------------------------------------------
// The retired algorithm — the equivalence oracle
// ---------------------------------------------------------------------------

/// A **frozen, verbatim copy** of the pre-change `map::write_map` (plus its private
/// trie helpers), kept in this file as the equivalence oracle.
///
/// Do not "improve" it. It is the yardstick the new implementation is measured
/// against; the only edits allowed are ones that keep it byte-for-byte the old
/// algorithm (e.g. following a rename in `beam_term`'s public API). It reaches the
/// crate only through the public API, so it cannot accidentally share state with
/// the live implementation.
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
    /// The caller must have reserved the words [`trie_words`] reports for this node
    /// and its sub-nodes (plus, for the head, its `size` word), contiguously from
    /// `*hp`.
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
                    // SAFETY: the caller reserved the `n + 1` words of a collision
                    // tuple at `p`; all of them are written before the pointer escapes.
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
        // SAFETY: the caller reserved `words` words at `p`; the header, the head's
        // `size` word and every slot are written before the pointer escapes.
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

    /// The pre-change `map::write_map`, verbatim: the quadratic per-insertion `eq`
    /// scan on both branches.
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
// Build helpers
// ---------------------------------------------------------------------------

/// A permanently-invalid marker word: no writer may touch it.
const SENTINEL: Eterm = Eterm::from_raw(0xDEAD_BEEF);

/// The two interchangeable writers compared by this file.
type Writer = unsafe fn(*mut Eterm, &[(Eterm, Eterm)], &AtomTable) -> TermResult<Eterm>;

/// A map built into an owned, over-sized buffer.
///
/// The tail beyond [`map_words_capacity`] is filled with [`SENTINEL`], so any write
/// past the documented capacity is caught (the buffer itself is over-sized so the
/// offending write does not corrupt the allocator first).
struct Built {
    buf: Vec<Eterm>,
    cap: usize,
    map: Eterm,
}

impl Built {
    /// The words the map is allowed to occupy.
    fn words(&self) -> &[Eterm] {
        &self.buf[..self.cap]
    }
}

/// Builds `pairs` with `writer`, asserting the capacity contract.
fn build(pairs: &[(Eterm, Eterm)], atoms: &AtomTable, writer: Writer) -> Built {
    let cap = map_words_capacity(pairs.len());
    let mut buf = vec![SENTINEL; cap + 64];
    let base = buf.as_mut_ptr();
    // SAFETY: `buf` reserved `cap` words for the map (plus 64 words of slack), and it
    // is kept alive by the returned `Built`.
    let map = unsafe { writer(base, pairs, atoms) }.expect("a well-formed map");
    for i in 0..64 {
        assert_eq!(
            buf[cap + i],
            SENTINEL,
            "the writer overflowed map_words_capacity({}) by at least {} words",
            pairs.len(),
            i + 1
        );
    }
    Built { buf, cap, map }
}

/// Builds `pairs` with the live [`map::write_map`].
fn build_new(pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Built {
    build(pairs, atoms, map::write_map)
}

/// Builds `pairs` with the retired algorithm (the oracle).
fn build_retired(pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Built {
    build(pairs, atoms, retired::write_map)
}

/// Normalizes a map buffer for base-independent comparison: each word becomes
/// `(true, offset)` when it is a tagged pointer *into* `buf`, else `(false, raw)`.
///
/// Two canonically laid-out maps (identical shape, different heap base) normalize
/// equal; two different shapes never do.
fn normalize(buf: &[Eterm]) -> Vec<(bool, usize)> {
    let base = buf.as_ptr() as usize;
    let end = base + std::mem::size_of_val(buf);
    buf.iter()
        .map(|w| {
            let raw = w.raw();
            // LIST (0x1) and BOXED (0x2) primary tags carry a tagged pointer.
            if raw & 0x3 == 0x1 || raw & 0x3 == 0x2 {
                let p = raw & !0x7usize;
                if p >= base && p < end {
                    return (true, p - base);
                }
            }
            (false, raw)
        })
        .collect()
}

/// Asserts that two independently built maps occupy the same block **word for word**
/// (in-buffer pointers compared by their block-relative offset).
fn assert_same_words(actual: &Built, expected: &Built, what: &str) {
    assert_eq!(
        actual.cap, expected.cap,
        "{what}: the two writers reserve the same capacity"
    );
    let a = normalize(actual.words());
    let b = normalize(expected.words());
    if a != b {
        for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            assert_eq!(
                x, y,
                "{what}: word {i} differs between the live and retired writers"
            );
        }
        panic!(
            "{what}: the buffers have different lengths ({} vs {})",
            a.len(),
            b.len()
        );
    }
}

/// The expected `(key, value)` set of a `write_map` call, computed *without* the map
/// builder: deduplicated with `eq` (first key word wins, last value wins), in the
/// order [`cmp`] gives.
///
/// This is deliberately a plain O(n²) scan — it is an oracle for small inputs, not a
/// hot path.
fn expected_pairs(pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Vec<(Eterm, Eterm)> {
    let mut out: Vec<(Eterm, Eterm)> = Vec::new();
    for &(key, value) in pairs {
        match out.iter_mut().find(|(k, _)| eq(*k, key, atoms)) {
            Some(entry) => entry.1 = value,
            None => out.push((key, value)),
        }
    }
    out.sort_by(|a, b| cmp(a.0, b.0, atoms));
    out
}

/// The full matrix assertion for one row: word equality against the retired writer,
/// the sentinel, the shape, `map_size`, `map_get` for every key, `map_pairs` and the
/// storage order (each key exactly once).
///
/// Returns the live build so a caller can assert more about it.
fn check(pairs: &[(Eterm, Eterm)], atoms: &AtomTable, what: &str) -> Built {
    let new = build_new(pairs, atoms);
    let oracle = build_retired(pairs, atoms);
    assert_same_words(&new, &oracle, what);

    let expected = expected_pairs(pairs, atoms);
    let n = expected.len();

    assert_eq!(map_size(new.map, atoms).unwrap(), n, "{what}: map_size");
    for &(key, value) in &expected {
        assert_eq!(
            map_get(new.map, key, atoms).unwrap(),
            Some(value),
            "{what}: map_get of a key"
        );
        assert_eq!(
            map_get(oracle.map, key, atoms).unwrap(),
            Some(value),
            "{what}: map_get of a key (retired writer)"
        );
    }

    // The layout is chosen by the *distinct* count: <= 32 is always a flat map.
    // SAFETY: `new.map` is a live boxed map.
    let flat = unsafe { is_flatmap(new.map) };
    let hash = unsafe { is_hashmap(new.map) };
    assert!(flat || hash, "{what}: the root must be a map");
    if n <= MAP_SMALL_MAP_LIMIT {
        assert!(flat, "{what}: {n} distinct keys must be a flat map");
    } else {
        assert!(hash, "{what}: {n} distinct keys must be a hash map");
    }

    assert_eq!(
        map_pairs(new.map, atoms).unwrap(),
        expected,
        "{what}: map_pairs (the cmp order) must be exactly the expected key/value set"
    );

    let storage = map_pairs_storage_order(new.map).unwrap();
    assert_eq!(storage.len(), n, "{what}: one stored cell per distinct key");
    for &(key, _) in &expected {
        assert_eq!(
            storage.iter().filter(|(k, _)| eq(*k, key, atoms)).count(),
            1,
            "{what}: a key must occur exactly once in the storage order"
        );
    }

    new
}

// ---------------------------------------------------------------------------
// Key construction (one fixture heap holding every key class)
// ---------------------------------------------------------------------------

/// A fixture heap plus the atom table, holding every key-class term the matrix uses.
///
/// The keys are built once and reused by every row: a key term is just a word, so the
/// same `Eterm` can be a key of many maps.
struct Keys {
    atoms: AtomTable,
    heap: Heap,
    /// Off-heap payloads of the refc-binary keys; kept alive (and unmoved) here.
    payloads: Vec<Vec<usize>>,
}

impl Keys {
    fn new() -> Keys {
        let mut atoms = AtomTable::new();
        atoms.seed(["a", "b", "c", "d", "e", "f", "g", "h"]);
        Keys {
            atoms,
            heap: Heap::new(1 << 16),
            payloads: Vec::new(),
        }
    }

    fn atom(&self, name: &str) -> Eterm {
        make_atom(self.atoms.lookup(name).expect("a seeded atom"))
    }

    fn tuple(&mut self, elems: &[Eterm]) -> Eterm {
        self.heap.tuple(elems).expect("heap room for a tuple")
    }

    /// A proper list of `elems`.
    fn list(&mut self, elems: &[Eterm]) -> Eterm {
        let mut tail = NIL;
        for &e in elems.iter().rev() {
            tail = self.heap.cons(e, tail).expect("heap room for a cons cell");
        }
        tail
    }

    /// An on-heap binary (`ErlHeapBits`) of `len` bytes.
    fn binary(&mut self, len: usize) -> Eterm {
        let bytes: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(31)).collect();
        let words = heap_bits_size_words(bytes.len() * 8);
        // SAFETY: `words` is exactly the block `write_binary` writes, and the heap
        // outlives every key built from it.
        let p = unsafe { self.heap.alloc_words(words) }.expect("heap room for a binary");
        // SAFETY: `p` addresses `words` writable words; `write_binary` writes them all.
        unsafe { write_binary(p, &bytes) }.expect("a well-formed binary")
    }

    /// An off-heap (`refc`) binary of `len` bytes: the shape a compiler-built map holds
    /// for any binary above `ERL_ONHEAP_BINARY_LIMIT`.
    fn refc_binary(&mut self, len: usize) -> Eterm {
        let n_words = len.div_ceil(core::mem::size_of::<usize>());
        let mut payload = vec![0usize; n_words];
        // SAFETY: `payload` covers `len` bytes and the heap outlives the key; the
        // payload's own allocation is word-aligned because it is a `Vec<usize>`.
        unsafe {
            let bytes = core::slice::from_raw_parts_mut(payload.as_mut_ptr() as *mut u8, len);
            for (i, b) in bytes.iter_mut().enumerate() {
                *b = (i as u8).wrapping_mul(17).wrapping_add(3);
            }
        }
        self.payloads.push(payload);
        let payload = self.payloads.last().expect("just pushed");
        let base = payload.as_ptr() as *const u8;
        let val = base as *mut Eterm;
        // SAFETY: 8 words for the `[BinRef][ErlSubBits]` pair, `base`/`val` pointing at
        // the payload's first byte, a null (end-of-chain) `next`, and a word-aligned
        // base; the payload lives in `self.payloads` for the fixture's lifetime.
        let p = unsafe { self.heap.alloc_words(8) }.expect("heap room for a refc binary");
        // SAFETY: as above.
        unsafe { write_refc_binary(p, val, core::ptr::null_mut(), base, 0, len * 8) }
    }

    fn float(&mut self, value: f64) -> Eterm {
        // SAFETY: a float block is 2 words and the heap outlives the key.
        let p = unsafe { self.heap.alloc_words(2) }.expect("heap room for a float");
        // SAFETY: `p` addresses the 2 words `write_float` writes.
        unsafe { write_float(p, value) }
    }

    /// A small nested map, built by the live writer into the fixture heap.
    fn nested_map(&mut self, pairs: &[(Eterm, Eterm)]) -> Eterm {
        let cap = map_words_capacity(pairs.len());
        // SAFETY: `cap` words are reserved from the fixture heap, which outlives the key.
        let p = unsafe { self.heap.alloc_words(cap) }.expect("heap room for a nested map");
        // SAFETY: as above.
        unsafe { map::write_map(p, pairs, &self.atoms) }.expect("a well-formed nested map")
    }
}

/// `(k, k * 10)` for every key in `keys` — distinct small-int keys with distinct values.
fn int_pairs(keys: &[i64]) -> Vec<(Eterm, Eterm)> {
    keys.iter()
        .map(|k| (make_small(*k), make_small(k * 10)))
        .collect()
}

/// `1..=n`.
fn keys_1_to(n: i64) -> Vec<i64> {
    (1..=n).collect()
}

/// A deterministic Fisher-Yates shuffle (xorshift64), so the pseudo-random rows are
/// reproducible on every run and every host.
fn shuffled<T: Copy>(items: &[T], seed: u64) -> Vec<T> {
    let mut out = items.to_vec();
    let mut state = seed | 1;
    for i in (1..out.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let j = (state % (i as u64 + 1)) as usize;
        out.swap(i, j);
    }
    out
}

// ---------------------------------------------------------------------------
// 1. The degenerate rows and the duplicate-key rule
// ---------------------------------------------------------------------------

#[test]
fn zero_pairs_is_the_three_word_empty_flatmap() {
    let k = Keys::new();
    let b = check(&[], &k.atoms, "0 pairs");
    assert_eq!(map_size(b.map, &k.atoms).unwrap(), 0);
    // SAFETY: `b.map` is a live boxed map.
    assert!(unsafe { is_flatmap(b.map) });
    assert_eq!(b.cap, 3, "the empty flat map is exactly 3 words");
    assert_eq!(map_pairs(b.map, &k.atoms).unwrap(), vec![]);
}

#[test]
fn one_pair() {
    let k = Keys::new();
    let b = check(&[(make_small(7), make_small(70))], &k.atoms, "1 pair");
    assert_eq!(map_size(b.map, &k.atoms).unwrap(), 1);
    assert_eq!(
        map_get(b.map, make_small(7), &k.atoms).unwrap(),
        Some(make_small(70))
    );
}

#[test]
fn duplicate_keys_only_last_value_wins() {
    let k = Keys::new();
    let b = check(
        &[
            (make_small(1), make_small(10)),
            (make_small(1), make_small(99)),
        ],
        &k.atoms,
        "dup only",
    );
    assert_eq!(map_size(b.map, &k.atoms).unwrap(), 1);
    assert_eq!(
        map_get(b.map, make_small(1), &k.atoms).unwrap(),
        Some(make_small(99))
    );

    // Two duplicates of the same key, interleaved with nothing.
    let b = check(
        &[
            (make_small(1), make_small(10)),
            (make_small(1), make_small(11)),
            (make_small(1), make_small(12)),
        ],
        &k.atoms,
        "three copies of one key",
    );
    assert_eq!(map_size(b.map, &k.atoms).unwrap(), 1);
    assert_eq!(
        map_get(b.map, make_small(1), &k.atoms).unwrap(),
        Some(make_small(12))
    );
}

#[test]
fn duplicates_interleaved_with_distinct_keys() {
    let a = Keys::new();
    let atoms = &a.atoms;
    let ka = a.atom("a");
    let kb = a.atom("b");
    let kc = a.atom("c");

    // Duplicate first.
    check(
        &[
            (make_small(1), make_small(10)),
            (make_small(1), make_small(11)),
            (make_small(2), make_small(20)),
            (make_small(3), make_small(30)),
        ],
        atoms,
        "dup first",
    );
    // Duplicate in the middle.
    check(
        &[
            (make_small(1), make_small(10)),
            (make_small(2), make_small(20)),
            (make_small(2), make_small(21)),
            (make_small(3), make_small(30)),
        ],
        atoms,
        "dup middle",
    );
    // Duplicate last.
    let b = check(
        &[
            (make_small(1), make_small(10)),
            (make_small(2), make_small(20)),
            (make_small(3), make_small(30)),
            (make_small(3), make_small(31)),
        ],
        atoms,
        "dup last",
    );
    assert_eq!(
        map_get(b.map, make_small(3), atoms).unwrap(),
        Some(make_small(31))
    );
    // Several keys duplicated.
    let b = check(
        &[
            (make_small(1), make_small(1)),
            (make_small(2), make_small(2)),
            (make_small(1), make_small(3)),
            (make_small(3), make_small(3)),
            (make_small(2), make_small(4)),
            (make_small(4), make_small(4)),
            (make_small(3), make_small(5)),
        ],
        atoms,
        "several keys duplicated",
    );
    assert_eq!(map_size(b.map, atoms).unwrap(), 4);
    assert_eq!(
        map_get(b.map, make_small(1), atoms).unwrap(),
        Some(make_small(3))
    );
    assert_eq!(
        map_get(b.map, make_small(2), atoms).unwrap(),
        Some(make_small(4))
    );
    assert_eq!(
        map_get(b.map, make_small(3), atoms).unwrap(),
        Some(make_small(5))
    );
    assert_eq!(
        map_get(b.map, make_small(4), atoms).unwrap(),
        Some(make_small(4))
    );
    // Atom keys duplicated: same rule.
    let b = check(
        &[
            (ka, make_small(1)),
            (kb, make_small(2)),
            (ka, make_small(3)),
            (kc, make_small(4)),
        ],
        atoms,
        "atom keys, one duplicated",
    );
    assert_eq!(map_size(b.map, atoms).unwrap(), 3);
    assert_eq!(map_get(b.map, ka, atoms).unwrap(), Some(make_small(3)));
}

// ---------------------------------------------------------------------------
// 2. Every key class
// ---------------------------------------------------------------------------

#[test]
fn non_atom_keys_across_every_class() {
    let mut k = Keys::new();
    let ka = k.atom("a");
    let kb = k.atom("b");
    let tup_a = k.tuple(&[make_small(1), make_small(2)]);
    let tup_b = k.tuple(&[ka]);
    let list_a = k.list(&[make_small(1), make_small(2), make_small(3)]);
    let list_b = k.list(&[kb, tup_b]);
    let bin_1 = k.binary(1);
    let bin_8 = k.binary(8);
    let bin_33 = k.binary(33);
    let refc = k.refc_binary(96);
    let f1 = k.float(1.5);
    let f2 = k.float(-0.0);
    let f3 = k.float(0.0);
    let nested = k.nested_map(&[(make_small(1), make_small(2)), (ka, kb)]);
    let atoms = k.atoms.clone();

    // One row per class, with a duplicate in each (so the dedup path is exercised for
    // that class, not just the insertion path).
    let rows: Vec<(&str, Vec<(Eterm, Eterm)>)> = vec![
        ("small ints", int_pairs(&keys_1_to(4))),
        (
            "atoms",
            vec![
                (ka, make_small(1)),
                (kb, make_small(2)),
                (ka, make_small(3)),
            ],
        ),
        (
            "tuples",
            vec![
                (tup_a, make_small(1)),
                (tup_b, make_small(2)),
                (tup_a, make_small(3)),
            ],
        ),
        (
            "lists",
            vec![
                (list_a, make_small(1)),
                (list_b, make_small(2)),
                (list_a, make_small(3)),
            ],
        ),
        (
            "binaries",
            vec![
                (bin_1, make_small(1)),
                (bin_8, make_small(2)),
                (bin_33, make_small(3)),
                (bin_8, make_small(4)),
            ],
        ),
        (
            "refc binary",
            vec![
                (refc, make_small(1)),
                (bin_33, make_small(2)),
                (refc, make_small(3)),
            ],
        ),
        (
            "floats",
            vec![
                (f1, make_small(1)),
                (f2, make_small(2)),
                (f3, make_small(3)),
                (f1, make_small(4)),
            ],
        ),
        (
            "nested maps",
            vec![
                (nested, make_small(1)),
                (tup_b, make_small(2)),
                (nested, make_small(3)),
            ],
        ),
        (
            "mixed classes",
            vec![
                (make_small(1), make_small(10)),
                (ka, make_small(11)),
                (tup_a, make_small(12)),
                (list_a, make_small(13)),
                (bin_8, make_small(14)),
                (refc, make_small(15)),
                (f1, make_small(16)),
                (nested, make_small(17)),
                (bin_1, make_small(18)),
                (list_b, make_small(19)),
                (tup_b, make_small(20)),
                (f3, make_small(21)),
                (make_small(1), make_small(22)),
                (ka, make_small(23)),
            ],
        ),
    ];
    for (what, pairs) in &rows {
        let b = check(pairs, &atoms, what);
        assert_eq!(
            map_size(b.map, &atoms).unwrap(),
            expected_pairs(pairs, &atoms).len(),
            "{what}: map_size"
        );
    }
}

/// The same mixed-class corpus **above** `MAP_SMALL_MAP_LIMIT`, so the hash branch's
/// quadratic scan (and its trie serialization) is covered with every key class.
#[test]
fn non_atom_keys_above_the_small_map_limit() {
    let mut k = Keys::new();
    let ka = k.atom("a");
    let tup = k.tuple(&[make_small(1), make_small(2)]);
    let list = k.list(&[make_small(9), make_small(8)]);
    let bin = k.binary(12);
    let refc = k.refc_binary(96);
    let f = k.float(2.5);
    let nested = k.nested_map(&[(ka, make_small(1))]);
    let atoms = k.atoms.clone();

    let mut pairs: Vec<(Eterm, Eterm)> = int_pairs(&keys_1_to(40));
    pairs.push((ka, make_small(1)));
    pairs.push((tup, make_small(2)));
    pairs.push((list, make_small(3)));
    pairs.push((bin, make_small(4)));
    pairs.push((refc, make_small(5)));
    pairs.push((f, make_small(6)));
    pairs.push((nested, make_small(7)));
    // Duplicates of two of the non-int keys, last value wins.
    pairs.push((ka, make_small(11)));
    pairs.push((tup, make_small(22)));
    let b = check(&pairs, &atoms, "47 mixed-class keys");
    assert_eq!(map_size(b.map, &atoms).unwrap(), 47);
    assert_eq!(map_get(b.map, ka, &atoms).unwrap(), Some(make_small(11)));
    assert_eq!(map_get(b.map, tup, &atoms).unwrap(), Some(make_small(22)));
}

// ---------------------------------------------------------------------------
// 3. Insertion order
// ---------------------------------------------------------------------------

#[test]
fn insertion_order_does_not_matter() {
    let k = Keys::new();
    let atoms = &k.atoms;
    // A small (flat) set and a large (hash) set, each in forward / reversed /
    // pseudo-random order.
    for n in [3usize, 32, 40, 100] {
        let forward = int_pairs(&keys_1_to(n as i64));
        let reversed: Vec<_> = forward.iter().rev().copied().collect();
        let scrambled = shuffled(&forward, 0x5eed_1234);

        let a = build_new(&forward, atoms);
        let b = build_new(&reversed, atoms);
        let c = build_new(&scrambled, atoms);

        assert_same_words(&a, &b, &format!("{n} keys, reversed input"));
        assert_same_words(&a, &c, &format!("{n} keys, shuffled input"));
        assert_eq!(
            cmp(a.map, b.map, atoms),
            core::cmp::Ordering::Equal,
            "{n} keys: a reversed input builds an =:= map"
        );
        assert_eq!(
            cmp(a.map, c.map, atoms),
            core::cmp::Ordering::Equal,
            "{n} keys: a shuffled input builds an =:= map"
        );
    }
}

#[test]
fn insertion_order_does_not_matter_with_repeated_identical_pairs() {
    let k = Keys::new();
    let atoms = &k.atoms;
    // 40 input pairs holding 8 *exact repeats* of an earlier pair (same key, same
    // value), so the distinct count (32) crosses back BELOW `MAP_SMALL_MAP_LIMIT`: the
    // hash branch must produce a *flat* map — identically from any input order, because
    // a repeated identical pair cannot change which value wins.
    let mut forward: Vec<(Eterm, Eterm)> = int_pairs(&keys_1_to(32));
    for i in 1..=8i64 {
        forward.push((make_small(i), make_small(i * 10)));
    }
    let reversed: Vec<_> = forward.iter().rev().copied().collect();
    let scrambled = shuffled(&forward, 0xabc_def);

    let a = check(&forward, atoms, "40 input / 32 distinct, forward");
    assert_eq!(map_size(a.map, atoms).unwrap(), 32);
    // SAFETY: `a.map` is a live boxed map.
    assert!(
        unsafe { is_flatmap(a.map) },
        "32 distinct keys must be a flat map"
    );
    assert_same_words(&a, &build_new(&reversed, atoms), "40/32 reversed");
    assert_same_words(&a, &build_new(&scrambled, atoms), "40/32 shuffled");
}

/// "Last value wins" is relative to the **input order** — that is the documented
/// `erts_map_put` rule, not an order-independence property. Reversing an input whose
/// duplicates carry *different* values therefore changes which value wins, and the two
/// builders must agree on it in both directions.
#[test]
fn a_repeated_key_takes_the_value_of_its_last_occurrence_in_input_order() {
    let k = Keys::new();
    let atoms = &k.atoms;
    let forward = [
        (make_small(1), make_small(10)),
        (make_small(2), make_small(20)),
        (make_small(1), make_small(99)),
    ];
    let reversed: Vec<_> = forward.iter().rev().copied().collect();

    let fwd = check(&forward, atoms, "last value wins, forward");
    assert_eq!(
        map_get(fwd.map, make_small(1), atoms).unwrap(),
        Some(make_small(99))
    );
    let rev = check(&reversed, atoms, "last value wins, reversed");
    assert_eq!(
        map_get(rev.map, make_small(1), atoms).unwrap(),
        Some(make_small(10))
    );
    // Both are the same two-key flat map *shape*; only the surviving value differs.
    assert_eq!(map_size(fwd.map, atoms).unwrap(), 2);
    assert_eq!(map_size(rev.map, atoms).unwrap(), 2);
}
// ---------------------------------------------------------------------------
// 4. Layout boundaries and the dedup crossover
// ---------------------------------------------------------------------------

#[test]
fn layout_boundaries() {
    let k = Keys::new();
    for n in [31usize, 32, 33, 34, 40, 100, 1000] {
        let pairs = int_pairs(&keys_1_to(n as i64));
        let b = check(&pairs, &k.atoms, &format!("{n} distinct keys"));
        assert_eq!(map_size(b.map, &k.atoms).unwrap(), n);
    }
}

#[test]
fn the_dedup_crossover_picks_the_layout_from_the_distinct_count() {
    let k = Keys::new();
    let atoms = &k.atoms;

    // 33 input pairs, one duplicate => 32 distinct => FLAT.
    let mut pairs = int_pairs(&keys_1_to(32));
    pairs.push((make_small(1), make_small(111)));
    let b = check(&pairs, atoms, "33 input / 32 distinct");
    assert_eq!(map_size(b.map, atoms).unwrap(), 32);
    // SAFETY: `b.map` is a live boxed map.
    assert!(unsafe { is_flatmap(b.map) });
    assert_eq!(
        map_get(b.map, make_small(1), atoms).unwrap(),
        Some(make_small(111))
    );

    // 33 input pairs, two duplicates => 31 distinct => FLAT.
    let mut pairs = int_pairs(&keys_1_to(31));
    pairs.push((make_small(1), make_small(111)));
    pairs.push((make_small(2), make_small(222)));
    let b = check(&pairs, atoms, "33 input / 31 distinct");
    assert_eq!(map_size(b.map, atoms).unwrap(), 31);
    // SAFETY: `b.map` is a live boxed map.
    assert!(unsafe { is_flatmap(b.map) });

    // 34 input pairs, one duplicate => 33 distinct => HASH.
    let mut pairs = int_pairs(&keys_1_to(33));
    pairs.push((make_small(33), make_small(333)));
    let b = check(&pairs, atoms, "34 input / 33 distinct");
    assert_eq!(map_size(b.map, atoms).unwrap(), 33);
    // SAFETY: `b.map` is a live boxed map.
    assert!(unsafe { is_hashmap(b.map) });

    // 40 input pairs, 8 duplicates => 32 distinct => FLAT.
    let mut pairs = int_pairs(&keys_1_to(32));
    for i in 1..=8i64 {
        pairs.push((make_small(i), make_small(4000 + i)));
    }
    let b = check(&pairs, atoms, "40 input / 32 distinct");
    assert_eq!(map_size(b.map, atoms).unwrap(), 32);
    // SAFETY: `b.map` is a live boxed map.
    assert!(unsafe { is_flatmap(b.map) });
}

// ---------------------------------------------------------------------------
// 5. Exact (=:=) key identity: the pairs `eq` must NOT collapse
// ---------------------------------------------------------------------------

/// `1` and `1.0` are different keys, and so are `0.0` and `-0.0` (`0.0 =:= -0.0` is
/// `false` in Erlang — only `==` equates them, and map keys use `=:=`). Both pairs
/// must therefore survive dedup, each with its own value.
///
/// Oracle-checked against the system `erl`:
/// `maps:to_list(#{0.0 => a, -0.0 => b})` → `[{-0.0,b},{0.0,a}]` (size 2),
/// `maps:to_list(#{1 => a, 1.0 => b})` → `[{1,a},{1.0,b}]` (size 2).
#[test]
fn exact_key_identity_is_not_collapsed_by_dedup() {
    let mut k = Keys::new();
    let zero = k.float(0.0);
    let neg_zero = k.float(-0.0);
    let one_float = k.float(1.0);
    let atoms = k.atoms.clone();

    let b = check(
        &[(zero, make_small(1)), (neg_zero, make_small(2))],
        &atoms,
        "0.0 / -0.0",
    );
    assert_eq!(
        map_size(b.map, &atoms).unwrap(),
        2,
        "0.0 and -0.0 are two distinct keys"
    );
    assert_eq!(map_get(b.map, zero, &atoms).unwrap(), Some(make_small(1)));
    assert_eq!(
        map_get(b.map, neg_zero, &atoms).unwrap(),
        Some(make_small(2))
    );

    let b = check(
        &[(make_small(1), make_small(1)), (one_float, make_small(2))],
        &atoms,
        "1 / 1.0",
    );
    assert_eq!(
        map_size(b.map, &atoms).unwrap(),
        2,
        "1 and 1.0 are two distinct keys"
    );
    assert_eq!(
        map_get(b.map, make_small(1), &atoms).unwrap(),
        Some(make_small(1))
    );
    assert_eq!(
        map_get(b.map, one_float, &atoms).unwrap(),
        Some(make_small(2))
    );

    // The same pair must survive the hash branch too, where a `u64` pre-filter sits in
    // front of `eq`: 40 int keys plus both float pairs.
    let mut pairs = int_pairs(&keys_1_to(40));
    pairs.push((zero, make_small(1)));
    pairs.push((neg_zero, make_small(2)));
    pairs.push((make_small(41), make_small(3)));
    pairs.push((one_float, make_small(4)));
    let b = check(&pairs, &atoms, "hash branch: 0.0 / -0.0 / 1 / 1.0");
    assert_eq!(map_size(b.map, &atoms).unwrap(), 44);
    assert_eq!(map_get(b.map, zero, &atoms).unwrap(), Some(make_small(1)));
    assert_eq!(
        map_get(b.map, neg_zero, &atoms).unwrap(),
        Some(make_small(2))
    );
    assert_eq!(
        map_get(b.map, one_float, &atoms).unwrap(),
        Some(make_small(4))
    );
}

// ---------------------------------------------------------------------------
// 6. The premise of the coming rewrite: eq-equal keys sort adjacent
// ---------------------------------------------------------------------------

/// The adjacent-run collapse is only correct if, for *either* branch's sort key,
/// `eq`-equal keys compare `Equal` — then a **stable** sort keeps them adjacent and in
/// input order, so "first key word of the run, last value of the run" is exactly what
/// the per-insertion scan produces today.
///
/// This pins that premise for a corpus spanning every key class, for the flat branch's
/// key ([`cmp_flatmap_keys`]) and the hash branch's key
/// (`(swizzle_map_hash(erts_map_hash(..)), cmp)`).
#[test]
fn eq_equal_keys_are_adjacent_after_the_branch_sort_passes() {
    let mut k = Keys::new();
    let ka = k.atom("a");
    let tup = k.tuple(&[make_small(1), make_small(2)]);
    let tup_copy = k.tuple(&[make_small(1), make_small(2)]); // structurally equal, distinct words
    let list = k.list(&[make_small(1), make_small(2)]);
    let list_copy = k.list(&[make_small(1), make_small(2)]);
    let bin = k.binary(9);
    let bin_copy = k.binary(9);
    let nested = k.nested_map(&[(ka, make_small(1)), (make_small(2), make_small(3))]);
    let nested_copy = k.nested_map(&[(make_small(2), make_small(3)), (ka, make_small(1))]);
    let atoms = k.atoms.clone();

    // Distinct-by-word key pairs that are `=:=` (the pairs a stable sort must keep in
    // input order and the collapse must merge).
    let twins: Vec<(Eterm, Eterm)> = vec![
        (tup, tup_copy),
        (list, list_copy),
        (bin, bin_copy),
        (nested, nested_copy),
    ];
    for (a, b) in &twins {
        assert_eq!(
            cmp(*a, *b, &atoms),
            core::cmp::Ordering::Equal,
            "the fixture pair must be =:= (otherwise this test is vacuous)"
        );
        assert!(eq(*a, *b, &atoms));
    }

    // The flat branch's key.
    let mut flat_keys: Vec<Eterm> = int_pairs(&keys_1_to(20)).iter().map(|p| p.0).collect();
    flat_keys.push(ka);
    flat_keys.push(k.float(1.5));
    for (a, b) in &twins {
        flat_keys.push(*a);
        flat_keys.push(*b);
    }
    flat_keys.sort_by(|x, y| cmp_flatmap_keys(*x, *y, &atoms));
    assert_eq_equal_keys_adjacent(&flat_keys, &atoms, "cmp_flatmap_keys");

    // The hash branch's key.
    let mut hash_keys: Vec<Eterm> = int_pairs(&keys_1_to(40)).iter().map(|p| p.0).collect();
    for (a, b) in &twins {
        hash_keys.push(*a);
        hash_keys.push(*b);
    }
    hash_keys.sort_by(|x, y| {
        swizzle_map_hash(erts_map_hash(*x))
            .cmp(&swizzle_map_hash(erts_map_hash(*y)))
            .then_with(|| cmp(*x, *y, &atoms))
    });
    assert_eq_equal_keys_adjacent(&hash_keys, &atoms, "swizzle(erts_map_hash) + cmp");

    // ... and the stable sort preserves the *input* order inside each equal run, which
    // is what makes the run's first key word the same word the retired scan keeps.
    let mut raw: Vec<(Eterm, Eterm)> = Vec::new();
    for (i, (a, b)) in twins.iter().enumerate() {
        raw.push((*a, make_small(i as i64)));
        raw.push((*b, make_small(100 + i as i64)));
    }
    let mut sorted = raw.clone();
    sorted.sort_by(|x, y| cmp_flatmap_keys(x.0, y.0, &atoms));
    for (a, _) in &twins {
        let first = sorted
            .iter()
            .find(|(key, _)| eq(*key, *a, &atoms))
            .expect("every fixture key is in the corpus");
        assert_eq!(
            first.0, *a,
            "a stable sort keeps the FIRST input key word of an equal run"
        );
    }
}

/// Asserts that every `eq`-equal group of a sorted key list is contiguous — i.e. that
/// no non-equal key is sorted between two equal ones.
fn assert_eq_equal_keys_adjacent(keys: &[Eterm], atoms: &AtomTable, what: &str) {
    for i in 0..keys.len() {
        // Find the end of the run starting at `i`.
        let mut j = i;
        while j < keys.len() && eq(keys[i], keys[j], atoms) {
            j += 1;
        }
        // No later key may be equal to `keys[i]`.
        for (offset, key) in keys[j..].iter().enumerate() {
            assert!(
                !eq(keys[i], *key, atoms),
                "{what}: an =:= key is not adjacent (run starting at {i} is broken at \
                 {})",
                j + offset
            );
        }
        // Continue after the run.
        let _ = j;
    }
}

// ---------------------------------------------------------------------------
// 7. Hard-coded goldens captured from the pre-change implementation
// ---------------------------------------------------------------------------

/// A small flat map built from `[(a,1), (b,2), (a,3)]`: two keys, `a`'s value is the
/// last one. Captured from the implementation **before** the rewrite, so it is an
/// oracle independent of both writers in this file.
///
/// The block is `[arityval(2), key_a, key_b, MAP_HEADER_FLATMAP, 2, keys-tuple,
/// value_a, value_b]` with the root at word 3 and the keys tuple pointing at word 0.
#[test]
fn golden_two_atom_keys_with_a_duplicate() {
    let k = Keys::new();
    let pairs = [
        (k.atom("a"), make_small(1)),
        (k.atom("b"), make_small(2)),
        (k.atom("a"), make_small(3)),
    ];
    let b = check(&pairs, &k.atoms, "golden: [a, b, a]");
    assert_eq!(normalize(b.words()), GOLDEN_TWO_KEYS.to_vec());
}

/// A three-key flat map with a duplicate in the middle:
/// `[(1,10), (2,20), (1,11), (3,30)]`.
#[test]
fn golden_three_int_keys_with_a_duplicate() {
    let k = Keys::new();
    let pairs = [
        (make_small(1), make_small(10)),
        (make_small(2), make_small(20)),
        (make_small(1), make_small(11)),
        (make_small(3), make_small(30)),
    ];
    let b = check(&pairs, &k.atoms, "golden: [1, 2, 1, 3]");
    assert_eq!(normalize(b.words()), GOLDEN_THREE_KEYS.to_vec());
}

/// The same three-key set with **no** duplicate keys, fed in reverse order: the used 10
/// words are byte-identical to [`GOLDEN_THREE_KEYS`]'s, because the pair order of the
/// input is irrelevant (only a repeated key with a *different* value is order
/// sensitive — see
/// [`a_repeated_key_takes_the_value_of_its_last_occurrence_in_input_order`]).
///
/// The normalized vectors differ in *length* only because the reserved block is sized
/// from the **input** arity (`flatmap_size_words(3) == 10` here vs `4` input pairs'
/// `12`), so the golden's two sentinel words are simply absent.
#[test]
fn golden_three_int_keys_reversed_without_duplicates_is_the_same_map() {
    let k = Keys::new();
    let forward = [
        (make_small(1), make_small(11)),
        (make_small(2), make_small(20)),
        (make_small(3), make_small(30)),
    ];
    let reversed: Vec<_> = forward.iter().rev().copied().collect();
    let fwd = build_new(&forward, &k.atoms);
    let rev = build_new(&reversed, &k.atoms);
    assert_eq!(fwd.cap, 10);
    assert_same_words(&fwd, &rev, "3 distinct keys, reversed input");
    assert_eq!(normalize(rev.words()), GOLDEN_THREE_KEYS[..10].to_vec());
}
// ---------------------------------------------------------------------------
// 8. The practical quadratic guard
// ---------------------------------------------------------------------------

/// 20 000 distinct keys interleaved with 20 000 duplicates, asserted on the **output**
/// only — never on elapsed time.
///
/// The retired per-insertion scan needs on the order of `2 * 10^8` comparisons for this
/// input (each of the 40 000 insertions scans up to 20 000 existing entries), but on the
/// hash branch the `u64` pre-filter makes those comparisons nearly free: measured on this
/// host the whole row takes ~0.12 s. So this is a **correctness** guard at a size where a
/// per-insertion scan is still affordable — it pins that a rewritten dedup (one stable
/// sort + one linear collapse) produces the same key set, the same values (`last value
/// wins`), one stored cell per key and the same `map` order for a large input. It is *not*
/// a timing guard; the elapsed-time signal for the O(n²)→O(n log n) fix lives in
/// `examples/map_build_bench.rs`, whose `21-32` bucket already costs ~4.7 µs per map.
#[test]
fn twenty_thousand_distinct_keys_interleaved_with_duplicates() {
    const N: i64 = 20_000;
    let k = Keys::new();
    let mut input: Vec<(Eterm, Eterm)> = Vec::with_capacity(2 * N as usize);
    for key in 1..=N {
        input.push((make_small(key), make_small(key * 10)));
        // The duplicate immediately after its original: the last value wins.
        input.push((make_small(key), make_small(key * 10 + 1)));
    }
    assert_eq!(input.len(), 40_000);

    let b = build_new(&input, &k.atoms);
    assert_eq!(map_size(b.map, &k.atoms).unwrap(), N as usize);
    // SAFETY: `b.map` is a live boxed map.
    assert!(
        unsafe { is_hashmap(b.map) },
        "20 000 distinct keys is a hash map"
    );

    // Every key resolves to its winning value.
    for key in 1..=N {
        assert_eq!(
            map_get(b.map, make_small(key), &k.atoms).unwrap(),
            Some(make_small(key * 10 + 1)),
            "key {key}"
        );
    }

    // One stored cell per key: `map_pairs` is in `cmp` order, which for 1..=N distinct
    // small ints is the numeric ascending order — so the exact sequence pins both the
    // count and the "each key exactly once" property in one assertion.
    let pairs = map_pairs(b.map, &k.atoms).unwrap();
    assert_eq!(pairs.len(), N as usize);
    for (i, (key, value)) in pairs.iter().enumerate() {
        assert_eq!(*key, make_small(i as i64 + 1), "ascending key {i}");
        assert_eq!(
            *value,
            make_small((i as i64 + 1) * 10 + 1),
            "winning value {i}"
        );
    }
    assert_eq!(map_pairs_storage_order(b.map).unwrap().len(), N as usize);
}

// ---------------------------------------------------------------------------
// The captured golden vectors
// ---------------------------------------------------------------------------

/// The marker still in the unused tail of a reserved block (`SENTINEL.raw()`), pinned
/// literally here: it also proves a writer leaves the words past the map's own size
/// untouched.
const SENTINEL_RAW: usize = 0xDEAD_BEEF;

/// `write_map` of `[(a,1), (b,2), (a,3)]`, normalized to
/// `(is_pointer_into_the_block, offset_or_raw_word)`.
///
/// Captured from the implementation **before** the O(n²) fix, so it is an oracle
/// independent of both writers in this file.
///
/// The 8 used words of the 10-word reserved block are
/// `[arityval(2), a, b, MAP_HEADER_FLATMAP, size, keys-tuple, value-a, value-b]`;
/// `value-a == 3` is the *last* value of the duplicated key. The last two words are
/// the untouched sentinel (dedup shrank 3 input pairs to 2).
const GOLDEN_TWO_KEYS: [(bool, usize); 10] = [
    (false, 0x80),  // arityval(2) == 2 << 6
    (false, 0xB),   // key   a == atom index 0
    (false, 0x4B),  // key   b == atom index 1  (1 << 6 | 0xB)
    (false, 0x12C), // MAP_HEADER_FLATMAP == _make_header(4, MAP_SUBTAG)
    (false, 0x2),   // size
    (true, 0x0),    // keys tuple -> block base
    (false, 0x3F),  // value a == 3
    (false, 0x2F),  // value b == 2
    (false, SENTINEL_RAW),
    (false, SENTINEL_RAW),
];

/// `write_map` of `[(1,10), (2,20), (1,11), (3,30)]`, normalized: three distinct keys,
/// `1`'s value is the last one (11). The last two words are the untouched sentinel
/// (dedup shrank 4 input pairs to 3).
const GOLDEN_THREE_KEYS: [(bool, usize); 12] = [
    (false, 0xC0),  // arityval(3) == 3 << 6
    (false, 0x1F),  // key   1
    (false, 0x2F),  // key   2
    (false, 0x3F),  // key   3
    (false, 0x12C), // MAP_HEADER_FLATMAP
    (false, 0x3),   // size
    (true, 0x0),    // keys tuple -> block base
    (false, 0xBF),  // value 1 == 11
    (false, 0x14F), // value 2 == 20
    (false, 0x1EF), // value 3 == 30
    (false, SENTINEL_RAW),
    (false, SENTINEL_RAW),
];
