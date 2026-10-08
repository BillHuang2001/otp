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

//! The dynamic driver loader — `dlopen`/`dlsym` + the `ErlDrvEntry` load contract.
//!
//! This is the runtime half of contract **A.4** (the driver C ABI): it reproduces what
//! `erlang:load_driver/2` (`erl_ddll_try_load_3`) does between a path/name pair and a usable
//! [`ErlDrvEntry`] — open the shared object, resolve `driver_init`, validate the returned entry in
//! the oracle's **exact order** with its exact reason atoms and diagnostic texts, and expose the
//! entry's callbacks through safe wrappers.
//!
//! Ground truth: `do_load_driver_entry` (`erts/emulator/beam/erl_bif_ddll.c:1466-1533`) — `dlopen`,
//! the `driver_init` lookup (`erts_sys_ddll_load_driver_init`,
//! `erts/emulator/sys/unix/erl_unix_sys_ddll.c:216-228`), the marker/version/name checks and the
//! `init` callback — and `init_driver` (`erts/emulator/beam/io.c:7559-7621`).
//!
//! # Error surface (matches the oracle's `{error,Reason}`)
//!
//! Every failure is a [`DriverLoadError`] carrying both the reason atom
//! ([`DriverLoadError::reason_atom`]) and a diagnostic text ([`DriverLoadError::text`]). The reason
//! atoms come from the `errcode_tab` (`erl_bif_ddll.c:1709-1720`); the texts from the matching
//! `erts_ddll_error` strings (`erl_bif_ddll.c:1201-1224`).
//!
//! | oracle check | reason atom | variant |
//! |---|---|---|
//! | `dlopen` fails | (dynamic `open_error` code) | [`DriverLoadError::Unspecified`] |
//! | `dlsym "driver_init"`/`"_driver_init"` fails | `no_driver_init` | [`DriverLoadError::NoDriverInit`] |
//! | `driver_init()` → `NULL` | `driver_init_failed` | [`DriverLoadError::DriverInitFailed`] |
//! | marker missing / version out of range | `driver_incorrect_version` | [`DriverLoadError::DriverIncorrectVersion`] |
//! | `driver_name` mismatch | `bad_driver_name` | [`DriverLoadError::BadDriverName`] |
//! | `init` callback veto | `driver_init_failed` | [`DriverLoadError::DriverInitFailed`] |
//!
//! [`DriverLoadError::SymbolNotFound`] rounds out the reason surface (`symbol_not_found` is in the
//! oracle's `errcode_tab`) but is **never produced by [`load`]**: a missing `driver_init` maps to
//! `no_driver_init` (`ERL_DE_LOAD_ERROR_NO_INIT`, `erl_bif_ddll.c:1478-1482`), exactly as in the C.
//! This mirrors [`NifLoadError`](crate::nif::loader::NifLoadError)'s never-produced variants.
//!
//! # The C's default callbacks (`init_driver`, `io.c:7559-7621`)
//!
//! The C never calls a `NULL` entry slot. For `output`/`ready_input`/`ready_output`/`timeout`/
//! `stop_select` it substitutes a logging no-op (`no_output_callback` &c., `io.c:7520-7553`); every
//! other slot it null-checks before the call (`if (drv->stop) …`). The [`DriverLibrary`] wrappers
//! report that case as [`DriverCallError::AbsentCallback`] rather than hiding it, so the caller
//! applies whichever of the two C behaviours matches the slot it drives.
//!
//! # Documented deviations from the C
//!
//! 1. **The `.so` suffix is appended only when absent** (the C appends `FILE_EXT` before `dlopen`),
//!    the same rule [`crate::nif::loader`] uses.
//! 2. **A `dlopen` failure lands in [`DriverLoadError::Unspecified`], carrying the dynamic loader's
//!    own `dlerror` text.** The C returns a `dynamic_error`-offset code with no atom in
//!    `errcode_tab`, rendered as `{open_error, N}`; that shape has no variant here.
//! 3. **A `NULL` `driver_name` is a typed [`DriverLoadError::BadDriverName`]**; the C's
//!    `sys_strcmp(name, dp->driver_name)` would fault on it.
//! 4. **No code server / driver list is consulted.** The C's global driver list, per-handle
//!    reference counts and reload/pending protocol are all `DE_Handle` state; [`load`] performs only
//!    the *entry* validation, leaving registration and reload policy to the runtime.
//!
//! # Ownership
//!
//! [`DriverLibrary`] owns the `dlopen` handle: its `Drop` invokes the entry's `finish` callback and
//! then `dlclose`s, in the C's `do_unload_driver_entry` order (`erl_bif_ddll.c:1559-1564`). The
//! private `dl_last_error`/`dl_sym`/`OpenHandle`/`read_cstring`/`shared_object_path` helpers
//! deliberately mirror [`crate::nif::loader`]'s: both loaders speak to the same `libc` loader, and
//! neither module's helpers are visible outside their own file.
use std::ffi::{CStr, CString};
use std::fmt;

