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

//! **Test-only** structural counters for the native baseline tier.
//!
//! This module exists to *diagnose* where a baseline-tier run spends its time; it is
//! deliberately **not** part of the tier's behaviour and it is **absent from every
//! production build**: the whole module is declared behind
//! `#[cfg(test)]` (`super::mod`'s `#[cfg(test)] mod counters;`) and every call site
//! is individually `#[cfg(test)]`-gated, so `cargo build -p beam_engine` compiles
//! with the counters — and the code that would touch them — entirely gone
//! (zero-cost by construction, not by a runtime check).
//!
//! # What is counted
//!
//! * **Per-`Instr`-variant deopts** — [`record_step_one`] (from
//!   [`jit_step_one`](super::runtime::jit_step_one)) and [`record_step_ext`] (from
//!   [`jit_step_ext`](super::runtime)), each attributed to the opcode at the
//!   `(func, pc)` the helper ran by looking the instruction up in the **active**
//!   module's code (the same module the helper executes the step against).
//! * **Block entries / driver round-trips** — [`record_block_entry`]/
//!   [`record_block_entered`] from `mod::enter_compiled`, and
//!   [`record_driver_round_trip`]/[`record_driver_interp_step`] from `mod::drive`
//!   (every block-ABI word the driver had to decode, classified).
//! * **Prologue position/publish calls** — [`record_set_position`] /
//!   [`record_publish_y`] / [`record_refresh_y`] from the three prologue helpers.
//! * **`Y`-window traffic** — [`record_y_access`] (every emitted guarded `Y`-slot read
//!   or write, with its emitted byte length) and [`record_y_invalidation`] (every
//!   in-block helper call after which a later instruction of the same block still
//!   accesses a `Y` slot — the site that must re-publish the window), plus, from
//!   [`record_block`], the `Y`-touching block count and the same-block **slot-reuse**
//!   count ([`Counters::y_slot_repeats`]) a single-slot `Y`-value cache could at most
//!   save.
//! * **`call_ext*` resolution outcomes** — [`record_ext_resolved`] (native transfer)
//!   / [`record_ext_declined`] (the host's `Ok(None)`, the once-only seam) /
//!   [`record_ext_fallback`] (the whole instruction re-run through `jit_step_one`).
//! * **Register-cache traffic** — [`record_spill`] (every store of a cached
//!   x-register back into `machine.x`) and [`record_fill`] (every load of a cached
//!   x-register out of `machine.x`), each classified by its site, plus
//!   [`record_block`] once per emitted block (its IR size, cache size, write-set size,
//!   the allocator's live-range statistics — candidate-register count and the selected
//!   intervals' total/max length — and native byte length).
//!
//! # One run, three register-cache spill models (and two fill models)
//!
//! Both directions of the cache are **partial** — `spill` stores a subset of the block's
//! cache and `fill` reloads the registers the continuation can still observe
//! (`Allocation::fill_slots`, its block-local liveness unioned with the write set) — so a
//! single measured run has to yield *every* model's count at once. Each `record_*` sees
//! only the count it was handed (that is all `spill`/`fill` know per call), so it records
//! that count and queues the site; [`record_block`] — which is handed the whole
//! `Allocation` — then attributes the two models only the block can see to each of the
//! block's queued sites. From one run, **three** spill models:
//!
//! * **written-set** `= Σ_sites written.len()` = [`Counters::spill_stores_written`] — the
//!   cache registers the block may have written ([`Allocation::written`], a subset of the
//!   cache). This is the cleanest model the emitter could emit without a per-register
//!   dirty analysis: it still stores a cached register the block wrote and then left
//!   holding its old `machine.x` value.
//! * **`spill`'s own model** `= Σ_sites n` over the counts [`record_spill`] was handed =
//!   [`Counters::spill_stores`] — "the stores
//!   [`spill`](super::emit::regs::spill) currently emits". At the present commit `spill`
//!   passes `allocation.written.len()`, so this **equals** `spill_stores_written`; a
//!   companion allocator change will make `spill` pass the **dirty** set instead (only
//!   registers whose in-flight machine value was overwritten), at which point
//!   `spill_stores_written − spill_stores` is exactly the redundant ("clean") stores that
//!   change removes.
//! * **full-cache** (the model before the partial spill) `= Σ_sites slots.len()` =
//!   [`Counters::spill_stores_full`];
//!
//! and **two** fill models:
//!
//! * **liveness** `= Σ_sites fill_slots(pc).len()` = [`Counters::fill_registers`];
//! * **full-cache** `= Σ_sites slots.len()` = [`Counters::fill_registers_full`].
//!
//! # Two-phase use (timing vs counting)
//!
//! Recording is a per-thread switch ([`set_enabled`], default **off**), so a caller
//! can take a *clean* timing pass (no `format!`, no bookkeeping — the record
//! functions return on a single `Cell` read) and then a *counting* pass with
//! recording on. The counter block itself ([`Counters`]) is thread-local, so a test
//! running in parallel with others never sees their counts.
#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use beam_loader::{Instr, Module};

