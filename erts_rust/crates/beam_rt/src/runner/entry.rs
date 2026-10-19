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

//! The runtime-scheduled process **entry point** and its **engine state**.
//!
//! A process's entry point and the engine that drives it are the runtime's own
//! pid → entry side table: `beam_process`'s PCB has no field for either, because only the
//! runtime knows how to execute code. [`ProcessEntry`] says what to run, [`ProcessEngine`]
//! holds the machine that runs it, and [`RunnerState`] is the whole table plus the
//! outcomes every process recorded.

use std::sync::Arc;

use beam_common::BeamError;
use beam_engine::{Machine, Tier};
use beam_loader::Module;
use beam_process::ProcessId;
use beam_term::{Atom, Eterm};

use crate::context::LocalFunTarget;
use crate::hasher::FastHashMap;
use crate::timers::TimerHandle;

/// The entry point of one runtime-scheduled process: what to run, with which arguments.
///
/// A process's entry point lives here (not on the `beam_process` PCB, which has no such
/// field) because only the runtime knows how to execute code: it is the runtime's own
/// pid → entry side table.
pub(crate) enum ProcessEntry {
    /// A module export: enter `module:function/arity` with the process's args.
    Export {
        /// The module the entry point lives in (shared with the code server's cache).
        module: Arc<Module>,
        /// The exported function's name atom, in that module's atom table.
        function: Atom,
        /// The arity the function is entered with (always `args.len()`).
        arity: u32,
    },
    /// A fun value: apply the fun to the process's args.
    ///
    /// This is what `erlang:spawn/1` (and the fun head of `spawn_link/1`/`spawn_opt/2`)
    /// creates: an `-eval` evaluator closure, or a `.beam` fun term.
    ///
    /// A **`.beam` local** (`make_fun3`) closure is *not* applied through
    /// [`ExecutionContext::apply_fun`](beam_engine::ExecutionContext::apply_fun) — that
    /// entry runs the closure on a fresh, non-blocking machine, so a `receive` inside it
    /// could never suspend the process. It is instead resolved once
    /// ([`crate::context::RuntimeContext::local_fun_target`], remembered in
    /// [`ProcessEntry::Fun::target`]) and entered at its own label on **this** process's
    /// [`ProcessEngine::machine`], exactly as an export entry is, so its `receive` parks
    /// the process and a later message resumes it.
    ///
    /// The two fun kinds with no local label entry — an external `fun M:F/A` and the
    /// `-eval` closure — keep the synchronous `apply_fun` path, which routes an `-eval`
    /// closure back into the evaluator (`crate::eval::apply_eval_closure`) instead of
    /// re-implementing the evaluator here.
    Fun {
        /// The fun term to apply, built on a heap that outlives the process (the
        /// runtime's arena or a pooled BIF heap — both are pointer-stable and never
        /// reset in Milestone 1).
        fun: Eterm,
        /// The resolved launch target of a **local** fun, or `None`.
        ///
        /// Filled in by the first run of such a process (the runner is the only writer),
        /// and re-read by every later slice — the `Wait` resume needs the module the
        /// suspended `(func, pc)` belong to. It stays `None` for both other fun kinds,
        /// which are applied synchronously and never park.
        target: Option<LocalFunTarget>,
    },
}

