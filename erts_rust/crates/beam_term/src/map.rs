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

//! Maps: the flat-map / HAMT (`CHAMP`) builder, lookups and structural size.
//!
//! Owns the *second* frozen interior layout this crate is responsible for: the
//! Erlang map. A map is a boxed term whose header is a
//! [`MAP_SUBTAG`](crate::tag::MAP_SUBTAG) header (see [`crate::boxed`]). There are
//! exactly two shapes, both pinned line-by-line to the legacy C ground truth
//! (`erts/emulator/beam/erl_map.h`, `erl_map.c`):
//!
//! * a **flat map** (`MAP_HEADER_FLATMAP`) holding up to
//!   [`MAP_SMALL_MAP_LIMIT`] pairs, whose key tuple is sorted in the internal
//!   *map-key order* (`erts_cmp_flatmap_keys`); its layout is written by
//!   [`crate::thing::write_flatmap`];
//! * a **hash map** — a 16-way (`4` hash bits per level, `HAMT_MAX_LEVEL = 16`)
//!   compressed hash-array-mapped trie above the limit, whose node layouts are
//!   written by this module.
//!
//! # The HAMT node layout (frozen)
//!
//! Level `L` of the trie (root `L = 0`) selects the slot
//! `maskval(swizzle(erts_map_hash(key)), L) = (erts_map_hash(key) >> 4*L) & 0xF`
//! of the node reached so far — i.e. the **`L`-th least-significant** nibble of the
//! key's [`erts_map_hash`](crate::ihash::erts_map_hash) indexes level `L` (the C
//! `maskval`, `erl_map.c:109`, applied to the swizzled hash; see
//! [`crate::ihash::hashmap_nibble`]). A node is one of
//!
//! | node | header | words | layout |
//! |---|---|---|---|
//! | head, 16 slots | `MAP_HEADER_HAMT_HEAD_ARRAY` | `18` | `[hdr][size][16 slots]` |
//! | head, bitmap | `MAP_HEADER_HAMT_HEAD_BITMAP(bmp)` | `2 + n` | `[hdr][size][n slots]` |
//! | inner, bitmap | `MAP_HEADER_HAMT_NODE_BITMAP(bmp)` | `1 + n` | `[hdr][n slots]` |
//! | collision | `make_arityval(n)` | `1 + n` | a normal tuple of `n` kv cons cells |
//!
//! where `n = popcount(bmp)` is the number of set bits (the *bitmap* marks the
//! present nibbles) and each slot is **one term word**: either a leaf (a
//! two-word `[Key, Value]` cons cell), a pointer to a sub-node, or — at the last
//! level — a collision-node tuple. A key whose subtree is unambiguous is reached
//! through a chain of single-bit inner nodes down to the level where it diverges
//! from its sibling. The collision-node cells are sorted in `CMP_TERM` order
//! (`crate::compare::cmp`), while a flat map's keys are sorted in
//! `erts_cmp_flatmap_keys` order ([`crate::compare::cmp_flatmap_keys`]) — atoms by
//! *index*, everything else by term order.
//!
//! The head node carries the map's key count in its `size` word (`hashmap_size`),
//! which is what [`map_size`] and `erts_map_size` report. This layout is what
//! `beam_gc::copy::move_size` already sizes
//! (`nelts = header_arity(hdr) + map_header_val(hdr).count_ones()`), so it must be
//! reproduced exactly.
//!
//! # Heap-agnostic contract
//!
//! `beam_term` cannot allocate (it is a dependency *sink*), so every writer takes
//! a caller-reserved `hp: *mut Eterm` base; the caller sizes it with
//! [`map_words_capacity`] (production: `beam_alloc`; tests: [`crate::heap::Heap`]).
//!
//! # Incremental update (path copying)
//!
//! [`write_map`] rebuilds a whole map — Θ(n) work and Θ(n) fresh words — which is
//! what a from-scratch builder needs, but not what one `maps:put/3` should cost.
//! The *incremental* writers [`map_put_cow`] / [`map_remove_cow`] (implemented in
//! `src/map/cow.rs`) reproduce the C's path-copying shape (C `erts_maps_put`,
//! `erts_hashmap_insert`/`_down`/`_up` and `hashmap_delete`): they copy **only the
//! nodes on the hash path** into `hp` and *share every off-path subtree, keys tuple
//! and collision cell by word*, because terms are immutable and never relocated in
//! place — so sharing a word is a copy-free copy. A single-key update of a large
//! map therefore costs Θ(log n) words instead of Θ(n).
//!
//! Two consequences the caller owns:
//!
//! * the produced map **references the source map's storage**, so `hp` must live in
//!   a heap that outlives the source map for as long as the result is used;
//! * the reservation is asked for with [`map_put_cow_words_capacity`] /
//!   [`map_remove_cow_words_capacity`] (a size-only bound), while
//!   [`map_put_cow_words_required`] / [`map_remove_cow_words_required`] answer the
//!   exact figure from a read-only descent — the difference matters only for a
//!   hash collision, whose node a map's *size* cannot bound.
//!
//! # Hash function
//!
//! The trie is driven by the **bit-exact** [`erts_map_hash`](crate::ihash::erts_map_hash)
//! port in [`crate::ihash`] (`erl_term_hashing.c`), so a `write_map`-built hash map
//! places its keys in the same HAMT slots as the oracle and therefore *stores* its
//! pairs in the same order (`maps:to_list/1` order). [`write_map`] lays the leaf
//! cons cells out sorted **ascending by `swizzle_map_hash(hash)`** — the nibble
//! reversal of the hash (`erl_map.c:1170` `hxnodecmp`) — with equal-hash runs
//! broken by [`crate::compare::cmp`] on the key (the C's `hxnodecmpkey` collision
//! order). Because the swizzle is a nibble reversal, "ascending swizzle" sorts by
//! the *least*-significant nibble first, exactly the order in which
//! [`build_trie`] consumes [`hashmap_nibble`], so equal-nibble runs stay contiguous
//! at every level and one contiguous grouping pass reproduces the C's
//! ascending-nibble slot order.
//!
//! # Deduplication (sort once, duplicates become adjacent)
//!
//! [`write_map`] collapses duplicate keys with **one stable sort followed by one
//! linear pass over adjacent [`eq`]-equal runs** (`collapse_eq_runs`), the shape of
//! the C `erts_validate_and_sort_flatmap` (`erl_map.c`) — never a per-insertion scan.
//! Both branches' sort keys (`cmp_flatmap_keys`, and the hash branch's
//! `(swizzle, cmp)` pair) order `eq`-equal keys as `Equal`, so `eq`-equal keys are
//! necessarily *adjacent* — and, the sort being a **stable** one, in input order.
//! One run is therefore one key, and keeping the run's first element while taking
//! its last value is exactly the first-occurrence-key / last-value result of a
//! per-insertion scan.
//! Each sort is a total order on the surviving set, so the produced map stays
//! independent of the caller's pair order.

