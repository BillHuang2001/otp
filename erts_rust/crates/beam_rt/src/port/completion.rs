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

//! The **driver→owner completion channel**: `beam_io`'s term-free
//! [`PortCompletion`](beam_io::PortCompletion) turned into the two messages an inet port's
//! owner is owed.
//!
//! # What it is for
//!
//! A socket driver cannot report the outcome of an `erlang:port_command/2` through a control
//! op's return value, and it cannot report a queued asynchronous operation there either: the C
//! driver API *sends a message to the port's owner* instead
//! (`erl_drv_send_term`/`driver_send_term`). Two message families ride that path:
//!
//! | Completion | Message | C |
//! |---|---|---|
//! | [`CompletionEvent::InetReply`] | `{inet_reply, Port, ok}` / `{inet_reply, Port, {error, R}}`, plus the decoded caller tag as a fourth element when one was sent | `inet_reply_finish`, `inet_drv.c:2614-2637` |
//! | [`CompletionEvent::InetAsync`] | `{inet_async, Port, Id, R}` where `R` is `ok`, `{ok, Data}`, `{ok, Port2}` or `{error, R}` | `send_async_ok`/`send_async_ok_port`/`send_async_error`, `inet_drv.c:2493-2562` |
//!
//! Without this half, `prim_inet:send/4` and its siblings block for ever in
//! `receive {inet_reply, S, Status, Mref}` (`erts/preloaded/src/prim_inet.erl`), which is what
//! made `gen_udp:send/4`, `gen_udp:recv/3`, `gen_tcp:accept` and `gen_tcp:recv` unreachable.
//!
//! # The three steps
//!
//! 1. **Drain** ([`PortRegistry::drain_io_completions`]) — pull everything
//!    [`beam_io::PortRegistry::take_completions`] has staged and bind each completion to the
//!    facts its recipient needs (`owner`, `stream`) *now*, while the port's metadata is still
//!    readable. Completions are durable (`beam_io` keeps them on the port until drained), so a
//!    drain sees everything any earlier round staged.
//! 2. **Adopt** an [`InetAsyncResult::Accepted`] descriptor into a real port
//!    ([`PortRegistry::adopt_accepted`]) and rewrite the result to `{ok, NewPort}`.
//! 3. **Render** ([`RuntimeContext::completion_message`]) — build the Erlang tuple, on the
//!    same pointer-stable arena and through the same stream shaper as every other port
//!    message.
//!
//! # Where the drain happens (every round, and every close)
//!
//! A completion is staged by a driver callback, so the drain must follow **every** point a
//! driver callback can run: [`PortRegistry::pump`](super::PortRegistry) after each port-task
//! round (and after the close-report round on its "nothing live to poll" path),
//! [`PortRegistry::write_command`](super::PortRegistry) after both of its branches (the
//! `OUTPUT` task that carries a command is what stages `gen_udp:send`'s `InetReply`),
//! [`PortRegistry::port_control`](super::PortRegistry) defensively, and
//! [`PortRegistry::close`](super::PortRegistry) after the teardown — which is where a driver's
//! `async_error_am_all` answers every queued operation with `{error, closed}`, harvested by
//! `beam_io::PortRegistry::close` into a registry-owned queue reported exactly once by the next
//! drain.
//!
//! # The recipient is the port's **owner**, not the C's `desc->caller`
//!
//! The C addresses a completion to `desc->caller` — the process that issued the control op that
//! queued the operation. The frozen [`beam_io::Driver`] seam carries no caller, so this runtime
//! addresses the port's **owner** ([`super::PortMeta::owner`]). For every `prim_inet` caller the
//! two are the same process (the owner is the one that issued the control op), so the divergence
//! is unobservable in practice; it is recorded in `src/port/CONTEXT.md` regardless.
//!
//! # Term-free until the last moment
//!
//! A [`CompletionPacket`] carries no term: the caller tag travels **verbatim** as the bytes of
//! `term_to_binary(Mref, [local])` and a reason travels as the atom *name* it must become
//! ([`InetReason::atom_name`]). Everything is materialized in
//! [`RuntimeContext::completion_message`], **after** the port registry's borrow has been
//! released — the same snapshot rule the `{Port, {data, …}}` builders follow.

use std::cell::Cell;
use std::collections::VecDeque;
use std::os::fd::RawFd;

use beam_io::{
    CompletionEvent, Driver, FdDriver, InetAsyncResult, InetReason, InetReply, PortCompletion,
    TcpInetDriver,
};
use beam_process::{PortId, ProcessId};
use beam_term::{make_atom, make_small, Atom, AtomTable, Eterm};

