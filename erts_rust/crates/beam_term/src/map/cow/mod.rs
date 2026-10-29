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
//! (levels `0..=15`) and each widest node is `18` words if every level is priced as
//! a head, so the node spine is at most `288` (the true maximum is `273`: one
//! `18`-word head plus fifteen `17`-word inner nodes); a put adds the leaf's
//! `2`-word cons cell. A *divergence chain* can reach one node deeper: it hangs off
//! the level-`15` node and ends in a two-cell collision node (`3` words) when the
//! two keys agree on all 16 nibbles, i.e. `16 · 18 + 3 + 2 = 293`. A chain that
//! starts at level `lvl <= 15` costs `lvl · 18 + (15 - lvl) · 2 + 3 + 2 = 16 · lvl +
//! 35 <= 275` — always below the `290` spine-plus-cell figure, which is why only the
//! collision case sets the bound. A remove never writes a cons cell and its
//! flat-conversion arm writes `flatmap_size_words(32) = 68`, so `288` covers it.
//!
//! **What those figures are conditional on.** A collision node's cell count is not
//! bounded by the path, so a *canonical* map is the only thing the size-only bound
//! prices: a collision node needs `>= 2` keys sharing all `64` bits of
//! [`erts_map_hash`](crate::ihash::erts_map_hash), and `ihash_mix64` is a bijection
//! over an immediate's raw word, so no such pair is constructible from ordinary
//! keys. A **hand-forged** map whose collision node is wider is refused with
//! [`BadMap`](crate::error::TermError::BadMap) *before* a word is written: the
//! figures the refusals are derived from are `276 + c` (a value replacement in a
//! `c`-cell node), `277 + c` (an insert into one) and `273 + c` (a delete from one),
//! against the named `293`/`288` maxima, i.e. `c <= 16` for a put and `c <= 15` for
//! a remove (`words.rs`'s `HAMT_PUT_COLLISION_CELLS_MAX` /
//! `HAMT_REMOVE_COLLISION_CELLS_MAX`). A one-cell collision node is refused as
//! well.
//!
//! An over-estimate costs address space only; an under-estimate corrupts. The
//! size-only bound is loose in exactly one place a *number* cannot express: a
//! **collision node**'s cell count is bounded by the map's size, not by the path
//! depth, so the exact figure must be read from the map itself —
//! [`map_put_cow_words_required`] / [`map_remove_cow_words_required`] descend the
//! path read-only (hashing and nibbles only, no `atoms`) and answer the exact word
//! count, including `c + 1` words for an insert into a `c`-cell collision node and
//! `c - 1` for a delete from one. They share this plan, so they refuse exactly the
//! maps the writers refuse — a `required` figure can never lead a caller into a
//! reservation the write then exceeds.
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

mod node;
mod put;
mod remove;
mod words;

use crate::atom::AtomTable;
use crate::boxed::{arityval, boxed_val, is_arity_value, is_boxed_term};
use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::list::make_list;

use put::{exec_put, plan_put};
use remove::{exec_remove, plan_remove};

pub use words::{
    map_put_cow_words_capacity, map_put_cow_words_required, map_remove_cow_words_capacity,
    map_remove_cow_words_required,
};

// ---------------------------------------------------------------------------
// Flat maps: the pair count and its authority
// ---------------------------------------------------------------------------

/// The pair count of a flat map, taken from the **authority** and cross-checked
/// against the head word's `size` field.
///
/// A flat map is the word block `[hdr][n][keys tuple][v0 … v(n-1)]`
/// ([`write_flatmap`](crate::thing::write_flatmap)), so `n` appears twice: as the
/// head's raw `size` word and as the keys tuple's arity. Both planners must size
/// their reservation from the same figure the writer's extent comes from, so this
/// reads the arity and refuses a map where the two disagree — a typed
/// [`TermError::BadMap`], never a silent preference for one of them (preferring the
/// head word could under-reserve the merge, preferring the tuple could read past the
/// value sequence).
///
/// Every map this crate builds satisfies the equality by construction, so this is
/// unreachable for a well-formed map.
///
/// # Safety
///
/// `p` must address a live flat map's head word; word 2 is then its keys tuple.
pub(super) unsafe fn flat_pair_count(p: *const Eterm) -> TermResult<usize> {
    // SAFETY: `p` is a flat map's head word: word 1 is its raw pair count and word 2
    // its keys tuple term.
    let (size, keys) = unsafe { ((*p.add(1)).raw(), *p.add(2)) };
    if !is_boxed_term(keys) {
        return Err(TermError::BadMap);
    }
    // SAFETY: `keys` is a live boxed term, so this is its header word.
    let keys_header = unsafe { *boxed_val(keys) };
    if !is_arity_value(keys_header) || arityval(keys_header) != size {
        return Err(TermError::BadMap);
    }
    Ok(size)
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
// Public API
// ---------------------------------------------------------------------------

/// `maps:put(Key, Value, Map)` into a caller-reserved heap region, copying **only
/// the nodes on `Key`'s hash path** (C `erts_maps_put`, `erl_map.c:2105-2235`).
///
/// Returns the new map, or the input map word unchanged when the stored value word
/// is already `Value` (nothing is written then). `hp` must address at least
/// [`map_put_cow_words_capacity`]`(`[`map_size`](crate::map::map_size)`(map))` words — or, when `key`'s
/// path crosses a hash collision node, the exact
/// [`map_put_cow_words_required`]`(map, key)` — of a heap that outlives `map`.
///
/// A `map` this writer refuses (a collision node wider than the size-only
/// reservation covers, a collision node with fewer than two cells, or a flat map
/// whose head word disagrees with its keys tuple) is the typed
/// [`BadMap`](crate::error::TermError::BadMap) and writes nothing.
///
/// # Safety
///
/// * `map` must be a **well-formed map produced by this crate's builders**
///   ([`write_map`](crate::map::write_map), [`write_flatmap`](crate::thing::write_flatmap) or
///   this writer): a malformed interior is a typed `BadMap`, never a wrong result,
///   but the walk cannot validate the *extent* of a forged node — a word that is
///   not part of the map must not be reachable from it.
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
/// [`map_remove_cow_words_capacity`]`(`[`map_size`](crate::map::map_size)`(map))` words — or, when
/// `key`'s path crosses a hash collision node, the exact
/// [`map_remove_cow_words_required`]`(map, key)` — of a heap that outlives `map`.
/// A map this writer refuses answers [`BadMap`](crate::error::TermError::BadMap)
/// and writes nothing, exactly like [`map_put_cow`].
///
/// # Safety
///
/// The same preconditions [`map_put_cow`] states — including that `map` is a
/// well-formed map produced by this crate's builders.
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
