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

//! The **host-free** half of the driver host layer — part 1 of the `driver_*` / `erl_drv_*` C ABI
//! (workspace contract **A.4**).
//!
//! Every function here is a real `#[no_mangle] pub unsafe extern "C" fn` matching one field of the
//! frozen [`ErlDrvApi`](crate::driver::api::ErlDrvApi) table, and none of them needs a port, a
//! scheduler or a runtime service: the memory allocators, the dynamic-loader quartet, the time trio,
//! the environment/errno pair, `driver_system_info` and the stateless odds and ends
//! (`null_func` / `erl_drv_consume_timeslice` / `driver_mk_term_nil`).
//!
//! The port/host-dependent half lives in [`super::port`]; the shared
//! [`driver_term_nil`](super::driver_term_nil) data symbol, the module docs and the populated table
//! live in [`super`].

use core::sync::atomic::{AtomicI64, Ordering};
use std::cell::RefCell;
use std::sync::OnceLock;

use libc::{c_char, c_int, c_void, size_t};

use crate::common::ERTS_NAPI_TIME_ERROR;
use crate::driver::api::ErlDrvApi;
use crate::driver::types::{
    ErlDrvBinary, ErlDrvPort, ErlDrvSInt, ErlDrvSizeT, ErlDrvSysInfo, ErlDrvTermData, ErlDrvTime,
    ErlDrvTimeUnit,
};
use crate::{
    ERL_DRV_EXTENDED_MAJOR_VERSION, ERL_DRV_EXTENDED_MINOR_VERSION, ERL_NIF_MAJOR_VERSION,
    ERL_NIF_MINOR_VERSION,
};

use super::driver_term_nil;

// ---------------------------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------------------------

/// `void *driver_alloc(ErlDrvSizeT size);` — allocate driver memory.
///
/// # Safety
///
/// Release the result with [`driver_free`]/[`driver_realloc`], never the Rust allocator. A zero
/// `size` is permitted.
#[no_mangle]
pub unsafe extern "C" fn driver_alloc(size: ErlDrvSizeT) -> *mut c_void {
    // SAFETY: `malloc` is unconditional; a zero size is permitted and may return null.
    unsafe { libc::malloc(size) }
}

/// `void driver_free(void *ptr);` — free driver-allocated memory.
///
/// # Safety
///
/// `ptr` must be null or a live block from [`driver_alloc`]/[`driver_realloc`].
#[no_mangle]
pub unsafe extern "C" fn driver_free(ptr: *mut c_void) {
    // SAFETY: the caller contract above; `free(null)` is a no-op.
    unsafe { libc::free(ptr) };
}

/// `void *driver_realloc(void *ptr, ErlDrvSizeT size);` — resize driver memory.
///
/// # Safety
///
/// `ptr` must be null or a live block from [`driver_alloc`]/[`driver_realloc`].
#[no_mangle]
pub unsafe extern "C" fn driver_realloc(ptr: *mut c_void, size: ErlDrvSizeT) -> *mut c_void {
    // SAFETY: the caller contract above; `realloc(null, n)` behaves like `malloc(n)`.
    unsafe { libc::realloc(ptr, size) }
}

// ---------------------------------------------------------------------------------------------
// Driver binaries: the hidden reference-count header
// ---------------------------------------------------------------------------------------------

/// The reference count a freshly allocated driver binary starts with —
/// `erts_refc_init(&res->intern.refc, 1)` (`erts/emulator/beam/erl_binary.h:278`). Pinned against
/// the real ERTS (OTP 29.1.1 / erts-17.1) by `tests/driver_binary_refc.rs`, which observes
/// `driver_binary_get_refc(driver_alloc_binary(8)) == 1`.
const INITIAL_REFC: i64 = 1;

/// The value this seam answers for a **null** binary operand, where the C would dereference it.
///
/// `driver_binary_get_refc`/`inc_refc`/`dec_refc` (`io.c:6536-6558`) convert their operand with
/// `ErlDrvBinary2Binary` and dereference the result unconditionally, so `NULL` is a crash on the
/// real ERTS (it cannot be probed — it would take the emulator down). This crate's policy is
/// "refuse rather than fault, never a wrong result" (`CONTEXT.md` §3), so a null operand is
/// answered with the same `-1` refusal the rest of the `driver_*` layer uses
/// (`driver_enq`/`driver_pushq`/…). It is a deliberate, documented divergence: `-1` is also a
/// value a real `dec_refc` can return, so no driver can mistake the refusal for a live count —
/// it can only *not* be told apart from an over-decremented binary, which is already undefined
/// behaviour in the C.
const REFC_REFUSED: ErlDrvSInt = -1;

/// The hidden, **non-public** reference-count header that precedes the public [`ErlDrvBinary`] in
/// every block [`driver_alloc_binary`] hands out — this crate's analogue of the emulator's private
/// `Binary` header.
///
/// The C keeps the count in an emulator-internal `Binary` *allocated before* the pointer a driver
/// ever sees, and converts between the two by pointer arithmetic
/// (`Binary2ErlDrvBinary(B) = &((ErtsBinary *) B)->driver.binary`,
/// `ErlDrvBinary2Binary(D) = (Binary *) ((char *) D - offsetof(ErtsBinary, driver.binary))`;
/// `erts/emulator/beam/erl_binary.h:173-176`). This type is the minimum such a header needs: the
/// C's `flags`/`apparent_size` words are internals no driver can observe through this ABI.
///
/// Layout of one allocated block (the header first, then the public struct verbatim):
///
/// ```text
/// +0   refc: AtomicI64        <- BinaryHeader (8 bytes, 8-aligned)
/// +8   orig_size: ErlDrvSInt  <- ErlDrvBinary.orig_size  == the pointer that is returned
/// +16  orig_bytes[0..size]    <- ErlDrvBinary.orig_bytes
/// ```
///
/// `malloc` returns 16-byte-aligned storage, so `orig_size` (offset 8) and `orig_bytes`
/// (offset 16) are both 8-byte aligned. The first is what the frozen [`ErlDrvBinary`] layout
/// requires; the second is the C's own `ERTS_CHK_BIN_ALIGNMENT` invariant (*"Address of
/// orig_bytes[0] of a Binary should always be 8-byte aligned"*, `erl_binary.h:216`). Keeping the
/// payload 8-aligned is why the header is a whole word rather than a byte.
///
/// The count is atomic because the C's is an `erts_atomic_t` (`sys.h:964`) and the trio is
/// documented for use from driver threads. Like the C's, the count never releases the block:
/// only [`driver_free_binary`] does (`erts_bin_release` -> `erts_bin_free`,
/// `erl_binary.h:399-405`).
#[repr(C)]
struct BinaryHeader {
    /// The reference count; [`INITIAL_REFC`] on allocation.
    refc: AtomicI64,
}

/// The block prefix a driver binary needs before its payload: the hidden header plus the public
/// `orig_size` word. `offset_of!(ErlDrvBinary, orig_bytes) == 8` on LP64, so this is 16 bytes — the
/// exact distance ERTS's `Binary2ErlDrvBinary`/`ErlDrvBinary2Binary` pair implies.
const DRIVER_BINARY_PREFIX: ErlDrvSizeT =
    core::mem::size_of::<BinaryHeader>() + core::mem::offset_of!(ErlDrvBinary, orig_bytes);

/// The bytes a driver binary of `size` payload bytes needs: the hidden header, the `orig_size`
/// word and the `size`-byte `orig_bytes` array.
///
/// `None` when the total would overflow `ErlDrvSizeT`, mirroring the C's `IS_BINARY_SIZE_OK` guard
/// (`erl_binary.h:244`) which refuses an unstorable size with `NULL` instead of truncating it.
#[inline]
const fn driver_binary_block_size(size: ErlDrvSizeT) -> Option<ErlDrvSizeT> {
    size.checked_add(DRIVER_BINARY_PREFIX)
}

/// The block base behind a public `ErlDrvBinary *` — `ErlDrvBinary2Binary`'s pointer arithmetic.
///
/// # Safety
///
/// `bin` must be a non-null pointer returned by [`driver_alloc_binary`]/[`driver_realloc_binary`]
/// (every caller checks for null first, since the subtraction itself requires a real block).
unsafe fn binary_header(bin: *mut ErlDrvBinary) -> *mut BinaryHeader {
    // SAFETY: the caller's contract above; the header sits `size_of::<BinaryHeader>()` bytes before
    // the public struct in the same allocation.
    unsafe { (bin as *mut u8).sub(core::mem::size_of::<BinaryHeader>()) as *mut BinaryHeader }
}

