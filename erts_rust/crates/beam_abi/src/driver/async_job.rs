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

//! The **safe async-job runner** — one C async job behind a safe, crate-owned handle.
//!
//! A driver that wants work done off its port's thread calls
//!
//! ```text
//! long driver_async(ErlDrvPort ix, unsigned int *key, void (*async_invoke)(void *),
//!                   void *async_data, void (*async_free)(void *));
//! ```
//!
//! (`erts/emulator/beam/erl_driver.h:605-607`). The last three operands **are** the job:
//! `async_invoke` does the work on `async_data`, and `async_free` releases `async_data` when the job
//! is finished with it. With the emulator's async pool disabled (`+A 0`), the C runs the job
//! **synchronously** in the calling thread — `(*async_invoke)(async_data)`, then, when the entry has
//! a `ready_async` callback, `(*async_free)(async_data)` (`erl_async.c:604-613`).
//!
//! The driver-facing half already exists: [`driver_async`](crate::driver::drv::driver_async) hands
//! the job to `DriverHost::r#async`. The **host** half was the gap — `invoke`, `free` and `data`
//! arrive as raw C pointers, so a runtime that forbids `unsafe` (the sibling `beam_rt` is
//! `#![deny(unsafe_code)]`) can receive a job but never run it.
//! [`AsyncJob`] closes that gap: the **single** `unsafe` entry point ([`AsyncJob::new`]) absorbs the
//! caller's promise that the three operands are a live C async job, and everything after it —
//! running the job, releasing it, inspecting it — is **safe**. No `unsafe` leaks into the rest of
//! the type's API.
//!
//! # How a host implements `DriverHost::r#async`
//!
//! The C's own no-pool path *is* an inline run, and it shows the whole API:
//!
//! ```text
//! fn r#async(&mut self, port, key, invoke, data, free) -> c_long {
//!     // SAFETY: `driver_async`'s operands — a live C async job.
//!     let Some(job) = (unsafe { AsyncJob::new(invoke, free, data) }) else {
//!         return -1; // `async_invoke == NULL`: nothing to run, so the caller still owns the data
//!     };
//!     let tdata = job.invoke();              // `(*async_invoke)(async_data)`
//!     let _ = self.ready_async(port, tdata); // the C then calls the entry's `ready_async`
//!     job.finish();                          // ... and finally `(*async_free)(async_data)`, once
//!     0                                      // `0` = "completed", as the C reports an inline job
//! }
//! ```
//!
//! The hook's signature and default stay exactly as they are: a host that has no async pool keeps
//! answering `-1`, and the existence of this type changes no slot's semantics (the addition is
//! **inert**).
//!
//! The C runs that final `async_free` whenever the result has been consumed — the entry's
//! `ready_async` callback took it (`erl_async.c:332`) or the port is already gone
//! (`erl_async.c:322`) — so a host reproduces the same gate by choosing *when* it releases:
//! `finish` runs it, dropping the handle runs it, and a host that hands the data on instead simply
//! never calls either.
//!
//! A host with a real worker pool queues the job instead (the C's `async_add`, `erl_async.c:544`);
//! note that the handle is neither `Send` nor `Sync` because it holds raw C pointers, so a pooled
//! host either constructs the handle on the worker thread that runs the job or re-establishes that
//! guarantee itself. Transferring a job between threads is the *pool's* contract, not something this
//! type can assert.
//!
//! # `unsafe`
//!
//! Every `unsafe` block below carries a `// SAFETY:` rationale and [`AsyncJob::new`] documents its
//! caller contract (workspace `CONTEXT.md` §3). `invoke`, `finish` and `drop` call the *driver's*
//! C callbacks, which is only sound because `new` was told they are a live job's callbacks.

use core::fmt;

use libc::c_void;

/// The C signature a driver's `async_invoke` / `async_free` callbacks have: `void (*)(void *)`
/// (`erl_driver.h:605-607`).
///
/// Both operands of `driver_async` share this one signature, so the type names them both — and it is
/// exactly the type `DriverHost::r#async` takes, so a host forwards its parameters straight into
/// [`AsyncJob::new`].
pub type AsyncCallback = unsafe extern "C" fn(*mut c_void);

