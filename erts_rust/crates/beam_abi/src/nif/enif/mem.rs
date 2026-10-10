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

//! `enif_*` — allocation, the binary family and the ETF pair.
//!
//! Owned slots: `enif_alloc`, `enif_free`, `enif_realloc`, `enif_alloc_binary`,
//! `enif_realloc_binary`, `enif_release_binary`, `enif_inspect_binary`,
//! `enif_inspect_iolist_as_binary`, `enif_make_new_binary`, `enif_make_binary`,
//! `enif_make_sub_binary`, `enif_term_to_binary`, `enif_binary_to_term`.
//!
//! # `ErlNifBinary` ownership marker
//!
//! The C runtime distinguishes an *owned* (`enif_alloc_binary`'d) `ErlNifBinary` from an *inspected*
//! zero-copy view through `ref_bin` (`NULL` = owned). Here `ref_bin` is always `NULL` and the
//! distinction lives in `__spare__[0]`:
//!
//! * `__spare__[0] == null` — **owned**: the payload was `malloc`'d and [`enif_release_binary`] /
//!   [`enif_realloc_binary`] may `free`/`realloc` it.
//! * `__spare__[0] != null` (the sentinel `1 as *mut c_void`) — **inspected**: the payload points
//!   into the BEAM heap; release is a no-op.

use super::*;

use libc::{c_int, c_uchar, c_uint, c_void, size_t};

use crate::nif::types::ErlNifBinary;

/// Populate the `ErlNifApi` slots this file owns (see the module docs).
pub(super) const fn populate(mut api: ErlNifApi) -> ErlNifApi {
    api.enif_alloc = Some(enif_alloc);
    api.enif_free = Some(enif_free);
    api.enif_realloc = Some(enif_realloc);
    api.enif_inspect_binary = Some(enif_inspect_binary);
    api.enif_inspect_iolist_as_binary = Some(enif_inspect_iolist_as_binary);
    api.enif_alloc_binary = Some(enif_alloc_binary);
    api.enif_realloc_binary = Some(enif_realloc_binary);
    api.enif_release_binary = Some(enif_release_binary);
    api.enif_make_new_binary = Some(enif_make_new_binary);
    api.enif_make_binary = Some(enif_make_binary);
    api.enif_make_sub_binary = Some(enif_make_sub_binary);
    api.enif_term_to_binary = Some(enif_term_to_binary);
    api.enif_binary_to_term = Some(enif_binary_to_term);
    api
}

// ---------------------------------------------------------------------------------------------
// Allocation
// ---------------------------------------------------------------------------------------------

/// `void *enif_alloc(size_t size);`
///
/// # Safety
///
/// The returned block must be released with [`enif_free`] (or [`enif_realloc`]); no provenance
/// obligations beyond that of `malloc`.
#[no_mangle]
pub unsafe extern "C" fn enif_alloc(size: size_t) -> *mut c_void {
    // SAFETY: `malloc` returns null on failure, which the caller must check (C semantics).
    unsafe { libc::malloc(size) }
}

/// `void enif_free(void *ptr);`
///
/// # Safety
///
/// `ptr` must be null or a pointer returned by [`enif_alloc`]/[`enif_realloc`] that has not already
/// been freed.
#[no_mangle]
pub unsafe extern "C" fn enif_free(ptr: *mut c_void) {
    // SAFETY: `free(NULL)` is a no-op; the caller guarantees any non-null pointer was `malloc`'d.
    unsafe { libc::free(ptr) };
}

/// `void *enif_realloc(void *ptr, size_t size);`
///
/// # Safety
///
/// `ptr` must be null or a pointer returned by [`enif_alloc`]/[`enif_realloc`] that has not already
/// been freed.
#[no_mangle]
pub unsafe extern "C" fn enif_realloc(ptr: *mut c_void, size: size_t) -> *mut c_void {
    // SAFETY: `realloc(NULL, n)` == `malloc(n)`; the caller guarantees the block's provenance.
    unsafe { libc::realloc(ptr, size) }
}

