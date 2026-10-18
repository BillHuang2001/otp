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
//! The **off-heap refc-binary reclamation** point: pruning an engine machine's payload
//! reservoir at the slice boundary, gated by the process's binary virtual heap.
//!
//! A process's off-heap refc binaries are owned by the machine that built them
//! ([`Machine::refc_binaries`](beam_engine::Machine::refc_binaries), the reservoir), and
//! [`Machine::reset`](beam_engine::Machine::reset) deliberately never clears it: a returned term
//! may still name a payload after the run that built it. So a machine that builds one off-heap
//! binary per iteration — `<<0:N/unit:8>>` with a variable `N`, a `binary:copy/2`, any
//! `bs_init_writable` accumulator — accumulates one handle per iteration **for the life of the
//! process**, and both the `binary_alloc` live-bytes figure (`erlang:memory(binary)`) and RSS grow
//! with the loop count where the oracle's stay flat.
//!
//! This module closes that loop. [`reclaim`] walks the process's own root set, asks
//! [`beam_gc::for_each_reachable_binary_payload`] which payloads it still reaches, and hands that
//! set to [`Machine::retain_refc_binaries`](beam_engine::Machine::retain_refc_binaries) — which
//! **moves nothing**: a retained payload keeps its exact `data_ptr`, so no live on-heap `BinRef`
//! is invalidated and no term needs republishing. The C counterpart is `sweep_off_heap`
//! (`erts/emulator/beam/erl_gc.c:3051-3100`), where a collection counts the surviving binaries and
//! releases the dead ones; the difference is that ERTS collects and sweeps in one pass over a heap
//! it just evacuated, while this runtime's payloads live outside every heap the collector owns and
//! so need a separate reachability question against the same roots.
//!
//! # Why the single reclamation point is the slice-boundary park
//!
//! A prune is only sound against the process's **complete** root set, and only one site has both
//! halves at once — the machine *and* that root set:
//!
//! * **The heap-admission doors cannot host it.** [`super::ensure_heap`],
//!   [`super::ensure_heap_full`] and [`super::alloc_words_rooted`] run *inside* a slice, from the
//!   engine's `ExecutionContext` implementation, and the running engine is held **out** of
//!   [`RunnerState::engines`](crate::runner::RunnerState) for that whole slice (the runner takes it
//!   out to drive it — see [`crate::runner`]). The `ExecutionContext` seam exposes no
//!   `&mut Machine` at all: `call_bif_in_machine` hands over a `&Machine'. So a door reached from
//!   inside the slice cannot even *see* the reservoir, let alone prune it — reaching for it through
//!   the engines table would need the very `&mut Machine` the runner is holding, the twin-borrow
//!   that split the table out in the first place.
//! * **The park arm is the one place the twin-borrow is closed.** At the end of a slice the runner
//!   owns `&mut Machine` (it just drove it), `&mut Process` (through the table) and `&mut Runtime`
//!   simultaneously, and a **publishing** park has just handed the machine's whole root set over to
//!   the process (`publish_parked_roots`, `RootSet::ALL_LIVE`), so the process's root set really is
//!   the machine's. That is exactly the pair a reachability question needs.
//!
//! # The residual: a process that never runs again
//!
//! Only the **`Yield`** (pre-emption) arm calls this. The `Wait` arm deliberately does not: a
//! `receive` park sets the process **relocation-exempt** and publishes nothing
//! ([`crate::runner::run`]'s park), so the machine stays the authoritative root set and the roots
//! this walk reads would be stale by construction — the first valve below declines it anyway. The
//! consequence is a **leak, never a dangle**: a process that parks on `receive` and is never
//! scheduled again keeps its reservoir until its machine drops with the process. A process that
//! wakes up and yields again reclaims then.
//!
//! # Why this is its own file
//!
//! [`super`] (`src/context/gc.rs`) is already ~3 200 lines — the crate's ~1000-line file budget
//! says a new responsibility goes in its own file rather than growing it further.
//!
//! # What had to be verified before the predicate was allowed to free anything
//!
//! The predicate is the soundness assertion (see
//! [`Machine::retain_refc_binaries`](beam_engine::Machine::retain_refc_binaries)): a **false reject**
//! frees a payload a live `BinRef` still names — a dangling pointer read later through the frozen
//! term layout — while a **false accept** is merely a leak. Each secondary owner of a payload was
//! therefore traced, not assumed:
//!
//! * **Another process holds it (the send path).** A message carrying an off-heap binary copies the
//!   `[BinRef][ErlSubBits]` pair onto the receiver's heap and **clones** the sender's handle into the
//!   envelope — `beam_message::copy::copy_refc_binary` resolves the source handle by payload pointer
//!   and calls `push_binary` (`crates/beam_message/src/copy.rs:404-410`), and
//!   `enqueue_copied` drains those handles onto the [`Message`](beam_message::Message)
//!   (`crates/beam_message/src/mailbox.rs:150-164`). A received payload is then kept alive again
//!   when the receiver removes the message (`MailboxState::remove_at` clones the envelope's handles,
//!   `crates/beam_rt/src/mailbox.rs:268-282`). So a payload another process still references is
//!   owned by *that* handle with `refcount() > 1`, and the prune below cannot free it.
//! * **ETS holds it.** It cannot: table storage re-materialises a refc binary as a self-contained
//!   on-heap bitstring. `ets:insert/2` deep-copies the object into the table's never-relocating
//!   arena (`beam_ets::Table::insert` → `store_object` → `TermStore::copy`,
//!   `crates/beam_ets/src/table/mod.rs:526-546` and `crates/beam_ets/src/store/mod.rs:263-288`),
//!   and the sub-bits arm gathers the payload bytes and rewrites them on-heap
//!   (`TermStore::copy_sub_bits`, `crates/beam_ets/src/store/mod.rs:766-787`) — the module doc says
//!   so explicitly ("re-materialised as self-contained on-heap bitstrings … so no off-heap pointer
//!   survives", `crates/beam_ets/src/store/mod.rs:70-75`). No table row shares a payload pointer, so
//!   the prune cannot dangle one.
//! * **The escaped runtime store holds it.** `Runtime::adopt_escaped_binaries` **clones** every
//!   handle a finished nested machine left into `Runtime::eval_off_heap`
//!   (`crates/beam_rt/src/runtime.rs:1618-1630`, called from the nested run's teardown at
//!   `crates/beam_rt/src/context/pump.rs:282-284`), so a payload that store owns is a second owner
//!   with `refcount() > 1` and survives.
//! * **Nested machines are never pruned — the correct decline.** `RuntimeContext::run_export`,
//!   `apply_fun` and the `-eval` evaluator run a machine synchronously and drop it (adopting its
//!   handles into the escaped store on the way out); such a machine is never parked, so this point
//!   never sees it. Its reservoir dies with it, which is the pre-existing behaviour.
//!
//! The `refcount() > 1` guard in step 5 is the **belt for anything the four findings above do not
//! cover**: a payload some other owner already cloned is retained whatever this process's roots say,
//! so the pruner never has to model every secondary owner. A false accept is a leak; a false reject
//! remains impossible.
//!
//! # Why the valve is NOT `Process::has_unpublished_roots`
//!
//! [`Process::has_unpublished_roots`] is `true` for a `ProcessState::Running` process
//! (`crates/beam_process/src/process.rs:1439`), and a process is still `Running` at the park — it
//! has neither returned nor blocked. Consulting it would decline on **every** park, i.e. disable
//! this point entirely. The correct pair is
//! [`Process::is_relocation_exempt`](beam_process::Process::is_relocation_exempt) (the window
//! published nothing) plus the park's own `parked_published` flag, and the runner gates on the
//! latter before calling in (see `RuntimeRunner::reclaim_off_heap_binaries`).
use core::mem::size_of;
use beam_common::Result;
use beam_engine::Machine;
use beam_gc::{BinVheap, HeapPolicy};
use beam_process::{Process, ProcessId, ProcessTable};
use beam_term::Eterm;
use crate::runtime::Runtime;
/// A process's off-heap **binary virtual heap** state: the C `p->bin_vheap_sz` budget
/// ([`BinVheap`]) plus the running off-heap overhead figure it is compared against — the C
/// `MSO(p).overhead`.
///
/// One of these lives per process engine ([`ProcessEngine::off_heap`](crate::runner::ProcessEngine)),
/// created lazily on the first park that reaches it (from `process.collector().policy()`, so a
/// `+hms`/`+hbm*` policy is honoured) and dead with the machine.
///
/// # Why the overhead is *observed*, not summed
///
/// [`Machine::off_heap_overhead_bytes`](beam_engine::Machine::off_heap_overhead_bytes) is
/// `O(reservoir)`, and the trigger is consulted at **every** park — so summing there would put a
/// walk over every payload the process ever built on the per-slice path. The reservoir only ever
/// *grows* between prunes (the engine appends, and nothing else releases a handle), so "the handle
/// count did not move" implies "the overhead did not move": [`OffHeapReclaimer::observe`] re-derives
/// the figure only when the count changed, and every release path
/// ([`OffHeapReclaimer::record_collection`]) re-primes the pair.
#[derive(Debug, Clone)]
pub(crate) struct OffHeapReclaimer {
    /// The binary-virtual-heap budget — the C `p->bin_vheap_sz`.
    vheap: BinVheap,
    /// The reservoir's handle count at the last observation (see the type docs).
    seen_handles: usize,
    /// The reservoir's off-heap overhead, in bytes, at the last observation.
    overhead_bytes: usize,
    /// How many times [`OffHeapReclaimer::observe`] had to re-derive the overhead — the
    /// observation's own [O(1)](OffHeapReclaimer::observe) pin.
    #[cfg(test)]
    rederivations: usize,
}
impl OffHeapReclaimer {
    /// A fresh reclaimer for a process started with `policy`: the budget's floor is
    /// [`HeapPolicy::min_bin_vheap_words`], and nothing has been observed yet (`0` handles, `0`
    /// bytes) — which is exactly what an empty reservoir holds, so a machine that has not built a
    /// payload is already observed correctly and one that has is derived on the first
    /// [`OffHeapReclaimer::observe`].
    pub(crate) fn new(policy: &HeapPolicy) -> OffHeapReclaimer {
        OffHeapReclaimer {
            vheap: BinVheap::new(policy),
            seen_handles: 0,
            overhead_bytes: 0,
            #[cfg(test)]
            rederivations: 0,
        }
    }
    /// Refreshes the observed off-heap overhead and returns it, in **bytes**.
    ///
    /// `O(1)` when the reservoir's handle count has not moved since the last observation (the
    /// common case: a slice that built no off-heap binary, or one that built fewer than the
    /// trigger allows), and `O(reservoir)` only when it has. See the type docs for why the count
    /// implies the figure.
    pub(crate) fn observe(&mut self, machine: &Machine) -> usize {
        if machine.refc_binaries().len() == self.seen_handles {
            return self.overhead_bytes;
        }
        self.rederive(machine)
    }
    /// Re-derives the observed figure from `machine` unconditionally and returns it, in bytes.
    fn rederive(&mut self, machine: &Machine) -> usize {
        self.seen_handles = machine.refc_binaries().len();
        self.overhead_bytes = machine.off_heap_overhead_bytes();
        #[cfg(test)]
        {
            self.rederivations += 1;
        }
        self.overhead_bytes
    }
    /// Whether the observed off-heap overhead is past the budget — the C
    /// `Proc->off_heap.overhead > Proc->bin_vheap_sz` (`erts/emulator/beam/erl_gc.h:132-135`), the
    /// second disjunct of `ERTS_IS_GC_DESIRED_INTERNAL`.
    ///
    /// The comparison is **strictly greater**, exactly as in the C (an overhead equal to the limit
    /// is still within budget), and the budget is in term **words** while the observed figure is in
    /// bytes — so the conversion is the truncating division the machine's own docs prescribe.
    pub(crate) fn triggered(&self) -> bool {
        self.vheap
            .triggered(self.overhead_bytes / size_of::<Eterm>())
    }
    /// The current budget, in term words (the C `p->bin_vheap_sz`).
    pub(crate) fn limit_words(&self) -> usize {
        self.vheap.limit_words()
    }
    /// Re-sizes the budget from the payload that **survived** a reclamation and re-primes the
    /// observation pair from `machine`, returning the surviving overhead in bytes.
    ///
    /// This is the C pair
    ///
    /// ```text
    /// p->bin_vheap_sz = next_vheap_size(p, bin_vheap, p->bin_vheap_sz);
    /// MSO(p).overhead = bin_vheap;
    /// ```
    ///
    /// (`erts/emulator/beam/erl_gc.c:3288-3289`), reproduced by
    /// [`BinVheap::record_collection`]. The re-prime is what makes the next
    /// [`OffHeapReclaimer::observe`] cheap again: after a release the handle count has moved but
    /// this call has already read the post-release figure, so it records it instead of making the
    /// next park pay for it.
    ///
    /// The single `O(reservoir)` re-derivation here is on the **triggered** path only — the one
    /// park that actually released something — never on the per-slice path.
    pub(crate) fn record_collection(
        &mut self,
        policy: &HeapPolicy,
        machine: &Machine,
    ) -> usize {
        let surviving_bytes = self.rederive(machine);
        self.vheap.record_collection(
            policy,
            surviving_bytes / size_of::<Eterm>(),
        );
        surviving_bytes
    }
    /// How many times the overhead has been re-derived — the [`OffHeapReclaimer::observe`]
    /// `O(1)` pin (test-only).
    #[cfg(test)]
    pub(crate) fn rederivations(&self) -> usize {
        self.rederivations
    }
}
/// Reclaims the off-heap refc-binary payloads `pid`'s machine built that nothing reachable from
/// `pid`'s root set names, returning how many handles were released.
///
/// The caller is the runner's `Yield` park arm, for a **publishing** window: the machine's whole
/// root set (`RootSet::ALL_LIVE`) has already been handed to the process, so `pid`'s registers
/// really do hold everything the machine holds (see the module docs for why no other site can).
/// `slot` is the process engine's reclaimer, built from
/// `process.collector().policy()` on first use.
///
/// Every gate is a **decline**: the whole reservoir is retained and `Ok(0)` is returned, exactly
/// as [`Process::sweep_off_heap_binaries_reachable`] declines. A decline's residual is a leak —
/// never a dangling pointer — because a reclamation is an optimization and must never turn a
/// working program into a failing one.
///
/// The gates, in order:
///
/// 1. the process is in the caller's table;
/// 2. [`Process::is_relocation_exempt`] is `false` — an exempt window published nothing, so the
///    machine is the authoritative root set and a walk over the process's roots would be reading
///    words nothing maintains;
/// 3. [`Runtime::gc_process_enabled`] — the existing pinned/halted/global-escape-valve gate, so a
///    runtime store holding an escaped off-heap term closes this point exactly as it closes a
///    collection;
/// 4. the **[`BinVheap`] trigger** — the off-heap overhead (in words) is past the budget the
///    process last sized itself to. This is what keeps the point off the per-slice path: a process
///    that is not holding much off-heap payload pays one `i64` compare;
/// 5. the **unscanned-root valve** ([`super::valve_verdict`]) — a root that reaches a container in
///    a region the collector does not scan may hold a `BinRef` the walk cannot see, and a false
///    reject here is a dangling pointer;
/// 6. the **reachability walk** itself ([`beam_gc::for_each_reachable_binary_payload`]) — a walk
///    that cannot complete declines rather than reporting "nothing is live".
///
/// The release is [`Machine::retain_refc_binaries`](beam_engine::Machine::retain_refc_binaries) with
/// `live(p) || shared(p)`, where `shared` is the set of payloads whose
/// [`RefcBinary::refcount`](beam_alloc::RefcBinary::refcount) is `> 1` — the defence-in-depth belt
/// the module docs describe. Afterwards the process's off-heap byte figure is republished
/// ([`Process::set_off_heap_binary_bytes`]) so `max_heap_size`'s
/// `include_shared_binaries` charge and any
/// memory reporter see the real number.
///
/// # Errors
///
/// None are produced: every failure mode above is the documented decline. The `Result` matches the
/// other collection sites' shape so the runner's arm reads uniformly, and leaves room for a future
/// gate that really can fail.
pub(crate) fn reclaim(
    runtime: &mut Runtime,
    table: &mut ProcessTable,
    pid: ProcessId,
    machine: &mut Machine,
    slot: &mut Option<OffHeapReclaimer>,
) -> Result<usize> {
    let Some(process) = table.get_mut(pid) else {
        return Ok(0);
    };
    // An exempt window published nothing: the parked machine is the authoritative root set, so the
    // process's registers are not this walk's business (and relocating against them would be the
    // very move the exemption declines).
    if process.is_relocation_exempt() {
        return Ok(0);
    }
    if !runtime.gc_process_enabled(pid) {
        return Ok(0);
    }
    if slot.is_none() {
        *slot = Some(OffHeapReclaimer::new(process.collector().policy()));
    }
    let Some(reclaimer) = slot.as_mut() else {
        // Unreachable: the line above made it `Some`. A decline (retain everything) rather than a
        // panic, like every other gate here.
        return Ok(0);
    };
    reclaimer.observe(machine);
    if !reclaimer.triggered() {
        return Ok(0);
    }
    // The process's **complete** root set, gathered exactly as
    // `Process::gather_scan_roots` gathers it (that one is private; the four
    // `gather_roots`/`Message` accessors are public). This is complete *because* the park
    // published the machine's own root set into `Registers` — x, the whole stack and the live FR
    // prefix — immediately before this call.
    let mut roots: Vec<Eterm> = Vec::new();
    {
        let process: &Process = process;
        process.registers().gather_roots(&mut roots);
        for message in process.mailbox().iter() {
            roots.push(message.payload());
            roots.push(message.from());
            roots.push(message.token());
        }
        process.dict().gather_roots(&mut roots);
        process.saved_args().gather_roots(&mut roots);
        process.signals().gather_roots(&mut roots);
    }
    // The **unscanned-root valve**, the same one every collection site runs. A decline opens
    // (never closes) the per-pid descent latch: closing it belongs to the three doors that hand
    // the walk the machine's whole root set as a *collection*, and this point does not change that
    // policy.
    let descend = runtime.foreign_reachable(pid);
    let verdict = super::valve_verdict(runtime.walk_scratch(), process, roots.iter(), descend);
    if verdict.declined {
        runtime.note_foreign_reachable(pid);
        return Ok(0);
    }
    let mut live: Vec<usize> = Vec::new();
    if let Err(err) =
        beam_gc::for_each_reachable_binary_payload(process.collector(), &roots, |payload| {
            live.push(payload)
        })
    {
        // A walk that cannot complete has not proved anything is dead, so nothing is released: the
        // residual is a leak, never a dangle.
        crate::debug_trace::trace(format_args!(
            "off-heap reclamation declined for {pid:?}: the reachability walk failed ({err})"
        ));
        return Ok(0);
    }
    live.sort_unstable();
    // The payloads another owner already cloned. A clone is what the send path, the escaped
    // runtime store and a `bs_init_writable` accumulator all take, so a payload at `refcount > 1`
    // is live whatever this process's roots say (see the module docs).
    let shared: Vec<usize> = machine
        .refc_binaries()
        .iter()
        .filter(|binary| binary.refcount() > 1)
        .map(|binary| binary.data_ptr() as usize)
        .collect();
    let released = machine.retain_refc_binaries(|payload| {
        let payload = payload as usize;
        live.binary_search(&payload).is_ok() || shared.contains(&payload)
    });
    // Re-size the budget from what survived and re-prime the observation pair (the C
    // `p->bin_vheap_sz = next_vheap_size(...)` / `MSO(p).overhead = bin_vheap` pair), then
    // republish the process's off-heap byte figure from the same reading.
    let surviving = reclaimer.record_collection(process.collector().policy(), machine);
    process.set_off_heap_binary_bytes(surviving);
    if released > 0 {
        crate::debug_trace::trace(format_args!(
            "off-heap reclamation for {pid:?}: released {released} handle(s), {surviving} bytes \
             left, binary-vheap limit now {} words",
            reclaimer.limit_words()
        ));
    }
    Ok(released)
}

