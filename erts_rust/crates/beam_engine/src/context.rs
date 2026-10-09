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

//! The runtime seam: everything the engine needs from its host.
//!
//! [`ExecutionContext`] isolates BIF dispatch (`call_bif`), heap management
//! (`ensure_heap`) and the **mailbox** (`send_message`/`receive_*`) behind a trait,
//! so the Stage-1 engine depends only on `beam_loader` and never on the Stage-5
//! runtime core (`../CONTEXT.md`, "The seam types"). `beam_rt` supplies the
//! concrete implementation; [`MinimalContext`] is the test/embedding
//! implementation (unbounded heap plus a pluggable BIF hook and mailbox).
//!
//! The same trait also exposes the hooks the term-building layer
//! ([`crate::terms`]) and the call paths need: [`alloc_words`] hands out a stable
//! pointer to raw heap words, [`apply`] is the general external-function dispatch
//! entry point, and [`apply_fun`] applies a fun/closure value (the `call_fun`/
//! `apply` path). All are added with default bodies, so an existing implementor
//! keeps compiling unchanged.
//!
//! ## The root-set hand-off (GC admission for `alloc_words`)
//!
//! [`ensure_heap`] is the engine's *admission point*: it hands the live root set
//! over as mutable slices so the host can collect and move terms in place. Term
//! **construction**, however, goes through [`alloc_words`], which has neither an
//! admission point nor root access — so a host whose heap must be collected cannot
//! collect while the engine builds a term. [`alloc_words_rooted`] closes that gap:
//! it is the rooted twin of [`alloc_words`], taking a [`RootSet`] (the X
//! registers, the whole Erlang stack and the whole FR file) and defaulting to
//! [`alloc_words`] with the roots discarded, so **every existing implementor
//! compiles and behaves byte-identically**. [`crate::machine::Machine::alloc_rooted`]
//! is where the engine's own slices are handed over.//!
//! ## The mailbox / message-receive seam
//!
//! The `receive` opcode family ([`crate::families::receive`]) never looks at a
//! message's *contents*: the compiled code's match tests decide whether the
//! current candidate fits, and the family only ever **positions** the mailbox scan
//! (`loop_rec`/`loop_rec_end`), **removes** the matched candidate
//! (`remove_message`) or resets the position (`timeout`), plus the four
//! `recv_marker_*` bookkeeping opcodes. That host interface is [`Mailbox`], and
//! [`ExecutionContext`] carries one defaulted method per entry (each returning a
//! typed [`crate::error::unsupported`] out of the box) so any implementor — the
//! production `beam_rt` context, a test harness or [`MinimalContext`] — compiles
//! unchanged and opts in only to what it can serve.
//!
//! A **receive marker** term is fully **opaque** to the engine: it is produced by
//! [`receive_marker_reserve`](ExecutionContext::receive_marker_reserve), handed
//! back verbatim by `recv_marker_bind`, and only a separate *reference* term is
//! ever passed to `recv_marker_use`/`recv_marker_clear`. The engine stores such a
//! marker in a register and never inspects, compares or hashes it.
//!
//! Installing a mailbox does **not** change the semantics of the non-blocking
//! entries: they cannot suspend, so a `receive` that blocks still surfaces as the
//! typed [`crate::error::blocked`] — the mailbox only decides *whether* the scan
//! blocks, never what a blocking run reports.
//!
//! ## The execution-trace seam
//!
//! The engine reports the `call`/`return_from`/`return_to` event family (contract
//! A.8) through two **defaulted** methods: [`ExecutionContext::trace_enabled`] (a
//! cheap gate, `false` out of the box — one predictable branch per call/return
//! instruction) and [`ExecutionContext::trace_event`] (a no-op out of the box).
//! The events are the raw ones the engine's own instruction stream produces,
//! borrowed straight from the running module so a traced run allocates nothing on
//! the engine's side; the host decides *matching*, exactly as the oracle arms
//! individual functions with `erlang:trace_pattern/3`.
//!
//! [`alloc_words`]: ExecutionContext::alloc_words
//! [`apply`]: ExecutionContext::apply
//! [`apply_fun`]: ExecutionContext::apply_fun

use std::sync::Arc;

use beam_common::Result;
use beam_loader::{Label, Module};
use beam_term::Eterm;

/// The mailbox/message-receive seam: the receive opcode family's host interface.
///
/// One method per receive opcode group; `beam_engine` drives them at instruction
/// boundaries and keeps no mailbox state of its own. The scan position is
/// entirely the host's: the engine never learns *which* message is current, only
/// the candidate [`receive_next`](Mailbox::receive_next) returns.
///
/// [`ExecutionContext`] exposes one defaulted method per entry below, so a host
/// implements the mailbox either by overriding those methods directly or — like
/// [`MinimalContext`] — by owning a `Box<dyn Mailbox>` and forwarding to it.
pub trait Mailbox {
    /// `send`: deliver `message` to the pid/port term `to`.
    fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()>;

    /// `loop_rec`: the candidate at the current mailbox scan position, `Ok(None)`
    /// when the scan/mailbox is exhausted.
    fn receive_next(&mut self) -> Result<Option<Eterm>>;

    /// `loop_rec_end`: advance the scan position past the current candidate.
    fn receive_skip(&mut self) -> Result<()>;

    /// `remove_message`: unlink the currently-matched candidate and reposition the
    /// scan at the first message.
    fn receive_remove(&mut self) -> Result<()>;

    /// `timeout`: reset the scan position to the first message.
    fn receive_reset(&mut self) -> Result<()>;

    /// `recv_marker_reserve`: an opaque marker term for the current scan position.
    fn receive_marker_reserve(&mut self) -> Result<Eterm>;

    /// `recv_marker_bind`: bind the opaque `marker` to the (internal) reference
    /// `reference`.
    fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()>;

    /// `recv_marker_use`: set the scan position to the position `reference`'s
    /// marker recorded.
    fn receive_marker_use(&mut self, reference: Eterm) -> Result<()>;

    /// `recv_marker_clear`: release `reference`'s marker.
    fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()>;
}

