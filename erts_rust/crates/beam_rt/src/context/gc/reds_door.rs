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
//! The **reduction/yield-boundary** collection point: a collection site that needs no
//! allocation request.
//!
//! The engine's heap-admission doors ([`super::ensure_heap`], [`super::ensure_heap_full`],
//! [`super::alloc_words_rooted`]) are reached only where the *code* has to allocate on the
//! process heap — `test_heap`/`allocate_heap`, and the rooted term-construction seam. A loop
//! whose body allocates **solely through registered BIFs**
//! (`_ = list_to_tuple(lists:seq(1, 400)), loop(N - 1)`) emits no such instruction, so it
//! reaches none of them: the unrooted [`super::alloc_words`] seam and the BIF-result copy-out
//! ([`RuntimeContext::move_result_into_process_heap`](crate::context::RuntimeContext)) can only
//! **grow** the nursery, never collect it. Such a workload therefore keeps its whole allocation
//! history resident — the nursery is a *chain* of the spaces a growth retired
//! ([`beam_gc::Collector::grow_nursery`]), whose summed capacity is the `heap_size` the C
//! reports as `ERTS_PI_IX_HEAP_SIZE` — and that figure climbs with the loop count where the
//! oracle's stays flat at its live set.
//!
//! This door closes that hole from the one place a slice always ends: the runner's pre-emption
//! arm, which parks a machine the reduction budget ran out on
//! ([`crate::runner::RuntimeRunner`]). It is *not* an allocation door — it is handed no request
//! — so the question it answers is purely "has this process allocated enough since the last
//! boundary to be worth collecting?", which is [`collect_at_reds_boundary`]'s two gates: a
//! **reduction interval** (a collection at most once per pre-emption) and an **occupancy
//! watermark** (only when the allocation area is nearly exhausted — the C's own
//! `ERTS_VBUMP_ALLOC_REDS` rule, see [`BOUNDARY_OCCUPANCY_NUM`]).
//!
//! # Why nothing here can lose a term
//!
//! Every refusal on the way is a typed no-op that leaves the heap exactly where it was, and the
//! one action is the same publish/collect/scatter hand-off the full heap-admission door runs
//! ([`super::publish_roots_full`] / [`super::scatter_roots_full`]): the caller hands the
//! machine's **whole** [`RootSet`] over, so the door publishes every pointer-bearing x slot, the
//! whole Erlang stack and the machine's live FR prefix, and scatters the relocated words back
//! into the very slots they came from. A published `fr_count` that disagrees with the park's
//! would mis-size the FR region, so the caller passes the park's own count (see the parameter
//! docs).
use beam_common::Result;
use beam_engine::context::RootSet;
use beam_gc::CollectionKind;
use beam_process::ProcessTable;
use crate::RuntimeContext;
use super::{machine_root_bounds, publish_roots_full, scatter_roots_full, valve_verdict};
/// How many reductions must have accumulated since this process's last boundary collection
/// before the next pre-emption may collect.
///
/// **One slice budget** (`beam_common::CONTEXT_REDS`, the engine's reduction quantum and the
/// runner's default `DEFAULT_REDUCTIONS_PER_SLICE`), declared rather than guessed: a slice is
/// the unit the runner pre-empts in, so this makes the door run at most **once per
/// pre-emption** and never on a slice that yielded early. A smaller interval would put a
/// collection inside the same slice's worth of allocation it is meant to bound; a larger one
/// would let the nursery chain grow over several slices before it is collapsed.
///
/// The interval is accumulated per **pid** (`Runtime::accumulate_reds_since_gc_boundary`), not
/// read from the slice, so a process that pre-empts on a partially spent budget still pays its
/// way: several short slices add up to one boundary.
///
/// **Retained unchanged when the watermark moved to 7/8, and measured to be the wrong knob to
/// move.** The interval is the *rate limit*: a slice is one budget, so on the default
/// (native-baseline) tier every pre-emption already arrives with a full
/// `DEFAULT_REDUCTIONS_PER_SLICE` and the interval never binds — it only stops a process that
/// yields early from collecting inside the same slice's worth of allocation it is meant to bound.
/// The figure it does set is the *no-heap-admission* shape's collection rate (`collections == 3N /
/// 1 000`, see [`crate::runner`]'s `reds_door` suite), which is a property of the reduction cost
/// of the loop and not of the occupancy watermark; lengthening it would simply collect that shape
/// less often — eroding the one shape this door exists for — without touching the churn
/// regression, which the watermark fixed.
pub(crate) const REDS_BOUNDARY_MIN_REDS: u32 = 4_000;
/// The occupancy watermark's numerator: collect once **seven eighths** of the current young space
/// is used ([`BOUNDARY_OCCUPANCY_DEN`] is the denominator).
///
/// This is the C's own rule in the only unit this door has. The C's allocation-area check
/// (`ERTS_VBUMP_ALLOC_REDS`, out of `HAlloc`'s v-bump path) runs when the allocation area is
/// **exhausted**, not when it is half used; this door is handed no allocation request — only the
/// slice-boundary occupancy — so the faithful transcription is "collect when the space is nearly
/// gone". Two jobs:
///
/// * it is what makes `heap_size` **plateau**: the live set of an allocate-and-abandon loop is a
///   handful of words, so a collection sizes the nursery from the live set plus the policy's
///   floors and the next round fills that same space again — the capacity stops growing once it
///   covers one boundary's worth of allocation;
/// * without it the door would collect on every pre-emption of a process whose nursery is nearly
///   empty (a mostly-parked or barely-allocating process), paying a Cheney traversal for nothing.
///
/// **Measured (single-build interleaved A/B, `taskset -c 3`, 15 pairs, deterministic counters,
/// `gc_bound` churn, default native-baseline tier).** The churn shape's own heap-admission door
/// already settles its collection debt every iteration, so a boundary collection here is nearly
/// always a duplicate — and an expensive one, because `beam_gc::Collector::collect` escalates to a
/// **major** whenever the mature generation is not larger than the nursery in use. At a `1/2`
/// watermark the door fired on **47 of 95** boundaries (N = 500; 191 of 383 at N = 2 000) and cost
/// **+0.0479 collections/call (+0.69 %)** and **+0.1337 majors/call (+8.9 %)** against a
/// door-disabled build of the same tree. At `7/8` the same runs measure **exactly** the
/// door-disabled counts — 6.984032 collections and 1.497006 majors per call at N = 500, 6.996002
/// and 1.499250 at N = 2 000 — while the door's own 11 / 47 collections still keep the
/// retired-space chain collapsed (one young space at the 233-word floor, against the door-off two
/// spaces / 376 words). Closing the gate further buys nothing: the C-exact `used >= capacity`
/// (`1/1`) measures the *same* counters and only trades 11 firings for 3, so the looser `7/8` is
/// taken — see just below for why a hard `used == capacity` test is a liability rather than a
/// virtue on a nursery that grows on demand.
///
/// **Not `1/1`.** `Process::grow_nursery` extends the current space in place, so a space one word
/// short of full refills immediately and satisfies an exact `used == capacity` test on consecutive
/// boundaries; chained that way the door would amplify majors instead of rationing them. `7/8`
/// leaves the growth seam its headroom and is measured indistinguishable from `1/1` in the
/// counters.
///
/// **What would regress it:** a shape whose slice-boundary occupancy sits in `[1/2, 7/8)` — it
/// would then be collected only through the debt arm, and if its growth seam retired no space it
/// would climb again. Not observed: the churn shape is bimodal in eighths (12 of 95 checks empty,
/// 24 at ≥ 7/8, the rest spread over `[1/8, 7/8)` but always *after* a collection), the
/// no-heap-admission shape reports a debt at every check, and the pre-empting-but-not-churning
/// shapes (`etf_roundtrip`, `list_walk`, `ets_copy`) keep a flat 233/376-word nursery.
const BOUNDARY_OCCUPANCY_NUM: usize = 7;
/// The occupancy watermark's denominator (see [`BOUNDARY_OCCUPANCY_NUM`]).
const BOUNDARY_OCCUPANCY_DEN: usize = 8;