#[cfg(test)]
mod tests {
    //! The reclamation's own pins: the `O(1)` observation, the strict trigger, the
    //! `record_collection` re-prime, the decline arms, the `refcount > 1` belt, and one positive
    //! prune over a **real** `Machine` + `Process` driven by a real compiled module.
    //!
    //! The positive case needs payloads in the reservoir, and the only way to build one from this
    //! crate is to execute compiled code (`Machine::alloc_refc_binary` is `beam_engine`-private), so
    //! that one compiles a fixture with the system `erlc` and **skips loudly** when the toolchain is
    //! absent — the crate's convention.
    use std::sync::Arc;
    use beam_process::SignalAtoms;
    use beam_term::{make_small, AtomTable};
    use crate::context::test_support::{compile_fixture, resolve_erlc};
    use crate::context::RuntimeContext;
    use super::*;

    /// A policy whose binary-virtual-heap floor is tiny, so the trigger fires on a handful of
    /// payload bytes instead of the 371 KB the BEAM default (`46422` words) requires.
    fn impatient_policy() -> HeapPolicy {
        HeapPolicy {
            min_bin_vheap_words: 1,
            ..HeapPolicy::default()
        }
    }

    /// A runtime with the default configuration (the crate's in-src test convention).
    fn runtime() -> Runtime {
        Runtime::with_defaults().expect("the default runtime")
    }