use crate::atom::AtomTable;
use crate::boxed::{
    boxed_val, is_arity_value, is_boxed_term, is_flatmap_header, is_map_header, make_arityval,
    make_boxed, map_header_type, map_header_val,
};
use crate::compare::{cmp, cmp_flatmap_keys, eq};
use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::ihash::{erts_map_hash, hashmap_nibble, swizzle_map_hash};
use crate::list::{car, cdr, is_list, make_list, CAR_OFFSET, CDR_OFFSET};
use crate::tag::{
    MAP_HEADER_TAG_HAMT_HEAD_ARRAY, MAP_HEADER_TAG_HAMT_HEAD_BITMAP,
    MAP_HEADER_TAG_HAMT_NODE_BITMAP, MAP_HEADER_ARITY_SZ, MAP_HEADER_TAG_SZ, MAP_SUBTAG,
};
use crate::thing::{flatmap_parts, flatmap_size_words, write_flatmap};
use crate::tuple::{tuple_element, tuple_size};

/// `MAP_SMALL_MAP_LIMIT` (`erl_map.h:96`): the largest flat-map pair count. A map
/// with at most this many pairs is a flat map; a larger one is a hash map. (The C
/// only lowers this to `3` under `DEBUG`; the release value is the one that pins
/// the observable layout, so it is used unconditionally here.)
pub const MAP_SMALL_MAP_LIMIT: usize = 32;

