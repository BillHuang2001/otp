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
//!
//! A binary family view is therefore one of three shapes: a zero-copy *inspected* window (a
//! byte-aligned bitstring base), an *owned* fresh buffer (an `enif_alloc_binary` block, a
//! flattened iolist, or the copy [`enif_inspect_binary`] makes of a **byte-sized** bitstring
//! whose start is not byte-aligned — the C's env-owned tmp object, which this crate has no
//! env-local allocator for, so the buffer is released by [`enif_release_binary`] instead).
//!
//! # On-heap vs off-heap (refc) binaries — a documented representation deviation
//!
//! Every binary **term** this module builds is an *on-heap* `ErlHeapBits`: its payload words live in
//! the arena the host hands out through `NifHost::heap`. The oracle switches representation at
//! `ERL_ONHEAP_BINARY_LIMIT` (`beam_term::refc`: 64 bytes / 512 bits): above it the term is the
//! 8-word `[BinRef][ErlSubBits]` pair over an **off-heap refcounted payload**
//! (`erl_nif.c:1409-1421` — `enif_alloc_binary` is *always* off-heap; `erl_nif.c:1698-1757`;
//! `enif_make_new_binary` → `erts_new_bitstring_refc`, `erl_bits.c:2811-2838`).
//!
//! What stays **exact**: the payload bytes, `bin.size`, `bin.data` (non-null, at least 8-byte
//! aligned, and writable on the `enif_make_new_binary` path), the bit size, every bitstring
//! predicate, and the byte-exact window `enif_make_sub_binary` returns (it splits on
//! `ERL_ONHEAP_BITS_LIMIT` exactly as `erts_build_sub_bitstring` does). No path here yields a wrong
//! byte, a wrong size, or a term that is not a bitstring.
//!
//! What differs, and is observable through the C ABI:
//!
//! * `enif_alloc_binary` leaves `bin.ref_bin == NULL`, where the C stores its off-heap `Binary*`
//!   there. `ref_bin` is labelled "internals (avert your eyes)" in `erl_nif.h:166`, but it is a
//!   public struct field a NIF can read.
//! * A binary **above** the on-heap limit costs this crate `2 + ceil(size / 8)` on-heap words
//!   (`heap_bits_size(8 * size)`), where the oracle's refc term costs `ERL_REFC_BITS_SIZE == 8`
//!   words plus the off-heap payload — `size_object_x`'s `SUB_BITS_SUBTAG` arm counts the whole pair
//!   and never the payload (`copy.c:219-242`). So `enif_term_size` and `erts_debug:flat_size`
//!   disagree above the limit and agree at or below it: measured on OTP 29.1.1 / erts-17.1 through
//!   the public `enif_*` entries, `enif_alloc_binary(65, …)` + `enif_make_binary` and
//!   `enif_make_new_binary(65, …)` both report `8` words (`64` bytes) there and `11` words here,
//!   while a 10-byte binary reports `4` words (`32` bytes) on both sides.
//! * The payload is therefore not shared by reference: a term produced here is copied byte-wise
//!   (never refcount-shared) if it is later sent to another process.
//!
//! This is a **deviation, not a wrong result**, and it cannot be closed inside this crate. Faithful
//! representation needs an *owner for the off-heap payload handle that outlives the call*: the C
//! parks the `BinRef` on the process's MSO list and lets the next minor GC release the payload
//! (`erl_nif.c:1719-1736`, `:1737-1746`). `NifHost` exposes only the term arena — no off-heap
//! registration — and the runtime's owners are `beam_gc::OffHeap` (the per-process `MSO(p)` list,
//! whose `val` word convention is the payload's first byte, `beam_term::refc`) and
//! `beam_message::OffHeapTable` (the copy engine's source table of
//! [`beam_alloc::RefcBinary`](beam_alloc::RefcBinary) handles). Neither is reachable from a wave-0
//! declaration crate that must not depend on `beam_gc`. Writing the pair with **no** owner would
//! leak one payload per call and would leave `BinRef.val` dangling as soon as a caller-owned handle
//! died, so the on-heap shape stands until the seam exists.

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
/// Hands back an owned, writable, uninitialised `size`-byte buffer with `ref_bin == NULL`.
///
/// **Representation deviation (see the module docs).** The C allocates an *off-heap refcounted*
/// `Binary` for **every** size (`erts_bin_drv_alloc_fnf`, `erl_nif.c:1413`), stores it in
/// `bin.ref_bin` and points `bin.data` at its payload. Here the payload is a plain `malloc` block
/// and `ref_bin` stays `NULL`, so a NIF that inspects the (internal) `ref_bin` field sees a
/// difference. Everything a NIF can act on is exact: `size`, a non-null payload, its writability
/// and the ownership protocol of [`enif_realloc_binary`]/[`enif_release_binary`].
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
    if base.is_null() {
        // A null data base can be neither exposed nor copied from; refuse rather than dereference.
        return 0;
    }
    if offset & 7 == 0 {
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
        return 1;
    }
    // A *byte-sized* window whose start is not byte-aligned: the C always returns `1` here and
    // materialises the real bits with `erts_copy_bits_fwd(base, offset, bin->data, 0, size)`
    // into an env-owned tmp buffer (`erl_nif.c:1353-1362`). This crate's `NifEnv` has no
    // tmp-object list, so the copy is handed back as an **owned** buffer instead: the pointer is
    // non-NULL, `size` is the real byte count, and `__spare__[0]` stays null so
    // [`enif_release_binary`] frees it exactly once. A NIF that never releases the view leaks
    // (safe, and the same contract [`enif_inspect_iolist_as_binary`] already gives a flattened
    // iolist); the bits exposed are always the term's own, never uninitialised bytes.
    let len = bits / 8;
    let alloc_bytes = if len == 0 { 1 } else { len };
    // SAFETY: a fresh writable `alloc_bytes`-byte block.
    let data = unsafe { libc::malloc(alloc_bytes) }.cast::<c_uchar>();
    if data.is_null() {
        return 0;
    }
    if len == 0 {
        // A zero-byte window still gets the C's non-NULL `data`; zero it so no uninitialised byte
        // is even reachable (nothing is exposed at `size == 0`, but the block is then defined).
        // SAFETY: `data` is one writable byte (just allocated).
        unsafe { data.write(0) };
    } else {
        // SAFETY: `data` is writable for `len` bytes and is disjoint from the live term's storage.
        unsafe { copy_bits_fwd(term, bits, data) };
    }
    // SAFETY: `bin` is non-null and writable; the null marker records the copy as owned.
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

