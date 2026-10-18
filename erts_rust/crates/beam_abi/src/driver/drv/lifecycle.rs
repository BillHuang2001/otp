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

//! **Port lifecycle, failures & attributes** family of the driver host layer (part of the
//! `driver_*` / `erl_drv_*` C ABI, workspace contract **A.4**).
//!
//! This file owns the functions that close a port, set its busy/control state, describe its
//! limits/attributes and acknowledge an asynchronous init. Every one is a
//! `#[no_mangle] pub unsafe extern "C" fn` matching one field of the frozen
//! [`ErlDrvApi`](crate::driver::api::ErlDrvApi) table **exactly**, and every `ErlDrvPort` operand
//! **is** the pointer [`DriverEnv::as_port`](crate::driver::host::DriverEnv::as_port) hands out
//! ([`DriverEnv::from_port`](crate::driver::host::DriverEnv::from_port) recovers it through the
//! shared [`env_for`](super::env_for) helper, exactly as the sibling `port.rs`/`queues.rs` do).
//!
//! # Slots owned by this file
//!
//! | slot | C | state |
//! |---|---|---|
//! | `erl_drv_busy_msgq_limits` | `erl_port_task.c:771` | implemented |
//! | `driver_failure_eof` | `io.c:7278` | implemented |
//! | `driver_failure_atom` | `io.c:7263` | implemented |
//! | `driver_failure_posix` | `io.c:7273` | implemented |
//! | `driver_failure` | `io.c:7258` | implemented |
//! | `driver_exit` | `io.c:7234` | implemented |
//! | `set_busy_port` | `io.c:5208` | implemented |
//! | `set_port_control_flags` | `io.c:5318` | implemented |
//! | `get_port_flags` | `io.c:5325` | implemented |
//! | `erl_drv_init_ack` | `io.c:2932` | implemented |
//! | `erl_drv_set_os_pid` | `io.c:2970` | implemented |
//! | `driver_async_port_key` | `erl_async.c:518` | implemented |
//!
//! # The implemented family
//!
//! Every port-carrying member delegates to a
//! [`DriverHost`](crate::driver::host::DriverHost) hook, one per C entry point, so the runtime stays
//! the authority on what a "port" is:
//!
//! * the whole `driver_failure*` family collapses onto
//!   [`DriverHost::failure`](crate::driver::host::DriverHost::failure) with a
//!   [`PortFailure`](crate::driver::host::PortFailure) reason (`Eof` / `Atom(_)` / `Posix(_)` /
//!   `Error(_)`) — the four C spellings differ only in how the reason is expressed
//!   (`driver_failure_term`, `io.c:7196`);
//! * [`driver_exit`] routes to [`DriverHost::port_exit`](crate::driver::host::DriverHost::port_exit)
//!   (the C unlinks the connected process and then closes with `normal` / an errno atom,
//!   `io.c:7244-7253`);
//! * the attributes go to [`set_busy_port`](crate::driver::host::DriverHost::set_busy_port),
//!   [`set_control_flags`](crate::driver::host::DriverHost::set_control_flags) and
//!   [`get_port_flags`](crate::driver::host::DriverHost::get_port_flags), so a
//!   `set_port_control_flags` → `get_port_flags` **round-trips through the seam**;
//! * the async/lifecycle set goes to
//!   [`busy_msgq_limits`](crate::driver::host::DriverHost::busy_msgq_limits),
//!   [`init_ack`](crate::driver::host::DriverHost::init_ack),
//!   [`set_os_pid`](crate::driver::host::DriverHost::set_os_pid) and
//!   [`async_port_key`](crate::driver::host::DriverHost::async_port_key).
//!
//! The C's **failure value for an invalid port** is reproduced everywhere it exists (`-1` for the
//! `driver_failure*`/`driver_exit` family, `0` for `get_port_flags` and `driver_async_port_key`, a
//! silent return for `set_busy_port`/`set_port_control_flags`/`erl_drv_set_os_pid`,
//! `ERL_DRV_BUSY_MSGQ_DISABLED` for `erl_drv_busy_msgq_limits`) — the invalid-port operand is the C's
//! sentinel `ERTS_INVALID_ERL_DRV_PORT` (pointer `-1`, `erl_port.h:60-67`), and a `NULL` operand is
//! refused the same way through the one shared [`env_for`](super::env_for) guard, so neither is
//! dereferenced (the C does not test for `NULL` and faults; refusing it is a deliberate divergence).
//! A hook that reports "unsupported" (`-1`/`None`, the trait defaults) is reported
//! exactly as the C reports a port it cannot resolve: a driver following the documented `< 0` idiom
//! sees identical behaviour.
//!
//! # Slots owned elsewhere
//!
//! `add_driver_entry`, `remove_driver_entry`, `driver_lock_driver` and `driver_create_port` — the
//! four slots that act on the emulator's **process-global** driver list and on port creation instead
//! of one port's term services — live in the sibling [`registry`](super::registry) file, which
//! documents their C semantics and the thread-local host publication the two entry-list slots use.
//!
//! # Documented deviations from the oracle
//!
//! Each is also stated at the function that carries it; none of them can produce a *fabricated*
//! success (`CONTEXT.md` §3 — never a wrong result):
//!
//! * [`erl_drv_busy_msgq_limits`] — the seam's `busy_msgq_limits(low, high)` hook is a **setter**
//!   with `c_int` watermarks, while the C function is an in/out buffer. Consequently (a) the C's
//!   *read* form (a bound of `ERL_DRV_BUSY_MSGQ_READ_ONLY` = `0`, "report the current bound",
//!   `erl_port_task.c:804`) cannot be answered: the requested bounds are echoed back, so a
//!   read-only bound stays `0` — a value the C never reports as an effective bound, so "no value"
//!   stays distinguishable; (b) `ERL_DRV_BUSY_MSGQ_DISABLED` (`(ErlDrvSizeT)!0`) **and** any
//!   watermark above `i32::MAX` map to the seam's `-1` (a queue can never reach either, which is
//!   `DISABLED`'s meaning); (c) a null port or a host without the hook reports
//!   `ERL_DRV_BUSY_MSGQ_DISABLED` for both bounds — exactly the C's "no `bpq`" path.
//! * [`driver_failure_atom`] — the name must be **UTF-8** (the C interns the raw bytes as Latin-1,
//!   `io.c:7266`; the seam's `PortFailure::Atom(&str)` carries a Rust `&str`, the same convention
//!   `decode_term_spec`'s `ERL_DRV_ATOM` operand uses), and a **null** pointer is refused with `-1`
//!   where the C would fault inside `sys_strlen`.
//! * [`driver_exit`] — the seam's `port_exit(code)` carries the raw `int`; building the reason
//!   (`normal` for `0`, else the `erl_errno_id(err)` atom) and unlinking the connected process
//!   (`io.c:7244-7253`) is the host's job.
//! * [`set_busy_port`]/[`set_port_control_flags`] — the C returns `void`, so the hook's code is
//!   discarded; a null port is a silent no-op (the C's early return).
//! * [`set_port_control_flags`] — the frozen C function has a **single** `flags` operand
//!   (`erl_driver.h:399`); the seam's extra `mode` operand is passed `0`.
//! * [`get_port_flags`] — an unsupported host (`None`) is reported as `0`, which is exactly the
//!   value the C returns for a port it cannot resolve (`io.c:5332`). Note the seam's *source* of the
//!   value: the C derives `PORT_FLAG_BINARY`/`PORT_FLAG_LINE` from the port's IO state
//!   (`ERTS_PORT_SFLG_BINARY_IO`/`_LINEBUF_IO`, set by `open_port`'s `binary`/`line` options,
//!   `erl_bif_port.c:990`/`1141`) and `set_port_control_flags` writes a **different** field
//!   (`prt->control_flags`, `io.c:5320`) that no C function reads back. The seam routes both through
//!   `DriverHost`, which keeps them consistent, so `set_port_control_flags` → `get_port_flags`
//!   **round-trips through the C ABI** here — the seam's contract, not the C's field split.
//! * [`erl_drv_init_ack`] — `res` is compared against the three `ERL_DRV_ERROR_*` sentinels exactly
//!   as the C does (`erl_driver.h:176-191`); a **failure** `res` closes the port through
//!   [`port_exit(0)`](crate::driver::host::DriverHost::port_exit) — the C's
//!   `driver_failure_term(ix, am_normal, 0)` (`io.c:2955`) — and does **not** call the `init_ack`
//!   hook, because the C's reply there is an error atom this seam cannot spell and acking a failed
//!   init would fabricate a successful one. A non-failure `res` (including `NULL`) acks, as the C's
//!   reply is then the port id.
//! * [`erl_drv_set_os_pid`] — a pid outside `c_int` is refused (the C stores the full `ErlDrvSInt`,
//!   `io.c:2977`; truncating would record a *wrong* pid).
//! * [`driver_async_port_key`] — the key comes from the seam; `0` is reported for a null port (the
//!   C's `driver_mk_port` → `NIL` → `0`, `erl_async.c:521`) and for an unsupported host.
//!
//! # `unsafe`
//!
//! The whole file is FFI. Every `unsafe fn` carries a `# Safety` section (its caller contract) and
//! every `unsafe` block a preceding `// SAFETY:` rationale (workspace `CONTEXT.md` §3). All raw
//! operand reads go through the documented helpers [`read_limit`]/[`write_limit`]/[`cstr_bytes`]
//! and all pointer recovery through [`env_for`](super::env_for); no runtime data is `unwrap()`ed and
//! no failure path panics.

