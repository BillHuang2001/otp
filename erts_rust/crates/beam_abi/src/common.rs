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

//! Shared NIF/driver types — everything declared in `erts/emulator/beam/erl_drv_nif.h`.
//!
//! This is the common ground between the NIF C ABI (`erl_nif.h`) and the driver C ABI
//! (`erl_driver.h`): the 64-bit integer typedefs, `SysIOVec`, `ErlDrvSysInfo`, `ErlDrvThreadOpts`,
//! the dirty-job / select flag sets, `ErlDrvMonitor` and the `ERTS_NAPI_*` time constants.
//!
//! Every declaration reproduces the **LP64** layout of the header (`SIZEOF_LONG == 8`,
//! `SIZEOF_VOID_P == 8`, `int` = 4, pointer = 8). Sizes/aligns are pinned by the compile-time
//! assertions at the bottom of this file; the exhaustive field-offset freeze lives in
//! [`crate::layout`].

use libc::{c_char, c_int, c_void};

// ---------------------------------------------------------------------------------------------
// Integer typedefs (`erl_drv_nif.h`, `SIZEOF_LONG == 8` / `SIZEOF_VOID_P == 8` branch)
// ---------------------------------------------------------------------------------------------

/// `ErlNapiUInt64` — `unsigned long` on LP64 (`SIZEOF_LONG == 8`).
pub type ErlNapiUInt64 = libc::c_ulong;

/// `ErlNapiSInt64` — `signed long` on LP64.
pub type ErlNapiSInt64 = libc::c_long;

/// `ErlNapiUInt` — `ErlNapiUInt64` because `SIZEOF_VOID_P == 8`.
pub type ErlNapiUInt = ErlNapiUInt64;

/// `ErlNapiSInt` — `ErlNapiSInt64` because `SIZEOF_VOID_P == 8`.
pub type ErlNapiSInt = ErlNapiSInt64;

// ---------------------------------------------------------------------------------------------
// SysIOVec (`erl_drv_nif.h`; on Unix the header uses `struct iovec` from `<sys/uio.h>`)
// ---------------------------------------------------------------------------------------------

/// `SysIOVec` — a scatter/gather vector, ABI-identical to `struct iovec`.
///
/// The header aliases the libc `struct iovec` on Unix; this guarantees `readv`/`writev`
/// compatibility. Layout: `iov_base` at 0, `iov_len` at 8 (16 bytes, align 8 on LP64).
pub type SysIOVec = libc::iovec;

// ---------------------------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------------------------

/// `ErlDrvSysInfo` (48 bytes of data, 56 with tail padding) — the driver/NIF "system info"
/// structure, also aliased as `ErlNifSysInfo` by `erl_nif.h`.
///
/// Field order is exactly the header's; offsets are `driver_major_version` 0,
/// `driver_minor_version` 4, `erts_version` 8, `otp_release` 16, `thread_support` 24,
/// `smp_support` 28, `async_threads` 32, `scheduler_threads` 36, `nif_major_version` 40,
/// `nif_minor_version` 44, `dirty_scheduler_support` 48.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct ErlDrvSysInfo {
    /// Major version of the driver API.
    pub driver_major_version: c_int,
    /// Minor version of the driver API.
    pub driver_minor_version: c_int,
    /// NUL-terminated ERTS version string (owned by the emulator).
    pub erts_version: *mut c_char,
    /// NUL-terminated OTP release string (owned by the emulator).
    pub otp_release: *mut c_char,
    /// Non-zero if the emulator has thread support.
    pub thread_support: c_int,
    /// Non-zero if the emulator is SMP capable.
    pub smp_support: c_int,
    /// Number of async threads.
    pub async_threads: c_int,
    /// Number of scheduler threads.
    pub scheduler_threads: c_int,
    /// NIF major version supported by the emulator.
    pub nif_major_version: c_int,
    /// NIF minor version supported by the emulator.
    pub nif_minor_version: c_int,
    /// Non-zero if dirty schedulers are supported.
    pub dirty_scheduler_support: c_int,
}

