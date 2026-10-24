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

//! The copier's **destination-pricing gate**: how many words a copy of `term` will write into
//! `ctx`'s destination, counted in one walk over a per-thread **warm scratch**.
//!
//! [`preflight_words`] is the one place the copy engine asks that question, and
//! [`crate::copy::copy_struct`] / [`crate::copy_struct_shared`] consult its answer before writing a
//! word — so a refusal is total (no partial write) and the typed
//! [`AllocKind::Message`](beam_common::AllocKind) failure carries the **exact** figure the caller
//! then reuses to size a message buffer ([`crate::send_fragmented_after_refusal`]).
//!
//! # It prices what the copier *writes*, not what the source *occupies*
//!
//! The count is the C's `size_object_litopt(obj, area)`: the `litopt != NULL` half of
//! `size_object_x` (`erts/emulator/beam/copy.c:105,119`), the form the **message-heap charge** uses
//! (`global.h:1136`; `erl_message.c:771,839`, whose reduction charge follows at `erl_message.c:881`
//! on the `do_send` path of `bif.c`) and *not* the `size_object_x(t, NULL)` source-occupancy count
//! (`global.h:1135`) [`beam_term::size_object`] publishes.
//!
//! That destination-pricing model is **delegated**: the DFS, the per-object interior
//! (`beam_term::layout`), the bound and the cycle verdict are `beam_term::size`'s, driven by the
//! [`SizingPolicy`] this crate supplies ([`CopyPricing`]). The copier keeps only the two axes on
//! which its price differs from the source occupancy, one per policy method:
//!
//! 1. **Sharing** — [`SizingPolicy::is_shared`] is the copier's own [`crate::copy::is_shared`], so
//!    a **literal-area** subterm costs `0` destination words and is not descended into (the copy
//!    shares it by identity, `copy.c:99,119`). The `beam_term` default policy would count its
//!    interior and over-demand words the copy never writes; `tests/destination_pricing.rs` pins the
//!    delta.
//! 2. **Per-object pricing** — [`SizingPolicy::object_words`] overrides the one shape
//!    `beam_term::layout::object_words` prices for the *source* heap: a **refc binary whose payload
//!    no source off-heap handle owns** costs its *flattened* window ([`refc_binary_words`]), because
//!    the copier finds no payload to share and flattens it into an on-heap bitstring — the
//!    `heap_bits_size_words(end - start)` block `copy.rs`'s `copy_match_context` writes, typically
//!    far more than the 8-word `[BinRef(3)][ErlSubBits(5)]` pair the layout counts. The default
//!    policy would under-count and the sized path would reserve too few words, failing *mid-copy*
//!    after a partial write; the price depends on [`CopyContext`]'s **source** off-heap table, which
//!    is precisely why the policy is a struct over the copy boundary rather than a term-only walk.
//!
//! Every other shape keeps the canonical answer, including a `SUB_BITS` **match context**, whose
//! `beam_term::layout::object_words` already *is* the flattened window the copier writes.
//!
//! # One walk, exact in both directions, and the cycle guard on it
//!
//! A term the engine builds is always acyclic, but a *forged* term (a raw write into the heap) can
//! point back at one of its own ancestors, and a naive walk of such a graph would loop forever (or
//! exhaust memory). [`preflight_words`] therefore runs the shared walk **unbounded**
//! (`limit = usize::MAX`), the one mode in which the walk detects the back-edge itself through its
//! DFS path set and reports [`TermError::TermGraphCyclic`] — which the copier maps to a typed
//! [`BeamError::BadArg`](beam_common::BeamError::BadArg) instead of hanging. That single walk is
//! also **exact**: at `usize::MAX` the running total saturates rather than passing the limit, so the
//! walk's over-limit arm is unreachable and an acyclic graph always comes back with its true
//! destination price. One walk is the whole gate: the caller compares that figure against the
//! destination's free words, and — on the fragment path — reuses the very same figure to size the
//! message buffer (see [`crate::send_fragmented`]). This is the C's shape, two walks per send
//! (`size_object_litopt` then `copy_struct_litopt`, `erl_message.c:771,797,839,860`).
//!
//! The fast path inherits the same guarantee from the other direction: its trial target hands out
//! only the destination's finite free words, so a forged cycle exhausts them (in at most
//! `available / 2` objects), the attempt is abandoned and the preflight path re-derives the typed
//! `BadArg`.
//!
//! # The warm scratch: one per thread, allocation-free once warm
//!
//! A walk needs a `Vec` of pending steps and a `HashSet` of the objects on the current DFS path;
//! `beam_term::size` lets the caller own both as a [`SizingScratch`], which every walk **clears**
//! rather than drops. So the walk is a per-call allocation *only* if the caller builds a fresh
//! scratch each time — and the copier is the workspace's hot sizer (it prices every message it
//! moves), so it keeps one.
//!
//! The scratch lives in a **thread-local**, not in [`CopyContext`]: a `CopyContext` is constructed
//! per call by every host in the workspace (`beam_rt`'s send sites, `beam_process`'s control-signal
//! staging, `beam_message`'s own fragment path), so a scratch carried as a context field would be
//! cold at every one of them — which is exactly the cost this module exists to remove. A
//! thread-local is warm across calls by construction, and one scratch per thread keeps the walk
//! free of lock contention (the sizing walk is read-only over the source graph, so two threads
//! pricing concurrently already share nothing but the graph).
//!
//! The scratch is **not** a GC root holder and carries no live state: a walk clears it before it
//! starts, it holds only prices-in-progress (stack steps and source-object addresses, both cleared
//! before the walk returns), it is never scanned by a collector, and nothing is registered in it
//! across a collection. Retaining its buffers therefore cannot keep a term alive, and reusing one
//! across graphs cannot change a result (`beam_term`'s scratch docs make the same claim).
//!
//! Retaining capacity means an outlier payload leaves a high-water-mark buffer behind;
//! [`shrink_warm_scratch`] gives it back to the allocator for a host that would rather pay the
//! re-allocation later.

