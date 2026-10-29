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

//! The map writer/reader: the flat-map builder [`make_flatmap`] and the general
//! [`make_map`] (flat ≤ 32 keys, else a hashmap), plus the frozen-layout readers
//! [`map_entries`]/[`is_hashmap`] that walk both representations.
//!
//! The classic [`make_flatmap`]/[`make_map`] take their already-read pairs and
//! allocate through the plain [`ExecutionContext::alloc_words`]; their `*_rooted`
//! twins reserve the block **before** a reader closure supplies the pairs, so a
//! moving collector may relocate the operands in between ([`super::slack`]).
//!
//! [`map_cow_rooted`] is the **incremental** (path-copying) update seam: instead of
//! rebuilding a map from a pair list it reserves one region of fixed [`CowMapRegion`]
//! slots and lets a closure chain [`beam_term::map::map_put_cow`] calls into them, so
//! one `put_map_assoc`/`put_map_exact` over a hash map costs Θ(log n) words rather
//! than Θ(n).
use beam_common::{BeamError, Result};
use beam_loader::Module;
use beam_term::{thing, AtomTable, Eterm};

use super::{reserve, Slack, HAMT_ARRAY_SLOTS, HAMT_HEAD_ITEM_OFFSET, HAMT_NODE_ITEM_OFFSET, MAP_SMALL_MAP_LIMIT};
use crate::context::ExecutionContext;
use crate::machine::Machine;

/// Builds a small *flat* map term from `pairs` (insertion order preserved).
///
/// The `[arityval(n), k0 … k(n-1), head, n, keys, v0 … v(n-1)]` layout is written
/// by [`beam_term::thing::write_flatmap`]. Duplicate keys are not detected here
/// (that needs term comparison).
///
/// This is the *flat-layout-only* builder: more than `MAP_SMALL_MAP_LIMIT` (32)
/// pairs would need a **hashmap**, so it is rejected with
/// [`crate::error::unsupported`] rather than writing a wrong term. Use [`make_map`]
/// for the general builder, which selects the layout (flat ≤ 32 pairs, hashmap
/// above) as the oracle does.
pub fn make_flatmap(ctx: &mut dyn ExecutionContext, pairs: &[(Eterm, Eterm)]) -> Result<Eterm> {
    let n = pairs.len();
    if n > MAP_SMALL_MAP_LIMIT {
        return Err(crate::error::unsupported("map > 32 pairs (use make_map)"));
    }
    let hp = ctx.alloc_words(thing::flatmap_size_words(n))?;
    // SAFETY: `alloc_words` returned `flatmap_size_words(n)` writable words and
    // `n <= MAP_SMALL_MAP_LIMIT`, the preconditions `write_flatmap` documents.
    Ok(unsafe { thing::write_flatmap(hp, pairs) })
}

/// Builds a map term from `pairs`, choosing the frozen layout by size — the
/// general map builder (`put_map_assoc` / `put_map_exact`).
///
/// Delegates to [`beam_term::map::write_map`], which reproduces
/// `erts_map_from_ks_and_vs` / `erts_maps_put`: duplicate keys are collapsed
/// **last value wins**, a map of at most `MAP_SMALL_MAP_LIMIT` (32) distinct keys
/// is written as a **flat** map (keys in `erts_cmp_flatmap_keys` order) and a
/// larger one as a **hash map** (HAMT), so a map grown past the flat-map limit by
/// compiled code becomes a correct hashmap, exactly as the oracle builds it. The
/// pair order of `pairs` is irrelevant.
///
/// Storage is reserved with [`beam_term::map::map_words_capacity`]`(pairs.len())`
/// — the exact flat-map size and a sound upper bound for a hashmap. Because
/// `write_map` deduplicates its input, reserving for the *input* length is always
/// sufficient for the produced map.
pub fn make_map(
    ctx: &mut dyn ExecutionContext,
    pairs: &[(Eterm, Eterm)],
    atoms: &AtomTable,
) -> Result<Eterm> {
    let hp = ctx.alloc_words(beam_term::map::map_words_capacity(pairs.len()))?;
    // SAFETY: `alloc_words` returned `map_words_capacity(pairs.len())`
    // exclusively-owned, writable words. `write_map` writes every word of the
    // produced map and never more than that (deduplication can only shrink the
    // map), the precondition its own `# Safety` section documents. The returned
    // term borrows the arena storage the context keeps live.
    let map = unsafe { beam_term::map::write_map(hp, pairs, atoms) }?;
    Ok(map)
}