/// `ErlDrvThreadOpts` — options for `erl_drv_thread_create` (and `enif_thread_create`).
///
/// The only *public* field is `suggested_stack_size`; the C struct is asserted to be exactly one
/// `int` wide (the ABI promise is that this struct grows only via the allocator's size).
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct ErlDrvThreadOpts {
    /// Suggested stack size in **kilowords** (`-1` = the platform default).
    ///
    /// `erl_driver.md` (*ErlDrvThreadOpts*): "A suggestion, in kilowords, on how large a stack".
    /// The C conversion is `ETHR_KW2B` = `KW * sizeof(void *) * 1024` (`ethread.h`), and `-1` is the
    /// "use the platform default" sentinel the C stores when no suggestion is made
    /// (`erl_drv_thread.c:591`).
    pub suggested_stack_size: c_int,
}

/// `ErlDrvMonitor` — an opaque driver monitor holder.
///
/// The C type is a plain byte array sized `sizeof(void *) * 4` (32 bytes on LP64, align 1): it is
/// written and read only by the emulator, so the payload is modelled as raw bytes.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct ErlDrvMonitor {
    /// Opaque monitor payload (`sizeof(void *) * 4` bytes).
    pub data: [u8; core::mem::size_of::<*mut c_void>() * 4],
}

// ---------------------------------------------------------------------------------------------
// ErlDirtyJobFlags — `enum ErlDirtyJobFlags` (bit-wise OR-combinable, so a newtype, not a Rust enum)
// ---------------------------------------------------------------------------------------------

/// `ERL_DIRTY_JOB_CPU_BOUND` — a dirty job that is CPU bound.
pub const ERL_DIRTY_JOB_CPU_BOUND: c_int = 1;
/// `ERL_DIRTY_JOB_IO_BOUND` — a dirty job that is I/O bound.
pub const ERL_DIRTY_JOB_IO_BOUND: c_int = 2;

/// `enum ErlDirtyJobFlags` — describes the kind of work a dirty job performs.
///
/// Modelled as a `#[repr(transparent)]` `c_int` newtype rather than a field-less Rust enum,
/// because the values are OR-combined by callers and a Rust enum cannot hold a combined value
/// without UB (crate `CONTEXT.md`, Design Decisions).
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct ErlDirtyJobFlags(c_int);

impl ErlDirtyJobFlags {
    /// `ERL_DIRTY_JOB_CPU_BOUND`.
    pub const CPU_BOUND: Self = Self(ERL_DIRTY_JOB_CPU_BOUND);
    /// `ERL_DIRTY_JOB_IO_BOUND`.
    pub const IO_BOUND: Self = Self(ERL_DIRTY_JOB_IO_BOUND);

    /// The raw C value.
    #[inline]
    pub const fn bits(self) -> c_int {
        self.0
    }

    /// Wrap a raw C value.
    #[inline]
    pub const fn from_bits(bits: c_int) -> Self {
        Self(bits)
    }
}

// ---------------------------------------------------------------------------------------------
// ErlNifSelectFlags — `enum ErlNifSelectFlags` (values for enif_select and driver_select mode)
// ---------------------------------------------------------------------------------------------

/// `ERL_NIF_SELECT_READ`.
pub const ERL_NIF_SELECT_READ: c_int = 1 << 0;
/// `ERL_NIF_SELECT_WRITE`.
pub const ERL_NIF_SELECT_WRITE: c_int = 1 << 1;
/// `ERL_NIF_SELECT_STOP`.
pub const ERL_NIF_SELECT_STOP: c_int = 1 << 2;
/// `ERL_NIF_SELECT_CANCEL`.
pub const ERL_NIF_SELECT_CANCEL: c_int = 1 << 3;
/// `ERL_NIF_SELECT_CUSTOM_MSG`.
pub const ERL_NIF_SELECT_CUSTOM_MSG: c_int = 1 << 4;
/// `ERL_NIF_SELECT_ERROR`.
pub const ERL_NIF_SELECT_ERROR: c_int = 1 << 5;

