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

//! The **rooted-allocation regression suite** (Handoff #11): the three proofs that
//! every `terms::*_rooted` builder routes its storage through the **rooted** seam
//! ([`Machine::alloc_rooted`](crate::machine::Machine::alloc_rooted) →
//! [`ExecutionContext::alloc_words_rooted`](crate::context::ExecutionContext::alloc_words_rooted))
//! and stays correct when that seam runs a **moving** collector underneath it.
//!
//! The reroute this suite guards replaced the builders' unrooted
//! [`ExecutionContext::alloc_words`](crate::context::ExecutionContext::alloc_words)
//! calls with the rooted seam, whose whole point is the frozen
//! **reserve → read → write** discipline documented in [`super::slack`]: because a
//! moving collector relocates every term reachable from the [`RootSet`] and rewrites
//! the root slices in place, an `Eterm` the engine is holding in a Rust local across
//! a collecting allocation is stale — so a builder must reserve its whole block
//! *first* and read its operands *afterwards*, from the machine's post-collection
//! registers.
//!
//! Three deliverables, all in-module so they need no production hooks:
//!
//! 1. **[`CountingContext`]** — a counting, non-collecting context. It reaches the
//!    same arena as [`MinimalContext`] through **both** seams and counts each, so
//!    [`the_builders_route_every_allocation_through_the_rooted_seam`] can assert the
//!    builders fire the rooted hook (and *never* the unrooted one). That count was
//!    exactly `0` before the reroute.
//! 2. **[`MovingContext`]** — a small **semi-space copying collector** over two
//!    fixed regions. Every [`alloc_words_rooted`](crate::context::ExecutionContext::alloc_words_rooted)
//!    first relocates every block reachable from the roots into the *other* region,
//!    rewrites the [`RootSet`] slices and the copied blocks' internal pointers, and
//!    poisons the vacated region. It is the correctness witness: a builder that read
//!    its operands before the reserve would write poisned, pre-move addresses into
//!    its block, and every assertion below would fail.
//! 3. **Peak-word characterization** — the same allocation-heavy loop measured
//!    against the never-freeing [`MinimalContext`] arena (peak grows linearly in the
//!    loop length) and against the collecting context (peak *live* words bounded).
//!
//! This is a **raw-heap** module: it walks and rewrites untyped term words at raw
//! region addresses, exactly like the rest of the `terms` tree, so it carries the
//! module-level `#![allow(unsafe_code)]` and a `// SAFETY:` rationale on every
//! `unsafe` block. Term *layout* is still only ever read through `beam_term`
//! accessors (`is_list`/`is_boxed_term`/`boxed_val`/`big_parts`).
#![allow(unsafe_code)]

use beam_common::Result;
use beam_term::{big_parts, boxed_val, is_boxed_term, is_list, make_atom, make_small, Atom};
use beam_term::{AtomTable, Eterm, TAG_PTR_MASK};

use super::{
    float_value, list_elements, make_float_rooted, make_list_rooted, make_map_rooted,
    make_tuple_rooted, reserve_slack, Sink, tuple_arity, tuple_element,
};
use crate::context::{ExecutionContext, MinimalContext, RootSet};
use crate::machine::Machine;

/// The number of bytes in one term word.
const WORD_BYTES: usize = core::mem::size_of::<Eterm>();

/// Region capacity in words (64 Ki words = 512 KiB per region, two per context).
///
/// Generous: the collecting tests keep only a handful of words live, so the
/// semi-space collector never grows and the two region addresses stay fixed for the
/// whole run — which is what makes "the new address differs from the old one" a
/// structural property rather than an allocator accident.
const REGION_WORDS: usize = 1 << 16;

/// The word written over a region's contents once its live blocks have been copied
/// out — "poison", so a stale (pre-move) pointer reads garbage instead of the data
/// it used to hold.
///
/// Its low bits make it neither a valid boxed/list pointer nor a valid float header,
/// so every term-type check rejects it: the guard that turns a pre-reserve read into
/// a loud test failure rather than a lucky pass.
const POISON: Eterm = Eterm::from_raw(0xDEAD_BEEF_DEAD_BEEF);