/// The **rooted** twin of [`make_flatmap`]: builds a small *flat* map term from the
/// `capacity_pairs` pairs the `read` closure supplies (insertion order preserved).
///
/// The `[arityval(n), k0 … k(n-1), head, n, keys, v0 … v(n-1)]` layout is written
/// by [`beam_term::thing::write_flatmap`]. The block is reserved **rooted** for
/// `flatmap_size_words(capacity_pairs) + extra` words and the closure is then invoked
/// (so the pairs are read from the machine's post-collection state); it may hold at
/// most `capacity_pairs` pairs. Duplicate keys are not detected here (that needs term
/// comparison).
///
/// This is the *flat-layout-only* builder: more than `MAP_SMALL_MAP_LIMIT` (32)
/// pairs would need a **hashmap**, so it is rejected with
/// [`crate::error::unsupported`] rather than writing a wrong term. Use
/// [`make_map_rooted`] for the general builder, which selects the layout (flat ≤ 32
/// pairs, hashmap above) as the oracle does.
pub fn make_flatmap_rooted<R>(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    live: u32,
    capacity_pairs: usize,
    extra: usize,
    read: R,
) -> Result<Eterm>
where
    R: FnOnce(&Machine, &mut Slack) -> Result<Vec<(Eterm, Eterm)>>,
{
    let n = capacity_pairs;
    if n > MAP_SMALL_MAP_LIMIT {
        return Err(crate::error::unsupported("map > 32 pairs (use make_map)"));
    }
    let hp = reserve(machine, ctx, thing::flatmap_size_words(n) + extra, live)?;
    // SAFETY: the reserve returned `flatmap_size_words(n) + extra` exclusively-owned,
    // writable words; the `extra`-word tail after the map words is the slack.
    let mut slack = unsafe { Slack::new(hp.add(thing::flatmap_size_words(n)), extra) };
    let pairs = read(machine, &mut slack)?;
    if pairs.len() > n {
        return Err(crate::error::unsupported(
            "make_flatmap: pair count exceeds the reserved capacity",
        ));
    }
    // SAFETY: the reserve returned `flatmap_size_words(n)` writable map words with
    // `n <= MAP_SMALL_MAP_LIMIT` and the closure returned `<= n` pairs, the
    // preconditions `write_flatmap` documents.
    Ok(unsafe { thing::write_flatmap(hp, &pairs) })
}