/// `HAMT_MAX_LEVEL` (`erl_map.h:220`): `64 / 4` — the number of 4-bit levels in a
/// 64-bit hash. A node at level `L` uses [`hashmap_nibble`]`(hash, L)`.
pub const HAMT_MAX_LEVEL: usize = 16;

/// The (inclusive) deepest level a *node* may occupy; its children are leaves or
/// collision nodes.
const HAMT_LAST_NODE_LEVEL: usize = HAMT_MAX_LEVEL - 1;

/// A fully populated 16-slot bitmap (the head-array trigger).
const HAMT_FULL_BITMAP: u16 = 0xffff;/// `MAKE_MAP_HEADER(Type, Arity, Val)` (`erl_map.h:181`):
/// `_make_header((((Uint16)Val << MAP_HEADER_ARITY_SZ) | Arity) << MAP_HEADER_TAG_SZ | Type,
/// MAP_SUBTAG)`.
pub const fn make_map_header(map_type: usize, arity: usize, val: usize) -> Eterm {
    let inner = (((val as u16) as usize) << MAP_HEADER_ARITY_SZ) | arity;
    crate::boxed::make_header((inner << MAP_HEADER_TAG_SZ) | map_type, MAP_SUBTAG)
}

/// The words a caller must reserve before [`write_map`] with `n_pairs` input
/// pairs.
///
/// For a flat map this is exact ([`flatmap_size_words`]). For a hash map it is a
/// **sound upper bound** (never an under-estimate), derived from the trie shape:
///
/// * `2n` words for the `n` two-word key/value cons cells;
/// * at most `3n/2` words for collision nodes — every collision node holds `>= 2`
///   cells, so `(1 + c)` summed over at most `n/2` nodes is `<= 1.5n`;
/// * at most `17n` words for the trie nodes. A node at depth `d` exists only when
///   `>= 2` of the `n` keys share its `d`-nibble prefix, so (each key has exactly
///   one `d`-prefix) there are at most `n/2` such prefixes per depth and at most
///   `8n` nodes over the 16 depths. With `sum(slots) = nodes + leaves - 1`, the
///   node words `sum(1 + slots)` plus the head's `size` word are `2 * nodes +
///   leaves <= 2 * 8n + n = 17n`.
///
/// Because [`write_map`] deduplicates its input, reserving for the *input* length
/// is always sufficient for the produced map.
pub const fn map_words_capacity(n_pairs: usize) -> usize {
    if n_pairs <= MAP_SMALL_MAP_LIMIT {
        flatmap_size_words(n_pairs)
    } else {
        21 * n_pairs + 18
    }
}

// ---------------------------------------------------------------------------
// Temporary trie (built in Rust memory, then serialized into `hp`)
// ---------------------------------------------------------------------------

/// One slot of a [`TrieNode`].
enum TrieChild {
    /// A leaf: a `[Key, Value]` cons cell (one key in this subtree).
    Leaf(Eterm),
    /// A collision node: `>= 2` cons cells sharing a full 64-bit hash, sorted in
    /// `CMP_TERM` (map-key) order.
    Collision(Vec<Eterm>),
    /// A sub-node one level deeper.
    Node(Box<TrieNode>),
}

/// A trie node: the bitmap of present nibbles plus the slots in ascending nibble
/// order (one per set bit).
struct TrieNode {
    bitmap: u16,
    children: Vec<TrieChild>,
}

/// Builds the trie for `leaves` (sorted ascending by `swizzle_map_hash(hash)`, all
/// sharing nibbles `0..level-1`) at `level`.
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
            // Reaching here means every hash in the group agreed on all 16
            // nibbles: identical hashes, i.e. a collision node.
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