    /// A process table independent of the runtime (what the scheduler owns).
    fn table() -> ProcessTable {
        ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
    }

    #[test]
    fn the_observation_is_o1_while_the_handle_count_does_not_move() {
        // A fresh reclaimer has observed "0 handles, 0 bytes", which is exactly what an empty
        // reservoir holds — so the first observation of an empty machine is already the
        // short-circuit, not a derivation.
        let machine = Machine::new();
        let mut reclaimer = OffHeapReclaimer::new(&impatient_policy());
        assert_eq!(reclaimer.observe(&machine), 0);
        assert_eq!(reclaimer.rederivations(), 0, "an empty reservoir is pre-observed");
        // A count the reclaimer has not seen is the only thing that makes it look at the
        // reservoir (this is how a machine that has built payloads is first observed).
        let mut stale = OffHeapReclaimer::new(&impatient_policy());
        stale.seen_handles = 7;
        assert_eq!(stale.observe(&machine), machine.off_heap_overhead_bytes());
        assert_eq!(stale.rederivations(), 1);
        assert_eq!(stale.seen_handles, 0, "...and it is now primed at the machine's real count");
        // The pin itself: with the count unchanged, `observe` hands back the cached figure
        // without summing the reservoir — proved by a deliberately wrong cached figure that
        // survives the call untouched.
        stale.overhead_bytes = 12_345;
        assert_eq!(
            stale.observe(&machine),
            12_345,
            "an unchanged handle count must not re-sum the reservoir"
        );
        assert_eq!(stale.rederivations(), 1, "still the one derivation");
    }
    #[test]
    fn the_trigger_is_strictly_greater_than_the_budget() {
        let policy = HeapPolicy {
            min_bin_vheap_words: 100,
            ..HeapPolicy::default()
        };
        let mut reclaimer = OffHeapReclaimer::new(&policy);
        assert_eq!(reclaimer.limit_words(), 100);
        // 800 bytes is exactly 100 words: still within budget, exactly as the C's `>`.
        reclaimer.overhead_bytes = 800;
        assert!(
            !reclaimer.triggered(),
            "an overhead equal to the limit is not a trigger"
        );
        reclaimer.overhead_bytes = 808; // one word past it
        assert!(reclaimer.triggered(), "one word past the limit triggers");
        reclaimer.overhead_bytes = 799; // below, and not word-aligned
        assert!(!reclaimer.triggered());
    }