/// One block handed out by [`MovingContext`], as a word offset into its region and
/// a word count.
#[derive(Clone, Copy, Debug)]
struct Block {
    offset: usize,
    size: usize,
}

// ---------------------------------------------------------------------------
// The counting (non-collecting) mock
// ---------------------------------------------------------------------------

/// A counting, **non-collecting** context.
///
/// It hands words out of the [`MinimalContext`] arena (which never frees, so its
/// cumulative word count *is* its peak) and counts the words requested through each
/// of the two allocation seams. Keeping the two seams — and their counters —
/// distinct is what makes `unrooted_calls == 0` a regression proof rather than a
/// tautology ([`alloc_words_rooted`](ExecutionContext::alloc_words_rooted)
/// deliberately does **not** forward through
/// [`alloc_words`](ExecutionContext::alloc_words)).
struct CountingContext {
    arena: MinimalContext,
    rooted_calls: usize,
    unrooted_calls: usize,
    words: usize,
}

impl CountingContext {
    fn new() -> CountingContext {
        CountingContext {
            arena: MinimalContext::new(),
            rooted_calls: 0,
            unrooted_calls: 0,
            words: 0,
        }
    }
}

impl ExecutionContext for CountingContext {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        self.arena.call_bif(module, function, arity, args)
    }

    fn ensure_heap(
        &mut self,
        _heap_need: u32,
        _live: u32,
        _x_regs: &mut [Eterm],
        _stack: &mut [Eterm],
    ) -> Result<()> {
        // The arena is unbounded and nothing may collect here, so the words handed
        // out are exactly the words still resident (the "peak" of the BEFORE run).
        Ok(())
    }

    fn heap_available(&self) -> usize {
        usize::MAX
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.unrooted_calls += 1;
        self.words += n;
        self.arena.alloc_words(n)
    }

    fn alloc_words_rooted(&mut self, n: usize, _roots: RootSet<'_>) -> Result<*mut Eterm> {
        self.rooted_calls += 1;
        self.words += n;
        // Deliberately NOT `self.alloc_words(n)`: the unrooted counter must record
        // only genuine unrooted calls by the engine.
        self.arena.alloc_words(n)
    }
}

// ---------------------------------------------------------------------------
// The moving (semi-space copying) mock
// ---------------------------------------------------------------------------

/// The pointer address a term word refers to, if it is a list/boxed **pointer**.
///
/// `None` for every immediate (a small int, an atom, `NIL`, a pid, …) and for a
/// bare header word. The predicates are `beam_term`'s own tag tests; the caller
/// still has to confirm the address lies inside a region (see [`block_at`]), which
/// is what keeps a word that merely *looks* like a pointer from being treated as one.
fn pointer_addr(w: Eterm) -> Option<usize> {
    if is_list(w) || is_boxed_term(w) {
        Some(boxed_val(w) as usize)
    } else {
        None
    }
}

/// The index of the block of `blocks` (laid out at `base`) that the term word `w`
/// points into.
fn block_at(blocks: &[Block], base: usize, w: Eterm) -> Option<usize> {
    let addr = pointer_addr(w)?;
    blocks.iter().position(|b| {
        let start = base + b.offset * WORD_BYTES;
        addr >= start && addr < start + b.size * WORD_BYTES
    })
}

/// Rebases an old absolute address onto its post-collection address using the
/// relocation table of the collection in flight — including an address that points
/// into the *middle* of a block (a node carved inside a bigger reservation).
fn relocate(moved: &[(Block, Block)], from_base: usize, to_base: usize, addr: usize) -> Option<usize> {
    for (old, new) in moved {
        let start = from_base + old.offset * WORD_BYTES;
        if addr >= start && addr < start + old.size * WORD_BYTES {
            return Some(to_base + new.offset * WORD_BYTES + (addr - start));
        }
    }
    None
}