use super::emit::{Allocation, FillSite, SpillSite};

thread_local! {
    /// Whether this thread is currently recording (default: off — see the module docs).
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    /// This thread's counter block.
    static COUNTERS: RefCell<Counters> = RefCell::new(Counters::default());
    /// The spill sites emitted since the last [`record_block`], so the block that
    /// emitted them can attribute the two models it alone can see to them — the
    /// **written-set** store count (Σ `written.len()`) and the **full-cache** store count
    /// (Σ `slots.len()`), both of which need the block's `Allocation`. Drained by
    /// [`record_block`].
    static PENDING_SPILLS: RefCell<Vec<SpillSite>> = const { RefCell::new(Vec::new()) };
    /// The fill sites emitted since the last [`record_block`] — the same trick as
    /// [`PENDING_SPILLS`], for the **full-cache** reload count the previous `fill` model
    /// would have emitted. Drained by [`record_block`].
    static PENDING_FILLS: RefCell<Vec<FillSite>> = const { RefCell::new(Vec::new()) };
    /// The `Y` slots already accessed in the block currently being emitted — the
    /// same-block reuse detector behind [`Counters::y_slot_repeats`]. Drained by
    /// [`record_block`], because a block's `Y` accesses all precede its own
    /// `record_block` (a block that fails to compile simply leaks its slots into the
    /// next block's set, which only ever *over*-counts reuses in a diagnostic).
    static Y_SLOTS_SEEN: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

/// A snapshot of the baseline tier's structural counters.
///
/// One field per event class; the `*_by_variant`/`*_by_word` maps attribute the
/// per-instruction deopts and the driver round-trips.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counters {
    /// `jit_step_one` invocations (the per-instruction deopt).
    pub(crate) step_one: u64,
    /// `jit_step_ext` invocations (the `call_ext*` deopt carrying the host's answer).
    pub(crate) step_ext: u64,
    /// `jit_ext_call` invocations that resolved a callee for this machine.
    pub(crate) ext_call_resolved: u64,
    /// `jit_ext_call` invocations the host declined (`Ok(None)`).
    pub(crate) ext_call_declined: u64,
    /// `jit_ext_call` invocations that fell back to the full interpreter step.
    pub(crate) ext_call_fallback: u64,
    /// `enter_compiled` lookups (one per driver iteration that asked for native code).
    pub(crate) block_entry_lookups: u64,
    /// Of those, how many found a compiled entry (an IR block start or a sub-block).
    pub(crate) block_entry_hits: u64,
    /// Of those, how many were actually entered (the register-file guard passed).
    pub(crate) block_entry_natives: u64,
    /// Block-ABI words the driver had to decode (a block ran and returned).
    pub(crate) driver_round_trips: u64,
    /// Driver iterations with no native entry: one interpreter step each.
    pub(crate) driver_interp_steps: u64,
    /// `jit_set_position` calls (every Y-free block prologue).
    pub(crate) set_position: u64,
    /// `jit_publish_y` calls (every block prologue whose body touches a `Y` slot).
    pub(crate) publish_y: u64,
    /// `jit_refresh_y` calls (the gated in-block Y-window re-publish).
    pub(crate) refresh_y: u64,
    /// Emitted **guarded Y-slot reads** ([`super::emit::emit_guarded_y_load`]): one
    /// frame-window guard plus one load, per inlined `Y` source.
    pub(crate) y_loads: u64,
    /// Emitted **guarded Y-slot writes** ([`super::emit::emit_guarded_y_store`]): one
    /// frame-window guard plus one store, per inlined `Y` destination.
    pub(crate) y_stores: u64,
    /// The emitted bytes those guarded Y accesses occupy (the guard plus its access
    /// word), summed over the whole compilation — the traffic a Y-value cache would have
    /// to reduce.
    pub(crate) y_bytes: u64,
    /// Emitted blocks whose body accesses a `Y` slot
    /// ([`Allocation::y_last`](super::emit::Allocation::y_last)): the blocks a per-block
    /// Y-value cache would have to publish (guard + load) the slot in.
    pub(crate) y_blocks: u64,
    /// Emitted Y-window **refresh** sites: the in-block helper calls after which a later
    /// instruction of the *same* block still accesses a `Y` slot
    /// ([`Allocation::refresh_y_after`](super::emit::Allocation::refresh_y_after)). The
    /// static twin of the runtime [`refresh_y`](Counters::refresh_y) count, and the number
    /// of times a Y-value cache would have to invalidate + reload.
    pub(crate) y_invalidations: u64,
    /// Emitted `Y`-slot accesses that re-touch a slot **already accessed earlier in the
    /// same block** — the upper bound on what a per-block single-slot `Y`-value cache
    /// could save: such a cache cannot forward across a block exit (the exit teardown
    /// destroys the cache registers) and cannot survive an in-block helper, so only
    /// intra-block reuse of one slot is exploitable.
    pub(crate) y_slot_repeats: u64,
    /// The per-opcode deopt attribution: variant name -> count.
    pub(crate) deopts_by_variant: BTreeMap<String, u64>,
    /// The driver round-trips by the word class they returned (`HALT`/`ERROR`/
    /// `ENTER`/`WAIT`/`GOTO`).
    pub(crate) round_trips_by_word: BTreeMap<String, u64>,
    /// `emit_block` calls (IR blocks **and** continuation sub-blocks).
    pub(crate) blocks: u64,
    /// Total IR instructions the emitted blocks cover (`end - start`, summed).
    pub(crate) block_ir_instructions: u64,
    /// Total native bytes the emitted blocks produced (prologue through `ret`).
    pub(crate) block_emitted_bytes: u64,
    /// Σ over blocks of the cache size (`Allocation::slots.len()`).
    pub(crate) block_cache_slots: u64,
    /// Σ over blocks of the write-set size (`Allocation::written.len()`).
    pub(crate) block_written_slots: u64,
    /// Σ over blocks of the allocator's **candidate-register count**
    /// (`Allocation::live_candidates`): how many x-registers the interval scan ranked.
    pub(crate) block_live_candidates: u64,
    /// Σ over blocks of the selection's **total live length**
    /// (`Allocation::live_total`) — the residency quality the linear scan maximises.
    pub(crate) block_live_total: u64,
    /// The **longest** selected live interval over all blocks
    /// (`max` of `Allocation::live_max`).
    pub(crate) block_live_max: u64,
    /// Blocks by write-set size: how many blocks spill `0`, `1`, `2`, … registers on an
    /// exit under the written-set model.
    pub(crate) blocks_by_written_slots: BTreeMap<u64, u64>,
    /// Blocks by cache size (the full-cache model's per-exit store count).
    pub(crate) blocks_by_cache_slots: BTreeMap<u64, u64>,
    /// Spill sites by kind ([`SpillSite`]) — the number of `spill` calls.
    pub(crate) spill_calls_by_site: BTreeMap<SpillSite, u64>,
    /// Spill **stores** by kind under the model
    /// [`spill`](super::emit::regs::spill) currently emits: Σ of the counts
    /// [`record_spill`] was handed. At the present commit that is `written.len()`, so it
    /// equals [`spill_stores_written_by_site`](Counters::spill_stores_written_by_site).
    pub(crate) spill_stores_by_site: BTreeMap<SpillSite, u64>,
    /// Spill **stores** by kind under the **written-set** model: Σ `written.len()`.
    pub(crate) spill_stores_written_by_site: BTreeMap<SpillSite, u64>,
    /// Spill **stores** by kind under the **full-cache** model: Σ `slots.len()`.
    pub(crate) spill_stores_by_site_full: BTreeMap<SpillSite, u64>,
    /// Total spill stores under the model
    /// [`spill`](super::emit::regs::spill) currently emits (the counts it passes to
    /// [`record_spill`]). At the present commit this is the written set, so it equals
    /// [`spill_stores_written`](Counters::spill_stores_written); after the companion
    /// allocator change it will be Σ |dirty|, and
    /// `spill_stores_written − spill_stores` the redundant ("clean") stores removed.
    pub(crate) spill_stores: u64,
    /// Total spill stores under the **written-set** model (Σ `written.len()`).
    pub(crate) spill_stores_written: u64,
    /// Total spill stores under the **full-cache** model (Σ `slots.len()`).
    pub(crate) spill_stores_full: u64,
    /// Fill sites by kind ([`FillSite`]) — the number of `fill` calls.
    pub(crate) fill_calls_by_site: BTreeMap<FillSite, u64>,
    /// Fill register loads by kind under the liveness model: Σ `fill_slots(pc).len()`.
    pub(crate) fill_registers_by_site: BTreeMap<FillSite, u64>,
    /// Fill register loads by kind under the previous full-cache model: Σ `slots.len()`.
    pub(crate) fill_registers_by_site_full: BTreeMap<FillSite, u64>,
    /// Total fill register loads under the liveness model (Σ `fill_slots(pc).len()`).
    pub(crate) fill_registers: u64,
    /// Total fill register loads under the previous full-cache model (Σ `slots.len()`).
    pub(crate) fill_registers_full: u64,
    /// **Static census.** Every instruction the emitter's lowering dispatch
    /// ([`super::emit::emit_block`]) **visited**, keyed by variant name: one count per
    /// IR instruction per emitted block, so a continuation sub-block counts its range a
    /// second time. This is the census' denominator.
    pub(crate) instr_instances: BTreeMap<String, u64>,
    /// **Static census.** Every instruction the dispatch did **not** lower — the `_`
    /// arm plus each `try_inline_*` refusal — keyed by variant name. This is the
    /// census' numerator, and it is a *compile-time* decision: a run-time guard
    /// rejection of a shape that **is** inlined (a `try_inline_*`'s internal
    /// `emit_step_and_continue`) is not counted here, only by the run-time
    /// [`deopts_by_variant`](Counters::deopts_by_variant).
    pub(crate) instr_deopts: BTreeMap<String, u64>,
}

