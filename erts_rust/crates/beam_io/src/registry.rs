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

//! The [`PortRegistry`] — the set of live ports, the id allocator and the reactor glue.
//!
//! Ids come from the inner `beam_process::PortTable` (shared with `beam_process`, so `ptab`
//! stays the single id authority); the registry keys its live ports by that port **number**.
//! The [`Port`](crate::port::Port) itself — its lifecycle, its input/output byte queues and
//! its driver dispatch — lives in the sibling [`crate::port`] module.
//!
//! [`PortRegistry::handle_events`] is the reactor glue: it turns the poller's readiness
//! [`Event`](crate::poller::Event)s into **scheduled port tasks**
//! ([`PortTaskKind`](crate::port_task::PortTaskKind)) rather than calling a driver inline,
//! mirroring `erts_port_task_schedule`/`erts_port_task_execute`
//! (`erts/emulator/beam/erl_port_task.{c,h}`). [`PortRegistry::execute_port_tasks`] runs the
//! scheduled tasks under a reduction budget and reports what happened as a
//! [`PortNotification`](crate::port_task::PortNotification) list — the seam the runtime turns
//! into `{Port, {data, Data}}` messages. [`PortRegistry::port_command`] queues data and
//! schedules an `OUTPUT` task; [`PortRegistry::port_control`] forwards a named control
//! operation **verbatim** to the driver's `control` (as the C `erts_port_control` does),
//! reserving only `"close"` for itself.
//!
//! The legacy C analogue is the `erts_port` control block and its `ioq`
//! (`erts/emulator/beam/erl_port.h`, `io.c`). The *scheduler* wiring (port run queues, poll
//! threads, reductions charged to a scheduler) still belongs to `beam_process`/`beam_rt`.
//!
//! # Single-thread invariant (enforced)
//!
//! A registry is **not** thread-safe and this crate has no locking: it must be created and
//! driven from **one OS thread**, and concurrent registration or dispatch from several threads
//! is unsupported. The invariant is enforced by the type system rather than by documentation
//! alone: the [`PhantomData<core::cell::Cell<()>>`](core::marker::PhantomData) marker on
//! [`PortRegistry`] makes it neither `Send` nor `Sync`, so it cannot be moved into or shared
//! with another thread (pinned by the compile-fail doctests on [`PortRegistry`] below).

use std::cell::Cell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::time::Instant;

use beam_common::Result;
use beam_process::{PortId, PortTable};

use crate::driver::{Driver, DriverRegistry, PortExitReason};
use crate::error;
use crate::poller::{Event, Poller};
use crate::port::{Port, PortCommandOptions, PortControl, PortState, DEFAULT_READINESS_RED_BUDGET};
use crate::port_task::{PortNotification, PortTaskKind, PortTaskOutcome, Readiness};

/// The set of live ports, keyed by port number, plus the id allocator.
///
/// Ids come from the inner [`PortTable`] (shared with `beam_process`, so `ptab` stays the
/// single id authority); the `ports` map is keyed by `id.number()`.
///
/// A `PortRegistry` is **not** `Send` or `Sync` (see the module docs, "Single-thread
/// invariant"): it must be driven from one OS thread, and the [`PhantomData<Cell<()>>`]
/// marker below makes that a compile-time property rather than a comment. Both properties are
/// pinned by the compile-fail doctests below, so making the registry thread-safe by accident
/// breaks the build.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<beam_io::PortRegistry>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<beam_io::PortRegistry>();
/// ```
///
/// [`PhantomData<Cell<()>>`]: core::marker::PhantomData
pub struct PortRegistry {
    /// The port-number allocator (the single id authority).
    ids: PortTable,
    /// Live ports, keyed by port number.
    ports: HashMap<u64, Port>,
    /// Ports closed since the last execution round, reported as
    /// [`PortNotification::Closed`].
    ///
    /// A port's number stays **allocated** in `ids` while it sits here: the release happens when
    /// the close is actually reported (see [`PortRegistry::take_closed_notifications`]), so a
    /// deferred report can never name a number a later [`PortRegistry::open`] reused.
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

    /// Writes `data` to the given port's output queue and asks the port's driver to write it
    /// **inline**.
    ///
    /// A write the driver cannot finish leaves the remainder queued **and schedules an
    /// [`PortTaskKind::Output`] task**, so the next port-task round re-drives it even when the
    /// driver did not re-arm write readiness (see [`Port::output`]).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not a live port, or whatever the port's
    /// driver returns — including the typed accounting diagnostic a driver produces by
    /// reporting a byte count it did not consume (see [`Port::output`]).
    pub fn output(&mut self, poller: &mut Poller, id: PortId, data: &[u8]) -> Result<usize> {
        match self.ports.get_mut(&id.number()) {
            Some(port) => port.output(poller, data),
            None => Err(error::badarg()),
        }
    }

