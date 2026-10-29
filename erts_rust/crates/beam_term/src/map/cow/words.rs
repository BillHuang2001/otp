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
use crate::map::{map_words_capacity, MAP_SMALL_MAP_LIMIT};
use crate::thing::flatmap_size_words;

use super::put::plan_put;
use super::remove::plan_remove;

// ---------------------------------------------------------------------------
// Word bounds
// ---------------------------------------------------------------------------

/// The largest number of words [`map_put_cow`](super::map_put_cow) can write into a map that is
/// already a hash map (`size > MAP_SMALL_MAP_LIMIT`): a `16`-node spine of
/// `18`-word nodes (`288`), a two-cell collision node (`3`) and the new leaf's
/// cons cell (`2`). See the module doc for the derivation.
const HAMT_PUT_WORDS_MAX: usize = 293;

/// The largest number of words [`map_remove_cow`](super::map_remove_cow) can write into a map that is
/// already a hash map (`size > MAP_SMALL_MAP_LIMIT`): a `16`-node spine of
/// `18`-word nodes (`288`). A remove writes no cons cell, and its
/// "shrank to `<= 32` keys" flat-conversion arm writes
/// `flatmap_size_words(32) = 68`, which is below the spine bound.
const HAMT_REMOVE_WORDS_MAX: usize = 288;

/// The words a caller must reserve for [`map_put_cow`](super::map_put_cow) on a map of `size` pairs,
/// knowing only the size.
///
/// `size` is [`map_size`](crate::map::map_size) **before** the operation. The figure is sound (never an
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

/// The words a caller must reserve for [`map_remove_cow`](super::map_remove_cow) on a map of `size`
/// pairs, knowing only the size.
///
/// `size` is [`map_size`](crate::map::map_size) **before** the operation; the reservation for an absent
/// key is included (nothing is written then). Sound, and a bound rather than an
/// exact figure — see [`map_remove_cow_words_required`].
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

/// The **exact** number of words [`map_remove_cow`](super::map_remove_cow) will write for this map and
/// key (`0` for an absent key, which writes nothing). Otherwise the same read-only
/// O(log n) descent [`map_put_cow_words_required`] performs.
pub fn map_remove_cow_words_required(map: Eterm, key: Eterm) -> TermResult<usize> {
    let plan = plan_remove(map, key, &AtomTable::new())?;
    Ok(plan.words)
}