/// The thread-local counter block, or `None` when this thread is not recording.
fn with<R>(f: impl FnOnce(&mut Counters) -> R) -> Option<R> {
    if !ENABLED.with(Cell::get) {
        return None;
    }
    Some(COUNTERS.with(|c| f(&mut c.borrow_mut())))
}

/// Turn recording on or off for this thread (default off — see the module docs).
pub(crate) fn set_enabled(on: bool) {
    ENABLED.with(|e| e.set(on));
}

/// Zero this thread's counters (recording state is left as it is).
pub(crate) fn reset() {
    COUNTERS.with(|c| *c.borrow_mut() = Counters::default());
    PENDING_SPILLS.with(|p| p.borrow_mut().clear());
    PENDING_FILLS.with(|p| p.borrow_mut().clear());
    Y_SLOTS_SEEN.with(|s| s.borrow_mut().clear());
}

/// This thread's counters, cloned.
pub(crate) fn snapshot() -> Counters {
    COUNTERS.with(|c| c.borrow().clone())
}

/// The opcode name a `(func, pc)` names in `module`, or a placeholder when the
/// address is out of range (which cannot happen for a real deopt — the helper only
/// runs an instruction the driver located in that module).
fn variant_at(module: &Module, func: u32, pc: u32) -> String {
    match module
        .functions
        .get(func as usize)
        .and_then(|function| function.code.get(pc as usize))
    {
        Some(instr) => variant_name(instr),
        None => format!("<out-of-range {func}:{pc}>"),
    }
}