/// The **rooted** twin of [`make_map`]: builds a map term from the `capacity_pairs`
/// pairs the `read` closure supplies, choosing the frozen layout by size — the
/// general map builder (`put_map_assoc` / `put_map_exact`).
///
/// Delegates to [`beam_term::map::write_map`], which reproduces
/// `erts_map_from_ks_and_vs` / `erts_maps_put`: duplicate keys are collapsed
/// **last value wins**, a map of at most `MAP_SMALL_MAP_LIMIT` (32) distinct keys
/// is written as a **flat** map (keys in `erts_cmp_flatmap_keys` order) and a
/// larger one as a **hash map** (HAMT), so a map grown past the flat-map limit by
/// compiled code becomes a correct hashmap, exactly as the oracle builds it. The
/// pair order returned by `read` is irrelevant.
///
/// The block is reserved **rooted** for
/// [`beam_term::map::map_words_capacity`]`(capacity_pairs) + extra` words **before**
/// the closure runs, so the pairs are read from the machine's post-collection state
/// and a non-small integer operand is materialized into the spare tail (never a
/// second collecting allocation that would strand the reserved block). `write_map`
/// deduplicates its input and writes at most `map_words_capacity(pairs.len())`
/// words, which the reservation covers because the closure must not exceed
/// `capacity_pairs` pairs.
///
/// The atom **namespace** `write_map` orders/hashes keys with — and the one the
/// `read` closure must use for its own key equality — is taken from the host through
/// [`ExecutionContext::atoms`](crate::context::ExecutionContext::atoms) **after** the
/// reserve (the last `&mut ctx` use), so it is the host's live shared namespace and
/// not the running module's load-time snapshot. The closure receives it, which is the
/// only way for a key-equality comparison to see the same namespace (a `&AtomTable`
/// borrowed from `ctx` cannot be held across the reserve's `&mut ctx`).
pub fn make_map_rooted<R>(
    machine: &mut Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    live: u32,
    capacity_pairs: usize,
    extra: usize,
    read: R,
) -> Result<Eterm>
where
    R: FnOnce(&Machine, &mut Slack, &AtomTable) -> Result<Vec<(Eterm, Eterm)>>,
{
    let words = beam_term::map::map_words_capacity(capacity_pairs);
    let hp = reserve(machine, ctx, words + extra, live)?;
    // SAFETY: the reserve returned `map_words_capacity(capacity_pairs) + extra`
    // exclusively-owned, writable words; the `extra`-word tail after the map words is
    // the slack.
    let mut slack = unsafe { Slack::new(hp.add(words), extra) };
    // Taken after the reserve (its last `&mut ctx` use) and held only across the
    // closure call and `write_map`, neither of which touches `ctx`.
    let atoms = ctx.atoms(module);
    let pairs = read(machine, &mut slack, atoms)?;
    if pairs.len() > capacity_pairs {
        return Err(crate::error::unsupported(
            "make_map: pair count exceeds the reserved capacity",
        ));
    }
    // SAFETY: the reserve returned `map_words_capacity(capacity_pairs)` writable map
    // words. `write_map` writes every word of the produced map and never more than
    // `map_words_capacity(pairs.len())` (deduplication can only shrink the map), which
    // is `<= map_words_capacity(capacity_pairs)`, the precondition its own `# Safety`
    // section documents. The returned term borrows the arena storage the context keeps
    // live.
    let map = unsafe { beam_term::map::write_map(hp, &pairs, atoms) }?;
    Ok(map)
}

// ---------------------------------------------------------------------------
// The path-copying (COW) update seam — `beam_term::map::map_put_cow`
// ---------------------------------------------------------------------------

/// The words a caller must reserve for **one** path-copying put over a map of
/// `size` pairs — the size-only capacity bound of
/// [`beam_term::map::map_put_cow`].
///
/// Forwarded verbatim from `beam_term`, which owns the figure and documents its
/// derivation and its soundness domain: `293` for a hash-map source of more than
/// [`MAP_SMALL_MAP_LIMIT`] keys, `map_words_capacity(33)` = `711` at exactly the
/// limit, and `flatmap_size_words(size + 1)` below it. It is a **bound**, not the
/// figure a particular put writes (`map_put_cow_words_required` is the exact one,
/// and it must not be used to size a real write — it plans against an empty
/// [`AtomTable`]).
///
/// A safe wrapper so the instruction families never name the raw-heap writer
/// directly: this module is the crate's single raw-heap home.
pub fn map_put_cow_capacity(size: usize) -> usize {
    beam_term::map::map_put_cow_words_capacity(size)
}

/// A caller-reserved region of `count` fixed-`stride` **slots**, one per
/// path-copying map put — the staging area an incremental map update writes its
/// copied nodes into.
///
/// The region is carved out of one rooted reservation by [`map_cow_rooted`], so a
/// moving collector may relocate the *operands* between the reserve and the first
/// put but never the region itself (its pointer is not a root, which is exactly why
/// nothing may allocate from the BEAM heap once the writes start).
///
/// Every slot is disjoint from the source map's storage and from every other slot;
/// a slot a put declines to use (the writer's zero-write no-change arm) is simply
/// left untouched.
#[derive(Debug)]
pub struct CowMapRegion {
    /// The first word of the reservation (slot 0).
    base: *mut Eterm,
    /// The words reserved per slot — one [`map_put_cow_capacity`] figure.
    stride: usize,
    /// How many slots the reservation holds.
    count: usize,
}

impl CowMapRegion {
    /// The number of put slots the reservation holds.
    pub fn count(&self) -> usize {
        self.count
    }

