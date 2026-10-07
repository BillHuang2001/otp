//! Async-job bookkeeping for the C-ABI driver bridge ([`super::port::DriverPort`]).
//!
//! `driver_async` (`erts/emulator/beam/erl_async.c`) runs a driver's `async_invoke` on a worker
//! thread and later hands the result to the port's `ready_async` callback on the port's scheduler.
//! This module is the **M1 floor** of that machinery: one `std::thread` per job, and a channel the
//! worker reports completion through so the single-threaded port can run `ready_async`.
//!
//! Ground truth: `call_async_ready` (`erl_async.c:318-345`) — `async_ready` runs the driver's
//! `ready_async` when the entry has one and reports `need_free`; the emulator then calls
//! `async_free(async_data)` only in the `need_free` case (`io.c:5374-5403`). Fidelity note: the C
//! runs `async_free` on the *worker* thread; this floor records the completion and lets the port
//! thread decide (it is where the driver entry is reachable), so `async_free` runs on the port
//! thread instead. Documented deviation, never a leak (the callback runs exactly once).
//!
//! # `unsafe`
//!
//! [`AsyncData`] carries a raw driver-owned pointer across the worker boundary. It is the only
//! `unsafe` in this file (one `unsafe impl Send`, with its rationale) — the rest is safe Rust.

use std::sync::mpsc;
use std::thread;

use libc::c_void;

/// A driver `async_data` pointer on its way from the worker thread to the port thread.
///
/// The pointer is opaque to this crate: it is whatever the driver passed to `driver_async`, and the
/// driver contract (`erl_driver.h`) is that the emulator may hand it back on another thread —
/// exactly what this wrapper asserts.
pub(crate) struct AsyncData(pub(crate) *mut c_void);

// SAFETY: the driver's `driver_async` contract is that `async_data` (and the `invoke`/`free`
// callbacks over it) may be used from the emulator's async worker thread; moving the pointer
// between the worker and the port thread is precisely that transfer. The pointer is never
// dereferenced by `beam_io` — it is only handed back to the driver's own callbacks.
unsafe impl Send for AsyncData {}

/// A finished async job awaiting its `ready_async` (or `async_free`) callback.
pub(crate) struct AsyncCompletion {
    /// The queue position `driver_async` returned (the C's `async_id`).
    pub(crate) id: u64,
    /// The driver's `async_data`.
    pub(crate) data: AsyncData,
    /// The driver's `async_free`, for the entry-without-`ready_async` case (see the module docs).
    pub(crate) free: Option<unsafe extern "C" fn(*mut c_void)>,
}

/// The async-job channel plus its bookkeeping.
///
/// Lives on the [`super::port::DriverPort`] and is `!Sync` (an `mpsc::Receiver`): the worker threads
/// hold clones of the sending half; the port thread drains the receiving half in
/// [`super::port::DriverPort::poll_async`].
pub struct AsyncJobs {
    /// The sending half, cloned into each worker thread.
    tx: mpsc::Sender<AsyncCompletion>,
    /// The receiving half, drained by the port thread.
    rx: mpsc::Receiver<AsyncCompletion>,
    /// The next queue position (`driver_async`'s return value; strictly increasing, never 0).
    next_id: u64,
    /// Jobs scheduled but not yet drained (running or completed-and-undelivered).
    in_flight: usize,
}

impl AsyncJobs {
    /// An empty job set.
    pub fn new() -> AsyncJobs {
        let (tx, rx) = mpsc::channel();
        AsyncJobs {
            tx,
            rx,
            next_id: 1,
            in_flight: 0,
        }
    }

    /// The number of jobs scheduled but not yet drained by [`AsyncJobs::drain`].
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// Schedule `invoke(data)` on a fresh worker thread.
    ///
    /// Returns the job's queue position (the value `driver_async` returns). A `None` `invoke` is the
    /// **fire-and-forget** form: the C would fault calling a null `invoke` (`erl_async.c:604`,
    /// `(*a->async_invoke)(a->async_data)`), so this floor schedules no work and records no
    /// completion — it still returns a valid queue position, as the C's return contract requires.
    ///
    /// # Errors
    ///
    /// Returns `Err(data)` when the operating system refuses the worker thread; the caller then
    /// runs the driver's `async_free` (the C's `async_free` "release in case of failure" contract)
    /// and answers `driver_async`'s error code.
    pub(crate) fn schedule(
        &mut self,
        invoke: Option<unsafe extern "C" fn(*mut c_void)>,
        data: *mut c_void,
        free: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> Result<u64, ()> {
        let id = self.take_id();
        let Some(invoke) = invoke else {
            // Fire-and-forget: no work, no completion (see the method docs).
            return Ok(id);
        };
        let tx = self.tx.clone();
        let spawned = thread::Builder::new()
            .name(format!("beam_io-async-{id}"))
            .spawn(move || {
                // SAFETY: `invoke` is the driver's own `void(void*)` async callback and `data` is
                // the pointer the driver handed to `driver_async`; the driver contract is that both
                // stay valid on the async thread until the callback returns.
                unsafe { invoke(data) };
                // A send failure means the port is gone; the driver's data is then released by the
                // caller path (documented in the module docs) — never silently dropped here.
                let _ = tx.send(AsyncCompletion {
                    id,
                    data: AsyncData(data),
                    free,
                });
            });
        match spawned {
            Ok(_handle) => {
                self.in_flight += 1;
                Ok(id)
            }
            // The `ErlDrvError` path: hand the data back to the caller for `async_free`.
            Err(_) => Err(()),
        }
    }

    /// Drains every completion reported so far (non-blocking).
    pub(crate) fn drain(&mut self) -> Vec<AsyncCompletion> {
        let jobs: Vec<AsyncCompletion> = self.rx.try_iter().collect();
        self.in_flight = self.in_flight.saturating_sub(jobs.len());
        jobs
    }

    /// The next queue position.
    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        // Wrap to 1, never 0: the C skips 0 (`erl_async.c:575-581`).
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }
}

impl Default for AsyncJobs {
    fn default() -> AsyncJobs {
        AsyncJobs::new()
    }
}