use libc::{c_char, c_int, c_uint, c_void};

use crate::driver::types::{
    ErlDrvData, ErlDrvEntry, ErlDrvEvent, ErlDrvMonitor, ErlDrvPort, ErlDrvSSizeT, ErlDrvSizeT,
    ErlDrvThreadData, ErlIOVec,
};
use crate::{
    ERL_DRV_EXTENDED_MAJOR_VERSION, ERL_DRV_EXTENDED_MARKER, ERL_DRV_EXTENDED_MINOR_VERSION,
    ERL_DRV_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD,
};

/// The shared-object suffix the oracle appends (`erl_unix_sys_ddll.c:49`, `FILE_EXT`).
const SHARED_OBJECT_SUFFIX: &str = ".so";

// The `erts_ddll_error` texts (`erl_bif_ddll.c:1201-1224`): the oracle's diagnostics for the reason
// atoms our variants carry.
const NO_DRIVER_INIT: &str = "No driver init in dynamic library";
const DRIVER_INIT_FAILED: &str = "Driver init failed";
const BAD_DRIVER_NAME: &str = "Bad driver name";
const DRIVER_INCORRECT_VERSION: &str = "Driver compiled with incorrect version of erl_driver.h";
const UNSPECIFIED_ERROR: &str = "Unspecified dynamic library error";

/// The C signature of `driver_init`: `ErlDrvEntry* driver_init(void)`.
type DriverInitFn = unsafe extern "C" fn() -> *mut ErlDrvEntry;

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// A failure of [`load`], mirroring the oracle's `{error,Reason}`.
///
/// The payload is a diagnostic text (the oracle's `erts_ddll_error` message, or the dynamic
/// loader's own description); [`DriverLoadError::reason_atom`] is the reason atom.
#[derive(Debug)]
pub enum DriverLoadError {
    /// `no_driver_init` — the object exports neither `driver_init` nor `_driver_init`.
    NoDriverInit(String),
    /// `driver_init_failed` — `driver_init()` returned `NULL`, or the entry's `init` callback
    /// vetoed the load with a non-zero result.
    DriverInitFailed(String),
    /// `bad_driver_name` — the entry's `driver_name` is `NULL` or does not match the requested name.
    BadDriverName(String),
    /// `driver_incorrect_version` — the entry lacks `ERL_DRV_EXTENDED_MARKER`, or its
    /// `major_version`/`minor_version` are outside the range this runtime accepts.
    DriverIncorrectVersion(String),
    /// `symbol_not_found` — a symbol lookup failed (never produced by [`load`], which maps a missing
    /// `driver_init` to [`DriverLoadError::NoDriverInit`]); oracle text `"Symbol not found in
    /// dynamic library"`.
    SymbolNotFound(String),
    /// `unspecified_error` — the object could not be opened, or the failure has no more specific
    /// reason atom.
    Unspecified(String),}

impl DriverLoadError {
    /// The reason atom the oracle places in `{error,Reason}` (`erl_bif_ddll.c:1709-1720`).
    pub fn reason_atom(&self) -> &'static str {
        match self {
            DriverLoadError::NoDriverInit(_) => "no_driver_init",
            DriverLoadError::DriverInitFailed(_) => "driver_init_failed",
            DriverLoadError::BadDriverName(_) => "bad_driver_name",
            DriverLoadError::DriverIncorrectVersion(_) => "driver_incorrect_version",
            DriverLoadError::SymbolNotFound(_) => "symbol_not_found",
            DriverLoadError::Unspecified(_) => "unspecified_error",
        }
    }

    /// The diagnostic text describing the failure.
    pub fn text(&self) -> &str {
        match self {
            DriverLoadError::NoDriverInit(text)
            | DriverLoadError::DriverInitFailed(text)
            | DriverLoadError::BadDriverName(text)
            | DriverLoadError::DriverIncorrectVersion(text)
            | DriverLoadError::SymbolNotFound(text)
            | DriverLoadError::Unspecified(text) => text,
        }
    }
}

impl fmt::Display for DriverLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.reason_atom(), self.text())
    }
}

impl std::error::Error for DriverLoadError {}

/// A failure of a [`DriverLibrary`] callback wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverCallError {
    /// The entry leaves this callback `NULL` — the C's default-callback case (see the module docs's
    /// default-callback table). Reporting it lets the caller pick the C's behaviour for that slot: a
    /// logging no-op, or a plain skip.
    AbsentCallback {
        /// The `ErlDrvEntry` field name (`"output"`, `"start"`, …).
        callback: &'static str,
    },
}

