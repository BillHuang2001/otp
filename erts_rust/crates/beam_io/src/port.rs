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

use crate::driver::{Driver, DriverRegistry};
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
        let _ = out;
        todo!("self.input.read_into(out)")
    }

    /// Removes and returns every buffered input byte.
    pub fn drain_input(&mut self) -> Vec<u8> {
        todo!("take self.input.front() and clear the queue")
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
        let _ = poller;
        todo!("destructure self, build a DriverContext, call driver.start, set PortState::Open")
    }

    /// Queues `data` for output and asks the driver to drain it.
    ///
    /// Returns the number of bytes the driver actually wrote; the remainder stays queued.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `output` returns.
    pub fn output(&mut self, poller: &mut Poller, data: &[u8]) -> Result<usize> {
        let _ = (poller, data);
        todo!("push `data` onto the output queue, then call driver.output via a DriverContext")
    }

    /// Dispatches read readiness to the driver's `ready_input`.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `ready_input` returns.
    pub fn ready_input(&mut self, poller: &mut Poller) -> Result<()> {
        let _ = poller;
        todo!("destructure self, build a DriverContext, call driver.ready_input")
    }

    /// Dispatches write readiness to the driver's `ready_output`.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `ready_output` returns.
    pub fn ready_output(&mut self, poller: &mut Poller) -> Result<usize> {
        let _ = poller;
        todo!("destructure self, build a DriverContext, call driver.ready_output")
    }

    /// Delivers a port-control command to the driver and returns its reply bytes.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `control` returns.
    pub fn control(
        &mut self,
        poller: &mut Poller,
        command: &str,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        let _ = (poller, command, arg);
        todo!("destructure self, build a DriverContext, call driver.control")
    }

    /// Stops the driver and marks the port [`PortState::Closed`].
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `stop` returns.
    pub fn close(&mut self, poller: &mut Poller) -> Result<()> {
        let _ = poller;
        todo!("destructure self, build a DriverContext, call driver.stop, set PortState::Closed")
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
        let _ = (poller, driver);
        todo!("ids.alloc(), insert Port::new(id, driver), then port.open(poller)")
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
        let _ = (poller, registry, name);
        todo!("registry.create(name) then self.open(poller, driver)")
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
        let _ = (poller, id, data);
        todo!("get_mut(id) or error::badarg(), then port.output(poller, data)")
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
        let _ = (poller, events);
        todo!("for each event, look the port up by key and dispatch its Ready to the driver")
    }

    /// Stops and removes the port with id `id`, releasing its number.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not a live port, or whatever the driver's
    /// `stop` returns.
    pub fn close(&mut self, poller: &mut Poller, id: PortId) -> Result<()> {
        let _ = (poller, id);
        todo!("remove the port, call its close, then ids.release(id)")
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
