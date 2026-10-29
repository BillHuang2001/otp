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

//! The word accounting of the path-copying writers: the two size-only capacity
//! bounds, the two path-derived exact figures and the derivation of each figure.

use crate::atom::AtomTable;
use crate::error::TermResult;
use crate::eterm::Eterm;
use crate::map::{map_words_capacity, HAMT_MAX_LEVEL, MAP_SMALL_MAP_LIMIT};
use crate::thing::flatmap_size_words;

use super::put::plan_put;
use super::remove::plan_remove;

// ---------------------------------------------------------------------------
// The node word shapes (the parts every figure below is derived from)
// ---------------------------------------------------------------------------

/// The widest head node: `[hdr][size][16 slots]` (`erl_map.h:181-199`).
const HAMT_HEAD_WORDS_MAX: usize = 2 + HAMT_MAX_LEVEL;

/// The widest inner bitmap node: `[hdr][16 slots]`.
const HAMT_INNER_WORDS_MAX: usize = 1 + HAMT_MAX_LEVEL;

/// A `[Key, Value]` cons cell.
const HAMT_LEAF_WORDS: usize = 2;

/// A collision node's header word (`[arityval(cells)][cells]`, `erl_map.c:2642-2646`).
const HAMT_COLLISION_HEADER_WORDS: usize = 1;

/// The fewest cells a collision node can hold: two keys sharing a whole
/// `erts_map_hash` (the C's `ASSERT(n >= 2)`, `erl_map.c:2570`).
pub(super) const HAMT_COLLISION_CELLS_MIN: usize = 2;

// ---------------------------------------------------------------------------
// Word bounds
// ---------------------------------------------------------------------------

/// A two-cell collision node (`[arityval(2)][cell][cell]`).
const HAMT_COLLISION_WORDS_MIN: usize = HAMT_COLLISION_HEADER_WORDS + HAMT_COLLISION_CELLS_MIN;

/// The widest node spine a hash path can copy: the head plus one inner node per
/// remaining level — `18 + 15 · 17 = 273`. This is the figure the collision-node
/// thresholds below are derived from, because it is what a *real* path occupies.
const HAMT_SPINE_WORDS_MAX: usize =
    HAMT_HEAD_WORDS_MAX + (HAMT_MAX_LEVEL - 1) * HAMT_INNER_WORDS_MAX;

/// The same spine priced as if **every** level were a head (`16 · 18 = 288`) — the
/// deliberately loose bound the two named maxima below are built on.
const HAMT_SPINE_WORDS_LOOSE: usize = HAMT_MAX_LEVEL * HAMT_HEAD_WORDS_MAX;

/// The largest number of words [`map_put_cow`](super::map_put_cow) can write into a map that is
/// already a hash map (`size > MAP_SMALL_MAP_LIMIT`): [`HAMT_SPINE_WORDS_LOOSE`]
/// (`288`), a two-cell collision node (`3`) and the new leaf's cons cell (`2`) =
/// `293`. See the module doc for the derivation, and
/// [`HAMT_PUT_COLLISION_CELLS_MAX`] for the one condition it is sound under.
const HAMT_PUT_WORDS_MAX: usize = HAMT_SPINE_WORDS_LOOSE + HAMT_COLLISION_WORDS_MIN + HAMT_LEAF_WORDS;

/// The largest number of words [`map_remove_cow`](super::map_remove_cow) can write into a map that is
/// already a hash map (`size > MAP_SMALL_MAP_LIMIT`): the same `288`-word spine. A
/// remove writes no cons cell, and its "shrank to `<= 32` keys" flat-conversion arm
/// writes `flatmap_size_words(32) = 68`, which is below the spine bound. Sound under
/// the same condition as [`HAMT_PUT_WORDS_MAX`] — see [`HAMT_REMOVE_COLLISION_CELLS_MAX`].
const HAMT_REMOVE_WORDS_MAX: usize = HAMT_SPINE_WORDS_LOOSE;

/// The largest collision-node cell count [`map_put_cow`](super::map_put_cow)'s size-only
/// reservation covers.
///
/// An insert into a `c`-cell collision node writes the widest spine (`273`), the
/// node's header, `c + 1` cells and the new leaf's cons cell —
/// `273 + 1 + (c + 1) + 2 = 277 + c` — so `c` may be at most
/// `HAMT_PUT_WORDS_MAX - 277 = 16`. A wider node is refused with
/// [`TermError::BadMap`](crate::error::TermError::BadMap) *before* anything is
/// written instead of over-running the reservation (a `debug_assert!` is not a
/// release-mode guard).
pub(super) const HAMT_PUT_COLLISION_CELLS_MAX: usize =
    HAMT_PUT_WORDS_MAX - (HAMT_SPINE_WORDS_MAX + HAMT_COLLISION_HEADER_WORDS + HAMT_LEAF_WORDS + 1);

