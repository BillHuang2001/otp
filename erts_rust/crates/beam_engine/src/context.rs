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
//! it is the rooted twin of [`alloc_words`], taking a [`RootSet`] (the live
//! X-register prefix, the whole Erlang stack and the live FR prefix) and defaulting to
//! [`alloc_words`] with the roots discarded, so **every existing implementor
//! compiles and behaves byte-identically**. [`crate::machine::Machine::alloc_rooted`]
//! is where the engine's own slices are handed over.
//!
//! ## The in-machine BIF-dispatch door
//!
//! [`call_bif`] hands a host only the `module`/`function`/`arity`/`args` of a BIF,
//! never the process it is running in — so an implementation cannot read the
//! caller's execution state (the shape `erlang:process_info(self(),
//! current_stacktrace)` needs). [`wants_bif_in_machine`] + [`call_bif_in_machine`]
//! close that gap: a host that returns `true` from the probe is handed the live
//! [`Machine`] and the run's **entry module** on every BIF dispatch. The probe is
//! latched once per run by **both** the interpreters' driver
//! ([`crate::machine::Machine::drive`]) and the native baseline tier's, and every
//! BIF dispatch funnels through [`crate::machine::Machine::dispatch_bif_call`], so
//! the door behaves identically on every execution tier. The default `false` keeps
//! the plain [`call_bif`] path byte-identical.
//!
//! ## The BIF suspend channel
//!
//! [`call_bif`] can answer only a value or an error, so a BIF that cannot complete
//! **without waiting** (a `erlang:port_command/2,3` aimed at a busy port, a blocking
//! port write) has no way to park its caller and be re-issued. [`BifOutcome`] is that
//! channel, and it has an outcome-aware door beside each of the two dispatch doors:
//! [`call_bif_outcome`] (the plain twin of [`call_bif`]) and
//! [`call_bif_in_machine_outcome`] (the twin of [`call_bif_in_machine`], which is what
//! a host that opted into the in-machine door must override). Both **default** to
//! forwarding and wrapping the value, so every existing implementor is
//! byte-identical.
//!
//! A [`BifOutcome::Suspend`] means the BIF did not complete and the instruction was
//! **not** applied: the engine parks the machine on that instruction (its own
//! `(func, pc)`, nothing else — it neither copies the arguments nor remembers the
//! BIF) and reports [`crate::RunOutcome::Suspend`]. Resumption **re-executes** the
//! instruction, which re-reads its operands from `x`/`y`/literals/stack — i.e. from
//! the frozen [`RootSet`] — so a suspended BIF's live operands are rooted by
//! construction. It is the engine-side twin of ERTS's `ERTS_BIF_YIELD2/3`.
//!
//! ## The closure-apply suspend channel
//!
//! [`apply_fun`] has the same limitation for a host that **interprets** a closure
//! itself (a tree-walking `-eval` evaluator), so [`FunOutcome`] and
//! [`apply_fun_outcome`] are its twin: a [`FunOutcome::Suspend`] parks the machine on
//! the `call_fun`/`call_fun2` instruction, and the resume **re-consults the door with
//! the same `fun` and `args`**, so the host must *continue* the pending application
//! rather than restart it. Both doors **default** to forwarding and wrapping the
//! value, so a host that predates them is byte-identical.
//!
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
//! ## The atom-name resolution seam
//!
//! [`Module::atom_name`](beam_loader::Module::atom_name) indexes the module's
//! **owned load-time snapshot** of the shared atom table, so a module loaded early
//! cannot name an atom interned into the shared namespace afterwards. The engine
//! resolves a **runtime atom value** (a variable `M`/`F` in `apply(M,F,A)` /
//! `M:F(...)`) through [`ExecutionContext::atom_name`], whose default body is that
//! module-local lookup and whose override lets a host that owns the live shared
//! namespace answer where the snapshot cannot.
//!
//! [`alloc_words`]: ExecutionContext::alloc_words
//! [`apply`]: ExecutionContext::apply
//! [`apply_fun`]: ExecutionContext::apply_fun
//! [`call_bif`]: ExecutionContext::call_bif
//! [`wants_bif_in_machine`]: ExecutionContext::wants_bif_in_machine
//! [`call_bif_in_machine`]: ExecutionContext::call_bif_in_machine
//! [`call_bif_outcome`]: ExecutionContext::call_bif_outcome
//! [`call_bif_in_machine_outcome`]: ExecutionContext::call_bif_in_machine_outcome
//! [`RootSet`]: crate::context::RootSet
//! [`atom_name`]: ExecutionContext::atom_name
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
use beam_loader::{Label, Module, RecordDefinition};
use beam_term::{Atom, AtomTable, Eterm};

use crate::machine::Machine;

/// The arena's two **reclaim tiers** (`src/context/reclaim.rs`): the `HeapMark` rewind
/// ([`ContextHeap::rewind`] / [`MinimalContext::heap_rewind`]) and the copying collapse
/// ([`ContextHeap::reclaim`] / [`MinimalContext::reclaim_heap`]).
///
/// Both are **additive**: they change no allocation policy, are never invoked
/// automatically, and leave [`MinimalContext::alloc_words`](ExecutionContext::alloc_words),
/// `alloc_words_rooted`, [`ensure_heap`](ExecutionContext::ensure_heap) and
/// [`heap_available`](ExecutionContext::heap_available) byte-identical. Each one is a
/// *caller-asserted* operation — see the report types and the two doors for the liveness
/// contract a host must establish first.
mod reclaim;
pub use reclaim::{HeapReclaim, HeapRewind};

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
/// This is what [`ExecutionContext::alloc_words_rooted`] receives: the live roots *by
/// mutable slice*, with each moved term written back through them, so a collector can
/// move a term without the engine keeping any frame liveness map (the same contract
/// [`ExecutionContext::ensure_heap`] documents).
///
/// # The root-set contract (frozen)
///
/// The live roots of a running [`Machine`](crate::machine::Machine) are exactly:
///
/// * `x[0..live_len()]` — the leading X registers of [`RootSet::x`]; the trailing
///   registers are dead for the instruction in flight;
/// * **the whole** [`RootSet::stack`] (every Y slot of every live frame — there are
///   no frame liveness maps);
/// * **the whole** [`RootSet::fr`] **as handed** (the FR file, which at Milestone 1
///   holds `Eterm` **term slots**, so it can hold heap pointers like any other
///   register).
///
/// **The two register slices are exactly what the machine hands over for this run,
/// never a whole file**: each carries the file's *used extent*
/// (`machine::registers::used_extent` — the larger of the register-write seam's
/// high-water mark and the extent a slot-escaping writer reported, clamped to the
/// file), and every slot at or beyond it is [`NIL`](beam_term::NIL) by construction,
/// so it is not a root and cannot be relocated. A host therefore **relocates and
/// writes a moved term back only inside the slices it receives** — it physically
/// cannot write outside them — and *that hand-off*, not a host promise, is what
/// bounds [`Machine::reset`](crate::machine::Machine::reset)'s refill to the words a
/// run actually used.
///
/// [`RootSet::live`] is only a **hint**: [`RootSet::live_len`] clamps it to the
/// slice length, so [`RootSet::ALL_LIVE`] (`u32::MAX`) means "every handed
/// register". Declaring more than is really live is always *safe* for a copying
/// collector (it can only keep a term alive that would otherwise die, never free a
/// live one); the engine computes no register liveness of its own.
///
/// Nothing outside the root set is a root: a raw [`Eterm`] the engine is holding
/// only in a local variable, or a `*mut Eterm` from a previous
/// [`ExecutionContext::alloc_words`], is **not** preserved across a collection —
/// exactly the C `HAlloc`/`heap_top` convention, so a pointer returned by
/// `alloc_words` is invalidated by the next `ensure_heap`/collecting `alloc_words`
/// and the engine must re-read any live term from `x`/`stack`/`fr`.
#[derive(Debug)]
pub struct RootSet<'a> {
    /// The writable **root prefix** of the X-register file — what the machine hands
    /// over for this run (the file's used extent), never the whole file.
    pub x: &'a mut [Eterm],
    /// The whole Erlang stack: the Y slots of every live frame, oldest first.
    pub stack: &'a mut [Eterm],
    /// The live float-register (FR) prefix the machine hands over for this run
    /// (`Eterm` term slots at Milestone 1); a slot beyond it is `NIL`.
    pub fr: &'a mut [Eterm],
    /// How many leading `x` slots the caller declares live (a hint; clamped by
    /// [`RootSet::live_len`]). [`RootSet::ALL_LIVE`] means "every handed register".
    pub live: u32,
}

impl<'a> RootSet<'a> {
    /// The value of [`RootSet::live`] that declares **every** handed X register live.
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

    /// The declared live X-register count, clamped to the handed root prefix.
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

/// The outcome of a BIF dispatch that is allowed to **wait**.
///
/// [`ExecutionContext::call_bif`] can only answer a value or an error, which leaves a
/// BIF that *cannot complete without waiting* — a plain `erlang:port_command/2,3`
/// aimed at a busy port, a blocking port write — with no way to park its caller and be
/// re-issued: it can only raise a typed "cannot pump" error. This enum is the missing
/// channel, the engine-side twin of ERTS's `ERTS_BIF_YIELD2/3` (`erts/emulator/beam/
/// bif.h:330-337`), whose `BIF_TRAP_N` writes the BIF's arguments into
/// `x_reg_array[0..N]`, points `c_p->i` at a synthetic **trap export for the same
/// BIF** and returns `THE_NON_VALUE`.
///
/// * [`BifOutcome::Return`] — the ordinary answer, exactly what
///   [`ExecutionContext::call_bif`] returns.
/// * [`BifOutcome::Suspend`] — the BIF could not complete without waiting. The engine
///   does **not** apply the instruction: it parks on it (the machine's own
///   `(func, pc)`) and reports a suspend ([`crate::RunOutcome::Suspend`]). The host
///   later resumes the process, and the BIF is **re-issued from the same instruction
///   with the same operands** — so the caller sees *that* call's value, never a
///   value the abandoned attempt computed.
///
/// # Rooting
///
/// The engine keeps **no** suspended-BIF state: it neither copies the arguments nor
/// remembers the BIF's identity. Parking is the machine's `(func, pc)` plus its
/// registers, its Erlang stack and its FR file — and resumption **re-executes the
/// instruction**, which re-reads its operands from `x`/`y`/literals/stack. Those are
/// exactly the frozen [`RootSet`] (`x[..live]`, the whole stack, the whole FR file), so
/// a suspended BIF's live operands are rooted **by construction** and a moving
/// collector may relocate them. (In C the equivalent is the process-private
/// `c_arg_reg`/`arg_reg` array, saved by `copy_out_registers` and restored by
/// `copy_in_registers` — `beam_common.c:306`/`:336`.)
///
/// # Where a suspend is honoured
///
/// Only the re-execution-idempotent dispatch sites turn a suspend into a park (its
/// operand reads are pure and every write is success-only). A site that rewrites
/// registers *before* dispatching — the `erlang:apply/3` route, which has already
/// unpacked the argument list into `x(0..arity)` — reports a **typed error** instead of
/// silently treating the suspend as a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BifOutcome {
    /// The BIF completed; this is its value.
    Return(Eterm),
    /// The BIF could not complete without waiting; the instruction was **not** applied
    /// and must be re-executed on resume (see the type docs).
    Suspend,
}

