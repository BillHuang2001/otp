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
//! The **delivery half** of the C async-job pool: what a finished `driver_async` job waits on
//! between the pool that ran it and the driver port that must deliver it.
//!
//! [`beam_abi::driver::async_pool`] owns the threads, the job ids and the `async_free`
//! bookkeeping, and it hands a completed job's fate to two host obligations (see its module docs):
//! the host drains the pool, and the host calls the driver's `ready_async`. Neither can happen
//! where the pool is drained — the pool is drained on the **port round**
//! ([`PortRegistry::pump`](crate::port::PortRegistry)), which holds no driver, while `ready_async`
//! needs the loaded `DriverLibrary` **and** the port's private `ErlDrvData`, which live together
//! only in [`DriverPortAdapter`](super::driver::DriverPortAdapter) — and the pool's ready queue is
//! **global**, so one port's drive point cannot deliver another port's job.
//!
//! This handle is the missing middle. It is one shared [`Rc`] cell, cloned into
//! * every driver port's host (which needs the pool and records the `+A 0` inline hand-offs),
//! * every [`DriverPortAdapter`](super::driver::DriverPortAdapter) (which performs the real
//!   `ready_async` delivery at its own drive point), and
//! * the [`PortRegistry`](crate::port::PortRegistry) (which drains the pool once per port round and
//!   distributes the results per port),
//!
//! and it holds, per `ErlDrvPort` handle:
//!
//! * the **ready queue** — finished jobs the pool handed over, waiting for their port's drive point
//!   ([`AsyncReady::offer`] / [`AsyncReady::take_ready`]);
//! * the **inline hand-offs** — the `+A 0` path's already-handed-off `async_data`, recorded from
//!   the host's [`DriverHost::deliver_ready_async`](beam_abi::DriverHost::deliver_ready_async) and
//!   delivered later by the adapter ([`AsyncReady::defer_inline`] / [`AsyncReady::take_inline`]);
//! * the **outstanding count** — jobs submitted through this port that have not reached a terminal
//!   disposition yet. It is what the port's drive point and its unload rule are gated on, and it is
//!   deliberately an **over**-approximation: a job a worker is in the middle of running is in
//!   neither of the pool's queues, so only this side knows it exists
//!   ([`AsyncReady::note_submission`] / [`AsyncReady::note_delivery`]);
//! * the **deferred unloads** — a library whose port closed while jobs of its own were still in
//!   flight. `dlclose` while a worker can still enter the driver's `async_invoke` would be
//!   undefined behaviour, so the [`Driver`] is kept here and released at the first round after its
//!   port's work is gone ([`AsyncReady::defer_unload`] / [`AsyncReady::reap_unloads`]).
//!
//! # The C's own reference duty
//!
//! The C pins the driver at submit (`erts_ddll_reference_referenced_driver`,
//! `erts/emulator/beam/erl_async.c:236-237`) and releases it at completion (`:343-344`), so its
//! `dlclose` can never precede a running job. The Rust pool cannot do that (the driver object is
//! the host's, not the pool's), so the duty lands here: [`AsyncReady::pending_for`] is the "is this
//! library still referenced" question and [`AsyncReady::reap_unloads`] is the release.
//!
//! # `unsafe`
//!
//! None — and the module is a `#![deny(unsafe_code)]` crate. The one raw value it carries (a job's
//! `async_data`) is only ever **moved**, never dereferenced: it is stored in a [`VecDeque`] and
//! handed back out unchanged, exactly the way the host received it.
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use libc::c_void;
use beam_abi::driver::types::ErlDrvPort;
use beam_abi::driver::{AsyncPool, CompletedAsync};
use super::driver::Driver;

/// A cheaply cloneable handle to the async-job delivery state of one runtime (see the module docs).
///
/// Cloning is an `Rc` clone; the state lives until the last clone is dropped, which is what makes
/// it safe to hold one inside a driver's host (whose lifetime is the port's) and another in the port
/// registry (whose lifetime is the runtime's).
#[derive(Clone)]
pub struct AsyncReady {
    /// The shared state. Every operation takes this borrow for its own duration only, so a
    /// delivery that calls back into the handle (a `ready_async` that submits another job) cannot
    /// deadlock on a borrow that is held across a driver callback.
    inner: Rc<RefCell<ReadyState>>,
}

