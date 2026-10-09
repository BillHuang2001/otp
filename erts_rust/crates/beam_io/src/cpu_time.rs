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

//! CPU-time facility — a safe `getrusage(2)`/`clock_gettime(2)` floor.
//!
//! This is the raw-OS work behind `erlang:statistics(runtime)`: the runtime crate `beam_rt`
//! is `#![deny(unsafe_code)]` and cannot issue the syscalls itself, so the measurement lives
//! here behind a safe API — the same role [`crate::signal`] plays for `os:set_signal/2` and
//! [`crate::ddll`] for `erl_ddll`.
//!
//! # C fidelity — the oracle's `erlang:statistics(runtime)`
//!
//! `erlang:statistics(runtime)` returns a **2-tuple `{Total, SinceLastCall}`** of integer
//! **milliseconds** (`erts/emulator/beam/erl_bif_info.c:4277-4291`). The runtime arm calls
//! `erts_runtime_elapsed_both(&u1, NULL, &u2, NULL)` and builds `TUPLE2(u1, u2)`. Both
//! **system-time** out-parameters are `NULL`, so only the **user** CPU time feeds the pair
//! (that function still computes the system time, but this arm discards it).
//!
//! `erts_runtime_elapsed_both` (`erts/emulator/beam/erl_time_sup.c:1421-1481`) measures the
//! **whole process's** CPU time with `getrusage(RUSAGE_SELF, &now)` and converts
//! `now.ru_utime` to milliseconds:
//!
//! ```c
//! user  = (ErtsMonotonicTime) now.ru_utime.tv_sec;
//! user *= (ErtsMonotonicTime) 1000000;
//! user += (ErtsMonotonicTime) now.ru_utime.tv_usec;
//! user /= (ErtsMonotonicTime) 1000;         /* µs → ms, truncating */
//! ```
//!
//! `SinceLastCall` is the difference against a **process-global previous value initialised
//! to 0** (`runtime_prev.data.user = 0`, `erl_time_sup.c:1071`), updated on **every** call ⇒
//! **the first call's `SinceLastCall` equals its `Total`**. The C protects the
//! read/update with a mutex because any scheduler thread may call it; here the counter is
//! owned by the single-threaded runtime — see [`RuntimeClock`].
//!
//! [`process_cpu_times`] is exactly that `getrusage(RUSAGE_SELF)` measurement, returning both
//! the user and the system component. The `runtime` arm ignores the system half, and nothing
//! else in the tree consumes it either — `erlang:statistics(runtime)` is the **only** caller of
//! `erts_runtime_elapsed_both` (`erl_bif_info.c:4281`), and it passes `NULL` for both system
//! out-parameters — so the system component is returned here for completeness.
//! [`thread_cpu_times`] is the per-**thread** analogue via
//! `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`, whose reading is **user + system combined** —
//! so it is reported as `user_ms` with `system_ms == 0` (the kernel does not split the two
//! for that clock). It is *not* what `erlang:statistics(runtime)` uses.
//!
//! # `beam_rt` adoption contract
//!
//! Adopting this into the `erlang:statistics(runtime)` BIF is **`beam_rt`'s job**, not this
//! crate's. The runtime:
//!
//! 1. owns **one** [`RuntimeClock`] for the **whole lifetime of the runtime** (its `prev`
//!    field plays the role of the C's process-global `runtime_prev`, so a fresh clock per
//!    call would make `SinceLastCall` always equal `Total`), and
//! 2. serves the `statistics/1` `runtime` key from `RuntimeClock::runtime()` (the
//!    `RuntimeHooksBridge` arm, `crates/beam_rt/src/context/runtime_hooks.rs`), rendering the
//!    returned `(Total, SinceLastCall)` as the oracle's 2-tuple of integers.
//!
//! `runtime()` returns the pair **in milliseconds**, already in the oracle's units and shape,
//! so the runtime adds no arithmetic of its own.
//!
//! # Errors
//!
//! Both measurement functions report a failing syscall as [`beam_common::BeamError::Io`] carrying the
//! errno — never a panic and never a silently fabricated `0`. (The C aborts the emulator on a
//! failed `getrusage`; a Rust runtime must not, so the failure is returned and the caller
//! decides.)
//!
//! # Safety
//!
//! `unsafe` appears **only** in the two syscall wrappers ([`process_cpu_times`] and
//! [`thread_cpu_times`]), each with an inline `// SAFETY:` rationale. Every other item here is
//! safe Rust, and the crate never requires a caller to uphold a safety contract.