/// `ErlDrvBinary *driver_alloc_binary(ErlDrvSizeT size);` — allocate a driver binary: a hidden
/// reference-count header, then the public `orig_size` word and a `size`-byte `orig_bytes` payload.
/// A zero `size` still yields a valid (payload-less) block; an unstorable size yields `NULL`.
///
/// The returned pointer is the public [`ErlDrvBinary`] (i.e. its `orig_size` field), so the frozen
/// struct layout, [`super::binary_payload`] and every existing caller are unaffected by the header.
///
/// # Safety
///
/// Release the result with [`driver_free_binary`]/[`driver_realloc_binary`].
#[no_mangle]
pub unsafe extern "C" fn driver_alloc_binary(size: ErlDrvSizeT) -> *mut ErlDrvBinary {
    let Some(total) = driver_binary_block_size(size) else {
        return core::ptr::null_mut();
    };
    // SAFETY: `malloc` is unconditional; the block is always large enough for the header (>= 8).
    let base = unsafe { libc::malloc(total) } as *mut u8;
    if base.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: `base` heads a live `total`-byte block whose first word is the header; `ptr::write`
    // initialises that word without reading the (uninitialised) bytes it replaces. `base + 8` is
    // the public head, in bounds because `total >= 16`, and 8-byte aligned because `malloc` is
    // 16-byte aligned.
    unsafe {
        core::ptr::write(
            base as *mut BinaryHeader,
            BinaryHeader {
                refc: AtomicI64::new(INITIAL_REFC),
            },
        );
        let bin = base.add(core::mem::size_of::<BinaryHeader>()) as *mut ErlDrvBinary;
        (*bin).orig_size = size as ErlDrvSInt;
        bin
    }
}

/// `ErlDrvBinary *driver_realloc_binary(ErlDrvBinary *bin, ErlDrvSizeT size);` — resize a driver
/// binary, updating `orig_size`. The first `min(old, new)` payload bytes are preserved (`realloc`
/// semantics) and the hidden reference count is carried across the move (it is the first word of
/// the block, which `realloc` always preserves); a null `bin` is a fresh allocation, as in the C.
///
/// # Safety
///
/// `bin` must be null or a live block from [`driver_alloc_binary`]/[`driver_realloc_binary`].
#[no_mangle]
pub unsafe extern "C" fn driver_realloc_binary(
    bin: *mut ErlDrvBinary,
    size: ErlDrvSizeT,
) -> *mut ErlDrvBinary {
    if bin.is_null() {
        // SAFETY: `driver_alloc_binary` only needs a size (the C does the same, `io.c:6583`).
        return unsafe { driver_alloc_binary(size) };
    }
    let Some(total) = driver_binary_block_size(size) else {
        return core::ptr::null_mut();
    };
    // SAFETY: the caller contract above.
    let base = unsafe { binary_header(bin) };
    // SAFETY: `base` is the block base of a live driver binary of `total` requested bytes; the
    // header's reference count is at offset 0, so `realloc`'s "first min(old,new) bytes survive"
    // rule carries it over unchanged.
    let moved = unsafe { libc::realloc(base as *mut c_void, total) } as *mut u8;
    if moved.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: `moved` heads a live block of `total >= 16` bytes.
    unsafe {
        let newbin = moved.add(core::mem::size_of::<BinaryHeader>()) as *mut ErlDrvBinary;
        (*newbin).orig_size = size as ErlDrvSInt;
        newbin
    }
}

/// `void driver_free_binary(ErlDrvBinary *bin);` — release one reference to a driver binary.
///
/// This is the C's `erts_bin_release` (`io.c:6595`): decrement the hidden count, and free the whole
/// block **only** when that decrement reaches zero. A null `bin` is a no-op (the C's
/// `if (!dbin) return;`, `io.c:6598`).
///
/// # Safety
///
/// `bin` must be null or a live block from [`driver_alloc_binary`]/[`driver_realloc_binary`] whose
/// final reference this call releases.
#[no_mangle]
pub unsafe extern "C" fn driver_free_binary(bin: *mut ErlDrvBinary) {
    if bin.is_null() {
        return;
    }
    // SAFETY: `bin` is non-null, so it is a live block from the allocator above; `binary_header`
    // recovers the header word that `driver_alloc_binary` wrote.
    let header = unsafe { binary_header(bin) };
    // SAFETY: `header` is the live header of a binary this crate allocated. `fetch_sub` returns the
    // *previous* count, so `prev == 1` is exactly the C's `erts_refc_dectest(&bp->intern.refc, 0)
    // == 0` (the count is now 0).
    let prev = unsafe { (*header).refc.fetch_sub(1, Ordering::AcqRel) };
    if prev == 1 {
        // SAFETY: the count reached zero, so this was the last reference and nothing can observe
        // the block afterwards (`AcqRel` orders that against every earlier reader).
        unsafe { libc::free(header as *mut c_void) };
    }
}

// ---------------------------------------------------------------------------------------------
// The binary reference-count trio
// ---------------------------------------------------------------------------------------------

/// `ErlDrvSInt driver_binary_get_refc(ErlDrvBinary *dbp);` — read a driver binary's reference
/// count (`io.c:6536`, `erts_refc_read`).
///
/// A null `dbp` is refused with [`REFC_REFUSED`] (`-1`) instead of faulting, as documented on that
/// constant.
///
/// # Safety
///
/// `dbp` must be null or a live pointer from [`driver_alloc_binary`]/[`driver_realloc_binary`].
#[no_mangle]
pub unsafe extern "C" fn driver_binary_get_refc(dbp: *mut ErlDrvBinary) -> ErlDrvSInt {
    if dbp.is_null() {
        return REFC_REFUSED;
    }
    // SAFETY: `dbp` is non-null, so it is a live driver binary; `Acquire` makes the count (and,
    // through it, the payload a writer published before releasing) visible on this thread.
    unsafe { (*binary_header(dbp)).refc.load(Ordering::Acquire) as ErlDrvSInt }
}

/// `ErlDrvSInt driver_binary_inc_refc(ErlDrvBinary *dbp);` — take one more reference. Returns the
/// count **after** the increment (`io.c:6545`, `erts_refc_inctest` -> `erts_atomic_inc_read_nob`).
///
/// A null `dbp` is refused with [`REFC_REFUSED`] (`-1`) instead of faulting.
///
/// # Safety
///
/// `dbp` must be null or a live pointer from [`driver_alloc_binary`]/[`driver_realloc_binary`].
#[no_mangle]
pub unsafe extern "C" fn driver_binary_inc_refc(dbp: *mut ErlDrvBinary) -> ErlDrvSInt {
    if dbp.is_null() {
        return REFC_REFUSED;
    }
    // SAFETY: `dbp` is non-null, so it is a live driver binary.
    unsafe { (*binary_header(dbp)).refc.fetch_add(1, Ordering::AcqRel) + 1 }
}

/// `ErlDrvSInt driver_binary_dec_refc(ErlDrvBinary *dbp);` — drop one reference **without**
/// releasing the block, even when the count reaches zero. Returns the count **after** the
/// decrement (`io.c:6553`, `erts_refc_dectest`).
///
/// This is the C's own contract: `erl_driver.md` is explicit that
/// *"`driver_binary_dec_refc` does _not_ free the binary if the reference count reaches zero"* —
/// only [`driver_free_binary`] does. Both facts are pinned against the real ERTS by
/// `tests/driver_binary_refc.rs`.
///
/// A null `dbp` is refused with [`REFC_REFUSED`] (`-1`) instead of faulting.
///
/// # Safety
///
/// `dbp` must be null or a live pointer from [`driver_alloc_binary`]/[`driver_realloc_binary`].
#[no_mangle]
pub unsafe extern "C" fn driver_binary_dec_refc(dbp: *mut ErlDrvBinary) -> ErlDrvSInt {
    if dbp.is_null() {
        return REFC_REFUSED;
    }
    // SAFETY: `dbp` is non-null, so it is a live driver binary.
    unsafe { (*binary_header(dbp)).refc.fetch_sub(1, Ordering::AcqRel) - 1 }
}

// ---------------------------------------------------------------------------------------------
// Dynamic loading
// ---------------------------------------------------------------------------------------------

/// `void *driver_dl_open(char *path);` — open a dynamic library (`RTLD_NOW`).
///
/// # Safety
///
/// `path` must be null or NUL-terminated; release the handle with [`driver_dl_close`].
#[no_mangle]
pub unsafe extern "C" fn driver_dl_open(path: *mut c_char) -> *mut c_void {
    // SAFETY: `dlopen` only reads the NUL-terminated `path`; a null path means "the main program".
    unsafe { libc::dlopen(path as *const c_char, libc::RTLD_NOW) }
}

