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

//! Incremental (path-copying) map update: one `maps:put/3` or `maps:remove/2`
//! costs Θ(log n) words instead of Θ(n).
//!
//! [`write_map`](super::write_map) rebuilds a whole map from scratch. That is the
//! right shape for a builder (`from_list`/`from_keys`/`merge`/`with`/`without`/the
//! ETF decoder), but it is the wrong shape for a *single-key update* of a large
//! map: it copies every node, every leaf and the whole keys tuple. The C runtime
//! does not: a `M#{K => V}` copies **only the nodes on the key's hash path** and
//! shares everything else by word (`erts_maps_put`/`erts_hashmap_insert*`,
//! `erts/emulator/beam/erl_map.c:2105-2790`; delete at `hashmap_delete`, `:2881`).
//! This module is that algorithm.
//!
//! # What is copied and what is shared
//!
//! * **Flat arm** (`n <= 32`): an existing key keeps the source map's **keys-tuple
//!   word verbatim** — the C's `*hp++ = mp->keys` — and copies only the `n` values
//!   (`3 + n` words); a new key writes a freshly merged, freshly sorted flat map
//!   (`flatmap_size_words(n + 1)`); the 33rd key converts to a hash map.
//! * **HAMT arm**: one node copy per level of the path (a head array `18` words, a
//!   head bitmap `2 + n`, an inner bitmap `1 + n`), every off-path subtree shared by
//!   word, one new two-word `[Key, Value]` cons cell for the leaf, and — when the
//!   new key diverges from the leaf it meets — the C's *divergence chain* of
//!   single-bit inner nodes down to the first differing nibble.
//!
//! Sharing a word is sound precisely because terms are immutable and are never
//! rewritten or relocated in place: the result graph simply references the source
//! map's sub-graphs, so the *only* obligation is lifetime — `hp` must live in a heap
//! that outlives the source map for as long as the result is used (see the
//! `# Safety` sections).
//!
//! # Two word figures: capacity (size-only) vs required (exact)
//!
//! A caller stages the result in a heap it has reserved ahead of time, typically
//! from the map's *size* alone. [`map_put_cow_words_capacity`] /
//! [`map_remove_cow_words_capacity`] answer that size-only bound:
//!
//! | op | `size < 32` | `size == 32` | `size > 32` |
//! |---|---|---|---|
//! | put | `flatmap_size_words(size + 1) <= 70` | `map_words_capacity(33) = 711` | `293` |
//! | remove | `flatmap_size_words(size - 1) <= 68` | `flatmap_size_words(31) = 66` | `288` |
//!
//! Derivation of the `size > 32` figures. At most `16` nodes lie on a hash path
//! (levels `0..=15`) and the widest is `18` words, so the node spine is at most
//! `288`; a put adds the leaf's `2`-word cons cell. A *divergence chain* can reach
//! one node deeper: it hangs off the level-`15` node and ends in a two-cell
//! collision node (`3` words) when the two keys agree on all 16 nibbles, i.e.
//! `16 · 18 + 3 + 2 = 293`. A chain that starts at level `lvl <= 15` costs
//! `lvl · 18 + (15 - lvl) · 2 + 3 + 2 = 16 · lvl + 35 <= 275` — always below the
//! `290` spine-plus-cell figure, which is why only the collision case sets the
//! bound. A remove never writes a cons cell and its flat-conversion arm writes
//! `flatmap_size_words(32) = 68`, so `288` covers it.
//!
//! An over-estimate costs address space only; an under-estimate corrupts. The
//! size-only bound is loose in exactly one place a *number* cannot express: a
//! **collision node**'s cell count is bounded by the map's size, not by the path
//! depth, so the exact figure must be read from the map itself —
//! [`map_put_cow_words_required`] / [`map_remove_cow_words_required`] descend the
//! path read-only (hashing and nibbles only, no `atoms`) and answer the exact word
//! count, including `c + 1` words for an insert into a `c`-cell collision node and
//! `c - 1` for a delete from one.
//!
//! Every writer `debug_assert!`s that the words it wrote equal its plan's figure
//! (the exactness half the tests lean on) — on every arm except the flat→HAMT
//! conversion, whose plan is `write_map`'s capacity bound `map_words_capacity(n +
//! 1)` rather than the words written. That arm is therefore the one place where a
//! `*_words_required` figure is a bound and not an exact figure; the remove side is
//! exact on every arm, its flat conversion writing exactly
//! `flatmap_size_words(32)`. The plan's figure is *not* asserted
//! against the size-only capacity bound: a collision node on the key's path may
//! legitimately exceed it, which is the one case that must read
//! [`map_put_cow_words_required`] / [`map_remove_cow_words_required`] instead.
//!
//! # Semantics worth pinning
//!
//! * A put whose stored value word is **already the new value word** returns the
//!   input map and writes *nothing* (`erl_map.c:2523-2525` for the HAMT leaf).
//! * A remove of an absent key answers `None` and writes nothing (the caller then
//!   returns the input map, as `maps:remove/2` does).
//! * A hash map that shrinks to `<= 32` keys is rebuilt as a flat map
//!   (`erl_map.c:2946-2981`).
//! * A head bitmap whose `hval | bp == 0xffff` is rewritten with the
//!   `MAP_HEADER_HAMT_HEAD_ARRAY` header — the same `18`-word layout
//!   (`erl_map.c:2748-2750`).
//! * Inserting a key increments the head's `size` word by one; replacing a value
//!   leaves it alone.

use core::cmp::Ordering;

use crate::atom::AtomTable;
use crate::boxed::{
    boxed_val, is_arity_value, is_boxed_term, is_flatmap_header, is_map_header, make_arityval,
    make_boxed, map_header_type, map_header_val,
};
use crate::compare::{cmp, cmp_flatmap_keys, eq};
use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::ihash::{erts_map_hash, hashmap_nibble};
use crate::list::{car, cdr, is_list, make_list};
use crate::tag::{
    MAP_HEADER_TAG_FLATMAP_HEAD, MAP_HEADER_TAG_HAMT_HEAD_ARRAY,
    MAP_HEADER_TAG_HAMT_HEAD_BITMAP, MAP_HEADER_TAG_HAMT_NODE_BITMAP,
};
use crate::thing::{flatmap_parts, flatmap_size_words, write_flatmap};
use crate::tuple::{tuple_element, tuple_size};