    /// The words reserved per slot (one [`map_put_cow_capacity`] figure).
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// `beam_term::map::map_put_cow` into slot `index`: the map `map` with `key`
    /// associated to `value`, copying only the nodes on `key`'s hash path.
    ///
    /// Answers the **input map word unchanged** where the stored value word already
    /// equals `value` (nothing is written then), so a chain of such puts answers the
    /// source map itself — the observable [`crate::families::maps`]' no-change
    /// contract relies on. An absent key inserts. A malformed map is the typed
    /// [`BeamError::BadArg`], the same error the [`map_entries`] walk produces.
    ///
    /// An `index` at or past [`count`](Self::count) is a typed
    /// [`crate::error::unsupported`] — a caller sizing bug, never a write past the
    /// reservation.
    pub fn put(
        &self,
        index: usize,
        map: Eterm,
        key: Eterm,
        value: Eterm,
        atoms: &AtomTable,
    ) -> Result<Eterm> {
        if index >= self.count {
            return Err(crate::error::unsupported(
                "map_put_cow: slot index beyond the reserved region",
            ));
        }
        // SAFETY: `index < count`, so `base + index * stride` addresses the
        // `stride`-word slot the caller reserved for this put, inside a region that
        // is exclusively owned (the source map never lives in it), is at least
        // `count * stride` words long, and outlives the returned map — the three
        // preconditions `map_put_cow` states. The region's words are never handed
        // out twice, and no second reader of them exists (`&self` only).
        Ok(unsafe {
            beam_term::map::map_put_cow(self.base.add(index * self.stride), map, key, value, atoms)
        }?)
    }
}

/// The **rooted** entry point of an incremental map update: ONE rooted reservation
/// of `words + extra` words, a [`CowMapRegion`] of `words / per` slots over it, and
/// then the `write` closure, which chains the path-copying puts and answers the
/// resulting map.
///
/// # Why one reservation, and why nothing may allocate after it
///
/// The reserve is the single allocating — and therefore possibly **collecting** —
/// door ([`crate::terms::slack`]): a moving collector relocates every term
/// reachable from the machine's root set and rewrites the roots in place, so a raw
/// `Eterm` read *before* the reserve is stale afterwards, and the reserved block's
/// own pointer is not a root. The closure therefore re-reads every operand from the
/// machine's post-collection state, and neither it nor the writer may allocate from
/// the BEAM heap: a second collecting allocation would strand the nodes already
/// written into the region (nothing points at them yet).
///
/// The `extra`-word tail after the region is the [`Slack`] a non-small
/// `Operand::Integer` operand is materialized into (see
/// [`crate::operand::operand_slack_words`]), so the whole build still costs one
/// allocation.
///
/// `words` is the total the caller reserved and must be a whole number of `per`
/// (`per` is [`map_put_cow_capacity`]) word slots — the caller derives it as
/// `per * pairs.len()`; a non-multiple (or a zero `per`) is a typed
/// [`crate::error::unsupported`] sizing error, never a short reservation.
///
/// The atom **namespace** the writer hashes and compares keys with is taken from
/// the host ([`ExecutionContext::atoms`](crate::context::ExecutionContext::atoms))
/// **after** the reserve — the last `&mut ctx` use — and handed to the closure,
/// exactly as [`make_map_rooted`] does.
pub fn map_cow_rooted<R>(
    machine: &mut Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    live: u32,
    words: usize,
    per: usize,
    extra: usize,
    write: R,
) -> Result<Eterm>
where
    R: FnOnce(&Machine, &CowMapRegion, &mut Slack, &AtomTable) -> Result<Eterm>,
{
    if per == 0 || words % per != 0 {
        return Err(crate::error::unsupported(
            "map_cow: the reserved words are not a whole number of put slots",
        ));
    }
    let count = words / per;
    let total = words
        .checked_add(extra)
        .ok_or_else(|| crate::error::unsupported("map_cow: reservation overflows a word count"))?;
    let hp = reserve(machine, ctx, total, live)?;
    // SAFETY: the reserve returned `words + extra` exclusively-owned, writable
    // words; the `extra`-word tail after the region is the slack.
    let mut slack = unsafe { Slack::new(hp.add(words), extra) };
    // Taken after the reserve (its last `&mut ctx` use) and held only across the
    // closure call and the puts it performs, neither of which touches `ctx`.
    let atoms = ctx.atoms(module);
    let region = CowMapRegion {
        base: hp,
        stride: per,
        count,
    };
    write(machine, &region, &mut slack, atoms)
}

