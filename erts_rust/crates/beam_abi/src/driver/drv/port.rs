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

//! The **port/host-dependent** half of the driver host layer — part 2 of the `driver_*` /
//! `erl_drv_*` C ABI (workspace contract **A.4**).
//!
//! Every function here is a real `#[no_mangle] pub unsafe extern "C" fn` matching one field of the
//! frozen [`ErlDrvApi`](crate::driver::api::ErlDrvApi) table, and each one's `ErlDrvPort` operand
//! **is** the pointer [`DriverEnv::as_port`](crate::driver::host::DriverEnv::as_port) hands out
//! ([`DriverEnv::from_port`](crate::driver::host::DriverEnv::from_port) recovers it), so they all
//! delegate to the [`DriverHost`](crate::driver::host::DriverHost) seam: the `driver_output*`
//! family, the term-data family, the port queries and the select/timer/async group.
//!
//! The host-free half lives in [`super::mem`]; the shared [`env_for`](super::env_for) helper, the
//! `driver_term_nil` data symbol, the module docs and the populated table live in [`super`].
//!
//! The port/host-dependent half **is** the larger half of the C's surface, so its tests (the
//! port-pointer recovery, the term-data round-trips through the populated table, and the
//! output/select/timer host traffic) live in the sibling `port_tests.rs`, pulled in by the
//! `#[path]`-declared `mod tests` at the end of this file.

use libc::{c_char, c_int, c_long, c_uint, c_ulong, c_void};

use beam_term::prelude::Eterm;

use crate::driver::api::ErlDrvApi;
use crate::driver::host::decode_term_spec;
use crate::driver::types::{
    ErlDrvBinary, ErlDrvEvent, ErlDrvPort, ErlDrvSizeT, ErlDrvTermData, ErlIOVec,
};

use super::{driver_term_nil, env_for};

// ---------------------------------------------------------------------------------------------
// Port output (host)
// ---------------------------------------------------------------------------------------------

/// A borrowed view over the `len` bytes at `ptr` (`None` for a null `ptr` with a non-zero length).
///
/// # Safety
///
/// `ptr` must be null or point at `len` readable bytes.
unsafe fn read_bytes<'a>(ptr: *const u8, len: ErlDrvSizeT) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return if len == 0 { Some(&[]) } else { None };
    }
    // SAFETY: the caller guarantees `len` readable bytes at `ptr`.
    Some(unsafe { core::slice::from_raw_parts(ptr, len) })
}

/// Deliver `hlen` header bytes at `hbuf` followed by `body` to `port` (the `driver_output*` family).
///
/// Returns the C's `-1` when the port is null (its `ERTS_INVALID_ERL_DRV_PORT` path) or the header
/// is malformed, else the host's [`DriverHost::output`](crate::driver::host::DriverHost::output) code. The header and the body are joined
/// into one buffer because the seam hands the host a single byte slice.
///
/// # Safety
///
/// `hbuf` must be null or point at `hlen` readable bytes, and `port` must be null or a live port
/// handle.
unsafe fn output_pieces(
    port: ErlDrvPort,
    hbuf: *const c_char,
    hlen: ErlDrvSizeT,
    body: &[u8],
) -> c_int {
    // SAFETY: `hbuf`/`hlen` are the caller's header buffer (contract above).
    let Some(header) = (unsafe { read_bytes(hbuf as *const u8, hlen) }) else {
        return -1;
    };
    // SAFETY: `port` is null or a live handle (contract above).
    let Some(env) = (unsafe { env_for(port) }) else {
        return -1;
    };
    if header.is_empty() {
        return env.host().output(port, body);
    }
    let mut joined = Vec::with_capacity(header.len() + body.len());
    joined.extend_from_slice(header);
    joined.extend_from_slice(body);
    env.host().output(port, &joined)
}

/// `int driver_output(ErlDrvPort port, char *buf, ErlDrvSizeT len);` — send `len` bytes at `buf` to
/// the port owner (`io.c:6442`, which is `driver_output2` with a null header).
///
/// # Safety
///
/// `port` must be null or a live port handle; `buf` must be null or point at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn driver_output(
    port: ErlDrvPort,
    buf: *mut c_char,
    len: ErlDrvSizeT,
) -> c_int {
    // SAFETY: `buf`/`len` are the caller's output buffer (contract above).
    let Some(body) = (unsafe { read_bytes(buf as *const u8, len) }) else {
        return -1;
    };
    // SAFETY: `port` is null or a live handle, and there is no header (contract above).
    unsafe { output_pieces(port, core::ptr::null(), 0, body) }
}

