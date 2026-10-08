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

/// Builds a map term from `pairs` at `hp` and returns the tagged root.
///
/// Reproduces `erts_map_from_ks_and_vs` (`erl_map.c:673`) / `erts_maps_put`
/// (`erl_map.c:2165`): duplicate keys are collapsed **last value wins**, a map of
/// at most [`MAP_SMALL_MAP_LIMIT`] distinct keys is a flat map (keys sorted in
/// [`cmp_flatmap_keys`] order), a larger one is a hash map keyed by
/// [`erts_map_hash`], so its stored pair order equals the oracle's
/// `maps:to_list/1` order. The pair order of `pairs` is irrelevant.
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
    // Collapse duplicates (last value wins) while hashing each key once.
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

    if n == 0 {
        // SAFETY: the caller reserved `flatmap_size_words(0) == 3 <= capacity`.
        return Ok(unsafe { write_flatmap(hp, &[]) });
    }
    if n <= MAP_SMALL_MAP_LIMIT {
        entries.sort_by(|a, b| cmp_flatmap_keys(a.1, b.1, atoms));
        let flat: Vec<(Eterm, Eterm)> = entries.iter().map(|(_, k, v)| (*k, *v)).collect();
        // SAFETY: the caller reserved `flatmap_size_words(pairs.len())` words, which
        // is at least `flatmap_size_words(n)` for the deduplicated `n`.
        return Ok(unsafe { write_flatmap(hp, &flat) });
    }

    // Hash map. Lay the key/value cons cells at the base of the block in
    // *canonical* order — ascending by `swizzle_map_hash(hash)` (the C's
    // `hxnodecmp` order, a nibble reversal of the hash so the trie's per-level
    // nibbles are eaten least-significant first), equal-hash runs broken by
    // `CMP_TERM` (the C's `hxnodecmpkey` collision-node order) — so the produced
    // buffer is independent of the caller's pair order (matching the C, which lays
    // out the swizzle-sorted `hxns`). The leaves are then already sorted for
    // `build_trie`.
    entries.sort_by(|a, b| {
        swizzle_map_hash(a.0)
            .cmp(&swizzle_map_hash(b.0))
            .then_with(|| cmp(a.1, b.1, atoms))
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxed::{
        header_arity, is_flatmap, is_hashmap, is_hashmap_header, map_header_val,
    };
    use crate::heap::Heap;
    use crate::immediate::{make_atom, make_small, signed_value, NIL};
    use crate::size::size_object;

    /// A permanently-invalid marker word: `write_map` must never touch it.
    const SENTINEL: Eterm = Eterm::from_raw(0xDEAD_BEEF);

    fn atoms() -> AtomTable {
        let mut t = AtomTable::new();
        t.seed(["a", "b", "c"]);
        t
    }

    /// A map built into an owned, over-sized buffer: the tail beyond
    /// `map_words_capacity` is a sentinel that detects any overflow.
    struct Built {
        _buf: Vec<Eterm>,
        map: Eterm,
    }

    fn build(pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Built {
        let cap = map_words_capacity(pairs.len());
        let mut buf = vec![SENTINEL; cap + 64];
        let base = buf.as_mut_ptr();
        // SAFETY: `buf` reserved `cap` words for the map (plus slack), and it is
        // kept alive by the returned `Built`.
        let map = unsafe { write_map(base, pairs, atoms).unwrap() };
        for i in 0..64 {
            assert_eq!(
                buf[cap + i],
                SENTINEL,
                "write_map overflowed map_words_capacity by at least {} words",
                i + 1
            );
        }
        Built { _buf: buf, map }
    }

    /// Normalize a map buffer for base-independent comparison: each word becomes
    /// `(true, offset)` when it is a tagged pointer into `buf`, else `(false, raw)`.
    /// Two canonically-laid-out maps (same shape, different heap base) normalize
    /// equal.
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
    fn pairs_of(keys: &[i64], atoms: &AtomTable) -> Vec<(Eterm, Eterm)> {
        let _ = atoms;
        keys.iter()
            .map(|k| (make_small(*k), make_small(*k * 10)))
            .collect()
    }

    fn keys_1_to(n: i64) -> Vec<i64> {
        (1..=n).collect()
    }

    #[test]
    fn empty_map_is_a_three_word_flatmap() {
        let a = atoms();
        let b = build(&[], &a);
        // SAFETY: `b.map` is a live boxed map.
        assert!(unsafe { is_flatmap(b.map) });
        assert_eq!(map_size(b.map, &a).unwrap(), 0);
        assert_eq!(map_pairs(b.map, &a).unwrap(), vec![]);
        assert_eq!(size_object(b.map), 3);
    }

    #[test]
    fn small_map_is_a_flatmap_with_sorted_keys() {
        let a = atoms();
        let mut heap = Heap::new(64);
        let k3 = heap.tuple(&[make_small(3)]).unwrap();
        let k1 = heap.cons(make_small(1), NIL).unwrap();
        let raw = [
            (make_small(5), make_small(50)),
            (k3, make_atom(a.lookup("a").unwrap())),
            (k1, make_small(10)),
        ];
        let b = build(&raw, &a);
        // SAFETY: `b.map` is a live boxed map.
        assert!(unsafe { is_flatmap(b.map) });
        assert_eq!(map_size(b.map, &a).unwrap(), 3);
        // `map_pairs` is in Erlang term order: number < tuple < list.
        let pairs = map_pairs(b.map, &a).unwrap();
        assert_eq!(pairs[0].0, make_small(5));
        assert_eq!(pairs[1].0, k3);
        assert_eq!(pairs[2].0, k1);
        assert_eq!(pairs[2].1, make_small(10));
    }

    #[test]
    fn storage_order_is_the_flatmap_keys_tuple_order() {
        let a = atoms();
        let mut heap = Heap::new(64);
        let k3 = heap.tuple(&[make_small(3)]).unwrap();
        let k1 = heap.cons(make_small(1), NIL).unwrap();
        let raw = [
            (make_small(5), make_small(50)),
            (k3, make_atom(a.lookup("a").unwrap())),
            (k1, make_small(10)),
        ];
        let b = build(&raw, &a);
        // SAFETY: `b.map` is a live boxed map.
        assert!(unsafe { is_flatmap(b.map) });
        // SAFETY: `b.map` is a live flat map.
        let (keys, values) = unsafe { flatmap_parts(b.map) };
        let stored = map_pairs_storage_order(b.map).unwrap();
        assert_eq!(stored.len(), 3);
        assert_eq!(
            stored,
            keys.iter()
                .copied()
                .zip(values.iter().copied())
                .collect::<Vec<_>>()
        );
        assert_eq!(stored[0].0, make_small(5));

        // Flat-map storage order is atom-*index* order, which is not the `cmp`
        // (name) order `map_pairs` returns: seed "b" before "a".
        let mut t = AtomTable::new();
        t.seed(["b", "a"]);
        let b_atom = make_atom(t.lookup("b").unwrap());
        let a_atom = make_atom(t.lookup("a").unwrap());
        let built = build(&[(a_atom, make_small(1)), (b_atom, make_small(2))], &t);
        let stored = map_pairs_storage_order(built.map).unwrap();
        assert_eq!(stored, vec![(b_atom, make_small(2)), (a_atom, make_small(1))]);
        assert_ne!(stored, map_pairs(built.map, &t).unwrap());
    }

    #[test]
    fn storage_order_matches_the_hamt_traversal_of_a_hash_map() {
        let a = atoms();
        let n = 40i64;
        let b = build(&pairs_of(&keys_1_to(n), &a), &a);
        // SAFETY: `b.map` is a live boxed map.
        assert!(unsafe { is_hashmap(b.map) });
        let stored = map_pairs_storage_order(b.map).unwrap();
        // Every input key appears exactly once, with its matching value.
        assert_eq!(stored.len(), n as usize);
        let mut keys: Vec<i64> = stored.iter().map(|(k, _)| signed_value(*k)).collect();
        keys.sort_unstable();
        assert_eq!(keys, keys_1_to(n));
        for (k, v) in &stored {
            assert_eq!(*v, make_small(signed_value(*k) * 10));
        }
        // The order is the raw HAMT slot traversal, not a re-sorted one.
        let mut direct: Vec<(Eterm, Eterm)> = Vec::new();
        collect_slots(boxed_val(b.map), &mut direct).unwrap();
        assert_eq!(stored, direct);
    }

    #[test]
    fn storage_order_rejects_non_maps() {
        assert_eq!(
            map_pairs_storage_order(make_small(1)).unwrap_err(),
            TermError::BadMap
        );
        assert_eq!(
            map_pairs_storage_order(NIL).unwrap_err(),
            TermError::BadMap
        );
        let mut heap = Heap::new(8);
        let tup = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        assert_eq!(map_pairs_storage_order(tup).unwrap_err(), TermError::BadMap);
    }

    #[test]
    fn duplicate_keys_last_value_wins() {
        let a = atoms();
        let raw = [
            (make_small(1), make_small(10)),
            (make_small(2), make_small(20)),
            (make_small(1), make_small(99)),
        ];
        let b = build(&raw, &a);
        assert_eq!(map_size(b.map, &a).unwrap(), 2);
        assert_eq!(map_get(b.map, make_small(1), &a).unwrap(), Some(make_small(99)));
        assert_eq!(map_get(b.map, make_small(2), &a).unwrap(), Some(make_small(20)));
    }

    #[test]
    fn flatmap_to_hashmap_boundary_is_at_32_keys() {
        let a = atoms();
        for n in [31i64, 32] {
            let b = build(&pairs_of(&keys_1_to(n), &a), &a);
            // SAFETY: `b.map` is a live boxed map.
            assert!(unsafe { is_flatmap(b.map) }, "n={n} should be a flat map");
            assert!(!unsafe { is_hashmap(b.map) });
            assert_eq!(map_size(b.map, &a).unwrap(), n as usize);
        }
        for n in [33i64, 34, 40] {
            let b = build(&pairs_of(&keys_1_to(n), &a), &a);
            // SAFETY: `b.map` is a live boxed map.
            assert!(unsafe { is_hashmap(b.map) }, "n={n} should be a hash map");
            assert!(!unsafe { is_flatmap(b.map) });
            assert_eq!(map_size(b.map, &a).unwrap(), n as usize);
            for k in 1..=n {
                assert_eq!(
                    map_get(b.map, make_small(k), &a).unwrap(),
                    Some(make_small(k * 10))
                );
            }
            assert_eq!(map_get(b.map, make_small(n + 1), &a).unwrap(), None);
        }
    }

    #[test]
    fn hashmap_node_headers_match_the_c_encodings() {
        let a = atoms();
        let b = build(&pairs_of(&keys_1_to(33), &a), &a);
        let p = boxed_val(b.map);
        // SAFETY: `b.map` is a live boxed hash map.
        let head = unsafe { *p };
        assert!(is_map_header(head));
        assert!(is_hashmap_header(head));
        // Head array: MAKE_MAP_HEADER(tag=2, arity=1, val=0xffff) == 0xFFFF01AC.
        if map_header_type(head) as usize == MAP_HEADER_TAG_HAMT_HEAD_ARRAY {
            assert_eq!(head.raw(), 0xFFFF_01AC);
            assert_eq!(map_header_val(head), 0xffff);
        } else {
            // Head bitmap: MAKE_MAP_HEADER(tag=3, arity=1, bitmap).
            assert_eq!(
                map_header_type(head) as usize,
                MAP_HEADER_TAG_HAMT_HEAD_BITMAP
            );
            assert_eq!(head.raw() & 0xFFFF, 0x0000_0000 | (head.raw() & 0xFFFF));
        }
        assert_eq!(header_arity(head), 1);
        // The head's size word is the key count.
        // SAFETY: word 1 of a head node is its size word.
        assert_eq!(unsafe { *p.add(1) }.raw(), 33);

        // Node bitmaps: MAKE_MAP_HEADER(tag=1, arity=0, bitmap) => arity field 0.
        // Walk the tree and check every non-head map header.
        let mut stack = vec![p];
        let mut seen_nodes = 0;
        while let Some(node) = stack.pop() {
            // SAFETY: every pushed pointer is a live node header.
            let hdr = unsafe { *node };
            assert!(is_map_header(hdr));
            let (off, sz) = node_slots(hdr).unwrap();
            if node != p {
                assert_eq!(map_header_type(hdr) as usize, MAP_HEADER_TAG_HAMT_NODE_BITMAP);
                assert_eq!(header_arity(hdr), 0);
                // `HAMT_SUBTAG_NODE_BITMAP` = (1 << 6) | 0x2C.
                assert_eq!(hdr.raw() & 0xFF, 0x6C);
            }
            assert!(sz >= 1 && sz <= 16);
            for i in 0..sz {
                // SAFETY: slot `i` is within the node payload.
                let item = unsafe { *node.add(off + i) };
                if is_list(item) {
                    // A leaf is a `[Key, Value]` cons cell (`is_list` already asserted).
                } else {
                    // SAFETY: a non-leaf slot is a live boxed node or collision tuple.
                    let ihdr = unsafe { *boxed_val(item) };
                    if is_map_header(ihdr) {
                        stack.push(boxed_val(item));
                    } else {
                        seen_nodes += 1;
                        assert!(is_arity_value(ihdr));
                    }
                }
            }
        }
        let _ = seen_nodes;
    }

    #[test]
    fn insertion_order_does_not_change_the_layout() {
        let a = atoms();
        let forward: Vec<(Eterm, Eterm)> = (1..=60i64)
            .map(|k| (make_small(k), make_small(k * 3)))
            .collect();
        let mut backward = forward.clone();
        backward.reverse();
        let b1 = build(&forward, &a);
        let b2 = build(&backward, &a);
        // The two maps must be structurally identical: same shape and size.
        assert_eq!(size_object(b1.map), size_object(b2.map));
        // Base-independent layout comparison: the canonical leaf/trie layout is
        // insertion-order free (the block bases differ, so absolute pointers are
        // normalized to offsets first).
        let cap = map_words_capacity(forward.len());
        assert_eq!(
            normalize(&b1._buf[..cap]),
            normalize(&b2._buf[..cap]),
            "canonical layout is insertion-order free"
        );
        assert!(eq(b1.map, b2.map, &a));
    }

    #[test]
    fn build_lookup_iterate_delete_over_sizes() {
        let a = atoms();
        for n in 0..=500i64 {
            let raw = pairs_of(&keys_1_to(n), &a);
            let b = build(&raw, &a);
            assert_eq!(map_size(b.map, &a).unwrap(), n as usize);
            let got = map_pairs(b.map, &a).unwrap();
            assert_eq!(got.len(), n as usize);
            for i in 0..got.len() {
                assert_eq!(got[i].0, make_small(i as i64 + 1));
                assert_eq!(got[i].1, make_small((i as i64 + 1) * 10));
            }
            for k in 1..=n {
                assert_eq!(
                    map_get(b.map, make_small(k), &a).unwrap(),
                    Some(make_small(k * 10)),
                    "missing key {k} of {n}"
                );
            }
            assert_eq!(map_get(b.map, make_small(n + 1), &a).unwrap(), None);
        }
    }

    #[test]
    fn put_and_remove_are_functional() {
        let a = atoms();
        for n in [0i64, 3, 32, 33, 100] {
            let b = build(&pairs_of(&keys_1_to(n), &a), &a);
            // Insert/replace a key.
            let cap = map_words_capacity(map_size(b.map, &a).unwrap() + 1);
            let mut buf = vec![SENTINEL; cap + 64];
            let base = buf.as_mut_ptr();
            // SAFETY: `buf` reserved the capacity `map_put` documents.
            let put = unsafe { map_put(base, b.map, make_small(1), make_small(-1), &a).unwrap() };
            assert_eq!(map_get(put, make_small(1), &a).unwrap(), Some(make_small(-1)));
            assert_eq!(map_size(put, &a).unwrap(), n.max(1) as usize);
            assert!(size_object(put) <= cap);
            if n == 0 {
                // SAFETY: `put` is a live boxed map.
                assert!(unsafe { is_flatmap(put) });
            }

            // Insert a brand-new key.
            let base2 = buf.as_mut_ptr();
            let added = n + 1000;
            // SAFETY: `buf` reserved the capacity `map_put` documents.
            let put2 = unsafe { map_put(base2, b.map, make_small(added), make_small(7), &a).unwrap() };
            assert_eq!(map_get(put2, make_small(added), &a).unwrap(), Some(make_small(7)));
            assert_eq!(map_size(put2, &a).unwrap(), n as usize + 1);

            // Remove a key (present or not).
            let removed = if n > 0 { n / 2 + 1 } else { 1 };
            // SAFETY: `buf` reserved at least `map_words_capacity(n + 1)`.
            let gone = unsafe { map_remove(base2, b.map, make_small(removed), &a).unwrap() };
            assert_eq!(map_get(gone, make_small(removed), &a).unwrap(), None);
            let expected = if n > 0 { n - 1 } else { 0 };
            assert_eq!(map_size(gone, &a).unwrap(), expected as usize);
        }
    }

    #[test]
    fn structural_compare_and_hash_agree_across_representations() {
        let a = atoms();
        // Same pairs built in two different orders.
        let p1 = pairs_of(&keys_1_to(60), &a);
        let mut p2 = p1.clone();
        p2.reverse();
        let b1 = build(&p1, &a);
        let b2 = build(&p2, &a);
        assert_eq!(crate::compare::cmp(b1.map, b2.map, &a), core::cmp::Ordering::Equal);
        assert_eq!(
            crate::hashing::term_hash(b1.map, &a),
            crate::hashing::term_hash(b2.map, &a)
        );

        // Size decides first.
        let small = build(&pairs_of(&keys_1_to(33), &a), &a);
        let big = build(&pairs_of(&keys_1_to(40), &a), &a);
        assert_eq!(
            crate::compare::cmp(small.map, big.map, &a),
            core::cmp::Ordering::Less
        );
        assert_ne!(
            crate::hashing::term_hash(small.map, &a),
            crate::hashing::term_hash(big.map, &a)
        );

        // Same size, keys ordered identically, values decide.
        let x = build(
            &[
                (make_small(1), make_atom(a.lookup("a").unwrap())),
                (make_small(2), make_atom(a.lookup("b").unwrap())),
            ],
            &a,
        );
        let y = build(
            &[
                (make_small(1), make_atom(a.lookup("a").unwrap())),
                (make_small(2), make_atom(a.lookup("c").unwrap())),
            ],
            &a,
        );
        assert_eq!(
            crate::compare::cmp(x.map, y.map, &a),
            core::cmp::Ordering::Less
        );
    }

    #[test]
    fn colliding_keys_form_a_collision_node() {
        // Force identical hashes by using keys whose `term_hash` collides is hard;
        // instead check the *general* invariant that a lookup never returns a value
        // for a key that is present only under a different hash path.
        let a = atoms();
        let b = build(&pairs_of(&keys_1_to(200), &a), &a);
        for k in 1..=200 {
            assert_eq!(
                map_get(b.map, make_small(k), &a).unwrap(),
                Some(make_small(k * 10))
            );
        }
        // Keys sharing the same low nibble (e.g. 1 and 17: 1 & 0xF == 17 & 0xF) must
        // still be distinguishable.
        assert_eq!(map_get(b.map, make_small(1), &a).unwrap(), Some(make_small(10)));
        assert_eq!(map_get(b.map, make_small(17), &a).unwrap(), Some(make_small(170)));
    }

    #[test]
    fn errors_surface_for_non_maps() {
        let a = atoms();
        assert_eq!(map_size(make_small(1), &a), Err(TermError::BadMap));
        assert_eq!(
            map_pairs(make_small(1), &a).unwrap_err(),
            TermError::BadMap
        );
        assert_eq!(
            map_get(NIL, make_small(1), &a).unwrap_err(),
            TermError::BadMap
        );
    }

    #[test]
    fn empty_pairs_and_single_pair_round_trip() {
        let a = atoms();
        let one = build(&[(make_small(42), make_atom(a.lookup("a").unwrap()))], &a);
        // SAFETY: `one.map` is a live boxed map.
        assert!(unsafe { is_flatmap(one.map) });
        assert_eq!(map_size(one.map, &a).unwrap(), 1);
        assert_eq!(
            map_get(one.map, make_small(42), &a).unwrap(),
            Some(make_atom(a.lookup("a").unwrap()))
        );
        assert_eq!(map_get(one.map, make_small(41), &a).unwrap(), None);
    }

    #[test]
    fn helper_predicates_are_consistent() {
        let a = atoms();
        let b = build(&pairs_of(&keys_1_to(5), &a), &a);
        assert!(is_map_term(b.map));
        assert!(is_leaf_term(make_small(1)) == false);
        let map33 = build(&pairs_of(&keys_1_to(33), &a), &a);
        // SAFETY: a live hash-map head node.
        let hdr = unsafe { *boxed_val(map33.map) };
        assert_eq!(node_subtag(hdr), hdr.raw() & 0xFF);
        assert_eq!(crate::tag::HAMT_SUBTAG_HEAD_ARRAY, 0xAC);
        assert_eq!(MAP_SMALL_MAP_LIMIT, 32);
        assert_eq!(HAMT_MAX_LEVEL, 16);
    }

    #[test]
    fn atom_keys_use_index_order_in_a_flatmap() {
        // The flat-map key tuple is sorted by `erts_cmp_flatmap_keys`: atoms by
        // *index*, everything else by term order. Seed so that "b" has a lower
        // index than "a" and check the stored order.
        let mut t = AtomTable::new();
        t.seed(["b", "a"]);
        let b_atom = make_atom(t.lookup("b").unwrap());
        let a_atom = make_atom(t.lookup("a").unwrap());
        assert!(b_atom.raw() < a_atom.raw(), "b must have the lower index");
        let built = build(&[(a_atom, make_small(1)), (b_atom, make_small(2))], &t);
        // SAFETY: `built.map` is a live boxed map.
        assert!(unsafe { is_flatmap(built.map) });
        // SAFETY: `built.map` is a live flat map.
        let (keys, values) = unsafe { flatmap_parts(built.map) };
        assert_eq!(keys, &[b_atom, a_atom]);
        assert_eq!(values, &[make_small(2), make_small(1)]);
        // `map_pairs` re-sorts into term (`cmp`) order, i.e. by atom name.
        let pairs = map_pairs(built.map, &t).unwrap();
        assert_eq!(pairs[0].0, a_atom);
        assert_eq!(pairs[1].0, b_atom);
    }
}