/// The state behind [`AsyncReady`] (see the module docs for what each map means).
struct ReadyState {
    /// The pool itself — built lazily, on the first `{spawn_driver, Name}` port of the runtime
    /// ([`AsyncReady::ensure_pool`]), because a runtime that never opens a driver port must not
    /// start async threads. `+A 0` is `AsyncPool::new(0)` (jobs run inline), **not** `None`: with no
    /// pool at all `driver_async` would fall back to the legacy hook and report `-1`, which is the
    /// divergence this module exists to close.
    pool: Option<AsyncPool>,
    /// The worker count the pool was built with, so a later
    /// [`AsyncReady::set_threads`] can tell whether a rebuild is needed (`AsyncPool::threads()`
    /// reports the workers the OS actually started, which is not the requested count).
    pool_threads: usize,
    /// The `ErlDrvPort` handles of the driver ports that can receive a delivery. A job for a port
    /// that is not in here has no driver left to run `ready_async`, which is the C's `!p` branch
    /// (`erl_async.c:320-330`): its `async_free` runs, nothing is delivered.
    live: HashSet<usize>,
    /// Jobs submitted through a live port that have not reached a terminal disposition
    /// (delivered, freed, or released with the port). See [`AsyncReady::note_submission`].
    outstanding: HashMap<usize, usize>,
    /// Finished jobs the pool handed over, waiting for their port's drive point, in completion
    /// order.
    ready: HashMap<usize, VecDeque<CompletedAsync>>,
    /// `+A 0` inline hand-offs: the `async_data` of a job the pool already ran on the submitting
    /// thread and whose `ready_async` the host could not call. Already handed off — the pool's
    /// `async_free` must **not** run for these.
    inline: HashMap<usize, VecDeque<*mut c_void>>,
    /// Libraries whose port closed while their jobs were still in flight (see the module docs).
    unloads: Vec<(usize, Driver)>,
}

impl ReadyState {
    /// Releases every deferred library, **keeping** the ones that could still be entered.
    ///
    /// The runtime is going away, so this is the last chance: a library whose port still has queued
    /// or running jobs is deliberately leaked (`mem::forget`) rather than `dlclose`d under a worker
    /// thread. That is the C's own choice at shutdown, which does not clean its async queues up
    /// either ("we do not bother to clean everything up since we are about to terminate the runtime
    /// system", `erl_async.c:451-456`), and a leak on an abnormal teardown is the only safe option
    /// — unloading the code under a running callback is undefined behaviour.
    fn release_unloads(&mut self) {
        for (port, driver) in std::mem::take(&mut self.unloads) {
            if self.pending_for(port) > 0 {
                std::mem::forget(driver);
            }
        }
    }

    /// How many jobs of `port` the runtime still owes a disposition for: the pool's own queued and
    /// ready records plus the ones this side knows are running (see the module docs).
    fn pending_for(&self, port: usize) -> usize {
        let pooled = self
            .pool
            .as_ref()
            .map_or(0, |pool| pool.pending_for_port(port as ErlDrvPort));
        let outstanding = self.outstanding.get(&port).copied().unwrap_or(0);
        pooled + outstanding
    }
}

impl Drop for ReadyState {
    fn drop(&mut self) {
        self.release_unloads();
    }
}

impl AsyncReady {
    /// An empty delivery state with **no pool yet** (see [`ReadyState::pool`]).
    pub fn new() -> AsyncReady {
        AsyncReady {
            inner: Rc::new(RefCell::new(ReadyState {
                pool: None,
                pool_threads: 0,
                live: HashSet::new(),
                outstanding: HashMap::new(),
                ready: HashMap::new(),
                inline: HashMap::new(),
                unloads: Vec::new(),
            })),
        }
    }

    /// Builds the pool with `threads` workers if it does not exist yet — the emulator's `+A N`.
    ///
    /// `0` is `+A 0`: [`AsyncPool::new(0)`] builds no thread and every job runs inline on the
    /// submitting thread. Called from the first `{spawn_driver, Name}` port of the runtime, which
    /// is the first moment a `driver_async` could be called at all.
    pub fn ensure_pool(&self, threads: usize) {
        let mut state = self.inner.borrow_mut();
        if state.pool.is_none() {
            state.pool = Some(AsyncPool::new(threads));
            state.pool_threads = threads;
        }
    }

    /// Sets the worker count the pool is (or will be) built with — the emulator's `+A N`.
    ///
    /// Meant for startup, before any driver port is opened: a pool that already exists is rebuilt
    /// only when the requested count differs, and the old one is dropped (its workers are joined),
    /// so calling this while jobs are in flight stalls until they finish.
    pub fn set_threads(&self, threads: usize) {
        let mut state = self.inner.borrow_mut();
        if state.pool.is_none() {
            state.pool_threads = threads;
            return;
        }
        if state.pool_threads == threads {
            return;
        }
        state.pool = Some(AsyncPool::new(threads));
        state.pool_threads = threads;
    }