use super::{
    collect_pairs, make_map_header, map_size, map_words_capacity, write_map, HAMT_FULL_BITMAP,
    HAMT_MAX_LEVEL, MAP_SMALL_MAP_LIMIT,
};

// ---------------------------------------------------------------------------
// Word bounds
// ---------------------------------------------------------------------------

/// The largest number of words [`map_put_cow`] can write into a map that is
/// already a hash map (`size > MAP_SMALL_MAP_LIMIT`): a `16`-node spine of
/// `18`-word nodes (`288`), a two-cell collision node (`3`) and the new leaf's
/// cons cell (`2`). See the module doc for the derivation.
const HAMT_PUT_WORDS_MAX: usize = 293;

/// The largest number of words [`map_remove_cow`] can write into a map that is
/// already a hash map (`size > MAP_SMALL_MAP_LIMIT`): a `16`-node spine of
/// `18`-word nodes (`288`). A remove writes no cons cell, and its
/// "shrank to `<= 32` keys" flat-conversion arm writes
/// `flatmap_size_words(32) = 68`, which is below the spine bound.
const HAMT_REMOVE_WORDS_MAX: usize = 288;

/// The words a caller must reserve for [`map_put_cow`] on a map of `size` pairs,
/// knowing only the size.
///
/// `size` is [`map_size`] **before** the operation. The figure is sound (never an
/// under-estimate) and only a bound: for an exact figure — which differs only when
/// the key's path crosses a hash **collision node**, whose cell count the map's
/// size cannot bound — call [`map_put_cow_words_required`].
/// An over-estimate costs address space; an under-estimate corrupts.
pub fn map_put_cow_words_capacity(size: usize) -> usize {
    if size < MAP_SMALL_MAP_LIMIT {
        flatmap_size_words(size + 1)
    } else if size == MAP_SMALL_MAP_LIMIT {
        map_words_capacity(MAP_SMALL_MAP_LIMIT + 1)
    } else {
        HAMT_PUT_WORDS_MAX
    }
}

/// The words a caller must reserve for [`map_remove_cow`] on a map of `size`
/// pairs, knowing only the size.
///
/// `size` is [`map_size`] **before** the operation; the reservation for an absent
/// key is included (nothing is written then). Sound, and a bound rather than an
/// exact figure — see [`map_remove_cow_words_required`].
pub fn map_remove_cow_words_capacity(size: usize) -> usize {
    if size <= MAP_SMALL_MAP_LIMIT {
        flatmap_size_words(size.saturating_sub(1))
    } else {
        HAMT_REMOVE_WORDS_MAX
    }
}

/// The number of words [`map_put_cow`] will write for this map and key: a read-only
/// O(log n) descent (hashing and nibbles only — no `atoms`, no term allocation), so
/// it answers for a key that is not in the map yet.
///
/// It differs from [`map_put_cow_words_capacity`] exactly where the size cannot
/// say: an insert into a `c`-cell collision node writes `c + 1` cells (`1 + c + 1`
/// words for the node) and a value replacement inside one writes `c` cells —
/// `c + 1` is `size`-bounded but not `path`-bounded.
/// No value is known here, so the figure is the *writing* put's: a put that finds
/// the stored value word already identical writes nothing (`erl_map.c:2523-2525`),
/// which makes this figure a sound upper bound for it.
///
/// The figure is exact on every arm **except the flat→HAMT conversion** — a flat map
/// holding `MAP_SMALL_MAP_LIMIT` pairs and an absent key — where it is `write_map`'s
/// capacity bound `map_words_capacity(n + 1)`: `711` at `n = 32`, against the `127`
/// words `write_map` actually writes for `33` keys.
/// That is an over-reservation of address space only, never an under-reservation,
/// which is why this arm carries no `written == plan.words` assert.
/// [`map_remove_cow_words_required`] has no such arm: it is exact everywhere.
///
/// A caller that already holds the map can always reserve this figure; a caller
/// that only knows the size must use the capacity bound.
pub fn map_put_cow_words_required(map: Eterm, key: Eterm) -> TermResult<usize> {
    let plan = plan_put(map, key, None, &AtomTable::new())?;
    Ok(plan.words)
}

/// The **exact** number of words [`map_remove_cow`] will write for this map and
/// key (`0` for an absent key, which writes nothing). Otherwise the same read-only
/// O(log n) descent [`map_put_cow_words_required`] performs.
pub fn map_remove_cow_words_required(map: Eterm, key: Eterm) -> TermResult<usize> {
    let plan = plan_remove(map, key, &AtomTable::new())?;
    Ok(plan.words)
}

// ---------------------------------------------------------------------------
// Write cursor
// ---------------------------------------------------------------------------

/// A cursor over the caller-reserved region: `base` is the first word, `used` the
/// number of words written so far.
struct Out {
    base: *mut Eterm,
    used: usize,
}

impl Out {
    fn new(base: *mut Eterm) -> Out {
        Out { base, used: 0 }
    }

    /// The words written so far (the `debug_assert` guard the tests lean on).
    fn written(&self) -> usize {
        self.used
    }

    /// The next free word.
    ///
    /// # Safety
    ///
    /// At least one more reserved word must be available.
    unsafe fn here(&self) -> *mut Eterm {
        // SAFETY: the caller guarantees the reservation has room.
        unsafe { self.base.add(self.used) }
    }

    /// Appends one word.
    ///
    /// # Safety
    ///
    /// As [`Out::here`].
    unsafe fn put(&mut self, w: Eterm) {
        // SAFETY: the caller guarantees the reservation has room for one more word.
        unsafe { *self.base.add(self.used) = w };
        self.used += 1;
    }

    /// Writes a fresh, header-less `[Key, Value]` cons cell and returns its term.
    ///
    /// # Safety
    ///
    /// Two reserved words must be available.
    unsafe fn leaf(&mut self, key: Eterm, value: Eterm) -> Eterm {
        let p = unsafe { self.here() };
        // SAFETY: two reserved words are available at `p` (`CAR` at offset 0,
        // `CDR` at offset 1) and both are written before the pointer escapes.
        unsafe {
            self.put(key);
            self.put(value);
        }
        make_list(p as *const Eterm)
    }