/// The variant name of an `Instr`: the leading identifier of its `Debug` rendering
/// (`Move { .. }` -> `Move`, `Send` -> `Send`). Test-only, so the allocation and the
/// `Debug` formatting are acceptable.
///
/// A `Test` is keyed by its **kind** (`Test(IsEqExact)`, `Test(IsTuple)`, …) because the
/// inline subset decides a `test` predicate by predicate: the per-kind split is what
/// makes "which `test` shapes still deopt" readable from the counter table.
pub(crate) fn variant_name(instr: &Instr) -> String {
    if let Instr::Test { kind, .. } = instr {
        return format!("Test({kind:?})");
    }
    let rendered = format!("{instr:?}");
    let end = rendered
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rendered.len());
    rendered[..end].to_string()
}

/// Attribute a `jit_step_one` deopt to the opcode at `(func, pc)`.
pub(crate) fn record_step_one(module: &Module, func: u32, pc: u32) {
    with(|c| {
        c.step_one += 1;
        let name = variant_at(module, func, pc);
        *c.deopts_by_variant.entry(name).or_insert(0) += 1;
    });
}

/// Attribute a `jit_step_ext` (`call_ext*` deopt) to the opcode at `(func, pc)`.
pub(crate) fn record_step_ext(module: &Module, func: u32, pc: u32) {
    with(|c| {
        c.step_ext += 1;
        let name = variant_at(module, func, pc);
        *c.deopts_by_variant.entry(name).or_insert(0) += 1;
    });
}

