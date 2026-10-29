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
//! The per-runtime pool of idle [`DispatchMemo`]s — why a context's dispatch memo
//! **outlives** the context that filled it.
//!
//! # The problem this exists for
//!
//! [`DispatchMemo`] is *per context*, and a [`RuntimeContext`](super::RuntimeContext) is built
//! per scheduler slice ([`crate::runner`]'s `for_process_with_engines`) and per nested run. A
//! process that parks on `receive` and is resumed therefore re-enters with a **fresh, empty**
//! memo, so on the park-heavy message-passing shape (`tools/bench`'s `copy_bound` cases
//! `send_recv`/`send_recv_plain`) *every* slice pays the dispatch prologue again — and a miss is
//! not one cost but four: the FNV slot fold, the registry `lookup` (two SipHash probes of the
//! `beam_bifs` name maps), one [`beam_bifs::hook_flags::flags`] probe, and finally the entry's
//! own allocation (`Box<DispatchEntry>` + two `String`s) — measured by `valgrind --tool=callgrind`
//! at 6.12 `malloc`s per iteration of `send_recv_plain`, plus a drop that had to scan all
//! [`DISPATCH_MEMO_SLOTS`](super::dispatch) and free every box so filled.
//!
//! # Why recycling one is sound
//!
//! [`DispatchMemo`]'s own soundness argument
//! ([`DispatchMemo`]'s "Why a cached answer can never go stale") is that **every input the
//! answer is built from is immutable for the life of the runtime**: the `beam_bifs` `needs_*`
//! rosters are `&'static` tables folded into one `OnceLock` flag table, and
//! [`Runtime::registry`](crate::Runtime) is an `Rc<BifRegistry>` built once at boot with no
//! interior mutability. The memo's *lifetime* was chosen per context, never its *domain* — so
//! hand a memo to the next context of the **same** runtime and every slot still answers exactly
//! what the registry and the hook roster would answer now. The pool therefore hangs off
//! [`Runtime`](crate::Runtime) (like the baseline tier's compiled-block cache, whose validity
//! domain is the same one emulator instance, `Runtime::new`'s `clear_baseline_cache`) and never
//! outlives it: a second `Runtime` in one OS process starts with an empty pool, so no answer
//! computed against another runtime's registry can ever be read.
//!
//! Nothing observable changes. The memo answers only through
//! [`DispatchEntry::matches`](super::dispatch)'s full field-for-field key equality, so a reused
//! slot is either a hit for exactly that key or a miss; a memo that arrives already full of
//! another process's keys merely turns that process's misses into hits. The two things the memo
//! is *observed* by (the `BEAM_RT_PERF_COUNTERS` hit/miss tallies and the `last` hint) are
//! per-context statistics, not semantics.
//!
//! # How the memo gets back
//!
//! [`MemoSlot`] is the field [`RuntimeContext::dispatch_memo`] now has: it derefs to a
//! [`DispatchMemo`], so [`RuntimeContext::dispatch`](super::RuntimeContext) and the tests read
//! and write `ctx.dispatch_memo.slots[i]` unchanged, and its [`Drop`] hands the memo back to the
//! runtime's pool instead of freeing its boxes. (A `Drop` on `RuntimeContext` itself would be the
//! obvious alternative and cannot work: [`crate::context::calls`] destructures a context
//! **by value** to build its bridges, which a type with a `Drop` impl forbids.)
//!
//! The slot arrives holding `None` only if the pool was empty — the nested-run case, or a
//! context built before any memo came back — in which case it behaves exactly like the plain
//! `DispatchMemo::default()` it replaced: the first use allocates a fresh memo. A pool that is
//! full (or momentarily borrowed) likewise just drops the memo, which is the pre-pool behaviour.
//!
//! [`RuntimeContext::dispatch_memo`]: super::RuntimeContext
//! [`RuntimeContext::dispatch`]: super::RuntimeContext

use std::cell::RefCell;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;
use std::sync::OnceLock;

use super::dispatch::DispatchMemo;

/// How many idle memos one runtime keeps.
///
/// Contexts nest strictly (a nested run is built while the outer context is alive and dies
/// before it), so an idle stack of a few covers the deepest ordinary nesting. Past that a
/// nested context simply starts with a fresh memo — the pre-pool cost, never a wrong answer.
const MEMO_POOL_DEPTH: usize = 4;