impl ProcessEntry {
    /// The module the process's **entry** code lives in, once that is known.
    ///
    /// This is the module a suspension's `(func, pc)` are resumed against
    /// ([`Machine::resume_outcome`](beam_engine::Machine::resume_outcome) is handed it as
    /// the run's *entry* module) and the module a parked receive's timeout is recovered
    /// from (see `crate::runner::reason`). `None` for a fun entry that has no local label
    /// to enter — such a process is applied synchronously and never parks.
    pub(super) fn entry_module(&self) -> Option<&Arc<Module>> {
        match self {
            ProcessEntry::Export { module, .. } => Some(module),
            ProcessEntry::Fun { target, .. } => target.as_ref().map(|target| &target.module),
        }
    }
}
/// The engine state of one runtime-scheduled process.
pub(super) struct ProcessEngine {
    /// The process's engine machine. It outlives every single slice: a `receive` that
    /// blocks resumes in **this** machine, so its registers, stack and frames survive
    /// (see `beam_engine`'s `RunOutcome`/`resume_outcome` protocol).
    pub(super) machine: Machine,
    /// Where the process starts and what it is called with.
    pub(super) entry: ProcessEntry,
    /// The first run's arguments (seeded into `x(0..arity)`).
    ///
    /// Held for the engine's whole life: the first entry reads them
    /// ([`RunnerState::spawn`] hands them to the machine, which seeds `x(0..arity)`), and no arm
    /// clears or replaces them afterwards, so a re-run of a finished process is impossible to get
    /// wrong. Their **provenance** decides whether they may be handed over as a reclamation root
    /// — see [`ProcessEngine::arena_args`].
    pub(super) args: Vec<Eterm>,
    /// Whether `args` (and a [`ProcessEntry::Fun`]'s fun) are copies on the **pointer-stable
    /// arena** rather than terms of a heap a collection may move or a process's death may free.
    ///
    /// Only a spawn that stabilized its entry and arguments through
    /// [`Runtime::try_stabilize_escaped`](crate::runtime::Runtime::try_stabilize_escaped) sets
    /// this (the pending-spawn route, [`RunnerState::spawn_stable`]). It is the soundness gate
    /// of [`RunnerState::gather_off_heap_roots`]: that function's caller **dereferences** what it
    /// hands over, and a term that is not a live copy on a stable region must never be rooted —
    /// which is why the un-stabilized spawn routes contribute nothing here (their terms are on
    /// the spawning process's own heap, whose root set is the collection's business, not the
    /// prune's).
    pub(super) arena_args: bool,
    /// The `(func, pc)` a `receive` suspended at, until the process is woken.
    ///
    /// A [`ProcessEntry::Export`] process and a **`ProcessEntry::Fun`** process whose fun
    /// resolved to a local label (see [`ProcessEntry::Fun::target`]) both park here: the
    /// first run is entered on the process's own machine, so a `receive` inside it reports
    /// [`RunOutcome::Wait`](beam_engine::RunOutcome::Wait) and the very same machine is
    /// resumed later. A fun with no local label is applied synchronously (the evaluator
    /// never returns a resume address), so `parked` stays `None` for it.
    pub(super) parked: Option<(u32, u32)>,
    /// The pending **finite receive timeout** of a parked process, if its `wait_timeout`
    /// carried a statically known positive delay.
    ///
    /// While the process is parked the deadline lives in
    /// [`Runtime::timers`](crate::Runtime); this is the handle that cancels it when a real
    /// message wakes the receiver first (so the receive does not also fire its timeout). A
    /// `wait` with no timeout — including the `after infinity` the compiler emits as a plain
    /// `wait` — leaves it `None`, so the process parks indefinitely.
    pub(super) parked_timeout: Option<TimerHandle>,
    /// How many leading slots of the machine's **float-register (FR) file** were published
    /// into the process's Erlang stack at the last park, so the resume scatters exactly that
    /// region back.
    ///
    /// The runner derives the count once per slice from the engine's
    /// [`Machine::fr_mark`](beam_engine::Machine::fr_mark) — the exact live FR prefix (the same
    /// value that pins a process whose FR is live) — and `publish_parked_roots` appends
    /// exactly those leading slots. A process that touched no float has `fr_mark() == 0`, so
    /// no FR word is appended at all; a process whose FR is live publishes **only** its live
    /// prefix instead of all 1024 slots.
    ///
    /// The **resume** must pass the same count, because
    /// [`scatter_roots_full`](crate::context::gc::scatter_roots_full) rejects a stack whose
    /// length does not match what was published (and a `false` there halts the process's
    /// collection). The count is written by the park arms and read by the resume; it is `0` on
    /// a fresh engine (a freshly reset machine's FR is entirely `NIL`, so such a process never
    /// parks before its first slice).
    pub(super) fr_published: usize,
    /// Whether the park this engine is currently sitting in **published** the machine's root
    /// set into the process's `Registers` — i.e. whether the parked window is one a collection
    /// could relocate the heap from.
    ///
    /// A park opens one of two windows ([`park_machine`](super::run)): a **relocation-exempt**
    /// one — every `receive` park, and a `Yield` park whose slice ended with a live FR prefix —
    /// where every relocation site refuses the move for the whole window, so the parked machine
    /// itself stays the authoritative root set and the copy into the process would be an
    /// identity round-trip that nothing ever reads; or a **plain** one (a `Yield` park with a
    /// dead FR prefix), which a delivery-driven collection *can* relocate over, so the machine's
    /// roots must be handed over.
    ///
    /// The flag is written by the park arm and read by the resume, which re-syncs the machine
    /// from the process's root set
    /// ([`resume_from_process_roots`](super::run::resume_from_process_roots)) **only** for a
    /// publishing window: an un-published (exempt) window's roots were never moved, so the
    /// machine already holds them. It is `false` on a fresh engine and cleared at every resume,
    /// so a stale `true` can never re-sync against registers an earlier window published.
    pub(super) parked_published: bool,
    /// The execution tier this process is running on, chosen at its **first** entry and
    /// remembered for the rest of its life.
    ///
    /// It is selected there by
    /// [`select_entry_tier`](super::tier::select_entry_tier) (or forced by a
    /// [`JitMode`](super::tier::JitMode)) and written by the runner; every later re-entry —
    /// a slice pre-empted on the reduction budget, or a `receive` that parked — resumes on
    /// **this** tier through
    /// [`Machine::resume_outcome_with_tier`](beam_engine::Machine::resume_outcome_with_tier),
    /// so a native-tier process is not silently demoted to the interpreter after its first
    /// slice. A process that has not run yet holds the conservative
    /// [`Tier::Interpreter`] (its first entry overwrites it), which is also the always-correct
    /// value if a tier were ever read before that entry.
    pub(super) tier: Tier,
    /// The process's off-heap **binary virtual heap** state — the budget
    /// (`beam_gc::BinVheap`, the C `p->bin_vheap_sz`) and the running overhead figure the
    /// slice-boundary reclamation compares it against, plus the reservoir observation pair that
    /// keeps that comparison `O(1)`.
    ///
    /// It lives **here** because this struct is the one per-pid runtime state the runtime owns: it
    /// is created with the machine ([`RunnerState::spawn`]) and dies with it, so the budget's
    /// lifetime is exactly the payload reservoir's — and the reclamation site
    /// ([`RuntimeRunner::reclaim_off_heap_binaries`](super::RuntimeRunner)) already holds
    /// `&mut ProcessEngine`, hence both `&mut machine` and `&mut off_heap` in one call without a
    /// second table.
    ///
    /// `None` until the first park reaches the reclamation, which builds it from
    /// `process.collector().policy()` — so a `+hms`/`+hbm*`/`spawn_opt` heap policy is honoured and
    /// a process that never builds off-heap payloads never pays for one.
    pub(super) off_heap: Option<crate::context::gc::OffHeapReclaimer>,
}

