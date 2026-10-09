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

use beam_common::Result;
use beam_term::{make_small, thing, Eterm, MAX_SMALL, MIN_SMALL};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::terms;

/// The spare **tail** of a reserved block, where a builder materializes an operand
/// that would otherwise need its own (collecting) heap allocation.
///
/// The builders reserve `block_words + extra` words in one rooted allocation and
/// hand the `extra`-word tail to the reader closure as a [`Slack`]. The closure may
/// then materialize a non-small integer operand **inside** the slack (via
/// [`Slack::integer`]) instead of performing a second — collecting — allocation that
/// would invalidate the reserved block: the block's own pointer is not a root, so a
/// second collection would strand it.
///
/// The slack is a plain cursor over the tail; it never touches the context heap.
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

    /// Materialize `value` into the slack: a small int when it fits, an **exact
    /// bignum** otherwise. Never allocates from the context heap.
    ///
    /// A tail too small for the bignum is a typed [`crate::error::unsupported`] —
    /// a caller sizing bug, never a wrong term.
    pub fn integer(&mut self, value: i64) -> Result<Eterm> {
        if (MIN_SMALL..=MAX_SMALL).contains(&value) {
            return Ok(make_small(value));
        }
        let need = thing::big_size_words(1);
        if self.remaining() < need {
            return Err(error::unsupported("slack: no room for a bignum operand"));
        }
        // `unsigned_abs` is total (including `i64::MIN`) and a non-small value has a
        // non-zero magnitude, so there is no leading zero digit to strip.
        let digits = [value.unsigned_abs()];
        // SAFETY: `remaining() >= big_size_words(1)` words are unused at `cursor`, so
        // `base + cursor` addresses a bignum-sized writable region inside the reserved
        // block, which `write_big` fully initialises (header + one `ErtsDigit`).
        let hp = unsafe { self.base.add(self.cursor) };
        self.cursor += need;
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