use std::cell::RefCell;

use beam_common::Result;
use beam_term::error::TermError;
use beam_term::prelude::*;
use beam_term::size::{size_object_bounded_with_scratch, SizingPolicy, SizingScratch};

use crate::copy::{is_shared, CopyContext};
use crate::layout;

thread_local! {
    /// The copier's pricing scratch, one per thread: cleared (never re-`malloc`ed) by every walk,
    /// so a thread that has priced one graph of a given size prices the next allocation-free.
    ///
    /// A `RefCell` because the walk needs `&mut` and the borrow must be checked, not assumed: a
    /// nested pricing call (see [`preflight_words`]) must not alias it.
    static WARM: RefCell<SizingScratch> = RefCell::new(SizingScratch::new());
}

/// Prices `term`'s graph for a copy into `ctx`'s destination, returning the flat word count to
/// reserve — the copy engine's destination-capacity gate, in **exactly one traversal** of the graph,
/// over this thread's warm [`SizingScratch`].
///
/// The count is [`CopyPricing`]'s over `beam_term::size`'s walk, run **unbounded**
/// (`size_object_bounded_with_scratch(term, usize::MAX, …)`), which is both exact and total in that
/// one pass:
///
/// * an **acyclic** graph comes back exact — the walk's over-limit arm cannot fire at `usize::MAX`
///   (the running total saturates instead of passing the limit), so the `Complete(n)` verdict is the
///   true destination price for a graph that fits *and* for one that does not. That matters for the
///   refusal: the typed [`AllocKind::Message`](beam_common::AllocKind) failure carries this exact
///   figure as its `requested` byte count, and [`crate::send_fragmented`] sizes the message buffer
///   from it, so a bounded accept/too-large split would only have to re-walk for the same number;
/// * a **cyclic** or otherwise malformed graph is a typed
///   [`BeamError::BadArg`](beam_common::BeamError::BadArg), never a hang: the unbounded walk is
///   exactly the mode whose DFS path set is what detects a back-edge
///   ([`TermError::TermGraphCyclic`]).
///
/// The walk's buffers come from the thread's scratch ([`WARM`]), which the walk clears rather than
/// drops — so after one warm-up call of comparable size this function performs **no allocation at
/// all**. The borrow is taken **only for the walk**: it is released before this function returns, so
/// the copy that follows the call never runs with the scratch held, and a nested pricing call can
/// never find it held by an outer walk (see the cold branches below).
///
/// A re-entrant call — a walk whose [`SizingPolicy`] somehow priced a second graph — and a call from
/// a thread whose scratch is already destroyed run the identical walk on a **locally built**
/// [`SizingScratch::new`] instead of panicking on the borrow. The answer is the same either way; only
/// the allocation is paid.
///
/// The caller compares the returned figure against the destination's free words
/// ([`crate::copy::copy_struct`] does: `to.available() < needed` ⇒ `heap_exhausted(needed)`, so the
/// refusal is total and never leaves a partial write).
///
/// `limit` is the caller's capacity target and is **not consulted**, deliberately: the figure must be
/// exact whether or not the graph fits, so bounding the walk could only buy an early refusal that
/// then has to be re-priced. It stays in the signature because it is the capacity the caller is
/// gating against, and because the recorded equivalence oracle (`copy_reference`) drives both this
/// walk and the retired bounded one at every limit from `0` to past each graph's exact count.
pub(crate) fn preflight_words(term: Eterm, limit: usize, ctx: &CopyContext<'_>) -> Result<usize> {
    let warmed = WARM.try_with(|cell| match cell.try_borrow_mut() {
        Ok(mut warm) => Some(preflight_words_with_scratch(term, limit, ctx, &mut warm)),
        Err(_) => None,
    });
    match warmed {
        Ok(Some(result)) => result,
        // Cold: either the scratch is already borrowed (a re-entrant walk) or it is gone (thread
        // teardown). Same walk, same answer, its own scratch.
        _ => preflight_words_with_scratch(term, limit, ctx, &mut SizingScratch::new()),
    }
}

