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

//! The driver host seam — the runtime services the `erl_drv_*` layer needs.
//!
//! `beam_abi` freezes the *shape* of the driver C ABI (the `ErlDrv*` types and the
//! `driver_*`/`erl_drv_*` table in [`super::api`]). The driver functions cannot do their job alone:
//! `erl_drv_output_term`/`erl_drv_send_term` must turn a **term-data array** into real terms
//! (allocating on a heap, interning atoms), `driver_select` must register an event with the poller,
//! and `driver_set_timer` must talk to the timer wheel. Those are **runtime** services — `beam_abi`
//! must **not** pull in the scheduler, `beam_io` or a specific module's atom table.
//!
//! [`DriverHost`] is that seam. [`DriverEnv`] is the concrete per-port environment behind the
//! C-facing opaque `*mut ErlDrvPort_` (`struct _erl_drv_port`, opaque in [`super::types`]).
//!
//! **The `ErlDrvPort` a runtime passes to a driver callback IS the pointer returned by
//! [`DriverEnv::as_port`]**, recovered with [`DriverEnv::from_port`] — the exact analogue of
//! [`NifEnv::as_ptr`](crate::nif::host::NifEnv::as_ptr) /
//! [`NifEnv::from_opaque`](crate::nif::host::NifEnv::from_opaque). `unsafe` is confined to
//! [`DriverEnv::host`], [`DriverEnv::from_port`] and [`decode_term_spec`]'s operand readers, each
//! documented with a `# Safety` section / `// SAFETY:` rationale (workspace `CONTEXT.md` §3).
//!
//! Two frozen slots carry **no** port operand — `add_driver_entry`/`remove_driver_entry` take only
//! an `ErlDrvEntry *` (`io.c:7640`, `io.c:7698`) — so there is no [`DriverEnv`] to recover a host
//! from. [`with_current_host`] (re-exported here from [`crate::driver::current_host`]) is the scoped
//! publication the runtime wraps those entry points in, so they can reach a [`DriverHost`].

use libc::{c_char, c_int, c_long, c_uint, c_ulong, c_void};

use beam_alloc::Heap;
use beam_term::prelude::{self, Eterm, NIL};

use crate::common::SysIOVec;
use crate::driver::types::{
    ErlDrvBinary, ErlDrvData, ErlDrvEntry, ErlDrvEvent, ErlDrvMonitor, ErlDrvNowData, ErlDrvPDL,
    ErlDrvPort, ErlDrvPort_, ErlDrvSInt, ErlDrvTermData, ERTS_INVALID_ERL_DRV_PORT, ERL_DRV_ATOM,
    ERL_DRV_BINARY, ERL_DRV_BUF2BINARY, ERL_DRV_FLOAT, ERL_DRV_INT, ERL_DRV_INT64, ERL_DRV_LIST,
    ERL_DRV_NIL, ERL_DRV_PORT, ERL_DRV_STRING, ERL_DRV_STRING_CONS, ERL_DRV_TUPLE, ERL_DRV_UINT,
    ERL_DRV_UINT64,
};

// Re-exported so the runtime reaches the publication through the same module as the seam it belongs
// to (`beam_abi::driver::host::with_current_host`); `current_host` documents the mechanism.
pub use crate::driver::current_host::with_current_host;

/// The reason a driver asks for its port to be closed — the C's whole `driver_failure*` family
/// (`erts/emulator/beam/io.c`) collapsed into one seam hook ([`DriverHost::failure`]).
///
/// The four C entry points differ only in how the reason is expressed, so they model as variants
/// rather than four hooks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortFailure<'a> {
    /// `driver_failure_eof(port)` — the connected process reached end of file.
    Eof,
    /// `driver_failure_atom(port, string)` — close with `string` as the reason atom.
    ///
    /// The C operand is a NUL-terminated `char *`; this seam carries the name as a `&str` and the
    /// host interns it, exactly as [`DriverHost::intern_atom`] does for the `ERL_DRV_ATOM`
    /// term-data operand.
    Atom(&'a str),
    /// `driver_failure_posix(port, error)` — close with `{error, <posix reason for the errno>}`.
    Posix(c_int),
    /// `driver_failure(port, error)` — close with `{error, <errno>}`.
    Error(c_int),
}

/// Services the runtime provides to the `erl_drv_*` implementations of one port.
pub trait DriverHost {
    /// The term-allocation arena for the duration of the current call. Must not reallocate while the
    /// driver holds a pointer into it (see [`beam_alloc::Heap`]).
    fn heap(&mut self) -> &mut Heap;

    /// The runtime's global atom table: intern `name`, returning the atom term.
    fn intern_atom(&mut self, name: &str) -> Option<Eterm>;

    /// Deliver `data` to the port owner (`driver_output`/`erl_drv_output`). `0` = ok, `-1` = fail.
    fn output(&mut self, port: ErlDrvPort, data: &[u8]) -> c_int;

    /// Deliver a decoded term (`erl_drv_output_term`/`erl_drv_send_term`): to the port owner when
    /// `to` is `None`, else to that specific process. `0` = ok, `-1` = fail.
    fn send_term(&mut self, port: ErlDrvPort, to: Option<Eterm>, term: Eterm) -> c_int;

    /// Register/cancel interest in `event` (`driver_select`). `mode` is `ERL_DRV_READ`/
    /// `ERL_DRV_WRITE`/`ERL_DRV_USE`; `on` non-zero selects, zero deselects. `0` = ok, `-1` = fail
    /// (default: unsupported).
    fn select(&mut self, port: ErlDrvPort, event: ErlDrvEvent, mode: c_int, on: c_int) -> c_int {
        let _ = (port, event, mode, on);
        -1
    }

    /// Start a `ms`-millisecond timer for `port` (`driver_set_timer`). `0` = ok, `-1` = fail
    /// (default: unsupported).
    fn set_timer(&mut self, port: ErlDrvPort, ms: c_ulong) -> c_int {
        let _ = (port, ms);
        -1
    }

