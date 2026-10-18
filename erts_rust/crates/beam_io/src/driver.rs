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

//! The driver seam: [`Driver`], its context, flags and the name→factory [`DriverRegistry`].
//!
//! A **driver** is the Rust side of the OS/driver boundary (workspace `../CONTEXT.md` §1
//! **A.4**): it owns whatever OS resources its port needs (fds, sockets, …) and reacts to
//! three kinds of events — an output request, input readiness and output readiness —
//! through the [`Driver`] trait. It never touches the poller or the two port byte queues
//! directly; the *only* handle it is given is a [`DriverContext`], which mediates every
//! registration and queue access. That keeps the port the single owner of the queues and
//! the poller (Prevent-Duplication), mirroring the C `ioq` being the sole source of truth
//! (`erts/emulator/beam/io.c`, `erl_io_queue.{c,h}`).
//!
//! This crate models a driver as a **Rust trait** for Milestone-1; the frozen C-ABI bridge
//! (`beam_abi::driver`'s `ErlDrvEntry`/`ErlDrvApi`) is documented future work — a NIF/driver
//! `.so` cannot implement a Rust trait, so a later adapter implements [`Driver`] over an
//! `ErlDrvEntry` function table (see the crate `CONTEXT.md`, Design Decisions).

use std::collections::{HashMap, VecDeque};
use std::os::unix::io::RawFd;
use std::time::Instant;

use beam_common::Result;
use beam_process::PortId;

use crate::error;
use crate::poller::{Interest, Poller, Token};
use crate::port_task::ReadMessage;
use crate::queue::ByteQueue;
/// Why a port is exiting, as reported to a driver through [`Driver::port_exit`].
///
/// This is a small, closed vocabulary — deliberately *not* an Erlang term — because it is the
/// driver-facing reason for a port's exit, distinct from whatever an observer of the port sees.
/// The runtime picks the variant that matches its exit path; a driver that does not care
/// inherits the no-op default of [`Driver::port_exit`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PortExitReason {
    /// The port was closed on request (the reserved `close` control op or
    /// [`crate::PortRegistry::close`]).
    Closed,
    /// The port's owning process exited, taking the port down with it.
    OwnerExited,
    /// The port terminated normally — the driver itself decided the port is done (its stream
    /// ended, its peer closed, …).
    Normal,
}

/// A bitset of driver capability flags.
///
/// The flags a driver reports to the runtime in [`Driver::flags`]; only
/// [`DriverFlags::SOFT_BUSY`] is defined for Milestone-1.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DriverFlags(u32);

impl DriverFlags {
    /// The driver can be busy while still accepting output without blocking the emulator.
    pub const SOFT_BUSY: DriverFlags = DriverFlags(1 << 0);

    /// The empty flag set.
    pub const fn empty() -> DriverFlags {
        DriverFlags(0)
    }

    /// The union of two flag sets (every flag in `self` or in `other`).
    ///
    /// The C-ABI bridge ([`crate::abi::DriverPort`]) uses this to map an `ErlDrvEntry`'s
    /// `driver_flags` bitfield onto this crate's flag vocabulary, which has no single
    /// `from_bits` constructor (the field is private on purpose — flags are only ever
    /// combined from the named constants).
    pub const fn union(self, other: DriverFlags) -> DriverFlags {
        DriverFlags(self.0 | other.0)
    }

    /// Whether this set contains every flag in `other`.
    pub fn contains(self, other: DriverFlags) -> bool {
        self.0 & other.0 == other.0
    }
}

/// The readiness directions a driver has registered with the poller, as seen through its
/// [`DriverContext`].
///
/// The poller keys **every** registration by the port number, so a `poller::Event` tells the
/// reactor *which port* fired but not which of the port's descriptors (its read fd or its
/// write fd) it was. The legacy C has no such ambiguity — each `ErlDrvEvent` carries its own
/// task handle — so this type restores the missing information in Rust: the [`crate::Port`]
/// owns one [`PortArms`] and the reactor consults it before scheduling a direction.
///
/// That is why a full-close event (`EPOLLHUP`, which `mio` reports as **both** `read_closed`
/// and `write_closed`) does not schedule an `OUTPUT` task for a read-only port that never
/// armed a write fd.
#[derive(Clone, Debug, Default)]
pub(crate) struct PortArms {
    /// The live registrations, as `(token, interest)` pairs (a couple per port at most).
    entries: Vec<(Token, Interest)>,
}

impl PortArms {
    /// Records a new registration, or updates the interest of an existing one.
    pub(crate) fn record(&mut self, token: Token, interest: Interest) {
        match self.entries.iter_mut().find(|(armed, _)| *armed == token) {
            Some(entry) => entry.1 = interest,
            None => self.entries.push((token, interest)),
        }
    }

    /// Forgets a registration that was removed.
    pub(crate) fn forget(&mut self, token: Token) {
        self.entries.retain(|(armed, _)| *armed != token);
    }

    /// Whether the driver has an armed registration that wants readability.
    pub(crate) fn read_armed(&self) -> bool {
        self.entries
            .iter()
            .any(|(_, interest)| interest.is_readable())
    }

    /// Whether the driver has an armed registration that wants writability.
    pub(crate) fn write_armed(&self) -> bool {
        self.entries
            .iter()
            .any(|(_, interest)| interest.is_writable())
    }
}

/// The driver's view of its port.
///
/// This is the **only** way a driver reaches the poller or the port's byte queues. Poll
/// registration keys are assigned by the context (they are the port number
/// [`PortId::number`]), and the [`DriverContext::input`]/[`DriverContext::output`] queues
/// are the port's own buffers, not copies.
pub struct DriverContext<'a> {
    /// The port this context belongs to.
    port: PortId,
    /// The shared reactor, used for every registration.
    poller: &'a mut Poller,
    /// The port's input queue (bytes the driver read, handed up to Erlang).
    input: &'a mut ByteQueue,
    /// The port's output queue (bytes waiting to be written by the driver).
    output: &'a mut ByteQueue,
    /// The registrations this driver holds, so the reactor knows which directions it can
    /// serve (see [`PortArms`]).
    arms: &'a mut PortArms,
    /// The port's sink for **completed reads** (see [`DriverContext::deliver_read`]), or
    /// `None` in a bare test context — a driver that asks for the split shape without a sink
    /// gets the typed `unsupported` instead of a silently flattened read.
    reads: Option<&'a mut VecDeque<ReadMessage>>,
}

