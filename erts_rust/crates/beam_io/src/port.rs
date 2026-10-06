//! The [`Port`] — a process-like I/O endpoint — and the [`PortRegistry`] reactor glue.
//!
//! A port owns a [`Driver`], an **input** byte queue, an **output** byte queue, a
//! [`PortTaskQueue`] and a lifecycle ([`PortState`]). Port ids are allocated by
//! `beam_process::PortTable` (never a second number allocator) and the registry keys its
//! live ports by that port **number**.
//!
//! [`PortRegistry::handle_events`] is the reactor glue: it turns the poller's readiness
//! [`Event`]s into **scheduled port tasks** ([`PortTaskKind`]) rather than calling a driver
//! inline, mirroring `erts_port_task_schedule`/`erts_port_task_execute`
//! (`erts/emulator/beam/erl_port_task.{c,h}`). [`PortRegistry::execute_port_tasks`] runs the
//! scheduled tasks under a reduction budget and reports what happened as a
//! [`PortNotification`] list — the seam the runtime turns into `{Port, {data, Data}}`
//! messages. [`PortRegistry::port_command`] queues data and schedules an `OUTPUT` task;
//! [`PortRegistry::port_control`] delivers a recognised `{command, …}`-style control
//! operation to the driver's `control`.
//!
//! The legacy C analogue is the `Port`/`erts_port` control block and its `ioq`
//! (`erts/emulator/beam/erl_port.h`, `io.c`). The *scheduler* wiring (port run queues, poll
//! threads, reductions charged to a scheduler) still belongs to `beam_process`/`beam_rt`.
//!
//! # Single-thread invariant (enforced)
//!
//! Ports are **not** thread-safe and this crate has no locking: [`Port`] and
//! [`PortRegistry`] must be created and driven from **one OS thread**, and concurrent
//! registration or dispatch from several threads is unsupported. The invariant is enforced
//! by the type system rather than by documentation alone: [`PortRegistry`] carries a
//! [`PhantomData<core::cell::Cell<()>>`](core::marker::PhantomData) marker, so it is
//! neither `Send` nor `Sync` and cannot be moved into or shared with another thread. (A
//! `Port` already holds a `Box<dyn Driver>` with no `Send`/`Sync` bound and is therefore
//! single-threaded by construction.)
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

use std::cell::Cell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::time::Instant;

use beam_common::Result;
use beam_process::{PortId, PortTable};

use crate::driver::{Driver, DriverContext, DriverRegistry, PortArms};
use crate::error;
use crate::poller::{Event, Poller};
use crate::port_task::{
    PortNotification, PortTask, PortTaskKind, PortTaskOutcome, PortTaskQueue, Readiness,
    PORT_REDS_EXECUTE,
};
use crate::queue::ByteQueue;

/// The reduction budget [`PortRegistry::handle_events`] gives the tasks it schedules when
/// the caller does not drive the queue through [`PortRegistry::execute_port_tasks`] itself.
///
/// It is one whole port execution round (`ERTS_PORT_REDS_EXECUTE`), so a single batch of
/// poller events can run at most [`crate::port_task::PORT_REDS_EXECUTE`] reductions' worth of
/// port tasks; anything left over stays queued for the next round.
pub const DEFAULT_READINESS_RED_BUDGET: usize = PORT_REDS_EXECUTE;
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

