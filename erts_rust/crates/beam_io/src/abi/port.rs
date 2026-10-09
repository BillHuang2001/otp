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

//! [`DriverPort`] — a loaded C driver (`ErlDrvEntry`) presented as a
//! [`Driver`](crate::driver::Driver).
//!
//! This is the adapter the frozen driver C ABI (contract **A.4**) needs: a `.so` loaded through
//! [`beam_abi::driver::loader::load`] cannot implement a Rust trait, so [`DriverPort`] implements
//! [`Driver`](crate::driver::Driver) *over* the loaded
//! [`DriverLibrary`](beam_abi::driver::loader::DriverLibrary)'s callback slots.
//!
//! # What maps to what
//!
//! | [`Driver`](crate::driver::Driver) | `ErlDrvEntry` slot | notes |
//! |---|---|---|
//! | [`Driver::start`] | `start` | absent slot ⇒ `badarg` (the C's `init_driver` requires it) |
//! | [`Driver::output`] | `outputv`, falling back to `output` | the port's output queue is drained and handed over |
//! | [`Driver::ready_input`] | `ready_input` | absent slot ⇒ no-op (the C's `no_ready_input_callback`) |
//! | [`Driver::ready_output`] | `outputv`/`output`, then `ready_output` | the write path, then the write-readiness notification when the slot exists |
//! | [`Driver::timeout`] | `timeout` | absent slot ⇒ no-op (`no_timeout_callback`) |
//! | [`Driver::control`] | `control` | absent slot ⇒ `badarg` (`io.c:4035`) |
//! | [`Driver::port_exit`] | `process_exit`, falling back to `emergency_close` | the C has no port-exit callback |
//! | [`Driver::stop`] | `stop` | absent slot ⇒ no-op |
//! | [`Driver::next_timeout`] | the `driver_set_timer` deadline | see [`crate::AbiHost`] |
//! | [`DriverPort::poll_async`] | `ready_async` / `async_free` | drained from the `driver_async` job set |
//! | — | `stop_select` | the `ERL_DRV_USE` release callback; runs outside the `Driver` trait, on deselect and at teardown |
//!
//! # The port handle is stable, and the host outlives every callback
//!
//! A driver callback receives an opaque `ErlDrvPort`, which
//! [`beam_abi`](beam_abi::driver::host::DriverEnv) defines as the pointer
//! [`DriverEnv::as_port`](beam_abi::driver::host::DriverEnv::as_port) returns. Both the
//! [`DriverEnv`](beam_abi::driver::host::DriverEnv) and the [`AbiHost`] it points at are **boxed**
//! and moved into this struct, so the handle is the same pointer for the port's whole life — the
//! property the C gives a driver for free (`&prt->drv_port` never moves) and the one that lets a
//! driver stash it in its own data.
//!
//! **M1 limitation:** the handle being stable is necessary but not sufficient. The
//! [`DriverEnv`](beam_abi::driver::host::DriverEnv) borrows an [`AbiHost`] that is only *reachable*
//! through a live [`DriverPort`], and the runtime's port context is installed per callback — so a
//! driver must **not** call `driver_*` (or touch the stashed handle) after its callback has
//! returned, e.g. from a thread it started itself. Async work has a proper channel for exactly this
//! ([`DriverPort::poll_async`]); anything else is out of scope until the runtime keeps a per-port
//! `DriverEnv` alive for the port's whole life.
//!
//! # Where the current host is published
//!
//! Two frozen `ErlDrvApi` slots carry **no** port operand (`add_driver_entry`/`remove_driver_entry`:
//! they act on the emulator's process-global `driver_list`), so they cannot recover their host from
//! a port. The runtime therefore publishes the host it is running driver code for with
//! [`beam_abi::with_current_host`], and those two slots read it back. Both places this adapter runs
//! driver code publish it:
//!
//! * [`DriverPort::call`] — around **every** callback it routes (`start`/`output`/`ready_input`/
//!   `ready_output`/`control`/`timeout`/`port_exit`/`stop`), including the `ready_async` branch of
//!   [`DriverPort::poll_async`], and around the `stop_select` callbacks `DriverPort` fires on a
//!   released `ERL_DRV_USE` selection; and
//! * the `async_free` branch of [`DriverPort::poll_async`], which bypasses `call` (the C's
//!   `async_free` is `void (*)(void*)`, with no port and no `ready_async`).
//!
//! # `ERL_DRV_USE` — the `stop_select` release callback
//!
//! A `driver_select` whose `mode` carries `ERL_DRV_USE` (`ERL_NIF_SELECT_STOP`) tells the runtime
//! the driver wants `stop_select(event, NULL)` when the selection is really gone — the last moment
//! at which it may close or recycle the handle (`ERL_DRV_USE_NO_CALLBACK`, i.e. `ERL_DRV_USE |
//! ERL_DRV_USE << 1`, keeps the bookkeeping but declines the callback). [`AbiHost::select`] records
//! the two bits with the registration; this adapter fires the callback from three places, all of
//! them **after** the driver call that produced the release has returned, as in the C:
//!
//! * a deselect (`driver_select(..., 0)`) — when the `call` that ran it returns
//!   (`erl_check_io.c:1210-1216`), with the event the *deselecting* call was given;
//! * port teardown ([`Driver::stop`]) — for every registration the driver left selected, with the
//!   fd cast to an `ErlDrvEvent` (`erl_check_io.c:2232-2238`); and
//! * [`Drop`] — the same teardown release for a port that never ran `stop`.
//!
//! The remaining `Driver` methods ([`Driver::drive_async`] = `poll_async`, [`Driver::next_timeout`],
//! [`Driver::take_exit_status`]) run no driver code, so they publish nothing. A driver that calls
//! `driver_*` from a thread it started itself is outside this: no context is installed there (the
//! M1 limitation above).
//!
//! # The floor (documented deviations)
//!
//! * **`output`/`ready_output` bytes are treated as consumed.** The C `output`/`ready_output`
//!   callbacks return `void`; the port hands the driver whatever is in the output queue and counts
//!   the whole buffer as written. Both paths drain that queue through the **`outputv`** slot when
//!   the entry declares one, else its `output` (`if (drv->outputv) … else … drv->output`,
//!   `io.c:1782`), so an entry whose only port-command callback is `outputv` still receives its
//!   `port_command` bytes. When the entry additionally declares `ready_output`, that slot runs
//!   afterwards as the C's write-readiness notification; it receives no buffer (its parameter is
//!   the event), so the bytes are delivered by `output`/`outputv`, never by it.
//! * **`ready_input`/`ready_output` name one fd.** The C fires one callback per `ErlDrvEvent`; the
//!   poller here keys a registration by port number, so the fd is taken from the driver's own
//!   `driver_select` bookkeeping (see [`DriverState::fd_armed`](crate::DriverState)). A port with
//!   several selected fds in one direction sees them in fd order, one per port-task round.
//! * **`control` verbs are numeric.** `Driver::control`'s `command` is parsed as a decimal `c_uint`
//!   (the C's `INET_REQ_*`-style code); the built-in string verbs (`"close"`, `"line"`, …) belong
//!   to this crate's Rust drivers and are not meaningful to a C driver.
//! * **A `start` error sentinel is `badarg`.** `ERL_DRV_ERROR_GENERAL`/`_ERRNO`/`_BADARG` all map
//!   to [`BeamError::BadArg`]: the C turns them into the `{'EXIT', {einval|Errno|badarg, Where}}`
//!   the *caller* of `open_port/2` sees, which is a runtime (`beam_rt`) rendering decision, not
//!   this layer's.
//!
//! # Where the end-to-end C-driver test plugs in
//!
//! A sibling task owns the real end-to-end test: a C driver source
//! `erts_rust/crates/beam_io/tests/fixtures/host_driver.c` (built with the system `cc`, linking
//! against `erts/emulator/beam/erl_driver.h`) loaded through
//! [`beam_abi::driver::loader::load`] + [`DriverPort::load`], opened through
//! [`PortRegistry::open`](crate::PortRegistry::open), and driven through
//! [`PortRegistry::execute_port_tasks`](crate::PortRegistry::execute_port_tasks) with
//! [`DriverPort::poll_async`] after each round. That test's binary must reference
//! [`beam_abi::driver::drv::driver_api_table`] (or call a `driver_*` function directly) so the
//! `#[no_mangle]` implementations reach the link and are exported for the `.so`
//! (`build.rs` adds `-Wl,--export-dynamic` to the test targets). The in-module test below does the
//! minimal version of that reference.
//!
//! # `unsafe`
//!
//! This is an explicitly-marked `unsafe` module (workspace `../CONTEXT.md` §3): it calls every
//! callback slot of a foreign object and reads the driver's reply buffer. Each `unsafe` block
//! carries a `// SAFETY:` rationale.