/// [`relocate`] for a whole term word, preserving its pointer tag (list/boxed and
/// the literal-area bit) so a relocated cons cell stays a cons cell.
fn relocate_term(
    moved: &[(Block, Block)],
    from_base: usize,
    to_base: usize,
    w: Eterm,
) -> Option<Eterm> {
    let addr = pointer_addr(w)?;
    let new = relocate(moved, from_base, to_base, addr)?;
    Some(Eterm::from_raw(new | (w.raw() & TAG_PTR_MASK)))
}

/// A small **semi-space copying collector**: two fixed regions, a bump cursor in the
/// active one, and a collection on every rooted allocation.
///
/// # What a collection does
///
/// 1. **Mark** — every block a root ([`RootSet`]'s `x`/`stack`/`fr`) points at.
/// 2. **Trace** — transitively, every block a *live* block's words point at
///    (conservative: any list/boxed word whose address lands in a known block).
/// 3. **Copy** — the live blocks, compacted, into the *other* region, building the
///    old→new relocation table.
/// 4. **Rewrite** — the root slices **in place** and the pointers *inside* the copied
///    blocks (a whole block moves as a unit, so its internal absolute addresses must
///    be rebased too).
/// 5. **Poison** — the vacated region, so a stale pointer reads garbage.
///
/// Garbage (a block no root reaches) is simply *not* copied: the region swap reclaims
/// it, which is what bounds the live set in
/// [`rooted_collection_bounds_peak_live_words_while_the_unrooted_arena_grows`].
///
/// [`alloc_words`](ExecutionContext::alloc_words) is deliberately **not** overridden:
/// the trait default is a typed `unsupported`, so a builder that fell back to the
/// unrooted seam would fail loudly instead of silently skipping the collector.
///
/// It is `pub(crate)` (with its constructor and the `collections`/`moved_any`
/// witnesses) so the **family**-level regression suites (`families::records`,
/// `families::bitstrings`) can drive their own instruction paths under the same
/// moving collector — the shared witness for the whole rooted-allocation reroute
/// (Handoff #11). It stays test-only: the module that owns it is `#[cfg(test)]`.
pub(crate) struct MovingContext {
    /// The two region buffers — never read directly (they only keep the words alive;
    /// every access goes through the raw `base` pointers).
    _regions: [Box<[Eterm]>; 2],
    /// The stable raw base of each region (the boxed buffers never move).
    base: [*mut Eterm; 2],
    /// The region currently handed out of.
    current: usize,
    /// Words used in `regions[current]`.
    used: usize,
    /// The blocks handed out in `regions[current]`, oldest first (contiguous, since a
    /// collection compacts and every allocation appends).
    blocks: Vec<Block>,
    /// How many collections have run.
    pub(crate) collections: usize,
    /// The high-water mark of **live** words: the words retained by a collection.
    peak_live_words: usize,
    /// Whether any collection has actually relocated a block (i.e. changed an
    /// address) — the "the collector really moves something" witness.
    pub(crate) moved_any: bool,
}

impl MovingContext {
    /// A collector with two empty `REGION_WORDS`-word semi-spaces.
    pub(crate) fn new() -> MovingContext {
        let a = vec![Eterm::default(); REGION_WORDS].into_boxed_slice();
        let b = vec![Eterm::default(); REGION_WORDS].into_boxed_slice();
        let base = [a.as_ptr() as *mut Eterm, b.as_ptr() as *mut Eterm];
        MovingContext {
            _regions: [a, b],
            base,
            current: 0,
            used: 0,
            blocks: Vec::new(),
            collections: 0,
            peak_live_words: 0,
            moved_any: false,
        }
    }

    /// Hand out `n` words from the active region's bump cursor and record the block.
    fn bump(&mut self, n: usize) -> Result<*mut Eterm> {
        if self.used + n > REGION_WORDS {
            return Err(crate::error::unsupported(
                "moving collector: region exhausted",
            ));
        }
        let base = self.base[self.current];
        if n > 0 {
            self.blocks.push(Block {
                offset: self.used,
                size: n,
            });
        }
        let ptr = unsafe {
            // SAFETY: `used + n <= REGION_WORDS` words, so the `n` words starting at
            // `used` are inside the region; they are uninitialised for the caller
            // under the standard `alloc_words` stability contract (valid until the
            // next collecting allocation).
            base.add(self.used)
        };
        self.used += n;
        Ok(ptr)
    }