    /// Copies `count` slot words of a node starting at `src + off`, writing
    /// `child` at output slot `at` and — when `replace` is set — consuming (not
    /// copying) source slot `at` in its place.
    ///
    /// # Safety
    ///
    /// `src` must address at least `off + count` readable words of a live source
    /// node (in a heap that outlives the result) and the cursor must have room for
    /// `count` words, plus one when `replace` is clear.
    unsafe fn slots(
        &mut self,
        src: *const Eterm,
        off: usize,
        count: usize,
        at: usize,
        child: Eterm,
        replace: bool,
    ) {
        let mut inserted = false;
        for i in 0..count {
            if replace && i == at {
                // SAFETY: the caller reserved this word; one source slot is skipped.
                unsafe { self.put(child) };
                inserted = true;
                continue;
            }
            if !replace && i == at {
                // SAFETY: the caller reserved this word.
                unsafe { self.put(child) };
                inserted = true;
            }
            // SAFETY: `src` has `off + count` readable words (caller's contract).
            unsafe { self.put(*src.add(off + i)) };
        }
        if !inserted {
            // The child goes last (an append past the source's last slot).
            unsafe { self.put(child) };
        }
    }

    /// Copies `count` slot words starting at `src + off`, skipping source slot
    /// `drop` (a delete's node shrink).
    ///
    /// # Safety
    ///
    /// As [`Out::slots`], with room for `count - 1` words.
    unsafe fn slots_dropping(&mut self, src: *const Eterm, off: usize, count: usize, drop: usize) {
        for i in 0..count {
            if i == drop {
                continue;
            }
            // SAFETY: `src` has `off + count` readable words (caller's contract).
            unsafe { self.put(*src.add(off + i)) };
        }
    }
}

// ---------------------------------------------------------------------------
// Node classification (the C's `primary_tag` dispatch)
// ---------------------------------------------------------------------------

/// What a HAMT child slot holds.
enum Child {
    /// A two-word `[Key, Value]` cons cell.
    Leaf(Eterm),
    /// A deeper hash-map node, at its header word.
    Node(*const Eterm),
    /// A collision node: an `arityval` tuple of leaf cells.
    Collision(Eterm),
}

/// Classifies one HAMT child slot word (`erl_map.c:2538`'s `primary_tag` switch).
fn classify_child(t: Eterm) -> TermResult<Child> {
    if is_list(t) {
        return Ok(Child::Leaf(t));
    }
    if !is_boxed_term(t) {
        return Err(TermError::BadMap);
    }
    let p = boxed_val(t) as *const Eterm;
    // SAFETY: `t` is a live boxed term, so `p` addresses its header word.
    let header = unsafe { *p };
    if is_map_header(header) {
        if is_flatmap_header(header) {
            // A flat map can never be a hash-map child.
            return Err(TermError::BadMap);
        }
        Ok(Child::Node(p))
    } else if is_arity_value(header) {
        Ok(Child::Collision(t))
    } else {
        Err(TermError::BadMap)
    }
}

/// The layout facts of a hash-map node header: `(slot-word offset, bitmap, is a
/// head)`. `(2, HAMT_FULL_BITMAP, true)` for an 18-word head array.
fn node_layout(header: Eterm) -> TermResult<(usize, u16, bool)> {
    match map_header_type(header) as usize {
        MAP_HEADER_TAG_HAMT_HEAD_ARRAY => Ok((2, HAMT_FULL_BITMAP, true)),
        MAP_HEADER_TAG_HAMT_HEAD_BITMAP => Ok((2, map_header_val(header) as u16, true)),
        MAP_HEADER_TAG_HAMT_NODE_BITMAP => Ok((1, map_header_val(header) as u16, false)),
        _ => Err(TermError::BadMap),
    }
}

/// The output slot index of nibble `nib` in a node with `bitmap`: the number of
/// set bits below it (`hashmap_bitcount(bmp & (bp - 1))`, `erl_map.h:206`).
fn slot_of(bitmap: u16, bit: u16) -> usize {
    (bitmap & (bit - 1)).count_ones() as usize
}

/// The header word of a head/inner bitmap node with `bitmap` set bits. A head
/// whose bitmap is full is the 18-word `MAP_HEADER_HAMT_HEAD_ARRAY`
/// (`erl_map.c:2748-2750`); an inner full-bitmap node stays a bitmap
/// (`erl_map.c:1146`).
fn bitmap_header(bitmap: u16, head: bool) -> Eterm {
    if head {
        if bitmap == HAMT_FULL_BITMAP {
            make_map_header(MAP_HEADER_TAG_HAMT_HEAD_ARRAY, 1, HAMT_FULL_BITMAP as usize)
        } else {
            make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, bitmap as usize)
        }
    } else {
        make_map_header(MAP_HEADER_TAG_HAMT_NODE_BITMAP, 0, bitmap as usize)
    }
}

// ---------------------------------------------------------------------------
// Put: plan
// ---------------------------------------------------------------------------

/// One node on a put's copy path, fully determined at plan time (so the writing
/// pass allocates nothing and only copies).
enum PutFrame {
    /// A bitmap node — the head (`[hdr][size][n slots]`, `2 + n` words) or an inner
    /// node (`[hdr][n slots]`, `1 + n` words). `bitmap` is the **new** bitmap, so
    /// `popcount(bitmap)` is the new slot count; `insert` means the child occupies a
    /// freshly set nibble, so the source has *no* word at `slot` (it is shifted in).
    Bitmap {
        src: *const Eterm,
        head: bool,
        bitmap: u16,
        slot: usize,
        insert: bool,
    },
    /// A collision node: `[arityval(cells)][cells]` (`1 + cells` words), the child
    /// written at `slot` (`insert` again meaning "a new cell, shift the rest").
    Collision {
        src: *const Eterm,
        cells: usize,
        slot: usize,
        insert: bool,
    },
    /// One single-bit inner node of a divergence chain: `2` words.
    ChainSingle { bit: u16 },
    /// The two-slot inner node that ends a divergence chain: `3` words, the two
    /// children in ascending-bit order.
    ChainPair {
        first_bit: u16,
        second_bit: u16,
        new_first: bool,
        old_leaf: Eterm,
    },
    /// The two-cell collision node that ends a divergence chain which agreed on all
    /// `HAMT_MAX_LEVEL` nibbles (`erl_map.c:2642-2646`): `3` words, cells in
    /// `compare::cmp` order.
    ChainCollision { old_leaf: Eterm, new_first: bool },
}