use std::ffi::CString;
use std::os::unix::io::RawFd;
use std::sync::Arc;
use std::time::Instant;

use libc::{c_char, c_int, c_uint, c_void};

use beam_abi::driver::host::{DriverEnv, DriverHost};
use beam_abi::driver::loader::{load, DriverLibrary};
use beam_abi::driver::types::{
    ErlDrvBinary, ErlDrvData, ErlDrvEvent, ErlDrvMonitor, ErlDrvPort, ErlDrvSizeT,
    ErlDrvThreadData, ErlIOVec, ERL_DRV_ERROR_BADARG, ERL_DRV_ERROR_ERRNO,
    ERL_DRV_ERROR_GENERAL, ERL_DRV_FLAG_SOFT_BUSY,
};
use beam_abi::SysIOVec;
use beam_common::{BeamError, ExitReason, Raise, Result};

use crate::abi::host::{AbiHost, PortHost};
use crate::driver::{Driver, DriverContext, DriverFlags, PortExitReason};
use crate::error;

/// The capacity of the reply buffer handed to a driver's `control` callback.
///
/// `control`'s `rlen` argument is the caller's buffer capacity (`io.c:4023-4083`), and a driver may
/// either fill it or re-point the reply pointer at its own memory. The C uses a stack buffer of a
/// few kilobytes; this is that buffer. A driver that ignores `rlen` and writes more is a bug in
/// native code the runtime loaded — see the trust-boundary note on [`Driver::control`].
const CONTROL_REPLY_CAPACITY: usize = 4096;

