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

use std::collections::HashMap;
use std::sync::Arc;

use beam_common::BeamError;
use beam_engine::Machine;
use beam_loader::Module;
use beam_process::ProcessId;
use beam_term::{Atom, Eterm};

use crate::context::LocalFunTarget;
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
    /// The first run's arguments (seeded into `x(0..arity)`); unused once the process
    /// has run, but kept so a re-run of a finished process is impossible to get wrong.
    pub(super) args: Vec<Eterm>,
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
}

/// The per-pid engine state and recorded outcomes of every process the runtime spawned.
///
/// [`Runtime`](crate::runtime::Runtime) owns one; a scheduler pass *moves it out* so the
/// [`RuntimeRunner`](crate::runner::RuntimeRunner) can hold it (and the runtime) at the
/// same time as the scheduler holds the process table — see the module docs.
#[derive(Default)]
pub(crate) struct RunnerState {
    /// The live engines, keyed by pid.
    pub(super) engines: HashMap<ProcessId, ProcessEngine>,
    /// The `x(0)` of every process that ran to its `return`.
    pub(super) results: HashMap<ProcessId, Eterm>,
    /// The exit reason term of every process that finished by raising.
    pub(super) exits: HashMap<ProcessId, Eterm>,
    /// The **original** [`BeamError`] of every process that finished by raising.
    ///
    /// [`RunnerState::exits`] keeps only the reason *term* — what the process table
    /// propagates and [`Runtime::process_exit`](crate::runtime::Runtime::process_exit)
    /// exposes. A raise's **class** (`error`/`exit`/`throw`) and its `unsupported`
    /// classification live only on the error itself, so it is kept here too: the CLI's
    /// `run` action recovers it with
    /// [`Runtime::take_process_error`](crate::runtime::Runtime::take_process_error), which
    /// makes the pumped, scheduler-driven path classify exactly like the single-pass one.
    pub(super) errors: HashMap<ProcessId, BeamError>,
    /// The execution-tier selection mode, or `None` to use the environment default.
    ///
    /// `None` (the derived default) means "read `BEAM_RT_TIER` once" — see
    /// [`RunnerState::jit_mode`]. A test sets it directly to force a tier without touching
    /// the process environment (the crate is `#![deny(unsafe_code)]`, and `set_var` is
    /// `unsafe` in edition 2024) and the CLI is measured by setting the variable.
    pub(super) jit_mode: Option<super::tier::JitMode>,
    /// The memoized per-module tier switch-safety verdict, keyed by module name.
    ///
    /// [`jit_is_switch_safe`](super::tier::jit_is_switch_safe) is evaluated once per
    /// module per pass and is a linear scan of the module's code, so a benchmark that
    /// spawns many processes of one module would otherwise rescan it on every spawn. The
    /// memo is sound because a "safe" verdict is sticky — the floor list is constant and
    /// the NIF/registry sets only grow (see `super::tier`).
    pub(super) jit_safety: HashMap<String, bool>,
}
impl RunnerState {
    /// The execution-tier mode in force: the explicitly set one, else the `BEAM_RT_TIER`
    /// environment default ([`JitMode::Auto`](super::tier::JitMode::Auto) when unset).
    pub(super) fn jit_mode(&self) -> super::tier::JitMode {
        self.jit_mode.unwrap_or_else(super::tier::env_default)
    }

    /// Registers the engine of a freshly spawned process.
    pub(crate) fn spawn(&mut self, pid: ProcessId, entry: ProcessEntry, args: Vec<Eterm>) {
        self.engines.insert(
            pid,
            ProcessEngine {
                machine: Machine::new(),
                entry,
                args,
                parked: None,
                parked_timeout: None,
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
    /// * a **fun-bodied** process ([`ProcessEntry::Fun`]): its entry is a fun value, and
    ///   the position items stay the honest capability gap there (they are named from the
    ///   module's function table, not from the lambda the engine entered), never a
    ///   fabricated module;
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