    /// Cancel `port`'s timer (`driver_cancel_timer`). `0` = ok, `-1` = fail (default: unsupported).
    fn cancel_timer(&mut self, port: ErlDrvPort) -> c_int {
        let _ = port;
        -1
    }

    /// The remaining milliseconds of `port`'s timer (`driver_read_timer`), or `None` when there is
    /// no timer / the host does not support them (default: unsupported).
    fn read_timer(&mut self, port: ErlDrvPort) -> Option<c_ulong> {
        let _ = port;
        None
    }

    /// Schedule an asynchronous `invoke` on `port` (`driver_async`). `key` identifies the job,
    /// `data` is passed to `invoke`, and `free` releases `data` when no `invoke` runs. Returns a
    /// queue position (`=> 0`) or a negative error (default: unsupported, `-1`).
    ///
    /// A raw identifier because `async` is a Rust keyword; the C name is `driver_async`.
    fn r#async(
        &mut self,
        port: ErlDrvPort,
        key: *mut c_uint,
        invoke: Option<unsafe extern "C" fn(*mut c_void)>,
        data: *mut c_void,
        free: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_long {
        let _ = (port, key, invoke, data, free);
        -1
    }

    /// The pid the port is connected to (`driver_connected`), or `None` (default: unsupported).
    fn connected_pid(&mut self, port: ErlDrvPort) -> Option<Eterm> {
        let _ = port;
        None
    }

    /// The pid that opened the port (`driver_caller`), or `None` (default: unsupported).
    fn caller_pid(&mut self, port: ErlDrvPort) -> Option<Eterm> {
        let _ = port;
        None
    }

    // ---------------------------------------------------------------------------------------
    // Port queue — `driver_enq`/`driver_pushq`/`driver_deq`/`driver_sizeq`/`driver_peekq[v]`
    //
    // The C functions take an `ErlDrvPort`; here the host **is** the port's environment, so these
    // new hooks take no port operand (the same "own port" convention the `lifecycle`/`pdl`/`queues`
    // families are specified against). The existing hooks above keep their `ErlDrvPort` operand for
    // signature stability.
    // ---------------------------------------------------------------------------------------

    /// Append `data` to the port's output queue (`driver_enq`/`driver_enq_bin`). `is_binary` says
    /// whether the driver presented those bytes as an `ErlDrvBinary` window rather than a plain
    /// buffer (the seam hands the host a byte slice either way). `0` = ok, `-1` = fail (default:
    /// unsupported).
    fn enq(&mut self, data: &[u8], is_binary: bool) -> c_int {
        let _ = (data, is_binary);
        -1
    }

    /// Prepend `data` to the port's output queue (`driver_pushq`/`driver_pushq_bin`). `is_binary` as
    /// for [`enq`](DriverHost::enq). `0` = ok, `-1` = fail (default: unsupported).
    fn pushq(&mut self, data: &[u8], is_binary: bool) -> c_int {
        let _ = (data, is_binary);
        -1
    }

    /// Drop up to `len` bytes from the head of the port's output queue (`driver_deq`). Returns the
    /// number of bytes actually dropped, or `-1` (default: unsupported).
    fn deq(&mut self, len: c_int) -> c_int {
        let _ = len;
        -1
    }

    /// The number of bytes queued on the port (`driver_sizeq`), or `-1` (default: unsupported).
    fn sizeq(&mut self) -> c_int {
        -1
    }

    /// A `SysIOVec` view of the head of the port's output queue (`driver_peekq`/`driver_peekqv`), or
    /// `None` when unsupported or the queue is empty (default: unsupported).
    ///
    /// The C writes the vector length through a `int *` out-parameter and returns a pointer into the
    /// port's own queue; returning an owned `Vec<SysIOVec>` is the safe, ownership-clean spelling of
    /// the same information.
    fn peekq(&mut self) -> Option<Vec<SysIOVec>> {
        None
    }

    // ---------------------------------------------------------------------------------------
    // Port attributes / flags — `set_busy_port`/`set_port_control_flags`/`get_port_flags`
    // ---------------------------------------------------------------------------------------

    /// Set (`true`) or clear (`false`) the port's busy state (`set_busy_port`). `0` = ok, `-1` =
    /// fail (default: unsupported).
    fn set_busy_port(&mut self, busy: bool) -> c_int {
        let _ = busy;
        -1
    }

    /// Set the port's control `mode`/`flags` (`set_port_control_flags`; `mode` is the C macro's
    /// first argument, `flags` the `PORT_CONTROL_FLAG_*` bit set). `0` = ok, `-1` = fail (default:
    /// unsupported).
    fn set_control_flags(&mut self, mode: c_int, flags: c_int) -> c_int {
        let _ = (mode, flags);
        -1
    }

    /// The port's flags (`get_port_flags`, the `PORT_FLAG_*` bit set), or `None` (default:
    /// unsupported).
    fn get_port_flags(&mut self) -> Option<c_int> {
        None
    }

    // ---------------------------------------------------------------------------------------
    // Lifecycle & failures — `driver_failure*`/`driver_exit`/`erl_drv_busy_msgq_limits`/
    // `erl_drv_init_ack`/`erl_drv_set_os_pid`/`driver_async_port_key`
    // ---------------------------------------------------------------------------------------

    /// Close the port with `reason` — the single hook behind the C's `driver_failure*` family.
    /// `0` = ok, `-1` = fail (default: unsupported).
    fn failure(&mut self, reason: PortFailure<'_>) -> c_int {
        let _ = reason;
        -1
    }

    /// Immediately terminate the port's connected process with exit `code` (`driver_exit`). `0` =
    /// ok, `-1` = fail (default: unsupported).
    fn port_exit(&mut self, code: c_int) -> c_int {
        let _ = code;
        -1
    }