    /// Schedules the port tasks a batch of poller events implies, runs what it scheduled, and
    /// reports the round's outcome.
    ///
    /// Each event's key is a port number; an event for an unknown key (including the waker's
    /// [`WAKER_KEY`](crate::poller::WAKER_KEY)) is a no-op. Readiness is turned into
    /// scheduled port tasks rather than into direct driver calls (see
    /// [`PortRegistry::schedule_events`]), and the scheduled tasks are then executed under
    /// [`DEFAULT_READINESS_RED_BUDGET`] so a caller that does not drive the queue itself
    /// still sees ports make progress. A caller that wants full control over the reduction
    /// budget schedules with [`PortRegistry::schedule_events`] and runs the queue through
    /// [`PortRegistry::execute_port_tasks`].
    ///
    /// The two entries differ in what they report. `handle_events` **harvests no input**: the
    /// bytes an `INPUT` task read stay in the port's input queue for the caller to take with
    /// [`Port::drain_input`]/[`Port::read_input`], so
    /// [`PortTaskOutcome::notifications`] carries the [`PortNotification::Closed`] reports of
    /// the ports closed since the previous round — drained, and their numbers retired, through
    /// the same helper [`PortRegistry::execute_port_tasks`] uses — and **no**
    /// [`PortNotification::Data`]. A caller looping on `handle_events` alone therefore sees every
    /// close exactly once instead of accumulating them. A caller that wants the
    /// `{Port, {data, Data}}` payloads must use [`PortRegistry::execute_port_tasks`]; the count of
    /// events that named a live port is not carried by the outcome (a caller that needs it
    /// compares [`PortRegistry::schedule_events`]'s return value with its own batch).
    ///
    /// # Errors
    ///
    /// Returns any error raised by a port's driver while the scheduled tasks run; the closes
    /// still pending at that point stay queued (they are reported, and their numbers retired, by
    /// the next round) rather than being dropped.
    pub fn handle_events(
        &mut self,
        poller: &mut Poller,
        events: &[Event],
    ) -> Result<PortTaskOutcome> {
        self.schedule_events(events);
        let (ran, reds_used) = self.run_scheduled_tasks(poller, DEFAULT_READINESS_RED_BUDGET)?;
        Ok(PortTaskOutcome {
            ran,
            reds_used,
            notifications: self.take_closed_notifications(),
        })
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
    /// [`Readiness::write_ready`] ⇒ a [`PortTaskKind::Output`] task, each applied only when
    /// the driver has **armed** that direction with the poller (`Port::read_armed`/
    /// `Port::write_armed`). A pure EOF/`EPOLLHUP` event (`read_closed` with
    /// `readable == false`) therefore still reaches the driver's `ready_input` — which is
    /// where the end-of-file latch lives — while a write-closed flag on a port that never
    /// armed a write fd schedules nothing (the kernel reports `EPOLLHUP` as *both* closed
    /// edges, so a full close of a read-only port must not be mistaken for write readiness).
    ///
    /// A poll **error** reaches both armed directions, matching the C: the legacy reactor
    /// re-triggers *every* selected event on `ERTS_POLL_EV_ERR` (`EPOLLERR | EPOLLHUP`,
    /// `erl_poll.h:184`; `revents = state->active_events`, `erl_check_io.c:2018-2027`), so
    /// [`Readiness::write_ready`] counts `error` too and a fully armed port gets an `Output`
    /// task as well as an `Input` task. The armed-direction gate above is unchanged, so the
    /// error bit cannot schedule a direction the driver never armed.
    pub fn schedule_readiness(&mut self, key: u64, readiness: Readiness) -> bool {
        let Some(port) = self.ports.get_mut(&key) else {
            return false;
        };
        if readiness.read_ready() && port.read_armed() {
            port.schedule_task(PortTaskKind::Input);
        }
        if readiness.write_ready() && port.write_armed() {
            port.schedule_task(PortTaskKind::Output);
        }
        true
    }

    /// Executes the scheduled port tasks under `reds_budget` and reports what happened.
    ///
    /// The round runs the queued tasks in kind-priority order (`INPUT` → `OUTPUT` →
    /// `TIMEOUT`), stopping as soon as `reds_budget` is exhausted and leaving the rest
    /// queued; a [`PortTaskKind::Timeout`] task whose deadline has not passed is not run.
    /// Afterwards every port holding buffered input has it drained into a
    /// [`PortNotification::Data`] — or, for a port in `{line, N}` mode
    /// ([`PortRegistry::set_line_mode`]), into one [`PortNotification::DataLine`] per line
    /// frame the read produced — and every port closed since the previous round is
    /// reported as a [`PortNotification::Closed`].
    ///
    /// # Errors
    ///
    /// Returns any error raised by a port's driver while a task runs; the tasks after it
    /// stay queued. A close pending at that point is **not** lost: the closes are drained only
    /// after every fallible step of the round has succeeded, so an error leaves them queued for
    /// the next round (and keeps their numbers allocated meanwhile — see
    /// [`PortRegistry::take_closed_notifications`]).
    pub fn execute_port_tasks(
        &mut self,
        poller: &mut Poller,
        reds_budget: usize,
    ) -> Result<PortTaskOutcome> {
        let (ran, reds_used) = self.run_scheduled_tasks(poller, reds_budget)?;

        // Drive async completions **after** the scheduled tasks and **before** harvesting input.
        // `driver_async` has no port-task kind in M1, so a finished job only reaches the driver's
        // `ready_async`/`async_free` at a drive point; doing it here (rather than leaving it to an
        // explicit `PortRegistry::drive_async` call) lets a `ready_async` that calls `driver_output`
        // surface its bytes as a `Data` notification in this same round — the property the runtime
        // needs to make `driver_async` work end-to-end. The probe is `Driver::has_pending_async`, so
        // a port with no outstanding job pays nothing. Keys are snapshotted first so the drive does
        // not borrow the port map while it is being mutated.
        for key in self.sorted_keys() {
            let pending = self
                .ports
                .get(&key)
                .is_some_and(crate::port::Port::has_pending_async);
            if !pending {
                continue;
            }
            if let Some(port) = self.ports.get_mut(&key) {
                port.drive_async(poller)?;
            }
        }

        // Collect the closes only now that the round's fallible steps are behind us: an early
        // return above leaves them queued for the next round instead of dropping them, because
        // nothing has drained `self.closed` yet. Minting a report also retires the closed port's
        // number (see `take_closed_notifications`).
        let mut notifications = self.take_closed_notifications();

        // Harvest the data the INPUT tasks read: a port task never hands bytes up itself,
        // so the round reports them for the runtime to turn into `{Port, {data, Data}}`. A
        // port in line mode (`PortRegistry::set_line_mode`) reports **every** frame the read
        // produced — one read can yield many `{data, {eol|noeol, Data}}` messages — so the
        // notifications are extended, never replaced.
        let keys = self.sorted_keys();
        for key in keys {
            if let Some(port) = self.ports.get_mut(&key) {
                notifications.extend(port.take_input_notifications());
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
    /// The operation name `op` is forwarded **verbatim** to the driver's `control` — the
    /// driver owns the vocabulary (see [`PortControl`]) — and its reply bytes are returned.
    /// The single exception is the reserved [`PortControl::CLOSE`] (`"close"`), which closes
    /// the port like [`PortRegistry::close`] and answers with no bytes.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not a live port, or whatever the driver's
    /// `control`/`stop` returns; an unrecognised operation is the *driver's*
    /// `BeamError::BadArg`, not a silent no-op.
    pub fn port_control(
        &mut self,
        poller: &mut Poller,
        id: PortId,
        op: &str,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        if op == PortControl::CLOSE {
            self.close(poller, id)?;
            return Ok(Vec::new());
        }
        match self.ports.get_mut(&id.number()) {
            Some(port) => port.control(poller, op, arg),
            None => Err(error::badarg()),
        }
    }

    /// Drives port `id`'s asynchronous driver completions, returning how many were processed.
    ///
    /// A `driver_async` job finishes on a worker thread; the runtime then hands it to the driver's
    /// `ready_async` (or runs its `async_free`). There is no readiness- or reduction-driven port
    /// task for that hand-off in Milestone-1 — the C does it from `call_async_ready` on the port's
    /// scheduler — so the runtime calls this from its main loop, once per port-task round, to let
    /// a port with pending jobs make progress (see [`Driver::drive_async`]).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not a live port, or whatever the port's driver
    /// returns from [`Driver::drive_async`].
    pub fn drive_async(&mut self, poller: &mut Poller, id: PortId) -> Result<usize> {
        match self.ports.get_mut(&id.number()) {
            Some(port) => port.drive_async(poller),
            None => Err(error::badarg()),
        }
    }

    /// Takes the reaped exit status of the OS child behind port `id`, once.
    ///
    /// This is the seam a runtime uses to synthesize the `{Port, {exit_status, Status}}`
    /// message of a port opened with the `exit_status` option: `Ok(None)` means "no child",
    /// "not reaped yet" or "already reported", so a caller polls it and reports the status
    /// exactly once (see [`crate::drivers::SpawnDriver::take_exit_status`]).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not a live port, or whatever the port's
    /// driver returns.
    pub fn take_exit_status(&mut self, id: PortId) -> Result<Option<i32>> {
        match self.ports.get_mut(&id.number()) {
            Some(port) => port.take_exit_status(),
            None => Err(error::badarg()),
        }
    }

    /// Takes port `id`'s end-of-file notification, once, and flushes its line buffer.
    ///
    /// This is the seam a runtime uses to synthesize the bare `eof` message of a port opened
    /// with the `eof` option: `Ok(true)` is reported exactly once, when the port's input stream
    /// reached end of file; `Ok(false)` means "not asked for", "not reached yet" or "already
    /// reported" (see [`crate::drivers::SpawnDriver`]).
    ///
    /// It is also the crate's **driver-end-of-file observation point** (the C's `driver_eof`),
    /// so a reported end-of-file releases a `{line, N}` port's pending partial line as a
    /// `noeol` frame for the next round's harvest (see [`PortRegistry::set_line_mode`] and
    /// [`Port::flush_linebuf`]). A caller that knows the stream has ended but has no `eof`
    /// option to report may call [`PortRegistry::flush_linebuf`] directly.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not a live port, or whatever the port's driver
    /// returns.
    pub fn take_eof(&mut self, id: PortId) -> Result<bool> {
        match self.ports.get_mut(&id.number()) {
            Some(port) => port.take_eof(),
            None => Err(error::badarg()),
        }
    }

    /// Enables or disables **line framing** (`{line, N}`) on port `id`'s input.
    ///
    /// `Some(max_len)` splits the bytes the port's driver hands up into
    /// [`PortNotification::DataLine`] frames of at most `max_len` bytes (`0` = no length
    /// limit); `None` (the default) delivers them raw as [`PortNotification::Data`]. This is
    /// the runtime's adoption point for `open_port(..., [line])` / `[binary, {line, N}]`
    /// (`ERTS_PORT_SFLG_LINEBUF_IO`, `erl_bif_port.c:862-869,986`) — see
    /// [`Port::set_line_mode`].
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not a live port.
    pub fn set_line_mode(&mut self, id: PortId, max_len: Option<usize>) -> Result<()> {
        match self.ports.get_mut(&id.number()) {
            Some(port) => {
                port.set_line_mode(max_len);
                Ok(())
            }
            None => Err(error::badarg()),
        }
    }

    /// Releases port `id`'s pending partial line as `noeol` frame(s) for the next harvest.
    ///
    /// This is the C's `flush_linebuf_messages`: it is what [`PortRegistry::take_eof`] runs when
    /// the driver reports the end of the stream, exposed separately for a caller that knows the
    /// stream has ended without an `eof` option to report. A port in raw mode is untouched, and
    /// a repeat call stages nothing (see [`Port::flush_linebuf`]). Closing a port
    /// ([`PortRegistry::close`]) never flushes.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not a live port.
    pub fn flush_linebuf(&mut self, id: PortId) -> Result<()> {
        match self.ports.get_mut(&id.number()) {
            Some(port) => {
                port.flush_linebuf();
                Ok(())
            }
            None => Err(error::badarg()),
        }
    }

    /// Arms port `id`'s `TIMEOUT` task at `deadline`, replacing any deadline already armed.
    ///
    /// This is the runtime's explicit timer entry point and the driver-facing timer seam's
    /// registry surface ([`Driver::next_timeout`] is the driver's own declaration, mirrored by
    /// [`Port::refresh_timeout`]). The driver's `timeout` callback runs when the next task
    /// round at or after `deadline` executes the timer — never before it, because
    /// [`PortTaskQueue::pop_next`](crate::port_task::PortTaskQueue::pop_next) withholds a
    /// not-yet-due `TIMEOUT` task. The next driver interaction re-derives the deadline from the
    /// driver, so an explicit arm survives only until the driver declares something different.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not a live port.
    pub fn schedule_timeout(&mut self, id: PortId, deadline: Instant) -> Result<()> {
        match self.ports.get_mut(&id.number()) {
            Some(port) => {
                port.arm_timeout(deadline);
                Ok(())
            }
            None => Err(error::badarg()),
        }
    }

    /// Drops port `id`'s pending `TIMEOUT` task, reporting whether one was pending.
    ///
    /// The next driver interaction re-derives the timer from [`Driver::next_timeout`] (see
    /// [`Port::refresh_timeout`]).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not a live port.
    pub fn cancel_timeout(&mut self, id: PortId) -> Result<bool> {
        match self.ports.get_mut(&id.number()) {
            Some(port) => Ok(port.cancel_timeout()),
            None => Err(error::badarg()),
        }
    }

    /// The earliest deadline any live port's driver currently declares, if any.
    ///
    /// The runtime passes this to the poller's timeout (as an upper bound on how long it may
    /// block) so a port timer fires even when no descriptor becomes ready; `None` means no
    /// live driver declares a deadline (the runtime may then block indefinitely).
    pub fn next_deadline(&self) -> Option<Instant> {
        self.ports.values().filter_map(Port::next_timeout).min()
    }

    /// Notifies port `id`'s driver that the port is exiting, with `reason`.
    ///
    /// The runtime calls this on its port-exit path (its owner exited, or it is being closed)
    /// **before** [`PortRegistry::close`], so the driver can observe the exit through
    /// [`Driver::port_exit`] — a notification distinct from the teardown `stop` that `close`
    /// runs. The notification is delivered **at most once** per port: a repeated call is a
    /// no-op. The port stays live, so a `notify_port_exit` followed by a later
    /// [`PortRegistry::close`] is well-defined.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not a live port, or whatever the port's
    /// driver returns from `port_exit` on the first call.
    pub fn notify_port_exit(
        &mut self,
        poller: &mut Poller,
        id: PortId,
        reason: PortExitReason,
    ) -> Result<()> {
        match self.ports.get_mut(&id.number()) {
            Some(port) => port.notify_exit(poller, reason),
            None => Err(error::badarg()),
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

    /// Stops and removes the port with id `id`, queueing its number for retirement.
    ///
    /// The close is reported as a [`PortNotification::Closed`] by the next round — either
    /// [`PortRegistry::execute_port_tasks`] or [`PortRegistry::handle_events`] — and the port
    /// **number is released at that point, not here**. Keeping it allocated until the report is
    /// minted is what makes a `Closed` notification trustworthy: `beam_process::PortTable` hands
    /// a released number straight back out to the next [`PortRegistry::open`], so releasing it
    /// here would let a `close` + `open` pair make the deferred report name a *different, live*
    /// port. A caller that closes a port and never runs another round therefore keeps the number
    /// allocated — the round is what retires it.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not a live port, or whatever the driver's
    /// `stop` returns. The port is removed and its close queued either way, so a teardown
    /// failure neither leaves the port looking live nor leaks its number.
    pub fn close(&mut self, poller: &mut Poller, id: PortId) -> Result<()> {
        let Some(mut port) = self.ports.remove(&id.number()) else {
            return Err(error::badarg());
        };
        let result = port.close(poller);
        // The port is gone from the registry either way, so its close is queued (and its number
        // retired by the reporting round) even if the driver's teardown reports a failure.
        self.closed.push(id);
        result
    }

    /// Drains the closes pending since the last round into [`PortNotification::Closed`] values,
    /// **retiring each closed port's number as its report is minted**.
    ///
    /// This is the single place a closed port's number is handed back to the id table (see
    /// [`PortRegistry::close`]), which is why it is shared by both round drivers — a caller
    /// looping on either one drains the queue, so closes are reported exactly once and
    /// `self.closed` cannot grow without bound.
    ///
    /// Minting the report is what ends the number's reservation: a port opened between the
    /// `close` and this call gets a **different** number, so a
    /// [`PortNotification::Closed`] never names a reused, live port. Once the report is handed
    /// over, the number is free again — a consumer must act on the notification in the round
    /// that delivered it rather than storing the bare number for later.
    fn take_closed_notifications(&mut self) -> Vec<PortNotification> {
        let closed = std::mem::take(&mut self.closed);
        closed
            .into_iter()
            .map(|port| {
                self.ids.release(port);
                PortNotification::Closed { port }
            })
            .collect()
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

    /// The registry's port-number table, mutably.
    ///
    /// This is the **same** [`PortTable`] instance [`PortRegistry::open`] allocates from and
    /// [`PortRegistry::close`] releases through, so a relationship recorded on it (an owner, a
    /// link, a monitor) is the one the registry's own operations observe, and vice versa.
    ///
    /// It is the seam a host uses to drive the [`beam_process::ProcessTable`] port-monitor API —
    /// [`monitor_port`](beam_process::ProcessTable::monitor_port) /
    /// [`monitor_port_term`](beam_process::ProcessTable::monitor_port_term) /
    /// [`demonitor_port`](beam_process::ProcessTable::demonitor_port) — for `erlang:monitor/2`
    /// on a **port** target. Those take a `&mut PortTable` alongside the process table's
    /// `&mut self`, and the ports they touch are the registry's live ports, so the table they
    /// mutate must be this one.
    ///
    /// # Borrowing rule
    ///
    /// The returned `&mut PortTable` **borrows the registry**. A host that holds the registry
    /// behind a `RefCell` (as `beam_rt` does: `Rc<RefCell<PortRegistry>>`, reached through
    /// `RuntimePorts::registry_mut` → `try_borrow_mut`) must **scope this borrow so it is
    /// dropped before any nested borrow of the same registry**: acquire the registry's
    /// `RefMut`, take the `&mut PortTable`, run the single monitor/demonitor call, and drop the
    /// whole chain in the same statement block:
    ///
    /// ```text
    /// let mut registry = runtime_ports.registry_mut()?;   // borrow the RefCell once
    /// let ports = registry.port_table_mut();              // borrow the registry
    /// let reference = table.monitor_port(watcher, port, ports)?;   // the one call
    /// // `ports` and `registry` are both dropped here, before the next statement.
    /// ```
    ///
    /// Never hold the `&mut PortTable` across a call that **re-enters** the registry (a
    /// `ports/0`, `port_info/1`, `port_command/3`, `ets:*`, … driven through the same shared
    /// cell): that is a nested `RefCell` borrow, rejected by `beam_rt`'s `registry_mut` as a
    /// typed capability gap — `error::unsupported("the port table is already in use")` — or, for
    /// a bare `borrow_mut()`, a panic.
    ///
    /// The `&mut ProcessTable` side of `monitor_port`/`demonitor_port` carries the **same** rule,
    /// because the process table is likewise shared behind a cell. When one monitor or demonitor
    /// needs **both** tables, take both borrows together, run the call, and release both together
    /// so nothing re-enters either cell mid-call.
    pub fn port_table_mut(&mut self) -> &mut PortTable {
        &mut self.ids
    }

    /// The registry's port-number table, by shared reference — the same instance
    /// [`PortRegistry::port_table_mut`] hands out.
    ///
    /// A `&PortTable` needs only a shared borrow of the registry, so a caller holding the
    /// registry behind a `RefCell` can reach it through an ordinary `borrow()`. All the
    /// borrowing rules documented on [`PortRegistry::port_table_mut`] still apply to any
    /// **nested** registry access made while the shared borrow is live.
    pub fn port_table(&self) -> &PortTable {
        &self.ids
    }
}

#[cfg(test)]
mod tests {
    use super::PortRegistry;
    use crate::driver::{Driver, DriverContext, DriverRegistry};
    use crate::drivers::FdDriver;
    use crate::error;
    use crate::poller::{Event, Interest, Poller, Ready, Token, WAKER_KEY};
    use crate::port::{PortCommandOptions, PortControl, PortState, DEFAULT_READINESS_RED_BUDGET};
    use crate::port_task::{
        PortNotification, PortTaskKind, Readiness, PORT_REDS_INPUT, PORT_REDS_TIMEOUT,
    };
    use beam_common::{BeamError, Result};
    use beam_process::{PortId, PortWatch, ProcessTable, SignalAtoms};
    use beam_term::prelude::AtomTable;
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

    /// A closed port's number is **not** reused until its close has been reported, so a
    /// deferred [`PortNotification::Closed`] can never name a freshly opened (live) port.
    #[test]
    fn a_closed_port_number_is_retired_only_when_its_close_is_reported() {
        let mut poller = poller();
        let (mut registry, first) = registry_with_pipe(&mut poller);
        let second = registry
            .open(&mut poller, Box::new(FdDriver::pipe().expect("a pipe")))
            .expect("a second port");
        assert_ne!(first, second);

        registry.close(&mut poller, first).expect("closes");
        assert_eq!(registry.len(), 1);

        // The number is still reserved, so the next open cannot take it: the close has not been
        // reported yet and the deferred report must not name a live port.
        let third = registry
            .open(&mut poller, Box::new(FdDriver::pipe().expect("a pipe")))
            .expect("a third port");
        assert_ne!(third, first, "an unreported close keeps its number reserved");
        assert_eq!(registry.len(), 2);

        // The reporting round hands the report over **and** retires the number with it.
        let round = registry
            .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("runs");
        assert_eq!(
            round.notifications,
            vec![PortNotification::Closed { port: first }],
            "the close names the port that was closed"
        );
        assert!(
            !registry.contains(first),
            "the reported port is not a live port"
        );
        assert!(!registry.port_table().contains(first), "its number is retired");

        // Retirement happens exactly once, with the report: the number is handed out again to the
        // next open, and no second close report appears.
        let reused = registry
            .open(&mut poller, Box::new(FdDriver::pipe().expect("a pipe")))
            .expect("a fourth port");
        assert_eq!(reused, first, "the retired number is reusable");
        let later = registry
            .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("runs");
        assert!(
            later.notifications.is_empty(),
            "the close was reported exactly once"
        );
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
    fn the_built_in_efile_driver_opens_and_closes() {
        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let drivers = DriverRegistry::with_builtins();
        let id = registry
            .open_named(&mut poller, &drivers, "efile")
            .expect("the efile driver is built in");
        assert_eq!(registry.get(id).expect("live").driver_name(), "efile");
        // An unopened `efile` port owns no fds, so `start` registers nothing with the poller.
        assert!(poller.is_empty());
        registry.close(&mut poller, id).expect("closes cleanly");
        assert!(!registry.contains(id));
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
        // are skipped without touching any driver — and, with no port task queued, the round
        // runs nothing and reports nothing.
        let events = vec![
            Event::new(token, 4242, Ready::default()),
            Event::new(token, WAKER_KEY, Ready::default()),
        ];
        let outcome = registry.handle_events(&mut poller, &events).expect("ok");
        assert_eq!(outcome.ran, 0, "an unknown key schedules no task");
        assert_eq!(outcome.reds_used, 0);
        assert!(
            outcome.notifications.is_empty(),
            "an unknown key reports nothing"
        );
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

    // --- `{line, N}` line framing --------------------------------------------

    /// A `{data, {eol|noeol, Bytes}}` notification for `port`.
    fn line(port: PortId, bytes: &[u8], eol: bool) -> PortNotification {
        PortNotification::DataLine {
            port,
            bytes: bytes.to_vec(),
            eol,
        }
    }

    /// A driver that banks a fixed payload on its first `ready_input` and reports the end of
    /// its input stream through the once-only `take_eof` seam.
    ///
    /// It arms no descriptor, so the line-framing tests drive its task queue directly
    /// ([`Port::schedule_task`]) instead of through `schedule_readiness` — the line framing
    /// itself never looks at the poller.
    struct LineEofDriver {
        payload: Vec<u8>,
        fed: bool,
        eof_reported: bool,
    }

    impl LineEofDriver {
        fn new(payload: &[u8]) -> LineEofDriver {
            LineEofDriver {
                payload: payload.to_vec(),
                fed: false,
                eof_reported: false,
            }
        }
    }

    impl Driver for LineEofDriver {
        fn name(&self) -> &str {
            "line-eof-test"
        }

        fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            Ok(())
        }

        fn output(&mut self, _ctx: &mut DriverContext<'_>) -> Result<usize> {
            Ok(0)
        }

        fn ready_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
            if !self.fed {
                self.fed = true;
                let payload = self.payload.clone();
                ctx.input().push(&payload);
            }
            Ok(())
        }

        fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            Ok(())
        }

        fn take_eof(&mut self) -> Result<bool> {
            if self.eof_reported {
                Ok(false)
            } else {
                self.eof_reported = true;
                Ok(true)
            }
        }
    }

    /// Opens a port around `driver` in a fresh registry.
    fn open_with(poller: &mut Poller, driver: Box<dyn Driver>) -> (PortRegistry, PortId) {
        let mut registry = PortRegistry::new();
        let id = registry.open(poller, driver).expect("the port starts");
        (registry, id)
    }

    /// Runs one port-task round and returns the notifications it produced.
    fn round(poller: &mut Poller, registry: &mut PortRegistry) -> Vec<PortNotification> {
        registry
            .execute_port_tasks(poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("the round runs")
            .notifications
    }

    /// A pipe port in `{line, N}` mode with `bytes` written to its read end.
    fn line_pipe(poller: &mut Poller, bytes: &[u8], max_len: usize) -> (PortRegistry, PortId) {
        let (mut registry, id) = registry_with_pipe(poller);
        registry
            .set_line_mode(id, Some(max_len))
            .expect("the port is live");
        assert_eq!(registry.output(poller, id, bytes).expect("the write"), bytes.len());
        (registry, id)
    }

    #[test]
    fn line_mode_reports_one_notification_per_line() {
        let mut poller = poller();
        let (mut registry, id) = line_pipe(&mut poller, b"a\nb\nc\n", 1024);

        // One read, three lines: the round reports **three** notifications, not one blob.
        assert!(registry.schedule_readiness(
            id.number(),
            Readiness {
                readable: true,
                ..Readiness::default()
            }
        ));
        assert_eq!(
            round(&mut poller, &mut registry),
            vec![
                line(id, b"a", true),
                line(id, b"b", true),
                line(id, b"c", true),
            ]
        );
        // A second round has nothing left to report (the frames were drained).
        assert!(round(&mut poller, &mut registry).is_empty());
    }

    #[test]
    fn line_mode_reports_a_blank_line_as_an_empty_eol_frame() {
        // The oracle delivers `{Port, {data, {eol, []}}}` for a lone newline: the empty frame
        // must not be dropped (the raw path drops an empty payload).
        let mut poller = poller();
        let (mut registry, id) = line_pipe(&mut poller, b"\n", 1024);
        assert!(registry.schedule_readiness(
            id.number(),
            Readiness {
                readable: true,
                ..Readiness::default()
            }
        ));
        assert_eq!(round(&mut poller, &mut registry), vec![line(id, b"", true)]);
    }

    #[test]
    fn line_mode_splits_an_over_long_line_at_the_limit() {
        let mut poller = poller();
        let (mut registry, id) = line_pipe(&mut poller, b"abcdefgh\n", 4);
        assert!(registry.schedule_readiness(
            id.number(),
            Readiness {
                readable: true,
                ..Readiness::default()
            }
        ));
        assert_eq!(
            round(&mut poller, &mut registry),
            vec![
                line(id, b"abcd", false),
                line(id, b"efgh", true),
            ],
            "the length-split chunk is noeol, the newline-terminated one eol"
        );
    }

    #[test]
    fn one_read_yields_every_line_frame_it_contains() {
        // A single read can carry many messages: 597 `a`s and a newline at a `{line,4}` limit is
        // 149 `noeol` chunks plus one newline-terminated `eol` — **150** notifications from one
        // round, which is why the round harvests a `Vec` per port rather than a single one.
        let mut payload = vec![b'a'; 597];
        payload.push(b'\n');
        let mut poller = poller();
        let (mut registry, id) = line_pipe(&mut poller, &payload, 4);
        assert!(registry.schedule_readiness(
            id.number(),
            Readiness {
                readable: true,
                ..Readiness::default()
            }
        ));
        let frames = round(&mut poller, &mut registry);
        assert_eq!(frames.len(), 150, "one read, 150 frames");
        assert_eq!(
            frames
                .iter()
                .filter(|notification| matches!(
                    notification,
                    PortNotification::DataLine { eol: true, .. }
                ))
                .count(),
            1,
            "exactly the newline-terminated frame is `eol` ({frames:?})"
        );
        assert_eq!(frames[0], line(id, &[b'a'; 4], false), "every chunk is `noeol`");
        assert_eq!(
            frames.last(),
            Some(&line(id, b"a", true)),
            "the tail before the newline is the one `eol` frame"
        );
    }

    #[test]
    fn line_mode_is_off_by_default_and_delivers_raw_bytes() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);
        assert_eq!(registry.get(id).expect("live").line_mode(), None);
        assert_eq!(
            registry.output(&mut poller, id, b"a\nb\n").expect("writes"),
            4
        );
        assert!(registry.schedule_readiness(
            id.number(),
            Readiness {
                readable: true,
                ..Readiness::default()
            }
        ));
        assert_eq!(
            round(&mut poller, &mut registry),
            vec![PortNotification::Data {
                port: id,
                bytes: b"a\nb\n".to_vec()
            }],
            "with line mode off the whole read crosses as one Data notification"
        );
    }

    #[test]
    fn set_line_mode_and_flush_linebuf_reject_an_unknown_port() {
        let mut registry = PortRegistry::new();
        let missing = PortId::new(77);
        assert!(matches!(
            registry.set_line_mode(missing, Some(4)),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            registry.flush_linebuf(missing),
            Err(BeamError::BadArg)
        ));
    }

    /// The partial-line flush is driven by **end of file**, and only by it: a `{line, N}` port
    /// holds a line with no terminator, and `flush_linebuf`/`take_eof` release it as `noeol`
    /// (the C's `flush_linebuf_messages`, run from `driver_eof`). A plain `close` does not.
    #[test]
    fn a_partial_line_is_held_until_either_the_flush_or_the_close() {
        let mut poller = poller();
        let (mut registry, id) = line_pipe(&mut poller, b"a\nb", 1024);
        assert!(registry.schedule_readiness(
            id.number(),
            Readiness {
                readable: true,
                ..Readiness::default()
            }
        ));
        assert_eq!(
            round(&mut poller, &mut registry),
            vec![line(id, b"a", true)],
            "the partial line is held back"
        );

        // The explicit flush (what `take_eof` runs) releases it, once, as `noeol`.
        registry.flush_linebuf(id).expect("the port is live");
        assert_eq!(round(&mut poller, &mut registry), vec![line(id, b"b", false)]);
        assert!(
            round(&mut poller, &mut registry).is_empty(),
            "the flush is not repeated"
        );

        // A second partial line, released by `close` this time: closing flushes nothing.
        assert_eq!(registry.output(&mut poller, id, b"c\nd").expect("writes"), 3);
        assert!(registry.schedule_readiness(
            id.number(),
            Readiness {
                readable: true,
                ..Readiness::default()
            }
        ));
        assert_eq!(round(&mut poller, &mut registry), vec![line(id, b"c", true)]);
        registry.close(&mut poller, id).expect("closes");
        assert_eq!(
            round(&mut poller, &mut registry),
            vec![PortNotification::Closed { port: id }],
            "a close reports the close and never a flushed frame"
        );
    }

    /// `take_eof` is the crate's driver-end-of-file observation point, so a reported
    /// end-of-file releases the pending partial line for the next round's harvest — the C's
    /// `driver_eof` → `flush_linebuf_messages` (`io.c:7211`).
    #[test]
    fn take_eof_flushes_the_pending_partial_line() {
        let mut poller = poller();
        let (mut registry, id) = open_with(&mut poller, Box::new(LineEofDriver::new(b"a\nb")));
        registry
            .set_line_mode(id, Some(1024))
            .expect("the port is live");

        // Feed the payload: the complete line is reported, the terminator-less tail is held.
        assert!(registry
            .get_mut(id)
            .expect("live")
            .schedule_task(PortTaskKind::Input));
        assert_eq!(
            round(&mut poller, &mut registry),
            vec![line(id, b"a", true)]
        );

        // Observing the end of the stream stages the flush…
        assert!(
            registry.take_eof(id).expect("the check succeeds"),
            "the driver reports the end of the stream"
        );
        // …which the next round reports as a `noeol` frame; the `eof` report is once-only.
        assert_eq!(round(&mut poller, &mut registry), vec![line(id, b"b", false)]);
        assert!(!registry.take_eof(id).expect("the check succeeds"));
        assert!(round(&mut poller, &mut registry).is_empty());
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
        assert!(
            registry.closed.is_empty(),
            "the reporting round drained the close queue"
        );

        let second = registry
            .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("runs");
        assert!(
            second.notifications.is_empty(),
            "a close is reported exactly once"
        );
    }

    /// `handle_events` routes the close draining through the same helper `execute_port_tasks`
    /// uses, so a caller looping on it alone sees every close exactly once (and the pending-close
    /// queue cannot grow without bound).
    #[test]
    fn handle_events_reports_and_retires_the_closes_it_drains() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);
        registry.close(&mut poller, id).expect("closes");
        assert_eq!(registry.closed.len(), 1, "the close is pending");

        let outcome = registry.handle_events(&mut poller, &[]).expect("runs");
        assert_eq!(
            outcome.notifications,
            vec![PortNotification::Closed { port: id }],
            "handle_events reports the close it drained"
        );
        assert_eq!(outcome.ran, 0, "no port task was scheduled");
        assert!(
            registry.closed.is_empty(),
            "the close queue was drained by handle_events"
        );
        assert!(
            !registry.port_table().contains(id),
            "the reported close retired the port's number"
        );
        assert!(
            !registry.contains(id),
            "the reported port is not a live port"
        );

        // Exactly once: a second round has nothing left to report.
        let again = registry.handle_events(&mut poller, &[]).expect("runs");
        assert!(again.notifications.is_empty());
    }

    /// A driver error while a round runs its tasks must not drop a close that was already
    /// pending: the closes are collected only after the round's fallible steps, so the report
    /// survives — and keeps its number reserved — for the next round.
    #[test]
    fn a_task_error_does_not_drop_a_pending_close_report() {
        /// A driver whose `output` always fails.
        struct FailingWriteDriver;

        impl Driver for FailingWriteDriver {
            fn name(&self) -> &str {
                "failing-write"
            }

            fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
                Ok(())
            }

            fn output(&mut self, _ctx: &mut DriverContext<'_>) -> Result<usize> {
                Err(error::badarg())
            }

            fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
                Ok(())
            }

            fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
                Ok(())
            }
        }

        let mut poller = poller();
        let mut registry = PortRegistry::new();

        // A port that is closed (so a close is pending) ...
        let closed = registry
            .open(&mut poller, Box::new(FdDriver::pipe().expect("a pipe")))
            .expect("opens");
        registry.close(&mut poller, closed).expect("closes");

        // ... and a port with a scheduled OUTPUT task whose driver fails when it runs.
        let failing = registry
            .open(&mut poller, Box::new(FailingWriteDriver))
            .expect("opens");
        registry
            .port_command(&mut poller, failing, b"x", PortCommandOptions::new())
            .expect("the command is queued");

        let failed = registry.execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET);
        assert!(failed.is_err(), "the driver's failure surfaces");
        assert_eq!(
            registry.closed,
            vec![closed],
            "the pending close stays queued instead of being dropped"
        );
        assert!(
            registry.port_table().contains(closed),
            "its number stays reserved while the close is unreported"
        );