use std::io::Error as IoError;

use beam_common::Result;

/// Microseconds per second — the C's `1000000` factor in the `timeval` conversion.
const USEC_PER_SEC: u64 = 1_000_000;

/// Microseconds per millisecond — the C's `/1000` in the `timeval` conversion.
const USEC_PER_MSEC: u64 = 1_000;

/// Nanoseconds per second — the factor in the `timespec` conversion.
const NSEC_PER_SEC: u64 = 1_000_000_000;

/// Nanoseconds per millisecond — the `timespec` (nanosecond) → millisecond divisor.
const NSEC_PER_MSEC: u64 = 1_000_000;

/// A CPU-time reading in **whole milliseconds**, split into its user and system components.
///
/// The unit and truncation are the oracle's: microseconds divided by 1000
/// (`erts_runtime_elapsed_both`, `erl_time_sup.c:1421-1481`), so a reading subdivides exactly
/// as the C's does.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CpuTimes {
    /// **User**-mode CPU time in milliseconds.
    ///
    /// For [`process_cpu_times`] this is the process's user time (`rusage.ru_utime`), the
    /// value `erlang:statistics(runtime)` reports. For [`thread_cpu_times`] it is the calling
    /// thread's **combined** user + system time, because `CLOCK_THREAD_CPUTIME_ID` does not
    /// distinguish the two.
    pub user_ms: u64,
    /// **System**-mode CPU time in milliseconds (`rusage.ru_stime`); always `0` for
    /// [`thread_cpu_times`] (the thread clock has no split).
    pub system_ms: u64,
}

/// The **process's** accumulated CPU time via `getrusage(RUSAGE_SELF)`, in milliseconds.
///
/// `RUSAGE_SELF` reports the resource usage of the **calling process** — the sum over all its
/// threads — which is precisely the C's measurement for `erlang:statistics(runtime)`
/// (`erts_runtime_elapsed_both`, `erl_time_sup.c:1421-1481`). Both components are returned:
/// `user_ms` from `ru_utime`, `system_ms` from `ru_stime`; the oracle's `runtime` arm uses the
/// user half only.
///
/// The microsecond→millisecond conversion (truncating `/ 1000`) matches the C exactly, so the
/// numbers agree with the reference to the millisecond.
///
/// # Errors
///
/// [`beam_common::BeamError::Io`] carrying the errno if `getrusage(2)` fails.
pub fn process_cpu_times() -> Result<CpuTimes> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: `RUSAGE_SELF` is a valid `who` for this call, and `usage.as_mut_ptr()` points
    // at a live, writable (uninitialized) `rusage` that the kernel fully writes on success.
    // On failure the buffer is left untouched and is never read — the `assume_init` below is
    // reached only when the call returned 0.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if rc != 0 {
        return Err(crate::error::io(IoError::last_os_error()));
    }
    // SAFETY: `getrusage` returned 0, so it initialised every field of `usage`.
    let usage = unsafe { usage.assume_init() };
    Ok(CpuTimes {
        user_ms: timeval_ms(usage.ru_utime),
        system_ms: timeval_ms(usage.ru_stime),
    })
}

