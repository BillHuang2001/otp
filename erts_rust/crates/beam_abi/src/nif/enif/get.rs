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

//! `enif_*` — the `enif_get_*` accessors.
//!
//! Owned slots: `enif_get_int`, `enif_get_uint`, `enif_get_long`, `enif_get_ulong`,
//! `enif_get_double`, `enif_get_list_cell`, `enif_get_tuple`, `enif_get_list_length`,
//! `enif_get_string`, `enif_get_string_length`, `enif_get_atom`, `enif_get_atom_length`.
//!
//! `enif_get_int64`/`enif_get_uint64` are also implemented here, but on LP64 the header `#define`s
//! them onto their `long`/`ulong` siblings, so they are free symbols rather than `ErlNifApi` slots.
//!
//! # Atom accessors and the per-call atom record
//!
//! [`enif_get_atom`] and [`enif_get_atom_length`] resolve an atom's name through the env's
//! **per-call record first** (see [`NifEnv::recorded_atom_name`]) and fall back to
//! [`NifHost::atom_name`]. The record closes a real divergence: the runtime commits a freshly
//! interned atom lazily, so the host's reverse lookup does not see an atom interned earlier in the
//! *same* call, and `enif_get_atom` would answer `0` where the oracle answers the name. A miss
//! falls through to the hook exactly as before.

use super::*;

use libc::{c_char, c_double, c_int, c_long, c_uint, c_ulong};

use crate::nif::types::ErlNifCharEncoding;

/// Populate the `ErlNifApi` slots this file owns (see the module docs).
pub(super) const fn populate(mut api: ErlNifApi) -> ErlNifApi {
    api.enif_get_int = Some(enif_get_int);
    api.enif_get_uint = Some(enif_get_uint);
    api.enif_get_long = Some(enif_get_long);
    api.enif_get_ulong = Some(enif_get_ulong);
    api.enif_get_double = Some(enif_get_double);
    api.enif_get_list_cell = Some(enif_get_list_cell);
    api.enif_get_tuple = Some(enif_get_tuple);
    api.enif_get_list_length = Some(enif_get_list_length);
    api.enif_get_string = Some(enif_get_string);
    api.enif_get_string_length = Some(enif_get_string_length);
    api.enif_get_atom = Some(enif_get_atom);
    api.enif_get_atom_length = Some(enif_get_atom_length);
    api
}

// ---------------------------------------------------------------------------------------------
// Accessors
// ---------------------------------------------------------------------------------------------

/// `int enif_get_int(ErlNifEnv*, ERL_NIF_TERM term, int* ip);`
///
/// # Safety
///
/// `ip` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_int(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    ip: *mut c_int,
) -> c_int {
    match term_to_sint64(t(term)) {
        Some(v) if (c_int::MIN as i64..=c_int::MAX as i64).contains(&v) => {
            // SAFETY: `ip` may be null (handled) or valid for one write (caller contract).
            (unsafe { write_out(ip, v as c_int) }) as c_int
        }
        _ => 0,
    }
}

/// `int enif_get_uint(ErlNifEnv*, ERL_NIF_TERM term, unsigned* ip);`
///
/// # Safety
///
/// `ip` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_uint(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    ip: *mut c_uint,
) -> c_int {
    match term_to_uint64(t(term)) {
        Some(v) if v <= c_uint::MAX as u64 => {
            // SAFETY: `ip` may be null (handled) or valid for one write.
            (unsafe { write_out(ip, v as c_uint) }) as c_int
        }
        _ => 0,
    }
}

/// `int enif_get_long(ErlNifEnv*, ERL_NIF_TERM term, long* ip);`
///
/// # Safety
///
/// `ip` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_long(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    ip: *mut c_long,
) -> c_int {
    match term_to_sint64(t(term)) {
        // SAFETY: `ip` may be null (handled) or valid for one write.
        Some(v) => (unsafe { write_out(ip, v as c_long) }) as c_int,
        None => 0,
    }
}

/// `int enif_get_ulong(ErlNifEnv*, ERL_NIF_TERM term, unsigned long* ip);`
///
/// # Safety
///
/// `ip` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_ulong(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    ip: *mut c_ulong,
) -> c_int {
    match term_to_uint64(t(term)) {
        // SAFETY: `ip` may be null (handled) or valid for one write.
        Some(v) => (unsafe { write_out(ip, v as c_ulong) }) as c_int,
        None => 0,
    }
}