/// `enum ErlNifSelectFlags` — the *mode* argument of `enif_select` / `driver_select`.
///
/// As with [`ErlDirtyJobFlags`], this is an OR-combinable bit set modelled as a
/// `#[repr(transparent)]` `c_int` newtype.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct ErlNifSelectFlags(c_int);

impl ErlNifSelectFlags {
    /// `ERL_NIF_SELECT_READ` — wait for the handle to become readable.
    pub const READ: Self = Self(ERL_NIF_SELECT_READ);
    /// `ERL_NIF_SELECT_WRITE` — wait for the handle to become writable.
    pub const WRITE: Self = Self(ERL_NIF_SELECT_WRITE);
    /// `ERL_NIF_SELECT_STOP` — wait for the handle to be safe to release.
    pub const STOP: Self = Self(ERL_NIF_SELECT_STOP);
    /// `ERL_NIF_SELECT_CANCEL` — cancel an outstanding selection.
    pub const CANCEL: Self = Self(ERL_NIF_SELECT_CANCEL);
    /// `ERL_NIF_SELECT_CUSTOM_MSG` — a custom notification message is supplied.
    pub const CUSTOM_MSG: Self = Self(ERL_NIF_SELECT_CUSTOM_MSG);
    /// `ERL_NIF_SELECT_ERROR` — select on error.
    pub const ERROR: Self = Self(ERL_NIF_SELECT_ERROR);

    /// The raw C value.
    #[inline]
    pub const fn bits(self) -> c_int {
        self.0
    }

    /// Wrap a raw C value.
    #[inline]
    pub const fn from_bits(bits: c_int) -> Self {
        Self(bits)
    }
}

// ---------------------------------------------------------------------------------------------
// Return bits from enif_select (`erl_nif.h`)
// ---------------------------------------------------------------------------------------------

/// `ERL_NIF_SELECT_STOP_CALLED` — the stop callback has already been called.
pub const ERL_NIF_SELECT_STOP_CALLED: c_int = 1 << 0;
/// `ERL_NIF_SELECT_STOP_SCHEDULED` — the stop callback was scheduled.
pub const ERL_NIF_SELECT_STOP_SCHEDULED: c_int = 1 << 1;
/// `ERL_NIF_SELECT_INVALID_EVENT` — the event handle is invalid for this platform.
pub const ERL_NIF_SELECT_INVALID_EVENT: c_int = 1 << 2;
/// `ERL_NIF_SELECT_FAILED` — the selection failed.
pub const ERL_NIF_SELECT_FAILED: c_int = 1 << 3;
/// `ERL_NIF_SELECT_READ_CANCELLED` — a read selection was cancelled.
pub const ERL_NIF_SELECT_READ_CANCELLED: c_int = 1 << 4;
/// `ERL_NIF_SELECT_WRITE_CANCELLED` — a write selection was cancelled.
pub const ERL_NIF_SELECT_WRITE_CANCELLED: c_int = 1 << 5;
/// `ERL_NIF_SELECT_ERROR_CANCELLED` — an error selection was cancelled.
pub const ERL_NIF_SELECT_ERROR_CANCELLED: c_int = 1 << 6;
/// `ERL_NIF_SELECT_NOTSUP` — selection is not supported for this handle.
pub const ERL_NIF_SELECT_NOTSUP: c_int = 1 << 7;

// ---------------------------------------------------------------------------------------------
// ERTS_NAPI_* time constants (`erl_drv_nif.h`)
// ---------------------------------------------------------------------------------------------

/// `ERTS_NAPI_SEC__` — seconds.
pub const ERTS_NAPI_SEC: c_int = 0;
/// `ERTS_NAPI_MSEC__` — milliseconds.
pub const ERTS_NAPI_MSEC: c_int = 1;
/// `ERTS_NAPI_USEC__` — microseconds.
pub const ERTS_NAPI_USEC: c_int = 2;
/// `ERTS_NAPI_NSEC__` — nanoseconds.
pub const ERTS_NAPI_NSEC: c_int = 3;

