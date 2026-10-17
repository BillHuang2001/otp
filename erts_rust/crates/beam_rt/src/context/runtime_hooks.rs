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

//! The runtime's concrete [`RuntimeHooks`] dispatcher — the Stage-5 BIF families.
//!
//! `beam_bifs` registers the whole declared `bif.tab` surface, but a large part of it can
//! only be answered by the *running* runtime: the timer wheel, the code store and process
//! control. Every one of those keys resolves through the **single** defaulted
//! [`RuntimeHooks::call`] seam; with no provider installed the default is the typed
//! `unsupported_bif` capability gap (exit 3). This module is the provider
//! [`RuntimeContext::call_registered`](super::RuntimeContext::call_registered) installs for
//! the keys [`RuntimeContext::uses_runtime_hooks`](super::RuntimeContext) claims.
//!
//! # Served keys (and only these)
//!
//! | Key | Provider |
//! |---|---|
//! | `erlang:start_timer/3,4` | [`crate::timers`] — deliver `{timeout, TRef, Msg}`; `/4`'s `{abs, boolean()}` selects a relative time or an absolute Erlang monotonic one |
//! | `erlang:send_after/4` | [`crate::timers`] — deliver `Msg`, under the same `{abs, boolean()}` option |
//! | `erlang:cancel_timer/2` | [`crate::timers`] — the `/1` answers under the option list |
//! | `erlang:function_exported/3` | the [`CodeServer`](crate::CodeServer)'s loaded exports + the **emulator builtins** (the declared `bif.tab` surface plus the `erlang`-namespace registry extras) |
//! | `erlang:link/1`, `erlang:unlink/1` | [`beam_process::ProcessTable`]'s link set — a **local** pid through [`ProcessTable::link`]/`unlink`, a **remote** pid through [`ProcessTable::link_term`]/`unlink_term` (the remote-link set + the wire `DOP_LINK`/`DOP_UNLINK_ID`) |
//! | `erlang:port_set_data/2`, `erlang:port_get_data/1` | [`PortRegistry`](crate::port::PortRegistry)'s opaque per-port data term (`prt->data`) |
//! | `erts_internal:port_connect/2` | [`crate::port::ops::connect`] — the owner transfer, shared with the floor's `erlang:port_connect/2` spelling |
//! | `erlang:dt_spread_tag/1`, `erlang:dt_restore_tag/1` | the oracle's boolean check (dynamic tracing is compiled out) |
//! | `erlang:statistics/1` | `run_queue`; `run_queues`/`run_queue_lengths`/`run_queue_lengths_all`/`total_run_queue_lengths`/`total_run_queue_lengths_all` (the runtime's **one** real run queue, `beam_process`'s `Scheduler`); `wall_clock` (the process clock); `reductions`/`exact_reductions`/`context_switches` (the runtime's own cumulative counters); `runtime` (the long-lived `beam_io::cpu_time::RuntimeClock`); `io` (the port subsystem's byte counters); `garbage_collection` (the runtime's own node-global cumulative collection counters); `scheduler_wall_time`/`scheduler_wall_time_all` (`undefined` — the switch is recorded but no per-scheduler wall time is sampled) |
//! | `erts_internal:scheduler_wall_time/1` | the node-global sampling switch: any non-`true` argument disables, `true` enables, and the **previous** state is answered as `true`/`false` (the C's `atomic_xchg`, `bif.c:5357-5366`) |
//! | `os:set_signal/2` | [`signal`] — the one long-lived `beam_io::signal::SignalFacility` (the dispositions persist for the runtime's life) |
//! | `net_kernel:dflag_unicode_io/1` | `true` for a local pid (a `badarg` for a non-pid; the typed gap for a remote pid) — the io-device read path's unicode fast-path probe |
//!
//! Every **other** `(module, function, arity)` answers the same typed capability gap the
//! defaulted seam does (see [`RuntimeHooksBridge::call`]), so installing this bridge never
//! turns a gap into a fabricated value (workspace `CONTEXT.md` §3).
//!
//! # Documented gaps inside a served key
//!
//! * `{abs, true}` **is** served, and its accepted window is the oracle's exactly
//!   (`[erlang:system_info(start_time), erlang:system_info(end_time)]` in ms, see
//!   [`crate::timers::timer_absolute_delay`]) — but this runtime's monotonic **origin** is
//!   its own process start rather than ERTS's fixed interface instant, so the absolute values
//!   it hands out are small positives rather than `-576460752000`-and-up. A value the oracle
//!   rejects is rejected here too (the window is the same one); a value in that window but
//!   outside *this* runtime's own clock range is accepted and simply fires at the next
//!   scheduler pass, exactly as the C's wheel does with a deadline in the past.
//! * `statistics/1` serves **only** the keys this runtime can measure for real
//!   (`run_queue`, the `run_queues`/`run_queue_lengths*`/`total_run_queue_lengths*` family,
//!   `wall_clock`, `reductions`/`exact_reductions`, `context_switches`, `runtime`, `io`,
//!   `garbage_collection` and
//!   `scheduler_wall_time`/`scheduler_wall_time_all`, see
//!   [`RuntimeHooksBridge::statistics`]); every other key the oracle accepts is a valid
//!   statistics key this runtime cannot answer honestly, so it reports the typed capability
//!   gap. A name that is not a statistics key at all is the oracle's `badarg`.
//! * `scheduler_wall_time`/`scheduler_wall_time_all` answer `undefined` **whatever**
//!   [`RuntimeHooksBridge::scheduler_wall_time`]'s switch holds: this runtime keeps no
//!   per-scheduler wall-time accounting, so the C's `{SchedulerId, ActiveTime, TotalTime}`
//!   tuple list cannot be produced honestly. The switch is recorded (so
//!   `erts_internal:scheduler_wall_time/1`'s previous-value exchange is faithful) but drives
//!   no sampling, and the reader never fabricates a figure from it.
//! * `os:set_signal/2` installs the disposition for real (the one process-global
//!   [`signal`] facility) and the `{notify, Signal}` leg is wired: a caught signal is
//!   written to the facility's self-pipe, and the port layer's pumps deliver the 2-tuple
//!   `{notify, Signal}` to the process registered as `erl_signal_server`, dropping it
//!   silently when none is registered — exactly as the C does
//!   (`erts/emulator/sys/unix/sys.c:589-604`). The three `sigstop` forms are the oracle's
//!   `ok` without a syscall — the kernel refuses every disposition change for a kernel-only
//!   signal. `signal`'s module docs carry the full reasoning.
//!
//! # Destination semantics (the C `erts_hl_timer`, read-only reference)
//!
//! A pid target is pinned when the timer is scheduled; a registered name is resolved when
//! the timer **fires** (so a name registered in the meantime is found); a dead pid simply
//! receives nothing. Both rules are [`crate::timers::TimerTarget`]'s and are shared with the
//! runtime-native `erlang:send_after/3`.

use beam_alloc::Heap;
use beam_bifs::bif_tab::BifTab;
use beam_bifs::{BifContext, BifRegistry, RuntimeHooks};
use beam_common::{BeamError, ExitReason, Raise, Result};
use beam_engine::terms;
use beam_io::signal::{Disposition, Signal};
use beam_process::{PidTarget, ProcessId, ProcessTable};
use beam_term::prelude::{atom_val, is_atom, is_small, make_small, signed_value, NIL};
use beam_term::{AtomTable, Eterm};

use std::cell::Cell;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::error;
use crate::runtime::Runtime;
use crate::timers::{self, TimerTarget};

use super::signal;

/// The [`RuntimeHooks`] provider over the running runtime.
///
/// It holds `&mut Runtime` (the timer heap, the reference id space, the code server) plus
/// the calling process's table and pid — the same pair of borrows
/// [`ProcessBridge`](super::ProcessBridge) resolves, which is why it is its own mutually
/// exclusive arm: the table is the caller-supplied one on a scheduler pass (the runtime's
/// own has been moved out) and the runtime's own otherwise.
pub(super) struct RuntimeHooksBridge<'a> {
    /// The runtime: the timer heap, the reference id space and the code server.
    pub(super) runtime: &'a mut Runtime,
    /// The caller-supplied process table, if any (see [`ProcessBridge::table_mut`]).
    pub(super) table: Option<&'a mut ProcessTable>,
    /// The pid the call runs as (the sender of every timer delivery and the subject of
    /// `link/1`/`unlink/1`).
    pub(super) self_pid: ProcessId,
}

impl<'a> RuntimeHooksBridge<'a> {
    /// The process table a pid resolves against: the caller-supplied one when there is one,
    /// else the runtime's own.
    fn table_mut(&mut self) -> &mut ProcessTable {
        match self.table {
            Some(ref mut table) => &mut **table,
            None => &mut self.runtime.processes,
        }
    }

    /// `erlang:start_timer/3` — the `/4` form with the empty option list.
    ///
    /// # Errors
    ///
    /// Propagates [`Self::start_timer`]'s errors.
    fn start_timer_3(
        &mut self,
        heap: &mut Heap,
        atoms: &AtomTable,
        args: &[Eterm],
    ) -> Result<Eterm> {
        self.start_timer(heap, atoms, args, false)
    }

    /// `erlang:start_timer/4` — `{abs, false}` (the default) or `{abs, true}`.
    ///
    /// # Errors
    ///
    /// `badarg` for a non-pid/non-atom destination, a negative or unrepresentable time, an
    /// absolute time outside the ERTS monotonic range, or a malformed option list.
    fn start_timer_4(&mut self, heap: &mut Heap, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        let options = *args.get(3).ok_or(BeamError::BadArg)?;
        let abs = self.timer_options(options)?;
        self.start_timer(heap, atoms, args, abs)
    }

    /// The shared body of `erlang:start_timer/3,4`: `Time`, `Dest`, `Msg` (and whether the
    /// time is absolute) deliver `{timeout, TRef, Msg}` and answer `TRef`.
    ///
    /// # Errors
    ///
    /// As for [`Self::start_timer_4`]; plus [`BeamError::BadArg`] when an argument is
    /// missing.
    fn start_timer(
        &mut self,
        heap: &mut Heap,
        atoms: &AtomTable,
        args: &[Eterm],
        abs: bool,
    ) -> Result<Eterm> {
        let delay = self.timer_delay_checked(heap, atoms, args, abs)?;
        let target = timers::timer_target(*args.get(1).ok_or(BeamError::BadArg)?)?;
        let message = *args.get(2).ok_or(BeamError::BadArg)?;
        let reference = self.runtime.refs_mut().next().to_eterm(heap)?;
        let timeout = heap.tuple(&[
            atom_term(self.runtime.code_server.atoms(), "timeout")?,
            reference,
            message,
        ])?;
        self.schedule(delay, target, timeout, reference, heap)
    }

