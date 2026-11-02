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

//! The **table-less route's** BIF-result store: how a container result is moved off the pooled
//! BIF heap when there is no scheduled process to move it onto.
//!
//! # Two re-homing destinations, one decision
//!
//! [`RuntimeContext::call_registered`](super::RuntimeContext::call_registered) moves a container
//! result off the pooled BIF heap and then rewinds the pool to that call's `mark`. Where the
//! result goes depends on **who is running**:
//!
//! * a scheduled process — its own **scanned young heap**
//!   ([`move_result_into_process_heap`](super::RuntimeContext::move_result_into_process_heap)), so
//!   the copy is collectable and relocated by its collector;
//! * nobody (`RuntimeContext::new`: the `-eval` evaluator, `-s`/`-run`, and every single-pass
//!   `run_*` entry) — the runtime's long-lived store, [`Runtime::eval_heap`] plus the off-heap
//!   table [`Runtime::eval_off_heap`] ([`RuntimeContext::move_result_into_runtime_store`], this
//!   module). That is the very store [`Runtime::stabilize_term`] already uses for the `-eval`
//!   route's own terms (a non-byte-aligned bitstring, and the final value), so no second store
//!   and no second off-heap table is introduced.
//!
//! # Why a table-less route needs a rewind at all
//!
//! Without one the pool's frontier only ever marched: a container result stayed on the region it
//! was built on for the run's whole life, so the bump pointer crossed each region's fixed
//! capacity and every page it crossed was touched for the first time — a per-call first-touch
//! page-fault tax that grew with the call count. The escaped-payload store is the destination that
//! makes the rewind possible: a copy is *cheap per surviving word* and, unlike the pool, it never
//! holds a call's **temporaries**.
//!
//! # Soundness
//!
//! The rewind is allowed only when nothing reachable from the caller's still-live environment
//! points into the words being freed. Two things guarantee it here:
//!
//! 1. the caller rewinds **only** after this function reported a copy — a refused copy (a
//!    malformed graph, an unresolvable off-heap container, a store that could not be grown) leaves
//!    both the result and the pool exactly as they were, so the result keeps pointing into the
//!    pool's own words;
//! 2. the destination is **pointer-stable and never reset** ([`Runtime::eval_heap`], whose retired
//!    generations [`Runtime::grow_eval_heap`] keeps alive), so the copy can be stored in the
//!    evaluator's environment and read back later in the same expression.
//!
//! The store is a runtime-level region, i.e. outside every process's scanned heap, so the copy
//! also opens the escape-store descent gate ([`Runtime::note_escaped_store_roots`], the same call
//! [`Runtime::stabilize_term`] makes) — over-approximate by design, and free of charge for the
//! overwhelmingly common case where the latch is already set.
//!
//! A **hand-through** (the short-circuit above) is sound for the rewind by the same argument, but
//! it is not sound for the **GC** on the root test alone: the store and the arena are never-scanned,
//! so a container handed through with scanned-heap children would be relocated out from under
//! itself. The short-circuit therefore also runs
//! [`super::calls::dest::graph_reaches_scanned_heap`] and hands a term through only when its whole
//! reachable graph is pointer-stable; a graph that names any scanned word falls through to the deep
//! copy, which re-roots it on this store.

use beam_term::Eterm;

