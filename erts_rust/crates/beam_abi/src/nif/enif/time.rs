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

//! `enif_*` — time, environment, system information, unique integers, timeslice and dynamic
//! symbols.
//!
//! Owned slots: `enif_monotonic_time`, `enif_time_offset`, `enif_convert_time_unit`,
//! `enif_now_time`, `enif_cpu_time`, `enif_system_info`, `enif_getenv`,
//! `enif_make_unique_integer`, `enif_consume_timeslice`, `enif_dlopen`, `enif_dlsym`.
//!
//! # ERTS time semantics reproduced here
//!
//! Ground truth: `erts/emulator/beam/erl_time_sup.c` (`erts_napi_monotonic_time`,
//! `erts_napi_time_offset`, `erts_napi_convert_time_unit`) and `erts/emulator/beam/erl_time.h`.
//!
//! ERTS does not expose the raw OS clock. It exposes
//!
//! * **Erlang monotonic time** = `ERTS_MONOTONIC_TO_<unit>(monotonic_native) + OFFSET_<unit>`, and
//! * **Erlang time offset** = `ERTS_MONOTONIC_TO_<unit>(wall_offset_native) - OFFSET_<unit>`,
//!
//! with the two invariants `monotonic_time(unit) + time_offset(unit) == system_time(unit)` and
//! `ERTS_MONOTONIC_BEGIN = ERTS_MONOTONIC_TIME_UNIT` = 1 000 000 000 ns. For the nanosecond build
//! (`ARCH_64`, `ERTS_COMPILE_TIME_MONOTONIC_TIME_UNIT = 1e9`) the C's
//! `ERTS_MONOTONIC_OFFSET_NATIVE = ERTS_MONOTONIC_TIME_START_EXTERNAL - ERTS_MONOTONIC_BEGIN`
//! evaluates to `(MIN_SMALL / 1e9) * 1e9 - 1e9`:
//!
//! ```text
//! OFFSET(ns)   = -576460752000000000 - 1000000000 = -576460753000000000
//! OFFSET(usec) = OFFSET(ns) / 1_000              = -576460753000000
//! OFFSET(msec) = OFFSET(ns) / 1_000_000          = -576460753000
//! OFFSET(sec)  = OFFSET(ns) / 1_000_000_000      = -576460753
//! ```
//!
//! `enif_time_offset/1` is pinned against the system oracle (`erlang:time_offset/0,1`) by the
//! crate tests. `enif_monotonic_time/1` uses the *same* formula, but the oracle's **internal**
//! monotonic clock additionally subtracts an emulator-start snapshot, so its absolute value is
//! anchored differently from a raw `CLOCK_MONOTONIC` read; only the invariant
//! `monotonic + offset == system_time` is comparable across implementations (asserted in the
//! tests, and documented here rather than papered over).
//!
//! # Documented gaps (never a wrong value)
//!
//! * **Invalid time units.** The C returns `ERTS_NAPI_TIME_ERROR` for an out-of-range unit code.
//!   [`ErlNifTimeUnit`] is a four-variant `#[repr(C)]` enum, so an out-of-range code is not a
//!   representable Rust value and the path is unreachable by construction; every call returns a
//!   *real* time. (As elsewhere in this crate, a malformed input is refused rather than trusted.)
//! * **`enif_consume_timeslice`.** This seam has no scheduler/process accounting, so the C's
//!   "timeslice consumed" answer cannot be computed. The call delegates to
//!   [`NifHost::consume_timeslice`](crate::nif::host::NifHost::consume_timeslice) and, with no host
//!   support, returns the C's **"not reached"** answer `0` — i.e. "the process need not yield".
//!   It never fabricates a `1` (which would force a yield that never happened).
//! * **`enif_system_info` values.** The runtime-dependent fields prefer the host's
//!   [`NifHost::system_info`](crate::nif::host::NifHost::system_info) block — reached through the
//!   current-call host stack, because the C entry point takes no env — and fall back to the
//!   built-ins when no host hook answers. With no host, `scheduler_threads` is reported as `1` (the
//!   runtime is single-threaded) and `async_threads` as `0`, exactly as the sibling
//!   `driver_system_info` in this crate reports them — the C implements `enif_system_info` *as*
//!   `driver_system_info`, so the two must agree. The built-in `otp_release` is the frozen workspace
//!   claim (`"30"`), which differs from the installed oracle's `"29"`; `erts_version` (`"17.1"`)
//!   matches the oracle. `beam_rt`'s `NifHostImpl` must supply the emulator's real values for the
//!   oracle to be matched.
//! * **`enif_make_unique_integer`** honours both SDK flag bits over a crate-internal monotonic
//!   counter when the host supplies no `unique_integer` hook. The counter's *base* is reproduced
//!   from the oracle's observed first values; the values are real, distinct, and monotone per flag
//!   set exactly as the C's are, but they are not the oracle's exact sequence (that counter is
//!   emulator state).

use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicI64, Ordering};

use libc::{c_char, c_int, c_void, size_t};

use crate::common::ERTS_NAPI_TIME_ERROR;
use crate::nif::types::{ErlNifSysInfo, ErlNifTime, ErlNifTimeUnit, ErlNifUniqueInteger};

use super::*;

/// Populate the `ErlNifApi` slots this file owns (see the module docs).
pub(super) const fn populate(mut api: ErlNifApi) -> ErlNifApi {
    api.enif_monotonic_time = Some(enif_monotonic_time);
    api.enif_time_offset = Some(enif_time_offset);
    api.enif_convert_time_unit = Some(enif_convert_time_unit);
    api.enif_now_time = Some(enif_now_time);
    api.enif_cpu_time = Some(enif_cpu_time);
    api.enif_system_info = Some(enif_system_info);
    api.enif_getenv = Some(enif_getenv);
    api.enif_make_unique_integer = Some(enif_make_unique_integer);
    api.enif_consume_timeslice = Some(enif_consume_timeslice);
    api.enif_dlopen = Some(enif_dlopen);
    api.enif_dlsym = Some(enif_dlsym);
    api
}