    /// `erlang:send_after/4` — the `/3` form plus the `{abs, Bool}` option list, delivering
    /// the bare `Msg` (the oracle's `send_after` never wraps it: only `start_timer` sends
    /// `{timeout, TRef, Msg}`).
    ///
    /// # Errors
    ///
    /// As for [`Self::start_timer_4`].
    fn send_after_4(&mut self, heap: &mut Heap, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        let options = *args.get(3).ok_or(BeamError::BadArg)?;
        let abs = self.timer_options(options)?;
        let delay = self.timer_delay_checked(heap, atoms, args, abs)?;
        let target = timers::timer_target(*args.get(1).ok_or(BeamError::BadArg)?)?;
        let message = *args.get(2).ok_or(BeamError::BadArg)?;
        let reference = self.runtime.refs_mut().next().to_eterm(heap)?;
        self.schedule(delay, target, message, reference, heap)
    }

    /// `erlang:cancel_timer/2` — cancelling with the option list `[{async, B}, {info, B}]`.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`), and exactly what this answers:
    ///
    /// * the first argument must be a **reference this runtime minted** (a timer reference,
    ///   a `make_ref/0` reference, a monitor reference) — anything else is `badarg`;
    /// * `Options` must be a list of `{async, boolean()}` / `{info, boolean()}` tuples, in
    ///   any order, **last occurrence winning**; anything else (a bare atom such as `[info]`,
    ///   a non-list, an improper list, `[{info, 1}]`, an unknown key) is `badarg`;
    /// * with `{async, true}` — or with `{info, false}`, the default — the answer is the atom
    ///   `ok`, whether or not the timer was still pending;
    /// * with `{info, true}` and no `async`, the remaining milliseconds when the timer was
    ///   still pending, else the atom `false` (a fired/cancelled timer, and any other
    ///   reference, are `false`).
    ///
    /// # Errors
    ///
    /// `badarg` for a non-reference first argument or a malformed option list.
    fn cancel_timer_2(&mut self, heap: &mut Heap, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        // A timer reference, or a reference this runtime minted for something else
        // (`make_ref/0`, a monitor) — anything else is not a reference and is `badarg`, the
        // same test `erlang:cancel_timer/1` (the runtime-native floor) applies. The ONE
        // provenance test decides (`Runtime::is_known_local_reference`).
        if !self.runtime.is_known_local_reference(term) {
            return Err(BeamError::BadArg);
        }
        let options = *args.get(1).ok_or(BeamError::BadArg)?;
        let (async_, info) = self.cancel_options(options)?;
        let cancelled = self.runtime.timers_mut().cancel(term);
        if async_ || !info {
            return atom_term(self.runtime.code_server.atoms(), "ok");
        }
        match cancelled {
            Some(remaining) => {
                // Whole milliseconds, truncated — the oracle answers the same unit.
                let millis = i64::try_from(remaining.as_millis()).unwrap_or(i64::MAX);
                Ok(make_small(millis))
            }
            None => atom_term(self.runtime.code_server.atoms(), "false"),
        }
        .map_err(|err| {
            let _ = heap;
            err
        })
    }

    /// `erlang:function_exported/3` — whether a **loaded** module exports `Function/Arity`
    /// or `M:F/A` is an **emulator builtin**.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): the module and the function must both be atoms
    /// and the arity a small integer (anything else is `badarg` — a negative arity is *not*
    /// an error, it simply answers `false`); the answer is `true` iff the module is loaded
    /// and exports the function **or** the `M:F/A` is a built-in BIF
    /// (`erts/emulator/beam/bif.c:4409-4424`, which calls `erts_find_function` then
    /// `erts_is_builtin`). It never **autoloads**: an unloaded module answers `false`.
    ///
    /// The loaded-module half is the [`CodeServer`](crate::CodeServer)'s real module set —
    /// the 22 preloaded `.beam`s plus everything resolved on demand. The built-in half is
    /// [`is_builtin`] — the emulator's own BIF table, **never** the whole
    /// [`registry`](crate::Runtime::registry): that registry also carries the stdlib *fast
    /// paths* (this runtime's implementations of real *Erlang* functions such as
    /// `lists:map/2`), which are not builtins and must not be reported as exported by an
    /// unloaded module. The answer is therefore always about *this* runtime's real state
    /// (never a fabricated export). Like `erlang:module_loaded/1`, a module this runtime has
    /// not loaded answers `false` where the oracle — which boots a much larger module set —
    /// may answer `true`; that is a module-load-state difference, not a wrong answer about a
    /// loaded module.
    ///
    /// # Errors
    ///
    /// `badarg` for a non-atom module/function or a non-integer arity.
    fn function_exported(&mut self, heap: &mut Heap, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        let module_term = *args.first().ok_or(BeamError::BadArg)?;
        let function_term = *args.get(1).ok_or(BeamError::BadArg)?;
        let arity_term = *args.get(2).ok_or(BeamError::BadArg)?;
        if !is_atom(module_term) || !is_atom(function_term) || !is_small(arity_term) {
            return Err(BeamError::BadArg);
        }
        let module = atoms
            .name(atom_val(module_term))
            .ok_or(BeamError::BadArg)?;
        let function = atoms
            .name(atom_val(function_term))
            .ok_or(BeamError::BadArg)?;
        // A negative arity can never match an export or a built-in, so it answers `false`
        // (the oracle does the same — it is a valid small integer, not an error).
        let arity = u32::try_from(signed_value(arity_term)).unwrap_or(u32::MAX);
        let exported = is_builtin(
            declared_bif_tab(),
            &self.runtime.registry,
            module,
            function,
            arity,
        ) || self
            .runtime
            .code_server
            .module(module)
            .is_some_and(|loaded| module_exports(loaded, function, arity));
        let _ = heap;
        atom_term(atoms, if exported { "true" } else { "false" })
    }

    /// `os:set_signal/2` — install an OS-signal disposition.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`) against
    /// `erts/emulator/beam/erl_bif_os.c:145-160` (`os_set_signal_2`) and
    /// `erts/emulator/sys/unix/sys.c:698-767` (`signalterm_to_signum`/`erts_set_signal`):
    ///
    /// * both arguments must be **atoms** (`is_atom`; a string or an integer is `badarg`);
    /// * the option must be exactly `default`, `handle` or `ignore` — in the C the option is
    ///   validated **first**, so an unknown option is `badarg` even for an unknown signal,
    ///   and both are `badarg` regardless of order here (the answer is the same);
    /// * the signal must be one of the twelve atoms `signalterm_to_signum` maps
    ///   ([`Signal::ALL`]) — `sigkill`, `sigttin`, `sigttou`, `siginfo` and every other atom
    ///   are `badarg`;
    /// * otherwise the answer is the bare atom `ok` and the disposition is installed
    ///   *process-globally*: the C's `sys_signal` is a plain `sigaction(2)`, so the
    ///   disposition outlives the calling process and is shared by the whole node. This
    ///   runtime keeps that in the one long-lived [`signal`] facility, so
    ///   `os:set_signal(sigusr1, ignore)` followed by `os:set_signal(sigusr1, handle)`
    ///   really does leave `handle` installed.
    ///
    /// `sigstop` is the one accepted signal the kernel refuses to change (`sigaction(2)`
    /// answers `EINVAL` for it, whose result the C discards); it is the oracle's `ok` here
    /// too, without issuing the syscall — see [`signal::install`].
    ///
    /// The **notification** leg is wired: a caught signal is written to the facility's
    /// self-pipe, and the port layer's pumps deliver ERTS's `{notify, SignalName}` to the
    /// process registered as `erl_signal_server`, dropping it silently when none is
    /// registered — matching the C (`sys.c:589-604`, see [`signal`]).
    ///
    /// # Errors
    ///
    /// `badarg` for a non-atom argument, an unknown signal name or an unknown option; a
    /// genuine `BeamError::Io` only if the OS refuses to create the facility or install the
    /// disposition (reported, never swallowed).
    fn set_signal(&mut self, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        let signal_term = *args.first().ok_or(BeamError::BadArg)?;
        let option_term = *args.get(1).ok_or(BeamError::BadArg)?;
        if !is_atom(signal_term) || !is_atom(option_term) {
            return Err(BeamError::BadArg);
        }
        // A name the caller's shared table does not hold cannot be one of the accepted
        // atoms, so it is the oracle's `badarg` — never the typed "not interned" gap.
        let signal_name = atoms.name(atom_val(signal_term)).ok_or(BeamError::BadArg)?;
        let option_name = atoms.name(atom_val(option_term)).ok_or(BeamError::BadArg)?;
        let signal = Signal::from_name(signal_name).ok_or(BeamError::BadArg)?;
        let disposition = Disposition::from_name(option_name).ok_or(BeamError::BadArg)?;
        signal::install(signal, disposition)?;
        atom_term(atoms, "ok")
    }

