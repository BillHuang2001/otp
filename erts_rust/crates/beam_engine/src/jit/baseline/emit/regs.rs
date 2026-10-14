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

//! The baseline JIT's **register cache**: which x-registers a block holds in machine
//! registers, and the two directions of that cache's contract.
//!
//! A block's [`Allocation`] fixes the cache's **membership for the whole block** (see
//! [`super`]'s module docs for why the membership must not change mid-block): an
//! x-register is either *always* held in its machine register — and written back before
//! the block leaves its frame — or *never* cached, so the two representations can never
//! disagree.
//!
//! # Which registers a [`spill`] stores, and which a [`fill`] reloads
//!
//! [`spill`] writes back only the cached registers the block **writes natively**
//! ([`Allocation::written`]) — the destination registers of the block's inline
//! lowerings. A cached register that no inline lowering of the block writes holds
//! exactly its `machine.x` value at every program point of the block: the prologue
//! loads it, no lowering diverges the two, and every post-helper [`fill`] that matters
//! re-syncs it after a helper — which *is* the interpreter, and may have written
//! `machine.x`. Storing such a register writes back the value it already holds, so
//! skipping it is a no-op. The write set is a **static superset** of the registers any
//! lowering actually writes, so an over-approximation costs a redundant store but can
//! never omit one.
//!
//! [`fill`] is **partial**: it reloads only the cached registers a helper at that `pc`
//! can still be observed through — the block-local liveness set of the continuation
//! unioned with the block's write set ([`super::liveness::fill_sets`], and see that
//! module for why the write set is part of the rule). A cached register left out of the
//! fill set keeps a stale word, which is safe because it is never observed: a later
//! **read** through that machine register would make the register live at that helper
//! (so it is in the set), and every **store** of a cached register stores exactly the
//! write set, which the fill set always contains.

use beam_common::Result;

use crate::jit::emitter::{CodeBuffer, Reg};

use super::operands::x_offset;

/// Which spill site a cached-register store came from (test-only bookkeeping — see
/// [`super::super::counters`]; the parameter itself costs nothing in production).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum SpillSite {
    /// The spill before an in-block helper call ([`super::emit_helper_call`]).
    Helper,
    /// The spill at a block exit (a [`super::link`] reached exit, or a tail exit).
    Exit,
}

/// Which fill site a cached-register load came from (test-only bookkeeping — see
/// [`super::super::counters`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum FillSite {
    /// The post-helper refill on a continue arm ([`super::emit_continue_check`], and the
    /// `test` helper's own `success` arm).
    Helper,
    /// The block prologue's load of the whole cache ([`super::emit_block`]).
    ///
    /// Constructed only from the test-gated prologue counter — in a production build
    /// the prologue loads the cache inline and no `fill` names this site.
    #[cfg(test)]
    Prologue,
}

/// A block's register allocation: the `(x-register index, machine register)` pairs the
/// prologue loads and the spill writes back.
#[derive(Default)]
pub(crate) struct Allocation {
    /// The slots, in assignment order (`src` before `dst` within a `move`).
    pub(crate) slots: Vec<(u32, Reg)>,
    /// The cached x-registers an inline lowering of the block **may write** — a subset
    /// of `slots`' indices, and exactly the registers [`spill`] stores.
    ///
    /// A cached register outside this set holds its `machine.x` value throughout the
    /// block (see the module docs), so a store of it would be a no-op.
    pub(crate) written: Vec<u32>,
    /// The **highest pc of the block whose instruction accesses a Y slot**, or `None`
    /// when no instruction of the block does.
    ///
    /// The emitter's two Y-window decisions both read it, and both are compile-time
    /// (so a Y-free block pays nothing at run time):
    ///
    /// * the **prologue** publishes the current frame's Y window iff `y_last.is_some()`
    ///   (a block that may read `JitRuntime::y_base`/`y_end`);
    /// * the **post-deopt refresh** ([`Allocation::refresh_y_after`]) is emitted only
    ///   when a still-later instruction accesses a Y slot.
    ///
    /// Only the arithmetic fast path reads the window (see the Y-operand contract in
    /// [`super::arith`]): an instruction "accesses a Y slot" exactly when
    /// [`super::arith::uses_y`] says so. A `move`/`get_list`/… with a Y operand is
    /// *not* a window access — it deopts to the interpreter, which reads
    /// `machine.stack` itself.
    pub(crate) y_last: Option<u32>,
    /// The block's **first pc** (`start`), the base `fill_sets` is indexed from.
    pub(crate) block_start: u32,
    /// The **fill set of every instruction of the block**, indexed by `pc - block_start`
    /// ([`super::liveness::fill_sets`]): the cached x-registers a helper call at that pc
    /// must reload ([`Allocation::fill_slots`]).
    ///
    /// Always as long as the block, so an index inside the block always resolves; an
    /// **empty** table (a synthetic `Allocation` a unit test built directly) means "no
    /// liveness was computed" and makes every fill set the whole cache.
    pub(crate) fill_sets: Vec<Vec<u32>>,
}

