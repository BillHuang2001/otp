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

//! The runtime's **process-heap collector adoption**: the safety state, the accounting and
//! the out-of-band term stabilization.
//!
//! A scheduled process allocates on its **own** [`beam_process::Process`] young heap and the
//! collector behind it (`beam_gc::Collector`, reached through
//! [`Process::collect`](beam_process::Process::collect)) runs at the engine's
//! heap-admission point — see [`crate::context::gc`] for the GC point itself. This module
//! holds the state that decision needs and the two safety valves that keep the change
//! strictly behaviour-preserving:
//!
//! * the **pinning** set ([`GcState::pinned`]) — a pid whose engine float-register file was
//!   observed non-`NIL` is never collected again, because `Machine::fr` holds *terms* at
//!   Milestone 1 and is not part of the root set `ensure_heap` hands out;
//! * the **halt** set ([`GcState::halted`]) — a pid whose collection *failed* (or whose
//!   root-set sizes did not survive a collection) stops collecting, because a failing
//!   collection must never fail the program where the unbounded arena could not;
//! * the global **valve** ([`GcState::disabled`]) — tripped by
//!   [`Runtime::stabilize_escaped`] when a runtime-level store cannot be copied onto the
//!   pointer-stable arena. From then on **no** process heap is moved again, so the store's
//!   term can never dangle.
//!
//! Everything here is counters plus two small hash sets, deliberately: the runtime is
//! single-threaded and these are read on the dispatch path, not the hot path (the GC point
//! is only armed inside `runner::budget::BudgetContext::ensure_heap`).
//!
//! # Wiring (the remaining work — this module is not declared yet)
//!
//! This file is deliberately **not** in the module tree, so it is inert: it must be wired
//! before it compiles. The steps, in order:
//!
//! 1. `src/runtime.rs`: `mod gc;`, `pub use self::gc::{GcState, GcStatsSnapshot};` (the
//!    re-export is what lets the `pub fn gc_stats()` return a type from a private module),
//!    a `gc: GcState` field on `Runtime` and `gc: GcState::new()` in `Runtime::new`.
//! 2. `src/context/gc.rs` (new): `module_uses_floats(&Module) -> bool` (scan
//!    `module.functions[*].code` for `Fmove`/`Fconv`/`Fadd`/`Fsub`/`Fmul`/`Fdiv`/`Fnegate`)
//!    and `bump(&mut beam_alloc::Heap, n, &[Eterm]) -> Result<*mut Eterm>` — the safe
//!    hand-bump (`beam_alloc::Heap::alloc_words` is `unsafe fn`, but `hp()` is safe and a
//!    `&scratch[..n - 1]` `tuple()` write advances `used` by `n`, writing a valid header at
//!    the block base and `NIL`s after it).
//! 3. `src/context/mod.rs`: `mod gc;`, the `gc_armed: bool` latch and the `gc_safe:
//!    Option<bool>` flag on `RuntimeContext`, the `ensure_heap`/`heap_available`/
//!    `alloc_words` bodies moved into `context::gc` (deleting them from the
//!    `impl ExecutionContext` block), and a `pub(crate) fn arm_gc(&mut self)`.
//! 4. `src/runner/budget.rs`: `BudgetContext::ensure_heap` arms the latch **immediately
//!    before** delegating, and `RuntimeContext::ensure_heap` clears it as its first
//!    statement. This is what makes "the slice's own machine" exact: every nested context
//!    (`run_export`'s new `Machine`, a fun applied by a BIF, the evaluator) is a plain
//!    `RuntimeContext` whose `ensure_heap` can therefore never collect — see the deviation
//!    note in the completion report.
//! 5. `src/runner/run.rs`: after the slice, scan `machine.get_fr(i)` for
//!    `i in 0..beam_engine::machine::MAX_FLOAT_REGISTER` and `pin_process(pid)` if any slot
//!    is not `NIL`.
//! 6. The escape stores (`timers`, `persistent_terms`, `trace`, `seq_trace`,
//!    `RunnerState::{args,entry,exits}`, `pending_spawns`) go through
//!    [`Runtime::stabilize_escaped`]; `Runtime::references` must **not** (its keys are
//!    pointer-keyed and already minted on the never-reset pooled BIF heap).
use std::collections::{HashMap, HashSet};

use beam_loader::Module;
use beam_process::ProcessId;
use beam_term::Eterm;

use super::Runtime;