/// The words the serialized form of `node` (at `level`) occupies, excluding the
/// cons cells it points at.
fn trie_words(node: &TrieNode, level: usize) -> usize {
    let nslots = node.bitmap.count_ones() as usize;
    // Head nodes carry a `size` word; inner nodes do not.
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

/// Writes `node` (at `level`) into `*hp`, advancing `*hp` past it, and returns
/// the tagged node term.
///
/// # Safety
///
/// The caller must have reserved the words [`trie_words`] reports for this node
/// and its sub-nodes (plus, for the head, its `size` word), contiguously from
/// `*hp`.
fn serialize_node(
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
            TrieChild::Node(sub) => serialize_node(sub, level + 1, total_keys, hp),
        };
        slots.push(term);
    }

    let p = *hp;
    let bitmap = node.bitmap;
    let nslots = bitmap.count_ones() as usize;
    // `MAP_HEADER_HAMT_HEAD_ARRAY` when every slot is present, else a bitmap head;
    // inner nodes are always bitmaps (`erl_map.c:1146`).
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

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Collapses every maximal run of **adjacent** [`eq`]-equal keys in `items` to one
/// element: the run's **first** element (keeping its key word, and on the hash branch
/// its `erts_map_hash`), carrying the run's **last** value.
///
/// `key` reads the element's key word and `take_value` overwrites the first element's
/// value word from a later one; both operate on the element tuple in **host** memory,
/// so the collapse never touches the reserved heap words.
/// `items` must already be sorted by a total order that orders `eq`-equal keys as
/// `Equal` with a **stable** sort, so that each run is contiguous and in input order;
/// then this is exactly the first-occurrence-key / last-value result of a
/// per-insertion scan, in one linear pass. See the module's *Deduplication* section
/// for why both branches' sort keys have that property.
fn collapse_eq_runs<T: Copy>(
    items: &mut Vec<T>,
    key: impl Fn(&T) -> Eterm,
    take_value: impl Fn(&mut T, &T),
    atoms: &AtomTable,
) {
    // `run` indexes the current run's surviving element; `i` walks the buffer. Because
    // `run <= i` always, the in-place move only ever overwrites elements already
    // folded into a survivor.
    let mut run = 0;
    for i in 1..items.len() {
        if eq(key(&items[run]), key(&items[i]), atoms) {
            let later = items[i];
            take_value(&mut items[run], &later);
        } else {
            run += 1;
            items[run] = items[i];
        }
    }
    if !items.is_empty() {
        items.truncate(run + 1);
    }
}