use libc::{c_char, c_int, c_uint};

use crate::driver::api::ErlDrvApi;
use crate::driver::host::PortFailure;
use crate::driver::types::{
    ErlDrvData, ErlDrvPort, ErlDrvSInt, ErlDrvSizeT, ERL_DRV_BUSY_MSGQ_DISABLED,
    ERL_DRV_ERROR_BADARG, ERL_DRV_ERROR_ERRNO, ERL_DRV_ERROR_GENERAL,
};
use super::env_for;

// ---------------------------------------------------------------------------------------------
// Raw-operand helpers
// ---------------------------------------------------------------------------------------------

/// The current value of the busy-message-queue out-parameter `p`, or `None` when it is absent.
///
/// The C treats a null bound pointer as "this bound was not supplied" (`erl_port_task.c:787-788`),
/// so `None` models the *operand*, not a value.
///
/// # Safety
///
/// `p` must be null or point at a readable `ErlDrvSizeT`.
unsafe fn read_limit(p: *mut ErlDrvSizeT) -> Option<ErlDrvSizeT> {
    if p.is_null() {
        None
    } else {
        // SAFETY: `p` is non-null and readable (caller contract).
        Some(unsafe { *p })
    }
}

/// Store `value` through the busy-message-queue out-parameter `p` (a no-op for a null pointer).
///
/// # Safety
///
/// `p` must be null or point at a writable `ErlDrvSizeT`.
unsafe fn write_limit(p: *mut ErlDrvSizeT, value: ErlDrvSizeT) {
    if !p.is_null() {
        // SAFETY: `p` is non-null and writable (caller contract).
        unsafe { *p = value };
    }
}