impl DriverCallError {
    /// `true` when the failure is just an unimplemented callback (the C's default-callback case).
    pub fn is_absent(&self) -> bool {
        matches!(self, DriverCallError::AbsentCallback { .. })
    }
}

impl fmt::Display for DriverCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriverCallError::AbsentCallback { callback } => {
                write!(f, "driver callback '{callback}' is not implemented")
            }
        }
    }
}

impl std::error::Error for DriverCallError {}

/// `DriverCallError::AbsentCallback` for `callback` — the "entry left the slot `NULL`" case.
fn absent(callback: &'static str) -> DriverCallError {
    DriverCallError::AbsentCallback { callback }
}

// ---------------------------------------------------------------------------------------------
// The loaded driver
// ---------------------------------------------------------------------------------------------

/// A loaded driver: an open `dlopen` handle plus the driver's [`ErlDrvEntry`].
///
/// The entry is static data inside the loaded object, so it stays valid for as long as the handle is
/// open — that is, for as long as `self`.
#[derive(Debug)]
pub struct DriverLibrary {
    /// The `dlopen` handle, closed by [`DriverLibrary::drop`].
    handle: *mut c_void,
    /// The `driver_init` result (live static data inside the loaded object).
    entry: *mut ErlDrvEntry,
    /// The validated `driver_name` (the C's `drv->name`).
    name: String,
}

impl DriverLibrary {
    /// The driver's `ErlDrvEntry`.
    pub fn entry(&self) -> &ErlDrvEntry {
        // SAFETY: `self.entry` is non-null and points at the driver's static `ErlDrvEntry` inside
        // the loaded object, which stays mapped for the lifetime of `&self` (`dlclose` runs only in
        // `Drop`).
        unsafe { &*self.entry }
    }

    /// The driver name the entry declares (already validated against the requested name).
    pub fn name(&self) -> String {
        self.name.clone()
    }

    /// The entry's `init` callback (`int init(void)`), or `None` when the entry has none.
    ///
    /// [`load`] has **already invoked** this callback once (the C's `init_driver` is the load path's
    /// only caller of `init`, `io.c:7610-7620`); this accessor exists for callers that manage the
    /// entry themselves. The C treats a missing `init` as success (`if (!de->init) return 0;`).
    pub fn init(&self) -> Option<c_int> {
        let init = self.entry().init?;
        // SAFETY: `init` is the driver's own `int(void)` callback, invoked while its code is mapped.
        Some(unsafe { init() })
    }

    /// The entry's `finish` callback (`void finish(void)`), invoked before unloading.
    ///
    /// Absence is the C's silent no-op (`if (q->finish) (*q->finish)();`,
    /// `erl_bif_ddll.c:1559-1563`). [`Drop`] invokes the same callback, so calling this explicitly
    /// on a live library runs it a second time.
    pub fn finish(&self) -> Result<(), DriverCallError> {
        // SAFETY: the slot holds the driver's own `void(void)` callback; `finish` runs before the
        // `dlclose` in `Drop`, so its code is still mapped.
        match self.entry().finish {
            Some(cb) => { unsafe { cb() }; Ok(()) }
            None => Err(absent("finish")),
        }
    }

    /// `start(ErlDrvPort, char*)` — `open_port/2` spawned the driver. The returned [`ErlDrvData`] is
    /// handed back verbatim; the C failure signals (`ERL_DRV_ERROR_GENERAL` −1,
    /// `ERL_DRV_ERROR_ERRNO` −2, `ERL_DRV_ERROR_BADARG` −3) are the caller's to interpret.
    pub fn start(&self, port: ErlDrvPort, command: *mut c_char) -> Result<ErlDrvData, DriverCallError> {
        // SAFETY: the slot holds the driver's own `ErlDrvData(ErlDrvPort, char*)` callback.
        match self.entry().start {
            Some(cb) => Ok(unsafe { cb(port, command) }),
            None => Err(absent("start")),
        }
    }

    /// `control(...)` — the driver "ioctl" behind `port_control/3`. Absent ⇒ the C's silent no-op.
    pub fn control(&self, data: ErlDrvData, cmd: c_uint, buf: *mut c_char, len: ErlDrvSizeT,
                   rbuf: *mut *mut c_char, rlen: ErlDrvSizeT) -> Result<ErlDrvSSizeT, DriverCallError> {
        // SAFETY: the slot holds the driver's own `control` callback.
        match self.entry().control {
            Some(cb) => Ok(unsafe { cb(data, cmd, buf, len, rbuf, rlen) }),
            None => Err(absent("control")),
        }
    }