/// `int driver_output2(ErlDrvPort port, char *hbuf, ErlDrvSizeT hlen, char *buf, ErlDrvSizeT len);`
/// — send a header followed by body data (`io.c:6387`; the header goes out as a list prefix).
///
/// # Safety
///
/// `port` must be null or a live port handle; `hbuf`/`buf` must each be null or point at `hlen`/
/// `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn driver_output2(
    port: ErlDrvPort,
    hbuf: *mut c_char,
    hlen: ErlDrvSizeT,
    buf: *mut c_char,
    len: ErlDrvSizeT,
) -> c_int {
    // SAFETY: `buf`/`len` are the caller's body buffer (contract above).
    let Some(body) = (unsafe { read_bytes(buf as *const u8, len) }) else {
        return -1;
    };
    // SAFETY: `hbuf`/`hlen` are the caller's header buffer and `port` its handle (contract above).
    unsafe { output_pieces(port, hbuf, hlen, body) }
}

/// The `len` bytes at byte `offs` inside a driver binary (`None` when malformed).
///
/// The C trusts `offs + len <= bin->orig_size` (`io.c:6339`); the **checked add** refuses an
/// out-of-range request instead of reading past the payload — a driver gets `-1` where the C would
/// read whatever memory follows, never a wrong result and never an over-read.
///
/// # Safety
///
/// `bin` must be null or point at a live [`ErlDrvBinary`] whose `orig_bytes` flexible array holds at
/// least `orig_size` bytes.
unsafe fn binary_payload_bytes<'a>(
    bin: *const ErlDrvBinary,
    offs: ErlDrvSizeT,
    len: ErlDrvSizeT,
) -> Option<&'a [u8]> {
    if bin.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a live `ErlDrvBinary` at `bin`.
    let orig_size = unsafe { (*bin).orig_size };
    if orig_size < 0 {
        return None;
    }
    if offs.checked_add(len)? > orig_size as ErlDrvSizeT {
        return None;
    }
    // SAFETY: `orig_size` payload bytes are readable at `orig_bytes` (caller contract) and
    // `offs + len <= orig_size` (checked above), so `offs` heads `len` readable bytes (one past the
    // last payload byte is still a valid base for a zero-length view).
    let base = unsafe { core::ptr::addr_of!((*bin).orig_bytes) } as *const u8;
    Some(unsafe { core::slice::from_raw_parts(base.add(offs), len) })
}

/// `int driver_output_binary(ErlDrvPort port, char *hbuf, ErlDrvSizeT hlen, ErlDrvBinary *bin,
/// ErlDrvSizeT offset, ErlDrvSizeT len);` — send a header plus a sub-range of a driver binary
/// (`io.c:6339`).
///
/// # Safety
///
/// `port` must be null or a live port handle; `hbuf` must be null or point at `hlen` readable bytes;
/// `bin` must be null or a live driver binary whose payload covers `offset + len`.
#[no_mangle]
pub unsafe extern "C" fn driver_output_binary(
    port: ErlDrvPort,
    hbuf: *mut c_char,
    hlen: ErlDrvSizeT,
    bin: *mut ErlDrvBinary,
    offs: ErlDrvSizeT,
    len: ErlDrvSizeT,
) -> c_int {
    // SAFETY: `bin`/`offs`/`len` are the caller's driver binary and its sub-range (contract above).
    let Some(body) = (unsafe { binary_payload_bytes(bin, offs, len) }) else {
        return -1;
    };
    // SAFETY: `hbuf`/`hlen` are the caller's header buffer and `port` its handle (contract above).
    unsafe { output_pieces(port, hbuf, hlen, body) }
}