// ---------------------------------------------------------------------------------------------
// Binaries
// ---------------------------------------------------------------------------------------------

/// The sentinel stored in `__spare__[0]` for an *inspected* (non-owned) binary view.
const INSPECTED: *mut c_void = 1 as *mut c_void;

/// `int enif_alloc_binary(size_t size, ErlNifBinary* bin);`
///
/// # Safety
///
/// `bin` must be null or point at a writable `ErlNifBinary`.
#[no_mangle]
pub unsafe extern "C" fn enif_alloc_binary(size: size_t, bin: *mut ErlNifBinary) -> c_int {
    if bin.is_null() {
        return 0;
    }
    // SAFETY: `malloc(size)` yields a `size`-byte writable block (or null).
    let data = unsafe { libc::malloc(size) }.cast::<c_uchar>();
    if data.is_null() {
        return 0;
    }
    // SAFETY: `bin` is non-null and writable (caller contract); an owned buffer's marker is null.
    unsafe {
        bin.write(ErlNifBinary {
            size,
            data,
            ref_bin: core::ptr::null_mut(),
            __spare__: [core::ptr::null_mut(), core::ptr::null_mut()],
        });
    }
    1
}

/// `int enif_realloc_binary(ErlNifBinary* bin, size_t size);`
///
/// # Safety
///
/// `bin` must be null or point at a live `ErlNifBinary` produced by this module.
#[no_mangle]
pub unsafe extern "C" fn enif_realloc_binary(bin: *mut ErlNifBinary, size: size_t) -> c_int {
    if bin.is_null() {
        return 0;
    }
    // SAFETY: `bin` is non-null (checked) and live (caller contract).
    let b = unsafe { &mut *bin };
    if b.__spare__[0].is_null() {
        // Owned: a plain realloc keeps the contents.
        // SAFETY: the block came from `malloc`/`realloc` and is owned by this struct.
        let grown = unsafe { libc::realloc(b.data.cast::<c_void>(), size) }.cast::<c_uchar>();
        if grown.is_null() {
            return 0;
        }
        b.data = grown;
        b.size = size;
        return 1;
    }
    // Inspected: allocate a fresh owned buffer and copy what fits.
    // SAFETY: a fresh `size`-byte block.
    let fresh = unsafe { libc::malloc(size) }.cast::<c_uchar>();
    if fresh.is_null() {
        return 0;
    }
    let copy = core::cmp::min(size, b.size);
    if copy > 0 && !b.data.is_null() {
        // SAFETY: both buffers are valid for `copy` bytes (`copy <= size` and `copy <= b.size`).
        unsafe { core::ptr::copy_nonoverlapping(b.data, fresh, copy) };
    }
    b.data = fresh;
    b.size = size;
    b.__spare__[0] = core::ptr::null_mut();
    1
}

/// `void enif_release_binary(ErlNifBinary* bin);`
///
/// # Safety
///
/// `bin` must be null or point at a live `ErlNifBinary` produced by this module.
#[no_mangle]
pub unsafe extern "C" fn enif_release_binary(bin: *mut ErlNifBinary) {
    if bin.is_null() {
        return;
    }
    // SAFETY: `bin` is non-null (checked) and live (caller contract).
    let b = unsafe { &mut *bin };
    if b.__spare__[0].is_null() && !b.data.is_null() {
        // SAFETY: an owned (non-inspected) payload was `malloc`'d here; free it exactly once.
        unsafe { libc::free(b.data.cast::<c_void>()) };
    }
    b.data = core::ptr::null_mut();
    b.size = 0;
    b.__spare__[0] = core::ptr::null_mut();
}