use super::calls::{copy_result_fresh, copy_shortfall_words, Rehomed};
use super::runtime_context::RuntimeContext;
use crate::runtime::EVAL_STORE_GROWTH_FLOOR;
impl RuntimeContext<'_> {
    /// Moves a registered BIF's container result off the pooled BIF heap onto the runtime's
    /// long-lived `-eval`/single-pass store ([`Runtime::eval_heap`] + [`Runtime::eval_off_heap`]),
    /// returning the copy — or `None` when it cannot be re-homed, in which case the caller leaves
    /// both the result and the pool exactly as they were.
    ///
    /// This is the **table-less** arm of the result re-homing (see the module docs): there is no
    /// process heap to move the result onto, so without it nothing rewinds the pool and its
    /// frontier marches through every region for the run's whole life.
    ///
    /// # The copy shares what it can
    ///
    /// The copy is [`copy_result_fresh`] — the same single-traversal copier the process route
    /// uses, with its own fresh, empty destination table (so the copier's fast path stays
    /// eligible however much the shared store already holds) — and its off-heap registrations are
    /// **adopted** into [`Runtime::eval_off_heap`] on success. A refc-binary payload that no
    /// source table owns is flattened into an on-heap bitstring by the copier itself (byte
    /// faithful), exactly as on the process route.
    ///
    /// # Growth uses the copier's own shortfall
    ///
    /// A store that cannot hold the graph is grown **once**, by the word count the copier's own
    /// preflight already reported ([`copy_shortfall_words`]) — never by a second traversal — and
    /// the copy is retried against the fresh region. Region replacement never moves a term, so
    /// everything the store already held stays valid (see [`Runtime::grow_eval_heap`]).
    pub(super) fn move_result_into_runtime_store(
        &mut self,
        term: Eterm,
        pool: Option<&beam_alloc::Heap>,
    ) -> Rehomed {
        let RuntimeContext { runtime, .. } = self;
        // THE SHORT-CIRCUIT (store arm, [`super::calls::dest`]): the copy has exactly one job —
        // move the result **off** the pooled region the call allocated from, because the tail
        // rewinds that region once this returns. Two answers mean there is nothing to move:
        //
        // * the root already lives in this store (the live `eval_heap` region, or one a growth
        //   step retired), so copying it would only replace a live, store-owned object by a
        //   structural twin;
        // * `pool` is known and the root lies on **no pooled region at all**, so the graph it
        //   reaches predates this call and the rewind cannot invalidate a word of it. This is the
        //   arm that keeps a no-op container BIF's term identity observable on the
        //   `-eval`/`-s`/`-run` route (`erts_debug:same(maps:put(a,V,M), M)` ⇒ `true`), where the
        //   evaluator's own containers stand on the engine **arena**, not on this store.
        //
        // A **third** answer — the root standing on a pooled region — is now handed through too,
        // **by reference**, as `KeepPool`: the graph is pointer-stable (see the gate below), so the
        //   caller keeps the term exactly where the BIF built it and skips the rewind. Its words
        //   are never recycled: the next call's `mark` sits above them, and a region that the
        //   frontier later retires keeps its `used` and is never allocated over (see
        //   `super::calls::frontier` / `Runtime::put_bif_heap`). This is the arm that removes the
        //   unconditional deep-copy on the khepri keep-while fan-out (a ~2500-node state map
        //   rebuilt ~2500 times by `maps:put/3`-style path copying).
        //
        // `pool == None` is the caller saying it cannot vouch for the region the result came from
        // (the aliased-ETS `TermStore`, the `system_info` scratch region): then only the store half
        // can answer, and the copy runs as it always did.
        //
        // A hand-through on any answer is **not** enough on its own: the store, the pool and the
        // arena are pointer-stable but never-scanned, so a container whose root stands there but
        // whose children live on a scanned young heap would be relocated out from under itself by
        // that process's next collection (the copier hands such a root `Target::Foreign` and does
        // not descend). The guard below descends the whole reachable graph instead
        // ([`super::calls::dest::graph_reaches_scanned_heap`]): a graph that names **any** scanned
        // word falls through to the deep copy (which re-roots it on this never-reset store), while
        // a fully self-contained graph is handed through unchanged — which is what keeps a no-op
        // BIF's term identity observable. The escape-store descent gate is opened exactly as on the
        // copy path below: the store is outside every process's scanned heap, so a handed-through
        // container must keep the over-approximate "an escaped store may hold roots" latch set just
        // the same.
        let on_store = super::calls::dest::store_holds(runtime, term);
        let on_pool = match pool {
            Some(pool) => super::calls::dest::pool_holds(runtime, pool, term),
            None => false,
        };
        // Known region: hand through what the store already holds (`RewindSafe`), anything that is
        // not on a pooled region (`RewindSafe`, nothing to move), and a fully pointer-stable graph
        // that IS on a pooled region (`KeepPool`, skip the rewind). Unknown origin: only the store
        // half can answer.
        let hand_through = on_store || (pool.is_some() && !on_pool) || on_pool;
        if hand_through && !super::calls::dest::graph_reaches_scanned_heap(runtime, term) {
            runtime.note_escaped_store_roots(term);
            return if on_pool {
                Rehomed::KeepPool(term)
            } else {
                Rehomed::RewindSafe(term)
            };
        }
        // One source-payload snapshot for both attempts, built once and borrowed at each call site
        // (`SourcePayloads` is not `Copy`): the BIF-built refc-binary handles now held in
        // `eval_off_heap` (adopted before this copy-out) plus the runtime process's retained
        // mailbox families.
        let payloads = runtime.source_payloads(runtime.self_pid(), &[]);
        let needed = match copy_result_fresh(
            &mut runtime.eval_heap,
            &mut runtime.eval_off_heap,
            term,
            &payloads,
        ) {
            Ok(copied) => {
                runtime.note_escaped_store_roots(copied);
                return Rehomed::RewindSafe(copied);
            }
            Err(error) if !beam_message::is_message_heap_exhaustion(&error) => {
                // Not a capacity refusal — above all the copier's malformed-graph `badarg` or an
                // unresolvable off-heap container. Growing cannot fix it, so a retry would walk
                // the graph again and fail with the very same reason: leave the result (and the
                // pool) untouched.
                return Rehomed::Failed;
            }
            Err(error) => copy_shortfall_words(&error),
        };
        if runtime
            .grow_eval_heap(needed.max(EVAL_STORE_GROWTH_FLOOR))
            .is_err()
        {
            return Rehomed::Failed;
        }
        let copied = match copy_result_fresh(
            &mut runtime.eval_heap,
            &mut runtime.eval_off_heap,
            term,
            &payloads,
        ) {
            Ok(copied) => copied,
            Err(_) => return Rehomed::Failed,
        };
        runtime.note_escaped_store_roots(copied);
        Rehomed::RewindSafe(copied)
    }
}

#[cfg(test)]
mod tests;