/// The bytes of `ev`'s vectors with the first `skip` bytes dropped, gathered into one buffer.
///
/// The C walks the [`SysIOVec`](crate::common::SysIOVec) array and hands the remaining vectors
/// straight to the port (`io.c:6448-6504`); this seam takes one contiguous slice, so the bytes are
/// copied. `skip >= size` yields an empty body — the C's early return, which sends the header alone.
/// A `skip` past the vectors' declared lengths is malformed (`None`), never a partial body.
///
/// # Safety
///
/// `ev` must be null or point at a live [`ErlIOVec`] whose `iov` array holds `vsize` readable
/// `SysIOVec` entries, each heading `iov_len` readable bytes.
unsafe fn gather_iovec(ev: *const ErlIOVec, skip: ErlDrvSizeT) -> Option<Vec<u8>> {
    if ev.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a live `ErlIOVec` at `ev`.
    let (vsize, size, iov) = unsafe { ((*ev).vsize, (*ev).size, (*ev).iov) };
    if vsize < 0 {
        return None;
    }
    if size <= skip {
        return Some(Vec::new()); // The C's `vec->size <= skip`: header only.
    }
    if iov.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `vsize` readable `SysIOVec` entries at `iov`.
    let entries = unsafe { core::slice::from_raw_parts(iov, vsize as usize) };
    let mut out = Vec::new();
    let mut left = skip;
    for entry in entries {
        let len = entry.iov_len;
        if left >= len {
            left -= len;
            continue;
        }
        // SAFETY: `iov_base` heads `iov_len` readable bytes (caller contract); the view starts
        // `left` bytes in and runs to the entry's end.
        let bytes = unsafe {
            core::slice::from_raw_parts((entry.iov_base as *const u8).add(left), len - left)
        };
        out.extend_from_slice(bytes);
        left = 0;
    }
    if left != 0 {
        return None; // `skip` ran past the vectors' declared lengths: malformed.
    }
    Some(out)
}

/// `int driver_outputv(ErlDrvPort port, char *hbuf, ErlDrvSizeT hlen, ErlIOVec *ev,
/// ErlDrvSizeT skip);` — send a header plus a scatter/gather vector with the first `skip` bytes
/// dropped (`io.c:6448`).
///
/// # Safety
///
/// `port` must be null or a live port handle; `hbuf` must be null or point at `hlen` readable bytes;
/// `ev` must be null or a live vector as required by [`gather_iovec`].
#[no_mangle]
pub unsafe extern "C" fn driver_outputv(
    port: ErlDrvPort,
    hbuf: *mut c_char,
    hlen: ErlDrvSizeT,
    ev: *mut ErlIOVec,
    skip: ErlDrvSizeT,
) -> c_int {
    // SAFETY: `ev`/`skip` are the caller's vector and its skip count (contract above).
    let Some(body) = (unsafe { gather_iovec(ev, skip) }) else {
        return -1;
    };
    // SAFETY: `hbuf`/`hlen` are the caller's header buffer and `port` its handle (contract above).
    unsafe { output_pieces(port, hbuf, hlen, &body) }
}

// ---------------------------------------------------------------------------------------------
// Term data (host)
// ---------------------------------------------------------------------------------------------

/// The C-facing port pointer encoded in an `ErlDrvTermData` (the `erl_drv_*_term` port operand).
///
/// [`driver_mk_port`] produces it; the C passes the port's *id term* instead (documented deviation,
/// see the module header).
#[inline]
fn port_from_term_data(port: ErlDrvTermData) -> ErlDrvPort {
    port as usize as ErlDrvPort
}

/// Decode a term-data array and hand the term to the host (all four `*_output_term`/`*_send_term`
/// forms).
///
/// Returns `-1` for a null port, a negative `len`, a null array with a non-zero `len`, or a
/// malformed spec (never a wrong term: [`decode_term_spec`] is total), else the host's
/// [`DriverHost::send_term`](crate::driver::host::DriverHost::send_term) code. `to` is `None` for the `_output_term` forms (the port owner) and
/// the destination term for the `_send_term` forms.
///
/// # Safety
///
/// `port` must be null or a live port handle; `data` must be null or point at `len` readable
/// `ErlDrvTermData` words.
unsafe fn deliver_term(
    port: ErlDrvPort,
    to: Option<Eterm>,
    data: *mut ErlDrvTermData,
    len: c_int,
) -> c_int {
    // SAFETY: `port` is null or a live handle (contract above).
    let Some(env) = (unsafe { env_for(port) }) else {
        return -1;
    };
    if len < 0 || (data.is_null() && len != 0) {
        return -1;
    }
    let spec: &[ErlDrvTermData] = if data.is_null() {
        &[] // `len == 0` (checked above): `from_raw_parts(null, 0)` would be UB.
    } else {
        // SAFETY: the caller guarantees `len` readable words at `data`.
        unsafe { core::slice::from_raw_parts(data, len as usize) }
    };
    match decode_term_spec(env, spec) {
        Some(term) => env.host().send_term(port, to, term),
        None => -1,
    }
}