    /// `call(...)` — a synchronous call into the driver. Absent ⇒ the C's silent no-op.
    pub fn call(&self, data: ErlDrvData, cmd: c_uint, buf: *mut c_char, len: ErlDrvSizeT,
                rbuf: *mut *mut c_char, rlen: ErlDrvSizeT, flags: *mut c_uint) -> Result<ErlDrvSSizeT, DriverCallError> {
        // SAFETY: the slot holds the driver's own `call` callback.
        match self.entry().call {
            Some(cb) => Ok(unsafe { cb(data, cmd, buf, len, rbuf, rlen, flags) }),
            None => Err(absent("call")),
        }
    }

    /// `stop` — the port was closed. Absent ⇒ the C's silent no-op.
    pub fn stop(&self, data: ErlDrvData) -> Result<(), DriverCallError> {
        // SAFETY: the slot holds the driver's own `void(ErlDrvData)` callback.
        match self.entry().stop {
            Some(cb) => { unsafe { cb(data) }; Ok(()) }
            None => Err(absent("stop")),
        }
    }

    /// `output` — output from Erlang to the port. Absent ⇒ the C's logging `no_output_callback`
    /// (`io.c:7520-7529`).
    pub fn output(&self, data: ErlDrvData, buf: *mut c_char, len: ErlDrvSizeT) -> Result<(), DriverCallError> {
        // SAFETY: the slot holds the driver's own `void(ErlDrvData, char*, ErlDrvSizeT)` callback.
        match self.entry().output {
            Some(cb) => { unsafe { cb(data, buf, len) }; Ok(()) }
            None => Err(absent("output")),
        }
    }

    /// `outputv` — the vector form of `output`. Absent ⇒ the C's silent no-op.
    pub fn outputv(&self, data: ErlDrvData, ev: *mut ErlIOVec) -> Result<(), DriverCallError> {
        // SAFETY: the slot holds the driver's own `void(ErlDrvData, ErlIOVec*)` callback.
        match self.entry().outputv {
            Some(cb) => { unsafe { cb(data, ev) }; Ok(()) }
            None => Err(absent("outputv")),
        }
    }

    /// `timeout` — a driver timeout fired. Absent ⇒ `no_timeout_callback`, a no-op (`io.c:7540`).
    pub fn timeout(&self, data: ErlDrvData) -> Result<(), DriverCallError> {
        // SAFETY: the slot holds the driver's own `void(ErlDrvData)` callback.
        match self.entry().timeout {
            Some(cb) => { unsafe { cb(data) }; Ok(()) }
            None => Err(absent("timeout")),
        }
    }

    /// `ready_input` — input arrived on a selected handle. Absent ⇒ `no_ready_input_callback`
    /// (`io.c:7516`).
    pub fn ready_input(&self, data: ErlDrvData, event: ErlDrvEvent) -> Result<(), DriverCallError> {
        // SAFETY: the slot holds the driver's own `void(ErlDrvData, ErlDrvEvent)` callback.
        match self.entry().ready_input {
            Some(cb) => { unsafe { cb(data, event) }; Ok(()) }
            None => Err(absent("ready_input")),
        }
    }

    /// `ready_output` — output is possible on a selected handle. Absent ⇒ `no_ready_output_callback`
    /// (`io.c:7524`).
    pub fn ready_output(&self, data: ErlDrvData, event: ErlDrvEvent) -> Result<(), DriverCallError> {
        // SAFETY: the slot holds the driver's own `void(ErlDrvData, ErlDrvEvent)` callback.
        match self.entry().ready_output {
            Some(cb) => { unsafe { cb(data, event) }; Ok(()) }
            None => Err(absent("ready_output")),
        }
    }

    /// `ready_async` — an async job completed. Absent ⇒ the C's silent no-op.
    pub fn ready_async(&self, data: ErlDrvData, tdata: ErlDrvThreadData) -> Result<(), DriverCallError> {
        // SAFETY: the slot holds the driver's own `void(ErlDrvData, ErlDrvThreadData)` callback.
        match self.entry().ready_async {
            Some(cb) => { unsafe { cb(data, tdata) }; Ok(()) }
            None => Err(absent("ready_async")),
        }
    }

    /// `flush` — flush queued output before the port closes. Absent ⇒ the C's silent no-op.
    pub fn flush(&self, data: ErlDrvData) -> Result<(), DriverCallError> {
        // SAFETY: the slot holds the driver's own `void(ErlDrvData)` callback.
        match self.entry().flush {
            Some(cb) => { unsafe { cb(data) }; Ok(()) }
            None => Err(absent("flush")),
        }
    }

    /// `process_exit` — a monitored process exited. Absent ⇒ the C's silent no-op.
    pub fn process_exit(&self, data: ErlDrvData, monitor: *mut ErlDrvMonitor) -> Result<(), DriverCallError> {
        // SAFETY: the slot holds the driver's own `void(ErlDrvData, ErlDrvMonitor*)` callback.
        match self.entry().process_exit {
            Some(cb) => { unsafe { cb(data, monitor) }; Ok(()) }
            None => Err(absent("process_exit")),
        }
    }

