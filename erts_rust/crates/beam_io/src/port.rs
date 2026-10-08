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

//! The [`Port`] — a process-like I/O endpoint — plus its lifecycle, control and command
//! vocabularies.
//!
//! A port owns a [`Driver`], an **input** byte queue, an **output** byte queue, a
//! [`PortTaskQueue`] and a lifecycle ([`PortState`]). Port ids are allocated by
//! `beam_process::PortTable` (never a second number allocator); the set of live ports, the id
//! allocator and the reactor glue that schedules and runs their tasks live in the sibling
//! [`crate::registry`] module ([`PortRegistry`]).
//!
//! The legacy C analogue is the `Port`/`erts_port` control block and its `ioq`
//! (`erts/emulator/beam/erl_port.h`, `io.c`). The *scheduler* wiring (port run queues, poll
//! threads, reductions charged to a scheduler) still belongs to `beam_process`/`beam_rt`.
//!
//! # Single-thread invariant (enforced)
//!
//! Ports are **not** thread-safe and this crate has no locking: [`Port`] and [`PortRegistry`]
//! must be created and driven from **one OS thread**, and concurrent registration or dispatch
//! from several threads is unsupported. The invariant is enforced by the type system rather
//! than by documentation alone: [`PortRegistry`] carries a
//! [`PhantomData<core::cell::Cell<()>>`](core::marker::PhantomData) marker, so it is neither
//! `Send` nor `Sync` and cannot be moved into or shared with another thread. (A `Port` already
//! holds a `Box<dyn Driver>` with no `Send`/`Sync` bound and is therefore single-threaded by
//! construction.)
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

use std::time::Instant;

use beam_common::Result;
use beam_process::PortId;

use crate::driver::{Driver, DriverContext, PortArms, PortExitReason};
use crate::poller::Poller;
use crate::port_task::{PortNotification, PortTask, PortTaskKind, PortTaskQueue, PORT_REDS_EXECUTE};
use crate::queue::ByteQueue;

