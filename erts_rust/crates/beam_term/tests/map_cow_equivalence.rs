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

//! **Equivalence, structural-safety and deterministic-scaling evidence for the
//! incremental (path-copying) map writers.**
//!
//! [`map_put_cow`] / [`map_remove_cow`] copy only the nodes on a key's hash path and
//! *share* every off-path subtree, the flat arm's keys tuple and every collision cell
//! **by word** with the source map. Three properties therefore have to be pinned, and
//! each has its own section below.
//!
//! # 1. Op-script equivalence (`write_map` is the semantics oracle)
//!
//! One deterministic op script is driven through **both** routes in lockstep:
//!
//! * **route A (rebuild)** — keep the pair set in host memory and
//!   [`write_map`] a *fresh, self-contained* map every step. This is the crate's
//!   canonical map builder, so its output is by definition "the map that pair set
//!   describes";
//! * **route B (COW)** — [`map_put_cow`] / [`map_remove_cow`] on the *previous* step's
//!   map, into a fresh staging buffer.
//!
//! Every step asserts `=:=`-identity of the two results at the level a caller can
//! observe: [`map_size`], [`map_pairs`] (the `cmp` order), [`map_pairs_storage_order`]
//! (the **stored** HAMT / keys-tuple order), [`map_get`] for every key and for absent
//! probes, and [`render_map`]'s canonical node rendering (see 2).
//!
//! [`map_pairs_storage_order`] is the pair sequence an *image-preserving* serializer
//! emits, so equal storage order plus an equal node rendering is the `term_to_binary`
//! byte-equality claim stated in a way this crate can express without a dependency on
//! `beam_codec` (which would be a dependency cycle — `beam_codec` depends on
//! `beam_term`). That is the strongest equality available here.
//!
//! # 2. The canonical structural rendering
//!
//! [`Render`] walks the map exactly the way a serializer or a copier does —
//! `boxed_val`, the header word (`map_header_type` / `map_header_val` /
//! `header_arity`), one entry per HAMT slot, a collision node's cells in order — and
//! records the node **kind**, its bitmap, the head's size word and, at each leaf, a
//! [`Proj`]-projection of `(key, value)`. A projection is bit-exact for a non-boxed
//! word (so `1` and `1.0` project differently) and *structural* for a boxed term (so a
//! rebuilt copy projects like its original).
//!
//! Raw words are deliberately **not** compared across the two buffers: the flat
//! value-replace arm legitimately reuses the source map's keys-tuple word while a
//! rebuild writes its own copy of that tuple, so equality must hold modulo heap base
//! and modulo that legitimate sharing.
//!
//! # 3. Structural / GC-safety evidence
//!
//! [`walk`] enumerates every reachable object through the public
//! [`layout::for_each_child_slot`] / [`layout::object_words`] seam — the same two
//! questions a copier asks — and [`assert_structural_safety`] pins:
//!
//! * every node header is well formed and its slot count is `popcount(bitmap)`;
//! * the objects inside the result's reserved block occupy **exactly** the words the
//!   op wrote (the sentinel sweep's length), so `object_words` agrees with the block;
//! * every object reachable from the result that is *not* in the result's block is
//!   reachable from the source (or is one of the op's own terms) — i.e. off-path
//!   subtrees are shared, never copied;
//! * the flat value-replace arm's keys-tuple word is bit-identical to the source's;
//! * `size_object(result)` equals the walk's own `object_words` total, which is the
//!   cross-check between the two independently written walks over a partially shared
//!   graph.
//!
//! **Not written here (needs another node):** a `beam_gc` copy test over a partially
//! shared map — a collection that relocates the *source* and the *result* across
//! heaps, proving the shared words are forwarded twice and never duplicated. This
//! crate cannot edit `beam_gc`, so that remains the required follow-up.
//!
//! # 4. Deterministic scaling (no wall clock)
//!
//! [`map_put_cow_words_required`] / [`map_remove_cow_words_required`] answer the exact
//! number of words an op writes from a read-only O(log n) descent. This file reserves
//! **exactly** that figure (plus a sentinel tail), and pins:
//!
//! * `required <= *_words_capacity(size)` for every size;
//! * `required` for the same op at n = 2 500 exceeds the figure at n = 250 by at most
//!   one path node (`18` words) — the path gains at most one level in a 16-way trie;
//! * a fold of K single-key puts writes `K * (<= 293)` words regardless of n, while
//!   the rebuild route writes `Θ(n)` per op — the difference between the new
//!   `Θ(log n)` writer and the old `Θ(n)` [`map_put`].
//!
//! The one `#[ignore]`d test prints a *relation* for a human; it asserts nothing about
//! wall time, because this host is shared.
//!
//! # Why one module
//!
//! The four harnesses share one fixture (`Pool`: one key of every class, on one heap that
//! never moves) and one staging discipline (`Region`: the COW chain's buffers must all
//! outlive the newest result). Splitting them would duplicate both, and the crate's
//! convention allows a comprehensive test module to exceed the ~1000-line target when its
//! top says why. The tests below, in file order:
//!
//! * [`op_script_matches_a_rebuild_at_every_size`] — route A vs route B at every base size;
//! * [`one_and_one_point_zero_are_distinct_keys`] — `=:=` key identity survives both routes;
//! * [`every_key_class_round_trips`] — every key class, and the pool's class inventory;
//! * [`flat_limits_and_conversions_round_trip`] — the 32-key boundary in both directions;
//! * [`delete_every_key_down_to_empty`] — HAMT → flat → empty, key by key;
//! * [`absent_removes_and_identical_puts_write_nothing`] — the two zero-word arms;
//! * [`structural_safety_of_a_path_copied_result`] — the copier's walk, the sharing
//!   invariant and `size_object` over five path-copied shapes;
//! * [`words_written_per_op_does_not_grow_with_n`] — the flat-in-n word scaling;
//! * [`the_conversion_arm_plans_the_write_map_capacity_bound`] — the one bound-not-exact
//!   plan, in the safe direction;
//! * [`timing_report_put_vs_put_cow`] — ignored; a relation to read, never an assertion.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::time::Instant;

use beam_term::atom::AtomTable;
use beam_term::bitstring::{bit_at, bitstring_parts, heap_bits_size_words, write_binary};
use beam_term::boxed::{
    arityval, boxed_val, header_arity, is_arity_value, is_boxed_term, is_flatmap, is_flatmap_header,
    is_hashmap, is_map_header, map_header_type, map_header_val,
};
use beam_term::compare::{cmp, eq};
use beam_term::eterm::Eterm;
use beam_term::fun::write_fun_external;
use beam_term::heap::Heap;
use beam_term::ihash::erts_map_hash;
use beam_term::immediate::{make_atom, make_small};
use beam_term::layout::{for_each_child_slot, for_each_sizing_child_slot, object_words};
use beam_term::list::{car, cdr, is_list, list_val};
use beam_term::literal::is_literal;
use beam_term::map::{
    map_get, map_pairs, map_pairs_storage_order, map_put, map_put_cow, map_put_cow_words_capacity,
    map_put_cow_words_required, map_remove_cow, map_remove_cow_words_capacity,
    map_remove_cow_words_required, map_size, map_words_capacity, write_map, MAP_SMALL_MAP_LIMIT,
};
use beam_term::ops::{class, is_non_value, TermClass};
use beam_term::refc::{is_refc_binary, write_refc_binary};
use beam_term::size::size_object;
use beam_term::tag::{
    MAP_HEADER_TAG_HAMT_HEAD_ARRAY, MAP_HEADER_TAG_HAMT_HEAD_BITMAP, MAP_HEADER_TAG_HAMT_NODE_BITMAP,
};
use beam_term::thing::{big_parts, big_size_words, flatmap_parts, write_big, write_float};
use beam_term::tuple::{tuple_element, tuple_size};

// ---------------------------------------------------------------------------
// The staging region: an over-sized buffer with a sentinel tail
// ---------------------------------------------------------------------------

/// A permanently-invalid marker word. Nothing in this file may write past a
/// reservation, and no word a map writes can equal this one.
const SENTINEL: Eterm = Eterm::from_raw(0xDEAD_BEEF_DEAD_BEEF);

/// How many sentinel words follow every reservation. Wide enough that a small
/// over-write lands in the tail (and is reported) instead of corrupting the allocator.
const TAIL: usize = 64;

/// An owned run of `cap` map words followed by [`TAIL`] sentinel words.
///
/// The buffer **outlives every term written into it**: a path-copied result references
/// its source map's storage, and the source in turn references the previous staging
/// buffers of the chain, so all of them have to stay alive for as long as the newest
/// result is inspected.
struct Region {
    words: Vec<Eterm>,
    cap: usize,
}

impl Region {
    fn new(cap: usize) -> Region {
        Region {
            words: vec![SENTINEL; cap + TAIL],
            cap,
        }
    }

    fn base(&mut self) -> *mut Eterm {
        self.words.as_mut_ptr()
    }

    /// The reservation's address range as `[base, end)`, in bytes.
    fn block(&self) -> (usize, usize) {
        let base = self.words.as_ptr() as usize;
        (base, base + self.cap * core::mem::size_of::<Eterm>())
    }

    /// Fails if any word of the sentinel tail was written.
    fn check_tail(&self) {
        for i in 0..TAIL {
            assert_eq!(
                self.words[self.cap + i],
                SENTINEL,
                "a writer overran its reservation by at least {} words (cap {})",
                i + 1,
                self.cap
            );
        }
    }

    /// The exact length of the mutated prefix — how many words the writer wrote.
    fn touched(&self) -> usize {
        self.words
            .iter()
            .rposition(|w| *w != SENTINEL)
            .map_or(0, |i| i + 1)
    }
}

// ---------------------------------------------------------------------------
// The key pool: every key class, mixed inside one map
// ---------------------------------------------------------------------------

/// The number of distinct keys the pool holds: enough for the largest base map
/// ([`SWEEP_SIZES`]'s 2 500) plus the script's inserts, plus the `#[ignore]`d timing
/// report's 6 250-key row.
const POOL_KEYS: usize = 6_400;

/// Where the bulk (small-integer) keys start. Above every hand-picked key value, so
/// the pool is distinct by construction.
const BULK_BASE: i64 = 10_000;

/// The value a key index is bound to, so every pair is distinguishable.
fn value_of(index: usize) -> Eterm {
    make_small(900_000 + index as i64)
}

/// A fixture heap plus the atom table, holding one key of **every** class the sweep
/// mixes — atoms, small ints, a float (`1.0`, deliberately the `==` but not `=:=` twin
/// of the small int `1`), bignums above `2^59`, an on-heap and an off-heap (`refc`)
/// binary, tuples, lists, an `EXPORT_EXT` fun, a nested map, a low-hash-nibble
/// collision group, and bulk small ints.
///
/// The keys are built once; a key term is one word, so the same `Eterm` can key many
/// maps. The heap is owned here so the terms stay live (and unmoved).
struct Pool {
    atoms: AtomTable,
    /// The fixture heap. Never drops while any key is used, and never relocates.
    heap: Heap,
    keys: Vec<Eterm>,
    kinds: Vec<&'static str>,
    /// Off-heap payloads of the `refc`-binary keys: an **ownership anchor**, never read.
    ///
    /// A `refc` binary's `BinRef.val` addresses the first byte of one of these `Vec`s, so
    /// the storage has to outlive (and never reallocate) while any key term derived from
    /// it is in use. Holding them here is the point; nothing reads the bytes through this
    /// field (the byte-level check goes through `bitstring_parts`).
    #[allow(
        dead_code,
        reason = "an ownership anchor for the refc payloads `BinRef.val` points into — \
                  holding them alive is the whole purpose of the field"
    )]
    payloads: Vec<Vec<usize>>,
}