    #[test]
    fn record_collection_resizes_the_budget_and_reprimes_the_observation() {
        let policy = HeapPolicy {
            min_bin_vheap_words: 233,
            ..HeapPolicy::default()
        };
        let machine = Machine::new();
        let mut reclaimer = OffHeapReclaimer::new(&policy);
        // Nothing survived: the budget halves toward the floor, which wins.
        assert_eq!(reclaimer.record_collection(&policy, &machine), 0);
        assert_eq!(reclaimer.limit_words(), 233, "the policy floor holds");
        assert_eq!(reclaimer.rederivations(), 1);
        // The pair is re-primed, so the next observation finds the count unchanged and does not
        // re-derive.
        assert_eq!(reclaimer.observe(&machine), 0);
        assert_eq!(reclaimer.rederivations(), 1, "re-primed, not re-derived");
        // A budget above the floor is re-sized by the C `next_vheap_size` rule from the surviving
        // words the machine reports — with an empty machine that is zero survivors, so the budget
        // either stands or halves, never grows.
        let policy = HeapPolicy {
            min_bin_vheap_words: 1,
            ..HeapPolicy::default()
        };
        let mut shrinking = OffHeapReclaimer::new(&policy);
        let before = shrinking.limit_words();
        let surviving = shrinking.record_collection(&policy, &machine);
        assert_eq!(surviving, 0, "the machine is empty");
        assert!(
            shrinking.limit_words() <= before,
            "an empty survivor set can only shrink (or hold) the budget, never grow it"
        );
    }