/// Builds a map term from `pairs` at `hp` and returns the tagged root.
///
/// Reproduces `erts_map_from_ks_and_vs` (`erl_map.c:673`) / `erts_maps_put`
/// (`erl_map.c:2165`): duplicate keys are collapsed **last value wins**, a map of
/// at most [`MAP_SMALL_MAP_LIMIT`] distinct keys is a flat map (keys sorted in
/// [`cmp_flatmap_keys`] order), a larger one is a hash map keyed by
/// [`erts_map_hash`], so its stored pair order equals the oracle's
/// `maps:to_list/1` order. The pair order of `pairs` is irrelevant.
///
/// Construction is the C's *sort once, duplicates become adjacent* shape (the module's
/// *Deduplication* section has the byte-equivalence argument): one stable sort of the
/// input pairs followed by `collapse_eq_runs`'s single linear pass, never a
/// per-insertion scan.
/// The layout is decided by the **input** arity first, because deduplication can
/// only shrink the key count: an input of at most [`MAP_SMALL_MAP_LIMIT`] pairs is
/// necessarily a flat map, so it is copied into one buffer, sorted by
/// [`cmp_flatmap_keys`] (no hashing), collapsed, and written with [`write_flatmap`] —
/// the layout is known from the input arity, so no hashing is needed to pick it.
/// Only a larger input needs the hash map's [`erts_map_hash`] placement, one hash per
/// input key.
///
/// # Safety
///
/// `hp` must point at [`map_words_capacity`]`(pairs.len())` exclusively-owned,
/// writable words. Every word of the produced map is written by this call; the
/// returned term borrows `hp`'s storage, so it must stay live and unmoved while
/// the term is used.
pub unsafe fn write_map(
    hp: *mut Eterm,
    pairs: &[(Eterm, Eterm)],
    atoms: &AtomTable,
) -> TermResult<Eterm> {
    if pairs.len() <= MAP_SMALL_MAP_LIMIT {
        // One buffer: the input copy is sorted in `cmp_flatmap_keys` order — which
        // orders `eq`-equal keys `Equal`, so a stable sort leaves each such run
        // adjacent and in input order — then collapsed in place. `eq` alone decides
        // identity here: the flat branch never hashes.
        let mut flat: Vec<(Eterm, Eterm)> = pairs.to_vec();
        flat.sort_by(|a, b| cmp_flatmap_keys(a.0, b.0, atoms));
        collapse_eq_runs(&mut flat, |p| p.0, |dst, src| dst.1 = src.1, atoms);
        // SAFETY: the caller reserved `flatmap_size_words(pairs.len())` words, which is
        // at least `flatmap_size_words(flat.len())` for the collapsed length, and
        // `flat.len() <= MAP_SMALL_MAP_LIMIT` — the preconditions `write_flatmap`
        // documents. `pairs.len() == 0` yields the 3-word empty block, as before.
        return Ok(unsafe { write_flatmap(hp, &flat) });
    }

    // More than `MAP_SMALL_MAP_LIMIT` input pairs: hash each key once, sort into the
    // canonical leaf order, then collapse adjacent equal keys in one pass and choose
    // the layout from the *distinct* count.
    let mut entries: Vec<(u64, Eterm, Eterm)> = Vec::with_capacity(pairs.len());
    for &(key, value) in pairs {
        entries.push((erts_map_hash(key), key, value));
    }

    // Hash map. Lay the key/value cons cells at the base of the block in
    // *canonical* order — ascending by `swizzle_map_hash(hash)` (the C's
    // `hxnodecmp` order, a nibble reversal of the hash so the trie's per-level
    // nibbles are eaten least-significant first), equal-hash runs broken by
    // `CMP_TERM` (the C's `hxnodecmpkey` collision-node order) — so the produced
    // buffer is independent of the caller's pair order (matching the C, which lays
    // out the swizzle-sorted `hxns`). The leaves are then already sorted for
    // `build_trie`. `eq`-equal keys share an `erts_map_hash` (the invariant the trie
    // itself relies on), so this order leaves each duplicate run adjacent, in input
    // order, for the collapse.
    entries.sort_by(|a, b| {
        swizzle_map_hash(a.0)
            .cmp(&swizzle_map_hash(b.0))
            .then_with(|| cmp(a.1, b.1, atoms))
    });
    collapse_eq_runs(&mut entries, |e| e.1, |dst, src| dst.2 = src.2, atoms);
    let n = entries.len();
    if n <= MAP_SMALL_MAP_LIMIT {
        // Collapsing more than `MAP_SMALL_MAP_LIMIT` pairs can land at or below the
        // limit, which makes it a flat map after all.
        entries.sort_by(|a, b| cmp_flatmap_keys(a.1, b.1, atoms));
        let flat: Vec<(Eterm, Eterm)> = entries.iter().map(|(_, k, v)| (*k, *v)).collect();
        // SAFETY: the caller reserved `flatmap_size_words(pairs.len())` words, which
        // is at least `flatmap_size_words(n)` for the collapsed `n`.
        return Ok(unsafe { write_flatmap(hp, &flat) });
    }
    let mut cur = hp;
    let mut leaves: Vec<(u64, Eterm)> = Vec::with_capacity(n);
    for &(hash, key, value) in &entries {
        let cell = cur;
        // SAFETY: words `0..2n` of the reserved block hold the `n` cons cells; both
        // words of each cell are written before the term escapes.
        unsafe {
            *cell.add(CAR_OFFSET) = key;
            *cell.add(CDR_OFFSET) = value;
            cur = cur.add(2);
        }
        leaves.push((hash, make_list(cell as *const Eterm)));
    }

    let trie = build_trie(&leaves, 0, atoms);
    debug_assert!(
        2 * n + trie_words(&trie, 0) <= map_words_capacity(n),
        "map_words_capacity({n}) is not an upper bound",
    );
    Ok(serialize_node(&trie, 0, n, &mut cur))
}

// ---------------------------------------------------------------------------
// Readers
// ---------------------------------------------------------------------------