/// `int enif_get_int64(ErlNifEnv*, ERL_NIF_TERM term, ErlNifSInt64* ip);`
///
/// On LP64 the header `#define`s this onto [`enif_get_long`]; it is exported here as a free symbol
/// too (it is not a separate `ErlNifApi` slot on LP64).
///
/// # Safety
///
/// `ip` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_int64(
    env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    ip: *mut i64,
) -> c_int {
    // SAFETY: `ip` is a `*mut i64` and `c_long` is `i64` on LP64; same contract.
    unsafe { enif_get_long(env, term, ip.cast::<c_long>()) }
}

/// `int enif_get_uint64(ErlNifEnv*, ERL_NIF_TERM term, ErlNifUInt64* ip);`
///
/// On LP64 the header `#define`s this onto [`enif_get_ulong`].
///
/// # Safety
///
/// `ip` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_uint64(
    env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    ip: *mut u64,
) -> c_int {
    // SAFETY: `ip` is a `*mut u64` and `c_ulong` is `u64` on LP64; same contract.
    unsafe { enif_get_ulong(env, term, ip.cast::<c_ulong>()) }
}

/// `int enif_get_double(ErlNifEnv*, ERL_NIF_TERM term, double* dp);`
///
/// # Safety
///
/// `dp` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_double(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    dp: *mut c_double,
) -> c_int {
    let term = t(term);
    if !is_term_word(term) {
        return 0;
    }
    // SAFETY: `is_float` only dereferences the header of a boxed term.
    if !unsafe { prelude::is_float(term) } {
        return 0;
    }
    // SAFETY: `term` is a live boxed float.
    let value = unsafe { prelude::float_value(term) };
    // SAFETY: `dp` may be null (handled) or valid for one write.
    (unsafe { write_out(dp, value) }) as c_int
}

/// `int enif_get_atom(ErlNifEnv*, ERL_NIF_TERM atom, char* buf, unsigned len, ErlNifCharEncoding);`
///
/// Returns the number of bytes written **including the NUL**, or `0` on failure. The name is taken
/// from the env's per-call atom record first, then from [`NifHost::atom_name`].
///
/// # Safety
///
/// `env` must be a live env; `buf` must be null or point at `len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn enif_get_atom(
    env: *mut crate::nif::types::ErlNifEnv,
    atom: ERL_NIF_TERM,
    buf: *mut c_char,
    len: c_uint,
    encoding: ErlNifCharEncoding,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    let name: String = match e.recorded_atom_name(t(atom)) {
        Some(name) => name.to_string(),
        None => match e.host().atom_name(t(atom)) {
            Some(name) => name,
            None => return 0,
        },
    };
    let len = len as usize;
    if len == 0 || buf.is_null() {
        return 0;
    }
    match encoding {
        ErlNifCharEncoding::Latin1 => {
            let mut bytes = Vec::with_capacity(name.len());
            for ch in name.chars() {
                let code = ch as u32;
                if code > 0xFF {
                    // Not representable in Latin-1 (the C's `latin1_chars < 0`).
                    return 0;
                }
                bytes.push(code as u8);
            }
            if bytes.len() >= len {
                return 0;
            }
            // SAFETY: `buf` has `len > bytes.len()` writable bytes; write the name and its NUL.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), buf, bytes.len());
                buf.add(bytes.len()).write(0);
            }
            (bytes.len() + 1) as c_int
        }
        ErlNifCharEncoding::Utf8 => {
            let bytes = name.as_bytes();
            if bytes.len() >= len {
                return 0;
            }
            // SAFETY: `buf` has `len > bytes.len()` writable bytes; write the name and its NUL.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), buf, bytes.len());
                buf.add(bytes.len()).write(0);
            }
            (bytes.len() + 1) as c_int
        }
    }
}

/// `int enif_get_atom_length(ErlNifEnv*, ERL_NIF_TERM atom, unsigned* len, ErlNifCharEncoding);`
///
/// The name is taken from the env's per-call atom record first, then from [`NifHost::atom_name`].
///
/// # Safety
///
/// `env` must be a live env; `len` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_atom_length(
    env: *mut crate::nif::types::ErlNifEnv,
    atom: ERL_NIF_TERM,
    len: *mut c_uint,
    encoding: ErlNifCharEncoding,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    let name: String = match e.recorded_atom_name(t(atom)) {
        Some(name) => name.to_string(),
        None => match e.host().atom_name(t(atom)) {
            Some(name) => name,
            None => return 0,
        },
    };
    let n = match encoding {
        ErlNifCharEncoding::Latin1 => {
            if name.chars().any(|ch| (ch as u32) > 0xFF) {
                0
            } else {
                name.chars().count() as c_uint
            }
        }
        ErlNifCharEncoding::Utf8 => name.len() as c_uint,
    };
    // SAFETY: `len` may be null (handled) or valid for one write.
    (unsafe { write_out(len, n) }) as c_int
}