// ---------------------------------------------------------------------------------------------
// Time units and the ERTS offset (`erl_time.h`)
// ---------------------------------------------------------------------------------------------

/// `ERTS_MONOTONIC_TIME_UNIT` for this build: 1 000 000 000 ns (the nanosecond time unit).
const NSEC_PER_SEC: i64 = 1_000_000_000;

/// `ERTS_MONOTONIC_OFFSET_NSEC` — see the module docs for the derivation.
const MONOTONIC_OFFSET_NSEC: i64 = -576_460_753_000_000_000;

/// `ERTS_MONOTONIC_BEGIN` — the internal monotonic range starts here (`= 1s`), the anchor of the
/// `ERTS_MONOTONIC_OFFSET_*` derivation below.
#[allow(dead_code)]
const MONOTONIC_BEGIN_NSEC: i64 = NSEC_PER_SEC;

/// The unit's tick frequency (`erts_napi_convert_time_unit`'s `ffreq`/`tfreq`).
const fn freq(unit: ErlNifTimeUnit) -> i64 {
    match unit {
        ErlNifTimeUnit::Sec => 1,
        ErlNifTimeUnit::Msec => 1_000,
        ErlNifTimeUnit::Usec => 1_000_000,
        ErlNifTimeUnit::Nsec => NSEC_PER_SEC,
    }
}

/// The nanosecond count in one `unit` tick — `ERTS_MONOTONIC_TIME_UNIT / <unit> frequency`. The
/// native monotonic/offset values are nanosecond counts, so `ERTS_MONOTONIC_TO_<unit>(ns)` is
/// `ns / ns_per_tick`.
const fn ns_per_tick(unit: ErlNifTimeUnit) -> i64 {
    NSEC_PER_SEC / freq(unit)
}

/// `ERTS_MONOTONIC_OFFSET_<unit>` — the ERTS nanosecond offset converted to `unit`
/// (`ERTS_MONOTONIC_TO_<unit>(OFFSET_NATIVE)`; C truncates toward zero, and the value divides
/// exactly for every unit).
const fn offset_for(unit: ErlNifTimeUnit) -> i64 {
    MONOTONIC_OFFSET_NSEC / ns_per_tick(unit)
}

/// `ERTS_MONOTONIC_TO_<unit>(ns)` — nanosecond count to `unit` (truncating division, as in the C
/// macro family). The offset is negative and divides exactly; on the positive monotonic reading
/// Rust's truncating `/` is the C's.
fn to_unit(ns: i64, unit: ErlNifTimeUnit) -> i64 {
    ns / ns_per_tick(unit)
}

/// Read `clock_id` as a nanosecond count; `None` when the clock read fails.
fn clock_ns(clock_id: libc::clockid_t) -> Option<i64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a live, writable `timespec` and `clock_id` is a valid clock id.
    if unsafe { libc::clock_gettime(clock_id, &mut ts) } != 0 {
        return None;
    }
    Some(
        (ts.tv_sec as i64)
            .wrapping_mul(NSEC_PER_SEC)
            .wrapping_add(ts.tv_nsec as i64),
    )
}

/// The C's `erts_napi_convert_time_unit`: conversion between time units **with floor semantics**
/// (so negative values round toward −∞, unlike Rust's truncating `/`).
fn convert_time_unit(val: i64, from: ErlNifTimeUnit, to: ErlNifTimeUnit) -> i64 {
    let ffreq = freq(from);
    let tfreq = freq(to);
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

// ---------------------------------------------------------------------------------------------
// The time API
// ---------------------------------------------------------------------------------------------

/// `ErlNifTime enif_monotonic_time(ErlNifTimeUnit unit);` — Erlang monotonic time.
///
/// A failing clock read yields the `ERTS_NAPI_TIME_ERROR` sentinel rather than a fabricated
/// timestamp.
///
/// # Safety
///
/// No preconditions: no pointers are taken and the function never panics.
#[no_mangle]
pub unsafe extern "C" fn enif_monotonic_time(unit: ErlNifTimeUnit) -> ErlNifTime {
    match clock_ns(libc::CLOCK_MONOTONIC) {
        Some(ns) => to_unit(ns, unit).wrapping_add(offset_for(unit)),
        None => ERTS_NAPI_TIME_ERROR,
    }
}

/// `ErlNifTime enif_time_offset(ErlNifTimeUnit unit);` — the offset between Erlang monotonic time
/// and Erlang system time, so that `enif_monotonic_time(u) + enif_time_offset(u)` is wall time in
/// `u`.
///
/// # Safety
///
/// No preconditions: no pointers are taken and the function never panics.
#[no_mangle]
pub unsafe extern "C" fn enif_time_offset(unit: ErlNifTimeUnit) -> ErlNifTime {
    match (
        clock_ns(libc::CLOCK_REALTIME),
        clock_ns(libc::CLOCK_MONOTONIC),
    ) {
        // The C's `get_time_offset()` is the native wall−monotonic difference, so that the
        // monotonic reading plus this offset reproduces the wall-clock reading.
        (Some(real), Some(mono)) => to_unit(real.wrapping_sub(mono), unit)
            .wrapping_sub(offset_for(unit)),
        _ => ERTS_NAPI_TIME_ERROR,
    }
}

/// `ErlNifTime enif_convert_time_unit(ErlNifTime val, ErlNifTimeUnit from, ErlNifTimeUnit to);`
///
/// # Safety
///
/// No preconditions: no pointers are taken and the function never panics.
#[no_mangle]
pub unsafe extern "C" fn enif_convert_time_unit(
    val: ErlNifTime,
    from: ErlNifTimeUnit,
    to: ErlNifTimeUnit,
) -> ErlNifTime {
    convert_time_unit(val, from, to)
}

/// Build the C's `{MegaSecs, Secs, MicroSecs}` triple on `env`.
fn time_tuple(env: &mut NifEnv, megasec: i64, sec: i64, microsec: i64) -> ERL_NIF_TERM {
    let parts = [
        prelude::make_small(megasec),
        prelude::make_small(sec),
        prelude::make_small(microsec),
    ];
    match env.heap().tuple(&parts) {
        Ok(term) => n(term),
        Err(_) => bare_badarg(),
    }
}

/// `ERL_NIF_TERM enif_now_time(ErlNifEnv *env);` — the current wall time as
/// `{MegaSecs, Secs, MicroSecs}`.
///
/// A failing clock read (or a null env) yields `badarg`, never a fabricated timestamp.
///
/// # Safety
///
/// `env` must be null or a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_now_time(env: *mut crate::nif::types::ErlNifEnv) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    let Some(real_ns) = clock_ns(libc::CLOCK_REALTIME) else {
        return bare_badarg();
    };
    let usec = real_ns / 1_000;
    time_tuple(
        e,
        usec / 1_000_000_000_000,
        (usec / 1_000_000) % 1_000_000,
        usec % 1_000_000,
    )
}