/// The bytes of the NUL-terminated C string at `ptr`, without the terminator (`None` when null).
///
/// # Safety
///
/// `ptr` must be null or point at a NUL-terminated byte string.
unsafe fn cstr_bytes<'a>(ptr: *const c_char) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0usize;
    // SAFETY: the caller guarantees a NUL-terminated string, so the scan stops at the terminator and
    // never leaves the allocation.
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: the `len` bytes before the terminator are readable (caller contract).
    Some(unsafe { core::slice::from_raw_parts(ptr as *const u8, len) })
}

/// The seam's `c_int` spelling of a busy-message-queue watermark.
///
/// `ERL_DRV_BUSY_MSGQ_DISABLED` is `(ErlDrvSizeT)!0`, whose `c_int` spelling is `-1`; a watermark
/// above `i32::MAX` can never be reached by a port queue — which *is* `DISABLED`'s meaning — so it
/// maps there too. Every other value passes through unchanged.
fn limit_to_c_int(value: ErlDrvSizeT) -> c_int {
    if value == ERL_DRV_BUSY_MSGQ_DISABLED {
        return -1;
    }
    c_int::try_from(value).unwrap_or(-1)
}

// ---------------------------------------------------------------------------------------------
// Failures & exit — the `driver_failure*` family
// ---------------------------------------------------------------------------------------------

/// `int driver_failure_eof(ErlDrvPort port);` — close the port because the connected process reached
/// end of file (the C's `driver_failure_term(ix, NIL, 1)`, `io.c:7278`).
///
/// Returns `0` for a successful close request and `-1` for an operand that names no port — the C's
/// invalid-port sentinel `ERTS_INVALID_ERL_DRV_PORT` (`io.c:7204`, the faithful `-1`) or `NULL`
/// (refused here where the C would fault) — or for a host without the hook.
///
/// # Safety
///
/// `port` must be null or a live port handle (see [`env_for`](super::env_for)).
#[no_mangle]
pub unsafe extern "C" fn driver_failure_eof(port: ErlDrvPort) -> c_int {
    // SAFETY: the caller's contract is `env_for`'s contract, restated above.
    match unsafe { env_for(port) } {
        Some(env) => env.host().failure(PortFailure::Eof),
        None => -1,
    }
}

/// `int driver_failure_atom(ErlDrvPort port, char *string);` — close the port with `string` as the
/// reason atom (the C's `driver_failure_term(ix, erts_atom_put(string), 0)`, `io.c:7263`).
///
/// Returns `0`/`-1` like [`driver_failure_eof`]. A **null** `string` is refused with `-1` (the C
/// would fault) and a name that is not UTF-8 with `-1` too — see the module header's deviations.
///
/// # Safety
///
/// `port` must be null or a live port handle; `string` must be null or point at a NUL-terminated
/// byte string.
#[no_mangle]
pub unsafe extern "C" fn driver_failure_atom(port: ErlDrvPort, string: *mut c_char) -> c_int {
    // SAFETY: `string` is the caller's NUL-terminated C string (contract above).
    let Some(bytes) = (unsafe { cstr_bytes(string) }) else {
        return -1;
    };
    let Ok(name) = core::str::from_utf8(bytes) else {
        return -1; // The seam's reason is a `&str`; the C's Latin-1 bytes cannot be spelled.
    };
    // SAFETY: `port` is null or a live handle (contract above).
    match unsafe { env_for(port) } {
        Some(env) => env.host().failure(PortFailure::Atom(name)),
        None => -1,
    }
}

/// `int driver_failure_posix(ErlDrvPort port, int error);` — close the port with the POSIX reason
/// `error` names (the C's `driver_failure_atom(ix, erl_errno_id(err))`, `io.c:7273`).
///
/// Returns `0`/`-1` like [`driver_failure_eof`]. The errno → reason mapping is the host's; the raw
/// errno is what crosses the seam.
///
/// # Safety
///
/// `port` must be null or a live port handle.
#[no_mangle]
pub unsafe extern "C" fn driver_failure_posix(port: ErlDrvPort, error: c_int) -> c_int {
    // SAFETY: `port` is null or a live handle (contract above).
    match unsafe { env_for(port) } {
        Some(env) => env.host().failure(PortFailure::Posix(error)),
        None => -1,
    }
}

/// `int driver_failure(ErlDrvPort port, int error);` — close the port with the error number (the
/// C's `driver_failure_term(ix, make_small(code), code == 0)`, `io.c:7258`).
///
/// Returns `0`/`-1` like [`driver_failure_eof`]. The C's third argument is the **eof flag**, and it
/// is `code == 0`: a `0` code takes the eof route with the reason discarded
/// (`erts_deliver_port_exit(prt, id, eof ? am_normal : term, …)`), so it is reported to the host as
/// [`PortFailure::Eof`] — exactly what [`driver_failure_eof`] reports, and what the port owner
/// observes as the reason `normal` — while every other code is
/// [`PortFailure::Error(code)`](PortFailure::Error) (the bare integer). The eof route's further
/// consequences (buffered `[line]` data flushed before the exit, and `{Port,eof}` with the port left
/// **alive** on a port opened with the `eof` option) are the host's, under
/// [`DriverHost::failure`](crate::driver::host::DriverHost::failure).
///
/// # Safety
///
/// `port` must be null or a live port handle.
#[no_mangle]
pub unsafe extern "C" fn driver_failure(port: ErlDrvPort, error: c_int) -> c_int {
    // SAFETY: `port` is null or a live handle (contract above).
    match unsafe { env_for(port) } {
        // `error == 0` **is** the C's `eof` flag, which discards the reason term: same request as
        // `driver_failure_eof`, hence the same variant (never `Error(0)`).
        Some(env) => env.host().failure(if error == 0 {
            PortFailure::Eof
        } else {
            PortFailure::Error(error)
        }),
        None => -1,
    }
}