/// `void *driver_dl_sym(void *handle, char *name);` — look up a symbol in a dynamic library.
///
/// # Safety
///
/// `handle` must be live (from [`driver_dl_open`]); `name` must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn driver_dl_sym(handle: *mut c_void, name: *mut c_char) -> *mut c_void {
    // SAFETY: the caller contract above; `dlsym` reads the NUL-terminated `name`.
    unsafe { libc::dlsym(handle, name as *const c_char) }
}

/// `int driver_dl_close(void *handle);` — close a dynamic library.
///
/// # Safety
///
/// `handle` must be a live handle from [`driver_dl_open`], closed exactly once.
#[no_mangle]
pub unsafe extern "C" fn driver_dl_close(handle: *mut c_void) -> c_int {
    // SAFETY: the caller contract above.
    unsafe { libc::dlclose(handle) }
}

/// `char *driver_dl_error(void);` — the most recent dynamic-loader error message.
///
/// # Safety
///
/// The result is valid only until the next dynamic-loader call on this thread (`dlerror` clears the
/// error); copy it before then.
#[no_mangle]
pub unsafe extern "C" fn driver_dl_error() -> *mut c_char {
    // SAFETY: `dlerror` has no preconditions; it returns null when no error is pending.
    unsafe { libc::dlerror() }
}

// ---------------------------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------------------------

/// The ticks of `unit` in one second — the frequency table of `erts_napi_convert_time_unit`
/// (`erts/emulator/beam/erl_time_sup.c:2472-2486`). The match is total: every [`ErlDrvTimeUnit`] has
/// a frequency, and the C's `default:` arm only catches an out-of-range *integer*, which a
/// `#[repr(C)]` Rust enum cannot hold.
const fn unit_freq(unit: ErlDrvTimeUnit) -> ErlDrvTime {
    match unit {
        ErlDrvTimeUnit::Sec => 1,
        ErlDrvTimeUnit::Msec => 1_000,
        ErlDrvTimeUnit::Usec => 1_000_000,
        ErlDrvTimeUnit::Nsec => 1_000_000_000,
    }
}

/// `erts_napi_convert_time_unit` — scaling up multiplies; scaling down divides with **floor** (toward
/// −∞) semantics, so `-1` msec is `-1` sec (the C's `(val - (denom - 1)) / denom`). The arithmetic
/// wraps rather than panicking, mirroring the C's two's-complement behaviour on a pathological input.
fn convert_time_unit(val: ErlDrvTime, from: ErlDrvTimeUnit, to: ErlDrvTimeUnit) -> ErlDrvTime {
    let ffreq = unit_freq(from);
    let tfreq = unit_freq(to);
    if tfreq >= ffreq {
        return val.wrapping_mul(tfreq / ffreq);
    }
    let denom = ffreq / tfreq;
    if val >= 0 {
        val / denom
    } else {
        val.wrapping_sub(denom - 1) / denom
    }
}

/// `ErlDrvTime erl_drv_monotonic_time(ErlDrvTimeUnit time_unit);` — Erlang monotonic time, read from
/// `clock_gettime(CLOCK_MONOTONIC)` and converted to `time_unit`. A failing clock read yields the
/// `ERTS_NAPI_TIME_ERROR` sentinel rather than a fabricated timestamp.
///
/// # Safety
///
/// No preconditions: it takes no pointers and never panics.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_monotonic_time(time_unit: ErlDrvTimeUnit) -> ErlDrvTime {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a live, writable `timespec` and `CLOCK_MONOTONIC` is a valid clock id.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return ERTS_NAPI_TIME_ERROR;
    }
    let ns = (ts.tv_sec as ErlDrvTime)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(ts.tv_nsec as ErlDrvTime);
    convert_time_unit(ns, ErlDrvTimeUnit::Nsec, time_unit)
}

/// `ErlDrvTime erl_drv_time_offset(ErlDrvTimeUnit time_unit);` — the offset between Erlang monotonic
/// time and Erlang system time. Always `0` here: this runtime anchors monotonic time on the system
/// clock's epoch (a documented approximation, see the module header), so there is nothing to measure.
///
/// # Safety
///
/// No preconditions: it takes no pointers and never panics.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_time_offset(_time_unit: ErlDrvTimeUnit) -> ErlDrvTime {
    0
}

/// `ErlDrvTime erl_drv_convert_time_unit(ErlDrvTime val, ErlDrvTimeUnit from, ErlDrvTimeUnit to);`
/// — time-unit conversion.
///
/// # Safety
///
/// No preconditions: it takes no pointers and never panics.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_convert_time_unit(
    val: ErlDrvTime,
    from: ErlDrvTimeUnit,
    to: ErlDrvTimeUnit,
) -> ErlDrvTime {
    convert_time_unit(val, from, to)
}

// ---------------------------------------------------------------------------------------------
// Environment and errno
// ---------------------------------------------------------------------------------------------

/// `ERTS_UNKNOWN_ERRNO_BUF_SZ` (`erl_errno_str.c:48`) — the answer buffer's size: "big enough even
/// if errno should be 64-bit integer". It holds the `errno_<N>` form (at most `errno_` + `-` + ten
/// digits = 17 bytes), the longest name a C library can report here (`ENOTRECOVERABLE`, 15 bytes)
/// and the NUL.
const ERTS_UNKNOWN_ERRNO_BUF_SZ: usize = 30;

thread_local! {
    /// The errno-answer buffer, one per thread. The C keeps one process-wide set of strings
    /// (`errno_map` plus a never-shrinking hash table, `erl_errno_str.c:1206-1300`) whose lifetime is
    /// the runtime's; the documented contract on the answer is only "valid until the next call", so a
    /// thread-local scratch gives every caller exactly that guarantee without a lock and without
    /// allocating per call.
    static ERRNO_ID_BUF: RefCell<[c_char; ERTS_UNKNOWN_ERRNO_BUF_SZ]> =
        const { RefCell::new([0; ERTS_UNKNOWN_ERRNO_BUF_SZ]) };
}

/// `const char *strerrorname_np(int)` — the C library's errno-number → errno-*name* lookup, the
/// primitive the C's `errno_name()` falls back on under `HAVE_STRERRORNAME_NP`
/// (`erl_errno_str.c:43-45`, `:1106-1108`).
type StrerrorNameNp = unsafe extern "C" fn(c_int) -> *const c_char;

/// The one-time `dlsym` resolution of [`StrerrorNameNp`]. `None` when this C library exports no such
/// function, in which case every errno the collision table does not name answers `errno_<N>` — which
/// is the C's own behaviour when `errno_name()` returns NULL (`erl_errno_str.c:1279-1290`).
static STRERRORNAME_NP: OnceLock<Option<StrerrorNameNp>> = OnceLock::new();

/// Resolve `strerrorname_np` from the process's dynamic symbol table (cached for the process's
/// lifetime, so the hot path is a load plus a null check).
///
/// Safe to call: the `dlsym` probe has no preconditions (it is given `RTLD_DEFAULT` and a
/// NUL-terminated literal, and a symbol found under this name has the C-declared `const char *(int)`
/// signature), and the result is either `None` or a function pointer the caller only invokes with an
/// `int`.
fn strerror_name_np() -> Option<StrerrorNameNp> {
    *STRERRORNAME_NP.get_or_init(|| {
        // SAFETY: `RTLD_DEFAULT` is this platform's "global scope" pseudo-handle and the name is a
        // NUL-terminated literal, so `dlsym` reads only its own arguments.
        let sym = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"strerrorname_np".as_ptr()) };
        if sym.is_null() {
            return None;
        }
        // SAFETY: a global symbol named `strerrorname_np` *is* `const char *(*)(int)` (the glibc
        // declaration the C's configure check probes for), and a `dlsym` result is by contract
        // convertible to the type the caller knows the symbol to have. Both are one word on LP64.
        Some(unsafe { core::mem::transmute::<*mut c_void, StrerrorNameNp>(sym) })
    })
}