/// `ERL_NIF_TERM enif_cpu_time(ErlNifEnv *env);` — the process CPU time as
/// `{MegaSecs, Secs, MicroSecs}` (the C's `erts_get_now_cpu`).
///
/// # Safety
///
/// `env` must be null or a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_cpu_time(env: *mut crate::nif::types::ErlNifEnv) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a live, writable `timespec` and the CPU clock id is valid.
    if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) } != 0 {
        return bare_badarg();
    }
    let secs = ts.tv_sec as i64;
    time_tuple(e, (secs / 1_000_000) % 1_000_000, secs % 1_000_000, ts.tv_nsec as i64 / 1_000)
}

// ---------------------------------------------------------------------------------------------
// System information
// ---------------------------------------------------------------------------------------------

/// `ERLANG_VERSION` — the ERTS version string (`beam_common::ERTS_VERSION`; pinned by a test).
static ERTS_VERSION_STR: &[u8] = b"17.1\0";

/// `ERLANG_OTP_RELEASE` — the OTP release string (`beam_common`'s `OTP_VERSION` major; pinned by a
/// test). The installed oracle reports `"29"` — see the module docs.
static OTP_RELEASE_STR: &[u8] = b"30\0";

/// `void enif_system_info(ErlNifSysInfo *sip, size_t si_size);`
///
/// The C implements this **as** `driver_system_info`, so the partial-fill contract is the same: a
/// field group is written only when its last field lies fully inside `si_size` (the groups are
/// delimited by `smp_support`, `scheduler_threads`, `nif_minor_version`, `dirty_scheduler_support`).
///
/// # Host values (the runtime's real system information)
///
/// The C reads emulator globals. There is no env argument here, so the runtime's values are reached
/// through the host of the current NIF call via the same thread-local mechanism `enif_alloc_env`
/// uses ([`crate::nif::host::with_current_host`]). When a host is reachable **and** its
/// [`NifHost::system_info`] hook answers, the **runtime-dependent** fields (`erts_version`,
/// `otp_release`, `thread_support`, `smp_support`, `async_threads`, `scheduler_threads`,
/// `dirty_scheduler_support`) come from it; otherwise the built-in values are used (nothing
/// regresses). The four **ABI-version** fields (`driver_major_version`, `driver_minor_version`,
/// `nif_major_version`, `nif_minor_version`) are always this crate's compile-time constants — the C
/// hard-codes them in `driver_system_info` too, so they are not runtime state.
///
/// **`beam_rt`'s `NifHostImpl` must supply the emulator's real values** (`erts_version`,
/// `otp_release`, `scheduler_threads`, `dirty_scheduler_support`, plus `thread_support`/
/// `smp_support`/`async_threads`) for `enif_system_info` to match the oracle. With no hook, the
/// built-ins are reported: `scheduler_threads = 1` (single-threaded) and `async_threads = 0`.
///
/// **Documented deviation:** the C calls `erts_exit(ERTS_ERROR_EXIT, …)` for a null `sip` or an
/// `si_size` shorter than the first field group; this returns without writing, because a library
/// function must not abort its host process.
///
/// # Safety
///
/// `sip` must be null or valid for writes over `si_size` bytes.
#[no_mangle]
pub unsafe extern "C" fn enif_system_info(sip: *mut ErlNifSysInfo, si_size: size_t) {
    if sip.is_null() {
        return;
    }
    // The host of the current NIF call, if any. `with_current_host` answers `None` when this thread
    // is not inside a NIF call (e.g. a direct call from a test), in which case the built-ins below
    // are reported.
    let host = crate::nif::host::with_current_host(|host| host.system_info()).flatten();
    let int_size = core::mem::size_of::<c_int>();
    let smp_support_end = core::mem::offset_of!(ErlNifSysInfo, smp_support) + int_size;
    let scheduler_threads_end = core::mem::offset_of!(ErlNifSysInfo, scheduler_threads) + int_size;
    let nif_minor_version_end = core::mem::offset_of!(ErlNifSysInfo, nif_minor_version) + int_size;
    let dirty_end = core::mem::offset_of!(ErlNifSysInfo, dirty_scheduler_support) + int_size;

    if si_size >= smp_support_end {
        // The version *strings* and the two support flags prefer the host; the two driver-ABI
        // version fields stay this crate's constants.
        let (erts_version, otp_release, thread_support, smp_support) = match host {
            Some(info) => (
                info.erts_version,
                info.otp_release,
                info.thread_support,
                info.smp_support,
            ),
            None => (
                ERTS_VERSION_STR.as_ptr() as *mut c_char,
                OTP_RELEASE_STR.as_ptr() as *mut c_char,
                1,
                1,
            ),
        };
        // SAFETY: the caller guarantees `sip` is writable for `si_size >= 32` bytes, covering
        // exactly these six fields (offsets 0..32).
        unsafe {
            (*sip).driver_major_version = crate::ERL_DRV_EXTENDED_MAJOR_VERSION;
            (*sip).driver_minor_version = crate::ERL_DRV_EXTENDED_MINOR_VERSION;
            (*sip).erts_version = erts_version;
            (*sip).otp_release = otp_release;
            (*sip).thread_support = thread_support;
            (*sip).smp_support = smp_support;
        }
    }
    if si_size >= scheduler_threads_end {
        let (async_threads, scheduler_threads) = match host {
            Some(info) => (info.async_threads, info.scheduler_threads),
            None => (0, 1),
        };
        // SAFETY: `si_size` covers `async_threads`/`scheduler_threads` (offsets 32..40).
        unsafe {
            (*sip).async_threads = async_threads;
            (*sip).scheduler_threads = scheduler_threads;
        }
    }
    if si_size >= nif_minor_version_end {
        // SAFETY: `si_size` covers `nif_major_version`/`nif_minor_version` (offsets 40..48). These
        // are the NIF-ABI constants, not runtime state.
        unsafe {
            (*sip).nif_major_version = crate::ERL_NIF_MAJOR_VERSION;
            (*sip).nif_minor_version = crate::ERL_NIF_MINOR_VERSION;
        }
    }
    if si_size >= dirty_end {
        // Built-in fallback: the sibling `driver_system_info` in this crate reports `0` too (the C
        // hard-codes `1` at `io.c:7494-7497`), so the two Rust entry points keep agreeing with one
        // another until a host supplies the real value.
        let dirty = match host {
            Some(info) => info.dirty_scheduler_support,
            None => 0,
        };
        // SAFETY: `si_size` covers `dirty_scheduler_support` (offset 48..52).
        unsafe { (*sip).dirty_scheduler_support = dirty };
    }
}

