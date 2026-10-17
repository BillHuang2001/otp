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

use beam_term::Eterm;

use super::calls::{copy_result_fresh, copy_shortfall_words};
use super::runtime_context::RuntimeContext;

/// The floor of a **replacement** store region, in words.
///
/// A copy that does not fit the current region replaces it with one twice as large
/// ([`Runtime::grow_eval_heap`]); the floor only matters for a store whose capacity is so small
/// that doubling would still be a single call's worth of words, and it keeps the region count
/// logarithmic rather than one region per oversized result.
const EVAL_STORE_GROWTH_FLOOR: usize = 256;

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
    pub(super) fn move_result_into_runtime_store(&mut self, term: Eterm) -> Option<Eterm> {
        let RuntimeContext { runtime, .. } = self;
        let needed = match copy_result_fresh(
            &mut runtime.eval_heap,
            &mut runtime.eval_off_heap,
            term,
        ) {
            Ok(copied) => {
                runtime.note_escaped_store_roots(copied);
                return Some(copied);
            }
            Err(error) if !beam_message::is_message_heap_exhaustion(&error) => {
                // Not a capacity refusal — above all the copier's malformed-graph `badarg` or an
                // unresolvable off-heap container. Growing cannot fix it, so a retry would walk
                // the graph again and fail with the very same reason: leave the result (and the
                // pool) untouched.
                return None;
            }
            Err(error) => copy_shortfall_words(&error),
        };
        runtime
            .grow_eval_heap(needed.max(EVAL_STORE_GROWTH_FLOOR))
            .ok()?;
        let copied = copy_result_fresh(
            &mut runtime.eval_heap,
            &mut runtime.eval_off_heap,
            term,
        )
        .ok()?;
        runtime.note_escaped_store_roots(copied);
        Some(copied)
    }
}

#[cfg(test)]
mod tests;