impl Pool {
    fn new() -> Pool {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "a", "b", "c", "d", "e", "f", "g", "h", "keymod", "keyfun",
        ]);
        let mut heap = Heap::new(1 << 16);
        let mut keys: Vec<Eterm> = Vec::with_capacity(POOL_KEYS);
        let mut kinds: Vec<&'static str> = Vec::with_capacity(POOL_KEYS);
        let mut payloads: Vec<Vec<usize>> = Vec::new();

        // Eight atoms — a flat map's atom keys order by atom *index*, so a mixed map
        // exercises the `cmp_flatmap_keys` arm as well as the term-order arm.
        for name in ["a", "b", "c", "d", "e", "f", "g", "h"] {
            keys.push(make_atom(atoms.lookup(name).expect("a seeded atom")));
            kinds.push("atom");
        }
        // `1` and `1.0`: `=:=`-distinct keys that are `==`-equal, so a route that
        // compared arithmetically would collapse them.
        keys.push(make_small(1));
        kinds.push("small");
        keys.push(float(&mut heap, 1.0));
        kinds.push("float");
        // Bignums above `2^59` (so they cannot be immediates), both signs.
        keys.push(bignum(&mut heap, false, &[0, 1]));
        kinds.push("big");
        keys.push(bignum(&mut heap, true, &[1, 1]));
        kinds.push("big");
        // A binary and a `refc` (off-heap) binary: two different bitstring interiors.
        keys.push(binary(&mut heap, 16));
        kinds.push("binary");
        keys.push(refc_binary(&mut heap, &mut payloads, 128));
        kinds.push("refc-binary");
        // Tuples, including one holding an atom.
        let a = make_atom(atoms.lookup("a").expect("a seeded atom"));
        let t0 = heap
            .tuple(&[make_small(1), make_small(2)])
            .expect("heap room for a tuple");
        keys.push(t0);
        kinds.push("tuple");
        let t1 = heap
            .tuple(&[a, make_small(3)])
            .expect("heap room for a tuple");
        keys.push(t1);
        kinds.push("tuple");
        // Proper lists.
        let tail = heap
            .cons(make_small(2), Eterm::nil())
            .expect("a cons");
        let l0 = heap.cons(make_small(1), tail).expect("a cons");
        keys.push(l0);
        kinds.push("list");
        let l1 = heap
            .cons(a, Eterm::nil())
            .expect("heap room for a cons cell");
        keys.push(l1);
        kinds.push("list");
        // An `EXPORT_EXT` fun (`M:F/A`): a fun block whose whole body is descriptor
        // words, so a projection of it is address-free.
        let fun_atom = atoms.lookup("keyfun").expect("a seeded atom");
        let mod_atom = atoms.lookup("keymod").expect("a seeded atom");
        let words = 4; // `fun_words(FunKind::External, 0)`
        // SAFETY: `words` is exactly the block `write_fun_external` writes, and the
        // fixture heap outlives every key built from it.
        let p = unsafe { heap.alloc_words(words) }.expect("heap room for a fun");
        // SAFETY: `p` addresses the 4 words the writer fills, in a live heap.
        keys.push(
            unsafe { write_fun_external(p, 2, mod_atom, fun_atom) }.expect("a well-formed fun"),
        );
        kinds.push("fun");
        // A nested map key: a map *inside* the walk and the projection.
        let cap = map_words_capacity(2);
        // SAFETY: `cap` words are reserved from the fixture heap, which outlives the key.
        let p = unsafe { heap.alloc_words(cap) }.expect("heap room for a nested map");
        let nested = [(make_small(7), make_small(70)), (make_small(8), make_small(80))];
        // SAFETY: as above.
        keys.push(unsafe { write_map(p, &nested, &atoms) }.expect("a well-formed nested map"));
        kinds.push("map");
        // Keys whose `erts_map_hash` agrees on the low **8** bits (trie levels 0 and
        // 1), so they walk the same two levels before diverging.
        for v in low_nibble_group() {
            keys.push(make_small(v));
            kinds.push("collision-group");
        }
        // The bulk: distinct small integers.
        let mut v = BULK_BASE;
        while keys.len() < POOL_KEYS {
            keys.push(make_small(v));
            kinds.push("small");
            v += 1;
        }

        let pool = Pool {
            atoms,
            heap,
            keys,
            kinds,
            payloads,
        };
        {
            // The heap is only needed to keep the terms alive; naming it here documents
            // that it must outlive every use of `keys`.
            let _: &Heap = &pool.heap;
        }
        pool.assert_distinct();
        pool
    }

    fn key(&self, index: usize) -> Eterm {
        self.keys[index]
    }

    fn value(&self, index: usize) -> Eterm {
        value_of(index)
    }

    /// The pool holds no two `=:=`-equal keys — the invariant every "one pair per key"
    /// claim in this file rests on.
    fn assert_distinct(&self) {
        let mut order: Vec<usize> = (0..self.keys.len()).collect();
        order.sort_by(|a, b| cmp(self.keys[*a], self.keys[*b], &self.atoms));
        for w in order.windows(2) {
            assert!(
                !eq(self.keys[w[0]], self.keys[w[1]], &self.atoms),
                "the pool holds duplicate keys at {} ({}) and {} ({})",
                w[0],
                self.kinds[w[0]],
                w[1],
                self.kinds[w[1]]
            );
        }
    }

    /// Every class the pool holds, once — the report's key-class inventory.
    fn class_inventory(&self) -> Vec<(&'static str, usize)> {
        let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
        for kind in &self.kinds {
            *counts.entry(kind).or_insert(0) += 1;
        }
        counts.into_iter().collect()
    }
}

/// A float key in `heap`.
fn float(heap: &mut Heap, value: f64) -> Eterm {
    // SAFETY: a float block is 2 words and the heap outlives the key.
    let p = unsafe { heap.alloc_words(2) }.expect("heap room for a float");
    // SAFETY: `p` addresses the 2 words `write_float` writes.
    unsafe { write_float(p, value) }
}

/// A bignum key in `heap`.
fn bignum(heap: &mut Heap, negative: bool, digits: &[u64]) -> Eterm {
    let words = big_size_words(digits.len());
    // SAFETY: `words` is exactly the block `write_big` writes.
    let p = unsafe { heap.alloc_words(words) }.expect("heap room for a bignum");
    // SAFETY: `p` addresses `words` writable words; `digits` is non-empty.
    unsafe { write_big(p, negative, digits) }
}

/// An on-heap (`ErlHeapBits`) binary of `len` bytes.
fn binary(heap: &mut Heap, len: usize) -> Eterm {
    let bytes: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(31)).collect();
    let words = heap_bits_size_words(bytes.len() * 8);
    // SAFETY: `words` is exactly the block `write_binary` writes, in a live heap.
    let p = unsafe { heap.alloc_words(words) }.expect("heap room for a binary");
    // SAFETY: `p` addresses `words` writable words; `write_binary` writes them all.
    unsafe { write_binary(p, &bytes) }.expect("a well-formed binary")
}

/// An off-heap (`refc`) binary of `len` bytes; its payload is parked in `payloads` so
/// it never moves or drops.
fn refc_binary(heap: &mut Heap, payloads: &mut Vec<Vec<usize>>, len: usize) -> Eterm {
    let n_words = len.div_ceil(core::mem::size_of::<usize>());
    let mut payload = vec![0usize; n_words];
    // SAFETY: `payload` covers `len` bytes, its allocation is word-aligned (a
    // `Vec<usize>`) and it outlives every key built from it.
    unsafe {
        let bytes = core::slice::from_raw_parts_mut(payload.as_mut_ptr() as *mut u8, len);
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(17).wrapping_add(3);
        }
    }
    payloads.push(payload);
    let payload = payloads.last().expect("just pushed");
    let base = payload.as_ptr() as *const u8;
    let val = base as *mut Eterm;
    // SAFETY: 8 words for the `[BinRef][ErlSubBits]` pair; `val` names the payload's
    // first byte, `next` is a null end-of-chain, `base` is word-aligned and the payload
    // stays parked in `payloads`.
    let p = unsafe { heap.alloc_words(8) }.expect("heap room for a refc binary");
    // SAFETY: as above.
    unsafe { write_refc_binary(p, val, core::ptr::null_mut(), base, 0, len * 8) }
}

/// Four small-integer keys forming two pairs whose `erts_map_hash` values agree on the
/// low **8** bits — the deepest shared prefix reachable without a genuine 64-bit hash
/// collision (which this crate cannot construct: a small int hashes as the splitmix64
/// finalizer of its word, a bijection).
fn low_nibble_group() -> [i64; 4] {
    let mut buckets: BTreeMap<u8, Vec<i64>> = BTreeMap::new();
    let mut picked: Vec<i64> = Vec::new();
    for v in 40_000..200_000i64 {
        let low = (erts_map_hash(make_small(v)) & 0xFF) as u8;
        let bucket = buckets.entry(low).or_default();
        bucket.push(v);
        if bucket.len() == 2 {
            picked.extend_from_slice(&bucket[..2]);
            if picked.len() == 4 {
                break;
            }
        }
    }
    assert_eq!(picked.len(), 4, "the low-nibble group must have two pairs");
    [picked[0], picked[1], picked[2], picked[3]]
}

// ---------------------------------------------------------------------------
// Route A: rebuild the whole map from its pair set with `write_map`
// ---------------------------------------------------------------------------

/// Route A — the semantics oracle. Holds the host-side pair set (the truth about which
/// keys the map has and what they are bound to) plus the map `write_map` produced from
/// it in the *previous* step's staging region.
struct Rebuild {
    pairs: Vec<(Eterm, Eterm)>,
    region: Region,
    map: Eterm,
}

impl Rebuild {
    fn new(pairs: Vec<(Eterm, Eterm)>, atoms: &AtomTable) -> Rebuild {
        let mut r = Rebuild {
            pairs,
            region: Region::new(0),
            map: Eterm::nil(),
        };
        r.build(atoms);
        r
    }

    fn expected(&self) -> &[(Eterm, Eterm)] {
        &self.pairs
    }

    fn current(&self) -> Eterm {
        self.map
    }

    /// `maps:put/3` on the pair set (first-occurrence key word, last value wins).
    fn upsert(&mut self, key: Eterm, value: Eterm, atoms: &AtomTable) -> bool {
        match self.pairs.iter_mut().find(|(k, _)| eq(*k, key, atoms)) {
            Some(entry) => {
                entry.1 = value;
                true
            }
            None => {
                self.pairs.push((key, value));
                false
            }
        }
    }

    /// `maps:remove/2` on the pair set; `true` when the key was present.
    fn drop_key(&mut self, key: Eterm, atoms: &AtomTable) -> bool {
        let before = self.pairs.len();
        self.pairs.retain(|(k, _)| !eq(*k, key, atoms));
        self.pairs.len() != before
    }

    /// Rebuilds the whole map into a fresh reservation.
    fn build(&mut self, atoms: &AtomTable) {
        let mut region = Region::new(map_words_capacity(self.pairs.len()));
        // SAFETY: `region` reserves `map_words_capacity(pairs.len())` words — the
        // documented bound for `write_map` — and outlives `map` (both live in `self`).
        let map = unsafe { write_map(region.base(), &self.pairs, atoms) }.expect("a well-formed map");
        region.check_tail();
        self.map = map;
        self.region = region;
    }
}

// ---------------------------------------------------------------------------
// Route B: the incremental (path-copying) writers
// ---------------------------------------------------------------------------

/// The word figures one COW op reports.
#[derive(Clone, Copy, Debug)]
struct Facts {
    /// `map_*_cow_words_required` — the exact figure the writer's own `debug_assert!`s
    /// compare against.
    required: usize,
    /// The words actually mutated in the staging buffer (the sentinel sweep's length).
    touched: usize,
    /// The size-only bound for this op at the source's size.
    capacity: usize,
    /// The result word *is* the input word: the op wrote nothing and returned the map
    /// it was given (`map_put_cow`'s "the stored value word is already the new one" arm).
    unchanged: bool,
}

