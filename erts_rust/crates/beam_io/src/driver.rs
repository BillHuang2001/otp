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

use beam_common::Result;
use beam_process::PortId;

use crate::poller::{Interest, Poller, Token};
use crate::queue::ByteQueue;

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

    /// Whether this set contains every flag in `other`.
    pub fn contains(self, other: DriverFlags) -> bool {
        self.0 & other.0 == other.0
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
}

impl<'a> DriverContext<'a> {
    /// Builds a context for `port` over the given disjoint poller/queue borrows.
    ///
    /// Called by [`crate::port::Port`] after it has destructured itself into disjoint
    /// fields, so the driver and its context borrow separate members of the `Port`.
    pub(crate) fn new(
        port: PortId,
        poller: &'a mut Poller,
        input: &'a mut ByteQueue,
        output: &'a mut ByteQueue,
    ) -> DriverContext<'a> {
        DriverContext {
            port,
            poller,
            input,
            output,
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
        self.poller.register(fd, interest, self.port.number())
    }

    /// Changes the interest mask of a registration made by this context.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the token is unknown or the re-registration fails.
    pub fn reregister(&mut self, fd: RawFd, token: Token, interest: Interest) -> Result<()> {
        self.poller.reregister(fd, token, interest)
    }

    /// Removes a registration made by this context.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the token is unknown or the de-registration fails.
    pub fn deregister(&mut self, fd: RawFd, token: Token) -> Result<()> {
        self.poller.deregister(fd, token)
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
/// Implemented by the built-in drivers ([`crate::drivers::FdDriver`]) and, later, by the
/// frozen C-ABI bridge. Every method receives a [`DriverContext`] and nothing else.
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
    /// arms write readiness by re-registering with [`Interest::WRITABLE`].
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

    /// Handles the expiry of a port timer; the default does nothing.
    ///
    /// # Errors
    ///
    /// Returns a `beam_common::BeamError` when the timeout handling fails.
    fn timeout(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        let _ = ctx;
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
    /// Milestone-1 registers the single built-in `"fd"` driver
    /// ([`crate::drivers::FdDriver`]).
    pub fn with_builtins() -> DriverRegistry {
        let mut registry = DriverRegistry::new();
        registry.register("fd", make_fd);
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
    use super::{DriverFlags, DriverRegistry};
    use beam_common::BeamError;

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
    fn with_builtins_registers_the_fd_driver() {
        let registry = DriverRegistry::with_builtins();
        assert!(registry.contains("fd"));
        assert_eq!(registry.names(), vec!["fd"]);

        let driver = registry.create("fd").expect("the fd driver exists");
        assert_eq!(driver.name(), "fd");
        assert!(driver.flags() == DriverFlags::empty());
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
}