/// The answer for an errno value that carries more than one name — the C has always returned the
/// same spelling for such a collision, so a fixed table bypasses the C library's possibly-different
/// choice (`erl_errno_str.c:979-1108`): the switch is consulted *before* `strerrorname_np`, and each
/// of its guards is "only list this name if the two spellings are distinct values".
///
/// On this target (`EAGAIN == EWOULDBLOCK`, `EBUSY != EALREADY`, `EWOULDBLOCK != EDEADLK`) that
/// leaves `EAGAIN`, `EALREADY`, `EBUSY`, `ECONNREFUSED` and `EDEADLK`, in the C's order.
fn colliding_errno_name(error: c_int) -> Option<&'static [u8]> {
    // Written as `if`s rather than a `match`: `EAGAIN` and `EWOULDBLOCK` are the same value here, so
    // two arms could never both be reachable and one would be flagged as an unreachable pattern.
    if error == libc::EAGAIN {
        return Some(b"eagain\0");
    }
    if libc::EBUSY != libc::EALREADY && error == libc::EALREADY {
        return Some(b"ealready\0");
    }
    if error == libc::EBUSY {
        return Some(b"ebusy\0");
    }
    if error == libc::ECONNREFUSED {
        return Some(b"econnrefused\0");
    }
    if libc::EWOULDBLOCK != libc::EDEADLK && error == libc::EDEADLK {
        return Some(b"edeadlk\0");
    }
    None
}

/// Copy `bytes` into the per-thread answer buffer, NUL-terminate it, and return its address.
///
/// The address is stable for as long as the calling thread lives; a later call on the same thread
/// overwrites the text but never moves the buffer, which is the guarantee
/// [`erl_errno_id`]'s documentation makes.
fn write_errno_answer(bytes: &[u8]) -> *mut c_char {
    ERRNO_ID_BUF.with(|cell| {
        let buf: &mut [c_char] = &mut *cell.borrow_mut();
        let room = buf.len() - 1; // the last byte is always the NUL
        let n = bytes.len().min(room);
        for (dst, &src) in buf.iter_mut().zip(bytes.iter()).take(n) {
            *dst = src as c_char;
        }
        buf[n] = 0;
        buf.as_mut_ptr()
    })
}

/// Copy a C string into the per-thread answer buffer, ASCII-lowercased and NUL-terminated — the C's
/// `tolower_copy_estring` (`erl_errno_str.c:151-168`) at the point the oracle stores a
/// `strerrorname_np` answer, which is upper case at the source.
///
/// # Safety
///
/// `name` must be a NUL-terminated C string readable for the duration of this call.
unsafe fn write_lowercased(name: *const c_char) -> *mut c_char {
    let mut lower = [0u8; ERTS_UNKNOWN_ERRNO_BUF_SZ];
    let mut n = 0;
    while n < lower.len() - 1 {
        // SAFETY: `name` is NUL-terminated (caller contract), so the read stays inside its bytes.
        let c = unsafe { *name.add(n) };
        if c == 0 {
            break;
        }
        // Bit-preserving for any byte; only ASCII `A`..`Z` are folded (`'A' <= c && c <= 'Z'`).
        let b = c as u8;
        lower[n] = if b.is_ascii_uppercase() {
            b + (b'a' - b'A')
        } else {
            b
        };
        n += 1;
    }
    write_errno_answer(&lower[..n])
}

/// Render the C's unknown-errno answer — `ERTS_UNKNOWN_ERRNO_FORMAT` `"errno_%d"`
/// (`erl_errno_str.c:47`, `:203-208`) — into the per-thread answer buffer and return it.
///
/// No heap object is allocated: the digits are rendered into a stack array (at most
/// `errno_` + `-` + ten digits = 17 bytes of the 29 usable ones).
fn write_unknown_errno(error: c_int) -> *mut c_char {
    const PREFIX: &[u8] = b"errno_";
    let mut text = [0u8; ERTS_UNKNOWN_ERRNO_BUF_SZ];
    text[..PREFIX.len()].copy_from_slice(PREFIX);
    let mut n = PREFIX.len();
    if error < 0 {
        text[n] = b'-';
        n += 1;
    }
    // `unsigned_abs` keeps `i32::MIN` from overflowing; the sign is emitted separately, as `%d` does.
    let mut value = error.unsigned_abs();
    let mut digits = [0u8; 10];
    let mut count = 0;
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        value /= 10;
        count += 1;
        if value == 0 {
            break;
        }
    }
    while count > 0 {
        count -= 1;
        text[n] = digits[count];
        n += 1;
    }
    write_errno_answer(&text[..n])
}

/// `char *erl_errno_id(int error);` — the errno's **lowercase POSIX name** (`"eacces"`, `"enoent"`,
/// `"eio"`, `"einval"`) and the form `errno_<N>` for any errno the C library cannot name — including
/// `0`, which the C maps to `errno_0` because `errno_map[0]` is written with
/// `write_unknown_errno()` before its name loop starts, so `errno_name(0)` is never consulted
/// (`erl_errno_str.c:1131-1152`, `:1233-1236`).
///
/// The lookup order is the C's `errno_name()` (`erl_errno_str.c:979-1108`): the collision table
/// first (see [`colliding_errno_name`]), then the C library's `strerrorname_np`, then the
/// `errno_<N>` form. The answer is ASCII-lowercased, because the oracle lowercases the C library's
/// upper-case name on the way into its own storage (`tolower_copy_estring`).
///
/// # Safety
///
/// The returned pointer is NUL-terminated and stays valid and readable until the next
/// `erl_errno_id` call **on the same thread**; it must not be freed or mutated. A name from the
/// collision table points at static read-only storage and stays valid for the process's lifetime.
#[no_mangle]
pub unsafe extern "C" fn erl_errno_id(error: c_int) -> *mut c_char {
    if let Some(name) = colliding_errno_name(error) {
        // The table holds NUL-terminated literals; the C caller only reads them.
        return name.as_ptr() as *mut c_char;
    }
    // `0` bypasses the name lookup: glibc's `strerrorname_np(0)` answers `"0"`, while the oracle's
    // `errno_map[0]` is pre-filled with `errno_0` and never asks for a name.
    if error != 0 {
        if let Some(lookup) = strerror_name_np() {
            // SAFETY: `lookup` is the resolved `strerrorname_np`; its answer is NULL or a
            // NUL-terminated string owned by the C library.
            let name = unsafe { lookup(error) };
            if !name.is_null() {
                // SAFETY: just checked — non-null and NUL-terminated.
                return unsafe { write_lowercased(name) };
            }
        }
    }
    write_unknown_errno(error)
}

/// `int erl_drv_putenv(const char *key, char *value);` — set an environment variable: `0` on success,
/// `-1` on failure (the C's "not found" code). A null `key`/`value` is rejected without touching
/// libc.
///
/// # Safety
///
/// `key` and `value` must be NUL-terminated C strings.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_putenv(key: *const c_char, value: *mut c_char) -> c_int {
    if key.is_null() || value.is_null() {
        return -1;
    }
    // SAFETY: both are NUL-terminated C strings (caller contract); `setenv` copies them into its own
    // storage, so the caller's buffers need not outlive the call.
    if unsafe { libc::setenv(key, value as *const c_char, 1) } == 0 {
        0
    } else {
        -1
    }
}

/// `int erl_drv_getenv(const char *key, char *value, size_t *value_size);` — read an environment
/// variable.
///
/// `*value_size` is `value`'s capacity on entry. On success the value is copied NUL-terminated,
/// `*value_size` is set to its length **excluding** the NUL, and `0` is returned. The C's two other
/// codes are reproduced: `1` when the buffer is too small (`*value_size` becomes the needed length
/// **including** the NUL) and `-1` when the key is absent. A null argument is rejected with `-1`
/// rather than dereferenced.
///
/// # Safety
///
/// `key` must be NUL-terminated; `value_size` must point at a live `size_t`; `value` must be writable
/// for `*value_size` bytes.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_getenv(
    key: *const c_char,
    value: *mut c_char,
    value_size: *mut size_t,
) -> c_int {
    if key.is_null() || value.is_null() || value_size.is_null() {
        return -1;
    }
    // SAFETY: `key` is a NUL-terminated C string (caller contract).
    let found = unsafe { libc::getenv(key) };
    if found.is_null() {
        return -1;
    }
    // SAFETY: `found` is a NUL-terminated C string, so `strlen` reads only its own bytes.
    let len = unsafe { libc::strlen(found) };
    // SAFETY: `value_size` points at a live `size_t` (caller contract).
    let cap = unsafe { *value_size };
    if len + 1 > cap {
        // SAFETY: as above — report the capacity needed (value length plus the NUL).
        unsafe { *value_size = len + 1 };
        return 1;
    }
    // SAFETY: `value` is writable for `cap >= len + 1` bytes and `found` for `len + 1`; the two
    // regions cannot overlap (one is the environment, the other the caller's buffer).
    unsafe {
        core::ptr::copy_nonoverlapping(found as *const c_char, value, len + 1);
        *value_size = len;
    }
    0
}

