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
//! # Deliberate divergences: refuse rather than fault
//!
//! Where the C dereferences an operand this ABI cannot vouch for, these wrappers refuse with the C's
//! own failure value instead of faulting (the same policy [`super::pdl`]'s monitor family and the
//! queue family follow). One case cannot be cross-checked against the oracle at all:
//! `driver_mk_port(NULL)`. The legacy VM does **not** null-check the operand — it hands the `NULL`
//! port to its signal path and the emulator **exits on the spot** (a probe sees an exit signal and
//! empty stdout, so there is no observable result to pin). This wrapper keeps the safe behaviour and
//! returns [`driver_term_nil`] for a null handle — the value the C returns for an *invalid* (but
//! non-null) one — rather than reproducing a crash. See that function's docs.
//!
//! The port/host-dependent half **is** the larger half of the C's surface, so its tests (the
//! port-pointer recovery, the term-data round-trips through the populated table, and the
//! output/select/timer host traffic) live in the sibling `port_tests.rs`, pulled in by the
//! `#[path]`-declared `mod tests` at the end of this file.
//!
//! # `driver_mk_port`'s handle encoding resolves back to the port term
//!
//! [`driver_mk_port`] answers with the **handle** encoding (a port handle here *is* the
//! [`DriverEnv`](crate::driver::host::DriverEnv) pointer), where the C answers the port's id term.
//! That is unchanged: the `erl_drv_output_term`/`erl_drv_send_term` **port parameter** decodes the
//! same encoding back to the handle. What closes the loop for a driver written the C way is
//! [`DriverHost::port_term`](crate::driver::host::DriverHost::port_term): an `ERL_DRV_PORT`
//! **operand** carrying this encoding is resolved to the port's id term by
//! [`decode_term_spec`](crate::driver::host::decode_term_spec), so
//! `{ERL_DRV_PORT, driver_mk_port(port)}` delivers the port term exactly as the emulator does.
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

/// The C's `buf != NULL` distinction (`deliver_read_message`, `io.c:3372`; `deliver_vec_message`,
/// `io.c:3502`) — the decisive test is the **pointer**, never `len != 0`.
///
/// The outer `None` is malformed (a null `buf` with a non-zero `len`, which the C would deref); the
/// inner `None` is the C's `buf == NULL` — **no tail at all** — while `Some(&[])` is a present,
/// empty tail (the C's `<<>>`). The two must stay distinct: `[h…]` and `[h… | <<>>]` are different
/// terms.
///
/// # Safety
///
/// `buf` must be null or point at `len` readable bytes.
unsafe fn tail_bytes<'a>(buf: *const c_char, len: ErlDrvSizeT) -> Option<Option<&'a [u8]>> {
    if buf.is_null() {
        return if len == 0 { Some(None) } else { None };
    }
    // SAFETY: the caller guarantees `len` readable bytes at `buf`.
    Some(Some(unsafe {
        core::slice::from_raw_parts(buf as *const u8, len)
    }))
}

/// Hand a **split** reply to the port owner through the
/// [`DriverHost::output2`](crate::driver::host::DriverHost::output2) seam: `header` is the C's
/// `hbuf`/`hlen` list prefix and `body` its `buf`/`len` tail (`None` = `buf == NULL`).
///
/// Nothing is joined here — preserving the split is the point of the hook. Returns the C's `-1` when
/// the operand names no port — the invalid-port sentinel
/// [`ERTS_INVALID_ERL_DRV_PORT`](crate::driver::types::ERTS_INVALID_ERL_DRV_PORT) (the C's own
/// failure path) or `NULL` (refused here instead of faulted) — else the host's code.
///
/// # Safety
///
/// `port` must be null or a live port handle.
unsafe fn deliver_split(port: ErlDrvPort, header: &[u8], body: Option<&[u8]>) -> c_int {
    // SAFETY: `port` is null or a live handle (contract above).
    match unsafe { env_for(port) } {
        Some(env) => env.host().output2(port, header, body),
        None => -1,
    }
}

