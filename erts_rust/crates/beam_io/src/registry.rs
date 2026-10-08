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
    /// [`Readiness::write_ready`] ⇒ a [`PortTaskKind::Output`] task, each applied only when
    /// the driver has **armed** that direction with the poller (`Port::read_armed`/
    /// `Port::write_armed`). A pure EOF/`EPOLLHUP` event (`read_closed` with
    /// `readable == false`) therefore still reaches the driver's `ready_input` — which is
    /// where the end-of-file latch lives — while a write-closed flag on a port that never
    /// armed a write fd schedules nothing (the kernel reports `EPOLLHUP` as *both* closed
    /// edges, so a full close of a read-only port must not be mistaken for write readiness).
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

    /// Takes port `id`'s end-of-file notification, once.
    ///
    /// This is the seam a runtime uses to synthesize the bare `eof` message of a port opened
    /// with the `eof` option: `Ok(true)` is reported exactly once, when the port's input stream
    /// reached end of file; `Ok(false)` means "not asked for", "not reached yet" or "already
    /// reported" (see [`crate::drivers::SpawnDriver`]).
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

        // Closing through the registry still drives the SAME table, so the accessor agrees
        // the number is released.
        registry.close(&mut poller, id).expect("closes");
        assert!(!registry.contains(id));
        assert!(!registry.port_table().contains(id));
    }
}