/// The memo a [`MemoSlot`] that has already given its own away answers reads from.
///
/// Unreachable on a live context (the only take is [`MemoSlot`]'s own `Drop`), and harmless even
/// if it were reached: an all-empty memo misses, and a miss recomputes.
fn empty_memo() -> &'static DispatchMemo {
    static EMPTY: OnceLock<DispatchMemo> = OnceLock::new();
    EMPTY.get_or_init(DispatchMemo::default)
}

/// The idle [`DispatchMemo`]s of one [`Runtime`](crate::Runtime).
///
/// Cloned into every [`MemoSlot`] it hands out (an `Rc`, so a context costs one refcount bump
/// and can hand the memo back with no access to the runtime).
#[derive(Clone, Default)]
pub(crate) struct MemoPool {
    idle: Rc<RefCell<Vec<DispatchMemo>>>,
}

impl MemoPool {
    /// A slot for a new context: the most recently returned memo if there is one, else an empty
    /// slot (which behaves exactly like `DispatchMemo::default()`).
    pub(in crate::context) fn slot(&self) -> MemoSlot {
        let memo = self.idle.try_borrow_mut().ok().and_then(|mut idle| idle.pop());
        MemoSlot {
            memo,
            pool: Rc::clone(&self.idle),
        }
    }

    /// Park `memo` for the next context, or drop it when the pool is full.
    fn give(&self, memo: DispatchMemo) {
        if let Ok(mut idle) = self.idle.try_borrow_mut() {
            if idle.len() < MEMO_POOL_DEPTH {
                idle.push(memo);
            }
        }
    }
}

/// A [`DispatchMemo`] a [`RuntimeContext`](super::RuntimeContext) borrows from its runtime's
/// [`MemoPool`], returned to the pool when the context is dropped.
///
/// Derefs to the memo, so every reader and writer goes through it unchanged; see the module docs
/// for why recycling cannot change an answer.
pub(in crate::context) struct MemoSlot {
    /// The memo in use; `None` only for a slot the pool could not fill, and taken by `Drop`.
    memo: Option<DispatchMemo>,
    /// The pool this memo goes back to.
    pool: Rc<RefCell<Vec<DispatchMemo>>>,
}

impl Deref for MemoSlot {
    type Target = DispatchMemo;

    fn deref(&self) -> &DispatchMemo {
        self.memo.as_ref().unwrap_or_else(|| empty_memo())
    }
}

impl DerefMut for MemoSlot {
    fn deref_mut(&mut self) -> &mut DispatchMemo {
        self.memo.get_or_insert_with(DispatchMemo::default)
    }
}

impl Drop for MemoSlot {
    fn drop(&mut self) {
        if let Some(memo) = self.memo.take() {
            MemoPool {
                idle: Rc::clone(&self.pool),
            }
            .give(memo);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slot from an empty pool is `None` (an empty memo) and a fresh one is handed out by the
    /// next take once it comes back.
    #[test]
    fn a_returned_memo_goes_to_the_next_context() {
        let pool = MemoPool::default();
        {
            let mut first = pool.slot();
            assert!(first.memo.is_none(), "an empty pool hands out an empty slot");
            // Write through the `DerefMut` half, exactly as `dispatch` does.
            first.last = Some(3);
        }
        let second = pool.slot();
        assert_eq!(second.last, Some(3), "the memo came back with its hint");
        assert!(second.memo.is_some());
    }

    /// A full pool is not an error: the extra memos are simply dropped.
    #[test]
    fn the_pool_is_bounded() {
        let pool = MemoPool::default();
        let slots: Vec<MemoSlot> = (0..MEMO_POOL_DEPTH * 2)
            .map(|_| {
                let mut slot = pool.slot();
                // Materialize a memo through the `DerefMut` half, so every slot has one to give
                // back — a slot the empty pool left `None` gives nothing back.
                let _ = &mut *slot;
                slot
            })
            .collect();
        drop(slots);
        assert_eq!(pool.idle.borrow().len(), MEMO_POOL_DEPTH);
    }

    /// The empty-slot fallback answers a miss, never a wrong answer: it is an all-empty memo.
    #[test]
    fn an_empty_slot_reads_as_an_empty_memo() {
        let pool = MemoPool::default();
        let slot = pool.slot();
        assert!(slot.last.is_none());
        assert!(slot.slots.iter().all(Option::is_none));
    }
}