/// `int enif_inspect_binary(ErlNifEnv*, ERL_NIF_TERM bin_term, ErlNifBinary* bin);`
///
/// # Safety
///
/// `bin` must be null or writable; `bin_term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_inspect_binary(
    _env: *mut crate::nif::types::ErlNifEnv,
    bin_term: ERL_NIF_TERM,
    bin: *mut ErlNifBinary,
) -> c_int {
    if bin.is_null() {
        return 0;
    }
    let term = t(bin_term);
    if !is_term_word(term) {
        return 0;
    }
    // SAFETY: `is_bitstring` only dereferences the header of a boxed term.
    if !unsafe { prelude::is_bitstring(term) } {
        return 0;
    }
    // SAFETY: `term` is a live bitstring (just tested).
    let bits = unsafe { prelude::bitstring_size(term) };
    if bits & 7 != 0 {
        return 0;
    }
    // SAFETY: as above; `bitstring_parts` reports `(base, bit_offset, bit_size)`.
    let (base, offset, _) = unsafe { prelude::bitstring_parts(term) };
    if offset & 7 != 0 || base.is_null() {
        // An unaligned base cannot be exposed as a byte buffer without a copy; refuse honestly.
        return 0;
    }
    // SAFETY: the payload starts at `base + offset/8` and is `bits/8` bytes long.
    let payload = unsafe { base.add(offset / 8) }.cast_mut();
    // SAFETY: `bin` is non-null and writable; `payload` borrows the live term's storage.
    unsafe {
        bin.write(ErlNifBinary {
            size: bits / 8,
            data: payload,
            ref_bin: core::ptr::null_mut(),
            __spare__: [INSPECTED as *mut c_void, core::ptr::null_mut()],
        });
    }
    1
}

/// Append the flattened bytes of the iolist **object** `obj` to `out`; `false` on a type error.
///
/// This is the C's object walk (`generic_iolist_size`, `erts/emulator/beam/erl_iolist.c:150-233`,
/// called with `Unit = 8`). An *object* — the top-level term, the tail of a list, or a list nested
/// in head position — must be a **list**, a **byte-aligned bitstring**, or **`nil`**. Anything else
/// (a bare integer, a float, an atom, a non-byte-aligned bitstring, …) is a type error and fails the
/// whole iolist. Note in particular that a **bare byte integer is NOT an iolist**: a byte is only
/// ever accepted in the *head* position of a cons cell (see [`flatten_iolist_element`]).
///
/// The tail is walked iteratively (the `while (is_list(obj))` loop), so a long proper list costs no
/// stack; only *nesting* recurses, exactly as the C's estack does not.
fn flatten_iolist(obj: Eterm, out: &mut Vec<u8>) -> bool {
    let mut obj = obj;
    loop {
        if !is_term_word(obj) {
            return false;
        }
        if prelude::is_nil(obj) {
            return true;
        }
        // SAFETY: `is_binary` only dereferences the header of a boxed term.
        if unsafe { prelude::is_binary(obj) } {
            return push_bitstring_bytes(obj, out);
        }
        if !prelude::is_list(obj) {
            return false;
        }
        // SAFETY: `obj` is a live cons cell.
        let (head, tail) = unsafe { (prelude::car(obj), prelude::cdr(obj)) };
        if !flatten_iolist_element(head, out) {
            return false;
        }
        obj = tail;
    }
}

/// Append the bytes of one list **element** — the `CAR` of a cons cell — to `out`.
///
/// The C accepts an element only in the head position (`erl_iolist.c:167-204`): a **byte** (a small
/// integer in `0..=255`), a **byte-aligned bitstring**, a **nested list**, or **`nil`**. A small
/// integer outside `0..=255` (`256`, `-1`) is a type error — it is *not* a byte merely because it is
/// small — and so is every other term.
fn flatten_iolist_element(head: Eterm, out: &mut Vec<u8>) -> bool {
    if !is_term_word(head) {
        return false;
    }
    if prelude::is_byte(head) {
        out.push(prelude::signed_value(head) as u8);
        return true;
    }
    // SAFETY: `is_binary` only dereferences the header of a boxed term.
    if unsafe { prelude::is_binary(head) } {
        return push_bitstring_bytes(head, out);
    }
    if prelude::is_nil(head) {
        return true;
    }
    if prelude::is_list(head) {
        return flatten_iolist(head, out);
    }
    false
}