/// The **calling thread's** accumulated CPU time via `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`.
///
/// This clock counts the thread's own consumption — it excludes time spent by other threads
/// and time the thread spent off-CPU. The kernel reports it as a **single combined user +
/// system** count, so the reading is returned as [`CpuTimes::user_ms`] and
/// [`CpuTimes::system_ms`] is left `0` (there is no split to report; this is documented rather
/// than guessed at).
///
/// The nanosecond reading is truncated to milliseconds, consistent with
/// [`process_cpu_times`] and the oracle's unit.
///
/// This is the analogue of the emulator's own thread-clock read `sys_get_cputime`
/// (`erts/emulator/sys/unix/erl_unix_sys.h:319`, `clock_gettime(CLOCK_THREAD_CPUTIME_ID, …)`,
/// the `HAVE_CLOCK_GETTIME_CPU_TIME` branch). The NIF `enif_cpu_time` reaches that same read
/// **indirectly**, through `erts_get_now_cpu` (`erl_nif.c:2375` → `erl_time_sup.c:1949-1958`);
/// on a platform whose header selects `gethrvtime` instead (`erl_unix_sys.h:309-315`) that
/// emulator read is per **process**, not per thread.
///
/// It is **not** what `erlang:statistics(runtime)` measures — that arm uses `getrusage`, see
/// [`process_cpu_times`] — so the runtime adopts [`RuntimeClock`] for that key.
///
/// # Errors
///
/// [`beam_common::BeamError::Io`] carrying the errno if `clock_gettime(2)` fails.
pub fn thread_cpu_times() -> Result<CpuTimes> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `CLOCK_THREAD_CPUTIME_ID` is a clock id this platform serves, and `ts` is a
    // live, writable `timespec` that the kernel fully initialises on success. On failure the
    // value written into `ts` is not read.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if rc != 0 {
        return Err(crate::error::io(IoError::last_os_error()));
    }
    Ok(CpuTimes {
        user_ms: timespec_ms(ts),
        system_ms: 0,
    })
}

/// `tv_sec * 1000000 + tv_usec`, then `/ 1000` — the C's `timeval` → milliseconds conversion.
///
/// The unchecked casts and the wrapping arithmetic mirror the C's own 64-bit conversion
/// (`(ErtsMonotonicTime) now.ru_utime.tv_sec`, `erl_time_sup.c:1439-1442`): a value the kernel
/// could never produce wraps rather than panicking in a debug build. The C's `ErtsMonotonicTime`
/// is a **signed** 64-bit type (`erl_unix_sys.h:159-167`), whereas the casts here target `u64`;
/// the two conversions still agree for every value a `getrusage` `timeval` can hold, because such
/// a `timeval` is always non-negative.
fn timeval_ms(tv: libc::timeval) -> u64 {
    (tv.tv_sec as u64)
        .wrapping_mul(USEC_PER_SEC)
        .wrapping_add(tv.tv_usec as u64)
        / USEC_PER_MSEC
}

/// `tv_sec * 1000000000 + tv_nsec`, then `/ 1000000` — nanosecond `timespec` → milliseconds.
///
/// The same truncating, wrapping conversion as [`timeval_ms`], one unit lower.
fn timespec_ms(ts: libc::timespec) -> u64 {
    (ts.tv_sec as u64)
        .wrapping_mul(NSEC_PER_SEC)
        .wrapping_add(ts.tv_nsec as u64)
        / NSEC_PER_MSEC
}

/// The `erlang:statistics(runtime)` counter: an anchored view of the process's user CPU time.
///
/// Reproduces the C's `runtime_prev` state machine (`erl_time_sup.c:1425-1481`):
/// [`RuntimeClock::runtime`] reads the process's user CPU time and returns `(Total,
/// SinceLastCall)`, where `SinceLastCall` is the difference from the **previous** reading and
/// the anchor starts at **0** — so the **first** call returns `(t, t)` with both elements
/// equal (the C initialises `runtime_prev.data.user = 0`). It is the counter
/// `erlang:statistics(runtime)` is built from.
///
/// **Keep one instance alive for the whole lifetime of the runtime.** The anchor is stateful
/// and process-wide in the C; a per-call clock would report `SinceLastCall == Total` every
/// time. There is no locking here: the C's mutex exists because any scheduler thread may call
/// `erts_runtime_elapsed_both`, whereas this type is a plain value the single-threaded runtime
/// owns — the same counter without the lock.
///
/// # Examples
///
/// ```ignore
/// let mut clock = beam_io::cpu_time::RuntimeClock::new();
/// let (total, since_last) = clock.runtime()?;   // first call: total == since_last
/// let (total2, delta) = clock.runtime()?;       // total2 >= total, delta == total2 - total
/// # Ok::<(), beam_common::BeamError>(())
/// ```
#[derive(Debug)]
pub struct RuntimeClock {
    /// The previous `Total` (user CPU time in ms); the C's `runtime_prev.data.user`, anchored
    /// at 0.
    prev_user_ms: u64,
}