    /// `erlang:link/1` (`unlink = false`) and `erlang:unlink/1` (`unlink = true`): link or
    /// unlink the calling process and `Target`, answering `true`.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): `Target` must be a pid — anything else is
    /// `badarg`; a pid answers `true`, including `self()`, a pid no process backs (the
    /// oracle discards a link to a non-existent process and still reports success, exactly
    /// as the C's `erts_proc_lookup_raw` miss does) **and a remote pid** (measured against a
    /// real peer: `erl -sname Peer` + `erlang:link(rpc:call(Peer, erlang, self, []))` ⇒
    /// `true`, and `erlang:unlink/1` on the same pid ⇒ `true`).
    ///
    /// # The remote pid is a **remote link**, not a capability gap
    ///
    /// [`ProcessTable::link_term`] / [`ProcessTable::unlink_term`] are the entry points that
    /// implement the oracle's semantics for a `PidTarget::Remote`: the pair is recorded in
    /// the caller's **remote-link set** *and* the wire control signal
    /// ([`DistSignal::Link`](beam_process::DistSignal) / `Unlink` ⇒ `DOP_LINK` /
    /// `DOP_UNLINK_ID`) is handed to the [`DistSignalRouter`](beam_process::DistSignalRouter)
    /// installed on the process table — which this runtime supplies
    /// ([`crate::dist_client::DistRouter`]). The signal only reaches a socket when this
    /// runtime **is** a node *and* holds an established link to the peer's node; otherwise
    /// the router reports its own typed `distribution: no established connection to node …`
    /// error, which is propagated rather than swallowed. A runtime with **no** router at all
    /// (no `-name`/`-sname`) keeps the local record and reports `true` — which is the
    /// oracle's own observable answer for this BIF, never a fabricated success on top of a
    /// silently dropped signal.
    ///
    /// # Errors
    ///
    /// `badarg` for a non-pid; the typed gap for a caller with no process in the table; the
    /// router's typed transport error for a remote pid it cannot deliver; and any other
    /// link-set failure the table reports.
    fn link(&mut self, args: &[Eterm], unlink: bool) -> Result<Eterm> {
        let target = link_target(*args.first().ok_or(BeamError::BadArg)?)?;
        let self_pid = self.self_pid;
        if !self.table_mut().contains(self_pid) {
            return Err(error::no_process(self_pid));
        }
        let outcome = if unlink {
            self.table_mut().unlink_term(self_pid, target)
        } else {
            self.table_mut().link_term(self_pid, target)
        };
        match outcome {
            Ok(()) => atom_term(self.runtime.code_server.atoms(), "true"),
            // The target pid is not in the table (the `unlink` half reports an unknown pair
            // as `TermNotFound`): the oracle treats a pid no process backs as a successful
            // no-op — the `{'EXIT',…}` simply never arrives and `unlink/1` still answers
            // `true` — so this is the oracle's semantics rather than a swallowed failure. The
            // *caller* is known to exist (checked above), so this can only be the target.
            Err(BeamError::TermNotFound) => {
                atom_term(self.runtime.code_server.atoms(), "true")
            }
            Err(other) => Err(other),
        }
    }

    /// `erlang:port_set_data/2` — store `Data` as the port's opaque data term and answer the
    /// atom `true`.
    ///
    /// The body is [`PortRegistry::set_port_data`](crate::port::PortRegistry) — the *one*
    /// implementation, shared with nothing else because the `PortHook` seam has no data-term
    /// member. What this arm adds is the two things a BIF has and a table does not:
    ///
    /// * the port argument is resolved the C's way (`lookup_port`): an internal port term, or
    ///   an **atom** naming a registered port. Anything else — a pid, an integer, a name
    ///   naming no port — is the oracle's `badarg`;
    /// * `Data` is **stabilised** before it is stored. The runtime-level data table outlives
    ///   the call (and the heap the term was built on), so the word is copied onto the
    ///   pointer-stable escape store exactly as every other runtime-level store does
    ///   ([`Runtime::try_stabilize_escaped`]) — afterwards it may be read back by a later
    ///   `port_get_data/1`, or by the driver itself.
    ///
    /// A **poll/interrupt-disabled** port refuses (`ERTS_PORT_OP_BUSY`) and a dead one is
    /// `badarg`; the latter is [`PortRegistry::set_port_data`]'s own check.
    ///
    /// # Errors
    ///
    /// `badarg` for a non-port/non-name argument, a name naming no port, or a port the runtime
    /// does not hold; the typed capability gap when the port table is already borrowed (never a
    /// panic); whatever [`Runtime::try_stabilize_escaped`] reports for a term it cannot copy.
    fn port_set_data(&mut self, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        let port = *args.first().ok_or(BeamError::BadArg)?;
        let data = *args.get(1).ok_or(BeamError::BadArg)?;
        let data = self.runtime.try_stabilize_escaped(data)?;
        let ports = Rc::clone(&self.runtime.ports);
        let id = match ports.try_borrow() {
            Ok(registry) => registry.resolve_port(port).ok_or(BeamError::BadArg)?,
            Err(_) => return Err(error::unsupported("the port table is already in use")),
        };
        match ports.try_borrow_mut() {
            Ok(mut registry) => registry.set_port_data(id, data)?,
            Err(_) => return Err(error::unsupported("the port table is already in use")),
        }
        atom_term(atoms, "true")
    }

    /// `erlang:port_get_data/1` — the port's opaque data term, or the atom `undefined` when
    /// `port_set_data/2` was never called for it.
    ///
    /// The stored word is already on the pointer-stable escape store (see
    /// [`Self::port_set_data`]), so it is handed back as-is: a data term set by one call and
    /// read by a much later one is the same immutable term, exactly as the C's `prt->data` is.
    ///
    /// # Errors
    ///
    /// `badarg` for a non-port/non-name argument, a name naming no port, or a port the runtime
    /// does not hold (the oracle's own answer for a dead port); the typed capability gap when
    /// the port table is already borrowed.
    fn port_get_data(&mut self, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        let port = *args.first().ok_or(BeamError::BadArg)?;
        let ports = Rc::clone(&self.runtime.ports);
        let data = {
            let registry = match ports.try_borrow() {
                Ok(registry) => registry,
                Err(_) => return Err(error::unsupported("the port table is already in use")),
            };
            let id = registry.resolve_port(port).ok_or(BeamError::BadArg)?;
            registry.port_data(id)?
        };
        match data {
            Some(term) => Ok(term),
            // The C initialises the field to `am_undefined` (`erts_init_port_data`).
            None => atom_term(atoms, "undefined"),
        }
    }

    /// `erts_internal:port_connect/2` — hand the port to `Target`, moving its **owner** (so
    /// every future `{Port, {data, …}}` / `{Port, {exit_status, …}}` message is addressed to
    /// `Target`) while **adding** a link rather than replacing the old owner's.
    ///
    /// The body is [`crate::port::ops::connect`], shared with the `erlang:port_connect/2`
    /// spelling the runtime-native floor serves (`erlang.erl`'s wrapper
    /// `port_connect(Port, Pid) -> erts_internal:port_connect(Port, Pid)` cannot be entered
    /// here because `erlang` is a floor namespace), so the transfer logic exists once.
    ///
    /// The exit **signal** a connected port sends its links when it dies is a separate
    /// mechanism (the `'EXIT'` delivery path), not part of this transfer.
    ///
    /// # Errors
    ///
    /// `badarg` for a non-port/non-name port argument, a name naming no port, a port the
    /// runtime does not hold, a non-pid `Target`, or a `Target` with no live process (the
    /// oracle's own answer: `port_connect(Port, DeadPid)` ⇒ `error:badarg`); the typed
    /// capability gap when the port table is already borrowed.
    fn port_connect_2(&mut self, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        let port = *args.first().ok_or(BeamError::BadArg)?;
        let target = *args.get(1).ok_or(BeamError::BadArg)?;
        let answer = atom_term(atoms, "true")?;
        let ports = Rc::clone(&self.runtime.ports);
        // The target must be a live **local** process; the table this call runs against is the
        // caller-supplied one on a scheduler pass and the runtime's own otherwise (the same
        // resolution [`Self::link`] uses).
        let table: &ProcessTable = match self.table.as_deref() {
            Some(table) => table,
            None => &self.runtime.processes,
        };
        crate::port::ops::connect(&ports, table, port, target)?;
        Ok(answer)
    }

    /// `erlang:dt_spread_tag/1` — the dynamic-tracing tag spreader, a **boolean check**.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): the argument must be the atom `true` **or** the
    /// atom `false` — anything else is `badarg` — and the answer is the atom `true`. With
    /// dynamic tracing compiled out (the oracle's own build, and this runtime's only mode)
    /// the C's `dt_spread_tag_1` does exactly that and nothing else
    /// (`erts/emulator/beam/bif.c:6080-6112`): it validates the boolean and answers `true`;
    /// only under `USE_VM_PROBES` would it instead answer the previous `{Flags, Tag}` pair.
    /// `lib/kernel/src/file.erl`'s `call/2` brackets every file-server round trip with
    /// `dt_spread_tag(true)` / `dt_restore_tag(X)`, so serving this pair is what lets the
    /// `file:` / `code:where_is_file` / `.app`-read boot path run at all.
    ///
    /// # Errors
    ///
    /// `badarg` for anything but the atoms `true`/`false`.
    fn dt_spread_tag(&self, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        let tag = *args.first().ok_or(BeamError::BadArg)?;
        // Both booleans are accepted; the validation is the whole body (see above).
        let _ = boolean_value(atoms, tag)?;
        atom_term(atoms, "true")
    }

    /// `erlang:dt_restore_tag/1` — the dynamic-tracing tag restorer, a **`true` check**.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): the argument must be the atom `true` —
    /// anything else, `false` **included**, is `badarg` — and the answer is the atom
    /// `true`. That is `dt_restore_tag_1`'s no-`USE_VM_PROBES` body
    /// (`erts/emulator/beam/bif.c:6162-6167`); under `USE_VM_PROBES` the argument is
    /// instead the `{Flags, Tag}` tuple [`Self::dt_spread_tag`] returned, which this build
    /// never produces. The two are always called in that pairing by
    /// `file:call/2`.
    ///
    /// # Errors
    ///
    /// `badarg` for anything but the atom `true`.
    fn dt_restore_tag(&self, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        let tag = *args.first().ok_or(BeamError::BadArg)?;
        // `boolean_value` reads `true` as `true` and `false` as `false`; only the former is
        // accepted, so `false` is the oracle's `badarg`.
        if !boolean_value(atoms, tag)? {
            return Err(BeamError::BadArg);
        }
        atom_term(atoms, "true")
    }

    /// `erlang:dt_get_tag/0`, `erlang:dt_get_tag_data/0`, `erlang:dt_put_tag/1` — the
    /// dynamic-tracing tag family.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): with dynamic tracing compiled out
    /// (`!USE_VM_PROBES`, `erts/emulator/beam/bif.c:5980-6160`) all three answer the atom
    /// `undefined`; `dt_put_tag/1` ignores its argument entirely.
    ///
    /// # Errors
    ///
    /// The typed capability gap when the atom `undefined` is not interned.
    fn dt_tag(&self, atoms: &AtomTable) -> Result<Eterm> {
        atom_term(atoms, "undefined")
    }

