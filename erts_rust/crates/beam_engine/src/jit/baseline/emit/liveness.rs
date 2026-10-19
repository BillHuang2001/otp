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

//! Block-local liveness for the baseline JIT's register cache: **which cached
//! x-registers a post-helper [`fill`](super::regs::fill) must reload**.
//!
//! An in-block helper call *is* the interpreter: it reads `machine.x` (so the block's
//! inline prefix must be spilled into it first — [`super::emit_helper_call`]) and may
//! write **any** x-register (so a cached register may be stale the moment the helper
//! returns). Re-loading the whole cache afterwards was the previous model; only the
//! registers the *continuation* can still observe need it:
//!
//! * a register a **later instruction of the same block reads through its machine
//!   register** — the inline lowerings read the cache register, never `machine.x`, so
//!   such a register must be reloaded. [`fill_sets`] computes exactly that set,
//!   block-locally and conservatively (a register is live until a later instruction
//!   writes it: `live = (live \ writes) ∪ reads`);
//! * a register the block **may have diverged from `machine.x` at that very `pc`**
//!   ([`super::regalloc::cacheable_writes`] of the instruction *at* the helper's `pc`).
//!   This is not liveness: it is the join rule. A guarded lowering that rejects branches to
//!   a `jit_step_one` fallback which *joins* the block's fast path (the `move` module's
//!   Y-operand fallback, for instance): on the fast path the destination's register is
//!   authoritative and `machine.x` stale, and on the fallback path `machine.x` is
//!   authoritative and the register stale. Reloading the write set on the fallback path
//!   makes the two agree — the register then holds exactly `machine.x` — so a later
//!   [`spill`](super::regs::spill) of that register (the dirty-set walk marks it dirty from
//!   this `pc` on) stores the fresh word on both paths.
//!
//! The join rule is **`pc`-local, not block-global**: the divergence a helper call can leave
//! behind is exactly the write set of the instruction that call executes. A register the
//! block writes *elsewhere* is not part of the merge — either it is read after `pc` (so it
//! is in `live`) or its cached value is dead there (the dirty-set walk kills it at its own
//! writing instruction and no store can carry a stale word out of the block). Narrowing the
//! term from the block-wide union to `cacheable_writes(pc)` is what makes a
//! helper-instruction-heavy block — a bitstring decoder, where ~90 `Bs*` helper calls
//! follow one inline `move` — refill only what it can observe, instead of reloading every
//! register the block ever wrote at every helper site.
//!
//! Nothing is live at the **end** of the block: the exit spill has already made
//! `machine.x` authoritative for every register the block diverged, a register this
//! block never diverged still holds its `machine.x` value, and the successor (a
//! compiled block's prologue, or one interpreter step) loads what it needs out of
//! `machine.x`. A cached register is therefore never carried across a block boundary,
//! which is what makes the backwards walk below start from the empty set rather than
//! from the full cache.
//!
//! # Why a partially-filled register may simply be dropped
//!
//! A cached register outside the fill set at `pc` is left holding a word `machine.x` may
//! no longer match. It is never observed: a later **read** through its machine register
//! makes the register live at `pc` (so it *is* reloaded), and a later **store** of it is
//! a [`spill`](super::regs::spill), which only ever stores a **dirty** register — one an
//! inline lowering wrote since the last syncing event, at which point the machine
//! register holds that lowering's own (fresh) word. So the stale word can only be
//! overwritten, never read *and* never carried out of the block.

use beam_loader::{Instr, Module};

use crate::interpreter::LabelMap;

use super::regalloc::{cacheable_reads, cacheable_writes};
use super::Allocation;

/// The block's register liveness, from one backwards walk: for every instruction index of
/// the block `[start, end)`, the **x-registers a helper call at that index must reload**
/// out of `machine.x` before the block continues, **and** the block's **live-in** set — the
/// cached registers its prologue loads.
///
/// Index `i` of the returned fill table is the fill set of a helper call at
/// `pc = start + i` (which is what every helper site passes to
/// [`fill`](super::regs::fill)), so a caller resolves one with `sets[(pc - start) as usize]`;
/// a slice shorter than the pc offset has no computed liveness and must be treated as
/// **the whole cache** (see [`Allocation::fill_slots`]).
///
/// The fill set is `(live ∪ writes(pc)) ∩ slots`, where `live` is the block-local liveness
/// of the instruction *after* the helper (the interpreter has already executed the
/// instruction at `pc`, so the continuation starts at `pc + 1`), `writes(pc)` is the inline
/// write set of the instruction **at** `pc` (the `pc`-local join rule in the module docs)
/// and `slots` the block's cache. The set is an over-approximation of what the continuation
/// actually reads — a register it also lists but never reads is reloaded with the value it
/// already holds — never an under-approximation.
///
/// The walk steps *reads* back (`live = (live \ writes) ∪ reads`), so once every
/// instruction has been stepped over `live` is the block's **live-in** set: the registers
/// the block can still observe before it writes them — exactly the registers the prologue
/// must load. It is filtered to the block's cache on the way out. A cached register whose
/// status this walk cannot decide is *in* the set (a shape may claim a register it never
/// reads), so the prologue loads it: a conservative extra load is a missed optimization,
/// never a wrong result. A cached register the block writes before it reads is deliberately
/// left **unloaded** — the writing instruction sets its machine register (never
/// `machine.x`), and a later store of that register only happens while it is dirty, i.e.
/// after such a write.
pub(super) fn fill_sets(
    module: &Module,
    labels: &LabelMap,
    code: &[Instr],
    start: usize,
    end: usize,
    allocation: &Allocation,
) -> (Vec<Vec<u32>>, Vec<u32>) {
    let end = end.min(code.len());
    let start = start.min(end);
    let cached: Vec<u32> = allocation.slots.iter().map(|(x, _)| *x).collect();
    let mut sets: Vec<Vec<u32>> = vec![Vec::new(); end - start];
    // `live`: the x-registers a still-later instruction of this block reads before
    // writing them. Empty at the block end; the block's live-in set once the loop is
    // done (see the module docs).
    let mut live: Vec<u32> = Vec::new();
    for i in (start..end).rev() {
        // The instruction's **own** inline write set: the only cached registers a helper at
        // this pc can leave diverged from `machine.x` (the `pc`-local join rule).
        let writes = cacheable_writes(module, labels, &code[i]);
        // The fill set of a helper at `pc = i`: the registers the continuation reads, plus
        // every cached register this instruction's own fallback may have diverged.
        sets[i - start] = cached
            .iter()
            .copied()
            .filter(|x| live.contains(x) || writes.contains(x))
            .collect();
        // Step back over instruction `i` — what it writes is killed, what it reads is
        // generated (an over-approximation of "reads": a register a shape claims but
        // never actually reads is loaded, which the write set covers anyway).
        for x in &writes {
            live.retain(|v| v != x);
        }
        for x in cacheable_reads(module, labels, &code[i]) {
            if !live.contains(&x) {
                live.push(x);
            }
        }
    }
    // What is left is the block's live-in set, filtered to its cache: the registers the
    // prologue loads.
    let entry = cached.iter().copied().filter(|x| live.contains(x)).collect();
    (sets, entry)
}