    /// Relocate every live block into the other region and rewrite the roots.
    fn collect(&mut self, roots: RootSet<'_>) {
        let from = self.current;
        let to = 1 - from;
        let from_base = self.base[from];
        let to_base = self.base[to];
        let from_addr = from_base as usize;
        let to_addr = to_base as usize;
        let from_used = self.used;

        // (1) Mark the blocks a root points at.
        let mut live = vec![false; self.blocks.len()];
        for &w in roots.x.iter().chain(roots.stack.iter()).chain(roots.fr.iter()) {
            if let Some(i) = block_at(&self.blocks, from_addr, w) {
                live[i] = true;
            }
        }

        // (2) Transitive closure: a pointer inside a live block keeps its target live.
        let mut changed = true;
        while changed {
            changed = false;
            for i in 0..self.blocks.len() {
                if !live[i] {
                    continue;
                }
                let b = self.blocks[i];
                for k in 0..b.size {
                    let w = unsafe {
                        // SAFETY: block `i` owns words `[offset, offset + size)` of the
                        // live `from` region and `k < size`, so the word is in bounds.
                        *from_base.add(b.offset + k)
                    };
                    if let Some(j) = block_at(&self.blocks, from_addr, w) {
                        if !live[j] {
                            live[j] = true;
                            changed = true;
                        }
                    }
                }
            }
        }

        // (3) Copy the live blocks into `to`, compacted, building the relocation table.
        let mut new_blocks = Vec::new();
        let mut moved: Vec<(Block, Block)> = Vec::new();
        let mut cursor = 0usize;
        for (i, b) in self.blocks.iter().enumerate() {
            if !live[i] {
                continue;
            }
            let nb = Block {
                offset: cursor,
                size: b.size,
            };
            unsafe {
                // SAFETY: the source and destination are `size` live words in two
                // *distinct* region buffers (so they cannot overlap), and the
                // destination prefix `[0, cursor)` is not yet written.
                core::ptr::copy_nonoverlapping(
                    from_base.add(b.offset),
                    to_base.add(cursor),
                    b.size,
                );
            }
            new_blocks.push(nb);
            moved.push((*b, nb));
            cursor += b.size;
        }

        // (4) Rewrite the roots in place: they still hold the *old* addresses.
        for slot in roots
            .x
            .iter_mut()
            .chain(roots.stack.iter_mut())
            .chain(roots.fr.iter_mut())
        {
            if let Some(new) = relocate_term(&moved, from_addr, to_addr, *slot) {
                *slot = new;
            }
        }

        // (5) Rebase the pointers *inside* the copied blocks (a cons cell's CDR, a
        // carved node's child link): a block moves as one unit, so every internal
        // absolute address has to move with it.
        for nb in &new_blocks {
            for k in 0..nb.size {
                let w = unsafe {
                    // SAFETY: `nb` owns words `[offset, offset + size)` of the live `to`
                    // region and `k < size`, so the word is in bounds.
                    *to_base.add(nb.offset + k)
                };
                if let Some(new) = relocate_term(&moved, from_addr, to_addr, w) {
                    unsafe {
                        // SAFETY: as above — the word is in bounds and exclusively ours.
                        *to_base.add(nb.offset + k) = new;
                    }
                }
            }
        }

        // (6) Poison the vacated region: a stale pointer must read garbage, not the
        // data it used to point at.
        for k in 0..from_used {
            unsafe {
                // SAFETY: `k < from_used <= REGION_WORDS`, so the word is in bounds.
                *from_base.add(k) = POISON;
            }
        }

        // (7) Swap in the collected region.
        self.current = to;
        self.used = cursor;
        self.blocks = new_blocks;
        self.collections += 1;
        if cursor > 0 {
            self.moved_any = true;
        }
        self.peak_live_words = self.peak_live_words.max(cursor);
    }
}

