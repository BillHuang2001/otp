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

//! The runtime façade: boot, the code server and the **vertical slice**.
//!
//! [`Runtime`] is the whole picture in one object. It owns
//!
//! * the parsed [`RuntimeConfig`] and its init arguments (contract A.6);
//! * the BIF [`BifRegistry`] (`beam_bifs`'s `bif.tab` surface) and a
//!   [`TableRegistry`] (`ets:*`);
//! * the [`CodeServer`] — the **one shared** [`AtomTable`](beam_term::AtomTable),
//!   the module cache and the ordered code path. Every module the runtime loads is
//!   interned into that single table, so atoms built by a BIF are the indices the
//!   engine compares against for **every** loaded module;
//! * the [`ProcessTable`] and the runtime's own [`ProcessId`];
//! * the preloaded-module registry skeleton (contract A.7);
//! * the process [`Heap`] (owned, so a term returned by `run_*` stays valid), a
//!   separate scratch `info_heap` for `system_info`/`format_term`, and the
//!   pointer-stable [`MinimalContext`] arena that serves the engine's
//!   `alloc_words`/`ensure_heap` seam (Milestone-1 has no GC).
//!
//! The **scheduler pass** — the `run_*`/pump entry points, their shared pass body and the
//! port/timer/distribution halves — lives in the [`scheduler`] submodule, so this file stays the
//! façade it describes.
//!
//! `Runtime::run_function`/`run_module` are the evidence for this crate: they load a
//! real `.beam` and execute it through the real [`RuntimeContext`] and the real BIF
//! registry — the same path the `beam_rt` executable takes. The dispatch that makes
//! this a *multi-module* host (native floor → `beam_bifs` registry → load-on-demand
//! external call) lives in [`RuntimeContext::call_bif`](crate::context).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use beam_alloc::Heap;
use beam_bifs::{BifRegistry, OutputSink, TableRegistry};
use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext, MinimalContext};
use beam_gc::HeapPolicy;
use beam_loader::Module;
use beam_process::{
    MonitorRef, Priority, ProcessId, ProcessState, ProcessTable, RefAllocator, Scheduler,
    SignalAtoms,
};
use beam_term::{atom_val, is_atom, make_atom, make_small, Atom, AtomTable, Eterm};

use crate::boot::bootscript::{self, BootScript, BootStop};
use crate::boot::{self, PreloadedModules};
use crate::codeserver::CodeServer;
use crate::config::{ArgTerm, InitArgs, RuntimeConfig};
use crate::context::{PersistentTermStore, RuntimeContext};
use crate::dist::{resolve_cookie, DistNode, NONODE_NAME};
use crate::error;
use crate::eval::FunBody;
use crate::format;
use crate::info::{self, InfoValue};
use crate::mailbox::MailboxState;
use crate::nif::NifRegistry;
use crate::perf_map::PerfMap;
use crate::port::{IoCounters, PortRegistry};
use crate::runner::{ProcessEntry, RunnerState};
use crate::seq_trace::SeqTrace;
use crate::timers::{TimerHandle, Timers};
use crate::trace::TraceState;

mod ets_owner;
mod gc;
mod scheduler;

pub use self::gc::GcStatsSnapshot;
pub(crate) use self::gc::GcState;
pub use self::scheduler::RUN_DEADLINE;

/// The size in term words of the scratch heap `system_info`/`format_term` build on.
///
/// It is `reset()` at the start of every such call, so it only has to hold one
/// materialized value.
const INFO_HEAP_WORDS: usize = 4_096;

/// The smallest process heap a runtime accepts (`heap_words` is floored at this).
const MIN_HEAP_WORDS: usize = 1_024;

/// The size in term words of the runtime's term-construction heaps (the BIF-heap pool and
/// the `-eval` heap) for `config`.
///
/// It is [`RuntimeConfig::heap_words`] (default [`DEFAULT_HEAP_WORDS`]), floored at
/// [`MIN_HEAP_WORDS`] so the smallest program still has room, and **capped by `+hmax`**
/// when that switch is given: `+hmax N` is the emulator's maximum heap size, so a workload
/// needing more than `N` words must hit the limit. Milestone 1 has no collector to grow
/// the heap with (see [`crate::runtime`] and the crate `CONTEXT.md`), so the cap is
/// enforced by *sizing* the heap; the allocation that would exceed it fails and
/// [`crate::error::enforce_term_heap_limit`] turns that into the documented
/// `error:system_limit` raise. A `+hmax` below [`MIN_HEAP_WORDS`] is floored, exactly as
/// `heap_words` is.
fn term_heap_words(config: &RuntimeConfig) -> usize {
    let base = config.heap_words.max(MIN_HEAP_WORDS);
    match config.heap_options().policy().max_heap_words {
        0 => base,
        max => base.min(max.max(MIN_HEAP_WORDS)),
    }
}

/// A process an `erlang:spawn*` created **from inside a scheduler slice**, waiting for the
/// pass to register and enqueue it.
///
/// See [`Runtime::pending_spawns`]: the child's pid is already installed in the pass's
/// process table (with its heap policy and priority) when this record is made; the entry and
/// the arguments are what the pass still has to install into its runner state.
pub(crate) struct PendingSpawn {
    /// The child's pid, already installed in the pass's process table.
    pub(crate) pid: ProcessId,
    /// What the child runs: the resolved module export, or the fun value.
    pub(crate) entry: ProcessEntry,
    /// The arguments seeded into the child's `x(0..arity)` on its first run.
    pub(crate) args: Vec<Eterm>,
}