// ---------------------------------------------------------------------------------------------
// System info and stateless odds and ends
// ---------------------------------------------------------------------------------------------

/// `ERLANG_VERSION` — the ERTS version string reported in [`ErlDrvSysInfo::erts_version`].
///
/// Mirrors [`beam_common::ERTS_VERSION`]; `driver_system_info_fills_the_version_fields` pins the two
/// together so they cannot drift.
static ERTS_VERSION_STR: &[u8] = b"17.1\0";

/// `ERLANG_OTP_RELEASE` — the OTP release string reported in [`ErlDrvSysInfo::otp_release`]. The C
/// derives it from `OTP_VERSION`'s major component; the same test pins it to
/// `beam_common::otp_version().major`.
static OTP_RELEASE_STR: &[u8] = b"30\0";

/// `void driver_system_info(ErlDrvSysInfo *sip, size_t si_size);` — fill in the driver system info.
///
/// A field group is written only when its last field lies fully inside `si_size`, mirroring
/// `driver_system_info` (`erts/emulator/beam/io.c:7444-7500`), so a short `si_size` never causes a
/// write past the caller's buffer. Values: the frozen driver/NIF ABI versions;
/// `erts_version`/`otp_release` as static NUL-terminated strings (never freed by the caller);
/// `thread_support` and `smp_support` 1 (as in the C); `async_threads` 0 and `scheduler_threads` 1
/// (no async pool, single-threaded today); and `dirty_scheduler_support` 0, so a driver that checks
/// it is told the truth instead of being allowed to schedule dirty work.
///
/// **Documented deviation:** the C calls `erts_exit(ERTS_ERROR_EXIT, …)` for a null `sip` or an
/// `si_size` shorter than the first field group; this returns without writing instead, because a
/// library function must not abort its host process.
///
/// # Safety
///
/// `sip` must be null or valid for writes over `si_size` bytes.
#[no_mangle]
pub unsafe extern "C" fn driver_system_info(sip: *mut ErlDrvSysInfo, si_size: size_t) {
    if sip.is_null() {
        return;
    }
    let int_size = core::mem::size_of::<c_int>();
    // `ERL_DRV_SYS_INFO_SIZE(field)` — the byte offset just past `field`.
    let smp_support_end = core::mem::offset_of!(ErlDrvSysInfo, smp_support) + int_size;
    let scheduler_threads_end = core::mem::offset_of!(ErlDrvSysInfo, scheduler_threads) + int_size;
    let nif_minor_version_end = core::mem::offset_of!(ErlDrvSysInfo, nif_minor_version) + int_size;
    let dirty_scheduler_support_end =
        core::mem::offset_of!(ErlDrvSysInfo, dirty_scheduler_support) + int_size;

    if si_size >= smp_support_end {
        // SAFETY: the caller guarantees `sip` is writable for `si_size >= 32` bytes, which covers
        // exactly these six fields (offsets 0..32).
        unsafe {
            (*sip).driver_major_version = ERL_DRV_EXTENDED_MAJOR_VERSION;
            (*sip).driver_minor_version = ERL_DRV_EXTENDED_MINOR_VERSION;
            (*sip).erts_version = ERTS_VERSION_STR.as_ptr() as *mut c_char;
            (*sip).otp_release = OTP_RELEASE_STR.as_ptr() as *mut c_char;
            (*sip).thread_support = 1;
            (*sip).smp_support = 1;
        }
    }
    if si_size >= scheduler_threads_end {
        // SAFETY: `si_size` covers `async_threads`/`scheduler_threads` (offsets 32..40).
        unsafe {
            (*sip).async_threads = 0;
            (*sip).scheduler_threads = 1;
        }
    }
    if si_size >= nif_minor_version_end {
        // SAFETY: `si_size` covers `nif_major_version`/`nif_minor_version` (offsets 40..48).
        unsafe {
            (*sip).nif_major_version = ERL_NIF_MAJOR_VERSION;
            (*sip).nif_minor_version = ERL_NIF_MINOR_VERSION;
        }
    }
    if si_size >= dirty_scheduler_support_end {
        // SAFETY: `si_size` covers `dirty_scheduler_support` (offset 48..52).
        unsafe { (*sip).dirty_scheduler_support = 0 };
    }
}

/// `int null_func(void);` — the header's do-nothing placeholder function (always `0`).
///
/// # Safety
///
/// No preconditions: it takes no pointers and never panics.
#[no_mangle]
pub unsafe extern "C" fn null_func() -> c_int {
    0
}

/// `int erl_drv_consume_timeslice(ErlDrvPort port, int percent);` — inform the runtime about lengthy
/// work done for the port's process. Always `0`: this runtime has no port-timeslice accounting, so
/// consuming a timeslice never forces the port's process to yield. `port` is ignored — there is no
/// scheduler to charge.
///
/// # Safety
///
/// `port` is unused; it may be any value, including null.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_consume_timeslice(_port: ErlDrvPort, _percent: c_int) -> c_int {
    0
}

/// `ErlDrvTermData driver_mk_term_nil(void);` — the driver term data for the empty list.
///
/// # Safety
///
/// No preconditions: it takes no pointers and never panics.
#[no_mangle]
pub unsafe extern "C" fn driver_mk_term_nil() -> ErlDrvTermData {
    driver_term_nil
}

// ---------------------------------------------------------------------------------------------
// Table assembly (part 1 of the frozen `ErlDrvApi`)
// ---------------------------------------------------------------------------------------------