impl ExecutionContext for MovingContext {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        _arity: u32,
        _args: &[Eterm],
    ) -> Result<Eterm> {
        Err(crate::error::unsupported(&format!("bif {module}:{function}")))
    }

    fn ensure_heap(
        &mut self,
        _heap_need: u32,
        _live: u32,
        _x_regs: &mut [Eterm],
        _stack: &mut [Eterm],
    ) -> Result<()> {
        Ok(())
    }

    fn alloc_words_rooted(&mut self, n: usize, roots: RootSet<'_>) -> Result<*mut Eterm> {
        // Collect on *every* rooted allocation: the most aggressive schedule, so any
        // reserve-before-read bug is exercised on the very first build.
        self.collect(roots);
        self.bump(n)
    }
}

// ---------------------------------------------------------------------------
// The shared allocation-heavy workload
// ---------------------------------------------------------------------------

/// One iteration of the allocation-heavy workload: a tuple, a list and a flat map,
/// each built through its own rooted reservation, with the map kept as the loop's
/// live set (so the collecting run has something to move and everything else is
/// garbage it must reclaim).
///
/// The elements are small ints, so no built term refers to a *different* built
/// block: a reclamation-capable collector can drop every non-rooted block without
/// leaving a live term dangling.
fn workload_iteration(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    atoms: &AtomTable,
    k1: Atom,
    k2: Atom,
    i: i64,
) -> Result<()> {
    let tuple = make_tuple_rooted(machine, ctx, RootSet::ALL_LIVE, 3, 0, |_, _| {
        Ok(vec![make_small(i), make_small(i + 1), make_small(i + 2)])
    })?;
    let list = make_list_rooted(machine, ctx, RootSet::ALL_LIVE, 3, 0, |_, _| {
        Ok(vec![make_small(i), make_small(i + 1), make_small(i + 2)])
    })?;
    let map = make_map_rooted(machine, ctx, RootSet::ALL_LIVE, 2, 0, atoms, |_, _| {
        Ok(vec![
            (make_atom(k1), make_small(i)),
            (make_atom(k2), make_small(i + 1)),
        ])
    })?;
    // `tuple`/`list` are built and immediately garbage; only the map is rooted.
    let _ = (tuple, list);
    machine.set_x(0, map)?;
    Ok(())
}

/// The cumulative words the non-collecting arena handed out for `n` iterations.
fn unrooted_peak_words(n: usize, atoms: &AtomTable, k1: Atom, k2: Atom) -> usize {
    let mut machine = Machine::new();
    let mut ctx = CountingContext::new();
    for i in 0..n {
        workload_iteration(&mut machine, &mut ctx, atoms, k1, k2, i as i64).expect("workload");
    }
    ctx.words
}

/// The collector's peak **live** words and its collection count for `n` iterations.
fn rooted_peak_live_words(n: usize, atoms: &AtomTable, k1: Atom, k2: Atom) -> (usize, usize) {
    let mut machine = Machine::new();
    let mut ctx = MovingContext::new();
    for i in 0..n {
        workload_iteration(&mut machine, &mut ctx, atoms, k1, k2, i as i64).expect("workload");
    }
    (ctx.peak_live_words, ctx.collections)
}

// ---------------------------------------------------------------------------
// Deliverable #2 — the rooted hook fires
// ---------------------------------------------------------------------------

/// The builders must reach the heap through the **rooted** seam and never through
/// the unrooted one.
///
/// 1000 iterations of tuple + list + map are three reservations each, so the rooted
/// count is exactly `3 * 1000`; the unrooted count must be `0` (it was the whole
/// allocation count before the reroute, and the rooted count was `0`).
#[test]
fn the_builders_route_every_allocation_through_the_rooted_seam() {
    let mut atoms = AtomTable::new();
    let (k1, k2) = (atoms.intern("a"), atoms.intern("b"));
    let mut machine = Machine::new();
    let mut ctx = CountingContext::new();

    const N: usize = 1_000;
    for i in 0..N {
        workload_iteration(&mut machine, &mut ctx, &atoms, k1, k2, i as i64).expect("workload");
    }

    assert_eq!(
        ctx.rooted_calls,
        3 * N,
        "every builder must reserve through alloc_words_rooted"
    );
    assert_eq!(
        ctx.unrooted_calls, 0,
        "no builder may fall back to the unrooted alloc_words"
    );
    // A sanity check that the loop actually allocated: three reservations per
    // iteration, each non-empty.
    assert!(ctx.words >= 3 * N, "the loop must hand out real words");
}