/// The per-pid engine state and recorded outcomes of every process the runtime spawned.
///
/// [`Runtime`](crate::runtime::Runtime) owns one; a scheduler pass *moves it out* so the
/// [`RuntimeRunner`](crate::runner::RuntimeRunner) can hold it (and the runtime) at the
/// same time as the scheduler holds the process table — see the module docs.
#[derive(Default)]
pub(crate) struct RunnerState {
    /// The live engines, keyed by pid.
    ///
    /// A [`FastHashMap`] (not `RandomState`'s SipHash): the map is only ever probed by pid
    /// (`get`/`insert`/`remove` in this module, `runner/run.rs` and `runtime/scheduler.rs`), never
    /// iterated for an observable order, and the keys are the runtime's own dense pids — see
    /// [`crate::hasher`].
    pub(super) engines: FastHashMap<ProcessId, ProcessEngine>,
    /// The `x(0)` of every process that ran to its `return`.
    pub(super) results: FastHashMap<ProcessId, Eterm>,
    /// The exit reason term of every process that finished by raising.
    pub(super) exits: FastHashMap<ProcessId, Eterm>,
    /// The **original** [`BeamError`] of every process that finished by raising.
    ///
    /// [`RunnerState::exits`] keeps only the reason *term* — what the process table
    /// propagates and [`Runtime::process_exit`](crate::runtime::Runtime::process_exit)
    /// exposes. A raise's **class** (`error`/`exit`/`throw`) and its `unsupported`
    /// classification live only on the error itself, so it is kept here too: the CLI's
    /// `run` action recovers it with
    /// [`Runtime::take_process_error`](crate::runtime::Runtime::take_process_error), which
    /// makes the pumped, scheduler-driven path classify exactly like the single-pass one.
    pub(super) errors: FastHashMap<ProcessId, BeamError>,
    /// The execution-tier selection mode, or `None` to use the environment default.
    ///
    /// `None` (the derived default) means "read `BEAM_RT_TIER` once" — see
    /// [`RunnerState::jit_mode`]. A test sets it directly to force a tier without touching
    /// the process environment (the crate is `#![deny(unsafe_code)]`, and `set_var` is
    /// `unsafe` in edition 2024) and the CLI is measured by setting the variable.
    pub(super) jit_mode: Option<super::tier::JitMode>,
}
impl RunnerState {
    /// The execution-tier mode in force: the explicitly set one, else the `BEAM_RT_TIER`
    /// environment default ([`JitMode::Auto`](super::tier::JitMode::Auto) when the variable
    /// is unset — the native baseline tier for every process entry).
    pub(super) fn jit_mode(&self) -> super::tier::JitMode {
        self.jit_mode.unwrap_or_else(super::tier::env_default)
    }