/// `int driver_output(ErlDrvPort port, char *buf, ErlDrvSizeT len);` — send `len` bytes at `buf` to
/// the port owner (`io.c:6442`, which is literally `driver_output2(ix, NULL, 0, buf, len)`).
///
/// There is no header, and `buf`'s pointer — not `len` — decides whether the reply carries a tail:
/// a null `buf` sends the header-only (proper, empty) list, a non-null `buf` with `len == 0` sends a
/// present, empty tail (`<<>>` in `[binary]` port mode). Both reach the host as such through
/// [`DriverHost::output2`](crate::driver::host::DriverHost::output2). A null `buf` with a non-zero
/// `len` is refused with `-1` instead of being dereferenced.
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
    let Some(body) = (unsafe { tail_bytes(buf, len) }) else {
        return -1;
    };
    // SAFETY: `port` is null or a live handle, and there is no header (contract above).
    unsafe { deliver_split(port, &[], body) }
}

/// `int driver_output2(ErlDrvPort port, char *hbuf, ErlDrvSizeT hlen, char *buf, ErlDrvSizeT len);`
/// — send a header followed by body data (`io.c:6387`; the header goes out as a list prefix and the
/// body as the tail, `[H1,H2,H3 | T]` per the C's own comment).
///
/// The header stays a prefix and the body a tail: `buf`'s pointer decides whether a tail exists at
/// all (a null `buf` is the C's `buf == NULL`, a header-only reply), so nothing is joined. A null
/// `buf` with a non-zero `len` is refused with `-1` instead of being dereferenced.
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
    // SAFETY: `hbuf`/`hlen` are the caller's header buffer (contract above).
    let Some(header) = (unsafe { read_bytes(hbuf as *const u8, hlen) }) else {
        return -1;
    };
    // SAFETY: `buf`/`len` are the caller's body buffer (contract above).
    let Some(body) = (unsafe { tail_bytes(buf, len) }) else {
        return -1;
    };
    // SAFETY: `port` is null or a live handle (contract above).
    unsafe { deliver_split(port, header, body) }
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
/// The sub-range is always a **present** body (the payload of a driver binary is never the C's
/// `buf == NULL`), so a zero-length sub-range reaches the host as `Some(&[])` — the C's `<<>>` in
/// `[binary]` mode — not as a header-only reply. A null binary or an out-of-range sub-range is
/// refused with `-1` (see [`binary_payload_bytes`]).
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
    // SAFETY: `hbuf`/`hlen` are the caller's header buffer (contract above).
    let Some(header) = (unsafe { read_bytes(hbuf as *const u8, hlen) }) else {
        return -1;
    };
    // SAFETY: `bin`/`offs`/`len` are the caller's driver binary and its sub-range (contract above).
    let Some(body) = (unsafe { binary_payload_bytes(bin, offs, len) }) else {
        return -1;
    };
    // SAFETY: `port` is null or a live handle (contract above).
    unsafe { deliver_split(port, header, Some(body)) }
}

/// The body of a `driver_outputv` reply: `ev`'s vectors **after** the C's `skip` stripping, with the
/// last remaining vector as the improper tail (`deliver_vec_message`, `io.c:3502-3621`).
enum VecBody<'a> {
    /// The C's `vec->size <= skip` early return — `return driver_output2(ix, hbuf, hlen, NULL, 0)`
    /// (`io.c:6469`): a header and **no** tail at all (the proper list `[h…]`), which is *not* the
    /// same term as an empty-binary tail.
    NoTail,
    /// The remaining vectors: `prefix` are the ones before the last (in order) and `tail` is the
    /// last one, which the C leaves as the improper tail of the reply. `prefix` is empty for the
    /// single-vector case, which is byte-for-byte (and shape-for-shape) the C.
    Vectors {
        prefix: Vec<&'a [u8]>,
        tail: &'a [u8],
    },
}

