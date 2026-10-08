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

//! Saved BIF/NIF arguments: [`SavedArgs`] — the extra per-process GC sub-rootset.
//!
//! The workspace root-set contract names `x[0..live)` + the whole Erlang stack (plus this
//! crate's mailbox payloads and [`crate::dict::Dict`]); the legacy C process additionally
//! roots **saved arguments** that live in the heap while an operation is in flight — a BIF
//! that has partially built a result, a `receive` that is matching, the arguments a NIF holds
//! across its call (`ErlSaveCallArgs` / `save_calls`, `erts/emulator/beam/erl_process.h`).
//! [`SavedArgs`] is the minimal in-heap root list for exactly those words.
//!
//! # Handoff: the fragment type is owned by a sibling
//!
//! The C also roots *heap fragments* (`ErlHeapFragment`, `heap_frags`) — off-heap heap
//! extensions used by cross-heap copies and, in the rewrite, by
//! `beam_message::OffHeapTable` (already a field on [`crate::process::Process`]). There is
//! **no** `ErlHeapFragment` type in the frozen sibling crates: both `beam_message` and
//! `beam_gc` document the heap-fragment rootset as a Milestone-1 follow-up, and the in-heap
//! message copy is handled through `beam_message::OffHeapTable`. This crate therefore does
//! **not** invent a duplicate fragment type; it stores saved arguments as a plain `Eterm`
//! root list. If fragmented message / heap-fragment support is later required, the missing
//! piece is a heap-fragment type owned by `beam_gc` (the collector + size walker) and consumed
//! by `beam_message` (the copy engine), which this crate would then reference here instead of
//! a flat word list.
//!
//! # Term ownership
//!
//! The stored words are terms resident in THIS process's young heap; the caller (the BIF/NIF
//! layer) copies them in, exactly as it does for [`crate::dict::Dict`]. `SavedArgs` never
//! allocates — it records words — so it must be scanned by
//! [`crate::process::Process::collect`], which it is (as the last region).

use beam_term::prelude::Eterm;

/// The saved-argument root list: a small `Vec` of live term words owned by the process heap.
///
/// Purely a rootset container — it holds words the collector must relocate and preserves
/// their order. See the module docs for the fragment-type handoff and the ownership contract.
pub struct SavedArgs {
    words: Vec<Eterm>,
}

impl SavedArgs {
    /// An empty saved-argument list.
    pub fn new() -> SavedArgs {
        SavedArgs { words: Vec::new() }
    }

    /// The number of saved words.
    pub fn len(&self) -> usize {
        self.words.len()
    }

    /// Whether nothing is saved.
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Drops every saved word.
    pub fn clear(&mut self) {
        self.words.clear();
    }

    /// Saves one more word.
    pub fn push(&mut self, word: Eterm) {
        self.words.push(word);
    }

    /// Removes and returns the most recently saved word, or `None` when nothing is saved.
    pub fn pop(&mut self) -> Option<Eterm> {
        self.words.pop()
    }

    /// The saved words, oldest first.
    pub fn as_slice(&self) -> &[Eterm] {
        &self.words
    }

    /// The saved words, oldest first, mutably.
    pub fn as_mut_slice(&mut self) -> &mut [Eterm] {
        &mut self.words
    }

    /// Appends every saved word to `out` and returns the **start offset** — the index in `out`
    /// at which this container's words begin.
    ///
    /// Pairing with [`SavedArgs::scatter_roots`] with the same `start` makes the two a clean
    /// round-trip.
    pub fn gather_roots(&self, out: &mut Vec<Eterm>) -> usize {
        let start = out.len();
        out.extend_from_slice(&self.words);
        start
    }

    /// Writes the (relocated) words back, starting at `start` — the offset returned by
    /// [`SavedArgs::gather_roots`].
    ///
    /// Defensive and total: it never panics. A `roots` slice too short to cover the whole list,
    /// or a `start` past its end, simply leaves the uncovered tail untouched (in practice the
    /// collector preserves the slice and the offsets are exact).
    pub fn scatter_roots(&mut self, roots: &[Eterm], start: usize) {
        let Some(available) = roots.len().checked_sub(start) else {
            // `start` is past the end of the slice: nothing to write, no panic.
            return;
        };
        let count = available.min(self.words.len());
        self.words[..count].copy_from_slice(&roots[start..start + count]);
    }
}

impl Default for SavedArgs {
    fn default() -> SavedArgs {
        SavedArgs::new()
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the saved-argument root list and its `gather_roots`/`scatter_roots`
    //! round-trip.

    use super::*;
    use beam_term::prelude::make_small;

    #[test]
    fn push_pop_len_and_clear() {
        let mut args = SavedArgs::new();
        assert!(args.is_empty());
        assert_eq!(args.len(), 0);
        assert_eq!(args.pop(), None);

        args.push(make_small(1));
        args.push(make_small(2));
        assert_eq!(args.len(), 2);
        assert_eq!(args.as_slice(), &[make_small(1), make_small(2)]);

        assert_eq!(args.pop(), Some(make_small(2)));
        assert_eq!(args.pop(), Some(make_small(1)));
        assert_eq!(args.pop(), None);

        args.push(make_small(3));
        args.clear();
        assert!(args.is_empty());
    }

    #[test]
    fn as_mut_slice_writes_through() {
        let mut args = SavedArgs::new();
        args.push(make_small(1));
        args.push(make_small(2));
        args.as_mut_slice()[0] = make_small(9);
        assert_eq!(args.as_slice(), &[make_small(9), make_small(2)]);
    }

    #[test]
    fn gather_and_scatter_round_trip() {
        let mut args = SavedArgs::new();
        args.push(make_small(1));
        args.push(make_small(2));
        args.push(make_small(3));

        let mut roots = vec![make_small(-1)]; // caller-owned prefix is preserved
        let start = args.gather_roots(&mut roots);
        assert_eq!(start, 1);
        assert_eq!(&roots[start..], &[make_small(1), make_small(2), make_small(3)]);

        // Simulate the collector relocating every root.
        roots[start..].copy_from_slice(&[make_small(11), make_small(22), make_small(33)]);
        args.scatter_roots(&roots, start);
        assert_eq!(args.as_slice(), &[make_small(11), make_small(22), make_small(33)]);
    }

    #[test]
    fn scatter_is_total_for_a_mismatched_slice() {
        let mut args = SavedArgs::new();
        args.push(make_small(1));
        args.push(make_small(2));

        // A one-word slice covers only the first entry; the rest is untouched, no panic.
        let short = [make_small(11)];
        args.scatter_roots(&short, 0);
        assert_eq!(args.as_slice(), &[make_small(11), make_small(2)]);

        // A `start` past the end is a no-op.
        args.scatter_roots(&short, 99);
        assert_eq!(args.as_slice(), &[make_small(11), make_small(2)]);
    }

    #[test]
    fn gather_of_an_empty_list_returns_the_current_len() {
        let args = SavedArgs::new();
        let mut roots = vec![make_small(7)];
        assert_eq!(args.gather_roots(&mut roots), 1);
        assert_eq!(roots, vec![make_small(7)]);
    }
}