use super::{
    PortEvent, PortMeta, PortName, PortRegistry, RuntimeContext, StreamShape, StreamType,
};
/// One drained completion, bound to the port that produced it **and** to everything its
/// delivery still needs once that port's metadata may be gone.
///
/// The `owner`/`stream` pair is resolved at **drain** time (see
/// [`PortRegistry::drain_io_completions`]) rather than at delivery time, because a completion
/// staged around a port's teardown outlives the port's [`PortMeta`] — the C's
/// `async_error_am_all` case — and a message with no recoverable recipient would be lost.
#[derive(Debug)]
pub(crate) struct CompletionPacket {
    /// The port whose driver produced the completion (the C's `desc->dport`), i.e. the `Port`
    /// element of both message families.
    pub(crate) port: PortId,
    /// The process the message belongs to — the port's owner (see the module docs on the
    /// addressee divergence).
    pub(crate) owner: ProcessId,
    /// The port's `binary`/`list` option, which decides how an
    /// [`InetAsyncResult::Data`] payload is rendered.
    pub(crate) stream: StreamType,
    /// What the driver handed up, already rewritten by
    /// [`PortRegistry::adopt_accepted`] when it carried an accepted descriptor.
    pub(crate) event: CompletionEvent,
}

/// The facts a completion still needs from a port's [`PortMeta`] **after** that metadata is
/// gone.
///
/// [`PortRegistry::close`](super::PortRegistry) snapshots one of these before it removes the
/// port (the port's own `{inet_reply, …}`/`{inet_async, …}` must still reach its owner), and
/// passes it to [`PortRegistry::drain_io_completions`] so the drain can attribute a completion
/// whose port no longer has metadata. It is keyed by port number, so a **recycled** number can
/// never inherit the snapshot: the drain only consults it for the very port that was closed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClosedPortSnapshot {
    /// The port that was closed.
    pub(crate) port: PortId,
    /// The process that owned it — the recipient of anything it still owes.
    pub(crate) owner: ProcessId,
    /// Its `binary`/`list` option.
    pub(crate) stream: StreamType,
    /// Its line framing, carried to an accepted socket the way `open_port/2`'s driver-port arm
    /// carries it.
    pub(crate) line: Option<usize>,
}

impl PortRegistry {
    /// The [`ClosedPortSnapshot`] of `id`, or `None` when this registry does not hold it.
    ///
    /// [`PortRegistry::close`] takes it **before** it removes the port's metadata, which is the
    /// only moment the recipient is still readable.
    pub(super) fn closed_snapshot(&self, id: PortId) -> Option<ClosedPortSnapshot> {
        self.meta.get(&id.number()).map(|meta| ClosedPortSnapshot {
            port: id,
            owner: meta.owner,
            stream: meta.stream,
            line: meta.line,
        })
    }

    /// Drains **both** channels a driver callback can hand work up through, at every point a
    /// callback can run: the driver-failure channel
    /// ([`PortRegistry::drain_failures`], which finishes the teardown of a port a driver closed
    /// itself and publishes the `'EXIT'`/`'DOWN'` it owes) and then this one
    /// ([`PortRegistry::sweep_completions`]).
    ///
    /// The two are one entry point rather than two calls because a driver's callback is what
    /// stages **both** — the C's `driver_failure*` is a port-closing request and a socket
    /// driver's `{inet_reply, …}` is a message, both from inside `output`/`control`/`ready_*` —
    /// so every drain point owes both sweeps and no site can forget one. A failure's port is
    /// gone from [`PortRegistry::meta`] by the time the completion sweep runs, and its
    /// [`ClosedPortSnapshot`] is folded into the fallback list, so an operation it left queued
    /// (`async_error_am_all`) still reaches its owner.
    ///
    /// `fallback` is the [`ClosedPortSnapshot`] of the port a caller has just closed: a
    /// completion harvested from a port that is no longer in [`PortRegistry::meta`] is
    /// attributed through it. Anything else with no metadata has no recoverable recipient and is
    /// dropped — the rule every other port-event harvest follows for a port that has left the
    /// runtime (and an [`InetAsyncResult::Accepted`] descriptor in that state is **closed**, so
    /// it cannot leak).
    pub(super) fn drain_io_completions(
        &mut self,
        fallback: Option<ClosedPortSnapshot>,
    ) -> Vec<PortEvent> {
        let mut fallbacks = self.drain_failures();
        fallbacks.extend(fallback);
        self.sweep_completions(&fallbacks)
    }

