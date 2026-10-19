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
//!
//! # The harness' own (tiny) BIF floor
//!
//! Most workloads here are **BIF-free by construction** (`cowboy_bstr:to_lower/1`,
//! `cowboy_bstr:capitalize_token/1`, the `list_walk` program, the bitstring fixtures), so the
//! inner host's rejecting default hook is exactly right for them. Two real foreign functions
//! are not: `emqx_base62:encode/1` calls `erlang:iolist_to_binary/1` once per encoded group, and
//! the base62 charset functions do small-integer arithmetic. [`CountingContext::host_bif`]
//! therefore serves that **closed, documented set** — `iolist_to_binary/1` plus the small-int
//! `+ - * div rem abs` — and **every other BIF stays rejected** with the inner host's typed
//! `unsupported_bif`.
//!
//! Two rules make that floor safe to trust:
//!
//! * it **never guesses**: an operand that is not a small integer, an integer overflow, a
//!   non-`0..=255` byte, an improper list or a bitstring that is not byte-aligned is a typed
//!   `BadArg`/`unsupported`, never a substituted value;
//! * it deliberately does **not** serve `erlang:bit_size/1`. A half-correct `bit_size` would
//!   turn a *guard* failure into a silently wrong result (the engine folds a guard BIF's error
//!   into a failed guard), and a correct one needs a raw-heap bit-size reader that this node
//!   cannot reach — so the `emqx_base62:decode/1` case reports a loud skip instead (see
//!   [`super::foreign`]).
//!
//! The allocating BIF (`iolist_to_binary/1`) is served **here**, not by the inner host's
//! `set_bif_hook`: a [`BifHook`](crate::context::BifHook) receives `(module, function, arity,
//! args)` and *no host handle*, so a hook cannot allocate. Serving it in the wrapper's
//! [`call_bif`](ExecutionContext::call_bif) puts the result on the same region heap the machine
//! is running on (`make_heap_binary` goes through this wrapper's own
//! [`alloc_words`](ExecutionContext::alloc_words), so the floor's allocation is counted too) and
//! survives `reset_inner` by construction, because the floor is a property of the wrapper rather
//! than of the host it wraps.

use std::collections::BTreeMap;
use std::sync::Arc;

use beam_common::{BeamError, Result};
use beam_loader::{Label, Module};
use beam_term::{is_small, make_small, signed_value, Atom, AtomTable, Eterm, MAX_SMALL, MIN_SMALL};

use crate::context::{ExecutionContext, MinimalContext, RootSet};
use crate::terms::{binary_bytes, list_elements, make_heap_binary};

/// The deepest iolist nesting `erlang:iolist_to_binary/1`'s floor implementation will flatten.
///
/// A guard rail against a cyclic/absurd term turning a benchmark into a stack overflow; real
/// iolists are a handful of levels deep.
const MAX_IOLIST_DEPTH: usize = 64;

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
    /// BIF dispatches the harness' own floor **recognised by name but declined** on the
    /// operands it was given (see the module docs' "never guesses" rule).
    ///
    /// A decline is a bench-harness limitation, never an engine result: the workload's own
    /// correctness predicate is what catches a decline that changed an answer, and the report
    /// asserts this counter is zero so a reader never has to wonder.
    pub(crate) floor_declines: u64,
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

    /// The harness' **closed** BIF floor: `Some(result)` when the name is one the floor
    /// models, `None` when the call belongs to the inner host (which rejects it).
    ///
    /// The served set is deliberately tiny and named in the module docs:
    /// `erlang:iolist_to_binary/1` plus the small-int `erlang:'+'/2`, `'-'/2`, `'*'/2`,
    /// `div/2`, `rem/2` and `abs/1`. Every other `(module, function, arity)` is `None`, so the
    /// call keeps the inner host's typed `unsupported_bif` — a workload that needed it reports
    /// a loud skip rather than a figure.
    ///
    /// The floor **allocates through this context** ([`make_heap_binary`] →
    /// [`ExecutionContext::alloc_words`]), so its traffic is counted like the engine's own and
    /// it keeps working across [`Self::reset_inner`].
    fn host_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Option<Result<Eterm>> {
        if module != "erlang" || args.len() != arity as usize {
            return None;
        }
        match (function, arity) {
            ("iolist_to_binary", 1) => {
                let mut bytes = Vec::new();
                Some(match flatten_iolist(args[0], &mut bytes, 0) {
                    Ok(()) => make_heap_binary(self, &bytes),
                    Err(error) => Err(error),
                })
            }
            ("+", 2) | ("-", 2) | ("*", 2) | ("div", 2) | ("rem", 2) => {
                Some(small_int_arith(function, args[0], args[1]))
            }
            ("abs", 1) => Some(small_int_abs(args[0])),
            _ => None,
        }
    }
}