/// The engine's **mutable root set** — the terms a collection must keep alive and
/// may relocate **in place**.
///
/// This is the hand-off [`ExecutionContext::alloc_words_rooted`] receives: an
/// allocation that may run the collector is handed the live roots *by mutable
/// slice*, and the context writes each moved term back through those slices, so a
/// collector can compact/move a term without the engine keeping any frame liveness
/// map (the same contract [`ExecutionContext::ensure_heap`] documents).
///
/// # The root-set contract (frozen)
///
/// The live roots of a running [`Machine`](crate::machine::Machine) are exactly:
///
/// * `x[0..live)` — the leading `live` X registers of [`RootSet::x`]; the trailing
///   registers are dead for the instruction in flight;
/// * **the whole** [`RootSet::stack`] (every Y slot of every live frame — there are
///   no frame liveness maps);
/// * **the whole** [`RootSet::fr`] (the float-register file, which at Milestone 1
///   holds `Eterm` **term slots**, so it can hold heap pointers like any other
///   register).
///
/// [`RootSet::live`] is only a **hint**: the engine hands over the whole x-register
/// file (a conservative over-approximation is always *safe* for a copying
/// collector — it can only keep a term alive that would otherwise die, never free a
/// live one), and [`RootSet::live_len`] clamps it to the slice length, so
/// [`RootSet::ALL_LIVE`] (`u32::MAX`) means "treat every register as live".
///
/// Nothing outside the root set is a root: a raw [`Eterm`] the engine is holding
/// only in a local variable, or a `*mut Eterm` from a previous
/// [`ExecutionContext::alloc_words`], is **not** preserved across a collection —
/// exactly the C `HAlloc`/`heap_top` convention, so a pointer returned by
/// `alloc_words` is invalidated by the next `ensure_heap`/collecting `alloc_words`
/// and the engine must re-read any live term from `x`/`stack`/`fr`.
#[derive(Debug)]
pub struct RootSet<'a> {
    /// The whole X-register file; `x[0..live_len()]` is declared live.
    pub x: &'a mut [Eterm],
    /// The whole Erlang stack: the Y slots of every live frame, oldest first.
    pub stack: &'a mut [Eterm],
    /// The whole float-register (FR) file (`Eterm` term slots at Milestone 1).
    pub fr: &'a mut [Eterm],
    /// How many leading `x` slots the caller declares live (a hint; clamped by
    /// [`RootSet::live_len`]). [`RootSet::ALL_LIVE`] means "all of them".
    pub live: u32,
}

impl<'a> RootSet<'a> {
    /// The value of [`RootSet::live`] that declares **every** X register live.
    pub const ALL_LIVE: u32 = u32::MAX;

    /// Build a root set over the three mutable slices.
    pub fn new(
        x: &'a mut [Eterm],
        stack: &'a mut [Eterm],
        fr: &'a mut [Eterm],
        live: u32,
    ) -> RootSet<'a> {
        RootSet { x, stack, fr, live }
    }

    /// The declared live X-register count, clamped to the actual file length.
    #[inline]
    pub fn live_len(&self) -> usize {
        (self.live as usize).min(self.x.len())
    }

    /// The live X registers (`x[0..live_len()]`).
    #[inline]
    pub fn live_x(&self) -> &[Eterm] {
        &self.x[..self.live_len()]
    }

    /// The live X registers, mutably (the slice a collector writes moved terms
    /// back through).
    #[inline]
    pub fn live_x_mut(&mut self) -> &mut [Eterm] {
        let live = self.live_len();
        &mut self.x[..live]
    }
}

/// One execution-trace event the engine reports to its host (contract A.8).
///
/// The engine observes exactly three event shapes from its own instruction
/// stream — the oracle's
/// `{trace, Pid, call, {M, F, Args}}`,
/// `{trace, Pid, return_from, {M, F, A}, Ret}` and
/// `{trace, Pid, return_to, {M, F, Arity}}` — and hands each one to the host
/// through [`ExecutionContext::trace_event`] **verbatim**: the host decides
/// *matching* (which `{M,F,A}` it cares about) and builds the message.
///
/// Every name is **borrowed** ([`Module::atom_name`](beam_loader::Module::atom_name)
/// of the running module, or the per-run import table for a `call_ext*`), and
/// `args` is a borrowed slice of the callee's `x(0..arity)` registers, so the
/// engine allocates nothing while reporting and the whole hook stays off the hot
/// path when [`ExecutionContext::trace_enabled`] is `false`.
#[derive(Clone, Copy, Debug)]
pub enum TraceEvent<'a> {
    /// (a) a function was entered (`call`).
    ///
    /// Oracle shape: `{trace, Pid, call, {M, F, Args}}`. The oracle's third tuple
    /// element is the **argument list** (the `arity` trace flag switches it to the
    /// bare arity), so the host builds it from `args`; an arity-0 call carries an
    /// empty `args`.
    Call {
        /// The entered module's name.
        module: &'a str,
        /// The entered function's name.
        function: &'a str,
        /// The entered function's arity (`args.len()`).
        arity: u32,
        /// The call's arguments (`x(0..arity)`), in order.
        args: &'a [Eterm],
    },
    /// (b) a call returned to its caller (`return_from`) — the **returning**
    /// function.
    ///
    /// Oracle shape: `{trace, Pid, return_from, {M, F, A}, Ret}`. Note that
    /// `return_from` is **not** a settable `erlang:trace/3` flag in the oracle: it
    /// is armed per function by a `{return_trace}` match-spec action, so whether
    /// this event means anything for a given function is the host's matching
    /// decision.
    ReturnFrom {
        /// The module of the function that returned.
        module: &'a str,
        /// The name of the function that returned.
        function: &'a str,
        /// Its arity.
        arity: u32,
        /// The returned value (`x(0)` at the `return`).
        value: Eterm,
    },
    /// (c) a return to the caller (`return_to`) — the function control returns
    /// **to**.
    ///
    /// Oracle shape: `{trace, Pid, return_to, {M, F, Arity}}` — always the MFA of
    /// the function at the return address (i.e. the caller), never any arguments.
    /// A return whose caller is outside the running module (the oracle's
    /// `return_to undefined`) is **not** reported: the engine can only name a
    /// module-local caller.
    ReturnTo {
        /// The module of the function control returns to.
        module: &'a str,
        /// The name of the function control returns to.
        function: &'a str,
        /// Its arity.
        arity: u32,
    },
}

/// Everything the engine needs from the runtime (BIF dispatch, heap/GC, limits).
pub trait ExecutionContext {
    /// Dispatch `module:function/arity` with `args`, returning the result term.
    ///
    /// Failure is return-based: an `Err` (a `BadArith`/`BadArg`/`Raise`/…) that the
    /// engine turns into a guard branch or a raise (it never panics).
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm>;

    /// Ensure `heap_need` heap words are available (for `test_heap`/
    /// `allocate_heap`).
    ///
    /// The live root set is `x_regs[0..live)` plus the whole `stack`. A real context
    /// (`beam_rt`, Stages 4/5) runs the collector here and may **move terms**,
    /// updating `x_regs` and `stack` in place — there are no frame liveness maps.
    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()>;