/// `int driver_exit(ErlDrvPort port, int err);` — terminate the port's connected process
/// (`io.c:7234`).
///
/// The C unlinks the connected process and then closes the port with `normal` (`err == 0`) or with
/// the atom `erl_errno_id(err)` names; it returns `-1` for an invalid port and `0` otherwise. This
/// implementation hands the raw `err` to [`DriverHost::port_exit`], which owns both the unlink and
/// the reason derivation, and reports `-1` for a null port or a host without the hook.
///
/// # Safety
///
/// `port` must be null or a live port handle.
#[no_mangle]
pub unsafe extern "C" fn driver_exit(port: ErlDrvPort, err: c_int) -> c_int {
    // SAFETY: `port` is null or a live handle (contract above).
    match unsafe { env_for(port) } {
        Some(env) => env.host().port_exit(err),
        None => -1,
    }
}

// ---------------------------------------------------------------------------------------------
// Port attributes / flags
// ---------------------------------------------------------------------------------------------

/// `void set_busy_port(ErlDrvPort port, int on);` — mark the port busy (`on != 0`) or idle
/// (`io.c:5208`).
///
/// The C returns `void` (it only moves the port's scheduler flags), so the host's code is discarded;
/// an operand that names no port — the C's invalid-port sentinel `ERTS_INVALID_ERL_DRV_PORT` (its
/// early return) or `NULL` (refused here rather than faulted) — is a silent no-op.
///
/// # Safety
///
/// `port` must be null or a live port handle.
#[no_mangle]
pub unsafe extern "C" fn set_busy_port(port: ErlDrvPort, on: c_int) {
    // SAFETY: `port` is null or a live handle (contract above).
    if let Some(env) = unsafe { env_for(port) } {
        let _ = env.host().set_busy_port(on != 0);
    }
}

/// `void set_port_control_flags(ErlDrvPort port, int flags);` — set the port's control flags
/// (`PORT_CONTROL_FLAG_*`, `io.c:5318`).
///
/// The frozen C function takes a **single** `flags` operand, so the seam's extra `mode` operand is
/// passed `0`. The C returns `void`, so the host's code is discarded; a null port is a silent no-op.
/// The value set here is what [`get_port_flags`] must report back.
///
/// # Safety
///
/// `port` must be null or a live port handle.
#[no_mangle]
pub unsafe extern "C" fn set_port_control_flags(port: ErlDrvPort, flags: c_int) {
    // SAFETY: `port` is null or a live handle (contract above).
    if let Some(env) = unsafe { env_for(port) } {
        let _ = env.host().set_control_flags(0, flags);
    }
}

/// `int get_port_flags(ErlDrvPort port);` — the port's flags (`PORT_FLAG_*`, `io.c:5325`).
///
/// The C derives `PORT_FLAG_BINARY`/`PORT_FLAG_LINE` from the port's state and returns `0` for a
/// port it cannot resolve; this implementation returns whatever the host recorded through
/// `set_port_control_flags`, and `0` for a null port or an unsupported host — the same value the C
/// gives for an unresolvable port.
///
/// # Safety
///
/// `port` must be null or a live port handle.
#[no_mangle]
pub unsafe extern "C" fn get_port_flags(port: ErlDrvPort) -> c_int {
    // SAFETY: `port` is null or a live handle (contract above).
    match unsafe { env_for(port) } {
        Some(env) => env.host().get_port_flags().unwrap_or(0),
        None => 0,
    }
}

// ---------------------------------------------------------------------------------------------
// Busy-message-queue window, async-init ack, OS pid, async key
// ---------------------------------------------------------------------------------------------

/// `void erl_drv_busy_msgq_limits(ErlDrvPort port, ErlDrvSizeT *low, ErlDrvSizeT *high);` — read or
/// write the port's busy-message-queue watermarks (`erl_port_task.c:771`).
///
/// Requests are forwarded to [`DriverHost::busy_msgq_limits`]; the outcome reproduces the C's two
/// observable paths:
///
/// * a null/unreachable port, or a host without the hook, reports **`ERL_DRV_BUSY_MSGQ_DISABLED`**
///   for both bounds — the C's "no `bpq`" answer (`erl_port_task.c:779-785`);
/// * otherwise the requested bounds are echoed back, since the seam applies them verbatim (the C
///   writes back its own possibly-clamped `low`/`high`). A bound the caller left at
///   `ERL_DRV_BUSY_MSGQ_READ_ONLY` (`0`, the C's *read* form) therefore stays `0`: the seam is a
///   setter and cannot report the current bound — see the module header's deviations.
///
/// A null bound pointer is "not supplied" (the C's `lowp ? *lowp : 0`); every write below is skipped
/// for it.
///
/// # Safety
///
/// `port` must be null or a live port handle; each bound pointer must be null or point at a readable
/// *and writable* `ErlDrvSizeT`.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_busy_msgq_limits(
    port: ErlDrvPort,
    low: *mut ErlDrvSizeT,
    high: *mut ErlDrvSizeT,
) {
    // SAFETY: both bound pointers are null or readable/writable (contract above).
    let (low_in, high_in) = unsafe { (read_limit(low), read_limit(high)) };

    // SAFETY: `port` is null or a live handle (contract above).
    let Some(env) = (unsafe { env_for(port) }) else {
        // SAFETY: the bound pointers are null or writable (contract above).
        unsafe {
            write_limit(low, ERL_DRV_BUSY_MSGQ_DISABLED);
            write_limit(high, ERL_DRV_BUSY_MSGQ_DISABLED);
        }
        return; // The C's invalid-port path (`erl_port_task.c:779`).
    };

    let applied = env.host().busy_msgq_limits(
        limit_to_c_int(low_in.unwrap_or(0)),
        limit_to_c_int(high_in.unwrap_or(0)),
    );
    if applied < 0 {
        // The host has no busy-message-queue feature: the C's `!bpq` answer.
        // SAFETY: the bound pointers are null or writable (contract above).
        unsafe {
            write_limit(low, ERL_DRV_BUSY_MSGQ_DISABLED);
            write_limit(high, ERL_DRV_BUSY_MSGQ_DISABLED);
        }
        return;
    }
    // SAFETY: the bound pointers are null or writable (contract above). Echoing `Some(v)` where `v`
    // came from the same pointer is a no-op write for a read-only bound — deliberate.
    unsafe {
        if let Some(v) = low_in {
            write_limit(low, v);
        }
        if let Some(v) = high_in {
            write_limit(high, v);
        }
    }
}