/// Pins a driver's loaded object (`Arc<DriverLibrary>`) for as long as one of its `driver_async` jobs
/// (or an undelivered completion) still exists.
///
/// The port installs one of these in its [`AsyncJobs`](crate::abi::AsyncJobs) so the `dlclose` a
/// dropped port would otherwise run is deferred until the last job's `async_free` has run — mirroring
/// the C's `DE_Handle` reference for an outstanding async job (`erl_async.c:343-344`). Without it, a
/// job that outlives its port would run unmapped code.
struct LibraryKeepAlive(#[allow(dead_code)] Arc<DriverLibrary>);

// SAFETY: the wrapped `Arc<DriverLibrary>` is only ever held as a reference count and is never
// dereferenced by `beam_io`. The driver's code and static data are used from the async worker thread
// by the driver contract itself (the C likewise hands its worker the driver's `DE_Handle`), so moving
// or sharing the handle cannot violate Rust aliasing — the only shared state is the atomic refcount
// inside the `Arc`.
unsafe impl Send for LibraryKeepAlive {}
unsafe impl Sync for LibraryKeepAlive {}

/// A loaded C driver, presented as a [`Driver`](crate::driver::Driver).
pub struct DriverPort {
    /// The `dlopen` handle and the driver's `ErlDrvEntry`; kept alive for the port's whole life.
    ///
    /// Shared through an [`Arc`] so the *process-global driver
    /// table* ([`crate::abi::DriverTable`]) can keep the same loaded driver registered while
    /// ports are created from it, and so several ports of one driver share a single `dlopen`
    /// handle. The `DriverLibrary`'s `Drop` (`finish` + `dlclose`) runs when the last owner
    /// goes — the load path's `do_unload_driver_entry` order.
    lib: Arc<DriverLibrary>,
    /// The driver name the entry declares (the C's `drv->name`), reported by [`Driver::name`].
    name: String,
    /// The `open_port/2` command string, NUL-terminated, handed to `start`.
    command: CString,
    /// The [`DriverHost`](beam_abi::driver::host::DriverHost) behind the opaque port handle.
    ///
    /// Boxed so the pointer the driver holds through
    /// [`DriverEnv`](beam_abi::driver::host::DriverEnv) stays valid for the port's whole life.
    abi: Box<AbiHost>,
    /// The C-facing port environment; `DriverEnv::as_port()` **is** the `ErlDrvPort` a driver sees.
    env: Box<DriverEnv>,
    /// The driver's private data, as returned by its `start` callback.
    data: ErlDrvData,
}

impl DriverPort {
    /// Loads the C driver at `path` (its `driver_init` entry must declare `name`) and wraps it in a
    /// [`Driver`](crate::driver::Driver) over `port_host`.
    ///
    /// `command` is the `open_port({spawn, Name}, …)` string the driver's `start` callback receives.
    ///
    /// # Errors
    ///
    /// * [`BeamError::BadArg`] when `command` contains an interior NUL (it cannot be turned into
    ///   the C string `start` takes).
    /// * [`BeamError::Raise`] carrying the oracle's load-failure reason atom
    ///   ([`DriverLoadError::reason_atom`](beam_abi::driver::loader::DriverLoadError::reason_atom) —
    ///   `unspecified_error` for an unopenable object, `no_driver_init`, `bad_driver_name`,
    ///   `driver_incorrect_version`, `driver_init_failed`) when the object cannot be loaded or
    ///   validated.
    /// * Whatever the term-heap allocation returns (see [`AbiHost::new`]).
    pub fn load(
        path: &str,
        name: &str,
        command: impl Into<String>,
        port_host: Box<dyn PortHost>,
    ) -> Result<DriverPort> {
        // The command is validated **before** the object is opened, so a NUL-bearing command is
        // reported as the argument fault it is rather than as a load failure.
        let command = CString::new(command.into()).map_err(|_| error::badarg())?;
        let lib = load(path, name).map_err(load_failure)?;
        DriverPort::from_library(Arc::new(lib), command, port_host)
    }

    /// Wraps an **already loaded** driver (a [`DriverLibrary`] owned by this crate or by the
    /// process-global driver table) as a [`Driver`](crate::driver::Driver) over `port_host`.
    ///
    /// This is the constructor the process-global registry
    /// ([`DriverTable::create_port`](crate::abi::DriverTable::create_port)) uses: it holds a
    /// registration's [`Arc<DriverLibrary>`] and opens a new port from it **without** a
    /// second `dlopen`. The entry's `init` callback is *not* run here — the load path already
    /// ran it (`do_load_driver_entry` → `erts_add_driver_entry` → `init_driver`), and
    /// [`Driver::start`] runs for the new port in
    /// [`PortRegistry::open`](crate::PortRegistry::open).
    ///
    /// `command` is the NUL-terminated `open_port/2` command string the driver's `start`
    /// callback receives; building it (`CString::new`) is the caller's job, so an interior
    /// NUL is refused there.
    ///
    /// # Errors
    ///
    /// Whatever the term-heap allocation returns (see [`AbiHost::new`]).
    pub fn from_library(
        library: Arc<DriverLibrary>,
        command: CString,
        port_host: Box<dyn PortHost>,
    ) -> Result<DriverPort> {
        let name = library.name();
        let mut abi = Box::new(AbiHost::for_driver(port_host, name.clone(), true)?);
        // Every `driver_async` job this port schedules — and every completion it never drained —
        // clones this handle, so the driver's loaded object stays mapped until the last job has been
        // released. That is the C's `DE_Handle` reference for an outstanding async job
        // (`erts_ddll_dereference_driver`, `erl_async.c:343-344`): without it, the `dlclose` a
        // dropped port runs would unmap the very code (`async_invoke`/`async_free`) a job that
        // outlives its port still has to execute.
        abi.state_mut()
            .jobs
            .set_keepalive(Arc::new(LibraryKeepAlive(Arc::clone(&library))));
        // The `ErlDrvPort` a driver sees is `DriverEnv::as_port()`'s pointer, so the env must point
        // at an allocation that never moves. `abi` is boxed here (stable address) and both boxes are
        // moved into `DriverPort`, which owns them for the port's whole life — so the raw pointer the
        // env stores stays valid, and `DriverPort` never hands the env out by value.
        let env = Box::new(DriverEnv::new(&mut *abi));
        Ok(DriverPort {
            lib: library,
            name,
            command,
            abi,
            env,
            data: core::ptr::null_mut(),
        })
    }

    /// Runs a driver callback with the port context installed, clearing it afterwards.
    ///
    /// `f` receives the loaded library and the driver's private data — never `&mut self`, so this
    /// can hold an immutable borrow of the [`AbiHost`] for the cleanup guard's lifetime while the
    /// callback runs.
    ///
    /// The cleanup is a `Drop` guard, so the context is cleared even if the callback body unwinds —
    /// the installed pointer must never outlive the [`DriverContext`] it came from.
    ///
    /// The port's host is **published** for the dynamic extent of the callback
    /// ([`beam_abi::with_current_host`]), so the two frozen driver slots that carry no port operand
    /// (`add_driver_entry`/`remove_driver_entry`) can reach it. This is the central publication
    /// point: every `call`-routed driver method (`start`/`output`/`ready_input`/`ready_output`/
    /// `control`/`timeout`/`port_exit`/`stop`) is covered by it.
    fn call<R>(
        &mut self,
        ctx: &mut DriverContext<'_>,
        f: impl FnOnce(&DriverPort, ErlDrvPort) -> R,
    ) -> R {
        let port = self.env.as_port();
        // SAFETY: `abi_ptr` is `&mut *self.abi` taken as a raw pointer; that temporary borrow ends
        // at this statement, so nothing below re-borrows `self.abi` through it in the borrow
        // checker. The `&mut dyn DriverHost` derived from it reaches only the `AbiHost`'s
        // interior-mutable state (`Cell`/`DriverState`), which no other live borrow touches: `f`
        // reads and writes only *other* `DriverPort` fields (`lib`/`data`). The publication is
        // scoped to the `with_current_host` call, so the pointer is retired before the borrow ends.
        // This mirrors the crate's existing raw-pointer re-entry (`AbiHost::with_ctx`,
        // `DriverEnv::new`/`host`).
        let abi_ptr: *mut AbiHost = &mut *self.abi;
        self.abi.install(ctx);
        let _guard = CtxGuard(abi_ptr);
        // SAFETY: `abi_ptr` is the address of the boxed `AbiHost` field (see above), live for the
        // whole of this call.
        let host: &mut dyn DriverHost = unsafe { &mut *abi_ptr };
        let result = beam_abi::with_current_host(host, || f(self, port));
        // A `driver_select(..., 0)` the driver just made queued its `ERL_DRV_USE` release (see
        // `AbiHost::select`); the callback runs now, after the driver's own call has returned.
        self.drain_queued_stop_select();
        result
    }

    /// Fires the driver's `stop_select` callback for each `ErlDrvEvent` in `events`.
    ///
    /// `stop_select` is the frozen `ERL_DRV_USE` release callback: a driver that selected a handle
    /// with `ERL_DRV_USE` (`ERL_NIF_SELECT_STOP`) is told when the selection is really gone, so it
    /// may close or recycle the handle. The C invokes it with the event the *deselecting*
    /// `driver_select` was given (`erl_check_io.c:1215`) or, at port teardown, with the fd cast to
    /// an `ErlDrvEvent` (`erl_check_io.c:2235`). Its signature is `void(ErlDrvEvent, void*)`, with
    /// no port and no failure channel, so an entry without the slot is simply nothing to do (the
    /// C's `no_stop_select_callback`, `io.c:7546-7553`).
    ///
    /// The host is published for the same reason [`DriverPort::call`] publishes it around every
    /// other callback: a driver may reach the two no-port registry slots from `stop_select` too.
    fn fire_stop_select(&mut self, events: Vec<ErlDrvEvent>) {
        if events.is_empty() || self.lib.entry().stop_select.is_none() {
            return;
        }
        let abi_ptr: *mut AbiHost = &mut *self.abi;
        let lib = &self.lib;
        // SAFETY: `abi_ptr` is the address of the boxed `AbiHost` field (see `DriverPort::call`),
        // live for the whole of this call; the closure below reads only `self.lib` (a disjoint
        // field), so nothing re-borrows `*abi_ptr` through it, and the pointer is retired when
        // `with_current_host` returns.
        let host: &mut dyn DriverHost = unsafe { &mut *abi_ptr };
        beam_abi::with_current_host(host, || {
            for event in events {
                // Each `event` is a value the driver itself handed to `driver_select` (or its fd),
                // so it is exactly the operand the C hands back. The `Result` is only the absent-
                // slot report, already excluded above.
                let _ = lib.stop_select(event, core::ptr::null_mut());
            }
        });
    }

    /// Fires the `stop_select` callbacks queued by a `driver_select(..., 0)` during the driver
    /// callback that just returned.
    ///
    /// The C likewise calls `stop_select` after releasing the fd lock (`erl_check_io.c:1210-1216`),
    /// never re-entrantly into the driver's own `driver_select`.
    fn drain_queued_stop_select(&mut self) {
        let queued = self.abi.state_mut().take_queued_stop_select();
        self.fire_stop_select(queued);
    }

    /// Releases every `driver_select` registration still in force when the port goes away.
    ///
    /// A `ERL_DRV_USE` registration still selected at teardown owes its owner the same
    /// `stop_select` callback the deselect path fires; the C passes `(ErlDrvEvent) fd` there
    /// (`erl_check_io.c:2232-2238`), the fd cast to a pointer rather than the operand the driver
    /// originally selected with (they coincide for the fd-cast convention every driver uses).
    ///
    /// The poller registration is withdrawn too when a [`DriverContext`] is at hand — the ordinary
    /// [`Driver::stop`] path — so a closed port leaves the reactor clean; [`Drop`] has no context
    /// and can only run the callback (the registration then dies with the poller).
    fn release_selects_on_teardown(&mut self, mut ctx: Option<&mut DriverContext<'_>>) {
        let selects = self.abi.state_mut().take_all_selected();
        // A release a *deselect* queued and that no `call`/`poll_async` has drained yet is owed
        // too: teardown is the last moment at which it can be delivered, so the queued events ride
        // along with the still-selected ones (queued first, their deselect happened earlier).
        let mut events = self.abi.state_mut().take_queued_stop_select();
        for (fd, token, owes_stop_select) in selects {
            if let Some(ctx) = ctx.as_mut() {
                let _ = ctx.deregister(fd, token);
            }
            if owes_stop_select {
                events.push(fd as usize as ErlDrvEvent);
            }
        }
        self.fire_stop_select(events);
    }

    /// Hands `bytes` to the driver's `outputv` when the entry declares one, else to its `output`.
    ///
    /// This is the C's own preference: `if (drv->outputv) { … } else { … drv->output … }`
    /// (`io.c:1782` on the `port_command` path, `io.c:1597` on the async one). An entry that
    /// declares **both** callbacks therefore gets the vector one, and an entry that declares only
    /// `outputv` is not left without any port-command callback at all — the divergence
    /// `GAPS.md` §"A.4" records (an `outputv`-only entry never receiving `port_command/2`).
    ///
    /// The vector mirrors the C's build (`io.c:1842-1864`): `iov[0]` is the empty **header** block
    /// a driver may fill in (`iov_base = NULL`, `iov_len = 0`), `iov[1]` the data, `binv` runs
    /// parallel to `iov` (all `NULL` — these bytes are not backed by an `ErlDrvBinary`, the same
    /// convention `beam_abi`'s own queue views use), `vsize` counts the header plus the data block
    /// and `size` is the data's total byte count.
    ///
    /// The buffer is passed mutably because the C hands the driver the caller's own bytes (a driver
    /// may write into them in place, e.g. to upper-case them). Nothing re-reads it afterwards: the
    /// drained bytes are counted as consumed (the `output`/`outputv`/`ready_output` callbacks return
    /// `void`).
    fn write_driver(&mut self, ctx: &mut DriverContext<'_>, bytes: &mut [u8]) {
        // `iov`/`binv` live on this stack frame and are the driver's to read (and, for the header
        // block, to write) for the duration of the callback only — exactly the C's contract, where
        // the vector is built on the caller's stack (`ev.common.iov = iv`, `io.c:1806`).
        let prefers_outputv = self.lib.entry().outputv.is_some();
        self.call(ctx, |port, _handle| {
            if prefers_outputv {
                let mut iov = [
                    SysIOVec {
                        iov_base: core::ptr::null_mut(),
                        iov_len: 0,
                    },
                    SysIOVec {
                        iov_base: bytes.as_mut_ptr() as *mut c_void,
                        iov_len: bytes.len(),
                    },
                ];
                let mut binv: [*mut ErlDrvBinary; 2] = [core::ptr::null_mut(); 2];
                let mut ev = ErlIOVec {
                    vsize: iov.len() as c_int,
                    size: bytes.len() as ErlDrvSizeT,
                    iov: iov.as_mut_ptr(),
                    binv: binv.as_mut_ptr(),
                };
                let _ = port.lib.outputv(port.data, &mut ev);
            } else {
                let _ = port.lib.output(
                    port.data,
                    bytes.as_mut_ptr() as *mut c_char,
                    bytes.len() as ErlDrvSizeT,
                );
            }
        });
    }

    /// Runs the driver's `ready_async` for each finished `driver_async` job, or its `async_free`
    /// when the entry has no `ready_async`.
    ///
    /// Returns how many jobs were processed. The runtime calls this once per port-task round (there
    /// is no [`PortTaskKind`](crate::PortTaskKind) variant for async work in M1 — a
    /// `PortTaskKind::Async` is a documented follow-up), so a port with pending jobs makes progress
    /// on the next round.
    ///
    /// `ready_async` receives **no port**, matching `erl_driver.h` (`void ready_async(ErlDrvData,
    /// ErlDrvThreadData)`) — a driver that needs its port during `ready_async` uses the handle it
    /// stashed in `start`, which is subject to the M1 limitation in the module docs (it is in fact
    /// still valid here, because the callback runs inside a `DriverPort` method with the context
    /// installed).
    ///
    /// # Errors
    ///
    /// Always `Ok`: the C's `ready_async`/`async_free` are `void` callbacks.
    pub fn poll_async(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let mut jobs = self.abi.state_mut().jobs.drain();
        // The C runs a port's async completions in queue order; the channel delivers in completion
        // order, so sorting by the `driver_async` queue position reproduces the queue order exactly
        // (and makes the callback order deterministic under concurrency).
        jobs.sort_by_key(|job| job.id());
        let mut processed = 0usize;
        for mut job in jobs {
            if self.lib.entry().ready_async.is_some() {
                let thread_data = job.data() as ErlDrvThreadData;
                self.call(&mut *ctx, |port, _port_handle| {
                    let _ = port.lib.ready_async(port.data, thread_data);
                });
                // `ready_async` owns `async_data` from here (the C's `need_free = 0`,
                // `io.c:5399-5400`), so this completion must not release it: disarming it *before*
                // it drops is what keeps the release exactly-once.
                job.handed_to_ready_async();
            } else {
                // No `ready_async` on the entry: the release is this port's (the C's `need_free`
                // path, `io.c:5406`). `async_free` carries no port, so it is one of the driver entry
                // points that does not go through `DriverPort::call`; the host is published here for
                // the same reason `call` publishes it — a driver may call the no-port registry slots
                // (`add_driver_entry`/`remove_driver_entry`) from any of its callbacks.
                let abi_ptr: *mut AbiHost = &mut *self.abi;
                // SAFETY: see `DriverPort::call` — the published `&mut` reaches only the
                // `AbiHost`'s interior-mutable state and is retired before this iteration's borrow
                // ends; the closure touches only the completion (`job`), which is disjoint from the
                // host.
                let host: &mut dyn DriverHost = unsafe { &mut *abi_ptr };
                beam_abi::with_current_host(host, || job.release());
            }
            processed += 1;
        }
        // An `async_free` runs outside `call`, so a `driver_select(..., 0)` it made queued its
        // release without a `call` to drain it (see `DriverPort::fire_stop_select`).
        self.drain_queued_stop_select();
        Ok(processed)
    }
}

/// Clears an [`AbiHost`]'s installed port context when a driver callback returns (or unwinds).
///
/// Holds a **raw** pointer rather than a `&AbiHost`: [`DriverPort::call`] publishes the host as a
/// `&mut dyn DriverHost` for the callback's dynamic extent, which a live shared borrow of the host
/// would forbid — so the guard deliberately keeps no borrow, only the address of the boxed host the
/// port owns.
struct CtxGuard(*mut AbiHost);

impl Drop for CtxGuard {
    fn drop(&mut self) {
        // SAFETY: the pointer is the address of the `AbiHost` this guard was created from in
        // `DriverPort::call`; that host is a `Box` owned by the live `DriverPort`, so it is valid
        // for the guard's whole life (which is bounded by the same method), and `clear` only writes
        // the host's own `Cell` slot.
        unsafe { (*self.0).clear() };
    }
}

/// The [`BeamError`] for a failed driver load: a raise whose reason is the oracle's atom.
///
/// `erlang:load_driver/2` answers `{error, Reason}` with one of the atoms in `errcode_tab`
/// (`erl_bif_ddll.c:1709-1720`); carrying it as the raise reason keeps the atom observable.
fn load_failure(err: beam_abi::driver::loader::DriverLoadError) -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom(err.reason_atom())))
}