/// The collecting context refuses the unrooted seam, so a builder that bypassed the
/// rooted one would fail loudly instead of silently skipping the collector.
#[test]
fn the_collecting_context_refuses_the_unrooted_seam() {
    let mut ctx = MovingContext::new();
    assert!(
        ctx.alloc_words(4).is_err(),
        "the mock collector must not serve the unrooted seam"
    );
}

// ---------------------------------------------------------------------------
// Deliverable #3 — correctness under a moving collector
// ---------------------------------------------------------------------------

/// A tuple built from root-resident operands is correct after the collection its own
/// reserve triggered, and at least one operand root actually moved.
///
/// The four elements are floats parked in `x(0..4)`; building the tuple runs a
/// collection that relocates them, so the builder's `read` closure sees (and stores)
/// post-collection addresses. A builder that had read its operands *before* the
/// reserve would store pre-move addresses, which the collector has poisoned.
#[test]
fn a_moving_collection_during_the_reserve_keeps_a_built_tuple_correct() {
    let mut machine = Machine::new();
    let mut ctx = MovingContext::new();

    let arity = 4u32;
    for i in 0..arity {
        let f = make_float_rooted(&mut machine, &mut ctx, f64::from(i) + 0.5).expect("float");
        machine.set_x(i, f).expect("x register");
    }
    let before: Vec<usize> = (0..arity)
        .map(|i| machine.x(i).expect("element root").raw())
        .collect();

    let n_elems = arity as usize;
    let tuple = make_tuple_rooted(
        &mut machine,
        &mut ctx,
        RootSet::ALL_LIVE,
        n_elems,
        0,
        |m, _slack| Ok((0..n_elems).map(|i| m.x(i as u32).expect("live element")).collect()),
    )
    .expect("tuple");
    machine.set_x(arity, tuple).expect("tuple root");

    // (c) The collector really moved something: every element root changed address.
    assert!(ctx.moved_any, "the mock collector must have relocated a block");
    let moved = (0..arity).any(|i| machine.x(i).expect("x").raw() != before[i as usize]);
    assert!(moved, "the reserve must have moved at least one element root");

    // (a) The tuple is correct: each field is the (moved) element root, with the
    // value the element was built with.
    let tuple = machine.x(arity).expect("tuple root");
    assert_eq!(tuple_arity(tuple).expect("arity"), n_elems);
    for i in 0..arity {
        let element = tuple_element(tuple, i as usize).expect("element");
        assert_eq!(element, machine.x(i).expect("x"), "field {i} is the moved root");
        assert_eq!(float_value(element).expect("float"), f64::from(i) + 0.5);
    }

    // Negative control: the pre-move address now reads as poison (the vacated region
    // was overwritten when its live blocks were copied out), so the assertion above
    // is only satisfiable by a builder that read *after* the reserve.
    let stale = Eterm::from_raw(before[0]);
    assert!(
        float_value(stale).is_err(),
        "the pre-move address must no longer read as a float"
    );
}