    /// Heap words currently available (introspection). Default: unbounded.
    fn heap_available(&self) -> usize {
        usize::MAX
    }

    /// Allocate `n` **uninitialized** heap words and return a pointer to them.
    ///
    /// The caller (`crate::terms`) may write boxed terms into the returned block
    /// and read them back. The pointer is valid **until the next
    /// [`ensure_heap`](ExecutionContext::ensure_heap) call / GC**, which may move
    /// or reclaim the block; it must never be retained across such a call. This
    /// mirrors the C heap-pointer convention (`HAlloc`/`heap_top`): the heap is
    /// owned by the context and the engine only borrows raw storage.
    ///
    /// The default body is a typed [`crate::error::unsupported`]: a context with
    /// no exposed heap (only `call_bif`/`ensure_heap`) cannot serve raw words.
    /// [`MinimalContext`] and the production `beam_rt` context override it.
    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        let _ = n;
        Err(crate::error::unsupported("alloc_words"))
    }

    /// Allocate `n` uninitialized heap words **with the live root set handed in**,
    /// so the context may run the collector (and move terms) as part of the
    /// allocation.
    ///
    /// [`alloc_words`](ExecutionContext::alloc_words) has no admission point and no
    /// root-set access, so a context whose heap must be collected cannot collect
    /// when the engine builds a term: every `crate::terms::make_*` goes through
    /// `alloc_words`, and there is nowhere to hand the roots over. This method is
    /// that missing admission point: it takes the [`RootSet`] (the X registers, the
    /// whole Erlang stack and the whole FR file — see the type docs for the frozen
    /// contract) and returns a pointer to `n` **uninitialized** words under exactly
    /// [`alloc_words`](ExecutionContext::alloc_words)'s stability rule (valid until
    /// the next `ensure_heap`/collecting allocation).
    ///
    /// The default body is [`alloc_words`](ExecutionContext::alloc_words) with the
    /// roots discarded, so **every existing implementor keeps working unchanged**
    /// (and byte-identically): a host with a real collector overrides this and runs
    /// its collector over `roots` first, writing any moved term back through
    /// `roots.x`/`roots.stack`/`roots.fr`.
    ///
    /// The engine reaches this method through
    /// [`Machine::alloc_rooted`](crate::machine::Machine::alloc_rooted), which is
    /// where the machine's own slices are handed over.
    fn alloc_words_rooted(&mut self, n: usize, roots: RootSet<'_>) -> Result<*mut Eterm> {
        let _ = roots;
        self.alloc_words(n)
    }
    /// Dispatch the external-function / BIF application `module:function/arity`.
    ///
    /// This is the general `apply`/`call_ext` hook; the default delegates to
    /// [`call_bif`](ExecutionContext::call_bif) (the BIF dispatch path), so a
    /// context that only implements the BIF surface needs no extra work.
    fn apply(&mut self, module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
        self.call_bif(module, function, arity, args)
    }

    /// Apply a fun/closure value to `args` (the `call_fun`/`apply` path).
    ///
    /// A host with a module/closure registry overrides this to look the fun's
    /// target up and run it. The default body is a typed
    /// [`crate::error::unsupported`]: the engine keeps no closure registry, so a
    /// context that does not override this cannot apply funs.
    ///
    /// # Required x-register layout (frozen)
    ///
    /// The engine passes the **call arguments** in `args` (already read from
    /// `x(0..arity)`); the context must enter the closure's target with
    /// `x(0..arity)` = those call arguments, and then `x(arity..)` = the
    /// closure's **free variables** — the C `call_fun` order
    /// (`reg[i] = args[i]` for `i < arity`, then `reg[i + arity] = funp->env[i]`,
    /// `../../erts/emulator/beam/beam_common.c:1631-1633`).
    ///
    /// Entering with the captures **before** the arguments (`free ++ args`) is a
    /// **silent** wrong-result / infinite-loop bug: the target reads `x(k)` for
    /// `k < arity` and gets a capture instead of an argument. A closure with
    /// **zero** captures cannot detect this (both layouts coincide), so the bug is
    /// invisible to zero-capture funs and only surfaces with a genuinely capturing
    /// closure.
    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        let _ = (fun, args);
        Err(crate::error::unsupported("apply_fun"))
    }

    /// `send`: deliver `message` to the pid/port term `to` (the `send` opcode).
    ///
    /// The default body is a typed [`crate::error::unsupported`] — a context with
    /// no mailbox cannot send. On `Ok` the `send` opcode stores the **message
    /// itself** in `x(0)` (C's `erl_send` returns the message, and
    /// `Pid ! Msg` evaluates to `Msg`); on `Err` the `send` opcode has no fail
    /// label, so the family raises it through the catch-aware path.
    fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()> {
        let _ = (to, message);
        Err(crate::error::unsupported("send_message"))
    }

    /// `loop_rec`: the candidate at the current mailbox scan position, `Ok(None)`
    /// when the scan/mailbox is exhausted.
    ///
    /// The default body is a typed [`crate::error::unsupported`]. The engine does
    /// **not** match the candidate against the receive's patterns — the compiled
    /// match tests that follow `loop_rec` do that; the family only moves the
    /// candidate into the instruction's destination register.
    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        Err(crate::error::unsupported("receive_next"))
    }

    /// `loop_rec_end`: advance the scan position past the current candidate.
    ///
    /// The default body is a typed [`crate::error::unsupported`].
    fn receive_skip(&mut self) -> Result<()> {
        Err(crate::error::unsupported("receive_skip"))
    }

    /// `remove_message`: unlink the currently-matched candidate and reposition the
    /// scan at the first message.
    ///
    /// The default body is a typed [`crate::error::unsupported`].
    fn receive_remove(&mut self) -> Result<()> {
        Err(crate::error::unsupported("receive_remove"))
    }

    /// `timeout`: reset the scan position to the first message.
    ///
    /// The default body is a typed [`crate::error::unsupported`].
    fn receive_reset(&mut self) -> Result<()> {
        Err(crate::error::unsupported("receive_reset"))
    }

    /// `recv_marker_reserve`: an opaque marker term for the current scan position.
    ///
    /// The returned term is **opaque** to the engine (it is stored in a register
    /// and only ever handed back through
    /// [`receive_marker_bind`](ExecutionContext::receive_marker_bind)); the engine
    /// never inspects, compares or hashes it. The default body is a typed
    /// [`crate::error::unsupported`].
    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        Err(crate::error::unsupported("receive_marker_reserve"))
    }

    /// `recv_marker_bind`: bind the opaque `marker` to the (internal) reference
    /// `reference`.
    ///
    /// The engine passes both terms through verbatim and never inspects either.
    /// The default body is a typed [`crate::error::unsupported`].
    fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()> {
        let _ = (marker, reference);
        Err(crate::error::unsupported("receive_marker_bind"))
    }

    /// `recv_marker_use`: set the scan position to the position `reference`'s
    /// marker recorded.
    ///
    /// The default body is a typed [`crate::error::unsupported`].
    fn receive_marker_use(&mut self, reference: Eterm) -> Result<()> {
        let _ = reference;
        Err(crate::error::unsupported("receive_marker_use"))
    }

    /// `recv_marker_clear`: release `reference`'s marker.
    ///
    /// The default body is a typed [`crate::error::unsupported`].
    fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()> {
        let _ = reference;
        Err(crate::error::unsupported("receive_marker_clear"))
    }

    /// Resolve a cross-module call into a module + entry the **same machine** can
    /// execute (the cross-module call seam).
    ///
    /// A `call_ext*` is otherwise dispatched **synchronously**: the engine hands
    /// `module:function/arity` to [`call_bif`](ExecutionContext::call_bif), and a
    /// host that runs the callee on a *fresh* machine cannot let a `receive`/`wait`
    /// inside that callee suspend the calling process — the block surfaces as the
    /// typed [`crate::error::blocked`] at the call site.
    ///
    /// A host that wants a cross-module callee to run **in the calling machine**
    /// (so its `receive` suspends *this* process and its `return`/raise flows back
    /// into the caller's frames) overrides this method and returns
    /// `Some((callee_module, entry_label))`:
    ///
    /// * `callee_module` is the host-owned handle of the module to enter. It is
    ///   kept alive by the engine for as long as a suspended frame can return into
    ///   it, so the host must hand over a **shared** handle (an
    ///   `Arc<Module>` it already owns) — the same `Arc` it loaded the module with.
    /// * `entry_label` is the **export's entry label** — the label *after*
    ///   `func_info`, i.e. `callee_module.export(function, arity)`'s `Label`, the
    ///   exact label [`Machine::run_export`](crate::Machine::run_export) enters a
    ///   called export at. The engine resolves it against the callee's own label
    ///   map and enters it with `x(0..arity)` = the call arguments (they are
    ///   already there: a `call_ext` reads its arguments from `x(0..arity)`).
    ///
    /// The default body returns `Ok(None)` — "I cannot run this callee in your
    /// machine" — which keeps the existing synchronous [`call_bif`] path as the
    /// fallback, so every existing host is unchanged.
    ///
    /// [`call_bif`]: ExecutionContext::call_bif
    fn resolve_external_call(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
    ) -> Result<Option<(Arc<Module>, Label)>> {
        let _ = (module, function, arity);
        Ok(None)
    }

    /// Cheap tracing gate: does the host want `call`/`return_from`/`return_to`
    /// execution-trace events reported for the process currently running?
    ///
    /// Consulted **once per run** — the driver that executes instructions latches the
    /// answer into the `Machine::trace_on` flag and the call/return arms then
    /// read that plain `bool`, so an untraced run pays one `bool` load per call/return
    /// instruction rather than a vtable call, and the default `false` builds no event
    /// data. A run is one entry plus its `resume`s: a host that flips the answer
    /// **mid-run** sees it take effect at the process's next run entry/resume, not at
    /// the next instruction. This is the engine's side of the oracle's function-trace
    /// breakpoints (contract A.8): a host that turns tracing on arms its own
    /// per-`{M,F,A}` patterns and decides matching in
    /// [`trace_event`](ExecutionContext::trace_event).
    ///
    /// The gate is **not** the oracle's `erlang:trace(Pid, …, [call, return_to])`
    /// flag set itself — it is the single cheap bit the engine consults before it
    /// reports anything, so a production context can answer it from a per-process
    /// trace state without the engine having to know about trace flags.
    #[inline]
    fn trace_enabled(&self) -> bool {
        false
    }

    /// Report one execution-trace event (contract A.8).
    ///
    /// Only called when [`trace_enabled`](ExecutionContext::trace_enabled) returned
    /// `true`. The host decides **matching** (e.g. a `{M,F,A}` pattern table, the
    /// way the oracle arms a function with `erlang:trace_pattern/3`) and emits
    /// nothing for an unmatched function; the engine reports **every** event it
    /// sees while tracing is on. The default body is a **no-op** — deliberately not
    /// a typed error: a host that does not trace simply ignores the hook.
    ///
    /// Reporting never changes a run's observable behaviour. An event whose names
    /// cannot be resolved against the running module is **skipped**, never raised,
    /// and the caller's registers are only read, so a traced run produces the same
    /// result, raise or suspension as an untraced one.
    fn trace_event(&mut self, event: TraceEvent<'_>) {
        let _ = event;
    }
}