/// `int driver_output_term(ErlDrvPort ix, ErlDrvTermData *data, int len);` — `ERL_DRV_DEPRECATED_FUNC`;
/// output a decoded term to the port owner (`io.c:6271`).
///
/// # Safety
///
/// `ix` must be null or a live port handle; `data` must be null or point at `len` readable
/// `ErlDrvTermData` words.
#[no_mangle]
pub unsafe extern "C" fn driver_output_term(
    ix: ErlDrvPort,
    data: *mut ErlDrvTermData,
    len: c_int,
) -> c_int {
    // SAFETY: `ix` is null or a live handle and `data`/`len` are the caller's spec (contract above).
    unsafe { deliver_term(ix, None, data, len) }
}

/// `int driver_send_term(ErlDrvPort ix, ErlDrvTermData to, ErlDrvTermData *data, int len);` —
/// `ERL_DRV_DEPRECATED_FUNC`; send a decoded term to `to` (`io.c:6306`).
///
/// `to` is a **raw term word** (the C's `Eterm to`, e.g. a pid from [`driver_connected`]); it is
/// passed to [`DriverHost::send_term`](crate::driver::host::DriverHost::send_term) as the destination, exactly as the C passes its `Eterm`.
///
/// # Safety
///
/// `ix` must be null or a live port handle; `data` must be null or point at `len` readable
/// `ErlDrvTermData` words; `to` must be a valid term word for a destination process.
#[no_mangle]
pub unsafe extern "C" fn driver_send_term(
    ix: ErlDrvPort,
    to: ErlDrvTermData,
    data: *mut ErlDrvTermData,
    len: c_int,
) -> c_int {
    // SAFETY: `ix` is null or a live handle and `data`/`len` are the caller's spec (contract above).
    unsafe { deliver_term(ix, Some(Eterm::from_raw(to as usize)), data, len) }
}

/// `int erl_drv_output_term(ErlDrvTermData port, ErlDrvTermData *data, int len);` — output a decoded
/// term to the port owner, the non-deprecated form (`io.c:6254`).
///
/// The `port` operand carries the [`driver_mk_port`] encoding of the port handle (a pointer), which
/// [`port_from_term_data`] casts back — see the module header for this deviation from the C's port
/// id term.
///
/// # Safety
///
/// `port` must be `0` or the [`driver_mk_port`] encoding of a live port handle; `data` must be null
/// or point at `len` readable `ErlDrvTermData` words.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_output_term(
    port: ErlDrvTermData,
    data: *mut ErlDrvTermData,
    len: c_int,
) -> c_int {
    // SAFETY: `port` encodes a null or live handle and `data`/`len` are the caller's spec.
    unsafe { deliver_term(port_from_term_data(port), None, data, len) }
}

/// `int erl_drv_send_term(ErlDrvTermData port, ErlDrvTermData to, ErlDrvTermData *data, int len);`
/// — send a decoded term to `to`, the non-deprecated form (`io.c:6289`).
///
/// `port` is the [`driver_mk_port`] pointer encoding (see [`erl_drv_output_term`]); `to` is a raw
/// term word (see [`driver_send_term`]).
///
/// # Safety
///
/// As [`erl_drv_output_term`], plus `to` must be a valid term word for a destination process.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_send_term(
    port: ErlDrvTermData,
    to: ErlDrvTermData,
    data: *mut ErlDrvTermData,
    len: c_int,
) -> c_int {
    // SAFETY: `port` encodes a null or live handle and `data`/`len` are the caller's spec.
    unsafe {
        deliver_term(
            port_from_term_data(port),
            Some(Eterm::from_raw(to as usize)),
            data,
            len,
        )
    }
}

/// `ErlDrvTermData driver_mk_atom(char *string);` — the term data for the atom named `string`
/// (`io.c:7285`).
///
/// **Documented deviation:** the C interns `string` and returns the atom *term*. This function takes
/// no port, so it has no host and cannot intern; it returns the **`string` pointer itself** as the
/// term data. That is exactly what [`decode_term_spec`]'s `ERL_DRV_ATOM` operand expects (a
/// NUL-terminated `char*` interned at decode time), so the pair round-trips to the same atom the C
/// would produce.
///
/// # Safety
///
/// `string` must be null or NUL-terminated (the resulting term data is only useful as an
/// `ERL_DRV_ATOM` operand, which requires exactly that).
#[no_mangle]
pub unsafe extern "C" fn driver_mk_atom(string: *mut c_char) -> ErlDrvTermData {
    string as usize as ErlDrvTermData
}