/// A whole multi-node graph carved out of ONE rooted reservation stays correct across
/// a collection — both the collection that ran *during* the reservation and a later
/// one that copies the finished graph.
///
/// The graph (`[{Float, 1}, {Big, 2}]`) is carved node-by-node out of one
/// [`reserve_slack`] region, with the bignum operand materialized into the
/// reservation's spare tail ([`Slack::integer`](super::Slack::integer)). Its nodes
/// reference each other by absolute address, so the later collection has to rebase
/// the *interior* pointers as well as the roots.
#[test]
fn a_rooted_reservation_survives_a_moving_collection_during_the_reserve() {
    let mut machine = Machine::new();
    let mut ctx = MovingContext::new();

    // An operand the graph will reference: a float parked in a root so the
    // reservation's collection relocates it.
    let float = make_float_rooted(&mut machine, &mut ctx, 2.5).expect("float");
    machine.set_x(0, float).expect("x register");
    let float_before = machine.x(0).expect("float root").raw();

    // ONE rooted reservation for the whole graph: nothing between the nodes can
    // collect (their pointers are not roots), so the graph cannot be stranded.
    let mut slack = reserve_slack(&mut machine, &mut ctx, 64).expect("reserve");

    // The collection ran during the reservation: the float root moved.
    let float_after = machine.x(0).expect("float root");
    assert_ne!(
        float_before,
        float_after.raw(),
        "the reservation must have moved the float root"
    );

    // Read operands AFTER the reserve (the reserve→read→write discipline), then carve.
    let big = slack.integer(1 << 60).expect("bignum into the spare tail");
    let t0 = slack.tuple(&[float_after, make_small(1)]).expect("tuple 0");
    let t1 = slack.tuple(&[big, make_small(2)]).expect("tuple 1");
    let list = slack.list(&[t0, t1]).expect("list");
    machine.set_x(1, list).expect("graph root");

    // The graph is correct: the float operand is the (moved) root, the carved bignum
    // tail holds the exact value, and the list links its two tuples.
    let (t0, t1) = read_graph(&machine).expect("graph");
    assert_eq!(tuple_element(t0, 0).expect("t0.0"), machine.x(0).expect("x(0)"));
    assert_eq!(float_value(tuple_element(t0, 0).expect("t0.0")).expect("float"), 2.5);
    assert_eq!(tuple_element(t0, 1).expect("t0.1"), make_small(1));
    assert_eq!(tuple_element(t1, 0).expect("t1.0"), big);
    assert_eq!(big_value(tuple_element(t1, 0).expect("t1.0")), (false, 1u64 << 60));
    assert_eq!(tuple_element(t1, 1).expect("t1.1"), make_small(2));

    // Force a *later* collection: the whole graph block — whose nodes reference each
    // other by absolute address — is copied as a unit, so the collector must rebase
    // its interior cons-cell/node pointers too.
    let _ = make_float_rooted(&mut machine, &mut ctx, 7.5).expect("second float");
    assert!(ctx.collections >= 3, "no collection ran");

    // The graph survived the copy: every node, link and value is still in place.
    let (t0, t1) = read_graph(&machine).expect("graph after the second collection");
    assert_eq!(tuple_element(t0, 0).expect("t0.0"), machine.x(0).expect("x(0)"));
    assert_eq!(float_value(tuple_element(t0, 0).expect("t0.0")).expect("float"), 2.5);
    assert_eq!(tuple_element(t0, 1).expect("t0.1"), make_small(1));
    assert_eq!(big_value(tuple_element(t1, 0).expect("t1.0")), (false, 1u64 << 60));
    assert_eq!(tuple_element(t1, 1).expect("t1.1"), make_small(2));
}

/// The two tuples of the graph rooted in `x(1)`, in order.
fn read_graph(machine: &Machine) -> Result<(Eterm, Eterm)> {
    let list = machine.x(1).ok_or(beam_common::BeamError::BadArg)?;
    let mut elems = list_elements(list)?;
    if elems.len() != 2 {
        return Err(beam_common::BeamError::BadArg);
    }
    let t1 = elems.pop().ok_or(beam_common::BeamError::BadArg)?;
    let t0 = elems.pop().ok_or(beam_common::BeamError::BadArg)?;
    Ok((t0, t1))
}

/// The `(negative, single digit)` value of the one-digit bignum `t`.
fn big_value(t: Eterm) -> (bool, u64) {
    // SAFETY: `t` is a live bignum carved by `Slack::integer` into the reserved block
    // the test still holds, so its header and one `ErtsDigit` are live and in bounds.
    let (negative, digits) = unsafe { big_parts(t) };
    (negative, digits.first().copied().expect("one digit"))
}