    /// Set the port's busy-message-queue watermarks (`erl_drv_busy_msgq_limits`): the emulator
    /// blocks the port on `high` queued messages and unblocks it on `low`. `0` = ok, `-1` = fail
    /// (default: unsupported).
    fn busy_msgq_limits(&mut self, low: c_int, high: c_int) -> c_int {
        let _ = (low, high);
        -1
    }

    /// Acknowledge an asynchronous port init (`erl_drv_init_ack`). `0` = ok, `-1` = fail (default:
    /// unsupported).
    fn init_ack(&mut self) -> c_int {
        -1
    }

    /// Record the port's OS pid as `pid` for `port_info(_, os_pid)` (`erl_drv_set_os_pid`). `0` =
    /// ok, `-1` = fail (default: unsupported).
    fn set_os_pid(&mut self, pid: c_int) -> c_int {
        let _ = pid;
        -1
    }

    /// Write the port's async key into `key` (`driver_async_port_key`; the C returns it directly, a
    /// `*mut c_uint` out-parameter keeps a failure reportable). `0` = ok, `-1` = fail (default:
    /// unsupported).
    fn async_port_key(&mut self, key: *mut c_uint) -> c_int {
        let _ = key;
        -1
    }

    // ---------------------------------------------------------------------------------------
    // Monitors — `driver_monitor_process`/`driver_demonitor_process`/`driver_get_monitored_process`
    // ---------------------------------------------------------------------------------------

    /// Monitor the process `pid` (`driver_monitor_process`), returning the monitor holder the driver
    /// later passes to [`demonitor_process`](DriverHost::demonitor_process) /
    /// [`get_monitored_process`](DriverHost::get_monitored_process). `None` = unsupported (default).
    fn monitor_process(&mut self, pid: Eterm) -> Option<ErlDrvMonitor> {
        let _ = pid;
        None
    }

    /// Cancel the monitor `mon` (`driver_demonitor_process`). `0` = ok, `-1` = fail (default:
    /// unsupported).
    fn demonitor_process(&mut self, mon: *const ErlDrvMonitor) -> c_int {
        let _ = mon;
        -1
    }

    /// The pid behind the monitor `mon` (`driver_get_monitored_process`), or `None` (default:
    /// unsupported).
    fn get_monitored_process(&mut self, mon: *const ErlDrvMonitor) -> Option<Eterm> {
        let _ = mon;
        None
    }

    // ---------------------------------------------------------------------------------------
    // Port data lock — `driver_pdl_*`
    //
    // The C models a PDL as an opaque `ErlDrvPDL` handle to a per-port refcounted lock object. The
    // host owns the object (it must outlive the port) and hands the driver the same handle back on
    // every call, so the seam passes `ErlDrvPDL` through unchanged — a `Vec<u8>` blob would not
    // survive the round-trip and could not be identified per port.
    // ---------------------------------------------------------------------------------------

    /// Create (or return the existing) port data lock, as its opaque `ErlDrvPDL` handle
    /// (`driver_pdl_create`); `None` = unsupported (default).
    fn pdl_create(&mut self) -> Option<ErlDrvPDL> {
        None
    }

    /// Take the port data lock `pdl` (`driver_pdl_lock`). `0` = ok, `-1` = fail (default:
    /// unsupported).
    fn pdl_lock(&mut self, pdl: ErlDrvPDL) -> c_int {
        let _ = pdl;
        -1
    }

    /// Release the port data lock `pdl` (`driver_pdl_unlock`). `0` = ok, `-1` = fail (default:
    /// unsupported).
    fn pdl_unlock(&mut self, pdl: ErlDrvPDL) -> c_int {
        let _ = pdl;
        -1
    }

    /// The port data lock's reference count (`driver_pdl_get_refc`), or `None` (default:
    /// unsupported).
    fn pdl_get_refc(&mut self, pdl: ErlDrvPDL) -> Option<ErlDrvSInt> {
        let _ = pdl;
        None
    }

    /// Increment the port data lock's reference count, returning the new value
    /// (`driver_pdl_inc_refc`), or `None` (default: unsupported).
    fn pdl_inc_refc(&mut self, pdl: ErlDrvPDL) -> Option<ErlDrvSInt> {
        let _ = pdl;
        None
    }

    /// Decrement the port data lock's reference count, returning the new value
    /// (`driver_pdl_dec_refc`), or `None` (default: unsupported).
    fn pdl_dec_refc(&mut self, pdl: ErlDrvPDL) -> Option<ErlDrvSInt> {
        let _ = pdl;
        None
    }

    // ---------------------------------------------------------------------------------------
    // Deprecated wall clock — `driver_get_now`
    // ---------------------------------------------------------------------------------------

    /// The wall-clock breakdown (`driver_get_now`, deprecated in the C), or `None` (default:
    /// unsupported).
    fn now(&mut self) -> Option<ErlDrvNowData> {
        None
    }

    // ---------------------------------------------------------------------------------------
    // Driver registry & port creation — `add_driver_entry`/`remove_driver_entry`/
    // `driver_lock_driver`/`driver_create_port`
    //
    // These four reach **process-global** runtime structure (the emulator's driver list, a loaded
    // driver's shared-object handle, the port table) rather than one port's term services. Every one
    // is defaulted to the C's own failure value, so a host that does not model the service answers
    // exactly as the C does for a request it cannot honour — never a fabricated success.
    // ---------------------------------------------------------------------------------------

    /// Make the statically linked driver entry `de` known to the emulator — the `void add_driver_entry(ErlDrvEntry *drv)`
    /// wrapper (`io.c:7640`) over `int erts_add_driver_entry(de, handle, driver_list_locked, taint)`
    /// (`io.c:7650`), which records `de` in the global `driver_list` and runs `init_driver` on it.
    ///
    /// `0` = recorded, non-zero = `init_driver` refused the entry, `-1` = unsupported (default).
    /// The C wrapper **discards** this result on purpose ("the init is not allowed to fail when
    /// drivers are added by drivers", `io.c:7642-7645`), so a caller sees nothing through the C ABI;
    /// it is a value here only so the refusal is modelable at all.
    fn add_driver_entry(&mut self, de: *mut ErlDrvEntry) -> c_int {
        let _ = de;
        -1
    }

