//! The `driver_*` / `erl_drv_*` host functions — the Rust side of the driver C ABI (workspace
//! contract **A.4**).
//!
//! Every function here is a real `#[no_mangle] pub unsafe extern "C" fn` whose signature matches the
//! corresponding field of the frozen [`ErlDrvApi`] table **exactly** (see [`crate::driver::api`]).
//!
//! [`driver_api_table`] exposes the `#[used] static DRIVER_API` — the runtime's `driver_*` surface as
//! a populated table. The `#[used]` static is what keeps the `#[no_mangle]` symbols in the linkage of
//! a binary that pulls this crate in (a driver `.so` resolves them through `-Wl,--export-dynamic`),
//! exactly as [`crate::nif::enif::enif_api_table`] does for the NIF half.
//!
//! # File size
//!
//! This file is legitimately long: it *is* the driver C ABI's host layer — one `#[no_mangle]`
//! function per implemented [`ErlDrvApi`] slot (the counterpart of `../nif/enif.rs` on the NIF
//! side), the `#[used]` table that names them, and their tests. It stays one file because every
//! slot's signature is checked against the one frozen table.
//!
//! # Scope: part 1 (host-free) and part 2 (port/host-dependent)
//!
//! **Part 1 — host-free.** The functions that need **no port, no scheduler and no runtime service**:
//! `driver_alloc`/`driver_free`/`driver_realloc`, `driver_alloc_binary`/`driver_realloc_binary`/
//! `driver_free_binary`, `driver_system_info`, `null_func`, `erl_errno_id`,
//! `erl_drv_consume_timeslice`, the time trio (`erl_drv_monotonic_time`/`erl_drv_time_offset`/
//! `erl_drv_convert_time_unit`), the `driver_dl_open`/`driver_dl_sym`/`driver_dl_close`/
//! `driver_dl_error` quartet, `erl_drv_putenv`/`erl_drv_getenv` and `driver_mk_term_nil`.
//!
//! **Part 2 — port/host-dependent.** The functions whose `ErlDrvPort` operand **is** the pointer
//! [`DriverEnv::as_port`] hands out ([`DriverEnv::from_port`] recovers it) and which therefore
//! delegate to the [`DriverHost`](crate::driver::host::DriverHost) seam:
//!
//! * **output** — [`driver_output`], [`driver_output2`], [`driver_output_binary`],
//!   [`driver_outputv`]: they gather the header/body bytes and call [`DriverHost::output`](crate::driver::host::DriverHost::output).
//! * **term data** — [`driver_output_term`], [`driver_send_term`], [`erl_drv_output_term`],
//!   [`erl_drv_send_term`]: they decode the `ErlDrvTermData[]` spec with [`decode_term_spec`] and
//!   call [`DriverHost::send_term`](crate::driver::host::DriverHost::send_term).
//! * **port queries** — [`driver_connected`], [`driver_caller`], [`driver_mk_port`],
//!   [`driver_mk_atom`].
//! * **select / timers / async** — [`driver_select`], [`driver_set_timer`], [`driver_cancel_timer`],
//!   [`driver_read_timer`], [`driver_async`].
//!
//! Each of them returns the C failure code (`-1`) for a null port, a malformed spec or a
//! host-reported failure — never a crash, never a wrong result.
//!
//! Everything else stays `None` (it needs runtime structure this seam does not expose):
//!
//! * **port queues** — `driver_vec_to_buf` and the `driver_enq`/`driver_pushq`/`driver_deq`/
//!   `driver_sizeq`/`driver_peekq*`/`driver_enqv`/`driver_pushqv` family.
//! * **port lifecycle & attributes** — `driver_create_port`, `add_driver_entry`,
//!   `remove_driver_entry`, `driver_lock_driver`, `erl_drv_init_ack`, `erl_drv_set_os_pid`,
//!   `erl_drv_busy_msgq_limits`, the `driver_failure*`/`driver_exit` family, `set_busy_port`,
//!   `set_port_control_flags`, `get_port_flags`.
//! * **monitors & port data lock** — the `driver_monitor_*` family and the whole `driver_pdl_*`
//!   family.
//! * **async thread pool** — `driver_async_port_key` (the "fair" async key is derived from the
//!   port's id term, which this seam has no representation for).
//! * **threads** — the whole `erl_drv_mutex_*`/`erl_drv_cond_*`/`erl_drv_rwlock_*`/`erl_drv_tsd_*`/
//!   `erl_drv_thread_*`/`erl_drv_*_name` family.
//! * **deprecated** — `driver_get_now` (`ERL_DRV_DEPRECATED_FUNC`).
//!
//! The **binary refcount trio** (`driver_binary_get_refc`/`inc_refc`/`dec_refc`) is `None` too, and
//! deliberately: the frozen [`ErlDrvBinary`] layout is exactly `{ orig_size, orig_bytes }` (16 bytes,
//! asserted in [`crate::driver::types`]) — there is no refcount word, so there is nothing to report
//! or adjust, and the emulator's private `Binary` header is not part of this ABI.
//!
//! # Documented deviations from the oracle
//!
//! * [`erl_errno_id`] returns `strerror`'s **descriptive message**; the oracle returns the lowercase
//!   POSIX errno *name* (`"enoent"`, via `strerrorname_np`). Same input, different text.
//! * [`erl_drv_time_offset`] returns `0` by construction: this runtime anchors Erlang monotonic time
//!   on the system clock's epoch, so there is no separate offset to measure.
//! * [`driver_system_info`] **returns without writing** for a null `sip` or an `si_size` shorter than
//!   the first field group, where the C calls `erts_exit(ERTS_ERROR_EXIT, …)`: a library function
//!   must not abort its host process (workspace `CONTEXT.md` §3, never panic-as-control-flow).
//!
//! Part 2 (port/host-dependent) adds one deviation per port-carrying operand, because the C's port
//! *id term* has no counterpart in this seam:
//!
//! * [`driver_mk_port`] returns the **port handle pointer**, pointer-encoded, where the C returns the
//!   port's id term (`prt->common.id`); [`driver_mk_atom`] returns the **name pointer** where the C
//!   returns a pre-interned atom handle. Both are exactly the operands [`erl_drv_output_term`]/
//!   [`erl_drv_send_term`] and [`decode_term_spec`]'s `ERL_DRV_ATOM` expect, so each pair round-trips
//!   (`driver_mk_port` → `erl_drv_output_term`, `driver_mk_atom` → `ERL_DRV_ATOM`).
//! * [`driver_output_term`]/[`driver_send_term`]/[`erl_drv_output_term`]/[`erl_drv_send_term`] return
//!   the host's code (`0` = delivered) where the C returns `1` for a delivered term and `0` for an
//!   undeliverable one; a driver following the documented idiom of testing `< 0` sees identical
//!   behaviour. The header/body split of `driver_output*` is likewise joined into one byte slice,
//!   since [`DriverHost::output`](crate::driver::host::DriverHost::output) takes a single slice (the C can keep the header out as a list
//!   prefix).
//! * A malformed or out-of-range request is **refused** (`-1`) where the C would read past the end of
//!   a caller buffer: `driver_output_binary`'s `offset + len > orig_size`, a `driver_outputv` `skip`
//!   past the vector's declared lengths, and a null `time_left`/`data`/`ErlIOVec`.
//! * `driver_select`/`driver_set_timer`/`driver_cancel_timer`/`driver_read_timer`/`driver_async`
//!   forward to [`DriverHost`](crate::driver::host::DriverHost), whose defaulted methods report
//!   "unsupported" as `-1` (the C's
//!   failure code for a driver without the matching callback): a driver told `-1` falls back exactly
//!   as it would against an ERTS whose port has no such callback.
//!
//! # `unsafe`
//!
//! Every function here is FFI. Each `unsafe fn` carries a `# Safety` section for its caller contract
//! and every `unsafe` block a preceding `// SAFETY:` rationale (workspace `CONTEXT.md` §3). No
//! `unwrap()` happens on runtime data and no failure path panics. Part 2 adds the port-pointer
//! recovery ([`DriverEnv::from_port`]) and the raw operand reads of the term-data/vector/binary
//! arguments; each is reached only through one documented helper.