/// [`preflight_words`] over a **caller-owned** [`SizingScratch`] — the real body: [`CopyPricing`]
/// over `beam_term::size`'s unbounded walk, with the cycle verdict mapped to the crate's typed
/// error.
///
/// Split out so the measurement in `tests/traversals.rs` can price a graph on a scratch it controls
/// (cold vs warm) without reaching for the thread-local, and so a host that already owns a scratch
/// can pass it in instead of borrowing this thread's.
pub(crate) fn preflight_words_with_scratch(
    term: Eterm,
    _limit: usize,
    ctx: &CopyContext<'_>,
    scratch: &mut SizingScratch,
) -> Result<usize> {
    let policy = CopyPricing { ctx };
    match size_object_bounded_with_scratch(term, usize::MAX, &policy, scratch) {
        Ok(words) => Ok(words),
        // The only error the unbounded walk can report (see above): a back-edge, i.e. a forged or
        // otherwise malformed graph.
        Err(TermError::TermGraphCyclic) => Err(crate::error::bad_term()),
        // Unreachable today (`usize::MAX` cannot be exceeded), kept so a future change to
        // `beam_term::size`'s verdict machinery surfaces as a typed error rather than a wrong count.
        Err(e) => Err(e.into()),
    }
}

/// Releases this thread's warm pricing scratch — the buffers [`preflight_words`] retains — back to
/// the allocator.
///
/// Retaining those buffers is the point (a warm scratch makes the walk allocation-free), so an
/// outlier payload leaves a high-water-mark `Vec`/`HashSet` behind for the life of the thread. A host
/// that would rather give that memory back — bounding RSS after a burst of very large messages —
/// calls this; the next preflight simply re-grows the buffers, so **no result changes**, only the
/// allocation timing. It is also what the cold-vs-warm measurement in `tests/traversals.rs` uses to
/// put the scratch back into its cold state.
///
/// A call that cannot take the borrow (a re-entrant one, from inside a walk) is a no-op: the scratch
/// is in use, so there is nothing to shrink.
pub fn shrink_warm_scratch() {
    let _ = WARM.try_with(|cell| {
        if let Ok(mut warm) = cell.try_borrow_mut() {
            warm.shrink_to_fit();
        }
    });
}