/// `int enif_get_string(ErlNifEnv*, ERL_NIF_TERM list, char* buf, unsigned len, ErlNifCharEncoding);`
///
/// # Safety
///
/// `env` must be a live env; `buf` must be null or point at `len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn enif_get_string(
    _env: *mut crate::nif::types::ErlNifEnv,
    list: ERL_NIF_TERM,
    buf: *mut c_char,
    len: c_uint,
    encoding: ErlNifCharEncoding,
) -> c_int {
    let len = len as usize;
    if buf.is_null() || len < 1 {
        return 0;
    }
    match encoding {
        ErlNifCharEncoding::Latin1 => {
            let mut n = 0usize;
            let mut cur = t(list);
            loop {
                if !prelude::is_list(cur) {
                    break;
                }
                // SAFETY: `cur` is a live cons cell.
                let (head, tail) = unsafe { (prelude::car(cur), prelude::cdr(cur)) };
                if !prelude::is_byte(head) {
                    // SAFETY: `n < len` here (the overflow return is below), so `buf[n]` is writable.
                    unsafe { buf.add(n).write(0) };
                    return 0;
                }
                // SAFETY: `n < len`, so index `n` is inside the caller's buffer.
                unsafe { buf.add(n).write(prelude::unsigned_value(head) as c_char) };
                n += 1;
                if n >= len {
                    // SAFETY: `n >= 1` and `n - 1 < len`.
                    unsafe { buf.add(n - 1).write(0) };
                    return -(n as c_int);
                }
                cur = tail;
            }
            if prelude::is_not_nil(cur) {
                // SAFETY: `n < len` (checked in the loop).
                unsafe { buf.add(n).write(0) };
                return 0;
            }
            // SAFETY: `n < len`; terminate the string.
            unsafe { buf.add(n).write(0) };
            (n + 1) as c_int
        }
        ErlNifCharEncoding::Utf8 => {
            let mut codepoints: Vec<char> = Vec::new();
            let mut cur = t(list);
            while prelude::is_list(cur) {
                // SAFETY: `cur` is a live cons cell.
                let (head, tail) = unsafe { (prelude::car(cur), prelude::cdr(cur)) };
                match decode_codepoint(head) {
                    Some(ch) => codepoints.push(ch),
                    None => {
                        // SAFETY: `len >= 1`.
                        unsafe { buf.write(0) };
                        return 0;
                    }
                }
                cur = tail;
            }
            if prelude::is_not_nil(cur) {
                // SAFETY: `len >= 1`.
                unsafe { buf.write(0) };
                return 0;
            }
            let encoded: Vec<u8> = codepoints.iter().collect::<String>().into_bytes();
            let mut n = 0usize;
            for &byte in &encoded {
                // SAFETY: `n < len` (the overflow branch returns first), so `buf[n]` is writable.
                unsafe { buf.add(n).write(byte as c_char) };
                n += 1;
                if n >= len {
                    // SAFETY: `n >= 1` and `n - 1 < len`.
                    unsafe { buf.add(n - 1).write(0) };
                    return -(n as c_int);
                }
            }
            // SAFETY: `n < len` here (the loop returns as soon as `n >= len`).
            unsafe { buf.add(n).write(0) };
            (n + 1) as c_int
        }
    }
}

/// Decode a list element as a Unicode codepoint (`None` for anything else).
fn decode_codepoint(term: Eterm) -> Option<char> {
    let value = if prelude::is_small(term) {
        let v = prelude::signed_value(term);
        if v < 0 {
            return None;
        }
        v as u64
    } else {
        term_to_uint64(term)?
    };
    u32::try_from(value).ok().and_then(char::from_u32)
}