/// The bench harness' refusal: a typed error that names the floor, so a printed skip says
/// "the harness declined this" rather than "the engine failed".
fn refuse(what: &str) -> BeamError {
    crate::error::unsupported(&format!("bench BIF floor: {what}"))
}

/// Flatten `term` — a byte, a binary or a proper (possibly nested) list of those — into
/// `out`, reproducing `erlang:iolist_to_binary/1`.
///
/// Anything it cannot flatten **exactly** is [`refuse`]d: a non-byte small integer, a
/// non-byte-aligned bitstring, an improper tail, an atom/float/tuple element, or nesting
/// deeper than [`MAX_IOLIST_DEPTH`]. Nothing is ever coerced.
fn flatten_iolist(term: Eterm, out: &mut Vec<u8>, depth: usize) -> Result<()> {
    if depth > MAX_IOLIST_DEPTH {
        return Err(refuse("iolist nesting is deeper than MAX_IOLIST_DEPTH"));
    }
    if is_small(term) {
        // `is_byte` is the frozen `0..=255` small-int test, so a negative or wide integer is
        // declined instead of being truncated to a byte.
        if !beam_term::is_byte(term) {
            return Err(refuse("iolist element is a small integer outside 0..=255"));
        }
        out.push(signed_value(term) as u8);
        return Ok(());
    }
    if beam_term::is_nil(term) {
        return Ok(());
    }
    // A byte-aligned binary — on-heap or off-heap refc. A non-byte-aligned bitstring is not a
    // legal iolist element and `binary_bytes` rejects it, so it falls through to the refusal.
    if let Ok(bytes) = binary_bytes(term) {
        out.extend_from_slice(bytes);
        return Ok(());
    }
    if beam_term::is_list(term) {
        for element in list_elements(term)? {
            flatten_iolist(element, out, depth + 1)?;
        }
        return Ok(());
    }
    Err(refuse(
        "iolist element is neither a byte, a binary nor a proper list",
    ))
}

/// One operand of the floor's arithmetic: a **small** integer, never a bignum, float or atom.
fn small_operand(function: &str, term: Eterm) -> Result<i64> {
    if !is_small(term) {
        return Err(refuse(&format!(
            "erlang:{function} was given a non-small-integer operand"
        )));
    }
    Ok(signed_value(term))
}

/// A checked small-int result: outside [`MIN_SMALL`]`..=`[`MAX_SMALL`] is [`refuse`]d (the
/// real BIF would promote to a bignum, which this floor does not model — declining is the
/// honest answer, and [`Counters::floor_declines`] makes it visible).
fn small_result(function: &str, value: i64) -> Result<Eterm> {
    if value < MIN_SMALL || value > MAX_SMALL {
        return Err(refuse(&format!(
            "erlang:{function} left the small-integer range ({value})"
        )));
    }
    Ok(make_small(value))
}

/// `erlang:'+'/2`, `'-'/2`, `'*'/2`, `div/2`, `rem/2` over two small integers.
fn small_int_arith(function: &str, lhs: Eterm, rhs: Eterm) -> Result<Eterm> {
    let lhs = small_operand(function, lhs)?;
    let rhs = small_operand(function, rhs)?;
    let value = match function {
        "+" => lhs.checked_add(rhs),
        "-" => lhs.checked_sub(rhs),
        "*" => lhs.checked_mul(rhs),
        "div" => lhs.checked_div(rhs),
        // `checked_rem` panics on `i64::MIN % -1` in debug builds, so exclude it explicitly.
        "rem" => {
            if rhs == 0 || (lhs == i64::MIN && rhs == -1) {
                None
            } else {
                Some(lhs % rhs)
            }
        }
        other => return Err(refuse(&format!("erlang:{other}/2 is not modelled"))),
    };
    let Some(value) = value else {
        return Err(refuse(&format!(
            "erlang:{function}/2 on {lhs} and {rhs} (zero divisor or overflow)"
        )));
    };
    small_result(function, value)
}

/// `erlang:abs/1` over a small integer.
fn small_int_abs(operand: Eterm) -> Result<Eterm> {
    let value = small_operand("abs", operand)?;
    let Some(value) = value.checked_abs() else {
        return Err(refuse("erlang:abs/1 overflowed"));
    };
    small_result("abs", value)
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
        // The harness' own floor comes first; anything it does not model keeps the inner
        // host's rejecting behaviour. A *declined* operand is counted so the report can
        // assert the floor never silently shaped a result.
        if let Some(result) = self.host_bif(module, function, arity, args) {
            if result.is_err() {
                self.counters.floor_declines += 1;
            }
            return result;
        }
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