/// Populate the **host-free** slots of the frozen `ErlDrvApi` this file implements (part 1).
///
/// Called from [`super::DRIVER_API`]'s `populate` chain, so this file — not `mod.rs` — owns the
/// `Some(…)` list for the functions it defines. Every field is `const`-assignable, so the whole
/// chain folds at compile time.
///
/// Implements: `driver_alloc`/`driver_free`/`driver_realloc`,
/// `driver_alloc_binary`/`driver_realloc_binary`/`driver_free_binary`, the binary reference-count
/// trio (`driver_binary_get_refc`/`driver_binary_inc_refc`/`driver_binary_dec_refc`),
/// `driver_system_info`, `null_func`, `erl_errno_id`, `erl_drv_consume_timeslice`, the time trio
/// (`erl_drv_monotonic_time`/`erl_drv_time_offset`/`erl_drv_convert_time_unit`), the
/// `driver_dl_open`/`driver_dl_sym`/`driver_dl_close`/`driver_dl_error` quartet,
/// `erl_drv_putenv`/`erl_drv_getenv` and `driver_mk_term_nil`.
pub(super) const fn populate(mut api: ErlDrvApi) -> ErlDrvApi {
    api.driver_alloc = Some(driver_alloc);
    api.driver_free = Some(driver_free);
    api.driver_realloc = Some(driver_realloc);
    api.driver_alloc_binary = Some(driver_alloc_binary);
    api.driver_realloc_binary = Some(driver_realloc_binary);
    api.driver_free_binary = Some(driver_free_binary);
    api.driver_binary_get_refc = Some(driver_binary_get_refc);
    api.driver_binary_inc_refc = Some(driver_binary_inc_refc);
    api.driver_binary_dec_refc = Some(driver_binary_dec_refc);
    api.driver_system_info = Some(driver_system_info);
    api.null_func = Some(null_func);
    api.erl_errno_id = Some(erl_errno_id);
    api.erl_drv_consume_timeslice = Some(erl_drv_consume_timeslice);
    api.erl_drv_monotonic_time = Some(erl_drv_monotonic_time);
    api.erl_drv_time_offset = Some(erl_drv_time_offset);
    api.erl_drv_convert_time_unit = Some(erl_drv_convert_time_unit);
    api.driver_dl_open = Some(driver_dl_open);
    api.driver_dl_sym = Some(driver_dl_sym);
    api.driver_dl_close = Some(driver_dl_close);
    api.driver_dl_error = Some(driver_dl_error);
    api.erl_drv_putenv = Some(erl_drv_putenv);
    api.erl_drv_getenv = Some(erl_drv_getenv);
    api.driver_mk_term_nil = Some(driver_mk_term_nil);
    api
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ffi::CStr;
    use core::mem::{offset_of, size_of};
    use std::ffi::CString;

    use beam_term::prelude::NIL;

    use super::super::binary_payload;

    /// An `ErlDrvSysInfo` with sentinel values, so an unwritten field is detectable.
    fn sentinel_sys_info() -> ErlDrvSysInfo {
        ErlDrvSysInfo {
            driver_major_version: -1,
            driver_minor_version: -1,
            erts_version: 1 as *mut c_char,
            otp_release: 2 as *mut c_char,
            thread_support: -1,
            smp_support: -1,
            async_threads: -1,
            scheduler_threads: -1,
            nif_major_version: -1,
            nif_minor_version: -1,
            dirty_scheduler_support: -1,
        }
    }

    #[test]
    fn driver_binary_alloc_grow_and_free() {
        // SAFETY: the driver-binary allocators are plain malloc wrappers; every use is balanced.
        let bin = unsafe { driver_alloc_binary(4) };
        assert!(!bin.is_null());
        // SAFETY: `bin` is a live driver binary of 4 payload bytes.
        unsafe { assert_eq!((*bin).orig_size, 4) };
        binary_payload(bin, 4).copy_from_slice(b"abcd");

        // Grow to 8: the original four bytes survive and the new bytes are writable.
        // SAFETY: `bin` came from `driver_alloc_binary` and is not freed before this call.
        let grown = unsafe { driver_realloc_binary(bin, 8) };
        assert!(!grown.is_null());
        // SAFETY: `grown` is a live driver binary of 8 payload bytes.
        unsafe { assert_eq!((*grown).orig_size, 8) };
        let payload = binary_payload(grown, 8);
        assert_eq!(&payload[..4], b"abcd");
        payload[4..].copy_from_slice(b"efgh");
        assert_eq!(&payload[..], b"abcdefgh");
        // SAFETY: `grown` is freed exactly once.
        unsafe { driver_free_binary(grown) };

        // A zero-size binary is still a valid, payload-less block; a null binary reallocs fresh.
        // SAFETY: as above.
        let empty = unsafe { driver_alloc_binary(0) };
        assert!(!empty.is_null());
        // SAFETY: `empty` is live; its header word is in bounds even with no payload bytes.
        unsafe {
            assert_eq!((*empty).orig_size, 0);
            driver_free_binary(empty);
        }
        // SAFETY: a null binary is the documented "fresh allocation" input.
        let fresh = unsafe { driver_realloc_binary(core::ptr::null_mut(), 3) };
        assert!(!fresh.is_null());
        // SAFETY: `fresh` is a live driver binary of 3 payload bytes.
        unsafe {
            assert_eq!((*fresh).orig_size, 3);
            driver_free_binary(fresh);
        }
    }

    /// The sequence the real system ERTS (OTP 29.1.1 / erts-17.1) reports, measured through
    /// `driver_binary_refc()`'s shared C path by `tests/driver_binary_refc.rs`:
    ///
    /// ```text
    /// get=1 inc=2 get=2 inc=3 dec=2 get=2 dec=1 get=1
    /// ```
    #[test]
    fn driver_binary_refc_trio_matches_the_measured_oracle_sequence() {
        // SAFETY: the allocator/refcount calls below are balanced: the block is allocated once and
        // released exactly once, at the single reference the driver owns.
        unsafe {
            let bin = driver_alloc_binary(8);
            assert!(!bin.is_null());
            assert_eq!(
                driver_binary_get_refc(bin),
                1,
                "a fresh driver binary starts at one reference"
            );
            assert_eq!(
                driver_binary_inc_refc(bin),
                2,
                "inc_refc returns the NEW value (the oracle reports 2)"
            );
            assert_eq!(driver_binary_get_refc(bin), 2);
            assert_eq!(driver_binary_inc_refc(bin), 3);
            assert_eq!(
                driver_binary_dec_refc(bin),
                2,
                "dec_refc returns the NEW value (the oracle reports 2)"
            );
            assert_eq!(driver_binary_get_refc(bin), 2);
            assert_eq!(driver_binary_dec_refc(bin), 1);
            assert_eq!(driver_binary_get_refc(bin), 1);
            // Back at the driver's own single reference: this releases the block.
            driver_free_binary(bin);
        }
    }

    /// The oracle's `size_before=4 get=1 inc=2 size_after=64 refc_after=2 dec=1` and
    /// `get=1 dec=0 get=0 dec=-1`: a move preserves the count, and the trio never frees at zero.
    #[test]
    fn driver_binary_refc_survives_realloc_and_never_frees_at_zero() {
        // SAFETY: `bin` is allocated once. `grown` *is* `bin` (moved), so only `grown` is ever
        // released; the second block is deliberately left with a count of -1 (the oracle's own
        // behaviour for `driver_free_binary` after the count reached zero) and leaks.
        unsafe {
            let bin = driver_alloc_binary(4);
            assert!(!bin.is_null());
            assert_eq!(driver_binary_get_refc(bin), 1);
            let grown = driver_realloc_binary(bin, 64);
            assert!(!grown.is_null());
            assert_eq!((*grown).orig_size, 64);
            assert_eq!(
                driver_binary_get_refc(grown),
                1,
                "driver_realloc_binary carries the count across the move"
            );
            // The C's `erts_refc_dectest` only decrements; `erl_driver.md` says so explicitly.
            assert_eq!(driver_binary_dec_refc(grown), 0);
            assert_eq!(
                driver_binary_get_refc(grown),
                0,
                "dec_refc reaching zero must not free the block"
            );
            assert_eq!(driver_binary_dec_refc(grown), -1);
            driver_free_binary(grown); // -1 -> -2: no release, by design (a one-block leak)
        }
    }

    /// A null operand is refused, never dereferenced (the C would fault; see [`REFC_REFUSED`]).
    #[test]
    fn driver_binary_refc_refuses_a_null_operand() {
        // SAFETY: every call below passes null, which each function checks before dereferencing.
        unsafe {
            assert_eq!(driver_binary_get_refc(core::ptr::null_mut()), REFC_REFUSED);
            assert_eq!(driver_binary_inc_refc(core::ptr::null_mut()), REFC_REFUSED);
            assert_eq!(driver_binary_dec_refc(core::ptr::null_mut()), REFC_REFUSED);
        }
    }

    /// The hidden header must not disturb the frozen public struct: the payload stays 8-byte
    /// aligned (the C's `ERTS_CHK_BIN_ALIGNMENT` invariant), the whole payload is writable, and
    /// writing it cannot touch the reference count that precedes it.
    #[test]
    fn driver_binary_payload_stays_8_byte_aligned_and_clear_of_the_header() {
        let size = 24usize;
        // SAFETY: `bin` heads a live block of `size` payload bytes; the payload writes stay inside
        // it, and `bin` is released exactly once at the end.
        unsafe {
            let bin = driver_alloc_binary(size);
            assert!(!bin.is_null());
            let payload_addr = bin as usize + offset_of!(ErlDrvBinary, orig_bytes);
            assert_eq!(
                payload_addr % 8,
                0,
                "orig_bytes must stay 8-byte aligned (the header is a whole word for this reason)"
            );
            for (i, byte) in binary_payload(bin, size).iter_mut().enumerate() {
                *byte = 0xA5 ^ i as u8;
            }
            assert_eq!(
                driver_binary_get_refc(bin),
                INITIAL_REFC,
                "writing the payload must not clobber the header"
            );
            assert_eq!((*bin).orig_size, size as ErlDrvSInt);
            driver_free_binary(bin);
        }
    }

    /// An unstorable size is refused with `NULL` (the C's `IS_BINARY_SIZE_OK` guard), never
    /// truncated into a short allocation.
    #[test]
    fn driver_alloc_binary_refuses_an_unstorable_size() {
        // SAFETY: a refused size returns null without allocating, so nothing is leaked here.
        unsafe {
            assert!(driver_alloc_binary(ErlDrvSizeT::MAX).is_null());
            assert!(driver_alloc_binary(ErlDrvSizeT::MAX - 8).is_null());
        }
    }

    #[test]
    fn driver_alloc_free_realloc_round_trip() {
        // SAFETY: the driver allocators are plain malloc wrappers; `p` is freed exactly once.
        let p = unsafe { driver_alloc(16) } as *mut u8;
        assert!(!p.is_null());
        // SAFETY: `p` is writable for 16 bytes.
        unsafe {
            core::ptr::write_bytes(p, 0xAB, 16);
            for i in 0..16 {
                assert_eq!(*p.add(i), 0xAB);
            }
        }
        // SAFETY: `p` came from `driver_alloc` and has not been freed.
        let grown = unsafe { driver_realloc(p as *mut c_void, 32) } as *mut u8;
        assert!(!grown.is_null());
        // SAFETY: `grown` is writable for 32 bytes; `realloc` carried the first 16 over.
        unsafe {
            for i in 0..16 {
                assert_eq!(*grown.add(i), 0xAB);
            }
            core::ptr::write_bytes(grown.add(16), 0xCD, 16);
            assert_eq!(*grown.add(31), 0xCD);
            driver_free(grown as *mut c_void);
            driver_free(core::ptr::null_mut()); // free(NULL) is a no-op
        }
    }

    #[test]
    fn convert_time_unit_matches_the_c_floor_arithmetic() {
        use ErlDrvTimeUnit::{Msec, Nsec, Sec, Usec};
        assert_eq!(convert_time_unit(12_345, Msec, Msec), 12_345); // same unit: identity
        assert_eq!(convert_time_unit(0, Nsec, Nsec), 0);
        assert_eq!(convert_time_unit(2, Sec, Nsec), 2_000_000_000); // up-scaling multiplies
        assert_eq!(convert_time_unit(1_500, Msec, Usec), 1_500_000);
        assert_eq!(convert_time_unit(1_999, Msec, Sec), 1); // down-scaling floors
        assert_eq!(convert_time_unit(-1, Sec, Msec), -1_000);
        // Floor (not truncate-toward-zero) for negatives: -1 ms is -1 s, not 0 s.
        assert_eq!(convert_time_unit(-1, Msec, Sec), -1);
        assert_eq!(convert_time_unit(-1_000, Msec, Sec), -1);
        assert_eq!(convert_time_unit(-1_001, Msec, Sec), -2);
        // Every unit pair is total, so the error sentinel is unreachable for a valid unit.
        for &from in &[Sec, Msec, Usec, Nsec] {
            for &to in &[Sec, Msec, Usec, Nsec] {
                assert_ne!(convert_time_unit(1, from, to), ERTS_NAPI_TIME_ERROR);
            }
        }
        // SAFETY: no preconditions. The exported C entry point forwards to the same arithmetic.
        assert_eq!(
            unsafe { erl_drv_convert_time_unit(2, Sec, Nsec) },
            convert_time_unit(2, Sec, Nsec)
        );
    }

    #[test]
    fn monotonic_time_is_monotonic_and_the_offset_is_zero() {
        // SAFETY: no preconditions.
        let ns = unsafe { erl_drv_monotonic_time(ErlDrvTimeUnit::Nsec) };
        assert_ne!(ns, ERTS_NAPI_TIME_ERROR);
        assert!(ns > 0);
        // The two reads are microseconds apart at most, so the scaled values agree within 1 ms.
        let us = unsafe { erl_drv_monotonic_time(ErlDrvTimeUnit::Usec) };
        assert!(ns.abs_diff(us.saturating_mul(1_000)) < 1_000_000);
        let later = unsafe { erl_drv_monotonic_time(ErlDrvTimeUnit::Nsec) };
        assert!(later >= ns, "monotonic time must not go backwards");
        // The approximated offset (no separate Erlang epoch here) is always 0.
        assert_eq!(unsafe { erl_drv_time_offset(ErlDrvTimeUnit::Nsec) }, 0);
        assert_eq!(unsafe { erl_drv_time_offset(ErlDrvTimeUnit::Sec) }, 0);
    }

    #[test]
    fn stateless_stubs_and_term_nil() {
        // SAFETY: no preconditions (no pointer is dereferenced).
        unsafe {
            assert_eq!(null_func(), 0);
            // No port-timeslice accounting: consuming a timeslice never forces a yield.
            assert_eq!(erl_drv_consume_timeslice(core::ptr::null_mut(), 100), 0);
            assert_eq!(driver_mk_term_nil(), driver_term_nil);
        }
        assert_eq!(driver_term_nil, NIL.raw() as ErlDrvTermData);
        // A driver binary's `orig_size` header is a full machine word on this target.
        assert_eq!(size_of::<ErlDrvSInt>(), 8);
        assert_eq!(offset_of!(ErlDrvBinary, orig_bytes), 8);
    }

    /// Read an `erl_errno_id` answer as a Rust string (the caller keeps the pointer only until the
    /// next call, which no test does).
    fn errno_id(error: c_int) -> String {
        // SAFETY: `erl_errno_id` accepts any `int` and answers a NUL-terminated string valid until
        // the next call on this thread.
        let p = unsafe { erl_errno_id(error) };
        assert!(!p.is_null(), "erl_errno_id({error}) must never answer NULL");
        // SAFETY: non-null and NUL-terminated (the documented contract).
        let bytes = unsafe { CStr::from_ptr(p) }.to_bytes();
        // The oracle's answers are pure ASCII: the lowercased POSIX name or `errno_<N>`.
        assert!(
            bytes.is_ascii() && !bytes.is_empty(),
            "erl_errno_id({error}) must answer non-empty ASCII, got {bytes:?}"
        );
        String::from_utf8(bytes.to_vec()).expect("checked ASCII above")
    }

    #[test]
    fn errno_id_answers_the_lowercase_posix_name() {
        // The oracle's names (`erl_errno_str.c`'s `errno_name()` + `tolower_copy_estring`): the C
        // library's upper-case symbol, ASCII-lowercased.
        assert_eq!(errno_id(libc::EACCES), "eacces");
        assert_eq!(errno_id(libc::ENOENT), "enoent");
        assert_eq!(errno_id(libc::EINVAL), "einval");
        // The collision table resolves a value with more than one name to the oracle's spelling:
        // `EAGAIN` and `EWOULDBLOCK` are the same integer here, and the C has always answered
        // `eagain` for it.
        assert_eq!(
            libc::EAGAIN,
            libc::EWOULDBLOCK,
            "the premise of the collision case"
        );
        assert_eq!(errno_id(libc::EAGAIN), "eagain");
        assert_eq!(errno_id(libc::EWOULDBLOCK), "eagain");
        assert_eq!(errno_id(libc::EBUSY), "ebusy");
        assert_eq!(errno_id(libc::ECONNREFUSED), "econnrefused");
        assert_eq!(errno_id(libc::EDEADLK), "edeadlk");
        assert_eq!(errno_id(libc::EALREADY), "ealready");
        // Nothing resembling `strerror` prose may survive: no spaces, no upper case.
        for error in [libc::EACCES, libc::ENOENT, libc::EINVAL, libc::EAGAIN] {
            let answer = errno_id(error);
            assert!(
                answer
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
                "erl_errno_id({error}) = {answer:?} must be a bare lowercase name"
            );
        }
    }

    #[test]
    fn errno_id_answers_errno_n_for_anything_unnamed() {
        // `0` is the C's `errno_map[0]`: written with `write_unknown_errno()` before the name loop,
        // so it never reaches the lookup (`erl_errno_str.c:1146-1152`).
        assert_eq!(errno_id(0), "errno_0");
        // An errno this C library cannot name — the oracle's measured answer for `9999`.
        assert_eq!(errno_id(9999), "errno_9999");
        // Negative values take the same route (OTP's own `driver_SUITE_data/erl_errno_id_drv.c`
        // pins `-4711` and `-1`).
        assert_eq!(errno_id(-4711), "errno_-4711");
        assert_eq!(errno_id(-1), "errno_-1");
        // The extreme still fits the buffer and prints in full.
        assert_eq!(errno_id(c_int::MIN), format!("errno_{}", c_int::MIN));
        assert_eq!(errno_id(c_int::MAX), format!("errno_{}", c_int::MAX));
    }

    #[test]
    fn errno_id_answers_stay_readable_until_the_next_call() {
        // The documented `*mut c_char` contract: NUL-terminated, and the text is unchanged while no
        // further call happens — both for a name the collision table owns (static storage, valid for
        // the process's lifetime) and for one rendered into the per-thread answer buffer.
        // SAFETY: any `int` is accepted; each pointer is valid until the next call.
        let static_name = unsafe { erl_errno_id(libc::EAGAIN) };
        // SAFETY: as above.
        let unknown = unsafe { erl_errno_id(9999) };
        assert_ne!(
            static_name, unknown,
            "a table answer lives in static storage, not in the rendered buffer"
        );
        // SAFETY: neither answer has been overwritten — no `erl_errno_id` call happened since.
        assert_eq!(unsafe { CStr::from_ptr(static_name) }.to_bytes(), b"eagain");
        // SAFETY: `unknown` is still valid — the read above is not a call.
        assert_eq!(unsafe { CStr::from_ptr(unknown) }.to_bytes(), b"errno_9999");
        // A further rendered answer overwrites the per-thread buffer's text but never moves it, so
        // the pointer stays a valid, readable answer.
        // SAFETY: as above.
        let reused = unsafe { erl_errno_id(0) };
        assert_eq!(
            reused, unknown,
            "the per-thread answer buffer must not move between calls"
        );
        // SAFETY: `reused` is the current answer.
        assert_eq!(unsafe { CStr::from_ptr(reused) }.to_bytes(), b"errno_0");
        // SAFETY: the static answer is untouched by any of the calls above.
        assert_eq!(unsafe { CStr::from_ptr(static_name) }.to_bytes(), b"eagain");
    }

    #[test]
    fn putenv_getenv_round_trip_and_edge_cases() {
        let key = CString::new("BEAM_ABI_DRV_TEST_ENV").expect("no interior NUL");
        let value = CString::new("hello").expect("no interior NUL");
        // SAFETY: both are NUL-terminated C strings.
        assert_eq!(
            unsafe { erl_drv_putenv(key.as_ptr(), value.as_ptr() as *mut c_char) },
            0
        );

        let mut buf = [0 as c_char; 64];
        let mut size: size_t = buf.len();
        // SAFETY: `buf` is `size` writable bytes and `size` is a live `size_t`.
        assert_eq!(
            unsafe { erl_drv_getenv(key.as_ptr(), buf.as_mut_ptr(), &mut size) },
            0
        );
        assert_eq!(size, 5, "size excludes the NUL");
        // SAFETY: `erl_drv_getenv` NUL-terminated the buffer on success.
        assert_eq!(
            unsafe { CStr::from_ptr(buf.as_ptr()) }
                .to_str()
                .expect("ascii value"),
            "hello"
        );

        // A too-small buffer: code `1` and the required size (length + NUL).
        let mut small = [0 as c_char; 3];
        let mut small_size: size_t = small.len();
        assert_eq!(
            unsafe { erl_drv_getenv(key.as_ptr(), small.as_mut_ptr(), &mut small_size) },
            1
        );
        assert_eq!(small_size, 6);

        // An absent key: the C's "not found" code `-1`.
        let missing = CString::new("BEAM_ABI_DRV_TEST_ENV_ABSENT").expect("no interior NUL");
        let mut missing_size: size_t = buf.len();
        assert_eq!(
            unsafe { erl_drv_getenv(missing.as_ptr(), buf.as_mut_ptr(), &mut missing_size) },
            -1
        );

        // SAFETY: every pointer is null, which the functions check before use.
        unsafe {
            assert_eq!(
                erl_drv_getenv(core::ptr::null(), core::ptr::null_mut(), core::ptr::null_mut()),
                -1
            );
            assert_eq!(erl_drv_putenv(core::ptr::null(), value.as_ptr() as *mut c_char), -1);
        }
    }

    #[test]
    fn dl_helpers_open_the_main_program_and_report_failures() {
        // SAFETY: a null path makes `dlopen` return a handle to the main program.
        let handle = unsafe { driver_dl_open(core::ptr::null_mut()) };
        assert!(!handle.is_null(), "dlopen(NULL) failed");
        let name = CString::new("driver_alloc").expect("no interior NUL");
        // SAFETY: `handle` is live and `name` is NUL-terminated.
        let sym = unsafe { driver_dl_sym(handle, name.as_ptr() as *mut c_char) };
        if sym.is_null() {
            // `driver_alloc` reaches the dynamic symbol table only when the test binary is linked
            // with `-Wl,--export-dynamic` (`build.rs`); report loudly rather than fail spuriously.
            eprintln!("SKIP: `driver_alloc` not exported from this binary's dynamic symbol table");
        } else {
            assert_eq!(
                sym as usize,
                driver_alloc as *const () as usize,
                "dlsym found the wrong address"
            );
        }
        // SAFETY: `handle` came from `driver_dl_open` and is closed exactly once.
        assert_eq!(unsafe { driver_dl_close(handle) }, 0);

        // A missing library fails cleanly with a loader error message.
        let missing = CString::new("/nonexistent/beam_abi_no_such_library.so").expect("no NUL");
        // SAFETY: `missing` is a NUL-terminated C string.
        assert!(unsafe { driver_dl_open(missing.as_ptr() as *mut c_char) }.is_null());
        // SAFETY: `dlerror` has no preconditions and returns null when no error is pending.
        let err = unsafe { driver_dl_error() };
        assert!(!err.is_null(), "a failed dlopen must leave an error for dlerror");
        // SAFETY: `err` is a NUL-terminated message owned by the dynamic loader.
        assert!(!unsafe { CStr::from_ptr(err) }.to_bytes().is_empty());
    }

    #[test]
    fn driver_system_info_fills_the_version_fields() {
        let mut info = sentinel_sys_info();
        // SAFETY: `info` is a live, fully sized `ErlDrvSysInfo`.
        unsafe { driver_system_info(&mut info as *mut ErlDrvSysInfo, size_of::<ErlDrvSysInfo>()) };

        assert_eq!(info.driver_major_version, ERL_DRV_EXTENDED_MAJOR_VERSION);
        assert_eq!(info.driver_minor_version, ERL_DRV_EXTENDED_MINOR_VERSION);
        assert_eq!((info.thread_support, info.smp_support), (1, 1));
        assert_eq!(
            (info.nif_major_version, info.nif_minor_version),
            (ERL_NIF_MAJOR_VERSION, ERL_NIF_MINOR_VERSION)
        );
        assert_eq!((info.async_threads, info.scheduler_threads), (0, 1));
        assert_eq!(info.dirty_scheduler_support, 0);

        assert!(!info.erts_version.is_null() && !info.otp_release.is_null());
        // SAFETY: both are non-null, NUL-terminated static strings owned by this crate.
        unsafe {
            assert_eq!(
                CStr::from_ptr(info.erts_version).to_str().expect("ascii"),
                beam_common::ERTS_VERSION
            );
            assert_eq!(
                CStr::from_ptr(info.otp_release).to_str().expect("ascii"),
                beam_common::otp_version().major.to_string()
            );
        }
    }

    #[test]
    fn driver_system_info_honours_si_size_and_ignores_bad_input() {
        const N: usize = size_of::<ErlDrvSysInfo>();
        let smp_end = offset_of!(ErlDrvSysInfo, smp_support) + size_of::<c_int>();
        let nif_end = offset_of!(ErlDrvSysInfo, nif_minor_version) + size_of::<c_int>();

        // A buffer painted with `0xFF`: only the first `si_size` bytes may be touched.
        let mut buf = [0xFFu8; N];
        // SAFETY: the buffer is `N` bytes, which is at least `smp_end`.
        unsafe { driver_system_info(buf.as_mut_ptr() as *mut ErlDrvSysInfo, smp_end) };
        assert_eq!(
            c_int::from_ne_bytes(buf[0..4].try_into().expect("4 bytes")),
            ERL_DRV_EXTENDED_MAJOR_VERSION
        );
        assert_eq!(
            c_int::from_ne_bytes(buf[24..28].try_into().expect("4 bytes")),
            1,
            "thread_support lives at offset 24"
        );
        assert!(
            buf[smp_end..].iter().all(|&b| b == 0xFF),
            "driver_system_info wrote past si_size"
        );

        // A size covering the NIF group also fills it, still without overrunning.
        let mut buf = [0xFFu8; N];
        // SAFETY: the buffer is `N` bytes, which is at least `nif_end`.
        unsafe { driver_system_info(buf.as_mut_ptr() as *mut ErlDrvSysInfo, nif_end) };
        assert_eq!(
            c_int::from_ne_bytes(buf[40..44].try_into().expect("4 bytes")),
            ERL_NIF_MAJOR_VERSION
        );
        assert_eq!(
            c_int::from_ne_bytes(buf[44..48].try_into().expect("4 bytes")),
            ERL_NIF_MINOR_VERSION
        );
        assert!(buf[nif_end..].iter().all(|&b| b == 0xFF));

        // Below the first group's end, nothing at all is written.
        let mut buf = [0xFFu8; N];
        // SAFETY: the buffer is `N` bytes; `si_size` is smaller still.
        unsafe { driver_system_info(buf.as_mut_ptr() as *mut ErlDrvSysInfo, 4) };
        assert!(buf.iter().all(|&b| b == 0xFF), "a short si_size wrote data");

        // SAFETY: a null `sip` is checked before any write.
        unsafe { driver_system_info(core::ptr::null_mut(), 0) };
        // An `si_size` larger than the struct must not make the function write beyond it.
        let mut info = sentinel_sys_info();
        // SAFETY: `info` is a live, fully sized `ErlDrvSysInfo`.
        unsafe {
            driver_system_info(&mut info as *mut ErlDrvSysInfo, N + 64);
        }
        assert_eq!(info.driver_major_version, ERL_DRV_EXTENDED_MAJOR_VERSION);
        assert_eq!(info.dirty_scheduler_support, 0);
    }
}