    /// Registers the engine of a freshly spawned process.
    ///
    /// The arguments are **not** asserted stable ([`ProcessEngine::arena_args`] is `false`), so
    /// this engine contributes no reclamation root; a spawn path that stabilized its terms uses
    /// [`RunnerState::spawn_stable`] instead.
    pub(crate) fn spawn(&mut self, pid: ProcessId, entry: ProcessEntry, args: Vec<Eterm>) {
        self.spawn_with_stability(pid, entry, args, false);
    }

    /// [`RunnerState::spawn`] for the spawn paths that copied the entry fun and every argument
    /// onto the **pointer-stable arena** first ([`Runtime::note_pending_spawn`], i.e. the
    /// `erlang:spawn*` calls made from inside a scheduler slice) — the only shape whose terms
    /// [`RunnerState::gather_off_heap_roots`] may hand over.
    ///
    /// [`Runtime::note_pending_spawn`]: crate::runtime::Runtime::note_pending_spawn
    pub(crate) fn spawn_stable(&mut self, pid: ProcessId, entry: ProcessEntry, args: Vec<Eterm>) {
        self.spawn_with_stability(pid, entry, args, true);
    }

    /// The one engine constructor, so the two staged/stable entry points cannot drift.
    fn spawn_with_stability(
        &mut self,
        pid: ProcessId,
        entry: ProcessEntry,
        args: Vec<Eterm>,
        arena_args: bool,
    ) {
        self.engines.insert(
            pid,
            ProcessEngine {
                machine: Machine::new(),
                entry,
                args,
                arena_args,
                parked: None,
                parked_timeout: None,
                // A freshly reset machine's FR file is all `NIL` (`fr_mark() == 0`), so its
                // first park (if any) publishes no FR word; the park arm overwrites this.
                fr_published: 0,
                // A fresh engine is not parked, so no window has published anything yet; the
                // park arm writes the real answer (see the field docs).
                parked_published: false,
                // Only a placeholder: the process's first entry selects the real tier and
                // writes it here (see the field docs).
                tier: Tier::Interpreter,
                // Built on the first park that reaches the reclamation, from the process's own
                // heap policy (see the field docs).
                off_heap: None,
            },
        );
    }