    /// `erlang:dt_append_vm_tag_data/1`, `erlang:dt_prepend_vm_tag_data/1` — the VM-probe tag
    /// data helpers.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): with dynamic tracing compiled out both return
    /// their argument **unchanged** and apply no type check — `dt_append_vm_tag_data(42)`
    /// answers `42` (`bif.c`'s `!USE_VM_PROBES` bodies). `lib/kernel/src/file.erl` brackets
    /// its calls with these, so serving them is what keeps that path running.
    ///
    /// # Errors
    ///
    /// `badarg` when the argument is missing.
    fn dt_vm_tag_data(&self, args: &[Eterm]) -> Result<Eterm> {
        args.first().copied().ok_or(BeamError::BadArg)
    }

    /// `erlang:system_profile/0` — the profiler start request.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): answers the atom `undefined`. The C only answers
    /// anything else when a profiling subsystem is actually running in the emulator, and this
    /// runtime has none.
    ///
    /// # Errors
    ///
    /// The typed capability gap when the atom `undefined` is not interned.
    fn system_profile(&self, atoms: &AtomTable) -> Result<Eterm> {
        atom_term(atoms, "undefined")
    }

    /// `erlang:check_old_code/1` — whether any process still runs old code for `Module`.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): a non-atom is `badarg`; **any** atom answers
    /// `false` (even an unloaded one). There is never old code here: loading a module never
    /// replaces a live one in this runtime, so the honest answer is always `false`.
    ///
    /// # Errors
    ///
    /// `badarg` for a non-atom.
    fn check_old_code(&self, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        let module = *args.first().ok_or(BeamError::BadArg)?;
        if !is_atom(module) {
            return Err(BeamError::BadArg);
        }
        atom_term(atoms, "false")
    }

    /// `erlang:finish_loading/1` — the prepared-code loading primitive.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): the empty list answers `ok`; anything else —
    /// including the non-empty list a real `code:load_binary` would pass — is `badarg`,
    /// because `prepare_loading/2` (which mints the internal prepared-code terms) is not
    /// implemented and the C requires a genuine prepared term
    /// (`erts/emulator/beam/beam_bif_load.c:793-845`).
    ///
    /// # Errors
    ///
    /// `badarg` for anything but the empty list.
    fn finish_loading(&self, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        let prepared = *args.first().ok_or(BeamError::BadArg)?;
        if !terms::list_elements(prepared)
            .map_err(|_| BeamError::BadArg)?
            .is_empty()
        {
            return Err(BeamError::BadArg);
        }
        atom_term(atoms, "ok")
    }

    /// `erlang:call_on_load_function/1`, `erlang:has_prepared_code_on_load/1` and
    /// `erlang:finish_after_on_load/2` — the on-load-hook primitives.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`, a BEAMASM build): all three always answer
    /// `badarg`. They require an internal prepared-code term that only `prepare_loading/2`
    /// can mint (and on BEAMASM `call_on_load_function` rejects even that form,
    /// `beam_bif_load.c:793-799`), so `badarg` is the honest, complete answer.
    ///
    /// # Errors
    ///
    /// Always `badarg`.
    fn badarg_always(&self) -> Result<Eterm> {
        Err(BeamError::BadArg)
    }