/// The `(slots word offset, slot count)` of a hash-map node header.
fn node_slots(header: Eterm) -> TermResult<(usize, usize)> {
    match map_header_type(header) as usize {
        MAP_HEADER_TAG_HAMT_HEAD_ARRAY => Ok((2, HAMT_MAX_LEVEL)),
        MAP_HEADER_TAG_HAMT_HEAD_BITMAP => Ok((2, map_header_val(header).count_ones() as usize)),
        MAP_HEADER_TAG_HAMT_NODE_BITMAP => Ok((1, map_header_val(header).count_ones() as usize)),
        _ => Err(TermError::BadMap),
    }
}

/// Forces a map node slot: pushes every key/value of every leaf it contains.
fn collect_slots(node: *mut Eterm, out: &mut Vec<(Eterm, Eterm)>) -> TermResult<()> {
    // SAFETY: `node` points at a live hash-map node header.
    let header = unsafe { *node };
    let (off, sz) = node_slots(header)?;
    for i in 0..sz {
        // SAFETY: slot `i` is within the node's payload.
        let item = unsafe { *node.add(off + i) };
        if is_list(item) {
            // SAFETY: `item` is a `[Key, Value]` cons cell.
            out.push(unsafe { (car(item), cdr(item)) });
        } else {
            // SAFETY: a non-leaf slot holds a live boxed node or collision tuple.
            let ihdr = unsafe { *boxed_val(item) };
            if is_map_header(ihdr) {
                collect_slots(boxed_val(item), out)?;
            } else if is_arity_value(ihdr) {
                // SAFETY: `item` is a collision-node tuple of cons cells.
                let n = unsafe { tuple_size(item) };
                for j in 0..n {
                    // SAFETY: element `j` is within the tuple's payload.
                    let cell = unsafe { tuple_element(item, j) };
                    // SAFETY: a collision cell is a `[Key, Value]` cons cell.
                    out.push(unsafe { (car(cell), cdr(cell)) });
                }
            } else {
                return Err(TermError::BadMap);
            }
        }
    }
    Ok(())
}

/// Every `(key, value)` pair of the map `t` (unordered).
fn collect_pairs(t: Eterm) -> TermResult<Vec<(Eterm, Eterm)>> {
    // An immediate can never be a map; guard before dereferencing.
    if !is_boxed_term(t) {
        return Err(TermError::BadMap);
    }
    let p = boxed_val(t);
    // SAFETY: `t` is a live boxed term.
    let header = unsafe { *p };
    if is_flatmap_header(header) {
        // SAFETY: `t` is a live flat map.
        let (keys, values) = unsafe { flatmap_parts(t) };
        return Ok(keys.iter().copied().zip(values.iter().copied()).collect());
    }
    if !is_map_header(header) {
        return Err(TermError::BadMap);
    }
    let mut pairs: Vec<(Eterm, Eterm)> = Vec::new();
    collect_slots(p, &mut pairs)?;
    Ok(pairs)
}

/// Every `(key, value)` pair of the map `t`, sorted in `crate::compare::cmp`
/// (Erlang map-key / term) order.
///
/// The storage order of the underlying flat map or HAMT is hash- and
/// representation-dependent, so the pairs are re-sorted to give callers a single
/// canonical order.
pub fn map_pairs(t: Eterm, atoms: &AtomTable) -> TermResult<Vec<(Eterm, Eterm)>> {
    let mut pairs = collect_pairs(t)?;
    pairs.sort_by(|a, b| cmp(a.0, b.0, atoms));
    Ok(pairs)
}

/// Every `(key, value)` pair of the map `t` in the order the map **stores** them.
///
/// A flat map yields its keys-tuple order; a hash map its HAMT slot traversal
/// order. That order is representation-dependent (and, for a hash map,
/// hash-dependent), so it is **not** the [`crate::compare::cmp`] key order that
/// [`map_pairs`] produces. It is the order an image-preserving serializer must
/// use to reproduce an existing map byte-for-byte.
pub fn map_pairs_storage_order(t: Eterm) -> TermResult<Vec<(Eterm, Eterm)>> {
    collect_pairs(t)
}

