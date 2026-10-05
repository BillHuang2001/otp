//! The [`Port`] — a process-like I/O endpoint — and the [`PortRegistry`] reactor glue.
//!
//! A port owns a [`Driver`], an **input** byte queue and an **output** byte queue, and a
//! lifecycle ([`PortState`]). Port ids are allocated by `beam_process::PortTable` (never a
//! second number allocator) and the registry keys its live ports by that port **number**.
//! [`PortRegistry::handle_events`] is the reactor glue: it turns the poller's readiness
//! [`Event`]s into driver callbacks.
//!
//! The legacy C analogue is the `Port`/`erts_port` control block and its `ioq`
//! (`erts/emulator/beam/erl_port.h`, `io.c`); scheduling (port tasks, reductions, the
//! reactor thread) is deliberately out of scope here and belongs to `beam_process`/`beam_rt`.
//!
//! # Implementation note: destructuring a `Port`
//!
//! A [`Port`] method that calls into its driver must build a [`DriverContext`] from
//! **disjoint** fields of `self`: destructure first, e.g.
//!
//! ```ignore
//! let Port { driver, input, output, id, .. } = self;
//! let mut ctx = DriverContext::new(*id, poller, input, output);
//! driver.output(&mut ctx)
//! ```
//!
//! so the driver (`driver`) and its context (`poller`, `input`, `output`) borrow separate
//! members of the struct. Do **not** add a combined accessor that hands out `&mut` to both
//! the driver and the queues at once — that cannot borrow-check.

use std::collections::HashMap;

use beam_common::Result;
use beam_process::{PortId, PortTable};

use crate::driver::{Driver, DriverContext, DriverRegistry};
use crate::error;
use crate::poller::{Event, Poller};
use crate::queue::ByteQueue;

/// The lifecycle state of a port.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PortState {
    /// Allocated but the driver's `start` has not completed.
    Opening,
    /// Started and able to move data.
    Open,
    /// Closing: the driver is shutting down.
    Closing,
    /// Fully closed; the id has been released.
    Closed,
}

/// A single I/O endpoint: a driver plus its input/output byte queues.
pub struct Port {
    /// This port's id (from `beam_process::PortTable`).
    id: PortId,
    /// The lifecycle state.
    state: PortState,
    /// The owning driver.
    driver: Box<dyn Driver>,
    /// Bytes read from the device, waiting for Erlang.
    input: ByteQueue,
    /// Bytes written by Erlang, waiting for the device.
    output: ByteQueue,
}

impl Port {
    /// Builds a port in the [`PortState::Opening`] state around `driver`.
    ///
    /// Called by [`PortRegistry::open`]; not part of the public API.
    pub(crate) fn new(id: PortId, driver: Box<dyn Driver>) -> Port {
        Port {
            id,
            state: PortState::Opening,
            driver,
            input: ByteQueue::new(),
            output: ByteQueue::new(),
        }
    }

    /// This port's id.
    pub fn id(&self) -> PortId {
        self.id
    }

    /// This port's lifecycle state.
    pub fn state(&self) -> PortState {
        self.state
    }

    /// The registered name of the port's driver.
    pub fn driver_name(&self) -> &str {
        self.driver.name()
    }

    /// The number of bytes currently buffered in the input queue.
    pub fn input_len(&self) -> usize {
        self.input.len()
    }

    /// Copies up to `out.len()` buffered input bytes into `out`, returning how many were
    /// copied.
    pub fn read_input(&mut self, out: &mut [u8]) -> usize {
        self.input.read_into(out)
    }

    /// Removes and returns every buffered input byte.
    pub fn drain_input(&mut self) -> Vec<u8> {
        let queue = std::mem::replace(&mut self.input, ByteQueue::new());
        queue.front().to_vec()
    }

    /// The number of bytes currently buffered in the output queue.
    pub fn output_len(&self) -> usize {
        self.output.len()
    }

    /// Starts the port: builds a context and calls the driver's `start`.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `start` returns.
    pub fn open(&mut self, poller: &mut Poller) -> Result<()> {
        let id = self.id;
        {
            // Destructure first: the driver and its context borrow disjoint fields.
            let Port {
                driver,
                input,
                output,
                ..
            } = self;
            let mut ctx = DriverContext::new(id, poller, input, output);
            driver.start(&mut ctx)?;
        }
        self.state = PortState::Open;
        Ok(())
    }

    /// Queues `data` for output and asks the driver to drain it.
    ///
    /// Returns the number of bytes the driver actually wrote; the remainder stays queued.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `output` returns.
    pub fn output(&mut self, poller: &mut Poller, data: &[u8]) -> Result<usize> {
        let id = self.id;
        let Port {
            driver,
            input,
            output,
            ..
        } = self;
        output.push(data);
        let mut ctx = DriverContext::new(id, poller, input, output);
        driver.output(&mut ctx)
    }