use libc::{c_char, c_int, c_long, c_uint, c_ulong, c_void, size_t};

use beam_term::prelude::{Eterm, NIL};

use crate::common::ERTS_NAPI_TIME_ERROR;
use crate::driver::api::ErlDrvApi;
use crate::driver::host::{decode_term_spec, DriverEnv};
use crate::driver::types::{
    ErlDrvBinary, ErlDrvEvent, ErlDrvPort, ErlDrvSInt, ErlDrvSizeT, ErlDrvSysInfo, ErlDrvTermData,
    ErlDrvTime, ErlDrvTimeUnit, ErlIOVec,
};
use crate::{
    ERL_DRV_EXTENDED_MAJOR_VERSION, ERL_DRV_EXTENDED_MINOR_VERSION, ERL_NIF_MAJOR_VERSION,
    ERL_NIF_MINOR_VERSION,
};

// ---------------------------------------------------------------------------------------------
// Static data exported to drivers
// ---------------------------------------------------------------------------------------------

/// `driver_term_nil` — the `ErlDrvTermData` for the empty list: the C is
/// `const ErlDrvTermData driver_term_nil = (ErlDrvTermData) NIL;` (`erts/emulator/beam/io.c:78`).
///
/// It is a **data** symbol, not a function, so it is not part of the function-pointer [`ErlDrvApi`]
/// table (see that module's note on `driver_mk_term_nil`).
#[no_mangle]
pub static driver_term_nil: ErlDrvTermData = NIL.raw() as ErlDrvTermData;