/// Assemble the `bits`-bit window of the bitstring `term` into the byte-aligned buffer `dst`,
/// most-significant-bit first — the C's `erts_copy_bits_fwd(base, offset, dst, 0, bits)`
/// (`erl_nif.c:1359`) behind both `enif_inspect_binary`'s unaligned case and the iolist
/// flattening (`erl_iolist.c` `erts_iolist_to_buf`).
///
/// `bits` must be a multiple of 8, `dst` writable for `bits / 8` bytes and disjoint from the
/// term's storage.
///
/// Output byte `k` is source bits `offset + 8k .. offset + 8k + 8`; when `offset` is not
/// byte-aligned those 8 bits straddle the source bytes `base[(offset + 8k) / 8]` and its
/// successor, combined as `(hi << 8 | lo) >> (8 - offset % 8)`. The successor is read **only**
/// for a non-aligned offset, where the window's last needed bit (`offset + bits - 1`) lands in
/// it — a bit `bitstring_parts` guarantees the base covers, so the read stays in range.
///
/// # Safety
///
/// `term` must be a live bitstring whose `bitstring_parts` base covers `offset + bits` bits
/// (`bits` a multiple of 8); `dst` must be non-null and writable for `bits / 8` bytes.
unsafe fn copy_bits_fwd(term: Eterm, bits: usize, dst: *mut u8) {
    // SAFETY: `term` is a live bitstring (caller contract).
    let (base, offset, _) = unsafe { prelude::bitstring_parts(term) };
    for k in 0..(bits / 8) {
        let bit = offset + k * 8;
        let r = bit & 7;
        // SAFETY: the base covers bits `offset .. offset + bits`, so byte `bit / 8` is readable.
        let hi = unsafe { *base.add(bit >> 3) } as u16;
        let byte = if r == 0 {
            hi as u8
        } else {
            // SAFETY: `r != 0`, so this output byte's last source bit is `bit + 7`, which is a bit
            // the window covers and so lies in `base[(bit / 8) + 1]` (in range; `r == 0` above
            // avoids ever touching the byte past the window).
            let lo = unsafe { *base.add((bit >> 3) + 1) } as u16;
            ((hi << 8 | lo) >> (8 - r)) as u8
        };
        // SAFETY: `k < bits / 8`, so `dst + k` is inside the caller's `bits / 8`-byte buffer.
        unsafe { dst.add(k).write(byte) };
    }
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

/// Append the bytes of the byte-sized bitstring `term` to `out` (`false` when it has no whole
/// number of bytes, or a null data base).
///
/// A byte-aligned base is copied straight out of the term's storage; a **bit-offset** window is
/// reassembled with [`copy_bits_fwd`], exactly as the C's `erts_iolist_to_buf` does
/// (`erl_iolist.c:269-305`, which copies a `head_offset`-shifted bitstring bitwise), so a
/// byte-sized bitstring at a non-byte-aligned offset contributes its real bytes rather than
/// failing the whole iolist.
fn push_bitstring_bytes(term: Eterm, out: &mut Vec<u8>) -> bool {
    // SAFETY: `term` is a live byte-aligned bitstring (the caller tested `is_binary`).
    let bits = unsafe { prelude::bitstring_size(term) };
    if bits & 7 != 0 {
        // A non-byte-sized bitstring is a type error inside an iolist (`erl_iolist.c:170-174`).
        return false;
    }
    // SAFETY: as above; `bitstring_parts` reports `(base, bit_offset, bit_size)`.
    let (base, offset, _) = unsafe { prelude::bitstring_parts(term) };
    if base.is_null() {
        return false;
    }
    let len = bits / 8;
    if offset & 7 == 0 {
        // SAFETY: the payload starts at `base + offset/8` and is `len` bytes long (`is_binary`).
        let bytes = unsafe { core::slice::from_raw_parts(base.add(offset / 8), len) };
        out.extend_from_slice(bytes);
        return true;
    }
    let start = out.len();
    out.resize(start + len, 0);
    // SAFETY: `out` now holds `len` writable bytes at `start`, disjoint from the term's storage,
    // and `bits` is a multiple of 8.
    unsafe { copy_bits_fwd(term, bits, out.as_mut_ptr().add(start)) };
    true
}

/// `int enif_inspect_iolist_as_binary(ErlNifEnv*, ERL_NIF_TERM term, ErlNifBinary* bin);`
///
/// A flat, byte-aligned binary yields the same zero-copy view as [`enif_inspect_binary`];
/// otherwise the term is flattened into a freshly `malloc`'d (owned) buffer the caller must release.
/// The shape rules are the C's (`erl_nif.c:1385-1407` over `erl_iolist.c:150-233`): an integer is
/// only ever a **byte in head position** of a cons cell, so a bare integer, a non-byte integer
/// (`256`, `-1`), an improper-list tail, and a **non-byte-sized** bitstring (`<<1:7>>`) are all
/// refused with `0`.
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
/// Reserves a binary of `size` bytes, writes the term through `termp`, and returns a writable
/// pointer to its payload (C semantics).
///
/// **Representation deviation (see the module docs).** The term is always an on-heap
/// `ErlHeapBits`, so the returned pointer addresses this crate's heap block. Above
/// `ERL_ONHEAP_BINARY_LIMIT` (64 bytes / 512 bits) the oracle instead allocates an off-heap
/// refcounted payload and returns a pointer into it, giving an 8-word `[BinRef][ErlSubBits]` term
/// (`erts_new_binary` → `erts_new_bitstring_refc`, `erl_bits.c:2811-2838`). The `size`, the
/// writability of the returned pointer and the payload's bytes are therefore identical; the term's
/// counted heap size is not (`enif_term_size`/`erts_debug:flat_size`).
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
/// Builds an **on-heap** binary copying `bin`'s bytes, then consumes `bin`: the payload is no longer
/// the caller's to free, and an owned buffer is released exactly once.
///
/// **Representation deviation (see the module docs).** At or below `ERL_ONHEAP_BINARY_LIMIT`
/// (64 bytes / 512 bits) the C also produces an on-heap bitstring of the same `2 + ceil(size / 8)`
/// words, so both sides agree. Above it the C keeps the off-heap refcounted payload and returns the
/// 8-word `[BinRef][ErlSubBits]` pair (`erl_nif.c:1737-1746`; for an inspected `bin` with a null
/// `ref_bin`, `erts_new_binary_from_data` makes the same split), while this crate writes the whole
/// payload on-heap — the same bytes and `byte_size`, but a counted heap size of
/// `2 + ceil(size / 8)` words instead of `8`.
///
/// **Consumption deviation.** The C's `enif_release_binary` clears `data`/`ref_bin` only under
/// `#ifdef DEBUG` (`erl_nif.c:1451-1461`), so a release oracle leaves the caller's `bin.size` (and
/// `bin.data`) readable after the call; this crate zeroes `size`/`data` on every path. The ownership
/// transfer — the payload is no longer the caller's to release — is identical.
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

/// The C's `ERL_ONHEAP_BITS_LIMIT` (`erl_bits.h:166-168`: 64 bytes / 512 bits) — the bit size at or
/// below which `erts_build_sub_bitstring` materialises the window as a **heap bitstring** (the bits
/// copied into a fresh block), and above which it builds an `ErlSubBits` **window** over the
/// source's own storage (`erl_bits.c:2643-2669`, with `erts_extracted_bitstring_size` sizing the
/// reservation for the same split).
const ERL_ONHEAP_BITS_LIMIT: usize = 512;

/// `ERL_NIF_TERM enif_make_sub_binary(ErlNifEnv*, ERL_NIF_TERM bin_term, size_t pos, size_t size);`
///
/// The window is bits `NBITS(pos) .. NBITS(pos) + NBITS(size)` of the source's **bit** string:
/// `pos`/`size` are byte counts, but the C adds `NBITS(pos)` to the source's *bit* offset
/// (`erl_nif.c:1776-1783`, `source_offset + NBITS(pos)`, `NBITS(size)`). Any bitstring source is
/// therefore valid: byte-aligned or not, byte-sized or not. The result is always `NBITS(size)` bits
/// — hence always byte-sized — and is built with `erts_build_sub_bitstring`'s split: a **heap
/// bitstring** (bits copied) at or below [`ERL_ONHEAP_BITS_LIMIT`] bits, an `ErlSubBits` window
/// over the source's storage above it (so a sub-window of an off-heap binary stays a window into
/// that binary rather than becoming a fresh heap blob).
///
/// `size == 0` yields the empty bitstring for **any** `pos`, including `pos == total_bytes + 1`:
/// the C's three bounds `ASSERT`s are compiled out of the release oracle, nothing is read, and
/// `<<>>` comes back (measured). A `size > 0` window running past the source's last bit, or a
/// **non-bitstring** source, raises `badarg` here: the C reads out of bounds (or faults) there, and
/// this crate refuses rather than faults (the documented policy — the oracle's
/// `enif_make_sub_binary(env, ok, 0, 1)` SIGSEGVs).
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
    // SAFETY: `is_bitstring` only dereferences the header of a boxed term.
    if !unsafe { prelude::is_bitstring(term) } {
        // The C's `ASSERT(is_bitstring(bin_term))` is compiled out; a non-bitstring would be read
        // through the bitstring macros (out of bounds / a fault).
        return n(badarg_atom(e));
    }
    // SAFETY: `term` is a live bitstring.
    let source_bits = unsafe { prelude::bitstring_size(term) };
    let (n_bits, pos_bits) = if size == 0 {
        // Nothing is read, so any `pos` is accepted (the C returns `<<>>` even for
        // `pos == total_bytes + 1`); 0 bits also keeps `pos_bits == 0` (never used).
        (0, 0)
    } else {
        let (Some(n_bits), Some(pos_bits)) = (size.checked_mul(8), pos.checked_mul(8)) else {
            return n(badarg_atom(e));
        };
        // `pos_bits + n_bits > source_bits`, written so the subtraction cannot wrap.
        if n_bits > source_bits || pos_bits > source_bits - n_bits {
            return n(badarg_atom(e));
        }
        (n_bits, pos_bits)
    };
    if n_bits <= ERL_ONHEAP_BITS_LIMIT {
        // `erts_build_sub_bitstring`'s heap arm: copy the window's bits into a fresh `ErlHeapBits`
        // (`HEAP_BITSTRING` / `copy_binary_to_buffer`, `erl_bits.h:171-176`).
        let n_bytes = prelude::bit_count_to_bytes(n_bits);
        // SAFETY: `heap_bits_size_words` sizes the `[hdr][size][data…]` block for `n_bits` bits.
        let Some(hp) = (unsafe { alloc(e, prelude::heap_bits_size_words(n_bits)) }) else {
            return bare_badarg();
        };
        let mut bytes = vec![0u8; n_bytes];
        for i in 0..n_bits {
            // SAFETY: `pos_bits + n_bits <= source_bits`, so bit `pos_bits + i` is inside the
            // source (`bit_at` reads bit `i` of the term's own bit string, source offset included).
            if unsafe { prelude::bit_at(term, pos_bits + i) } {
                bytes[i / 8] |= 1 << (7 - (i % 8));
            }
        }
        // SAFETY: `hp` heads the reserved block and `bytes` holds exactly the `ceil(n_bits / 8)`
        // payload bytes it needs (a bit count of 0 writes the header only).
        let Ok(out) = (unsafe { prelude::write_heap_bits(hp, &bytes, n_bits) }) else {
            return bare_badarg();
        };
        return n(out);
    }
    // The `ErlSubBits` arm: a window over the source's storage, at the **absolute** bit offset
    // `source_offset + NBITS(pos)` into it — which is what makes a non-byte-aligned source work.
    // SAFETY: `term` is a live bitstring; `(base, offset, _)` is its `(data base, bit offset, bit
    // size)` triple.
    let (base, offset, _) = unsafe { prelude::bitstring_parts(term) };
    if base.is_null() {
        return n(badarg_atom(e));
    }
    // SAFETY: `sub_bits_size_words()` sizes the 5-word `ErlSubBits` block.
    let Some(hp) = (unsafe { alloc(e, prelude::sub_bits_size_words()) }) else {
        return bare_badarg();
    };
    let start = offset + pos_bits;
    let end = start + n_bits;
    // SAFETY: `hp` heads the reserved 5-word block; `base` identifies the underlying storage (kept
    // alive by `term`, which is also the `orig` slot), and the window is inside that storage by the
    // bound check above (`base` is word-aligned, so no flag bit is set in the base word).
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

#[cfg(test)]
mod tests {
    //! Path-included-module-style tests for the binary family's **representation** (see the module
    //! docs): the word counts this crate produces against the oracle's, measured through the public
    //! `enif_*` entries.
    //!
    //! The oracle column is the measured `enif_term_size` (OTP 29.1.1 / erts-17.1) of the same call:
    //!
    //! | `size` | oracle `enif_make_new_binary` | oracle `enif_alloc_binary` + `enif_make_binary` |
    //! |---|---|---|
    //! | 0 | 2 words | 2 words |
    //! | 1 | 3 words | 3 words |
    //! | 10 | 4 words | 4 words |
    //! | 64 | 10 words | 10 words |
    //! | 65 | **8 words** (refc pair) | **8 words** (refc pair) |
    //! | 100 | **8 words** (refc pair) | **8 words** (refc pair) |
    //!
    //! So the two sides agree at and below `ERL_ONHEAP_BINARY_LIMIT` (`2 + ceil(size / 8)` words) and
    //! differ above it, which is the documented deviation these tests pin. The oracle's
    //! `b.ref_bin` is non-NULL after `enif_alloc_binary` for **every** size and its `b.size` is
    //! unchanged by `enif_make_binary` (release build).

    use super::*;

    use beam_alloc::Heap;
    use beam_term::atom::AtomTable;

    use crate::nif::host::NifHost;

    /// The minimal host the binary family needs: an arena plus an atom table (`badarg` interning).
    struct BinaryHost {
        heap: Heap,
        atoms: AtomTable,
    }

    impl BinaryHost {
        fn new() -> BinaryHost {
            BinaryHost {
                heap: Heap::new(4096).expect("a small test heap is allocatable"),
                atoms: AtomTable::new(),
            }
        }
    }

    impl NifHost for BinaryHost {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }

        fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
            Some(prelude::make_atom(self.atoms.intern(name)))
        }

        fn atom_name(&mut self, atom: Eterm) -> Option<String> {
            if !prelude::is_atom(atom) {
                return None;
            }
            self.atoms
                .name(prelude::atom_val(atom))
                .map(str::to_string)
        }
    }

    /// `enif_term_size` in whole words.
    fn term_words(term: ERL_NIF_TERM) -> usize {
        // SAFETY: `term` is a live term word returned by the call under test and `enif_term_size`
        // dereferences nothing for a bitstring — it walks the term's shape.
        let bytes = unsafe { enif_term_size(term) };
        bytes / core::mem::size_of::<ERL_NIF_TERM>()
    }

    /// The oracle's word count for a fresh `size`-byte binary, below the on-heap limit.
    fn on_heap_words(size: usize) -> usize {
        2 + size.div_ceil(core::mem::size_of::<ERL_NIF_TERM>())
    }

    #[test]
    fn make_new_binary_matches_the_oracle_at_or_below_the_on_heap_limit() {
        let mut host = BinaryHost::new();
        let mut env = NifEnv::new(&mut host);
        for size in [0usize, 1, 10, 64] {
            let mut term = ERL_NIF_TERM::default();
            // SAFETY: `env` is live for the whole test and `term` is a live local.
            let payload = unsafe { enif_make_new_binary(env.as_ptr(), size, &mut term) };
            assert!(!payload.is_null(), "a {size}-byte payload");
            assert_eq!(
                term_words(term),
                on_heap_words(size),
                "a {size}-byte binary is on-heap on both sides"
            );
            // The bytes handed out are the term's own, and writable.
            // SAFETY: `payload` is writable for `size` bytes (the call's contract).
            unsafe { payload.write_bytes(0xAB, size) };
            // SAFETY: `term` is a live bitstring of `size` bytes.
            assert_eq!(unsafe { prelude::bitstring_size(t(term)) }, size * 8);
        }
    }

    #[test]
    fn make_new_binary_is_on_heap_above_the_limit_where_the_oracle_wraps_it_refc() {
        let mut host = BinaryHost::new();
        let mut env = NifEnv::new(&mut host);
        for size in [65usize, 100] {
            let mut term = ERL_NIF_TERM::default();
            // SAFETY: `env` is live for the whole test and `term` is a live local.
            let payload = unsafe { enif_make_new_binary(env.as_ptr(), size, &mut term) };
            assert!(!payload.is_null(), "a {size}-byte payload");
            // The deviation: `2 + ceil(size / 8)` words here, the oracle's 8-word refc pair.
            assert_eq!(term_words(term), on_heap_words(size));
            assert!(
                term_words(term) > prelude::ERL_REFC_BITS_SIZE,
                "the on-heap shape costs more than the oracle's refc pair at {size} bytes"
            );
            // SAFETY: `term` is a live bitstring (not a `BinRef` root), so this only reads its
            // header and its `orig` slot.
            assert!(
                !unsafe { prelude::is_refc_binary(t(term)) },
                "this crate builds no refc binary (the documented deviation)"
            );
            // SAFETY: `term` is a live bitstring of `size * 8` bits.
            assert_eq!(unsafe { prelude::bitstring_size(t(term)) }, size * 8);
        }
    }

    #[test]
    fn alloc_binary_hands_back_a_null_ref_bin_and_a_writable_payload() {
        let mut host = BinaryHost::new();
        let mut env = NifEnv::new(&mut host);
        let mut bin = ErlNifBinary {
            size: 0,
            data: core::ptr::null_mut(),
            ref_bin: core::ptr::null_mut(),
            __spare__: [core::ptr::null_mut(), core::ptr::null_mut()],
        };
        // SAFETY: `bin` is a live local and takes no env.
        assert_eq!(unsafe { enif_alloc_binary(65, &mut bin) }, 1);
        assert_eq!(bin.size, 65);
        assert!(!bin.data.is_null());
        assert_eq!(
            bin.data as usize % 8,
            0,
            "the oracle's off-heap payload is 8-byte aligned; `malloc` keeps the same promise"
        );
        assert!(
            bin.ref_bin.is_null(),
            "the oracle stores its off-heap `Binary*` here (the documented deviation)"
        );
        // The payload is the caller's while the binary is owned.
        // SAFETY: `bin.data` is writable for `bin.size` bytes.
        unsafe { bin.data.write_bytes(0x5A, bin.size) };

        // `enif_make_binary` copies those bytes into an on-heap term and consumes the struct.
        // SAFETY: `env` is live and `bin` is a live owned binary.
        let term = unsafe { enif_make_binary(env.as_ptr(), &mut bin) };
        assert_eq!(term_words(term), on_heap_words(65));
        // SAFETY: `term` is a live bitstring; `bitstring_parts` reports its payload.
        let (base, offset, bits) = unsafe { prelude::bitstring_parts(t(term)) };
        assert_eq!((offset, bits), (0, 65 * 8));
        // SAFETY: the term's payload is `ceil(65 * 8 / 8)` = 65 readable bytes at `base`.
        let bytes = unsafe { core::slice::from_raw_parts(base, 65) };
        assert!(bytes.iter().all(|&b| b == 0x5A), "the payload was copied");
        assert!(bin.data.is_null(), "the struct is consumed");
    }
}