/// Append the bytes of the byte-aligned bitstring `term` (`false` when its base is not byte-aligned).
fn push_bitstring_bytes(term: Eterm, out: &mut Vec<u8>) -> bool {
    // SAFETY: `term` is a live byte-aligned bitstring (the caller tested `is_binary`).
    let bits = unsafe { prelude::bitstring_size(term) };
    // SAFETY: as above; `bitstring_parts` reports `(base, bit_offset, bit_size)`.
    let (base, offset, _) = unsafe { prelude::bitstring_parts(term) };
    if offset & 7 != 0 || base.is_null() {
        // An unaligned base cannot be exposed as a byte run without a bit-shifting copy; refuse
        // honestly rather than emit shifted bytes.
        return false;
    }
    // SAFETY: the payload starts at `base + offset/8` and is `bits/8` bytes long (`is_binary`).
    let bytes = unsafe { core::slice::from_raw_parts(base.add(offset / 8), bits / 8) };
    out.extend_from_slice(bytes);
    true
}

/// `int enif_inspect_iolist_as_binary(ErlNifEnv*, ERL_NIF_TERM term, ErlNifBinary* bin);`
///
/// A flat, byte-aligned binary yields the same zero-copy view as [`enif_inspect_binary`];
/// otherwise the term is flattened into a freshly `malloc`'d (owned) buffer the caller must release.
/// The shape rules are the C's (`erl_nif.c:1385-1407` over `erl_iolist.c:150-233`): an integer is
/// only ever a **byte in head position** of a cons cell, so a bare integer, a non-byte integer
/// (`256`, `-1`), an improper-list tail, and a non-byte-aligned bitstring are all refused with `0`.
/// An empty iolist (`[]`, or any byte-less one) is accepted with a **non-NULL** dummy `data` and
/// `size == 0` — the shape `enif_inspect_iolist_as_binary` gives `nil` in the C (`erl_nif.c:1392`).
///
/// # Safety
///
/// `bin` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_inspect_iolist_as_binary(
    env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    bin: *mut ErlNifBinary,
) -> c_int {
    // SAFETY: forwards the caller's pointer contract unchanged.
    if unsafe { enif_inspect_binary(env, term, bin) } != 0 {
        return 1;
    }
    if bin.is_null() {
        return 0;
    }
    let mut bytes = Vec::new();
    if !flatten_iolist(t(term), &mut bytes) {
        return 0;
    }
    let len = bytes.len();
    // The C gives the empty iolist a *non-NULL* dummy `data` (`bin->data = (unsigned char
    // *)&bin->data;`, `erl_nif.c:1392`). Here the dummy is a one-byte **owned** block instead: the
    // pointer is non-NULL, `size` is `0`, and `enif_release_binary` stays a well-defined `free` of
    // the block this module allocated (the C's dummy is not a heap block at all).
    let alloc_bytes = if len == 0 { 1 } else { len };
    // SAFETY: a fresh writable `alloc_bytes`-byte block.
    let data = unsafe { libc::malloc(alloc_bytes) }.cast::<c_uchar>();
    if data.is_null() {
        return 0;
    }
    if len > 0 {
        // SAFETY: `data` is `len` writable bytes and `bytes` has `len` readable bytes (disjoint).
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), data, len) };
    }
    // SAFETY: `bin` is non-null and writable; the buffer is owned (marker null).
    unsafe {
        bin.write(ErlNifBinary {
            size: len,
            data,
            ref_bin: core::ptr::null_mut(),
            __spare__: [core::ptr::null_mut(), core::ptr::null_mut()],
        });
    }
    1
}