/// `ERLANG_VERSION` — the ERTS version string reported in [`ErlDrvSysInfo::erts_version`].
///
/// Mirrors [`beam_common::ERTS_VERSION`]; `driver_system_info_fills_the_version_fields` pins the two
/// together so they cannot drift.
static ERTS_VERSION_STR: &[u8] = b"17.1\0";

/// `ERLANG_OTP_RELEASE` — the OTP release string reported in [`ErlDrvSysInfo::otp_release`]. The C
/// derives it from `OTP_VERSION`'s major component; the same test pins it to
/// `beam_common::otp_version().major`.
static OTP_RELEASE_STR: &[u8] = b"30\0";

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
// Port output (host)
// ---------------------------------------------------------------------------------------------

/// The driver environment behind a C-facing port pointer, or `None` for a null pointer.
///
/// The whole of part 2 goes through this one recovery point, so the port-pointer safety contract is
/// stated once: **[`DriverEnv::as_port`] + [`DriverEnv::from_port`] are the port ABI** (see
/// [`crate::driver::host`]).
///
/// # Safety
///
/// `port` must be null or a pointer previously returned by [`DriverEnv::as_port`] for an environment
/// (and host) that is still alive — exactly [`DriverEnv::from_port`]'s own contract.
unsafe fn env_for<'a>(port: ErlDrvPort) -> Option<&'a mut DriverEnv> {
    // SAFETY: the caller's contract is `DriverEnv::from_port`'s contract, restated above.
    unsafe { DriverEnv::from_port(port) }
}

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
/// is what [`erl_drv_output_term`]/[`erl_drv_send_term`] decode back through [`DriverEnv::from_port`]
/// and what [`driver_async_port_key`](ErlDrvApi::driver_async_port_key) would key on. A null handle
/// yields [`driver_term_nil`], as the C does for an invalid one.
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
// The `#[used]` table
// ---------------------------------------------------------------------------------------------

