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

//! The per-collection **reused buffers**: [`CollectionScratch`], the only owner of the three
//! allocations one rooted collection needs — the flat root slice, the drained-mailbox list and the
//! off-heap reachability set.
//!
//! A steady-state collection runs at every heap admission (and the collector's own walk runs again
//! at the collection's off-heap tail), so a per-round allocation of these three buffers is pure
//! overhead on the hottest path in the crate. [`crate::process::Process`] therefore owns one
//! [`CollectionScratch`] for its lifetime and lends the buffers out of it on every collection:
//!
//! * `roots` — the flat root slice of [`crate::process::Process::run_rooted_collection`] (the
//!   registers/stack/extra roots, each queued message's three envelope words, the dictionary, the
//!   saved BIF/NIF arguments and the pending signals) **and** of the read-only scan gather
//!   ([`crate::process::Process::sweep_off_heap_binaries_reachable`]);
//! * `drained` — the messages [`crate::process::Process::run_rooted_collection`] takes out of the
//!   mailbox while their envelope words are roots, and re-installs afterwards in FIFO order;
//! * `reachable` — the payload pointers an off-heap reachability walk admits
//!   ([`crate::process::Process::sweep_off_heap_binaries_reachable`] and the same sweep at the tail
//!   of every rooted collection), which decide which process-owned refc binaries are retained.
//!
//! # [`CollectionScratch`] — the reused, and therefore clearable, collection state
//!
//! Reuse is only sound because every buffer is **cleared before it is seeded**, and the stale
//! content of each one is a real hazard, not stale bookkeeping:
//!
//! * a stale **root word** left in `roots` is a **wrong root set**: a word the collection treats as
//!   live, so it is relocated — reading it out of a from-space a previous collection has already
//!   evacuated or freed — and written back into a root region, and (through the same slice) counted
//!   by the off-heap reachability sweep as naming a payload;
//! * a stale [`Message`] left in `drained` is a **duplicated / dangling message**: a message owns a
//!   [`beam_message::Fragments`] chain and the off-heap refcounts of the objects it copied, so
//!   re-enqueuing one would duplicate a mailbox entry and double-drop/double-hold its refcounts —
//!   while the message the mailbox really lost is gone;
//! * a stale `usize` left in `reachable` retains an off-heap payload **nothing names any more** — a
//!   leak of a whole binary payload (the mirror of the dangle the walk exists to avoid).
//!
//! The clear is therefore **unconditional and happens before the gather/drain loop admits
//! anything**, in every entry point: [`CollectionScratch::begin`] empties all three buffers and
//! lends them back as a [`ScratchBuffers`], which is the **only** way to reach them (the fields are
//! private to this module and [`CollectionScratch::new`] starts empty), so there is no way to
//! construct a dirty scratch or to seed a buffer before it has been cleared.
//!
//! Two consequences worth stating explicitly:
//!
//! * **An `Err` return is harmless.** A collection action that fails (a pinned minor whose promotion
//!   does not fit, an `ensure_capacity` whose growth is refused) leaves `roots`/`drained` populated —
//!   the write-back steps still run, but the buffers are not emptied. That is fine precisely because
//!   the *next* entry point clears them before it seeds anything; nothing ever reads a buffer it did
//!   not just fill.
//! * **Clearing is what makes the reuse invisible.** The buffers are a pure allocation saving: every
//!   collection reports exactly what a freshly constructed process would (`tests/collection_scratch_reuse.rs`
//!   pins this from outside, for all three buffers).
//!
//! This mirrors `beam_gc`'s walk scratch (`beam_gc::reachable::Scratch`: one allocation owned by the
//! owner of the loop, every entry point clearing the whole used prefix before admitting anything),
//! so the two halves of a collection follow one discipline.

use std::collections::HashSet;

use beam_message::Message;
use beam_term::prelude::Eterm;

/// The three buffers one rooted collection reuses: the flat root slice, the drained-mailbox list and
/// the off-heap reachability set — see the module docs for why every entry point clears them.
///
/// One instance is owned by a [`crate::process::Process`] for its whole lifetime
/// ([`crate::process::Process::with_policy_and_priority`] builds it empty, allocating nothing). The
/// fields are private: the buffers are reachable only through [`CollectionScratch::begin`].
pub(crate) struct CollectionScratch {
    /// The flat root slice a collection gathers (see
    /// [`crate::process::Process::run_rooted_collection`] for the region list and order).
    roots: Vec<Eterm>,
    /// The messages taken out of the mailbox while their envelope words are roots.
    drained: Vec<Message>,
    /// The off-heap payload pointers (`RefcBinary::data_ptr` as `usize`) an off-heap reachability
    /// walk admitted, i.e. the payloads to **retain**.
    reachable: HashSet<usize>,
}

