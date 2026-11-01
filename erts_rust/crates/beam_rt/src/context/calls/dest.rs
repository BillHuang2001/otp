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
//! # The rule — one question per route: *does this result still have to move?*
//!
//! The copy-out exists for exactly one reason: to move a result **off** the pooled BIF region the
//! call allocated from, because the tail rewinds that region to this call's `mark` and every word
//! above `mark` becomes garbage. So each route asks its destination a question about the word, and
//! a result that still has nothing to move is handed through **unchanged** with no copy:
//!
//! * **process route** ([`process_holds`]) — is the root already on a region this process's
//!   collector **scans**? Then it is already collectable (the copy's other job) and already
//!   outside the pool;
//! * **table-less route** ([`store_holds`], [`pool_holds`]) — no process scans anything here, so
//!   the rewind is the only thing that matters, and the word is handed through when it is already
//!   in the runtime's own `-eval` store **or** when it lies on **no pooled region at all**: the
//!   whole graph it reaches then predates this call, so not one word of it can be a word the
//!   rewind frees.
//!
//! # Why handing the word through is sound — the rewind, and now the GC
//!
//! The workspace law is that a raw `Eterm` in a long-lived slot roots nothing and that a
//! pointer-stable, never-scanned store must not hold a container whose children live on a scanned
//! heap. Both destinations this module names satisfy the law for the words it hands them:
//!
//! * the **process heap** is the region that process's collector scans, relocates and roots — a
//!   term standing on it is exactly as rooted as one the copier would have written there, and the
//!   predicate is the copier's **own** classification
//!   ([`Geometry::word_is_scanned`](super::gc::Geometry::word_is_scanned)), i.e. the very answer
//!   the copier's `Target::Foreign` decision reads, so a term in a retired nursery space or in the
//!   mature generation counts as "on it" for the same reason the copier does;
//! * the **`-eval` store** ([`Runtime::eval_heap`](crate::runtime::Runtime) plus every region a
//!   growth step retired) is pointer-stable and never reset — a word in it stays valid for the
//!   rest of the run, whether or not the rewind runs;
//! * and the arena/literal words the [`pool_holds`] arm hands through are pointer-stable too: the
//!   engine's arena is never moved and a literal region is frozen at load time.
//!
//! Those three facts make the **rewind** decision airtight — a container whose root is on no pooled
//! region predates the call, so the rewind cannot free a word of it. They do **not**, by
//! themselves, make the **GC** decision airtight: a store/arena/literal root can have children that
//! live on a scheduled process's scanned young heap (a `-eval` route reaching a nested run or a
//! spawned closure), and a collection hands such a root the `Target::Foreign` verdict and does not
//! descend into it — relocating the children out from under it. So the table-less arm no longer
//! hands a word through on the root test alone: it also runs [`graph_reaches_scanned_heap`], which
//! walks the whole reachable graph and answers `true` when any readable container word (root
//! included) lives inside a scanned region of **any** live process. A hand-through is therefore
//! taken only for a graph that is **fully self-contained** on pointer-stable regions (or nothing
//! but immediates); a mixed-heap graph falls through to the deep copy, which re-roots the whole
//! graph on the never-reset store. A container holding a **reference** still never reaches either
//! arm — the tail's `graph_holds_reference` check runs first on the process route and keeps its
//! conservative "never copy, never rewind" answer, because a reference's identity *is* its
//! address.
//!
//! The information only a caller has — *which* region this call allocated from — is a parameter
//! (`Option<&Heap>`): `None` means "this result came from somewhere the caller cannot vouch for"
//! (the aliased-ETS table's own `TermStore`, or the `system_info` scratch region), and that arm
//! always copies.
use beam_alloc::Heap;
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
/// table-less route's *first* positive answer (its companion is [`pool_holds`], which covers a
/// result that is on no pooled region at all, and therefore has nothing to move either).
///
/// The store is [`Runtime::eval_heap`](crate::runtime::Runtime) plus every region
/// [`Runtime::grow_eval_heap`](crate::runtime::Runtime::grow_eval_heap) retired (a replaced region
/// is never moved and never freed while anything can name it, so its words are as valid as the
/// live region's), which is exactly the word range
/// [`Runtime::for_each_off_heap_region`](crate::runtime::Runtime) already publishes.
#[inline]
pub(crate) fn store_holds(runtime: &crate::runtime::Runtime, term: Eterm) -> bool {
    super::super::gc::term_in_heap_block(&runtime.eval_heap, term)
        || runtime
            .eval_heap_retired
            .iter()
            .any(|heap| super::super::gc::term_in_heap_block(heap, term))
}