    /// `erts_internal:is_system_process/1` and `erts_internal:is_process_executing_dirty/1`
    /// — process-state predicates over an **internal** pid.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): a local pid answers `false` for both (this
    /// runtime has no system process and never executes Process code on a dirty scheduler);
    /// anything that is not a pid is `badarg`. A **remote** pid is the typed capability gap
    /// rather than a fabricated `false`: the C reads the local process record
    /// (`erl_bif_info.c:4336-4360`, `erl_process.c:9216-9250`), which a foreign pid has none
    /// of (`PidTarget::Remote` is a distribution, contract A.5, not implemented here).
    ///
    /// # Errors
    ///
    /// `badarg` for a non-pid; the typed gap for a remote pid.
    fn is_process_predicate(&self, atoms: &AtomTable, args: &[Eterm], what: &str) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        match PidTarget::from_eterm(term) {
            Ok(PidTarget::Local(_)) => atom_term(atoms, "false"),
            Ok(PidTarget::Remote(_)) => Err(error::unsupported(&format!(
                "erts_internal:{what}/1 on a remote pid: distribution is not implemented"
            ))),
            Err(_) => Err(BeamError::BadArg),
        }
    }

    /// `net_kernel:dflag_unicode_io/1` — whether the distribution unicode-IO flag is set for
    /// the node `Pid` belongs to.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): a non-pid argument is `badarg`; a pid whose
    /// distribution entry is **this** node answers the atom `true`
    /// (`dist.c:6693-6709`'s `net_kernel_dflag_unicode_io_1`, whose
    /// `de == erts_this_dist_entry` arm short-circuits to `am_true` *before* reading any
    /// flags). This runtime is a single node, so a **local** pid always answers `true`. A
    /// **remote** pid would need the peer's negotiated `DFLAG_UNICODE_IO`, but the accepted
    /// links (`crate::dist::DistConnection`) expose no peer-flag accessor and a pid term
    /// carries no link identity, so the honest answer is the typed capability gap rather
    /// than a fabricated `true`. `lib/stdlib/src/io.erl`'s `bc_req/3` reads this flag on
    /// every `io:request/2`, so serving the local case is what lets the io-device read path
    /// (`file:read/2`, `io:scan_erl_exprs/4`, …) run at all.
    ///
    /// # Errors
    ///
    /// `badarg` for a non-pid; the typed gap for a remote pid.
    fn dflag_unicode_io(&self, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        match PidTarget::from_eterm(term) {
            Ok(PidTarget::Local(_)) => atom_term(atoms, "true"),
            Ok(PidTarget::Remote(_)) => Err(error::unsupported(
                "net_kernel:dflag_unicode_io/1 on a remote pid: distribution is not implemented",
            )),
            Err(_) => Err(BeamError::BadArg),
        }
    }

    /// `erts_internal:erase_persistent_terms/0` — erase **every** persistent term.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): answers `true`. This is the test suite's reset
    /// hook (`erts/emulator/beam/erl_bif_persistent.c:551-567`), and it really clears this
    /// runtime's own store (`Runtime::persistent_terms`) — a later
    /// `persistent_term:get/0` sees an empty table.
    ///
    /// # Errors
    ///
    /// The typed capability gap when the store is already borrowed (the BIF cannot re-enter
    /// it), mirroring the `PersistentTerm` arm's nested-borrow contract.
    fn erase_persistent_terms(&self, atoms: &AtomTable) -> Result<Eterm> {
        let store = Rc::clone(&self.runtime.persistent_terms);
        let mut store = store.try_borrow_mut().map_err(|_| {
            error::unsupported(
                "erts_internal:erase_persistent_terms/0: the persistent-term store is in use",
            )
        })?;
        for (key, _) in store.get_all() {
            store.erase(key, atoms);
        }
        atom_term(atoms, "true")
    }

    /// `erts_internal:scheduler_wall_time/1` — arm or disarm the node-global scheduler
    /// wall-time sampling switch, answering the **previous** state.
    ///
    /// The C is `erts/emulator/beam/bif.c:5357-5366`:
    ///
    /// ```c
    /// new = (BIF_ARG_1 == am_true) ? 1 : 0;      /* any non-`true` is 0, NOT badarg */
    /// old = atomic_xchg(sched_wall_time, new);   /* the previous value */
    /// BIF_RET(old ? am_true : am_false);
    /// ```
    ///
    /// so this arm accepts **any** argument: the atom `true` enables sampling and everything
    /// else (`false`, `foo`, `1`, a tuple, …) disables it — never a `badarg`, never a raise.
    /// The answer is the state the switch held *before* this call, as the atom
    /// `true`/`false`. `kernel_refc`'s `resource(scheduler_wall_time, Enable)`
    /// (`lib/kernel/src/kernel_refc.erl:141-142`) is the boot-path caller: `kernel_sup` starts
    /// it `permanent`, so a raise here tears `kernel_sup` down with `start_error`.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`), **one VM**, in order:
    ///
    /// ```text
    /// erts_internal:scheduler_wall_time(true)  -> false
    /// erts_internal:scheduler_wall_time(false) -> true
    /// erts_internal:scheduler_wall_time(true)  -> false
    /// erts_internal:scheduler_wall_time(foo)   -> true    (a non-`true` disables; prev was armed)
    /// erts_internal:scheduler_wall_time(foo)   -> false
    /// ```
    ///
    /// and on a **fresh** VM each of `(true)`, `(false)`, `(foo)`, `(1)` answers `false` (the
    /// switch starts disabled). `erlang:system_flag(scheduler_wall_time, Bool)`
    /// (`bif.c:5225-5227`) exchanges the same slot; the native floor that owns
    /// `erlang:system_flag/2` here serves only its `reset_seq_trace`/`system_logger` arms, so
    /// that spelling stays the typed gap (wiring it to this slot would touch the floor, not
    /// this bridge) — see [`crate::seq_trace::SeqTrace::system_flag_2`].
    ///
    /// The switch lives in a `thread_local` ([`SCHEDULER_WALL_TIME`]), not on `Runtime`,
    /// mirroring [`WALL_CLOCK`]: it is the C's single node-global `sched_wall_time` atomic,
    /// and the runtime is single-threaded (`!Send`).
    ///
    /// **Residual (honest, not fabricated).** This runtime records the switch but does **not**
    /// sample per-scheduler wall time — it keeps none of the per-scheduler CPU accounting the
    /// C's `ERTS_SCHED_WALL_TIME` collects — so the sibling
    /// `erlang:statistics(scheduler_wall_time|scheduler_wall_time_all)` keeps answering the
    /// atom `undefined` **even once the switch is armed** (see [`Self::statistics`]). The
    /// switch and that reader therefore cannot drift into a fabricated
    /// `{SchedulerId, ActiveTime, TotalTime}` tuple list; arming it is observable only through
    /// this exchange's own previous-value answer.
    ///
    /// # Errors
    ///
    /// Propagates [`atom_term`]'s typed capability gap for a name the shared table does not
    /// hold — the boot seed ([`CORE_ATOMS`](crate::boot::CORE_ATOMS)) interns both `true` and
    /// `false`, so it is unreachable in practice. The **argument** is never rejected.
    fn scheduler_wall_time(&self, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        // Any non-`true` argument disables (`bif.c:5357`'s `BIF_ARG_1 == am_true` test): a
        // missing argument can only happen if the dispatch arity drifts, and it is a disabling
        // request rather than a `badarg`, exactly as any other non-`true` term is.
        let enable = args
            .first()
            .is_some_and(|arg| is_atom(*arg) && atoms.name(atom_val(*arg)) == Some("true"));
        let previous = SCHEDULER_WALL_TIME.with(|slot| slot.replace(enable));
        atom_term(atoms, if previous { "true" } else { "false" })
    }
    /// `erts_literal_area_collector:release_area_switch/0` — the literal-area switch probe.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): the oracle **raises** `error notsup` (`i_notsup`
    /// in `erts/emulator/beam/erl_bif_info.c`) — the facility is not compiled in — so this
    /// arm must raise the same reason, never return the bare atom. This runtime has no
    /// literal-area-collector process at all, so `notsup` is the honest answer rather than a
    /// fabricated success.
    ///
    /// # Errors
    ///
    /// Always the `error notsup` raise the oracle produces.
    fn notsup(&self) -> Result<Eterm> {
        Err(BeamError::Raise(Raise::error(ExitReason::atom(String::from(
            "notsup",
        )))))
    }

    /// `erts_trace_cleaner:check/0` — whether the trace-cleaner has work to do.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): answers `false`
    /// (`erts/emulator/beam/erl_bif_trace.c:605-640` returns `am_false` unless a cleanup
    /// signal is pending). This runtime never mints one.
    ///
    /// # Errors
    ///
    /// The typed gap when the atom `false` is not interned.
    fn trace_cleaner_check(&self, atoms: &AtomTable) -> Result<Eterm> {
        atom_term(atoms, "false")
    }

    /// `error_logger:warning_map/0` — the logger-level → error-logger-event mapping.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): answers the atom `warning`, the fixed
    /// `erts_error_logger_warnings` mapping this build ships.
    ///
    /// # Errors
    ///
    /// The typed gap when the atom `warning` is not interned.
    fn warning_map(&self, atoms: &AtomTable) -> Result<Eterm> {
        atom_term(atoms, "warning")
    }

    /// `code:set_coverage_mode/1` — set the coverage mode, answering the **previous** mode.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): only the atoms `none` and `line` are accepted
    /// (anything else, `call` included, is `badarg`) and the answer is `none` — this runtime
    /// has no coverage-instrumented code, so the previous mode is always `none`
    /// (`code:coverage_support/0` is `true`, `code:get_coverage_mode/0` is `none`; the
    /// per-module and per-call coverage *readers* — `get_coverage/2`, `reset_coverage/1`,
    /// `get_debug_info/1` — are `badarg` for every argument in this build, so they stay the
    /// typed capability gap).
    ///
    /// # Errors
    ///
    /// `badarg` for a non-atom or an atom other than `none`/`line`.
    fn set_coverage_mode(&self, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        let mode = *args.first().ok_or(BeamError::BadArg)?;
        if !is_atom(mode) {
            return Err(BeamError::BadArg);
        }
        match atoms.name(atom_val(mode)) {
            Some("none") | Some("line") => atom_term(atoms, "none"),
            _ => Err(BeamError::BadArg),
        }
    }

    /// `erlang:statistics/1` — the counters this runtime can answer **honestly** from its
    /// own real state.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`). The served keys, and why each figure is a real
    /// measurement rather than a placeholder:
    ///
    /// * `run_queue` → the total length of the runnable queues, as a small integer. This is
    ///   [`Runtime::run_queue_len`], the length of the runtime's **one** real run queue
    ///   (`beam_process`'s `Scheduler`). The oracle sums every scheduler's queue
    ///   (`erts_run_queues_len(...)`); this runtime is single-threaded, so it is the same
    ///   quantity over the same kind of queue — both answer `0` once the node is idle.
    /// * `wall_clock` → `{TotalMilliseconds, MillisecondsSinceLastCall}`, read from the
    ///   process monotonic clock ([`std::time::Instant`]). The oracle's relationship holds
    ///   exactly: the first read answers the same value for both elements, and every later
    ///   read answers the delta since the previous one. This is what `c:uptime/0` reads.
    /// * `reductions`/`exact_reductions` → `{Total, SinceLast}` from the node-global
    ///   **cumulative** reduction count the runner keeps ([`Runtime::note_reductions`]): the
    ///   number of reduction units (`BudgetProbe`) the executing machine was charged at its
    ///   heap-admission points, summed over every scheduler slice. `SinceLast` is the delta
    ///   since the previous read of the *same* key (see [`Runtime::reductions_statistics`]).
    /// * `context_switches` → `{Total, SinceLast}` from the node-global process-dispatch
    ///   counter the scheduler keeps ([`Runtime::note_context_switches`]): one per slice the
    ///   run loop dispatched. `SinceLast` is the delta since the previous read.
    /// * `runtime` → `{Total, SinceLastCall}` in **milliseconds of user CPU time** from the
    ///   runtime's one long-lived [`beam_io::cpu_time::RuntimeClock`]
    ///   ([`Runtime::cpu_time_statistics`]): the whole process's `getrusage(RUSAGE_SELF)` user
    ///   time, exactly what the oracle's `erts_runtime_elapsed_both` measures
    ///   (`erts/emulator/beam/erl_time_sup.c:1421-1481`). The runtime owns **one** clock, so
    ///   its anchor persists across calls and the first read answers two equal elements, like
    ///   the C's process-global `runtime_prev`. `beam_rt` is `#![deny(unsafe_code)]`, so the
    ///   syscall lives in `beam_io` behind a safe API (never a wall-clock substitute).
    /// * `io` → `{{input, BytesIn}, {output, BytesOut}}` from the shared byte counters the
    ///   port subsystem bumps ([`crate::port::IoCounters`]): bytes delivered from a port as
    ///   `{Port, {data, …}}` and bytes written to a port by `port_command/2,3`.
    /// * `run_queues`, `run_queue_lengths`, `run_queue_lengths_all`,
    ///   `total_run_queue_lengths`, `total_run_queue_lengths_all` → the runtime's **one** real
    ///   run queue ([`Runtime::run_queue_len`], the single `beam_process::Scheduler`), read
    ///   exactly as the C's `erts_run_queues_len` reads its per-scheduler queues
    ///   (`erl_process.c:9275-9302`): the tuple/list forms carry one element per run queue and
    ///   the `total_*` forms the sum, so all five answer this runtime's single queue (see
    ///   [`Self::run_queues`]).
    /// * `garbage_collection` → `{GCs, WordsReclaimed, 0}`, the shape the C's
    ///   `gather_gc_info_result/1` builds (`erlang.erl:13392-13404`: the third element is the
    ///   reserved constant `0`). Both figures are this runtime's **node-global cumulative**
    ///   collection counters ([`Runtime::gc_stats`]'s `collections`/`reclaimed`), so the answer
    ///   is the collection work this runtime really did since it started — never a value summed
    ///   from whichever processes happen to be **live** at the read (see
    ///   [`Self::garbage_collection`]).
    /// * `scheduler_wall_time`, `scheduler_wall_time_all` → the atom `undefined`, the
    ///   oracle's own "not enabled" answer. Only
    ///   `erlang:system_flag(scheduler_wall_time, true)` (or
    ///   [`Self::scheduler_wall_time`]) turns either into per-scheduler
    ///   `{SchedulerId, ActiveTime, TotalTime}` tuples, and this runtime measures no
    ///   per-scheduler wall time, so it answers `undefined` **even once the switch is armed**
    ///   — the switch is recorded, never sampled. `undefined` is therefore exact for the
    ///   disabled case and the honest "this runtime cannot measure it" for the armed one,
    ///   never a fabricated tuple list.
    ///
    /// **Documented divergence (the epoch/scope, never the clock or counter).** The oracle's
    /// `wall_clock` total is measured from emulator start; this runtime anchors it at the
    /// **first** `statistics(wall_clock)` read, because no earlier anchor is reachable from
    /// this seam — the runtime's other monotonic origin lives inside `beam_bifs`
    /// (`src/time.rs`) and exposes no reader. The numbers are therefore real elapsed time with
    /// a different zero, exactly the class of divergence as `erlang:processes/0` ("an answer
    /// that reads *this* runtime's own state"). The same "different zero" holds for
    /// `reductions` and `context_switches`: they count *this* runtime's own work from its own
    /// start, and the oracle counts far more of both because it runs a much larger emulator.
    /// `garbage_collection` has the same character: its two figures are this runtime's own
    /// cumulative counters since its start, where the oracle's are per-scheduler accumulators
    /// over its whole far larger emulator (see [`Self::garbage_collection`]).
    ///
    /// Every **other** key the oracle accepts is a genuine statistics key this runtime
    /// cannot measure — `active_tasks*`/`total_active_tasks*` need the C's per-run-queue
    /// "a scheduler is executing from this queue" (`ERTS_RUNQ_FLG_EXEC`) state, which
    /// `beam_process::Scheduler` does not record, and `microstate_accounting` the MSACC
    /// per-thread machinery — so each answers the typed capability gap rather than a
    /// fabricated number (workspace `CONTEXT.md` §3). A name that is **not** a statistics key
    /// at all is the oracle's `badarg` (`erl_bif_info.c:4333`), which is reproduced rather
    /// than reported as a gap.
    ///
    /// # Errors
    ///
    /// `badarg` for a non-atom or an unknown key; the typed capability gap for a valid key
    /// this runtime cannot measure.
    fn statistics(&self, heap: &mut Heap, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        let key = *args.first().ok_or(BeamError::BadArg)?;
        if !is_atom(key) {
            return Err(BeamError::BadArg);
        }
        // A name the shared table does not hold cannot name a statistics key, so it is the
        // oracle's `badarg` (never a substituted key).
        let name = atoms.name(atom_val(key)).ok_or(BeamError::BadArg)?;
        match name {
            "run_queue" => Ok(self.total_run_queue_lengths()),
            // The run-queue family: this runtime has one real run queue and runs no dirty
            // schedulers, so the per-queue forms carry exactly one element (see
            // [`Self::run_queues`]).
            "run_queues" => self.run_queues(heap),
            "run_queue_lengths" | "run_queue_lengths_all" => self.run_queue_lengths(heap),
            "total_run_queue_lengths" | "total_run_queue_lengths_all" => {
                Ok(self.total_run_queue_lengths())
            }
            "wall_clock" => {
                let (total, since) = wall_clock_millis();
                heap.tuple(&[make_small(total), make_small(since)])
            }
            // `scheduler_wall_time`/`scheduler_wall_time_all` answer `undefined` until
            // `erlang:system_flag(scheduler_wall_time, true)` has been applied
            // (`erl_bif_info.c:4174-4183`). This runtime *records* that switch (see
            // [`Self::scheduler_wall_time`]) but keeps no per-scheduler wall-time accounting,
            // so it answers `undefined` even once the switch is armed — the oracle's own "not
            // enabled" answer for the disabled case, and the honest "cannot measure" for the
            // armed one, never a fabricated tuple list (`tests/runtime_hooks.rs` pins the
            // disabled answer against the live oracle).
            "scheduler_wall_time" | "scheduler_wall_time_all" => atom_term(atoms, "undefined"),
            // The three `{Total, SinceLast}` counter pairs, read from the runtime's own real
            // cumulative counters (`SinceLast` is the delta since the previous read of this
            // same key — see `Runtime::reductions_statistics`).
            "reductions" => counter_pair(heap, self.runtime.reductions_statistics(false)),
            "exact_reductions" => counter_pair(heap, self.runtime.reductions_statistics(true)),
            "context_switches" => counter_pair(heap, self.runtime.context_switch_statistics()),
            // `{Total, SinceLast}` in **milliseconds of user CPU time** from the runtime's one
            // long-lived `beam_io::cpu_time::RuntimeClock` (the C's `getrusage(RUSAGE_SELF)`
            // measurement, `erts_runtime_elapsed_both`, `erl_time_sup.c:1421-1481`). The clock
            // is the runtime's own, so its anchor persists across calls exactly as the C's
            // `runtime_prev` does.
            "runtime" => counter_pair(heap, self.runtime.cpu_time_statistics()?),
            // `{{input, BytesIn}, {output, BytesOut}}` from the port subsystem's own byte
            // counters — the bytes this runtime really read from and wrote to its ports.
            "io" => {
                let (input, output) = self.runtime.io_statistics();
                let in_pair = heap.tuple(&[atom_term(atoms, "input")?, small_u64(input)])?;
                let out_pair = heap.tuple(&[atom_term(atoms, "output")?, small_u64(output)])?;
                heap.tuple(&[in_pair, out_pair])
            }
            // `{GCs, WordsReclaimed, 0}` — the C's `gather_gc_info_result/1` shape, built from
            // this runtime's own node-global collection counters (see
            // [`Self::garbage_collection`]).
            "garbage_collection" => self.garbage_collection(heap),
            other if STATISTICS_KEYS.contains(&other) => Err(error::unsupported(&format!(
                "erlang:statistics/1: the `{other}` counter is not implemented"
            ))),
            _ => Err(BeamError::BadArg),
        }
    }

    /// The runtime's **one** real run queue's length, as a term-ready integer.
    ///
    /// [`Runtime::run_queue_len`] is the single `beam_process::Scheduler`'s run queue — this
    /// runtime runs no dirty schedulers — so it is the whole of every run-queue figure the
    /// oracle's `erts_run_queues_len` computes over its scheduler set
    /// (`erl_process.c:9275-9302`).
    fn run_queue_length(&self) -> i64 {
        // A `usize` queue length is a small integer on any platform this runtime builds on;
        // the saturation keeps the conversion total without trapping.
        i64::try_from(self.runtime.run_queue_len()).unwrap_or(i64::MAX)
    }

    /// `statistics(run_queues)` — the per-run-queue length **tuple**
    /// (`erl_bif_info.c:4310-4331`).
    ///
    /// The C emits one element per run queue (`erts_no_run_queues + ERTS_NUM_DIRTY_RUNQS`).
    /// This runtime has exactly **one** run queue and no dirty schedulers, so the honest tuple
    /// has one element: padding it with the oracle's always-allocated dirty-CPU/dirty-I/O
    /// queues would name run queues that do not exist here, and an empty tuple would name
    /// none at all.
    ///
    /// # Errors
    ///
    /// Propagates the heap's failure when the tuple cannot be built.
    fn run_queues(&self, heap: &mut Heap) -> Result<Eterm> {
        heap.tuple(&[make_small(self.run_queue_length())])
    }

    /// `statistics(run_queue_lengths)` / `statistics(run_queue_lengths_all)` — the
    /// per-run-queue length **list** (`erl_bif_info.c:4200-4230`).
    ///
    /// `run_queue_lengths` covers the normal and dirty-CPU run queues, `_all` adds the dirty
    /// I/O ones. This runtime has a single run queue and no dirty ones, so both are the same
    /// one-element list — a faithful reading of the same state, not a padded one.
    ///
    /// # Errors
    ///
    /// Propagates the heap's failure when the list cell cannot be built.
    fn run_queue_lengths(&self, heap: &mut Heap) -> Result<Eterm> {
        heap.cons(make_small(self.run_queue_length()), NIL)
    }

    /// `statistics(total_run_queue_lengths)` / `statistics(total_run_queue_lengths_all)` —
    /// the **sum** over the run queues (`erl_bif_info.c:4184-4199`).
    ///
    /// With one run queue it is [`Self::run_queue_length`] itself; the `_all` form only widens
    /// the coverage to the dirty-I/O queues, of which this runtime has none.
    fn total_run_queue_lengths(&self) -> Eterm {
        make_small(self.run_queue_length())
    }

    /// `statistics(garbage_collection)` — `{GCs, WordsReclaimed, 0}`, the shape the C's
    /// `gather_gc_info_result/1` builds (`erts/preloaded/src/erlang.erl:13392-13404`; the
    /// third element is the reserved constant `0`).
    ///
    /// The C sums two **per-scheduler** accumulators — `gc_info.garbage_cols` (collections run)
    /// and `gc_info.reclaimed` (words reclaimed) — plus the dirty schedulers' own
    /// (`erl_gc.c:3546-3603`); those accumulators outlive every process. This runtime keeps the
    /// same two quantities **node-globally**, on [`Runtime::gc_stats`]: `collections` (every
    /// collection this runtime really ran) and `reclaimed` (the words those collections really
    /// freed, each collection's own outcome — see [`Runtime::note_gc_reclaimed`]). Both are real
    /// measurements of work this runtime really did, in the same scope, so the two elements can
    /// never describe different things.
    ///
    /// **Why not the live processes' collectors** (an earlier shape of this arm): a
    /// `statistics/1` read taken from inside a **scheduler slice** is answered while the
    /// scheduler has the process table moved out of the runtime, so summing the live processes
    /// sees none of them and answers `{0, 0, 0}` — precisely when a real workload's collections
    /// are the interesting ones. It also drops a terminated process's contribution, where the
    /// C's per-scheduler accumulators keep it.
    ///
    /// **Divergence (scope, documented):** the counters are this runtime's own cumulative work
    /// since *its* start, exactly as `reductions`/`context_switches` are, where the oracle's are
    /// per-scheduler accumulators of a much larger emulator. The quantities are the same, the
    /// zero and the magnitude are not.
    ///
    /// # Errors
    ///
    /// Propagates the heap's failure when the tuple cannot be built.
    fn garbage_collection(&self, heap: &mut Heap) -> Result<Eterm> {
        let stats = self.runtime.gc_stats();
        heap.tuple(&[
            small_u64(stats.collections),
            small_u64(stats.reclaimed),
            make_small(0),
        ])
    }

    /// Schedules `message` for `target` at `delay` from now under the reference `reference`,
    /// and answers `reference`.
    ///
    /// # Errors
    ///
    /// The storage stabilization's failure: the message genuinely cannot be copied onto the
    /// pointer-stable arena ([`Runtime::try_stabilize_escaped`], reported as the emulator's
    /// `error:system_limit`). The reference term itself is already built by the caller before
    /// scheduling, so building it cannot fail here.
    fn schedule(
        &mut self,
        delay: std::time::Duration,
        target: TimerTarget,
        message: Eterm,
        reference: Eterm,
        heap: &mut Heap,
    ) -> Result<Eterm> {
        let _ = heap;
        // The message is delivered **later**, by the scheduler loop: copy it onto the
        // pointer-stable arena now (see [`Runtime::try_stabilize_escaped`]) so it survives the
        // scheduling process's own collection, reporting a term the copier genuinely cannot
        // copy as the honest `system_limit`. `from` needs no copy (a pid term is a heap-free
        // immediate) and `reference` must not be copied (it is the caller's own handle and the
        // `Timers::refs` lookup key).
        let message = self.runtime.try_stabilize_escaped(message)?;
        let from = self.self_pid.to_eterm();
        self.runtime
            .timers_mut()
            .schedule(delay, target, from, message, reference);
        Ok(reference)
    }

    /// The delay of a timer BIF's time argument.
    ///
    /// A **relative** time (the `/3` forms, and `{abs, false}`) is validated by the shared
    /// [`crate::timers::timer_delay`]. An **absolute** one (`{abs, true}`) is the remaining
    /// time to that Erlang monotonic time, computed by
    /// [`crate::timers::timer_absolute_delay`] against the runtime's own monotonic clock —
    /// read from the registered `erlang:monotonic_time/0` by [`Self::monotonic_now_nanos`] so
    /// that the subtraction cancels against the caller's value whatever the clock's origin.
    ///
    /// The time argument is validated **first**, so a bad time is the oracle's `badarg` even
    /// when the option list also asks for the absolute form.
    ///
    /// # Errors
    ///
    /// `badarg` for a time the oracle rejects — a non-integer, a negative relative time, an
    /// absolute time outside the ERTS monotonic range, or a deadline the runtime's monotonic
    /// clock cannot represent — as for the two kernels; plus whatever
    /// [`Self::monotonic_now_nanos`] reports.
    fn timer_delay_checked(
        &mut self,
        heap: &mut Heap,
        atoms: &AtomTable,
        args: &[Eterm],
        abs: bool,
    ) -> Result<std::time::Duration> {
        let time = *args.first().ok_or(BeamError::BadArg)?;
        if !abs {
            return timers::timer_delay(time);
        }
        let now_nanos = self.monotonic_now_nanos(heap, atoms)?;
        timers::timer_absolute_delay(time, now_nanos)
    }

    /// The runtime's monotonic clock, in native nanoseconds — read from the **registered**
    /// `erlang:monotonic_time/0`.
    ///
    /// An `{abs, true}` time is an Erlang **monotonic** time, and its caller derived it from
    /// that very BIF (`lib/stdlib/src/timer.erl`'s `system_time/0` is
    /// `(erlang:monotonic_time(microsecond) + 999) div 1000`, and `req/2` adds the delay to
    /// it). Reading the clock back through the same BIF is therefore what makes the
    /// subtraction exact: an `Instant` epoch of this module's own — the same *unit*, a
    /// different *origin* — would leave the whole origin difference in the delay. The
    /// [`Rc`]`<`[`BifRegistry`]`>` is taken before the context is built, so the shared
    /// registry is only ever borrowed immutably alongside the caller's heap and atom table.
    ///
    /// # Errors
    ///
    /// Propagates the BIF's own errors: the heap's failure while materializing the reading,
    /// and [`timers::monotonic_nanos`]'s typed refusal of a reading this runtime cannot
    /// express as an `i64`.
    fn monotonic_now_nanos(&mut self, heap: &mut Heap, atoms: &AtomTable) -> Result<i64> {
        let registry = Rc::clone(&self.runtime.registry);
        let mut context = BifContext::new(heap, atoms, self.self_pid);
        let reading = registry.call("erlang", "monotonic_time", 0, &mut context, &[])?;
        timers::monotonic_nanos(reading)
    }

    /// Parses the `start_timer/4` / `send_after/4` option list, answering whether the time is
    /// absolute.
    ///
    /// Only `{abs, boolean()}` is accepted (the oracle's documented option set for these two
    /// BIFs); a non-list, an improper list, a non-2-tuple element, an unknown key and a
    /// non-boolean value are all `badarg`.
    ///
    /// # Errors
    ///
    /// `badarg` for a malformed option list.
    fn timer_options(&self, options: Eterm) -> Result<bool> {
        let elements = terms::list_elements(options).map_err(|_| BeamError::BadArg)?;
        let mut abs = false;
        for option in elements {
            let (key, value) = option_pair(self.runtime.code_server.atoms(), option)?;
            if key != "abs" {
                return Err(BeamError::BadArg);
            }
            abs = boolean_value(self.runtime.code_server.atoms(), value)?;
        }
        Ok(abs)
    }

    /// Parses the `cancel_timer/2` option list, answering `(async, info)`.
    ///
    /// The oracle accepts `{async, boolean()}` and `{info, boolean()}` in any order with the
    /// **last** occurrence of each key winning (verified against OTP 29/`erts-17.1`).
    ///
    /// # Errors
    ///
    /// `badarg` for a malformed option list, exactly as [`Self::timer_options`].
    fn cancel_options(&self, options: Eterm) -> Result<(bool, bool)> {
        let elements = terms::list_elements(options).map_err(|_| BeamError::BadArg)?;
        let mut async_ = false;
        let mut info = false;
        for option in elements {
            let (key, value) = option_pair(self.runtime.code_server.atoms(), option)?;
            let flag = boolean_value(self.runtime.code_server.atoms(), value)?;
            match key.as_str() {
                "async" => async_ = flag,
                "info" => info = flag,
                _ => return Err(BeamError::BadArg),
            }
        }
        Ok((async_, info))
    }
}