impl<'a> DriverContext<'a> {
    /// Builds a context for `port` over the given disjoint poller/queue/arming borrows.
    ///
    /// Called by [`crate::port::Port`] after it has destructured itself into disjoint
    /// fields, so the driver and its context borrow separate members of the `Port`. A context
    /// built here has **no** read sink, so [`DriverContext::deliver_read`] answers the typed
    /// `unsupported`; [`crate::port::Port`] installs one with
    /// [`DriverContext::with_reads`] at every driver callback.
    pub(crate) fn new(
        port: PortId,
        poller: &'a mut Poller,
        input: &'a mut ByteQueue,
        output: &'a mut ByteQueue,
        arms: &'a mut PortArms,
    ) -> DriverContext<'a> {
        DriverContext {
            port,
            poller,
            input,
            output,
            arms,
            reads: None,
        }
    }

    /// Installs `reads` as this context's completed-read sink (chainable).
    ///
    /// Every driver callback a [`crate::port::Port`] dispatches gets one, so a driver may
    /// call [`DriverContext::deliver_read`] from any of them.
    pub(crate) fn with_reads(
        mut self,
        reads: &'a mut VecDeque<ReadMessage>,
    ) -> DriverContext<'a> {
        self.reads = Some(reads);
        self
    }
    /// The port this context belongs to.
    pub fn port(&self) -> PortId {
        self.port
    }

    /// Registers `fd` with the poller under this port's key.
    ///
    /// Returns the [`Token`] to pass to [`DriverContext::reregister`]/
    /// [`DriverContext::deregister`].
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the operating system rejects the registration.
    pub fn register(&mut self, fd: RawFd, interest: Interest) -> Result<Token> {
        let token = self.poller.register(fd, interest, self.port.number())?;
        self.arms.record(token, interest);
        Ok(token)
    }

    /// Changes the interest mask of a registration made by this context.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the token is unknown or the re-registration fails.
    pub fn reregister(&mut self, fd: RawFd, token: Token, interest: Interest) -> Result<()> {
        self.poller.reregister(fd, token, interest)?;
        self.arms.record(token, interest);
        Ok(())
    }

    /// Removes a registration made by this context.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the token is unknown or the de-registration fails.
    pub fn deregister(&mut self, fd: RawFd, token: Token) -> Result<()> {
        self.poller.deregister(fd, token)?;
        self.arms.forget(token);
        Ok(())
    }

    /// The port's input queue, which the driver appends bytes it read to.
    ///
    /// Bytes pushed here are **one body-only read** at the next frame close — a
    /// [`ReadMessage`] with no header and the bytes as its body
    /// ([`ReadMessage::body_only`]) — reported to the owner after every byte the driver
    /// banked before it. It is the plain `driver_output(bytes, len)` shape; a driver that has
    /// a header apart from its body (the C's `driver_output2`) must use
    /// [`DriverContext::deliver_read`] instead, because a flattened push cannot express the
    /// split.
    pub fn input(&mut self) -> &mut ByteQueue {
        self.input
    }

    /// Hands one completed read up to the port owner, header apart from body.
    ///
    /// This is the C's `driver_output2(ix, hbuf, hlen, buf, len)` —
    /// `driver_output_binary`/`driver_outputv` have the same shape — as seen from a driver:
    /// **one** read message per call, [`header`](ReadMessage::header) the `hbuf`/`hlen` bytes
    /// and [`body`](ReadMessage::body) the `buf`/`len` bytes, with `body == None` meaning the
    /// C's `buf == NULL` (no binary tail at all, rather than an empty one). See
    /// [`ReadMessage`] for the pinned oracle shapes and how a runtime renders them.
    ///
    /// Ordering: bytes the driver banked on [`DriverContext::input`] are closed into their
    /// own body-only read **first**, so a `push` followed by a `deliver_read` inside one
    /// callback delivers the two messages in that order (the C delivers synchronously, so the
    /// order is observable). Two calls are never merged, and an empty read
    /// (`deliver_read(&[], None)`) is still one message — the oracle's `{Port, {data, []}}`.
    ///
    /// # Errors
    ///
    /// Returns the typed `unsupported` diagnostic when this context has **no** read sink: a
    /// bare context (a unit test, or a driver driven outside a port) cannot report the split,
    /// and flattening it into [`DriverContext::input`] would report the wrong shape rather
    /// than a degraded one.
    pub fn deliver_read(&mut self, header: &[u8], body: Option<&[u8]>) -> Result<()> {
        let Some(reads) = self.reads.as_mut() else {
            return Err(error::unsupported(
                "a driver handed up a split read (driver_output2) outside a port: no read sink is installed",
            ));
        };
        // Close whatever the driver banked before this call, so arrival order is preserved.
        // The close itself lives in `Port` (one implementation, see `close_input_frame`).
        crate::port::close_input_frame(self.input, reads);
        reads.push_back(ReadMessage {
            header: header.to_vec(),
            body: body.map(<[u8]>::to_vec),
        });
        Ok(())
    }    /// The port's output queue, which the driver drains bytes waiting to be written from.
    pub fn output(&mut self) -> &mut ByteQueue {
        self.output
    }
}

/// A port driver.
///
/// Implemented by the built-in drivers ([`crate::drivers::FdDriver`],
/// [`crate::drivers::EfileDriver`]) and, later, by the frozen C-ABI bridge. Every method
/// receives a [`DriverContext`] and nothing else.
pub trait Driver {
    /// The driver's registered name (the key it is looked up under in the
    /// [`DriverRegistry`]).
    fn name(&self) -> &str;