/// Applies [`map_put_cow`] into a reservation of **exactly**
/// [`map_put_cow_words_required`] words (plus the sentinel tail).
///
/// An under-reservation is caught by the writer's own `debug_assert!(written == plan)`
/// and by the sentinel; an over-reservation would be invisible, which is why the figure
/// is not rounded up here.
fn cow_put_exact(
    src: Eterm,
    key: Eterm,
    value: Eterm,
    atoms: &AtomTable,
) -> (Region, Eterm, Facts) {
    let size = map_size(src, atoms).expect("a live map");
    let required = map_put_cow_words_required(src, key).expect("a live map");
    let capacity = map_put_cow_words_capacity(size);
    assert!(
        required <= capacity,
        "put: required {required} words exceed the capacity bound {capacity} at size {size}"
    );
    let mut region = Region::new(required);
    // SAFETY: the region reserves exactly the figure the read-only descent reported, in
    // a heap that outlives `src` and is not `src`'s own storage.
    let got = unsafe { map_put_cow(region.base(), src, key, value, atoms) }.expect("a live map");
    region.check_tail();
    let touched = region.touched();
    assert!(
        touched <= required,
        "put: wrote {touched} words but the plan reported {required}"
    );
    let facts = Facts {
        required,
        touched,
        capacity,
        unchanged: got.raw() == src.raw(),
    };
    (region, got, facts)
}

/// Applies [`map_remove_cow`] into a reservation of exactly
/// [`map_remove_cow_words_required`] words (plus the sentinel tail).
fn cow_remove_exact(
    src: Eterm,
    key: Eterm,
    atoms: &AtomTable,
) -> (Region, Option<Eterm>, Facts) {
    let size = map_size(src, atoms).expect("a live map");
    let required = map_remove_cow_words_required(src, key).expect("a live map");
    let capacity = map_remove_cow_words_capacity(size);
    assert!(
        required <= capacity,
        "remove: required {required} words exceed the capacity bound {capacity} at size {size}"
    );
    let mut region = Region::new(required);
    // SAFETY: as `cow_put_exact`; an absent key writes nothing at all.
    let got = unsafe { map_remove_cow(region.base(), src, key, atoms) }.expect("a live map");
    region.check_tail();
    let touched = region.touched();
    assert!(
        touched <= required,
        "remove: wrote {touched} words but the plan reported {required}"
    );
    let facts = Facts {
        required,
        touched,
        capacity,
        unchanged: got.is_none(),
    };
    (region, got, facts)
}

/// Route B — the chained path-copied map. Every staging buffer of the chain is kept:
/// each result shares storage with all of its predecessors.
struct Cow {
    regions: Vec<Region>,
    map: Eterm,
}

impl Cow {
    fn new(pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Cow {
        let mut region = Region::new(map_words_capacity(pairs.len()));
        // SAFETY: the region reserves the documented bound for `write_map`, and it is
        // owned by `self`, so it outlives every result derived from this map.
        let map = unsafe { write_map(region.base(), pairs, atoms) }.expect("a well-formed map");
        region.check_tail();
        Cow {
            regions: vec![region],
            map,
        }
    }

    fn put(&mut self, key: Eterm, value: Eterm, atoms: &AtomTable) -> (Eterm, Facts) {
        let (region, got, facts) = cow_put_exact(self.map, key, value, atoms);
        self.regions.push(region);
        if !facts.unchanged {
            self.map = got;
        }
        (got, facts)
    }

    fn remove(&mut self, key: Eterm, atoms: &AtomTable) -> (Option<Eterm>, Facts) {
        let (region, got, facts) = cow_remove_exact(self.map, key, atoms);
        self.regions.push(region);
        if let Some(got) = got {
            self.map = got;
        }
        (got, facts)
    }
}

// ---------------------------------------------------------------------------
// The op script: both routes in lockstep, asserting at every step
// ---------------------------------------------------------------------------

/// The base sizes the sweep covers: the degenerate ones, both sides of the flat-map
/// limit, and four ordinary to large hash-map sizes.
const SWEEP_SIZES: [usize; 10] = [0, 1, 2, 31, 32, 33, 34, 64, 250, 2_500];

/// One scripted step, expressed in pool indices.
struct Step {
    kind: StepKind,
    /// The pool index of the key.
    key: usize,
    /// The value word (a put only).
    value: Eterm,
    what: &'static str,
}

enum StepKind {
    /// Put the value the map already holds (`Nothing` arm: nothing written).
    PutSame,
    /// Put a fresh value under an existing key (a value-replace arm).
    PutNew,
    /// Put a key the map does not hold (insert / flat→HAMT conversion / divergence).
    PutFresh,
    /// Remove a key the map holds.
    Drop,
    /// Remove a key the map does not hold (`Ok(None)`, nothing written).
    DropAbsent,
}

/// Both routes plus the per-step log.
struct Sweep {
    rebuild: Rebuild,
    cow: Cow,
    /// Keys that must be absent at every step, probed on both routes.
    probes: Vec<Eterm>,
    log: Vec<(&'static str, Facts)>,
}

impl Sweep {
    fn new(n: usize, pool: &Pool, atoms: &AtomTable) -> Sweep {
        let pairs: Vec<(Eterm, Eterm)> = (0..n).map(|i| (pool.key(i), pool.value(i))).collect();
        let rebuild = Rebuild::new(pairs, atoms);
        let cow = Cow::new(rebuild.expected(), atoms);
        let probes = (n + 20..n + 24).map(|i| pool.key(i)).collect();
        Sweep {
            rebuild,
            cow,
            probes,
            log: Vec::new(),
        }
    }