/// Whether `t` is a map using the frozen **hashmap** (HAMT) layout.
///
/// A safe wrapper over [`beam_term::is_hashmap`] (a classifier that dereferences
/// the term's header word), so callers that may not use `unsafe` — e.g. tests under
/// the crate-wide `#![deny(unsafe_code)]` — can classify a map's representation.
/// Returns `false` for a flat map and for any non-map term.
pub fn is_hashmap(t: Eterm) -> bool {
    // SAFETY: the classifier handles an immediate term first (`is_boxed` rejects it
    // before any dereference); a boxed `t` is a live term from the context heap or a
    // literal area, never unvalidated raw memory.
    unsafe { beam_term::is_hashmap(t) }
}

/// Splits a map term into its `(key, value)` entries, in storage order.
///
/// Both frozen map layouts are read: a *flat* map (at most `MAP_SMALL_MAP_LIMIT`
/// pairs) through [`beam_term::thing::flatmap_parts`], and a **hashmap** (HAMT) by
/// walking its node tree ([`hashmap_entries`]). A term that is not a map, or a map
/// whose node words are not a well-shaped HAMT node, yields [`BeamError::BadArg`] —
/// never a wrong pair set.
///
/// The order is the map's *stored* order (flat-map insertion order; HAMT node
/// order). ERTS stores a flat map's keys in `erts_cmp_flatmap_keys` order, which
/// the module-local atom model cannot reproduce, so callers must treat the order as
/// implementation-defined and match keys with [`beam_term::cmp`].
pub fn map_entries(t: Eterm) -> Result<Vec<(Eterm, Eterm)>> {
    // SAFETY: `is_map`/`is_flatmap` check `is_boxed` before dereferencing.
    if !unsafe { beam_term::is_map(t) } {
        return Err(BeamError::BadArg);
    }
    if unsafe { beam_term::is_flatmap(t) } {
        return flatmap_entries(t);
    }
    // SAFETY: `is_map` established that `t` is a live boxed map; a non-flat map is
    // a HAMT whose root is a live head node.
    unsafe { hashmap_entries(t) }
}

/// Splits a *flat* map term into its `(key, value)` entries (stored order).
///
/// The `[head, size, keys, v0 … v(n-1)]` layout written by [`make_flatmap`]; the
/// root points at the `[head, size, keys]` prefix, so word 1 is the raw pair count.
fn flatmap_entries(t: Eterm) -> Result<Vec<(Eterm, Eterm)>> {
    // A well-formed flatmap never exceeds `MAP_SMALL_MAP_LIMIT` pairs.
    // SAFETY: `is_flatmap` established that `t` is a live flatmap, so word 1 is a
    // live, initialised term word.
    let n = unsafe { (*beam_term::boxed_val(t).add(1)).raw() };
    if n > MAP_SMALL_MAP_LIMIT {
        return Err(BeamError::BadArg);
    }
    // SAFETY: the guards established that `t` is a live `n`-pair flat map.
    let (keys, values) = unsafe { thing::flatmap_parts(t) };
    Ok(keys.iter().copied().zip(values.iter().copied()).collect())
}

/// Walks a **hashmap** (HAMT) tree from its head node, collecting every leaf
/// `(key, value)` pair.
///
/// This is the enumeration of C's `hashmap_iterator_next`
/// (`erl_map.c:2346-2381`) and needs **no hash function**: the walk descends every
/// occupied slot of every node and leaves are the `[K | V]` cons cells, so key
/// matching stays with the caller's [`beam_term::cmp`] scan. (`erts_map_hash` is an
/// internal detail of how a *writer* chooses slots, not part of the frozen term
/// layout, and `beam_term::term_hash` is deliberately *not* it.)
///
/// The walk uses an explicit work stack rather than recursion, so a deeply nested
/// tree (at most `HAMT_MAX_LEVEL` = 16 levels, `erl_map.h:220`) cannot overflow the
/// native stack.
///
/// # Safety
///
/// `root` must be a live boxed HAMT head node (a live map term that is not a
/// flatmap).
unsafe fn hashmap_entries(root: Eterm) -> Result<Vec<(Eterm, Eterm)>> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        // SAFETY: `root` is a live head node (caller contract) and every value
        // pushed onto `stack` is a live sub-node read from a live node, so `node`
        // is a live boxed HAMT node.
        unsafe { collect_hashmap_node(node, &mut stack, &mut out)? };
    }
    Ok(out)
}

