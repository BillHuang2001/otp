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

//! The **rooted allocation seam** the term builders reserve their storage
//! through, and the [`Slack`] tail they materialize operand terms into.
//!
//! # Why the reserve happens first
//!
//! A host with a **moving** collector
//! ([`ExecutionContext::alloc_words_rooted`](crate::context::ExecutionContext::alloc_words_rooted))
//! relocates every term reachable from the [`RootSet`](crate::context::RootSet) it is
//! handed (the machine's `x`/`stack`/`fr`) and rewrites those root slices in place.
//! Any `Eterm` **copy** the engine holds in a Rust local across that allocation is
//! therefore a stale pointer, and a `*mut Eterm` from a *previous* allocation is
//! dead. The frozen contract every builder in this module follows is therefore
//! **reserve, then read, then write**:
//!
//! 1. reserve the whole block **first** with *exactly one* rooted allocation
//!    ([`reserve`]) — a collection may run here, relocating the operands;
//! 2. then read every operand **from the machine's post-collection state** into the
//!    reserved block (a builder's reader closure reads operands with
//!    [`crate::operand::read_operands_slack`], never through a collecting call);
//! 3. then write the header/tag words.
//!
//! Nothing may allocate from the BEAM heap between the reserve and the final write;
//! the reader closure's contract is exactly that.
//!
//! An operand that would *itself* need a heap block (a non-small `integer`/`character`
//! operand → an exact bignum) is materialized into the reserved block's spare
//! **tail** through [`Slack`], so the whole build still costs a single allocation.
//!
//! # Carving a whole multi-node graph out of one reservation
//!
//! A build that produces *several* nodes (a `{M,F,A,Location}` stacktrace item and
//! its `[{file,File},{line,N}]` location, a list of such items, …) cannot use one
//! builder per node: each builder would reserve its **own** block, and a collection
//! under a later reservation would strand the nodes already written (their pointers
//! are not roots). Such a build therefore sizes the whole graph from **metadata**
//! before allocating, takes ONE reservation ([`reserve_slack`]) and carves every
//! node out of that single region with the [`Slack`]'s bump cursor —
//! [`Slack::alloc_words`] plus the [`Sink`](super::Sink) writers, which never touch
//! the context heap, so nothing can collect between the nodes.

use beam_common::Result;
use beam_term::{make_small, thing, Eterm, MAX_SMALL, MIN_SMALL};

use crate::context::{ExecutionContext, RootSet};
use crate::error;
use crate::machine::Machine;

/// The storage of a reserved block: the spare **tail** a builder materializes an
/// operand into, or the **whole** region a multi-node builder carves node-by-node.
///
/// The builders reserve `block_words + extra` words in one rooted allocation and
/// hand the `extra`-word tail to the reader closure as a [`Slack`]. The closure may
/// then materialize a non-small integer operand **inside** the slack (via
/// [`Slack::integer`]) instead of performing a second — collecting — allocation that
/// would invalidate the reserved block: the block's own pointer is not a root, so a
/// second collection would strand it.
///
/// The slack is a plain bump cursor; it never touches the context heap.
#[derive(Debug)]
pub struct Slack {
    /// The first word of the spare tail (inside the reserved block).
    base: *mut Eterm,
    /// How many words have been handed out so far.
    cursor: usize,
    /// How many words the tail holds.
    cap: usize,
}

impl Slack {
    /// A slack over `cap` words starting at `base`.
    ///
    /// # Safety
    ///
    /// `base` must point at `cap` exclusively-owned, writable words inside a block
    /// the caller has just reserved and will not hand out again.
    pub(crate) unsafe fn new(base: *mut Eterm, cap: usize) -> Slack {
        Slack {
            base,
            cursor: 0,
            cap,
        }
    }

    /// The words still unused in the tail.
    #[inline]
    pub fn remaining(&self) -> usize {
        self.cap - self.cursor
    }

    /// Bump-allocate `n` words out of the slack and return a pointer to them.
    ///
    /// This is the **carve** primitive a multi-node builder uses: it reserves the
    /// whole graph in one rooted allocation, wraps it in a [`Slack`] over the whole
    /// region ([`reserve_slack`]) and carves every node out of it. Nothing here
    /// touches the context heap, so a carve can never trigger a collection — which is
    /// exactly what keeps the nodes already written (whose pointers are not roots)
    /// alive.
    ///
    /// A slack too small for `n` words is a typed [`crate::error::unsupported`] — a
    /// caller sizing bug, never a wrong term.
    #[inline]
    pub(crate) fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        if self.remaining() < n {
            return Err(error::unsupported(
                "slack: reserved region too small for the node",
            ));
        }
        // SAFETY: `remaining() >= n` words are unused at `cursor`, so `base + cursor`
        // addresses `n` exclusively-owned, writable words inside the reserved block
        // (which the caller has not handed out twice).
        let hp = unsafe { self.base.add(self.cursor) };
        self.cursor += n;
        Ok(hp)
    }

    /// Materialize `value` into the slack: a small int when it fits, an **exact
    /// bignum** otherwise. Never allocates from the context heap.
    ///
    /// A tail too small for the bignum is a typed [`crate::error::unsupported`] —
    /// a caller sizing bug, never a wrong term.
    pub fn integer(&mut self, value: i64) -> Result<Eterm> {
        if (MIN_SMALL..=MAX_SMALL).contains(&value) {
            return Ok(make_small(value));
        }
        // `unsigned_abs` is total (including `i64::MIN`) and a non-small value has a
        // non-zero magnitude, so there is no leading zero digit to strip.
        let digits = [value.unsigned_abs()];
        let hp = self.alloc_words(thing::big_size_words(1))?;
        // SAFETY: `alloc_words` returned `thing::big_size_words(1)` exclusively-owned,
        // writable words, which `write_big` fully initialises (header + one
        // `ErtsDigit`).
        Ok(unsafe { thing::write_big(hp, value < 0, &digits) })
    }
}
/// Reserve `n` words for a term build through the **rooted** allocation seam.
///
/// This is the single admission point every builder in [`terms`] goes through: the
/// machine hands its whole live root set over (so a moving collector may run and
/// relocate the operands) and the returned pointer is the frozen
/// [`Machine::alloc_rooted`] storage (valid until the next collecting allocation).
/// `live` declares how many leading X registers are live; callers that have no
/// operand-derived count use [`crate::context::RootSet::ALL_LIVE`].
#[inline]
pub(crate) fn reserve(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    n: usize,
    live: u32,
) -> Result<*mut Eterm> {
    machine.alloc_rooted(ctx, n, live)
}

/// Reserve `n` words through the **rooted** seam and hand them back as a [`Slack`]
/// over the **whole** region — the entry point a multi-node builder carves from.
///
/// The one collection the reservation may run relocates the machine's roots, so a
/// caller must compute every node's *shape* (and read every operand) only
/// afterwards. `live` is fixed to [`RootSet::ALL_LIVE`]: a graph built from module
/// metadata has no operand-derived live-X count, and treating every register as live
/// is always a safe over-approximation for a copying collector.
#[inline]
pub fn reserve_slack(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    n: usize,
) -> Result<Slack> {
    let hp = reserve(machine, ctx, n, RootSet::ALL_LIVE)?;
    // SAFETY: `reserve` returned `n` exclusively-owned, writable words and the slack
    // is exactly those `n` words; the caller carves them, one node at a time, and
    // hands no word out twice.
    Ok(unsafe { Slack::new(hp, n) })
}