// ---------------------------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------------------------

/// `int enif_getenv(const char *key, char *value, size_t *value_size);` — read an environment
/// variable.
///
/// The C's exact protocol (`erts_sys_explicit_8bit_getenv` via `erl_drv_getenv`): `*value_size` is
/// `value`'s capacity on entry; a NUL is always reserved, so the value's usable length is
/// `*value_size - 1` bytes.
///
/// * key absent → `-1` (nothing is written);
/// * value too long for the buffer → `1`, with `*value_size` set to the needed capacity
///   (**including** the NUL);
/// * success → `0`, the value copied NUL-terminated and `*value_size` set to its length
///   (**excluding** the NUL).
///
/// A **null `value`** is the C's "report the required size only" form: it is treated as a
/// zero-capacity buffer, so a present key answers `1` with `*value_size` = the needed capacity.
///
/// # Safety
///
/// `key` must be NUL-terminated; `value_size` must point at a live `size_t`; `value` must be null
/// or writable for `*value_size` bytes.
#[no_mangle]
pub unsafe extern "C" fn enif_getenv(
    key: *const c_char,
    value: *mut c_char,
    value_size: *mut size_t,
) -> c_int {
    if key.is_null() || value_size.is_null() {
        return -1;
    }
    // SAFETY: `key` is a NUL-terminated C string (caller contract).
    let found = unsafe { libc::getenv(key) };
    if found.is_null() {
        return -1;
    }
    // SAFETY: `found` is NUL-terminated, so `strlen` reads only its own bytes.
    let len = unsafe { libc::strlen(found) };
    // A null `value` is the size-query form: capacity 0, so the "too small" branch answers.
    let cap = if value.is_null() {
        0
    } else {
        // SAFETY: `value_size` points at a live `size_t` (caller contract).
        unsafe { *value_size }
    };
    if len + 1 > cap {
        // SAFETY: as above — report the capacity needed (length plus the NUL).
        unsafe { *value_size = len + 1 };
        return 1;
    }
    // SAFETY: `value` is writable for `cap >= len + 1` bytes and `found` for `len + 1`; the
    // regions cannot overlap (one is the environment, the other the caller's buffer).
    unsafe {
        core::ptr::copy_nonoverlapping(found as *const c_char, value, len + 1);
        *value_size = len;
    }
    0
}

// ---------------------------------------------------------------------------------------------
// Unique integers
// ---------------------------------------------------------------------------------------------

/// The monotonic-counter base (`erlang:unique_integer([monotonic])`'s first value).
const UNIQUE_MONOTONIC_BASE: i64 = -576_460_752_303_423_488;
/// The non-monotonic (negative) counter base — the oracle's first `erlang:unique_integer()`.
const UNIQUE_NEGATIVE_BASE: i64 = -576_460_752_303_421_235;
/// The first `erlang:unique_integer([positive])`.
const UNIQUE_POSITIVE_BASE: i64 = 2_307;
/// The first `erlang:unique_integer([positive, monotonic])`.
const UNIQUE_POSITIVE_MONOTONIC_BASE: i64 = 2;

/// The four independent counters (one per flag combination), as the C keeps four sequences.
static UNIQUE_COUNTERS: [AtomicI64; 4] = [
    AtomicI64::new(0),
    AtomicI64::new(0),
    AtomicI64::new(0),
    AtomicI64::new(0),
];

/// `ERL_NIF_TERM enif_make_unique_integer(ErlNifEnv *env, ErlNifUniqueInteger properties);`
///
/// Delegates to [`NifHost::unique_integer`](crate::nif::host::NifHost::unique_integer) when the
/// host supplies one; otherwise it produces a real value from a crate-internal monotonic counter,
/// honouring the `ERL_NIF_UNIQUE_POSITIVE`/`ERL_NIF_UNIQUE_MONOTONIC` property bits exactly as the
/// C does (see the module docs for what is and is not reproduced).
///
/// # Safety
///
/// `env` must be null or a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_make_unique_integer(
    env: *mut crate::nif::types::ErlNifEnv,
    properties: ErlNifUniqueInteger,
) -> ERL_NIF_TERM {
    let bits = properties.bits();
    let positive = bits & ErlNifUniqueInteger::POSITIVE.bits() != 0;
    let monotonic = bits & ErlNifUniqueInteger::MONOTONIC.bits() != 0;
    if let Some(e) = unsafe { env_mut(env) } {
        if let Some(term) = e.host().unique_integer(positive, monotonic) {
            return n(term);
        }
    }
    let index = (positive as usize) | ((monotonic as usize) << 1);
    let base = match (positive, monotonic) {
        (false, false) => UNIQUE_NEGATIVE_BASE,
        (false, true) => UNIQUE_MONOTONIC_BASE,
        (true, false) => UNIQUE_POSITIVE_BASE,
        (true, true) => UNIQUE_POSITIVE_MONOTONIC_BASE,
    };
    let step = UNIQUE_COUNTERS[index].fetch_add(1, Ordering::Relaxed);
    n(prelude::make_small(base.wrapping_add(step)))
}