impl PutFrame {
    fn words(&self) -> usize {
        match *self {
            PutFrame::Bitmap {
                head, bitmap, ..
            } => {
                let slots = bitmap.count_ones() as usize;
                if head {
                    2 + slots
                } else {
                    1 + slots
                }
            }
            PutFrame::Collision { cells, .. } => 1 + cells,
            PutFrame::ChainSingle { .. } => 2,
            PutFrame::ChainPair { .. } => 3,
            PutFrame::ChainCollision { .. } => 3,
        }
    }
}

/// What a put has to do.
enum PutAction {
    /// The stored value word is already the new one: write nothing, return the input.
    Nothing,
    /// A flat map's existing key takes a new value: `[hdr][n][keys]` + `n` values,
    /// reusing the source's keys-tuple word (`erl_map.c:2142-2150`).
    FlatReplace {
        n: usize,
        keys_word: Eterm,
        slot: usize,
    },
    /// A new key in a flat map with `n < 32` pairs: a freshly merged, sorted flat map.
    FlatInsert { n: usize },
    /// The 33rd key: `erts_hashmap_from_ks_and_vs_extra` — convert to a hash map.
    FlatConvert { n: usize },
    /// The HAMT path: `frames` is head-first, so the writer walks it in reverse.
    Hamt { frames: Vec<PutFrame>, delta: usize },
}

/// A put's plan: what to do and exactly how many words it writes.
struct PutPlan {
    action: PutAction,
    words: usize,
}

fn plan_put(
    map: Eterm,
    key: Eterm,
    value: Option<Eterm>,
    atoms: &AtomTable,
) -> TermResult<PutPlan> {
    if !is_boxed_term(map) {
        return Err(TermError::BadMap);
    }
    let p = boxed_val(map) as *const Eterm;
    // SAFETY: `map` is a live boxed term, so `p` addresses its header word.
    let header = unsafe { *p };
    if is_flatmap_header(header) {
        return plan_flat_put(p, map, key, value, atoms);
    }
    if !is_map_header(header) {
        return Err(TermError::BadMap);
    }
    plan_hamt_put(p, key, value, atoms)
}

/// The flat-map put arm (`erl_map.c:2105-2208`).
fn plan_flat_put(
    p: *const Eterm,
    map: Eterm,
    key: Eterm,
    value: Option<Eterm>,
    atoms: &AtomTable,
) -> TermResult<PutPlan> {
    // SAFETY: `p` is a flat map's head word; word 1 is its raw pair count.
    let n = unsafe { (*p.add(1)).raw() };
    // SAFETY: `map` is a live flat map.
    let (keys, values) = unsafe { flatmap_parts(map) };
    let n = n.min(keys.len());
    for i in 0..n {
        if eq(keys[i], key, atoms) {
            if let Some(v) = value {
                if values[i] == v {
                    return Ok(PutPlan {
                        action: PutAction::Nothing,
                        words: 0,
                    });
                }
            }
            // SAFETY: word 2 of a flat map is its keys tuple.
            let keys_word = unsafe { *p.add(2) };
            return Ok(PutPlan {
                action: PutAction::FlatReplace {
                    n,
                    keys_word,
                    slot: i,
                },
                words: 3 + n,
            });
        }
    }
    if n < MAP_SMALL_MAP_LIMIT {
        Ok(PutPlan {
            action: PutAction::FlatInsert { n },
            words: flatmap_size_words(n + 1),
        })
    } else {
        Ok(PutPlan {
            action: PutAction::FlatConvert { n },
            words: map_words_capacity(n + 1),
        })
    }
}

/// The HAMT put arm: copy the path, share the rest (`erts_hashmap_insert_down` /
/// `_up`, `erl_map.c:2506-2790`).
fn plan_hamt_put(
    p: *const Eterm,
    key: Eterm,
    value: Option<Eterm>,
    atoms: &AtomTable,
) -> TermResult<PutPlan> {
    let hash = erts_map_hash(key);
    let mut frames: Vec<PutFrame> = Vec::new();
    let mut node = p;
    let mut level = 0usize;
    let delta;
    loop {
        // SAFETY: `node` is a live hash-map node's header word.
        let header = unsafe { *node };
        let (off, bitmap, head) = node_layout(header)?;
        let nib = hashmap_nibble(hash, level) as usize;
        let bit = 1u16 << nib;
        let slot = slot_of(bitmap, bit);
        if bitmap & bit == 0 {
            // A nibble this node does not cover yet: a new slot, one word wider.
            frames.push(PutFrame::Bitmap {
                src: node,
                head,
                bitmap: bitmap | bit,
                slot,
                insert: true,
            });
            delta = 1;
            break;
        }
        frames.push(PutFrame::Bitmap {
            src: node,
            head,
            bitmap,
            slot,
            insert: false,
        });
        // SAFETY: `node` has `off + popcount(bitmap)` slot words and `slot` indexes them.
        let child = unsafe { *node.add(off + slot) };
        match classify_child(child)? {
            Child::Node(sub) => {
                if level + 1 > HAMT_MAX_LEVEL - 1 {
                    return Err(TermError::BadMap);
                }
                level += 1;
                node = sub;
            }
            Child::Leaf(cell) => {
                // SAFETY: a leaf slot holds a live `[Key, Value]` cons cell.
                let (ckey, cval) = unsafe { (car(cell), cdr(cell)) };
                if eq(ckey, key, atoms) {
                    if let Some(v) = value {
                        if cval == v {
                            return Ok(PutPlan {
                                action: PutAction::Nothing,
                                words: 0,
                            });
                        }
                    }
                    delta = 0;
                } else {
                    push_divergence(&mut frames, cell, ckey, key, hash, level + 1, atoms);
                    delta = 1;
                }
                break;
            }
            Child::Collision(coll) => {
                // SAFETY: a collision slot holds a live arityval tuple of cons cells.
                let cells = unsafe { tuple_size(coll) };
                let src = boxed_val(coll) as *const Eterm;
                let mut slot = cells;
                let mut found = false;
                for i in 0..cells {
                    // SAFETY: `i < cells`.
                    let cell = unsafe { tuple_element(coll, i) };
                    // SAFETY: a collision cell is a live cons cell.
                    let c = cmp(key, unsafe { car(cell) }, atoms);
                    if c == Ordering::Equal {
                        if let Some(v) = value {
                            if unsafe { cdr(cell) } == v {
                                return Ok(PutPlan {
                                    action: PutAction::Nothing,
                                    words: 0,
                                });
                            }
                        }
                        frames.push(PutFrame::Collision {
                            src,
                            cells,
                            slot: i,
                            insert: false,
                        });
                        found = true;
                        break;
                    }
                    if c == Ordering::Less {
                        slot = i;
                        break;
                    }
                }
                if found {
                    delta = 0;
                } else {
                    frames.push(PutFrame::Collision {
                        src,
                        cells: cells + 1,
                        slot,
                        insert: true,
                    });
                    delta = 1;
                }
                break;
            }
        }
    }
    let words = 2 + frames.iter().map(PutFrame::words).sum::<usize>();
    Ok(PutPlan {
        action: PutAction::Hamt { frames, delta },
        words,
    })
}