    /// Remove the driver entry `de` from the emulator's global `driver_list`
    /// (`int remove_driver_entry(ErlDrvEntry *drv)`, `io.c:7698`).
    ///
    /// `1` = removed, `0` = no such entry (the default: unsupported), `-1` = `de` names a
    /// **dynamically** loaded driver (`dp->handle != NULL`, "not allowed for dynamic drivers",
    /// `io.c:7709-7713`).
    fn remove_driver_entry(&mut self, de: *mut ErlDrvEntry) -> c_int {
        let _ = de;
        0
    }

    /// Lock the driver behind `port`'s **own** loaded shared object for the rest of the machine's
    /// lifetime (`int driver_lock_driver(ErlDrvPort ix)`, `io.c:7324`).
    ///
    /// `0` = locked; `-1` = unsupported (default) or the port's driver has no `dlopen` handle — a
    /// statically linked driver (`(dh = prt->drv_ptr->handle) == NULL`, `io.c:7337`). The C consults
    /// `prt->drv_ptr->handle` (the handle of the *port's own* driver, `io.c:7337`), **not** a lookup
    /// by name in any global table.
    fn lock_driver(&mut self, port: ErlDrvPort) -> c_int {
        let _ = port;
        -1
    }

    /// Create a new port owned by the emitting driver — `ErlDrvPort driver_create_port(ErlDrvPort
    /// creator_port_ix, ErlDrvTermData pid, char *name, ErlDrvData drv_data)` (`io.c:787`): `name`
    /// selects the driver to instantiate, `pid` becomes the new port's owner/caller and `drv_data`
    /// its driver-private data.
    ///
    /// Returns the new port handle, or the invalid-port sentinel [`ERTS_INVALID_ERL_DRV_PORT`]
    /// (pointer `-1`, `erl_port.h:67`) — which is the C's answer for **every** failure and hence the
    /// default here.
    fn create_port(
        &mut self,
        creator: ErlDrvPort,
        connected: ErlDrvTermData,
        name: *mut c_char,
        drv_data: ErlDrvData,
    ) -> ErlDrvPort {
        let _ = (creator, connected, name, drv_data);
        ERTS_INVALID_ERL_DRV_PORT
    }
}

/// The concrete per-port driver environment behind the C-facing opaque `*mut ErlDrvPort_`.
///
/// Holds a raw pointer to the host, so the environment itself is lifetime-free and can be recovered
/// from the opaque C pointer by [`DriverEnv::from_port`] (see the module docs).
pub struct DriverEnv {
    /// The host, borrowed for the lifetime of this environment.
    ///
    /// A raw fat pointer rather than `&'a mut dyn DriverHost`: the C side only ever sees
    /// `*mut ErlDrvPort_`, and the recover path ([`DriverEnv::from_port`]) has no way to name the
    /// borrow's lifetime. The caller must keep the host alive for as long as the environment is.
    /// (The `'static` that a `*mut dyn Trait` spells by default is erased by the documented
    /// lifetime-erasing cast in [`DriverEnv::new`].)
    host: *mut dyn DriverHost,
}

impl DriverEnv {
    /// Wrap `host` in a fresh per-port environment.
    pub fn new(host: &mut dyn DriverHost) -> DriverEnv {
        // SAFETY: the only thing reinterpreted here is the erased trait-object lifetime. The
        // C-facing env has no lifetime parameter (`struct _erl_drv_port` is plain C), so the
        // borrow cannot be expressed in the type; `host as *mut dyn DriverHost` alone would be
        // rejected because a raw pointer to a trait object is invariant and defaults to `'static`
        // (raw pointer *casts* may not extend a trait-object lifetime either — rust#141402).
        // `&mut dyn DriverHost` and `*mut dyn DriverHost` are layout-identical (data word + vtable
        // word), and `mem::transmute` between them is a bit-preserving cast, so the caller's borrow
        // is reproduced exactly by `DriverEnv::host`. The caller must keep the host alive for as
        // long as the environment is used (`DriverEnv` is not `Clone`, so there is exactly one
        // handle).
        let host: *mut dyn DriverHost = unsafe { core::mem::transmute(host) };
        DriverEnv { host }
    }

    /// Reborrow the host (the env borrows the host for its lifetime).
    pub fn host(&mut self) -> &mut dyn DriverHost {
        // SAFETY: `host` was produced from a live `&mut dyn DriverHost` in `new` (or a previous
        // `host`/`as_port`/`from_port` round-trip), and `DriverEnv` is not `Clone`, so this is the
        // only outstanding handle to it; the returned reborrow is tied to `&mut self`.
        unsafe { &mut *self.host }
    }

    /// Convenience: `self.host().heap()`.
    pub fn heap(&mut self) -> &mut Heap {
        self.host().heap()
    }

    /// The C-facing opaque port pointer. Casts `self as *mut DriverEnv as *mut ErlDrvPort_`.
    pub fn as_port(&mut self) -> ErlDrvPort {
        self as *mut DriverEnv as *mut ErlDrvPort_
    }

    /// Recover the concrete env from an opaque port pointer.
    ///
    /// # Safety
    ///
    /// `port` must be a pointer previously returned by [`DriverEnv::as_port`] for an env that is
    /// still alive (and whose host is still alive). Returns `None` for a null pointer.
    pub unsafe fn from_port<'a>(port: ErlDrvPort) -> Option<&'a mut DriverEnv> {
        if port.is_null() {
            return None;
        }
        // SAFETY: the caller guarantees `port` came from `DriverEnv::as_port` on a live env, i.e. it
        // is a `*mut DriverEnv` with the provenance required to reborrow the same allocation; `'a`
        // is the caller's promise that the env outlives the returned reference.
        Some(unsafe { &mut *(port as *mut DriverEnv) })
    }
}