/// A `jit_ext_call` that resolved a callee for this machine.
pub(crate) fn record_ext_resolved() {
    with(|c| c.ext_call_resolved += 1);
}

/// A `jit_ext_call` the host declined (`Ok(None)`).
pub(crate) fn record_ext_declined() {
    with(|c| c.ext_call_declined += 1);
}

/// A `jit_ext_call` that fell back to the full interpreter step.
pub(crate) fn record_ext_fallback() {
    with(|c| c.ext_call_fallback += 1);
}

/// An `enter_compiled` lookup; `hit` is whether a compiled entry was found.
pub(crate) fn record_block_entry(hit: bool) {
    with(|c| {
        c.block_entry_lookups += 1;
        if hit {
            c.block_entry_hits += 1;
        }
    });
}

/// An `enter_compiled` lookup that was actually entered (the register-file guard passed).
pub(crate) fn record_block_entered() {
    with(|c| c.block_entry_natives += 1);
}

/// One block-ABI word the driver decoded, classified by the word class.
pub(crate) fn record_driver_round_trip(word: i64) {
    with(|c| {
        c.driver_round_trips += 1;
        *c.round_trips_by_word.entry(word_class(word).to_string()).or_insert(0) += 1;
    });
}

/// One driver iteration with no native entry (one interpreter step).
pub(crate) fn record_driver_interp_step() {
    with(|c| c.driver_interp_steps += 1);
}

/// A `jit_set_position` prologue call.
pub(crate) fn record_set_position() {
    with(|c| c.set_position += 1);
}

/// A `jit_publish_y` prologue call (position store + Y-window publish).
pub(crate) fn record_publish_y() {
    with(|c| c.publish_y += 1);
}

/// A `jit_refresh_y` call (the gated in-block Y-window refresh).
pub(crate) fn record_refresh_y() {
    with(|c| c.refresh_y += 1);
}

/// One emitted guarded Y-slot access: `load` distinguishes a read
/// ([`emit_guarded_y_load`](super::emit::emit_guarded_y_load)) from a write
/// ([`emit_guarded_y_store`](super::emit::emit_guarded_y_store)), `slot` is the access's
/// frame-relative `Y` index (the same-block reuse detector for
/// [`Counters::y_slot_repeats`]), and `bytes` the emitted length of its frame-window
/// guard plus its access word.
pub(crate) fn record_y_access(load: bool, slot: u32, bytes: u64) {
    if !ENABLED.with(Cell::get) {
        return;
    }
    let repeat = Y_SLOTS_SEEN.with(|seen| {
        let mut seen = seen.borrow_mut();
        if seen.contains(&slot) {
            true
        } else {
            seen.push(slot);
            false
        }
    });
    COUNTERS.with(|c| {
        let c = &mut *c.borrow_mut();
        if load {
            c.y_loads += 1;
        } else {
            c.y_stores += 1;
        }
        c.y_bytes += bytes;
        if repeat {
            c.y_slot_repeats += 1;
        }
    });
}