/// The Rust ERTS runtime.
pub struct Runtime {
    /// The configuration this runtime was created with.
    config: RuntimeConfig,
    /// The BIF registry every `call_bif` is dispatched through.
    ///
    /// Shared through an [`Rc`] so a `BifContext` can be dispatched against a cheap clone
    /// that does not borrow the runtime (see [`crate::context::RuntimeContext::call_registered`]).
    pub(crate) registry: Rc<BifRegistry>,
    /// The code server: the one shared atom table, the module cache, the code path.
    pub(crate) code_server: CodeServer,
    /// The process table (Milestone-1: the runtime's own single process).
    ///
    /// `pub(crate)` so [`RuntimeContext`](crate::context::RuntimeContext) can resolve a
    /// process's mailbox against it (the receive seam) without a runtime accessor.
    pub(crate) processes: ProcessTable,
    /// The per-process **mailbox scan state** — the receive cursor and the
    /// `recv_marker_*` table — keyed by pid.
    ///
    /// It lives here, not in a context, because a `receive` that suspends on an empty
    /// mailbox is resumed against exactly this position once a message arrives (see
    /// `crate::mailbox`). A pid with no entry has never scanned, which is the default
    /// state (cursor at the first message, no markers).
    pub(crate) mailboxes: HashMap<ProcessId, MailboxState>,
    /// The pids a **heap-fragment** delivery woke, waiting to be absorbed into the run queue.
    ///
    /// A delivery into the receiver's own heap records its wake on the process table's wake
    /// list (filled by `ProcessTable::deliver`), which the scheduler drains. A delivery
    /// through the **heap-fragment** model
    /// ([`RuntimeContext::send_message`](crate::context::RuntimeContext)) writes into the
    /// receiver's mailbox directly instead, so its wake has no entry on that list and is
    /// recorded here; [`Runtime::scheduler_pass`] absorbs it into the real run queue at the
    /// same points it absorbs the table's own wake list.
    fragment_wakes: Vec<ProcessId>,
    /// The **process-tracing** state (contract A.8, item 1): one trace entry (flags + tracer)
    /// per traced pid, plus the monotonic delivery counter behind `erlang:trace_delivered/1`.
    ///
    /// It lives on the runtime rather than in a process because it is keyed **by** pid and
    /// outlives every single-`run_*` call: `erlang:trace/3` in one call must be visible to
    /// `erlang:trace_info/2` in the next. See [`crate::trace`] for the model and exactly what is
    /// deferred (event emission and `trace_pattern`).
    trace: TraceState,
    /// The preloaded-module registry skeleton (contract A.7).
    preloaded: PreloadedModules,
    /// The heaps BIF calls allocate on, used as a stack: a call takes one out, runs, and
    /// puts it back (see [`Runtime::take_bif_heap`]). A `beam_alloc::Heap` never moves its
    /// terms and is never reset in Milestone-1 (there is no GC), so a heap must outlive
    /// every term built on it — reusing one heap for sequential calls keeps that free,
    /// while a nested call (inside a fun a BIF applied) gets its own heap.
    pub(crate) bif_heaps: Vec<Heap>,
    /// The size in term words of a freshly created BIF heap.
    pub(crate) bif_heap_words: usize,
    /// The single-threaded scheduler: the run queue of the processes
    /// [`Runtime::spawn_process`] created, plus its thread-progress counter.
    ///
    /// It persists across calls, so [`Runtime::run_one_slice`] followed by
    /// [`Runtime::run_scheduler`] continues exactly where the slice left off.
    scheduler: Scheduler,
    /// The per-pid engine state (the machine, its entry point and the address a blocked
    /// `receive` resumes at) and the recorded outcomes of every spawned process.
    ///
    /// It lives here rather than in a pass-local value because it must outlive a single
    /// scheduler pass — a process blocked in a `receive` is resumed by a *later* pass —
    /// and it is **moved out** for the duration of a pass so the runner can hold it at the
    /// same time as the process table (see [`crate::runner`]).
    runner_state: RunnerState,
    /// The pointer-stable arena serving the engine's `alloc_words` seam.
    pub(crate) arena: MinimalContext,
    /// The **process-heap collection** state: the per-module float guard, the pin/halt sets,
    /// the global escape-store valve and the measured counters (see [`crate::runtime::gc`]).
    gc: GcState,
    /// The scratch heap for `system_info`/`format_term`, reset per call.
    pub(crate) info_heap: Heap,
    /// The long-lived heap for terms the `-eval` evaluator must build on a
    /// [`beam_alloc::Heap`] rather than the engine arena — today a non-byte-aligned
    /// bitstring, which [`beam_codec::term::make_bitstring`] materializes.
    ///
    /// Like [`Runtime::bif_heaps`] it never moves its terms and Milestone-1 never
    /// resets it (there is no GC), so a term built on it stays valid for the runtime's
    /// lifetime.
    pub(crate) eval_heap: Heap,
    /// The **destination** off-heap table for every copy onto [`Runtime::eval_heap`].
    ///
    /// A copy of a term that references an off-heap object (the 8-word `[BinRef][ErlSubBits]`
    /// refc-binary pair, or an MSO container) does not deep-copy the payload: it re-registers
    /// the *owning handle* in the copy's destination table
    /// ([`beam_message::CopyContext::dst_off_heap_mut`]). That table therefore **owns** the
    /// payload references the copied term depends on, so it must outlive both the copied term
    /// and [`Runtime::eval_heap`] — which is exactly why it is a `Runtime` field rather than a
    /// throwaway local (a dropped destination table would free the payloads a stabilized term
    /// still points at). It is only ever grown, never reset: like `eval_heap` it is a
    /// runtime-lifetime store (Milestone 1 has no GC), so it can never free a payload a
    /// stabilized term still needs.
    pub(crate) eval_off_heap: beam_message::OffHeapTable,
    /// The `ets:*` tables.
    ///
    /// Shared through an [`Rc`]`<`[`RefCell`]`>` so a BIF call can hold the registry for
    /// the duration of its call without borrowing the runtime; a **nested** call (a fun
    /// applied by a BIF) finds it already borrowed and runs without ETS — the `ets:*` BIFs
    /// then raise `unsupported_bif`, never a wrong result.
    pub(crate) tables: Rc<RefCell<TableRegistry>>,
    /// The **ETS owner-death index** (contract A.1): the `named_table`s each live process
    /// created, plus the pids observed dead and waiting to be discharged.
    ///
    /// Shared with the [`ProcessExitObserver`](beam_process::ProcessExitObserver) installed on
    /// the process table (see [`Runtime::new`]) — the observer only records a death here,
    /// because it runs inside `beam_process`'s finalizer, which can be reached while the ETS
    /// registry's `RefCell` is already borrowed. The runtime drains it at scheduler-pass
    /// boundaries, where the registry is free (see [`crate::runtime::ets_owner`]).
    pub(crate) ets_owners: Rc<RefCell<ets_owner::OwnerIndex>>,
    /// The port table and the live ports behind `erlang:open_port/2`.
    ///
    /// Shared through an [`Rc`]`<`[`RefCell`]`>` for the same reason as
    /// [`Runtime::tables`]: a BIF call holds it for the duration of its call without
    /// borrowing the runtime, so the fun-caller bridge can borrow the runtime at the same
    /// time (see [`crate::context::RuntimeContext::call_registered`]). A **nested** borrow
    /// (a port operation re-entered from inside a port operation) is reported as a typed
    /// capability gap rather than panicking.
    pub(crate) ports: Rc<RefCell<PortRegistry>>,
    /// The live NIF libraries behind `erlang:load_nif/2` (contract A.3).
    ///
    /// Shared through an [`Rc`]`<`[`RefCell`]`>` for the same reason as
    /// [`Runtime::tables`]/[`Runtime::ports`]: the dispatch that calls a NIF
    /// ([`RuntimeContext::call_bif`](crate::context::RuntimeContext)) holds the registry for
    /// the duration of the call while the runtime stays free for the per-call NIF host. The
    /// registry is `!Send` (it owns `dlopen` handles), which is fine — the runtime is
    /// single-threaded (see [`crate::nif`]).
    pub(crate) nifs: Rc<RefCell<NifRegistry>>,
    /// The node-global **persistent-term** store behind the seven `persistent_term:*`
    /// BIFs.
    ///
    /// Shared through an [`Rc`]`<`[`RefCell`]`>` for the same reason as
    /// [`Runtime::tables`]/[`Runtime::ports`]/[`Runtime::nifs`]: the BIF call holds the
    /// store for the duration of its call without borrowing the runtime, so its arm in
    /// [`RuntimeContext::call_registered`](crate::context::RuntimeContext::call_registered)
    /// can coexist with every other bridge. A **nested** borrow (a persistent-term
    /// operation re-entered from inside one) is reported as a typed capability gap rather
    /// than panicking. The store keeps raw `Eterm` words, which is safe because Milestone-1
    /// has no GC and every heap is pointer-stable (see
    /// [`PersistentTermStore`](crate::context::PersistentTermStore)).
    pub(crate) persistent_terms: Rc<RefCell<PersistentTermStore>>,
    /// The pid code runs as (`erlang:self/0`).
    pub(crate) self_pid: ProcessId,
    /// The node-global **cumulative reduction count** — the `Total` behind
    /// `erlang:statistics(reductions)`/`(exact_reductions)`.
    ///
    /// Bumped by the runner by exactly the reduction units it charged in a slice
    /// ([`Runtime::note_reductions`]), so the figure is real scheduled work rather than a
    /// placeholder. Per-process counters are reset by every slice and could not be summed
    /// into an honest node total; this one is monotonic for the runtime's lifetime.
    reductions: u64,
    /// The anchor of the previous `erlang:statistics(reductions)` read (`SinceLast`).
    reductions_anchor: Cell<u64>,
    /// The anchor of the previous `erlang:statistics(exact_reductions)` read.
    exact_reductions_anchor: Cell<u64>,
    /// The node-global **process-dispatch count** — the `Total` behind
    /// `erlang:statistics(context_switches)`.
    ///
    /// Bumped by the scheduler by the number of slices one pass dispatched
    /// ([`Runtime::note_context_switches`]): one per process the run loop switched to.
    context_switches: u64,
    /// The anchor of the previous `erlang:statistics(context_switches)` read.
    context_switches_anchor: Cell<u64>,
    /// The shared **I/O byte counters** behind `erlang:statistics(io)`.
    ///
    /// A clone of the [`PortRegistry`]'s own handle ([`crate::port::IoCounters`]), bumped
    /// where port bytes really move, so the `statistics(io)` arm reads them without borrowing
    /// the port table.
    io_counters: Rc<IoCounters>,
    /// The **one long-lived CPU-time clock** behind `erlang:statistics(runtime)`.
    ///
    /// [`beam_io::cpu_time::RuntimeClock`] measures the whole process's user CPU time with
    /// `getrusage(RUSAGE_SELF)` and carries the anchor the C keeps in its process-global
    /// `runtime_prev` (`erts/emulator/beam/erl_time_sup.c:1421-1481`). The runtime must own
    /// **one** clock for its whole lifetime: a fresh clock per call would make
    /// `SinceLastCall` always equal `Total` (see `beam_io/src/cpu_time.rs`). `beam_rt` is
    /// `#![deny(unsafe_code)]`, so the syscall lives in `beam_io` behind a safe API and the
    /// clock is read through [`Runtime::cpu_time_statistics`].
    ///
    /// The clock's reading is **stateful** while the `statistics/1` arm only has `&self`, so
    /// the clock sits behind a [`RefCell`] — the same interior mutability the `reductions`/
    /// `context_switches` anchors use for their own `&self` readers. The runtime is
    /// single-threaded, so the borrow can never contend.
    cpu_clock: RefCell<beam_io::cpu_time::RuntimeClock>,
    /// Whether [`Runtime::boot`] has run.
    booted: bool,
    /// The exit status recorded by a native `erlang:halt/1` (see [`crate::native`]).
    pub(crate) halt: Option<i32>,
    /// The **sequential-tracing** state (contract A.8): the per-process trace tokens
    /// `erlang:seq_trace/2` maintains, the node-global serial clock a token's `Serial` is
    /// stamped from, and the system tracer a `print` event would be handed to.
    ///
    /// It lives on the runtime rather than in a process because it is keyed **by** pid and
    /// outlives every single-`run_*` call: `seq_trace:set_token/2` in one call must be visible
    /// to `seq_trace:get_token/0` in the next, and `reset_seq_trace` clears it for *all*
    /// processes. See [`crate::seq_trace`] for the model and exactly what is deferred.
    seq_trace: SeqTrace,
    /// The reference id space every `erlang:make_ref/0` and `erlang:monitor/2` draws from.
    ///
    /// One space for both, so a bare reference and a monitor reference can never collide
    /// (see `crate::native::process`).
    pub(crate) refs: RefAllocator,
    /// Every reference term the runtime minted, keyed by the term itself.
    ///
    /// A reference term is the address of a freshly allocated 4-word thing, so it is a
    /// unique key; the entry is `Some(MonitorRef)` for a monitor reference and `None` for a
    /// bare `make_ref/0`. This is how `erlang:demonitor/1` recovers the monitor a reference
    /// stands for without reading the thing's payload — which would need `unsafe`, and this
    /// crate is `#![deny(unsafe_code)]`.
    pub(crate) references: HashMap<Eterm, Option<MonitorRef>>,
    /// The **per-process group leader** (`erlang:group_leader/0,2`), a pid → pid map.
    ///
    /// A pid with no entry has this runtime's **root** pid (`self_pid`) as its leader — the
    /// process OTP's `init` stands in for here, and the value every spawn inherits unless a
    /// leader was set explicitly (`Runtime::inherit_group_leader`). `erlang:group_leader/0`,
    /// `erlang:group_leader/2`, `erts_internal:group_leader/2,3` and
    /// `process_info(Pid, group_leader)` all read this one map through
    /// [`Runtime::group_leader_of`], so they can never disagree. The model, the oracle
    /// evidence and the divergences live in [`crate::native::group_leader`].
    ///
    /// A terminated process leaves its entry behind, which is harmless: a pid's serial is
    /// bumped when its table slot is reused ([`ProcessTable::take`]), so the stale key names
    /// no process that can ever be observed again.
    pub(crate) group_leaders: HashMap<ProcessId, ProcessId>,
    /// Closures built by the `-eval` evaluator, indexed by the fun term's `index`.
    ///
    /// An `-eval` `fun` may be applied *inside a loaded module* (e.g.
    /// `lists:map(fun(X) -> X*X end, …)` calls back into the closure), so the closure
    /// bodies must outlive the one evaluation and be reachable from
    /// [`RuntimeContext::apply_fun`](crate::context::RuntimeContext). The evaluator
    /// stamps its funs with the sentinel `uniq` [`crate::eval::EVAL_FUN_UNIQ`] and pushes
    /// the body here; the store is append-only, so an index is stable for the runtime's
    /// lifetime.
    pub(crate) eval_closures: Vec<Rc<FunBody>>,
    /// The pending **timer deadlines** (`erlang:send_after/3`).
    ///
    /// A min-heap of absolute deadlines that only the scheduler loop advances: a pass fires
    /// every deadline that has passed (delivering the message into the target's mailbox and
    /// waking it through the normal resume-on-send path) and, with nothing else to do, sleeps
    /// until the earliest one — the timer counterpart of the port half's poll clamp. See
    /// [`crate::timers`] for why this is a runtime-local heap rather than
    /// `beam_process::TimerService` (that wheel is driven by a thread, and the runtime is not
    /// `Send`).
    pub(crate) timers: Timers,
    /// The **timed-out resume address** of every process whose finite `receive … after T`
    /// deadline has fired, keyed by pid.
    ///
    /// A fired deadline must resume its process at the `timeout` instruction — *not* by
    /// delivering a message — so the address travels here from the fire
    /// ([`Runtime::fire_due_timers`]) to the runner ([`crate::runner`]), which consumes it
    /// when it next runs the process. See [`crate::timers::TimerAction::Resume`].
    timer_resumes: HashMap<ProcessId, (u32, u32)>,
    /// The pids a fired receive deadline woke, waiting to be absorbed into the run queue.
    ///
    /// The receive-timeout counterpart of [`Runtime::fragment_wakes`]: the fire makes the
    /// parked process runnable, and [`Runtime::absorb_timer_wakes`] turns it into a run-queue
    /// entry at the same points the other wake sources are absorbed.
    timer_wakes: Vec<ProcessId>,
    /// The processes an `erlang:spawn*` created **from inside a scheduler slice**, waiting for
    /// the pass to register and enqueue them.
    ///
    /// A slice executes against the pass's [`ProcessTable`] (the runtime's own is an empty
    /// placeholder for the duration, see [`Runtime::take_processes`]) but can reach neither the
    /// pass's runner state nor its run queue, so the native spawn
    /// ([`crate::native::process`]) allocates the child's pid in that table — with the child's
    /// heap policy and priority — and records the rest here
    /// ([`Runtime::note_pending_spawn`]). [`Runtime::absorb_pending_spawns`] is where the record
    /// becomes a real schedulable process: its engine entry lands in the pass's runner state and
    /// the pid goes on the pass's run queue, so the child really runs instead of being silently
    /// lost. Drained at the same point as the other wake sources.
    pending_spawns: Vec<PendingSpawn>,
    /// The **distributed node** (contract A.5, Phase 1), when `-name`/`-sname` was given.
    ///
    /// `None` for a runtime with no node name — the default, and what every
    /// differential `-eval` case without a node switch sees. When present, the
    /// node is bound (and published to `epmd`) at construction and driven by
    /// [`Runtime::pump_dist`] from the scheduler pass, next to the port half. See
    /// [`crate::dist`] for exactly how little of distribution this implements.
    dist: Option<DistNode>,
    /// The distribution **cookie** the handshake digests use, resolved once at construction
    /// from `-setcookie`, else `$HOME/.erlang.cookie`, else the documented default (see
    /// [`crate::dist::resolve_cookie`]).
    ///
    /// Kept on the runtime (not only inside [`DistNode`]) so it is observable and mutable
    /// without a live node: [`Runtime::cookie`]/[`Runtime::set_cookie_bytes`] are the runtime
    /// side of `erlang:set_cookie/2`.
    cookie: Vec<u8>,
    /// The atom naming this runtime's **local node**: the configured `node@host` when
    /// `-name`/`-sname` made it a node, else the fixed `nonode@nohost` identity
    /// ([`crate::dist::NONODE_NAME`]) the node bridge also hands `beam_codec`.
    ///
    /// Installed on the process table (see [`Runtime::new`]) so a named monitor can spell its
    /// `{Name, <node>}` `{'DOWN', …}` target, and exposed as
    /// [`Runtime::local_node`]/[`Runtime::local_node_term`].
    local_node: Atom,
    /// Whether a scheduler pass **routes** inbound distribution control messages into
    /// local mailboxes ([`Runtime::dispatch_dist_inbound`]).
    ///
    /// Default **`false`**: a node still accepts a peer, completes the `'N'`
    /// handshake and pumps its links, but nothing a peer sends is enqueued — so the
    /// differential corpus, whose `-eval` probes must stay byte-identical to the
    /// oracle, never observes a delivery it did not ask for.
    /// [`Runtime::allow_inbound_delivery`] turns it on; the explicit
    /// [`Runtime::dispatch_inbound`] entry never consults it (an explicit caller has
    /// already asked).
    inbound_delivery: bool,
    /// The **perf-map / JIT-dump** files `+JPperf` selected (contract A.8).
    ///
    /// Created here — the runtime-construction step, the equivalent of the C's
    /// `beamasm_metadata_early_init` at emulator start — and appended to the moment a native
    /// code range becomes known ([`PerfMap::record`]). The runner feeds it the baseline JIT's
    /// **real** cached ranges as a scheduled process enters a module export (`+JPperf map`
    /// only); an address is never fabricated, and the map stays empty when no such entry runs.
    /// An unopenable file is recorded on the [`PerfMap`] (never raised, never printed),
    /// exactly as the oracle logs it and carries on. See [`crate::perf_map`].
    perf_map: PerfMap,
    /// The open **whole-file descriptors** `prim_file:open/2` handed out and
    /// `prim_file:close/1` has not yet removed (contract A.7).
    ///
    /// Keyed by the reference id minted for the descriptor's `handle`
    /// ([`Runtime::refs_mut`]'s one id space, whose ids are never reused), the value the
    /// host `std::fs::File` the descriptor denotes. The oracle keeps the same table in C
    /// behind a NIF resource and *decodes* the `handle` reference to find it; this runtime
    /// carries the id in the descriptor's own `Data` map instead, because reading a
    /// reference's payload needs `unsafe` and this crate is `#![deny(unsafe_code)]`. The
    /// table is owned by the `Runtime` (not a `thread_local!`) because several runtimes can
    /// live in one thread — a process-global table would let their handles collide. See
    /// [`crate::native::prim_file`].
    open_files: HashMap<u64, std::fs::File>,
    /// The **native `file_server_2` service** and its live io-devices (contract A.9) — see
    /// [`crate::native::file_server`].
    ///
    /// `pub(crate)` because the service's whole implementation lives in that module (its
    /// request decoding, reply shaping and scheduler-pass step); the field itself is the
    /// state that ties the registered `file_server_2` pid to the runtime.
    pub(crate) file_services: crate::native::file_server::FileServices,
    /// The `{Name, Vsn}` header of the OTP boot script this runtime has **loaded**, or
    /// `None` before any script was read/executed — the source of `init:script_id/0`. Set
    /// by [`Runtime::boot_from_script`] / [`Runtime::boot_from_script_reporting`].
    boot_script_id: Option<(String, String)>,
    /// The path of the OTP boot script this runtime has **loaded**, or `None` before any
    /// script was read/executed. Recorded alongside [`Runtime::boot_script_id`] so
    /// [`Runtime::restart`] can re-run the very script the system booted from.
    boot_script_path: Option<PathBuf>,
}

