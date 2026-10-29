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

//! The remove arm: the read-only plan (`plan_remove`) and the writer
//! (`exec_remove`) — the flat removal and the C's upward `res` state machine.

use crate::atom::AtomTable;
use crate::boxed::{
    boxed_val, is_boxed_term, is_flatmap_header, is_map_header, make_arityval, make_boxed,
};
use crate::compare::{cmp_flatmap_keys, eq};
use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::ihash::{erts_map_hash, hashmap_nibble};
use crate::list::{car, is_list};
use crate::map::{collect_pairs, map_size, HAMT_MAX_LEVEL, MAP_SMALL_MAP_LIMIT};
use crate::thing::{flatmap_parts, flatmap_size_words, write_flatmap};
use crate::tuple::{tuple_element, tuple_size};

use super::node::{bitmap_header, classify_child, node_layout, slot_of, Child};
use super::Out;

// ---------------------------------------------------------------------------
// Remove: plan
// ---------------------------------------------------------------------------

/// The result a delete's upward pass carries into the parent — the C's `res`
/// (`THE_NON_VALUE` = [`Res::Removed`], a leaf cons cell = [`Res::Leaf`], a copied
/// node = [`Res::Node`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Res {
    Removed,
    Leaf,
    Node,
}

/// One node on a delete's copy path, fully determined at plan time (so the writing
/// pass keys only on [`Res`] and the frame).
pub(super) enum DelFrame {
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
pub(super) enum DelSpec {
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
pub(super) enum DelAction {
    /// The key is not in the map: write nothing.
    Absent,
    /// Write the source's pairs minus `key` as a flat map of `n_after` pairs.
    Flat { n_after: usize },
    /// The HAMT path, stored **leaf-first** (`frames[0]` is the deepest node) — the
    /// order the upward `res` pass produces, which is also the write order.
    Hamt { frames: Vec<DelFrame> },
}

/// A remove's plan: what to do and exactly how many words it writes.
pub(super) struct DelPlan {
    action: DelAction,
    pub(super) words: usize,
}

impl DelPlan {
    fn absent() -> DelPlan {
        DelPlan {
            action: DelAction::Absent,
            words: 0,
        }
    }
}

pub(super) fn plan_remove(map: Eterm, key: Eterm, atoms: &AtomTable) -> TermResult<DelPlan> {
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
pub(super) unsafe fn exec_remove(
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