/// `unsigned char* enif_make_new_binary(ErlNifEnv*, size_t size, ERL_NIF_TERM* termp);`
///
/// Reserves an on-heap binary of `size` bytes, writes the term through `termp`, and returns a
/// writable pointer to its payload (C semantics).
///
/// # Safety
///
/// `env` must be a live env; `termp` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_make_new_binary(
    env: *mut crate::nif::types::ErlNifEnv,
    size: size_t,
    termp: *mut ERL_NIF_TERM,
) -> *mut c_uchar {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return core::ptr::null_mut();
    };
    let n_bits = size * 8;
    // SAFETY: `heap_bits_size_words` sizes the `[hdr][size][data…]` block.
    let Some(hp) = (unsafe { alloc(e, prelude::heap_bits_size_words(n_bits)) }) else {
        return core::ptr::null_mut();
    };
    let zeros = vec![0u8; size];
    // SAFETY: `hp` heads a block of `heap_bits_size_words(n_bits)` words and `zeros` holds exactly
    // the `size` payload bytes the bit count needs.
    let Ok(term) = (unsafe { prelude::write_heap_bits(hp, &zeros, n_bits) }) else {
        return core::ptr::null_mut();
    };
    if !(unsafe { write_out(termp, n(term)) }) {
        return core::ptr::null_mut();
    }
    // SAFETY: `term` is the heap binary just written, so its payload pointer is live and writable.
    unsafe { prelude::bitstring_parts(term).0.cast_mut().cast::<c_uchar>() }
}

/// `ERL_NIF_TERM enif_make_binary(ErlNifEnv* env, ErlNifBinary* bin);`
///
/// Builds an **on-heap** binary copying `bin`'s bytes, then consumes `bin` (releases an owned buffer
/// and zeroes the struct) exactly as the C does.
///
/// # Safety
///
/// `env` must be a live env; `bin` must be a live `ErlNifBinary`.
#[no_mangle]
pub unsafe extern "C" fn enif_make_binary(
    env: *mut crate::nif::types::ErlNifEnv,
    bin: *mut ErlNifBinary,
) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    if bin.is_null() {
        return bare_badarg();
    }
    // SAFETY: `bin` is non-null (checked) and live (caller contract).
    let b = unsafe { &mut *bin };
    let bytes: &[u8] = if b.data.is_null() || b.size == 0 {
        &[]
    } else {
        // SAFETY: the struct declares `size` readable bytes at `data`.
        unsafe { core::slice::from_raw_parts(b.data, b.size) }
    };
    // SAFETY: `heap_bits_size_words` sizes the block for `size * 8` bits.
    let Some(hp) = (unsafe { alloc(e, prelude::heap_bits_size_words(b.size * 8)) }) else {
        return bare_badarg();
    };
    // SAFETY: `hp` heads the reserved block and `bytes` provides the payload.
    let term = match unsafe { prelude::write_binary(hp, bytes) } {
        Ok(term) => term,
        Err(_) => return bare_badarg(),
    };
    // Consume the caller's binary (C: `enif_release_binary(bin)`).
    // SAFETY: `bin` is live and owned/inspected per its own marker.
    unsafe { enif_release_binary(bin) };
    n(term)
}

