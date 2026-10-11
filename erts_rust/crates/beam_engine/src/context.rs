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
//!
//! # `unsafe`
//!
//! The crate root keeps `#![deny(unsafe_code)]`; this module's `MinimalContext`
//! heap ([`ContextHeap`]) is the crate's **raw `ReservedHeap` bump seam** — the one
//! place the test/embedding heap advances another crate's bump pointer over
//! uninitialised words — so it carries the module-local `#![allow(unsafe_code)]`
//! below. Every `unsafe` block states its `// SAFETY:` rationale, exactly as
//! [`crate::terms`] does for term construction.
#![allow(unsafe_code)]

use std::sync::Arc;

use beam_alloc::{GrowOutcome, ReservedHeap};
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

    /// Ensure `heap_need` heap words are available, with the engine's **whole live
    /// root set** handed in.
    ///
    /// This is the full-root twin of
    /// [`ensure_heap`](ExecutionContext::ensure_heap): where `ensure_heap` receives
    /// only the X registers and the Erlang stack, this method receives the whole
    /// [`RootSet`] — the X-register file, the whole Erlang stack **and the whole FR
    /// file**. The FR file holds `Eterm` **term slots** at Milestone 1, so a float
    /// register can point into the process heap and must be a root; the narrow
    /// `ensure_heap` cannot see it, so a module containing any float-family
    /// instruction could never be collected over correctly.
    /// [`Machine::ensure_heap`](crate::machine::Machine::ensure_heap) — reached by
    /// `test_heap`/`allocate_heap` — calls **this** method, so a host whose heap must
    /// be collected at the primary admission point now sees every live root.
    ///
    /// The default body forwards to
    /// [`ensure_heap`](ExecutionContext::ensure_heap) with the FR file discarded, so
    /// **every existing implementor keeps working unchanged** (and byte-identically):
    /// a host with a real collector overrides this and runs its collector over the
    /// full `roots` first, writing any moved term back through
    /// `roots.x`/`roots.stack`/`roots.fr`.
    fn ensure_heap_full(&mut self, heap_need: u32, roots: RootSet<'_>) -> Result<()> {
        self.ensure_heap(heap_need, roots.live, roots.x, roots.stack)
    }

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
    /// A **zero-word** request (`n == 0`) asks for no storage at all: a host may
    /// answer `Ok` with a **dangling but non-null, well-aligned** pointer (never an
    /// error), so a caller that builds a zero-word shape does not have to special-case
    /// it. Nothing may be read or written through such a pointer.
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

    /// `send`: deliver `message` to the pid/port term `to`, handing over the
    /// **source off-heap table** the receiver-side copy needs to resolve any
    /// engine-built off-heap (refc) binary in `message`.
    ///
    /// `src_binaries` are the payload handles of the machine that built `message`
    /// (the in-flight `Machine::refc_binaries()` slice): a `bs_init_writable` /
    /// `bs_create_bin` result over `beam_term::refc::ERL_ONHEAP_BINARY_LIMIT` bytes
    /// is stored off the process heap, and the copy engine resolves such a term by
    /// its data pointer through the copy's **source** off-heap table. The default
    /// body forwards to [`send_message`](ExecutionContext::send_message) and
    /// discards `src_binaries`, so every existing implementor (including all test
    /// contexts and `MinimalContext`) compiles and behaves exactly as today --
    /// a host with no off-heap payloads sees the documented degradation (an
    /// engine-built off-heap binary in a message is refused by the receiver copy).
    fn send_message_with_off_heap(
        &mut self,
        to: Eterm,
        message: Eterm,
        src_binaries: &[beam_alloc::RefcBinary],
    ) -> Result<()> {
        let _ = src_binaries;
        self.send_message(to, message)
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

    /// Resolve a **closure application** into a lambda the **same machine** can
    /// execute — the in-machine *closure-apply* seam.
    ///
    /// A `call_fun`/`call_fun2` is otherwise dispatched through the
    /// **value-returning** [`apply_fun`](ExecutionContext::apply_fun): the host
    /// runs the closure and hands back its result, so a host that enters the
    /// closure on a *fresh* machine cannot let a `receive`/`wait` inside it
    /// suspend the *calling* process — the block surfaces as the typed
    /// [`crate::error::blocked`] at the call site (and a raise inside the closure
    /// escapes the caller's `try`/`catch`).
    ///
    /// A host that owns the closure's defining module overrides this method and
    /// returns `Some((callee_module, lambda_entry_label))` so the engine enters
    /// the lambda body **in this machine**, exactly as C's `call_fun`
    /// (`../../erts/emulator/beam/beam_common.c:1604`) dispatches to the fun
    /// body's code in the same process:
    ///
    /// * `callee_module` is the host-owned handle of the module that defines the
    ///   closure. It is kept alive by the engine for as long as a suspended frame
    ///   can return into it, so the host must hand over a **shared** handle (an
    ///   `Arc<Module>` it already owns) — the engine dedups module contexts by
    ///   `Arc::ptr_eq`.
    /// * `lambda_entry_label` is the lambda's **`FunT` entry label** — the label
    ///   [`Machine::run_label`](crate::Machine::run_label) enters the closure body
    ///   at. The engine resolves it against the callee's **own** label map, so an
    ///   unknown label is the typed [`crate::error::unknown_label`], never a jump
    ///   into the wrong module.
    ///
    /// The engine places the registers itself (C's `call_fun`): the call `args`
    /// are already in `x(0..arity)`, and the engine copies the closure's free
    /// variables into `x(arity..)` — which is why the host is handed the `args`
    /// (it reuses its existing `(uniq, index)` → `(module, label)` lookup and need
    /// not repeat the combined-argument ordering).
    ///
    /// The default body returns `Ok(None)` — "I cannot run this closure in your
    /// machine" — which keeps the value-returning
    /// [`apply_fun`](ExecutionContext::apply_fun) fallback (fun kinds with no local
    /// label: external `fun M:F/A` and the `-eval` evaluator's own closure), so
    /// every existing host is unchanged.
    ///
    /// [`apply_fun`]: ExecutionContext::apply_fun
    fn resolve_local_fun(
        &mut self,
        fun: Eterm,
        args: &[Eterm],
    ) -> Result<Option<(Arc<Module>, Label)>> {
        let _ = (fun, args);
        Ok(None)
    }

    /// The **per-instruction reduction hook**: charge `n` reductions for the
    /// instruction the engine has just executed.
    ///
    /// A BEAM process is pre-emptible because the runtime prices work in
    /// *reductions* and checks its budget at known points. The engine's **only**
    /// other charging point is [`ensure_heap`](ExecutionContext::ensure_heap)
    /// (reached by `test_heap`/`allocate_heap`), which a tight **non-allocating**
    /// loop — the one shape that can monopolise a scheduler — never touches. This
    /// hook closes that gap: [`Machine::drive`](crate::machine::Machine) calls it
    /// **once per interpreted instruction** with `n = 1`, after the instruction
    /// succeeded, so a host can price instructions rather than heap admissions.
    ///
    /// The default body is a **no-op** ([`Ok`]) — deliberately *not* a typed error:
    /// a host that does not pre-empt simply ignores the hook, so every existing
    /// implementor compiles and behaves byte-identically (it pays only the call).
    /// Returning `Err` propagates out of the run exactly like any other engine
    /// failure, so a host that has exhausted its budget returns **its own** typed
    /// exhaustion error (a host that pre-empts *by failing* must remember that the
    /// machine keeps its position: re-entering through
    /// [`Machine::resume_outcome`](crate::machine::Machine::resume_outcome) is what
    /// continues the process, not [`Machine::run_export`](crate::machine::Machine::run_export)).
    ///
    /// The engine also counts the same charge in
    /// [`Machine::reductions`](crate::machine::Machine::reductions) — the slice's
    /// instruction count, readable *after* the slice returns — so a host that
    /// prefers to poll rather than hook can; see that accessor for the tier caveat.
    fn reduce(&mut self, n: u32) -> Result<()> {
        let _ = n;
        Ok(())
    }

    /// Cheap reduction-hook gate: does [`reduce`](ExecutionContext::reduce) need to
    /// be called at all for the process currently running?
    ///
    /// [`Machine::drive`](crate::machine::Machine) charges **one reduction per
    /// interpreted instruction**, and that charge is also offered to the host
    /// through [`reduce`](ExecutionContext::reduce). The hook call is a
    /// `&mut dyn ExecutionContext` vtable crossing the interpreter cannot elide, so
    /// the driver latches this answer **once per run** into the
    /// `Machine::reduce_on` flag and then skips the hook call entirely on a run
    /// whose host declares it does not need it — the per-instruction hot path pays a
    /// single `bool` load instead.
    ///
    /// **The default is the CONSERVATIVE `true`**: every host keeps receiving every
    /// charge unless it explicitly opts out, so an implementor that *does* pre-empt
    /// by returning an error from `reduce` (e.g. a scheduler enforcing a time slice)
    /// is never silently skipped. A host may answer `false` **only** when its
    /// `reduce` is provably the default no-op (a test/embedding context), never when
    /// it uses the hook for pre-emption.
    ///
    /// `Machine::reductions` accounting is **unaffected**: the slice counter is
    /// still bumped on every interpreted instruction, so a host that polls
    /// [`Machine::reductions`](crate::machine::Machine::reductions) after the slice
    /// observes the exact same count either way; only the per-instruction hook *call*
    /// is skipped.
    ///
    /// Like the A.8 tracing gate, the answer is latched at a run/resume entry, so a
    /// host that flips it mid-run sees the change take effect at the process's next
    /// run entry (or [`Machine::resume_outcome`](crate::machine::Machine::resume_outcome)).
    #[inline]
    fn wants_reduce(&self) -> bool {
        true
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

/// A local-closure resolver: the pluggable body of
/// [`ExecutionContext::resolve_local_fun`].
///
/// Maps a fun/closure value plus its call `args` to the closure's **defining
/// module handle** and its lambda entry [`Label`], or `None` to keep the
/// value-returning [`apply_fun`](ExecutionContext::apply_fun) fallback.
/// [`MinimalContext::new`] leaves it unset, so `resolve_local_fun` stays the
/// trait's `Ok(None)` default.
pub type LocalFunResolver = Box<dyn FnMut(Eterm, &[Eterm]) -> Result<Option<(Arc<Module>, Label)>>>;

/// The first region's **reservation** in `Eterm` words (4 MiB).
///
/// A [`ContextHeap`] region reserves a large address range up front and commits only
/// a slice of it ([`HEAP_COMMIT_WORDS`]), so growth is normally a **commit**: the
/// region's bump pointer does not move and every pointer it has already handed out
/// stays valid. The reservation itself is *address space only* — `beam_alloc`'s
/// carriers come from anonymous private `mmap` (via `mseg`), so reserved pages fault
/// lazily and a large reservation commits no RSS. It bounds how far one region can
/// grow *in place* before a fresh region is needed.
const HEAP_RESERVE_WORDS: usize = 4 * 1024 * 1024 / core::mem::size_of::<Eterm>();

/// A region's **initial committed** slice in `Eterm` words (64 KiB).
///
/// A lightly-using context commits only this much; the rest of the reservation is
/// committed lazily (`try_grow`), one request at a time, as the context asks for more.
const HEAP_COMMIT_WORDS: usize = 64 * 1024 / core::mem::size_of::<Eterm>();

/// The pointer-stable heap backing
/// [`MinimalContext::alloc_words`](ExecutionContext::alloc_words): a list of
/// [`ReservedHeap`] **regions** that only ever grows.
///
/// # Why regions instead of one growable heap
///
/// A term the engine has built lives at a raw address the engine already holds — in
/// an X-register, on the Erlang stack, inside a structure in flight — so
/// `alloc_words` **may not move** a word it has handed out. That is the same
/// "valid until the next `ensure_heap`" stability contract the trait documents (in
/// this context `ensure_heap` never moves anything, so a pointer is valid until the
/// context is dropped).
///
/// A single [`ReservedHeap`] cannot satisfy that once its reservation is exhausted:
/// its only way forward is [`ReservedHeap::relocate`], which **moves the whole live
/// block** and returns only the constant pointer delta. That would invalidate every
/// pointer the context has already handed out, and it could not fix the *interior*
/// pointers the heap's own words hold (a cons cell's `cdr`, a tuple's elements, a
/// `BinRef`'s payload pointer) — telling a pointer word from raw payload bytes needs
/// term-structure knowledge this heap seam deliberately does not have. So
/// `relocate` is **never** called here.
///
/// Instead the heap is a `Vec` of reservations that only ever grows: an exhausted
/// region is left exactly as it is (its words untouched, every pointer into it still
/// valid) and a fresh, larger region becomes the allocation frontier. That is the
/// identical "stable until dropped" shape the arena has always had, with
/// [`ReservedHeap`] supplying the bump pointer, the in-place commit growth and a
/// typed, return-based failure.
///
/// # Growth
///
/// An allocation is served from the **frontier** region (the newest one). When the
/// request does not fit the frontier's committed slice, the region is asked to raise
/// its committed limit **in place** ([`GrowOutcome::GrewInPlace`]): the bump pointer
/// did not move, so the retry lands on the same region. Only when the whole
/// *reservation* is exhausted ([`GrowOutcome::MoveRequired`]) is a fresh, doubled
/// region created and pushed; every earlier region stays alive and unmoved for the
/// context's lifetime.
struct ContextHeap {
    /// Every region ever handed out, oldest first. The **last one is the allocation
    /// frontier** (`regions.len() - 1` is the current-region index); the `Vec` only
    /// ever grows, so no region is freed — and no word moves — before the context is
    /// dropped.
    regions: Vec<ReservedHeap>,
    /// The first region's reservation geometry, and the floor every later region's
    /// reservation doubles up from.
    reserve_words: usize,
    /// The words committed up front in a region.
    commit_words: usize,
}

impl ContextHeap {
    /// An empty heap: no region is reserved until the first request, so a context
    /// that never allocates costs no address space and `MinimalContext`'s
    /// constructors can stay infallible.
    fn new() -> ContextHeap {
        ContextHeap {
            regions: Vec::new(),
            reserve_words: HEAP_RESERVE_WORDS,
            commit_words: HEAP_COMMIT_WORDS,
        }
    }

    /// A heap with explicit first-region geometry, so the growth paths can be driven
    /// deterministically with small, exact numbers. Test-only: production contexts
    /// always take [`ContextHeap::new`]'s geometry.
    #[cfg(test)]
    fn with_geometry(reserve_words: usize, commit_words: usize) -> ContextHeap {
        ContextHeap {
            regions: Vec::new(),
            reserve_words,
            commit_words,
        }
    }

    /// Hand out `n` contiguous, uninitialised words and return a pointer that stays
    /// valid and **unmoved** until this heap is dropped.
    ///
    /// A zero-word request hands out nothing: it yields a dangling but non-null,
    /// well-aligned pointer, exactly as
    /// [`ExecutionContext::alloc_words`] documents, and reserves no region.
    fn alloc(&mut self, n: usize) -> Result<*mut Eterm> {
        if n == 0 {
            // No storage is requested, so no region is reserved: the documented
            // dangling-but-valid pointer is the answer.
            return Ok(core::ptr::NonNull::<Eterm>::dangling().as_ptr());
        }
        // Reserve the first region lazily, on first use (construction stays
        // infallible; a reservation failure surfaces here as a typed error).
        if self.regions.is_empty() {
            let first = self.fresh_region(n)?;
            self.regions.push(first);
        }
        // Fast path: the frontier region's committed slice already has room.
        if self.frontier().available() >= n {
            return self.alloc_from_frontier(n);
        }
        // Otherwise raise the frontier's committed limit in place. The bump pointer
        // does not move, so a retry on the SAME region is correct (only `limit()`
        // moved, and `alloc_words` re-reads it).
        let outcome = self.frontier_mut().try_grow(n)?;
        if outcome == GrowOutcome::GrewInPlace {
            return self.alloc_from_frontier(n);
        }
        // The whole reservation is exhausted, so this region can never serve the
        // request. Never `relocate` (it moves the live block and would invalidate
        // every pointer handed out — see the struct docs): keep every existing region
        // alive and make a fresh, larger one the frontier. A reservation failure
        // surfaces as a typed `BeamError::Alloc`, never a panic.
        let fresh = self.fresh_region(n)?;
        self.regions.push(fresh);
        self.alloc_from_frontier(n)
    }

    /// The allocation frontier: the newest region. Only called once a region exists.
    fn frontier(&self) -> &ReservedHeap {
        self.regions
            .last()
            .expect("a region is ensured before the frontier is read")
    }

    /// The allocation frontier, mutably.
    fn frontier_mut(&mut self) -> &mut ReservedHeap {
        self.regions
            .last_mut()
            .expect("a region is ensured before the frontier is read")
    }

    /// Hand out `n` words from the frontier region's committed slice.
    ///
    /// The caller guarantees `n <= frontier.available()`: the fast path checked it,
    /// and each growth path ends with the committed limit covering `n`.
    fn alloc_from_frontier(&mut self, n: usize) -> Result<*mut Eterm> {
        let heap = self.frontier_mut();
        debug_assert!(
            heap.available() >= n,
            "the frontier's committed slice must cover the request"
        );
        // SAFETY: the caller guarantees `n <= heap.available()`, so the request lies
        // wholly inside this region's committed slice — exactly `alloc_words`'s
        // precondition. The region is owned exclusively by this heap (a `&mut self`
        // method) and stays alive, unmoved, until the heap is dropped, so the returned
        // pointer is valid (and stable) for the context's lifetime.
        unsafe { heap.alloc_words(n) }
    }

    /// A fresh region that can serve `n` words with headroom.
    ///
    /// The reservation **doubles** the previous one (a fresh region only ever follows
    /// an exhausted region, so total capacity stays exponential in the region count),
    /// floored at the configured geometry and raised to at least `n`; at least `n`
    /// words are committed, so the new frontier serves this very request without a
    /// `try_grow`. A reservation failure (byte-size overflow, or the allocator's own
    /// typed error) propagates as [`BeamError::Alloc`](beam_common::BeamError::Alloc).
    fn fresh_region(&self, n: usize) -> Result<ReservedHeap> {
        let reserved = self
            .regions
            .last()
            .map_or(self.reserve_words, |prev| {
                prev.reserved_words().saturating_mul(2)
            })
            .max(self.reserve_words)
            .max(n);
        let committed = self.commit_words.max(n).min(reserved);
        ReservedHeap::new(reserved, committed)
    }
}

/// Minimal context: an unbounded heap plus a pluggable BIF hook.
///
/// Used by tests and embeddings. [`MinimalContext::ensure_heap`] is a no-op (the
/// heap is treated as unbounded), [`MinimalContext::new`] installs a default hook
/// that rejects every BIF with [`crate::error::unsupported`], and
/// [`MinimalContext::alloc_words`](ExecutionContext::alloc_words) hands out words
/// from a pointer-stable region heap (see the field docs).
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
    /// An optional local-closure resolver (the
    /// [`ExecutionContext::resolve_local_fun`] body).
    local_fun_resolver: Option<LocalFunResolver>,
    /// An optional mailbox (the body of the nine receive-seam methods).
    mailbox: Option<Box<dyn Mailbox>>,
    /// The pointer-stable heap backing [`ExecutionContext::alloc_words`].
    ///
    /// A [`ContextHeap`]: a list of [`ReservedHeap`] regions that only ever grows.
    /// An allocation is served from the frontier region's committed slice (raised
    /// **in place**, so no word ever moves), and only an exhausted *reservation*
    /// pushes a fresh, larger region — leaving every earlier region alive and every
    /// pointer it handed out valid. Nothing is freed before the context is dropped,
    /// which is exactly the "valid until the next `ensure_heap`" stability contract
    /// the trait documents (in this context `ensure_heap` never moves anything).
    heap: ContextHeap,
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
            local_fun_resolver: None,
            mailbox: None,
            heap: ContextHeap::new(),
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

    /// A context with the given local-closure resolver and the default
    /// (rejecting) BIF hook.
    ///
    /// Combine with a BIF hook via [`set_bif_hook`](MinimalContext::set_bif_hook).
    pub fn with_local_fun_resolver(resolver: LocalFunResolver) -> MinimalContext {
        let mut ctx = MinimalContext::new();
        ctx.local_fun_resolver = Some(resolver);
        ctx
    }

    /// Install (or replace) the local-closure resolver.
    pub fn set_local_fun_resolver(&mut self, resolver: LocalFunResolver) {
        self.local_fun_resolver = Some(resolver);
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

    /// [`MinimalContext`] never pre-empts: its [`reduce`](ExecutionContext::reduce)
    /// is the trait's default no-op, so the per-instruction hook call is pure
    /// overhead here and the driver skips it. The reduction *count*
    /// ([`Machine::reductions`](crate::machine::Machine::reductions)) is unchanged —
    /// only the vtable crossing is elided.
    #[inline]
    fn wants_reduce(&self) -> bool {
        false
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        // The region heap hands out `n` uninitialised words whose pointer stays
        // valid (and unmoved) for the life of the context: the frontier region
        // commits more room in place and only an exhausted reservation pushes a
        // fresh, larger region, leaving the old one — and every pointer into it —
        // untouched. A zero-word request still yields a non-null, well-aligned
        // pointer.
        self.heap.alloc(n)
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

    fn resolve_local_fun(
        &mut self,
        fun: Eterm,
        args: &[Eterm],
    ) -> Result<Option<(Arc<Module>, Label)>> {
        // Without an installed resolver there is no closure registry to consult,
        // so the in-machine route declines and the value-returning `apply_fun`
        // fallback stays the dispatch route (the trait default).
        match self.local_fun_resolver.as_mut() {
            Some(resolver) => resolver(fun, args),
            None => {
                let _ = (fun, args);
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
    fn the_reduce_gate_defaults_conservative_and_the_minimal_context_opts_out() {
        // The trait default is `true`: a host that does not override it keeps receiving
        // every reduction charge, so a pre-empting host is never silently skipped.
        assert!(
            OnlyRequired.wants_reduce(),
            "the reduce-hook gate must default to the conservative `true`"
        );
        // `MinimalContext` never pre-empts (its `reduce` is the default no-op), so it
        // opts out of the per-instruction vtable crossing.
        assert!(
            !MinimalContext::new().wants_reduce(),
            "MinimalContext must opt out of the reduction hook"
        );
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
        let ptr = ctx.alloc_words(4).expect("the region heap allocates");
        assert!(!ptr.is_null(), "a non-empty block has a non-null pointer");
    }

    #[test]
    fn minimal_context_zero_word_allocation_is_non_null() {
        // Documented: a zero-word request may return a dangling, but non-null,
        // well-aligned pointer.
        let mut ctx = MinimalContext::new();
        let ptr = ctx
            .alloc_words(0)
            .expect("the region heap allocates an empty block");
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
    fn minimal_context_allocation_is_pointer_stable_across_growth() {
        // The whole stability contract through the public API: a pointer handed out
        // by `alloc_words` still owns its words after enough later allocations to
        // grow the heap several times over.
        let mut ctx = MinimalContext::new();
        let first = ctx.alloc_words(4).expect("the region heap allocates");
        // SAFETY: `alloc_words(4)` returned 4 exclusively-owned writable words.
        unsafe { first.write(Eterm::from_raw(0x5A5A_5A5A)) };

        for _ in 0..50_000 {
            let later = ctx.alloc_words(4).expect("the region heap keeps allocating");
            // SAFETY: `alloc_words(4)` returned 4 exclusively-owned writable words.
            unsafe { later.write(Eterm::from_raw(0x1122_3344)) };
        }

        // SAFETY: `first` is still inside a live region and exclusively owned.
        assert_eq!(
            unsafe { first.read() },
            Eterm::from_raw(0x5A5A_5A5A),
            "the first allocation's words survive every later one"
        );
    }

    // ---- the region heap (`ContextHeap`) ----------------------------------
    //
    // These drive the heap type directly with small, exact geometry (`with_geometry`)
    // so each growth path is forced deterministically instead of by allocating
    // megabytes; the tests above cover the same contracts through the public seam.

    #[test]
    fn a_zero_word_request_answers_ok_and_reserves_nothing() {
        let mut heap = ContextHeap::with_geometry(8, 2);
        let ptr = heap.alloc(0).expect("a zero-word request is Ok");
        assert!(!ptr.is_null(), "a zero-word block is dangling but non-null");
        assert_eq!(ptr as usize % core::mem::align_of::<Eterm>(), 0);
        assert!(
            heap.regions.is_empty(),
            "a zero-word request must not reserve a region"
        );

        // ...and it stays non-null and aligned once a region exists.
        let _ = heap.alloc(1).expect("the first region is reserved");
        let ptr = heap.alloc(0).expect("a zero-word request is Ok");
        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % core::mem::align_of::<Eterm>(), 0);
    }

    #[test]
    fn an_in_place_grow_keeps_earlier_pointers_valid() {
        // Reserve 64 words but commit only 4: the next request fits the reservation,
        // so it must be served by raising the committed limit IN PLACE.
        let mut heap = ContextHeap::with_geometry(64, 4);
        let first = heap.alloc(4).expect("the committed slice serves 4 words");
        // SAFETY: 4 exclusively-owned words; write the first.
        unsafe { first.write(Eterm::from_raw(0xFEED_FACE)) };

        // `used == committed == 4`, so this request needs growth — which the
        // reservation can serve without moving the bump pointer.
        let second = heap.alloc(2).expect("in-place growth serves the request");
        assert_eq!(
            heap.regions.len(),
            1,
            "an in-place grow must not add a region"
        );
        assert_eq!(
            second as usize,
            first as usize + 4 * core::mem::size_of::<Eterm>(),
            "an in-place grow continues at the same (unmoved) bump pointer"
        );
        // SAFETY: `first` is still a live, exclusively-owned 4-word block.
        assert_eq!(
            unsafe { first.read() },
            Eterm::from_raw(0xFEED_FACE),
            "raising the committed limit never disturbs the words already handed out"
        );
    }

    #[test]
    fn an_exhausted_reservation_keeps_the_old_region_alive() {
        // Reserve == commit == 8, so one request fills the whole reservation and the
        // next cannot be served in place: the MoveRequired path.
        let mut heap = ContextHeap::with_geometry(8, 8);
        let first = heap.alloc(8).expect("the whole reservation serves 8 words");
        assert_eq!(heap.regions.len(), 1);
        // SAFETY: 8 exclusively-owned words; write the first and the last.
        unsafe {
            first.write(Eterm::from_raw(0xDEAD_BEEF));
            first.add(7).write(Eterm::from_raw(0xCAFE_BABE));
        }

        let second = heap.alloc(8).expect("a fresh region serves the request");
        assert_eq!(
            heap.regions.len(),
            2,
            "an exhausted reservation must be followed by a fresh region"
        );
        // The two regions are disjoint: the fresh region's 8 words do not overlap the
        // old region's (either order is fine — they are separate reservations).
        let span = 8 * core::mem::size_of::<Eterm>();
        assert!(
            second as usize >= first as usize + span
                || (second as usize) + span <= first as usize,
            "the fresh region must not overlap the region it replaces"
        );
        // The OLD region is kept ALIVE and UNMOVED: its contents survive, read back
        // through the pointer handed out before the fresh region existed.
        // SAFETY: `first` still points into the live first region.
        unsafe {
            assert_eq!(first.read(), Eterm::from_raw(0xDEAD_BEEF));
            assert_eq!(first.add(7).read(), Eterm::from_raw(0xCAFE_BABE));
        }
    }

    #[test]
    fn an_exhausted_region_is_followed_by_a_larger_one() {
        let mut heap = ContextHeap::with_geometry(8, 8);
        let _ = heap.alloc(8).expect("the first reservation is served");
        let _ = heap.alloc(8).expect("a fresh region is created");
        assert_eq!(heap.regions.len(), 2);
        assert!(
            heap.regions[1].reserved_words() > heap.regions[0].reserved_words(),
            "a fresh region doubles the reservation ({} -> {})",
            heap.regions[0].reserved_words(),
            heap.regions[1].reserved_words()
        );
    }

    #[test]
    fn a_request_larger_than_the_whole_reservation_is_served() {
        // Reserve a mere 4 words: the first request cannot be served at all, so a
        // fresh region sized to the request (with headroom) must be created.
        let mut heap = ContextHeap::with_geometry(4, 4);
        let big = heap.alloc(64).expect("a region sized to the request is created");
        assert_eq!(heap.regions.len(), 1);
        assert!(heap.regions[0].reserved_words() >= 64);
        // The request was served in full: every word is writable and readable.
        for i in 0..64 {
            // SAFETY: `big` owns 64 exclusive words, so index `i` is in bounds.
            unsafe { big.add(i).write(Eterm::from_raw(0x1000 + i)) };
        }
        for i in 0..64 {
            // SAFETY: as above.
            assert_eq!(unsafe { big.add(i).read() }, Eterm::from_raw(0x1000 + i));
        }
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