    /// Registers a freshly spawned process whose entry point is a **fun value**.
    ///
    /// This is the `erlang:spawn/1` (and `spawn_link/1`/`spawn_opt/2`) path: `fun` is an
    /// `-eval` closure or a `.beam` fun term and it is applied to `args` when the process
    /// first runs (see [`ProcessEntry::Fun`]). The launch target of a `.beam` local fun is
    /// resolved by that first run, not here.
    pub(crate) fn spawn_fun(&mut self, pid: ProcessId, fun: Eterm, args: Vec<Eterm>) {
        self.spawn(pid, ProcessEntry::Fun { fun, target: None }, args);
    }

    /// Appends every **stable** term a live engine still names to `out`: the arguments of a
    /// [`ProcessEngine::arena_args`] engine and the fun term of its [`ProcessEntry::Fun`] entry.
    ///
    /// An engine outlives the slice that created it (a `receive` resumes in the same machine), and
    /// its first arguments are the child's **first root set** — the arena copies a spawn made
    /// ([`RunnerState::spawn_stable`]). Nothing else in this runtime names them: they stand on the
    /// pointer-stable arena, not in any process's mailbox or dictionary, so without this the
    /// reclamation prunes (`crate::eval::reclaim`) answer a NIF resource a spawn argument carries
    /// unreachable and release it while the child still holds it.
    ///
    /// Only stable engines are enumerated, and the enumeration is `O(1)` when there are no
    /// engines at all — the overwhelmingly common case for a runtime that spawned nothing.
    pub(crate) fn gather_off_heap_roots(&self, out: &mut Vec<Eterm>) {
        for engine in self.engines.values() {
            if !engine.arena_args {
                continue;
            }
            out.extend_from_slice(&engine.args);
            if let ProcessEntry::Fun { fun, .. } = &engine.entry {
                out.push(*fun);
            }
        }
    }

    /// Whether `pid` is a process this runtime spawned an engine for.
    pub(crate) fn contains(&self, pid: ProcessId) -> bool {
        self.engines.contains_key(&pid)
    }

    /// `pid`'s executing module and engine machine, when this runtime holds them.
    ///
    /// This is the read-only seam behind `erlang:process_info/2`'s **execution-position**
    /// items (`current_function`/`current_location`/`current_stacktrace`/`initial_call`,
    /// see [`crate::native::memory`]): a process's position lives in its
    /// [`Machine`] — the
    /// [`Machine::current_function`]-family accessors are the only builders of those
    /// terms — and only this table knows which machine belongs to which pid.
    ///
    /// `None` for:
    ///
    /// * a pid this runtime never spawned an engine for (the root `-eval` process, a
    ///   foreign pid);
    /// * a **fun-bodied** process ([`ProcessEntry::Fun`]): nothing but the raw engine is
    ///   reachable through this accessor, so a caller wanting the position items of one
    ///   uses [`RunnerState::position_engine_at`] instead (it answers once the fun's local
    ///   target has been resolved, and reports the `erlang:apply/2` `$initial_call`);
    /// * the process whose machine the caller is *currently executing*: a scheduler pass
    ///   must take that one engine **out** of this map to drive it (see the module docs),
    ///   so it is simply absent while it runs.
    pub(crate) fn engine_at(&self, pid: ProcessId) -> Option<(&Module, &Machine)> {
        let engine = self.engines.get(&pid)?;
        match &engine.entry {
            ProcessEntry::Export { module, .. } => Some((module, &engine.machine)),
            ProcessEntry::Fun { .. } => None,
        }
    }