    /// The driver's capability flags (empty by default).
    fn flags(&self) -> DriverFlags {
        DriverFlags::empty()
    }

    /// Initialises the driver for a freshly opened port.
    ///
    /// This is where a driver registers its fds with the poller.
    ///
    /// # Errors
    ///
    /// Returns a `beam_common::BeamError` when setup fails.
    fn start(&mut self, ctx: &mut DriverContext<'_>) -> Result<()>;

    /// Drains bytes from [`DriverContext::output`] to the device and returns how many were
    /// written.
    ///
    /// Bytes that cannot be written yet (e.g. `EAGAIN`) are left queued and the driver
    /// arms write readiness by re-registering with [`Interest::WRITABLE`]. A write that leaves
    /// the queue non-empty without arming write readiness does not strand the remainder: the
    /// port schedules an [`crate::PortTaskKind::Output`] task for it (see
    /// [`Port::output`](crate::Port::output)).
    ///
    /// The returned count is a **contract**: the driver must have called
    /// [`ctx.output().consume(n)`](crate::queue::ByteQueue::consume) for exactly the bytes it
    /// counts, because the queue is the only record of what still has to be written. The port
    /// verifies it on every write path and reports a typed error when it does not hold
    /// ([`Port::check_write_accounting`](crate::Port::output)), rather than trusting a count
    /// that would make the next drive write the same bytes again.
    ///
    /// # Errors
    ///
    /// Returns a `beam_common::BeamError` when the write fails irrecoverably.
    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize>;

    /// Handles read readiness: read from the device into [`DriverContext::input`].
    ///
    /// # Errors
    ///
    /// Returns a `beam_common::BeamError` when the read fails irrecoverably.
    fn ready_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()>;

    /// Handles write readiness; the default drains the output queue via [`Driver::output`].
    ///
    /// The returned count carries the same consume-what-you-report contract as
    /// [`Driver::output`], and is verified the same way.
    ///
    /// # Errors
    ///
    /// Returns a `beam_common::BeamError` when the write fails irrecoverably.
    fn ready_output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        self.output(ctx)
    }

    /// Handles a port-control command; the default accepts it and returns no reply bytes.
    ///
    /// # Errors
    ///
    /// Returns a `beam_common::BeamError` when the command is rejected.
    fn control(
        &mut self,
        ctx: &mut DriverContext<'_>,
        command: &str,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        let _ = (ctx, command, arg);
        Ok(Vec::new())
    }

    /// Whether the driver's C counterpart has a non-`NULL` `control` entry.
    ///
    /// This is the **control-capability answer** of the frozen C ABI: `erlang:port_control/3`
    /// first asks whether the driver has a control entry at all and, when it has none, answers
    /// `badarg` **without calling anything** — `call_driver_control` opens with
    /// `if (!prt->drv_ptr->control) return ERTS_PORT_OP_BADARG;`
    /// (`erts/emulator/beam/io.c:4034-4036`). The knowledge belongs to the driver that has it, so
    /// it is a property here rather than a name list at the runtime.
    ///
    /// `true` (the default) means [`Driver::control`] is the whole story. A `false` answer means
    /// the runtime must answer the oracle's `badarg`/refusal for **every**
    /// `erlang:port_control/3` call on this port and never consult [`Driver::control`] — the
    /// driver's `control` the trait still requires is then unreachable through that BIF (the port
    /// layer's own [`crate::PortRegistry::port_control`] stays driver-agnostic; this is the
    /// runtime's gate).
    ///
    /// The built-in drivers whose C entry points do have a `control` slot (the `inet` pair) answer
    /// `true`; [`crate::drivers::RamFileDriver`] answers `false`, because the C's
    /// `ram_file_driver_entry` initialises `control` to `NULL`
    /// (`erts/emulator/drivers/common/ram_file_drv.c:116`) and the oracle therefore raises
    /// `badarg` for `port_control` on a `ram_file_drv` port.
    fn has_control(&self) -> bool {
        true
    }

    /// Declares the deadline at which this driver next wants [`Driver::timeout`] to run.
    ///
    /// This is the driver-facing **timer-arming seam**: a driver that needs a port timer (an
    /// idle timeout, a retransmit timer, a poll deadline, …) returns the instant it wants to
    /// be woken at, and the port mirrors it into its single `TIMEOUT`
    /// [port task](crate::PortTaskKind). The default (`None`) declares no deadline, so the
    /// built-in `fd`/`efile`/`spawn` drivers are unaffected.
    ///
    /// # Contract
    ///
    /// * The port calls this **after every driver interaction** — [`Driver::start`],
    ///   [`Driver::output`], [`Driver::ready_input`], [`Driver::ready_output`],
    ///   [`Driver::control`] and [`Driver::timeout`] — so the declaration is re-read whenever
    ///   the driver's needs may have changed.
    /// * `Some(deadline)` arms (or **re-points**) the port's `TIMEOUT` task at exactly
    ///   `deadline`; `None` **clears** any pending timer. The declaration is therefore
    ///   authoritative: return `Some` for as long as a timer is wanted, and `None` to stop it.
    /// * The deadline is a **lower bound**: the port task queue never hands a `TIMEOUT` task
    ///   out before its deadline has passed, so [`Driver::timeout`] never runs early. It may
    ///   run later (the runtime drives the task round).
    /// * A driver that wants to re-arm from within [`Driver::timeout`] simply returns the next
    ///   deadline the next time this is called (which the port does right after that call).
    fn next_timeout(&self) -> Option<Instant> {
        None
    }

    /// Handles the expiry of a port timer; the default does nothing.
    ///
    /// # Errors
    ///
    /// Returns a `beam_common::BeamError` when the timeout handling fails.
    fn timeout(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        let _ = ctx;
        Ok(())
    }