/// One emitted Y-window refresh site (see [`Counters::y_invalidations`]).
pub(crate) fn record_y_invalidation() {
    with(|c| c.y_invalidations += 1);
}

/// **Static census.** One instruction the emitter's lowering dispatch visited (see
/// [`Counters::instr_instances`]). Recorded at the top of [`emit_block`]'s per-instruction
/// loop, so it counts a continuation sub-block's range a second time.
pub(crate) fn record_instr_instance(name: &str) {
    with(|c| *c.instr_instances.entry(name.to_string()).or_insert(0) += 1);
}

/// **Static census.** One instruction the dispatch did **not** lower — the `_` arm or a
/// `try_inline_*` refusal (see [`Counters::instr_deopts`]).
pub(crate) fn record_instr_deopt(name: &str) {
    with(|c| *c.instr_deopts.entry(name.to_string()).or_insert(0) += 1);
}

/// One `spill` call: `n` cached x-registers are stored back into `machine.x`, at `site`.
///
/// `n` is whatever [`spill`](super::emit::regs::spill) currently emits — the model
/// [`Counters::spill_stores`] reports (the block's **written** set at the present
/// commit). The **written-set** and **full-cache** store counts need the block's
/// `Allocation`, which only the enclosing block has, so the site is queued for
/// [`record_block`] to attribute them.
pub(crate) fn record_spill(site: SpillSite, written_len: u64) {
    if with(|c| {
        *c.spill_calls_by_site.entry(site).or_insert(0) += 1;
        *c.spill_stores_by_site.entry(site).or_insert(0) += written_len;
        c.spill_stores += written_len;
    })
    .is_none()
    {
        return;
    }
    PENDING_SPILLS.with(|p| p.borrow_mut().push(site));
}

/// One `fill` call: `n` cached x-registers are loaded out of `machine.x`, at `site`.
///
/// `fill` is **partial** ([`super::emit::regs::fill`]): it reloads only the registers a
/// helper at that `pc` can still be observed through (the continuation's block-local
/// liveness unioned with the block's write set), so `n` is the **new** load count. The
/// **old** (full-cache) count is `slots.len()`, which only the enclosing block knows — so
/// the site is queued for [`record_block`] to attribute it, exactly as
/// [`record_spill`] does.
pub(crate) fn record_fill(site: FillSite, n: u64) {
    if with(|c| {
        *c.fill_calls_by_site.entry(site).or_insert(0) += 1;
        *c.fill_registers_by_site.entry(site).or_insert(0) += n;
        c.fill_registers += n;
    })
    .is_none()
    {
        return;
    }
    PENDING_FILLS.with(|p| p.borrow_mut().push(site));
}

/// One emitted block: its IR size (`ir_len`), the `allocation` it was compiled with,
/// and the native `bytes` it produced.
///
/// Besides the per-block sums/histograms, this **drains** the block's queued spill
/// sites ([`record_spill`]) and adds the block's **write-set size** (the written-set
/// model) and its **cache size** (the full-cache model) to each, so one recorded run
/// yields all three spill-store totals — the count [`spill`](super::emit::regs::spill)
/// itself emitted, the written-set count and the full-cache count (see the module
/// docs).
///
/// It also folds in the allocation's **live-range statistics**
/// ([`Allocation::live_candidates`]/[`live_total`](Allocation::live_total)/
/// [`live_max`](Allocation::live_max)), the allocator's own view of how much liveness the
/// selection covers, counts the block as `Y`-touching when its body accesses a `Y` slot,
/// and drains the block's `Y`-slot set ([`Y_SLOTS_SEEN`]) so the next block's
/// [`record_y_access`] reuse detection starts fresh.
pub(crate) fn record_block(ir_len: u64, allocation: &Allocation, bytes: u64) {
    if !ENABLED.with(Cell::get) {
        return;
    }
    let cache_slots = allocation.slots.len() as u64;
    let written_slots = allocation.written.len() as u64;
    let pending = PENDING_SPILLS.with(|p| std::mem::take(&mut *p.borrow_mut()));
    let pending_fills = PENDING_FILLS.with(|p| std::mem::take(&mut *p.borrow_mut()));
    Y_SLOTS_SEEN.with(|s| s.borrow_mut().clear());
    COUNTERS.with(|c| {
        let c = &mut *c.borrow_mut();
        c.blocks += 1;
        c.block_ir_instructions += ir_len;
        c.block_emitted_bytes += bytes;
        if allocation.y_last.is_some() {
            c.y_blocks += 1;
        }
        c.block_cache_slots += cache_slots;
        c.block_written_slots += written_slots;
        c.block_live_candidates += u64::from(allocation.live_candidates);
        c.block_live_total += u64::from(allocation.live_total);
        c.block_live_max = c.block_live_max.max(u64::from(allocation.live_max));
        *c.blocks_by_cache_slots.entry(cache_slots).or_insert(0) += 1;
        *c.blocks_by_written_slots.entry(written_slots).or_insert(0) += 1;
        for site in pending {
            *c.spill_stores_written_by_site.entry(site).or_insert(0) += written_slots;
            c.spill_stores_written += written_slots;
            *c.spill_stores_by_site_full.entry(site).or_insert(0) += cache_slots;
            c.spill_stores_full += cache_slots;
        }
        for site in pending_fills {
            *c.fill_registers_by_site_full.entry(site).or_insert(0) += cache_slots;
            c.fill_registers_full += cache_slots;
        }
    });
}