impl BifOutcome {
    /// Is this a [`BifOutcome::Suspend`]?
    pub fn is_suspend(&self) -> bool {
        matches!(self, BifOutcome::Suspend)
    }

    /// The returned value, or `None` for a suspend.
    pub fn into_value(self) -> Option<Eterm> {
        match self {
            BifOutcome::Return(value) => Some(value),
            BifOutcome::Suspend => None,
        }
    }

    /// The returned value, or the typed suspend error `at` names.
    ///
    /// The bridge for a dispatch site that is **not** re-execution-idempotent (see
    /// [`BifOutcome`]): a suspend cannot be honoured there, and it must never be
    /// mistaken for a value, so it becomes a [`crate::error::unsupported`] naming the
    /// site.
    pub fn into_value_or_unsupported(self, at: &str) -> Result<Eterm> {
        match self {
            BifOutcome::Return(value) => Ok(value),
            BifOutcome::Suspend => Err(crate::error::unsupported(at)),
        }
    }
}

/// The outcome of a **fun/closure application** that is allowed to **wait** — the
/// closure-apply twin of [`BifOutcome`].
///
/// [`ExecutionContext::apply_fun`] can only answer a value or an error, which leaves a
/// host that **interprets** the closure itself — `beam_rt`'s tree-walking `-eval`
/// evaluator, for one — with no way to park its caller when the body has to wait (a
/// `receive` with no message, a timer): it can only raise. This enum is that missing
/// channel, and [`ExecutionContext::apply_fun_outcome`] is the door beside
/// [`apply_fun`](ExecutionContext::apply_fun).
///
/// * [`FunOutcome::Return`] — the application completed; this is its value, exactly
///   what [`ExecutionContext::apply_fun`] returns.
/// * [`FunOutcome::Suspend`] — the application could not complete without waiting. The
///   engine does **not** apply the instruction: it parks on it (the machine's own
///   `(func, pc)`) and reports a suspend ([`crate::RunOutcome::Suspend`]). The host
///   later resumes the process, and this method is **consulted again with the same
///   `fun` and `args`** — see the contract on
///   [`ExecutionContext::apply_fun_outcome`] for what that requires of the host.
///
/// # Rooting
///
/// The engine keeps **no** suspended-application state: it neither copies the
/// arguments nor remembers the closure. Parking is the machine's `(func, pc)` plus its
/// registers, its Erlang stack and its FR file — and resumption **re-executes the
/// instruction**, which re-reads the fun (`x(arity)`, or the `call_fun2` operand) and
/// the arguments (`x(0..arity)`) from that state. Those are exactly the frozen
/// [`RootSet`], so a suspended application's live operands are rooted **by
/// construction** and a moving collector may relocate them.
///
/// # Where a suspend is honoured
///
/// At the `Ok(None)` arm of the `call_fun`/`call_fun2` closure seam — the one engine
/// call site of the apply fallback, whose operand reads are pure and whose single write
/// (`x(0)` with the value) is success-only, so re-executing it applies nothing twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FunOutcome {
    /// The application completed; this is its value.
    Return(Eterm),
    /// The application could not complete without waiting; the instruction was **not**
    /// applied and the host must continue it on resume (see the type docs).
    Suspend,
}

impl FunOutcome {
    /// Is this a [`FunOutcome::Suspend`]?
    pub fn is_suspend(&self) -> bool {
        matches!(self, FunOutcome::Suspend)
    }

    /// The returned value, or `None` for a suspend.
    pub fn into_value(self) -> Option<Eterm> {
        match self {
            FunOutcome::Return(value) => Some(value),
            FunOutcome::Suspend => None,
        }
    }

    /// The returned value, or the typed suspend error `at` names.
    ///
    /// The bridge for a caller that cannot honour a suspend (see [`FunOutcome`]): it
    /// must never be mistaken for a value, so it becomes a
    /// [`crate::error::unsupported`] naming the site.
    pub fn into_value_or_unsupported(self, at: &str) -> Result<Eterm> {
        match self {
            FunOutcome::Return(value) => Ok(value),
            FunOutcome::Suspend => Err(crate::error::unsupported(at)),
        }
    }
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