/// Strip the first `skip` bytes off `ev`'s vectors exactly as `driver_outputv` does
/// (`io.c:6448-6504`) and split what remains into the C's header part and its improper tail.
///
/// The C's stripping loop is a `do { … } while (skip > 0)` (`io.c:6478-6493`), so it runs **at least
/// once**: with `skip == 0` it still consumes a single leading zero-length vector. That is
/// reproduced here. A `skip` past the vectors' declared lengths is malformed (`None`), never a
/// partial body.
///
/// # Safety
///
/// `ev` must be null or point at a live [`ErlIOVec`] whose `iov` array holds `vsize` readable
/// `SysIOVec` entries, each heading `iov_len` readable bytes.
unsafe fn split_iovec<'a>(ev: *const ErlIOVec, skip: ErlDrvSizeT) -> Option<VecBody<'a>> {
    if ev.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a live `ErlIOVec` at `ev`.
    let (vsize, size, iov) = unsafe { ((*ev).vsize, (*ev).size, (*ev).iov) };
    if vsize < 0 {
        return None;
    }
    if size <= skip {
        return Some(VecBody::NoTail); // The C's `vec->size <= skip`: header only.
    }
    if iov.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `vsize` readable `SysIOVec` entries at `iov`.
    let entries = unsafe { core::slice::from_raw_parts(iov, vsize as usize) };

    // The C's `do { … } while (skip > 0)` (`io.c:6478-6493`): `idx` walks past fully-skipped
    // vectors, and `offset` is how far into the first *remaining* vector the view starts.
    let mut idx = 0usize;
    let mut offset = 0usize;
    let mut left = skip;
    loop {
        let Some(entry) = entries.get(idx) else {
            return None; // `skip` ran past the vectors' declared lengths: malformed.
        };
        let len = entry.iov_len;
        if len <= left {
            left -= len;
            idx += 1;
            if left == 0 {
                break; // `while (skip > 0)`: no skip remains, the loop ends.
            }
        } else {
            offset = left;
            break;
        }
    }
    let mut remaining = Vec::with_capacity(entries.len().saturating_sub(idx));
    for (i, entry) in entries[idx..].iter().enumerate() {
        let start = if i == 0 { offset } else { 0 };
        let len = entry.iov_len;
        if len <= start {
            remaining.push(&[] as &[u8]);
            continue;
        }
        // SAFETY: `iov_base` heads `iov_len` readable bytes (caller contract); the view starts
        // `start` bytes in and runs to the entry's end.
        remaining.push(unsafe {
            core::slice::from_raw_parts((entry.iov_base as *const u8).add(start), len - start)
        });
    }
    let Some((tail, prefix)) = remaining.split_last() else {
        return None; // `size > skip` guarantees at least one remaining vector.
    };
    Some(VecBody::Vectors {
        prefix: prefix.to_vec(),
        tail,
    })
}
/// `int driver_outputv(ErlDrvPort port, char *hbuf, ErlDrvSizeT hlen, ErlIOVec *ev,
/// ErlDrvSizeT skip);` — send a header plus a scatter/gather vector with the first `skip` bytes
/// dropped (`io.c:6448`).
///
/// After the C's skipping, the vectors are handed to `deliver_vec_message` (`io.c:3502-3621`), which
/// makes each remaining vector its own binary cons cell and leaves the **last** one as the improper
/// tail: `[h… | <<v0>>, <<v1>>, …, <<vlast>>]` in `[binary]` port mode (an int list in `[list]`
/// mode). This seam carries one header byte stream plus one tail, so:
///
/// * the **last** remaining vector is the tail (`body`) and `hbuf` plus every preceding remaining
///   vector's bytes (in order) are the header — for the single-vector case, which is every inet
///   driver read path, that is byte-for-byte *and* shape-for-shape the C (`[h… | <<v0>>]`);
/// * for more than one remaining vector the flattened byte sequence (and `[list]` mode entirely) is
///   identical, but the **nesting of the earlier vectors** differs: the C makes each of them its own
///   binary cons cell while this seam folds their bytes into the header's int list
///   (`[h…, v0 bytes… | <<vlast>>]`). That is a seam-width residual (the host hook carries one
///   header stream and one tail), never a byte difference — see
///   [`DriverHost::output2`](crate::driver::host::DriverHost::output2).
/// * `ev->size <= skip` is the C's header-only reply (`body: None`), not an empty tail.
///
/// # Safety
///
/// `port` must be null or a live port handle; `hbuf` must be null or point at `hlen` readable bytes;
/// `ev` must be null or a live vector as required by [`split_iovec`].
#[no_mangle]
pub unsafe extern "C" fn driver_outputv(
    port: ErlDrvPort,
    hbuf: *mut c_char,
    hlen: ErlDrvSizeT,
    ev: *mut ErlIOVec,
    skip: ErlDrvSizeT,
) -> c_int {
    // SAFETY: `ev`/`skip` are the caller's vector and its skip count (contract above).
    let Some(body) = (unsafe { split_iovec(ev, skip) }) else {
        return -1;
    };
    // SAFETY: `hbuf`/`hlen` are the caller's header buffer (contract above).
    let Some(header) = (unsafe { read_bytes(hbuf as *const u8, hlen) }) else {
        return -1;
    };
    match body {
        // SAFETY: `port` is null or a live handle (contract above).
        VecBody::NoTail => unsafe { deliver_split(port, header, None) },
        VecBody::Vectors { prefix, tail } if prefix.is_empty() => {
            // SAFETY: `port` is null or a live handle (contract above).
            unsafe { deliver_split(port, header, Some(tail)) }
        }
        VecBody::Vectors { prefix, tail } => {
            let extra: usize = prefix.iter().map(|piece| piece.len()).sum();
            let mut joined = Vec::with_capacity(header.len() + extra);
            joined.extend_from_slice(header);
            for piece in &prefix {
                joined.extend_from_slice(piece);
            }
            // SAFETY: `port` is null or a live handle (contract above).
            unsafe { deliver_split(port, &joined, Some(tail)) }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Term data (host)
// ---------------------------------------------------------------------------------------------

/// The C-facing port pointer encoded in an `ErlDrvTermData` (the `erl_drv_*_term` port operand).
///
/// [`driver_mk_port`] produces it; the C passes the port's *id term* instead (documented deviation,
/// see the module header). Only the `erl_drv_*_term` entry points' **port parameter** uses this
/// cast — an `ERL_DRV_PORT` **operand** carrying the same encoding is resolved to the port's id term
/// by [`decode_term_spec`] through
/// [`DriverHost::port_term`](crate::driver::host::DriverHost::port_term).
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
/// [`driver_async_port_key`](crate::driver::api::ErlDrvApi::driver_async_port_key) would key on.
///
/// The encoding is resolvable **back to the id term**: an `ERL_DRV_PORT` term-data operand carrying
/// it is turned into the port term by [`decode_term_spec`] through
/// [`DriverHost::port_term`](crate::driver::host::DriverHost::port_term), so the C idiom
/// `{ERL_DRV_PORT, driver_mk_port(port)}` delivers the port term on a runtime that implements that
/// hook.
///
/// A null handle yields [`driver_term_nil`] — the value the C returns for an invalid (non-null)
/// handle. That is a **deliberate, documented divergence** (see the module header): the legacy VM
/// does not null-check the operand, and `driver_mk_port(NULL)` makes it crash (an exit signal with
/// empty stdout), so the oracle behaviour is a probe hazard that cannot be pinned or reproduced —
/// the safe Rust answer is kept instead.
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