/// The word class of a block-ABI word (`HALT`/`ERROR`/`ENTER`/`WAIT`/`GOTO`).
fn word_class(word: i64) -> &'static str {
    if word == super::HALT {
        "HALT"
    } else if word == super::ERROR {
        "ERROR"
    } else if word == super::ENTER {
        "ENTER"
    } else if super::decode_wait(word).is_some() {
        "WAIT"
    } else {
        "GOTO"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The variant name is the leading identifier of the `Debug` rendering, except
    /// that a `Test` is keyed by its kind.
    #[test]
    fn variant_names_are_the_opcode_identifier() {
        assert_eq!(variant_name(&Instr::Send), "Send");
        assert_eq!(
            variant_name(&Instr::Move {
                src: beam_loader::Operand::X(0),
                dst: beam_loader::Operand::X(1),
            }),
            "Move"
        );
        assert_eq!(
            variant_name(&Instr::Test {
                kind: beam_loader::TestKind::IsEqExact,
                fail: beam_loader::Label(0),
                args: vec![beam_loader::Operand::X(0), beam_loader::Operand::X(1)],
            }),
            "Test(IsEqExact)"
        );
        assert_eq!(
            variant_name(&Instr::Test {
                kind: beam_loader::TestKind::IsTuple,
                fail: beam_loader::Label(0),
                args: vec![beam_loader::Operand::X(0)],
            }),
            "Test(IsTuple)"
        );
    }

    /// The word classes distinguish the four sentinels plus a plain continuation.
    #[test]
    fn word_classes_distinguish_the_sentinels() {
        assert_eq!(word_class(super::super::HALT), "HALT");
        assert_eq!(word_class(super::super::ERROR), "ERROR");
        assert_eq!(word_class(super::super::ENTER), "ENTER");
        assert_eq!(word_class(super::super::encode_wait(1, 2)), "WAIT");
        assert_eq!(word_class(super::super::encode_target(1, 2)), "GOTO");
    }

    /// Recording is off by default, and `set_enabled(true)` turns it on.
    #[test]
    fn recording_defaults_off_and_round_trips() {
        set_enabled(false);
        reset();
        record_set_position();
        assert_eq!(snapshot().set_position, 0, "recording is off by default");

        set_enabled(true);
        record_set_position();
        record_step_one(&Module::default(), 0, 0);
        let snap = snapshot();
        assert_eq!(snap.set_position, 1);
        assert_eq!(snap.step_one, 1);

        set_enabled(false);
        reset();
        assert_eq!(snapshot(), Counters::default());
    }

    /// One recorded run yields **all three** spill models: the stores
    /// [`spill`](super::emit::regs::spill) currently emits, the written-set stores
    /// (`Allocation::written`) and the full-cache stores — the latter two attributed by
    /// the enclosing `record_block`, which owns the cache and write-set sizes.
    #[test]
    fn one_run_yields_both_the_written_and_the_full_cache_spill_counts() {
        use crate::jit::emitter::Reg;

        set_enabled(true);
        reset();

        // A block caching 4 x-registers but writing only 2 of them.
        let allocation = Allocation {
            slots: vec![(0, Reg::R12), (1, Reg::R13), (2, Reg::R14), (3, Reg::R15)],
            written: vec![0, 3],
            y_last: None,
            live_candidates: 6,
            live_total: 12,
            live_max: 5,
            ..Default::default()
        };
        // Three fill sites in the block: the prologue's whole-cache load (4) and two
        // partial post-helper refills (1 and 0 registers — the liveness model).
        record_fill(FillSite::Prologue, allocation.slots.len() as u64);
        record_fill(FillSite::Helper, 1);
        record_fill(FillSite::Helper, 0);
        record_spill(SpillSite::Helper, allocation.written.len() as u64);
        record_spill(SpillSite::Exit, allocation.written.len() as u64);
        record_block(6, &allocation, 128);

        let snap = snapshot();
        // `spill`'s own model: 2 + 2 stores (it passes written.len() today).
        assert_eq!(snap.spill_stores, 4, "Σ the counts spill passed, per site");
        assert_eq!(snap.spill_calls_by_site.get(&SpillSite::Helper), Some(&1));
        assert_eq!(snap.spill_stores_by_site.get(&SpillSite::Helper), Some(&2));
        assert_eq!(snap.spill_stores_by_site.get(&SpillSite::Exit), Some(&2));
        // Written-set model: 2 + 2 = 4 (derived by `record_block` from written.len()).
        assert_eq!(
            snap.spill_stores_written, 4,
            "written-set: Σ written.len() over spill sites"
        );
        assert_eq!(
            snap.spill_stores_written_by_site.get(&SpillSite::Helper),
            Some(&2)
        );
        assert_eq!(
            snap.spill_stores_written_by_site.get(&SpillSite::Exit),
            Some(&2)
        );
        // At this commit `spill` passes the written set, so the two models coincide; the
        // `spill`-vs-written difference (the redundant clean stores a dirty set removes)
        // is therefore 0.
        assert_eq!(snap.spill_stores, snap.spill_stores_written);
        assert_eq!(
            snap.spill_stores_written - snap.spill_stores,
            0,
            "no companion dirty-set change yet, so no redundant stores are removed"
        );
        // Full-cache model: 2 spill sites × a 4-register cache = 8 stores.
        assert_eq!(
            snap.spill_stores_full, 8,
            "full-cache: Σ slots.len() over spill sites"
        );
        assert_eq!(snap.spill_stores_by_site_full.get(&SpillSite::Helper), Some(&4));
        assert_eq!(snap.spill_stores_by_site_full.get(&SpillSite::Exit), Some(&4));
        // New (partial) fill model: 4 + 1 + 0 loads.
        assert_eq!(snap.fill_registers, 5, "new: Σ fill_slots(pc).len()");
        assert_eq!(snap.fill_calls_by_site.get(&FillSite::Prologue), Some(&1));
        assert_eq!(snap.fill_calls_by_site.get(&FillSite::Helper), Some(&2));
        // Old (full-cache) fill model: 3 fill sites × a 4-register cache = 12 loads.
        assert_eq!(
            snap.fill_registers_full, 12,
            "old: Σ slots.len() over fill sites"
        );
        assert_eq!(
            snap.fill_registers_by_site_full.get(&FillSite::Prologue),
            Some(&4)
        );
        assert_eq!(
            snap.fill_registers_by_site_full.get(&FillSite::Helper),
            Some(&8)
        );
        // Per-block view.
        assert_eq!(snap.blocks, 1);
        assert_eq!(snap.block_ir_instructions, 6);
        assert_eq!(snap.block_emitted_bytes, 128);
        assert_eq!(snap.block_cache_slots, 4);
        assert_eq!(snap.block_written_slots, 2);
        assert_eq!(snap.blocks_by_cache_slots.get(&4), Some(&1));
        assert_eq!(snap.blocks_by_written_slots.get(&2), Some(&1));
        // The allocator's live-range statistics ride along with the block.
        assert_eq!(snap.block_live_candidates, 6);
        assert_eq!(snap.block_live_total, 12);
        assert_eq!(snap.block_live_max, 5);

        set_enabled(false);
        reset();
        assert_eq!(snapshot(), Counters::default());
    }
}
