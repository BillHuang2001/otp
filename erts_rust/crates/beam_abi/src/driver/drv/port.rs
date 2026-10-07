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

use libc::{c_char, c_int, c_long, c_uint, c_ulong, c_void};

use beam_term::prelude::Eterm;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    use beam_alloc::Heap;
    use beam_term::atom::AtomTable;
    use beam_term::prelude::{
        atom_val, is_atom, is_tuple, make_atom, make_small, tuple_element, tuple_size,
    };

    use crate::driver::host::{DriverEnv, DriverHost};
    use crate::driver::types::{term_data, ERL_DRV_ATOM, ERL_DRV_INT, ERL_DRV_READ, ERL_DRV_TUPLE};

    use super::super::mem::{driver_alloc_binary, driver_free_binary};
    use super::super::{binary_payload, driver_api_table};

    /// A recording [`DriverHost`] over a bump heap and an atom table.
    ///
    /// Every host call is captured, so a test can assert exactly what reached the port (`output`,
    /// `sent`, `selected`, `timers_*`, `async_calls`); the optional methods return recognizable
    /// values instead of the trait's `unsupported` defaults.
    struct TestDriverHost {
        heap: Heap,
        atoms: AtomTable,
        output: Vec<u8>,
        sent: Vec<(Option<Eterm>, Eterm)>,
        selected: Vec<(usize, c_int, c_int)>,
        timers_set: Vec<c_ulong>,
        timers_cancelled: usize,
        timer_left: Option<c_ulong>,
        async_calls: Vec<(bool, usize)>,
        connected: Option<Eterm>,
        caller: Option<Eterm>,
    }

    impl TestDriverHost {
        fn new() -> TestDriverHost {
            TestDriverHost {
                heap: Heap::new(1024).expect("a small test heap is allocatable"),
                atoms: AtomTable::new(),
                output: Vec::new(),
                sent: Vec::new(),
                selected: Vec::new(),
                timers_set: Vec::new(),
                timers_cancelled: 0,
                timer_left: None,
                async_calls: Vec::new(),
                connected: None,
                caller: None,
            }
        }

        /// The name behind an atom term, through this host's own atom table.
        fn atom_name(&self, term: Eterm) -> Option<&str> {
            if !is_atom(term) {
                return None;
            }
            self.atoms.name(atom_val(term))
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

        fn send_term(&mut self, _port: ErlDrvPort, to: Option<Eterm>, term: Eterm) -> c_int {
            self.sent.push((to, term));
            0
        }

        fn select(
            &mut self,
            _port: ErlDrvPort,
            event: ErlDrvEvent,
            mode: c_int,
            on: c_int,
        ) -> c_int {
            self.selected.push((event as usize, mode, on));
            0
        }

        fn set_timer(&mut self, _port: ErlDrvPort, ms: c_ulong) -> c_int {
            self.timers_set.push(ms);
            0
        }

        fn cancel_timer(&mut self, _port: ErlDrvPort) -> c_int {
            self.timers_cancelled += 1;
            0
        }

        fn read_timer(&mut self, _port: ErlDrvPort) -> Option<c_ulong> {
            self.timer_left
        }

        fn r#async(
            &mut self,
            _port: ErlDrvPort,
            _key: *mut c_uint,
            invoke: Option<unsafe extern "C" fn(*mut c_void)>,
            data: *mut c_void,
            _free: Option<unsafe extern "C" fn(*mut c_void)>,
        ) -> c_long {
            self.async_calls.push((invoke.is_some(), data as usize));
            5 // A recognizable "queued" handle.
        }

        fn connected_pid(&mut self, _port: ErlDrvPort) -> Option<Eterm> {
            self.connected
        }

        fn caller_pid(&mut self, _port: ErlDrvPort) -> Option<Eterm> {
            self.caller
        }
    }

    /// The `{atom, value}` shape the term-data tests send: the atom's name and the second element,
    /// or `None` when `term` is not a 2-tuple.
    ///
    /// A comparison helper rather than a raw-word one: every `*_output_term` call decodes the spec
    /// afresh, so equal-content tuples live in *different* heap boxes.
    fn atom_int_pair(host: &TestDriverHost, term: Eterm) -> Option<(Option<&str>, Eterm)> {
        // SAFETY: `term` is a live term word.
        if !unsafe { is_tuple(term) } {
            return None;
        }
        // SAFETY: `term` is a tuple (checked above).
        if unsafe { tuple_size(term) } != 2 {
            return None;
        }
        // SAFETY: both indices are inside a 2-tuple (checked above).
        let (first, second) = unsafe { (tuple_element(term, 0), tuple_element(term, 1)) };
        Some((host.atom_name(first), second))
    }

    /// A host that overrides **nothing** but the required methods: every optional method keeps its
    /// `unsupported` default, so the C failure code must come back.
    struct MinimalDriverHost {
        heap: Heap,
    }

    impl MinimalDriverHost {
        fn new() -> MinimalDriverHost {
            MinimalDriverHost {
                heap: Heap::new(1024).expect("a small test heap is allocatable"),
            }
        }
    }

    impl DriverHost for MinimalDriverHost {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }

        fn intern_atom(&mut self, _name: &str) -> Option<Eterm> {
            None
        }

        fn output(&mut self, _port: ErlDrvPort, _data: &[u8]) -> c_int {
            -1
        }

        fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, _term: Eterm) -> c_int {
            -1
        }
    }

    #[test]
    fn driver_output_reaches_the_host_with_the_exact_bytes() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        assert!(!port.is_null(), "the port pointer is never null");

        let mut body = *b"hello";
        // SAFETY: `port` is a live env handle and `body` declares its own length.
        let rc = unsafe { driver_output(port, body.as_mut_ptr() as *mut c_char, body.len()) };
        assert_eq!(rc, 0);
        assert_eq!(host.output, b"hello");

        // `driver_output2` puts the header in front of the body.
        host.output.clear();
        let mut head = *b"H";
        // SAFETY: `port` is a live handle; both buffers declare their lengths.
        let rc = unsafe {
            driver_output2(
                port,
                head.as_mut_ptr() as *mut c_char,
                head.len(),
                body.as_mut_ptr() as *mut c_char,
                body.len(),
            )
        };
        assert_eq!(rc, 0);
        assert_eq!(host.output, b"Hhello");

        // An empty body still delivers the header.
        host.output.clear();
        // SAFETY: `port` is live; a zero length with a valid pointer is well formed.
        let rc = unsafe {
            driver_output2(
                port,
                head.as_mut_ptr() as *mut c_char,
                head.len(),
                body.as_mut_ptr() as *mut c_char,
                0,
            )
        };
        assert_eq!(rc, 0);
        assert_eq!(host.output, b"H");
    }

    #[test]
    fn driver_output_binary_slices_the_payload_and_refuses_an_over_read() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        // SAFETY: the driver-binary allocator is a malloc wrapper; `bin` is freed below.
        let bin = unsafe { driver_alloc_binary(8) };
        binary_payload(bin, 8).copy_from_slice(b"abcdefgh");
        let mut head = *b"H";

        // SAFETY: `bin` is live and `2 + 3 <= orig_size`.
        let rc =
            unsafe { driver_output_binary(port, head.as_mut_ptr() as *mut c_char, 1, bin, 2, 3) };
        assert_eq!(rc, 0);
        assert_eq!(host.output, b"Hcde");

        // A sub-range past the payload is refused (the C would read past it).
        host.output.clear();
        // SAFETY: `bin` is live; the request deliberately runs past its payload.
        let rc =
            unsafe { driver_output_binary(port, head.as_mut_ptr() as *mut c_char, 1, bin, 6, 3) };
        assert_eq!(rc, -1);
        assert!(host.output.is_empty());

        // The empty sub-range at the very end is still well formed.
        // SAFETY: `bin` is live and `8 + 0 <= orig_size`.
        let rc =
            unsafe { driver_output_binary(port, head.as_mut_ptr() as *mut c_char, 1, bin, 8, 0) };
        assert_eq!(rc, 0);
        assert_eq!(host.output, b"H");

        // A null binary is refused.
        // SAFETY: the null binary is checked before any read.
        let rc = unsafe {
            driver_output_binary(
                port,
                head.as_mut_ptr() as *mut c_char,
                1,
                core::ptr::null_mut(),
                0,
                0,
            )
        };
        assert_eq!(rc, -1);

        // SAFETY: `bin` came from `driver_alloc_binary` and is freed exactly once.
        unsafe { driver_free_binary(bin) };
    }

    #[test]
    fn driver_outputv_gathers_the_vectors_and_drops_skip_bytes() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        let mut first = *b"abcd";
        let mut second = *b"efgh";
        let mut iov = [
            libc::iovec {
                iov_base: first.as_mut_ptr() as *mut c_void,
                iov_len: first.len(),
            },
            libc::iovec {
                iov_base: second.as_mut_ptr() as *mut c_void,
                iov_len: second.len(),
            },
        ];
        let mut ev = ErlIOVec {
            vsize: iov.len() as c_int,
            size: 8,
            iov: iov.as_mut_ptr(),
            binv: core::ptr::null_mut(),
        };
        let mut head = *b"H";

        // SAFETY: `ev` describes the two live buffers and `skip` is inside `size`.
        let rc = unsafe { driver_outputv(port, head.as_mut_ptr() as *mut c_char, 1, &mut ev, 5) };
        assert_eq!(rc, 0);
        assert_eq!(host.output, b"Hfgh", "skip drops the first 5 bytes");

        // `skip == size`: the header alone (the C's early return).
        host.output.clear();
        // SAFETY: `ev` is live and `skip == size`.
        let rc = unsafe { driver_outputv(port, head.as_mut_ptr() as *mut c_char, 1, &mut ev, 8) };
        assert_eq!(rc, 0);
        assert_eq!(host.output, b"H");

        // A `skip` past the vectors' declared lengths is malformed.
        let mut short_iov = [libc::iovec {
            iov_base: first.as_mut_ptr() as *mut c_void,
            iov_len: first.len(),
        }];
        let mut short_ev = ErlIOVec {
            vsize: 1,
            size: 8,
            iov: short_iov.as_mut_ptr(),
            binv: core::ptr::null_mut(),
        };
        host.output.clear();
        // SAFETY: `short_ev` is live; its single entry is shorter than the declared `size`.
        let rc =
            unsafe { driver_outputv(port, head.as_mut_ptr() as *mut c_char, 1, &mut short_ev, 5) };
        assert_eq!(rc, -1);
        assert!(host.output.is_empty());

        // A null vector, and a null port, are refused.
        // SAFETY: the null pointers are checked before any read.
        unsafe {
            assert_eq!(
                driver_outputv(port, core::ptr::null_mut(), 0, core::ptr::null_mut(), 0),
                -1
            );
            assert_eq!(
                driver_outputv(core::ptr::null_mut(), core::ptr::null_mut(), 0, &mut ev, 0),
                -1
            );
        }
    }

    #[test]
    fn erl_drv_output_term_decodes_and_forwards_a_tuple() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        let name = CString::new("a").expect("no interior NUL");

        // `{a, 7}`
        let spec = [
            ERL_DRV_ATOM,
            term_data(name.as_ptr() as usize),
            ERL_DRV_INT,
            7,
            ERL_DRV_TUPLE,
            2,
        ];
        // SAFETY: `port` is a live env handle (only compared and pointer-encoded).
        let port_id = unsafe { driver_mk_port(port) };
        assert_ne!(
            port_id, driver_term_nil,
            "a live port has a term-data handle"
        );

        // SAFETY: `port_id` encodes the live `port` and `spec` is a live six-word array.
        let rc = unsafe { erl_drv_output_term(port_id, spec.as_ptr() as *mut ErlDrvTermData, 6) };
        assert_eq!(rc, 0);
        assert_eq!(host.sent.len(), 1);
        let (to, term) = host.sent[0];
        assert_eq!(to, None, "the `_output_term` forms target the port owner");
        // SAFETY: `term` is a live 2-tuple (asserted below).
        unsafe {
            assert!(is_tuple(term));
            assert_eq!(tuple_size(term), 2);
            assert_eq!(tuple_element(term, 1), make_small(7));
        }
        // SAFETY: element 0 of a 2-tuple.
        let first = unsafe { tuple_element(term, 0) };
        assert_eq!(host.atom_name(first), Some("a"));

        // The deprecated `driver_output_term` form behaves identically. Each call decodes the spec
        // afresh, so the tuple is a *new* heap box: the comparison is structural, not raw-word.
        host.sent.clear();
        // SAFETY: `port` is live and `spec` is the same live array.
        let rc = unsafe { driver_output_term(port, spec.as_ptr() as *mut ErlDrvTermData, 6) };
        assert_eq!(rc, 0);
        assert_eq!(host.sent.len(), 1);
        assert_eq!(host.sent[0].0, None);
        assert_eq!(
            atom_int_pair(&host, host.sent[0].1),
            atom_int_pair(&host, term)
        );

        // The `_send_term` forms pass the destination term through.
        host.sent.clear();
        let dest = make_small(9);
        // SAFETY: `port_id` encodes the live `port`; `dest` is a term word (the C's `Eterm to`).
        let rc = unsafe {
            erl_drv_send_term(
                port_id,
                dest.raw() as ErlDrvTermData,
                spec.as_ptr() as *mut ErlDrvTermData,
                6,
            )
        };
        assert_eq!(rc, 0);
        assert_eq!(host.sent.len(), 1);
        assert_eq!(host.sent[0].0, Some(dest));
        assert_eq!(
            atom_int_pair(&host, host.sent[0].1),
            Some((Some("a"), make_small(7)))
        );
        host.sent.clear();
        // SAFETY: as above, through the deprecated `ErlDrvPort` form.
        let rc = unsafe {
            driver_send_term(
                port,
                dest.raw() as ErlDrvTermData,
                spec.as_ptr() as *mut ErlDrvTermData,
                6,
            )
        };
        assert_eq!(rc, 0);
        assert_eq!(host.sent.len(), 1);
        assert_eq!(host.sent[0].0, Some(dest));
        assert_eq!(
            atom_int_pair(&host, host.sent[0].1),
            Some((Some("a"), make_small(7)))
        );

        // A malformed spec never reaches the host.
        host.sent.clear();
        // Two values left on the stack.
        let bad = [ERL_DRV_INT, 1, ERL_DRV_INT, 2];
        // SAFETY: `bad` is a live four-word array.
        let rc = unsafe { erl_drv_output_term(port_id, bad.as_ptr() as *mut ErlDrvTermData, 4) };
        assert_eq!(rc, -1);
        assert!(host.sent.is_empty());

        // A non-empty spec behind a null array is refused, not dereferenced.
        // SAFETY: the null array is checked before any read.
        let rc = unsafe { erl_drv_output_term(port_id, core::ptr::null_mut(), 2) };
        assert_eq!(rc, -1);
        // A host without an atom table cannot intern, so the spec cannot be built.
        let mut bare = MinimalDriverHost::new();
        let mut bare_env = DriverEnv::new(&mut bare);
        let bare_port = bare_env.as_port();
        // SAFETY: `bare_env` is live and `spec` is a live six-word array.
        let rc = unsafe {
            erl_drv_output_term(
                driver_mk_port(bare_port),
                spec.as_ptr() as *mut ErlDrvTermData,
                6,
            )
        };
        assert_eq!(rc, -1, "an uninternable atom fails the spec");
    }

    #[test]
    fn driver_connected_and_caller_return_term_words_or_nil() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        // A host with no known pid reports `nil`: the C's "no process" answer.
        // SAFETY: `port` is live; neither call takes another pointer.
        unsafe {
            assert_eq!(driver_connected(port), driver_term_nil);
            assert_eq!(driver_caller(port), driver_term_nil);
        }

        // The host's pids come back as raw term words.
        let connected = make_small(11);
        let caller = make_small(12);
        host.connected = Some(connected);
        host.caller = Some(caller);
        // SAFETY: as above.
        unsafe {
            assert_eq!(driver_connected(port), connected.raw() as ErlDrvTermData);
            assert_eq!(driver_caller(port), caller.raw() as ErlDrvTermData);
        }
    }

    #[test]
    fn driver_mk_atom_returns_a_pointer_the_atom_spec_interns() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let name = CString::new("ok").expect("no interior NUL");

        // SAFETY: `name` is a NUL-terminated C string.
        let td = unsafe { driver_mk_atom(name.as_ptr() as *mut c_char) };
        assert_eq!(td, name.as_ptr() as usize as ErlDrvTermData);
        // That is exactly the `ERL_DRV_ATOM` operand `decode_term_spec` interns.
        let term = decode_term_spec(&mut env, &[ERL_DRV_ATOM, td]).expect("a valid atom spec");
        assert_eq!(host.atom_name(term), Some("ok"));
    }

    #[test]
    fn driver_mk_port_encodes_the_handle_and_nil_stays_nil() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        // SAFETY: `port` is a live handle; a null handle is only compared.
        unsafe {
            assert_eq!(driver_mk_port(port), port as usize as ErlDrvTermData);
            assert_eq!(driver_mk_port(core::ptr::null_mut()), driver_term_nil);
        }
    }

    #[test]
    fn timers_select_and_async_delegate_to_the_host() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        // SAFETY: `port` is live; no other pointer is dereferenced by these two calls.
        unsafe {
            assert_eq!(driver_set_timer(port, 250), 0);
            assert_eq!(host.timers_set, vec![250]);
            assert_eq!(driver_cancel_timer(port), 0);
            assert_eq!(host.timers_cancelled, 1);
        }

        let mut left: c_ulong = 0;
        // SAFETY: `port` is live and `left` is a writable `unsigned long`.
        unsafe {
            // No timer pending: the host's `None` becomes the C's failure code.
            assert_eq!(driver_read_timer(port, &mut left), -1);
            assert_eq!(left, 0, "a failed read leaves the out-parameter alone");
        }
        host.timer_left = Some(123);
        // SAFETY: as above.
        unsafe {
            assert_eq!(driver_read_timer(port, &mut left), 0);
            assert_eq!(left, 123);
            // A null out-parameter is refused instead of dereferenced.
            assert_eq!(driver_read_timer(port, core::ptr::null_mut()), -1);
        }

        // The event is an opaque handle the host only records.
        let event: ErlDrvEvent = 8usize as *mut c_void as ErlDrvEvent;
        // SAFETY: `port` is live and `event` is only compared/stored by the host.
        unsafe {
            assert_eq!(driver_select(port, event, ERL_DRV_READ, 1), 0);
            assert_eq!(driver_select(port, event, ERL_DRV_READ, 0), 0);
        }
        assert_eq!(
            host.selected,
            vec![(8, ERL_DRV_READ, 1), (8, ERL_DRV_READ, 0)]
        );

        let mut key: c_uint = 0;
        // SAFETY: `port` is live; the job arguments are passed straight through to the host.
        let rc = unsafe { driver_async(port, &mut key, None, 0x1234usize as *mut c_void, None) };
        assert_eq!(rc, 5, "the host's queue handle comes back verbatim");
        assert_eq!(host.async_calls, vec![(false, 0x1234)]);
        assert_eq!(key, 0, "the key operand is the host's, not ours");
    }

    #[test]
    fn defaulted_host_methods_report_the_c_failure_code() {
        let mut host = MinimalDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        let mut left: c_ulong = 0;
        // SAFETY: `port` is live; every other operand is null or a local out-parameter.
        unsafe {
            assert_eq!(
                driver_select(port, core::ptr::null_mut(), ERL_DRV_READ, 1),
                -1
            );
            assert_eq!(driver_set_timer(port, 10), -1);
            assert_eq!(driver_cancel_timer(port), -1);
            assert_eq!(driver_read_timer(port, &mut left), -1);
            assert_eq!(left, 0);
            assert_eq!(
                driver_async(
                    port,
                    core::ptr::null_mut(),
                    None,
                    core::ptr::null_mut(),
                    None
                ),
                -1
            );
            assert_eq!(driver_connected(port), driver_term_nil);
            assert_eq!(driver_caller(port), driver_term_nil);
            assert_eq!(driver_output(port, core::ptr::null_mut(), 0), -1);
        }
    }

    #[test]
    fn null_ports_return_the_failure_code_without_panicking() {
        // SAFETY: every pointer here is null or empty, which the functions check before use.
        unsafe {
            let null_port: ErlDrvPort = core::ptr::null_mut();
            assert_eq!(
                driver_output(null_port, core::ptr::null_mut(), 0),
                -1,
                "driver_output"
            );
            assert_eq!(
                driver_output2(
                    null_port,
                    core::ptr::null_mut(),
                    0,
                    core::ptr::null_mut(),
                    0
                ),
                -1,
                "driver_output2"
            );
            assert_eq!(
                driver_output_binary(
                    null_port,
                    core::ptr::null_mut(),
                    0,
                    core::ptr::null_mut(),
                    0,
                    0
                ),
                -1,
                "driver_output_binary"
            );
            assert_eq!(
                driver_outputv(
                    null_port,
                    core::ptr::null_mut(),
                    0,
                    core::ptr::null_mut(),
                    0
                ),
                -1,
                "driver_outputv"
            );
            assert_eq!(
                driver_output_term(null_port, core::ptr::null_mut(), 0),
                -1,
                "driver_output_term"
            );
            assert_eq!(
                driver_send_term(null_port, driver_term_nil, core::ptr::null_mut(), 0),
                -1,
                "driver_send_term"
            );
            assert_eq!(
                erl_drv_output_term(0, core::ptr::null_mut(), 0),
                -1,
                "erl_drv_output_term"
            );
            assert_eq!(
                erl_drv_send_term(0, driver_term_nil, core::ptr::null_mut(), 0),
                -1,
                "erl_drv_send_term"
            );
            assert_eq!(driver_connected(null_port), driver_term_nil);
            assert_eq!(driver_caller(null_port), driver_term_nil);
            assert_eq!(driver_mk_port(null_port), driver_term_nil);
            assert_eq!(driver_set_timer(null_port, 0), -1, "driver_set_timer");
            assert_eq!(driver_cancel_timer(null_port), -1, "driver_cancel_timer");
            assert_eq!(
                driver_read_timer(null_port, core::ptr::null_mut()),
                -1,
                "driver_read_timer"
            );
            assert_eq!(
                driver_select(null_port, core::ptr::null_mut(), ERL_DRV_READ, 1),
                -1,
                "driver_select"
            );
            assert_eq!(
                driver_async(
                    null_port,
                    core::ptr::null_mut(),
                    None,
                    core::ptr::null_mut(),
                    None
                ),
                -1,
                "driver_async"
            );
            // A negative length is malformed even without a port.
            assert_eq!(
                driver_output_term(null_port, core::ptr::null_mut(), -1),
                -1,
                "a negative spec length"
            );
        }
    }

    #[test]
    fn driver_term_data_round_trips_through_the_table() {
        // The table's port-carrying slots are the very functions the tests above call: driving one
        // through the table proves the frozen table and the `#[no_mangle]` symbol agree.
        let t = driver_api_table();
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        let output = t.driver_output.expect("driver_output is populated");
        let mut body = *b"via the table";
        // SAFETY: the table slot is `driver_output` itself; `port`/`body` are live.
        let rc = unsafe { output(port, body.as_mut_ptr() as *mut c_char, body.len()) };
        assert_eq!(rc, 0);
        assert_eq!(host.output, b"via the table");

        let mk_port = t.driver_mk_port.expect("driver_mk_port is populated");
        // SAFETY: `port` is a live handle.
        let port_id = unsafe { mk_port(port) };
        let name = CString::new("x").expect("no interior NUL");
        let spec = [ERL_DRV_ATOM, term_data(name.as_ptr() as usize)];
        let output_term = t
            .erl_drv_output_term
            .expect("erl_drv_output_term is populated");
        // SAFETY: `port_id` encodes the live port and `spec` is a live two-word array.
        let rc = unsafe { output_term(port_id, spec.as_ptr() as *mut ErlDrvTermData, 2) };
        assert_eq!(rc, 0);
        assert_eq!(host.sent, vec![(None, make_atom(host.atoms.intern("x")))]);
    }
}