/// A snapshot of this runtime's process-heap collection activity.
///
/// The counters are **measured own-state** figures (they describe this runtime's own
/// collector, not the oracle's) and exist so a test — or a future `statistics/1` — can see
/// that the collector really ran, and *why it did not* when it did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcStatsSnapshot {
    /// How many collections ran (minor **and** major).
    pub collections: u64,
    /// How many of [`collections`](GcStatsSnapshot::collections) were fullsweeps.
    pub major: u64,
    /// How many pids are permanently pinned (a live float register was observed).
    pub pinned: usize,
    /// How many pids have their collection halted (a collection failed or mis-sized).
    pub halted: usize,
    /// Whether process-heap collection is globally disabled (the escape-store valve).
    pub disabled: bool,
    /// How many engine allocations fell back to the pointer-stable arena.
    pub fallbacks: u64,
    /// How many collections failed (each one halting its own process).
    pub errors: u64,
    /// How many escape-store stabilizations tripped the global valve.
    pub valve_trips: u64,
}

/// The runtime's process-heap collection state (see the module docs).
pub(crate) struct GcState {
    /// Per-module answer to "may this module's code be collected over?" — `true` when the
    /// module contains **no** float-family instruction (the float guard).
    ///
    /// Keyed by module name; a module's code never changes once loaded, so the answer is
    /// cached for the runtime's lifetime.
    float_free: HashMap<String, bool>,
    /// Pids that must never be collected again (a live float register was observed).
    pinned: HashSet<ProcessId>,
    /// Pids whose collection is halted (a collection failed, or the root-set sizes changed).
    halted: HashSet<ProcessId>,
    /// Whether **every** process heap must stay put (the global escape-store valve).
    disabled: bool,
    /// Why the global valve tripped (the first reason wins), for diagnostics.
    disabled_reason: Option<&'static str>,
    /// How many escape stores tripped the valve.
    valve_trips: u64,
    /// Collections run (minor and major).
    collections: u64,
    /// Fullsweeps among them.
    major: u64,
    /// Engine allocations that fell back to the arena.
    fallbacks: u64,
    /// Collections that failed.
    errors: u64,
    /// The reusable **placeholder fill** the safe heap bump needs (see
    /// [`crate::context::gc::bump`]): a slice of [`NIL`](beam_term::NIL) of the largest size
    /// asked for so far, so a bump does not allocate.
    ///
    /// The words it holds are never a term graph — they are overwritten by the engine before
    /// the block they pad is ever reachable — and it is deliberately *not* a GC root.
    scratch: Vec<Eterm>,
}

impl GcState {
    /// A fresh state: nothing collected, nothing pinned, the valve closed.
    pub(crate) fn new() -> GcState {
        GcState {
            float_free: HashMap::new(),
            pinned: HashSet::new(),
            halted: HashSet::new(),
            disabled: false,
            disabled_reason: None,
            valve_trips: 0,
            collections: 0,
            major: 0,
            fallbacks: 0,
            errors: 0,
            scratch: Vec::new(),
        }
    }
}

// The wiring step lands the call sites incrementally (the collection point first, then the
// escape-store stabilization), so every helper here is legitimately unused until its caller
// arrives. Replacing this with real use is what removes the attribute.
#[allow(dead_code)]
impl Runtime {
    /// The runtime's collection counters and safety state.
    pub fn gc_stats(&self) -> GcStatsSnapshot {
        GcStatsSnapshot {
            collections: self.gc.collections,
            major: self.gc.major,
            pinned: self.gc.pinned.len(),
            halted: self.gc.halted.len(),
            disabled: self.gc.disabled,
            fallbacks: self.gc.fallbacks,
            errors: self.gc.errors,
            valve_trips: self.gc.valve_trips,
        }
    }

    /// Whether **any** process heap may be moved — the global valve is closed and the
    /// runtime has not tripped it.
    pub(crate) fn gc_enabled(&self) -> bool {
        !self.gc.disabled
    }

    /// Whether `pid`'s own process heap may be moved (it is not pinned, not halted, and the
    /// global valve is closed).
    pub(crate) fn gc_process_enabled(&self, pid: ProcessId) -> bool {
        self.gc_enabled() && !self.gc.pinned.contains(&pid) && !self.gc.halted.contains(&pid)
    }