    /// Drives the driver's **asynchronous completions**, returning how many were processed; the
    /// default drains nothing.
    ///
    /// `driver_async` schedules a job on a worker thread (`erl_async.c`); when it finishes, the
    /// runtime hands its result to the driver's `ready_async` callback (or runs its `async_free`).
    /// The C does that from `call_async_ready` on the port's scheduler, but this crate has **no
    /// port-task kind for async work** in Milestone-1 (a `PortTaskKind::Async` is a documented
    /// follow-up), so the runtime calls this at a port-task-round **drive point** instead — one
    /// call per round is enough for pending jobs to make progress.
    ///
    /// A driver with no async support inherits `Ok(0)`. The C-ABI bridge
    /// ([`crate::abi::DriverPort`]) overrides it with
    /// [`DriverPort::poll_async`](crate::abi::DriverPort::poll_async), which runs each finished
    /// job's `ready_async` (or its `async_free`).
    ///
    /// # Errors
    ///
    /// Returns a `beam_common::BeamError` when driving the completions fails; the C's
    /// `ready_async`/`async_free` are `void` callbacks, so the bridge always answers `Ok`.
    fn drive_async(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let _ = ctx;
        Ok(0)
    }

    /// Whether the driver has asynchronous work that a [`Driver::drive_async`] call would process.
    ///
    /// This is the **cheap probe** the port-task round uses to decide whether a port needs an async
    /// drive point at all: the runtime calls [`Driver::drive_async`] for every port whose driver
    /// answers `true`, and skips the rest. Returning `false` must therefore never hide pending
    /// work — a driver with async jobs must report `true` until its last completion has been
    /// handed over.
    ///
    /// The default (`false`) is correct for a driver that never schedules async work, so the
    /// built-in drivers are unaffected. The C-ABI bridge
    /// ([`crate::abi::DriverPort`]) overrides it with
    /// [`AbiHost::has_pending_async`](crate::AbiHost::has_pending_async), which reports whether
    /// the port's `driver_async` job set is non-empty.
    fn has_pending_async(&self) -> bool {
        false
    }

    /// Takes the reaped exit status of the OS child this port owns, **once**.
    ///
    /// A driver that starts no child — the built-in [`crate::drivers::FdDriver`] and
    /// [`crate::drivers::EfileDriver`] — never has one to report and inherits the default
    /// (`Ok(None)`), so this capability is purely additive. `Ok(None)` also means "the child
    /// has not exited yet" or "the status was already reported"; only
    /// [`crate::drivers::SpawnDriver`] overrides it, for a port opened with the
    /// `exit_status` option (surfaced as [`crate::PortRegistry::take_exit_status`]).
    ///
    /// # Errors
    ///
    /// Returns a `beam_common::BeamError` when the underlying wait fails.
    fn take_exit_status(&mut self) -> Result<Option<i32>> {
        Ok(None)
    }

    /// Takes the port's **end-of-file** notification, **once**, when the port asked for it.
    ///
    /// A port opened with the `eof` option is told its input stream ended: the oracle delivers a
    /// bare `eof` message (not a `{Port, {data, …}}`) once the child's stdout reaches end of
    /// file. `Ok(true)` means "the stream reached end-of-file and this is the first time it is
    /// reported"; `Ok(false)` means "nothing to report yet" — because the option was not asked
    /// for, because end-of-file has not been observed, or because it was already reported.
    ///
    /// A driver that never streams input — the built-in [`crate::drivers::FdDriver`] and
    /// [`crate::drivers::EfileDriver`] — inherits the default (`Ok(false)`), so this capability
    /// is purely additive; it is the input-side counterpart of
    /// [`Driver::take_exit_status`] (surfaced as [`crate::PortRegistry::take_eof`]).
    ///
    /// # Errors
    ///
    /// Returns a `beam_common::BeamError` when the check fails.
    fn take_eof(&mut self) -> Result<bool> {
        Ok(false)
    }

    /// Tells the driver its port is exiting, **before** [`Driver::stop`] tears it down.
    ///
    /// This is the driver-facing **port-exit notification channel**: on its port-exit path
    /// (owner exit, an explicit close, a normal stream end) the runtime calls
    /// [`crate::PortRegistry::notify_port_exit`], which forwards `reason` here. It is
    /// deliberately distinct from [`Driver::stop`]: `port_exit` is an *advisory notification*
    /// the driver may use to interrupt in-flight work, flush a final message or unblock
    /// waiters, whereas `stop` is the mandatory resource teardown. A driver that does not need
    /// it inherits the no-op default (so the built-in drivers are unaffected).
    ///
    /// # Contract
    ///
    /// * Delivery is **at most once** per port (see
    ///   [`crate::PortRegistry::notify_port_exit`]); the port is still live and its
    ///   registrations are still armed when this runs.
    /// * `port_exit` and `stop` are **independent and both well-defined**: a `port_exit`
    ///   followed by a later `stop` (or a `stop` with no preceding `port_exit`) is legal, and
    ///   neither implies the other. A driver must therefore tolerate `stop` running whether or
    ///   not it saw a `port_exit`.
    /// * This must not be used to release OS resources; that is `stop`'s job.
    ///
    /// # Errors
    ///
    /// Returns a `beam_common::BeamError` when the driver's exit handling fails; the failure
    /// is reported to the caller and does **not** prevent a subsequent `stop`.
    fn port_exit(&mut self, ctx: &mut DriverContext<'_>, reason: PortExitReason) -> Result<()> {
        let _ = (ctx, reason);
        Ok(())
    }