/// A BIF hook: maps `(module, function, arity, args)` to a result.
pub type BifHook = Box<dyn FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm>>;

/// An apply hook: computes the result of applying a fun/closure value to `args`.
///
/// This is the pluggable body of [`ExecutionContext::apply_fun`]: the `call_fun`/
/// `call_fun2` opcodes reach the host through it. A host that keeps a closure
/// registry (or, as in the tests, knows the closures a module builds) overrides it;
/// [`MinimalContext::new`] leaves it unset, so `apply_fun` stays a typed
/// `unsupported`.
pub type ApplyHook = Box<dyn FnMut(Eterm, &[Eterm]) -> Result<Eterm>>;

/// An external-call resolver: the pluggable body of
/// [`ExecutionContext::resolve_external_call`].
///
/// Maps `(module, function, arity)` of a `call_ext*` to the callee module handle
/// plus its export entry [`Label`], or `None` to keep the synchronous
/// [`call_bif`](ExecutionContext::call_bif) path. [`MinimalContext::new`] leaves it
/// unset, so `resolve_external_call` stays the trait's `Ok(None)` default.
pub type ExternalResolver =
    Box<dyn FnMut(&str, &str, u32) -> Result<Option<(Arc<Module>, Label)>>>;

/// Arena chunk size in `Eterm` words (64 KiB).
///
/// Large enough that the per-chunk `Box` handle (and the arena's own bookkeeping)
/// is amortised to nothing over the thousands of small allocations a term-building
/// run makes, yet small enough that a lightly-using context does not reserve 64 KiB
/// for one word. A request that does not fit the current chunk's tail starts a new
/// chunk (so no allocation straddles a boundary) and a request larger than a whole
/// chunk gets a dedicated chunk of its own.
const ARENA_CHUNK_WORDS: usize = 64 * 1024 / core::mem::size_of::<Eterm>();