/// Prices `term`'s graph for a copy into `ctx`'s destination — the [`SizingPolicy`] that turns
/// `beam_term::size`'s walk into the copier's own destination-pricing cost model.
///
/// The copier's price differs from `beam_term::size_object`'s source occupancy on exactly the two
/// axes the trait exposes, one per method: the copy **shares** a literal-area subterm by identity
/// ([`is_shared`], so it costs `0` destination words and is not descended into) and it **flattens** a
/// refc binary whose payload no source off-heap handle owns ([`refc_binary_words`]). Every other
/// object is priced by the canonical `beam_term::layout::object_words`, while the DFS, the bound and
/// the cycle verdict stay `beam_term::size`'s (see the module docs).
struct CopyPricing<'a> {
    ctx: &'a CopyContext<'a>,
}

impl SizingPolicy for CopyPricing<'_> {
    fn is_shared(&self, t: Eterm) -> bool {
        is_shared(t)
    }

    fn object_words(&self, header: Eterm, payload: *const Eterm) -> Option<usize> {
        // Reconstructing the term from its header address is faithful here: `SizingPolicy`'s docs
        // bless exactly this, and a literal-tagged word was already filtered out by `is_shared`
        // before any pricing happened.
        let t = make_boxed(payload);
        if layout::is_refc_binary_term(t) {
            // The one shape `beam_term::layout::object_words` prices for the *source* heap while
            // the copier writes a flattened window, so its price depends on the copy boundary's
            // source off-heap table.
            return Some(refc_binary_words(t, self.ctx));
        }
        // SAFETY: the walk only prices a live, initialized object of `header`'s shape, which is
        // exactly `object_words`'s caller contract.
        Some(unsafe { layout::object_words(header, payload) })
    }
}

/// The destination words the copier writes for the refc binary `t`.
///
/// A refc binary whose payload is owned by a handle in the source process's off-heap table is copied
/// as the 8-word `[BinRef(3)][ErlSubBits(5)]` pair ([`ERL_REFC_BITS_SIZE`]) that **shares** the
/// payload (see `copy.rs`'s `copy_refc_binary`). One whose payload no held handle owns has no pair to
/// share, so the copier **flattens** it into a fresh on-heap bitstring of the window's bits — the
/// `heap_bits_size_words(end - start)` block `copy.rs`'s `copy_match_context` writes.
///
/// This sizing lives beside the flag it mirrors (`layout::is_refc_binary_term`) so the preflight and
/// the copy never disagree about what a refc binary costs. It is the one price [`CopyPricing`]
/// overrides, and the retired walk kept by `copy_reference` prices through it too.
pub(crate) fn refc_binary_words(t: Eterm, ctx: &CopyContext<'_>) -> usize {
    // SAFETY: `t` is a live refc binary (the caller's `is_refc_binary_term` guard), so its
    // `ErlSubBits` words 1..4 are initialised and its `orig` slot is the live `BinRef` whose `val`
    // is the payload's first data byte.
    let (_base_flags, start, end, orig) = unsafe { sub_bits_parts(t) };
    // SAFETY: `orig` is the `BinRef` of a refc binary, so its two payload words are live.
    let (val, _next) = unsafe { bin_ref_parts(orig) };
    if ctx.src_off_heap().binary_by_data_ptr(val as *const u8).is_some() {
        ERL_REFC_BITS_SIZE
    } else {
        heap_bits_size_words(end.raw().saturating_sub(start.raw()))
    }
}

#[cfg(test)]
mod tests {
    use beam_alloc::Heap;
    use beam_term::prelude::*;
    use beam_term::size::SizingScratch;

    use super::{preflight_words, preflight_words_with_scratch, shrink_warm_scratch};
    use crate::copy::{copy_struct, size_of_copy, CopyContext};
    use crate::off_heap::OffHeapTable;