    /// The decline arms that need no reservoir: an absent process, an exempt process, a closed
    /// global valve and an untriggered budget all retain everything and report `0`.
    #[test]
    fn every_valve_declines_without_releasing_anything() {
        let mut rt = runtime();
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");
        let mut machine = Machine::new();
        let mut slot: Option<OffHeapReclaimer> = Some(OffHeapReclaimer::new(&impatient_policy()));
        // (1) A pid the caller's table does not hold.
        let absent = ProcessId::new(9_999, 0);
        assert_eq!(
            reclaim(&mut rt, &mut table, absent, &mut machine, &mut slot)
                .expect("a decline is not an error"),
            0
        );
        // (2) A relocation-exempt window published nothing.
        table
            .get_mut(pid)
            .expect("the process")
            .set_relocation_exempt(true);
        assert_eq!(
            reclaim(&mut rt, &mut table, pid, &mut machine, &mut slot).expect("declines"),
            0
        );
        table
            .get_mut(pid)
            .expect("the process")
            .set_relocation_exempt(false);
        // (3) The runtime's global escape valve is closed.
        rt.disable_gc("test: an escaped off-heap term is held");
        assert_eq!(
            reclaim(&mut rt, &mut table, pid, &mut machine, &mut slot).expect("declines"),
            0
        );
        // (4) Nothing off-heap has been built, so the budget cannot be exceeded: with the default
        // floor the `observe`/`triggered` pair is the whole answer.
        let mut rt = runtime();
        let mut slot: Option<OffHeapReclaimer> = None;
        assert_eq!(
            reclaim(&mut rt, &mut table, pid, &mut machine, &mut slot).expect("declines"),
            0
        );
        assert_eq!(
            slot.expect("the reclaimer was built from the process's policy")
                .limit_words(),
            HeapPolicy::default().min_bin_vheap_words,
            "the lazily built budget is the process's own policy floor"
        );
        assert!(
            machine.refc_binaries().is_empty(),
            "no decline may release a handle (there is none here to release)"
        );
    }