/// Appends the C's `insert_subnodes` divergence chain (`erl_map.c:2604-2646`): the
/// `new` key meets `old_leaf` (holding `old_key`) at `level`, so single-bit inner
/// nodes are pushed for every nibble the two agree on and a two-slot node for the
/// first nibble they disagree on; agreeing on all `HAMT_MAX_LEVEL` nibbles means a
/// full `erts_map_hash` collision, i.e. a two-cell collision node.
fn push_divergence(
    frames: &mut Vec<PutFrame>,
    old_leaf: Eterm,
    old_key: Eterm,
    new_key: Eterm,
    new_hash: u64,
    level: usize,
    atoms: &AtomTable,
) {
    let old_hash = erts_map_hash(old_key);
    let mut lvl = level;
    loop {
        if lvl >= HAMT_MAX_LEVEL {
            frames.push(PutFrame::ChainCollision {
                old_leaf,
                new_first: cmp(new_key, old_key, atoms) == Ordering::Less,
            });
            return;
        }
        let i = hashmap_nibble(new_hash, lvl) as u16;
        let j = hashmap_nibble(old_hash, lvl) as u16;
        if i != j {
            frames.push(PutFrame::ChainPair {
                first_bit: 1 << i.min(j),
                second_bit: 1 << i.max(j),
                new_first: i < j,
                old_leaf,
            });
            return;
        }
        frames.push(PutFrame::ChainSingle { bit: 1 << i });
        lvl += 1;
    }
}

// ---------------------------------------------------------------------------
// Put: write
// ---------------------------------------------------------------------------

/// Writes one put frame with `child` in its slot, returning the new node's term.
///
/// # Safety
///
/// The cursor must have room for `frame.words()` words, `child` must be a live term
/// of the frame's slot kind, and `src` must point into the live source map.
unsafe fn put_frame(out: &mut Out, frame: &PutFrame, child: Eterm, head_size: usize) -> Eterm {
    match *frame {
        PutFrame::Bitmap {
            src,
            head,
            bitmap,
            slot,
            insert,
        } => {
            let p = unsafe { out.here() };
            let source_slots = bitmap.count_ones() as usize - usize::from(insert);
            // SAFETY: the reservation covers the header (and the head's `size`
            // word) plus one output slot per new slot; `src` has `source_slots`.
            unsafe {
                out.put(bitmap_header(bitmap, head));
                let off = if head {
                    out.put(Eterm::from_raw(head_size));
                    2
                } else {
                    1
                };
                out.slots(src, off, source_slots, slot, child, !insert);
            }
            make_boxed(p)
        }
        PutFrame::Collision {
            src,
            cells,
            slot,
            insert,
        } => {
            let p = unsafe { out.here() };
            // SAFETY: the reservation covers `1 + cells` words; the source has
            // `cells - 1` cells when a new one is inserted.
            unsafe {
                out.put(make_arityval(cells));
                out.slots(src, 1, cells - usize::from(insert), slot, child, !insert);
            }
            make_boxed(p)
        }
        PutFrame::ChainSingle { bit } => {
            let p = unsafe { out.here() };
            // SAFETY: two reserved words: the inner node's header and its one slot.
            unsafe {
                out.put(make_map_header(MAP_HEADER_TAG_HAMT_NODE_BITMAP, 0, bit as usize));
                out.put(child);
            }
            make_boxed(p)
        }
        PutFrame::ChainPair {
            first_bit,
            second_bit,
            new_first,
            old_leaf,
        } => {
            let p = unsafe { out.here() };
            // SAFETY: three reserved words: the header and the two slots.
            unsafe {
                out.put(make_map_header(
                    MAP_HEADER_TAG_HAMT_NODE_BITMAP,
                    0,
                    (first_bit | second_bit) as usize,
                ));
                if new_first {
                    out.put(child);
                    out.put(old_leaf);
                } else {
                    out.put(old_leaf);
                    out.put(child);
                }
            }
            make_boxed(p)
        }
        PutFrame::ChainCollision {
            old_leaf,
            new_first,
        } => {
            let p = unsafe { out.here() };
            // SAFETY: three reserved words: the arityval header and the two cells.
            unsafe {
                out.put(make_arityval(2));
                if new_first {
                    out.put(child);
                    out.put(old_leaf);
                } else {
                    out.put(old_leaf);
                    out.put(child);
                }
            }
            make_boxed(p)
        }
    }
}

// ---------------------------------------------------------------------------
// Put: write
// ---------------------------------------------------------------------------