    /// The configured worker count (`0` = the inline `+A 0` path).
    pub fn threads(&self) -> usize {
        self.inner.borrow().pool_threads
    }

    /// The pool, if it has been built — a cheap `Arc` clone. `None` until the runtime opens a
    /// driver port (see [`AsyncReady::ensure_pool`]).
    pub fn pool(&self) -> Option<AsyncPool> {
        self.inner.borrow().pool.clone()
    }

    /// A driver port came up: its handle can now receive `ready_async` deliveries.
    ///
    /// Called from [`beam_io::Driver::start`](beam_io::Driver::start) — where the `ErlDrvPort`
    /// handle first exists — **before** the driver's own `start` callback runs, so a `driver_async`
    /// issued from `start` is already addressed to a live port.
    pub fn attach(&self, port: ErlDrvPort) {
        self.inner.borrow_mut().live.insert(port as usize);
    }

    /// A driver port is going away: nothing of its own is deliverable any more.
    ///
    /// Releases everything this side still held for it — the C's `!p` branch, which runs a gone
    /// job's `async_free` and delivers nothing (`erl_async.c:320-330`) — and forgets the port's
    /// outstanding count. A `+A 0` inline hand-off that the driver's `ready_async` never saw cannot
    /// be released (its job was already handed off), so it is dropped: a leak of the driver's own
    /// `async_data`, never a double free.
    ///
    /// Returns how many finished jobs were released.
    pub fn detach(&self, port: ErlDrvPort) -> usize {
        let key = port as usize;
        let mut state = self.inner.borrow_mut();
        state.live.remove(&key);
        state.outstanding.remove(&key);
        let mut released = 0;
        if let Some(items) = state.ready.remove(&key) {
            for item in items {
                item.finish();
                released += 1;
            }
        }
        state.inline.remove(&key);
        // Everything the **pool** still holds for the port is marked gone, so its completion frees
        // the data instead of delivering it — the C's `!p` branch, applied at the earliest moment it
        // is known. (A job `AsyncReady::offer` meets first is released the same way, so this is
        // belt and braces, never the only guard.)
        if let Some(pool) = state.pool.as_ref() {
            pool.port_closed(port);
        }
        released
    }

    /// Records that one more job was submitted through `port` (see [`ReadyState::outstanding`]).
    ///
    /// Called from the host's [`DriverHost::async_pool`](beam_abi::DriverHost::async_pool), which
    /// `driver_async` reaches exactly once per call. A call the ABI then refuses (a `NULL`
    /// `async_invoke`) over-counts by one, which is safe: the count only ever gates a drive point
    /// and an unload, so an over-count keeps a library mapped, never the other way round.
    pub fn note_submission(&self, port: ErlDrvPort) {
        let key = port as usize;
        let mut state = self.inner.borrow_mut();
        if !state.live.contains(&key) {
            return;
        }
        *state.outstanding.entry(key).or_insert(0) += 1;
    }