    /// Dispatches read readiness to the driver's `ready_input`.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `ready_input` returns.
    pub fn ready_input(&mut self, poller: &mut Poller) -> Result<()> {
        let id = self.id;
        let Port {
            driver,
            input,
            output,
            ..
        } = self;
        let mut ctx = DriverContext::new(id, poller, input, output);
        driver.ready_input(&mut ctx)
    }

    /// Dispatches write readiness to the driver's `ready_output`.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `ready_output` returns.
    pub fn ready_output(&mut self, poller: &mut Poller) -> Result<usize> {
        let id = self.id;
        let Port {
            driver,
            input,
            output,
            ..
        } = self;
        let mut ctx = DriverContext::new(id, poller, input, output);
        driver.ready_output(&mut ctx)
    }

    /// Delivers a port-control command to the driver and returns its reply bytes.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `control` returns.
    pub fn control(&mut self, poller: &mut Poller, command: &str, arg: &[u8]) -> Result<Vec<u8>> {
        let id = self.id;
        let Port {
            driver,
            input,
            output,
            ..
        } = self;
        let mut ctx = DriverContext::new(id, poller, input, output);
        driver.control(&mut ctx, command, arg)
    }

    /// Stops the driver and marks the port [`PortState::Closed`].
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `stop` returns.
    pub fn close(&mut self, poller: &mut Poller) -> Result<()> {
        self.state = PortState::Closing;
        let result = {
            let id = self.id;
            let Port {
                driver,
                input,
                output,
                ..
            } = self;
            let mut ctx = DriverContext::new(id, poller, input, output);
            driver.stop(&mut ctx)
        };
        // The port is closed either way; a teardown failure must not leave it looking
        // open.
        self.state = PortState::Closed;
        result
    }
}

/// The set of live ports, keyed by port number, plus the id allocator.
///
/// Ids come from the inner [`PortTable`] (shared with `beam_process`, so `ptab` stays the
/// single id authority); the `ports` map is keyed by `id.number()`.
pub struct PortRegistry {
    /// The port-number allocator (the single id authority).
    ids: PortTable,
    /// Live ports, keyed by port number.
    ports: HashMap<u64, Port>,
}

impl PortRegistry {
    /// An empty registry.
    pub fn new() -> PortRegistry {
        PortRegistry {
            ids: PortTable::new(),
            ports: HashMap::new(),
        }
    }

    /// Allocates an id, wraps `driver` in a [`Port`] and starts it.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::SystemLimit` when the id space is exhausted, or whatever the
    /// driver's `start` returns.
    pub fn open(&mut self, poller: &mut Poller, driver: Box<dyn Driver>) -> Result<PortId> {
        let id = self.ids.alloc()?;
        let mut port = Port::new(id, driver);
        if let Err(err) = port.open(poller) {
            // A port whose driver failed to start never becomes live: hand its number
            // back so the id space does not leak.
            self.ids.release(id);
            return Err(err);
        }
        self.ports.insert(id.number(), port);
        Ok(id)
    }

    /// Looks a driver up in `registry` and opens a port around a fresh instance.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when no driver is registered under `name`, or any error
    /// [`PortRegistry::open`] can return.
    pub fn open_named(
        &mut self,
        poller: &mut Poller,
        registry: &DriverRegistry,
        name: &str,
    ) -> Result<PortId> {
        let driver = registry.create(name)?;
        self.open(poller, driver)
    }

    /// The port with id `id`, if it is live.
    pub fn get(&self, id: PortId) -> Option<&Port> {
        self.ports.get(&id.number())
    }

    /// The port with id `id`, mutably, if it is live.
    pub fn get_mut(&mut self, id: PortId) -> Option<&mut Port> {
        self.ports.get_mut(&id.number())
    }

    /// Writes `data` to the given port's output queue.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not a live port, or whatever the port's
    /// driver returns.
    pub fn output(&mut self, poller: &mut Poller, id: PortId, data: &[u8]) -> Result<usize> {
        match self.ports.get_mut(&id.number()) {
            Some(port) => port.output(poller, data),
            None => Err(error::badarg()),
        }
    }

    /// Dispatches a batch of poller events to their ports, returning how many events were
    /// handled.
    ///
    /// Each event's key is a port number; an event for an unknown key is a no-op. Readiness
    /// is routed to the port's `ready_input`/`ready_output`.
    ///
    /// # Errors
    ///
    /// Returns any error raised by a port's driver while handling the event.
    pub fn handle_events(&mut self, poller: &mut Poller, events: &[Event]) -> Result<usize> {
        let mut handled = 0;
        for event in events {
            // The waker event carries `WAKER_KEY` and never names a port, so it falls
            // through here like any other unknown key.
            let Some(port) = self.ports.get_mut(&event.key()) else {
                continue;
            };
            handled += 1;
            let ready = event.ready();
            if ready.readable() {
                port.ready_input(poller)?;
            }
            if ready.writable() {
                port.ready_output(poller)?;
            }
        }
        Ok(handled)
    }