    /// Pins `pid` on the runtime: its heap is never moved again.
    ///
    /// Used when the machine's float-register file holds a live term — `Machine::fr` is not
    /// part of the root set the engine hands `ensure_heap`, and this crate may neither read
    /// nor write it from there, so a move while a float is live could silently corrupt float
    /// code. Pinning is permanent for the runtime: the register is only cleared by
    /// `Machine::reset`, which this crate cannot observe.
    pub(crate) fn pin_process(&mut self, pid: ProcessId) {
        self.gc.pinned.insert(pid);
    }

    /// Records that a collection ran.
    pub(crate) fn note_gc_collection(&mut self, major: bool) {
        self.gc.collections += 1;
        if major {
            self.gc.major += 1;
        }
    }

    /// Records that an engine allocation could not be served from a process young heap and
    /// fell back to the pointer-stable arena.
    pub(crate) fn note_gc_fallback(&mut self) {
        self.gc.fallbacks += 1;
    }

    /// Records that `pid`'s collection failed, and stops collecting for it.
    ///
    /// The pre-GC runtime was an unbounded arena that could not fail at the heap-admission
    /// point, so a failing collection must not become a program failure: the process simply
    /// keeps its (unmoved) heap and allocation continues to fall back to the arena.
    pub(crate) fn halt_process_gc(&mut self, pid: ProcessId) {
        self.gc.errors += 1;
        self.gc.halted.insert(pid);
    }

    /// Trips the global valve: no process heap is ever moved again.
    ///
    /// The first reason wins, so the diagnostic names the store that disabled collection.
    pub(crate) fn disable_gc(&mut self, reason: &'static str) {
        if !self.gc.disabled {
            self.gc.disabled = true;
            self.gc.disabled_reason = Some(reason);
        }
        self.gc.valve_trips += 1;
    }

    /// Why the global valve tripped, if it did.
    #[allow(dead_code)]
    pub(crate) fn gc_disabled_reason(&self) -> Option<&'static str> {
        self.gc.disabled_reason
    }

    /// Whether `name`'s module may be collected over — `true` when the module contains no
    /// float-family instruction (cached per module name).
    pub(crate) fn module_is_float_free(&mut self, name: &str, module: &Module) -> bool {
        if let Some(&known) = self.gc.float_free.get(name) {
            return known;
        }
        let answer = !crate::context::gc::module_uses_floats(module);
        self.gc.float_free.insert(name.to_string(), answer);
        answer
    }

    /// Copies `term` onto the pointer-stable arena and returns the copy — the
    /// **escape-store** stabilization every runtime-level store that can hold a term built by
    /// a running process must go through.
    ///
    /// A term on a scheduled process's young heap is only kept alive while it is reachable
    /// from that process's root set ([`Process::collect`](beam_process::Process::collect)),
    /// so a term parked in a *runtime*-level store (a timer's message, a persistent term, a
    /// trace match spec, a pending spawn's arguments, …) would otherwise dangle the moment
    /// the heap it lives on is moved or dropped. The copy is semantically identical (terms
    /// are immutable and compared structurally), so it is an unobservable change — with one
    /// exception this function handles by tripping the valve:
    ///
    /// [`beam_message::copy_struct_simple`] can only copy what this runtime's own copy engine
    /// covers. A term that contains an **off-heap** payload (a reference-counted large binary,
    /// MSO data, an external pid/port/ref) is copied as part of a message but is not
    /// trivially duplicable here, so the original term is stored instead **and every process
    /// heap is frozen** ([`Runtime::disable_gc`]). That keeps the store exactly as correct as
    /// it was before the collector existed, at the price of collecting nothing for the rest of
    /// the run.
    pub(crate) fn stabilize_escaped(&mut self, term: Eterm) -> Eterm {
        match self.stabilize_term(term) {
            Ok(copy) => copy,
            Err(_) => {
                self.disable_gc("an out-of-band store holds a term with an off-heap payload");
                term
            }
        }
    }

    /// The reusable placeholder fill of `n` words the safe heap bump pads with.
    ///
    /// Only ever grown, so a bump never allocates (see
    /// [`crate::context::gc::bump`]); the words are [`NIL`](beam_term::NIL) — exactly what an
    /// uninitialized word of the arena is, so a block the engine allocated but has not written
    /// yet holds the same words on either heap.
    pub(crate) fn gc_scratch(&mut self, n: usize) -> &[Eterm] {
        if self.gc.scratch.len() < n {
            self.gc.scratch.resize(n, beam_term::NIL);
        }
        &self.gc.scratch[..n]
    }
}