/// The borrowed, **already cleared** buffers of a [`CollectionScratch`] — the value
/// [`CollectionScratch::begin`] returns.
///
/// The three fields are disjoint borrows of the scratch's own fields, so a caller may hold all three
/// at once (and, in the collection path, hold `roots`/`drained` while driving the gather, the
/// collection action and the write-back). Safe Rust only — the compiler proves the borrows disjoint.
pub(crate) struct ScratchBuffers<'a> {
    /// The (empty) root slice.
    pub(crate) roots: &'a mut Vec<Eterm>,
    /// The (empty) drained-mailbox list.
    pub(crate) drained: &'a mut Vec<Message>,
    /// The (empty) off-heap reachability set.
    pub(crate) reachable: &'a mut HashSet<usize>,
}

impl CollectionScratch {
    /// An empty scratch: three empty containers, **allocating nothing** (the heap sizing happens on
    /// the first collection, which is also the first time the buffers are cleared).
    pub(crate) fn new() -> CollectionScratch {
        CollectionScratch {
            roots: Vec::new(),
            drained: Vec::new(),
            reachable: HashSet::new(),
        }
    }

    /// **Clear, then lend.** Empties all three buffers — `roots`, `drained` and `reachable` — and
    /// returns them as disjoint mutable borrows.
    ///
    /// This is the only way to reach the buffers, so the clear cannot be skipped: whatever a previous
    /// collection (or a previous failed one) left behind is dropped here, *before* the caller seeds
    /// anything. See the module docs for what each stale buffer would otherwise cost.
    pub(crate) fn begin(&mut self) -> ScratchBuffers<'_> {
        self.roots.clear();
        self.drained.clear();
        self.reachable.clear();
        ScratchBuffers {
            roots: &mut self.roots,
            drained: &mut self.drained,
            reachable: &mut self.reachable,
        }
    }
}

impl Default for CollectionScratch {
    fn default() -> CollectionScratch {
        CollectionScratch::new()
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the clear-before-lend contract: `begin` empties every buffer (keeping the
    //! capacities, so the reuse really is an allocation saving) and the lent borrows are the
    //! scratch's own buffers.

    use super::*;
    use beam_message::Message;
    use beam_term::prelude::make_small;

    #[test]
    fn a_fresh_scratch_is_empty() {
        let mut scratch = CollectionScratch::new();
        let ScratchBuffers {
            roots,
            drained,
            reachable,
        } = scratch.begin();
        assert!(roots.is_empty());
        assert!(drained.is_empty());
        assert!(reachable.is_empty());
    }

    #[test]
    fn begin_clears_every_buffer_before_lending_it() {
        let mut scratch = CollectionScratch::new();
        {
            let ScratchBuffers {
                roots,
                drained,
                reachable,
            } = scratch.begin();
            roots.push(make_small(1));
            roots.push(make_small(2));
            drained.push(Message::new(make_small(3), make_small(4)));
            reachable.insert(0xdead_beef);
        }

        // The next entry point sees empty buffers again — and the same allocations (the capacities
        // survive the clear, which is the whole point of the reuse).
        let ScratchBuffers {
            roots,
            drained,
            reachable,
        } = scratch.begin();
        assert!(roots.is_empty(), "a stale root word is a wrong root set");
        assert!(
            drained.is_empty(),
            "a stale message would be duplicated/dangling on re-install"
        );
        assert!(
            reachable.is_empty(),
            "a stale payload pointer would retain nothing-named memory"
        );
        assert!(
            roots.capacity() >= 2,
            "the root capacity survives the clear (nothing was reallocated away)"
        );
        assert!(
            drained.capacity() >= 1,
            "the drain capacity survives the clear"
        );
        assert!(
            reachable.capacity() > 0,
            "the reachability set's capacity survives the clear"
        );
    }

    /// The clear is what makes reuse sound, so it must happen **before** the caller seeds: seeding
    /// twice into the same scratch through two `begin` calls leaves only the second round's words.
    #[test]
    fn a_second_begin_does_not_keep_the_first_rounds_words() {
        let mut scratch = CollectionScratch::new();
        {
            let buffers = scratch.begin();
            buffers.roots.push(make_small(11));
        }
        let buffers = scratch.begin();
        buffers.roots.push(make_small(22));
        assert_eq!(buffers.roots.as_slice(), &[make_small(22)]);
        assert!(buffers.drained.is_empty());
        assert!(buffers.reachable.is_empty());
    }
}
