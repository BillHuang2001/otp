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

//! The per-process `save_calls` ring behind `erlang:process_info(Pid, last_calls)`.
//!
//! The C keeps a `struct saved_calls` on the PCB (`erl_process.h`, allocated by
//! `process_flag(save_calls, N)`, `bif.c:1902-1960`): a fixed ring of `N` `Export *` slots plus
//! a write cursor and a fill count, appended to by `save_calls(p, e)` (`erl_trace.c:1409-1423`)
//! and rendered **newest first** by `ERTS_PI_IX_LAST_CALLS` (`erl_bif_info.c:2183-2225`).
//!
//! This module is the ring itself — no terms, no runtime state. `beam_rt` keeps one per armed
//! pid ([`crate::Runtime::saved_calls`]) and renders it in
//! [`crate::native::memory`].
//!
//! # Owned copies, never terms
//!
//! The C stores an `Export *`, a pointer into the loaded-code area, and renders
//! `e->info.mfa.module`. This runtime's equivalent of a code-area name is a **shared atom-table
//! entry**, but a ring entry outlives the heap of the process that made the call (the ring is
//! queryable for **another** pid, and the caller's heap may be collected in between), so the
//! names are kept as owned `String`s copied at record time — the owned-copy rule for anything
//! that crosses a heap boundary. Arity is a plain `u32`. Nothing here is a term, so nothing here
//! needs rooting.
//!
//! # The recorded entry vocabulary
//!
//! [`SavedEntry::Mfa`] is a real recorded call — every `call_ext*` and every fun application
//! (the C's `dispatch_save_calls_export` / `dispatch_save_calls_fun`, `jit/x86/instr_call.rs:100`
//! and `jit/x86/instr_fun.cpp:159`). The three bare atoms are the C's pseudo entries, which
//! render as atoms instead of triples (`erl_bif_info.c:2204-2211`): `send`
//! (`save_calls(p, &exp_send)`, `bif.c:2452` and friends), `'receive'`
//! (`jit/beam_jit_common.cpp:1098`) and `timeout` (`:1251`).

use std::collections::VecDeque;

/// One ring entry: a recorded call's `{Module, Function, Arity}`, or one of the C's three
/// pseudo entries that render as a bare atom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SavedEntry {
    /// A recorded `call_ext*`/fun application.
    Mfa {
        /// The called module's name.
        module: String,
        /// The called function's name.
        function: String,
        /// The call's arity.
        arity: u32,
    },
    /// The pseudo entry `send`.
    ///
    /// The C records it from the send BIF's own arms (`save_calls(p, &exp_send)`,
    /// `bif.c:2452-2672`). This runtime's send funnel (`crate::context::send`) classifies a
    /// destination before it holds the runtime mutably, so the entry is not recorded yet — see
    /// the crate `CONTEXT.md` `## Known Issues`.
    #[allow(dead_code)]
    Send,
    /// The pseudo entry `'receive'`.
    Receive,
    /// The pseudo entry `timeout`.
    Timeout,
}

/// One process's armed `save_calls` ring — the C's `struct saved_calls`.
///
/// Entries are stored **oldest first**; [`Self::entries`] yields them in that order and the
/// renderer walks it in reverse, which is the oracle's newest-first list. Only ever created with
/// a non-zero capacity (`process_flag(save_calls, 0)` removes the ring instead).
#[derive(Debug)]
pub(crate) struct SavedCalls {
    entries: VecDeque<SavedEntry>,
    /// The ring's capacity, i.e. the armed `N`. Never zero.
    capacity: usize,
}

impl SavedCalls {
    /// A fresh, empty ring of `capacity` entries (the C's `scb->len = i`, `scb->cur = 0`,
    /// `scb->n = 0`).
    pub(crate) fn new(capacity: u16) -> Self {
        let capacity = usize::from(capacity).max(1);
        SavedCalls {
            // The ring is lazily grown: a 10 000-entry buffer must not cost 10 000 slots
            // before it holds anything, and only the first `capacity` pushes allocate.
            entries: VecDeque::with_capacity(capacity.min(64)),
            capacity,
        }
    }

    /// Re-arm at a new capacity, **discarding** what the ring held — the C's
    /// `ERTS_PROC_SET_SAVED_CALLS_BUF` installs a brand-new zeroed buffer, so a re-arm starts
    /// empty (measured on OTP 29.1.1: re-arming with `process_flag(save_calls, 30)` leaves only
    /// the querying call in the ring).
    pub(crate) fn rearm(&mut self, capacity: u16) {
        let capacity = usize::from(capacity).max(1);
        self.entries.clear();
        self.capacity = capacity;
    }

    /// The armed capacity.
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    /// The entries, **oldest first**.
    pub(crate) fn entries(&self) -> impl DoubleEndedIterator<Item = &SavedEntry> {
        self.entries.iter()
    }

    /// Append one entry, dropping the oldest once the ring is full (the C's
    /// `if (++scb->cur >= len) scb->cur = 0` wrap-around).
    fn push(&mut self, entry: SavedEntry) {
        while self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    /// Record a `{Module, Function, Arity}` call.
    pub(crate) fn push_mfa(&mut self, module: &str, function: &str, arity: u32) {
        self.push(SavedEntry::Mfa {
            module: module.to_string(),
            function: function.to_string(),
            arity,
        });
    }

    /// Record one of the bare-atom pseudo entries.
    pub(crate) fn push_pseudo(&mut self, entry: SavedEntry) {
        self.push(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(ring: &SavedCalls) -> Vec<String> {
        ring.entries()
            .map(|entry| match entry {
                SavedEntry::Mfa {
                    module,
                    function,
                    arity,
                } => format!("{module}:{function}/{arity}"),
                SavedEntry::Send => "send".to_string(),
                SavedEntry::Receive => "receive".to_string(),
                SavedEntry::Timeout => "timeout".to_string(),
            })
            .collect()
    }

    #[test]
    fn an_armed_ring_starts_empty_and_fills_oldest_first() {
        let mut ring = SavedCalls::new(4);
        assert_eq!(ring.capacity(), 4);
        assert!(names(&ring).is_empty());
        ring.push_mfa("erlang", "make_ref", 0);
        ring.push_pseudo(SavedEntry::Send);
        assert_eq!(names(&ring), ["erlang:make_ref/0", "send"]);
    }

    #[test]
    fn overflow_drops_the_oldest_entry() {
        let mut ring = SavedCalls::new(3);
        ring.push_mfa("a", "one", 0);
        ring.push_mfa("b", "two", 1);
        ring.push_mfa("c", "three", 2);
        ring.push_mfa("d", "four", 3);
        assert_eq!(names(&ring), ["b:two/1", "c:three/2", "d:four/3"]);
    }

    #[test]
    fn rearming_discards_the_previous_entries() {
        let mut ring = SavedCalls::new(8);
        ring.push_mfa("lists", "reverse", 1);
        ring.rearm(30);
        assert_eq!(ring.capacity(), 30);
        assert!(names(&ring).is_empty());
        ring.push_mfa("erlang", "process_info", 2);
        assert_eq!(names(&ring), ["erlang:process_info/2"]);
    }

    #[test]
    fn a_zero_capacity_ring_still_holds_one_entry() {
        // `SavedCalls::new(0)` is never constructed by the runtime (a zero size disarms), but
        // the ring must not say it can hold nothing while claiming to exist.
        let mut ring = SavedCalls::new(0);
        assert_eq!(ring.capacity(), 1);
        ring.push_mfa("erlang", "self", 0);
        assert_eq!(names(&ring), ["erlang:self/0"]);
    }
}