    /// A source heap holding a tuple of `n` small integers, plus that tuple.
    fn tuple(n: usize) -> (Heap, Eterm) {
        let mut heap = Heap::new(64).expect("source heap");
        let elems: Vec<Eterm> = (0..n as i64).map(make_small).collect();
        let t = heap.tuple(&elems).expect("the tuple fits");
        (heap, t)
    }

    #[test]
    fn the_warm_scratch_answers_exactly_what_a_fresh_scratch_does() {
        let (_heap, term) = tuple(5);
        let src = OffHeapTable::new();
        let mut dst_off = OffHeapTable::new();
        let ctx = CopyContext::new(&src, &mut dst_off);

        // Price the graph on the thread's scratch, then on a fresh one: identical figures, and the
        // figure is the copy's own price (a 6-word tuple, nothing shared).
        let warm = preflight_words(term, usize::MAX, &ctx).expect("acyclic");
        let mut fresh = SizingScratch::new();
        let cold = preflight_words_with_scratch(term, usize::MAX, &ctx, &mut fresh).expect("acyclic");
        assert_eq!(warm, cold);
        assert_eq!(warm, size_of_copy(term));
        assert_eq!(warm, 6, "a 5-element tuple is one header word + five elements");
    }

    #[test]
    fn a_reused_scratch_carries_no_state_between_walks() {
        let (mut heap, term) = tuple(4);
        let list = heap.cons(make_small(7), NIL).expect("the cons fits");
        let src = OffHeapTable::new();
        let mut dst_off = OffHeapTable::new();
        let ctx = CopyContext::new(&src, &mut dst_off);

        let mut scratch = SizingScratch::new();
        let first = preflight_words_with_scratch(term, usize::MAX, &ctx, &mut scratch)
            .expect("acyclic");
        let second =
            preflight_words_with_scratch(list, usize::MAX, &ctx, &mut scratch).expect("acyclic");
        let third = preflight_words_with_scratch(term, usize::MAX, &ctx, &mut scratch)
            .expect("acyclic");
        assert_eq!(first, third, "the same graph prices the same on a reused scratch");
        assert_ne!(first, second, "and a different graph prices differently");
        assert_eq!(second, 2, "a cons cell is two words");
    }

    #[test]
    fn a_forged_cycle_is_a_typed_badarg_on_the_warm_path_too() {
        let mut heap = Heap::new(8).expect("source heap");
        let root = heap.cons(make_small(1), NIL).expect("the cons fits");
        // SAFETY: `root` is a cons cell on `heap`, whose two words this test owns; pointing the tail
        // at the cell itself is what makes the graph self-referential (the malformed shape priced
        // here), and nothing is read between the write and the pricing below.
        unsafe { *list_val(root).add(1) = root };

        let src = OffHeapTable::new();
        let mut dst_off = OffHeapTable::new();
        let ctx = CopyContext::new(&src, &mut dst_off);
        assert!(
            preflight_words(root, usize::MAX, &ctx).is_err(),
            "the unbounded walk reports the back-edge instead of looping"
        );

        let mut dst = Heap::new(8).expect("destination heap");
        let mut copy_ctx = CopyContext::new(&src, &mut dst_off);
        assert!(copy_struct(root, &mut dst, &mut copy_ctx).is_err());
        assert_eq!(dst.used(), 0, "a forged graph writes no destination word");
    }

    #[test]
    fn shrinking_the_warm_scratch_changes_no_result() {
        let (_heap, term) = tuple(6);
        let src = OffHeapTable::new();
        let mut dst_off = OffHeapTable::new();
        let ctx = CopyContext::new(&src, &mut dst_off);

        let before = preflight_words(term, usize::MAX, &ctx).expect("acyclic");
        // Twice: the first call may itself be the one that initialises this thread's scratch.
        shrink_warm_scratch();
        shrink_warm_scratch();
        let after = preflight_words(term, usize::MAX, &ctx).expect("acyclic");
        assert_eq!(before, after);
        assert_eq!(before, size_of_copy(term));
    }
}