    /// Drains every completion `beam_io` has staged into term-free [`PortEvent`]s, adopting any
    /// accepted descriptor on the way.
    ///
    /// `fallbacks` are the [`ClosedPortSnapshot`]s of every port that has just left the runtime
    /// in this same drain — the one the caller closed, plus every one a driver closed itself
    /// ([`PortRegistry::drain_failures`]) — because a completion staged around a teardown
    /// outlives its [`PortMeta`] (the C's `async_error_am_all`). They are matched by port
    /// **number**, so a recycled number can never inherit one.
    ///
    /// The [`InetAsyncResult::Accepted`] rewrite is the C's `tcp_inet_copy`
    /// (`inet_drv.c:11834`): a driver in this crate cannot create a port, so the descriptor
    /// crosses the channel and the port is opened here
    /// ([`PortRegistry::adopt_accepted`]) — before any term is built.
    fn sweep_completions(&mut self, fallbacks: &[ClosedPortSnapshot]) -> Vec<PortEvent> {
        let drained = self.registry.take_completions();
        let mut events = Vec::with_capacity(drained.len());
        for PortCompletion { port, event } in drained {
            let resolved = match self.meta.get(&port.number()) {
                Some(meta) => Some((meta.owner, meta.stream, meta.line)),
                None => fallbacks
                    .iter()
                    .find(|snapshot| snapshot.port == port)
                    .map(|snapshot| (snapshot.owner, snapshot.stream, snapshot.line)),
            };
            let Some((owner, stream, line)) = resolved else {
                if let CompletionEvent::InetAsync {
                    result: InetAsyncResult::Accepted { fd },
                    ..
                } = event
                {
                    // No recipient and no adopter: the descriptor is this runtime's to release.
                    discard_descriptor(fd);
                }
                continue;
            };
            let event = match event {
                CompletionEvent::InetAsync {
                    id,
                    result: InetAsyncResult::Accepted { fd },
                } => CompletionEvent::InetAsync {
                    id,
                    result: self.adopt_accepted(fd, owner, stream, line),
                },
                other => other,
            };
            events.push(PortEvent::Completion {
                packet: CompletionPacket {
                    port,
                    owner,
                    stream,
                    event,
                },
            });
        }
        events
    }

    /// Adopts the connection descriptor an `accept` operation took off a listening socket into a
    /// new `tcp_inet` port owned by the same process — the runtime half of the C's
    /// `tcp_inet_copy` (`inet_drv.c:11834`), which the driver cannot do itself.
    ///
    /// The descriptor is **owned by the completion** the moment it is staged
    /// ([`InetAsyncResult::Accepted`]): `beam_io` never stores it and never closes it, so this is
    /// the only place it can be adopted or released. Every path here either hands it to a driver
    /// (which closes it on teardown) or closes it ([`discard_descriptor`]) — it is never leaked.
    ///
    /// The `PortMeta` is the same block `open_port/2`'s built-in-driver arm builds
    /// (`super::RuntimePorts::open_port`): a [`PortName::SpawnDriverBuiltin`] port with no OS
    /// child, so it reports `os_pid` `undefined` and never synthesizes an `exit_status`, and with
    /// the driver's own `control` capability (`tcp_inet` declares `inet_control`, so
    /// `port_control/3` reaches it exactly as it does on a port `prim_inet:open/5` opened). The
    /// stream option is inherited from the **listening** port — `prim_inet:open/5` always opens
    /// the driver port `[binary]` and `prim_inet:accept_opts/3` then sets the accepted socket's
    /// mode, so inheriting is what makes the two agree.
    ///
    /// # Reasons
    ///
    /// A descriptor `beam_io` cannot describe as a socket answers `{error, ebadf}` — the errno
    /// the kernel itself reports for a descriptor that is not one, and the only one available
    /// here (the frozen [`beam_io::Driver`] constructor seam hands back a `BeamError`, not an
    /// `errno`). A port table with no number left answers `{error, emfile}`, the C's own answer
    /// to an `accept` on a node that cannot create another port. Neither is silent: the caller
    /// sees the failure as the operation's result.
    fn adopt_accepted(
        &mut self,
        fd: RawFd,
        owner: ProcessId,
        stream: StreamType,
        line: Option<usize>,
    ) -> InetAsyncResult {
        let driver = match TcpInetDriver::from_socket(fd) {
            Ok(driver) => driver,
            Err(_) => {
                discard_descriptor(fd);
                return InetAsyncResult::Error(InetReason::from_errno(libc::EBADF));
            }
        };
        let driver_has_control = driver.has_control();
        let meta = PortMeta {
            // `port_info(Port, name)` is the driver's own name, as it is for any driver port.
            command: beam_io::drivers::inet::DRIVER_NAME.to_string(),
            os_pid: None,
            stream,
            line,
            packet: None,
            notify_exit: false,
            monitors: Vec::new(),
            owner,
            kind: PortName::SpawnDriverBuiltin,
            driver_has_control,
            driver_terms: None,
            driver_atoms: None,
            driver_control_flags: None,
            driver_busy: None,
            driver_monitors: None,
            input: Cell::new(0),
            output: Cell::new(0),
            exit_status_taken: Cell::new(true),
            held_lines: VecDeque::new(),
            held_since: Cell::new(None),
        };
        // A failed `register` tears its driver down (`beam_io::PortRegistry::open`'s contract),
        // which closes the descriptor — so this path leaks nothing either.
        match self.register(Box::new(driver), meta) {
            Ok(port) => InetAsyncResult::Port(port),
            Err(_) => InetAsyncResult::Error(InetReason::from_errno(libc::EMFILE)),
        }
    }
}