    /// The fixture the positive case drives: a loop that builds one off-heap binary **per
    /// iteration** (a runtime-sized `<<0:Size/unit:8>>`, which the compiler lowers to
    /// `bs_create_bin` → an off-heap refc payload) and discards it, returning a `{Sum, Last}`
    /// tuple so the last binary is still reachable from `x(0)` at the end of the run.
    const FIXTURE: &str = "\
-module(ogcbuild).
-export([build/2]).

build(Count, Size) ->
    {build(Count, Size, 0), <<7:Size/unit:8>>}.

build(0, _Size, Sum) ->
    Sum;
build(N, Size, Sum) ->
    Bin = <<0:Size/unit:8>>,
    build(N - 1, Size, Sum + byte_size(Bin)).
";

    /// Runs the fixture on a machine this test owns and publishes the machine's root set into
    /// `pid`, exactly as the park arm does (`RootSet::ALL_LIVE`, no FR words) — returning the
    /// machine so the caller can prune it.
    fn machine_that_built_payloads(
        rt: &mut Runtime,
        table: &mut ProcessTable,
        pid: ProcessId,
        count: i64,
        size: i64,
    ) -> Machine {
        let erlc = match resolve_erlc() {
            Some(erlc) => erlc,
            None => {
                eprintln!(
                    "SKIP: no system `erlc` on PATH -- the off-heap reclamation prune is skipped"
                );
                return Machine::new();
            }
        };
        let dir = compile_fixture(&erlc, "offheap_reclaim", "ogcbuild", FIXTURE);
        let module = Arc::new(
            beam_loader::load_file(dir.path().join("ogcbuild.beam")).expect("load the fixture"),
        );
        let function = module.atom_table.lookup("build").expect("build/2 is interned");
        let mut machine = Machine::new();
        {
            let mut ctx = RuntimeContext::for_process(rt, table, pid);
            machine
                .run_export_outcome(
                    &module,
                    function,
                    2,
                    &[make_small(count), make_small(size)],
                    &mut ctx,
                )
                .expect("the fixture runs");
        }
        // The park's publication, verbatim: the machine's whole root set into the process.
        let process = table.get_mut(pid).expect("the process");
        machine.with_roots(beam_engine::context::RootSet::ALL_LIVE, |roots| {
            assert!(
                super::super::publish_roots_full(process, &roots, 0),
                "the machine's root set publishes into the process"
            );
        });
        machine
    }