// The registry lives in the sibling [`crate::registry`] module; re-export it here so the
// in-crate `crate::port::PortRegistry` path (used by the built-in drivers) keeps resolving.
pub use crate::registry::PortRegistry;

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
    /// Which readiness directions the driver has registered with the poller (see
    /// [`PortArms`]); the reactor only schedules a direction the driver can serve.
    arms: PortArms,
    /// Set once a [`PortExitReason`] has been delivered to the driver, so
    /// [`Port::notify_exit`] delivers the port-exit notification **at most once**.
    exit_notified: bool,
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
            arms: PortArms::default(),
            exit_notified: false,
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
    /// The port's `TIMEOUT` task is then armed from [`Driver::next_timeout`] (see
    /// [`Port::refresh_timeout`]), so a driver that declares a deadline in `start` gets its
    /// timer.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `start` returns.
    pub fn open(&mut self, poller: &mut Poller) -> Result<()> {
        let id = self.id;
        let result = {
            // Destructure first: the driver and its context borrow disjoint fields.
            let Port {
                driver,
                input,
                output,
                arms,
                ..
            } = self;
            let mut ctx = DriverContext::new(id, poller, input, output, arms);
            driver.start(&mut ctx)
        };
        self.refresh_timeout();
        result?;
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
    /// Returns the number of bytes the driver actually wrote; the remainder stays queued. The
    /// port's `TIMEOUT` task is re-armed from [`Driver::next_timeout`] afterwards (see
    /// [`Port::refresh_timeout`]).
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `output` returns.
    pub fn output(&mut self, poller: &mut Poller, data: &[u8]) -> Result<usize> {
        self.queue_output(data);
        let id = self.id;
        let result = {
            let Port {
                driver,
                input,
                output,
                arms,
                ..
            } = self;
            let mut ctx = DriverContext::new(id, poller, input, output, arms);
            driver.output(&mut ctx)
        };
        self.refresh_timeout();
        result
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

    /// Arms this port's `TIMEOUT` task at `deadline`, **replacing** any deadline already armed.
    ///
    /// This is the explicit counterpart of the automatic re-arm from [`Driver::next_timeout`]
    /// (see [`Port::refresh_timeout`]), and the one place a caller can re-point the timer: it
    /// always (re-)targets the port's single `TIMEOUT` task, unlike
    /// [`Port::schedule_timeout`], which is a no-op while a timer is already pending. The task
    /// still only runs once `deadline` has passed (see [`PortTaskQueue::pop_next`]).
    pub fn arm_timeout(&mut self, deadline: Instant) {
        self.tasks.arm_timeout(deadline);
    }

    /// Drops any pending `TIMEOUT` task, reporting whether one was pending.
    ///
    /// A later driver interaction re-derives the timer from [`Driver::next_timeout`], which
    /// may re-arm it (see [`Port::refresh_timeout`]).
    pub fn cancel_timeout(&mut self) -> bool {
        self.tasks.remove(PortTaskKind::Timeout)
    }

    /// The deadline the driver currently wants its `timeout` callback at, if any.
    ///
    /// Reads [`Driver::next_timeout`] directly; the port's `TIMEOUT` task mirrors this value
    /// after each driver interaction (see [`Port::refresh_timeout`]).
    pub fn next_timeout(&self) -> Option<Instant> {
        self.driver.next_timeout()
    }

    /// Re-arms (or clears) this port's `TIMEOUT` task from the driver's declared deadline.
    ///
    /// Called **after every driver interaction** (`start`/`output`/`ready_input`/
    /// `ready_output`/`control`/`timeout`), so the `TIMEOUT` task always mirrors
    /// [`Driver::next_timeout`]: a declared deadline arms (or re-points) the timer, and no
    /// declared deadline clears it. The deadline is a lower bound — an armed task whose
    /// deadline has not passed stays queued ([`PortTaskQueue::pop_next`]), so the driver's
    /// `timeout` never runs early.
    pub(crate) fn refresh_timeout(&mut self) {
        match self.driver.next_timeout() {
            Some(deadline) => self.tasks.arm_timeout(deadline),
            None => {
                self.tasks.remove(PortTaskKind::Timeout);
            }
        }
    }

    /// Whether the driver has armed a registration that wants readability.
    ///
    /// The reactor consults this before scheduling an [`PortTaskKind::Input`] task, so a
    /// direction the driver never registered cannot be served (see [`PortArms`]).
    pub(crate) fn read_armed(&self) -> bool {
        self.arms.read_armed()
    }

    /// Whether the driver has armed a registration that wants writability.
    ///
    /// The reactor consults this before scheduling an [`PortTaskKind::Output`] task (see
    /// [`PortArms`]).
    pub(crate) fn write_armed(&self) -> bool {
        self.arms.write_armed()
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
    /// `INPUT` runs the driver's `ready_input`; `OUTPUT` runs its `ready_output` (which the
    /// driver may define in terms of `output`); `TIMEOUT` runs its `timeout`.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver callback returns.
    pub(crate) fn run_task(&mut self, poller: &mut Poller, task: &PortTask) -> Result<usize> {
        match task.kind() {
            PortTaskKind::Input => {
                self.ready_input(poller)?;
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

    /// Whether this port is holding input bytes it owes its owner.
    ///
    /// The buffered input queue is the single source of truth for a
    /// [`PortNotification::Data`] report (see [`Port::take_input_notification`]): every byte a
    /// driver hands up — whether from an `INPUT` task's `ready_input` **or** from a
    /// `driver_output` inside `start`/`control`/`timeout`/`output` — lands in this queue, and the
    /// port-task round reports whatever is there.
    pub(crate) fn has_input(&self) -> bool {
        !self.input.is_empty()
    }

    /// Takes the [`PortNotification::Data`] this port owes, if any.
    ///
    /// The buffered input bytes are drained out of the input queue here, so a report happens
    /// **exactly once per byte**: after this returns `Some`, the queue is empty and the next
    /// call answers `None` until the driver hands up more input.
    ///
    /// The condition is the *queue*, not "an `INPUT` task ran": the legacy C delivers
    /// `driver_output` to the port owner as soon as the driver makes it, whatever callback it
    /// came from (`io.c`'s `driver_output`/`erts_port_output`), so a `driver_output` made from
    /// `start`/`control`/`timeout`/`output` — i.e. outside any `INPUT` task — must reach the
    /// owner on the next port-task round exactly like one made from `ready_input`. Gating the
    /// report on an `INPUT` task having run ([`Port::run_task`]) would strand those bytes in the
    /// queue, which is why the gate was removed.
    pub(crate) fn take_input_notification(&mut self) -> Option<PortNotification> {
        if !self.has_input() {
            return None;
        }
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
    /// The port's `TIMEOUT` task is re-armed from [`Driver::next_timeout`] afterwards (see
    /// [`Port::refresh_timeout`]).
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `ready_input` returns.
    pub fn ready_input(&mut self, poller: &mut Poller) -> Result<()> {
        let id = self.id;
        let result = {
            let Port {
                driver,
                input,
                output,
                arms,
                ..
            } = self;
            let mut ctx = DriverContext::new(id, poller, input, output, arms);
            driver.ready_input(&mut ctx)
        };
        self.refresh_timeout();
        result
    }

    /// Dispatches write readiness to the driver's `ready_output`.
    ///
    /// The port's `TIMEOUT` task is re-armed from [`Driver::next_timeout`] afterwards (see
    /// [`Port::refresh_timeout`]).
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `ready_output` returns.
    pub fn ready_output(&mut self, poller: &mut Poller) -> Result<usize> {
        let id = self.id;
        let result = {
            let Port {
                driver,
                input,
                output,
                arms,
                ..
            } = self;
            let mut ctx = DriverContext::new(id, poller, input, output, arms);
            driver.ready_output(&mut ctx)
        };
        self.refresh_timeout();
        result
    }

    /// Dispatches a port-timer expiry to the driver's `timeout`.
    ///
    /// The port's `TIMEOUT` task is then re-armed from [`Driver::next_timeout`] (see
    /// [`Port::refresh_timeout`]), so a driver re-arms its timer by declaring the next deadline
    /// — no pending timer is left behind if it declares none.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `timeout` returns.
    pub fn timeout(&mut self, poller: &mut Poller) -> Result<()> {
        let id = self.id;
        let result = {
            let Port {
                driver,
                input,
                output,
                arms,
                ..
            } = self;
            let mut ctx = DriverContext::new(id, poller, input, output, arms);
            driver.timeout(&mut ctx)
        };
        self.refresh_timeout();
        result
    }

    /// Delivers a port-control command to the driver and returns its reply bytes.
    ///
    /// The port's `TIMEOUT` task is re-armed from [`Driver::next_timeout`] afterwards, so a
    /// driver that changes its timer in response to a control command (e.g. an idle timeout)
    /// takes effect immediately (see [`Port::refresh_timeout`]).
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `control` returns.
    pub fn control(&mut self, poller: &mut Poller, command: &str, arg: &[u8]) -> Result<Vec<u8>> {
        let id = self.id;
        let result = {
            let Port {
                driver,
                input,
                output,
                arms,
                ..
            } = self;
            let mut ctx = DriverContext::new(id, poller, input, output, arms);
            driver.control(&mut ctx, command, arg)
        };
        self.refresh_timeout();
        result
    }

    /// Whether the driver has asynchronous work pending (a `driver_async` completion not yet handed
    /// over).
    ///
    /// This is the probe [`PortRegistry::execute_port_tasks`](crate::PortRegistry::execute_port_tasks)
    /// consults to decide whether the port needs a [`Port::drive_async`] point this round. It never
    /// touches the driver context, so it is safe to call while iterating the registry.
    pub fn has_pending_async(&self) -> bool {
        self.driver.has_pending_async()
    }

    /// Drives the driver's asynchronous completions, returning how many were processed.
    ///
    /// The port-task round has **no task kind** for `driver_async` completion in Milestone-1 (the
    /// C runs `ready_async` from `call_async_ready` on the port's scheduler), so the runtime calls
    /// this explicitly — mirroring every other driver interaction, the port's `TIMEOUT` task is
    /// re-armed from [`Driver::next_timeout`] afterwards (see [`Port::refresh_timeout`]).
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's [`Driver::drive_async`] returns.
    pub fn drive_async(&mut self, poller: &mut Poller) -> Result<usize> {
        let id = self.id;
        let result = {
            let Port {
                driver,
                input,
                output,
                arms,
                ..
            } = self;
            let mut ctx = DriverContext::new(id, poller, input, output, arms);
            driver.drive_async(&mut ctx)
        };
        self.refresh_timeout();
        result
    }

    /// Takes the reaped exit status of the OS child this port owns, once.
    ///
    /// Forwards to the driver's [`Driver::take_exit_status`]: a port whose driver starts no
    /// child (the built-in `fd`/`efile` drivers) always answers `Ok(None)`, and so does a
    /// port whose child has not been reaped yet or whose status has already been taken (see
    /// [`crate::drivers::SpawnDriver`]).
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `take_exit_status` returns.
    pub fn take_exit_status(&mut self) -> Result<Option<i32>> {
        self.driver.take_exit_status()
    }

    /// Takes the port's end-of-file notification, once.
    ///
    /// Forwards to the driver's [`Driver::take_eof`]: `Ok(true)` means the port's input stream
    /// reached end-of-file and the port asked for the `eof` option — the signal a runtime turns
    /// into the bare `eof` message the oracle delivers. `Ok(false)` means "nothing to report"
    /// (no `eof` option, no end-of-file yet, or already reported); a port whose driver never
    /// streams input always answers `Ok(false)`.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `take_eof` returns.
    pub fn take_eof(&mut self) -> Result<bool> {
        self.driver.take_eof()
    }

    /// Tells the driver this port is exiting, **at most once**.
    ///
    /// Forwards `reason` to [`Driver::port_exit`] — the driver's advisory exit-notification
    /// channel, distinct from the teardown [`Driver::stop`] — and records that it was
    /// delivered, so a repeated call is a no-op that never reaches the driver again. The port
    /// stays live and its registrations stay armed; a later [`Port::close`] then runs `stop`,
    /// so the two are independent and a `port_exit` before a `stop` is well-defined.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `port_exit` returns on the **first** call; a repeat never
    /// calls the driver (it returns `Ok(())`), so the first call's outcome is the only one
    /// observable.
    pub fn notify_exit(&mut self, poller: &mut Poller, reason: PortExitReason) -> Result<()> {
        if self.exit_notified {
            return Ok(());
        }
        // Record the delivery before calling the driver so a failed notification is still
        // delivered at most once (a repeat cannot re-run a driver callback with side effects).
        self.exit_notified = true;
        let id = self.id;
        let Port {
            driver,
            input,
            output,
            arms,
            ..
        } = self;
        let mut ctx = DriverContext::new(id, poller, input, output, arms);
        driver.port_exit(&mut ctx, reason)
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

/// The **reserved** port-control operation namespace.
///
/// [`PortRegistry::port_control`] forwards every operation name it is handed **verbatim** to
/// the port's driver: the driver's `control` vocabulary (`"line"`/`"noline"`/`"binary"`/
/// `"text"`/`"getopts"` for the built-in `fd` driver, plus the `efile` family's
/// `"open"`/`"close"`/`"path"`/`"eof"` with an attached fd — see [`crate::drivers`]) is
/// authoritative, exactly as in the C `erts_port_control`. The registry itself never decodes
/// that vocabulary; it reserves **one** name before any driver sees it:
///
/// | Name | Meaning here |
/// |---|---|
/// | [`PortControl::CLOSE`] (`"close"`) | close the port (driver `stop` + id release) |
///
/// Every other name — recognised by a driver or not — is passed through unchanged, so an
/// unrecognised verb surfaces the driver's own [`beam_common::BeamError::BadArg`] rather than
/// being silently ignored here.
pub struct PortControl;

impl PortControl {
    /// The reserved `"close"` operation: closes the port like [`PortRegistry::close`] and
    /// answers with no bytes.
    pub const CLOSE: &'static str = "close";
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