// The term-data array decoder (`erl_drv_output_term`/`erl_drv_send_term`).

/// Turn a driver **term-data array** into an [`Eterm`], building the term on the host heap.
///
/// The array is the C `ErlDrvTermData spec[]`: tag/operand pairs in *reverse Polish* order —
/// operands are pushed first, and a `TUPLE`/`LIST` tag pops its operands (ground truth
/// `erts/emulator/beam/io.c`, `ERTS_DDT_*`). It must leave **exactly one** value on the stack and be
/// fully consumed, as the C requires (`io.c`: `if (depth != 1 || ptr != ptr_end) fail`). Returns
/// `None` for any malformed spec — never a wrong term.
///
/// Handled tags: `ERL_DRV_NIL`, `ERL_DRV_ATOM`, `ERL_DRV_INT`, `ERL_DRV_UINT`, `ERL_DRV_INT64`,
/// `ERL_DRV_UINT64`, `ERL_DRV_STRING`, `ERL_DRV_STRING_CONS`, `ERL_DRV_BUF2BINARY`,
/// `ERL_DRV_BINARY`, `ERL_DRV_TUPLE`, `ERL_DRV_LIST`, `ERL_DRV_PORT`, `ERL_DRV_FLOAT`.
///
/// # Documented deviations from the C (never a wrong term)
///
/// * `ERL_DRV_ATOM`'s operand is treated as a NUL-terminated `char*` interned through
///   [`DriverHost::intern_atom`]. In the C the operand is already an atom term from
///   `driver_mk_atom`; this seam has no `driver_mk_atom`, so the spec carries the *name* and the
///   host interns it — the same atom either way.
/// * `ERL_DRV_PID`, `ERL_DRV_EXT2TERM` and `ERL_DRV_MAP` are **unsupported** (`None`): they need a
///   pid term, an ETF decoder and a map builder outside this seam.
/// * Counts (`LIST`/`TUPLE` arity, `STRING`/`BUF2BINARY` length) are read as non-negative 32-bit
///   values, mirroring the C's `(int)` casts, so a malformed spec cannot request an astronomically
///   large copy. `ERL_DRV_BINARY`'s `size`/`offset` are read as full words and bounds-checked
///   against the binary's `orig_size`, exactly as the C does.
pub fn decode_term_spec(env: &mut DriverEnv, spec: &[ErlDrvTermData]) -> Option<Eterm> {
    // The intermediate value stack (the C's `ESTACK_PUSH`/`ESTACK_POP`).
    let mut stack: Vec<Eterm> = Vec::new();
    let mut ix = 0usize;

    while ix < spec.len() {
        let tag = spec[ix];
        ix += 1;
        match tag {
            ERL_DRV_NIL => stack.push(NIL),

            ERL_DRV_ATOM => {
                let name = *spec.get(ix)?;
                ix += 1;
                // SAFETY: the `ERL_DRV_ATOM` operand is a NUL-terminated C string (see the docs).
                let bytes = unsafe { cstr(name as *const c_char) }?;
                stack.push(env.host().intern_atom(core::str::from_utf8(bytes).ok()?)?);
            }

            ERL_DRV_INT => {
                let value = *spec.get(ix)?;
                ix += 1;
                stack.push(int_from_i64(env, value as i64)?);
            }

            ERL_DRV_UINT => {
                let value = *spec.get(ix)?;
                ix += 1;
                stack.push(int_from_u64(env, value)?);
            }

            ERL_DRV_INT64 => {
                let ptr = *spec.get(ix)?;
                ix += 1;
                // SAFETY: the `ERL_DRV_INT64` operand is a `Sint64*`.
                let value = unsafe { read_scalar(ptr as *const i64) }?;
                stack.push(int_from_i64(env, value)?);
            }

            ERL_DRV_UINT64 => {
                let ptr = *spec.get(ix)?;
                ix += 1;
                // SAFETY: the `ERL_DRV_UINT64` operand is a `Uint64*`.
                let value = unsafe { read_scalar(ptr as *const u64) }?;
                stack.push(int_from_u64(env, value)?);
            }

            ERL_DRV_STRING | ERL_DRV_STRING_CONS => {
                let ptr = *spec.get(ix)?;
                let len = *spec.get(ix + 1)?;
                ix += 2;
                let n = count_operand(len)?;
                if ptr == 0 {
                    return None; // The C rejects a NULL buffer even for a zero length.
                }
                let tail = if tag == ERL_DRV_STRING_CONS {
                    stack.pop()? // The C requires a value to cons onto (`depth < 1` fails).
                } else {
                    NIL
                };
                // SAFETY: `(char*, length)` declares `n` readable bytes at `ptr`.
                let bytes = unsafe { buf(ptr as *const c_char, n) }?;
                stack.push(byte_list(env, bytes, tail)?);
            }

            ERL_DRV_BUF2BINARY => {
                let ptr = *spec.get(ix)?;
                let len = *spec.get(ix + 1)?;
                ix += 2;
                let n = count_operand(len)?;
                // SAFETY: as `ERL_DRV_STRING`; the C allows a NULL only when the length is 0.
                let bytes = unsafe { buf(ptr as *const c_char, n) }?;
                stack.push(binary_term(env, bytes)?);
            }

            ERL_DRV_BINARY => {
                let bin = *spec.get(ix)?;
                let size = *spec.get(ix + 1)?;
                let offset = *spec.get(ix + 2)?;
                ix += 3;
                // SAFETY: the operand is an `ErlDrvBinary*`; `driver_binary_bytes` bounds-checks
                // `(offset, size)` against `orig_size` before reading.
                let bytes =
                    unsafe { driver_binary_bytes(bin as *const ErlDrvBinary, size, offset) }?;
                stack.push(binary_term(env, bytes)?);
            }

            ERL_DRV_TUPLE => {
                let n = count_operand(*spec.get(ix)?)?;
                ix += 1;
                if n == 0 {
                    stack.push(prelude::tuple0()); // The C's shared empty-tuple literal.
                    continue;
                }
                if stack.len() < n {
                    return None;
                }
                let elems = stack.split_off(stack.len() - n);
                stack.push(env.heap().tuple(&elems).ok()?);
            }

            ERL_DRV_LIST => {
                let n = count_operand(*spec.get(ix)?)?;
                ix += 1;
                if n == 0 || stack.len() < n {
                    return None; // The C rejects `ERL_DRV_LIST 0` outright (`ptr[0] <= 0`).
                }
                let elems = stack.split_off(stack.len() - n);
                let mut acc = NIL;
                for elem in elems.into_iter().rev() {
                    acc = env.heap().cons(elem, acc).ok()?;
                }
                stack.push(acc);
            }

            ERL_DRV_PORT => {
                let term = Eterm::from_raw(*spec.get(ix)? as usize);
                ix += 1;
                if !prelude::is_internal_port(term) {
                    return None; // The C rejects a non-internal-port operand.
                }
                stack.push(term);
            }

            ERL_DRV_FLOAT => {
                // SAFETY: the `ERL_DRV_FLOAT` operand is a `double*`.
                let value = unsafe { read_scalar(*spec.get(ix)? as *const f64) }?;
                ix += 1;
                stack.push(float_term(env, value)?);
            }

            // `ERL_DRV_PID` / `ERL_DRV_EXT2TERM` / `ERL_DRV_MAP` and any unknown tag.
            _ => return None,
        }
    }

    // Exactly one value, array fully consumed (the C's final depth check).
    if stack.len() != 1 {
        return None;
    }
    stack.pop()
}