    #[test]
    fn a_parked_process_releases_the_payloads_its_roots_no_longer_reach() {
        let mut rt = runtime();
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");
        let mut machine = machine_that_built_payloads(&mut rt, &mut table, pid, 6, 512);
        if machine.refc_binaries().is_empty() {
            // The loud skip above returned an empty machine.
            return;
        }
        let before = machine.refc_binaries().len();
        assert!(
            before >= 7,
            "the fixture must build one payload per iteration plus the returned one, got {before}"
        );
        // The returned `<<7:Size/unit:8>>` is a big-endian integer segment: `Size-1` zero bytes
        // and a trailing `7` (the oracle's own reading — `hd(binary_to_list(<<7:512/unit:8>>))`
        // is `0`). The discarded loop payloads are the all-zero `<<0:Size/unit:8>>` ones.
        let returns_seven = |bytes: &[u8]| {
            bytes.last() == Some(&7) && bytes[..bytes.len() - 1].iter().all(|byte| *byte == 0)
        };
        let live_payload = machine
            .refc_binaries()
            .iter()
            .find(|binary| returns_seven(binary.as_bytes()))
            .map(|binary| binary.data_ptr())
            .expect("the returned <<7:Size/unit:8>> is in the reservoir");
        let mut slot: Option<OffHeapReclaimer> = Some(OffHeapReclaimer::new(&impatient_policy()));
        let released = reclaim(&mut rt, &mut table, pid, &mut machine, &mut slot)
            .expect("the reclamation does not fail");
        let after = machine.refc_binaries().len();
        assert_eq!(
            released,
            before - after,
            "the count released is exactly the handles the reservoir lost"
        );
        assert!(
            released >= before - 2,
            "every discarded loop payload must go (the register file may keep one live word), \
             released {released} of {before}"
        );
        assert!(after >= 1, "the reachable payload is retained");
        // The retained live payload is the very same block, unmoved: `retain_refc_binaries`
        // compacts handles, never payloads, so no on-heap `BinRef.val` is invalidated.
        assert!(
            machine
                .refc_binaries()
                .iter()
                .any(|binary| binary.data_ptr() == live_payload),
            "the reachable payload keeps its exact data_ptr"
        );
        assert!(
            machine
                .refc_binaries()
                .iter()
                .any(|binary| returns_seven(binary.as_bytes())),
            "and it is still readable"
        );
        // The process's off-heap byte figure is republished from the same reading, so
        // `max_heap_size`'s `include_shared_binaries` charge sees the truth.
        let published = table
            .get(pid)
            .expect("the process")
            .off_heap_binary_bytes();
        assert_eq!(published, machine.off_heap_overhead_bytes());
        assert!(
            published > 0 && published < 512 * before,
            "the published figure is the survivors', not the whole history's ({published})"
        );
        // The budget was re-sized from the survivors and is now primed at the real count.
        let mut reclaimer = slot.expect("the reclaimer");
        assert!(reclaimer.limit_words() >= 1);
        assert_eq!(
            reclaimer.observe(&machine),
            machine.off_heap_overhead_bytes()
        );
        assert_eq!(
            reclaimer.rederivations(),
            2,
            "one derivation on the trigger, one on the post-release re-prime -- never per park"
        );
    }