    /// Shuts the driver down, releasing its OS resources.
    ///
    /// # Errors
    ///
    /// Returns a `beam_common::BeamError` when teardown fails.
    fn stop(&mut self, ctx: &mut DriverContext<'_>) -> Result<()>;
}

/// A driver constructor: the function a [`DriverRegistry`] stores under a name.
pub type DriverFactory = fn() -> Result<Box<dyn Driver>>;

/// The [`DriverFactory`] of the built-in `"fd"` driver.
///
/// A plain `fn` (not a closure) because [`DriverFactory`] is a function pointer: the
/// factory captures nothing and builds a fresh [`crate::drivers::FdDriver::pipe`] each call.
fn make_fd() -> Result<Box<dyn Driver>> {
    Ok(Box::new(crate::drivers::FdDriver::pipe()?))
}

/// The emulator's **statically-linked driver set** — the names the C `driver_tab.c` table holds.
///
/// These three are the drivers linked *into* the emulator itself, so they are exactly the names an
/// `open_port({spawn_driver, Name}, …)` can resolve (workspace `../CONTEXT.md` §1 **A.4**). They
/// are **not** `.so`-loadable: `erts-*/lib/` ships zero `.so` files, so no dynamic load path can
/// reach them and none may be modelled as a loadable driver. The system oracle's
/// `erl_ddll:loaded_drivers/0` answers `{ok,["tcp_inet","udp_inet","ram_file_drv"]}` — exactly this
/// set (the oracle's `driver_tab.c` order; this list is sorted ascending instead, so compare as a
/// set, not positionally).
///
/// [`crate::drivers::FdDriver`] (`"fd"`) and [`crate::drivers::EfileDriver`] (`"efile"`) are this
/// crate's own internal drivers — they exist so this crate's ports and tests can build a byte
/// stream / file port, but they are **not** emulator drivers at all: the oracle refuses
/// `open_port({spawn_driver,"fd"}, …)` and `{spawn_driver,"efile"}` with `badarg`. Hence they are
/// deliberately absent here even though [`DriverRegistry::with_builtins`] registers them.
///
/// A runtime resolving `{spawn_driver, Name}` must gate on [`is_static_spawn_driver`] /
/// [`DriverRegistry::create_static_spawn`] — *not* on [`DriverRegistry::with_builtins`] — so those
/// two names produce the oracle's `badarg` instead of a working port.
pub const STATIC_SPAWN_DRIVERS: [&str; 3] = ["ram_file_drv", "tcp_inet", "udp_inet"];

/// Whether `name` is one of the emulator's statically-linked drivers ([`STATIC_SPAWN_DRIVERS`]).
///
/// This is the membership test an `open_port({spawn_driver, Name}, …)` handler must apply before
/// asking a [`DriverRegistry`] for the driver: an emulator driver name is addressable by that
/// syntax, any other name is the oracle's `badarg` — including this crate's own internal `"fd"` and
/// `"efile"`, which [`DriverRegistry::with_builtins`] does register.
#[must_use]
pub fn is_static_spawn_driver(name: &str) -> bool {
    STATIC_SPAWN_DRIVERS.contains(&name)
}

/// The name→factory table: the Rust analogue of the C `driver_tab.c` (`lib/driver_tab.c`).
pub struct DriverRegistry {
    /// Registered driver factories, keyed by driver name.
    factories: HashMap<String, DriverFactory>,
}

impl DriverRegistry {
    /// An empty registry (no drivers).
    pub fn new() -> DriverRegistry {
        DriverRegistry {
            factories: HashMap::new(),
        }
    }

    /// A registry pre-populated with this crate's built-in drivers.
    ///
    /// Milestone-1 registers five built-ins: `"fd"` ([`crate::drivers::FdDriver`], a byte-stream
    /// driver over a pipe/fd), `"efile"` ([`crate::drivers::EfileDriver`], the file driver —
    /// an `FdDriver` plus the `efile` control family), `"tcp_inet"`
    /// ([`crate::drivers::TcpInetDriver`], the TCP socket driver — the name the oracle reports
    /// from `port_info(Port, name)` for a TCP port and the name `prim_inet` opens), `"udp_inet"`
    /// ([`crate::drivers::UdpInetDriver`], the UDP datagram socket driver — the name the oracle
    /// reports for a `gen_udp` socket), and `"ram_file_drv"`
    /// ([`crate::drivers::RamFileDriver`], the in-memory file driver `ram_file.erl` talks to).
    ///
    /// Only three of the five are emulator drivers addressable through
    /// `open_port({spawn_driver, Name}, …)`: `"tcp_inet"`, `"udp_inet"` and `"ram_file_drv"`
    /// ([`STATIC_SPAWN_DRIVERS`]). `"fd"`/`"efile"` are this crate's internal drivers and the oracle
    /// refuses them with `badarg`, so a `{spawn_driver, Name}` resolution must go through
    /// [`DriverRegistry::create_static_spawn`] (see [`is_static_spawn_driver`]) rather than
    /// [`DriverRegistry::create`] on this registry.
    pub fn with_builtins() -> DriverRegistry {
        let mut registry = DriverRegistry::new();
        registry.register("fd", make_fd);
        registry.register("efile", crate::drivers::make_efile);
        registry.register("tcp_inet", crate::drivers::make_tcp_inet);
        registry.register("udp_inet", crate::drivers::make_udp_inet);
        registry.register("ram_file_drv", crate::drivers::make_ram_file);
        registry
    }

    /// A registry holding **only** the emulator's statically-linked drivers.
    ///
    /// Exactly [`STATIC_SPAWN_DRIVERS`] — `"ram_file_drv"`, `"tcp_inet"` and `"udp_inet"`, built from
    /// the same built-in factories [`DriverRegistry::with_builtins`] uses. The internal
    /// `"fd"`/`"efile"` drivers are absent, so [`DriverRegistry::names`] is exactly the oracle's
    /// `erl_ddll:loaded_drivers/0` set and [`DriverRegistry::create_static_spawn`] agrees with
    /// [`is_static_spawn_driver`] on every name.
    ///
    /// This is the registry a runtime should hold if it wants a `{spawn_driver, Name}` resolution
    /// that is oracle-exact by construction; a runtime that also needs `"fd"`/`"efile"` for its own
    /// ports keeps [`DriverRegistry::with_builtins`] and gates on [`is_static_spawn_driver`].
    pub fn with_static_spawn_drivers() -> DriverRegistry {
        let mut registry = DriverRegistry::new();
        registry.register("ram_file_drv", crate::drivers::make_ram_file);
        registry.register("tcp_inet", crate::drivers::make_tcp_inet);
        registry.register("udp_inet", crate::drivers::make_udp_inet);
        registry
    }