/// Maps an `ErlDrvEntry`'s `driver_flags` bitfield onto this crate's [`DriverFlags`].
///
/// Only `ERL_DRV_FLAG_SOFT_BUSY` has a counterpart here ([`DriverFlags::SOFT_BUSY`]);
/// `ERL_DRV_FLAG_USE_PORT_LOCKING`, `ERL_DRV_FLAG_NO_BUSY_MSGQ` and `ERL_DRV_FLAG_USE_INIT_ACK`
/// describe emulator machinery (`beam_io` has no port locking, no busy message-queue limits and no
/// `erl_drv_init_ack` yet — see the crate `CONTEXT.md`) and are dropped rather than misrepresented.
fn driver_flags(bits: c_int) -> DriverFlags {
    let mut flags = DriverFlags::empty();
    if bits & ERL_DRV_FLAG_SOFT_BUSY != 0 {
        flags = flags.union(DriverFlags::SOFT_BUSY);
    }
    flags
}

/// Whether a `start` callback's return value is one of the C's three error sentinels.
fn is_start_failure(data: ErlDrvData) -> bool {
    let raw = data as isize;
    raw == ERL_DRV_ERROR_GENERAL as isize
        || raw == ERL_DRV_ERROR_ERRNO as isize
        || raw == ERL_DRV_ERROR_BADARG as isize
}