/// A **chunked** pointer-stable arena: the backing store of
/// [`MinimalContext::alloc_words`](ExecutionContext::alloc_words).
///
/// Requests are served as sub-slices of a large fixed-size *chunk*, so an
/// allocation costs its **payload only** — the arena holds **one** `Box` handle per
/// chunk (a few thousand words), not one per allocation. (The previous shape boxed
/// every request, retaining a whole `Box<[Eterm]>` handle plus an allocator
/// header per allocation.)
///
/// # Pointer stability
///
/// The chunks are boxed and pushed into a `Vec` that only ever grows: the `Vec`'s
/// reallocation moves the *chunk handles*, never the chunk buffers, so a pointer
/// handed out by [`Arena::alloc`] stays valid (and unmoved) until the context is
/// dropped. A request **larger than a chunk** gets a dedicated chunk of its own,
/// marked fully consumed, so a later small request can never alias into it.
struct Arena {
    /// The chunks, oldest first. The last one is the allocation frontier.
    chunks: Vec<Box<[Eterm]>>,
    /// Words already handed out of the newest chunk.
    used: usize,
}

impl Arena {
    /// An empty arena (no chunk is allocated until the first request).
    fn new() -> Arena {
        Arena {
            chunks: Vec::new(),
            used: 0,
        }
    }

    /// Hand out `n` uninitialized words and return a stable pointer to them.
    fn alloc(&mut self, n: usize) -> *mut Eterm {
        // A request larger than a chunk gets its own chunk. `used` is set past its
        // end so a later small request cannot be served from — and alias into — the
        // bytes the caller is about to write.
        if n > ARENA_CHUNK_WORDS {
            let mut block = vec![Eterm::default(); n].into_boxed_slice();
            let ptr = block.as_mut_ptr();
            self.chunks.push(block);
            self.used = n;
            return ptr;
        }
        // Room in the current chunk? (`saturating_sub` keeps the check total: no
        // state can make `used` exceed a chunk's length.)
        let room = match self.chunks.last() {
            Some(chunk) => chunk.len().saturating_sub(self.used) >= n,
            None => false,
        };
        if !room {
            self.chunks
                .push(vec![Eterm::default(); ARENA_CHUNK_WORDS].into_boxed_slice());
            self.used = 0;
        }
        let chunk = self.chunks.last_mut().expect("a chunk was just ensured");
        // Safe indexing into the free tail: the pointer to its first word. A
        // zero-word request at the end of a full chunk yields a one-past-the-end
        // (non-null, aligned) pointer, exactly as documented.
        let ptr = chunk[self.used..].as_mut_ptr();
        self.used += n;
        ptr
    }
}

/// Minimal context: an unbounded heap plus a pluggable BIF hook.
///
/// Used by tests and embeddings. [`MinimalContext::ensure_heap`] is a no-op (the
/// heap is treated as unbounded), [`MinimalContext::new`] installs a default hook
/// that rejects every BIF with [`crate::error::unsupported`], and
/// [`MinimalContext::alloc_words`](ExecutionContext::alloc_words) hands out words
/// from a pointer-stable arena (see the field docs).
///
/// An **apply hook** ([`with_apply_hook`](MinimalContext::with_apply_hook) /
/// [`set_apply_hook`](MinimalContext::set_apply_hook)) may additionally be installed
/// to serve [`apply_fun`](ExecutionContext::apply_fun); without one it is a typed
/// `unsupported`.
///
/// A **mailbox** ([`with_mailbox`](MinimalContext::with_mailbox) /
/// [`set_mailbox`](MinimalContext::set_mailbox)) may be installed to serve the
/// nine receive-seam methods; without one each stays the trait's typed
/// `unsupported`.
pub struct MinimalContext {
    hook: BifHook,
    /// An optional fun-application hook (the [`ExecutionContext::apply_fun`] body).
    apply_hook: Option<ApplyHook>,
    /// An optional cross-module-call resolver (the
    /// [`ExecutionContext::resolve_external_call`] body).
    external_resolver: Option<ExternalResolver>,
    /// An optional mailbox (the body of the nine receive-seam methods).
    mailbox: Option<Box<dyn Mailbox>>,
    /// A **chunked**, pointer-stable arena backing
    /// [`ExecutionContext::alloc_words`].
    ///
    /// Each allocation is served as a sub-slice of a large fixed-size chunk (see
    /// [`Arena`]), so an allocation costs its payload only — one `Box` handle is
    /// retained **per chunk**, not per allocation. Chunks are boxed, so growing the
    /// arena moves the chunk handles but never a chunk's buffer: a pointer handed
    /// out by `alloc_words` stays valid for the lifetime of the context (in this
    /// context `ensure_heap` never moves anything). The chunks are never freed
    /// before the context is dropped, which is exactly the "valid until the next
    /// `ensure_heap`" stability contract the trait documents.
    arena: Arena,
}

impl MinimalContext {
    /// A context whose hook rejects every BIF with a typed unsupported error.
    pub fn new() -> MinimalContext {
        MinimalContext::with_bif_hook(Box::new(|module, function, _arity, _args| {
            Err(crate::error::unsupported(&format!(
                "bif {module}:{function}"
            )))
        }))
    }

    /// A context with the given BIF hook.
    pub fn with_bif_hook(hook: BifHook) -> MinimalContext {
        MinimalContext {
            hook,
            apply_hook: None,
            external_resolver: None,
            mailbox: None,
            arena: Arena::new(),
        }
    }

    /// A context with the given fun-application hook and the default (rejecting)
    /// BIF hook.
    ///
    /// Combine with a BIF hook via [`set_bif_hook`](MinimalContext::set_bif_hook).
    pub fn with_apply_hook(hook: ApplyHook) -> MinimalContext {
        let mut ctx = MinimalContext::new();
        ctx.apply_hook = Some(hook);
        ctx
    }

    /// A context with the given mailbox and the default (rejecting) BIF hook.
    ///
    /// Combine with a BIF hook via [`set_bif_hook`](MinimalContext::set_bif_hook).
    pub fn with_mailbox(mailbox: Box<dyn Mailbox>) -> MinimalContext {
        let mut ctx = MinimalContext::new();
        ctx.mailbox = Some(mailbox);
        ctx
    }

    /// Replace the BIF hook.
    pub fn set_bif_hook(&mut self, hook: BifHook) {
        self.hook = hook;
    }

    /// Install (or replace) the fun-application hook.
    pub fn set_apply_hook(&mut self, hook: ApplyHook) {
        self.apply_hook = Some(hook);
    }