    /// Instantiates the emulator driver `name` for an `open_port({spawn_driver, Name}, …)` request.
    ///
    /// **This is the resolution path a runtime's `{spawn_driver, Name}` handling should call.** It
    /// applies two gates in order:
    ///
    /// 1. the **static gate** — [`is_static_spawn_driver`] — which is what makes `"fd"`/`"efile"`
    ///    (and any invented name) the oracle's `badarg` even on a registry that *does* register
    ///    them, i.e. a membership refusal, not merely a missing entry;
    /// 2. the **factory lookup** through [`DriverRegistry::create`], so the driver is instantiated
    ///    by the same factory as everywhere else.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `name` is not in [`STATIC_SPAWN_DRIVERS`] — exactly the
    /// `badarg` the oracle raises for `{spawn_driver,"fd"}`/`{spawn_driver,"efile"}` — **and** when
    /// the static name happens to be absent from *this* registry (e.g. a plain
    /// [`DriverRegistry::new`]: a static name is addressable, but nothing here can build it), or
    /// whatever the factory itself returns.
    pub fn create_static_spawn(&self, name: &str) -> Result<Box<dyn Driver>> {
        if !is_static_spawn_driver(name) {
            return Err(crate::error::badarg());
        }
        self.create(name)
    }

    /// Registers `factory` under `name`, reporting whether the name was free.
    ///
    /// A duplicate name is rejected (`false`) and leaves the existing factory in place.
    pub fn register(&mut self, name: &str, factory: DriverFactory) -> bool {
        if self.factories.contains_key(name) {
            return false;
        }
        self.factories.insert(name.to_string(), factory);
        true
    }

    /// Instantiates the driver registered under `name`.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when no driver is registered under `name`, or whatever
    /// the factory itself returns.
    pub fn create(&self, name: &str) -> Result<Box<dyn Driver>> {
        match self.factories.get(name) {
            Some(factory) => (*factory)(),
            None => Err(crate::error::badarg()),
        }
    }

    /// Whether a driver is registered under `name`.
    pub fn contains(&self, name: &str) -> bool {
        self.factories.contains_key(name)
    }

    /// The registered driver names, sorted so the order is deterministic.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.factories.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::{
        is_static_spawn_driver, Driver, DriverContext, DriverFlags, DriverRegistry, PortArms,
        PortExitReason, STATIC_SPAWN_DRIVERS,
    };
    use crate::poller::Poller;
    use crate::queue::ByteQueue;
    use beam_common::{BeamError, Result};
    use beam_process::PortId;
    use std::collections::VecDeque;
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::Duration;

    /// The bounded wait for the `erl` oracle, so a wedged runtime cannot hang the suite.
    const ERL_TIMEOUT: Duration = Duration::from_secs(30);

    #[test]
    fn driver_flags_are_a_bitset() {
        assert!(DriverFlags::empty().contains(DriverFlags::empty()));
        assert!(!DriverFlags::empty().contains(DriverFlags::SOFT_BUSY));
        assert!(DriverFlags::SOFT_BUSY.contains(DriverFlags::SOFT_BUSY));
        assert!(DriverFlags::SOFT_BUSY.contains(DriverFlags::empty()));
        assert!(DriverFlags::SOFT_BUSY != DriverFlags::empty());
    }

    #[test]
    fn an_empty_registry_knows_no_drivers() {
        let registry = DriverRegistry::new();
        assert!(!registry.contains("fd"));
        assert!(registry.names().is_empty());
        assert!(matches!(registry.create("fd"), Err(BeamError::BadArg)));
    }

    #[test]
    fn with_builtins_registers_the_built_in_drivers() {
        let registry = DriverRegistry::with_builtins();
        assert!(registry.contains("fd"));
        assert!(registry.contains("efile"));
        assert!(registry.contains("tcp_inet"));
        assert!(registry.contains("udp_inet"));
        assert!(registry.contains("ram_file_drv"));
        assert_eq!(
            registry.names(),
            vec!["efile", "fd", "ram_file_drv", "tcp_inet", "udp_inet"]
        );

        let driver = registry.create("fd").expect("the fd driver exists");
        assert_eq!(driver.name(), "fd");
        assert!(driver.flags() == DriverFlags::empty());
        let efile = registry.create("efile").expect("the efile driver exists");
        assert_eq!(efile.name(), "efile");
        let inet = registry
            .create("tcp_inet")
            .expect("the tcp_inet driver exists");
        assert_eq!(inet.name(), "tcp_inet");
        let udp = registry
            .create("udp_inet")
            .expect("the udp_inet driver exists");
        assert_eq!(udp.name(), "udp_inet");
        let ram_file = registry
            .create("ram_file_drv")
            .expect("the ram_file_drv driver exists");
        assert_eq!(ram_file.name(), "ram_file_drv");
        assert!(matches!(registry.create("nope"), Err(BeamError::BadArg)));
    }

    /// The `has_control` capability answer: `true` by default, `false` for the built-in whose C
    /// entry point leaves `control` `NULL` (`ram_file_drv`).
    #[test]
    fn has_control_defaults_to_true_and_ram_file_drv_answers_false() {
        // The additive trait default: a driver that overrides nothing inherits `true`.
        assert!(MinimalDriver.has_control(), "the default is `true`");
        // The one built-in without a C `control` entry (`ram_file_drv.c:116`).
        assert!(
            !super::super::drivers::RamFileDriver::new().has_control(),
            "ram_file_drv's C entry has `control == NULL`"
        );
        // Every other built-in keeps the default, including the `inet` pair.
        let registry = DriverRegistry::with_builtins();
        for name in registry.names() {
            let driver = registry.create(name).expect("the built-in constructs");
            assert_eq!(
                driver.has_control(),
                name != "ram_file_drv",
                "unexpected has_control for {name}"
            );
        }
        // `spawn` (constructed, not registered) has a `control` entry too.
        let spawn = crate::drivers::SpawnDriver::new(
            &crate::drivers::SpawnSpec::Spawn {
                command: "true".to_string(),
                argv: None,
            },
            &crate::drivers::SpawnOptions::default(),
        )
        .expect("the child starts");
        assert!(spawn.has_control());
    }