impl Runtime {
    /// Creates a runtime for `config`.
    ///
    /// The registries, the shared atom table, the process table and the heaps are set up
    /// immediately; the A.7 **boot step** runs on demand (or explicitly through
    /// [`Runtime::boot`]). The code path is seeded from `config.code_paths`; the system
    /// library directories are added lazily by [`CodeServer`] when a module is first
    /// needed, so no external process is spawned until then.
    ///
    /// # Errors
    ///
    /// Propagates heap/process allocation failures and an out-of-range `+P`-style
    /// limit.
    pub fn new(config: RuntimeConfig) -> Result<Runtime> {
        let mut code_server = CodeServer::new();
        boot::seed_core_atoms(code_server.atoms_mut());
        // The configured `-pa`/`-pz` directories are *boot-time default* code-path entries: a
        // later `init:restart/0` resets the code path to exactly them plus the system `ebin`
        // directories (see [`Runtime::restart`] and [`CodeServer::reset_paths_to_defaults`]).
        for dir in &config.code_paths {
            code_server.add_default_path(dir.clone());
        }
        let signals = SignalAtoms::from_table(code_server.atoms_mut());
        let mut processes = ProcessTable::new(signals);
        // The ETS owner-death index and the observer that fills it: a process death records the
        // pid here and nothing else, and the runtime drains the obligations at a scheduler
        // boundary where the ETS registry is free (see [`crate::runtime::ets_owner`]). The
        // observer touches only this cell, so it is safe to run from inside the finalizer even
        // while a BIF call holds the ETS registry.
        let ets_owners = Rc::new(RefCell::new(ets_owner::OwnerIndex::default()));
        processes.set_process_exit_observer(Box::new(ets_owner::EtsExitObserver::new(Rc::clone(
            &ets_owners,
        ))));
        // The runtime's own root process carries the emulator's default heap policy — the
        // same one `erlang:spawn` gives a child — so `process_info(Pid, min_heap_size | …)`
        // reports the oracle's figures for it too (see [`Runtime::default_spawn_policy`]).
        let self_pid = processes.spawn(config.heap.policy().clone())?;
        // The runtime's own root process stands in for OTP's `init`: it is the process the
        // runtime boots and runs the start action as, and the one `erlang:group_leader/0`
        // already reports (`src/native/process.rs`). Registering the name is what makes
        // `erlang:whereis(init)` answer that live pid — the oracle answers `<0.0.0>` — and
        // makes `erlang:registered/0`/`erlang:whereis/1` honest about the runtime's own
        // name registry rather than reporting an empty one. `init` is in the core-atom seed
        // interned two lines up, so the lookup cannot miss.
        let init_name = code_server
            .atoms()
            .lookup("init")
            .ok_or_else(|| error::message("boot: the `init` atom is not seeded"))?;
        processes
            .register(init_name, self_pid)
            .map_err(|_| error::message("boot: the `init` name is already registered"))?;
        // The native **`file_server_2` service** (contract A.9): a `Runnable`, never-enqueued
        // process that answers the OTP file protocol, so the real `bootstrap/bin/start.boot`'s
        // final `{apply,{c,erlangrc,[]}}` directive completes instead of dying with
        // `{noproc,{gen_server,call,[file_server_2,…]}}` (see `src/native/file_server.rs`).
        let file_services = crate::native::file_server::FileServices::start(
            &mut processes,
            code_server.atoms_mut(),
            config.heap.policy().clone(),
        )?;
        // A zero-word heap can hold nothing, so the configured size is floored — and a
        // `+hmax` the emulator was given is the ceiling (see `term_heap_words`).
        let heap_words = term_heap_words(&config);
        // A configured node name (`-name`/`-sname`) makes this a **distributed node**:
        // the listener is bound here (a bind failure is a genuine start-up error, never
        // a silently un-distributed runtime) and the node is published to `epmd` on the
        // first scheduler pass. The cookie comes from `-setcookie`/`$HOME/.erlang.cookie`,
        // resolved **once** so the runtime's own `cookie` field and the node agree.
        let cookie = resolve_cookie(&config.init_args);
        let dist = match config.emulator.node_name.as_deref() {
            Some(name) => Some(DistNode::start(name, cookie.clone())?),
            None => None,
        };
        // The local-node identity: the configured node's full `node@host` name when this is a
        // node, else the fixed `nonode@nohost` (the same one the node bridge gives
        // `beam_codec`). Installing it on the process table is what lets a **named** monitor
        // spell its `{'DOWN', …}` target as `{Name, <node>}` instead of the typed gap.
        let local_node_name = match dist.as_ref() {
            Some(node) => node.node_name(),
            None => NONODE_NAME,
        };
        let local_node = code_server.atoms_mut().intern(local_node_name);
        processes.set_local_node(local_node);
        // The perf files are created up front, like the C's `beamasm_metadata_early_init`:
        // the map is then appended to as ranges become known. Nothing is created when the
        // flag selected no file, and a failure to create one is recorded on the `PerfMap`
        // (the oracle logs it and keeps running), never raised here.
        let perf_map = PerfMap::new(config.perf_map.clone());
        // The port registry owns the I/O byte counters; the runtime keeps a clone of the very
        // same `Rc` so `statistics(io)` reads them without borrowing the port table.
        let ports = Rc::new(RefCell::new(PortRegistry::new()?));
        let io_counters = ports.borrow().io_counters();
        Ok(Runtime {
            config,
            registry: Rc::new(BifRegistry::with_core_bifs()),
            code_server,
            processes,
            mailboxes: HashMap::new(),
            fragment_wakes: Vec::new(),
            trace: TraceState::new(),
            preloaded: PreloadedModules::standard(),
            bif_heaps: vec![Heap::new(heap_words)?],
            bif_heap_words: heap_words,
            scheduler: Scheduler::new(),
            runner_state: RunnerState::default(),
            arena: MinimalContext::new(),
            gc: GcState::new(),
            info_heap: Heap::new(INFO_HEAP_WORDS)?,
            eval_heap: Heap::new(heap_words)?,
            eval_off_heap: beam_message::OffHeapTable::new(),
            tables: Rc::new(RefCell::new(TableRegistry::new())),
            ets_owners,
            ports,
            nifs: Rc::new(RefCell::new(NifRegistry::new())),
            persistent_terms: Rc::new(RefCell::new(PersistentTermStore::new())),
            self_pid,
            reductions: 0,
            reductions_anchor: Cell::new(0),
            exact_reductions_anchor: Cell::new(0),
            context_switches: 0,
            context_switches_anchor: Cell::new(0),
            io_counters,
            cpu_clock: RefCell::new(beam_io::cpu_time::RuntimeClock::new()),
            booted: false,
            halt: None,
            seq_trace: SeqTrace::new(),
            refs: RefAllocator::new(),
            references: HashMap::new(),
            group_leaders: HashMap::new(),
            eval_closures: Vec::new(),
            timers: Timers::new(),
            timer_resumes: HashMap::new(),
            timer_wakes: Vec::new(),
            pending_spawns: Vec::new(),
            dist,
            cookie,
            local_node,
            inbound_delivery: false,
            perf_map,
            open_files: HashMap::new(),
            file_services,
            boot_script_id: None,
            boot_script_path: None,
        })
    }

    /// The default configuration: [`RuntimeConfig::default`], booted on demand.
    ///
    /// # Errors
    ///
    /// As [`Runtime::new`].
    pub fn with_defaults() -> Result<Runtime> {
        Runtime::new(RuntimeConfig::default())
    }

    /// Runs the **boot sequence** (contract A.7). Idempotent.
    ///
    /// The registry of preloaded modules is checked, the system code path is added and
    /// the whole preloaded set ([`crate::boot::PRELOADED_MODULE_NAMES`], the erts
    /// application's `.beam`s) is loaded into the shared module cache (see
    /// [`crate::boot::load_preloaded`]). A boot with an empty registry, a missing system
    /// installation or an unparsable preloaded `.beam` is a typed error — never a silent
    /// runtime without its foundation.
    ///
    /// Nothing is printed: the CLI must be silent on stderr for `-eval`/`run`/`-s`, so
    /// boot emits no banner (the version banner is [`Runtime`]'s `-version` action).
    ///
    /// # Errors
    ///
    /// [`BeamError::Raise`](beam_common::BeamError::Raise) when the preloaded-module
    /// registry is empty; otherwise the code server's/loader's typed error for the
    /// preloaded set (see [`crate::boot::load_preloaded`]).
    pub fn boot(&mut self) -> Result<()> {
        if self.booted {
            return Ok(());
        }
        if self.preloaded.is_empty() {
            return Err(error::unsupported("empty preloaded-module registry"));
        }
        boot::load_preloaded(&mut self.code_server, &self.preloaded)?;
        self.booted = true;
        Ok(())
    }

    /// Whether the boot sequence has run.
    pub fn is_booted(&self) -> bool {
        self.booted
    }

    /// The `{Name, Vsn}` header of the boot script this runtime loaded, if any — the source
    /// of `init:script_id/0` (see [`crate::native::init`]).
    ///
    /// `None` until a script has been read through [`Runtime::boot_from_script`] /
    /// [`Runtime::boot_from_script_reporting`].
    pub fn boot_script_id(&self) -> Option<(&str, &str)> {
        self.boot_script_id
            .as_ref()
            .map(|(name, vsn)| (name.as_str(), vsn.as_str()))
    }

