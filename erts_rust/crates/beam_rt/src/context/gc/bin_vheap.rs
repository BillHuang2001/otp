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
//! # The residual: a slice that never ends cannot reach this point
//!
//! This is a **`Yield`-arm-only** reclamation point, and that is the whole limit of it.
//!
//! * The **`Wait`** arm does not call it, so a `receive`-parked process reaches no prune at all —
//!   a live-FR window publishes nothing (the machine stays the authoritative root set, which is
//!   gate 2 below), and a publishing window's roots would be readable but no caller asks. A
//!   process that parks on `receive` and is never scheduled again keeps its reservoir until its
//!   machine drops with the process (a leak, never a dangle). A process that wakes up and
//!   pre-empts again reclaims then.
//! * A slice that **never pre-empts** reaches neither arm, and that is the unbounded case. On the
//!   default native baseline tier a linked chain charges **no reduction per link**
//!   (`Linker::emit_tail_exit` jumps straight to the successor entry and
//!   `jit/baseline/runtime.rs::jit_tail_call` is only the checked frame pop), so a BIF-free
//!   self-recursive tail loop — `f(N) -> _ = <<0:(B*8)>>, f(N-1).` — runs its **whole**
//!   computation inside one slice: no park, no prune, and the reservoir grows **linearly with the
//!   loop count**. Measured on this tree: 1.31 GB of dead `binary_alloc` payloads after 20 000
//!   iterations of a 64 KiB binary, with `erlang:memory(binary)` reporting every byte of it; the
//!   same program under `BEAM_RT_TIER=interpreter` (which charges one reduction per instruction)
//!   is a bounded sawtooth of ≤ 203 payloads. That contrast pins the mechanism to the **linked
//!   tail exit**, not to this point: nothing here is skipped, the point is simply never reached.
//!   The repair is a charge on the linked-exit path — a `beam_engine` change
//!   (`jit/baseline/runtime.rs::jit_tail_call`, plus `emit/link.rs`'s `call_only`/`jump` arm for
//!   the complete form) — and `crate::runner::budget`'s *Why a native baseline-JIT block is only
//!   priced, never pre-empted at a call* records the same conclusion for the sibling seams. It is
//!   deliberately **not** repaired here: a host-side charge at an admission door cannot end such a
//!   slice (the engine's position store parks at block granularity, so a mid-block refusal would
//!   re-execute the block's prefix — a wrong result, not a pre-emption), and pruning mid-slice is
//!   unsound outright because a running machine's roots are unpublished (see the two bullets
//!   above).
//!
//! # Reading `erlang:memory(binary)` against this point: the figure is a slice phase, not a leak
//!
//! A dead payload is freed only here, so between two parks `erlang:memory(binary)` reports exactly
//! the payloads the slice has built since the last park, and a read taken at a **fixed iteration
//! offset** therefore always samples the same point of the slice cycle. Measured on this tree with
//! a 4 KiB payload and a 1 000-iteration block: the block reads `0` at **every** rung when it is
//! `999` iterations (the read lands exactly on a park), `k` payloads at rung `k` for `1 000`,
//! `2k` for `1 001` and `3k` for `1 002`. A 2 000-rung `1 000`-iteration ladder is hard-capped at
//! **999 payloads** (one slice) with a reset to `0` every 1 000 rungs, identically at
//! 1 024/2 048/4 096-byte payloads — and the allocator's own block census agrees: after every park
//! `alloc_calls - free_calls == ` the number of payloads still live, i.e. the whole reservoir was
//! released. So a monotonically rising ladder is **not** by itself evidence of a leak. The two
//! tests of a real leak are: does the peak exceed one slice's worth
//! (`DEFAULT_REDUCTIONS_PER_SLICE / this loop's reductions per iteration` payloads), and does it
//! rise with the **rung count** at a fixed total iteration count. A fixed-offset read of a
//! bounded reservoir fails both.
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
//!   `crates/beam_rt/src/context/pump.rs:472`), so a payload that store owns is a second owner
//!   with `refcount() > 1` and survives.
//! * **Nested machines are never pruned — the correct decline.** `RuntimeContext::run_export`,
//!   `apply_fun` and the `-eval` evaluator run a machine synchronously and drop it (adopting its
//!   handles into the escaped store on the way out); such a machine is never parked, so this point
//!   never sees it. Its reservoir dies with it, which is the pre-existing behaviour.
//!
//! The `refcount() > 1` guard in the release predicate is the **belt for anything the four
//! findings above do not cover**: a payload some other owner already cloned is retained whatever
//! this process's roots say, so the pruner never has to model every secondary owner. A false accept
//! is a leak; a false reject remains impossible.
//!
//! # The prune's own valve, and why it is strictly stronger than the collection's
//!
//! The reachability walk this point leans on ([`beam_gc::for_each_reachable_binary_payload`]) is
//! the **collector's** own enumeration, so it enters a term only when that term addresses a
//! **from-space** word of the process's heap (`beam_gc::reachable`, the copier's `Target::Foreign`
//! rule). Everything else — the pointer-stable arena (`Runtime::alloc_words`, the fallback the
//! heap-admission doors take when the nursery cannot grow: `+hmax`, the term-heap cap), the pooled
//! BIF heaps, the `-eval` heap, a message fragment — is skipped **whole**, so a
//! `[BinRef][ErlSubBits]` pair living there names a payload the walk reports as dead. The existing
//! [collection valve](super::valve_verdict) does not catch that shape, and must not: its hazard
//! test is "a foreign container that **forwards** a word a relocation would move", while
//! `BinRef.val` points at an **off-heap payload** that no collection ever moves. So a foreign
//! container of a foreign `BinRef` pair is inert to every collection and lethal to this prune.
//!
//! [`reclaim`] therefore runs a **second, prune-only** valve
//! ([`super::reaches_an_unscanned_word`]) after the collection one: it declines when *any* root
//! word, or *any* child slot of any object reachable from the roots through the collector's
//! scanned graph, is pointer-bearing and lies in a region the collector does not scan. That is a
//! **strict superset** of the collection valve's decline set — dropping the forwarding test can
//! only add declines — and the asymmetry is deliberate:
//!
//! > a **false accept** frees a payload a live `BinRef` still names; a **false reject** keeps a
//! > payload that is already garbage.
//!
//! A false accept is a leak; a false reject is a **dangling pointer**, read later through the
//! frozen term layout. Every decline here opens the per-pid descent latch
//! ([`Runtime::note_foreign_reachable`]) exactly as the collection valve's does, so a process that
//! later drops the offending term also gets its collections back on the fast path.
//!
//! # What the unit suite can and cannot construct (read before "fixing" a case)
//!
//! A machine driven **outside** the runner's armed latch — a nested run, which is what a unit-level
//! fixture is (`RuntimeContext::for_process`) — allocates exactly like an armed slice: the unarmed
//! rooted seam delegates to the unrooted one, so the blocks it builds land on the process's **own
//! scanned young heap**, and the pointer-stable arena is a **refusal-only** fallback (the rule is
//! this node's `CONTEXT.md`, *A young-space replacement needs a complete root set …*). Measured,
//! `process.heap_used_words()` is non-zero after `ogcbuild:build/2` runs and the returned tuple —
//! with the `[BinRef][ErlSubBits]` pair inside it — is classified
//! [`WordRegion::Scanned`](super::WordRegion), so the *fixture's own* returned term is a
//! **retention**.
//!
//! A decline therefore has to be **constructed**, and the suite builds it: the
//! `[BinRef][ErlSubBits]` pair `a_live_term_the_collector_cannot_scan_declines_the_prune` installs
//! as the sole root is written into the runtime's **pointer-stable arena** — the region
//! [`Runtime::alloc_words`] serves and no collector scans — while the process's own young heap is
//! live beside it. That coexistence *is* the hazard. The suite pins every half with roots it
//! installs itself: an empty register file + stack for the **release**
//! (`a_prune_with_clean_roots_releases_every_payload_nothing_reaches`), an on-heap
//! `[BinRef][ErlSubBits]` pair written on the **process's own** young heap for the **retention**
//! (`a_payload_a_scanned_root_reaches_is_retained`), the arena pair for the foreign *root*, and a
//! foreign *child of a scanned root* (`a_foreign_child_of_a_scanned_root_declines_the_prune`) to
//! assert that the collection valve passes exactly what the prune refuses.
//!
//! What is still owed is the **end-to-end** witness: a real `.beam` run through the real scheduler,
//! where the machine's live terms really are on the process heap and the park arm publishes them,
//! showing `erlang:memory(binary)` (and RSS) staying flat across a build-a-binary loop. That belongs
//! in `crates/beam_rt/tests/` as an integration suite, not here.
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
/// 6. the **prune-only valve** ([`super::reaches_an_unscanned_word`]) — the strictly stronger
///    criterion this point needs: *any* pointer-bearing word a root or a scanned-graph child names
///    in a region the collector does not scan (see the module docs' *the prune's own valve*);
/// 7. the **reachability walk** itself ([`beam_gc::for_each_reachable_binary_payload`]) — a walk
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
    let verdict = super::valve_verdict_at(
        crate::context::gc_valve_trace::ValveDoor::BinVheapCollection,
        runtime.walk_scratch(),
        process,
        roots.iter(),
        descend,
    );
    if verdict.declined {
        runtime.note_foreign_reachable(pid);
        return Ok(0);
    }
    // The **prune-only** valve: strictly stronger than the collection valve above, because the
    // prune frees the **off-heap** payload a `BinRef.val` names — a target no relocation would
    // move, so the collection valve (whose hazard test is "a foreign container that forwards a
    // word a collection would relocate") legitimately ignores it. The reachability walk below runs
    // the collector's own admission rule, so a `[BinRef][ErlSubBits]` pair in a region the
    // collector does not scan is *invisible* to it: freeing on that silence is a dangling pointer.
    // See [`super::reaches_an_unscanned_word`] for the criterion and why it declines strictly more
    // often than the collection does.
    let unscanned = super::reaches_an_unscanned_word_at(
        crate::context::gc_valve_trace::ValveDoor::BinVheapPrune,
        runtime.walk_scratch(),
        process,
        roots.iter(),
    );
    if unscanned {
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
    // The released set is decided once, here, so the census below and the release itself can
    // never disagree (the module docs' *Reading `erlang:memory(binary)`* explains why the
    // reservoir must be empty of dead payloads after this call, and why that is the property
    // to assert rather than the absolute figure).
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
        if crate::debug_trace::enabled() {
            // The census that makes a *leak* distinguishable from a *slice phase* without a
            // second run: `retained_live + retained_shared` is every handle this park kept, so
            // `released + retained_live + retained_shared` is the whole reservoir the walk
            // examined. Built only when tracing is on (`all_stats` allocates).
            let retained_live = machine
                .refc_binaries()
                .iter()
                .filter(|binary| live.binary_search(&(binary.data_ptr() as usize)).is_ok())
                .count();
            let retained_shared = machine.refc_binaries().len() - retained_live;
            let binary_live_bytes: u64 = beam_alloc::registry::all_stats()
                .into_iter()
                .filter(|(ty, _)| *ty == beam_alloc::AllocType::Binary)
                .map(|(_, stats)| stats.live_bytes as u64)
                .sum();
            crate::debug_trace::trace(format_args!(
                "off-heap reclamation for {pid:?}: released {released} handle(s), {surviving} \
                 bytes left, binary-vheap limit now {} words, retained_by_live={retained_live} \
                 retained_by_shared={retained_shared} binary_alloc_live={binary_live_bytes} \
                 escaped_store_handles={}",
                reclaimer.limit_words(),
                runtime.eval_off_heap.binaries().len(),
            ));
        } else {
            crate::debug_trace::trace(format_args!(
                "off-heap reclamation for {pid:?}: released {released} handle(s), {surviving} bytes \
                 left, binary-vheap limit now {} words",
                reclaimer.limit_words()
            ));
        }
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

    /// Runs the fixture on a machine this test owns, returning it with one payload per iteration
    /// plus the returned one in its reservoir. **Nothing is published**: each case below installs
    /// the root set it wants to reason about.
    ///
    /// The machine's own register file is deliberately *not* treated as a root set here. The run is
    /// a nested one (`RuntimeContext::for_process`, no armed latch), so its blocks land on the
    /// process's **scanned** young heap (see the module docs), and publishing them would make every
    /// case a *retention* case. A decline has to be built from a root the suite places itself.
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
        machine
    }

    /// Parks a machine that holds **nothing**: an empty register file, an empty stack and no FR
    /// word — the root set of a slice that returned an immediate. No root word is pointer-bearing,
    /// so neither valve can see a payload through them and the prune is free to reason about the
    /// reservoir alone.
    fn publish_empty_roots(table: &mut ProcessTable, pid: ProcessId) {
        let process = table.get_mut(pid).expect("the process");
        let regs = process.registers_mut();
        regs.stack_mut().clear();
        regs.x_mut().set_live(0).expect("an empty register file");
    }

    /// Installs `terms` as `pid`'s **only** roots, in `x(0..terms.len())`.
    fn root_in_registers(table: &mut ProcessTable, pid: ProcessId, terms: &[Eterm]) {
        let process = table.get_mut(pid).expect("the process");
        let regs = process.registers_mut();
        regs.stack_mut().clear();
        regs.x_mut().set_live(terms.len()).expect("the live prefix");
        for (index, &term) in terms.iter().enumerate() {
            regs.x_mut().set(index, term).expect("a live slot");
        }
    }

    /// The process's gathered root set, exactly as [`reclaim`] gathers it (minus the mailbox /
    /// dictionary / saved-argument / signal families, which these cases leave empty).
    fn gathered_roots(table: &ProcessTable, pid: ProcessId) -> Vec<Eterm> {
        let mut roots: Vec<Eterm> = Vec::new();
        table
            .get(pid)
            .expect("the process")
            .registers()
            .gather_roots(&mut roots);
        roots
    }

    /// Writes the on-heap `[BinRef][ErlSubBits]` pair naming `data_ptr` on the process's **own**
    /// young heap and returns the tagged root (the `ErlSubBits`).
    ///
    /// # Safety
    ///
    /// `data_ptr` must be a live, word-aligned payload of `len` bytes that outlives the returned
    /// term — here the machine's reservoir owns it, and the prune never moves a payload.
    #[allow(unsafe_code)]
    unsafe fn pair_on_the_process_heap(
        process: &mut Process,
        data_ptr: *mut u8,
        len: usize,
    ) -> Eterm {
        // SAFETY: the caller keeps the payload alive for the returned term's lifetime, and
        // `data_ptr` is word-aligned (`beam_alloc` hands out `base + HEADER_SIZE`, a unit), so
        // `write_refc_binary`'s flag-bit assert holds. The 8-word pair is allocated on the
        // process's young heap, i.e. inside the region its collector scans.
        unsafe {
            let hp = process
                .heap_mut()
                .alloc_words(beam_term::refc::ERL_REFC_BITS_SIZE)
                .expect("the pair fits the nursery");
            beam_term::refc::write_refc_binary(
                hp,
                data_ptr as *mut Eterm,
                core::ptr::null_mut(),
                data_ptr,
                0,
                len * 8,
            )
        }
    }

    /// Writes the same `[BinRef][ErlSubBits]` pair into the runtime's **pointer-stable arena** — the
    /// region [`Runtime::alloc_words`] serves and no collector ever scans — and returns the tagged
    /// root (the `ErlSubBits`).
    ///
    /// This is the decline fixture's control. The arena is the one region a live `BinRef` pair can
    /// stand in at unit level *while the process's own young heap is live* (the run these cases
    /// share builds on the process heap, see
    /// `a_live_term_the_collector_cannot_scan_declines_the_prune`), and it is precisely the fallback
    /// seam's own region, so the shape is the runtime's rather than a test artefact.
    ///
    /// # Safety
    ///
    /// `data_ptr` must be a live, word-aligned payload of `len` bytes that outlives the returned
    /// term — here the machine's reservoir owns it, and the prune never moves a payload.
    #[allow(unsafe_code)]
    unsafe fn pair_on_the_arena(rt: &mut Runtime, data_ptr: *mut u8, len: usize) -> Eterm {
        // SAFETY: the caller keeps the payload alive for the returned term's lifetime and
        // `data_ptr` is word-aligned (`beam_alloc` hands out `base + HEADER_SIZE`, a unit), so
        // `write_refc_binary`'s flag-bit assert holds. The arena hands out exclusively-owned,
        // writable words that are **never freed and never moved** (no region of it has a release
        // path), so the 8-word pair stays live and addressable for as long as the root is used —
        // and it lies outside every region the process collector relocates, which is the point.
        unsafe {
            let hp = rt
                .alloc_words(beam_term::refc::ERL_REFC_BITS_SIZE)
                .expect("the arena serves an 8-word block");
            beam_term::refc::write_refc_binary(
                hp,
                data_ptr as *mut Eterm,
                core::ptr::null_mut(),
                data_ptr,
                0,
                len * 8,
            )
        }
    }

    /// **The foreign-root decline.** A live `[BinRef][ErlSubBits]` pair standing in a region the
    /// process collector does not scan, installed as the process's **only** root, so the
    /// collector's own reachability walk — the one [`reclaim`] leans on — reports none of the
    /// payload that root names. The collection valve passes it (its hazard test is "forwards a word
    /// a relocation would move", and `BinRef.val` names an **off-heap** payload no collection
    /// moves); the prune's own valve declines it.
    ///
    /// The premise is **constructed**: the shared run is a nested one and builds its blocks on the
    /// process's scanned young heap, so this case has a live heap *and* a root stored outside it —
    /// the exact coexistence in which a collection relocates children out from under a container,
    /// and in which freeing a payload the walk cannot see is a dangling pointer.
    #[test]
    #[allow(unsafe_code)]
    fn a_live_term_the_collector_cannot_scan_declines_the_prune() {
        let mut rt = runtime();
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");
        let mut machine = machine_that_built_payloads(&mut rt, &mut table, pid, 6, 512);
        if machine.refc_binaries().is_empty() {
            return; // the loud skip above
        }
        let before = machine.refc_binaries().len();
        // A payload the loop discarded: the reservoir's handle alone does not make it reachable, so
        // the pair below is what names it — and the walk must still not report it.
        let (data_ptr, len) = machine
            .refc_binaries()
            .iter()
            .find(|binary| binary.as_bytes().iter().all(|byte| *byte == 0))
            .map(|binary| (binary.data_ptr(), binary.as_bytes().len()))
            .expect("a discarded loop payload is in the reservoir");
        // (a) the live pair, deliberately outside every region the collector scans.
        let root = {
            // SAFETY: the reservoir owns `data_ptr` for the whole case and never moves it.
            unsafe { pair_on_the_arena(&mut rt, data_ptr, len) }
        };
        let process = table.get(pid).expect("the process");
        // (b) the process young heap IS live here: the nested run built the fixture's returned tuple
        // (and the copy-outs) on it, so this case carries the crash shape — a live heap *plus* a
        // root the collector never reaches — rather than a vacant one.
        assert!(
            process.heap_used_words() > 0,
            "the nested run builds on the process's scanned young heap, so this fixture has a live \
             heap beside its foreign root"
        );
        // (c) the root word, and the pair's **storage** under it, are outside every scanned region.
        assert_eq!(
            super::super::word_region_exactly(process, root),
            Some(super::super::WordRegion::Foreign),
            "the root must be a container in a region the collector does not scan"
        );
        let sub_bits = beam_term::list::ptr_val(root) as usize;
        let bin_ref = sub_bits - beam_term::refc::bin_ref_size_words() * size_of::<Eterm>();
        for storage in [sub_bits, bin_ref] {
            assert!(
                super::super::outside_scanned_exactly(process.collector(), storage),
                "the pair's own words are outside every region the collector relocates, so no \
                 collection ever visits them"
            );
        }
        // The constructed pair is the process's **only** root (the stack is cleared, x holds it).
        root_in_registers(&mut table, pid, &[root]);
        let roots = gathered_roots(&table, pid);
        assert_eq!(roots, vec![root], "the constructed root is the whole root set");
        let process = table.get(pid).expect("the process");
        // The collection valve passes these roots...
        let collection = super::super::valve_verdict(
            rt.walk_scratch(),
            process,
            roots.iter(),
            /* descend */ true,
        );
        assert!(
            !collection.declined,
            "no foreign container here forwards a word a collection would move"
        );
        // ...and the collector's own walk reports nothing, which is what made the release unsafe.
        let mut reported: Vec<usize> = Vec::new();
        beam_gc::for_each_reachable_binary_payload(process.collector(), &roots, |payload| {
            reported.push(payload)
        })
        .expect("the walk runs");
        assert!(
            reported.is_empty(),
            "the payload walk admits only from-space objects, so a live term in the arena is \
             invisible to it (a false reject if trusted)"
        );
        // The prune's own valve declines, and the prune therefore releases nothing.
        let mut slot: Option<OffHeapReclaimer> = Some(OffHeapReclaimer::new(&impatient_policy()));
        let released = reclaim(&mut rt, &mut table, pid, &mut machine, &mut slot)
            .expect("a decline is not an error");
        assert_eq!(released, 0, "a decline releases nothing");
        assert_eq!(
            machine.refc_binaries().len(),
            before,
            "the whole reservoir is retained"
        );
        assert!(
            rt.foreign_reachable(pid),
            "the decline opens the pid's descent latch, exactly as the collection valve's does"
        );
    }

    /// **The positive release path.** With roots that hold nothing (an empty register file, an
    /// empty stack) the prune runs its walk and releases **every** payload: nothing is live, so
    /// nothing survives, and the process's off-heap byte figure is republished as `0`.
    #[test]
    fn a_prune_with_clean_roots_releases_every_payload_nothing_reaches() {
        let mut rt = runtime();
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");
        let mut machine = machine_that_built_payloads(&mut rt, &mut table, pid, 6, 512);
        if machine.refc_binaries().is_empty() {
            return; // the loud skip above
        }
        let before = machine.refc_binaries().len();
        assert!(
            before >= 7,
            "the fixture must build one payload per iteration plus the returned one, got {before}"
        );
        publish_empty_roots(&mut table, pid);
        let mut slot: Option<OffHeapReclaimer> = Some(OffHeapReclaimer::new(&impatient_policy()));
        let released = reclaim(&mut rt, &mut table, pid, &mut machine, &mut slot)
            .expect("the reclamation does not fail");
        assert_eq!(released, before, "nothing is reachable, so everything goes");
        assert!(
            machine.refc_binaries().is_empty(),
            "the reservoir is empty afterwards"
        );
        let process = table.get(pid).expect("the process");
        assert_eq!(
            process.off_heap_binary_bytes(),
            0,
            "the published off-heap byte figure is the survivors'"
        );
        // The budget was re-sized from the survivors and is primed at the real count, so the next
        // park pays the `O(1)` observation rather than a second derivation.
        let mut reclaimer = slot.expect("the reclaimer");
        assert!(reclaimer.limit_words() >= 1);
        assert_eq!(reclaimer.observe(&machine), 0);
        assert_eq!(
            reclaimer.rederivations(),
            2,
            "one derivation on the trigger, one on the post-release re-prime -- never per park"
        );
    }

    /// **The positive retention path, at unit level.** A payload an on-heap `[BinRef][ErlSubBits]`
    /// pair on the **process's own heap** names is reachable from a root, so the collector's walk
    /// reports it, the prune retains it, and every other payload is released. The retained payload
    /// keeps its exact `data_ptr` — `retain_refc_binaries` compacts handles, never payloads, so no
    /// on-heap `BinRef.val` is invalidated.
    #[test]
    #[allow(unsafe_code)]
    fn a_payload_a_scanned_root_reaches_is_retained() {
        let mut rt = runtime();
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");
        let mut machine = machine_that_built_payloads(&mut rt, &mut table, pid, 6, 512);
        if machine.refc_binaries().is_empty() {
            return; // the loud skip above
        }
        let before = machine.refc_binaries().len();
        // A payload the loop discarded: the pair this case writes names it, which is what makes it
        // reachable — the reservoir's handle alone does not.
        let (data_ptr, len) = machine
            .refc_binaries()
            .iter()
            .find(|binary| binary.as_bytes().iter().all(|byte| *byte == 0))
            .map(|binary| (binary.data_ptr(), binary.as_bytes().len()))
            .expect("a discarded loop payload is in the reservoir");
        let kept = data_ptr as usize;
        let root = {
            let process = table.get_mut(pid).expect("the process");
            // SAFETY: the reservoir owns `data_ptr` for the whole case and never moves it.
            unsafe { pair_on_the_process_heap(process, data_ptr, len) }
        };
        root_in_registers(&mut table, pid, &[root]);
        // The root is inside a region the collector scans: neither valve may decline.
        let roots = gathered_roots(&table, pid);
        let process = table.get(pid).expect("the process");
        assert_eq!(
            super::super::word_region_exactly(process, root),
            Some(super::super::WordRegion::Scanned),
            "the pair was written on the process's own young heap"
        );
        assert!(
            !super::super::reaches_an_unscanned_word(rt.walk_scratch(), process, roots.iter()),
            "a scanned root holding a scanned pair is not a decline"
        );
        let mut reported: Vec<usize> = Vec::new();
        beam_gc::for_each_reachable_binary_payload(process.collector(), &roots, |payload| {
            reported.push(payload)
        })
        .expect("the walk runs");
        assert_eq!(
            reported,
            vec![kept],
            "the collector's walk reports exactly the named payload"
        );
        let mut slot: Option<OffHeapReclaimer> = Some(OffHeapReclaimer::new(&impatient_policy()));
        let released = reclaim(&mut rt, &mut table, pid, &mut machine, &mut slot)
            .expect("the reclamation does not fail");
        assert_eq!(
            released,
            before - 1,
            "every payload but the reached one is released"
        );
        let surviving = machine.refc_binaries();
        assert_eq!(surviving.len(), 1, "exactly the reached payload survives");
        assert_eq!(
            surviving[0].data_ptr() as usize,
            kept,
            "the retained payload keeps its exact data_ptr"
        );
        assert_eq!(surviving[0].as_bytes().len(), len, "and its bytes are intact");
        let process = table.get(pid).expect("the process");
        assert_eq!(
            process.off_heap_binary_bytes(),
            machine.off_heap_overhead_bytes(),
            "the published figure is the survivors'"
        );
        assert!(
            process.off_heap_binary_bytes() > 0,
            "the survivor is charged, not the whole history"
        );
    }

    /// **The prune declines strictly more often than the collection does.** A scanned root whose
    /// *child* is a pointer-bearing word in a region the collector does not scan: a collection
    /// passes it (that container forwards nothing a relocation would move), while the prune must
    /// decline — a `[BinRef][ErlSubBits]` pair inside it would name a payload the walk cannot see,
    /// and freeing that payload is a dangling pointer.
    #[test]
    fn a_foreign_child_of_a_scanned_root_declines_the_prune() {
        let mut rt = runtime();
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");
        let mut machine = machine_that_built_payloads(&mut rt, &mut table, pid, 4, 512);
        if machine.refc_binaries().is_empty() {
            return; // the loud skip above
        }
        let before = machine.refc_binaries().len();
        // A container in a region outside the process's collector (`beam_term::heap` is the
        // test-only host this crate keeps for exactly that), holding immediates only — so a
        // *collection* has nothing to complain about.
        let mut foreign_heap = beam_term::heap::Heap::new(64);
        let foreign = foreign_heap
            .tuple(&[make_small(1), make_small(2)])
            .expect("a foreign tuple");
        // The scanned root: a cons cell on the process's own heap whose car is that container.
        let root = {
            let process = table.get_mut(pid).expect("the process");
            process
                .heap_mut()
                .cons(foreign, beam_term::prelude::NIL)
                .expect("the cons fits the nursery")
        };
        root_in_registers(&mut table, pid, &[root]);
        let roots = gathered_roots(&table, pid);
        let process = table.get(pid).expect("the process");
        assert_eq!(
            super::super::word_region_exactly(process, root),
            Some(super::super::WordRegion::Scanned),
            "the root itself is scanned"
        );
        assert_eq!(
            super::super::word_region_exactly(process, foreign),
            Some(super::super::WordRegion::Foreign),
            "its child is not"
        );
        let collection =
            super::super::valve_verdict(rt.walk_scratch(), process, roots.iter(), /* descend */ true);
        assert!(
            !collection.declined,
            "a collection may proceed: nothing this container forwards would be relocated"
        );
        assert!(
            super::super::reaches_an_unscanned_word(rt.walk_scratch(), process, roots.iter()),
            "the prune must decline: a `BinRef` pair inside that child would name a payload the \
             payload walk can never report"
        );
        let mut slot: Option<OffHeapReclaimer> = Some(OffHeapReclaimer::new(&impatient_policy()));
        let released = reclaim(&mut rt, &mut table, pid, &mut machine, &mut slot)
            .expect("a decline is not an error");
        assert_eq!(released, 0, "the whole reservoir is retained");
        assert_eq!(machine.refc_binaries().len(), before);
        assert!(
            rt.foreign_reachable(pid),
            "and the pid's descent latch is opened"
        );
    }

    /// The `refcount > 1` **belt**: a payload another owner already cloned is retained whatever
    /// this process's roots say. The roots here hold nothing (`publish_empty_roots`), so every
    /// payload the roots do not name is released — except the one the clone owns.
    #[test]
    fn a_payload_another_handle_holds_survives_the_prune() {
        let mut rt = runtime();
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");
        let mut machine = machine_that_built_payloads(&mut rt, &mut table, pid, 4, 512);
        if machine.refc_binaries().is_empty() {
            return; // the loud skip above
        }
        let before = machine.refc_binaries().len();
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
        publish_empty_roots(&mut table, pid);
        let mut slot: Option<OffHeapReclaimer> = Some(OffHeapReclaimer::new(&impatient_policy()));
        let released = reclaim(&mut rt, &mut table, pid, &mut machine, &mut slot)
            .expect("the reclamation does not fail");
        assert_eq!(
            released,
            before - 1,
            "every other discarded payload is released"
        );
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