/// One C **async job** handed over by `driver_async`, owned by this handle.
///
/// The handle owns the job in the C's sense: it may run `async_invoke` on the data, and it releases
/// the data with `async_free` when it is done with it (either explicitly through [`AsyncJob::finish`]
/// or by dropping the handle) — **exactly once**, never twice.
///
/// Constructing one is the only `unsafe` operation ([`AsyncJob::new`], which validates that there is
/// something to run); everything else is safe, so a runtime that forbids `unsafe` can drive a
/// driver's async callbacks.
pub struct AsyncJob {
    /// The C `async_invoke`. `Some` by construction: [`AsyncJob::new`] refuses a `NULL` invoke,
    /// because there is nothing to run and no job to own.
    invoke: AsyncCallback,
    /// The C `async_free`, or `None` when the driver passed a `NULL` callback (then there is nothing
    /// to release, and a finished/dropped job is a no-op).
    free: Option<AsyncCallback>,
    /// The C `async_data`, handed back verbatim to each callback. Never dereferenced by this crate.
    data: *mut c_void,
    /// Set once `async_free` has run, so it can never run a second time (see [`AsyncJob::release`]).
    finished: bool,
}

impl AsyncJob {
    /// Take ownership of the C async job `driver_async` received.
    ///
    /// Returns `None` — and owns nothing — when `invoke` is `None`: the C's `async_invoke` is what a
    /// job *is*, so a `NULL` one leaves nothing to run and nothing for this handle to release (the
    /// data remains the caller's).
    ///
    /// # Safety
    ///
    /// The caller must be the code that received the operands of a `driver_async` call — the
    /// byte-only entry points here and in `DriverHost::r#async` cannot check them — and must
    /// guarantee, for as long as the returned handle can run callbacks:
    ///
    /// * `invoke` is a live C `async_invoke` function pointer of the signature above, and `data` is
    ///   the matching `async_data` pointer it expects (valid for `async_invoke` to read/write);
    /// * `free`, when `Some`, is a live C `async_free` function pointer for that same `data`;
    /// * neither callback is invoked concurrently on this `data` by anyone else, and the driver is
    ///   still loaded and not unloaded while the handle lives (a `.so` must outlive its callbacks).
    pub unsafe fn new(
        invoke: Option<AsyncCallback>,
        free: Option<AsyncCallback>,
        data: *mut c_void,
    ) -> Option<AsyncJob> {
        Some(AsyncJob {
            invoke: invoke?,
            free,
            data,
            finished: false,
        })
    }

    /// Run the job's `async_invoke` on the data and return the data pointer.
    ///
    /// This is the C's `(*async_invoke)(async_data)`; the returned pointer is the `tdata` the C then
    /// passes to the entry's `ready_async` callback (`erl_async.c:590-613`), so a host that
    /// implements the inline path can forward it directly.
    ///
    /// The C invokes a job at most once, and so should the caller of this method; calling it after
    /// [`AsyncJob::finish`] is impossible (that consumes the handle) and calling it twice would be a
    /// misuse of the driver's own callback contract.
    pub fn invoke(&self) -> *mut c_void {
        // SAFETY: `new` recorded this `invoke` as a live C `async_invoke` and this `data` as its
        // matching `async_data`, and this handle is the sole owner of the job, so the call is
        // neither concurrent nor against a released operand.
        unsafe { (self.invoke)(self.data) };
        self.data
    }

    /// Release the job: run `async_free` on the data, exactly once, and consume the handle.
    ///
    /// A handle that is dropped instead is released the same way (see the `Drop` impl), so a host can
    /// run a job inline and then `finish` it — or simply queue it and let it fall out of scope. When
    /// the driver passed a `NULL` `async_free`, this is a no-op.
    pub fn finish(mut self) {
        self.release();
    }

    /// The job's `async_data` pointer — the same pointer [`AsyncJob::invoke`] hands back and the
    /// callbacks receive. A pointer: it is never dereferenced here.
    pub fn data(&self) -> *mut c_void {
        self.data
    }

    /// Whether the job has a C `async_invoke` (always `true` — [`AsyncJob::new`] refuses a `NULL`
    /// one, so this is the handle's invariant, stated for symmetry with [`AsyncJob::has_free`]).
    pub fn has_invoke(&self) -> bool {
        true
    }