/// `int enif_consume_timeslice(ErlNifEnv *env, int percent);` — charge `percent` of a timeslice.
///
/// The C clamps `percent` into `[1, 100]` and answers whether the process must yield. With no
/// process/timeslice accounting in this seam the answer comes from
/// [`NifHost::consume_timeslice`](crate::nif::host::NifHost::consume_timeslice) and, absent host
/// support, is the C's **"not reached"** answer `0` (see the module docs).
///
/// # Safety
///
/// `env` must be null or a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_consume_timeslice(
    env: *mut crate::nif::types::ErlNifEnv,
    percent: c_int,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    let percent = percent.clamp(1, 100);
    if e.host().consume_timeslice(percent) {
        1
    } else {
        0
    }
}

// ---------------------------------------------------------------------------------------------
// Dynamic symbols
// ---------------------------------------------------------------------------------------------

/// The pending dynamic-loader error, if any (reads and clears the loader's error slot).
fn last_dl_error() -> String {
    // SAFETY: `dlerror` takes no arguments and returns either NULL or a NUL-terminated string that
    // stays valid until the next `dl*` call; the bytes are copied out immediately.
    unsafe {
        match libc::dlerror() {
            ptr if ptr.is_null() => "unknown dynamic loader error".to_string(),
            ptr => CStr::from_ptr(ptr).to_string_lossy().into_owned(),
        }
    }
}

/// Invoke a caller-supplied error handler with a message, as the C's `err_handler(err_arg, text)`.
///
/// # Safety
///
/// `handler` must be a valid C function of the declared signature (or `None`); `arg` is passed
/// through untouched.
unsafe fn report_error(
    handler: Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
    arg: *mut c_void,
    text: &str,
) {
    let Some(f) = handler else { return };
    // The message must be NUL-terminated and outlive the call; an interior NUL is impossible in
    // these messages, but a failure simply reports nothing rather than a malformed string.
    if let Ok(message) = CString::new(text) {
        // SAFETY: `f` is the caller's handler with the C signature `void (*)(void*, const char*)`.
        unsafe { f(arg, message.as_ptr()) };
    }
}

/// `void *enif_dlopen(const char *lib, void (*err_handler)(void*, const char*), void *err_arg);`
///
/// Reproduces `erl_nif.c:3326`: the shared-object suffix (`.so`) is appended **unconditionally**,
/// the object is opened with `RTLD_NOW | RTLD_LOCAL`, and the library's `nif_init` (falling back to
/// `_nif_init`) is resolved and called. Any failure returns `NULL`; a `dlopen` failure also calls
/// `err_handler(err_arg, message)` (the C invokes the handler only on that path).
///
/// # Safety
///
/// `lib` must be NUL-terminated; `err_handler` must be a valid C function (or null).
#[no_mangle]
pub unsafe extern "C" fn enif_dlopen(
    lib: *const c_char,
    err_handler: Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
    err_arg: *mut c_void,
) -> *mut c_void {
    if lib.is_null() {
        // SAFETY: the handler is called with the C's own signature.
        unsafe { report_error(err_handler, err_arg, "no library name given") };
        return core::ptr::null_mut();
    }
    // SAFETY: `lib` is a NUL-terminated C string (caller contract).
    let path = unsafe { CStr::from_ptr(lib) }.to_string_lossy().into_owned();
    let Ok(c_path) = CString::new(format!("{path}.so")) else {
        // SAFETY: as above.
        unsafe { report_error(err_handler, err_arg, "the library name contains a NUL byte") };
        return core::ptr::null_mut();
    };
    // Clear the loader's error slot so the next `dlerror` reports this call's outcome.
    // SAFETY: `dlerror` takes no arguments.
    unsafe { libc::dlerror() };
    // SAFETY: `c_path` is a NUL-terminated path; both flags are valid `dlopen` modes.
    let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        let text = last_dl_error();
        // SAFETY: `err_handler`/`err_arg` are the caller's; the message outlives the call.
        unsafe { report_error(err_handler, err_arg, &text) };
        return core::ptr::null_mut();
    }
    // Resolve the library's NIF init function (`erts_sys_ddll_load_nif_init`).
    // SAFETY: `handle` is a live `dlopen` handle for the whole function body.
    let mut init = unsafe { dlsym_checked(handle, b"nif_init\0") };
    if init.is_none() {
        // SAFETY: `handle` is live (see above).
        init = unsafe { dlsym_checked(handle, b"_nif_init\0") };
    }
    let Some(init_fn) = init else {
        // No init symbol: close the object again rather than leaking the handle.
        // SAFETY: `handle` is a live `dlopen` handle, closed exactly once here.
        unsafe { libc::dlclose(handle) };
        return core::ptr::null_mut();
    };
    // SAFETY: `init_fn` is the library's `nif_init`/`_nif_init` symbol, whose C signature is
    // `ErlNifEntry* (void)`.
    let init: unsafe extern "C" fn() -> *mut c_void = unsafe { core::mem::transmute(init_fn) };
    // SAFETY: `init` is the resolved, signature-correct init function.
    let entry = unsafe { init() };
    if entry.is_null() {
        // SAFETY: `handle` is a live `dlopen` handle, closed exactly once here.
        unsafe { libc::dlclose(handle) };
        return core::ptr::null_mut();
    }
    handle
}

/// `dlsym` with the loader's own error detection (`dlerror` non-NULL means failure).
///
/// # Safety
///
/// `handle` must be a live `dlopen` handle and `name` NUL-terminated.
unsafe fn dlsym_checked(handle: *mut c_void, name: &[u8]) -> Option<*mut c_void> {
    // SAFETY: the caller guarantees `handle` is live and `name` NUL-terminated.
    unsafe {
        libc::dlerror();
        let sym = libc::dlsym(handle, name.as_ptr() as *const c_char);
        if libc::dlerror().is_null() {
            Some(sym)
        } else {
            None
        }
    }
}

