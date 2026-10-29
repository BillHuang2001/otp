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

//! The put arm: the read-only plan (`plan_put`) that decides the arm and its exact
//! word count, and the writer (`exec_put`) that executes it.

use core::cmp::Ordering;

use crate::atom::AtomTable;
use crate::boxed::{
    boxed_val, is_boxed_term, is_flatmap_header, is_map_header, make_arityval, make_boxed,
};
use crate::compare::{cmp, cmp_flatmap_keys, eq};
use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::ihash::{erts_map_hash, hashmap_nibble};
use crate::list::{car, cdr};
use crate::map::{
    make_map_header, map_size, map_words_capacity, write_map, HAMT_MAX_LEVEL, MAP_SMALL_MAP_LIMIT,
};
use crate::tag::{MAP_HEADER_TAG_FLATMAP_HEAD, MAP_HEADER_TAG_HAMT_NODE_BITMAP};
use crate::thing::{flatmap_parts, flatmap_size_words, write_flatmap};
use crate::tuple::{tuple_element, tuple_size};

use super::node::{bitmap_header, classify_child, node_layout, slot_of, Child};
use super::Out;

// ---------------------------------------------------------------------------
// Put: plan
// ---------------------------------------------------------------------------

/// One node on a put's copy path, fully determined at plan time (so the writing
/// pass allocates nothing and only copies).
pub(super) enum PutFrame {
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
pub(super) enum PutAction {
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
pub(super) struct PutPlan {
    action: PutAction,
    pub(super) words: usize,
}

pub(super) fn plan_put(
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
pub(super) unsafe fn exec_put(
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