    #[test]
    fn a_payload_another_handle_holds_survives_the_prune() {
        let mut rt = runtime();
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");
        let mut machine = machine_that_built_payloads(&mut rt, &mut table, pid, 4, 512);
        if machine.refc_binaries().is_empty() {
            return; // the loud skip above
        }
        // Clone the handle of an **unreachable** (discarded loop) payload: the clone bumps the
        // payload's refcount, which is what the send path, the escaped runtime store and a
        // writable accumulator all do. The belt must retain it whatever the roots say.
        let other_owner = machine
            .refc_binaries()
            .iter()
            .find(|binary| binary.as_bytes().iter().all(|byte| *byte == 0))
            .cloned()
            .expect("a discarded loop payload is in the reservoir");
        assert!(other_owner.refcount() > 1, "the clone is a second owner");
        let kept = other_owner.data_ptr();
        let mut slot: Option<OffHeapReclaimer> = Some(OffHeapReclaimer::new(&impatient_policy()));
        let released = reclaim(&mut rt, &mut table, pid, &mut machine, &mut slot)
            .expect("the reclamation does not fail");
        assert!(released > 0, "the other discarded payloads are released");
        assert!(
            machine
                .refc_binaries()
                .iter()
                .any(|binary| binary.data_ptr() == kept),
            "a payload another handle holds is never released, reachable or not"
        );
        assert_eq!(
            other_owner.as_bytes().len(),
            512,
            "and its payload is intact through the other owner's handle"
        );
    }
}