impl RuntimeHooks for RuntimeHooksBridge<'_> {
    fn call(
        &mut self,
        heap: &mut Heap,
        atoms: &AtomTable,
        module: &str,
        function: &str,
        args: &[Eterm],
    ) -> Result<Eterm> {
        match (module, function, args.len()) {
            ("erlang", "start_timer", 3) => self.start_timer_3(heap, atoms, args),
            ("erlang", "start_timer", 4) => self.start_timer_4(heap, atoms, args),
            ("erlang", "send_after", 4) => self.send_after_4(heap, atoms, args),
            ("erlang", "cancel_timer", 2) => self.cancel_timer_2(heap, args),
            ("erlang", "function_exported", 3) => self.function_exported(heap, atoms, args),
            ("os", "set_signal", 2) => self.set_signal(atoms, args),
            ("net_kernel", "dflag_unicode_io", 1) => self.dflag_unicode_io(atoms, args),
            ("erlang", "link", 1) => self.link(args, false),
            ("erlang", "unlink", 1) => self.link(args, true),
            // The port table's own operations: the opaque data term and the owner transfer.
            // `erlang:port_connect/2`'s wrapper is un-enterable (`erlang` is a floor
            // namespace), so the floor serves that spelling and this arm the `erts_internal:`
            // one; both reach the same `crate::port::ops::connect`.
            ("erlang", "port_set_data", 2) => self.port_set_data(atoms, args),
            ("erlang", "port_get_data", 1) => self.port_get_data(atoms, args),
            ("erts_internal", "port_connect", 2) => self.port_connect_2(atoms, args),
            ("erlang", "dt_spread_tag", 1) => self.dt_spread_tag(atoms, args),
            ("erlang", "dt_restore_tag", 1) => self.dt_restore_tag(atoms, args),
            ("erlang", "statistics", 1) => self.statistics(heap, atoms, args),
            // Dynamic-tracing tag family: compiled out, so each is the oracle's fixed answer.
            ("erlang", "dt_get_tag", 0)
            | ("erlang", "dt_get_tag_data", 0)
            | ("erlang", "dt_put_tag", 1) => self.dt_tag(atoms),
            ("erlang", "dt_append_vm_tag_data", 1)
            | ("erlang", "dt_prepend_vm_tag_data", 1) => self.dt_vm_tag_data(args),
            ("erlang", "system_profile", 0) => self.system_profile(atoms),
            // The code-store primitives the runtime can answer from its own real state.
            ("erlang", "check_old_code", 1) => self.check_old_code(atoms, args),
            ("erlang", "finish_loading", 1) => self.finish_loading(atoms, args),
            ("erlang", "call_on_load_function", 1)
            | ("erlang", "has_prepared_code_on_load", 1)
            | ("erlang", "finish_after_on_load", 2) => self.badarg_always(),
            // Internal process-state predicates and the persistent-term reset hook.
            ("erts_internal", "is_system_process", 1) => {
                self.is_process_predicate(atoms, args, "is_system_process")
            }
            ("erts_internal", "is_process_executing_dirty", 1) => {
                self.is_process_predicate(atoms, args, "is_process_executing_dirty")
            }
            ("erts_internal", "erase_persistent_terms", 0) => self.erase_persistent_terms(atoms),
            ("erts_internal", "scheduler_wall_time", 1) => {
                self.scheduler_wall_time(atoms, args)
            }
            // The internal `notsup` probes and the trace-cleaner / error-logger constants.
            ("erts_literal_area_collector", "release_area_switch", 0) => self.notsup(),
            ("erts_trace_cleaner", "check", 0) => self.trace_cleaner_check(atoms),
            ("error_logger", "warning_map", 0) => self.warning_map(atoms),
            // The `code:*` coverage-mode surface (see `set_coverage_mode`).
            ("code", "coverage_support", 0) => atom_term(atoms, "true"),
            ("code", "get_coverage_mode", 0) => atom_term(atoms, "none"),
            ("code", "set_coverage_mode", 1) => self.set_coverage_mode(atoms, args),
            // Unreachable: `RuntimeContext::uses_runtime_hooks` gates this bridge to exactly
            // the keys above. Reporting the same typed capability gap the defaulted seam
            // reports keeps an ungated key honest rather than fabricated (workspace
            // `CONTEXT.md` §3).
            _ => Err(error::unsupported(&format!(
                "{module}:{function}/{}",
                args.len()
            ))),
        }
    }
}