/// `int enif_get_string_length(ErlNifEnv *env, ERL_NIF_TERM list, unsigned *len, ErlNifCharEncoding);`
///
/// # Safety
///
/// `len` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_string_length(
    _env: *mut crate::nif::types::ErlNifEnv,
    list: ERL_NIF_TERM,
    len: *mut c_uint,
    encoding: ErlNifCharEncoding,
) -> c_int {
    let mut cur = t(list);
    if prelude::is_nil(cur) {
        // SAFETY: `len` may be null (handled) or valid for one write.
        return (unsafe { write_out(len, 0) }) as c_int;
    }
    match encoding {
        ErlNifCharEncoding::Latin1 => {
            let mut n = 0u32;
            while prelude::is_list(cur) {
                // SAFETY: `cur` is a live cons cell.
                let (head, tail) = unsafe { (prelude::car(cur), prelude::cdr(cur)) };
                if !prelude::is_byte(head) {
                    return 0;
                }
                n = match n.checked_add(1) {
                    Some(n) => n,
                    None => return 0,
                };
                cur = tail;
            }
            if prelude::is_not_nil(cur) {
                return 0;
            }
            // SAFETY: `len` may be null (handled) or valid for one write.
            (unsafe { write_out(len, n) }) as c_int
        }
        ErlNifCharEncoding::Utf8 => {
            let mut codepoints: Vec<char> = Vec::new();
            while prelude::is_list(cur) {
                // SAFETY: `cur` is a live cons cell.
                let (head, tail) = unsafe { (prelude::car(cur), prelude::cdr(cur)) };
                match decode_codepoint(head) {
                    Some(ch) => codepoints.push(ch),
                    None => return 0,
                }
                cur = tail;
            }
            if prelude::is_not_nil(cur) {
                return 0;
            }
            let bytes = codepoints.iter().collect::<String>().len();
            // SAFETY: `len` may be null (handled) or valid for one write.
            (unsafe { write_out(len, bytes as c_uint) }) as c_int
        }
    }
}

/// `int enif_get_tuple(ErlNifEnv* env, ERL_NIF_TERM tpl, int* arity, const ERL_NIF_TERM** array);`
///
/// # Safety
///
/// `arity` and `array` must be null or writable; the returned array borrows the tuple's storage.
#[no_mangle]
pub unsafe extern "C" fn enif_get_tuple(
    _env: *mut crate::nif::types::ErlNifEnv,
    tpl: ERL_NIF_TERM,
    arity: *mut c_int,
    array: *mut *const ERL_NIF_TERM,
) -> c_int {
    let term = t(tpl);
    if !is_term_word(term) {
        return 0;
    }
    // SAFETY: `is_tuple` only dereferences the header of a boxed term.
    if unsafe { prelude::is_not_tuple(term) } {
        return 0;
    }
    // SAFETY: `term` is a live tuple.
    let size = unsafe { prelude::tuple_size(term) };
    // SAFETY: `arity` may be null (handled) or valid for one write.
    if !(unsafe { write_out(arity, size as c_int) }) {
        return 0;
    }
    // SAFETY: `term` is a live tuple (checked above); `tuple_val` returns its first word and the
    // element array starts at the word after the header (`tuple_val(t) + 1`). `array` may be null
    // (handled) or valid for one write.
    let elements = unsafe { prelude::tuple_val(term).add(1) }
        .cast::<ERL_NIF_TERM>()
        .cast_const();
    // SAFETY: `array` may be null (handled) or valid for one write.
    (unsafe { write_out(array, elements) }) as c_int
}

/// `int enif_get_list_cell(ErlNifEnv* env, ERL_NIF_TERM term, ERL_NIF_TERM* head, ERL_NIF_TERM* tail);`
///
/// # Safety
///
/// `head` and `tail` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_list_cell(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    head: *mut ERL_NIF_TERM,
    tail: *mut ERL_NIF_TERM,
) -> c_int {
    let term = t(term);
    if prelude::is_not_list(term) {
        return 0;
    }
    // SAFETY: `term` is a live cons cell.
    let (car, cdr) = unsafe { (prelude::car(term), prelude::cdr(term)) };
    // SAFETY: both out-pointers may be null (handled) or valid for one write; both are written
    // (no short-circuit) so a null `head` never suppresses the `tail` write.
    let wrote_head = unsafe { write_out(head, n(car)) };
    let wrote_tail = unsafe { write_out(tail, n(cdr)) };
    (wrote_head && wrote_tail) as c_int
}

/// `int enif_get_list_length(ErlNifEnv* env, ERL_NIF_TERM term, unsigned* len);`
///
/// # Safety
///
/// `len` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_list_length(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    len: *mut c_uint,
) -> c_int {
    let mut cur = t(term);
    let mut n: u64 = 0;
    loop {
        if prelude::is_nil(cur) {
            // SAFETY: `len` may be null (handled) or valid for one write.
            return (unsafe { write_out(len, n as c_uint) }) as c_int;
        }
        if !prelude::is_list(cur) {
            return 0;
        }
        n += 1;
        if n > c_uint::MAX as u64 {
            return 0;
        }
        // SAFETY: `cur` is a live cons cell.
        cur = unsafe { prelude::cdr(cur) };
    }
}