    #[test]
    fn register_rejects_a_duplicate_name() {
        let mut registry = DriverRegistry::new();
        assert!(registry.register("fd", super::make_fd));
        assert!(!registry.register("fd", super::make_fd));
        assert_eq!(registry.names(), vec!["fd"]);
        assert!(registry.register("other", super::make_fd));
        assert_eq!(registry.names(), vec!["fd", "other"]);
    }

    /// A driver overriding only the required methods, to pin the additive defaults of
    /// [`Driver::next_timeout`] and [`Driver::port_exit`].
    struct MinimalDriver;

    impl Driver for MinimalDriver {
        fn name(&self) -> &str {
            "minimal"
        }

        fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            Ok(())
        }

        fn output(&mut self, _ctx: &mut DriverContext<'_>) -> Result<usize> {
            Ok(0)
        }

        fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            Ok(())
        }

        fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_timer_and_port_exit_seams_default_to_no_ops() {
        let mut poller = Poller::new().expect("a poll set");
        let mut input = ByteQueue::new();
        let mut output = ByteQueue::new();
        let mut arms = PortArms::default();
        let mut driver = MinimalDriver;

        // A driver that declares nothing wants no timer.
        assert!(driver.next_timeout().is_none());

        // The default `port_exit` accepts (and ignores) every reason.
        let mut ctx = DriverContext::new(
            PortId::new(0),
            &mut poller,
            &mut input,
            &mut output,
            &mut arms,
        );
        for reason in [
            PortExitReason::Closed,
            PortExitReason::OwnerExited,
            PortExitReason::Normal,
        ] {
            assert!(driver.port_exit(&mut ctx, reason).is_ok());
        }
    }

    // -------------------------------------------------------------------------------------------
    // The completed-read sink (`driver_output2`'s header/body split)
    // -------------------------------------------------------------------------------------------

    #[test]
    fn deliver_read_without_a_sink_is_the_typed_unsupported_never_a_flattening() {
        // A bare context (a unit test, or a driver driven outside a port) cannot carry the
        // split, and reporting a header/body pair as one flat `input()` blob would be the
        // *wrong shape* rather than a degraded one — so it is a typed error, not a fallback.
        let mut poller = Poller::new().expect("a poll set");
        let mut input = ByteQueue::new();
        let mut output = ByteQueue::new();
        let mut arms = PortArms::default();
        let mut ctx = DriverContext::new(
            PortId::new(0),
            &mut poller,
            &mut input,
            &mut output,
            &mut arms,
        );
        let error = ctx
            .deliver_read(b"HDR", Some(b"abc"))
            .expect_err("no sink is installed");
        assert!(
            error.to_string().contains("no read sink"),
            "the diagnostic names the missing sink: {error}"
        );
        assert!(
            ctx.input().is_empty(),
            "the refused read is never silently flattened into the input queue"
        );
    }

    #[test]
    fn deliver_read_pushes_one_message_and_closes_the_banked_frame_first() {
        let mut poller = Poller::new().expect("a poll set");
        let mut input = ByteQueue::new();
        let mut output = ByteQueue::new();
        let mut arms = PortArms::default();
        let mut reads = VecDeque::new();
        let mut ctx = DriverContext::new(
            PortId::new(0),
            &mut poller,
            &mut input,
            &mut output,
            &mut arms,
        )
        .with_reads(&mut reads);

        // Banked bytes first, then the split read: the bank is closed into its own body-only
        // message ahead of it (the C delivers synchronously, so the order is observable).
        ctx.input().push(b"banked");
        ctx.deliver_read(b"HDR", Some(b"abc"))
            .expect("the sink is installed");
        // An explicit empty read is still one message, and a `None` body differs from `Some([])`
        // — which is why the caller passes an `Option`.
        ctx.deliver_read(b"", None).expect("one empty message");

        let pushed: Vec<(Vec<u8>, Option<Vec<u8>>)> =
            reads.drain(..).map(|read| (read.header, read.body)).collect();
        assert_eq!(
            pushed,
            vec![
                (Vec::new(), Some(b"banked".to_vec())),
                (b"HDR".to_vec(), Some(b"abc".to_vec())),
                (Vec::new(), None),
            ]
        );
        assert!(input.is_empty(), "the banked frame was drained, not copied");
    }

    #[test]
    fn port_exit_reason_is_a_closed_copyable_vocabulary() {
        let all = [
            PortExitReason::Closed,
            PortExitReason::OwnerExited,
            PortExitReason::Normal,
        ];
        for (i, a) in all.iter().enumerate() {            for (j, b) in all.iter().enumerate() {
                assert_eq!(a == b, i == j);
            }
            // `Copy`: a by-value use does not move out of the array.
            let copied: PortExitReason = *a;
            assert_eq!(copied, *a);
            // `Debug` is implemented (the format starts with the variant name).
            assert!(
                format!("{a:?}").starts_with("Closed")
                    || format!("{a:?}").starts_with("OwnerExited")
                    || format!("{a:?}").starts_with("Normal")
            );
        }
    }

    // -------------------------------------------------------------------------------------------
    // The emulator's statically-linked driver set (`driver_tab.c`)
    // -------------------------------------------------------------------------------------------

    #[test]
    fn every_static_spawn_driver_constructs_its_named_driver() {
        let registry = DriverRegistry::with_static_spawn_drivers();
        for name in STATIC_SPAWN_DRIVERS {
            assert!(
                is_static_spawn_driver(name),
                "{name} is a member of STATIC_SPAWN_DRIVERS"
            );
            let driver = registry
                .create_static_spawn(name)
                .unwrap_or_else(|err| panic!("{name} is a static driver, got {err}"));
            // The driver reports the very name the request asked for — the property `beam_rt`
            // renders as `port_info(Port, name)`.
            assert_eq!(driver.name(), name);
        }
        // A `with_static_spawn_drivers()` registry is exactly the static set.
        assert_eq!(registry.names(), STATIC_SPAWN_DRIVERS.to_vec());
    }

    #[test]
    fn the_static_gate_refuses_fd_and_efile_even_where_they_are_registered() {
        // `with_builtins()` DOES register the two internal drivers ...
        let registry = DriverRegistry::with_builtins();
        assert!(registry.contains("fd"));
        assert!(registry.contains("efile"));
        assert_eq!(registry.create("fd").expect("registered").name(), "fd");
        assert_eq!(
            registry.create("efile").expect("registered").name(),
            "efile"
        );

        // ... yet the static gate refuses them: the refusal is the oracle's `badarg` for
        // `open_port({spawn_driver,"fd"}, …)`, i.e. a membership decision, not a missing entry.
        for name in ["fd", "efile"] {
            assert!(
                !is_static_spawn_driver(name),
                "{name} is not an emulator driver"
            );
            match registry.create_static_spawn(name) {
                Err(BeamError::BadArg) => {}
                Err(other) => panic!("expected the oracle's badarg for {name}, got {other}"),
                Ok(driver) => panic!(
                    "expected the oracle's badarg for {name}, got a {} driver",
                    driver.name()
                ),
            }
        }

        // An invented name is likewise refused.
        assert!(matches!(
            registry.create_static_spawn("spawn"),
            Err(BeamError::BadArg)
        ));

        // And a static name is addressable but unbuildable on a registry that lacks it, which is
        // still a typed `BadArg` (never a wrong driver).
        let empty = DriverRegistry::new();
        assert!(matches!(
            empty.create_static_spawn("tcp_inet"),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn the_static_set_is_sorted_and_membership_agrees_with_it() {
        // Sorted ascending, no duplicates — the order `names()`/set comparisons rely on.
        let mut sorted = STATIC_SPAWN_DRIVERS.to_vec();
        sorted.sort_unstable();
        assert_eq!(STATIC_SPAWN_DRIVERS.to_vec(), sorted);
        assert_eq!(STATIC_SPAWN_DRIVERS.len(), 3);
        for (i, name) in STATIC_SPAWN_DRIVERS.iter().enumerate() {
            assert!(
                !STATIC_SPAWN_DRIVERS[..i].contains(name),
                "{name} appears once"
            );
        }

        // Membership, both directions.
        for name in STATIC_SPAWN_DRIVERS {
            assert!(is_static_spawn_driver(name));
        }
        for name in ["fd", "efile", "spawn", "", "TCP_INET", "tcp_inet "] {
            assert!(
                !is_static_spawn_driver(name),
                "{name:?} is not a static driver name"
            );
        }
    }

    /// Print a loud skip banner (never a silent pass), with the reason.
    fn loud_skip(reason: &str) {
        println!("================================================================");
        println!("SKIP: {reason}");
        println!("================================================================");
    }

    /// Runs `erl -noshell -eval <code> -s init stop` and returns its stdout, or `None` — after a
    /// loud skip message — when `erl` cannot be spawned or does not answer within [`ERL_TIMEOUT`].
    fn erl_stdout(code: &str) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        let code = code.to_string();
        std::thread::spawn(move || {
            let _ = tx.send(
                Command::new("erl")
                    .arg("-noshell")
                    .arg("-eval")
                    .arg(&code)
                    .arg("-s")
                    .arg("init")
                    .arg("stop")
                    .output(),
            );
        });

        match rx.recv_timeout(ERL_TIMEOUT) {
            Err(_) => {
                loud_skip(&format!(
                    "the `erl` oracle did not answer within {ERL_TIMEOUT:?}"
                ));
                None
            }
            Ok(Err(err)) => {
                loud_skip(&format!(
                    "could not spawn `erl` ({err}); is Erlang on `PATH`?"
                ));
                None
            }
            Ok(Ok(out)) => {
                assert!(
                    out.status.success(),
                    "the oracle expression failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                Some(String::from_utf8_lossy(&out.stdout).into_owned())
            }
        }
    }

    /// The double-quoted strings in `text`, in order — the names inside the oracle's printed
    /// `{ok,["tcp_inet","udp_inet","ram_file_drv"]}` list.
    fn quoted_names(text: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut rest = text;
        while let Some(open) = rest.find('"') {
            let after = &rest[open + 1..];
            let Some(close) = after.find('"') else { break };
            names.push(after[..close].to_string());
            rest = &after[close + 1..];
        }
        names
    }

    /// The oracle's own answer for the static set — `erl_ddll:loaded_drivers/0` — against
    /// [`STATIC_SPAWN_DRIVERS`].
    ///
    /// The system oracle reports `{ok,["tcp_inet","udp_inet","ram_file_drv"]}`: the `driver_tab.c`
    /// order, whose members are exactly this crate's static set (compare as a set — the oracle's
    /// list is not sorted, ours is). `fd`/`efile` are absent because they are not emulator drivers
    /// at all. Parsed rather than matched as a whole string so the test does not pin the oracle's
    /// print formatting.
    #[test]
    fn the_oracle_agrees_on_the_static_driver_set() {
        let Some(stdout) = erl_stdout("io:format(\"~p~n\", [erl_ddll:loaded_drivers()])") else {
            return;
        };

        assert!(
            stdout.contains("{ok,"),
            "the oracle answered with something other than {{ok, List}}: {stdout}"
        );
        let mut names = quoted_names(&stdout);
        assert_eq!(
            names.len(),
            STATIC_SPAWN_DRIVERS.len(),
            "the oracle's static driver list is not the three static drivers: {stdout}"
        );
        names.sort_unstable();
        assert_eq!(
            names,
            STATIC_SPAWN_DRIVERS.to_vec(),
            "the oracle's loaded_drivers/0 set differs from STATIC_SPAWN_DRIVERS: {stdout}"
        );

        // The two internal drivers really are not emulator drivers.
        for name in ["fd", "efile"] {
            assert!(
                !names.iter().any(|n| n == name),
                "the oracle reports {name} as a loaded driver: {stdout}"
            );
        }
    }
}