/// Executes a put plan.
///
/// # Safety
///
/// `hp` must address at least `plan.words` writable words in a heap that outlives
/// `map` (the result shares the source map's sub-graphs by word) and that is not
/// the source map's own live region (a node is never relocated in place).
unsafe fn exec_put(
    hp: *mut Eterm,
    plan: PutPlan,
    map: Eterm,
    key: Eterm,
    value: Eterm,
    atoms: &AtomTable,
) -> TermResult<Eterm> {
    let size = map_size(map, atoms)?;
    match plan.action {
        PutAction::Nothing => Ok(map),
        PutAction::FlatReplace { n, keys_word, slot } => {
            // SAFETY: `map` is a live flat map with `n` pairs.
            let (_, values) = unsafe { flatmap_parts(map) };
            let mut out = Out::new(hp);
            // SAFETY: the reservation is `3 + n` words and each slot is written once.
            unsafe {
                // `MAP_HEADER_FLATMAP` is `MAKE_MAP_HEADER(TAG_FLATMAP_HEAD, 1, 0)`
                // = `make_header(4, MAP_SUBTAG)` = `0x12C` (`erl_map.h:181-186`) —
                // the arity half of the inner field is `1`, not the header's word
                // count, so this must not be spelled with any other arity.
                out.put(make_map_header(MAP_HEADER_TAG_FLATMAP_HEAD, 1, 0));
                out.put(Eterm::from_raw(n));
                out.put(keys_word);
                for (i, v) in values.iter().enumerate().take(n) {
                    out.put(if i == slot { value } else { *v });
                }
            }
            debug_assert_eq!(out.written(), plan.words);
            Ok(make_boxed(hp))
        }
        PutAction::FlatInsert { n } => {
            // SAFETY: `map` is a live flat map with `n` pairs.
            let (keys, values) = unsafe { flatmap_parts(map) };
            let mut pairs: Vec<(Eterm, Eterm)> = keys
                .iter()
                .copied()
                .zip(values.iter().copied())
                .take(n)
                .collect();
            pairs.push((key, value));
            // The C's linear merge (`while (n && cmp(*ks, key) < 0) *hp++ = ...`)
            // writes the merged keys tuple in `cmp_flatmap_keys` order; `write_flatmap`
            // sorts the pair slice by the same order, so the result is identical.
            pairs.sort_by(|a, b| cmp_flatmap_keys(a.0, b.0, atoms));
            debug_assert_eq!(flatmap_size_words(pairs.len()), plan.words);
            let map = unsafe { write_flatmap(hp, &pairs) };
            Ok(map)
        }
        PutAction::FlatConvert { n } => {
            // SAFETY: `map` is a live flat map with `n` pairs.
            let (keys, values) = unsafe { flatmap_parts(map) };
            let mut pairs: Vec<(Eterm, Eterm)> = keys
                .iter()
                .copied()
                .zip(values.iter().copied())
                .take(n)
                .collect();
            pairs.push((key, value));
            debug_assert_eq!(pairs.len(), n + 1);
            unsafe { write_map(hp, &pairs, atoms) }
        }
        PutAction::Hamt { frames, delta } => {
            let mut out = Out::new(hp);
            // SAFETY: the reservation is `2 + Σ frames`, starting with the new leaf.
            let mut child = unsafe { out.leaf(key, value) };
            let head_size = size + delta;
            for frame in frames.iter().rev() {
                // SAFETY: the cursor has room for this frame and `src` points into
                // the live source map.
                child = unsafe { put_frame(&mut out, frame, child, head_size) };
            }
            debug_assert_eq!(out.written(), plan.words);
            Ok(child)
        }
    }
}

// ---------------------------------------------------------------------------
// Remove: plan
// ---------------------------------------------------------------------------

/// The result a delete's upward pass carries into the parent — the C's `res`
/// (`THE_NON_VALUE` = [`Res::Removed`], a leaf cons cell = [`Res::Leaf`], a copied
/// node = [`Res::Node`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Res {
    Removed,
    Leaf,
    Node,
}

/// One node on a delete's copy path, fully determined at plan time (so the writing
/// pass keys only on [`Res`] and the frame).
enum DelFrame {
    /// The node keeps its slot count and its `slot` word becomes the rising child
    /// (`size - 1` at the head).
    BitmapKeep {
        src: *const Eterm,
        head: bool,
        bitmap: u16,
        slot: usize,
    },
    /// The node loses `slot` (`bitmap` is already `bitmap ^ bp`).
    BitmapDrop {
        src: *const Eterm,
        head: bool,
        bitmap: u16,
        slot: usize,
    },
    /// A collision node loses one cell.
    CollisionShrink {
        src: *const Eterm,
        cells: usize,
        slot: usize,
    },
    /// A two-cell collision node collapses: the surviving cell rises (0 words).
    Collapse { other: Eterm },
    /// The node itself vanishes and the child result rises unchanged (0 words).
    Pass,
}

impl DelFrame {
    fn words(&self) -> usize {
        match *self {
            DelFrame::BitmapKeep {
                head, bitmap, ..
            }
            | DelFrame::BitmapDrop {
                head, bitmap, ..
            } => {
                let slots = bitmap.count_ones() as usize;
                if head {
                    2 + slots
                } else {
                    1 + slots
                }
            }
            DelFrame::CollisionShrink { cells, .. } => 1 + cells,
            DelFrame::Collapse { .. } | DelFrame::Pass => 0,
        }
    }
}

/// One node's raw facts, recorded top-down by the descent and turned into a
/// [`DelFrame`] bottom-up once the required size is known.
enum DelSpec {
    Bitmap {
        src: *const Eterm,
        head: bool,
        bitmap: u16,
        bp: u16,
        slot: usize,
        slots: usize,
    },
    Collision {
        src: *const Eterm,
        cells: usize,
        slot: usize,
    },
}

/// What a remove has to do.
enum DelAction {
    /// The key is not in the map: write nothing.
    Absent,
    /// Write the source's pairs minus `key` as a flat map of `n_after` pairs.
    Flat { n_after: usize },
    /// The HAMT path, stored **leaf-first** (`frames[0]` is the deepest node) — the
    /// order the upward `res` pass produces, which is also the write order.
    Hamt { frames: Vec<DelFrame> },
}

/// A remove's plan: what to do and exactly how many words it writes.
struct DelPlan {
    action: DelAction,
    words: usize,
}

impl DelPlan {
    fn absent() -> DelPlan {
        DelPlan {
            action: DelAction::Absent,
            words: 0,
        }
    }
}

fn plan_remove(map: Eterm, key: Eterm, atoms: &AtomTable) -> TermResult<DelPlan> {
    if !is_boxed_term(map) {
        return Err(TermError::BadMap);
    }
    let p = boxed_val(map) as *const Eterm;
    // SAFETY: `map` is a live boxed term, so `p` addresses its header word.
    let header = unsafe { *p };
    if is_flatmap_header(header) {
        return plan_flat_remove(p, map, key, atoms);
    }
    if !is_map_header(header) {
        return Err(TermError::BadMap);
    }
    plan_hamt_remove(map, p, key, atoms)
}