/// Whether any **pooled BIF region** could hold `term`'s root — the table-less route's
/// "is there anything to move?" test, given the region this call allocated from.
///
/// `pool` is the region the caller's call is bumping (the one the tail is about to rewind), and
/// the runtime's idle [`bif_heaps`](crate::runtime::Runtime) are consulted too: a region that was
/// handed back to the pool can be **lent out again** and rewound from a later `mark`, so a word
/// standing on one is no safer than a word on the current region.
///
/// `false` therefore means "no pooled region contains this word", which is the strongest form of
/// the answer this module can give: whatever the root reaches predates the call (see the module
/// docs), so the rewind cannot invalidate it and there is nothing to copy.
#[inline]
pub(crate) fn pool_holds(runtime: &crate::runtime::Runtime, pool: &Heap, term: Eterm) -> bool {
    super::super::gc::term_in_heap_block(pool, term)
        || runtime
            .bif_heaps
            .iter()
            .any(|heap| super::super::gc::term_in_heap_block(heap, term))
}

/// Whether **any** word reachable from `term` lives on a region some live process's collector
/// **scans** — a scheduled process's young heap, a growth-retired nursery space, or the mature
/// generation.
///
/// # Why the table-less short-circuit must ask this
///
/// [`store_holds`] and [`pool_holds`] classify only the **root** object, which is the exact answer
/// the **rewind** decision needs ("does the rewind free the root?"). It is not the answer the
/// **GC** decision needs: the store and the engine arena are pointer-stable but never-scanned, so a
/// collection hands a container standing on either the `Target::Foreign` verdict and does not
/// descend into it. A container whose root stands there but whose **child** stands on a scanned
/// young heap is therefore relocated out from under itself the next time that process collects —
/// the `0x3f` forwarding-header CAR the `error_info_abort_probe` D1c residual shows. This walk
/// closes that hole: it answers `true` when any readable container word in the graph (root
/// included) lives inside a scanned region of **any** live process, and `false` only when the whole
/// graph is pointer-stable (the store, the arena, a literal area, or nothing but immediates), in
/// which case handing the word through unchanged is sound and keeps a no-op BIF's term identity
/// observable.
///
/// # Safety
///
/// The walk reads only words reachable from a live `term` through the workspace's canonical child
/// enumeration ([`beam_term::layout::for_each_child_slot`]), guarded exactly as
/// [`super::RuntimeContext::graph_holds_reference`] guards its reads: a literal, an immediate and a
/// non-term `HEADER` word are skipped, and a word whose pointer names no address a term can live at
/// ([`super::super::gc::dereferenceable_pointer`]) is skipped. A forged cycle exhausts
/// [`super::WALK_VISIT_BUDGET`] and answers `true` — "assume it reaches a scanned heap" — whose
/// only consequence is a deep copy, never a wrong move.
#[allow(unsafe_code)]
pub(crate) fn graph_reaches_scanned_heap(
    runtime: &crate::runtime::Runtime,
    term: Eterm,
) -> bool {
    fn scanned_by_any(runtime: &crate::runtime::Runtime, word: Eterm) -> bool {
        runtime.processes().pids().any(|pid| {
            let Some(process) = runtime.processes().get(pid) else {
                return false;
            };
            super::super::gc::Geometry::of(process).word_is_scanned(process, word, false)
        })
    }

    let mut work = super::Walklist::new();
    let mut budget = super::WALK_VISIT_BUDGET;
    work.push(term);
    while let Some(popped) = work.pop() {
        if !super::RuntimeContext::walk_may_lead_further(popped) {
            continue;
        }
        let Some(mut address) = super::super::gc::dereferenceable_pointer(popped) else {
            continue;
        };
        let mut item = popped;
        loop {
            if budget == 0 {
                return true;
            }
            budget -= 1;
            if scanned_by_any(runtime, item) {
                return true;
            }
            if !beam_term::prelude::is_list(item) {
                let ptr = address as *const Eterm;
                // SAFETY: `item` passed the walk's admission test and is not a cons, so it is a
                // boxed **term** word whose object pointer is non-null and aligned; the word at
                // that address is its live header, and `for_each_child_slot` visits exactly that
                // shape's term-reference slots.
                unsafe {
                    let header = *ptr;
                    beam_term::layout::for_each_child_slot(header, ptr, |offset| {
                        let child = *ptr.add(offset);
                        if super::RuntimeContext::walk_may_lead_further(child) {
                            work.push(child);
                        }
                    });
                }
                break;
            }
            let cell = address as *const Eterm;
            // SAFETY: `item` is a term word with the `LIST` primary tag whose cell pointer is
            // non-null and aligned, so the two words at that address are its live `car` and `cdr`.
            let (car, cdr) = unsafe { (*cell, *cell.add(1)) };
            if super::RuntimeContext::walk_may_lead_further(car) {
                work.push(car);
            }
            if super::RuntimeContext::walk_may_lead_further(cdr) {
                if beam_term::prelude::is_list(cdr) {
                    match super::super::gc::dereferenceable_pointer(cdr) {
                        Some(next) => {
                            item = cdr;
                            address = next;
                            continue;
                        }
                        None => break,
                    }
                }
                work.push(cdr);
            }
            break;
        }
    }
    false
}

#[cfg(test)]
mod tests;