/// `void erl_drv_init_ack(ErlDrvPort ix, ErlDrvData res);` — acknowledge an asynchronous port init
/// (`io.c:2932`).
///
/// The C inspects the **words** of `res`: exactly the three failure sentinels
/// `ERL_DRV_ERROR_BADARG`/`_ERRNO`/`_GENERAL` (`erl_driver.h:176-191`, `-3`/`-2`/`-1`) name a failed
/// init, anything else (including `NULL`, which drivers use for "no private state") is the port's
/// id, i.e. a **successful** ack. It then sends that reply and, on a failure, additionally closes
/// the port with `normal` (`driver_failure_term(ix, am_normal, 0)`, `io.c:2951-2955`).
///
/// The seam's [`DriverHost::init_ack`] carries no result operand, so:
///
/// * a **successful** `res` calls [`init_ack`](crate::driver::host::DriverHost::init_ack) — the
///   reply is the port itself, which is what the hook means;
/// * a **failure** `res` calls
///   [`port_exit(0)`](crate::driver::host::DriverHost::port_exit) instead — the very close the C
///   performs (the same code path `driver_exit(ix, 0)` reaches: `io.c:7244-7247`), and *not*
///   `init_ack`, because the C's reply there is an error atom this seam cannot spell and acking a
///   failed init would fabricate a successful one.
///
/// A null port is a no-op (the C would dereference it, `io.c:2935`).
///
/// # Safety
///
/// `port` must be null or a live port handle; `res` must be the `ErlDrvData` the driver's `start`
/// callback produced (an opaque pointer, or one of the `ERL_DRV_ERROR_*` sentinels).
#[no_mangle]
pub unsafe extern "C" fn erl_drv_init_ack(port: ErlDrvPort, res: ErlDrvData) {
    let word = res as isize;
    let failed = word == ERL_DRV_ERROR_BADARG as isize
        || word == ERL_DRV_ERROR_ERRNO as isize
        || word == ERL_DRV_ERROR_GENERAL as isize;
    // SAFETY: `port` is null or a live handle (contract above).
    let Some(env) = (unsafe { env_for(port) }) else {
        return;
    };
    if failed {
        // The C's failed-init close (`io.c:2955`), spelled through the hook `driver_exit(_, 0)`
        // also routes to.
        let _ = env.host().port_exit(0);
    } else {
        let _ = env.host().init_ack();
    }
}

/// `void erl_drv_set_os_pid(ErlDrvPort ix, ErlDrvSInt pid);` — record the port's OS pid for
/// `port_info(_, os_pid)` (`io.c:2970`).
///
/// The C returns immediately for an invalid port and otherwise stores the full `ErlDrvSInt`; this
/// implementation forwards the pid to [`DriverHost::set_os_pid`], whose operand is a `c_int`. A pid
/// outside `c_int` is **refused** rather than truncated (a truncated pid would be a wrong result),
/// and a null port is a no-op.
///
/// # Safety
///
/// `port` must be null or a live port handle.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_set_os_pid(port: ErlDrvPort, pid: ErlDrvSInt) {
    let Ok(pid) = c_int::try_from(pid) else {
        return; // The seam's operand is narrower than the C's `ErlDrvSInt`.
    };
    // SAFETY: `port` is null or a live handle (contract above).
    if let Some(env) = unsafe { env_for(port) } {
        let _ = env.host().set_os_pid(pid);
    }
}

/// `unsigned int driver_async_port_key(ErlDrvPort port);` — a fair async key for the port
/// (`erl_async.c:518`).
///
/// The C has no failure code: it derives the key from the port's id term and returns `0` when the
/// port cannot be resolved (`driver_mk_port` → `NIL`). This implementation returns the key the host
/// provides and `0` for a null port or an unsupported host.
///
/// # Safety
///
/// `port` must be null or a live port handle.
#[no_mangle]
pub unsafe extern "C" fn driver_async_port_key(port: ErlDrvPort) -> c_uint {
    // SAFETY: `port` is null or a live handle (contract above).
    let Some(env) = (unsafe { env_for(port) }) else {
        return 0; // The C's invalid-port answer (`erl_async.c:521`).
    };
    let mut key: c_uint = 0;
    if env.host().async_port_key(&mut key) < 0 {
        return 0; // An unsupported host: the port has no key.
    }
    key
}

// ---------------------------------------------------------------------------------------------
// Table assembly (the `populate` chain step owned by this file)
// ---------------------------------------------------------------------------------------------