    /// `pid`'s off-heap refc-binary **reservoir**, when this runtime holds an engine for it.
    ///
    /// [`Machine::refc_binaries`] is the one holder of the off-heap payloads a process's
    /// compiled bitstring code builds: the engine appends every binary it materializes above
    /// [`ERL_ONHEAP_BINARY_LIMIT`](beam_term::refc::ERL_ONHEAP_BINARY_LIMIT) and
    /// [`Machine::reset`] never clears them, so the reservoir is the process's off-heap payload
    /// roster. It is **not** append-only in practice: it is pruned at the slice boundary
    /// whenever the process's binary virtual heap triggers
    /// ([`crate::context::gc::reclaim`] via `RuntimeRunner::reclaim_off_heap_binaries`), which
    /// releases every payload no live root reaches — so the roster no longer grows with the
    /// process's whole allocation history. It is what `erlang:process_info(Pid, binary)` reports
    /// ([`crate::native::memory`]) — read here rather than mirrored onto the process, because
    /// the PCB's own off-heap table is a never-compacted slot space whose per-call scans
    /// would otherwise grow with every payload the process ever built.
    ///
    /// Unlike [`RunnerState::engine_at`], which declines a fun-bodied process (it has no
    /// module to name an execution position from), this answers for **any** entry shape: the
    /// reservoir is per-machine state, independent of what the machine was entered as.
    ///
    /// `None` for a pid this runtime spawned no engine for (the root `-eval` process, a
    /// foreign pid), and for the process whose machine a scheduler pass is *currently*
    /// executing: a pass takes that one engine **out** of this map to drive it (see the module
    /// docs), so it is absent for the whole slice. Its payloads are owned either way — by the
    /// reservoir the running machine itself holds — so the worst a query about that one pid can
    /// do is omit an entry, never dangle one.
    pub(crate) fn reservoir_at(&self, pid: ProcessId) -> Option<&[beam_alloc::RefcBinary]> {
        self.engines
            .get(&pid)
            .map(|engine| engine.machine.refc_binaries())
    }

    /// `pid`'s executing module and machine for the **execution-position items**, plus
    /// whether the process's entry was a **fun applied as a value** (`erlang:spawn/1`).
    ///
    /// This extends [`RunnerState::engine_at`] with the two shapes that accessor declines,
    /// both of which the oracle *does* answer:
    ///
    /// * a **fun-bodied** process whose local target a first run has already resolved
    ///   (`ProcessEntry::Fun { target: Some(..) }`): its entry lambda belongs to a real
    ///   module, so `current_function`/`current_location`/`current_stacktrace` name it
    ///   exactly as an export entry's position is named. Oracle (OTP 29.1.1 / erts-17.1) for
    ///   `spawn(fun() -> receive stop -> ok end end)` parked in the receive:
    ///   `process_info(P, current_function)` ⇒ `{current_function,{M,'-f/0-fun-0-',0}}`
    ///   (the lambda's own name, resolved from its defining module's function table).
    /// * the returned `applied` flag: a fun-bodied process's `$initial_call` is
    ///   `erlang:apply/2`, not the entry lambda — ERTS spawned it by *applying* a fun
    ///   (oracle: `process_info(P, initial_call)` ⇒ `{initial_call,{erlang,apply,2}}`).
    ///
    /// `None` for everything [`RunnerState::engine_at`] declines plus a fun entry whose
    /// target is still unresolved (it has not run yet, so nothing can name its module).
    ///
    /// The process whose machine a scheduler pass is *currently executing* is absent from
    /// this map for the whole slice, exactly as in [`RunnerState::engine_at`]; its **own**
    /// `erlang:process_info/1,2` query is answered instead through the in-machine BIF door
    /// ([`crate::context::position`]), which the engine feeds the live machine.
    pub(crate) fn position_engine_at(&self, pid: ProcessId) -> Option<(&Module, &Machine, bool)> {
        let engine = self.engines.get(&pid)?;
        match &engine.entry {
            ProcessEntry::Export { module, .. } => Some((module, &engine.machine, false)),
            ProcessEntry::Fun { target, .. } => target
                .as_ref()
                .map(|target| (&target.module as &Module, &engine.machine, true)),
        }
    }

    /// The `x(0)` `pid` returned, once it ran to its `return`.
    pub(crate) fn result(&self, pid: ProcessId) -> Option<Eterm> {
        self.results.get(&pid).copied()
    }

    /// The raise reason `pid` finished with, once it failed.
    pub(crate) fn exit(&self, pid: ProcessId) -> Option<Eterm> {
        self.exits.get(&pid).copied()
    }

