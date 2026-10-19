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

use beam_common::BeamError;
use beam_io::{
    CompletionEvent, Driver, InetAsyncResult, InetReason, InetReply, PortCompletion,
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
                    result: self.adopt_accepted(port, fd, owner, stream, line),
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
    /// # Accepted-socket option inheritance
    ///
    /// The C's `tcp_inet_copy` copies the listening descriptor's whole option block into the
    /// accepted one (`inet_drv.c:11810-11827`). This crate's drivers cannot create a port
    /// ([`InetAsyncResult::Accepted`] hands the bare descriptor up), but the *copy itself* is the
    /// driver's own: [`TcpInetDriver::from_accepted(fd, listener)`] adopts the descriptor and takes
    /// the listener's option store (`SocketOptions::inherited_from`) plus the two cached socket tags
    /// (`stype`/`family`) across in one call. So the **option store** half of `tcp_inet_copy` needs
    /// nothing from this module — no record is retained for a replay and none is replayed.
    ///
    /// Reaching that call is what [`beam_io::PortRegistry::driver_as`] exists for: a port owns its
    /// driver as a `Box<dyn Driver>`, so the *listener's* `&TcpInetDriver` is recoverable only
    /// through the read-only downcast ([`beam_io::Driver::downcast_ref`]). The downcast borrows the
    /// registry **shared**; `from_accepted` is pure (no poller, no port table), so the driver is
    /// built and the two copied latches are read while that borrow is held, and it ends before
    /// [`PortRegistry::register`](super::PortRegistry) needs the registry exclusively.
    ///
    /// What the driver **cannot** carry across is the other half: the two fields that are this
    /// runtime's mirror rather than the driver's option store. The C copies exactly those two —
    /// `inet.mode` (`inet_drv.c:11811`) and `inet.exitf` (`:11812`) — so
    /// [`PortRegistry::seed_inet_latches`] installs both on the new port. `inet.active` is
    /// deliberately **not** copied: the C's copy list omits it (`:11814`-`:11844`), `inet_start`
    /// resets it (`:10967`/`:10932`), and `prim_inet:accept_opts/3`
    /// (`erts/preloaded/src/prim_inet.erl:502-517`) is what re-applies it from the Erlang side — the
    /// accepted port therefore starts **passive**, exactly like the oracle's.
    ///
    /// # Reasons
    ///
    /// A descriptor the constructor refuses answers the **constructor's own** errno: `from_accepted`
    /// reports the failing `getsockopt(2)`/`getpeername(2)` errno inside [`BeamError::Io`] and never
    /// rewrites it, so a descriptor that is not a socket is `enotsock`, not a fabricated `ebadf`.
    ///
    /// The two shapes that carry **no** errno are the unreachable-but-honest ones, and both answer
    /// `{error, closed}` — the C's own teardown reason (`async_error_am_all`,
    /// `inet_reply_error_am(desc, am_closed)`), never an invented number:
    ///
    /// * the listener's driver is **not** a `tcp_inet` one, or its port is no longer live
    ///   ([`beam_io::PortRegistry::driver_as`] answers `None`). Unreachable from `beam_io`'s accept
    ///   path — [`InetAsyncResult::Accepted`] is staged **only** by `TcpInetDriver`
    ///   (`beam_io/src/drivers/inet/accept.rs:345-384`), `UdpInetDriver` has no accept path, and the
    ///   `INET_REQ_FDOPEN` prebound route (`adopt_prebound`) copies nothing and stages no accept
    ///   completion (`inet_drv.c:5223`-`:5282`) — but a listener whose port has just been torn down
    ///   would reach it, so it is a documented, tested, descriptor-releasing answer rather than an
    ///   `unwrap`;
    /// * a constructor failure that is not an [`BeamError::Io`] (nothing in today's
    ///   `from_accepted`/`from_socket` produces one) — same route, for the same reason: no errno is
    ///   fabricated to fill the gap.
    ///
    /// A port table with no number left answers `{error, emfile}` — `beam_io`'s `open` has already
    /// torn the driver (and with it the descriptor) down, so nothing leaks there either.
    ///
    /// [`TcpInetDriver::from_accepted(fd, listener)`]: beam_io::TcpInetDriver::from_accepted
    fn adopt_accepted(
        &mut self,
        listener: PortId,
        fd: RawFd,
        owner: ProcessId,
        stream: StreamType,
        line: Option<usize>,
    ) -> InetAsyncResult {
        // The listener's driver, its *data* mode and its `exit_on_close` latch — all read under one
        // **shared** borrow of the registry, which ends here. `data_mode` falls back to the port
        // stream option (which `prim_inet` always opens `[binary]`) when no `{mode, …}` record was
        // ever seen; `inet_exit_on_close` falls back to the C's `exitf = 1`
        // (`inet_drv.c:10929`).
        let (mode, exit_on_close, driver) = {
            let Some(listener_driver) = self.registry.driver_as::<TcpInetDriver>(listener) else {
                return refuse_accepted(fd);
            };
            let mode = self.data_mode(listener, stream);
            let exit_on_close = self.inet_exit_on_close(listener);
            match TcpInetDriver::from_accepted(fd, listener_driver) {
                Ok(driver) => (mode, exit_on_close, driver),
                Err(error) => return refuse_accepted_error(fd, &error),
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
            // A built-in inet driver has no `erl_drv_set_os_pid` hook: `port_info(Port, os_pid)`
            // is this port's `undefined`.
            driver_os_pid: None,
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
            Ok(port) => {
                // The two latches the *driver* cannot carry across, because they are this
                // runtime's mirror and not its option store: the C's `inet.mode`
                // (`inet_drv.c:11811`) and `inet.exitf` (`:11812`). `inet.active` is deliberately
                // absent from the C's copy list (`:11814`-`:11844`), so the accepted port starts
                // passive.
                self.seed_inet_latches(port, mode, exit_on_close);
                InetAsyncResult::Port(port)
            }
            Err(_) => InetAsyncResult::Error(InetReason::from_errno(libc::EMFILE)),
        }
    }
}

/// Refuses an accepted descriptor for a reason that carries **no** errno, releasing it.
///
/// The answer is `{error, closed}` — the C's own teardown reason
/// (`inet_reply_error_am(desc, am_closed)`, `inet_drv.c:12690`) — never an invented `ebadf`/`einval`
/// (`InetReason::Closed`'s atom). Two shapes reach it, both documented on
/// [`PortRegistry::adopt_accepted`]: a listener [`beam_io::PortRegistry::driver_as`] cannot resolve
/// to a `TcpInetDriver`, and a constructor failure that carries no errno. Neither is reachable from
/// `beam_io`'s accept path; both are truthful and neither leaks.
fn refuse_accepted(fd: RawFd) -> InetAsyncResult {
    discard_descriptor(fd);
    InetAsyncResult::Error(InetReason::Closed)
}

/// Refuses an accepted descriptor a constructor **failure** produced, releasing it.
///
/// The errno is the constructor's own, read back from the error rather than defaulted:
/// [`TcpInetDriver::from_accepted`] answers the failing `getsockopt(2)`/`getpeername(2)` errno
/// inside [`BeamError::Io`] (`beam_io/src/drivers/inet/mod.rs:833-846`), so a descriptor that is
/// not a socket is `enotsock` — the kernel's own answer, which is what the oracle would report for
/// the same descriptor. A failure carrying no errno takes [`refuse_accepted`]'s route instead of
/// borrowing one.
///
/// The descriptor is still the caller's on **every** `Err`: `from_socket` evaluates
/// `is_listening`/`has_peer` *before* the struct literal whose `Drop` closes the socket, so a
/// partial `TcpInetDriver` never exists and never closes anything
/// (`beam_io/src/drivers/inet/mod.rs:918-920`).
fn refuse_accepted_error(fd: RawFd, error: &BeamError) -> InetAsyncResult {
    match error {
        BeamError::Io(io) => match io.raw_os_error() {
            Some(errno) => {
                discard_descriptor(fd);
                InetAsyncResult::Error(InetReason::from_errno(errno))
            }
            None => refuse_accepted(fd),
        },
        _ => refuse_accepted(fd),
    }
}

/// Releases `fd`, discarding the outcome.
///
/// This crate denies `unsafe_code`, so it cannot call `close(2)` itself: [`beam_io::close_fd`] is
/// `beam_io`'s public, errno-preserving close primitive, written for exactly this hand-off (its
/// module docs name [`InetAsyncResult::Accepted`] as the caller). The outcome is dropped
/// deliberately — the descriptor is released whatever `close(2)` answers, so a failure leaves the
/// caller with nothing better to do than carry on with the refusal it is already reporting (and a
/// destructor-like helper must not panic).
///
/// Used on every path where an accepted descriptor cannot become a port: the listener's driver is
/// not a `tcp_inet` one, the constructor refused the descriptor, or a descriptor arrived with no
/// recipient at all. Each arm is documented where it is taken, and every one releases.
fn discard_descriptor(fd: RawFd) {
    let _ = beam_io::close_fd(fd);
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
pub(super) fn terms_tuple(
    ctx: &mut RuntimeContext<'_>,
    elements: &[Eterm],
) -> beam_common::Result<Eterm> {
    beam_engine::terms::make_tuple(ctx, elements)
}


/// The A.4 **accepted-socket option inheritance** suite: a real loopback `accept(2)` whose
/// descriptor [`PortRegistry::adopt_accepted`] adopts through
/// [`TcpInetDriver::from_accepted`](beam_io::TcpInetDriver::from_accepted), the inherited
/// `tcp_inet_copy` block read back through the driver's own `getopts`/framed `send`, the two copied
/// latches, the negative `setopts` rows, and the two descriptor-releasing refusals (a constructor
/// failure's own errno, and the no-listener-driver arm) — see
/// `src/port/accept_inherit_tests.rs`. A separate file so the wire vocabulary it spells does not
/// grow this one.
#[cfg(test)]
#[path = "accept_inherit_tests.rs"]
mod accept_inherit_tests;