/// Populate this family's slots of the frozen [`ErlDrvApi`].
///
/// Called from [`super::DRIVER_API`]'s `populate` chain, so this file — not `mod.rs` — owns the
/// `Some(…)` list for the functions it defines.
pub(super) const fn populate(mut api: ErlDrvApi) -> ErlDrvApi {
    api.erl_drv_busy_msgq_limits = Some(erl_drv_busy_msgq_limits);
    api.driver_failure_eof = Some(driver_failure_eof);
    api.driver_failure_atom = Some(driver_failure_atom);
    api.driver_failure_posix = Some(driver_failure_posix);
    api.driver_failure = Some(driver_failure);
    api.driver_exit = Some(driver_exit);
    api.set_busy_port = Some(set_busy_port);
    api.set_port_control_flags = Some(set_port_control_flags);
    api.get_port_flags = Some(get_port_flags);
    api.erl_drv_init_ack = Some(erl_drv_init_ack);
    api.erl_drv_set_os_pid = Some(erl_drv_set_os_pid);
    api.driver_async_port_key = Some(driver_async_port_key);
    api
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use beam_alloc::Heap;
    use beam_term::atom::AtomTable;
    use beam_term::prelude::{make_atom, Eterm};

    use crate::driver::host::{DriverEnv, DriverHost};

    /// `PORT_CONTROL_FLAG_BINARY` (`erl_driver.h:98`).
    const CONTROL_FLAG_BINARY: c_int = 1 << 0;
    /// `PORT_CONTROL_FLAG_HEAVY` (`erl_driver.h:99`).
    const CONTROL_FLAG_HEAVY: c_int = 1 << 1;

    /// A host that records everything this family drives through the seam.
    ///
    /// It plays the part of the runtime: `set_control_flags` is the one hook whose effect is
    /// readable again (`get_port_flags`), so the round-trip can be pinned; every other hook records
    /// the request. With `unsupported = true` every hook reports the trait default, which pins the
    /// "host without the service" behaviour.
    struct LifecycleHost {
        heap: Heap,
        atoms: AtomTable,
        /// Every `failure` reason, in call order (`PortFailure`'s `Debug` spelling).
        failures: Vec<String>,
        /// Every `port_exit` code.
        exits: Vec<c_int>,
        /// Every `set_busy_port` request.
        busy: Vec<bool>,
        /// Every `set_control_flags` request: `(mode, flags)`.
        control: Vec<(c_int, c_int)>,
        /// What `get_port_flags` reports (the last value `set_control_flags` was given).
        flags: Option<c_int>,
        /// Every `busy_msgq_limits` request.
        limits: Vec<(c_int, c_int)>,
        /// The number of `init_ack` calls.
        acks: usize,
        /// Every `set_os_pid` request.
        os_pids: Vec<c_int>,
        /// The key `async_port_key` writes (`None` = unsupported).
        key: Option<c_uint>,
        /// When true every hook reports "unsupported" (`-1`/`None`).
        unsupported: bool,
    }

    impl LifecycleHost {
        fn new() -> LifecycleHost {
            LifecycleHost {
                heap: Heap::new(4096).expect("a test heap is allocatable"),
                atoms: AtomTable::new(),
                failures: Vec::new(),
                exits: Vec::new(),
                busy: Vec::new(),
                control: Vec::new(),
                flags: None,
                limits: Vec::new(),
                acks: 0,
                os_pids: Vec::new(),
                key: Some(0xABCD),
                unsupported: false,
            }
        }
    }

    impl DriverHost for LifecycleHost {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }

        fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
            Some(make_atom(self.atoms.intern(name)))
        }

        fn output(&mut self, _port: ErlDrvPort, _data: &[u8]) -> c_int {
            0
        }

        fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, _term: Eterm) -> c_int {
            0
        }

        fn set_busy_port(&mut self, busy: bool) -> c_int {
            if self.unsupported {
                return -1;
            }
            self.busy.push(busy);
            0
        }

        fn set_control_flags(&mut self, mode: c_int, flags: c_int) -> c_int {
            if self.unsupported {
                return -1;
            }
            self.control.push((mode, flags));
            self.flags = Some(flags);
            0
        }

        fn get_port_flags(&mut self) -> Option<c_int> {
            self.flags
        }

        fn failure(&mut self, reason: PortFailure<'_>) -> c_int {
            if self.unsupported {
                return -1;
            }
            self.failures.push(format!("{reason:?}"));
            0
        }

        fn port_exit(&mut self, code: c_int) -> c_int {
            if self.unsupported {
                return -1;
            }
            self.exits.push(code);
            0
        }

        fn busy_msgq_limits(&mut self, low: c_int, high: c_int) -> c_int {
            if self.unsupported {
                return -1;
            }
            self.limits.push((low, high));
            0
        }

        fn init_ack(&mut self) -> c_int {
            if self.unsupported {
                return -1;
            }
            self.acks += 1;
            0
        }

        fn set_os_pid(&mut self, pid: c_int) -> c_int {
            if self.unsupported {
                return -1;
            }
            self.os_pids.push(pid);
            0
        }

        fn async_port_key(&mut self, key: *mut c_uint) -> c_int {
            let Some(k) = self.key else {
                return -1; // An unsupported hook must not touch the out-parameter.
            };
            if key.is_null() {
                return -1;
            }
            // SAFETY: the hook's out-parameter contract is a writable `c_uint`.
            unsafe { *key = k };
            0
        }
    }

    // -----------------------------------------------------------------------------------------
    // `driver_failure*` / `driver_exit`
    // -----------------------------------------------------------------------------------------

    #[test]
    fn the_failure_family_routes_each_c_spelling_to_one_reason() {
        let mut host = LifecycleHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        let name = c"enoent";

        // SAFETY: `port` came from `DriverEnv::as_port` on a live env; `name` is NUL-terminated.
        unsafe {
            assert_eq!(driver_failure_eof(port), 0);
            assert_eq!(driver_failure_atom(port, name.as_ptr() as *mut c_char), 0);
            assert_eq!(driver_failure_posix(port, libc::ENOENT), 0);
            assert_eq!(driver_failure(port, 0), 0);
            assert_eq!(driver_failure(port, 7), 0);
        }
        assert_eq!(
            host.failures,
            [
                "Eof".to_string(),
                "Atom(\"enoent\")".to_string(),
                format!("Posix({})", libc::ENOENT),
                // `driver_failure(P, 0)` is the C's **eof route** (`driver_failure_term(ix,
                // make_small(0), code == 0)`, `io.c:7258`): the term is discarded, so the owner
                // observes the measured reason `normal` — identical to `driver_failure_eof`.
                "Eof".to_string(),
                "Error(7)".to_string(),
            ],
            "each C entry point must land on the one `failure` hook with its own reason"
        );
    }

    #[test]
    fn a_null_or_sentinel_port_is_the_c_failure_value_and_is_never_dereferenced() {
        let mut host = LifecycleHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        let name = c"enoent";
        // `NULL` and the C's invalid-port sentinel (`ERTS_INVALID_ERL_DRV_PORT`, pointer `-1`,
        // `erl_port.h:60-67`) both name no environment and must answer the same failure values.
        let absent = [
            core::ptr::null_mut(),
            crate::driver::types::ERTS_INVALID_ERL_DRV_PORT,
        ];

        for operand in absent {
            // SAFETY: neither operand names an environment (`env_for`'s documented `None`); `name`
            // is NUL-terminated; `port` is live, which proves the absent calls record nothing.
            unsafe {
                assert_eq!(driver_failure_eof(operand), -1);
                assert_eq!(driver_failure_atom(operand, name.as_ptr() as *mut c_char), -1);
                assert_eq!(driver_failure_posix(operand, 1), -1);
                assert_eq!(driver_failure(operand, 0), -1, "the eof route is refused too");
                assert_eq!(driver_failure(operand, 1), -1);
                assert_eq!(driver_exit(operand, 1), -1);
                assert_eq!(get_port_flags(operand), 0);
                assert_eq!(driver_async_port_key(operand), 0);
                set_busy_port(operand, 1);
                set_port_control_flags(operand, CONTROL_FLAG_BINARY);
                erl_drv_set_os_pid(operand, 42);
                erl_drv_init_ack(operand, std::ptr::dangling_mut());
            }
        }
        assert!(host.failures.is_empty(), "an absent port must record nothing");
        assert!(host.exits.is_empty());
        assert!(host.busy.is_empty());
        assert!(host.control.is_empty());
        assert!(host.os_pids.is_empty());
        assert_eq!(host.acks, 0);
        // SAFETY: `port` is live.
        assert_eq!(unsafe { get_port_flags(port) }, 0, "the live port is untouched");
    }

    #[test]
    fn a_null_or_non_utf8_atom_name_is_refused_not_faulted() {
        let mut host = LifecycleHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        let bad = [0xffu8, 0];

        // SAFETY: `port` is live; `bad` is NUL-terminated; a null string is `None` by contract.
        unsafe {
            assert_eq!(driver_failure_atom(port, core::ptr::null_mut()), -1);
            assert_eq!(
                driver_failure_atom(port, bad.as_ptr() as *mut c_char),
                -1,
                "the seam's reason is a `&str`, so non-UTF-8 Latin-1 bytes are refused"
            );
        }
        assert!(host.failures.is_empty());
    }

    #[test]
    fn driver_exit_hands_the_raw_code_to_the_host() {
        let mut host = LifecycleHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        // SAFETY: `port` is live.
        unsafe {
            assert_eq!(driver_exit(port, 0), 0);
            assert_eq!(driver_exit(port, 3), 0);
        }
        assert_eq!(host.exits, [0, 3]);
    }

    // -----------------------------------------------------------------------------------------
    // Attributes / flags
    // -----------------------------------------------------------------------------------------

    #[test]
    fn the_busy_flag_is_forwarded_and_control_flags_round_trip() {
        let mut host = LifecycleHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        // SAFETY: `port` is live.
        unsafe {
            set_busy_port(port, 1);
            set_busy_port(port, 0);
            assert_eq!(get_port_flags(port), 0, "a fresh port reports no flags");
            set_port_control_flags(port, CONTROL_FLAG_BINARY);
            assert_eq!(get_port_flags(port), CONTROL_FLAG_BINARY);
            set_port_control_flags(port, CONTROL_FLAG_HEAVY);
            assert_eq!(get_port_flags(port), CONTROL_FLAG_HEAVY);
            set_port_control_flags(port, 0);
            assert_eq!(get_port_flags(port), 0);
        }
        assert_eq!(host.busy, [true, false]);
        assert_eq!(
            host.control,
            [(0, CONTROL_FLAG_BINARY), (0, CONTROL_FLAG_HEAVY), (0, 0)],
            "the C has a single `flags` operand, so the seam's `mode` operand is 0"
        );
    }

    // -----------------------------------------------------------------------------------------
    // Busy-message-queue limits
    // -----------------------------------------------------------------------------------------

    #[test]
    fn busy_msgq_limits_are_forwarded_and_echoed_back() {
        let mut host = LifecycleHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        let mut low: ErlDrvSizeT = 2;
        let mut high: ErlDrvSizeT = 8;

        // SAFETY: `port` is live and both bound pointers are writable.
        unsafe { erl_drv_busy_msgq_limits(port, &mut low, &mut high) };
        assert_eq!(host.limits, [(2, 8)]);
        assert_eq!((low, high), (2, 8), "the applied bounds are echoed back");

        // The disable sentinel travels as the seam's `-1`.
        let mut low: ErlDrvSizeT = ERL_DRV_BUSY_MSGQ_DISABLED;
        let mut high: ErlDrvSizeT = ERL_DRV_BUSY_MSGQ_DISABLED;
        // SAFETY: as above.
        unsafe { erl_drv_busy_msgq_limits(port, &mut low, &mut high) };
        assert_eq!(host.limits, [(2, 8), (-1, -1)]);
        assert_eq!(
            (low, high),
            (ERL_DRV_BUSY_MSGQ_DISABLED, ERL_DRV_BUSY_MSGQ_DISABLED)
        );

        // A read-only bound (`0`) stays `0`: the seam is a setter and cannot report a bound.
        let mut low: ErlDrvSizeT = 0;
        let mut high: ErlDrvSizeT = 4;
        // SAFETY: as above.
        unsafe { erl_drv_busy_msgq_limits(port, &mut low, &mut high) };
        assert_eq!(host.limits.last(), Some(&(0, 4)));
        assert_eq!((low, high), (0, 4));

        // An absent bound pointer is "not supplied", and no write may be attempted through it.
        let mut high: ErlDrvSizeT = 3;
        // SAFETY: a null (absent) low bound and a writable high bound.
        unsafe { erl_drv_busy_msgq_limits(port, core::ptr::null_mut(), &mut high) };
        assert_eq!(host.limits.last(), Some(&(0, 3)));
        assert_eq!(high, 3);
    }

    #[test]
    fn busy_msgq_limits_report_disabled_for_an_unreachable_or_bare_host() {
        let mut host = LifecycleHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        let mut low: ErlDrvSizeT = 1;
        let mut high: ErlDrvSizeT = 2;

        // A null port never reaches the host — the C's invalid-port path.
        // SAFETY: a null port and two writable bound pointers.
        unsafe { erl_drv_busy_msgq_limits(core::ptr::null_mut(), &mut low, &mut high) };
        assert_eq!(
            (low, high),
            (ERL_DRV_BUSY_MSGQ_DISABLED, ERL_DRV_BUSY_MSGQ_DISABLED)
        );
        assert!(host.limits.is_empty());

        // A host without the hook is the C's "no `bpq`" answer.
        host.unsupported = true;
        let mut low: ErlDrvSizeT = 1;
        let mut high: ErlDrvSizeT = 2;
        // SAFETY: `port` is live and both bound pointers are writable.
        unsafe { erl_drv_busy_msgq_limits(port, &mut low, &mut high) };
        assert_eq!(
            (low, high),
            (ERL_DRV_BUSY_MSGQ_DISABLED, ERL_DRV_BUSY_MSGQ_DISABLED)
        );
        assert!(host.limits.is_empty(), "the unsupported host records nothing");
    }

    // -----------------------------------------------------------------------------------------
    // Async init ack / OS pid / async key
    // -----------------------------------------------------------------------------------------

    #[test]
    fn init_ack_acknowledges_success_and_closes_on_a_failure_result() {
        let mut host = LifecycleHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        // SAFETY: `port` is live; the operands have the C's `ErlDrvData` shape.
        unsafe {
            erl_drv_init_ack(port, std::ptr::dangling_mut());
            erl_drv_init_ack(port, core::ptr::null_mut());
        }
        assert_eq!(host.acks, 2, "a pointer (or NULL) result is the C's success reply");
        assert!(host.exits.is_empty());

        // A failure sentinel does not ack: the C sends an error reply (not expressible here) and
        // closes the port with `normal` — `port_exit(0)`, the code `driver_exit(_, 0)` reaches.
        // SAFETY: `port` is live.
        unsafe {
            erl_drv_init_ack(port, ERL_DRV_ERROR_BADARG);
            erl_drv_init_ack(port, ERL_DRV_ERROR_ERRNO);
            erl_drv_init_ack(port, ERL_DRV_ERROR_GENERAL);
        }
        assert_eq!(host.acks, 2, "a failed init must never be acknowledged");
        assert_eq!(host.exits, [0, 0, 0]);

        // A null port reaches no hook at all.
        // SAFETY: a null port, which is `env_for`'s documented `None`.
        unsafe { erl_drv_init_ack(core::ptr::null_mut(), std::ptr::dangling_mut()) };
        assert_eq!((host.acks, host.exits.len()), (2, 3));
    }

    #[test]
    fn set_os_pid_is_forwarded_and_refuses_an_unrepresentable_pid() {
        let mut host = LifecycleHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        // SAFETY: `port` is live.
        unsafe {
            erl_drv_set_os_pid(port, 4242);
            erl_drv_set_os_pid(port, -1); // a legal `ErlDrvSInt`, still a `c_int`
            erl_drv_set_os_pid(port, 1 << 40); // outside `c_int`: refused, never truncated
        }
        assert_eq!(host.os_pids, [4242, -1]);
    }

    #[test]
    fn the_async_port_key_comes_from_the_host_and_zero_when_it_cannot() {
        let mut host = LifecycleHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        // SAFETY: `port` is live.
        assert_eq!(unsafe { driver_async_port_key(port) }, 0xABCD);

        host.key = None;
        // SAFETY: `port` is live.
        assert_eq!(unsafe { driver_async_port_key(port) }, 0);
    }

    // -----------------------------------------------------------------------------------------
    // Table assembly
    // -----------------------------------------------------------------------------------------

    #[test]
    fn the_frozen_table_populates_every_slot_this_family_implements() {
        let table = crate::driver::drv::driver_api_table();
        let populated = [
            table.erl_drv_busy_msgq_limits.is_some(),
            table.driver_failure_eof.is_some(),
            table.driver_failure_atom.is_some(),
            table.driver_failure_posix.is_some(),
            table.driver_failure.is_some(),
            table.driver_exit.is_some(),
            table.set_busy_port.is_some(),
            table.set_port_control_flags.is_some(),
            table.get_port_flags.is_some(),
            table.erl_drv_init_ack.is_some(),
            table.erl_drv_set_os_pid.is_some(),
            table.driver_async_port_key.is_some(),
        ];
        assert!(populated.iter().all(|&p| p), "{populated:?}");
    }
}