/// The all-`None` base table.
///
/// `crate::driver::api::ErlDrvApi::empty()` is a plain `fn` (not `const`), so it cannot initialise a
/// `static`; this `const` is the `const`-evaluable equivalent. Every field of [`ErlDrvApi`] is an
/// `Option<unsafe extern "C" fn …>`, for which `None` is the null pointer (the null-pointer
/// optimization), so an all-zero struct **is** the all-`None` table.
/// `empty_base_matches_the_all_none_table_by_bytes` pins that equality against `ErlDrvApi::empty()`,
/// so a layout change cannot slip through unnoticed.
const EMPTY_DRIVER_API: ErlDrvApi = unsafe { core::mem::zeroed() };

/// The runtime's populated `driver_*` / `erl_drv_*` table.
///
/// `#[used]` keeps this static (and hence the `#[no_mangle]` functions it names) in the linkage, so a
/// driver `.so` loaded into the process can resolve them. Slots left `None` are listed in the module
/// header.
#[used]
static DRIVER_API: ErlDrvApi = ErlDrvApi {
    driver_alloc: Some(driver_alloc),
    driver_free: Some(driver_free),
    driver_realloc: Some(driver_realloc),
    driver_alloc_binary: Some(driver_alloc_binary),
    driver_realloc_binary: Some(driver_realloc_binary),
    driver_free_binary: Some(driver_free_binary),
    // Part 2: the port/host-dependent slots (see the module header).
    driver_output: Some(driver_output),
    driver_output2: Some(driver_output2),
    driver_output_binary: Some(driver_output_binary),
    driver_outputv: Some(driver_outputv),
    driver_output_term: Some(driver_output_term),
    driver_send_term: Some(driver_send_term),
    erl_drv_output_term: Some(erl_drv_output_term),
    erl_drv_send_term: Some(erl_drv_send_term),
    driver_mk_atom: Some(driver_mk_atom),
    driver_mk_port: Some(driver_mk_port),
    driver_connected: Some(driver_connected),
    driver_caller: Some(driver_caller),
    driver_select: Some(driver_select),
    driver_set_timer: Some(driver_set_timer),
    driver_cancel_timer: Some(driver_cancel_timer),
    driver_read_timer: Some(driver_read_timer),
    driver_async: Some(driver_async),
    driver_system_info: Some(driver_system_info),
    null_func: Some(null_func),
    erl_errno_id: Some(erl_errno_id),
    erl_drv_consume_timeslice: Some(erl_drv_consume_timeslice),
    erl_drv_monotonic_time: Some(erl_drv_monotonic_time),
    erl_drv_time_offset: Some(erl_drv_time_offset),
    erl_drv_convert_time_unit: Some(erl_drv_convert_time_unit),
    driver_dl_open: Some(driver_dl_open),
    driver_dl_sym: Some(driver_dl_sym),
    driver_dl_close: Some(driver_dl_close),
    driver_dl_error: Some(driver_dl_error),
    erl_drv_putenv: Some(erl_drv_putenv),
    erl_drv_getenv: Some(erl_drv_getenv),
    driver_mk_term_nil: Some(driver_mk_term_nil),
    ..EMPTY_DRIVER_API
};

