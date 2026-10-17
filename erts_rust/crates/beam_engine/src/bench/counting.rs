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

//! The counting [`ExecutionContext`] the foreign-workload harness drives the engine through.
//!
//! A thin, delegating wrapper over [`MinimalContext`] that tallies the four engine→host
//! doors the harness reports on — the per-instruction reduction hook
//! ([`reduce`](ExecutionContext::reduce)), the per-built-term allocation door
//! ([`alloc_words_rooted`](ExecutionContext::alloc_words_rooted)), the per-`test_heap`
//! admission door ([`ensure_heap_full`](ExecutionContext::ensure_heap_full) /
//! [`ensure_heap`](ExecutionContext::ensure_heap)) and the BIF dispatch
//! ([`call_bif`](ExecutionContext::call_bif), with a `module:function/arity` histogram) —
//! plus one counter each for the remaining dispatch seams (`apply`, `apply_fun`, the three
//! resolvers) and the unrooted allocation path.
//!
//! Every counter is a **deterministic count**, not a timing: it is exactly what the engine
//! asked the host for, so the figures reproduce run to run and on any host.
//!
//! Two delegation facts make the numbers trustworthy:
//!
//! * [`MinimalContext::wants_reduce`] answers `false` (it has no scheduler, so it opts out
//!   of the per-instruction vtable crossing). This wrapper **must** therefore override
//!   [`wants_reduce`](ExecutionContext::wants_reduce) to `true` — the engine only calls
//!   [`reduce`](ExecutionContext::reduce) when the host asks for it, so without that the
//!   instruction count would silently stay zero.
//! * the wrapper installs an **external-call resolver** over the single module under
//!   measurement, so a `call_ext` naming a function of that *same* module is entered **in
//!   this machine** instead of folding into a `call_bif` the minimal host rejects. In practice
//!   the compiler emits a same-module callee as a **local** `call_last {f,Label}` (which is why
//!   the report shows `local_call_requests` and no `external_call_requests`), so the resolver
//!   is the safety net for the `call_ext` shape rather than the path this workload takes.

use std::collections::BTreeMap;
use std::sync::Arc;

use beam_common::Result;
use beam_loader::{Label, Module};
use beam_term::{Atom, AtomTable, Eterm};

use crate::context::{ExecutionContext, MinimalContext, RootSet};

/// The counters one measured phase accumulates.
///
/// All fields are `pub(crate)` so the harness in [`super`] can format them without a second
/// accessor layer; the type lives beside the context that owns it because they are one unit.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    /// Reductions charged by the engine's per-instruction hook (`n = 1` per **interpreted**
    /// instruction; the native baseline tier makes no such call, so this is the
    /// interpreter's instruction count only).
    pub(crate) instructions: u64,
    /// `alloc_words_rooted` calls: the per-built-term allocation admission door.
    pub(crate) rooted_calls: u64,
    /// Heap words requested through `alloc_words_rooted`.
    pub(crate) rooted_words: u64,
    /// `ensure_heap_full`/`ensure_heap` calls: the per-`test_heap`/`allocate_heap` door.
    pub(crate) heap_calls: u64,
    /// Heap words requested through those two doors.
    pub(crate) heap_words: u64,
    /// `call_bif` calls (an in-crate BIF dispatch).
    pub(crate) bif_calls: u64,
    /// `call_bif` calls keyed `"module:function/arity"`.
    pub(crate) bif_histogram: BTreeMap<String, u64>,
    /// `apply` calls (the general `apply`/`call_ext` hook).
    pub(crate) apply_calls: u64,
    /// `apply_fun` calls (the value-returning closure-application fallback).
    pub(crate) apply_fun_calls: u64,
    /// `resolve_external_call` requests: the `call_ext*` cross-module-call seam.
    pub(crate) external_call_requests: u64,
    /// `resolve_local_fun` requests: the in-machine closure-application seam.
    pub(crate) local_fun_requests: u64,
    /// `resolve_local_call` requests: the local-call-as-BIF seam. One per **local**
    /// `call`/`call_only`/`call_last` (which is how the compiler emits a same-module callee).
    pub(crate) local_call_requests: u64,
    /// `alloc_words` calls: the **unrooted** allocation path (no admission point).
    pub(crate) unrooted_calls: u64,
}

/// A [`MinimalContext`] that counts what the engine asks it for.
///
/// The inner context is replaced between batches (see [`CountingContext::reset_inner`]) while
/// the counters keep accumulating, so a phased measurement reports one set of counts for the
/// whole phase. The module handle is kept so a fresh inner always gets the same
/// external-call resolver (see the module docs).
pub(crate) struct CountingContext {
    /// The wrapped heap/BIF host.
    inner: MinimalContext,
    /// The module the resolver serves (a shared handle, so the engine's registration dedups
    /// by `Arc::ptr_eq` and a `call_ext` into it only ever allocates one module context).
    module: Arc<Module>,
    /// This phase's counters.
    pub(crate) counters: Counters,
}

impl CountingContext {
    /// A counting context whose inner host resolves `call_ext`s against `module`.
    pub(crate) fn new(module: Arc<Module>) -> CountingContext {
        let inner = fresh_inner(&module);
        CountingContext {
            inner,
            module,
            counters: Counters::default(),
        }
    }