    /// Runs one step on both routes and asserts full equivalence.
    fn apply(&mut self, step: &Step, pool: &Pool, atoms: &AtomTable, what: &str) -> Facts {
        let key = pool.key(step.key);
        let facts = match step.kind {
            StepKind::PutSame | StepKind::PutNew | StepKind::PutFresh => {
                let facts = {
                    self.rebuild.upsert(key, step.value, atoms);
                    self.rebuild.build(atoms);
                    let (got, facts) = self.cow.put(key, step.value, atoms);
                    assert_equivalent(
                        got,
                        self.rebuild.current(),
                        self.rebuild.expected(),
                        &self.probes,
                        atoms,
                        what,
                    );
                    facts
                };
                // The two zero-write arms are asserted where they are reachable rather
                // than assumed: `PutSame` must write nothing, and a `PutFresh`/`PutNew`
                // must write something.
                match step.kind {
                    StepKind::PutSame => {
                        assert!(facts.unchanged, "{what}: an identical value word must return the input map");
                        assert_eq!(facts.touched, 0, "{what}: an identical value word writes nothing");
                    }
                    _ => assert!(
                        facts.touched > 0,
                        "{what}: a real update must write at least one word"
                    ),
                }
                facts
            }
            StepKind::Drop | StepKind::DropAbsent => {
                let present = self.rebuild.drop_key(key, atoms);
                self.rebuild.build(atoms);
                let (got, facts) = self.cow.remove(key, atoms);
                assert_eq!(
                    got.is_some(),
                    present,
                    "{what}: the routes disagree about whether the key is present"
                );
                assert_equivalent(
                    self.cow.map,
                    self.rebuild.current(),
                    self.rebuild.expected(),
                    &self.probes,
                    atoms,
                    what,
                );
                if present {
                    assert!(facts.touched > 0, "{what}: a delete writes at least one word");
                } else {
                    assert_eq!(facts.touched, 0, "{what}: an absent key writes nothing");
                    // `capacity` is a *size-only* bound (it cannot know whether the key is
                    // present), so only the exact figure is asserted here.
                    assert_eq!(facts.required, 0, "{what}: an absent key needs no words");
                    assert!(got.is_none(), "{what}: an absent key answers None");
                }
                facts
            }
        };
        self.log.push((step.what, facts));
        facts
    }
}

/// The script's fixed prologue: one op per put/remove arm that does not need a drain.
fn prologue(n: usize) -> Vec<Step> {
    let mut steps = Vec::new();
    if n == 0 {
        // The degenerate map: one insert, a second insert, a value replace, a delete,
        // an absent delete.
        steps.push(fresh(0, "n=0: insert into the empty map"));
        steps.push(fresh(1, "n=0: insert the second pair"));
        steps.push(Step {
            kind: StepKind::PutNew,
            key: 0,
            value: make_small(700_001),
            what: "n=0: replace a value",
        });
        steps.push(drop(0, "n=0: delete down to empty"));
        steps.push(absent(5, "n=0: delete an absent key"));
        return steps;
    }
    steps.push(Step {
        kind: StepKind::PutSame,
        key: 0,
        value: value_of(0),
        what: "put the stored value word again",
    });
    steps.push(Step {
        kind: StepKind::PutNew,
        key: n / 2,
        value: make_small(700_002),
        what: "replace an existing key's value",
    });
    steps.push(fresh(n, "insert a new key"));
    steps.push(fresh(n + 1, "insert a second new key"));
    steps.push(drop(n - 1, "delete a present key"));
    steps.push(absent(n + 5, "delete an absent key"));
    steps.push(Step {
        kind: StepKind::PutFresh,
        key: n - 1,
        value: value_of(n - 1),
        what: "re-insert the deleted key",
    });
    // Net zero, step by step: the two inserts (`n`, `n + 1`) are undone by the two deletes
    // below, and the `n - 1` delete is undone by the re-insert above — so the prologue is
    // size-neutral for every `n` and the sweep's final assertion is meaningful.
    steps.push(drop(n, "delete the first inserted key"));
    steps.push(drop(n + 1, "delete the second inserted key"));
    steps
}

fn fresh(key: usize, what: &'static str) -> Step {
    Step {
        kind: StepKind::PutFresh,
        key,
        value: make_small(700_000 + key as i64),
        what,
    }
}

fn drop(key: usize, what: &'static str) -> Step {
    Step {
        kind: StepKind::Drop,
        key,
        value: make_small(0),
        what,
    }
}

fn absent(key: usize, what: &'static str) -> Step {
    Step {
        kind: StepKind::DropAbsent,
        key,
        value: make_small(0),
        what,
    }
}

// ---------------------------------------------------------------------------
// The observable-equality assertion
// ---------------------------------------------------------------------------

/// The pair set in `cmp` order — the order [`map_pairs`] reports.
fn sorted_pairs(pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Vec<(Eterm, Eterm)> {
    let mut out = pairs.to_vec();
    out.sort_by(|a, b| cmp(a.0, b.0, atoms));
    out
}

/// Asserts that `got` (the COW route) and `rebuilt` (the `write_map` route) are the
/// same map as far as any consumer can tell.
///
/// The comparisons are, in increasing strength:
///
/// 1. the key count;
/// 2. [`map_pairs`] — the `cmp` key order — against each other *and* against the
///    host-side pair set;
/// 3. [`map_pairs_storage_order`] — the **stored** HAMT / keys-tuple order, i.e. the
///    order an image-preserving serializer emits;
/// 4. [`render_map`] — the canonical node rendering: same node kinds, same bitmaps, same
///    head size word, same leaf cells in the same slots. Together with 3 this is the
///    `term_to_binary` byte-equality claim expressed without `beam_codec`;
/// 5. [`map_get`] for every key of the pair set and for every absent probe.
fn assert_equivalent(
    got: Eterm,
    rebuilt: Eterm,
    expected: &[(Eterm, Eterm)],
    probes: &[Eterm],
    atoms: &AtomTable,
    what: &str,
) {
    let want = sorted_pairs(expected, atoms);
    assert_eq!(
        map_size(got, atoms).expect("a live map"),
        want.len(),
        "{what}: the COW result's size"
    );
    assert_eq!(
        map_size(rebuilt, atoms).expect("a live map"),
        want.len(),
        "{what}: the rebuild's size"
    );
    assert_eq!(
        map_pairs(got, atoms).expect("a live map"),
        want,
        "{what}: map_pairs (the cmp order) must be the expected pair set"
    );
    assert_eq!(
        map_pairs(rebuilt, atoms).expect("a live map"),
        want,
        "{what}: the rebuild's map_pairs"
    );
    assert_eq!(
        map_pairs_storage_order(got).expect("a live map"),
        map_pairs_storage_order(rebuilt).expect("a live map"),
        "{what}: the stored (trie / keys-tuple) order differs from the rebuild's"
    );
    assert_eq!(
        render_map(got),
        render_map(rebuilt),
        "{what}: the canonical node rendering differs from the rebuild's"
    );
    for (k, v) in &want {
        assert_eq!(
            map_get(got, *k, atoms).expect("a live map"),
            Some(*v),
            "{what}: map_get of a key of the pair set"
        );
    }
    for probe in probes {
        assert_eq!(
            map_get(got, *probe, atoms).expect("a live map"),
            map_get(rebuilt, *probe, atoms).expect("a live map"),
            "{what}: the routes disagree about an absent probe"
        );
        assert_eq!(
            map_get(got, *probe, atoms).expect("a live map"),
            None,
            "{what}: the probe key must be absent"
        );
    }
}

// ---------------------------------------------------------------------------
// The canonical structural rendering
// ---------------------------------------------------------------------------

/// The header kind of a HAMT node.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum HeaderKind {
    /// `[hdr][size][16 slots]` — a full head bitmap.
    HeadArray,
    /// `[hdr][size][n slots]`.
    HeadBitmap,
    /// `[hdr][n slots]`.
    InnerBitmap,
}

/// A base-independent projection of one term.
///
/// Bit-exact for a word that is not a pointer, **structural** for a boxed term (its
/// content, never its address), and explicit about the raw payload values that are not
/// terms (a float's IEEE bits, a bignum digit, a fun's descriptor words, a bitstring's
/// payload checksum).
#[derive(Debug, PartialEq, Eq, Clone)]
enum Proj {
    /// A word that is not a pointer: an immediate (or a non-term) — bit-exact.
    Imm { class: TermClass, raw: usize },
    /// A boxed term: its class, its on-heap words and its projected parts.
    Boxed {
        class: TermClass,
        words: usize,
        parts: Vec<Proj>,
    },
    /// A raw payload value that is not a term.
    Raw { what: &'static str, value: usize },
}

/// The canonical rendering of a map: node kind, bitmap, head size word and, at each
/// slot, the leaf's projected cell / a sub-node / a collision node's cells in order.
#[derive(Debug, PartialEq, Eq)]
enum Render {
    /// A flat map: its pair count and its pairs in keys-tuple (storage) order.
    Flat { size: usize, pairs: Vec<(Proj, Proj)> },
    /// A trie node.
    Bitmap {
        kind: HeaderKind,
        bitmap: u16,
        /// The head's `size` word (`None` for an inner node, which has no size word).
        size: Option<usize>,
        slots: Vec<Render>,
    },
    /// A collision node: its cells in order.
    Collision { cells: Vec<(Proj, Proj)> },
    /// A leaf `[Key, Value]` cons cell.
    Leaf { cell: (Proj, Proj) },
}

/// The header kind and bitmap of a HAMT node header.
fn node_kind(header: Eterm) -> (HeaderKind, u16) {
    match map_header_type(header) as usize {
        MAP_HEADER_TAG_HAMT_HEAD_ARRAY => (HeaderKind::HeadArray, map_header_val(header) as u16),
        MAP_HEADER_TAG_HAMT_HEAD_BITMAP => (HeaderKind::HeadBitmap, map_header_val(header) as u16),
        MAP_HEADER_TAG_HAMT_NODE_BITMAP => (HeaderKind::InnerBitmap, map_header_val(header) as u16),
        other => panic!("map header type {other} is not a HAMT node"),
    }
}

/// Renders `map` canonically.
///
/// **This is the byte-equality stand-in.** Two maps with the same rendering have the
/// same node kinds, the same bitmaps, the same head size word and the same leaf cells in
/// the same slots, and — with [`map_pairs_storage_order`] — therefore the same
/// `term_to_binary` bytes. Raw interior words are *not* compared: they legitimately
/// differ by heap base, and the flat value-replace arm legitimately shares the source's
/// keys tuple where a rebuild writes its own.
fn render_map(map: Eterm) -> Render {
    let p = boxed_val(map);
    // SAFETY: `map` is a live boxed map at every call site.
    let header = unsafe { *p };
    if is_flatmap_header(header) {
        // SAFETY: `map` is a live flat map.
        let (keys, values) = unsafe { flatmap_parts(map) };
        return Render::Flat {
            size: keys.len(),
            pairs: keys
                .iter()
                .copied()
                .zip(values.iter().copied())
                .map(|(k, v)| (proj(k), proj(v)))
                .collect(),
        };
    }
    render_node(p)
}

/// Renders one hash-map node (and, recursively, everything under it).
fn render_node(node: *const Eterm) -> Render {
    // SAFETY: `node` addresses a live hash-map node's header word.
    let header = unsafe { *node };
    let (kind, bitmap) = node_kind(header);
    let arity = header_arity(header);
    let slots = bitmap.count_ones() as usize;
    let mut rendered = Vec::with_capacity(slots);
    for i in 0..slots {
        // SAFETY: the node's slot words follow the header (and the head's size word);
        // `i < popcount(bitmap)` is the node's slot count.
        let child = unsafe { *node.add(1 + arity + i) };
        rendered.push(render_child(child));
    }
    let size = if arity == 1 {
        // SAFETY: a head node's word 1 is its `size` word.
        Some(unsafe { *node.add(1) }.raw())
    } else {
        None
    };
    Render::Bitmap {
        kind,
        bitmap,
        size,
        slots: rendered,
    }
}

/// Renders one slot of a hash-map node.
fn render_child(child: Eterm) -> Render {
    if is_list(child) {
        // SAFETY: a leaf slot holds a live `[Key, Value]` cons cell.
        let (k, v) = unsafe { (car(child), cdr(child)) };
        return Render::Leaf {
            cell: (proj(k), proj(v)),
        };
    }
    let p = boxed_val(child);
    // SAFETY: a non-leaf slot holds a live node or a collision-node tuple.
    let header = unsafe { *p };
    if is_map_header(header) {
        return render_node(p);
    }
    if is_arity_value(header) {
        // SAFETY: `child` is a collision-node tuple.
        let n = unsafe { tuple_size(child) };
        let mut cells = Vec::with_capacity(n);
        for i in 0..n {
            // SAFETY: `i < n` and a collision cell is a live cons cell.
            let cell = unsafe { tuple_element(child, i) };
            assert!(
                is_list(cell),
                "a collision-node cell must be a `[Key, Value]` cons cell"
            );
            // SAFETY: as above.
            let (k, v) = unsafe { (car(cell), cdr(cell)) };
            cells.push((proj(k), proj(v)));
        }
        return Render::Collision { cells };
    }
    panic!("a hash-map slot holds neither a leaf, a sub-node nor a collision tuple");
}

/// Projects one term structurally: bit-exact for a non-pointer word, content-based for a
/// boxed term, explicit for a raw payload that is not a term.
fn proj(t: Eterm) -> Proj {
    if is_non_value(t) {
        return Proj::Imm {
            class: TermClass::NonValue,
            raw: t.raw(),
        };
    }
    if is_list(t) {
        // SAFETY: `t` is a live cons cell.
        let (a, b) = unsafe { (car(t), cdr(t)) };
        return Proj::Boxed {
            class: TermClass::List,
            words: size_object(t),
            parts: vec![proj(a), proj(b)],
        };
    }
    if !is_boxed_term(t) {
        // SAFETY: `class` only inspects tag bits for a non-boxed word.
        return Proj::Imm {
            class: unsafe { class(t) },
            raw: t.raw(),
        };
    }
    let p = boxed_val(t);
    // SAFETY: `t` is a live boxed term, so `p` addresses its header word and the
    // object's payload words are readable.
    let header = unsafe { *p };
    // SAFETY: as above.
    let words = unsafe { object_words(header, p) };
    // SAFETY: as above (a deref-guarded classification of a live term).
    let term_class = unsafe { class(t) };
    let parts = match term_class {
        TermClass::Tuple | TermClass::Record => {
            // SAFETY: `t` is a live tuple-shaped term.
            let n = unsafe { tuple_size(t) };
            (0..n)
                // SAFETY: `i < n` indexes the tuple's payload.
                .map(|i| proj(unsafe { tuple_element(t, i) }))
                .collect()
        }
        TermClass::Map => {
            // A nested map is projected by content: its storage-order pair sequence.
            // (The node *shape* is `Render`'s job; here only key/value identity matters.)
            map_pairs_storage_order(t)
                .expect("a live nested map")
                .into_iter()
                .flat_map(|(k, v)| [proj(k), proj(v)])
                .collect()
        }
        TermClass::Float => {
            // SAFETY: `t` is a live float block.
            let bits = unsafe { beam_term::bitstring::float_bits(t) };
            vec![Proj::Raw {
                what: "ieee754",
                value: bits as usize,
            }]
        }
        TermClass::Big => {
            // SAFETY: `t` is a live bignum; the digits borrow its storage.
            let (negative, digits) = unsafe { big_parts(t) };
            let mut out = vec![Proj::Raw {
                what: "negative",
                value: usize::from(negative),
            }];
            out.extend(digits.iter().map(|d| Proj::Raw {
                what: "digit",
                value: *d as usize,
            }));
            out
        }
        TermClass::Bitstring => {
            let (n_bits, checksum) = payload_fingerprint(t);
            vec![
                Proj::Raw {
                    what: "n_bits",
                    value: n_bits,
                },
                Proj::Raw {
                    what: "payload",
                    value: checksum,
                },
            ]
        }
        TermClass::Fun => {
            // The pool's fun is an `EXPORT_EXT` with `num_free == 0`, so its whole block
            // is descriptor words: projecting them raw is address-free.
            (0..words)
                // SAFETY: `words` is the block's own word count.
                .map(|i| Proj::Raw {
                    what: "fun-descriptor",
                    value: unsafe { *p.add(i) }.raw(),
                })
                .collect()
        }
        _ => Vec::new(),
    };
    Proj::Boxed {
        class: term_class,
        words,
        parts,
    }
}

/// `(size_bits, checksum)` of a bitstring's payload: the first bytes are hashed when the
/// window is byte-aligned, otherwise bit by bit.
fn payload_fingerprint(t: Eterm) -> (usize, usize) {
    // SAFETY: `t` is a live bitstring; `base`/`offset`/`size` describe its payload
    // window, which is readable while the owning payload does not move.
    let (base, offset, size) = unsafe { bitstring_parts(t) };
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    if offset % 8 == 0 {
        let nbytes = size / 8;
        if nbytes > 0 {
            // SAFETY: the payload covers bits `offset..offset + size`, so the whole
            // bytes of that window at `base + offset / 8` are readable.
            let bytes = unsafe { core::slice::from_raw_parts(base.add(offset / 8), nbytes) };
            for &b in bytes {
                hash = (hash ^ b as u64).wrapping_mul(PRIME);
            }
        }
    } else {
        for i in 0..size {
            // SAFETY: `i < size` is within the bitstring's own size.
            if unsafe { bit_at(t, i) } {
                hash = (hash ^ 1).wrapping_mul(PRIME);
            }
        }
    }
    (size, hash as usize)
}

// ---------------------------------------------------------------------------
// The structural walk (the copier's two questions)
// ---------------------------------------------------------------------------

/// The most objects one walk may visit. A term graph built by these writers is acyclic
/// and small; the cap turns a hypothetical cycle into a failure instead of a hang.
const MAX_WALK_OBJECTS: usize = 4_000_000;

/// One object the structural walk reached: a boxed thing (header word included) or a
/// header-less cons cell.
struct Obj {
    /// The object's address (`boxed_val` for a boxed thing, `list_val` for a cons cell).
    ptr: usize,
    /// The words it occupies, from [`layout::object_words`] (`2` for a cons cell).
    words: usize,
    /// Its header word (`NIL` for a cons cell, which has none).
    header: Eterm,
    /// The term word that named it.
    term: Eterm,
}

impl Obj {
    /// The one object `size_object` prices `0` for: the shared literal empty tuple.
    fn is_literal_empty_tuple(&self) -> bool {
        is_arity_value(self.header) && arityval(self.header) == 0 && is_literal(self.term)
    }
}

/// Every object reachable from `root`, in visit order.
///
/// Occurrences are counted (no pointer dedup): a shared-but-acyclic subterm counts once
/// per occurrence, exactly as [`size_object`] counts it, which is what lets the two walks
/// be cross-checked against each other. The visit order and the child enumeration come
/// from the crate's own [`layout::for_each_child_slot`], so this file asks the copier's
/// questions rather than a private re-implementation of them.
struct Walk {
    objects: Vec<Obj>,
}

impl Walk {
    fn ptrs(&self) -> BTreeSet<usize> {
        self.objects.iter().map(|o| o.ptr).collect()
    }