/// Like [`map_pairs`] but in the *internal* flat-map key order
/// ([`crate::compare::cmp_flatmap_keys`]): two atom keys order by atom index,
/// every other pair by term order.
///
/// This is the order in which a flat map stores its keys and the order
/// [`crate::compare`] and [`crate::hashing`] use, so a hash map and a flat map
/// holding the same key set present their pairs identically.
pub(crate) fn map_pairs_flatmap_order(
    t: Eterm,
    atoms: &AtomTable,
) -> TermResult<Vec<(Eterm, Eterm)>> {
    let mut pairs = collect_pairs(t)?;
    pairs.sort_by(|a, b| cmp_flatmap_keys(a.0, b.0, atoms));
    Ok(pairs)
}

/// `erts_map_size` — the number of keys in the map `t`.
pub fn map_size(t: Eterm, _atoms: &AtomTable) -> TermResult<usize> {
    // An immediate can never be a map; guard before dereferencing.
    if !is_boxed_term(t) {
        return Err(TermError::BadMap);
    }
    let p = boxed_val(t);
    // SAFETY: `t` is a live boxed term.
    let header = unsafe { *p };
    if is_flatmap_header(header) {
        // The flat-map head is `[thing_word, size, keys, values...]`.
        // SAFETY: word 1 of a live flat map is its size.
        return Ok(unsafe { *p.add(1) }.raw());
    }
    if !is_map_header(header) {
        return Err(TermError::BadMap);
    }
    // The hash-map head is `[thing_word, size, slots...]`; `size` is the key count.
    // SAFETY: word 1 of a live hash-map head node is its size.
    Ok(unsafe { *p.add(1) }.raw())
}

/// `erts_maps_get` — the value bound to `key` in the map `t`, if any.
///
/// Flat maps are scanned linearly (with `=:=` key comparison); hash maps follow
/// `hashmap_nibble(erts_map_hash(key), L)` from the root, falling back to a linear
/// scan of the collision node when the path ends in one.
pub fn map_get(t: Eterm, key: Eterm, atoms: &AtomTable) -> TermResult<Option<Eterm>> {
    // An immediate can never be a map; guard before dereferencing.
    if !is_boxed_term(t) {
        return Err(TermError::BadMap);
    }
    let p = boxed_val(t);
    // SAFETY: `t` is a live boxed term.
    let header = unsafe { *p };
    if is_flatmap_header(header) {
        // SAFETY: `t` is a live flat map.
        let (keys, values) = unsafe { flatmap_parts(t) };
        for (k, v) in keys.iter().zip(values.iter()) {
            if eq(*k, key, atoms) {
                return Ok(Some(*v));
            }
        }
        return Ok(None);
    }
    if !is_map_header(header) {
        return Err(TermError::BadMap);
    }

    let hash = erts_map_hash(key);
    let mut node = p;
    let mut level = 0usize;
    loop {
        // SAFETY: `node` points at a live hash-map node header.
        let nheader = unsafe { *node };
        let (off, bitmap) = match map_header_type(nheader) as usize {
            MAP_HEADER_TAG_HAMT_HEAD_ARRAY => (2usize, HAMT_FULL_BITMAP),
            MAP_HEADER_TAG_HAMT_HEAD_BITMAP => (2, map_header_val(nheader) as u16),
            MAP_HEADER_TAG_HAMT_NODE_BITMAP => (1, map_header_val(nheader) as u16),
            _ => return Err(TermError::BadMap),
        };
        let nib = hashmap_nibble(hash, level);
        if bitmap & (1u16 << nib) == 0 {
            return Ok(None);
        }
        let index = (bitmap & ((1u16 << nib) - 1)).count_ones() as usize;
        // SAFETY: `index < popcount(bitmap) = the node's slot count`.
        let item = unsafe { *node.add(off + index) };
        if is_list(item) {
            // SAFETY: `item` is a `[Key, Value]` cons cell.
            let (k, v) = unsafe { (car(item), cdr(item)) };
            // A leaf reached through a matching nibble may still hold a different
            // key (64-bit hash collision), so the key is confirmed by comparison.
            return Ok(if eq(k, key, atoms) { Some(v) } else { None });
        }
        // SAFETY: a non-leaf slot holds a live boxed node or collision tuple.
        let ihdr = unsafe { *boxed_val(item) };
        if is_map_header(ihdr) {
            level += 1;
            if level > HAMT_LAST_NODE_LEVEL {
                return Err(TermError::BadMap);
            }
            node = boxed_val(item);
        } else if is_arity_value(ihdr) {
            // SAFETY: `item` is a collision-node tuple of cons cells.
            let n = unsafe { tuple_size(item) };
            for j in 0..n {
                // SAFETY: element `j` is within the tuple's payload.
                let cell = unsafe { tuple_element(item, j) };
                // SAFETY: a collision cell is a `[Key, Value]` cons cell.
                let (k, v) = unsafe { (car(cell), cdr(cell)) };
                if eq(k, key, atoms) {
                    return Ok(Some(v));
                }
            }
            return Ok(None);
        } else {
            return Err(TermError::BadMap);
        }
    }
}