/// Every key the oracle's `erlang:statistics/1` accepts beyond the ones this runtime serves
/// (`erts/emulator/beam/erl_bif_info.c:4169-4332`, plus the `ERTS_ENABLE_MSACC`-gated
/// `microstate_accounting`).
///
/// The list exists to tell a **valid** key this runtime cannot measure (the honest typed
/// capability gap, [`RuntimeHooksBridge::statistics`]) from a name that is not a statistics
/// key at all (the oracle's `badarg`) — the only two answers, since a fabricated figure is
/// never acceptable (workspace `CONTEXT.md` §3). A key this runtime *does* serve has its own
/// arm in [`RuntimeHooksBridge::statistics`] and must **not** appear here.
///
/// Each survivor names a piece of per-scheduler state this runtime genuinely lacks:
///
/// * `active_tasks`/`active_tasks_all`/`total_active_tasks`/`total_active_tasks_all` add, to
///   each run queue's length, `1` when a scheduler is **currently executing a process from
///   that queue** (the C's `ERTS_RUNQ_FLG_EXEC`, `erl_process.c:9266-9267`). This runtime's
///   `beam_process::Scheduler` dequeues a pid into a local and records **no** current-process
///   state, so that increment cannot be derived truthfully — reporting the bare queue length
///   would silently answer a different question.
/// * `microstate_accounting` gathers the MSACC per-thread microstate counters
///   (`erts_msacc_request`, `erl_bif_info.c:4232-4238`); this runtime keeps no such
///   accounting and has no scheduler threads to describe.
const STATISTICS_KEYS: &[&str] = &[
    "total_active_tasks",
    "total_active_tasks_all",
    "active_tasks",
    "active_tasks_all",
    "microstate_accounting",
];

/// The `{Total, SinceLast}` pair of small integers the three counter keys answer with.
///
/// # Errors
///
/// Propagates the heap's failure when the tuple cannot be built.
fn counter_pair(heap: &mut Heap, (total, since): (u64, u64)) -> Result<Eterm> {
    heap.tuple(&[small_u64(total), small_u64(since)])
}