/// A single I/O endpoint: a driver plus its input/output byte queues and task queue.
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
    /// The tasks scheduled on this port, in kind-priority order.
    tasks: PortTaskQueue,
    /// Set when an `INPUT` task has run since the last notification harvest; it marks the
    /// port as a candidate for a [`PortNotification::Data`] report.
    input_pending: bool,
    /// Which readiness directions the driver has registered with the poller (see
    /// [`PortArms`]); the reactor only schedules a direction the driver can serve.
    arms: PortArms,
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
            tasks: PortTaskQueue::new(),
            input_pending: false,
            arms: PortArms::default(),
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
                arms,
                ..
            } = self;
            let mut ctx = DriverContext::new(id, poller, input, output, arms);
            driver.start(&mut ctx)?;
        }
        self.state = PortState::Open;
        Ok(())
    }

    /// Appends `data` to the output queue **without** asking the driver to write anything.
    ///
    /// The queued bytes are handed to the device by the driver's `output`/`ready_output`,
    /// which [`Port::output`] runs inline and an `OUTPUT` [port task](PortTaskKind) runs
    /// when it is executed.
    pub fn queue_output(&mut self, data: &[u8]) {
        self.output.push(data);
    }

    /// Queues `data` for output and asks the driver to drain it.
    ///
    /// Returns the number of bytes the driver actually wrote; the remainder stays queued.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `output` returns.
    pub fn output(&mut self, poller: &mut Poller, data: &[u8]) -> Result<usize> {
        self.queue_output(data);
        let id = self.id;
        let Port {
            driver,
            input,
            output,
            arms,
            ..
        } = self;
        let mut ctx = DriverContext::new(id, poller, input, output, arms);
        driver.output(&mut ctx)
    }

    /// Schedules a task of `kind`, reporting whether it was newly scheduled.
    ///
    /// At most one task of a kind is ever pending (see [`PortTaskQueue::enqueue`]), so a
    /// repeated call while the task is still queued is a no-op.
    pub fn schedule_task(&mut self, kind: PortTaskKind) -> bool {
        self.tasks.enqueue(kind)
    }

    /// Schedules a [`PortTaskKind::Timeout`] task that may not run before `deadline`.
    pub fn schedule_timeout(&mut self, deadline: Instant) -> bool {
        self.tasks.enqueue_timeout(deadline)
    }

    /// The number of tasks currently scheduled on this port.
    pub fn pending_tasks(&self) -> usize {
        self.tasks.len()
    }

    /// The kind of the task that would run next, if any.
    pub fn next_task(&self) -> Option<PortTaskKind> {
        self.tasks.next()
    }

    /// Whether a task of `kind` is scheduled on this port.
    pub fn has_pending_task(&self, kind: PortTaskKind) -> bool {
        self.tasks.contains(kind)
    }

    /// Removes and returns the task that may run at `now`, if there is one.
    pub(crate) fn pop_ready_task(&mut self, now: Instant) -> Option<PortTask> {
        self.tasks.pop_next(now)
    }

    /// Runs one scheduled `task` against the driver, returning the reductions it cost.
    ///
    /// `INPUT` runs the driver's `ready_input` and marks the port as a candidate for a
    /// [`PortNotification::Data`] report; `OUTPUT` runs its `ready_output` (which the driver
    /// may define in terms of `output`); `TIMEOUT` runs its `timeout`.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver callback returns.
    pub(crate) fn run_task(&mut self, poller: &mut Poller, task: &PortTask) -> Result<usize> {
        match task.kind() {
            PortTaskKind::Input => {
                self.ready_input(poller)?;
                self.input_pending = true;
                Ok(PortTaskKind::Input.reductions())
            }
            PortTaskKind::Output => {
                let _written = self.ready_output(poller)?;
                Ok(PortTaskKind::Output.reductions())
            }
            PortTaskKind::Timeout => {
                self.timeout(poller)?;
                Ok(PortTaskKind::Timeout.reductions())
            }
        }
    }

    /// Takes the [`PortNotification::Data`] this port owes, if any.
    ///
    /// The bytes an `INPUT` task read are drained out of the input queue here; a port that
    /// ran no `INPUT` task (or whose driver read nothing) yields `None`.
    pub(crate) fn take_input_notification(&mut self) -> Option<PortNotification> {
        if !self.input_pending {
            return None;
        }
        self.input_pending = false;
        let bytes = self.drain_input();
        if bytes.is_empty() {
            None
        } else {
            Some(PortNotification::Data {
                port: self.id,
                bytes,
            })
        }
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
            arms,
            ..
        } = self;
        let mut ctx = DriverContext::new(id, poller, input, output, arms);
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
            arms,
            ..
        } = self;
        let mut ctx = DriverContext::new(id, poller, input, output, arms);
        driver.ready_output(&mut ctx)
    }

    /// Dispatches a port-timer expiry to the driver's `timeout`.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `timeout` returns.
    pub fn timeout(&mut self, poller: &mut Poller) -> Result<()> {
        let id = self.id;
        let Port {
            driver,
            input,
            output,
            arms,
            ..
        } = self;
        let mut ctx = DriverContext::new(id, poller, input, output, arms);
        driver.timeout(&mut ctx)
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
            arms,
            ..
        } = self;
        let mut ctx = DriverContext::new(id, poller, input, output, arms);
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
                arms,
                ..
            } = self;
            let mut ctx = DriverContext::new(id, poller, input, output, arms);
            driver.stop(&mut ctx)
        };
        // The port is closed either way; a teardown failure must not leave it looking
        // open.
        self.state = PortState::Closed;
        result
    }
}