/// The `ErlDrvEvent` for `fd` (the C convention: the fd cast to a pointer), or null when the driver
/// has no fd selected in the direction being dispatched.
fn fd_to_event(fd: Option<RawFd>) -> ErlDrvEvent {
    match fd {
        Some(fd) => fd as usize as ErlDrvEvent,
        None => core::ptr::null_mut(),
    }
}

/// A [`DriverFactory`](crate::DriverFactory)-style constructor for a **loaded** driver `.so`.
///
/// # Built-in drivers and loaded `.so`s are the same `Box<dyn Driver>`
///
/// This is the whole unification: a built-in driver ([`FdDriver`](crate::FdDriver) &c.) implements
/// [`Driver`](crate::driver::Driver) natively and is produced by
/// [`DriverRegistry::create`](crate::DriverRegistry::create), while a C driver `.so` is loaded here
/// and produces a [`DriverPort`] — and *both* come back as `Box<dyn Driver>` that opens through the
/// same [`PortRegistry::open`](crate::PortRegistry::open). Nothing downstream of the port needs to
/// know which kind it is: the poller, the byte queues, the port tasks and the notifications are the
/// same for either.
///
/// It cannot be a [`DriverFactory`](crate::DriverFactory) (a `fn()` pointer) because a loaded driver
/// needs a path, a name, a command and a [`PortHost`] — the same reason
/// [`SpawnDriver`](crate::SpawnDriver) is constructed directly rather than registered.
///
/// # Errors
///
/// Whatever [`DriverPort::load`] returns.
pub fn make_loaded(
    path: &str,
    name: &str,
    command: &str,
    host: Box<dyn PortHost>,
) -> Result<Box<dyn Driver>> {
    Ok(Box::new(DriverPort::load(path, name, command, host)?))
}