/// Collects the running process's own heap at a **reduction/yield boundary** — the slice-end
/// collection point for a workload whose loop reaches no heap-admission door.
///
/// Called by the runner's pre-emption arm, with the parked machine's whole root set handed over
/// through [`Machine::with_roots`](beam_engine::Machine::with_roots). The gates are checked in
/// this order, each a typed `Ok(())` no-op:
///
/// 1. **the process may be collected at all** ([`Runtime::gc_process_enabled`]: not pinned, not
///    halted, the global valve closed);
/// 2. **the reduction interval** ([`REDS_BOUNDARY_MIN_REDS`] since this pid's last boundary);
/// 3. **a caller module is known** — the same refusal [`super::ensure_heap_full`] makes, and
///    *not* a float guard: this door publishes the machine's whole root set, the FR prefix
///    included, so a live float register is relocated like any other root;
/// 4. **the process is in the table** the caller schedules from;
/// 5. **the window is collectable** ([`beam_process::Process::is_relocation_exempt`]: an exempt
///    park published nothing and its machine stays authoritative — collecting there would move
///    the heap out from under it);
/// 6. **the nursery is nearly exhausted** (seven eighths of the current space, or a growth's
///    collection debt);
/// 7. **the unscanned-root valve** ([`super::valve_verdict`]), exactly as the full heap-admission
///    door runs it — because this door too hands the valve the machine's whole root set, a clean
///    descent may also **close** the process's per-pid descent latch.
///
/// `root` is the parked machine's root set; `fr_count` is the live FR prefix the matching park
/// published ([`Machine::fr_mark`](beam_engine::Machine::fr_mark)), which the resume re-reads, so
/// a different count here would mis-size the FR region and halt the process's collection;
/// `reds_since_check` is the reduction count the just-ended slice spent, accumulated into the
/// pid's interval.
///
/// The action is [`beam_process::Process::collect`] **unconditionally**, never the admission
/// doors' `collect_or_grow`: [`beam_process::Process::ensure_capacity`]'s `nursery.grow` is a
/// no-op when the current space already has room, so it never collapses the retired chain — and
/// collapsing that chain is the whole point of this door.
///
/// # Errors
///
/// None reachable: a failed (or mis-sized) collection halts collection for that process
/// ([`Runtime::halt_process_gc`]) and reports success, exactly as the heap-admission doors do —
/// the process keeps its unmoved heap and allocation continues as before.
pub(crate) fn collect_at_reds_boundary(
    ctx: &mut RuntimeContext<'_>,
    mut roots: RootSet<'_>,
    fr_count: usize,
    reds_since_check: u32,
) -> Result<()> {
    let pid = ctx.mailbox_pid;
    if !ctx.runtime.gc_process_enabled(pid) {
        return Ok(());
    }
    // The **interval**: the accumulation is the gate, so a pid that pre-empted on a partially
    // spent budget keeps its credit and the boundary is only reset once it is really reached.
    let since = ctx
        .runtime
        .accumulate_reds_since_gc_boundary(pid, reds_since_check);
    if since < REDS_BOUNDARY_MIN_REDS {
        return Ok(());
    }
    ctx.runtime.reset_reds_since_gc_boundary(pid);
    // The door needs a caller module to name at all — a fun entry, or the `-eval` evaluator, has
    // none — and is otherwise left alone. Not a float guard: the whole root set is published
    // below, the FR prefix included (see the module docs).
    if ctx.current_module.is_none() {
        return Ok(());
    }
    let RuntimeContext {
        runtime,
        mailbox_table,
        ..
    } = ctx;
    let table: &mut ProcessTable = match mailbox_table {
        Some(table) => table,
        None => return Ok(()),
    };
    let Some(process) = table.get_mut(pid) else {
        return Ok(());
    };
    // An **exempt** park published nothing: nothing may relocate the heap for the whole window
    // (the machine is the authoritative root set), so a collection here would leave the parked
    // machine's own words dangling into the evacuated from-space.
    if process.is_relocation_exempt() {
        return Ok(());
    }
    // The **occupancy watermark**, plus the collector's *collection debt* — the growth seam's own
    // signal that the nursery is a chain only a collection can collapse (see
    // [`beam_gc::Collector::collection_debt`]). The two are OR-ed exactly as the heap-admission
    // door OR-ed its own occupancy test with the debt: the watermark says "this allocation area is
    // nearly exhausted" (the C's `ERTS_VBUMP_ALLOC_REDS` moment), the debt says "a growth retired a
    // space, so only a collection can collapse the chain" — the one collection the watermark alone
    // would never schedule on a shape whose boundary occupancy stays low.
    let debt = process.collection_debt();
    let (used, capacity) = {
        let young = process.collector().young();
        (young.used_words(), young.capacity_words())
    };
    let occupied = used > 0
        && used.saturating_mul(BOUNDARY_OCCUPANCY_DEN)
            >= capacity.saturating_mul(BOUNDARY_OCCUPANCY_NUM);
    if !occupied && !debt {
        return Ok(());
    }
    // The **pointer-bearing prefix** of each register file, computed once: one past the last
    // boxed-or-list word of the x file, or `0` for a file of immediates (the stack is always
    // whole). The FR bound is the caller's `fr_count` — the count the park published and the
    // resume will re-read — never a recomputation, because a disagreement between the two halves
    // of one hand-off halts the process's collection.
    let (x_count, _) = machine_root_bounds(&roots);
    let fr_count = fr_count.min(roots.fr.len());
    // The **unscanned-root valve**, exactly as the full heap-admission door runs it: a root in a
    // region the collector does not scan declines the move (see [`super::root_is_unscanned`]), so
    // the heap keeps its terms and the unscanned root stays valid. A hit opens `pid`'s descent
    // latch permanently — the only direction it can ever move on its own.
    let descend = runtime.foreign_reachable(pid);
    let machine_roots = || {
        roots.x[..x_count]
            .iter()
            .chain(roots.stack.iter())
            .chain(roots.fr[..fr_count].iter())
    };
    let verdict = valve_verdict(runtime.walk_scratch(), process, machine_roots(), descend);
    if verdict.declined {
        runtime.note_foreign_reachable(pid);
        return Ok(());
    }
    // This door handed the valve the machine's whole root set, so a descent that reached no
    // container from a region the collector does not scan may **close** the latch (the same
    // closing half the full heap-admission door runs; see
    // [`Runtime::close_foreign_reachable`]).
    if verdict.foreign_free {
        runtime.close_foreign_reachable(pid);
    }
    // Hand the machine's root set over **whole** — deliberately *not* narrowed to `x_count`.
    //
    // The length rule of this hand-off is not this door's to pick: the machine reached this point
    // through the runner's park arm, which published it with
    // [`RootSet::ALL_LIVE`](beam_engine::context::RootSet::ALL_LIVE) (the whole x file), and the
    // matching resume scatters with `ALL_LIVE` too
    // ([`RuntimeContext::resync_parked_roots`](crate::context::RuntimeContext)). Every register
    // this door leaves out would shorten the process's live x slice, and the resume's
    // `scatter_roots_full` would then find a length it did not publish — a `false`, which halts
    // the process's collection for the rest of its life. So the publish/scatter pair here uses
    // exactly the park's width; `x_count` bounds only the valve's per-word scan, whose verdict it
    // cannot change (the words it skips are immediates, which no reachability test reads).
    if !publish_roots_full(process, &roots, fr_count) {
        runtime.halt_process_gc(pid);
        return Ok(());
    }
    // The action is a **young-only** collection, never `collect_or_grow` and never the
    // escalating `Process::collect`: `Process::ensure_capacity`'s `nursery.grow` is a no-op when
    // the current space already has room (so it would leave the retired chain exactly where it
    // is — the mechanism of the climb this door exists to stop), and the unpinned `collect`
    // escalates a would-not-fit minor to a **major**, which turns this door's rationed
    // slice-boundary collection into a fullsweep. `Process::collect_kind(Minor)` evacuates the
    // nursery only and reports the typed heap exhaustion when the promotion cannot fit, which
    // the `Err` arm below turns into the same `halt_process_gc` no-op every other refusal takes.
    match process.collect_kind(CollectionKind::Minor) {
        Ok(collection) => {
            runtime.note_gc_collection(matches!(collection.kind, CollectionKind::Major));
            runtime.note_gc_reclaimed(collection.stats.words_reclaimed);
        }
        Err(_) => {
            runtime.halt_process_gc(pid);
            return Ok(());
        }
    }
    if !scatter_roots_full(process, &mut roots, fr_count) {
        runtime.halt_process_gc(pid);
        return Ok(());
    }
    Ok(())
}