/// The populated `driver_*` / `erl_drv_*` table (see [`DRIVER_API`]).
pub fn driver_api_table() -> &'static ErlDrvApi {
    &DRIVER_API
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ffi::CStr;
    use core::mem::{offset_of, size_of, size_of_val};
    use std::ffi::CString;

    use beam_alloc::Heap;
    use beam_term::atom::AtomTable;
    use beam_term::prelude::{
        atom_val, is_atom, is_tuple, make_atom, make_small, tuple_element, tuple_size,
    };

    use crate::driver::host::DriverHost;

    use crate::driver::types::{term_data, ERL_DRV_ATOM, ERL_DRV_INT, ERL_DRV_READ, ERL_DRV_TUPLE};

    /// A payload view of a driver binary, reached through the block base rather than the declared
    /// one-element `orig_bytes` array (the C's flexible-array-member convention).
    fn binary_payload<'a>(bin: *mut ErlDrvBinary, len: usize) -> &'a mut [u8] {
        // SAFETY: `bin` heads a `driver_binary_block_size(len)`-byte block, so the payload at
        // `offset_of!(orig_bytes)` is valid for `len` bytes.
        unsafe {
            core::slice::from_raw_parts_mut(
                (bin as *mut u8).add(offset_of!(ErlDrvBinary, orig_bytes)),
                len,
            )
        }
    }

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

    #[test]
    fn empty_base_matches_the_all_none_table_by_bytes() {
        let ours = EMPTY_DRIVER_API;
        let theirs = ErlDrvApi::empty();
        let n = size_of::<ErlDrvApi>();
        // SAFETY: both are live values of exactly `n` bytes (the table is a plain pointer mix).
        let a = unsafe { core::slice::from_raw_parts(&ours as *const _ as *const u8, n) };
        // SAFETY: as above.
        let b = unsafe { core::slice::from_raw_parts(&theirs as *const _ as *const u8, n) };
        assert_eq!(a, b, "the zeroed base must equal the all-None table");
    }

    #[test]
    fn driver_api_table_populates_the_implemented_slots_and_leaves_the_rest_none() {
        let t = driver_api_table();
        let populated = [
            // Part 1: host-free.
            t.driver_alloc.is_some(),
            t.driver_free.is_some(),
            t.driver_realloc.is_some(),
            t.driver_alloc_binary.is_some(),
            t.driver_realloc_binary.is_some(),
            t.driver_free_binary.is_some(),
            t.driver_system_info.is_some(),
            t.null_func.is_some(),
            t.erl_errno_id.is_some(),
            t.erl_drv_consume_timeslice.is_some(),
            t.erl_drv_monotonic_time.is_some(),
            t.erl_drv_time_offset.is_some(),
            t.erl_drv_convert_time_unit.is_some(),
            t.driver_dl_open.is_some(),
            t.driver_dl_sym.is_some(),
            t.driver_dl_close.is_some(),
            t.driver_dl_error.is_some(),
            t.erl_drv_putenv.is_some(),
            t.erl_drv_getenv.is_some(),
            t.driver_mk_term_nil.is_some(),
            // Part 2: port/host-dependent.
            t.driver_output.is_some(),
            t.driver_output2.is_some(),
            t.driver_output_binary.is_some(),
            t.driver_outputv.is_some(),
            t.driver_output_term.is_some(),
            t.driver_send_term.is_some(),
            t.erl_drv_output_term.is_some(),
            t.erl_drv_send_term.is_some(),
            t.driver_mk_atom.is_some(),
            t.driver_mk_port.is_some(),
            t.driver_connected.is_some(),
            t.driver_caller.is_some(),
            t.driver_select.is_some(),
            t.driver_set_timer.is_some(),
            t.driver_cancel_timer.is_some(),
            t.driver_read_timer.is_some(),
            t.driver_async.is_some(),
        ];
        assert!(
            populated.iter().all(|&p| p),
            "every implemented slot must be populated: {populated:?}"
        );

        // The families that need runtime structure this seam does not expose stay `None`.
        assert!(t.driver_vec_to_buf.is_none());
        assert!(t.driver_enq.is_none());
        assert!(t.driver_pushq.is_none());
        assert!(t.driver_monitor_process.is_none());
        assert!(t.erl_drv_mutex_create.is_none());
        assert!(t.driver_create_port.is_none());
        assert!(t.driver_async_port_key.is_none());
        assert!(t.erl_drv_init_ack.is_none());
        assert!(t.set_busy_port.is_none());
        assert!(t.driver_failure_eof.is_none());
        assert!(t.driver_pdl_create.is_none());
        // No refcount word exists in the frozen `ErlDrvBinary` layout.
        assert!(t.driver_binary_get_refc.is_none());
        assert!(t.driver_binary_inc_refc.is_none());
        assert!(t.driver_binary_dec_refc.is_none());

        assert_eq!(size_of_val(t), size_of::<ErlDrvApi>());
    }

    // -----------------------------------------------------------------------------------------
    // Part 2: the port/host-dependent functions
    // -----------------------------------------------------------------------------------------

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
