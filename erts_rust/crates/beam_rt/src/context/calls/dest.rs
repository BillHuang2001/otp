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
//! The BIF-result copy-out's **destination-containment short-circuit**.
//!
//! # The defect this exists for
//!
//! `beam_bifs` returns the argument **word unchanged** for a no-op container operation —
//! `maps:put/3` whose key is already bound to that exact value (`src/maps.rs::put_3`), the
//! sibling `update/3`/`remove/2` shapes — exactly as the C runtime's `erts_maps_put` etc. return
//! the very `Eterm` they were handed. ERTS answers `erts_debug:same/2` (`erts_internal:same_2/2`,
//! plain word equality) with `true` for
//! `M = #{a => 1}, erts_debug:same(maps:put(a, 1, M), M)`.
//!
//! This crate used to answer `false`, because the registered-call tail re-homes **every**
//! container result unconditionally: the word comes back pointing at a term that already lives on
//! the destination (the process's scanned young heap, or the runtime's `-eval` store), and
//! [`super::RuntimeContext::copy_result_fresh`] copied it anyway — so the two words named two
//! structurally equal but distinct objects.
//!
//! # The rule
//!
//! A result whose **root object already lives on the destination region of the same kind the
//! copy-out would put it on** is handed through **unchanged**, and no copy runs. "The
//! destination" here is the region the caller's collector really scans (the process route) or the
//! region the runtime's long-lived store really owns (the `-eval` route). Handing such a word
//! through is sound because:
//!
//! * the word is **already** collectable / lifetime-managed by the destination — the copy exists
//!   solely to move a result *into* that region, and it is already there;
//! * a container the BIF built on its own pooled frontier cannot reach this arm — the pooled
//!   region is neither scanned by a process collector nor part of the `-eval` store, so the
//!   containment test sends it to the copy exactly as before;
//! * the pool rewind the tail licenses on a successful move stays sound: the handed-through word
//!   is not on the pooled region and the call's own words `[mark, used)` are reclaimed only when
//!   nothing above them lives, which the rewind's own rule already requires.
//!
//! # Why the test is a real containment test, not a heuristic
//!
//! A result can be a **mixed-heap** container: a map on the destination whose values were built
//! on the pool, or (on the `-eval` route) a container whose children the evaluator nested from an
//! earlier call's words. The predicate is therefore
//! [`Geometry::word_is_scanned`](super::gc::Geometry::word_is_scanned) — the **same**
//! classification the copier and the GC valve use to decide `Target::Foreign` vs in-place — for
//! the process route, and [`term_in_heap_block`](super::gc::term_in_heap_block) (the exact block
//! test [`Geometry`](super::gc::Geometry) hoists) over the store's live and retired regions for
//! the `-eval` route. Neither is re-derived here: both are `super::gc`'s definitions.
//!
//! A mixed-heap container whose **root** is already on the destination is still handed through,
//! and that is the correct answer: every child it can reach is either already on a region the
//! destination's owner keeps alive (the store's own words, another scanned space) or an immediate
//! or literal. A container holding a **reference** never reaches this arm — the tail's
//! `graph_holds_reference` check runs first on the process route, and keeps the conservative
//! "never copy, never rewind" answer it always had.
use beam_term::Eterm;

/// Whether `term`'s root object already lives on a region `process`'s collector **scans** — the
/// process route's destination-containment test, i.e. the exact answer
/// [`Geometry::word_is_scanned`](super::gc::Geometry::word_is_scanned) gives for the word the
/// copy-out would otherwise re-copy.
///
/// `Scanned` is the right notion rather than "inside the current young space": the copy-out's
/// purpose is to put the result on a region the process's collector walks and relocates, and a
/// term in a growth-retired nursery space or in the mature generation is already there. An
/// immediate (neither boxed nor a cons) and a literal-area constant answer `false` at the shape
/// test, so this predicate is only ever consulted for a container.
#[inline]
pub(super) fn process_holds(process: &beam_process::Process, term: Eterm) -> bool {
    super::super::gc::Geometry::of(process).word_is_scanned(
        process,
        term,
        super::super::gc_valve_trace::enabled(),
    )
}

/// Whether `term`'s root object already lives inside the `-eval`/single-pass **store** — the
/// table-less route's destination-containment test.
///
/// The store is [`Runtime::eval_heap`](crate::runtime::Runtime) plus every region
/// [`Runtime::grow_eval_heap`](crate::runtime::Runtime::grow_eval_heap) retired (a replaced region
/// is never moved and never freed while anything can name it, so its words are as valid as the
/// live region's), which is exactly the word range
/// [`Runtime::for_each_off_heap_region`](crate::runtime::Runtime) already publishes.
#[inline]
pub(super) fn store_holds(runtime: &crate::runtime::Runtime, term: Eterm) -> bool {
    super::super::gc::term_in_heap_block(&runtime.eval_heap, term)
        || runtime
            .eval_heap_retired
            .iter()
            .any(|heap| super::super::gc::term_in_heap_block(heap, term))
}

#[cfg(test)]
mod tests;