impl Driver for DriverPort {
    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn flags(&self) -> DriverFlags {
        driver_flags(self.lib.entry().driver_flags)
    }

    /// `start`: hand the driver the port handle and the command string, and keep its `ErlDrvData`.
    ///
    /// The command pointer is cast to `char*` because that is the frozen signature
    /// (`ErlDrvData start(ErlDrvPort, char*)`); the driver must treat it as read-only (the C's
    /// `open_port` command string is likewise the driver's input, not its scratch space).
    ///
    /// # Errors
    ///
    /// * `BeamError::BadArg` when the entry has no `start` slot — the C's `init_driver` requires
    ///   one (`de->start` is called unconditionally, `io.c:7559-7621`).
    /// * `BeamError::BadArg` for the three sentinels the C uses to fail an `open_port/2`:
    ///   `ERL_DRV_ERROR_GENERAL` (`{'EXIT',{einval,Where}}`), `ERL_DRV_ERROR_ERRNO`
    ///   (`{'EXIT',{Errno,Where}}`) and `ERL_DRV_ERROR_BADARG` (`{'EXIT',{badarg,Where}}`). The C
    ///   distinguishes them by rendering the reason; that rendering belongs to the runtime, so all
    ///   three collapse to `badarg` here and the distinction is documented rather than invented.
    fn start(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        let started = self.call(ctx, |port, handle| {
            port.lib
                .start(handle, port.command.as_ptr() as *mut c_char)
        });
        match started {
            Ok(data) if !is_start_failure(data) => {
                self.data = data;
                Ok(())
            }
            // A sentinel `Ok` (see above) or an absent `start` slot both fail the open.
            Ok(_) | Err(_) => Err(error::badarg()),
        }
    }