/// A recognised port-control operation.
///
/// This is the `{command, …}` surface [`PortRegistry::port_control`] decodes. Only the
/// names this crate can serve **honestly** are recognised, and the atom/tuple decoding of
/// the Erlang side (`Port ! {self(), {command, Data}}`, `erlang:port_control/3`'s integer
/// operations) is deliberately the runtime's job:
///
/// | Name | Meaning here |
/// |---|---|
/// | `"command"` | forward the payload to the driver's `control` |
/// | `"getopts"` | forward to the driver's `control` (the default driver has no options) |
/// | `"close"` | close the port (driver `stop` + id release) |
///
/// Anything else is an argument fault (`BeamError::BadArg`) rather than a silently ignored
/// command.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PortControl {
    /// A driver-defined command carrying payload bytes.
    Command,
    /// Ask the driver for its current options.
    GetOpts,
    /// Close the port.
    Close,
}

impl PortControl {
    /// Decodes a control-operation name, or `None` when the name is not recognised.
    pub fn from_str(name: &str) -> Option<PortControl> {
        match name {
            "command" => Some(PortControl::Command),
            "getopts" => Some(PortControl::GetOpts),
            "close" => Some(PortControl::Close),
            _ => None,
        }
    }

    /// The canonical name of this operation (the string the driver's `control` sees).
    pub const fn as_str(self) -> &'static str {
        match self {
            PortControl::Command => "command",
            PortControl::GetOpts => "getopts",
            PortControl::Close => "close",
        }
    }
}

/// The options `erlang:port_command/3` accepts, as a small flag set.
///
/// `nosuspend` and `force` mirror the Erlang options. Milestone-1 ports have **no** busy
/// state, no command queue and never suspend the caller (there is only one OS thread and
/// the command is queued unconditionally), so both flags are validated and carried but
/// change nothing yet: the runtime's `port_command/3` BIF layer owns their effect once a
/// busy-port command queue exists. They are modeled here rather than dropped so that the
/// flag plumbing is in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PortCommandOptions {
    /// The `nosuspend` option.
    nosuspend: bool,
    /// The `force` option.
    force: bool,
}

impl PortCommandOptions {
    /// The empty option set (`port_command/2`).
    pub const EMPTY: PortCommandOptions = PortCommandOptions {
        nosuspend: false,
        force: false,
    };

    /// The empty option set.
    pub const fn new() -> PortCommandOptions {
        PortCommandOptions::EMPTY
    }

    /// This option set with `nosuspend` set to `nosuspend`.
    pub const fn with_nosuspend(self, nosuspend: bool) -> PortCommandOptions {
        PortCommandOptions {
            nosuspend,
            force: self.force,
        }
    }

    /// This option set with `force` set to `force`.
    pub const fn with_force(self, force: bool) -> PortCommandOptions {
        PortCommandOptions {
            nosuspend: self.nosuspend,
            force,
        }
    }

    /// Whether the `nosuspend` option is set.
    pub const fn is_nosuspend(self) -> bool {
        self.nosuspend
    }

    /// Whether the `force` option is set.
    pub const fn is_force(self) -> bool {
        self.force
    }
}

/// The set of live ports, keyed by port number, plus the id allocator.
///
/// Ids come from the inner [`PortTable`] (shared with `beam_process`, so `ptab` stays the
/// single id authority); the `ports` map is keyed by `id.number()`.
///
/// A `PortRegistry` is **not** `Send` or `Sync` (see the module docs, "Single-thread
/// invariant"): it must be driven from one OS thread, and the [`PhantomData<Cell<()>>`]
/// marker below makes that a compile-time property rather than a comment.
///
/// [`PhantomData<Cell<()>>`]: core::marker::PhantomData
pub struct PortRegistry {
    /// The port-number allocator (the single id authority).
    ids: PortTable,
    /// Live ports, keyed by port number.
    ports: HashMap<u64, Port>,
    /// Ports closed since the last execution round, reported as
    /// [`PortNotification::Closed`].
    closed: Vec<PortId>,
    /// Keeps this type single-threaded (`Cell` is neither `Send` nor `Sync`).
    single_threaded: PhantomData<Cell<()>>,
}

impl PortRegistry {
    /// An empty registry.
    pub fn new() -> PortRegistry {
        PortRegistry {
            ids: PortTable::new(),
            ports: HashMap::new(),
            closed: Vec::new(),
            single_threaded: PhantomData,
        }
    }