impl RuntimeClock {
    /// A fresh counter with an anchor of **0** (the C's `runtime_prev.data.user = 0`).
    ///
    /// Infallible: nothing is measured until [`RuntimeClock::runtime`] is called.
    pub const fn new() -> Self {
        Self { prev_user_ms: 0 }
    }

    /// The `erlang:statistics(runtime)` reading: `(Total, SinceLastCall)`, in **milliseconds
    /// of user CPU time**.
    ///
    /// `Total` is the process's user CPU time so far (the C's `u1`) and `SinceLastCall` is the
    /// difference from the previous call (the C's `u2`). Because the anchor starts at 0, the
    /// **first** call returns two equal elements. The anchor is updated on every call, so
    /// these are the exact semantics the oracle exposes.
    ///
    /// # Errors
    ///
    /// [`beam_common::BeamError::Io`] carrying the errno if the underlying `getrusage(2)`
    /// fails. The anchor is then left **unchanged** (nothing is stored), so a transient failure
    /// cannot corrupt `SinceLastCall`; the C instead aborts the emulator at that point, which a
    /// Rust runtime must not do.
    pub fn runtime(&mut self) -> Result<(u64, u64)> {
        let total = process_cpu_times()?.user_ms;
        // The C's `user - prev_user` on an unsigned 64-bit type, so a clock that went
        // backwards would wrap rather than panic (it never does in practice).
        let since_last = total.wrapping_sub(self.prev_user_ms);
        self.prev_user_ms = total;
        Ok((total, since_last))
    }

    /// Re-anchors the counter to 0, so the next [`RuntimeClock::runtime`] call again returns
    /// two equal elements.
    ///
    /// The C has no reset (its `runtime_prev` lives for the whole emulator lifetime); this is
    /// the additive seam the runtime uses for `erts_internal`-style resets and for tests.
    pub fn reset(&mut self) {
        self.prev_user_ms = 0;
    }
}

impl Default for RuntimeClock {
    /// Equivalent to [`RuntimeClock::new`] — an anchor of 0.
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    //! Deterministic pins for the parts of this module that have no need of a syscall: the two
    //! unit conversions (the only numeric-parity risk against the C) and the `RuntimeClock`
    //! anchor bookkeeping (reachable through the private field). The syscall wrappers themselves
    //! are covered by the real-`getrusage`/`clock_gettime` cases in `tests/cpu_time.rs`, which
    //! assert monotone/structural properties only — never exact CPU-time values.

    use super::{timespec_ms, timeval_ms, RuntimeClock};

    /// A `libc::timeval` for the pure-conversion cases.
    fn timeval(sec: libc::time_t, usec: libc::suseconds_t) -> libc::timeval {
        libc::timeval {
            tv_sec: sec,
            tv_usec: usec,
        }
    }

    /// A `libc::timespec` for the pure-conversion cases.
    fn timespec(sec: libc::time_t, nsec: libc::c_long) -> libc::timespec {
        libc::timespec {
            tv_sec: sec,
            tv_nsec: nsec,
        }
    }

    /// The `timeval` conversion is the C's `tv_sec * 1000000 + tv_usec` then `/ 1000`
    /// (`erl_time_sup.c:1439-1442`): whole milliseconds, truncating. The expected values are
    /// hand-computed (not re-derived from the implementation) so a changed unit or a rounding
    /// switch is caught.
    #[test]
    fn timeval_ms_truncates_microseconds_to_whole_milliseconds() {
        let cases: [(libc::time_t, libc::suseconds_t, u64); 11] = [
            (0, 0, 0),
            (0, 1, 0),
            (0, 999, 0),
            (0, 1_000, 1),
            (0, 1_999, 1),
            (0, 999_999, 999),
            (1, 0, 1_000),
            (2, 500_000, 2_500),
            (3_600, 999_999, 3_600_999),
            (86_400, 0, 86_400_000),
            (1_000_000, 999, 1_000_000_000),
        ];
        for (sec, usec, expected) in cases {
            assert_eq!(
                timeval_ms(timeval(sec, usec)),
                expected,
                "timeval {{ {sec}, {usec} }} is {expected} ms"
            );
        }
    }

