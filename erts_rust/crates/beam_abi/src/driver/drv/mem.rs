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

/// The bytes a driver binary of `size` payload bytes needs: the `orig_size` word plus the flexible
/// `orig_bytes` array (`offset_of!(ErlDrvBinary, orig_bytes) == 8` on LP64).
#[inline]
const fn driver_binary_block_size(size: ErlDrvSizeT) -> ErlDrvSizeT {
    core::mem::offset_of!(ErlDrvBinary, orig_bytes) + size
}

/// `ErlDrvBinary *driver_alloc_binary(ErlDrvSizeT size);` — allocate a driver binary: an `orig_size`
/// header followed by a `size`-byte `orig_bytes` payload. A zero `size` still yields a valid
/// (payload-less) block.
///
/// # Safety
///
/// Release the result with [`driver_free_binary`]/[`driver_realloc_binary`].
#[no_mangle]
pub unsafe extern "C" fn driver_alloc_binary(size: ErlDrvSizeT) -> *mut ErlDrvBinary {
    // SAFETY: `malloc` is unconditional; the block is always large enough for the header (>= 8).
    let mem = unsafe { libc::malloc(driver_binary_block_size(size)) } as *mut ErlDrvBinary;
    if mem.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: `mem` heads a live block of at least `offset_of!(orig_bytes)` bytes, so the
    // `orig_size` field (offset 0) is in bounds and aligned by `malloc`.
    unsafe { (*mem).orig_size = size as ErlDrvSInt };
    mem
}

/// `ErlDrvBinary *driver_realloc_binary(ErlDrvBinary *bin, ErlDrvSizeT size);` — resize a driver
/// binary, updating `orig_size`. The first `min(old, new)` payload bytes are preserved (`realloc`
/// semantics); a null `bin` is a fresh allocation, as in the C.
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
        // SAFETY: `driver_alloc_binary` only needs a size.
        return unsafe { driver_alloc_binary(size) };
    }
    // SAFETY: the caller contract above.
    let mem = unsafe { libc::realloc(bin as *mut c_void, driver_binary_block_size(size)) }
        as *mut ErlDrvBinary;
    if mem.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: `mem` heads a live block of at least `offset_of!(orig_bytes)` bytes.
    unsafe { (*mem).orig_size = size as ErlDrvSInt };
    mem
}

/// `void driver_free_binary(ErlDrvBinary *bin);` — free a driver binary.
///
/// # Safety
///
/// `bin` must be null or a live block from [`driver_alloc_binary`]/[`driver_realloc_binary`].
#[no_mangle]
pub unsafe extern "C" fn driver_free_binary(bin: *mut ErlDrvBinary) {
    // SAFETY: the caller contract above; `free(null)` is a no-op.
    unsafe { libc::free(bin as *mut c_void) };
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

/// `char *erl_errno_id(int error);` — plain-text text for an errno value.
///
/// **Documented deviation:** the oracle returns the lowercase POSIX errno *name* (`"enoent"`); this
/// returns libc `strerror`'s descriptive message (`"No such file or directory"`).
///
/// # Safety
///
/// The result is owned by libc's static message buffer; do not free or mutate it.
#[no_mangle]
pub unsafe extern "C" fn erl_errno_id(error: c_int) -> *mut c_char {
    // SAFETY: `strerror` accepts any `int`; the returned pointer is valid until the next call.
    unsafe { libc::strerror(error) }
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
/// `driver_alloc_binary`/`driver_realloc_binary`/`driver_free_binary`, `driver_system_info`,
/// `null_func`, `erl_errno_id`, `erl_drv_consume_timeslice`, the time trio
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

    #[test]
    fn errno_id_returns_a_non_empty_message() {
        // SAFETY: `strerror` accepts any `int` and returns a NUL-terminated static string.
        let p = unsafe { erl_errno_id(libc::ENOENT) };
        assert!(!p.is_null());
        // SAFETY: non-null and NUL-terminated (libc static message buffer).
        let s = unsafe { CStr::from_ptr(p) }.to_str().expect("ascii message");
        assert!(!s.is_empty());
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