    /// `output`: drain the port's output queue and hand the bytes to the driver.
    ///
    /// The C callback returns `void`, so the whole drained buffer is counted as written (see the
    /// module docs). An absent `output` slot is the C's logging `no_output_callback`
    /// (`io.c:7516-7520`) when the entry has no `outputv` either: the bytes are still counted as
    /// consumed, exactly as the C's default callback lets them vanish. When the entry declares
    /// `outputv`, that slot is preferred (see [`DriverPort::write_driver`]).
    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let mut buf = ctx.output().front().to_vec();
        let len = buf.len();
        ctx.output().consume(len);
        self.write_driver(ctx, &mut buf);
        Ok(len)
    }

    /// `ready_input`: tell the driver its selected fd is readable.
    ///
    /// The fd comes from the driver's own `driver_select` bookkeeping (see the module docs); with
    /// none selected the callback receives a null event, and an absent `ready_input` slot is the
    /// C's `no_ready_input_callback` no-op (`io.c:7516`).
    fn ready_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        let event = fd_to_event(self.abi.state().fd_armed(true));
        self.call(ctx, |port, _handle| {
            let _ = port.lib.ready_input(port.data, event);
        });
        Ok(())
    }

    /// `ready_output`: drain the port's output queue to the driver **and** report write-readiness.
    ///
    /// The drained bytes always go down this crate's single write path,
    /// [`DriverPort::write_driver`] — the C's `port_command` → `output`/`outputv` delivery
    /// (`io.c:1782`/`io.c:1517`) — so a queued command reaches the driver whichever callback slots
    /// its entry declares. When the entry also declares a `ready_output` slot, that slot then runs
    /// with the write-armed fd as the event: it is the C's write-**readiness** notification (the
    /// `ERTS_PORT_TASK_OUTPUT` arm, `erl_port_task.c:1793-1797`), not a second write path, so an
    /// entry declaring both callbacks sees the C's combined effect (bytes via `output`/`outputv`,
    /// then the notification).
    ///
    /// A slot-absent entry is unaffected: [`DriverPort::write_driver`] routes to the entry's
    /// `outputv`/`output`, and an entry declaring neither gets the C's logging `no_output_callback`
    /// (`io.c:7516-7520`), which writes nothing — the bytes still count as written (the C callbacks
    /// return `void`).
    fn ready_output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let mut buf = ctx.output().front().to_vec();
        let len = buf.len();
        ctx.output().consume(len);
        // Read the write-armed fd *before* the write: the notification reports the readiness the
        // task was scheduled for, not whatever the write callback may re-arm (`output`/`outputv`
        // are free to call `driver_select`).
        let event = fd_to_event(self.abi.state().fd_armed(false));
        self.write_driver(ctx, &mut buf);
        if self.lib.entry().ready_output.is_some() {
            self.call(ctx, |port, _handle| {
                let _ = port.lib.ready_output(port.data, event);
            });
        }
        Ok(len)
    }

    /// `control`: the driver's numeric "ioctl" (`port_control/3`).
    ///
    /// `command` is parsed as a decimal `c_uint` — a C driver's verbs are integer codes
    /// (`INET_REQ_OPEN`, `TCP_REQ_SEND`, …), not this crate's built-in string verbs. The reply is
    /// the `cres` bytes at the pointer the driver returned.
    ///
    /// # Errors
    ///
    /// * `BeamError::BadArg` when `command` is not a decimal `c_uint`.
    /// * `BeamError::BadArg` when the entry has no `control` slot (the C's
    ///   `!prt->drv_ptr->control ⇒ BADARG`, `io.c:4035`) or when the driver returned a negative
    ///   result (`io.c:4074`).
    ///
    /// The driver may re-point the reply pointer at its own buffer and must keep it valid until the
    /// next `control` call (the C contract). That is a trust boundary: a driver is native code the
    /// runtime loaded, and a reply larger than the buffer handed in is that driver's bug.
    fn control(
        &mut self,
        ctx: &mut DriverContext<'_>,
        command: &str,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        let cmd = command.parse::<c_uint>().map_err(|_| error::badarg())?;
        let mut argbuf = arg.to_vec();
        let mut reply = vec![0u8; CONTROL_REPLY_CAPACITY];
        let mut rbuf = reply.as_mut_ptr() as *mut c_char;
        let rlen = reply.len() as ErlDrvSizeT;
        let result = self.call(ctx, |port, _handle| {
            port.lib.control(
                port.data,
                cmd,
                argbuf.as_mut_ptr() as *mut c_char,
                argbuf.len() as ErlDrvSizeT,
                &mut rbuf,
                rlen,
            )
        });
        let cres = match result {
            Ok(cres) if cres >= 0 => cres,
            _ => return Err(error::badarg()),
        };
        let n = cres as usize;
        if n == 0 {
            return Ok(Vec::new());
        }
        if rbuf.is_null() {
            return Err(error::badarg());
        }
        // SAFETY: the driver reported `n` readable bytes at `rbuf` — either the buffer handed in
        // (still alive: `reply` is a local here) or the driver's own memory, which the C `control`
        // contract keeps valid until the next `control` call. `rbuf` is non-null (checked above).
        let bytes = unsafe { core::slice::from_raw_parts(rbuf as *const u8, n) }.to_vec();
        Ok(bytes)
    }

    /// `ready_async`/`async_free`: hand the driver its finished `driver_async` jobs.
    ///
    /// This is the `Driver::drive_async` drive point over the C entry's async slots; the work
    /// lives in [`DriverPort::poll_async`] (which the runtime may also call directly).
    fn drive_async(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        self.poll_async(ctx)
    }

    /// Whether the driver has an outstanding `driver_async` job.
    ///
    /// Delegates to [`AbiHost::has_pending_async`]: `true` while a job is running or has completed
    /// but not yet had its `ready_async`/`async_free` run, so the port-task round knows this port
    /// needs its [`Driver::drive_async`] = [`DriverPort::poll_async`] point.
    fn has_pending_async(&self) -> bool {
        self.abi.has_pending_async()
    }

    /// The `driver_set_timer` deadline, as the port's next `TIMEOUT`.
    fn next_timeout(&self) -> Option<Instant> {
        self.abi.state().deadline()
    }

    /// `timeout`: the port timer fired; the driver's deadline is consumed.
    ///
    /// Clearing the deadline here is what makes [`crate::Port::refresh_timeout`] drop the port's
    /// `TIMEOUT` task: a driver re-arms by calling `driver_set_timer` again during the callback
    /// (which sets a new deadline), and a driver that does not gets no further timer.
    fn timeout(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        self.call(ctx, |port, _handle| {
            let _ = port.lib.timeout(port.data);
        });
        self.abi.state_mut().deadline = None;
        Ok(())
    }

    /// The port is exiting: notify the driver.
    ///
    /// The C has no port-exit callback, so this uses the closest slots the entry has: `process_exit`
    /// (the C's "the process you monitor exited" callback) when present, else `emergency_close`
    /// (the C's "the port is being closed abruptly" callback), else nothing. Unlike the C's
    /// `process_exit` the monitor argument is null — the port carries no `ErlDrvMonitor` yet.
    fn port_exit(&mut self, ctx: &mut DriverContext<'_>, reason: PortExitReason) -> Result<()> {
        let _ = reason; // M1 does not expose the exit reason to a C driver.
        if self.lib.entry().process_exit.is_some() {
            self.call(ctx, |port, _handle| {
                let _ = port
                    .lib
                    .process_exit(port.data, core::ptr::null_mut::<ErlDrvMonitor>());
            });
        } else if self.lib.entry().emergency_close.is_some() {
            self.call(ctx, |port, _handle| {
                let _ = port.lib.emergency_close(port.data);
            });
        }
        Ok(())
    }

    /// A C driver's child reaping is a handoff: `beam_io` cannot wait on a driver-private child, so
    /// this always reports "no status" (the inherited default, documented here).
    fn take_exit_status(&mut self) -> Result<Option<i32>> {
        Ok(None)
    }

    /// `stop`: release the driver's resources.
    ///
    /// An absent `stop` slot is the C's silent no-op (`if (drv->stop) …`). The library itself is
    /// unloaded only when the [`DriverPort`] drops (its `finish` callback then runs), matching the
    /// C's split between closing a port and unloading a driver.
    ///
    /// A `driver_select`ion the driver did **not** de-select is released here, exactly as the C's
    /// port teardown releases it (`erl_check_io.c:2232-2238`): the poller registration is withdrawn
    /// and an `ERL_DRV_USE` one owes its owner a final `stop_select`.
    fn stop(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        self.call(ctx, |port, _handle| {
            let _ = port.lib.stop(port.data);
        });
        self.release_selects_on_teardown(Some(ctx));
        Ok(())
    }
}

/// A port's last chance to release its `driver_select` registrations and its undelivered
/// `driver_async` completions.
///
/// The ordinary teardown is [`Driver::stop`], which has a [`DriverContext`] and so can also
/// withdraw the poller registrations. This catches a [`DriverPort`] dropped **without** `stop`
/// running — a port whose `start` failed, or a whole [`PortRegistry`](crate::PortRegistry) dropped
/// while ports are still live — and fires the `stop_select` callbacks such a port still owes, so a
/// driver is never left believing it still owns a handle the runtime has discarded. There is no
/// context here, so only the callback runs (the poller registration dies with the poller).
impl Drop for DriverPort {
    fn drop(&mut self) {
        // Release the `driver_async` jobs the port never delivered **first**, and inside this
        // `drop` rather than after it: each completion's `async_free` is a driver callback, so it
        // must run while the loaded library is still mapped (this struct's `lib` field, dropped
        // after `drop` returns) — and before the teardown below, keeping the drop order a
        // `stop_select`-observing test pins. A completion still queued is released here; a job
        // whose worker is still running sees the channel close and releases its own data, the C's
        // port-gone branch (`erl_async.c:324-330`). Either way each `async_free` runs exactly once.
        drop(self.abi.state_mut().jobs.drain());
        self.release_selects_on_teardown(None);
    }
}