/// `value` as a small-integer term, saturated at `i64::MAX`.
///
/// A counter is `u64` so it can never wrap, while a term's small integer is signed; the
/// saturation keeps the conversion total without ever trapping.
fn small_u64(value: u64) -> Eterm {
    make_small(i64::try_from(value).unwrap_or(i64::MAX))
}

thread_local! {
    /// The wall-clock anchor and the previous `statistics(wall_clock)` read, in one slot.
    ///
    /// A `thread_local` (rather than a `Runtime` field) because this seam may not touch the
    /// runtime's layout, and it is sound because the runtime is single-threaded (`!Send`,
    /// the same rationale `src/context/trace.rs`'s slot uses). The `None` state is the
    /// first read: the anchor is then *now*, which is the runtime's documented wall-clock
    /// epoch (see [`RuntimeHooksBridge::statistics`]).
    static WALL_CLOCK: Cell<Option<(Instant, Instant)>> = const { Cell::new(None) };
}

thread_local! {
    /// The node-global `sched_wall_time` switch behind
    /// `erts_internal:scheduler_wall_time/1` — the C's `erts_atomic32_t sched_wall_time`
    /// (`erts/emulator/beam/erl_bif_info.c`), which starts disabled.
    ///
    /// A `thread_local` for the same reason [`WALL_CLOCK`] is one: the seam may not touch the
    /// runtime's layout, and it is sound because the runtime is single-threaded (`!Send`). The
    /// switch is recorded but no sampling is driven from it (see
    /// [`RuntimeHooksBridge::scheduler_wall_time`]).
    static SCHEDULER_WALL_TIME: Cell<bool> = const { Cell::new(false) };
}
/// The wall clock in whole milliseconds: `(since the anchor, since the previous read)`.
///
/// `Instant` is the platform's monotonic clock, so the figures are real elapsed time and
/// never go backwards. The first read returns `(0, 0)` — the oracle's first read returns
/// the same value for both elements too, only measured from emulator start rather than from
/// this anchor (see [`RuntimeHooksBridge::statistics`]).
fn wall_clock_millis() -> (i64, i64) {
    let now = Instant::now();
    WALL_CLOCK.with(|slot| {
        // An unanchored slot is its own anchor and its own "previous read".
        let (base, last) = slot.get().unwrap_or((now, now));
        slot.set(Some((base, now)));
        (
            whole_millis(now.saturating_duration_since(base)),
            whole_millis(now.saturating_duration_since(last)),
        )
    })
}

/// `duration` in whole milliseconds, saturated at `i64::MAX` (a `Duration` can exceed an
/// `i64` of milliseconds only after ~292 million years, but the conversion must not trap).
fn whole_millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// A `{Key, Value}` option tuple, or `badarg` for anything else.
///
/// # Errors
///
/// `badarg` for a non-tuple, a tuple of another arity, or a key that is not an atom of the
/// caller's shared table.
fn option_pair(atoms: &AtomTable, option: Eterm) -> Result<(String, Eterm)> {
    if terms::tuple_arity(option).map_err(|_| BeamError::BadArg)? != 2 {
        return Err(BeamError::BadArg);
    }
    let key = terms::tuple_element(option, 0).map_err(|_| BeamError::BadArg)?;
    let value = terms::tuple_element(option, 1).map_err(|_| BeamError::BadArg)?;
    if !is_atom(key) {
        return Err(BeamError::BadArg);
    }
    let key = atoms.name(atom_val(key)).ok_or(BeamError::BadArg)?;
    Ok((key.to_string(), value))
}

/// The boolean the atom `value` names, or `badarg`.
///
/// # Errors
///
/// `badarg` for anything but the atoms `true`/`false` — the oracle rejects `{abs, 1}` and
/// `{info, 1}` alike.
fn boolean_value(atoms: &AtomTable, value: Eterm) -> Result<bool> {
    if !is_atom(value) {
        return Err(BeamError::BadArg);
    }
    match atoms.name(atom_val(value)) {
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        _ => Err(BeamError::BadArg),
    }
}

/// The local pid `term` names, or the oracle's rejection of it.
///
/// The **local-only** classifier, and the one place this bridge spells the "not a pid ⇒
/// `badarg`" rule for a pid-taking key. Its remote arm is the typed capability gap for a
/// caller that has no peer-side answer to give (`distribution is not implemented`); a key
/// whose oracle semantics for a remote pid *are* implementable must classify with its own
/// entry point instead of this one — [`link_target`] is that sibling for
/// `erlang:link/1`/`unlink/1`.
///
/// # Errors
///
/// `badarg` for anything that is not a pid; the typed capability gap for a **remote** pid —
/// never a fabricated local pid.
fn local_pid(term: Eterm, what: &str) -> Result<Eterm> {
    match PidTarget::from_eterm(term) {
        Ok(PidTarget::Local(_)) => Ok(term),
        Ok(PidTarget::Remote(_)) => Err(error::unsupported(&format!(
            "erlang:{what}/1 on a remote pid: distribution is not implemented"
        ))),
        Err(_) => Err(BeamError::BadArg),
    }
}

/// The `erlang:link/1`/`unlink/1` target `term` names, or the oracle's rejection of it.
///
/// A **local** pid is classified by [`local_pid`] — the local-only classifier, so the
/// "not a pid ⇒ `badarg`" rule for this arm stays spelled in exactly one place — while a
/// **remote** (external) pid is accepted unchanged: linking a process on another node is a
/// **remote link**, which [`ProcessTable::link_term`]/`unlink_term` record in the caller's
/// remote-link set and route as a wire `DOP_LINK`/`DOP_UNLINK_ID` control signal
/// ([`RuntimeHooksBridge::link`] carries the full contract). This is deliberately *not* the
/// blanket capability gap the local-only classifier reports for a remote pid — that guard
/// remains in force for the keys that genuinely need peer state
/// ([`RuntimeHooksBridge::is_process_predicate`],
/// [`RuntimeHooksBridge::dflag_unicode_io`], which do not go through here).
///
/// # Errors
///
/// `badarg` for anything that is not a pid — the same reason [`local_pid`] reports.
fn link_target(term: Eterm) -> Result<Eterm> {
    match PidTarget::from_eterm(term) {
        Ok(PidTarget::Local(_)) => local_pid(term, "link"),
        Ok(PidTarget::Remote(_)) => Ok(term),
        Err(_) => Err(BeamError::BadArg),
    }
}

/// The emulator's **declared** builtin roster (`erts/emulator/beam/bif.tab`), parsed once.
///
/// `bif.tab` is the source of truth for which `M:F/A` the emulator itself implements — the
/// table `erts_is_builtin` consults (`erts/emulator/beam/bif.c:4409-4424`). It is read
/// **once per OS process** (the file cannot change under a running runtime) and memoized in
/// a [`OnceLock`], so a call never touches the filesystem.
///
/// `None` covers both the file being absent (a bare container with no OTP source tree
/// above the crate — [`read_real_bif_tab`](beam_bifs::bif_tab::read_real_bif_tab) answers
/// `None`) and the file being unparsable (corrupted source data, reported once on stderr).
/// Either way [`is_builtin`] falls back to the `erlang`-namespace extras alone, so the
/// predicate can only ever *under*-report builtin-ness — never claim it without evidence.
pub(super) fn declared_bif_tab() -> Option<&'static BifTab> {
    static CACHE: OnceLock<Option<BifTab>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let text = beam_bifs::bif_tab::read_real_bif_tab()?;
            match BifTab::parse(&text) {
                Ok(tab) => Some(tab),
                Err(err) => {
                    // A malformed `bif.tab` is a bug in the source data, not a runtime
                    // failure, so it is reported rather than swallowed silently.
                    eprintln!("beam_rt: cannot parse bif.tab: {err}");
                    None
                }
            }
        })
        .as_ref()
}

/// Whether `M:F/A` is an **emulator builtin** — the `erts_is_builtin` half of
/// `erlang:function_exported/3` (`erts/emulator/beam/bif.c:4424`).
///
/// The emulator's builtin table is `bif.tab` **plus** the extra builtins it registers
/// itself, which all live in the `erlang` module (`erlang:memory/0,1` here; this tree's
/// `bif.tab` declares `pid_to_binary`/`ref_to_binary`/`port_to_binary` itself). Hence:
///
/// ```text
/// declared(module, name, arity) || (module == "erlang" && registry contains it)
/// ```
///
/// The `beam_bifs` registry is deliberately **not** the source of truth for any other
/// module: it also carries the stdlib *fast paths* — this runtime's implementations of real
/// *Erlang* functions such as `lists:map/2` — which `erts_is_builtin` does not know and
/// which must not make an unloaded module look exported.
///
/// When `tab` is `None` (no readable `bif.tab`) the declared half is unavailable and only
/// the `erlang`-namespace extras are honoured; see [`declared_bif_tab`].
pub(super) fn is_builtin(
    tab: Option<&BifTab>,
    registry: &BifRegistry,
    module: &str,
    function: &str,
    arity: u32,
) -> bool {
    let declared = tab.is_some_and(|tab| tab.contains(module, function, arity));
    declared || (module == "erlang" && registry.contains(module, function, arity))
}

/// Whether the loaded `module` exports `function/arity`.
///
/// The module's own atom table names its `ExpT` entries (the loader interns them there),
/// so the comparison is by **name**, never by index — a caller's atom index is a different
/// namespace.
fn module_exports(module: &beam_loader::Module, function: &str, arity: u32) -> bool {
    module.exports.iter().any(|export| {
        export.arity == arity && module.atom_name(export.function) == Some(function)
    })
}

/// An interned atom's term, resolved by **name** through `atoms`.
///
/// The runtime's boot seed (see [`CORE_ATOMS`](crate::boot::CORE_ATOMS)) holds every atom
/// this bridge emits (`timeout`, `true`, `false`, `ok`), so the lookup always succeeds.
///
/// # Errors
///
/// The typed capability gap for a name the shared table does not hold — never a substituted
/// atom index.
fn atom_term(atoms: &AtomTable, name: &str) -> Result<Eterm> {
    match atoms.lookup(name) {
        Some(atom) => Ok(beam_term::make_atom(atom)),
        None => Err(error::unsupported(&format!(
            "runtime hook: the atom `{name}` is not interned"
        ))),
    }
}