    /// A context with the given cross-module-call resolver and the default
    /// (rejecting) BIF hook.
    ///
    /// Combine with a BIF hook via [`set_bif_hook`](MinimalContext::set_bif_hook).
    pub fn with_external_resolver(resolver: ExternalResolver) -> MinimalContext {
        let mut ctx = MinimalContext::new();
        ctx.external_resolver = Some(resolver);
        ctx
    }

    /// Install (or replace) the cross-module-call resolver.
    pub fn set_external_resolver(&mut self, resolver: ExternalResolver) {
        self.external_resolver = Some(resolver);
    }

    /// Install (or replace) the mailbox.
    pub fn set_mailbox(&mut self, mailbox: Box<dyn Mailbox>) {
        self.mailbox = Some(mailbox);
    }

    /// Forward to the installed mailbox, or return the receive seam's typed
    /// `unsupported` naming `method`.
    ///
    /// The single definition every receive-seam override below shares, so the
    /// "no mailbox installed" contract stays byte-identical to the trait defaults.
    fn mailbox_call<T>(
        &mut self,
        method: &str,
        call: impl FnOnce(&mut dyn Mailbox) -> Result<T>,
    ) -> Result<T> {
        match self.mailbox.as_mut() {
            Some(mailbox) => call(mailbox.as_mut()),
            None => Err(crate::error::unsupported(method)),
        }
    }
}

impl Default for MinimalContext {
    /// Same as [`MinimalContext::new`]: the rejecting default hook.
    fn default() -> MinimalContext {
        MinimalContext::new()
    }
}