    /// The `timespec` conversion truncates the same way, one unit lower: nanoseconds → ms.
    #[test]
    fn timespec_ms_truncates_nanoseconds_to_whole_milliseconds() {
        let cases: [(libc::time_t, libc::c_long, u64); 10] = [
            (0, 0, 0),
            (0, 999_999, 0),
            (0, 1_000_000, 1),
            (0, 1_999_999, 1),
            (0, 999_999_999, 999),
            (1, 0, 1_000),
            (2, 500_000_000, 2_500),
            (3_600, 999_999_999, 3_600_999),
            (86_400, 0, 86_400_000),
            (1_000_000, 0, 1_000_000_000),
        ];
        for (sec, nsec, expected) in cases {
            assert_eq!(
                timespec_ms(timespec(sec, nsec)),
                expected,
                "timespec {{ {sec}, {nsec} }} is {expected} ms"
            );
        }
    }

    /// The two helpers describe the same instant in the same unit: a `timeval` and the
    /// `timespec` holding the identical point in time (µs × 1000 = ns) must agree to the
    /// millisecond. This is the cross-check that neither helper has drifted from the other.
    #[test]
    fn the_two_conversions_agree_for_the_same_instant() {
        for sec in [0, 1, 2, 3_600, 86_400] {
            for usec in [0, 1, 999, 1_000, 1_500, 999_999] {
                let from_timeval = timeval_ms(timeval(sec, usec));
                let from_timespec = timespec_ms(timespec(sec, usec * 1_000));
                assert_eq!(
                    from_timeval, from_timespec,
                    "{sec}s + {usec}us must convert to the same milliseconds either way"
                );
            }
        }
    }

    /// `RuntimeClock::new`/`Default` anchor the user total at 0 — the C's
    /// `runtime_prev.data.user = 0` (`erl_time_sup.c:1071`).
    #[test]
    fn runtime_clock_new_and_default_anchor_the_user_total_at_zero() {
        assert_eq!(RuntimeClock::new().prev_user_ms, 0);
        assert_eq!(RuntimeClock::default().prev_user_ms, 0);
    }

    /// `runtime()` stores the reading it just took as the new anchor, and reports the difference
    /// against the *previous* anchor (the C's `runtime_prev.data.user = user` inside the locked
    /// block, `erl_time_sup.c:1473-1483`). With a 0 anchor the first difference *is* the total.
    #[test]
    fn runtime_clock_runtime_stores_the_latest_total_as_the_anchor() {
        let mut clock = RuntimeClock::new();
        let (total, since_last) = clock.runtime().expect("the runtime counter is readable");
        assert_eq!(
            clock.prev_user_ms, total,
            "the anchor must become the reading just taken"
        );
        assert_eq!(since_last, total, "the anchor started at 0");

        let (total2, since_last2) = clock.runtime().expect("the runtime counter is readable");
        assert_eq!(clock.prev_user_ms, total2);
        assert!(total2 >= total);
        assert_eq!(since_last2, total2 - total);
    }

    /// `reset()` re-anchors at 0 (an additive seam with no C equivalent), so the next pair is
    /// equal again.
    #[test]
    fn runtime_clock_reset_re_anchors_the_user_total_at_zero() {
        let mut clock = RuntimeClock::new();
        clock.prev_user_ms = 12_345;
        clock.reset();
        assert_eq!(clock.prev_user_ms, 0);

        let (total, since_last) = clock.runtime().expect("the runtime counter is readable");
        assert_eq!(
            since_last, total,
            "a re-anchored clock reports the total as its delta"
        );
        assert_eq!(clock.prev_user_ms, total);
    }
}