// ---------------------------------------------------------------------------
// Functional update
// ---------------------------------------------------------------------------

/// `maps:put/3` — a copy of `t` with `key` bound to `value`.
///
/// Rebuilds the map from its pairs, so the result is the canonical map for the
/// new pair set; storing an existing key replaces its value in place.
///
/// # Safety
///
/// `hp` must point at [`map_words_capacity`]`(map_size(t) + 1)` exclusively-owned,
/// writable words.
pub unsafe fn map_put(
    hp: *mut Eterm,
    t: Eterm,
    key: Eterm,
    value: Eterm,
    atoms: &AtomTable,
) -> TermResult<Eterm> {
    let mut pairs = map_pairs(t, atoms)?;
    let mut replaced = false;
    for pair in pairs.iter_mut() {
        if !replaced && eq(pair.0, key, atoms) {
            pair.1 = value;
            replaced = true;
        }
    }
    if !replaced {
        pairs.push((key, value));
    }
    // SAFETY: the caller reserved `map_words_capacity(map_size(t) + 1) >=
    // map_words_capacity(pairs.len())` words.
    unsafe { write_map(hp, &pairs, atoms) }
}

/// `maps:remove/2` — a copy of `t` with `key` (if present) removed.
///
/// # Safety
///
/// `hp` must point at [`map_words_capacity`]`(map_size(t))` — or simply at
/// [`map_words_capacity`]`(map_size(t) + 1)` — exclusively-owned, writable words.
pub unsafe fn map_remove(
    hp: *mut Eterm,
    t: Eterm,
    key: Eterm,
    atoms: &AtomTable,
) -> TermResult<Eterm> {
    let pairs: Vec<(Eterm, Eterm)> = map_pairs(t, atoms)?
        .into_iter()
        .filter(|(k, _)| !eq(*k, key, atoms))
        .collect();
    // SAFETY: the caller reserved at least `map_words_capacity(pairs.len())` words.
    unsafe { write_map(hp, &pairs, atoms) }
}

/// Whether `header` is a map header (used by the test module and downstream).
#[cfg(test)]
fn is_map_term(t: Eterm) -> bool {
    // SAFETY: `is_map` only dereferences the header of a live boxed term.
    unsafe { crate::boxed::is_map(t) }
}

/// Whether the boxed term `t` is a leaf cons cell (`is_list`) — helper for docs.
#[cfg(test)]
fn is_leaf_term(t: Eterm) -> bool {
    is_list(t)
}

/// The `HAMT_SUBTAG_*` constant of a node header (test/telemetry helper).
#[cfg(test)]
fn node_subtag(header: Eterm) -> usize {
    header.raw() & (0x3 << 6 | 0x3F)
}

// ---------------------------------------------------------------------------
// Incremental (path-copying) update — see `src/map/cow.rs`
// ---------------------------------------------------------------------------

mod cow;

pub use cow::{
    map_put_cow, map_put_cow_words_capacity, map_put_cow_words_required, map_remove_cow,
    map_remove_cow_words_capacity, map_remove_cow_words_required,
};

#[cfg(test)]
mod tests;