    /// Whether the job has a C `async_free` to run ([`AsyncJob::finish`]/`Drop` are no-ops without
    /// one).
    pub fn has_free(&self) -> bool {
        self.free.is_some()
    }

    /// Release the data through `async_free` unless that has already happened.
    ///
    /// The single release point, shared by [`AsyncJob::finish`] and `Drop`, guarded so the C's
    /// `async_free` can never run twice for one job.
    fn release(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Some(free) = self.free {
            // SAFETY: `new` recorded this `free` as a live C `async_free` for this `data`, and
            // `finished` above makes this the one and only call on it.
            unsafe { free(self.data) };
        }
    }
}

impl Drop for AsyncJob {
    /// Release the job if it was not already released through [`AsyncJob::finish`] — the C's
    /// "no `async_invoke` ran (or it is already done), so `async_free` owns the data now" path.
    ///
    /// The `async_invoke` callback is **not** run here: dropping a job means giving up on it.
    fn drop(&mut self) {
        self.release();
    }
}

impl fmt::Debug for AsyncJob {
    /// A handle dump that **never** dereferences the job's operands — only the callback addresses,
    /// the data pointer and the released flag are printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsyncJob")
            .field("async_invoke", &(self.invoke as *const ()))
            .field("async_free", &self.free.map(|free| free as *const ()))
            .field("async_data", &self.data)
            .field("finished", &self.finished)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::AsyncJob;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering::SeqCst;

    use libc::c_void;

    /// A non-null address that can stand in for a driver's `async_data` payload.
    ///
    /// A function-local `static` so the pointer is valid for the whole test binary — the callbacks
    /// below never read it (they only compare/store the address), exactly like the C's opaque
    /// `async_data`.
    fn data_ptr() -> *mut c_void {
        static DATA: AtomicUsize = AtomicUsize::new(0x5eed_1234);
        core::ptr::addr_of!(DATA) as *mut c_void
    }

    /// A `NULL` `async_invoke` is refused: there is no job to own, and the caller keeps its data.
    #[test]
    fn a_job_without_an_invoke_callback_is_refused() {
        static FREES: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn free_count(_: *mut c_void) {
            FREES.fetch_add(1, SeqCst);
        }

        // SAFETY: `free_count` is a live C callback for a pointer payload it only counts; the
        // `None` invoke is exactly the case under test.
        let job = unsafe { AsyncJob::new(None, Some(free_count), data_ptr()) };
        assert!(job.is_none(), "a NULL `async_invoke` means there is nothing to run");
        assert_eq!(
            FREES.load(SeqCst),
            0,
            "refusing a job must not run the driver's `async_free`"
        );
    }

    /// The happy path: `invoke` runs `async_invoke` on the data and hands the data back;
    /// `finish` then runs `async_free` **once**, after the invoke.
    #[test]
    fn invoke_runs_the_invoke_callback_and_finish_frees_exactly_once() {
        static INVOKES: AtomicUsize = AtomicUsize::new(0);
        static FREES: AtomicUsize = AtomicUsize::new(0);
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        static INVOKE_AT: AtomicUsize = AtomicUsize::new(0);
        static FREE_AT: AtomicUsize = AtomicUsize::new(0);
        static SEEN_DATA: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn job_invoke(data: *mut c_void) {
            INVOKES.fetch_add(1, SeqCst);
            SEEN_DATA.store(data as usize, SeqCst);
            INVOKE_AT.store(SEQUENCE.fetch_add(1, SeqCst) + 1, SeqCst);
        }

        unsafe extern "C" fn job_free(data: *mut c_void) {
            FREES.fetch_add(1, SeqCst);
            SEEN_DATA.store(data as usize, SeqCst);
            FREE_AT.store(SEQUENCE.fetch_add(1, SeqCst) + 1, SeqCst);
        }

        let data = data_ptr();
        // SAFETY: the two callbacks are live C functions of the frozen signature and `data` is a
        // valid, never-dereferenced payload for them; nothing else touches them concurrently.
        let job = unsafe { AsyncJob::new(Some(job_invoke), Some(job_free), data) }
            .expect("a job with an `async_invoke` is a job");

        assert_eq!(job.data(), data);
        assert!(job.has_invoke());
        assert!(job.has_free());
        // The `Debug` impl reports the operands without touching them.
        let shown = format!("{job:?}");
        assert!(shown.contains("AsyncJob"), "the handle must be printable: {shown}");
        assert!(
            shown.contains("finished: false"),
            "a fresh job is not released: {shown}"
        );

        assert_eq!(job.invoke(), data, "`invoke` hands back the async data for `ready_async`");
        assert_eq!(INVOKES.load(SeqCst), 1, "`invoke` runs `async_invoke` exactly once");
        assert_eq!(FREES.load(SeqCst), 0, "`async_free` must not run before `finish`");

        // `finish` consumes the handle; its own `Drop` must not free a second time.
        job.finish();
        assert_eq!(INVOKES.load(SeqCst), 1);
        assert_eq!(FREES.load(SeqCst), 1, "`finish` runs `async_free` exactly once");
        assert_eq!(INVOKE_AT.load(SeqCst), 1, "`async_invoke` ran first");
        assert_eq!(FREE_AT.load(SeqCst), 2, "`async_free` ran after it");
        assert_eq!(
            SEEN_DATA.load(SeqCst),
            data as usize,
            "both callbacks must receive the data pointer the job was built with"
        );
    }