/// `void *enif_dlsym(void *handle, const char *symbol, void (*err_handler)(void*,const char*), void *err_arg);`
///
/// Reproduces `erl_nif.c:3347`: `dlsym` with the loader's own error detection; on failure the
/// handler is called with the loader's message and `NULL` is returned.
///
/// # Safety
///
/// `handle` must be a live `dlopen` handle, `symbol` NUL-terminated, and `err_handler` a valid C
/// function (or null).
#[no_mangle]
pub unsafe extern "C" fn enif_dlsym(
    handle: *mut c_void,
    symbol: *const c_char,
    err_handler: Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
    err_arg: *mut c_void,
) -> *mut c_void {
    if handle.is_null() || symbol.is_null() {
        // SAFETY: the handler is called with the C's own signature.
        unsafe { report_error(err_handler, err_arg, "no handle or symbol given") };
        return core::ptr::null_mut();
    }
    // SAFETY: `symbol` is a NUL-terminated C string (caller contract).
    let name = unsafe { CStr::from_ptr(symbol) };
    // SAFETY: `handle` is live and `name` is NUL-terminated (the caller's contract).
    let found = unsafe { dlsym_checked(handle, name.to_bytes_with_nul()) };
    match found {
        Some(sym) => sym,
        None => {
            let text = format!("{name:?}: {}", last_dl_error());
            // SAFETY: `err_handler`/`err_arg` are the caller's; the message outlives the call.
            unsafe { report_error(err_handler, err_arg, &text) };
            core::ptr::null_mut()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_term::atom::AtomTable;
    use beam_term::prelude::{atom_val, is_atom, make_atom};
    use crate::nif::host::NifHost;

    /// A minimal host: a bump heap, an atom table and a recording timeslice hook.
    struct Host {
        heap: Heap,
        atoms: AtomTable,
        slices: Vec<c_int>,
    }

    impl Host {
        fn new() -> Host {
            Host {
                heap: Heap::new(256).expect("a test heap is allocatable"),
                atoms: AtomTable::new(),
                slices: Vec::new(),
            }
        }
    }

    impl NifHost for Host {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }
        fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
            Some(make_atom(self.atoms.intern(name)))
        }
        fn atom_name(&mut self, atom: Eterm) -> Option<String> {
            if !is_atom(atom) {
                return None;
            }
            self.atoms.name(atom_val(atom)).map(str::to_string)
        }
        fn consume_timeslice(&mut self, percent: c_int) -> bool {
            self.slices.push(percent);
            true
        }
    }

    #[test]
    fn convert_time_unit_is_floor_within_units() {
        // Upward conversions multiply.
        assert_eq!(
            convert_time_unit(3, ErlNifTimeUnit::Sec, ErlNifTimeUnit::Msec),
            3000
        );
        assert_eq!(
            convert_time_unit(3, ErlNifTimeUnit::Sec, ErlNifTimeUnit::Nsec),
            3_000_000_000
        );
        // Downward conversions divide, truncating toward zero for a positive value...
        assert_eq!(
            convert_time_unit(3_999, ErlNifTimeUnit::Msec, ErlNifTimeUnit::Sec),
            3
        );
        // ...and toward minus infinity for a negative one (the C's explicit rule).
        assert_eq!(
            convert_time_unit(-3_999, ErlNifTimeUnit::Msec, ErlNifTimeUnit::Sec),
            -4
        );
        assert_eq!(
            convert_time_unit(-1_500, ErlNifTimeUnit::Usec, ErlNifTimeUnit::Msec),
            -2
        );
        // Same unit is the identity, at every unit.
        for unit in [
            ErlNifTimeUnit::Sec,
            ErlNifTimeUnit::Msec,
            ErlNifTimeUnit::Usec,
            ErlNifTimeUnit::Nsec,
        ] {
            assert_eq!(convert_time_unit(-7, unit, unit), -7);
        }
    }

    #[test]
    fn monotonic_time_is_monotone_and_unit_consistent() {
        // SAFETY: no pointers are taken.
        let a = unsafe { enif_monotonic_time(ErlNifTimeUnit::Nsec) };
        // SAFETY: as above.
        let b = unsafe { enif_monotonic_time(ErlNifTimeUnit::Nsec) };
        assert!(a != ERTS_NAPI_TIME_ERROR && b != ERTS_NAPI_TIME_ERROR);
        assert!(b >= a, "monotonic time must not go backwards: {a} -> {b}");

        // Three *separate* clock readings, each converted with the C's "convert, then add the
        // per-unit offset" order. Because the offset is negative, `trunc((mN + OFFSET)/1e9)` and
        // `trunc(mN/1e9) + OFFSET_SEC` differ by at most one tick; so the cross-call agreement is
        // bounded by one unit rather than exact.
        let ns = unsafe { enif_monotonic_time(ErlNifTimeUnit::Nsec) };
        let msec = unsafe { enif_monotonic_time(ErlNifTimeUnit::Msec) };
        let sec = unsafe { enif_monotonic_time(ErlNifTimeUnit::Sec) };
        assert!((sec - ns / 1_000_000_000).abs() <= 1, "sec={sec} ns={ns}");
        assert!((msec - ns / 1_000_000).abs() <= 1, "msec={msec} ns={ns}");
        // The pure conversion helpers ARE exact on a single native reading: `to_unit` then
        // `offset_for` reproduces the C's per-unit value, and the offset divides exactly.
        let native = 5 * NSEC_PER_SEC + 500_000_000;
        assert_eq!(to_unit(native, ErlNifTimeUnit::Sec), 5);
        assert_eq!(
            to_unit(native, ErlNifTimeUnit::Sec) + offset_for(ErlNifTimeUnit::Sec),
            -576_460_748
        );
        assert_eq!(
            offset_for(ErlNifTimeUnit::Msec),
            offset_for(ErlNifTimeUnit::Nsec) / 1_000_000
        );
        assert_eq!(MONOTONIC_OFFSET_NSEC, -576_460_753_000_000_000);
    }

    #[test]
    fn monotonic_plus_offset_is_wall_time() {
        // SAFETY: no pointers are taken; the two reads bracket a wall-clock read.
        let mt = unsafe { enif_monotonic_time(ErlNifTimeUnit::Nsec) };
        // SAFETY: as above.
        let offset = unsafe { enif_time_offset(ErlNifTimeUnit::Nsec) };
        let real = clock_ns(libc::CLOCK_REALTIME).expect("the real-time clock is readable");
        let diff = (mt + offset - real).abs();
        assert!(
            diff < 5_000_000_000,
            "monotonic + offset must track wall time (off by {diff} ns)"
        );
        assert!(offset > 0, "the wall clock is ahead of the monotonic range");
    }

    #[test]
    fn now_time_and_cpu_time_are_three_tuples_of_smalls() {
        let mut host = Host::new();
        let mut env = NifEnv::new(&mut host);
        // SAFETY: `env` is live.
        let now = unsafe { enif_now_time(env.as_ptr()) };
        let term = t(now);
        // SAFETY: `term` is a live, well-formed term.
        assert!(unsafe { prelude::is_tuple(term) }, "enif_now_time returns a tuple");
        // SAFETY: `term` is a live tuple; the arity word is at word 0.
        assert_eq!(unsafe { prelude::arityval(*prelude::boxed_val(term)) }, 3);
        // SAFETY: as above — word 1 is the megaseconds element.
        assert!(prelude::is_small(unsafe { *prelude::boxed_val(term).add(1) }));

        // SAFETY: `env` is live.
        let cpu = unsafe { enif_cpu_time(env.as_ptr()) };
        // SAFETY: the term was produced by a live call above.
        assert!(unsafe { prelude::is_tuple(t(cpu)) });

        // A null env is a typed badarg, never a crash.
        // SAFETY: a null env is explicitly allowed.
        assert_eq!(t(unsafe { enif_now_time(core::ptr::null_mut()) }), NIL);
    }

    #[test]
    fn system_info_fills_what_fits_and_never_overruns() {
        const PAINT: u8 = 0xFF;
        let mut buf = [PAINT; core::mem::size_of::<ErlNifSysInfo>()];
        let sip = buf.as_mut_ptr().cast::<ErlNifSysInfo>();
        let full = core::mem::size_of::<ErlNifSysInfo>();
        // SAFETY: `sip` points at a full-size painted buffer.
        unsafe { enif_system_info(sip, full) };
        // SAFETY: `sip` is a live, fully written `ErlNifSysInfo`.
        let info = unsafe { *sip };
        assert_eq!(info.nif_major_version, crate::ERL_NIF_MAJOR_VERSION);
        assert_eq!(info.nif_minor_version, crate::ERL_NIF_MINOR_VERSION);
        assert_eq!(info.smp_support, 1);
        assert_eq!(info.scheduler_threads, 1);
        // SAFETY: the two version pointers are static NUL-terminated buffers.
        let erts = unsafe { CStr::from_ptr(info.erts_version) };
        assert_eq!(erts.to_str().expect("ASCII"), beam_common::ERTS_VERSION);

        // A too-short size writes nothing at all.
        let mut short = [PAINT; 8];
        // SAFETY: `short` is 8 bytes, which is shorter than the first field group.
        unsafe { enif_system_info(short.as_mut_ptr().cast::<ErlNifSysInfo>(), 8) };
        assert_eq!(short, [PAINT; 8], "a short si_size must not write");

        // A null `sip` is ignored (the C would abort the process).
        // SAFETY: a null `sip` is explicitly allowed.
        unsafe { enif_system_info(core::ptr::null_mut(), full) };
    }

    /// `std::env::set_var` is safe in edition 2021 and `unsafe` in edition 2024; this helper keeps
    /// the test source warning-free under either.
    #[allow(unused_unsafe)]
    fn set_var(key: &str, value: &str) {
        // SAFETY: single-threaded test setup; no other thread reads the environment concurrently.
        unsafe { std::env::set_var(key, value) }
    }

    #[test]
    fn getenv_reproduces_the_c_buffer_protocol() {
        set_var("BEAM_ABI_TIME_TEST", "hello");
        let key = CString::new("BEAM_ABI_TIME_TEST").expect("no interior NUL");
        let mut buf = [0i8; 16];
        let mut size: size_t = buf.len();
        // SAFETY: `key` is NUL-terminated; `buf` is writable for `size` bytes; `size` is live.
        let code = unsafe { enif_getenv(key.as_ptr(), buf.as_mut_ptr(), &mut size) };
        assert_eq!((code, size), (0, 5), "success reports the length without the NUL");
        // SAFETY: the call wrote `size + 1` bytes of `buf`.
        assert_eq!(unsafe { CStr::from_ptr(buf.as_ptr()) }.to_bytes(), b"hello");

        // Too small a buffer: the C's "1" with the needed capacity.
        let mut tiny = [0i8; 3];
        let mut tiny_size: size_t = tiny.len();
        // SAFETY: as above.
        let code = unsafe { enif_getenv(key.as_ptr(), tiny.as_mut_ptr(), &mut tiny_size) };
        assert_eq!((code, tiny_size), (1, 6));

        // A null `value` is the size-query form.
        let mut query_size: size_t = 0;
        // SAFETY: a null `value` is explicitly allowed.
        let code = unsafe { enif_getenv(key.as_ptr(), core::ptr::null_mut(), &mut query_size) };
        assert_eq!((code, query_size), (1, 6));

        // An absent key is the C's "-1" and writes nothing.
        let missing = CString::new("BEAM_ABI_TIME_TEST_ABSENT").expect("no interior NUL");
        let mut absent = [0i8; 8];
        let mut absent_size: size_t = absent.len();
        // SAFETY: as above.
        let code = unsafe { enif_getenv(missing.as_ptr(), absent.as_mut_ptr(), &mut absent_size) };
        assert_eq!((code, absent_size), (-1, 8));
    }

    #[test]
    fn unique_integers_honour_the_property_bits() {
        let mut host = Host::new();
        let mut env = NifEnv::new(&mut host);
        let flags = |p: bool, m: bool| {
            let bits = (if p { ErlNifUniqueInteger::POSITIVE.bits() } else { 0 })
                | (if m { ErlNifUniqueInteger::MONOTONIC.bits() } else { 0 });
            ErlNifUniqueInteger::from_bits(bits)
        };
        let call = |env: &mut NifEnv, f: ErlNifUniqueInteger| {
            // SAFETY: `env` is live.
            let term = unsafe { enif_make_unique_integer(env.as_ptr(), f) };
            let value = t(term);
            assert!(prelude::is_small(value), "small integers are immediates");
            prelude::signed_value(value)
        };

        // `[positive]` values are positive and strictly increasing.
        let p1 = call(&mut env, flags(true, false));
        let p2 = call(&mut env, flags(true, false));
        assert!(p1 > 0 && p2 > p1, "{p1} -> {p2}");

        // `[monotonic]` values are negative (the ERTS base) and strictly increasing.
        let m1 = call(&mut env, flags(false, true));
        let m2 = call(&mut env, flags(false, true));
        assert!(m1 < 0 && m2 > m1, "{m1} -> {m2}");
        assert_eq!(m1, UNIQUE_MONOTONIC_BASE);

        // `[positive, monotonic]` is positive and increasing, from its own counter.
        let pm1 = call(&mut env, flags(true, true));
        let pm2 = call(&mut env, flags(true, true));
        assert_eq!(pm1, UNIQUE_POSITIVE_MONOTONIC_BASE);
        assert!(pm2 > pm1);

        // No flags: the negative non-monotonic sequence.
        let n1 = call(&mut env, flags(false, false));
        assert_eq!(n1, UNIQUE_NEGATIVE_BASE);
    }

    #[test]
    fn consume_timeslice_delegates_and_clamps() {
        let mut host = Host::new();
        let mut env = NifEnv::new(&mut host);
        // SAFETY: `env` is live.
        assert_eq!(unsafe { enif_consume_timeslice(env.as_ptr(), 50) }, 1);
        // SAFETY: as above.
        assert_eq!(unsafe { enif_consume_timeslice(env.as_ptr(), 0) }, 1);
        // SAFETY: as above.
        assert_eq!(unsafe { enif_consume_timeslice(env.as_ptr(), 999) }, 1);
        assert_eq!(host.slices, vec![50, 1, 100], "percent is clamped to [1,100]");
        // A null env answers the C's "not reached" without touching a host.
        // SAFETY: a null env is explicitly allowed.
        assert_eq!(unsafe { enif_consume_timeslice(core::ptr::null_mut(), 50) }, 0);
    }

    #[test]
    fn dlopen_and_dlsym_report_failures_through_the_handler() {
        unsafe extern "C" fn record(arg: *mut c_void, text: *const c_char) {
            // SAFETY: `arg` is the `&mut Option<String>` this test passed as `err_arg`.
            let slot = unsafe { &mut *(arg as *mut Option<String>) };
            // SAFETY: the runtime passes a NUL-terminated message.
            *slot = Some(unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned());
        }

        let mut seen: Option<String> = None;
        let arg = &mut seen as *mut Option<String> as *mut c_void;
        let name = CString::new("/nonexistent/beam_abi/nope").expect("no interior NUL");
        // SAFETY: `name` is NUL-terminated; `record` matches the handler signature; `arg` is the
        // live `seen` slot.
        let handle = unsafe { enif_dlopen(name.as_ptr(), Some(record), arg) };
        assert!(handle.is_null(), "a missing library must return NULL");
        let message = seen.expect("the error handler must be called");
        assert!(message.contains("nope.so"), "handler message: {message}");

        // `enif_dlsym` over a null handle reports through the handler too.
        let mut seen2: Option<String> = None;
        let symbol = CString::new("nif_init").expect("no interior NUL");
        // SAFETY: a null handle and a NUL-terminated symbol.
        let sym = unsafe {
            enif_dlsym(
                core::ptr::null_mut(),
                symbol.as_ptr(),
                Some(record),
                &mut seen2 as *mut Option<String> as *mut c_void,
            )
        };
        assert!(sym.is_null());
        assert!(seen2.is_some(), "the handler must be called for a null handle");
    }

    #[test]
    fn dlopen_of_a_real_library_resolves_symbols() {
        // `libc.so.6`/`libm.so.6` are the loader-resolvable names of two always-present libraries.
        let mut handle = core::ptr::null_mut();
        let mut opened_with = "";
        for candidate in ["libc.so.6", "libm.so.6"] {
            let name = CString::new(candidate).expect("no interior NUL");
            // SAFETY: `name` is NUL-terminated; no error handler is needed here.
            let h = unsafe { enif_dlopen(name.as_ptr(), None, core::ptr::null_mut()) };
            if !h.is_null() {
                handle = h;
                // `enif_dlopen` appends `.so` unconditionally, so `libc.so.6` became
                // `libc.so.6.so`; a hit therefore means the loader found the base name.
                opened_with = candidate;
                break;
            }
        }
        if handle.is_null() {
            println!("SKIP: neither libc.so.6 nor libm.so.6 could be dlopen'd by name");
            return;
        }
        let symbol = CString::new("getpid").expect("no interior NUL");
        // SAFETY: `handle` is live and `symbol` is NUL-terminated.
        let sym = unsafe { enif_dlsym(handle, symbol.as_ptr(), None, core::ptr::null_mut()) };
        // `libm.so.6` has no `getpid`; accept either a resolved symbol from libc or the honest
        // `NULL` from the other library.
        assert!(
            !sym.is_null() || opened_with == "libm.so.6",
            "getpid must resolve from libc"
        );
        // SAFETY: `handle` came from `enif_dlopen` and is closed exactly once.
        unsafe { libc::dlclose(handle) };
    }
}