    /// Allocates an id, wraps `driver` in a [`Port`] and starts it.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::SystemLimit` when the id space is exhausted, or whatever the
    /// driver's `start` returns. When `start` fails, the driver is **torn down** (its
    /// `stop` runs, deregistering any fd it registered before failing) and its number is
    /// released, so neither the id space nor the poll set leaks; the `stop` outcome is
    /// deliberately discarded because the `start` failure is the error the caller must
    /// observe.
    pub fn open(&mut self, poller: &mut Poller, driver: Box<dyn Driver>) -> Result<PortId> {
        let id = self.ids.alloc()?;
        let mut port = Port::new(id, driver);
        if let Err(err) = port.open(poller) {
            // A port whose driver failed to start never becomes live: run the driver's
            // teardown so a registration it made before failing is released, then hand its
            // number back so the id space does not leak.
            let _teardown = port.close(poller);
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

    /// Schedules the port tasks a batch of poller events implies and then runs what it
    /// scheduled, returning how many events named a live port.
    ///
    /// Each event's key is a port number; an event for an unknown key (including the
    /// waker's [`WAKER_KEY`](crate::poller::WAKER_KEY)) is a no-op. Readiness is turned into
    /// scheduled port tasks rather than into direct driver calls (see
    /// [`PortRegistry::schedule_events`]), and the scheduled tasks are then executed under
    /// [`DEFAULT_READINESS_RED_BUDGET`] so a caller that does not drive the queue itself
    /// still sees ports make progress. A caller that wants full control over the reduction
    /// budget schedules with [`PortRegistry::schedule_events`] and runs the queue through
    /// [`PortRegistry::execute_port_tasks`].
    ///
    /// # Errors
    ///
    /// Returns any error raised by a port's driver while the scheduled tasks run.
    pub fn handle_events(&mut self, poller: &mut Poller, events: &[Event]) -> Result<usize> {
        let handled = self.schedule_events(events);
        let _outcome = self.run_scheduled_tasks(poller, DEFAULT_READINESS_RED_BUDGET)?;
        Ok(handled)
    }

    /// Schedules the port tasks a batch of poller events implies, returning how many events
    /// named a **live** port.
    ///
    /// Nothing is executed; the caller drives the queue with
    /// [`PortRegistry::execute_port_tasks`].
    pub fn schedule_events(&mut self, events: &[Event]) -> usize {
        let mut handled = 0;
        for event in events {
            if self.schedule_readiness(event.key(), Readiness::from(event.ready())) {
                handled += 1;
            }
        }
        handled
    }

    /// Schedules the port tasks `readiness` implies for the port registered under `key`,
    /// returning whether `key` named a live port.
    ///
    /// The rule is [`Readiness::read_ready`] ⇒ an [`PortTaskKind::Input`] task and
    /// [`Readiness::write_ready`] ⇒ an [`PortTaskKind::Output`] task, each applied only when
    /// the driver has **armed** that direction with the poller ([`PortArms`]). A pure
    /// EOF/`EPOLLHUP` event (`read_closed` with `readable == false`) therefore still reaches
    /// the driver's `ready_input` — which is where the end-of-file latch lives — while a
    /// write-closed flag on a port that never armed a write fd schedules nothing (the
    /// kernel reports `EPOLLHUP` as *both* closed edges, so a full close of a read-only
    /// port must not be mistaken for write readiness).
    pub fn schedule_readiness(&mut self, key: u64, readiness: Readiness) -> bool {
        let Some(port) = self.ports.get_mut(&key) else {
            return false;
        };
        if readiness.read_ready() && port.arms.read_armed() {
            port.schedule_task(PortTaskKind::Input);
        }
        if readiness.write_ready() && port.arms.write_armed() {
            port.schedule_task(PortTaskKind::Output);
        }
        true
    }

    /// Executes the scheduled port tasks under `reds_budget` and reports what happened.
    ///
    /// The round runs the queued tasks in kind-priority order (`INPUT` → `OUTPUT` →
    /// `TIMEOUT`), stopping as soon as `reds_budget` is exhausted and leaving the rest
    /// queued; a [`PortTaskKind::Timeout`] task whose deadline has not passed is not run.
    /// Afterwards every port that ran an `INPUT` task has its buffered input drained into a
    /// [`PortNotification::Data`], and every port closed since the previous round is
    /// reported as a [`PortNotification::Closed`].
    ///
    /// # Errors
    ///
    /// Returns any error raised by a port's driver while a task runs; the tasks after it
    /// stay queued.
    pub fn execute_port_tasks(
        &mut self,
        poller: &mut Poller,
        reds_budget: usize,
    ) -> Result<PortTaskOutcome> {
        let mut notifications: Vec<PortNotification> = self
            .closed
            .drain(..)
            .map(|port| PortNotification::Closed { port })
            .collect();

        let (ran, reds_used) = self.run_scheduled_tasks(poller, reds_budget)?;

        // Harvest the data the INPUT tasks read: a port task never hands bytes up itself,
        // so the round reports them for the runtime to turn into `{Port, {data, Data}}`.
        let keys = self.sorted_keys();
        for key in keys {
            if let Some(port) = self.ports.get_mut(&key) {
                if let Some(notification) = port.take_input_notification() {
                    notifications.push(notification);
                }
            }
        }

        Ok(PortTaskOutcome {
            ran,
            reds_used,
            notifications,
        })
    }

    /// Queues `data` on the port's output queue and schedules an [`PortTaskKind::Output`]
    /// task, returning as soon as the data is queued.
    ///
    /// This mirrors `erlang:port_command/2`, which returns `true` immediately: the actual
    /// write happens when the scheduled task runs (through
    /// [`PortRegistry::execute_port_tasks`], or through [`PortRegistry::handle_events`]'s
    /// default round when the port is write-ready).
    ///
    /// The `poller` argument is taken for signature symmetry with the registry's other
    /// entry points; this call does not touch the poll set (the write itself belongs to the
    /// port task, which is where any write-readiness arming happens).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not a live, open port.
    pub fn port_command(
        &mut self,
        _poller: &mut Poller,
        id: PortId,
        data: &[u8],
        options: PortCommandOptions,
    ) -> Result<()> {
        let Some(port) = self.ports.get_mut(&id.number()) else {
            return Err(error::badarg());
        };
        if port.state() != PortState::Open {
            return Err(error::badarg());
        }
        // `nosuspend`/`force` describe what a *busy* port does with a command; M1 ports are
        // never busy and never suspend (see `PortCommandOptions`), so the flags are carried
        // but change no behaviour yet.
        let _options = options;
        port.queue_output(data);
        port.schedule_task(PortTaskKind::Output);
        Ok(())
    }

    /// Delivers a named port-control operation to the port's driver.
    ///
    /// Only the [`PortControl`] names are recognised; an unknown name, or an `id` that is
    /// not a live port, is `BeamError::BadArg`. `"command"`/`"getopts"` are forwarded to the
    /// driver's `control` (whose default accepts everything and answers with no bytes);
    /// `"close"` closes the port like [`PortRegistry::close`] and returns no bytes.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` for an unknown control name or an unknown port, or
    /// whatever the driver's `control`/`stop` returns.
    pub fn port_control(
        &mut self,
        poller: &mut Poller,
        id: PortId,
        command: &str,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        let Some(control) = PortControl::from_str(command) else {
            return Err(error::badarg());
        };
        match control {
            PortControl::Close => {
                self.close(poller, id)?;
                Ok(Vec::new())
            }
            PortControl::Command | PortControl::GetOpts => match self.ports.get_mut(&id.number())
            {
                Some(port) => port.control(poller, control.as_str(), arg),
                None => Err(error::badarg()),
            },
        }
    }

    /// Runs the tasks scheduled on every port until `reds_budget` is exhausted.
    ///
    /// Returns `(tasks run, reductions used)`. The ports are visited in port-number order
    /// so a round is deterministic.
    fn run_scheduled_tasks(
        &mut self,
        poller: &mut Poller,
        reds_budget: usize,
    ) -> Result<(usize, usize)> {
        let mut ran = 0usize;
        let mut reds_used = 0usize;
        for key in self.sorted_keys() {
            while reds_used < reds_budget {
                let now = Instant::now();
                let Some(port) = self.ports.get_mut(&key) else {
                    break;
                };
                let Some(task) = port.pop_ready_task(now) else {
                    break;
                };
                let reds = port.run_task(poller, &task)?;
                ran += 1;
                reds_used += reds;
            }
            if reds_used >= reds_budget {
                break;
            }
        }
        Ok((ran, reds_used))
    }

    /// The live port numbers, ascending, so iteration order is deterministic.
    fn sorted_keys(&self) -> Vec<u64> {
        let mut keys: Vec<u64> = self.ports.keys().copied().collect();
        keys.sort_unstable();
        keys
    }

    /// Stops and removes the port with id `id`, releasing its number.
    ///
    /// The close is reported as a [`PortNotification::Closed`] by the next
    /// [`PortRegistry::execute_port_tasks`] round.
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
        self.closed.push(id);
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
    use super::{
        PortCommandOptions, PortControl, PortNotification, PortRegistry, PortState,
        DEFAULT_READINESS_RED_BUDGET,
    };
    use crate::driver::{Driver, DriverContext, DriverRegistry};
    use crate::drivers::FdDriver;
    use crate::error;
    use crate::poller::{Event, Interest, Poller, Ready, Token, WAKER_KEY};
    use crate::port_task::{PortTaskKind, Readiness, PORT_REDS_INPUT, PORT_REDS_TIMEOUT};
    use beam_common::{BeamError, Result};
    use beam_process::PortId;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Instant;
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

    // --- port-task scheduling ------------------------------------------------

    #[test]
    fn a_pipe_port_arms_only_the_read_direction() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);

        // `FdDriver::pipe` registers its read fd `READABLE` in `start`; nothing arms
        // writability, so write readiness cannot be served by this driver.
        assert!(registry.schedule_readiness(
            id.number(),
            Readiness {
                readable: true,
                ..Readiness::default()
            }
        ));
        assert!(registry
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Input));