/// `ERL_NIF_TERM enif_make_sub_binary(ErlNifEnv*, ERL_NIF_TERM bin_term, size_t pos, size_t size);`
///
/// # Safety
///
/// `env` must be a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_make_sub_binary(
    env: *mut crate::nif::types::ErlNifEnv,
    bin_term: ERL_NIF_TERM,
    pos: size_t,
    size: size_t,
) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    let term = t(bin_term);
    if !is_term_word(term) {
        return n(badarg_atom(e));
    }
    // SAFETY: `is_binary` only dereferences the header of a boxed term.
    if !unsafe { prelude::is_binary(term) } {
        return n(badarg_atom(e));
    }
    // SAFETY: `term` is a live byte-aligned bitstring.
    let bits = unsafe { prelude::bitstring_size(term) };
    let total_bytes = bits / 8;
    if pos > total_bytes || size > total_bytes - pos {
        return n(badarg_atom(e));
    }
    if pos == 0 && size == total_bytes {
        return bin_term;
    }
    // SAFETY: as above — `(base, bit_offset, _)`.
    let (base, offset, _) = unsafe { prelude::bitstring_parts(term) };
    if offset & 7 != 0 || base.is_null() {
        return n(badarg_atom(e));
    }
    // SAFETY: `sub_bits_size_words()` sizes the 5-word `ErlSubBits` block.
    let Some(hp) = (unsafe { alloc(e, prelude::sub_bits_size_words()) }) else {
        return bare_badarg();
    };
    let start = (pos * 8) as usize;
    let end = ((pos + size) * 8) as usize;
    // SAFETY: `hp` heads the reserved 5-word block; the base pointer identifies the original
    // payload (kept alive by `term`'s heap), and the window is inside it by the checks above.
    let sub = unsafe {
        prelude::write_sub_bits(
            hp,
            Eterm::from_raw((base as usize) & !prelude::ERL_SUB_BITS_FLAG_MASK),
            Eterm::from_raw(start),
            Eterm::from_raw(end),
            term,
        )
    };
    n(sub)
}

/// `int enif_term_to_binary(ErlNifEnv *env, ERL_NIF_TERM term, ErlNifBinary *bin);`
///
/// # Safety
///
/// `env` must be a live env; `bin` must be writable.
#[no_mangle]
pub unsafe extern "C" fn enif_term_to_binary(
    env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    bin: *mut ErlNifBinary,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    if bin.is_null() {
        return 0;
    }
    if !is_term_word(t(term)) {
        return 0;
    }
    let Some(bytes) = e.host().term_to_binary(t(term)) else {
        return 0;
    };
    let len = bytes.len();
    // SAFETY: a fresh `len`-byte writable block (owned).
    let data = if len == 0 {
        core::ptr::null_mut()
    } else {
        let p = unsafe { libc::malloc(len) }.cast::<c_uchar>();
        if p.is_null() {
            return 0;
        }
        // SAFETY: `p` is `len` writable bytes and `bytes` `len` readable bytes (disjoint).
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), p, len) };
        p
    };
    // SAFETY: `bin` is non-null and writable; the buffer is owned.
    unsafe {
        bin.write(ErlNifBinary {
            size: len,
            data,
            ref_bin: core::ptr::null_mut(),
            __spare__: [core::ptr::null_mut(), core::ptr::null_mut()],
        });
    }
    1
}

/// `size_t enif_binary_to_term(ErlNifEnv*, const unsigned char* data, size_t sz, ERL_NIF_TERM* term, unsigned int opts);`
///
/// Returns the number of bytes consumed, or `0` on failure.
///
/// # Safety
///
/// `env` must be a live env; `data` must be `sz` readable bytes; `term` must be writable.
#[no_mangle]
pub unsafe extern "C" fn enif_binary_to_term(
    env: *mut crate::nif::types::ErlNifEnv,
    data: *const c_uchar,
    sz: size_t,
    term: *mut ERL_NIF_TERM,
    _opts: c_uint,
) -> size_t {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    if data.is_null() || term.is_null() {
        return 0;
    }
    // SAFETY: the caller declares `sz` readable bytes at `data`.
    let bytes = unsafe { core::slice::from_raw_parts(data, sz) };
    match e.host().binary_to_term(bytes) {
        Some(value) => {
            // SAFETY: `term` is non-null and writable (checked above).
            unsafe { term.write(n(value)) };
            sz
        }
        None => 0,
    }
}