/// Reads one HAMT node: appends its leaf pairs and pushes its sub-nodes onto
/// `stack`.
///
/// # Safety
///
/// `node` must be a live boxed HAMT node of a well-formed tree — its header word
/// and every one of its item words must be live and in bounds.
unsafe fn collect_hashmap_node(
    node: Eterm,
    stack: &mut Vec<Eterm>,
    out: &mut Vec<(Eterm, Eterm)>,
) -> Result<()> {
    let base = beam_term::boxed_val(node);
    // SAFETY: `node` is a live boxed node, so its header word is live.
    let (first_item, count) = hashmap_node_shape(unsafe { *base })?;
    for i in 0..count {
        // SAFETY: a node of `count` items owns `first_item + count` words past its
        // header, so every item word is live and in bounds.
        let item = unsafe { *base.add(first_item + i) };
        // `THE_NON_VALUE` (the array-head "unoccupied slot" marker, `erl_map.h:75`)
        // is neither a leaf nor a sub-node, and `is_boxed` would *accept* it (its low
        // bit is clear), so it must be rejected before any dereference. ERTS fills
        // all 16 array-head slots before using an array head, but a hand-built or
        // corrupt term need not.
        if beam_term::is_non_value(item) {
            return Err(BeamError::BadArg);
        }
        if beam_term::is_list(item) {
            // A leaf: the `[K | V]` cons cell (`erl_map.c:2370-2372`).
            // SAFETY: `is_list` established that `item` is a live cons cell, so both
            // of its words are live.
            out.push(unsafe { (beam_term::car(item), beam_term::cdr(item)) });
        } else if beam_term::is_boxed(item) {
            // A sub-node: a bitmap, array or collision node.
            stack.push(item);
        } else {
            // An immediate in an item slot is not a HAMT node shape at all.
            return Err(BeamError::BadArg);
        }
    }
    Ok(())
}

/// The `(first-item word offset, item count)` of a HAMT node, from its header
/// word — the C `hashmap_node_size(hdr, &ptr)` together with its `ptr` bump
/// (`erl_map.c:2312-2335`).
///
/// The node layouts (`erl_map.h:142-205`):
/// * head array — `[MAP_HEADER_HAMT_HEAD_ARRAY, size, item × 16]`;
/// * head bitmap — `[header, size, item × popcount]` (`HAMT_HEAD_BITMAP_SZ(n) = 2 + n`);
/// * node bitmap — `[header, item × popcount]` (`HAMT_NODE_BITMAP_SZ(n) = 1 + n`);
/// * collision node — `[make_arityval(n), item × n]`.
fn hashmap_node_shape(hdr: Eterm) -> Result<(usize, usize)> {
    if beam_term::is_map_header(hdr) {
        let count = |hdr: Eterm| beam_term::map_header_val(hdr) as usize;
        match beam_term::map_header_type(hdr) as usize {
            beam_term::MAP_HEADER_TAG_HAMT_HEAD_ARRAY => {
                Ok((HAMT_HEAD_ITEM_OFFSET, HAMT_ARRAY_SLOTS))
            }
            beam_term::MAP_HEADER_TAG_HAMT_HEAD_BITMAP => {
                Ok((HAMT_HEAD_ITEM_OFFSET, count(hdr).count_ones() as usize))
            }
            beam_term::MAP_HEADER_TAG_HAMT_NODE_BITMAP => {
                Ok((HAMT_NODE_ITEM_OFFSET, count(hdr).count_ones() as usize))
            }
            // A flatmap head is read by [`flatmap_entries`], never as a HAMT node.
            _ => Err(BeamError::BadArg),
        }
    } else if beam_term::is_arity_value(hdr) {
        // A collision node: an untagged arityval header plus the leaves
        // (`MAP_HEADER_HAMT_COLLISION_NODE`, `erl_map.h:197`).
        Ok((HAMT_NODE_ITEM_OFFSET, beam_term::arityval(hdr)))
    } else {
        Err(BeamError::BadArg)
    }
}