/// `ErlDrvTermData driver_mk_port(ErlDrvPort port);` — the term data for a port (`io.c:7295`).
///
/// **Documented deviation:** the C returns the port's id term (`prt->common.id`), and `NIL` for an
/// invalid handle. This seam has no port term, so the handle is returned **pointer-encoded** — which
/// is what [`erl_drv_output_term`]/[`erl_drv_send_term`] decode back through
/// [`DriverEnv::from_port`](crate::driver::host::DriverEnv::from_port) and what
/// [`driver_async_port_key`](crate::driver::api::ErlDrvApi::driver_async_port_key) would key on. A
/// null handle yields [`driver_term_nil`], as the C does for an invalid one.
///
/// # Safety
///
/// `port` must be null or a live port handle (it is only compared, never dereferenced).
#[no_mangle]
pub unsafe extern "C" fn driver_mk_port(port: ErlDrvPort) -> ErlDrvTermData {
    if port.is_null() {
        driver_term_nil
    } else {
        port as usize as ErlDrvTermData
    }
}

/// `ErlDrvTermData driver_connected(ErlDrvPort port);` — the term data of the port's connected
/// process (`io.c:7304`), or [`driver_term_nil`] when there is none (a null handle included, as in
/// the C).
///
/// # Safety
///
/// `port` must be null or a live port handle.
#[no_mangle]
pub unsafe extern "C" fn driver_connected(port: ErlDrvPort) -> ErlDrvTermData {
    // SAFETY: `port` is null or a live handle (contract above).
    let Some(env) = (unsafe { env_for(port) }) else {
        return driver_term_nil;
    };
    match env.host().connected_pid(port) {
        Some(pid) => pid.raw() as ErlDrvTermData,
        None => driver_term_nil,
    }
}

/// `ErlDrvTermData driver_caller(ErlDrvPort port);` — the term data of the process that opened the
/// port (`io.c:7314`), or [`driver_term_nil`] when there is none (a null handle included).
///
/// # Safety
///
/// `port` must be null or a live port handle.
#[no_mangle]
pub unsafe extern "C" fn driver_caller(port: ErlDrvPort) -> ErlDrvTermData {
    // SAFETY: `port` is null or a live handle (contract above).
    let Some(env) = (unsafe { env_for(port) }) else {
        return driver_term_nil;
    };
    match env.host().caller_pid(port) {
        Some(pid) => pid.raw() as ErlDrvTermData,
        None => driver_term_nil,
    }
}

// ---------------------------------------------------------------------------------------------
// Select, timers and async (host)
// ---------------------------------------------------------------------------------------------

/// `int driver_set_timer(ErlDrvPort port, unsigned long time);` — schedule the port's `timeout`
/// callback `time` milliseconds from now (`io.c:6909`).
///
/// Returns [`DriverHost::set_timer`](crate::driver::host::DriverHost::set_timer)'s code: `0` on success, `-1` for a null port or a host without
/// timers — the same `-1` the C returns when the driver has no `timeout` callback.
///
/// # Safety
///
/// `port` must be null or a live port handle.
#[no_mangle]
pub unsafe extern "C" fn driver_set_timer(port: ErlDrvPort, time: c_ulong) -> c_int {
    // SAFETY: `port` is null or a live handle (contract above).
    match unsafe { env_for(port) } {
        Some(env) => env.host().set_timer(port, time),
        None => -1,
    }
}

/// `int driver_cancel_timer(ErlDrvPort port);` — cancel the port's pending timer (`io.c:6925`).
/// Returns [`DriverHost::cancel_timer`](crate::driver::host::DriverHost::cancel_timer)'s code
/// (`0`/`-1`).
///
/// # Safety
///
/// `port` must be null or a live port handle.
#[no_mangle]
pub unsafe extern "C" fn driver_cancel_timer(port: ErlDrvPort) -> c_int {
    // SAFETY: `port` is null or a live handle (contract above).
    match unsafe { env_for(port) } {
        Some(env) => env.host().cancel_timer(port),
        None => -1,
    }
}

/// `int driver_read_timer(ErlDrvPort port, unsigned long *time_left);` — write the port timer's
/// remaining milliseconds to `*time_left` (`io.c:6936`). Returns `0` on success and `-1` for a null
/// port, a host without timers, or a null `time_left` (which the C would dereference).
///
/// # Safety
///
/// `port` must be null or a live port handle, and `time_left` must be null or point at a writable
/// `unsigned long`.
#[no_mangle]
pub unsafe extern "C" fn driver_read_timer(port: ErlDrvPort, time_left: *mut c_ulong) -> c_int {
    // SAFETY: `port` is null or a live handle (contract above).
    let Some(env) = (unsafe { env_for(port) }) else {
        return -1;
    };
    if time_left.is_null() {
        return -1;
    }
    match env.host().read_timer(port) {
        Some(ms) => {
            // SAFETY: `time_left` is non-null (checked above) and the caller guarantees it is
            // writable.
            unsafe { *time_left = ms };
            0
        }
        // No timer pending, or the host does not support timers: the C's failure code.
        None => -1,
    }
}

