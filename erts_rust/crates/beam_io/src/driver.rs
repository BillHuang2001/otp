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

use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::time::Instant;

use beam_common::Result;
use beam_process::PortId;

use crate::poller::{Interest, Poller, Token};
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
}

impl<'a> DriverContext<'a> {
    /// Builds a context for `port` over the given disjoint poller/queue/arming borrows.
    ///
    /// Called by [`crate::port::Port`] after it has destructured itself into disjoint
    /// fields, so the driver and its context borrow separate members of the `Port`.
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
        }
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
    pub fn input(&mut self) -> &mut ByteQueue {
        self.input
    }

    /// The port's output queue, which the driver drains bytes waiting to be written from.
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
    pub fn with_builtins() -> DriverRegistry {
        let mut registry = DriverRegistry::new();
        registry.register("fd", make_fd);
        registry.register("efile", crate::drivers::make_efile);
        registry.register("tcp_inet", crate::drivers::make_tcp_inet);
        registry.register("udp_inet", crate::drivers::make_udp_inet);
        registry.register("ram_file_drv", crate::drivers::make_ram_file);
        registry
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
    use super::{Driver, DriverContext, DriverFlags, DriverRegistry, PortArms, PortExitReason};
    use crate::poller::Poller;
    use crate::queue::ByteQueue;
    use beam_common::{BeamError, Result};
    use beam_process::PortId;

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

    #[test]
    fn port_exit_reason_is_a_closed_copyable_vocabulary() {
        let all = [
            PortExitReason::Closed,
            PortExitReason::OwnerExited,
            PortExitReason::Normal,
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
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
}