    /// Stops and removes the port with id `id`, releasing its number.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not a live port, or whatever the driver's
    /// `stop` returns.
    pub fn close(&mut self, poller: &mut Poller, id: PortId) -> Result<()> {
        let Some(mut port) = self.ports.remove(&id.number()) else {
            return Err(error::badarg());
        };
        // The port is gone from the registry either way, so its number is released
        // even if the driver's teardown reports a failure.
        let result = port.close(poller);
        self.ids.release(id);
        result
    }

    /// Whether a port with id `id` is live.
    pub fn contains(&self, id: PortId) -> bool {
        self.ports.contains_key(&id.number())
    }

    /// The number of live ports.
    pub fn len(&self) -> usize {
        self.ports.len()
    }

    /// Whether the registry holds no ports.
    pub fn is_empty(&self) -> bool {
        self.ports.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{PortRegistry, PortState};
    use crate::driver::DriverRegistry;
    use crate::drivers::FdDriver;
    use crate::poller::{Event, Interest, Poller, Ready, WAKER_KEY};
    use beam_common::BeamError;
    use beam_process::PortId;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;

    fn poller() -> Poller {
        Poller::new().expect("a poll set")
    }

    fn registry_with_pipe(poller: &mut Poller) -> (PortRegistry, PortId) {
        let mut registry = PortRegistry::new();
        let driver = Box::new(FdDriver::pipe().expect("a pipe"));
        let id = registry.open(poller, driver).expect("the port starts");
        (registry, id)
    }

    #[test]
    fn a_new_registry_is_empty() {
        let registry = PortRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.get(PortId::new(0)).is_none());
        assert!(!registry.contains(PortId::new(0)));
    }

    #[test]
    fn open_starts_the_port_and_close_marks_it_closed() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);
        assert_eq!(registry.len(), 1);
        assert!(registry.contains(id));
        assert_eq!(registry.get(id).expect("live").state(), PortState::Open);
        assert_eq!(registry.get(id).expect("live").driver_name(), "fd");

        registry.close(&mut poller, id).expect("closes");
        assert!(!registry.contains(id));
        assert!(registry.is_empty());
    }

    #[test]
    fn a_closed_port_id_is_reused_by_the_next_open() {
        let mut poller = poller();
        let (mut registry, first) = registry_with_pipe(&mut poller);
        let second = registry
            .open(&mut poller, Box::new(FdDriver::pipe().expect("a pipe")))
            .expect("a second port");
        assert_ne!(first, second);

        registry.close(&mut poller, first).expect("closes");
        assert_eq!(registry.len(), 1);

        let reused = registry
            .open(&mut poller, Box::new(FdDriver::pipe().expect("a pipe")))
            .expect("a third port");
        assert_eq!(reused, first, "the released number is reused");
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn unknown_ids_are_bad_arg() {
        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let missing = PortId::new(77);
        assert!(matches!(
            registry.output(&mut poller, missing, b"x"),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            registry.close(&mut poller, missing),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            registry.open_named(&mut poller, &DriverRegistry::new(), "missing"),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn open_named_uses_the_registered_driver() {
        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let drivers = DriverRegistry::with_builtins();
        let id = registry
            .open_named(&mut poller, &drivers, "fd")
            .expect("the fd driver is built in");
        assert_eq!(registry.get(id).expect("live").driver_name(), "fd");
        registry.close(&mut poller, id).expect("closes");
    }

    #[test]
    fn an_empty_port_reports_no_input() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);
        let port = registry.get_mut(id).expect("live");
        assert_eq!(port.input_len(), 0);
        assert_eq!(port.output_len(), 0);
        let mut out = [0u8; 4];
        assert_eq!(port.read_input(&mut out), 0);
        assert!(port.drain_input().is_empty());
    }

    #[test]
    fn handle_events_ignores_unknown_keys_and_the_waker() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);

        // Tokens are opaque registration handles; take one from a real registration and
        // forge the readiness the reactor would see.
        let (_peer, fd) = UnixStream::pair().expect("socketpair");
        let token = poller
            .register(fd.as_raw_fd(), Interest::READABLE, id.number())
            .expect("the fd registers");

        // Unknown keys (including the waker's, whose token is never assigned to a port)
        // are skipped without touching any driver.
        let events = vec![
            Event::new(token, 4242, Ready::default()),
            Event::new(token, WAKER_KEY, Ready::default()),
        ];
        assert_eq!(registry.handle_events(&mut poller, &events).expect("ok"), 0);
        assert!(registry.contains(id));
    }
}