// Operand readers and term builders.

/// A count operand (`LIST`/`TUPLE` arity, `STRING`/`BUF2BINARY` length) as a `usize`.
///
/// The C reads these through an `(int)` cast and rejects a negative result, so a word whose low 32
/// bits are negative is malformed rather than an enormous count.
fn count_operand(word: ErlDrvTermData) -> Option<usize> {
    let n = word as i32;
    if n < 0 {
        None
    } else {
        Some(n as usize)
    }
}

/// A borrowed `&[u8]` view over the `len` bytes at `ptr`.
///
/// # Safety
///
/// `ptr` must point at `len` readable bytes (a null `ptr` is accepted only for `len == 0`).
unsafe fn buf<'a>(ptr: *const c_char, len: usize) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return if len == 0 { Some(&[]) } else { None };
    }
    // SAFETY: the caller guarantees `len` readable bytes at `ptr`.
    Some(unsafe { core::slice::from_raw_parts(ptr.cast::<u8>(), len) })
}

/// A borrowed `&[u8]` view over a NUL-terminated C string (`None` when the pointer is null).
///
/// # Safety
///
/// `ptr` must be null or point at a NUL-terminated byte sequence valid for the call.
unsafe fn cstr<'a>(ptr: *const c_char) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a NUL-terminated string; `CStr::from_ptr` only reads up to it.
    Some(unsafe { std::ffi::CStr::from_ptr(ptr) }.to_bytes())
}

/// Read one `Copy` value through `ptr` (`None` when the pointer is null).
///
/// # Safety
///
/// `ptr` must be null or point at one readable, properly-aligned value of type `T`.
unsafe fn read_scalar<T: Copy>(ptr: *const T) -> Option<T> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees one readable, aligned `T` at `ptr`.
    Some(unsafe { ptr.read() })
}

/// The `size` bytes at byte `offset` inside an `ErlDrvBinary` (`None` when malformed).
///
/// # Safety
///
/// `bin` must be null or point at a live `ErlDrvBinary` whose `orig_bytes` flexible array holds at
/// least `orig_size` bytes.
unsafe fn driver_binary_bytes<'a>(
    bin: *const ErlDrvBinary,
    size: ErlDrvTermData,
    offset: ErlDrvTermData,
) -> Option<&'a [u8]> {
    if bin.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a live `ErlDrvBinary` at `bin`.
    let orig_size = unsafe { (*bin).orig_size };
    if orig_size < 0 {
        return None;
    }
    // The C rejects `size + offset > b->orig_size` (an overflow-free checked add here).
    if offset.checked_add(size)? > orig_size as u64 {
        return None;
    }
    if size == 0 {
        return Some(&[]);
    }
    // SAFETY: the caller guarantees `orig_size` readable bytes at `orig_bytes`; `offset + size` is
    // within `orig_size` (checked above), so `base.add(offset)` heads `size` readable bytes.
    let base = unsafe { core::ptr::addr_of!((*bin).orig_bytes) } as *const u8;
    Some(unsafe { core::slice::from_raw_parts(base.add(offset as usize), size as usize) })
}

/// A list of small ints from `bytes` (unsigned, as the C's `buf_to_intlist`), ending in `tail`.
fn byte_list(env: &mut DriverEnv, bytes: &[u8], tail: Eterm) -> Option<Eterm> {
    let mut acc = tail;
    for &byte in bytes.iter().rev() {
        acc = env.heap().cons(prelude::make_small(byte as i64), acc).ok()?;
    }
    Some(acc)
}

/// An on-heap byte-aligned binary term from `bytes` (`None` on heap exhaustion).
fn binary_term(env: &mut DriverEnv, bytes: &[u8]) -> Option<Eterm> {
    let bits = bytes.len().checked_mul(8)?;
    // SAFETY: `heap_bits_size_words(n_bits)` sizes the `ErlHeapBits` block for `n_bits` bits.
    let hp = unsafe { env.heap().alloc_words(prelude::heap_bits_size_words(bits)) }.ok()?;
    // SAFETY: `hp` heads the reserved block and `bytes` provides the payload.
    unsafe { prelude::write_binary(hp, bytes) }.ok()
}