/// The flat-map remove arm (`erts_maps_take`, `erl_map.c:1943-2026`): the keys are
/// already in `cmp_flatmap_keys` order, so dropping one and re-writing is a linear
/// copy.
fn plan_flat_remove(
    p: *const Eterm,
    map: Eterm,
    key: Eterm,
    atoms: &AtomTable,
) -> TermResult<DelPlan> {
    // SAFETY: `p` is a flat map's head word; word 1 is its raw pair count.
    let n = unsafe { (*p.add(1)).raw() };
    // SAFETY: `map` is a live flat map.
    let (keys, _) = unsafe { flatmap_parts(map) };
    let n = n.min(keys.len());
    for k in keys.iter().take(n) {
        if eq(*k, key, atoms) {
            let n_after = n - 1;
            return Ok(DelPlan {
                action: DelAction::Flat { n_after },
                words: flatmap_size_words(n_after),
            });
        }
    }
    Ok(DelPlan::absent())
}

/// The HAMT remove arm (`hashmap_delete`, `erl_map.c:2881-3180`).
fn plan_hamt_remove(
    map: Eterm,
    p: *const Eterm,
    key: Eterm,
    atoms: &AtomTable,
) -> TermResult<DelPlan> {
    let hash = erts_map_hash(key);
    let mut specs: Vec<DelSpec> = Vec::new();
    let mut node = p;
    let mut level = 0usize;
    loop {
        // SAFETY: `node` is a live hash-map node's header word.
        let header = unsafe { *node };
        let (off, bitmap, head) = node_layout(header)?;
        let nib = hashmap_nibble(hash, level) as usize;
        let bp = 1u16 << nib;
        if bitmap & bp == 0 {
            return Ok(DelPlan::absent());
        }
        let slots = bitmap.count_ones() as usize;
        let slot = slot_of(bitmap, bp);
        specs.push(DelSpec::Bitmap {
            src: node,
            head,
            bitmap,
            bp,
            slot,
            slots,
        });
        // SAFETY: `node` has `off + slots` slot words and `slot` indexes them.
        let child = unsafe { *node.add(off + slot) };
        match classify_child(child)? {
            Child::Node(sub) => {
                if level + 1 > HAMT_MAX_LEVEL - 1 {
                    return Err(TermError::BadMap);
                }
                level += 1;
                node = sub;
            }
            Child::Leaf(cell) => {
                // SAFETY: a leaf slot holds a live `[Key, Value]` cons cell.
                if !eq(unsafe { car(cell) }, key, atoms) {
                    return Ok(DelPlan::absent());
                }
                break;
            }
            Child::Collision(coll) => {
                // SAFETY: a collision slot holds a live arityval tuple of cons cells.
                let cells = unsafe { tuple_size(coll) };
                let src = boxed_val(coll) as *const Eterm;
                let mut found = None;
                for i in 0..cells {
                    // SAFETY: `i < cells`, and a collision cell is a live `[K, V]` cons.
                    let cell = unsafe { tuple_element(coll, i) };
                    // SAFETY: as above.
                    let ckey = unsafe { car(cell) };
                    if eq(ckey, key, atoms) {
                        found = Some(i);
                        break;
                    }
                }
                match found {
                    Some(slot) => specs.push(DelSpec::Collision { src, cells, slot }),
                    None => return Ok(DelPlan::absent()),
                }
                break;
            }
        }
    }
    let size = map_size(map, atoms)?;
    debug_assert!(size >= 1, "a map that holds the key has at least one pair");
    let n_after = size.saturating_sub(1);
    if n_after <= MAP_SMALL_MAP_LIMIT {
        // `erl_map.c:2946-2981`: a hash map that shrank to the flat limit is rebuilt
        // as a flat map, in `cmp_flatmap_keys` order.
        return Ok(DelPlan {
            action: DelAction::Flat { n_after },
            words: flatmap_size_words(n_after),
        });
    }
    // The C's upward pass (`erl_map.c:2990-3180`), bottom-up: each frame's shape
    // depends only on its own facts and on whether the child slot vanished
    // (`res == THE_NON_VALUE`), kept a leaf (a list word), or came back as a node.
    let mut frames: Vec<DelFrame> = Vec::with_capacity(specs.len());
    let mut res = Res::Removed;
    for spec in specs.iter().rev() {
        match *spec {
            DelSpec::Collision { src, cells, slot } => {
                if cells > 2 {
                    // Collision nodes here are the innermost level, so the copied node
                    // drops one cell and stays a node.
                    frames.push(DelFrame::CollisionShrink {
                        src,
                        cells: cells - 1,
                        slot,
                    });
                    res = Res::Node;
                } else {
                    // Two cells: the surviving one rises into the parent.
                    // SAFETY: `src` holds `cells == 2` cell words at offsets 1 and 2.
                    let other = unsafe { *src.add(1 + (1 - slot)) };
                    frames.push(DelFrame::Collapse { other });
                    res = Res::Leaf;
                }
            }
            DelSpec::Bitmap {
                src,
                head,
                bitmap,
                bp,
                slot,
                slots,
            } => match (res, head, slots) {
                (Res::Removed, false, 1) => {
                    // An inner node whose only slot vanished has nothing left: it goes.
                    frames.push(DelFrame::Pass);
                }
                (Res::Removed, false, 2) => {
                    // SAFETY: `src` holds two slot words at offsets 1 and 2.
                    let other = unsafe { *src.add(1 + (1 - slot)) };
                    if is_list(other) {
                        // The sibling is a leaf: the node collapses into it.
                        frames.push(DelFrame::Collapse { other });
                        res = Res::Leaf;
                    } else {
                        frames.push(DelFrame::BitmapDrop {
                            src,
                            head,
                            bitmap: bitmap ^ bp,
                            slot,
                        });
                        res = Res::Node;
                    }
                }
                (Res::Removed, _, _) => {
                    frames.push(DelFrame::BitmapDrop {
                        src,
                        head,
                        bitmap: bitmap ^ bp,
                        slot,
                    });
                    res = Res::Node;
                }
                (Res::Leaf, false, 1) => {
                    // A leaf rising into a single-slot inner node: it takes its place.
                    frames.push(DelFrame::Pass);
                }
                _ => {
                    frames.push(DelFrame::BitmapKeep {
                        src,
                        head,
                        bitmap,
                        slot,
                    });
                    res = Res::Node;
                }
            },
        }
    }
    let words = frames.iter().map(DelFrame::words).sum();
    Ok(DelPlan {
        action: DelAction::Hamt { frames },
        words,
    })
}

// ---------------------------------------------------------------------------
// Remove: write
// ---------------------------------------------------------------------------