    /// Records that one job of `port` reached its terminal disposition.
    pub fn note_delivery(&self, port: ErlDrvPort) {
        let key = port as usize;
        let mut state = self.inner.borrow_mut();
        if let Some(count) = state.outstanding.get_mut(&key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.outstanding.remove(&key);
            }
        }
    }

    /// How many jobs of `port` are submitted but not yet disposed of (see [`ReadyState::outstanding`]).
    pub fn outstanding(&self, port: ErlDrvPort) -> usize {
        self.inner
            .borrow()
            .outstanding
            .get(&(port as usize))
            .copied()
            .unwrap_or(0)
    }

    /// Takes the jobs the pool completed, routing each to the port it belongs to.
    ///
    /// A job whose port is no longer live is released here — the C's gone-port branch — and its
    /// outstanding count is dropped with it. Returns how many were released immediately.
    pub fn offer(&self, drained: Vec<CompletedAsync>) -> usize {
        if drained.is_empty() {
            return 0;
        }
        let mut state = self.inner.borrow_mut();
        let mut released = 0;
        for item in drained {
            let key = item.port() as usize;
            if state.live.contains(&key) {
                state.ready.entry(key).or_default().push_back(item);
            } else {
                item.finish();
                state.outstanding.remove(&key);
                released += 1;
            }
        }
        released
    }

    /// Takes the finished jobs waiting for `port`'s drive point, in completion order.
    ///
    /// The caller owns the completion policy: it must call
    /// [`CompletedAsync::complete`](beam_abi::driver::CompletedAsync::complete) with the port's own
    /// delivery hook (which runs `ready_async` and answers whether the data was consumed) or
    /// [`finish`](beam_abi::driver::CompletedAsync::finish). A forgotten value releases its data
    /// exactly once anyway (the pool's own drop rule), so this can never double-free.
    pub fn take_ready(&self, port: ErlDrvPort) -> Vec<CompletedAsync> {
        let mut state = self.inner.borrow_mut();
        state
            .ready
            .remove(&(port as usize))
            .map(VecDeque::into_iter)
            .map(Iterator::collect)
            .unwrap_or_default()
    }

    /// Whether `port` has anything to deliver at its next drive point.
    pub fn has_delivery(&self, port: ErlDrvPort) -> bool {
        let key = port as usize;
        let state = self.inner.borrow();
        state.ready.get(&key).is_some_and(|items| !items.is_empty())
            || state.inline.get(&key).is_some_and(|items| !items.is_empty())
    }

    /// Queues one `+A 0` inline delivery for `port` (see [`ReadyState::inline`]).
    ///
    /// Returns `false` when the port is no longer live — the caller then answers the pool with
    /// "free it", exactly as the C's gone-port branch does. The `data` operand is only stored; this
    /// crate never dereferences it.
    pub fn defer_inline(&self, port: ErlDrvPort, data: *mut c_void) -> bool {
        let key = port as usize;
        let mut state = self.inner.borrow_mut();
        if !state.live.contains(&key) {
            state.outstanding.remove(&key);
            return false;
        }
        state.inline.entry(key).or_default().push_back(data);
        true
    }

    /// Takes the `+A 0` inline hand-offs queued for `port`, in submission order.
    pub fn take_inline(&self, port: ErlDrvPort) -> Vec<*mut c_void> {
        let mut state = self.inner.borrow_mut();
        state
            .inline
            .remove(&(port as usize))
            .map(VecDeque::into_iter)
            .map(Iterator::collect)
            .unwrap_or_default()
    }

    /// Keeps `driver` mapped until nothing of `port` can still be running (see the module docs).
    pub fn defer_unload(&self, port: ErlDrvPort, driver: Driver) {
        self.inner
            .borrow_mut()
            .unloads
            .push((port as usize, driver));
    }

    /// Releases every deferred library whose port's work is gone. Returns how many were released.
    ///
    /// Idempotent and cheap when nothing was deferred (the common case: a port almost always closes
    /// with no job in flight), which is why a port round can call it unconditionally.
    pub fn reap_unloads(&self) -> usize {
        let mut state = self.inner.borrow_mut();
        if state.unloads.is_empty() {
            return 0;
        }
        let before = state.unloads.len();
        // Collected first so no `RefCell` borrow is held across the driver's own drop (which runs
        // the entry's `finish` and the `dlclose`).
        let mut released = Vec::new();
        let mut kept = Vec::new();
        for (port, driver) in std::mem::take(&mut state.unloads) {
            if state.pending_for(port) > 0 {
                kept.push((port, driver));
            } else {
                released.push(driver);
            }
        }
        state.unloads = kept;
        drop(state);
        drop(released);
        before - self.inner.borrow().unloads.len()
    }

    /// Whether the pool has any work at all: something finished and waiting, an inline hand-off, a
    /// library kept mapped, or a job still queued/running for a live port.
    ///
    /// The port round uses it to decide whether it may block for a whole poll slice, and
    /// [`PortRegistry::has_nothing_to_poll`](crate::port::PortRegistry) to decide whether a blocked
    /// process can still be woken — a `ready_async`'s `driver_output` is a `{Port, {data, …}}` the
    /// owner is owed.
    pub fn pool_is_busy(&self, pool: &AsyncPool) -> bool {
        let state = self.inner.borrow();
        if !state.ready.is_empty() || !state.inline.is_empty() || !state.unloads.is_empty() {
            return true;
        }
        if pool.ready_len() > 0 {
            return true;
        }
        state.live.iter().any(|port| state.pending_for(*port) > 0)
    }
}

impl Default for AsyncReady {
    fn default() -> AsyncReady {
        AsyncReady::new()
    }
}

impl std::fmt::Debug for AsyncReady {
    /// Reports the shape, never a job's `async_data` payload.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.inner.borrow();
        f.debug_struct("AsyncReady")
            .field("threads", &state.pool_threads)
            .field("live", &state.live.len())
            .field("ready", &state.ready.len())
            .field("inline", &state.inline.len())
            .field("unloads", &state.unloads.len())
            .finish()
    }
}