impl ExecutionContext for MinimalContext {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        (self.hook)(module, function, arity, args)
    }

    fn ensure_heap(
        &mut self,
        _heap_need: u32,
        _live: u32,
        _x_regs: &mut [Eterm],
        _stack: &mut [Eterm],
    ) -> Result<()> {
        // Unbounded heap: nothing to do.
        Ok(())
    }

    fn heap_available(&self) -> usize {
        usize::MAX
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        // The chunked arena hands out a sub-slice of a fixed-size chunk (its words
        // are pre-initialised to NIL, so they are already the "uninitialized words"
        // the trait contracts); a zero-word request still yields a non-null,
        // well-aligned pointer. The pointer stays stable for the life of the
        // context because the chunks are boxed and never move.
        Ok(self.arena.alloc(n))
    }

    fn apply(&mut self, module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
        // No separate external-call machinery in the minimal context: the BIF hook
        // is the dispatch path.
        self.call_bif(module, function, arity, args)
    }

    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        // Without an installed apply hook there is no closure registry to consult,
        // so fun application stays the trait's documented typed `unsupported`.
        match self.apply_hook.as_mut() {
            Some(hook) => hook(fun, args),
            None => {
                let _ = (fun, args);
                Err(crate::error::unsupported("apply_fun"))
            }
        }
    }

    fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()> {
        self.mailbox_call("send_message", |mailbox| mailbox.send_message(to, message))
    }

    fn resolve_external_call(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
    ) -> Result<Option<(Arc<Module>, Label)>> {
        // Without an installed resolver there is no module registry, so the
        // synchronous `call_bif` path stays the dispatch route.
        match self.external_resolver.as_mut() {
            Some(resolver) => resolver(module, function, arity),
            None => {
                let _ = (module, function, arity);
                Ok(None)
            }
        }
    }

    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        self.mailbox_call("receive_next", |mailbox| mailbox.receive_next())
    }

    fn receive_skip(&mut self) -> Result<()> {
        self.mailbox_call("receive_skip", |mailbox| mailbox.receive_skip())
    }

    fn receive_remove(&mut self) -> Result<()> {
        self.mailbox_call("receive_remove", |mailbox| mailbox.receive_remove())
    }

    fn receive_reset(&mut self) -> Result<()> {
        self.mailbox_call("receive_reset", |mailbox| mailbox.receive_reset())
    }

    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        self.mailbox_call("receive_marker_reserve", |mailbox| {
            mailbox.receive_marker_reserve()
        })
    }

    fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()> {
        self.mailbox_call("receive_marker_bind", |mailbox| {
            mailbox.receive_marker_bind(marker, reference)
        })
    }

    fn receive_marker_use(&mut self, reference: Eterm) -> Result<()> {
        self.mailbox_call("receive_marker_use", |mailbox| {
            mailbox.receive_marker_use(reference)
        })
    }

    fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()> {
        self.mailbox_call("receive_marker_clear", |mailbox| {
            mailbox.receive_marker_clear(reference)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_hook_rejects_bifs() {
        let mut ctx = MinimalContext::new();
        assert!(ctx.call_bif("erlang", "'+'", 2, &[]).is_err());
    }

    #[test]
    fn custom_hook_dispatches() {
        let mut ctx = MinimalContext::with_bif_hook(Box::new(|m, f, a, _args| {
            assert_eq!((m, f, a), ("erlang", "'+'", 2));
            Ok(Eterm::nil())
        }));
        assert_eq!(ctx.call_bif("erlang", "'+'", 2, &[]).unwrap(), Eterm::nil());
    }

    #[test]
    fn heap_is_unbounded_and_ensure_is_a_noop() {
        let mut ctx = MinimalContext::default();
        assert_eq!(ctx.heap_available(), usize::MAX);
        let mut x = [Eterm::nil()];
        let mut stack = [Eterm::nil()];
        assert!(ctx.ensure_heap(1_000_000, 1, &mut x, &mut stack).is_ok());
    }

    #[test]
    fn set_bif_hook_replaces_the_hook() {
        let mut ctx = MinimalContext::new();
        ctx.set_bif_hook(Box::new(|_m, _f, _a, _args| {
            Err(beam_common::BeamError::BadArg)
        }));
        assert!(ctx.call_bif("x", "y", 0, &[]).is_err());
    }

    /// A context that implements only the two required trait methods, so the
    /// defaulted `alloc_words`/`apply` bodies are exercised.
    struct OnlyRequired;

    impl ExecutionContext for OnlyRequired {
        fn call_bif(
            &mut self,
            _module: &str,
            _function: &str,
            _arity: u32,
            _args: &[Eterm],
        ) -> Result<Eterm> {
            Err(beam_common::BeamError::BadArg)
        }

        fn ensure_heap(
            &mut self,
            _heap_need: u32,
            _live: u32,
            _x_regs: &mut [Eterm],
            _stack: &mut [Eterm],
        ) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn default_alloc_words_is_a_typed_unsupported_error() {
        // A context with no exposed heap cannot serve raw words.
        assert!(OnlyRequired.alloc_words(8).is_err());
    }

    #[test]
    fn default_apply_forwards_to_call_bif() {
        // The default `apply` delegates to `call_bif`; here `call_bif` always
        // fails, and the failure must surface identically through `apply`.
        assert!(OnlyRequired.apply("erlang", "self", 0, &[]).is_err());
    }

    #[test]
    fn minimal_context_alloc_words_is_ok_and_non_null() {
        let mut ctx = MinimalContext::new();
        let ptr = ctx.alloc_words(4).expect("arena allocates");
        assert!(!ptr.is_null(), "a non-empty block has a non-null pointer");
    }

    #[test]
    fn minimal_context_zero_word_allocation_is_non_null() {
        // Documented: a zero-word request may return a dangling, but non-null,
        // well-aligned pointer.
        let mut ctx = MinimalContext::new();
        let ptr = ctx.alloc_words(0).expect("arena allocates an empty block");
        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % core::mem::align_of::<Eterm>(), 0);
    }

    #[test]
    fn minimal_context_allocations_do_not_alias() {
        let mut ctx = MinimalContext::new();
        let a = ctx.alloc_words(4).expect("first alloc");
        let b = ctx.alloc_words(4).expect("second alloc");
        assert!(!a.is_null() && !b.is_null());
        assert_ne!(a, b, "distinct allocations own distinct storage");
    }

    #[test]
    fn minimal_context_apply_forwards_to_the_hook() {
        let mut ctx = MinimalContext::with_bif_hook(Box::new(|m, f, a, _args| {
            assert_eq!((m, f, a), ("lists", "map", 2));
            Ok(Eterm::nil())
        }));
        assert_eq!(ctx.apply("lists", "map", 2, &[]).unwrap(), Eterm::nil());
    }

    // ---- the execution-trace seam (contract A.8) ---------------------------

    /// A logged [`TraceEvent`] in owned, comparable form (the real event borrows
    /// the running module and a register slice).
    #[derive(Debug, PartialEq, Eq)]
    enum Owned {
        Call(String, String, u32, usize),
        ReturnFrom(String, String, u32),
        ReturnTo(String, String, u32),
    }

    /// A context overriding only the two trace hooks, so the gate/report split is
    /// observable without any heap or mailbox machinery.
    struct TraceHost {
        enabled: bool,
        events: std::rc::Rc<std::cell::RefCell<Vec<Owned>>>,
    }

    impl ExecutionContext for TraceHost {
        fn call_bif(
            &mut self,
            _module: &str,
            _function: &str,
            _arity: u32,
            _args: &[Eterm],
        ) -> Result<Eterm> {
            Err(beam_common::BeamError::BadArg)
        }

        fn ensure_heap(
            &mut self,
            _heap_need: u32,
            _live: u32,
            _x_regs: &mut [Eterm],
            _stack: &mut [Eterm],
        ) -> Result<()> {
            Ok(())
        }

        fn trace_enabled(&self) -> bool {
            self.enabled
        }

        fn trace_event(&mut self, event: TraceEvent<'_>) {
            // The test double honours its own gate: a host whose `trace_enabled` is
            // off records nothing even though it implements the report hook. The engine
            // also gates (it latches the flag once per run), so a real host is only
            // asked to report while tracing is on; this guard pins that contract on the
            // host side too, which is what `trace_gate_is_independent_of_reporting`
            // asserts.
            if !self.enabled {
                return;
            }
            self.events.borrow_mut().push(match event {
                TraceEvent::Call {
                    module,
                    function,
                    arity,
                    args,
                } => Owned::Call(module.into(), function.into(), arity, args.len()),
                TraceEvent::ReturnFrom {
                    module,
                    function,
                    arity,
                    ..
                } => Owned::ReturnFrom(module.into(), function.into(), arity),
                TraceEvent::ReturnTo {
                    module,
                    function,
                    arity,
                } => Owned::ReturnTo(module.into(), function.into(), arity),
            });
        }
    }

    /// Compiles only because [`TraceEvent`] is `Copy` — the engine hands the host a
    /// value and keeps reporting, so the event must never be moved out of the
    /// reporter.
    fn event_is_copy(event: TraceEvent<'_>) -> (TraceEvent<'_>, TraceEvent<'_>) {
        (event, event)
    }

    #[test]
    fn trace_is_off_and_a_noop_by_default() {
        let mut ctx = MinimalContext::new();
        assert!(!ctx.trace_enabled(), "tracing is off by default");
        // The default `trace_event` is a no-op: it accepts every event and never
        // panics or raises.
        let args = [Eterm::nil()];
        ctx.trace_event(TraceEvent::Call {
            module: "m",
            function: "f",
            arity: 1,
            args: &args,
        });
        ctx.trace_event(TraceEvent::ReturnFrom {
            module: "m",
            function: "f",
            arity: 1,
            value: Eterm::nil(),
        });
        ctx.trace_event(TraceEvent::ReturnTo {
            module: "m",
            function: "f",
            arity: 1,
        });
        // A context implementing only the two required methods inherits the same
        // defaults, so no existing host pays for the hook.
        assert!(!OnlyRequired.trace_enabled());
    }

    #[test]
    fn trace_event_carries_the_borrowed_names_arity_and_args() {
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut ctx = TraceHost {
            enabled: true,
            events: events.clone(),
        };
        assert!(ctx.trace_enabled());
        let args = [Eterm::nil(), Eterm::nil()];
        // The borrowed names, the arity and the argument count all survive the
        // hand-off (the host, not the engine, decides matching).
        let (first, second) = event_is_copy(TraceEvent::Call {
            module: "lists",
            function: "reverse",
            arity: 2,
            args: &args,
        });
        ctx.trace_event(first);
        ctx.trace_event(second);
        ctx.trace_event(TraceEvent::ReturnFrom {
            module: "lists",
            function: "reverse",
            arity: 2,
            value: Eterm::nil(),
        });
        ctx.trace_event(TraceEvent::ReturnTo {
            module: "lists",
            function: "map",
            arity: 3,
        });
        assert_eq!(
            *events.borrow(),
            vec![
                Owned::Call("lists".into(), "reverse".into(), 2, 2),
                Owned::Call("lists".into(), "reverse".into(), 2, 2),
                Owned::ReturnFrom("lists".into(), "reverse".into(), 2),
                Owned::ReturnTo("lists".into(), "map".into(), 3),
            ]
        );
    }

    #[test]
    fn trace_gate_is_independent_of_reporting() {
        // The gate alone decides whether the host is told anything: with it off the
        // host records nothing even though it implements `trace_event`.
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut ctx = TraceHost {
            enabled: false,
            events: events.clone(),
        };
        assert!(!ctx.trace_enabled());
        ctx.trace_event(TraceEvent::ReturnTo {
            module: "m",
            function: "f",
            arity: 0,
        });
        assert!(
            events.borrow().is_empty(),
            "the gate, not the report, is what an untraced run relies on"
        );
    }

    // ---- the mailbox / receive seam ---------------------------------------
    /// The shared call log of a [`LoggingMailbox`].
    type Log = std::rc::Rc<std::cell::RefCell<Vec<String>>>;

    /// A mailbox that records every seam call as a string (so forwarding through
    /// the context's boxed trait object is observable) and answers with fixed
    /// results.
    struct LoggingMailbox {
        log: Log,
        next: Option<Eterm>,
    }

    impl LoggingMailbox {
        /// A mailbox logging to a fresh log, with no candidate available.
        fn new() -> (Log, LoggingMailbox) {
            let log: Log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            (std::rc::Rc::clone(&log), LoggingMailbox { log, next: None })
        }

        /// Record one call (`format!`ed, so argument passthrough is visible too).
        fn record(&mut self, call: String) {
            self.log.borrow_mut().push(call);
        }
    }

    impl Mailbox for LoggingMailbox {
        fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()> {
            self.record(format!("send_message {to:?} {message:?}"));
            Ok(())
        }

        fn receive_next(&mut self) -> Result<Option<Eterm>> {
            self.record("receive_next".to_string());
            Ok(self.next)
        }

        fn receive_skip(&mut self) -> Result<()> {
            self.record("receive_skip".to_string());
            Ok(())
        }

        fn receive_remove(&mut self) -> Result<()> {
            self.record("receive_remove".to_string());
            Ok(())
        }

        fn receive_reset(&mut self) -> Result<()> {
            self.record("receive_reset".to_string());
            Ok(())
        }

        fn receive_marker_reserve(&mut self) -> Result<Eterm> {
            self.record("receive_marker_reserve".to_string());
            Ok(Eterm::from_raw(0xABCD))
        }

        fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()> {
            self.record(format!("receive_marker_bind {marker:?} {reference:?}"));
            Ok(())
        }

        fn receive_marker_use(&mut self, reference: Eterm) -> Result<()> {
            self.record(format!("receive_marker_use {reference:?}"));
            Ok(())
        }

        fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()> {
            self.record(format!("receive_marker_clear {reference:?}"));
            Ok(())
        }
    }

    #[test]
    fn the_receive_seam_defaults_are_typed_unsupported_errors() {
        // A context that overrides nothing (the `OnlyRequired` stub) gets all nine
        // receive-seam methods as the trait's documented typed `unsupported`, so
        // every existing implementor keeps compiling and behaves predictably.
        let mut ctx = OnlyRequired;
        let e = Eterm::from_raw(7);
        assert!(matches!(
            ctx.send_message(e, e).unwrap_err(),
            beam_common::BeamError::Raise(_)
        ));
        assert!(ctx.receive_next().is_err());
        assert!(ctx.receive_skip().is_err());
        assert!(ctx.receive_remove().is_err());
        assert!(ctx.receive_reset().is_err());
        assert!(ctx.receive_marker_reserve().is_err());
        assert!(ctx.receive_marker_bind(e, e).is_err());
        assert!(ctx.receive_marker_use(e).is_err());
        assert!(ctx.receive_marker_clear(e).is_err());
    }

    #[test]
    fn minimal_context_without_a_mailbox_keeps_the_typed_unsupported_defaults() {
        // The nine methods name themselves in the reason, exactly as the defaults do.
        let mut ctx = MinimalContext::new();
        let e = Eterm::from_raw(7);
        for (reason, result) in [
            ("send_message", ctx.send_message(e, e).err()),
            ("receive_next", ctx.receive_next().err()),
            ("receive_skip", ctx.receive_skip().err()),
            ("receive_remove", ctx.receive_remove().err()),
            ("receive_reset", ctx.receive_reset().err()),
            ("receive_marker_reserve", ctx.receive_marker_reserve().err()),
            ("receive_marker_bind", ctx.receive_marker_bind(e, e).err()),
            ("receive_marker_use", ctx.receive_marker_use(e).err()),
            ("receive_marker_clear", ctx.receive_marker_clear(e).err()),
        ] {
            match result.expect("no mailbox ⇒ typed error") {
                beam_common::BeamError::Raise(raise) => assert_eq!(
                    raise.reason.as_atom(),
                    Some(format!("unsupported_opcode: {reason}").as_str()),
                    "the error names the seam method"
                ),
                other => panic!("expected a typed raise for {reason}, got {other:?}"),
            }
        }
    }

    #[test]
    fn minimal_context_forwards_every_receive_seam_call_to_the_installed_mailbox() {
        let (log, mailbox) = LoggingMailbox::new();
        let mut ctx = MinimalContext::with_mailbox(Box::new(mailbox));
        let e = Eterm::from_raw(9);

        ctx.send_message(e, e).unwrap();
        assert_eq!(ctx.receive_next().unwrap(), None);
        ctx.receive_skip().unwrap();
        ctx.receive_remove().unwrap();
        ctx.receive_reset().unwrap();
        assert_eq!(
            ctx.receive_marker_reserve().unwrap(),
            Eterm::from_raw(0xABCD)
        );
        ctx.receive_marker_bind(Eterm::from_raw(1), Eterm::from_raw(2))
            .unwrap();
        ctx.receive_marker_use(Eterm::from_raw(3)).unwrap();
        ctx.receive_marker_clear(Eterm::from_raw(4)).unwrap();

        assert_eq!(
            *log.borrow(),
            vec![
                format!("send_message {e:?} {e:?}"),
                "receive_next".to_string(),
                "receive_skip".to_string(),
                "receive_remove".to_string(),
                "receive_reset".to_string(),
                "receive_marker_reserve".to_string(),
                format!(
                    "receive_marker_bind {:?} {:?}",
                    Eterm::from_raw(1),
                    Eterm::from_raw(2)
                ),
                format!("receive_marker_use {:?}", Eterm::from_raw(3)),
                format!("receive_marker_clear {:?}", Eterm::from_raw(4)),
            ]
        );
    }

    #[test]
    fn set_mailbox_replaces_the_previous_mailbox() {
        // The first mailbox yields no candidate; after `set_mailbox` the second one
        // yields a term — proof the replacement took effect.
        let (_, first) = LoggingMailbox::new();
        let (_, mut second) = LoggingMailbox::new();
        second.next = Some(Eterm::from_raw(0x42));
        let mut ctx = MinimalContext::with_mailbox(Box::new(first));
        assert_eq!(ctx.receive_next().unwrap(), None);
        ctx.set_mailbox(Box::new(second));
        assert_eq!(ctx.receive_next().unwrap(), Some(Eterm::from_raw(0x42)));
    }
}