/// The largest collision-node cell count [`map_remove_cow`](super::map_remove_cow)'s size-only
/// reservation covers: a shrink from `c` cells writes `1 + (c - 1)` of them, so the
/// total is `273 + c` and `c` may be at most `288 - 273 = 15`. A wider node is
/// refused with `BadMap`, exactly like the put side.
pub(super) const HAMT_REMOVE_COLLISION_CELLS_MAX: usize =
    HAMT_REMOVE_WORDS_MAX - HAMT_SPINE_WORDS_MAX;

/// The words a caller must reserve for [`map_put_cow`](super::map_put_cow) on a map of `size` pairs,
/// knowing only the size.
///
/// `size` is [`map_size`](crate::map::map_size) **before** the operation. The figure is sound for every
/// map the writer **accepts** — a canonical map, i.e. one whose collision nodes
/// hold exactly [`HAMT_COLLISION_CELLS_MIN`] cells (any map produced by
/// [`write_map`](crate::map::write_map) or by these writers: two keys sharing all
/// `64` bits of [`erts_map_hash`](crate::ihash::erts_map_hash) are not
/// constructible from ordinary keys). It is a bound and not an exact figure: for
/// an exact one — which differs wherever the key's path crosses a hash
/// **collision node**, whose cell count the map's size cannot bound — call
/// [`map_put_cow_words_required`].
///
/// A map this writer **refuses** (a collision node wider than
/// [`HAMT_PUT_COLLISION_CELLS_MAX`] cells, or a flat map whose head word disagrees
/// with its keys tuple) writes nothing, so no reservation can be over-run for it
/// either; the refusal is the typed
/// [`BadMap`](crate::error::TermError::BadMap).
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

/// The words a caller must reserve for [`map_remove_cow`](super::map_remove_cow) on a map of `size`
/// pairs, knowing only the size.
///
/// `size` is [`map_size`](crate::map::map_size) **before** the operation; the reservation for an absent
/// key is included (nothing is written then). Sound for every map the writer
/// accepts, in the same sense as [`map_put_cow_words_capacity`] (a collision node
/// wider than [`HAMT_REMOVE_COLLISION_CELLS_MAX`] cells is refused, not written),
/// and a bound rather than an exact figure — see
/// [`map_remove_cow_words_required`].
pub fn map_remove_cow_words_capacity(size: usize) -> usize {
    if size <= MAP_SMALL_MAP_LIMIT {
        flatmap_size_words(size.saturating_sub(1))
    } else {
        HAMT_REMOVE_WORDS_MAX
    }
}

/// The number of words [`map_put_cow`](super::map_put_cow) will write for this map and key: a read-only
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
/// The figure is exact for every map the writer **accepts**, on every arm **except
/// the flat→HAMT conversion** — a flat map holding `MAP_SMALL_MAP_LIMIT` pairs and
/// an absent key — where it is `write_map`'s capacity bound
/// `map_words_capacity(n + 1)`: `711` at `n = 32`, against the `127` words
/// `write_map` actually writes for `33` keys.
/// That is an over-reservation of address space only, never an under-reservation,
/// which is why this arm carries no `written == plan.words` assert.
/// [`map_remove_cow_words_required`] has no such arm: it is exact everywhere.
///
/// A map the writer refuses is refused here too: the descent shares its plan, so a
/// collision node wider than [`HAMT_PUT_COLLISION_CELLS_MAX`] cells or a flat map
/// whose head word disagrees with its keys tuple answers the same
/// [`BadMap`](crate::error::TermError::BadMap) — this figure can never lead a
/// caller into a reservation that the write then exceeds.
///
/// A caller that already holds the map can always reserve this figure; a caller
/// that only knows the size must use the capacity bound.
pub fn map_put_cow_words_required(map: Eterm, key: Eterm) -> TermResult<usize> {
    let plan = plan_put(map, key, None, &AtomTable::new())?;
    Ok(plan.words)
}

/// The **exact** number of words [`map_remove_cow`](super::map_remove_cow) will write for this map and
/// key (`0` for an absent key, which writes nothing). Otherwise the same read-only
/// O(log n) descent [`map_put_cow_words_required`] performs, and the same refusal
/// for a map the writer rejects (a collision node wider than
/// [`HAMT_REMOVE_COLLISION_CELLS_MAX`] cells, or a flat map whose head word
/// disagrees with its keys tuple).
pub fn map_remove_cow_words_required(map: Eterm, key: Eterm) -> TermResult<usize> {
    let plan = plan_remove(map, key, &AtomTable::new())?;
    Ok(plan.words)
}