    /// Drop the inner host's region heap and start a fresh one (same resolver, same counters).
    ///
    /// A [`MinimalContext`] never frees what it hands out — every term it built since the
    /// last reset stays mapped — so a long measurement must retire the host periodically or
    /// the harness' own fixture terms, not the engine, would dominate memory.
    pub(crate) fn reset_inner(&mut self) {
        self.inner = fresh_inner(&self.module);
    }

    /// The inner host, for fixture setup the harness must **not** charge to the engine (the
    /// reused input term is built through this path, exactly as a caller of the engine would
    /// build its own argument before timing a run).
    pub(crate) fn inner_mut(&mut self) -> &mut MinimalContext {
        &mut self.inner
    }
}

/// A [`MinimalContext`] whose `call_ext` seam serves `module`'s own functions.
///
/// A `call_ext` that names a function of the module the machine is already running would
/// otherwise fall through to the synchronous BIF path, which a minimal host rejects; answering
/// the seam here makes the engine enter that code **in this machine**, exactly as it would for
/// another module. The answer is the function's `entry_label` — the same address
/// [`Module::export`] hands out for a genuinely exported function.
fn fresh_inner(module: &Arc<Module>) -> MinimalContext {
    let handle = Arc::clone(module);
    MinimalContext::with_external_resolver(Box::new(move |callee, function, arity| {
        let Some(callee) = handle.atom_table.lookup(callee) else {
            return Ok(None);
        };
        if callee != handle.name {
            return Ok(None);
        }
        let Some(function) = handle.atom_table.lookup(function) else {
            return Ok(None);
        };
        Ok(handle
            .function(function, arity)
            .map(|entry| (Arc::clone(&handle), entry.entry_label)))
    }))
}

impl ExecutionContext for CountingContext {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        self.counters.bif_calls += 1;
        *self
            .counters
            .bif_histogram
            .entry(format!("{module}:{function}/{arity}"))
            .or_insert(0) += 1;
        self.inner.call_bif(module, function, arity, args)
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.counters.heap_calls += 1;
        self.counters.heap_words += u64::from(heap_need);
        self.inner.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn ensure_heap_full(&mut self, heap_need: u32, roots: RootSet<'_>) -> Result<()> {
        self.counters.heap_calls += 1;
        self.counters.heap_words += u64::from(heap_need);
        self.inner.ensure_heap_full(heap_need, roots)
    }

    fn heap_available(&self) -> usize {
        self.inner.heap_available()
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.counters.unrooted_calls += 1;
        self.inner.alloc_words(n)
    }

    fn alloc_words_rooted(&mut self, n: usize, roots: RootSet<'_>) -> Result<*mut Eterm> {
        self.counters.rooted_calls += 1;
        self.counters.rooted_words += n as u64;
        self.inner.alloc_words_rooted(n, roots)
    }

    fn apply(&mut self, module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
        self.counters.apply_calls += 1;
        self.inner.apply(module, function, arity, args)
    }

    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        self.counters.apply_fun_calls += 1;
        self.inner.apply_fun(fun, args)
    }

    fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()> {
        self.inner.send_message(to, message)
    }

    fn resolve_external_call(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
    ) -> Result<Option<(Arc<Module>, Label)>> {
        self.counters.external_call_requests += 1;
        self.inner.resolve_external_call(module, function, arity)
    }

    fn resolve_local_fun(
        &mut self,
        fun: Eterm,
        args: &[Eterm],
    ) -> Result<Option<(Arc<Module>, Label)>> {
        self.counters.local_fun_requests += 1;
        self.inner.resolve_local_fun(fun, args)
    }

    fn resolve_local_call(&mut self, module: &str, function: &str, arity: u32) -> Result<bool> {
        self.counters.local_call_requests += 1;
        self.inner.resolve_local_call(module, function, arity)
    }

    fn atom_name(&self, module: &Module, atom: Atom) -> Option<String> {
        self.inner.atom_name(module, atom)
    }

    fn atoms<'m>(&'m self, module: &'m Module) -> &'m AtomTable {
        self.inner.atoms(module)
    }

    fn reduce(&mut self, n: u32) -> Result<()> {
        self.counters.instructions += u64::from(n);
        self.inner.reduce(n)
    }

    /// `true`: the harness *wants* the per-instruction hook, unlike the inner host (which
    /// opts out because its `reduce` is the trait's no-op). The engine latches this once per
    /// run, so a `false` here would leave [`Counters::instructions`] at zero.
    fn wants_reduce(&self) -> bool {
        true
    }

    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        self.inner.receive_next()
    }

    fn receive_skip(&mut self) -> Result<()> {
        self.inner.receive_skip()
    }

    fn receive_remove(&mut self) -> Result<()> {
        self.inner.receive_remove()
    }

    fn receive_reset(&mut self) -> Result<()> {
        self.inner.receive_reset()
    }

    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        self.inner.receive_marker_reserve()
    }

    fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()> {
        self.inner.receive_marker_bind(marker, reference)
    }

    fn receive_marker_use(&mut self, reference: Eterm) -> Result<()> {
        self.inner.receive_marker_use(reference)
    }

    fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()> {
        self.inner.receive_marker_clear(reference)
    }
}