/// A root that points into a heap block is rewritten to the block's new address by
/// every collection — the property the two tests above rely on.
#[test]
fn the_mock_collector_relocates_a_root_across_every_collection() {
    let mut machine = Machine::new();
    let mut ctx = MovingContext::new();

    let first = make_float_rooted(&mut machine, &mut ctx, 1.25).expect("float");
    machine.set_x(0, first).expect("x register");

    // Each further rooted allocation collects, so the root moves — and keeps reading
    // as the same float at its new address.
    let mut seen = vec![first.raw()];
    for round in 0..4 {
        let _ = make_float_rooted(&mut machine, &mut ctx, round as f64).expect("float");
        let now = machine.x(0).expect("root");
        assert_ne!(now.raw(), *seen.last().expect("a previous address"));
        assert_eq!(float_value(now).expect("float"), 1.25);
        seen.push(now.raw());
    }
    assert!(ctx.moved_any, "no block was relocated");
    assert!(ctx.collections >= 5, "a collection must run per rooted allocation");
}

// ---------------------------------------------------------------------------
// Deliverable #4 — measured peak words, before vs after
// ---------------------------------------------------------------------------

/// The loop lengths the memory table below is measured over.
const PEAK_TABLE_LENGTHS: [usize; 4] = [1_000, 5_000, 20_000, 50_000];

/// The reroute's memory payoff, measured on the same allocation-heavy loop
/// ([`unrooted_peak_words`]/[`rooted_peak_live_words`], i.e. the rooted
/// [`make_tuple_rooted`]/[`make_list_rooted`]/[`make_map_rooted`] builders) over a
/// growing loop length, printed as a **before/after** table
/// (`... -p beam_engine --lib -- --nocapture`).
///
/// * **BEFORE** — the local reproduction of the unrooted path: the never-freeing
///   [`MinimalContext`] arena, whose cumulative resident words are its peak and grow
///   with the loop length.
/// * **AFTER** — the rooted path plus the collecting [`MovingContext`], which reclaims
///   every iteration's garbage, so its peak **live** words stay bounded however long the
///   loop runs (the live set is the map rooted at `x(0)` plus the blocks in flight).
#[test]
fn the_rooted_seam_bounds_peak_live_words_as_the_loop_grows() {
    let mut atoms = AtomTable::new();
    let (k1, k2) = (atoms.intern("a"), atoms.intern("b"));

    println!(
        "{:>6} | {:>35} | {:>29} | {:>11}",
        "N", "BEFORE: unrooted words", "AFTER: rooted live", "collections"
    );
    let mut unrooted = Vec::new();
    let mut rooted = Vec::new();
    for n in PEAK_TABLE_LENGTHS {
        let before = unrooted_peak_words(n, &atoms, k1, k2);
        let (live, collections) = rooted_peak_live_words(n, &atoms, k1, k2);
        println!("{n:>6} | {before:>35} | {live:>29} | {collections:>11}");

        // Every iteration's three builders reserve once each through the rooted seam,
        // so the collector runs at least once per reservation.
        assert!(
            collections >= 3 * n,
            "the collector must run on every rooted reservation at N={n}: {collections}"
        );
        assert!(
            live < 64,
            "the rooted live set must stay bounded at N={n}: {live}"
        );
        unrooted.push(before);
        rooted.push(live);
    }

    // BEFORE grows with N (each iteration hands out a constant number of words).
    for pair in unrooted.windows(2) {
        assert!(
            pair[1] > pair[0],
            "the unrooted peak must grow with N: {unrooted:?}"
        );
    }
    // AFTER does not: the collecting live set is the same handful of words at every N.
    let bounded = rooted[0];
    for live in &rooted {
        assert_eq!(
            *live, bounded,
            "the rooted live set must not grow with N: {rooted:?}"
        );
    }
    // And the two are orders of magnitude apart at the largest N.
    let largest_unrooted = unrooted.last().copied().expect("a table row");
    let largest_rooted = rooted.last().copied().expect("a table row");
    assert!(
        largest_rooted * 1000 < largest_unrooted,
        "the collecting live set ({largest_rooted}) must be far below the unrooted peak \
         ({largest_unrooted})"
    );
}