/// `int driver_select(ErlDrvPort port, ErlDrvEvent event, int mode, int on);` — register/deregister
/// interest in `event` (`erl_check_io.c:987`). Returns [`DriverHost::select`](crate::driver::host::DriverHost::select)'s code (`0`/`-1`).
///
/// # Safety
///
/// `port` must be null or a live port handle; `event` must be whatever the host's
/// [`DriverHost::select`](crate::driver::host::DriverHost::select) expects for that port (an OS handle the driver owns).
#[no_mangle]
pub unsafe extern "C" fn driver_select(
    port: ErlDrvPort,
    event: ErlDrvEvent,
    mode: c_int,
    on: c_int,
) -> c_int {
    // SAFETY: `port` is null or a live handle (contract above).
    match unsafe { env_for(port) } {
        Some(env) => env.host().select(port, event, mode, on),
        None => -1,
    }
}

/// `long driver_async(ErlDrvPort ix, unsigned int *key, void (*async_invoke)(void *), void
/// *async_data, void (*async_free)(void *));` — schedule `invoke` on an async thread
/// (`erl_async.c:541`).
///
/// Returns [`DriverHost::r#async`](crate::driver::host::DriverHost::r#async)'s code: the C returns `0` when the job completed synchronously,
/// a positive handle for a queued job, and `-1` on error (a null port, or a host without an async
/// pool — `driver_async_port_key`'s companion is `None` here too).
///
/// # Safety
///
/// `port` must be null or a live port handle; `key`, `data` and the two callbacks must be whatever
/// the host's [`DriverHost::r#async`](crate::driver::host::DriverHost::r#async) expects (they are passed through untouched).
#[no_mangle]
pub unsafe extern "C" fn driver_async(
    ix: ErlDrvPort,
    key: *mut c_uint,
    invoke: Option<unsafe extern "C" fn(*mut c_void)>,
    data: *mut c_void,
    free: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_long {
    // SAFETY: `ix` is null or a live handle (contract above).
    match unsafe { env_for(ix) } {
        Some(env) => env.host().r#async(ix, key, invoke, data, free),
        None => -1,
    }
}

// ---------------------------------------------------------------------------------------------
// Table assembly (part 2 of the frozen `ErlDrvApi`)
// ---------------------------------------------------------------------------------------------

/// Populate the **port/host-dependent** slots of the frozen `ErlDrvApi` this file implements
/// (part 2).
///
/// Called from [`super::DRIVER_API`]'s `populate` chain, so this file — not `mod.rs` — owns the
/// `Some(…)` list for the functions it defines. Every field is `const`-assignable, so the whole
/// chain folds at compile time.
///
/// Implements: the `driver_output*` family, the term-data family, the port queries
/// (`driver_mk_atom`/`driver_mk_port`/`driver_connected`/`driver_caller`) and the
/// select/timer/async group.
pub(super) const fn populate(mut api: ErlDrvApi) -> ErlDrvApi {
    api.driver_output = Some(driver_output);
    api.driver_output2 = Some(driver_output2);
    api.driver_output_binary = Some(driver_output_binary);
    api.driver_outputv = Some(driver_outputv);
    api.driver_output_term = Some(driver_output_term);
    api.driver_send_term = Some(driver_send_term);
    api.erl_drv_output_term = Some(erl_drv_output_term);
    api.erl_drv_send_term = Some(erl_drv_send_term);
    api.driver_mk_atom = Some(driver_mk_atom);
    api.driver_mk_port = Some(driver_mk_port);
    api.driver_connected = Some(driver_connected);
    api.driver_caller = Some(driver_caller);
    api.driver_select = Some(driver_select);
    api.driver_set_timer = Some(driver_set_timer);
    api.driver_cancel_timer = Some(driver_cancel_timer);
    api.driver_read_timer = Some(driver_read_timer);
    api.driver_async = Some(driver_async);
    api
}

#[cfg(test)]
#[path = "port_tests.rs"]
mod tests;