/// Writes one delete frame, returning the new child result.
///
/// # Safety
///
/// The cursor must have room for `frame.words()`, and `src` must point into the live
/// source map.
unsafe fn put_del_frame(
    out: &mut Out,
    frame: &DelFrame,
    child: Option<Eterm>,
    head_size: usize,
) -> TermResult<Option<Eterm>> {
    match *frame {
        DelFrame::Pass => Ok(child),
        DelFrame::Collapse { other } => Ok(Some(other)),
        DelFrame::BitmapKeep {
            src,
            head,
            bitmap,
            slot,
        } => {
            let child = child.ok_or(TermError::BadMap)?;
            let p = unsafe { out.here() };
            let slots = bitmap.count_ones() as usize;
            // SAFETY: the reservation is `2 + slots` (head) or `1 + slots`, and the
            // source has the same slot count.
            unsafe {
                out.put(bitmap_header(bitmap, head));
                let off = if head {
                    out.put(Eterm::from_raw(head_size));
                    2
                } else {
                    1
                };
                out.slots(src, off, slots, slot, child, true);
            }
            Ok(Some(make_boxed(p)))
        }
        DelFrame::BitmapDrop {
            src,
            head,
            bitmap,
            slot,
        } => {
            let p = unsafe { out.here() };
            let slots = bitmap.count_ones() as usize;
            // SAFETY: the reservation is `2 + slots` (head) or `1 + slots`; the
            // source has `slots + 1` and one is dropped.
            unsafe {
                out.put(bitmap_header(bitmap, head));
                let off = if head {
                    out.put(Eterm::from_raw(head_size));
                    2
                } else {
                    1
                };
                out.slots_dropping(src, off, slots + 1, slot);
            }
            Ok(Some(make_boxed(p)))
        }
        DelFrame::CollisionShrink { src, cells, slot } => {
            let p = unsafe { out.here() };
            // SAFETY: the reservation is `1 + cells`; the source holds `cells + 1`.
            unsafe {
                out.put(make_arityval(cells));
                out.slots_dropping(src, 1, cells + 1, slot);
            }
            Ok(Some(make_boxed(p)))
        }
    }
}

/// Executes a remove plan.
///
/// # Safety
///
/// `hp` must address at least `plan.words` writable words in a heap that outlives
/// `map` (the result shares the source map's sub-graphs by word) and that is not the
/// source map's own live region.
unsafe fn exec_remove(
    hp: *mut Eterm,
    plan: DelPlan,
    map: Eterm,
    key: Eterm,
    atoms: &AtomTable,
) -> TermResult<Option<Eterm>> {
    let size = map_size(map, atoms)?;
    match plan.action {
        DelAction::Absent => Ok(None),
        DelAction::Flat { n_after } => {
            let mut pairs: Vec<(Eterm, Eterm)> = collect_pairs(map)?;
            pairs.retain(|(k, _)| !eq(*k, key, atoms));
            debug_assert_eq!(pairs.len(), n_after);
            // The source keys are already in `cmp_flatmap_keys` order, so this sort is
            // a linear-order pass (`erts_validate_and_sort_flatmap`'s shape).
            pairs.sort_by(|a, b| cmp_flatmap_keys(a.0, b.0, atoms));
            Ok(Some(unsafe { write_flatmap(hp, &pairs) }))
        }
        DelAction::Hamt { frames } => {
            let mut out = Out::new(hp);
            let head_size = size.saturating_sub(1);
            // `frames` is stored leaf-first (the order the upward `res` pass
            // produced it: `frames[0]` is the deepest node, `frames.last()` the head),
            // so the write walks it forwards — the C's `ESTACK_POP` order.
            let mut res: Option<Eterm> = None;
            for frame in frames.iter() {
                // SAFETY: the cursor has room for this frame and `src` points into
                // the live source map.
                res = unsafe { put_del_frame(&mut out, frame, res, head_size)? };
            }
            debug_assert_eq!(out.written(), plan.words);
            // The head is always a bitmap frame, so it always produces a node.
            match res {
                Some(head) => Ok(Some(head)),
                None => Err(TermError::BadMap),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// `maps:put(Key, Value, Map)` into a caller-reserved heap region, copying **only
/// the nodes on `Key`'s hash path** (C `erts_maps_put`, `erl_map.c:2105-2235`).
///
/// Returns the new map, or the input map word unchanged when the stored value word
/// is already `Value` (nothing is written then). `hp` must address at least
/// [`map_put_cow_words_capacity`]`(`[`map_size`]`(map))` words — or, when `key`'s
/// path crosses a hash collision node, the exact
/// [`map_put_cow_words_required`]`(map, key)` — of a heap that outlives `map`.
///
/// # Safety
///
/// * `hp` must address that many *writable* words, in a region that is not the
///   source map's live storage (a node is never relocated in place).
/// * The region must live at least as long as any use of the returned map: every
///   off-path subtree, the reused keys tuple and every reused collision cell are
///   shared with the source map **by word**, which is sound only because terms are
///   immutable and are never rewritten in place.
pub unsafe fn map_put_cow(
    hp: *mut Eterm,
    map: Eterm,
    key: Eterm,
    value: Eterm,
    atoms: &AtomTable,
) -> TermResult<Eterm> {
    let plan = plan_put(map, key, Some(value), atoms)?;
    // SAFETY: the caller upholds the two preconditions above.
    unsafe { exec_put(hp, plan, map, key, value, atoms) }
}

/// `maps:remove(Key, Map)` into a caller-reserved heap region, copying **only the
/// nodes on `Key`'s hash path** (C `hashmap_delete`, `erl_map.c:2881-3180`).
///
/// `Ok(None)` iff the key is absent — nothing is written then, and the caller
/// returns its input map (what `maps:remove/2` does). `hp` must address at least
/// [`map_remove_cow_words_capacity`]`(`[`map_size`]`(map))` words — or, when
/// `key`'s path crosses a hash collision node, the exact
/// [`map_remove_cow_words_required`]`(map, key)` — of a heap that outlives `map`.
///
/// # Safety
///
/// The same two preconditions [`map_put_cow`] states.
pub unsafe fn map_remove_cow(
    hp: *mut Eterm,
    map: Eterm,
    key: Eterm,
    atoms: &AtomTable,
) -> TermResult<Option<Eterm>> {
    let plan = plan_remove(map, key, atoms)?;
    // SAFETY: the caller upholds the preconditions of `map_put_cow`.
    unsafe { exec_remove(hp, plan, map, key, atoms) }
}

#[cfg(test)]
mod tests;