    /// Dropping a handle releases the data **without** running the job — the C's path for a job that
    /// never got to run (`async_free` owns the data then).
    #[test]
    fn dropping_a_job_frees_the_data_without_invoking_it() {
        static INVOKES: AtomicUsize = AtomicUsize::new(0);
        static FREES: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn job_invoke(_: *mut c_void) {
            INVOKES.fetch_add(1, SeqCst);
        }

        unsafe extern "C" fn job_free(_: *mut c_void) {
            FREES.fetch_add(1, SeqCst);
        }

        // SAFETY: two live C callbacks of the frozen signature and a never-dereferenced payload.
        let job = unsafe { AsyncJob::new(Some(job_invoke), Some(job_free), data_ptr()) }
            .expect("a job with an `async_invoke` is a job");
        drop(job);

        assert_eq!(INVOKES.load(SeqCst), 0, "dropping a job must not run `async_invoke`");
        assert_eq!(FREES.load(SeqCst), 1, "dropping a job must release the data exactly once");
    }

    /// A driver may pass a `NULL` `async_free` (the C treats that as "nothing to release"); the
    /// handle then runs the job and is dropped cleanly.
    #[test]
    fn a_job_without_a_free_callback_releases_nothing() {
        static INVOKES: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn job_invoke(_: *mut c_void) {
            INVOKES.fetch_add(1, SeqCst);
        }

        let data = data_ptr();
        // SAFETY: `job_invoke` is a live C callback; `None` free is the C's own "no release" case.
        let job = unsafe { AsyncJob::new(Some(job_invoke), None, data) }
            .expect("a job with an `async_invoke` is a job");
        assert!(!job.has_free());
        assert_eq!(job.invoke(), data);
        assert_eq!(INVOKES.load(SeqCst), 1);
        // Neither `finish` nor the ensuing `Drop` may call the absent callback.
        job.finish();
        assert_eq!(INVOKES.load(SeqCst), 1);
    }

    /// `new`'s `Option` is decided by the invoke callback alone: a `NULL` free is kept as-is, and
    /// the handle still reports the operands it owns (never dereferencing them).
    #[test]
    fn the_handle_reports_the_operands_it_owns() {
        unsafe extern "C" fn noop(_: *mut c_void) {}

        let data = data_ptr();
        // SAFETY: `noop` is a live C callback and `data` is never dereferenced by this crate.
        let job = unsafe { AsyncJob::new(Some(noop), Some(noop), data) }
            .expect("a job with an `async_invoke` is a job");
        assert_eq!(job.data(), data);
        assert!(job.has_invoke());
        assert!(job.has_free());
        let shown = format!("{job:?}");
        assert!(shown.contains("Some"), "the free callback's presence is reported: {shown}");
        // Consume the (never-invoked) job so the counter-assertions above stay per-test.
        job.finish();
    }
}