    /// The configuration this runtime was created with.
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }
    /// The **perf-map / JIT-dump** files `+JPperf` selected (contract A.8).
    ///
    /// Inactive (no file open) unless the flag selected one; see [`crate::perf_map`] for
    /// exactly what this runtime writes (a map with the oracle's exact line grammar, filled
    /// with the baseline JIT's **real** code ranges as a scheduled process runs, and **no**
    /// fabricated addresses) and why a file it could not create is recorded rather than
    /// raised.
    pub fn perf_map(&self) -> &PerfMap {
        &self.perf_map
    }
    /// The mutable [`PerfMap`], so a caller that *does* know a native code range can append
    /// it through [`PerfMap::record`].
    pub fn perf_map_mut(&mut self) -> &mut PerfMap {
        &mut self.perf_map
    }

    /// The parsed init arguments (the source of `init:get_argument/1`).
    pub fn init_args(&self) -> &InitArgs {
        &self.config.init_args
    }

    /// The preloaded-module registry skeleton (contract A.7).
    pub fn preloaded(&self) -> &PreloadedModules {
        &self.preloaded
    }

    /// The preloaded module names that have been **loaded into the cache** (contract A.7).
    ///
    /// Empty before [`Runtime::boot`]; after a successful boot it holds the whole
    /// [`crate::boot::PRELOADED_MODULE_NAMES`] set. The order is the registry's
    /// (`erlang:pre_loaded/0`'s order), so a caller can compare it directly against
    /// [`PreloadedModules::names`].
    pub fn preloaded_loaded(&self) -> Vec<String> {
        self.preloaded
            .names()
            .filter(|name| self.code_server.is_loaded(name))
            .map(str::to_string)
            .collect()
    }

    /// Whether the preloaded module `name` has been loaded into the cache.
    ///
    /// `false` for a name outside the preloaded registry, whatever the cache holds.
    pub fn is_preloaded_loaded(&self, name: &str) -> bool {
        self.preloaded.is_preloaded(name) && self.code_server.is_loaded(name)
    }

    /// The code server (the shared atom table, the module cache, the code path).
    pub fn code_server(&self) -> &CodeServer {
        &self.code_server
    }

    /// The shared atom table, mutably (a caller interning an atom before a run).
    pub fn code_server_atoms_mut(&mut self) -> &mut AtomTable {
        self.code_server.atoms_mut()
    }

    /// The shared atom table.
    pub fn atoms(&self) -> &AtomTable {
        self.code_server.atoms()
    }

    /// Interns `name` in the shared atom table and returns it as an atom term.
    pub fn intern(&mut self, name: &str) -> Eterm {
        make_atom(self.code_server.atoms_mut().intern(name))
    }

    /// The name of the atom `term`, if `term` is an atom.
    pub fn atom_name_of(&self, term: Eterm) -> Option<&str> {
        if !is_atom(term) {
            return None;
        }
        self.code_server.atoms().name(atom_val(term))
    }

    /// The exit status recorded by a native `erlang:halt/1`, if one ran.
    pub fn halt_status(&self) -> Option<i32> {
        self.halt
    }

    /// The runtime's **sequential-tracing** state (contract A.8) — the per-process trace
    /// tokens, the serial clock and the system tracer (see [`crate::seq_trace`]).
    ///
    /// Read-only, for the introspection surface: `erlang:seq_trace_info/1` and
    /// `process_info(Pid, sequential_trace_token)` both answer from it.
    pub(crate) fn seq_trace(&self) -> &SeqTrace {
        &self.seq_trace
    }

    /// The mutable sequential-tracing state — what `erlang:seq_trace/2`,
    /// `erlang:seq_trace_print/1,2` and `erlang:system_flag(reset_seq_trace, _)` mutate.
    pub(crate) fn seq_trace_mut(&mut self) -> &mut SeqTrace {
        &mut self.seq_trace
    }

    /// The **process-tracing** state (contract A.8, item 1) — the trace flags and tracers
    /// `erlang:trace/3` maintains, read by `erlang:trace_info/2` (see [`crate::trace`]).
    pub(crate) fn trace(&self) -> &TraceState {
        &self.trace
    }

    /// The mutable process-tracing state — what `erts_internal:trace/3` and
    /// `erlang:trace_delivered/1` mutate.
    pub(crate) fn trace_mut(&mut self) -> &mut TraceState {
        &mut self.trace
    }

    /// The installed **function trace patterns** — the `erlang:trace_pattern/2,3` state
    /// (contract A.8, item 1), read-only.
    ///
    /// No BIF reports this table back (`erlang:trace_info/2`'s `match_spec` is a *process*
    /// match spec), so the accessor exists for the suites that pin an install or a removal and
    /// for a future `beam_engine` per-call hook — see [`crate::trace::PatternTable`].
    pub fn trace_patterns(&self) -> &crate::trace::PatternTable {
        self.trace.patterns()
    }

    /// Heap words still available to the running code (unbounded in Milestone-1).
    pub fn arena_available(&self) -> usize {
        self.arena.heap_available()
    }

    /// The runtime's one reference id space (the source of `erlang:make_ref/0` and every
    /// `erlang:monitor/2` reference — see [`Runtime::references`]).
    pub fn refs_mut(&mut self) -> &mut RefAllocator {
        &mut self.refs
    }

    /// The runtime's pending timer deadlines (`erlang:send_after/3`).
    ///
    /// Scheduling and cancellation are the only operations the BIF floor needs; the
    /// deadlines themselves are fired by the scheduler loop (see [`Runtime::scheduler_pass`]).
    pub(crate) fn timers_mut(&mut self) -> &mut Timers {
        &mut self.timers
    }

    /// Registers the **finite receive timeout** a process just parked on, returning the
    /// handle that cancels it.
    ///
    /// Called by the runner when it parks a process at a `wait_timeout` whose operand is a
    /// statically known positive delay (see [`crate::runner`]); on expiry the deadline
    /// resumes the process at `(func, pc)` — the `timeout` instruction — instead of
    /// delivering a message. The handle is what lets the runner cancel the deadline when a
    /// real message wakes the receiver first.
    pub(crate) fn schedule_receive_timeout(
        &mut self,
        pid: ProcessId,
        delay: Duration,
        func: u32,
        pc: u32,
    ) -> TimerHandle {
        self.timers.schedule_resume(delay, pid, func, pc)
    }

    /// Cancels a receive deadline [scheduled](Runtime::schedule_receive_timeout) for a
    /// receiver that a real message has already woken.
    ///
    /// Cancelling a deadline that has already fired is a no-op: the process then resumes on
    /// its timed-out path, which is exactly the oracle's own timeout/message race.
    pub(crate) fn cancel_receive_timeout(&mut self, handle: TimerHandle) {
        self.timers.cancel_handle(handle);
    }

    /// The address a **fired** receive deadline wants `pid` resumed at, consuming it.
    ///
    /// The runner asks this when it resumes a parked process: `Some(address)` means the
    /// deadline beat the message (resume at the timed-out path), `None` means the process was
    /// woken by a message (resume the receive scan).
    pub(crate) fn take_timer_resume(&mut self, pid: ProcessId) -> Option<(u32, u32)> {
        self.timer_resumes.remove(&pid)
    }

    /// The open **whole-file-descriptor** table (contract A.7): the host files
    /// `prim_file:open/2` handed a descriptor for and `prim_file:close/1` has not yet
    /// removed.
    ///
    /// Keyed by the reference id the descriptor's `handle` carries; see the field's own
    /// docs for why the key is carried in the clear and the table is per-`Runtime`.
    /// [`crate::native::prim_file`] is the module that mutates it.
    pub(crate) fn open_files_mut(&mut self) -> &mut HashMap<u64, std::fs::File> {
        &mut self.open_files
    }

    /// Takes a heap for one BIF call out of the pool.
    ///
    /// The pool is a stack: a call takes a heap, runs, and puts it back
    /// ([`Runtime::put_bif_heap`]). Reusing the same heap for sequential calls costs
    /// nothing and keeps every term built on it valid (a `beam_alloc::Heap` never moves
    /// its terms and Milestone-1 never resets one — there is no GC). A **nested** call (a
    /// fun a BIF applied) finds the pool empty and gets a fresh heap, so the outer call's
    /// heap is never aliased.
    ///
    /// # Errors
    ///
    /// Propagates the fresh heap's allocation failure.
    pub(crate) fn take_bif_heap(&mut self) -> Result<Heap> {
        match self.bif_heaps.pop() {
            Some(heap) => Ok(heap),
            None => Heap::new(self.bif_heap_words),
        }
    }

    /// Returns a heap taken by [`Runtime::take_bif_heap`] to the pool.
    pub(crate) fn put_bif_heap(&mut self, heap: Heap) {
        self.bif_heaps.push(heap);
    }
    /// Allocates `n` raw heap words from the pointer-stable arena.
    ///
    /// # Errors
    ///
    /// Propagates the arena's allocation failure.
    pub fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.arena.alloc_words(n)
    }

    /// Copies a scheduled process's return value onto the runtime's long-lived,
    /// never-reset `-eval` arena and returns the copy.
    ///
    /// A process's code runs on a heap that dies with it, and a value taken out of the
    /// mailbox lives in exactly that heap — so an `x(0)` that is a container or a binary
    /// would dangle the moment the process terminated. The runner records every return
    /// value through here ([`crate::runner`]) so
    /// [`Runtime::process_result`](Runtime::process_result) really does hand back a term
    /// valid for the runtime's lifetime — the property the scheduler tests rely on (see
    /// `tests/port_wake.rs`, where the receiver returns the port's stream binary).
    ///
    /// The destination is the same pointer-stable arena the `-eval` evaluator's
    /// non-byte-aligned bitstrings use ([`Runtime::eval_heap`]); nothing resets it in
    /// Milestone 1. An immediate is returned unchanged (the copier shares immediates).
    ///
    /// # Errors
    ///
    /// The arena's allocation failure when the value does not fit, translated to the
    /// emulator's documented `error:system_limit` — an honest error, never a dangling term
    /// (see [`crate::error::enforce_term_heap_limit`]).
    ///
    /// **A value carrying an off-heap payload is refused with `BeamError::BadArg`** — the
    /// runtime's `error:badarg`, which is not what the value raised. The copy runs through
    /// `beam_message::copy_struct_simple`, which builds **empty** source *and* destination
    /// off-heap tables (`beam_message/src/copy.rs:761-763`), so the refc-binary arm cannot
    /// resolve the payload's owning handle out of the *source* table and refuses
    /// (`beam_message/src/copy.rs:487-490`, the `bad_term()` class
    /// [`Runtime::try_stabilize_escaped`] documents as the off-heap-payload capability gap).
    /// An escape store may keep the original term and freeze collection; a *return value*
    /// may not — the process heap dies with the process, taking the 8-word
    /// `[BinRef][ErlSubBits]` descriptor with it and, with the producing `Machine`, the
    /// payload itself. So a program whose result is an off-heap binary — `base64:encode/1`,
    /// whose `bs_init_writable` accumulator is always a refc binary even when empty — answers
    /// `beam_rt: badarg` (exit 1) where the oracle prints the binary (see
    /// `tests/empty_binary_result.rs`).
    ///
    /// Closing it needs the copy engine to materialize such a payload itself: a
    /// self-contained ("flatten") copy mode in `beam_message` — the window flattening
    /// `copy_match_context` already performs for a match context works verbatim on a refc
    /// `ErlSubBits`, and its sizing must count the flattened heap-bits block instead of
    /// `ERL_REFC_BITS_SIZE` (`beam_message/src/copy.rs:309-312`) — adopted here as this one
    /// call. `beam_message` is outside this crate's node, so the refusal stands until that
    /// entry point exists.
    pub(crate) fn stabilize_term(&mut self, term: Eterm) -> Result<Eterm> {
        beam_message::copy_struct_simple(term, &mut self.eval_heap)
            .map_err(error::enforce_term_heap_limit)
    }

    /// [`Runtime::stabilize_term`] for a term that may reference the **off-heap payloads** an
    /// engine machine built (`Machine::refc_binaries`), resolved against `src_binaries`.
    ///
    /// A `bs_init_writable`/`bs_create_bin` result larger than
    /// [`beam_term::refc::ERL_ONHEAP_BINARY_LIMIT`] bytes is a frozen 8-word
    /// `[BinRef][ErlSubBits]` pair whose payload lives off the process heap, owned by a
    /// [`beam_alloc::RefcBinary`] handle the machine keeps (`Machine::refc_binaries`). The copy
    /// engine resolves such a pair by its data pointer through the copy's **source** off-heap
    /// table ([`beam_message::OffHeapTable::binary_by_data_ptr`]); the empty source table the
    /// simple form uses cannot serve that lookup, so a plain `stabilize_term` of an
    /// engine-built off-heap binary is refused with [`BeamError::BadArg`] (a **wrong** answer
    /// where the term is perfectly copyable).
    ///
    /// This form fixes that: it clones each `src_binaries` handle (refcount++) into a source
    /// table, copies with a real [`beam_message::CopyContext`], and pushes the referenced
    /// handles into [`Runtime::eval_off_heap`] — the destination table that **owns** the payload
    /// references the copy depends on. `src_binaries` must be the payload handles of the
    /// machine that built `term`; the caller (the runner, which drives that machine) is the only
    /// one that can name them.
    ///
    /// # Errors
    ///
    /// As [`Runtime::stabilize_term`]: the documented `error:system_limit` when the copy
    /// genuinely does not fit the arena ([`crate::error::enforce_term_heap_limit`]).
    pub(crate) fn stabilize_term_with(
        &mut self,
        term: Eterm,
        src_binaries: &[beam_alloc::RefcBinary],
    ) -> Result<Eterm> {
        // No engine handles reachable on this path: the simple, documented form is exactly
        // equivalent (an empty source table cannot resolve an off-heap payload either way).
        if src_binaries.is_empty() {
            return self.stabilize_term(term);
        }
        // The copy's source table: one clone (refcount++) per engine payload handle, so the
        // lookup in `copy_refc_binary` resolves `term`'s `BinRef.val` to a live handle.
        let mut source = beam_message::OffHeapTable::new();
        for handle in src_binaries {
            source.push_binary(handle.clone());
        }
        // `eval_off_heap` is the copy's destination table: the `RefcBinary` handles the copy
        // re-registers there own the payloads the copied term points at, so they must outlive
        // the copy — which is why they live on the runtime (see the field docs).
        let mut ctx = beam_message::CopyContext::new(&source, &mut self.eval_off_heap);
        beam_message::copy_struct(term, &mut self.eval_heap, &mut ctx)
            .map_err(error::enforce_term_heap_limit)
    }
    /// Ensures `heap_need` words are available in the arena.
    ///
    /// # Errors
    ///
    /// Propagates the arena's failure (never in Milestone-1; see the module docs).
    pub fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.arena.ensure_heap(heap_need, live, x_regs, stack)
    }

    /// The process table (a fresh runtime holds the runtime's own `init` root process and
    /// the native `file_server_2` service; user code adds the rest).
    pub fn processes(&self) -> &ProcessTable {
        &self.processes
    }

    /// The per-pid engine state (the machine and its entry point), for the crash-dump
    /// writer's per-process register rendering.
    ///
    /// `crash_dump` (a sibling module of this crate) reaches a scheduled process's engine
    /// through this accessor; the same read-only [`RunnerState::engine_at`] seam backs
    /// `process_info/2`'s execution-position items.
    pub(crate) fn runner_state(&self) -> &RunnerState {
        &self.runner_state
    }

    /// The pid the runtime runs as.
    pub fn self_pid(&self) -> ProcessId {
        self.self_pid
    }

    /// The **group leader** of `pid` (`erlang:group_leader/0,2`).
    ///
    /// A pid with no explicit entry has the runtime's own root pid as its leader — the
    /// process OTP's `init` stands in for here (see [`Runtime::group_leaders`]). This is the
    /// **one** source of truth every reader shares — the runtime-native
    /// [`erlang:group_leader/0,2`](crate::native::group_leader) floor, the
    /// `ProcessHook::group_leader` bridge ([`crate::context::process_bridge`]) and
    /// `erlang:process_info/2`'s `group_leader` item — so they can never disagree about the
    /// same process.
    ///
    /// The answer is a real, live pid (or a pid that *was* one when set — the oracle accepts a
    /// dead leader), never an invented term.
    pub(crate) fn group_leader_of(&self, pid: ProcessId) -> ProcessId {
        self.group_leaders.get(&pid).copied().unwrap_or(self.self_pid)
    }

    /// Sets `pid`'s group leader (`erlang:group_leader/2` and
    /// `erts_internal:group_leader/2`).
    ///
    /// The leader is recorded as given and is **not** required to be live: the C's
    /// `erts_internal_group_leader_2` only checks the first argument is a pid
    /// (`bif.c:4912-4947`), so a dead leader is accepted (probed against the oracle:
    /// `erlang:group_leader(<dead pid>, self())` ⇒ `true`).
    pub(crate) fn set_group_leader(&mut self, pid: ProcessId, leader: ProcessId) {
        self.group_leaders.insert(pid, leader);
    }

    /// Gives a freshly spawned `child` the group leader its `parent` has
    /// (`erl_process.c:12654-12663`).
    ///
    /// A child with no explicit `spawn_opt` leader inherits its parent's, so an explicit
    /// `erlang:group_leader(GL, Parent)` is observed by everything `Parent` later spawns (and
    /// `group_leader/2` reported `ProcessInfo` for the child). A parent whose leader is the
    /// root **default** leaves the child with no entry at all — [`Runtime::group_leader_of`]
    /// then answers the same pid, so the map stays as small as the explicit sets.
    pub(crate) fn inherit_group_leader(&mut self, parent: ProcessId, child: ProcessId) {
        let leader = self.group_leader_of(parent);
        if leader != self.self_pid {
            self.group_leaders.insert(child, leader);
        }
    }

    /// Loads `path` into the code server and returns the cached module.
    ///
    /// A module loaded twice replaces the cached one (mirroring a code reload).
    ///
    /// # Errors
    ///
    /// Propagates the loader's error (a malformed container, an unsupported chunk, …)
    /// or returns a diagnostic when the module has no resolvable name.
    pub fn load_module(&mut self, path: impl AsRef<Path>) -> Result<&Module> {
        let module = self.code_server.load_path(path)?;
        let name = module
            .atom_name(module.name)
            .ok_or_else(|| error::unknown_module("<unnamed .beam>"))?
            .to_string();
        self.code_server
            .module(&name)
            .ok_or_else(|| error::unknown_module(&name))
    }

    /// A cached module by name, if it has been loaded.
    pub fn module(&self, name: &str) -> Option<&Module> {
        self.code_server.module(name)
    }

    /// The names of the cached modules, sorted.
    pub fn loaded_modules(&self) -> Vec<String> {
        self.code_server.loaded()
    }

    /// Finds `<module>.beam` in the configured code path (then the system paths).
    pub fn find_module_file(&mut self, module: &str) -> Option<PathBuf> {
        if let Some(found) = self.code_server.find_file(module) {
            return Some(found);
        }
        // The code server adds the system library paths lazily; a failure here is
        // reported honestly by `CodeServer::load`, so an exploratory lookup is `None`.
        if self.code_server.add_system_paths().is_ok() {
            self.code_server.find_file(module)
        } else {
            None
        }
    }

    /// Loads a module **by name** from the code path into the shared cache (contract A.7).
    ///
    /// This is the boot script's `{primLoad, …}`/`{module, …}`/`{preLoaded, …}` loader: it
    /// resolves `<name>.beam` through the code server's ordered path (and the system `ebin`
    /// directories) and memoizes the result. Loading a module twice is a no-op.
    ///
    /// # Errors
    ///
    /// The code server's errors: the bare `error:undef` raise when `<name>.beam` is on no
    /// code-path directory (never a silent skip), plus the loader's error for a malformed
    /// image. See [`crate::codeserver::CodeServer::load`].
    pub fn load_code_module(&mut self, name: &str) -> Result<()> {
        self.code_server.load(name).map(|_| ())
    }

    /// Appends a boot script's `{path, [Dir]}` directory to the end of the code path.
    ///
    /// [`crate::boot::bootscript`] expands a leading `$ROOT` before calling this, so the
    /// argument is an ordinary directory; a directory already present is not added twice.
    pub fn add_code_path(&mut self, dir: impl Into<PathBuf>) {
        self.code_server.add_path(dir);
    }

    /// Registers `pid` under the name `name` (the explicit-registration half of the boot
    /// name space, contract A.7).
    ///
    /// The name is interned into the runtime's shared atom table, so a later
    /// `erlang:whereis/1` resolves it.
    ///
    /// The boot-script executor **does not** use this for a `{kernelProcess, …}` directive:
    /// `init` never registers a kernel process's name — the process registers **itself** (see
    /// [`crate::boot::bootscript`] and the private `drive::start_kernel_process`), and a
    /// pre-registration would turn that self-registration into the `badarg` "name already
    /// registered" that kills the process. This stays the explicit entry point for a caller
    /// that really needs to install a name before a process runs.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] when `name` is already registered (the process table's
    /// `name_taken` rule) — never a silent overwrite.
    pub fn register_boot_process(&mut self, name: &str, pid: ProcessId) -> Result<()> {
        let atom = self.code_server.atoms_mut().intern(name);
        self.processes.register(atom, pid)
    }

    /// Unlinks `a` and `b` in both directions (`ProcessTable::unlink`).
    ///
    /// This is the boot-script executor's seam for keeping a `{kernelProcess, …}` alive (see
    /// the private `drive::start_kernel_process`). A real `init` runs each kernel-process
    /// `apply` in the **one** long-lived process, so a `spawn_link` inside it
    /// (`application_controller:start/1`, `logger_server:start_link/0`) is linked to a parent
    /// that never exits. This runtime runs the directive in a *short-lived* process, whose
    /// `normal` exit would otherwise tear the kernel server down: a `gen_server`/`proc_lib`
    /// server traps exits and terminates on its parent's `{'EXIT', _, normal}`. Unlinking the
    /// server from the directive **before the directive can exit** reproduces `init`'s
    /// supervision without a long-lived stand-in process.
    ///
    /// Both pids must be **live**: calling this once the directive has already terminated is
    /// the process table's typed unknown-pid diagnostic, so a caller that cannot prove
    /// liveness (a `{kernelProcess, …}` whose `apply` returns before the process it spawned
    /// registers, such as `heart`) checks [`Runtime::processes`]`().contains(..)` first.
    ///
    /// # Errors
    ///
    /// The process table's typed diagnostic when either pid is not a live process of this
    /// runtime's table — never a panic, never a silent no-op.
    pub fn unlink_processes(&mut self, a: ProcessId, b: ProcessId) -> Result<()> {
        self.processes.unlink(a, b)
    }

    /// Reads the OTP **boot script** at `path` and executes it (contract A.7, part 2).
    ///
    /// The runtime boots first (the preloaded set and the system code path), the script is
    /// parsed into its typed form ([`crate::boot::bootscript::read`]) and then executed in
    /// instruction order ([`crate::boot::bootscript::execute`]). Execution stops at the
    /// first directive that cannot run, returning its typed error — the real `start.boot`
    /// runs all 22 of its directives through to `{progress,started}`, and a script that
    /// still needs machinery M1 does not have stops at the first such directive (see the
    /// `bootscript` module docs). Nothing is printed.
    ///
    /// # Errors
    ///
    /// The read/parse errors of [`crate::boot::bootscript::read`] and the first directive's
    /// error from [`crate::boot::bootscript::execute`].
    pub fn boot_from_script(&mut self, path: impl AsRef<Path>) -> Result<BootScript> {
        self.boot_if_needed()?;
        let script = bootscript::read(path.as_ref())?;
        // Record the script's real `{Name, Vsn}` header before executing it, so
        // `init:script_id/0` can answer it even if execution later stops early — and record
        // the path, so `init:restart/0` can re-run the very script the system booted from.
        self.boot_script_id = Some((script.name.clone(), script.vsn.clone()));
        self.boot_script_path = Some(path.as_ref().to_path_buf());
        bootscript::execute(self, &script)?;
        Ok(script)
    }

    /// Reads the OTP **boot script** at `path` and executes it, **reporting the exact
    /// directive the boot stopped at** ([`crate::boot::bootscript::BootStop`]).
    ///
    /// This is [`Runtime::boot_from_script`] with the failure's position preserved: on a
    /// directive that cannot run it hands back a [`BootStop`] carrying the 0-based
    /// `index`, the script's `total`, the failing `instruction` and the `error`, instead of
    /// flattening those into the error alone. The two entry points share one execution
    /// ([`crate::boot::bootscript::execute_reporting`]), so a script's stopping point is
    /// identical whichever is used.
    ///
    /// A failure **before** the first directive — the runtime cannot boot, or `path` is not a
    /// readable/parseable script — is reported with `index`/`instruction` unset (no directive
    /// ran), exactly as [`crate::boot::bootscript::execute_reporting`] reports its own
    /// pre-directive failure.
    ///
    /// # Errors
    ///
    /// A [`BootStop`] for the first directive that cannot run, or one with `index`/`instruction`
    /// unset when the runtime cannot boot or the script cannot be read/parsed.
    pub fn boot_from_script_reporting(
        &mut self,
        path: impl AsRef<Path>,
    ) -> std::result::Result<(), BootStop> {
        let script = match self.boot_if_needed().and_then(|()| bootscript::read(path.as_ref())) {
            Ok(script) => script,
            // No directive ran: the pre-directive failure shape `execute_reporting` uses.
            Err(error) => {
                return Err(BootStop {
                    index: None,
                    total: 0,
                    instruction: None,
                    error,
                })
            }
        };
        self.boot_script_id = Some((script.name.clone(), script.vsn.clone()));
        self.boot_script_path = Some(path.as_ref().to_path_buf());
        bootscript::execute_reporting(self, &script)
    }

    /// The path of the boot script this runtime has **loaded**, if any — the source of
    /// [`Runtime::restart`]'s re-run (and the file whose header `init:script_id/0` reports).
    ///
    /// `None` until a script has been read through [`Runtime::boot_from_script`] /
    /// [`Runtime::boot_from_script_reporting`] — in particular on the plain `-eval`/`run`
    /// path, which boots the preloaded set only.
    pub fn boot_script_path(&self) -> Option<&Path> {
        self.boot_script_path.as_deref()
    }

    /// The **in-process reboot** behind `init:restart/0`
    /// (`erts/preloaded/src/init.erl:492-518,937-954`).
    ///
    /// The oracle's `restart/0` only sends `init ! {stop,restart}` and answers `ok` at once;
    /// it is the `init` process that takes the applications down, unloads code, closes the
    /// ports and boots again. This runtime has no live `init` process, so the reboot is
    /// performed here, reaching the same **observable** end state — the node is booted again
    /// and keeps answering:
    ///
    /// 1. the code path is reset to its **boot-time default**
    ///    ([`CodeServer::reset_paths_to_defaults`]): the `-pa`/`-pz` directories the runtime
    ///    was configured with plus the system `ebin` directories, dropping whatever a boot
    ///    script's `{path, …}` directive or a later [`Runtime::add_code_path`] appended;
    /// 2. the boot sequence is re-run **forcibly** — the `booted` flag is what makes
    ///    [`Runtime::boot`] idempotent, so it is cleared first — re-establishing the whole
    ///    preloaded set;
    /// 3. the boot script this runtime loaded, if any, is executed again
    ///    ([`Runtime::boot_from_script`]), exactly as the oracle's `do_restart/3` re-runs
    ///    `boot(Start, Flags, Args)` (`init.erl:951-954`).
    ///
    /// # Fidelity limit (honest, not hidden)
    ///
    /// The oracle's restart first **clears the system** — it takes every application down,
    /// unloads all code and closes every port (`init.erl:929-962`). This runtime models no
    /// application-owned process or port to take down, and never unloads a module, so that
    /// teardown is *approximated* by re-running the boot sequence: the module cache and any
    /// live boot-spawned process are left in place. What a caller can observe — the node is
    /// booted again, the code path is back to its default and `init:*`/`code:*` still answer
    /// — is therefore reproduced, with the documented difference that re-running the real
    /// `start.boot` re-runs its `{kernelProcess, …}` directives against servers that are
    /// **already** registered (a re-registration is the process table's `badarg`, which the
    /// boot drive tolerates; the existing server keeps the name).
    ///
    /// A failure at any step is reported as its typed error — never a silently half-restarted
    /// runtime.
    ///
    /// # Errors
    ///
    /// [`Runtime::boot`]'s errors (an unresolvable system installation, an unparsable
    /// preloaded `.beam`) and [`Runtime::boot_from_script`]'s (a script that cannot be read,
    /// parsed or executed).
    pub fn restart(&mut self) -> Result<()> {
        self.code_server.reset_paths_to_defaults();
        self.booted = false;
        self.boot()?;
        if let Some(path) = self.boot_script_path.clone() {
            self.boot_from_script(path)?;
        }
        Ok(())
    }

    /// **The slice.** Loads `path`, caches the module, runs `function/arity` with
    /// `args` and returns `x(0)`.
    ///
    /// `io:format/2` output of the executed code goes to the process's stdout; the
    /// *result* is not printed (the caller decides — see [`crate::cli`]).
    ///
    /// # Errors
    ///
    /// * the loader's error for `path`;
    /// * a diagnostic when the module has no name or does not export `function/arity`;
    /// * any error the running code raises (BIF failures included).
    pub fn run_function(
        &mut self,
        path: impl AsRef<Path>,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        self.boot_if_needed()?;
        let module = self.code_server.load_path(path)?;
        let mut context = RuntimeContext::new(self);
        context.run_export(&module, function, arity, args)
    }

    /// The general primitive: run `function/arity` against a **caller-owned** module
    /// with an explicit output sink.
    ///
    /// # Errors
    ///
    /// A diagnostic when `function/arity` is not in the module's atom table or is not
    /// exported, plus any error the running code raises.
    pub fn run_module(
        &mut self,
        module: &Module,
        function: &str,
        arity: u32,
        args: &[Eterm],
        out: &mut dyn OutputSink,
    ) -> Result<Eterm> {
        self.boot_if_needed()?;
        let mut context = RuntimeContext::with_output(self, out);
        context.run_export(module, function, arity, args)
    }

    /// Loads the module `name` from the code path and runs its exported
    /// `function/arity`, discarding nothing (the result is returned).
    ///
    /// This is what `-s`/`-run` use: the module is resolved through the code path
    /// (including the system library directories), not from a file argument.
    ///
    /// # Errors
    ///
    /// The code server's not-found diagnostic, plus any error the running code raises.
    pub fn run_named(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        self.boot_if_needed()?;
        // `find_module_file` also seeds the system library directories (stdlib, kernel)
        // on first use, so `-s lists …` reaches the real stdlib. A miss here is not
        // fatal: `CodeServer::load` below reports it, naming every directory searched.
        let _ = self.find_module_file(module);
        let loaded = self.code_server.load(module)?;
        let mut context = RuntimeContext::new(self);
        context.run_export(&loaded, function, arity, args)
    }

    // --- spawning and scheduling (the process runner's public API) ------------

    /// Spawns a process whose entry point is `module:function/arity(args)` and makes it
    /// **runnable** (it is appended to the scheduler's run queue).
    ///
    /// The module is resolved through the code path exactly as an external call is (`-pa`/
    /// `-pz` directories first, then the system library `ebin`s), so it is loaded on
    /// demand and cached in the one shared atom table. `args` must already be valid terms:
    /// the runtime never moves them (Milestone 1 has no GC), and they are seeded into the
    /// process's `x(0..arity)` when it first runs.
    ///
    /// The process does **not** run until [`Runtime::run_slices`]/
    /// [`Runtime::run_scheduler`] is called. Its `x(0)` is recorded and readable through
    /// [`Runtime::process_result`] once it has run to its `return` (a normally-returning
    /// process's *exit reason* is `normal`, not that value — see [`crate::runner`]).
    ///
    /// # Errors
    ///
    /// * the code server's not-found/malformed diagnostic for `module`;
    /// * [`crate::error::undef`] (class `error`, bare reason atom `undef`) when
    ///   `function/arity` is not exported by it — the same raise
    ///   `RuntimeContext::run_export` produces for a call the release does not have
    ///   (the module/function/arity live in the stacktrace, not the reason);
    /// * the process table's allocation failure.
    pub fn spawn_process(
        &mut self,
        module: &str,
        function: &str,
        args: Vec<Eterm>,
    ) -> Result<ProcessId> {
        let policy = self.default_spawn_policy();
        self.spawn_process_with_policy(module, function, args, policy, None)
    }

    /// The heap policy a process created **without** `spawn_opt` heap options gets: the
    /// emulator's own policy, with `+hms`/`+hmbs`/`+hmax` already applied
    /// ([`RuntimeConfig::heap_options`]).
    ///
    /// This is the C's single global default — the `n` / `BIN_Vn` / `H_MAX_SIZE` a process
    /// is created with when `spawn_opt` supplied no heap option
    /// (`erl_process.c:12242-12248`, `12567-12572`) — so both the runtime's root process
    /// and every plain `spawn*` child report the `min_heap_size` /
    /// `min_bin_vheap_size` / `fullsweep_after` / `max_heap_size` the oracle does.
    pub(crate) fn default_spawn_policy(&self) -> HeapPolicy {
        self.config.heap.policy().clone()
    }

    /// [`Runtime::spawn_process`] under an explicit heap policy and priority — the
    /// `spawn_opt` path.
    ///
    /// Both extras are honoured by **creating** the child with them: `policy` is the
    /// [`ProcessTable::spawn`] policy (the child's own `beam_gc` policy, which
    /// `process_info(Pid, …)` then reports for *that* process) and `priority` is set before
    /// the child is queued, so the scheduler can never observe a half-configured process.
    ///
    /// # Errors
    ///
    /// As [`Runtime::spawn_process`], plus the process table's diagnostic when a just-spawned
    /// pid cannot take `priority`.
    pub(crate) fn spawn_process_with_policy(
        &mut self,
        module: &str,
        function: &str,
        args: Vec<Eterm>,
        policy: HeapPolicy,
        priority: Option<Priority>,
    ) -> Result<ProcessId> {
        let entry = self.resolve_export_entry(module, function, args.len() as u32)?;
        let pid = self.processes.spawn(policy)?;
        if let Some(priority) = priority {
            self.processes.set_priority(pid, priority)?;
        }
        // The child inherits the caller's group leader (`erl_process.c:12654-12663`). This
        // path only runs over the runtime's own table — i.e. as the runtime's root process —
        // so the parent is `self_pid` (see `RuntimeContext::owns_process_table`).
        self.inherit_group_leader(self.self_pid, pid);
        self.runner_state.spawn(pid, entry, args);
        self.enqueue_process(pid)?;
        Ok(pid)
    }

    /// Resolves `module:function/arity` to the [`ProcessEntry`] a spawned child starts at.
    ///
    /// The module is resolved through the code path exactly as an external call is (`-pa`/
    /// `-pz` directories first, then the system library `ebin`s), so it is loaded on demand
    /// and cached in the one shared atom table. This is the module resolution
    /// [`Runtime::spawn_process_with_policy`] performs — the code-path search, the load and
    /// the export check — factored out so a spawn made from **inside a scheduler slice**
    /// resolves its entry point exactly as a direct one does (see
    /// [`RuntimeContext::spawn_in_pass`](crate::context::RuntimeContext); the caller owns the
    /// child's pid allocation).
    ///
    /// # Errors
    ///
    /// * the code server's not-found/malformed diagnostic for `module`;
    /// * [`crate::error::undef`] (class `error`, bare reason atom `undef`) when
    ///   `function/arity` is not exported by it.
    pub(crate) fn resolve_export_entry(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
    ) -> Result<ProcessEntry> {
        self.boot_if_needed()?;
        // A module the code server already holds (loaded explicitly by
        // [`Runtime::load_module`], say) needs no code-path search — which also avoids
        // resolving the system library directories. Otherwise a miss here is not fatal:
        // `CodeServer::load` reports it, naming every directory searched.
        if self.code_server.module(module).is_none() {
            let _ = self.find_module_file(module);
        }
        let loaded = self.code_server.load(module)?;
        let function_atom = loaded
            .atom_table
            .lookup(function)
            .ok_or_else(error::undef)?;
        if loaded.export(function_atom, arity).is_none() {
            return Err(error::undef());
        }
        Ok(ProcessEntry::Export {
            module: loaded,
            function: function_atom,
            arity,
        })
    }

    /// Records a process created by an `erlang:spawn*` **from inside a scheduler slice** for
    /// the pass to register and enqueue (see [`Runtime::pending_spawns`]).
    ///
    /// # Errors
    ///
    /// The stabilization failure of the entry fun or an argument:
    /// [`BeamError::SystemLimit`] when the term genuinely cannot be copied onto the arena (see
    /// [`Runtime::try_stabilize_escaped`]). Nothing is recorded in that case, so a pass never
    /// runs a child whose entry or arguments dangle — the caller reports the honest error
    /// instead.
    pub(crate) fn note_pending_spawn(&mut self, mut spawn: PendingSpawn) -> Result<()> {
        // The entry point and the arguments are read **later**, by the pass that registers and
        // enqueues the child ([`Runtime::absorb_pending_spawns`]) — and the spawning process
        // may be collected in between, which moves or drops the heap these terms were built
        // on. Copy them onto the pointer-stable arena first (see
        // [`Runtime::try_stabilize_escaped`], which reports a term the copier genuinely cannot
        // copy as the honest `system_limit`); this is also what makes the copies
        // [`RunnerState::spawn`] then stores safe.
        spawn.entry = match spawn.entry {
            ProcessEntry::Fun { fun, .. } => ProcessEntry::Fun {
                fun: self.try_stabilize_escaped(fun)?,
                // A pending spawn has not run yet, so its local fun cannot have been
                // resolved: the first run of the child does that (see
                // [`ProcessEntry::Fun`]).
                target: None,
            },
            export => export,
        };
        spawn.args = spawn
            .args
            .into_iter()
            .map(|arg| self.try_stabilize_escaped(arg))
            .collect::<Result<Vec<Eterm>>>()?;
        self.pending_spawns.push(spawn);
        Ok(())
    }

    /// Spawns a process whose entry point is the **fun** `fun`, applied to `args`.
    ///
    /// This is the process half of `erlang:spawn/1` (and the fun head of
    /// `spawn_link/1`/`spawn_opt/2`): the child is created in the runtime's own process
    /// table, its engine entry registered as a
    /// [`ProcessEntry::Fun`](crate::runner::ProcessEntry::Fun), and it is appended to the
    /// scheduler's run queue — so it runs on the next scheduler pass, including a pass
    /// driven from inside `-eval` ([`Runtime::pump_scheduler`]).
    ///
    /// `fun` must be a fun term built on a heap that outlives the process (the runtime's
    /// arena or a pooled BIF heap — both are pointer-stable and never reset in Milestone 1).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] when `fun` is not a fun term (the caller — the native
    /// `erlang:spawn/*` floor — checks this first and reports the oracle's `badarg`, so a
    /// non-fun here is a programming error and never a wrong process), plus the process
    /// table's allocation failure.
    pub fn spawn_fun_process(&mut self, fun: Eterm, args: Vec<Eterm>) -> Result<ProcessId> {
        let policy = self.default_spawn_policy();
        self.spawn_fun_process_with_policy(fun, args, policy, None)
    }

    /// [`Runtime::spawn_fun_process`] under an explicit heap policy and priority — the
    /// `spawn_opt/2` path (see [`Runtime::spawn_process_with_policy`]).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] when `fun` is not a fun term, plus the process table's
    /// allocation/priority diagnostics.
    pub(crate) fn spawn_fun_process_with_policy(
        &mut self,
        fun: Eterm,
        args: Vec<Eterm>,
        policy: HeapPolicy,
        priority: Option<Priority>,
    ) -> Result<ProcessId> {
        if !terms::is_fun(fun) {
            return Err(BeamError::BadArg);
        }
        self.boot_if_needed()?;
        let pid = self.processes.spawn(policy)?;
        if let Some(priority) = priority {
            self.processes.set_priority(pid, priority)?;
        }
        // As `spawn_process_with_policy`: the child inherits the root process's group leader.
        self.inherit_group_leader(self.self_pid, pid);
        self.runner_state.spawn_fun(pid, fun, args);
        self.enqueue_process(pid)?;
        Ok(pid)
    }

    /// Appends a process created by [`Runtime::spawn_process`] to the scheduler's run
    /// queue and marks it `Runnable`.
    ///
    /// [`Runtime::spawn_process`] already does this, so a caller needs it only to put a
    /// process it took off the queue **back** on it — for instance a receiver woken by a
    /// message delivered from outside a scheduler pass. Enqueuing a pid that is already
    /// queued is a no-op, so a caller cannot accidentally run one process twice per round.
    ///
    /// # Errors
    ///
    /// A typed diagnostic when `pid` is not a process this runtime spawned (there is no
    /// entry point to run) — never a silent no-op that would hide the mistake.
    pub fn enqueue_process(&mut self, pid: ProcessId) -> Result<()> {
        if !self.runner_state.contains(pid) {
            return Err(error::unknown_process_entry(pid));
        }
        if let Some(process) = self.processes.get_mut(pid) {
            if process.flags().in_run_queue() {
                return Ok(());
            }
            process.flags_mut().set_in_run_queue(true);
            process.set_state(ProcessState::Runnable);
        }
        self.scheduler.enqueue(pid);
        Ok(())
    }

    /// The `x(0)` a process spawned with [`Runtime::spawn_process`] returned, once it has
    /// run to its `return`.
    ///
    /// A process that is still running, blocked, or finished by raising has no recorded
    /// result (`None`); see [`Runtime::process_exit`] for the failing case.
    pub fn process_result(&self, pid: ProcessId) -> Option<Eterm> {
        self.runner_state.result(pid)
    }

    /// The exit reason a process spawned with [`Runtime::spawn_process`] finished with,
    /// once it **raised**.
    ///
    /// A normally-returning process records no exit reason here (its termination is
    /// `normal` and its value is [`Runtime::process_result`]).
    pub fn process_exit(&self, pid: ProcessId) -> Option<Eterm> {
        self.runner_state.exit(pid)
    }

    /// The number of processes waiting in the scheduler's run queue.
    pub fn run_queue_len(&self) -> usize {
        self.scheduler.run_queue().len()
    }

    /// Credits the `n` reduction units one scheduler slice charged to the node-global
    /// cumulative count behind `erlang:statistics(reductions)`/`(exact_reductions)`.
    ///
    /// Called by the runner ([`crate::runner`]) with exactly what the slice's
    /// `BudgetProbe` charged, so the figure is the runtime's real scheduled work. Saturating,
    /// so a run long enough to overflow `u64` reductions degrades to a clamped count instead
    /// of wrapping into a smaller one.
    pub(crate) fn note_reductions(&mut self, n: u64) {
        self.reductions = self.reductions.saturating_add(n);
    }

    /// Credits the `n` process dispatches one scheduler round performed to the node-global
    /// context-switch count behind `erlang:statistics(context_switches)`.
    ///
    /// One dispatch is one process the run loop switched to (see
    /// [`crate::runtime::scheduler`]); a passive port wait is not a process switch and is not
    /// counted.
    pub(crate) fn note_context_switches(&mut self, n: u64) {
        self.context_switches = self.context_switches.saturating_add(n);
    }

    /// `erlang:statistics(reductions)`/`(exact_reductions)`: `(Total, SinceLast)`.
    ///
    /// `exact` selects which key's anchor is used, so each key's `SinceLast` is the delta
    /// since the previous read of **that** key — a pair of independent cursors over the one
    /// counter, exactly as the two oracle keys name the same quantity. `Total` is the
    /// runtime's own cumulative reduction count since it started (its documented "different
    /// zero", like `wall_clock`'s).
    pub(crate) fn reductions_statistics(&self, exact: bool) -> (u64, u64) {
        let anchor = if exact {
            &self.exact_reductions_anchor
        } else {
            &self.reductions_anchor
        };
        let total = self.reductions;
        let last = anchor.replace(total);
        (total, total.saturating_sub(last))
    }

    /// `erlang:statistics(context_switches)`: `(Total, SinceLast)`, from the same cumulative
    /// dispatch counter and the same per-key anchor rule as
    /// [`Runtime::reductions_statistics`].
    pub(crate) fn context_switch_statistics(&self) -> (u64, u64) {
        let total = self.context_switches;
        let last = self.context_switches_anchor.replace(total);
        (total, total.saturating_sub(last))
    }

    /// `erlang:statistics(io)`: `(BytesIn, BytesOut)` from the port subsystem's own counters
    /// (see [`IoCounters`]) — the bytes this runtime really read from and wrote to its ports.
    pub(crate) fn io_statistics(&self) -> (u64, u64) {
        self.io_counters.bytes()
    }

    /// `erlang:statistics(runtime)`: `(Total, SinceLastCall)` in **milliseconds of user CPU
    /// time**, read from the runtime's one long-lived
    /// [`RuntimeClock`](beam_io::cpu_time::RuntimeClock).
    ///
    /// `Total` is the whole process's accumulated user CPU time (`getrusage(RUSAGE_SELF)`,
    /// exactly the C's measurement) and `SinceLastCall` is the delta since the previous read,
    /// with the anchor starting at 0 — so the **first** read answers two equal elements,
    /// matching `erts_runtime_elapsed_both` (`erts/emulator/beam/erl_time_sup.c:1421-1481`).
    /// The clock is read through a [`RefCell`] so the `statistics/1` arm can call it with
    /// `&self`, and it is the runtime's **one** clock, so the anchor persists across calls.
    ///
    /// # Errors
    ///
    /// Propagates the clock's failure: a failing `getrusage(2)` is
    /// [`BeamError::Io`] carrying the errno, never a silently fabricated `0` (the C aborts the
    /// emulator at that point; this runtime returns the error instead).
    pub(crate) fn cpu_time_statistics(&self) -> Result<(u64, u64)> {
        self.cpu_clock.borrow_mut().runtime()
    }
    /// The runtime's distributed node, when `-name`/`-sname` configured one.
    ///
    /// The observable surface of contract A.5's Phase 1: the node's name, its listen
    /// port, whether `epmd` accepted the registration, and every live link with its
    /// handshake state (`DistConnection::is_established`) and peer name. `None` means
    /// this runtime is not a node (`nonode@nohost`).
    pub fn dist_node(&self) -> Option<&DistNode> {
        self.dist.as_ref()
    }

    /// The node's full name (`node@host`), when this runtime is a distributed node.
    pub fn node_name(&self) -> Option<&str> {
        self.dist.as_ref().map(|node| node.node_name())
    }

    /// The distribution **cookie** this runtime's handshake digests use.
    ///
    /// Resolved once at construction from `-setcookie`, else `$HOME/.erlang.cookie`, else the
    /// documented default (see [`crate::dist::resolve_cookie`]); independent of whether a node
    /// was configured. [`Runtime::set_cookie_bytes`] replaces it.
    pub fn cookie(&self) -> &[u8] {
        &self.cookie
    }

    /// Replaces the distribution cookie (the runtime side of `erlang:set_cookie/2`).
    pub fn set_cookie_bytes(&mut self, cookie: Vec<u8>) {
        self.cookie = cookie;
    }

    /// The atom naming this runtime's local node: the configured `node@host` when
    /// `-name`/`-sname` made it a node, else the fixed `nonode@nohost` identity.
    pub fn local_node(&self) -> Atom {
        self.local_node
    }

    /// [`Runtime::local_node`] as the atom **term** the engine compares against.
    pub fn local_node_term(&self) -> Eterm {
        make_atom(self.local_node)
    }

    /// Whether this runtime is a **live** distributed node: bound and registered with `epmd`,
    /// so a remote node can reach it.
    ///
    /// `false` for a runtime with no `-name`/`-sname`, and for a node `epmd` refused or has not
    /// yet registered (a registration failure is recorded, never raised — see
    /// [`crate::dist`]).
    pub fn is_alive(&self) -> bool {
        self.dist.as_ref().is_some_and(DistNode::is_alive)
    }

    /// Turns routing of **inbound** distribution control messages into local mailboxes
    /// on or off for the scheduler pass ([`Runtime::dispatch_dist_inbound`]).
    ///
    /// Off by default (see the `inbound_delivery` field): a node handshakes and pumps
    /// its links either way, but only with this enabled does the pass route what a
    /// peer decoded — a `DOP_SEND*`/`DOP_REG_SEND*` — into the addressed local
    /// process's mailbox. Enabling it is what makes a peer's message observable to the
    /// running code; leaving it off keeps a runtime's behaviour byte-identical to one
    /// with no inbound traffic.
    ///
    /// This is a runtime capability, not an emulator switch: no `-name`/`-sname` →
    /// no node → nothing to route.
    pub fn allow_inbound_delivery(&mut self, allow: bool) {
        self.inbound_delivery = allow;
    }

    /// Whether the scheduler pass routes inbound distribution deliveries (see
    /// [`Runtime::allow_inbound_delivery`]).
    pub fn inbound_delivery_enabled(&self) -> bool {
        self.inbound_delivery
    }
    /// Materializes parsed command-line arguments into terms on the shared atom table
    /// and heap (an atom's index can only be assigned here).
    ///
    /// # Errors
    /// Propagates heap allocation failures while building lists.
    pub fn materialize_args(&mut self, args: &[ArgTerm]) -> Result<Vec<Eterm>> {
        let mut out = Vec::with_capacity(args.len());
        for arg in args {
            let term = materialize(arg, &mut self.code_server, &mut self.arena)?;
            out.push(term);
        }
        Ok(out)
    }

    /// `system_info/1` (contract A.8) materialized as a term.
    ///
    /// The term is built on the runtime's **scratch heap**, which is reset at the start
    /// of every `system_info`/`format_term` call: a returned term is valid **until the
    /// next such call**.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for an unknown key, plus
    /// the materialization errors of [`InfoValue::to_eterm`].
    pub fn system_info(&mut self, key: &str) -> Result<Eterm> {
        let value = self.system_info_value(key)?;
        self.info_heap.reset();
        let Runtime {
            info_heap,
            code_server,
            ..
        } = self;
        value.to_eterm(info_heap, code_server.atoms())
    }

    /// `system_info/1` as a heap-independent value — the testable source of truth.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for an unknown key.
    pub fn system_info_value(&self, key: &str) -> Result<InfoValue> {
        info::system_info_value(key, self)
    }

    /// Evaluates a general Erlang expression (contract A.6).
    ///
    /// The expression is parsed and evaluated by the `erl_eval`-style tree-walker in
    /// [`crate::eval`], dispatched through the same [`RuntimeContext`] the vertical slice
    /// uses — so arithmetic, comparison and every call reach the `beam_bifs` registry
    /// (or the load-on-demand stdlib) with their exact `erl` semantics. The result is
    /// built on the process heap, so it stays valid for the runtime's lifetime.
    ///
    /// # Errors
    ///
    /// A diagnostic for an expression outside the grammar ([`crate::error::bad_eval`],
    /// a capability gap), whatever a called BIF/module raised, a raise of the evaluated
    /// expression itself (including the uncatchable `erlang:halt/0,1`), or a documented
    /// Milestone-1 gap.
    pub fn eval(&mut self, expr: &str) -> Result<Eterm> {
        let mut ctx = RuntimeContext::new(&mut *self);
        crate::eval::eval(&mut ctx, expr)
    }

    /// Prints `term` through the registered `io:format/2` `~p` printer.
    ///
    /// `module` selects the atom table the printer resolves atoms against: the loaded
    /// module's (the one that produced the term) when given, the runtime's shared table
    /// otherwise. Like [`Runtime::system_info`], the format/argument lists are built on
    /// the scratch heap, so the returned text — not any term — is the result.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a term outside the
    /// Milestone-1 printable set; the heap's allocation failure otherwise.
    pub fn format_term(&mut self, term: Eterm, module: Option<&Module>) -> Result<String> {
        self.format_with(term, module, format::FORMAT)
    }

    /// Like [`Runtime::format_term`] but with the `~w` control (no string rule).
    ///
    /// # Errors
    ///
    /// As [`Runtime::format_term`].
    pub fn format_term_w(&mut self, term: Eterm, module: Option<&Module>) -> Result<String> {
        self.format_with(term, module, format::FORMAT_W)
    }

    /// The one printer: renders `term` with `control` on the scratch heap.
    fn format_with(
        &mut self,
        term: Eterm,
        module: Option<&Module>,
        control: &[u8],
    ) -> Result<String> {
        self.info_heap.reset();
        let Runtime {
            info_heap,
            code_server,
            registry,
            self_pid,
            ..
        } = self;
        let table = match module {
            Some(module) => &module.atom_table,
            None => code_server.atoms(),
        };
        format::term_to_string_with(registry, info_heap, table, *self_pid, term, control)
    }

    /// Boots on demand (idempotent).
    fn boot_if_needed(&mut self) -> Result<()> {
        if self.booted {
            Ok(())
        } else {
            self.boot()
        }
    }
}