#[cfg(test)]
mod tests {
    use super::{driver_flags, is_start_failure, load_failure, make_loaded, DriverPort};
    use crate::abi::LocalAtomPortHost;
    use crate::driver::{Driver, DriverFlags, DriverRegistry, PortExitReason};
    use crate::poller::Poller;
    use crate::registry::PortRegistry;
    use beam_abi::driver::types::{
        ERL_DRV_ERROR_BADARG, ERL_DRV_ERROR_ERRNO, ERL_DRV_ERROR_GENERAL, ERL_DRV_FLAG_SOFT_BUSY,
    };
    use beam_common::BeamError;

    /// A path that cannot possibly hold a driver — the loader must answer a typed error.
    fn missing_library_path() -> String {
        format!(
            "/nonexistent/beam_io-driver-host-layer-{}.so",
            std::process::id()
        )
    }

    #[test]
    fn loading_a_missing_shared_object_is_a_typed_raise() {
        let err = DriverPort::load(
            &missing_library_path(),
            "host_driver",
            "cmd",
            Box::new(LocalAtomPortHost::new()),
        )
        .err()
        .expect("a nonexistent driver object must not load");
        match err {
            BeamError::Raise(raise) => {
                assert_eq!(raise.reason, beam_common::ExitReason::atom("unspecified_error"));
            }
            other => panic!("a load failure must be a raise, got {other:?}"),
        }
    }

    #[test]
    fn an_interior_nul_in_the_command_is_a_badarg() {
        let err = DriverPort::load(
            &missing_library_path(),
            "host_driver",
            "bad\0command",
            Box::new(LocalAtomPortHost::new()),
        )
        .err()
        .expect("a command with an interior NUL cannot be a C string");
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");
    }

    #[test]
    fn a_load_failure_carries_the_oracle_reason_atom() {
        let err = load_failure(beam_abi::driver::loader::DriverLoadError::NoDriverInit(
            "no driver_init".to_string(),
        ));
        assert_eq!(err.as_atom(), Some("no_driver_init"));
    }

    #[test]
    fn the_driver_flag_bitfield_maps_soft_busy_only() {
        // `DriverFlags` has no `Debug`, so compare with `==` rather than `assert_eq!`.
        assert!(driver_flags(0) == DriverFlags::empty());
        assert!(driver_flags(ERL_DRV_FLAG_SOFT_BUSY) == DriverFlags::SOFT_BUSY);
        // Every other `ERL_DRV_FLAG_*` has no counterpart here and is dropped.
        assert!(driver_flags(1 | 4 | 8) == DriverFlags::empty());
        assert!(driver_flags(1 | ERL_DRV_FLAG_SOFT_BUSY | 8) == DriverFlags::SOFT_BUSY);
    }

    #[test]
    fn only_the_three_start_sentinels_are_failures() {
        assert!(is_start_failure(ERL_DRV_ERROR_GENERAL));
        assert!(is_start_failure(ERL_DRV_ERROR_ERRNO));
        assert!(is_start_failure(ERL_DRV_ERROR_BADARG));
        assert!(!is_start_failure(core::ptr::null_mut()));
        let data = 1usize as beam_abi::driver::types::ErlDrvData;
        assert!(!is_start_failure(data));
    }

    /// The `driver_*` symbols must reach the link of a test binary that loads a driver `.so`
    /// (`build.rs` exports them with `-Wl,--export-dynamic`); referencing the populated table here
    /// is what pins that.
    #[test]
    fn the_driver_api_table_is_reachable_from_the_abi_layer() {
        let table = beam_abi::driver::drv::driver_api_table();
        assert!(table.driver_output.is_some());
        assert!(table.driver_select.is_some());
        assert!(table.driver_async.is_some());
    }

    /// A driver the port can open with no arguments — the built-in `"fd"` driver's shape.
    fn open_builtin() -> (PortRegistry, Poller, beam_process::PortId) {
        let mut poller = Poller::new().expect("a poll set");
        let mut ports = PortRegistry::new();
        let registry = DriverRegistry::with_builtins();
        let id = ports
            .open_named(&mut poller, &registry, "fd")
            .expect("the built-in fd driver opens");
        (ports, poller, id)
    }

    #[test]
    fn a_built_in_and_a_loaded_driver_are_the_same_boxed_driver() {
        // The built-in half: the registry hands back a `Box<dyn Driver>` whose name is "fd".
        let builtin = DriverRegistry::with_builtins()
            .create("fd")
            .expect("the fd driver exists");
        assert_eq!(builtin.name(), "fd");

        // Built-in drivers open through the ordinary port path.
        let (ports, _poller, id) = open_builtin();
        assert_eq!(
            ports.get(id).map(|port| port.driver_name().to_string()),
            Some("fd".to_string())
        );

        // The loaded half presents as the *same* `Box<dyn Driver>` type through `make_loaded`;
        // a bad path is a typed error before any port exists (no `.so` needed for this half).
        let loaded = make_loaded(
            &missing_library_path(),
            "host_driver",
            "cmd",
            Box::new(LocalAtomPortHost::new()),
        );
        assert!(loaded.is_err(), "a missing object must not produce a driver");
        let loaded: Result<Box<dyn Driver>, BeamError> = make_loaded(
            "/nonexistent/another.so",
            "host_driver",
            "cmd",
            Box::new(LocalAtomPortHost::new()),
        );
        assert!(matches!(loaded, Err(BeamError::Raise(_))));
    }

    /// The port-exit and timer seams are wired but must not be exercised against a real `.so` here;
    /// this pins that the adapter's optional hooks are reachable through the `Driver` trait.
    #[test]
    fn the_adapter_exposes_the_optional_driver_hooks() {
        fn assert_driver<D: Driver>() {}
        assert_driver::<DriverPort>();

        // `PortExitReason` is the vocabulary `DriverPort::port_exit` accepts, and the defaulted
        // `Driver` hooks (flags/next_timeout) are reachable through the trait.
        let _ = PortExitReason::Closed;
        let _: fn(&DriverPort) -> DriverFlags = <DriverPort as Driver>::flags;
        let _: fn(&DriverPort) -> Option<std::time::Instant> = <DriverPort as Driver>::next_timeout;
    }
}