/// `ERTS_NAPI_TIME_ERROR__` — the invalid-time sentinel (`ERL_NAPI_SINT64_MIN__`).
pub const ERTS_NAPI_TIME_ERROR: ErlNapiSInt64 = i64::MIN;

// ---------------------------------------------------------------------------------------------
// Compile-time layout freeze (LP64)
// ---------------------------------------------------------------------------------------------

const _: () = {
    assert!(core::mem::size_of::<ErlDrvSysInfo>() == 56);
    assert!(core::mem::align_of::<ErlDrvSysInfo>() == 8);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, driver_major_version) == 0);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, driver_minor_version) == 4);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, erts_version) == 8);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, otp_release) == 16);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, thread_support) == 24);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, smp_support) == 28);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, async_threads) == 32);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, scheduler_threads) == 36);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, nif_major_version) == 40);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, nif_minor_version) == 44);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, dirty_scheduler_support) == 48);
};

const _: () = {
    assert!(core::mem::size_of::<ErlDrvThreadOpts>() == 4);
    assert!(core::mem::align_of::<ErlDrvThreadOpts>() == 4);
    assert!(core::mem::offset_of!(ErlDrvThreadOpts, suggested_stack_size) == 0);
};

const _: () = {
    assert!(core::mem::size_of::<ErlDrvMonitor>() == 32);
    assert!(core::mem::align_of::<ErlDrvMonitor>() == 1);
    assert!(core::mem::offset_of!(ErlDrvMonitor, data) == 0);
};

const _: () = {
    assert!(core::mem::size_of::<ErlDirtyJobFlags>() == 4);
    assert!(core::mem::align_of::<ErlDirtyJobFlags>() == 4);
    assert!(core::mem::size_of::<ErlNifSelectFlags>() == 4);
    assert!(core::mem::align_of::<ErlNifSelectFlags>() == 4);
};

const _: () = {
    assert!(core::mem::size_of::<SysIOVec>() == 16);
    assert!(core::mem::align_of::<SysIOVec>() == 8);
    assert!(core::mem::offset_of!(SysIOVec, iov_base) == 0);
    assert!(core::mem::offset_of!(SysIOVec, iov_len) == 8);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_bit_values_match_the_header() {
        assert_eq!(ErlDirtyJobFlags::CPU_BOUND.bits(), 1);
        assert_eq!(ErlDirtyJobFlags::IO_BOUND.bits(), 2);
        assert_eq!(ErlNifSelectFlags::READ.bits(), 1 << 0);
        assert_eq!(ErlNifSelectFlags::WRITE.bits(), 1 << 1);
        assert_eq!(ErlNifSelectFlags::STOP.bits(), 1 << 2);
        assert_eq!(ErlNifSelectFlags::CANCEL.bits(), 1 << 3);
        assert_eq!(ErlNifSelectFlags::CUSTOM_MSG.bits(), 1 << 4);
        assert_eq!(ErlNifSelectFlags::ERROR.bits(), 1 << 5);
    }

    #[test]
    fn flag_bits_round_trip() {
        let combined = ERL_NIF_SELECT_READ | ERL_NIF_SELECT_WRITE;
        assert_eq!(ErlNifSelectFlags::from_bits(combined).bits(), combined);
        assert_eq!(ErlDirtyJobFlags::from_bits(3).bits(), 3);
    }

    #[test]
    fn time_constants() {
        assert_eq!(
            [ERTS_NAPI_SEC, ERTS_NAPI_MSEC, ERTS_NAPI_USEC, ERTS_NAPI_NSEC],
            [0, 1, 2, 3]
        );
        assert_eq!(ERTS_NAPI_TIME_ERROR, i64::MIN);
    }
}