impl Allocation {
    /// The machine register holding x-register `x`, if it is cached.
    pub(crate) fn slot(&self, x: u32) -> Option<Reg> {
        self.slots
            .iter()
            .find(|(n, _)| *n == x)
            .map(|(_, reg)| *reg)
    }

    /// Whether this block's inline lowerings write cached x-register `x`.
    pub(crate) fn is_written(&self, x: u32) -> bool {
        self.written.contains(&x)
    }

    /// The registers a [`spill`] of this allocation stores (its **written** cache).
    pub(crate) fn spill_slots(&self) -> impl Iterator<Item = &(u32, Reg)> {
        self.slots.iter().filter(|(x, _)| self.is_written(*x))
    }

    /// The cached slots a post-helper [`fill`] at `pc` reloads, as
    /// `(x-register index, machine register)` pairs.
    ///
    /// The set is the block-local liveness of the continuation at `pc` unioned with the
    /// block's write set ([`super::liveness::fill_sets`]); a table that does not cover
    /// `pc` (an empty one — a synthetic allocation from a unit test) conservatively
    /// means the **whole** cache, which is the pre-liveness model.
    pub(crate) fn fill_slots(&self, pc: u32) -> Vec<(u32, Reg)> {
        match self
            .fill_sets
            .get(pc.wrapping_sub(self.block_start) as usize)
        {
            Some(set) => self
                .slots
                .iter()
                .filter(|(x, _)| set.contains(x))
                .copied()
                .collect(),
            None => self.slots.clone(),
        }
    }

    /// Whether a helper call made at `pc` — whose `Step::Next` continues the block
    /// in-block at `pc + 1` — must be followed by a Y-window refresh.
    ///
    /// True only when a later instruction of the block still accesses a Y slot: the
    /// helper ran one instruction in the interpreter, which may have pushed a frame,
    /// `trim`med one, or **reallocated** `machine.stack`, so the window the block
    /// published in its prologue may be stale. When no later instruction accesses a Y
    /// slot the refresh is not emitted at all — and when the helper leaves the block to
    /// the driver, the next block's prologue publishes instead, so no driver
    /// round-trip ever refreshes.
    pub(crate) fn refresh_y_after(&self, pc: u32) -> bool {
        matches!(self.y_last, Some(last) if last > pc)
    }
}

/// Write the block's **written** cached x-registers back into `machine.x` (`rbx` holds
/// the base).
///
/// Only [`Allocation::written`] is stored (see the module docs): a cached register no
/// inline lowering of the block writes still holds exactly its `machine.x` value, so
/// storing it would write back an unchanged word. `site` classifies the call for the
/// test-only structural counters and is otherwise unused.
pub(super) fn spill(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    site: SpillSite,
) -> Result<()> {
    let _ = site;
    #[cfg(test)]
    super::super::counters::record_spill(site, allocation.written.len() as u64);
    for (x, reg) in allocation.spill_slots() {
        buffer.mov_mr(Reg::Rbx, x_offset(*x), *reg)?;
    }
    Ok(())
}

/// Reload the cached x-registers a helper call at `pc` can still be observed through
/// (`rbx` holds the base of `machine.x`).
///
/// The mandatory counterpart of [`spill`] after a helper call: the helper ran the
/// interpreter on `machine.x`, so a cached register whose `machine.x` slot the helper
/// wrote is stale. Reloading only [`Allocation::fill_slots`] instead of the whole cache
/// is the block-local liveness model — a cached register the continuation cannot observe
/// is left stale on purpose (see the module docs and
/// [`super::liveness`]). `site` classifies the call for the test-only structural
/// counters and is otherwise unused.
pub(super) fn fill(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    site: FillSite,
    pc: u32,
) -> Result<()> {
    let _ = site;
    let slots = allocation.fill_slots(pc);
    #[cfg(test)]
    super::super::counters::record_fill(site, slots.len() as u64);
    for (x, reg) in slots {
        buffer.mov_rm(reg, Reg::Rbx, x_offset(x))?;
    }
    Ok(())
}