/// A float term from `value` (`None` for a non-finite value, which the C rejects).
fn float_term(env: &mut DriverEnv, value: f64) -> Option<Eterm> {
    if !value.is_finite() {
        return None;
    }
    // SAFETY: `FLOAT_SIZE_WORDS` (2) sizes the `[HEADER_FLONUM][bits]` block.
    let hp = unsafe { env.heap().alloc_words(prelude::FLOAT_SIZE_WORDS) }.ok()?;
    // SAFETY: `hp` heads the reserved 2-word block, both words of which are written.
    Some(unsafe { prelude::write_float(hp, value) })
}

/// `make_small` when the value fits, else a one-limb bignum of the right sign.
///
/// Equivalent to `nif/enif.rs`'s `small_or_big`, which cannot be shared without touching that
/// module (out of this step's scope); it is a pure function of `beam_term`'s public writers.
fn small_or_big(env: &mut DriverEnv, negative: bool, magnitude: u64) -> Option<Eterm> {
    let fits_small = if negative {
        magnitude <= prelude::MIN_SMALL.unsigned_abs()
    } else {
        magnitude <= prelude::MAX_SMALL as u64
    };
    if fits_small {
        let value = if negative {
            (magnitude as i64).wrapping_neg()
        } else {
            magnitude as i64
        };
        return Some(prelude::make_small(value));
    }
    // SAFETY: `big_size_words(1)` sizes the `[hdr][digit]` block.
    let hp = unsafe { env.heap().alloc_words(prelude::big_size_words(1)) }.ok()?;
    // SAFETY: `hp` heads the reserved `1 + 1`-word block, which `write_big` writes in full.
    Some(unsafe { prelude::write_big(hp, negative, &[magnitude]) })
}

/// An `ERL_DRV_INT` operand (`ErlDrvSInt`, a `long` on LP64) as a term.
fn int_from_i64(env: &mut DriverEnv, value: i64) -> Option<Eterm> {
    if value < 0 {
        small_or_big(env, true, value.unsigned_abs())
    } else {
        small_or_big(env, false, value as u64)
    }
}