    /// `stop_select` — it is now safe to release `event`. Absent ⇒ `no_stop_select_callback`, which
    /// logs `"Driver does not implement stop_select callback"` (`io.c:7546-7553`).
    pub fn stop_select(&self, event: ErlDrvEvent, reserved: *mut c_void) -> Result<(), DriverCallError> {
        // SAFETY: the slot holds the driver's own `void(ErlDrvEvent, void*)` callback.
        match self.entry().stop_select {
            Some(cb) => { unsafe { cb(event, reserved) }; Ok(()) }
            None => Err(absent("stop_select")),
        }
    }

    /// `emergency_close` — the port is being closed abruptly. The C wires this slot only for drivers
    /// that advertise version ≥ 3.2 (`IS_DRIVER_VERSION_GE(de,3,2)`, `io.c:7604`); absent ⇒ the C's
    /// silent no-op.
    pub fn emergency_close(&self, data: ErlDrvData) -> Result<(), DriverCallError> {
        // SAFETY: the slot holds the driver's own `void(ErlDrvData)` callback.
        match self.entry().emergency_close {
            Some(cb) => { unsafe { cb(data) }; Ok(()) }
            None => Err(absent("emergency_close")),
        }
    }
}

impl Drop for DriverLibrary {
    fn drop(&mut self) {
        // SAFETY: `self.entry` is live — the object is still mapped, `dlclose` is below.
        if let Some(finish) = unsafe { (*self.entry).finish } {
            // SAFETY: `finish` is the driver's own `void(void)` callback, invoked while its code is
            // still mapped — the C's `do_unload_driver_entry` order (`erl_bif_ddll.c:1559-1564`).
            unsafe { finish() };
        }
        if !self.handle.is_null() {
            // SAFETY: `self.handle` came from a successful `dlopen` and is closed exactly once.
            unsafe { libc::dlclose(self.handle) };
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------------------------

/// Load a driver shared object and validate it in the oracle's exact order
/// (`erl_bif_ddll.c:1466-1533`).
///
/// `path` is as passed to `erlang:load_driver/2`; the shared-object suffix (`.so`) is appended when
/// absent. `name` is the registered driver name, which the entry's `driver_name` must equal.
///
/// The returned [`DriverLibrary`] owns the `dlopen` handle and closes it on drop (after running the
/// entry's `finish` callback).
pub fn load(path: &str, name: &str) -> Result<DriverLibrary, DriverLoadError> {
    let so_path = shared_object_path(path);
    let Ok(c_so_path) = CString::new(so_path.as_str()) else {
        return Err(DriverLoadError::Unspecified(format!(
            "{UNSPECIFIED_ERROR}: invalid path (interior NUL byte): '{so_path}'"
        )));
    };

    // --- 1. dlopen --------------------------------------------------------------
    // A failed open carries the loader's `dlerror` description (deviation 2 in the module docs).
    // SAFETY: `c_so_path` is a NUL-terminated path; `dlopen` accepts `RTLD_NOW | RTLD_LOCAL`.
    let handle =
        OpenHandle(unsafe { libc::dlopen(c_so_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) });
    if handle.get().is_null() {
        // `dlopen` failed, so the loader's error slot holds the reason.
        let errdesc = dl_last_error().unwrap_or_else(|| UNSPECIFIED_ERROR.to_string());
        return Err(DriverLoadError::Unspecified(errdesc));
    }

    // --- 2. driver_init / _driver_init ------------------------------------------
    // `erts_sys_ddll_load_driver_init` tries `driver_init` then the leading-underscore form
    // (`erl_unix_sys_ddll.c:216-228`); a failure becomes `ERL_DE_LOAD_ERROR_NO_INIT`
    // (`erl_bif_ddll.c:1478-1482`), i.e. the `no_driver_init` atom.
    // SAFETY: `handle.get()` is a live `dlopen` handle; both names are NUL-terminated.
    let init_fn = unsafe {
        match dl_sym(handle.get(), b"driver_init\0") {
            Ok(sym) => sym,
            Err(_) => match dl_sym(handle.get(), b"_driver_init\0") {
                Ok(sym) => sym,
                Err(_) => return Err(DriverLoadError::NoDriverInit(NO_DRIVER_INIT.to_string())),
            },
        }
    };
    if init_fn.is_null() {
        // `dlsym` reported no error but produced a null symbol value (a weak/absolute symbol); the
        // C's `ASSERT(fn)` would abort here.
        return Err(DriverLoadError::NoDriverInit(NO_DRIVER_INIT.to_string()));
    }

    // --- 3. call driver_init() --------------------------------------------------
    // SAFETY: `init_fn` is the object's `driver_init`/`_driver_init` symbol, whose C signature is
    // `ErlDrvEntry* (void)`.
    let init: DriverInitFn = unsafe { core::mem::transmute(init_fn) };
    // SAFETY: `init` is the resolved, signature-correct init function.
    let entry = unsafe { init() };
    if entry.is_null() {
        return Err(DriverLoadError::DriverInitFailed(DRIVER_INIT_FAILED.to_string()));
    }

    // --- 4. marker / version / name validation ----------------------------------
    // SAFETY: `entry` is non-null and live — the object stays open until the returned
    // `DriverLibrary` is dropped, and `OpenHandle` closes it on every error return below.
    let (marker, major, minor) =
        unsafe { ((*entry).extended_marker, (*entry).major_version, (*entry).minor_version) };
    if marker != ERL_DRV_EXTENDED_MARKER || !driver_version_accepted(major, minor) {
        // Either an old driver without the extended marker, or an out-of-range version.
        return Err(DriverLoadError::DriverIncorrectVersion(
            DRIVER_INCORRECT_VERSION.to_string(),
        ));
    }

    // SAFETY: `entry` is non-null and live.
    let Some(driver_name) = (unsafe { read_cstring((*entry).driver_name) }) else {
        // A NULL `driver_name` would fault in the C's `sys_strcmp` (deviation 3 in the module docs).
        return Err(DriverLoadError::BadDriverName(BAD_DRIVER_NAME.to_string()));
    };
    if driver_name != name {
        return Err(DriverLoadError::BadDriverName(BAD_DRIVER_NAME.to_string()));
    }

    // --- 5. the init callback ---------------------------------------------------
    // SAFETY: `entry` is non-null and live.
    if let Some(init_cb) = unsafe { (*entry).init } {
        // SAFETY: `init_cb` is the driver's own `int(void)` callback.
        let res = unsafe { init_cb() };
        if res != 0 {
            // `erts_add_driver_entry` propagates a non-zero `init` result as
            // `ERL_DE_LOAD_ERROR_FAILED_INIT` (`erl_bif_ddll.c:1519-1527`).
            return Err(DriverLoadError::DriverInitFailed(DRIVER_INIT_FAILED.to_string()));
        }
    }

    Ok(DriverLibrary { handle: handle.release(), entry, name: driver_name })
}

/// The oracle's driver version acceptance test — the negation of the rejection condition at
/// `erl_bif_ddll.c:1492-1499`: reject when `major < ERL_DRV_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD`, or
/// `major > ERL_DRV_EXTENDED_MAJOR_VERSION`, or (`major == ERL_DRV_EXTENDED_MAJOR_VERSION` and
/// `minor > ERL_DRV_EXTENDED_MINOR_VERSION`).
fn driver_version_accepted(major: c_int, minor: c_int) -> bool {
    major >= ERL_DRV_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD
        && (ERL_DRV_EXTENDED_MAJOR_VERSION > major
            || (ERL_DRV_EXTENDED_MAJOR_VERSION == major
                && ERL_DRV_EXTENDED_MINOR_VERSION >= minor))
}

// ---------------------------------------------------------------------------------------------
// Helpers (deliberate mirrors of `crate::nif::loader`'s private helpers — see the module docs)
// ---------------------------------------------------------------------------------------------

/// `path` with the shared-object suffix appended when absent.
fn shared_object_path(path: &str) -> String {
    if path.ends_with(SHARED_OBJECT_SUFFIX) {
        path.to_string()
    } else {
        format!("{path}{SHARED_OBJECT_SUFFIX}")
    }
}

/// Read a NUL-terminated C string, returning `None` for a null pointer.
///
/// Invalid UTF-8 is replaced (never an error, never a panic).
fn read_cstring(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `ptr` is NUL-terminated (a C string from the loaded object);
    // the string is copied out immediately.
    Some(unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
}

/// The pending dynamic-loader error, if any (reads and clears the loader's error slot).
fn dl_last_error() -> Option<String> {
    // SAFETY: `dlerror` takes no arguments and returns either NULL or a NUL-terminated string that
    // stays valid until the next `dl*` call; the string is copied out immediately.
    unsafe {
        let ptr = libc::dlerror();
        if ptr.is_null() {
            return None;
        }
        Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
    }
}

/// `dlsym` with the loader's own error detection (`dlerror` is non-NULL on failure), mirroring
/// `erts_sys_ddll_sym2` (`erl_unix_sys_ddll.c:163`). A null symbol *value* is therefore a success
/// if the loader reports no error; the caller null-checks separately.
///
/// # Safety
///
/// `handle` must be a live `dlopen` handle, and `name` must be NUL-terminated.
unsafe fn dl_sym(handle: *mut c_void, name: &[u8]) -> Result<*mut c_void, String> {
    // SAFETY: the caller guarantees `handle` is a live handle and `name` is NUL-terminated.
    unsafe {
        libc::dlerror(); // clear the loader's error slot
        let sym = libc::dlsym(handle, name.as_ptr() as *const c_char);
        match dl_last_error() {
            None => Ok(sym),
            Some(text) => Err(text),
        }
    }
}

/// RAII owner of a `dlopen` handle: closed on drop unless released into a [`DriverLibrary`].
struct OpenHandle(*mut c_void);

impl OpenHandle {
    fn get(&self) -> *mut c_void {
        self.0
    }

    /// Hand the raw handle to the caller, disarming the drop.
    fn release(self) -> *mut c_void {
        let handle = self.0;
        core::mem::forget(self);
        handle
    }
}

impl Drop for OpenHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` came from a successful `dlopen` and has not been released.
            unsafe { libc::dlclose(self.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts calls into the fake `output` / `init` callbacks.
    static OUTPUT_CALLS: AtomicUsize = AtomicUsize::new(0);
    static INIT_CALLS: AtomicUsize = AtomicUsize::new(0);

    /// The pointer value the fake `start` returns, so the test can prove it round-trips.
    fn sentinel() -> ErlDrvData {
        0x1234_5678usize as ErlDrvData
    }

    /// `core::ptr::null_mut()` with the pointee type taken from each call site.
    fn null<T>() -> *mut T {
        core::ptr::null_mut()
    }
    extern "C" fn fake_output(_data: ErlDrvData, _buf: *mut c_char, _len: ErlDrvSizeT) {
        OUTPUT_CALLS.fetch_add(1, Ordering::SeqCst);
    }

    extern "C" fn fake_init() -> c_int {
        INIT_CALLS.fetch_add(1, Ordering::SeqCst);
        0
    }

    extern "C" fn fake_start(_port: ErlDrvPort, _command: *mut c_char) -> ErlDrvData {
        sentinel()
    }

    extern "C" fn fake_timeout(_data: ErlDrvData) {}

    /// A [`DriverLibrary`] over a test-owned entry: a null handle (nothing to `dlclose`) and a
    /// pointer to a stack-local `ErlDrvEntry` the test keeps alive for the library's lifetime.
    fn library_over(entry: &mut ErlDrvEntry) -> DriverLibrary {
        DriverLibrary { handle: core::ptr::null_mut(), entry: entry as *mut ErlDrvEntry, name: "t".to_string() }
    }

    #[test]
    fn shared_object_path_appends_the_suffix_only_when_absent() {
        assert_eq!(shared_object_path("/tmp/x"), "/tmp/x.so");
        assert_eq!(shared_object_path("/tmp/x.so"), "/tmp/x.so");
        assert_eq!(shared_object_path("x"), "x.so");
    }

    #[test]
    fn driver_version_acceptance_matches_the_oracle_condition() {
        assert!(driver_version_accepted(3, 3)); // exactly this runtime's version
        assert!(driver_version_accepted(3, 0));
        assert!(driver_version_accepted(2, 0)); // the minimum required major
        assert!(!driver_version_accepted(3, 4)); // newer minor
        assert!(!driver_version_accepted(4, 0)); // newer major
        assert!(!driver_version_accepted(1, 9)); // older than the minimum
    }

    #[test]
    fn error_reason_atoms_and_texts_match_the_oracle() {
        // The reason atoms come from `errcode_tab` (`erl_bif_ddll.c:1709-1720`).
        for (err, atom) in [
            (DriverLoadError::NoDriverInit(String::new()), "no_driver_init"),
            (DriverLoadError::DriverInitFailed(String::new()), "driver_init_failed"),
            (DriverLoadError::BadDriverName(String::new()), "bad_driver_name"),
            (DriverLoadError::DriverIncorrectVersion(String::new()), "driver_incorrect_version"),
            (DriverLoadError::SymbolNotFound(String::new()), "symbol_not_found"),
            (DriverLoadError::Unspecified(String::new()), "unspecified_error"),
        ] {
            assert_eq!(err.reason_atom(), atom);
        }

        // `text()` is the payload verbatim; `Display` is `"<atom>: <text>"`.
        assert_eq!(DriverLoadError::NoDriverInit("boom".into()).text(), "boom");
        assert_eq!(DriverLoadError::NoDriverInit("boom".into()).to_string(), "no_driver_init: boom");
        assert_eq!(DriverLoadError::Unspecified("x".into()).to_string(), "unspecified_error: x");

        // The payload texts are the `erts_ddll_error` strings (`erl_bif_ddll.c:1201-1224`).
        assert_eq!(NO_DRIVER_INIT, "No driver init in dynamic library");
        assert_eq!(DRIVER_INIT_FAILED, "Driver init failed");
        assert_eq!(BAD_DRIVER_NAME, "Bad driver name");
        assert_eq!(DRIVER_INCORRECT_VERSION, "Driver compiled with incorrect version of erl_driver.h");
        assert_eq!(UNSPECIFIED_ERROR, "Unspecified dynamic library error");
    }

    #[test]
    fn absent_callbacks_are_reported_not_silently_ignored() {
        let mut entry = ErlDrvEntry::default();
        let lib = library_over(&mut entry);
        assert_eq!(lib.name(), "t");
        assert!(lib.init().is_none(), "no init callback ⇒ None");

        // Every wrapper reports the missing slot, naming the `ErlDrvEntry` field.
        let pairs = [            (lib.output(null(), null(), 0).err(), "output"),
            (lib.outputv(null(), null()).err(), "outputv"),
            (lib.stop(null()).err(), "stop"),
            (lib.flush(null()).err(), "flush"),
            (lib.timeout(null()).err(), "timeout"),
            (lib.ready_input(null(), null()).err(), "ready_input"),
            (lib.ready_output(null(), null()).err(), "ready_output"),
            (lib.ready_async(null(), null()).err(), "ready_async"),
            (lib.process_exit(null(), null()).err(), "process_exit"),
            (lib.stop_select(null(), null()).err(), "stop_select"),
            (lib.emergency_close(null()).err(), "emergency_close"),
            (lib.start(null(), null()).err(), "start"),
            (lib.finish().err(), "finish"),
            (lib.control(null(), 0, null(), 0, null(), 0).err(), "control"),
            (lib.call(null(), 0, null(), 0, null(), 0, null()).err(), "call"),
        ];
        for (result, callback) in pairs {
            let absent = result.expect("a missing callback must be reported");
            assert!(absent.is_absent());
            assert_eq!(absent, DriverCallError::AbsentCallback { callback });
        }
        assert_eq!(absent("flop").to_string(), "driver callback 'flop' is not implemented");
    }

    #[test]
    fn present_callbacks_are_invoked_and_their_values_returned() {        let mut entry = ErlDrvEntry {
            init: Some(fake_init),
            start: Some(fake_start),
            output: Some(fake_output),
            timeout: Some(fake_timeout),
            ..ErlDrvEntry::default()
        };
        let lib = library_over(&mut entry);

        let before = OUTPUT_CALLS.load(Ordering::SeqCst);
        assert!(lib.output(core::ptr::null_mut(), core::ptr::null_mut(), 0).is_ok());
        assert_eq!(OUTPUT_CALLS.load(Ordering::SeqCst), before + 1);

        assert_eq!(lib.start(core::ptr::null_mut(), core::ptr::null_mut()).unwrap(), sentinel());
        assert_eq!(lib.init(), Some(0));
        assert_eq!(INIT_CALLS.load(Ordering::SeqCst), 1);
        assert!(lib.timeout(core::ptr::null_mut()).is_ok());
    }

    #[test]
    fn read_cstring_handles_null_and_valid_strings() {
        assert_eq!(read_cstring(core::ptr::null()), None);
        let s = b"hello\0";
        assert_eq!(read_cstring(s.as_ptr() as *const c_char).as_deref(), Some("hello"));
    }

    #[test]
    fn a_missing_shared_object_is_a_typed_error_never_a_crash() {
        let path = format!("/tmp/beam_abi-driver-does-not-exist-{}.so", std::process::id());
        let err = load(&path, "t").expect_err("loading a nonexistent object must fail");
        assert_eq!(err.reason_atom(), "unspecified_error");
        // `dlerror` names the file we tried to open.
        assert!(
            err.text().contains(&path) || err.text().contains("does-not-exist"),
            "the loader's description should name the path: {}",
            err.text()
        );
    }

    #[test]
    fn a_shared_object_without_driver_init_is_no_driver_init() {
        // Any real shared object that is not a driver exercises `dlopen` success followed by the
        // `dlsym` miss, which the C maps to `ERL_DE_LOAD_ERROR_NO_INIT` ⇒ `no_driver_init`.
        match load("libc.so.6", "t") {
            // Some libc builds (musl, unusual multiarch paths) are not reachable by that soname:
            // skip loudly rather than assert on an unrelated failure.
            Err(DriverLoadError::Unspecified(text)) => {
                eprintln!("SKIP: libc.so.6 is not dlopen-able here: {text}");
            }
            Err(err) => assert_eq!(
                err.reason_atom(),
                "no_driver_init",
                "a non-driver shared object must fail the `driver_init` lookup: {err}"
            ),
            Ok(lib) => panic!("libc must not export driver_init; loaded '{}'", lib.name()),
        }
    }
}