/// Materializes one parsed argument term (the [`Runtime::materialize_args`] walk).
fn materialize(arg: &ArgTerm, code_server: &mut CodeServer, arena: &mut MinimalContext) -> Result<Eterm> {
    Ok(match arg {
        ArgTerm::Int(value) => make_small(*value),
        ArgTerm::Atom(name) => make_atom(code_server.atoms_mut().intern(name)),
        ArgTerm::Str(text) => charlist(text, arena)?,
        ArgTerm::List(elements) => {
            let mut terms = Vec::with_capacity(elements.len());
            for element in elements {
                terms.push(materialize(element, code_server, arena)?);
            }
            terms::make_list(arena, &terms)?
        }
    })
}

/// `text` as an Erlang string (a char list) built on the arena.
fn charlist(text: &str, arena: &mut MinimalContext) -> Result<Eterm> {
    let elems: Vec<Eterm> = text
        .chars()
        .map(|c| make_small(i64::from(c as u32)))
        .collect();
    terms::make_list(arena, &elems)
}

#[cfg(test)]
mod tests {
    use beam_term::{make_atom, make_small, signed_value};

    use super::*;
    use crate::config::Action;

    /// The committed oracle fixture (reused by relative path — never copied).
    pub(crate) const SLICE_BEAM: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../beam_loader/tests/fixtures/slice.beam"
    );

    fn runtime() -> Runtime {
        Runtime::with_defaults().expect("the default runtime")
    }

    #[test]
    fn boot_is_idempotent_and_reports_itself() {
        let mut rt = runtime();
        assert!(!rt.is_booted());
        if let Err(error) = rt.boot() {
            // Boot now loads the erts preloaded `.beam`s, so it needs a system install;
            // skip loudly rather than fail on a machine without one.
            eprintln!("SKIP: boot needs the system Erlang install ({error})");
            return;
        }
        assert!(rt.is_booted());
        rt.boot().expect("booting twice is a no-op");
        assert!(rt.is_booted());
        assert_eq!(rt.preloaded().len(), boot::PRELOADED_MODULE_NAMES.len());
        assert!(rt.preloaded().is_preloaded("erlang"));
        // The whole preloaded set is really in the shared cache now, not just its names.
        assert_eq!(
            rt.preloaded_loaded(),
            boot::PRELOADED_MODULE_NAMES
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        );
        for name in ["erlang", "init", "erts_internal"] {
            assert!(rt.is_preloaded_loaded(name), "{name} must be loaded");
        }
        assert_eq!(rt.config().action, Action::Boot);
    }

    #[test]
    fn a_fresh_runtime_has_core_atoms_and_its_native_service_processes() {
        let rt = runtime();
        assert!(rt.atoms().lookup("unix").is_some());
        assert!(rt.atoms().lookup("linux").is_some());
        // `Runtime::new` starts exactly **two** processes, both of them the runtime's own —
        // neither was spawned by user code:
        //   1. the **root process**, registered as `init` (`erlang:whereis(init)` answers it —
        //      the oracle answers `<0.0.0>`), which every action runs as; and
        //   2. the native **`file_server_2` service** (contract A.9), a `Runnable`,
        //      never-enqueued process that answers the OTP file protocol the boot script needs
        //      (`src/native/file_server.rs`).
        assert_eq!(rt.processes().len(), 2);
        for name in ["init", "file_server_2"] {
            let atom = rt
                .atoms()
                .lookup(name)
                .unwrap_or_else(|| panic!("`{name}` is a core atom"));
            assert!(
                rt.processes().whereis(atom).is_some(),
                "the fresh runtime must have a live `{name}` process"
            );
        }
        assert_eq!(
            rt.system_info_value("process_count").expect("count"),
            InfoValue::Int(2)
        );
    }

    #[test]
    fn an_oversized_return_value_becomes_the_documented_system_limit() {
        // A runtime whose term-construction heaps are at the floor, so a value larger than the
        // heap is cheap to build. `stabilize_term` copies a process's return value onto the
        // never-reset `eval_heap`; a value too large for it is a copy-engine exhaustion
        // (`AllocKind::Message`), which the runtime must report as the emulator's documented
        // `error:system_limit` — never the raw internal `allocation failure: message (N bytes)`
        // (see `error::enforce_term_heap_limit`).
        let mut rt = Runtime::new(RuntimeConfig::new().with_heap_words(MIN_HEAP_WORDS))
            .expect("the small-heap runtime");
        // Four words of list per element budget: far more than the eval heap holds.
        let elems: Vec<Eterm> = (0..(MIN_HEAP_WORDS as i64 * 4)).map(make_small).collect();
        let big = terms::make_list(&mut rt.arena, &elems).expect("the engine arena is unbounded");
        assert!(
            beam_term::size_object(big) > MIN_HEAP_WORDS,
            "the value must exceed the eval heap to exercise the translation"
        );
        assert!(
            matches!(rt.stabilize_term(big), Err(BeamError::SystemLimit)),
            "an oversized return value is the documented system_limit, not a raw Alloc"
        );
    }

    /// The off-heap payload owner a `beam_codec::term::make_refc_binary` call needs: exactly the
    /// role `beam_engine`'s `Machine::refc_binaries` plays (and the shape of the handles the
    /// runner passes to [`Runtime::stabilize_term_with`]).
    struct RefcPayloads(Vec<beam_alloc::RefcBinary>);

    impl beam_codec::OffHeapBinaries for RefcPayloads {
        fn alloc_refc_payload(&mut self, bytes: &[u8]) -> Result<*mut Eterm> {
            let handle = beam_alloc::RefcBinary::new(bytes)?;
            let data = handle.data_ptr();
            self.0.push(handle);
            Ok(data.cast::<Eterm>())
        }
    }

    /// Builds the frozen 8-word `[BinRef][ErlSubBits]` pair over a fresh off-heap payload of 200
    /// bytes — the genuine representation `bs_init_writable`/`bs_create_bin` produce above
    /// [`beam_term::refc::ERL_ONHEAP_BINARY_LIMIT`], and the one the transport copy must resolve.
    ///
    /// Returns the source heap (which owns the pair — it must outlive the term), the payload
    /// handles and the term.
    fn off_heap_binary() -> (Heap, RefcPayloads, Vec<u8>, Eterm) {
        let bytes: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
        assert!(bytes.len() > beam_term::refc::ERL_ONHEAP_BINARY_LIMIT);
        let mut heap = Heap::new(16).expect("the source heap");
        let mut payloads = RefcPayloads(Vec::new());
        let term =
            beam_codec::term::make_refc_binary(&mut heap, &mut payloads, &bytes, 0, bytes.len() * 8)
                .expect("the source refc-binary term");
        assert_eq!(payloads.0.len(), 1, "one off-heap payload");
        (heap, payloads, bytes, term)
    }

    /// An engine-built **off-heap** binary (the 8-word `[BinRef][ErlSubBits]` pair whose payload
    /// a `beam_alloc::RefcBinary` handle owns) must survive the transport copy onto the runtime's
    /// long-lived arena.
    ///
    /// The copy engine resolves the payload by its data pointer through the copy's **source**
    /// off-heap table (`beam_message::copy::copy_refc_binary`), so the *empty* table the simple
    /// form uses refuses the term with `BadArg` — the wrong answer this test pins the fix
    /// against. [`Runtime::stabilize_term_with`] builds that table from the caller's handles (the
    /// runner passes `Machine::refc_binaries()`), and its **destination** table
    /// ([`Runtime::eval_off_heap`]) owns the payload references the copied term depends on — the
    /// refcount assertions below are that ownership invariant.
    #[test]
    fn an_engine_built_off_heap_binary_survives_the_transport_copy() {
        let (_source_heap, payloads, bytes, term) = off_heap_binary();
        assert_eq!(payloads.0[0].refcount(), 1, "only the source machine owns it");

        let mut rt = Runtime::new(RuntimeConfig::new().with_heap_words(MIN_HEAP_WORDS))
            .expect("the small-heap runtime");

        // The documented degradation: with no reachable engine handles, the empty-source copy
        // cannot resolve the payload and refuses the term.
        assert!(
            matches!(rt.stabilize_term(term), Err(BeamError::BadArg)),
            "the empty-source copy refuses an engine-built off-heap binary"
        );

        // The machine-aware form copies it, resolving the payload through the handles.
        let copied = rt
            .stabilize_term_with(term, &payloads.0)
            .expect("an engine-built off-heap binary stabilizes through its handles");

        // Byte-identical: `compare::eq` compares bitstrings bit by bit (contents, not pointers),
        // against an independently built on-heap reference holding the same bytes.
        let mut reference_heap = Heap::new(64).expect("the reference heap");
        let reference =
            beam_codec::term::make_binary(&mut reference_heap, &bytes).expect("the reference");
        assert!(
            beam_term::compare::eq(copied, reference, rt.atoms()),
            "the stabilized copy reads back byte-identical"
        );

        // The destination table owns a second reference to the payload, so the copied term can
        // never dangle once the source engine is dropped.
        assert_eq!(
            payloads.0[0].refcount(),
            2,
            "the destination table holds its own reference"
        );
        assert_eq!(rt.eval_off_heap.binaries().len(), 1);
        drop(payloads);
        let held = rt.eval_off_heap.binaries();
        assert_eq!(held[0].refcount(), 1, "the runtime keeps the payload alive");
        assert_eq!(
            held[0].as_bytes(),
            bytes.as_slice(),
            "the destination table owns the copied payload"
        );
    }

    /// The escape-store form of the same transport copy: a term carrying an engine-built off-heap
    /// binary must stabilize **through the machine's handles** rather than trip the global
    /// "capability gap" valve over a payload the empty source table could not resolve.
    ///
    /// The empty-source call is the documented degradation — it keeps the original term and
    /// freezes every process heap ([`Runtime::try_stabilize_escaped`]) — so it is asserted
    /// **after** the resolving form, and it pins that the unresolvable case still behaves exactly
    /// as before.
    #[test]
    fn the_escape_store_stabilization_resolves_engine_off_heap_handles() {
        let (_source_heap, payloads, bytes, term) = off_heap_binary();
        let mut rt = Runtime::new(RuntimeConfig::new().with_heap_words(MIN_HEAP_WORDS))
            .expect("the small-heap runtime");
        assert!(rt.gc_enabled(), "a fresh runtime collects");

        let copied = rt
            .try_stabilize_escaped_with(term, &payloads.0)
            .expect("the exit-reason copy resolves the engine's handles");
        let mut reference_heap = Heap::new(64).expect("the reference heap");
        let reference =
            beam_codec::term::make_binary(&mut reference_heap, &bytes).expect("the reference");
        assert!(
            beam_term::compare::eq(copied, reference, rt.atoms()),
            "the escaped copy reads back byte-identical"
        );
        assert!(
            rt.gc_enabled(),
            "a resolved off-heap payload must NOT trip the escape-store valve"
        );

        // The unresolvable form: the original term is kept (never a dangling copy) and every
        // process heap is frozen.
        let kept = rt
            .try_stabilize_escaped(term)
            .expect("the documented degradation keeps the original term");
        assert_eq!(kept, term, "the original term is returned unchanged");
        assert!(
            !rt.gc_enabled(),
            "the empty-source capability gap freezes collection"
        );
    }
    /// `erlang:statistics(runtime)` is read from the runtime's **one** long-lived CPU-time
    /// clock, so its anchor persists: the first read's delta is the whole total (the clock's
    /// anchor starts at 0, the C's `runtime_prev.data.user = 0`), and every later read's delta
    /// is the increase since the previous read of the same clock. A clock rebuilt per call
    /// would answer `total == since` every time.
    #[test]
    fn the_cpu_time_clock_anchors_across_statistics_reads() {
        let rt = runtime();
        let (total1, since1) = rt.cpu_time_statistics().expect("the first CPU-time read");
        assert_eq!(
            since1, total1,
            "the first read's delta is the whole total (the clock anchor starts at 0)"
        );

        // Burn a little user CPU so the clock has something real to advance by, then read
        // again from the **same** runtime (the same long-lived clock). `black_box` keeps the
        // loop from being optimised out.
        let mut spin: u64 = 0;
        for i in 0..2_000_000u64 {
            spin = std::hint::black_box(spin.wrapping_add(i ^ (spin >> 1)));
        }

        let (total2, since2) = rt.cpu_time_statistics().expect("the second CPU-time read");
        assert!(
            total2 >= total1,
            "the process's user CPU time never goes backwards: {total2} >= {total1} (spin {spin})"
        );
        assert_eq!(
            since2,
            total2 - total1,
            "a later read's delta is the increase since the previous read"
        );
    }

    #[test]
    fn a_runtime_without_a_node_is_not_alive_and_names_nonode() {
        let rt = runtime();
        // No `-name`/`-sname`: no node, so not alive.
        assert!(!rt.is_alive());
        assert!(rt.dist_node().is_none());
        // The local node is the fixed `nonode@nohost` identity (seeded at boot), and the
        // process table was told the same atom (named monitors spell their target with it).
        let nonode = rt
            .atoms()
            .lookup("nonode@nohost")
            .expect("seeded at boot");
        assert_eq!(rt.local_node(), nonode);
        assert_eq!(rt.local_node_term(), make_atom(nonode));
        assert_eq!(rt.processes().local_node(), Some(nonode));
    }

    #[test]
    fn the_cookie_comes_from_the_init_arguments() {
        // `-setcookie secret` is the one path that resolves deterministically (a cookie file
        // would change the answer), so assert exactly that.
        let mut config = RuntimeConfig::default();
        config
            .init_args
            .record("setcookie", vec!["secret".to_string()]);
        let mut rt = Runtime::new(config).expect("the runtime");
        assert_eq!(rt.cookie(), b"secret");
        // And it is mutable without a live node.
        rt.set_cookie_bytes(b"other".to_vec());
        assert_eq!(rt.cookie(), b"other");
    }

    #[test]
    fn loads_and_caches_a_module_in_the_shared_table() {
        let mut rt = runtime();
        let module = rt.load_module(SLICE_BEAM).expect("load slice.beam");
        assert_eq!(module.atom_name(module.name), Some("slice"));
        assert_eq!(rt.loaded_modules(), vec!["slice".to_string()]);
        assert!(rt.module("slice").is_some());
        assert!(rt.module("nope").is_none());
        // The shared table knows the module's atoms.
        assert!(rt.atoms().lookup("pick").is_some());
        rt.load_module(SLICE_BEAM).expect("reloading replaces");
        assert_eq!(rt.loaded_modules().len(), 1);
    }

    #[test]
    fn a_missing_file_is_an_error() {
        let mut rt = runtime();
        assert!(rt.load_module("/nonexistent/nope.beam").is_err());
        assert!(rt
            .run_function("/nonexistent/nope.beam", "f", 0, &[])
            .is_err());
    }

    #[test]
    fn a_missing_export_raises_undef() {
        // `run_function`/`run_module` delegate the export lookup to
        // `RuntimeContext::run_export`, which now raises the oracle's bare `error:undef`
        // for a function the module does not have (see `crate::context::run_export`).
        let mut rt = runtime();
        let error = rt
            .run_function(SLICE_BEAM, "nope", 0, &[])
            .expect_err("not exported");
        assert_eq!(error.as_atom(), Some("undef"), "{error}");
        let error = rt
            .run_function(SLICE_BEAM, "add", 1, &[make_small(1)])
            .expect_err("wrong arity");
        assert_eq!(error.as_atom(), Some("undef"), "{error}");
    }

    #[test]
    fn a_spawn_of_a_missing_export_raises_undef() {
        // `spawn_process` resolves the entry point exactly as `run_export` does, so a
        // function the module does not export (or a wrong arity) is the same **bare**
        // `error:undef` — never a message-bearing diagnostic and never a capability gap.
        let mut rt = runtime();
        rt.load_module(SLICE_BEAM).expect("load slice.beam");

        let error = rt
            .spawn_process("slice", "nope", vec![])
            .expect_err("slice:nope/0 is not exported");
        assert!(error.is_raise(), "{error}");
        assert_eq!(error.as_atom(), Some("undef"), "{error}");
        assert_eq!(error.to_string(), "error undef", "{error}");
        // A genuine raise, not a Milestone-1 capability gap (so the CLI exits 1).
        assert_eq!(error::unsupported_reason(&error), None, "{error}");

        let error = rt
            .spawn_process("slice", "add", vec![make_small(1)])
            .expect_err("slice:add/1 is not exported");
        assert_eq!(error.as_atom(), Some("undef"), "{error}");

        assert_eq!(rt.run_queue_len(), 0, "no failed spawn enqueued anything");
    }

    #[test]
    fn the_slice_runs_through_the_real_context() {
        let mut rt = runtime();
        let sum = rt
            .run_function(SLICE_BEAM, "add", 2, &[make_small(2), make_small(3)])
            .expect("add/2");
        assert_eq!(signed_value(sum), 5);
        let fib = rt
            .run_function(SLICE_BEAM, "fib", 1, &[make_small(10)])
            .expect("fib/1");
        assert_eq!(signed_value(fib), 55);
    }

    #[test]
    fn the_slice_takes_an_atom_argument_from_the_shared_table() {
        let image = beam_loader::load_file(SLICE_BEAM).expect("load slice.beam");
        let a = make_atom(image.atom_table.lookup("a").expect("a is interned"));
        let mut rt = runtime();
        let mut sink = beam_bifs::NullSink;
        let picked = rt
            .run_module(&image, "pick", 1, &[a], &mut sink)
            .expect("pick(a)");
        assert_eq!(signed_value(picked), 1);
    }

    #[test]
    fn system_info_round_trips_through_the_scratch_heap() {
        let mut rt = runtime();
        let wordsize = rt.system_info("wordsize").expect("wordsize");
        assert_eq!(signed_value(wordsize), std::mem::size_of::<usize>() as i64);
        assert!(matches!(
            rt.system_info("nonsense"),
            Err(beam_common::BeamError::BadArg)
        ));
        let version = rt.system_info("version").expect("version");
        assert!(beam_term::is_list(version));
        let again = rt.system_info("wordsize").expect("wordsize again");
        assert_eq!(signed_value(again), std::mem::size_of::<usize>() as i64);
        let five = rt.eval("2+3").expect("2+3");
        assert_eq!(rt.format_term(five, None).expect("printable"), "5\n");
    }

    #[test]
    fn eval_dispatches_through_the_bif_registry() {
        let mut rt = runtime();
        assert_eq!(signed_value(rt.eval("2+3").expect("2+3")), 5);
        assert_eq!(signed_value(rt.eval("2*3+4").expect("2*3+4")), 10);
        assert_eq!(signed_value(rt.eval("(1+2)*3").expect("(1+2)*3")), 9);
        // A bare atom is a value; only something outside the grammar is an error.
        let atom = rt.eval("nope").expect("an atom evaluates to itself");
        assert_eq!(rt.atom_name_of(atom), Some("nope"));
        assert!(rt.eval("1 +").is_err());
    }

    #[test]
    fn format_term_uses_the_registered_printer() {
        let mut rt = runtime();
        assert_eq!(rt.format_term(make_small(7), None).expect("printable"), "7\n");
        assert_eq!(
            rt.format_term(beam_term::NIL, None).expect("printable"),
            "[]\n"
        );
        // `~w` does not apply the string rule.
        assert_eq!(
            rt.format_term_w(beam_term::NIL, None).expect("printable"),
            "[]\n"
        );
    }

    #[test]
    fn materializes_parsed_argument_terms() {
        let mut rt = runtime();
        let args = vec![
            ArgTerm::Int(-7),
            ArgTerm::Atom("a".to_string()),
            ArgTerm::Str("hi".to_string()),
            ArgTerm::List(vec![ArgTerm::Int(1), ArgTerm::Atom("b".to_string())]),
        ];
        let terms = rt.materialize_args(&args).expect("materialize");
        assert_eq!(signed_value(terms[0]), -7);
        assert_eq!(rt.atom_name_of(terms[1]), Some("a"));
        let chars = terms::list_elements(terms[2]).expect("a char list");
        assert_eq!(signed_value(chars[0]), i64::from(b'h'));
        assert_eq!(terms::list_elements(terms[3]).expect("a list").len(), 2);
    }

    #[test]
    fn module_files_are_found_through_the_code_path() {
        let dir = Path::new(SLICE_BEAM).parent().expect("a parent directory");
        let config = RuntimeConfig::default().with_code_path(dir);
        let mut rt = Runtime::new(config).expect("runtime");
        assert_eq!(rt.find_module_file("slice"), Some(PathBuf::from(SLICE_BEAM)));
    }

}