/// An `ERL_DRV_UINT`/`ERL_DRV_UINT64` operand (`ErlDrvUInt`) as a term.
fn int_from_u64(env: &mut DriverEnv, value: u64) -> Option<Eterm> {
    small_or_big(env, false, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    use beam_term::atom::AtomTable;
    use beam_term::prelude::{
        atom_val, car, cdr, is_atom, is_big, is_list, is_small, is_tuple, make_atom, make_small,
        signed_value, tuple_element, tuple_size,
    };
    use std::ffi::CString;

    use crate::driver::types::{term_data, ERL_DRV_PID};

    /// A minimal [`DriverHost`]: a bump heap, an atom table, and recording output/term buffers.
    struct TestDriverHost {
        heap: Heap,
        atoms: AtomTable,
        output: Vec<u8>,
        sent: Vec<Eterm>,
    }

    impl TestDriverHost {
        fn new() -> TestDriverHost {
            TestDriverHost {
                heap: Heap::new(1024).expect("a small test heap is allocatable"),
                atoms: AtomTable::new(),
                output: Vec::new(),
                sent: Vec::new(),
            }
        }

        fn atoms(&self) -> &AtomTable {
            &self.atoms
        }
    }

    impl DriverHost for TestDriverHost {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }

        fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
            Some(make_atom(self.atoms.intern(name)))
        }

        fn output(&mut self, _port: ErlDrvPort, data: &[u8]) -> c_int {
            self.output.extend_from_slice(data);
            0
        }

        fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, term: Eterm) -> c_int {
            self.sent.push(term);
            0
        }
    }

    #[test]
    fn as_port_round_trips_through_from_port() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        assert!(!port.is_null(), "the port pointer is never null");
        // SAFETY: `port` came from `DriverEnv::as_port` on `env`, which is still alive here.
        let recovered = unsafe { DriverEnv::from_port(port) }.expect("non-null port");
        // The recovered env drives the very same host: a term sent through it lands in `host`.
        // (`DriverEnv::new` erases the borrow, so `host` is usable again here.)
        assert_eq!(recovered.host().send_term(port, None, make_small(7)), 0);
        assert_eq!(host.sent, vec![make_small(7)]);
    }

    #[test]
    fn from_port_of_null_is_none() {
        // SAFETY: a null pointer is explicitly allowed by the contract.
        let recovered = unsafe { DriverEnv::from_port(std::ptr::null_mut()) };
        assert!(recovered.is_none());
    }

    #[test]
    fn decode_term_spec_handles_an_atom() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let name = CString::new("hello").expect("no interior NUL");
        let spec = [ERL_DRV_ATOM, term_data(name.as_ptr() as usize)];
        let term = decode_term_spec(&mut env, &spec).expect("a valid atom spec");
        assert!(is_atom(term));
        assert_eq!(host.atoms().name(atom_val(term)).as_deref(), Some("hello"));
    }

    #[test]
    fn decode_term_spec_handles_ints_and_bignums() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);

        let term = decode_term_spec(&mut env, &[ERL_DRV_INT, 42]).expect("a valid int spec");
        assert!(is_small(term));
        assert_eq!(signed_value(term), 42);

        let term = decode_term_spec(&mut env, &[ERL_DRV_INT, (-1i64) as ErlDrvTermData])
            .expect("a negative int is valid");
        assert_eq!(signed_value(term), -1);

        // A magnitude outside the 60-bit small range becomes a bignum.
        let term =
            decode_term_spec(&mut env, &[ERL_DRV_UINT, u64::MAX]).expect("a valid uint spec");
        // SAFETY: `term` is a valid boxed term (a bignum, never a small int here).
        assert!(unsafe { is_big(term) });
    }

    #[test]
    fn decode_term_spec_handles_a_string() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let text = b"abc";
        let spec = [
            ERL_DRV_STRING,
            term_data(text.as_ptr() as usize),
            text.len() as ErlDrvTermData,
        ];
        let term = decode_term_spec(&mut env, &spec).expect("a valid string spec");
        // "abc" is the list [97, 98, 99].
        assert!(is_list(term));
        // SAFETY: `term` is a live three-element list.
        let (a, rest) = unsafe { (car(term), cdr(term)) };
        // SAFETY: `rest` is a live two-element list.
        let (b, rest) = unsafe { (car(rest), cdr(rest)) };
        // SAFETY: `rest` is a live one-element list.
        let (c, tail) = unsafe { (car(rest), cdr(rest)) };
        assert_eq!(
            [signed_value(a), signed_value(b), signed_value(c)],
            [97, 98, 99]
        );
        assert_eq!(tail, NIL);
    }

    #[test]
    fn decode_term_spec_handles_a_tuple_and_a_list() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let name = CString::new("a").expect("no interior NUL");

        // {a, 1}
        let spec = [
            ERL_DRV_ATOM,
            term_data(name.as_ptr() as usize),
            ERL_DRV_INT,
            1,
            ERL_DRV_TUPLE,
            2,
        ];
        let term = decode_term_spec(&mut env, &spec).expect("a valid tuple spec");
        // SAFETY: `term` is a live tuple (asserted by the size check below).
        assert!(unsafe { is_tuple(term) });
        // SAFETY: as above.
        assert_eq!(unsafe { tuple_size(term) }, 2);
        // SAFETY: element 0 of a 2-tuple.
        assert!(is_atom(unsafe { tuple_element(term, 0) }));
        // SAFETY: element 1 of a 2-tuple.
        assert_eq!(signed_value(unsafe { tuple_element(term, 1) }), 1);

        // [1, 2]
        let term = decode_term_spec(&mut env, &[ERL_DRV_INT, 1, ERL_DRV_INT, 2, ERL_DRV_LIST, 2])
            .expect("a valid list spec");
        assert!(is_list(term));
        // SAFETY: `term` is a live two-element list.
        let (head, rest) = unsafe { (car(term), cdr(term)) };
        // SAFETY: `rest` is a live one-element list.
        let (second, tail) = unsafe { (car(rest), cdr(rest)) };
        assert_eq!(signed_value(head), 1);
        assert_eq!(signed_value(second), 2);
        assert_eq!(tail, NIL);
    }

    #[test]
    fn decode_term_spec_rejects_malformed_and_unsupported_specs() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);

        // An empty spec leaves no value; a tag whose operands are missing; two values left on the
        // stack; a tuple arity larger than the stack; `ERL_DRV_LIST 0` (which the C rejects);
        // `ERL_DRV_PID` (like `EXT2TERM`/`MAP`, unsupported); a NULL string buffer; a null float
        // pointer.
        for spec in [
            &[] as &[ErlDrvTermData],
            &[ERL_DRV_INT],
            &[ERL_DRV_INT, 1, ERL_DRV_INT, 2],
            &[ERL_DRV_TUPLE, 1],
            &[ERL_DRV_LIST, 0],
            &[ERL_DRV_PID, 0],
            &[ERL_DRV_STRING, 0, 0],
            &[ERL_DRV_FLOAT, 0],
        ] {
            assert_eq!(decode_term_spec(&mut env, spec), None, "spec {spec:?}");
        }
    }

    #[test]
    fn the_new_family_hooks_default_to_unsupported() {
        let mut host = TestDriverHost::new();

        // Port queue: every hook reports failure / no data.
        assert_eq!(host.enq(b"abc", false), -1);
        assert_eq!(host.enq(b"abc", true), -1);
        assert_eq!(host.pushq(b"abc", false), -1);
        assert_eq!(host.pushq(b"abc", true), -1);
        assert_eq!(host.deq(4), -1);
        assert_eq!(host.sizeq(), -1);
        assert!(host.peekq().is_none());

        // Port attributes / flags.
        assert_eq!(host.set_busy_port(true), -1);
        assert_eq!(host.set_busy_port(false), -1);
        assert_eq!(host.set_control_flags(0, 0), -1);
        assert!(host.get_port_flags().is_none());

        // Lifecycle & failures: each `driver_failure*` spelling lands on the same hook.
        for reason in [
            PortFailure::Eof,
            PortFailure::Atom("enoent"),
            PortFailure::Posix(libc::ENOENT),
            PortFailure::Error(libc::EIO),
        ] {
            assert_eq!(host.failure(reason), -1);
        }
        assert_eq!(host.port_exit(1), -1);
        assert_eq!(host.busy_msgq_limits(1, 8), -1);
        assert_eq!(host.init_ack(), -1);
        assert_eq!(host.set_os_pid(42), -1);
        let mut key: c_uint = 7;
        assert_eq!(host.async_port_key(&mut key), -1);
        assert_eq!(key, 7, "an unsupported hook must not touch the out-parameter");

        // Monitors.
        assert!(host.monitor_process(NIL).is_none());
        assert_eq!(host.demonitor_process(core::ptr::null()), -1);
        assert!(host.get_monitored_process(core::ptr::null()).is_none());

        // Port data lock.
        assert!(host.pdl_create().is_none());
        assert_eq!(host.pdl_lock(core::ptr::null_mut()), -1);
        assert_eq!(host.pdl_unlock(core::ptr::null_mut()), -1);
        assert!(host.pdl_get_refc(core::ptr::null_mut()).is_none());
        assert!(host.pdl_inc_refc(core::ptr::null_mut()).is_none());
        assert!(host.pdl_dec_refc(core::ptr::null_mut()).is_none());

        // Deprecated wall clock.
        assert!(host.now().is_none());
    }
}