/// Releases `fd`, discarding the outcome.
///
/// This crate denies `unsafe_code`, so it cannot call `close(2)` itself; `beam_io`'s own
/// descriptor-owning driver is the safe way to hand a raw descriptor over. `FdDriver::read_only`
/// adopts it without duplicating it and closes it when dropped (its `close_fds` runs from
/// `stop` and from `Drop`), and it touches no syscall in between — `Driver::start` is what puts
/// the descriptor in non-blocking mode, and it is never started here.
///
/// Used on the two paths where an accepted descriptor cannot become a port. Both are unreachable
/// for a descriptor `accept(2)` just produced, but a leaked descriptor is worse than a dropped
/// one, so neither is left to chance.
fn discard_descriptor(fd: RawFd) {
    drop(FdDriver::read_only(fd));
}

impl RuntimeContext<'_> {
    /// The message one [`CompletionPacket`] delivers to its owner.
    ///
    /// Built on the runtime's pointer-stable `-eval` arena (like every other port message) after
    /// the port registry's borrow has been released, with every atom interned in the runtime's
    /// **one shared** table so the printer can name it and a `receive` clause can match it.
    ///
    /// # Errors
    ///
    /// The arena's allocation failure when a term does not fit.
    pub(crate) fn completion_message(&mut self, packet: &CompletionPacket) -> beam_common::Result<Eterm> {
        match &packet.event {
            CompletionEvent::InetReply { caller_ref, reply } => {
                self.inet_reply_message(packet.port, caller_ref.as_deref(), reply)
            }
            CompletionEvent::InetAsync { id, result } => {
                self.inet_async_message(packet.port, *id, result, packet.stream)
            }
        }
    }

    /// `{inet_reply, Port, Reply}` — or the 4-tuple with the caller's tag appended.
    ///
    /// The C's `inet_reply_finish` (`inet_drv.c:2614-2637`) sends the 3-tuple and, when the
    /// command carried a caller tag (`desc->caller_ref`), appends the *decoded* tag as a fourth
    /// element — which is what `prim_inet:send/4` matches on
    /// (`{inet_reply, S, Status, Mref}`). A tag this runtime cannot decode keeps the tuple
    /// 3-wide rather than inventing a fourth element: the caller then sees a divergence (exit
    /// code or a non-matching message), never a wrong term.
    ///
    /// `Reply` is `ok` or `{error, Reason}` with the reason's atom from
    /// [`InetReason::atom_name`].
    fn inet_reply_message(
        &mut self,
        port: PortId,
        caller_ref: Option<&[u8]>,
        reply: &InetReply,
    ) -> beam_common::Result<Eterm> {
        let (tag, ok, error) = {
            let atoms = self.runtime.code_server.atoms_mut();
            (
                make_atom(atoms.intern("inet_reply")),
                make_atom(atoms.intern("ok")),
                make_atom(atoms.intern("error")),
            )
        };
        let status = match reply {
            InetReply::Ok => ok,
            InetReply::Error(reason) => {
                let name = reason.atom_name();
                let reason = make_atom(self.runtime.code_server.atoms_mut().intern(&name));
                terms_tuple(self, &[error, reason])?
            }
        };
        let mut elements = vec![tag, port.to_eterm(), status];
        if let Some(bytes) = caller_ref {
            if let Ok(decoded) = self.decode_caller_tag(bytes) {
                elements.push(decoded);
            }
        }
        terms_tuple(self, &elements)
    }

    /// `{inet_async, Port, Id, Result}` — the completion of a queued asynchronous operation.
    ///
    /// `Id` is the 16-bit id the operation's control-op reply handed out (the C's `int Ref`), a
    /// bare small integer. `Result` is the C's fourth element: `ok`, `{ok, Data}`,
    /// `{ok, Port2}` or `{error, Reason}`. `Data` is the read's payload rendered through the
    /// **same** shaper a `{Port, {data, …}}` message uses
    /// ([`RuntimeContext::make_stream_payload`](super::RuntimeContext) with
    /// [`StreamShape::Data`]), so a UDP datagram in `[binary]` mode is the C's improper
    /// `[AF, PortHi, PortLo, A, B, C, D | <<Payload>>]` and one in `[list]` mode a single flat
    /// integer list with the address first.
    fn inet_async_message(
        &mut self,
        port: PortId,
        id: u16,
        result: &InetAsyncResult,
        stream: StreamType,
    ) -> beam_common::Result<Eterm> {
        let (tag, ok, error) = {
            let atoms = self.runtime.code_server.atoms_mut();
            (
                make_atom(atoms.intern("inet_async")),
                make_atom(atoms.intern("ok")),
                make_atom(atoms.intern("error")),
            )
        };
        let answer = match result {
            InetAsyncResult::Ok => ok,
            InetAsyncResult::Data(read) => {
                let payload = self.make_stream_payload(read, stream, StreamShape::Data)?;
                terms_tuple(self, &[ok, payload])?
            }
            InetAsyncResult::Port(accepted) => terms_tuple(self, &[ok, accepted.to_eterm()])?,
            InetAsyncResult::Error(reason) => {
                let name = reason.atom_name();
                let reason = make_atom(self.runtime.code_server.atoms_mut().intern(&name));
                terms_tuple(self, &[error, reason])?
            }
            InetAsyncResult::Accepted { fd } => {
                // The drain adopts every accepted descriptor (`PortRegistry::adopt_accepted`),
                // so a packet that still carries one never came through it. It is released
                // rather than leaked, and reported as the C's teardown reason instead of a
                // fabricated `{ok, Port}`.
                let fd = *fd;
                discard_descriptor(fd);
                let closed = make_atom(self.runtime.code_server.atoms_mut().intern("closed"));
                terms_tuple(self, &[error, closed])?
            }
        };
        let id = make_small(i64::from(id));
        terms_tuple(self, &[tag, port.to_eterm(), id, answer])
    }

    /// Decodes the caller tag a `{inet_reply, …}` completion carries — the
    /// `term_to_binary(Mref, [local])` image `prim_inet:send/3` prefixed its command with.
    ///
    /// The tag is a reference image, so it names no atom; the decode still uses the runtime's
    /// **one shared** table through the per-thread scratch image
    /// ([`crate::nif::with_decode_scratch`], the same entry point
    /// [`DriverPort::binary_to_term`](super::driver::DriverPort) uses) and lands any name it
    /// did append back into the live table in intern order, so a tag that *did* carry a name
    /// cannot resolve against a scratch index the runtime's table does not hold.
    ///
    /// The term lives on the runtime's never-reset `-eval` arena, so it stays valid for the
    /// runtime's lifetime — the same home every other port message is built on.
    ///
    /// # Errors
    ///
    /// Whatever [`beam_codec::decode_term`] reports for a malformed image (the caller then keeps
    /// the 3-tuple), or the arena's allocation failure.
    fn decode_caller_tag(&mut self, bytes: &[u8]) -> beam_common::Result<Eterm> {
        let (decoded, appended) = {
            // Disjoint field borrows of `Runtime`: the table is read-only here, the arena is the
            // destination. Committing a name is a separate step below, once this borrow is gone.
            let atoms: &AtomTable = self.runtime.code_server.atoms();
            let heap = &mut self.runtime.eval_heap;
            crate::nif::with_decode_scratch(atoms, |scratch| {
                let known = scratch.len();
                let decoded = beam_codec::decode_term(bytes, heap, scratch);
                let appended: Vec<String> = (known..scratch.len())
                    .filter_map(|index| {
                        scratch
                            .name(Atom::from_raw(index as u32))
                            .map(str::to_owned)
                    })
                    .collect();
                (decoded, appended)
            })
        };
        let term = decoded?;
        if !appended.is_empty() {
            let atoms = self.runtime.code_server.atoms_mut();
            for name in &appended {
                atoms.intern(name);
            }
        }
        Ok(term)
    }
}

/// `terms::make_tuple` over a runtime context, spelled once so the builders in this module read
/// as the tuples they build.
///
/// # Errors
///
/// The arena's allocation failure when the tuple does not fit.
fn terms_tuple(ctx: &mut RuntimeContext<'_>, elements: &[Eterm]) -> beam_common::Result<Eterm> {
    beam_engine::terms::make_tuple(ctx, elements)
}