        // The next round reports it (the failed task was consumed by the round that ran it).
        let next = registry
            .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("runs");
        assert_eq!(
            next.notifications,
            vec![PortNotification::Closed { port: closed }],
            "the close survives the failed round"
        );
        assert!(!registry.port_table().contains(closed), "and retires its number");
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
    fn the_reserved_control_op_is_close() {
        // The registry reserves exactly one control op for itself; every other name is the
        // driver's vocabulary and is forwarded verbatim (see `PortControl`/`port_control`).
        assert_eq!(PortControl::CLOSE, "close");
    }

    #[test]
    fn port_control_forwards_driver_verbs_and_rejects_unknown_ones() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);

        // `getopts` reaches the driver, which answers with its current options; the default
        // pipe `FdDriver` is in binary mode.
        assert_eq!(
            registry
                .port_control(&mut poller, id, "getopts", b"")
                .expect("getopts reaches the driver"),
            b"binary"
        );

        // A real driver verb is reachable through the same verbatim forwarding: `line`
        // switches the driver into line mode ...
        assert!(registry
            .port_control(&mut poller, id, "line", b"1024")
            .expect("line is a driver verb")
            .is_empty());
        // ... which the next `getopts` then reflects.
        assert_eq!(
            registry
                .port_control(&mut poller, id, "getopts", b"")
                .expect("getopts reaches the driver"),
            b"line,1024"
        );

        // An unknown verb surfaces the driver's `BadArg`, never a silent no-op.
        assert!(matches!(
            registry.port_control(&mut poller, id, "invent", b""),
            Err(BeamError::BadArg)
        ));

        // The reserved `close` closes the port and answers with no bytes.
        assert!(registry
            .port_control(&mut poller, id, PortControl::CLOSE, b"")
            .expect("close is reserved")
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

    // --- port table accessors + the port-monitor seam ------------------------

    /// `port_table`/`port_table_mut` hand out the registry's **own** id table — the same one
    /// `open`/`close` allocate and release through — and the `beam_process` port-monitor API is
    /// drivable through it.
    #[test]
    fn the_port_table_accessors_reach_the_registrys_own_table_and_drive_port_monitors() {
        let mut poller = poller();
        let (mut registry, id) = registry_with_pipe(&mut poller);

        // The accessor sees exactly what the registry's own operations see: while the port is
        // live, both agree it is allocated.
        assert!(registry.contains(id));
        assert!(registry.port_table().contains(id));
        assert!(registry.port_table().monitors(id).is_empty());

        // A real process table (with its own heap + signal atoms) supplies the watcher.
        let mut table = ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()));
        let watcher = table.spawn_default().expect("a watcher process");

        // Monitor the port through the accessor: the returned reference is recorded on BOTH
        // halves — the incoming one on the port (the registry's own table) ...
        let reference = table
            .monitor_port(watcher, id, registry.port_table_mut())
            .expect("a live port is watchable");
        let incoming = registry.port_table().monitors(id);
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].reference, reference);
        assert_eq!(incoming[0].watcher, watcher);

        // ... and the outgoing one on the watcher.
        assert_eq!(
            table
                .get(watcher)
                .expect("the watcher is live")
                .port_monitors(),
            &[PortWatch {
                reference,
                port: id
            }]
        );

        // Demonitor through the accessor: it reports the monitor existed and drops both halves.
        assert!(table
            .demonitor_port(watcher, reference, registry.port_table_mut())
            .expect("the watcher is live"));
        assert!(registry.port_table().monitors(id).is_empty());
        assert!(table
            .get(watcher)
            .expect("the watcher is live")
            .port_monitors()
            .is_empty());
        // A second demonitor of the same reference reports `false` (it is already gone).
        assert!(!table
            .demonitor_port(watcher, reference, registry.port_table_mut())
            .expect("the watcher is live"));

        // Closing through the registry still drives the SAME table: the port is gone from the
        // live map immediately, but its number stays reserved in the port table until the close
        // is reported (so a deferred `Closed` can never name a reused id).
        registry.close(&mut poller, id).expect("closes");
        assert!(!registry.contains(id));
        assert!(
            registry.port_table().contains(id),
            "the number is reserved until the close is reported"
        );

        // The reporting round is what releases the number. Nothing is scheduled, so the round
        // only mints the close.
        let outcome = registry
            .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("the round runs");
        assert_eq!(
            outcome.notifications,
            vec![PortNotification::Closed { port: id }]
        );
        assert!(
            !registry.port_table().contains(id),
            "reporting the close retires the number"
        );
    }
}