    /// Takes the **original** error `pid` failed with, once it failed.
    ///
    /// The reason *term* ([`RunnerState::exit`]) is what a process's exit propagates; the
    /// error itself is what carries the raise class and the `unsupported` classification,
    /// so the caller that must reproduce the CLI's diagnostics takes it here (see the field
    /// docs). Taking consumes it: the first caller gets it, later ones get `None`.
    pub(crate) fn take_error(&mut self, pid: ProcessId) -> Option<BeamError> {
        self.errors.remove(&pid)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use beam_term::{make_small, NIL};

    use super::*;

    /// The type pin for [`RunnerState::engines`]: the field really is the crate's fast-hashed map.
    /// This stops compiling if it ever reverts to `RandomState`'s `HashMap`.
    fn assert_fast_hashed(map: &FastHashMap<ProcessId, ProcessEngine>) -> usize {
        map.len()
    }

    /// The same pin for the three outcome maps.
    fn assert_fast_hashed_results(map: &FastHashMap<ProcessId, Eterm>) -> usize {
        map.len()
    }

    /// The same pin for [`RunnerState::exits`].
    fn assert_fast_hashed_exits(map: &FastHashMap<ProcessId, Eterm>) -> usize {
        map.len()
    }

    /// The same pin for [`RunnerState::errors`].
    fn assert_fast_hashed_errors(map: &FastHashMap<ProcessId, BeamError>) -> usize {
        map.len()
    }

    /// `RunnerState`'s four `ProcessId`-keyed maps are over [`crate::hasher::FastHashMap`], not
    /// `RandomState`'s SipHash.
    ///
    /// The soundness argument is what is pinned here, never a clock: every use of these four maps
    /// is a keyed `get`/`get_mut`/`insert`/`remove`/`contains_key` — this module's own accessors,
    /// `runner/run.rs`'s take/put of the executing engine and its recorded outcomes, and
    /// `runtime/scheduler.rs`'s `take_error` — with no iteration whose order is observable. So a
    /// cheaper hasher may change the probe cost and the (unobserved) order, and nothing else.
    #[test]
    fn the_runner_state_maps_are_fast_hashed() {
        // A mixed corpus: two pids differing in the serial alone, plus boundary shapes.
        let corpus = [
            ProcessId::new(0, 0),
            ProcessId::new(0, 1),
            ProcessId::new(1, 0),
            ProcessId::new(u64::MAX, u64::MAX),
        ];
        let mut state = RunnerState::default();
        let mut reference: HashMap<ProcessId, ()> = HashMap::new();
        for pid in corpus {
            state.spawn_fun(pid, make_small(0), Vec::new());
            reference.insert(pid, ());
        }
        state.results.insert(corpus[0], make_small(41));
        state.exits.insert(corpus[1], NIL);
        state.errors.insert(corpus[2], BeamError::BadArg);

        // The maps hold exactly what was inserted (and each field is the pinned concrete type).
        assert_eq!(assert_fast_hashed(&state.engines), reference.len());
        assert_eq!(assert_fast_hashed_results(&state.results), 1);
        assert_eq!(assert_fast_hashed_exits(&state.exits), 1);
        assert_eq!(assert_fast_hashed_errors(&state.errors), 1);

        // The verdicts agree with a default-hasher map: only the probe order is unobserved.
        for pid in corpus {
            assert_eq!(
                state.contains(pid),
                reference.contains_key(&pid),
                "the engine membership verdict agrees for {pid:?}"
            );
        }
        assert!(
            !state.contains(ProcessId::new(7, 7)),
            "a pid nobody spawned an engine for has none"
        );
        assert_eq!(state.result(corpus[0]), Some(make_small(41)));
        assert_eq!(state.exit(corpus[1]), Some(NIL));
        assert!(state.take_error(corpus[2]).is_some(), "the error is kept");
        assert!(
            state.take_error(corpus[2]).is_none(),
            "and taking consumes it"
        );
    }
}