        assert!(registry.schedule_readiness(
            id.number(),
            Readiness {
                writable: true,
                ..Readiness::default()
            }
        ));
        assert!(
            !registry
                .get(id)
                .expect("live")
                .has_pending_task(PortTaskKind::Output),
            "an unarmed write direction schedules nothing"
        );

        // An unknown key is not a live port.
        assert!(!registry.schedule_readiness(
            4242,
            Readiness {
                readable: true,
                ..Readiness::default()
            }
        ));
    }

    #[test]
    fn schedule_events_counts_live_keys_and_schedules_nothing_for_idle_events() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);
        let (_peer, probe) = UnixStream::pair().expect("socketpair");
        let token = poller
            .register(probe.as_raw_fd(), Interest::READABLE, id.number())
            .expect("the probe registers");

        // `Ready` has no public constructor, so a forged event can only report the empty
        // mask; the events below therefore schedule nothing while still naming a live port,
        // which is what the "handled" count counts.
        let idle = vec![
            Event::new(token, id.number(), Ready::default()),
            Event::new(token, 4242, Ready::default()),
            Event::new(token, WAKER_KEY, Ready::default()),
        ];
        assert_eq!(registry.schedule_events(&idle), 1);
        assert_eq!(registry.get(id).expect("live").pending_tasks(), 0);
    }

    #[test]
    fn a_pure_eof_event_schedules_an_input_task() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);

        // `EPOLLHUP` with neither the readable nor the writable bit: the event the old
        // inline dispatch dropped. It must still reach `ready_input`, which is where
        // `FdDriver` latches end-of-file.
        let eof = Readiness {
            read_closed: true,
            ..Readiness::default()
        };
        assert!(eof.read_ready() && !eof.write_ready());
        assert!(registry.schedule_readiness(id.number(), eof));
        assert_eq!(
            registry.get(id).expect("live").next_task(),
            Some(PortTaskKind::Input)
        );

        let outcome = registry
            .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("the EOF task runs");
        assert_eq!(outcome.ran, 1, "the EOF event ran exactly one INPUT task");
        assert_eq!(outcome.reds_used, PORT_REDS_INPUT);
        assert!(
            outcome.notifications.is_empty(),
            "EOF reads no bytes, so no data is owed"
        );
    }

    #[test]
    fn an_input_task_reports_the_bytes_it_read() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);

        // `output` writes straight to the write end, so the read end is readable at once and
        // the round below needs no `poll` at all.
        assert_eq!(registry.output(&mut poller, id, b"hi").expect("writes"), 2);
        assert!(registry.schedule_readiness(
            id.number(),
            Readiness {
                readable: true,
                ..Readiness::default()
            }
        ));

        let outcome = registry
            .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("the input task runs");
        assert_eq!(outcome.ran, 1);
        assert_eq!(outcome.reds_used, PORT_REDS_INPUT);
        assert_eq!(
            outcome.notifications,
            vec![PortNotification::Data {
                port: id,
                bytes: b"hi".to_vec()
            }]
        );
        assert_eq!(
            registry.get_mut(id).expect("live").input_len(),
            0,
            "the report drained the input queue"
        );
    }

    #[test]
    fn an_empty_read_reports_no_data() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);

        // Read readiness on an empty pipe: `ready_input` runs (and finds `EAGAIN`), but it
        // reads nothing, so no `Data` notification is owed.
        assert!(registry.schedule_readiness(
            id.number(),
            Readiness {
                readable: true,
                ..Readiness::default()
            }
        ));
        let outcome = registry
            .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("runs");
        assert_eq!(outcome.ran, 1);
        assert!(outcome.notifications.is_empty());
    }

    #[test]
    fn an_output_task_drains_the_queued_bytes() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);
        registry.get_mut(id).expect("live").queue_output(b"out");
        assert_eq!(registry.get(id).expect("live").output_len(), 3);

        assert!(registry
            .get_mut(id)
            .expect("live")
            .schedule_task(PortTaskKind::Output));
        let outcome = registry
            .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("runs");
        assert_eq!(outcome.ran, 1);
        assert!(outcome.notifications.is_empty());
        assert_eq!(
            registry.get(id).expect("live").output_len(),
            0,
            "the write drained the output queue"
        );
    }

    #[test]
    fn a_due_timeout_task_runs_through_the_driver() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);

        // A deadline already in the past is due immediately.
        assert!(registry
            .get_mut(id)
            .expect("live")
            .schedule_timeout(Instant::now()));
        let outcome = registry
            .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("runs");
        assert_eq!(outcome.ran, 1);
        assert_eq!(outcome.reds_used, PORT_REDS_TIMEOUT);
        assert_eq!(registry.get(id).expect("live").pending_tasks(), 0);
    }

    #[test]
    fn an_unexpired_timeout_stays_queued() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        assert!(registry.get_mut(id).expect("live").schedule_timeout(deadline));

        let outcome = registry
            .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("runs");
        assert_eq!(outcome.ran, 0, "a timer that is not due must not run early");
        assert!(registry
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Timeout));
    }

    #[test]
    fn a_close_is_reported_once_as_a_closed_notification() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);
        registry.close(&mut poller, id).expect("closes");

        let first = registry
            .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("runs");
        assert_eq!(first.notifications, vec![PortNotification::Closed { port: id }]);

        let second = registry
            .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("runs");
        assert!(
            second.notifications.is_empty(),
            "a close is reported exactly once"
        );
    }

    #[test]
    fn the_reduction_budget_bounds_one_round() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);
        assert!(registry
            .get_mut(id)
            .expect("live")
            .schedule_task(PortTaskKind::Input));
        assert!(registry
            .get_mut(id)
            .expect("live")
            .schedule_task(PortTaskKind::Output));

        // A zero budget runs nothing and leaves the queue intact.
        let none = registry.execute_port_tasks(&mut poller, 0).expect("runs");
        assert_eq!((none.ran, none.reds_used), (0, 0));
        assert_eq!(registry.get(id).expect("live").pending_tasks(), 2);

        // One INPUT's worth of reductions runs exactly the INPUT task.
        let one = registry
            .execute_port_tasks(&mut poller, PORT_REDS_INPUT)
            .expect("runs");
        assert_eq!((one.ran, one.reds_used), (1, PORT_REDS_INPUT));
        assert_eq!(
            registry.get(id).expect("live").next_task(),
            Some(PortTaskKind::Output)
        );
    }

    // --- `port_command` and the control protocol family ----------------------

    #[test]
    fn port_command_queues_and_schedules_without_writing() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);

        let options = PortCommandOptions::new().with_nosuspend(true).with_force(true);
        registry
            .port_command(&mut poller, id, b"cmd!", options)
            .expect("the command is queued");

        // Nothing was written: the bytes are still queued and only a task is pending, which
        // is exactly what `erlang:port_command/2` returning `true` immediately means.
        assert_eq!(registry.get(id).expect("live").output_len(), 4);
        assert!(registry
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Output));

        let outcome = registry
            .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("the output task runs");
        assert_eq!(outcome.ran, 1);
        assert_eq!(registry.get(id).expect("live").output_len(), 0);

        // An unknown port is an argument fault.
        assert!(matches!(
            registry.port_command(&mut poller, PortId::new(9999), b"x", PortCommandOptions::new()),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn port_command_rejects_a_closed_port() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);
        registry.close(&mut poller, id).expect("closes");
        assert!(matches!(
            registry.port_command(&mut poller, id, b"x", PortCommandOptions::new()),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn port_command_options_are_a_flag_set() {
        assert_eq!(PortCommandOptions::default(), PortCommandOptions::EMPTY);
        assert!(!PortCommandOptions::EMPTY.is_nosuspend());
        assert!(!PortCommandOptions::EMPTY.is_force());

        let both = PortCommandOptions::EMPTY.with_nosuspend(true).with_force(true);
        assert!(both.is_nosuspend() && both.is_force());
        assert!(!both.with_nosuspend(false).is_nosuspend());
        assert!(!both.with_force(false).is_force());
        assert!(both.with_force(false).is_nosuspend());
    }

    #[test]
    fn the_control_protocol_family_is_decoded() {
        assert_eq!(PortControl::from_str("command"), Some(PortControl::Command));
        assert_eq!(PortControl::from_str("getopts"), Some(PortControl::GetOpts));
        assert_eq!(PortControl::from_str("close"), Some(PortControl::Close));
        assert_eq!(PortControl::from_str("nosuch"), None);

        assert_eq!(PortControl::Command.as_str(), "command");
        assert_eq!(PortControl::GetOpts.as_str(), "getopts");
        assert_eq!(PortControl::Close.as_str(), "close");
    }

    #[test]
    fn port_control_forwards_known_commands_and_rejects_unknown_ones() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);

        // `getopts`/`command` reach the driver's `control`; the fd driver's default accepts
        // them and answers with no bytes.
        assert!(registry
            .port_control(&mut poller, id, "getopts", b"")
            .expect("getopts is recognised")
            .is_empty());
        assert!(registry
            .port_control(&mut poller, id, "command", b"\x00")
            .expect("command is recognised")
            .is_empty());

        // An unknown name is an argument fault, never a silent no-op.
        assert!(matches!(
            registry.port_control(&mut poller, id, "invent", b""),
            Err(BeamError::BadArg)
        ));

        // `close` closes the port and answers with no bytes.
        assert!(registry
            .port_control(&mut poller, id, "close", b"")
            .expect("close is recognised")
            .is_empty());
        assert!(!registry.contains(id));

        // The port is gone, so anything else addressed to it is an argument fault.
        assert!(matches!(
            registry.port_control(&mut poller, id, "getopts", b""),
            Err(BeamError::BadArg)
        ));
    }

    // --- failed `start` teardown ---------------------------------------------

    /// A driver whose `start` registers a descriptor with the poller and *then* fails, so the
    /// failed-`start` teardown path (`start` failure ⇒ `stop` ⇒ id release) can be pinned.
    struct FailingStartDriver {
        stream: UnixStream,
        token: Option<Token>,
    }

    impl FailingStartDriver {
        fn new(stream: UnixStream) -> FailingStartDriver {
            FailingStartDriver {
                stream,
                token: None,
            }
        }
    }

    impl Driver for FailingStartDriver {
        fn name(&self) -> &str {
            "failing"
        }

        fn start(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
            self.token = Some(ctx.register(self.stream.as_raw_fd(), Interest::READABLE)?);
            Err(error::system_limit())
        }

        fn output(&mut self, _ctx: &mut DriverContext<'_>) -> Result<usize> {
            Ok(0)
        }

        fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            Ok(())
        }

        fn stop(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
            match self.token.take() {
                Some(token) => ctx.deregister(self.stream.as_raw_fd(), token),
                None => Ok(()),
            }
        }
    }

    #[test]
    fn a_failed_start_deregisters_the_fd_and_releases_the_id() {
        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let (_peer, end) = UnixStream::pair().expect("socketpair");

        let failed = registry.open(&mut poller, Box::new(FailingStartDriver::new(end)));
        assert!(matches!(failed, Err(BeamError::SystemLimit)));
        assert!(
            poller.is_empty(),
            "the registration made before the failure is released by the teardown"
        );
        assert!(registry.is_empty(), "a port that never started is not live");
        assert_eq!(registry.ids.len(), 0, "the failed port's number was released");

        let id = registry
            .open(&mut poller, Box::new(FdDriver::pipe().expect("a pipe")))
            .expect("the next port starts");
        assert_eq!(
            id.number(),
            0,
            "the number the failed port held is handed out again"
        );
    }
}