    /// The **outcome-aware** BIF door: [`call_bif`](ExecutionContext::call_bif) that is
    /// also allowed to answer [`BifOutcome::Suspend`].
    ///
    /// A host whose BIFs can **wait** — a `erlang:port_command/2,3` at a busy port, a
    /// blocking port write — overrides this door; the engine then folds a
    /// [`BifOutcome::Suspend`] into [`Step::Suspend`](crate::Step) at every
    /// re-execution-idempotent dispatch site, parking the machine on the instruction
    /// and reporting [`crate::RunOutcome::Suspend`] to a blocking-aware run entry.
    ///
    /// **Contract.**
    /// * `Suspend` means the BIF did **not** complete and the instruction was **not**
    ///   applied: the host resumes the process itself and the BIF is re-issued from
    ///   the same instruction with the same operands, so it must be **re-execution
    ///   safe** on the host side too (do not consume a one-shot resource on the
    ///   suspending attempt).
    /// * Only the sites listed on [`BifOutcome`] honour a suspend. The
    ///   `erlang:apply/3` route (which rewrites `x(0..arity)` before dispatching)
    ///   reports a typed error for one.
    /// * A host that answered `true` from
    ///   [`wants_bif_in_machine`](ExecutionContext::wants_bif_in_machine) must
    ///   override [`call_bif_in_machine_outcome`](ExecutionContext::call_bif_in_machine_outcome)
    ///   instead — the engine then calls **only** that door, exactly as it calls only
    ///   [`call_bif_in_machine`](ExecutionContext::call_bif_in_machine) today.
    /// * [`crate::RunOutcome::Suspend`] is only reachable from a blocking-aware run
    ///   entry; a non-blocking one folds it into the typed
    ///   [`crate::error::suspended`] error.
    ///
    /// The default body forwards to [`call_bif`](ExecutionContext::call_bif) and wraps
    /// the value, so **every existing implementor compiles and behaves
    /// byte-identically**.
    fn call_bif_outcome(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<BifOutcome> {
        self.call_bif(module, function, arity, args)
            .map(BifOutcome::Return)
    }

    /// The **in-machine outcome-aware** BIF door: the
    /// [`call_bif_in_machine`](ExecutionContext::call_bif_in_machine) dispatch that is
    /// also allowed to answer [`BifOutcome::Suspend`].
    ///
    /// The engine takes this door only when
    /// [`wants_bif_in_machine`](ExecutionContext::wants_bif_in_machine) answered `true`
    /// for the run, so a host that opts in must override **this** method (never only
    /// [`call_bif_outcome`](ExecutionContext::call_bif_outcome)) to make its BIFs able
    /// to wait. `machine` is the live process state, `entry_module` the module the run
    /// was entered at, and the arguments are exactly
    /// [`call_bif_in_machine`](ExecutionContext::call_bif_in_machine)'s.
    ///
    /// The suspend contract is [`call_bif_outcome`](ExecutionContext::call_bif_outcome)'s
    /// verbatim: the engine keeps no suspended-BIF state, so parking is the machine's
    /// own `(func, pc)` and its rooted registers/stack/FR file, and resumption
    /// re-executes the instruction.
    ///
    /// The default body discards the two extra arguments, forwards to
    /// [`call_bif_in_machine`](ExecutionContext::call_bif_in_machine) and wraps the
    /// value, so every existing implementor is byte-identical.
    fn call_bif_in_machine_outcome(
        &mut self,
        machine: &Machine,
        entry_module: &Module,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<BifOutcome> {
        self.call_bif_in_machine(machine, entry_module, module, function, arity, args)
            .map(BifOutcome::Return)
    }
    /// The probe for the **in-machine** BIF-dispatch door
    /// ([`call_bif_in_machine`](ExecutionContext::call_bif_in_machine)).
    ///
    /// A host that returns `true` receives the *currently executing* [`Machine`]
    /// and the run's **entry module** alongside every BIF dispatch, so a BIF
    /// implementation can read the running process's execution state from *inside*
    /// the call — the shape `erlang:process_info(self(), current_stacktrace)`
    /// needs when it is reached through `gen_server:handle_common_reply/5`'s
    /// `?STACKTRACE()`. The machine and the context are distinct objects at the
    /// dispatch site, so this is the only way a `&mut self` context method can be
    /// handed a `&Machine`.
    ///
    /// **Contract.** A host that returns `true` **must** also override
    /// [`call_bif_in_machine`](ExecutionContext::call_bif_in_machine): the engine
    /// then calls **only** that method, never [`call_bif`](ExecutionContext::call_bif).
    ///
    /// The answer is latched **once per run** by the driver that owns the run —
    /// [`Machine::drive`](crate::machine::Machine::drive) for the interpreter and
    /// the native baseline tier's own driver for
    /// [`Tier::BaselineJit`](crate::jit::Tier::BaselineJit) — exactly like
    /// [`wants_reduce`](ExecutionContext::wants_reduce), so the door is taken on
    /// **both execution tiers** and a host that leaves the default `false` keeps the
    /// plain `call_bif` path **byte-identical** and pays a single `bool` load per
    /// dispatch instead of an extra vtable crossing.
    ///
    /// The seam is a **read-only** borrow of the machine and involves no `unsafe`:
    /// the host can inspect the process state, and the engine's own state machine is
    /// untouched by the call.
    fn wants_bif_in_machine(&self) -> bool {
        false
    }

    /// The **entry-aware BIF-dispatch door**: the same dispatch as
    /// [`call_bif`](ExecutionContext::call_bif), with the currently executing
    /// [`Machine`] and the run's **entry module** handed in.
    ///
    /// The engine takes this door only when
    /// [`wants_bif_in_machine`](ExecutionContext::wants_bif_in_machine) answered
    /// `true` for the run (so a host that does not opt in never sees this method
    /// called). `module`/`function`/`arity`/`args` are exactly
    /// [`call_bif`](ExecutionContext::call_bif)'s; `machine` is the process state
    /// the BIF is dispatching inside and `entry_module` is the module the run was
    /// entered at.
    ///
    /// `entry_module` is the module [`Machine::drive`](crate::machine::Machine::drive)
    /// was entered with — the module the **context-0** frames belong to. A
    /// [`Machine`] borrows context `0` rather than owning it (see
    /// [`Machine::current_module`](crate::machine::Machine::current_module)), so the
    /// entry module cannot be recovered from `&Machine` and is threaded here
    /// instead. It is what the machine's execution-position accessors expect for a
    /// context-0 frame:
    ///
    /// * [`Machine::current_stacktrace`](crate::machine::Machine::current_stacktrace)
    /// * [`Machine::current_function`](crate::machine::Machine::current_function)
    /// * [`Machine::current_location`](crate::machine::Machine::current_location)
    /// * [`Machine::initial_call`](crate::machine::Machine::initial_call)
    ///
    /// so a host calls, e.g.,
    /// `machine.current_stacktrace(entry_module, self)`.
    ///
    /// Is called on **both execution tiers**: the interpreter's dispatch loop and
    /// the native baseline tier's deopt helper both reach the single BIF-dispatch
    /// funnel ([`Machine::dispatch_bif_call`](crate::machine::Machine)), so the same
    /// module/context is handed the live machine whichever tier it runs on. The
    /// machine is borrowed read-only — there is no `unsafe` here, and neither the
    /// host's call nor the engine's state machine is affected by the inspection.
    ///
    /// The default body discards the two extra arguments and forwards to
    /// [`call_bif`](ExecutionContext::call_bif), so **every existing implementor
    /// compiles and behaves byte-identically**.
    fn call_bif_in_machine(
        &mut self,
        machine: &Machine,
        entry_module: &Module,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        let _ = (machine, entry_module);
        self.call_bif(module, function, arity, args)
    }

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
    /// [`RootSet`] — the live X-register prefix, the whole Erlang stack **and the live
    /// FR prefix** the root set carries. The FR file holds `Eterm` **term slots** at
    /// Milestone 1, so a float register can point into the process heap and must be a
    /// root; the narrow `ensure_heap` cannot see it, so a module containing any
    /// float-family instruction could never be collected over correctly.
    /// [`Machine::ensure_heap`](crate::machine::Machine::ensure_heap) — reached by
    /// `test_heap`/`allocate_heap` — calls **this** method, so a host whose heap must
    /// be collected at the primary admission point now sees every live root.
    ///
    /// The default body forwards to
    /// [`ensure_heap`](ExecutionContext::ensure_heap) with the FR slice discarded, so
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
    /// that missing admission point: it takes the [`RootSet`] (the live X-register
    /// prefix, the whole Erlang stack and the live FR prefix — see the type docs for
    /// the frozen contract) and returns a pointer to `n` **uninitialized** words under
    /// exactly [`alloc_words`](ExecutionContext::alloc_words)'s stability rule (valid
    /// until the next `ensure_heap`/collecting allocation).
    ///
    /// The default body is [`alloc_words`](ExecutionContext::alloc_words) with the
    /// roots discarded, so **every existing implementor keeps working unchanged**
    /// (and byte-identically): a host with a real collector overrides this and runs
    /// its collector over `roots` first, writing any moved term back through
    /// `roots.x`/`roots.stack`/`roots.fr` — inside the slices it was handed, which is
    /// the whole writable window.
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

    /// The **outcome-aware** closure-apply door: [`apply_fun`](ExecutionContext::apply_fun)
    /// that is also allowed to answer [`FunOutcome::Suspend`].
    ///
    /// A host that **interprets** the closure itself and whose body can **wait** — a
    /// tree-walking evaluator applying an `-eval` lambda that reaches a `receive` with
    /// no message — overrides this door; the engine then folds a
    /// [`FunOutcome::Suspend`] into [`Step::Suspend`](crate::Step) at the apply
    /// fallback, parking the machine on the `call_fun`/`call_fun2` instruction and
    /// reporting [`crate::RunOutcome::Suspend`] to a blocking-aware run entry. It is
    /// the closure-apply twin of the BIF suspend channel
    /// ([`call_bif_outcome`](ExecutionContext::call_bif_outcome) / [`BifOutcome`]).
    ///
    /// **Contract.**
    /// * `Suspend` means the application did **not** complete and the
    ///   `call_fun`/`call_fun2` instruction was **not** applied: no register, stack
    ///   slot, frame or continuation was written.
    /// * The engine parks the machine on that instruction and resumption
    ///   **re-executes** it, re-consulting this method with the **same** `fun` and the
    ///   **same** `args`. The host must therefore **CONTINUE** the pending
    ///   application, never restart it — an evaluator body is not re-execution
    ///   idempotent — and must not consume a one-shot resource on the suspending
    ///   attempt. (Contrast [`BifOutcome`], where the re-issued dispatch re-runs the
    ///   BIF from its operands.)
    /// * A host that declines the in-machine closure seam does not necessarily reach
    ///   this door: [`resolve_local_fun`](ExecutionContext::resolve_local_fun) is
    ///   consulted **first**, and a resolved closure is entered in this machine.
    /// * [`crate::RunOutcome::Suspend`] is only reachable from a blocking-aware run
    ///   entry; a non-blocking one folds it into the typed [`crate::error::suspended`]
    ///   error.
    ///
    /// The default body forwards to [`apply_fun`](ExecutionContext::apply_fun) and
    /// wraps the answer in [`FunOutcome::Return`], so every existing implementor
    /// compiles and behaves byte-identically (an unimplemented `apply_fun` still
    /// answers the typed [`crate::error::unsupported`]).
    fn apply_fun_outcome(&mut self, fun: Eterm, args: &[Eterm]) -> Result<FunOutcome> {
        self.apply_fun(fun, args).map(FunOutcome::Return)
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
    /// machine" — which keeps the
    /// [`apply_fun_outcome`](ExecutionContext::apply_fun_outcome) fallback (fun kinds
    /// with no local label: external `fun M:F/A` and the `-eval` evaluator's own
    /// closure), so every existing host is unchanged. That fallback is
    /// **outcome-aware**: its default forwards to the value-returning
    /// [`apply_fun`](ExecutionContext::apply_fun), and a host that overrides it may
    /// answer [`FunOutcome::Suspend`] to park the machine on the `call_fun`/`call_fun2`
    /// instruction ([`Step::Suspend`](crate::Step),
    /// [`crate::RunOutcome::Suspend`]) instead of failing. Such a host must **continue**
    /// the pending application when the instruction is re-executed, never restart it.
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

    /// Serve a **local** (same-module) call as a BIF instead of entering the
    /// callee's own code — the *local-call* seam.
    ///
    /// The OTP compiler emits a **local** `call`/`call_only`/`call_last` for a
    /// same-module function: a self-qualified call to an *exported* function is a
    /// `call_ext`, but a call to a non-exported one stays local. A local call
    /// therefore consults **no** BIF seam today — the engine resolves the label and
    /// enters that code — so a module's non-exported functions can never reach
    /// [`call_bif`](ExecutionContext::call_bif). That is exactly the shape ERTS uses
    /// for a built-in/static NIF library (`lib/kernel/src/prim_tty.erl`: the exported
    /// Erlang wrappers run in-module and chain **locally** into the non-exported NIF
    /// stubs `tty_create/1`/`read_nif/3`/…), and this seam is the only way a host can
    /// serve those stubs: the three local call arms
    /// (`call`/`call_only`/`call_last`) consult it before jumping, in the interpreter
    /// **and** on the native baseline tier.
    ///
    /// * `Ok(true)` — "I serve this `module:function/arity`": the engine marshals the
    ///   instruction's own register arguments (`x(0..arity)`) into a
    ///   [`call_bif`](ExecutionContext::call_bif) call exactly as a `call_ext` would,
    ///   then applies the arm's own frame discipline to the returned value (a
    ///   non-tail `call` continues at the next instruction; a tail `call_only`/
    ///   `call_last` follows the frame's existing continuation, `call_last` popping
    ///   its `deallocate` frame first). **No callee code runs**, so the host must be
    ///   able to produce the result from the arguments alone.
    /// * `Ok(false)` — "enter the callee in this machine": today's behaviour, an
    ///   in-machine jump to the local label (with the arm's continuation store, its
    ///   `call_last` frame pop and its contract-A.8 `call` event). **This is the
    ///   default**, and it is byte-identical to the tier before the seam existed.
    ///
    /// The engine recovers `module:function/arity` from the resolved callee's own
    /// function entry and atom table; when either name cannot be resolved it does not
    /// call this method at all and takes the in-machine jump, so a host is never
    /// asked about a fabricated name.
    ///
    /// An `Err` propagates out of the run exactly like any other engine failure — the
    /// raise funnel's frame discipline included, so a raise lands in a live
    /// `try`/`catch` when there is one.
    ///
    /// The default body returns `Ok(false)`, so every existing host compiles and
    /// behaves byte-identically (it pays only the call).
    ///
    /// # The engine consults this **once per distinct callee per run**
    ///
    /// A local call is on the hottest path a module has, and OTP charges **no**
    /// reduction for an intra-module call, so paying a virtual call plus a name
    /// comparison per executed `call` would price an engine artefact into every
    /// workload (`Machine::local_call_seam` does the memoizing).
    /// The answer is therefore cached per `(module image address, function index)`
    /// for the life of a run: a repeat local call costs one indexed load, not a
    /// consultation.
    ///
    /// **The answer must be a pure function of `(module, function, arity)`** — that
    /// purity is what makes the cache sound. A host whose answer set changes
    /// **mid-run** must say so through
    /// [`Machine::invalidate_local_call_verdicts`](crate::Machine); the engine
    /// already arms that invalidation itself when it dispatches
    /// `erlang:load_nif/2`, the one documented way a stub's verdict flips mid-run.
    /// A verdict must never be baked into **compiled** code: `JitBackend::compile`
    /// receives no `ExecutionContext` and the compiled-block cache is shared across
    /// hosts and runs, so the memo lives on the `Machine` (per run), never on the
    /// artifact.
    ///
    /// [`call_bif`]: ExecutionContext::call_bif
    fn resolve_local_call(&mut self, module: &str, function: &str, arity: u32) -> Result<bool> {
        let _ = (module, function, arity);
        Ok(false)
    }

    /// Resolve a **record definition** that is not the running module's own — the
    /// host record-table seam behind `put_record`'s **create** form.
    ///
    /// A native record's create form (`Src` is the literal `nil`,
    /// `emu/ops.tab:1648`) must find the definition *before* it can build anything.
    /// An **atom** `Id` names a record of the running module, whose definition is
    /// right there in [`Module::record`] — no seam, no table. A **`{Module,Name}`**
    /// `Id` naming **another module** is exactly what C answers from the code
    /// server's record table (`erts_record_find_entry`, `erl_record.c:376`), and
    /// that table belongs to the host: it is the code/data owner, and this crate
    /// never loads modules.
    ///
    /// `owner`/`name` are the two atom **values** the `Id` tuple carries (already
    /// resolved from the operand); the host is expected to look them up in its own
    /// loaded-module record table.
    ///
    /// * `Ok(Some(def))` — the host resolved `{owner, name}`. **The `is_exported`
    ///   flag is part of the answer, not a filter**: the host must return the
    ///   definition it found *even when* `def.is_exported == false`, because C then
    ///   raises `badrecord` with the `Id` term itself as the reason
    ///   (`erl_record.c:401-409`) instead of creating the record. Returning `None`
    ///   for a non-exported hit would silently turn that raise into the typed
    ///   capability error below, so the engine asks for the definition **and** the
    ///   flag and decides both branches itself.
    /// * `Ok(None)` — the host cannot answer: it has no record table, or the
    ///   defining module is not (and cannot be) loaded. The family then keeps its
    ///   honest typed `error::unsupported("… create: no record definition table for
    ///   another module")`. A definition is **never guessed**, so a host that does
    ///   not implement this seam keeps today's behaviour byte-for-byte
    ///   ([`MinimalContext`] and every mock/test host are unchanged).
    ///
    /// # Atom-namespace contract
    ///
    /// [`Atom`] is a **table-local index**. The returned definition's `name` and
    /// every `fields[i].name` must therefore be nameable in the **running process's**
    /// atom namespace — the namespace [`atoms`](ExecutionContext::atoms) answers
    /// from (the runtime's ONE live table, or the running module's snapshot for a
    /// host that does not override it). The family re-resolves every one of those
    /// names through that live namespace before writing it into a term, and a name
    /// the namespace cannot spell is a typed error: an index from a foreign table is
    /// never written into a term, where it would silently name a *different* atom.
    /// Default values are ordinary terms and are stored verbatim.
    ///
    /// # Deviation from C (recorded, never silent)
    ///
    /// C's create form has a `code:ensure_loaded/1` trap path
    /// (`erl_create_native_record_jit`, `erl_record.c:412-507`, plus its
    /// continuation `erl_finish_create_record_jit`, `:509-565`): a module that is not
    /// loaded yet is loaded, and the create is retried. This runtime has no code
    /// server on the execution path, so that "load, then retry" half is the host's
    /// `Ok(None)` — the host answers only for modules it can already resolve.
    ///
    /// The default body returns `Ok(None)`, so this seam is **purely additive**:
    /// every existing implementor compiles and behaves byte-identically.
    fn foreign_record_definition(
        &mut self,
        owner: Atom,
        name: Atom,
    ) -> Result<Option<RecordDefinition>> {
        let _ = (owner, name);
        Ok(None)
    }
    /// Resolve an interned atom id to its name, preferring the host's **live**
    /// atom namespace over the running module's load-time snapshot.
    ///
    /// [`Module::atom_name`](beam_loader::Module::atom_name) indexes the module's
    /// *owned load-time snapshot* of the shared atom table, so a module loaded
    /// early cannot name an atom interned afterwards (its global id is beyond the
    /// snapshot). The engine resolves a **runtime atom value** (a variable
    /// `M`/`F` in `apply(M,F,A)` / `M:F(...)`) through this seam, so a host that
    /// owns the live shared namespace can answer where the module's snapshot
    /// cannot.
    ///
    /// The default body is this module's own table only
    /// ([`Module::atom_name`](beam_loader::Module::atom_name)), so every existing
    /// implementor — including every test context — is byte-identical.
    ///
    /// The name is returned **owned** so the caller can hold both resolved names
    /// across subsequent `&mut` host calls (e.g.
    /// [`resolve_external_call`](ExecutionContext::resolve_external_call) and
    /// [`call_bif`](ExecutionContext::call_bif), both of which take
    /// `&mut dyn ExecutionContext`); a borrowed `&str` tied to `&self` would
    /// conflict with that mutable borrow. The allocation is only on the
    /// variable-`apply` path, never a hot path.
    fn atom_name(&self, module: &Module, atom: Atom) -> Option<String> {
        module.atom_name(atom).map(str::to_owned)
    }

    /// The atom **namespace** a comparison must use.
    ///
    /// Every term comparison the engine performs on the execution path — the
    /// `test`/`is_*` comparison kinds and `select_val`, the native baseline tier's
    /// comparison helper, and the map-key `lookup`/`position` equality — resolves a
    /// comparison's atom names through this seam. The default body is the running
    /// module's own **load-time snapshot** ([`Module::atom_table`]), which is
    /// byte-identical to the pre-seam behaviour.
    ///
    /// A host that owns **one** shared, live atom namespace (the runtime) must
    /// override this so guard/`select_val`/map-key comparisons order atoms by
    /// **name** exactly as the `<`/`==` BIFs do. The bug it closes: `beam_term`'s
    /// `erts_cmp_atoms` compares atom *names*, but falls back to the raw atom id when
    /// a name is unknown to the supplied table — and a module loaded early cannot
    /// name an atom interned into the shared namespace *afterwards* (its global id is
    /// beyond the snapshot). A comparison of two such atoms then silently ordered by
    /// id instead of name, so `{module_info,0} < {G,1}` (with `G` interned later)
    /// took the wrong clause in `sofs:restrict_n/5`.
    ///
    /// The returned reference is borrowed from `self` (and `module`), so a caller
    /// **must take it after** its operand reads and **never hold it across** a
    /// `&mut dyn ExecutionContext` call (e.g. the map writers reserve through `ctx`
    /// and only then read the namespace).
    fn atoms<'m>(&'m self, module: &'m Module) -> &'m AtomTable {
        &module.atom_table
    }
    /// The **reduction hook**: charge `n` reductions for the work the engine is
    /// about to execute.
    ///
    /// A BEAM process is pre-emptible because the runtime prices work in
    /// *reductions* and checks its budget at known points. The engine's **only**
    /// other charging point is [`ensure_heap`](ExecutionContext::ensure_heap)
    /// (reached by `test_heap`/`allocate_heap`), which a tight **non-allocating**
    /// loop — the one shape that can monopolise a scheduler — never touches. This
    /// hook closes that gap at each tier's own granularity:
    /// [`Machine::drive`](crate::machine::Machine) calls it **once per interpreted
    /// instruction** with `n = 1`, **before** the instruction's body runs, and the
    /// native tiers' shared driver (`Tier::BaselineJit` and `Tier::Cranelift` both
    /// run on it) calls it **once per driver-loop iteration** — a block entry, or the
    /// per-instruction step it takes for an address no compiled block covers — also
    /// with `n = 1`, **before** the block is entered. A host therefore prices
    /// instructions on the interpreter and block entries on the native tiers, rather
    /// than heap admissions on either; a natively *linked* local tail-call chain
    /// never returns to the driver and is priced once for the whole chain.
    ///
    /// The default body is a **no-op** ([`Ok`]) — deliberately *not* a typed error:
    /// a host that does not pre-empt simply ignores the hook, so every existing
    /// implementor compiles and behaves byte-identically (it pays only the call).
    /// Returning `Err` propagates out of the run exactly like any other engine
    /// failure, so a host that has exhausted its budget returns **its own** typed
    /// exhaustion error. Because the charge precedes the work, that error leaves the
    /// instruction (or the block) **not applied** and the machine parked at its
    /// `(func, pc)` ([`Machine::current_code`](crate::machine::Machine::current_code)),
    /// so a host that pre-empts *by failing* continues the process by re-entering
    /// through
    /// [`Machine::resume_outcome`](crate::machine::Machine::resume_outcome) — which
    /// applies that instruction **exactly once** — not by
    /// [`Machine::run_export`](crate::machine::Machine::run_export).
    ///
    /// The engine also counts the same charge in
    /// [`Machine::reductions`](crate::machine::Machine::reductions) — the slice's
    /// reduction count, readable *after* the slice returns — so a host that
    /// prefers to poll rather than hook can; see that accessor for the per-tier unit.
    fn reduce(&mut self, n: u32) -> Result<()> {
        let _ = n;
        Ok(())
    }

    /// Cheap reduction-hook gate: does [`reduce`](ExecutionContext::reduce) need to
    /// be called at all for the process currently running?
    ///
    /// [`Machine::drive`](crate::machine::Machine) charges **one reduction per
    /// interpreted instruction** (before the instruction's effect) and the native
    /// tiers' shared driver charges **one per driver-loop iteration** (a block
    /// entry, before the block is entered), and each charge is also offered to the
    /// host through [`reduce`](ExecutionContext::reduce). The
    /// hook call is a
    /// `&mut dyn ExecutionContext` vtable crossing the interpreter cannot elide, so
    /// the driver latches this answer **once per run** into the
    /// `Machine::reduce_on` flag and then skips the hook call entirely on a run
    /// whose host declares it does not need it — the per-charge hot path pays a
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
    /// still bumped on every charge (one per interpreted instruction, or one per
    /// driver-loop iteration on the native tiers), so a host that polls
    /// [`Machine::reductions`](crate::machine::Machine::reductions) after the slice
    /// observes the exact same count either way; only the hook *call* is skipped.
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

/// An **outcome-aware** BIF hook: maps `(module, function, arity, args)` to a
/// [`BifOutcome`], so a hook may answer [`BifOutcome::Suspend`].
///
/// This is the pluggable body of
/// [`ExecutionContext::call_bif_outcome`]: it is consulted **instead of** the plain
/// [`BifHook`] when it is installed (see
/// [`MinimalContext::with_bif_outcome_hook`]). [`MinimalContext::new`] leaves it
/// unset, so `call_bif_outcome` stays the trait default — forward to
/// [`call_bif`](ExecutionContext::call_bif) and wrap the value.
pub type BifOutcomeHook =
    Box<dyn FnMut(&str, &str, u32, &[Eterm]) -> Result<BifOutcome>>;
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

/// A local-call resolver: the pluggable body of
/// [`ExecutionContext::resolve_local_call`].
///
/// Answers whether the host serves a **same-module** `call`/`call_only`/`call_last`
/// target as a BIF instead of letting the engine enter its code: `true` = serve it
/// (the engine dispatches `(module, function, arity)` through
/// [`call_bif`](ExecutionContext::call_bif)), `false` = the in-machine jump (the trait
/// default). [`MinimalContext::new`] leaves it unset, so `resolve_local_call` stays the
/// trait's `Ok(false)` default.
pub type LocalCallResolver = Box<dyn FnMut(&str, &str, u32) -> Result<bool>>;

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
/// arena is discarded).
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
/// arena's lifetime.
///
/// # The two reclaim tiers (never automatic)
///
/// Nothing here reclaims *by itself*: the growth path above never truncates, resets or
/// reallocates a region, so `alloc_words` hands out words until the arena is dropped. Two
/// **caller-invoked** doors exist beside it (see `src/context/reclaim.rs`), and each one
/// makes a liveness claim the arena cannot check:
///
/// * [`rewind`](ContextHeap::rewind) moves the frontier's bump pointer back to a
///   [`HeapMark`] and drops every later region — sound only when nothing reachable lies
///   at or above the mark;
/// * [`reclaim`](ContextHeap::reclaim) **copies** the live graph into one fresh region
///   through [`beam_gc`]'s relocator and drops every old region — sound only when the
///   forwarded root set really is the whole live set.
///
/// A region's `Drop` (the arena being dropped, or a rewind/reclaim dropping it) is the
/// **only** path that returns the carrier's memory: [`ReservedHeap::reset`] merely moves
/// the bump pointer back and frees nothing.
struct ContextHeap {
    /// Every region handed out, oldest first. The **last one is the allocation frontier**
    /// (`regions.len() - 1` is the current-region index). The `Vec` grows on allocation
    /// and shrinks **only** through the two caller-invoked reclaim tiers
    /// ([`rewind`](ContextHeap::rewind) truncates it, [`reclaim`](ContextHeap::reclaim)
    /// replaces it with a single survivor); nothing else frees a region, so no word moves
    /// before then.
    regions: Vec<ReservedHeap>,
    /// The first region's reservation geometry, and the floor every later region's
    /// reservation doubles up from.
    reserve_words: usize,
    /// The words committed up front in a region.
    commit_words: usize,
    /// The arena's **mark epoch**, bumped by every successful
    /// [`rewind`](ContextHeap::rewind) and [`reclaim`](ContextHeap::reclaim).
    ///
    /// A [`HeapMark`] carries the epoch it was taken in, so a mark taken *before* a rewind
    /// is refused afterwards **even when** its region index, base address and used figure
    /// happen to line up with the new frontier again — the words written in between were
    /// discarded, so a silent accept would let the caller rewind over live terms. `0` is a
    /// fresh arena (nothing has been rewound or reclaimed yet), which is also
    /// [`HeapMark::EMPTY`]'s epoch.
    epoch: u64,
    /// The words the two reclaim tiers have taken back out of the arena's live accounting,
    /// summed over the arena's whole life. `stats()` adds this to `Σ regions[i].used()` so
    /// that [`ContextHeapStats::used_words`] stays *cumulative demand* across a rewind.
    released_words: usize,
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
            epoch: 0,
            released_words: 0,
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
            epoch: 0,
            released_words: 0,
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

    /// The heap's **census**: the region count, the summed reservation/commit/use/peak
    /// figures and the frontier region's own two numbers.
    ///
    /// Read-only and allocation-free (`O(regions)`), so it is safe to call between any
    /// two allocations — it never allocates, never moves a word and never changes the
    /// allocation policy. The public door is
    /// [`MinimalContext::heap_stats`].
    ///
    /// `used_words` stays **cumulative demand** across a reclaim
    /// ([`rewind`](ContextHeap::rewind)/[`reclaim`](ContextHeap::reclaim)) because the
    /// words the tier took back are counted in `released_words` and added on;
    /// `live_words` is the honest "words handed out right now" figure that *does* fall.
    fn stats(&self) -> ContextHeapStats {
        let mut stats = ContextHeapStats {
            regions: self.regions.len(),
            released_words: self.released_words,
            ..ContextHeapStats::default()
        };
        for region in &self.regions {
            stats.reserved_words += region.reserved_words();
            stats.committed_words += region.committed_words();
            stats.live_words += region.used();
            stats.peak_words += region.peak_used();
        }
        stats.used_words = stats.live_words + self.released_words;
        if let Some(frontier) = self.regions.last() {
            stats.frontier_used = frontier.used();
            stats.frontier_available = frontier.available();
        }
        stats
    }

    /// The **mark** of the current allocation frontier: its region index, its base
    /// address, its [`used`](ReservedHeap::used) figure and the arena's current mark
    /// epoch.
    ///
    /// [`HeapMark::EMPTY`] while no region has been reserved (a heap that has never
    /// allocated), so an empty mark is the "nothing was handed out yet" value a caller
    /// can compare against later.
    ///
    /// The mark is a **pure observation** — it records where the frontier stands, and it
    /// makes nothing safe: see [`HeapMark`] for what a rewind would additionally require.
    fn mark(&self) -> HeapMark {
        match self.regions.len() {
            0 => HeapMark::EMPTY,
            len => {
                let frontier = &self.regions[len - 1];
                HeapMark {
                    region: Some(len - 1),
                    base: frontier.base() as usize,
                    used: frontier.used(),
                    epoch: self.epoch,
                }
            }
        }
    }

    /// Whether `mark` still names **this** heap's current frontier region, was taken in
    /// the arena's current epoch, and was taken at or before the frontier's current
    /// [`used`](ReservedHeap::used) figure.
    ///
    /// `true` means the region the mark was taken in has not been replaced, no reclaim has
    /// run since it was taken, and the bump pointer has not gone backwards — the structural
    /// precondition for the mark to be usable at all. `false` means the mark is **stale**
    /// (the region it named has since been replaced by a fresh frontier region, so its words
    /// are still live but no longer the allocation frontier), **forward** (it claims more
    /// words than the frontier has handed out — a mark from another heap, or a corrupted
    /// one) or **pre-reclaim** (a [`rewind`](ContextHeap::rewind)/[`reclaim`](ContextHeap::reclaim)
    /// has run since it was taken, so the words above it are gone).
    ///
    /// A [`HeapMark::EMPTY`] mark matches **only** a heap that still has no region *and* has
    /// never been rewound or reclaimed (its epoch is `0`): once a region exists, "no region
    /// existed yet" says nothing about the frontier that appeared, so the conservative
    /// answer is `false`.
    ///
    /// This is a **precondition check, not a permission**: a matching mark still does not
    /// make a rewind sound (see [`HeapMark`]).
    fn mark_matches(&self, mark: HeapMark) -> bool {
        if mark.epoch != self.epoch {
            return false;
        }
        match mark.region {
            None => self.regions.is_empty(),
            Some(index) => match self.regions.get(index) {
                // The index must be the CURRENT frontier (a region that was replaced is
                // stale even though it is still live), and the base must be that
                // region's — which is what catches a replaced region even if an index
                // happened to coincide.
                Some(region) => {
                    index + 1 == self.regions.len()
                        && region.base() as usize == mark.base
                        && mark.used <= region.used()
                }
                None => false,
            },
        }
    }

    /// Visit every region's **written prefix** (`[0, used)`) oldest-first, as a mutable
    /// word slice of exactly `used()` words.
    ///
    /// A region whose [`used`](ReservedHeap::used) is zero is skipped (an empty prefix
    /// carries no words, and a zero-length slice would need no valid base); every other
    /// region's slice spans exactly the words the bump pointer has passed, never the
    /// committed-but-unwritten tail.
    ///
    /// # What the visitor may do with the slice
    ///
    /// **It must not interpret a word as a term.** [`alloc_words`](ExecutionContext::alloc_words)
    /// hands out *uninitialised* words and the caller writes them, so the prefix is only
    /// as initialised as the callers' writes left it: the words are raw payload (a
    /// bitstring's bytes, a float's mantissa, a tuple's element words, a cons pair) and
    /// only a reachability walk knows which of them are term slots. The slice is for a
    /// **census or a reachability walk**, never for reading a term — use the frozen
    /// [`RootSet`] door ([`Machine::with_roots`](crate::machine::Machine::with_roots)) for
    /// anything that needs to interpret words.
    ///
    /// It must also **not keep** the slice: the borrow ends when `visit` returns, and a
    /// later allocation may extend the same region.
    ///
    /// The `&mut self` borrow is what makes the slice exclusive: no allocation, and hence
    /// no new word handed out, can run while one is being visited.
    fn for_each_region(&mut self, mut visit: impl FnMut(&mut [Eterm])) {
        for region in &mut self.regions {
            let used = region.used();
            if used == 0 {
                continue;
            }
            let base = region.base();
            // SAFETY: `base` is the start of the allocator-provided reservation and
            // `used <= committed <= reserved` words, so the range is wholly inside this
            // region's exclusively-owned block; `&mut self` makes this the sole access to
            // those words for the slice's lifetime, and `used` bounds the slice to the
            // prefix the bump pointer has passed (never the uncommitted reservation).
            // The words themselves are only as initialised as the callers' own writes
            // left them, which is why the visitor is forbidden from interpreting them as
            // terms (see the docs above) — the same contract
            // `beam_alloc::Heap::used_words_mut` carries.
            let words = unsafe { core::slice::from_raw_parts_mut(base, used) };
            visit(words);
        }
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

/// A **census** of a [`MinimalContext`]'s region heap: what it has reserved, committed,
/// handed out and peaked at, region by region.
///
/// Returned by [`MinimalContext::heap_stats`]. Every word figure is exact for a 64-bit
/// `Eterm` when converted with `size_of::<Eterm>()`; nothing here is sampled or
/// estimated, and reading it changes no allocation state.
///
/// # `used_words` is CUMULATIVE DEMAND, not live memory
///
/// Nothing here reclaims by itself: allocation grows the arena and never frees, shrinks or
/// resets a region (see [`ContextHeap`]'s docs — both reclaim tiers are explicit,
/// caller-invoked operations). So [`used_words`](ContextHeapStats::used_words) is the
/// **total** number of words ever handed out across the whole life of the context. It is a
/// lower bound on the memory the arena holds and an upper bound on nothing: a run that
/// allocated and discarded terms leaves `used_words` at its high-water figure forever, so
/// this is **not** a live-memory reading and must not be compared with a collector's heap
/// size. A reclaim ([`MinimalContext::heap_rewind`]/[`MinimalContext::reclaim_heap`]) keeps
/// it unchanged by definition: the words it takes back are counted in
/// [`released_words`](ContextHeapStats::released_words) and added on.
///
/// [`live_words`](ContextHeapStats::live_words) is the honest counterpart — the words the
/// regions hold *right now* — and it does fall on a reclaim.
///
/// # Why the arena reports an unbounded `heap_available`
///
/// [`ExecutionContext::heap_available`] answers `usize::MAX` for this context, and this
/// census is the honest counterpart of that: the region heap **reserves lazily and never
/// refuses** — an exhausted reservation is followed by a fresh, doubled one — so there is
/// no capacity to report against, only what has been used so far. A host that needs a
/// real admission bound must impose one (a `max_heap_size` check, a collection trigger),
/// because the engine's `ensure_heap` is a no-op here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContextHeapStats {
    /// How many regions are currently held (the first one lazily, on first
    /// allocation; every later one when the previous **reservation** was exhausted). It
    /// falls again only when a reclaim drops regions. `0` means the context holds no word.
    pub regions: usize,
    /// The reservation's total size in `Eterm` words, summed over the regions currently
    /// held. This is **address space**, not resident memory: a reservation is one
    /// anonymous (lazily-faulted) `mmap` per region, released when that region is dropped.
    pub reserved_words: usize,
    /// The total committed slice in `Eterm` words, summed over the regions currently held
    /// — the words the bump allocators may hand out without growing a region first.
    pub committed_words: usize,
    /// The total number of words **ever handed out**, summed over every region the arena
    /// has held across its whole life.
    ///
    /// **Cumulative demand, not live memory**: this figure only ever grows and includes
    /// terms the engine has already discarded *and* the words a reclaim has taken back (see
    /// the type docs). Read [`live_words`](ContextHeapStats::live_words) for the current
    /// figure.
    pub used_words: usize,
    /// The words the regions **currently hold** — `Σ regions[i].used()` — i.e. the words
    /// handed out and not since released.
    ///
    /// This is the one figure that falls on a reclaim; `used_words - live_words` is
    /// [`released_words`](ContextHeapStats::released_words).
    pub live_words: usize,
    /// The words a reclaim ([`MinimalContext::heap_rewind`] or
    /// [`MinimalContext::reclaim_heap`]) has taken back out of the arena, summed over the
    /// arena's whole life. `0` until the first successful reclaim, and it only ever grows
    /// (a reclaim that finds nothing to do does not run).
    pub released_words: usize,
    /// The summed per-region high-water mark of [`used`](ContextHeapStats::live_words) over
    /// the regions **still held**. Equal to
    /// [`live_words`](ContextHeapStats::live_words) whenever no region was reset, which is
    /// every state that has not been through a [`MinimalContext::heap_rewind`]; a rewind
    /// lowers `live_words` while leaving the surviving frontier's peak alone, and dropping a
    /// region drops its peak with it.
    pub peak_words: usize,
    /// The frontier (newest) region's [`used`](ContextHeapStats::live_words) figure, or
    /// `0` when no region is held. This is the figure a [`HeapMark`] records.
    pub frontier_used: usize,
    /// The words still available in the frontier region's committed slice, or `0` when no
    /// region is held. An allocation larger than this asks the frontier to raise its
    /// committed limit in place (and only a request past the whole reservation pushes a
    /// fresh region).
    pub frontier_available: usize,
}

/// A **mark** of a [`MinimalContext`]'s allocation frontier: the region it names, that
/// region's base address, how many words it had handed out and the arena's mark epoch.
///
/// Returned by [`MinimalContext::heap_mark`] and checked by
/// [`MinimalContext::heap_mark_matches`]; [`MinimalContext::heap_rewind`] is the door that
/// acts on one. It is a `Copy` value with private fields — the only ways to obtain one are
/// [`HeapMark::EMPTY`] and `heap_mark()` — so a mark always describes a state this heap
/// actually reached.
///
/// # What a mark is for, and what it is NOT
///
/// The mark exists so a caller can later ask the **typed question** "is the region I
/// recorded still the allocation frontier, and has the bump pointer only moved
/// forward?" — the structural precondition any rewind needs. It carries the region's
/// **base address** as well as its index because a region can be *replaced* (an
/// exhausted reservation is followed by a fresh, larger one whose words never overlap
/// the old region's), and an address is the one property a replaced region cannot
/// preserve. That is the same discipline `beam_rt`'s `-eval` scratch store applies when
/// it windows its own heap.
///
/// A mark makes **nothing safe**. A rewind (moving the frontier's bump pointer back to
/// `used()`) is sound only when the caller has independently proved that **nothing
/// reachable lies at or above the mark** — because every word above it is, by
/// construction, either unreachable *and* allowed to be overwritten, or it is garbage.
/// The arena cannot check that: it cannot tell a term slot from raw payload bytes
/// (telling them apart needs term-structure knowledge this heap seam deliberately does
/// not have — the same reason it never calls
/// [`ReservedHeap::relocate`](beam_alloc::ReservedHeap::relocate)). So the mark itself is
/// **inert**: it carries the observation and the typed predicate, and the reachability
/// proof — a walk over the machine's roots
/// ([`Machine::for_each_root`](crate::machine::Machine::for_each_root)) and the heap's
/// regions ([`MinimalContext::for_each_heap_region`]) — is the caller's, which is why
/// [`MinimalContext::heap_rewind`] is a *caller* decision rather than an arena one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapMark {
    /// The index of the region the mark was taken in (`None` = no region existed).
    region: Option<usize>,
    /// The marked region's base address, as an address value (not a pointer, so a mark
    /// stays a plain `Send + Sync` value and comparing two marks compares addresses).
    base: usize,
    /// The marked region's `used()` figure at the moment of the mark.
    used: usize,
    /// The arena's **mark epoch** at the moment of the mark — bumped by every successful
    /// rewind/reclaim, so a mark taken before one is refused afterwards even when its
    /// index, base and used figure happen to line up again.
    epoch: u64,
}

impl HeapMark {
    /// The mark of a heap that has **not reserved a region yet**.
    ///
    /// It matches only a heap that still has no region ([`MinimalContext::heap_mark_matches`]):
    /// once a region exists, "nothing has been handed out" is no longer a state the
    /// frontier can be in, so an empty mark is reported as not matching rather than
    /// silently accepting a rewind to a state the heap cannot be rewound to. Its epoch is
    /// `0`, the epoch of a fresh arena, so it also stops matching once the arena has been
    /// through a rewind or a reclaim.
    ///
    /// [`MinimalContext::heap_rewind`] accepts an epoch-matching empty mark as the
    /// **arena-reset** door: it drops every region the arena holds.
    pub const EMPTY: HeapMark = HeapMark {
        region: None,
        base: 0,
        used: 0,
        epoch: 0,
    };

    /// How many words the marked region had handed out (`0` for [`HeapMark::EMPTY`]).
    pub fn used(&self) -> usize {
        self.used
    }

    /// The index of the region the mark was taken in, or `None` for [`HeapMark::EMPTY`]
    /// (a heap that held no region when the mark was taken).
    pub fn region(&self) -> Option<usize> {
        self.region
    }

    /// The marked region's **base address** (`0` for [`HeapMark::EMPTY`]).
    ///
    /// The one property a *replaced* region cannot preserve, which is what makes a stale
    /// mark detectable even when its index coincides.
    pub fn base(&self) -> usize {
        self.base
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
/// # Lifetime contract: a term belongs to the context that built it
///
/// The arena owns the **only** copy of every term a run allocates in it, so an `Eterm`
/// a run produced — the value a `RunOutcome::Returned` carries, and every register and
/// stack word read back from the `Machine` — stays readable exactly as long as the
/// `MinimalContext` that produced it lives. A `Machine` does **not** own the heap.
/// Dropping the context releases its carrier blocks to `beam_alloc`'s process-global
/// segment pool, and the next arena (another tier's context, or a concurrently running
/// test's) reuses that memory and writes its own words over it, so reading a term after
/// its context was dropped is a use-after-free whose symptom is a **wrong value** or the
/// term-integrity abort `beam_term::ops::class: invalid boxed header subtag` — never a
/// typed error, and never reliably reproducible (it needs the recycled slot to hold a
/// non-header word).
/// A test that compares terms produced by **two** runs (two tiers, an eager and a
/// parking host) must therefore keep both hosts alive until the comparison, or compare
/// inside each run's own scope.
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
///
/// **Record definitions**
/// ([`with_foreign_record_definitions`](MinimalContext::with_foreign_record_definitions) /
/// [`set_foreign_record_definitions`](MinimalContext::set_foreign_record_definitions))
/// may be installed to serve
/// [`foreign_record_definition`](ExecutionContext::foreign_record_definition) — the
/// `put_record` create form's other-module record table; without them the seam is
/// the trait's `Ok(None)` (`"no record table"`).
pub struct MinimalContext {
    hook: BifHook,
    /// An optional **outcome-aware** BIF hook (the
    /// [`ExecutionContext::call_bif_outcome`] body).
    ///
    /// When installed it is consulted **instead of** `hook`, so a test host can answer
    /// [`BifOutcome::Suspend`] and drive the engine's park/resume protocol. `None` out
    /// of the box, which keeps `call_bif_outcome` the trait default (forward to
    /// [`ExecutionContext::call_bif`] and wrap the value), byte-identical to the
    /// pre-seam engine.
    outcome_hook: Option<BifOutcomeHook>,
    /// An optional fun-application hook (the [`ExecutionContext::apply_fun`] body).
    apply_hook: Option<ApplyHook>,
    /// An optional cross-module-call resolver (the
    /// [`ExecutionContext::resolve_external_call`] body).
    external_resolver: Option<ExternalResolver>,
    /// An optional local-closure resolver (the
    /// [`ExecutionContext::resolve_local_fun`] body).
    local_fun_resolver: Option<LocalFunResolver>,
    /// An optional local-call resolver (the
    /// [`ExecutionContext::resolve_local_call`] body).
    local_call_resolver: Option<LocalCallResolver>,
    /// The host-supplied **record definitions** — the body of
    /// [`ExecutionContext::foreign_record_definition`], keyed by their exact
    /// `(owner, name)` atom values.
    ///
    /// Empty out of the box, so the seam is the trait default (`Ok(None)`,
    /// byte-identical to the pre-seam engine). A test that needs to model a host
    /// with a loaded-module record table installs entries through
    /// [`with_foreign_record_definitions`](MinimalContext::with_foreign_record_definitions)
    /// / [`set_foreign_record_definitions`](MinimalContext::set_foreign_record_definitions).
    foreign_records: Vec<(Atom, Atom, RecordDefinition)>,
    /// An optional mailbox (the body of the nine receive-seam methods).
    mailbox: Option<Box<dyn Mailbox>>,
    /// An optional shared atom namespace (the body of
    /// [`ExecutionContext::atoms`]).
    ///
    /// `None` out of the box, so the seam is the trait default (the running
    /// module's own load-time snapshot), byte-identical to the pre-seam behaviour.
    /// A test that needs to model the runtime's **live** shared namespace installs
    /// one through [`with_shared_atoms`](MinimalContext::with_shared_atoms) /
    /// [`set_shared_atoms`](MinimalContext::set_shared_atoms) — a namespace strictly
    /// longer than a module's snapshot, exactly as the runtime's is.
    shared_atoms: Option<AtomTable>,
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
            outcome_hook: None,
            apply_hook: None,
            external_resolver: None,
            local_fun_resolver: None,
            local_call_resolver: None,
            foreign_records: Vec::new(),
            mailbox: None,
            shared_atoms: None,
            heap: ContextHeap::new(),
        }
    }

    /// A context with the given **outcome-aware** BIF hook and the default (rejecting)
    /// plain BIF hook.
    ///
    /// The hook serves [`ExecutionContext::call_bif_outcome`], so it may answer
    /// [`BifOutcome::Suspend`] and a test can drive the engine's park/resume protocol
    /// (`RunOutcome::Suspend` → the host pumps → `Machine::resume_outcome` re-executes
    /// the instruction). Combine with an ordinary BIF hook via
    /// [`set_bif_hook`](MinimalContext::set_bif_hook) for the plain door.
    pub fn with_bif_outcome_hook(hook: BifOutcomeHook) -> MinimalContext {
        let mut ctx = MinimalContext::new();
        ctx.outcome_hook = Some(hook);
        ctx
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

    /// Install (or replace) the **outcome-aware** BIF hook — the body of
    /// [`ExecutionContext::call_bif_outcome`].
    pub fn set_bif_outcome_hook(&mut self, hook: BifOutcomeHook) {
        self.outcome_hook = Some(hook);
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

    /// A context with the given local-call resolver and the default (rejecting) BIF
    /// hook.
    ///
    /// Combine with a BIF hook via [`set_bif_hook`](MinimalContext::set_bif_hook).
    pub fn with_local_call_resolver(resolver: LocalCallResolver) -> MinimalContext {
        let mut ctx = MinimalContext::new();
        ctx.local_call_resolver = Some(resolver);
        ctx
    }

    /// Install (or replace) the local-call resolver.
    pub fn set_local_call_resolver(&mut self, resolver: LocalCallResolver) {
        self.local_call_resolver = Some(resolver);
    }

    /// A context with the given **record definitions** and the default (rejecting)
    /// BIF hook.
    ///
    /// Each entry is `(owner, name, definition)` and answers
    /// [`ExecutionContext::foreign_record_definition`] for an **exact** `(owner,
    /// name)` atom-value match, returning a [`clone`](Clone::clone) of the
    /// definition — the shape `put_record`'s create form consults for a
    /// `{Module,Name}` record id naming another module. The `is_exported` flag is
    /// carried through as given: an entry whose definition is *not* exported is a
    /// `badrecord` on the C's semantics, exactly like a real record table.
    ///
    /// Combine with a BIF hook via [`set_bif_hook`](MinimalContext::set_bif_hook).
    pub fn with_foreign_record_definitions(
        definitions: Vec<(Atom, Atom, RecordDefinition)>,
    ) -> MinimalContext {
        let mut ctx = MinimalContext::new();
        ctx.foreign_records = definitions;
        ctx
    }

    /// Install (or replace) the record definitions
    /// [`ExecutionContext::foreign_record_definition`] answers from. The default
    /// (never installed) is the trait's `Ok(None)` — "this host has no record
    /// table".
    pub fn set_foreign_record_definitions(
        &mut self,
        definitions: Vec<(Atom, Atom, RecordDefinition)>,
    ) {
        self.foreign_records = definitions;
    }
    /// Install (or replace) the mailbox.
    pub fn set_mailbox(&mut self, mailbox: Box<dyn Mailbox>) {
        self.mailbox = Some(mailbox);
    }

    /// A context with the given shared atom namespace and the default (rejecting)
    /// BIF hook.
    ///
    /// The namespace is what [`ExecutionContext::atoms`] returns, modelling the
    /// runtime's **live** shared atom table — typically a [`clone`](Clone::clone) of
    /// a module's load-time snapshot with one or more names interned afterwards, so
    /// its global ids run past the snapshot's length.
    ///
    /// Combine with a BIF hook via [`set_bif_hook`](MinimalContext::set_bif_hook).
    pub fn with_shared_atoms(atoms: AtomTable) -> MinimalContext {
        let mut ctx = MinimalContext::new();
        ctx.shared_atoms = Some(atoms);
        ctx
    }

    /// Install (or replace) the shared atom namespace [`ExecutionContext::atoms`]
    /// answers from. The default (never installed) is the running module's own
    /// load-time snapshot.
    pub fn set_shared_atoms(&mut self, atoms: AtomTable) {
        self.shared_atoms = Some(atoms);
    }

    /// The region heap's **[`census`](ContextHeapStats)**: region count, reserved /
    /// committed / live / cumulative / released / peak words and the frontier's own two
    /// figures.
    ///
    /// Infallible, allocation-free (`O(regions)`) and read-only: it changes no allocation
    /// state, so it is safe to call between any two allocations (and from a test that
    /// needs to assert the growth path). Read
    /// [`ContextHeapStats::used_words`] as **cumulative demand**, never as live memory —
    /// allocation never frees or shrinks a region, and a reclaim keeps the cumulative
    /// figure unchanged by moving the words it takes back into
    /// [`released_words`](ContextHeapStats::released_words).
    /// [`live_words`](ContextHeapStats::live_words) is the current figure, and the one
    /// that falls on a [`heap_rewind`](MinimalContext::heap_rewind) or
    /// [`reclaim_heap`](MinimalContext::reclaim_heap).
    pub fn heap_stats(&self) -> ContextHeapStats {
        self.heap.stats()
    }

    /// A **[`HeapMark`]** of the current allocation frontier.
    ///
    /// [`HeapMark::EMPTY`] until the first allocation reserves a region. The mark is
    /// inert by design: it lets a caller detect that the frontier region was replaced
    /// ([`MinimalContext::heap_mark_matches`]) and nothing more — a rewind additionally
    /// requires a proof that nothing reachable lies at or above the mark, which is the
    /// caller's to establish (see [`HeapMark`] and
    /// [`heap_rewind`](MinimalContext::heap_rewind)).
    pub fn heap_mark(&self) -> HeapMark {
        self.heap.mark()
    }

    /// Whether `mark` still names this context's **current allocation frontier**, taken
    /// at or before its current used figure and in the arena's current mark epoch.
    ///
    /// `false` for a **stale** mark (the region it recorded has since been replaced by a
    /// fresh frontier region), for a **forward** mark (it claims more words than the
    /// frontier has handed out), for a **pre-reclaim** mark (a
    /// [`heap_rewind`](MinimalContext::heap_rewind)/[`reclaim_heap`](MinimalContext::reclaim_heap)
    /// has run since it was taken) and for [`HeapMark::EMPTY`] once a region exists. This
    /// is the typed predicate a caller applies *before* trusting a mark; it grants no
    /// permission to rewind.
    pub fn heap_mark_matches(&self, mark: HeapMark) -> bool {
        self.heap.mark_matches(mark)
    }

    /// Visit every region's **written prefix** (`[0, used)` words) as a mutable slice,
    /// **oldest region first**.
    ///
    /// This is the census/reachability-walk door the region heap exposes: a caller that
    /// must survey every word the arena has ever handed out (a liveness proof for a
    /// [`heap_rewind`](MinimalContext::heap_rewind), a leak census, a memory-accounting
    /// sweep) walks the regions here instead of holding raw pointers. Regions with no
    /// words handed out are skipped.
    ///
    /// # `visit` MUST NOT interpret the words as terms, and MUST NOT keep the slice
    ///
    /// [`ExecutionContext::alloc_words`] hands out **uninitialised** words that the
    /// caller then writes, so a region's prefix holds a mixture of term words and raw
    /// payload (a bitstring's bytes, a float's mantissa, a tuple's element words) — only
    /// a walk that knows the term structure can tell them apart. `visit` therefore gets
    /// opaque words: it may compare them or read them as raw bits
    /// ([`Eterm::raw`](beam_term::Eterm::raw)), but dereferencing or interpreting one as a
    /// term is a bug. The slice borrow ends when `visit` returns;
    /// holding it (or a pointer derived from it) past that point is not supported, since a
    /// later allocation may extend the very same region.
    ///
    /// The `&mut self` borrow makes that safety real: no allocation can run — hence no
    /// word be handed out — while a region is being visited.
    pub fn for_each_heap_region(&mut self, visit: impl FnMut(&mut [Eterm])) {
        self.heap.for_each_region(visit);
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

    /// Answer from the installed **outcome-aware** hook when one was installed, else the
    /// trait default (forward to [`call_bif`](ExecutionContext::call_bif) and wrap the
    /// value) — the pre-seam behaviour exactly.
    fn call_bif_outcome(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<BifOutcome> {
        match self.outcome_hook.as_mut() {
            Some(hook) => hook(module, function, arity, args),
            None => self.call_bif(module, function, arity, args).map(BifOutcome::Return),
        }
    }
    /// Answer from the installed shared namespace, or the trait default (the running
    /// module's own snapshot) when none was installed.
    fn atoms<'m>(&'m self, module: &'m Module) -> &'m AtomTable {
        match &self.shared_atoms {
            Some(atoms) => atoms,
            None => &module.atom_table,
        }
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

    fn resolve_local_call(&mut self, module: &str, function: &str, arity: u32) -> Result<bool> {
        // Without an installed resolver the in-machine jump is the only route, so the
        // three local call arms behave exactly as they did before the seam existed.
        match self.local_call_resolver.as_mut() {
            Some(resolver) => resolver(module, function, arity),
            None => {
                let _ = (module, function, arity);
                Ok(false)
            }
        }
    }

    /// Answer from the installed record definitions, or keep the trait default
    /// (`Ok(None)`, "no record table") when none were installed.
    fn foreign_record_definition(
        &mut self,
        owner: Atom,
        name: Atom,
    ) -> Result<Option<RecordDefinition>> {
        Ok(self
            .foreign_records
            .iter()
            .find(|(o, n, _)| *o == owner && *n == name)
            .map(|(_, _, definition)| definition.clone()))
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

    // ---- the heap census, mark and region walk ----------------------------
    //
    // The measurement + mark primitives added on top of the region heap. They change no
    // allocation policy, no sizing and no reclaim, so each test asserts an *observation*
    // of an exactly-known growth sequence (driven with `ContextHeap::with_geometry`).

    #[test]
    fn heap_stats_is_empty_before_the_first_allocation() {
        let ctx = MinimalContext::new();
        assert_eq!(
            ctx.heap_stats(),
            ContextHeapStats::default(),
            "a context that has not allocated reports an all-zero census"
        );
        assert_eq!(ctx.heap_mark(), HeapMark::EMPTY, "no region, so no mark");
        assert_eq!(ctx.heap_mark().used(), 0);
        assert!(
            ctx.heap_mark_matches(HeapMark::EMPTY),
            "the empty mark matches the empty heap"
        );

        // A zero-word request reserves nothing (§ `alloc`'s documented dangling pointer),
        // so the census stays empty and the walk visits no region at all.
        let mut ctx = MinimalContext::new();
        let _ = ctx.alloc_words(0).expect("a zero-word request is Ok");
        assert_eq!(ctx.heap_stats(), ContextHeapStats::default());
        let mut regions = 0;
        ctx.for_each_heap_region(|words| {
            regions += 1;
            assert!(words.is_empty(), "an unallocated heap has no written prefix");
        });
        assert_eq!(regions, 0, "no region exists yet");

        // ...and the public doors work at the production geometry: one allocation makes
        // exactly one region, whose used figure is the cumulative demand.
        let _ = ctx.alloc_words(3).expect("the first region is reserved lazily");
        let stats = ctx.heap_stats();
        assert_eq!(stats.regions, 1);
        assert_eq!(stats.used_words, 3, "the three words just handed out");
        assert_eq!(stats.peak_words, 3, "nothing rewinds, so peak == used");
        assert_eq!(stats.frontier_used, 3);
        assert_eq!(
            stats.committed_words,
            stats.frontier_used + stats.frontier_available,
            "the frontier's two figures partition its committed slice"
        );
        assert!(
            stats.reserved_words >= stats.committed_words,
            "a reservation covers its committed slice"
        );
        assert!(
            !ctx.heap_mark_matches(HeapMark::EMPTY),
            "once a region exists, 'no region yet' no longer matches"
        );
        assert!(ctx.heap_mark_matches(ctx.heap_mark()));
    }

    #[test]
    fn heap_stats_reports_every_region_and_the_frontier() {
        // Reserve == commit == 4: every allocation of 4 words exactly fills a reservation,
        // so each one is followed by a fresh, doubled region — three regions after four
        // allocations, with no in-place growth in between.
        let mut heap = ContextHeap::with_geometry(4, 4);
        for _ in 0..4 {
            let _ = heap.alloc(4).expect("every region serves 4 words");
        }
        assert_eq!(heap.regions.len(), 3, "three regions were pushed");

        let stats = heap.stats();
        assert_eq!(stats.regions, 3);
        assert_eq!(
            stats.reserved_words,
            4 + 8 + 16,
            "the reservations, summed over every region (each fresh one doubles)"
        );
        assert_eq!(
            stats.committed_words,
            4 + 8 + 4,
            "each region commits its geometry (the last one starts over at 4)"
        );
        assert_eq!(stats.used_words, 4 + 8 + 4, "every region was filled");
        assert_eq!(stats.peak_words, stats.used_words);
        assert_eq!(
            stats.frontier_used, 4,
            "the frontier's own figure, not the total"
        );
        assert_eq!(stats.frontier_available, 0, "the frontier is full");

        // The sums are over the regions, stated region by region.
        assert_eq!(
            stats.reserved_words,
            heap.regions
                .iter()
                .map(ReservedHeap::reserved_words)
                .sum::<usize>()
        );
        assert_eq!(
            stats.committed_words,
            heap.regions
                .iter()
                .map(ReservedHeap::committed_words)
                .sum::<usize>()
        );
        assert_eq!(
            stats.used_words,
            heap.regions.iter().map(ReservedHeap::used).sum::<usize>()
        );
        assert_eq!(
            stats.peak_words,
            heap.regions.iter().map(ReservedHeap::peak_used).sum::<usize>()
        );
    }

    #[test]
    fn used_words_is_cumulative_across_regions() {
        // Reserve 8 but commit only 2, so the committed limit is raised IN PLACE three
        // times before the reservation is exhausted and a second region appears.
        let mut heap = ContextHeap::with_geometry(8, 2);
        let mut previous = heap.stats().used_words;
        let mut readings = Vec::new();
        for _ in 0..4 {
            let _ = heap.alloc(2).expect("the first region is grown in place");
            let stats = heap.stats();
            assert_eq!(stats.regions, 1, "an in-place grow adds no region");
            assert_eq!(
                stats.used_words,
                previous + 2,
                "each allocation adds exactly its words to the cumulative figure"
            );
            previous = stats.used_words;
            readings.push(stats.used_words);
        }
        assert_eq!(readings, vec![2, 4, 6, 8]);

        // The fifth request cannot be served by the exhausted reservation: a fresh region
        // becomes the frontier, and the OLD region's eight words stay counted.
        let _ = heap.alloc(2).expect("a fresh region serves the request");
        let stats = heap.stats();
        assert_eq!(stats.regions, 2);
        assert_eq!(stats.frontier_used, 2, "the fresh region has handed out 2 words");
        assert_eq!(
            stats.used_words, 10,
            "cumulative demand: 8 in the retired region + 2 in the frontier"
        );
        assert_eq!(
            stats.peak_words, stats.used_words,
            "no region is ever reset, so peak == used"
        );
        assert!(
            stats.used_words > stats.frontier_used,
            "used_words is CUMULATIVE DEMAND, so it exceeds what the frontier holds"
        );
        assert_eq!(
            stats.used_words,
            heap.regions.iter().map(ReservedHeap::used).sum::<usize>()
        );
    }

    #[test]
    fn heap_mark_reports_the_frontier_and_detects_a_replaced_region() {
        // Reserve 8, commit 4: two allocations of 2 grow the frontier in place, then 4
        // more fill the reservation, then the next request replaces the region.
        let mut heap = ContextHeap::with_geometry(8, 4);
        let _ = heap.alloc(2).expect("the committed slice serves 2 words");
        let mark = heap.mark();
        assert_eq!(mark.used(), 2, "the mark records the frontier's used figure");
        assert_eq!(heap.regions.len(), 1);
        assert!(heap.mark_matches(mark), "the mark names the live frontier");

        let _ = heap.alloc(2).expect("in-place growth serves the request");
        assert!(
            heap.mark_matches(mark),
            "the bump pointer only moved FORWARD, so the older mark still matches"
        );
        let _ = heap.alloc(4).expect("the rest of the committed slice");
        assert!(heap.mark_matches(mark), "still the same region");

        // The reservation is now exhausted, so the next request replaces the region: the
        // old mark names a region that is alive but no longer the allocation frontier.
        let _ = heap.alloc(2).expect("a fresh region serves the request");
        assert_eq!(heap.regions.len(), 2);
        assert!(
            !heap.mark_matches(mark),
            "a replaced region makes the old mark stale"
        );
        let fresh = heap.mark();
        assert_ne!(fresh, mark);
        assert_eq!(fresh.used(), 2, "the fresh frontier's own used figure");
        assert!(heap.mark_matches(fresh));

        // A FORWARD mark — one claiming more words than the frontier has handed out — is
        // rejected too, which is the second way a mark can be unusable.
        let mut forward = fresh;
        forward.used += 1;
        assert!(
            !heap.mark_matches(forward),
            "a mark past the bump pointer is not a state the heap reached"
        );
        let mut behind = fresh;
        behind.used -= 1;
        assert!(
            heap.mark_matches(behind),
            "a mark at or behind the bump pointer in the same region still matches"
        );

        // A mark never outlives its heap's identity check: a mark from another heap's
        // frontier (a different base) is rejected even at the same index and used figure.
        let mut other = ContextHeap::with_geometry(8, 4);
        let _ = other.alloc(2).expect("the other heap's first region");
        if other.regions[0].base() as usize != heap.regions[1].base() as usize {
            let other_mark = other.mark();
            assert!(
                !heap.mark_matches(other_mark),
                "a mark names a region by address, so another heap's mark is stale here"
            );
        }
    }

    #[test]
    fn for_each_heap_region_hands_only_the_written_prefix_oldest_first() {
        // Reserve 16, commit 8: the first allocation leaves 6 committed-but-unwritten
        // words, which the walk must NOT expose.
        let mut heap = ContextHeap::with_geometry(16, 8);
        let first = heap.alloc(2).expect("the committed slice serves 2 words");
        // SAFETY: `alloc(2)` returned 2 exclusively-owned writable words.
        unsafe {
            first.write(Eterm::from_raw(0xA0));
            first.add(1).write(Eterm::from_raw(0xA1));
        }
        // Fill the rest of the first region (14 more words) so the next request must
        // replace it.
        let rest = heap.alloc(14).expect("in-place growth fills the reservation");
        for i in 0..14 {
            // SAFETY: `rest` owns 14 exclusive words, so index `i` is in bounds.
            unsafe { rest.add(i).write(Eterm::from_raw(0xB0 + i)) };
        }
        assert_eq!(heap.regions.len(), 1);
        assert_eq!(heap.regions[0].used(), 16);
        // A fresh region, committed to 8 words but with only 2 handed out.
        let fresh = heap.alloc(2).expect("a fresh region serves the request");
        // SAFETY: `fresh` owns 2 exclusively-owned writable words.
        unsafe {
            fresh.write(Eterm::from_raw(0xC0));
            fresh.add(1).write(Eterm::from_raw(0xC1));
        }
        assert_eq!(heap.regions.len(), 2);
        assert_eq!(heap.regions[1].committed_words(), 8);
        assert_eq!(heap.regions[1].used(), 2);

        let mut visits: Vec<Vec<Eterm>> = Vec::new();
        heap.for_each_region(|words| visits.push(words.to_vec()));

        assert_eq!(visits.len(), 2, "one visit per region with a written prefix");
        let mut expected_first = vec![Eterm::from_raw(0xA0), Eterm::from_raw(0xA1)];
        expected_first.extend((0..14).map(|i| Eterm::from_raw(0xB0 + i)));
        assert_eq!(
            visits[0], expected_first,
            "the oldest region first, its words in allocation order"
        );
        assert_eq!(
            visits[0].len(),
            16,
            "exactly the written prefix of a full region"
        );
        assert_eq!(
            visits[1],
            vec![Eterm::from_raw(0xC0), Eterm::from_raw(0xC1)],
            "the frontier's written prefix, and NOT its committed-but-unwritten tail"
        );
        assert_eq!(
            visits.iter().map(Vec::len).sum::<usize>(),
            heap.stats().used_words,
            "the walk covers exactly the cumulative demand"
        );

        // The slice is the LIVE region: a rewrite through it is visible afterwards.
        heap.for_each_region(|words| {
            for word in words.iter_mut() {
                *word = Eterm::from_raw(0xDEAD);
            }
        });
        // SAFETY: `first` still points into the first region, which is alive and unmoved.
        assert_eq!(unsafe { first.read() }, Eterm::from_raw(0xDEAD));
        // SAFETY: `fresh` still points into the second region's first word.
        assert_eq!(unsafe { fresh.read() }, Eterm::from_raw(0xDEAD));
        // The words past `used` were never handed to the visitor: the frontier still has
        // six committed-but-unwritten words left after the walk.
        assert_eq!(
            heap.regions[1].committed_words() - heap.regions[1].used(),
            6,
            "six committed words stayed out of the walk"
        );
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

    // ---- the BIF suspend channel (`BifOutcome`) ---------------------------
    //
    // Both outcome-aware doors are **defaulted**, so a host that predates the channel
    // is byte-identical to the pre-seam engine: each default forwards to its older
    // twin and wraps the answer in `BifOutcome::Return`.

    /// A host that knows nothing about the suspend channel: only the two required
    /// methods, with `call_bif` answering a known small integer.
    struct PlainBifHost;

    impl ExecutionContext for PlainBifHost {
        fn call_bif(
            &mut self,
            _module: &str,
            _function: &str,
            _arity: u32,
            _args: &[Eterm],
        ) -> Result<Eterm> {
            Ok(beam_term::make_small(7))
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
    fn the_outcome_door_defaults_to_a_wrapped_return() {
        // The defaulted `call_bif_outcome` forwards to `call_bif` and wraps the value,
        // so a host that overrides only that door can never report a suspend and keeps
        // the exact value it always answered.
        let mut ctx = PlainBifHost;
        assert_eq!(
            ExecutionContext::call_bif_outcome(&mut ctx, "erlang", "self", 0, &[]).unwrap(),
            BifOutcome::Return(beam_term::make_small(7)),
            "the defaulted outcome door must forward to `call_bif` and wrap its value"
        );
    }

    /// A host that opted into the **in-machine** door but knows nothing about the
    /// suspend channel: the gate is on and only `call_bif_in_machine` is overridden
    /// (`call_bif` rejects, so taking it would be observable).
    struct InMachineBifHost;

    impl ExecutionContext for InMachineBifHost {
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

        fn wants_bif_in_machine(&self) -> bool {
            true
        }

        fn call_bif_in_machine(
            &mut self,
            _machine: &Machine,
            _entry_module: &Module,
            _module: &str,
            _function: &str,
            _arity: u32,
            _args: &[Eterm],
        ) -> Result<Eterm> {
            Ok(beam_term::make_small(9))
        }
    }

    #[test]
    fn the_in_machine_outcome_door_defaults_to_a_wrapped_return() {
        // The in-machine twin forwards to `call_bif_in_machine` (never to `call_bif`)
        // and wraps the value, so an opted-in host that predates the channel keeps its
        // value route.
        let mut ctx = InMachineBifHost;
        let machine = Machine::new();
        let entry = Module::default();

        assert_eq!(
            ExecutionContext::call_bif_in_machine_outcome(
                &mut ctx,
                &machine,
                &entry,
                "erlang",
                "self",
                0,
                &[]
            )
            .unwrap(),
            BifOutcome::Return(beam_term::make_small(9)),
            "the defaulted in-machine outcome door must forward to `call_bif_in_machine`"
        );
        assert!(
            ctx.call_bif("erlang", "self", 0, &[]).is_err(),
            "`call_bif` rejects here, so the answer above came from the in-machine door"
        );
    }

    #[test]
    fn minimal_context_prefers_its_outcome_hook_and_falls_back_to_the_plain_one() {
        // An installed outcome hook IS the door, so a suspend is expressible while the
        // plain hook keeps answering the plain route.
        let mut ctx = MinimalContext::with_bif_outcome_hook(Box::new(|_m, _f, _a, _args| {
            Ok(BifOutcome::Suspend)
        }));
        ctx.set_bif_hook(Box::new(|_m, _f, _a, _args| Ok(beam_term::make_small(3))));
        assert_eq!(
            ctx.call_bif_outcome("erlang", "port_command", 2, &[])
                .unwrap(),
            BifOutcome::Suspend,
            "an installed outcome hook must be consulted by the outcome door"
        );
        assert_eq!(
            ctx.call_bif("erlang", "port_command", 2, &[]).unwrap(),
            beam_term::make_small(3),
            "the plain door must keep answering the plain hook"
        );

        // With no outcome hook the outcome door is the trait default: the plain answer,
        // wrapped (so the pre-seam behaviour is exactly preserved).
        let mut plain = MinimalContext::with_bif_hook(Box::new(|_m, _f, _a, _args| {
            Ok(beam_term::make_small(4))
        }));
        assert_eq!(
            plain.call_bif_outcome("lists", "reverse", 1, &[]).unwrap(),
            BifOutcome::Return(beam_term::make_small(4)),
            "without an outcome hook the door must forward and wrap"
        );
    }
}