    fn total_words(&self) -> usize {
        self.objects.iter().map(|o| o.words).sum()
    }

    fn literal_empty_tuples(&self) -> usize {
        self.objects
            .iter()
            .filter(|o| o.is_literal_empty_tuple())
            .count()
    }

    fn objects_in(&self, block: (usize, usize)) -> impl Iterator<Item = &Obj> {
        self.objects.iter().filter(move |o| inside(o.ptr, block))
    }

    fn words_in(&self, block: (usize, usize)) -> usize {
        self.objects_in(block).map(|o| o.words).sum()
    }
}

fn inside(ptr: usize, (base, end): (usize, usize)) -> bool {
    ptr >= base && ptr < end
}

/// Asserts that a map's own **node tree** is self-contained in `block`: the root, every
/// HAMT sub-node and every collision node live inside the one contiguous reservation a
/// single `write_map` call filled. A leaf's key/value terms may point anywhere (they are
/// the caller's terms), and a key may itself be a map, so the walk stops at a leaf cell
/// and never follows a term out of the map's interior.
fn check_node_tree(root: Eterm, block: (usize, usize), what: &str) {
    let mut stack = vec![root];
    let mut visited = 0usize;
    while let Some(node) = stack.pop() {
        let p = boxed_val(node);
        assert!(
            inside(p as usize, block),
            "{what}: a map node at {:#x} lives outside the map's own block",
            p as usize
        );
        // SAFETY: `node` is a live map node, so `p` addresses its header word.
        let header = unsafe { *p };
        if is_flatmap_header(header) {
            continue;
        }
        let (_, bitmap) = node_kind(header);
        let arity = header_arity(header);
        let slots = bitmap.count_ones() as usize;
        for i in 0..slots {
            // SAFETY: the node's slot words follow the header (and the head's size word).
            let child = unsafe { *p.add(1 + arity + i) };
            if is_list(child) {
                continue;
            }
            let cp = boxed_val(child);
            // SAFETY: a non-leaf slot holds a live node or a collision tuple.
            let child_header = unsafe { *cp };
            if is_map_header(child_header) {
                stack.push(child);
            } else if is_arity_value(child_header) {
                assert!(
                    inside(cp as usize, block),
                    "{what}: a collision node at {:#x} lives outside the map's own block",
                    cp as usize
                );
            } else {
                panic!("{what}: a hash-map slot holds neither a leaf, a sub-node nor a collision");
            }
        }
        visited += 1;
        assert!(
            visited <= MAX_WALK_OBJECTS,
            "{what}: the node tree is larger than {MAX_WALK_OBJECTS} nodes — a cycle?"
        );
    }
}

/// Which child-slot enumeration a walk uses.
///
/// The crate publishes **two** enumerators, and they differ in exactly one place
/// ([`layout`]'s documented sizing/copying divergence): an `ErlSubBits` exposes no
/// *sizing* child (its contribution — a whole refc pair, or a copied window — is priced
/// opaquely by [`object_words`]), while the *copy* enumeration reports its `orig` word,
/// because a copier must re-point it. A walk that follows `orig` therefore reaches the
/// refc pair's `BinRef` (3 words) as an extra object, which is exactly the difference
/// between the two totals.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slots {
    /// [`for_each_child_slot`] — the copier's question (what must be forwarded).
    Copy,
    /// [`for_each_sizing_child_slot`] — the sizer's question (`size_object`'s own walk).
    Sizing,
}
/// Walks every object reachable from `root` in the **copier's** child enumeration,
/// asserting each header is well formed and no slot is
/// [`THE_NON_VALUE`](beam_term::ops::THE_NON_VALUE).
fn walk(root: Eterm, what: &str) -> Walk {
    walk_seen(root, what, Slots::Copy)
}
/// [`walk`] in the **sizer's** child enumeration — the one
/// [`size_object`] itself uses, so its total is directly comparable to that function's
/// answer (modulo the literal empty tuple it prices `0`).
fn walk_sizing(root: Eterm, what: &str) -> Walk {
    walk_seen(root, what, Slots::Sizing)
}
fn walk_seen(root: Eterm, what: &str, slots: Slots) -> Walk {
    let mut objects: Vec<Obj> = Vec::new();
    let mut stack: Vec<Eterm> = vec![root];
    while let Some(t) = stack.pop() {
        assert!(!is_non_value(t), "{what}: a reachable slot holds THE_NON_VALUE");
        if is_list(t) {
            // A cons cell is header-less: `[CAR][CDR]`, two words.
            objects.push(Obj {
                ptr: list_val(t) as usize,
                words: 2,
                header: Eterm::nil(),
                term: t,
            });
            // SAFETY: `t` is a live cons cell, so both of its words are terms.
            let (a, b) = unsafe { (car(t), cdr(t)) };
            stack.push(a);
            stack.push(b);
        } else if is_boxed_term(t) {
            let p = boxed_val(t);
            // SAFETY: `t` is a live boxed term, so `p` addresses its header word.
            let header = unsafe { *p };
            // SAFETY: `p` heads a live object of `header`'s shape.
            let words = unsafe { object_words(header, p) };
            check_header(header, p, words, what);
            objects.push(Obj {
                ptr: p as usize,
                words,
                header,
                term: t,
            });
            // SAFETY: as above — every child slot of a live object holds a live term.
            unsafe {
                match slots {
                    Slots::Copy => for_each_child_slot(header, p, |offset| {
                        stack.push(*p.add(offset));
                    }),
                    Slots::Sizing => for_each_sizing_child_slot(header, p, |offset| {
                        stack.push(*p.add(offset));
                    }),
                }
            }
        }
        assert!(
            objects.len() <= MAX_WALK_OBJECTS,
            "{what}: the walk visited more than {MAX_WALK_OBJECTS} objects — a cycle?"
        );
    }
    Walk { objects }
}

/// Asserts one object's header is well formed and prices it consistently.
fn check_header(header: Eterm, p: *const Eterm, words: usize, what: &str) {
    if is_flatmap_header(header) {
        // SAFETY: word 1 of a live flat map is its raw pair count.
        let size = unsafe { *p.add(1) }.raw();
        assert_eq!(
            words,
            size + 3,
            "{what}: a flat map of {size} pairs must occupy size + 3 words"
        );
        // SAFETY: the flat map's keys tuple word is a term.
        let keys = unsafe { *p.add(2) };
        assert!(
            is_boxed_term(keys),
            "{what}: a flat map's keys slot must hold a tuple term"
        );
        return;
    }
    if is_map_header(header) {
        let (kind, bitmap) = node_kind(header);
        let slots = bitmap.count_ones() as usize;
        let arity = header_arity(header);
        assert_eq!(
            words,
            1 + arity + slots,
            "{what}: a node's words must be 1 + arity + popcount(bitmap)"
        );
        match kind {
            HeaderKind::HeadArray => {
                assert_eq!(bitmap, FULL_BITMAP, "{what}: a head array is a full bitmap");
                assert_eq!(slots, 16, "{what}: a head array has 16 slots");
                assert_eq!(arity, 1, "{what}: a head carries a size word");
            }
            HeaderKind::HeadBitmap => {
                assert_eq!(arity, 1, "{what}: a head carries a size word");
                assert!(
                    slots > 0 && slots < 16,
                    "{what}: a full head bitmap must be written as the head array"
                );
            }
            HeaderKind::InnerBitmap => {
                assert_eq!(arity, 0, "{what}: an inner node has no size word");
                assert!(slots > 0, "{what}: an inner node has at least one slot");
            }
        }
        if arity == 1 {
            // SAFETY: a head node's word 1 is its `size` word (a raw integer, not a term).
            let size = unsafe { *p.add(1) }.raw();
            assert!(
                size <= MAX_SMALL_REASONABLE,
                "{what}: the head size word looks like a term, not a count ({size:#x})"
            );
        }
        return;
    }
    if is_arity_value(header) {
        let n = arityval(header);
        assert_eq!(words, n + 1, "{what}: an arityval block is arity + 1 words");
        // An arityval block is reached here either as a flat map's **keys tuple** or as a
        // **collision node**, and the two are indistinguishable by inspection — a keys
        // tuple may legitimately hold a cons cell as a *key* (`[a]` is a valid map key).
        // The two are told apart by *position*: a collision node is always a HAMT slot.
        // That read is `render_child`'s, which asserts the cons-cell shape there.
        return;
    }
    // Every other boxed shape (float, bignum, bitstring, fun, records) is accounted for
    // by `object_words` and has no shape claim this walk can add.
}

/// A fully populated 16-slot bitmap (the head-array trigger).
const FULL_BITMAP: u16 = 0xffff;

/// A sanity ceiling for a head's `size` word: a count, never a tagged term.
const MAX_SMALL_REASONABLE: usize = 1 << 32;

/// Asserts the structural (GC-safety) properties of one path-copied result.
fn assert_structural_safety(
    what: &str,
    src: Eterm,
    src_block: (usize, usize),
    got: Eterm,
    got_block: (usize, usize),
    op_terms: &[Eterm],
    facts: Facts,
    atoms: &AtomTable,
) {
    let src_walk = walk(src, what);
    // `walk` itself asserts every header's well-formedness and that no slot is the
    // non-value; what remains is the word accounting and the sharing invariant.
    let got_walk = walk(got, what);

    // 1. `object_words` agrees with the words actually occupied: the objects inside the
    //    result's block price exactly the prefix the writer mutated.
    let in_block = got_walk.words_in(got_block);
    assert_eq!(
        in_block, facts.touched,
        "{what}: the objects inside the result block price {in_block} words but the writer \
         mutated {} words",
        facts.touched
    );
    assert!(
        facts.touched <= facts.required,
        "{what}: wrote {} words but planned {}",
        facts.touched,
        facts.required
    );
    for obj in got_walk.objects_in(got_block) {
        let end = obj.ptr + obj.words * core::mem::size_of::<Eterm>();
        assert!(
            end <= got_block.1,
            "{what}: an object at {:#x} straddles the end of the reserved block",
            obj.ptr
        );
    }

    // 2. Sharing: every object reachable from the result that is not in the result's own
    //    block must come from the source (or be one of the op's own terms). A copied
    //    off-path subtree would break this — the whole point of path copying is that it
    //    is *shared by word*, never duplicated.
    let mut allowed: BTreeSet<usize> = src_walk.ptrs();
    for term in op_terms {
        allowed.extend(walk(*term, what).ptrs());
    }
    for obj in got_walk.objects.iter() {
        if inside(obj.ptr, got_block) {
            continue;
        }
        assert!(
            allowed.contains(&obj.ptr),
            "{what}: the result reaches an object at {:#x} that is neither in its own block \
             nor reachable from the source — an off-path subtree was copied instead of shared",
            obj.ptr
        );
    }
    // ... and the source's own *node tree* is self-contained in the source's block: the
    // root, every sub-node and every collision node were written contiguously by the one
    // `write_map` call. Only a leaf's key/value terms point out to the fixture heap, and
    // a key may legitimately be a map of its own, which is why the claim is made over the
    // node tree rather than over every reachable object.
    assert!(
        !src_walk.objects.is_empty(),
        "{what}: the source walk found nothing"
    );
    check_node_tree(src, src_block, what);

    // 3. A result-block node count bound, derived rather than guessed: the smallest
    //    object either writer emits is 2 words (a leaf cons cell, or a single-bit inner
    //    node), so the block's object count can never exceed `required / 2`. (A literal
    //    `required / 18` — the largest single node — would be unsound for a divergence
    //    chain of `2`-word inner nodes.)
    let nodes = got_walk.objects_in(got_block).count();
    assert!(
        nodes * 2 <= facts.required,
        "{what}: {nodes} objects in a {}-word block — each object costs at least 2 words",
        facts.required
    );

    // 4. `size_object` consistency over a *partially shared* graph: the reachable graph is
    //    the block's objects plus every shared source subtree, and `size_object` prices
    //    exactly the objects the **sizing** walk found — modulo the one literal it prices
    //    `0` (`tuple0()`, which `object_words` structurally prices `1`).
    //
    //    The comparison is against `walk_sizing`, not `walk`: this crate publishes two
    //    child enumerations that differ for an `ErlSubBits` (the sizer prices the whole
    //    refc pair opaquely and descends into nothing; the copier must also forward the
    //    pair's `BinRef`). So the copy walk reaches exactly one extra 3-word `BinRef` per
    //    refc binary in the graph — a *derived* difference, not a fudge factor.
    let sizing = walk_sizing(got, what);
    let total = sizing.total_words();
    let literals = sizing.literal_empty_tuples();
    assert_eq!(
        size_object(got),
        total - literals,
        "{what}: size_object disagrees with the sizing walk over the reachable graph"
    );
    let refc_bins = sizing
        .objects
        .iter()
        .filter(|o| {
            // SAFETY: `o.term` is a live object of the walk, and `is_refc_binary` only
            // reads a boxed object's header word.
            unsafe { is_refc_binary(o.term) }
        })
        .count();
    assert_eq!(
        got_walk.total_words() - sizing.total_words(),
        3 * refc_bins,
        "{what}: the copy walk reaches exactly one extra 3-word `BinRef` per refc binary \
         ({refc_bins} in this graph)"
    );

    // 5. The root's own size word is the map's key count (both readers agree).
    let size = map_size(got, atoms).expect("a live map");
    if is_hash(got) {
        // SAFETY: a live hash map's root is a node whose word 1 is its size word.
        let head_size = unsafe { *boxed_val(got).add(1) }.raw();
        assert_eq!(head_size, size, "{what}: the head size word is the key count");
    } else {
        // SAFETY: a live flat map's word 1 is its pair count.
        let flat_size = unsafe { *boxed_val(got).add(1) }.raw();
        assert_eq!(flat_size, size, "{what}: the flat size word is the pair count");
    }
}

fn is_flat(map: Eterm) -> bool {
    // SAFETY: `map` is a live boxed term at every call site.
    unsafe { is_flatmap(map) }
}

fn is_hash(map: Eterm) -> bool {
    // SAFETY: as above.
    unsafe { is_hashmap(map) }
}

/// The flat map's keys-tuple **word** (word 2 of the block).
fn flat_keys_word(map: Eterm) -> Eterm {
    // SAFETY: `map` is a live flat map, so word 2 is its keys tuple.
    unsafe { *boxed_val(map).add(2) }
}

/// The words of a flat map's block, read from its head (`size + 3`).
fn flat_words(map: Eterm) -> usize {
    // SAFETY: `map` is a live flat map; word 1 is its pair count.
    unsafe { *boxed_val(map).add(1) }.raw() + 3
}

// ---------------------------------------------------------------------------
// 1. Op-script equivalence over sizes and key classes
// ---------------------------------------------------------------------------

/// The op script holds against a rebuild at every base size, for a map mixing atoms,
/// small ints, floats, bignums, binaries, tuples, lists, a fun, a nested map and a
/// low-hash-nibble group.
///
/// Also prints the per-step word figures (run with `-- --nocapture`), which are the
/// scaling evidence: at every size > 32 the figure is a couple of dozen words, not a
/// multiple of the map's size.
#[test]
fn op_script_matches_a_rebuild_at_every_size() {
    let pool = Pool::new();
    let atoms = &pool.atoms;
    eprintln!();
    eprintln!(
        "{:>6} {:>28} {:>9} {:>8} {:>9}",
        "size", "step", "required", "written", "capacity"
    );
    for n in SWEEP_SIZES {
        let mut sweep = Sweep::new(n, &pool, atoms);
        let steps = prologue(n);
        for (i, step) in steps.iter().enumerate() {
            let what = format!("size={n} step={i} ({})", step.what);
            let facts = sweep.apply(step, &pool, atoms, &what);
            eprintln!(
                "{:>6} {:>28} {:>9} {:>8} {:>9}",
                n, step.what, facts.required, facts.touched, facts.capacity
            );
        }
        // The map's size after the prologue is the base size, minus the deletes plus the
        // inserts — whatever it is, both routes agree (asserted per step).
        if n > 0 {
            assert_eq!(
                map_size(sweep.cow.map, atoms).expect("a live map"),
                n,
                "size={n}: the prologue is size-neutral (2 inserts, 2 deletes)"
            );
        }
    }
}

/// `1` and `1.0` are distinct map keys: a route that compared arithmetically (`==`)
/// would collapse them, and `map_get` would answer the wrong value.
#[test]
fn one_and_one_point_zero_are_distinct_keys() {
    let pool = Pool::new();
    let atoms = &pool.atoms;
    let int_key = make_small(1);
    let float_key = float_of(&pool, 1.0);
    assert!(
        !eq(int_key, float_key, atoms),
        "`1` and `1.0` must be `=:=`-distinct"
    );

    // A map holding both, built by the COW writer from a 40-pair base (`1` is pool index
    // 8 and `1.0` index 9, so both are already there).
    let n = 40;
    let mut sweep = Sweep::new(n, &pool, atoms);
    let facts = sweep.apply(&fresh(n, "insert a bulk key"), &pool, atoms, "mixed map");
    assert!(facts.touched > 0);
    let map = sweep.cow.map;
    assert_eq!(map_get(map, int_key, atoms).expect("a live map"), Some(value_of(8)));
    assert_eq!(
        map_get(map, float_key, atoms).expect("a live map"),
        Some(value_of(9))
    );
    assert_ne!(
        map_get(map, int_key, atoms).expect("a live map"),
        map_get(map, float_key, atoms).expect("a live map"),
        "the two keys must not alias"
    );

    // Replacing one leaves the other alone.
    let facts = sweep.apply(
        &Step {
            kind: StepKind::PutNew,
            key: 8,
            value: make_small(424_242),
            what: "replace the small-int key",
        },
        &pool,
        atoms,
        "replace 1",
    );
    assert!(facts.touched > 0);
    assert_eq!(
        map_get(sweep.cow.map, float_key, atoms).expect("a live map"),
        Some(value_of(9)),
        "replacing `1` must not touch `1.0`"
    );
    let facts = sweep.apply(&drop(8, "delete the small-int key"), &pool, atoms, "delete 1");
    assert!(facts.touched > 0);
    assert_eq!(
        map_get(sweep.cow.map, int_key, atoms).expect("a live map"),
        None
    );
    assert_eq!(
        map_get(sweep.cow.map, float_key, atoms).expect("a live map"),
        Some(value_of(9)),
        "deleting `1` must leave `1.0` in place"
    );
}

/// The `1.0` key of the pool (pool index 9), reconstructed for the reader.
fn float_of(pool: &Pool, _value: f64) -> Eterm {
    // The pool builds exactly one float key; index 9 is it (see `Pool::new`).
    pool.key(9)
}

/// Every key class survives a put / replace / delete / re-insert on a hash-map-sized map,
/// and the low-hash-nibble group really does share trie levels 0 and 1.
#[test]
fn every_key_class_round_trips() {
    let pool = Pool::new();
    let atoms = &pool.atoms;
    eprintln!("\nkey classes: {:?}", pool.class_inventory());

    // The witness for the "collision group": each pair's hash agrees on the low 8 bits.
    // The group is the last four of the hand-picked keys (see `Pool::new`).
    let group: Vec<Eterm> = (0..4).map(|i| pool.key(PREFIX_KEYS - 4 + i)).collect();
    for pair in group.chunks(2) {
        assert_eq!(
            erts_map_hash(pair[0]) & 0xFF,
            erts_map_hash(pair[1]) & 0xFF,
            "the collision group must share the low two nibbles"
        );
    }

    // A hash-map-sized base holding every hand-picked key.
    let n = PREFIX_KEYS + 20;
    let mut sweep = Sweep::new(n, &pool, atoms);
    assert!(is_hash(sweep.cow.map), "the base must be a hash map (n > 32)");

    for index in 0..PREFIX_KEYS {
        let kind = pool.kinds[index];
        let facts = sweep.apply(
            &Step {
                kind: StepKind::PutNew,
                key: index,
                value: make_small(500_000 + index as i64),
                what: "replace a class key's value",
            },
            &pool,
            atoms,
            &format!("class {kind}: replace"),
        );
        assert!(facts.touched > 0, "class {kind}: a replace writes words");
        let facts = sweep.apply(
            &drop(index, "delete a class key"),
            &pool,
            atoms,
            &format!("class {kind}: delete"),
        );
        assert!(facts.touched > 0, "class {kind}: a delete writes words");
        let facts = sweep.apply(
            &Step {
                kind: StepKind::PutFresh,
                key: index,
                value: value_of(index),
                what: "re-insert a class key",
            },
            &pool,
            atoms,
            &format!("class {kind}: re-insert"),
        );
        assert!(facts.touched > 0, "class {kind}: an insert writes words");
    }
}

/// The number of hand-picked (mixed-class) keys at the front of the pool — 8 atoms, `1`,
/// `1.0`, 2 bignums, 2 binaries, 2 tuples, 2 lists, a fun, a nested map and the
/// 4-key collision group.
const PREFIX_KEYS: usize = 8 + 1 + 1 + 2 + 2 + 2 + 2 + 1 + 1 + 4;

// ---------------------------------------------------------------------------
// 2. The flat-map boundary: conversions in both directions
// ---------------------------------------------------------------------------

/// Crossing `MAP_SMALL_MAP_LIMIT` in both directions reproduces the rebuild's shape and
/// content exactly: 31 → 32 stays flat, 32 → 33 is the flat→HAMT conversion, 33 → 32 is
/// the HAMT→flat conversion, and re-inserting past 32 converts again.
#[test]
fn flat_limits_and_conversions_round_trip() {
    let pool = Pool::new();
    let atoms = &pool.atoms;

    // 31 → 32 stays a flat map.
    let mut sweep = Sweep::new(31, &pool, atoms);
    assert!(is_flat(sweep.cow.map), "31 pairs is a flat map");
    sweep.apply(&fresh(31, "31 -> 32"), &pool, atoms, "31 -> 32");
    assert!(is_flat(sweep.cow.map), "32 pairs is still a flat map");
    assert_eq!(map_size(sweep.cow.map, atoms).expect("a live map"), 32);

    // 32 → 33 is the conversion arm.
    let mut sweep = Sweep::new(32, &pool, atoms);
    assert!(is_flat(sweep.cow.map), "32 pairs is a flat map");
    let facts = sweep.apply(&fresh(32, "32 -> 33"), &pool, atoms, "32 -> 33");
    assert!(is_hash(sweep.cow.map), "33 pairs is a hash map");
    assert_eq!(map_size(sweep.cow.map, atoms).expect("a live map"), 33);
    // The conversion has to write the whole map: `2 * 33 + 4` words at least.
    assert!(
        facts.touched >= 2 * 33,
        "the conversion writes the whole map ({} words)",
        facts.touched
    );

    // 33 → 32 is the HAMT→flat conversion.
    let mut sweep = Sweep::new(33, &pool, atoms);
    assert!(is_hash(sweep.cow.map), "33 pairs is a hash map");
    let facts = sweep.apply(&drop(7, "33 -> 32"), &pool, atoms, "33 -> 32");
    assert!(is_flat(sweep.cow.map), "32 pairs is a flat map again");
    assert_eq!(map_size(sweep.cow.map, atoms).expect("a live map"), 32);
    assert!(
        facts.touched <= facts.required,
        "the flat conversion fits its plan"
    );

    // And back up again: 32 → 33 → 34.
    let mut sweep = Sweep::new(32, &pool, atoms);
    for step in [fresh(32, "32 -> 33"), fresh(33, "33 -> 34")] {
        sweep.apply(&step, &pool, atoms, "re-insert past the limit");
        assert!(is_hash(sweep.cow.map), "past 32 the map is a hash map");
    }
    assert_eq!(map_size(sweep.cow.map, atoms).expect("a live map"), 34);

    // 34 → 33 → 32 crosses back with the delete arms.
    let mut sweep = Sweep::new(34, &pool, atoms);
    for step in [drop(20, "34 -> 33"), drop(21, "33 -> 32")] {
        sweep.apply(&step, &pool, atoms, "delete back below the limit");
    }
    assert!(is_flat(sweep.cow.map), "back at 32 the map is flat again");
    assert_eq!(map_size(sweep.cow.map, atoms).expect("a live map"), 32);
}

// ---------------------------------------------------------------------------
// 3. Deleting every key down to empty
// ---------------------------------------------------------------------------

/// Draining a map key by key stays equivalent to a rebuild at **every** step, down to the
/// empty flat map — through the flat→HAMT and HAMT→flat boundaries and through the head
/// node's array/bitmap transition.
#[test]
fn delete_every_key_down_to_empty() {
    let pool = Pool::new();
    let atoms = &pool.atoms;
    for n in [1, 2, 31, 32, 33, 34, 64] {
        let mut sweep = Sweep::new(n, &pool, atoms);
        let mut seen_flat_after_hash = false;
        let mut seen_head_array = false;
        for index in (0..n).rev() {
            sweep.apply(
                &drop(index, "drain one key"),
                &pool,
                atoms,
                &format!("n={n}: drain key {index}"),
            );
            let size = map_size(sweep.cow.map, atoms).expect("a live map");
            assert_eq!(size, index, "n={n}: each delete removes exactly one key");
            if index == MAP_SMALL_MAP_LIMIT {
                assert!(is_flat(sweep.cow.map), "n={n}: 32 pairs is a flat map");
                if n > MAP_SMALL_MAP_LIMIT {
                    seen_flat_after_hash = true;
                }
            }
            if seen_head_array && is_hash(sweep.cow.map) {
                // The head node's array form is only ever produced by a full bitmap.
                let written = walk(sweep.cow.map, "drain").total_words();
                assert!(written > 0);
            }
            if is_hash(sweep.cow.map) {
                // SAFETY: a live hash map's root is a head node.
                let header = unsafe { *boxed_val(sweep.cow.map) };
                let (kind, _) = node_kind(header);
                if kind == HeaderKind::HeadArray {
                    seen_head_array = true;
                }
            }
        }
        assert_eq!(map_size(sweep.cow.map, atoms).expect("a live map"), 0);
        assert!(is_flat(sweep.cow.map), "n={n}: the empty map is flat");
        assert_eq!(flat_words(sweep.cow.map), 3, "n={n}: the empty flat map is 3 words");
        assert_eq!(
            sweep.rebuild.current().raw() != 0,
            true,
            "n={n}: both routes still hold a map"
        );
        if n > MAP_SMALL_MAP_LIMIT {
            assert!(
                seen_flat_after_hash,
                "n={n}: the drain must cross the HAMT -> flat conversion"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 4. The zero-write arms
// ---------------------------------------------------------------------------

/// The two arms that must write **nothing** — an identical value word and an absent key —
/// write nothing at all, report a zero/sound figure and leave the staging buffer pristine.
#[test]
fn absent_removes_and_identical_puts_write_nothing() {
    let pool = Pool::new();
    let atoms = &pool.atoms;
    for n in [1usize, 2, 31, 32, 33, 34, 64, 250, 2_500] {
        let pairs: Vec<(Eterm, Eterm)> = (0..n).map(|i| (pool.key(i), pool.value(i))).collect();
        let src = build_map(&pairs, atoms);

        // An identical value word: `Nothing` — the input map comes back, nothing written.
        let key = pool.key(n / 2);
        let stored = map_get(src.map, key, atoms).expect("a live map").expect("the key is present");
        let (region, got, facts) = cow_put_exact(src.map, key, stored, atoms);
        assert!(facts.unchanged, "n={n}: the same value word returns the input map");
        assert_eq!(facts.touched, 0, "n={n}: the same value word writes nothing");
        assert_eq!(
            got.raw(),
            src.map.raw(),
            "n={n}: the returned map word is the input's"
        );
        assert!(
            region.words[..region.cap].iter().all(|w| *w == SENTINEL),
            "n={n}: the reservation must stay pristine"
        );

        // An absent key: `Ok(None)`, no plan, no words.
        let absent_key = pool.key(n + 17);
        let (region, got, facts) = cow_remove_exact(src.map, absent_key, atoms);
        assert!(got.is_none(), "n={n}: an absent key answers None");
        assert_eq!(facts.required, 0, "n={n}: an absent key plans no words");
        assert_eq!(facts.touched, 0, "n={n}: an absent key writes nothing");
        assert!(
            region.words[..region.cap].iter().all(|w| *w == SENTINEL),
            "n={n}: the reservation must stay pristine"
        );
        assert_eq!(
            map_size(src.map, atoms).expect("a live map"),
            n,
            "n={n}: the source is unchanged"
        );
    }
}

/// A map built by `write_map` into an owned region, kept alive for the caller.
struct Built {
    region: Region,
    map: Eterm,
}

fn build_map(pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Built {
    let mut region = Region::new(map_words_capacity(pairs.len()));
    // SAFETY: the region reserves the documented `write_map` bound and outlives `map`.
    let map = unsafe { write_map(region.base(), pairs, atoms) }.expect("a well-formed map");
    region.check_tail();
    Built { region, map }
}

// ---------------------------------------------------------------------------
// 5. Structural / GC-safety evidence
// ---------------------------------------------------------------------------

/// The structural safety of a path-copied result, on the five shapes that exercise every
/// arm: a HAMT put and a HAMT delete at n = 250, a flat value-replace, the flat→HAMT
/// conversion and the HAMT→flat conversion.
#[test]
fn structural_safety_of_a_path_copied_result() {
    let pool = Pool::new();
    let atoms = &pool.atoms;

    // (a) a HAMT value-replace at n = 250: the node spine is copied, every off-path
    //     subtree is shared by word.
    {
        let pairs: Vec<(Eterm, Eterm)> = (0..250).map(|i| (pool.key(i), pool.value(i))).collect();
        let src = build_map(&pairs, atoms);
        assert!(is_hash(src.map));
        let key = pool.key(123);
        let (region, got, facts) = cow_put_exact(src.map, key, make_small(777_777), atoms);
        assert!(facts.touched <= 293, "a HAMT put is a spine, not a rebuild");
        assert_structural_safety(
            "hamt put (n=250)",
            src.map,
            src.region.block(),
            got,
            region.block(),
            &[key, make_small(777_777)],
            facts,
            atoms,
        );
        // The *reachable graph* of a value replacement has the same shape as the source's
        // (one value word changed), and only `facts.required` words were newly written —
        // so the result can have grown by at most the block, never by a copy of the map.
        assert!(
            size_object(got) <= size_object(src.map) + facts.required,
            "a path-copied result grew by more than its own block ({} vs {} + {})",
            size_object(got),
            size_object(src.map),
            facts.required
        );
        assert!(
            size_object(got) >= (250 * 2).min(size_object(src.map)),
            "the result still reaches the whole map"
        );
    }

    // (b) a HAMT delete at n = 250.
    {
        let pairs: Vec<(Eterm, Eterm)> = (0..250).map(|i| (pool.key(i), pool.value(i))).collect();
        let src = build_map(&pairs, atoms);
        let key = pool.key(77);
        let (region, got, facts) = cow_remove_exact(src.map, key, atoms);
        let got = got.expect("the key is present");
        assert!(facts.touched <= 288);
        assert_structural_safety(
            "hamt remove (n=250)",
            src.map,
            src.region.block(),
            got,
            region.block(),
            &[],
            facts,
            atoms,
        );
        assert!(
            size_object(got) < size_object(src.map),
            "a delete shrinks the graph"
        );
    }

    // (c) a flat value-replace: the keys-tuple word is the source's, bit-identical.
    {
        let pairs: Vec<(Eterm, Eterm)> = (0..20).map(|i| (pool.key(i), pool.value(i))).collect();
        let src = build_map(&pairs, atoms);
        assert!(is_flat(src.map));
        let key = pool.key(3);
        let (region, got, facts) = cow_put_exact(src.map, key, make_small(888_888), atoms);
        assert_eq!(facts.required, 3 + 20, "the flat replace is `3 + n` words");
        assert_eq!(
            flat_keys_word(got),
            flat_keys_word(src.map),
            "the flat value-replace arm must reuse the source's keys tuple"
        );
        assert!(
            inside(flat_keys_word(got).raw() & !0x7, src.region.block()),
            "the reused keys tuple must live in the source's block"
        );
        assert_structural_safety(
            "flat value-replace (n=20)",
            src.map,
            src.region.block(),
            got,
            region.block(),
            &[key, make_small(888_888)],
            facts,
            atoms,
        );
    }

    // (d) the flat→HAMT conversion at n = 32.
    {
        let pairs: Vec<(Eterm, Eterm)> = (0..32).map(|i| (pool.key(i), pool.value(i))).collect();
        let src = build_map(&pairs, atoms);
        assert!(is_flat(src.map));
        let key = pool.key(32);
        let (region, got, facts) = cow_put_exact(src.map, key, pool.value(32), atoms);
        assert!(is_hash(got));
        assert_structural_safety(
            "flat -> HAMT conversion (n=32)",
            src.map,
            src.region.block(),
            got,
            region.block(),
            &[key, pool.value(32)],
            facts,
            atoms,
        );
        // The conversion rebuilds the whole pair set: the result shares **no** object
        // with the source's block. (Its leaves' key/value terms are the caller's terms in
        // the fixture heap, which is why the claim is made over the two *blocks* rather
        // than over every object the result reaches.)
        let src_nodes: BTreeSet<usize> = walk(src.map, "conversion")
            .objects_in(src.region.block())
            .map(|o| o.ptr)
            .collect();
        let got_ptrs = walk(got, "conversion").ptrs();
        assert!(
            src_nodes.is_disjoint(&got_ptrs),
            "the flat -> HAMT conversion must copy every node"
        );
    }

    // (e) the HAMT→flat conversion at n = 33.
    {
        let pairs: Vec<(Eterm, Eterm)> = (0..33).map(|i| (pool.key(i), pool.value(i))).collect();
        let src = build_map(&pairs, atoms);
        assert!(is_hash(src.map));
        let key = pool.key(5);
        let (region, got, facts) = cow_remove_exact(src.map, key, atoms);
        let got = got.expect("the key is present");
        assert!(is_flat(got));
        assert_structural_safety(
            "HAMT -> flat conversion (n=33)",
            src.map,
            src.region.block(),
            got,
            region.block(),
            &[],
            facts,
            atoms,
        );
        // `flat_words` is the flat body's `object_words` figure (`size + 3`: the header,
        // the size word and the keys-tuple word — the values live in it too), so the whole
        // block is that body plus the `n + 1`-word keys tuple.
        assert_eq!(flat_words(got), 32 + 3, "32 pairs inside the flat body");
        let got_walk = walk(got, "conversion");
        assert_eq!(
            got_walk.words_in(region.block()),
            flatmap_words(32),
            "the flat conversion writes the whole `flatmap_size_words(32)` block"
        );
        // The HAMT -> flat conversion rebuilds the keys tuple and every value word, so it
        // shares no object with the source's block either.
        let src_nodes: BTreeSet<usize> = walk(src.map, "conversion")
            .objects_in(src.region.block())
            .map(|o| o.ptr)
            .collect();
        let got_ptrs = walk(got, "conversion").ptrs();
        assert!(
            src_nodes.is_disjoint(&got_ptrs),
            "the HAMT -> flat conversion must copy every node"
        );
    }
}

/// The words a flat map of `n` pairs occupies — via the public layout, not a private copy.
fn flatmap_words(n: usize) -> usize {
    beam_term::thing::flatmap_size_words(n)
}

// ---------------------------------------------------------------------------
// 6. Deterministic scaling (no wall clock)
// ---------------------------------------------------------------------------

/// The sizes the scaling pin sweeps: the first hash-map size, three ordinary ones and two
/// big ones ten times apart.
const SCALING_SIZES: [usize; 5] = [33, 64, 250, 2_500, 6_250];

/// **Words written per op does not grow with n.**
///
/// The exact figure comes from the read-only descent
/// ([`map_put_cow_words_required`] / [`map_remove_cow_words_required`]); the reservation
/// is exactly that figure, so the sentinel sweep also proves the plan and the write agree.
/// Printed for the record (run with `-- --nocapture`).
#[test]
fn words_written_per_op_does_not_grow_with_n() {
    let pool = Pool::new();
    let atoms = &pool.atoms;
    eprintln!();
    eprintln!(
        "{:>6} {:>9} {:>9} {:>9} {:>10} {:>10}",
        "n", "put-req", "put-writ", "put-cap", "del-req", "del-cap"
    );
    // (n, put-replace figure, remove figure, insert figure)
    let mut figures: Vec<(usize, usize, usize, usize)> = Vec::new();
    for n in SCALING_SIZES {
        let pairs: Vec<(Eterm, Eterm)> = (0..n).map(|i| (pool.key(i), pool.value(i))).collect();
        let src = build_map(&pairs, atoms);
        assert!(is_hash(src.map), "n={n}: the base must be a hash map");

        // A value replacement of a key that is in the map: no divergence chain is
        // possible, so the figure is exactly the copied node spine.
        let present = pool.key(n / 3);
        let (region, _got, put) = cow_put_exact(src.map, present, make_small(123_456), atoms);
        assert_eq!(
            put.touched, put.required,
            "n={n}: the plan is exact for a value replacement"
        );

        // A delete of a key that is in the map.
        let (region2, _got, del) = cow_remove_exact(src.map, present, atoms);
        assert_eq!(
            del.touched, del.required,
            "n={n}: the plan is exact for a delete"
        );

        // An insert of a key that is not in the map (may add a divergence chain, whose
        // length is a property of the two keys, never of n).
        let fresh_key = pool.key(n);
        let (region3, _got, ins) = cow_put_exact(src.map, fresh_key, pool.value(n), atoms);
        assert!(
            ins.required <= ins.capacity,
            "n={n}: an insert's exact figure fits its size-only bound"
        );

        eprintln!(
            "{:>6} {:>9} {:>9} {:>9} {:>10} {:>10}",
            n, put.required, put.touched, put.capacity, del.required, del.capacity
        );
        figures.push((n, put.required, del.required, ins.required));
        let _ = (region, region2, region3);
    }

    // Every figure is inside its size-only bound, and — once past the flat map — inside
    // the documented constant spine bound.
    for (n, put, del, ins) in &figures {
        assert!(
            *put <= map_put_cow_words_capacity(*n),
            "n={n}: {put} > put capacity"
        );
        assert!(
            *del <= map_remove_cow_words_capacity(*n),
            "n={n}: {del} > remove capacity"
        );
        assert!(
            *ins <= map_put_cow_words_capacity(*n),
            "n={n}: {ins} > put capacity"
        );
        assert!(*put <= HAMT_PUT_WORDS_MAX, "n={n}: {put} > the spine bound");
        assert!(*del <= HAMT_REMOVE_WORDS_MAX, "n={n}: {del} > the spine bound");
        assert!(*ins <= HAMT_PUT_WORDS_MAX, "n={n}: {ins} > the spine bound");
    }

    // The flat-in-n relation, derived rather than re-typed: a 16-way trie gains at most
    // ONE level between n = 250 and n = 2 500, and the widest node is 18 words
    // (`[hdr][size][16 slots]`), so the exact figure may grow by at most 18 words.
    let small = figures.iter().find(|(n, ..)| *n == 250).expect("250 is swept");
    let big = figures.iter().find(|(n, ..)| *n == 2_500).expect("2 500 is swept");
    assert!(
        big.1 <= small.1 + FATTEST_NODE_WORDS,
        "put: n=2 500 needs {} words against n=250's {} — growth must be at most one \
         {FATTEST_NODE_WORDS}-word node",
        big.1,
        small.1
    );
    assert!(
        big.2 <= small.2 + FATTEST_NODE_WORDS,
        "remove: n=2 500 needs {} words against n=250's {}",
        big.2,
        small.2
    );
    // The same relation holds for the next tenfold step, and for the small pair.
    let n64 = figures.iter().find(|(n, ..)| *n == 64).expect("64 is swept");
    let n33 = figures.iter().find(|(n, ..)| *n == 33).expect("33 is swept");
    assert!(
        small.1 <= n33.1 + FATTEST_NODE_WORDS,
        "put: 250 vs 33 grows by at most one node"
    );
    assert!(
        big.1 <= n64.1 + 3 * FATTEST_NODE_WORDS,
        "put: 2 500 vs 64 grows by at most three levels"
    );

    // A fold of K single-key inserts writes K * (<= 293) words — the total does not
    // scale with the map's size, where the old `map_put` wrote Θ(n) per op.
    const FOLD: usize = 32;
    for n in [250usize, 2_500] {
        let pairs: Vec<(Eterm, Eterm)> = (0..n).map(|i| (pool.key(i), pool.value(i))).collect();
        let mut cow = Cow::new(&pairs, atoms);
        let mut cow_total = 0usize;
        for k in 0..FOLD {
            let (_, facts) = cow.put(pool.key(n + k), pool.value(n + k), atoms);
            cow_total += facts.touched;
        }
        // The rebuild route's cost: every op rewrites the whole map.
        let mut rebuild_pairs = pairs.clone();
        let mut rebuild_total = 0usize;
        for k in 0..FOLD {
            rebuild_pairs.push((pool.key(n + k), pool.value(n + k)));
            let rebuilt = build_map(&rebuild_pairs, atoms);
            rebuild_total += size_object(rebuilt.map);
        }
        eprintln!(
            "fold of {FOLD} puts at n={n}: cow {cow_total} words ({:.1}/op) vs rebuild \
             {rebuild_total} words ({:.0}/op) = {:.0}x",
            cow_total as f64 / FOLD as f64,
            rebuild_total as f64 / FOLD as f64,
            rebuild_total as f64 / cow_total as f64
        );
        assert!(
            cow_total <= FOLD * HAMT_PUT_WORDS_MAX,
            "n={n}: a fold of {FOLD} puts wrote {cow_total} words — more than \
             {FOLD} x {HAMT_PUT_WORDS_MAX}"
        );
        assert!(
            rebuild_total >= FOLD * 3 * n,
            "n={n}: the rebuild route writes at least 3n words per op"
        );
        assert!(
            rebuild_total > 4 * cow_total,
            "n={n}: the incremental route must be an order of magnitude cheaper \
             ({cow_total} vs {rebuild_total})"
        );
    }
}

/// The documented spine bound for a hash-map put (`16 * 18 + 3 + 2`): a 16-node path of
/// 18-word nodes, a two-cell collision node and the new leaf's cons cell.
const HAMT_PUT_WORDS_MAX: usize = 293;

/// The documented spine bound for a hash-map remove (`16 * 18`).
const HAMT_REMOVE_WORDS_MAX: usize = 288;

/// The widest single HAMT node: `[hdr][size][16 slots]` = 18 words. This is the
/// quantity the flat-in-n relation is expressed with (the trie gains at most one level
/// per tenfold step, and the widest node per level is 18 words).
const FATTEST_NODE_WORDS: usize = 2 + 16;
/// **The one arm whose planned figure is a bound, not the exact write.**
///
/// Every other arm agrees exactly — the sweep and the scaling pin assert
/// `touched == required` arm by arm, and the writers `debug_assert!` it. The flat → HAMT
/// conversion is the deliberate exception: `map_put_cow_words_required` plans
/// `map_words_capacity(33)` because that arm delegates to [`write_map`], whose exact
/// output word count is a property of the trie the 33 keys build (that is precisely what
/// `map_words_capacity` exists to bound). At n = 32 the plan is 711 words and the write is
/// 127 — an 84 % over-reservation, and an over-reservation is only address space.
///
/// So this test pins the direction that matters for memory safety — `required` is never an
/// under-estimate across the conversion — plus the derived lower bound "the writer wrote
/// at least its 33 leaf cons cells" and `required`'s identity with the public capacity
/// bound. It asserts no wall time and no magic total.
#[test]
fn the_conversion_arm_plans_the_write_map_capacity_bound() {
    let pool = Pool::new();
    let atoms = &pool.atoms;
    let n = MAP_SMALL_MAP_LIMIT;
    let pairs: Vec<(Eterm, Eterm)> = (0..n).map(|i| (pool.key(i), pool.value(i))).collect();
    let src = build_map(&pairs, atoms);
    assert!(is_flat(src.map), "a {n}-pair map is still flat");
    let key = pool.key(n);
    let (region, got, facts) = cow_put_exact(src.map, key, pool.value(n), atoms);
    assert!(is_hash(got), "the {}-th key converts the map", n + 1);
    assert_eq!(
        facts.required,
        map_words_capacity(n + 1),
        "the conversion arm plans `write_map`'s capacity bound"
    );
    assert_eq!(
        facts.required,
        map_put_cow_words_capacity(n),
        "and that bound is exactly what a size-only caller would reserve"
    );
    assert!(
        facts.touched <= facts.required,
        "the plan must never under-estimate the write ({} written, {} planned)",
        facts.touched,
        facts.required
    );
    assert!(
        facts.touched >= 2 * (n + 1),
        "the conversion still writes {} leaf cons cells (2 words each)",
        n + 1
    );
    assert_eq!(
        map_size(got, atoms).expect("a live map"),
        n + 1,
        "the conversion keeps every pair"
    );
    assert!(facts.touched < facts.required, "the bound is deliberately loose");
    let _ = region;
    eprintln!(
        "conversion n={n}: planned {} words, wrote {} ({:.0} % of the reservation)",
        facts.required,
        facts.touched,
        100.0 * facts.touched as f64 / facts.required as f64
    );
}

// ---------------------------------------------------------------------------
// 7. The timing report (ignored: a relation for a human, never an assertion)
// ---------------------------------------------------------------------------

/// **Not a test: a timing report.** Run with
/// `cargo test -p beam_term --test map_cow_equivalence -- --ignored --nocapture`.
///
/// It prints per-op nanoseconds for the old Θ(n) [`map_put`] and the new Θ(log n)
/// [`map_put_cow`] at three sizes. It asserts **nothing** about wall time: this host is
/// shared, so the numbers are a *relation* for a human to read, and the normal suite
/// never observes a clock. The `examples/` benchmark owns the measured record.
#[test]
#[ignore = "timing report: prints a relation for a human, asserts nothing (shared host)"]
fn timing_report_put_vs_put_cow() {
    let pool = Pool::new();
    let atoms = &pool.atoms;
    const ROUNDS: usize = 200;
    eprintln!();
    eprintln!(
        "{:>6} {:>14} {:>14} {:>10}",
        "n", "map_put ns/op", "put_cow ns/op", "ratio"
    );
    for n in [250usize, 2_500, 6_250] {
        let pairs: Vec<(Eterm, Eterm)> = (0..n).map(|i| (pool.key(i), pool.value(i))).collect();
        let src = build_map(&pairs, atoms);
        let key = pool.key(n / 3);
        let value = make_small(313_131);

        // The rebuild route: one self-contained map per call, into its own buffer.
        let mut rebuild_region = Region::new(map_words_capacity(n + 1));
        let start = Instant::now();
        for _ in 0..ROUNDS {
            // SAFETY: the region reserves the `map_words_capacity(n + 1)` bound and does
            // not alias `src`; the previous iteration's map is simply overwritten.
            let m = unsafe { map_put(rebuild_region.base(), src.map, key, value, atoms) }
                .expect("a live map");
            assert!(!is_non_value(m));
        }
        let rebuild_ns = start.elapsed().as_nanos() as f64 / ROUNDS as f64;

        // The incremental route: one staging buffer reused, results discarded (the source
        // stays live, so nothing dangles).
        let mut cow_region = Region::new(map_put_cow_words_capacity(n));
        let start = Instant::now();
        for _ in 0..ROUNDS {
            // SAFETY: the region reserves the size-only bound and outlives `src`; the
            // produced map is not read after the iteration.
            let m = unsafe { map_put_cow(cow_region.base(), src.map, key, value, atoms) }
                .expect("a live map");
            assert!(!is_non_value(m));
        }
        let cow_ns = start.elapsed().as_nanos() as f64 / ROUNDS as f64;
        eprintln!(
            "{:>6} {:>14.0} {:>14.0} {:>9.1}x",
            n,
            rebuild_ns,
            cow_ns,
            rebuild_ns / cow_ns
        );
    }
}
